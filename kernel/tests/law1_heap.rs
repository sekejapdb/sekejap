//! Law 1: live heap must not grow with the store. Counting allocator, not RSS
//! (RSS counts page cache doing its job). Growth ABOVE the post-open baseline,
//! plus the absolute figure so the pool itself is checked flat too.
//! Bound is *2+1MiB: rows quadruple, so proportional lands near 4x, bounded
//! near 1x -- the threshold sits between. A looser bound once swallowed a
//! textbook 4x violation.

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

fn peaks(rows: u64) -> (f64, f64, f64) {
    let d = tempfile::TempDir::new().unwrap();
    // pool far smaller than the biggest store, so it must do its job
    let cfg = Config { budget_bytes: 16 << 20, io: IoMode::Buffered, sync: SyncMode::Off };
    let mut s = Store::create(d.path(), cfg).unwrap();
    let base = LIVE.load(Relaxed);
    let v = vec![b'x'; 200];
    let mut pw = 0usize;
    for i in 0..rows {
        s.put(&i.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_be_bytes(), &v).unwrap();
        if i % 4096 == 0 { pw = pw.max(LIVE.load(Relaxed).saturating_sub(base)); }
    }
    s.commit().unwrap();
    let mut pr = 0usize;
    let mut n = 0u64;
    for (i, r) in s.scan(&[]).unwrap().enumerate() {
        r.unwrap(); n += 1;
        if i % 4096 == 0 { pr = pr.max(LIVE.load(Relaxed).saturating_sub(base)); }
    }
    assert_eq!(n, rows);
    (mib(pw), mib(pr), mib(LIVE.load(Relaxed)))
}

#[test]
fn heap_does_not_grow_with_the_store() {
    let (w1, r1, a1) = peaks(200_000);
    let (w2, r2, a2) = peaks(800_000);
    eprintln!("200k: write {w1:.1} read {r1:.1} abs {a1:.1} MiB");
    eprintln!("800k: write {w2:.1} read {r2:.1} abs {a2:.1} MiB");
    assert!(w2 < w1 * 2.0 + 1.0, "write heap grew {w1:.1} -> {w2:.1} MiB over 4x rows");
    assert!(r2 < r1 * 2.0 + 1.0, "read heap grew {r1:.1} -> {r2:.1} MiB: scan accumulating");
    assert!(a2 < a1 * 2.0 + 1.0, "absolute heap grew {a1:.1} -> {a2:.1} MiB: budget not fixed");
}
