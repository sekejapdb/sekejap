//! `Union`, and `Buffered` with its `Replay` leaves: `UNION [ALL]`
//! (`docs/lang/GQL_PROFILE_DESIGN_M5_M7.md` §2.5, Q25).
//!
//! What is at risk, and the test that pins it:
//!
//! * `Union` gives the rows of each branch in turn, branch order then row
//!   order, keeping duplicates (`UNION ALL`); the existing `Distinct` over
//!   it removes duplicate whole rows (`UNION`)
//!   (`a_union_gives_each_branch_in_turn_and_distinct_removes_duplicates`);
//! * after `NEXT`, EVERY branch sees the whole incoming table: `Buffered`
//!   reads it once and each branch's `Replay` leaf hands it over from the
//!   start, so a branch that aggregates it counts every row
//!   (`every_branch_after_next_sees_the_whole_incoming_table`);
//! * the buffered table is charged as `sort_bytes`, held until the last
//!   branch has replayed it and charged again to each page before that, and
//!   refused past the ceiling by name
//!   (`the_buffered_table_is_charged_and_refused_past_the_cap`);
//! * the pages of one cursor concatenate to the one-shot answer
//!   (`paging_equals_one_shot`);
//! * a plan that misuses the operators is refused when the cursor opens
//!   (`a_malformed_union_plan_is_refused_at_open`).

use sekejap_core::collections::gql::{
    AggSpec, BindingRow, BindingValue, EvalCx, ExprId, GqlBudget, GqlCursor, GqlHost, GqlWork,
    ListRef, OpSpec, SeedId, SlotId, Truth, ValueType,
};
use sekejap_core::collections::{Database, PreparedQuery, QueryError, QueryResult, WorkResource};

mod common;
use common::cfg;

// ── the test host ─────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum E {
    Param(usize),
    Slot(u16),
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

use BindingValue::Int;

fn ints(items: &[i64]) -> BindingValue {
    BindingValue::List(ListRef {
        items: items.iter().map(|i| Int(*i)).collect::<Vec<_>>().into(),
        elem: ValueType::Int,
    })
}

/// `FOR x IN $param RETURN x`: a first-part branch, one column.
fn values(host: &mut TestHost, param: usize) -> OpSpec {
    let xs = host.expr(E::Param(param));
    let x = host.expr(E::Slot(0));
    OpSpec::Project {
        input: Box::new(OpSpec::Unnest {
            input: Box::new(OpSpec::Unit { width: 1 }),
            list: xs,
            out: SlotId(0),
        }),
        cols: Box::new([x]),
        width: 1,
    }
}

/// `RETURN COUNT(*)` over the replayed table.
fn count() -> OpSpec {
    OpSpec::Aggregate {
        input: Box::new(OpSpec::Replay { width: 1 }),
        keys: Box::new([]),
        aggs: Box::new([AggSpec::CountRows]),
        width: 1,
    }
}

/// `RETURN x` over the replayed table.
fn same(host: &mut TestHost) -> OpSpec {
    let x = host.expr(E::Slot(0));
    OpSpec::Project {
        input: Box::new(OpSpec::Replay { width: 1 }),
        cols: Box::new([x]),
        width: 1,
    }
}

