//! The operators a GQL plan instantiates (`docs/lang/GQL_PROFILE_DESIGN.md`
//! §3.1, §3.2): pull-based, one row at a time.
//!
//! [`build`] turns an [`OpSpec`] into a tree of [`Operator`]s, checking that
//! every slot an operator names lies inside its row. Each operator pulls
//! its input when it needs another row and charges the page's meter for
//! everything it reads. The pattern operators keep nothing between rows
//! beyond their current input row and one bounded refill; the blocking ones
//! keep what [`Held`] accounts for.
//!
//! | file | operators |
//! | --- | --- |
//! | `seed.rs` | `Seed` (key, index, bound, scan) |
//! | `expand.rs` | `Expand`, `ExpandInto` |
//! | `filter.rs` | `Filter` |
//! | `project.rs` | `Unit`, `Let`, `Project` |
//! | `aggregate.rs` | `Aggregate` |
//! | `distinct.rs` | `Distinct` |
//! | `sort.rs` | `Sort`, and its top-k form under a `Page` |
//! | `page.rs` | `Page` |
//! | `unnest.rs` | `Unnest` |
//! | `optional.rs` | `OptionalApply`, and its `Argument` leaf |
//! | `exists.rs` | `ExistsApply` (filter and mark forms) |
//! | `call.rs` | `CallApply` |
//! | `union.rs` | `Union`, `Buffered` and its `Replay` leaf |
//! | `reach.rs` | `Reach`: the existing node BFS, where the planner proved it |
//! | `../paths/enumerate.rs` | `PathSearch::Enumerate` |
//! | `../paths/bfs.rs`, `../paths/dijkstra.rs` | `PathSearch::Any`, `Shortest`, `Cheapest` |

mod aggregate;
mod call;
mod distinct;
mod exists;
mod expand;
mod filter;
mod optional;
mod page;
mod project;
mod reach;
mod seed;
mod sort;
mod union;
mod unnest;

pub(super) use expand::{ranges, refill, Range, REFILL_POSTINGS};

use super::super::{invalid_query, PreparedQuery, QueryResult, WorkResource};
use super::host::{EvalCx, ExecMeter, ExprId, GqlHost, SeedId, Truth};
use super::paths::{Enumerate, Select};
use super::budget::MEMORY;
use super::plan::{CountExpr, ExistsMode, OpSpec, PathSearch, SeedSource, Target};
use super::reader::ElementReader;
use super::value::{BindingRow, BindingValue, SlotId};
use crate::collections::{Database, EntityId};
use crate::index::graph::MAX_BFS_DEPTH;

/// One operator of an executing plan.
pub(super) trait Operator<'q> {
    /// The next row, or `None` once the stream is complete. Charges the
    /// meter for everything it reads.
    fn next(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<BindingRow>>;
}

pub(super) type Op<'q> = Box<dyn Operator<'q> + 'q>;

/// What every operator runs against during one page: the database (one
/// snapshot for the whole execution), the host, the execution's parameters,
/// the page's meter and the page's number.
pub(super) struct ExecCx<'q, 'x, 'm> {
    pub(super) db: &'q Database,
    pub(super) host: &'q dyn GqlHost,
    pub(super) params: &'x [BindingValue],
    pub(super) meter: &'x mut ExecMeter<'m>,
    /// The cursor's page count, from 1: a [`Held`] that sees a new number
    /// charges what it holds to this page's meter, which started empty.
    pub(super) page: u64,
    /// The input row an apply hands its inner side, until the inner side's
    /// `Argument` leaf takes it (`optional.rs`).
    pub(super) argument: Option<BindingRow>,
    /// The incoming table a `Buffered` lends its union while it pulls it,
    /// which the branches' `Replay` leaves read (`union.rs`).
    pub(super) replay: Option<Vec<BindingRow>>,
}

/// The memory a blocking operator -- or a path search's stack, frontier
/// and predecessor arcs -- holds
/// across rows and pages, per resource, and the page it was last charged
/// to.
///
/// Memory is metered per PAGE (each page's meter starts at zero), but a
/// blocking operator's state outlives a page. So at its first call in each
/// page it charges everything it still holds again ([`Held::recharge`]):
/// a page that cannot afford what the execution already holds is refused,
/// by name, before it does anything more.
///
/// `Groups` is the existing base resource, which the base meter cannot give
/// back within a page: a released group leaves that page's count as it was,
/// and only the pages after it see fewer.
#[derive(Default)]
pub(super) struct Held {
    sort_bytes: u64,
    list_bytes: u64,
    groups: u64,
    queue_entries: u64,
    predecessor_arcs: u64,
    page: u64,
}

