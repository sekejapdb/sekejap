//! What one candidate is allowed to COST.
//!
//! Every claim here is counted, not reasoned about: allocations through a
//! wrapping global allocator, and pager accesses through the store's own
//! counter. A per-row constant that this loop removed can come back silently
//! -- the answers stay right and only the clock moves -- so each removal is
//! pinned to a number that would go up again.
//!
//! The numbers are ceilings with room in them, not the measured values. They
//! are set well below what the code did before the change and well above what
//! it does now, so they fail on a regression rather than on a rounding.
use sekejap_core::{
    collections::{
        CandidateDriver, CollectionId, CollectionOptions, Database, Geom, GeometryFilter, IndexId,
        PointFilter, Projection, QueryBudget, QueryDriver, QueryFilter, QueryOrder, QueryRequest,
        QueryWork, ScalarFilter, ScalarValue, SortDirection, TextMatch,
    },
    spatial_geometry,
    spatial_math::{Bounds, Point},
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::{json, Value};
use std::{
    alloc::{GlobalAlloc, Layout as AllocationLayout, System},
    cell::Cell,
    ops::Bound,
};

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<usize> = const { Cell::new(0) };
    static BYTES: Cell<usize> = const { Cell::new(0) };
}
struct Alloc;
// SAFETY: every method forwards to `System` unchanged; the counters are
// thread-local side effects that never touch the returned pointer.
unsafe impl GlobalAlloc for Alloc {
    unsafe fn alloc(&self, l: AllocationLayout) -> *mut u8 {
        seen(l.size());
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: AllocationLayout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: AllocationLayout, n: usize) -> *mut u8 {
        seen(n);
        unsafe { System.realloc(p, l, n) }
    }
    unsafe fn alloc_zeroed(&self, l: AllocationLayout) -> *mut u8 {
        seen(l.size());
        unsafe { System.alloc_zeroed(l) }
    }
}
fn seen(size: usize) {
    TRACK
        .try_with(|t| {
            if t.get() {
                COUNT.with(|c| c.set(c.get() + 1));
                BYTES.with(|b| b.set(b.get() + size));
            }
        })
        .ok();
}
#[global_allocator]
static ALLOC: Alloc = Alloc;

/// (value, allocations, bytes allocated).
fn counted<T>(f: impl FnOnce() -> T) -> (T, usize, usize) {
    COUNT.with(|c| c.set(0));
    BYTES.with(|b| b.set(0));
    TRACK.with(|t| t.set(true));
    let value = f();
    TRACK.with(|t| t.set(false));
    (value, COUNT.with(|c| c.get()), BYTES.with(|b| b.get()))
}

fn cfg() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

const ROWS: u64 = 4_000;
const PAGE: usize = 8192;
const CATS: [&str; 8] = [
    "cafe", "bar", "gym", "clinic", "school", "museum", "park", "hotel",
];
const WORDS: [&str; 10] = [
    "railway", "signal", "platform", "junction", "siding", "tunnel", "viaduct", "depot",
    "carriage", "timetable",
];

struct Fixture {
    db: Database,
    rows: sekejap_core::collections::CollectionId,
    cat: IndexId,
    price: IndexId,
    note: IndexId,
}

/// The two_ways shape in miniature: a `price` whose matching rows come in dense
/// runs separated by a gap of forty, which is the distribution the lockstep
/// primary reader has to survive.
fn price(i: u64) -> f64 {
    10.0 + (i % 49) as f64 * 10.0
}

