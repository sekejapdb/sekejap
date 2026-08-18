//! An index may change how long an answer takes, never what it is.
//!
//! The companion to `compaction_invariance`. That one asserts an answer survives
//! rows moving between the overlay and the base; this one asserts it survives the
//! indexes being taken away.
//!
//! The distinction the suite has to make is between an index that *accelerates*
//! an answer and one that *is* the answer:
//!
//! * `WHERE n > 10`, `ILIKE`, `ST_DWithin`, `VECTOR_NEAR` and the graph verbs all
//!   have a scan behind them, so dropping their index must change nothing at all
//! * `SEARCH` and `BM25` are read entirely out of their index and have no scan
//!   behind them, so dropping theirs must produce the refusal added on
//!   2026-08-18 — never "no rows", which is what they used to answer
//!
//! Both halves are checked here, because getting either one wrong is the same
//! defect: a query that quietly answers a different question from the one asked.

use sekejap::{Config, CoreDB, SqlError};
use serde_json::json;

fn modes() -> Vec<(&'static str, Config)> {
    vec![("default", Config::default()), ("resident", Config::resident())]
}

fn build(dir: &std::path::Path, cfg: Config) -> CoreDB {
    let mut db = CoreDB::open_with_config(dir, cfg).unwrap();
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, name TEXT, body TEXT, n INTEGER)").unwrap();
    db.execute("CREATE TABLE q (_key TEXT PRIMARY KEY, name TEXT)").unwrap();
    for i in 0..24 {
        db.put(&format!("p/n{i}"), &json!({
            "_collection": "p", "_key": format!("n{i}"),
            "name": format!("row {i}"), "n": i as i64,
            "body": format!("a heron beside the riverbank number {i}"),
            "geometry": {"type": "Point",
                         "coordinates": [144.96 + (i as f64) * 0.001, -37.81 + (i as f64) * 0.001]},
        }).to_string()).unwrap();
        db.put_vector(&format!("p/n{i}"), "vec", &[
            ((i * 37 % 97) as f32) / 10.0,
            ((i * 61 % 89) as f32) / 10.0,
            ((i * 17 % 83) as f32) / 10.0,
        ]).unwrap();
    }
    for i in 0..8 {
        db.put(&format!("q/m{i}"), &json!({
            "_collection": "q", "_key": format!("m{i}"), "name": format!("other {i}"),
        }).to_string()).unwrap();
    }
    for i in 0..23 { db.link(&format!("p/n{i}"), &format!("p/n{}", i + 1), "next"); }
    db.execute("CREATE INDEX ON p USING btree (n)").unwrap();
    db.execute("CREATE INDEX ON p USING gin (body)").unwrap();
    db.execute("CREATE INDEX ON p USING bm25 (body)").unwrap();
    db.execute("CREATE INDEX ON p USING search (body)").unwrap();
    db.execute("CREATE INDEX ON p USING spatial (geometry)").ok();
    db.build_spatial_index();
    db.build_hnsw_index("vec", 16, 100).expect("hnsw");
    db.compact().unwrap();
    db
}

/// Queries that must answer identically with or without any index.
const SCANNABLE: &[(&str, &str)] = &[
    ("scan",         "SELECT _key FROM p"),
    ("eq",           "SELECT _key FROM p WHERE n = 7"),
    ("range",        "SELECT _key FROM p WHERE n > 10 AND n < 20"),
    ("between",      "SELECT _key FROM p WHERE n BETWEEN 4 AND 9"),
    ("in",           "SELECT _key FROM p WHERE n IN (1,2,3)"),
    ("order limit",  "SELECT _key FROM p ORDER BY n DESC LIMIT 5"),
    ("aggregate",    "SELECT COUNT(*) AS c, SUM(n) AS s, MIN(n) AS mn, MAX(n) AS mx FROM p"),
    ("ilike",        "SELECT _key FROM p WHERE body ILIKE '%heron%'"),
    ("ilike middle", "SELECT _key FROM p WHERE body ILIKE '%riverbank number 1%'"),
    ("not eq",       "SELECT _key FROM p WHERE n != 3"),
    ("is null",      "SELECT _key FROM p WHERE name IS NOT NULL"),
    ("spatial",      "SELECT _key FROM p WHERE ST_DWithin(geometry, POINT(144.96 -37.81), 5000)"),
    ("vector",       "SELECT _key FROM p WHERE VECTOR_NEAR(vec, [1.0,1.0,2.0], 5)"),
    ("match",        "SELECT b._key FROM MATCH (a:p)-[:next]->(b:p)"),
    ("shortest",     "SELECT b._key AS k, length(r) AS hops FROM MATCH SHORTEST \
                      (a:p)-[r*]->(b:p) WHERE a._key = 'n0' AND b._key = 'n9'"),
];