impl Held {
    fn count(&mut self, resource: WorkResource) -> &mut u64 {
        match resource {
            WorkResource::SortBytes => &mut self.sort_bytes,
            WorkResource::ListBytes => &mut self.list_bytes,
            WorkResource::Groups => &mut self.groups,
            WorkResource::QueueEntries => &mut self.queue_entries,
            WorkResource::PredecessorArcs => &mut self.predecessor_arcs,
            _ => unreachable!("{resource:?} is not held by a blocking operator"),
        }
    }

    /// At the first call in a page, charge what is held to that page.
    pub(super) fn recharge(&mut self, cx: &mut ExecCx<'_, '_, '_>) -> QueryResult<()> {
        if self.page == cx.page {
            return Ok(());
        }
        self.page = cx.page;
        for (resource, amount) in [
            (WorkResource::SortBytes, self.sort_bytes),
            (WorkResource::ListBytes, self.list_bytes),
            (WorkResource::Groups, self.groups),
            (WorkResource::QueueEntries, self.queue_entries),
            (WorkResource::PredecessorArcs, self.predecessor_arcs),
        ] {
            if amount > 0 {
                cx.meter.charge(resource, amount)?;
            }
        }
        Ok(())
    }

    /// Hold `amount` more of `resource`, or refuse it by name.
    pub(super) fn charge(
        &mut self,
        cx: &mut ExecCx<'_, '_, '_>,
        resource: WorkResource,
        amount: u64,
    ) -> QueryResult<()> {
        cx.meter.charge(resource, amount)?;
        *self.count(resource) += amount;
        Ok(())
    }

    /// Stop holding `amount` of `resource`.
    pub(super) fn release(
        &mut self,
        cx: &mut ExecCx<'_, '_, '_>,
        resource: WorkResource,
        amount: u64,
    ) {
        let held = self.count(resource);
        *held = held.saturating_sub(amount);
        if resource != WorkResource::Groups {
            cx.meter.release(resource, amount);
        }
    }

    /// Stop holding everything: the operator's state is dropped.
    pub(super) fn release_all(&mut self, cx: &mut ExecCx<'_, '_, '_>) {
        for resource in [
            WorkResource::SortBytes,
            WorkResource::ListBytes,
            WorkResource::Groups,
            WorkResource::QueueEntries,
            WorkResource::PredecessorArcs,
        ] {
            let amount = *self.count(resource);
            self.release(cx, resource, amount);
        }
    }
}

impl<'q, 'm> ExecCx<'q, '_, 'm> {
    fn eval_cx(&mut self) -> EvalCx<'_, 'm> {
        EvalCx {
            reader: ElementReader::new(self.db),
            params: self.params,
            meter: &mut *self.meter,
        }
    }

    pub(super) fn eval(&mut self, expr: ExprId, row: &BindingRow) -> QueryResult<BindingValue> {
        let host = self.host;
        host.eval(expr, row, &mut self.eval_cx())
    }

    /// Does `expr` hold for `row`? Only `True` does: `Unknown` does not.
    pub(super) fn holds(&mut self, expr: ExprId, row: &BindingRow) -> QueryResult<bool> {
        let host = self.host;
        Ok(host.test(expr, row, &mut self.eval_cx())? == Truth::True)
    }

    fn open_seed(
        &mut self,
        seed: SeedId,
        row: &BindingRow,
    ) -> QueryResult<Option<PreparedQuery<'q>>> {
        let (host, db) = (self.host, self.db);
        host.open_seed(seed, db, row, &mut self.eval_cx())
    }
}

/// The operator tree for `spec`, and the width of the rows it produces.
/// `params` are the execution's: an `OFFSET` / `LIMIT` parameter is read,
/// and range-checked, here.
pub(super) fn build<'q>(spec: &'q OpSpec, params: &[BindingValue]) -> QueryResult<(Op<'q>, u16)> {
    build_in(spec, params, Scope::Top)
}

