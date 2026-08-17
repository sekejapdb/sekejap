//! Every read path must answer the same in both storage modes.
//!
//! Paged mode splits the database in two — compacted rows in an immutable mmap
//! ("the base"), recent writes in a small RAM map ("the overlay"). Any read path
//! that consults the overlay alone sees a fraction of the data and reports it as
//! the whole truth, silently. That one mistake produced twelve separate bugs,
//! including compaction deleting every edge in the graph.
//!
//! This runs the same operations against both modes and requires identical
//! answers, and separately computes ground truth in Rust with no index involved.
//! Both halves are necessary. Every self-consistency check we had — `MAX` agreeing
//! with `ORDER BY`, `COUNT` agreeing with a scan — *passed* while a stale btree
//! index was returning nothing for `WHERE n > 15`, because both sides were reading
//! the same stale index and agreeing with each other. Only ground truth caught it.
//!
//! Keep this green. It is the thing standing between a storage change and silent
//! data loss.

use sekejap::{Config, CoreDB};
use serde_json::json;

// ── dataset ──────────────────────────────────────────────────────────────────

fn seed(db: &mut CoreDB, range: std::ops::Range<usize>) {
    for i in range {
        let animal = ["fox", "dog", "heron"][i % 3];
        db.put(&format!("p/n{i}"), &json!({
            "_collection": "p", "_key": format!("n{i}"),
            "name": format!("node {i}"),
            "body": format!("the quick brown {animal} number {i} leaps the lazy riverbank"),
            "n": i as i64, "grp": (i % 4) as i64,
            "geometry": {"type": "Point",
                         "coordinates": [144.96 + (i as f64) * 0.002, -37.81 + (i as f64) * 0.002]},
        }).to_string()).unwrap();
        // Deliberately irregular so no two vectors tie on cosine distance. A
        // regular formula produced collinear pairs — n0=[0,2,1] and n2=[2,4,0] are
        // both exactly 0.9129 from [1,2,1] — and which one lands in the top-k is
        // then arbitrary, not a property either mode owes the other.
        db.put_vector(&format!("p/n{i}"), "vec", &[
            ((i * 37 % 97) as f32) / 10.0,
            ((i * 61 % 89) as f32) / 10.0,
            ((i * 17 % 83) as f32) / 10.0,
        ]).unwrap();
    }
}

fn seed_other(db: &mut CoreDB, range: std::ops::Range<usize>) {
    for i in range {
        db.put(&format!("q/m{i}"), &json!({
            "_collection": "q", "_key": format!("m{i}"),
            "label": format!("tag {i}"), "n": (i * 3) as i64,
        }).to_string()).unwrap();
    }
}

fn build(dir: &std::path::Path) { build_with(dir, Config::default()) }

fn build_with(dir: &std::path::Path, cfg: Config) {
    let mut db = CoreDB::open_with_config(dir, cfg).unwrap();
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, name TEXT, body TEXT, n INTEGER, grp INTEGER)").unwrap();
    seed(&mut db, 0..30);
    seed_other(&mut db, 0..8);
    for i in 0..29 { db.link(&format!("p/n{i}"), &format!("p/n{}", i + 1), "next"); }
    for i in (0..29).step_by(3) {
        db.link_meta(&format!("p/n{i}"), &format!("q/m{}", i % 5), "tagged",
                     &json!({"weight": i as i64}).to_string()).unwrap();
    }
    db.execute("CREATE INDEX ON p USING btree (n)").unwrap();
    db.execute("CREATE INDEX ON p USING gin (body)").unwrap();
    db.execute("CREATE INDEX ON p USING bm25 (body)").unwrap();
    db.execute("CREATE INDEX ON p USING search (body)").unwrap();
    db.build_spatial_index();
    db.build_hnsw_index("vec", 16, 100).expect("hnsw build");
    db.compact().unwrap();
}

// ── probes ───────────────────────────────────────────────────────────────────

fn rows(db: &CoreDB, sql: &str) -> String {
    match db.query(sql) {
        Ok(s) => {
            let mut k: Vec<String> = s.collect().iter().map(|h| h.slug.clone()).collect();
            k.sort();
            k.join(",")
        }
        Err(e) => format!("ERR {e:?}"),
    }
}