/// Queries that are read out of an index and cannot be answered without it.
const INDEX_ONLY: &[(&str, &str)] = &[
    ("search", "SELECT _key FROM p WHERE SEARCH('riverbank')"),
    ("bm25",   "SELECT _key FROM p WHERE BM25(body,'heron') > 0"),
];

fn rows(db: &CoreDB, sql: &str) -> String {
    match db.query(sql) {
        Ok(s) => {
            let mut v: Vec<String> = s.collect().iter()
                .map(|h| if h.slug.is_empty() {
                    h.payload.as_ref().map(|p| p.to_string()).unwrap_or_default()
                } else { h.slug.clone() })
                .collect();
            v.sort();
            v.join(",")
        }
        Err(e) => format!("ERR {e}"),
    }
}

#[test]
fn dropping_an_index_does_not_change_what_can_be_scanned() {
    for (label, cfg) in modes() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = build(dir.path(), cfg.clone());

        let before: Vec<(&str, String)> =
            SCANNABLE.iter().map(|(n, sql)| (*n, rows(&db, sql))).collect();
        for (name, v) in &before {
            assert!(!v.is_empty() && !v.starts_with("ERR"),
                "[{label}] probe `{name}` answers nothing before anything is dropped: {v}");
        }
        drop(db);

        // One fresh database per drop, so each is measured against the same start
        // rather than against whatever the previous drop left behind.
        for ddl in [
            "DROP INDEX ON p USING btree (n)",
            "DROP INDEX ON p USING gin (body)",
            "DROP INDEX ON p USING spatial (geometry)",
            "DROP INDEX ON p USING hnsw (vec)",
        ] {
            let dir = tempfile::TempDir::new().unwrap();
            let mut db = build(dir.path(), cfg.clone());
            if db.execute(ddl).is_err() {
                continue; // this build does not declare that one; nothing to prove
            }
            for (name, want) in &before {
                let got = rows(&db, SCANNABLE.iter().find(|(n, _)| n == name).unwrap().1);
                assert_eq!(want, &got,
                    "[{label}] `{ddl}` changed the answer to `{name}`\n  \
                     with index = {want}\n  without    = {got}");
            }
        }
    }
}

/// The other half: an index that *is* the answer must be missed out loud.
#[test]
fn dropping_a_search_index_is_refused_not_answered_empty() {
    for (label, cfg) in modes() {
        for (name, sql) in INDEX_ONLY {
            let dir = tempfile::TempDir::new().unwrap();
            let mut db = build(dir.path(), cfg.clone());

            let before = rows(&db, sql);
            assert!(!before.is_empty() && !before.starts_with("ERR"),
                "[{label}] `{name}` found nothing to begin with");

            let ddl = if *name == "search" {
                "DROP INDEX ON p USING search (body)"
            } else {
                "DROP INDEX ON p USING bm25 (body)"
            };
            db.execute(ddl).unwrap();

            match db.query(sql) {
                Err(SqlError::IndexNotBuilt { .. }) => {}
                Err(other) => panic!("[{label}] after `{ddl}`, `{name}` gave {other:?}"),
                Ok(set) => panic!(
                    "[{label}] after `{ddl}`, `{name}` answered with {} row(s) — the \
                     documents are still there and still contain the term, so any \
                     row count here is a claim about data that was not consulted",
                    set.collect().len()),
            }

            // The rows themselves are untouched, which is what makes an empty
            // answer the wrong one.
            assert_eq!(db.query("SELECT _key FROM p").unwrap().collect().len(), 24,
                "[{label}] `{ddl}` removed rows");
            assert_eq!(db.query("SELECT _key FROM p WHERE body ILIKE '%heron%'")
                           .unwrap().collect().len(), 24,
                "[{label}] the text is still there and still scannable");
        }
    }
}

