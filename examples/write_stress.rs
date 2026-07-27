//! Heavy write-load stress test. 10,000 *real* OS threads exceeds this machine's
//! thread limit (ulimit -u ≈ 2666), so we simulate 10,000+ concurrent clients the
//! way a real server does: a BOUNDED worker pool work-stealing over a large total
//! of writes. Measures sustained durable throughput and — the point — verifies
//! ZERO lost/duplicated rows under saturation, on sekejap vs SQLite.
//!
//!   cargo run --release --features engine --example write_stress

use rusqlite::Connection;
use sekejap::engine::Engine;
use sekejap::Config;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

const WORKERS: usize = 200;      // bounded pool (well under the OS thread cap)
const TOTAL: usize = 500_000;    // total durable writes (≈ 10k clients' worth)
const BATCH: usize = 1000;        // rows per durable commit

fn sekejap_stress() -> (f64, usize) {
    let dir = tempfile::tempdir().unwrap();
    // Group-commit: buffer single-row inserts; the shared buffer coalesces
    // concurrent writers into few fsyncs (drain empties everything pending).
    let engine = Arc::new(Engine::builder(dir.path().to_str().unwrap()).buffer_size(BATCH).build().unwrap());
    engine.execute("CREATE TABLE sensors (_key TEXT PRIMARY KEY, ts INTEGER, val REAL)").unwrap();
    engine.flush().unwrap(); // ensure the table exists before workers insert
    let cursor = Arc::new(AtomicUsize::new(0));
    let t = Instant::now();
    let hs: Vec<_> = (0..WORKERS).map(|_| {
        let (e, c) = (Arc::clone(&engine), Arc::clone(&cursor));
        std::thread::spawn(move || loop {
            let i = c.fetch_add(1, Ordering::Relaxed);
            if i >= TOTAL { break; }
            // One IoT reading per call → buffered → group-committed.
            e.execute(&format!("INSERT INTO sensors (_key, ts, val) VALUES ('k{i}', {}, {i}.5)", 1_700_000_000usize + i)).unwrap();
        })
    }).collect();
    for h in hs { h.join().unwrap(); }
    engine.flush().unwrap(); // drain any partial buffer
    let secs = t.elapsed().as_secs_f64();
    let n = engine.query("SELECT COUNT(*) AS n FROM sensors").unwrap()[0].payload.as_ref().unwrap()["n"].as_i64().unwrap() as usize;
    (TOTAL as f64 / secs, n)
}

fn sekejap_prepared_stress() -> (f64, usize) {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(Engine::builder(dir.path().to_str().unwrap()).buffer_size(BATCH).build().unwrap());
    engine.execute("CREATE TABLE sensors (_key TEXT PRIMARY KEY, ts INTEGER, val REAL)").unwrap();
    engine.flush().unwrap();
    let stmt = Arc::new(engine.prepare_insert("sensors", &["_key", "ts", "val"]).unwrap());
    let cursor = Arc::new(AtomicUsize::new(0));
    let t = Instant::now();
    let hs: Vec<_> = (0..WORKERS).map(|_| {
        let (e, s, c) = (Arc::clone(&engine), Arc::clone(&stmt), Arc::clone(&cursor));
        std::thread::spawn(move || loop {
            let i = c.fetch_add(1, Ordering::Relaxed);
            if i >= TOTAL { break; }
            // Bound params only — no SQL parsed per row.
            e.insert_prepared(&s, &[json!(format!("k{i}")), json!(1_700_000_000usize + i), json!(i as f64 + 0.5)]).unwrap();
        })
    }).collect();
    for h in hs { h.join().unwrap(); }
    engine.flush().unwrap();
    let secs = t.elapsed().as_secs_f64();
    let n = engine.query("SELECT COUNT(*) AS n FROM sensors").unwrap()[0].payload.as_ref().unwrap()["n"].as_i64().unwrap() as usize;
    (TOTAL as f64 / secs, n)
}

