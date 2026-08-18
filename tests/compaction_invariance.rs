//! Every answer must survive a compaction unchanged.
//!
//! A compaction moves rows out of the RAM overlay and into the mmap base. It is
//! supposed to change where bytes live and nothing else. Three separate bugs
//! found on 2026-08-18 were all the same mistake — code reading the overlay and
//! reporting what it found as the whole store — and all three were invisible
//! until something compacted:
//!
//! * `slug_of` answered `None` for every row, which made the Python and Node
//!   bindings' `bm25_search` return an empty list
//! * `centroid` answered `None` for every row
//! * the GIN/BM25/HNSW rebuilds after `DROP COLUMN` produced an empty index
//!
//! Each was found by hand. This is the axis they share, swept across the public
//! surface at once: ask everything, compact, ask again, and require the same
//! answers. It is a cheap test to extend — a new accessor is one more line — and
//! the failure it catches is the one that does not look like a failure, because
//! the query still runs and simply returns less.
//!
//! Runs in both layouts. The resident layout has no base, so it cannot exhibit
//! this family at all; it is here to prove the probes themselves are stable.

use sekejap::{Config, CoreDB};
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
            "body": "a heron beside the riverbank",
            "geometry": {"type": "Polygon", "coordinates": [[
                [144.95, -37.80], [144.98, -37.80], [144.98, -37.83],
                [144.95, -37.83], [144.95, -37.80]]]},
        }).to_string()).unwrap();
        // Vectors are stored through their own path, not as a payload field.
        // Irregular on purpose so no two tie on distance — a tie makes the top-k
        // selection arbitrary and the comparison below meaningless.
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
    for i in (0..23).step_by(3) {
        db.link_meta(&format!("p/n{i}"), &format!("q/m{}", i % 8), "tagged",
                     &json!({"weight": i as i64}).to_string()).unwrap();
    }
    db.execute("CREATE INDEX ON p USING btree (n)").unwrap();
    db.execute("CREATE INDEX ON p USING gin (body)").unwrap();
    db.execute("CREATE INDEX ON p USING bm25 (body)").unwrap();
    db.execute("CREATE INDEX ON p USING search (body)").unwrap();
    db.build_spatial_index();
    db.build_hnsw_index("vec", 16, 100).expect("hnsw");
    db
}

/// Every answer, as a string, with the name of what asked.
///
/// `&mut` because a few of these can rebuild an index on demand; the point is
/// what they *answer*, and an accessor that rebuilds must answer the same too.
fn answers(db: &mut CoreDB) -> Vec<(&'static str, String)> {
    let rows = |db: &CoreDB, sql: &str| -> String {
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
    };
    let mut out: Vec<(&'static str, String)> = vec![
        ("node_count",        db.node_count().to_string()),
        ("edge_count",        db.edge_count().to_string()),
        ("collection_names",  { let mut c = db.collection_names(); c.sort(); c.join(",") }),
        ("all_slugs",         { let mut s = db.all_slugs(); s.sort(); s.len().to_string() }),
        ("contains",          db.contains("p/n7").to_string()),
        ("get",               db.get("p/n7").unwrap_or_else(|| "NONE".into()).len().to_string()),
        ("stats.nodes",       db.stats().nodes.to_string()),
        ("dump INSERTs",      db.dump_sql().lines().filter(|l| l.starts_with("INSERT")).count().to_string()),
        ("schema_ddl",        db.schema_ddl("p").unwrap_or_else(|| "NONE".into())),

        // The three that were broken, and their neighbours.
        ("centroid",          format!("{:?}", db.centroid("p/n7"))),
        ("get_vector",        format!("{:?}", db.get_vector("p/n7", "vec").is_some())),
        ("gin_ilike",         db.gin_ilike("body", "%heron%", None).len().to_string()),
        ("bm25_search",       db.bm25_search("body", "heron", 50).len().to_string()),
        ("bm25 named",        { let h = db.bm25_search("body", "heron", 50);
                                h.iter().filter(|(x, _)| db.slug_of(*x).is_some()).count().to_string() }),

        ("edges_from",        db.edges_from("p/n0").len().to_string()),
        ("edges_to",          db.edges_to("p/n5").len().to_string()),
        ("edges_from_coll",   db.edges_from_collection("p").len().to_string()),
        ("edges_between",     db.edges_between("p", "q").len().to_string()),
        ("edge_types_from",   { let mut t = db.edge_types_from("p/n0"); t.sort(); t.join(",") }),
        ("edge_schema",       { let mut t: Vec<String> = db.edge_schema().iter()
                                   .map(|(a, b, c)| format!("{a}-{b}->{c}")).collect();
                                t.sort(); t.join(",") }),
        ("edge attrs",        db.edges_from("p/n0").iter()
                                .filter_map(|e| e.meta.as_ref().map(|m| m.to_string()))
                                .collect::<Vec<_>>().join("|")),
        ("forward hop",       db.one("p/n0").forward("next").collect().len().to_string()),
        ("backward hop",      db.one("p/n5").backward("next").collect().len().to_string()),
    ];
    for (name, sql) in [
        ("scan",        "SELECT _key FROM p"),
        ("where eq",    "SELECT _key FROM p WHERE n = 7"),
        ("where range", "SELECT _key FROM p WHERE n > 10 AND n < 20"),
        ("order limit", "SELECT _key FROM p ORDER BY n DESC LIMIT 5"),
        ("aggregate",   "SELECT COUNT(*) AS c, SUM(n) AS s, MIN(n) AS mn FROM p"),
        ("ilike",       "SELECT _key FROM p WHERE body ILIKE '%heron%'"),
        ("bm25",        "SELECT _key FROM p WHERE BM25(body,'heron') > 0"),
        ("search",      "SELECT _key FROM p WHERE SEARCH('riverbank')"),
        ("spatial",     "SELECT _key FROM p WHERE ST_DWithin(geometry, POINT(144.96 -37.81), 5000)"),
        ("vector",      "SELECT _key FROM p WHERE VECTOR_NEAR(vec, [1.0,1.0,2.0], 5)"),
        ("match",       "SELECT b._key FROM MATCH (a:p)-[:next]->(b:p)"),
        ("match cross", "SELECT b._key FROM MATCH (a:p)-[:tagged]->(b:q)"),
        ("shortest",    "SELECT b._key AS k, length(r) AS hops FROM MATCH SHORTEST \
                         (a:p)-[r*]->(b:p) WHERE a._key = 'n0' AND b._key = 'n9'"),
    ] {
        out.push((name, rows(db, sql)));
    }
    out
}

