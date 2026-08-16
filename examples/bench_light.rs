//! Light benchmark — the numbers the contract is judged on.
//!
//! Run before and after any change to the storage or write path:
//!
//!     cargo run --release --example bench_light
//!
//! It exists to answer one question: **does cost stay proportional to change
//! rather than to size?** Every row is measured at two database sizes, and what
//! matters is not the absolute figure but whether the figure *grows with the
//! store*. A row that scales with size is a violation of Law 2 regardless of how
//! fast it looks today. See .workbench/CONTRACT.md.
//!
//! Deliberately small enough to run often. It is not a competitive benchmark.

use sekejap::{CoreDB, SyncMode};
use serde_json::json;
use std::time::Instant;

#[cfg(target_os = "macos")]
fn rss_mb() -> f64 {
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map(|kb| kb / 1024.0)
        .unwrap_or(0.0)
}
#[cfg(not(target_os = "macos"))]
fn rss_mb() -> f64 { 0.0 }

fn ms(t: Instant) -> f64 { t.elapsed().as_secs_f64() * 1000.0 }

fn row(i: usize) -> String {
    json!({"_collection": "p", "_key": format!("n{i}"), "n": i as i64,
           "body": format!("the quick brown fox number {i} leaps the lazy riverbank")}).to_string()
}

/// Build a database of `n` rows and `n-1` edges, compacted into the base.
fn build(dir: &std::path::Path, n: usize, indexed: bool) {
    let mut db = CoreDB::open(dir).unwrap();
    db.set_wal_sync(SyncMode::Off);              // measuring structure, not fsync
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER, body TEXT)").unwrap();
    for i in 0..n { db.put(&format!("p/n{i}"), &row(i)).unwrap(); }
    for i in 0..n - 1 { db.link(&format!("p/n{i}"), &format!("p/n{}", i + 1), "next"); }
    if indexed {
        db.execute("CREATE INDEX ON p USING btree (n)").unwrap();
        db.execute("CREATE INDEX ON p USING bm25 (body)").unwrap();
    }
    db.compact().unwrap();
}

struct Row { label: &'static str, small: String, large: String, scales: bool }

fn main() {
    const SMALL: usize = 20_000;
    const LARGE: usize = 200_000;
    let mut out: Vec<Row> = Vec::new();

    let mut measure = |label: &'static str, f: &dyn Fn(usize) -> f64, unit: &str| {
        let s = f(SMALL);
        let l = f(LARGE);
        // "Scales" = grew by more than 3x for a 10x bigger database. A change-
        // proportional operation should be flat; some growth is noise and cache.
        let scales = l > s * 3.0 && l > 1.0;
        out.push(Row {
            label,
            small: format!("{s:.2}{unit}"),
            large: format!("{l:.2}{unit}"),
            scales,
        });
    };

    // ── writes on top of a compacted base ────────────────────────────────────
    measure("insert (no index)", &|n| {
        let dir = tempfile::TempDir::new().unwrap();
        build(dir.path(), n, false);
        let mut db = CoreDB::open_paged(dir.path()).unwrap();
        db.set_wal_sync(SyncMode::Off);
        let t = Instant::now();
        for i in n..n + 200 { db.put(&format!("p/n{i}"), &row(i)).unwrap(); }
        ms(t) / 200.0
    }, "ms");

    measure("insert (btree + bm25)", &|n| {
        let dir = tempfile::TempDir::new().unwrap();
        build(dir.path(), n, true);
        let mut db = CoreDB::open_paged(dir.path()).unwrap();
        db.set_wal_sync(SyncMode::Off);
        let t = Instant::now();
        for i in n..n + 200 { db.put(&format!("p/n{i}"), &row(i)).unwrap(); }
        ms(t) / 200.0
    }, "ms");

    measure("delete", &|n| {
        let dir = tempfile::TempDir::new().unwrap();
        build(dir.path(), n, true);
        let mut db = CoreDB::open_paged(dir.path()).unwrap();
        db.set_wal_sync(SyncMode::Off);
        let t = Instant::now();
        for i in 0..200 { db.remove(&format!("p/n{i}")); }
        ms(t) / 200.0
    }, "ms");

    // ── reads ────────────────────────────────────────────────────────────────
    measure("point read", &|n| {
        let dir = tempfile::TempDir::new().unwrap();
        build(dir.path(), n, false);
        let db = CoreDB::open_paged(dir.path()).unwrap();
        let t = Instant::now();
        for i in 0..2000 { std::hint::black_box(db.get(&format!("p/n{}", i % n))); }
        ms(t) * 1000.0 / 2000.0
    }, "us");

    measure("1-hop traversal", &|n| {
        let dir = tempfile::TempDir::new().unwrap();
        build(dir.path(), n, false);
        let db = CoreDB::open_paged(dir.path()).unwrap();
        let t = Instant::now();
        for i in 0..2000 {
            std::hint::black_box(db.one(&format!("p/n{}", i % (n - 1))).forward("next").collect().len());
        }
        ms(t) * 1000.0 / 2000.0
    }, "us");

    measure("open_paged", &|n| {
        let dir = tempfile::TempDir::new().unwrap();
        build(dir.path(), n, false);
        let t = Instant::now();
        std::hint::black_box(CoreDB::open_paged(dir.path()).unwrap());
        ms(t)
    }, "ms");

    // ── the operation the contract exists because of ─────────────────────────
    measure("compact after 200 writes", &|n| {
        let dir = tempfile::TempDir::new().unwrap();
        build(dir.path(), n, false);
        let mut db = CoreDB::open_paged(dir.path()).unwrap();
        db.set_wal_sync(SyncMode::Off);
        for i in n..n + 200 { db.put(&format!("p/n{i}"), &row(i)).unwrap(); }
        let t = Instant::now();
        db.compact().unwrap();
        ms(t)
    }, "ms");

    // NOTE: peak RSS during compaction is deliberately NOT measured here. Within a
    // single process the build phase has already set the high-water mark, so the
    // delta reads as ~0 and would falsely report a violation as clean. Measure it
    // per-process instead:
    //
    //     cargo run --release --example compact_ram 1000000
    //
    // A misleading green is worse than a missing row.

    // ── report ───────────────────────────────────────────────────────────────
    println!("\n  light benchmark — {SMALL} vs {LARGE} rows (10x)\n");
    println!("  {:<28}{:>12}{:>12}   {}", "", "20k", "200k", "verdict");
    println!("  {}", "-".repeat(70));
    let mut violations = 0;
    for r in &out {
        let verdict = if r.scales { violations += 1; "SCALES WITH SIZE  <-- Law 2" } else { "flat" };
        println!("  {:<28}{:>12}{:>12}   {verdict}", r.label, r.small, r.large);
    }
    println!();
    if violations == 0 {
        println!("  every measured operation is flat across a 10x database.");
    } else {
        println!("  {violations} operation(s) scale with database size — see .workbench/CONTRACT.md Law 2.");
    }
}
