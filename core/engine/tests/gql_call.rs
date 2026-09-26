//! `CallApply`, the operator of `CALL (imports) { ... }`
//! (`docs/lang/GQL_PROFILE_DESIGN_M5_M7.md` §2.4).
//!
//! What is at risk, and the test that pins it:
//!
//! * a LATERAL INNER join per input row: each row the body returns extends
//!   the input row once, written into the output slots; an input row whose
//!   body returns nothing is DROPPED; a body that aggregates with no key
//!   returns one row over empty input, so it keeps the row with `COUNT` 0
//!   (`each_body_row_extends_the_input_row_and_an_empty_body_drops_it`);
//! * the body may hold blocking operators, and they run per input row:
//!   `ORDER BY ... LIMIT 2` inside gives at most two rows per input row
//!   (`a_body_sorts_and_pages_per_input_row`);
//! * the body's tree is rebuilt per input row and what it held is given
//!   back when it is dropped: a body whose sort holds 60% of the
//!   `sort_bytes` ceiling runs over many input rows without a refusal, and
//!   a body cut short by its `LIMIT` gives back the list it still held
//!   (`a_body_sort_at_sixty_percent_of_the_cap_runs_over_many_rows`,
//!   `a_body_cut_short_gives_back_what_it_held`);
//! * each row out is charged as a binding row, and the pages of one cursor
//!   concatenate to the one-shot answer, even when a page ends inside one
//!   input row's body rows (`paging_equals_one_shot_and_rows_are_charged`);
//! * a plan that misuses the operator is refused when the cursor opens
//!   (`a_malformed_call_plan_is_refused_at_open`).

use sekejap_core::collections::gql::{
    AggSpec, BindingRow, BindingValue, CountExpr, EvalCx, ExprId, GqlBudget, GqlCursor, GqlHost,
    GqlWork, ListRef, OpSpec, SeedId, SlotId, SortKey, Truth, ValueType,
};
use sekejap_core::collections::{Database, PreparedQuery, QueryError, QueryResult};

mod common;
use common::cfg;

// ── the test host ─────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum E {
    Param(usize),
    Slot(u16),
    /// Item `i` of the list in a slot.
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
        unreachable!("these plans seed by list")
    }
}

// ── values and plans ──────────────────────────────────────────────────────

use BindingValue::{Int, Null};

fn text(s: &str) -> BindingValue {
    BindingValue::Text(s.into())
}

fn list(items: Vec<BindingValue>) -> BindingValue {
    BindingValue::List(ListRef {
        items: items.into(),
        elem: ValueType::Unknown,
    })
}

fn ints(items: &[i64]) -> BindingValue {
    list(items.iter().map(|i| Int(*i)).collect())
}

/// One tuple `[key, list]` per input row. Rows are 5 slots wide: 0 the
/// tuple, 1 the key, 2 the list, 3 the element the body introduces (its
/// slots live in the outer row), 4 the body's one output column.
fn table() -> BindingValue {
    list(vec![
        list(vec![text("a"), ints(&[3, 1, 2])]),
        // An empty body answer: the row is dropped.
        list(vec![text("b"), ints(&[])]),
        list(vec![text("c"), Null]),
        list(vec![text("d"), ints(&[5])]),
    ])
}

const WIDTH: u16 = 5;
const OUT: SlotId = SlotId(4);

/// `FOR t IN $0 LET key = t[0], xs = t[1]`.
fn input(host: &mut TestHost) -> OpSpec {
    let tuples = host.expr(E::Param(0));
    let key = host.expr(E::Item(0, 0));
    let xs = host.expr(E::Item(0, 1));
    OpSpec::Let {
        input: Box::new(OpSpec::Unnest {
            input: Box::new(OpSpec::Unit { width: WIDTH }),
            list: tuples,
            out: SlotId(0),
        }),
        assign: vec![(SlotId(1), key), (SlotId(2), xs)].into_boxed_slice(),
    }
}

