//! Independent edge identity: `docs/core/GRAPH_CONTRACT.md` §2.3.
//!
//! An edge created with `Database::create_edge` carries its own id, so two
//! edges of the same (source, context, type, destination) coexist, each with
//! its own properties. The id is an extra segment at the END of both edge keys
//! (`0x71 ... destination || ordered(id)` and its `0x72` mirror), behind the
//! additive feature bit `EDGE_ID_FEATURE = 0x40000`.
//!
//! What is at risk, one test each: that parallel edges both survive the write
//! and are both seen by every read (neighbours, BFS, per-hop predicates); that
//! an update or a delete by id touches exactly that edge and nothing else --
//! the endpoint sets (§2.7) included; that ids survive a reopen and are never
//! handed out twice; that the old tuple-keyed `put_edge` still upserts; that a
//! file which never creates an id-bearing edge is byte-for-byte what it was
//! and reads the same; that a binary predating the bit refuses a file carrying
//! it as `Unsupported`, never `Corrupt` (Law 8); and that the verifier and the
//! rebuild both understand the new keys.
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::{
    collections::{
        rebuild::{rebuild_derived_indexes, RebuildLimits},
        verification::{verify_indexed_source, VerificationLimits},
        BfsRequest, Cmp, CollectionId, Database, Direction, EdgeId, EdgeKey, EdgePredicate,
        EdgeTypeId, EntityId, Error, GraphContextId, NeighborRequest, QueryBudget, ScalarValue,
        EDGE_ID_FEATURE, ENDPOINT_FEATURE, SUPPORTED_LOGICAL_FEATURES,
    },
    internal::{admit_logical_features, logical_features},
    pagewal::PageWalStore,
    Kind,
};
use serde_json::json;
use std::path::Path;

const PRIMARY: u8 = 0x71;
const REVERSE: u8 = 0x72;

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

/// `0x80 + width`, then minimal unsigned big-endian: the frozen ordered
/// integer encoding, spelled here so the test reads keys with its own decoder.
fn ordered(n: u64) -> Vec<u8> {
    let b = n.to_be_bytes();
    let start = b.iter().position(|x| *x != 0).unwrap_or(7);
    let mut k = vec![0x80 + (8 - start) as u8];
    k.extend_from_slice(&b[start..]);
    k
}

fn entity(k: &mut Vec<u8>, id: EntityId) {
    k.extend(ordered(u64::from(id.collection.0)));
    k.extend(ordered(id.sequence));
}

/// The key this test expects for one edge. `id == 0` is the implicit identity
/// of a tuple-keyed edge and carries no segment at all.
fn expected_key(tag: u8, e: EdgeKey, id: u64) -> Vec<u8> {
    let mut k = vec![tag];
    let (near, far) = if tag == PRIMARY {
        (e.source, e.destination)
    } else {
        (e.destination, e.source)
    };
    entity(&mut k, near);
    k.extend(ordered(e.context.0));
    k.extend(ordered(e.edge_type.0));
    entity(&mut k, far);
    if id != 0 {
        k.extend(ordered(id));
    }
    k
}

/// Every key of one tag on disk, raw.
fn keyspace(path: &Path, tag: u8) -> Vec<(Vec<u8>, Vec<u8>)> {
    let raw = PageWalStore::open_snapshot(path, 1 << 20).unwrap();
    let mut out = Vec::new();
    for row in raw.range(&[tag]).unwrap() {
        let (key, value) = row.unwrap();
        if key.first() != Some(&tag) {
            break;
        }
        out.push((key, value));
    }
    out
}

struct Fixture {
    db: Database,
    place: CollectionId,
    ids: Vec<EntityId>,
    road: EdgeTypeId,
}

