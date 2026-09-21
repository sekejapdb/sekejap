//! An ADVERSARIAL review of the bounded, resumable write passes
//! (`core/engine/src/collections/write_set.rs`).
//!
//! Every probe here computes its answer BRUTE FORCE in this process, over a
//! `BTreeMap` fixture the test itself built, and compares the engine's file
//! against that. Nothing is ever compared against a second reading by the
//! engine.
//!
//! The risks probed, one section each:
//!
//!   1. resume across EVERY driver kind the pass accepts, with rows deleted
//!      and inserted between two calls;
//!   2. Halloween -- a write that re-files an index the walk is standing on,
//!      or an index the walk is not standing on;
//!   3. the per-row RESTRICT probe cap;
//!   4. the work a sparse predicate charges.
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::{
    verification::{verify_indexed_source, VerificationLimits},
    BfsRequest, CandidateDriver, CollectionId, CollectionOptions, Database, DeleteMode, Direction,
    EntityId,
    IndexId, PointFilter, Projection, QueryBudget, QueryDriver, QueryFilter,
    QueryOrder, QueryRequest, Result as CoreResult, ScalarFilter, ScalarValue, TextMatch,
    UpdatePatch, WriteAction, WriteCursor, WriteRequest, RESTRICT_ROW_PROBE_SEEKS,
};
use sekejap_core::spatial_math::Point;
use sekejap_core::Kind;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

