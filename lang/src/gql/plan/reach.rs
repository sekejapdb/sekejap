//! When the existing node BFS may stand in for a path search
//! (`docs/lang/GQL_PROFILE_DESIGN.md` §4.6, `GRAPH_CONTRACT` §4.1).
//!
//! The node BFS keeps ONE visited set on nodes: it reports each node once,
//! at the first depth it reaches it, never reports its start at a depth of
//! one or more, and keeps no path, no multiplicity and no later depth. A
//! `PathSearch` keeps all of them. [`choose`] replaces a `PathSearch` by a
//! [`Reach`] only when the whole plan proves that nothing observes the
//! difference -- all seven rules below hold -- and otherwise leaves it and
//! records the first rule that failed, which `EXPLAIN` prints. The test is
//! conservative: a case it cannot prove keeps the path search.
//!
//! 1. **Shape.** The pattern is one quantified edge `(s)-[:T]->{lo,hi}(t)`:
//!    at most one edge type, no edge predicate, no subpath of more than one
//!    edge, no path variable.
//! 2. **Start.** The search starts from `s`, bound by its seed, once per
//!    input row, and the end is NOT bound before it (a repeated or
//!    key-seeded end makes the search point to point).
//! 3. **Observed outputs.** Until the row is projected, nothing reads a
//!    variable the pattern binds other than `t` (an edge group, a node
//!    group).
//! 4. **Consumer.** The rows become a set before anything counts them: the
//!    selector `ANY` or `ANY SHORTEST` itself, or a later `Distinct`, or an
//!    aggregate of identity folds only (`COUNT(DISTINCT x)`, `MIN`, `MAX`).
//!    After it nothing depends on the order rows arrive in (a `LIMIT` or
//!    `OFFSET`, an `ARRAY_AGG`), because the node BFS delivers them in
//!    another order.
//! 5. **Lower bound.** `lo = 0`; or `lo = 1` and the path cannot return to
//!    its start as an end: the mode is `ACYCLIC`, or the end's labels
//!    exclude every collection the start may be in.
//! 6. **No pruning inside the repeat.** The positions inside the repeat
//!    test nothing; the end's labels and predicate are applied after the
//!    walk, to each node it reached.
//! 7. **Bounds fit.** `hi <= 64`, the BFS's depth ceiling. An unbounded
//!    quantifier is refused: the BFS would stop at 64.
//!
//! With rules 1-7, the set of ends is the same under `WALK`, `TRAIL` and
//! `ACYCLIC` -- a walk within `hi` hops contains a simple path within `hi`
//! hops -- except for the start, which rule 5 covers.

use super::super::bind::TypeRef;
use super::super::eval::Program;
use super::super::schema::BindingSchema;
use super::super::stage::{StageView, TableOp};
use super::explain::{var, walks};
use super::{flat, Op};
use crate::SqlResult2;
use sekejap_core::collections::gql::{
    AggSpec, ExprId, OpSpec, PathAutomaton, PathLink, PathMode, PathSearch, ReachSpec, SeedSource,
    SlotId, Target, ValueType,
};
use sekejap_core::collections::{CollectionId, Database, Direction, GraphContextId};

/// A path pattern the node BFS answers, and why (§4.6).
#[derive(Clone, Debug)]
pub(in crate::gql) struct Reach {
    from: SlotId,
    to: Option<SlotId>,
    /// The edge types as written, looked up when the tree is built.
    types: Option<Vec<TypeRef>>,
    direction: Direction,
    min: u32,
    max: u32,
    mode: PathMode,
    end_labels: Option<Box<[CollectionId]>>,
    end_filter: Option<ExprId>,
    /// Rule 4's consumer and rule 5's reason, as `EXPLAIN` prints them.
    consumer: &'static str,
    lower: &'static str,
}

/// The first of §4.6's rules a path search fails, and why.
#[derive(Clone, Debug)]
pub(in crate::gql) struct Refusal {
    rule: u8,
    reason: String,
}

impl Refusal {
    /// `EXPLAIN`'s line.
    pub(super) fn describe(&self) -> String {
        format!("not Reach: rule {} fails -- {}", self.rule, self.reason)
    }
}

fn refuse<T>(rule: u8, reason: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal {
        rule,
        reason: reason.into(),
    })
}

