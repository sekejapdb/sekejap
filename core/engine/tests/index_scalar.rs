//! An EXPRESSION index (`IndexExpr`, `src/collections/catalog.rs`) stores
//! `expression(field)` while `IndexInfo::field` stays the SOURCE field. Every
//! place that recomputes a posting or a predicate FROM THE ROW has to pass
//! through the expression the same way `scalar_build_key_into` does, or the
//! row is compared field-against-image and the answer silently loses every
//! row whose stored value is not already its own image.
//!
//! Three such places exist and all three are reachable from the public
//! query API, which is why this file drives them directly rather than
//! through SQL -- the SQL planner happens to answer the shapes it can build
//! from the postings alone:
//!
//! 1. `filters.rs::scalar_filter_matches` for a non-driving EQUALITY when the
//!    candidate's row has already been read -- the entity walk carries it.
//! 2. The same function for a non-driving RANGE whose `MembershipSet`
//!    overflowed, which a caller's own `QueryBudget` decides.
//! 3. `filters.rs::persisted_scalar_key`, through `rank.rs` and `score.rs`,
//!    when the ORDER is the expression index itself.
//!
//! Every expectation here is a brute-force filter over the fixture held in
//! this process, with the oracle doing its own lowercasing.
use sekejap_core::{
    collections::{
        CandidateDriver, CollectionOptions, Database, EntityId, IndexExpr, Projection, QueryBudget,
        QueryFilter, QueryOrder, QueryRequest, ScalarFilter, ScalarValue, SortDirection,
    },
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;
use std::ops::Bound;

/// Deliberately mixed case: three spellings share one image under `lower`.
const KINDS: [&str; 4] = ["Home", "home", "HOME", "Farm"];
const ROWS: i64 = 400;

fn cfg() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn budget(scalar_postings: u64) -> QueryBudget {
    QueryBudget {
        candidates: 1_000_000,
        primary_reads: 1_000_000,
        scalar_postings,
        graph_edges: 1_000_000,
        graph_visited: 1_000_000,
        spatial_postings: 1_000_000,
        text_postings: 1_000_000,
        text_tokens: 1_000_000,
        vector_locators: 1_000_000,
        vector_sidecars: 1_000_000,
        vector_lanes: 1_000_000,
        key_postings: 1_000_000,
        rows_written: 1_000_000,
        groups: 1_000_000,
        output_bytes: 16 << 20,
        // No wall-clock bound: this suite asserts WORK, which is the
        // reproducible bound (`docs/dist/OPS_CONTRACT.md` §3).
        deadline: None,
    }
}

struct Fixture {
    db: Database,
    lower: sekejap_core::collections::IndexId,
    born: sekejap_core::collections::IndexId,
    /// `(key, kind, born)` for every row, in insertion order.
    rows: Vec<(String, &'static str, i64)>,
}

fn build(dir: &std::path::Path) -> Fixture {
    let mut db = Database::create(dir.join("db"), cfg()).unwrap();
    let c = db
        .create_collection(
            "place",
            vec![("kind".into(), Kind::Text), ("born".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    let mut rows = Vec::new();
    for i in 0..ROWS {
        let kind = KINDS[(i % KINDS.len() as i64) as usize];
        let key = format!("k{i:04}");
        db.put(c, &key, &json!({ "kind": kind, "born": i })).unwrap();
        rows.push((key, kind, i));
    }
    db.commit().unwrap();
    let lower = db
        .create_expression_index(c, "kind_lower", "kind", IndexExpr::Lower, false)
        .unwrap();
    db.build_index_to_ready(lower, 64).unwrap();
    let born = db.create_scalar_index(c, "born_ix", "born", false).unwrap();
    db.build_index_to_ready(born, 64).unwrap();
    db.commit().unwrap();
    Fixture { db, lower, born, rows }
}

fn collection(f: &Fixture) -> sekejap_core::collections::CollectionId {
    f.db.collection("place").unwrap().unwrap()
}

/// Every id one request returns, paged, in the order the pages produced them.
fn walk(
    f: &Fixture,
    filters: &[QueryFilter],
    order: QueryOrder,
    driver: CandidateDriver,
    page: usize,
    scalar_postings: u64,
) -> Vec<EntityId> {
    let fields = ["kind", "born"];
    let mut query = f
        .db
        .prepare_query(QueryRequest {
            collection: collection(f),
            filters,
            order,
            projection: Projection::Fields(&fields),
            total_limit: None,
            driver,
        })
        .unwrap();
    let mut out = Vec::new();
    loop {
        let answer = query
            .next_page(page, budget(scalar_postings), || false)
            .unwrap();
        out.extend(answer.rows.iter().map(|row| row.id));
        if answer.done {
            break;
        }
    }
    out
}

/// The ids of the fixture rows a predicate admits, by definition.
fn oracle(f: &Fixture, keep: impl Fn(&(String, &'static str, i64)) -> bool) -> Vec<EntityId> {
    let c = collection(f);
    f.rows
        .iter()
        .enumerate()
        .filter(|(_, row)| keep(row))
        .map(|(at, _)| EntityId {
            collection: c,
            sequence: at as u64 + 1,
        })
        .collect()
}

/// A non-driving EQUALITY on an expression index, with the candidate's row
/// already in hand.
///
/// The entity walk carries the row it read, so `filters_match` finds
/// `encoded` already filled and answers the predicate from the ROW rather
/// than from the posting. Recomputing `kind` instead of `lower(kind)` there
/// keeps only the one spelling that is already lower case -- a quarter of
/// this fixture instead of three quarters.
#[test]
fn a_non_driving_expression_equality_over_the_entity_walk_equals_the_filter() {
    let dir = tempfile::tempdir().unwrap();
    let f = build(dir.path());
    let filters = [QueryFilter::Scalar {
        index: f.lower,
        predicate: ScalarFilter::Eq(ScalarValue::Text("home")),
    }];
    let got = walk(
        &f,
        &filters,
        QueryOrder::EntityId,
        CandidateDriver::Entities,
        64,
        1_000_000,
    );
    let want = oracle(&f, |(_, kind, _)| kind.to_lowercase() == "home");
    // The fixture must make the two answers differ, or the test proves
    // nothing.
    let raw = oracle(&f, |(_, kind, _)| *kind == "home");
    assert!(raw.len() * 2 < want.len(), "the fixture must be mixed case");
    assert_eq!(got, want);
}

/// The same equality beside a range that drives, still over the entity walk.
#[test]
fn a_non_driving_expression_equality_beside_a_range_equals_the_filter() {
    let dir = tempfile::tempdir().unwrap();
    let f = build(dir.path());
    let filters = [
        QueryFilter::Scalar {
            index: f.lower,
            predicate: ScalarFilter::Eq(ScalarValue::Text("home")),
        },
        QueryFilter::Scalar {
            index: f.born,
            predicate: ScalarFilter::Range {
                lower: Bound::Included(ScalarValue::I64(0)),
                upper: Bound::Included(ScalarValue::I64(120)),
            },
        },
    ];
    let got = walk(
        &f,
        &filters,
        QueryOrder::EntityId,
        CandidateDriver::Entities,
        16,
        1_000_000,
    );
    assert_eq!(
        got,
        oracle(&f, |(_, kind, born)| kind.to_lowercase() == "home"
            && (0..=120).contains(born))
    );
}

/// A non-driving RANGE whose membership set OVERFLOWED.
///
/// `ensure_membership_sets` walks a non-driving range's postings once and
/// keeps the answer as a set; a caller whose `QueryBudget` cannot afford that
/// walk gets `MembershipSet::Overflow` instead, which is the named sacrifice
/// (`src/query/membership.rs`) that sends every candidate back to the ROW.
/// That row path is `scalar_filter_matches` again, so it has the same
/// obligation to apply the expression.
#[test]
fn an_overflowed_membership_set_over_an_expression_range_equals_the_filter() {
    let dir = tempfile::tempdir().unwrap();
    let f = build(dir.path());
    let filters = [
        QueryFilter::Scalar {
            index: f.lower,
            predicate: ScalarFilter::Range {
                lower: Bound::Included(ScalarValue::Text("ho")),
                upper: Bound::Excluded(ScalarValue::Text("hp")),
            },
        },
        QueryFilter::Scalar {
            index: f.born,
            predicate: ScalarFilter::Range {
                lower: Bound::Included(ScalarValue::I64(0)),
                upper: Bound::Included(ScalarValue::I64(40)),
            },
        },
    ];
    let want = oracle(&f, |(_, kind, born)| {
        kind.to_lowercase().starts_with("ho") && (0..=40).contains(born)
    });
    // Generous: the membership walk succeeds and the postings answer it.
    assert_eq!(
        walk(
            &f,
            &filters,
            QueryOrder::EntityId,
            CandidateDriver::Auto,
            16,
            1_000_000
        ),
        want,
        "the posting-side path"
    );
    // Tight: neither range's posting walk can be afforded, both sets
    // overflow, and every candidate is decided from its ROW instead. The
    // entity walk drives, because it spends no scalar postings of its own
    // and so cannot be starved by the same budget. The ANSWER must not
    // change -- only the work does.
    assert_eq!(
        walk(
            &f,
            &filters,
            QueryOrder::EntityId,
            CandidateDriver::Entities,
            16,
            60
        ),
        want,
        "the row-side path after the membership set overflowed"
    );
}

/// ORDER BY the expression index: the rank key a row produces is the index's
/// own value.
///
/// `rank.rs` and `score.rs` ask `persisted_scalar_key` for the key a
/// candidate would have in the ordering index. For an expression index that
/// is `lower(kind)`'s key; the raw field's key sorts differently -- `HOME`,
/// `Home` and `home` are three different byte strings on either side of
/// `Farm` -- so a wrong key here reorders the answer and, because the same
/// key is the page RESUME cursor, drops or repeats rows across pages.
#[test]
fn ordering_by_an_expression_index_ranks_by_the_expressions_value() {
    let dir = tempfile::tempdir().unwrap();
    let f = build(dir.path());
    let filters = [QueryFilter::Scalar {
        index: f.born,
        predicate: ScalarFilter::Range {
            lower: Bound::Included(ScalarValue::I64(0)),
            upper: Bound::Included(ScalarValue::I64(120)),
        },
    }];
    let c = collection(&f);
    // The oracle: ascending by `lower(kind)`, then by entity id, computed
    // here with this test's own lowercasing.
    let mut want: Vec<(String, u64)> = f
        .rows
        .iter()
        .enumerate()
        .filter(|(_, (_, _, born))| (0..=120).contains(born))
        .map(|(at, (_, kind, _))| (kind.to_lowercase(), at as u64 + 1))
        .collect();
    want.sort();
    let want: Vec<EntityId> = want
        .into_iter()
        .map(|(_, sequence)| EntityId {
            collection: c,
            sequence,
        })
        .collect();
    // One page, and then pages small enough that the resume cursor is
    // exercised between every few rows.
    for page in [1024usize, 7] {
        assert_eq!(
            walk(
                &f,
                &filters,
                QueryOrder::Scalar {
                    index: f.lower,
                    direction: SortDirection::Ascending
                },
                CandidateDriver::Auto,
                page,
                1_000_000
            ),
            want,
            "page size {page}"
        );
    }
}
