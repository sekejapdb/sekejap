//! The selector searches: `PathSearch::Any` and `PathSearch::Shortest` (a
//! BFS), and `PathSearch::Cheapest` (Dijkstra, whose frontier and cost are
//! in `dijkstra.rs`) -- one witness per (input row, end node)
//! (`docs/lang/GQL_PROFILE_DESIGN.md` §4.3, §4.7).
//!
//! # Product states
//!
//! The search runs over PRODUCT states, never bare nodes: the node, the
//! automaton state, the active repeat's counter `k` (saturated at the
//! minimum for an unbounded repeat, as enumeration does), and the values of
//! the slots a later test may still read that can differ between two
//! arrivals -- every singleton the pattern has bound since the start, and,
//! strictly inside a repeat body, the current iteration's group elements.
//! `(D, q, 1)` and `(D, q, 2)` are different questions once the pattern has
//! a lower bound, and a node reached with a different earlier binding may
//! pass a later predicate the first one failed. A predicate sees exactly
//! those slots and the input row, the start's binding written into it once
//! (it is the same for every state of the search): a group slot of a
//! finished iteration reads `Null`. A slot the pattern binds only later
//! still holds the input row's value, so it tells no two states apart and
//! is left out of the identity, which is exactly as fine-grained as it
//! would be with every slot in it.
//!
//! Each state is a record in one list: its parent's index and the edge it
//! crossed (a predecessor arc), so the path is rebuilt only for the rows
//! that are emitted.
//!
//! # The mode
//!
//! * `Walk`: a visited set on product states. The future of a state
//!   depends only on the state, so every prefix of a shortest (cheapest)
//!   walk is a shortest (cheapest) walk to its own state and the first time
//!   a state is settled is final. A repeat's counter is bounded or
//!   saturated, so the states are finite and an unbounded WALK ends.
//! * `Trail`, `Acyclic`: the future depends on the whole path, so there is
//!   no visited set: every admitted extension is a new state, and the mode
//!   is checked per PATH by walking the new state's predecessor chain (the
//!   check enumeration makes). The first time an end node is settled is
//!   still a minimum, because states are settled in priority order.
//!   **Cost (named):** exponential in the worst case, bounded by the
//!   budgets.
//!
//! # Order and emission
//!
//! A state is settled when it leaves the frontier. `Shortest` and `Any`
//! use a level-synchronous FIFO (priority: hops, then creation order);
//! `Cheapest` a heap (cost, hops, creation order). A state is created in
//! the order its parent's moves are tried: `Same` moves in the automaton's
//! order, then the edge step's adjacency ranges -- outgoing before
//! incoming, types in the step's order, postings in key order. A `Same`
//! move crosses no edge, so its state keeps its parent's priority and is
//! settled in the same level. A settled state's edge steps are walked only
//! after every state of its priority is settled; its edge successors all
//! cost more, so the creation order stays as described, and a
//! point-to-point search that accepts at that priority crosses no further
//! edge (nor evaluates its COST).
//!
//! When an ACCEPTING state is settled and its node has no witness yet for
//! this input row, the row is emitted; later accepting states at that node
//! are skipped. ANY promises only some qualifying path, and gets the
//! shortest one.
//!
//! When the input row already holds the accepting position's slot -- the
//! end node was bound in advance -- only that node can accept, and the
//! search stops after its first witness (point to point). A `Null` start
//! gives no row; an unreachable target gives none.
//!
//! # Charges
//!
//! * `graph_edges`: 1 per turn of an adjacency walk, the end probe
//!   included; predicates and COST charge what they read.
//! * `path_states`: 1 per product state first discovered (every state,
//!   under `Trail` and `Acyclic`).
//! * `queue_entries`, held: 1 per frontier entry, and, during one walk, 1
//!   per edge in its refill buffer (at most 256).
//! * `predecessor_arcs`, held: 1 per state record, until the input row's
//!   search ends. A cheaper arrival at a state not yet settled is a new
//!   record, and the superseded one stays in the frontier until popped.
//! * `list_bytes`: each emitted group list, while the row is built.
//! * `binding_rows`: 1 per witness.
//!
//! The frontier and the records outlive a page, so they are charged again
//! to each page ([`Held`]). **Cost (named):** the visited map and an edge's
//! decoded property bag held by a record are not charged beyond one
//! predecessor arc each.