/// SQL multi-row INSERT (the SQL bulk idiom) — now routes through put_value_bulk.
fn sekejap_sql_multirow_stress() -> (f64, usize) {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(Engine::builder(dir.path().to_str().unwrap()).build().unwrap());
    engine.execute("CREATE TABLE sensors (_key TEXT PRIMARY KEY, ts INTEGER, val REAL)").unwrap();
    let cursor = Arc::new(AtomicUsize::new(0));
    let t = Instant::now();
    let hs: Vec<_> = (0..WORKERS).map(|_| {
        let (e, c) = (Arc::clone(&engine), Arc::clone(&cursor));
        std::thread::spawn(move || loop {
            let start = c.fetch_add(BATCH, Ordering::Relaxed);
            if start >= TOTAL { break; }
            let end = (start + BATCH).min(TOTAL);
            let vals: Vec<String> = (start..end)
                .map(|i| format!("('k{i}', {}, {i}.5)", 1_700_000_000usize + i)).collect();
            e.execute(&format!("INSERT INTO sensors (_key, ts, val) VALUES {}", vals.join(", "))).unwrap();
        })
    }).collect();
    for h in hs { h.join().unwrap(); }
    let secs = t.elapsed().as_secs_f64();
    let n = engine.query("SELECT COUNT(*) AS n FROM sensors").unwrap()[0].payload.as_ref().unwrap()["n"].as_i64().unwrap() as usize;
    (TOTAL as f64 / secs, n)
}

fn sqlite_stress() -> (f64, usize) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite");
    {
        let c = Connection::open(&path).unwrap();
        c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE sensors (key TEXT PRIMARY KEY, ts INTEGER, val REAL);").unwrap();
    }
    let cursor = Arc::new(AtomicUsize::new(0));
    let t = Instant::now();
    let hs: Vec<_> = (0..WORKERS).map(|_| {
        let (p, c) = (path.clone(), Arc::clone(&cursor));
        std::thread::spawn(move || {
            let conn = Connection::open(&p).unwrap();
            conn.busy_timeout(std::time::Duration::from_secs(120)).unwrap();
            conn.execute_batch("PRAGMA synchronous=FULL;").unwrap();
            loop {
                let start = c.fetch_add(BATCH, Ordering::Relaxed);
                if start >= TOTAL { break; }
                let end = (start + BATCH).min(TOTAL);
                conn.execute_batch("BEGIN IMMEDIATE").unwrap();
                {
                    let mut stmt = conn.prepare("INSERT INTO sensors (key, ts, val) VALUES (?1, ?2, ?3)").unwrap();
                    for i in start..end { stmt.execute(rusqlite::params![format!("k{i}"), 1_700_000_000i64 + i as i64, i as f64 + 0.5]).unwrap(); }
                }
                conn.execute_batch("COMMIT").unwrap();
            }
        })
    }).collect();
    for h in hs { h.join().unwrap(); }
    let secs = t.elapsed().as_secs_f64();
    let conn = Connection::open(&path).unwrap();
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM sensors", [], |r| r.get(0)).unwrap();
    (TOTAL as f64 / secs, n as usize)
}

fn main() {
    let _ = Config::default();
    println!("== heavy write stress: {WORKERS} workers × {TOTAL} total writes (batch {BATCH}) ==");
    println!("   (10k real OS threads exceed ulimit -u ≈ 2666; a bounded pool is the safe model)\n");

    let (sk_wps, sk_n) = sekejap_stress();
    assert_eq!(sk_n, TOTAL, "sekejap lost/duplicated rows! ({sk_n} != {TOTAL})");
    println!("  sekejap (SQL single buffered): {sk_wps:>9.0} w/s   verified {sk_n} rows (0 lost)");

    let (skm_wps, skm_n) = sekejap_sql_multirow_stress();
    assert_eq!(skm_n, TOTAL, "sekejap-sql-multirow lost/duplicated! ({skm_n} != {TOTAL})");
    println!("  sekejap (SQL multi-row)      : {skm_wps:>9.0} w/s   verified {skm_n} rows (0 lost)");

    let (skp_wps, skp_n) = sekejap_prepared_stress();
    assert_eq!(skp_n, TOTAL, "sekejap-prepared lost/duplicated rows! ({skp_n} != {TOTAL})");
    println!("  sekejap (PREPARED typed)    : {skp_wps:>9.0} w/s   verified {skp_n} rows (0 lost)");

    let (sq_wps, sq_n) = sqlite_stress();
    assert_eq!(sq_n, TOTAL, "sqlite lost/duplicated rows! ({sq_n} != {TOTAL})");
    println!("  sqlite (prepared)      : {sq_wps:>9.0} w/s   verified {sq_n} rows (0 lost)");

    println!("\n== both survived {TOTAL} writes under {WORKERS}-way contention with 0 data loss ==");
}
