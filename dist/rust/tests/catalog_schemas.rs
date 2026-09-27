//! The Rust API's catalog sees what SQL sees: tables in named schemas, and
//! edge tables as edges (`docs/dist/RUST_API.md` §5).
//!
//! What is at risk, one test each:
//!
//! * `collections()` lists a table in a named schema, qualified, and a
//!   `public` table by its bare name (`collections_names_every_schema`);
//! * `describe()` and the row calls take `schema.table`, and `describe`
//!   reports the schema (`a_qualified_name_reaches_its_table`);
//! * `describe()` of an edge table reports its ends, label, key and graph,
//!   and no `_key` column; an ordinary table reports none
//!   (`describe_marks_an_edge_table`).

use sekejap::{Db, EdgeTableInfo};
use serde_json::json;

fn open() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("db")).unwrap();
    (dir, db)
}

fn run(db: &Db, sql: &str) {
    db.execute(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
}

#[test]
fn collections_names_every_schema() {
    let (_dir, db) = open();
    run(&db, "CREATE TABLE notes (_key TEXT PRIMARY KEY, body TEXT)");
    run(&db, "CREATE SCHEMA geo");
    run(&db, "CREATE TABLE geo.places (_key TEXT PRIMARY KEY, name TEXT)");
    let names = db.collections().unwrap();
    assert!(names.contains(&"notes".to_owned()), "{names:?}");
    assert!(names.contains(&"geo.places".to_owned()), "{names:?}");
    assert!(!names.contains(&"places".to_owned()), "a schema table is named with its schema: {names:?}");
}

#[test]
fn a_qualified_name_reaches_its_table() {
    let (_dir, db) = open();
    run(&db, "CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT)");
    run(&db, "CREATE SCHEMA geo");
    run(&db, "CREATE TABLE geo.places (_key TEXT PRIMARY KEY, name TEXT)");
    run(&db, "INSERT INTO geo.places (_key, name) VALUES ('p1', 'Tanah Lot')");
    let described = db.describe("geo.places").unwrap().expect("geo.places is described");
    assert_eq!(described.schema, "geo");
    assert_eq!(described.name, "places");
    let public = db.describe("places").unwrap().unwrap();
    assert_eq!(public.schema, "public");
    assert_eq!(db.describe("public.places").unwrap().unwrap().schema, "public");
    assert!(db.describe("nowhere.places").unwrap().is_none());
    // The row calls take the same name.
    assert_eq!(db.get(("geo.places", "p1")).unwrap().unwrap()["name"], json!("Tanah Lot"));
    assert!(db.get(("places", "p1")).unwrap().is_none(), "the public table is another table");
    db.put(("geo.places", "p2"), &json!({"name": "Uluwatu"})).unwrap();
    assert_eq!(db.count_rows("geo.places").unwrap(), 2);
    assert_eq!(db.scan_count_all_rows().unwrap(), 2, "every schema's rows are counted");
    // A collection created through the API with a qualified name lands in
    // its schema.
    db.create_collection("geo.roads", &[("name", sekejap::FieldKind::Text)]).unwrap();
    assert_eq!(db.describe("geo.roads").unwrap().unwrap().schema, "geo");
}

#[test]
fn describe_marks_an_edge_table() {
    let (_dir, db) = open();
    for sql in [
        "CREATE TABLE artist (_key TEXT PRIMARY KEY, name TEXT)",
        "CREATE TABLE song (_key TEXT PRIMARY KEY, title TEXT)",
        "CREATE TABLE wrote (artist_id TEXT REFERENCES artist, song_id TEXT REFERENCES song, year INT, PRIMARY KEY (artist_id, song_id))",
        "CREATE TABLE draft (artist_id TEXT REFERENCES artist, song_id TEXT REFERENCES song)",
        "CREATE PROPERTY GRAPH music VERTEX TABLES (artist, song) EDGE TABLES (wrote SOURCE KEY (artist_id) REFERENCES artist (_key) DESTINATION KEY (song_id) REFERENCES song (_key) LABEL composed)",
    ] {
        run(&db, sql);
    }
    let wrote = db.describe("wrote").unwrap().unwrap();
    assert_eq!(
        wrote.edge,
        Some(EdgeTableInfo {
            references: vec![
                ("artist_id".to_owned(), "artist".to_owned()),
                ("song_id".to_owned(), "song".to_owned())
            ],
            key: vec!["artist_id".to_owned(), "song_id".to_owned()],
            source: Some("artist_id".to_owned()),
            source_table: Some("artist".to_owned()),
            destination: Some("song_id".to_owned()),
            destination_table: Some("song".to_owned()),
            label: Some("composed".to_owned()),
            graph: Some("music".to_owned()),
        })
    );
    let names: Vec<&str> = wrote.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["artist_id", "song_id", "year"], "an edge table has no _key");
    // Declared, no property graph yet: its references and no ends.
    let draft = db.describe("draft").unwrap().unwrap().edge.unwrap();
    assert_eq!(draft.source, None);
    assert_eq!(draft.label, None);
    assert_eq!(draft.references.len(), 2);
    // An ordinary table is not an edge table.
    assert!(db.describe("artist").unwrap().unwrap().edge.is_none());
}
