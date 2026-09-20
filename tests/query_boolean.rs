//! The Tier-1 boolean atomics of `docs/QL_CONTRACT.md` §3 -- `Any` (union),
//! `All` (intersection), `Not` (complement) and `Ids` (a semi-join set) --
//! against a brute-force oracle over the 2,000-row fixture.
//!
//! The oracle is SQL's own three-valued logic, evaluated per row: a predicate
//! over a NULL value is UNKNOWN, `NOT UNKNOWN` is UNKNOWN, and a row reaches
//! the answer only when the whole tree is TRUE. That is the claim the set
//! algebra has to meet -- a union of index-side sets and a complement over a
//! leaf's own universe are only worth having if they answer the question the
//! rows answer.

#[path = "sqlslice/fixture.rs"]
mod fixture;

use e4_prototype::{
    collections::{
        CandidateDriver, CollectionOptions, Database, EntityId, Projection, QueryBudget,
        QueryDriver, QueryError, QueryFilter, QueryOrder, QueryRequest, ScalarFilter, ScalarValue,
        TextMatch, WorkResource,
    },
    spatial_math::{wgs84_distance_metres, Point},
    Kind,
};
use fixture::{Fixture, Rng, Row, KINDS, VOCAB};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::{json, Value};
use std::ops::Bound;
use tempfile::TempDir;

const PAGE: usize = 8192;
const TREES: usize = 500;

/// Rows added on top of the shared fixture with a MISSING `text` field, and
/// as many again with a missing `loc`: 5% of the 2,000-row base each.
///
/// Without them `Node::Text` and `Node::Radius` always answer `Some`, because
/// every fixture row carries both -- and the three-valued oracle then cannot
/// see the one place a complement's universe matters. `Score` was the only
/// nullable leaf and it is a scalar range, which is the case that was already
/// right.
///
/// They are added HERE rather than in `tests/sqlslice/fixture.rs` on purpose:
/// that file is shared with three other suites, and this suite's oracle is
/// the only thing that wants a null text field.
const MISSING_TEXT: usize = fixture::ROWS / 20;
const MISSING_LOC: usize = fixture::ROWS / 20;

/// One row as the ORACLE sees it: the fixture's row, plus whether the stored
/// document actually carries the two fields that have no NULL of their own in
/// `Row`. A document with no `text` has no posting and no norm; one with no
/// `loc` has no point posting. Both are UNKNOWN in SQL, and UNKNOWN is what
/// `eval` returns for them.
#[derive(Clone, Debug)]
struct Doc {
    row: Row,
    has_text: bool,
    has_loc: bool,
}

fn open() -> (TempDir, Fixture, Vec<Doc>) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut f = fixture::build(&path);
    let docs = extend(&mut f);
    (dir, f, docs)
}

/// Append the null-field rows to the built fixture and hand back the whole
/// corpus. `Fixture::rows` is kept in step, so a test that indexes rows by
/// position still names the same entity sequence.
fn extend(f: &mut Fixture) -> Vec<Doc> {
    let mut docs: Vec<Doc> = f
        .rows
        .iter()
        .cloned()
        .map(|row| Doc {
            row,
            has_text: true,
            has_loc: true,
        })
        .collect();
    for i in 0..(MISSING_TEXT + MISSING_LOC) {
        let mut row = f.rows[(i * 7) % fixture::ROWS].clone();
        row.key = format!("n{i:05}");
        let has_loc = i < MISSING_TEXT;
        let doc = Doc {
            row,
            has_text: !has_loc,
            has_loc,
        };
        let mut object = serde_json::Map::new();
        object.insert("key".into(), json!(doc.row.key));
        object.insert("name".into(), json!(doc.row.name));
        object.insert("descr".into(), json!(doc.row.desc));
        if doc.has_text {
            object.insert("text".into(), json!(doc.row.text));
        }
        object.insert("born".into(), json!(doc.row.born));
        object.insert("kind".into(), json!(doc.row.kind));
        if doc.has_loc {
            object.insert(
                "loc".into(),
                json!({"type": "Point", "coordinates": [doc.row.lon, doc.row.lat]}),
            );
        }
        object.insert("plot".into(), fixture::geom_json(&doc.row.plot));
        object.insert("emb".into(), json!(doc.row.emb));
        object.insert(
            "score".into(),
            match doc.row.score {
                Some(value) => json!(value),
                None => Value::Null,
            },
        );
        object.insert("flag".into(), json!(doc.row.flag));
        if let Some(tag) = &doc.row.tag {
            object.insert("tag".into(), json!(tag));
        }
        let id = f
            .db
            .put(f.place, &doc.row.key, &Value::Object(object))
            .unwrap();
        assert_eq!(
            id.sequence as usize,
            docs.len() + 1,
            "the extra rows follow the fixture's in sequence"
        );
        docs.push(doc);
    }
    f.db.commit().unwrap();
    f.rows = docs.iter().map(|doc| doc.row.clone()).collect();
    f.keys = f.rows.iter().map(|row| row.key.clone()).collect();
    docs
}