fn cfg() -> Config {
    Config {
        budget_bytes: 16 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn generous() -> QueryBudget {
    QueryBudget::unlimited()
}

/// The centre every "near" row is scattered around.
const CENTRE: (f64, f64) = (144.96, -37.81);

/// The fixture row generator, deterministic and held in this process.
///
/// `rank` is not a function of the row number, so a range over it is a prefix
/// of neither the key order nor the id order. `band` is a SECOND indexed
/// column, so a patch can move an index the walk is NOT standing on. `label`
/// carries the text terms. `near` decides whether the row has a point inside
/// the probe radius.
fn rows(n: usize) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for i in 0..n {
        let rank = ((i * 37) % 100) as i64;
        let band = (i % 10) as i64;
        let near = i % 3 == 0;
        let (lon, lat) = if near {
            (
                CENTRE.0 + (i % 7) as f64 * 0.0005,
                CENTRE.1 + (i % 5) as f64 * 0.0005,
            )
        } else {
            (150.0 + (i % 11) as f64 * 0.01, -30.0 - (i % 13) as f64 * 0.01)
        };
        out.insert(
            format!("k{i:04}"),
            json!({
                "rank": rank,
                "band": band,
                "label": format!("term{} common", rank % 7),
                "loc": {"type": "Point", "coordinates": [lon, lat]},
            }),
        );
    }
    out
}

struct Fixture {
    handle: Option<Database>,
    path: std::path::PathBuf,
    collection: CollectionId,
    by_rank: IndexId,
    by_band: IndexId,
    by_label: IndexId,
    by_loc: IndexId,
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
                ("band".into(), Kind::Int),
                ("label".into(), Kind::Text),
                ("loc".into(), Kind::Point),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    let oracle = rows(n);
    for (key, document) in &oracle {
        db.put(collection, key, document).unwrap();
    }
    db.commit().unwrap();
    let by_rank = db
        .create_scalar_index(collection, "by_rank", "rank", false)
        .unwrap();
    let by_band = db
        .create_scalar_index(collection, "by_band", "band", false)
        .unwrap();
    let by_label = db.create_text_index(collection, "by_label", "label").unwrap();
    let by_loc = db.create_point_index(collection, "by_loc", "loc").unwrap();
    db.commit().unwrap();
    for index in [by_rank, by_band, by_label, by_loc] {
        db.build_index_to_ready(index, 256).unwrap();
    }
    db.commit().unwrap();
    Fixture {
        handle: Some(db),
        path,
        collection,
        by_rank,
        by_band,
        by_label,
        by_loc,
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

    /// The collection as the file now holds it, managed fields dropped.
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

fn rank_range(index: IndexId, lower: i64, upper: i64) -> QueryFilter<'static> {
    QueryFilter::Scalar {
        index,
        predicate: ScalarFilter::Range {
            lower: Bound::Included(ScalarValue::I64(lower)),
            upper: Bound::Excluded(ScalarValue::I64(upper)),
        },
    }
}

fn band_eq(index: IndexId, value: i64) -> QueryFilter<'static> {
    QueryFilter::Scalar {
        index,
        predicate: ScalarFilter::Eq(ScalarValue::I64(value)),
    }
}

/// The keys a predicate names, computed here in plain Rust.
fn brute<F: Fn(&Value) -> bool>(oracle: &BTreeMap<String, Value>, keep: F) -> BTreeSet<String> {
    oracle
        .iter()
        .filter(|(_, document)| keep(document))
        .map(|(key, _)| key.clone())
        .collect()
}

// ── 1. resume, over every driver kind the pass accepts ────────────────────

/// One case of the resume battery: a name, the filters, the driver the pass
/// is to ride, and the brute-force set of keys the predicate names.
struct Case {
    name: &'static str,
    driver: CandidateDriver,
    expect: QueryDriver,
    /// The graph case deletes the very rows its traversal reaches over, so
    /// it is the one case that asks for CASCADE; every other case is the
    /// RESTRICT default.
    mode: DeleteMode,
    doomed: BTreeSet<String>,
}

#[test]
fn a_budget_that_stops_mid_way_resumes_to_the_brute_force_end_state_over_every_driver_kind() {
    // Each driver kind gets its own fixture, because the pass it runs deletes
    // the rows the next one would have matched.
    for which in 0..7 {
        let mut f = fixture(300);
        let (collection, by_rank, by_band, by_label, by_loc) =
            (f.collection, f.by_rank, f.by_band, f.by_label, f.by_loc);
        let centre = Point::new(CENTRE.0, CENTRE.1).unwrap();

        // The graph case needs a context, a type and edges before the walk.
        let mut graph_reach: BTreeSet<String> = BTreeSet::new();
        if which == 6 {
            f.db().enable_graph().unwrap();
            let context = f.db().create_graph_context("routes").unwrap();
            let edge_type = f.db().create_edge_type("near").unwrap();
            let keys: Vec<String> = f.oracle.keys().cloned().collect();
            let seed: EntityId = f.db().get(collection, &keys[0]).unwrap().unwrap().id;
            // A star: the seed reaches every fifth row at depth one.
            for key in keys.iter().skip(1).step_by(5) {
                let far = f.db().get(collection, key).unwrap().unwrap().id;
                f.db()
                    .put_edge(context, seed, edge_type, far, &json!({}))
                    .unwrap();
                graph_reach.insert(key.clone());
            }
            f.db().commit().unwrap();
            let request = BfsRequest {
                seed,
                direction: Direction::Outgoing,
                context,
                edge_type: Some(edge_type),
                min_depth: 1,
                max_depth: 1,
                include_seed: false,
                max_visited: 4_096,
                max_edges: 4_096,
                result_limit: 4_096,
                edge_where: &[],
                node_where: &[],
            };
            let filters = [QueryFilter::Graph(request)];
            let case = Case {
                name: "graph traversal",
                driver: CandidateDriver::Auto,
                expect: QueryDriver::Graph { filter: 0 },
                mode: DeleteMode::Cascade,
                doomed: graph_reach.clone(),
            };
            run_resume_case(&mut f, &filters, case);
            continue;
        }

        let key_lower = "k0050".to_owned();
        let key_upper = "k0150".to_owned();
        let near_keys = brute(&f.oracle, |row| {
            let coordinates = row["loc"]["coordinates"].as_array().unwrap();
            let lon = coordinates[0].as_f64().unwrap();
            lon < 149.0
        });
        let (filters, case): (Vec<QueryFilter<'_>>, Case) = match which {
            0 => (
                vec![rank_range(by_rank, 0, 50)],
                Case {
                    name: "scalar range",
                    mode: DeleteMode::Restrict,
                    driver: CandidateDriver::Auto,
                    expect: QueryDriver::Scalar(by_rank),
                    doomed: brute(&f.oracle, |row| row["rank"].as_i64().unwrap() < 50),
                },
            ),
            1 => (
                vec![band_eq(by_band, 3)],
                Case {
                    name: "entity cursor",
                    mode: DeleteMode::Restrict,
                    driver: CandidateDriver::Entities,
                    expect: QueryDriver::Entities,
                    doomed: brute(&f.oracle, |row| row["band"].as_i64().unwrap() == 3),
                },
            ),
            2 => (
                vec![QueryFilter::Key {
                    lower: Bound::Included(Box::leak(key_lower.clone().into_boxed_str())),
                    upper: Bound::Excluded(Box::leak(key_upper.clone().into_boxed_str())),
                }],
                Case {
                    name: "external-key walk",
                    mode: DeleteMode::Restrict,
                    driver: CandidateDriver::Keys,
                    expect: QueryDriver::Keys,
                    doomed: f
                        .oracle
                        .keys()
                        .filter(|key| **key >= key_lower && **key < key_upper)
                        .cloned()
                        .collect(),
                },
            ),
            3 => (
                vec![QueryFilter::Text {
                    index: by_label,
                    query: "term3",
                    matching: TextMatch::All,
                }],
                Case {
                    name: "text merge",
                    mode: DeleteMode::Restrict,
                    driver: CandidateDriver::Auto,
                    expect: QueryDriver::Text(by_label),
                    doomed: brute(&f.oracle, |row| {
                        row["label"].as_str().unwrap().starts_with("term3 ")
                    }),
                },
            ),
            4 => (
                vec![QueryFilter::Any(Box::leak(Box::new([
                    band_eq(by_band, 1),
                    band_eq(by_band, 4),
                ])))],
                Case {
                    name: "membership union (boolean OR)",
                    mode: DeleteMode::Restrict,
                    driver: CandidateDriver::Auto,
                    expect: QueryDriver::Membership { filter: 0 },
                    doomed: brute(&f.oracle, |row| {
                        matches!(row["band"].as_i64().unwrap(), 1 | 4)
                    }),
                },
            ),
            5 => (
                vec![QueryFilter::Point {
                    index: by_loc,
                    predicate: PointFilter::Radius {
                        center: centre,
                        radius_metres: 2_000.0,
                    },
                }],
                Case {
                    name: "spatial radius",
                    mode: DeleteMode::Restrict,
                    driver: CandidateDriver::Auto,
                    expect: QueryDriver::Spatial {
                        index: by_loc,
                        fallback_world: false,
                    },
                    doomed: near_keys,
                },
            ),
            _ => unreachable!(),
        };
        run_resume_case(&mut f, &filters, case);
    }
}

/// One driver kind's resume: a budget of seven rows at a time, resumed to the
/// end, against the brute-force set.
fn run_resume_case(f: &mut Fixture, filters: &[QueryFilter<'_>], case: Case) {
    let collection = f.collection;
    assert!(
        case.doomed.len() >= 30,
        "{}: the fixture must name a real subset, not {} rows",
        case.name,
        case.doomed.len()
    );
    let mut budget = generous();
    budget.rows_written = 7;

    let mut cursor = WriteCursor::start();
    let mut total = 0u64;
    let mut calls = 0;
    // Every key the passes actually removed, so "no row twice, no row
    // skipped" is a statement about a set and not only about a count.
    let mut before = f.read_back();
    loop {
        let progress = f
            .db()
            .write_where(
                WriteRequest {
                    collection,
                    filters,
                    action: WriteAction::Delete(case.mode),
                    driver: case.driver,
                    after: cursor.clone(),
                },
                budget,
            )
            .unwrap_or_else(|e| panic!("{}: {e:?}", case.name));
        f.db().commit().unwrap();
        assert_eq!(
            progress.driver, case.expect,
            "{}: the pass rode the wrong driver",
            case.name
        );
        assert!(
            progress.rows_written <= 7,
            "{}: a pass wrote past its budget",
            case.name
        );
        let after = f.read_back();
        // No row the predicate does not name was touched, and no row was
        // written twice: every removal this call made is a doomed key that
        // was still present.
        for key in before.keys() {
            if !after.contains_key(key) {
                assert!(
                    case.doomed.contains(key),
                    "{}: `{key}` was deleted but the brute-force pass never named it",
                    case.name
                );
            }
        }
        assert_eq!(
            before.len() - after.len(),
            progress.rows_written as usize,
            "{}: rows_written does not match the rows that left the file",
            case.name
        );
        before = after;
        total += progress.rows_written;
        cursor = progress.cursor.clone();
        calls += 1;
        assert!(calls < 400, "{}: the resume is not converging", case.name);
        if progress.done {
            break;
        }
    }
    assert_eq!(
        total,
        case.doomed.len() as u64,
        "{}: the resumed passes wrote a different number of rows than the oracle names",
        case.name
    );
    for key in &case.doomed {
        f.oracle.remove(key);
    }
    assert_eq!(
        f.read_back(),
        f.oracle,
        "{}: the resumed pass did not reach the brute-force end state",
        case.name
    );
    f.indexes_are_consistent(case.name);
}

#[test]
fn a_row_deleted_between_two_calls_of_a_resumed_pass_is_neither_counted_nor_missed() {
    let mut f = fixture(300);
    let (collection, by_rank) = (f.collection, f.by_rank);
    let doomed = brute(&f.oracle, |row| row["rank"].as_i64().unwrap() < 50);
    let filters = [rank_range(by_rank, 0, 50)];
    let mut budget = generous();
    budget.rows_written = 20;

    let first = f
        .db()
        .delete_where(collection, &filters, budget)
        .unwrap();
    f.db().commit().unwrap();
    assert_eq!(first.rows_written, 20);
    assert!(!first.done);

    // Between the two calls a third party removes matching rows that the
    // pass has NOT reached yet -- the ones with the highest ranks in range.
    let survivors: Vec<String> = doomed
        .iter()
        .filter(|key| f.read().get(collection, key).unwrap().is_some())
        .cloned()
        .collect();
    let removed: Vec<String> = survivors
        .iter()
        .rev()
        .take(5)
        .cloned()
        .collect();
    for key in &removed {
        assert!(f.db().delete(collection, key).unwrap());
    }
    f.db().commit().unwrap();

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
    // The pass wrote every doomed row EXCEPT the ones that had already gone.
    assert!(
        total <= doomed.len() as u64 && total >= (doomed.len() - removed.len()) as u64,
        "a row removed behind the pass must not be counted as written: {total} of {}",
        doomed.len()
    );
    for key in &doomed {
        f.oracle.remove(key);
    }
    assert_eq!(f.read_back(), f.oracle, "the end state is the oracle's");
    f.indexes_are_consistent("after a concurrent delete");
}

#[test]
fn a_row_inserted_between_two_calls_is_swept_when_it_lands_after_the_cursor_and_named_when_it_does_not(
) {
    let mut f = fixture(300);
    let (collection, by_rank) = (f.collection, f.by_rank);
    let filters = [rank_range(by_rank, 0, 50)];
    let mut budget = generous();
    budget.rows_written = 20;

    let first = f
        .db()
        .delete_where(collection, &filters, budget)
        .unwrap();
    f.db().commit().unwrap();
    assert!(!first.done);

    // The driver is `by_rank`, so the cursor is a (rank, sequence) pair and
    // "before" / "after" it are RANK statements. The pass has written the 20
    // lowest ranks in range, so rank 0 is behind the cursor and rank 49 is
    // ahead of it.
    let behind = json!({
        "rank": 0, "band": 0, "label": "term0 common",
        "loc": {"type": "Point", "coordinates": [150.0, -30.0]},
    });
    let ahead = json!({
        "rank": 49, "band": 0, "label": "term0 common",
        "loc": {"type": "Point", "coordinates": [150.0, -30.0]},
    });
    f.db().put(collection, "z-behind", &behind).unwrap();
    f.db().put(collection, "z-ahead", &ahead).unwrap();
    f.db().commit().unwrap();

    let mut cursor = first.cursor.clone();
    let mut calls = 1;
    loop {
        let next = f
            .db()
            .delete_where_after(collection, &filters, budget, &cursor)
            .unwrap();
        f.db().commit().unwrap();
        cursor = next.cursor.clone();
        calls += 1;
        assert!(calls < 100, "the resume is not converging");
        if next.done {
            break;
        }
    }
    let left = f.read_back();
    // The contract this pins: a resumed pass sweeps forward from its cursor.
    // A row inserted AHEAD of the cursor is swept with the rest; one inserted
    // BEHIND it is not, and is still there for the caller to see.
    assert!(
        !left.contains_key("z-ahead"),
        "a matching row inserted ahead of the cursor is swept by the resume"
    );
    assert!(
        left.contains_key("z-behind"),
        "a matching row inserted BEHIND the cursor is not swept: the cursor is a resume point, not a re-scan"
    );
    for key in brute(&f.oracle, |row| row["rank"].as_i64().unwrap() < 50) {
        f.oracle.remove(&key);
    }
    f.oracle.insert("z-behind".into(), behind);
    assert_eq!(f.read_back(), f.oracle);
    f.indexes_are_consistent("after a concurrent insert");
}

// ── 2. Halloween ──────────────────────────────────────────────────────────

#[test]
fn an_update_that_moves_a_non_driving_index_refiles_its_posting_and_leaves_the_verifier_clean() {
    let mut f = fixture(300);
    let (collection, by_rank, by_band) = (f.collection, f.by_rank, f.by_band);
    let touched = brute(&f.oracle, |row| {
        let rank = row["rank"].as_i64().unwrap();
        (20..40).contains(&rank)
    });
    assert!(touched.len() > 30);
    // The walk rides `by_rank`; the patch moves `by_band`, a DIFFERENT
    // index -- the posting the walk is standing on is not touched, the other
    // one must be re-filed.
    let mut patch = UpdatePatch::new();
    patch.set("band", json!(99)).unwrap();
    let filters = [rank_range(by_rank, 20, 40)];
    let progress = f
        .db()
        .update_where(collection, &filters, &patch, generous())
        .unwrap();
    f.db().commit().unwrap();
    assert_eq!(progress.driver, QueryDriver::Scalar(by_rank));
    assert_eq!(progress.rows_written, touched.len() as u64);

    for key in &touched {
        f.oracle.get_mut(key).unwrap()["band"] = json!(99);
    }
    assert_eq!(f.read_back(), f.oracle);

    // The NEW posting is there: every patched row answers `band = 99`.
    let found = keys_matching(f.read(), collection, &[band_eq(by_band, 99)]);
    assert_eq!(found, touched, "the new posting is in the index");
    // The OLD postings are gone: no patched row answers its old band any
    // more. The brute-force answer to `band = b` is what the oracle says.
    for band in 0..10 {
        let expect = brute(&f.oracle, |row| row["band"].as_i64().unwrap() == band);
        let found = keys_matching(f.read(), collection, &[band_eq(by_band, band)]);
        assert_eq!(found, expect, "`band = {band}` is the oracle's answer");
    }
    f.indexes_are_consistent("after an update that moved a non-driving index");
}

#[test]
fn an_update_that_rewrites_the_text_a_text_driver_walks_writes_each_row_exactly_once() {
    let mut f = fixture(300);
    let (collection, by_label) = (f.collection, f.by_label);
    let touched = brute(&f.oracle, |row| {
        row["label"].as_str().unwrap().starts_with("term3 ")
    });
    assert!(touched.len() > 30);
    // `SET label = label || ' term9'`. The text driver walks `by_label`, and
    // this patch REWRITES that index for every row it touches: the old
    // postings go, a new term arrives. A row written twice ends with two
    // ` term9` suffixes, which the oracle comparison catches exactly.
    let mut append = |row: &Value| -> CoreResult<Value> {
        Ok(json!(format!("{} term9", row["label"].as_str().unwrap())))
    };
    let mut patch = UpdatePatch::new();
    patch.set_row("label", &mut append).unwrap();
    let filters = [QueryFilter::Text {
        index: by_label,
        query: "term3",
        matching: TextMatch::All,
    }];
    let progress = f
        .db()
        .update_where(collection, &filters, &patch, generous())
        .unwrap();
    f.db().commit().unwrap();
    assert_eq!(
        progress.driver,
        QueryDriver::Text(by_label),
        "the pass rides the text merge"
    );
    assert_eq!(
        progress.rows_written,
        touched.len() as u64,
        "a text-driven UPDATE that rewrites its own index writes each row once"
    );
    for key in &touched {
        let row = f.oracle.get_mut(key).unwrap();
        row["label"] = json!(format!("{} term9", row["label"].as_str().unwrap()));
    }
    assert_eq!(f.read_back(), f.oracle, "no row was written twice");
    f.indexes_are_consistent("after a text-driven rewrite of the text field");
}

#[test]
fn an_update_over_a_membership_driver_that_moves_one_leafs_index_writes_each_row_exactly_once() {
    let mut f = fixture(300);
    let (collection, by_band) = (f.collection, f.by_band);
    let touched = brute(&f.oracle, |row| {
        matches!(row["band"].as_i64().unwrap(), 1 | 4)
    });
    assert!(touched.len() > 30);
    // The union of `band = 1` and `band = 4` drives; the patch moves `band`
    // to 4 for the `band = 1` half, so one leaf's postings move INTO the
    // other leaf while the walk is standing on the union.
    let mut to_four = |row: &Value| -> CoreResult<Value> {
        let band = row["band"].as_i64().unwrap();
        Ok(json!(if band == 1 { 4 } else { 7 }))
    };
    let mut patch = UpdatePatch::new();
    patch.set_row("band", &mut to_four).unwrap();
    let leaves = [band_eq(by_band, 1), band_eq(by_band, 4)];
    let filters = [QueryFilter::Any(&leaves)];
    let progress = f
        .db()
        .update_where(collection, &filters, &patch, generous())
        .unwrap();
    f.db().commit().unwrap();
    assert_eq!(
        progress.driver,
        QueryDriver::Membership { filter: 0 },
        "the union drives"
    );
    assert_eq!(
        progress.rows_written,
        touched.len() as u64,
        "a membership-driven UPDATE that moves a leaf's postings writes each row once"
    );
    for key in &touched {
        let row = f.oracle.get_mut(key).unwrap();
        let band = row["band"].as_i64().unwrap();
        row["band"] = json!(if band == 1 { 4 } else { 7 });
    }
    assert_eq!(f.read_back(), f.oracle, "no row was written twice");
    f.indexes_are_consistent("after a membership-driven leaf move");
}

#[test]
fn a_patch_that_writes_the_driving_scalar_field_is_refused_even_when_the_value_does_not_change() {
    let mut f = fixture(120);
    let (collection, by_rank) = (f.collection, f.by_rank);
    // `SET rank = rank` is a no-op per row, and it is REFUSED all the same:
    // the preflight is over the COLUMNS a patch writes, not over the values
    // it would produce, because the values are only known one row at a time
    // and a preflight that asked per row would already be inside the walk.
    // Stated here so the choice is pinned rather than incidental.
    let mut same = |row: &Value| -> CoreResult<Value> { Ok(row["rank"].clone()) };
    let mut patch = UpdatePatch::new();
    patch.set_row("rank", &mut same).unwrap();
    let filters = [rank_range(by_rank, 0, 10)];
    let refused = f
        .db()
        .update_where(collection, &filters, &patch, generous())
        .unwrap_err();
    let text = format!("{refused:?}");
    assert!(
        text.contains("by_rank") && text.contains("CandidateDriver::Entities"),
        "the refusal names the index and the remedy: {text}"
    );

    // A literal that happens to be the value already stored is refused for
    // the same reason and by the same text.
    let mut literal = UpdatePatch::new();
    literal.set("rank", json!(0)).unwrap();
    let refused = f
        .db()
        .update_where(collection, &filters, &literal, generous())
        .unwrap_err();
    assert!(format!("{refused:?}").contains("by_rank"));
    assert_eq!(f.read_back(), f.oracle, "a refused pass changes nothing");
}

// ── 3. the per-row RESTRICT probe cap ─────────────────────────────────────

#[test]
fn a_row_with_more_contexts_than_the_restrict_probe_cap_refuses_the_pass_and_says_the_probe_stopped()
{
    let mut f = fixture(40);
    let (collection, by_rank) = (f.collection, f.by_rank);
    f.db().enable_graph().unwrap();
    let edge_type = f.db().create_edge_type("near").unwrap();
    let doomed = brute(&f.oracle, |row| row["rank"].as_i64().unwrap() < 20);
    let anchored = doomed.iter().next().unwrap().clone();
    let source = f.db().get(collection, &anchored).unwrap().unwrap().id;
    let last_key = f.oracle.keys().last().unwrap().clone();
    let other = f.db().get(collection, &last_key).unwrap().unwrap().id;
    // One edge in each of more contexts than the probe walks.
    let contexts = RESTRICT_ROW_PROBE_SEEKS + 20;
    for i in 0..contexts {
        let context = f.db().create_graph_context(&format!("c{i:04}")).unwrap();
        f.db()
            .put_edge(context, source, edge_type, other, &json!({}))
            .unwrap();
    }
    f.db().commit().unwrap();
    let before = f.read_back();

    let filters = [rank_range(by_rank, 0, 20)];
    let refused = f
        .db()
        .delete_where(collection, &filters, generous())
        .unwrap_err();
    let text = format!("{refused:?}");
    assert!(
        text.contains("RESTRICT"),
        "the refusal names the mode: {text}"
    );
    assert!(
        text.contains(&RESTRICT_ROW_PROBE_SEEKS.to_string()),
        "a truncated probe says where it stopped: {text}"
    );
    f.db().rollback().unwrap();
    assert_eq!(
        f.read_back(),
        before,
        "a RESTRICT refusal deletes nothing and cascades nothing"
    );
    // The edges are all still there: nothing cascaded behind the refusal.
    // Counted here over the contexts this test created, which is the
    // brute-force answer it holds.
    for i in [0usize, contexts / 2, contexts - 1] {
        let context = f.db().graph_context(&format!("c{i:04}")).unwrap().unwrap();
        assert_eq!(
            f.read()
                .neighbors(sekejap_core::collections::NeighborRequest {
                    entity: source,
                    direction: Direction::Outgoing,
                    context,
                    edge_type: Some(edge_type),
                    limit: 8,
                })
                .unwrap()
                .len(),
            1,
            "context c{i:04} still holds its edge: a RESTRICT refusal cascades nothing"
        );
    }
    f.indexes_are_consistent("after a capped RESTRICT refusal");
}

// ── 4. the work a sparse predicate charges ────────────────────────────────

#[test]
fn a_sparse_predicate_charges_candidates_in_proportion_to_the_matches_and_not_to_the_pages() {
    // 1,200 rows, of which 120 match: every page of the pass is FULL except
    // the last, so the pass runs one page per 256 matches. If a page
    // re-walked the candidates the pages before it had already passed, the
    // candidate count would be quadratic in the pages.
    let mut f = fixture(1_200);
    let (collection, by_rank) = (f.collection, f.by_rank);
    let doomed = brute(&f.oracle, |row| row["rank"].as_i64().unwrap() < 10);
    assert!(doomed.len() > 100, "{} matches", doomed.len());
    let filters = [rank_range(by_rank, 0, 10)];
    let mut budget = generous();
    budget.rows_written = 16;

    let mut cursor = WriteCursor::start();
    let mut candidates = 0u64;
    let mut primary_reads = 0u64;
    let mut pages = 0u64;
    loop {
        let progress = f
            .db()
            .write_where(
                WriteRequest {
                    collection,
                    filters: &filters,
                    action: WriteAction::Delete(DeleteMode::Restrict),
                    driver: CandidateDriver::Auto,
                    after: cursor.clone(),
                },
                budget,
            )
            .unwrap();
        f.db().commit().unwrap();
        candidates += progress.work.candidates;
        primary_reads += progress.work.primary_reads;
        pages += 1;
        cursor = progress.cursor.clone();
        assert!(pages < 100, "the pass is not converging");
        if progress.done {
            break;
        }
    }
    let matches = doomed.len() as u64;
    assert!(
        pages >= 7,
        "the probe needs several pages to be about pages: {pages}"
    );
    // A posting-certified scalar equality range reads no row at all, and the
    // candidates are the postings inside the range. Both are the MATCHES,
    // with at most one extra candidate per page (the page that learns there
    // is more).
    assert!(
        candidates <= matches + pages,
        "candidates {candidates} for {matches} matches over {pages} pages is proportional to the pages"
    );
    assert!(
        primary_reads <= matches + pages,
        "primary reads {primary_reads} for {matches} matches over {pages} pages is proportional to the pages"
    );
}

/// The keys one prepared query returns, read through the engine's pages and
/// translated to external keys by a scan this test walks itself. Used only to
/// ask the INDEX a question; the oracle it is compared against is always the
/// `BTreeMap` the test built.
fn keys_matching(
    db: &Database,
    collection: CollectionId,
    filters: &[QueryFilter<'_>],
) -> BTreeSet<String> {
    let mut by_id: BTreeMap<u64, String> = BTreeMap::new();
    for entity in db.scan(collection, None).unwrap() {
        let entity = entity.unwrap();
        by_id.insert(entity.id.sequence, entity.key.clone());
    }
    let mut prepared = db
        .prepare_query(QueryRequest {
            collection,
            filters,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let mut out = BTreeSet::new();
    loop {
        let page = prepared.next_page(256, generous(), || false).unwrap();
        for row in &page.rows {
            out.insert(by_id[&row.id.sequence].clone());
        }
        if page.done {
            break;
        }
    }
    out
}

// ── 5. a refusal that arrives after the pass has written rows ─────────────

#[test]
fn a_refusal_raised_after_the_pass_has_written_rows_names_them_and_the_rows_stay_pending() {
    // e4 has ONE transaction per handle and no savepoint. A pass therefore
    // cannot undo only its own rows without discarding the caller's earlier
    // uncommitted work, which it has no right to do (Law 3). What it owes is
    // the TRUTH: a refusal raised after it has written must SAY how many
    // rows are pending, or a caller who reads "refused" commits them by
    // accident on the next statement.
    let mut f = fixture(300);
    let (collection, by_rank) = (f.collection, f.by_rank);
    f.db().enable_graph().unwrap();
    let context = f.db().create_graph_context("routes").unwrap();
    let edge_type = f.db().create_edge_type("near").unwrap();
    let doomed = brute(&f.oracle, |row| row["rank"].as_i64().unwrap() < 50);
    assert!(doomed.len() > 100);
    // The row the refusal lands on sits LATE in the driver's order -- the
    // walk rides `by_rank`, so rank 49 is near the end -- and many rows are
    // written before the pass reaches it.
    let anchored = f
        .oracle
        .iter()
        .filter(|(_, row)| row["rank"].as_i64().unwrap() == 49)
        .map(|(key, _)| key.clone())
        .next()
        .expect("the fixture has a row at rank 49");
    let hub_key = f.oracle.keys().last().unwrap().clone();
    let source = f.db().get(collection, &anchored).unwrap().unwrap().id;
    let hub = f.db().get(collection, &hub_key).unwrap().unwrap().id;
    f.db()
        .put_edge(context, source, edge_type, hub, &json!({}))
        .unwrap();
    f.db().commit().unwrap();

    let filters = [rank_range(by_rank, 0, 50)];
    let refused = f
        .db()
        .delete_where(collection, &filters, generous())
        .unwrap_err();
    let text = format!("{refused:?}");
    // Counted here, not asked of the engine: the oracle knows how many rows
    // the file has lost.
    let written = f.oracle.len() - f.read_back().len();
    assert!(
        written > 0,
        "the probe needs the refusal to arrive after some rows are written"
    );
    assert!(
        text.contains("RESTRICT") && text.contains("routes"),
        "the refusal still names its own reason: {text}"
    );
    let lower = text.to_ascii_lowercase();
    assert!(
        lower.contains("uncommitted") && lower.contains("rollback"),
        "the refusal says the rows already written are pending: {text}"
    );
    assert!(
        text.contains(&written.to_string()),
        "the refusal names how many ({written}): {text}"
    );

    // And the caller's word is what decides them.
    f.db().rollback().unwrap();
    assert_eq!(f.read_back(), f.oracle, "ROLLBACK restores the collection");
    f.indexes_are_consistent("after a rolled-back partial pass");
}

#[test]
fn a_refusal_before_the_first_write_carries_no_pending_row_sentence() {
    // The other half of the same rule: a pass refused before it writes
    // anything must not claim rows are pending, or the sentence stops
    // meaning anything.
    let mut f = fixture(120);
    let (collection, by_rank) = (f.collection, f.by_rank);
    let mut patch = UpdatePatch::new();
    patch.set("rank", json!(5)).unwrap();
    let filters = [rank_range(by_rank, 0, 10)];
    let refused = f
        .db()
        .update_where(collection, &filters, &patch, generous())
        .unwrap_err();
    let text = format!("{refused:?}");
    assert!(text.contains("by_rank"), "{text}");
    assert!(
        !text.to_ascii_lowercase().contains("already written"),
        "a pass that wrote nothing claims nothing: {text}"
    );
    assert_eq!(f.read_back(), f.oracle);
}

#[test]
fn a_sparse_predicate_over_the_entity_cursor_walks_the_collection_once_across_every_page() {
    // The entity cursor is the driver where re-walking would hurt most: a
    // page that did not resume would re-read every row the pages before it
    // had already passed, and the candidate count would be quadratic in the
    // pages. 1,200 rows, 120 matches, 16 rows a page.
    let mut f = fixture(1_200);
    let (collection, by_band) = (f.collection, f.by_band);
    let doomed = brute(&f.oracle, |row| row["band"].as_i64().unwrap() == 3);
    assert!(doomed.len() > 100, "{} matches", doomed.len());
    let filters = [band_eq(by_band, 3)];
    let mut budget = generous();
    budget.rows_written = 16;

    let mut cursor = WriteCursor::start();
    let mut candidates = 0u64;
    let mut pages = 0u64;
    loop {
        let progress = f
            .db()
            .write_where(
                WriteRequest {
                    collection,
                    filters: &filters,
                    action: WriteAction::Delete(DeleteMode::Restrict),
                    driver: CandidateDriver::Entities,
                    after: cursor.clone(),
                },
                budget,
            )
            .unwrap();
        f.db().commit().unwrap();
        assert_eq!(progress.driver, QueryDriver::Entities);
        candidates += progress.work.candidates;
        pages += 1;
        cursor = progress.cursor.clone();
        assert!(pages < 100, "the pass is not converging");
        if progress.done {
            break;
        }
    }
    let rows = f.oracle.len() as u64;
    let matches = doomed.len() as u64;
    assert!(pages >= 7, "the probe needs several pages: {pages}");
    // ONE walk of the collection, not one per page. The allowance on top of
    // it is per PAGE and not per row: a page walks one match past what it
    // returns, to learn whether there is more, and the next page resumes at
    // the match it returned -- so each page re-walks the rows in that one
    // gap, which is `rows / matches` of them.
    let gap = rows / matches;
    assert!(
        candidates <= rows + pages * (2 * gap + 1),
        "candidates {candidates} over {pages} pages of a {rows}-row collection with {matches} matches is more than one walk plus a gap a page"
    );
    // And it is nowhere near a re-walk per page, which is what the bound is
    // about.
    assert!(
        candidates * 4 < pages * rows,
        "candidates {candidates} is within a factor of four of the {} a re-walk per page would cost",
        pages * rows
    );
    for key in &doomed {
        f.oracle.remove(key);
    }
    assert_eq!(f.read_back(), f.oracle);
}
