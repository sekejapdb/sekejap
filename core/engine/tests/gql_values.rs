//! The GQL profile's binding values (`docs/lang/GQL_PROFILE_DESIGN.md` §2):
//! element identity, the grouping equality and hash, the internal total
//! order, shared-prefix paths and the stated `held_bytes` estimate.
//!
//! The data is invented: collections `people` and `places`, keys `k1`...

use sekejap_core::collections::gql::{
    BindingRow, BindingValue, EdgeRef, ListRef, NodeRef, PathRef, SlotId, ValueType,
};
use sekejap_core::collections::{CollectionId, CollectionOptions, Database, EdgeKey, EntityId};
use sekejap_core::Kind;
use serde_json::json;
use std::{
    cmp::Ordering,
    collections::{hash_map::DefaultHasher, HashSet},
    hash::{Hash, Hasher},
    sync::Arc,
};

mod common;
use common::cfg;

fn node(collection: u32, sequence: u64) -> NodeRef {
    NodeRef(EntityId {
        collection: CollectionId(collection),
        sequence,
    })
}

/// An edge between two synthetic nodes. `EdgeKey`'s context and type ids are
/// only compared here, never resolved, so small invented numbers do.
fn edge(from: NodeRef, to: NodeRef, edge_type: u64, id: u64) -> EdgeRef {
    use sekejap_core::collections::{EdgeTypeId, GraphContextId};
    EdgeRef {
        key: EdgeKey {
            source: from.0,
            context: GraphContextId(0),
            edge_type: EdgeTypeId(edge_type),
            destination: to.0,
        },
        id,
        bag: None,
    }
}

fn hash_of(v: &BindingValue) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

fn row(slots: Vec<BindingValue>) -> BindingRow {
    BindingRow {
        slots: slots.into_boxed_slice(),
    }
}

#[test]
fn two_collections_sharing_a_key_are_two_distinct_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("db"), cfg()).unwrap();
    let fields = || vec![("name".into(), Kind::Text)];
    let people = db
        .create_collection("people", fields(), CollectionOptions::default())
        .unwrap();
    let places = db
        .create_collection("places", fields(), CollectionOptions::default())
        .unwrap();
    let a = NodeRef(db.put(people, "k1", &json!({"name": "same"})).unwrap());
    let b = NodeRef(db.put(places, "k1", &json!({"name": "same"})).unwrap());

    // Same external key, same property value, different collection: two
    // nodes. Identity is the EntityId, never the key.
    assert_ne!(a, b);
    let (va, vb) = (BindingValue::Node(a), BindingValue::Node(b));
    assert_eq!(va.identity_eq(&vb), Some(false));
    assert_eq!(va.identity_eq(&va.clone()), Some(true));
    let distinct: HashSet<BindingValue> = [va.clone(), vb.clone(), va.clone()].into();
    assert_eq!(distinct.len(), 2);

    // Their PROPERTY values, read as scalars, are equal: property equality is
    // value equality and is a different question.
    let name = |s: &str| BindingValue::Text(Arc::from(s));
    assert_eq!(name("same").identity_eq(&name("same")), Some(true));
}

