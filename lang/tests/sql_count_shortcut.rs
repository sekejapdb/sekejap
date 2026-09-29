//! The live-count shortcut for `SELECT COUNT(*) FROM t` (finding
//! `vuln-a16`, 2026-09-29).
//!
//! A bare `COUNT(*)` reads the table's live count instead of walking it. The
//! shortcut used to answer before HAVING and the output limit were looked
//! at, so `HAVING COUNT(*) > 100` over one row and `LIMIT 0` both answered a
//! row holding 1.
//!
//! What is at risk, one test:
//!
//! * HAVING and LIMIT hold over the shortcut as over a walk
//!   (`having_and_limit_hold_over_the_live_count`).

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

fn rows(db: &mut Database, sql: &str) -> Vec<Vec<SqlValue>> {
    match db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}")) {
        SqlResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("`{sql}` answered {other:?}"),
    }
}

#[test]
fn having_and_limit_hold_over_the_live_count() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("c.sekejap"), cfg()).unwrap();
    db.sql("CREATE TABLE demo (_key TEXT PRIMARY KEY, n INT)", &[]).unwrap();
    db.sql("INSERT INTO demo (_key, n) VALUES ('a', 1)", &[]).unwrap();
    db.sql("COMMIT", &[]).unwrap();
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM demo"), [[SqlValue::Int(1)]]);
    assert!(rows(&mut db, "SELECT COUNT(*) FROM demo HAVING COUNT(*) > 100").is_empty());
    assert_eq!(
        rows(&mut db, "SELECT COUNT(*) FROM demo HAVING COUNT(*) > 0"),
        [[SqlValue::Int(1)]]
    );
    assert!(rows(&mut db, "SELECT COUNT(*) FROM demo LIMIT 0").is_empty());
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM demo LIMIT 1"), [[SqlValue::Int(1)]]);
}
