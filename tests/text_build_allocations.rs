//! What one posting of a late text build costs in allocations, and whether it
//! still spills scratch files to disk.
//!
//! The packed text build used to push every `(term, document, frequency)`
//! triple through the external sorter the scalar and spatial builds use. That
//! cost 6.92 allocations per posting, measured at 200,000 rows and 1.2M
//! postings: a clone of the key and of the value into the sorter, an 8 MiB
//! budget that spilled three times at that size, a read-back of every spilled
//! run to verify it, and a second read-back of ~86% of the postings during the
//! merge -- plus a fresh `BTreeMap<String, u32>` per document in the analyzer.
//!
//! Accumulating each term's postings under that term instead removes the sort,
//! the spill and the per-document map. This test is the budget that keeps it
//! removed: allocations per posting, and the absence of any spill directory.
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

// A reallocation is counted: it is the same cost the build is being asked
// about, and `Vec` growth is exactly what the old push-a-clone-per-posting
// shape spent it on.
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

const DOCUMENTS: u64 = 20_000;
/// Six distinct terms a document, one of them said twice.
const TERMS_PER_DOCUMENT: u64 = 6;
const VOCABULARY: [&str; 16] = [
    "flood", "levee", "river", "bridge", "survey", "harbour", "silt", "canal", "tide", "rail",
    "quay", "weir", "sluice", "wharf", "dyke", "basin",
];

fn body(i: u64) -> String {
    let mut out = String::new();
    for j in 0..TERMS_PER_DOCUMENT {
        if j > 0 {
            out.push(' ');
        }
        out.push_str(VOCABULARY[((i + j * 3) % VOCABULARY.len() as u64) as usize]);
        if j == 0 {
            // A repeat, so the term frequency path is exercised too.
            out.push(' ');
            out.push_str(VOCABULARY[(i % VOCABULARY.len() as u64) as usize]);
        }
    }
    out
}

#[test]
fn a_late_text_build_allocates_under_one_and_a_half_times_per_posting_and_never_spills() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("allocdb");
    let scratch = std::env::temp_dir().join("e4-index-sort").join("allocdb");
    let _ = std::fs::remove_dir_all(&scratch);

    let cfg = Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    };
    let mut db = Database::create(&path, cfg).unwrap();
    let people = db
        .create_collection(
            "people",
            vec![("bio".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    for i in 0..DOCUMENTS {
        db.put(people, &format!("p{i:06}"), &json!({ "bio": body(i) }))
            .unwrap();
        if i % 256 == 255 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    db.checkpoint().unwrap();

    let bio = db.create_text_index(people, "bio_idx", "bio").unwrap();
    db.commit().unwrap();

    ALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    db.build_index_to_ready(bio, 256).unwrap();
    COUNTING.store(false, Ordering::Relaxed);
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);

    let postings = DOCUMENTS * TERMS_PER_DOCUMENT;
    let per_posting = allocations as f64 / postings as f64;
    eprintln!(
        "COST text build {allocations} allocations over {postings} postings \
         = {per_posting:.3} per posting, against 6.92 before the sort was removed"
    );
    assert!(
        per_posting <= 1.5,
        "a late text build allocates {per_posting:.3} times per posting, over the 1.5 budget \
         (6.92 before the sort was removed)"
    );
    assert!(
        !scratch.exists(),
        "the text build spilled sort scratch to {}; it is not supposed to sort at all",
        scratch.display()
    );
}
