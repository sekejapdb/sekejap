// Hybrid cross-model query — the differentiator: scalar ∩ spatial ∩ text ∩ vector
// resolved in ONE statement (single-pass index intersection). A composed stack
// (SQLite/DuckDB + extensions) must run 4 queries + app-level joins for the same
// answer. This measures the sekejap side (latency + RSS); the composed baseline
// is run separately.
//
//   cargo run --release --example hybrid_bench [N]
use std::time::Instant;
use sekejap::CoreDB;

fn rss_mb() -> f64 {
    let pid = std::process::id().to_string();
    std::process::Command::new("ps").args(["-o", "rss=", "-p", &pid]).output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map(|kb| kb / 1024.0).unwrap_or(0.0)
}

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(50_000);
    let dir = std::env::temp_dir().join("sk-hybrid");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = CoreDB::open(&dir).unwrap();

    let cats = ["cafe", "resto", "bar", "bakery", "hotel"];
    let words = ["coffee", "brunch", "wine", "grilled", "vegan", "spicy", "seafood", "dessert"];
    let t = Instant::now();
    let mut pairs = Vec::with_capacity(n);
    for i in 0..n {
        // spread over Java/Bali-ish bbox
        let lon = 106.0 + (i % 1000) as f64 * 0.01;
        let lat = -6.0 - (i % 800) as f64 * 0.01;
        let content = format!("{} {} great place", words[i % words.len()], words[(i / 8) % words.len()]);
        pairs.push((format!("venues/v{i}"), format!(
            r#"{{"_collection":"venues","_key":"v{i}","category":"{}","price":{},"content":"{content}","geometry":{{"type":"Point","coordinates":[{lon},{lat}]}}}}"#,
            cats[i % cats.len()], i % 500)));
    }
    db.put_many(pairs.iter().map(|(s, p)| (s.as_str(), p.as_str()))).unwrap();
    // vectors on all (8-dim toy embedding)
    for i in 0..n {
        let base = (i % 8) as f32;
        let v: Vec<f32> = (0..8).map(|d| base + d as f32 * 0.1).collect();
        db.put_vector(&format!("venues/v{i}"), "emb", &v).unwrap();
    }
    println!("ingest {n} venues in {:?}", t.elapsed());

    // Build the four facet indexes.
    let t = Instant::now();
    db.build_field_index("venues", "category");
    db.build_bm25_index("content");
    db.build_spatial_index();
    db.build_hnsw_index("emb", 16, 200);
    println!("build 4 indexes in {:?}", t.elapsed());
    db.compact().unwrap();

    println!("RSS after build+compact: {:.1} MB", rss_mb());

    // The canonical hybrid query: category (scalar) ∩ near (spatial) ∩ coffee (text)
    // ranked by vector similarity — ONE statement, single pass.
    let q = "SELECT _key FROM venues \
             WHERE category = 'cafe' \
               AND ST_DWithin(geometry, POINT(106.5 -6.4), 30000) \
               AND BM25(content, 'coffee') > 0.0 \
             ORDER BY VECTOR_COSINE(emb, [0.0,0.1,0.2,0.3,0.4,0.5,0.6,0.7]) DESC \
             LIMIT 10";
    // warm + best-of-5
    for _ in 0..2 { let _ = db.query(q).map(|s| s.collect()); }
    let mut best = std::time::Duration::from_secs(999);
    let mut rows = 0;
    for _ in 0..5 {
        let t = Instant::now();
        rows = db.query(q).map(|s| s.collect().len()).unwrap_or(0);
        best = best.min(t.elapsed());
    }
    println!("HYBRID (scalar∩spatial∩text∩vector, 1 pass): {:?}  rows={rows}", best);
    println!("RSS after query: {:.1} MB", rss_mb());
    let _ = std::fs::remove_dir_all(&dir);
}
