//! `ORDER BY` and `LIKE`, checked against what PostgreSQL answers.
//!
//! Both had the same shape of defect and the same failure mode: a query that
//! looked like it asked one thing quietly answered another, with no error and
//! nothing in the result to say so.
//!
//! **`ORDER BY`** compared values of different types as *equal* — NULL included.
//! That is not a weaker ordering but an inconsistent one, and `sort_by` is
//! entitled to do anything when it does not get a total order. With rows
//! `3, NULL, 1` it left them where they were: `ASC` and `DESC` returned the same
//! sequence, and `ASC LIMIT 1` returned the largest value. One NULL corrupted the
//! order of every non-NULL row around it.
//!
//! **`LIKE`** stripped the `%` signs off the pattern and checked that the rest
//! appeared somewhere in the text. That is `contains`, not `LIKE`: `'reopened'
//! LIKE 'open'` was true, `_` was a literal underscore, there was no escape
//! character, and the pattern was trimmed of whitespace.
//!
//! Every case is run twice — once on a plain scan, once with an index on the
//! column — because both features had *five* code paths between them and the
//! index disagreed with the scan on nearly all of them.

use sekejap::{Config, CoreDB};

fn ordered(db: &CoreDB, sql: &str) -> String {
    db.query(sql)
        .unwrap_or_else(|e| panic!("`{sql}` did not run: {e:?}"))
        .collect()
        .iter()
        .map(|h| h.slug.split('/').nth(1).unwrap_or("?").to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn keys(db: &CoreDB, sql: &str) -> String {
    let mut v: Vec<String> = db
        .query(sql)
        .unwrap_or_else(|e| panic!("`{sql}` did not run: {e:?}"))
        .collect()
        .iter()
        .map(|h| h.slug.split('/').nth(1).unwrap_or("?").to_string())
        .collect();
    v.sort();
    v.join(",")
}

/// `b` is `3`, `NULL`, `1`, and absent — inserted in that order, so a comparator
/// that gives up and leaves the rows alone is visible as insertion order.
fn fixture(indexed: bool) -> (tempfile::TempDir, CoreDB) {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE s (_key TEXT PRIMARY KEY, b INTEGER)").unwrap();
    db.put("s/p", r#"{"_collection":"s","_key":"p","b":3}"#).unwrap();
    db.put("s/q", r#"{"_collection":"s","_key":"q","b":null}"#).unwrap();
    db.put("s/r", r#"{"_collection":"s","_key":"r","b":1}"#).unwrap();
    db.put("s/t", r#"{"_collection":"s","_key":"t"}"#).unwrap();
    db.execute("CREATE TABLE t (_key TEXT PRIMARY KEY, v TEXT)").unwrap();
    for (k, v) in [("1", "open"), ("2", "reopened"), ("3", "foo"), ("4", ""),
                   ("5", "100%"), ("6", "a_b"), ("7", "axb")] {
        db.put(&format!("t/{k}"),
               &format!(r#"{{"_collection":"t","_key":"{k}","v":"{v}"}}"#)).unwrap();
    }
    if indexed {
        db.execute("CREATE INDEX ON s USING btree (b)").unwrap();
        db.execute("CREATE INDEX ON t USING gin (v)").unwrap();
    }
    (dir, db)
}

/// `(query, what PostgreSQL returns)`. Rows `q` and `t` are both NULL, so they
/// tie; either order between them is correct and the check allows both.
const ORDERS: &[(&str, &str)] = &[
    ("SELECT _key FROM s ORDER BY b ASC", "r,p,q,t"),
    ("SELECT _key FROM s ORDER BY b DESC", "q,t,p,r"),
    ("SELECT _key FROM s ORDER BY b ASC LIMIT 1", "r"),
    ("SELECT _key FROM s ORDER BY b DESC LIMIT 1", "q"),
    ("SELECT _key FROM s ORDER BY b ASC LIMIT 2", "r,p"),
    ("SELECT _key FROM s ORDER BY b ASC LIMIT 2 OFFSET 1", "p,q"),
    ("SELECT _key, b FROM s ORDER BY b ASC", "r,p,q,t"),
    ("SELECT * FROM s ORDER BY b ASC", "r,p,q,t"),
];

/// `(condition, what PostgreSQL returns)`.
const PATTERNS: &[(&str, &str)] = &[
    ("v LIKE 'open'", "1"),
    ("v LIKE 'o'", ""),
    ("v LIKE 'o%'", "1"),
    ("v LIKE '%o%'", "1,2,3"),
    ("v LIKE '%o'", "3"),
    ("v LIKE ''", "4"),
    ("v LIKE '%'", "1,2,3,4,5,6,7"),
    ("v LIKE '_pen'", "1"),
    ("v LIKE 'a_b'", "6,7"),
    ("v LIKE '____'", "1,5"),   // four characters: `open` and `100%`
    ("v LIKE '100\\%'", "5"),
    ("v LIKE ' open'", ""),
    ("v ILIKE 'OPEN'", "1"),
    ("v ILIKE '%OPEN%'", "1,2"),
    ("v ILIKE 'RE%ED'", "2"),
];

#[test]
fn order_by_and_like_follow_postgres() {
    for indexed in [false, true] {
        let (_dir, db) = fixture(indexed);
        let plan = if indexed { "with an index" } else { "on a scan" };
        for (sql, pg) in ORDERS {
            let got = ordered(&db, sql);
            // `q` and `t` are the two NULL rows and are interchangeable in every
            // one of these orderings, so both are folded to the same marker
            // before comparing. Everything else — where `r` and `p` land, and how
            // many rows come back — is still checked exactly.
            //
            // The previous tolerance swapped the literal text "q,t", which
            // covered the full listings and not `LIMIT 1`, where only one of the
            // pair appears. That went unnoticed while the tie order happened to
            // match; it stopped matching when ties were made deterministic by
            // node id, and the test then failed for a difference SQL does not
            // define.
            let fold = |s: &str| s.replace('q', "N").replace('t', "N");
            assert_eq!(fold(&got), fold(pg),
                "`{sql}` {plan} gave [{got}], PostgreSQL gives [{pg}]");
        }
        for (cond, pg) in PATTERNS {
            let sql = format!("SELECT _key FROM t WHERE {cond}");
            assert_eq!(&keys(&db, &sql), pg,
                "`WHERE {cond}` {plan} disagrees with PostgreSQL");
        }
    }
}

/// **An index may not change an answer.** Stated separately because it is a
/// different failure from disagreeing with PostgreSQL: a result can be wrong in
/// both plans and still be consistent, or right in one and wrong in the other.
/// The second is worse, because nothing in the query says which path it took.
///
/// This is where `ORDER BY` was worst. Five separate code paths ordered rows by
/// an index, each walking the btree raw — and `FieldKey::Null` is its lowest key,
/// so every one of them led with the rows that have no value while the scan put
/// them last.
#[test]
fn an_index_does_not_change_the_order_or_the_matches() {
    let (_d1, scan) = fixture(false);
    let (_d2, idx) = fixture(true);
    for (sql, _) in ORDERS {
        assert_eq!(ordered(&scan, sql), ordered(&idx, sql),
            "`{sql}` answers differently once the column is indexed");
    }
    for (cond, _) in PATTERNS {
        let sql = format!("SELECT _key FROM t WHERE {cond}");
        assert_eq!(keys(&scan, &sql), keys(&idx, &sql),
            "`WHERE {cond}` answers differently once the column is indexed");
    }
}

/// The comparator must be a *total order*, which is what `sort_by` requires and
/// what the old one was not. A mixed-type column is the case that broke it, and
/// the property being pinned is not any particular arrangement — it is that the
/// same rows come back in the same order however they arrive.
#[test]
fn the_sort_comparator_is_consistent_on_mixed_types() {
    let mut orders: Vec<String> = Vec::new();
    for seed in 0..6usize {
        let dir = tempfile::TempDir::new().unwrap();
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("CREATE TABLE m (_key TEXT PRIMARY KEY, v TEXT)").unwrap();
        let rows = [r#"5"#, r#""apple""#, r#"null"#, r#"true"#, r#"1.5"#, r#""zebra""#];
        // A different insertion order each round.
        for i in 0..rows.len() {
            let j = (i + seed) % rows.len();
            db.put(&format!("m/k{j}"),
                   &format!(r#"{{"_collection":"m","_key":"k{j}","v":{}}}"#, rows[j])).unwrap();
        }
        orders.push(ordered(&db, "SELECT _key FROM m ORDER BY v ASC"));
    }
    let first = &orders[0];
    for (i, o) in orders.iter().enumerate() {
        assert_eq!(o, first,
            "insertion order {i} produced [{o}], but order 0 produced [{first}] — \
             the comparator is not a total order");
    }
}

/// **A table name that names nothing is an error, not an empty table.**
///
/// `SELECT _key FROM custmers` returned no rows, which is exactly what an empty
/// `customers` table returns. The reply to a typo was a statement about data that
/// was never consulted, and the reader's next move is to go looking for missing
/// records rather than a missing letter. PostgreSQL raises `relation "x" does not
/// exist`, and a table name is not a graph question.
///
/// "Exists" is generous on purpose. A collection here does not have to be
/// declared — `put` into a name creates it — so a declared schema counts and so
/// does a single stored row. Only a name with neither is refused, which is why
/// the empty-but-declared case below must still answer normally.
#[test]
fn a_table_that_does_not_exist_is_an_error() {
    use sekejap::SqlError;

    for (label, cfg) in [("default", Config::default()), ("resident", Config::resident())] {
        let dir = tempfile::TempDir::new().unwrap();
        let mut db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
        db.execute("CREATE TABLE customers (_key TEXT PRIMARY KEY, n INTEGER)").unwrap();
        db.put("customers/c1", r#"{"_collection":"customers","_key":"c1","n":1}"#).unwrap();
        // Declared and empty — a real table with nothing in it.
        db.execute("CREATE TABLE empty_but_real (_key TEXT PRIMARY KEY)").unwrap();
        // Undeclared but holding a row — `put` is allowed to create a collection.
        db.put("adhoc/a1", r#"{"_collection":"adhoc","_key":"a1"}"#).unwrap();
        db.compact().unwrap();

        for sql in [
            "SELECT _key FROM customers",
            "SELECT _key FROM empty_but_real",
            "SELECT _key FROM adhoc",
            "SELECT COUNT(*) FROM empty_but_real",
        ] {
            assert!(db.query(sql).is_ok(), "[{label}] `{sql}` is a real table and was refused");
        }

        for sql in [
            "SELECT _key FROM custmers",
            "SELECT COUNT(*) FROM custmers",
            "SELECT _key FROM customers_2024",
        ] {
            match db.query(sql) {
                Err(SqlError::UndefinedTable(name)) => {
                    assert!(!name.is_empty(), "[{label}] the error must name the table");
                }
                Err(other) => panic!("[{label}] `{sql}` gave the wrong error: {other:?}"),
                Ok(set) => panic!(
                    "[{label}] `{sql}` answered with {} row(s) as though the table \
                     existed and was empty", set.collect().len()
                ),
            }
        }
    }
}
