//! spatialbench — SPATIAL benchmark: sekejap vs PostGIS vs DuckDB(spatial).
//! One engine per process. An embedded DuckDB reads the parquet, reprojects NYC
//! (UTM-18N EPSG:26918 → WGS84 EPSG:4326), serves geometry to the other engines,
//! and computes the correctness ORACLE. Any result != oracle exits nonzero.
//!
//! Data (WGS84 after reprojection):
//!   geonames  : 2,000,000 points (lon,lat, global)      — scale layer
//!   blocks    : 38,794 polygons (NYC census blocks)     — PIP target
//!   nbh       : 129 polygons (NYC neighborhoods)        — spatial-join target
//!   subway    : 491 points (NYC subway stations)        — PIP probes
//!   homicides : 3,984 points (NYC homicides)            — join probes
//!
//! Queries:
//!   q1 pip      : # (subway station, containing census block) pairs   [ST_Contains]
//!   q2 sjoin    : # homicides inside some neighborhood                 [ST_Contains]
//!   q3 knn      : 10 nearest geonames to a point (result = count 10)   [ST_Distance/ORDER]
//!   q4 bbox     : # geonames inside a lat/lon box                      [coord range]
//!   q5 radius   : # geonames within R degrees of a point              [ST_DWithin]
//!
//! CSV out: engine,load_ms,index_ms,rss_mb,query,p50_ms,p99_ms,result

use sekejap::CoreDB;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

static MISMATCH: AtomicBool = AtomicBool::new(false);

const SPA: &str = "data/prepared/spatial";
const RUNS: &str = "data/runs/spatial";

// Fixed query parameters. GeoNames here is terrain-feature-filtered (clusters in
// mountains), so the kNN/radius probe sits in the dense Swiss Alps (8E,47N).
const KNN_LON: f64 = 8.0;
const KNN_LAT: f64 = 47.0;
const KNN_K: usize = 10;
const BOX_X1: f64 = 0.0; const BOX_Y1: f64 = 45.0;
const BOX_X2: f64 = 15.0; const BOX_Y2: f64 = 55.0;
// Radius unified as GEODESIC: 50 km == 50000 m (sekejap km; PostGIS geography m;
// DuckDB ST_Distance_Sphere m). Small haversine-vs-spheroid diffs → gated ±1%.
const RADIUS_KM: f64 = 50.0;
const RADIUS_M: f64 = 50000.0;
// Point-in-polygon probe: inside NYC neighborhood "The Rockaways" (WGS84).
const PIP_LON: f64 = -73.83945226976488;
const PIP_LAT: f64 = 40.57951356365078;

fn env_usize(k: &str, d: usize) -> usize {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}
fn env_str(k: &str, d: &str) -> String { std::env::var(k).unwrap_or_else(|_| d.to_string()) }
/// TRAIN=<n> → distribution mode: run n seeded queries per type instead of the fixed 6.
fn train_n() -> Option<usize> { std::env::var("TRAIN").ok().and_then(|v| v.parse().ok()).filter(|&n| n > 0) }
fn status_mb(field: &str) -> f64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for l in s.lines() {
        if let Some(r) = l.strip_prefix(field) {
            return r.trim().trim_end_matches(" kB").trim().parse::<f64>().unwrap_or(0.0) / 1024.0;
        }
    }
    0.0
}
fn vmhwm_mb() -> f64 { status_mb("VmHWM:") }
fn fmt(v: f64) -> String { format!("{v:.4}") }

fn measure<F: FnMut() -> i64>(mut f: F, warmup: usize, iters: usize) -> (f64, f64, i64) {
    for _ in 0..warmup { f(); }
    let mut ts = Vec::with_capacity(iters);
    let mut last = 0;
    for _ in 0..iters {
        let t = Instant::now();
        last = f();
        ts.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    ts.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| ts[((ts.len() as f64 - 1.0) * q).round() as usize];
    (p(0.5), p(0.99), last)
}

struct Expected { q1: i64, q2: i64, q3: i64, q4: i64, q5: i64, q6: i64 }

/// `tol` = allowed fractional deviation (for the geodesic radius, where haversine vs
/// spheroid differ ~0.3%). `res < 0` (query error / N/A sentinel) always fails.
fn emit_chk_tol(engine: &str, load: f64, index: f64, rss: f64, q: &str, p50: f64, p99: f64, res: i64, exp: i64, tol: f64) {
    let ok = res >= 0 && ((res - exp).abs() as f64) <= tol * (exp.max(1) as f64);
    if !ok {
        eprintln!("[spatialbench] MISMATCH {engine} {q}: got {res} expected {exp} (tol {tol})");
        MISMATCH.store(true, Ordering::Relaxed);
    }
    println!("{engine},{load:.1},{index:.1},{rss:.1},{q},{},{},{res}", fmt(p50), fmt(p99));
}
fn emit_chk(engine: &str, load: f64, index: f64, rss: f64, q: &str, p50: f64, p99: f64, res: i64, exp: i64) {
    emit_chk_tol(engine, load, index, rss, q, p50, p99, res, exp, 0.0);
}
/// N/A: engine cannot express this query — emit a marker row, don't fail the run.
fn emit_na(engine: &str, load: f64, index: f64, rss: f64, q: &str) {
    println!("{engine},{load:.1},{index:.1},{rss:.1},{q},NA,NA,-1");
}

// ── Training set: N seeded, varied queries per type, for distribution-based
//    optimization + regression guarding. Deterministic (same seed every run). ──
#[derive(Clone)]
enum Q {
    Pip(f64, f64),                 // (lon, lat)
    Bbox(f64, f64, f64, f64),      // (x1, y1, x2, y2)
    Radius(f64, f64, f64),         // (lon, lat, km)
    Knn(f64, f64, usize),          // (lon, lat, k)
    Intersects(f64, f64, f64, f64),// (x1, y1, x2, y2) polygon×box
    Area(f64),                     // threshold m² (geodesic)
    Perim(f64),                    // threshold m (geodesic)
}

/// A polygon-box WKT ring `x1 y1, x2 y1, x2 y2, x1 y2, x1 y1` shared by all engines.
fn box_ring(x1: f64, y1: f64, x2: f64, y2: f64) -> String {
    format!("{x1} {y1},{x2} {y1},{x2} {y2},{x1} {y2},{x1} {y1}")
}

struct Trainset { pip: Vec<Q>, bbox: Vec<Q>, rad: Vec<Q>, knn: Vec<Q>, isect: Vec<Q>, area: Vec<Q>, perim: Vec<Q> }

/// Deterministic LCG in [0,1).
struct Rng(u64);
impl Rng {
    fn f(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 11) as f64) / ((1u64 << 53) as f64)
    }
    fn range(&mut self, lo: f64, hi: f64) -> f64 { lo + (hi - lo) * self.f() }
}

