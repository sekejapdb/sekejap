use sekejap::{Config, CoreDB, SyncMode};
use serde_json::json;
fn row(i: usize) -> String {
    json!({"_collection":"p","_key":format!("n{i}"),"n":i as i64,
           "body":format!("the quick brown fox number {i} leaps the lazy riverbank")}).to_string()
}
fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(200_000);
    for (pay, adj) in [(false, false), (false, true), (true, false), (true, true)] {
        let dir = tempfile::TempDir::new().unwrap();
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            db.set_wal_sync(SyncMode::Off);
            db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER, body TEXT)").unwrap();
            for i in 0..n { db.put(&format!("p/n{i}"), &row(i)).unwrap(); }
            for i in 0..n - 1 { db.link(&format!("p/n{i}"), &format!("p/n{}", i + 1), "next"); }
            db.compact().unwrap();
        }
        let mut db = CoreDB::open_with_config(dir.path(), Config {
            paged_topology: true, paged_adjacency: adj, paged_payloads: pay,
            ..Config::default() }).unwrap();
        db.set_wal_sync(SyncMode::Off);
        for i in n..n + 200 { db.put(&format!("p/n{i}"), &row(i)).unwrap(); }
        eprintln!("\n  === {n} rows, paged payloads = {pay}, paged adjacency = {adj} ===");
        let t = std::time::Instant::now();
        db.compact().unwrap();
        eprintln!("    {:>8.1} ms  TOTAL", t.elapsed().as_secs_f64() * 1e3);
    }
}
