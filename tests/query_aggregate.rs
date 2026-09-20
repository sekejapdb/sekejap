//! The aggregation atomic against a brute-force fold of the same 2,000 rows.
//!
//! `docs/QL_CONTRACT.md` §4.7 says `count(*)`, `count(col)`, `sum`, `min`,
//! `max`, `avg`, `GROUP BY`, `HAVING` and `DISTINCT` compile to one atomic
//! that STREAMS when the group key is the driving index's own order and
//! HASHES otherwise. This file is that claim under test, in the shape the
//! item's brief names:
//!
//! * every combination of {no filter, scalar range, point radius, text} x
//!   {no group, indexed group, unindexed group} x the six accumulators equals
//!   a fold written here over `fixture::Fixture::rows`;
//! * the STREAMING and the HASHED shapes of the same question produce the
//!   same groups;
//! * a groups budget of one less than the distinct groups is
//!   `BudgetExceeded { groups }` -- never a spill, and never a wrong answer;
//! * pages of 1, 3 and 7 concatenate to the single-page answer;
//! * `HAVING` filters finished groups, and `DISTINCT` is a group with no
//!   accumulators;
//! * a fold cancelled part-way leaves no state: the retry asks the identical
//!   question and gets the identical answer.

#[path = "sqlslice/fixture.rs"]
mod fixture;

use e4_prototype::{
    collections::{
        Accumulator, AggValue, AggregateFn, AggregateInput, AggregateRequest, AggregateShape,
        CandidateDriver, GroupCmp, GroupKey, GroupOrder, GroupPredicate, GroupRow, IndexId,
        OwnedScalarValue, PointFilter, QueryBudget, QueryError, QueryFilter, ScalarFilter,
        ScalarValue, TextMatch, WorkResource, CollectionOptions,
    },
    spatial_math::{within_radius, Point},
    Kind,
};
use serde_json::json;
use std::ops::Bound;
use tempfile::TempDir;

const RADIUS_METRES: f64 = 18_000.0;
const BORN_LOWER: i64 = 19_520_101;
const BORN_UPPER: i64 = 19_800_101;
const TERM: &str = "kopi";

fn open() -> (TempDir, fixture::Fixture) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let f = fixture::build(&path);
    (dir, f)
}

// ── the questions, as both a request and a fold ───────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Which {
    None,
    BornRange,
    Radius,
    Text,
}

impl Which {
    fn all() -> [Which; 4] {
        [Which::None, Which::BornRange, Which::Radius, Which::Text]
    }

