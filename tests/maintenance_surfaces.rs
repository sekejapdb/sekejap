//! Maintenance operations that nothing tested, across both layouts and a restart.
//!
//! `trim_memory`, `REINDEX`, `DROP INDEX` and `sync` all had zero tests. They are
//! grouped here because they share a hazard: each one rebuilds or discards part of
//! the index state, and the paged layout keeps that state in two places — an mmap
//! base and a RAM overlay. Every bug this family has produced looked the same from
//! outside: the query still ran, and returned fewer rows.
//!
//! Each check runs in the default layout and the resident one, and again after
//! closing and reopening the database — the axis that caught a duplicate index
//! posting the day the differential audit gained it.

use sekejap::{Config, CoreDB};
use serde_json::json;

fn build(dir: &std::path::Path, cfg: Config) {
    let mut db = CoreDB::open_with_config(dir, cfg).unwrap();
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER, body TEXT)").unwrap();
    for i in 0..200 {
        db.put(&format!("p/n{i}"), &json!({
            "_collection": "p", "_key": format!("n{i}"), "n": i as i64,
            "body": format!("record {i} riverbank fox"),
        }).to_string()).unwrap();
    }
    db.execute("CREATE INDEX ON p USING btree (n)").unwrap();
    db.execute("CREATE INDEX ON p USING gin (body)").unwrap();
    db.build_bm25_index("body");
    db.compact().unwrap();
}

/// The answers that must not change, whatever maintenance runs.
const PROBES: &[(&str, &str)] = &[
    ("range",  "SELECT _key FROM p WHERE n > 190"),
    ("eq",     "SELECT _key FROM p WHERE n = 42"),
    ("ilike",  "SELECT _key FROM p WHERE body ILIKE '%fox%'"),
    ("bm25",   "SELECT _key FROM p WHERE BM25(body,'riverbank') > 0"),
    ("search", "SELECT _key FROM p WHERE SEARCH('riverbank')"),
    ("all",    "SELECT _key FROM p"),
];

fn answers(db: &CoreDB) -> Vec<(String, String)> {
    PROBES.iter().map(|(name, sql)| {
        let mut v: Vec<String> = db.query(sql)
            .unwrap_or_else(|e| panic!("`{sql}` did not run: {e:?}"))
            .collect().iter().map(|h| h.slug.clone()).collect();
        v.sort();
        (name.to_string(), v.join(","))
    }).collect()
}

fn modes() -> Vec<(&'static str, Config)> {
    vec![("default", Config::default()), ("resident", Config::resident())]
}

/// Run `op`, then check every answer is what it was — before and after a restart.
fn unchanged_by(what: &str, op: fn(&mut CoreDB)) {
    for (label, cfg) in modes() {
        let dir = tempfile::TempDir::new().unwrap();
        build(dir.path(), cfg.clone());

        let before = {
            let db = CoreDB::open_with_config(dir.path(), cfg.clone()).unwrap();
            answers(&db)
        };
        {
            let mut db = CoreDB::open_with_config(dir.path(), cfg.clone()).unwrap();
            op(&mut db);
            for ((name, want), (_, got)) in before.iter().zip(answers(&db)) {
                assert_eq!(want, &got,
                    "[{label}] {what} changed the answer to `{name}` in the same session");
            }
        }
        // …and after a restart, which is where index state that was rebuilt into
        // the wrong half of the store shows up.
        let db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
        for ((name, want), (_, got)) in before.iter().zip(answers(&db)) {
            assert_eq!(want, &got,
                "[{label}] {what} changed the answer to `{name}` after a restart");
        }
    }
}

/// `trim_memory` documents that it shrinks capacity and **never drops data or
/// indexes, so query results are unchanged**. Nothing checked the second half.
#[test]
fn trim_memory_changes_no_answer() {
    unchanged_by("trim_memory", |db| db.trim_memory());
}

/// Rebuilding an index in place is the exact base/overlay hazard that produced
/// the `DROP TABLE` and `UPDATE`-btree bugs. It was tested in the resident layout
/// only.
#[test]
fn reindex_changes_no_answer() {
    unchanged_by("REINDEX", |db| {
        db.execute("REINDEX ON p USING btree (n)").expect("REINDEX failed");
    });
}

/// Dropping an index must not change an answer either — the rows are still there
/// and the query falls back to a scan. In the paged layout the sidecar is mmap'd,
/// so dropping it while mapped is its own question.
#[test]
fn drop_index_changes_no_answer() {
    unchanged_by("DROP INDEX", |db| {
        db.execute("DROP INDEX ON p USING btree (n)").expect("DROP INDEX failed");
    });
}

/// `sync` claims durability before the OS gets round to it. What is checkable
/// from here is that it is not a wrong answer or a panic, and that the data is
/// intact afterwards.
#[test]
fn sync_changes_no_answer() {
    unchanged_by("sync", |db| { db.sync().expect("sync failed"); });
}

/// **`stats()` must describe the database that exists, not the one that has
/// compacted.**
///
/// `paged` and `overlay_nodes` were both answered from `!segments.is_empty()` —
/// "is a mapped topology segment present". That is false for a paged database
/// until a compaction has written and adopted one, so a fresh store in the
/// default layout reported itself resident with an empty overlay while holding
/// every row in RAM. A diagnostic that lies is worse than no diagnostic: it is
/// what someone reads when they are trying to work out why memory is climbing.
///
/// The same narrow proxy decided something much more expensive elsewhere —
/// whether to parse every geometry into RAM at open — which is why this is worth
/// a test rather than a one-line change.
#[test]
fn stats_describes_the_layout_it_is_actually_in() {
    for (label, cfg, want_paged) in [
        ("default", Config::default(), true),
        ("resident", Config::resident(), false),
    ] {
        let dir = tempfile::TempDir::new().unwrap();
        let mut db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER)").unwrap();
        for i in 0..50 {
            db.put(&format!("p/n{i}"), &serde_json::json!({
                "_collection": "p", "_key": format!("n{i}"), "n": i as i64,
            }).to_string()).unwrap();
        }

        // Before any compaction — the case that was wrong.
        let s = db.stats();
        assert_eq!(s.paged, want_paged,
            "[{label}] stats reported paged={} before the first compaction; the \
             layout is decided when the database is opened, not when it compacts",
            s.paged);
        if want_paged {
            assert_eq!(s.overlay_nodes, 50,
                "[{label}] 50 rows are held in the write overlay and stats reported \
                 {} — this is the number someone reads when memory is climbing",
                s.overlay_nodes);
        }

        // And after, where the overlay has been folded into the base.
        db.compact().unwrap();
        let s = db.stats();
        assert_eq!(s.paged, want_paged, "[{label}] stats changed its mind after a compaction");
        if want_paged {
            assert_eq!(s.overlay_nodes, 0,
                "[{label}] the overlay still reports {} rows after a compaction \
                 folded them into the base", s.overlay_nodes);
        }
    }
}
