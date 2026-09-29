//! `ON CONFLICT ... DO UPDATE SET c = EXCLUDED.c` where the INSERT left `c`
//! out (finding `vuln-a09`, 2026-09-29).
//!
//! In PostgreSQL `EXCLUDED` is the row the INSERT proposed, with every
//! DEFAULT already filled. The upsert used to read a left-out column as
//! NULL, so a conflicting row had its value replaced by NULL instead of the
//! column's DEFAULT -- or was refused by a NOT NULL the INSERT itself met.
//!
//! What is at risk, one test:
//!
//! * a left-out column takes its DEFAULT through `EXCLUDED`, on the path
//!   that updates and on the path that inserts, and a column with no DEFAULT
//!   still takes NULL (`excluded_carries_the_defaults_of_the_proposed_row`).

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

fn row(db: &mut Database, sql: &str) -> Vec<SqlValue> {
    match run(db, sql) {
        SqlResult::Rows { mut rows, .. } => rows.remove(0).values,
        other => panic!("{other:?}"),
    }
}

#[test]
fn excluded_carries_the_defaults_of_the_proposed_row() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("u.sekejap"), cfg()).unwrap();
    run(
        &mut db,
        "CREATE TABLE members (_key TEXT PRIMARY KEY, role TEXT DEFAULT 'member' NOT NULL, points INT DEFAULT 0, note TEXT)",
    );
    run(&mut db, "INSERT INTO members (_key, role, points, note) VALUES ('m1', 'admin', 9, 'first')");
    let upsert = "INSERT INTO members (_key) VALUES ('{k}') \
                  ON CONFLICT (_key) DO UPDATE SET role = EXCLUDED.role, points = EXCLUDED.points, note = EXCLUDED.note";
    for key in ["m1", "m2"] {
        run(&mut db, &upsert.replace("{k}", key));
    }
    run(&mut db, "COMMIT");
    for key in ["m1", "m2"] {
        let got = row(&mut db, &format!("SELECT role, points, note FROM members WHERE _key = '{key}'"));
        assert_eq!(got[..2], [SqlValue::Text("member".into()), SqlValue::Int(0)], "{key}");
        assert!(matches!(got[2], SqlValue::Null | SqlValue::Missing), "{key}: {:?}", got[2]);
    }
}
