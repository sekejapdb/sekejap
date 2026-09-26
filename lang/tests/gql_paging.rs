//! Paging and cancellation of a GQL answer end to end, through the public
//! Rust API (M3-E of `docs/lang/GQL_PROFILE_DESIGN.md`, §3.5; brief §11
//! matrix "Multiple pages" and "Budget exhaustion / cancel").
//!
//! What is at risk, and the test that pins it:
//!
//! * the pages of ONE execution, concatenated, are exactly the one-shot
//!   answer, row for row and in the same order, at every page size: path
//!   enumeration, `ANY SHORTEST`, `ANY CHEAPEST`, a `Reach`, `OPTIONAL
//!   MATCH`, a grouped `RETURN` and an outer `ORDER BY` + `LIMIT`, each
//!   answer spanning several pages (`pages_concatenate_*`);
//! * an ordered answer is ordered across page boundaries, not only within
//!   a page (`an_outer_order_by_*`);
//! * a budget that runs out is a NAMED failure carrying its limit and the
//!   count it reached, and the stream ends in that error, never in success
//!   (`an_exhausted_budget_*`);
//! * a cancel part way through stops the stream after the rows already
//!   handed out, and the call ends in the cancel, never in success
//!   (`a_cancel_mid_stream_*`);
//! * a runtime error raised on a LATER page keeps its SQLSTATE, and no row
//!   is handed out after it (`a_runtime_error_on_a_later_page_*`).
//!
//! Workload names are invented: people `p00`-`p29`, bands `b0`-`b4`, edge
//! types `knows` and `member_of`.

use sekejap_core::collections::{Database, EntityId, GraphContextId, QueryBudget};
use sekejap_core::Kind;
use sekejap_lang::{explain_sql, prepare_sql, SqlError, SqlResult, SqlRow, SqlValue};
use serde_json::json;
use tempfile::TempDir;

mod common;
use common::cfg;

const PEOPLE: usize = 30;

/// The age of person `i`: 20..49, each age once (7 is prime to 30), and
/// `p10` is the one aged 30.
fn age(i: usize) -> i64 {
    20 + (i * 7 % PEOPLE) as i64
}

/// ```text
/// person p00..p29 (name, age)        band b0..b4 (name)
/// member_of  p_i -> b_(i mod 5)
/// knows      p_i -> p_(i+1) (w = 1 + i mod 4)
///            p_i -> p_(i+2) (w = 3)
///            p29 -> p00     (w = 1)            -- one cycle
/// ```
fn fixture(dir: &TempDir) -> Database {
    let mut db = Database::create(dir.path().join("paging.sekejap"), cfg()).unwrap();
    let person = db
        .create_collection(
            "person",
            vec![
                ("name".to_owned(), Kind::Text),
                ("age".to_owned(), Kind::Int),
            ],
            Default::default(),
        )
        .unwrap();
    let band = db
        .create_collection(
            "band",
            vec![("name".to_owned(), Kind::Text)],
            Default::default(),
        )
        .unwrap();
    let people: Vec<EntityId> = (0..PEOPLE)
        .map(|i| {
            db.put(
                person,
                &format!("p{i:02}"),
                &json!({"name": format!("person {i}"), "age": age(i)}),
            )
            .unwrap()
        })
        .collect();
    let bands: Vec<EntityId> = (0..5)
        .map(|i| {
            db.put(
                band,
                &format!("b{i}"),
                &json!({"name": format!("band {i}")}),
            )
            .unwrap()
        })
        .collect();
    db.enable_graph().unwrap();
    let knows = db.create_edge_type("knows").unwrap();
    let member_of = db.create_edge_type("member_of").unwrap();
    let base = GraphContextId::BASE;
    for i in 0..PEOPLE {
        db.create_edge(base, people[i], member_of, bands[i % 5], &json!({}))
            .unwrap();
        if i + 1 < PEOPLE {
            db.create_edge(
                base,
                people[i],
                knows,
                people[i + 1],
                &json!({"w": 1 + i % 4}),
            )
            .unwrap();
        }
        if i + 2 < PEOPLE {
            db.create_edge(base, people[i], knows, people[i + 2], &json!({"w": 3}))
                .unwrap();
        }
    }
    db.create_edge(base, people[PEOPLE - 1], knows, people[0], &json!({"w": 1}))
        .unwrap();
    db.commit().unwrap();
    db
}

