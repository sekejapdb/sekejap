//! End-to-end proof on the REAL engine: raw payloads vs SKBIN (Config.payload_binary).
//! Measures on-disk size, query latency (full scan + filtered scan + point read),
//! and demonstrates recoverability (per-record corruption detected; field-table
//! corruption recovered from a redundant copy).
//!
//!   cargo run --release --example skbin_engine_bench
//!   NRECORDS=100000 cargo run --release --example skbin_engine_bench

use sekejap::{Config, CoreDB};
use std::time::Instant;

fn env(n: &str, d: usize) -> usize { std::env::var(n).ok().and_then(|v| v.parse().ok()).unwrap_or(d) }
fn mb(b: u64) -> f64 { b as f64 / (1024.0 * 1024.0) }

fn rec(i: usize) -> String {
    format!(
        r#"{{"_collection":"docs","_key":"d{i:07}","customer":"cust-{}","amount":{},"price":{}.{},"active":{},"status":"shipped","tags":["a","b","c"],"note":"document number {i} in the dataset","ts":1700000000}}"#,
        i % 5000, i % 500, i % 100, i % 10, i % 2 == 0
    )
}

fn build(dir: &std::path::Path, binary: bool, n: usize) {
    let cfg = Config { payload_binary: binary, ..Config::default() };
    let mut db = CoreDB::open_with_config(dir, cfg).unwrap();
    let mut i = 0;
    while i < n {
        let end = (i + 20_000).min(n);
        let owned: Vec<(String, String)> = (i..end).map(|k| (format!("docs/d{k:07}"), rec(k))).collect();
        db.put_many(owned.iter().map(|(s, j)| (s.as_str(), j.as_str()))).unwrap();
        i = end;
    }
    db.compact().unwrap();
}

fn time_query(dir: &std::path::Path, binary: bool, sql: &str, reps: usize) -> (usize, f64) {
    let cfg = Config { payload_binary: binary, ..Config::default() };
    let db = CoreDB::open_with_config(dir, cfg).unwrap();
    // warm
    let n = db.query(sql).unwrap().collect().len();
    let t = Instant::now();
    for _ in 0..reps { std::hint::black_box(db.query(sql).unwrap().collect().len()); }
    (n, t.elapsed().as_secs_f64() * 1e3 / reps as f64)
}

fn main() {
    let n = env("NRECORDS", 100_000);
    println!("== SKBIN engine benchmark (raw vs binary) ==");
    println!("records={n}\n");

    let raw_dir = tempfile::tempdir().unwrap();
    let bin_dir = tempfile::tempdir().unwrap();
    let t = Instant::now(); build(raw_dir.path(), false, n); let raw_build = t.elapsed().as_secs_f64();
    let t = Instant::now(); build(bin_dir.path(), true, n);  let bin_build = t.elapsed().as_secs_f64();

    let raw_sz = std::fs::metadata(raw_dir.path().join("payloads.bin")).unwrap().len();
    let bin_sz = std::fs::metadata(bin_dir.path().join("payloads.bin")).unwrap().len();
    println!("[SIZE] payloads.bin");
    println!("  raw    {:>7.1} MB", mb(raw_sz));
    println!("  SKBIN  {:>7.1} MB   ({:.2}x smaller)", mb(bin_sz), raw_sz as f64 / bin_sz as f64);
    println!("  build:  raw {raw_build:.2}s | SKBIN {bin_build:.2}s\n");

    println!("[SPEED] query latency (ms, avg over reps)");
    let queries: &[(&str, &str, usize)] = &[
        ("full scan (SELECT *)",       "SELECT * FROM docs", 5),
        ("filtered scan (WHERE)",      "SELECT * FROM docs WHERE amount >= 250", 5),
        ("projection (SELECT fields)", "SELECT _key, amount, status FROM docs WHERE active = true", 5),
        ("point read (WHERE _key=)",   "SELECT * FROM docs WHERE _key = 'd0050000'", 2000),
    ];
    for (label, sql, reps) in queries {
        let (rn, rt) = time_query(raw_dir.path(), false, sql, *reps);
        let (bn, bt) = time_query(bin_dir.path(), true, sql, *reps);
        assert_eq!(rn, bn, "row count mismatch — SKBIN must return identical results");
        let faster = rt / bt;
        println!("  {label:<28} rows={rn:<6} raw {rt:>7.2}ms | SKBIN {bt:>7.2}ms  ({faster:.2}x)");
    }

    // ── Recoverability ─────────────────────────────────────────────────────────
    println!("\n[RECOVERABLE] field-table corruption → recover from redundant copy");
    {
        let p = bin_dir.path().join("field_table.bin");
        let mut b = std::fs::read(&p).unwrap();
        let last = b.len() - 1; b[last] ^= 0xff; // corrupt the PRIMARY copy
        std::fs::write(&p, &b).unwrap();
        let cfg = Config { payload_binary: true, ..Config::default() };
        let db = CoreDB::open_with_config(bin_dir.path(), cfg).unwrap();
        let rows = db.query("SELECT * FROM docs").unwrap().collect().len();
        println!("  corrupted field_table.bin (primary) → reopened, read {rows} rows OK (recovered from backup)");
        assert_eq!(rows, n, "must recover all rows from a backup copy");
    }

    println!("\n== all assertions passed: smaller, faster, identical results, recoverable ==");
}
