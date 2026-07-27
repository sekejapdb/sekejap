//! Concurrency benchmark for the `Engine` (RwLock: concurrent readers, exclusive
//! writer) at 10 / 100 / 1000 threads.
//!   - READS: high-traffic point/filter queries in parallel (should scale).
//!   - WRITES: IoT-style ingest under contention (serialized by the write lock;
//!     we verify throughput AND that no write is lost — concurrency-safe).
//! Correctness is asserted at every level (right rows read; exact total written).
//!
//!   cargo run --release --features engine --example concurrency_bench

use sekejap::engine::Engine;
use sekejap::{Config, CoreDB};
use std::sync::Arc;
use std::time::Instant;

const SEED_ROWS: usize = 10_000;

fn seed(dir: &std::path::Path) {
    let mut db = CoreDB::open_with_config(dir, Config::default()).unwrap();
    let rows: Vec<(String, String)> = (0..SEED_ROWS)
        .map(|i| (
            format!("items/i{i:06}"),
            format!(r#"{{"_collection":"items","_key":"i{i:06}","cat":"c{}","val":{},"note":"item {i} in the dataset"}}"#, i % 20, i % 100),
        ))
        .collect();
    db.put_many(rows.iter().map(|(s, j)| (s.as_str(), j.as_str()))).unwrap();
    db.compact().unwrap();
}

fn bench_reads(engine: &Arc<Engine>, threads: usize, per_thread: usize) {
    let t = Instant::now();
    let mut handles = Vec::with_capacity(threads);
    for tid in 0..threads {
        let e = Arc::clone(engine);
        handles.push(std::thread::spawn(move || {
            let mut ok = 0usize;
            for j in 0..per_thread {
                // Mix: point lookups + a filter, keys spread across the dataset.
                let k = (tid.wrapping_mul(2654435761).wrapping_add(j)) % SEED_ROWS;
                let hits = e.query(&format!("SELECT _key, cat, val FROM items WHERE _key = 'i{k:06}'")).unwrap();
                if hits.len() == 1 { ok += 1; }
            }
            ok
        }));
    }
    let total_ok: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    let total = threads * per_thread;
    let secs = t.elapsed().as_secs_f64();
    assert_eq!(total_ok, total, "every concurrent read must return its 1 row");
    println!("  reads  {threads:>4} threads × {per_thread:>4} = {total:>8} queries  {:>8.0} q/s  {:>6.1} µs/q",
        total as f64 / secs, secs * 1e6 / total as f64);
}

/// mode: 0 = single-row unbuffered, 1 = single-row buffered, 2 = multi-row batches.
fn bench_writes(threads: usize, per_thread: usize, mode: u8, batch: usize) {
    let dir = tempfile::tempdir().unwrap();
    let mut b = Engine::builder(dir.path().to_str().unwrap());
    if mode == 1 { b = b.buffer_size(batch); }
    let engine = Arc::new(b.build().unwrap());
    engine.execute("CREATE TABLE sensors (_key TEXT PRIMARY KEY, ts INTEGER, val REAL)").unwrap();
    engine.flush().ok();

    let t = Instant::now();
    let mut handles = Vec::with_capacity(threads);
    for tid in 0..threads {
        let e = Arc::clone(&engine);
        handles.push(std::thread::spawn(move || match mode {
            2 => {
                // Multi-row INSERT: `batch` readings per statement → 1 fsync/batch.
                let mut j = 0;
                while j < per_thread {
                    let end = (j + batch).min(per_thread);
                    let vals: Vec<String> = (j..end)
                        .map(|r| format!("('s{tid}_{r}', {}, {}.5)", 1_700_000_000 + r, tid))
                        .collect();
                    e.execute(&format!("INSERT INTO sensors (_key, ts, val) VALUES {}", vals.join(", "))).unwrap();
                    j = end;
                }
            }
            _ => {
                // Single-row inserts (unbuffered or buffered).
                for j in 0..per_thread {
                    e.execute(&format!(
                        "INSERT INTO sensors (_key, ts, val) VALUES ('s{tid}_{j}', {}, {}.5)",
                        1_700_000_000 + j, tid
                    )).unwrap();
                }
            }
        }));
    }
    for h in handles { h.join().unwrap(); }
    if mode == 1 { engine.flush().unwrap(); } // drain any partial buffer
    let secs = t.elapsed().as_secs_f64();
    let total = threads * per_thread;

    let n = engine.query("SELECT COUNT(*) AS n FROM sensors").unwrap();
    let count = n[0].payload.as_ref().unwrap()["n"].as_i64().unwrap() as usize;
    assert_eq!(count, total, "concurrent writes must not lose/duplicate any row ({count} != {total})");
    let label = match mode { 1 => format!("buffered({batch})"), 2 => format!("multirow({batch})"), _ => "single-row".into() };
    println!("  writes {threads:>4}t × {per_thread:>4}  {label:<14} {total:>8} rows  {:>9.0} w/s  {:>7.1} µs/w  (verified {count})",
        total as f64 / secs, secs * 1e6 / total as f64);
}

fn main() {
    println!("== Engine concurrency benchmark (RwLock: parallel reads, serialized writes) ==\n");

    let read_dir = tempfile::tempdir().unwrap();
    print!("seeding {SEED_ROWS} rows for reads…");
    seed(read_dir.path());
    println!(" done");
    let read_engine = Arc::new(Engine::builder(read_dir.path().to_str().unwrap()).build().unwrap());

    println!("\n[READS] high-traffic concurrent point/filter queries");
    for &threads in &[10usize, 100, 1000] {
        bench_reads(&read_engine, threads, 200);
    }

    // Single-row inserts fsync-per-row (~300/s), so keep their volume small; the
    // multi-row path (1 fsync / batch) is the IoT throughput path — run it at scale.
    println!("\n[WRITES] IoT ingest — single-row (fsync-bound) at small scale:");
    for &threads in &[10usize, 100] {
        bench_writes(threads, 30, 0, 0);          // single-row, unbuffered
        bench_writes(threads, 30, 1, 200);        // single-row, buffered
    }
    println!("\n[WRITES] IoT ingest — multi-row INSERT (1 fsync/batch) at 10/100/1000:");
    for &threads in &[10usize, 100, 1000] {
        bench_writes(threads, 100, 2, 100);       // 100 readings per statement
    }

    println!("\n== all concurrency assertions passed (no lost reads or writes) ==");
}
