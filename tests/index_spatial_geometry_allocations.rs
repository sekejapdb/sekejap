//! Item X1a, DO(6): a 200K-geometry late build stays inside a small WAL
//! budget (proving it commits in bounded byte-sized groups, not one
//! transaction per index) and its allocations-per-row stay flat rather than
//! growing with the corpus. Mirrors `tests/scalar_build_allocations.rs`'s
//! counting-allocator pattern, adapted to the geometry family (which, unlike
//! scalar, pushes up to `MAX_CELLS` sorter entries per row instead of one).
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

const ROWS: u64 = 200_000;
/// A budget clearly too small to hold all 200K postings (~30 bytes each,
/// ~6 MB) in one transaction: the build can only finish under it by
/// committing in several byte-bounded groups (`indexes::sorted_run_budget`),
/// not as one all-or-nothing write.
const BUILD_BUDGET_BYTES: usize = 2 << 20;
/// Generous, not tuned: this is a flatness regression guard (allocations
/// must not grow with the corpus, i.e. stay roughly constant per row as N
/// scales to 200K), not a performance target. Measured on this corpus:
/// ~20.1 allocations/row, higher than the scalar build's own (~4/row)
/// because `build_geometry_entries` decodes the WHOLE document via
/// `decode_with_vector_values` (mirroring `spatial_indexes::build_point_entry`,
/// item X1a's model file) rather than reading just the one field, and then
/// allocates a `Vec<GeometryEntry>`/`Vec<(u32,u32)>` cover per row even
/// though a Point posts to exactly one cell.
const BUDGET_PER_ROW: f64 = 26.0;

#[test]
fn a_late_geometry_build_of_200k_rows_commits_in_bounded_groups_with_flat_allocations() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("geoallocdb");

    let cfg = Config {
        budget_bytes: BUILD_BUDGET_BYTES,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    };
    let mut db = Database::create(&path, cfg).unwrap();
    let shapes = db
        .create_collection(
            "shapes",
            vec![("shape".into(), Kind::Geo)],
            CollectionOptions::default(),
        )
        .unwrap();
    for i in 0..ROWS {
        let lon = ((i.wrapping_mul(2_654_435_761) % 360_000) as f64 / 1000.0) - 180.0;
        let lat = ((i.wrapping_mul(40_503_589) % 180_000) as f64 / 1000.0) - 90.0;
        db.put(
            shapes,
            &format!("s{i:07}"),
            &json!({ "shape": {"type": "Point", "coordinates": [lon, lat]} }),
        )
        .unwrap();
        if i % 4096 == 4095 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    db.checkpoint().unwrap();

    let index = db
        .create_geometry_index(shapes, "by_shape", "shape")
        .unwrap();
    db.commit().unwrap();

    ALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    // Succeeding at all, under a budget far smaller than the whole index,
    // is itself the evidence for bounded, byte-budgeted commits: this would
    // return `Err(ResourceLimit(_))` if the build tried to do it in one
    // transaction.
    let chunks = db.build_index_to_ready(index, 256).unwrap();
    COUNTING.store(false, Ordering::Relaxed);
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);

    assert!(chunks > 0);
    let per_row = allocations as f64 / ROWS as f64;
    eprintln!(
        "COST geometry build {allocations} allocations over {ROWS} rows = {per_row:.3} per row \
         under a {BUILD_BUDGET_BYTES}-byte WAL budget"
    );
    assert!(
        per_row <= BUDGET_PER_ROW,
        "a late geometry build allocates {per_row:.3} times per row, over the {BUDGET_PER_ROW} budget"
    );

    db.commit().unwrap();
    assert_eq!(db.index_info(index).unwrap().state, e4_prototype::collections::IndexState::Ready);
}
