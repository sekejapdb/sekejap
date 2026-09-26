//! The acceptance workloads, end to end (M5-G of
//! `docs/lang/GQL_PROFILE_DESIGN_M5_M7.md` §2.7): each query SHAPE the brief
//! names, re-expressed with invented tourism data, checked against an
//! oracle that enumerates the inserted edges directly.
//!
//! * two relationship routes combined, duplicates removed -- `UNION`
//!   (`two_routes_to_a_troupes_dances_are_one_set`);
//! * per-input local aggregation over either-direction edges, each stored
//!   edge counted once -- `CALL` with `COUNT(DISTINCT)`
//!   (`each_members_partners_are_counted_once_per_partner`);
//! * batched seeds and bounded quantifier coverage -- `FOR k IN $1`,
//!   `{0,6}`, `IN $2` (`batched_skills_reach_their_jobs`);
//! * deep backward cause chains with an anti-join -- `{1,8}` backwards and
//!   `NOT EXISTS` (`every_incident_traces_back_to_its_root_causes`);
//! * optional evidence keeps the node -- `OPTIONAL MATCH` + `COUNT`
//!   (`every_site_is_kept_with_its_good_reviews_counted`).
//!
//! Collaboration is stored as one edge per event, with its own identity:
//! two events between one pair are two parallel edges, and an
//! either-direction match must not count one stored edge twice. The
//! workloads rank and trace evidence; they prove no causation.

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::{Database, EntityId, GraphContextId, QueryBudget};
use sekejap_core::Kind;
use sekejap_lang::{prepare_sql, Param, SqlResult, SqlRow, SqlValue};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use tempfile::TempDir;

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

// ── the fixture, as edge lists the oracles read ───────────────────────────

const TROUPES: [&str; 2] = ["troupe_a", "troupe_b"];
const DANCERS: [&str; 5] = ["d1", "d2", "d3", "d4", "d5"];
const DANCES: [&str; 4] = ["kecak", "legong", "barong", "fire dance"];
/// dancer -member_of-> troupe
const MEMBER_OF: [(&str, &str); 5] = [
    ("d1", "troupe_a"),
    ("d2", "troupe_a"),
    ("d3", "troupe_a"),
    ("d4", "troupe_b"),
    ("d5", "troupe_b"),
];
/// troupe or dancer -performs-> dance
const PERFORMS: [(&str, &str); 6] = [
    ("troupe_a", "kecak"),
    ("troupe_a", "legong"),
    ("d1", "legong"),
    ("d2", "barong"),
    ("d4", "fire dance"),
    ("d5", "kecak"),
];
/// dancer -collaborates-> dancer, one edge per event: d1-d2 twice (two
/// events, parallel edges) and once each way between d2 and d3.
const COLLABORATES: [(&str, &str); 5] = [
    ("d1", "d2"),
    ("d1", "d2"),
    ("d2", "d3"),
    ("d3", "d2"),
    ("d4", "d5"),
];
const SKILLS: [&str; 6] = ["snorkelling", "free_diving", "boat_handling", "navigation", "first_aid", "reef_survey"];
/// skill -enables-> skill (with a cycle navigation <-> boat_handling)
const ENABLES: [(&str, &str); 5] = [
    ("snorkelling", "free_diving"),
    ("free_diving", "reef_survey"),
    ("boat_handling", "navigation"),
    ("navigation", "boat_handling"),
    ("first_aid", "first_aid"),
];
const JOBS: [&str; 3] = ["dive_guide", "reef_monitor", "harbour_pilot"];
/// skill -useful_for-> job
const USEFUL_FOR: [(&str, &str); 4] = [
    ("free_diving", "dive_guide"),
    ("reef_survey", "reef_monitor"),
    ("navigation", "harbour_pilot"),
    ("first_aid", "dive_guide"),
];
const INCIDENTS: [&str; 7] = ["reef_damage", "boat_anchor", "storm", "erosion", "footpath", "crowding", "tide"];
/// cause -causes-> effect: two chains that share a root
const CAUSES: [(&str, &str); 6] = [
    ("boat_anchor", "reef_damage"),
    ("storm", "boat_anchor"),
    ("tide", "storm"),
    ("footpath", "erosion"),
    ("crowding", "footpath"),
    ("storm", "erosion"),
];
const SITES: [&str; 3] = ["temple_hill", "rice_terrace", "lagoon_view"];
/// review -about-> site, with its stars
const REVIEWS: [(&str, &str, i64); 4] = [
    ("r1", "temple_hill", 5),
    ("r2", "temple_hill", 2),
    ("r3", "rice_terrace", 4),
    ("r4", "rice_terrace", 1),
];