fn fixture(path: &Path, rows: usize) -> Fixture {
    let mut db = Database::create(path, cfg()).unwrap();
    let place = db
        .create_collection("place", vec![("n".into(), Kind::Int)], Default::default())
        .unwrap();
    db.enable_graph().unwrap();
    let road = db.create_edge_type("road").unwrap();
    let mut ids = Vec::new();
    for n in 0..rows {
        ids.push(db.put(place, &format!("p{n}"), &json!({ "n": n })).unwrap());
    }
    db.commit().unwrap();
    Fixture {
        db,
        place,
        ids,
        road,
    }
}

fn tuple(f: &Fixture, s: usize, d: usize) -> EdgeKey {
    EdgeKey {
        source: f.ids[s],
        context: GraphContextId::BASE,
        edge_type: f.road,
        destination: f.ids[d],
    }
}

fn out_of(f: &Fixture, s: usize) -> NeighborRequest {
    NeighborRequest {
        entity: f.ids[s],
        direction: Direction::Outgoing,
        context: GraphContextId::BASE,
        edge_type: Some(f.road),
        limit: 256,
    }
}

fn bfs<'a>(f: &Fixture, seed: usize, edge_where: &'a [EdgePredicate<'a>]) -> BfsRequest<'a> {
    BfsRequest {
        seed: f.ids[seed],
        direction: Direction::Outgoing,
        context: GraphContextId::BASE,
        edge_type: Some(f.road),
        min_depth: 1,
        max_depth: 4,
        include_seed: false,
        max_visited: 65_536,
        max_edges: 1_000_000,
        result_limit: 65_536,
        edge_where,
        node_where: &[],
    }
}

fn endpoints(f: &Fixture, dir: Direction) -> Vec<u64> {
    f.db.edge_endpoints(
        f.place,
        GraphContextId::BASE,
        f.road,
        dir,
        usize::MAX,
        usize::MAX,
        QueryBudget::unlimited(),
        || false,
    )
    .unwrap()
    .into_iter()
    .map(|id| id.sequence)
    .collect()
}

/// Close the handle (the verifier takes the file's writer lock), verify the
/// file is clean, and reopen it.
fn verified(f: Fixture, path: &Path) -> Fixture {
    let Fixture {
        db,
        place,
        ids,
        road,
    } = f;
    drop(db);
    clean(path);
    Fixture {
        db: Database::open(path, cfg()).unwrap(),
        place,
        ids,
        road,
    }
}

/// One edge's properties, found by its id in its source's adjacency -- the
/// read path the id is for -- or `None` when no such edge is there.
fn edge(db: &Database, e: EdgeId) -> Option<serde_json::Value> {
    db.neighbors(NeighborRequest {
        entity: e.key.source,
        direction: Direction::Outgoing,
        context: e.key.context,
        edge_type: Some(e.key.edge_type),
        limit: 256,
    })
    .unwrap()
    .into_iter()
    .find(|found| found.key == e.key && found.id == e.id)
    .map(|found| found.properties)
}

fn clean(path: &Path) {
    let report = verify_indexed_source(path, VerificationLimits::default(), |issue| {
        panic!("unexpected verifier issue: {issue:?}")
    })
    .unwrap();
    assert!(report.complete && report.clean, "{report:?}");
}

// ── parallel edges on the write and the read path ────────────────────────

#[test]
fn parallel_edges_with_different_weights_both_survive_and_both_match() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut f = fixture(&path, 3);
    let key = tuple(&f, 0, 1);
    let light =
        f.db.create_edge(
            key.context,
            key.source,
            key.edge_type,
            key.destination,
            &json!({"w": 1}),
        )
        .unwrap();
    let heavy =
        f.db.create_edge(
            key.context,
            key.source,
            key.edge_type,
            key.destination,
            &json!({"w": 5}),
        )
        .unwrap();
    f.db.commit().unwrap();
    assert_eq!(light.key, key);
    assert_eq!(heavy.key, key);
    assert_ne!(light.id, 0, "an explicitly created edge has an explicit id");
    assert_ne!(heavy.id, 0);
    assert_ne!(light.id, heavy.id, "every create is a NEW edge");

    // Outgoing: both edges, each with its own id and its own bag.
    let out = f.db.neighbors(out_of(&f, 0)).unwrap();
    let mut got: Vec<(u64, serde_json::Value)> =
        out.iter().map(|e| (e.id, e.properties.clone())).collect();
    got.sort_by_key(|g| g.0);
    let mut want = vec![(light.id, json!({"w": 1})), (heavy.id, json!({"w": 5}))];
    want.sort_by_key(|g| g.0);
    assert_eq!(got, want);
    assert!(out.iter().all(|e| e.key == key));

    // Incoming reads the properties back through the mirror, one per edge.
    let incoming =
        f.db.neighbors(NeighborRequest {
            direction: Direction::Incoming,
            ..out_of(&f, 1)
        })
        .unwrap();
    let mut got: Vec<(u64, serde_json::Value)> = incoming
        .iter()
        .map(|e| (e.id, e.properties.clone()))
        .collect();
    got.sort_by_key(|g| g.0);
    assert_eq!(got, want);

    // Adjacency is still DISTINCT entities.
    assert_eq!(f.db.neighbor_ids(out_of(&f, 0)).unwrap(), vec![f.ids[1]]);

    // A per-hop predicate that only the heavy edge satisfies still reaches
    // the destination, and binds THAT edge.
    let heavy_only = [EdgePredicate {
        property: "w",
        op: Cmp::Gt,
        value: ScalarValue::I64(3),
    }];
    let walked =
        f.db.traverse_bfs_binding_edges(bfs(&f, 0, &heavy_only))
            .unwrap();
    assert_eq!(walked.nodes.len(), 1);
    let via = walked.nodes[0].via.as_ref().unwrap();
    assert_eq!((via.key, via.id), (key, heavy.id));
    assert_eq!(via.properties, json!({"w": 5}));
    // Unfiltered, the destination is one node and both edges were walked.
    let walked = f.db.traverse_bfs(bfs(&f, 0, &[])).unwrap();
    assert_eq!(walked.nodes.len(), 1);
    assert_eq!(walked.scanned_edges, 2);

    // Both survive a reopen, with their ids.
    drop(f.db);
    let db = Database::open(&path, cfg()).unwrap();
    assert_eq!(edge(&db, light).unwrap(), json!({"w": 1}));
    assert_eq!(edge(&db, heavy).unwrap(), json!({"w": 5}));
}

