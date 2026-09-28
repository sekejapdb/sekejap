//! A NOT NULL refusal carries PostgreSQL's SQLSTATE `23502`
//! (not_null_violation), as the missing key already does -- so a client
//! tells it apart from every other error the way it does for PostgreSQL
//! (found dogfooding under an application, 2026-09-28).
//!
//! What is at risk, one test: a column left out, an explicit NULL, and an
//! UPDATE to NULL are each `23502`, naming the column and the table in
//! PostgreSQL's words, and nothing is written
//! (`every_not_null_refusal_is_23502`).

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{SqlDatabase, SqlError, SqlResult};
use tempfile::TempDir;

#[test]
fn every_not_null_refusal_is_23502() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(
        dir.path().join("n.sekejap"),
        Config {
            budget_bytes: 1 << 20,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )
    .unwrap();
    for sql in [
        "CREATE TABLE members (_key TEXT PRIMARY KEY, email TEXT NOT NULL, name TEXT)",
        "INSERT INTO members (_key, email) VALUES ('m1', 'ayu@example.com')",
        "COMMIT",
    ] {
        db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    }
    for sql in [
        "INSERT INTO members (_key, name) VALUES ('m2', 'Bayu')",
        "INSERT INTO members (_key, email) VALUES ('m3', NULL)",
        "UPDATE members SET email = NULL WHERE _key = 'm1'",
    ] {
        match db.sql(sql, &[]) {
            Err(SqlError::Coded { sqlstate, message }) => {
                assert_eq!(sqlstate, "23502", "`{sql}`");
                assert!(
                    message.contains("null value in column \"email\" of relation \"members\" violates not-null constraint"),
                    "`{sql}`: {message}"
                );
            }
            other => panic!("`{sql}`: 23502, not {other:?}"),
        }
        let _ = db.sql("ROLLBACK", &[]);
    }
    match db.sql("SELECT _key FROM members", &[]).unwrap() {
        SqlResult::Rows { rows, .. } => assert_eq!(rows.len(), 1, "nothing was written"),
        other => panic!("{other:?}"),
    }
}
