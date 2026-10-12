//! Arithmetic between columns in the SELECT list (0.20). A select item that
//! begins with a column followed by `+` or `-` is a row expression, evaluated
//! per returned row (QL_CONTRACT §4.1).
//!
//! The headline case is the one dogfooding reported as a syntax error:
//! subtracting two timestamps. We have no interval type, so `t2 - t1` is a
//! number of SECONDS (a DOUBLE) -- the duration between them.
//!
//! What is at risk, one test each:
//!
//! * two declared timestamps subtract to a number of seconds
//!   (`a_timestamp_difference_is_seconds`);
//! * plain numeric columns add and subtract, an integer pair staying integer
//!   and a real pair giving a real
//!   (`integer_columns_subtract_to_an_integer`, `real_columns_add_to_a_real`);
//! * a NULL operand makes the whole expression NULL
//!   (`a_null_operand_makes_the_difference_null`);
//! * a ranking expression that merely begins with a number is still the
//!   statement's ordering value, not stolen by the new arithmetic path
//!   (`a_leading_number_is_not_mistaken_for_column_arithmetic`).

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

fn rows(db: &mut Database, sql: &str) -> Vec<Vec<SqlValue>> {
    match db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}")) {
        SqlResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("`{sql}` answered {other:?}"),
    }
}

#[test]
fn a_timestamp_difference_is_seconds() {
    let (_dir, mut db) = open();
    run(&mut db, "CREATE TABLE trip (_key TEXT PRIMARY KEY, departed TIMESTAMPTZ, arrived TIMESTAMPTZ)");
    run(
        &mut db,
        "INSERT INTO trip (_key, departed, arrived) VALUES \
         ('t', '2026-10-13T08:00:00Z', '2026-10-13T08:30:00Z')",
    );
    // Half an hour is 1800 seconds.
    assert_eq!(
        rows(&mut db, "SELECT arrived - departed AS seconds FROM trip WHERE _key = 't'"),
        vec![vec![SqlValue::Float(1800.0)]]
    );
}

#[test]
fn integer_columns_subtract_to_an_integer() {
    let (_dir, mut db) = open();
    run(&mut db, "CREATE TABLE stock (_key TEXT PRIMARY KEY, have INT, want INT)");
    run(&mut db, "INSERT INTO stock (_key, have, want) VALUES ('s', 10, 4)");
    assert_eq!(
        rows(&mut db, "SELECT have - want AS spare FROM stock WHERE _key = 's'"),
        vec![vec![SqlValue::Int(6)]]
    );
}

#[test]
fn real_columns_add_to_a_real() {
    let (_dir, mut db) = open();
    run(&mut db, "CREATE TABLE fare (_key TEXT PRIMARY KEY, price REAL, tax REAL)");
    run(&mut db, "INSERT INTO fare (_key, price, tax) VALUES ('f', 12.5, 1.25)");
    assert_eq!(
        rows(&mut db, "SELECT price + tax AS total FROM fare WHERE _key = 'f'"),
        vec![vec![SqlValue::Float(13.75)]]
    );
}

#[test]
fn a_null_operand_makes_the_difference_null() {
    let (_dir, mut db) = open();
    run(&mut db, "CREATE TABLE trip (_key TEXT PRIMARY KEY, departed TIMESTAMPTZ, arrived TIMESTAMPTZ)");
    // `arrived` left NULL.
    run(&mut db, "INSERT INTO trip (_key, departed) VALUES ('t', '2026-10-13T08:00:00Z')");
    assert_eq!(
        rows(&mut db, "SELECT arrived - departed AS seconds FROM trip WHERE _key = 't'"),
        vec![vec![SqlValue::Null]]
    );
}

#[test]
fn a_leading_number_is_not_mistaken_for_column_arithmetic() {
    let (_dir, mut db) = open();
    run(&mut db, "CREATE TABLE note (_key TEXT PRIMARY KEY, score REAL)");
    run(&mut db, "INSERT INTO note (_key, score) VALUES ('a', 0.25), ('b', 0.75)");
    // `1 - score` begins with a number: it is the ordering expression path,
    // which already evaluates arithmetic over scalar names and numbers. It
    // must still parse and answer, not be refused or misrouted.
    let out = rows(&mut db, "SELECT _key FROM note ORDER BY 1 - score DESC");
    assert_eq!(
        out,
        vec![vec![SqlValue::Text("a".into())], vec![SqlValue::Text("b".into())]]
    );
}