use super::super::super::{invalid_query, QueryResult, WorkResource};
use super::super::host::ExprId;
use super::super::ops::{node_in, refill, ExecCx, Held, Op, Operator, Range};
use super::super::value::{BindingRow, BindingValue, EdgeRef, NodeRef, PathRef, SlotId};
use super::arrive::{
    admit, complete_row, edge_step, labels_admit, link_ranges, moves, restore, Move, Undo,
};
use super::automaton::{PathAutomaton, PathLink, PathMode};
use super::dijkstra::{add_cost, edge_cost, CostHeap};
use crate::collections::EntityId;
use crate::index::graph::Direction;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::mem::{replace, take};
use std::sync::Arc;

pub(in super::super) struct Select<'q> {
    input: Op<'q>,
    from: SlotId,
    automaton: &'q PathAutomaton,
    /// The COST expression, under `Cheapest`.
    cost: Option<ExprId>,
    /// Per automaton state, the slots whose values are part of its product
    /// state (see the module docs).
    keys: Box<[Box<[usize]>]>,
    /// Per link, the adjacency ranges its edge step walks (none for `Same`).
    ranges: Box<[Box<[Range]>]>,
    /// One walk's refill buffer, kept for the next walk.
    buffer: Vec<(EdgeRef, EntityId)>,
    run: Option<Run>,
    held: Held,
}

/// The search from one input row.
struct Run {
    /// The input row, with the start's bindings written in.
    base: BindingRow,
    /// `base` with one arrival's bindings written in, while its tests run;
    /// equal to `base` between arrivals.
    scratch: BindingRow,
    /// What one arrival wrote into `scratch`, to put back after it.
    undo: Undo,
    records: Vec<Record>,
    /// Under `Walk`: the record holding each product state's best arrival.
    best: HashMap<Key, usize>,
    frontier: Frontier,
    /// The end nodes already given a witness.
    ends: HashSet<NodeRef>,
    /// The end node was bound in advance: stop after the first witness.
    fixed: bool,
}

/// A product state's key slot values, shared by its record and its key;
/// `None` when it has no key slot, which is the common case and allocates
/// nothing.
type Binds = Option<Arc<[BindingValue]>>;

type Key = (NodeRef, usize, u32, Binds);

/// One product state, as first reached by its best arrival so far.
struct Record {
    node: EntityId,
    state: usize,
    k: u32,
    /// The predecessor arc: the parent record, and the edge crossed from it
    /// with its orientation (`None` for a `Same` move and the start).
    parent: Option<usize>,
    via: Option<(EdgeRef, bool)>,
    /// The values of `keys[state]`.
    binds: Binds,
    cost: f64,
    hops: u32,
    /// Taken off the frontier as the state's final arrival.
    settled: bool,
    /// A cheaper arrival at the same state replaced this one.
    superseded: bool,
}

impl Record {
    fn binds(&self) -> &[BindingValue] {
        self.binds.as_deref().unwrap_or_default()
    }
}

enum Frontier {
    Levels(Levels),
    Costs(CostHeap),
}

/// The BFS frontier: the level being settled and the one after it, each in
/// creation order, and the deferred edge walks of the level's settled
/// states, in settle order.
#[derive(Default)]
struct Levels {
    level: u32,
    now: VecDeque<usize>,
    walks: VecDeque<usize>,
    next: VecDeque<usize>,
}

impl Frontier {
    /// Queue settled record `record`'s edge walk behind every state of its
    /// priority: its successors all cost more, so settling those first
    /// changes no order, and a point-to-point search that accepts among
    /// them never crosses an edge it does not need (nor evaluates its COST).
    fn push_walk(&mut self, record: usize, cost: f64, hops: u32) {
        match self {
            Frontier::Levels(levels) => {
                debug_assert!(hops == levels.level);
                levels.walks.push_back(record);
            }
            Frontier::Costs(heap) => heap.push(record, cost, hops, true),
        }
    }

