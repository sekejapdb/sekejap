//! F1 through SQL (`docs/core/SUPPORTIVE.md` 2.c): on a file with column ids,
//! `RENAME COLUMN` and `DROP COLUMN` work on tables that hold rows, and cost
//! one column record, not a rewrite. What is at risk, one test each:
//!
//! * a populated column renamed answers SELECT, WHERE through its index, and
//!   survives reopen (`rename_column_on_a_populated_table`);
//! * a dropped column's values stay gone when the name is added back
//!   (`drop_then_add_the_same_name`);
//! * a column added with a DEFAULT to a table with rows reads the DEFAULT on
//!   those rows, in SELECT, WHERE and through its index, as PostgreSQL shows
//!   it, with no row rewritten (`add_column_default_reaches_existing_rows`);
//! * DEFAULT and NOT NULL change after the column exists, a new NOT NULL
//!   refused while a row holds no value (`defaults_and_not_null_change_later`);
//! * a table moves between schemas, an index and a schema are renamed, and
//!   every one keeps its rows and answers (`names_move_and_rename`);
//! * a bound edge table is dropped with its edges and its name is reused
//!   (`a_bound_edge_table_drops_and_its_name_returns`);
//! * an edge table's property columns change the same way, read through the
//!   table and through MATCH, and its ends stay refused
//!   (`edge_table_columns_change_like_any_table`).

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{SqlDatabase, SqlResult, SqlValue};

fn cfg() -> Config {
    Config { budget_bytes: 1 << 20, io: IoMode::Buffered, sync: SyncMode::Full }
}
fn register_file(path: &std::path::Path) -> Database {
    std::env::set_var("SEKEJAP_CREATE_REGISTER", "1");
    Database::create(path, cfg()).unwrap()
}
fn run(db: &mut Database, sql: &str) -> SqlResult {
    db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"))
}
fn rows(db: &mut Database, sql: &str) -> Vec<Vec<SqlValue>> {
    match run(db, sql) {
        SqlResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn rename_column_on_a_populated_table() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut db = register_file(&path);
    run(&mut db, "CREATE TABLE places (_key TEXT PRIMARY KEY, stars INT, name TEXT)");
    run(&mut db, "CREATE INDEX places_stars ON places (stars)");
    run(&mut db, "INSERT INTO places (_key, stars, name) VALUES ('a', 4, 'temple'), ('b', 5, 'beach')");
    run(&mut db, "ALTER TABLE places RENAME COLUMN stars TO rating");
    assert_eq!(rows(&mut db, "SELECT name FROM places WHERE rating = 5"), vec![vec![SqlValue::Text("beach".into())]]);
    drop(db);
    let mut db = Database::open(&path, cfg()).unwrap();
    assert_eq!(
        rows(&mut db, "SELECT _key, rating FROM places ORDER BY _key"),
        vec![
            vec![SqlValue::Text("a".into()), SqlValue::Int(4)],
            vec![SqlValue::Text("b".into()), SqlValue::Int(5)],
        ]
    );
    assert!(db.sql("SELECT stars FROM places", &[]).is_err(), "the old name is gone");
}

#[test]
fn drop_then_add_the_same_name() {
    let t = tempfile::tempdir().unwrap();
    let mut db = register_file(&t.path().join("db"));
    run(&mut db, "CREATE TABLE notes (_key TEXT PRIMARY KEY, body TEXT, secret TEXT)");
    run(&mut db, "INSERT INTO notes (_key, body, secret) VALUES ('a', 'hi', 'pin')");
    run(&mut db, "ALTER TABLE notes DROP COLUMN secret");
    run(&mut db, "ALTER TABLE notes ADD COLUMN secret TEXT");
    run(&mut db, "INSERT INTO notes (_key, body, secret) VALUES ('b', 'yo', 'new')");
    assert_eq!(
        rows(&mut db, "SELECT _key, secret FROM notes ORDER BY _key"),
        vec![
            vec![SqlValue::Text("a".into()), SqlValue::Missing],
            vec![SqlValue::Text("b".into()), SqlValue::Text("new".into())],
        ]
    );
}

#[test]
fn edge_table_columns_change_like_any_table() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut db = register_file(&path);
    for ddl in [
        "CREATE TABLE place (_key TEXT PRIMARY KEY, name TEXT)",
        "CREATE TABLE road (from_id TEXT REFERENCES place, to_id TEXT REFERENCES place, km INT, note TEXT, PRIMARY KEY (from_id, to_id))",
        "INSERT INTO place (_key, name) VALUES ('ubud', 'Ubud'), ('kuta', 'Kuta'), ('amed', 'Amed')",
        "CREATE PROPERTY GRAPH bali VERTEX TABLES (place) EDGE TABLES (road SOURCE KEY (from_id) REFERENCES place (_key) DESTINATION KEY (to_id) REFERENCES place (_key))",
        "INSERT INTO road VALUES ('ubud', 'kuta', 35, 'busy')",
        "ALTER TABLE road RENAME COLUMN km TO distance",
        "ALTER TABLE road DROP COLUMN note",
        "ALTER TABLE road ADD COLUMN note TEXT",
        "INSERT INTO road VALUES ('ubud', 'amed', 80, 'coast')",
        "ALTER TABLE road ADD COLUMN lanes INT DEFAULT 2",
    ] {
        run(&mut db, ddl);
    }
    db.commit().unwrap();
    drop(db);
    let mut db = Database::open(&path, cfg()).unwrap();
    assert_eq!(
        rows(&mut db, "SELECT to_id, distance, note FROM road WHERE from_id = 'ubud' ORDER BY to_id"),
        vec![
            vec![SqlValue::Text("amed".into()), SqlValue::Int(80), SqlValue::Text("coast".into())],
            vec![SqlValue::Text("kuta".into()), SqlValue::Int(35), SqlValue::Null],
        ]
    );
    assert_eq!(
        rows(&mut db, "SELECT * FROM GRAPH_TABLE (bali MATCH (a IS place)-[r:road]->(b IS place) WHERE b._key = 'kuta' RETURN r.distance AS d)"),
        vec![vec![SqlValue::Int(35)]]
    );
    assert!(db.sql("ALTER TABLE road DROP COLUMN to_id", &[]).is_err(), "an end stays");
    // F2 on edges: the edges already there read the DEFAULT.
    assert_eq!(
        rows(&mut db, "SELECT lanes FROM road WHERE from_id = 'ubud' AND to_id = 'kuta'"),
        vec![vec![SqlValue::Int(2)]]
    );
    // A NOT NULL column with no DEFAULT is false on the edges there.
    assert!(db.sql("ALTER TABLE road ADD COLUMN toll INT NOT NULL", &[]).is_err());
}


