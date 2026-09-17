//! Two predicates on ONE scalar index are one predicate.
//!
//! A conjunction over a single index is a single posting range. Before the
//! fold the planner let one of the two drive and handed the other the row:
//! `price >= 490 AND price < 500` walked the postings from 490, and then
//! opened the primary record of every candidate it found to ask whether the
//! value it had just read off the posting key was below 500. The answer was
//! never in doubt and the read was never free.
//!
//! Two claims are made here, and they are different claims:
//!
//! 1. the folded plan answers what the unfolded one answered -- checked
//!    against an oracle that evaluates the conjunction over the JSON the rows
//!    were written from, including the empty intersections and the nullish
//!    rows the index cannot tell apart;
//! 2. the folded plan COSTS what the single two-bound range costs -- checked
//!    by counting candidates, primary reads and pager accesses, not by timing.
use e4_prototype::{
    collections::{
        CandidateDriver, CollectionId, Database, IndexId, Projection, QueryBudget, QueryDriver,
        QueryFilter, QueryOrder, QueryRequest, ScalarFilter, ScalarValue,
    },
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;
use std::{
    collections::{BTreeSet, HashMap},
    ops::Bound,
};

const ROWS: u64 = 5_000;
const PAGE: usize = 8192;

fn cfg() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

/// The bench's `price`: ten to four hundred and ninety in steps of ten, so a
/// value repeats every forty-nine rows and `>= 490` is the last bucket alone.
fn price(i: u64) -> f64 {
    10.0 + (i % 49) as f64 * 10.0
}

/// What the row actually carries. `None` is a field that is not there;
/// `Some(None)` is a field that is there and null. Both encode to the same
/// nullish posting key, and the index cannot tell them apart -- which is
/// exactly why a fold must not decide anything on their behalf.
fn stored_price(i: u64) -> Option<Option<f64>> {
    if i % 97 == 0 {
        Some(None)
    } else if i % 89 == 0 {
        None
    } else {
        Some(Some(price(i)))
    }
}

struct Fixture {
    db: Database,
    rows: CollectionId,
    price: IndexId,
    cat: IndexId,
    /// Entity sequence back to the row number the oracle speaks in.
    row_of: HashMap<u64, u64>,
}

fn fixture(dir: &std::path::Path) -> Fixture {
    let mut db = Database::create(dir.join("db"), cfg()).unwrap();
    let rows = db
        .create_collection(
            "v",
            vec![("cat".into(), Kind::Text), ("price".into(), Kind::Real)],
            Default::default(),
        )
        .unwrap();
    db.commit().unwrap();
    const CATS: [&str; 4] = ["cafe", "bar", "gym", "clinic"];
    for i in 1..=ROWS {
        let mut document = json!({ "cat": CATS[(i % 4) as usize] });
        match stored_price(i) {
            Some(Some(value)) => document["price"] = json!(value),
            Some(None) => document["price"] = serde_json::Value::Null,
            None => {}
        }
        db.put(rows, &format!("k{i:08}"), &document).unwrap();
        if i % 256 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    let price = db
        .create_scalar_index(rows, "price_idx", "price", false)
        .unwrap();
    db.build_index_to_ready(price, 256).unwrap();
    let cat = db.create_scalar_index(rows, "cat_idx", "cat", false).unwrap();
    db.build_index_to_ready(cat, 256).unwrap();
    db.commit().unwrap();
    db.checkpoint().unwrap();
    let row_of = (1..=ROWS)
        .map(|i| {
            let entity = db.get(rows, &format!("k{i:08}")).unwrap().unwrap();
            (entity.id.sequence, i)
        })
        .collect();
    Fixture {
        db,
        rows,
        price,
        cat,
        row_of,
    }
}

// ── the oracle ────────────────────────────────────────────────────────────
//
// It knows nothing about scalar keys, bounds encoding or the planner. It
// answers each predicate against the value the row was WRITTEN with, and the
// conjunction by intersecting those answers.

#[derive(Clone, Copy, Debug)]
enum Predicate {
    Eq(f64),
    Range(Bound<f64>, Bound<f64>),
    IsNull,
    IsMissing,
}

impl Predicate {
    fn holds(self, value: Option<Option<f64>>) -> bool {
        match self {
            Self::IsMissing => value.is_none(),
            Self::IsNull => value == Some(None),
            Self::Eq(wanted) => value == Some(Some(wanted)),
            Self::Range(lower, upper) => {
                let Some(Some(value)) = value else {
                    // A range predicate is about VALUES. Neither a null nor a
                    // missing field has one, so neither is ever inside a
                    // range -- not even an unbounded one.
                    return false;
                };
                let above = match lower {
                    Bound::Included(bound) => value >= bound,
                    Bound::Excluded(bound) => value > bound,
                    Bound::Unbounded => true,
                };
                let below = match upper {
                    Bound::Included(bound) => value <= bound,
                    Bound::Excluded(bound) => value < bound,
                    Bound::Unbounded => true,
                };
                above && below
            }
        }
    }

    fn filter(self, index: IndexId) -> QueryFilter<'static> {
        let bound = |bound: Bound<f64>| match bound {
            Bound::Included(value) => Bound::Included(ScalarValue::F64(value)),
            Bound::Excluded(value) => Bound::Excluded(ScalarValue::F64(value)),
            Bound::Unbounded => Bound::Unbounded,
        };
        QueryFilter::Scalar {
            index,
            predicate: match self {
                Self::Eq(value) => ScalarFilter::Eq(ScalarValue::F64(value)),
                Self::Range(lower, upper) => ScalarFilter::Range {
                    lower: bound(lower),
                    upper: bound(upper),
                },
                Self::IsNull => ScalarFilter::IsNull,
                Self::IsMissing => ScalarFilter::IsMissing,
            },
        }
    }
}

fn oracle(predicates: &[Predicate]) -> BTreeSet<u64> {
    (1..=ROWS)
        .filter(|i| {
            let value = stored_price(*i);
            predicates.iter().all(|predicate| predicate.holds(value))
        })
        .collect()
}

/// Drain a query to its keys, with the work it charged along the way.
fn answer(
    fixture: &Fixture,
    filters: &[QueryFilter<'_>],
    driver: CandidateDriver,
) -> (BTreeSet<u64>, u64, u64, QueryDriver) {
    let mut prepared = fixture
        .db
        .prepare_query(QueryRequest {
            collection: fixture.rows,
            filters,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver,
        })
        .unwrap();
    let mut keys = BTreeSet::new();
    let mut candidates = 0;
    let mut primary = 0;
    let mut which = QueryDriver::Entities;
    loop {
        let page = prepared
            .next_page(PAGE, QueryBudget::unlimited(), || false)
            .unwrap();
        candidates += page.work.candidates;
        primary += page.work.primary_reads;
        which = page.driver;
        for row in &page.rows {
            keys.insert(fixture.row_of[&row.id.sequence]);
        }
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    (keys, candidates, primary, which)
}

fn catalogue() -> Vec<Predicate> {
    vec![
        Predicate::Eq(100.0),
        Predicate::Eq(490.0),
        // No row carries this price, so every fold with it is empty.
        Predicate::Eq(55.0),
        Predicate::Range(Bound::Unbounded, Bound::Unbounded),
        Predicate::Range(Bound::Included(490.0), Bound::Excluded(500.0)),
        Predicate::Range(Bound::Excluded(100.0), Bound::Included(300.0)),
        Predicate::Range(Bound::Included(100.0), Bound::Included(300.0)),
        Predicate::Range(Bound::Excluded(400.0), Bound::Unbounded),
        Predicate::Range(Bound::Unbounded, Bound::Excluded(200.0)),
        // Meets on one value that both sides exclude.
        Predicate::Range(Bound::Excluded(200.0), Bound::Excluded(200.0)),
        Predicate::IsNull,
        Predicate::IsMissing,
    ]
}

/// Every pair of predicates on one index, folded, against the oracle.
#[test]
fn folding_two_predicates_on_one_index_keeps_the_answer() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path());
    let catalogue = catalogue();
    let mut empties = 0;
    for left in &catalogue {
        for right in &catalogue {
            let filters = [left.filter(fixture.price), right.filter(fixture.price)];
            let (keys, ..) = answer(&fixture, &filters, CandidateDriver::Auto);
            let wanted = oracle(&[*left, *right]);
            assert_eq!(
                keys, wanted,
                "{left:?} AND {right:?} on one index gave {} keys, the oracle says {}",
                keys.len(),
                wanted.len()
            );
            if wanted.is_empty() {
                empties += 1;
            }
        }
    }
    // The catalogue is only worth running if a good share of it is the empty
    // intersection, which is the case the fold decides without the tree.
    assert!(
        empties >= 60,
        "only {empties} of the {} pairs were empty",
        catalogue.len() * catalogue.len()
    );
}

/// Three on one index, and two on one index beside one on another.
#[test]
fn folding_survives_a_third_predicate_and_a_second_index() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path());
    let catalogue = catalogue();
    for (position, left) in catalogue.iter().enumerate() {
        for right in &catalogue {
            let third = catalogue[(position + 5) % catalogue.len()];
            let filters = [
                left.filter(fixture.price),
                right.filter(fixture.price),
                third.filter(fixture.price),
            ];
            let (keys, ..) = answer(&fixture, &filters, CandidateDriver::Auto);
            assert_eq!(
                keys,
                oracle(&[*left, *right, third]),
                "{left:?} AND {right:?} AND {third:?}"
            );
        }
    }
    // A filter on ANOTHER index must not be folded into the group.
    let filters = [
        Predicate::Range(Bound::Included(100.0), Bound::Included(300.0)).filter(fixture.price),
        QueryFilter::Scalar {
            index: fixture.cat,
            predicate: ScalarFilter::Eq(ScalarValue::Text("cafe")),
        },
        Predicate::Range(Bound::Excluded(200.0), Bound::Unbounded).filter(fixture.price),
    ];
    let (keys, ..) = answer(&fixture, &filters, CandidateDriver::Auto);
    let wanted: BTreeSet<u64> = oracle(&[Predicate::Range(
        Bound::Excluded(200.0),
        Bound::Included(300.0),
    )])
    .into_iter()
    .filter(|i| i % 4 == 0)
    .collect();
    assert_eq!(keys, wanted);
}

