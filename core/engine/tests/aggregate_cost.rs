//! What one GROUP of the aggregation atomic is allowed to COST.
//!
//! `core/engine/tests/query_aggregate.rs` pins what the fold ANSWERS. This
//! file pins what it SPENDS, against the same kind of oracle -- a brute-force
//! fold over the rows the test itself generated and holds -- because the four
//! aggregate cases of the 50,000-row battery are slower than PostgreSQL for
//! reasons a correct answer cannot show:
//!
//!   1. DISTINCT over a scalar index (a group with NO accumulators) is one
//!      descent per DISTINCT VALUE, not a walk of every posting:
//!      `QueryWork::scalar_postings` counts values, not rows;
//!   2. an accumulator that needs no row -- `count(*)`, and `count`/`min`/
//!      `max` over the DRIVING index's own value -- is answered from the
//!      postings: `QueryWork::primary_reads == 0`;
//!   3. a NON-driving accumulator (`sum(born)` grouped by `kind`) reads its
//!      rows in ASCENDING ENTITY ORDER through one forward cursor, not with a
//!      random point-get per candidate in posting order: the buffer pool is
//!      touched about once per LEAF, not four times per row;
//!   4. the streaming fold allocates per GROUP, not per candidate.
//!
//! Everything here is counted rather than timed, so a regression names its
//! cause. The counters are `QueryWork`, `Database::pool_accesses` and a
//! counting global allocator, which is how `tests/scan_row_cost.rs` measures
//! the same three things for a key-only walk.
use sekejap_core::{
    collections::{
        Accumulator, AggValue, AggregateFn, AggregateInput, AggregateRequest, AggregateShape,
        CandidateDriver, CollectionId, CollectionOptions, Database, GroupCmp, GroupKey, GroupOrder,
        GroupPredicate, GroupRow,
        IndexId, OwnedScalarValue, Projection, QueryBudget, QueryFilter, QueryOrder, QueryRequest,
        QueryWork, ScalarFilter, ScalarValue,
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
    collections::BTreeMap,
    ops::Bound,
    path::Path,
};

// ── counting the allocations of one walk ──────────────────────────────────

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

// ── the corpus, held in the test ──────────────────────────────────────────

const ROWS: usize = 20_000;
const KINDS: [&str; 8] = [
    "depot", "farm", "home", "mill", "park", "port", "school", "shop",
];
/// A born year every row shares with about two hundred others, so `born` has
/// far fewer distinct values than rows and a DISTINCT over it is worth a skip.
const BORN_VALUES: i64 = 97;

#[derive(Clone, Debug)]
struct Row {
    key: String,
    kind: &'static str,
    born: i64,
    /// A NULL-bearing numeric column, so the POSTING JOIN's pass 2 has a
    /// nullish posting to skip: `sum`/`min`/`max`/`avg`/`count(col)` all
    /// ignore it, which is SQL's own rule and has to be true off the index
    /// exactly as it is true off the row.
    score: Option<i64>,
    /// A value PER ROW, so grouping by it opens as many groups as there are
    /// rows -- which is how the posting join's bitmap bound is reached.
    tick: i64,
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

struct Indexes {
    kind: IndexId,
    born: IndexId,
    score: IndexId,
    tick: IndexId,
}

struct Fixture {
    db: Database,
    place: CollectionId,
    rows: Vec<Row>,
    index: Indexes,
}

fn config() -> Config {
    Config {
        budget_bytes: 16 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

/// One collection, two scalar indexes, and a note wide enough that a 4 KiB
/// leaf holds tens of rows rather than hundreds -- which is what makes the
/// difference between a per-row point-get and one forward pass VISIBLE in the
/// pool-access count.
///
/// `kind` is assigned from a hash of the row's ordinal rather than from the
/// ordinal itself, so the postings of one kind are SCATTERED across the whole
/// primary tree: reading them in posting order is a random walk of the
/// collection, which is the shape the battery's `agg_sum_born_by_kind` has.
fn build(path: &Path) -> Fixture {
    let _ = std::fs::remove_dir_all(path);
    let mut db = Database::create(path, config()).unwrap();
    let place = db
        .create_collection(
            "place",
            vec![
                ("kind".into(), Kind::Text),
                ("born".into(), Kind::Int),
                ("score".into(), Kind::Int),
                ("tick".into(), Kind::Int),
                ("note".into(), Kind::Text),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    let mut rng = Rng(0x4147_4750_4552_4601);
    let mut rows = Vec::with_capacity(ROWS);
    for i in 0..ROWS {
        let draw = rng.next();
        let row = Row {
            key: format!("p{i:06}"),
            kind: KINDS[(draw % KINDS.len() as u64) as usize],
            born: 1_900 + ((draw >> 8) % BORN_VALUES as u64) as i64,
            // One row in five carries no score at all.
            score: if i % 5 == 0 {
                None
            } else {
                Some(((draw >> 16) % 1_000) as i64)
            },
            tick: i as i64,
        };
        let id = db
            .put(
                place,
                &row.key,
                &json!({
                    "kind": row.kind,
                    "born": row.born,
                    "score": match row.score {
                        Some(value) => serde_json::Value::from(value),
                        None => serde_json::Value::Null,
                    },
                    "tick": row.tick,
                    "note": "a note wide enough that a leaf holds tens of rows, not hundreds",
                }),
            )
            .unwrap();
        assert_eq!(id.sequence, (i + 1) as u64, "put order is file order");
        rows.push(row);
        if (i + 1) % 512 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    let index = Indexes {
        kind: db
            .create_scalar_index(place, "place_kind", "kind", false)
            .unwrap(),
        born: db
            .create_scalar_index(place, "place_born", "born", false)
            .unwrap(),
        score: db
            .create_scalar_index(place, "place_score", "score", false)
            .unwrap(),
        tick: db
            .create_scalar_index(place, "place_tick", "tick", false)
            .unwrap(),
    };
    db.commit().unwrap();
    for id in [index.kind, index.born, index.score, index.tick] {
        db.build_index_to_ready(id, 256).unwrap();
        db.commit().unwrap();
    }
    db.checkpoint().unwrap();
    Fixture {
        db,
        place,
        rows,
        index,
    }
}

fn open() -> (tempfile::TempDir, Fixture) {
    let dir = tempfile::tempdir().unwrap();
    let f = build(&dir.path().join("db"));
    (dir, f)
}

// ── the brute-force folds the test holds ──────────────────────────────────

/// Every distinct `kind`, in the scalar keyspace's own order (which for Text
/// is byte order, and these are ASCII).
fn distinct_kinds(rows: &[Row]) -> Vec<String> {
    let mut out: Vec<String> = rows.iter().map(|row| row.kind.to_owned()).collect();
    out.sort_unstable();
    out.dedup();
    out
}

fn distinct_born(rows: &[Row]) -> Vec<i64> {
    let mut out: Vec<i64> = rows.iter().map(|row| row.born).collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// `count(*)`, `sum(born)`, `min(born)`, `max(born)` per kind, by hand.
struct Folded {
    key: String,
    count: u64,
    sum: i64,
    min: i64,
    max: i64,
}

fn fold_by_kind(rows: &[Row]) -> Vec<Folded> {
    let mut table: BTreeMap<&str, (u64, i64, i64, i64)> = BTreeMap::new();
    for row in rows {
        let slot = table
            .entry(row.kind)
            .or_insert((0, 0, i64::MAX, i64::MIN));
        slot.0 += 1;
        slot.1 += row.born;
        slot.2 = slot.2.min(row.born);
        slot.3 = slot.3.max(row.born);
    }
    table
        .into_iter()
        .map(|(key, (count, sum, min, max))| Folded {
            key: key.to_owned(),
            count,
            sum,
            min,
            max,
        })
        .collect()
}

/// `count(*)`, `count(score)`, `sum`, `min`, `max` over the NULL-bearing
/// column, per kind, by hand. A null contributes to `count(*)` and to nothing
/// else, which is SQL's rule.
struct FoldedScore {
    key: String,
    rows: u64,
    scored: u64,
    sum: i64,
    min: i64,
    max: i64,
}

fn fold_score_by_kind(rows: &[Row]) -> Vec<FoldedScore> {
    let mut table: BTreeMap<&str, (u64, u64, i64, i64, i64)> = BTreeMap::new();
    for row in rows {
        let slot = table
            .entry(row.kind)
            .or_insert((0, 0, 0, i64::MAX, i64::MIN));
        slot.0 += 1;
        if let Some(score) = row.score {
            slot.1 += 1;
            slot.2 += score;
            slot.3 = slot.3.min(score);
            slot.4 = slot.4.max(score);
        }
    }
    table
        .into_iter()
        .map(|(key, (rows, scored, sum, min, max))| FoldedScore {
            key: key.to_owned(),
            rows,
            scored,
            sum,
            min,
            max,
        })
        .collect()
}

// ── running one aggregate ─────────────────────────────────────────────────

struct Run {
    groups: Vec<GroupRow>,
    work: QueryWork,
    shape: AggregateShape,
    pool_accesses: u64,
    allocations: usize,
}

/// Page one aggregate to exhaustion, summing the work of every page and
/// measuring the pool accesses and the allocations of the whole run.
fn run(
    db: &Database,
    request: AggregateRequest<'_>,
    page_size: usize,
    budget: QueryBudget,
) -> Run {
    let before = db.pool_accesses().unwrap();
    let ((groups, work, shape), allocations) = counted(|| {
        let mut prepared = db.prepare_aggregate(request).unwrap();
        let shape = prepared.shape();
        let mut groups = Vec::new();
        let mut work = QueryWork::default();
        loop {
            let page = prepared.next_page(page_size, budget, || false).unwrap();
            work.candidates += page.work.candidates;
            work.primary_reads += page.work.primary_reads;
            work.row_decodes += page.work.row_decodes;
            work.scalar_postings += page.work.scalar_postings;
            work.key_postings += page.work.key_postings;
            work.groups = work.groups.max(page.work.groups);
            work.membership_bytes = work.membership_bytes.max(page.work.membership_bytes);
            groups.extend(page.groups.iter().cloned());
            if page.done || page.groups.is_empty() {
                break;
            }
        }
        (groups, work, shape)
    });
    Run {
        groups,
        work,
        shape,
        pool_accesses: db.pool_accesses().unwrap() - before,
        allocations,
    }
}

fn text_key(group: &GroupRow) -> &str {
    match &group.key {
        Some(OwnedScalarValue::Text(text)) => text.as_str(),
        other => panic!("expected a text group key, got {other:?}"),
    }
}

fn int_of(value: &AggValue) -> i64 {
    match value {
        AggValue::Count(n) => *n as i64,
        AggValue::I64(v) => *v,
        other => panic!("expected a whole number, got {other:?}"),
    }
}

const COUNT_STAR: [Accumulator<'static>; 1] = [Accumulator {
    function: AggregateFn::CountStar,
    input: None,
}];

/// `count(*)`, `sum(born)`, `min(born)`, `max(born)`, `avg(born)` with `born`
/// named as ITS INDEX -- which is what makes the request a POSTING JOIN.
fn born_accumulators(index: &Indexes) -> [Accumulator<'static>; 5] {
    [
        Accumulator {
            function: AggregateFn::CountStar,
            input: None,
        },
        Accumulator {
            function: AggregateFn::Sum,
            input: Some(AggregateInput::Index(index.born)),
        },
        Accumulator {
            function: AggregateFn::Min,
            input: Some(AggregateInput::Index(index.born)),
        },
        Accumulator {
            function: AggregateFn::Max,
            input: Some(AggregateInput::Index(index.born)),
        },
        Accumulator {
            function: AggregateFn::Avg,
            input: Some(AggregateInput::Index(index.born)),
        },
    ]
}

/// The identical five accumulators with `born` named as a FIELD, which has
/// no index to walk and so reads the row: the same question, the other path.
fn born_fields() -> [Accumulator<'static>; 5] {
    [
        Accumulator {
            function: AggregateFn::CountStar,
            input: None,
        },
        Accumulator {
            function: AggregateFn::Sum,
            input: Some(AggregateInput::Field("born")),
        },
        Accumulator {
            function: AggregateFn::Min,
            input: Some(AggregateInput::Field("born")),
        },
        Accumulator {
            function: AggregateFn::Max,
            input: Some(AggregateInput::Field("born")),
        },
        Accumulator {
            function: AggregateFn::Avg,
            input: Some(AggregateInput::Field("born")),
        },
    ]
}

// ── 1. DISTINCT is one descent per distinct value ─────────────────────────

/// `SELECT DISTINCT kind` is a group with no accumulators, so nothing but the
/// EXISTENCE of each value matters: the walk seeks to the successor of the
/// current value's key prefix instead of stepping over every posting that
/// carries it. The answer is the test's own distinct set, and the cost is one
/// posting per distinct value, not one per row.
#[test]
fn distinct_over_a_scalar_index_costs_one_posting_per_distinct_value() {
    let (_dir, f) = open();
    let expected = distinct_kinds(&f.rows);
    let got = run(
        &f.db,
        AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::Index(f.index.kind)),
            accumulators: &[],
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        },
        8_192,
        QueryBudget::unlimited(),
    );
    let keys: Vec<String> = got.groups.iter().map(|g| text_key(g).to_owned()).collect();
    assert_eq!(keys, expected, "DISTINCT is the set of values, by hand");
    for group in &got.groups {
        assert!(group.values.is_empty(), "DISTINCT accumulates nothing");
    }
    // One peek per distinct value, plus the peek that runs off the end of the
    // index. Nothing else is read.
    assert!(
        got.work.scalar_postings <= expected.len() as u64 + 1,
        "DISTINCT over {} rows with {} distinct values charged {} scalar postings; \
         the bound is one per value plus the terminal peek",
        ROWS,
        expected.len(),
        got.work.scalar_postings
    );
    assert_eq!(got.work.primary_reads, 0, "DISTINCT reads no row");
    assert_eq!(
        got.work.candidates,
        expected.len() as u64,
        "one candidate per group, not one per row"
    );
    assert_eq!(
        got.shape,
        AggregateShape::Skip,
        "a group with no accumulators over the driving index skips"
    );
}

/// The same for an Int index with a hundred-odd distinct values: the win is
/// proportional to the run length, not to the type.
#[test]
fn distinct_over_an_int_index_costs_one_posting_per_distinct_value() {
    let (_dir, f) = open();
    let expected = distinct_born(&f.rows);
    let got = run(
        &f.db,
        AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::Index(f.index.born)),
            accumulators: &[],
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        },
        8_192,
        QueryBudget::unlimited(),
    );
    let keys: Vec<i64> = got
        .groups
        .iter()
        .map(|g| match &g.key {
            Some(OwnedScalarValue::I64(v)) => *v,
            other => panic!("expected an integer group key, got {other:?}"),
        })
        .collect();
    assert_eq!(keys, expected);
    assert!(
        got.work.scalar_postings <= expected.len() as u64 + 1,
        "{} distinct values charged {} scalar postings",
        expected.len(),
        got.work.scalar_postings
    );
}

/// A skip-scan pages like every other shape: three groups at a time
/// concatenate to the single-page answer, and the resume is the successor of
/// the last value handed out.
#[test]
fn a_skip_scan_pages_concatenate_to_the_single_page_answer() {
    let (_dir, f) = open();
    let whole = run(
        &f.db,
        AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::Index(f.index.kind)),
            accumulators: &[],
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        },
        8_192,
        QueryBudget::unlimited(),
    );
    for page_size in [1, 3, 7] {
        let paged = run(
            &f.db,
            AggregateRequest {
                collection: f.place,
                filters: &[],
                group: Some(GroupKey::Index(f.index.kind)),
                accumulators: &[],
                having: &[],
                order: GroupOrder::Key,
                driver: CandidateDriver::Auto,
                total_limit: None,
            },
            page_size,
            QueryBudget::unlimited(),
        );
        assert_eq!(
            paged.groups, whole.groups,
            "pages of {page_size} concatenate to the single-page answer"
        );
        assert!(
            paged.work.scalar_postings <= whole.work.scalar_postings * 2 + 4,
            "paging a skip-scan re-descends once per page, not once per row: {} postings over \
             pages of {page_size} against {} in one page",
            paged.work.scalar_postings,
            whole.work.scalar_postings
        );
    }
}

/// `LIMIT n` over DISTINCT stops after n values rather than after n rows.
#[test]
fn a_limited_distinct_stops_after_that_many_values() {
    let (_dir, f) = open();
    let expected = distinct_kinds(&f.rows);
    let got = run(
        &f.db,
        AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::Index(f.index.kind)),
            accumulators: &[],
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: Some(3),
        },
        8_192,
        QueryBudget::unlimited(),
    );
    let keys: Vec<String> = got.groups.iter().map(|g| text_key(g).to_owned()).collect();
    assert_eq!(keys, expected[..3].to_vec());
    assert!(
        got.work.scalar_postings <= 4,
        "three values cost {} scalar postings",
        got.work.scalar_postings
    );
}

