//! Where NULL sorts, as PostgreSQL puts it: after every value under
//! `ORDER BY col` and before every value under `ORDER BY col DESC`
//! (NULLS LAST / NULLS FIRST are PostgreSQL's defaults). A MISSING field sorts
//! with NULL. Rows tied at NULL keep row-id order, as every other tie does.
//!
//! What is at risk, one test: an indexed order puts NULL in PostgreSQL's
//! place with and without a LIMIT, in both directions
//! (`null_sorts_last_ascending_and_first_descending`). Paging through the same
//! orders is `dist/rust/tests/order_by.rs`.

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{SqlDatabase, SqlResult, SqlValue};
use tempfile::TempDir;

fn keys(db: &mut Database, sql: &str) -> Vec<String> {
    match db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}")) {
        SqlResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match &r.values[0] {
                SqlValue::Text(k) => k.clone(),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn null_sorts_last_ascending_and_first_descending() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(
        dir.path().join("o.sekejap"),
        Config {
            budget_bytes: 1 << 20,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )
    .unwrap();
    for sql in [
        "CREATE TABLE p (_key TEXT PRIMARY KEY, name TEXT, n INT) WITH (index: [name, n])",
        "INSERT INTO p (_key, name, n) VALUES ('a', 'Ayu', 1), ('x', NULL, NULL), ('c', 'Citra', 3)",
        "INSERT INTO p (_key) VALUES ('m')",
        "INSERT INTO p (_key, name, n) VALUES ('b', 'Bayu', 2)",
        "COMMIT",
    ] {
        db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    }
    // PostgreSQL 14.15's answers on the same rows, with the insertion order
    // added as the last sort key -- PostgreSQL leaves a tie unordered, and
    // sekejap breaks it by row id, which is insertion order (`m` has neither
    // column; `x` was written before `m`).
    for (sql, postgresql) in [
        ("SELECT _key FROM p ORDER BY name", ["a", "b", "c", "x", "m"].as_slice()),
        ("SELECT _key FROM p ORDER BY name DESC", &["x", "m", "c", "b", "a"]),
        ("SELECT _key FROM p ORDER BY name LIMIT 2", &["a", "b"]),
        ("SELECT _key FROM p ORDER BY name LIMIT 4", &["a", "b", "c", "x"]),
        ("SELECT _key FROM p ORDER BY name DESC LIMIT 1", &["x"]),
        ("SELECT _key FROM p ORDER BY name DESC LIMIT 3", &["x", "m", "c"]),
        ("SELECT _key FROM p ORDER BY n", &["a", "b", "c", "x", "m"]),
        ("SELECT _key FROM p ORDER BY n DESC LIMIT 3", &["x", "m", "c"]),
        ("SELECT _key FROM p WHERE name > 'Ayu' ORDER BY name", &["b", "c"]),
        ("SELECT _key FROM p WHERE name < 'Citra' ORDER BY name DESC", &["b", "a"]),
    ] {
        assert_eq!(keys(&mut db, sql), postgresql, "`{sql}`");
    }
}