#[test]
fn identity_equality_is_not_property_equality() {
    let (n1, n2) = (node(1, 1), node(1, 2));
    let plain = edge(n1, n2, 7, 0);
    let mut with_bag = plain.clone();
    with_bag.bag = Some(Arc::new(json!({"w": 1})));
    let mut other_bag = plain.clone();
    other_bag.bag = Some(Arc::new(json!({"w": 2})));
    // The cached bag is not identity: same key and id is the same edge.
    let e = |r: &EdgeRef| BindingValue::Edge(r.clone());
    assert_eq!(e(&plain).identity_eq(&e(&with_bag)), Some(true));
    assert_eq!(e(&with_bag).identity_eq(&e(&other_bag)), Some(true));
    assert_eq!(hash_of(&e(&with_bag)), hash_of(&e(&other_bag)));
    // A parallel edge: same key, another id, another edge.
    let parallel = edge(n1, n2, 7, 1);
    assert_eq!(e(&plain).identity_eq(&e(&parallel)), Some(false));
    assert_ne!(e(&plain), e(&parallel));

    // Null is unknown under identity equality, yet groups with Null.
    assert_eq!(BindingValue::Null.identity_eq(&BindingValue::Null), None);
    assert_eq!(BindingValue::Null.identity_eq(&BindingValue::Int(1)), None);
    assert_eq!(BindingValue::Null, BindingValue::Null);
    // Numbers compare as numbers.
    assert_eq!(
        BindingValue::Int(1).identity_eq(&BindingValue::Float(1.0)),
        Some(true)
    );
    // A node never equals a scalar, an edge or a path.
    let n = BindingValue::Node(n1);
    assert_eq!(n.identity_eq(&BindingValue::Int(1)), Some(false));
    assert_eq!(n.identity_eq(&e(&plain)), Some(false));
    assert_eq!(
        n.identity_eq(&BindingValue::Path(PathRef::new(n1))),
        Some(false)
    );
    // Lists: element by element, under three-valued logic.
    let list = |items: Vec<BindingValue>| {
        BindingValue::List(ListRef {
            items: items.into(),
            elem: ValueType::Int,
        })
    };
    let one_two = list(vec![BindingValue::Int(1), BindingValue::Int(2)]);
    assert_eq!(one_two.identity_eq(&one_two.clone()), Some(true));
    assert_eq!(
        one_two.identity_eq(&list(vec![BindingValue::Int(1), BindingValue::Null])),
        None
    );
    assert_eq!(
        one_two.identity_eq(&list(vec![BindingValue::Int(3), BindingValue::Null])),
        Some(false)
    );
    assert_eq!(one_two.identity_eq(&list(vec![BindingValue::Int(1)])), Some(false));
}

#[test]
fn a_path_extension_shares_its_whole_prefix() {
    let ns: Vec<NodeRef> = (0..4).map(|i| node(1, i)).collect();
    let p0 = PathRef::new(ns[0]);
    let p1 = p0.extend(edge(ns[0], ns[1], 1, 0), true, ns[1]);
    let qa = p1.extend(edge(ns[1], ns[2], 1, 0), true, ns[2]);
    let qb = p1.extend(edge(ns[3], ns[1], 1, 0), false, ns[3]);
    assert_eq!((p0.len(), p1.len(), qa.len(), qb.len()), (0, 1, 2, 2));
    // Extending leaves the prefix as it was.
    assert_eq!(p1.end(), ns[1]);
    assert_eq!((qa.start(), qa.end()), (ns[0], ns[2]));
    assert_eq!((qb.start(), qb.end()), (ns[0], ns[3]));

    // The bytes prove the sharing: a row holding both branches counts the
    // shared prefix once, so it holds exactly the prefix less than the two
    // branches held apart.
    let slot = BindingValue::Null.held_bytes();
    let path = |p: &PathRef| BindingValue::Path(p.clone());
    let apart = path(&qa).held_bytes() + path(&qb).held_bytes();
    let together = row(vec![path(&qa), path(&qb)]).held_bytes();
    let prefix_heap = path(&p1).held_bytes() - slot;
    assert_eq!(apart - together, prefix_heap);

    // k branches from a long prefix hold the prefix once: O(k), not O(k*L).
    let mut long = p0.clone();
    let mut at = ns[0];
    for i in 0..50u64 {
        let next = node(2, i);
        long = long.extend(edge(at, next, 1, 0), true, next);
        at = next;
    }
    let branches: Vec<BindingValue> = (0..100u64)
        .map(|i| {
            let leaf = node(3, i);
            path(&long.extend(edge(at, leaf, 1, 0), true, leaf))
        })
        .collect();
    let one_branch_heap = branches[0].held_bytes() - slot;
    let long_heap = path(&long).held_bytes() - slot;
    let step_heap = one_branch_heap - long_heap;
    assert_eq!(
        row(branches).held_bytes(),
        100 * slot + long_heap + 100 * step_heap
    );
    // A list of branch paths shares the same way.
    let a = long.extend(edge(at, ns[1], 1, 0), true, ns[1]);
    let b = long.extend(edge(at, ns[2], 1, 0), true, ns[2]);
    let listed = BindingValue::List(ListRef {
        items: vec![path(&a), path(&b)].into(),
        elem: ValueType::Path,
    });
    let unshared = path(&a).held_bytes() + path(&b).held_bytes() - 2 * slot;
    assert!(listed.held_bytes() < unshared);
}

