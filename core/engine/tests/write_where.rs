//! `Database::delete_where` and `Database::update_where`: the bounded,
//! resumable write passes of `docs/lang/QL_CONTRACT.md` §2, against a
//! brute-force oracle held in this process.
//!
//! The oracle is never the engine's second reading. Every fixture row is
//! built here from a deterministic generator, kept in a `BTreeMap` beside the
//! database, and the predicate is evaluated over THAT map in plain Rust. What
//! the engine did is read back afterwards and compared against what the map
//! says should be there.
use sekejap_core::collections::{
    verification::{verify_indexed_source, VerificationLimits},
    CandidateDriver, CollectionOptions, Database, DeleteMode, QueryBudget,
    QueryError, QueryFilter, QueryOrder, QueryRequest, Projection, ScalarFilter, ScalarValue,
    UpdatePatch, WorkResource, WriteAction, WriteCursor, WriteRequest,
};
use sekejap_core::Kind;
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ops::Bound;

fn cfg() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn generous() -> QueryBudget {
    QueryBudget::unlimited()
}

/// The fixture: `rows` with an indexed `rank`, an unindexed `label` and a
/// `flag`. `rank` is deliberately NOT a function of the row number, so a
/// range over it is neither a prefix of the key order nor of the id order.
fn rows(n: usize) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for i in 0..n {
        let rank = ((i * 37) % 100) as i64;
        out.insert(
            format!("k{i:04}"),
            json!({"rank": rank, "label": format!("L{}", rank % 7), "flag": i % 3 == 0}),
        );
    }
    out
}

struct Fixture {
    /// `None` only while `indexes_are_consistent` has the writer released:
    /// the current-source reader the verifier opens needs the writer lock
    /// free, so the handle is dropped and rebuilt around it.
    handle: Option<Database>,
    path: std::path::PathBuf,
    collection: sekejap_core::collections::CollectionId,
    rank_index: sekejap_core::collections::IndexId,
    oracle: BTreeMap<String, Value>,
    _temp: tempfile::TempDir,
}

fn fixture(n: usize) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let collection = db
        .create_collection(
            "rows",
            vec![
                ("rank".into(), Kind::Int),
                ("label".into(), Kind::Text),
                ("flag".into(), Kind::Bool),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    let oracle = rows(n);
    for (key, document) in &oracle {
        db.put(collection, key, document).unwrap();
    }
    db.commit().unwrap();
    let rank_index = db
        .create_scalar_index(collection, "by_rank", "rank", false)
        .unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(rank_index, 256).unwrap();
    db.commit().unwrap();
    Fixture {
        handle: Some(db),
        path,
        collection,
        rank_index,
        oracle,
        _temp: temp,
    }
}

impl Fixture {
    fn db(&mut self) -> &mut Database {
        self.handle.as_mut().expect("the fixture holds a handle")
    }

    fn read(&self) -> &Database {
        self.handle.as_ref().expect("the fixture holds a handle")
    }

    /// The collection as the engine now holds it: key -> document, with the
    /// managed `__e4_key` field dropped so it compares against the oracle.
    fn read_back(&self) -> BTreeMap<String, Value> {
        let mut out = BTreeMap::new();
        for entity in self.read().scan(self.collection, None).unwrap() {
            let entity = entity.unwrap();
            let mut document = entity.document.clone();
            let object = document.as_object_mut().unwrap();
            object.retain(|name, _| !name.starts_with("__e4"));
            out.insert(entity.key.clone(), document);
        }
        out
    }

    /// Every index of the file agrees with an independent walk of the rows.
    fn indexes_are_consistent(&mut self, what: &str) {
        self.db().commit().unwrap();
        self.handle = None;
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
        self.handle = Some(Database::open(&self.path, cfg()).unwrap());
    }
}

/// The brute-force answer: every key whose row satisfies `rank` in the
/// half-open range, computed in this process over the oracle map.
fn keys_in_rank_range(oracle: &BTreeMap<String, Value>, lower: i64, upper: i64) -> Vec<String> {
    oracle
        .iter()
        .filter(|(_, document)| {
            let rank = document["rank"].as_i64().unwrap();
            rank >= lower && rank < upper
        })
        .map(|(key, _)| key.clone())
        .collect()
}

fn rank_range(index: sekejap_core::collections::IndexId, lower: i64, upper: i64) -> QueryFilter<'static> {
    QueryFilter::Scalar {
        index,
        predicate: ScalarFilter::Range {
            lower: Bound::Included(ScalarValue::I64(lower)),
            upper: Bound::Excluded(ScalarValue::I64(upper)),
        },
    }
}

