//! `OptionalApply`, the operator of `OPTIONAL MATCH`
//! (`docs/lang/GQL_PROFILE_DESIGN.md` §3.2, owner answer Q3).
//!
//! What is at risk, and the test that pins it:
//!
//! * a LEFT OUTER JOIN per input row: every row the inner side gives for it,
//!   or -- when the inner side gives none, whether it found nothing or its
//!   predicate rejected everything it found -- the input row ONCE with the
//!   introduced slots `NULL`; an input row is never dropped and the input
//!   order is kept (`each_input_row_gives_its_matches_or_one_null_row`);
//! * the inner side restarts from each input row, through the `Argument`
//!   leaf, and holds nothing from the row before
//!   (`each_input_row_gives_its_matches_or_one_null_row`);
//! * the pages of one cursor concatenate to the one-shot answer, even when
//!   a page ends in the middle of one input row's matches
//!   (`paging_equals_one_shot`);
//! * the null row is charged as a row (`the_null_row_is_charged_as_a_binding_row`);
//! * a plan that misuses the operator is refused when the cursor opens,
//!   never run: an `Argument` leaf outside an inner side, an `Argument`
//!   whose width is not the input's, a blocking operator inside an inner
//!   side, and an introduced slot outside the row
//!   (`a_malformed_optional_plan_is_refused_at_open`).
//!
//! The rows come from `FOR` over a parameter list of tuples, so no graph is
//! needed: the inner side unnests a list the input row carries and keeps
//! its `true` elements. [`TestHost`] is a minimal [`GqlHost`].

use sekejap_core::collections::gql::{
    BindingRow, BindingValue, EvalCx, ExprId, GqlBudget, GqlCursor, GqlHost, GqlWork, ListRef,
    OpSpec, SeedId, SlotId, SortKey, Truth, ValueType,
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
        Ok(None)
    }
}

// ── values and plans ──────────────────────────────────────────────────────

use BindingValue::{Bool, Null};

fn text(s: &str) -> BindingValue {
    BindingValue::Text(s.into())
}

fn list(items: Vec<BindingValue>) -> BindingValue {
    BindingValue::List(ListRef {
        items: items.into(),
        elem: ValueType::Unknown,
    })
}

/// The input: one tuple `[key, list]` per row. Rows are 4 slots wide: 0 the
/// tuple, 1 the key, 2 the list, 3 the element the inner side introduces.
fn table() -> BindingValue {
    list(vec![
        // Two of three elements hold: two rows.
        list(vec![
            text("a"),
            list(vec![Bool(true), Bool(false), Bool(true)]),
        ]),
        // Nothing to find: the null row.
        list(vec![text("b"), list(vec![])]),
        // Found, but the predicate rejects everything: the null row.
        list(vec![text("c"), list(vec![Bool(false), Null])]),
        // A NULL list: nothing to find.
        list(vec![text("d"), Null]),
        // One hit.
        list(vec![text("e"), list(vec![Bool(true)])]),
    ])
}

const WIDTH: u16 = 4;

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

/// The inner side: `FOR x IN xs FILTER x`, from the input row.
fn inner(host: &mut TestHost) -> OpSpec {
    let xs = host.expr(E::Slot(2));
    let x = host.expr(E::Slot(3));
    OpSpec::Filter {
        input: Box::new(OpSpec::Unnest {
            input: Box::new(OpSpec::Argument { width: WIDTH }),
            list: xs,
            out: SlotId(3),
        }),
        predicate: x,
    }
}

