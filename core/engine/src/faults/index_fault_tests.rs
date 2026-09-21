//! Every injected key-write failure must leave the last published transaction
//! intact, including catalog replicas and postings. Included by indexes.rs.
use super::*;
use kernel::{io::IoMode, store::SyncMode};
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
        .map(|r| r.unwrap())
        .collect()
}
fn all_ids(db: &Database, index: IndexId) -> Vec<EntityId> {
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
    FirstCatalog,
    AnotherIndex,
    Update,
    Delete,
    BuildProgress,
    BuildPublish,
    DropBegin,
    DropProgress,
    DropFinish,
}
impl Op {
    fn apply(self, db: &mut Database, c: CollectionId, i: Option<IndexId>) -> Result<()> {
        match self {
            Self::FirstCatalog | Self::AnotherIndex => {
                db.create_scalar_index(c, "next", "age", false).map(|_| ())
            }
            Self::Update => db.update(c, "a", &json!({"age":30})).map(|_| ()),
            Self::Delete => db.delete(c, "a").map(|deleted| assert!(deleted)),
            Self::BuildProgress => db
                .build_index_step(i.unwrap(), 1)
                .map(|done| assert!(!done)),
            Self::BuildPublish => db
                .build_index_step(i.unwrap(), 16)
                .map(|done| assert!(done)),
            Self::DropBegin => db.begin_drop_index(i.unwrap()),
            Self::DropProgress => db.drop_index_step(i.unwrap(), 1).map(|done| assert!(!done)),
            Self::DropFinish => db.drop_index_step(i.unwrap(), 16).map(|done| assert!(done)),
        }
    }
}

#[test]
fn scalar_index_write_boundaries_preserve_committed_state_and_readers() {
    for operation in [
        Op::FirstCatalog,
        Op::AnotherIndex,
        Op::Update,
        Op::Delete,
        Op::BuildProgress,
        Op::BuildPublish,
        Op::DropBegin,
        Op::DropProgress,
        Op::DropFinish,
    ] {
        let mut reached_success = false;
        // Stop at the first non-failing boundary, so changes in the number of
        // storage writes cannot silently leave newly added writes untested.
        for fail_at in 0..64 {
            let t = tempfile::tempdir().unwrap();
            let p = t.path().join("db");
            let mut db = Database::create(&p, cfg()).unwrap();
            let c = db
                .create_collection(
                    "people",
                    vec![("age".into(), Kind::Int)],
                    Default::default(),
                )
                .unwrap();
            let a = db.put(c, "a", &json!({"age":10})).unwrap();
            let b = db.put(c, "b", &json!({"age":20})).unwrap();
            db.commit().unwrap();
            let index = if matches!(operation, Op::FirstCatalog) {
                None
            } else {
                let i = db.create_scalar_index(c, "age", "age", false).unwrap();
                if !matches!(operation, Op::BuildProgress | Op::BuildPublish) {
                    assert!(db.build_index_step(i, 16).unwrap());
                }
                db.commit().unwrap();
                Some(i)
            };
            // For drop reclamation the held reader remains on the READY
            // generation while the writer commits DROPPING before injection.
            let old = Database::open_snapshot(&p, cfg()).unwrap();
            let old_rows = all_rows(&old);
            let old_ready =
                index.filter(|i| old.index_info(*i).unwrap().state == IndexState::Ready);
            if matches!(operation, Op::DropProgress | Op::DropFinish) {
                db.begin_drop_index(index.unwrap()).unwrap();
                db.commit().unwrap();
            }
            let baseline = all_rows(&db);
            let baseline_indexes = db.list_indexes(c).unwrap();
            db.store().unwrap().arm_write_fault(fail_at);
            let result = operation.apply(&mut db, c, index);
            if result.is_ok() {
                assert!(fail_at > 0, "{operation:?} performed no tested writes");
                assert_eq!(all_rows(&old), old_rows);
                // No commit here: the fault is still armed for the next write.
                drop(db);
                let reopened = Database::open(&p, cfg()).unwrap();
                assert_eq!(all_rows(&reopened), baseline);
                reached_success = true;
                break;
            }
            assert!(
                db.commit().is_err(),
                "{operation:?} at {fail_at} must poison failed writer"
            );
            assert!(db.get_by_id(a).is_err());
            assert_eq!(
                all_rows(&old),
                old_rows,
                "pinned snapshot changed at {operation:?}/{fail_at}"
            );
            let published = Database::open_snapshot(&p, cfg()).unwrap();
            assert_eq!(
                all_rows(&published),
                baseline,
                "partial publication at {operation:?}/{fail_at}"
            );
            db.rollback().unwrap();
            assert_eq!(
                all_rows(&db),
                baseline,
                "incomplete rollback at {operation:?}/{fail_at}"
            );
            assert_eq!(db.list_indexes(c).unwrap(), baseline_indexes);
            assert_eq!(
                db.get_by_id(a).unwrap().unwrap().document,
                json!({"age":10})
            );
            assert_eq!(
                db.get_by_id(b).unwrap().unwrap().document,
                json!({"age":20})
            );
            if let Some(i) = old_ready {
                assert_eq!(all_ids(&old, i), vec![a, b]);
            }
            // The injected error is consumed. Retry really commits, proving
            // rollback restored a usable writer, then reopen the catalog.
            operation.apply(&mut db, c, index).unwrap();
            db.commit().unwrap();
            let committed = all_rows(&db);
            assert_eq!(all_rows(&old), old_rows);
            if let Some(i) = old_ready {
                assert_eq!(all_ids(&old, i), vec![a, b]);
            }
            drop(published);
            drop(db);
            let reopened = Database::open(&p, cfg()).unwrap();
            assert_eq!(all_rows(&reopened), committed);
        }
        assert!(
            reached_success,
            "{operation:?} exceeded 64 writes or failed without injection"
        );
    }
}
