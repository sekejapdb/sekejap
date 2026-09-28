//! The trigram index (0.19 A1): `CREATE INDEX ... USING gin (col
//! gin_trgm_ops)`, PostgreSQL's pg_trgm spelling, answering `LIKE` and
//! `ILIKE` with any pattern from 3-character pieces of the text.
//!
//! The index only NARROWS: every candidate it hands back is checked by the
//! same row matcher `sql_like.rs` pins, so an index may never change an
//! answer. The one way it could is by missing a row the matcher accepts --
//! a required piece the row does not carry. What is at risk, one test each:
//!
//! * the PostgreSQL answers of `sql_like.rs` stay exact with the index
//!   (`the_postgresql_answers_hold_with_the_index`);
//! * over random text and random patterns, indexed and unindexed answers are
//!   the same rows (`random_text_and_patterns_answer_as_the_row_check`);
//! * the characters that fold unevenly -- final sigma, the dotted capital I,
//!   accents -- and the wildcards and escapes beside an anchor keep every
//!   matching row (`uneven_folds_and_anchors_keep_every_row`);
//! * inserts, updates, deletes, a rollback and a reopen keep it exact
//!   (`writes_keep_the_index_exact`);
//! * EXPLAIN names it; a pattern with no 3-character piece and `NOT LIKE`
//!   check rows, with a notice; a plain `'abc%'` keeps its btree range
//!   (`explain_names_the_index_and_short_patterns_check_rows`);
//! * the statements as PostgreSQL writes them, and its refusals with its
//!   SQLSTATEs (`statements_as_postgresql_writes_them`);
//! * a full-text index and a trigram index on one column never stand in for
//!   each other, in either creation order and across a reopen
//!   (`full_text_and_trigram_on_one_column_stay_apart`);
//! * a file without a trigram index carries no new feature bit
//!   (`only_a_trigram_index_sets_its_feature_bit`).
//!
//! From the code review (2026-09-28), one test each:
//!
//! * a trigram filter that does NOT drive -- a second indexed LIKE, a key or
//!   a scalar driving -- is a candidate check, never a BM25 score, and a
//!   query of 33 indexed LIKEs keeps within the 64-filter bound it compiled
//!   under without the index (`a_trigram_filter_that_does_not_drive_is_membership_only`);
//! * a text with no piece (NUL only), marker characters inside text, and a
//!   pattern with hundreds of pieces answer as the row check
//!   (`texts_without_pieces_markers_and_long_patterns_answer_as_the_row_check`);
//! * an index built over rows that already exist (the packed build) answers
//!   as the row check, verifies clean, and does so again after a reopen and
//!   more writes (`a_late_built_index_answers_and_verifies_clean`);
//! * the word-query API refuses a trigram walk on a words index, and a column
//!   named `to_tsvector` takes `gin_trgm_ops`
//!   (`the_api_and_the_parser_keep_the_two_analyzers_apart`).

#[path = "common/like_vectors.rs"]
mod like_vectors;

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use like_vectors::{ROWS, VECTORS};
use sekejap_core::collections::Database;
use sekejap_core::internal::logical_features;
use sekejap_lang::{prepare_sql, SqlDatabase, SqlError, SqlResult, SqlValue};
use tempfile::TempDir;

/// The trigram family's additive feature bit (docs/core/FORMAT_V2.md).
const TRIGRAM_FEATURE: u64 = 0x800000;

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn create(dir: &TempDir) -> Database {
    Database::create(dir.path().join("trgm.sekejap"), cfg()).unwrap()
}

