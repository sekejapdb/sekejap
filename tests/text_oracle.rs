//! Text search against the word lists the documents were built from.
//!
//! An oracle: the fixture knows which documents contain which words, because it
//! chose them. Nothing asks the engine a second way.
//!
//! sekejap offers three text tools over the same field and they answer different
//! questions, so each is checked against what it promises:
//!
//! * `ILIKE '%word%'` — a pattern match on the raw text
//! * `SEARCH('word')` — the positional search index
//! * `BM25(field, 'word') > 0` — relevance scoring, where a positive score means
//!   the term was found
//!
//! The vocabulary is chosen so no word is a substring of another and none shares
//! a stem, which keeps the expected set unambiguous — the point is to test the
//! search paths, not to have an opinion about tokenisation.

use sekejap::{Config, CoreDB};
use serde_json::json;
use std::collections::HashSet;

const VOCAB: &[&str] = &[
    "heron", "riverbank", "lantern", "quarry", "meadow", "thistle", "harbour", "kestrel",
];
const DOCS: usize = 60;

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

/// Build the documents and return, for each word, the set of keys containing it.
fn fixture(dir: &std::path::Path, cfg: Config, seed: u64)
    -> Vec<(&'static str, HashSet<String>)>
{
    let mut rng = Rng(seed);
    let mut db = CoreDB::open_with_config(dir, cfg).unwrap();
    db.execute("CREATE TABLE d (_key TEXT PRIMARY KEY, body TEXT)").unwrap();
    let mut contains: Vec<(&'static str, HashSet<String>)> =
        VOCAB.iter().map(|w| (*w, HashSet::new())).collect();

    for i in 0..DOCS {
        // Two to four distinct words per document.
        let want = 2 + rng.below(3) as usize;
        let mut words: Vec<&'static str> = Vec::new();
        while words.len() < want {
            let w = VOCAB[rng.below(VOCAB.len() as u64) as usize];
            if !words.contains(&w) { words.push(w) }
        }
        let key = format!("d{i}");
        db.put(&format!("d/{key}"), &json!({
            "_collection": "d", "_key": key,
            "body": format!("the {} beside the {}", words.join(" and the "), "water"),
        }).to_string()).unwrap();
        for w in &words {
            contains.iter_mut().find(|(v, _)| v == w).unwrap().1.insert(format!("d/{key}"));
        }
    }
    // Declared, not built by hand: `CREATE INDEX … USING search` is how a search
    // index comes into existence and is what survives a reopen. `build_text_indexes`
    // does not declare one, which is how the first run of this test managed to ask
    // `SEARCH('heron')` of a database that had no search index at all.
    db.execute("CREATE INDEX ON d USING gin (body)").unwrap();
    db.execute("CREATE INDEX ON d USING bm25 (body)").unwrap();
    db.execute("CREATE INDEX ON d USING search (body)").unwrap();
    db.compact().unwrap();
    contains
}

fn keys(db: &CoreDB, sql: &str) -> HashSet<String> {
    db.query(sql)
        .unwrap_or_else(|e| panic!("`{sql}` did not run: {e:?}"))
        .collect()
        .iter()
        .map(|h| h.slug.clone())
        .collect()
}

#[test]
fn every_text_tool_finds_the_documents_that_contain_the_word() {
    for (label, cfg) in [("default", Config::default()), ("resident", Config::resident())] {
        let dir = tempfile::TempDir::new().unwrap();
        let contains = fixture(dir.path(), cfg.clone(), 0x7E47);
        let db = CoreDB::open_with_config(dir.path(), cfg).unwrap();

        let mut exercised = 0;
        for (word, want) in &contains {
            // Every word must actually appear somewhere, or the comparison below
            // is between two empty sets and says nothing.
            assert!(!want.is_empty(), "[{label}] '{word}' is in no document");
            assert!(want.len() < DOCS, "[{label}] '{word}' is in every document");

            let got = keys(&db, &format!("SELECT _key FROM d WHERE body ILIKE '%{word}%'"));
            assert_eq!(&got, want,
                "[{label}] ILIKE '%{word}%' does not match the documents containing it\n  \
                 engine = {} rows\n  oracle = {} rows", got.len(), want.len());

            let got = keys(&db, &format!("SELECT _key FROM d WHERE SEARCH('{word}')"));
            assert_eq!(&got, want,
                "[{label}] SEARCH('{word}') does not match the documents containing it\n  \
                 engine = {} rows\n  oracle = {} rows", got.len(), want.len());

            let got = keys(&db, &format!("SELECT _key FROM d WHERE BM25(body, '{word}') > 0"));
            assert_eq!(&got, want,
                "[{label}] BM25(body, '{word}') > 0 does not match the documents \
                 containing it\n  engine = {} rows\n  oracle = {} rows",
                got.len(), want.len());

            exercised += 1;
        }
        assert_eq!(exercised, VOCAB.len(), "[{label}] not every word was checked");
    }
}

/// A word that is in no document must return nothing from all three — the other
/// direction, and the one a lenient matcher gets wrong.
#[test]
fn a_word_that_is_absent_matches_nothing() {
    let dir = tempfile::TempDir::new().unwrap();
    let _ = fixture(dir.path(), Config::default(), 0xAB5E17);
    let db = CoreDB::open(dir.path()).unwrap();

    for absent in ["zeppelin", "marmalade", "cartography"] {
        for sql in [
            format!("SELECT _key FROM d WHERE body ILIKE '%{absent}%'"),
            format!("SELECT _key FROM d WHERE SEARCH('{absent}')"),
            format!("SELECT _key FROM d WHERE BM25(body, '{absent}') > 0"),
        ] {
            let got = keys(&db, &sql);
            assert!(got.is_empty(),
                "`{sql}` returned {} rows for a word that is in no document: {got:?}",
                got.len());
        }
    }
}

/// **A text tool with no index must say so, not answer "nothing matches".**
///
/// `SEARCH` and `BM25` are served entirely from their indexes. With no index
/// declared they used to return an empty result — a claim about the documents
/// made by code that had not read one, and indistinguishable from the truthful
/// empty answer for a word that genuinely does not occur.
///
/// This test exists because that behaviour cost real time: the first run of the
/// oracle above declared no search index, and the resulting silence read as a
/// bug in the search index rather than a missing `CREATE INDEX`.
///
/// `ILIKE` is the control. It answers correctly by scanning, so an index only
/// makes it faster — and it must keep working here.
#[test]
fn a_text_tool_without_its_index_says_so() {
    use sekejap::SqlError;

    for (label, cfg) in [("default", Config::default()), ("resident", Config::resident())] {
        let dir = tempfile::TempDir::new().unwrap();
        let mut db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
        db.execute("CREATE TABLE d (_key TEXT PRIMARY KEY, body TEXT)").unwrap();
        db.execute("CREATE TABLE t (_key TEXT PRIMARY KEY)").unwrap();
        for i in 0..5 {
            db.put(&format!("d/d{i}"), &json!({
                "_collection": "d", "_key": format!("d{i}"),
                "body": "the heron beside the riverbank",
            }).to_string()).unwrap();
        }
        db.put("t/1", &json!({"_collection": "t", "_key": "1"}).to_string()).unwrap();
        db.link("t/1", "d/d0", "sees");
        db.compact().unwrap();

        // The word is in every document, so an empty answer would be wrong.
        assert_eq!(
            db.query("SELECT _key FROM d WHERE body ILIKE '%heron%'").unwrap().collect().len(),
            5,
            "[{label}] ILIKE must answer without an index — it scans",
        );

        for sql in [
            "SELECT _key FROM d WHERE SEARCH('heron')",
            "SELECT _key FROM d WHERE BM25(body, 'heron') > 0",
            // Inside a MATCH the plan has a different shape and a separate check.
            "SELECT b._key FROM MATCH (a:t)-[:sees]->(b:d) WHERE BM25(b.body, 'heron') > 0",
            // Scoring rather than filtering: with no index every score used to be
            // 0, which reorders the results silently instead of emptying them.
            "SELECT _key, BM25(body, 'heron') AS s FROM d ORDER BY s DESC",
            // `SEARCH_SCORE` is the same shape with a different index behind it.
            "SELECT _key, SEARCH_SCORE('heron') AS s FROM d ORDER BY s DESC",
        ] {
            match db.query(sql) {
                Err(SqlError::IndexNotBuilt { declared, .. }) => {
                    assert!(!declared, "[{label}] `{sql}`: no index was ever declared");
                }
                Err(other) => panic!("[{label}] `{sql}` gave the wrong error: {other:?}"),
                Ok(set) => panic!(
                    "[{label}] `{sql}` answered with {} row(s) rather than reporting \
                     that it has no index to read",
                    set.collect().len()
                ),
            }
        }
    }
}
