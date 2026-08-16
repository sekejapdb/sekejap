//! How does compaction cost scale with store size? Measured, to extrapolate from.
use sekejap::{CoreDB, SyncMode};
use serde_json::json;
use std::time::Instant;

fn main() {
    for n in [500_000usize, 1_000_000, 2_000_000, 4_000_000] {
        let dir = tempfile::TempDir::new().unwrap();
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            db.set_wal_sync(SyncMode::Off);
            db.set_auto_compact(sekejap::AutoCompact::Off);
            db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER)").unwrap();
            for i in 0..n {
                db.put(&format!("p/n{i}"), &json!({"_collection":"p","_key":format!("n{i}"),"n":i as i64}).to_string()).unwrap();
            }
            db.compact().unwrap();
        }
        let mut db = CoreDB::open_paged(dir.path()).unwrap();
        db.set_wal_sync(SyncMode::Off);
        db.set_auto_compact(sekejap::AutoCompact::Off);
        // the 200,000 writes that trigger a real compaction
        for i in n..n + 200_000 {
            db.put(&format!("p/n{i}"), &json!({"_collection":"p","_key":format!("n{i}"),"n":i as i64}).to_string()).unwrap();
        }
        let t = Instant::now();
        db.compact().unwrap();
        let secs = t.elapsed().as_secs_f64();
        let bytes: u64 = std::fs::read_dir(dir.path()).unwrap()
            .filter_map(|e| e.ok()).filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum();
        println!("  {:>10} rows  store {:>7.0} MB   compaction {:>7.1}s",
                 n, bytes as f64 / 1_048_576.0, secs);
    }
}