fn vals(db: &CoreDB, sql: &str) -> String {
    match db.query(sql) {
        Ok(s) => {
            let mut v: Vec<String> = s.collect().iter()
                .map(|h| h.payload.as_ref().map(|p| p.to_string()).unwrap_or_default())
                .collect();
            v.sort();
            v.join("|")
        }
        Err(e) => format!("ERR {e:?}"),
    }
}

type Probe = (&'static str, fn(&mut CoreDB) -> String);

fn probes() -> Vec<Probe> {
    vec![
        ("node_count",            |d| d.node_count().to_string()),
        ("edge_count",            |d| d.edge_count().to_string()),
        ("collection_names",      |d| { let mut c = d.collection_names(); c.sort(); c.join(",") }),
        ("all_slugs.len",         |d| d.all_slugs().len().to_string()),
        ("contains",              |d| d.contains("p/n7").to_string()),
        ("get",                   |d| d.get("p/n7").map(|s| s.len().to_string()).unwrap_or("NONE".into())),
        ("stats.nodes",           |d| d.stats().nodes.to_string()),
        ("dump_sql inserts",      |d| d.dump_sql().lines().filter(|l| l.starts_with("INSERT")).count().to_string()),

        ("scan p",                |d| rows(d, "SELECT _key FROM p")),
        ("scan q",                |d| rows(d, "SELECT _key FROM q")),
        ("where eq",              |d| rows(d, "SELECT _key FROM p WHERE n = 7")),
        ("where range",           |d| rows(d, "SELECT _key FROM p WHERE n > 10 AND n < 25")),
        ("where between",         |d| rows(d, "SELECT _key FROM p WHERE n BETWEEN 5 AND 15")),
        ("where in",              |d| rows(d, "SELECT _key FROM p WHERE n IN (1,2,3)")),
        ("order by desc",         |d| rows(d, "SELECT _key FROM p ORDER BY n DESC LIMIT 5")),
        ("count",                 |d| vals(d, "SELECT COUNT(*) AS c FROM p")),
        ("sum/avg/min/max",       |d| vals(d, "SELECT SUM(n) AS s, AVG(n) AS a, MIN(n) AS mn, MAX(n) AS mx FROM p")),
        ("group by",              |d| vals(d, "SELECT grp, COUNT(*) AS c FROM p GROUP BY grp")),

        ("ilike",                 |d| rows(d, "SELECT _key FROM p WHERE body ILIKE '%fox%'")),
        ("bm25 set",              |d| rows(d, "SELECT _key FROM p WHERE BM25(body,'heron') > 0")),
        ("search",                |d| rows(d, "SELECT _key FROM p WHERE SEARCH('riverbank')")),
        ("gin direct",            |d| d.gin_ilike("body", "%fox%", None).len().to_string()),
        // Scores, not ranks: with many documents scoring identically the top-k
        // selection among exact ties is arbitrary and not a guarantee we make.
        ("bm25 scores",           |d| d.bm25_search("body", "heron", 5).iter()
                                       .map(|(_, s)| format!("{s:.6}")).collect::<Vec<_>>().join(",")),

        ("spatial order",         |d| { d.build_spatial_index();
                                        rows(d, "SELECT _key FROM p ORDER BY ST_DISTANCE(geometry, POINT(144.96 -37.81)) ASC LIMIT 5") }),
        ("get_vector",            |d| d.get_vector("p/n7", "vec").map(|v| format!("{v:?}")).unwrap_or("NONE".into())),
        ("vector_near",           |d| rows(d, "SELECT _key FROM p WHERE VECTOR_NEAR(vec, [1.0,2.0,1.0], 5)")),

        ("forward hop",           |d| d.one("p/n0").forward("next").collect().len().to_string()),
        ("backward hop",          |d| d.one("p/n5").backward("next").collect().len().to_string()),
        ("edges_from",            |d| d.edges_from("p/n0").len().to_string()),
        ("edges_to",              |d| d.edges_to("p/n5").len().to_string()),
        ("edges_from_collection", |d| d.edges_from_collection("p").len().to_string()),
        ("edges_between",         |d| d.edges_between("p", "q").len().to_string()),
        ("edge_types_from",       |d| { let mut t = d.edge_types_from("p/n0"); t.sort(); t.join(",") }),
        ("edge_schema",           |d| { let mut t: Vec<String> = d.edge_schema().iter()
                                           .map(|(a,b,c)| format!("{a}-{b}->{c}")).collect();
                                        t.sort(); t.join(",") }),
        ("edge attrs",            |d| d.edges_from("p/n0").iter()
                                       .filter_map(|e| e.meta.as_ref().map(|m| m.to_string()))
                                       .collect::<Vec<_>>().join("|")),
        ("MATCH",                 |d| rows(d, "SELECT _key FROM MATCH (a:p)-[:next]->(b:p)")),
        ("MATCH cross",           |d| rows(d, "SELECT _key FROM MATCH (a:p)-[:tagged]->(b:q)")),
        ("SHORTEST",              |d| rows(d, "SELECT _key FROM MATCH SHORTEST (a)-[r*]->(b) \
                                               WHERE a._key = 'p/n0' AND b._key = 'p/n9'")),

        ("SHOW TABLES",           |d| d.show("SHOW TABLES").map(|h| h.len().to_string()).unwrap_or("ERR".into())),
        ("SHOW EDGES",            |d| d.show("SHOW EDGES").map(|h| h.len().to_string()).unwrap_or("ERR".into())),

        ("snapshot == live",      |d| match d.snapshot_db() {
                                       Some(s) => {
                                           let a = rows(&s, "SELECT _key FROM p");
                                           let b = rows(d, "SELECT _key FROM p");
                                           if a == b { "agree".into() } else { format!("snap={a} live={b}") }
                                       }
                                       None => "agree".into() }),
    ]
}

// ── ground truth ─────────────────────────────────────────────────────────────
//
// A cross-mode diff cannot catch a bug that is wrong in BOTH modes — a stale index
// answers whoever asks. These answers are computed from the stored rows directly.

fn ground_truth(db: &CoreDB, mode: &str) {
    let rows_p: Vec<serde_json::Value> = db.all_slugs().iter()
        .filter(|s| s.starts_with("p/"))
        .filter_map(|s| db.get(s))
        .filter_map(|j| serde_json::from_str(&j).ok())
        .collect();
    let ns: Vec<i64> = rows_p.iter().filter_map(|r| r.get("n").and_then(|v| v.as_i64())).collect();
    let cnt = |sql: &str| db.query(sql).map(|s| s.collect().len()).unwrap_or(0);
    let num = |sql: &str, k: &str| {
        let v = vals(db, sql);
        let pat = format!("\"{k}\":");
        v.find(&pat).map(|i| v[i + pat.len()..].trim_end_matches('}').trim_end_matches(".0").to_string())
            .unwrap_or(v)
    };

    assert_eq!(num("SELECT COUNT(*) AS c FROM p", "c"), rows_p.len().to_string(), "[{mode}] COUNT");
    assert_eq!(num("SELECT MAX(n) AS mx FROM p", "mx"),
               ns.iter().max().unwrap().to_string(), "[{mode}] MAX");
    assert_eq!(num("SELECT MIN(n) AS mn FROM p", "mn"),
               ns.iter().min().unwrap().to_string(), "[{mode}] MIN");
    assert_eq!(num("SELECT SUM(n) AS s FROM p", "s"),
               ns.iter().sum::<i64>().to_string(), "[{mode}] SUM");
    assert_eq!(cnt("SELECT _key FROM p WHERE n > 15"),
               ns.iter().filter(|&&v| v > 15).count(), "[{mode}] WHERE n > 15");
    assert_eq!(cnt("SELECT _key FROM p WHERE n BETWEEN 5 AND 15"),
               ns.iter().filter(|&&v| (5..=15).contains(&v)).count(), "[{mode}] BETWEEN");

    let has = |needle: &str| rows_p.iter()
        .filter(|r| r.get("body").and_then(|v| v.as_str()).map_or(false, |b| b.contains(needle)))
        .count();
    assert_eq!(cnt("SELECT _key FROM p WHERE body ILIKE '%fox%'"), has("fox"), "[{mode}] ILIKE");
    assert_eq!(cnt("SELECT _key FROM p WHERE BM25(body,'fox') > 0"), has("fox"), "[{mode}] BM25");
    assert_eq!(cnt("SELECT _key FROM p WHERE SEARCH('fox')"), has("fox"), "[{mode}] SEARCH");
}

// ── harness ──────────────────────────────────────────────────────────────────

/// Every storage mode the engine can be opened in.
///
/// The third exists because a mode that is not in this list is a mode nothing
/// compares against: paged adjacency keeps the graph in slotted pages instead of
/// rebuilding CSR, and every edge probe below has to answer identically through it
/// or it is silently returning a different graph.
fn modes() -> Vec<(&'static str, Config)> {
    vec![
        ("resident", Config::default()),
        ("paged", Config { paged_topology: true, ..Config::default() }),
        ("paged+adj", Config {
            paged_topology: true, paged_adjacency: true, ..Config::default()
        }),
        ("paged+all", Config {
            paged_topology: true, paged_adjacency: true, paged_nodes: true,
            paged_payloads: true, ..Config::default()
        }),
    ]
}

fn run(name: &str, mutate: fn(&mut CoreDB)) {
    let mut built: Vec<(&str, tempfile::TempDir, CoreDB)> = Vec::new();
    for (label, cfg) in modes() {
        let dir = tempfile::TempDir::new().unwrap();
        // Built in the mode it will be read in. A database whose payloads were
        // written flat cannot be reopened with paged payloads — the two are not the
        // same file, and the reopen is refused rather than reinterpreting one as
        // the other.
        build_with(dir.path(), cfg.clone());
        let mut db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
        mutate(&mut db);
        ground_truth(&db, label);
        built.push((label, dir, db));
    }

    // Every mode is compared against the first, which is the plain in-RAM engine —
    // the one with no base, no overlay and nothing to merge, so a disagreement
    // names the mode that is wrong rather than leaving two suspects.
    let mut bad = Vec::new();
    for (label, f) in &probes() {
        let reference = f(&mut built[0].2);
        for (mode, _, db) in built.iter_mut().skip(1) {
            let got = f(db);
            if got != reference {
                bad.push(format!(
                    "  {label:<24} resident ={reference}\n  {:<24} {mode:<9}={got}", ""));
            }
        }
    }
    assert!(bad.is_empty(),
            "[{name}] storage modes disagree — this is a base/overlay bug:\n{}",
            bad.join("\n"));
}

#[test]
fn all_base() { run("ALL-BASE", |_| {}); }

#[test]
fn mixed_base_and_overlay() {
    run("MIXED", |db| {
        seed(db, 30..40);
        db.link("p/n29", "p/n30", "next");
        db.execute("UPDATE p SET name = 'renamed' WHERE n = 3").unwrap();
    });
}

#[test]
fn tombstoned_base_rows() {
    run("TOMBSTONED", |db| {
        db.execute("DELETE FROM p WHERE n IN (2,3,4)").unwrap();
        db.remove("p/n11");
    });
}

#[test]
fn churn_write_then_compact() {
    run("CHURN", |db| {
        for round in 0..3 {
            let lo = 30 + round * 5;
            seed(db, lo..lo + 5);
            db.compact().unwrap();
        }
    });
}

#[test]
fn rewrite_deleted_keys() {
    run("REWRITE", |db| {
        db.execute("DELETE FROM p WHERE n IN (5,6,7)").unwrap();
        seed(db, 5..8);
    });
}

#[test]
fn update_base_rows_in_place() {
    run("UPDATED", |db| {
        db.execute("UPDATE p SET name = 'updated' WHERE n < 5").unwrap();
    });
}

/// Withdrawing edges that live in the immutable base.
///
/// This scenario was missing, and it hid a real one: `EdgeStore::unlink` can only
/// retain-out of the RAM adjacency maps, so in paged mode — where the edges are in
/// the mapped base and a read merges the two — unlinking did *nothing*. The call
/// returned, the overlay had never held the edge, and the next read produced it
/// again from the base. Resident mode was correct, paged mode silently kept the
/// edge, and the two modes had never been compared on a delete.
#[test]
fn unlinked_base_edges() {
    run("UNLINK", |db| {
        db.unlink("p/n0", "p/n1", "next");
        db.unlink("p/n5", "p/n6", "next");
        // One that never existed must stay a no-op rather than removing a
        // neighbour's edge by association.
        db.unlink("p/n20", "p/n25", "next");
    });
}

/// The same, but survived across a compaction — the withdrawal is recorded outside
/// the base, so the base being rewritten is exactly when it could be lost.
#[test]
fn unlinked_base_edges_then_compacted() {
    run("UNLINK-COMPACT", |db| {
        db.unlink("p/n0", "p/n1", "next");
        db.unlink("p/n5", "p/n6", "next");
        db.compact().unwrap();
        db.unlink("p/n10", "p/n11", "next");
    });
}

/// Edges added and withdrawn on both sides of a compaction, mixed with new nodes,
/// so the fold has to apply removals and additions in an order that survives both.
#[test]
fn edges_added_and_withdrawn_around_a_compaction() {
    run("EDGE-CHURN", |db| {
        db.link("p/n2", "p/n8", "next");
        db.unlink("p/n2", "p/n8", "next");
        db.link("p/n3", "p/n9", "next");
        db.compact().unwrap();
        db.unlink("p/n3", "p/n9", "next");
        db.link("p/n4", "p/n7", "next");
        seed(db, 40..45);
    });
}

// ── mutating comparison ──────────────────────────────────────────────────────
//
// Everything above compares the storage modes on *reading the same data*. Every
// write happens in `build_with`, before the comparison starts, so the path that
// takes a write from the RAM overlay down into the durable store was never
// compared at all — it was exercised once, identically, in setup.
//
// That gap is where the bugs were. Five of the seven found in one afternoon lived
// there: UPDATE not maintaining a btree index, ALTER TABLE changing a schema
// without touching a row, DROP TABLE dropping nothing. All of them read the
// overlay directly and so did nothing on a paged store, and every read probe above
// happily agreed, because the data they compared had been written before the bug
// could act on it.
//
// So: apply a mutation to every mode, then compare every read probe, then apply
// the next. A divergence names the mutation that caused it rather than appearing
// at the end of a long script.

/// One step: a name, and the mutation applied to each mode in turn.
type Step = (&'static str, fn(&mut CoreDB));

fn steps() -> Vec<Step> {
    vec![
        ("put new rows", |db| { seed(db, 100..110); }),
        ("put over existing rows", |db| { seed(db, 0..5); }),
        ("remove rows", |db| { for i in 20..25 { db.remove(&format!("p/n{i}")); } }),
        ("SQL UPDATE on an indexed column", |db| {
            db.execute("UPDATE p SET n = 5000 WHERE n = 12").unwrap();
        }),
        ("SQL UPDATE on a text column", |db| {
            db.execute("UPDATE p SET name = 'renamed' WHERE n < 4").unwrap();
        }),
        ("SQL DELETE by predicate", |db| {
            db.execute("DELETE FROM p WHERE n > 26 AND n < 29").unwrap();
        }),
        ("link new edges", |db| {
            for i in 100..108 { db.link(&format!("p/n{i}"), &format!("p/n{}", i + 1), "next"); }
        }),
        ("unlink an edge", |db| { db.unlink("p/n1", "p/n2", "next"); }),
        ("link_meta then update_edge", |db| {
            db.link_meta("p/n0", "p/n9", "tagged", &json!({"weight": 1}).to_string()).unwrap();
            db.update_edge("p/n0", "p/n9", "tagged", "{}", &json!({"weight": 42}).to_string());
        }),
        ("unlink_where by attribute", |db| {
            db.unlink_where("p/n0", "p/n9", "tagged", &json!({"weight": 42}).to_string());
        }),
        ("compact", |db| { db.compact().unwrap(); }),
        ("write after a compaction", |db| { seed(db, 200..205); }),
        ("remove after a compaction", |db| { for i in 200..202 { db.remove(&format!("p/n{i}")); } }),
        ("a transaction that commits", |db| {
            let mut tx = db.begin();
            for i in 300..305 {
                tx.put(&format!("p/n{i}"),
                       &json!({"_collection":"p","_key":format!("n{i}"),"n":i as i64,
                               "name":format!("row {i}"),"body":"the quick brown fox",
                               "grp": (i % 3) as i64}).to_string()).unwrap();
            }
            tx.link("p/n300", "p/n301", "next");
            tx.commit().unwrap();
        }),
        ("a transaction that rolls back", |db| {
            let mut tx = db.begin();
            tx.put("p/n999", &json!({"_collection":"p","_key":"n999","n":999}).to_string()).unwrap();
            tx.rollback();
        }),
        ("bulk write", |db| {
            let rows: Vec<(String, serde_json::Value)> = (400..410)
                .map(|i| (format!("p/n{i}"), json!({
                    "_collection":"p","_key":format!("n{i}"),"n":i as i64,
                    "name":format!("row {i}"),"body":"the quick brown fox",
                    "grp": (i % 3) as i64})))
                .collect();
            db.put_value_bulk(rows).unwrap();
        }),
        ("vectors written after the index exists", |db| {
            for i in 400..405 {
                db.put_vector(&format!("p/n{i}"), "vec",
                              &[i as f32 * 0.1, 1.0, 2.0]).unwrap();
            }
        }),
        ("geometry written after the index exists", |db| {
            for i in 405..408 {
                db.put(&format!("p/n{i}"), &json!({
                    "_collection":"p","_key":format!("n{i}"),"n":i as i64,
                    "name":format!("row {i}"),"body":"the quick brown fox",
                    "grp":0,"geometry":{"type":"Point","coordinates":[144.96, -37.81]}
                }).to_string()).unwrap();
            }
            db.build_spatial_index();
        }),
        ("compact again", |db| { db.compact().unwrap(); }),
        ("ALTER TABLE ADD COLUMN", |db| {
            db.execute("ALTER TABLE p ADD COLUMN extra TEXT").unwrap();
        }),
        ("ALTER TABLE DROP COLUMN", |db| {
            db.execute("ALTER TABLE p DROP COLUMN grp").unwrap();
        }),
        ("final compaction", |db| { db.compact().unwrap(); }),
    ]
}

/// **The permanent net.** Every mutation, compared across every storage mode,
/// immediately after it happens.
#[test]
fn every_mutation_leaves_the_modes_agreeing() {
    let mut built: Vec<(&str, tempfile::TempDir, CoreDB)> = Vec::new();
    for (label, cfg) in modes() {
        let dir = tempfile::TempDir::new().unwrap();
        build_with(dir.path(), cfg.clone());
        let db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
        built.push((label, dir, db));
    }

    for (step, apply) in steps() {
        for (_, _, db) in built.iter_mut() {
            apply(db);
        }
        let mut bad = Vec::new();
        for (label, f) in &probes() {
            let reference = f(&mut built[0].2);
            for (mode, _, db) in built.iter_mut().skip(1) {
                let got = f(db);
                if got != reference {
                    bad.push(format!(
                        "  {label:<24} resident ={reference}\n  {:<24} {mode:<9}={got}", ""));
                }
            }
        }
        assert!(bad.is_empty(),
                "after `{step}` the storage modes disagree — a write reached one and \
                 not the others:\n{}", bad.join("\n"));
        // And ground truth, so a mutation that is wrong in *every* mode is caught
        // too rather than agreed upon. The step is named in the mode label, because
        // "ILIKE disagrees" without knowing which write caused it is a bug report
        // with the useful half missing.
        for (label, _, db) in built.iter() {
            ground_truth(db, &format!("{label} after `{step}`"));
        }
    }
}
