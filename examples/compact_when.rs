//! When does compaction actually fire, and what does each one cost?
use sekejap::{CoreDB, SyncMode};
use serde_json::json;
use std::time::Instant;

fn main() {
    let base_n: usize = 1_000_000;
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.set_wal_sync(SyncMode::Off);
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER)").unwrap();
        for i in 0..base_n {
            db.put(&format!("p/n{i}"), &json!({"_collection":"p","_key":format!("n{i}"),"n":i as i64}).to_string()).unwrap();
        }
        db.compact().unwrap();
    }

    let mut db = CoreDB::open_paged(dir.path()).unwrap();
    db.set_wal_sync(SyncMode::Off);
    println!("base = {base_n} rows. writing continuously, auto-compaction on.\n");
    println!("  {:>10}  {:>12}  {:>10}  {:>12}", "write #", "this write", "compactions", "wal.log");

    // A compaction is unmistakable from the write it lands on, so detect it by
    // latency rather than calling stats() on the hot path.
    let start = base_n;
    let mut hits = 0;
    let mut last = 0usize;
    for i in 0..450_000usize {
        let k = start + i;
        let t = Instant::now();
        db.put(&format!("p/n{k}"), &json!({"_collection":"p","_key":format!("n{k}"),"n":k as i64}).to_string()).unwrap();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        if ms > 200.0 {
            hits += 1;
            let wal = std::fs::metadata(dir.path().join("wal.log")).map(|m| m.len()).unwrap_or(0);
            println!("  {:>10}  {:>10.0}ms  {:>10}  {:>10.1}MB   <-- COMPACTION (+{} writes)",
                     i + 1, ms, hits, wal as f64 / 1_048_576.0, i + 1 - last);
            last = i + 1;
        }
    }
    println!("\n  450,000 writes on a {base_n}-row store → {hits} compactions");
    println!("  overlay threshold 200,000 · wal threshold 64 MB");
}
