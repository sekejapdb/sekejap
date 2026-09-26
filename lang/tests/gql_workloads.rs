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
//!   (`every_site_is_kept_with_its_good_reviews_counted`);
//! * (M6-J) hybrid top-k, then deep backward cause chains, an anti-join and
//!   two aggregations, as brief §9.5 writes it: text AND radius seeding the
//!   incidents, read in exact vector order
//!   (`hybrid_top_k_incidents_trace_back_to_their_root_causes`), and the M6
//!   forms in combination with the M5 operators (`combination_m6_*`).
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
use sekejap_lang::{explain_sql, prepare_sql, Param, SqlDatabase, SqlResult, SqlRow, SqlValue};
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
/// Each incident's `(body, lon, lat, embedding, realm)`, M6-J's hybrid first
/// stage reads them (brief §9.5).
const INCIDENT_ROWS: [(&str, f64, f64, [f32; 3], &str); 7] = [
    ("reef damage near uluwatu", 115.0849, -8.8291, [1.0, 0.0, 0.0], "sea"),
    ("boat anchor dragged over the reef", 115.10, -8.80, [0.9, 0.1, 0.0], "sea"),
    ("storm swell from the south", 115.60, -8.40, [0.5, 0.5, 0.0], "sea"),
    ("beach erosion at seminyak", 115.158, -8.691, [0.0, 1.0, 0.0], "land"),
    ("footpath across the dunes", 115.16, -8.70, [0.0, 0.9, 0.1], "land"),
    ("crowding at the beach", 115.17, -8.71, [0.0, 0.5, 0.5], "land"),
    ("king tide", 115.30, -8.95, [0.3, 0.0, 0.7], "sea"),
];
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
    // The incidents carry the hybrid fields, with the indexes that seed them.
    db.sql(
        "CREATE TABLE incident (name TEXT, body TEXT, loc GEOMETRY(Point,4326), emb VECTOR(3) NOT NULL, realm TEXT) \
         WITH (index: none)",
        &[],
    )
    .unwrap();
    for (key, (body, lon, lat, emb, realm)) in INCIDENTS.iter().zip(INCIDENT_ROWS) {
        db.sql(
            "INSERT INTO incident (_key, name, body, loc, emb, realm) VALUES ($1, $1, $2, $3, $4, $5)",
            &[
                Param::Text((*key).into()),
                Param::Text(body.into()),
                Param::Text(format!(r#"{{"type":"Point","coordinates":[{lon:?},{lat:?}]}}"#)),
                Param::Vector(emb.to_vec()),
                Param::Text(realm.into()),
            ],
        )
        .unwrap();
    }
    db.sql("COMMIT", &[]).unwrap();
    for ddl in [
        "CREATE INDEX incident_body ON incident USING gin (to_tsvector('simple', body))",
        "CREATE INDEX incident_loc ON incident USING gist (loc)",
        "CREATE INDEX incident_emb ON incident USING exact (emb)",
        "CREATE INDEX incident_realm ON incident USING btree (realm)",
    ] {
        db.sql(ddl, &[]).unwrap();
    }
    let incident = db.collection("incident").unwrap().unwrap();
    for key in INCIDENTS {
        ids.insert(key.to_owned(), db.get(incident, key).unwrap().unwrap().id);
    }
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
        for cause in root_causes(seed) {
            expected.insert(row(&[seed, cause]));
        }
    }
    assert_eq!(got, expected);
}

/// The causes of `effect` up to eight hops back that nothing causes.
fn root_causes(effect: &str) -> BTreeSet<&'static str> {
    let mut ancestors: BTreeSet<&str> = BTreeSet::new();
    let mut frontier = vec![effect];
    for _ in 0..8 {
        let next: Vec<&str> = CAUSES
            .iter()
            .filter(|(_, effect)| frontier.contains(effect))
            .map(|(cause, _)| *cause)
            .collect();
        ancestors.extend(next.iter().copied());
        frontier = next;
    }
    ancestors
        .into_iter()
        .filter(|cause| !CAUSES.iter().any(|(_, effect)| effect == cause))
        .collect()
}

/// Great-circle metres on a sphere of the WGS84 mean radius: the oracle's
/// radius test, used only where every incident is kilometres from the edge.
fn haversine_m((lon1, lat1): (f64, f64), (lon2, lat2): (f64, f64)) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let (dp, dl) = ((lat2 - lat1).to_radians(), (lon2 - lon1).to_radians());
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * 6_371_008.8 * a.sqrt().asin()
}

/// Brief §9.5 as written (M6-J): the top `k` incidents that match any of
/// `words`, lie within `metres` of `centre`, ranked by L2 distance to
/// `query`; then each one's root causes up to eight hops back, counted and
/// the first named.
fn hybrid_oracle(
    words: &[&str],
    centre: (f64, f64),
    metres: f64,
    query: [f32; 3],
    k: usize,
) -> BTreeSet<Vec<String>> {
    let mut candidates: Vec<(f64, &str)> = INCIDENTS
        .iter()
        .zip(INCIDENT_ROWS)
        .filter(|(_, (body, ..))| body.split(' ').any(|word| words.contains(&word)))
        .filter(|(_, (_, lon, lat, ..))| {
            let d = haversine_m(centre, (*lon, *lat));
            assert!((d - metres).abs() > 1000.0, "an incident {d} m away is too close to the radius");
            d <= metres
        })
        .map(|(key, (_, _, _, emb, _))| {
            let squared: f64 = emb.iter().zip(query).map(|(a, b)| f64::from(a - b).powi(2)).sum();
            (squared.sqrt(), *key)
        })
        .collect();
    candidates.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(b.1)));
    candidates
        .into_iter()
        .take(k)
        .filter_map(|(_, key)| {
            let roots = root_causes(key);
            let first = roots.iter().next()?;
            Some(row(&[key, &roots.len().to_string(), first]))
        })
        .collect()
}

