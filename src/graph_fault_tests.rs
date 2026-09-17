//! Exhaustive graph key-write boundary injection. Included by
//! graph_collections.rs so the test-only backend fault hook remains available.
use super::*;
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

fn all_rows(db: &Database) -> Vec<(Vec<u8>, Vec<u8>)> {
    db.store()
        .unwrap()
        .range(&[])
        .unwrap()
        .map(|row| row.unwrap())
        .collect()
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Enable,
    LinkWithNames,
    Put,
    Replace,
    Delete,
    Cascade,
}

struct Fixture {
    collection: CollectionId,
    a: EntityId,
    b: EntityId,
    keep: EntityId,
    edge_type: EdgeTypeId,
    edge: EdgeKey,
    scalar: IndexId,
}

fn fixture(db: &mut Database, operation: Op) -> Fixture {
    let collection = db
        .create_collection("nodes", vec![("age".into(), Kind::Int)], Default::default())
        .unwrap();
    let a = db.put(collection, "a", &json!({"age":10})).unwrap();
    let b = db.put(collection, "b", &json!({"age":20})).unwrap();
    let keep = db.put(collection, "keep", &json!({"age":30})).unwrap();
    let scalar = db
        .create_scalar_index(collection, "age", "age", false)
        .unwrap();
    assert!(db.build_index_step(scalar, 16).unwrap());
    db.commit().unwrap();

    if matches!(operation, Op::Enable) {
        return Fixture {
            collection,
            a,
            b,
            keep,
            edge_type: EdgeTypeId(1),
            edge: EdgeKey {
                source: a,
                context: GraphContextId::BASE,
                edge_type: EdgeTypeId(1),
                destination: b,
            },
            scalar,
        };
    }

    db.enable_graph().unwrap();
    let edge_type = db.create_edge_type("base").unwrap();
    let context = db.create_graph_context("history").unwrap();
    let edge = EdgeKey {
        source: a,
        context: GraphContextId::BASE,
        edge_type,
        destination: b,
    };
    if matches!(operation, Op::Replace | Op::Delete | Op::Cascade) {
        db.put_edge(
            edge.context,
            edge.source,
            edge.edge_type,
            edge.destination,
            &json!({"version":1}),
        )
        .unwrap();
    }
    if matches!(operation, Op::Cascade) {
        db.put_edge(context, b, edge_type, a, &json!({"incoming":true}))
            .unwrap();
        db.put_edge(context, b, edge_type, b, &json!({"self":true}))
            .unwrap();
        db.put_edge(
            GraphContextId::BASE,
            a,
            edge_type,
            keep,
            &json!({"keep":true}),
        )
        .unwrap();
    }
    db.commit().unwrap();
    Fixture {
        collection,
        a,
        b,
        keep,
        edge_type,
        edge,
        scalar,
    }
}

impl Op {
    fn apply(self, db: &mut Database, f: &Fixture) -> Result<()> {
        match self {
            Self::Enable => db.enable_graph(),
            Self::LinkWithNames => db
                .link(f.a, "new-type", f.b, "new-context", &json!({"new":true}))
                .map(|_| ()),
            Self::Put => db
                .put_edge(
                    GraphContextId::BASE,
                    f.a,
                    f.edge_type,
                    f.b,
                    &json!({"created":true}),
                )
                .map(|_| ()),
            Self::Replace => db
                .put_edge(
                    f.edge.context,
                    f.edge.source,
                    f.edge.edge_type,
                    f.edge.destination,
                    &json!({"version":2}),
                )
                .map(|_| ()),
            Self::Delete => db.delete_edge(f.edge).map(|deleted| assert!(deleted)),
            Self::Cascade => db.delete(f.collection, "b").map(|deleted| assert!(deleted)),
        }
    }
}

