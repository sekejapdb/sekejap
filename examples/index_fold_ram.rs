//! What does folding the field index into a new sidecar cost in RAM?
//!
//! Reads and writes against an indexed column are bounded now: the heap side is
//! a delta and reads merge it with the mapping. Compaction is not. It asks for
//! the merged view as one `BTreeMap<FieldKey, Vec<u64>>` so it can hand it to
//! `fieldstore::write`, which means the whole column lands on the heap at fold
//! time.
//!
//! Law 1 has no exemption for maintenance — "the occasion is exactly when the
//! database is largest". So this samples resident memory *during* the compaction
//! rather than around it, and reports the peak against the row count. Flat means
//! the fold is bounded. Rising means the column is still being materialised.

use sekejap::{AutoCompact, Config, CoreDB, SyncMode};
use std::sync::{atomic::{AtomicBool, AtomicU64, Ordering}, Arc};

fn rss_mb() -> u64 {
    let pid = std::process::id();
    std::process::Command::new("ps").args(["-o", "rss=", "-p", &pid.to_string()])
        .output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|kb| kb / 1024).unwrap_or(0)
}

fn load(db: &mut CoreDB, from: usize, n: usize, distinct: usize) {
    const CHUNK: usize = 25_000;
    let mut done = 0;
    while done < n {
        let take = CHUNK.min(n - done);
        let rows: Vec<(String, serde_json::Value)> = (from + done..from + done + take)
            .map(|i| (format!("items/n{i}"),
                 serde_json::json!({"_collection":"items","_key":format!("n{i}"),
                                    "n": i % distinct})))
            .collect();
        db.put_value_bulk(rows).unwrap();
        done += take;
    }
}

fn main() {
    // A column with many distinct values is the hard case: the directory grows
    // with the keys, not just the postings.
    let distinct: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(1_000_000);

    println!("{:>12} {:>14} {:>14} {:>12}", "rows", "rss_before_MB", "peak_MB", "fold_cost_MB");
    println!("{}", "-".repeat(56));

    for &n in &[100_000usize, 250_000, 500_000, 1_000_000] {
        let dir = std::env::temp_dir().join(format!("sk_foldram_{}_{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = Config { wal_sync: SyncMode::Off, auto_compact: AutoCompact::Off, ..Config::default() };
        let mut db = CoreDB::open_with_config(&dir, cfg).unwrap();

        db.execute("CREATE TABLE items (n INTEGER)").unwrap();
        load(&mut db, 0, n, distinct);
        // With `noidx` the same rows are folded with no btree index at all, which
        // is how much of the peak belongs to compaction generally rather than to
        // the index writer being measured.
        if std::env::var("NOIDX").is_err() {
            db.execute("CREATE INDEX ON items USING btree (n)").unwrap();
        }
        db.compact().unwrap();          // first fold: builds the base

        // A small delta, so the fold below is triggered by a tiny change against
        // a large store — the exact shape Law 2 is about.
        load(&mut db, n, 1_000, distinct);

        let before = rss_mb();
        let peak = Arc::new(AtomicU64::new(before));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (peak, stop) = (Arc::clone(&peak), Arc::clone(&stop));
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let r = rss_mb();
                    peak.fetch_max(r, Ordering::Relaxed);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            });
        }

        db.compact().unwrap();

        stop.store(true, Ordering::Relaxed);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let p = peak.load(Ordering::Relaxed);
        println!("{:>12} {:>14} {:>14} {:>12}", n, before, p, p.saturating_sub(before));

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    println!();
    println!("fold_cost flat  -> the fold is bounded by the change. Law 1 holds.");
    println!("fold_cost rising-> the column is still materialised to write it.");
}