impl Reach {
    pub(super) fn types(&self) -> &Option<Vec<TypeRef>> {
        &self.types
    }

    /// The engine operator over `input`, its type looked up under `db`.
    pub(super) fn spec(
        &self,
        db: &Database,
        context: Option<GraphContextId>,
        input: Box<OpSpec>,
    ) -> SqlResult2<OpSpec> {
        Ok(OpSpec::Reach {
            input,
            from: self.from,
            to: self.to,
            spec: ReachSpec {
                context: context.unwrap_or(GraphContextId::BASE),
                // A context not interned holds no edge: the walk crosses
                // nothing.
                types: match context {
                    None => Some(Box::new([])),
                    Some(_) => super::types(db, &self.types)?,
                },
                direction: self.direction,
                min_hops: self.min,
                max_hops: self.max,
                end_labels: self.end_labels.clone(),
                end_filter: self.end_filter,
            },
        })
    }

    /// `EXPLAIN`'s lines, the first numbered `n`.
    pub(super) fn describe(
        &self,
        n: &str,
        indent: &str,
        schema: &BindingSchema,
        text: &dyn Fn(&ExprId) -> String,
    ) -> String {
        let to = self
            .to
            .map_or_else(|| "an unnamed end".to_owned(), |slot| var(slot, schema));
        let walks = walks(self.direction);
        let mut out = format!(
            "{indent}{n}. Reach from {} to {to}: the node BFS -- each node once, at its first depth -- over {walks}, {} to {} hops, mode {:?} -- charges graph_edges, graph_visited, primary_reads, queue_entries, binding_rows\n",
            var(self.from, schema),
            self.min,
            self.max,
            self.mode,
        );
        out.push_str(&format!(
            "{indent}     admitted by rules 1, 2, 3, 4 ({}), 5 ({}), 6, 7 of GQL_PROFILE_DESIGN §4.6\n",
            self.consumer, self.lower
        ));
        if let Some(filter) = &self.end_filter {
            out.push_str(&format!(
                "{indent}     end test after the BFS, per reached node: {} -- charges primary_reads\n",
                text(filter)
            ));
        }
        out
    }
}

/// Put a [`Reach`] in place of every `PathSearch` of `ops` that §4.6's
/// seven rules admit, and record on every other the rule it failed.
/// `stages` locate each top-level operator's slot schema.
///
/// A union's branch (M5-E) is a row flow of its own: the rows the union
/// reads -- the projection before it, whose columns are bound -- then the
/// branch's steps, then what follows the union.
pub(super) fn choose(ops: &mut [Op], program: &Program, stages: &[StageView]) {
    let mut positions = Vec::new();
    for (at, op) in ops.iter().enumerate() {
        let stage = stages.iter().rposition(|s| s.first_op <= at).unwrap_or(0);
        let count = flat(std::slice::from_ref(op)).len();
        positions.extend(std::iter::repeat_n(stage, count));
    }
    let all = flat(ops);
    let decided = decisions(&all, 0..all.len(), program, |at| &stages[positions[at]].schema);
    apply(ops, &mut decided.into_iter());
    for at in 0..ops.len() {
        let (before, rest) = ops.split_at_mut(at);
        let Some((Op::Union(union), after)) = rest.split_first_mut() else { continue };
        let lead = before
            .iter()
            .rev()
            .find(|op| matches!(op, Op::Project { .. } | Op::Union(_)));
        let after = flat(after);
        for branch in &mut union.branches {
            let steps = flat(&branch.ops);
            let first = usize::from(lead.is_some());
            let range = first..first + steps.len();
            let all: Vec<&Op> = lead.into_iter().chain(steps).chain(after.iter().copied()).collect();
            let decided = decisions(&all, range, program, |_| &branch.view.schema);
            apply(&mut branch.ops, &mut decided.into_iter());
        }
    }
}

/// The decision for each `PathSearch` of `all` inside `range`, in order,
/// each read against the slot schema `schema` gives its position.
fn decisions<'s>(
    all: &[&Op],
    range: std::ops::Range<usize>,
    program: &Program,
    schema: impl Fn(usize) -> &'s BindingSchema,
) -> Vec<Result<Reach, Refusal>> {
    all.iter()
        .enumerate()
        .filter(|(at, op)| range.contains(at) && matches!(op, Op::PathSearch { .. }))
        .map(|(at, _)| decide(all, at, program, schema(at)))
        .collect()
}

