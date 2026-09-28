//! Constant column DEFAULTs, as PostgreSQL takes them: `DEFAULT 'member'`,
//! `DEFAULT 0`, `DEFAULT true` (owner decision 2026-09-28, with the
//! application dogfooding fixes).
//!
//! What is at risk, one test each:
//!
//! * a row that leaves a column out gets its constant, a written value --
//!   NULL included -- wins, and the constants survive a reopen; a quoted
//!   literal is read as the column's type by the rules an INSERT's literal
//!   follows (`a_constant_fills_a_column_the_insert_left_out`);
//! * a literal the column cannot hold, a constant on the key, and an
//!   ADD COLUMN whose constant old rows could not show are refused by name
//!   (`a_constant_that_cannot_hold_is_refused_by_name`).

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
fn a_constant_fills_a_column_the_insert_left_out() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("c.sekejap");
    {
        let mut db = Database::create(&path, cfg()).unwrap();
        run(
            &mut db,
            "CREATE TABLE members (_key TEXT PRIMARY KEY, role TEXT DEFAULT 'member' NOT NULL, active BOOLEAN DEFAULT true, \
             points INT DEFAULT 0, ratio DOUBLE PRECISION DEFAULT 1.5, note TEXT DEFAULT NULL, \
             joined DATE DEFAULT '2020-01-02', meta JSONB DEFAULT '{\"tier\": 1}')",
        );
        run(&mut db, "INSERT INTO members (_key) VALUES ('m1')");
        run(&mut db, "INSERT INTO members (_key, role, points, note) VALUES ('m2', 'admin', 7, NULL)");
        run(&mut db, "COMMIT");
    }
    let mut db = Database::open(&path, cfg()).unwrap();
    assert_eq!(
        row(&mut db, "SELECT role, active, points, ratio, note, joined FROM members WHERE _key = 'm1'"),
        [
            SqlValue::Text("member".into()),
            SqlValue::Bool(true),
            SqlValue::Int(0),
            SqlValue::Float(1.5),
            SqlValue::Missing,
            SqlValue::Text("2020-01-02".into()),
        ]
    );
    assert_eq!(
        row(&mut db, "SELECT role, points, note FROM members WHERE _key = 'm2'"),
        [SqlValue::Text("admin".into()), SqlValue::Int(7), SqlValue::Null],
        "a written value wins, NULL included"
    );
    let ddl = match run(&mut db, "SHOW CREATE TABLE members") {
        SqlResult::Rows { rows, .. } => match &rows[0].values[0] {
            SqlValue::Text(t) => t.clone(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    for part in ["role TEXT DEFAULT 'member' NOT NULL", "active BOOLEAN DEFAULT true", "points INT DEFAULT 0", "ratio DOUBLE PRECISION DEFAULT 1.5"] {
        assert!(ddl.contains(part), "`{part}` in:\n{ddl}");
    }
    assert!(!ddl.contains("note TEXT DEFAULT"), "DEFAULT NULL is no default:\n{ddl}");
}

#[test]
fn a_constant_that_cannot_hold_is_refused_by_name() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("c.sekejap"), cfg()).unwrap();
    for sql in [
        "CREATE TABLE a (_key TEXT PRIMARY KEY, n INT DEFAULT 'many')",
        "CREATE TABLE b (_key TEXT PRIMARY KEY, n INT DEFAULT 1.5)",
        "CREATE TABLE c (_key TEXT PRIMARY KEY, ok BOOLEAN DEFAULT 'maybe')",
        "CREATE TABLE d (_key TEXT PRIMARY KEY DEFAULT 'same')",
    ] {
        assert!(db.sql(sql, &[]).is_err(), "`{sql}` is refused");
    }
    run(&mut db, "CREATE TABLE e (_key TEXT PRIMARY KEY, name TEXT)");
    run(&mut db, "INSERT INTO e (_key, name) VALUES ('e1', 'x')");
    run(&mut db, "COMMIT");
    match db.sql("ALTER TABLE e ADD COLUMN role TEXT DEFAULT 'member'", &[]) {
        Err(e) => assert!(e.to_string().contains("DEFAULT"), "{e}"),
        Ok(r) => panic!("refused by name, not {r:?}"),
    }
    // On an empty table there is no old row to disagree.
    run(&mut db, "CREATE TABLE f (_key TEXT PRIMARY KEY)");
    run(&mut db, "ALTER TABLE f ADD COLUMN role TEXT DEFAULT 'member'");
}
