//! ORDER BY as PostgreSQL answers it: several keys, each ASC or DESC, over
//! indexed and unindexed columns, `_key`, and a numeric expression -- with no
//! index required (owner decision 2026-09-28, 0.18.3). NULL sorts last
//! ascending and first descending.
//!
//! Every expected list is PostgreSQL 14.15's answer on the same rows
//! (`COLLATE "C"`, the byte order sekejap's text keys use), with the
//! insertion order as the last key where the query leaves a tie -- sekejap
//! breaks every tie by row id, which is insertion order.
//!
//! What is at risk, one test: each shape of ORDER BY returns PostgreSQL's
//! rows in PostgreSQL's order (`order_by_answers_as_postgresql_does`).
//! Paging through the same orders is `dist/rust/tests/order_by.rs`.

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{SqlDatabase, SqlResult, SqlValue};
use tempfile::TempDir;

pub const ROWS: &str = "INSERT INTO p (_key, city, name, a, b, active) VALUES \
    ('k1', 'Ubud', 'Ayu', 2, 0, true), ('k2', 'Kuta', 'Bayu', 0, 1, false), ('k3', 'Ubud', 'Ayu', 1, 0, true), \
    ('k4', NULL, 'Citra', 0, 0.8, NULL), ('k5', 'Kuta', 'Ayu', 1, 1, true), ('k6', 'Ubud', 'Dewi', NULL, 2, false), \
    ('k7', 'Kuta', NULL, 2, 0, true), ('k8', 'Ubud', 'Bayu', 1, 0, NULL)";

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
fn order_by_answers_as_postgresql_does() {
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
        // `city` and `name` indexed, `a`, `b` and `active` not.
        "CREATE TABLE p (_key TEXT PRIMARY KEY, city TEXT, name TEXT, a DOUBLE PRECISION, b DOUBLE PRECISION, active BOOLEAN) WITH (index: [city, name])",
        ROWS,
        "COMMIT",
    ] {
        db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    }
    for (sql, postgresql) in [
        ("SELECT _key FROM p ORDER BY city, name, _key", "k5,k2,k7,k1,k3,k8,k6,k4"),
        ("SELECT _key FROM p ORDER BY city DESC, name, _key", "k4,k1,k3,k8,k6,k5,k2,k7"),
        ("SELECT _key FROM p ORDER BY name, _key LIMIT 3", "k1,k3,k5"),
        ("SELECT _key FROM p ORDER BY a", "k2,k4,k3,k5,k8,k1,k7,k6"),
        ("SELECT _key FROM p ORDER BY a DESC, _key", "k6,k1,k7,k3,k5,k8,k2,k4"),
        ("SELECT _key FROM p ORDER BY active DESC, name, _key", "k8,k4,k1,k3,k5,k7,k2,k6"),
        ("SELECT _key FROM p WHERE city = 'Ubud' ORDER BY name DESC, _key LIMIT 2", "k6,k8"),
        ("SELECT _key FROM p ORDER BY _key DESC, name", "k8,k7,k6,k5,k4,k3,k2,k1"),
        // k6's `a` is NULL: PostgreSQL's whole expression is NULL and sorts
        // first under DESC; sekejap's Score rule counts a NULL field as 0,
        // so k6 scores 10, also first. The two agree on these rows.
        ("SELECT _key FROM p ORDER BY b * 5 + a * 4 DESC, _key", "k6,k5,k1,k7,k2,k3,k4,k8"),
        ("SELECT _key FROM p ORDER BY name DESC, city, _key LIMIT 4", "k7,k6,k4,k2"),
    ] {
        assert_eq!(keys(&mut db, sql), postgresql, "`{sql}`");
    }
}
