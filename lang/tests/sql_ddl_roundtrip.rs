//! `SHOW CREATE TABLE` builds the same table again (found dogfooding under an
//! application, 2026-09-28): a tool that mirrors a schema replays the DDL a
//! database prints, so a key DEFAULT, a named PRIMARY KEY column or a NOT
//! NULL that the print leaves out is lost on the copy -- and an INSERT that
//! relied on the key DEFAULT fails there.
//!
//! What is at risk, one test each:
//!
//! * the print carries the key's DEFAULT, a named key column, NOT NULL and
//!   every generator DEFAULT, and replaying it gives the same print and the
//!   same behaviour (`show_create_table_replays_to_the_same_table`);
//! * `db_columns` reports the key's DEFAULT
//!   (`db_columns_reports_the_key_default`).

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

fn show(db: &mut Database, table: &str) -> String {
    match run(db, &format!("SHOW CREATE TABLE {table}")) {
        SqlResult::Rows { rows, .. } => match &rows[0].values[0] {
            SqlValue::Text(ddl) => ddl.clone(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

const TABLES: &[&str] = &[
    "CREATE TABLE members (_key TEXT PRIMARY KEY DEFAULT ulid(), email TEXT NOT NULL, joined TIMESTAMPTZ DEFAULT now())",
    "CREATE TABLE tags (id TEXT PRIMARY KEY DEFAULT uuid4(), label TEXT)",
    "CREATE TABLE people (id TEXT PRIMARY KEY, name TEXT)",
    "CREATE TABLE slugs (_key TEXT PRIMARY KEY, fixed TEXT DEFAULT uuid5('6ba7b810-9dad-11d1-80b4-00c04fd430c8', 'site-a'))",
];

#[test]
fn show_create_table_replays_to_the_same_table() {
    let source_dir = TempDir::new().unwrap();
    let mut source = Database::create(source_dir.path().join("a.sekejap"), cfg()).unwrap();
    for sql in TABLES {
        run(&mut source, sql);
    }
    run(&mut source, "COMMIT");
    let members = show(&mut source, "members");
    assert!(members.contains("_key TEXT PRIMARY KEY DEFAULT ulid()"), "{members}");
    assert!(members.contains("email TEXT NOT NULL"), "{members}");
    assert!(members.contains("joined TIMESTAMPTZ DEFAULT now()"), "{members}");
    let tags = show(&mut source, "tags");
    assert!(tags.contains("id TEXT PRIMARY KEY DEFAULT uuid4()"), "{tags}");
    assert!(!tags.contains("_key"), "a named key column is the key: {tags}");
    let slugs = show(&mut source, "slugs");
    assert!(
        slugs.contains("DEFAULT uuid5('6ba7b810-9dad-11d1-80b4-00c04fd430c8', 'site-a')"),
        "{slugs}"
    );

    // Replay every print on a fresh database: the same print comes back.
    let copy_dir = TempDir::new().unwrap();
    let mut copy = Database::create(copy_dir.path().join("b.sekejap"), cfg()).unwrap();
    for table in ["members", "tags", "people", "slugs"] {
        let ddl = show(&mut source, table);
        for statement in ddl.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            run(&mut copy, statement);
        }
        run(&mut copy, "COMMIT");
        assert_eq!(show(&mut copy, table), ddl, "`{table}` replays to itself");
    }
    // And the copy behaves: the key DEFAULT mints, the named key supplies.
    run(&mut copy, "INSERT INTO members (email) VALUES ('ayu@example.com')");
    run(&mut copy, "INSERT INTO tags (label) VALUES ('red')");
    run(&mut copy, "INSERT INTO people (id, name) VALUES ('p1', 'Ayu')");
    run(&mut copy, "COMMIT");
    match run(&mut copy, "SELECT name FROM people WHERE _key = 'p1'") {
        SqlResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("{other:?}"),
    }
}

#[test]
fn db_columns_reports_the_key_default() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("c.sekejap"), cfg()).unwrap();
    run(&mut db, TABLES[0]);
    run(&mut db, TABLES[1]);
    run(&mut db, "COMMIT");
    for (table, column) in [("members", "_key"), ("tags", "id")] {
        match run(
            &mut db,
            &format!("SELECT not_null, has_default FROM db_columns WHERE table = '{table}' AND name = '{column}'"),
        ) {
            SqlResult::Rows { rows, .. } => {
                assert_eq!(rows.len(), 1, "{table}.{column}");
                assert_eq!(rows[0].values, [SqlValue::Bool(true), SqlValue::Bool(true)], "{table}.{column}");
            }
            other => panic!("{other:?}"),
        }
    }
}
