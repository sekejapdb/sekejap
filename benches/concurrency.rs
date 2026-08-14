//! # Concurrency benchmark — do reads freeze behind writes?
//!
//! The mega benchmark measures ONE query at a time. This one measures what a busy
//! website actually does: **many readers at once, while a writer runs.** It's the
//! scoreboard for the snapshot-reads feature (see
//! `docs/developer/notes/snapshot-reads-design.md`) — that feature's whole point
//! is "reads never block writers", and only a concurrent benchmark can show it.
//!
//! ## Mode: paged
//!
//! Snapshots are a **paged-mode** feature (they share the immutable base), so this
//! benchmark runs the whole DB in paged mode (`open_paged`). Apps still share one
//! engine as `Arc<RwLock<CoreDB>>`; the question is what the *readers* do:
//!
//!   - **locked**    — the classic path: take the shared read lock, `get`, release.
//!                     Blocks whenever a writer holds the exclusive lock.
//!   - **published** — the engine pattern (Phase 2 `SharedDB`): a *publisher*
//!                     re-mints one snapshot periodically and stores it in a shared
//!                     slot; every reader grabs the latest by an `Arc` bump and
//!                     reads lock-free. Minting (which copies the overlay) happens
//!                     **once per publish**, shared by all readers — not once per
//!                     reader. Staleness is bounded by the publish interval.
//!   - **held**      — one snapshot minted for the whole window: the pure lock-free
//!                     read ceiling (maximally stale — an upper bound, not a mode
//!                     an app would use).
//!
//! Note: minting a snapshot copies the current write overlay, so its cost grows
//! with overlay size. This benchmark never compacts after the paged open, so the
//! overlay grows unbounded under the writers — a worst case for mint cost. The
//! *published* mode is what keeps that cost off the read path (one shared mint);
//! a naive "every reader re-mints" mode collapses here precisely because of it.
//!
//! ## Disturbers
//!
//!   - **writer**  — one thread doing single `put`s (brief exclusive locks).
//!   - **burst**   — one thread doing `BURST` puts per lock acquisition (a LONG
//!                   exclusive hold — the paged-mode stand-in for the "an index
//!                   rebuild / bulk import froze the app" case. `compact()` is a
//!                   resident-mode maintenance op and is not run live in paged mode.)
//!
//! The gap between the locked rows and the snapshot rows is exactly the pain
//! snapshot reads remove.
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
/// The publisher re-mints the shared snapshot roughly this often (staleness bound).
const PUBLISH_MS: u64 = 5;
/// Puts per lock acquisition for the burst-writer (a long exclusive hold).
const BURST: u64 = 4_000;

/// A slot holding the latest published snapshot. Readers `.read()` it (a tiny lock
/// guarding one `Arc` pointer — nothing to do with the DB lock) and `Arc`-clone the
/// snapshot out; the publisher `.write()`s a fresh one in. Models Phase 2's
/// `SharedDB`, where the writer republishes after each commit.
type SnapSlot = Arc<RwLock<Arc<CoreDB>>>;

/// Build a disk-backed store with `N_NODES` venues, compact it into an immutable
/// base, then **reopen in paged mode** and wrap it in the `Arc<RwLock<CoreDB>>`
/// that apps use to share one engine across threads. Paged mode is what makes the
/// store snapshottable.
fn setup() -> (Arc<RwLock<CoreDB>>, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().unwrap();
    {
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
        db.compact().unwrap(); // fold everything into the immutable base
    }
    let db = CoreDB::open_paged(dir.path()).unwrap();
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
    Burst,
}

/// How the reader threads read.
#[derive(Clone, Copy, PartialEq)]
enum Read {
    /// Take the shared read lock per `get` (blocks behind the writer).
    Locked,
    /// Read from a shared snapshot republished periodically by a publisher thread.
    Published,
    /// Read from a single snapshot minted once (pure lock-free ceiling).
    Held,
}

