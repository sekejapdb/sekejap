//! Laws 1 and 2 on the property-index paths specifically. e1's core failure
//! was RAM growing with the data; "the index is just keys in the same pool"
//! is a construction argument, and e1 had those too. Measure it.

use kernel::graph::Graph;
use kernel::io::IoMode;
use kernel::keys::enc_f64;
use kernel::store::{Config, Store, SyncMode};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

static LIVE: AtomicUsize = AtomicUsize::new(0);
struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() { LIVE.fetch_add(l.size(), Relaxed); }
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

const PRICE: u64 = 100;

fn build(n: u64) -> (tempfile::TempDir, Graph, usize) {
    let d = tempfile::TempDir::new().unwrap();
    // pool far smaller than the larger store: the index cannot hide in RAM
    let cfg = Config { budget_bytes: 8 << 20, io: IoMode::Buffered, sync: SyncMode::Off };
    let mut g = Graph::new(Store::create(d.path(), cfg).unwrap()).unwrap();
    let base = LIVE.load(Relaxed);
    let mut peak = 0usize;
    for i in 0..n {
        let id = g.add_node(None, 7, &vec![b'x'; 60]).unwrap();
        g.set_prop(PRICE, enc_f64((i % 5000) as f64 / 10.0), id).unwrap();
        if i % 4096 == 0 { peak = peak.max(LIVE.load(Relaxed).saturating_sub(base)); }
    }
    g.commit().unwrap();
    peak = peak.max(LIVE.load(Relaxed).saturating_sub(base));
    (d, g, peak)
}

#[test]
fn prop_index_is_disk_first_and_flat() {
    let (_d1, g1, w1) = build(100_000);
    let (_d2, g2, w2) = build(400_000);
    let mib = |b: usize| b as f64 / (1 << 20) as f64;
    eprintln!("index-write heap: 100k={:.2} MiB 400k={:.2} MiB", mib(w1), mib(w2));

    // Law 1, writes: indexing 4x the rows must not need more heap.
    assert!(
        mib(w2) < mib(w1) * 2.0 + 1.0,
        "index-write heap grew {:.2} -> {:.2} MiB over 4x rows",
        mib(w1), mib(w2)
    );

    // Law 2, reads: a SAME-SELECTIVITY range (fixed value window ~2% of the
    // domain) must cost reads ∝ its RESULT SIZE, so reads-per-result-row must
    // be flat even though the store quadrupled. And Law 1: the zero-alloc
    // count path must hold O(1) heap regardless of how many rows it counts.
    let window = (enc_f64(100.0), enc_f64(110.0));
    let measure = |g: &mut Graph| -> (f64, f64) {
        g.store().io_stats().unwrap().take();
        let base = LIVE.load(Relaxed);
        let rows = g.count_prop_range(PRICE, window.0, window.1).unwrap();
        let heap = LIVE.load(Relaxed).saturating_sub(base);
        let (_, _, reads) = g.store().io_stats().unwrap().take();
        assert!(rows > 500, "window matched only {rows}; fixture too thin");
        (reads as f64 / rows as f64, mib(heap))
    };
    let mut g1 = g1; let mut g2 = g2;
    let (r1, h1) = measure(&mut g1);
    let (r2, h2) = measure(&mut g2);
    eprintln!("range reads/row: 100k={r1:.4} 400k={r2:.4}; query heap {h1:.3}/{h2:.3} MiB");
    assert!(
        r2 < r1 * 1.5 + 0.02,
        "reads per result row grew {r1:.4} -> {r2:.4} over a 4x store: the index \
         read cost is following the database, not the answer"
    );
    assert!(h1 < 0.05 && h2 < 0.05,
        "the count path allocated {h1:.3}/{h2:.3} MiB; it is supposed to be zero-alloc");
}
