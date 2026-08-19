//! The btree field index as a delta over an immutable base.
//!
//! A write to an indexed column used to copy the entire mapped index onto the
//! heap first, because `field_index_ref` consulted the two halves in preference
//! order instead of merging them — the heap had to become the whole truth before
//! it could be changed. That cost 23 MB of RAM for a single write against a
//! 500 000-row index, and the cost grew with the store rather than the write.
//!
//! The heap side is a delta now and reads merge both halves, gated by the set of
//! rows the base is out of date about. Which means the interesting failure is no
//! longer "it is slow" but "it answers with a value the row no longer holds" —
//! and that is silent.
//!
//! So every question here is asked twice against identical data: once of a
//! collection with an index, once of one without, where only a scan can answer.
//! An index is allowed to change the speed. It is not allowed to change the
//! answer.

use sekejap::CoreDB;
use serde_json::Value;

const N: usize = 2_000;

/// Sorted `_key`s of every row a query returns, so two plans compare directly.
fn keys(db: &CoreDB, sql: &str) -> Vec<String> {
    let hits = db
        .query(sql)
        .unwrap_or_else(|e| panic!("`{sql}` did not run: {e:?}"))
        .collect();
    let mut out: Vec<String> = hits
        .iter()
        .filter_map(|h| {
            h.payload
                .as_ref()
                .and_then(|p| p.get("_key"))
                .and_then(|k| k.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    out.sort();
    out
}

fn scalar(db: &CoreDB, sql: &str, key: &str) -> Value {
    db.query(sql)
        .unwrap_or_else(|e| panic!("`{sql}` did not run: {e:?}"))
        .collect()
        .first()
        .and_then(|h| h.payload.clone())
        .and_then(|p| p.get(key).cloned())
        .unwrap_or(Value::Null)
}

/// Ask the same question of the indexed collection and the unindexed one.
fn both(db: &CoreDB, sql_with_placeholder: &str) {
    let indexed = keys(db, &sql_with_placeholder.replace("{}", "idx"));
    let scanned = keys(db, &sql_with_placeholder.replace("{}", "plain"));
    assert_eq!(
        indexed, scanned,
        "index and scan disagree for `{sql_with_placeholder}` \
         ({} rows indexed vs {} scanned)",
        indexed.len(),
        scanned.len()
    );
}

/// The `n` values a query returns, **in result order**.
///
/// `ORDER BY n` over data with ties has no total order, so which of the tied
/// rows comes back is arbitrary and an index is entitled to pick differently
/// from a scan — PostgreSQL makes no promise there either. What both must agree
/// on is the sequence of values, which is what actually encodes the ordering.
fn ordered_ns(db: &CoreDB, sql: &str) -> Vec<Value> {
    db.query(sql)
        .unwrap_or_else(|e| panic!("`{sql}` did not run: {e:?}"))
        .collect()
        .iter()
        .map(|h| {
            h.payload
                .as_ref()
                .and_then(|p| p.get("n"))
                .cloned()
                .unwrap_or(Value::Null)
        })
        .collect()
}

fn same_order(db: &CoreDB, sql: &str) {
    let a = ordered_ns(db, &sql.replace("{}", "idx"));
    let b = ordered_ns(db, &sql.replace("{}", "plain"));
    assert_eq!(a, b, "index and scan return a different ordering for `{sql}`");
}

fn agrees(db: &CoreDB, sql: &str, col: &str) {
    let a = scalar(db, &sql.replace("{}", "idx"), col);
    let b = scalar(db, &sql.replace("{}", "plain"), col);
    assert_eq!(a, b, "index and scan disagree for `{sql}`");
}

/// Identical rows in two collections; only `idx` is indexed. The compaction and
/// reopen are the point: afterwards the index is served from the mapping, so
/// every write below lands in the delta rather than in a heap copy.
fn fixture() -> (tempfile::TempDir, CoreDB) {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        for i in 0..N {
            let v = i % 100;
            db.put(
                &format!("idx/n{i}"),
                &format!(r#"{{"_collection":"idx","_key":"n{i}","n":{v}}}"#),
            )
            .unwrap();
            db.put(
                &format!("plain/n{i}"),
                &format!(r#"{{"_collection":"plain","_key":"n{i}","n":{v}}}"#),
            )
            .unwrap();
        }
        db.execute("CREATE INDEX ON idx USING btree (n)").unwrap();
        db.compact().unwrap();
    }
    let db = CoreDB::open(dir.path()).unwrap();
    (dir, db)
}

/// Apply the same change to both collections.
fn write(db: &mut CoreDB, i: usize, n: i64) {
    for coll in ["idx", "plain"] {
        db.put(
            &format!("{coll}/n{i}"),
            &format!(r#"{{"_collection":"{coll}","_key":"n{i}","n":{n}}}"#),
        )
        .unwrap();
    }
}

/// The whole battery, so each test can re-run it after a compaction or reopen.
fn check_all(db: &CoreDB) {
    both(db, "SELECT * FROM {} WHERE n = 9999");
    both(db, "SELECT * FROM {} WHERE n = 0");
    both(db, "SELECT * FROM {} WHERE n = 7");
    both(db, "SELECT * FROM {} WHERE n > 90");
    both(db, "SELECT * FROM {} WHERE n < 5");
    both(db, "SELECT * FROM {} WHERE n >= 50 AND n <= 60");
    same_order(db, "SELECT * FROM {} ORDER BY n LIMIT 25");
    same_order(db, "SELECT * FROM {} ORDER BY n DESC LIMIT 25");
    same_order(db, "SELECT * FROM {} ORDER BY n");
    agrees(db, "SELECT MIN(n) FROM {}", "min");
    agrees(db, "SELECT MAX(n) FROM {}", "max");
    agrees(db, "SELECT COUNT(n) FROM {}", "count");
    agrees(db, "SELECT SUM(n) FROM {}", "sum");
}

#[test]
fn a_write_after_reopen_moves_the_row_in_the_index() {
    let (_d, mut db) = fixture();
    // Move the first fifty rows to a value nothing else holds. The base still
    // files them under their old `n`, so `WHERE n = 0` is the sharp case: it must
    // lose exactly the rows that moved and keep the rest.
    for i in 0..50 {
        write(&mut db, i, 9999);
    }
    check_all(&db);
}

#[test]
fn a_delete_after_reopen_leaves_the_index() {
    let (_d, mut db) = fixture();
    db.execute("DELETE FROM idx WHERE n = 7").unwrap();
    db.execute("DELETE FROM plain WHERE n = 7").unwrap();
    // The base still lists every deleted row under 7.
    both(&db, "SELECT * FROM {} WHERE n = 7");
    both(&db, "SELECT * FROM {} WHERE n >= 5 AND n <= 9");
    agrees(&db, "SELECT COUNT(n) FROM {}", "count");
    agrees(&db, "SELECT SUM(n) FROM {}", "sum");
}

#[test]
fn a_row_written_back_is_not_counted_twice() {
    let (_d, mut db) = fixture();
    // Delete and re-write the same key. The slug hashes to the same id, so a
    // posting list that still holds the base's copy would now hold two.
    db.execute("DELETE FROM idx WHERE n = 3").unwrap();
    db.execute("DELETE FROM plain WHERE n = 3").unwrap();
    for i in (3..N).step_by(100) {
        write(&mut db, i, 3);
    }
    both(&db, "SELECT * FROM {} WHERE n = 3");
    agrees(&db, "SELECT COUNT(n) FROM {}", "count");
    agrees(&db, "SELECT SUM(n) FROM {}", "sum");
}

#[test]
fn the_delta_survives_compaction_and_reopen() {
    let (dir, mut db) = fixture();
    for i in 0..50 {
        write(&mut db, i, 9999);
    }
    db.execute("DELETE FROM idx WHERE n = 11").unwrap();
    db.execute("DELETE FROM plain WHERE n = 11").unwrap();
    check_all(&db);

    // Folding the delta into a new base must not change a single answer. This is
    // where writing the delta straight to the sidecar — rather than the merged
    // view — would drop every row the base held and show up as silence.
    db.compact().unwrap();
    check_all(&db);

    drop(db);
    let db = CoreDB::open(dir.path()).unwrap();
    check_all(&db);
}

#[test]
fn a_new_row_is_findable_through_the_index() {
    let (_d, mut db) = fixture();
    // Not in the base at all — the delta is the only place it exists.
    write(&mut db, N + 1, 12345);
    both(&db, "SELECT * FROM {} WHERE n = 12345");
    agrees(&db, "SELECT MAX(n) FROM {}", "max");
}

#[test]
fn renaming_a_collection_carries_the_whole_index() {
    let (_d, mut db) = fixture();
    for i in 0..50 {
        write(&mut db, i, 9999);
    }
    // The delta, the mapped base and the staleness set are all keyed by the
    // collection hash. Moving only the delta leaves the base filed under the old
    // name, and the merged read then answers out of a delta with nothing behind
    // it — fifty rows where there should be two thousand, and no error.
    db.execute("ALTER TABLE idx RENAME TO idx2").unwrap();
    db.execute("ALTER TABLE plain RENAME TO plain2").unwrap();

    let indexed = keys(&db, "SELECT * FROM idx2");
    let scanned = keys(&db, "SELECT * FROM plain2");
    assert_eq!(indexed.len(), N, "rename lost rows from the indexed collection");
    assert_eq!(indexed, scanned);

    for q in [
        "SELECT * FROM {} WHERE n = 9999",
        "SELECT * FROM {} WHERE n = 0",
        "SELECT * FROM {} WHERE n > 90",
    ] {
        let a = keys(&db, &q.replace("{}", "idx2"));
        let b = keys(&db, &q.replace("{}", "plain2"));
        assert_eq!(a, b, "after rename, index and scan disagree for `{q}`");
    }
}

#[test]
fn updating_one_column_leaves_the_other_index_intact() {
    // Caught by `differential_audit::mixed_base_and_overlay`, as `SUM` off by
    // exactly the value of the row that was touched.
    //
    // A whole-row write replaces every one of a row's indexed values at once, so
    // all of them go stale together. A partial `UPDATE` does not: it re-files the
    // row only under the columns it changed. Marking the row stale across every
    // index of the collection therefore withdrew it from the indexes nobody
    // touched — the base was told to ignore the row, and no delta entry replaced
    // it. The row still existed, still matched a scan, and had vanished from the
    // index.
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        for i in 0..200 {
            db.put(
                &format!("t/n{i}"),
                &format!(r#"{{"_collection":"t","_key":"n{i}","n":{i},"name":"a{i}"}}"#),
            )
            .unwrap();
        }
        db.execute("CREATE INDEX ON t USING btree (n)").unwrap();
        db.execute("CREATE INDEX ON t USING btree (name)").unwrap();
        db.compact().unwrap();
    }
    // Reopen so both indexes are served from the mapping.
    let mut db = CoreDB::open(dir.path()).unwrap();
    let before = scalar(&db, "SELECT SUM(n) FROM t", "sum");

    db.execute("UPDATE t SET name = 'renamed' WHERE n = 3").unwrap();

    assert_eq!(
        scalar(&db, "SELECT SUM(n) FROM t", "sum"),
        before,
        "changing `name` withdrew the row from the untouched `n` index"
    );
    assert_eq!(
        keys(&db, "SELECT * FROM t WHERE n = 3").len(),
        1,
        "the row is gone from the `n` index it was never removed from"
    );
    assert_eq!(
        keys(&db, "SELECT * FROM t WHERE name = 'renamed'").len(),
        1,
        "the column that actually changed is not findable under its new value"
    );
}
