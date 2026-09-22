//! `_key TEXT PRIMARY KEY` names the key every table already has.
//!
//! e1 accepted the spelling and its README taught it, so a table written for
//! e1 has to create here too. It declares nothing new: the column is not
//! stored a second time and no index is built over it, unlike a PRIMARY KEY
//! on a column the caller named, which holds the key twice.

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{SqlDatabase, SqlResult, SqlValue};
use tempfile::TempDir;

fn open() -> (TempDir, Database) {
    let dir = TempDir::new().unwrap();
    let db = Database::create(
        dir.path().join("db"),
        Config {
            budget_bytes: 1 << 20,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )
    .unwrap();
    (dir, db)
}

#[test]
fn declaring_the_builtin_key_creates_the_table_and_keys_rows_by_it() {
    let (_dir, mut db) = open();
    db.sql(
        "CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT, category TEXT)",
        &[],
    )
    .unwrap();
    db.sql(
        "INSERT INTO places (_key, name, category) VALUES ('uluwatu', 'Uluwatu', 'temple')",
        &[],
    )
    .unwrap();
    match db
        .sql("SELECT name FROM places WHERE _key = 'uluwatu'", &[])
        .unwrap()
    {
        SqlResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].values[0], SqlValue::Text("Uluwatu".into()));
        }
        other => panic!("{other:?}"),
    }
    // Not stored twice: the declared fields are the two the caller named.
    let collection = db.collection("places").unwrap().unwrap();
    let fields: Vec<String> = db
        .collection_info(collection)
        .unwrap()
        .layout
        .fields
        .iter()
        .map(|(name, _)| name.clone())
        .filter(|name| !name.starts_with('_'))
        .collect();
    assert_eq!(fields, ["name", "category"]);
}

#[test]
fn every_other_use_of_the_name_is_still_refused() {
    let (_dir, mut db) = open();
    for text in [
        "CREATE TABLE t (_key TEXT, name TEXT)",
        "CREATE TABLE t (_key INT PRIMARY KEY, name TEXT)",
        "CREATE TABLE t (_key TEXT PRIMARY KEY, code TEXT PRIMARY KEY)",
        "CREATE TABLE t (_id TEXT PRIMARY KEY, name TEXT)",
    ] {
        assert!(db.sql(text, &[]).is_err(), "`{text}` was accepted");
    }
}