#[test]
fn the_reverse_mirror_carries_the_same_id() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut f = fixture(&path, 2);
    let key = tuple(&f, 0, 1);
    let a =
        f.db.create_edge(
            key.context,
            key.source,
            key.edge_type,
            key.destination,
            &json!({}),
        )
        .unwrap();
    let b =
        f.db.create_edge(
            key.context,
            key.source,
            key.edge_type,
            key.destination,
            &json!({"x": 1}),
        )
        .unwrap();
    f.db.commit().unwrap();
    let primary: Vec<Vec<u8>> = keyspace(&path, PRIMARY).into_iter().map(|r| r.0).collect();
    let reverse: Vec<(Vec<u8>, Vec<u8>)> = keyspace(&path, REVERSE);
    let mut want_primary = vec![
        expected_key(PRIMARY, key, a.id),
        expected_key(PRIMARY, key, b.id),
    ];
    want_primary.sort();
    assert_eq!(primary, want_primary);
    let mut want_reverse = vec![
        (expected_key(REVERSE, key, a.id), Vec::new()),
        (expected_key(REVERSE, key, b.id), Vec::new()),
    ];
    want_reverse.sort();
    assert_eq!(reverse, want_reverse);
}

#[test]
fn update_by_id_rewrites_that_edge_only() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut f = fixture(&path, 2);
    let key = tuple(&f, 0, 1);
    let a =
        f.db.create_edge(
            key.context,
            key.source,
            key.edge_type,
            key.destination,
            &json!({"w": 1}),
        )
        .unwrap();
    let b =
        f.db.create_edge(
            key.context,
            key.source,
            key.edge_type,
            key.destination,
            &json!({"w": 2}),
        )
        .unwrap();
    f.db.commit().unwrap();
    assert!(f.db.update_edge_properties(a, &json!({"w": 10})).unwrap());
    f.db.commit().unwrap();
    assert_eq!(edge(&f.db, a), Some(json!({"w": 10})));
    assert_eq!(edge(&f.db, b), Some(json!({"w": 2})));
    // An id that was never handed out is not there to update.
    let missing = EdgeId {
        key,
        id: a.id.max(b.id) + 100,
    };
    assert!(!f.db.update_edge_properties(missing, &json!({})).unwrap());
    assert!(edge(&f.db, missing).is_none());
    // Neither edge moved: still two, still their own ids.
    assert_eq!(f.db.neighbors(out_of(&f, 0)).unwrap().len(), 2);
    drop(f);
    clean(&path);
}