/// The kind of apply whose inner side is being built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InnerKind {
    Optional,
    Exists,
    Call,
}

/// Where an operator is being built, which decides the leaf it may stand on
/// and the operators it may use.
#[derive(Clone, Copy, Debug)]
pub(super) enum Scope {
    /// Outside every inner side and buffered branch: the leaf is `Unit`.
    Top,
    /// The inner side of an apply of this kind, whose input rows are this
    /// wide: the leaf is `Argument`. An inner side runs again from each
    /// input row. An `OptionalApply`'s or `ExistsApply`'s holds only
    /// streaming operators; a `CallApply`'s may also hold blocking ones,
    /// because its tree is built afresh per input row.
    Inner(InnerKind, u16),
    /// A branch of a buffered union, whose incoming table is this wide: the
    /// leaf is `Replay`.
    Replayed(u16),
}

/// [`build`], in `scope`.
pub(super) fn build_in<'q>(
    spec: &'q OpSpec,
    params: &[BindingValue],
    scope: Scope,
) -> QueryResult<(Op<'q>, u16)> {
    refuse_in(spec, scope)?;
    let build = |spec: &'q OpSpec, params: &[BindingValue]| build_in(spec, params, scope);
    Ok(match spec {
        OpSpec::Unit { width } => (Box::new(project::Unit::new(*width)), *width),
        OpSpec::Argument { width } => match scope {
            Scope::Inner(_, input) if input == *width => (Box::new(optional::Argument), *width),
            Scope::Inner(_, input) => {
                return Err(invalid_query(format!(
                    "an Argument of {width} slots under an apply whose input rows have {input}"
                )))
            }
            _ => {
                return Err(invalid_query(
                    "an Argument stands only at the leaf of an apply's inner side",
                ))
            }
        },
        OpSpec::Replay { width } => match scope {
            Scope::Replayed(table) if table == *width => (Box::new(union::Replay::new()), *width),
            Scope::Replayed(table) => {
                return Err(invalid_query(format!(
                    "a Replay of {width} slots over an incoming table of {table}"
                )))
            }
            _ => {
                return Err(invalid_query(
                    "a Replay stands only at the leaf of a branch under a Buffered",
                ))
            }
        },
        OpSpec::OptionalApply {
            input,
            inner,
            introduced,
        } => {
            let (input, width) = build(input, params)?;
            for slot in introduced.iter() {
                inside(width, *slot)?;
            }
            let (inner, _) = build_in(inner, params, Scope::Inner(InnerKind::Optional, width))?;
            (
                Box::new(optional::OptionalApply::new(input, inner, introduced)),
                width,
            )
        }
        OpSpec::ExistsApply { input, inner, mode } => {
            let (input, width) = build(input, params)?;
            if let ExistsMode::Mark { slot } = mode {
                inside(width, *slot)?;
            }
            let (inner, _) = Fresh::build(inner, params, InnerKind::Exists, width)?;
            (
                Box::new(exists::ExistsApply::new(input, inner, *mode)),
                width,
            )
        }
        OpSpec::CallApply {
            input,
            inner,
            outputs,
        } => {
            let (input, width) = build(input, params)?;
            for slot in outputs.iter() {
                inside(width, *slot)?;
            }
            let mut sorted = outputs.to_vec();
            sorted.sort_unstable();
            if sorted.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err(invalid_query("a CallApply names an output slot twice"));
            }
            let (inner, returned) = Fresh::build(inner, params, InnerKind::Call, width)?;
            if usize::from(returned) != outputs.len() {
                return Err(invalid_query(format!(
                    "a CallApply writes {} output slots from rows of {returned}",
                    outputs.len()
                )));
            }
            (Box::new(call::CallApply::new(input, inner, outputs)), width)
        }
        OpSpec::Union { branches, width } => {
            let branches = branches_of(branches, *width, params, scope)?;
            (Box::new(union::Union::new(branches)), *width)
        }
        OpSpec::Buffered { input, union } => {
            let (input, table) = build(input, params)?;
            let OpSpec::Union { branches, width } = &**union else {
                return Err(invalid_query("a Buffered stands over a Union"));
            };
            let branches = branches_of(branches, *width, params, Scope::Replayed(table))?;
            (
                Box::new(union::Buffered::new(input, union::Union::new(branches))),
                *width,
            )
        }
        OpSpec::Seed { input, out, source } => {
            let (input, width) = build(input, params)?;
            inside(width, *out)?;
            match source {
                SeedSource::Bound { slot, .. } => inside(width, *slot)?,
                SeedSource::Key { labels, .. } | SeedSource::Scan { labels } => distinct(labels)?,
                SeedSource::Index { .. } => {}
            }
            (Box::new(seed::Seed::new(input, *out, source)), width)
        }
        OpSpec::Expand {
            input,
            from,
            edge,
            to,
            step,
        } => {
            let (input, width) = build(input, params)?;
            inside(width, *from)?;
            let (Target::New(far) | Target::Bound(far)) = to;
            inside(width, *far)?;
            match edge {
                Some(edge) => inside(width, *edge)?,
                None if step.edge_filter.is_some() => {
                    return Err(invalid_query("an edge predicate needs the edge in a slot"));
                }
                None => {}
            }
            if let Some(types) = &step.types {
                distinct(types)?;
            }
            // An existence test stops at its first row: its hops start
            // with a one-posting refill (`expand.rs`).
            let first_refill = match scope {
                Scope::Inner(InnerKind::Exists, _) => 1,
                _ => REFILL_POSTINGS,
            };
            (
                Box::new(expand::Expand::new(
                    input,
                    *from,
                    *edge,
                    *to,
                    step,
                    first_refill,
                )),
                width,
            )
        }
        OpSpec::Filter { input, predicate } => {
            let (input, width) = build(input, params)?;
            (Box::new(filter::Filter::new(input, *predicate)), width)
        }
        OpSpec::Let { input, assign } => {
            let (input, width) = build(input, params)?;
            for (slot, _) in assign.iter() {
                inside(width, *slot)?;
            }
            (Box::new(project::Let::new(input, assign)), width)
        }
        OpSpec::Project { input, cols, width } => {
            let (input, _) = build(input, params)?;
            if cols.len() > usize::from(*width) {
                return Err(invalid_query("a projection has more columns than its row"));
            }
            (Box::new(project::Project::new(input, cols, *width)), *width)
        }
        OpSpec::Aggregate {
            input,
            keys,
            aggs,
            width,
        } => {
            let (input, _) = build(input, params)?;
            if keys.len() + aggs.len() > usize::from(*width) {
                return Err(invalid_query("an aggregate has more columns than its row"));
            }
            (
                Box::new(aggregate::Aggregate::new(input, keys, aggs, *width)),
                *width,
            )
        }
        OpSpec::Distinct { input } => {
            let (input, width) = build(input, params)?;
            (Box::new(distinct::Distinct::new(input)), width)
        }
        OpSpec::Sort { input, keys } => {
            let (input, width) = build(input, params)?;
            (Box::new(sort::Sort::new(input, keys, None)), width)
        }
        OpSpec::Page {
            input,
            offset,
            limit,
        } => {
            let offset = match offset {
                Some(count) => count_value(*count, params)?,
                None => 0,
            };
            let limit = limit.map(|count| count_value(count, params)).transpose()?;
            let (input, width) = match (&**input, limit) {
                // Top-k: only the first `offset + limit` sorted rows can
                // come out of the page, so the sort keeps no more.
                (OpSpec::Sort { input, keys }, Some(limit)) => {
                    let (input, width) = build(input, params)?;
                    let keep = offset.saturating_add(limit);
                    let sort: Op<'q> = Box::new(sort::Sort::new(input, keys, Some(keep)));
                    (sort, width)
                }
                _ => build(input, params)?,
            };
            (Box::new(page::Page::new(input, offset, limit)), width)
        }
        OpSpec::Unnest { input, list, out } => {
            let (input, width) = build(input, params)?;
            inside(width, *out)?;
            (Box::new(unnest::Unnest::new(input, *list, *out)), width)
        }
        OpSpec::Reach {
            input,
            from,
            to,
            spec,
        } => {
            let (input, width) = build(input, params)?;
            inside(width, *from)?;
            match to {
                Some(to) => inside(width, *to)?,
                None if spec.end_filter.is_some() => {
                    return Err(invalid_query("an end predicate needs the end in a slot"));
                }
                None => {}
            }
            if spec.types.as_ref().is_some_and(|types| types.len() > 1) {
                return Err(invalid_query("the node BFS walks one edge type"));
            }
            if spec.min_hops > 1
                || spec.max_hops as usize > MAX_BFS_DEPTH
                || spec.min_hops > spec.max_hops
            {
                return Err(invalid_query(format!(
                    "the node BFS starts at 0 or 1 hops and stops by {MAX_BFS_DEPTH}, not {}..{}",
                    spec.min_hops, spec.max_hops
                )));
            }
            if let Some(labels) = &spec.end_labels {
                distinct(labels)?;
            }
            (Box::new(reach::Reach::new(input, *from, *to, spec)), width)
        }
        OpSpec::PathSearch {
            input,
            from,
            automaton,
            search,
        } => {
            let (input, width) = build(input, params)?;
            inside(width, *from)?;
            automaton.check(width, *search != PathSearch::Enumerate)?;
            let op: Op<'q> = match search {
                PathSearch::Enumerate => Box::new(Enumerate::new(input, *from, automaton)),
                PathSearch::Any | PathSearch::Shortest => {
                    Box::new(Select::new(input, *from, automaton, None)?)
                }
                PathSearch::Cheapest { cost } => {
                    Box::new(Select::new(input, *from, automaton, Some(*cost))?)
                }
            };
            (op, width)
        }
    })
}

