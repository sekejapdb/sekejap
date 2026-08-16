// Why is MATCH SHORTEST "slow" in the mega bench? Isolate SQL parse+lower cost from
// the actual BFS by comparing full-parse query() vs a prepared query on the same
// 255-node binary dependency tree (svc200 -> svc0, ~7 hops).
//
//   cargo run --release --example shortest_probe
use std::time::{Duration, Instant};
use sekejap::CoreDB;

fn best(iters: usize, mut f: impl FnMut() -> usize) -> (Duration, usize) {
    for _ in 0..50 { f(); }
    let mut b = Duration::from_secs(999);
    let mut n = 0;
    for _ in 0..iters { let t = Instant::now(); n = f(); b = b.min(t.elapsed()); }
    (b, n)
}

fn main() {
    let dir = std::env::temp_dir().join(format!("sk-sp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = CoreDB::open(&dir).unwrap();
    // 255-node binary tree; each node depends_on its parent (child -> parent edges),
    // so svc200 reaches the root svc0 in ~log2 hops.
    for i in 0..255u32 {
        db.put(&format!("services/svc{i}"), &format!(r#"{{"_collection":"services","_key":"svc{i}"}}"#)).unwrap();
    }
    for i in 1..255u32 {
        let parent = (i - 1) / 2;
        db.link(&format!("services/svc{i}"), &format!("services/svc{parent}"), "depends_on");
    }
    db.compact().unwrap();

    let sql = "SELECT a._key AS start, b._key AS end, length(r) AS hops \
               FROM MATCH SHORTEST (a:services)-[r*]->(b:services) \
               WHERE a._key = 'svc200' AND b._key = 'svc0'";

    // 1. Full parse every call (what the mega bench measures).
    let (t_query, n1) = best(20_000, || db.query(sql).unwrap().count());

    // 2. Prepared once, executed each call (fair vs sqlite prepare_cached).
    let prepared = db.prepare(sql).unwrap();
    let (t_prep, n2) = best(20_000, || db.query_prepared(&prepared, &[]).unwrap().count());

    println!("shortest path svc200 -> svc0 (255-node tree), rows found: {n1}/{n2}");
    println!("  full query() (re-parse each call) : {:?}", t_query);
    println!("  query_prepared() (parse once)     : {:?}", t_prep);
    let sp = t_query.as_secs_f64() / t_prep.as_secs_f64().max(1e-12);
    println!("  → parse+lower overhead is {:.0}% of query(); BFS itself ≈ prepared time", (1.0 - t_prep.as_secs_f64()/t_query.as_secs_f64())*100.0);
    println!("  → prepared is {:.1}x faster than re-parsing", sp);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);

    // Scale sweep: does the BFS stay flat as the graph grows? (early-termination BFS
    // visits ~path-length nodes regardless of graph size — the graph-first property.
    // SQLite's recursive CTE has no early exit: it walks the whole reachable set.)
    println!("\nshortest-path scale sweep (deep node -> root, prepared, best-of):");
    for depth in [8u32, 14, 17] {
        let n = (1u32 << (depth + 1)) - 1; // full binary tree, n nodes
        let dir = std::env::temp_dir().join(format!("sk-sp{depth}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut db = CoreDB::open(&dir).unwrap();
        let pairs: Vec<(String, String)> = (0..n).map(|i|
            (format!("services/svc{i}"), format!(r#"{{"_collection":"services","_key":"svc{i}"}}"#))).collect();
        db.put_many(pairs.iter().map(|(s, p)| (s.as_str(), p.as_str()))).unwrap();
        for i in 1..n {
            db.link(&format!("services/svc{i}"), &format!("services/svc{}", (i - 1) / 2), "depends_on");
        }
        db.compact().unwrap();
        let target = n - 1; // a deepest leaf → root svc0
        let sql = format!("SELECT length(r) AS hops FROM MATCH SHORTEST (a:services)-[r*]->(b:services) \
                           WHERE a._key = 'svc{target}' AND b._key = 'svc0'");
        let prep = db.prepare(&sql).unwrap();
        let (t, rows) = best(20_000, || db.query_prepared(&prep, &[]).unwrap().count());
        println!("  n={:>7}  path≈{} hops   sekejap BFS: {:?}   (rows {})", n, depth, t, rows);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
