//! What one row of a late SCALAR build costs in heap allocations.
//!
//! The build derives a key per row and hands it to the external sorter, which
//! takes its own copy. That copy is the sorter's storage and cannot be
//! avoided here. Everything around it could be: `scalar_key::encode` returned
//! a fresh `Vec` for the encoded value, `skey` allocated a one-byte `vec![tag]`
//! and grew it, and each of its two `ordered` calls allocated a throwaway
//! `Vec` of its own -- all four dropped a line later, once per row, for every
//! row of the collection.
//!
//! The `_into` variants write into one scratch buffer the scan reuses, and the
//! scan pushes the buffer borrowed, so the sorter's own copy is the only one
//! the scan is charged for: 7.084 allocations a row became 4.085. This test is
//! the budget that keeps the three removed ones gone.
use e4_prototype::{
    collections::{CollectionOptions, Database},
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static COUNTING: AtomicBool = AtomicBool::new(false);

struct Counting;

// A reallocation counts: growing `vec![tag]` from one byte to the key's width
// is exactly the cost this is asking about.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const ROWS: u64 = 20_000;

/// The whole build, not only the scan: the sort and the pack after it are the
/// same work in both shapes, so the difference is the scan's.
///
/// Measured on this corpus: **7.084 allocations per row before, 4.085 after**
/// -- the three the scan was throwing away, gone.
///
/// The bound is not near zero, and cannot be from here. The four that remain
/// belong to code this change does not touch: `ExternalSorter` stores every
/// record as its own `(Vec<u8>, Vec<u8>)`, so the sorter's copy is one per row
/// by construction, and the rest are inside the kernel's bulk packer. Removing
/// those is a change to the sorter's storage and to `kernel/src/bulk.rs`, not
/// to this build path. 4.2 is a regression guard on what this path owns.
const BUDGET_PER_ROW: f64 = 4.2;

#[test]
fn a_late_scalar_build_allocates_no_more_than_the_sorter_itself_needs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("allocdb");

    let cfg = Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    };
    let mut db = Database::create(&path, cfg).unwrap();
    let ticks = db
        .create_collection(
            "ticks",
            vec![("n".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    for i in 0..ROWS {
        // Not in key order, so the sort does real work.
        let n = ((i.wrapping_mul(2_654_435_761)) % 1_000_003) as i64;
        db.put(ticks, &format!("t{i:06}"), &json!({ "n": n })).unwrap();
        if i % 256 == 255 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    db.checkpoint().unwrap();

    let n_idx = db.create_scalar_index(ticks, "n_idx", "n", false).unwrap();
    db.commit().unwrap();

    ALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    db.build_index_to_ready(n_idx, 256).unwrap();
    COUNTING.store(false, Ordering::Relaxed);
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);

    let per_row = allocations as f64 / ROWS as f64;
    eprintln!(
        "COST scalar build {allocations} allocations over {ROWS} rows = {per_row:.3} per row, \
         against 7.084 before the scan stopped allocating a key and a value each row"
    );
    assert!(
        per_row <= BUDGET_PER_ROW,
        "a late scalar build allocates {per_row:.3} times per row, over the \
         {BUDGET_PER_ROW} budget"
    );
}
