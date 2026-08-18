//! A prepared query answers what the same query answers written out.
//!
//! `prepare` + `query_prepared` caches the tokens and re-lowers them with fresh
//! `$N` values, and `query_params` binds without the cache. Three routes to one
//! answer, and parameter binding is a classic place for them to part company —
//! a value bound in the wrong position, a type coerced differently, a `$1`
//! reused across two clauses.
//!
//! Three tests in the whole suite touched `prepare`, none comparing it against
//! the written-out form. This does that across the awkward values: nulls, empty
//! strings, whole floats against integers, negatives, text that looks numeric,
//! and strings carrying quotes.

use sekejap::CoreDB;
use serde_json::{json, Value};

fn fixture() -> (tempfile::TempDir, CoreDB) {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, v TEXT, w INTEGER)").unwrap();
    let rows: &[(&str, Value, Value)] = &[
        ("a", json!("kucing"), json!(1)),
        ("b", json!("kucing"), json!(2)),
        ("c", json!(""), json!(0)),
        ("d", json!("3"), json!(3)),
        ("e", Value::Null, json!(-1)),
        ("f", json!("it's \"quoted\""), json!(1)),
        ("g", json!("kucing"), json!(1.0)),
    ];
    for (k, v, w) in rows {
        let mut o = serde_json::Map::new();
        o.insert("_collection".into(), json!("p"));
        o.insert("_key".into(), json!(k));
        if !v.is_null() { o.insert("v".into(), v.clone()); }
        o.insert("w".into(), w.clone());
        db.put(&format!("p/{k}"), &Value::Object(o).to_string()).unwrap();
    }
    db.execute("CREATE INDEX ON p USING btree (w)").unwrap();
    db.compact().unwrap();
    (dir, db)
}

fn keys(hits: Vec<sekejap::Hit>) -> String {
    let mut v: Vec<String> = hits.iter().map(|h| h.slug.clone()).collect();
    v.sort();
    v.join(",")
}

/// `(parameterised sql, params, the same thing written out)`.
fn cases() -> Vec<(&'static str, Vec<Value>, String)> {
    vec![
        ("SELECT _key FROM p WHERE v = $1", vec![json!("kucing")],
         "SELECT _key FROM p WHERE v = 'kucing'".into()),
        ("SELECT _key FROM p WHERE v = $1", vec![json!("")],
         "SELECT _key FROM p WHERE v = ''".into()),
        ("SELECT _key FROM p WHERE v = $1", vec![json!("3")],
         "SELECT _key FROM p WHERE v = '3'".into()),
        ("SELECT _key FROM p WHERE v != $1", vec![json!("kucing")],
         "SELECT _key FROM p WHERE v != 'kucing'".into()),
        ("SELECT _key FROM p WHERE w = $1", vec![json!(1)],
         "SELECT _key FROM p WHERE w = 1".into()),
        ("SELECT _key FROM p WHERE w = $1", vec![json!(-1)],
         "SELECT _key FROM p WHERE w = -1".into()),
        ("SELECT _key FROM p WHERE w > $1", vec![json!(0)],
         "SELECT _key FROM p WHERE w > 0".into()),
        ("SELECT _key FROM p WHERE w BETWEEN $1 AND $2", vec![json!(0), json!(2)],
         "SELECT _key FROM p WHERE w BETWEEN 0 AND 2".into()),
        // Two parameters, so a swapped binding shows.
        ("SELECT _key FROM p WHERE v = $1 AND w = $2", vec![json!("kucing"), json!(1)],
         "SELECT _key FROM p WHERE v = 'kucing' AND w = 1".into()),
        ("SELECT _key FROM p WHERE v LIKE $1", vec![json!("k%")],
         "SELECT _key FROM p WHERE v LIKE 'k%'".into()),
        ("SELECT _key FROM p ORDER BY w ASC, _key ASC LIMIT $1", vec![json!(3)],
         "SELECT _key FROM p ORDER BY w ASC, _key ASC LIMIT 3".into()),
    ]
}

#[test]
fn a_prepared_query_answers_what_the_written_out_query_answers() {
    let (_dir, db) = fixture();
    for (sql, params, literal) in cases() {
        let want = keys(db.query(&literal)
            .unwrap_or_else(|e| panic!("`{literal}` did not run: {e:?}")).collect());

        let prepared = db.prepare(sql)
            .unwrap_or_else(|e| panic!("`{sql}` did not prepare: {e:?}"));
        let got = keys(db.query_prepared(&prepared, &params)
            .unwrap_or_else(|e| panic!("`{sql}` did not run prepared: {e:?}")).collect());
        assert_eq!(got, want,
            "prepared `{sql}` with {params:?} answered [{got}], but the same query \
             written out answered [{want}]");

        let got = keys(db.query_params(sql, &params)
            .unwrap_or_else(|e| panic!("`{sql}` did not run with params: {e:?}")).collect());
        assert_eq!(got, want,
            "`query_params` on `{sql}` with {params:?} answered [{got}], but the \
             same query written out answered [{want}]");
    }
}

/// A prepared query is meant to be **reused** with different values. Running one
/// twice with different parameters must give the two answers those parameters
/// call for — not the first answer twice, which is what a cache keyed on the
/// wrong thing would do.
#[test]
fn a_prepared_query_can_be_reused_with_different_values() {
    let (_dir, db) = fixture();
    let p = db.prepare("SELECT _key FROM p WHERE w = $1").unwrap();

    let first = keys(db.query_prepared(&p, &[json!(1)]).unwrap().collect());
    let second = keys(db.query_prepared(&p, &[json!(2)]).unwrap().collect());
    let first_again = keys(db.query_prepared(&p, &[json!(1)]).unwrap().collect());

    assert_eq!(first, keys(db.query("SELECT _key FROM p WHERE w = 1").unwrap().collect()));
    assert_eq!(second, keys(db.query("SELECT _key FROM p WHERE w = 2").unwrap().collect()));
    assert_ne!(first, second, "two different parameters gave the same answer");
    assert_eq!(first, first_again,
               "re-running with the original parameter did not give the original \
                answer — the prepared query is carrying state between runs");
}