fn fixture(dir: &TempDir) -> Database {
    let mut db = Database::create(dir.path().join("workloads.sekejap"), cfg()).unwrap();
    let text = |name: &str| (name.to_owned(), Kind::Text);
    let mut ids: BTreeMap<String, EntityId> = BTreeMap::new();
    let coll = |db: &mut Database, name: &str, keys: &[&str], ids: &mut BTreeMap<String, EntityId>| {
        let c = db.create_collection(name, vec![text("name")], Default::default()).unwrap();
        for key in keys {
            ids.insert((*key).to_owned(), db.put(c, key, &json!({"name": key})).unwrap());
        }
    };
    coll(&mut db, "troupe", &TROUPES, &mut ids);
    coll(&mut db, "dancer", &DANCERS, &mut ids);
    coll(&mut db, "dance", &DANCES, &mut ids);
    coll(&mut db, "skill", &SKILLS, &mut ids);
    coll(&mut db, "job", &JOBS, &mut ids);
    coll(&mut db, "incident", &INCIDENTS, &mut ids);
    coll(&mut db, "site", &SITES, &mut ids);
    let review = db
        .create_collection("review", vec![("stars".to_owned(), Kind::Int)], Default::default())
        .unwrap();
    for (key, _, stars) in REVIEWS {
        ids.insert(key.to_owned(), db.put(review, key, &json!({"stars": stars})).unwrap());
    }
    db.enable_graph().unwrap();
    let base = GraphContextId::BASE;
    let link = |db: &mut Database, t: &str, edges: &[(&str, &str)]| {
        let edge_type = db.create_edge_type(t).unwrap();
        for (event, (from, to)) in edges.iter().enumerate() {
            db.create_edge(base, ids[*from], edge_type, ids[*to], &json!({"event": event}))
                .unwrap();
        }
    };
    link(&mut db, "member_of", &MEMBER_OF);
    link(&mut db, "performs", &PERFORMS);
    link(&mut db, "collaborates", &COLLABORATES);
    link(&mut db, "enables", &ENABLES);
    link(&mut db, "useful_for", &USEFUL_FOR);
    link(&mut db, "causes", &CAUSES);
    let about: Vec<(&str, &str)> = REVIEWS.iter().map(|(r, s, _)| (*r, *s)).collect();
    link(&mut db, "about", &about);
    db.commit().unwrap();
    db
}

fn rows(db: &Database, body: &str, params: &[Param]) -> BTreeSet<Vec<String>> {
    let text = format!("SELECT * FROM GRAPH_TABLE (base {body})");
    let answer = prepare_sql(db, &text, params)
        .and_then(|prepared| prepared.run(db))
        .unwrap_or_else(|error| panic!("`{text}`: {error}"));
    let SqlResult::Rows { rows, .. } = answer else {
        panic!("`{text}` gave no rows")
    };
    let mut seen = BTreeSet::new();
    for row in rows {
        let row: Vec<String> = row
            .values
            .iter()
            .map(|value| match value {
                SqlValue::Text(text) => text.clone(),
                SqlValue::Int(i) => i.to_string(),
                SqlValue::Bool(b) => b.to_string(),
                SqlValue::Null => "NULL".to_owned(),
                other => format!("{other:?}"),
            })
            .collect();
        // Every workload answer is a set: a duplicate row is a defect.
        assert!(seen.insert(row.clone()), "`{body}` gave {row:?} twice");
    }
    seen
}

fn row(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| (*v).to_owned()).collect()
}

// ── the workloads ─────────────────────────────────────────────────────────