/// Refuse `spec` where `scope` does not admit it (the placement rules of
/// [`OpSpec`]'s module documentation).
fn refuse_in(spec: &OpSpec, scope: Scope) -> QueryResult<()> {
    let refused = match (scope, spec) {
        (_, OpSpec::Union { .. } | OpSpec::Buffered { .. }) => match scope {
            Scope::Top => None,
            Scope::Inner(..) => Some("an apply's inner side holds no Union and no Buffered"),
            Scope::Replayed(_) => Some("a branch after NEXT holds no Union and no Buffered"),
        },
        (Scope::Inner(InnerKind::Optional | InnerKind::Exists, _), spec) => match spec {
            OpSpec::Unit { .. }
            | OpSpec::Project { .. }
            | OpSpec::Aggregate { .. }
            | OpSpec::Distinct { .. }
            | OpSpec::Sort { .. }
            | OpSpec::Page { .. }
            | OpSpec::CallApply { .. }
            | OpSpec::Replay { .. } => Some(
                "the inner side of an OptionalApply or an ExistsApply holds only Argument, Seed, Expand, Filter, Let, Unnest, PathSearch, Reach, OptionalApply and ExistsApply",
            ),
            _ => None,
        },
        (Scope::Inner(InnerKind::Call, _) | Scope::Replayed(_), OpSpec::Unit { .. }) => {
            Some("a Unit stands only at the leaf of a plan's first part")
        }
        _ => None,
    };
    match refused {
        Some(message) => Err(invalid_query(message)),
        None => Ok(()),
    }
}