fn run(db: &mut Database, sql: &str) -> SqlResult {
    db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"))
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn keys(db: &mut Database, sql: &str) -> String {
    match run(db, sql) {
        SqlResult::Rows { rows, .. } => {
            let mut out: Vec<String> = rows
                .into_iter()
                .map(|r| match &r.values[0] {
                    SqlValue::Text(k) => k.clone(),
                    other => panic!("{other:?}"),
                })
                .collect();
            out.sort();
            out.join(",")
        }
        other => panic!("`{sql}` answered {other:?}"),
    }
}

fn explain(db: &mut Database, sql: &str) -> String {
    match run(db, &format!("EXPLAIN {sql}")) {
        SqlResult::Explain(text) => text,
        other => panic!("{other:?}"),
    }
}

/// Two tables holding the same rows, `plain` with no index and `indexed`
/// with the trigram index: the row check and the index side by side.
fn twins(db: &mut Database, rows: &[(String, Option<String>)]) {
    run(db, "CREATE TABLE plain (_key TEXT PRIMARY KEY, v TEXT)");
    run(db, "CREATE TABLE indexed (_key TEXT PRIMARY KEY, v TEXT)");
    run(db, "CREATE INDEX indexed_v_trgm ON indexed USING gin (v gin_trgm_ops)");
    for table in ["plain", "indexed"] {
        for chunk in rows.chunks(50) {
            let values: Vec<String> = chunk
                .iter()
                .map(|(k, v)| format!("({}, {})", quote(k), v.as_deref().map_or("NULL".into(), quote)))
                .collect();
            run(db, &format!("INSERT INTO {table} (_key, v) VALUES {}", values.join(", ")));
        }
    }
    run(db, "COMMIT");
}

/// The pattern answered by both twins, which must agree.
fn same_answer(db: &mut Database, op: &str, pattern: &str) {
    let plain = keys(db, &format!("SELECT _key FROM plain WHERE v {op} {pattern}"));
    let indexed = keys(db, &format!("SELECT _key FROM indexed WHERE v {op} {pattern}"));
    assert_eq!(indexed, plain, "`v {op} {pattern}`: the index changed the answer");
}

#[test]
fn the_postgresql_answers_hold_with_the_index() {
    let dir = TempDir::new().unwrap();
    let mut db = create(&dir);
    run(&mut db, "CREATE TABLE t (_key TEXT PRIMARY KEY, v TEXT, n INT)");
    run(&mut db, "CREATE INDEX t_v_trgm ON t USING gin (v gin_trgm_ops)");
    run(&mut db, ROWS);
    run(&mut db, "COMMIT");
    for (op, pattern, expected) in VECTORS {
        let sql = format!("SELECT _key FROM t WHERE v {op} {pattern}");
        assert_eq!(keys(&mut db, &sql), *expected, "`{sql}`");
    }
    let plan = explain(&mut db, "SELECT _key FROM t WHERE v ILIKE '%doe%'");
    assert!(plan.contains("t_v_trgm"), "an infix ILIKE reads the trigram index:\n{plan}");
}

/// A small deterministic generator: the same cases on every run.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a>(&mut self, items: &'a [&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

/// Characters that stress folding and boundaries: ASCII in both cases,
/// accents, the three sigmas, the dotted capital I, a space, punctuation, and
/// the wildcard and escape characters as TEXT.
const TEXT_CHARS: &[&str] = &[
    "a", "b", "c", "A", "B", "C", "é", "É", "Σ", "σ", "ς", "İ", "i", "ı", " ", "-", ".", "%", "_", "\\",
];

#[test]
fn random_text_and_patterns_answer_as_the_row_check() {
    let dir = TempDir::new().unwrap();
    let mut db = create(&dir);
    let mut rng = Rng(0x5eed_1a2b_3c4d_5e6f);
    let mut rows = Vec::new();
    for i in 0..300 {
        let len = rng.below(12);
        let text: String = (0..len).map(|_| rng.pick(TEXT_CHARS)).collect();
        rows.push((format!("r{i:03}"), (i % 29 != 0).then_some(text)));
    }
    twins(&mut db, &rows);
    // Patterns: literal runs cut from the rows, so most of them match
    // something, joined by wildcards and escaped characters.
    const PARTS: &[&str] = &["%", "_", "\\%", "\\_", "\\\\"];
    for _ in 0..600 {
        let mut pattern = String::new();
        for _ in 0..1 + rng.below(4) {
            if rng.below(3) == 0 {
                pattern.push_str(rng.pick(PARTS));
            } else {
                let source = rows[rng.below(rows.len())].1.clone().unwrap_or_default();
                let chars: Vec<char> = source.chars().collect();
                if !chars.is_empty() {
                    let from = rng.below(chars.len());
                    let to = (from + 1 + rng.below(5)).min(chars.len());
                    for c in &chars[from..to] {
                        // A literal wildcard or escape character is escaped.
                        if matches!(c, '%' | '_' | '\\') {
                            pattern.push('\\');
                        }
                        pattern.push(*c);
                    }
                }
            }
        }
        for op in ["LIKE", "ILIKE"] {
            same_answer(&mut db, op, &quote(&pattern));
        }
    }
}

#[test]
fn uneven_folds_and_anchors_keep_every_row() {
    let dir = TempDir::new().unwrap();
    let mut db = create(&dir);
    let rows: Vec<(String, Option<String>)> = [
        "ΑΟΣ", "ΟΔΟΣ ΠΑΛΙΑ", "σοφός", "İzmir", "izmir", "IZMIR", "xabc", "abcx", "abc", "aXbc", "abXc",
        "Café Ubud", "CAFÉ", "100% cotton", "under_score", "a\\b", "ab", "é",
    ]
    .iter()
    .enumerate()
    .map(|(i, v)| (format!("u{i:02}"), Some(v.to_string())))
    .collect();
    twins(&mut db, &rows);
    for pattern in [
        // Final sigma: the matcher folds the text whole-string, the pattern
        // by character.
        "'%αος%'", "'%ΑΟΣ%'", "'%οδος%'", "'%σοφος%'", "'%σοφός%'", "'αος'",
        // The dotted capital I lower-cases to two characters.
        "'%İzm%'", "'%izm%'", "'i̇zmir'", "'_zmir'", "'__zmir'", "'%zmir'",
        // Anchors beside `_`: none of these may require a start or end piece.
        "'_abc'", "'abc_'", "'a_bc%'", "'%ab_'", "'_bc%'", "'abc'", "'abc%'", "'%abc'",
        // Accents, and escapes resolved before the pieces are cut.
        "'%afé%'", "'%AFÉ%'", "'%\\%%'", "'%0\\% c%'", "'%r!_s%' ESCAPE '!'", "'%er_sc%'",
        "'%a\\\\b%'", "'%a\\b%'", "'%é%' ESCAPE 'é'", "'%abc%' ESCAPE ''", "'%ab%'",
    ] {
        for op in ["LIKE", "ILIKE"] {
            same_answer(&mut db, op, pattern);
        }
    }
}

#[test]
fn writes_keep_the_index_exact() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("trgm.sekejap");
    {
        let mut db = Database::create(&path, cfg()).unwrap();
        let rows: Vec<(String, Option<String>)> = (0..60)
            .map(|i| (format!("w{i:02}"), Some(format!("sunset beach {i} Seminyak"))))
            .collect();
        twins(&mut db, &rows);
        for table in ["plain", "indexed"] {
            for sql in [
                format!("UPDATE {table} SET v = 'quiet rice terrace' WHERE _key = 'w03'"),
                format!("UPDATE {table} SET v = NULL WHERE _key = 'w04'"),
                format!("UPDATE {table} SET v = 'sunset beach again' WHERE _key = 'w04'"),
                format!("DELETE FROM {table} WHERE _key = 'w05'"),
                format!("INSERT INTO {table} (_key, v) VALUES ('w99', 'Monkey Forest path')"),
            ] {
                run(&mut db, &sql);
            }
        }
        run(&mut db, "COMMIT");
        // A rolled-back write leaves nothing behind in either twin.
        run(&mut db, "BEGIN");
        run(&mut db, "UPDATE indexed SET v = 'volcano' WHERE _key = 'w06'");
        run(&mut db, "ROLLBACK");
        for pattern in ["'%sunset%'", "'%terrace%'", "'%forest%'", "'%volcano%'", "'%again%'", "'%Seminyak'"] {
            for op in ["LIKE", "ILIKE"] {
                same_answer(&mut db, op, pattern);
            }
        }
    }
    let mut db = Database::open(&path, cfg()).unwrap();
    for pattern in ["'%sunset%'", "'%terrace%'", "'%forest%'", "'%volcano%'", "'%beach 1%'"] {
        for op in ["LIKE", "ILIKE"] {
            same_answer(&mut db, op, pattern);
        }
    }
    assert_eq!(keys(&mut db, "SELECT _key FROM indexed WHERE v ILIKE '%monkey%'"), "w99");
}

