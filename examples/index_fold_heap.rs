//! Peak *heap* during the index fold, counted rather than sampled.
//!
//! Sampling RSS with `ps` gave 2 MB, 20 MB and 22 MB for identical runs — the
//! page cache and the allocator's own retention swamp the signal, and a number
//! that varies tenfold between runs cannot support a claim either way.
//!
//! So this counts every allocation the process makes and records the high-water
//! mark across a compaction. Deterministic, and it measures the thing Law 1 is
//! actually about: memory the program is holding, not pages the OS happens to
//! have mapped.

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
            let now = LIVE.fetch_add(l.size(), Ordering::Relaxed) + l.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
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
                let now = LIVE.fetch_add(new - l.size(), Ordering::Relaxed) + (new - l.size());
                PEAK.fetch_max(now, Ordering::Relaxed);
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

fn load(db: &mut CoreDB, from: usize, n: usize, distinct: usize) {
    const CHUNK: usize = 25_000;
    let mut done = 0;
    while done < n {
        let take = CHUNK.min(n - done);
        let rows: Vec<(String, serde_json::Value)> = (from + done..from + done + take)
            .map(|i| (format!("items/n{i}"),
                 serde_json::json!({"_collection":"items","_key":format!("n{i}"),"n": i % distinct})))
            .collect();
        db.put_value_bulk(rows).unwrap();
        done += take;
    }
}

fn main() {
    let distinct: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
    let indexed = std::env::var("NOIDX").is_err();
    println!("{} column, {} distinct values\n", if indexed { "indexed" } else { "unindexed" }, distinct);
    println!("{:>12} {:>16} {:>16}", "rows", "live_before_MB", "fold_peak_MB");
    println!("{}", "-".repeat(48));

    for &n in &[100_000usize, 250_000, 500_000, 1_000_000] {
        let dir = std::env::temp_dir().join(format!("sk_foldheap_{}_{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = Config { wal_sync: SyncMode::Off, auto_compact: AutoCompact::Off, ..Config::default() };
        let mut db = CoreDB::open_with_config(&dir, cfg).unwrap();

        db.execute("CREATE TABLE items (n INTEGER)").unwrap();
        load(&mut db, 0, n, distinct);
        if indexed {
            db.execute("CREATE INDEX ON items USING btree (n)").unwrap();
        }
        db.compact().unwrap();      // build the base
        load(&mut db, n, 1_000, distinct);   // a small change against a large store

        let before = LIVE.load(Ordering::Relaxed);
        PEAK.store(before, Ordering::Relaxed);
        db.compact().unwrap();
        let peak = PEAK.load(Ordering::Relaxed);

        println!("{:>12} {:>16.1} {:>16.1}", n, mb(before), mb(peak.saturating_sub(before)));

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
    println!();
    println!("fold_peak flat  -> bounded by the change. rising -> bounded by the store.");
}
