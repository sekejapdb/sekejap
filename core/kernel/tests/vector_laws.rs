//! The vector-ingest RAM gate. The failure it guards: vector ingest made RAM climb
//! with the data. This makes that curve a build failure.
//!
//! Counting allocator; 8 MiB pool; 1536-dim f32 vectors (6 KB each, a 2-page
//! chain every row). 25K rows = 150 MB, 100K rows = 600 MB -- both far past
//! the pool. Heap must not follow the data, and a rescore's heap must follow
//! k, not the candidate count.

use kernel::graph::{Graph, Metric};
use kernel::io::IoMode;
use kernel::store::{Config, Store, SyncMode};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

/// One vector field for these tests; to the kernel a field is an
/// opaque u64, so any constant names it.
const VF: u64 = 1;

static LIVE: AtomicUsize = AtomicUsize::new(0);
/// High-water mark, updated INSIDE alloc. Point-sampling LIVE after a call
/// returns missed a 30MB transient collect entirely -- the negative check
/// (a rescore that gathers all candidate vectors first) PASSED against the
/// sampled bound. Peaks are recorded where they happen or not at all.
static PEAK: AtomicUsize = AtomicUsize::new(0);
struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let now = LIVE.fetch_add(l.size(), Relaxed) + l.size();
            PEAK.fetch_max(now, Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, n) };
        if !q.is_null() { LIVE.fetch_add(n, Relaxed); LIVE.fetch_sub(l.size(), Relaxed); }
        q
    }
}
#[global_allocator]
static A: Counting = Counting;

const DIM: usize = 1536;

fn vec_for(i: u64) -> Vec<f32> {
    let s = i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (0..DIM).map(|j| ((s.wrapping_add(j as u64) % 1000) as f32) / 500.0 - 1.0).collect()
}

fn ingest(n: u64) -> (f64, f64) {
    let d = tempfile::TempDir::new().unwrap();
    let cfg = Config { budget_bytes: 8 << 20, io: IoMode::Buffered, sync: SyncMode::Off };
    let mut g = Graph::new(Store::create(d.path(), cfg).unwrap()).unwrap();
    let base = LIVE.load(Relaxed);
    let mut peak = 0usize;
    for i in 1..=n {
        g.set_vec(VF, i, &vec_for(i)).unwrap();
        if i % 512 == 0 { peak = peak.max(LIVE.load(Relaxed).saturating_sub(base)); }
    }
    g.commit().unwrap();
    peak = peak.max(LIVE.load(Relaxed).saturating_sub(base));

    // rescore 5K candidates for top-10: heap must be ~k, not ~candidates.
    // PEAK-based: the high-water DURING the call, not a sample after it.
    let cands: Vec<u64> = (1..=5_000.min(n)).collect();
    let q = vec_for(999_983);
    let before = LIVE.load(Relaxed);
    PEAK.store(before, Relaxed);
    let mut rpeak = 0usize;
    for _ in 0..3 {
        let top = g.rescore(VF, &cands, &q, Metric::Cosine, 10).unwrap();
        assert_eq!(top.len(), 10);
        rpeak = rpeak.max(PEAK.load(Relaxed).saturating_sub(before));
    }
    let mib = |b: usize| b as f64 / (1 << 20) as f64;
    (mib(peak), mib(rpeak))
}

#[test]
fn vector_ingest_and_rescore_hold_law_1() {
    let (w1, r1) = ingest(25_000);
    let (w2, r2) = ingest(100_000);
    eprintln!("ingest heap: 25k={w1:.2} MiB 100k={w2:.2} MiB (data 150MB -> 600MB)");
    eprintln!("rescore heap: {r1:.3}/{r2:.3} MiB for 5K candidates, k=10");
    assert!(
        w2 < w1 * 2.0 + 1.0,
        "the vector-ingest RAM curve is back: ingest heap {w1:.2} -> {w2:.2} MiB over 4x vectors"
    );
    // one 6KB vector in flight + top-10 heap; anything near the candidate
    // count (5K x 6KB = 30MB) means rescore is collecting, not streaming
    assert!(r1 < 1.0 && r2 < 1.0, "rescore heap {r1:.3}/{r2:.3} MiB is not ∝ k");
}

/// 2m: the fold's WAL disk high-water is bounded by the checkpoint
/// interval, not by the batch. Same data folded twice: an effectively
/// unbounded interval leaves the whole batch's row images in the WAL;
/// a small interval leaves at most one interval's worth.
#[test]
fn fold_wal_is_bounded_by_the_checkpoint_interval() {
    use kernel::graph::Graph;
    use kernel::store::{Config, Store};
    let wal_after = |every: u64| -> u64 {
        let d = tempfile::TempDir::new().unwrap();
        let mut g = Graph::new(Store::create(d.path(), Config::default()).unwrap()).unwrap();
        let vecf = |i: u64| -> Vec<f32> {
            let s = i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            (0..16).map(|j| ((s ^ (j as u64).wrapping_mul(0xD1B5_4A32_D192_ED03)) % 1000) as f32 / 500.0 - 1.0).collect()
        };
        for i in 1..=1200u64 { g.set_vec(VF, i, &vecf(i)).unwrap(); }
        g.commit().unwrap();
        g.checkpoint().unwrap();
        g.set_nav_fold_interval(every);
        g.fold_nav(VF).unwrap();
        std::fs::metadata(d.path().join("wal")).map(|m| m.len()).unwrap_or(0)
    };
    let unbounded = wal_after(u64::MAX);
    let bounded = wal_after(300);
    assert!(bounded * 2 < unbounded,
            "bounded fold WAL {bounded}B must be well under unbounded {unbounded}B");
}