/// The boundary of the skip, stated as a test rather than as a comment: with
/// a FILTER beside it, a value's group exists only if some row of that value
/// survives, so the fold has to see the rows and the shape is the ordinary
/// streaming walk. The answer is still the hand-folded one.
#[test]
fn a_distinct_under_a_filter_folds_every_candidate_and_still_agrees() {
    let (_dir, f) = open();
    let lower = 1_900 + BORN_VALUES / 2;
    let filters = [QueryFilter::Scalar {
        index: f.index.born,
        predicate: ScalarFilter::Range {
            lower: Bound::Included(ScalarValue::I64(lower)),
            upper: Bound::Unbounded,
        },
    }];
    let got = run(
        &f.db,
        AggregateRequest {
            collection: f.place,
            filters: &filters,
            group: Some(GroupKey::Index(f.index.kind)),
            accumulators: &[],
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        },
        8_192,
        QueryBudget::unlimited(),
    );
    let mut expected: Vec<String> = f
        .rows
        .iter()
        .filter(|row| row.born >= lower)
        .map(|row| row.kind.to_owned())
        .collect();
    expected.sort_unstable();
    expected.dedup();
    let keys: Vec<String> = got.groups.iter().map(|g| text_key(g).to_owned()).collect();
    assert_eq!(keys, expected);
    assert_ne!(
        got.shape,
        AggregateShape::Skip,
        "a filter can reject every row of a value, so the skip does not apply"
    );
}

