//! What does paged payloads buy on a real database, end to end?
use sekejap::{Config, CoreDB, SyncMode};
use serde_json::json;
use std::time::Instant;

fn row(i: usize) -> String {
    json!({"_collection":"p","_key":format!("n{i}"),"n":i as i64,
           "body":format!("the quick brown fox number {i} leaps the lazy riverbank")}).to_string()
}

fn run(n: usize, paged: bool) {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = Config { paged_payloads: paged, ..Default::default() };
    let mut db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
    db.set_wal_sync(SyncMode::Off);
    db.set_auto_compact(sekejap::AutoCompact::Off);
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER, body TEXT)").unwrap();

    let t = Instant::now();
    for i in 0..n { db.put(&format!("p/n{i}"), &row(i)).unwrap(); }
    let load = t.elapsed().as_secs_f64();
    db.compact().unwrap();

    // The workload that hurts: replace the whole store, then compact.
    let t = Instant::now();
    for i in 0..n {
        db.put(&format!("p/n{i}"), &row(i + 1_000_000)).unwrap();
    }
    let rewrite = t.elapsed().as_secs_f64();

    // Size BEFORE the final compaction: what the store costs while running, which
    // is the honest comparison. The flat store's small number afterwards is
    // achieved by the very rewrite this design exists to remove.
    let peak = std::fs::metadata(dir.path().join("payloads.bin")).map(|m| m.len()).unwrap_or(0);
    let t = Instant::now();
    db.compact().unwrap();
    let compact = t.elapsed().as_secs_f64();

    let bytes: u64 = std::fs::read_dir(dir.path()).unwrap()
        .filter_map(|e| e.ok()).filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum();
    let pay = std::fs::metadata(dir.path().join("payloads.bin")).map(|m| m.len()).unwrap_or(0);
    println!("  {:<7} {n:>7} rows: load {load:>5.1}s  overwrite {rewrite:>5.1}s  \
              compact {compact:>5.2}s   payloads before {:>5.0}MB after {:>5.0}MB",
             if paged { "paged" } else { "flat" },
             peak as f64 / 1_048_576.0, pay as f64 / 1_048_576.0);
    let _ = bytes;
}

fn main() {
    for n in [100_000usize, 400_000] {
        run(n, false);
        run(n, true);
        println!();
    }
}
