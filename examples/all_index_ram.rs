//! Which of the remaining index types is actually unbounded?
//!
//! The btree index is done: 0.4 MB resident after a fold, 0.2 MB peak during
//! one, flat from 100 000 rows to a million. Four are left — the spatial grid,
//! GIN, the search FST and BM25 — and the honest thing is to measure which is
//! worst rather than fix the one that seems worst.
//!
//! Two numbers per index, because the btree had a different bug behind each:
//!
//!   * `live_after_fold` — heap still held once the index has been written to
//!     disk. The btree kept the whole column here (96.6 MB at a million rows)
//!     and nothing noticed, because reads were served from it and answered
//!     correctly.
//!   * `fold_peak` — heap high-water mark while folding a *small* change into a
//!     large store. This is Law 2: the trigger is the change, so the work must
//!     be too.
//!
//! Counted, not sampled. Sampling RSS gave 2, 20 and 22 MB for identical runs
//! and hid the resident index inside that spread.

use sekejap::{AutoCompact, Config, CoreDB, SyncMode};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let n = LIVE.fetch_add(l.size(), Ordering::Relaxed) + l.size();
            PEAK.fetch_max(n, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new >= l.size() {
                let n = LIVE.fetch_add(new - l.size(), Ordering::Relaxed) + (new - l.size());
                PEAK.fetch_max(n, Ordering::Relaxed);
            } else {
                LIVE.fetch_sub(l.size() - new, Ordering::Relaxed);
            }
        }
        q
    }
}
#[global_allocator]
static A: Counting = Counting;

fn mb(b: usize) -> f64 { b as f64 / 1_048_576.0 }

/// One row shape carrying a number, a point and some text, so every index type
/// is built over the same data and the numbers are comparable.
fn row(i: usize) -> (String, serde_json::Value) {
    let lon = 115.0 + (i % 1000) as f64 * 0.001;
    let lat = -8.8 - (i % 997) as f64 * 0.001;
    // SHAPE isolates what the row carries, because the first run of this probe
    // showed an identical fold cost for every index type — including none —
    // which means the cost was coming from the data, not the index.
    let shape = std::env::var("SHAPE").unwrap_or_else(|_| "full".into());
    let mut v = serde_json::json!({
        "_collection": "items",
        "_key": format!("n{i}"),
        "n": i,
    });
    let m = v.as_object_mut().unwrap();
    if shape == "full" || shape == "geo" {
        m.insert("geometry".into(), serde_json::json!({"type": "Point", "coordinates": [lon, lat]}));
    }
    if shape == "full" || shape == "text" {
        m.insert("body".into(), serde_json::json!(
            format!("alpha{} beta{} gamma{} delta{}", i % 997, i % 89, i % 13, i % 3)));
    }
    (format!("items/n{i}"), v)
}

fn load(db: &mut CoreDB, from: usize, n: usize) {
    const CHUNK: usize = 25_000;
    let mut done = 0;
    while done < n {
        let take = CHUNK.min(n - done);
        db.put_value_bulk((from + done..from + done + take).map(row).collect()).unwrap();
        done += take;
    }
}

fn measure(kind: &str, n: usize) {
    let dir = std::env::temp_dir().join(format!("sk_allidx_{}_{}_{}", std::process::id(), kind, n));
    let _ = std::fs::remove_dir_all(&dir);
    let cfg = Config { wal_sync: SyncMode::Off, auto_compact: AutoCompact::Off, ..Config::default() };
    let mut db = CoreDB::open_with_config(&dir, cfg).unwrap();
    db.execute("CREATE TABLE items (n INTEGER, body TEXT)").unwrap();
    load(&mut db, 0, n);

    match kind {
        "none"    => {}
        "btree"   => { db.execute("CREATE INDEX ON items USING btree (n)").unwrap(); }
        "spatial" => { db.build_spatial_index(); }
        "gin"     => { db.execute("CREATE INDEX ON items USING gin (body)").unwrap(); }
        "search"  => { db.execute("CREATE INDEX ON items USING search (body)").unwrap(); }
        "bm25"    => { db.execute("CREATE INDEX ON items USING bm25 (body)").unwrap(); }
        _ => unreachable!(),
    }

    db.compact().unwrap();                       // the fold that should hand it to disk
    let live_after = LIVE.load(Ordering::Relaxed);

    // NODELTA folds with nothing pending, which separates "the fold costs the
    // store" from "the fold costs whatever the change dragged in".
    if std::env::var("NODELTA").is_err() {
        load(&mut db, n, 1_000);                 // a small change against a large store
    }
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    db.compact().unwrap();
    let peak = PEAK.load(Ordering::Relaxed);

    println!("{:<9} {:>10} {:>18.1} {:>14.1}", kind, n, mb(live_after), mb(peak.saturating_sub(before)));

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

fn main() {
    let sizes: Vec<usize> = match std::env::args().nth(1) {
        Some(s) => s.split(',').filter_map(|x| x.parse().ok()).collect(),
        None => vec![100_000, 250_000, 500_000],
    };
    let kinds: Vec<String> = match std::env::args().nth(2) {
        Some(s) => s.split(',').map(|x| x.to_string()).collect(),
        None => ["none", "btree", "spatial", "gin", "search", "bm25"]
            .iter().map(|s| s.to_string()).collect(),
    };

    println!("{:<9} {:>10} {:>18} {:>14}", "index", "rows", "live_after_fold_MB", "fold_peak_MB");
    println!("{}", "-".repeat(55));
    for k in &kinds {
        for &n in &sizes {
            measure(k, n);
        }
        println!();
    }
    println!("flat across sizes -> bounded. rising -> proportional to the store.");
    println!("btree is the reference: it reads ~0.4 / ~0.2 and does not move.");
}
