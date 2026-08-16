// Fast iteration harness for MATCH SHORTEST perf. Reproduces the mega bench's scale
// (20k venues + 255 services + ~60k edges) so cache pressure matches, but builds the
// dataset ONCE (persisted) and reopens+times — each optimization iteration is ~1s.
//
//   cargo run --release --example shortest_bench           build-if-needed + time
//   cargo run --release --example shortest_bench rebuild   force rebuild
use std::time::{Duration, Instant};
use sekejap::CoreDB;

fn main() {
    let dir = std::env::temp_dir().join("sk-spbench");
    let rebuild = std::env::args().nth(1).as_deref() == Some("rebuild");
    if rebuild || !dir.join("nodes.bin").exists() {
        let _ = std::fs::remove_dir_all(&dir);
        let mut db = CoreDB::open(&dir).unwrap();
        let mut pairs = Vec::with_capacity(20_255);
        for i in 0..20_000u32 {
            pairs.push((format!("venues/v{i}"),
                format!(r#"{{"_collection":"venues","_key":"v{i}","category":"cafe","price":{}}}"#, i % 500)));
        }
        for i in 0..255u32 {
            pairs.push((format!("services/svc{i}"),
                format!(r#"{{"_collection":"services","_key":"svc{i}","status":"ok"}}"#)));
        }
        db.put_many(pairs.iter().map(|(s, p)| (s.as_str(), p.as_str()))).unwrap();
        // services: binary dependency tree (child -> parent).
        for i in 1..255u32 {
            db.link(&format!("services/svc{i}"), &format!("services/svc{}", (i - 1) / 2), "depends_on");
        }
        // venues: ~3 related_to each → ~60k edges (matches mega scale / edge-store cache).
        for i in 0..20_000u32 {
            for j in 1..=3u32 {
                db.link(&format!("venues/v{i}"), &format!("venues/v{}", (i + j * 7919) % 20_000), "related_to");
            }
        }
        db.compact().unwrap();
        println!("built {} nodes", db.node_count());
    }

    let db = CoreDB::open(&dir).unwrap();
    let sql = "SELECT a._key AS start, b._key AS end, length(r) AS hops \
               FROM MATCH SHORTEST (a:services)-[r*]->(b:services) \
               WHERE a._key = 'svc200' AND b._key = 'svc0'";
    let prep = db.prepare(sql).unwrap();
    for _ in 0..2000 { db.query_prepared(&prep, &[]).unwrap().count(); }
    let mut best = Duration::from_secs(999);
    let mut n = 0;
    for _ in 0..100_000 {
        let t = Instant::now();
        n = db.query_prepared(&prep, &[]).unwrap().count();
        best = best.min(t.elapsed());
    }
    println!("prepared shortest svc200->svc0: {:?}  (rows {})   [sqlite ref: 5.6µs]", best, n);
}
