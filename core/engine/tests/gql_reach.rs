//! `Reach`: the existing node BFS as a GQL operator
//! (`docs/lang/GQL_PROFILE_DESIGN.md` §4.6). The planner puts it in place of
//! a path search only when it has proved the two equivalent
//! (`lang/tests/gql_reach.rs` pins that proof); this file pins the operator.
//!
//! What is at risk, and the test that pins it:
//!
//! * the answer is the node BFS's: every node reachable within `max_hops`,
//!   once, and the start only when `min_hops` is 0 -- never through a cycle
//!   back to it (`the_answer_is_each_reachable_node_once`);
//! * the end's labels and predicate are tested AFTER the walk, so a node
//!   the predicate rejects still leads on to the nodes behind it
//!   (`the_end_test_filters_after_the_walk_and_never_prunes_it`);
//! * a type or context no write has used walks nothing
//!   (`the_answer_is_each_reachable_node_once`);
//! * the pages of one cursor concatenate to the one-shot answer, and the
//!   answer an input row still holds is charged again to each page as
//!   `queue_entries` (`paging_equals_one_shot_and_the_held_answer_is_recharged`);
//! * a budget refusal and a cancellation poison the cursor
//!   (`budgets_and_cancellation_refuse_and_poison`);
//! * a spec the BFS cannot run is refused when the cursor opens
//!   (`a_spec_the_bfs_cannot_run_is_refused_at_open`).
//!
//! Workload names are invented: collections `site_a`, `site_b`, edge type
//! `r`.

use sekejap_core::collections::gql::{
    BindingRow, BindingValue, EvalCx, ExprId, GqlBudget, GqlCursor, GqlHost, GqlWork, OpSpec,
    ReachSpec, SeedId, SeedSource, SlotId, Truth,
};
use sekejap_core::collections::{
    CollectionId, Database, Direction, EdgeTypeId, EntityId, GraphContextId, PreparedQuery,
    QueryError, QueryResult, WorkResource,
};
use sekejap_core::Kind;
use serde_json::json;

mod common;
use common::cfg;

// ── the test host ─────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum E {
    Text(&'static str),
    /// `slot.x < k` on a node; `Null` is unknown.
    Below(u16, i64),
}

#[derive(Default)]
struct TestHost {
    exprs: Vec<E>,
}

impl TestHost {
    fn expr(&mut self, e: E) -> ExprId {
        self.exprs.push(e);
        ExprId(self.exprs.len() as u32 - 1)
    }
}

impl GqlHost for TestHost {
    fn eval(
        &self,
        expr: ExprId,
        row: &BindingRow,
        cx: &mut EvalCx<'_, '_>,
    ) -> QueryResult<BindingValue> {
        Ok(match &self.exprs[expr.0 as usize] {
            E::Text(t) => BindingValue::Text((*t).into()),
            E::Below(s, k) => match row.get(SlotId(*s)) {
                BindingValue::Node(n) => match cx.reader.node_property(*n, "x", cx.meter)? {
                    BindingValue::Int(v) => BindingValue::Bool(v < *k),
                    _ => BindingValue::Null,
                },
                other => panic!("a predicate saw {other:?}, not a node"),
            },
        })
    }

    fn test(&self, expr: ExprId, row: &BindingRow, cx: &mut EvalCx<'_, '_>) -> QueryResult<Truth> {
        Ok(match self.eval(expr, row, cx)? {
            BindingValue::Bool(true) => Truth::True,
            BindingValue::Bool(false) => Truth::False,
            _ => Truth::Unknown,
        })
    }

    fn open_seed<'db>(
        &self,
        _: SeedId,
        _: &'db Database,
        _: &BindingRow,
        _: &mut EvalCx<'_, '_>,
    ) -> QueryResult<Option<PreparedQuery<'db>>> {
        unreachable!("these plans seed by key")
    }
}

// ── the graph ─────────────────────────────────────────────────────────────

struct Graph {
    _dir: tempfile::TempDir,
    db: Database,
    site_a: CollectionId,
    site_b: CollectionId,
    r: EdgeTypeId,
    ids: Vec<(String, EntityId)>,
}

impl Graph {
    fn key(&self, id: EntityId) -> &str {
        &self.ids.iter().find(|(_, i)| *i == id).unwrap().0
    }
}