#[test]
fn deleting_one_parallel_edge_keeps_the_other_and_the_endpoint_sets() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut f = fixture(&path, 3);
    let key = tuple(&f, 0, 1);
    let a =
        f.db.create_edge(
            key.context,
            key.source,
            key.edge_type,
            key.destination,
            &json!({"w": 1}),
        )
        .unwrap();
    let b =
        f.db.create_edge(
            key.context,
            key.source,
            key.edge_type,
            key.destination,
            &json!({"w": 2}),
        )
        .unwrap();
    f.db.commit().unwrap();
    assert!(
        f.db.endpoint_sets_present(),
        "the first edge of an empty graph turns the endpoint sets on"
    );
    assert_eq!(endpoints(&f, Direction::Outgoing), vec![f.ids[0].sequence]);
    assert_eq!(endpoints(&f, Direction::Incoming), vec![f.ids[1].sequence]);

    assert!(f.db.delete_edge_by_id(a).unwrap());
    f.db.commit().unwrap();
    assert!(!f.db.delete_edge_by_id(a).unwrap(), "a deleted id is gone");
    let left = f.db.neighbors(out_of(&f, 0)).unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(
        (left[0].id, left[0].properties.clone()),
        (b.id, json!({"w": 2}))
    );
    // Both ends still have an edge of this (context, type, direction), so
    // both stay in their sets.
    assert_eq!(endpoints(&f, Direction::Outgoing), vec![f.ids[0].sequence]);
    assert_eq!(endpoints(&f, Direction::Incoming), vec![f.ids[1].sequence]);
    assert_eq!(keyspace(&path, PRIMARY).len(), 1);
    assert_eq!(keyspace(&path, REVERSE).len(), 1);
    f = verified(f, &path);

    // The LAST edge takes the ends with it.
    assert!(f.db.delete_edge_by_id(b).unwrap());
    f.db.commit().unwrap();
    assert!(endpoints(&f, Direction::Outgoing).is_empty());
    assert!(endpoints(&f, Direction::Incoming).is_empty());
    assert!(keyspace(&path, PRIMARY).is_empty());
    assert!(keyspace(&path, REVERSE).is_empty());
    drop(f);
    clean(&path);
}

#[test]
fn ids_survive_a_reopen_and_are_never_handed_out_twice() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut f = fixture(&path, 2);
    let key = tuple(&f, 0, 1);
    let mut made = Vec::new();
    for w in 0..3 {
        made.push(
            f.db.create_edge(
                key.context,
                key.source,
                key.edge_type,
                key.destination,
                &json!({"w": w}),
            )
            .unwrap(),
        );
    }
    f.db.commit().unwrap();
    let highest = made.iter().map(|e| e.id).max().unwrap();
    // Delete the newest, so a "max id + 1" allocator would reuse it.
    let newest = *made.iter().find(|e| e.id == highest).unwrap();
    assert!(f.db.delete_edge_by_id(newest).unwrap());
    f.db.commit().unwrap();
    drop(f.db);

    let mut db = Database::open(&path, cfg()).unwrap();
    for e in &made {
        let got = edge(&db, *e);
        if e.id == highest {
            assert!(got.is_none());
        } else {
            assert!(got.is_some(), "id {} is stable across a reopen", e.id);
        }
    }
    let next = db
        .create_edge(
            key.context,
            key.source,
            key.edge_type,
            key.destination,
            &json!({}),
        )
        .unwrap();
    db.commit().unwrap();
    assert!(next.id > highest, "id {} was handed out again", next.id);
}