#[test]
fn delete_where_removes_exactly_the_rows_a_brute_force_pass_names_and_leaves_the_rest() {
    let mut f = fixture(400);
    let (collection, rank_index) = (f.collection, f.rank_index);
    let doomed = keys_in_rank_range(&f.oracle, 20, 40);
    assert!(
        (40..200).contains(&doomed.len()),
        "the fixture must match a real subset, not nothing and not everything: {} of 400",
        doomed.len()
    );
    let filters = [rank_range(rank_index, 20, 40)];
    let progress = f
        .db()
        .delete_where(collection, &filters, generous())
        .unwrap();
    f.db().commit().unwrap();

    assert!(progress.done, "an unlimited pass finishes in one call");
    assert_eq!(progress.rows_written, doomed.len() as u64);
    assert_eq!(progress.work.rows_written, doomed.len() as u64);

    for key in &doomed {
        f.oracle.remove(key);
    }
    assert_eq!(f.read_back(), f.oracle, "the collection is the oracle");
    f.indexes_are_consistent("after delete_where");
}

#[test]
fn update_where_rewrites_exactly_the_rows_a_brute_force_pass_names() {
    let mut f = fixture(400);
    let (collection, rank_index) = (f.collection, f.rank_index);
    let touched = keys_in_rank_range(&f.oracle, 50, 70);
    assert!(touched.len() > 20, "the fixture must match a real subset");

    // The patch writes `label`, which the driving `by_rank` index is not
    // over, so the walk's key is not moved by the write.
    let mut patch = UpdatePatch::new();
    patch.set("label", json!("patched")).unwrap();
    let filters = [rank_range(rank_index, 50, 70)];
    let progress = f
        .db()
        .update_where(collection, &filters, &patch, generous())
        .unwrap();
    f.db().commit().unwrap();

    assert!(progress.done);
    assert_eq!(progress.rows_written, touched.len() as u64);

    for key in &touched {
        f.oracle.get_mut(key).unwrap()["label"] = json!("patched");
    }
    assert_eq!(f.read_back(), f.oracle);
    f.indexes_are_consistent("after update_where");
}

#[test]
fn a_row_expression_patch_reads_the_row_it_writes_and_never_another() {
    let mut f = fixture(120);
    let (collection, rank_index) = (f.collection, f.rank_index);
    let touched = keys_in_rank_range(&f.oracle, 0, 10);
    // `SET rank = rank + 1000` over the whole row document, applied by core
    // through the closure boundary the language layer compiles into.
    let mut bump = |row: &Value| -> sekejap_core::collections::Result<Value> {
        Ok(json!(row["rank"].as_i64().unwrap() + 1000))
    };
    let mut patch = UpdatePatch::new();
    patch.set_row("rank", &mut bump).unwrap();
    let filters = [rank_range(rank_index, 0, 10)];
    // The patch writes `rank`, which is what `by_rank` is over, so the
    // planner's own driver is refused and the caller takes the entity walk.
    let refused = f
        .db()
        .update_where(collection, &filters, &patch, generous())
        .unwrap_err();
    let text = format!("{refused:?}");
    assert!(
        text.contains("by_rank") && text.contains("CandidateDriver::Entities"),
        "the refusal names the index it would have ridden and the remedy: {text}"
    );

    let progress = f
        .db()
        .write_where(
            WriteRequest {
                collection: collection,
                filters: &filters,
                action: WriteAction::Update(&patch),
                driver: CandidateDriver::Entities,
                after: WriteCursor::start(),
            },
            generous(),
        )
        .unwrap();
    f.db().commit().unwrap();
    assert_eq!(progress.rows_written, touched.len() as u64);

    for key in &touched {
        let row = f.oracle.get_mut(key).unwrap();
        row["rank"] = json!(row["rank"].as_i64().unwrap() + 1000);
    }
    assert_eq!(f.read_back(), f.oracle);
    f.indexes_are_consistent("after a row-expression update");
}