/// Replace, in the order [`flat`] lists them, each `PathSearch` by its
/// decision.
fn apply(ops: &mut [Op], decisions: &mut impl Iterator<Item = Result<Reach, Refusal>>) {
    for op in ops {
        match op {
            Op::Optional { inner, .. } | Op::Exists { inner, .. } => apply(inner, decisions),
            Op::PathSearch { reach, .. } => {
                match decisions.next().expect("a decision per search") {
                    Ok(admitted) => *op = Op::Reach(admitted),
                    Err(refusal) => *reach = Some(refusal),
                }
            }
            _ => {}
        }
    }
}

/// The seven rules, in order, for the `PathSearch` at `all[at]`.
fn decide(
    all: &[&Op],
    at: usize,
    program: &Program,
    schema: &BindingSchema,
) -> Result<Reach, Refusal> {
    let Op::PathSearch {
        from,
        automaton: a,
        types,
        search,
        ..
    } = all[at]
    else {
        unreachable!("decide is asked of a PathSearch")
    };
    // Rule 1: one quantified edge, nothing else.
    if a.path.is_some() {
        return refuse(1, "a path variable: the node BFS keeps no path to bind it");
    }
    let step = match (&*a.states, &*a.links, &*a.repeats) {
        ([start, _, _, _], [PathLink::Same, PathLink::Edge(step), PathLink::Same], [repeat])
            if repeat.first == 1
                && repeat.last == 2
                && start.labels.is_none()
                && start.filter.is_none()
                && start.bind.is_none() =>
        {
            step
        }
        _ => {
            return refuse(
                1,
                "not one quantified edge: the node BFS crosses one edge per hop",
            );
        }
    };
    if step.filter.is_some() {
        return refuse(
            1,
            "an edge predicate: the node BFS evaluates none of this profile's expressions",
        );
    }
    let written = types.get(1).cloned().flatten();
    if written.as_ref().is_some_and(|types| types.len() > 1) {
        return refuse(1, "edge type alternatives: the node BFS walks one type");
    }
    let end = &a.states[3];
    // Rule 2: from the start, once per input row, to an end not yet bound.
    if let Some(slot) = end.bind {
        if slot == *from || bound_before(all, at).contains(&slot) {
            return refuse(
                2,
                format!(
                    "the end {} is bound before the search, which runs point to point to it; the node BFS answers from the start alone and never reports the start again",
                    var(slot, schema)
                ),
            );
        }
    }
    // Rule 3: nothing reads what the pattern binds, but the end.
    let hidden: Vec<SlotId> = super::super::automaton::binds(a)
        .into_iter()
        .filter(|slot| Some(*slot) != end.bind)
        .collect();
    if let Some(slot) = read_before_projection(all, at, program, &hidden) {
        return refuse(
            3,
            format!(
                "{} is read after the pattern: the node BFS binds only the start and the end",
                var(slot, schema)
            ),
        );
    }
    // Rule 4: a set before anything counts, and nothing reads the order.
    let consumer = consumer(all, at, *search)?;
    // Rule 5: the start comes back as an end only at zero hops.
    let repeat = a.repeats[0];
    let lower = match repeat.min {
        0 => "lo = 0",
        1 if a.mode == PathMode::Acyclic => "ACYCLIC",
        1 if excludes(end.labels.as_deref(), &schema.slot(*from).ty) => {
            "the end's labels exclude the start"
        }
        1 => {
            return refuse(
                5,
                "lo = 1 without ACYCLIC: a cycle can return to the start, which the node BFS never reports again",
            );
        }
        lo => {
            return refuse(
                5,
                format!("lo = {lo}: the node BFS sees a node at its first depth only"),
            );
        }
    };
    // Rule 6: nothing tested inside the repeat.
    if a.states[1..3]
        .iter()
        .any(|test| test.labels.is_some() || test.filter.is_some())
    {
        return refuse(
            6,
            "a label or predicate inside the repeat tests every intermediate node; the node BFS tests the end only, after the walk",
        );
    }
    // Rule 7: within the BFS's depth ceiling.
    let max = match repeat.max {
        Some(hi) if hi <= 64 => hi,
        Some(hi) => return refuse(7, format!("hi = {hi}: the node BFS stops at 64 hops")),
        None => return refuse(7, "an unbounded quantifier: the node BFS stops at 64 hops"),
    };
    Ok(Reach {
        from: *from,
        to: end.bind,
        types: written,
        direction: step.direction,
        min: repeat.min,
        max,
        mode: a.mode,
        end_labels: end.labels.clone(),
        end_filter: end.filter,
        consumer,
        lower,
    })
}