const FROM_P00: &str = "(s IS person WHERE s._key = 'p00')";

/// Every statement the paging tests run, by name. Each answer spans several
/// pages at the page sizes of [`PAGE_SIZES`].
fn statements() -> Vec<(&'static str, String)> {
    let graph = |body: String| format!("SELECT * FROM GRAPH_TABLE (base {body})");
    vec![
        (
            "enumerate",
            graph(format!(
                "MATCH p = {FROM_P00}-[:knows]->{{1,5}}(t) RETURN t._key AS t, PATH_LENGTH(p) AS n"
            )),
        ),
        (
            "any shortest",
            graph(format!(
                "MATCH p = ANY SHORTEST {FROM_P00}-[:knows]->{{1,20}}(t) \
                 RETURN t._key AS t, PATH_LENGTH(p) AS n"
            )),
        ),
        (
            "any cheapest",
            graph(format!(
                "MATCH p = ANY CHEAPEST {FROM_P00}-[e:knows COST e.w]->{{1,20}}(t) \
                 RETURN t._key AS t, PATH_LENGTH(p) AS n"
            )),
        ),
        (
            "reach",
            graph(format!(
                "MATCH ACYCLIC {FROM_P00}-[:knows]->{{1,20}}(t) RETURN DISTINCT t._key AS t"
            )),
        ),
        (
            "optional match",
            graph(
                "MATCH (a IS person) OPTIONAL MATCH (a)-[:knows]->(b IS person WHERE b.age > 40) \
                 RETURN a._key AS a, b._key AS b"
                    .to_owned(),
            ),
        ),
        (
            "grouped return",
            graph(
                "MATCH (a IS person)-[:knows]->{1,3}(t)-[:member_of]->(b IS band) \
                 RETURN a._key AS a, b._key AS b, COUNT(*) AS n"
                    .to_owned(),
            ),
        ),
        (
            "outer order by and limit",
            format!(
                "SELECT t, n FROM GRAPH_TABLE (base MATCH p = {FROM_P00}-[:knows]->{{1,5}}(t) \
                 RETURN t._key AS t, PATH_LENGTH(p) AS n) AS g ORDER BY n DESC, t LIMIT 40 OFFSET 3"
            ),
        ),
    ]
}

/// Page sizes each statement is paged at; 1 is a page per row.
const PAGE_SIZES: [usize; 4] = [1, 2, 3, 7];

/// The one-shot answer: one execution run to exhaustion by `run`.
fn one_shot(db: &Database, sql: &str) -> Vec<Vec<SqlValue>> {
    let prepared = prepare_sql(db, sql, &[]).unwrap_or_else(|e| panic!("prepare `{sql}`: {e}"));
    match prepared
        .run(db)
        .unwrap_or_else(|e| panic!("run `{sql}`: {e}"))
    {
        SqlResult::Rows { rows, .. } => rows.into_iter().map(|row| row.values).collect(),
        other => panic!("`{sql}` answered {other:?}"),
    }
}

/// The answer paged at `page_rows` through `for_each_row_with`: the rows
/// handed out, and how the call ended.
fn paged(
    db: &Database,
    sql: &str,
    page_rows: usize,
    budget: QueryBudget,
    cancelled: &mut dyn FnMut() -> bool,
) -> (Vec<Vec<SqlValue>>, Result<(), SqlError>) {
    let prepared = prepare_sql(db, sql, &[]).unwrap_or_else(|e| panic!("prepare `{sql}`: {e}"));
    let mut rows = Vec::new();
    let ended =
        prepared.for_each_row_with(db, page_rows, budget, cancelled, &mut |row: &SqlRow| {
            rows.push(row.values.clone());
            Ok(())
        });
    (rows, ended)
}

