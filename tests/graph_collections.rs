use e4_prototype::{
    collections::{
        BfsRequest, CollectionOptions, Database, Direction, Error, GraphContextId, NeighborRequest,
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
};

// The counting allocator of tests/codec_allocations.rs and tests/index_vector.rs:
// a per-row `Vec` shows up in the count even when it is small, which is the
// shape a traversal must not have.
thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<usize> = const { Cell::new(0) };
    static BYTES: Cell<usize> = const { Cell::new(0) };
}
struct Alloc;
unsafe impl GlobalAlloc for Alloc {
    unsafe fn alloc(&self, l: AllocationLayout) -> *mut u8 {
        TRACK
            .try_with(|t| {
                if t.get() {
                    COUNT.with(|n| n.set(n.get() + 1));
                    BYTES.with(|n| n.set(n.get() + l.size()));
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
                    BYTES.with(|b| b.set(b.get() + n));
                }
            })
            .ok();
        System.realloc(p, l, n)
    }
}
#[global_allocator]
static ALLOC: Alloc = Alloc;
fn measured<T>(f: impl FnOnce() -> T) -> (T, usize, usize) {
    COUNT.with(|c| c.set(0));
    BYTES.with(|b| b.set(0));
    TRACK.with(|t| t.set(true));
    let value = f();
    TRACK.with(|t| t.set(false));
    (value, COUNT.with(Cell::get), BYTES.with(Cell::get))
}

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn graph_fixture() -> (
    tempfile::TempDir,
    Database,
    [e4_prototype::collections::EntityId; 5],
) {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("db"), cfg()).unwrap();
    let people = db
        .create_collection(
            "people",
            vec![("name".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    let orgs = db
        .create_collection(
            "orgs",
            vec![("name".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    let a = db.put(people, "same", &json!({"name":"a"})).unwrap();
    let b = db.put(people, "b", &json!({"name":"b"})).unwrap();
    let c = db.put(people, "c", &json!({"name":"c"})).unwrap();
    let d = db.put(people, "d", &json!({"name":"d"})).unwrap();
    let org = db.put(orgs, "same", &json!({"name":"org"})).unwrap();
    db.enable_graph().unwrap();
    (dir, db, [a, b, c, d, org])
}

#[test]
fn directed_typed_context_edges_replace_properties_and_survive_reopen() {
    let (dir, mut db, [a, b, _, _, org]) = graph_fixture();
    let base = db
        .link(a, "knows\0人", b, "", &json!({"n":u64::MAX}))
        .unwrap();
    let named = db
        .link(a, "knows\0人", b, "private\0東京", &json!({"v":1}))
        .unwrap();
    let cross = db.link(a, "member_of", org, "", &json!({})).unwrap();
    assert_ne!(base.context, named.context);
    assert_ne!(a, org);
    assert_eq!(
        db.neighbors(NeighborRequest {
            entity: a,
            direction: Direction::Outgoing,
            context: GraphContextId::BASE,
            edge_type: Some(base.edge_type),
            limit: 8,
        })
        .unwrap()[0]
            .properties,
        json!({"n":u64::MAX})
    );
    assert_eq!(
        db.link(a, "knows\0人", b, "", &json!({"n":7})).unwrap(),
        base
    );
    let outgoing = db
        .neighbors(NeighborRequest {
            entity: a,
            direction: Direction::Outgoing,
            context: GraphContextId::BASE,
            edge_type: Some(base.edge_type),
            limit: 8,
        })
        .unwrap();
    assert_eq!(outgoing.len(), 1);
    assert_eq!(outgoing[0].properties, json!({"n":7}));
    let incoming = db
        .neighbors(NeighborRequest {
            entity: org,
            direction: Direction::Incoming,
            context: GraphContextId::BASE,
            edge_type: Some(cross.edge_type),
            limit: 8,
        })
        .unwrap();
    assert_eq!(incoming[0].key, cross);
    db.commit().unwrap();
    drop(db);
    let mut db = Database::open(dir.path().join("db"), cfg()).unwrap();
    assert!(db.unlink(a, "knows\0人", b, "private\0東京").unwrap());
    db.rollback().unwrap();
    assert_eq!(
        db.neighbors(NeighborRequest {
            entity: b,
            direction: Direction::Incoming,
            context: named.context,
            edge_type: Some(named.edge_type),
            limit: 2,
        })
        .unwrap()
        .len(),
        1
    );
}

#[test]
fn cyclic_bfs_is_shortest_hop_deterministic_and_bounded() {
    let (_dir, mut db, [a, b, c, d, _]) = graph_fixture();
    let edge_type = db.create_edge_type("knows").unwrap();
    for (from, to) in [(a, b), (a, c), (b, d), (c, d), (d, a)] {
        db.put_edge(GraphContextId::BASE, from, edge_type, to, &json!({}))
            .unwrap();
    }
    let traversal = db
        .traverse_bfs(BfsRequest {
            seed: a,
            direction: Direction::Outgoing,
            context: GraphContextId::BASE,
            edge_type: Some(edge_type),
            min_depth: 1,
            max_depth: 8,
            include_seed: false,
            max_visited: 16,
            max_edges: 32,
            result_limit: 16,
        })
        .unwrap();
    assert_eq!(
        traversal
            .nodes
            .iter()
            .map(|n| (n.entity, n.depth))
            .collect::<Vec<_>>(),
        vec![(b, 1), (c, 1), (d, 2)]
    );
    assert!(db
        .traverse_bfs(BfsRequest {
            seed: a,
            direction: Direction::Outgoing,
            context: GraphContextId::BASE,
            edge_type: Some(edge_type),
            min_depth: 1,
            max_depth: 8,
            include_seed: false,
            max_visited: 2,
            max_edges: 32,
            result_limit: 16,
        })
        .is_err());
    // The visited bound is enforced while each distinct next-wave entity is
    // discovered, rather than after a potentially much larger wave is held.
    assert!(db
        .traverse_bfs(BfsRequest {
            seed: a,
            direction: Direction::Outgoing,
            context: GraphContextId::BASE,
            edge_type: Some(edge_type),
            min_depth: 1,
            max_depth: 1,
            include_seed: false,
            max_visited: 1,
            max_edges: 1_000_000,
            result_limit: 16,
        })
        .is_err());
}

#[test]
fn neighbor_and_bfs_cancellation_never_return_partial_or_poison_reads() {
    let (_dir, mut db, [a, b, c, d, _]) = graph_fixture();
    let edge_type = db.create_edge_type("cancel-walk").unwrap();
    for (from, to) in [(a, b), (a, c), (b, d), (c, d)] {
        db.put_edge(GraphContextId::BASE, from, edge_type, to, &json!({}))
            .unwrap();
    }
    let neighbors = NeighborRequest {
        entity: a,
        direction: Direction::Outgoing,
        context: GraphContextId::BASE,
        edge_type: Some(edge_type),
        limit: 8,
    };
    let expected_neighbors = db.neighbors(neighbors).unwrap();
    let immediate = db.neighbors_with_cancel(neighbors, || true).unwrap_err();
    assert!(matches!(immediate, Error::InvalidInput(message) if message.contains("cancelled")));

    let mut neighbor_polls = 0;
    let mid = db
        .neighbors_with_cancel(neighbors, || {
            neighbor_polls += 1;
            neighbor_polls == 3
        })
        .unwrap_err();
    assert!(matches!(mid, Error::InvalidInput(message) if message.contains("cancelled")));
    assert_eq!(neighbor_polls, 3);
    assert_eq!(db.neighbors(neighbors).unwrap(), expected_neighbors);

    let bfs = BfsRequest {
        seed: a,
        direction: Direction::Outgoing,
        context: GraphContextId::BASE,
        edge_type: Some(edge_type),
        min_depth: 1,
        max_depth: 4,
        include_seed: false,
        max_visited: 16,
        max_edges: 32,
        result_limit: 16,
    };
    let expected_bfs = db.traverse_bfs(bfs).unwrap();
    let immediate = db.traverse_bfs_with_cancel(bfs, || true).unwrap_err();
    assert!(matches!(immediate, Error::InvalidInput(message) if message.contains("cancelled")));

    let mut bfs_polls = 0;
    let mid = db
        .traverse_bfs_with_cancel(bfs, || {
            bfs_polls += 1;
            bfs_polls == 5
        })
        .unwrap_err();
    assert!(matches!(mid, Error::InvalidInput(message) if message.contains("cancelled")));
    assert_eq!(bfs_polls, 5);
    assert_eq!(db.traverse_bfs(bfs).unwrap(), expected_bfs);
    assert!(db.get_by_id(a).unwrap().is_some());
}

#[test]
fn snapshots_unlink_missing_endpoints_and_delete_cascade_are_transactional() {
    let (dir, mut db, [a, b, c, _, _]) = graph_fixture();
    let edge = db.link(a, "knows", b, "", &json!({"old":true})).unwrap();
    db.commit().unwrap();
    let snapshot = Database::open_snapshot(dir.path().join("db"), cfg()).unwrap();
    assert!(db.unlink(a, "knows", b, "").unwrap());
    db.link(a, "knows", c, "", &json!({"new":true})).unwrap();
    db.rollback().unwrap();
    assert_eq!(
        db.neighbors(NeighborRequest {
            entity: a,
            direction: Direction::Outgoing,
            context: GraphContextId::BASE,
            edge_type: Some(edge.edge_type),
            limit: 4,
        })
        .unwrap()[0]
            .key
            .destination,
        b
    );
    assert_eq!(
        snapshot
            .neighbors(NeighborRequest {
                entity: a,
                direction: Direction::Outgoing,
                context: GraphContextId::BASE,
                edge_type: Some(edge.edge_type),
                limit: 4,
            })
            .unwrap()[0]
            .key
            .destination,
        b
    );
    assert!(db
        .put_edge(
            GraphContextId::BASE,
            a,
            edge.edge_type,
            e4_prototype::collections::EntityId {
                collection: b.collection,
                sequence: u64::MAX
            },
            &json!({})
        )
        .is_err());
}

#[test]
fn entity_delete_cascades_both_directions_all_contexts_and_self_edges() {
    let (_dir, mut db, [a, b, c, _, _]) = graph_fixture();
    let base = db
        .link(a, "related", b, "", &json!({"which":"base"}))
        .unwrap();
    let named = db
        .link(b, "related", a, "history", &json!({"which":"named"}))
        .unwrap();
    db.link(b, "related", b, "history", &json!({"self":true}))
        .unwrap();
    db.link(a, "related", c, "", &json!({"keep":true})).unwrap();
    db.commit().unwrap();
    assert!(db.delete(b.collection, "b").unwrap());
    db.commit().unwrap();
    assert!(db
        .neighbors(NeighborRequest {
            entity: a,
            direction: Direction::Outgoing,
            context: GraphContextId::BASE,
            edge_type: Some(base.edge_type),
            limit: 4,
        })
        .unwrap()
        .iter()
        .all(|edge| edge.key.destination == c));
    assert!(db
        .neighbors(NeighborRequest {
            entity: a,
            direction: Direction::Incoming,
            context: named.context,
            edge_type: Some(named.edge_type),
            limit: 4,
        })
        .unwrap()
        .is_empty());
    assert!(db.get_by_id(b).unwrap().is_none());
}

#[test]
fn entity_delete_refuses_degree_257_before_any_published_change() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let c = db
        .create_collection("nodes", vec![], CollectionOptions::default())
        .unwrap();
    let hub = db.put(c, "hub", &json!({})).unwrap();
    db.enable_graph().unwrap();
    let edge_type = db.create_edge_type("fanout").unwrap();
    let mut leaves = Vec::new();
    for i in 0..257 {
        let leaf = db.put(c, &format!("leaf/{i}"), &json!({})).unwrap();
        db.put_edge(GraphContextId::BASE, hub, edge_type, leaf, &json!({}))
            .unwrap();
        leaves.push(leaf);
    }
    db.commit().unwrap();
    assert!(db.delete(c, "hub").is_err());
    db.rollback().unwrap();
    assert!(db.get_by_id(hub).unwrap().is_some());
    assert_eq!(
        db.neighbors(NeighborRequest {
            entity: leaves[0],
            direction: Direction::Incoming,
            context: GraphContextId::BASE,
            edge_type: Some(edge_type),
            limit: 2,
        })
        .unwrap()[0]
            .key
            .source,
        hub
    );
    drop(db);
    let db = Database::open(path, cfg()).unwrap();
    assert!(db.get_by_id(hub).unwrap().is_some());
}

/// The bench shape: delete a person, reinsert it under a fresh identity, then
/// restore its three relationships. Every relationship the engine writes costs
/// two tree writes; everything else it charges is a read it took before
/// writing. This pins the total so a read-before-write creeping back in is a
/// test failure, not a benchmark regression noticed weeks later.
#[test]
fn reinsert_with_three_relationships_pays_for_its_writes_not_its_probes() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("db"), cfg()).unwrap();
    let people = db
        .create_collection(
            "people",
            vec![("name".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    let orgs = db
        .create_collection(
            "orgs",
            vec![("name".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.enable_graph().unwrap();
    let knows = db.create_edge_type("knows").unwrap();
    let member = db.create_edge_type("member_of").unwrap();
    let mut ids = Vec::new();
    for i in 0..600 {
        ids.push(
            db.put(people, &format!("p{i:05}"), &json!({"name":"x"}))
                .unwrap(),
        );
    }
    let org = db.put(orgs, "org", &json!({"name":"o"})).unwrap();
    db.commit().unwrap();
    for i in 0..600usize {
        db.put_edge(
            GraphContextId::BASE,
            ids[i],
            knows,
            ids[(i + 1) % 600],
            &json!({"slot":1}),
        )
        .unwrap();
        db.put_edge(
            GraphContextId::BASE,
            ids[i],
            knows,
            ids[(i + 7) % 600],
            &json!({"slot":7}),
        )
        .unwrap();
        db.put_edge(GraphContextId::BASE, ids[i], member, org, &json!({}))
            .unwrap();
    }
    db.commit().unwrap();

    // Round two: the measured shape. Fresh identities, three edges each.
    const ROUND: usize = 300;
    for i in 0..ROUND {
        assert!(db.delete(people, &format!("p{i:05}")).unwrap());
    }
    db.commit().unwrap();
    let before = db.pool_accesses().unwrap();
    for i in 0..ROUND {
        ids[i] = db
            .put(people, &format!("p{i:05}"), &json!({"name":"y"}))
            .unwrap();
    }
    db.commit().unwrap();
    let rows_spent = db.pool_accesses().unwrap() - before;
    for i in 0..ROUND {
        db.put_edge(
            GraphContextId::BASE,
            ids[i],
            knows,
            ids[(i + 1) % 600],
            &json!({"slot":1}),
        )
        .unwrap();
        db.put_edge(
            GraphContextId::BASE,
            ids[i],
            knows,
            ids[(i + 7) % 600],
            &json!({"slot":7}),
        )
        .unwrap();
        db.put_edge(GraphContextId::BASE, ids[i], member, org, &json!({}))
            .unwrap();
    }
    db.commit().unwrap();
    let spent = db.pool_accesses().unwrap() - before;
    let per_person = spent as f64 / ROUND as f64;
    println!(
        "reinsert+3 relationships: {spent} pool accesses over {ROUND} people ({per_person:.1}/person); rows {rows_spent} ({:.1}/person), edges {:.1}/edge",
        rows_spent as f64 / ROUND as f64,
        (spent - rows_spent) as f64 / (3 * ROUND) as f64
    );

    // The edges themselves must still be exactly what was asked for.
    let out = db
        .neighbors(NeighborRequest {
            entity: ids[5],
            direction: Direction::Outgoing,
            edge_type: Some(knows),
            context: GraphContextId::BASE,
            limit: 16,
        })
        .unwrap();
    assert_eq!(out.len(), 2);
    assert!(
        per_person <= 60.0,
        "reinsert of one person with three relationships charged {per_person:.1} pool accesses"
    );
}

/// The skipped reads are skipped because their answer is already known, not
/// because the guarantee was dropped. A freshly allocated identity that is
/// then deleted must still be refused as an endpoint, and a rolled-back
/// transaction must leave nothing "known" behind.
#[test]
fn fresh_endpoint_fast_path_keeps_every_guarantee_it_skips_a_read_for() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("db"), cfg()).unwrap();
    let people = db
        .create_collection(
            "people",
            vec![("name".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.enable_graph().unwrap();
    let knows = db.create_edge_type("knows").unwrap();

    let a = db.put(people, "a", &json!({"name":"a"})).unwrap();
    let b = db.put(people, "b", &json!({"name":"b"})).unwrap();
    let doomed = db.put(people, "doomed", &json!({"name":"d"})).unwrap();
    db.commit().unwrap();
    assert!(db.delete(people, "doomed").unwrap());
    db.commit().unwrap();
    // Allocated by this handle, and gone. The fast path must not vouch for it.
    assert!(matches!(
        db.put_edge(GraphContextId::BASE, a, knows, doomed, &json!({})),
        Err(Error::NotFound("graph endpoint"))
    ));
    assert!(matches!(
        db.put_edge(GraphContextId::BASE, doomed, knows, a, &json!({})),
        Err(Error::NotFound("graph endpoint"))
    ));
    // An identity the allocator has never issued is refused the same way.
    let never = e4_prototype::collections::EntityId {
        collection: a.collection,
        sequence: 9_999_999,
    };
    assert!(matches!(
        db.put_edge(GraphContextId::BASE, a, knows, never, &json!({})),
        Err(Error::NotFound("graph endpoint"))
    ));

    // Rewriting the same edge on a fresh endpoint replaces its properties
    // rather than duplicating or losing the reverse marker.
    let key = db
        .put_edge(GraphContextId::BASE, a, knows, b, &json!({"v":1}))
        .unwrap();
    db.put_edge(GraphContextId::BASE, a, knows, b, &json!({"v":2}))
        .unwrap();
    db.commit().unwrap();
    let out = db
        .neighbors(NeighborRequest {
            entity: a,
            direction: Direction::Outgoing,
            edge_type: Some(knows),
            context: GraphContextId::BASE,
            limit: 16,
        })
        .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].key, key);
    assert_eq!(out[0].properties, json!({"v":2}));
    let incoming = db
        .neighbors(NeighborRequest {
            entity: b,
            direction: Direction::Incoming,
            edge_type: Some(knows),
            context: GraphContextId::BASE,
            limit: 16,
        })
        .unwrap();
    assert_eq!(incoming.len(), 1);

    // A rolled-back transaction rewinds the allocator, so nothing it taught
    // the fast path may survive it.
    let ghost = db.put(people, "ghost", &json!({"name":"g"})).unwrap();
    db.rollback().unwrap();
    assert!(matches!(
        db.put_edge(GraphContextId::BASE, a, knows, ghost, &json!({})),
        Err(Error::NotFound("graph endpoint"))
    ));

    drop(db);
    let db = Database::open(dir.path().join("db"), cfg()).unwrap();
    let out = db
        .neighbors(NeighborRequest {
            entity: a,
            direction: Direction::Outgoing,
            edge_type: Some(knows),
            context: GraphContextId::BASE,
            limit: 16,
        })
        .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].properties, json!({"v":2}));
}

/// A traversal read is a scan, not a scan plus a lookup per edge. Walking the
/// 300 reverse keys of a fan-in, or the 200 primary keys of a fan-out, must
/// cost the leaf pages those keys live on and nothing per edge: the two
/// directions of an edge are written in one transaction, so a committed
/// snapshot cannot hold half a pair, and `verify_indexed_source` is the tool
/// that checks pair consistency.
#[test]
fn traversal_reads_pay_for_leaf_pages_not_for_edges() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("db"), cfg()).unwrap();
    let people = db
        .create_collection(
            "people",
            vec![("name".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    let orgs = db
        .create_collection(
            "orgs",
            vec![("name".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.enable_graph().unwrap();
    let member = db.create_edge_type("member_of").unwrap();
    let knows = db.create_edge_type("knows").unwrap();

    const FAN_IN: usize = 300;
    const FAN_OUT: usize = 200;
    let mut ids = Vec::new();
    for i in 0..FAN_IN {
        ids.push(
            db.put(people, &format!("p{i:05}"), &json!({"name": "x"}))
                .unwrap(),
        );
    }
    let org = db.put(orgs, "org", &json!({"name": "o"})).unwrap();
    let hub = db.put(people, "hub", &json!({"name": "hub"})).unwrap();
    db.commit().unwrap();
    for i in 0..FAN_IN {
        db.put_edge(GraphContextId::BASE, ids[i], member, org, &json!({"r": i}))
            .unwrap();
    }
    for i in 0..FAN_OUT {
        db.put_edge(GraphContextId::BASE, hub, knows, ids[i], &json!({"r": i}))
            .unwrap();
    }
    db.commit().unwrap();
    drop(db);
    let db = Database::open(dir.path().join("db"), cfg()).unwrap();

    // Warm the pages once, then measure the steady-state repeat.
    for _ in 0..2 {
        db.traverse_bfs(BfsRequest {
            seed: org,
            direction: Direction::Incoming,
            edge_type: Some(member),
            context: GraphContextId::BASE,
            min_depth: 1,
            max_depth: 1,
            include_seed: false,
            max_visited: 4096,
            max_edges: 4096,
            result_limit: 4096,
        })
        .unwrap();
    }
    let before = db.pool_accesses().unwrap();
    let hop = db
        .traverse_bfs(BfsRequest {
            seed: org,
            direction: Direction::Incoming,
            edge_type: Some(member),
            context: GraphContextId::BASE,
            min_depth: 1,
            max_depth: 1,
            include_seed: false,
            max_visited: 4096,
            max_edges: 4096,
            result_limit: 4096,
        })
        .unwrap();
    let bfs_cost = db.pool_accesses().unwrap() - before;
    assert_eq!(hop.nodes.len(), FAN_IN);

    // 300 reverse keys of ~40 bytes each occupy a handful of 4 KiB leaves;
    // 32 leaves plus 8 accesses of descent and header is generous for that and
    // still an order of magnitude below one lookup per edge.
    const BOUND: u64 = 40;
    assert!(
        bfs_cost <= BOUND,
        "BFS over {FAN_IN} incoming edges charged {bfs_cost} pool accesses (bound {BOUND}); a per-edge lookup would charge at least {FAN_IN}"
    );

    for _ in 0..2 {
        db.neighbors(NeighborRequest {
            entity: hub,
            direction: Direction::Outgoing,
            edge_type: Some(knows),
            context: GraphContextId::BASE,
            limit: 256,
        })
        .unwrap();
    }
    let before = db.pool_accesses().unwrap();
    let out = db
        .neighbors(NeighborRequest {
            entity: hub,
            direction: Direction::Outgoing,
            edge_type: Some(knows),
            context: GraphContextId::BASE,
            limit: 256,
        })
        .unwrap();
    let neighbor_cost = db.pool_accesses().unwrap() - before;
    assert_eq!(out.len(), FAN_OUT);
    assert!(out.iter().all(|e| e.properties.get("r").is_some()));
    println!("BFS fan-in {FAN_IN}: {bfs_cost} pool accesses; neighbors fan-out {FAN_OUT}: {neighbor_cost} pool accesses");
    assert!(
        neighbor_cost <= BOUND,
        "neighbors over {FAN_OUT} outgoing edges charged {neighbor_cost} pool accesses (bound {BOUND}); a per-edge lookup would charge at least {FAN_OUT}"
    );
}

/// Walking an edge must not allocate. The scan hands the key and the value as
/// borrows into the pinned leaf, the parse reads integers out of the key in
/// place, and the frontier is a vector sorted once per level -- so a traversal
/// allocates a constant plus its own result, not a heap cell per edge.
#[test]
fn bfs_allocates_a_constant_plus_its_result_not_per_edge() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("db"), cfg()).unwrap();
    let people = db
        .create_collection(
            "people",
            vec![("name".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    let orgs = db
        .create_collection(
            "orgs",
            vec![("name".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.enable_graph().unwrap();
    let member = db.create_edge_type("member_of").unwrap();

    const FAN_IN: usize = 300;
    let mut ids = Vec::new();
    for i in 0..FAN_IN {
        ids.push(
            db.put(people, &format!("p{i:05}"), &json!({"name": "x"}))
                .unwrap(),
        );
    }
    let org = db.put(orgs, "org", &json!({"name": "o"})).unwrap();
    db.commit().unwrap();
    for id in &ids {
        db.put_edge(GraphContextId::BASE, *id, member, org, &json!({"r": 1}))
            .unwrap();
    }
    db.commit().unwrap();
    drop(db);
    let db = Database::open(dir.path().join("db"), cfg()).unwrap();

    let request = BfsRequest {
        seed: org,
        direction: Direction::Incoming,
        edge_type: Some(member),
        context: GraphContextId::BASE,
        min_depth: 1,
        max_depth: 1,
        include_seed: false,
        max_visited: 4096,
        max_edges: 4096,
        result_limit: 4096,
    };
    // Warm the pages and any one-off lazy state, then measure the repeat.
    for _ in 0..2 {
        db.traverse_bfs(request).unwrap();
    }
    let (hop, allocations, bytes) = measured(|| db.traverse_bfs(request).unwrap());
    assert_eq!(hop.nodes.len(), FAN_IN);
    println!("BFS over {FAN_IN} incoming edges: {allocations} allocations, {bytes} bytes");
    // Counted here: 423 allocations before the borrowing walk, 74 after it,
    // 20 once the visited set stopped being a `BTreeSet` -- a tree that
    // allocated a node every few entities the traversal discovered. What is
    // left grows with the LOGARITHM of the result (vector doubling), so the
    // bound is flat and does not follow the fan-in any more.
    let bound = 32;
    assert!(
        allocations <= bound,
        "BFS over {FAN_IN} incoming edges allocated {allocations} times (bound {bound}); \
         a per-edge key and value Vec would allocate at least {}",
        2 * FAN_IN
    );
}

/// The adjacency itself, without the properties nobody asked for.
///
/// `neighbors` answers with whole `Edge`s: for an incoming read that costs one
/// point lookup of the authoritative row per edge, plus a decode of each
/// property object, to build values a caller that only wants "who is next to
/// me" throws away. `neighbor_ids` is the same walk with the same complete-or-
/// error bound, answering distinct adjacent identities. It must agree with
/// `neighbors` on every direction, including a self-loop, which is the one
/// shape where the same identity is reachable both ways.
#[test]
fn neighbor_ids_answer_the_same_adjacency_as_neighbors_without_properties() {
    let (_dir, mut db, [a, b, c, d, _]) = graph_fixture();
    let knows = db.create_edge_type("knows").unwrap();
    let other = db.create_edge_type("works_with").unwrap();
    for (from, to) in [(a, b), (a, c), (a, a), (b, a), (d, a)] {
        db.put_edge(GraphContextId::BASE, from, knows, to, &json!({"weight": 3}))
            .unwrap();
    }
    // A second type from the same entity: the type filter must still bind.
    db.put_edge(GraphContextId::BASE, a, other, d, &json!({}))
        .unwrap();
    db.commit().unwrap();

    for direction in [Direction::Outgoing, Direction::Incoming, Direction::Both] {
        let request = NeighborRequest {
            entity: a,
            direction,
            context: GraphContextId::BASE,
            edge_type: Some(knows),
            limit: 8,
        };
        let mut expected = db
            .neighbors(request)
            .unwrap()
            .into_iter()
            .map(|edge| {
                if edge.key.source == a && direction != Direction::Incoming {
                    edge.key.destination
                } else {
                    edge.key.source
                }
            })
            .collect::<Vec<_>>();
        expected.sort_unstable();
        expected.dedup();
        assert_eq!(db.neighbor_ids(request).unwrap(), expected, "{direction:?}");
    }
    // Outgoing from `a` under `knows`: b, c and the self-loop a.
    let mut outgoing = vec![a, b, c];
    outgoing.sort_unstable();
    assert_eq!(
        db.neighbor_ids(NeighborRequest {
            entity: a,
            direction: Direction::Outgoing,
            context: GraphContextId::BASE,
            edge_type: Some(knows),
            limit: 8,
        })
        .unwrap(),
        outgoing
    );
    // Incoming under `knows`: b, d and the self-loop a.
    let mut incoming = vec![a, b, d];
    incoming.sort_unstable();
    assert_eq!(
        db.neighbor_ids(NeighborRequest {
            entity: a,
            direction: Direction::Incoming,
            context: GraphContextId::BASE,
            edge_type: Some(knows),
            limit: 8,
        })
        .unwrap(),
        incoming
    );
    // The other type is a different adjacency, not a wider one.
    assert_eq!(
        db.neighbor_ids(NeighborRequest {
            entity: a,
            direction: Direction::Outgoing,
            context: GraphContextId::BASE,
            edge_type: Some(other),
            limit: 8,
        })
        .unwrap(),
        vec![d]
    );
    // Complete-or-error: a bound below the distinct count is refused, never
    // silently cut, exactly as `neighbors` refuses it.
    assert!(matches!(
        db.neighbor_ids(NeighborRequest {
            entity: a,
            direction: Direction::Outgoing,
            context: GraphContextId::BASE,
            edge_type: Some(knows),
            limit: 2,
        }),
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        db.neighbor_ids(NeighborRequest {
            entity: a,
            direction: Direction::Outgoing,
            context: GraphContextId::BASE,
            edge_type: Some(knows),
            limit: 300,
        }),
        Err(Error::InvalidInput(_))
    ));
}

/// The id walk must not allocate per edge. `neighbors` pays a key `Vec`, a
/// value `Vec` and a tree node for every edge it touches; the id walk reads
/// the far endpoint out of the pinned leaf and appends it to one vector.
#[test]
fn neighbor_ids_allocate_a_constant_plus_their_result() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("db"), cfg()).unwrap();
    let people = db
        .create_collection(
            "people",
            vec![("name".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.enable_graph().unwrap();
    let knows = db.create_edge_type("knows").unwrap();
    const FAN: usize = 200;
    let hub = db.put(people, "hub", &json!({"name":"h"})).unwrap();
    let mut ids = Vec::new();
    for i in 0..FAN {
        ids.push(db.put(people, &format!("p{i:05}"), &json!({"name":"x"})).unwrap());
    }
    db.commit().unwrap();
    for id in &ids {
        db.put_edge(GraphContextId::BASE, *id, knows, hub, &json!({"r":1}))
            .unwrap();
    }
    db.commit().unwrap();
    drop(db);
    let db = Database::open(dir.path().join("db"), cfg()).unwrap();
    let request = NeighborRequest {
        entity: hub,
        direction: Direction::Incoming,
        context: GraphContextId::BASE,
        edge_type: Some(knows),
        limit: 256,
    };
    for _ in 0..2 {
        db.neighbor_ids(request).unwrap();
    }
    let (found, allocations, bytes) = measured(|| db.neighbor_ids(request).unwrap());
    assert_eq!(found.len(), FAN);
    println!("neighbor_ids over {FAN} incoming edges: {allocations} allocations, {bytes} bytes");
    assert!(
        allocations <= 24,
        "neighbor_ids over {FAN} incoming edges allocated {allocations} times; \
         the property path allocates a key Vec, a value Vec and a lookup per edge"
    );

    // Pages, not just bytes. An incoming `neighbors` read descends once for the
    // scan and once more per edge, to fetch the authoritative row the property
    // decode needs; the id read descends once, full stop. Both used to descend
    // one extra time before that, for the seed row nobody had asked about.
    let before = db.pool_accesses().unwrap();
    db.neighbor_ids(request).unwrap();
    let ids_pages = db.pool_accesses().unwrap() - before;
    let before = db.pool_accesses().unwrap();
    db.neighbors(request).unwrap();
    let edge_pages = db.pool_accesses().unwrap() - before;
    println!("incoming fan-in {FAN}: neighbor_ids {ids_pages} pool accesses, neighbors {edge_pages}");
    assert!(
        ids_pages < edge_pages,
        "neighbor_ids opened {ids_pages} pages and neighbors {edge_pages}: the id walk \
         must not be paying for the per-edge authoritative-row lookup"
    );
    assert!(
        ids_pages <= 8,
        "a single incoming fan-in opened {ids_pages} pages; one descent plus its \
         leaves is the budget, and a seed-row probe would add a whole descent"
    );
}

/// The seed-existence probe moved behind the scan, so it must still be there.
///
/// A graph read used to spend one point lookup of the seed's row on every
/// call, including the overwhelming majority where the seed plainly exists
/// because its edges are right there. An edge cannot outlive its endpoints --
/// writes validate both, deletes cascade -- so an edge found IS the proof the
/// row exists. Only a seed with no edge at all still needs the lookup, and
/// that answer must be exactly what it was.
#[test]
fn graph_reads_still_refuse_a_seed_that_has_no_row() {
    let (_dir, mut db, [a, b, _, _, _]) = graph_fixture();
    let knows = db.create_edge_type("knows").unwrap();
    db.put_edge(GraphContextId::BASE, a, knows, b, &json!({}))
        .unwrap();
    db.commit().unwrap();

    let neighbors_of = |entity| NeighborRequest {
        entity,
        direction: Direction::Both,
        context: GraphContextId::BASE,
        edge_type: Some(knows),
        limit: 8,
    };
    let bfs_from = |seed| BfsRequest {
        seed,
        direction: Direction::Both,
        context: GraphContextId::BASE,
        edge_type: Some(knows),
        min_depth: 0,
        max_depth: 2,
        include_seed: true,
        max_visited: 64,
        max_edges: 64,
        result_limit: 64,
    };

    // An identity the allocator never issued.
    let never = e4_prototype::collections::EntityId {
        collection: a.collection,
        sequence: 9_999_999,
    };
    assert!(matches!(
        db.neighbors(neighbors_of(never)),
        Err(Error::NotFound("graph endpoint"))
    ));
    assert!(matches!(
        db.neighbor_ids(neighbors_of(never)),
        Err(Error::NotFound("graph endpoint"))
    ));
    assert!(matches!(
        db.traverse_bfs(bfs_from(never)),
        Err(Error::NotFound("graph endpoint"))
    ));

    // A row that exists and has no edge is an empty answer, not an error.
    let lonely = db.put(a.collection, "lonely", &json!({"name":"l"})).unwrap();
    db.commit().unwrap();
    assert!(db.neighbors(neighbors_of(lonely)).unwrap().is_empty());
    assert!(db.neighbor_ids(neighbors_of(lonely)).unwrap().is_empty());
    let alone = db.traverse_bfs(bfs_from(lonely)).unwrap();
    assert_eq!(alone.visited, 1);
    assert_eq!(alone.nodes.len(), 1);

    // Deleting it takes the row away and the answer goes back to NotFound.
    assert!(db.delete(a.collection, "lonely").unwrap());
    db.commit().unwrap();
    assert!(matches!(
        db.neighbors(neighbors_of(lonely)),
        Err(Error::NotFound("graph endpoint"))
    ));
    assert!(matches!(
        db.neighbor_ids(neighbors_of(lonely)),
        Err(Error::NotFound("graph endpoint"))
    ));
    assert!(matches!(
        db.traverse_bfs(bfs_from(lonely)),
        Err(Error::NotFound("graph endpoint"))
    ));

    // A seed whose edges were all cascaded away by deleting its partner is
    // still a live row, so it answers empty rather than refusing.
    assert!(db.delete(b.collection, "b").unwrap());
    db.commit().unwrap();
    assert!(db.neighbor_ids(neighbors_of(a)).unwrap().is_empty());
    assert!(db.neighbors(neighbors_of(a)).unwrap().is_empty());
    assert_eq!(db.traverse_bfs(bfs_from(a)).unwrap().visited, 1);
}