/// The operators of a union's `branches`, built in `scope`, each giving rows
/// of `width` slots.
fn branches_of<'q>(
    branches: &'q [OpSpec],
    width: u16,
    params: &[BindingValue],
    scope: Scope,
) -> QueryResult<Vec<Op<'q>>> {
    branches
        .iter()
        .map(|branch| {
            let (op, returned) = build_in(branch, params, scope)?;
            if returned != width {
                return Err(invalid_query(format!(
                    "a union branch gives rows of {returned} slots, not {width}"
                )));
            }
            Ok(op)
        })
        .collect()
}

/// An apply's inner side that is built AFRESH for each input row
/// (`ExistsApply`, `CallApply`): one stopped early, or holding a blocking
/// operator's state, cannot restart in place.
///
/// A dropped tree cannot give back what its operators held, since they
/// release through the meter only as they run. So `Fresh` watches the
/// meter's held memory across each pull of the tree -- only the tree runs
/// then -- and when it drops the tree it gives back what the tree still
/// held in this page. (A `groups` charge is the base meter's and stays
/// counted for the page, as for every operator.)
pub(super) struct Fresh<'q> {
    spec: &'q OpSpec,
    kind: InnerKind,
    width: u16,
    /// The tree for the current input row. The one built when the cursor
    /// opened -- which checked the plan -- serves the first row; `None`
    /// once a row's tree is dropped.
    tree: Option<Op<'q>>,
    /// What the running tree holds in the meter, per memory resource, and
    /// the page that meter belongs to.
    held: [u64; MEMORY.len()],
    page: u64,
}