#[test]
fn two_routes_to_a_troupes_dances_are_one_set() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let got = rows(
        &db,
        "MATCH (t IS troupe WHERE t._key = 'troupe_a')-[:performs]->(k IS dance) RETURN k._key AS dance \
         UNION \
         MATCH (t IS troupe WHERE t._key = 'troupe_a')<-[:member_of]-(d IS dancer)-[:performs]->(k IS dance) RETURN k._key AS dance",
        &[],
    );
    let members: BTreeSet<&str> = MEMBER_OF.iter().filter(|(_, t)| *t == "troupe_a").map(|(d, _)| *d).collect();
    let expected: BTreeSet<Vec<String>> = PERFORMS
        .iter()
        .filter(|(who, _)| *who == "troupe_a" || members.contains(who))
        .map(|(_, dance)| row(&[dance]))
        .collect();
    assert_eq!(got, expected);
}

#[test]
fn each_members_partners_are_counted_once_per_partner() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let got = rows(
        &db,
        "MATCH (t IS troupe WHERE t._key = 'troupe_a')<-[:member_of]-(d IS dancer) \
         CALL (d) { MATCH (d)-[:collaborates]-(o IS dancer) RETURN COUNT(DISTINCT o) AS partners } \
         RETURN d._key AS dancer, partners",
        &[],
    );
    let expected: BTreeSet<Vec<String>> = MEMBER_OF
        .iter()
        .filter(|(_, t)| *t == "troupe_a")
        .map(|(d, _)| {
            let partners: BTreeSet<&str> = COLLABORATES
                .iter()
                .filter_map(|(a, b)| match (*a == *d, *b == *d) {
                    (true, _) => Some(*b),
                    (_, true) => Some(*a),
                    _ => None,
                })
                .collect();
            row(&[d, &partners.len().to_string()])
        })
        .collect();
    assert_eq!(got, expected);
}

#[test]
fn batched_skills_reach_their_jobs() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let seeds = ["snorkelling", "free_diving", "boat_handling"];
    let wanted = ["dive_guide", "reef_monitor"];
    let got = rows(
        &db,
        "FOR k IN $1 \
         MATCH (s IS skill WHERE s._key = k)-[:enables]->{0,6}(t IS skill)-[:useful_for]->(j IS job) \
         FILTER j._key IN $2 \
         RETURN DISTINCT k AS skill, j._key AS job",
        &[Param::Json(json!(seeds)), Param::Json(json!(wanted))],
    );
    let mut expected = BTreeSet::new();
    for seed in seeds {
        // Every skill within 0..=6 enables-hops of the seed.
        let mut reached: BTreeSet<&str> = BTreeSet::from([seed]);
        let mut frontier = vec![seed];
        for _ in 0..6 {
            let next: Vec<&str> = ENABLES
                .iter()
                .filter(|(from, _)| frontier.contains(from))
                .map(|(_, to)| *to)
                .collect();
            reached.extend(next.iter().copied());
            frontier = next;
        }
        for (skill, job) in USEFUL_FOR {
            if reached.contains(skill) && wanted.contains(&job) {
                expected.insert(row(&[seed, job]));
            }
        }
    }
    assert_eq!(got, expected);
}

#[test]
fn every_incident_traces_back_to_its_root_causes() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let seeds = ["reef_damage", "erosion"];
    let got = rows(
        &db,
        "FOR k IN $1 \
         MATCH (i IS incident WHERE i._key = k)<-[:causes]-{1,8}(c IS incident) \
         FILTER NOT EXISTS { MATCH (c)<-[:causes]-(x) } \
         RETURN DISTINCT k AS incident, c._key AS root",
        &[Param::Json(json!(seeds))],
    );
    let mut expected = BTreeSet::new();
    for seed in seeds {
        let mut ancestors: BTreeSet<&str> = BTreeSet::new();
        let mut frontier = vec![seed];
        for _ in 0..8 {
            let next: Vec<&str> = CAUSES
                .iter()
                .filter(|(_, effect)| frontier.contains(effect))
                .map(|(cause, _)| *cause)
                .collect();
            ancestors.extend(next.iter().copied());
            frontier = next;
        }
        for cause in ancestors {
            if !CAUSES.iter().any(|(_, effect)| *effect == cause) {
                expected.insert(row(&[seed, cause]));
            }
        }
    }
    assert_eq!(got, expected);
}