#[test]
fn a_rolled_back_create_leaves_no_edge_and_no_bit() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut f = fixture(&path, 2);
    let key = tuple(&f, 0, 1);
    f.db.create_edge(
        key.context,
        key.source,
        key.edge_type,
        key.destination,
        &json!({}),
    )
    .unwrap();
    f.db.rollback().unwrap();
    assert_eq!(logical_features(&f.db) & EDGE_ID_FEATURE, 0);
    assert!(f.db.neighbors(out_of(&f, 0)).unwrap().is_empty());
    let e =
        f.db.create_edge(
            key.context,
            key.source,
            key.edge_type,
            key.destination,
            &json!({}),
        )
        .unwrap();
    f.db.commit().unwrap();
    assert!(edge(&f.db, e).is_some());
    drop(f);
    clean(&path);
}

// ── the old tuple-keyed API keeps its meaning ────────────────────────────

#[test]
fn put_edge_still_upserts_the_tuple_beside_parallel_edges() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut f = fixture(&path, 2);
    let key = tuple(&f, 0, 1);
    f.db.put_edge(
        key.context,
        key.source,
        key.edge_type,
        key.destination,
        &json!({"v": 1}),
    )
    .unwrap();
    f.db.put_edge(
        key.context,
        key.source,
        key.edge_type,
        key.destination,
        &json!({"v": 2}),
    )
    .unwrap();
    f.db.commit().unwrap();
    let out = f.db.neighbors(out_of(&f, 0)).unwrap();
    assert_eq!(out.len(), 1, "the tuple is the identity of a put_edge edge");
    assert_eq!((out[0].id, out[0].properties.clone()), (0, json!({"v": 2})));
    assert_eq!(logical_features(&f.db) & EDGE_ID_FEATURE, 0);

    // A created edge sits beside it; a further put_edge upserts only the
    // tuple's own edge.
    let created =
        f.db.create_edge(
            key.context,
            key.source,
            key.edge_type,
            key.destination,
            &json!({"c": 1}),
        )
        .unwrap();
    f.db.put_edge(
        key.context,
        key.source,
        key.edge_type,
        key.destination,
        &json!({"v": 3}),
    )
    .unwrap();
    f.db.commit().unwrap();
    let mut out: Vec<(u64, serde_json::Value)> =
        f.db.neighbors(out_of(&f, 0))
            .unwrap()
            .into_iter()
            .map(|e| (e.id, e.properties))
            .collect();
    out.sort_by_key(|e| e.0);
    assert_eq!(
        out,
        vec![(0, json!({"v": 3})), (created.id, json!({"c": 1}))]
    );
    // The implicit edge is addressable by id 0 too.
    let implicit = EdgeId { key, id: 0 };
    assert_eq!(edge(&f.db, implicit), Some(json!({"v": 3})));
    f = verified(f, &path);

    // Deleting by TUPLE removes every edge of the tuple.
    assert!(f.db.delete_edge(key).unwrap());
    f.db.commit().unwrap();
    assert!(f.db.neighbors(out_of(&f, 0)).unwrap().is_empty());
    assert!(edge(&f.db, created).is_none());
    assert!(keyspace(&path, PRIMARY).is_empty());
    assert!(keyspace(&path, REVERSE).is_empty());
    assert!(endpoints(&f, Direction::Outgoing).is_empty());
    drop(f);
    clean(&path);
}

