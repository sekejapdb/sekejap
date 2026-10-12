//! `INSERT ... SELECT`: read rows with a SELECT and write them into a table
//! (0.20). The target columns are named and map BY POSITION to the SELECT's
//! output columns, as in PostgreSQL -- `INSERT INTO t (a, b) SELECT x, y FROM
//! s` writes `s.x` into `t.a` and `s.y` into `t.b`.
//!
//! What is at risk, one test each:
//!
//! * the matched rows, and only those, land in the target with their values in
//!   the named columns (`a_select_copies_the_rows_it_matches`);
//! * a SELECT whose column count differs from the target column list is refused
//!   while the statement compiles, so nothing is written
//!   (`a_column_count_mismatch_is_refused`);
//! * a target whose key column is not among the named columns mints one key per
//!   row from the key's DEFAULT (`a_minted_key_is_filled_per_row`);
//! * a declared TIMESTAMPTZ column round-trips: the SELECT prints the stored
//!   instant as ISO text and the INSERT reads it back to the same instant
//!   (`a_timestamp_column_round_trips`);
//! * a target column whose value cannot round-trip through a SELECT yet (a
//!   GEOMETRY/VECTOR) is refused by name rather than stored wrong
//!   (`a_geometry_target_is_refused_for_now`).

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

fn affected(db: &mut Database, sql: &str) -> u64 {
    match db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}")) {
        SqlResult::Affected(n) => n,
        other => panic!("`{sql}` answered {other:?}"),
    }
}

fn rows(db: &mut Database, sql: &str) -> Vec<Vec<SqlValue>> {
    match db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}")) {
        SqlResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("`{sql}` answered {other:?}"),
    }
}

fn islands(db: &mut Database) {
    run(db, "CREATE TABLE island (_key TEXT PRIMARY KEY, name TEXT NOT NULL, visitors INT)");
    run(
        db,
        "INSERT INTO island (_key, name, visitors) VALUES \
         ('bali', 'Bali', 1000), ('lombok', 'Lombok', 200), ('gili', 'Gili', 50)",
    );
}

#[test]
fn a_select_copies_the_rows_it_matches() {
    let (_dir, mut db) = open();
    islands(&mut db);
    run(&mut db, "CREATE TABLE popular (_key TEXT PRIMARY KEY, name TEXT, visitors INT)");
    // Two islands clear the bar; the third does not.
    assert_eq!(
        affected(
            &mut db,
            "INSERT INTO popular (_key, name, visitors) \
             SELECT _key, name, visitors FROM island WHERE visitors >= 200",
        ),
        2
    );
    assert_eq!(
        rows(&mut db, "SELECT name, visitors FROM popular ORDER BY visitors DESC"),
        vec![
            vec![SqlValue::Text("Bali".into()), SqlValue::Int(1000)],
            vec![SqlValue::Text("Lombok".into()), SqlValue::Int(200)],
        ]
    );
}

#[test]
fn a_column_count_mismatch_is_refused() {
    let (_dir, mut db) = open();
    islands(&mut db);
    run(&mut db, "CREATE TABLE popular (_key TEXT PRIMARY KEY, name TEXT, visitors INT)");
    // Two target columns, three SELECT columns.
    let err = db
        .sql(
            "INSERT INTO popular (_key, name) SELECT _key, name, visitors FROM island",
            &[],
        )
        .unwrap_err();
    assert!(
        format!("{err}").contains("42601"),
        "a column-count mismatch is PostgreSQL's 42601, got: {err}"
    );
    // Nothing was written.
    assert_eq!(rows(&mut db, "SELECT _key FROM popular").len(), 0);
}

#[test]
fn a_minted_key_is_filled_per_row() {
    let (_dir, mut db) = open();
    islands(&mut db);
    run(&mut db, "CREATE TABLE tally (_key TEXT PRIMARY KEY DEFAULT ulid(), name TEXT)");
    // The key is not among the named columns, so each row mints its own.
    assert_eq!(
        affected(&mut db, "INSERT INTO tally (name) SELECT name FROM island"),
        3
    );
    assert_eq!(
        rows(&mut db, "SELECT name FROM tally ORDER BY name"),
        vec![
            vec![SqlValue::Text("Bali".into())],
            vec![SqlValue::Text("Gili".into())],
            vec![SqlValue::Text("Lombok".into())],
        ]
    );
    // Three distinct keys were minted.
    let keys = rows(&mut db, "SELECT _key FROM tally");
    assert_eq!(keys.len(), 3);
    let mut keys: Vec<_> = keys.into_iter().map(|r| r[0].clone()).collect();
    keys.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    keys.dedup();
    assert_eq!(keys.len(), 3, "each minted key is distinct");
}

#[test]
fn a_timestamp_column_round_trips() {
    let (_dir, mut db) = open();
    run(&mut db, "CREATE TABLE arrival (_key TEXT PRIMARY KEY, at TIMESTAMPTZ)");
    run(
        &mut db,
        "INSERT INTO arrival (_key, at) VALUES ('a', '2026-10-13T08:30:00Z')",
    );
    run(&mut db, "CREATE TABLE archive (_key TEXT PRIMARY KEY, at TIMESTAMPTZ)");
    assert_eq!(
        affected(
            &mut db,
            "INSERT INTO archive (_key, at) SELECT _key, at FROM arrival",
        ),
        1
    );
    // The archived instant equals the original, printed as ISO text.
    assert_eq!(
        rows(&mut db, "SELECT at FROM archive WHERE _key = 'a'"),
        rows(&mut db, "SELECT at FROM arrival WHERE _key = 'a'"),
    );
    assert_eq!(
        rows(&mut db, "SELECT at FROM archive WHERE _key = 'a'"),
        vec![vec![SqlValue::Text("2026-10-13T08:30:00Z".into())]],
    );
}

#[test]
fn a_geometry_target_is_refused_for_now() {
    let (_dir, mut db) = open();
    run(&mut db, "CREATE TABLE waypoint (_key TEXT PRIMARY KEY, spot GEOMETRY(Point, 4326))");
    run(
        &mut db,
        "INSERT INTO waypoint (_key, spot) VALUES ('w', ST_SetSRID(ST_MakePoint(115.0, -8.5), 4326))",
    );
    run(&mut db, "CREATE TABLE copy (_key TEXT PRIMARY KEY, spot GEOMETRY(Point, 4326))");
    let err = db
        .sql(
            "INSERT INTO copy (_key, spot) SELECT _key, spot FROM waypoint",
            &[],
        )
        .unwrap_err();
    let text = format!("{err}");
    assert!(
        text.contains("spot") && text.to_ascii_uppercase().contains("GEOMETRY"),
        "a geometry target is refused by name, got: {err}"
    );
}
