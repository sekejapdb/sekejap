//! Phase 2 scalar/catalog contract. Independent expected IDs, including late
//! indexing while writes continue, rollback and pinned-reader visibility.
use sekejap_core::{
    collections::{CollectionOptions, Database, IndexState, ScalarPredicate},
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
#[test]
fn late_index_tracks_crud_and_published_snapshots() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("db");
    let mut db = Database::create(&p, cfg()).unwrap();
    let c = db
        .create_collection(
            "people",
            vec![("age".into(), Kind::Int), ("name".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    let a = db.put(c, "a", &json!({"age":10,"name":"Alice"})).unwrap();
    let b = db.put(c, "b", &json!({"age":20,"name":"Bob"})).unwrap();
    let d = db.put(c, "d", &json!({"age":20,"name":"Dina"})).unwrap();
    db.commit().unwrap();
    let index = db.create_scalar_index(c, "age_idx", "age", false).unwrap();
    assert!(matches!(
        db.index_info(index).unwrap().state,
        IndexState::Building { .. }
    ));
    assert!(db
        .query_scalar(index, ScalarPredicate::Eq(json!(20)), 10)
        .is_err());
    assert!(!db.build_index_step(index, 1).unwrap());
    db.commit().unwrap();
    db.update(c, "a", &json!({"age":30})).unwrap();
    db.delete(c, "b").unwrap();
    let e = db.put(c, "e", &json!({"age":20,"name":"Eve"})).unwrap();
    while !db.build_index_step(index, 2).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();
    assert_eq!(
        db.query_scalar(index, ScalarPredicate::Eq(json!(20)), 10)
            .unwrap(),
        vec![d, e]
    );
    assert_eq!(
        db.query_scalar(
            index,
            ScalarPredicate::Range {
                lower: Some(json!(20)),
                upper: Some(json!(30))
            },
            10
        )
        .unwrap(),
        vec![d, e, a]
    );
    let snap = Database::open_snapshot(&p, cfg()).unwrap();
    db.update(c, "d", &json!({"age":99})).unwrap();
    db.commit().unwrap();
    assert_eq!(
        snap.query_scalar(index, ScalarPredicate::Eq(json!(20)), 10)
            .unwrap(),
        vec![d, e]
    );
    assert_eq!(
        db.query_scalar(index, ScalarPredicate::Eq(json!(20)), 10)
            .unwrap(),
        vec![e]
    );
    db.update(c, "e", &json!({"age":7})).unwrap();
    db.rollback().unwrap();
    assert_eq!(
        db.query_scalar(index, ScalarPredicate::Eq(json!(20)), 10)
            .unwrap(),
        vec![e]
    );
    drop(snap);
    drop(db);
    let mut db = Database::open(&p, cfg()).unwrap();
    assert_eq!(db.list_indexes(c).unwrap().len(), 1);
    assert_eq!(
        db.query_scalar(index, ScalarPredicate::Eq(json!(20)), 10)
            .unwrap(),
        vec![e]
    );
    assert!(db.get_by_id(b).unwrap().is_none());
    db.begin_drop_index(index).unwrap();
    db.commit().unwrap();
    assert!(db
        .query_scalar(index, ScalarPredicate::Eq(json!(20)), 10)
        .is_err());
    while !db.drop_index_step(index, 1).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();
    assert!(db.list_indexes(c).unwrap().is_empty());
    assert!(db.get_by_id(a).unwrap().is_some());
}
#[test]
fn unique_conflict_and_schema_changes_do_not_leave_partial_indexes() {
    let t = tempfile::tempdir().unwrap();
    let mut db = Database::create(t.path().join("db"), cfg()).unwrap();
    let c = db
        .create_collection(
            "p",
            vec![("name".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    let idx = db
        .create_scalar_index(c, "name_unique", "name", true)
        .unwrap();
    assert!(db.build_index_step(idx, 8).unwrap());
    let a = db.put(c, "a", &json!({"name":"A"})).unwrap();
    db.commit().unwrap();
    assert!(db.put(c, "b", &json!({"name":"A"})).is_err());
    db.rollback().unwrap();
    assert!(db.get(c, "b").unwrap().is_none());
    assert_eq!(
        db.query_scalar(idx, ScalarPredicate::Eq(json!("A")), 8)
            .unwrap(),
        vec![a]
    );
    assert!(db
        .alter_collection(c, vec![("name".into(), Kind::Int)])
        .is_err());
    // SQL-style unique semantics: null and missing do not conflict.
    db.put(c, "n1", &json!({"name":null})).unwrap();
    db.put(c, "n2", &json!({})).unwrap();
    db.commit().unwrap();
    assert_eq!(
        db.query_scalar(idx, ScalarPredicate::Eq(json!(null)), 8)
            .unwrap()
            .len(),
        2
    );
}
#[test]
fn drop_and_build_can_resume_after_reopen_without_claiming_ready() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("db");
    let mut db = Database::create(&p, cfg()).unwrap();
    let c = db
        .create_collection(
            "p",
            vec![("x".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    for i in 0..11 {
        db.put(c, &format!("p{i}"), &json!({"x":i})).unwrap();
    }
    db.commit().unwrap();
    let idx = db.create_scalar_index(c, "x", "x", false).unwrap();
    assert!(!db.build_index_step(idx, 3).unwrap());
    db.commit().unwrap();
    drop(db);
    let mut db = Database::open(&p, cfg()).unwrap();
    assert!(matches!(
        db.index_info(idx).unwrap().state,
        IndexState::Building { .. }
    ));
    while !db.build_index_step(idx, 3).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();
    assert_eq!(
        db.query_scalar(
            idx,
            ScalarPredicate::Range {
                lower: None,
                upper: None
            },
            20
        )
        .unwrap()
        .len(),
        11
    );
    db.begin_drop_index(idx).unwrap();
    assert!(!db.drop_index_step(idx, 3).unwrap());
    db.commit().unwrap();
    drop(db);
    let mut db = Database::open(&p, cfg()).unwrap();
    assert_eq!(db.index_info(idx).unwrap().state, IndexState::Dropping);
    while !db.drop_index_step(idx, 3).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();
    assert!(db.list_indexes(c).unwrap().is_empty());
}
