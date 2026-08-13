//! # Concurrency benchmark — do reads freeze behind writes?
//!
//! The mega benchmark measures ONE query at a time. This one measures what a busy
//! website actually does: **many readers at once, while a writer runs.** It's the
//! scoreboard for the snapshot-reads feature (see
//! `docs/developer/notes/snapshot-reads-design.md`) — that feature's whole point
//! is "reads never block writers", and only a concurrent benchmark can show it.
//!
//! Model: today apps share one engine as `Arc<RwLock<CoreDB>>` — many readers hold
//! the shared lock; a writer takes it exclusively (so readers queue behind it).
//! We run `READERS` threads doing point reads for a fixed window under three
//! conditions and report **reads/sec** (throughput) and **read latency p50/p99/max**:
//!
//!   1. reads only            — no writer (the ceiling).
//!   2. reads + writer        — one thread doing `put`s (brief exclusive locks).
//!   3. reads + compact       — one thread doing `compact()` (a LONG exclusive lock
//!                              — the "index rebuild froze the app" case).
//!
//! The gap between (1) and (2)/(3) is exactly the pain snapshot reads would remove.
//!
//! Run: `cargo bench --bench concurrency`

use sekejap::{Config, CoreDB, SyncMode};
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

const N_NODES: usize = 20_000;
const READERS: usize = 8;
const WINDOW_SECS: u64 = 3;
/// Record 1 latency sample every N reads (keeps memory bounded — point reads are
/// ~sub-µs, so a 3 s window does millions per thread).
const SAMPLE_EVERY: u64 = 64;

/// Build a disk-backed store with `N_NODES` venues and compact it once, then wrap
/// it in the `Arc<RwLock<CoreDB>>` that apps use to share one engine across threads.
fn setup() -> (Arc<RwLock<CoreDB>>, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().unwrap();
    // SyncMode::Normal: no per-write fsync during the bulk load (setup isn't measured).
    let mut db = CoreDB::open_with_config(
        dir.path(),
        Config { wal_sync: SyncMode::Normal, ..Default::default() },
    )
    .unwrap();
    for i in 0..N_NODES {
        let cat = ["cafe", "bar", "gym"][i % 3];
        db.put(
            &format!("venues/v{i}"),
            &json!({"_collection":"venues","_key":format!("v{i}"),"cat":cat,"n":i}).to_string(),
        )
        .unwrap();
    }
    db.compact().unwrap();
    (Arc::new(RwLock::new(db)), dir)
}

/// The p-th percentile of a sorted latency slice (ns).
fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

struct Stats {
    reads: u64,
    reads_per_sec: f64,
    p50_ns: u64,
    p99_ns: u64,
    max_ns: u64,
}

/// What the "disturber" thread does alongside the readers.
#[derive(Clone, Copy)]
enum Disturb {
    None,
    Writer,
    Compactor,
}

/// Run the reader pool for `WINDOW_SECS` while an optional disturber runs, and
/// aggregate throughput + latency.
fn run(db: &Arc<RwLock<CoreDB>>, disturb: Disturb) -> Stats {
    let stop = Arc::new(AtomicBool::new(false));
    let total_reads = Arc::new(AtomicU64::new(0));

    // Readers: each does point reads (`get`) — minimal per-op work, so the signal
    // is dominated by lock acquisition (exactly what contention hurts).
    let mut readers = Vec::new();
    for t in 0..READERS {
        let db = Arc::clone(db);
        let stop = Arc::clone(&stop);
        let total = Arc::clone(&total_reads);
        readers.push(thread::spawn(move || {
            let mut lats: Vec<u64> = Vec::with_capacity(1 << 16);
            let mut n: u64 = 0;
            // Deterministic key walk (no RNG needed): each thread strides differently.
            let mut k = t * 977;
            while !stop.load(Ordering::Relaxed) {
                k = (k + 1) % N_NODES;
                let slug = format!("venues/v{k}");
                let start = Instant::now();
                let guard = db.read().unwrap(); // shared lock — blocks while a writer holds it
                let _ = guard.get(&slug);
                drop(guard);
                if n % SAMPLE_EVERY == 0 {
                    lats.push(start.elapsed().as_nanos() as u64);
                }
                n += 1;
            }
            total.fetch_add(n, Ordering::Relaxed);
            lats
        }));
    }

    // Disturber: takes the exclusive lock repeatedly.
    let disturber = {
        let db = Arc::clone(db);
        let stop = Arc::clone(&stop);
        thread::spawn(move || match disturb {
            Disturb::None => {}
            Disturb::Writer => {
                let mut i = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    db.write().unwrap().put(
                        &format!("venues/w{i}"),
                        &json!({"_collection":"venues","_key":format!("w{i}"),"cat":"cafe","n":i}).to_string(),
                    ).unwrap();
                    i += 1;
                }
            }
            Disturb::Compactor => {
                while !stop.load(Ordering::Relaxed) {
                    // A long exclusive hold — rewrites snapshot + payloads.
                    db.write().unwrap().compact().unwrap();
                }
            }
        })
    };

    thread::sleep(Duration::from_secs(WINDOW_SECS));
    stop.store(true, Ordering::Relaxed);

    let mut all: Vec<u64> = Vec::new();
    for r in readers {
        all.extend(r.join().unwrap());
    }
    disturber.join().unwrap();
    all.sort_unstable();

    let reads = total_reads.load(Ordering::Relaxed);
    Stats {
        reads,
        reads_per_sec: reads as f64 / WINDOW_SECS as f64,
        p50_ns: pct(&all, 50.0),
        p99_ns: pct(&all, 99.0),
        max_ns: all.last().copied().unwrap_or(0),
    }
}

fn fmt_ns(ns: u64) -> String {
    if ns < 1_000 {
        format!("{ns}ns")
    } else if ns < 1_000_000 {
        format!("{:.1}µs", ns as f64 / 1e3)
    } else {
        format!("{:.2}ms", ns as f64 / 1e6)
    }
}

fn main() {
    println!("concurrency benchmark — {READERS} readers, {WINDOW_SECS}s window, {N_NODES} nodes\n");
    let (db, _dir) = setup();

    let conditions = [
        ("reads only (ceiling)", Disturb::None),
        ("reads + writer", Disturb::Writer),
        ("reads + compact", Disturb::Compactor),
    ];
    println!("| condition | reads/sec | p50 | p99 | max | vs ceiling |");
    println!("|---|---|---|---|---|---|");
    let mut ceiling = 0.0f64;
    for (label, d) in conditions {
        let s = run(&db, d);
        if matches!(d, Disturb::None) {
            ceiling = s.reads_per_sec;
        }
        let ratio = if ceiling > 0.0 {
            format!("{:.0}%", 100.0 * s.reads_per_sec / ceiling)
        } else {
            "—".into()
        };
        println!(
            "| {label} | {:.0} | {} | {} | {} | {ratio} |",
            s.reads_per_sec,
            fmt_ns(s.p50_ns),
            fmt_ns(s.p99_ns),
            fmt_ns(s.max_ns),
        );
        let _ = s.reads;
    }
    println!("\nBigger drop under writer/compact = more the app 'freezes' — the gap snapshot reads would close.");
}