    fn push(&mut self, record: usize, cost: f64, hops: u32) {
        match self {
            Frontier::Levels(levels) => {
                debug_assert!(hops == levels.level || hops == levels.level + 1);
                if hops == levels.level {
                    levels.now.push_back(record);
                } else {
                    levels.next.push_back(record);
                }
            }
            Frontier::Costs(heap) => heap.push(record, cost, hops, false),
        }
    }

    /// The next record, and whether it is a deferred edge walk.
    fn pop(&mut self) -> Option<(usize, bool)> {
        match self {
            Frontier::Levels(levels) => loop {
                if let Some(record) = levels.now.pop_front() {
                    return Some((record, false));
                }
                if let Some(record) = levels.walks.pop_front() {
                    return Some((record, true));
                }
                if levels.next.is_empty() {
                    return None;
                }
                std::mem::swap(&mut levels.now, &mut levels.next);
                levels.level += 1;
            },
            Frontier::Costs(heap) => heap.pop(),
        }
    }

    fn len(&self) -> usize {
        match self {
            Frontier::Levels(levels) => levels.now.len() + levels.walks.len() + levels.next.len(),
            Frontier::Costs(heap) => heap.len(),
        }
    }
}

impl<'q> Select<'q> {
    /// The selector search of `automaton`, which the caller has checked:
    /// `ANY` / `ANY SHORTEST` without `cost`, `ANY CHEAPEST` with it. A
    /// cheapest search whose pattern does not cross exactly one edge step,
    /// bound to a slot its COST can read, is refused.
    pub(in super::super) fn new(
        input: Op<'q>,
        from: SlotId,
        automaton: &'q PathAutomaton,
        cost: Option<ExprId>,
    ) -> QueryResult<Self> {
        if cost.is_some() {
            let mut steps = automaton.links.iter().filter_map(|link| match link {
                PathLink::Edge(step) => Some(step),
                PathLink::Same => None,
            });
            let one = steps.next().filter(|step| step.bind.is_some());
            if one.is_none() || steps.next().is_some() {
                return Err(invalid_query(
                    "ANY CHEAPEST takes one COST: its pattern must cross exactly one edge \
                     step, and bind that edge",
                ));
            }
        }
        Ok(Self {
            input,
            from,
            automaton,
            cost,
            keys: key_slots(automaton),
            ranges: link_ranges(automaton),
            buffer: Vec::new(),
            run: None,
            held: Held::default(),
        })
    }

    /// Start the search from `row`, or skip a row whose start is `Null`.
    fn start(&mut self, cx: &mut ExecCx<'q, '_, '_>, row: BindingRow) -> QueryResult<()> {
        let Some(start) = node_in(&row, self.from, "a path pattern's start")? else {
            return Ok(());
        };
        let a = self.automaton;
        let fixed = a.node_binding(a.states.len() - 1).is_some_and(|binding| {
            !binding.group && !matches!(row.slots[binding.slot], BindingValue::Null)
        });
        self.run = Some(Run {
            scratch: row.clone(),
            base: row,
            undo: Vec::new(),
            records: Vec::new(),
            best: HashMap::new(),
            frontier: match self.cost {
                Some(_) => Frontier::Costs(CostHeap::default()),
                None => Frontier::Levels(Levels::default()),
            },
            ends: HashSet::new(),
            fixed,
        });
        self.arrive(cx, None, 0, start, None, 0)
    }

