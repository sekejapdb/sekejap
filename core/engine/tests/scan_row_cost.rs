//! What ONE ROW of a key-only full enumeration is allowed to cost.
//!
//! `SELECT _key FROM person` with no filter and no projection is the cheapest
//! question this engine answers, and it is the one whose per-row constant the
//! loop-7 budget went looking for. Everything here is counted rather than
//! timed, so a regression names the cause:
//!
//!   1. the walk allocates nothing per row -- not for the id, not for the
//!      page's rows, not for the heap entry;
//!   2. the walk touches the buffer pool once per LEAF, not once per row --
//!      a leaf holds tens of rows and one pin has to serve all of them;
//!   3. the ids it returns are the collection's own, decoded from the key the
//!      cursor was standing on, with a second collection in the same tree to
//!      catch a decode that reads the wrong half of the key;
//!   4. a key-only row is charged its own 13 bytes of output -- 12 of
//!      identity and one for the row -- and not a byte more.
use sekejap_core::{
    collections::{
        CandidateDriver, CollectionId, CollectionOptions, Database, Projection, QueryBudget,
        QueryOrder, QueryRequest,
    },
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;
use std::{
    alloc::{GlobalAlloc, Layout as AllocationLayout, System},
    cell::Cell,
    path::Path,
};

thread_local! {
    static COUNT: Cell<usize> = const { Cell::new(0) };
    static TRACK: Cell<bool> = const { Cell::new(false) };
}

struct Alloc;
unsafe impl GlobalAlloc for Alloc {
    unsafe fn alloc(&self, l: AllocationLayout) -> *mut u8 {
        TRACK
            .try_with(|t| {
                if t.get() {
                    COUNT.with(|n| n.set(n.get() + 1));
                }
            })
            .ok();
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: AllocationLayout) {
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: AllocationLayout, n: usize) -> *mut u8 {
        TRACK
            .try_with(|t| {
                if t.get() {
                    COUNT.with(|c| c.set(c.get() + 1));
                }
            })
            .ok();
        System.realloc(p, l, n)
    }
}

#[global_allocator]
static ALLOC: Alloc = Alloc;

fn counted<T>(f: impl FnOnce() -> T) -> (T, usize) {
    COUNT.with(|c| c.set(0));
    TRACK.with(|t| t.set(true));
    let value = f();
    TRACK.with(|t| t.set(false));
    (value, COUNT.with(Cell::get))
}

fn cfg() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

const ROWS: u64 = 20_000;

/// Two collections in the same primary tree, the wanted one SECOND so its
/// keys never start at the beginning of the tree, and both with rows wide
/// enough that a 4 KiB leaf holds tens of them rather than hundreds.
fn fixture(path: &Path) -> (Database, CollectionId, CollectionId) {
    let mut db = Database::create(path, cfg()).unwrap();
    let decoy = db
        .create_collection(
            "decoy",
            vec![("note".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    let person = db
        .create_collection(
            "person",
            vec![("note".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();
    for i in 1..=ROWS {
        db.put(decoy, &format!("d{i:08}"), &json!({ "note": "x" })).unwrap();
        let id = db
            .put(
                person,
                &format!("p{i:08}"),
                &json!({ "note": "a note wide enough that a leaf holds tens of rows, not hundreds" }),
            )
            .unwrap();
        assert_eq!(id.sequence, i, "the fixture rests on sequence == insertion index");
        if i % 512 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    db.checkpoint().unwrap();
    (db, person, decoy)
}

struct Walk {
    ids: Vec<u64>,
    collections: Vec<u32>,
    pages: u64,
    output_bytes: u64,
    allocations: usize,
    pool_accesses: u64,
}

fn enumerate(db: &Database, c: CollectionId, page_size: usize) -> Walk {
    let before = db.pool_accesses().unwrap();
    let ((ids, collections, pages, output_bytes), allocations) = counted(|| {
        let mut prepared = db
            .prepare_query(QueryRequest {
                collection: c,
                filters: &[],
                order: QueryOrder::EntityId,
                projection: Projection::Ids,
                total_limit: None,
                driver: CandidateDriver::Auto,
            })
            .unwrap();
        let mut ids = Vec::with_capacity(ROWS as usize);
        let mut collections = Vec::new();
        let (mut pages, mut output_bytes) = (0u64, 0u64);
        loop {
            let page = prepared
                .next_page(page_size, QueryBudget::unlimited(), || false)
                .unwrap();
            pages += 1;
            output_bytes += page.work.output_bytes;
            for row in &page.rows {
                ids.push(row.id.sequence);
                if collections.last() != Some(&row.id.collection.0) {
                    collections.push(row.id.collection.0);
                }
            }
            if page.done || page.rows.is_empty() {
                break;
            }
        }
        (ids, collections, pages, output_bytes)
    });
    Walk {
        ids,
        collections,
        pages,
        output_bytes,
        allocations,
        pool_accesses: db.pool_accesses().unwrap() - before,
    }
}

#[test]
fn a_key_only_enumeration_does_not_allocate_per_row() {
    let dir = tempfile::tempdir().unwrap();
    let (db, person, _) = fixture(&dir.path().join("db"));
    let walk = enumerate(&db, person, 8192);
    assert_eq!(walk.ids.len(), ROWS as usize);
    let per_row = walk.allocations as f64 / ROWS as f64;
    assert!(
        per_row <= 0.01,
        "a key-only walk of {} rows over {} pages made {} allocations ({per_row:.4} per row); \
         the bound is 0.01 -- a handful per PAGE, none per row",
        walk.ids.len(),
        walk.pages,
        walk.allocations
    );
}

#[test]
fn a_key_only_enumeration_touches_the_pool_once_per_leaf_not_once_per_row() {
    let dir = tempfile::tempdir().unwrap();
    let (db, person, _) = fixture(&dir.path().join("db"));
    let walk = enumerate(&db, person, 8192);
    let per_row = walk.pool_accesses as f64 / ROWS as f64;
    // A leaf of this fixture holds tens of rows, and crossing into one costs
    // a few page opens through the parent path. One access per row would be
    // 1.0; the bound catches that a mile off while leaving the leaf-crossing
    // cost room to breathe.
    assert!(
        per_row <= 0.35,
        "a key-only walk of {} rows made {} buffer-pool accesses ({per_row:.4} per row); \
         the bound is 0.35 -- the walk pins a LEAF, and a leaf is tens of rows",
        walk.ids.len(),
        walk.pool_accesses
    );
}

#[test]
fn a_key_only_enumeration_returns_its_own_collections_ids() {
    let dir = tempfile::tempdir().unwrap();
    let (db, person, decoy) = fixture(&dir.path().join("db"));
    let walk = enumerate(&db, person, 8192);
    assert_eq!(
        walk.collections,
        vec![person.0],
        "every returned id must belong to the queried collection"
    );
    assert_eq!(
        walk.ids,
        (1..=ROWS).collect::<Vec<_>>(),
        "the walk must return every sequence once, in order"
    );
    let other = enumerate(&db, decoy, 8192);
    assert_eq!(other.collections, vec![decoy.0]);
    assert_eq!(other.ids, (1..=ROWS).collect::<Vec<_>>());
}

#[test]
fn a_key_only_row_is_charged_thirteen_bytes_of_output() {
    let dir = tempfile::tempdir().unwrap();
    let (db, person, _) = fixture(&dir.path().join("db"));
    for page_size in [1, 7, 1024, 8192] {
        let walk = enumerate(&db, person, page_size);
        assert_eq!(walk.ids.len(), ROWS as usize);
        assert_eq!(
            walk.output_bytes,
            ROWS * 13,
            "a key-only row is 12 bytes of identity plus the one byte every row \
             is charged, whatever the page size (here {page_size})"
        );
    }
}

/// Not an assertion -- the numbers the bounds above were set from, printed so
/// a run can be read rather than inferred. `cargo test --release --test
/// scan_row_cost -- --nocapture counted_numbers`.
#[test]
fn counted_numbers() {
    let dir = tempfile::tempdir().unwrap();
    let (db, person, _) = fixture(&dir.path().join("db"));
    for page_size in [1024, 8192] {
        let walk = enumerate(&db, person, page_size);
        println!(
            "page_size={page_size:<5} rows={} pages={} allocations={} ({:.5}/row) \
             pool_accesses={} ({:.5}/row) output_bytes={} ({}/row)",
            walk.ids.len(),
            walk.pages,
            walk.allocations,
            walk.allocations as f64 / ROWS as f64,
            walk.pool_accesses,
            walk.pool_accesses as f64 / ROWS as f64,
            walk.output_bytes,
            walk.output_bytes / ROWS,
        );
    }
}
