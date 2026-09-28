//! `INSERT ... RETURNING`, as PostgreSQL answers it (found dogfooding under
//! an application, 2026-09-28): with a generated key, RETURNING is the only
//! way a caller learns the key a row was given.
//!
//! What is at risk, one test each:
//!
//! * the rows come back with the columns named, the minted key among them,
//!   and a column filled by its DEFAULT carries the stored value
//!   (`returning_answers_the_inserted_rows_with_their_generated_values`);
//! * `RETURNING *` is the columns `SELECT *` answers, in that order (the
//!   key is asked for by name, `RETURNING _key, *`), and a row an
//!   `ON CONFLICT DO NOTHING` skipped is not returned, while one it updated
//!   is (`returning_star_and_on_conflict_follow_postgresql`);
//! * an unknown column is PostgreSQL's 42703, and the statement writes
//!   nothing (`returning_an_unknown_column_is_42703_and_writes_nothing`).

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{SqlDatabase, SqlError, SqlResult, SqlValue};
use tempfile::TempDir;

fn fixture(dir: &TempDir) -> Database {
    let mut db = Database::create(
        dir.path().join("r.sekejap"),
        Config {
            budget_bytes: 1 << 20,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )
    .unwrap();
    for sql in [
        "CREATE TABLE members (_key TEXT PRIMARY KEY DEFAULT ulid(), email TEXT NOT NULL, joined TIMESTAMPTZ DEFAULT now())",
        "CREATE TABLE tags (id TEXT PRIMARY KEY DEFAULT uuid4(), label TEXT)",
        "COMMIT",
    ] {
        db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    }
    db
}

fn rows(result: SqlResult) -> (Vec<String>, Vec<Vec<SqlValue>>) {
    match result {
        SqlResult::Rows { columns, rows } => (columns, rows.into_iter().map(|r| r.values).collect()),
        other => panic!("rows, not {other:?}"),
    }
}

fn text(value: &SqlValue) -> String {
    match value {
        SqlValue::Text(t) => t.clone(),
        other => panic!("text, not {other:?}"),
    }
}

#[test]
fn returning_answers_the_inserted_rows_with_their_generated_values() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    let (columns, answered) = rows(
        db.sql(
            "INSERT INTO members (email) VALUES ('ayu@example.com'), ('bayu@example.com') RETURNING _key, email, joined",
            &[],
        )
        .expect("INSERT ... RETURNING answers"),
    );
    assert_eq!(columns, ["_key", "email", "joined"]);
    assert_eq!(answered.len(), 2);
    for (row, email) in answered.iter().zip(["ayu@example.com", "bayu@example.com"]) {
        let key = text(&row[0]);
        assert_eq!(key.len(), 26, "a ULID: {key}");
        assert_eq!(text(&row[1]), email);
        assert!(
            !matches!(row[2], SqlValue::Null | SqlValue::Missing),
            "`joined` took its DEFAULT now(), and RETURNING reports it: {:?}",
            row[2]
        );
        // The key RETURNING reported is the row's key.
        let (_, found) = rows(
            db.sql(&format!("SELECT email FROM members WHERE _key = '{key}'"), &[])
                .unwrap(),
        );
        assert_eq!(found.len(), 1);
        assert_eq!(text(&found[0][0]), email);
    }
    // A named key column reports the minted key too.
    let (columns, answered) = rows(
        db.sql("INSERT INTO tags (label) VALUES ('red') RETURNING id", &[])
            .unwrap(),
    );
    assert_eq!(columns, ["id"]);
    assert_eq!(text(&answered[0][0]).len(), 36, "a UUID");
}

#[test]
fn returning_star_and_on_conflict_follow_postgresql() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    let (star, answered) = rows(
        db.sql(
            "INSERT INTO members (_key, email) VALUES ('m1', 'ayu@example.com') RETURNING _key, *",
            &[],
        )
        .unwrap(),
    );
    let (select_star, _) = rows(db.sql("SELECT * FROM members", &[]).unwrap());
    assert_eq!(star[0], "_key");
    assert_eq!(star[1..], select_star[..], "RETURNING * is SELECT *'s columns");
    assert_eq!(text(&answered[0][0]), "m1");
    // PostgreSQL: DO NOTHING returns no row for the conflicting one.
    let (_, answered) = rows(
        db.sql(
            "INSERT INTO members (_key, email) VALUES ('m1', 'other@example.com'), ('m2', 'bayu@example.com') ON CONFLICT (_key) DO NOTHING RETURNING _key",
            &[],
        )
        .unwrap(),
    );
    assert_eq!(answered.iter().map(|r| text(&r[0])).collect::<Vec<_>>(), ["m2"]);
    // DO UPDATE returns the row as it now is.
    let (_, answered) = rows(
        db.sql(
            "INSERT INTO members (_key, email) VALUES ('m1', 'new@example.com') ON CONFLICT (_key) DO UPDATE SET email = EXCLUDED.email RETURNING _key, email",
            &[],
        )
        .unwrap(),
    );
    assert_eq!(text(&answered[0][1]), "new@example.com");
}

#[test]
fn returning_an_unknown_column_is_42703_and_writes_nothing() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    match db.sql(
        "INSERT INTO members (_key, email) VALUES ('m9', 'x@example.com') RETURNING nope",
        &[],
    ) {
        Err(SqlError::Coded { sqlstate, .. }) => assert_eq!(sqlstate, "42703"),
        other => panic!("42703, not {other:?}"),
    }
    let (_, found) = rows(db.sql("SELECT _key FROM members WHERE _key = 'm9'", &[]).unwrap());
    assert!(found.is_empty(), "a refused statement wrote nothing");
}