/// ```text
/// site_a (x):  n0 (1)  n1 (9)  n2 (1)  n3 (1)  n4 (1)     site_b: b0 (1)
/// type r:      n0->n1  n0->n2  n1->n3  n2->n3  n3->n0  n3->n4  n1->b0
/// ```
fn graph() -> Graph {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("g.sekejap"), cfg()).unwrap();
    let site_a = db
        .create_collection("site_a", vec![("x".into(), Kind::Int)], Default::default())
        .unwrap();
    let site_b = db
        .create_collection("site_b", vec![("x".into(), Kind::Int)], Default::default())
        .unwrap();
    db.enable_graph().unwrap();
    let r = db.create_edge_type("r").unwrap();
    let mut ids = Vec::new();
    for (key, x) in [("n0", 1), ("n1", 9), ("n2", 1), ("n3", 1), ("n4", 1)] {
        ids.push((
            key.to_owned(),
            db.put(site_a, key, &json!({ "x": x })).unwrap(),
        ));
    }
    ids.push((
        "b0".to_owned(),
        db.put(site_b, "b0", &json!({ "x": 1 })).unwrap(),
    ));
    let id = |key: &str| ids.iter().find(|(k, _)| k == key).unwrap().1;
    for (from, to) in [
        ("n0", "n1"),
        ("n0", "n2"),
        ("n1", "n3"),
        ("n2", "n3"),
        ("n3", "n0"),
        ("n3", "n4"),
        ("n1", "b0"),
    ] {
        db.create_edge(GraphContextId::BASE, id(from), r, id(to), &json!({}))
            .unwrap();
    }
    db.commit().unwrap();
    Graph {
        _dir: dir,
        db,
        site_a,
        site_b,
        r,
        ids,
    }
}

fn spec(g: &Graph, min_hops: u32, max_hops: u32) -> ReachSpec {
    ReachSpec {
        context: GraphContextId::BASE,
        types: Some(Box::new([g.r])),
        direction: Direction::Outgoing,
        min_hops,
        max_hops,
        end_labels: None,
        end_filter: None,
    }
}

/// Seed slot 0 by key `start` in `site_a`, then `Reach` into slot 1.
fn plan(g: &Graph, host: &mut TestHost, start: &'static str, spec: ReachSpec) -> OpSpec {
    let key = host.expr(E::Text(start));
    OpSpec::Reach {
        input: Box::new(OpSpec::Seed {
            input: Box::new(OpSpec::Unit { width: 2 }),
            out: SlotId(0),
            source: SeedSource::Key {
                key,
                labels: Box::new([g.site_a]),
            },
        }),
        from: SlotId(0),
        to: Some(SlotId(1)),
        spec,
    }
}

fn never() -> bool {
    false
}

/// The keys of every end, in the order they came, and each page's work.
fn run_paged(
    g: &Graph,
    host: &TestHost,
    plan: &OpSpec,
    page_rows: usize,
) -> QueryResult<(Vec<String>, Vec<GqlWork>)> {
    let mut cursor = GqlCursor::open(&g.db, host, plan, Vec::new())?;
    let (mut keys, mut work) = (Vec::new(), Vec::new());
    loop {
        let page = cursor.next_page(page_rows, GqlBudget::unlimited(), never)?;
        for row in &page.rows {
            match row.get(SlotId(1)) {
                BindingValue::Node(n) => keys.push(g.key(n.0).to_owned()),
                other => panic!("an end that is not a node: {other:?}"),
            }
        }
        work.push(page.work);
        if page.done {
            return Ok((keys, work));
        }
    }
}

fn ends(g: &Graph, start: &'static str, spec: ReachSpec) -> Vec<String> {
    let mut host = TestHost::default();
    let plan = plan(g, &mut host, start, spec);
    let (mut keys, _) = run_paged(g, &host, &plan, 8192).unwrap();
    keys.sort();
    keys
}

// ── semantics ─────────────────────────────────────────────────────────────

