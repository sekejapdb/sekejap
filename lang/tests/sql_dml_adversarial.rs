//! An ADVERSARIAL review of the SQL side of the predicated writes: the
//! parser's back-up, the refusal that arrives after rows are already written,
//! `DELETE ... CASCADE`, and the bulk scope's counter.
//!
//! Every answer is computed brute force in this process, over a `BTreeMap`
//! this file built, and compared against the collection read back. Nothing is
//! compared against a second reading by the engine.
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::{
    verification::{verify_indexed_source, VerificationLimits},
    CollectionId, CollectionOptions, Database, Direction, EntityId, GraphContextId, IndexId,
    NeighborRequest, QueryBudget,
};
use sekejap_core::Kind;
use sekejap_lang::{Param, SqlDatabase, SqlResult};
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
    db: Option<Database>,
    path: std::path::PathBuf,
    collection: CollectionId,
    #[allow(dead_code)]
    amount_index: IndexId,
    oracle: BTreeMap<String, Value>,
    _dir: TempDir,
}

/// 300 rows: an indexed `amount`, an unindexed `label`, an unindexed `score`.
/// `amount` is not a function of the key order, so a range over it is a prefix
/// of neither the key order nor the id order.
fn open() -> Fixture {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
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
    // A second index, so a predicate that is NOT a key equality has
    // something to name (QL_CONTRACT §6) and the parser's back-up has a
    // statement to back up into.
    let label_index = db
        .create_scalar_index(collection, "by_label", "label", false)
        .unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(amount_index, 256).unwrap();
    db.build_index_to_ready(label_index, 256).unwrap();
    db.commit().unwrap();
    Fixture {
        db: Some(db),
        path,
        collection,
        amount_index,
        oracle,
        _dir: dir,
    }
}

impl Fixture {
    fn db(&mut self) -> &mut Database {
        self.db.as_mut().expect("the fixture holds a handle")
    }

    fn read(&self) -> &Database {
        self.db.as_ref().expect("the fixture holds a handle")
    }