    fn filters(self, index: &fixture::Indexes) -> Vec<QueryFilter<'static>> {
        match self {
            Which::None => Vec::new(),
            Which::BornRange => vec![QueryFilter::Scalar {
                index: index.born,
                predicate: ScalarFilter::Range {
                    lower: Bound::Included(ScalarValue::I64(BORN_LOWER)),
                    upper: Bound::Included(ScalarValue::I64(BORN_UPPER)),
                },
            }],
            Which::Radius => vec![QueryFilter::Point {
                index: index.loc,
                predicate: PointFilter::Radius {
                    center: fixture::centre(),
                    radius_metres: RADIUS_METRES,
                },
            }],
            Which::Text => vec![QueryFilter::Text {
                index: index.text,
                query: TERM,
                matching: TextMatch::Any,
            }],
        }
    }

    /// The same predicate, decided against the generated row itself.
    fn admits(self, row: &fixture::Row) -> bool {
        match self {
            Which::None => true,
            Which::BornRange => (BORN_LOWER..=BORN_UPPER).contains(&row.born),
            Which::Radius => {
                let point = Point::new(row.lon, row.lat).unwrap();
                within_radius(fixture::centre(), point, RADIUS_METRES).unwrap()
            }
            // Analyzer v1 folds to lower-case alphanumeric runs, and every
            // word of this corpus is already one.
            Which::Text => row.text.split_whitespace().any(|word| word == TERM),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Grouping {
    /// One group over every candidate.
    None,
    /// `kind`, which has a scalar index: index-side, and streaming when
    /// nothing else drives.
    IndexedKind,
    /// `tag`, which has none: the value comes from the row, so the shape is
    /// hashed whatever drives.
    UnindexedTag,
    /// `kind` named as a FIELD rather than as its index -- the same answer
    /// off the row path, which is how the two shapes are compared.
    FieldKind,
}

impl Grouping {
    fn key(self, index: &fixture::Indexes) -> Option<GroupKey<'static>> {
        match self {
            Grouping::None => None,
            Grouping::IndexedKind => Some(GroupKey::Index(index.kind)),
            Grouping::UnindexedTag => Some(GroupKey::Field("tag")),
            Grouping::FieldKind => Some(GroupKey::Field("kind")),
        }
    }

    fn of(self, row: &fixture::Row) -> Option<OwnedScalarValue> {
        match self {
            Grouping::None => None,
            Grouping::IndexedKind | Grouping::FieldKind => {
                Some(OwnedScalarValue::Text(row.kind.clone()))
            }
            Grouping::UnindexedTag => Some(match &row.tag {
                Some(tag) => OwnedScalarValue::Text(tag.clone()),
                None => OwnedScalarValue::Nullish,
            }),
        }
    }
}

/// The six accumulators, in one fixed order, so a fold and a request cannot
/// drift apart: count(*), count(score), sum(born), min(born), max(born),
/// avg(born).
fn accumulators(index: &fixture::Indexes) -> Vec<Accumulator<'static>> {
    vec![
        Accumulator {
            function: AggregateFn::CountStar,
            input: None,
        },
        Accumulator {
            function: AggregateFn::Count,
            input: Some(AggregateInput::Index(index.score)),
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

/// One group, as the fold below builds it.
#[derive(Clone, Debug)]
struct Folded {
    key: Option<OwnedScalarValue>,
    rows: u64,
    scores: u64,
    sum: i64,
    min: Option<i64>,
    max: Option<i64>,
}

fn key_order(left: &Option<OwnedScalarValue>, right: &Option<OwnedScalarValue>) -> std::cmp::Ordering {
    match (left, right) {
        (Some(OwnedScalarValue::Text(a)), Some(OwnedScalarValue::Text(b))) => a.cmp(b),
        (Some(OwnedScalarValue::Nullish), Some(OwnedScalarValue::Text(_))) => {
            std::cmp::Ordering::Less
        }
        (Some(OwnedScalarValue::Text(_)), Some(OwnedScalarValue::Nullish)) => {
            std::cmp::Ordering::Greater
        }
        _ => std::cmp::Ordering::Equal,
    }
}

/// The brute-force answer: fold the generated rows themselves, in key order.
fn fold(rows: &[fixture::Row], which: Which, grouping: Grouping) -> Vec<Folded> {
    let mut out: Vec<Folded> = Vec::new();
    for row in rows {
        if !which.admits(row) {
            continue;
        }
        let key = grouping.of(row);
        let at = match out.iter().position(|group| group.key == key) {
            Some(at) => at,
            None => {
                out.push(Folded {
                    key,
                    rows: 0,
                    scores: 0,
                    sum: 0,
                    min: None,
                    max: None,
                });
                out.len() - 1
            }
        };
        let group = &mut out[at];
        group.rows += 1;
        if row.score.is_some() {
            group.scores += 1;
        }
        group.sum += row.born;
        group.min = Some(group.min.map_or(row.born, |old| old.min(row.born)));
        group.max = Some(group.max.map_or(row.born, |old| old.max(row.born)));
    }
    out.sort_by(|a, b| key_order(&a.key, &b.key));
    out
}

fn int_of(value: &AggValue) -> i64 {
    match value {
        AggValue::Count(n) => *n as i64,
        AggValue::I64(v) => *v,
        other => panic!("expected a whole number, found {other:?}"),
    }
}

/// Every group of a prepared aggregate, in pages of `page_size`.
fn drain(
    prepared: &mut e4_prototype::collections::PreparedAggregate<'_>,
    page_size: usize,
    budget: QueryBudget,
) -> Result<Vec<GroupRow>, QueryError> {
    let mut out = Vec::new();
    loop {
        let page = prepared.next_page(page_size, budget, || false)?;
        let empty = page.groups.is_empty();
        out.extend(page.groups);
        if page.done || empty {
            break;
        }
    }
    Ok(out)
}

fn run(
    f: &fixture::Fixture,
    which: Which,
    grouping: Grouping,
    page_size: usize,
) -> (Vec<GroupRow>, AggregateShape) {
    let filters = which.filters(&f.index);
    let accumulators = accumulators(&f.index);
    let mut prepared = f
        .db
        .prepare_aggregate(AggregateRequest {
            collection: f.place,
            filters: &filters,
            group: grouping.key(&f.index),
            accumulators: &accumulators,
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        })
        .unwrap();
    let shape = prepared.shape();
    let groups = drain(&mut prepared, page_size, QueryBudget::unlimited()).unwrap();
    (groups, shape)
}

fn check(f: &fixture::Fixture, which: Which, grouping: Grouping, groups: &[GroupRow]) {
    let expected = fold(&f.rows, which, grouping);
    assert_eq!(
        groups.len(),
        expected.len(),
        "{which:?} / {grouping:?}: group count"
    );
    for (got, want) in groups.iter().zip(&expected) {
        assert_eq!(got.key, want.key, "{which:?} / {grouping:?}: group key");
        assert_eq!(
            int_of(&got.values[0]),
            want.rows as i64,
            "{which:?} / {grouping:?} / {:?}: count(*)",
            want.key
        );
        assert_eq!(
            int_of(&got.values[1]),
            want.scores as i64,
            "{which:?} / {grouping:?} / {:?}: count(score)",
            want.key
        );
        assert_eq!(
            int_of(&got.values[2]),
            want.sum,
            "{which:?} / {grouping:?} / {:?}: sum(born)",
            want.key
        );
        assert_eq!(
            int_of(&got.values[3]),
            want.min.unwrap(),
            "{which:?} / {grouping:?} / {:?}: min(born)",
            want.key
        );
        assert_eq!(
            int_of(&got.values[4]),
            want.max.unwrap(),
            "{which:?} / {grouping:?} / {:?}: max(born)",
            want.key
        );
        let AggValue::F64(average) = got.values[5] else {
            panic!("avg is a real number, found {:?}", got.values[5]);
        };
        let wanted = want.sum as f64 / want.rows as f64;
        assert!(
            (average - wanted).abs() <= wanted.abs() * 1e-12,
            "{which:?} / {grouping:?} / {:?}: avg(born) {average} against {wanted}",
            want.key
        );
    }
}

#[test]
fn every_filter_and_grouping_equals_a_brute_force_fold() {
    let (_dir, f) = open();
    for which in Which::all() {
        for grouping in [
            Grouping::None,
            Grouping::IndexedKind,
            Grouping::UnindexedTag,
        ] {
            let (groups, _) = run(&f, which, grouping, 8_192);
            assert!(
                !groups.is_empty(),
                "{which:?} / {grouping:?} admitted no rows, so it tests nothing"
            );
            check(&f, which, grouping, &groups);
        }
    }
}

/// The two shapes of the SAME question. `kind` named as its index streams
/// when nothing else drives; `kind` named as a field reads the row and
/// hashes. The groups must be identical.
#[test]
fn streaming_and_hashed_produce_identical_groups() {
    let (_dir, f) = open();
    for which in Which::all() {
        let (streamed, streamed_shape) = run(&f, which, Grouping::IndexedKind, 8_192);
        let (hashed, hashed_shape) = run(&f, which, Grouping::FieldKind, 8_192);
        assert_eq!(
            hashed_shape,
            AggregateShape::Hashed,
            "{which:?}: a row-side group key is always hashed"
        );
        if which == Which::None {
            assert_eq!(
                streamed_shape,
                AggregateShape::Streaming,
                "with no filter, the group index is the only thing that can drive"
            );
        }
        assert_eq!(streamed, hashed, "{which:?}: the two shapes disagree");
    }
}

/// One accumulator set is alive under STREAMING whatever the collection
/// holds, and one per distinct group under HASHED. The budget says so.
#[test]
fn the_groups_budget_bounds_the_hashed_shape_and_streaming_holds_one() {
    let (_dir, f) = open();
    let distinct = fixture::KINDS.len() as u64;
    let accumulators = accumulators(&f.index);

    let tight = QueryBudget {
        groups: distinct - 1,
        ..QueryBudget::unlimited()
    };
    let mut hashed = f
        .db
        .prepare_aggregate(AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::Field("kind")),
            accumulators: &accumulators,
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        })
        .unwrap();
    assert_eq!(hashed.shape(), AggregateShape::Hashed);
    match drain(&mut hashed, 8_192, tight) {
        Err(QueryError::BudgetExceeded {
            resource: WorkResource::Groups,
            limit,
            attempted,
        }) => {
            assert_eq!(limit, distinct - 1);
            assert_eq!(attempted, distinct);
        }
        other => panic!("a hashed fold over {distinct} groups under a budget of {} must be refused, got {other:?}", distinct - 1),
    }

    // The same question, streamed: the walk holds ONE set, so a budget of one
    // answers all eight groups.
    let mut streamed = f
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
    assert_eq!(streamed.shape(), AggregateShape::Streaming);
    let groups = drain(
        &mut streamed,
        8_192,
        QueryBudget {
            groups: 1,
            ..QueryBudget::unlimited()
        },
    )
    .expect("a streaming fold holds one accumulator set");
    assert_eq!(groups.len(), distinct as usize);
}

/// A refused page leaves nothing behind: the aggregate is exactly as it was,
/// and a retry under a budget that can afford it answers in full.
#[test]
fn a_refused_fold_leaves_no_state() {
    let (_dir, f) = open();
    let accumulators = accumulators(&f.index);
    let mut prepared = f
        .db
        .prepare_aggregate(AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::Field("kind")),
            accumulators: &accumulators,
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        })
        .unwrap();
    let refused = drain(
        &mut prepared,
        8_192,
        QueryBudget {
            groups: 1,
            ..QueryBudget::unlimited()
        },
    );
    assert!(refused.is_err(), "a hashed fold of eight groups needs eight");
    let groups = drain(&mut prepared, 8_192, QueryBudget::unlimited()).unwrap();
    check(&f, Which::None, Grouping::FieldKind, &groups);
}

