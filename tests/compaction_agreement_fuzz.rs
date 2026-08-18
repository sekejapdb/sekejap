//! A compaction must not change a single answer — searched, not sampled.
//!
//! Compaction is the most dangerous code in the store: it rewrites the durable
//! half and swaps it in. Its invariant is simple and total — every query answers
//! the same before and after, and again after a restart — and the bugs it has
//! produced were all violations of exactly that, found one at a time by someone
//! noticing a wrong number.
//!
//! `differential_audit` checks this shape across storage modes with fixed data
//! and a fixed probe list. This searches instead: seeded random rows and edges,
//! answers recorded, compacted, re-checked, reopened, re-checked. Any difference
//! is a bug, and the seed reproduces it.
//!
//! Like the index-agreement fuzzer, it is not a correctness oracle. It does not
//! know the right answer; it knows the answer must not move.

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
        5 => Some(json!(2.5)),
        6 => Some(json!("apple")),
        7 => Some(json!("")),
        _ => Some(json!(format!("row {}", rng.below(4)))),
    }
}

/// Build a store, then mutate it so the overlay and the base both hold something:
/// writes, updates, deletes and edges, in a seeded order.
fn build(db: &mut CoreDB, rng: &mut Rng, rows: usize) {
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, v TEXT, w INTEGER, body TEXT)").unwrap();
    for i in 0..rows {
        let mut o = serde_json::Map::new();
        o.insert("_collection".into(), json!("p"));
        o.insert("_key".into(), json!(format!("n{i}")));
        if let Some(v) = gen_value(rng) { o.insert("v".into(), v); }
        if let Some(w) = gen_value(rng) { o.insert("w".into(), w); }
        o.insert("body".into(), json!(format!("record {i} riverbank fox")));
        db.put(&format!("p/n{i}"), &Value::Object(o).to_string()).unwrap();
    }
    db.execute("CREATE INDEX ON p USING btree (w)").unwrap();
    for i in 0..rows.saturating_sub(1) {
        if rng.below(3) != 0 { db.link(&format!("p/n{i}"), &format!("p/n{}", i + 1), "next"); }
    }
    // Churn: delete some, rewrite some, so tombstones and updates are in play.
    for i in 0..rows {
        match rng.below(6) {
            0 => { db.remove(&format!("p/n{i}")); }
            1 => {
                let mut o = serde_json::Map::new();
                o.insert("_collection".into(), json!("p"));
                o.insert("_key".into(), json!(format!("n{i}")));
                if let Some(v) = gen_value(rng) { o.insert("v".into(), v); }
                if let Some(w) = gen_value(rng) { o.insert("w".into(), w); }
                o.insert("body".into(), json!(format!("rewritten {i} riverbank")));
                db.put(&format!("p/n{i}"), &Value::Object(o).to_string()).unwrap();
            }
            _ => {}
        }
    }
}

/// `(query, compare_values_not_rows)`.
///
/// `DISTINCT` is the one shape where the *rows* are not the answer. It returns
/// one representative per distinct value, and which row gets to represent a group
/// depends on scan order — which a compaction is entitled to change. PostgreSQL
/// does not return row identity for `SELECT DISTINCT v` at all; the values are
/// the answer, so the values are what this compares. Pinning the representative
/// would be pinning something the engine never promised, and the fuzzer would
/// report a false failure on every round with a duplicate in it — which it did,
/// 42 times, before this distinction was drawn.
const PROBES: &[(&str, bool)] = &[
    ("SELECT _key FROM p", false),
    ("SELECT _key FROM p WHERE w > 1", false),
    ("SELECT _key FROM p WHERE w IS NULL", false),
    ("SELECT _key FROM p WHERE v = 'apple'", false),
    ("SELECT _key FROM p WHERE v != 'apple'", false),
    ("SELECT _key FROM p WHERE body ILIKE '%riverbank%'", false),
    ("SELECT _key FROM p ORDER BY w ASC", false),
    ("SELECT _key FROM p ORDER BY w DESC LIMIT 5", false),
    ("SELECT COUNT(*) FROM p", false),
    ("SELECT COUNT(w) FROM p", false),
    ("SELECT SUM(w), MIN(w), MAX(w) FROM p", false),
    ("SELECT v, COUNT(*) FROM p GROUP BY v", false),
    ("SELECT DISTINCT v FROM p", true),
    ("SELECT _key FROM MATCH (a:p)-[:next]->(b:p)", false),
];

fn answers(db: &CoreDB) -> Vec<String> {
    let mut out = Vec::with_capacity(PROBES.len() + 3);
    for (sql, values_only) in PROBES {
        let text = match db.query(sql) {
            Ok(set) => {
                let hits = set.collect();
                let mut rows: Vec<String> = hits.iter()
                    .map(|h| match &h.payload {
                        Some(p) if *values_only || h.slug.is_empty() => p.to_string(),
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
    let mut slugs = db.all_slugs(); slugs.sort();
    out.push(format!("all_slugs => {}", slugs.join(",")));
    out
}

fn diff(what: &str, ctx: &str, before: &[String], after: &[String], bad: &mut Vec<String>) {
    for (b, a) in before.iter().zip(after) {
        if b != a {
            bad.push(format!("  {ctx}: {what} changed an answer\n    before {b}\n    after  {a}"));
        }
    }
}

#[test]
fn a_compaction_changes_no_answer() {
    // `SK_COMPFUZZ_ROUNDS=200 SK_COMPFUZZ_SEED=... cargo test --release
    //  --test compaction_agreement_fuzz` for a campaign.
    let rounds: u64 = std::env::var("SK_COMPFUZZ_ROUNDS").ok()
        .and_then(|s| s.parse().ok()).unwrap_or(24);
    let seed: u64 = std::env::var("SK_COMPFUZZ_SEED").ok()
        .and_then(|s| s.parse().ok()).unwrap_or(0xC0FFEE);

    let mut bad: Vec<String> = Vec::new();
    for round in 0..rounds {
        for (label, cfg) in [("default", Config::default()), ("resident", Config::resident())] {
            let rows = 1 + (round as usize % 30);
            let stream = seed.wrapping_add(round.wrapping_mul(0x9E37_79B9));
            let ctx = format!("round {round} (seed {seed}, {label}, {rows} rows)");

            let dir = tempfile::TempDir::new().unwrap();
            let before;
            let after_compact;
            {
                let mut db = CoreDB::open_with_config(dir.path(), cfg.clone()).unwrap();
                build(&mut db, &mut Rng(stream), rows);
                before = answers(&db);
                db.compact().unwrap();
                after_compact = answers(&db);
                diff("a compaction", &ctx, &before, &after_compact, &mut bad);
            }
            // …and the same again once it has been closed and reopened, which is
            // where a base written wrongly finally shows.
            let db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
            diff("a compaction then a restart", &ctx, &before, &answers(&db), &mut bad);
        }
    }
    assert!(bad.is_empty(),
            "compaction changed {} answer(s):\n{}", bad.len(), bad.join("\n"));
}