    /// Reach `state` at `node` from record `parent` (none for the start),
    /// crossing `via` = (link, edge, forward) when given, with counter `k`.
    /// Creates a record and queues it when every test holds and the state
    /// is new or reached more cheaply.
    fn arrive(
        &mut self,
        cx: &mut ExecCx<'q, '_, '_>,
        parent: Option<usize>,
        state: usize,
        node: EntityId,
        via: Option<(usize, EdgeRef, bool)>,
        k: u32,
    ) -> QueryResult<()> {
        let a = self.automaton;
        let run = self.run.as_mut().expect("a search is running");
        if let (Some(p), Some((_, edge, _))) = (parent, &via) {
            let revisits = match a.mode {
                PathMode::Walk => false,
                PathMode::Trail => chain_any(&run.records, p, |r| {
                    r.via.as_ref().is_some_and(|(e, _)| e == edge)
                }),
                PathMode::Acyclic => chain_any(&run.records, p, |r| r.node == node),
            };
            if revisits {
                return Ok(());
            }
        }

        // The parent's product-state slots, then this arrival's bindings.
        let (mut cost, mut hops) = (0.0, 0);
        if let Some(p) = parent {
            let parent = &run.records[p];
            for (slot, value) in self.keys[parent.state].iter().zip(parent.binds()) {
                let old = replace(&mut run.scratch.slots[*slot], value.clone());
                run.undo.push((*slot, old));
            }
            (cost, hops) = (parent.cost, parent.hops);
        }
        if via.is_some() {
            hops += 1;
        }
        let tested = test(cx, a, self.cost, run, state, node, via.as_ref(), cost);
        // The key slots are read only for an arrival that passed.
        let binds: Binds = match &tested {
            Ok(Some(_)) if !self.keys[state].is_empty() => Some(
                self.keys[state]
                    .iter()
                    .map(|slot| run.scratch.slots[*slot].clone())
                    .collect(),
            ),
            _ => None,
        };
        if parent.is_none() && matches!(tested, Ok(Some(_))) {
            // The start's bindings hold for the whole search.
            run.base = run.scratch.clone();
            run.undo.clear();
        } else {
            restore(&mut run.scratch, &mut run.undo, 0);
        }
        let Some(cost) = tested? else {
            return Ok(());
        };

        let index = run.records.len();
        if a.mode == PathMode::Walk {
            match run.best.entry((NodeRef(node), state, k, binds.clone())) {
                Entry::Occupied(mut best) => {
                    let old = &mut run.records[*best.get()];
                    if old.settled || (old.cost, old.hops) <= (cost, hops) {
                        return Ok(());
                    }
                    old.superseded = true;
                    best.insert(index);
                }
                Entry::Vacant(best) => {
                    cx.meter.charge(WorkResource::PathStates, 1)?;
                    best.insert(index);
                }
            }
        } else {
            cx.meter.charge(WorkResource::PathStates, 1)?;
        }
        self.held.charge(cx, WorkResource::PredecessorArcs, 1)?;
        self.held.charge(cx, WorkResource::QueueEntries, 1)?;
        run.records.push(Record {
            node,
            state,
            k,
            parent,
            via: via.map(|(_, edge, forward)| (edge, forward)),
            binds,
            cost,
            hops,
            settled: false,
            superseded: false,
        });
        run.frontier.push(index, cost, hops);
        Ok(())
    }

    /// The moves out of settled record `index`: its `Same` moves now, and,
    /// when `walks`, its edge steps; otherwise a deferred walk is queued
    /// for them ([`Frontier::push_walk`]).
    fn expand(&mut self, cx: &mut ExecCx<'q, '_, '_>, index: usize, walks: bool) -> QueryResult<()> {
        let a = self.automaton;
        let run = self.run.as_ref().expect("a search is running");
        let (node, state, k, cost, hops) = {
            let r = &run.records[index];
            (r.node, r.state, r.k, r.cost, r.hops)
        };
        let mut deferred = false;
        for next in moves(a, state, k).into_iter().flatten() {
            match next {
                Move::Same { to, k } if !walks => self.arrive(cx, Some(index), to, node, None, k)?,
                Move::Edge { link } if walks => self.walk(cx, index, node, link, k)?,
                Move::Edge { .. } => deferred = true,
                Move::Same { .. } => {}
            }
        }
        if deferred {
            self.held.charge(cx, WorkResource::QueueEntries, 1)?;
            let run = self.run.as_mut().expect("a search is running");
            run.frontier.push_walk(index, cost, hops);
        }
        Ok(())
    }

