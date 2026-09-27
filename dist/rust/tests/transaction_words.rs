//! `BEGIN`, `COMMIT` and `ROLLBACK` through the API's one-statement calls.
//!
//! `Db::execute` and `Db::query` each run in a transaction of their own and
//! commit it, so a transaction word passed through them used to answer `Ok`
//! and do nothing: a `ROLLBACK` after an `INSERT` left the row there. They
//! are refused by name now, pointing at `Db::transaction`, and so are they
//! inside `Tx::execute`, where a `COMMIT` would end the transaction the
//! caller is holding. `BEGIN BULK` / `END BULK` are other statements and
//! still run. (Found dogfooding under an application, 2026-09-28.)

use sekejap::Db;
use serde_json::json;

fn open() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("db")).unwrap();
    db.execute("CREATE TABLE topics (_key TEXT PRIMARY KEY, name TEXT)", &[]).unwrap();
    (dir, db)
}

#[test]
fn a_transaction_word_on_the_one_statement_calls_is_refused_by_name() {
    let (_dir, db) = open();
    for word in ["BEGIN", "BEGIN TRANSACTION", "START TRANSACTION", "COMMIT", "END", "ROLLBACK", "ABORT", "commit"] {
        let executed = db.execute(word, &[]);
        let error = executed.expect_err(&format!("`{word}` through Db::execute is refused"));
        assert!(error.to_string().contains("Db::transaction"), "`{word}`: {error}");
        assert!(db.query(word, &[]).is_err(), "`{word}` through Db::query is refused");
    }
    // Nothing was silently rolled back or committed: the ordinary statement
    // still commits on its own, as it always did.
    db.execute("INSERT INTO topics (_key, name) VALUES ('t1', 'Temp')", &[]).unwrap();
    assert!(db.get(("topics", "t1")).unwrap().is_some());
    // BEGIN BULK / END BULK are the bulk scope, not a transaction word.
    db.execute("BEGIN BULK", &[]).unwrap();
    db.execute("END BULK", &[]).unwrap();
}

#[test]
fn a_transaction_word_inside_a_transaction_is_refused_and_the_transaction_decides() {
    let (_dir, db) = open();
    let mut tx = db.transaction().unwrap();
    tx.execute("INSERT INTO topics (_key, name) VALUES ('t9', 'Temp')", &[]).unwrap();
    assert!(tx.execute("COMMIT", &[]).is_err(), "COMMIT would end the caller's transaction");
    tx.rollback().unwrap();
    assert!(db.get(("topics", "t9")).unwrap().is_none(), "the transaction's own rollback holds");
    let _ = json!(null);
}