// ── 2. an accumulator that needs no row reads none ────────────────────────

/// `count(*) GROUP BY kind`: the count needs no row, and the group key is the
/// driving index's own value, so the whole answer comes off the postings.
#[test]
fn count_star_by_an_indexed_group_reads_no_row() {
    let (_dir, f) = open();
    let expected = fold_by_kind(&f.rows);
    let got = run(
        &f.db,
        AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::Index(f.index.kind)),
            accumulators: &COUNT_STAR,
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        },
        8_192,
        QueryBudget::unlimited(),
    );
    assert_eq!(got.shape, AggregateShape::Streaming);
    assert_eq!(got.groups.len(), expected.len());
    for (group, want) in got.groups.iter().zip(&expected) {
        assert_eq!(text_key(group), want.key);
        assert_eq!(int_of(&group.values[0]), want.count as i64);
    }
    assert_eq!(
        got.work.primary_reads, 0,
        "count(*) over the driving index's own groups reads no row"
    );
    assert_eq!(got.work.row_decodes, 0, "and decodes none");
    assert_eq!(got.work.groups, 1, "one accumulator set is alive at a time");
}

/// `count(*)` with no group at all: the mapping keyspace is the driver and
/// the count needs nothing else.
#[test]
fn count_all_over_the_whole_collection_reads_no_row() {
    let (_dir, f) = open();
    let got = run(
        &f.db,
        AggregateRequest {
            collection: f.place,
            filters: &[],
            group: None,
            accumulators: &COUNT_STAR,
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        },
        8_192,
        QueryBudget::unlimited(),
    );
    assert_eq!(got.groups.len(), 1);
    assert_eq!(int_of(&got.groups[0].values[0]), ROWS as i64);
    assert_eq!(got.work.primary_reads, 0, "count(*) reads no row");
    assert_eq!(got.work.row_decodes, 0);
    assert_eq!(
        got.work.key_postings,
        ROWS as u64 + 1,
        "the mapping keyspace is the enumeration, plus the peek that runs off its end"
    );
}

