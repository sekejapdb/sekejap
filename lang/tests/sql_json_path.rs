//! `col->>'member'` as an index range: the SQL half of making a `JSONB`
//! column filterable.
//!
//! `docs/lang/INDEX_CONTRACT.md` recorded `JSONB` as a declared type with no
//! index family, no path operator and therefore no predicate that could ever
//! be answered index-side. What exists now is ONE operator in TWO positions
//! and nothing else:
//!
//! 1. `CREATE INDEX i ON t ((payload->>'status'))` -- an EXPRESSION scalar
//!    index (`IndexExpr::JsonText`) that stores the TEXT at one member of the
//!    document, in the scalar keyspace, under descriptor version 4.
//! 2. `WHERE payload->>'status' = 'live'` -- answered from exactly that
//!    index. Without a matching one the predicate is REFUSED naming the index
//!    it would need, never demoted to a scan (QL_CONTRACT §6).
//!
//! `->`, `#>`, `#>>` and `json_array_length` stay refused by name, and so
//! does `->>` in every other position: a SELECT list, an ORDER BY, an
//! ordering comparison.
//!
//! The oracle is a brute-force extraction over the same corpus, held in this
//! process and computed from the EXTRACTION RULE rather than from a second
//! reading of the engine's own answer. The corpus is randomised over the
//! shapes the rule has to be total across: strings, numbers, booleans, JSON
//! null, an absent member, an empty object, a nested object, an array, and a
//! row with no `payload` column at all.
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::{
    verification::{verify_indexed_source, VerificationLimits},
    Database,
};
use sekejap_lang::{SqlDatabase, SqlError, SqlResult, SqlRow, SqlValue, Tier};
use serde_json::{json, Value};
use tempfile::TempDir;

const ROWS: usize = 1_000;
const SEED: u64 = 0x4A53_4F4E_5041_5448;
/// The member values the corpus draws from. `live` and `draft` are common,
/// so an equality has a large answer and a large complement.
const STATUSES: [&str; 5] = ["live", "draft", "archived", "live", "hidden"];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn usize(&mut self, n: usize) -> usize {
        (self.next() as usize) % n
    }
}

#[derive(Clone, Debug)]
struct Row {
    key: String,
    n: i64,
    /// The document written into `payload`, or `None` when the row omits the
    /// column entirely.
    payload: Option<Value>,
}

/// One document per shape the extraction rule has to be total across.
fn generate(i: usize, rng: &mut Rng) -> Row {
    let status = STATUSES[rng.usize(STATUSES.len())];
    let payload = match rng.usize(12) {
        0 => None,
        1 => Some(Value::Null),
        2 => Some(json!({ "status": Value::Null, "tier": "gold" })),
        3 => Some(json!({ "tier": "gold" })),
        4 => Some(json!({})),
        5 => Some(json!({ "status": { "inner": status }, "tier": "silver" })),
        6 => Some(json!({ "status": [status, "draft"] })),
        7 => Some(json!({ "status": i as i64 % 7, "tier": "bronze" })),
        8 => Some(json!({ "status": i % 2 == 0 })),
        9 => Some(json!({ "status": "", "tier": "none" })),
        10 => Some(json!({ "status": status, "nested": { "status": "buried" } })),
        _ => Some(json!({ "status": status, "tier": "gold" })),
    };
    Row {
        key: format!("k{i:05}"),
        n: i as i64,
        payload,
    }
}

/// The EXTRACTION RULE, written out here.
///
/// `None` is the NULL key -- the key a missing value and a null value already
/// share, which no equality on a value can name. A string is itself; a number
/// and a boolean are their canonical JSON text; JSON null, an absent member,
/// an object, an array and an absent COLUMN are all the NULL key.
fn extracted(payload: Option<&Value>, member: &str) -> Option<String> {
    match payload.and_then(|d| d.get(member)) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(b)) => Some(if *b { "true" } else { "false" }.to_owned()),
        _ => None,
    }
}

struct Fixture {
    db: Database,
    rows: Vec<Row>,
}