/// `FOR x IN xs`, from the input row.
fn unnest(host: &mut TestHost) -> OpSpec {
    let xs = host.expr(E::Slot(2));
    OpSpec::Unnest {
        input: Box::new(OpSpec::Argument { width: WIDTH }),
        list: xs,
        out: SlotId(3),
    }
}

/// `RETURN x` over `body`: a one-column row.
fn returns_x(host: &mut TestHost, body: OpSpec) -> OpSpec {
    let x = host.expr(E::Slot(3));
    OpSpec::Project {
        input: Box::new(body),
        cols: Box::new([x]),
        width: 1,
    }
}

/// `ORDER BY x DESC`, then `LIMIT limit` when given.
fn sorted(host: &mut TestHost, body: OpSpec, limit: Option<u64>) -> OpSpec {
    let x = host.expr(E::Slot(3));
    let sort = OpSpec::Sort {
        input: Box::new(body),
        keys: Box::new([SortKey {
            expr: x,
            descending: true,
        }]),
    };
    match limit {
        None => sort,
        Some(limit) => OpSpec::Page {
            input: Box::new(sort),
            offset: None,
            limit: Some(CountExpr::Lit(limit)),
        },
    }
}

fn call(host: &mut TestHost, body: OpSpec) -> OpSpec {
    OpSpec::CallApply {
        input: Box::new(input(host)),
        inner: Box::new(body),
        outputs: Box::new([OUT]),
    }
}

/// `CALL (xs) { FOR x IN xs RETURN x }`.
fn plain(host: &mut TestHost) -> OpSpec {
    let body = unnest(host);
    let body = returns_x(host, body);
    call(host, body)
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
    let db = Database::create(dir.path().join("c.sekejap"), cfg()).unwrap();
    Env { _dir: dir, db }
}

/// Every row of one cursor in pages of `page_rows` -- key and output --
/// and each page's work.
fn run_with(
    db: &Database,
    host: &TestHost,
    plan: &OpSpec,
    params: Vec<BindingValue>,
    page_rows: usize,
    budget: GqlBudget,
) -> QueryResult<(Vec<(BindingValue, BindingValue)>, Vec<GqlWork>)> {
    let mut cursor = GqlCursor::open(db, host, plan, params)?;
    let (mut rows, mut work) = (Vec::new(), Vec::new());
    loop {
        let page = cursor.next_page(page_rows, budget, never)?;
        assert!(page.rows.len() <= page_rows);
        rows.extend(
            page.rows
                .into_iter()
                .map(|r| (r.get(SlotId(1)).clone(), r.get(OUT).clone())),
        );
        work.push(page.work);
        if page.done {
            return Ok((rows, work));
        }
    }
}

fn run(env: &Env, host: &TestHost, plan: &OpSpec) -> Vec<(BindingValue, BindingValue)> {
    run_with(
        &env.db,
        host,
        plan,
        vec![table()],
        8192,
        GqlBudget::unlimited(),
    )
    .unwrap()
    .0
}

fn pairs(expected: &[(&str, i64)]) -> String {
    let rows: Vec<(BindingValue, BindingValue)> = expected
        .iter()
        .map(|(key, x)| (text(key), Int(*x)))
        .collect();
    format!("{rows:?}")
}

// ── semantics ─────────────────────────────────────────────────────────────

#[test]
fn each_body_row_extends_the_input_row_and_an_empty_body_drops_it() {
    let env = env();
    let mut host = TestHost::default();
    let plan = plain(&mut host);
    assert_eq!(
        format!("{:?}", run(&env, &host, &plan)),
        pairs(&[("a", 3), ("a", 1), ("a", 2), ("d", 5)])
    );

    // `CALL (xs) { FOR x IN xs RETURN COUNT(*) }`: one row per input row.
    let mut host = TestHost::default();
    let body = OpSpec::Aggregate {
        input: Box::new(unnest(&mut host)),
        keys: Box::new([]),
        aggs: Box::new([AggSpec::CountRows]),
        width: 1,
    };
    let plan = call(&mut host, body);
    assert_eq!(
        format!("{:?}", run(&env, &host, &plan)),
        pairs(&[("a", 3), ("b", 0), ("c", 0), ("d", 1)])
    );
}

