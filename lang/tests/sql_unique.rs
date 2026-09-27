//! A key or a value that must be unique, as PostgreSQL enforces it: a plain
//! `INSERT` never overwrites, `ON CONFLICT` is the upsert, and a `UNIQUE`
//! column refuses a second equal value -- each with `23505`.
//!
//! What is at risk, one test each:
//!
//! * an INSERT of a key that is taken is 23505 and leaves the row as it was,
//!   also inside one multi-row INSERT
//!   (`an_insert_of_a_taken_key_is_refused_and_changes_nothing`);
//! * ON CONFLICT (_key) DO NOTHING and DO UPDATE SET c = EXCLUDED.c
//!   (`on_conflict_is_the_upsert_for_rows`);
//! * a UNIQUE column refuses an equal value on INSERT and on UPDATE, admits
//!   any number of NULLs and missing values, and lets a row keep its own
//!   value (`a_unique_column_refuses_a_second_equal_value`);
//! * the table constraint UNIQUE (col) and ALTER TABLE ADD CONSTRAINT ...
//!   UNIQUE, which refuses data that already breaks it
//!   (`unique_as_a_table_constraint_and_added_later`);
//! * what has no unique index is refused by name
//!   (`what_cannot_be_unique_is_refused_by_name`).

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{SqlDatabase, SqlError, SqlResult, SqlValue};
use tempfile::TempDir;

fn db(dir: &TempDir) -> Database {
    Database::create(
        dir.path().join("accounts.sekejap"),
        Config {
            budget_bytes: 1 << 20,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )
    .unwrap()
}

fn run(db: &mut Database, sql: &str) -> SqlResult {
    db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"))
}

fn one(db: &mut Database, sql: &str) -> Vec<Vec<SqlValue>> {
    match run(db, sql) {
        SqlResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("`{sql}` answered {other:?}"),
    }
}

fn sqlstate(result: Result<SqlResult, SqlError>) -> &'static str {
    match result {
        Err(SqlError::Coded { sqlstate, .. }) => sqlstate,
        other => panic!("a coded error, not {other:?}"),
    }
}

fn refused(result: Result<SqlResult, SqlError>, says: &str) {
    match result {
        Err(e) => assert!(e.to_string().contains(says), "`{e}` does not say `{says}`"),
        Ok(r) => panic!("refused, not {r:?}"),
    }
}

fn text(s: &str) -> SqlValue {
    SqlValue::Text(s.into())
}

#[test]
fn an_insert_of_a_taken_key_is_refused_and_changes_nothing() {
    let dir = TempDir::new().unwrap();
    let mut db = db(&dir);
    run(&mut db, "CREATE TABLE account (_key TEXT PRIMARY KEY, name TEXT)");
    run(&mut db, "INSERT INTO account (_key, name) VALUES ('u1', 'Ayu')");
    run(&mut db, "COMMIT");
    assert_eq!(
        sqlstate(db.sql("INSERT INTO account (_key, name) VALUES ('u1', 'Someone else')", &[])),
        "23505"
    );
    run(&mut db, "ROLLBACK");
    assert_eq!(
        sqlstate(db.sql("INSERT INTO account (_key, name) VALUES ('u2', 'Bayu'), ('u2', 'Bayu again')", &[])),
        "23505"
    );
    run(&mut db, "ROLLBACK");
    assert_eq!(one(&mut db, "SELECT name FROM account WHERE _key = 'u1'"), [[text("Ayu")]]);
    assert!(one(&mut db, "SELECT name FROM account WHERE _key = 'u2'").is_empty());
}

#[test]
fn on_conflict_is_the_upsert_for_rows() {
    let dir = TempDir::new().unwrap();
    let mut db = db(&dir);
    run(&mut db, "CREATE TABLE account (_key TEXT PRIMARY KEY, name TEXT, city TEXT)");
    run(&mut db, "INSERT INTO account (_key, name, city) VALUES ('u1', 'Ayu', 'Ubud')");
    run(
        &mut db,
        "INSERT INTO account (_key, name, city) VALUES ('u1', 'Other', 'Other') ON CONFLICT (_key) DO NOTHING",
    );
    assert_eq!(one(&mut db, "SELECT name, city FROM account WHERE _key = 'u1'"), [[text("Ayu"), text("Ubud")]]);
    run(
        &mut db,
        "INSERT INTO account (_key, name, city) VALUES ('u1', 'Ayu', 'Denpasar') ON CONFLICT (_key) DO UPDATE SET city = EXCLUDED.city",
    );
    run(
        &mut db,
        "INSERT INTO account (_key, name, city) VALUES ('u2', 'Bayu', 'Kuta') ON CONFLICT (_key) DO UPDATE SET city = EXCLUDED.city",
    );
    run(&mut db, "COMMIT");
    assert_eq!(one(&mut db, "SELECT name, city FROM account WHERE _key = 'u1'"), [[text("Ayu"), text("Denpasar")]]);
    assert_eq!(one(&mut db, "SELECT name FROM account WHERE _key = 'u2'"), [[text("Bayu")]]);
    refused(
        db.sql("INSERT INTO account (_key, name) VALUES ('u3', 'x') ON CONFLICT (name) DO NOTHING", &[]),
        "ON CONFLICT",
    );
}

