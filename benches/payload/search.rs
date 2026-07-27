//! Is SKBIN related to vector / BM25 / search? Measures two things across
//! raw-resident vs SKBIN-resident:
//!   (A) INDEX BUILD time — GIN & BM25 scan a payload field (SKBIN skip-scan) ;
//!       HNSW reads the vector store (SKBIN neutral).
//!   (B) QUERY time — ranking reads the index (neutral); projecting result fields
//!       reads payloads (SKBIN), scaling with result-set size.
//!
//!   cargo bench --bench payload_search

use sekejap::{Config, CoreDB};
use serde_json::json;
use std::time::Instant;

const VENUES: usize = 20_000;
const VEC_DIM: usize = 64;
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
fn vcontent(i: usize) -> String {
    if i % 5 == 0 { format!("Popular venue near the Maribyrnong River in {}, a {}.", vsub(i), vcat(i)) }
    else { format!("A great {} in {}. Visit the river and the park nearby.", vcat(i), vsub(i)) }
}

fn build_data(dir: &std::path::Path, binary: bool) {
    let cfg = Config { payload_binary: binary, ..Config::default() };
    let mut db = CoreDB::open_with_config(dir, cfg).unwrap();
    for i in 0..VENUES {
        let slug = format!("venues/v{i}");
        db.put(&slug, &json!({
            "_collection":"venues","_key":format!("v{i}"),"name":format!("Venue {i}"),
            "category":vcat(i),"suburb":vsub(i),"content":vcontent(i),
        }).to_string()).unwrap();
        db.put_vector(&slug, "emb", &make_vec(i)).unwrap();
    }
    db.compact().unwrap(); // payloads now SKBIN (if binary)
}

fn open(dir: &std::path::Path, binary: bool) -> CoreDB {
    CoreDB::open_with_config(dir, Config { payload_binary: binary, ..Config::default() }).unwrap()
}

fn time_build(dir: &std::path::Path, binary: bool, which: &str) -> f64 {
    let mut db = open(dir, binary);
    let t = Instant::now();
    match which {
        "gin"  => db.build_gin_index("content"),
        "bm25" => db.build_bm25_index("content"),
        "hnsw" => { db.build_hnsw_index("emb", 16, 200).unwrap(); }
        _ => unreachable!(),
    }
    t.elapsed().as_secs_f64() * 1e3
}

fn time_query(db: &CoreDB, sql: &str, reps: usize) -> (usize, f64) {
    let n = db.query(sql).unwrap().collect().len();
    let t = Instant::now();
    for _ in 0..reps { std::hint::black_box(db.query(sql).unwrap().collect().len()); }
    (n, t.elapsed().as_secs_f64() * 1e3 / reps as f64)
}

fn main() {
    println!("== vector / BM25 / search benchmark: raw-resident vs SKBIN-resident ==");
    println!("venues={VENUES} (payload + {VEC_DIM}-dim vectors)\n");
    let d_raw = tempfile::tempdir().unwrap();
    let d_bin = tempfile::tempdir().unwrap();
    print!("building raw…");   build_data(d_raw.path(), false); println!(" done");
    print!("building SKBIN…"); build_data(d_bin.path(), true);  println!(" done\n");

    // ── (A) INDEX BUILD TIMES ────────────────────────────────────────────────
    println!("[BUILD] index construction (ms) — scans payload field or vector store");
    println!("{:<28} {:>10} | {:>11}   {}", "index", "raw", "SKBIN", "scans");
    println!("{}", "-".repeat(72));
    for (which, scans) in [("gin","payload field (content)"), ("bm25","payload field (content)"), ("hnsw","vector store (emb)")] {
        let r = time_build(d_raw.path(), false, which);
        let b = time_build(d_bin.path(), true,  which);
        println!("{:<28} {r:>8.1}ms | {b:>9.1}ms ({:.1}x)  {scans}", format!("build {which}"), r / b);
    }

    // ── (B) QUERY TIMES ──────────────────────────────────────────────────────
    let mut raw = open(d_raw.path(), false);
    let mut bin = open(d_bin.path(), true);
    for db in [&mut raw, &mut bin] {
        db.build_gin_index("content");
        db.build_bm25_index("content");
        db.build_hnsw_index("emb", 16, 200).unwrap();
    }
    let qv: Vec<String> = make_vec(VENUES + 2).iter().map(|f| format!("{f:.4}")).collect();
    let vlit = qv.join(",");

    let scenarios: Vec<(&str, String, usize, &str)> = vec![
        ("bm25 rank COUNT",       "SELECT COUNT(*) FROM venues WHERE content ILIKE '%river%'".into(), 5, "index only"),
        ("bm25 rank PROJECT top20", "SELECT _key, name, content, BM25('content','river venue') AS s FROM venues ORDER BY s DESC LIMIT 20".into(), 10, "project 20 (SKBIN)"),
        ("gin ilike PROJECT (many)", "SELECT _key, name, content FROM venues WHERE content ILIKE '%river%'".into(), 5, "project matches (SKBIN)"),
        ("vector top-20 PROJECT",  format!("SELECT _key, name, category FROM venues WHERE VECTOR_NEAR(emb, [{vlit}], 20)"), 10, "project 20 (SKBIN, tiny)"),
        ("vector top-500 PROJECT", format!("SELECT _key, name, category FROM venues WHERE VECTOR_NEAR(emb, [{vlit}], 500)"), 10, "project 500 (SKBIN)"),
    ];
    println!("\n[QUERY] latency (ms) — ranking neutral, projection reads payload");
    println!("{:<28} {:>6}  {:>10} | {:>11}   {}", "scenario", "rows", "raw", "SKBIN", "payload touched");
    println!("{}", "-".repeat(88));
    for (label, sql, reps, touch) in &scenarios {
        let (rn, rt) = time_query(&raw, sql, *reps);
        let (bn, bt) = time_query(&bin, sql, *reps);
        let flag = if rn == bn { "" } else { " ⚠ROWS DIFFER" };
        println!("{label:<28} {rn:>6}  {rt:>8.2}ms | {bt:>9.2}ms ({:.1}x)  {touch}{flag}", rt / bt);
    }
    println!("\nBuild: GIN/BM25 scan a payload field → SKBIN skip-scan speeds it; HNSW is neutral.");
    println!("Query: ranking is neutral; SKBIN's win grows with how many result fields you project.");
}