#[test]
fn add_column_default_reaches_existing_rows() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut db = register_file(&path);
    run(&mut db, "CREATE TABLE member (_key TEXT PRIMARY KEY, name TEXT)");
    run(&mut db, "INSERT INTO member (_key, name) VALUES ('a', 'Ayu'), ('b', 'Bagus')");
    run(&mut db, "ALTER TABLE member ADD COLUMN status TEXT NOT NULL DEFAULT 'active'");
    run(&mut db, "INSERT INTO member (_key, name, status) VALUES ('c', 'Citra', 'paused')");
    db.commit().unwrap();
    drop(db);
    let mut db = Database::open(&path, cfg()).unwrap();
    assert_eq!(
        rows(&mut db, "SELECT _key, status FROM member ORDER BY _key"),
        vec![
            vec![SqlValue::Text("a".into()), SqlValue::Text("active".into())],
            vec![SqlValue::Text("b".into()), SqlValue::Text("active".into())],
            vec![SqlValue::Text("c".into()), SqlValue::Text("paused".into())],
        ]
    );
    assert_eq!(
        rows(&mut db, "SELECT _key FROM member WHERE status = 'active' ORDER BY _key"),
        vec![vec![SqlValue::Text("a".into())], vec![SqlValue::Text("b".into())]]
    );
}

