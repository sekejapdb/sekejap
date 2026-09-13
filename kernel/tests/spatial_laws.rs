//! Law 1 for spatial: live heap flat across a 4x geometry corpus.
use kernel::graph::Graph;
use kernel::io::IoMode;
use kernel::spatial::Geom;
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
        let lat = -38.0 + ((i * 7919) % 2_000) as f64 / 1000.0;
        let lon = 144.0 + ((i * 104729) % 2_000) as f64 / 1000.0;
        if i % 10 == 0 {
            g.set_geo(1, i, &Geom::Polygon(vec![vec![
                [lon, lat], [lon + 0.01, lat], [lon + 0.01, lat + 0.01],
                [lon, lat + 0.01], [lon, lat]]])).unwrap();
        } else {
            g.set_geo(1, i, &Geom::Point(lon, lat)).unwrap();
        }
        if i % 5_000 == 0 { g.commit().unwrap(); g.checkpoint().unwrap(); }
    }
    let q = g.within_radius(1, -37.5, 144.9, 10_000.0, 50).unwrap();
    assert!(!q.is_empty());
    LIVE.load(Relaxed).saturating_sub(base)
}

#[test]
fn geo_ingest_heap_is_flat_across_4x() {
    let a = ingest(10_000);
    let b = ingest(40_000);
    let mib = |x: usize| x as f64 / (1024.0 * 1024.0);
    eprintln!("live after 10K: {:.2} MiB, after 40K: {:.2} MiB", mib(a), mib(b));
    assert!(b < 4 << 20, "heap {:.2} MiB after 40K geometries", mib(b));
    assert!(b.saturating_sub(a) < 1 << 20,
            "heap grew {:.2} MiB across 4x -- RAM tracks the store", mib(b.saturating_sub(a)));
}