/// The hybrid body of brief §9.5, `$1` the words, `$2 $3` the centre, `$4`
/// the radius, `$5` the query vector, `$6` the k.
const HYBRID: &str = "MATCH (i IS incident WHERE to_tsvector('simple', i.body) @@ to_tsquery('simple', $1) \
                      AND ST_DWithin(i.loc, ST_MakePoint($2, $3)::geography, $4)) \
                      RETURN i, i.emb <-> $5::vector AS d ORDER BY d, i._key LIMIT $6 \
                      NEXT MATCH (i)<-[:causes]-{1,8}(c IS incident) \
                      FILTER NOT EXISTS { MATCH (c)<-[:causes]-(x) } \
                      RETURN i._key AS incident, COUNT(DISTINCT c) AS roots, MIN(c._key) AS first_root";

#[test]
fn hybrid_top_k_incidents_trace_back_to_their_root_causes() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let centre = (115.17, -8.72);
    for (words, query, k) in [
        ("reef | erosion", [1.0f32, 0.0, 0.0], 2i64),
        ("reef | erosion", [1.0, 0.0, 0.0], 3),
        ("reef | erosion", [0.0, 1.0, 0.0], 1),
        ("tide | storm | crowding", [0.0, 0.0, 1.0], 5),
    ] {
        let params = [
            Param::Text(words.into()),
            Param::Float(centre.0),
            Param::Float(centre.1),
            Param::Int(20_000),
            Param::Vector(query.to_vec()),
            Param::Int(k),
        ];
        let split: Vec<&str> = words.split(" | ").collect();
        let expected = hybrid_oracle(&split, centre, 20_000.0, query, k as usize);
        combo(&db, HYBRID, &params, expected);
    }
    // The first stage is ONE seed: text and radius through their indexes,
    // read in the exact vector index's order, and the sort stops early.
    let plan = explain_sql(
        &db,
        &format!("SELECT * FROM GRAPH_TABLE (base {HYBRID})"),
        &[
            Param::Text("reef".into()),
            Param::Float(115.17),
            Param::Float(-8.72),
            Param::Int(20_000),
            Param::Vector(vec![1.0, 0.0, 0.0]),
            Param::Int(2),
        ],
    )
    .unwrap();
    for part in ["`incident_body`", "`incident_loc`", "`incident_emb` (ordered by", "stops at the first row past"] {
        assert!(plan.contains(part), "{part} not in:\n{plan}");
    }
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

// ── M6 features in combination ───────────────────────────────────────────

fn incident_row(key: &str) -> (&'static str, f64, f64, [f32; 3], &'static str) {
    INCIDENT_ROWS[INCIDENTS.iter().position(|k| *k == key).unwrap()]
}

#[test]
fn combination_m6_lineage_moved_filter_before_a_call() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let body = "MATCH (i IS incident) RETURN i AS i NEXT FILTER i.realm = 'land' \
                CALL (i) { MATCH (i)<-[:causes]-(c IS incident) RETURN COUNT(*) AS direct } \
                RETURN i._key AS incident, direct";
    let expected = INCIDENTS
        .iter()
        .filter(|key| incident_row(key).4 == "land")
        .map(|key| {
            let direct = CAUSES.iter().filter(|(_, effect)| effect == key).count();
            row(&[key, &direct.to_string()])
        })
        .collect();
    combo(&db, body, &[], expected);
    let plan = explain_sql(&db, &format!("SELECT * FROM GRAPH_TABLE (base {body})"), &[]).unwrap();
    assert!(plan.contains("moved from FILTER by lineage"), "{plan}");
}

#[test]
fn combination_m6_text_match_on_a_far_node_inside_exists() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let expected = INCIDENTS
        .iter()
        .filter(|key| {
            CAUSES
                .iter()
                .any(|(cause, effect)| effect == *key && incident_row(cause).0.split(' ').any(|w| w == "storm"))
        })
        .map(|key| row(&[key]))
        .collect();
    combo(
        &db,
        "MATCH (i IS incident) WHERE EXISTS { MATCH (i)<-[:causes]-(c IS incident \
         WHERE to_tsvector('simple', c.body) @@ to_tsquery('simple', 'storm')) } RETURN i._key AS incident",
        &[],
        expected,
    );
}

#[test]
fn combination_m6_spatial_seed_optional_match_and_union() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let centre = (115.17, -8.72);
    let near = |key: &str| {
        let (_, lon, lat, ..) = incident_row(key);
        let d = haversine_m(centre, (lon, lat));
        assert!((d - 5000.0).abs() > 1000.0, "{key} is {d} m away, too close to the radius");
        d <= 5000.0
    };
    let mut expected = BTreeSet::new();
    for key in INCIDENTS.iter().filter(|key| near(key) || incident_row(key).4 == "sea") {
        let causes: Vec<&str> = CAUSES.iter().filter(|(_, e)| e == key).map(|(c, _)| *c).collect();
        if causes.is_empty() {
            expected.insert(row(&[key, "NULL"]));
        }
        for cause in causes {
            expected.insert(row(&[key, cause]));
        }
    }
    combo(
        &db,
        "MATCH (i IS incident WHERE ST_DWithin(i.loc, ST_MakePoint(115.17, -8.72)::geography, 5000)) \
         OPTIONAL MATCH (i)<-[:causes]-(c IS incident) RETURN i._key AS incident, c._key AS cause \
         UNION MATCH (i IS incident WHERE i.realm = 'sea') \
         OPTIONAL MATCH (i)<-[:causes]-(c IS incident) RETURN i._key AS incident, c._key AS cause",
        &[],
        expected,
    );
}