#[test]
fn explain_names_the_index_and_short_patterns_check_rows() {
    let dir = TempDir::new().unwrap();
    let mut db = create(&dir);
    run(&mut db, "CREATE TABLE place (_key TEXT PRIMARY KEY, name TEXT)");
    run(&mut db, "CREATE INDEX place_name_trgm ON place USING gin (name gin_trgm_ops)");
    run(&mut db, "INSERT INTO place (_key, name) VALUES ('ny', 'New York-City'), ('ub', 'Ubud'), ('yo', 'Yogya')");
    run(&mut db, "COMMIT");
    let plan = explain(&mut db, "SELECT _key FROM place WHERE name LIKE '%York-Ci%'");
    assert!(plan.contains("place_name_trgm"), "the index answers a pattern with pieces:\n{plan}");
    assert_eq!(keys(&mut db, "SELECT _key FROM place WHERE name LIKE '%York-Ci%'"), "ny");
    // Two characters make no 3-character piece: the rows are checked, and
    // the notice says so.
    let plan = explain(&mut db, "SELECT _key FROM place WHERE name ILIKE '%yo%'");
    assert!(!plan.contains("place_name_trgm"), "no piece, no index:\n{plan}");
    assert!(plan.contains("checked on each row"), "a notice names the row check:\n{plan}");
    assert_eq!(keys(&mut db, "SELECT _key FROM place WHERE name ILIKE '%yo%'"), "ny,yo");
    // An anchored start has pieces of its own.
    let plan = explain(&mut db, "SELECT _key FROM place WHERE name ILIKE 'ne%'");
    assert!(plan.contains("place_name_trgm"), "an anchored start reads the index:\n{plan}");
    // NOT LIKE never reads it.
    let plan = explain(&mut db, "SELECT _key FROM place WHERE name NOT LIKE '%York%'");
    assert!(!plan.contains("place_name_trgm"), "NOT LIKE checks rows:\n{plan}");
    // A plain `'abc%'` keeps the btree range when the column has one: exact,
    // and nothing to recheck.
    run(&mut db, "CREATE INDEX place_name_btree ON place (name)");
    let plan = explain(&mut db, "SELECT _key FROM place WHERE name LIKE 'Ub%'");
    assert!(plan.contains("place_name_btree"), "the prefix range keeps priority:\n{plan}");
}

