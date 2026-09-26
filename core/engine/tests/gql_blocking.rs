//! The GQL blocking operators (`docs/lang/GQL_PROFILE_DESIGN.md` §3.2-§3.5):
//! `Aggregate`, `Distinct`, `Sort`, `Page` and `Unnest`.
//!
//! What is at risk, and the test that pins it:
//!
//! * the empty-input aggregate rules: no grouping key gives ONE row (`COUNT`
//!   0, every other accumulator `NULL`, `ARRAY_AGG` too), a grouping key
//!   gives none (`empty_input_*`);
//! * `COUNT(*)` against `COUNT(x)` against `COUNT(DISTINCT x)`, on nodes
//!   (identity: two collections holding the same sequence are two nodes) and
//!   on scalars (`1` and `1.0` are one value) (`count_forms_*`);
//! * grouping by identity and by value, `NULL` grouping with `NULL`, and
//!   every accumulator's value (`grouped_accumulators_*`);
//! * a multi-key sort with PostgreSQL's null placement (last ascending,
//!   first descending) that keeps ties in input order, and a top-k under a
//!   limit that answers the same while holding less (`sort_*`);
//! * `DISTINCT` over whole rows, first occurrence kept (`distinct_*`);
//! * `OFFSET` / `LIMIT` at their edges, and their range (`page_*`);
//! * `FOR` over a `NULL`, an empty and a non-list value (`unnest_*`);
//! * every operator refusing at its cap BY NAME, what a blocking operator
//!   holds charged again at the start of every page, and a refused page
//!   poisoning the cursor (`*_refuses_*`, `held_state_is_recharged_*`);
//! * the pages of one cursor concatenating to the one-shot answer
//!   (`paging_equals_one_shot`).
//!
//! The input rows come from `FOR` over a parameter list of tuples, so no
//! graph is needed: [`TestHost`] is a minimal [`GqlHost`] whose expressions
//! read slots, list items and parameters.

use sekejap_core::collections::gql::{
    AggSpec, BindingRow, BindingValue, CountExpr, EvalCx, ExprId, GqlBudget, GqlCursor, GqlHost,
    GqlWork, ListRef, NodeRef, OpSpec, SeedId, SlotId, SortKey, Truth, ValueType,
};
use sekejap_core::collections::{
    CollectionId, Database, EntityId, PreparedQuery, QueryError, QueryResult, WorkResource,
};
use std::sync::Arc;

mod common;
use common::cfg;

// ── the test host ─────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum E {
    Param(usize),
    Slot(u16),
    /// Item `i` of the list in a slot; `Null` when the slot is not a list.
    Item(u16, usize),
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
            E::Param(n) => cx.params[*n].clone(),
            E::Slot(s) => row.get(SlotId(*s)).clone(),
            E::Item(s, i) => match row.get(SlotId(*s)) {
                BindingValue::List(list) => list.items[*i].clone(),
                _ => BindingValue::Null,
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
        Ok(None)
    }
}

// ── values ────────────────────────────────────────────────────────────────

use BindingValue::{Float, Int, Null};

fn text(s: &str) -> BindingValue {
    BindingValue::Text(s.into())
}

fn node(collection: u32, sequence: u64) -> BindingValue {
    BindingValue::Node(NodeRef(EntityId {
        collection: CollectionId(collection),
        sequence,
    }))
}

fn list(items: Vec<BindingValue>) -> BindingValue {
    BindingValue::List(ListRef {
        items: items.into(),
        elem: ValueType::Unknown,
    })
}

/// A parameter holding `rows` as a list of tuples: the input `FOR` walks.
fn table(rows: Vec<Vec<BindingValue>>) -> BindingValue {
    list(rows.into_iter().map(list).collect())
}

/// The exact spelling of a value: `Int(1)` and `Float(1.0)` are EQUAL as
/// binding values (grouping equality), so an assertion about a result's
/// kind compares this instead.
fn exact(rows: &[Vec<BindingValue>]) -> String {
    format!("{rows:?}")
}

