//! `LIKE` and `ILIKE` with every pattern, answered with no index and no extra
//! storage, as PostgreSQL answers them (owner decision 2026-09-28: "search
//! '%doe%' inside joshndoesadikin must be executed as default").
//!
//! What is at risk, one test each:
//!
//! * every pattern shape -- `%`, `_`, the backslash and an `ESCAPE` character,
//!   case folding beyond ASCII, the empty string, `NOT` -- answers exactly the
//!   rows PostgreSQL 16 answers; `VECTORS` (`common/like_vectors.rs`) was produced by it
//!   (`like_and_ilike_answer_as_postgresql`);
//! * a row with no value matches neither `LIKE` nor `NOT LIKE`, and a prefix
//!   `LIKE` keeps using the column's index
//!   (`null_matches_neither_and_a_prefix_keeps_its_index`);
//! * the forms with no row check are refused by name: an integer column, and
//!   a `LIKE` inside an `OR` (`what_has_no_row_check_is_refused_by_name`).

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{SqlDatabase, SqlError, SqlResult, SqlValue};
use tempfile::TempDir;

#[path = "common/like_vectors.rs"]
mod like_vectors;
use like_vectors::{ROWS, VECTORS};

fn fixture(dir: &TempDir) -> Database {
    let mut db = Database::create(
        dir.path().join("like.sekejap"),
        Config {
            budget_bytes: 1 << 20,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )
    .unwrap();
    for sql in [
        "CREATE TABLE t (_key TEXT PRIMARY KEY, v TEXT, n INT)",
        ROWS,
        "COMMIT",
    ] {
        db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    }
    db
}

fn keys(db: &mut Database, sql: &str) -> String {
    match db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}")) {
        SqlResult::Rows { rows, .. } => {
            let mut out: Vec<String> = rows
                .into_iter()
                .map(|r| match &r.values[0] {
                    SqlValue::Text(k) => k.clone(),
                    other => panic!("{other:?}"),
                })
                .collect();
            out.sort();
            out.join(",")
        }
        other => panic!("`{sql}` answered {other:?}"),
    }
}

#[test]
fn like_and_ilike_answer_as_postgresql() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    for (op, pattern, expected) in VECTORS {
        let sql = format!("SELECT _key FROM t WHERE v {op} {pattern}");
        assert_eq!(keys(&mut db, &sql), *expected, "`{sql}`");
    }
}

#[test]
fn null_matches_neither_and_a_prefix_keeps_its_index() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    assert!(!keys(&mut db, "SELECT _key FROM t WHERE v LIKE '%'").contains("k8"));
    assert!(!keys(&mut db, "SELECT _key FROM t WHERE v NOT LIKE 'zzz%'").contains("k8"));
    let plan = match db.sql("EXPLAIN SELECT _key FROM t WHERE v LIKE 'john%'", &[]).unwrap() {
        SqlResult::Explain(text) => text,
        other => panic!("{other:?}"),
    };
    assert!(plan.contains("t_v_btree"), "a prefix LIKE stays an index range:\n{plan}");
    let plan = match db.sql("EXPLAIN SELECT _key FROM t WHERE v ILIKE '%doe%'", &[]).unwrap() {
        SqlResult::Explain(text) => text,
        other => panic!("{other:?}"),
    };
    assert!(plan.to_lowercase().contains("row"), "an infix ILIKE says it checks rows:\n{plan}");
    // The key column takes the same predicates.
    assert_eq!(keys(&mut db, "SELECT _key FROM t WHERE _key LIKE '%1%'"), "k1,k10");
}

#[test]
fn what_has_no_row_check_is_refused_by_name() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    match db.sql("SELECT _key FROM t WHERE n LIKE '%1%'", &[]) {
        Err(SqlError::Coded { sqlstate, .. }) => assert_eq!(sqlstate, "42883"),
        other => panic!("LIKE on an INT column is PostgreSQL's 42883, not {other:?}"),
    }
    match db.sql("SELECT _key FROM t WHERE v LIKE '%doe%' OR v LIKE '%roe%'", &[]) {
        Err(e) => assert!(e.to_string().contains("LIKE"), "{e}"),
        Ok(r) => panic!("refused by name, not {r:?}"),
    }
}