/// The three OTHER shapes that need no row: `count(col)`, `min(col)` and
/// `max(col)` where `col` IS the driving index's own value. Each of them is
/// answered from the posting the walk is standing on.
#[test]
fn count_min_and_max_of_the_driving_value_read_no_row() {
    let (_dir, f) = open();
    let accumulators = [
        Accumulator {
            function: AggregateFn::Count,
            input: Some(AggregateInput::Index(f.index.kind)),
        },
        Accumulator {
            function: AggregateFn::Min,
            input: Some(AggregateInput::Index(f.index.kind)),
        },
        Accumulator {
            function: AggregateFn::Max,
            input: Some(AggregateInput::Index(f.index.kind)),
        },
    ];
    let expected = fold_by_kind(&f.rows);
    let got = run(
        &f.db,
        AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::Index(f.index.kind)),
            accumulators: &accumulators,
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        },
        8_192,
        QueryBudget::unlimited(),
    );
    assert_eq!(got.groups.len(), expected.len());
    for (group, want) in got.groups.iter().zip(&expected) {
        assert_eq!(text_key(group), want.key);
        assert_eq!(int_of(&group.values[0]), want.count as i64);
        // min and max of a column every row of the group shares IS that value.
        assert_eq!(group.values[1], AggValue::Text(want.key.clone()));
        assert_eq!(group.values[2], AggValue::Text(want.key.clone()));
    }
    assert_eq!(
        got.work.primary_reads, 0,
        "count/min/max over the DRIVING value read no row"
    );
}

