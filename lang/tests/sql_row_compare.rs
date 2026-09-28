//! Row comparison, `(a, b) > (x, y)`, as PostgreSQL answers it: the pairs
//! compared left to right, stopping at the first that differs -- and a NULL
//! there makes the comparison NULL, so the row is not returned; `=` needs
//! every pair equal and `<>` any non-NULL pair different. It is what keyset
//! paging ("load more") over a column with ties is written with (0.18.3).
//!
//! Every expected list is PostgreSQL 14.15's answer on the same rows
//! (`COLLATE "C"`, sekejap's text byte order).
//!
//! What is at risk, one test: each comparison, over indexed and unindexed
//! first columns and with two or three columns, returns PostgreSQL's rows
//! (`row_comparison_answers_as_postgresql_does`). The paged loop through the
//! API is `dist/rust/tests/order_by.rs`.

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{SqlDatabase, SqlResult, SqlValue};
use tempfile::TempDir;

fn keys(db: &mut Database, sql: &str) -> String {
    match db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}")) {
        SqlResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match &r.values[0] {
                SqlValue::Text(k) => k.clone(),
                other => panic!("{other:?}"),
            })
            .collect::<Vec<_>>()
            .join(","),
        other => panic!("{other:?}"),
    }
}

#[test]
fn row_comparison_answers_as_postgresql_does() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(
        dir.path().join("r.sekejap"),
        Config {
            budget_bytes: 1 << 20,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )
    .unwrap();
    for sql in [
        "CREATE TABLE p (_key TEXT PRIMARY KEY, city TEXT, name TEXT, a DOUBLE PRECISION, b DOUBLE PRECISION, active BOOLEAN) WITH (index: [city, name])",
        "INSERT INTO p (_key, city, name, a, b, active) VALUES \
         ('k1', 'Ubud', 'Ayu', 2, 0, true), ('k2', 'Kuta', 'Bayu', 0, 1, false), ('k3', 'Ubud', 'Ayu', 1, 0, true), \
         ('k4', NULL, 'Citra', 0, 0.8, NULL), ('k5', 'Kuta', 'Ayu', 1, 1, true), ('k6', 'Ubud', 'Dewi', NULL, 2, false), \
         ('k7', 'Kuta', NULL, 2, 0, true), ('k8', 'Ubud', 'Bayu', 1, 0, NULL)",
        "COMMIT",
    ] {
        db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    }
    for (sql, postgresql) in [
        ("SELECT _key FROM p WHERE (name, _key) > ('Ayu', 'k3') ORDER BY name, _key", "k5,k2,k8,k4,k6"),
        ("SELECT _key FROM p WHERE (name, _key) > ('Ayu', 'k3') ORDER BY name, _key LIMIT 2", "k5,k2"),
        ("SELECT _key FROM p WHERE (name, _key) < ('Citra', 'k0') ORDER BY name DESC, _key DESC LIMIT 3", "k8,k2,k5"),
        ("SELECT _key FROM p WHERE (a, _key) >= (1, 'k5') ORDER BY a, _key", "k5,k8,k1,k7"),
        ("SELECT _key FROM p WHERE (city, name, _key) > ('Kuta', 'Bayu', 'k2') ORDER BY city, name, _key", "k1,k3,k8,k6"),
        ("SELECT _key FROM p WHERE (name, _key) = ('Ayu', 'k3')", "k3"),
        ("SELECT _key FROM p WHERE (name, _key) <> ('Ayu', 'k3') ORDER BY _key", "k1,k2,k4,k5,k6,k7,k8"),
        ("SELECT _key FROM p WHERE (name, _key) <= ('Bayu', 'k2') ORDER BY name, _key", "k1,k3,k5,k2"),
    ] {
        assert_eq!(keys(&mut db, sql), postgresql, "`{sql}`");
    }
    // Two lists of different lengths is PostgreSQL's syntax error too.
    assert!(db.sql("SELECT _key FROM p WHERE (name, _key) > ('Ayu')", &[]).is_err());
}