#[test]
fn the_answer_is_each_reachable_node_once() {
    let g = graph();
    // n3 is reached twice and n0 again through n3: each node once, and the
    // start never at a depth of one or more.
    assert_eq!(
        ends(&g, "n0", spec(&g, 1, 3)),
        ["b0", "n1", "n2", "n3", "n4"]
    );
    assert_eq!(
        ends(&g, "n0", spec(&g, 0, 3)),
        ["b0", "n0", "n1", "n2", "n3", "n4"]
    );
    assert_eq!(ends(&g, "n0", spec(&g, 1, 1)), ["n1", "n2"]);
    assert_eq!(ends(&g, "n0", spec(&g, 0, 0)), ["n0"]);
    let incoming = ReachSpec {
        direction: Direction::Incoming,
        ..spec(&g, 1, 2)
    };
    assert_eq!(ends(&g, "n3", incoming), ["n0", "n1", "n2"]);
    let any_type = ReachSpec {
        types: None,
        direction: Direction::Both,
        ..spec(&g, 1, 1)
    };
    assert_eq!(ends(&g, "n4", any_type), ["n3"]);
    // A type no write has used: the walk crosses nothing.
    let unknown = ReachSpec {
        types: Some(Box::new([])),
        ..spec(&g, 0, 3)
    };
    assert_eq!(ends(&g, "n0", unknown), ["n0"]);
    let unknown = ReachSpec {
        types: Some(Box::new([])),
        ..spec(&g, 1, 3)
    };
    assert!(ends(&g, "n0", unknown).is_empty());
}

#[test]
fn the_end_test_filters_after_the_walk_and_never_prunes_it() {
    let g = graph();
    let mut host = TestHost::default();
    // n1 (x 9) fails `x < 5`, but b0 behind it does not.
    let below = host.expr(E::Below(1, 5));
    let filtered = ReachSpec {
        end_filter: Some(below),
        ..spec(&g, 0, 3)
    };
    let plan = plan(&g, &mut host, "n0", filtered);
    let (mut keys, work) = run_paged(&g, &host, &plan, 8192).unwrap();
    keys.sort();
    assert_eq!(keys, ["b0", "n0", "n2", "n3", "n4"]);
    let binding_rows: u64 = work.iter().map(|w| w.binding_rows).sum();
    assert_eq!(binding_rows, 1 + 5, "the seed, then one row per end kept");
    let labelled = ReachSpec {
        end_labels: Some(Box::new([g.site_b])),
        ..spec(&g, 0, 3)
    };
    assert_eq!(ends(&g, "n0", labelled), ["b0"]);
}

#[test]
fn a_null_start_gives_no_row() {
    let g = graph();
    let host = TestHost::default();
    // The start slot is Null, as after an OPTIONAL MATCH that found nothing.
    let plan = OpSpec::Reach {
        input: Box::new(OpSpec::Unit { width: 2 }),
        from: SlotId(0),
        to: Some(SlotId(1)),
        spec: spec(&g, 0, 3),
    };
    let (keys, _) = run_paged(&g, &host, &plan, 8192).unwrap();
    assert!(keys.is_empty());
}

// ── paging, budgets, cancellation ─────────────────────────────────────────

#[test]
fn paging_equals_one_shot_and_the_held_answer_is_recharged() {
    let g = graph();
    let mut host = TestHost::default();
    let plan = plan(&g, &mut host, "n0", spec(&g, 0, 3));
    let (one_shot, _) = run_paged(&g, &host, &plan, 8192).unwrap();
    assert_eq!(one_shot.len(), 6);
    for page_rows in [1, 2, 3] {
        let (keys, work) = run_paged(&g, &host, &plan, page_rows).unwrap();
        assert_eq!(keys, one_shot, "pages of {page_rows}");
        // The second page still holds what the walk found and the first
        // page did not hand out -- no more -- and is charged for it.
        assert_eq!(
            work[1].queue_entries as usize,
            one_shot.len() - page_rows,
            "pages of {page_rows}: {work:?}"
        );
    }
}

fn refusal(result: QueryResult<impl std::fmt::Debug>) -> (WorkResource, u64) {
    match result {
        Err(QueryError::BudgetExceeded {
            resource, limit, ..
        }) => (resource, limit),
        other => panic!("expected a budget refusal, got {other:?}"),
    }
}

