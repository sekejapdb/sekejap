//! Live writes over a text index whose postings were PACKED by a late build
//! (finding `v019-f2`, 2026-09-29, present since packed segments shipped).
//!
//! A packed posting cannot be rewritten in place, so a live write records the
//! document's current state in a HEAD row that overrides the segment -- a
//! frequency, or `0` for "no longer here". Removing a word used to DELETE the
//! head row whenever one existed, on the assumption that it was the write's
//! own earlier posting. When the head row was overriding a packed posting,
//! deleting it made the packed posting live again: the term's postings then
//! outnumbered its statistics, and every later query on that word reported
//! the database corrupt.
//!
//! What is at risk, one test each:
//!
//! * update a packed document keeping the word, then delete the document
//!   (`update_then_delete_keeps_the_index_exact`);
//! * remove the word, restore it, remove it again
//!   (`remove_restore_remove_keeps_the_index_exact`);
//! * a document written after the fold and then deleted leaves no marker
//!   behind: there is nothing packed for it to hide
//!   (`a_live_document_deleted_leaves_no_marker`).
//!
//! Each compares the answer with the row check and runs the verifier.

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

fn keys(db: &mut Database, sql: &str) -> Vec<String> {
    match run(db, sql) {
        SqlResult::Rows { rows, .. } => {
            let mut out: Vec<String> = rows
                .into_iter()
                .map(|r| match &r.values[0] {
                    SqlValue::Text(k) => k.clone(),
                    other => panic!("{other:?}"),
                })
                .collect();
            out.sort();
            out
        }
        other => panic!("`{sql}` answered {other:?}"),
    }
}

/// 300 rows all saying `reef`, the index built after them: every posting
/// packed.
fn packed(path: &std::path::Path) -> Database {
    let mut db = Database::create(path, cfg()).unwrap();
    run(&mut db, "CREATE TABLE doc (_key TEXT PRIMARY KEY, body TEXT) WITH (index: none)");
    let values: Vec<String> = (0..300).map(|i| format!("('d{i:03}', 'reef walk {i}')")).collect();
    run(&mut db, &format!("INSERT INTO doc (_key, body) VALUES {}", values.join(", ")));
    run(&mut db, "COMMIT");
    run(&mut db, "CREATE INDEX doc_body ON doc USING gin (to_tsvector('simple', body))");
    run(&mut db, "COMMIT");
    db
}

/// The indexed answers, ranked and unranked, equal the row check; the file
/// verifies clean.
fn exact(mut db: Database, path: &std::path::Path) {
    let row_check = keys(&mut db, "SELECT _key FROM doc WHERE body LIKE '%reef%'");
    let words = keys(
        &mut db,
        "SELECT _key FROM doc WHERE to_tsvector('simple', body) @@ to_tsquery('simple', 'reef')",
    );
    let ranked = keys(&mut db, "SELECT _key FROM doc ORDER BY bm25(body, 'reef') DESC");
    assert_eq!(words, row_check);
    assert_eq!(ranked, row_check);
    drop(db);
    use sekejap_core::collections::verification::{verify_indexed_source, VerificationLimits};
    let report = verify_indexed_source(path, VerificationLimits::default(), |issue| {
        panic!("unexpected verifier issue: {issue:?}")
    })
    .unwrap();
    assert!(report.complete && report.clean, "{report:?}");
}

#[test]
fn update_then_delete_keeps_the_index_exact() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("live.sekejap");
    let mut db = packed(&path);
    run(&mut db, "UPDATE doc SET body = 'reef reef' WHERE _key = 'd007'");
    run(&mut db, "COMMIT");
    run(&mut db, "DELETE FROM doc WHERE _key = 'd007'");
    run(&mut db, "COMMIT");
    exact(db, &path);
}

#[test]
fn remove_restore_remove_keeps_the_index_exact() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("live.sekejap");
    let mut db = packed(&path);
    for body in ["lagoon", "reef", "lagoon"] {
        run(&mut db, &format!("UPDATE doc SET body = '{body}' WHERE _key = 'd011'"));
        run(&mut db, "COMMIT");
    }
    exact(db, &path);
}

#[test]
fn a_live_document_deleted_leaves_no_marker() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("live.sekejap");
    let mut db = packed(&path);
    run(&mut db, "INSERT INTO doc (_key, body) VALUES ('z999', 'reef lagoon')");
    run(&mut db, "COMMIT");
    run(&mut db, "DELETE FROM doc WHERE _key = 'z999'");
    run(&mut db, "COMMIT");
    exact(db, &path);
}
