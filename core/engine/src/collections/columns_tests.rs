//! F1 on rows. What is at risk, one test each:
//!
//! * a populated column renamed reads back under the new name in every row,
//!   whatever layout wrote it, and its index follows it, across reopen
//!   (`rename_reaches_every_row_and_index`);
//! * a dropped column's values never come back, not even when a column of
//!   the same name is added later (`a_dropped_column_stays_dropped`);
//! * a 0.18-format file refuses both by name (`a_legacy_file_refuses`).

use super::*;
use crate::supportive::header::FORCE;
use kernel::{io::IoMode, store::SyncMode};
use serde_json::json;

fn cfg() -> Config {
    Config { budget_bytes: 1 << 20, io: IoMode::Buffered, sync: SyncMode::Full }
}
fn fields(names: &[(&str, Kind)]) -> Vec<(String, Kind)> {
    names.iter().map(|(n, k)| (n.to_string(), k.clone())).collect()
}

#[test]
fn rename_reaches_every_row_and_index() {
    FORCE.with(|f| f.set(Some(true)));
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let c = db.create_collection("t", fields(&[("a", Kind::Int), ("b", Kind::Text)]), Default::default()).unwrap();
    let ix = db.create_scalar_index(c, "t_a", "a", false).unwrap();
    db.put(c, "r1", &json!({"a": 1, "b": "x"})).unwrap();
    db.commit().unwrap();
    while !db.build_index_step(ix, 64).unwrap() {}
    db.commit().unwrap();
    // A second layout, so rows of two layouts are renamed at once.
    db.alter_collection(c, fields(&[("a", Kind::Int), ("b", Kind::Text), ("c", Kind::Int)])).unwrap();
    db.put(c, "r2", &json!({"a": 2, "b": "y", "c": 3})).unwrap();
    db.commit().unwrap();
    db.rename_column(c, "a", "n").unwrap();
    db.commit().unwrap();
    drop(db);
    let db = Database::open(&path, cfg()).unwrap();
    assert_eq!(db.get(c, "r1").unwrap().unwrap().document, json!({"n": 1, "b": "x"}));
    assert_eq!(db.get(c, "r2").unwrap().unwrap().document, json!({"n": 2, "b": "y", "c": 3}));
    let info = db.index_info(ix).unwrap();
    assert_eq!(info.field, "n");
    let hits = db
        .query_scalar(ix, ScalarPredicate::Eq(json!(2)), 10)
        .unwrap();
    assert_eq!(hits.len(), 1);
    let names: Vec<String> = db.collection_info(c).unwrap().layout.fields.into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, ["n", "b", "c"]);
}

#[test]
fn a_dropped_column_stays_dropped() {
    FORCE.with(|f| f.set(Some(true)));
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let c = db.create_collection("t", fields(&[("a", Kind::Int), ("b", Kind::Text)]), Default::default()).unwrap();
    db.put(c, "old", &json!({"a": 1, "b": "secret"})).unwrap();
    db.commit().unwrap();
    db.drop_column(c, "b").unwrap();
    db.commit().unwrap();
    assert_eq!(db.get(c, "old").unwrap().unwrap().document, json!({"a": 1}));
    // The same name again is a new column: the old value does not return.
    db.alter_collection(c, fields(&[("a", Kind::Int), ("b", Kind::Text)])).unwrap();
    db.put(c, "new", &json!({"a": 2, "b": "fresh"})).unwrap();
    db.commit().unwrap();
    drop(db);
    let db = Database::open(&path, cfg()).unwrap();
    assert_eq!(db.get(c, "old").unwrap().unwrap().document, json!({"a": 1}));
    assert_eq!(db.get(c, "new").unwrap().unwrap().document, json!({"a": 2, "b": "fresh"}));
}

#[test]
fn a_legacy_file_refuses() {
    FORCE.with(|f| f.set(Some(false)));
    let t = tempfile::tempdir().unwrap();
    let mut db = Database::create(t.path().join("db"), cfg()).unwrap();
    let c = db.create_collection("t", fields(&[("a", Kind::Int)]), Default::default()).unwrap();
    db.commit().unwrap();
    assert!(matches!(db.rename_column(c, "a", "b"), Err(Error::Unsupported(m)) if m.contains("sekejap-upgrade")));
    assert!(matches!(db.drop_column(c, "a"), Err(Error::Unsupported(_))));
}