    /// Cross the edge step of `link` from record `parent` at `node`: each
    /// adjacency range in refills, the walk paused while the refill's
    /// arrivals run their predicates.
    fn walk(
        &mut self,
        cx: &mut ExecCx<'q, '_, '_>,
        parent: usize,
        node: EntityId,
        link: usize,
        k: u32,
    ) -> QueryResult<()> {
        let a = self.automaton;
        let step = edge_step(a, link);
        let far_state = link + 1;
        let mut buffer = take(&mut self.buffer);
        for at in 0..self.ranges[link].len() {
            let range = self.ranges[link][at];
            let forward = range.0 == Direction::Outgoing;
            let mut paused = None;
            loop {
                paused = refill(
                    cx,
                    node,
                    step.context,
                    step.direction,
                    range,
                    step.bind.is_some(),
                    paused,
                    |far| labels_admit(a, far_state, far),
                    |cx, edge, far| {
                        cx.meter.charge(WorkResource::QueueEntries, 1)?;
                        buffer.push((edge, far));
                        Ok(())
                    },
                )?;
                for (edge, far) in buffer.drain(..) {
                    cx.meter.release(WorkResource::QueueEntries, 1);
                    self.arrive(
                        cx,
                        Some(parent),
                        far_state,
                        far,
                        Some((link, edge, forward)),
                        k,
                    )?;
                }
                if paused.is_none() {
                    break;
                }
            }
        }
        self.buffer = buffer;
        Ok(())
    }

    /// The witness row of accepting record `index`: the input row, the
    /// singletons, each group slot's list and the path, rebuilt from the
    /// predecessor chain.
    fn emit(&mut self, cx: &mut ExecCx<'q, '_, '_>, index: usize) -> QueryResult<BindingRow> {
        let a = self.automaton;
        let run = self.run.as_ref().expect("a search is running");
        let mut chain = vec![index];
        while let Some(parent) = run.records[*chain.last().expect("a chain")].parent {
            chain.push(parent);
        }
        chain.reverse();

        let mut row = run.base.clone();
        let accept = &run.records[index];
        for (slot, value) in self.keys[accept.state].iter().zip(accept.binds()) {
            row.slots[*slot] = value.clone();
        }
        let mut lists: Vec<Vec<BindingValue>> = vec![Vec::new(); a.groups.len()];
        let mut push = |slot: usize, value: BindingValue| {
            if let Some(at) = a.groups.iter().position(|(s, _)| usize::from(s.0) == slot) {
                lists[at].push(value);
            }
        };
        let root = &run.records[chain[0]];
        let mut path = PathRef::new(NodeRef(root.node));
        for (i, &at) in chain.iter().enumerate() {
            let record = &run.records[at];
            if let Some((edge, forward)) = &record.via {
                let link = run.records[chain[i - 1]].state;
                if let Some(binding) = a.edge_binding(link, edge_step(a, link)) {
                    if binding.group {
                        push(binding.slot, BindingValue::Edge(edge.clone()));
                    }
                }
                path = path.extend(edge.clone(), *forward, NodeRef(record.node));
            }
            if let Some(binding) = a.node_binding(record.state) {
                if binding.group {
                    push(binding.slot, BindingValue::Node(NodeRef(record.node)));
                }
            }
        }
        complete_row(cx, a, &mut row, lists, path)?;
        cx.meter.charge(WorkResource::BindingRows, 1)?;
        Ok(row)
    }

    /// The input row's search is over: drop its frontier and records.
    fn finish(&mut self, cx: &mut ExecCx<'q, '_, '_>) {
        if let Some(run) = self.run.take() {
            self.held
                .release(cx, WorkResource::QueueEntries, run.frontier.len() as u64);
            self.held
                .release(cx, WorkResource::PredecessorArcs, run.records.len() as u64);
        }
    }
}

