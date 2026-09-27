//! `describe()` reports every column's SQL type (found
//! dogfooding under an application, 2026-09-28): exactly as declared for a
//! table created through SQL, and
//! a spelling derived from the stored kind for a table that recorded none --
//! one created through the API, or before the declaration was recorded.

use sekejap::{Db, FieldKind};

fn declared(db: &Db, table: &str) -> Vec<(String, Option<String>)> {
    db.describe(table)
        .unwrap()
        .unwrap()
        .fields
        .into_iter()
        .map(|f| (f.name, f.declared))
        .collect()
}

#[test]
fn every_column_reports_its_declared_type() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("db")).unwrap();
    db.execute(
        "CREATE TABLE place (_key TEXT PRIMARY KEY, name TEXT, visits INT, rating REAL, big BIGINT, open BOOLEAN, extra JSONB, emb VECTOR(3), loc GEOMETRY(Point,4326), opened DATE, at TIMESTAMPTZ)",
        &[],
    )
    .unwrap();
    let got = declared(&db, "place");
    let want: Vec<(&str, &str)> = vec![
        ("_key", "TEXT"),
        ("name", "TEXT"),
        ("visits", "INT"),
        ("rating", "REAL"),
        ("big", "BIGINT"),
        ("open", "BOOLEAN"),
        ("extra", "JSONB"),
        ("emb", "VECTOR(3)"),
        ("loc", "GEOMETRY(Point,4326)"),
        ("opened", "DATE"),
        ("at", "TIMESTAMPTZ"),
    ];
    let got: Vec<(&str, &str)> = got
        .iter()
        .map(|(n, d)| (n.as_str(), d.as_deref().unwrap_or("<none>")))
        .collect();
    assert_eq!(got, want);
}

#[test]
fn a_table_that_recorded_no_type_reports_one_derived_from_its_kind() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("db")).unwrap();
    db.create_collection(
        "raw",
        &[
            ("t", FieldKind::Text),
            ("i", FieldKind::Int),
            ("r", FieldKind::Real),
            ("b", FieldKind::Bool),
            ("v", FieldKind::Vector(4)),
        ],
    )
    .unwrap();
    let got: Vec<(String, Option<String>)> = declared(&db, "raw");
    assert_eq!(
        got,
        vec![
            ("_key".to_owned(), Some("TEXT".to_owned())),
            ("t".to_owned(), Some("TEXT".to_owned())),
            ("i".to_owned(), Some("BIGINT".to_owned())),
            ("r".to_owned(), Some("DOUBLE PRECISION".to_owned())),
            ("b".to_owned(), Some("BOOLEAN".to_owned())),
            ("v".to_owned(), Some("VECTOR(4)".to_owned())),
        ]
    );
}
