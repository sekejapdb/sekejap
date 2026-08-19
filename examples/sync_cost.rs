//! Why is one insert ~6 ms when a query is microseconds?
//!
//! The suspicion is that almost all of it is one fsync per row, and that the
//! work sekejap does underneath is small. The way to know is to keep everything
//! identical and change only when the data is forced to the platter.

use sekejap::{AutoCompact, Config, CoreDB, SyncMode};
use std::time::Instant;

fn one_by_one(name: &str, n: usize, sync: SyncMode) -> f64 {
    let dir = std::env::temp_dir().join(format!("sk_sync_{}_{}", std::process::id(), name));
    let _ = std::fs::remove_dir_all(&dir);
    let cfg = Config { wal_sync: sync, auto_compact: AutoCompact::Off, ..Config::default() };
    let mut db = CoreDB::open_with_config(&dir, cfg).unwrap();

    let t = Instant::now();
    for i in 0..n {
        db.put_value(&format!("items/n{i}"),
            serde_json::json!({"_collection":"items","_key":format!("n{i}"),"n":i})).unwrap();
    }
    let us = t.elapsed().as_secs_f64() * 1_000_000.0 / n as f64;
    println!("{:<38} {:>10.1} us/insert", name, us);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    us
}

fn main() {
    // Small n: this is a per-row constant, not a slope, so it shows up immediately.
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(3_000);
    println!("{} rows per mode\n", n);

    let full   = one_by_one("1. fsync every row (the default)", n, SyncMode::Full);
    let normal = one_by_one("2. no per-row fsync",              n, SyncMode::Normal);
    let off    = one_by_one("3. no fsync at all",               n, SyncMode::Off);

    println!();
    println!("fsync accounts for {:.1}% of an insert  ({:.1} us of {:.1} us)",
             (full - normal) / full * 100.0, full - normal, full);
    println!("sekejap's own work per insert is about {:.1} us", off);
    println!();
    println!("1 billion rows, one at a time:");
    println!("  with per-row fsync : {:>8.1} days", full   * 1e9 / 1e6 / 86_400.0);
    println!("  without            : {:>8.1} days", normal * 1e9 / 1e6 / 86_400.0);
}