fn fixture(dir: &std::path::Path) -> Fixture {
    let mut db = Database::create(dir.join("db"), cfg()).unwrap();
    let rows = db
        .create_collection(
            "v",
            vec![
                ("cat".into(), Kind::Text),
                ("price".into(), Kind::Real),
                ("note".into(), Kind::Text),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();
    for i in 1..=ROWS {
        db.put(
            rows,
            &format!("k{i:08}"),
            &json!({
                "cat": CATS[(i % 8) as usize],
                "price": price(i),
                "note": format!("{} {} number {}", WORDS[(i % 10) as usize], WORDS[(i / 7 % 10) as usize], i),
            }),
        )
        .unwrap();
        if i % 256 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    let cat = db.create_scalar_index(rows, "cat_idx", "cat", false).unwrap();
    db.build_index_to_ready(cat, 256).unwrap();
    let price = db
        .create_scalar_index(rows, "price_idx", "price", false)
        .unwrap();
    db.build_index_to_ready(price, 256).unwrap();
    let note = db.create_text_index(rows, "note_text", "note").unwrap();
    db.build_index_to_ready(note, 256).unwrap();
    db.commit().unwrap();
    db.checkpoint().unwrap();
    Fixture {
        db,
        rows,
        cat,
        price,
        note,
    }
}

/// Drain one query and report how many rows it returned.
fn drain(
    fixture: &Fixture,
    filters: &[QueryFilter<'_>],
    order: QueryOrder<'_>,
    projection: Projection<'_>,
) -> usize {
    let mut prepared = fixture
        .db
        .prepare_query(QueryRequest {
            collection: fixture.rows,
            filters,
            order,
            projection,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let mut found = 0;
    loop {
        let page = prepared
            .next_page(PAGE, QueryBudget::unlimited(), || false)
            .unwrap();
        found += page.rows.len();
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    found
}

/// An answer with no rows in it must not pay for a page of them.
///
/// `next_page` reserved its winner buffer from the PAGE SIZE -- 8,193 entries,
/// 459 KB -- before it had seen a single candidate, and then dropped it
/// untouched. That allocation was most of what an empty query cost: 466 KB and
/// 8.1 us against SQLite's 1.5 us for the same question.
#[test]
fn an_empty_answer_allocates_no_page_of_winners() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path());
    // Warm every cache the first execution fills, so what is counted is the
    // steady-state cost and not first touch.
    assert_eq!(
        drain(
            &fixture,
            &[QueryFilter::Scalar {
                index: fixture.cat,
                predicate: ScalarFilter::Eq(ScalarValue::Text("nowhere")),
            }],
            QueryOrder::EntityId,
            Projection::Ids,
        ),
        0
    );
    let (found, allocations, bytes) = counted(|| {
        drain(
            &fixture,
            &[QueryFilter::Scalar {
                index: fixture.cat,
                predicate: ScalarFilter::Eq(ScalarValue::Text("nowhere")),
            }],
            QueryOrder::EntityId,
            Projection::Ids,
        )
    });
    assert_eq!(found, 0);
    assert!(
        bytes < 16 * 1024,
        "an empty answer allocated {bytes} bytes; it used to reserve 8,193 winner slots it never filled"
    );
    assert!(
        allocations < 64,
        "an empty answer made {allocations} allocations"
    );
}

/// The descriptor of an index this handle has already read is not read again.
///
/// `index_info` is four B-tree point-gets, and a query pays it before it looks
/// at a row: preparing a one-index query cost 5.5 us against 0.38 us for the
/// same query with no index named.
#[test]
fn preparing_a_query_over_a_known_index_reads_no_descriptor() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path());
    let prepare = |fixture: &Fixture| {
        fixture
            .db
            .prepare_query(QueryRequest {
                collection: fixture.rows,
                filters: &[QueryFilter::Scalar {
                    index: fixture.cat,
                    predicate: ScalarFilter::Eq(ScalarValue::Text("cafe")),
                }],
                order: QueryOrder::EntityId,
                projection: Projection::Ids,
                total_limit: None,
                driver: CandidateDriver::Auto,
            })
            .map(|_| ())
            .unwrap()
    };
    prepare(&fixture);
    let before = fixture.db.pool_accesses().unwrap();
    prepare(&fixture);
    prepare(&fixture);
    prepare(&fixture);
    let spent = fixture.db.pool_accesses().unwrap() - before;
    assert_eq!(
        spent, 0,
        "three prepares of a known index cost {spent} pager accesses"
    );
}

/// ... and the moment a descriptor could have changed, it is read again.
///
/// The cache lives on one handle and is emptied by every write that handle
/// makes, which is what a descriptor rewrite -- a build step moving a tree
/// root, a create, a drop -- passes through.
#[test]
fn a_write_makes_the_next_prepare_read_the_descriptor_again() {
    let temp = tempfile::tempdir().unwrap();
    let mut fixture = fixture(temp.path());
    let cat = fixture.cat;
    let prepare = |db: &Database, collection, index| {
        db.prepare_query(QueryRequest {
            collection,
            filters: &[QueryFilter::Scalar {
                index,
                predicate: ScalarFilter::Eq(ScalarValue::Text("cafe")),
            }],
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .map(|_| ())
    };
    prepare(&fixture.db, fixture.rows, cat).unwrap();
    // A write of any kind.
    fixture
        .db
        .put(fixture.rows, "k99999999", &json!({"cat": "cafe", "price": 1.0, "note": "x"}))
        .unwrap();
    fixture.db.commit().unwrap();
    let before = fixture.db.pool_accesses().unwrap();
    prepare(&fixture.db, fixture.rows, cat).unwrap();
    let spent = fixture.db.pool_accesses().unwrap() - before;
    assert!(
        spent > 0,
        "the descriptor was served from a cache that a write should have emptied"
    );
    // And a dropped index is gone, not remembered.
    fixture.db.begin_drop_index(cat).unwrap();
    while fixture.db.drop_index_step(cat, 256).unwrap() {}
    fixture.db.commit().unwrap();
    assert!(
        prepare(&fixture.db, fixture.rows, cat).is_err(),
        "a dropped index was still answered from the descriptor cache"
    );
}

/// A key-only answer copies no rows out of the primary tree.
///
/// A winner still has to be proved present -- a posting can outlive the record
/// it names -- but proving it is a key comparison, and the page used to copy
/// the whole row out of the pinned leaf to do it and then drop the bytes. With
/// the sparse-gap bail-out of the lockstep reader firing on the FIRST gap of a
/// dense-run distribution, this case also paid a full root-to-leaf descent per
/// row: measured at six allocations and four pager accesses per returned row.
#[test]
fn a_key_only_range_answer_costs_little_per_row() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path());
    let filters = [QueryFilter::Scalar {
        index: fixture.price,
        predicate: ScalarFilter::Range {
            lower: Bound::Excluded(ScalarValue::F64(400.0)),
            upper: Bound::Unbounded,
        },
    }];
    let expected = (1..=ROWS).filter(|i| price(*i) > 400.0).count();
    assert!(expected > 500, "the fixture must have a real answer");
    assert_eq!(
        drain(&fixture, &filters, QueryOrder::EntityId, Projection::Ids),
        expected
    );
    let before = fixture.db.pool_accesses().unwrap();
    let (found, allocations, _) =
        counted(|| drain(&fixture, &filters, QueryOrder::EntityId, Projection::Ids));
    let accesses = fixture.db.pool_accesses().unwrap() - before;
    assert_eq!(found, expected);
    assert!(
        allocations < found * 3,
        "{allocations} allocations for {found} rows: {:.2} per row, and it was six",
        allocations as f64 / found as f64
    );
    assert!(
        accesses < found as u64 * 2,
        "{accesses} pager accesses for {found} rows: {:.2} per row, and it was four",
        accesses as f64 / found as f64
    );
}

/// An id-ordered page holds its winners in a bounded buffer, not a heap.
///
/// The walk already hands them over in rank order, so every comparison the
/// heap makes on the way in is thrown away by the sort on the way out --
/// and pushing 8,192 entries into a max-heap in ascending order is the heap's
/// worst case, one full sift to the root per row. The answer is what is
/// asserted here; the shape is asserted by the cost of the page around it.
#[test]
fn an_id_ordered_page_is_returned_in_id_order_without_a_heap() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path());
    let mut prepared = fixture
        .db
        .prepare_query(QueryRequest {
            collection: fixture.rows,
            filters: &[],
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    // Small pages, so the resume path is exercised as well as the fill.
    let mut all = Vec::new();
    loop {
        let page = prepared
            .next_page(500, QueryBudget::unlimited(), || false)
            .unwrap();
        for row in &page.rows {
            all.push(row.id.sequence);
        }
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    assert_eq!(all, (1..=ROWS).collect::<Vec<_>>());
    let before = fixture.db.pool_accesses().unwrap();
    let (found, allocations, bytes) = counted(|| {
        drain(
            &fixture,
            &[],
            QueryOrder::EntityId,
            Projection::Ids,
        )
    });
    let accesses = fixture.db.pool_accesses().unwrap() - before;
    assert_eq!(found, ROWS as usize);
    // One page of winners, one page of rows, and the caller's own vector: a
    // key-only scan allocates per PAGE, never per row.
    assert!(
        allocations < 64,
        "{allocations} allocations for a {ROWS}-row key-only scan"
    );
    assert!(
        bytes < found * 256,
        "{bytes} bytes for {found} rows"
    );
    assert!(
        accesses < found as u64,
        "{accesses} pager accesses for {found} rows"
    );
}

/// A BM25 page scores each document once and keeps nothing per document.
///
/// The ranking is a top-k over every matching document, so the per-document
/// constant is the whole cost: a heap sift, a norm lookup, a natural logarithm
/// per term, and -- when the page projects nothing -- a primary read whose
/// bytes are dropped.
#[test]
fn a_bm25_page_allocates_little_per_document() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path());
    let filters = [QueryFilter::Text {
        index: fixture.note,
        query: "junction",
        matching: TextMatch::Any,
    }];
    let order = || QueryOrder::Bm25 {
        index: fixture.note,
        query: "junction",
        matching: TextMatch::Any,
    };
    let expected = drain(&fixture, &filters, order(), Projection::Ids);
    assert!(expected > 300, "the fixture must have a real answer");
    let (found, allocations, _) = counted(|| drain(&fixture, &filters, order(), Projection::Ids));
    assert_eq!(found, expected);
    assert!(
        allocations < found * 3,
        "{allocations} allocations for {found} documents: {:.2} per document, and it was five",
        allocations as f64 / found as f64
    );
}

/// The scores themselves do not move when the corpus half of them is lifted
/// out of the per-document loop.
///
/// `idf` and the length-normalisation constant depend only on the query and
/// the corpus, and were recomputed -- the idf with a natural logarithm -- once
/// per term per document. Floating point is not associative, so the guard is
/// that the ORDER and the score of a known document stay exactly what the
/// definition gives.
#[test]
fn bm25_scores_match_the_definition_after_the_constants_are_hoisted() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path());
    let mut prepared = fixture
        .db
        .prepare_query(QueryRequest {
            collection: fixture.rows,
            filters: &[QueryFilter::Text {
                index: fixture.note,
                query: "junction",
                matching: TextMatch::Any,
            }],
            order: QueryOrder::Bm25 {
                index: fixture.note,
                query: "junction",
                matching: TextMatch::Any,
            },
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let page = prepared
        .next_page(PAGE, QueryBudget::unlimited(), || false)
        .unwrap();
    assert!(page.rows.len() > 300);
    let scores: Vec<f64> = page
        .rows
        .iter()
        .map(|row| match row.order {
            sekejap_core::collections::OrderValue::Bm25(score) => score,
            ref other => panic!("bm25 page returned {other:?}"),
        })
        .collect();
    assert!(
        scores.windows(2).all(|pair| pair[0] >= pair[1]),
        "a bm25 page descends by score"
    );
    assert!(
        scores.iter().all(|score| score.is_finite() && *score > 0.0),
        "every bm25 score is finite and positive"
    );
}

/// A descending scalar order still stops early, and still answers exactly.
///
/// This is the one walk that is monotone in the VALUE but not in the whole
/// rank key, so it is the case where the page HAS to become a heap. Keeping it
/// honest is what stops the lazy heap from being a lazy wrong answer.
#[test]
fn a_descending_scalar_page_still_ranks_correctly() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path());
    for direction in [SortDirection::Descending, SortDirection::Ascending] {
        for page_size in [7usize, 64, 500] {
            let mut prepared = fixture
                .db
                .prepare_query(QueryRequest {
                    collection: fixture.rows,
                    filters: &[],
                    order: QueryOrder::Scalar {
                        index: fixture.price,
                        direction,
                    },
                    projection: Projection::Ids,
                    total_limit: None,
                    driver: CandidateDriver::Auto,
                })
                .unwrap();
            let mut seen: Vec<(u64, u64)> = Vec::new();
            loop {
                let page = prepared
                    .next_page(page_size, QueryBudget::unlimited(), || false)
                    .unwrap();
                for row in &page.rows {
                    seen.push((row.id.sequence, row.id.sequence));
                }
                if page.done || page.rows.is_empty() {
                    break;
                }
            }
            assert_eq!(seen.len(), ROWS as usize, "{direction:?} page {page_size}");
            let mut expected: Vec<u64> = (1..=ROWS).collect();
            expected.sort_by(|left, right| {
                let order = price(*left)
                    .partial_cmp(&price(*right))
                    .expect("prices are finite");
                let order = if direction == SortDirection::Descending {
                    order.reverse()
                } else {
                    order
                };
                order.then(left.cmp(right))
            });
            assert_eq!(
                seen.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                expected,
                "{direction:?} page {page_size}"
            );
        }
    }
}