#[test]
fn a_zero_length_path_is_one_node_and_no_edge() {
    let (n, m) = (node(1, 10), node(1, 11));
    let z = PathRef::new(n);
    assert_eq!(z.len(), 0);
    assert_eq!((z.start(), z.end()), (n, n));
    let pz = BindingValue::Path(z.clone());
    // Built twice, it is the same path.
    let again = BindingValue::Path(PathRef::new(n));
    assert_eq!(pz.identity_eq(&again), Some(true));
    assert_eq!(pz, again);
    assert_eq!(hash_of(&pz), hash_of(&again));
    // Another start is another path.
    assert_eq!(pz.identity_eq(&BindingValue::Path(PathRef::new(m))), Some(false));
    // A self-loop of length one is not the zero-length path at its node.
    let looped = z.extend(edge(n, n, 1, 0), true, n);
    assert_eq!((looped.len(), looped.start(), looped.end()), (1, n, n));
    assert_eq!(pz.identity_eq(&BindingValue::Path(looped)), Some(false));
    // It holds a start and nothing else, which is more than a null slot.
    assert!(pz.held_bytes() > BindingValue::Null.held_bytes());
}

/// A path as long as a trail search may build -- one step per frame, up to
/// the `queue_entries` cap -- is dropped without recursing once per step: a
/// long path is a refusal or an answer, never a stack overflow. Run on a
/// thread with a small stack so that a recursive drop fails here.
#[test]
fn a_long_path_drops_without_deep_recursion() {
    std::thread::Builder::new()
        .stack_size(256 << 10)
        .spawn(|| {
            let (a, b) = (node(1, 1), node(1, 2));
            let mut path = PathRef::new(a);
            for i in 0..300_000u64 {
                let (from, to) = if i % 2 == 0 { (a, b) } else { (b, a) };
                path = path.extend(edge(from, to, 1, i), true, to);
            }
            // A shared prefix stays whole while one holder is left.
            let keep = path.clone();
            drop(path);
            assert_eq!(keep.len(), 300_000);
            drop(keep);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn paths_compare_by_start_edges_and_orientation() {
    let (a, b) = (node(1, 1), node(1, 2));
    let ab = edge(a, b, 1, 0);
    let fwd = PathRef::new(a).extend(ab.clone(), true, b);
    let fwd_again = PathRef::new(a).extend(ab.clone(), true, b);
    // Built twice, it is the same path.
    assert_eq!(
        BindingValue::Path(fwd.clone()),
        BindingValue::Path(fwd_again.clone())
    );
    assert_eq!(
        hash_of(&BindingValue::Path(fwd.clone())),
        hash_of(&BindingValue::Path(fwd_again))
    );
    // The same stored edge crossed the other way is another path.
    let back = PathRef::new(b).extend(ab, false, a);
    assert_ne!(BindingValue::Path(fwd), BindingValue::Path(back));
}

/// A value of every kind, including the numeric corners the total order has
/// to handle: `-0.0`, NaN, and integers beyond an `f64`'s exact range.
fn samples() -> Vec<BindingValue> {
    let (n1, n2) = (node(1, 1), node(2, 1));
    let e1 = edge(n1, n2, 1, 0);
    let e2 = edge(n1, n2, 1, 1);
    let p = PathRef::new(n1).extend(e1.clone(), true, n2);
    let text = |s: &str| BindingValue::Text(Arc::from(s));
    let list = |items: Vec<BindingValue>| {
        BindingValue::List(ListRef {
            items: items.into(),
            elem: ValueType::Int,
        })
    };
    vec![
        BindingValue::Null,
        BindingValue::Bool(false),
        BindingValue::Bool(true),
        BindingValue::Int(-1),
        BindingValue::Int(0),
        BindingValue::Float(0.0),
        BindingValue::Float(-0.0),
        BindingValue::Int(1),
        BindingValue::Float(1.0),
        BindingValue::Float(1.5),
        BindingValue::Float(-1.5),
        BindingValue::Int(i64::MAX),
        BindingValue::Int(i64::MIN),
        BindingValue::Float(9_223_372_036_854_775_808.0),
        BindingValue::Float(-9_223_372_036_854_775_808.0),
        BindingValue::Int(9_007_199_254_740_993),
        BindingValue::Float(9_007_199_254_740_992.0),
        BindingValue::Float(f64::INFINITY),
        BindingValue::Float(f64::NEG_INFINITY),
        BindingValue::Float(f64::NAN),
        text(""),
        text("a"),
        text("b"),
        BindingValue::Json(Arc::new(json!({"a": 1, "b": [1, 2]}))),
        BindingValue::Json(Arc::new(json!({"b": [1.0, 2], "a": 1.0}))),
        BindingValue::Json(Arc::new(json!([1, "x", null]))),
        BindingValue::Json(Arc::new(json!(null))),
        BindingValue::Vector(Arc::from(vec![1.0f32, 2.0])),
        BindingValue::Vector(Arc::from(vec![1.0f32])),
        BindingValue::Geo(Arc::new(json!({"type": "Point", "coordinates": [1.0, 2.0]}))),
        BindingValue::Bytes(Arc::from(vec![1u8, 2])),
        BindingValue::Node(n1),
        BindingValue::Node(n2),
        BindingValue::Edge(e1),
        BindingValue::Edge(e2),
        BindingValue::Path(PathRef::new(n1)),
        BindingValue::Path(p),
        list(vec![]),
        list(vec![BindingValue::Int(1)]),
        list(vec![BindingValue::Float(1.0)]),
        list(vec![BindingValue::Int(1), BindingValue::Null]),
    ]
}

#[test]
fn equality_hash_and_total_order_agree() {
    let values = samples();
    for a in &values {
        assert_eq!(a.cmp(a), Ordering::Equal, "{a:?} is not equal to itself");
        for b in &values {
            let ord = a.cmp(b);
            assert_eq!(ord, b.cmp(a).reverse(), "{a:?} vs {b:?} is not antisymmetric");
            assert_eq!(a == b, ord == Ordering::Equal, "{a:?} vs {b:?}: eq disagrees with cmp");
            if a == b {
                assert_eq!(hash_of(a), hash_of(b), "{a:?} == {b:?} but hashes differ");
            }
            for c in &values {
                if ord != Ordering::Greater && b.cmp(c) != Ordering::Greater {
                    assert_ne!(a.cmp(c), Ordering::Greater, "{a:?} <= {b:?} <= {c:?} broken");
                }
            }
        }
    }
    let mut sorted = values.clone();
    sorted.sort();
    let rank = |v: &BindingValue| sorted.iter().position(|s| s == v).unwrap();
    // The stated cross-type order.
    let order = [
        BindingValue::Null,
        BindingValue::Bool(true),
        BindingValue::Int(i64::MIN),
        BindingValue::Text(Arc::from("")),
        BindingValue::Json(Arc::new(json!(null))),
        BindingValue::Vector(Arc::from(vec![1.0f32])),
        BindingValue::Geo(Arc::new(json!({"type": "Point", "coordinates": [1.0, 2.0]}))),
        BindingValue::Bytes(Arc::from(vec![1u8, 2])),
        BindingValue::Node(node(1, 1)),
        BindingValue::Edge(edge(node(1, 1), node(2, 1), 1, 0)),
        BindingValue::Path(PathRef::new(node(1, 1))),
        BindingValue::List(ListRef {
            items: vec![].into(),
            elem: ValueType::Int,
        }),
    ];
    for pair in order.windows(2) {
        assert!(rank(&pair[0]) < rank(&pair[1]), "{:?} !< {:?}", pair[0], pair[1]);
    }
    // Numbers: exact across Int and Float, -0.0 is 0, NaN sorts last.
    assert_eq!(BindingValue::Int(0), BindingValue::Float(-0.0));
    assert_eq!(BindingValue::Int(1), BindingValue::Float(1.0));
    assert!(BindingValue::Int(i64::MAX) < BindingValue::Float(9_223_372_036_854_775_808.0));
    assert!(BindingValue::Int(9_007_199_254_740_993) > BindingValue::Float(9_007_199_254_740_992.0));
    assert!(BindingValue::Float(f64::INFINITY) < BindingValue::Float(f64::NAN));
    assert_eq!(BindingValue::Float(f64::NAN), BindingValue::Float(f64::NAN));
    assert!(BindingValue::Float(-1.5) < BindingValue::Int(-1));
    // JSON numbers too, and object key order does not matter.
    assert_eq!(
        BindingValue::Json(Arc::new(json!({"a": 1, "b": [1, 2]}))),
        BindingValue::Json(Arc::new(json!({"b": [1.0, 2], "a": 1.0})))
    );
}

#[test]
fn held_bytes_grow_with_what_is_held() {
    let text = |n: usize| BindingValue::Text(Arc::from("x".repeat(n)));
    assert!(text(0).held_bytes() > BindingValue::Null.held_bytes());
    assert!(text(10).held_bytes() < text(11).held_bytes());
    let json = |n: usize| BindingValue::Json(Arc::new(json!((0..n).collect::<Vec<_>>())));
    assert!(json(1).held_bytes() < json(2).held_bytes());
    let vector = |n: usize| BindingValue::Vector(Arc::from(vec![0.0f32; n]));
    assert!(vector(3).held_bytes() < vector(4).held_bytes());
    let list = |n: usize| {
        BindingValue::List(ListRef {
            items: vec![BindingValue::Int(7); n].into(),
            elem: ValueType::Int,
        })
    };
    assert!(list(0).held_bytes() < list(1).held_bytes());
    assert!(list(1).held_bytes() < list(2).held_bytes());

    let (a, b) = (node(1, 1), node(1, 2));
    let mut path = PathRef::new(a);
    let mut last = BindingValue::Path(path.clone()).held_bytes();
    for i in 0..5 {
        // Back and forth over parallel a -> b edges: forward from a,
        // backward from b.
        let at_a = path.end() == a;
        path = path.extend(edge(a, b, 1, i), at_a, if at_a { b } else { a });
        let now = BindingValue::Path(path.clone()).held_bytes();
        assert!(now > last);
        last = now;
    }
    let plain = edge(a, b, 1, 0);
    let mut bagged = plain.clone();
    bagged.bag = Some(Arc::new(json!({"w": "some text"})));
    assert!(BindingValue::Edge(plain).held_bytes() < BindingValue::Edge(bagged).held_bytes());

    // A row is its slots.
    let short = row(vec![BindingValue::Int(1)]);
    let longer = row(vec![BindingValue::Int(1), text(3)]);
    assert!(short.held_bytes() < longer.held_bytes());
    assert_eq!(
        longer.held_bytes(),
        BindingValue::Int(1).held_bytes() + text(3).held_bytes()
    );
    assert_eq!(longer.get(SlotId(1)), &text(3));
}