// ── plans ─────────────────────────────────────────────────────────────────

/// `FOR t IN $0 LET c1 = t[0], ..., cN = t[N-1]`: rows of `1 + cols` slots,
/// slot 0 the tuple, slots `1..=cols` its items.
fn input(host: &mut TestHost, cols: usize) -> OpSpec {
    let list = host.expr(E::Param(0));
    let assign: Vec<(SlotId, ExprId)> = (0..cols)
        .map(|i| (SlotId(i as u16 + 1), host.expr(E::Item(0, i))))
        .collect();
    OpSpec::Let {
        input: Box::new(OpSpec::Unnest {
            input: Box::new(OpSpec::Unit {
                width: cols as u16 + 1,
            }),
            list,
            out: SlotId(0),
        }),
        assign: assign.into_boxed_slice(),
    }
}

/// Slots `1..=cols` of the input as a row of `cols` slots.
fn columns(host: &mut TestHost, plan: OpSpec, cols: usize) -> OpSpec {
    let exprs: Vec<ExprId> = (0..cols).map(|i| host.expr(E::Slot(i as u16 + 1))).collect();
    OpSpec::Project {
        input: Box::new(plan),
        cols: exprs.into_boxed_slice(),
        width: cols as u16,
    }
}

fn never() -> bool {
    false
}

struct Env {
    _dir: tempfile::TempDir,
    db: Database,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::create(dir.path().join("b.sekejap"), cfg()).unwrap();
    Env { _dir: dir, db }
}

/// Every row of one cursor in pages of `page_rows`, and each page's work.
fn run_with(
    db: &Database,
    host: &TestHost,
    plan: &OpSpec,
    params: Vec<BindingValue>,
    page_rows: usize,
    budget: GqlBudget,
) -> QueryResult<(Vec<Vec<BindingValue>>, Vec<GqlWork>)> {
    let mut cursor = GqlCursor::open(db, host, plan, params)?;
    let (mut rows, mut work) = (Vec::new(), Vec::new());
    loop {
        let page = cursor.next_page(page_rows, budget, never)?;
        assert!(page.rows.len() <= page_rows);
        rows.extend(page.rows.into_iter().map(|r| r.slots.into_vec()));
        work.push(page.work);
        if page.done {
            return Ok((rows, work));
        }
    }
}

fn run(
    db: &Database,
    host: &TestHost,
    plan: &OpSpec,
    params: Vec<BindingValue>,
) -> Vec<Vec<BindingValue>> {
    run_with(db, host, plan, params, 8192, GqlBudget::unlimited())
        .unwrap()
        .0
}

fn refused(result: QueryResult<impl std::fmt::Debug>, resource: WorkResource, limit: u64) {
    match result {
        Err(QueryError::BudgetExceeded {
            resource: r,
            limit: l,
            attempted,
        }) => {
            assert_eq!(r, resource);
            assert_eq!(l, limit);
            assert!(attempted > limit);
        }
        other => panic!("expected a {resource:?} refusal at {limit}, got {other:?}"),
    }
}

fn is_invalid(result: QueryResult<impl std::fmt::Debug>) {
    assert!(
        matches!(result, Err(QueryError::Database(_))),
        "expected an invalid-query error, got {result:?}"
    );
}

/// Every accumulator over slot 1, in the order the tests read them:
/// `COUNT(*)`, `COUNT(x)`, `COUNT(DISTINCT x)`, `SUM`, `AVG`, `MIN`, `MAX`,
/// `ARRAY_AGG`.
fn every_acc(host: &mut TestHost, slot: u16) -> Box<[AggSpec]> {
    let x = host.expr(E::Slot(slot));
    vec![
        AggSpec::CountRows,
        AggSpec::Count {
            arg: x,
            distinct: false,
        },
        AggSpec::Count {
            arg: x,
            distinct: true,
        },
        AggSpec::Sum(x),
        AggSpec::Avg(x),
        AggSpec::Min(x),
        AggSpec::Max(x),
        AggSpec::ArrayAgg {
            arg: x,
            elem: ValueType::Int,
        },
    ]
    .into_boxed_slice()
}

