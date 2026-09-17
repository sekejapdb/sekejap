//! What one page of a query is allowed to COST, measured rather than timed.
//!
//! The four properties below are the ones the loop-3 root-cause note found
//! broken (`.insert-loop/loop3/ROOTCAUSE-query-per-row-constant.md`), each
//! stated as work the engine reports about itself -- candidates examined,
//! primary rows read, buffer-pool accesses, heap allocations -- so a
//! regression names the cause instead of showing up as a slower stopwatch on
//! somebody else's machine.
//!
//!   1. A LIMIT is a stop condition, not a filter. When the driver already
//!      walks in the order the query ranks by, ten rows must cost ten
//!      candidates, not N.
//!   2. Page k+1 resumes where page k stopped. A three-page scan examines
//!      about N candidates in total, not 3N.
//!   3. An equality posting IS the proof of its own predicate, and a key-only
//!      answer needs no row. Retrieval by equality must read zero primary
//!      rows, and its buffer-pool cost per matched row must not grow with the
//!      size of the collection.
//!   4. Walking candidates must not allocate per row.
//!
//! Every bound here is a counted number with the measurement that produced it
//! written into the assertion message, so a future change can see what it
//! moved.
use e4_prototype::{
    collections::{
        CandidateDriver, CollectionId, CollectionOptions, Database, IndexId, Projection,
        QueryBudget, QueryFilter, QueryOrder, QueryRequest, ScalarFilter, ScalarValue, SortDirection,
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
    static BYTES: Cell<usize> = const { Cell::new(0) };
    static TRACK: Cell<bool> = const { Cell::new(false) };
}

struct Alloc;
unsafe impl GlobalAlloc for Alloc {
    unsafe fn alloc(&self, l: AllocationLayout) -> *mut u8 {
        TRACK
            .try_with(|t| {
                if t.get() {
                    COUNT.with(|n| n.set(n.get() + 1));
                    BYTES.with(|n| n.set(n.get() + l.size()));
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
                    BYTES.with(|b| b.set(b.get() + n));
                }
            })
            .ok();
        System.realloc(p, l, n)
    }
}

#[global_allocator]
static ALLOC: Alloc = Alloc;

fn measured<T>(f: impl FnOnce() -> T) -> (T, usize) {
    COUNT.with(|c| c.set(0));
    BYTES.with(|b| b.set(0));
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

const CATS: [&str; 8] = [
    "cafe", "bakery", "florist", "grocer", "hatter", "ironmonger", "jeweller", "lamplighter",
];

struct Fixture {
    db: Database,
    v: CollectionId,
    cat: IndexId,
    rating: IndexId,
}

/// `rows` entities whose sequence is their insertion index, one equality
/// index over a column that cycles every 8 rows (so a `cafe` lookup matches
/// exactly one eighth of the collection) and one ordered index over a real.
fn fixture(path: &Path, rows: u64) -> Fixture {
    let mut db = Database::create(path, cfg()).unwrap();
    let v = db
        .create_collection(
            "v",
            vec![("cat".into(), Kind::Text), ("rating".into(), Kind::Real)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();
    for i in 1..=rows {
        let id = db
            .put(
                v,
                &format!("k{i:08}"),
                &json!({"cat": CATS[(i % 8) as usize], "rating": (i % 1000) as f64 / 4.0}),
            )
            .unwrap();
        assert_eq!(id.sequence, i, "the fixture rests on sequence == insertion index");
        if i % 512 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    let cat = db.create_scalar_index(v, "cat_idx", "cat", false).unwrap();
    db.build_index_to_ready(cat, 255).unwrap();
    let rating = db.create_scalar_index(v, "rating_idx", "rating", false).unwrap();
    db.build_index_to_ready(rating, 255).unwrap();
    db.commit().unwrap();
    db.checkpoint().unwrap();
    Fixture { db, v, cat, rating }
}

struct PageCost {
    ids: Vec<u64>,
    candidates: u64,
    primary_reads: u64,
    scalar_postings: u64,
}

/// Drain a whole query, summing the work every page reports.
fn drain(
    db: &Database,
    collection: CollectionId,
    filters: &[QueryFilter<'_>],
    order: QueryOrder<'_>,
    projection: Projection<'_>,
    total_limit: Option<usize>,
    driver: CandidateDriver,
    page_size: usize,
) -> PageCost {
    let mut prepared = db
        .prepare_query(QueryRequest {
            collection,
            filters,
            order,
            projection,
            total_limit,
            driver,
        })
        .unwrap();
    let mut cost = PageCost {
        ids: Vec::new(),
        candidates: 0,
        primary_reads: 0,
        scalar_postings: 0,
    };
    loop {
        let page = prepared
            .next_page(page_size, QueryBudget::unlimited(), || false)
            .unwrap();
        cost.candidates += page.work.candidates;
        cost.primary_reads += page.work.primary_reads;
        cost.scalar_postings += page.work.scalar_postings;
        for row in &page.rows {
            cost.ids.push(row.id.sequence);
        }
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    cost
}

/// A limit is a stop condition. With the candidate stream already in the
/// order the query ranks by, ten rows must cost about ten candidates -- both
/// for the bare entity walk and for an equality posting that drives it.
#[test]
fn a_limit_stops_the_candidate_walk_when_the_driver_is_already_in_rank_order() {
    let temp = tempfile::tempdir().unwrap();
    let rows = 5_000u64;
    let Fixture { db, v, cat, .. } = fixture(&temp.path().join("db"), rows);

    let bare = drain(
        &db,
        v,
        &[],
        QueryOrder::EntityId,
        Projection::Ids,
        Some(10),
        CandidateDriver::Entities,
        8192,
    );
    assert_eq!(bare.ids, (1..=10).collect::<Vec<_>>(), "the first ten ids");
    assert!(
        bare.candidates <= 12,
        "entity walk for LIMIT 10 over {rows} rows examined {} candidates; \
         a stop condition bounds it by the limit (<= 12), walking the whole \
         collection does not",
        bare.candidates
    );

    let filters = [QueryFilter::Scalar {
        index: cat,
        predicate: ScalarFilter::Eq(ScalarValue::Text("cafe")),
    }];
    let driven = drain(
        &db,
        v,
        &filters,
        QueryOrder::EntityId,
        Projection::Ids,
        Some(10),
        CandidateDriver::Auto,
        8192,
    );
    // `cafe` is CATS[0], so it is exactly the sequences divisible by 8.
    let expected: Vec<u64> = (1..=10).map(|n| n * 8).collect();
    assert_eq!(driven.ids, expected, "the first ten `cafe` ids");
    assert!(
        driven.candidates <= 12,
        "equality-driven LIMIT 10 over {rows} rows examined {} candidates; \
         the posting range is already in id order, so the limit bounds it \
         (<= 12)",
        driven.candidates
    );
}

/// Page k+1 must resume where page k stopped. Three pages over one
/// collection examine about N candidates in total, not 3N -- and the pages
/// stay disjoint and complete while doing it.
#[test]
fn a_multi_page_scan_walks_the_collection_once_not_once_per_page() {
    let temp = tempfile::tempdir().unwrap();
    let rows = 20_000u64;
    let Fixture { db, v, rating, .. } = fixture(&temp.path().join("db"), rows);

    let scan = drain(
        &db,
        v,
        &[],
        QueryOrder::EntityId,
        Projection::Ids,
        None,
        CandidateDriver::Entities,
        8192,
    );
    assert_eq!(scan.ids, (1..=rows).collect::<Vec<_>>(), "pages are disjoint and complete");
    let pages = 3u64; // 8192 + 8192 + 3616
    assert!(
        scan.candidates <= rows + pages * 4,
        "a {pages}-page key scan of {rows} rows examined {} candidates; \
         one pass is {rows} plus a re-read of each page's last row \
         (<= {}), re-walking from the start per page is {}",
        scan.candidates,
        rows + pages * 4,
        rows * pages
    );

    // The same property against a posting range rather than the primary tree.
    let ordered = drain(
        &db,
        v,
        &[],
        QueryOrder::Scalar {
            index: rating,
            direction: SortDirection::Ascending,
        },
        Projection::Ids,
        None,
        CandidateDriver::Order,
        8192,
    );
    assert_eq!(ordered.ids.len(), rows as usize, "every row is emitted once");
    let mut seen = ordered.ids.clone();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), rows as usize, "pages are disjoint");
    assert!(
        ordered.scalar_postings <= rows + pages * 4,
        "a {pages}-page ordered scan of {rows} rows read {} scalar postings; \
         one pass is {rows} (<= {}), re-walking the posting range per page \
         is {}",
        ordered.scalar_postings,
        rows + pages * 4,
        rows * pages
    );
}

/// An equality posting already proves its own predicate, and a key-only
/// answer needs no row at all. So equality retrieval must read ZERO primary
/// rows, and its buffer-pool cost per matched row must not grow with N.
#[test]
fn equality_retrieval_of_keys_reads_no_primary_rows_and_stays_flat() {
    let temp = tempfile::tempdir().unwrap();
    let mut per_row = Vec::new();
    for rows in [2_000u64, 20_000u64] {
        let Fixture { db, v, cat, .. } = fixture(&temp.path().join(format!("db{rows}")), rows);
        let filters = [QueryFilter::Scalar {
            index: cat,
            predicate: ScalarFilter::Eq(ScalarValue::Text("cafe")),
        }];
        let before = db.pool_accesses().unwrap();
        let cost = drain(
            &db,
            v,
            &filters,
            QueryOrder::EntityId,
            Projection::Ids,
            None,
            CandidateDriver::Auto,
            8192,
        );
        let accesses = db.pool_accesses().unwrap() - before;
        let matched = cost.ids.len() as u64;
        assert_eq!(matched, rows / 8, "one eighth of {rows} rows are `cafe`");
        assert_eq!(
            cost.ids,
            (1..=matched).map(|n| n * 8).collect::<Vec<_>>(),
            "every `cafe` id, in order"
        );
        assert_eq!(
            cost.primary_reads, 0,
            "{rows} rows / {matched} matches: the page read {} primary rows. \
             The driving posting proves the predicate and the projection is \
             Ids, so nothing needs a row; before this fix it was 2 per match \
             (one to re-check the driver's own predicate, one to re-fetch the \
             winner)",
            cost.primary_reads
        );
        assert!(
            accesses <= matched / 4 + 96,
            "{rows} rows / {matched} matches: {accesses} buffer-pool accesses. \
             The posting range's leaf pages plus a small constant is \
             <= {}; two B-tree point-gets per match was 6.1/row at 2K and \
             8.0/row at 20K",
            matched / 4 + 96
        );
        per_row.push((rows, matched, accesses, accesses as f64 / matched as f64));
    }
    let (small, large) = (&per_row[0], &per_row[1]);
    assert!(
        large.3 <= small.3 * 1.15 + 0.05,
        "pool accesses per matched row must not grow with the collection: \
         {:.2}/row at {} rows ({} accesses / {} matches) vs {:.2}/row at {} \
         rows ({} accesses / {} matches). The measured growth before this fix \
         was 6.1 -> 8.0 -> 12.0 across 2K/20K/100K",
        small.3,
        small.0,
        small.2,
        small.1,
        large.3,
        large.0,
        large.2,
        large.1
    );
}

/// The candidate walk must borrow, not allocate. A key-only scan's
/// allocations belong to its RESULT, not to the rows it stepped over.
#[test]
fn a_key_only_scan_does_not_allocate_per_candidate() {
    let temp = tempfile::tempdir().unwrap();
    let rows = 5_000u64;
    let Fixture { db, v, .. } = fixture(&temp.path().join("db"), rows);

    let (cost, allocs) = measured(|| {
        drain(
            &db,
            v,
            &[],
            QueryOrder::EntityId,
            Projection::Ids,
            None,
            CandidateDriver::Entities,
            8192,
        )
    });
    assert_eq!(cost.ids.len(), rows as usize, "the whole collection");
    // The result itself is one `QueryRow` vector grown to `rows`, the test's
    // own id vector, and the page's heap: a few dozen allocations in total.
    assert!(
        allocs <= 512,
        "a key-only scan of {rows} rows made {allocs} allocations. The result \
         needs a few dozen; a key and a value `Vec` per candidate is ~2 per \
         row and the allocating range iterator measured ~8 per row (16,349 \
         for 2,000 rows) before this fix",
    );
}