    fn read_back(&self) -> BTreeMap<String, Value> {
        let mut out = BTreeMap::new();
        for entity in self.read().scan(self.collection, None).unwrap() {
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

    fn indexes_are_consistent(&mut self, what: &str) {
        self.db().commit().unwrap();
        self.db = None;
        let mut issues = Vec::new();
        let report = verify_indexed_source(&self.path, VerificationLimits::default(), |issue| {
            issues.push(issue.message.clone())
        })
        .unwrap();
        assert!(
            report.complete && report.clean,
            "{what}: verifier found {} issue(s): {issues:?}",
            issues.len()
        );
        self.db = Some(Database::open(&self.path, cfg()).unwrap());
    }
}

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

// ── 6. the parser's back-up ───────────────────────────────────────────────

#[test]
fn a_where_that_starts_like_a_key_equality_but_is_not_compiles_as_a_predicated_write() {
    // `Parser::mark` / `reset`: the key forms are taken only when the WHOLE
    // predicate is `_key = <literal>`. Each of these starts with those three
    // tokens and is something else, so the parser must back up and compile
    // the predicated form -- with the same answer the oracle computes here.
    let mut f = open();
    let k7 = f.oracle.keys().nth(7).unwrap().clone();
    let k9 = f.oracle.keys().nth(9).unwrap().clone();
    let label7 = f.oracle[&k7]["label"].as_str().unwrap().to_owned();

    // (a) `_key = $1 AND label = $2` -- a conjunction, not a point write.
    let n = affected(
        f.db()
            .sql(
                "DELETE FROM rows WHERE _key = $1 AND label = $2",
                &[Param::Text(k7.clone()), Param::Text(label7.clone())],
            )
            .unwrap(),
    );
    assert_eq!(n, 1, "the conjunction names exactly the one row");
    f.oracle.remove(&k7);

    // (b) `_key IN (...)` -- a disjunction over the key, not an equality.
    let n = affected(
        f.db()
            .sql(
                "DELETE FROM rows WHERE _key IN ($1, $2)",
                &[Param::Text(k9.clone()), Param::Text("nope".into())],
            )
            .unwrap(),
    );
    assert_eq!(n, 1, "`IN` over the key removes the one key that exists");
    f.oracle.remove(&k9);

    // (c) `_key = lower($1)` -- the right-hand side is a function call, so
    // the token after `=` is not a literal and the back-up must happen. It is
    // either compiled as a predicated write or REFUSED by name; what it must
    // never be is a silent point write at the wrong key.
    let k11 = f.oracle.keys().nth(11).unwrap().clone();
    match f.db().sql(
        "DELETE FROM rows WHERE _key = lower($1)",
        &[Param::Text(k11.to_uppercase())],
    ) {
        Ok(result) => {
            assert_eq!(affected(result), 1, "`lower($1)` names the lower-cased key");
            f.oracle.remove(&k11);
        }
        Err(error) => {
            let text = format!("{error}");
            assert!(
                text.contains("lower") || text.contains("_key") || text.contains("function"),
                "a refusal must name what it refuses: {text}"
            );
        }
    }

    f.db().commit().unwrap();
    assert_eq!(f.read_back(), f.oracle, "the file is the oracle");
    f.indexes_are_consistent("after the backed-up parses");
}

#[test]
fn the_explain_of_a_backed_up_where_is_stable_and_names_the_predicated_plan() {
    let mut f = open();
    // A predicated write is explained WITHOUT being run, so it is explained
    // through `sql` with the keyword and not through `sql_explain`, which
    // runs what it explains. The refusal `sql_explain` gives must say so.
    let refused = f
        .read()
        .sql_explain("DELETE FROM rows WHERE amount < 5", &[])
        .unwrap_err();
    let refused = format!("{refused}");
    assert!(
        refused.contains("EXPLAIN <statement>") && refused.contains("UPDATE/DELETE"),
        "the refusal names the route that does explain it: {refused}"
    );
    let mut explain = |text: &str, params: &[Param]| -> String {
        match f.db().sql(text, params).unwrap_or_else(|e| panic!("`{text}`: {e}")) {
            SqlResult::Explain(text) => text,
            other => panic!("expected an EXPLAIN, got {other:?}"),
        }
    };
    for (statement, params) in [
        (
            "EXPLAIN DELETE FROM rows WHERE _key = $1 AND label = $2",
            vec![Param::Text("k0007".into()), Param::Text("L4".into())],
        ),
        (
            "EXPLAIN DELETE FROM rows WHERE _key IN ($1, $2)",
            vec![Param::Text("k0007".into()), Param::Text("k0009".into())],
        ),
    ] {
        let first = explain(statement, &params);
        let second = explain(statement, &params);
        assert_eq!(first, second, "EXPLAIN is stable: `{statement}`");
        assert!(
            first.starts_with("DELETE FROM rows"),
            "`{statement}` explains the predicated write: {first}"
        );
        assert!(
            first.contains("driver:") && first.contains("rows written:"),
            "`{statement}` prints the driver and the bound: {first}"
        );
        assert!(
            first.contains("commits: nothing"),
            "`{statement}` says what it commits: {first}"
        );
    }
}

#[test]
fn delete_cascade_removes_the_edges_in_every_context_and_the_verifier_stays_clean() {
    let mut f = open();
    let collection = f.collection;
    f.db().enable_graph().unwrap();
    let contexts: Vec<GraphContextId> = ["routes", "owns", "cites"]
        .iter()
        .map(|name| f.db().create_graph_context(name).unwrap())
        .collect();
    let edge_type = f.db().create_edge_type("near").unwrap();
    let doomed = matching(&f.oracle, 0, 20);
    assert!(doomed.len() > 30);
    // A hub outside the predicate, and one edge per context on every doomed
    // row, both directions used: the incoming half is what a CASCADE must
    // also remove.
    let hub_key = f.oracle.keys().last().unwrap().clone();
    let hub: EntityId = f.db().get(collection, &hub_key).unwrap().unwrap().id;
    let mut ends: Vec<EntityId> = Vec::new();
    for key in &doomed {
        let id = f.db().get(collection, key).unwrap().unwrap().id;
        ends.push(id);
        for context in &contexts {
            f.db().put_edge(*context, id, edge_type, hub, &json!({})).unwrap();
            f.db().put_edge(*context, hub, edge_type, id, &json!({})).unwrap();
        }
    }
    f.db().commit().unwrap();

    // RESTRICT (the default) refuses and names a context.
    let error = f
        .db()
        .sql("DELETE FROM rows WHERE amount >= 0 AND amount < 20", &[])
        .unwrap_err();
    let text = format!("{error}");
    assert!(
        text.contains("RESTRICT") && (text.contains("routes") || text.contains("owns") || text.contains("cites")),
        "the default refusal names the mode and a context: {text}"
    );
    f.db().rollback().unwrap();

    let n = affected(
        f.db()
            .sql(
                "DELETE FROM rows WHERE amount >= 0 AND amount < 20 CASCADE",
                &[],
            )
            .unwrap(),
    );
    f.db().commit().unwrap();
    assert_eq!(n, doomed.len() as u64);
    for key in &doomed {
        f.oracle.remove(key);
    }
    assert_eq!(f.read_back(), f.oracle);

    // Every context is empty of those edges, in BOTH directions, counted
    // against the hub -- the endpoint that survived.
    for context in &contexts {
        for direction in [Direction::Outgoing, Direction::Incoming] {
            let edges = f
                .read()
                .neighbors(NeighborRequest {
                    entity: hub,
                    direction,
                    context: *context,
                    edge_type: Some(edge_type),
                    limit: 256,
                })
                .unwrap();
            assert!(
                edges.is_empty(),
                "CASCADE left {} edge(s) on the hub in one context",
                edges.len()
            );
        }
    }
    f.indexes_are_consistent("after DELETE ... CASCADE");
}

// ── 4. a refusal that arrives after rows are already written ──────────────

#[test]
fn a_rows_written_refusal_names_the_rows_it_already_wrote_and_they_are_pending_until_the_caller_speaks(
) {
    // The contract this pins (QL_CONTRACT §2): a predicated write is NOT a
    // transaction of its own. e4 has ONE transaction per handle and no
    // savepoint, so a statement cannot roll back "its own" rows without
    // discarding the caller's earlier uncommitted statements as well --
    // which is a destructive act the caller did not ask for. So the refusal
    // must SAY that the rows are pending, and the caller's ROLLBACK is the
    // remedy. This test pins both halves: the sentence, and the fact that a
    // later COMMIT does publish them if the caller commits instead.
    let mut f = open();
    let doomed = matching(&f.oracle, 0, 50);
    assert!(doomed.len() > 100);
    let mut budget = QueryBudget::unlimited();
    budget.rows_written = 12;

    let error = f
        .db()
        .sql_with(
            "DELETE FROM rows WHERE amount >= 0 AND amount < 50",
            &[],
            budget,
            &mut || false,
        )
        .unwrap_err();
    let text = format!("{error}");
    assert!(
        text.contains("12") && text.contains("uncommitted") && text.contains("ROLLBACK"),
        "the refusal names the budget, the count, and that the rows are pending: {text}"
    );

    // Half one: the rows ARE pending -- an uncommitted handle still shows
    // them gone to itself, and a snapshot does not.
    let left = f.read_back();
    assert_eq!(
        left.len(),
        f.oracle.len() - 12,
        "the refused statement left its 12 rows written in the transaction"
    );
    {
        let snapshot = Database::open_snapshot(&f.path, cfg()).unwrap();
        assert_eq!(
            snapshot.scan(f.collection, None).unwrap().count(),
            f.oracle.len(),
            "nothing of the refused statement is published"
        );
    }

    // Half two: the caller's word decides. ROLLBACK discards them.
    f.db().sql("ROLLBACK", &[]).unwrap();
    assert_eq!(
        f.read_back(),
        f.oracle,
        "ROLLBACK after a refused predicated write restores the collection"
    );
}

#[test]
fn a_restrict_refusal_raised_after_rows_are_written_says_so_in_the_same_words_the_budget_does() {
    // The refusal a predicated DELETE raises most often after it has already
    // written rows is RESTRICT: the pass writes every row of the page it can
    // and refuses on the row that has edges. That refusal must carry the
    // same sentence the budget refusal carries, or the caller is told the
    // statement was refused while rows are already gone from the
    // transaction.
    let mut f = open();
    let collection = f.collection;
    f.db().enable_graph().unwrap();
    let context = f.db().create_graph_context("routes").unwrap();
    let edge_type = f.db().create_edge_type("near").unwrap();
    let doomed = matching(&f.oracle, 0, 50);
    assert!(doomed.len() > 100);
    // An edge on a row LATE in the driver's order, so the pass writes many
    // rows before it reaches the one that refuses. `amount` drives, so a row
    // whose amount is 49 is near the end of the walk.
    let anchored = f
        .oracle
        .iter()
        .filter(|(_, row)| row["amount"].as_i64().unwrap() == 49)
        .map(|(key, _)| key.clone())
        .next()
        .expect("the fixture has a row at amount 49");
    let hub_key = f.oracle.keys().last().unwrap().clone();
    let source: EntityId = f.db().get(collection, &anchored).unwrap().unwrap().id;
    let hub: EntityId = f.db().get(collection, &hub_key).unwrap().unwrap().id;
    f.db()
        .put_edge(context, source, edge_type, hub, &json!({}))
        .unwrap();
    f.db().commit().unwrap();

    let error = f
        .db()
        .sql("DELETE FROM rows WHERE amount >= 0 AND amount < 50", &[])
        .unwrap_err();
    let text = format!("{error}");
    assert!(
        text.contains("RESTRICT") && text.contains("routes"),
        "the refusal names the mode and the context: {text}"
    );
    // How many rows the refused pass had already written, counted here.
    let written = f.oracle.len() - f.read_back().len();
    assert!(
        written > 0,
        "the probe needs the refusal to arrive AFTER some rows are written"
    );
    let lower = text.to_ascii_lowercase();
    assert!(
        lower.contains("uncommitted") && lower.contains("rollback"),
        "a refusal raised after {written} row(s) were written must say they are pending: {text}"
    );
    assert!(
        text.contains(&written.to_string()),
        "the refusal names how many rows of its own are already written ({written}): {text}"
    );

    f.db().sql("ROLLBACK", &[]).unwrap();
    assert_eq!(f.read_back(), f.oracle, "ROLLBACK restores the collection");
}

#[test]
fn a_refused_predicated_write_inside_a_bulk_scope_leaves_the_counter_where_it_was() {
    let mut f = open();
    f.db().sql("BEGIN BULK", &[]).unwrap();
    assert_eq!(f.db().bulk_depth(), 1);
    // A refusal the statement raises before it writes anything: a SET over
    // the column the driving index is on.
    let error = f
        .db()
        .sql(
            "UPDATE rows SET amount = amount + 1 WHERE amount >= 0 AND amount < 10",
            &[],
        )
        .unwrap_err();
    assert!(
        format!("{error}").contains("by_amount"),
        "the refusal names the index: {error}"
    );
    assert_eq!(
        f.db().bulk_depth(),
        1,
        "a refused statement neither opens nor closes a scope"
    );
    // A refusal the statement raises after it has written: an invalid put.
    let error = f
        .db()
        .sql(
            "INSERT INTO rows (_key, amount, label, score) VALUES ('z1', 1, 'a', 1), ('z2', 'not an int', 'b', 2)",
            &[],
        )
        .unwrap_err();
    assert!(format!("{error}").contains("amount"), "{error}");
    assert_eq!(f.db().bulk_depth(), 1, "the counter is still one");

    // ROLLBACK discards the batch and the scope together, and END BULK after
    // it is refused BY NAME rather than committing somebody else's rows.
    f.db().sql("ROLLBACK", &[]).unwrap();
    assert_eq!(f.db().bulk_depth(), 0);
    let error = f.db().sql("END BULK", &[]).unwrap_err();
    assert!(
        format!("{error}").contains("end_bulk") || format!("{error}").contains("begin_bulk"),
        "the unbalanced close is refused by name: {error}"
    );
    assert_eq!(f.read_back(), f.oracle, "the rollback restored everything");
}

#[test]
fn a_snapshot_opened_inside_a_bulk_scope_sees_nothing_of_the_batch() {
    let mut f = open();
    let doomed = matching(&f.oracle, 0, 20);
    assert!(doomed.len() > 30);
    f.db().sql("BEGIN BULK", &[]).unwrap();
    let n = affected(
        f.db()
            .sql("DELETE FROM rows WHERE amount >= 0 AND amount < 20", &[])
            .unwrap(),
    );
    assert_eq!(n, doomed.len() as u64);
    {
        let snapshot = Database::open_snapshot(&f.path, cfg()).unwrap();
        assert_eq!(
            snapshot.scan(f.collection, None).unwrap().count(),
            f.oracle.len(),
            "a snapshot opened mid-scope sees the collection as it was"
        );
    }
    assert_eq!(
        affected(f.db().sql("END BULK", &[]).unwrap()),
        0,
        "the outermost close commits"
    );
    for key in &doomed {
        f.oracle.remove(key);
    }
    let snapshot = Database::open_snapshot(&f.path, cfg()).unwrap();
    assert_eq!(snapshot.scan(f.collection, None).unwrap().count(), f.oracle.len());
    assert_eq!(f.read_back(), f.oracle);
    f.indexes_are_consistent("after a bulk scope around a predicated delete");
}
