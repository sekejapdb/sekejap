//! ORDER BY through the API, paged: every row once, in PostgreSQL's order,
//! however small the page -- so the walk's continuation is exact.
//!
//! What is at risk, one test: NULL in PostgreSQL's place survives paging
//! in both directions (`null_order_survives_paging`).

use sekejap::{Db, SqlValue};
use serde_json::json;

fn streamed(db: &Db, sql: &str, page: usize) -> Vec<String> {
    let mut seen = Vec::new();
    db.stream(sql, &[], page, &mut |row| {
        seen.push(match &row.values[0] {
            SqlValue::Text(t) => t.clone(),
            other => panic!("{other:?}"),
        });
        Ok(())
    })
    .unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    seen
}

#[test]
fn null_order_survives_paging() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("db")).unwrap();
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, name TEXT) WITH (index: [name])", &[])
        .unwrap();
    for (key, name) in [("a", json!("Ayu")), ("x", json!(null)), ("c", json!("Citra")), ("y", json!(null)), ("b", json!("Bayu"))] {
        db.execute("INSERT INTO p (_key, name) VALUES ($1, $2)", &[json!(key), name])
            .unwrap();
    }
    for page in [1, 2, 3, 10] {
        assert_eq!(
            streamed(&db, "SELECT _key FROM p ORDER BY name", page),
            ["a", "b", "c", "x", "y"],
            "ascending, pages of {page}"
        );
        assert_eq!(
            streamed(&db, "SELECT _key FROM p ORDER BY name DESC", page),
            ["x", "y", "c", "b", "a"],
            "descending, pages of {page}"
        );
    }
}

/// Several keys, indexed and not, streamed in small pages: every row once,
/// in the order the unpaged query gives (PostgreSQL's, pinned in
/// `lang/tests/sql_order_by.rs`).
#[test]
fn a_multi_key_order_pages_without_losing_or_repeating_a_row() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("db")).unwrap();
    db.execute(
        "CREATE TABLE p (_key TEXT PRIMARY KEY, city TEXT, name TEXT, a DOUBLE PRECISION, b DOUBLE PRECISION, active BOOLEAN) WITH (index: [city, name])",
        &[],
    )
    .unwrap();
    db.execute(
        "INSERT INTO p (_key, city, name, a, b, active) VALUES \
         ('k1', 'Ubud', 'Ayu', 2, 0, true), ('k2', 'Kuta', 'Bayu', 0, 1, false), ('k3', 'Ubud', 'Ayu', 1, 0, true), \
         ('k4', NULL, 'Citra', 0, 0.8, NULL), ('k5', 'Kuta', 'Ayu', 1, 1, true), ('k6', 'Ubud', 'Dewi', NULL, 2, false), \
         ('k7', 'Kuta', NULL, 2, 0, true), ('k8', 'Ubud', 'Bayu', 1, 0, NULL)",
        &[],
    )
    .unwrap();
    for (sql, want) in [
        ("SELECT _key FROM p ORDER BY city, name, _key", "k5,k2,k7,k1,k3,k8,k6,k4"),
        ("SELECT _key FROM p ORDER BY city DESC, name, _key", "k4,k1,k3,k8,k6,k5,k2,k7"),
        ("SELECT _key FROM p ORDER BY a DESC, _key", "k6,k1,k7,k3,k5,k8,k2,k4"),
        ("SELECT _key FROM p ORDER BY active DESC, name, _key", "k8,k4,k1,k3,k5,k7,k2,k6"),
        ("SELECT _key FROM p ORDER BY _key DESC, name", "k8,k7,k6,k5,k4,k3,k2,k1"),
    ] {
        for page in [1, 2, 3] {
            assert_eq!(streamed(&db, sql, page).join(","), want, "`{sql}` in pages of {page}");
        }
    }
}

/// "Load more" as an application writes it: the first page, then each next
/// page after the last row's (name, _key), with `$1, $2` bound. Every row
/// with a name comes once, in order -- the rows tied on a name are not
/// skipped at a page boundary. A row whose name is NULL is not reached, as in
/// PostgreSQL: a comparison with NULL is not true.
#[test]
fn keyset_paging_with_a_row_comparison_skips_no_tied_row() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("db")).unwrap();
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, name TEXT) WITH (index: [name])", &[])
        .unwrap();
    db.execute(
        "INSERT INTO p (_key, name) VALUES ('k1', 'Ayu'), ('k2', 'Bayu'), ('k3', 'Ayu'), ('k4', 'Citra'), \
         ('k5', 'Ayu'), ('k6', 'Dewi'), ('k7', NULL), ('k8', 'Bayu')",
        &[],
    )
    .unwrap();
    let page = |rows: sekejap::Rows| -> Vec<(String, String)> {
        rows.into_iter()
            .map(|row| match (&row.values[0], &row.values[1]) {
                (SqlValue::Text(name), SqlValue::Text(key)) => (name.clone(), key.clone()),
                other => panic!("{other:?}"),
            })
            .collect()
    };
    let mut seen = page(db.query("SELECT name, _key FROM p ORDER BY name, _key LIMIT 2", &[]).unwrap());
    loop {
        let (name, key) = seen.last().unwrap().clone();
        let next = page(
            db.query(
                "SELECT name, _key FROM p WHERE (name, _key) > ($1, $2) ORDER BY name, _key LIMIT 2",
                &[json!(name), json!(key)],
            )
            .unwrap(),
        );
        if next.is_empty() {
            break;
        }
        seen.extend(next);
    }
    assert_eq!(
        seen.iter().map(|(_, key)| key.as_str()).collect::<Vec<_>>(),
        ["k1", "k3", "k5", "k2", "k8", "k4", "k6"]
    );
}