/// Positions are user-visible, so the fold must keep them meaningful.
///
/// The rule: the request's own `CandidateDriver::Filter(position)` keeps the
/// folded predicate when it names one of the group, so an explicitly chosen
/// driver still drives; otherwise the lowest position of the group keeps it.
/// Either way naming any position of the group drives the same walk and
/// returns the same answer.
#[test]
fn a_named_driver_position_still_drives_after_folding() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path());
    let filters = [
        Predicate::Range(Bound::Included(490.0), Bound::Unbounded).filter(fixture.price),
        Predicate::Range(Bound::Unbounded, Bound::Excluded(500.0)).filter(fixture.price),
    ];
    let wanted = oracle(&[Predicate::Range(
        Bound::Included(490.0),
        Bound::Excluded(500.0),
    )]);
    assert!(!wanted.is_empty());
    for driver in [
        CandidateDriver::Auto,
        CandidateDriver::Filter(0),
        CandidateDriver::Filter(1),
        CandidateDriver::Entities,
    ] {
        let (keys, _, primary, which) = answer(&fixture, &filters, driver);
        assert_eq!(keys, wanted, "driver {driver:?} changed the answer");
        match driver {
            CandidateDriver::Entities => {
                assert_eq!(which, QueryDriver::Entities);
            }
            _ => {
                assert_eq!(
                    which,
                    QueryDriver::Scalar(fixture.price),
                    "driver {driver:?} did not drive the price index"
                );
                assert_eq!(
                    primary, 0,
                    "driver {driver:?} read {primary} primary records for a question the \
                     posting key answers"
                );
            }
        }
    }
}