// ── pages equal the one-shot answer ──────────────────────────────────────

#[test]
fn pages_concatenate_to_the_one_shot_answer_row_for_row_at_every_page_size() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    for (name, sql) in statements() {
        let whole = one_shot(&db, &sql);
        let largest = *PAGE_SIZES.last().unwrap();
        assert!(
            whole.len() > 2 * largest,
            "`{name}` answers {} rows, which does not span several pages of {largest}",
            whole.len()
        );
        for page_rows in PAGE_SIZES {
            let (rows, ended) = paged(&db, &sql, page_rows, QueryBudget::unlimited(), &mut || {
                false
            });
            ended.unwrap_or_else(|e| panic!("`{name}` at {page_rows} per page: {e}"));
            assert_eq!(
                rows, whole,
                "`{name}`: the pages of {page_rows} rows are not the one-shot answer"
            );
        }
    }
}

#[test]
fn the_reach_statement_really_runs_as_reach() {
    // The paging test's `reach` case is only a Reach case if the planner
    // chose the node BFS for it.
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let (_, sql) = statements()
        .into_iter()
        .find(|(name, _)| *name == "reach")
        .unwrap();
    let plan = explain_sql(&db, &sql, &[]).unwrap();
    assert!(plan.contains(". Reach from "), "not a Reach:\n{plan}");
}

#[test]
fn an_outer_order_by_is_ordered_across_page_boundaries() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let (_, sql) = statements()
        .into_iter()
        .find(|(name, _)| *name == "outer order by and limit")
        .unwrap();
    let (rows, ended) = paged(&db, &sql, 3, QueryBudget::unlimited(), &mut || false);
    ended.unwrap();
    assert_eq!(
        rows.len(),
        40,
        "LIMIT 40 after OFFSET 3 over more than 43 paths"
    );
    let key = |row: &Vec<SqlValue>| match (&row[1], &row[0]) {
        (SqlValue::Int(n), SqlValue::Text(t)) => (-n, t.clone()),
        other => panic!("row {other:?}"),
    };
    for pair in rows.windows(2) {
        assert!(key(&pair[0]) <= key(&pair[1]), "out of order: {pair:?}");
    }
    // The longest paths from p00 are five hops, and the answer starts there
    // after skipping three of them.
    assert_eq!(rows[0][1], SqlValue::Int(5));
}

// ── a stream that stops is never a complete answer ──────────────────────