#[test]
fn budgets_and_cancellation_refuse_and_poison() {
    let g = graph();
    let mut host = TestHost::default();
    let plan = plan(&g, &mut host, "n0", spec(&g, 0, 3));
    let unlimited = GqlBudget::unlimited();
    for (budget, resource, limit) in [
        (
            GqlBudget {
                queue_entries: 3,
                ..unlimited
            },
            WorkResource::QueueEntries,
            3,
        ),
        (
            GqlBudget {
                binding_rows: 2,
                ..unlimited
            },
            WorkResource::BindingRows,
            2,
        ),
        (
            GqlBudget {
                base: sekejap_core::collections::QueryBudget {
                    graph_edges: 4,
                    ..unlimited.base
                },
                ..unlimited
            },
            WorkResource::GraphEdges,
            4,
        ),
    ] {
        let mut cursor = GqlCursor::open(&g.db, &host, &plan, Vec::new()).unwrap();
        assert_eq!(
            refusal(cursor.next_page(8192, budget, never)),
            (resource, limit)
        );
        assert_eq!(
            refusal(cursor.next_page(8192, unlimited, never)),
            (resource, limit),
            "poisoned"
        );
    }
    let mut cursor = GqlCursor::open(&g.db, &host, &plan, Vec::new()).unwrap();
    let mut calls = 0;
    let cancel_later = || {
        calls += 1;
        calls > 3
    };
    assert!(matches!(
        cursor.next_page(8192, unlimited, cancel_later),
        Err(QueryError::Cancelled)
    ));
    assert!(matches!(
        cursor.next_page(8192, unlimited, never),
        Err(QueryError::Cancelled)
    ));
}

#[test]
fn a_spec_the_bfs_cannot_run_is_refused_at_open() {
    let g = graph();
    let mut host = TestHost::default();
    let below = host.expr(E::Below(1, 5));
    let bad = [
        ReachSpec {
            types: Some(Box::new([g.r, EdgeTypeId(g.r.0 + 1)])),
            ..spec(&g, 1, 3)
        },
        spec(&g, 2, 3),
        spec(&g, 1, 65),
        spec(&g, 3, 2),
    ];
    for spec in bad {
        let what = format!("{spec:?}");
        let plan = plan(&g, &mut host, "n0", spec);
        let opened = GqlCursor::open(&g.db, &host, &plan, Vec::new());
        assert!(
            matches!(opened, Err(QueryError::Database(_))),
            "{what}: {:?}",
            opened.err()
        );
    }
    // An end filter with no slot to read the end from, and slots outside
    // the row.
    for (to, from, end_filter) in [
        (None, SlotId(0), Some(below)),
        (Some(SlotId(2)), SlotId(0), None),
        (Some(SlotId(1)), SlotId(5), None),
    ] {
        let mut plan = plan(
            &g,
            &mut host,
            "n0",
            ReachSpec {
                end_filter,
                ..spec(&g, 1, 3)
            },
        );
        if let OpSpec::Reach { to: t, from: f, .. } = &mut plan {
            *t = to;
            *f = from;
        }
        assert!(
            matches!(
                GqlCursor::open(&g.db, &host, &plan, Vec::new()),
                Err(QueryError::Database(_))
            ),
            "to {to:?}, from {from:?}"
        );
    }
}

/// The work of a filtered and a labelled walk, page by page, is pinned
/// counter by counter: rows out, ends held, edges walked, nodes visited,
/// rows read and key lookups. A change to the walk may spend less, never
/// more. Taken before the M4 review's refactors (task R-C).
#[test]
fn the_work_of_a_walk_is_pinned() {
    let g = graph();
    let mut host = TestHost::default();
    let below = host.expr(E::Below(1, 5));
    let specs = [
        ReachSpec {
            end_filter: Some(below),
            ..spec(&g, 0, 3)
        },
        ReachSpec {
            end_labels: Some(Box::new([g.site_a])),
            ..spec(&g, 1, 2)
        },
    ];
    let mut spent = Vec::new();
    for spec in specs {
        let plan = plan(&g, &mut host, "n0", spec);
        let (_, work) = run_paged(&g, &host, &plan, 2).unwrap();
        spent.extend(work.iter().map(|w| {
            [
                w.binding_rows,
                w.queue_entries,
                w.base.graph_edges,
                w.base.graph_visited,
                w.base.primary_reads,
                w.base.key_postings,
            ]
        }));
    }
    let pinned: &[[u64; 6]] = &[
        [3, 6, 12, 6, 3, 1],
        [2, 3, 0, 0, 2, 0],
        [1, 1, 0, 0, 1, 0],
        [3, 3, 8, 5, 0, 1],
        [1, 1, 0, 0, 0, 0],
    ];
    assert_eq!(spent, pinned, "{spent:?}");
}