/// The counted claim: two filters cost what the one two-bound range costs.
///
/// On the 20,000-row bench `price >= 490 AND price < 500` is 408 rows. As ONE
/// range it was 408 candidates and 408 primary reads at 491.8 ns per row; as
/// TWO filters, 408 candidates and 408 primary reads at 654.0 ns per row --
/// the second predicate opening every candidate's record to re-read a value
/// its posting key had already yielded. Both are now 0 primary reads and
/// about 93 ns per row, which is what the equality walk costs.
///
/// Here the same shape is counted rather than timed: candidates, primary
/// reads and pager accesses, one spelling against the other.
#[test]
fn two_filters_on_one_index_cost_what_one_range_costs() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path());
    let one = [Predicate::Range(Bound::Included(490.0), Bound::Excluded(500.0)).filter(fixture.price)];
    let two = [
        Predicate::Range(Bound::Included(490.0), Bound::Unbounded).filter(fixture.price),
        Predicate::Range(Bound::Unbounded, Bound::Excluded(500.0)).filter(fixture.price),
    ];

    // Warm every cache the first execution fills.
    answer(&fixture, &one, CandidateDriver::Auto);
    answer(&fixture, &two, CandidateDriver::Auto);

    let before = fixture.db.pool_accesses().unwrap();
    let (one_keys, one_candidates, one_primary, _) = answer(&fixture, &one, CandidateDriver::Auto);
    let one_accesses = fixture.db.pool_accesses().unwrap() - before;

    let before = fixture.db.pool_accesses().unwrap();
    let (two_keys, two_candidates, two_primary, _) = answer(&fixture, &two, CandidateDriver::Auto);
    let two_accesses = fixture.db.pool_accesses().unwrap() - before;

    assert_eq!(one_keys, two_keys);
    assert!(!one_keys.is_empty());
    assert_eq!(
        one_primary, 0,
        "the single range read {one_primary} primary records"
    );
    assert_eq!(
        two_primary, 0,
        "two predicates on one index read {two_primary} primary records; it was one per \
         candidate, and the posting key had already answered"
    );
    assert_eq!(
        one_candidates, two_candidates,
        "two predicates examined {two_candidates} candidates against {one_candidates}"
    );
    assert_eq!(
        one_accesses, two_accesses,
        "two predicates cost {two_accesses} pager accesses against {one_accesses} for the \
         same range written once"
    );
}