/// The streaming fold holds one accumulator set and allocates per GROUP, not
/// per candidate: the group key is compared as the posting's own bytes and
/// decoded only when a group opens.
#[test]
fn the_streaming_fold_does_not_allocate_per_candidate() {
    let (_dir, f) = open();
    let got = run(
        &f.db,
        AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::Index(f.index.kind)),
            accumulators: &COUNT_STAR,
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        },
        8_192,
        QueryBudget::unlimited(),
    );
    let per_candidate = got.allocations as f64 / ROWS as f64;
    assert!(
        per_candidate <= 1.2,
        "a streaming count(*) over {ROWS} candidates made {} allocations \
         ({per_candidate:.3} per candidate); the bound is 1.2 -- the posting key the \
         cursor copies out of the leaf, and nothing else per row",
        got.allocations
    );
}

// ── 3. a non-driving accumulator reads its rows in entity order ───────────

/// `sum(born), min(born), max(born) GROUP BY kind` NAMED AS A FIELD: with no
/// index of its own `born` can only come off the row, so every candidate's
/// row is read. The candidates arrive in posting order -- `kind` scattered
/// the sequences -- so reading them one at a time would be a random point-get
/// per row. The streaming fold tells the reader the run ascends and restarts
/// it at each group boundary, so the pool is touched about once per leaf.
///
/// The column is spelled as a FIELD on purpose: named as its index this is
/// the POSTING JOIN below, which reads no row at all. This is the row path,
/// and the row path still has to be efficient -- an unindexed column has
/// nowhere else to read from.
#[test]
fn a_non_driving_accumulator_reads_its_rows_in_entity_order() {
    let (_dir, f) = open();
    let expected = fold_by_kind(&f.rows);
    let accumulators = [
        Accumulator {
            function: AggregateFn::CountStar,
            input: None,
        },
        Accumulator {
            function: AggregateFn::Sum,
            input: Some(AggregateInput::Field("born")),
        },
        Accumulator {
            function: AggregateFn::Min,
            input: Some(AggregateInput::Field("born")),
        },
        Accumulator {
            function: AggregateFn::Max,
            input: Some(AggregateInput::Field("born")),
        },
    ];
    let got = run(
        &f.db,
        AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::Index(f.index.kind)),
            accumulators: &accumulators,
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        },
        8_192,
        QueryBudget::unlimited(),
    );
    assert_eq!(got.groups.len(), expected.len());
    for (group, want) in got.groups.iter().zip(&expected) {
        assert_eq!(text_key(group), want.key);
        assert_eq!(int_of(&group.values[0]), want.count as i64);
        assert_eq!(int_of(&group.values[1]), want.sum);
        assert_eq!(int_of(&group.values[2]), want.min);
        assert_eq!(int_of(&group.values[3]), want.max);
    }
    assert_eq!(
        got.work.primary_reads, ROWS as u64,
        "every row is still read exactly once: the ORDER changed, not the set"
    );
    let per_row = got.pool_accesses as f64 / ROWS as f64;
    assert!(
        per_row <= 1.5,
        "a fold over {ROWS} rows touched the buffer pool {} times ({per_row:.2} per row); \
         the bound is 1.5 -- one pin per leaf plus the index walk, not a root-to-leaf \
         descent per row",
        got.pool_accesses
    );
}

/// The HASHED shape reads its rows the same way: the radius-free case here is
/// a group over an UNINDEXED expression of the driving walk, which forces
/// every group open at once and still reads each row once, in entity order.
#[test]
fn a_hashed_fold_reads_its_rows_in_entity_order_too() {
    let (_dir, f) = open();
    let expected = fold_by_kind(&f.rows);
    let accumulators = [Accumulator {
        function: AggregateFn::Sum,
        input: Some(AggregateInput::Index(f.index.born)),
    }];
    let got = run(
        &f.db,
        AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::Field("kind")),
            accumulators: &accumulators,
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        },
        8_192,
        QueryBudget::unlimited(),
    );
    assert_eq!(got.shape, AggregateShape::Hashed);
    assert_eq!(got.groups.len(), expected.len());
    for (group, want) in got.groups.iter().zip(&expected) {
        assert_eq!(text_key(group), want.key);
        assert_eq!(int_of(&group.values[0]), want.sum);
    }
    let per_row = got.pool_accesses as f64 / ROWS as f64;
    assert!(
        per_row <= 1.5,
        "a hashed fold over {ROWS} rows touched the buffer pool {} times ({per_row:.2} per row)",
        got.pool_accesses
    );
}

/// The row-order read is a REORDERING, not a widening: a candidate the fold
/// would not have read is not read. A page of one group reads that group's
/// rows and stops, which is what stopping at a group boundary is for. The
/// column is spelled as a FIELD, so this is the streaming row path and not
/// the posting join.
#[test]
fn a_page_of_one_group_reads_only_that_group_s_rows() {
    let (_dir, f) = open();
    let expected = fold_by_kind(&f.rows);
    let accumulators = [Accumulator {
        function: AggregateFn::Sum,
        input: Some(AggregateInput::Field("born")),
    }];
    let mut prepared = f
        .db
        .prepare_aggregate(AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::Index(f.index.kind)),
            accumulators: &accumulators,
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        })
        .unwrap();
    let page = prepared
        .next_page(1, QueryBudget::unlimited(), || false)
        .unwrap();
    assert_eq!(page.groups.len(), 1);
    assert_eq!(text_key(&page.groups[0]), expected[0].key);
    assert_eq!(int_of(&page.groups[0].values[0]), expected[0].sum);
    // The first group's own rows, and at most one batch of look-ahead past
    // the boundary -- never the whole collection.
    assert!(
        page.work.primary_reads < ROWS as u64 / 2,
        "a page of one group of eight read {} of {ROWS} rows",
        page.work.primary_reads
    );
}