fn optional(host: &mut TestHost) -> OpSpec {
    OpSpec::OptionalApply {
        input: Box::new(input(host)),
        inner: Box::new(inner(host)),
        introduced: Box::new([SlotId(3)]),
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
    let db = Database::create(dir.path().join("o.sekejap"), cfg()).unwrap();
    Env { _dir: dir, db }
}

/// Every row of one cursor in pages of `page_rows` -- key and element only
/// -- and each page's work.
fn run_with(
    db: &Database,
    host: &TestHost,
    plan: &OpSpec,
    page_rows: usize,
) -> QueryResult<(Vec<(BindingValue, BindingValue)>, Vec<GqlWork>)> {
    let mut cursor = GqlCursor::open(db, host, plan, vec![table()])?;
    let (mut rows, mut work) = (Vec::new(), Vec::new());
    loop {
        let page = cursor.next_page(page_rows, GqlBudget::unlimited(), never)?;
        assert!(page.rows.len() <= page_rows);
        rows.extend(
            page.rows
                .into_iter()
                .map(|r| (r.get(SlotId(1)).clone(), r.get(SlotId(3)).clone())),
        );
        work.push(page.work);
        if page.done {
            return Ok((rows, work));
        }
    }
}

fn expected() -> Vec<(BindingValue, BindingValue)> {
    vec![
        (text("a"), Bool(true)),
        (text("a"), Bool(true)),
        (text("b"), Null),
        (text("c"), Null),
        (text("d"), Null),
        (text("e"), Bool(true)),
    ]
}

// ── semantics ─────────────────────────────────────────────────────────────

#[test]
fn each_input_row_gives_its_matches_or_one_null_row() {
    let env = env();
    let mut host = TestHost::default();
    let plan = optional(&mut host);
    let (rows, _) = run_with(&env.db, &host, &plan, 8192).unwrap();
    assert_eq!(format!("{rows:?}"), format!("{:?}", expected()));
}

#[test]
fn paging_equals_one_shot() {
    let env = env();
    let mut host = TestHost::default();
    let plan = optional(&mut host);
    for page_rows in [1, 2, 3, 5] {
        let (rows, work) = run_with(&env.db, &host, &plan, page_rows).unwrap();
        assert_eq!(
            format!("{rows:?}"),
            format!("{:?}", expected()),
            "pages of {page_rows}"
        );
        assert!(work.len() > 1, "pages of {page_rows} took one page");
    }
}

#[test]
fn the_null_row_is_charged_as_a_binding_row() {
    let env = env();
    let mut host = TestHost::default();
    let plan = optional(&mut host);
    let (_, work) = run_with(&env.db, &host, &plan, 8192).unwrap();
    // The outer FOR makes 5 rows and the inner FOR 6 (a: 3, c: 2, e: 1);
    // the three null rows (b, c, d) are 3 more.
    let charged: u64 = work.iter().map(|w| w.binding_rows).sum();
    assert_eq!(charged, 5 + 6 + 3);
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
fn a_malformed_optional_plan_is_refused_at_open() {
    let env = env();
    let mut host = TestHost::default();

    // An Argument leaf with no OptionalApply above it.
    is_invalid(
        &env,
        &host,
        &OpSpec::Argument { width: WIDTH },
        "a bare Argument",
    );
    let plan = OpSpec::Filter {
        input: Box::new(OpSpec::Argument { width: WIDTH }),
        predicate: host.expr(E::Slot(0)),
    };
    is_invalid(&env, &host, &plan, "an Argument under a Filter only");
    // An Argument in the INPUT of an OptionalApply is not its inner side.
    let plan = OpSpec::OptionalApply {
        input: Box::new(OpSpec::Argument { width: WIDTH }),
        inner: Box::new(inner(&mut host)),
        introduced: Box::new([SlotId(3)]),
    };
    is_invalid(&env, &host, &plan, "an Argument as the input");

    // An Argument narrower than the input row.
    let xs = host.expr(E::Slot(2));
    let plan = OpSpec::OptionalApply {
        input: Box::new(input(&mut host)),
        inner: Box::new(OpSpec::Unnest {
            input: Box::new(OpSpec::Argument { width: WIDTH - 1 }),
            list: xs,
            out: SlotId(2),
        }),
        introduced: Box::new([SlotId(2)]),
    };
    is_invalid(&env, &host, &plan, "an Argument of another width");

    // A blocking operator inside the inner side: it would keep state from
    // one input row to the next.
    let key = host.expr(E::Slot(3));
    let plan = OpSpec::OptionalApply {
        input: Box::new(input(&mut host)),
        inner: Box::new(OpSpec::Sort {
            monotone_first: None,
            input: Box::new(inner(&mut host)),
            keys: Box::new([SortKey {
                expr: key,
                descending: false,
            }]),
        }),
        introduced: Box::new([SlotId(3)]),
    };
    is_invalid(&env, &host, &plan, "a Sort inside the inner side");

    // An inner side that never reads the input row.
    let plan = OpSpec::OptionalApply {
        input: Box::new(input(&mut host)),
        inner: Box::new(OpSpec::Unit { width: WIDTH }),
        introduced: Box::new([SlotId(3)]),
    };
    is_invalid(&env, &host, &plan, "a Unit as the inner side");

    // An introduced slot outside the row.
    let plan = OpSpec::OptionalApply {
        input: Box::new(input(&mut host)),
        inner: Box::new(inner(&mut host)),
        introduced: Box::new([SlotId(WIDTH)]),
    };
    is_invalid(&env, &host, &plan, "an introduced slot outside the row");
}