/// A probe that answers nothing agrees with itself whatever happens.
fn assert_probes_are_alive(probes: &[(&'static str, String)], label: &str) {
    let dead: Vec<&str> = probes.iter()
        .filter(|(_, v)| v.is_empty() || v == "0" || v == "NONE" || v.starts_with("ERR"))
        .map(|(n, _)| *n)
        .collect();
    assert!(dead.is_empty(),
            "[{label}] these probes answer nothing, so they cannot detect a change: {dead:?}");
}

#[test]
fn compaction_changes_where_bytes_live_and_nothing_else() {
    for (label, cfg) in modes() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut db = build(dir.path(), cfg.clone());

        let before = answers(&mut db);
        assert_probes_are_alive(&before, label);

        db.compact().unwrap();
        let after = answers(&mut db);
        for ((name, want), (_, got)) in before.iter().zip(&after) {
            assert_eq!(want, got,
                "[{label}] `{name}` changed across a compaction\n  before = {want}\n  after  = {got}");
        }

        // A second compaction with nothing in the overlay must also be inert.
        db.compact().unwrap();
        for ((name, want), (_, got)) in before.iter().zip(answers(&mut db)) {
            assert_eq!(want, &got, "[{label}] `{name}` changed across the second compaction");
        }

        // And the same database, reopened, still answers what it did.
        drop(db);
        let mut db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
        for ((name, want), (_, got)) in before.iter().zip(answers(&mut db)) {
            assert_eq!(want, &got,
                "[{label}] `{name}` changed across compaction + restart\n  before = {want}\n  after  = {got}");
        }
    }
}

/// **`DISTINCT` must return the same representative however the rows are laid out.**
///
/// `distinct_key_of` normalises numbers, so JSON `1` and `1.0` are one value —
/// which is right, and matches `WHERE v = 1` matching both, and matches
/// PostgreSQL, where `'1.0'::jsonb = '1'::jsonb` holds. A group can therefore
/// contain rows that render differently while being equal, and deduplication used
/// to keep whichever the scan reached first.
///
/// A compaction changes scan order. `SELECT DISTINCT v FROM p` answered
/// `{"v":1.0}` before one and `{"v":1}` after, over data nobody had touched.
/// Found by `compaction_agreement_fuzz` on seed 20260818778 at rounds 786 and
/// 796, in both layouts and in both directions — which is what ruled out a
/// simple encode/decode asymmetry and pointed at the representative instead.
///
/// PostgreSQL does not specify which row represents a group, so nothing required
/// this to be unstable. The rule is now the lowest slug, which costs a comparison
/// per duplicate and rewrites nothing.
#[test]
fn distinct_returns_the_same_representative_after_a_compaction() {
    for (label, cfg) in modes() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut db = CoreDB::open_with_config(dir.path(), cfg.clone()).unwrap();
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, v REAL)").unwrap();
        // Equal values written in both JSON forms, in both orders, so a
        // first-wins rule cannot accidentally agree with a lowest-slug rule.
        db.put("p/a", r#"{"_collection":"p","_key":"a","v":1.0}"#).unwrap();
        db.put("p/b", r#"{"_collection":"p","_key":"b","v":2.5}"#).unwrap();
        db.put("p/c", r#"{"_collection":"p","_key":"c","v":3}"#).unwrap();
        db.put("p/d", r#"{"_collection":"p","_key":"d","v":1}"#).unwrap();
        db.put("p/e", r#"{"_collection":"p","_key":"e","v":3.0}"#).unwrap();

        let distinct = |db: &CoreDB| -> String {
            let mut v: Vec<String> = db.query("SELECT DISTINCT v FROM p").unwrap()
                .collect().iter()
                .map(|h| h.payload.as_ref().map(|p| p.to_string()).unwrap_or_default())
                .collect();
            v.sort();
            v.join("|")
        };

        // The grouping itself: 1 and 1.0 are one value, 3 and 3.0 are one value.
        let before = distinct(&db);
        assert_eq!(before.matches('|').count() + 1, 3,
            "[{label}] equal numbers written in different forms must be one group: {before}");
        assert_eq!(db.query("SELECT _key FROM p WHERE v = 1").unwrap().collect().len(), 2,
            "[{label}] `v = 1` must match both 1 and 1.0, which is why they group");

        db.compact().unwrap();
        assert_eq!(before, distinct(&db),
            "[{label}] the DISTINCT representative moved across a compaction");

        drop(db);
        let db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
        assert_eq!(before, distinct(&db),
            "[{label}] the DISTINCT representative moved across a restart");
    }
}