// ── the same walk, asked as a plain query, for the counter baseline ───────

/// The reference the row-order claim is measured against: the identical set
/// of rows, read by the ordinary page path, which already batches. If this
/// ever costs less per row than the fold above, the fold is leaving something
/// on the table.
#[test]
fn the_fold_reads_rows_no_less_efficiently_than_the_page_path() {
    let (_dir, f) = open();
    let before = f.db.pool_accesses().unwrap();
    let mut prepared = f
        .db
        .prepare_query(QueryRequest {
            collection: f.place,
            filters: &[QueryFilter::Scalar {
                index: f.index.born,
                predicate: ScalarFilter::Range {
                    lower: Bound::Unbounded,
                    upper: Bound::Unbounded,
                },
            }],
            order: QueryOrder::Scalar {
                index: f.index.kind,
                direction: sekejap_core::collections::SortDirection::Ascending,
            },
            projection: Projection::Fields(&["born"]),
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let mut rows = 0u64;
    loop {
        let page = prepared
            .next_page(8_192, QueryBudget::unlimited(), || false)
            .unwrap();
        rows += page.rows.len() as u64;
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    let accesses = f.db.pool_accesses().unwrap() - before;
    assert_eq!(rows, ROWS as u64);
    let per_row = accesses as f64 / ROWS as f64;
    assert!(
        per_row <= 3.0,
        "the page path itself costs {per_row:.2} pool accesses per row"
    );
}

// ── 5. the POSTING JOIN: two index passes and no row ──────────────────────
//
// `sum(born) GROUP BY kind` with `born` INDEXED is 50,000 random point-gets
// of a 1.2 KB row into a 61 MB tree for one integer field. Both columns are
// indexed, so both can be walked instead: pass 1 turns each `kind` value's
// contiguous run of postings into a group with a bounded id bitmap, pass 2
// walks the `born` postings in key order and folds each `(value, id)` into
// the group whose bitmap claims the id. No row is read.

/// `count(*)`, `sum(born)`, `min(born)`, `max(born)`, `avg(born)` grouped by
/// `kind`, against the brute-force fold the test holds -- and at ZERO
/// `primary_reads`, with `scalar_postings` exactly one pass over `kind` plus
/// one over `born`.
#[test]
fn a_posting_join_answers_the_brute_force_fold_without_reading_a_row() {
    let (_dir, f) = open();
    let expected = fold_by_kind(&f.rows);
    let accumulators = born_accumulators(&f.index);
    let got = run(
        &f.db,
        AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::Index(f.index.kind)),
            accumulators: &accumulators,
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        },
        8_192,
        QueryBudget::unlimited(),
    );
    assert_eq!(got.shape, AggregateShape::PostingJoin);
    assert_eq!(got.groups.len(), expected.len());
    for (group, want) in got.groups.iter().zip(&expected) {
        assert_eq!(text_key(group), want.key);
        assert_eq!(int_of(&group.values[0]), want.count as i64, "count(*)");
        assert_eq!(int_of(&group.values[1]), want.sum, "sum(born)");
        assert_eq!(int_of(&group.values[2]), want.min, "min(born)");
        assert_eq!(int_of(&group.values[3]), want.max, "max(born)");
        let AggValue::F64(average) = group.values[4] else {
            panic!("avg is a real number, found {:?}", group.values[4]);
        };
        let wanted = want.sum as f64 / want.count as f64;
        assert!(
            (average - wanted).abs() <= wanted.abs() * 1e-12,
            "avg(born) {average} against {wanted}"
        );
    }
    assert_eq!(
        got.work.primary_reads, 0,
        "a posting join reads no row: every value it folds came off an index"
    );
    assert_eq!(
        got.work.candidates, ROWS as u64,
        "one candidate per driving posting, as every other shape counts them"
    );
    // One pass over the driving index and one over `born`: four accumulators
    // share the second walk, because what is walked is the COLUMN.
    assert_eq!(
        got.work.scalar_postings,
        ROWS as u64 * 2 + 2,
        "rows x (1 + accumulated columns), plus the one step each walk takes \
         to find its end"
    );
    // One bit per sequence in the collection's span, per group. The span is
    // the highest sequence the collection ever issued, so it is at least the
    // row count and within a few of it here.
    let bitmap = ROWS.div_ceil(8);
    assert!(
        got.work.membership_bytes >= (KINDS.len() * bitmap) as u64
            && got.work.membership_bytes <= (KINDS.len() * (bitmap + 8)) as u64,
        "{} bytes of id bitmap for {} groups of {ROWS} rows, against {bitmap} bytes per group",
        got.work.membership_bytes,
        KINDS.len()
    );
}

/// The same answer as the row path, which is the only claim that matters: the
/// identical request with the columns named as FIELDS reads every row and
/// streams, and the two must agree group for group.
#[test]
fn the_posting_join_and_the_row_reading_fold_answer_identically() {
    let (_dir, f) = open();
    let joined = run(
        &f.db,
        AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::Index(f.index.kind)),
            accumulators: &born_accumulators(&f.index),
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        },
        8_192,
        QueryBudget::unlimited(),
    );
    let read = run(
        &f.db,
        AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::Index(f.index.kind)),
            accumulators: &born_fields(),
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        },
        8_192,
        QueryBudget::unlimited(),
    );
    assert_eq!(joined.shape, AggregateShape::PostingJoin);
    assert_eq!(read.shape, AggregateShape::Streaming);
    assert_eq!(joined.groups, read.groups, "the two shapes disagree");
    assert_eq!(joined.work.primary_reads, 0);
    assert_eq!(read.work.primary_reads, ROWS as u64);
}

