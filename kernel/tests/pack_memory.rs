//! Law 1 for the bulk pack: peak RAM must not grow with the number of rows.
//!
//! `rss_anon` is not usable for this in-process -- glibc does not return freed
//! pages to the OS, so a level that was held and then dropped still shows up in
//! the RSS of a later measurement. A counting global allocator measures what we
//! actually mean: the high-water mark of live heap bytes during one call.
//!
//! This lives in its own integration binary because `#[global_allocator]` is
//! per-binary and the measurement wants a single thread running at a time.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use kernel::budget::MemoryBudget;
use kernel::io::{open_file, IoMode};
use kernel::page::PAGE_SIZE;
use kernel::pool::BufferPool;
use kernel::Result;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// Only `alloc`/`dealloc` are overridden: `GlobalAlloc`'s default `realloc` and
// `alloc_zeroed` are written in terms of them, so a `Vec` doubling shows up as
// the honest "both buffers live at once" peak rather than as an in-place resize
// the counter never sees.
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
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Peak live heap bytes ABOVE the level already live when `f` started.
fn peak_extra_bytes<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = LIVE.load(Ordering::SeqCst);
    PEAK.store(base, Ordering::SeqCst);
    let out = f();
    (out, PEAK.load(Ordering::SeqCst).saturating_sub(base))
}

fn pool_in(dir: &std::path::Path, frames: usize) -> BufferPool {
    let (file, _) = open_file(&dir.join("data"), IoMode::Buffered).unwrap();
    let budget = Arc::new(MemoryBudget::new(frames * PAGE_SIZE + (1 << 20)));
    BufferPool::new(file.into(), budget, frames).unwrap()
}

fn rows(n: u64) -> impl Iterator<Item = Result<(Vec<u8>, Vec<u8>, bool)>> {
    (0..n).map(|i| Ok((i.to_be_bytes().to_vec(), i.to_le_bytes().to_vec(), false)))
}

/// Pack the same tree at three row counts two orders of magnitude apart and
/// require the peak heap to be the SAME NUMBER, not merely a similar one.
///
/// The bound is on the DELTA between measurements, in absolute bytes, and that
/// is the whole point of the test rather than a detail of it. The first version
/// of this test asserted a ratio (`large < small * 2`) and it did fail on the
/// Vec-holding implementation it was written against -- 7.22x -- but only
/// because that implementation's constant floor was ~120 KiB. Streaming the
/// levels raised the floor to 537,648 B (two 256 KiB stream buffers), and a
/// floor is added to BOTH sides of a ratio: with the Task 20 defect
/// reintroduced verbatim the same fixture reported 1.91x and PASSED, while the
/// held vector was growing 76 KiB -> 636 KiB -> 13.4 MB across these three row
/// counts. A bound expressed relative to a quantity the change under test also
/// moves stops testing what it was written for, silently, and this project has
/// now shipped five tests that could not fail.
///
/// A delta bound has no such coupling: the floor cancels, both measurements are
/// taken with the same buffers, and what is left is exactly the property Law 1
/// states -- memory that does not grow with rows. The slack is 64 KiB against a
/// defect that shows 559 KiB at the 10x point and 13.4 MB at the 100x one.
#[test]
fn pack_tree_memory_is_flat_in_rows() {
    // 100x apart. The largest point is where a linear implementation stops
    // being arguable: the defect holds ~140 MB at the 209M-row reference load,
    // and this is the closest a unit test gets to seeing that.
    let counts = [200_000u64, 2_000_000, 20_000_000];
    let mut peaks = Vec::new();
    for n in counts {
        let d = tempfile::tempdir().unwrap();
        let pool = pool_in(d.path(), 64);
        let scratch = d.path().join("scratch");
        let _ = pool.allocate().unwrap(); // reserve page 0, as every real caller does
        let (root, peak) = peak_extra_bytes(|| {
            kernel::bulk::pack_tree(&pool, 1, rows(n), 0.9, &scratch).unwrap()
        });
        assert!(root > 0, "{n} rows must have produced a root");
        eprintln!("rows {n:>10} -> peak {peak} B");
        peaks.push(peak);
    }

    let lo = *peaks.iter().min().unwrap();
    let hi = *peaks.iter().max().unwrap();
    let delta = hi - lo;
    eprintln!(
        "peak spread {delta} B across a {}x row range (ratio {:.2}x)",
        counts[counts.len() - 1] / counts[0],
        hi as f64 / lo as f64,
    );
    // Absolute, because the quantity being bounded is itself absolute: "how
    // many more bytes does 100x the rows cost". Not a ratio -- see above.
    const SLACK: usize = 64 * 1024;
    assert!(
        delta <= SLACK,
        "peak allocation grew with rows: {delta} B of spread across {counts:?} rows \
         ({peaks:?}), more than the {SLACK} B a row-independent pack is allowed. \
         Law 1: no RAM proportional to the store."
    );
}
