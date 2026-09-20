//! Exhaustive text-index key-write boundary injection. Included by
//! text_indexes.rs so the test-only backend fault hook remains available.
use crate::collections::{
    CollectionId, Database, EntityId, Error, IndexFamily, IndexId, IndexState, ScalarPredicate,
};
use super::*;
use kernel::{io::IoMode, store::{Config, SyncMode}};
use serde_json::json;
use std::collections::BTreeSet;

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

fn text_ids(db: &Database, index: IndexId, query: &str) -> BTreeSet<EntityId> {
    db.query_text(
        index,
        query,
        TextMatch::Any,
        16,
        TextCandidates::All,
        32,
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
    FirstTextCatalog,
    AnotherTextIndex,
    Insert,
    UpdateValue,
    UpdateEmpty,
    UpdateNull,
    Delete,
    BuildingUpdate,
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
    text: Option<IndexId>,
}

fn fixture(db: &mut Database, operation: Op) -> Fixture {
    let collection = db
        .create_collection(
            "rows",
            vec![("body".into(), Kind::Text), ("age".into(), Kind::Int)],
            Default::default(),
        )
        .unwrap();
    let a = db
        .put(collection, "a", &json!({"body":"alpha common","age":10}))
        .unwrap();
    let b = db
        .put(collection, "b", &json!({"body":"beta common","age":20}))
        .unwrap();
    if matches!(operation, Op::FirstTextCatalog) {
        db.commit().unwrap();
        return Fixture {
            collection,
            a,
            b,
            scalar: None,
            text: None,
        };
    }

    let scalar = db
        .create_scalar_index(collection, "age", "age", false)
        .unwrap();
    assert!(db.build_index_step(scalar, 16).unwrap());
    let text = db.create_text_index(collection, "body", "body").unwrap();
    if !matches!(
        operation,
        Op::BuildingUpdate | Op::BuildProgress | Op::BuildPublish
    ) {
        assert!(db.build_index_step(text, 16).unwrap());
    }
    db.commit().unwrap();
    Fixture {
        collection,
        a,
        b,
        scalar: Some(scalar),
        text: Some(text),
    }
}

impl Op {
    fn apply(self, db: &mut Database, fixture: &Fixture) -> Result<()> {
        match self {
            Self::FirstTextCatalog | Self::AnotherTextIndex => db
                .create_text_index(fixture.collection, "next", "body")
                .map(|_| ()),
            Self::Insert => db
                .put(
                    fixture.collection,
                    "new",
                    &json!({"body":"gamma common","age":30}),
                )
                .map(|_| ()),
            Self::UpdateValue => db
                .update(
                    fixture.collection,
                    "a",
                    &json!({"body":"gamma changed","age":5}),
                )
                .map(|_| ()),
            Self::UpdateEmpty => db
                .update(fixture.collection, "a", &json!({"body":"","age":5}))
                .map(|_| ()),
            Self::UpdateNull => db
                .update(fixture.collection, "a", &json!({"body":null,"age":5}))
                .map(|_| ()),
            Self::Delete => db
                .delete(fixture.collection, "a")
                .map(|deleted| assert!(deleted)),
            Self::BuildingUpdate => db
                .update(
                    fixture.collection,
                    "a",
                    &json!({"body":"gamma changed","age":5}),
                )
                .map(|_| ()),
            Self::BuildProgress => db
                .build_index_step(fixture.text.unwrap(), 1)
                .map(|done| assert!(!done)),
            Self::BuildPublish => db
                .build_index_step(fixture.text.unwrap(), 16)
                .map(|done| assert!(done)),
            Self::DropBegin => db.begin_drop_index(fixture.text.unwrap()),
            Self::DropProgress => db
                .drop_index_step(fixture.text.unwrap(), 1)
                .map(|done| assert!(!done)),
            Self::DropFinish => db
                .drop_index_step(fixture.text.unwrap(), 32)
                .map(|done| assert!(done)),
        }
    }
}

fn semantic_after(db: &Database, operation: Op, fixture: &Fixture) {
    match operation {
        Op::FirstTextCatalog => {
            let indexes = db.list_indexes(fixture.collection).unwrap();
            assert_eq!(indexes.len(), 1);
            assert_eq!(indexes[0].family, IndexFamily::Text);
            assert_eq!(read_corpus(db, indexes[0].id).unwrap().documents, 0);
        }
        Op::AnotherTextIndex => {
            let indexes = db.list_indexes(fixture.collection).unwrap();
            assert_eq!(indexes.len(), 3);
            assert_eq!(indexes[2].family, IndexFamily::Text);
            assert_eq!(read_corpus(db, indexes[2].id).unwrap().documents, 0);
        }
        Op::Insert => {
            assert_eq!(text_ids(db, fixture.text.unwrap(), "common").len(), 3);
            assert_eq!(scalar_ids(db, fixture.scalar.unwrap()).len(), 3);
            assert_eq!(
                db.get(fixture.collection, "new").unwrap().unwrap().document,
                json!({"body":"gamma common","age":30})
            );
        }
        Op::UpdateValue => {
            assert_eq!(
                text_ids(db, fixture.text.unwrap(), "gamma"),
                [fixture.a].into_iter().collect()
            );
            assert_eq!(
                scalar_ids(db, fixture.scalar.unwrap()),
                vec![fixture.a, fixture.b]
            );
        }
        Op::UpdateEmpty => {
            assert!(text_ids(db, fixture.text.unwrap(), "alpha").is_empty());
            assert_eq!(read_corpus(db, fixture.text.unwrap()).unwrap().documents, 2);
        }
        Op::UpdateNull => {
            assert!(text_ids(db, fixture.text.unwrap(), "alpha").is_empty());
            assert_eq!(read_corpus(db, fixture.text.unwrap()).unwrap().documents, 1);
        }
        Op::Delete => {
            assert!(db.get_by_id(fixture.a).unwrap().is_none());
            assert_eq!(
                text_ids(db, fixture.text.unwrap(), "common"),
                [fixture.b].into_iter().collect()
            );
            assert_eq!(scalar_ids(db, fixture.scalar.unwrap()), vec![fixture.b]);
        }
        Op::BuildingUpdate => {
            let index = fixture.text.unwrap();
            assert!(matches!(
                db.index_info(index).unwrap().state,
                IndexState::Building { after: 0 }
            ));
            assert_eq!(read_corpus(db, index).unwrap().documents, 1);
            assert!(db
                .store()
                .unwrap()
                .get(&norm_key(index, fixture.a.sequence))
                .unwrap()
                .is_some());
        }
        Op::BuildProgress => {
            let index = fixture.text.unwrap();
            assert!(
                matches!(db.index_info(index).unwrap().state, IndexState::Building { after } if after == fixture.a.sequence)
            );
            assert_eq!(read_corpus(db, index).unwrap().documents, 1);
        }
        Op::BuildPublish => {
            let index = fixture.text.unwrap();
            assert_eq!(db.index_info(index).unwrap().state, IndexState::Ready);
            assert_eq!(
                text_ids(db, index, "common"),
                [fixture.a, fixture.b].into_iter().collect()
            );
        }
        Op::DropBegin | Op::DropProgress => assert_eq!(
            db.index_info(fixture.text.unwrap()).unwrap().state,
            IndexState::Dropping
        ),
        Op::DropFinish => assert!(matches!(
            db.index_info(fixture.text.unwrap()),
            Err(Error::NotFound("index"))
        )),
    }
}

#[test]
fn text_write_boundaries_preserve_entities_stats_indexes_and_readers() {
    for operation in [
        Op::FirstTextCatalog,
        Op::AnotherTextIndex,
        Op::Insert,
        Op::UpdateValue,
        Op::UpdateEmpty,
        Op::UpdateNull,
        Op::Delete,
        Op::BuildingUpdate,
        Op::BuildProgress,
        Op::BuildPublish,
        Op::DropBegin,
        Op::DropProgress,
        Op::DropFinish,
    ] {
        let mut reached_success = false;
        for fail_at in 0..160 {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("db");
            let mut db = Database::create(&path, cfg()).unwrap();
            let fixture = fixture(&mut db, operation);
            let old = Database::open_snapshot(&path, cfg()).unwrap();
            let old_rows = all_rows(&old);
            let old_text = fixture
                .text
                .filter(|index| old.index_info(*index).unwrap().state == IndexState::Ready);

            if matches!(operation, Op::DropProgress | Op::DropFinish) {
                db.begin_drop_index(fixture.text.unwrap()).unwrap();
                db.commit().unwrap();
            }
            let baseline = all_rows(&db);
            let baseline_indexes = db.list_indexes(fixture.collection).unwrap();
            db.store().unwrap().arm_write_fault(fail_at);
            let result = operation.apply(&mut db, &fixture);
            if result.is_ok() {
                assert!(fail_at > 0, "{operation:?} performed no tested writes");
                assert_eq!(all_rows(&old), old_rows);
                drop(db);
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
            assert_eq!(all_rows(&old), old_rows, "pinned text reader changed");
            let published = Database::open_snapshot(&path, cfg()).unwrap();
            assert_eq!(
                all_rows(&published),
                baseline,
                "partial text publication at {operation:?}/{fail_at}"
            );

            db.rollback().unwrap();
            assert_eq!(all_rows(&db), baseline, "text rollback was incomplete");
            assert_eq!(
                db.list_indexes(fixture.collection).unwrap(),
                baseline_indexes
            );
            if let Some(index) = old_text {
                assert_eq!(
                    text_ids(&old, index, "common"),
                    [fixture.a, fixture.b].into_iter().collect()
                );
            }

            operation.apply(&mut db, &fixture).unwrap();
            db.commit().unwrap();
            semantic_after(&db, operation, &fixture);
            let committed = all_rows(&db);
            assert_eq!(all_rows(&old), old_rows);
            if let Some(index) = old_text {
                assert_eq!(
                    text_ids(&old, index, "common"),
                    [fixture.a, fixture.b].into_iter().collect()
                );
            }
            drop(published);
            drop(db);
            let reopened = Database::open(&path, cfg()).unwrap();
            assert_eq!(all_rows(&reopened), committed);
            semantic_after(&reopened, operation, &fixture);
        }
        assert!(
            reached_success,
            "{operation:?} exceeded 160 writes or failed without injection"
        );
    }
}
