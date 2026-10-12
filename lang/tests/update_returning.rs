//! `UPDATE ... RETURNING` (0.20): a write reports the rows it changed, their
//! NEW values, exactly as `INSERT ... RETURNING` reports the rows it wrote.
//! Reported from dogfooding, where a claimant wanted the row it just moved.
//!
//! Both UPDATE forms serve it:
//!
//! * the keyed form (`WHERE _key = ...` with constant SETs) reads the one row
//!   back after the point write;
//! * the predicated form (any WHERE, a row expression in the SET) collects the
//!   rows the filter matches, writes them, and reads each back.
//!
//! What is at risk, one test each:
//!
//! * a keyed UPDATE returns the one changed row with its new values
//!   (`a_keyed_update_returns_the_changed_row`);
//! * a keyed UPDATE of a key that is not there returns no row
//!   (`a_keyed_update_of_a_missing_row_returns_nothing`);
//! * a compare-and-set with a row expression returns the claimed row, and a
//!   second claim returns nothing (`a_compare_and_set_returns_the_claimed_row`);
//! * a predicated UPDATE over a key range returns every changed row
//!   (`a_predicated_update_returns_every_changed_row`);
//! * `RETURNING *` answers the declared columns
//!   (`returning_star_answers_the_columns`);
//! * `DELETE ... RETURNING` is still refused -- returning deleted rows is a
//!   separate slice (`delete_returning_is_refused`).

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{SqlDatabase, SqlResult, SqlValue};
use tempfile::TempDir;

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn open() -> (TempDir, Database) {
    let dir = TempDir::new().unwrap();
    let db = Database::create(dir.path().join("t"), cfg()).unwrap();
    (dir, db)
}

fn run(db: &mut Database, sql: &str) {
    db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
}

fn result(db: &mut Database, sql: &str) -> SqlResult {
    db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"))
}

fn returned(db: &mut Database, sql: &str) -> (Vec<String>, Vec<Vec<SqlValue>>) {
    match result(db, sql) {
        SqlResult::Rows { columns, rows } => {
            (columns, rows.into_iter().map(|r| r.values).collect())
        }
        other => panic!("`{sql}` answered {other:?}"),
    }
}

fn task(db: &mut Database) {
    run(db, "CREATE TABLE task (_key TEXT PRIMARY KEY, state TEXT NOT NULL DEFAULT 'waiting', n INT DEFAULT 0)");
    run(db, "INSERT INTO task (_key, state) VALUES ('a', 'waiting'), ('b', 'waiting'), ('c', 'waiting')");
}

#[test]
fn a_keyed_update_returns_the_changed_row() {
    let (_dir, mut db) = open();
    task(&mut db);
    let (columns, rows) = returned(
        &mut db,
        "UPDATE task SET state = 'done' WHERE _key = 'a' RETURNING _key, state",
    );
    assert_eq!(columns, vec!["_key".to_string(), "state".to_string()]);
    assert_eq!(
        rows,
        vec![vec![SqlValue::Text("a".into()), SqlValue::Text("done".into())]]
    );
}

#[test]
fn a_keyed_update_of_a_missing_row_returns_nothing() {
    let (_dir, mut db) = open();
    task(&mut db);
    let (_columns, rows) = returned(
        &mut db,
        "UPDATE task SET state = 'done' WHERE _key = 'zzz' RETURNING _key, state",
    );
    assert!(rows.is_empty(), "a key that is not there changes no row");
}

#[test]
fn a_compare_and_set_returns_the_claimed_row() {
    let (_dir, mut db) = open();
    task(&mut db);
    // A row expression (n + 1) makes this the predicated form, driven by the
    // key. The claimant gets the row as it now is.
    let claim =
        "UPDATE task SET state = 'active', n = n + 1 WHERE _key = 'a' AND state = 'waiting' RETURNING _key, state, n";
    assert_eq!(
        returned(&mut db, claim).1,
        vec![vec![
            SqlValue::Text("a".into()),
            SqlValue::Text("active".into()),
            SqlValue::Int(1),
        ]]
    );
    // The second claim matches nothing and returns nothing.
    assert!(returned(&mut db, claim).1.is_empty());
}

#[test]
fn a_predicated_update_returns_every_changed_row() {
    let (_dir, mut db) = open();
    task(&mut db);
    // A key range is the predicated form, driven by the key. Every matched
    // row is returned, with its new state.
    let (_columns, mut rows) = returned(
        &mut db,
        "UPDATE task SET state = 'done' WHERE _key >= 'a' AND _key <= 'b' RETURNING _key, state",
    );
    rows.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
    assert_eq!(
        rows,
        vec![
            vec![SqlValue::Text("a".into()), SqlValue::Text("done".into())],
            vec![SqlValue::Text("b".into()), SqlValue::Text("done".into())],
        ]
    );
    // `c` was outside the range and is untouched.
    match result(&mut db, "SELECT state FROM task WHERE _key = 'c'") {
        SqlResult::Rows { rows, .. } => {
            assert_eq!(rows[0].values, vec![SqlValue::Text("waiting".into())])
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn returning_star_answers_the_columns() {
    let (_dir, mut db) = open();
    task(&mut db);
    let (columns, rows) = returned(
        &mut db,
        "UPDATE task SET state = 'done' WHERE _key = 'a' RETURNING *",
    );
    // `*` is the declared columns, as a SELECT * answers them (not `_key`).
    assert_eq!(columns, vec!["state".to_string(), "n".to_string()]);
    assert_eq!(
        rows,
        vec![vec![SqlValue::Text("done".into()), SqlValue::Int(0)]]
    );
}

#[test]
fn delete_returning_is_refused() {
    let (_dir, mut db) = open();
    task(&mut db);
    let err = db
        .sql("DELETE FROM task WHERE _key = 'a' RETURNING _key", &[])
        .unwrap_err();
    assert!(
        format!("{err}").to_ascii_uppercase().contains("RETURNING"),
        "DELETE ... RETURNING is refused by name, got: {err}"
    );
}
