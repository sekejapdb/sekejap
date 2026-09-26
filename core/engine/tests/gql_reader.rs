//! The GQL profile's element reads (`docs/lang/GQL_PROFILE_DESIGN.md` §2.1,
//! §2.5, §3.2) and the one adjacency cursor the traversals share.
//!
//! What is at risk, one test each: that the cursor hands out PARALLEL edges
//! one by one, each with its own id and its own property bag, in both
//! directions; that an incoming edge's bag is read from its primary posting
//! and that read is charged while an outgoing bag is free; that a node
//! property read costs one primary read; that a missing property and a
//! stored null both read as `Null` while the property names still tell them
//! apart; that labels are the collection and the edge type; and that
//! `ELEMENT_ID` tells apart two nodes sharing an external key in two
//! collections, and two parallel edges. And that a reference whose row or
//! primary posting is gone is corruption, as it is to every traversal: a
//! reference is only ever made from the snapshot it is read in.
//!
//! The per-node text reads (`docs/lang/GQL_PROFILE_DESIGN_M5_M7.md` §3.1,
//! M6-C): that `text_matches` and `text_score` answer exactly what the
//! collection-level text query answers for that one node, a non-matching
//! node scoring `0.0` as the SQL `bm25()` leaf does; that one read tests the
//! node's postings rather than walking a term's whole posting list; and that
//! a refusal inside the one-id query comes back as that refusal, never as
//! "no match".

use sekejap_core::collections::gql::{
    BindingValue, EdgeRef, ElementReader, GqlBudget, GqlMeter, NodeRef,
};
use sekejap_core::collections::{
    AdjacencyCursor, CandidateDriver, CollectionId, Database, Direction, EdgeTypeId, EntityId,
    Error, GraphContextId, IndexId, OrderValue, Projection, QueryBudget, QueryError, QueryFilter,
    QueryOrder, QueryRequest, QueryResult, ScoreExpr, SortDirection, TextMatch, WorkResource,
};
use sekejap_core::Kind;
use serde_json::json;
use std::sync::Arc;

mod common;
use common::cfg;

fn never() -> bool {
    false
}

struct Fixture {
    db: Database,
    band: CollectionId,
    p1: EntityId,
    p2: EntityId,
    knows: EdgeTypeId,
}

/// Two people and a band, all three sharing nothing but the shape; `p1`
/// knows `p2` twice over, through two parallel edges with their own bags.
fn fixture(path: &std::path::Path) -> Fixture {
    let mut db = Database::create(path, cfg()).unwrap();
    let person = db
        .create_collection(
            "person",
            vec![("name".into(), Kind::Text), ("age".into(), Kind::Int)],
            Default::default(),
        )
        .unwrap();
    let band = db
        .create_collection(
            "band",
            vec![("name".into(), Kind::Text)],
            Default::default(),
        )
        .unwrap();
    db.enable_graph().unwrap();
    let knows = db.create_edge_type("knows").unwrap();
    let p1 = db
        .put(
            person,
            "p1",
            &json!({ "name": "first", "age": 30, "note": "extra" }),
        )
        .unwrap();
    // A stored null for `name`, nothing at all for `age`.
    let p2 = db.put(person, "p2", &json!({ "name": null })).unwrap();
    db.commit().unwrap();
    Fixture {
        db,
        band,
        p1,
        p2,
        knows,
    }
}

/// Two parallel `knows` edges p1 -> p2, returned in creation order.
fn parallel(f: &mut Fixture) -> [u64; 2] {
    let a =
        f.db.create_edge(
            GraphContextId::BASE,
            f.p1,
            f.knows,
            f.p2,
            &json!({ "since": 1 }),
        )
        .unwrap();
    let b =
        f.db.create_edge(
            GraphContextId::BASE,
            f.p1,
            f.knows,
            f.p2,
            &json!({ "since": 2 }),
        )
        .unwrap();
    f.db.commit().unwrap();
    [a.id, b.id]
}