#[test]
fn defaults_and_not_null_change_later() {
    let t = tempfile::tempdir().unwrap();
    let mut db = register_file(&t.path().join("db"));
    run(&mut db, "CREATE TABLE stay (_key TEXT PRIMARY KEY, nights INT)");
    run(&mut db, "INSERT INTO stay (_key) VALUES ('a')");
    run(&mut db, "ALTER TABLE stay ALTER COLUMN nights SET DEFAULT 2");
    run(&mut db, "INSERT INTO stay (_key) VALUES ('b')");
    assert_eq!(rows(&mut db, "SELECT nights FROM stay WHERE _key = 'b'"), vec![vec![SqlValue::Int(2)]]);
    let refused = db.sql("ALTER TABLE stay ALTER COLUMN nights SET NOT NULL", &[]).unwrap_err().to_string();
    assert!(refused.contains("23502") || refused.contains("null"), "{refused}");
    run(&mut db, "UPDATE stay SET nights = 1 WHERE _key = 'a'");
    run(&mut db, "ALTER TABLE stay ALTER COLUMN nights SET NOT NULL");
    assert!(db.sql("INSERT INTO stay (_key, nights) VALUES ('c', NULL)", &[]).is_err());
    run(&mut db, "ALTER TABLE stay ALTER COLUMN nights DROP NOT NULL");
    run(&mut db, "ALTER TABLE stay ALTER COLUMN nights DROP DEFAULT");
    run(&mut db, "INSERT INTO stay (_key) VALUES ('d')");
    assert_eq!(rows(&mut db, "SELECT nights FROM stay WHERE _key = 'd'"), vec![vec![SqlValue::Missing]]);
}

#[test]
fn names_move_and_rename() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut db = register_file(&path);
    run(&mut db, "CREATE SCHEMA travel");
    run(&mut db, "CREATE TABLE guide (_key TEXT PRIMARY KEY, rating INT)");
    run(&mut db, "INSERT INTO guide (_key, rating) VALUES ('made', 5)");
    run(&mut db, "ALTER TABLE guide SET SCHEMA travel");
    run(&mut db, "ALTER INDEX guide_rating_btree RENAME TO by_rating");
    run(&mut db, "ALTER SCHEMA travel RENAME TO trips");
    db.commit().unwrap();
    drop(db);
    let mut db = Database::open(&path, cfg()).unwrap();
    assert_eq!(rows(&mut db, "SELECT _key FROM trips.guide WHERE rating = 5"), vec![vec![SqlValue::Text("made".into())]]);
    assert!(db.sql("SELECT _key FROM guide", &[]).is_err(), "the old place is empty");
    let c = db.collection_in("trips", "guide").unwrap().unwrap();
    let names: Vec<String> = db.list_indexes(c).unwrap().into_iter().map(|i| i.name).collect();
    assert!(names.contains(&"by_rating".to_owned()), "{names:?}");
}

#[test]
fn a_bound_edge_table_drops_and_its_name_returns() {
    let t = tempfile::tempdir().unwrap();
    let mut db = register_file(&t.path().join("db"));
    for ddl in [
        "CREATE TABLE place (_key TEXT PRIMARY KEY, name TEXT)",
        "CREATE TABLE road (from_id TEXT REFERENCES place, to_id TEXT REFERENCES place, km INT, PRIMARY KEY (from_id, to_id))",
        "INSERT INTO place (_key, name) VALUES ('ubud', 'Ubud'), ('kuta', 'Kuta')",
        "CREATE PROPERTY GRAPH bali VERTEX TABLES (place) EDGE TABLES (road SOURCE KEY (from_id) REFERENCES place (_key) DESTINATION KEY (to_id) REFERENCES place (_key))",
        "INSERT INTO road VALUES ('ubud', 'kuta', 35)",
        "COMMIT",
    ] {
        run(&mut db, ddl);
    }
    run(&mut db, "DROP PROPERTY GRAPH bali");
    run(&mut db, "DROP TABLE road");
    assert!(db.sql("SELECT * FROM road", &[]).is_err(), "the table is gone");
    for ddl in [
        "CREATE TABLE road (from_id TEXT REFERENCES place, to_id TEXT REFERENCES place, lanes INT, PRIMARY KEY (from_id, to_id))",
        "CREATE PROPERTY GRAPH bali VERTEX TABLES (place) EDGE TABLES (road SOURCE KEY (from_id) REFERENCES place (_key) DESTINATION KEY (to_id) REFERENCES place (_key))",
        "INSERT INTO road VALUES ('kuta', 'ubud', 2)",
        "COMMIT",
    ] {
        run(&mut db, ddl);
    }
    assert_eq!(
        rows(&mut db, "SELECT * FROM GRAPH_TABLE (bali MATCH (a IS place)-[r:road]->(b IS place) RETURN a._key AS a, b._key AS b)"),
        vec![vec![SqlValue::Text("kuta".into()), SqlValue::Text("ubud".into())]],
        "only the new table's edge: the dropped table's edge went with it"
    );
}
