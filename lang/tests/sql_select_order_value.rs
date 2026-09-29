//! A select-list expression reports this statement's ORDER BY value only
//! when it IS that expression (finding `vuln-a17`, 2026-09-29).
//!
//! The select list evaluates no arithmetic; an expression there has always
//! meant "the ranking value", as in `SELECT bm25(body, 'q') ... ORDER BY
//! bm25(body, 'q')`. The parser used to discard the written expression, so
//! `SELECT n / 2 ... ORDER BY n` and `SELECT 99 ... ORDER BY n` answered
//! `n` under the expression's name.
//!
//! What is at risk, one test:
//!
//! * an expression other than the ORDER BY's is refused, and the ORDER BY's
//!   own expression still reports its value
//!   (`only_the_order_expression_reports_the_order_value`).

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{SqlDatabase, SqlResult, SqlValue};
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

#[test]
fn only_the_order_expression_reports_the_order_value() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("o.sekejap"), cfg()).unwrap();
    for sql in [
        "CREATE TABLE demo (_key TEXT PRIMARY KEY, n INT, body TEXT)",
        "CREATE INDEX demo_n ON demo USING btree (n)",
        "CREATE INDEX demo_body ON demo USING gin (to_tsvector('simple', body))",
        "INSERT INTO demo (_key, n, body) VALUES ('a', 10, 'reef reef'), ('b', 20, 'reef lagoon')",
        "COMMIT",
    ] {
        run(&mut db, sql);
    }
    for sql in [
        "SELECT n / 2 FROM demo ORDER BY n",
        "SELECT 99 FROM demo ORDER BY n",
        "SELECT n + 1 FROM demo ORDER BY n",
        "SELECT bm25(body, 'lagoon') FROM demo ORDER BY bm25(body, 'reef') DESC",
    ] {
        match db.sql(sql, &[]) {
            Err(_) => {}
            Ok(r) => panic!("`{sql}` answered {r:?}"),
        }
    }
    // The ORDER BY's own expression, in either direction's spelling.
    match run(&mut db, "SELECT _key, bm25(body, 'reef') FROM demo ORDER BY bm25(body, 'reef') DESC") {
        SqlResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].values[0], SqlValue::Text("a".into()));
            assert!(matches!(rows[0].values[1], SqlValue::Float(s) if s > 0.0));
        }
        other => panic!("{other:?}"),
    }
}
