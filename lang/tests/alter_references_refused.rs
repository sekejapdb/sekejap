//! `ALTER TABLE ... ADD COLUMN ... REFERENCES` must never promise a check it
//! does not make (0.20 follow-up to a verified 0.19 finding: the REFERENCES was
//! parsed and then silently discarded, so the statement looked like it enforced
//! a foreign key and enforced nothing).
//!
//! Entity-table foreign keys are the 0.20 item; until they exist the construct
//! is refused by name (`0A000 feature_not_supported`), pointing at that work,
//! and nothing is applied.
//!
//! What is at risk, one test:
//!
//! * `ADD COLUMN ... REFERENCES` is refused with `0A000` naming the correction,
//!   and the column is not half-added -- a plain `ADD COLUMN` of the same name
//!   then succeeds (`alter_add_column_references_is_refused_by_name`).

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::SqlDatabase;
use tempfile::TempDir;

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn run(db: &mut Database, sql: &str) {
    db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
}

#[test]
fn alter_add_column_references_is_refused_by_name() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("fk"), cfg()).unwrap();
    run(&mut db, "CREATE TABLE country (_key TEXT PRIMARY KEY, name TEXT)");
    run(&mut db, "CREATE TABLE institution (_key TEXT PRIMARY KEY, name TEXT)");

    // The reported defect: this was accepted and the REFERENCES thrown away.
    let message = match db.sql(
        "ALTER TABLE institution ADD COLUMN country TEXT REFERENCES country",
        &[],
    ) {
        Ok(result) => panic!("expected a refusal, got {result:?}"),
        Err(error) => error.to_string(),
    };
    assert!(
        message.contains("0A000"),
        "the refusal carries feature_not_supported: {message}"
    );
    assert!(
        message.to_lowercase().contains("references")
            || message.to_lowercase().contains("foreign key"),
        "the refusal names what is not yet supported: {message}"
    );

    // Nothing was applied: the column name is still free, so a plain ADD COLUMN
    // of the same name succeeds and the column is real.
    run(&mut db, "ALTER TABLE institution ADD COLUMN country TEXT");
    run(
        &mut db,
        "INSERT INTO institution (_key, name, country) VALUES ('i1', 'A University', 'id')",
    );
}