fn semantic_after(db: &Database, operation: Op, f: &Fixture) {
    match operation {
        Op::Enable => assert_eq!(db.graph_context("").unwrap(), Some(GraphContextId::BASE)),
        Op::LinkWithNames => {
            let edge_type = db.edge_type("new-type").unwrap().unwrap();
            let context = db.graph_context("new-context").unwrap().unwrap();
            let edges = db
                .neighbors(NeighborRequest {
                    entity: f.a,
                    direction: Direction::Outgoing,
                    context,
                    edge_type: Some(edge_type),
                    limit: 2,
                })
                .unwrap();
            assert_eq!(edges[0].properties, json!({"new":true}));
        }
        Op::Put => assert_eq!(
            db.neighbors(NeighborRequest {
                entity: f.a,
                direction: Direction::Outgoing,
                context: GraphContextId::BASE,
                edge_type: Some(f.edge_type),
                limit: 2,
            })
            .unwrap()[0]
                .properties,
            json!({"created":true})
        ),
        Op::Replace => assert_eq!(
            db.neighbors(NeighborRequest {
                entity: f.a,
                direction: Direction::Outgoing,
                context: GraphContextId::BASE,
                edge_type: Some(f.edge_type),
                limit: 2,
            })
            .unwrap()[0]
                .properties,
            json!({"version":2})
        ),
        Op::Delete => assert!(db
            .neighbors(NeighborRequest {
                entity: f.a,
                direction: Direction::Outgoing,
                context: GraphContextId::BASE,
                edge_type: Some(f.edge_type),
                limit: 2,
            })
            .unwrap()
            .is_empty()),
        Op::Cascade => {
            assert!(db.get_by_id(f.b).unwrap().is_none());
            assert_eq!(
                db.query_scalar(
                    f.scalar,
                    ScalarPredicate::Range {
                        lower: None,
                        upper: None,
                    },
                    8,
                )
                .unwrap(),
                vec![f.a, f.keep]
            );
            let kept = db
                .neighbors(NeighborRequest {
                    entity: f.a,
                    direction: Direction::Outgoing,
                    context: GraphContextId::BASE,
                    edge_type: Some(f.edge_type),
                    limit: 2,
                })
                .unwrap();
            assert_eq!(kept.len(), 1);
            assert_eq!(kept[0].key.destination, f.keep);
        }
    }
}

#[test]
fn graph_write_boundaries_preserve_edges_scalar_postings_and_readers() {
    for operation in [
        Op::Enable,
        Op::LinkWithNames,
        Op::Put,
        Op::Replace,
        Op::Delete,
        Op::Cascade,
    ] {
        let mut reached_success = false;
        for fail_at in 0..64 {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("db");
            let mut db = Database::create(&path, cfg()).unwrap();
            let f = fixture(&mut db, operation);
            let old = Database::open_snapshot(&path, cfg()).unwrap();
            let old_rows = all_rows(&old);
            let baseline = all_rows(&db);
            db.store().unwrap().arm_write_fault(fail_at);
            let result = operation.apply(&mut db, &f);
            if result.is_ok() {
                assert!(fail_at > 0, "{operation:?} performed no writes");
                assert_eq!(all_rows(&old), old_rows);
                drop(db); // successful uncommitted candidate must not publish
                let reopened = Database::open(&path, cfg()).unwrap();
                assert_eq!(all_rows(&reopened), baseline);
                reached_success = true;
                break;
            }

            assert!(
                db.commit().is_err(),
                "{operation:?}/{fail_at} did not poison writer"
            );
            assert!(db.get_by_id(f.a).is_err());
            assert_eq!(all_rows(&old), old_rows, "old reader changed");
            let published = Database::open_snapshot(&path, cfg()).unwrap();
            assert_eq!(all_rows(&published), baseline, "partial graph publication");
            db.rollback().unwrap();
            assert_eq!(all_rows(&db), baseline, "graph rollback was incomplete");
            operation.apply(&mut db, &f).unwrap();
            db.commit().unwrap();
            semantic_after(&db, operation, &f);
            let committed = all_rows(&db);
            assert_eq!(all_rows(&old), old_rows);
            drop(published);
            drop(db);
            let reopened = Database::open(&path, cfg()).unwrap();
            assert_eq!(all_rows(&reopened), committed);
            semantic_after(&reopened, operation, &f);
        }
        assert!(reached_success, "{operation:?} exceeded 64 graph writes");
    }
}
