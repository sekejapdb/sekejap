//! A conditional UPDATE on the column it sets -- compare-and-set -- when the
//! WHERE holds an equality on the key (reported 2026-10-12 from dogfooding a
//! database-backed queue: claim a waiting row and move it to active).
//!
//! The planner was choosing the scalar index over the column being SET as the
//! driver, then refusing to move the posting it was standing on. With a key
//! equality in the WHERE it must drive by the key instead: at most one row is
//! visited, so patching an indexed column cannot make the walk meet it twice.
//!
//! What is at risk, one test each:
//!
//! * `UPDATE t SET state = 'active', n = n + 1 WHERE _key = $1 AND state =
//!   'waiting'` succeeds, affects the one row, and a second identical claim
//!   affects none -- the compare-and-set a queue is built on
//!   (`a_compare_and_set_claims_the_row_once`);
//! * the same holds when the only index over the filtered column is the
//!   automatic one, i.e. nothing was dropped to make it work
//!   (`the_set_column_keeps_its_automatic_index`).

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{Param, SqlDatabase, SqlResult, SqlValue};
use tempfile::TempDir;

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn run(db: &mut Database, sql: &str) {
    db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
}

fn affected(db: &mut Database, sql: &str, params: &[Param]) -> u64 {
    match db.sql(sql, params).unwrap_or_else(|e| panic!("`{sql}`: {e}")) {
        SqlResult::Affected(n) => n,
        other => panic!("`{sql}` answered {other:?}"),
    }
}

fn one(db: &mut Database, sql: &str) -> Vec<SqlValue> {
    match db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}")) {
        SqlResult::Rows { mut rows, .. } => rows.remove(0).values,
        other => panic!("`{sql}` answered {other:?}"),
    }
}

fn queue(db: &mut Database) {
    run(db, "CREATE TABLE task (_key TEXT PRIMARY KEY, state TEXT NOT NULL DEFAULT 'waiting', n INT DEFAULT 0)");
    run(db, "INSERT INTO task (_key, state) VALUES ('a', 'waiting')");
}

#[test]
fn a_compare_and_set_claims_the_row_once() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("q"), cfg()).unwrap();
    queue(&mut db);
    let claim = "UPDATE task SET state = 'active', n = n + 1 WHERE _key = $1 AND state = 'waiting'";
    // First caller wins the row.
    assert_eq!(affected(&mut db, claim, &[Param::Text("a".into())]), 1);
    // Every later caller sees it already active and claims nothing.
    assert_eq!(affected(&mut db, claim, &[Param::Text("a".into())]), 0);
    assert_eq!(affected(&mut db, claim, &[Param::Text("a".into())]), 0);
    // The row moved to active exactly once.
    assert_eq!(
        one(&mut db, "SELECT state, n FROM task WHERE _key = 'a'"),
        vec![SqlValue::Text("active".into()), SqlValue::Int(1)]
    );
}

#[test]
fn the_set_column_keeps_its_automatic_index() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("q"), cfg()).unwrap();
    queue(&mut db);
    // The automatic btree over `state` is still there (nothing was dropped),
    // so a predicate on `state` is answerable...
    assert!(matches!(
        db.sql("SELECT _key FROM task WHERE state = 'waiting'", &[]),
        Ok(SqlResult::Rows { .. })
    ));
    // ...and the compare-and-set that filters AND sets `state` still succeeds.
    assert_eq!(
        affected(
            &mut db,
            "UPDATE task SET state = 'active' WHERE _key = $1 AND state = 'waiting'",
            &[Param::Text("a".into())],
        ),
        1
    );
}
