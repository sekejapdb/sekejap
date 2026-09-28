//! What an application page needs from the API (found dogfooding under an
//! application, 2026-09-28): the key an INSERT minted, newest-first lists,
//! and a table description complete enough to build the table again.
//!
//! What is at risk, one test each:
//!
//! * `Db::query` answers `INSERT ... RETURNING` and commits it, in both
//!   modes; `Db::execute` counts its rows; inside `Tx::query` the rollback
//!   decides (`insert_returning_through_the_api_in_both_modes`);
//! * `ORDER BY _key DESC` streamed in small pages is every row once,
//!   newest first -- the continuation walks backwards too
//!   (`a_descending_key_walk_pages_without_losing_or_repeating_a_row`);
//! * `describe()` reports NOT NULL and the DEFAULT of every column, the key
//!   included (`describe_reports_not_null_and_defaults`).

use sekejap::{Db, SqlValue};
use serde_json::json;

fn setup(db: &Db) {
    db.execute(
        "CREATE TABLE members (_key TEXT PRIMARY KEY DEFAULT ulid(), email TEXT NOT NULL, joined TIMESTAMPTZ DEFAULT now())",
        &[],
    )
    .unwrap();
}

fn text(value: &SqlValue) -> String {
    match value {
        SqlValue::Text(t) => t.clone(),
        other => panic!("text, not {other:?}"),
    }
}

#[test]
fn insert_returning_through_the_api_in_both_modes() {
    for service in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        if service {
            // A service opens an existing database; create it first.
            drop(Db::open(&path).unwrap());
        }
        let db = if service { Db::open_service(&path) } else { Db::open(&path) }.unwrap();
        setup(&db);
        let rows = db
            .query(
                "INSERT INTO members (email) VALUES ($1) RETURNING _key, email",
                &[json!("ayu@example.com")],
            )
            .unwrap_or_else(|e| panic!("service={service}: {e}"));
        assert_eq!(rows.len(), 1);
        let key = text(&rows.iter().next().unwrap().values[0]);
        assert_eq!(key.len(), 26, "a ULID");
        assert!(db.get(("members", key.as_str())).unwrap().is_some(), "service={service}: committed");

        let moved = db
            .execute("INSERT INTO members (email) VALUES ('b@example.com'), ('c@example.com') RETURNING _key", &[])
            .unwrap();
        assert_eq!(moved, 2, "execute counts the rows a RETURNING statement wrote");

        let mut tx = db.transaction().unwrap();
        let inside = tx
            .query("INSERT INTO members (email) VALUES ('d@example.com') RETURNING _key", &[])
            .unwrap();
        let rolled = text(&inside.iter().next().unwrap().values[0]);
        tx.rollback().unwrap();
        assert!(db.get(("members", rolled.as_str())).unwrap().is_none(), "service={service}: rolled back");
    }
}

#[test]
fn a_descending_key_walk_pages_without_losing_or_repeating_a_row() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("db")).unwrap();
    db.execute("CREATE TABLE notes (_key TEXT PRIMARY KEY, body TEXT)", &[]).unwrap();
    for at in 0..7 {
        db.execute(
            "INSERT INTO notes (_key, body) VALUES ($1, 'x')",
            &[json!(format!("n{at}"))],
        )
        .unwrap();
    }
    for (sql, want) in [
        ("SELECT _key FROM notes ORDER BY _key DESC", ["n6", "n5", "n4", "n3", "n2", "n1", "n0"].as_slice()),
        ("SELECT _key FROM notes WHERE _key < 'n5' ORDER BY _key DESC", ["n4", "n3", "n2", "n1", "n0"].as_slice()),
        ("SELECT _key FROM notes ORDER BY _key", ["n0", "n1", "n2", "n3", "n4", "n5", "n6"].as_slice()),
    ] {
        let mut seen = Vec::new();
        db.stream(sql, &[], 2, &mut |row| {
            seen.push(text(&row.values[0]));
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, want, "`{sql}` in pages of two");
    }
}

#[test]
fn describe_reports_not_null_and_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("db")).unwrap();
    setup(&db);
    db.execute("CREATE TABLE tags (id TEXT PRIMARY KEY DEFAULT uuid4(), label TEXT)", &[])
        .unwrap();
    let shape = |table: &str| -> Vec<(String, bool, Option<String>)> {
        db.describe(table)
            .unwrap()
            .unwrap()
            .fields
            .into_iter()
            .map(|f| (f.name, f.not_null, f.default))
            .collect()
    };
    let owned = |rows: &[(&str, bool, Option<&str>)]| -> Vec<(String, bool, Option<String>)> {
        rows.iter()
            .map(|(n, nn, d)| (n.to_string(), *nn, d.map(str::to_owned)))
            .collect()
    };
    assert_eq!(
        shape("members"),
        owned(&[
            ("_key", true, Some("ulid()")),
            ("email", true, None),
            ("joined", false, Some("now()")),
        ])
    );
    assert_eq!(
        shape("tags"),
        owned(&[("_key", true, None), ("id", true, Some("uuid4()")), ("label", false, None)])
    );
}
