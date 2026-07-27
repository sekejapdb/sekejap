//! Write throughput vs thread count — is the concurrent write gap per-row cost or
//! contention? Sweeps threads for sekejap (prepared, buffered group-commit) vs
//! SQLite (WAL, synchronous=FULL), work-stealing over a fixed total.
//!   cargo run --release --features engine --example write_scaling

use rusqlite::Connection;
use sekejap::engine::Engine;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

const TOTAL: usize = 200_000;
const BATCH: usize = 1000;

fn sekejap(threads: usize) -> (f64, usize) {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(Engine::builder(dir.path().to_str().unwrap()).buffer_size(BATCH).build().unwrap());
    engine.execute("CREATE TABLE s (_key TEXT PRIMARY KEY, ts INTEGER, val REAL)").unwrap();
    engine.flush().unwrap();
    let stmt = Arc::new(engine.prepare_insert("s", &["_key", "ts", "val"]).unwrap());
    let cur = Arc::new(AtomicUsize::new(0));
    let t = Instant::now();
    let hs: Vec<_> = (0..threads).map(|_| {
        let (e, s, c) = (Arc::clone(&engine), Arc::clone(&stmt), Arc::clone(&cur));
        std::thread::spawn(move || loop {
            let i = c.fetch_add(1, Ordering::Relaxed);
            if i >= TOTAL { break; }
            e.insert_prepared(&s, &[json!(format!("k{i}")), json!(1_700_000_000usize + i), json!(i as f64 * 0.5)]).unwrap();
        })
    }).collect();
    for h in hs { h.join().unwrap(); }
    engine.flush().unwrap();
    let secs = t.elapsed().as_secs_f64();
    let n = engine.query("SELECT COUNT(*) AS n FROM s").unwrap()[0].payload.as_ref().unwrap()["n"].as_i64().unwrap() as usize;
    (TOTAL as f64 / secs, n)
}

fn sqlite(threads: usize) -> (f64, usize) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite");
    { let c = Connection::open(&path).unwrap(); c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE s (key TEXT PRIMARY KEY, ts INTEGER, val REAL);").unwrap(); }
    let cur = Arc::new(AtomicUsize::new(0));
    let t = Instant::now();
    let hs: Vec<_> = (0..threads).map(|_| {
        let (p, c) = (path.clone(), Arc::clone(&cur));
        std::thread::spawn(move || {
            let conn = Connection::open(&p).unwrap();
            conn.busy_timeout(std::time::Duration::from_secs(120)).unwrap();
            conn.execute_batch("PRAGMA synchronous=FULL;").unwrap();
            loop {
                let start = c.fetch_add(BATCH, Ordering::Relaxed);
                if start >= TOTAL { break; }
                let end = (start + BATCH).min(TOTAL);
                conn.execute_batch("BEGIN IMMEDIATE").unwrap();
                { let mut st = conn.prepare("INSERT INTO s (key,ts,val) VALUES (?1,?2,?3)").unwrap();
                  for i in start..end { st.execute(rusqlite::params![format!("k{i}"), 1_700_000_000i64 + i as i64, i as f64 * 0.5]).unwrap(); } }
                conn.execute_batch("COMMIT").unwrap();
            }
        })
    }).collect();
    for h in hs { h.join().unwrap(); }
    let secs = t.elapsed().as_secs_f64();
    let n: i64 = Connection::open(&path).unwrap().query_row("SELECT COUNT(*) FROM s", [], |r| r.get(0)).unwrap();
    (TOTAL as f64 / secs, n as usize)
}

fn main() {
    println!("== write throughput vs thread count ({TOTAL} rows, batch {BATCH}) ==\n");
    println!("  {:>7}  {:>16}  {:>16}", "threads", "sekejap (w/s)", "sqlite (w/s)");
    for &t in &[1usize, 2, 4, 8, 16, 32, 64, 128, 200] {
        let (sk, skn) = sekejap(t);
        let (sq, sqn) = sqlite(t);
        assert_eq!(skn, TOTAL); assert_eq!(sqn, TOTAL);
        println!("  {t:>7}  {sk:>14.0}    {sq:>14.0}");
    }
}