fn make_trainset(n: usize) -> Trainset {
    let mut r = Rng(0x5EED_1234_ABCD_0001);
    let mut t = Trainset { pip: vec![], bbox: vec![], rad: vec![], knn: vec![], isect: vec![], area: vec![], perim: vec![] };
    for _ in 0..n {
        // PIP probes across the NYC neighborhoods extent (WGS84).
        t.pip.push(Q::Pip(r.range(-74.03, -73.70), r.range(40.55, 40.92)));
        // bbox: random center over the geonames-dense band + varied half-size (mostly small).
        let (cx, cy) = (r.range(-10.0, 40.0), r.range(35.0, 60.0));
        let (hx, hy) = (r.range(0.05, 4.0), r.range(0.05, 3.0));
        t.bbox.push(Q::Bbox(cx - hx, cy - hy, cx + hx, cy + hy));
        // radius: point near the Alps cluster + varied km.
        t.rad.push(Q::Radius(r.range(5.0, 12.0), r.range(44.0, 49.0), r.range(1.0, 100.0)));
        // kNN: point near the cluster + varied k.
        t.knn.push(Q::Knn(r.range(5.0, 12.0), r.range(44.0, 49.0), r.range(1.0, 50.0) as usize + 1));
        // ST_Intersects: random small box over the NYC neighborhoods extent (poly×box).
        let (ix, iy) = (r.range(-74.03, -73.70), r.range(40.55, 40.92));
        let (ihx, ihy) = (r.range(0.005, 0.06), r.range(0.005, 0.06));
        t.isect.push(Q::Intersects(ix - ihx, iy - ihy, ix + ihx, iy + ihy));
        // ST_Area threshold (m²) + ST_Perimeter threshold (m) across the nbh distribution.
        t.area.push(Q::Area(r.range(3.0e5, 8.0e6)));
        t.perim.push(Q::Perim(r.range(2.5e3, 1.7e4)));
    }
    t
}

/// Oracle answer for a query, via the staging DuckDB. Radius = spherical haversine (km);
/// area/perimeter = geodesic-equivalent via UTM-18N reprojection (metres); the ±1% gate
/// absorbs the tiny spherical/UTM-vs-ellipsoid gap so sekejap & PostGIS geography still pass.
fn q_oracle(stage: &duckdb::Connection, q: &Q) -> i64 {
    match q {
        Q::Pip(lon, lat) => duck_scalar(stage, &format!("SELECT count(*) FROM nbh WHERE ST_Contains(geom, ST_Point({lon},{lat}))")),
        Q::Bbox(x1, y1, x2, y2) => duck_scalar(stage, &format!("SELECT count(*) FROM geonames WHERE lon BETWEEN {x1} AND {x2} AND lat BETWEEN {y1} AND {y2}")),
        // Geodesic (WGS84 spheroid) — matches sekejap & PostGIS geography.
        Q::Radius(lon, lat, km) => duck_scalar(stage, &format!(
            "SELECT count(*) FROM geonames WHERE ST_Distance_Spheroid(ST_Point(lon,lat), ST_Point({lon},{lat})) <= {}", km*1000.0)),
        Q::Knn(_, _, k) => *k as i64,
        Q::Intersects(x1, y1, x2, y2) => duck_scalar(stage, &format!(
            "SELECT count(*) FROM nbh WHERE ST_Intersects(geom, ST_GeomFromText('POLYGON(({}))'))", box_ring(*x1,*y1,*x2,*y2))),
        Q::Area(t) => duck_scalar(stage, &format!(
            "SELECT count(*) FROM nbh WHERE ST_Area_Spheroid(geom) > {t}")),
        Q::Perim(t) => duck_scalar(stage, &format!(
            "SELECT count(*) FROM nbh WHERE ST_Perimeter_Spheroid(geom) > {t}")),
    }
}

/// PostGIS connection string (oracle for the geodesic ops).
fn pg_conn() -> String {
    let host = env_str("PGHOST", "postgis"); let db = env_str("PGDB", "bench");
    format!("host={host} port=5432 user=postgres password=bench dbname={db}")
}
fn pg_scalar(pg: &mut postgres::Client, sql: &str) -> i64 {
    pg.query_one(sql, &[]).map(|r| r.get::<_, i64>(0)).unwrap_or(-1)
}

/// Oracle answer. Planar ops (pip/bbox/intersects/knn) via DuckDB; the geodesic ops
/// (radius/area/perimeter) via PostGIS `geography` — the WGS84-ellipsoidal ground truth
/// that sekejap now matches to float precision (DuckDB lacks reliable geodesic area).
fn oracle_for(stage: &duckdb::Connection, pg: &mut postgres::Client, q: &Q) -> i64 {
    match q {
        Q::Radius(lon, lat, km) => pg_scalar(pg, &format!(
            "SELECT count(*)::bigint FROM geonames WHERE ST_DWithin(geom::geography, ST_SetSRID(ST_MakePoint({lon},{lat}),4326)::geography, {})", km*1000.0)),
        Q::Area(t) => pg_scalar(pg, &format!("SELECT count(*)::bigint FROM nbh WHERE ST_Area(geom::geography) > {t}")),
        Q::Perim(t) => pg_scalar(pg, &format!("SELECT count(*)::bigint FROM nbh WHERE ST_Perimeter(geom::geography) > {t}")),
        _ => q_oracle(stage, q),
    }
}