/// The tests of one arrival over `run`'s scratch row (`admit`), then, under
/// `Cheapest`, the crossed edge's cost. The path cost after it, or `None`
/// when a test fails.
#[allow(clippy::too_many_arguments)]
fn test(
    cx: &mut ExecCx<'_, '_, '_>,
    a: &PathAutomaton,
    cost_expr: Option<ExprId>,
    run: &mut Run,
    state: usize,
    node: EntityId,
    via: Option<&(usize, EdgeRef, bool)>,
    cost: f64,
) -> QueryResult<Option<f64>> {
    let crossed = via.map(|(link, edge, _)| (*link, edge));
    if !admit(cx, a, &mut run.scratch, &mut run.undo, None, state, node, crossed)? {
        return Ok(None);
    }
    match (cost_expr, via) {
        (Some(expr), Some((_, edge, _))) => {
            let value = cx.eval(expr, &run.scratch)?;
            Ok(Some(add_cost(cost, edge_cost(&value, edge)?, edge)?))
        }
        _ => Ok(Some(cost)),
    }
}
impl<'q> Operator<'q> for Select<'q> {
    fn next(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<BindingRow>> {
        self.held.recharge(cx)?;
        loop {
            let Some(run) = self.run.as_mut() else {
                let Some(row) = self.input.next(cx)? else {
                    return Ok(None);
                };
                self.start(cx, row)?;
                continue;
            };
            let Some((index, walk)) = run.frontier.pop() else {
                self.finish(cx);
                continue;
            };
            self.held.release(cx, WorkResource::QueueEntries, 1);
            if walk {
                self.expand(cx, index, true)?;
                continue;
            }
            let record = &mut run.records[index];
            if record.superseded {
                continue;
            }
            record.settled = true;
            if record.state + 1 < self.automaton.states.len() {
                self.expand(cx, index, false)?;
                continue;
            }
            if !run.ends.insert(NodeRef(record.node)) {
                continue;
            }
            let fixed = run.fixed;
            let row = self.emit(cx, index)?;
            if fixed {
                self.finish(cx);
            }
            return Ok(Some(row));
        }
    }
}

/// Per automaton state `q`, the slots whose values belong to its product
/// state: every singleton slot bound at a position `1..=q` -- a node at a
/// state, an edge at the state it reaches -- and, at a state strictly
/// inside a repeat body before its last position, the group slots the
/// current iteration has bound so far. A slot the start binds is left out:
/// it is written into the search's base row once.
fn key_slots(a: &PathAutomaton) -> Box<[Box<[usize]>]> {
    let start = a.node_binding(0).map(|binding| binding.slot);
    // Each singleton slot, with the first position that binds it.
    let mut singles: Vec<(usize, usize)> = Vec::new();
    let mut single = |slot: usize, at: usize| {
        if Some(slot) != start && !singles.iter().any(|(s, _)| *s == slot) {
            singles.push((slot, at));
        }
    };
    for state in 1..a.states.len() {
        if let Some(PathLink::Edge(step)) = a.links.get(state - 1) {
            if let Some(binding) = a.edge_binding(state - 1, step).filter(|b| !b.group) {
                single(binding.slot, state);
            }
        }
        if let Some(binding) = a.node_binding(state).filter(|b| !b.group) {
            single(binding.slot, state);
        }
    }
    (0..a.states.len())
        .map(|state| {
            let mut slots: Vec<usize> = singles
                .iter()
                .filter(|(_, at)| *at <= state)
                .map(|(slot, _)| *slot)
                .collect();
            slots.sort_unstable();
            let body = a
                .repeats
                .iter()
                .find(|r| usize::from(r.first) <= state && state < usize::from(r.last));
            if let Some(r) = body {
                let first = usize::from(r.first);
                for q in first..=state {
                    if let Some(binding) = a.node_binding(q) {
                        slots.push(binding.slot);
                    }
                }
                for link in first..state {
                    if let PathLink::Edge(step) = &a.links[link] {
                        if let Some(binding) = a.edge_binding(link, step) {
                            slots.push(binding.slot);
                        }
                    }
                }
            }
            slots.into_boxed_slice()
        })
        .collect()
}

/// Does the predecessor chain from record `at` back to the start hold a
/// record passing `test`? The `TRAIL` and `ACYCLIC` checks.
fn chain_any(records: &[Record], mut at: usize, test: impl Fn(&Record) -> bool) -> bool {
    loop {
        let record = &records[at];
        if test(record) {
            return true;
        }
        match record.parent {
            Some(parent) => at = parent,
            None => return false,
        }
    }
}