/// The count a budget refusal says it reached.
fn attempted(error: &str) -> u64 {
    let at = error.find("attempted: ").expect("an attempted count") + "attempted: ".len();
    error[at..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap()
}

#[test]
fn an_exhausted_budget_is_a_named_failure_with_its_counters_and_never_success() {
    // Work is charged per page (design §3.5), so every `graph_edges`
    // ceiling from 1 up either lets the whole answer through or stops it
    // with the named refusal -- after a prefix of the answer when the pages
    // before the refused one fitted. Never a shorter answer that ends well.
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let mut truncated_mid_stream = Vec::new();
    for (name, sql) in statements() {
        let whole = one_shot(&db, &sql);
        let mut limit = 1u64;
        loop {
            let budget = QueryBudget {
                graph_edges: limit,
                ..QueryBudget::unlimited()
            };
            let (rows, ended) = paged(&db, &sql, 2, budget, &mut || false);
            let error = match ended {
                Ok(()) => {
                    assert_eq!(
                        rows, whole,
                        "`{name}` under {limit} edges a page ended well"
                    );
                    break;
                }
                Err(error) => error.to_string(),
            };
            assert!(
                error.contains("BudgetExceeded")
                    && error.contains("GraphEdges")
                    && error.contains(&format!("limit: {limit},"))
                    && attempted(&error) > limit,
                "`{name}`: the refusal names the resource, its limit and the count it \
                 reached: {error}"
            );
            // Every row may be out already: the refused page was the one
            // that would have found there is no more. The stream is still
            // refused, because nothing proved it complete.
            assert!(
                rows.len() <= whole.len(),
                "`{name}`: more rows than the answer"
            );
            assert_eq!(
                rows,
                whole[..rows.len()],
                "`{name}`: the rows before the refusal are the answer's first rows"
            );
            if !rows.is_empty() && !truncated_mid_stream.contains(&name) {
                truncated_mid_stream.push(name);
            }
            limit = if limit < 16 { limit + 1 } else { limit * 2 };
            assert!(limit < 1 << 20, "`{name}` never completed");
        }
    }
    assert!(
        truncated_mid_stream.contains(&"any shortest")
            && truncated_mid_stream.contains(&"any cheapest"),
        "some refusals land after rows were handed out: {truncated_mid_stream:?}"
    );
}

#[test]
fn a_cancel_mid_stream_stops_after_the_rows_already_handed_out() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let mut stopped_mid_stream = Vec::new();
    for (name, sql) in statements() {
        let whole = one_shot(&db, &sql);
        // How often one full run consults the cancel; the cancel then fires
        // half way through it, so a pipelined answer has rows out when it
        // lands.
        let mut polls = 0u64;
        let (_, ended) = paged(&db, &sql, 2, QueryBudget::unlimited(), &mut || {
            polls += 1;
            false
        });
        ended.unwrap();
        assert!(
            polls > 2,
            "`{name}`: the cancel is consulted as the work goes on"
        );
        let mut left = polls / 2;
        let (rows, ended) = paged(&db, &sql, 2, QueryBudget::unlimited(), &mut || {
            left = left.saturating_sub(1);
            left == 0
        });
        match ended {
            Ok(()) => panic!("`{name}`: a cancelled stream reported a complete answer"),
            Err(error) => assert!(
                error.to_string().contains("Cancelled"),
                "`{name}`: the stream ends in the cancel: {error}"
            ),
        }
        assert!(
            rows.len() < whole.len(),
            "`{name}`: a cancelled stream handed out everything"
        );
        assert_eq!(
            rows,
            whole[..rows.len()],
            "`{name}`: the rows before the cancel"
        );
        if !rows.is_empty() {
            stopped_mid_stream.push(name);
        }
    }
    for name in [
        "enumerate",
        "any shortest",
        "any cheapest",
        "optional match",
    ] {
        assert!(
            stopped_mid_stream.contains(&name),
            "`{name}` is pipelined, so its cancel lands after rows were handed out: \
             {stopped_mid_stream:?}"
        );
    }
}

#[test]
fn a_runtime_error_on_a_later_page_keeps_its_sqlstate_and_nothing_follows_it() {
    // A label scan is pipelined, in key order: `100 / (age - 30)` divides by
    // zero at p10, aged 30, the eleventh row -- page six at two rows a page.
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let sql = "SELECT * FROM GRAPH_TABLE (base MATCH (a IS person) \
               RETURN a._key AS k, 100 / (a.age - 30) AS v)";
    let (rows, ended) = paged(&db, sql, 2, QueryBudget::unlimited(), &mut || false);
    match ended {
        Err(SqlError::Coded { sqlstate, .. }) => assert_eq!(sqlstate, "22012"),
        other => panic!(
            "expected 22012 division_by_zero, got {other:?} after {} rows",
            rows.len()
        ),
    }
    let keys: Vec<SqlValue> = rows.iter().map(|row| row[0].clone()).collect();
    let before: Vec<SqlValue> = (0..10)
        .map(|i| SqlValue::Text(format!("p{i:02}")))
        .collect();
    assert_eq!(
        keys, before,
        "the ten rows before p10 were handed out, and none after"
    );
}