/// A NULL-bearing column. `score` is null in one row of five, and a null has
/// no value posting to fold: `count(score)` counts the rest, and `sum`, `min`,
/// `max` and `avg` ignore them -- which is SQL's rule, and is what the row
/// path answers for the same rows.
#[test]
fn a_null_bearing_column_counts_only_the_rows_that_have_a_value() {
    let (_dir, f) = open();
    let accumulators = [
        Accumulator {
            function: AggregateFn::CountStar,
            input: None,
        },
        Accumulator {
            function: AggregateFn::Count,
            input: Some(AggregateInput::Index(f.index.score)),
        },
        Accumulator {
            function: AggregateFn::Sum,
            input: Some(AggregateInput::Index(f.index.score)),
        },
        Accumulator {
            function: AggregateFn::Min,
            input: Some(AggregateInput::Index(f.index.score)),
        },
        Accumulator {
            function: AggregateFn::Max,
            input: Some(AggregateInput::Index(f.index.score)),
        },
    ];
    let got = run(
        &f.db,
        AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::Index(f.index.kind)),
            accumulators: &accumulators,
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        },
        8_192,
        QueryBudget::unlimited(),
    );
    assert_eq!(got.shape, AggregateShape::PostingJoin);
    assert_eq!(got.work.primary_reads, 0);

    let expected = fold_score_by_kind(&f.rows);
    assert_eq!(got.groups.len(), expected.len());
    let mut nulls = 0u64;
    for (group, want) in got.groups.iter().zip(&expected) {
        assert_eq!(text_key(group), want.key);
        assert_eq!(int_of(&group.values[0]), want.rows as i64, "count(*)");
        assert_eq!(int_of(&group.values[1]), want.scored as i64, "count(score)");
        assert_eq!(int_of(&group.values[2]), want.sum, "sum(score)");
        assert_eq!(int_of(&group.values[3]), want.min, "min(score)");
        assert_eq!(int_of(&group.values[4]), want.max, "max(score)");
        nulls += want.rows - want.scored;
    }
    assert!(
        nulls > 0,
        "the corpus carries no null score, so this test proves nothing"
    );
}

/// `HAVING` is applied to a FINISHED group before paging, exactly as it is
/// under the hashed fold: a rejected group never occupies a row of a page.
#[test]
fn a_having_over_a_posting_join_drops_whole_groups_before_paging() {
    let (_dir, f) = open();
    let accumulators = born_accumulators(&f.index);
    let full = fold_by_kind(&f.rows);
    // A threshold the corpus straddles: some kinds above it, some below.
    let floor = {
        let mut counts: Vec<u64> = full.iter().map(|group| group.count).collect();
        counts.sort_unstable();
        counts[counts.len() / 2] as f64
    };
    let having = [GroupPredicate {
        accumulator: 0,
        op: GroupCmp::Gt,
        value: floor,
    }];
    let got = run(
        &f.db,
        AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::Index(f.index.kind)),
            accumulators: &accumulators,
            having: &having,
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        },
        8_192,
        QueryBudget::unlimited(),
    );
    assert_eq!(got.shape, AggregateShape::PostingJoin);
    let expected: Vec<&Folded> = full
        .iter()
        .filter(|group| group.count as f64 > floor)
        .collect();
    assert!(
        !expected.is_empty() && expected.len() < full.len(),
        "the HAVING kept {} of {} groups, so it tests nothing",
        expected.len(),
        full.len()
    );
    assert_eq!(got.groups.len(), expected.len());
    for (group, want) in got.groups.iter().zip(&expected) {
        assert_eq!(text_key(group), want.key);
        assert_eq!(int_of(&group.values[1]), want.sum);
    }
}

/// Groups are FINISHED before paging, so pages of one, three and every group
/// at once concatenate to the same answer -- and the fold happens once,
/// whatever the page size.
#[test]
fn posting_join_pages_concatenate_to_the_single_page_answer() {
    let (_dir, f) = open();
    let accumulators = born_accumulators(&f.index);
    let request = || AggregateRequest {
        collection: f.place,
        filters: &[],
        group: Some(GroupKey::Index(f.index.kind)),
        accumulators: &accumulators,
        having: &[],
        order: GroupOrder::Key,
        driver: CandidateDriver::Auto,
        total_limit: None,
    };
    let whole = run(&f.db, request(), 8_192, QueryBudget::unlimited());
    for page_size in [1usize, 3, 7] {
        let paged = run(&f.db, request(), page_size, QueryBudget::unlimited());
        assert_eq!(
            paged.groups, whole.groups,
            "pages of {page_size} disagree with the single page"
        );
        assert_eq!(
            paged.work.scalar_postings, whole.work.scalar_postings,
            "pages of {page_size} walked the indexes more than once"
        );
        assert_eq!(paged.work.primary_reads, 0);
    }
}

