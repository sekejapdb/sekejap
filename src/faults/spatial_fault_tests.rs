//! Exhaustive spatial key-write boundary injection. Included by
//! spatial_indexes.rs so the test-only backend fault hook remains available.
use crate::collections::{
    CollectionId, Database, EntityId, Error, IndexFamily, IndexId, IndexState, ScalarPredicate,
};
use super::*;
use kernel::{io::IoMode, store::{Config, SyncMode}};
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

fn spatial_ids(db: &Database, index: IndexId) -> Vec<EntityId> {
    db.query_point_bbox(
        index,
        Bounds::new(-180.0, 180.0, -90.0, 90.0).unwrap(),
        16,
        SpatialCandidates::All,
        16,
        || false,
    )
    .unwrap()
}

fn nearest_ids(db: &Database, index: IndexId) -> Vec<EntityId> {
    db.query_point_nearest(
        index,
        Point::new(0.0, 0.0).unwrap(),
        16,
        SpatialCandidates::All,
        16,
        || false,
    )
    .unwrap()
    .into_iter()
    .map(|hit| hit.id)
    .collect()
}

fn scalar_ids(db: &Database, index: IndexId) -> Vec<EntityId> {
    db.query_scalar(
        index,
        ScalarPredicate::Range {
            lower: None,
            upper: None,
        },
        16,
    )
    .unwrap()
}

#[derive(Clone, Copy, Debug)]
enum Op {
    FirstSpatialCatalog,
    AnotherSpatialIndex,
    Insert,
    UpdateValue,
    UpdateNull,
    Delete,
    BuildProgress,
    BuildPublish,
    DropBegin,
    DropProgress,
    DropFinish,
}

struct Fixture {
    collection: CollectionId,
    a: EntityId,
    b: EntityId,
    scalar: Option<IndexId>,
    spatial: Option<IndexId>,
}

fn fixture(db: &mut Database, operation: Op) -> Fixture {
    let collection = db
        .create_collection(
            "rows",
            vec![("p".into(), Kind::Point), ("age".into(), Kind::Int)],
            Default::default(),
        )
        .unwrap();
    let a = db
        .put(
            collection,
            "a",
            &json!({"p":{"type":"Point","coordinates":[0.0,0.0]},"age":10}),
        )
        .unwrap();
    let b = db
        .put(
            collection,
            "b",
            &json!({"p":{"type":"Point","coordinates":[2.0,0.0]},"age":20}),
        )
        .unwrap();
    if matches!(operation, Op::FirstSpatialCatalog) {
        db.commit().unwrap();
        return Fixture {
            collection,
            a,
            b,
            scalar: None,
            spatial: None,
        };
    }

    let scalar = db
        .create_scalar_index(collection, "age", "age", false)
        .unwrap();
    assert!(db.build_index_step(scalar, 16).unwrap());
    let spatial = db.create_point_index(collection, "p", "p").unwrap();
    if !matches!(operation, Op::BuildProgress | Op::BuildPublish) {
        assert!(db.build_index_step(spatial, 16).unwrap());
    }
    db.commit().unwrap();
    Fixture {
        collection,
        a,
        b,
        scalar: Some(scalar),
        spatial: Some(spatial),
    }
}

impl Op {
    fn apply(self, db: &mut Database, fixture: &Fixture) -> Result<()> {
        match self {
            Self::FirstSpatialCatalog | Self::AnotherSpatialIndex => db
                .create_point_index(fixture.collection, "next", "p")
                .map(|_| ()),
            Self::Insert => db
                .put(
                    fixture.collection,
                    "new",
                    &json!({"p":{"type":"Point","coordinates":[3.0,0.0]},"age":30}),
                )
                .map(|_| ()),
            Self::UpdateValue => db
                .update(
                    fixture.collection,
                    "a",
                    &json!({"p":{"type":"Point","coordinates":[3.0,0.0]},"age":5}),
                )
                .map(|_| ()),
            Self::UpdateNull => db
                .update(fixture.collection, "a", &json!({"p":null,"age":5}))
                .map(|_| ()),
            Self::Delete => db
                .delete(fixture.collection, "a")
                .map(|deleted| assert!(deleted)),
            Self::BuildProgress => db
                .build_index_step(fixture.spatial.unwrap(), 1)
                .map(|done| assert!(!done)),
            Self::BuildPublish => db
                .build_index_step(fixture.spatial.unwrap(), 16)
                .map(|done| assert!(done)),
            Self::DropBegin => db.begin_drop_index(fixture.spatial.unwrap()),
            Self::DropProgress => db
                .drop_index_step(fixture.spatial.unwrap(), 1)
                .map(|done| assert!(!done)),
            Self::DropFinish => db
                .drop_index_step(fixture.spatial.unwrap(), 16)
                .map(|done| assert!(done)),
        }
    }
}

