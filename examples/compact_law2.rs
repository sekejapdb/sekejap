//! Law 2, measured: the same amount of change, applied to stores of very
//! different sizes. If compaction cost tracks the *change* it stays flat. If it
//! tracks the *store* it climbs, and a database that is fine at a million rows
//! is unusable at a billion.
//!
//! Setup runs with fsync off and in bulk — building the base is not what is
//! being timed, and at 6 ms a row it would otherwise take hours. The compaction
//! being measured is unaffected by either choice.

use sekejap::{AutoCompact, CompactThresholds, Config, CoreDB, SyncMode};
use std::time::Instant;

/// Insert `n` rows starting at `from`, in chunks, so the row vector never gets
/// large enough to be its own memory story.
fn load(db: &mut CoreDB, from: usize, n: usize) {
    const CHUNK: usize = 25_000;
    let mut done = 0;
    while done < n {
        let take = CHUNK.min(n - done);
        let rows: Vec<(String, serde_json::Value)> = (from + done..from + done + take)
            .map(|i| {
                (format!("items/n{i}"),
                 serde_json::json!({"_collection":"items","_key":format!("n{i}"),"n":i}))
            })
            .collect();
        db.put_value_bulk(rows).unwrap();
        done += take;
    }
}

fn rss_mb() -> u64 {
    let pid = std::process::id();
    std::process::Command::new("ps").args(["-o", "rss=", "-p", &pid.to_string()])
        .output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|kb| kb / 1024).unwrap_or(0)
}

fn main() {
    // The constant. Every store below gets exactly this many new rows before the
    // compaction that is timed.
    const CHANGE: usize = 50_000;

    println!("same change ({} rows) applied to stores of increasing size\n", CHANGE);
    println!("{:>12} {:>14} {:>12} {:>10}", "base_rows", "compact_ms", "vs_smallest", "rss_mb");
    println!("{}", "-".repeat(52));

    let mut first_ms: Option<f64> = None;

    for &base in &[100_000usize, 250_000, 500_000, 1_000_000] {
        let dir = std::env::temp_dir().join(format!("sk_law2_{}_{}", std::process::id(), base));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = Config {
            wal_sync: SyncMode::Off,
            auto_compact: AutoCompact::Off,
            compact_thresholds: CompactThresholds { wal_bytes: u64::MAX, overlay_entries: usize::MAX },
            ..Config::default()
        };
        let mut db = CoreDB::open_with_config(&dir, cfg).unwrap();

        // Build the base and fold it to disk, so the timed compaction below sees
        // a large store with a small overlay — the situation that matters.
        load(&mut db, 0, base);
        db.compact().unwrap();

        // The change. Identical at every size.
        load(&mut db, base, CHANGE);

        let t = Instant::now();
        db.compact().unwrap();
        let ms = t.elapsed().as_secs_f64() * 1000.0;

        let ratio = match first_ms {
            None => { first_ms = Some(ms); 1.0 }
            Some(f) => ms / f,
        };
        println!("{:>12} {:>14.1} {:>11.2}x {:>10}", base, ms, ratio, rss_mb());

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    println!();
    println!("flat  -> compaction is proportional to the change. Law 2 holds.");
    println!("rising-> compaction is proportional to the store. Law 2 violated.");
    println!("(10x the base rows with ~1x the time is the result you want.)");
}
