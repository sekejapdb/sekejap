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
    price: IndexId,
}

/// `rows` entities whose sequence is their insertion index, one equality
/// index over a column that cycles every 8 rows (so a `cafe` lookup matches
/// exactly one eighth of the collection) and one ordered index over a real.
fn fixture(path: &Path, rows: u64) -> Fixture {
    let mut db = Database::create(path, cfg()).unwrap();
    // Both index layouts have to answer these questions at the same cost. A
    // version-2 scalar index walks its OWN tree and a version-1 one walks the
    // primary tree; the cursors here -- forward and reverse -- reach them
    // through the same two entry points. `E4_INDEX_TREES=0` runs the whole
    // file against the version-1 layout.
    if std::env::var("E4_INDEX_TREES").is_ok_and(|mode| mode == "0") {
        db.set_create_index_trees(false);
    }
    let v = db
        .create_collection(
            "v",
            vec![
                ("cat".into(), Kind::Text),
                ("rating".into(), Kind::Real),
                ("price".into(), Kind::Real),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();
    for i in 1..=rows {
        let id = db
            .put(
                v,
                &format!("k{i:08}"),
                &json!({
                    "cat": CATS[(i % 8) as usize],
                    "rating": (i % 1000) as f64 / 4.0,
                    "price": (i % 617) as f64,
                }),
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
    let price = db.create_scalar_index(v, "price_idx", "price", false).unwrap();
    db.build_index_to_ready(price, 255).unwrap();
    db.commit().unwrap();
    db.checkpoint().unwrap();
    Fixture {
        db,
        v,
        cat,
        rating,
        price,
    }
}

struct PageCost {
    ids: Vec<u64>,
    projected: usize,
    candidates: u64,
    primary_reads: u64,
    scalar_postings: u64,
    row_decodes: u64,
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
        projected: 0,
        candidates: 0,
        primary_reads: 0,
        scalar_postings: 0,
        row_decodes: 0,
    };
    loop {
        let page = prepared
            .next_page(page_size, QueryBudget::unlimited(), || false)
            .unwrap();
        cost.candidates += page.work.candidates;
        cost.primary_reads += page.work.primary_reads;
        cost.scalar_postings += page.work.scalar_postings;
        cost.row_decodes += page.work.row_decodes;
        for row in &page.rows {
            cost.ids.push(row.id.sequence);
            cost.projected += row.projected.len();
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

// ── loop-5 residuals: the four costs item Q left standing ──────────────────

/// The rank order a DESC scalar query asks for: value descending, entity id
/// ascending inside a tie. Computed from the fixture's own generator so the
/// assertion names the answer rather than trusting the engine for it.
fn descending_rating_order(rows: u64) -> Vec<u64> {
    let mut all: Vec<(u64, u64)> = (1..=rows).map(|i| (i % 1000, i)).collect();
    all.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
    all.into_iter().map(|(_, id)| id).collect()
}

/// A DESCENDING scalar order is as much a walk order as an ascending one: the
/// same posting range read backwards. So the same two properties must hold --
/// a LIMIT stops the walk, and page k+1 resumes where page k stopped.
#[test]
fn a_descending_scalar_order_stops_at_its_limit_and_pages_by_continuation() {
    let temp = tempfile::tempdir().unwrap();
    let rows = 5_000u64;
    let Fixture { db, v, rating, .. } = fixture(&temp.path().join("db"), rows);
    let expected = descending_rating_order(rows);

    let top = drain(
        &db,
        v,
        &[],
        QueryOrder::Scalar {
            index: rating,
            direction: SortDirection::Descending,
        },
        Projection::Ids,
        Some(50),
        CandidateDriver::Order,
        8192,
    );
    assert_eq!(top.ids, expected[..50], "the fifty highest-rated ids");
    assert!(
        top.candidates <= 56,
        "DESC + LIMIT 50 over {rows} rows examined {} candidates; a reverse \
         posting cursor walking in rank order stops at the limit (<= 56), \
         collecting the whole index and sorting it is {rows}",
        top.candidates
    );

    // Paged, small pages, so continuation is exercised rather than a single
    // page that happens to cover everything.
    let pages = 5u64;
    let walk = drain(
        &db,
        v,
        &[],
        QueryOrder::Scalar {
            index: rating,
            direction: SortDirection::Descending,
        },
        Projection::Ids,
        None,
        CandidateDriver::Order,
        1_000,
    );
    assert_eq!(walk.ids, expected, "pages are disjoint, complete and in order");
    assert!(
        walk.scalar_postings <= rows + pages * 16,
        "a {pages}-page DESC scan of {rows} rows read {} scalar postings; one \
         reverse pass plus a re-walk of each page's boundary tie group is \
         {rows} + a little (<= {}), re-walking the posting range per page is \
         {}",
        walk.scalar_postings,
        rows + pages * 16,
        rows * pages
    );
}

/// An equality filter plus an order on a DIFFERENT ready index: walking the
/// ORDER index puts the walk in rank order, so the LIMIT stops it, and the
/// equality filter is answered from its own posting as a membership test.
/// Nothing reads a row to find out what the order field says.
#[test]
fn an_equality_filter_ordered_by_another_index_walks_the_order_index() {
    let temp = tempfile::tempdir().unwrap();
    let rows = 5_000u64;
    let Fixture { db, v, cat, rating, .. } = fixture(&temp.path().join("db"), rows);
    // `cafe` is CATS[0]: exactly the sequences divisible by 8.
    let matches = rows / 8;
    let expected: Vec<u64> = descending_rating_order(rows)
        .into_iter()
        .filter(|id| id % 8 == 0)
        .take(10)
        .collect();

    let filters = [QueryFilter::Scalar {
        index: cat,
        predicate: ScalarFilter::Eq(ScalarValue::Text("cafe")),
    }];
    let cost = drain(
        &db,
        v,
        &filters,
        QueryOrder::Scalar {
            index: rating,
            direction: SortDirection::Descending,
        },
        Projection::Ids,
        Some(10),
        CandidateDriver::Auto,
        8192,
    );
    assert_eq!(cost.ids, expected, "the ten highest-rated `cafe` ids");
    assert!(
        cost.candidates <= 256,
        "`cat = 'cafe' ORDER BY rating DESC LIMIT 10` over {rows} rows \
         ({matches} matches) examined {} candidates. One in eight rows is \
         accepted, so ten accepted rows are about eighty walked (<= 256); \
         driving from the equality posting examines every one of the \
         {matches} matches and sorts them",
        cost.candidates
    );
    assert!(
        cost.primary_reads == 0,
        "the same query read {} primary rows. The order key comes out of the \
         posting the walk is already on and the equality filter is answered \
         from -- and proved by -- its own posting, so nothing needs a row at \
         all; reading the order field out of each match was {matches}",
        cost.primary_reads
    );
    assert_eq!(
        cost.row_decodes, 0,
        "the same query walked {} dense-v3 rows; neither the order key nor \
         the membership test needs one",
        cost.row_decodes
    );
}

/// A non-driving RANGE filter on a second index cannot be answered from a
/// posting: the index is keyed value-first, and the candidate's value is
/// exactly what is unknown, so there is no key to probe. It is answered from
/// the row -- but each candidate's row must be read ONCE and walked ONCE, and
/// a winner must not pay for it a second time.
#[test]
fn a_second_range_filter_reads_and_decodes_each_candidate_once() {
    let temp = tempfile::tempdir().unwrap();
    let rows = 5_000u64;
    let Fixture { db, v, rating, price, .. } = fixture(&temp.path().join("db"), rows);

    let filters = [
        QueryFilter::Scalar {
            index: price,
            predicate: ScalarFilter::Range {
                lower: std::ops::Bound::Included(ScalarValue::F64(100.0)),
                upper: std::ops::Bound::Unbounded,
            },
        },
        QueryFilter::Scalar {
            index: rating,
            predicate: ScalarFilter::Range {
                lower: std::ops::Bound::Unbounded,
                upper: std::ops::Bound::Included(ScalarValue::F64(125.0)),
            },
        },
    ];
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
    let expected: Vec<u64> = (1..=rows)
        .filter(|i| (i % 617) as f64 >= 100.0 && (i % 1000) as f64 / 4.0 <= 125.0)
        .collect();
    assert_eq!(cost.ids, expected, "both ranges, intersected");
    let winners = cost.ids.len() as u64;
    assert!(
        cost.primary_reads <= cost.candidates + 4,
        "two ranges over {rows} rows examined {} candidates and read {} \
         primary rows. The second range needs one row per candidate (<= {}); \
         re-fetching each of the {winners} winners afterwards costs that again",
        cost.candidates,
        cost.primary_reads,
        cost.candidates + 4,
    );
    assert!(
        cost.row_decodes <= cost.candidates + 4,
        "the same query walked {} dense-v3 rows for {} candidates; one field \
         from one walk per candidate is <= {}",
        cost.row_decodes,
        cost.candidates,
        cost.candidates + 4,
    );
}

/// Projection. The entity cursor has already read the row out of the primary
/// tree, so a projected scan must not go back for it; and however many columns
/// are asked for, the row is one dense-v3 record and must be walked once.
#[test]
fn a_projected_scan_reuses_the_walked_row_and_decodes_it_once() {
    let temp = tempfile::tempdir().unwrap();
    let rows = 5_000u64;
    let Fixture { db, v, .. } = fixture(&temp.path().join("db"), rows);

    let (one, allocs) = measured(|| {
        drain(
            &db,
            v,
            &[],
            QueryOrder::EntityId,
            Projection::Fields(&["rating"]),
            None,
            CandidateDriver::Entities,
            8192,
        )
    });
    assert_eq!(one.ids.len(), rows as usize, "the whole collection");
    assert_eq!(one.projected, rows as usize, "one column per row");
    assert!(
        one.primary_reads <= rows + 4,
        "projecting one column over {rows} rows read {} primary rows. The \
         entity cursor already read every one of them (<= {}); fetching each \
         winner again is {}",
        one.primary_reads,
        rows + 4,
        rows * 2
    );
    assert!(
        one.row_decodes <= rows + 4,
        "projecting one column over {rows} rows walked {} dense-v3 rows; one \
         walk materialising one field is <= {}",
        one.row_decodes,
        rows + 4
    );
    assert!(
        allocs <= rows as usize * 8 + 512,
        "projecting one column over {rows} rows made {allocs} allocations. \
         The result is one name and one value per row; the bound is \
         {} (eight per emitted row plus the page's fixed cost)",
        rows as usize * 8 + 512
    );

    let five = drain(
        &db,
        v,
        &[],
        QueryOrder::EntityId,
        Projection::Fields(&["cat", "rating", "price"]),
        None,
        CandidateDriver::Entities,
        8192,
    );
    assert_eq!(five.projected, rows as usize * 3, "three columns per row");
    assert!(
        five.row_decodes <= rows + 4,
        "projecting three columns over {rows} rows walked {} dense-v3 rows; \
         the row is ONE record and three fields come out of one walk (<= {}), \
         one walk per projected field is {}",
        five.row_decodes,
        rows + 4,
        rows * 3
    );
}