/// `FOR x IN $0 RETURN x NEXT RETURN COUNT(*) UNION ALL RETURN x UNION ALL
/// RETURN COUNT(*)`.
fn after_next(host: &mut TestHost) -> OpSpec {
    let first = values(host, 0);
    let branches = [count(), same(host), count()];
    OpSpec::Buffered {
        input: Box::new(first),
        union: Box::new(OpSpec::Union {
            branches: Box::new(branches),
            width: 1,
        }),
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
    let db = Database::create(dir.path().join("u.sekejap"), cfg()).unwrap();
    Env { _dir: dir, db }
}

fn run_with(
    env: &Env,
    host: &TestHost,
    plan: &OpSpec,
    params: Vec<BindingValue>,
    page_rows: usize,
    budget: GqlBudget,
) -> QueryResult<(Vec<BindingValue>, Vec<GqlWork>)> {
    let mut cursor = GqlCursor::open(&env.db, host, plan, params)?;
    let (mut rows, mut work) = (Vec::new(), Vec::new());
    loop {
        let page = cursor.next_page(page_rows, budget, never)?;
        assert!(page.rows.len() <= page_rows);
        rows.extend(page.rows.into_iter().map(|r| r.get(SlotId(0)).clone()));
        work.push(page.work);
        if page.done {
            return Ok((rows, work));
        }
    }
}

fn run(env: &Env, host: &TestHost, plan: &OpSpec, params: Vec<BindingValue>) -> Vec<BindingValue> {
    run_with(env, host, plan, params, 8192, GqlBudget::unlimited())
        .unwrap()
        .0
}

fn of(items: &[i64]) -> String {
    format!("{:?}", items.iter().map(|i| Int(*i)).collect::<Vec<_>>())
}

// ── semantics ─────────────────────────────────────────────────────────────

#[test]
fn a_union_gives_each_branch_in_turn_and_distinct_removes_duplicates() {
    let env = env();
    let params = || vec![ints(&[2, 1, 2]), ints(&[3, 2])];
    let mut host = TestHost::default();
    let union = OpSpec::Union {
        branches: Box::new([values(&mut host, 0), values(&mut host, 1)]),
        width: 1,
    };
    assert_eq!(
        format!("{:?}", run(&env, &host, &union, params())),
        of(&[2, 1, 2, 3, 2])
    );
    let distinct = OpSpec::Distinct {
        input: Box::new(union),
    };
    assert_eq!(
        format!("{:?}", run(&env, &host, &distinct, params())),
        of(&[2, 1, 3])
    );
    // A branch with no rows gives nothing and the next one follows.
    assert_eq!(
        format!(
            "{:?}",
            run(&env, &host, &distinct, vec![ints(&[]), ints(&[4])])
        ),
        of(&[4])
    );
}

#[test]
fn every_branch_after_next_sees_the_whole_incoming_table() {
    let env = env();
    let mut host = TestHost::default();
    let plan = after_next(&mut host);
    assert_eq!(
        format!("{:?}", run(&env, &host, &plan, vec![ints(&[5, 6, 5])])),
        of(&[3, 5, 6, 5, 3])
    );
    // An empty incoming table: each COUNT is 0, the middle branch nothing.
    assert_eq!(
        format!("{:?}", run(&env, &host, &plan, vec![ints(&[])])),
        of(&[0, 0])
    );
}

#[test]
fn the_buffered_table_is_charged_and_refused_past_the_cap() {
    let env = env();
    let mut host = TestHost::default();
    // `... NEXT RETURN x UNION ALL RETURN x`: nothing else holds memory.
    let first = values(&mut host, 0);
    let plan = OpSpec::Buffered {
        input: Box::new(first),
        union: Box::new(OpSpec::Union {
            branches: Box::new([same(&mut host), same(&mut host)]),
            width: 1,
        }),
    };
    let incoming: Vec<i64> = (0..50).collect();
    let (_, work) = run_with(
        &env,
        &host,
        &plan,
        vec![ints(&incoming)],
        8192,
        GqlBudget::unlimited(),
    )
    .unwrap();
    let table = work[0].sort_bytes;
    // At least one slot per buffered row.
    assert!(
        table >= 50 * std::mem::size_of::<BindingValue>() as u64,
        "{table}"
    );
    let budget = GqlBudget {
        sort_bytes: table - 1,
        ..GqlBudget::unlimited()
    };
    let refused = run_with(&env, &host, &plan, vec![ints(&incoming)], 8192, budget);
    assert!(
        matches!(
            refused,
            Err(QueryError::BudgetExceeded {
                resource: WorkResource::SortBytes,
                ..
            })
        ),
        "{:?}",
        refused.map(|(rows, _)| rows.len())
    );
    // Held across pages: a page in the middle of the replay is charged the
    // whole table again.
    let (_, work) = run_with(
        &env,
        &host,
        &plan,
        vec![ints(&incoming)],
        10,
        GqlBudget::unlimited(),
    )
    .unwrap();
    assert!(work.len() > 2);
    assert_eq!(work[1].sort_bytes, table);
}

#[test]
fn paging_equals_one_shot() {
    let env = env();
    let mut host = TestHost::default();
    let plan = after_next(&mut host);
    let union = OpSpec::Distinct {
        input: Box::new(OpSpec::Union {
            branches: Box::new([values(&mut host, 0), values(&mut host, 1)]),
            width: 1,
        }),
    };
    for (plan, params) in [
        (&plan, vec![ints(&[5, 6, 5])]),
        (&union, vec![ints(&[2, 1, 2]), ints(&[3, 2])]),
    ] {
        let one_shot = run(&env, &host, plan, params.clone());
        for page_rows in [1, 2, 3] {
            let (rows, work) = run_with(
                &env,
                &host,
                plan,
                params.clone(),
                page_rows,
                GqlBudget::unlimited(),
            )
            .unwrap();
            assert_eq!(
                format!("{rows:?}"),
                format!("{one_shot:?}"),
                "pages of {page_rows}"
            );
            assert!(work.len() > 1, "pages of {page_rows}");
        }
    }
}

// ── malformed plans ───────────────────────────────────────────────────────

fn is_invalid(env: &Env, host: &TestHost, plan: &OpSpec, what: &str) {
    let result = GqlCursor::open(&env.db, host, plan, vec![ints(&[1]), ints(&[2])]);
    assert!(
        matches!(result, Err(QueryError::Database(_))),
        "{what}: expected an invalid-query error, got {:?}",
        result.err()
    );
}

#[test]
fn a_malformed_union_plan_is_refused_at_open() {
    let env = env();
    let mut host = TestHost::default();

    // A branch whose rows are not the union's width.
    let plan = OpSpec::Union {
        branches: Box::new([values(&mut host, 0), values(&mut host, 1)]),
        width: 2,
    };
    is_invalid(&env, &host, &plan, "a branch of another width");

    // A Replay with no Buffered above it.
    let plan = OpSpec::Union {
        branches: Box::new([same(&mut host)]),
        width: 1,
    };
    is_invalid(&env, &host, &plan, "a Replay outside a Buffered");
    is_invalid(&env, &host, &OpSpec::Replay { width: 1 }, "a bare Replay");

    // A Replay of another width than the incoming table.
    let first = values(&mut host, 0);
    let plan = OpSpec::Buffered {
        input: Box::new(first),
        union: Box::new(OpSpec::Union {
            branches: Box::new([OpSpec::Replay { width: 2 }]),
            width: 2,
        }),
    };
    is_invalid(&env, &host, &plan, "a Replay of another width");

    // A branch after NEXT that starts from a Unit, not the incoming table.
    let first = values(&mut host, 0);
    let plan = OpSpec::Buffered {
        input: Box::new(first),
        union: Box::new(OpSpec::Union {
            branches: Box::new([values(&mut host, 1)]),
            width: 1,
        }),
    };
    is_invalid(&env, &host, &plan, "a Unit inside a buffered branch");

    // A Buffered over something that is not a Union.
    let first = values(&mut host, 0);
    let plan = OpSpec::Buffered {
        input: Box::new(first),
        union: Box::new(same(&mut host)),
    };
    is_invalid(&env, &host, &plan, "a Buffered over no Union");

    // A union inside an inner side.
    let plan = OpSpec::OptionalApply {
        input: Box::new(values(&mut host, 0)),
        inner: Box::new(OpSpec::Union {
            branches: Box::new([OpSpec::Argument { width: 1 }]),
            width: 1,
        }),
        introduced: Box::new([]),
    };
    is_invalid(&env, &host, &plan, "a Union inside an OptionalApply");
}