#[test]
fn pages_of_one_three_and_seven_concatenate() {
    let (_dir, f) = open();
    for grouping in [
        Grouping::IndexedKind,
        Grouping::UnindexedTag,
        Grouping::None,
    ] {
        let (whole, _) = run(&f, Which::None, grouping, 8_192);
        for page_size in [1usize, 3, 7] {
            let (paged, _) = run(&f, Which::None, grouping, page_size);
            assert_eq!(
                paged, whole,
                "{grouping:?}: pages of {page_size} do not concatenate"
            );
        }
    }
}

/// A streaming page stops at a group boundary, so the number of pages it
/// takes is the number of groups divided by the page size -- not one pass per
/// page over the whole index.
#[test]
fn a_streaming_page_stops_and_resumes_at_a_group_boundary() {
    let (_dir, f) = open();
    let accumulators = accumulators(&f.index);
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
    assert_eq!(prepared.shape(), AggregateShape::Streaming);
    let first = prepared
        .next_page(2, QueryBudget::unlimited(), || false)
        .unwrap();
    assert_eq!(first.groups.len(), 2);
    assert!(!first.done);
    assert_eq!(first.work.groups, 1, "one accumulator set is alive");
    // A page of two of eight groups reads about a quarter of the collection,
    // not all of it: that is what stopping at a boundary buys.
    assert!(
        first.work.candidates < fixture::ROWS as u64 / 2,
        "a streaming page walked {} candidates of {}",
        first.work.candidates,
        fixture::ROWS
    );
    let rest = drain(&mut prepared, 8_192, QueryBudget::unlimited()).unwrap();
    assert_eq!(first.groups.len() + rest.len(), fixture::KINDS.len());
}

