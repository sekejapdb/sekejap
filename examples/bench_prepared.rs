//! Prepared statements across query shapes: filter, sort, point, graph MATCH,
//! aggregation, spatial, vector, and hybrid. For each, run BOTH ways (prepared vs
//! query_params), assert identical results (correctness), and time the speedup.
//!
//!   cargo run --release --example bench_prepared
use sekejap::{Config, CoreDB};
use serde_json::{json, Value};
use std::io::Write;
use std::time::Instant;

const N: usize = 1000;
const VEC_DIM: usize = 64;
const CATS: &[&str] = &["cafe", "restaurant", "park", "hospital", "school", "shop", "gym", "clinic"];

fn emb(i: usize) -> Vec<f32> {
    (0..VEC_DIM).map(|d| ((i + d) % 100) as f32 / 100.0).collect()
}
fn vec_lit(v: &[f32]) -> String {
    format!("[{}]", v.iter().map(|x| format!("{x:.3}")).collect::<Vec<_>>().join(","))
}

fn build() -> CoreDB {
    let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let mut db = CoreDB::open_with_config(dir.path(), Config::default()).unwrap();
    let rows: Vec<(String, Value)> = (0..N).map(|i| (
        format!("venues/v{i}"),
        json!({
            "_collection":"venues","_key":format!("v{i}"),
            "category":CATS[i % CATS.len()], "rating":1.0 + (i%40) as f64*0.1,
            "price":10.0 + (i%49) as f64*10.0,
            "content":format!("a great {} near the river, rated well", CATS[i%CATS.len()]),
            "geometry":{"type":"Point","coordinates":[144.96 + (i%400) as f64*0.001, -37.81 + (i%300) as f64*0.001]},
        }),
    )).collect();
    db.put_value_bulk(rows).unwrap();
    for i in 0..N { db.put_vector(&format!("venues/v{i}"), "embedding", &emb(i)).unwrap(); }
    db.execute("CREATE INDEX ON venues USING hash (category)").ok();
    db.execute("CREATE INDEX ON venues USING btree (price)").ok();
    db.execute("CREATE INDEX ON venues USING btree (rating)").ok();
    db.execute("CREATE INDEX ON venues USING spatial (geometry)").ok();
    db.execute("CREATE INDEX ON venues USING hnsw (embedding)").ok();
    db.execute("CREATE INDEX ON venues USING bm25 (content)").ok();
    let mut edges = Vec::new();
    for i in 0..N { for k in 1..=3 { edges.push((format!("venues/v{i}"), format!("venues/v{}", (i + k*7) % N))); } }
    db.link_many(edges.iter().map(|(f, t)| (f.as_str(), t.as_str(), "near")));
    db.compact().unwrap();
    db
}

fn slugs(db: &CoreDB, hits: Vec<sekejap::Hit>) -> Vec<String> {
    let _ = db;
    let mut s: Vec<String> = hits.into_iter().map(|h| {
        // key by slug + a stable projection of the payload so agg rows compare too
        format!("{}|{}", h.slug, h.payload.map(|p| p.to_string()).unwrap_or_default())
    }).collect();
    s.sort();
    s
}

fn main() {
    let db = build();
    let qv = vec_lit(&emb(1234));

    // (label, sql with $N where params apply, params)
    let cases: Vec<(&str, String, Vec<Value>)> = vec![
        ("eq_filter",   "SELECT _key FROM venues WHERE category = $1".into(), vec![json!("cafe")]),
        ("range_sort",  "SELECT _key, rating FROM venues WHERE price >= $1 AND price <= $2 ORDER BY rating DESC LIMIT 20".into(), vec![json!(100), json!(300)]),
        ("point_lookup","SELECT _key FROM venues WHERE _key = $1".into(), vec![json!("v500")]),
        ("match_1hop",  "SELECT b._key AS k FROM MATCH (a:venues)-[:near]->(b:venues) WHERE a._key = $1".into(), vec![json!("v100")]),
        ("match_agg",   "SELECT b.category AS c, COUNT(*) AS n FROM MATCH (a:venues)-[:near]->(b:venues) WHERE a._key = $1 GROUP BY b.category".into(), vec![json!("v1000")]),
        ("spatial",     "SELECT _key FROM venues WHERE ST_DWithin(geometry, POINT(144.96 -37.81), 5.0) AND category = $1".into(), vec![json!("cafe")]),
        ("vector_knn",  format!("SELECT _key FROM venues WHERE VECTOR_NEAR(embedding, {qv}, 10)"), vec![]),
        ("bm25_filter", "SELECT _key FROM venues WHERE BM25(content, 'cafe river') > 0.0 AND category = $1".into(), vec![json!("cafe")]),
        ("hybrid_rank", format!("SELECT _key FROM venues WHERE BM25(content,'cafe river')>0.0 ORDER BY BM25_NORM(content,'cafe river')*0.5 + VECTOR_COSINE(embedding,{qv})*0.5 DESC LIMIT 10"), vec![]),
        ("hybrid_spatial_vec", format!("SELECT _key FROM venues WHERE ST_DWithin(geometry, POINT(144.96 -37.81), 3.0) ORDER BY VECTOR_COSINE(embedding,{qv}) DESC LIMIT 10"), vec![]),
    ];

    println!("{:<22} {:>6} {:>12} {:>12} {:>9}  {}", "query shape", "rows", "not-prep ns", "prepared ns", "speedup", "correct?");
    println!("{}", "-".repeat(80));

    for (label, sql, params) in &cases {
        // correctness: prepared result must equal query_params result
        let stmt = match db.prepare(sql) {
            Ok(s) => s,
            Err(e) => { println!("{label:<22} prepare ERR: {e}"); continue; }
        };
        let qp = db.query_params(sql, params);
        let qp2 = db.query_params(sql, params); // self-consistency: same call twice
        let pp = db.query_prepared(&stmt, params);
        let (verdict, rows) = match (qp, qp2, pp) {
            (Ok(a), Ok(a2), Ok(b)) => {
                let (sa, sa2, sb) = (slugs(&db, a.collect()), slugs(&db, a2.collect()), slugs(&db, b.collect()));
                let v = if sa != sa2 { "NONDET (not prepared)" }
                        else if sa == sb { "OK" }
                        else { "PREPARED-BUG!" };
                (v, sa.len())
            }
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => { println!("{label:<22} query ERR: {e}"); continue; }
        };

        // Adaptive reps: ~30ms budget per query so slow BM25 queries don't blow up
        // the run. Probe one call, then size the loop to the budget.
        let probe = { let t = Instant::now(); let _ = db.query_params(sql, params).unwrap().collect(); t.elapsed().as_secs_f64() };
        let reps = (0.03 / probe.max(1e-9)).clamp(10.0, 50_000.0) as usize;

        let t = Instant::now();
        for _ in 0..reps { std::hint::black_box(db.query_params(sql, params).unwrap().collect().len()); }
        let np = t.elapsed().as_secs_f64() * 1e9 / reps as f64;
        let t = Instant::now();
        for _ in 0..reps { std::hint::black_box(db.query_prepared(&stmt, params).unwrap().collect().len()); }
        let pr = t.elapsed().as_secs_f64() * 1e9 / reps as f64;

        println!("{label:<22} {rows:>6} {np:>12.0} {pr:>12.0} {:>8.2}x  {verdict}", np / pr);
        let _ = std::io::stdout().flush();
    }
}
