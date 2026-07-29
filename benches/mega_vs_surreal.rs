//! Mega comparison: sekejap vs SurrealDB (embedded, SurrealKV — pure-Rust disk).
//!
//! Both engines are disk-backed and built with the same dataset + equivalent
//! indexes, then run the curated subset of the mega-benchmark scenarios that map
//! cleanly to both — filters, sort, point lookup, graph 1-hop, vector KNN, and
//! spatial distance. Scenarios with no fair SurrealDB equivalent (MATCH SHORTEST,
//! polygon ST_Within, graph→vector re-rank) are intentionally omitted.
//!
//!   cargo bench --features surreal-bench --bench mega_vs_surreal
//!
//! Env: VENUES=<n> (default 20000) to shrink for a quick smoke run.

use std::time::Instant;

use sekejap::{Config, CoreDB};
use serde_json::json;
use surrealdb::engine::local::{Db, SurrealKv};
use surrealdb::Surreal;

const CATEGORIES: &[&str] =
    &["cafe", "restaurant", "park", "hospital", "school", "shop", "office", "gym", "clinic", "library"];
const SUBURBS: &[&str] =
    &["fitzroy", "melbourne", "collingwood", "richmond", "carlton", "brunswick", "northcote", "prahran", "southbank", "docklands"];
const VEC_DIM: usize = 64;
const CENTRE_LAT: f64 = -37.8136;
const CENTRE_LON: f64 = 144.9631;

fn vcat(i: usize) -> &'static str { CATEGORIES[i % CATEGORIES.len()] }
fn vsub(i: usize) -> &'static str { SUBURBS[i % SUBURBS.len()] }
fn vrat(i: usize) -> f64 { 1.0 + (i % 40) as f64 * 0.1 }
fn vprice(i: usize) -> f64 { 10.0 + (i % 49) as f64 * 10.0 }
fn vlon(i: usize) -> f64 { CENTRE_LON + (i % 500) as f64 * 0.001 }
fn vlat(i: usize) -> f64 { CENTRE_LAT + (i % 400) as f64 * 0.001 }
fn vemb(i: usize) -> Vec<f64> {
    (0..VEC_DIM).map(|d| ((i + d) % 100) as f64 / 100.0).collect()
}
fn vec_literal(v: &[f64]) -> String {
    let parts: Vec<String> = v.iter().map(|x| format!("{x:.3}")).collect();
    format!("[{}]", parts.join(","))
}

fn n_venues() -> usize {
    std::env::var("VENUES").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000)
}

// ── sekejap ──────────────────────────────────────────────────────────────────

fn build_sekejap(dir: &std::path::Path, n: usize) -> CoreDB {
    let mut db = CoreDB::open_with_config(dir, Config::default()).unwrap();
    let rows: Vec<(String, serde_json::Value)> = (0..n)
        .map(|i| {
            (
                format!("venues/v{i}"),
                json!({
                    "_collection": "venues", "_key": format!("v{i}"),
                    "category": vcat(i), "suburb": vsub(i),
                    "rating": vrat(i), "price": vprice(i),
                    "embedding": vemb(i),
                    "geometry": {"type": "Point", "coordinates": [vlon(i), vlat(i)]},
                }),
            )
        })
        .collect();
    db.put_value_bulk(rows).unwrap();
    // sekejap vectors live in a dedicated vector store (HNSW indexes THAT, not the
    // JSON payload field), so register each embedding explicitly.
    for i in 0..n {
        let e: Vec<f32> = vemb(i).iter().map(|&x| x as f32).collect();
        db.put_vector(&format!("venues/v{i}"), "embedding", &e).unwrap();
    }
    db.execute("CREATE INDEX ON venues USING hash (category)").ok();
    db.execute("CREATE INDEX ON venues USING hash (suburb)").ok();
    db.execute("CREATE INDEX ON venues USING btree (price)").ok();
    db.execute("CREATE INDEX ON venues USING btree (rating)").ok();
    db.execute("CREATE INDEX ON venues USING hnsw (embedding)").ok();
    db.execute("CREATE INDEX ON venues USING spatial (geometry)").ok();

    // related_to edges: 3 per venue, skip-linked — bulk insert (single fsync).
    let mut slugs: Vec<(String, String)> = Vec::with_capacity(n * 3);
    for i in 0..n {
        for k in 1..=3 {
            let j = (i + k * 7) % n;
            slugs.push((format!("venues/v{i}"), format!("venues/v{j}")));
        }
    }
    db.link_many(slugs.iter().map(|(f, t)| (f.as_str(), t.as_str(), "related_to")));
    db.compact().unwrap();
    db
}