/// Do the end's `labels` exclude every collection a start of type `start`
/// may be in? Unknown -- an unlabelled start or end -- is no.
fn excludes(labels: Option<&[CollectionId]>, start: &ValueType) -> bool {
    match (labels, start) {
        (Some(labels), ValueType::Node(starts)) if !starts.is_empty() => {
            starts.iter().all(|c| !labels.contains(c))
        }
        _ => false,
    }
}

/// The slots bound by the operators before `all[at]` in its stage.
fn bound_before(all: &[&Op], at: usize) -> Vec<SlotId> {
    let mut bound = Vec::new();
    for op in &all[..at] {
        match op {
            Op::Seed { out, .. } => bound.push(*out),
            Op::Expand { edge, to, .. } => {
                bound.extend(*edge);
                if let Target::New(slot) = to {
                    bound.push(*slot);
                }
            }
            Op::PathSearch { automaton, .. } => {
                bound.extend(super::super::automaton::binds(automaton))
            }
            Op::Reach(reach) => bound.extend(reach.to),
            Op::Project { cols, .. } => bound = (0..cols.len() as u16).map(SlotId).collect(),
            Op::Union(union) => bound = (0..union.columns).map(SlotId).collect(),
            Op::Table(TableOp::Aggregate { keys, aggs, .. }) => {
                bound = (0..(keys.len() + aggs.len()) as u16).map(SlotId).collect();
            }
            Op::Table(TableOp::Let { assign }) => {
                bound.extend(assign.iter().map(|(slot, _)| *slot))
            }
            Op::Table(TableOp::Unnest { out, .. }) => bound.push(*out),
            // Its inner steps follow it in `flat` order and bind there: the
            // slots it introduces are not bound before they do.
            Op::Call { outputs, .. } => bound.extend(outputs.iter().copied()),
            Op::Optional { .. } | Op::Exists { .. } | Op::Filter { .. } | Op::Table(_) => {}
        }
    }
    bound
}

/// The first slot of `hidden` an operator after `all[at]` reads, up to the
/// projection that ends the stage's rows.
fn read_before_projection(
    all: &[&Op],
    at: usize,
    program: &Program,
    hidden: &[SlotId],
) -> Option<SlotId> {
    for op in &all[at + 1..] {
        let reads = reads(op, program);
        if let Some(slot) = reads.iter().find(|slot| hidden.contains(slot)) {
            return Some(*slot);
        }
        if matches!(
            op,
            Op::Project { .. } | Op::Union(_) | Op::Table(TableOp::Aggregate { .. })
        ) {
            return None;
        }
    }
    None
}