fn generous() -> QueryBudget {
    QueryBudget::unlimited()
}

// ── the tree, owned, with an oracle and a borrowed form ───────────────────

/// One node of a random boolean tree. Every leaf is index-side answerable,
/// which is what `prepare_query` requires of a boolean filter.
#[derive(Clone, Debug)]
enum Node {
    /// `kind = KINDS[i]`, always present in the fixture's rows.
    Kind(usize),
    /// `born BETWEEN lo AND hi`, always present.
    Born(i64, i64),
    /// `flag = b`, always present.
    Flag(bool),
    /// `score BETWEEN lo AND hi`. One row in seventeen has a JSON null here,
    /// so this is the leaf whose UNKNOWN the oracle and the engine must
    /// agree about.
    Score(f64, f64),
    /// `ST_DWithin(loc, centre, metres)`.
    Radius(f64),
    /// One term of the text index.
    Text(usize),
    Not(Box<Node>),
    Any(Vec<Node>),
    All(Vec<Node>),
}

impl Node {
    /// SQL's three-valued answer for one row: `None` is UNKNOWN.
    ///
    /// A document with no `text` field and one with no `loc` are UNKNOWN for
    /// the text and the radius leaf, exactly as a JSON null `score` is for the
    /// score range: the predicate has no value to be true or false about.
    fn eval(&self, doc: &Doc) -> Option<bool> {
        let row = &doc.row;
        match self {
            Self::Kind(at) => Some(row.kind == KINDS[*at]),
            Self::Born(lo, hi) => Some(row.born >= *lo && row.born <= *hi),
            Self::Flag(want) => Some(row.flag == *want),
            Self::Score(lo, hi) => row.score.map(|value| value >= *lo && value <= *hi),
            Self::Radius(metres) => {
                if !doc.has_loc {
                    return None;
                }
                let here = Point::new(row.lon, row.lat).unwrap();
                Some(wgs84_distance_metres(fixture::centre(), here) <= *metres)
            }
            Self::Text(at) => {
                if !doc.has_text {
                    return None;
                }
                Some(
                    row.text
                        .split_whitespace()
                        .any(|token| token == VOCAB[*at]),
                )
            }
            Self::Not(inner) => inner.eval(doc).map(|value| !value),
            Self::Any(children) => {
                let mut unknown = false;
                for child in children {
                    match child.eval(doc) {
                        Some(true) => return Some(true),
                        Some(false) => {}
                        None => unknown = true,
                    }
                }
                if unknown {
                    None
                } else {
                    Some(false)
                }
            }
            Self::All(children) => {
                let mut unknown = false;
                for child in children {
                    match child.eval(doc) {
                        Some(false) => return Some(false),
                        Some(true) => {}
                        None => unknown = true,
                    }
                }
                if unknown {
                    None
                } else {
                    Some(true)
                }
            }
        }
    }

    fn leaves(&self) -> usize {
        match self {
            Self::Any(children) | Self::All(children) => {
                children.iter().map(Self::leaves).sum()
            }
            Self::Not(inner) => inner.leaves(),
            _ => 1,
        }
    }
}

