//! Four storage configurations, one answer — searched, not sampled.
//!
//! A payload can be stored as raw JSON or as SKBIN (schema-aware binary: field
//! names interned to ids, values typed), in either the resident or the paged
//! layout. That is four ways to hold the same row, and every query must answer
//! the same through all of them.
//!
//! `differential_audit` compares storage modes with a fixed dataset and a fixed
//! probe list, and `skbin_surface` tests the encoder against hand-written cases.
//! Neither searches. This does: seeded random rows drawn from the awkward end —
//! absent fields, explicit nulls, empty strings, unicode, whole floats against
//! integers, numbers written as text, deep-ish nesting — answered through all
//! four configurations and compared.
//!
//! Like its siblings it is not a correctness oracle. It knows the four must
//! agree, which is the property the encodings exist to preserve.

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

/// Values chosen to stress the encoder rather than to look realistic.
fn gen_value(rng: &mut Rng) -> Option<Value> {
    match rng.below(16) {
        0 => None,                       // field absent
        1 => Some(Value::Null),
        2 => Some(json!(0)),
        3 => Some(json!(-7)),
        4 => Some(json!(i64::MAX)),
        5 => Some(json!(1.0)),           // whole, written as a float
        6 => Some(json!(-2.5)),
        7 => Some(json!("")),            // empty string
        8 => Some(json!("3")),           // a number as text
        9 => Some(json!("kucing")),
        10 => Some(json!("héllo wörld")),      // non-ASCII
        11 => Some(json!("with \"quotes\" and \\ backslash")),
        12 => Some(json!(true)),
        13 => Some(json!(false)),
        14 => Some(json!([1, 2, 3])),
        _ => Some(json!({"nested": {"deep": 1}})),
    }
}

fn build(db: &mut CoreDB, rng: &mut Rng, rows: usize) {
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, v TEXT, w INTEGER, body TEXT)").unwrap();
    for i in 0..rows {
        let mut o = serde_json::Map::new();
        o.insert("_collection".into(), json!("p"));
        o.insert("_key".into(), json!(format!("n{i}")));
        if let Some(v) = gen_value(rng) { o.insert("v".into(), v); }
        if let Some(w) = gen_value(rng) { o.insert("w".into(), w); }
        if let Some(b) = gen_value(rng) { o.insert("body".into(), b); }
        // A field only some rows have at all, so the interned table is uneven.
        if rng.below(3) == 0 { o.insert(format!("extra{}", rng.below(3)), json!("e")); }
        db.put(&format!("p/n{i}"), &Value::Object(o).to_string()).unwrap();
    }
    db.execute("CREATE INDEX ON p USING btree (w)").unwrap();
    for i in 0..rows.saturating_sub(1) {
        if rng.below(3) != 0 { db.link(&format!("p/n{i}"), &format!("p/n{}", i + 1), "next"); }
    }
    db.compact().unwrap();          // SKBIN encoding happens here
}

const PROBES: &[(&str, bool)] = &[
    ("SELECT _key FROM p", false),
    ("SELECT * FROM p", true),
    ("SELECT v, w FROM p", true),
    ("SELECT _key FROM p WHERE w = 0", false),
    ("SELECT _key FROM p WHERE w > 0", false),
    ("SELECT _key FROM p WHERE v IS NULL", false),
    ("SELECT _key FROM p WHERE v = ''", false),
    ("SELECT _key FROM p WHERE v = 'kucing'", false),
    ("SELECT _key FROM p WHERE v != 'kucing'", false),
    ("SELECT _key FROM p WHERE v LIKE 'k%'", false),
    ("SELECT _key FROM p ORDER BY w ASC", false),
    ("SELECT COUNT(*), COUNT(v), SUM(w), MIN(w), MAX(w) FROM p", true),
    ("SELECT v, COUNT(*) FROM p GROUP BY v", true),
    ("SELECT DISTINCT v FROM p", true),
    ("SELECT _key FROM MATCH (a:p)-[:next]->(b:p)", false),
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
    // The stored bytes themselves, read back one row at a time.
    let mut slugs = db.all_slugs();
    slugs.sort();
    for s in &slugs {
        out.push(format!("get({s}) => {}", db.get(s).unwrap_or_default()));
    }
    out
}

fn configs() -> Vec<(&'static str, Config)> {
    vec![
        ("resident+json",  Config { payload_binary: false, ..Config::resident() }),
        ("resident+skbin", Config { payload_binary: true,  ..Config::resident() }),
        ("paged+json",     Config { payload_binary: false, ..Config::default() }),
        ("paged+skbin",    Config { payload_binary: true,  ..Config::default() }),
    ]
}

#[test]
fn every_encoding_and_layout_gives_the_same_answer() {
    let rounds: u64 = std::env::var("SK_ENCFUZZ_ROUNDS").ok()
        .and_then(|s| s.parse().ok()).unwrap_or(24);
    let seed: u64 = std::env::var("SK_ENCFUZZ_SEED").ok()
        .and_then(|s| s.parse().ok()).unwrap_or(0x5E1AB);

    let mut bad: Vec<String> = Vec::new();
    for round in 0..rounds {
        let rows = 1 + (round as usize % 20);
        let stream = seed.wrapping_add(round.wrapping_mul(0x9E37_79B9));

        // Two things in a stored row are not answers and must come out before
        // comparing: the key order of the JSON text, which is an artefact of the
        // encoding, and `_created_unix` / `_updated_unix`, which are wall-clock
        // stamps and differ because the four databases are built moments apart.
        // Leaving the stamps in reported 732 differences on the first run, none
        // of them a bug.
        fn strip_stamps(v: &mut Value) {
            match v {
                Value::Object(m) => {
                    m.remove("_created_unix");
                    m.remove("_updated_unix");
                    for (_, inner) in m.iter_mut() { strip_stamps(inner); }
                }
                Value::Array(a) => for inner in a { strip_stamps(inner) },
                _ => {}
            }
        }
        let normalise = |v: &[String]| -> Vec<String> {
            v.iter().map(|line| match line.split_once(" => ") {
                Some((k, rest)) => {
                    // Each side of a `|`-joined row list is its own JSON value.
                    let parts: Vec<String> = rest.split('|').map(|p| {
                        match serde_json::from_str::<Value>(p) {
                            Ok(mut val) => {
                                strip_stamps(&mut val);
                                serde_json::to_string(&val).unwrap_or_default()
                            }
                            Err(_) => p.to_string(),
                        }
                    }).collect();
                    format!("{k} => {}", parts.join("|"))
                }
                None => line.clone(),
            }).collect()
        };

        let mut reference: Option<(&str, Vec<String>)> = None;
        for (label, cfg) in configs() {
            let dir = tempfile::TempDir::new().unwrap();
            let mut db = CoreDB::open_with_config(dir.path(), cfg.clone()).unwrap();
            build(&mut db, &mut Rng(stream), rows);
            let got = normalise(&answers(&db));
            match &reference {
                None => reference = Some((label, got)),
                Some((rl, want)) => {
                    for (w, g) in want.iter().zip(&got) {
                        if w != g {
                            bad.push(format!(
                                "  round {round} (seed {seed}, {rows} rows)\n    \
                                 {rl:<15} {w}\n    {label:<15} {g}"));
                        }
                    }
                }
            }
        }
    }
    assert!(bad.is_empty(),
            "storage configurations disagree on {} answer(s):\n{}",
            bad.len(), bad.join("\n"));
}