/// Every edge the cursor hands out: key, id, far node and decoded bag.
fn walk(
    f: &Fixture,
    near: EntityId,
    direction: Direction,
) -> Vec<(
    sekejap_core::collections::EdgeKey,
    u64,
    EntityId,
    Option<serde_json::Value>,
)> {
    let mut cursor =
        AdjacencyCursor::open(&f.db, near, direction, GraphContextId::BASE, Some(f.knows)).unwrap();
    let mut out = Vec::new();
    while let Some(posting) = cursor.next_posting().unwrap() {
        let edge = posting.edge().unwrap();
        out.push((edge.key, edge.id, edge.far, edge.bag().unwrap()));
    }
    out
}

#[test]
fn the_cursor_hands_out_parallel_edges_each_with_its_own_id_and_bag() {
    let dir = tempfile::tempdir().unwrap();
    let mut f = fixture(dir.path());
    let [a, b] = parallel(&mut f);
    assert_ne!(a, b);

    // Outgoing: the posting carries the bag.
    let out = walk(&f, f.p1, Direction::Outgoing);
    assert_eq!(out.len(), 2, "both parallel edges are produced: {out:?}");
    let mut seen: Vec<(u64, serde_json::Value)> = out
        .iter()
        .map(|(key, id, far, bag)| {
            assert_eq!((key.source, key.destination, *far), (f.p1, f.p2, f.p2));
            (
                *id,
                bag.clone().expect("an outgoing posting carries its bag"),
            )
        })
        .collect();
    seen.sort_by_key(|(id, _)| *id);
    assert_eq!(
        seen,
        vec![(a, json!({ "since": 1 })), (b, json!({ "since": 2 }))]
    );

    // Incoming: the same two edges, in their STORED orientation, and no
    // bag -- a reverse posting is a marker.
    let back = walk(&f, f.p2, Direction::Incoming);
    let mut ids: Vec<u64> = back
        .iter()
        .map(|(key, id, far, bag)| {
            assert_eq!((key.source, key.destination, *far), (f.p1, f.p2, f.p1));
            assert!(bag.is_none());
            *id
        })
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![a, b]);

    // A node with no edge of the type: an empty range, not an error.
    assert!(walk(&f, f.p2, Direction::Outgoing).is_empty());
}

#[test]
fn an_incoming_edge_reads_its_bag_from_the_primary_posting_and_is_charged() {
    let dir = tempfile::tempdir().unwrap();
    let mut f = fixture(dir.path());
    let [a, _] = parallel(&mut f);
    let reader = ElementReader::new(&f.db);
    let mut cancel = never;
    let mut meter = GqlMeter::new(GqlBudget::unlimited(), &mut cancel);

    for (key, id, _, _) in walk(&f, f.p2, Direction::Incoming) {
        let edge = EdgeRef { key, id, bag: None };
        let since = reader.edge_property(&edge, "since", &mut meter).unwrap();
        let expected = if id == a { 1 } else { 2 };
        assert!(
            matches!(since, BindingValue::Int(n) if n == expected),
            "edge {id} read {since:?}"
        );
    }
    // One primary-posting point read per incoming edge, charged where the
    // traversal charges it.
    assert_eq!(meter.work().base.graph_edges, 2);
    assert_eq!(meter.work().base.primary_reads, 0);

    // An outgoing edge brought its bag with it: reading it is free.
    for (key, id, _, bag) in walk(&f, f.p1, Direction::Outgoing) {
        let edge = EdgeRef {
            key,
            id,
            bag: bag.map(Arc::new),
        };
        reader.edge_property(&edge, "since", &mut meter).unwrap();
        assert_eq!(
            reader.edge_property_names(&edge, &mut meter).unwrap(),
            vec!["since"]
        );
    }
    assert_eq!(meter.work().base.graph_edges, 2);
}