#[test]
fn a_file_without_parallel_edges_reads_and_stores_exactly_as_before() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut f = fixture(&path, 4);
    let chain = [(0, 1), (1, 2), (2, 3), (0, 2)];
    for (s, d) in chain {
        let k = tuple(&f, s, d);
        f.db.put_edge(
            k.context,
            k.source,
            k.edge_type,
            k.destination,
            &json!({"s": s}),
        )
        .unwrap();
    }
    f.db.commit().unwrap();
    assert_eq!(
        logical_features(&f.db) & EDGE_ID_FEATURE,
        0,
        "no id-bearing key, no bit"
    );
    // The keys are the frozen tuple encoding, with no id segment.
    let mut want: Vec<Vec<u8>> = chain
        .iter()
        .map(|(s, d)| expected_key(PRIMARY, tuple(&f, *s, *d), 0))
        .collect();
    want.sort();
    let got: Vec<Vec<u8>> = keyspace(&path, PRIMARY).into_iter().map(|r| r.0).collect();
    assert_eq!(got, want);
    // Every read reports the implicit id 0.
    let out = f.db.neighbors(out_of(&f, 0)).unwrap();
    assert_eq!(out.len(), 2);
    assert!(out.iter().all(|e| e.id == 0));
    let walked = f.db.traverse_bfs_binding_edges(bfs(&f, 0, &[])).unwrap();
    let reached: Vec<(u64, usize)> = walked
        .nodes
        .iter()
        .map(|n| (n.entity.sequence, n.depth))
        .collect();
    assert_eq!(
        reached,
        vec![
            (f.ids[1].sequence, 1),
            (f.ids[2].sequence, 1),
            (f.ids[3].sequence, 2)
        ]
    );
    assert!(walked.nodes.iter().all(|n| n.via.as_ref().unwrap().id == 0));
    assert_eq!(walked.scanned_edges, 4);
    drop(f);
    clean(&path);
}

#[test]
fn deleting_an_entity_cascades_every_parallel_edge() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut f = fixture(&path, 3);
    let key = tuple(&f, 0, 1);
    for w in 0..3 {
        f.db.create_edge(
            key.context,
            key.source,
            key.edge_type,
            key.destination,
            &json!({"w": w}),
        )
        .unwrap();
    }
    let other = tuple(&f, 2, 1);
    let kept =
        f.db.create_edge(
            other.context,
            other.source,
            other.edge_type,
            other.destination,
            &json!({}),
        )
        .unwrap();
    f.db.commit().unwrap();
    assert!(f.db.delete(f.place, "p0").unwrap());
    f.db.commit().unwrap();
    let primary = keyspace(&path, PRIMARY);
    assert_eq!(primary.len(), 1);
    assert_eq!(primary[0].0, expected_key(PRIMARY, other, kept.id));
    assert_eq!(endpoints(&f, Direction::Incoming), vec![f.ids[1].sequence]);
    assert_eq!(endpoints(&f, Direction::Outgoing), vec![f.ids[2].sequence]);
    drop(f);
    clean(&path);
}

// ── Law 8 ────────────────────────────────────────────────────────────────

#[test]
fn an_edge_identity_file_is_unsupported_to_a_binary_that_predates_the_bit() {
    assert_eq!(EDGE_ID_FEATURE, 0x40000);
    assert_eq!(
        SUPPORTED_LOGICAL_FEATURES & EDGE_ID_FEATURE,
        EDGE_ID_FEATURE
    );
    let older = SUPPORTED_LOGICAL_FEATURES & !EDGE_ID_FEATURE;
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut f = fixture(&path, 2);
    let key = tuple(&f, 0, 1);
    f.db.create_edge(
        key.context,
        key.source,
        key.edge_type,
        key.destination,
        &json!({}),
    )
    .unwrap();
    f.db.commit().unwrap();
    let written = logical_features(&f.db);
    drop(f);
    assert_eq!(
        written & EDGE_ID_FEATURE,
        EDGE_ID_FEATURE,
        "the bit rides the first id-bearing key"
    );
    assert_eq!(written & ENDPOINT_FEATURE, ENDPOINT_FEATURE);
    admit_logical_features(written, SUPPORTED_LOGICAL_FEATURES).unwrap();
    Database::open(&path, cfg()).unwrap();
    let refused = admit_logical_features(written, older).unwrap_err();
    assert!(
        matches!(refused, Error::Unsupported(ref m) if m.contains(&format!("{written:#x}"))),
        "{refused:?}"
    );
}

