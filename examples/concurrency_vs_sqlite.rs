//! Concurrency head-to-head: sekejap `Engine` (one instance shared via Arc,
//! RwLock — parallel readers, serialized writer) vs SQLite (connection-per-thread,
//! WAL mode — SQLite's standard concurrency model). Matched durability: SQLite
//! runs `synchronous = FULL` (fsync per commit), like sekejap's fsync-per-write.
//!   READS  — concurrent point lookups at 10 / 100 / 1000 threads.
//!   WRITES — durable batched ingest (sekejap multi-row INSERT vs SQLite txn batch).
//! Correctness asserted on both sides (right rows read; exact totals written).
//!
//!   cargo run --release --features engine --example concurrency_vs_sqlite

use rusqlite::Connection;
use sekejap::engine::Engine;
use sekejap::{Config, CoreDB};
use std::sync::Arc;
use std::time::Instant;

const SEED: usize = 10_000;

// ── sekejap setup ───────────────────────────────────────────────────────────
fn sk_seed(dir: &std::path::Path) {
    let mut db = CoreDB::open_with_config(dir, Config::default()).unwrap();
    let rows: Vec<(String, String)> = (0..SEED)
        .map(|i| (format!("items/i{i:06}"),
            format!(r#"{{"_collection":"items","_key":"i{i:06}","cat":"c{}","val":{}}}"#, i % 20, i % 100)))
        .collect();
    db.put_many(rows.iter().map(|(s, j)| (s.as_str(), j.as_str()))).unwrap();
    db.compact().unwrap();
}

fn sk_reads(engine: &Arc<Engine>, threads: usize, per: usize) -> f64 {
    let t = Instant::now();
    let hs: Vec<_> = (0..threads).map(|tid| {
        let e = Arc::clone(engine);
        std::thread::spawn(move || {
            let mut ok = 0;
            for j in 0..per {
                let k = (tid.wrapping_mul(2654435761).wrapping_add(j)) % SEED;
                if e.query(&format!("SELECT _key, cat, val FROM items WHERE _key = 'i{k:06}'")).unwrap().len() == 1 { ok += 1; }
            }
            ok
        })
    }).collect();
    let ok: usize = hs.into_iter().map(|h| h.join().unwrap()).sum();
    assert_eq!(ok, threads * per, "sekejap: every read must return its row");
    (threads * per) as f64 / t.elapsed().as_secs_f64()
}

fn sk_writes(threads: usize, per: usize, batch: usize) -> f64 {
    let dir = tempfile::tempdir().unwrap();
    // Group-commit path: buffer single-row inserts; the shared buffer coalesces
    // concurrent writers' commits into few fsyncs (1 durable batch ≈ `batch` rows).
    let engine = Arc::new(Engine::builder(dir.path().to_str().unwrap()).buffer_size(batch).build().unwrap());
    engine.execute("CREATE TABLE sensors (_key TEXT PRIMARY KEY, ts INTEGER, val REAL)").unwrap();
    engine.flush().unwrap();
    let t = Instant::now();
    let hs: Vec<_> = (0..threads).map(|tid| {
        let e = Arc::clone(&engine);
        std::thread::spawn(move || {
            for r in 0..per {
                e.execute(&format!("INSERT INTO sensors (_key, ts, val) VALUES ('s{tid}_{r}', {}, {}.5)", 1_700_000_000 + r, tid)).unwrap();
            }
        })
    }).collect();
    for h in hs { h.join().unwrap(); }
    engine.flush().unwrap();
    let secs = t.elapsed().as_secs_f64();
    let count = engine.query("SELECT COUNT(*) AS n FROM sensors").unwrap()[0].payload.as_ref().unwrap()["n"].as_i64().unwrap() as usize;
    assert_eq!(count, threads * per, "sekejap: no lost writes");
    (threads * per) as f64 / secs
}

// ── SQLite setup (WAL + synchronous=FULL for durability parity) ──────────────
fn sq_open(path: &std::path::Path) -> Connection {
    let c = Connection::open(path).unwrap();
    c.busy_timeout(std::time::Duration::from_secs(60)).unwrap();
    c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;").unwrap();
    c
}

fn sq_seed(path: &std::path::Path) {
    let c = sq_open(path);
    c.execute_batch("CREATE TABLE items (key TEXT PRIMARY KEY, cat TEXT, val INTEGER);").unwrap();
    c.execute_batch("BEGIN").unwrap();
    let mut stmt = c.prepare("INSERT INTO items (key, cat, val) VALUES (?1, ?2, ?3)").unwrap();
    for i in 0..SEED {
        stmt.execute(rusqlite::params![format!("i{i:06}"), format!("c{}", i % 20), (i % 100) as i64]).unwrap();
    }
    drop(stmt);
    c.execute_batch("COMMIT").unwrap();
}

fn sq_reads(path: &std::path::Path, threads: usize, per: usize) -> f64 {
    let t = Instant::now();
    let hs: Vec<_> = (0..threads).map(|tid| {
        let p = path.to_path_buf();
        std::thread::spawn(move || {
            let c = Connection::open(&p).unwrap(); // own connection per thread
            let mut stmt = c.prepare("SELECT key, cat, val FROM items WHERE key = ?1").unwrap();
            let mut ok = 0;
            for j in 0..per {
                let k = (tid.wrapping_mul(2654435761).wrapping_add(j)) % SEED;
                let n = stmt.query_map(rusqlite::params![format!("i{k:06}")], |_| Ok(())).unwrap().count();
                if n == 1 { ok += 1; }
            }
            ok
        })
    }).collect();
    let ok: usize = hs.into_iter().map(|h| h.join().unwrap()).sum();
    assert_eq!(ok, threads * per, "sqlite: every read must return its row");
    (threads * per) as f64 / t.elapsed().as_secs_f64()
}

fn sq_writes(threads: usize, per: usize, batch: usize) -> f64 {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite");
    { let c = sq_open(&path); c.execute_batch("CREATE TABLE sensors (key TEXT PRIMARY KEY, ts INTEGER, val REAL);").unwrap(); }
    let t = Instant::now();
    let hs: Vec<_> = (0..threads).map(|tid| {
        let p = path.clone();
        std::thread::spawn(move || {
            let c = sq_open(&p);
            let mut j = 0;
            while j < per {
                let end = (j + batch).min(per);
                c.execute_batch("BEGIN IMMEDIATE").unwrap();
                {
                    let mut stmt = c.prepare("INSERT INTO sensors (key, ts, val) VALUES (?1, ?2, ?3)").unwrap();
                    for r in j..end { stmt.execute(rusqlite::params![format!("s{tid}_{r}"), 1_700_000_000i64 + r as i64, tid as f64 + 0.5]).unwrap(); }
                }
                c.execute_batch("COMMIT").unwrap();
                j = end;
            }
        })
    }).collect();
    for h in hs { h.join().unwrap(); }
    let secs = t.elapsed().as_secs_f64();
    let c = sq_open(&path);
    let count: i64 = c.query_row("SELECT COUNT(*) FROM sensors", [], |r| r.get(0)).unwrap();
    assert_eq!(count as usize, threads * per, "sqlite: no lost writes");
    (threads * per) as f64 / secs
}

fn main() {
    println!("== concurrency: sekejap Engine vs SQLite (WAL, synchronous=FULL) ==");
    println!("seed = {SEED} rows; durability matched (both fsync per durable commit)\n");

    let sk_dir = tempfile::tempdir().unwrap();
    sk_seed(sk_dir.path());
    let sk_engine = Arc::new(Engine::builder(sk_dir.path().to_str().unwrap()).build().unwrap());
    let sq_dir = tempfile::tempdir().unwrap();
    let sq_path = sq_dir.path().join("db.sqlite");
    sq_seed(&sq_path);

    println!("[READS] concurrent point lookups (queries/sec)");
    println!("  {:>8}  {:>14}  {:>14}", "threads", "sekejap", "sqlite");
    for &t in &[10usize, 100, 1000] {
        let (sk, sq) = (sk_reads(&sk_engine, t, 200), sq_reads(&sq_path, t, 200));
        println!("  {t:>8}  {sk:>12.0} q/s  {sq:>12.0} q/s", );
    }

    println!("\n[WRITES] durable batched ingest, batch=100 (writes/sec)");
    println!("  {:>8}  {:>14}  {:>14}", "threads", "sekejap", "sqlite");
    for &t in &[10usize, 100, 1000] {
        let (sk, sq) = (sk_writes(t, 100, 100), sq_writes(t, 100, 100));
        println!("  {t:>8}  {sk:>12.0} w/s  {sq:>12.0} w/s");
    }

    println!("\n== done: results verified on both engines (no lost reads or writes) ==");
}
