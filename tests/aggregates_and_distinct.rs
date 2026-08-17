//! Aggregates, `DISTINCT`, and what the parser refuses — against PostgreSQL.
//!
//! These share a failure mode with the `ORDER BY` and `LIKE` bugs: the query ran,
//! returned something plausible, and was wrong. Nothing errored.
//!
//! * an aggregate over zero rows produced **no column at all** — `SELECT COUNT(n)`
//!   came back as `{}` rather than `0`, and adding a second aggregate made even
//!   `COUNT(*)` disappear
//! * `SUM` over nothing answered `0`, which is a claim about the data rather than
//!   an admission that there was none
//! * `MIN`/`MAX` read only numbers, so over a text column they answered NULL
//! * `COUNT` of a text column answered `0` — but only once the column was
//!   indexed, because the index path had its own numeric-only copy of the
//!   accumulator
//! * `DISTINCT` treated an explicit NULL and a missing field as two values
//! * the parser stopped at whatever it understood and dropped the rest in
//!   silence, so `ORDER BY n ASC NULLS LAST` returned the plain `ASC` order and
//!   `WHERE a > 1 GARBAGE` ran
//!
//! Everything is checked on a scan and again with an index, because four of the
//! six were index-only or scan-only.

use sekejap::CoreDB;
use serde_json::Value;

/// The first row's payload, which is where a single-row aggregate lands.
fn row(db: &CoreDB, sql: &str) -> Value {
    db.query(sql)
        .unwrap_or_else(|e| panic!("`{sql}` did not run: {e:?}"))
        .collect()
        .first()
        .and_then(|h| h.payload.clone())
        .unwrap_or(Value::Null)
}

fn field(db: &CoreDB, sql: &str, key: &str) -> Value {
    row(db, sql).get(key).cloned().unwrap_or(Value::Null)
}