// ── the verifier and the rebuild ─────────────────────────────────────────

#[test]
fn the_verifier_is_clean_and_the_rebuild_keeps_parallel_edges_and_their_ids() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("db");
    let destination = t.path().join("rebuilt");
    let key;
    let made: Vec<EdgeId>;
    {
        let mut f = fixture(&source, 3);
        key = tuple(&f, 0, 1);
        let mut m = Vec::new();
        for w in 0..3 {
            m.push(
                f.db.create_edge(
                    key.context,
                    key.source,
                    key.edge_type,
                    key.destination,
                    &json!({"w": w}),
                )
                .unwrap(),
            );
        }
        // An implicit tuple edge beside them, and an ordinary one elsewhere.
        f.db.put_edge(
            key.context,
            key.source,
            key.edge_type,
            key.destination,
            &json!({"t": 1}),
        )
        .unwrap();
        let other = tuple(&f, 1, 2);
        f.db.put_edge(
            other.context,
            other.source,
            other.edge_type,
            other.destination,
            &json!({}),
        )
        .unwrap();
        f.db.commit().unwrap();
        made = m;
    }
    clean(&source);

    rebuild_derived_indexes(&source, &destination, RebuildLimits::default()).unwrap();
    clean(&destination);
    assert_eq!(keyspace(&source, PRIMARY), keyspace(&destination, PRIMARY));
    assert_eq!(keyspace(&source, REVERSE), keyspace(&destination, REVERSE));

    let mut db = Database::open(&destination, cfg()).unwrap();
    assert_eq!(logical_features(&db) & EDGE_ID_FEATURE, EDGE_ID_FEATURE);
    for (w, e) in made.iter().enumerate() {
        assert_eq!(edge(&db, *e), Some(json!({"w": w})));
    }
    // The allocator came across too: a new edge in the rebuilt file does not
    // collide with any id the source handed out.
    let next = db
        .create_edge(
            key.context,
            key.source,
            key.edge_type,
            key.destination,
            &json!({}),
        )
        .unwrap();
    db.commit().unwrap();
    assert!(made.iter().all(|e| e.id < next.id));
    drop(db);
    clean(&destination);
}

/// An id segment in a file that does NOT declare the bit is damage, not a
/// parallel edge: the verifier names it and the rebuild refuses to carry it
/// into a new file rather than guessing what it meant.
#[test]
fn an_id_segment_without_the_bit_is_reported_not_believed() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let f = fixture(&path, 2);
    let key = tuple(&f, 0, 1);
    drop(f);
    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    raw.put(&expected_key(PRIMARY, key, 7), &[1, 8, 0]).unwrap();
    raw.put(&expected_key(REVERSE, key, 7), &[]).unwrap();
    raw.commit().unwrap();
    drop(raw);

    let mut issues = Vec::new();
    let report = verify_indexed_source(&path, VerificationLimits::default(), |issue| {
        issues.push(format!("{issue:?}"))
    })
    .unwrap();
    assert!(!report.clean);
    assert!(
        issues
            .iter()
            .any(|i| i.contains("id segment without the edge identity feature")),
        "{issues:?}"
    );
    let refused =
        rebuild_derived_indexes(&path, &t.path().join("rebuilt"), RebuildLimits::default())
            .unwrap_err();
    assert!(format!("{refused:?}").contains("id segment"), "{refused:?}");
}