#[test]
fn a_budget_that_stops_mid_way_leaves_a_committed_prefix_and_a_cursor_that_reaches_the_same_end() {
    let mut f = fixture(400);
    let (collection, rank_index) = (f.collection, f.rank_index);
    let doomed = keys_in_rank_range(&f.oracle, 0, 50);
    assert!(doomed.len() > 100);
    let filters = [rank_range(rank_index, 0, 50)];

    let mut budget = generous();
    budget.rows_written = 30;

    // First pass: 30 rows and no more, with a cursor that is not the start.
    let first = f
        .db()
        .delete_where(collection, &filters, budget)
        .unwrap();
    f.db().commit().unwrap();
    assert!(!first.done, "a pass stopped by its budget is not done");
    assert_eq!(first.rows_written, 30);
    assert!(!first.cursor.is_start());

    let after_first = f.read_back();
    assert_eq!(
        after_first.len(),
        f.oracle.len() - 30,
        "the committed prefix is exactly the rows the budget allowed"
    );
    // Every row the prefix removed was a MATCHING row: a bounded pass never
    // touches a row the predicate does not name.
    for key in f.oracle.keys() {
        if !after_first.contains_key(key) {
            assert!(doomed.contains(key), "`{key}` was deleted but never matched");
        }
    }
    f.indexes_are_consistent("after a budgeted prefix");

    // Resume from the cursor until it is done, and check the final state is
    // the one the unbudgeted pass would have produced.
    let mut cursor = first.cursor.clone();
    let mut total = first.rows_written;
    let mut calls = 1;
    loop {
        let next = f
            .db()
            .delete_where_after(collection, &filters, budget, &cursor)
            .unwrap();
        f.db().commit().unwrap();
        total += next.rows_written;
        cursor = next.cursor.clone();
        calls += 1;
        assert!(calls < 100, "the resume is not converging");
        if next.done {
            break;
        }
    }
    assert_eq!(total, doomed.len() as u64);
    for key in &doomed {
        f.oracle.remove(key);
    }
    assert_eq!(f.read_back(), f.oracle, "the resumed pass reaches the same end");
    f.indexes_are_consistent("after the resume");
}

