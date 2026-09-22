//! A JSON-PATH expression index: `IndexExpr::JsonText`, the second variant of
//! the expression index the scalar family already shipped.
//!
//! `docs/lang/INDEX_CONTRACT.md` recorded a `JSONB` column as a declared type
//! nothing could filter on. This file is the engine half of closing that: the
//! index stores `col->>'member'` -- the TEXT at one member of the document --
//! in an ordinary scalar index, under descriptor version 4 and the additive
//! feature bit `JSON_EXPRESSION_FEATURE`.
//!
//! Three things are asserted here, and the SQL half lives in
//! `lang/tests/sql_json_path.rs`:
//!
//! 1. The EXTRACTION RULE. It is total: a missing member, a JSON null, an
//!    object and an array each have ONE stored value -- the NULL key -- and a
//!    row whose member is absent is therefore not findable by an equality on
//!    any value. A string is itself, a number and a boolean are their
//!    canonical JSON text. The oracle for this is written out in this process
//!    and never read back from the engine.
//! 2. The DESCRIPTOR. The member name travels with the index and comes back
//!    across a reopen, because it is part of the identity of the index a
//!    predicate names.
//! 3. LAW 8. A file carrying such a descriptor is refused WHOLE, as
//!    `Unsupported` and not as `Corrupt`, by a binary that predates the bit,
//!    with no byte of the file changed by the refusal.
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::{
    collections::{
        CandidateDriver, CollectionOptions, Database, IndexExpr, IndexId, ProjectedValue,
        Projection, QueryBudget, QueryFilter, QueryOrder, QueryRequest, ScalarFilter, ScalarValue,
        JSON_EXPRESSION_FEATURE, SUPPORTED_LOGICAL_FEATURES,
    },
    internal::{admit_logical_features, logical_features},
    Kind,
};
use serde_json::{json, Value};

fn cfg() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn budget() -> QueryBudget {
    QueryBudget {
        candidates: 1_000_000,
        primary_reads: 1_000_000,
        scalar_postings: 1_000_000,
        graph_edges: 1_000_000,
        graph_visited: 1_000_000,
        spatial_postings: 1_000_000,
        text_postings: 1_000_000,
        text_tokens: 1_000_000,
        vector_locators: 1_000_000,
        vector_sidecars: 1_000_000,
        vector_lanes: 1_000_000,
        key_postings: 1_000_000,
        rows_written: 1_000_000,
        groups: 1_000_000,
        output_bytes: 16 << 20,
        deadline: None,
    }
}

/// The documents the fixture holds, one shape per line of the extraction
/// rule, plus one row whose COLUMN is absent entirely.
///
/// `None` means the row is written without the `payload` field at all.
fn documents() -> Vec<(&'static str, Option<Value>)> {
    vec![
        ("a_string", Some(json!({ "status": "live" }))),
        ("another_string", Some(json!({ "status": "draft" }))),
        ("an_integer", Some(json!({ "status": 7 }))),
        ("a_negative", Some(json!({ "status": -2 }))),
        ("a_float", Some(json!({ "status": 2.5 }))),
        ("a_true", Some(json!({ "status": true }))),
        ("a_false", Some(json!({ "status": false }))),
        ("a_json_null", Some(json!({ "status": Value::Null }))),
        ("an_absent_member", Some(json!({ "other": "live" }))),
        ("an_empty_object", Some(json!({}))),
        ("an_object_at_the_member", Some(json!({ "status": { "inner": "live" } }))),
        ("an_array_at_the_member", Some(json!({ "status": ["live", "draft"] }))),
        ("a_null_column", Some(Value::Null)),
        ("a_missing_column", None),
    ]
}

/// The extraction rule, written out here so the assertion is against the
/// RULE and not against a second reading of the engine's own answer.
///
/// `None` is "the NULL key": the one key a missing value and a null value
/// already share, which no equality on a value can name.
fn extracted(document: Option<&Value>) -> Option<String> {
    match document.and_then(|d| d.get("status")) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(b)) => Some(if *b { "true" } else { "false" }.to_owned()),
        _ => None,
    }
}

