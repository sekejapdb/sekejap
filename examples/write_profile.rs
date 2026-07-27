//! Breakdown of the atomic bulk-write cost (single-threaded, no lock noise) for
//! an IoT-shaped numeric row. Tells us how much of put_value_bulk is JSON
//! serialize vs storage/WAL/maps — i.e. whether the atom is serialize-bound.
//!   cargo run --release --example write_profile

use sekejap::{Config, CoreDB};
use serde_json::json;
use std::time::Instant;

fn row(i: usize) -> serde_json::Value {
    json!({"_collection":"sensors","_key":format!("k{i}"),"ts":1_700_000_000u64 + i as u64,"val": i as f64 * 0.5})
}

fn per(label: &str, n: usize, secs: f64) {
    println!("  {label:<26} {:>10.0} rows/s   {:>7.3} µs/row", n as f64 / secs, secs * 1e6 / n as f64);
}

fn main() {
    let n = 500_000;
    println!("== atom write cost breakdown ({n} IoT numeric rows, single thread) ==\n");

    // 1. Build the Values (json! macro + format! for the key).
    let t = Instant::now();
    let vals: Vec<serde_json::Value> = (0..n).map(row).collect();
    per("build Values", n, t.elapsed().as_secs_f64());

    // 2. Serialize each Value to a JSON string (the cost inside put_value_bulk).
    let t = Instant::now();
    let mut sink = 0usize;
    for v in &vals { sink += serde_json::to_string(v).unwrap().len(); }
    let ser = t.elapsed().as_secs_f64();
    per("serialize-only (to_string)", n, ser);
    std::hint::black_box(sink);

    // 3. Full put_value_bulk on a pre-built (slug, Value) Vec (one batch, one fsync).
    let dir = tempfile::tempdir().unwrap();
    let mut db = CoreDB::open_with_config(dir.path(), Config { payload_binary: true, ..Config::default() }).unwrap();
    let rows: Vec<(String, serde_json::Value)> = vals.into_iter().enumerate()
        .map(|(i, v)| (format!("sensors/k{i}"), v)).collect();
    let t = Instant::now();
    let written = db.put_value_bulk(rows).unwrap();
    let bulk = t.elapsed().as_secs_f64();
    per("put_value_bulk (full)", n, bulk);
    assert_eq!(written, n);

    println!("\n  → storage+WAL+maps ≈ {:.3} µs/row (bulk − serialize)", (bulk - ser) * 1e6 / n as f64);
    println!("  → serialize is {:.0}% of the bulk write cost", ser / bulk * 100.0);
}
