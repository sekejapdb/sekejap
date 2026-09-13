//! Laws 1+2 on overflow paths. RAM ∝ ONE value is allowed and named (assembly
//! buffers the value being read/written); RAM ∝ store is not. Counting
//! allocator; 8 MiB pool against stores of 24 MB and 96 MB of chained values.

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

fn run(n: u64) -> (f64, f64) {
    let d = tempfile::TempDir::new().unwrap();
    let cfg = Config { budget_bytes: 8 << 20, io: IoMode::Buffered, sync: SyncMode::Off };
    let mut s = Store::create(d.path(), cfg).unwrap();
    let base = LIVE.load(Relaxed);
    let v = vec![3u8; 24_000]; // 6-page chain each
    let mut pw = 0usize;
    for i in 0..n {
        s.put(&i.to_be_bytes(), &v).unwrap();
        if i % 64 == 0 { pw = pw.max(LIVE.load(Relaxed).saturating_sub(base)); }
    }
    s.commit().unwrap();
    let mut pr = 0usize;
    for i in (0..n).step_by(7) {
        let got = s.get(&i.to_be_bytes()).unwrap().unwrap();
        assert_eq!(got.len(), 24_000);
        pr = pr.max(LIVE.load(Relaxed).saturating_sub(base));
    }
    let mib = |b: usize| b as f64 / (1 << 20) as f64;
    (mib(pw), mib(pr))
}

#[test]
fn chained_values_hold_the_laws() {
    let (w1, r1) = run(1_000);
    let (w2, r2) = run(4_000);
    eprintln!("write peak {w1:.2}/{w2:.2} MiB, read peak {r1:.2}/{r2:.2} MiB");
    // One 24KB value in flight is ~0.05 MiB; the WAL buffer is 0.25; slack 1.
    assert!(w2 < w1 * 2.0 + 1.0, "chain-write heap grew {w1:.2} -> {w2:.2} over 4x rows");
    assert!(r2 < r1 * 2.0 + 1.0, "chain-read heap grew {r1:.2} -> {r2:.2} over 4x rows");
}