#[test]
fn having_filters_finished_groups_before_they_are_paged() {
    let (_dir, f) = open();
    let expected = fold(&f.rows, Which::None, Grouping::IndexedKind);
    let accumulators = vec![Accumulator {
        function: AggregateFn::CountStar,
        input: None,
    }];
    let having = [GroupPredicate {
        accumulator: 0,
        op: GroupCmp::Gt,
        value: 249.0,
    }];
    let mut prepared = f
        .db
        .prepare_aggregate(AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::Index(f.index.kind)),
            accumulators: &accumulators,
            having: &having,
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        })
        .unwrap();
    let groups = drain(&mut prepared, 8_192, QueryBudget::unlimited()).unwrap();
    let want: Vec<_> = expected.iter().filter(|group| group.rows > 249).collect();
    assert_eq!(groups.len(), want.len());
    for (got, expect) in groups.iter().zip(&want) {
        assert_eq!(got.key, expect.key);
        assert_eq!(int_of(&got.values[0]), expect.rows as i64);
    }
}

/// DISTINCT is a group with no accumulators, and it is the same set of keys
/// the grouped fold produces.
#[test]
fn distinct_is_a_group_with_no_accumulators() {
    let (_dir, f) = open();
    for (grouping, key) in [
        (Grouping::IndexedKind, GroupKey::Index(f.index.kind)),
        (Grouping::UnindexedTag, GroupKey::Field("tag")),
    ] {
        let mut prepared = f
            .db
            .prepare_aggregate(AggregateRequest {
                collection: f.place,
                filters: &[],
                group: Some(key),
                accumulators: &[],
                having: &[],
                order: GroupOrder::Key,
                driver: CandidateDriver::Auto,
                total_limit: None,
            })
            .unwrap();
        let groups = drain(&mut prepared, 8_192, QueryBudget::unlimited()).unwrap();
        let expected = fold(&f.rows, Which::None, grouping);
        assert_eq!(groups.len(), expected.len(), "{grouping:?}");
        for (got, want) in groups.iter().zip(&expected) {
            assert_eq!(got.key, want.key, "{grouping:?}");
            assert!(got.values.is_empty(), "DISTINCT accumulates nothing");
        }
    }
}

