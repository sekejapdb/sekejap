//! An index may not change an answer — searched, rather than sampled.
//!
//! Several real bugs this branch fixed were index-vs-scan disagreements: `!=`
//! keeping NULL rows on one path, `COUNT` of a text column answering 0 once
//! indexed, `MIN`/`MAX` ignoring text through the index accumulator, `ORDER BY`
//! leading with NULLs on five separate index walks, `IN` matching every empty row
//! because a NULL in the list looked one up.
//!
//! Every one was found by hand-picking a case. This searches the space instead:
//! seeded random data, seeded random queries, each answered twice — once against
//! a collection with a btree index on the column and once against an identical
//! collection without one. Any disagreement is a bug in whichever path is wrong,
//! and the seed reproduces it exactly.
//!
//! Deliberately not a correctness oracle. It does not know what the right answer
//! is; it knows the two answers must match, which is the property an index is
//! *for* and the one that kept breaking.

use sekejap::CoreDB;
use serde_json::{json, Value};

/// Tiny deterministic PRNG — no dev-dependency, and a seed that reproduces.
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

/// A value from the awkward end of the space: nulls, missing fields, numbers that
/// are whole but written as floats, strings that look like numbers, negatives.
fn gen_value(rng: &mut Rng) -> Option<Value> {
    match rng.below(10) {
        0 => None,                                   // field absent entirely
        1 => Some(Value::Null),
        2 => Some(json!(0)),
        3 => Some(json!(-1)),
        4 => Some(json!(rng.below(5) as i64)),
        5 => Some(json!(1.0)),                       // whole, but a float
        6 => Some(json!(2.5)),
        7 => Some(json!("3")),                       // a number as text
        8 => Some(json!("apple")),
        _ => Some(json!("")),
    }
}

fn build(db: &mut CoreDB, coll: &str, rng: &mut Rng, rows: usize, indexed: bool) {
    db.execute(&format!(
        "CREATE TABLE {coll} (_key TEXT PRIMARY KEY, v TEXT, w INTEGER)")).unwrap();
    for i in 0..rows {
        let mut o = serde_json::Map::new();
        o.insert("_collection".into(), json!(coll));
        o.insert("_key".into(), json!(format!("n{i}")));
        if let Some(v) = gen_value(rng) { o.insert("v".into(), v); }
        if let Some(w) = gen_value(rng) { o.insert("w".into(), w); }
        db.put(&format!("{coll}/n{i}"), &Value::Object(o).to_string()).unwrap();
    }
    if indexed {
        db.execute(&format!("CREATE INDEX ON {coll} USING btree (v)")).unwrap();
        db.execute(&format!("CREATE INDEX ON {coll} USING btree (w)")).unwrap();
    }
}

/// Query shapes, `{}` standing in for the collection name.
const SHAPES: &[&str] = &[
    "SELECT _key FROM {} WHERE v = 'apple'",
    "SELECT _key FROM {} WHERE v != 'apple'",
    "SELECT _key FROM {} WHERE v IS NULL",
    "SELECT _key FROM {} WHERE v IS NOT NULL",
    "SELECT _key FROM {} WHERE w > 1",
    "SELECT _key FROM {} WHERE w <= 2",
    "SELECT _key FROM {} WHERE w BETWEEN 0 AND 3",
    "SELECT _key FROM {} WHERE w IN (0,1,2)",
    "SELECT _key FROM {} WHERE w NOT IN (0,1)",
    "SELECT _key FROM {} WHERE NOT (w > 1)",
    "SELECT _key FROM {} WHERE v LIKE 'a%'",
    "SELECT _key FROM {} WHERE v = 'apple' OR w = 0",
    "SELECT _key FROM {} ORDER BY w ASC",
    "SELECT _key FROM {} ORDER BY w DESC",
    "SELECT _key FROM {} ORDER BY w ASC LIMIT 3",
    "SELECT _key FROM {} ORDER BY w DESC LIMIT 3 OFFSET 2",
    "SELECT _key FROM {} ORDER BY v ASC",
    "SELECT COUNT(*) FROM {}",
    "SELECT COUNT(w) FROM {}",
    "SELECT COUNT(v) FROM {}",
    "SELECT SUM(w) FROM {}",
    "SELECT MIN(w), MAX(w) FROM {}",
    "SELECT MIN(v), MAX(v) FROM {}",
    "SELECT AVG(w) FROM {}",
    "SELECT DISTINCT v FROM {}",
    "SELECT v, COUNT(*) FROM {} GROUP BY v",
    "SELECT w, COUNT(*) FROM {} GROUP BY w",
];

/// The answer as text: keys in order for a row query, the payload for an
/// aggregate. Ordering is preserved for `ORDER BY` shapes, because that is
/// exactly where several of the bugs were.
fn answer(db: &CoreDB, sql: &str) -> String {
    match db.query(sql) {
        Ok(set) => {
            let hits = set.collect();
            let mut rows: Vec<String> = hits.iter()
                .map(|h| match &h.payload {
                    Some(p) if h.slug.is_empty() => p.to_string(),
                    _ => h.slug.clone(),
                })
                .collect();
            if !sql.contains("ORDER BY") { rows.sort(); }
            rows.join("|")
        }
        Err(e) => format!("ERR({e:?})"),
    }
}

#[test]
fn an_index_never_changes_an_answer() {
    // `SK_IDXFUZZ_ROUNDS=500 SK_IDXFUZZ_SEED=... cargo test --release
    //  --test index_agreement_fuzz` for a campaign; the committed run stays quick.
    let rounds: u64 = std::env::var("SK_IDXFUZZ_ROUNDS").ok()
        .and_then(|s| s.parse().ok()).unwrap_or(40);
    let seed: u64 = std::env::var("SK_IDXFUZZ_SEED").ok()
        .and_then(|s| s.parse().ok()).unwrap_or(0xA11CE);

    let mut bad: Vec<String> = Vec::new();
    for round in 0..rounds {
        let rows = 1 + (round as usize % 24);

        // The same data on both sides: one RNG stream, replayed from the same
        // seed, so the only difference between the two databases is the index.
        let stream = seed.wrapping_add(round.wrapping_mul(0x9E37_79B9));
        let d1 = tempfile::TempDir::new().unwrap();
        let d2 = tempfile::TempDir::new().unwrap();
        let mut plain = CoreDB::open(d1.path()).unwrap();
        let mut idx = CoreDB::open(d2.path()).unwrap();
        build(&mut plain, "p", &mut Rng(stream), rows, false);
        build(&mut idx,   "p", &mut Rng(stream), rows, true);

        for shape in SHAPES {
            let sql = shape.replace("{}", "p");
            let a = answer(&plain, &sql);
            let b = answer(&idx, &sql);
            if a != b {
                bad.push(format!(
                    "  round {round} (seed {seed}, {rows} rows)\n    {sql}\n      \
                     no index = {a}\n      indexed  = {b}"));
            }
        }
    }
    assert!(bad.is_empty(),
            "an index changed the answer to {} quer{}:\n{}",
            bad.len(), if bad.len() == 1 { "y" } else { "ies" }, bad.join("\n"));
}
