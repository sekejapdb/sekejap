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

use sekejap::CoreDB;

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
            let tie_swapped = pg.replace("q,t", "t,q");
            assert!(got == *pg || got == tie_swapped,
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