#[test]
fn every_site_is_kept_with_its_good_reviews_counted() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let got = rows(
        &db,
        "MATCH (s IS site) \
         OPTIONAL MATCH (s)<-[:about]-(r IS review) WHERE r.stars >= 4 \
         RETURN s._key AS site, COUNT(r) AS good GROUP BY s._key",
        &[],
    );
    let expected: BTreeSet<Vec<String>> = SITES
        .iter()
        .map(|site| {
            let good = REVIEWS.iter().filter(|(_, s, stars)| s == site && *stars >= 4).count();
            row(&[site, &good.to_string()])
        })
        .collect();
    assert_eq!(got, expected);
}

// ── M5 features in combination (owner: every combination lean-tested) ───

/// `body`'s answer equals `expected`, and paging it one and two rows at a
/// time hands out exactly the one-shot rows (M3-E), so no combination of
/// the M5 operators loses or repeats a row across a page boundary.
fn combo(db: &Database, body: &str, params: &[Param], expected: BTreeSet<Vec<String>>) {
    let one_shot = rows(db, body, params);
    assert_eq!(one_shot, expected, "`{body}`");
    let text = format!("SELECT * FROM GRAPH_TABLE (base {body})");
    for page_rows in [1, 2] {
        let prepared = prepare_sql(db, &text, params).unwrap();
        let mut paged = BTreeSet::new();
        prepared
            .for_each_row_with(db, page_rows, QueryBudget::unlimited(), &mut || false, &mut |row: &SqlRow| {
                let row: Vec<String> = row
                    .values
                    .iter()
                    .map(|value| match value {
                        SqlValue::Text(text) => text.clone(),
                        SqlValue::Int(i) => i.to_string(),
                        SqlValue::Bool(b) => b.to_string(),
                        SqlValue::Null => "NULL".to_owned(),
                        other => format!("{other:?}"),
                    })
                    .collect();
                assert!(paged.insert(row), "`{body}` repeated a row at page size {page_rows}");
                Ok(())
            })
            .unwrap_or_else(|error| panic!("`{body}` paged at {page_rows}: {error}"));
        assert_eq!(paged, one_shot, "`{body}` paged at {page_rows}");
    }
}

fn members(troupe: &str) -> BTreeSet<&'static str> {
    MEMBER_OF.iter().filter(|(_, t)| *t == troupe).map(|(d, _)| *d).collect()
}

fn performs(who: &str) -> Vec<&'static str> {
    PERFORMS.iter().filter(|(w, _)| *w == who).map(|(_, k)| *k).collect()
}

fn partners(dancer: &str) -> BTreeSet<&'static str> {
    COLLABORATES
        .iter()
        .filter_map(|(a, b)| match (*a == dancer, *b == dancer) {
            (true, _) => Some(*b),
            (_, true) => Some(*a),
            _ => None,
        })
        .collect()
}

#[test]
fn combination_exists_inside_union_branches() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let mut expected: BTreeSet<Vec<String>> = members("troupe_a")
        .into_iter()
        .filter(|d| !performs(d).is_empty())
        .map(|d| row(&[d]))
        .collect();
    expected.extend(members("troupe_b").into_iter().filter(|d| !partners(d).is_empty()).map(|d| row(&[d])));
    combo(
        &db,
        "MATCH (t IS troupe WHERE t._key = 'troupe_a')<-[:member_of]-(d IS dancer) \
         FILTER EXISTS { MATCH (d)-[:performs]->(k IS dance) } RETURN d._key AS dancer \
         UNION \
         MATCH (t IS troupe WHERE t._key = 'troupe_b')<-[:member_of]-(d IS dancer) \
         FILTER EXISTS { MATCH (d)-[:collaborates]-(o IS dancer) } RETURN d._key AS dancer",
        &[],
        expected,
    );
}

