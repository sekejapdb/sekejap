//! The GQL profile's budgets (`docs/lang/GQL_PROFILE_DESIGN.md` §3.4): every
//! new resource refuses BY NAME with its limit and the amount attempted, the
//! memory caps are a promise `unlimited()` does not lift, the resources the
//! existing meter already knows still refuse as before, and a cancel stops
//! the next charge. And the one-hop `Expand` holds its refill buffer as the
//! path searches hold theirs: one `queue_entries` per edge not yet handed
//! out, charged again to each page.

use sekejap_core::collections::gql::{
    BindingRow, BindingValue, EvalCx, ExprId, GqlBudget, GqlCursor, GqlHost, GqlMeter, GqlWork,
    OpSpec, SeedId, SeedSource, SlotId, StepSpec, Target, Truth,
};
use sekejap_core::collections::{
    Database, Direction, GraphContextId, PreparedQuery, QueryBudget, QueryError, QueryResult,
    WorkResource,
};
use sekejap_core::Kind;
use serde_json::json;
use std::cell::Cell;

mod common;

const RUN_BYTES: u64 = 8 << 20;

fn never() -> bool {
    false
}

fn refused(result: Result<(), QueryError>) -> (WorkResource, u64, u64) {
    match result {
        Err(QueryError::BudgetExceeded {
            resource,
            limit,
            attempted,
        }) => (resource, limit, attempted),
        other => panic!("expected a named refusal, got {other:?}"),
    }
}

/// A budget whose new ceilings are all `n`, everything else unlimited.
fn all_at(n: u64) -> GqlBudget {
    GqlBudget {
        binding_rows: n,
        path_states: n,
        queue_entries: n,
        predecessor_arcs: n,
        sort_bytes: n,
        list_bytes: n,
        ..GqlBudget::unlimited()
    }
}

const NEW: [WorkResource; 6] = [
    WorkResource::BindingRows,
    WorkResource::PathStates,
    WorkResource::QueueEntries,
    WorkResource::PredecessorArcs,
    WorkResource::SortBytes,
    WorkResource::ListBytes,
];

#[test]
fn every_new_resource_refuses_by_name_with_limit_and_attempted() {
    for resource in NEW {
        let mut cancel = never;
        let mut meter = GqlMeter::new(all_at(3), &mut cancel);
        meter.charge(resource, 2).unwrap();
        assert_eq!(refused(meter.charge(resource, 2)), (resource, 3, 4));
        // A refused charge is not counted: the ceiling is still reachable.
        meter.charge(resource, 1).unwrap();
        assert_eq!(refused(meter.charge(resource, 1)), (resource, 3, 4));
        // An overflowing amount is refused, not wrapped.
        assert_eq!(
            refused(meter.charge(resource, u64::MAX)),
            (resource, 3, u64::MAX)
        );
    }
}

#[test]
fn work_is_a_running_count_and_memory_a_high_water_mark() {
    let mut cancel = never;
    let mut meter = GqlMeter::new(all_at(10), &mut cancel);
    meter.charge(WorkResource::BindingRows, 4).unwrap();
    meter.charge(WorkResource::PathStates, 6).unwrap();
    meter.charge(WorkResource::SortBytes, 8).unwrap();
    meter.release(WorkResource::SortBytes, 8);
    // Released memory is held no more, so the cap is reachable again ...
    meter.charge(WorkResource::SortBytes, 9).unwrap();
    meter.charge(WorkResource::QueueEntries, 5).unwrap();
    meter.release(WorkResource::QueueEntries, 2);
    meter.charge(WorkResource::QueueEntries, 1).unwrap();
    let work: GqlWork = meter.work();
    assert_eq!(work.binding_rows, 4);
    assert_eq!(work.path_states, 6);
    // ... and the report is the most held at once, not a sum.
    assert_eq!(work.sort_bytes, 9);
    assert_eq!(work.queue_entries, 5);
    assert_eq!((work.predecessor_arcs, work.list_bytes), (0, 0));
    assert_eq!(work.base.graph_edges, 0);
}

#[test]
fn unlimited_does_not_lift_the_memory_caps() {
    let unlimited = GqlBudget::unlimited();
    assert_eq!(unlimited.base, QueryBudget::unlimited());
    assert_eq!((unlimited.binding_rows, unlimited.path_states), (u64::MAX, u64::MAX));
    assert_eq!((unlimited.sort_bytes, unlimited.list_bytes), (RUN_BYTES, RUN_BYTES));
    // Entry caps are RUN_BYTES divided by a positive per-entry cost.
    for cap in [unlimited.queue_entries, unlimited.predecessor_arcs] {
        assert!(cap > 0 && cap < RUN_BYTES, "{cap}");
    }
    assert_eq!(GqlBudget::from_query_budget(QueryBudget::unlimited()), unlimited);

    // Asking for more than the cap does not get it: the refusal names the
    // cap as the limit.
    let greedy = all_at(u64::MAX);
    for (resource, cap) in [
        (WorkResource::SortBytes, unlimited.sort_bytes),
        (WorkResource::ListBytes, unlimited.list_bytes),
        (WorkResource::QueueEntries, unlimited.queue_entries),
        (WorkResource::PredecessorArcs, unlimited.predecessor_arcs),
    ] {
        let mut cancel = never;
        let mut meter = GqlMeter::new(greedy, &mut cancel);
        meter.charge(resource, cap).unwrap();
        assert_eq!(refused(meter.charge(resource, 1)), (resource, cap, cap + 1));
    }
    // Work is the caller's to bound: an unlimited budget does not refuse it.
    let mut cancel = never;
    let mut meter = GqlMeter::new(greedy, &mut cancel);
    meter.charge(WorkResource::BindingRows, u64::MAX).unwrap();
    meter.charge(WorkResource::PathStates, u64::MAX).unwrap();
}