// ── Aggregate ─────────────────────────────────────────────────────────────

#[test]
fn empty_input_without_grouping_gives_one_row_of_zero_counts_and_nulls() {
    let env = env();
    let mut host = TestHost::default();
    let aggs = every_acc(&mut host, 1);
    let plan = OpSpec::Aggregate {
        input: Box::new(input(&mut host, 1)),
        keys: Box::new([]),
        aggs,
        width: 8,
    };
    let rows = run(&env.db, &host, &plan, vec![table(vec![])]);
    assert_eq!(
        exact(&rows),
        exact(&[vec![Int(0), Int(0), Int(0), Null, Null, Null, Null, Null]])
    );
    // A NULL list is an empty input too.
    let rows = run(&env.db, &host, &plan, vec![Null]);
    assert_eq!(rows.len(), 1);
}

#[test]
fn empty_input_with_grouping_gives_no_rows() {
    let env = env();
    let mut host = TestHost::default();
    let key = host.expr(E::Slot(1));
    let aggs = every_acc(&mut host, 1);
    let plan = OpSpec::Aggregate {
        input: Box::new(input(&mut host, 1)),
        keys: Box::new([key]),
        aggs,
        width: 9,
    };
    assert!(run(&env.db, &host, &plan, vec![table(vec![])]).is_empty());
}

#[test]
fn count_forms_on_nodes_count_identities() {
    let env = env();
    let mut host = TestHost::default();
    let aggs = every_acc(&mut host, 1);
    let plan = OpSpec::Aggregate {
        input: Box::new(input(&mut host, 1)),
        keys: Box::new([]),
        aggs: aggs[..3].into(),
        width: 3,
    };
    // Collections 1 and 2 both hold sequence 7: two nodes, not one.
    let rows = vec![
        vec![node(1, 7)],
        vec![node(2, 7)],
        vec![Null],
        vec![node(1, 7)],
        vec![node(1, 8)],
        vec![Null],
    ];
    let got = run(&env.db, &host, &plan, vec![table(rows)]);
    assert_eq!(exact(&got), exact(&[vec![Int(6), Int(4), Int(3)]]));
}

#[test]
fn count_forms_on_scalars_count_values() {
    let env = env();
    let mut host = TestHost::default();
    let aggs = every_acc(&mut host, 1);
    let plan = OpSpec::Aggregate {
        input: Box::new(input(&mut host, 1)),
        keys: Box::new([]),
        aggs: aggs[..3].into(),
        width: 3,
    };
    // 1 and 1.0 are one value; "1" is another.
    let rows = vec![
        vec![Int(1)],
        vec![Float(1.0)],
        vec![text("1")],
        vec![Null],
        vec![Int(2)],
    ];
    let got = run(&env.db, &host, &plan, vec![table(rows)]);
    assert_eq!(exact(&got), exact(&[vec![Int(5), Int(4), Int(3)]]));
}

#[test]
fn grouped_accumulators_group_by_identity_and_value_null_with_null() {
    let env = env();
    let mut host = TestHost::default();
    let key = host.expr(E::Slot(1));
    let aggs = every_acc(&mut host, 2);
    let plan = OpSpec::Aggregate {
        input: Box::new(input(&mut host, 2)),
        keys: Box::new([key]),
        aggs,
        width: 9,
    };
    let rows = vec![
        vec![node(1, 7), Int(3)],
        vec![Null, Int(5)],
        vec![node(2, 7), Null],
        vec![node(1, 7), Int(1)],
        vec![Null, Null],
        vec![node(1, 7), Int(3)],
    ];
    let got = run(&env.db, &host, &plan, vec![table(rows)]);
    // ARRAY_AGG's list carries the element type the plan names.
    let arr = |items: Vec<BindingValue>| {
        BindingValue::List(ListRef {
            items: items.into(),
            elem: ValueType::Int,
        })
    };
    // Groups come out in first-seen order.
    assert_eq!(
        exact(&got),
        exact(&[
            vec![
                node(1, 7),
                Int(3),
                Int(3),
                Int(2),
                Int(7),
                Float(7.0 / 3.0),
                Int(1),
                Int(3),
                arr(vec![Int(3), Int(1), Int(3)]),
            ],
            vec![
                Null,
                Int(2),
                Int(1),
                Int(1),
                Int(5),
                Float(5.0),
                Int(5),
                Int(5),
                arr(vec![Int(5), Null]),
            ],
            vec![
                node(2, 7),
                Int(1),
                Int(0),
                Int(0),
                Null,
                Null,
                Null,
                Null,
                arr(vec![Null]),
            ],
        ])
    );
}