/// The slots `op`'s expressions and seeds read. A `Distinct`, which
/// compares whole rows, reads none here: the planner puts one only after
/// the `Project` that ends [`read_before_projection`]'s scan.
fn reads(op: &Op, program: &Program) -> Vec<SlotId> {
    let expr = |id: &ExprId| program.get(*id).refs();
    match op {
        Op::Seed { source, .. } => match source {
            SeedSource::Key { key, .. } => expr(key),
            SeedSource::Index { seed } => program.seed_value(*seed).refs(),
            SeedSource::Bound { slot, .. } => vec![*slot],
            SeedSource::Scan { .. } => Vec::new(),
        },
        Op::Expand { from, to, hop, .. } => {
            let mut out = vec![*from];
            if let Target::Bound(slot) = to {
                out.push(*slot);
            }
            out.extend(hop.edge_filter.iter().chain(&hop.far_filter).flat_map(expr));
            out
        }
        Op::Filter { predicate, .. } => expr(predicate),
        Op::Project { cols, .. } => cols.iter().flat_map(expr).collect(),
        Op::PathSearch {
            from,
            automaton,
            search,
            ..
        } => {
            let mut out = vec![*from];
            out.extend(automaton_reads(automaton).iter().flat_map(expr));
            out.extend(super::super::automaton::binds(automaton));
            if let PathSearch::Cheapest { cost } = search {
                out.extend(expr(cost));
            }
            out
        }
        Op::Reach(reach) => {
            let mut out = vec![reach.from];
            out.extend(reach.end_filter.iter().flat_map(expr));
            out
        }
        Op::Table(table) => match table {
            TableOp::Let { assign } => assign.iter().flat_map(|(_, id)| expr(id)).collect(),
            TableOp::Unnest { list, .. } => expr(list),
            TableOp::Aggregate { keys, aggs, .. } => keys
                .iter()
                .chain(aggs.iter().filter_map(agg_arg))
                .flat_map(expr)
                .collect(),
            TableOp::Sort { keys, .. } => keys.iter().flat_map(|key| expr(&key.expr)).collect(),
            TableOp::Distinct | TableOp::Page { .. } => Vec::new(),
        },
        // Its inner steps follow it in `flat` order and are read there; a
        // union's branches are flows of their own (`choose`).
        Op::Optional { .. } | Op::Exists { .. } | Op::Union(_) => Vec::new(),
        // A CALL body is not listed in `flat` (its path searches keep the
        // full search): what it reads, it reads here.
        Op::Call { inner, .. } => inner.iter().flat_map(|op| reads(op, program)).collect(),
    }
}

/// Every predicate an automaton evaluates.
fn automaton_reads(automaton: &PathAutomaton) -> Vec<ExprId> {
    let nodes = automaton.states.iter().filter_map(|test| test.filter);
    let edges = automaton.links.iter().filter_map(|link| match link {
        PathLink::Edge(step) => step.filter,
        PathLink::Same => None,
    });
    nodes.chain(edges).collect()
}

fn agg_arg(agg: &AggSpec) -> Option<&ExprId> {
    match agg {
        AggSpec::CountRows => None,
        AggSpec::Count { arg, .. }
        | AggSpec::Sum(arg)
        | AggSpec::Avg(arg)
        | AggSpec::Min(arg)
        | AggSpec::Max(arg)
        | AggSpec::ArrayAgg { arg, .. } => Some(arg),
    }
}

/// Rule 4 for the search at `all[at]`: what makes its rows a set, or why
/// nothing does.
fn consumer(all: &[&Op], at: usize, search: PathSearch) -> Result<&'static str, Refusal> {
    let mut consumer = match search {
        PathSearch::Any => Some("ANY"),
        PathSearch::Shortest => Some("ANY SHORTEST"),
        PathSearch::Cheapest { .. } => {
            return refuse(
                4,
                "ANY CHEAPEST evaluates COST on every edge it relaxes, and a bad cost is an error the node BFS would not raise",
            );
        }
        PathSearch::Enumerate => None,
    };
    for op in &all[at + 1..] {
        match op {
            Op::Table(TableOp::Page { .. }) => {
                return refuse(
                    4,
                    "a LIMIT or OFFSET keeps rows in the order they arrive, and the node BFS delivers them in another",
                );
            }
            Op::Table(TableOp::Aggregate { aggs, .. }) => {
                if aggs
                    .iter()
                    .any(|agg| matches!(agg, AggSpec::ArrayAgg { .. }))
                {
                    return refuse(
                        4,
                        "ARRAY_AGG keeps rows in the order they arrive, and the node BFS delivers them in another",
                    );
                }
                if consumer.is_none() {
                    if aggs.iter().any(|agg| {
                        !matches!(
                            agg,
                            AggSpec::Count { distinct: true, .. }
                                | AggSpec::Min(_)
                                | AggSpec::Max(_)
                        )
                    }) {
                        return refuse(
                            4,
                            "an aggregate that counts or sums rows sees one row per path, and the node BFS gives one per end",
                        );
                    }
                    consumer = Some("an aggregate of identity folds");
                }
            }
            Op::Table(TableOp::Distinct) if consumer.is_none() => consumer = Some("a Distinct"),
            Op::Union(_) => {
                return refuse(
                    4,
                    "a UNION after it reads its rows in branches, which may count them or keep their order",
                );
            }
            _ => {}
        }
    }
    consumer.map_or_else(
        || {
            refuse(
                4,
                "every path is a row and nothing after it removes duplicates: the node BFS gives one row per end",
            )
        },
        Ok,
    )
}