#[test]
fn the_existing_resources_refuse_through_the_wrapped_budget() {
    let base = QueryBudget {
        graph_edges: 2,
        primary_reads: 1,
        ..QueryBudget::unlimited()
    };
    let mut cancel = never;
    let mut meter = GqlMeter::new(GqlBudget::from_query_budget(base), &mut cancel);
    meter.charge(WorkResource::GraphEdges, 2).unwrap();
    assert_eq!(
        refused(meter.charge(WorkResource::GraphEdges, 1)),
        (WorkResource::GraphEdges, 2, 3)
    );
    meter.charge(WorkResource::PrimaryReads, 1).unwrap();
    assert_eq!(meter.work().base.graph_edges, 2);
    assert_eq!(meter.work().base.primary_reads, 1);
}

#[test]
fn a_cancel_stops_the_next_charge_of_every_resource() {
    let stop = Cell::new(false);
    let mut cancel = || stop.get();
    let mut meter = GqlMeter::new(GqlBudget::unlimited(), &mut cancel);
    meter.charge(WorkResource::BindingRows, 1).unwrap();
    stop.set(true);
    for resource in NEW.into_iter().chain([WorkResource::GraphEdges]) {
        assert!(
            matches!(meter.charge(resource, 1), Err(QueryError::Cancelled)),
            "{resource:?} was charged after a cancel"
        );
    }
    // Nothing was counted by the refused charges.
    assert_eq!(meter.work().binding_rows, 1);
}

// ── the one-hop refill buffer ─────────────────────────────────────────────

/// The only expression these plans hold: the seed's key.
struct KeyHost;

impl GqlHost for KeyHost {
    fn eval(&self, _: ExprId, _: &BindingRow, _: &mut EvalCx<'_, '_>) -> QueryResult<BindingValue> {
        Ok(BindingValue::Text("p1".into()))
    }

    fn test(&self, _: ExprId, _: &BindingRow, _: &mut EvalCx<'_, '_>) -> QueryResult<Truth> {
        unreachable!("these plans hold no predicate")
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

/// `p1` with three outgoing edges; the plan seeds `p1` by key and expands
/// one hop out.
fn fanout(path: &std::path::Path) -> (Database, OpSpec) {
    let mut db = Database::create(path, common::cfg()).unwrap();
    let person = db
        .create_collection("person", vec![("x".into(), Kind::Int)], Default::default())
        .unwrap();
    db.enable_graph().unwrap();
    let knows = db.create_edge_type("knows").unwrap();
    let p1 = db.put(person, "p1", &json!({})).unwrap();
    for key in ["p2", "p3", "p4"] {
        let far = db.put(person, key, &json!({})).unwrap();
        db.create_edge(GraphContextId::BASE, p1, knows, far, &json!({}))
            .unwrap();
    }
    db.commit().unwrap();
    let plan = OpSpec::Expand {
        input: Box::new(OpSpec::Seed {
            input: Box::new(OpSpec::Unit { width: 3 }),
            out: SlotId(0),
            source: SeedSource::Key {
                key: ExprId(0),
                labels: [person].into(),
            },
        }),
        from: SlotId(0),
        edge: Some(SlotId(1)),
        to: Target::New(SlotId(2)),
        step: StepSpec {
            context: GraphContextId::BASE,
            types: None,
            direction: Direction::Outgoing,
            edge_filter: None,
            far_labels: None,
            far_filter: None,
        },
    };
    (db, plan)
}

/// One refill walks all three edges before the first is handed out: the
/// buffer holds three, then each page holds what is left of it -- and a
/// page that cannot hold it is refused by name.
#[test]
fn the_expand_refill_buffer_is_held_as_queue_entries() {
    let dir = tempfile::tempdir().unwrap();
    let (db, plan) = fanout(&dir.path().join("f.sekejap"));
    let mut cursor = GqlCursor::open(&db, &KeyHost, &plan, Vec::new()).unwrap();
    let mut held = Vec::new();
    loop {
        let page = cursor.next_page(1, GqlBudget::unlimited(), never).unwrap();
        held.push(page.work.queue_entries);
        if page.done {
            break;
        }
    }
    assert_eq!(held[..3], [3, 2, 1], "{held:?}");

    let mut cursor = GqlCursor::open(&db, &KeyHost, &plan, Vec::new()).unwrap();
    let tight = GqlBudget {
        queue_entries: 2,
        ..GqlBudget::unlimited()
    };
    let refusal = cursor.next_page(1, tight, never).map(|_| ());
    assert_eq!(refused(refusal), (WorkResource::QueueEntries, 2, 3));
}
