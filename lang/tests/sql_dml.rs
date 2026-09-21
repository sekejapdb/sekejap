//! The SQL spellings of the predicated writes and the bulk scope, against the
//! same brute-force oracle `core/engine/tests/write_where.rs` uses.
//!
//! The oracle is a `BTreeMap` built in this process and never the engine's
//! second reading: the predicate is evaluated over that map in plain Rust,
//! the statement runs, and the collection is read back and compared.

use sekejap_core::collections::{CollectionId, CollectionOptions, Database, IndexId};
use sekejap_core::Kind;
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_lang::{Param, SqlDatabase, SqlError, SqlResult, Tier};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tempfile::TempDir;

fn cfg() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

struct Fixture {
    db: Database,
    collection: CollectionId,
    #[allow(dead_code)]
    amount_index: IndexId,
    oracle: BTreeMap<String, Value>,
    _dir: TempDir,
}

/// 300 rows: an indexed `amount`, an unindexed `label` and an unindexed
/// `score`. `amount` is not a function of the key order, so a range over it is
/// neither a key prefix nor an id prefix.
fn open() -> Fixture {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("db"), cfg()).unwrap();
    let collection = db
        .create_collection(
            "rows",
            vec![
                ("amount".into(), Kind::Int),
                ("label".into(), Kind::Text),
                ("score".into(), Kind::Int),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    let mut oracle = BTreeMap::new();
    for i in 0..300usize {
        let amount = ((i * 37) % 100) as i64;
        let document = json!({
            "amount": amount,
            "label": format!("L{}", amount % 7),
            "score": amount * 2,
        });
        db.put(collection, &format!("k{i:04}"), &document).unwrap();
        oracle.insert(format!("k{i:04}"), document);
    }
    db.commit().unwrap();
    let amount_index = db
        .create_scalar_index(collection, "by_amount", "amount", false)
        .unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(amount_index, 256).unwrap();
    db.commit().unwrap();
    Fixture {
        db,
        collection,
        amount_index,
        oracle,
        _dir: dir,
    }
}

impl Fixture {
    fn read_back(&self) -> BTreeMap<String, Value> {
        let mut out = BTreeMap::new();
        for entity in self.db.scan(self.collection, None).unwrap() {
            let entity = entity.unwrap();
            let mut document = entity.document.clone();
            document
                .as_object_mut()
                .unwrap()
                .retain(|name, _| !name.starts_with("__e4"));
            out.insert(entity.key.clone(), document);
        }
        out
    }
}

/// The brute-force set of keys whose `amount` is in `[lower, upper)`.
fn matching(oracle: &BTreeMap<String, Value>, lower: i64, upper: i64) -> Vec<String> {
    oracle
        .iter()
        .filter(|(_, row)| {
            let amount = row["amount"].as_i64().unwrap();
            amount >= lower && amount < upper
        })
        .map(|(key, _)| key.clone())
        .collect()
}

fn affected(result: SqlResult) -> u64 {
    match result {
        SqlResult::Affected(n) => n,
        other => panic!("expected an affected count, got {other:?}"),
    }
}

#[test]
fn delete_from_t_where_a_predicate_removes_exactly_the_rows_the_oracle_names() {
    let mut f = open();
    let doomed = matching(&f.oracle, 20, 40);
    assert!(doomed.len() > 30 && doomed.len() < 200);
    let n = affected(
        f.db.sql("DELETE FROM rows WHERE amount >= 20 AND amount < 40", &[])
            .unwrap(),
    );
    f.db.commit().unwrap();
    assert_eq!(n, doomed.len() as u64);
    for key in &doomed {
        f.oracle.remove(key);
    }
    assert_eq!(f.read_back(), f.oracle);
}

#[test]
fn update_t_set_where_a_predicate_rewrites_exactly_the_rows_the_oracle_names() {
    let mut f = open();
    let touched = matching(&f.oracle, 50, 70);
    assert!(touched.len() > 20);
    let n = affected(
        f.db.sql(
            "UPDATE rows SET label = $1 WHERE amount >= 50 AND amount < 70",
            &[Param::Text("patched".into())],
        )
        .unwrap(),
    );
    f.db.commit().unwrap();
    assert_eq!(n, touched.len() as u64);
    for key in &touched {
        f.oracle.get_mut(key).unwrap()["label"] = json!("patched");
    }
    assert_eq!(f.read_back(), f.oracle);
}

#[test]
fn a_set_expression_over_the_same_row_is_a_row_function_and_reads_no_other_row() {
    let mut f = open();
    let touched = matching(&f.oracle, 0, 20);
    let n = affected(
        f.db.sql(
            "UPDATE rows SET score = score + 1, label = lower(label) WHERE amount >= 0 AND amount < 20",
            &[],
        )
        .unwrap(),
    );
    f.db.commit().unwrap();
    assert_eq!(n, touched.len() as u64);
    for key in &touched {
        let row = f.oracle.get_mut(key).unwrap();
        row["score"] = json!(row["score"].as_i64().unwrap() + 1);
        let lowered = row["label"].as_str().unwrap().to_lowercase();
        row["label"] = json!(lowered);
    }
    assert_eq!(f.read_back(), f.oracle);
}

#[test]
fn a_set_expression_over_the_driving_index_is_refused_and_names_the_index() {
    let mut f = open();
    let before = f.read_back();
    let error = f
        .db
        .sql("UPDATE rows SET amount = amount + 1000 WHERE amount < 20", &[])
        .unwrap_err();
    let text = format!("{error}");
    assert!(
        text.contains("by_amount") && text.contains("CandidateDriver::Entities"),
        "the refusal names the index the walk rides and the remedy: {text}"
    );
    f.db.rollback().unwrap();
    assert_eq!(f.read_back(), before, "a refused statement changes nothing");
}

#[test]
fn the_key_forms_of_update_and_delete_stay_the_single_key_atomics() {
    let mut f = open();
    assert_eq!(
        affected(
            f.db.sql("UPDATE rows SET label = 'one' WHERE _key = 'k0002'", &[])
                .unwrap()
        ),
        1
    );
    assert_eq!(
        affected(f.db.sql("DELETE FROM rows WHERE _key = 'k0001'", &[]).unwrap()),
        1
    );
    // A key that is not there affects nothing and is not an error.
    assert_eq!(
        affected(f.db.sql("DELETE FROM rows WHERE _key = 'nope'", &[]).unwrap()),
        0
    );
    f.db.commit().unwrap();
    f.oracle.get_mut("k0002").unwrap()["label"] = json!("one");
    f.oracle.remove("k0001");
    assert_eq!(f.read_back(), f.oracle);
}

#[test]
fn delete_where_a_key_range_is_the_predicated_form_over_the_key_driver() {
    let mut f = open();
    let doomed: Vec<String> = f
        .oracle
        .keys()
        .filter(|key| key.as_str() >= "k0290")
        .cloned()
        .collect();
    assert_eq!(doomed.len(), 10);
    let n = affected(
        f.db.sql("DELETE FROM rows WHERE _key >= 'k0290'", &[])
            .unwrap(),
    );
    f.db.commit().unwrap();
    assert_eq!(n, 10);
    for key in &doomed {
        f.oracle.remove(key);
    }
    assert_eq!(f.read_back(), f.oracle);
}

#[test]
fn restrict_is_the_default_and_cascade_is_the_explicit_word() {
    let mut f = open();
    f.db.enable_graph().unwrap();
    let context = f.db.create_graph_context("routes").unwrap();
    let edge_type = f.db.create_edge_type("near").unwrap();
    let doomed = matching(&f.oracle, 0, 20);
    let anchored = doomed.last().unwrap().clone();
    let source = f.db.get(f.collection, &anchored).unwrap().unwrap().id;
    let other = f.db.get(f.collection, "k0002").unwrap().unwrap().id;
    f.db.put_edge(context, source, edge_type, other, &json!({}))
        .unwrap();
    f.db.commit().unwrap();
    let before = f.read_back();

    let error = f
        .db
        .sql("DELETE FROM rows WHERE amount < 20", &[])
        .unwrap_err();
    let text = format!("{error}");
    assert!(
        text.contains("RESTRICT") && text.contains("routes"),
        "the default refusal names the mode and the context: {text}"
    );
    f.db.rollback().unwrap();
    assert_eq!(f.read_back(), before);

    let n = affected(
        f.db.sql("DELETE FROM rows WHERE amount < 20 CASCADE", &[])
            .unwrap(),
    );
    f.db.commit().unwrap();
    assert_eq!(n, doomed.len() as u64);
    for key in &doomed {
        f.oracle.remove(key);
    }
    assert_eq!(f.read_back(), f.oracle);
}

#[test]
fn explain_of_a_predicated_write_prints_the_driver_the_bound_and_the_mode_without_running_it() {
    let mut f = open();
    let before = f.read_back();
    let plan = match f
        .db
        .sql("EXPLAIN DELETE FROM rows WHERE amount >= 20 AND amount < 40", &[])
        .unwrap()
    {
        SqlResult::Explain(text) => text,
        other => panic!("expected an EXPLAIN, got {other:?}"),
    };
    assert!(plan.contains("DELETE FROM rows"), "{plan}");
    assert!(plan.contains("driver: Scalar"), "{plan}");
    assert!(plan.contains("by_amount"), "{plan}");
    assert!(plan.contains("rows written:"), "{plan}");
    assert!(plan.contains("rows_written"), "{plan}");
    assert!(plan.contains("RESTRICT"), "{plan}");
    assert!(plan.contains("commits: nothing"), "{plan}");
    assert_eq!(f.read_back(), before, "an EXPLAIN of a write does not run it");

    let plan = match f
        .db
        .sql(
            "EXPLAIN UPDATE rows SET score = score + 1, label = 'x' WHERE amount < 10",
            &[],
        )
        .unwrap()
    {
        SqlResult::Explain(text) => text,
        other => panic!("expected an EXPLAIN, got {other:?}"),
    };
    assert!(plan.contains("UPDATE rows"), "{plan}");
    assert!(plan.contains("row function"), "{plan}");
    assert!(plan.contains("constant"), "{plan}");
    assert_eq!(f.read_back(), before);

    let cascade = match f
        .db
        .sql("EXPLAIN DELETE FROM rows WHERE amount < 10 CASCADE", &[])
        .unwrap()
    {
        SqlResult::Explain(text) => text,
        other => panic!("expected an EXPLAIN, got {other:?}"),
    };
    assert!(cascade.contains("CASCADE"), "{cascade}");
}

#[test]
fn the_rows_written_budget_refuses_the_statement_rather_than_truncating_it() {
    let mut f = open();
    let mut budget = sekejap_core::collections::QueryBudget::unlimited();
    budget.rows_written = 12;
    let error = f
        .db
        .sql_with(
            "DELETE FROM rows WHERE amount >= 0 AND amount < 50",
            &[],
            budget,
            &mut || false,
        )
        .unwrap_err();
    let text = format!("{error}");
    assert!(
        text.contains("rows_written") && text.contains("12") && text.contains("refused"),
        "the refusal names the budget and the count it reached: {text}"
    );
    // The rows it wrote before the budget stopped it are uncommitted.
    f.db.rollback().unwrap();
    assert_eq!(f.read_back(), f.oracle);
}

#[test]
fn from_all_is_refused_by_name_in_both_statements_that_can_write_it() {
    let f = open();
    for text in [
        "SELECT amount FROM ALL WHERE amount > 3",
        "DELETE FROM ALL WHERE amount > 3",
        "DELETE FROM ALL",
    ] {
        let error = match f.db.sql_prepare(text, &[]) {
            Err(error) => error,
            Ok(_) => panic!("`{text}` must be refused"),
        };
        assert_eq!(error.tier(), Some(Tier::Two), "`{text}` -> {error}");
        let reason = error.reason().unwrap_or_default();
        assert!(
            reason.contains("CandidateDriver::Collections") && reason.contains("collection id"),
            "`{text}` must be refused with the driver it needs named: {reason}"
        );
    }
}

#[test]
fn begin_bulk_and_end_bulk_nest_and_only_the_outermost_close_commits() {
    let mut f = open();
    assert!(matches!(
        f.db.sql("BEGIN BULK", &[]).unwrap(),
        SqlResult::Notice(_)
    ));
    assert_eq!(f.db.bulk_depth(), 1);
    f.db.sql("INSERT INTO rows (_key, amount, label, score) VALUES ('z1', 1, 'a', 1)", &[])
        .unwrap();
    f.db.sql("BEGIN BULK", &[]).unwrap();
    assert_eq!(f.db.bulk_depth(), 2);
    f.db.sql("INSERT INTO rows (_key, amount, label, score) VALUES ('z2', 2, 'b', 2)", &[])
        .unwrap();
    assert!(matches!(
        f.db.sql("END BULK", &[]).unwrap(),
        SqlResult::Notice(_)
    ));
    assert_eq!(f.db.bulk_depth(), 1);
    assert!(matches!(
        f.db.sql("END BULK", &[]).unwrap(),
        SqlResult::Affected(0)
    ));
    assert_eq!(f.db.bulk_depth(), 0);
    assert!(f.db.get(f.collection, "z1").unwrap().is_some());
    assert!(f.db.get(f.collection, "z2").unwrap().is_some());
    // A close with no scope open is refused rather than absorbed.
    assert!(f.db.sql("END BULK", &[]).is_err());
}

#[test]
fn a_predicated_write_refuses_returning_and_a_patched_key_by_name() {
    let f = open();
    for (text, needle) in [
        ("UPDATE rows SET label = 'x' WHERE amount < 5 RETURNING _key", "RETURNING"),
        ("DELETE FROM rows WHERE amount < 5 RETURNING _key", "RETURNING"),
        ("UPDATE rows SET _key = 'x' WHERE amount < 5", "_key"),
    ] {
        let error = match f.db.sql_prepare(text, &[]) {
            Err(error) => error,
            Ok(_) => panic!("`{text}` must be refused"),
        };
        assert!(
            matches!(error, SqlError::Unsupported(_)),
            "`{text}` -> {error:?}"
        );
        assert!(format!("{error}").contains(needle), "`{text}` -> {error}");
    }
}