impl<'q> Fresh<'q> {
    /// The inner side `spec` of an apply of `kind` over rows `width` wide,
    /// checked, and the width of the rows it gives.
    fn build(
        spec: &'q OpSpec,
        params: &[BindingValue],
        kind: InnerKind,
        width: u16,
    ) -> QueryResult<(Self, u16)> {
        let (tree, returned) = build_in(spec, params, Scope::Inner(kind, width))?;
        Ok((
            Self {
                spec,
                kind,
                width,
                tree: Some(tree),
                held: [0; MEMORY.len()],
                page: 0,
            },
            returned,
        ))
    }

    /// Start the inner side from input row `row`: a fresh tree, whose
    /// `Argument` leaf takes the row.
    pub(super) fn start(
        &mut self,
        cx: &mut ExecCx<'q, '_, '_>,
        row: &BindingRow,
    ) -> QueryResult<()> {
        if self.tree.is_none() {
            let scope = Scope::Inner(self.kind, self.width);
            self.tree = Some(build_in(self.spec, cx.params, scope)?.0);
        }
        self.held = [0; MEMORY.len()];
        self.page = cx.page;
        cx.argument = Some(row.clone());
        Ok(())
    }

    /// The running tree's next row, keeping count of what it holds.
    pub(super) fn next(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<BindingRow>> {
        let Some(tree) = &mut self.tree else {
            return Ok(None);
        };
        if self.page != cx.page {
            // A new page's meter holds nothing yet; the tree charges what it
            // holds again as it runs.
            self.page = cx.page;
            self.held = [0; MEMORY.len()];
        }
        let before = cx.meter.held();
        let row = tree.next(cx)?;
        let after = cx.meter.held();
        for ((held, before), after) in self.held.iter_mut().zip(before).zip(after) {
            *held = (*held + after).saturating_sub(before);
        }
        Ok(row)
    }

    /// Drop the running tree, giving back what it still held.
    pub(super) fn stop(&mut self, cx: &mut ExecCx<'q, '_, '_>) {
        self.tree = None;
        cx.argument = None;
        if self.page == cx.page {
            for (resource, held) in MEMORY.into_iter().zip(self.held) {
                if held > 0 {
                    cx.meter.release(resource, held);
                }
            }
        }
        self.held = [0; MEMORY.len()];
    }
}

/// An `OFFSET` / `LIMIT` count: an integer in `0..=i64::MAX` (design Q13).
fn count_value(count: CountExpr, params: &[BindingValue]) -> QueryResult<u64> {
    let value = match count {
        CountExpr::Lit(n) => Some(n),
        CountExpr::Param(n) => match params.get(n) {
            Some(BindingValue::Int(i)) => u64::try_from(*i).ok(),
            _ => None,
        },
    };
    value
        .filter(|n| i64::try_from(*n).is_ok())
        .ok_or_else(|| invalid_query("OFFSET and LIMIT take an integer from 0 to 9223372036854775807"))
}

/// Does `slot` lie inside a row of `width` slots?
pub(super) fn inside(width: u16, slot: SlotId) -> QueryResult<()> {
    if slot.0 < width {
        Ok(())
    } else {
        Err(invalid_query(format!(
            "slot {} is outside a row of {width} slots",
            slot.0
        )))
    }
}

/// A label or type named twice would walk its range twice and double every
/// row (or path) it gives.
pub(super) fn distinct<T: Ord>(items: &[T]) -> QueryResult<()> {
    let mut sorted: Vec<&T> = items.iter().collect();
    sorted.sort_unstable();
    if sorted.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(invalid_query("a label or edge type is named twice"));
    }
    Ok(())
}

/// The node in `slot` of `row`: `None` for `Null` -- a pattern from or into
/// nothing matches nothing -- and a refusal naming `what` for anything
/// else.
pub(super) fn node_in(row: &BindingRow, slot: SlotId, what: &str) -> QueryResult<Option<EntityId>> {
    match row.get(slot) {
        BindingValue::Node(node) => Ok(Some(node.0)),
        BindingValue::Null => Ok(None),
        other => Err(invalid_query(format!("{what} must be a node, not {other:?}"))),
    }
}