fn sqlstate(db: &mut Database, sql: &str) -> String {
    let result = db.sql(sql, &[]);
    let _ = db.sql("ROLLBACK", &[]);
    match result {
        Err(SqlError::Coded { sqlstate, message }) => format!("{sqlstate} {message}"),
        other => panic!("`{sql}` should be refused with a SQLSTATE, not {other:?}"),
    }
}

fn notices(db: &mut Database, sql: &str) -> String {
    match run(db, sql) {
        SqlResult::Notice(text) => text,
        other => panic!("`{sql}` should answer a notice, not {other:?}"),
    }
}

#[test]
fn statements_as_postgresql_writes_them() {
    let dir = TempDir::new().unwrap();
    let mut db = create(&dir);
    // A migration script's first line: accepted, and it does nothing.
    assert!(notices(&mut db, "CREATE EXTENSION pg_trgm").contains("built in"));
    assert!(notices(&mut db, "CREATE EXTENSION IF NOT EXISTS pg_trgm").contains("built in"));
    run(&mut db, "CREATE TABLE place (_key TEXT PRIMARY KEY, name TEXT, visits INT)");
    // GiST's operator class builds the same index, and says so.
    let gist = "CREATE INDEX place_name_gist ON place USING gist (name gist_trgm_ops)";
    let said = prepare_sql(&db, gist, &[]).unwrap().notices().join("\n");
    assert!(said.contains("gin_trgm_ops"), "{said}");
    run(&mut db, gist);
    run(&mut db, "INSERT INTO place (_key, name, visits) VALUES ('ub', 'Ubud', 3)");
    run(&mut db, "COMMIT");
    let plan = explain(&mut db, "SELECT _key FROM place WHERE name LIKE '%bud%'");
    assert!(plan.contains("place_name_gist"), "{plan}");
    // PostgreSQL 14's refusals, with its SQLSTATEs.
    let refused = sqlstate(&mut db, "CREATE INDEX ON place USING gin (visits gin_trgm_ops)");
    assert!(refused.starts_with("42804"), "{refused}");
    // sekejap's INT is PostgreSQL's bigint, so PostgreSQL's sentence names it.
    assert!(refused.contains(r#"operator class "gin_trgm_ops" does not accept data type bigint"#), "{refused}");
    let refused = sqlstate(&mut db, "CREATE UNIQUE INDEX ON place USING gin (name gin_trgm_ops)");
    assert!(refused.starts_with("0A000"), "{refused}");
    assert!(refused.contains(r#"access method "gin" does not support unique indexes"#), "{refused}");
    // What A1 leaves out is refused by name, not answered wrongly.
    for sql in [
        "SELECT similarity(name, 'ubd') FROM place",
        "SELECT _key FROM place WHERE name % 'ubd'",
    ] {
        assert!(db.sql(sql, &[]).is_err(), "`{sql}` is not built and must be refused");
        let _ = db.sql("ROLLBACK", &[]);
    }
}

#[test]
fn full_text_and_trigram_on_one_column_stay_apart() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("trgm.sekejap");
    {
        let mut db = Database::create(&path, cfg()).unwrap();
        run(&mut db, "CREATE TABLE a (_key TEXT PRIMARY KEY, body TEXT)");
        run(&mut db, "CREATE TABLE b (_key TEXT PRIMARY KEY, body TEXT)");
        // Full text first on `a`, trigram first on `b`.
        run(&mut db, "CREATE INDEX a_fts ON a USING gin (to_tsvector('simple', body))");
        run(&mut db, "CREATE INDEX a_trgm ON a USING gin (body gin_trgm_ops)");
        run(&mut db, "CREATE INDEX b_trgm ON b USING gin (body gin_trgm_ops)");
        run(&mut db, "CREATE INDEX b_fts ON b USING gin (to_tsvector('simple', body))");
        for table in ["a", "b"] {
            run(&mut db, &format!("INSERT INTO {table} (_key, body) VALUES ('x', 'sunset over the reef'), ('y', 'rice terrace walk')"));
        }
        run(&mut db, "COMMIT");
    }
    let mut db = Database::open(&path, cfg()).unwrap();
    for table in ["a", "b"] {
        let words = format!("SELECT _key FROM {table} WHERE to_tsvector('simple', body) @@ to_tsquery('simple', 'reef')");
        let plan = explain(&mut db, &words);
        assert!(plan.contains(&format!("{table}_fts")) && !plan.contains(&format!("{table}_trgm")), "{plan}");
        assert_eq!(keys(&mut db, &words), "x");
        let piece = format!("SELECT _key FROM {table} WHERE body LIKE '%erra%'");
        let plan = explain(&mut db, &piece);
        assert!(plan.contains(&format!("{table}_trgm")) && !plan.contains(&format!("{table}_fts")), "{plan}");
        assert_eq!(keys(&mut db, &piece), "y");
    }
    // Dropping one leaves the other answering.
    run(&mut db, "DROP INDEX a_trgm");
    run(&mut db, "COMMIT");
    assert_eq!(keys(&mut db, "SELECT _key FROM a WHERE to_tsvector('simple', body) @@ to_tsquery('simple', 'rice')"), "y");
    assert_eq!(keys(&mut db, "SELECT _key FROM a WHERE body LIKE '%erra%'"), "y");
}

#[test]
fn only_a_trigram_index_sets_its_feature_bit() {
    let dir = TempDir::new().unwrap();
    let mut db = create(&dir);
    run(&mut db, "CREATE TABLE t (_key TEXT PRIMARY KEY, v TEXT)");
    run(&mut db, "CREATE INDEX t_fts ON t USING gin (to_tsvector('simple', v))");
    run(&mut db, "INSERT INTO t (_key, v) VALUES ('a', 'Ubud')");
    run(&mut db, "COMMIT");
    assert_eq!(logical_features(&db) & TRIGRAM_FEATURE, 0, "no trigram index, no bit");
    run(&mut db, "CREATE INDEX t_trgm ON t USING gin (v gin_trgm_ops)");
    run(&mut db, "COMMIT");
    assert_ne!(logical_features(&db) & TRIGRAM_FEATURE, 0, "the first trigram index sets the bit");
}

#[test]
fn a_trigram_filter_that_does_not_drive_is_membership_only() {
    let dir = TempDir::new().unwrap();
    let mut db = create(&dir);
    let rows: Vec<(String, Option<String>)> = ["abcdef", "abcxyz", "xyzdef", "abc", "def", "abcabcdef"]
        .iter()
        .enumerate()
        .map(|(i, v)| (format!("m{i}"), Some(v.to_string())))
        .collect();
    twins(&mut db, &rows);
    for where_clause in [
        "v LIKE '%abc%' AND v LIKE '%def%'",
        "_key = 'm0' AND v LIKE '%def%'",
        "_key >= 'm2' AND v ILIKE '%ABC%'",
        "v LIKE '%abc%' AND v NOT LIKE '%xyz%'",
    ] {
        let plain = keys(&mut db, &format!("SELECT _key FROM plain WHERE {where_clause}"));
        let indexed = keys(&mut db, &format!("SELECT _key FROM indexed WHERE {where_clause}"));
        assert_eq!(indexed, plain, "`{where_clause}`");
    }
    // 33 LIKEs are 33 filters without the index; with it, the optional
    // narrowing must not push the statement past the 64-filter bound.
    let many = vec!["v ILIKE '%abc%'"; 33].join(" AND ");
    let plain = keys(&mut db, &format!("SELECT _key FROM plain WHERE {many}"));
    let indexed = keys(&mut db, &format!("SELECT _key FROM indexed WHERE {many}"));
    assert_eq!(indexed, plain);
}

#[test]
fn texts_without_pieces_markers_and_long_patterns_answer_as_the_row_check() {
    let dir = TempDir::new().unwrap();
    let mut db = create(&dir);
    twins(&mut db, &[]);
    let long: String = (0..600).map(|i| char::from(b'a' + (i * 7 % 26) as u8)).collect();
    // First a corpus whose only text has no piece at all: a present
    // document with zero pieces, which is not damage.
    for table in ["plain", "indexed"] {
        let c = db.collection(table).unwrap().unwrap();
        db.put(c, "n0", &serde_json::json!({ "v": "\0" })).unwrap();
    }
    run(&mut db, "COMMIT");
    for op in ["LIKE", "ILIKE"] {
        same_answer(&mut db, op, "'%abc%'");
    }
    for table in ["plain", "indexed"] {
        let c = db.collection(table).unwrap().unwrap();
        for (key, text) in [
            ("n1", "\u{2}\u{2}x\u{3}".to_owned()),
            ("n2", format!("pre {long} post")),
            ("n3", "plain text".to_owned()),
        ] {
            db.put(c, key, &serde_json::json!({ "v": text })).unwrap();
        }
    }
    run(&mut db, "COMMIT");
    for pattern in [
        "'%abc%'".to_owned(),
        "'%x%'".to_owned(),
        "'%text%'".to_owned(),
        format!("'%{long}%'"),
        format!("'pre {}%'", &long[..300]),
    ] {
        for op in ["LIKE", "ILIKE"] {
            same_answer(&mut db, op, &pattern);
        }
    }
}

#[test]
fn the_api_and_the_parser_keep_the_two_analyzers_apart() {
    let dir = TempDir::new().unwrap();
    let mut db = create(&dir);
    run(&mut db, "CREATE TABLE t (_key TEXT PRIMARY KEY, to_tsvector TEXT, body TEXT)");
    run(&mut db, "CREATE INDEX t_body_fts ON t USING gin (to_tsvector('simple', body))");
    run(&mut db, "CREATE INDEX t_odd_trgm ON t USING gin (to_tsvector gin_trgm_ops)");
    run(&mut db, "INSERT INTO t (_key, to_tsvector, body) VALUES ('a', 'xabc', 'xabc')");
    run(&mut db, "COMMIT");
    assert_eq!(keys(&mut db, r#"SELECT _key FROM t WHERE "to_tsvector" LIKE '%abc%'"#), "a");
    let c = db.collection("t").unwrap().unwrap();
    let words = db
        .list_indexes(c)
        .unwrap()
        .into_iter()
        .find(|i| i.name == "t_body_fts")
        .unwrap()
        .id;
    let trigram_walk = sekejap_core::collections::TextMatch::Trigram { escape: Some('\\'), insensitive: false };
    let asked = db.query_text(
        words,
        "%abc%",
        trigram_walk,
        10,
        sekejap_core::collections::TextCandidates::All,
        1_000,
        || false,
    );
    assert!(asked.is_err(), "a words index answered a trigram walk: {asked:?}");
}

fn verified_clean(path: &std::path::Path) {
    use sekejap_core::collections::verification::{verify_indexed_source, VerificationLimits};
    let report = verify_indexed_source(path, VerificationLimits::default(), |issue| {
        panic!("unexpected verifier issue: {issue:?}")
    })
    .unwrap();
    assert!(report.complete && report.clean, "{report:?}");
}

#[test]
fn a_late_built_index_answers_and_verifies_clean() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("late.sekejap");
    {
        let mut db = Database::create(&path, cfg()).unwrap();
        run(&mut db, "CREATE TABLE plain (_key TEXT PRIMARY KEY, v TEXT)");
        run(&mut db, "CREATE TABLE indexed (_key TEXT PRIMARY KEY, v TEXT)");
        // More rows than one trigram scan run, so the build crosses runs.
        for table in ["plain", "indexed"] {
            for chunk in 0..6 {
                let values: Vec<String> = (0..100)
                    .map(|i| {
                        let n = chunk * 100 + i;
                        format!("('l{n:03}', 'visit {n} to Seminyak Beach and Ubud {}')", n % 7)
                    })
                    .collect();
                run(&mut db, &format!("INSERT INTO {table} (_key, v) VALUES {}", values.join(", ")));
            }
        }
        run(&mut db, "COMMIT");
        run(&mut db, "CREATE INDEX indexed_v_trgm ON indexed USING gin (v gin_trgm_ops)");
        run(&mut db, "COMMIT");
        for pattern in ["'%ubud 3%'", "'%visit 12 %'", "'visit 5%'", "'%beach%'"] {
            for op in ["LIKE", "ILIKE"] {
                same_answer(&mut db, op, pattern);
            }
        }
    }
    verified_clean(&path);
    {
        let mut db = Database::open(&path, cfg()).unwrap();
        for table in ["plain", "indexed"] {
            run(&mut db, &format!("UPDATE {table} SET v = 'rice terrace walk' WHERE _key = 'l007'"));
            run(&mut db, &format!("DELETE FROM {table} WHERE _key = 'l008'"));
        }
        run(&mut db, "COMMIT");
        for pattern in ["'%terrace%'", "'%visit 8 %'", "'%ubud 1%'"] {
            for op in ["LIKE", "ILIKE"] {
                same_answer(&mut db, op, pattern);
            }
        }
    }
    verified_clean(&path);
}