#[test]
fn a_restrict_pass_refuses_a_row_with_edges_naming_the_context_and_changes_nothing() {
    let mut f = fixture(60);
    let (collection, rank_index) = (f.collection, f.rank_index);
    f.db().enable_graph().unwrap();
    let context = f.db().create_graph_context("routes").unwrap();
    let edge_type = f.db().create_edge_type("near").unwrap();
    // One edge, on a row the predicate names.
    let doomed = keys_in_rank_range(&f.oracle, 0, 20);
    let anchored = doomed.last().unwrap().clone();
    let source = f.db().get(collection, &anchored).unwrap().unwrap().id;
    let other = f.db().get(collection, "k0001").unwrap().unwrap().id;
    f.db().put_edge(context, source, edge_type, other, &json!({}))
        .unwrap();
    f.db().commit().unwrap();
    let before = f.read_back();

    let filters = [rank_range(rank_index, 0, 20)];
    let refused = f
        .db()
        .delete_where(collection, &filters, generous())
        .unwrap_err();
    let text = format!("{refused:?}");
    assert!(
        text.contains("RESTRICT") && text.contains("routes"),
        "the refusal names the mode and the context: {text}"
    );
    f.db().rollback().unwrap();
    assert_eq!(f.read_back(), before, "a refused pass changes nothing");

    // CASCADE is the explicit opt-in, and it removes the edge with the row.
    let progress = f
        .db()
        .write_where(
            WriteRequest {
                collection: collection,
                filters: &filters,
                action: WriteAction::Delete(DeleteMode::Cascade),
                driver: CandidateDriver::Auto,
                after: WriteCursor::start(),
            },
            generous(),
        )
        .unwrap();
    f.db().commit().unwrap();
    assert_eq!(progress.rows_written, doomed.len() as u64);
    for key in &doomed {
        f.oracle.remove(key);
    }
    assert_eq!(f.read_back(), f.oracle);
    assert!(
        f.db().neighbors(sekejap_core::collections::NeighborRequest {
            entity: other,
            direction: sekejap_core::collections::Direction::Incoming,
            context,
            edge_type: Some(edge_type),
            limit: 8,
        })
        .unwrap()
        .is_empty(),
        "CASCADE removed the incident edge with the row"
    );
    f.indexes_are_consistent("after a CASCADE pass");
}

#[test]
fn law_two_the_work_of_a_write_pass_is_the_rows_matched_and_not_the_collection() {
    // Two collections of the same shape, one six times the other, with the
    // SAME number of matching rows. If the work were proportional to the
    // collection the second would cost six times the first.
    let small = 200;
    let large = 1_200;
    let mut costs = Vec::new();
    for n in [small, large] {
        let mut f = fixture(n);
        let (collection, rank_index) = (f.collection, f.rank_index);
        // `rank == 7` names one value, and the generator makes the count of
        // rows holding it proportional to n -- so pick a range the oracle
        // says holds the same count in both, by taking a fixed key prefix
        // instead: `rank` in [0, 4) over the first 120 rows only.
        let filters = [rank_range(rank_index, 0, 4)];
        let matched = keys_in_rank_range(&f.oracle, 0, 4).len() as u64;
        let progress = f
            .db()
            .delete_where(collection, &filters, generous())
            .unwrap();
        f.db().commit().unwrap();
        assert_eq!(progress.rows_written, matched);
        costs.push((n as u64, matched, progress.work.candidates));
    }
    let (_, small_matched, small_candidates) = costs[0];
    let (_, large_matched, large_candidates) = costs[1];
    // The candidate count tracks the rows matched, not the collection: the
    // ratio of candidates is the ratio of matches, within one candidate of
    // the range-boundary probe each walk pays.
    let ratio_rows = large_matched as f64 / small_matched as f64;
    let ratio_work = large_candidates as f64 / small_candidates as f64;
    assert!(
        (ratio_work - ratio_rows).abs() < 0.2,
        "candidates/rows: {small_candidates}/{small_matched} then {large_candidates}/{large_matched}; \
         collection grew {}x while work grew {ratio_work:.2}x against {ratio_rows:.2}x of matches",
        large as f64 / small as f64
    );
    assert!(
        large_candidates < large as u64 / 2,
        "a write pass over {large_matched} matches read {large_candidates} candidates of a {large}-row collection: that is a scan"
    );
}