#[test]
fn a_body_sorts_and_pages_per_input_row() {
    let env = env();
    let mut host = TestHost::default();
    let body = unnest(&mut host);
    let body = sorted(&mut host, body, Some(2));
    let body = returns_x(&mut host, body);
    let plan = call(&mut host, body);
    assert_eq!(
        format!("{:?}", run(&env, &host, &plan)),
        pairs(&[("a", 3), ("a", 2), ("d", 5)])
    );
}

/// `rows` input rows, each with a key of the same length and the same list
/// of `len` integers, so every row's body holds the same bytes.
fn many(rows: usize, len: i64) -> BindingValue {
    list(
        (0..rows)
            .map(|i| {
                list(vec![
                    text(&format!("r{i:03}")),
                    ints(&(0..len).collect::<Vec<_>>()),
                ])
            })
            .collect(),
    )
}

#[test]
fn a_body_sort_at_sixty_percent_of_the_cap_runs_over_many_rows() {
    let env = env();
    let mut host = TestHost::default();
    let body = unnest(&mut host);
    let body = sorted(&mut host, body, None);
    let body = returns_x(&mut host, body);
    let plan = call(&mut host, body);
    // What ONE input row's sort holds.
    let (_, work) = run_with(
        &env.db,
        &host,
        &plan,
        vec![many(1, 20)],
        8192,
        GqlBudget::unlimited(),
    )
    .unwrap();
    let one = work[0].sort_bytes;
    assert!(one > 0);
    // A ceiling that one row's sort fills to 60%, over 30 rows in one page.
    let budget = GqlBudget {
        sort_bytes: one * 5 / 3,
        ..GqlBudget::unlimited()
    };
    let (rows, work) = run_with(&env.db, &host, &plan, vec![many(30, 20)], 8192, budget).unwrap();
    assert_eq!(rows.len(), 30 * 20);
    assert_eq!(work.len(), 1);
    assert_eq!(work[0].sort_bytes, one);
}

#[test]
fn a_body_cut_short_gives_back_what_it_held() {
    // `CALL (xs) { FOR x IN xs RETURN x LIMIT 1 }`: the body stops with the
    // whole list still held by its FOR.
    let env = env();
    let mut host = TestHost::default();
    let body = unnest(&mut host);
    let body = OpSpec::Page {
        input: Box::new(body),
        offset: None,
        limit: Some(CountExpr::Lit(1)),
    };
    let body = returns_x(&mut host, body);
    let plan = call(&mut host, body);
    let outer = many(40, 64);
    let inner = ints(&(0..64).collect::<Vec<_>>());
    let one_row = outer.held_bytes() + inner.held_bytes();
    let (rows, work) = run_with(
        &env.db,
        &host,
        &plan,
        vec![outer.clone()],
        8192,
        GqlBudget::unlimited(),
    )
    .unwrap();
    assert_eq!(rows.len(), 40);
    assert_eq!(work.len(), 1);
    assert_eq!(work[0].list_bytes, one_row);
    let budget = GqlBudget {
        list_bytes: one_row,
        ..GqlBudget::unlimited()
    };
    let (rows, _) = run_with(&env.db, &host, &plan, vec![outer], 8192, budget).unwrap();
    assert_eq!(rows.len(), 40);
}

