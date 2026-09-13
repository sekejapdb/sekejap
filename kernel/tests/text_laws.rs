//! Law 1 for full-text: live heap must not grow with the indexed corpus.
//! Same counting-allocator + PEAK discipline as the vector gate.

use kernel::graph::Graph;
use kernel::io::IoMode;
use kernel::store::{Config, Store, SyncMode};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let now = LIVE.fetch_add(l.size(), Relaxed) + l.size();
        PEAK.fetch_max(now, Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Relaxed);
        unsafe { System.dealloc(p, l) }
    }
}
#[global_allocator]
static A: Counting = Counting;

fn ingest(n: u64) -> usize {
    let d = tempfile::TempDir::new().unwrap();
    let cfg = Config { budget_bytes: 8 << 20, io: IoMode::Buffered, sync: SyncMode::Off };
    let mut g = Graph::new(Store::create(d.path(), cfg).unwrap()).unwrap();
    let base = LIVE.load(Relaxed);
    for i in 1..=n {
        let text = format!("record number {i} of the survey batch {} region {}",
                           i % 97, i % 13);
        g.index_text(1, i, &text).unwrap();
        if i % 5_000 == 0 { g.commit().unwrap(); g.checkpoint().unwrap(); }
    }
    g.fold_text(1).unwrap();
    let q = g.text_search(1, "survey region", 10).unwrap();
    assert_eq!(q.len(), 10);
    LIVE.load(Relaxed).saturating_sub(base)
}

#[test]
fn text_ingest_heap_is_flat_across_4x_corpus() {
    let a = ingest(10_000);
    let b = ingest(40_000);
    let mib = |x: usize| x as f64 / (1024.0 * 1024.0);
    eprintln!("live after 10K: {:.2} MiB, after 40K: {:.2} MiB", mib(a), mib(b));
    assert!(b < 6 << 20,
            "live heap {:.2} MiB after 40K docs; the 8 MiB pool should bound it", mib(b));
    assert!(b.saturating_sub(a) < 2 << 20,
            "heap grew {:.2} MiB across 4x corpus -- RAM is tracking the store", mib(b.saturating_sub(a)));
}