fn sk_count(db: &CoreDB, sql: &str) -> usize {
    db.query(sql).unwrap().collect().len()
}

// ── SurrealDB ─────────────────────────────────────────────────────────────────

async fn build_surreal(path: &str, n: usize) -> Surreal<Db> {
    let db = Surreal::new::<SurrealKv>(path).await.unwrap();
    db.use_ns("bench").use_db("bench").await.unwrap();

    // Coerce the geometry field so geo::distance works on it.
    db.query("DEFINE FIELD geometry ON venues TYPE geometry<point>")
        .await.unwrap().check().unwrap();

    // CREATE with explicit ident record ids (venues:vN) so RELATE + KNN resolve
    // them. Bulk INSERT with an "id" string produces a string id that RELATE
    // can't match — the classic SurrealDB record-id gotcha.
    for k in 0..n {
        let content = json!({
            "key": format!("v{k}"),
            "category": vcat(k), "suburb": vsub(k),
            "rating": vrat(k), "price": vprice(k),
            "embedding": vemb(k),
            "geometry": {"type": "Point", "coordinates": [vlon(k), vlat(k)]},
        });
        db.query(format!("CREATE venues:v{k} CONTENT {}", serde_json::to_string(&content).unwrap()))
            .await.unwrap().check().unwrap();
    }

    for stmt in [
        "DEFINE INDEX cat ON venues FIELDS category".to_string(),
        "DEFINE INDEX sub ON venues FIELDS suburb".to_string(),
        "DEFINE INDEX pri ON venues FIELDS price".to_string(),
        "DEFINE INDEX rat ON venues FIELDS rating".to_string(),
        "DEFINE INDEX keyx ON venues FIELDS key UNIQUE".to_string(),
        format!("DEFINE INDEX emb ON venues FIELDS embedding MTREE DIMENSION {VEC_DIM} DIST COSINE TYPE F32"),
    ] {
        db.query(stmt).await.unwrap().check().unwrap();
    }

    let mut slugs: Vec<(usize, usize)> = Vec::with_capacity(n * 3);
    for k in 1..=3usize {
        for i in 0..n {
            slugs.push((i, (i + k * 7) % n));
        }
    }
    for (i, j) in slugs {
        db.query(format!("RELATE venues:v{i}->related_to->venues:v{j}"))
            .await.unwrap().check().unwrap();
    }
    db
}

/// Run a SurrealDB query and return the row count, or None if it errored (so a
/// single bad SurrealQL syntax prints ERR instead of killing the whole run).
fn sr_count(rt: &tokio::runtime::Runtime, db: &Surreal<Db>, sql: &str) -> Option<usize> {
    rt.block_on(async {
        let mut res = db.query(sql).await.ok()?;
        let rows: Vec<serde_json::Value> = res.take(0).ok()?;
        Some(rows.len())
    })
}

// ── driver ────────────────────────────────────────────────────────────────────

fn time_sk<F: FnMut() -> usize>(reps: usize, mut f: F) -> (usize, f64) {
    let n = f();
    let t = Instant::now();
    for _ in 0..reps { std::hint::black_box(f()); }
    (n, t.elapsed().as_secs_f64() * 1e3 / reps as f64)
}

fn time_sr<F: FnMut() -> Option<usize>>(reps: usize, mut f: F) -> (Option<usize>, f64) {
    let n = f();
    if n.is_none() { return (None, f64::NAN); }
    let t = Instant::now();
    for _ in 0..reps { std::hint::black_box(f()); }
    (n, t.elapsed().as_secs_f64() * 1e3 / reps as f64)
}

fn row(label: &str, skn: usize, skt: f64, srn: Option<usize>, srt: f64) {
    match srn {
        Some(srn) => {
            let flag = if skn == srn { "" } else { " (row mismatch!)" };
            println!("{label:<20} {}{:>8} {skt:>12.3} {srt:>12.3} {:>8.2}x{flag}",
                     format!("{skn}/"), srn, srt / skt);
        }
        None => println!("{label:<20} {skn:>9}/ERR {skt:>12.3} {:>12} {:>9}", "ERR", "—"),
    }
}

