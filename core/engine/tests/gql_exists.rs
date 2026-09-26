//! `ExistsApply`, the operator of `EXISTS { }` and `NOT EXISTS { }`
//! (`docs/lang/GQL_PROFILE_DESIGN_M5_M7.md` §2.3).
//!
//! What is at risk, and the test that pins it:
//!
//! * the FILTER form keeps an input row when the inner side gives a row
//!   (`EXISTS`) or gives none (`NOT EXISTS`), and never multiplies it: an
//!   inner side with several matches still gives the row once
//!   (`the_filter_form_keeps_or_drops_each_row_once`);
//! * the MARK form keeps every row and writes whether a match exists into
//!   its slot, agreeing with the filter form row by row
//!   (`the_mark_form_writes_the_answer_and_agrees_with_the_filter_form`);
//! * the inner side stops at its FIRST row and is rebuilt for the next input
//!   row, so what it had not handed out never leaks into that row's answer
//!   (`the_filter_form_keeps_or_drops_each_row_once`: row `b` follows a row
//!   whose inner side stopped with rows left);
//! * what an inner side cut short still held is given back when it is
//!   dropped, so many input rows never add up to a refusal
//!   (`an_inner_side_cut_short_gives_back_what_it_held`);
//! * an existence test over a node with 1,000 incoming edges walks at most
//!   two adjacency turns (`an_existence_test_stops_at_the_first_edge`);
//! * the pages of one cursor concatenate to the one-shot answer
//!   (`paging_equals_one_shot`);
//! * a plan that misuses the operator is refused when the cursor opens
//!   (`a_malformed_exists_plan_is_refused_at_open`).
//!
//! Workload names are invented: a `site` collection, a `visitor`
//! collection, edge type `visits`.

use sekejap_core::collections::gql::{
    BindingRow, BindingValue, EvalCx, ExistsMode, ExprId, GqlBudget, GqlCursor, GqlHost, GqlWork,
    ListRef, OpSpec, SeedId, SeedSource, SlotId, SortKey, StepSpec, Target, Truth, ValueType,
};
use sekejap_core::collections::{
    Database, Direction, GraphContextId, PreparedQuery, QueryError, QueryResult,
};
use sekejap_core::Kind;
use serde_json::json;

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
        unreachable!("these plans seed by key or by list")
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

/// One tuple `[key, list]` per input row. Rows are 5 slots wide: 0 the
/// tuple, 1 the key, 2 the list, 3 the element the inner side introduces,
/// 4 the mark.
fn table() -> BindingValue {
    list(vec![
        // Two of three elements hold: EXISTS, and the row still comes once.
        // The inner side stops at the first with two elements left.
        list(vec![
            text("a"),
            list(vec![Bool(true), Bool(false), Bool(true)]),
        ]),
        // Nothing to find.
        list(vec![text("b"), list(vec![])]),
        // Found, but the predicate rejects everything.
        list(vec![text("c"), list(vec![Bool(false), Null])]),
        // A NULL list.
        list(vec![text("d"), Null]),
        // One hit, after a miss.
        list(vec![text("e"), list(vec![Bool(false), Bool(true)])]),
    ])
}

const WIDTH: u16 = 5;
const MARK: SlotId = SlotId(4);

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

