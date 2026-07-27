//! Where does SKBIN matter in HYBRID queries (spatial + vector + graph + search)?
//! Builds venues with payload + 64-dim vectors + spatial + GIN, then runs hybrids
//! as COUNT (no payload read → SKBIN neutral) vs field-PROJECTION (payload read →
//! SKBIN accelerates the last mile). Compares raw-resident vs SKBIN-resident.
//! (Paged excluded: spatial metadata isn't served from the paged base.)
//!
//!   cargo bench --bench payload_hybrid

use sekejap::{Config, CoreDB};
use serde_json::json;
use std::time::Instant;

const VENUES: usize = 20_000;
const VEC_DIM: usize = 64;
const CENTRE_LAT: f64 = -37.8136;
const CENTRE_LON: f64 = 144.9631;
const CATEGORIES: &[&str] = &["cafe","restaurant","park","hospital","school","shop","office","gym","clinic","library"];
const SUBURBS: &[&str] = &["fitzroy","melbourne","collingwood","richmond","carlton","brunswick","northcote","prahran","southbank","docklands"];

fn make_vec(seed: usize) -> Vec<f32> {
    (0..VEC_DIM).map(|i| {
        let x = seed.wrapping_mul(6364136223846793005).wrapping_add(i.wrapping_mul(1442695040888963407));
        (x >> 33) as f32 / u32::MAX as f32 * 2.0 - 1.0
    }).collect()
}
fn vcat(i: usize) -> &'static str { CATEGORIES[i % CATEGORIES.len()] }
fn vsub(i: usize) -> &'static str { SUBURBS[i % SUBURBS.len()] }
fn vrat(i: usize) -> f64 { 1.0 + (i % 40) as f64 * 0.1 }
fn vprice(i: usize) -> f64 { 10.0 + (i % 49) as f64 * 10.0 }
fn vcontent(i: usize) -> String {
    if i % 5 == 0 { format!("Popular venue near the Maribyrnong River in {}, a {}.", vsub(i), vcat(i)) }
    else { format!("A great {} in {}. Rated {:.1} stars, price {:.0}.", vcat(i), vsub(i), vrat(i), vprice(i)) }
}

fn build(dir: &std::path::Path, binary: bool) {
    let cfg = Config { payload_binary: binary, ..Config::default() };
    let mut db = CoreDB::open_with_config(dir, cfg).unwrap();
    for i in 0..VENUES {
        let slug = format!("venues/v{i}");
        db.put(&slug, &json!({
            "_collection":"venues","_key":format!("v{i}"),"name":format!("Venue {i}"),
            "category":vcat(i),"suburb":vsub(i),"rating":vrat(i),"price":vprice(i),"content":vcontent(i),
            "geometry":{"type":"Point","coordinates":[CENTRE_LON-0.25+(i%500) as f64*0.001, CENTRE_LAT-0.20+(i%400) as f64*0.001]},
        }).to_string()).unwrap();
        db.put_vector(&slug, "emb", &make_vec(i)).unwrap();
    }
    db.build_spatial_index();
    db.build_gin_index("content");
    db.build_hnsw_index("emb", 16, 200).unwrap();
    db.compact().unwrap();
}

fn time_q(dir: &std::path::Path, binary: bool, sql: &str, reps: usize) -> (usize, f64) {
    let cfg = Config { payload_binary: binary, ..Config::default() };
    let db = CoreDB::open_with_config(dir, cfg).unwrap();
    let n = db.query(sql).unwrap().collect().len();
    let t = Instant::now();
    for _ in 0..reps { std::hint::black_box(db.query(sql).unwrap().collect().len()); }
    (n, t.elapsed().as_secs_f64() * 1e3 / reps as f64)
}

fn main() {
    println!("== hybrid query benchmark: raw-resident vs SKBIN-resident ==");
    println!("venues={VENUES} (payload + {VEC_DIM}-dim vectors + spatial + GIN)\n");
    let d_raw = tempfile::tempdir().unwrap();
    let d_bin = tempfile::tempdir().unwrap();
    print!("building raw…");   build(d_raw.path(), false); println!(" done");
    print!("building SKBIN…"); build(d_bin.path(), true);  println!(" done\n");

    let qv: Vec<String> = make_vec(VENUES + 2).iter().map(|f| format!("{f:.4}")).collect();
    let vec_lit = qv.join(",");

    // (label, sql, reps, "what touches payload?")
    let scenarios: Vec<(&str, String, usize, &str)> = vec![
        ("spatial COUNT",        format!("SELECT COUNT(*) FROM venues WHERE ST_DWithin(geometry, POINT({CENTRE_LON} {CENTRE_LAT}), 5.0)"), 5, "none (spatial index only)"),
        ("spatial+attr COUNT",   format!("SELECT COUNT(*) FROM venues WHERE ST_DWithin(geometry, POINT({CENTRE_LON} {CENTRE_LAT}), 5.0) AND category='hospital'"), 5, "attr filter (SKBIN)"),
        ("spatial+attr PROJECT", format!("SELECT _key, name, rating FROM venues WHERE ST_DWithin(geometry, POINT({CENTRE_LON} {CENTRE_LAT}), 5.0) AND category='hospital'"), 5, "attr filter + projection (SKBIN)"),
        ("ilike COUNT",          "SELECT COUNT(*) FROM venues WHERE content ILIKE '%Maribyrnong%'".into(), 5, "none (GIN index only)"),
        ("ilike PROJECT",        "SELECT _key, name, category, rating FROM venues WHERE content ILIKE '%Maribyrnong%'".into(), 5, "projection of matches (SKBIN)"),
        ("vector top-20 PROJECT",format!("SELECT _key, name, category FROM venues WHERE VECTOR_NEAR(emb, [{vec_lit}], 20)"), 10, "projection of top-20 (SKBIN, tiny set)"),
    ];

    println!("{:<24} {:>6}  {:>10} | {:>11}   {}", "scenario", "rows", "raw-resid", "SKBIN-res", "payload touched");
    println!("{}", "-".repeat(92));
    for (label, sql, reps, touch) in &scenarios {
        let (rn, rt) = time_q(d_raw.path(), false, sql, *reps);
        let (bn, bt) = time_q(d_bin.path(), true,  sql, *reps);
        let flag = if rn == bn { "" } else { " ⚠ROWS DIFFER" };
        println!("{label:<24} {rn:>6}  {rt:>8.2}ms | {bt:>8.2}ms ({:.1}x)  {touch}{flag}", rt / bt);
    }
    println!("\nSKBIN is neutral where no payload is read (COUNT over spatial/GIN),");
    println!("and speeds the stages that read payload fields (attr filters, projection).");
}