/// Build the borrowed `QueryFilter` tree on the stack, the way the SQL layer
/// does: a tree of references cannot be returned, so the innermost frame is
/// where it is used.
fn with_node<R>(node: &Node, f: &Fixture, k: &mut dyn FnMut(&QueryFilter<'_>) -> R) -> R {
    match node {
        Node::Kind(at) => k(&QueryFilter::Scalar {
            index: f.index.kind,
            predicate: ScalarFilter::Eq(ScalarValue::Text(KINDS[*at])),
        }),
        Node::Born(lo, hi) => k(&QueryFilter::Scalar {
            index: f.index.born,
            predicate: ScalarFilter::Range {
                lower: Bound::Included(ScalarValue::I64(*lo)),
                upper: Bound::Included(ScalarValue::I64(*hi)),
            },
        }),
        Node::Flag(want) => k(&QueryFilter::Scalar {
            index: f.index.flag,
            predicate: ScalarFilter::Eq(ScalarValue::Bool(*want)),
        }),
        Node::Score(lo, hi) => k(&QueryFilter::Scalar {
            index: f.index.score,
            predicate: ScalarFilter::Range {
                lower: Bound::Included(ScalarValue::F64(*lo)),
                upper: Bound::Included(ScalarValue::F64(*hi)),
            },
        }),
        Node::Radius(metres) => k(&QueryFilter::Point {
            index: f.index.loc,
            predicate: e4_prototype::collections::PointFilter::Radius {
                center: fixture::centre(),
                radius_metres: *metres,
            },
        }),
        Node::Text(at) => k(&QueryFilter::Text {
            index: f.index.text,
            query: VOCAB[*at],
            matching: TextMatch::Any,
        }),
        Node::Not(inner) => with_node(inner, f, &mut |child| k(&QueryFilter::Not(child))),
        Node::Any(children) => with_children(children, f, None, &mut |built| {
            k(&QueryFilter::Any(built))
        }),
        Node::All(children) => with_children(children, f, None, &mut |built| {
            k(&QueryFilter::All(built))
        }),
    }
}

struct Built<'a> {
    node: &'a QueryFilter<'a>,
    previous: Option<&'a Built<'a>>,
}

