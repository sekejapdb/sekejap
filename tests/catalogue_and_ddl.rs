//! What the database says exists, and what DDL will act on.
//!
//! These two answers used to disagree with each other, and each was wrong on its
//! own terms:
//!
//! * `SHOW TABLES` kept listing a table after `DROP TABLE` — before and after a
//!   restart, with every row gone — because collection names were read from the
//!   immutable mmap base while the tombstones that emptied it live in the
//!   overlay. The base/overlay split that has produced this whole family of bugs,
//!   here in the catalogue instead of in a query.
//! * the same catalogue did not list a table that `CREATE TABLE` had just
//!   declared, because it was derived from stored rows alone, so an empty table
//!   was indistinguishable from one that never existed
//! * `DROP TABLE` and `ALTER TABLE` refused a collection created by `put` with
//!   "table 'x' does not exist" — a claim the next `SELECT` disproves by
//!   returning its rows. `DROP TABLE` had *meant* to allow it and asked
//!   `self.collections`, the overlay, which is empty on a compacted database, so
//!   an undeclared collection could not be dropped at all.
//!
//! Everything is checked in both layouts and after a restart, and every fixture
//! compacts, because compaction is what moves rows from the overlay to the base
//! and is therefore what decided the answer.

use sekejap::{Config, CoreDB, SqlError};
use serde_json::json;

fn modes() -> Vec<(&'static str, Config)> {
    vec![("default", Config::default()), ("resident", Config::resident())]
}

fn names(db: &CoreDB) -> String {
    let mut n = db.collection_names();
    n.sort();
    n.join(",")
}

/// `declared` has a schema and rows, `empty_declared` has a schema and none,
/// `adhoc` has rows and no schema — the three ways a collection can stand.
fn fixture(dir: &std::path::Path, cfg: Config) -> CoreDB {
    let mut db = CoreDB::open_with_config(dir, cfg).unwrap();
    db.execute("CREATE TABLE declared (_key TEXT PRIMARY KEY, n INTEGER)").unwrap();
    db.execute("CREATE TABLE empty_declared (_key TEXT PRIMARY KEY)").unwrap();
    for i in 0..4 {
        db.put(&format!("declared/d{i}"), &json!({
            "_collection": "declared", "_key": format!("d{i}"), "n": i,
        }).to_string()).unwrap();
        db.put(&format!("adhoc/a{i}"), &json!({
            "_collection": "adhoc", "_key": format!("a{i}"), "n": i,
        }).to_string()).unwrap();
    }
    // The step that moves every row out of the overlay and into the base.
    db.compact().unwrap();
    db
}

#[test]
fn the_catalogue_lists_what_exists_and_nothing_else() {
    for (label, cfg) in modes() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut db = fixture(dir.path(), cfg.clone());

        assert_eq!(names(&db), "adhoc,declared,empty_declared",
            "[{label}] a declared table with no rows is still a table");

        db.execute("DROP TABLE declared").unwrap();
        assert_eq!(names(&db), "adhoc,empty_declared",
            "[{label}] a dropped table is still listed");

        // …and it stays dropped across the compaction that rewrites the base,
        // and across a restart that reads it back.
        db.compact().unwrap();
        assert_eq!(names(&db), "adhoc,empty_declared",
            "[{label}] the dropped table came back at the next compaction");
        drop(db);
        let db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
        assert_eq!(names(&db), "adhoc,empty_declared",
            "[{label}] the dropped table came back after a restart");
    }
}

/// **The same query must not answer differently either side of a compaction.**
///
/// After `DROP TABLE p`, the name kept resolving until the next compaction
/// rewrote the name map from the rows that were left. So the query said "no rows"
/// for a while and "no such table" afterwards, with nothing but a maintenance
/// operation between them.
#[test]
fn a_dropped_table_answers_the_same_before_and_after_a_compaction() {
    for (label, cfg) in modes() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut db = fixture(dir.path(), cfg.clone());
        db.execute("DROP TABLE declared").unwrap();

        let mut handle = Some(db);
        for when in ["immediately", "after a compaction", "after a restart"] {
            if when == "after a compaction" { handle.as_mut().unwrap().compact().unwrap(); }
            if when == "after a restart" {
                // Dropped before reopening: the store holds an exclusive lock, and
                // assigning over the binding would construct the new handle while
                // the old one still held it.
                drop(handle.take());
                handle = Some(CoreDB::open_with_config(dir.path(), cfg.clone()).unwrap());
            }
            let db = handle.as_ref().unwrap();
            match db.query("SELECT _key FROM declared") {
                Err(SqlError::UndefinedTable(n)) => assert_eq!(n, "declared"),
                Err(other) => panic!("[{label}] {when}: wrong error {other:?}"),
                Ok(set) => panic!("[{label}] {when}: answered with {} row(s)",
                                  set.collect().len()),
            }
        }
    }
}

