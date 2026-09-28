//! The row key, as a table declares it (from dogfooding under an
//! application, 2026-09-28).
//!
//! What is at risk, one test each:
//!
//! * an INSERT that names no key is refused with PostgreSQL's 23502, rather
//!   than taking the first column as the key; a named PRIMARY KEY column
//!   supplies the key (`the_key_comes_from_the_declared_primary_key`);
//! * `DEFAULT ulid()` / `uuid4()` on the key mints one per row, for the
//!   built-in `_key` and for a named key column, and survives a reopen
//!   (`a_key_default_mints_one_per_row`);
//! * `ORDER BY _key [DESC]` walks the rows in key order with no index,
//!   beside a filter and under a LIMIT (`order_by_key_needs_no_index`);
//! * a keyset page (`WHERE _key < $last ORDER BY _key DESC LIMIT n`) starts
//!   after the key the previous page ended on
//!   (`keyset_pages_walk_the_key_in_both_directions`).

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{SqlDatabase, SqlError, SqlResult, SqlValue};
use tempfile::TempDir;

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn run(db: &mut Database, sql: &str) -> SqlResult {
    db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"))
}

fn texts(db: &mut Database, sql: &str) -> Vec<String> {
    match run(db, sql) {
        SqlResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match &r.values[0] {
                SqlValue::Text(t) => t.clone(),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("`{sql}` answered {other:?}"),
    }
}

fn sqlstate(result: Result<SqlResult, SqlError>) -> &'static str {
    match result {
        Err(SqlError::Coded { sqlstate, .. }) => sqlstate,
        other => panic!("a coded error, not {other:?}"),
    }
}

#[test]
fn the_key_comes_from_the_declared_primary_key() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("k.sekejap"), cfg()).unwrap();
    run(&mut db, "CREATE TABLE posts (_key TEXT PRIMARY KEY, title TEXT, slug TEXT)");
    assert_eq!(
        sqlstate(db.sql("INSERT INTO posts (title, slug) VALUES ('Hello', 'hello')", &[])),
        "23502",
        "the title must not silently become the key"
    );
    run(&mut db, "ROLLBACK");
    run(&mut db, "CREATE TABLE people (id TEXT PRIMARY KEY, name TEXT)");
    run(&mut db, "INSERT INTO people (name, id) VALUES ('Ayu', 'p1')");
    assert_eq!(texts(&mut db, "SELECT name FROM people WHERE _key = 'p1'"), ["Ayu"]);
    assert_eq!(texts(&mut db, "SELECT id FROM people WHERE _key = 'p1'"), ["p1"]);
    assert_eq!(sqlstate(db.sql("INSERT INTO people (name) VALUES ('Bayu')", &[])), "23502");
}

#[test]
fn a_key_default_mints_one_per_row() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("k.sekejap");
    {
        let mut db = Database::create(&path, cfg()).unwrap();
        run(&mut db, "CREATE TABLE notes (_key TEXT PRIMARY KEY DEFAULT ulid(), body TEXT)");
        run(&mut db, "CREATE TABLE tags (id TEXT PRIMARY KEY DEFAULT uuid4(), label TEXT)");
        run(&mut db, "INSERT INTO notes (body) VALUES ('one'), ('two')");
        run(&mut db, "INSERT INTO tags (label) VALUES ('red')");
        run(&mut db, "COMMIT");
    }
    let mut db = Database::open(&path, cfg()).unwrap();
    run(&mut db, "INSERT INTO notes (body) VALUES ('three')");
    run(&mut db, "COMMIT");
    let keys = texts(&mut db, "SELECT _key FROM notes ORDER BY _key");
    assert_eq!(keys.len(), 3);
    assert!(keys.iter().all(|k| k.len() == 26), "a ULID is 26 characters: {keys:?}");
    let mut unique = keys.clone();
    unique.dedup();
    assert_eq!(unique.len(), 3, "one key per row");
    // A named key column holds the minted key too.
    let tag = texts(&mut db, "SELECT _key FROM tags ORDER BY _key").remove(0);
    assert_eq!(tag.len(), 36, "a UUID is 36 characters: {tag}");
    assert_eq!(texts(&mut db, &format!("SELECT id FROM tags WHERE _key = '{tag}'")), [tag]);
    // A key written explicitly still wins over the default.
    run(&mut db, "INSERT INTO notes (_key, body) VALUES ('mine', 'four')");
    assert_eq!(texts(&mut db, "SELECT body FROM notes WHERE _key = 'mine'"), ["four"]);
}