/// `count(*)` with no group and no filter is one group, and the plan names
/// the mapping keyspace -- the same driver the `count_all` case of `popsim`
/// and `q7_budget` has always used.
#[test]
fn count_all_is_one_group_over_the_key_order_driver() {
    let (_dir, f) = open();
    let accumulators = vec![Accumulator {
        function: AggregateFn::CountStar,
        input: None,
    }];
    let mut prepared = f
        .db
        .prepare_aggregate(AggregateRequest {
            collection: f.place,
            filters: &[],
            group: None,
            accumulators: &accumulators,
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        })
        .unwrap();
    let plan = prepared.describe();
    assert_eq!(plan.query.driver, e4_prototype::collections::QueryDriver::Keys);
    let page = prepared
        .next_page(8_192, QueryBudget::unlimited(), || false)
        .unwrap();
    assert!(page.done);
    assert_eq!(page.groups.len(), 1);
    assert_eq!(page.groups[0].key, None);
    assert_eq!(int_of(&page.groups[0].values[0]), fixture::ROWS as i64);
    assert_eq!(
        page.work.primary_reads, 0,
        "count(*) over the mapping keyspace reads no primary row"
    );
}

/// A cancelled fold leaves no state behind: the retry asks the identical
/// question and gets the identical answer.
#[test]
fn cancellation_mid_fold_leaves_no_state() {
    let (_dir, f) = open();
    let accumulators = accumulators(&f.index);
    for group in [
        Some(GroupKey::Index(f.index.kind)),
        Some(GroupKey::Field("kind")),
        None,
    ] {
        let mut prepared = f
            .db
            .prepare_aggregate(AggregateRequest {
                collection: f.place,
                filters: &[],
                group,
                accumulators: &accumulators,
                having: &[],
                order: GroupOrder::Key,
                driver: CandidateDriver::Auto,
                total_limit: None,
            })
            .unwrap();
        let mut seen = 0usize;
        let error = prepared
            .next_page(8_192, QueryBudget::unlimited(), || {
                seen += 1;
                seen > 500
            })
            .expect_err("a fold cancelled part-way must not return an answer");
        assert!(matches!(error, QueryError::Cancelled), "{error:?}");
        let after = drain(&mut prepared, 8_192, QueryBudget::unlimited()).unwrap();
        let grouping = match group {
            Some(GroupKey::Index(_)) | Some(GroupKey::Field("kind")) => Grouping::FieldKind,
            None => Grouping::None,
            _ => unreachable!(),
        };
        check(&f, Which::None, grouping, &after);
    }
}