/// **A value the index cannot represent must not be dropped from an ORDER BY.**
///
/// Four separate paths built an ordered answer *out of the btree* — the Sort
/// step's index-assisted branch, `try_index_order_limit`, `try_covered_sort_limit`
/// and `btree_sorted_seed_from_steps` — and each replaced the candidate list with
/// what it found there. A row the index has no entry for was therefore not
/// mis-ordered but gone.
///
/// A missing field is safe, because absence is stored as `FieldKey::Null`, and
/// that is why this went unnoticed for so long. An array is not: it has no btree
/// key at all. `SELECT _key FROM c ORDER BY tags` returned **nothing** with an
/// index on `tags` and every row without one. Creating an index deleted the
/// answer.
///
/// The comparison is against the same query with no index, in full and in order,
/// because a count alone would not catch a row that merely moved.
#[test]
fn ordering_by_a_column_the_index_cannot_hold_still_returns_every_row() {
    const QUERIES: &[&str] = &[
        "SELECT _key FROM c ORDER BY tags",
        "SELECT _key FROM c ORDER BY tags LIMIT 3",
        "SELECT _key FROM c ORDER BY tags DESC",
        "SELECT _key FROM c ORDER BY tags DESC LIMIT 3",
        "SELECT _key FROM c ORDER BY tags LIMIT 3 OFFSET 2",
        "SELECT _key, n FROM c ORDER BY tags",
        "SELECT * FROM c ORDER BY tags",
        "SELECT _key FROM c WHERE n >= 0 ORDER BY tags",
        // The scalar column must keep working, so the fix cannot have been to
        // stop using the index at all.
        "SELECT _key FROM c ORDER BY n",
        "SELECT _key FROM c ORDER BY n DESC LIMIT 4",
    ];

    for (label, cfg) in modes() {
        let build = |indexed: bool| -> (tempfile::TempDir, CoreDB) {
            let dir = tempfile::TempDir::new().unwrap();
            let mut db = CoreDB::open_with_config(dir.path(), cfg.clone()).unwrap();
            db.execute("CREATE TABLE c (_key TEXT PRIMARY KEY, tags JSON, n INTEGER)").unwrap();
            for i in 0..6 {
                db.put(&format!("c/x{i}"), &json!({
                    "_collection": "c", "_key": format!("x{i}"),
                    "tags": [format!("t{i}"), "shared"], "n": i as i64,
                }).to_string()).unwrap();
            }
            // A row with no `tags` at all, which the index *does* hold (as NULL),
            // so the two kinds of unindexable row are both in play.
            db.put("c/none", &json!({"_collection": "c", "_key": "none", "n": 99}).to_string())
                .unwrap();
            if indexed {
                db.execute("CREATE INDEX ON c USING btree (tags)").unwrap();
                db.execute("CREATE INDEX ON c USING btree (n)").unwrap();
            }
            db.compact().unwrap();
            (dir, db)
        };

        let (_d1, scan) = build(false);
        let (_d2, idx) = build(true);

        for sql in QUERIES {
            let a = rows(&scan, sql);
            let b = rows(&idx, sql);
            assert!(!a.is_empty(), "[{label}] `{sql}` found nothing even unindexed");
            assert_eq!(a, b,
                "[{label}] `{sql}` answers differently once the column is indexed\n  \
                 without index = {a}\n  with index    = {b}");
        }
    }
}