#[test]
fn order_by_key_needs_no_index() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("k.sekejap"), cfg()).unwrap();
    run(&mut db, "CREATE TABLE users (_key TEXT PRIMARY KEY, name TEXT, city TEXT) WITH (index: [city])");
    run(
        &mut db,
        "INSERT INTO users (_key, name, city) VALUES ('u3', 'Citra', 'Ubud'), ('u1', 'Ayu', 'Ubud'), ('u2', 'Bayu', 'Kuta'), ('u4', 'Dewi', 'Ubud')",
    );
    run(&mut db, "COMMIT");
    assert_eq!(texts(&mut db, "SELECT _key FROM users ORDER BY _key"), ["u1", "u2", "u3", "u4"]);
    assert_eq!(texts(&mut db, "SELECT _key FROM users ORDER BY _key LIMIT 2"), ["u1", "u2"]);
    assert_eq!(
        texts(&mut db, "SELECT _key FROM users WHERE city = 'Ubud' ORDER BY _key"),
        ["u1", "u3", "u4"]
    );
    // DESC walks the same mapping backwards: newest first, for ULID keys.
    assert_eq!(texts(&mut db, "SELECT _key FROM users ORDER BY _key DESC"), ["u4", "u3", "u2", "u1"]);
    assert_eq!(texts(&mut db, "SELECT _key FROM users ORDER BY _key DESC LIMIT 2"), ["u4", "u3"]);
    assert_eq!(
        texts(&mut db, "SELECT _key FROM users WHERE city = 'Ubud' ORDER BY _key DESC"),
        ["u4", "u3", "u1"]
    );
}

/// Keyset paging, the answer to OFFSET: the next page starts after the last
/// key the previous page showed, in either direction.
#[test]
fn keyset_pages_walk_the_key_in_both_directions() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("k.sekejap"), cfg()).unwrap();
    run(&mut db, "CREATE TABLE users (_key TEXT PRIMARY KEY, name TEXT)");
    run(
        &mut db,
        "INSERT INTO users (_key, name) VALUES ('u1', 'a'), ('u2', 'b'), ('u3', 'c'), ('u4', 'd'), ('u5', 'e')",
    );
    run(&mut db, "COMMIT");
    assert_eq!(
        texts(&mut db, "SELECT _key FROM users WHERE _key < 'u4' ORDER BY _key DESC LIMIT 2"),
        ["u3", "u2"]
    );
    assert_eq!(
        texts(&mut db, "SELECT _key FROM users WHERE _key <= 'u4' ORDER BY _key DESC LIMIT 2"),
        ["u4", "u3"]
    );
    assert_eq!(
        texts(&mut db, "SELECT _key FROM users WHERE _key > 'u2' ORDER BY _key LIMIT 2"),
        ["u3", "u4"]
    );
    assert_eq!(
        texts(&mut db, "SELECT _key FROM users WHERE _key > 'u1' AND _key < 'u5' ORDER BY _key DESC"),
        ["u4", "u3", "u2"]
    );
    // The last page of a DESC walk ends at the smallest key.
    assert_eq!(texts(&mut db, "SELECT _key FROM users WHERE _key < 'u2' ORDER BY _key DESC LIMIT 2"), ["u1"]);
}