#[test]
fn combination_call_inside_union_branches() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let mut expected = BTreeSet::from([row(&["d1", &performs("d1").len().to_string()])]);
    for troupe in TROUPES {
        expected.insert(row(&[troupe, &performs(troupe).len().to_string()]));
    }
    combo(
        &db,
        "MATCH (d IS dancer WHERE d._key = 'd1') \
         CALL (d) { MATCH (d)-[:performs]->(k IS dance) RETURN COUNT(*) AS n } RETURN d._key AS who, n \
         UNION ALL \
         MATCH (t IS troupe) \
         CALL (t) { MATCH (t)-[:performs]->(k IS dance) RETURN COUNT(*) AS n } RETURN t._key AS who, n",
        &[],
        expected,
    );
}

#[test]
fn combination_not_exists_inside_a_call_body() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let expected = TROUPES
        .iter()
        .map(|troupe| {
            let idle = members(troupe).into_iter().filter(|d| performs(d).is_empty()).count();
            row(&[troupe, &idle.to_string()])
        })
        .collect();
    combo(
        &db,
        "MATCH (t IS troupe) \
         CALL (t) { MATCH (t)<-[:member_of]-(d IS dancer) FILTER NOT EXISTS { MATCH (d)-[:performs]->(k) } \
                    RETURN COUNT(*) AS idle } \
         RETURN t._key AS troupe, idle",
        &[],
        expected,
    );
}

#[test]
fn combination_comma_optional_match_inside_a_call_body() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // Per member: one row per (dance performed, outgoing collaboration
    // edge) pair, both optional together; COUNT(k) counts the matched rows.
    let expected = TROUPES
        .iter()
        .map(|troupe| {
            let both: usize = members(troupe)
                .into_iter()
                .map(|d| performs(d).len() * COLLABORATES.iter().filter(|(a, _)| *a == d).count())
                .sum();
            row(&[troupe, &both.to_string()])
        })
        .collect();
    combo(
        &db,
        "MATCH (t IS troupe) \
         CALL (t) { MATCH (t)<-[:member_of]-(d IS dancer) \
                    OPTIONAL MATCH (d)-[:performs]->(k IS dance), (d)-[:collaborates]->(o IS dancer) \
                    RETURN COUNT(k) AS both } \
         RETURN t._key AS troupe, both",
        &[],
        expected,
    );
}

#[test]
fn combination_call_after_next() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let expected = TROUPES
        .iter()
        .map(|troupe| row(&[troupe, &members(troupe).len().to_string()]))
        .collect();
    combo(
        &db,
        "MATCH (t IS troupe) RETURN t AS t, t._key AS troupe \
         NEXT CALL (t) { MATCH (t)<-[:member_of]-(d IS dancer) RETURN COUNT(*) AS members } \
         RETURN troupe, members",
        &[],
        expected,
    );
}

#[test]
fn combination_union_after_next_with_exists_branches() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let mut expected: BTreeSet<Vec<String>> = DANCERS
        .iter()
        .filter(|d| performs(d).contains(&"kecak"))
        .map(|d| row(&[d]))
        .collect();
    expected.extend(members("troupe_b").into_iter().map(|d| row(&[d])));
    combo(
        &db,
        "MATCH (d IS dancer) RETURN d AS d, d._key AS dancer \
         NEXT FILTER EXISTS { MATCH (d)-[:performs]->(k IS dance WHERE k._key = 'kecak') } RETURN dancer \
         UNION \
         FILTER EXISTS { MATCH (d)-[:member_of]->(t IS troupe WHERE t._key = 'troupe_b') } RETURN dancer",
        &[],
        expected,
    );
}

#[test]
fn combination_not_exists_mark_beside_a_call() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let in_a = members("troupe_a");
    let expected = DANCERS
        .iter()
        .map(|d| {
            let outsider = !in_a.contains(d);
            row(&[d, &performs(d).len().to_string(), &outsider.to_string()])
        })
        .collect();
    combo(
        &db,
        "MATCH (d IS dancer) \
         CALL (d) { MATCH (d)-[:performs]->(k IS dance) RETURN COUNT(*) AS dances } \
         RETURN d._key AS dancer, dances, \
                NOT EXISTS { MATCH (d)-[:member_of]->(t IS troupe WHERE t._key = 'troupe_a') } AS outsider",
        &[],
        expected,
    );
}