/// The one grouping EXPRESSION: `born / 10000`, computed index-side from the
/// Int posting, which is what keeps its groups contiguous.
#[test]
fn a_divided_group_key_streams_and_equals_the_fold() {
    let (_dir, f) = open();
    let accumulators = vec![Accumulator {
        function: AggregateFn::CountStar,
        input: None,
    }];
    let mut prepared = f
        .db
        .prepare_aggregate(AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::IndexDiv {
                index: f.index.born,
                divisor: 10_000,
            }),
            accumulators: &accumulators,
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        })
        .unwrap();
    assert_eq!(prepared.shape(), AggregateShape::Streaming);
    let groups = drain(&mut prepared, 3, QueryBudget::unlimited()).unwrap();

    let mut expected: Vec<(i64, u64)> = Vec::new();
    for row in &f.rows {
        let key = row.born / 10_000;
        match expected.iter_mut().find(|(k, _)| *k == key) {
            Some((_, n)) => *n += 1,
            None => expected.push((key, 1)),
        }
    }
    expected.sort_unstable();
    assert_eq!(groups.len(), expected.len());
    for (got, (key, n)) in groups.iter().zip(&expected) {
        assert_eq!(got.key, Some(OwnedScalarValue::I64(*key)));
        assert_eq!(int_of(&got.values[0]), *n as i64);
    }
}

/// `ORDER BY <an aggregate> LIMIT n` is the one place a sort over memory
/// happens, and it is bounded by the groups budget because what it sorts IS
/// the group table.
#[test]
fn an_order_by_an_accumulator_sorts_the_finished_groups() {
    let (_dir, f) = open();
    let accumulators = vec![Accumulator {
        function: AggregateFn::CountStar,
        input: None,
    }];
    let mut prepared = f
        .db
        .prepare_aggregate(AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::Field("tag")),
            accumulators: &accumulators,
            having: &[],
            order: GroupOrder::Accumulator {
                at: 0,
                direction: e4_prototype::collections::SortDirection::Descending,
            },
            driver: CandidateDriver::Auto,
            total_limit: Some(3),
        })
        .unwrap();
    assert_eq!(
        prepared.shape(),
        AggregateShape::Hashed,
        "a ranking over an accumulator cannot be answered before every group is finished"
    );
    let groups = drain(&mut prepared, 8_192, QueryBudget::unlimited()).unwrap();
    assert_eq!(groups.len(), 3, "LIMIT 3 bounds the GROUPS returned");
    let mut counts: Vec<i64> = fold(&f.rows, Which::None, Grouping::UnindexedTag)
        .iter()
        .map(|group| group.rows as i64)
        .collect();
    counts.sort_unstable_by(|a, b| b.cmp(a));
    let got: Vec<i64> = groups.iter().map(|g| int_of(&g.values[0])).collect();
    assert_eq!(got, counts[..3].to_vec());
}

