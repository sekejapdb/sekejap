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

/// Finding vuln-a04 (0.18.5): the guard read the statement's first word from
/// the raw text, so a comment in front of a transaction word hid it from the
/// guard while the SQL parser still ran it -- `/* note */ COMMIT` inside a
/// `Tx` committed the caller's transaction, and its rollback then undid
/// nothing. A comment is not a statement: the word behind it is refused, in
/// both modes.
#[test]
fn a_comment_does_not_hide_a_transaction_word() {
    for service in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let db = if service { Db::open_service(&path).unwrap() } else { Db::open(&path).unwrap() };
        db.execute("CREATE TABLE topics (_key TEXT PRIMARY KEY, name TEXT)", &[]).unwrap();
        for word in ["/* note */ COMMIT", "-- note\nCOMMIT", "/* a */ /* b */ ROLLBACK", "  /*x*/BEGIN"] {
            assert!(db.execute(word, &[]).is_err(), "service={service}: `{word}` through Db::execute");
            let mut tx = db.transaction().unwrap();
            tx.execute("INSERT INTO topics (_key, name) VALUES ('t1', 'Temp')", &[]).unwrap();
            assert!(tx.execute(word, &[]).is_err(), "service={service}: `{word}` inside a Tx");
            tx.rollback().unwrap();
            assert!(
                db.get(("topics", "t1")).unwrap().is_none(),
                "service={service}: `{word}` committed the caller's transaction"
            );
        }
    }
}

/// Finding vuln-a07 (0.18.5): a statement that failed part way inside a `Tx`
/// -- a two-row INSERT whose second row repeats the first row's new key --
/// left its first row written, and a caller that committed after the error
/// made it durable. As in PostgreSQL, an error aborts the transaction: later
/// statements are refused with 25P02, `commit` rolls back and reports it, and
/// `rollback` ends it as always. Both modes.
#[test]
fn a_failed_statement_aborts_the_transaction() {
    for service in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let db = if service { Db::open_service(&path).unwrap() } else { Db::open(&path).unwrap() };
        db.execute("CREATE TABLE topics (_key TEXT PRIMARY KEY, name TEXT)", &[]).unwrap();
        db.execute("INSERT INTO topics (_key, name) VALUES ('t0', 'Kept')", &[]).unwrap();

        let mut tx = db.transaction().unwrap();
        tx.execute("INSERT INTO topics (_key, name) VALUES ('t1', 'Before')", &[]).unwrap();
        assert!(tx
            .execute("INSERT INTO topics (_key, name) VALUES ('t2', 'A'), ('t2', 'B')", &[])
            .is_err());
        let refused = tx.execute("INSERT INTO topics (_key, name) VALUES ('t3', 'After')", &[]);
        assert!(refused.unwrap_err().to_string().contains("25P02"), "service={service}");
        let committed = tx.commit();
        assert!(committed.unwrap_err().to_string().contains("25P02"), "service={service}: commit reports the abort");
        for key in ["t1", "t2", "t3"] {
            assert!(db.get(("topics", key)).unwrap().is_none(), "service={service}: `{key}` was committed");
        }
        assert!(db.get(("topics", "t0")).unwrap().is_some());
        // The handle is usable afterwards.
        db.execute("INSERT INTO topics (_key, name) VALUES ('t4', 'Next')", &[]).unwrap();
        assert!(db.get(("topics", "t4")).unwrap().is_some());
    }
}
