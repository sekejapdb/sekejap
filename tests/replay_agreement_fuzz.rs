//! What the log replays must be what was written — searched, not sampled.
//!
//! `compaction_agreement_fuzz` compacts and then reopens, so it exercises reading
//! a base that a compaction just wrote. This is the other half: reopening a
//! database that has **not** compacted, where every row has to come back out of
//! the write-ahead log, and reopening one that has written more *on top of* a
//! base, where replay lands on an existing generation.
//!
//! Those are the paths that decide whether a committed write survives a restart
//! at all, and the mutation mix here is chosen to make replay work for it:
//! inserts, in-place updates, deletes, deletes of rows that were already deleted,
//! rewrites of deleted keys, edges added and withdrawn.
//!
//! Not a correctness oracle — it does not know the right answer. It knows the
//! answer must not change across a restart.

use sekejap::{Config, CoreDB};
use serde_json::{json, Value};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 { if n == 0 { 0 } else { self.next() % n } }
}

fn gen_value(rng: &mut Rng) -> Option<Value> {
    match rng.below(9) {
        0 => None,
        1 => Some(Value::Null),
        2 => Some(json!(rng.below(6) as i64)),
        3 => Some(json!(-(rng.below(4) as i64))),
        4 => Some(json!(1.0)),
        5 => Some(json!("kucing")),
        6 => Some(json!("")),
        7 => Some(json!(true)),
        _ => Some(json!(format!("row {}", rng.below(4)))),
    }
}

fn row(i: usize, rng: &mut Rng, tag: &str) -> String {
    let mut o = serde_json::Map::new();
    o.insert("_collection".into(), json!("p"));
    o.insert("_key".into(), json!(format!("n{i}")));
    if let Some(v) = gen_value(rng) { o.insert("v".into(), v); }
    if let Some(w) = gen_value(rng) { o.insert("w".into(), w); }
    o.insert("body".into(), json!(format!("{tag} {i} riverbank")));
    Value::Object(o).to_string()
}

/// A mutation mix that gives replay something to get wrong.
fn churn(db: &mut CoreDB, rng: &mut Rng, rows: usize) {
    for i in 0..rows {
        match rng.below(8) {
            0 => { db.remove(&format!("p/n{i}")); }
            1 => {                                   // delete then write back
                db.remove(&format!("p/n{i}"));
                db.put(&format!("p/n{i}"), &row(i, rng, "reborn")).unwrap();
            }
            2 => { db.remove(&format!("p/n{i}")); db.remove(&format!("p/n{i}")); }
            3 => { db.put(&format!("p/n{i}"), &row(i, rng, "updated")).unwrap(); }
            4 => { db.link(&format!("p/n{i}"), &format!("p/n{}", (i + 2) % rows), "also"); }
            5 => { db.unlink(&format!("p/n{i}"), &format!("p/n{}", (i + 1) % rows), "next"); }
            _ => {}
        }
    }
}

const PROBES: &[(&str, bool)] = &[
    ("SELECT _key FROM p", false),
    ("SELECT _key FROM p WHERE w > 0", false),
    ("SELECT _key FROM p WHERE v IS NULL", false),
    ("SELECT _key FROM p WHERE v = 'kucing'", false),
    ("SELECT _key FROM p WHERE body ILIKE '%riverbank%'", false),
    // A tie-break, deliberately. `ORDER BY w` alone leaves rows with equal `w`
    // in an order SQL does not specify and PostgreSQL does not promise either —
    // here it falls out of base-versus-overlay iteration, which a restart is
    // entitled to change. Without the tie-break this probe reported eleven
    // "failures" that were all adjacent pairs of equal `w` swapping.
    //
    // The rows themselves are still checked, by the unordered probes above and by
    // `slugs`; what is relaxed is only the part that was never a guarantee.
    ("SELECT _key FROM p ORDER BY w ASC, _key ASC", false),
    ("SELECT COUNT(*), COUNT(w), SUM(w), MIN(w), MAX(w) FROM p", true),
    ("SELECT v, COUNT(*) FROM p GROUP BY v", true),
    ("SELECT b._key FROM MATCH (a:p)-[:next]->(b:p)", false),
    ("SELECT b._key FROM MATCH (a:p)-[:also]->(b:p)", false),
];