#[test]
fn a_unique_column_refuses_a_second_equal_value() {
    let dir = TempDir::new().unwrap();
    let mut db = db(&dir);
    run(&mut db, "CREATE TABLE account (_key TEXT PRIMARY KEY, email TEXT UNIQUE, name TEXT)");
    run(&mut db, "INSERT INTO account (_key, email, name) VALUES ('u1', 'ayu@example.com', 'Ayu')");
    run(&mut db, "COMMIT");
    assert_eq!(
        sqlstate(db.sql("INSERT INTO account (_key, email, name) VALUES ('u2', 'ayu@example.com', 'Bayu')", &[])),
        "23505"
    );
    run(&mut db, "ROLLBACK");
    run(&mut db, "INSERT INTO account (_key, email, name) VALUES ('u2', 'bayu@example.com', 'Bayu')");
    run(&mut db, "COMMIT");
    assert_eq!(
        sqlstate(db.sql("UPDATE account SET email = 'ayu@example.com' WHERE _key = 'u2'", &[])),
        "23505"
    );
    run(&mut db, "ROLLBACK");
    // A row keeps its own value, and NULL or missing values never collide.
    run(&mut db, "UPDATE account SET email = 'ayu@example.com', name = 'Ayu P' WHERE _key = 'u1'");
    run(&mut db, "INSERT INTO account (_key, name) VALUES ('u3', 'no email yet')");
    run(&mut db, "INSERT INTO account (_key, name) VALUES ('u4', 'nor this one')");
    run(&mut db, "INSERT INTO account (_key, email, name) VALUES ('u5', NULL, 'written null')");
    run(&mut db, "INSERT INTO account (_key, email, name) VALUES ('u6', NULL, 'written null too')");
    run(&mut db, "COMMIT");
    assert_eq!(one(&mut db, "SELECT _key FROM account WHERE email = 'ayu@example.com'"), [[text("u1")]]);
}

#[test]
fn unique_as_a_table_constraint_and_added_later() {
    let dir = TempDir::new().unwrap();
    let mut db = db(&dir);
    run(&mut db, "CREATE TABLE member (_key TEXT PRIMARY KEY, handle TEXT, CONSTRAINT member_handle_key UNIQUE (handle)) WITH (index: none)");
    run(&mut db, "INSERT INTO member (_key, handle) VALUES ('m1', 'ayu')");
    assert_eq!(sqlstate(db.sql("INSERT INTO member (_key, handle) VALUES ('m2', 'ayu')", &[])), "23505");
    run(&mut db, "ROLLBACK");

    run(&mut db, "CREATE TABLE guest (_key TEXT PRIMARY KEY, phone TEXT)");
    run(&mut db, "INSERT INTO guest (_key, phone) VALUES ('g1', '555-0100'), ('g2', '555-0100')");
    run(&mut db, "COMMIT");
    assert_eq!(
        sqlstate(db.sql("ALTER TABLE guest ADD CONSTRAINT guest_phone_key UNIQUE (phone)", &[])),
        "23505",
        "the data already breaks it"
    );
    run(&mut db, "ROLLBACK");
    run(&mut db, "UPDATE guest SET phone = '555-0101' WHERE _key = 'g2'");
    run(&mut db, "COMMIT");
    run(&mut db, "ALTER TABLE guest ADD UNIQUE (phone)");
    assert_eq!(sqlstate(db.sql("INSERT INTO guest (_key, phone) VALUES ('g3', '555-0101')", &[])), "23505");
    run(&mut db, "ROLLBACK");
    // CREATE UNIQUE INDEX carries the same code.
    run(&mut db, "CREATE TABLE badge (_key TEXT PRIMARY KEY, code TEXT) WITH (index: none)");
    run(&mut db, "CREATE UNIQUE INDEX badge_code ON badge (code)");
    run(&mut db, "INSERT INTO badge (_key, code) VALUES ('b1', 'X1')");
    assert_eq!(sqlstate(db.sql("INSERT INTO badge (_key, code) VALUES ('b2', 'X1')", &[])), "23505");
    run(&mut db, "ROLLBACK");
    // The spelled-out method once parsed UNIQUE and built a non-unique
    // index; the word is kept now.
    run(&mut db, "CREATE TABLE pass (_key TEXT PRIMARY KEY, code TEXT) WITH (index: none)");
    run(&mut db, "CREATE UNIQUE INDEX pass_code ON pass USING btree (code)");
    run(&mut db, "INSERT INTO pass (_key, code) VALUES ('p1', 'Y1')");
    assert_eq!(sqlstate(db.sql("INSERT INTO pass (_key, code) VALUES ('p2', 'Y1')", &[])), "23505");
}

#[test]
fn what_cannot_be_unique_is_refused_by_name() {
    let dir = TempDir::new().unwrap();
    let mut db = db(&dir);
    refused(
        db.sql("CREATE TABLE pair (_key TEXT PRIMARY KEY, a TEXT, b TEXT, UNIQUE (a, b))", &[]),
        "one column",
    );
    refused(
        db.sql("CREATE TABLE shape (_key TEXT PRIMARY KEY, emb VECTOR(3) UNIQUE)", &[]),
        "UNIQUE",
    );
}