fn exists(host: &mut TestHost, mode: ExistsMode) -> OpSpec {
    OpSpec::ExistsApply {
        input: Box::new(input(host)),
        inner: Box::new(inner(host)),
        mode,
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
    let db = Database::create(dir.path().join("e.sekejap"), cfg()).unwrap();
    Env { _dir: dir, db }
}

/// Every row of one cursor in pages of `page_rows` -- the key, the inner
/// slot and the mark -- and each page's work.
#[allow(clippy::type_complexity)]
fn run_with(
    db: &Database,
    host: &TestHost,
    plan: &OpSpec,
    params: Vec<BindingValue>,
    page_rows: usize,
    budget: GqlBudget,
) -> QueryResult<(
    Vec<(BindingValue, BindingValue, BindingValue)>,
    Vec<GqlWork>,
)> {
    let mut cursor = GqlCursor::open(db, host, plan, params)?;
    let (mut rows, mut work) = (Vec::new(), Vec::new());
    loop {
        let page = cursor.next_page(page_rows, budget, never)?;
        assert!(page.rows.len() <= page_rows);
        rows.extend(page.rows.into_iter().map(|r| {
            (
                r.get(SlotId(1)).clone(),
                r.get(SlotId(3)).clone(),
                r.get(MARK).clone(),
            )
        }));
        work.push(page.work);
        if page.done {
            return Ok((rows, work));
        }
    }
}

fn keys(rows: &[(BindingValue, BindingValue, BindingValue)]) -> Vec<BindingValue> {
    rows.iter().map(|(key, _, _)| key.clone()).collect()
}

// ── semantics ─────────────────────────────────────────────────────────────

#[test]
fn the_filter_form_keeps_or_drops_each_row_once() {
    let env = env();
    let mut host = TestHost::default();
    let plan = exists(&mut host, ExistsMode::Filter { negated: false });
    let (rows, _) = run_with(
        &env.db,
        &host,
        &plan,
        vec![table()],
        8192,
        GqlBudget::unlimited(),
    )
    .unwrap();
    assert_eq!(
        format!("{:?}", keys(&rows)),
        format!("{:?}", [text("a"), text("e")])
    );
    // The inner side's slots are not the answer: the row is the input row.
    assert!(rows.iter().all(|(_, x, _)| matches!(x, Null)), "{rows:?}");

    let mut host = TestHost::default();
    let plan = exists(&mut host, ExistsMode::Filter { negated: true });
    let (rows, _) = run_with(
        &env.db,
        &host,
        &plan,
        vec![table()],
        8192,
        GqlBudget::unlimited(),
    )
    .unwrap();
    assert_eq!(
        format!("{:?}", keys(&rows)),
        format!("{:?}", [text("b"), text("c"), text("d")])
    );
}

#[test]
fn the_mark_form_writes_the_answer_and_agrees_with_the_filter_form() {
    let env = env();
    let mut host = TestHost::default();
    let plan = exists(&mut host, ExistsMode::Mark { slot: MARK });
    let (rows, work) = run_with(
        &env.db,
        &host,
        &plan,
        vec![table()],
        8192,
        GqlBudget::unlimited(),
    )
    .unwrap();
    let marks: Vec<(BindingValue, BindingValue)> = rows
        .iter()
        .map(|(key, _, mark)| (key.clone(), mark.clone()))
        .collect();
    assert_eq!(
        format!("{marks:?}"),
        format!(
            "{:?}",
            [
                (text("a"), Bool(true)),
                (text("b"), Bool(false)),
                (text("c"), Bool(false)),
                (text("d"), Bool(false)),
                (text("e"), Bool(true)),
            ]
        )
    );
    // The filter form keeps exactly the rows the mark calls true.
    let mut host = TestHost::default();
    let filter = exists(&mut host, ExistsMode::Filter { negated: false });
    let (kept, _) = run_with(
        &env.db,
        &host,
        &filter,
        vec![table()],
        8192,
        GqlBudget::unlimited(),
    )
    .unwrap();
    let marked: Vec<BindingValue> = marks
        .iter()
        .filter(|(_, mark)| matches!(mark, Bool(true)))
        .map(|(key, _)| key.clone())
        .collect();
    assert_eq!(format!("{:?}", keys(&kept)), format!("{marked:?}"));
    // The operator charges nothing of its own: the outer FOR's 5 rows, and
    // the inner FOR's rows up to each first hit (a: 1, c: 2, e: 2).
    let charged: u64 = work.iter().map(|w| w.binding_rows).sum();
    assert_eq!(charged, 5 + 1 + 2 + 2);
}

#[test]
fn an_inner_side_cut_short_gives_back_what_it_held() {
    // Every input row's inner list is long and its first element holds, so
    // each inner side stops with its whole list still held.
    let inner_list = list(vec![Bool(true); 64]);
    let rows = 40;
    let outer = list(
        (0..rows)
            .map(|i| list(vec![text(&format!("r{i}")), inner_list.clone()]))
            .collect(),
    );
    let one_row = outer.held_bytes() + inner_list.held_bytes();
    let env = env();
    let mut host = TestHost::default();
    let plan = exists(&mut host, ExistsMode::Filter { negated: false });
    let (kept, work) = run_with(
        &env.db,
        &host,
        &plan,
        vec![outer.clone()],
        8192,
        GqlBudget::unlimited(),
    )
    .unwrap();
    assert_eq!(kept.len(), rows);
    // At most the outer list and ONE inner list at once, over one page.
    assert_eq!(work.len(), 1);
    assert_eq!(work[0].list_bytes, one_row);
    // So a ceiling of exactly that runs every row.
    let budget = GqlBudget {
        list_bytes: one_row,
        ..GqlBudget::unlimited()
    };
    let (kept, _) = run_with(&env.db, &host, &plan, vec![outer], 8192, budget).unwrap();
    assert_eq!(kept.len(), rows);
}

#[test]
fn paging_equals_one_shot() {
    let env = env();
    for mode in [
        ExistsMode::Filter { negated: false },
        ExistsMode::Filter { negated: true },
        ExistsMode::Mark { slot: MARK },
    ] {
        let mut host = TestHost::default();
        let plan = exists(&mut host, mode.clone());
        let (one_shot, _) = run_with(
            &env.db,
            &host,
            &plan,
            vec![table()],
            8192,
            GqlBudget::unlimited(),
        )
        .unwrap();
        for page_rows in [1, 2, 3] {
            let (rows, work) = run_with(
                &env.db,
                &host,
                &plan,
                vec![table()],
                page_rows,
                GqlBudget::unlimited(),
            )
            .unwrap();
            assert_eq!(
                format!("{rows:?}"),
                format!("{one_shot:?}"),
                "{mode:?}, pages of {page_rows}"
            );
            if one_shot.len() > page_rows {
                assert!(work.len() > 1, "{mode:?}, pages of {page_rows}");
            }
        }
    }
}

// ── the first-row stop over a graph ───────────────────────────────────────

/// Site `s1` has 1,000 incoming `visits` edges (parallel ones included);
/// site `s2` has none.
#[test]
fn an_existence_test_stops_at_the_first_edge() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("g.sekejap"), cfg()).unwrap();
    let site = db
        .create_collection(
            "site",
            vec![("name".into(), Kind::Text)],
            Default::default(),
        )
        .unwrap();
    let visitor = db
        .create_collection(
            "visitor",
            vec![("name".into(), Kind::Text)],
            Default::default(),
        )
        .unwrap();
    db.enable_graph().unwrap();
    let visits = db.create_edge_type("visits").unwrap();
    let s1 = db.put(site, "s1", &json!({ "name": "temple" })).unwrap();
    db.put(site, "s2", &json!({ "name": "beach" })).unwrap();
    let visitors: Vec<_> = (0..10)
        .map(|i| {
            db.put(
                visitor,
                &format!("v{i}"),
                &json!({ "name": format!("v{i}") }),
            )
            .unwrap()
        })
        .collect();
    for i in 0..1000 {
        db.create_edge(
            GraphContextId::BASE,
            visitors[i % 10],
            visits,
            s1,
            &json!({}),
        )
        .unwrap();
    }
    db.commit().unwrap();

    // `MATCH (s IS site WHERE s._key = $0) FILTER [NOT] EXISTS { (s)<-[:visits]-() }`.
    let plan = |host: &mut TestHost, negated: bool| {
        let key = host.expr(E::Param(0));
        OpSpec::ExistsApply {
            input: Box::new(OpSpec::Seed {
                input: Box::new(OpSpec::Unit { width: 2 }),
                out: SlotId(0),
                source: SeedSource::Key {
                    key,
                    labels: Box::new([site]),
                },
            }),
            inner: Box::new(OpSpec::Expand {
                input: Box::new(OpSpec::Argument { width: 2 }),
                from: SlotId(0),
                edge: None,
                to: Target::New(SlotId(1)),
                step: StepSpec {
                    context: GraphContextId::BASE,
                    types: Some(Box::new([visits])),
                    direction: Direction::Incoming,
                    edge_filter: None,
                    far_filter: None,
                    far_labels: None,
                },
            }),
            mode: ExistsMode::Filter { negated },
        }
    };
    let run = |key: &str, negated: bool| {
        let mut host = TestHost::default();
        let plan = plan(&mut host, negated);
        let mut cursor = GqlCursor::open(&db, &host, &plan, vec![text(key)]).unwrap();
        let page = cursor.next_page(8, GqlBudget::unlimited(), never).unwrap();
        assert!(page.done);
        (page.rows.len(), page.work)
    };
    let (kept, work) = run("s1", false);
    assert_eq!(kept, 1);
    assert!(
        work.base.graph_edges <= 2,
        "EXISTS over 1,000 edges walked {} turns",
        work.base.graph_edges
    );
    assert!(work.queue_entries <= 1, "{work:?}");
    let (kept, work) = run("s1", true);
    assert_eq!(kept, 0);
    assert!(work.base.graph_edges <= 2, "{work:?}");
    // No edge: one turn finds the range's end.
    let (kept, work) = run("s2", true);
    assert_eq!(kept, 1);
    assert_eq!(work.base.graph_edges, 1);
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
fn a_malformed_exists_plan_is_refused_at_open() {
    let env = env();
    let mut host = TestHost::default();

    // A mark slot outside the row.
    let plan = exists(
        &mut host,
        ExistsMode::Mark {
            slot: SlotId(WIDTH),
        },
    );
    is_invalid(&env, &host, &plan, "a mark slot outside the row");

    // An Argument narrower than the input row.
    let xs = host.expr(E::Slot(2));
    let plan = OpSpec::ExistsApply {
        input: Box::new(input(&mut host)),
        inner: Box::new(OpSpec::Unnest {
            input: Box::new(OpSpec::Argument { width: WIDTH - 1 }),
            list: xs,
            out: SlotId(3),
        }),
        mode: ExistsMode::Filter { negated: false },
    };
    is_invalid(&env, &host, &plan, "an Argument of another width");

    // A blocking operator inside the inner side: nothing blocking can
    // change whether a first row exists.
    let key = host.expr(E::Slot(3));
    let plan = OpSpec::ExistsApply {
        input: Box::new(input(&mut host)),
        inner: Box::new(OpSpec::Sort {
            input: Box::new(inner(&mut host)),
            keys: Box::new([SortKey {
                expr: key,
                descending: false,
            }]),
        }),
        mode: ExistsMode::Filter { negated: false },
    };
    is_invalid(&env, &host, &plan, "a Sort inside the inner side");

    // A CALL inside an existence test: no grammar puts one there.
    let plan = OpSpec::ExistsApply {
        input: Box::new(input(&mut host)),
        inner: Box::new(OpSpec::CallApply {
            input: Box::new(OpSpec::Argument { width: WIDTH }),
            inner: Box::new(inner(&mut host)),
            outputs: Box::new([]),
        }),
        mode: ExistsMode::Filter { negated: false },
    };
    is_invalid(&env, &host, &plan, "a CallApply inside the inner side");

    // An inner side that never reads the input row.
    let plan = OpSpec::ExistsApply {
        input: Box::new(input(&mut host)),
        inner: Box::new(OpSpec::Unit { width: WIDTH }),
        mode: ExistsMode::Filter { negated: false },
    };
    is_invalid(&env, &host, &plan, "a Unit as the inner side");
}
