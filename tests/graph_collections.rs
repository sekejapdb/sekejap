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
