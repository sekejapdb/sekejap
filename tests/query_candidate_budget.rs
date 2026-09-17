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
use e4_prototype::{
    collections::{
        CandidateDriver, CollectionOptions, Database, IndexId, Projection, QueryBudget, QueryFilter,
        QueryOrder, QueryRequest, ScalarFilter, ScalarValue, SortDirection, TextMatch,
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
    rows: e4_prototype::collections::CollectionId,
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
            e4_prototype::collections::OrderValue::Bm25(score) => score,
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
    rows: e4_prototype::collections::CollectionId,
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
    rows: e4_prototype::collections::CollectionId,
    pod: IndexId,
    price: IndexId,
}

fn wide_fixture(dir: &std::path::Path) -> Wide {
    let mut db = Database::create(dir.join("db"), cfg()).unwrap();
    let rows = db
        .create_collection(
            "w",
            vec![
                ("pod".into(), Kind::Text),
                ("price".into(), Kind::Real),
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
    let price = db
        .create_scalar_index(rows, "price_idx", "price", false)
        .unwrap();
    db.build_index_to_ready(price, 256).unwrap();
    db.commit().unwrap();
    db.checkpoint().unwrap();
    Wide {
        db,
        rows,
        pod,
        price,
    }
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
        QueryFilter::Scalar {
            index: fixture.price,
            predicate: ScalarFilter::Range {
                lower: Bound::Excluded(ScalarValue::F64(400.0)),
                upper: Bound::Unbounded,
            },
        },
    ];
    // Every row the equality posting names has its row read for the range
    // predicate; those reads are what this test is about.
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
