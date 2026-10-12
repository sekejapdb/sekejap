//! A select-list expression reports this statement's ORDER BY value only
//! when it IS that expression (finding `vuln-a17`, 2026-09-29).
//!
//! The parser used to DISCARD a written select-list expression, so `SELECT n
//! / 2 ... ORDER BY n` and `SELECT 99 ... ORDER BY n` answered `n` under the
//! expression's name -- the wrong values under a misleading header. The rule
//! that fixed it: an expression the select list cannot COMPUTE (a bare number,
//! `n / 2`, a `bm25` that differs from the ORDER BY's) means the ranking value
//! and must BE the ORDER BY's own expression, else it is refused. Nothing is
//! ever returned under a name that is not what it holds.
//!
//! An expression the select list CAN compute is a §4.1 row function and is
//! answered as itself: the string and date functions, `||`, and -- since 0.20
//! -- `+`/`-` arithmetic that begins with a column (`n + 1`, `arrived -
//! departed`). That is a real value under its own header, not the vuln-a17
//! mislabeling.
//!
//! What is at risk, one test:
//!
//! * an expression the list cannot compute, other than the ORDER BY's, is
//!   refused; the ORDER BY's own expression still reports its value; and a
//!   computable `+`/`-` expression is answered as itself
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
        // Not computable here (`/` and a bare number are not row functions),
        // and not the ORDER BY's expression: refused, never mislabeled.
        "SELECT n / 2 FROM demo ORDER BY n",
        "SELECT 99 FROM demo ORDER BY n",
        // A ranking function that differs from the ORDER BY's: refused.
        "SELECT bm25(body, 'lagoon') FROM demo ORDER BY bm25(body, 'reef') DESC",
    ] {
        match db.sql(sql, &[]) {
            Err(_) => {}
            Ok(r) => panic!("`{sql}` answered {r:?}"),
        }
    }
    // A computable `+`/`-` expression over a column is a §4.1 row function
    // (0.20): it is answered as ITSELF -- the real value under its own header,
    // ordered by `n` -- not the ranking value, and not mislabeled.
    match run(&mut db, "SELECT n + 1 FROM demo ORDER BY n") {
        SqlResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].values[0], SqlValue::Int(11));
            assert_eq!(rows[1].values[0], SqlValue::Int(21));
        }
        other => panic!("`SELECT n + 1 ...` answered {other:?}"),
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