fn config() -> Config {
    Config {
        budget_bytes: 32 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn write_rows(db: &mut Database, rows: &[Row]) {
    let c = db.collection("doc").unwrap().unwrap();
    for (n, row) in rows.iter().enumerate() {
        let mut document = json!({ "n": row.n });
        if let Some(payload) = &row.payload {
            document["payload"] = payload.clone();
        }
        db.put(c, &row.key, &document).unwrap();
        if (n + 1) % 256 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
}

fn build(dir: &TempDir) -> Fixture {
    let mut db = Database::create(&dir.path().join("db"), config()).unwrap();
    // `n` carries an ordinary btree so a predicate that is NOT the driver can
    // still be an extracted-member equality.
    db.sql(
        "CREATE TABLE doc (payload JSONB, n INT) WITH (index: [n])",
        &[],
    )
    .unwrap();
    db.commit().unwrap();
    let mut rng = Rng(SEED);
    let rows: Vec<Row> = (0..ROWS).map(|i| generate(i, &mut rng)).collect();
    // The documents are written through the engine handle rather than as SQL
    // literals: the corpus is the shape set above, and a JSON literal in a
    // statement would only re-parse it.
    write_rows(&mut db, &rows);
    db.sql("CREATE INDEX doc_status ON doc ((payload->>'status'))", &[])
        .unwrap();
    db.commit().unwrap();
    Fixture { db, rows }
}

fn rows_of(result: SqlResult) -> Vec<SqlRow> {
    match result {
        SqlResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn text(value: &SqlValue) -> String {
    match value {
        SqlValue::Text(text) => text.clone(),
        other => panic!("expected text, got {other:?}"),
    }
}

/// The keys one statement returns, sorted.
fn keys(f: &mut Fixture, statement: &str) -> Vec<String> {
    let mut out: Vec<String> = rows_of(f.db.sql(statement, &[]).unwrap())
        .iter()
        .map(|row| text(&row.values[0]))
        .collect();
    out.sort();
    out
}

/// The brute-force answer: every corpus row the rule admits, with no index
/// and no rewrite in sight.
fn oracle(f: &Fixture, keep: impl Fn(&Row) -> bool) -> Vec<String> {
    let mut out: Vec<String> = f
        .rows
        .iter()
        .filter(|row| keep(row))
        .map(|row| row.key.clone())
        .collect();
    out.sort();
    out
}

fn message(error: &SqlError) -> String {
    format!("{error}")
}

// ── the oracle ────────────────────────────────────────────────────────────

#[test]
fn an_extracted_member_equality_names_exactly_the_rows_the_brute_force_extraction_names() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    // Every value the corpus can produce an extraction of, plus values it
    // cannot: the spellings of a JSON null, of an object and of an array are
    // asked for by name and must find nothing.
    let asked = [
        "live", "draft", "archived", "hidden", "", "true", "false", "0", "1", "2", "3", "4", "5",
        "6", "buried", "null", "{}", "[]", r#"{"inner":"live"}"#, "gold",
    ];
    for want in asked {
        let got = keys(
            &mut f,
            &format!("SELECT _key FROM doc WHERE payload->>'status' = '{want}'"),
        );
        assert_eq!(
            got,
            oracle(&f, |row| extracted(row.payload.as_ref(), "status").as_deref()
                == Some(want)),
            "payload->>'status' = '{want}'"
        );
    }
    // The corpus is not degenerate: the common value has a large answer, and
    // the rows the rule sends to the NULL key are a real share of it.
    let live = keys(
        &mut f,
        "SELECT _key FROM doc WHERE payload->>'status' = 'live'",
    );
    assert!(live.len() > 50, "the corpus holds live rows: {}", live.len());
    let nulls = f
        .rows
        .iter()
        .filter(|row| extracted(row.payload.as_ref(), "status").is_none())
        .count();
    assert!(
        nulls > 100,
        "the corpus holds rows with no extracted value: {nulls}"
    );
}

#[test]
fn a_row_whose_member_is_absent_is_not_findable_by_an_equality_on_any_value() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    let absent: Vec<String> = f
        .rows
        .iter()
        .filter(|row| extracted(row.payload.as_ref(), "status").is_none())
        .map(|row| row.key.clone())
        .collect();
    assert!(!absent.is_empty());
    for want in ["live", "draft", "", "null", "true", "0", "gold", "{}"] {
        let found = keys(
            &mut f,
            &format!("SELECT _key FROM doc WHERE payload->>'status' = '{want}'"),
        );
        for key in &absent {
            assert!(
                !found.contains(key),
                "`{key}` has no extracted value and must not answer = '{want}'"
            );
        }
    }
}

#[test]
fn an_extracted_member_equality_that_is_not_the_driver_equals_the_brute_force_filter() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    // `n < 400` drives from its own ordinary btree; the extracted equality is
    // then recomputed FROM THE ROW, which is the path that has to pass
    // through the expression rather than compare the document itself.
    let got = keys(
        &mut f,
        "SELECT _key FROM doc WHERE n < 400 AND payload->>'status' = 'live'",
    );
    assert_eq!(
        got,
        oracle(&f, |row| row.n < 400
            && extracted(row.payload.as_ref(), "status").as_deref() == Some("live"))
    );
    assert!(!got.is_empty());
}

// ── the refusals ──────────────────────────────────────────────────────────

#[test]
fn a_json_member_predicate_without_its_expression_index_is_refused_naming_the_index_it_needs() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    // `tier` is a member of the same documents with no index over it.
    let error = f
        .db
        .sql("SELECT _key FROM doc WHERE payload->>'tier' = 'gold'", &[])
        .unwrap_err();
    let said = message(&error);
    assert!(
        matches!(error, SqlError::Unsupported(_)),
        "a missing index is Unsupported, not Engine: {said}"
    );
    assert!(
        said.contains("CREATE INDEX ... ON t ((payload->>'tier'))"),
        "the refusal names the index it would need: {said}"
    );
    assert!(
        said.contains("does not exist"),
        "the refusal says the index is not there: {said}"
    );
    // And nothing was scanned: the statement produced no rows at all.
    assert!(f
        .db
        .sql("SELECT _key FROM doc WHERE payload->>'tier' = 'gold'", &[])
        .is_err());
}

#[test]
fn two_indexes_over_two_members_of_one_column_do_not_answer_each_others_predicate() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    f.db.sql("CREATE INDEX doc_tier ON doc ((payload->>'tier'))", &[])
        .unwrap();
    f.db.commit().unwrap();
    for member in ["status", "tier"] {
        for want in ["live", "gold", "draft", "silver"] {
            let got = keys(
                &mut f,
                &format!("SELECT _key FROM doc WHERE payload->>'{member}' = '{want}'"),
            );
            assert_eq!(
                got,
                oracle(&f, |row| extracted(row.payload.as_ref(), member).as_deref()
                    == Some(want)),
                "payload->>'{member}' = '{want}'"
            );
        }
    }
    // The two answers differ, which is why the member is part of the identity
    // of the index a predicate names.
    let status = keys(
        &mut f,
        "SELECT _key FROM doc WHERE payload->>'status' = 'live'",
    );
    let tier = keys(&mut f, "SELECT _key FROM doc WHERE payload->>'tier' = 'gold'");
    assert!(!status.is_empty() && !tier.is_empty() && status != tier);
}

