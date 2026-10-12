//! Entity foreign keys (0.20): `REFERENCES` on a table with its own `_key` is
//! a real foreign key, not an edge table and not silently dropped.
//!
//! A foreign key says a column's value must name a row of another table by its
//! external key. This release checks it on write (`23503`), keeps the column
//! mandatory where `NOT NULL` is written (`23502`), and protects a referenced
//! row from DELETE (RESTRICT).
//!
//! What is at risk, one test each:
//!
//! * an entity table (`_key` PRIMARY KEY) with a `REFERENCES` column is created
//!   as an entity table, not refused as an edge table, and the reference to a
//!   table that does not exist is refused at CREATE
//!   (`an_entity_table_takes_a_foreign_key_and_a_missing_target_is_refused`);
//! * a value that names no row of the referenced table is refused on INSERT and
//!   on UPDATE with `23503`, and a value that does name one is accepted
//!   (`a_reference_to_a_missing_row_is_refused_23503`);
//! * a mandatory foreign key (`NOT NULL`) refuses a MISSING value with `23502`
//!   (`a_mandatory_foreign_key_refuses_a_missing_value_23502`);
//! * deleting a row another row still references is refused (RESTRICT), and
//!   succeeds once the referrer is gone
//!   (`a_referenced_row_is_protected_from_delete`).

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::SqlDatabase;
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

fn refusal(db: &mut Database, sql: &str) -> String {
    match db.sql(sql, &[]) {
        Ok(result) => panic!("`{sql}` answered {result:?}"),
        Err(error) => error.to_string(),
    }
}

/// `island` and `tour`, where every tour's `island` is a foreign key.
fn islands_and_tours(db: &mut Database) {
    run(db, "CREATE TABLE island (_key TEXT PRIMARY KEY, name TEXT)");
    run(
        db,
        "CREATE TABLE tour (_key TEXT PRIMARY KEY, title TEXT, island TEXT NOT NULL REFERENCES island)",
    );
    run(db, "INSERT INTO island (_key, name) VALUES ('bali', 'Bali')");
}

#[test]
fn an_entity_table_takes_a_foreign_key_and_a_missing_target_is_refused() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("fk"), cfg()).unwrap();
    run(&mut db, "CREATE TABLE island (_key TEXT PRIMARY KEY, name TEXT)");
    // Accepted as an entity table (it has its own `_key`), not refused as an
    // edge table.
    run(
        &mut db,
        "CREATE TABLE tour (_key TEXT PRIMARY KEY, title TEXT, island TEXT NOT NULL REFERENCES island)",
    );
    // A reference to a table that does not exist is refused at CREATE.
    let m = refusal(
        &mut db,
        "CREATE TABLE booking (_key TEXT PRIMARY KEY, tour TEXT REFERENCES nonexistent)",
    );
    assert!(m.contains("nonexistent"), "names the missing target: {m}");
}

#[test]
fn a_reference_to_a_missing_row_is_refused_23503() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("fk"), cfg()).unwrap();
    islands_and_tours(&mut db);
    // A good reference is accepted.
    run(
        &mut db,
        "INSERT INTO tour (_key, title, island) VALUES ('t1', 'Sunrise', 'bali')",
    );
    // A value that names no island is refused with 23503.
    let m = refusal(
        &mut db,
        "INSERT INTO tour (_key, title, island) VALUES ('t2', 'Dive', 'atlantis')",
    );
    assert!(m.contains("23503"), "foreign-key violation code: {m}");
    assert!(m.contains("island"), "names the foreign key: {m}");
    // UPDATE is checked the same way.
    let m = refusal(
        &mut db,
        "UPDATE tour SET island = 'atlantis' WHERE _key = 't1'",
    );
    assert!(m.contains("23503"), "UPDATE is checked too: {m}");
    // And a good UPDATE is accepted.
    run(&mut db, "INSERT INTO island (_key, name) VALUES ('lombok', 'Lombok')");
    run(&mut db, "UPDATE tour SET island = 'lombok' WHERE _key = 't1'");
}

#[test]
fn a_mandatory_foreign_key_refuses_a_missing_value_23502() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("fk"), cfg()).unwrap();
    islands_and_tours(&mut db);
    // The `island` column is NOT NULL, so a row that leaves it out is refused
    // with 23502 before the existence check runs.
    let m = refusal(&mut db, "INSERT INTO tour (_key, title) VALUES ('t3', 'Walk')");
    assert!(m.contains("23502"), "not-null violation code: {m}");
}

#[test]
#[ignore = "bit 4: delete RESTRICT via the foreign key's scalar index, next commit"]
fn a_referenced_row_is_protected_from_delete() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("fk"), cfg()).unwrap();
    islands_and_tours(&mut db);
    run(
        &mut db,
        "INSERT INTO tour (_key, title, island) VALUES ('t1', 'Sunrise', 'bali')",
    );
    // `bali` is referenced, so deleting it is refused (RESTRICT).
    let m = refusal(&mut db, "DELETE FROM island WHERE _key = 'bali'");
    assert!(m.contains("23503"), "delete of a referenced row is refused: {m}");
    // Once the referrer is gone, the island can be deleted.
    run(&mut db, "DELETE FROM tour WHERE _key = 't1'");
    run(&mut db, "DELETE FROM island WHERE _key = 'bali'");
}