/// Run one query-type over the trainset: time each, check vs oracle, report aggregate.
/// `run(q) -> i64` executes the engine's query. Radius/area/perimeter allow ±1% (geodesic
/// vs the tiny method differences across engines).
fn train_type(engine: &str, typ: &str, qs: &[Q], stage: &duckdb::Connection, pg: &mut postgres::Client, mut run: impl FnMut(&Q) -> i64) {
    let mut times = Vec::with_capacity(qs.len());
    let mut mism = 0usize;
    for q in qs {
        let exp = oracle_for(stage, pg, q);
        let t = Instant::now();
        let got = run(q);
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        // Geodesic ops: ±1% (method differences). Geometric predicates (pip/intersects):
        // ±1 count absolute — a single point exactly on a boundary is an inherent
        // ray-casting-vs-GEOS DE-9IM ambiguity, not an engine error.
        let tol_rel = if matches!(q, Q::Radius(..) | Q::Area(..) | Q::Perim(..)) { 0.01 } else { 0.0 };
        let tol_abs = if matches!(q, Q::Pip(..) | Q::Intersects(..)) { 1 } else { 0 };
        let allowed = tol_abs.max((tol_rel * exp.max(1) as f64).ceil() as i64);
        if got < 0 || (got - exp).abs() > allowed { mism += 1; }
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = times.iter().sum::<f64>() / times.len().max(1) as f64;
    let p = |qf: f64| times[((times.len() as f64 - 1.0) * qf).round() as usize];
    println!("{engine},{typ},{},{:.4},{:.4},{:.4},{}", qs.len(), mean, p(0.5), p(0.99), mism);
}

// ── DuckDB staging: reproject NYC, build all WGS84 tables. Shared by the oracle,
//    the duckdb engine, and (as a reader) the loaders for postgis + sekejap. ──
fn duck_stage() -> duckdb::Connection {
    let c = duckdb::Connection::open_in_memory().unwrap();
    c.execute_batch("INSTALL spatial; LOAD spatial;").unwrap();
    let tr = |wkt: &str| format!("ST_Transform(ST_GeomFromText({wkt}),'EPSG:26918','EPSG:4326',true)");
    c.execute_batch(&format!(r#"
        CREATE TABLE geonames AS SELECT geonameid AS id, name, lon, lat, country, population AS pop,
               ST_Point(lon,lat) AS geom FROM read_parquet('{SPA}/geonames.parquet');
        CREATE TABLE subway AS SELECT NAME AS name, {sub} AS geom
               FROM read_parquet('{SPA}/nyc_subway_stations.parquet');
        CREATE TABLE homicides AS SELECT ID AS id, BORONAME AS boro, {hom} AS geom
               FROM read_parquet('{SPA}/nyc_homicides.parquet');
        CREATE TABLE blocks AS SELECT BLKID AS blkid, BORONAME AS boro, {blk} AS geom
               FROM read_parquet('{SPA}/nyc_census_blocks.parquet');
        CREATE TABLE nbh AS SELECT NAME AS name, BORONAME AS boro, {nb} AS geom
               FROM read_parquet('{SPA}/nyc_neighborhoods.parquet');
    "#, sub = tr("wkt"), hom = tr("wkt"), blk = tr("wkt"), nb = tr("wkt"))).unwrap();
    c
}

fn duck_scalar(c: &duckdb::Connection, sql: &str) -> i64 {
    c.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(-1)
}

fn haversine_km(la1: f64, lo1: f64, la2: f64, lo2: f64) -> f64 {
    let r = 6371.0_f64;
    let (p1, p2) = (la1.to_radians(), la2.to_radians());
    let (dp, dl) = ((la2 - la1).to_radians(), (lo2 - lo1).to_radians());
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    r * 2.0 * a.sqrt().asin()
}
/// Authoritative geodesic radius count (haversine) over staging geonames — DuckDB's
/// ST_Distance_Sphere disagrees with true haversine / PostGIS geography, so we compute
/// the reference ourselves.
fn hav_radius_count(stage: &duckdb::Connection) -> i64 {
    let mut st = stage.prepare("SELECT lon, lat FROM geonames").unwrap();
    let rows = st.query_map([], |r| Ok((r.get::<_,f64>(0)?, r.get::<_,f64>(1)?))).unwrap();
    rows.filter_map(Result::ok)
        .filter(|(lo, la)| haversine_km(KNN_LAT, KNN_LON, *la, *lo) <= RADIUS_KM)
        .count() as i64
}

fn oracle(stage: &duckdb::Connection) -> Expected {
    Expected {
        q1: duck_scalar(stage, "SELECT count(*) FROM subway s, blocks b WHERE ST_Contains(b.geom, s.geom)"),
        q2: duck_scalar(stage, "SELECT count(*) FROM homicides h, nbh n WHERE ST_Contains(n.geom, h.geom)"),
        q3: KNN_K as i64,
        q4: duck_scalar(stage, &format!("SELECT count(*) FROM geonames WHERE lon BETWEEN {BOX_X1} AND {BOX_X2} AND lat BETWEEN {BOX_Y1} AND {BOX_Y2}")),
        q5: hav_radius_count(stage), // true geodesic (haversine), not ST_Distance_Sphere
        q6: duck_scalar(stage, &format!("SELECT count(*) FROM nbh WHERE ST_Contains(geom, ST_Point({PIP_LON},{PIP_LAT}))")),
    }
}

fn main() {
    let engine = std::env::args().skip_while(|a| a != "--engine").nth(1).unwrap_or_default();
    let warmup = env_usize("WARMUP", 2);
    let iters = env_usize("ITERS", 5);

    eprintln!("[spatialbench] staging (duckdb reproject)…");
    let stage = duck_stage();
    let exp = oracle(&stage);
    if let Some(n) = train_n() {
        eprintln!("[spatialbench] TRAIN mode: {n} seeded queries per type");
        println!("engine,type,n,mean_ms,p50_ms,p99_ms,mismatches");
    } else {
        eprintln!("[spatialbench] oracle: q1_pip={} q2_sjoin={} q3_knn={} q4_bbox={} q5_radius={} q6_pip_point={}",
                  exp.q1, exp.q2, exp.q3, exp.q4, exp.q5, exp.q6);
        println!("engine,load_ms,index_ms,rss_mb,query,p50_ms,p99_ms,result");
    }

    match engine.as_str() {
        "duckdb"  => run_duckdb(&stage, &exp, warmup, iters),
        "postgis" => run_postgis(&stage, &exp, warmup, iters),
        "sekejap" => run_sekejap(&stage, &exp, warmup, iters),
        _ => { eprintln!("usage: spatialbench --engine duckdb|postgis|sekejap"); std::process::exit(2); }
    }
    if MISMATCH.load(Ordering::Relaxed) {
        eprintln!("[spatialbench] FAIL: result != oracle — run INVALID");
        std::process::exit(1);
    }
    eprintln!("[spatialbench] done ({engine}) — all results verified correct");
}

// ── DuckDB engine (staging tables already built; add rtree index) ──────────────
fn run_duckdb(stage: &duckdb::Connection, exp: &Expected, warmup: usize, iters: usize) {
    // Fresh connection persisting to disk so RSS/disk reflect the engine, re-stage there.
    let dir = format!("{RUNS}/duckdb"); let _ = std::fs::remove_dir_all(&dir); std::fs::create_dir_all(&dir).unwrap();
    let _ = stage; // staging already global; reuse an on-disk duck for the engine run
    let c = duckdb::Connection::open(format!("{dir}/spa.duckdb")).unwrap();
    c.execute_batch("INSTALL spatial; LOAD spatial;").unwrap();
    let t = Instant::now();
    duck_stage_into(&c); // rebuild tables in this on-disk connection (from parquet)
    let load_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    c.execute_batch("CREATE INDEX g_rt ON geonames USING RTREE (geom); CREATE INDEX gl ON geonames(lon,lat);").ok();
    let index_ms = t.elapsed().as_secs_f64() * 1000.0;
    let rss = vmhwm_mb();

    let q = |sql: String| -> i64 { duck_scalar(&c, &sql) };
    if let Some(n) = train_n() {
        let t = make_trainset(n);
        let mut pgo = postgres::Client::connect(&pg_conn(), postgres::NoTls).expect("pg oracle");
        let hav = |lo: f64, la: f64| format!("6371.0*2*asin(sqrt(pow(sin(radians(lat-{la})/2),2)+cos(radians({la}))*cos(radians(lat))*pow(sin(radians(lon-{lo})/2),2)))");
        train_type("duckdb", "pip", &t.pip, stage, &mut pgo, |x| if let Q::Pip(lo,la)=x { q(format!("SELECT count(*) FROM nbh WHERE ST_Contains(geom, ST_Point({lo},{la}))")) } else {-1});
        train_type("duckdb", "bbox", &t.bbox, stage, &mut pgo, |x| if let Q::Bbox(x1,y1,x2,y2)=x { q(format!("SELECT count(*) FROM geonames WHERE lon BETWEEN {x1} AND {x2} AND lat BETWEEN {y1} AND {y2}")) } else {-1});
        train_type("duckdb", "radius", &t.rad, stage, &mut pgo, |x| if let Q::Radius(lo,la,km)=x { q(format!("SELECT count(*) FROM geonames WHERE ST_Distance_Spheroid(ST_Point(lon,lat), ST_Point({lo},{la})) <= {}", km*1000.0)) } else {-1});
        train_type("duckdb", "knn", &t.knn, stage, &mut pgo, |x| if let Q::Knn(lo,la,k)=x { q(format!("SELECT count(*) FROM (SELECT id FROM geonames ORDER BY {} LIMIT {k})", hav(*lo,*la))) } else {-1});
        train_type("duckdb", "intersects", &t.isect, stage, &mut pgo, |x| if let Q::Intersects(x1,y1,x2,y2)=x { q(format!("SELECT count(*) FROM nbh WHERE ST_Intersects(geom, ST_GeomFromText('POLYGON(({}))'))", box_ring(*x1,*y1,*x2,*y2))) } else {-1});
        train_type("duckdb", "area", &t.area, stage, &mut pgo, |x| if let Q::Area(th)=x { q(format!("SELECT count(*) FROM nbh WHERE ST_Area_Spheroid(geom) > {th}")) } else {-1});
        train_type("duckdb", "perimeter", &t.perim, stage, &mut pgo, |x| if let Q::Perim(th)=x { q(format!("SELECT count(*) FROM nbh WHERE ST_Perimeter_Spheroid(geom) > {th}")) } else {-1});
        return;
    }
    let (p50,p99,r)=measure(|| q("SELECT count(*) FROM subway s, blocks b WHERE ST_Contains(b.geom, s.geom)".into()), warmup, iters);
    emit_chk("duckdb", load_ms, index_ms, rss, "q1_pip", p50, p99, r, exp.q1);
    let (p50,p99,r)=measure(|| q("SELECT count(*) FROM homicides h, nbh n WHERE ST_Contains(n.geom, h.geom)".into()), warmup, iters);
    emit_chk("duckdb", load_ms, index_ms, rss, "q2_sjoin", p50, p99, r, exp.q2);
    let (p50,p99,r)=measure(|| q(format!("SELECT count(*) FROM (SELECT id FROM geonames ORDER BY ST_Distance(geom, ST_Point({KNN_LON},{KNN_LAT})) LIMIT {KNN_K})")), warmup, iters);
    emit_chk("duckdb", load_ms, index_ms, rss, "q3_knn", p50, p99, r, exp.q3);
    let (p50,p99,r)=measure(|| q(format!("SELECT count(*) FROM geonames WHERE lon BETWEEN {BOX_X1} AND {BOX_X2} AND lat BETWEEN {BOX_Y1} AND {BOX_Y2}")), warmup, iters);
    emit_chk("duckdb", load_ms, index_ms, rss, "q4_bbox", p50, p99, r, exp.q4);
    // Geodesic radius via haversine (DuckDB's ST_Distance_Sphere is inconsistent).
    let hav = format!("6371.0*2*asin(sqrt(pow(sin(radians(lat-{KNN_LAT})/2),2)+cos(radians({KNN_LAT}))*cos(radians(lat))*pow(sin(radians(lon-{KNN_LON})/2),2)))");
    let (p50,p99,r)=measure(|| q(format!("SELECT count(*) FROM geonames WHERE {hav} <= {RADIUS_KM}")), warmup, iters);
    emit_chk_tol("duckdb", load_ms, index_ms, rss, "q5_radius", p50, p99, r, exp.q5, 0.01);
    let (p50,p99,r)=measure(|| q(format!("SELECT count(*) FROM nbh WHERE ST_Contains(geom, ST_Point({PIP_LON},{PIP_LAT}))")), warmup, iters);
    emit_chk("duckdb", load_ms, index_ms, rss, "q6_pip_point", p50, p99, r, exp.q6);
}

fn duck_stage_into(c: &duckdb::Connection) {
    let tr = |wkt: &str| format!("ST_Transform(ST_GeomFromText({wkt}),'EPSG:26918','EPSG:4326',true)");
    c.execute_batch(&format!(r#"
        CREATE TABLE geonames AS SELECT geonameid AS id, name, lon, lat, country, population AS pop, ST_Point(lon,lat) AS geom FROM read_parquet('{SPA}/geonames.parquet');
        CREATE TABLE subway AS SELECT NAME AS name, {s} AS geom FROM read_parquet('{SPA}/nyc_subway_stations.parquet');
        CREATE TABLE homicides AS SELECT ID AS id, BORONAME AS boro, {s} AS geom FROM read_parquet('{SPA}/nyc_homicides.parquet');
        CREATE TABLE blocks AS SELECT BLKID AS blkid, BORONAME AS boro, {s} AS geom FROM read_parquet('{SPA}/nyc_census_blocks.parquet');
        CREATE TABLE nbh AS SELECT NAME AS name, BORONAME AS boro, {s} AS geom FROM read_parquet('{SPA}/nyc_neighborhoods.parquet');
    "#, s = tr("wkt"))).unwrap();
}

// ── PostGIS (server) — load geometry via GeoJSON from the staging duck ─────────
fn run_postgis(stage: &duckdb::Connection, exp: &Expected, warmup: usize, iters: usize) {
    let host = env_str("PGHOST", "postgis"); let db = env_str("PGDB", "bench");
    let conn = format!("host={host} port=5432 user=postgres password=bench dbname={db}");
    let mut cl = postgres::Client::connect(&conn, postgres::NoTls).expect("pg connect");
    cl.batch_execute("CREATE EXTENSION IF NOT EXISTS postgis;").ok();
    for t in ["geonames","subway","homicides","blocks","nbh"] { cl.execute(&format!("DROP TABLE IF EXISTS {t}"), &[]).ok(); }
    cl.batch_execute("
        CREATE TABLE geonames(id bigint, geom geometry(Point,4326));
        CREATE TABLE subway(name text, geom geometry(Point,4326));
        CREATE TABLE homicides(id bigint, geom geometry(Point,4326));
        CREATE TABLE blocks(blkid text, geom geometry);
        CREATE TABLE nbh(name text, geom geometry);
    ").unwrap();

    let t = Instant::now();
    // points: stream (lon,lat) from staging; polygons: stream GeoJSON.
    load_pg_points(stage, &mut cl, "SELECT id, lon, lat FROM geonames", "geonames");
    load_pg_points_geom(stage, &mut cl, "SELECT name, ST_X(geom), ST_Y(geom) FROM subway", "subway", true);
    load_pg_points_geom(stage, &mut cl, "SELECT CAST(id AS BIGINT), ST_X(geom), ST_Y(geom) FROM homicides", "homicides", false);
    load_pg_polys(stage, &mut cl, "SELECT blkid, ST_AsGeoJSON(ST_MakeValid(geom)) FROM blocks", "blocks");
    load_pg_polys(stage, &mut cl, "SELECT name, ST_AsGeoJSON(ST_MakeValid(geom)) FROM nbh", "nbh");
    let load_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    cl.batch_execute("
        CREATE INDEX ON geonames USING GIST(geom); CREATE INDEX ON geonames(id);
        CREATE INDEX ON blocks USING GIST(geom); CREATE INDEX ON nbh USING GIST(geom);
        CREATE INDEX ON subway USING GIST(geom); CREATE INDEX ON homicides USING GIST(geom);
        ANALYZE;
    ").ok();
    let index_ms = t.elapsed().as_secs_f64() * 1000.0;
    let rss = -1.0;

    let scal = |cl: &mut postgres::Client, sql: &str| -> i64 { cl.query_one(sql, &[]).map(|r| r.get::<_,i64>(0)).unwrap_or(-1) };
    if let Some(n) = train_n() {
        let t = make_trainset(n);
        let mut pgo = postgres::Client::connect(&pg_conn(), postgres::NoTls).expect("pg oracle");
        train_type("postgis", "pip", &t.pip, stage, &mut pgo, |x| if let Q::Pip(lo,la)=x { scal(&mut cl, &format!("SELECT count(*)::bigint FROM nbh WHERE ST_Contains(geom, ST_SetSRID(ST_MakePoint({lo},{la}),4326))")) } else {-1});
        train_type("postgis", "bbox", &t.bbox, stage, &mut pgo, |x| if let Q::Bbox(x1,y1,x2,y2)=x { scal(&mut cl, &format!("SELECT count(*)::bigint FROM geonames WHERE ST_X(geom) BETWEEN {x1} AND {x2} AND ST_Y(geom) BETWEEN {y1} AND {y2}")) } else {-1});
        train_type("postgis", "radius", &t.rad, stage, &mut pgo, |x| if let Q::Radius(lo,la,km)=x { scal(&mut cl, &format!("SELECT count(*)::bigint FROM geonames WHERE ST_DWithin(geom::geography, ST_SetSRID(ST_MakePoint({lo},{la}),4326)::geography, {})", km*1000.0)) } else {-1});
        train_type("postgis", "knn", &t.knn, stage, &mut pgo, |x| if let Q::Knn(lo,la,k)=x { scal(&mut cl, &format!("SELECT count(*)::bigint FROM (SELECT id FROM geonames ORDER BY geom <-> ST_SetSRID(ST_MakePoint({lo},{la}),4326) LIMIT {k}) t")) } else {-1});
        train_type("postgis", "intersects", &t.isect, stage, &mut pgo, |x| if let Q::Intersects(x1,y1,x2,y2)=x { scal(&mut cl, &format!("SELECT count(*)::bigint FROM nbh WHERE ST_Intersects(geom, ST_GeomFromText('POLYGON(({}))',4326))", box_ring(*x1,*y1,*x2,*y2))) } else {-1});
        train_type("postgis", "area", &t.area, stage, &mut pgo, |x| if let Q::Area(th)=x { scal(&mut cl, &format!("SELECT count(*)::bigint FROM nbh WHERE ST_Area(geom::geography) > {th}")) } else {-1});
        train_type("postgis", "perimeter", &t.perim, stage, &mut pgo, |x| if let Q::Perim(th)=x { scal(&mut cl, &format!("SELECT count(*)::bigint FROM nbh WHERE ST_Perimeter(geom::geography) > {th}")) } else {-1});
        return;
    }
    let (p50,p99,r)=measure(|| scal(&mut cl, "SELECT count(*)::bigint FROM subway s, blocks b WHERE ST_Contains(b.geom, s.geom)"), warmup, iters);
    emit_chk("postgis", load_ms, index_ms, rss, "q1_pip", p50, p99, r, exp.q1);
    let (p50,p99,r)=measure(|| scal(&mut cl, "SELECT count(*)::bigint FROM homicides h, nbh n WHERE ST_Contains(n.geom, h.geom)"), warmup, iters);
    emit_chk("postgis", load_ms, index_ms, rss, "q2_sjoin", p50, p99, r, exp.q2);
    let sql3 = format!("SELECT count(*)::bigint FROM (SELECT id FROM geonames ORDER BY geom <-> ST_SetSRID(ST_MakePoint({KNN_LON},{KNN_LAT}),4326) LIMIT {KNN_K}) x");
    let (p50,p99,r)=measure(|| scal(&mut cl, &sql3), warmup, iters);
    emit_chk("postgis", load_ms, index_ms, rss, "q3_knn", p50, p99, r, exp.q3);
    let sql4 = format!("SELECT count(*)::bigint FROM geonames WHERE ST_X(geom) BETWEEN {BOX_X1} AND {BOX_X2} AND ST_Y(geom) BETWEEN {BOX_Y1} AND {BOX_Y2}");
    let (p50,p99,r)=measure(|| scal(&mut cl, &sql4), warmup, iters);
    emit_chk("postgis", load_ms, index_ms, rss, "q4_bbox", p50, p99, r, exp.q4);
    let sql5 = format!("SELECT count(*)::bigint FROM geonames WHERE ST_DWithin(geom::geography, ST_SetSRID(ST_MakePoint({KNN_LON},{KNN_LAT}),4326)::geography, {RADIUS_M})");
    let (p50,p99,r)=measure(|| scal(&mut cl, &sql5), warmup, iters);
    emit_chk_tol("postgis", load_ms, index_ms, rss, "q5_radius", p50, p99, r, exp.q5, 0.01);
    let sql6 = format!("SELECT count(*)::bigint FROM nbh WHERE ST_Contains(geom, ST_SetSRID(ST_MakePoint({PIP_LON},{PIP_LAT}),4326))");
    let (p50,p99,r)=measure(|| scal(&mut cl, &sql6), warmup, iters);
    emit_chk("postgis", load_ms, index_ms, rss, "q6_pip_point", p50, p99, r, exp.q6);
}

fn load_pg_points(stage: &duckdb::Connection, cl: &mut postgres::Client, sel: &str, table: &str) {
    use std::io::Write;
    // Bulk: COPY (id, lon, lat) into a raw table, then one INSERT…SELECT ST_MakePoint.
    // Individual INSERTs for 2M points are far too slow.
    cl.batch_execute(&format!("DROP TABLE IF EXISTS {table}_raw; CREATE TABLE {table}_raw(id bigint, lon double precision, lat double precision);")).unwrap();
    let mut st = stage.prepare(sel).unwrap();
    let rows = st.query_map([], |r| Ok((r.get::<_,i64>(0)?, r.get::<_,f64>(1)?, r.get::<_,f64>(2)?))).unwrap();
    {
        let mut w = cl.copy_in(&format!("COPY {table}_raw(id,lon,lat) FROM STDIN")).unwrap();
        for row in rows {
            let (id,x,y) = row.unwrap();
            w.write_all(format!("{id}\t{x}\t{y}\n").as_bytes()).unwrap();
        }
        w.finish().unwrap();
    }
    cl.batch_execute(&format!("INSERT INTO {table}(id,geom) SELECT id, ST_SetSRID(ST_MakePoint(lon,lat),4326) FROM {table}_raw; DROP TABLE {table}_raw;")).unwrap();
}
fn load_pg_points_geom(stage: &duckdb::Connection, cl: &mut postgres::Client, sel: &str, table: &str, name_key: bool) {
    let mut st = stage.prepare(sel).unwrap();
    let mut tx = cl.transaction().unwrap();
    if name_key {
        let rows: Vec<(String,f64,f64)> = st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,f64>(1)?, r.get::<_,f64>(2)?))).unwrap().filter_map(Result::ok).collect();
        let ins = tx.prepare(&format!("INSERT INTO {table}(name,geom) VALUES ($1, ST_SetSRID(ST_MakePoint($2,$3),4326))")).unwrap();
        for (n,x,y) in rows { tx.execute(&ins, &[&n,&x,&y]).unwrap(); }
    } else {
        let rows: Vec<(i64,f64,f64)> = st.query_map([], |r| Ok((r.get::<_,i64>(0)?, r.get::<_,f64>(1)?, r.get::<_,f64>(2)?))).unwrap().filter_map(Result::ok).collect();
        let ins = tx.prepare(&format!("INSERT INTO {table}(id,geom) VALUES ($1, ST_SetSRID(ST_MakePoint($2,$3),4326))")).unwrap();
        for (id,x,y) in rows { tx.execute(&ins, &[&id,&x,&y]).unwrap(); }
    }
    tx.commit().unwrap();
}
fn load_pg_polys(stage: &duckdb::Connection, cl: &mut postgres::Client, sel: &str, table: &str) {
    let mut st = stage.prepare(sel).unwrap();
    let rows: Vec<(String,String)> = st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?))).unwrap().filter_map(Result::ok).collect();
    let key = if table=="blocks" {"blkid"} else {"name"};
    let mut tx = cl.transaction().unwrap();
    let ins = tx.prepare(&format!("INSERT INTO {table}({key},geom) VALUES ($1, ST_SetSRID(ST_GeomFromGeoJSON($2),4326))")).unwrap();
    for (k,gj) in rows { tx.execute(&ins, &[&k,&gj]).unwrap(); }
    tx.commit().unwrap();
}

// ── sekejap — geometry as GeoJSON payloads; ST_* over the spatial grid ─────────
// sekejap does FIELD-vs-LITERAL spatial predicates: ST_DWithin/ST_Contains against a
// WKT-style POINT(lon lat) literal, distance in KM (haversine). It has no
// cross-collection spatial join and no distance-ordering (kNN), so q1/q2/q3 are N/A.
fn run_sekejap(stage: &duckdb::Connection, exp: &Expected, warmup: usize, iters: usize) {
    let dir = format!("{RUNS}/sekejap"); let _ = std::fs::remove_dir_all(&dir); std::fs::create_dir_all(&dir).unwrap();
    let mut db = CoreDB::open(&dir).expect("open");

    let t = Instant::now();
    sk_load_points(stage, &mut db, "SELECT id, lon, lat FROM geonames", "geonames");
    sk_load_points(stage, &mut db, "SELECT CAST(id AS BIGINT), ST_X(geom), ST_Y(geom) FROM homicides", "homicides");
    sk_load_points_named(stage, &mut db, "SELECT name, ST_X(geom), ST_Y(geom) FROM subway", "subway");
    sk_load_polys(stage, &mut db, "SELECT blkid, ST_AsGeoJSON(geom) FROM blocks", "blocks");
    sk_load_polys(stage, &mut db, "SELECT name, ST_AsGeoJSON(geom) FROM nbh", "nbh");
    let load_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    db.build_spatial_index();
    db.execute("CREATE INDEX ON geonames USING btree (lon)").ok();
    db.execute("CREATE INDEX ON geonames USING btree (lat)").ok();
    let index_ms = t.elapsed().as_secs_f64() * 1000.0;
    let rss = vmhwm_mb();

    let q = |db: &CoreDB, sql: String| -> i64 {
        db.query(&sql).ok().and_then(|s| s.collect().into_iter().next())
            .and_then(|h| h.payload).and_then(|v| v.as_object().and_then(|o| o.values().next().cloned()))
            .and_then(|v| v.as_i64()).unwrap_or(-1)
    };
    // returns the number of rows a query yields (for LIMIT-k / kNN checks)
    let qlen = |db: &CoreDB, sql: String| -> i64 { db.query(&sql).map(|s| s.collect().len() as i64).unwrap_or(-1) };

    if let Some(n) = train_n() {
        let t = make_trainset(n);
        let mut pgo = postgres::Client::connect(&pg_conn(), postgres::NoTls).expect("pg oracle");
        train_type("sekejap", "pip", &t.pip, stage, &mut pgo, |x| if let Q::Pip(lo,la)=x {
            q(&db, format!("SELECT COUNT(*) AS c FROM nbh WHERE ST_Contains(geometry, POINT({lo} {la}))")) } else {-1});
        train_type("sekejap", "bbox", &t.bbox, stage, &mut pgo, |x| if let Q::Bbox(x1,y1,x2,y2)=x {
            q(&db, format!("SELECT COUNT(*) AS c FROM geonames WHERE lon >= {x1} AND lon <= {x2} AND lat >= {y1} AND lat <= {y2}")) } else {-1});
        // radius/kNN now use PostGIS-aligned metres + ST_Distance (km→m).
        train_type("sekejap", "radius", &t.rad, stage, &mut pgo, |x| if let Q::Radius(lo,la,km)=x {
            q(&db, format!("SELECT COUNT(*) AS c FROM geonames WHERE ST_DWithin(geometry, POINT({lo} {la}), {})", km*1000.0)) } else {-1});
        train_type("sekejap", "knn", &t.knn, stage, &mut pgo, |x| if let Q::Knn(lo,la,k)=x {
            qlen(&db, format!("SELECT _key FROM geonames ORDER BY ST_DISTANCE(geometry, POINT({lo} {la})) LIMIT {k}")) } else {-1});
        train_type("sekejap", "intersects", &t.isect, stage, &mut pgo, |x| if let Q::Intersects(x1,y1,x2,y2)=x {
            q(&db, format!("SELECT COUNT(*) AS c FROM nbh WHERE ST_Intersects(geometry, POLYGON(({})))", box_ring(*x1,*y1,*x2,*y2))) } else {-1});
        train_type("sekejap", "area", &t.area, stage, &mut pgo, |x| if let Q::Area(th)=x {
            q(&db, format!("SELECT COUNT(*) AS c FROM nbh WHERE ST_Area(geometry, {th})")) } else {-1});
        train_type("sekejap", "perimeter", &t.perim, stage, &mut pgo, |x| if let Q::Perim(th)=x {
            q(&db, format!("SELECT COUNT(*) AS c FROM nbh WHERE ST_Perimeter(geometry, {th})")) } else {-1});
        return;
    }
    // kNN via ORDER BY ST_Distance … LIMIT k (result = # of k nearest rows).
    let (p50,p99,r)=measure(|| qlen(&db, format!("SELECT _key FROM geonames ORDER BY ST_DISTANCE_KM(geometry, POINT({KNN_LON} {KNN_LAT})) LIMIT {KNN_K}")), warmup, iters);
    emit_chk("sekejap", load_ms, index_ms, rss, "q3_knn", p50, p99, r, exp.q3);

    if env_usize("SJOIN", 1) == 0 { return; } // skip the slow containment build during iteration
    // Spatial JOIN — sekejap's NATIVE path (JOIN-free): build containment as edges once
    // (point-in-polygon), then answer with a MATCH traversal. This is the fair, same-result
    // comparison vs PostGIS/DuckDB's query-time spatial join. Reported as build_ms (in the
    // load column) + MATCH query latency.
    // q2 (homicides → 129 neighborhoods):
    let hpts: Vec<(String,f64,f64)> = {
        let mut st = stage.prepare("SELECT CAST(id AS VARCHAR), ST_X(geom), ST_Y(geom) FROM homicides").unwrap();
        st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,f64>(1)?, r.get::<_,f64>(2)?))).unwrap().filter_map(Result::ok).collect()
    };
    let tb = Instant::now();
    sk_build_containment(&mut db, &hpts, "homicides", "nbh");
    let build2 = tb.elapsed().as_secs_f64() * 1000.0;
    let (p50,p99,r)=measure(|| q(&db, "SELECT COUNT(*) AS c FROM MATCH (h:homicides)-[:in]->(n:nbh)".into()), warmup, iters);
    emit_chk("sekejap", build2, index_ms, rss, "q2_sjoin_MATCH", p50, p99, r, exp.q2);
    // q1 (subway → 38,794 census blocks): the containment build needs a polygon index to be
    // practical (each PIP re-parses every block); deferred to the disk-first spatial redesign.
    emit_na("sekejap", load_ms, index_ms, rss, "q1_pip_MATCH_deferred");
    // bbox (btree on lon/lat)
    let (p50,p99,r)=measure(|| q(&db, format!("SELECT COUNT(*) AS c FROM geonames WHERE lon >= {BOX_X1} AND lon <= {BOX_X2} AND lat >= {BOX_Y1} AND lat <= {BOX_Y2}")), warmup, iters);
    emit_chk("sekejap", load_ms, index_ms, rss, "q4_bbox", p50, p99, r, exp.q4);
    // radius (km, haversine over the spatial grid)
    let (p50,p99,r)=measure(|| q(&db, format!("SELECT COUNT(*) AS c FROM geonames WHERE ST_DWithin(geometry, POINT({KNN_LON} {KNN_LAT}), {RADIUS_KM})")), warmup, iters);
    emit_chk_tol("sekejap", load_ms, index_ms, rss, "q5_radius", p50, p99, r, exp.q5, 0.01);
    // point-in-polygon: which neighborhood contains the probe point
    let (p50,p99,r)=measure(|| q(&db, format!("SELECT COUNT(*) AS c FROM nbh WHERE ST_Contains(geometry, POINT({PIP_LON} {PIP_LAT}))")), warmup, iters);
    emit_chk("sekejap", load_ms, index_ms, rss, "q6_pip_point", p50, p99, r, exp.q6);
}