#[test]
fn law_six_a_snapshot_opened_before_the_update_still_sees_the_old_rows() {
    let mut f = fixture(120);
    let (collection, rank_index) = (f.collection, f.rank_index);
    f.db().commit().unwrap();
    // A reader opened BEFORE the write, on the same file.
    let snapshot = Database::open_snapshot(&f.path, cfg()).unwrap();
    let watched = keys_in_rank_range(&f.oracle, 50, 70)[0].clone();
    let before = snapshot
        .get(collection, &watched)
        .unwrap()
        .unwrap()
        .document
        .clone();

    let mut patch = UpdatePatch::new();
    patch.set("label", json!("after")).unwrap();
    let filters = [rank_range(rank_index, 50, 70)];
    f.db().update_where(collection, &filters, &patch, generous())
        .unwrap();
    f.db().commit().unwrap();

    let after = snapshot
        .get(collection, &watched)
        .unwrap()
        .unwrap()
        .document
        .clone();
    assert_eq!(
        before, after,
        "the snapshot opened before the pass reads what it read then"
    );
    assert_eq!(
        f.db().get(collection, &watched)
            .unwrap()
            .unwrap()
            .document["label"],
        json!("after"),
        "the writer's own handle sees the new row"
    );
}

#[test]
fn law_three_a_write_pass_commits_nothing_of_its_own_and_a_rollback_discards_all_of_it() {
    let mut f = fixture(120);
    let (collection, rank_index) = (f.collection, f.rank_index);
    let before = f.read_back();
    let filters = [rank_range(rank_index, 0, 30)];
    let progress = f
        .db()
        .delete_where(collection, &filters, generous())
        .unwrap();
    assert!(progress.rows_written > 0);
    // Not committed: the caller's transaction owns it.
    f.db().rollback().unwrap();
    assert_eq!(
        f.read_back(),
        before,
        "a rolled-back pass leaves the collection as it was"
    );
    f.indexes_are_consistent("after a rolled-back pass");
}

#[test]
fn a_pass_over_a_predicate_nothing_matches_writes_nothing_and_reports_done() {
    let mut f = fixture(60);
    let (collection, rank_index) = (f.collection, f.rank_index);
    let filters = [rank_range(rank_index, 1_000, 2_000)];
    let progress = f
        .db()
        .delete_where(collection, &filters, generous())
        .unwrap();
    assert!(progress.done);
    assert_eq!(progress.rows_written, 0);
    assert!(progress.cursor.is_start());
    assert_eq!(f.read_back(), f.oracle);
}

#[test]
fn a_zero_rows_written_budget_writes_nothing_and_says_it_is_not_done() {
    let mut f = fixture(60);
    let (collection, rank_index) = (f.collection, f.rank_index);
    let mut budget = generous();
    budget.rows_written = 0;
    let filters = [rank_range(rank_index, 0, 50)];
    let progress = f
        .db()
        .delete_where(collection, &filters, budget)
        .unwrap();
    assert!(!progress.done);
    assert_eq!(progress.rows_written, 0);
    assert_eq!(f.read_back(), f.oracle);
}

#[test]
fn a_patch_that_names_a_managed_field_is_refused_before_a_row_is_read() {
    let mut patch = UpdatePatch::new();
    for name in ["_id", "_key", "_collection", "__e4_key"] {
        let error = patch.set(name, json!(1)).unwrap_err();
        assert!(
            format!("{error}").contains(name),
            "the refusal names the field: {error}"
        );
    }
    patch.set("label", json!("x")).unwrap();
    let twice = patch.set("label", json!("y")).unwrap_err();
    assert!(format!("{twice}").contains("twice"));
}

#[test]
fn the_candidate_walk_of_a_write_pass_is_the_same_walk_the_same_select_takes() {
    // A `DELETE ... WHERE` must not plan its own way to the rows: the driver
    // it reports is the driver the identical SELECT reports.
    let mut f = fixture(300);
    let (collection, rank_index) = (f.collection, f.rank_index);
    let filters = [rank_range(rank_index, 10, 30)];
    let select_driver = {
        let mut prepared = f
            .db()
            .prepare_query(QueryRequest {
                collection: collection,
                filters: &filters,
                order: QueryOrder::Driver,
                projection: Projection::Ids,
                total_limit: None,
                driver: CandidateDriver::Auto,
            })
            .unwrap();
        prepared.next_page(64, generous(), || false).unwrap().driver
    };
    let progress = f
        .db()
        .delete_where(collection, &filters, generous())
        .unwrap();
    assert_eq!(progress.driver, select_driver);
}