#[test]
fn a_node_property_read_costs_one_primary_read() {
    let dir = tempfile::tempdir().unwrap();
    let f = fixture(dir.path());
    let reader = ElementReader::new(&f.db);
    let mut cancel = never;
    let mut meter = GqlMeter::new(GqlBudget::unlimited(), &mut cancel);
    let p1 = NodeRef(f.p1);

    let name = reader.node_property(p1, "name", &mut meter).unwrap();
    assert!(
        matches!(&name, BindingValue::Text(t) if &**t == "first"),
        "{name:?}"
    );
    assert_eq!(meter.work().base.primary_reads, 1);
    let age = reader.node_property(p1, "age", &mut meter).unwrap();
    assert!(matches!(age, BindingValue::Int(30)), "{age:?}");
    // An undeclared property lives in the extras and reads the same way.
    let note = reader.node_property(p1, "note", &mut meter).unwrap();
    assert!(
        matches!(&note, BindingValue::Text(t) if &**t == "extra"),
        "{note:?}"
    );
    // The external key is a property too.
    let key = reader.node_property(p1, "_key", &mut meter).unwrap();
    assert!(
        matches!(&key, BindingValue::Text(t) if &**t == "p1"),
        "{key:?}"
    );
    assert_eq!(meter.work().base.primary_reads, 4);
}

#[test]
fn a_missing_property_and_a_stored_null_both_read_as_null() {
    let dir = tempfile::tempdir().unwrap();
    let f = fixture(dir.path());
    let reader = ElementReader::new(&f.db);
    let mut cancel = never;
    let mut meter = GqlMeter::new(GqlBudget::unlimited(), &mut cancel);
    let p2 = NodeRef(f.p2);

    for property in ["name", "age", "never_written"] {
        let value = reader.node_property(p2, property, &mut meter).unwrap();
        assert!(matches!(value, BindingValue::Null), "{property}: {value:?}");
    }
    // The storage difference survives in the names: a stored null is
    // present, an absent field is not.
    assert_eq!(
        reader.node_property_names(p2, &mut meter).unwrap(),
        vec!["_key", "name"]
    );
    assert_eq!(
        reader
            .node_property_names(NodeRef(f.p1), &mut meter)
            .unwrap(),
        vec!["_key", "age", "name", "note"]
    );
}

#[test]
fn labels_are_the_collection_and_the_edge_type() {
    let dir = tempfile::tempdir().unwrap();
    let mut f = fixture(dir.path());
    parallel(&mut f);
    let reader = ElementReader::new(&f.db);
    assert_eq!(reader.node_label(NodeRef(f.p1)).unwrap(), "person");
    let (key, id, _, _) = walk(&f, f.p1, Direction::Outgoing).remove(0);
    let edge = EdgeRef { key, id, bag: None };
    assert_eq!(reader.edge_label(&edge).unwrap(), "knows");
}

#[test]
fn element_ids_tell_apart_a_shared_key_and_parallel_edges() {
    let dir = tempfile::tempdir().unwrap();
    let mut f = fixture(dir.path());
    // A band stored under the same external key as a person.
    let b1 = f.db.put(f.band, "p1", &json!({ "name": "b1" })).unwrap();
    f.db.commit().unwrap();
    parallel(&mut f);

    let person = NodeRef(f.p1).element_id();
    let band = NodeRef(b1).element_id();
    assert_ne!(person, band);
    assert!(
        person.starts_with("n1:") && band.starts_with("n1:"),
        "{person} {band}"
    );
    assert_eq!(
        person,
        NodeRef(f.p1).element_id(),
        "the id is a function of the node"
    );

    let edges: Vec<String> = walk(&f, f.p1, Direction::Outgoing)
        .into_iter()
        .map(|(key, id, _, _)| EdgeRef { key, id, bag: None }.element_id())
        .collect();
    assert_eq!(edges.len(), 2);
    assert_ne!(edges[0], edges[1]);
    assert!(edges.iter().all(|e| e.starts_with("e1:")), "{edges:?}");
    // A node id and an edge id never collide.
    assert!(edges.iter().all(|e| *e != person));
}

/// A node or an edge the snapshot does not hold was never handed out by it:
/// the reader reports corruption, the category every traversal reports for
/// a posting whose row is gone -- not "not found".
#[test]
fn a_reference_to_a_missing_element_is_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let mut f = fixture(dir.path());
    let [a, b] = parallel(&mut f);
    let reader = ElementReader::new(&f.db);
    let mut cancel = never;
    let mut meter = GqlMeter::new(GqlBudget::unlimited(), &mut cancel);
    let corrupt = |result: QueryResult<BindingValue>| {
        assert!(
            matches!(result, Err(QueryError::Database(Error::Corrupt(_)))),
            "{result:?}"
        );
    };

    let (key, _, _, _) = walk(&f, f.p2, Direction::Incoming)[0];
    let gone = EdgeRef {
        key,
        id: a.max(b) + 1,
        bag: None,
    };
    corrupt(reader.edge_property(&gone, "since", &mut meter));
    let gone = NodeRef(EntityId {
        sequence: f.p2.sequence + 100,
        ..f.p2
    });
    corrupt(reader.node_property(gone, "name", &mut meter));
}