fn answers(db: &CoreDB) -> Vec<String> {
    let mut out = Vec::new();
    for (sql, values) in PROBES {
        let text = match db.query(sql) {
            Ok(set) => {
                let hits = set.collect();
                let mut rows: Vec<String> = hits.iter()
                    .map(|h| match &h.payload {
                        Some(p) if *values || h.slug.is_empty() => p.to_string(),
                        _ => h.slug.clone(),
                    })
                    .collect();
                if !sql.contains("ORDER BY") { rows.sort(); }
                rows.join("|")
            }
            Err(e) => format!("ERR({e:?})"),
        };
        out.push(format!("{sql} => {text}"));
    }
    out.push(format!("node_count => {}", db.node_count()));
    out.push(format!("edge_count => {}", db.edge_count()));
    let mut slugs = db.all_slugs();
    slugs.sort();
    out.push(format!("slugs => {}", slugs.join(",")));
    for s in &slugs {
        // The stored bytes, minus the wall-clock stamps, which differ by when a
        // row happened to be written and are not an answer.
        let text = db.get(s).and_then(|j| serde_json::from_str::<Value>(&j).ok())
            .map(|mut v| {
                if let Value::Object(m) = &mut v {
                    m.remove("_created_unix");
                    m.remove("_updated_unix");
                }
                serde_json::to_string(&v).unwrap_or_default()
            })
            .unwrap_or_default();
        out.push(format!("get({s}) => {text}"));
    }
    out
}

#[test]
fn a_restart_replays_exactly_what_was_written() {
    let rounds: u64 = std::env::var("SK_REPFUZZ_ROUNDS").ok()
        .and_then(|s| s.parse().ok()).unwrap_or(24);
    let seed: u64 = std::env::var("SK_REPFUZZ_SEED").ok()
        .and_then(|s| s.parse().ok()).unwrap_or(0x2E91A);

    let mut bad: Vec<String> = Vec::new();
    for round in 0..rounds {
        for compact_first in [false, true] {
            for (label, cfg) in [("default", Config::default()), ("resident", Config::resident())] {
                let rows = 2 + (round as usize % 24);
                let stream = seed.wrapping_add(round.wrapping_mul(0x9E37_79B9));
                let ctx = format!(
                    "round {round} (seed {seed}, {label}, {rows} rows, {})",
                    if compact_first { "replay onto a base" } else { "replay only" });

                let dir = tempfile::TempDir::new().unwrap();
                let before = {
                    let mut rng = Rng(stream);
                    let mut db = CoreDB::open_with_config(dir.path(), cfg.clone()).unwrap();
                    db.execute(
                        "CREATE TABLE p (_key TEXT PRIMARY KEY, v TEXT, w INTEGER, body TEXT)"
                    ).unwrap();
                    for i in 0..rows { db.put(&format!("p/n{i}"), &row(i, &mut rng, "first")).unwrap(); }
                    for i in 0..rows - 1 {
                        db.link(&format!("p/n{i}"), &format!("p/n{}", i + 1), "next");
                    }
                    // Optionally fold everything so far into a base, then keep
                    // writing — replay then lands on an existing generation.
                    if compact_first { db.compact().unwrap(); }
                    churn(&mut db, &mut rng, rows);
                    answers(&db)
                    // dropped here: no explicit flush, exactly as a process exiting
                };

                let db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
                for (b, a) in before.iter().zip(answers(&db)) {
                    if *b != a {
                        bad.push(format!("  {ctx}\n    before {b}\n    after  {a}"));
                    }
                }
            }
        }
    }
    assert!(bad.is_empty(),
            "a restart changed {} answer(s):\n{}", bad.len(), bad.join("\n"));
}
