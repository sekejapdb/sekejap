//! Gate arm (d): Law 1 on graph ops. Live heap flat across a 4x graph, for
//! both the build and a BFS whose reachable set is bounded by depth.
//! Counting allocator, growth above post-open baseline, absolute also checked.
//! Bound *2+1MiB: 4x rows -> proportional lands near 4x, bounded near 1x.

use kernel::graph::Graph;
use kernel::io::IoMode;
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

fn mib(b: usize) -> f64 { b as f64 / (1 << 20) as f64 }

fn peaks(n: u64) -> (f64, f64) {
    let d = tempfile::TempDir::new().unwrap();
    let cfg = Config { budget_bytes: 8 << 20, io: IoMode::Buffered, sync: SyncMode::Off };
    let mut g = Graph::new(Store::create(d.path(), cfg).unwrap()).unwrap();
    let base = LIVE.load(Relaxed);
    let mut pk = 0usize;
    // Nodes first, then edges over the FULL id range -- edges computed
    // against a growing count gave node 1 nothing but self-loops, and a bfs
    // from it was legitimately empty (fixture bug, found by the assert).
    for i in 0..n {
        g.add_node(None, 7, &vec![b'x'; 100]).unwrap();
        if i % 2048 == 0 { pk = pk.max(LIVE.load(Relaxed).saturating_sub(base)); }
    }
    for src in 1..=n {
        for (ty, dst) in kernel::bench_edges(src, n) {
            g.add_edge(0, src, ty, dst, b"").unwrap();
        }
        if src % 2048 == 0 { pk = pk.max(LIVE.load(Relaxed).saturating_sub(base)); }
    }
    g.commit().unwrap();
    let build = mib(pk.max(LIVE.load(Relaxed).saturating_sub(base)));

    let mut pq = 0usize;
    for q in 0..50u64 {
        let root = 1 + (q * 48_271) % n;
        let r = g.bfs(0, root, None, 3).unwrap();
        assert!(!r.is_empty());
        pq = pq.max(LIVE.load(Relaxed).saturating_sub(base));
    }
    (build, mib(pq))
}

#[test]
fn graph_heap_does_not_grow_with_the_store() {
    let (b1, q1) = peaks(50_000);
    let (b2, q2) = peaks(200_000);
    eprintln!("50k:  build {b1:.2} MiB, bfs {q1:.2} MiB");
    eprintln!("200k: build {b2:.2} MiB, bfs {q2:.2} MiB");
    assert!(b2 < b1 * 2.0 + 1.0, "graph build heap grew {b1:.2} -> {b2:.2} MiB over 4x");
    assert!(q2 < q1 * 2.0 + 1.0, "bfs heap grew {q1:.2} -> {q2:.2} MiB over 4x: \
            the traversal is holding the store, not the frontier");
}