#[test]
fn sum_and_avg_mix_ints_and_floats_and_refuse_overflow() {
    let env = env();
    let mut host = TestHost::default();
    let x = host.expr(E::Slot(1));
    let plan = OpSpec::Aggregate {
        input: Box::new(input(&mut host, 1)),
        keys: Box::new([]),
        aggs: Box::new([AggSpec::Sum(x), AggSpec::Avg(x)]),
        width: 2,
    };
    let got = run(
        &env.db,
        &host,
        &plan,
        vec![table(vec![vec![Int(1)], vec![Float(0.5)], vec![Int(2)]])],
    );
    assert_eq!(exact(&got), exact(&[vec![Float(3.5), Float(3.5 / 3.0)]]));
    let over = run_with(
        &env.db,
        &host,
        &plan,
        vec![table(vec![vec![Int(i64::MAX)], vec![Int(1)]])],
        8192,
        GqlBudget::unlimited(),
    );
    is_invalid(over);
}

// ── Sort ──────────────────────────────────────────────────────────────────

/// Rows `(a, b, tag)`: sorted by `a` ASC, then `b` DESC; `tag` shows which
/// input row a result came from.
fn sort_rows() -> Vec<Vec<BindingValue>> {
    vec![
        vec![Int(2), Int(1), text("r0")],
        vec![Null, Int(1), text("r1")],
        vec![Int(1), Null, text("r2")],
        vec![Int(1), Int(5), text("r3")],
        vec![Int(2), Int(1), text("r4")],
        vec![Int(1), Int(5), text("r5")],
        vec![Null, Null, text("r6")],
        vec![Int(2), Int(9), text("r7")],
        vec![Int(1), Int(2), text("r8")],
    ]
}

fn sort_plan(host: &mut TestHost) -> OpSpec {
    let (a, b) = (host.expr(E::Slot(1)), host.expr(E::Slot(2)));
    let sorted = OpSpec::Sort {
        input: Box::new(input(host, 3)),
        keys: Box::new([
            SortKey {
                expr: a,
                descending: false,
            },
            SortKey {
                expr: b,
                descending: true,
            },
        ]),
    };
    columns(host, sorted, 3)
}