#[test]
fn the_json_operators_with_no_index_atomic_are_still_refused_by_name() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    for (statement, keyword) in [
        (
            "SELECT _key FROM doc WHERE payload->'status' = 'live'",
            "->",
        ),
        (
            "SELECT _key FROM doc WHERE payload#>'{status}' = 'live'",
            "#>",
        ),
        (
            "SELECT _key FROM doc WHERE payload#>>'{status}' = 'live'",
            "#>>",
        ),
        (
            "SELECT _key FROM doc WHERE json_array_length(payload) = 2",
            "JSON_ARRAY_LENGTH",
        ),
        // `->>` itself, outside the two positions that compile.
        ("SELECT payload->>'status' FROM doc", "->>"),
        (
            "SELECT _key FROM doc WHERE n < 10 ORDER BY payload->>'status'",
            "->>",
        ),
    ] {
        let error = f.db.sql(statement, &[]).unwrap_err();
        match &error {
            SqlError::Refused { keyword: got, tier, .. } => {
                assert_eq!(got, keyword, "{statement}");
                assert_eq!(*tier, Tier::Two, "{statement}");
            }
            other => panic!("`{statement}` answered {other:?} rather than a named refusal"),
        }
    }
    // An ORDERING comparison over the extracted value is refused too: the
    // index stores the member's text and the rewrite the contract names is an
    // equality.
    let error = f
        .db
        .sql(
            "SELECT _key FROM doc WHERE payload->>'status' > 'live'",
            &[],
        )
        .unwrap_err();
    assert!(
        message(&error).contains("ordering comparison"),
        "{}",
        message(&error)
    );
}