/// **A collection that exists must not be told it does not.**
///
/// `adhoc` was created by `put`, so it has rows and no schema. Every statement
/// that reads or writes rows accepts it, so the ones that reject it may not say
/// it is absent.
#[test]
fn ddl_treats_an_undeclared_collection_as_the_collection_it_is() {
    for (label, cfg) in modes() {
        // Row-level statements work on it — this is what makes "does not exist"
        // a false statement rather than a stylistic complaint.
        {
            let dir = tempfile::TempDir::new().unwrap();
            let mut db = fixture(dir.path(), cfg.clone());
            assert_eq!(db.query("SELECT _key FROM adhoc").unwrap().collect().len(), 4);
            db.execute("UPDATE adhoc SET n = 9 WHERE n = 1").unwrap();
            db.execute("DELETE FROM adhoc WHERE n = 2").unwrap();
            db.execute("CREATE INDEX ON adhoc USING btree (n)").unwrap();
        }

        // DROP TABLE removes it. Before, this refused on a compacted database —
        // the check read the overlay, which a compaction empties — so there was
        // no way to drop an undeclared collection at all.
        {
            let dir = tempfile::TempDir::new().unwrap();
            let mut db = fixture(dir.path(), cfg.clone());
            assert_eq!(db.execute("DROP TABLE adhoc").unwrap(), 4,
                "[{label}] DROP TABLE did not remove the undeclared collection");
            assert!(!names(&db).contains("adhoc"), "[{label}] it is still listed");
        }

        // RENAME moves its rows. A declaration is not what makes rows movable:
        // they carry their own collection name.
        {
            let dir = tempfile::TempDir::new().unwrap();
            let mut db = fixture(dir.path(), cfg.clone());
            db.execute("ALTER TABLE adhoc RENAME TO renamed").unwrap();
            assert_eq!(db.query("SELECT _key FROM renamed").unwrap().collect().len(), 4,
                "[{label}] the rename did not bring the rows");
        }

        // Column operations change the declaration, so they still need one — but
        // they must say that, not that the table is missing.
        {
            let dir = tempfile::TempDir::new().unwrap();
            let mut db = fixture(dir.path(), cfg.clone());
            for sql in [
                "ALTER TABLE adhoc ADD COLUMN extra TEXT",
                "ALTER TABLE adhoc DROP COLUMN n",
                "ALTER TABLE adhoc RENAME COLUMN n TO m",
            ] {
                match db.execute(sql) {
                    Err(SqlError::InvalidValue(msg)) => assert!(
                        msg.contains("no declared schema"),
                        "[{label}] `{sql}` says the wrong thing: {msg}"),
                    other => panic!("[{label}] `{sql}` gave {other:?}"),
                }
            }
        }
    }
}

/// A name with nothing behind it is still refused, in every DDL verb — that is
/// the case the generous rule above must not swallow.
#[test]
fn a_name_that_was_never_anything_is_refused() {
    for (label, cfg) in modes() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut db = fixture(dir.path(), cfg);
        for sql in [
            "DROP TABLE typo",
            "ALTER TABLE typo RENAME TO other",
            "ALTER TABLE typo ADD COLUMN x TEXT",
        ] {
            assert!(matches!(db.execute(sql), Err(SqlError::UndefinedTable(_))),
                "[{label}] `{sql}` was not refused");
        }
        // …except with IF EXISTS, which asks for exactly that.
        assert_eq!(db.execute("DROP TABLE IF EXISTS typo").unwrap(), 0,
            "[{label}] IF EXISTS must stay quiet");
    }
}

/// **Dropping a column from one table must not empty another table's index.**
///
/// When `ALTER TABLE … DROP COLUMN` removes a column that other collections
/// still index under the same name, the shared index is rebuilt from the
/// collections that remain. All three rebuilds — GIN, BM25 and HNSW — walked
/// `self.collections` for members and `self.nodes` for the record, and both of
/// those are the overlay. A compaction empties the overlay by moving every row
/// into the mmap base, so on the default layout the rebuild ran over nothing and
/// the surviving collection lost every entry: `gin_ilike("body", "%heron%")`
/// answered 0 where 6 was right.
///
/// The resident layout was correct throughout, which is why this went unnoticed
/// — the bug only exists where a base exists. That makes cross-layout agreement
/// the test: the two layouts must answer the same, and both must answer 6.
#[test]
fn dropping_a_column_leaves_another_collection_s_index_intact() {
    for (label, cfg) in modes() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, body TEXT)").unwrap();
        db.execute("CREATE TABLE q (_key TEXT PRIMARY KEY, body TEXT)").unwrap();
        for i in 0..6 {
            for coll in ["p", "q"] {
                db.put(&format!("{coll}/{coll}{i}"), &json!({
                    "_collection": coll, "_key": format!("{coll}{i}"),
                    "body": "a heron by the water",
                }).to_string()).unwrap();
            }
        }
        db.execute("CREATE INDEX ON p USING gin (body)").unwrap();
        db.execute("CREATE INDEX ON q USING gin (body)").unwrap();
        db.execute("CREATE INDEX ON p USING bm25 (body)").unwrap();
        db.execute("CREATE INDEX ON q USING bm25 (body)").unwrap();
        // The step that decides this: every row moves out of the overlay.
        db.compact().unwrap();

        assert_eq!(db.gin_ilike("body", "%heron%", None).len(), 12,
            "[{label}] the fixture itself is wrong — both collections should be indexed");

        db.execute("ALTER TABLE p DROP COLUMN body").unwrap();

        assert_eq!(db.gin_ilike("body", "%heron%", None).len(), 6,
            "[{label}] the GIN index over `q` was rebuilt from the overlay, so it \
             lost every row that had already been compacted into the base");
        assert_eq!(db.bm25_search("body", "heron", 20).len(), 6,
            "[{label}] the BM25 index over `q` lost its rows the same way");

        // And the query surface agrees, in both directions.
        assert_eq!(db.query("SELECT _key FROM q WHERE body ILIKE '%heron%'")
                       .unwrap().collect().len(), 6,
            "[{label}] `q` lost rows from ILIKE");
        assert_eq!(db.query("SELECT _key FROM q WHERE BM25(body, 'heron') > 0")
                       .unwrap().collect().len(), 6,
            "[{label}] `q` lost rows from BM25");
        assert_eq!(db.query("SELECT _key FROM p WHERE body ILIKE '%heron%'")
                       .unwrap().collect().len(), 0,
            "[{label}] `p` no longer has the column and must match nothing");
    }
}