fn tags(rows: &[Vec<BindingValue>]) -> Vec<String> {
    rows.iter()
        .map(|r| match &r[2] {
            BindingValue::Text(t) => t.to_string(),
            other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn sort_is_multi_key_with_postgres_null_placement_and_stable_ties() {
    let env = env();
    let mut host = TestHost::default();
    let plan = sort_plan(&mut host);
    let got = run(&env.db, &host, &plan, vec![table(sort_rows())]);
    // a ASC NULLS LAST; within one a, b DESC NULLS FIRST; ties in input
    // order (r3 before r5, r0 before r4). Inside the null-a group r6 comes
    // before r1 because b's null comes first descending.
    assert_eq!(
        tags(&got),
        ["r2", "r3", "r5", "r8", "r7", "r0", "r4", "r6", "r1"]
    );
}

#[test]
fn sort_top_k_under_a_limit_equals_the_full_sort_and_holds_less() {
    let env = env();
    let mut host = TestHost::default();
    let (a, b) = (host.expr(E::Slot(1)), host.expr(E::Slot(2)));
    // The top-k form: the Sort DIRECTLY under the Page.
    let sorted = OpSpec::Sort {
        input: Box::new(input(&mut host, 3)),
        keys: Box::new([
            SortKey {
                expr: a,
                descending: false,
            },
            SortKey {
                expr: b,
                descending: true,
            },
        ]),
    };
    let top = |offset: u64, limit: u64| OpSpec::Page {
        input: Box::new(sorted.clone()),
        offset: Some(CountExpr::Lit(offset)),
        limit: Some(CountExpr::Lit(limit)),
    };
    let everything = run(&env.db, &host, &sorted, vec![table(sort_rows())]);
    for (offset, limit) in [(0, 1), (0, 3), (2, 3), (1, 20), (8, 1), (9, 5), (0, 9)] {
        let got = run(&env.db, &host, &top(offset, limit), vec![table(sort_rows())]);
        let want: Vec<_> = everything
            .iter()
            .skip(offset as usize)
            .take(limit as usize)
            .cloned()
            .collect();
        assert_eq!(exact(&got), exact(&want), "offset {offset} limit {limit}");
    }
    let held = |plan: &OpSpec| {
        run_with(&env.db, &host, plan, vec![table(sort_rows())], 8192, GqlBudget::unlimited())
            .unwrap()
            .1
            .iter()
            .map(|w| w.sort_bytes)
            .max()
            .unwrap()
    };
    // Keeping two rows peaks at three (one arriving, one evicted); the full
    // sort holds all nine.
    let (two, all) = (held(&top(1, 1)), held(&sorted));
    assert!(two > 0 && two * 2 < all, "top-2 held {two}, full sort held {all}");
}

#[test]
fn sort_refuses_at_its_cap_by_name() {
    let env = env();
    let mut host = TestHost::default();
    let plan = sort_plan(&mut host);
    let budget = GqlBudget {
        sort_bytes: 64,
        ..GqlBudget::unlimited()
    };
    refused(
        run_with(&env.db, &host, &plan, vec![table(sort_rows())], 8192, budget),
        WorkResource::SortBytes,
        64,
    );
}

// ── Distinct ──────────────────────────────────────────────────────────────

#[test]
fn distinct_keeps_the_first_of_each_row_by_identity_and_value() {
    let env = env();
    let mut host = TestHost::default();
    let plan = {
        let cols = input(&mut host, 2);
        OpSpec::Distinct {
            input: Box::new(columns(&mut host, cols, 2)),
        }
    };
    let rows = vec![
        vec![node(1, 7), Int(1)],
        vec![node(2, 7), Int(1)],
        vec![node(1, 7), Float(1.0)],
        vec![Null, Null],
        vec![node(1, 7), Int(2)],
        vec![Null, Null],
        vec![node(2, 7), Int(1)],
    ];
    let got = run(&env.db, &host, &plan, vec![table(rows)]);
    assert_eq!(
        exact(&got),
        exact(&[
            vec![node(1, 7), Int(1)],
            vec![node(2, 7), Int(1)],
            vec![Null, Null],
            vec![node(1, 7), Int(2)],
        ])
    );
}

#[test]
fn distinct_refuses_at_its_cap_by_name() {
    let env = env();
    let mut host = TestHost::default();
    let cols = input(&mut host, 1);
    let plan = OpSpec::Distinct {
        input: Box::new(columns(&mut host, cols, 1)),
    };
    let rows = (0..100).map(|i| vec![Int(i)]).collect();
    let budget = GqlBudget {
        sort_bytes: 200,
        ..GqlBudget::unlimited()
    };
    refused(
        run_with(&env.db, &host, &plan, vec![table(rows)], 8192, budget),
        WorkResource::SortBytes,
        200,
    );
}

// ── Page ──────────────────────────────────────────────────────────────────

fn paged(host: &mut TestHost, offset: Option<CountExpr>, limit: Option<CountExpr>) -> OpSpec {
    let cols = input(host, 1);
    OpSpec::Page {
        input: Box::new(columns(host, cols, 1)),
        offset,
        limit,
    }
}

fn five() -> BindingValue {
    table((0..5).map(|i| vec![Int(i)]).collect())
}

fn ints(rows: &[Vec<BindingValue>]) -> Vec<i64> {
    rows.iter()
        .map(|r| match r[0] {
            Int(i) => i,
            ref other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn page_offset_and_limit_at_their_edges() {
    let env = env();
    let mut host = TestHost::default();
    let lit = |n: u64| Some(CountExpr::Lit(n));
    let cases: [(Option<CountExpr>, Option<CountExpr>, &[i64]); 8] = [
        (None, None, &[0, 1, 2, 3, 4]),
        (lit(0), None, &[0, 1, 2, 3, 4]),
        (lit(2), None, &[2, 3, 4]),
        (lit(5), None, &[]),
        (lit(9), lit(1), &[]),
        (None, lit(2), &[0, 1]),
        (lit(3), lit(9), &[3, 4]),
        (lit(i64::MAX as u64), lit(i64::MAX as u64), &[]),
    ];
    for (offset, limit, want) in cases {
        let plan = paged(&mut host, offset, limit);
        let got = run(&env.db, &host, &plan, vec![five()]);
        assert_eq!(ints(&got), want, "{offset:?} {limit:?}");
    }
}

#[test]
fn page_limit_zero_returns_nothing_and_pulls_nothing() {
    let env = env();
    let mut host = TestHost::default();
    let plan = paged(&mut host, None, Some(CountExpr::Lit(0)));
    let (rows, work) = run_with(&env.db, &host, &plan, vec![five()], 8, GqlBudget::unlimited())
        .unwrap();
    assert!(rows.is_empty());
    assert_eq!(work.iter().map(|w| w.binding_rows).sum::<u64>(), 0);
}

#[test]
fn page_counts_come_from_parameters_in_range() {
    let env = env();
    let mut host = TestHost::default();
    let plan = paged(
        &mut host,
        Some(CountExpr::Param(1)),
        Some(CountExpr::Param(2)),
    );
    let got = run(&env.db, &host, &plan, vec![five(), Int(1), Int(2)]);
    assert_eq!(ints(&got), [1, 2]);
    // Negative, NULL, non-integer and missing counts are refused when the
    // execution opens.
    for bad in [Int(-1), Null, Float(1.0), text("1")] {
        is_invalid(GqlCursor::open(&env.db, &host, &plan, vec![five(), Int(0), bad]).map(|_| ()));
    }
    is_invalid(GqlCursor::open(&env.db, &host, &plan, vec![five(), Int(0)]).map(|_| ()));
    let past = paged(&mut host, None, Some(CountExpr::Lit(i64::MAX as u64 + 1)));
    is_invalid(GqlCursor::open(&env.db, &host, &past, vec![five()]).map(|_| ()));
}

// ── Unnest ────────────────────────────────────────────────────────────────

#[test]
fn unnest_of_null_and_empty_lists_gives_no_rows_and_a_non_list_is_an_error() {
    let env = env();
    let mut host = TestHost::default();
    let cols = input(&mut host, 1);
    let plan = columns(&mut host, cols, 1);
    assert!(run(&env.db, &host, &plan, vec![Null]).is_empty());
    assert!(run(&env.db, &host, &plan, vec![list(vec![])]).is_empty());
    assert_eq!(ints(&run(&env.db, &host, &plan, vec![five()])), [0, 1, 2, 3, 4]);
    is_invalid(run_with(&env.db, &host, &plan, vec![Int(3)], 8, GqlBudget::unlimited()));
    // A null ELEMENT is a row, as in the list.
    let with_null = table(vec![vec![Int(1)], vec![Null]]);
    let got = run(&env.db, &host, &plan, vec![with_null]);
    assert_eq!(exact(&got), exact(&[vec![Int(1)], vec![Null]]));
}

#[test]
fn unnest_refuses_at_its_caps_by_name() {
    let env = env();
    let mut host = TestHost::default();
    let cols = input(&mut host, 1);
    let plan = columns(&mut host, cols, 1);
    let rows = BindingValue::List(ListRef {
        items: Arc::from((0..50).map(|i| list(vec![Int(i)])).collect::<Vec<_>>()),
        elem: ValueType::Unknown,
    });
    let small_list = GqlBudget {
        list_bytes: 100,
        ..GqlBudget::unlimited()
    };
    refused(
        run_with(&env.db, &host, &plan, vec![rows.clone()], 8192, small_list),
        WorkResource::ListBytes,
        100,
    );
    let few_rows = GqlBudget {
        binding_rows: 10,
        ..GqlBudget::unlimited()
    };
    refused(
        run_with(&env.db, &host, &plan, vec![rows], 8192, few_rows),
        WorkResource::BindingRows,
        10,
    );
}

// ── Aggregate caps ────────────────────────────────────────────────────────

#[test]
fn aggregate_refuses_at_each_cap_by_name() {
    let env = env();
    let mut host = TestHost::default();
    let key = host.expr(E::Slot(1));
    let x = host.expr(E::Slot(1));
    let grouped = |aggs: Box<[AggSpec]>, input: OpSpec| OpSpec::Aggregate {
        input: Box::new(input),
        keys: Box::new([key]),
        aggs,
        width: 2,
    };
    let rows: Vec<Vec<BindingValue>> = (0..100).map(|i| vec![Int(i)]).collect();
    let params = || vec![table(rows.clone())];
    let counted = grouped(Box::new([AggSpec::CountRows]), input(&mut host, 1));
    // Group keys: sort_bytes.
    refused(
        run_with(&env.db, &host, &counted, params(), 8192, GqlBudget {
            sort_bytes: 500,
            ..GqlBudget::unlimited()
        }),
        WorkResource::SortBytes,
        500,
    );
    // Groups: the existing resource.
    let mut few_groups = GqlBudget::unlimited();
    few_groups.base.groups = 10;
    refused(
        run_with(&env.db, &host, &counted, params(), 8192, few_groups),
        WorkResource::Groups,
        10,
    );
    // Output rows: binding_rows (the input `FOR` spends 100 of them first).
    refused(
        run_with(&env.db, &host, &counted, params(), 8192, GqlBudget {
            binding_rows: 101,
            ..GqlBudget::unlimited()
        }),
        WorkResource::BindingRows,
        101,
    );
    // ARRAY_AGG: list_bytes, over one group. The input `FOR` holds its list
    // as list_bytes too, so the cap sits just above that.
    let whole = OpSpec::Aggregate {
        input: Box::new(input(&mut host, 1)),
        keys: Box::new([]),
        aggs: Box::new([AggSpec::ArrayAgg {
            arg: x,
            elem: ValueType::Int,
        }]),
        width: 1,
    };
    let for_only = OpSpec::Aggregate {
        input: Box::new(input(&mut host, 1)),
        keys: Box::new([]),
        aggs: Box::new([AggSpec::CountRows]),
        width: 1,
    };
    let for_bytes = run_with(&env.db, &host, &for_only, params(), 8192, GqlBudget::unlimited())
        .unwrap()
        .1[0]
        .list_bytes;
    let cap = for_bytes + 200;
    refused(
        run_with(&env.db, &host, &whole, params(), 8192, GqlBudget {
            list_bytes: cap,
            ..GqlBudget::unlimited()
        }),
        WorkResource::ListBytes,
        cap,
    );
    // COUNT(DISTINCT x): the distinct set is sort_bytes.
    let distinct = OpSpec::Aggregate {
        input: Box::new(input(&mut host, 1)),
        keys: Box::new([]),
        aggs: Box::new([AggSpec::Count {
            arg: x,
            distinct: true,
        }]),
        width: 1,
    };
    refused(
        run_with(&env.db, &host, &distinct, params(), 8192, GqlBudget {
            sort_bytes: 500,
            ..GqlBudget::unlimited()
        }),
        WorkResource::SortBytes,
        500,
    );
}

// ── paging, re-charging and poison ────────────────────────────────────────

#[test]
fn held_state_is_recharged_at_every_page_start() {
    let env = env();
    let mut host = TestHost::default();
    let plan = sort_plan(&mut host);
    let mut cursor = GqlCursor::open(&env.db, &host, &plan, vec![table(sort_rows())]).unwrap();
    let first = cursor.next_page(1, GqlBudget::unlimited(), never).unwrap();
    assert_eq!(first.rows.len(), 1);
    // The sort still holds eight rows: the next page pays for them before
    // it hands out one more.
    let second = cursor.next_page(1, GqlBudget::unlimited(), never).unwrap();
    assert!(second.work.sort_bytes > 0);
    assert!(second.work.sort_bytes < first.work.sort_bytes);
    let tight = GqlBudget {
        sort_bytes: 16,
        ..GqlBudget::unlimited()
    };
    refused(cursor.next_page(1, tight, never), WorkResource::SortBytes, 16);
    // Poisoned: the same refusal again, even under a generous budget, and
    // never `done`.
    refused(
        cursor.next_page(1, GqlBudget::unlimited(), never),
        WorkResource::SortBytes,
        16,
    );
}

#[test]
fn a_cancelled_blocking_page_poisons_the_cursor() {
    let env = env();
    let mut host = TestHost::default();
    let plan = sort_plan(&mut host);
    let mut cursor = GqlCursor::open(&env.db, &host, &plan, vec![table(sort_rows())]).unwrap();
    let err = cursor.next_page(4, GqlBudget::unlimited(), || true).unwrap_err();
    assert!(matches!(err, QueryError::Cancelled));
    let again = cursor.next_page(4, GqlBudget::unlimited(), never).unwrap_err();
    assert!(matches!(again, QueryError::Cancelled));
}

#[test]
fn paging_equals_one_shot() {
    let env = env();
    let mut host = TestHost::default();
    // FOR -> LET -> GROUP BY a (COUNT, SUM, ARRAY_AGG) -> DISTINCT -> ORDER
    // BY count DESC, a ASC -> OFFSET 1 LIMIT 6; and the same without the
    // page, and a plain DISTINCT over the input.
    let key = host.expr(E::Slot(1));
    let b = host.expr(E::Slot(2));
    let grouped = OpSpec::Aggregate {
        input: Box::new(input(&mut host, 2)),
        keys: Box::new([key]),
        aggs: Box::new([
            AggSpec::CountRows,
            AggSpec::Sum(b),
            AggSpec::ArrayAgg {
                arg: b,
                elem: ValueType::Int,
            },
        ]),
        width: 4,
    };
    let (count, a) = (host.expr(E::Slot(1)), host.expr(E::Slot(0)));
    let sorted = OpSpec::Sort {
        input: Box::new(OpSpec::Distinct {
            input: Box::new(grouped),
        }),
        keys: Box::new([
            SortKey {
                expr: count,
                descending: true,
            },
            SortKey {
                expr: a,
                descending: false,
            },
        ]),
    };
    let paged = OpSpec::Page {
        input: Box::new(sorted.clone()),
        offset: Some(CountExpr::Lit(1)),
        limit: Some(CountExpr::Lit(6)),
    };
    let plain = {
        let cols = input(&mut host, 2);
        OpSpec::Distinct {
            input: Box::new(columns(&mut host, cols, 2)),
        }
    };
    let rows: Vec<Vec<BindingValue>> = (0..60)
        .map(|i: i64| {
            let a = if i % 11 == 0 { Null } else { Int(i % 7) };
            vec![a, Int(i % 5)]
        })
        .collect();
    for plan in [&sorted, &paged, &plain] {
        let one_shot = run(&env.db, &host, plan, vec![table(rows.clone())]);
        assert!(!one_shot.is_empty());
        for page_rows in [1, 2, 3, 7] {
            let (got, _) = run_with(
                &env.db,
                &host,
                plan,
                vec![table(rows.clone())],
                page_rows,
                GqlBudget::unlimited(),
            )
            .unwrap();
            assert_eq!(exact(&got), exact(&one_shot), "page_rows {page_rows}");
        }
    }
}