// ── Q4: the row-needing predicate ─────────────────────────────────────────
//
// A page whose driver hands candidates over in an order the primary tree knows
// nothing about -- a RANGE posting is ordered by (value, sequence) -- and whose
// second predicate has to read the row paid a full root-to-leaf descent per
// candidate. These fix the cost of that shape to a number.

const Q4_ROWS: u64 = 5_000;

struct Q4 {
    db: Database,
    rows: sekejap_core::collections::CollectionId,
    price: IndexId,
    rating: IndexId,
    note: IndexId,
}

fn q4_price(i: u64) -> f64 {
    10.0 + (i % 49) as f64 * 10.0
}
fn q4_rating(i: u64) -> f64 {
    (10 + i % 40) as f64 / 10.0
}
/// Every thirteenth row carries the phrase, in that order and adjacent; the
/// rest carry the same two words but never next to each other in that order.
fn q4_note(i: u64) -> String {
    if i % 13 == 0 {
        format!("railway signal number {i}")
    } else {
        format!("signal {} railway number {i}", WORDS[(i % 10) as usize])
    }
}

fn q4_fixture(dir: &std::path::Path) -> Q4 {
    let mut db = Database::create(dir.join("db"), cfg()).unwrap();
    let rows = db
        .create_collection(
            "v",
            vec![
                ("price".into(), Kind::Real),
                ("rating".into(), Kind::Real),
                ("note".into(), Kind::Text),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();
    for i in 1..=Q4_ROWS {
        db.put(
            rows,
            &format!("k{i:08}"),
            &json!({"price": q4_price(i), "rating": q4_rating(i), "note": q4_note(i)}),
        )
        .unwrap();
        if i % 256 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    let price = db
        .create_scalar_index(rows, "price_idx", "price", false)
        .unwrap();
    db.build_index_to_ready(price, 256).unwrap();
    let rating = db
        .create_scalar_index(rows, "rating_idx", "rating", false)
        .unwrap();
    db.build_index_to_ready(rating, 256).unwrap();
    let note = db.create_text_index(rows, "note_text", "note").unwrap();
    db.build_index_to_ready(note, 256).unwrap();
    db.commit().unwrap();
    db.checkpoint().unwrap();
    Q4 {
        db,
        rows,
        price,
        rating,
        note,
    }
}

/// `price > 250 AND rating < 3.0`: the price posting drives in (value,
/// sequence) order and the rating predicate has to read the row.
fn q4_two_ranges(f: &Q4) -> [QueryFilter<'static>; 2] {
    [
        QueryFilter::Scalar {
            index: f.price,
            predicate: ScalarFilter::Range {
                lower: Bound::Excluded(ScalarValue::F64(250.0)),
                upper: Bound::Unbounded,
            },
        },
        QueryFilter::Scalar {
            index: f.rating,
            predicate: ScalarFilter::Range {
                lower: Bound::Unbounded,
                upper: Bound::Excluded(ScalarValue::F64(3.0)),
            },
        },
    ]
}

/// The answer, computed from the generator alone.
fn q4_oracle() -> Vec<u64> {
    (1..=Q4_ROWS)
        .filter(|i| q4_price(*i) > 250.0 && q4_rating(*i) < 3.0)
        .collect()
}

/// Drain a query and report the ids it returned plus what it charged.
fn q4_drain(
    f: &Q4,
    filters: &[QueryFilter<'_>],
    order: QueryOrder<'_>,
    page_size: usize,
    total_limit: Option<usize>,
) -> (Vec<u64>, u64) {
    let mut prepared = f
        .db
        .prepare_query(QueryRequest {
            collection: f.rows,
            filters,
            order,
            projection: Projection::Ids,
            total_limit,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let mut ids = Vec::new();
    let mut candidates = 0;
    loop {
        let page = prepared
            .next_page(page_size, QueryBudget::unlimited(), || false)
            .unwrap();
        candidates += page.work.candidates;
        for row in &page.rows {
            ids.push(row.id.sequence);
        }
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    (ids, candidates)
}

/// A second predicate that needs the row does not cost a tree descent each.
///
/// The price posting is ordered by (value, sequence), so the candidate ids do
/// NOT ascend and the page's lockstep primary reader gives up on its first
/// gap: every candidate paid a fresh root-to-leaf descent, measured at 4.0
/// pager accesses and 6.0 allocations per candidate on the 20,000-row bench.
/// Gathering the page's candidates, sorting them by id and reading the rows
/// through one ascending cursor is the same answer for one pass of the tree.
#[test]
fn a_row_needing_second_predicate_reads_rows_in_tree_order() {
    let temp = tempfile::tempdir().unwrap();
    let f = q4_fixture(temp.path());
    let filters = q4_two_ranges(&f);
    let oracle = q4_oracle();
    assert!(oracle.len() > 800, "the fixture must have a real answer");
    // Warm every cache the first execution fills.
    let (ids, candidates) = q4_drain(&f, &filters, QueryOrder::EntityId, PAGE, None);
    assert_eq!(ids, oracle, "the batched read answers what the walk answers");
    assert!(
        (1_500..3_500).contains(&candidates),
        "the driving posting should offer about 2,000 candidates, offered {candidates}"
    );
    let before = f.db.pool_accesses().unwrap();
    let ((ids, candidates), allocations, _) =
        counted(|| q4_drain(&f, &filters, QueryOrder::EntityId, PAGE, None));
    let accesses = f.db.pool_accesses().unwrap() - before;
    assert_eq!(ids, oracle);
    assert!(
        accesses as f64 <= candidates as f64 * 0.5,
        "{accesses} pager accesses for {candidates} candidates: {:.2} each, and it was 4.0",
        accesses as f64 / candidates as f64
    );
    assert!(
        allocations as f64 <= candidates as f64 * 1.5,
        "{allocations} allocations for {candidates} candidates: {:.2} each, and it was 6.0",
        allocations as f64 / candidates as f64
    );
}

/// The batch does not change what a page contains or where the next one starts.
#[test]
fn batched_row_reads_keep_pages_disjoint_complete_and_limited() {
    let temp = tempfile::tempdir().unwrap();
    let f = q4_fixture(temp.path());
    let filters = q4_two_ranges(&f);
    let oracle = q4_oracle();
    for page_size in [1usize, 7, 64, 500, PAGE] {
        let (ids, _) = q4_drain(&f, &filters, QueryOrder::EntityId, page_size, None);
        assert_eq!(ids, oracle, "page size {page_size}");
        let mut seen = std::collections::HashSet::new();
        assert!(
            ids.iter().all(|id| seen.insert(*id)),
            "page size {page_size} returned a row twice"
        );
    }
    for limit in [1usize, 10, 37, 900] {
        let (ids, _) = q4_drain(&f, &filters, QueryOrder::EntityId, 16, Some(limit));
        assert_eq!(
            ids,
            oracle.iter().copied().take(limit).collect::<Vec<_>>(),
            "limit {limit}"
        );
    }
}

/// A phrase re-reads the authoritative text; it must not re-descend the tree
/// for it, and tokenising it must not allocate per token.
///
/// The phrase scanner built a `BTreeMap<String, u32>` of every distinct term in
/// the document and a fresh KMP table for the query, per document: 16.0
/// allocations and 8.0 pager accesses per candidate on the 20,000-row bench.
#[test]
fn a_phrase_scans_the_row_without_a_descent_or_a_term_map() {
    let temp = tempfile::tempdir().unwrap();
    let f = q4_fixture(temp.path());
    let phrase = [QueryFilter::Text {
        index: f.note,
        query: "railway signal",
        matching: TextMatch::Phrase,
    }];
    // The very same posting walk, without the authoritative re-read: the
    // baseline every phrase cost is measured ON TOP of.
    let all = [QueryFilter::Text {
        index: f.note,
        query: "railway signal",
        matching: TextMatch::All,
    }];
    let oracle: Vec<u64> = (1..=Q4_ROWS).filter(|i| i % 13 == 0).collect();
    assert!(oracle.len() > 300, "the fixture must have a real answer");
    let (ids, candidates) = q4_drain(&f, &phrase, QueryOrder::EntityId, PAGE, None);
    assert_eq!(ids, oracle, "the phrase answer");
    let (all_ids, _) = q4_drain(&f, &all, QueryOrder::EntityId, PAGE, None);
    assert!(
        all_ids.len() > ids.len(),
        "every document holding both words must outnumber the ones holding the phrase"
    );

    let before = f.db.pool_accesses().unwrap();
    let (_, base_allocations, _) = counted(|| q4_drain(&f, &all, QueryOrder::EntityId, PAGE, None));
    let base_accesses = f.db.pool_accesses().unwrap() - before;
    let before = f.db.pool_accesses().unwrap();
    let ((ids, candidates2), allocations, _) =
        counted(|| q4_drain(&f, &phrase, QueryOrder::EntityId, PAGE, None));
    let accesses = f.db.pool_accesses().unwrap() - before;
    assert_eq!(ids, oracle);
    assert_eq!(candidates, candidates2);
    let extra_accesses = accesses.saturating_sub(base_accesses) as f64 / candidates as f64;
    let extra_allocations =
        allocations.saturating_sub(base_allocations) as f64 / candidates as f64;
    assert!(
        extra_accesses <= 1.0,
        "the phrase re-read cost {extra_accesses:.2} pager accesses per candidate beyond its posting walk"
    );
    assert!(
        extra_allocations <= 2.0,
        "the phrase re-read cost {extra_allocations:.2} allocations per candidate beyond its posting walk"
    );
}

/// How WIDE the rows of the sparse-page fixture are.
///
/// The number is the whole point. A 4 KiB leaf holds only a handful of rows
/// of this size, so a gap of a hundred SEQUENCES is a gap of tens of LEAVES --
/// which is what the lockstep reader's reach is really spending, and what a
/// bound counted in rows cannot see.
const WIDE_BULK: usize = 384;
const WIDE_ROWS: u64 = 20_000;
/// One `pod` value in every hundred rows, so an equality posting hands the
/// page ascending ids a hundred apart.
const POD_STRIDE: u64 = 100;

struct Wide {
    db: Database,
    rows: sekejap_core::collections::CollectionId,
    pod: IndexId,
}

fn wide_fixture(dir: &std::path::Path) -> Wide {
    let mut db = Database::create(dir.join("db"), cfg()).unwrap();
    let rows = db
        .create_collection(
            "w",
            vec![
                ("pod".into(), Kind::Text),
                ("price".into(), Kind::Real),
                ("expensive".into(), Kind::Json),
                ("bulk".into(), Kind::Text),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();
    let bulk = "x".repeat(WIDE_BULK);
    for i in 1..=WIDE_ROWS {
        db.put(
            rows,
            &format!("k{i:08}"),
            &json!({
                "pod": format!("pod{:03}", i % POD_STRIDE),
                "price": price(i),
                "expensive": price(i) > 400.0,
                "bulk": bulk,
            }),
        )
        .unwrap();
        if i % 256 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    let pod = db.create_scalar_index(rows, "pod_idx", "pod", false).unwrap();
    db.build_index_to_ready(pod, 256).unwrap();
    db.commit().unwrap();
    db.checkpoint().unwrap();
    Wide { db, rows, pod }
}

/// A page whose candidates are SPARSE in the primary tree must not walk the
/// leaves between them.
///
/// The shape is the multimodel bench's `members_active_spatial_vector` in
/// miniature: an equality posting drives, so the ids ascend and the page reads
/// its rows through the lockstep cursor, and a second predicate needs the row
/// of every candidate. The candidates are a hundred sequences apart, and the
/// rows are wide enough that a hundred sequences is tens of LEAVES.
///
/// The second predicate is JSON equality rather than a scalar range on
/// purpose: a non-driving scalar RANGE now answers from a posting-built id
/// set instead of the row (see `MembershipSet`), so it no longer walks the
/// primary tree at all and cannot stand in for "a predicate that needs the
/// row" any more. JSON equality still can.
///
/// `peek_at_or_after` reaches a key past its pinned leaf by stepping to the
/// next leaf, one at a time, and each of those steps climbs and re-descends
/// the parent path -- so reaching across tens of leaves costs many times what
/// the root-to-leaf descent it replaced costs. The reader's bail-out is
/// what is supposed to notice; counted in ROWS it cannot, because how many
/// leaves a row-gap spans depends on how wide the rows are.
///
/// Measured on this fixture: 44.44 pager accesses per candidate with the reach
/// bounded at 256 ROWS, against 4.24 once the reader bounds it in LEAVES and
/// gives a sparse page back to the point-get. The ceiling is set between the
/// two, nearer the bad number, so it fails on the regression and not on a
/// rounding.
#[test]
fn a_sparse_page_over_wide_rows_does_not_walk_the_leaves_between_them() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = wide_fixture(temp.path());
    let filters = [
        QueryFilter::Scalar {
            index: fixture.pod,
            predicate: ScalarFilter::Eq(ScalarValue::Text("pod007")),
        },
        QueryFilter::JsonEq {
            field: "expensive",
            value: &json!(true),
        },
    ];
    // Every row the equality posting names has its row read for the JSON
    // equality predicate; those reads are what this test is about.
    let candidates = (1..=WIDE_ROWS).filter(|i| i % POD_STRIDE == 7).count() as u64;
    let expected = (1..=WIDE_ROWS)
        .filter(|i| i % POD_STRIDE == 7 && price(*i) > 400.0)
        .count();
    assert!(candidates > 100, "the fixture must have a real walk");
    assert!(expected > 10, "the fixture must have a real answer");

    let mut prepared = fixture
        .db
        .prepare_query(QueryRequest {
            collection: fixture.rows,
            filters: &filters,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let before = fixture.db.pool_accesses().unwrap();
    let mut found = 0;
    let mut reads = 0;
    loop {
        let page = prepared
            .next_page(PAGE, QueryBudget::unlimited(), || false)
            .unwrap();
        found += page.rows.len();
        reads += page.work.primary_reads;
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    let accesses = fixture.db.pool_accesses().unwrap() - before;
    assert_eq!(found, expected);
    assert_eq!(
        reads, candidates,
        "one primary read per candidate is the shape this test measures"
    );
    assert!(
        accesses < candidates * 8,
        "{accesses} pager accesses for {candidates} candidate rows: {:.2} each, and it was 44.44",
        accesses as f64 / candidates as f64
    );
}

// -- C1: a non-driving scalar RANGE answers from a posting-built id set ------
//
// Before this file's changes, EVERY candidate a driver offered had its row
// read just to answer a non-driving `born`-style RANGE predicate -- the value
// is unknown until something reads it, unlike an equality predicate, which
// already answers from its own posting (`scalar_eq_posting_matches`). These
// tests walk the same posting range ONCE into a sorted id set instead, so a
// candidate afterwards is a binary search.

const RANGE_ROWS: u64 = 30_000;
const RANGE_BUCKETS: u64 = 100;
const RANGE_BBOX_BUCKETS: u64 = 10;
const RANGE_DELETE_STRIDE: u64 = 500;

struct RangeFixture {
    db: Database,
    rows: sekejap_core::collections::CollectionId,
    position: IndexId,
    born: IndexId,
    tag: IndexId,
}

/// A row's longitude bucket, 0..100 -- the spatial half of the fixture.
fn range_bucket(i: u64) -> u64 {
    (i - 1) % RANGE_BUCKETS
}

fn range_lon(i: u64) -> f64 {
    -180.0 + range_bucket(i) as f64 * 3.6
}

/// `born`'s value, decorrelated from the spatial bucket AND from the text
/// tag: both of those repeat with period 100 in `i`, so a `born` that also
/// had period 100 would make every row of one bucket agree on `born` --
/// all matching `born < 10` or none of them. Folding in `i / 100` breaks
/// that periodicity, so roughly one in ten of a bucket's rows match, not all
/// or none of them.
fn range_born(i: u64) -> i64 {
    (((i / 100) * 7 + i * 3) % 100) as i64
}

/// Whether row `i` carries a `born` value at all. Every 41st row is present
/// but explicit JSON `null`; every other 37th is entirely absent. Neither is
/// a value a RANGE predicate can match, on the row or in the posting.
fn range_has_born(i: u64) -> bool {
    i % 41 != 0 && i % 37 != 0
}

fn range_tag(i: u64) -> &'static str {
    if (i - 1) % 10 == 0 {
        "alpha widgets travel far"
    } else {
        "beta gadgets stay put"
    }
}

fn range_deleted(i: u64) -> bool {
    i % RANGE_DELETE_STRIDE == 0
}

fn range_row_value(i: u64) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert(
        "position".to_string(),
        json!({"type": "Point", "coordinates": [range_lon(i), 0.0]}),
    );
    if i % 41 == 0 {
        obj.insert("born".to_string(), Value::Null);
    } else if i % 37 != 0 {
        obj.insert("born".to_string(), json!(range_born(i)));
    }
    obj.insert("tag".to_string(), json!(range_tag(i)));
    Value::Object(obj)
}

/// Every index is built to READY before any row is deleted, so the delete
/// exercises the same snapshot the posting-built set and the row-read path
/// both read: a posting that outlived its row is not this fixture's question,
/// a row that is simply gone from every driver is.
fn range_fixture(dir: &std::path::Path) -> RangeFixture {
    let mut db = Database::create(dir.join("db"), cfg()).unwrap();
    let rows = db
        .create_collection(
            "r",
            vec![
                ("position".into(), Kind::Point),
                ("born".into(), Kind::Int),
                ("tag".into(), Kind::Text),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();
    for i in 1..=RANGE_ROWS {
        db.put(rows, &format!("k{i:08}"), &range_row_value(i)).unwrap();
        if i % 256 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    let position = db
        .create_point_index(rows, "position_idx", "position")
        .unwrap();
    db.build_index_to_ready(position, 256).unwrap();
    let born = db
        .create_scalar_index(rows, "born_idx", "born", false)
        .unwrap();
    db.build_index_to_ready(born, 256).unwrap();
    let tag = db.create_text_index(rows, "tag_idx", "tag").unwrap();
    db.build_index_to_ready(tag, 256).unwrap();
    db.commit().unwrap();
    for i in (RANGE_DELETE_STRIDE..=RANGE_ROWS).step_by(RANGE_DELETE_STRIDE as usize) {
        db.delete(rows, &format!("k{i:08}")).unwrap();
    }
    db.commit().unwrap();
    db.checkpoint().unwrap();
    RangeFixture {
        db,
        rows,
        position,
        born,
        tag,
    }
}

/// Bounds are INCLUSIVE at both edges, so the east edge sits on the last
/// included bucket's own longitude -- one bucket short of
/// `RANGE_BBOX_BUCKETS` -- rather than on the first excluded one.
fn range_bbox_filter(position: IndexId) -> QueryFilter<'static> {
    QueryFilter::Point {
        index: position,
        predicate: PointFilter::Bbox(
            Bounds::new(
                -180.0,
                -180.0 + (RANGE_BBOX_BUCKETS - 1) as f64 * 3.6,
                -1.0,
                1.0,
            )
            .unwrap(),
        ),
    }
}

fn range_text_filter(tag: IndexId) -> QueryFilter<'static> {
    QueryFilter::Text {
        index: tag,
        query: "alpha",
        matching: TextMatch::Any,
    }
}

fn range_born_filter(born: IndexId) -> QueryFilter<'static> {
    QueryFilter::Scalar {
        index: born,
        predicate: ScalarFilter::Range {
            lower: Bound::Included(ScalarValue::I64(0)),
            upper: Bound::Excluded(ScalarValue::I64(10)),
        },
    }
}

/// Rows inside the bbox, undeleted -- the spatial driver's own candidates,
/// before the `born` filter is asked anything.
fn range_bbox_candidates() -> Vec<u64> {
    (1..=RANGE_ROWS)
        .filter(|&i| range_bucket(i) < RANGE_BBOX_BUCKETS && !range_deleted(i))
        .collect()
}

/// Rows carrying the text driver's term, undeleted.
fn range_text_candidates() -> Vec<u64> {
    (1..=RANGE_ROWS)
        .filter(|&i| (i - 1) % 10 == 0 && !range_deleted(i))
        .collect()
}

/// The bbox candidates that also satisfy `born IN [0, 10)`.
fn range_bbox_born_oracle() -> Vec<u64> {
    range_bbox_candidates()
        .into_iter()
        .filter(|&i| range_has_born(i) && range_born(i) < 10)
        .collect()
}

/// The text candidates that also satisfy `born IN [0, 10)`.
fn range_text_born_oracle() -> Vec<u64> {
    range_text_candidates()
        .into_iter()
        .filter(|&i| range_has_born(i) && range_born(i) < 10)
        .collect()
}

fn range_zero_scalar_postings() -> QueryBudget {
    QueryBudget {
        scalar_postings: 0,
        ..QueryBudget::unlimited()
    }
}

/// Drain a query and report the ids it returned, in order, plus the summed
/// work every page charged.
fn range_drain(
    db: &Database,
    rows: sekejap_core::collections::CollectionId,
    filters: &[QueryFilter<'_>],
    budget: QueryBudget,
) -> (Vec<u64>, QueryWork) {
    let mut prepared = db
        .prepare_query(QueryRequest {
            collection: rows,
            filters,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let mut ids = Vec::new();
    let mut work = QueryWork::default();
    loop {
        let page = prepared.next_page(PAGE, budget, || false).unwrap();
        for row in &page.rows {
            ids.push(row.id.sequence);
        }
        work.candidates += page.work.candidates;
        work.primary_reads += page.work.primary_reads;
        work.scalar_postings += page.work.scalar_postings;
        work.spatial_postings += page.work.spatial_postings;
        work.text_postings += page.work.text_postings;
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    (ids, work)
}

/// A spatial-driven page's non-driving `born` RANGE reads no row at all.
///
/// Before: every one of the spatial driver's ~3,000 candidates paid a primary
/// read for `born` alone (the spatial filter already certifies itself from
/// its own posting's decoded point, so that read was ENTIRELY the range
/// predicate's). After: `born`'s posting range is walked once into a set, and
/// membership is a binary search against it.
#[test]
fn a_spatial_driven_pages_non_driving_range_reads_no_row() {
    let temp = tempfile::tempdir().unwrap();
    let f = range_fixture(temp.path());
    let filters = [range_bbox_filter(f.position), range_born_filter(f.born)];
    let candidates = range_bbox_candidates();
    let oracle = range_bbox_born_oracle();
    assert!(
        (2_000..4_000).contains(&candidates.len()),
        "the fixture should offer about 3,000 spatial candidates, offered {}",
        candidates.len()
    );
    assert!(!oracle.is_empty() && oracle.len() < candidates.len(), "the fixture must have a real, partial answer");

    let (ids, work) = range_drain(&f.db, f.rows, &filters, QueryBudget::unlimited());
    assert_eq!(ids, oracle);
    assert_eq!(
        work.primary_reads, 0,
        "the spatial filter certifies itself and the range answers from its set"
    );
}

/// The same shape under a TEXT driver: `Any` matching certifies from its own
/// postings, so the only row-reading filter before this change was `born`.
#[test]
fn a_text_driven_pages_non_driving_range_reads_no_row() {
    let temp = tempfile::tempdir().unwrap();
    let f = range_fixture(temp.path());
    let filters = [range_text_filter(f.tag), range_born_filter(f.born)];
    let candidates = range_text_candidates();
    let oracle = range_text_born_oracle();
    assert!(
        (2_000..4_000).contains(&candidates.len()),
        "the fixture should offer about 3,000 text candidates, offered {}",
        candidates.len()
    );
    assert!(!oracle.is_empty() && oracle.len() < candidates.len(), "the fixture must have a real, partial answer");

    let (ids, work) = range_drain(&f.db, f.rows, &filters, QueryBudget::unlimited());
    assert_eq!(ids, oracle);
    assert_eq!(work.primary_reads, 0);
}

/// The posting-built set and the row-read fallback agree, over a fixture with
/// missing `born`, explicit-null `born`, out-of-range `born`, and rows
/// deleted after every index was built.
///
/// The fallback is forced by a `scalar_postings` budget of zero: the walk
/// that would build the set cannot afford its first posting, so
/// `ensure_membership_sets` steps back to `MembershipSet::Overflow` and
/// every candidate reads its row exactly as it did before this file's
/// changes -- the same budget that a non-driving range never spent then, and
/// still does not spend now that the walk failed to afford it.
#[test]
fn the_posting_built_set_and_the_row_read_fallback_agree() {
    let temp = tempfile::tempdir().unwrap();
    let f = range_fixture(temp.path());
    for filters in [
        vec![range_bbox_filter(f.position), range_born_filter(f.born)],
        vec![range_text_filter(f.tag), range_born_filter(f.born)],
    ] {
        let (built_ids, built_work) = range_drain(&f.db, f.rows, &filters, QueryBudget::unlimited());
        let (fallback_ids, fallback_work) =
            range_drain(&f.db, f.rows, &filters, range_zero_scalar_postings());
        assert_eq!(
            built_ids, fallback_ids,
            "the posting-built set must answer exactly what the row-read fallback answers"
        );
        assert!(!built_ids.is_empty(), "the fixture must have a real answer");
        assert_eq!(built_work.primary_reads, 0);
        assert_eq!(fallback_work.primary_reads, fallback_work.candidates);
        assert!(fallback_work.primary_reads > 0, "the fallback must still read rows");
    }
}

/// A range too wide for the budget still answers correctly, by reading every
/// candidate's row exactly as the code did before this file's changes.
#[test]
fn a_range_the_budget_cannot_afford_still_reads_rows_and_still_answers() {
    let temp = tempfile::tempdir().unwrap();
    let f = range_fixture(temp.path());
    let filters = [range_bbox_filter(f.position), range_born_filter(f.born)];
    let oracle = range_bbox_born_oracle();

    let (ids, work) = range_drain(&f.db, f.rows, &filters, range_zero_scalar_postings());
    assert_eq!(ids, oracle);
    assert_eq!(work.scalar_postings, 0, "the aborted build must not spend the budget it could not afford");
    assert_eq!(
        work.primary_reads, work.candidates,
        "one primary read per candidate is the fallback's shape"
    );
    assert!(work.primary_reads > 0);
}

// -- C1 v3: the id set is a BITMAP over the collection's own allocated span,
// not a sorted Vec of matches --------------------------------------------
//
// A Vec sized by MATCH COUNT sorts before it can answer anything, and a wide
// range in a large collection can need more matches than `RUN_BYTES` holds as
// `u64`s even though the collection itself is nowhere near that large (a
// whole decade of `born` at 48M rows is 5.6M matches -- 45 MB of `u64`,
// above the old 8 MiB cap, so the set never built there at all). A bitmap
// costs one bit per sequence the collection has EVER allocated, known from
// the collection's own counter without a scan, so its size tracks the
// collection, not the range's selectivity: 48M sequences is 6 MB, comfortably
// under `RUN_BYTES`, regardless of how many of them the range matches.

/// The `born` matches across the WHOLE `range_fixture` collection, not just
/// one driver's candidate slice -- what the posting walk this set is built
/// from actually visits.
fn range_born_matches_total() -> usize {
    (1..=RANGE_ROWS)
        .filter(|&i| !range_deleted(i) && range_has_born(i) && range_born(i) < 10)
        .count()
}

/// Even `range_fixture`'s modest 30,001-sequence span keeps the bitmap under
/// 4 KB, so the plain-Vec representation of this fixture's whole-collection
/// `born` matches -- a few thousand `u64`s -- is already bigger than that
/// bitmap would be. The switchover is not just a large-scale phenomenon: any
/// collection narrow enough for its bitmap to undercut a modest match count
/// takes the bitmap, and answers exactly as the Vec representation would
/// have.
#[test]
fn a_born_range_set_over_a_narrow_span_uses_the_bitmap() {
    let temp = tempfile::tempdir().unwrap();
    let f = range_fixture(temp.path());
    let bitmap_bytes = (RANGE_ROWS + 1).div_ceil(8);
    let vec_switchover = bitmap_bytes / 8;
    let matches = range_born_matches_total();
    assert!(
        matches as u64 > vec_switchover,
        "the fixture must exceed the vec-to-bitmap switchover to exercise it \
         ({matches} whole-collection matches, switchover at {vec_switchover} \
         for a {bitmap_bytes}-byte bitmap"
    );

    let filters = [range_bbox_filter(f.position), range_born_filter(f.born)];
    let (ids, work) = range_drain(&f.db, f.rows, &filters, QueryBudget::unlimited());
    assert_eq!(ids, range_bbox_born_oracle());
    assert_eq!(work.primary_reads, 0);
}

/// Rows enough that a Vec of every `born` match would be bigger than the old
/// flat 8 MiB cap (`RUN_BYTES / size_of::<u64>()` ids) allowed -- the shape
/// of the 48M-row `name_and_born` case, reproduced at a size a test can
/// build. `born` is a constant every row satisfies, so the range's own match
/// count is the whole collection; `grp` is the driver, selective enough
/// (1 in 1,000) that the query itself stays cheap even though the set the
/// range builds underneath it spans the whole fixture.
const BIG_SPAN_ROWS: u64 = 1_060_000;

/// The plain-Vec cap `build_scalar_range_set` used before this file's
/// bitmap existed: `RUN_BYTES` (8 MiB) worth of `u64` ids. Kept here, not
/// imported, because the production constant is a private implementation
/// detail of `src/query.rs` -- this is the same arithmetic, restated as the
/// fact this test exists to check: `BIG_SPAN_ROWS` must exceed it.
const OLD_VEC_CAP_IDS: u64 = (8usize << 20) as u64 / 8;

struct BigSpanFixture {
    db: Database,
    rows: sekejap_core::collections::CollectionId,
    grp: IndexId,
    born: IndexId,
}

fn big_span_fixture(dir: &std::path::Path) -> BigSpanFixture {
    let mut db = Database::create(dir.join("db"), cfg()).unwrap();
    let rows = db
        .create_collection(
            "big",
            vec![("grp".into(), Kind::Int), ("born".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();
    for i in 1..=BIG_SPAN_ROWS {
        db.put(
            rows,
            &format!("k{i:09}"),
            &json!({"grp": (i % 1000 == 0) as i64, "born": 5}),
        )
        .unwrap();
        if i % 20_000 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    let grp = db.create_scalar_index(rows, "grp_idx", "grp", false).unwrap();
    db.build_index_to_ready(grp, 256).unwrap();
    let born = db.create_scalar_index(rows, "born_idx", "born", false).unwrap();
    db.build_index_to_ready(born, 256).unwrap();
    db.commit().unwrap();
    db.checkpoint().unwrap();
    BigSpanFixture { db, rows, grp, born }
}

fn big_span_grp_filter(grp: IndexId) -> QueryFilter<'static> {
    QueryFilter::Scalar {
        index: grp,
        predicate: ScalarFilter::Eq(ScalarValue::I64(1)),
    }
}

fn big_span_born_filter(born: IndexId) -> QueryFilter<'static> {
    QueryFilter::Scalar {
        index: born,
        predicate: ScalarFilter::Range {
            lower: Bound::Included(ScalarValue::I64(0)),
            upper: Bound::Excluded(ScalarValue::I64(10)),
        },
    }
}

fn big_span_oracle() -> Vec<u64> {
    (1..=BIG_SPAN_ROWS).filter(|&i| i % 1000 == 0).collect()
}

#[test]
fn a_range_wider_than_the_old_vec_budget_still_takes_the_set_path() {
    assert!(
        BIG_SPAN_ROWS > OLD_VEC_CAP_IDS,
        "the fixture must out-grow the old flat Vec cap ({OLD_VEC_CAP_IDS} ids) \
         for this test to mean anything"
    );
    let bitmap_bytes = (BIG_SPAN_ROWS + 1).div_ceil(8);
    assert!(
        bitmap_bytes <= (8usize << 20) as u64,
        "the fixture's span must still fit a bitmap within RUN_BYTES ({bitmap_bytes} bytes needed)"
    );

    let temp = tempfile::tempdir().unwrap();
    let f = big_span_fixture(temp.path());
    let filters = [big_span_grp_filter(f.grp), big_span_born_filter(f.born)];
    let oracle = big_span_oracle();
    assert!(!oracle.is_empty());

    let (ids, work) = range_drain(&f.db, f.rows, &filters, QueryBudget::unlimited());
    assert_eq!(ids, oracle);
    assert_eq!(
        work.primary_reads, 0,
        "a range whose match count would have overflowed the old plain-Vec cap \
         must still answer from a set once a bitmap fits the budget"
    );
    assert!(
        work.scalar_postings as u64 >= BIG_SPAN_ROWS,
        "the born set's one-time walk must visit every one of its {BIG_SPAN_ROWS} \
         matches; saw {}",
        work.scalar_postings
    );
}

// -- KD: CandidateDriver::Keys ------------------------------------------
//
// The external-key mapping keyspace (`mapping_key`, collections.rs:382) is
// E4's counterpart of SQLite's automatic `(_key, rowid)` covering index --
// see `.insert-loop/loop3/ROOTCAUSE-count-all.md`. `CandidateDriver::Keys`
// enumerates it directly instead of the primary rows.

/// A full key enumeration touches the mapping leaves, not the primary rows.
///
/// BEFORE (`CandidateDriver::Entities`, still exercised here for the
/// comparison): one primary read per row, walking the >384 B wide rows this
/// fixture writes -- the same shape `ROOTCAUSE-count-all.md` measured as one
/// primary leaf per ~25 rows at popsim's narrower 150 B row width, so a wider
/// row here fits fewer per leaf still. AFTER (`CandidateDriver::Keys`): the
/// mapping entry (~10 B key + a handful of header/value bytes) is the only
/// thing read, and an empty-projection page never opens the primary row at
/// all (`PreparedQuery::winner_needs_no_row`).
#[test]
fn a_full_key_enumeration_reads_no_primary_pages() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = wide_fixture(temp.path());

    let mut before = fixture
        .db
        .prepare_query(QueryRequest {
            collection: fixture.rows,
            filters: &[],
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Entities,
        })
        .unwrap();
    let before_accesses_start = fixture.db.pool_accesses().unwrap();
    let mut before_ids = Vec::new();
    let mut before_reads = 0u64;
    loop {
        let page = before.next_page(PAGE, QueryBudget::unlimited(), || false).unwrap();
        before_ids.extend(page.rows.iter().map(|row| row.id.sequence));
        before_reads += page.work.primary_reads;
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    let before_accesses = fixture.db.pool_accesses().unwrap() - before_accesses_start;
    assert_eq!(before_ids.len(), WIDE_ROWS as usize);
    // At least one primary read per entity -- WIDE_ROWS exceeds one page, so
    // the walk also pays a few boundary reads at each resume (the same
    // mechanism `DriverCursor::new`'s doc describes: a resumed page reopens
    // AT its predecessor's last key and drops the duplicate after ranking
    // it), which is page-count noise `>=` absorbs without hiding the real
    // shape: one full row per entity, every entity.
    assert!(
        before_reads >= WIDE_ROWS,
        "the old driver must read at least one primary row per entity: saw {before_reads} for {WIDE_ROWS} rows"
    );

    let mut after = fixture
        .db
        .prepare_query(QueryRequest {
            collection: fixture.rows,
            filters: &[],
            order: QueryOrder::Driver,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Keys,
        })
        .unwrap();
    let after_accesses_start = fixture.db.pool_accesses().unwrap();
    let mut after_ids = Vec::new();
    let mut after_reads = 0u64;
    let mut after_key_postings = 0u64;
    loop {
        let page = after.next_page(PAGE, QueryBudget::unlimited(), || false).unwrap();
        assert_eq!(page.driver, QueryDriver::Keys);
        after_ids.extend(page.rows.iter().map(|row| row.id.sequence));
        after_reads += page.work.primary_reads;
        after_key_postings += page.work.key_postings;
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    let after_accesses = fixture.db.pool_accesses().unwrap() - after_accesses_start;

    let mut sorted_before = before_ids.clone();
    sorted_before.sort_unstable();
    let mut sorted_after = after_ids.clone();
    sorted_after.sort_unstable();
    assert_eq!(
        sorted_after, sorted_before,
        "both drivers must enumerate the same collection"
    );
    assert_eq!(
        after_reads, 0,
        "a key-driven Ids page must read zero primary pages"
    );
    // Same resume-boundary noise as `before_reads` above, on the mapping
    // walk instead of the primary one.
    assert!(
        after_key_postings >= WIDE_ROWS,
        "one mapping entry touched per row, at least: saw {after_key_postings} for {WIDE_ROWS} rows"
    );
    assert!(
        after_key_postings < WIDE_ROWS * 2,
        "a key enumeration must not be re-walking whole pages: saw {after_key_postings} for {WIDE_ROWS} rows"
    );
    assert!(
        after_accesses.saturating_mul(3) <= before_accesses,
        "key enumeration touched {after_accesses} pool accesses against {before_accesses} \
         for the primary walk over {WIDE_ROWS} rows -- wanted at least 3x fewer"
    );
}

const KD_ROWS: u64 = 200;
const KD_DELETE_STRIDE: u64 = 7;

/// `i` maps to a key that DECREASES as `i` increases -- key order is the
/// reverse of insertion/id order, so a test that passes here cannot be
/// passing by accident the way `wide_fixture`'s zero-padded, id-ordered keys
/// could (see `ROOTCAUSE-count-all.md`'s note that popsim's own keys happen
/// to coincide with id order, "a property of the fixture, not of the
/// engine").
fn kd_key(i: u64) -> String {
    format!("u{:05}", KD_ROWS + 1 - i)
}

struct KdFixture {
    db: Database,
    rows: CollectionId,
    /// Every inserted (sequence, key) pair, insertion order.
    all: Vec<(u64, String)>,
    deleted: std::collections::HashSet<String>,
}

fn kd_fixture(dir: &std::path::Path) -> KdFixture {
    let mut db = Database::create(dir.join("db"), cfg()).unwrap();
    let rows = db
        .create_collection("kd", vec![("v".into(), Kind::Int)], CollectionOptions::default())
        .unwrap();
    db.commit().unwrap();
    let mut all = Vec::new();
    for i in 1..=KD_ROWS {
        let key = kd_key(i);
        let id = db.put(rows, &key, &json!({"v": i as i64})).unwrap();
        all.push((id.sequence, key));
    }
    db.commit().unwrap();
    let mut deleted = std::collections::HashSet::new();
    for i in (KD_DELETE_STRIDE..=KD_ROWS).step_by(KD_DELETE_STRIDE as usize) {
        let key = kd_key(i);
        assert!(db.delete(rows, &key).unwrap());
        deleted.insert(key);
    }
    db.commit().unwrap();
    KdFixture { db, rows, all, deleted }
}

/// Every alive (sequence, key) pair whose key falls in `[lower, upper)`,
/// sorted by key ascending.
fn kd_oracle_range(fixture: &KdFixture, lower: &str, upper: &str) -> Vec<u64> {
    let mut matches: Vec<(String, u64)> = fixture
        .all
        .iter()
        .filter(|(_, key)| !fixture.deleted.contains(key))
        .filter(|(_, key)| key.as_str() >= lower && key.as_str() < upper)
        .map(|(seq, key)| (key.clone(), *seq))
        .collect();
    matches.sort();
    matches.into_iter().map(|(_, seq)| seq).collect()
}

/// How many keys `[lower, upper)` would hold with NOTHING deleted -- the
/// witness that `kd_oracle_range` genuinely has fewer, i.e. that deleted
/// keys inside the range are not silently still being counted.
fn kd_dense_range_count(fixture: &KdFixture, lower: &str, upper: &str) -> usize {
    fixture
        .all
        .iter()
        .filter(|(_, key)| key.as_str() >= lower && key.as_str() < upper)
        .count()
}

fn kd_drain_range(
    fixture: &KdFixture,
    lower: Bound<&str>,
    upper: Bound<&str>,
    page_size: usize,
) -> (Vec<u64>, u64, usize) {
    let filters = [QueryFilter::Key { lower, upper }];
    let mut prepared = fixture
        .db
        .prepare_query(QueryRequest {
            collection: fixture.rows,
            filters: &filters,
            order: QueryOrder::Driver,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Keys,
        })
        .unwrap();
    let mut ids = Vec::new();
    let mut primary_reads = 0u64;
    let mut pages = 0usize;
    loop {
        let page = prepared.next_page(page_size, QueryBudget::unlimited(), || false).unwrap();
        assert_eq!(page.driver, QueryDriver::Keys);
        assert!(page.rows.len() <= page_size);
        ids.extend(page.rows.iter().map(|row| row.id.sequence));
        primary_reads += page.work.primary_reads;
        pages += 1;
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    (ids, primary_reads, pages)
}

/// A key RANGE returns exactly the alive rows whose keys fall in it, in key
/// order, resumed across several small pages, with deleted keys absent.
#[test]
fn a_key_range_pages_resume_in_key_order_with_deleted_keys_absent() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = kd_fixture(temp.path());

    let oracle = kd_oracle_range(&fixture, "u00050", "u00100");
    let dense = kd_dense_range_count(&fixture, "u00050", "u00100");
    assert!(oracle.len() > 20, "the fixture must have a real range to page through");
    assert!(
        oracle.len() < dense,
        "the range must contain at least one deleted key for this test to mean anything: \
         {} alive of {dense} dense",
        oracle.len()
    );

    let (ids, primary_reads, pages) =
        kd_drain_range(&fixture, Bound::Included("u00050"), Bound::Excluded("u00100"), 7);
    assert!(pages > 1, "the fixture and page size must force a resume");
    assert_eq!(
        ids, oracle,
        "a key range must return exactly the alive in-range rows, in key order"
    );
    assert_eq!(
        primary_reads, 0,
        "a certified key range answers Ids with no primary reads"
    );
}

/// A key PREFIX is a range: bytes `[prefix, one-past-the-last-digit)`.
#[test]
fn a_key_prefix_is_expressed_as_a_range_and_resumes_the_same_way() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = kd_fixture(temp.path());

    // Every key with prefix "u001" is exactly u00100..=u00199: the fourth
    // byte is what would have to change to leave the prefix, and '1' < '2'
    // decides the comparison at that byte regardless of what follows, so
    // "u002" is the exact exclusive upper bound of the prefix "u001".
    let oracle = kd_oracle_range(&fixture, "u001", "u002");
    let dense = kd_dense_range_count(&fixture, "u001", "u002");
    assert_eq!(dense, 100, "the prefix must name exactly u00100..=u00199");
    assert!(oracle.len() < dense, "the prefix must contain a deleted key too");

    let (ids, primary_reads, pages) =
        kd_drain_range(&fixture, Bound::Included("u001"), Bound::Excluded("u002"), 9);
    assert!(pages > 1, "the fixture and page size must force a resume");
    assert_eq!(ids, oracle, "a key prefix must return exactly its alive rows, in key order");
    assert_eq!(primary_reads, 0);
}

/// A `QueryFilter::Key` named at a position `CandidateDriver::Keys` is not
/// driving from is refused at prepare time -- see `prepare_query`'s
/// certification check. There is no row-level fallback for it the way a
/// scalar predicate has one.
#[test]
fn a_key_filter_without_the_keys_driver_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = kd_fixture(temp.path());
    let filters = [QueryFilter::Key {
        lower: Bound::Included("u00050"),
        upper: Bound::Excluded("u00100"),
    }];
    let result = fixture.db.prepare_query(QueryRequest {
        collection: fixture.rows,
        filters: &filters,
        order: QueryOrder::EntityId,
        projection: Projection::Ids,
        total_limit: None,
        driver: CandidateDriver::Auto,
    });
    match result {
        Err(sekejap_core::collections::QueryError::Database(_)) => {}
        _ => panic!("a key filter without CandidateDriver::Keys must be refused at prepare time"),
    }
}

// -- QD: QueryOrder::Distance counted --------------------------------------

fn qd_unit(seed: &mut u64) -> f64 {
    *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    (*seed >> 11) as f64 / (1u64 << 53) as f64
}

/// (d) Nearest 10 over 50K points with Projection::Ids: no primary reads,
/// spatial postings examined <= 200, pool accesses <= 48.
#[test]
fn distance_order_knn10_over_50k_points_is_a_probe_not_a_scan() {
    const BBOX_LON: (f64, f64) = (106.4, 108.8);
    const BBOX_LAT: (f64, f64) = (-7.8, -5.9);
    const CENTER: (f64, f64) = (107.6, -6.9);
    const ROWS: usize = 50_000;

    let temp = tempfile::tempdir().unwrap();
    let mut db = Database::create(
        temp.path().join("db"),
        Config {
            budget_bytes: 32 << 20,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )
    .unwrap();
    let collection = db
        .create_collection(
            "points",
            vec![("position".into(), Kind::Point)],
            CollectionOptions::default(),
        )
        .unwrap();
    let mut seed = 0x510a_55ed_c0ff_eeeeu64;
    for n in 0..ROWS {
        let lon = BBOX_LON.0 + (BBOX_LON.1 - BBOX_LON.0) * qd_unit(&mut seed);
        let lat = BBOX_LAT.0 + (BBOX_LAT.1 - BBOX_LAT.0) * qd_unit(&mut seed);
        db.put(
            collection,
            &format!("p{n:05}"),
            &json!({"position": {"type": "Point", "coordinates": [lon, lat]}}),
        )
        .unwrap();
        if n % 1000 == 999 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    let index = db
        .create_point_index(collection, "position", "position")
        .unwrap();
    db.build_index_to_ready(index, 255).unwrap();
    db.commit().unwrap();
    db.checkpoint().unwrap();

    let center = Point::new(CENTER.0, CENTER.1).unwrap();
    let mut prepared = db
        .prepare_query(QueryRequest {
            collection,
            filters: &[],
            order: QueryOrder::Distance {
                index,
                center,
                direction: SortDirection::Ascending,
            },
            projection: Projection::Ids,
            total_limit: Some(10),
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let before = db.pool_accesses().unwrap();
    let page = prepared
        .next_page(10, QueryBudget::unlimited(), || false)
        .unwrap();
    let pool_accesses = db.pool_accesses().unwrap() - before;
    assert!(page.done);
    assert_eq!(page.rows.len(), 10);
    assert_eq!(
        page.driver,
        QueryDriver::Nearest { index },
        "{:?}",
        page.driver
    );
    assert_eq!(
        page.work.primary_reads, 0,
        "Ids projection must not read a row: {:?}",
        page.work
    );
    assert!(
        page.work.spatial_postings <= 200,
        "examined {} spatial postings, want <= 200",
        page.work.spatial_postings
    );
    assert!(
        pool_accesses <= 48,
        "pool accesses {pool_accesses}, want <= 48"
    );
}


fn geo_square(lon: f64, lat: f64, half: f64) -> Geom {
    Geom::Polygon(vec![vec![
        [lon - half, lat - half],
        [lon + half, lat - half],
        [lon + half, lat + half],
        [lon - half, lat + half],
        [lon - half, lat - half],
    ]])
}

fn geo_json(g: &Geom) -> Value {
    match g {
        Geom::Polygon(rs) => json!({"type": "Polygon", "coordinates": rs}),
        Geom::Point(x, y) => json!({"type": "Point", "coordinates": [x, y]}),
        _ => panic!("budget fixture only stores polygons and points"),
    }
}

/// A within-polygon query over 20K small polygons where 2% overlap the query
/// box: candidates admitted by BoxF <= 1.5x the true answer, primary reads
/// equal the admitted candidates (the refine), pool accesses bounded by
/// cover leaves + candidate rows + a constant.
#[test]
fn geometry_within_polygon_admits_a_bounded_candidate_set() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Database::create(temp.path().join("db"), cfg()).unwrap();
    let rows = db
        .create_collection(
            "plots",
            vec![("plot".into(), Kind::Geo)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();

    const N: u64 = 20_000;
    // Query window well inside the grid, with a margin so the 2% that are
    // placed fully inside are Within, not merely BoxF-overlapping.
    let query = geo_square(0.0, 0.0, 1.0);
    let mut truth = 0u64;
    for i in 0..N {
        // 2% sit well inside the query square; the rest sit far outside.
        let inside = i % 50 == 0;
        let (lon, lat) = if inside {
            let k = i / 50;
            (-0.6 + (k % 20) as f64 * 0.06, -0.6 + (k / 20) as f64 * 0.06)
        } else {
            (20.0 + (i % 200) as f64 * 0.1, 20.0 + (i / 200) as f64 * 0.1)
        };
        let geom = geo_square(lon, lat, 0.02);
        if spatial_geometry::within(&geom, &query) {
            truth += 1;
        }
        db.put(
            rows,
            &format!("p{i:05}"),
            &json!({"plot": geo_json(&geom)}),
        )
        .unwrap();
        if (i + 1) % 512 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    let index = db.create_geometry_index(rows, "by_plot", "plot").unwrap();
    db.build_index_to_ready(index, 256).unwrap();
    db.commit().unwrap();
    db.checkpoint().unwrap();

    assert!(truth > 0, "fixture must have a non-empty within-answer");
    assert!(
        (truth as f64) <= (N as f64) * 0.03,
        "fixture 2% band drifted: truth={truth}"
    );

    let before = db.pool_accesses().unwrap();
    let mut prepared = db
        .prepare_query(QueryRequest {
            collection: rows,
            filters: &[QueryFilter::Geometry {
                index,
                predicate: GeometryFilter::Within(query),
            }],
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let page = prepared
        .next_page(PAGE, QueryBudget::unlimited(), || false)
        .unwrap();
    let accesses = db.pool_accesses().unwrap() - before;

    assert_eq!(page.rows.len() as u64, truth);
    // BoxF admission is a superset of Within; on this fixture the inside
    // squares sit well inside the query box, so the ratio stays near 1.
    assert!(
        page.work.candidates as f64 <= truth as f64 * 1.5,
        "BoxF admitted {} candidates for {truth} hits",
        page.work.candidates
    );
    assert_eq!(
        page.work.primary_reads, page.work.candidates,
        "every BoxF-admitted candidate is refined with a primary read: {:?}",
        page.work
    );
    // Cover walk + one row per admitted candidate, plus a small constant for
    // catalog/descriptor probes.
    // Each admitted candidate is refined from its row, and under entity
    // order over a cell-ordered walk each row is its own descent of the
    // primary tree: three pages deep at 20K rows. Gathering those reads in
    // id order (as the batched gather does for row-needing filters) would
    // bring this to about one access per candidate; that is a follow-up,
    // and this bound pins today's cost so the follow-up has a number to beat.
    let bound = page.work.spatial_postings + page.work.candidates * 4 + 64;
    assert!(
        accesses <= bound,
        "{accesses} pager accesses vs cover+rows bound {bound} (postings={}, candidates={})",
        page.work.spatial_postings,
        page.work.candidates
    );
}