/// An intersection that is empty answers without walking the tree at all.
#[test]
fn an_empty_intersection_never_touches_the_index() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path());
    let filters = [
        Predicate::Range(Bound::Unbounded, Bound::Included(100.0)).filter(fixture.price),
        Predicate::Range(Bound::Excluded(300.0), Bound::Unbounded).filter(fixture.price),
    ];
    answer(&fixture, &filters, CandidateDriver::Auto);
    let (keys, candidates, primary, _) = answer(&fixture, &filters, CandidateDriver::Auto);
    assert!(keys.is_empty());
    assert_eq!(candidates, 0, "an empty range examined {candidates} candidates");
    assert_eq!(primary, 0);

    // Eq outside a range is the same statement.
    let filters = [
        Predicate::Eq(100.0).filter(fixture.price),
        Predicate::Range(Bound::Excluded(300.0), Bound::Unbounded).filter(fixture.price),
    ];
    let (keys, candidates, ..) = answer(&fixture, &filters, CandidateDriver::Auto);
    assert!(keys.is_empty());
    assert_eq!(candidates, 0);

    // And so is a nullish predicate beside a value predicate.
    let filters = [
        Predicate::IsNull.filter(fixture.price),
        Predicate::Range(Bound::Unbounded, Bound::Unbounded).filter(fixture.price),
    ];
    let (keys, candidates, ..) = answer(&fixture, &filters, CandidateDriver::Auto);
    assert!(keys.is_empty());
    assert_eq!(candidates, 0);
}
