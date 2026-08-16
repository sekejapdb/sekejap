//! What does an un-compacted WAL actually cost? Not queries — startup.
use sekejap::{CoreDB, SyncMode};
use serde_json::json;
use std::time::Instant;

fn row(i: usize) -> String {
    json!({"_collection":"p","_key":format!("n{i}"),"n":i as i64}).to_string()
}

fn main() {
    let base = 200_000usize;
    for pending in [0usize, 20_000, 100_000, 200_000] {
        let dir = tempfile::TempDir::new().unwrap();
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            db.set_wal_sync(SyncMode::Off);
            db.set_auto_compact(sekejap::AutoCompact::Off);
            db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER)").unwrap();
            for i in 0..base { db.put(&format!("p/n{i}"), &row(i)).unwrap(); }
            db.compact().unwrap();                 // WAL now empty
            for i in base..base + pending { db.put(&format!("p/n{i}"), &row(i)).unwrap(); }
        }                                          // dropped WITHOUT compacting
        let wal = std::fs::metadata(dir.path().join("wal.log")).map(|m| m.len()).unwrap_or(0);

        let t = Instant::now();
        let db = CoreDB::open_paged(dir.path()).unwrap();
        let open_ms = t.elapsed().as_secs_f64() * 1000.0;

        // A record written since the compaction, and one from the base.
        let t = Instant::now();
        for i in 0..5000 { std::hint::black_box(db.get(&format!("p/n{}", base + (i % pending.max(1))))); }
        let recent_us = t.elapsed().as_secs_f64() * 1_000_000.0 / 5000.0;
        let t = Instant::now();
        for i in 0..5000 { std::hint::black_box(db.get(&format!("p/n{}", i % base))); }
        let base_us = t.elapsed().as_secs_f64() * 1_000_000.0 / 5000.0;

        println!("  pending {pending:>7}  wal {:>7.1}MB  open {open_ms:>8.1}ms   \
                  read recent {recent_us:>5.2}us   read base {base_us:>5.2}us",
                 wal as f64 / 1_048_576.0);
    }
    println!("\n  base = {base} compacted rows; 'pending' = written after, never compacted");
}
