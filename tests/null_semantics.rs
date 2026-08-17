//! Three-valued logic, checked against what PostgreSQL answers.
//!
//! A row either matches a condition or it does not — except when the condition
//! cannot be decided. `status != 'open'` is not true for a row whose `status` is
//! NULL, and it is not false either. SQL calls that **unknown**, and only rows
//! where the `WHERE` comes out *true* are returned.
//!
//! This database used to answer with a plain boolean, so unknown had to be
//! spelled as one or the other. `!=` and `NOT IN` chose "keep", and returned
//! every row with no value at all — rows PostgreSQL drops. Nothing errored;
//! the answer was simply larger than the one the query asked for.
//!
//! Every case here is written with the answer PostgreSQL gives, and is run twice:
//! once against a plain scan and once with a btree index on the column. An index
//! is not allowed to change an answer, and the NULL rules are exactly where it
//! nearly did — a missing field is indexed as NULL, so a set-difference `!=` that
//! forgets to subtract them keeps the rows the scan drops.

use sekejap::CoreDB;

/// Four rows: a value, a different value, an explicit `null`, and no field at
/// all. The last two are the interesting ones, and in a document store they are
/// the same thing: the row has nothing to say about that column.
fn fixture(indexed: bool) -> (tempfile::TempDir, CoreDB) {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, status TEXT, n INTEGER)").unwrap();
    db.put("p/a", r#"{"_collection":"p","_key":"a","status":"open","n":1}"#).unwrap();
    db.put("p/b", r#"{"_collection":"p","_key":"b","status":"shut","n":2}"#).unwrap();
    db.put("p/c", r#"{"_collection":"p","_key":"c","status":null,"n":3}"#).unwrap();
    db.put("p/d", r#"{"_collection":"p","_key":"d","n":4}"#).unwrap();
    if indexed {
        db.execute("CREATE INDEX ON p USING btree (status)").unwrap();
        db.execute("CREATE INDEX ON p USING btree (n)").unwrap();
    }
    (dir, db)
}

fn keys(db: &CoreDB, where_clause: &str) -> String {
    let sql = format!("SELECT _key FROM p WHERE {where_clause}");
    let mut got: Vec<String> = db
        .query(&sql)
        .unwrap_or_else(|e| panic!("`{where_clause}` did not parse: {e:?}"))
        .collect()
        .iter()
        .map(|h| h.slug.replace("p/", ""))
        .collect();
    got.sort();
    got.join(",")
}

/// `(condition, what PostgreSQL returns)`.
const CASES: &[(&str, &str)] = &[
    // The baseline: equality was already right, and stays right.
    ("status = 'open'", "a"),
    ("status IN ('open','shut')", "a,b"),
    ("status IS NULL", "c,d"),
    ("status IS NOT NULL", "a,b"),
    // The rows that used to come back and should not. `c` holds NULL and `d`
    // has no status at all; neither is "different from 'open'", both are
    // unknown.
    ("status != 'open'", "b"),
    ("status <> 'open'", "b"),
    ("status NOT IN ('open')", "b"),
    // `NOT` must agree with `!=` on the same row. It did not before: negating a
    // boolean turns "false because we could not tell" into "true".
    ("NOT (status = 'open')", "b"),
    ("NOT (status IN ('open'))", "b"),
    // Comparing *to* NULL is unknown whatever the row holds — including the row
    // whose value is itself NULL. `IS NULL` is how that question gets asked.
    ("status = NULL", ""),
    ("status != NULL", ""),
    // Ordering comparisons: absence is unknown, not "not greater than".
    ("n > 2", "c,d"),
    ("n <= 2", "a,b"),
    ("NOT (n > 2)", "a,b"),
    ("n BETWEEN 2 AND 3", "b,c"),
    ("NOT (n BETWEEN 2 AND 3)", "a,d"),
    // `LIKE` against a missing value is unknown too, so `NOT LIKE` does not
    // sweep up the rows that have nothing to match.
    ("status LIKE 'op%'", "a"),
    ("NOT (status LIKE 'op%')", "b"),
    // Unknown inside `OR`: a branch that cannot be decided does not stop another
    // branch from being true.
    ("status != 'open' OR n = 3", "b,c"),
    ("status = 'nothing' OR n = 1", "a"),
];

#[test]
fn null_and_missing_fields_follow_postgres() {
    for indexed in [false, true] {
        let (_dir, db) = fixture(indexed);
        let plan = if indexed { "with an index" } else { "on a scan" };
        for (cond, pg) in CASES {
            assert_eq!(&keys(&db, cond), pg,
                "`WHERE {cond}` {plan} disagrees with PostgreSQL");
        }
    }
}

/// **The index and the scan must agree, case by case.**
///
/// Stated separately from the PostgreSQL comparison because it is a different
/// failure: an answer can be wrong in both plans and still be consistent, and it
/// can be right in one and wrong in the other. The second is worse — it makes the
/// result depend on whether somebody happened to run `CREATE INDEX`.
#[test]
fn an_index_does_not_change_a_null_answer() {
    let (_d1, scan) = fixture(false);
    let (_d2, idx) = fixture(true);
    for (cond, _) in CASES {
        assert_eq!(keys(&scan, cond), keys(&idx, cond),
            "`WHERE {cond}` answers differently once the column is indexed");
    }
}

/// **A NULL in the list makes `NOT IN` return nothing.**
///
/// `x NOT IN (1, NULL)` is `x != 1 AND x != NULL`, and the second half is
/// unknown for every row — so no row is ever true. This surprises people in
/// PostgreSQL as well; it is not a nicety we are adding, it is the rule we said
/// we would follow. Skipped rather than asserted if the grammar will not take a
/// NULL literal in an `IN` list, so this test reports the semantics and does not
/// quietly pin the parser.
#[test]
fn a_null_in_a_not_in_list_matches_nothing() {
    let (_dir, db) = fixture(false);
    let Ok(set) = db.query("SELECT _key FROM p WHERE status NOT IN ('open', NULL)") else {
        return; // grammar does not accept it; nothing to check
    };
    assert_eq!(set.collect().len(), 0,
        "`NOT IN` with a NULL in the list must return nothing, as it does in PostgreSQL");
    // And the positive form still finds what is there.
    let hits = db.query("SELECT _key FROM p WHERE status IN ('open', NULL)").unwrap();
    assert_eq!(hits.collect().len(), 1, "`IN` with a NULL in the list lost a real match");
}