#[test]
fn an_extracted_member_index_over_a_column_that_is_not_json_is_refused_by_name() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    let error = f
        .db
        .sql("CREATE INDEX doc_n ON doc ((n->>'status'))", &[])
        .unwrap_err();
    assert!(
        message(&error).contains("JSONB"),
        "the refusal names the kind `->>` reads: {}",
        message(&error)
    );
}

// ── durability and the verifier ───────────────────────────────────────────

#[test]
fn an_extracted_member_index_survives_a_reopen_and_still_answers() {
    let dir = TempDir::new().unwrap();
    let before = {
        let mut f = build(&dir);
        let answer = keys(
            &mut f,
            "SELECT _key FROM doc WHERE payload->>'status' = 'live'",
        );
        assert!(!answer.is_empty());
        answer
    };
    let mut f = Fixture {
        db: Database::open(&dir.path().join("db"), config()).unwrap(),
        rows: {
            let mut rng = Rng(SEED);
            (0..ROWS).map(|i| generate(i, &mut rng)).collect()
        },
    };
    let after = keys(
        &mut f,
        "SELECT _key FROM doc WHERE payload->>'status' = 'live'",
    );
    assert_eq!(after, before);
    assert_eq!(
        after,
        oracle(&f, |row| extracted(row.payload.as_ref(), "status").as_deref()
            == Some("live"))
    );
    // The catalog says what it is, by the spelling a statement would write.
    let definition = rows_of(
        f.db.sql(
            "SELECT indexdef FROM pg_indexes WHERE indexname = 'doc_status'",
            &[],
        )
        .unwrap(),
    );
    assert_eq!(definition.len(), 1);
    assert!(
        text(&definition[0].values[0]).contains("((payload->>'status'))"),
        "pg_indexes prints the expression target: {:?}",
        text(&definition[0].values[0])
    );
}

#[test]
fn the_verifier_is_clean_after_writes_that_change_the_extracted_value() {
    let dir = TempDir::new().unwrap();
    let path = {
        let mut f = build(&dir);
        let c = f.db.collection("doc").unwrap().unwrap();
        // Rewrite every eighth row so its extracted value MOVES: a string
        // becomes a different string, a scalar becomes an object, an absent
        // member gains one, and one row loses the column altogether. Each of
        // those has to delete one posting and write another.
        let mut changed = 0;
        for i in (0..ROWS).step_by(8) {
            let row = &mut f.rows[i];
            row.payload = match i % 32 {
                0 => Some(json!({ "status": "rewritten", "tier": "gold" })),
                8 => Some(json!({ "status": { "now": "an object" } })),
                16 => Some(json!({ "status": 99, "tier": "bronze" })),
                _ => None,
            };
            let mut document = json!({ "n": row.n });
            if let Some(payload) = &row.payload {
                document["payload"] = payload.clone();
            }
            f.db.put(c, &row.key, &document).unwrap();
            changed += 1;
        }
        f.db.commit().unwrap();
        assert!(changed > 100);
        // The index still answers the rule over the CHANGED corpus.
        for want in ["live", "rewritten", "99", "draft"] {
            let got = keys(
                &mut f,
                &format!("SELECT _key FROM doc WHERE payload->>'status' = '{want}'"),
            );
            assert_eq!(
                got,
                oracle(&f, |row| extracted(row.payload.as_ref(), "status").as_deref()
                    == Some(want)),
                "after the rewrite: payload->>'status' = '{want}'"
            );
        }
        assert!(!keys(
            &mut f,
            "SELECT _key FROM doc WHERE payload->>'status' = 'rewritten'"
        )
        .is_empty());
        drop(f);
        dir.path().join("db")
    };
    let mut seen: Vec<String> = Vec::new();
    let report = verify_indexed_source(&path, VerificationLimits::default(), |issue| {
        if seen.len() < 8 {
            seen.push(format!("{issue:?}"));
        }
    })
    .unwrap();
    assert!(
        report.clean && report.complete,
        "a healthy JSON-member index must verify clean; first issues: {seen:?}"
    );
    assert_eq!(report.derived_issues, 0);
    assert_eq!(report.primary_issues, 0);
    assert_eq!(report.catalog_issues, 0);
}