/// A construct with no atomic is refused with a named reason, never emulated
/// (the eighth law).
#[test]
fn what_has_no_atomic_is_refused_by_name() {
    let (_dir, f) = open();
    let count = vec![Accumulator {
        function: AggregateFn::CountStar,
        input: None,
    }];
    // sum over a Text column.
    let bad = vec![Accumulator {
        function: AggregateFn::Sum,
        input: Some(AggregateInput::Index(f.index.kind)),
    }];
    assert!(f
        .db
        .prepare_aggregate(AggregateRequest {
            collection: f.place,
            filters: &[],
            group: None,
            accumulators: &bad,
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        })
        .is_err());
    // A divided group key over a non-Int index.
    assert!(f
        .db
        .prepare_aggregate(AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::IndexDiv {
                index: f.index.kind,
                divisor: 10,
            }),
            accumulators: &count,
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        })
        .is_err());
    // A divisor that is not positive is not monotone, so it cannot stream.
    assert!(f
        .db
        .prepare_aggregate(AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::IndexDiv {
                index: f.index.born,
                divisor: 0,
            }),
            accumulators: &count,
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        })
        .is_err());
    // A field this collection does not declare.
    assert!(f
        .db
        .prepare_aggregate(AggregateRequest {
            collection: f.place,
            filters: &[],
            group: Some(GroupKey::Field("no_such_column")),
            accumulators: &count,
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        })
        .is_err());
    let _ = IndexId(1);
}

/// A group whose accumulator is NULL (every input nullish) is DROPPED by a
/// HAVING on it, as SQL's three-valued logic drops it -- never an error that
/// aborts the statement. And a HAVING over min/max of a Text column is
/// refused at prepare, before any walk.
#[test]
fn having_over_an_all_null_group_drops_it_and_text_extremes_are_refused_at_prepare() {
    let (_dir, mut f) = open();
    let c = f
        .db
        .create_collection(
            "nullagg",
            vec![("kind".into(), Kind::Text), ("born".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    let kind_index = f.db.create_scalar_index(c, "nullagg_kind", "kind", false).unwrap();
    for (key, kind, born) in [
        ("a1", "a", Some(10)),
        ("a2", "a", Some(20)),
        ("b1", "b", None),
        ("b2", "b", None),
    ] {
        let doc = match born {
            Some(b) => json!({"kind": kind, "born": b}),
            None => json!({"kind": kind}),
        };
        f.db.put(c, key, &doc).unwrap();
    }
    f.db.commit().unwrap();
    f.db.build_index_to_ready(kind_index, 256).unwrap();

    let accumulators = [Accumulator {
        function: AggregateFn::Sum,
        input: Some(AggregateInput::Field("born")),
    }];
    let having = [GroupPredicate {
        accumulator: 0,
        op: GroupCmp::Gt,
        value: 0.0,
    }];
    let mut agg = f
        .db
        .prepare_aggregate(AggregateRequest {
            collection: c,
            filters: &[],
            group: Some(GroupKey::Index(kind_index)),
            accumulators: &accumulators,
            having: &having,
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        })
        .unwrap();
    let page = agg.next_page(64, QueryBudget::unlimited(), || false).unwrap();
    assert!(page.done);
    assert_eq!(page.groups.len(), 1, "the all-null group is dropped, not an error: {:?}", page.groups);
    assert_eq!(page.groups[0].key, Some(OwnedScalarValue::Text("a".into())));

    // min over a Text column with a HAVING: refused at prepare.
    let text_min = [Accumulator {
        function: AggregateFn::Min,
        input: Some(AggregateInput::Index(kind_index)),
    }];
    let err = f
        .db
        .prepare_aggregate(AggregateRequest {
            collection: c,
            filters: &[],
            group: None,
            accumulators: &text_min,
            having: &having,
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        })
        .err()
        .expect("refused at prepare");
    assert!(err.to_string().contains("non-numeric"), "{err}");
}