struct Fixture {
    db: Database,
    index: IndexId,
    rows: Vec<(&'static str, Option<Value>)>,
}

fn build(dir: &std::path::Path) -> Fixture {
    let mut db = Database::create(dir.join("db"), cfg()).unwrap();
    let c = db
        .create_collection(
            "doc",
            vec![("payload".into(), Kind::Json), ("n".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    let rows = documents();
    for (n, (key, document)) in rows.iter().enumerate() {
        let mut row = json!({ "n": n as i64 });
        if let Some(document) = document {
            row["payload"] = document.clone();
        }
        db.put(c, key, &row).unwrap();
    }
    db.commit().unwrap();
    let index = db
        .create_expression_index(
            c,
            "doc_status",
            "payload",
            IndexExpr::JsonText("status".into()),
            false,
        )
        .unwrap();
    db.build_index_to_ready(index, 64).unwrap();
    db.commit().unwrap();
    Fixture { db, index, rows }
}

/// The keys an equality over the expression index returns, sorted.
fn keys(f: &Fixture, want: &str) -> Vec<String> {
    let collection = f.db.collection("doc").unwrap().unwrap();
    let fields = ["n"];
    let filter = QueryFilter::Scalar {
        index: f.index,
        predicate: ScalarFilter::Eq(ScalarValue::Text(want)),
    };
    let mut query = f
        .db
        .prepare_query(QueryRequest {
            collection,
            filters: &[filter],
            order: QueryOrder::EntityId,
            projection: Projection::Fields(&fields),
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    // The row carries its own ordinal, which is the fixture's index into
    // `documents()`: the test names rows by the key it wrote them under
    // without asking the engine to map an id back to a key.
    let mut out: Vec<String> = Vec::new();
    loop {
        let page = query.next_page(8, budget(), || false).unwrap();
        for row in &page.rows {
            let n = match &row.projected[0].1 {
                ProjectedValue::Value(value) => value.as_i64().unwrap(),
                other => panic!("expected the ordinal, got {other:?}"),
            };
            out.push(f.rows[n as usize].0.to_owned());
        }
        if page.done {
            break;
        }
    }
    out.sort();
    out
}

/// Every byte of a database, file by file in name order, so "no byte changed"
/// is asserted over the whole store rather than over one file of it.
fn bytes_of(path: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = std::fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(!out.is_empty(), "the store has files");
    out
}

fn oracle(f: &Fixture, want: &str) -> Vec<String> {
    let mut out: Vec<String> = f
        .rows
        .iter()
        .filter(|(_, document)| extracted(document.as_ref()).as_deref() == Some(want))
        .map(|(key, _)| (*key).to_owned())
        .collect();
    out.sort();
    out
}

#[test]
fn a_missing_member_a_null_and_a_non_scalar_each_store_the_one_null_key() {
    let dir = tempfile::tempdir().unwrap();
    let f = build(dir.path());
    // Every value the corpus can produce, asked for by equality. The five
    // rows whose rule says "the NULL key" -- the JSON null, the absent
    // member, the empty object, the object and the array -- plus the null
    // column and the missing column are named by NONE of them.
    for want in [
        "live", "draft", "7", "-2", "2.5", "true", "false", "null", "{}", "[]", "",
    ] {
        assert_eq!(keys(&f, want), oracle(&f, want), "payload->>'status' = {want:?}");
    }
    // Said as the rule says it: a row whose member is absent is findable by
    // no equality at all.
    for (key, document) in &f.rows {
        if extracted(document.as_ref()).is_some() {
            continue;
        }
        for want in ["live", "draft", "7", "true", "null", "", "{}"] {
            assert!(
                !keys(&f, want).contains(&(*key).to_owned()),
                "`{key}` has no extracted value and must not answer = {want:?}"
            );
        }
    }
    // And the rows that DO have one are each found by their own value, so the
    // assertion above is not vacuous.
    assert_eq!(keys(&f, "live"), vec!["a_string".to_owned()]);
    assert_eq!(keys(&f, "7"), vec!["an_integer".to_owned()]);
    assert_eq!(keys(&f, "true"), vec!["a_true".to_owned()]);
    assert_eq!(keys(&f, "2.5"), vec!["a_float".to_owned()]);
}

#[test]
fn the_member_name_travels_in_the_descriptor_and_comes_back_across_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let f = build(dir.path());
    drop(f);
    let mut db = Database::open(dir.path().join("db"), cfg()).unwrap();
    let c = db.collection("doc").unwrap().unwrap();
    let indexes = db.list_indexes(c).unwrap();
    let info = indexes
        .iter()
        .find(|i| i.name == "doc_status")
        .expect("the expression index is in the catalog after a reopen");
    assert_eq!(info.expression, Some(IndexExpr::JsonText("status".into())));
    assert_eq!(info.encoding_version, 4);
    // The index KEYS are Text while the SOURCE field is the Json column: the
    // two kinds differ, which is what the layout check had to learn.
    assert_eq!(info.kind, Kind::Text);
    assert_eq!(info.field, "payload");
    // A second index over a DIFFERENT member of the same column is a
    // different index, and the catalog holds both.
    let other = db
        .create_expression_index(
            c,
            "doc_other",
            "payload",
            IndexExpr::JsonText("other".into()),
            false,
        )
        .unwrap();
    db.build_index_to_ready(other, 64).unwrap();
    db.commit().unwrap();
    let indexes = db.list_indexes(c).unwrap();
    assert_eq!(
        indexes
            .iter()
            .filter(|i| i.expression.is_some())
            .count(),
        2
    );
    assert!(indexes
        .iter()
        .any(|i| i.expression == Some(IndexExpr::JsonText("other".into()))));
}

#[test]
fn an_expression_over_a_json_member_refuses_a_column_that_is_not_json() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("db"), cfg()).unwrap();
    let c = db
        .create_collection(
            "plain",
            vec![("label".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();
    let refused = db
        .create_expression_index(
            c,
            "bad",
            "label",
            IndexExpr::JsonText("status".into()),
            false,
        )
        .unwrap_err();
    assert!(
        format!("{refused}").contains("JSONB"),
        "the refusal names the kind the expression reads: {refused}"
    );
    // And `lower` over the Json column is refused the same way, from the
    // other side of the same check.
    let c2 = db
        .create_collection(
            "docs",
            vec![("payload".into(), Kind::Json)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();
    let refused = db
        .create_expression_index(c2, "bad2", "payload", IndexExpr::Lower, false)
        .unwrap_err();
    assert!(
        format!("{refused}").contains("TEXT"),
        "the refusal names the kind `lower` reads: {refused}"
    );
}

// ── Law 8: the older binary ───────────────────────────────────────────────

/// A file carrying a JSON-path expression index is refused at ADMISSION by a
/// binary that predates the bit, as `Unsupported` and not as `Corrupt`, and
/// the refusal changes no byte of it.
///
/// The bit is taken even though descriptor version 4 would also stop such a
/// binary on its own: that refusal happens when a DESCRIPTOR is read, deep
/// inside admission, and reads as "index family 1 encoding 4" rather than as
/// "this file is newer than this build". Law 8 asks for the refusal that
/// cannot be mistaken for damage, and the feature word is it.
#[test]
fn a_json_path_expression_file_is_unsupported_to_a_binary_that_predates_the_bit() {
    assert_eq!(JSON_EXPRESSION_FEATURE, 0x10000);
    assert_eq!(
        SUPPORTED_LOGICAL_FEATURES & JSON_EXPRESSION_FEATURE,
        JSON_EXPRESSION_FEATURE,
        "the mask this build publishes must contain the bit it writes"
    );
    let older = SUPPORTED_LOGICAL_FEATURES & !JSON_EXPRESSION_FEATURE;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let f = {
        let f = build(dir.path());
        let written = logical_features(&f.db);
        drop(f);
        written
    };
    assert_eq!(
        f & JSON_EXPRESSION_FEATURE,
        JSON_EXPRESSION_FEATURE,
        "a file holding a JSON-path expression index declares the bit"
    );
    // A `lower(col)` index declares the SHIPPED expression bit and not this
    // one, so the new bit is not set by every expression index.
    {
        let other = dir.path().join("lower");
        let mut db = Database::create(&other, cfg()).unwrap();
        let c = db
            .create_collection(
                "t",
                vec![("label".into(), Kind::Text)],
                CollectionOptions::default(),
            )
            .unwrap();
        db.commit().unwrap();
        let id = db
            .create_expression_index(c, "t_lower", "label", IndexExpr::Lower, false)
            .unwrap();
        db.build_index_to_ready(id, 64).unwrap();
        db.commit().unwrap();
        assert_eq!(logical_features(&db) & JSON_EXPRESSION_FEATURE, 0);
    }

    // This build opens the file it writes.
    admit_logical_features(f, SUPPORTED_LOGICAL_FEATURES).unwrap();
    let before = bytes_of(&path);
    Database::open(&path, cfg()).unwrap();

    // The binary that predates the bit refuses it WHOLE, before a descriptor
    // is read, and as Unsupported naming the feature word it could not
    // honour.
    let refused = admit_logical_features(f, older).unwrap_err();
    assert!(
        matches!(refused, sekejap_core::collections::Error::Unsupported(ref m)
            if m.contains(&format!("{f:#x}"))),
        "an intact newer file must be Unsupported and name its feature word: {refused:?}"
    );
    assert_eq!(bytes_of(&path), before, "a refused admission writes nothing");
}
