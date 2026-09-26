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

use sekejap_core::collections::gql::{
    BindingValue, EdgeRef, ElementReader, GqlBudget, GqlMeter, NodeRef,
};
use sekejap_core::collections::{
    AdjacencyCursor, CollectionId, Database, Direction, EdgeTypeId, EntityId, Error,
    GraphContextId, QueryError, QueryResult,
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