/// `n` is 1, 2, 3 and absent; `name` is three names and one missing.
fn fixture(indexed: bool) -> (tempfile::TempDir, CoreDB) {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER, name TEXT)").unwrap();
    db.put("p/a", r#"{"_collection":"p","_key":"a","n":1,"name":"bob"}"#).unwrap();
    db.put("p/b", r#"{"_collection":"p","_key":"b","n":2,"name":"amy"}"#).unwrap();
    db.put("p/c", r#"{"_collection":"p","_key":"c","n":3,"name":"zed"}"#).unwrap();
    db.put("p/d", r#"{"_collection":"p","_key":"d","name":null}"#).unwrap();
    db.execute("CREATE TABLE e (_key TEXT PRIMARY KEY, n INTEGER)").unwrap();
    db.put("e/x", r#"{"_collection":"e","_key":"x","n":1}"#).unwrap();
    if indexed {
        db.execute("CREATE INDEX ON p USING btree (n)").unwrap();
        db.execute("CREATE INDEX ON p USING btree (name)").unwrap();
        db.execute("CREATE INDEX ON e USING btree (n)").unwrap();
    }
    (dir, db)
}

#[test]
fn aggregates_follow_postgres() {
    for indexed in [false, true] {
        let (_dir, db) = fixture(indexed);
        let plan = if indexed { "with an index" } else { "on a scan" };

        // ── over rows that exist ────────────────────────────────────────────
        assert_eq!(field(&db, "SELECT COUNT(*) FROM p", "count"), Value::from(4),
            "COUNT(*) {plan} must count every row");
        assert_eq!(field(&db, "SELECT COUNT(n) FROM p", "count"), Value::from(3),
            "COUNT(col) {plan} must skip the rows with no value");
        assert_eq!(field(&db, "SELECT COUNT(name) FROM p", "count"), Value::from(3),
            "COUNT of a text column {plan} answered as if the column were empty");

        // A sum of whole numbers is a whole number. `6.0` is a type the data
        // never had, and past 2^53 it stops being the right number at all.
        assert_eq!(field(&db, "SELECT SUM(n) FROM p", "sum"), Value::from(6),
            "SUM {plan} must keep an integer column integral");
        assert_eq!(field(&db, "SELECT MIN(n) FROM p", "min"), Value::from(1));
        assert_eq!(field(&db, "SELECT MAX(n) FROM p", "max"), Value::from(3));
        assert_eq!(field(&db, "SELECT AVG(n) FROM p", "avg"), Value::from(2.0));

        // MIN/MAX are not numeric-only: they order by the same rule ORDER BY does.
        assert_eq!(field(&db, "SELECT MIN(name) FROM p", "min"), Value::from("amy"),
            "MIN of a text column {plan} answered NULL");
        assert_eq!(field(&db, "SELECT MAX(name) FROM p", "max"), Value::from("zed"),
            "MAX of a text column {plan} answered NULL");

        // ── over no rows at all ─────────────────────────────────────────────
        //
        // The column has to be there. A caller reading `row["count"]` gets a
        // missing key otherwise, which is indistinguishable from a typo.
        assert_eq!(field(&db, "SELECT COUNT(n) FROM e WHERE n > 100", "count"),
            Value::from(0), "COUNT over no rows {plan} must be 0, and must be present");
        assert_eq!(field(&db, "SELECT SUM(n) FROM e WHERE n > 100", "sum"),
            Value::Null, "SUM over no rows {plan} must be NULL, not 0");
        assert_eq!(field(&db, "SELECT MIN(n) FROM e WHERE n > 100", "min"), Value::Null);
        assert_eq!(field(&db, "SELECT MAX(n) FROM e WHERE n > 100", "max"), Value::Null);
        assert_eq!(field(&db, "SELECT AVG(n) FROM e WHERE n > 100", "avg"), Value::Null);

        // Two aggregates together: the second used to take the first with it.
        let both = row(&db, "SELECT COUNT(*), SUM(n) FROM e WHERE n > 100");
        assert_eq!(both.get("count").cloned().unwrap_or(Value::Null), Value::from(0),
            "COUNT(*) disappeared {plan} when a second aggregate was added");
        assert_eq!(both.get("sum").cloned().unwrap_or(Value::Null), Value::Null);
    }
}

/// **An index may not change an aggregate.** The index path had its own copy of
/// the accumulator that only understood numbers, so this is where `COUNT(name)`
/// and `MIN(name)` diverged.
#[test]
fn an_index_does_not_change_an_aggregate() {
    let (_d1, scan) = fixture(false);
    let (_d2, idx) = fixture(true);
    for sql in [
        "SELECT COUNT(*) FROM p",
        "SELECT COUNT(n) FROM p",
        "SELECT COUNT(name) FROM p",
        "SELECT SUM(n) FROM p",
        "SELECT MIN(n) FROM p",
        "SELECT MAX(n) FROM p",
        "SELECT AVG(n) FROM p",
        "SELECT MIN(name) FROM p",
        "SELECT MAX(name) FROM p",
        "SELECT SUM(n) FROM e WHERE n > 100",
        "SELECT COUNT(n) FROM e WHERE n > 100",
    ] {
        assert_eq!(row(&scan, sql), row(&idx, sql),
            "`{sql}` answers differently once the column is indexed");
    }
}

/// An explicit NULL and a missing field are one value, so `DISTINCT` returns
/// them once. Serializing the projected row made `{"s":null}` and `{}` two
/// different strings.
#[test]
fn distinct_treats_null_and_missing_as_one_value() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE d (_key TEXT PRIMARY KEY, s TEXT)").unwrap();
    db.put("d/1", r#"{"_collection":"d","_key":"1","s":"x"}"#).unwrap();
    db.put("d/2", r#"{"_collection":"d","_key":"2","s":"x"}"#).unwrap();
    db.put("d/3", r#"{"_collection":"d","_key":"3","s":null}"#).unwrap();
    db.put("d/4", r#"{"_collection":"d","_key":"4","s":null}"#).unwrap();
    db.put("d/5", r#"{"_collection":"d","_key":"5"}"#).unwrap();
    db.put("d/6", r#"{"_collection":"d","_key":"6"}"#).unwrap();

    let n = db.query("SELECT DISTINCT s FROM d").unwrap().collect().len();
    assert_eq!(n, 2, "DISTINCT must return two values — 'x' and NULL — not three");
}

/// **A clause the parser does not understand must be an error, not a shrug.**
///
/// This is the one that turns a wrong answer into a question the user can act
/// on. Someone who notices NULLs in the wrong place and writes `NULLS LAST` to
/// fix it deserves to be told the clause is not supported, rather than handed
/// the same order back.
#[test]
fn the_parser_refuses_what_it_cannot_apply() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE q (_key TEXT PRIMARY KEY, a INTEGER)").unwrap();
    db.put("q/x", r#"{"_collection":"q","_key":"x","a":1}"#).unwrap();

    for sql in [
        "SELECT _key FROM q ORDER BY a ASC NULLS LAST",
        "SELECT _key FROM q ORDER BY a ASC BANANA",
        "SELECT _key FROM q WHERE a > 1 GARBAGE HERE",
        "SELECT _key FROM q ORDER BY a DESC LIMIT 1 OFFSET 1 EXTRA",
    ] {
        assert!(db.query(sql).is_err(),
            "`{sql}` was accepted; the part that was not understood is being ignored");
    }

    // And nothing legitimate was caught by the same net.
    for sql in [
        "SELECT _key FROM q",
        "SELECT _key FROM q WHERE a > 0",
        "SELECT _key FROM q ORDER BY a ASC",
        "SELECT _key FROM q ORDER BY a DESC LIMIT 1 OFFSET 0",
        "SELECT COUNT(*) FROM q",
        "SELECT _key FROM q;",
    ] {
        assert!(db.query(sql).is_ok(), "`{sql}` is valid and was refused");
    }
}

/// **A mutation that is not understood must not run.**
///
/// This is the same defect as the `SELECT` case above and a far worse outcome.
/// `DELETE FROM p GARBAGE WHERE n = 1` parsed as `DELETE FROM p`: the parser
/// looked for `WHERE`, found an identifier, concluded there was no predicate, and
/// deleted **every row in the table**. The rest of the statement was discarded
/// without a word.
///
/// A typo between the table name and the predicate was the difference between
/// removing one row and emptying the collection, and nothing in the result told
/// the caller which had happened.
#[test]
fn a_mutation_that_is_not_understood_does_not_run() {
    let refill = |db: &mut CoreDB| {
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER)").ok();
        for i in 0..5 {
            db.put(&format!("p/n{i}"),
                   &format!(r#"{{"_collection":"p","_key":"n{i}","n":{i}}}"#)).unwrap();
        }
    };
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();

    for sql in [
        "DELETE FROM p GARBAGE WHERE n = 1",
        "DELETE FROM p WHERE n = 1 EXTRA",
        "UPDATE p SET n = 9 GARBAGE WHERE n = 1",
    ] {
        refill(&mut db);
        let before = db.query("SELECT _key FROM p").unwrap().collect().len();
        assert!(db.execute(sql).is_err(), "`{sql}` was accepted");
        let after = db.query("SELECT _key FROM p").unwrap().collect().len();
        assert_eq!(after, before,
            "`{sql}` was refused but still changed the table: {before} rows -> {after}");
    }

    // The statements it is meant to accept still work.
    refill(&mut db);
    db.execute("DELETE FROM p WHERE n = 1").unwrap();
    assert_eq!(db.query("SELECT _key FROM p").unwrap().collect().len(), 4,
        "a plain DELETE with a predicate must still delete exactly its rows");
    db.execute("UPDATE p SET n = 99 WHERE n = 2").unwrap();
    assert_eq!(db.query("SELECT _key FROM p WHERE n = 99").unwrap().collect().len(), 1);
}