fn with_children<R>(
    nodes: &[Node],
    f: &Fixture,
    previous: Option<&Built<'_>>,
    k: &mut dyn FnMut(&[QueryFilter<'_>]) -> R,
) -> R {
    match nodes.split_first() {
        None => {
            let mut out = Vec::new();
            let mut link = previous;
            while let Some(built) = link {
                out.push(built.node.clone());
                link = built.previous;
            }
            out.reverse();
            k(&out)
        }
        Some((head, rest)) => with_node(head, f, &mut |node| {
            let built = Built { node, previous };
            with_children(rest, f, Some(&built), k)
        }),
    }
}

fn leaf(rng: &mut Rng) -> Node {
    match rng.usize(6) {
        0 => Node::Kind(rng.usize(KINDS.len())),
        1 => {
            let lo = 19_500_101 + (rng.usize(700) as i64) * 100;
            Node::Born(lo, lo + (rng.usize(120) as i64) * 100)
        }
        2 => Node::Flag(rng.usize(2) == 0),
        3 => {
            let lo = (rng.usize(100) as f64) / 4.0;
            Node::Score(lo, lo + (rng.usize(40) as f64) / 4.0)
        }
        4 => Node::Radius(2_000.0 + (rng.usize(40) as f64) * 1_000.0),
        _ => Node::Text(rng.usize(VOCAB.len())),
    }
}

/// A random tree of depth at most `depth`, counted as in the brief: a leaf is
/// depth 1.
fn tree(rng: &mut Rng, depth: usize) -> Node {
    if depth <= 1 {
        return leaf(rng);
    }
    match rng.usize(4) {
        0 => Node::Not(Box::new(tree(rng, depth - 1))),
        1 | 2 => {
            let n = 2 + rng.usize(2);
            Node::Any((0..n).map(|_| tree(rng, depth - 1)).collect())
        }
        _ => {
            let n = 2 + rng.usize(2);
            Node::All((0..n).map(|_| tree(rng, depth - 1)).collect())
        }
    }
}

fn run(db: &Database, f: &Fixture, node: &Node) -> Result<Vec<EntityId>, QueryError> {
    with_node(node, f, &mut |filter| {
        let filters = [filter.clone()];
        let mut prepared = db.prepare_query(QueryRequest {
            collection: f.place,
            filters: &filters,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })?;
        let mut out = Vec::new();
        loop {
            let page = prepared.next_page(PAGE, generous(), || false)?;
            out.extend(page.rows.iter().map(|row| row.id));
            if page.done {
                break;
            }
        }
        Ok(out)
    })
}

fn expected(
    f: &Fixture,
    docs: &[Doc],
    node: &Node,
    live: &dyn Fn(usize) -> bool,
) -> Vec<EntityId> {
    docs.iter()
        .enumerate()
        .filter(|(at, doc)| live(*at) && node.eval(doc) == Some(true))
        .map(|(at, _)| EntityId {
            collection: f.place,
            sequence: (at + 1) as u64,
        })
        .collect()
}

// ── the oracle ────────────────────────────────────────────────────────────

#[test]
fn random_boolean_trees_match_brute_force() {
    let (_dir, f, docs) = open();
    let mut rng = Rng(0x424F_4F4C_5F54_5245);
    let all_live = |_: usize| true;
    let mut with_not = 0usize;
    for _ in 0..TREES {
        let node = tree(&mut rng, 3);
        assert!(node.leaves() <= 64, "the tree stays inside MAX_BOOLEAN_LEAVES");
        if format!("{node:?}").contains("Not") {
            with_not += 1;
        }
        let got = run(&f.db, &f, &node).unwrap_or_else(|error| {
            panic!("boolean tree {node:?} was refused: {error:?}");
        });
        let want = expected(&f, &docs, &node, &all_live);
        assert_eq!(got, want, "boolean tree {node:?}");
    }
    assert!(
        with_not > TREES / 8,
        "the generator produced {with_not} complements out of {TREES} trees"
    );
}

// ── the complement, and rows that are no longer there ─────────────────────

#[test]
fn a_deleted_row_is_never_in_a_complement() {
    let (_dir, mut f, docs) = open();
    // Every hundredth row, which is 20 of the 2,000.
    let removed: Vec<usize> = (0..fixture::ROWS).filter(|at| at % 100 == 7).collect();
    for at in &removed {
        let key = f.rows[*at].key.clone();
        assert!(f.db.delete(f.place, &key).unwrap(), "the row was there");
    }
    f.db.commit().unwrap();
    let live = |at: usize| !removed.contains(&at);

    // One complement per universe the engine can take one against: a point
    // index's postings, the live rows behind a text match, and a scalar
    // index's own keyspace.
    for node in [
        Node::Not(Box::new(Node::Radius(12_000.0))),
        Node::Not(Box::new(Node::Text(0))),
        Node::Not(Box::new(Node::Kind(2))),
        Node::Not(Box::new(Node::Score(10.0, 20.0))),
        Node::Any(vec![
            Node::Not(Box::new(Node::Radius(9_000.0))),
            Node::Kind(1),
        ]),
    ] {
        let got = run(&f.db, &f, &node).unwrap();
        let want = expected(&f, &docs, &node, &live);
        assert_eq!(got, want, "complement {node:?}");
        for at in &removed {
            let gone = EntityId {
                collection: f.place,
                sequence: (*at + 1) as u64,
            };
            assert!(
                !got.contains(&gone),
                "a deleted row reached the answer of {node:?}"
            );
        }
    }
}

// ── the driver a disjunction gets, and the one it does not take ───────────

#[test]
fn a_union_drives_only_when_nothing_else_can() {
    let (_dir, f, docs) = open();
    let node = Node::Any(vec![Node::Kind(0), Node::Kind(3)]);
    with_node(&node, &f, &mut |filter| {
        let filters = [filter.clone()];
        let mut prepared = f
            .db
            .prepare_query(QueryRequest {
                collection: f.place,
                filters: &filters,
                order: QueryOrder::EntityId,
                projection: Projection::Ids,
                total_limit: None,
                driver: CandidateDriver::Auto,
            })
            .unwrap();
        let page = prepared.next_page(PAGE, generous(), || false).unwrap();
        assert_eq!(page.driver, QueryDriver::Membership { filter: 0 });
        let plan = prepared.describe();
        assert_eq!(plan.filters[0].family, "boolean");
        assert!(
            plan.filters[0].detail.starts_with("union: union("),
            "EXPLAIN prints the algebra: {}",
            plan.filters[0].detail
        );
        assert!(
            plan.filters[0].detail.contains("ids")
                || plan.filters[0].detail.contains("bitmap"),
            "EXPLAIN prints the set and its size: {}",
            plan.filters[0].detail
        );
    });

    // With a second conjunct that can drive, the union stays a filter.
    let union = Node::Any(vec![Node::Kind(0), Node::Kind(3)]);
    with_node(&union, &f, &mut |filter| {
        let filters = [
            QueryFilter::Scalar {
                index: f.index.flag,
                predicate: ScalarFilter::Eq(ScalarValue::Bool(true)),
            },
            filter.clone(),
        ];
        let mut prepared = f
            .db
            .prepare_query(QueryRequest {
                collection: f.place,
                filters: &filters,
                order: QueryOrder::EntityId,
                projection: Projection::Ids,
                total_limit: None,
                driver: CandidateDriver::Auto,
            })
            .unwrap();
        let page = prepared.next_page(PAGE, generous(), || false).unwrap();
        assert_eq!(page.driver, QueryDriver::Scalar(f.index.flag));
        let want: Vec<EntityId> = f
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.flag && (row.kind == KINDS[0] || row.kind == KINDS[3]))
            .map(|(at, _)| EntityId {
                collection: f.place,
                sequence: (at + 1) as u64,
            })
            .collect();
        assert_eq!(page.rows.iter().map(|r| r.id).collect::<Vec<_>>(), want);
    });
}

// ── paging ────────────────────────────────────────────────────────────────

#[test]
fn a_union_pages_disjointly_and_completely() {
    let (_dir, f, docs) = open();
    let node = Node::Any(vec![
        Node::Kind(0),
        Node::Not(Box::new(Node::Radius(30_000.0))),
        Node::Text(4),
    ]);
    let whole = run(&f.db, &f, &node).unwrap();
    assert!(whole.len() > 300, "the union is worth paging: {}", whole.len());
    for page_size in [1usize, 7, 64, 333] {
        let paged = with_node(&node, &f, &mut |filter| {
            let filters = [filter.clone()];
            let mut prepared = f
                .db
                .prepare_query(QueryRequest {
                    collection: f.place,
                    filters: &filters,
                    order: QueryOrder::EntityId,
                    projection: Projection::Ids,
                    total_limit: None,
                    driver: CandidateDriver::Auto,
                })
                .unwrap();
            let mut out = Vec::new();
            loop {
                let page = prepared.next_page(page_size, generous(), || false).unwrap();
                assert!(page.rows.len() <= page_size);
                out.extend(page.rows.iter().map(|row| row.id));
                if page.done {
                    break;
                }
            }
            out
        });
        assert_eq!(paged, whole, "page size {page_size}");
    }
}

// ── cancellation ──────────────────────────────────────────────────────────

#[test]
fn a_union_walk_is_cancellable() {
    let (_dir, f, docs) = open();
    let node = Node::Any(vec![Node::Kind(0), Node::Kind(1), Node::Kind(2)]);
    with_node(&node, &f, &mut |filter| {
        let filters = [filter.clone()];
        let mut prepared = f
            .db
            .prepare_query(QueryRequest {
                collection: f.place,
                filters: &filters,
                order: QueryOrder::EntityId,
                projection: Projection::Ids,
                total_limit: None,
                driver: CandidateDriver::Auto,
            })
            .unwrap();
        // The set is walked on the first page, so a cancellation raised
        // before it starts is the walk's own, not a candidate's.
        let error = prepared
            .next_page(PAGE, generous(), || true)
            .expect_err("a cancelled page is an error");
        assert!(matches!(error, QueryError::Cancelled), "{error:?}");
    });

    // And once the set is built, the WALK over it is cancellable too.
    with_node(&node, &f, &mut |filter| {
        let filters = [filter.clone()];
        let mut prepared = f
            .db
            .prepare_query(QueryRequest {
                collection: f.place,
                filters: &filters,
                order: QueryOrder::EntityId,
                projection: Projection::Ids,
                total_limit: None,
                driver: CandidateDriver::Auto,
            })
            .unwrap();
        prepared.next_page(1, generous(), || false).unwrap();
        let mut calls = 0usize;
        let error = prepared
            .next_page(PAGE, generous(), || {
                calls += 1;
                calls > 4
            })
            .expect_err("a cancelled page is an error");
        assert!(matches!(error, QueryError::Cancelled), "{error:?}");
    });
}

// ── what has no set, and is refused rather than emulated ──────────────────

#[test]
fn a_leaf_with_no_set_is_refused_at_prepare() {
    let (_dir, f, docs) = open();
    let centre = fixture::centre();
    let refusals: Vec<(&str, QueryFilter<'_>, &str)> = vec![
        (
            "geometry",
            QueryFilter::Geometry {
                index: f.index.plot,
                predicate: e4_prototype::collections::GeometryFilter::DWithin {
                    geometry: e4_prototype::collections::Geom::Point(
                        centre.longitude(),
                        centre.latitude(),
                    ),
                    metres: 1_000.0,
                },
            },
            "geometry posting's box is a candidate test",
        ),
        (
            "is null",
            QueryFilter::Scalar {
                index: f.index.score,
                predicate: ScalarFilter::IsNull,
            },
            "nullish index key",
        ),
        (
            "is missing",
            QueryFilter::Scalar {
                index: f.index.score,
                predicate: ScalarFilter::IsMissing,
            },
            "nullish index key",
        ),
        (
            "phrase",
            QueryFilter::Text {
                index: f.index.text,
                query: "kebun sekolah",
                matching: TextMatch::Phrase,
            },
            "phrase cannot be a boolean leaf",
        ),
        (
            "json",
            QueryFilter::JsonEq {
                field: "tag",
                value: &serde_json::Value::Null,
            },
            "JSON equality cannot be a boolean leaf",
        ),
    ];
    for (name, inner, needle) in refusals {
        let leaves = [
            QueryFilter::Scalar {
                index: f.index.kind,
                predicate: ScalarFilter::Eq(ScalarValue::Text(KINDS[0])),
            },
            inner,
        ];
        let filters = [QueryFilter::Any(&leaves)];
        let error = f
            .db
            .prepare_query(QueryRequest {
                collection: f.place,
                filters: &filters,
                order: QueryOrder::EntityId,
                projection: Projection::Ids,
                total_limit: None,
                driver: CandidateDriver::Auto,
            })
            .err()
            .expect("a leaf with no set is refused");
        let text = format!("{error:?}");
        assert!(
            text.contains(needle),
            "{name}: the refusal names the missing atomic, got {text}"
        );
    }

    // A traversal too: a bounded frontier is not a membership set.
    let seed = f.db.get(f.place, &f.rows[0].key).unwrap().unwrap().id;
    let bfs = e4_prototype::collections::BfsRequest {
        seed,
        direction: e4_prototype::collections::Direction::Outgoing,
        context: f.context,
        edge_type: Some(f.near),
        min_depth: 1,
        max_depth: 1,
        include_seed: false,
        max_visited: 64,
        max_edges: 64,
        result_limit: 64,
        edge_where: &[],
        node_where: &[],
    };
    let leaves = [
        QueryFilter::Scalar {
            index: f.index.kind,
            predicate: ScalarFilter::Eq(ScalarValue::Text(KINDS[0])),
        },
        QueryFilter::Graph(bfs),
    ];
    let filters = [QueryFilter::Any(&leaves)];
    let error = f
        .db
        .prepare_query(QueryRequest {
            collection: f.place,
            filters: &filters,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .err()
        .expect("a traversal has no set");
    assert!(format!("{error:?}").contains("bounded frontier"));
}

// ── the explicit semi-join set ────────────────────────────────────────────

#[test]
fn a_semi_join_set_is_checked_not_trusted() {
    let (_dir, f, docs) = open();
    let wanted: Vec<EntityId> = (1..=40u64)
        .step_by(3)
        .map(|sequence| EntityId {
            collection: f.place,
            sequence,
        })
        .collect();
    let filters = [QueryFilter::Ids(&wanted)];
    let mut prepared = f
        .db
        .prepare_query(QueryRequest {
            collection: f.place,
            filters: &filters,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let page = prepared.next_page(PAGE, generous(), || false).unwrap();
    assert_eq!(page.driver, QueryDriver::Membership { filter: 0 });
    assert_eq!(page.rows.iter().map(|row| row.id).collect::<Vec<_>>(), wanted);

    // Out of order is refused rather than silently answering `contains`
    // wrongly.
    let jumbled = [
        EntityId {
            collection: f.place,
            sequence: 9,
        },
        EntityId {
            collection: f.place,
            sequence: 2,
        },
    ];
    let filters = [QueryFilter::Ids(&jumbled)];
    let error = f
        .db
        .prepare_query(QueryRequest {
            collection: f.place,
            filters: &filters,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .err()
        .expect("an unsorted set is refused");
    assert!(format!("{error:?}").contains("ascending"));
}

// ── the budget the walk is charged against ────────────────────────────────

#[test]
fn a_complement_is_bounded_by_a_named_resource() {
    let (_dir, f, _docs) = open();
    // A TEXT complement walks the text index's own document universe -- the
    // documents it holds a norm for -- and that walk is charged as text
    // postings. An explicit `Ids` set has no index of its own, so ITS
    // complement is still the live primary keyspace, charged as primary
    // reads. One case per universe, each named.
    let some_ids: Vec<EntityId> = (1..=8u64)
        .map(|sequence| EntityId {
            collection: f.place,
            sequence,
        })
        .collect();
    let cases: Vec<(&str, Node, Option<Vec<EntityId>>, WorkResource)> = vec![
        (
            "text",
            Node::Not(Box::new(Node::Text(1))),
            None,
            WorkResource::TextPostings,
        ),
        (
            "ids",
            Node::Kind(0),
            Some(some_ids.clone()),
            WorkResource::PrimaryReads,
        ),
    ];
    for (name, node, ids, resource) in cases {
        let run = |filter: &QueryFilter<'_>| {
            let filters = [filter.clone()];
            let mut prepared = f
                .db
                .prepare_query(QueryRequest {
                    collection: f.place,
                    filters: &filters,
                    order: QueryOrder::EntityId,
                    projection: Projection::Ids,
                    total_limit: None,
                    driver: CandidateDriver::Auto,
                })
                .unwrap();
            let mut budget = QueryBudget::unlimited();
            budget.primary_reads = 16;
            budget.text_postings = 16;
            prepared
                .next_page(PAGE, budget, || false)
                .expect_err("the universe walk is bounded")
        };
        let error = match &ids {
            None => with_node(&node, &f, &mut |filter| run(filter)),
            Some(ids) => run(&QueryFilter::Not(&QueryFilter::Ids(ids))),
        };
        assert!(
            matches!(error, QueryError::BudgetExceeded { resource: r, .. } if r == resource),
            "{name}: {error:?}"
        );
    }
}

// ── the universe a text complement is taken against ──────────────────────

#[test]
fn a_null_text_field_is_in_neither_the_leaf_nor_its_complement() {
    let (_dir, f, docs) = open();
    // The rows with no `text` field at all, by entity sequence.
    let null_text: Vec<u64> = docs
        .iter()
        .enumerate()
        .filter(|(_, doc)| !doc.has_text)
        .map(|(at, _)| (at + 1) as u64)
        .collect();
    assert_eq!(null_text.len(), MISSING_TEXT, "the fixture has null texts");

    for term in 0..3usize {
        let leaf = Node::Text(term);
        let complement = Node::Not(Box::new(Node::Text(term)));
        let inside = run(&f.db, &f, &leaf).unwrap();
        let outside = run(&f.db, &f, &complement).unwrap();
        for sequence in &null_text {
            let id = EntityId {
                collection: f.place,
                sequence: *sequence,
            };
            assert!(!inside.contains(&id), "a null text matched term {term}");
            assert!(
                !outside.contains(&id),
                "a null text reached the complement of term {term}: NOT UNKNOWN is UNKNOWN"
            );
        }
        // And nothing else was lost: the two halves partition the documents
        // the index actually holds.
        let both = inside.len() + outside.len();
        assert_eq!(
            both,
            docs.len() - MISSING_TEXT,
            "the leaf and its complement partition the index's own documents"
        );
    }

    // The mixed tree the review names: a scalar leaf gets Kleene semantics
    // from the nullish key, and the text half must agree with it.
    let mixed = Node::Not(Box::new(Node::Any(vec![
        Node::Born(19_500_101, 19_600_101),
        Node::Text(0),
    ])));
    let got = run(&f.db, &f, &mixed).unwrap();
    let want = expected(&f, &docs, &mixed, &|_| true);
    assert_eq!(got, want, "NOT (born range OR text) over a null text");
}

// ── a caller's id that the collection never issued ───────────────────────

#[test]
fn an_out_of_span_id_is_dropped_not_corrupt() {
    let (_dir, f, docs) = open();
    let past = (docs.len() as u64) + 5_000;
    // Small enough to stay a plain Vec on its own, but the union below turns
    // the pair into a bitmap of the collection's span -- which is where the
    // out-of-span sequence used to be reported as database corruption.
    let ids = [EntityId {
        collection: f.place,
        sequence: past,
    }];
    let leaves = [
        QueryFilter::Ids(&ids),
        QueryFilter::Scalar {
            index: f.index.kind,
            predicate: ScalarFilter::Eq(ScalarValue::Text(KINDS[0])),
        },
    ];
    let filters = [QueryFilter::Any(&leaves)];
    let mut prepared = f
        .db
        .prepare_query(QueryRequest {
            collection: f.place,
            filters: &filters,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let mut got = Vec::new();
    loop {
        let page = prepared.next_page(PAGE, generous(), || false).unwrap();
        got.extend(page.rows.iter().map(|row| row.id));
        if page.done {
            break;
        }
    }
    let want = expected(&f, &docs, &Node::Kind(0), &|_| true);
    assert_eq!(got, want, "the out-of-span id simply names no row");

    // The same id alone, and as a complement: still an answer, never Corrupt.
    let filters = [QueryFilter::Ids(&ids)];
    let page = f
        .db
        .prepare_query(QueryRequest {
            collection: f.place,
            filters: &filters,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap()
        .next_page(PAGE, generous(), || false)
        .unwrap();
    assert!(page.rows.is_empty(), "no row has that sequence");

    let not = QueryFilter::Not(&QueryFilter::Ids(&ids));
    let filters = [not];
    let mut prepared = f
        .db
        .prepare_query(QueryRequest {
            collection: f.place,
            filters: &filters,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let mut complement = Vec::new();
    loop {
        let page = prepared.next_page(PAGE, generous(), || false).unwrap();
        complement.extend(page.rows.iter().map(|row| row.id));
        if page.done {
            break;
        }
    }
    assert_eq!(
        complement.len(),
        docs.len(),
        "the complement of a set that names nothing is every live row"
    );
}

// ── the scan between two members of a sparse bitmap ──────────────────────

#[test]
fn a_sparse_bitmap_scan_is_charged_and_pollable() {
    // 4 KiB of bitmap is 32,768 bits, so a collection has to be wider than
    // that before the zero-bit scan reaches its first poll at all.
    const ROWS: u64 = 40_000;
    let dir = TempDir::new().unwrap();
    let config = Config {
        budget_bytes: 32 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    };
    let mut db = Database::create(&dir.path().join("db"), config).unwrap();
    let wide = db
        .create_collection(
            "wide",
            vec![("key".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    for i in 0..ROWS {
        let key = format!("w{i:06}");
        db.put(wide, &key, &json!({"key": key})).unwrap();
        if (i + 1) % 4_096 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();

    // Every live row, as a caller's set. Its complement is the empty bitmap
    // of the whole span: 40,000 zero bits to walk and no member to find.
    let all: Vec<EntityId> = (1..=ROWS)
        .map(|sequence| EntityId {
            collection: wide,
            sequence,
        })
        .collect();
    let inner = QueryFilter::Ids(&all);
    let filters = [QueryFilter::Not(&inner)];
    let mut prepared = db
        .prepare_query(QueryRequest {
            collection: wide,
            filters: &filters,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let page = prepared
        .next_page(PAGE, QueryBudget::unlimited(), || false)
        .unwrap();
    assert!(page.rows.is_empty(), "every row is in the set");
    assert!(page.done);
    // No candidate was produced, and yet the scan was charged: one
    // `Candidates` per 4 KiB of bitmap scanned past.
    assert_eq!(
        page.work.candidates, 1,
        "40,000 zero bits is one 4 KiB poll, charged even with no member: {:?}",
        page.work
    );
}