#[test]
fn paging_equals_one_shot_and_rows_are_charged() {
    let env = env();
    let mut host = TestHost::default();
    let plan = plain(&mut host);
    let (one_shot, work) = run_with(
        &env.db,
        &host,
        &plan,
        vec![table()],
        8192,
        GqlBudget::unlimited(),
    )
    .unwrap();
    // The outer FOR's 4 rows, the body FOR's 4, and 4 rows out.
    let charged: u64 = work.iter().map(|w| w.binding_rows).sum();
    assert_eq!(charged, 4 + 4 + 4);

    // The same host holds both plans' expressions.
    let body = unnest(&mut host);
    let body = sorted(&mut host, body, None);
    let body = returns_x(&mut host, body);
    let sorted_plan = call(&mut host, body);
    let (sorted_one_shot, _) = run_with(
        &env.db,
        &host,
        &sorted_plan,
        vec![table()],
        8192,
        GqlBudget::unlimited(),
    )
    .unwrap();
    for (plan, expected) in [(&plan, &one_shot), (&sorted_plan, &sorted_one_shot)] {
        for page_rows in [1, 2, 3] {
            let (rows, work) = run_with(
                &env.db,
                &host,
                plan,
                vec![table()],
                page_rows,
                GqlBudget::unlimited(),
            )
            .unwrap();
            assert_eq!(
                format!("{rows:?}"),
                format!("{expected:?}"),
                "pages of {page_rows}"
            );
            assert!(work.len() > 1, "pages of {page_rows}");
        }
    }
    // A body's sort holding rows across a page boundary is charged to the
    // next page too.
    let (_, work) = run_with(
        &env.db,
        &host,
        &sorted_plan,
        vec![table()],
        1,
        GqlBudget::unlimited(),
    )
    .unwrap();
    assert!(work[1].sort_bytes > 0, "{work:?}");
}

// ── malformed plans ───────────────────────────────────────────────────────

fn is_invalid(env: &Env, host: &TestHost, plan: &OpSpec, what: &str) {
    let result = GqlCursor::open(&env.db, host, plan, vec![table()]);
    assert!(
        matches!(result, Err(QueryError::Database(_))),
        "{what}: expected an invalid-query error, got {:?}",
        result.err()
    );
}

#[test]
fn a_malformed_call_plan_is_refused_at_open() {
    let env = env();
    let mut host = TestHost::default();

    // A body row of one column, and two output slots.
    let body = unnest(&mut host);
    let body = returns_x(&mut host, body);
    let plan = OpSpec::CallApply {
        input: Box::new(input(&mut host)),
        inner: Box::new(body),
        outputs: Box::new([SlotId(3), OUT]),
    };
    is_invalid(&env, &host, &plan, "outputs wider than the body's rows");

    // An output slot outside the row.
    let body = unnest(&mut host);
    let body = returns_x(&mut host, body);
    let plan = OpSpec::CallApply {
        input: Box::new(input(&mut host)),
        inner: Box::new(body),
        outputs: Box::new([SlotId(WIDTH)]),
    };
    is_invalid(&env, &host, &plan, "an output slot outside the row");

    // One output slot named twice.
    let x = host.expr(E::Slot(3));
    let body = OpSpec::Project {
        input: Box::new(unnest(&mut host)),
        cols: Box::new([x, x]),
        width: 2,
    };
    let plan = OpSpec::CallApply {
        input: Box::new(input(&mut host)),
        inner: Box::new(body),
        outputs: Box::new([OUT, OUT]),
    };
    is_invalid(&env, &host, &plan, "an output slot named twice");

    // An Argument narrower than the input row.
    let xs = host.expr(E::Slot(2));
    let body = OpSpec::Unnest {
        input: Box::new(OpSpec::Argument { width: WIDTH - 1 }),
        list: xs,
        out: SlotId(3),
    };
    let body = returns_x(&mut host, body);
    let plan = call(&mut host, body);
    is_invalid(&env, &host, &plan, "an Argument of another width");

    // A union inside a body: a body is one stage.
    let body = unnest(&mut host);
    let body = returns_x(&mut host, body);
    let plan = call(
        &mut host,
        OpSpec::Union {
            branches: Box::new([body]),
            width: 1,
        },
    );
    is_invalid(&env, &host, &plan, "a Union inside a body");
}
