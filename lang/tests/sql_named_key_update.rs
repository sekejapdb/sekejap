//! A declared PRIMARY KEY column is the row's identity (finding vuln-a08,
//! 0.18.5).
//!
//! `CREATE TABLE demo (id TEXT PRIMARY KEY)` makes `id` supply the row's
//! key. An UPDATE could assign `id` like any other column -- to another
//! row's value or to NULL -- while the row kept its old key underneath: the
//! declared key was then duplicated or missing and disagreed with the row
//! it named. As `_key` already is, the key column is refused in every UPDATE
//! form, with the reason; a new key is a new row.

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
fn a_declared_key_column_cannot_be_assigned() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("key.sekejap"), cfg()).unwrap();
    run(&mut db, "CREATE TABLE demo (id TEXT PRIMARY KEY, note TEXT)");
    run(&mut db, "INSERT INTO demo (id, note) VALUES ('a', 'x'), ('b', 'y')");
    run(&mut db, "COMMIT");
    for sql in [
        "UPDATE demo SET id = 'a' WHERE _key = 'b'",
        "UPDATE demo SET id = NULL WHERE _key = 'b'",
        "UPDATE demo SET id = 'c' WHERE note = 'y'",
        "UPDATE demo SET note = 'z', id = 'c' WHERE id = 'b'",
    ] {
        match db.sql(sql, &[]) {
            Err(e) => assert!(e.to_string().contains("PRIMARY KEY"), "`{sql}`: {e}"),
            Ok(r) => panic!("`{sql}` was accepted: {r:?}"),
        }
        let _ = db.sql("ROLLBACK", &[]);
    }
    // The other columns still update, and the keys are as written.
    run(&mut db, "UPDATE demo SET note = 'z' WHERE _key = 'b'");
    run(&mut db, "COMMIT");
    match run(&mut db, "SELECT _key, id, note FROM demo ORDER BY _key") {
        SqlResult::Rows { rows, .. } => {
            let got: Vec<Vec<SqlValue>> = rows.into_iter().map(|r| r.values).collect();
            assert_eq!(
                got,
                vec![
                    vec![SqlValue::Text("a".into()), SqlValue::Text("a".into()), SqlValue::Text("x".into())],
                    vec![SqlValue::Text("b".into()), SqlValue::Text("b".into()), SqlValue::Text("z".into())],
                ]
            );
        }
        other => panic!("{other:?}"),
    }
}