fn main() {
    let n = n_venues();
    println!("== sekejap vs SurrealDB (embedded SurrealKV, disk) — {n} venues ==\n");

    let rt = tokio::runtime::Runtime::new().unwrap();
    let sk_dir = tempfile::tempdir().unwrap();
    let sr_dir = tempfile::tempdir().unwrap();

    print!("building sekejap…");
    let t = Instant::now();
    let sk = build_sekejap(sk_dir.path(), n);
    println!(" {:.1}s", t.elapsed().as_secs_f64());

    print!("building surrealdb…");
    let t = Instant::now();
    let sr = rt.block_on(build_surreal(sr_dir.path().join("db").to_str().unwrap(), n));
    println!(" {:.1}s\n", t.elapsed().as_secs_f64());

    // Keys/params scaled to n so they always hit real data.
    let lookup = format!("v{}", n / 2);
    let hop = format!("v{}", n / 4);
    let qvec = vec_literal(&vemb(n / 3));
    let cbd = format!("[{CENTRE_LON}, {CENTRE_LAT}]");

    println!("{:<20} {:>10} {:>12} {:>12} {:>9}", "case", "rows sk/sr", "sekejap ms", "surreal ms", "speedup");
    println!("{}", "-".repeat(68));

    let cases: Vec<(&str, String, String, usize)> = vec![
        ("eq_filter",
         "SELECT _key FROM venues WHERE category = 'cafe'".into(),
         "SELECT key FROM venues WHERE category = 'cafe'".into(), 20),
        ("neq_filter",
         "SELECT _key FROM venues WHERE category != 'hospital'".into(),
         "SELECT key FROM venues WHERE category != 'hospital'".into(), 10),
        ("range_filter",
         "SELECT _key FROM venues WHERE price > 100 AND price <= 300".into(),
         "SELECT key FROM venues WHERE price > 100 AND price <= 300".into(), 20),
        ("sort_limit",
         "SELECT _key, rating FROM venues ORDER BY rating DESC LIMIT 50".into(),
         "SELECT key, rating FROM venues ORDER BY rating DESC LIMIT 50".into(), 20),
        ("point_lookup",
         format!("SELECT _key FROM venues WHERE _key = '{lookup}'"),
         format!("SELECT key FROM venues WHERE key = '{lookup}'"), 100),
        ("compound_filter",
         "SELECT _key FROM venues WHERE category = 'cafe' AND suburb = 'fitzroy'".into(),
         "SELECT key FROM venues WHERE category = 'cafe' AND suburb = 'fitzroy'".into(), 20),
    ];
    for (label, sksql, srsql, reps) in &cases {
        let (skn, skt) = time_sk(*reps, || sk_count(&sk, sksql));
        let (srn, srt) = time_sr(*reps, || sr_count(&rt, &sr, srsql));
        row(label, skn, skt, srn, srt);
    }

    // graph 1-hop
    {
        let sksql = format!("SELECT b._key AS k FROM MATCH (a:venues)-[:related_to]->(b:venues) WHERE a._key = '{hop}'");
        // FROM record->edge->table returns one row per neighbor venue (comparable
        // to sekejap's row-per-path), not a single nested-array row.
        let srsql = format!("SELECT key FROM venues:{hop}->related_to->venues");
        let (skn, skt) = time_sk(20, || sk_count(&sk, &sksql));
        let (srn, srt) = time_sr(20, || sr_count(&rt, &sr, &srsql));
        row("graph_1hop", skn, skt, srn, srt);
    }
    // vector KNN (top 20)
    {
        let sksql = format!("SELECT _key FROM venues WHERE VECTOR_NEAR(embedding, {qvec}, 20)");
        let srsql = format!("SELECT key FROM venues WHERE embedding <|20|> {qvec}");
        let (skn, skt) = time_sk(20, || sk_count(&sk, &sksql));
        let (srn, srt) = time_sr(20, || sr_count(&rt, &sr, &srsql));
        row("vector_knn_top20", skn, skt, srn, srt);
    }
    // spatial distance (5 km around CBD)
    {
        let sksql = format!("SELECT _key FROM venues WHERE ST_DWithin(geometry, POINT({CENTRE_LON} {CENTRE_LAT}), 5.0)");
        let srsql = format!("SELECT key FROM venues WHERE geo::distance(geometry, {{type:'Point', coordinates:{cbd}}}) < 5000");
        let (skn, skt) = time_sk(20, || sk_count(&sk, &sksql));
        let (srn, srt) = time_sr(20, || sr_count(&rt, &sr, &srsql));
        row("spatial_5km", skn, skt, srn, srt);
    }

    println!("\nN/A for SurrealDB (no fair equivalent): MATCH SHORTEST, ST_Within polygon, graph→vector re-rank.");
}
