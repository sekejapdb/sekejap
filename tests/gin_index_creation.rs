//! `CREATE INDEX ... USING gin` on a table that has no rows yet.
//!
//! `build_gin_index` only registered the index when it found documents to put in
//! it, so creating one on an empty table produced nothing. The write path then
//! asks whether any GIN index exists before marking a field dirty, so no later
//! row built one either: the index stayed absent for the life of the database,
//! in the ordinary order of creating a schema and then loading data.
//!
//! Nothing broke visibly, which is why it lasted. `ILIKE` falls back to a full
//! scan when no index can answer, so every query kept returning the right rows
//! and only the plan was wrong — the opposite of the usual failure, and harder
//! to notice for it.
//!
//! So these go through `gin_ilike`, which asks the index and has no fallback.
//! A test written against SQL `ILIKE` cannot see this bug at all; that is the
//! mistake this file exists to not repeat.

use sekejap::CoreDB;

fn rows(db: &mut CoreDB, range: std::ops::Range<usize>) {
    let payload: Vec<(String, serde_json::Value)> = range
        .map(|i| {
            (
                format!("t/n{i}"),
                serde_json::json!({
                    "_collection": "t",
                    "_key": format!("n{i}"),
                    "body": format!("alpha{i} vine gamma"),
                }),
            )
        })
        .collect();
    db.put_value_bulk(payload).unwrap();
}

#[test]
fn an_index_created_before_the_rows_still_indexes_them() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE t (body TEXT)").unwrap();
    // The schema first, the data after — which is the normal order, and the one
    // that produced no index at all.
    db.execute("CREATE INDEX ON t USING gin (body)").unwrap();
    rows(&mut db, 0..300);

    assert_eq!(
        db.gin_ilike("body", "%vine%", None).len(),
        300,
        "the index was created before the rows and never picked them up"
    );

    // And it survives being written to disk and read back.
    db.compact().unwrap();
    assert_eq!(db.gin_ilike("body", "%vine%", None).len(), 300, "after compact");

    drop(db);
    let db = CoreDB::open(dir.path()).unwrap();
    assert_eq!(db.gin_ilike("body", "%vine%", None).len(), 300, "after reopen");
}

#[test]
fn creating_the_index_after_the_rows_still_works() {
    // The order that always worked, kept so a fix to the other one cannot quietly
    // break this.
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE t (body TEXT)").unwrap();
    rows(&mut db, 0..300);
    db.execute("CREATE INDEX ON t USING gin (body)").unwrap();

    assert_eq!(db.gin_ilike("body", "%vine%", None).len(), 300);
    db.compact().unwrap();
    assert_eq!(db.gin_ilike("body", "%vine%", None).len(), 300, "after compact");
}

#[test]
fn rows_added_after_the_first_fold_reach_the_index() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE t (body TEXT)").unwrap();
    db.execute("CREATE INDEX ON t USING gin (body)").unwrap();
    rows(&mut db, 0..200);
    db.compact().unwrap();

    // Written against a base rather than an empty index.
    rows(&mut db, 200..300);
    assert_eq!(
        db.gin_ilike("body", "%vine%", None).len(),
        300,
        "rows written after the first fold are missing from the index"
    );
    db.compact().unwrap();
    assert_eq!(db.gin_ilike("body", "%vine%", None).len(), 300, "after the second fold");
}