fn semantic_after(db: &Database, operation: Op, fixture: &Fixture) {
    match operation {
        Op::FirstSpatialCatalog => {
            let indexes = db.list_indexes(fixture.collection).unwrap();
            assert_eq!(indexes.len(), 1);
            assert_eq!(indexes[0].family, IndexFamily::SpatialPoint);
            assert!(matches!(
                indexes[0].state,
                IndexState::Building { after: 0 }
            ));
        }
        Op::AnotherSpatialIndex => {
            let indexes = db.list_indexes(fixture.collection).unwrap();
            assert_eq!(indexes.len(), 3);
            assert_eq!(indexes[2].family, IndexFamily::SpatialPoint);
        }
        Op::Insert => {
            assert_eq!(spatial_ids(db, fixture.spatial.unwrap()).len(), 3);
            assert_eq!(scalar_ids(db, fixture.scalar.unwrap()).len(), 3);
            assert_eq!(
                db.get(fixture.collection, "new").unwrap().unwrap().document,
                json!({"p":{"type":"Point","coordinates":[3.0,0.0]},"age":30})
            );
        }
        Op::UpdateValue => {
            assert_eq!(
                nearest_ids(db, fixture.spatial.unwrap()),
                vec![fixture.b, fixture.a]
            );
            assert_eq!(
                scalar_ids(db, fixture.scalar.unwrap()),
                vec![fixture.a, fixture.b]
            );
        }
        Op::UpdateNull => {
            assert_eq!(spatial_ids(db, fixture.spatial.unwrap()), vec![fixture.b]);
            assert_eq!(
                scalar_ids(db, fixture.scalar.unwrap()),
                vec![fixture.a, fixture.b]
            );
        }
        Op::Delete => {
            assert!(db.get_by_id(fixture.a).unwrap().is_none());
            assert_eq!(spatial_ids(db, fixture.spatial.unwrap()), vec![fixture.b]);
            assert_eq!(scalar_ids(db, fixture.scalar.unwrap()), vec![fixture.b]);
        }
        Op::BuildProgress => assert!(matches!(
            db.index_info(fixture.spatial.unwrap()).unwrap().state,
            IndexState::Building { after } if after == fixture.a.sequence
        )),
        Op::BuildPublish => {
            assert_eq!(
                db.index_info(fixture.spatial.unwrap()).unwrap().state,
                IndexState::Ready
            );
            assert_eq!(
                spatial_ids(db, fixture.spatial.unwrap()),
                vec![fixture.a, fixture.b]
            );
        }
        Op::DropBegin | Op::DropProgress => assert_eq!(
            db.index_info(fixture.spatial.unwrap()).unwrap().state,
            IndexState::Dropping
        ),
        Op::DropFinish => assert!(matches!(
            db.index_info(fixture.spatial.unwrap()),
            Err(Error::NotFound("index"))
        )),
    }
}

#[test]
fn spatial_write_boundaries_preserve_entities_indexes_and_readers() {
    for operation in [
        Op::FirstSpatialCatalog,
        Op::AnotherSpatialIndex,
        Op::Insert,
        Op::UpdateValue,
        Op::UpdateNull,
        Op::Delete,
        Op::BuildProgress,
        Op::BuildPublish,
        Op::DropBegin,
        Op::DropProgress,
        Op::DropFinish,
    ] {
        let mut reached_success = false;
        for fail_at in 0..96 {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("db");
            let mut db = Database::create(&path, cfg()).unwrap();
            let fixture = fixture(&mut db, operation);
            let old = Database::open_snapshot(&path, cfg()).unwrap();
            let old_rows = all_rows(&old);
            let old_spatial = fixture
                .spatial
                .filter(|index| old.index_info(*index).unwrap().state == IndexState::Ready);

            if matches!(operation, Op::DropProgress | Op::DropFinish) {
                db.begin_drop_index(fixture.spatial.unwrap()).unwrap();
                db.commit().unwrap();
            }
            let baseline = all_rows(&db);
            let baseline_indexes = db.list_indexes(fixture.collection).unwrap();
            db.store().unwrap().arm_write_fault(fail_at);
            let result = operation.apply(&mut db, &fixture);
            if result.is_ok() {
                assert!(fail_at > 0, "{operation:?} performed no tested writes");
                assert_eq!(all_rows(&old), old_rows);
                drop(db); // a successful uncommitted candidate is discarded
                let reopened = Database::open(&path, cfg()).unwrap();
                assert_eq!(all_rows(&reopened), baseline);
                reached_success = true;
                break;
            }

            assert!(
                db.commit().is_err(),
                "{operation:?}/{fail_at} did not poison the writer"
            );
            assert!(db.get_by_id(fixture.a).is_err());
            assert_eq!(all_rows(&old), old_rows, "pinned spatial reader changed");
            let published = Database::open_snapshot(&path, cfg()).unwrap();
            assert_eq!(
                all_rows(&published),
                baseline,
                "partial spatial publication at {operation:?}/{fail_at}"
            );

            db.rollback().unwrap();
            assert_eq!(all_rows(&db), baseline, "spatial rollback was incomplete");
            assert_eq!(
                db.list_indexes(fixture.collection).unwrap(),
                baseline_indexes
            );
            if let Some(index) = old_spatial {
                assert_eq!(spatial_ids(&old, index), vec![fixture.a, fixture.b]);
            }

            operation.apply(&mut db, &fixture).unwrap();
            db.commit().unwrap();
            semantic_after(&db, operation, &fixture);
            let committed = all_rows(&db);
            assert_eq!(all_rows(&old), old_rows);
            if let Some(index) = old_spatial {
                assert_eq!(spatial_ids(&old, index), vec![fixture.a, fixture.b]);
            }
            drop(published);
            drop(db);
            let reopened = Database::open(&path, cfg()).unwrap();
            assert_eq!(all_rows(&reopened), committed);
            semantic_after(&reopened, operation, &fixture);
        }
        assert!(
            reached_success,
            "{operation:?} exceeded 96 writes or failed without injection"
        );
    }
}