/// Run the reader pool for `WINDOW_SECS` while an optional disturber runs, and
/// aggregate throughput + latency.
fn run(db: &Arc<RwLock<CoreDB>>, disturb: Disturb, mode: Read) -> Stats {
    let stop = Arc::new(AtomicBool::new(false));
    let total_reads = Arc::new(AtomicU64::new(0));

    // For snapshot modes, mint an initial photo. Published mode shares it through a
    // slot a publisher thread keeps refreshing; Held mode reuses this one forever.
    let slot: Option<SnapSlot> = if mode == Read::Locked {
        None
    } else {
        let snap = db.read().unwrap().snapshot_db().expect("paged mode is snapshottable");
        Some(Arc::new(RwLock::new(snap)))
    };

    // Publisher: re-mint the shared snapshot every PUBLISH_MS (Published mode only).
    let publisher = if mode == Read::Published {
        let db = Arc::clone(db);
        let stop = Arc::clone(&stop);
        let slot = slot.clone().unwrap();
        Some(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Some(fresh) = db.read().unwrap().snapshot_db() {
                    *slot.write().unwrap() = fresh;
                }
                thread::sleep(Duration::from_millis(PUBLISH_MS));
            }
        }))
    } else {
        None
    };

    let mut readers = Vec::new();
    for t in 0..READERS {
        let db = Arc::clone(db);
        let stop = Arc::clone(&stop);
        let total = Arc::clone(&total_reads);
        let slot = slot.clone();
        readers.push(thread::spawn(move || {
            let mut lats: Vec<u64> = Vec::with_capacity(1 << 16);
            let mut n: u64 = 0;
            let mut k = t * 977; // deterministic per-thread key walk (no RNG)
            // Held mode: grab the one snapshot once and never touch the slot again.
            let held = if mode == Read::Held {
                Some(slot.as_ref().unwrap().read().unwrap().clone())
            } else {
                None
            };
            while !stop.load(Ordering::Relaxed) {
                k = (k + 1) % N_NODES;
                let slug = format!("venues/v{k}");
                let start = Instant::now();
                match mode {
                    Read::Locked => {
                        let guard = db.read().unwrap(); // shared lock — waits behind a writer
                        let _ = guard.get(&slug);
                    }
                    Read::Published => {
                        // Grab the latest published photo (Arc bump under a tiny slot
                        // lock), then read it lock-free.
                        let cur = slot.as_ref().unwrap().read().unwrap().clone();
                        let _ = cur.get(&slug);
                    }
                    Read::Held => {
                        let _ = held.as_ref().unwrap().get(&slug);
                    }
                }
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
            Disturb::Burst => {
                let mut i = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    // One long exclusive hold: BURST puts before releasing the lock.
                    let mut g = db.write().unwrap();
                    for _ in 0..BURST {
                        g.put(
                            &format!("venues/b{i}"),
                            &json!({"_collection":"venues","_key":format!("b{i}"),"cat":"bar","n":i}).to_string(),
                        ).unwrap();
                        i += 1;
                    }
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
    if let Some(p) = publisher {
        p.join().unwrap();
    }
    all.sort_unstable();

    let reads = total_reads.load(Ordering::Relaxed);
    Stats {
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
    println!(
        "concurrency benchmark (paged) — {READERS} readers, {WINDOW_SECS}s window, {N_NODES} nodes\n"
    );
    let (db, _dir) = setup();

    // (label, disturber, read-mode)
    let conditions = [
        ("reads only", "locked (ceiling)", Disturb::None, Read::Locked),
        ("reads + writer", "locked", Disturb::Writer, Read::Locked),
        ("reads + writer", "published", Disturb::Writer, Read::Published),
        ("reads + burst-writer", "locked", Disturb::Burst, Read::Locked),
        ("reads + burst-writer", "published", Disturb::Burst, Read::Published),
        ("reads + burst-writer", "held", Disturb::Burst, Read::Held),
    ];

    println!("| condition | reader | reads/sec | p50 | p99 | max | vs ceiling |");
    println!("|---|---|---|---|---|---|---|");
    let mut ceiling = 0.0f64;
    for (label, reader, d, m) in conditions {
        let s = run(&db, d, m);
        if matches!(d, Disturb::None) {
            ceiling = s.reads_per_sec;
        }
        let ratio = if ceiling > 0.0 {
            format!("{:.0}%", 100.0 * s.reads_per_sec / ceiling)
        } else {
            "—".into()
        };
        println!(
            "| {label} | {reader} | {:.0} | {} | {} | {} | {ratio} |",
            s.reads_per_sec,
            fmt_ns(s.p50_ns),
            fmt_ns(s.p99_ns),
            fmt_ns(s.max_ns),
        );
    }
    println!(
        "\nLocked reads collapse under a writer/burst (they queue behind the exclusive \
         lock). Published reads stay near the ceiling — readers only ever bump an Arc \
         and read a snapshot lock-free; the overlay copy happens once per publish, off \
         the read path. Held is the lock-free upper bound (one photo, never refreshed)."
    );
}