#[test]
fn the_rows_written_budget_is_a_work_resource_a_caller_can_name() {
    // The dimension is additive on `QueryBudget` and reported on `QueryWork`;
    // a refusal elsewhere in the budget still names its own resource.
    let mut f = fixture(120);
    let (collection, rank_index) = (f.collection, f.rank_index);
    let mut budget = generous();
    budget.candidates = 3;
    let filters = [rank_range(rank_index, 0, 90)];
    let error = f
        .db()
        .delete_where(collection, &filters, budget)
        .unwrap_err();
    match error {
        QueryError::BudgetExceeded { resource, .. } => {
            assert_eq!(resource, WorkResource::Candidates);
        }
        other => panic!("expected a candidate budget refusal, got {other:?}"),
    }
}

// ── the bulk scope (docs/dist/OPS_CONTRACT.md §7) ─────────────────────────

#[test]
fn a_bulk_scope_nests_and_only_the_outermost_close_commits() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let collection = db
        .create_collection(
            "rows",
            vec![("rank".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();

    db.begin_bulk().unwrap();
    assert_eq!(db.bulk_depth(), 1);
    db.put(collection, "a", &json!({"rank": 1})).unwrap();
    db.begin_bulk().unwrap();
    assert_eq!(db.bulk_depth(), 2);
    db.put(collection, "b", &json!({"rank": 2})).unwrap();

    // The inner close commits nothing: a reader opened now sees neither row.
    assert!(!db.end_bulk().unwrap(), "an inner close does not commit");
    assert_eq!(db.bulk_depth(), 1);
    {
        let snapshot = Database::open_snapshot(&path, cfg()).unwrap();
        assert!(snapshot.get(collection, "a").unwrap().is_none());
        assert!(snapshot.get(collection, "b").unwrap().is_none());
    }

    assert!(db.end_bulk().unwrap(), "the outermost close commits");
    assert_eq!(db.bulk_depth(), 0);
    let snapshot = Database::open_snapshot(&path, cfg()).unwrap();
    assert!(snapshot.get(collection, "a").unwrap().is_some());
    assert!(snapshot.get(collection, "b").unwrap().is_some());
}

#[test]
fn an_unbalanced_close_is_refused_and_a_rollback_forgets_the_scope() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let collection = db
        .create_collection(
            "rows",
            vec![("rank".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();
    assert!(db.end_bulk().is_err(), "a close outside a scope is refused");

    db.begin_bulk().unwrap();
    db.put(collection, "a", &json!({"rank": 1})).unwrap();
    // A failed batch leaves nothing committed, and the scope goes with it.
    db.rollback().unwrap();
    assert_eq!(db.bulk_depth(), 0);
    assert!(db.get(collection, "a").unwrap().is_none());
    assert!(db.end_bulk().is_err());
}

#[test]
fn a_bulk_scope_commits_with_the_same_durability_as_any_other_commit() {
    // §7: there is no weakened barrier to ask for. The scope's commit is
    // `Database::commit`, so the rows survive a reopen exactly as any
    // committed rows do.
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let collection = {
        let mut db = Database::create(&path, cfg()).unwrap();
        let collection = db
            .create_collection(
                "rows",
                vec![("rank".into(), Kind::Int)],
                CollectionOptions::default(),
            )
            .unwrap();
        db.commit().unwrap();
        db.begin_bulk().unwrap();
        for i in 0..500 {
            db.put(collection, &format!("k{i:04}"), &json!({"rank": i}))
                .unwrap();
        }
        assert!(db.end_bulk().unwrap());
        collection
    };
    let db = Database::open(&path, cfg()).unwrap();
    assert_eq!(db.scan(collection, None).unwrap().count(), 500);
}