/// Sites of the tourism world with a text body, and a text index on it.
/// Many markets share `market`, so a read that walked that term's whole
/// posting list would show in the charge.
struct Sites {
    db: Database,
    nodes: Vec<EntityId>,
    body: IndexId,
    rating: IndexId,
}

const MARKETS: u64 = 60;

fn sites(path: &std::path::Path) -> Sites {
    let mut db = Database::create(path, cfg()).unwrap();
    let site = db
        .create_collection(
            "site",
            vec![("body".into(), Kind::Text), ("rating".into(), Kind::Int)],
            Default::default(),
        )
        .unwrap();
    let mut bodies = vec![
        (
            "temple",
            "a cliff temple where the kecak dance starts at sunset".to_owned(),
        ),
        (
            "beach",
            "a surf beach with a long sunset walk and a reef".to_owned(),
        ),
        ("reef", "a reef dive site, calm water, no beach".to_owned()),
        ("gallery", "woven baskets and carved masks".to_owned()),
        ("empty", String::new()),
    ];
    for i in 0..MARKETS {
        bodies.push((
            "market",
            format!("market stall {i} sells woven baskets at the market"),
        ));
    }
    let mut nodes = Vec::new();
    for (i, (kind, body)) in bodies.iter().enumerate() {
        nodes.push(
            db.put(
                site,
                &format!("{kind}_{i}"),
                &json!({ "body": body, "rating": i as i64 % 5 }),
            )
            .unwrap(),
        );
    }
    // One row with no body at all.
    nodes.push(db.put(site, "bare", &json!({ "rating": 1 })).unwrap());
    db.commit().unwrap();
    let body = db.create_text_index(site, "site_body", "body").unwrap();
    db.build_index_to_ready(body, 256).unwrap();
    let rating = db
        .create_scalar_index(site, "site_rating", "rating", false)
        .unwrap();
    db.build_index_to_ready(rating, 256).unwrap();
    Sites {
        db,
        nodes,
        body,
        rating,
    }
}

