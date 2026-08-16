//! What does loading a province-scale dataset actually cost, end to end?
use sekejap::{CoreDB, SyncMode};
use serde_json::json;
use std::time::Instant;

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(2_000_000);
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.set_wal_sync(SyncMode::Off);
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, name TEXT, n INTEGER)").unwrap();

    // Bulk mode: auto-compaction is skipped while defer_wal_sync is set, so the
    // load runs without the intermediate compactions a plain insert loop triggers.
    let t = Instant::now();
    db.begin_bulk();
    let mut batch: Vec<(String, serde_json::Value)> = Vec::with_capacity(10_000);
    for i in 0..n {
        batch.push((format!("p/n{i}"), json!({
            "_collection":"p","_key":format!("n{i}"),
            "name": format!("record {i} west java"), "n": i as i64
        })));
        if batch.len() == 10_000 {
            db.put_value_bulk(std::mem::take(&mut batch)).unwrap();
            batch = Vec::with_capacity(10_000);
        }
    }
    if !batch.is_empty() { db.put_value_bulk(batch).unwrap(); }
    db.end_bulk();
    let load = t.elapsed().as_secs_f64();

    let t = Instant::now();
    db.compact().unwrap();
    let compact = t.elapsed().as_secs_f64();

    let bytes: u64 = std::fs::read_dir(dir.path()).unwrap()
        .filter_map(|e| e.ok()).filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum();
    let rows = db.node_count();
    println!("  {n} rows: load {load:.1}s + final compact {compact:.1}s = {:.1}s total",
             load + compact);
    println!("  store {:.0} MB, {rows} rows intact, {:.0} rows/sec sustained",
             bytes as f64 / 1_048_576.0, n as f64 / (load + compact));
    println!("  extrapolated to 48M: {:.1} min", (load + compact) * 48.0 / (n as f64 / 1e6) / 60.0);
}
