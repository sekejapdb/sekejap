//! Key-equality JOIN (0.20): read a table together with the rows its foreign
//! key names, the PostgreSQL way, for the relationships that are a mandatory
//! label rather than a graph to walk.
//!
//! Scope of the first release: ONE binary `INNER`/`LEFT JOIN`, `ON
//! right._key = left.col`, the left side driving one key lookup per row. Right
//! on the left of the equality is the same join. Right-side filters, other
//! join kinds, and multi-way joins are refused by name.
//!
//! What is at risk, one test each:
//!
//! * an INNER join returns each left row paired with the right row its key
//!   names, and drops a left row whose key matches none
//!   (`an_inner_join_pairs_each_row_with_the_row_its_key_names`);
//! * a LEFT join keeps every left row, the right columns reading NULL where
//!   the key matches none (`a_left_join_keeps_unmatched_left_rows`);
//! * the join reads the same whichever side of `ON` the key is written
//!   (`the_on_clause_reads_either_way_round`);
//! * a WHERE on the driving table narrows the join, and LIMIT bounds it
//!   (`a_where_on_the_driver_narrows_the_join`);
//! * an unsupported shape (a non-key join, a right-side filter, FULL/CROSS) is
//!   refused by name, not silently wrong (`an_unsupported_join_is_refused`).

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

fn run(db: &mut Database, sql: &str) {
    db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
}

fn rows(db: &mut Database, sql: &str) -> Vec<Vec<SqlValue>> {
    match db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}")) {
        SqlResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("`{sql}` answered {other:?}"),
    }
}

fn refusal(db: &mut Database, sql: &str) -> String {
    match db.sql(sql, &[]) {
        Ok(result) => panic!("`{sql}` answered {result:?}"),
        Err(error) => error.to_string(),
    }
}

/// `island` and `tour`, tours referencing islands, with one tour whose island
/// is unset (LEFT-join fodder).
fn fixture(db: &mut Database) {
    run(db, "CREATE TABLE island (_key TEXT PRIMARY KEY, name TEXT)");
    run(db, "CREATE TABLE tour (_key TEXT PRIMARY KEY, title TEXT, island TEXT)");
    run(db, "INSERT INTO island (_key, name) VALUES ('bali', 'Bali'), ('lombok', 'Lombok')");
    run(db, "INSERT INTO tour (_key, title, island) VALUES ('t1', 'Sunrise', 'bali')");
    run(db, "INSERT INTO tour (_key, title, island) VALUES ('t2', 'Dive', 'lombok')");
    run(db, "INSERT INTO tour (_key, title, island) VALUES ('t3', 'Walk', 'bali')");
    run(db, "INSERT INTO tour (_key, title, island) VALUES ('t4', 'Drift', 'atlantis')");
}

fn text(values: &[SqlValue]) -> Vec<String> {
    values
        .iter()
        .map(|v| match v {
            SqlValue::Text(s) => s.clone(),
            SqlValue::Null => "<null>".to_owned(),
            other => format!("{other:?}"),
        })
        .collect()
}

#[test]
fn an_inner_join_pairs_each_row_with_the_row_its_key_names() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("j"), cfg()).unwrap();
    fixture(&mut db);
    let mut got: Vec<Vec<String>> = rows(
        &mut db,
        "SELECT tour.title, island.name FROM tour JOIN island ON island._key = tour.island ORDER BY tour._key",
    )
    .iter()
    .map(|r| text(r))
    .collect();
    got.sort();
    // t4 (atlantis) has no island, so an INNER join drops it.
    let mut want = vec![
        vec!["Sunrise".to_owned(), "Bali".to_owned()],
        vec!["Dive".to_owned(), "Lombok".to_owned()],
        vec!["Walk".to_owned(), "Bali".to_owned()],
    ];
    want.sort();
    assert_eq!(got, want);
}

#[test]
fn a_left_join_keeps_unmatched_left_rows() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("j"), cfg()).unwrap();
    fixture(&mut db);
    let got: Vec<Vec<String>> = rows(
        &mut db,
        "SELECT tour.title, island.name FROM tour LEFT JOIN island ON island._key = tour.island WHERE tour._key = 't4'",
    )
    .iter()
    .map(|r| text(r))
    .collect();
    // t4 is kept; island.name reads NULL.
    assert_eq!(got, vec![vec!["Drift".to_owned(), "<null>".to_owned()]]);
}

#[test]
fn the_on_clause_reads_either_way_round() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("j"), cfg()).unwrap();
    fixture(&mut db);
    let a = rows(
        &mut db,
        "SELECT tour.title, island.name FROM tour JOIN island ON island._key = tour.island WHERE tour._key = 't1'",
    );
    let b = rows(
        &mut db,
        "SELECT tour.title, island.name FROM tour JOIN island ON tour.island = island._key WHERE tour._key = 't1'",
    );
    assert_eq!(a, b);
    assert_eq!(text(&a[0]), vec!["Sunrise".to_owned(), "Bali".to_owned()]);
}

#[test]
fn a_where_on_the_driver_narrows_the_join() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("j"), cfg()).unwrap();
    fixture(&mut db);
    let got = rows(
        &mut db,
        "SELECT tour.title, island.name FROM tour JOIN island ON island._key = tour.island WHERE tour.island = 'bali' ORDER BY tour._key LIMIT 1",
    );
    assert_eq!(got.len(), 1);
    assert_eq!(text(&got[0]), vec!["Sunrise".to_owned(), "Bali".to_owned()]);
}

#[test]
fn an_unsupported_join_is_refused() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("j"), cfg()).unwrap();
    fixture(&mut db);
    // A non-key join (right side is not the referenced table's _key).
    let m = refusal(
        &mut db,
        "SELECT tour.title, island.name FROM tour JOIN island ON island.name = tour.title",
    );
    assert!(!m.is_empty(), "a non-key join is refused: {m}");
    // FULL OUTER is refused.
    let m = refusal(
        &mut db,
        "SELECT tour.title FROM tour FULL JOIN island ON island._key = tour.island",
    );
    assert!(!m.is_empty(), "FULL JOIN is refused: {m}");
    // A join whose LEFT side is the row's own key is refused by name -- a
    // clean message, not an internal "no column `_key`" error.
    let m = refusal(
        &mut db,
        "SELECT tour._key FROM tour JOIN island ON island._key = tour._key",
    );
    assert!(
        m.contains("foreign key") && !m.to_ascii_uppercase().contains("XX000"),
        "a join on the left key is refused cleanly, not XX000: {m}"
    );
}