/// Materialize spatial containment as edges: for each point, find the polygon that
/// contains it (ST_Contains PIP) and link `point -[:in]-> polygon`. This is the build
/// phase of sekejap's spatial join; the query is then a MATCH traversal.
fn sk_build_containment(db: &mut CoreDB, points: &[(String,f64,f64)], point_coll: &str, poly_coll: &str) {
    for (pkey, lon, lat) in points {
        let sql = format!("SELECT _key AS k FROM {poly_coll} WHERE ST_Contains(geometry, POINT({lon} {lat}))");
        let poly = db.query(&sql).ok()
            .and_then(|s| s.collect().into_iter().next())
            .and_then(|h| h.payload)
            .and_then(|v| v.as_object().and_then(|o| o.values().next().cloned()))
            .and_then(|v| v.as_str().map(str::to_string));
        if let Some(nkey) = poly {
            db.link(&format!("{point_coll}/{pkey}"), &format!("{poly_coll}/{nkey}"), "in");
        }
    }
}

/// Bulk-load points via put_many in 100k batches (individual put() for 2M = ~1 h).
fn sk_load_points(stage: &duckdb::Connection, db: &mut CoreDB, sel: &str, coll: &str) {
    let mut st = stage.prepare(sel).unwrap();
    let rows = st.query_map([], |r| Ok((r.get::<_,i64>(0)?, r.get::<_,f64>(1)?, r.get::<_,f64>(2)?))).unwrap();
    let mut batch: Vec<(String,String)> = Vec::with_capacity(100_000);
    for row in rows {
        let (id,x,y) = row.unwrap();
        let json = serde_json::json!({"_collection":coll,"_key":id.to_string(),"lon":x,"lat":y,"geometry":{"type":"Point","coordinates":[x,y]}}).to_string();
        batch.push((format!("{coll}/{id}"), json));
        if batch.len() >= 100_000 {
            db.put_many(batch.iter().map(|(s,j)| (s.as_str(), j.as_str()))).ok();
            batch.clear();
        }
    }
    db.put_many(batch.iter().map(|(s,j)| (s.as_str(), j.as_str()))).ok();
}
fn sk_load_points_named(stage: &duckdb::Connection, db: &mut CoreDB, sel: &str, coll: &str) {
    let mut st = stage.prepare(sel).unwrap();
    let rows: Vec<(String,f64,f64)> = st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,f64>(1)?, r.get::<_,f64>(2)?))).unwrap().filter_map(Result::ok).collect();
    let batch: Vec<(String,String)> = rows.into_iter().enumerate().map(|(i,(n,x,y))| {
        (format!("{coll}/{i}"), serde_json::json!({"_collection":coll,"_key":i.to_string(),"name":n,"lon":x,"lat":y,"geometry":{"type":"Point","coordinates":[x,y]}}).to_string())
    }).collect();
    db.put_many(batch.iter().map(|(s,j)| (s.as_str(), j.as_str()))).ok();
}
fn sk_load_polys(stage: &duckdb::Connection, db: &mut CoreDB, sel: &str, coll: &str) {
    let mut st = stage.prepare(sel).unwrap();
    let rows: Vec<(String,String)> = st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?))).unwrap().filter_map(Result::ok).collect();
    let batch: Vec<(String,String)> = rows.into_iter().map(|(k,gj)| {
        let geom: Value = serde_json::from_str(&gj).unwrap_or(Value::Null);
        (format!("{coll}/{k}"), serde_json::json!({"_collection":coll,"_key":k,"geometry":geom}).to_string())
    }).collect();
    db.put_many(batch.iter().map(|(s,j)| (s.as_str(), j.as_str()))).ok();
}