/// THE BOUND, AND WHAT IT IS NOT.
///
/// The join holds one id bitmap per group, `ceil(span / 8)` bytes each, and
/// the page promises to hold no more than `RUN_BYTES` of them. Grouping by a
/// column with one value PER ROW asks for 20,000 of them -- 50 MB against an
/// 8 MiB promise.
///
/// Past that bound the join GIVES WAY to the fold that ran before it existed
/// and answers anyway. It is not a refusal, and that is the point: a request
/// that streamed inside ONE accumulator set yesterday must not become a
/// `BudgetExceeded` today because the engine learnt a new shape
/// (`docs/lang/QL_CONTRACT.md` section 6).
#[test]
fn past_its_bitmap_bound_the_posting_join_gives_way_and_still_answers() {
    let (_dir, f) = open();
    let accumulators = [Accumulator {
        function: AggregateFn::Sum,
        input: Some(AggregateInput::Index(f.index.born)),
    }];
    let mut prepared = f
        .db
        .prepare_aggregate(AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::Index(f.index.tick)),
            accumulators: &accumulators,
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        })
        .unwrap();
    assert_eq!(
        prepared.shape(),
        AggregateShape::PostingJoin,
        "prepare cannot know how many groups there will be, so it chooses the join"
    );
    let mut groups = Vec::new();
    loop {
        let page = prepared
            .next_page(8_192, QueryBudget::unlimited(), || false)
            .expect("a bound is not a refusal");
        let empty = page.groups.is_empty();
        groups.extend(page.groups);
        if page.done || empty {
            break;
        }
    }
    assert_eq!(
        prepared.shape(),
        AggregateShape::Streaming,
        "the join gave way to the fold this request had before it existed"
    );
    // `tick` is the row ordinal, so every row is its own group and the sum of
    // a group is that row's own `born`.
    assert_eq!(groups.len(), ROWS);
    for (at, group) in groups.iter().enumerate() {
        assert_eq!(group.key, Some(OwnedScalarValue::I64(at as i64)));
        assert_eq!(int_of(&group.values[0]), f.rows[at].born);
    }
}

/// The CALLER's own `groups` budget is the second bound, and it gives way the
/// same way. A budget of one accumulator set is what a streaming fold has
/// always been able to answer this question inside; the join wants one per
/// group, so it stands down rather than refuse.
#[test]
fn a_groups_budget_of_one_makes_the_posting_join_give_way_rather_than_refuse() {
    let (_dir, f) = open();
    let expected = fold_by_kind(&f.rows);
    let accumulators = born_accumulators(&f.index);
    let mut prepared = f
        .db
        .prepare_aggregate(AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::Index(f.index.kind)),
            accumulators: &accumulators,
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        })
        .unwrap();
    assert_eq!(prepared.shape(), AggregateShape::PostingJoin);
    let tight = QueryBudget {
        groups: 1,
        ..QueryBudget::unlimited()
    };
    let mut groups = Vec::new();
    loop {
        let page = prepared
            .next_page(8_192, tight, || false)
            .expect("a streaming fold holds ONE accumulator set, and this one may too");
        let empty = page.groups.is_empty();
        assert_eq!(page.work.groups, 1, "one accumulator set is alive");
        groups.extend(page.groups);
        if page.done || empty {
            break;
        }
    }
    assert_eq!(prepared.shape(), AggregateShape::Streaming);
    assert_eq!(groups.len(), expected.len());
    for (group, want) in groups.iter().zip(&expected) {
        assert_eq!(text_key(group), want.key);
        assert_eq!(int_of(&group.values[1]), want.sum);
    }
}

/// The join is chosen for what it WINS, and nothing else. With no accumulator
/// that would otherwise read a row, the streaming fold already costs zero
/// `primary_reads` and holds one accumulator set, so spending a bitmap per
/// group would buy nothing: `count(*) GROUP BY kind` still streams.
#[test]
fn an_aggregate_that_reads_no_row_already_is_left_streaming() {
    let (_dir, f) = open();
    let got = run(
        &f.db,
        AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::Index(f.index.kind)),
            accumulators: &COUNT_STAR,
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        },
        8_192,
        QueryBudget::unlimited(),
    );
    assert_eq!(got.shape, AggregateShape::Streaming);
    assert_eq!(got.work.primary_reads, 0);
    assert_eq!(got.work.membership_bytes, 0, "no bitmap was built");
}

/// A FILTER takes the join off the table: pass 1 is the driving index's own
/// full forward walk, and a filter would have to reject candidates whose ids
/// the bitmaps have already claimed -- some of them by reading the row this
/// shape exists to avoid. The answer is the one the row-reading fold gives.
#[test]
fn a_filter_beside_the_group_key_leaves_the_posting_join_unchosen() {
    let (_dir, f) = open();
    let accumulators = born_accumulators(&f.index);
    let lower = 1_900 + BORN_VALUES / 2;
    let got = run(
        &f.db,
        AggregateRequest {
            collection: f.place,
            filters: &[QueryFilter::Scalar {
                index: f.index.born,
                predicate: ScalarFilter::Range {
                    lower: Bound::Included(ScalarValue::I64(lower)),
                    upper: Bound::Unbounded,
                },
            }],
            group: Some(GroupKey::Index(f.index.kind)),
            accumulators: &accumulators,
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        },
        8_192,
        QueryBudget::unlimited(),
    );
    assert_ne!(got.shape, AggregateShape::PostingJoin);
    let mut expected: BTreeMap<&str, (u64, i64)> = BTreeMap::new();
    for row in f.rows.iter().filter(|row| row.born >= lower) {
        let slot = expected.entry(row.kind).or_insert((0, 0));
        slot.0 += 1;
        slot.1 += row.born;
    }
    assert_eq!(got.groups.len(), expected.len());
    for (group, (key, (count, sum))) in got.groups.iter().zip(&expected) {
        assert_eq!(&text_key(group), key);
        assert_eq!(int_of(&group.values[0]), *count as i64);
        assert_eq!(int_of(&group.values[1]), *sum);
    }
}