/// Every row of `db`'s collection a whole-collection query answers, with
/// its ranking value.
fn collection_rows(
    db: &Database,
    collection: CollectionId,
    filters: &[QueryFilter<'_>],
    order: QueryOrder<'_>,
) -> Vec<(EntityId, OrderValue)> {
    let mut query = db
        .prepare_query(QueryRequest {
            collection,
            filters,
            order,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let mut out = Vec::new();
    loop {
        let page = query
            .next_page(1024, QueryBudget::unlimited(), || false)
            .unwrap();
        out.extend(page.rows.into_iter().map(|row| (row.id, row.order)));
        if page.done {
            return out;
        }
    }
}

/// The queries every text test asks, in every match mode but the
/// typo-tolerant one: single terms, several terms, a phrase that holds and
/// one whose words are present but not adjacent, and a term no row holds.
const QUERIES: [&str; 6] = [
    "sunset",
    "woven baskets",
    "reef beach",
    "sunset walk",
    "beach walk",
    "volcano",
];
const MODES: [TextMatch; 3] = [TextMatch::Any, TextMatch::All, TextMatch::Phrase];

#[test]
fn a_per_node_text_match_equals_the_collection_text_query_restricted_to_that_node() {
    let dir = tempfile::tempdir().unwrap();
    let s = sites(dir.path());
    let collection = s.nodes[0].collection;
    let reader = ElementReader::new(&s.db);
    let mut cancel = never;
    let mut meter = GqlMeter::new(GqlBudget::unlimited(), &mut cancel);
    for query in QUERIES {
        for matching in MODES {
            let wanted: Vec<EntityId> = collection_rows(
                &s.db,
                collection,
                &[QueryFilter::Text {
                    index: s.body,
                    query,
                    matching,
                }],
                QueryOrder::EntityId,
            )
            .into_iter()
            .map(|(id, _)| id)
            .collect();
            for &node in &s.nodes {
                let got = reader
                    .text_matches(NodeRef(node), s.body, query, matching, &mut meter)
                    .unwrap();
                assert_eq!(
                    got,
                    wanted.contains(&node),
                    "{query:?} {matching:?} on {node:?}"
                );
            }
        }
    }
}

#[test]
fn a_per_node_text_score_equals_the_collection_bm25_score_and_a_non_match_scores_zero() {
    let dir = tempfile::tempdir().unwrap();
    let s = sites(dir.path());
    let collection = s.nodes[0].collection;
    let reader = ElementReader::new(&s.db);
    let mut cancel = never;
    let mut meter = GqlMeter::new(GqlBudget::unlimited(), &mut cancel);
    for query in QUERIES {
        for matching in MODES {
            // The ranking a whole-collection BM25 order gives the matches.
            let ranked = collection_rows(
                &s.db,
                collection,
                &[],
                QueryOrder::Bm25 {
                    index: s.body,
                    query,
                    matching,
                },
            );
            // The SQL `bm25()` leaf over EVERY row: a non-match scores 0.0.
            let leaf = ScoreExpr::Bm25 {
                index: s.body,
                query,
                matching,
            };
            let scored = collection_rows(
                &s.db,
                collection,
                &[],
                QueryOrder::Score {
                    expr: &leaf,
                    direction: SortDirection::Descending,
                },
            );
            assert_eq!(scored.len(), s.nodes.len());
            for &node in &s.nodes {
                let got = reader
                    .text_score(NodeRef(node), s.body, query, matching, &mut meter)
                    .unwrap();
                let bm25 =
                    ranked
                        .iter()
                        .find(|(id, _)| *id == node)
                        .map(|(_, order)| match order {
                            OrderValue::Bm25(score) => *score,
                            other => panic!("a BM25 order reports {other:?}"),
                        });
                let leaf = match scored.iter().find(|(id, _)| *id == node) {
                    Some((_, OrderValue::Score(score))) => *score,
                    other => panic!("a score order reports {other:?}"),
                };
                assert_eq!(
                    got,
                    bm25.unwrap_or(0.0),
                    "{query:?} {matching:?} on {node:?}"
                );
                assert_eq!(got, leaf, "{query:?} {matching:?} on {node:?}");
            }
        }
    }
}

/// One read is ONE candidate, and tests that candidate's postings -- the
/// document's norm and one point read per query term, each a `text_postings`
/// unit as the engine's scorer charges them -- rather than walking a term's
/// posting list: `market` is held by sixty rows and a read of it charges at
/// most two.
#[test]
fn a_per_node_text_read_charges_one_candidate_and_its_own_postings() {
    let dir = tempfile::tempdir().unwrap();
    let s = sites(dir.path());
    let reader = ElementReader::new(&s.db);
    let market = NodeRef(s.nodes[5]);
    let temple = NodeRef(s.nodes[0]);
    type Read = fn(
        &ElementReader<'_>,
        NodeRef,
        IndexId,
        &str,
        TextMatch,
        &mut GqlMeter<'_, fn() -> bool>,
    ) -> QueryResult<()>;
    let reads: [(&str, Read); 2] = [
        ("text_matches", |r, n, i, q, m, meter| {
            r.text_matches(n, i, q, m, meter).map(drop)
        }),
        ("text_score", |r, n, i, q, m, meter| {
            r.text_score(n, i, q, m, meter).map(drop)
        }),
    ];
    for (name, read) in reads {
        for (node, query, terms) in [
            (market, "market", 1),
            (temple, "market", 1),
            (market, "woven baskets market", 3),
        ] {
            let mut cancel: fn() -> bool = never;
            let mut meter = GqlMeter::new(GqlBudget::unlimited(), &mut cancel);
            read(&reader, node, s.body, query, TextMatch::Any, &mut meter).unwrap();
            let work = meter.work().base;
            assert_eq!(work.candidates, 1, "{name} {query:?}: {work:?}");
            assert!(
                (1..=1 + terms).contains(&work.text_postings),
                "{name} {query:?} over {MARKETS} markets charged {} text postings",
                work.text_postings
            );
        }
        // A phrase is decided by the row's own tokens, and says so.
        let mut cancel: fn() -> bool = never;
        let mut meter = GqlMeter::new(GqlBudget::unlimited(), &mut cancel);
        read(
            &reader,
            temple,
            s.body,
            "kecak dance",
            TextMatch::Phrase,
            &mut meter,
        )
        .unwrap();
        assert!(
            meter.work().base.text_tokens > 0,
            "{name}: {:?}",
            meter.work()
        );
    }
}

/// A refusal inside the one-id query is the answer: a budget ceiling is
/// refused by name against the GQL page's own budget, and an index that is
/// not a text index is an error -- neither is ever "no match".
#[test]
fn a_refusal_inside_a_per_node_text_read_is_returned_not_turned_into_no_match() {
    let dir = tempfile::tempdir().unwrap();
    let s = sites(dir.path());
    let reader = ElementReader::new(&s.db);
    let temple = NodeRef(s.nodes[0]);
    let mut cancel = never;

    // Out of text postings: refused naming the resource.
    let budget = GqlBudget::from_query_budget(QueryBudget {
        text_postings: 0,
        ..QueryBudget::unlimited()
    });
    let mut meter = GqlMeter::new(budget, &mut cancel);
    for result in [
        reader.text_matches(temple, s.body, "sunset", TextMatch::Any, &mut meter),
        reader
            .text_score(temple, s.body, "sunset", TextMatch::Any, &mut meter)
            .map(|_| true),
    ] {
        assert!(
            matches!(
                result,
                Err(QueryError::BudgetExceeded {
                    resource: WorkResource::TextPostings,
                    limit: 0,
                    ..
                })
            ),
            "{result:?}"
        );
    }

    // A ceiling the page has partly spent is restated against the page:
    // the limit it was given and the total the charge would have reached.
    let budget = GqlBudget::from_query_budget(QueryBudget {
        text_postings: 3,
        ..QueryBudget::unlimited()
    });
    let mut meter = GqlMeter::new(budget, &mut cancel);
    let mut refused = None;
    for _ in 0..4 {
        if let Err(error) =
            reader.text_matches(temple, s.body, "sunset", TextMatch::Any, &mut meter)
        {
            refused = Some(error);
            break;
        }
    }
    assert!(
        matches!(
            refused,
            Some(QueryError::BudgetExceeded {
                resource: WorkResource::TextPostings,
                limit: 3,
                attempted: 4,
            })
        ),
        "{refused:?}"
    );

    // Not a text index.
    let mut meter = GqlMeter::new(GqlBudget::unlimited(), &mut cancel);
    let result = reader.text_matches(temple, s.rating, "sunset", TextMatch::Any, &mut meter);
    assert!(result.is_err(), "{result:?}");
    let result = reader.text_score(temple, s.rating, "sunset", TextMatch::Any, &mut meter);
    assert!(result.is_err(), "{result:?}");
}

/// The typo-tolerant search truncates its dictionary walk with a notice
/// (`QL_CONTRACT` §4.6), and a per-node answer is one boolean or one number
/// with nowhere to carry it: refused by name, never answered short (Q30).
#[test]
fn a_typo_tolerant_search_is_refused_per_node() {
    let dir = tempfile::tempdir().unwrap();
    let s = sites(dir.path());
    let reader = ElementReader::new(&s.db);
    let temple = NodeRef(s.nodes[0]);
    let mut cancel = never;
    let mut meter = GqlMeter::new(GqlBudget::unlimited(), &mut cancel);
    let matched = reader.text_matches(temple, s.body, "sunst", TextMatch::Search, &mut meter);
    let scored = reader.text_score(temple, s.body, "sunst", TextMatch::Search, &mut meter);
    for message in [
        format!("{:?}", matched.map(|_| ())),
        format!("{:?}", scored.map(|_| ())),
    ] {
        assert!(message.contains("search()"), "{message}");
    }
    assert_eq!(meter.work().base.text_postings, 0);
}
