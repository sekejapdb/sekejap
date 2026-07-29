//! RAM residual / leak probe. Loads a FIXED dataset, then runs repeated identical
//! query + full-scan bursts and samples process RSS between them.
//!
//! Reading it: the working-set RAM (holding the data) is expected to stay — that's
//! not a leak. The tell for a real leak is RSS that KEEPS GROWING across identical
//! cycles. Flat across cycles = healthy (even if freed memory isn't returned to the
//! OS immediately — allocators retain it for reuse). We also idle between cycles to
//! see how much transient query memory is released.
//!
//!   cargo bench --bench ram_residual

use sekejap::{Config, CoreDB};
use serde_json::json;
use std::time::Duration;

const N: usize = 100_000;

/// Resident set size (MB) of THIS process, via `ps` (macOS/Linux rss is KB).
fn rss_mb() -> f64 {
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &pid]).output();
    out.ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map(|kb| kb / 1024.0)
        .unwrap_or(0.0)
}

fn main() {
    println!("== RAM residual probe ({N} rows, disk-backed SKBIN) ==\n");
    let dir = tempfile::tempdir().unwrap();
    let mut db = CoreDB::open_with_config(dir.path(), Config { payload_binary: true, ..Config::default() }).unwrap();
    let base_empty = rss_mb();
    println!("  baseline (empty db open):        {base_empty:>8.1} MB");

    // Load a fixed dataset (the working set) and compact to SKBIN on disk.
    db.put_value_bulk((0..N).map(|i| (
        format!("s/k{i:06}"),
        json!({"_collection":"s","_key":format!("k{i:06}"),"v":i,"note":format!("row {i} of the fixed dataset")}),
    )).collect()).unwrap();
    db.compact().unwrap();
    let loaded = rss_mb();
    println!("  after load + compact (working):  {loaded:>8.1} MB   (+{:.1})\n", loaded - base_empty);

    // Phase A — repeated point-lookup bursts (tiny per-query alloc).
    println!("  [point lookups] 200k queries/cycle — RSS should stay flat (no leak):");
    for c in 1..=6 {
        for j in 0..200_000usize {
            let k = (j.wrapping_mul(2654435761)) % N;
            let _ = db.query(&format!("SELECT _key, v FROM s WHERE _key = 'k{k:06}'")).unwrap().collect();
        }
        let after = rss_mb();
        std::thread::sleep(Duration::from_millis(400));
        let idle = rss_mb();
        println!("    cycle {c}: after burst {after:>8.1} MB | after idle {idle:>8.1} MB");
    }

    // Phase B — repeated FULL scans (each materializes all N rows, then frees).
    println!("\n  [full scans] 20 × SELECT * per cycle — big transient alloc that must free:");
    for c in 1..=6 {
        for _ in 0..20 {
            let hits: Vec<_> = db.query("SELECT * FROM s").unwrap().collect();
            std::hint::black_box(hits.len());
        }
        let after = rss_mb();
        std::thread::sleep(Duration::from_millis(400));
        let idle = rss_mb();
        println!("    cycle {c}: after burst {after:>8.1} MB | after idle {idle:>8.1} MB");
    }

    // Phase C — repeated open/close lifecycles on the SAME db dir (lifecycle leak?).
    println!("\n  [open/close] reopen the DB 20× per cycle — RSS should stay flat:");
    drop(db);
    for c in 1..=5 {
        for _ in 0..20 {
            let d = CoreDB::open_with_config(dir.path(), Config { payload_binary: true, ..Config::default() }).unwrap();
            let _ = d.query("SELECT COUNT(*) AS n FROM s").unwrap().collect();
            drop(d);
        }
        let after = rss_mb();
        println!("    cycle {c}: {after:>8.1} MB");
    }

    let end = rss_mb();
    println!("\n  final: {end:.1} MB  (working set ~{:.1} MB above empty baseline)", end - base_empty);
    println!("  verdict: leak IFF RSS climbs monotonically across a phase's cycles;");
    println!("           a stable plateau (even above the load baseline) = no leak.");
}
