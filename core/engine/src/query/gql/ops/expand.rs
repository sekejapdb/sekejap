//! `Expand` and `ExpandInto`: one hop per input row
//! (`docs/lang/GQL_PROFILE_DESIGN.md` §3.2).
//!
//! For each input row the hop walks one adjacency range per (direction,
//! edge type) -- outgoing before incoming, types in the plan's order -- and
//! emits one row per admitted edge: parallel edges are one row EACH. An
//! either-direction hop skips, in its incoming walk, an edge whose source is
//! its destination: a self-loop is one match, not two orientations.
//!
//! What admits an edge, cheapest first:
//!
//! 1. the far node's label -- its collection, carried in its identity, so
//!    the test reads nothing;
//! 2. for `ExpandInto`, the far node being the bound one -- an identity test;
//! 3. the inline edge predicate, through the host; reading an incoming
//!    edge's property is one primary-posting read, an outgoing edge carries
//!    its bag;
//! 4. the inline far-node predicate, through the host; reading the far row
//!    is one primary read.
//!
//! The walk is done in REFILLS of at most 256 postings, and between two
//! refills it holds no page: the adjacency cursor pins the leaf it stands on,
//! and every predicate and every operator downstream reads other pages
//! while this one waits for its next pull. So a refill walks, pauses the
//! cursor (`PausedAdjacency`, which keeps only the last key), and only then
//! are its edges tested and handed out.
//!
//! Inside the inner side of an `ExistsApply`, which stops at its first row,
//! a hop's first refill per input row walks ONE posting, and each refill
//! after it twice as many as the one before, up to 256: an existence test
//! over a node with many edges reads one posting past the seek. The cost of
//! that choice, named: a hop that must walk far pauses and resumes its
//! cursor up to eight times more per input row than a full refill would.
//!
//! Charges: `graph_edges` 1 per turn of the walk, the turn that finds the
//! range's end included (the schedule the existing traversal charges on);
//! `queue_entries` HELD, 1 per edge of the refill not yet handed out --
//! the path searches' rule for their own refill buffers -- charged again to
//! each page; whatever the predicates read; `binding_rows` 1 per row out.
//!
//! [`refill`] is the one refill walk: this hop's and both path searches'.

use super::super::super::{QueryResult, WorkResource};
use super::super::plan::{StepSpec, Target};
use super::super::value::{BindingRow, BindingValue, EdgeRef, NodeRef, SlotId};
use super::{node_in, ExecCx, Held, Op, Operator};
use crate::collections::{AdjacencyCursor, EntityId, PausedAdjacency};
use crate::index::graph::{Direction, EdgeTypeId, GraphContextId};
use std::collections::VecDeque;
use std::sync::Arc;

/// Postings one refill walks before it pauses: the bound on what a walk
/// holds between two pulls (§3.2).
pub(in super::super) const REFILL_POSTINGS: usize = 256;

/// One adjacency range: a direction and an edge type (`None`: every type).
pub(in super::super) type Range = (Direction, Option<EdgeTypeId>);

/// The adjacency ranges one edge step walks, in order: outgoing before
/// incoming, types in the step's order (`None`: every type).
pub(in super::super) fn ranges(direction: Direction, types: Option<&[EdgeTypeId]>) -> Box<[Range]> {
    let directions: &[Direction] = match direction {
        Direction::Both => &[Direction::Outgoing, Direction::Incoming],
        Direction::Outgoing => &[Direction::Outgoing],
        Direction::Incoming => &[Direction::Incoming],
    };
    let types: Vec<Option<EdgeTypeId>> = match types {
        None => vec![None],
        Some(types) => types.iter().copied().map(Some).collect(),
    };
    directions
        .iter()
        .flat_map(|direction| types.iter().map(move |edge_type| (*direction, *edge_type)))
        .collect()
}

/// Walk up to `postings` (at most [`REFILL_POSTINGS`]) postings of `range`
/// from `near`, resumed
/// from `paused` (or from the range's start), charging `graph_edges` 1 per
/// turn, and hand each admitted edge and its far node to `push`, which
/// charges and buffers it. Then set the walk down: the paused walk, or
/// `None` once the range has ended.
///
/// Admitted: not a self-loop met again in the incoming range of an
/// either-direction step (`step`), and the far node passing `admit`. The
/// bag is decoded only when `bag` says the edge lands in a slot; an
/// incoming posting has none to decode.
#[allow(clippy::too_many_arguments)]
pub(in super::super) fn refill<'q, 'x, 'm>(
    cx: &mut ExecCx<'q, 'x, 'm>,
    near: EntityId,
    context: GraphContextId,
    step: Direction,
    (direction, edge_type): Range,
    bag: bool,
    paused: Option<PausedAdjacency>,
    postings: usize,
    admit: impl Fn(EntityId) -> bool,
    mut push: impl FnMut(&mut ExecCx<'q, 'x, 'm>, EdgeRef, EntityId) -> QueryResult<()>,
) -> QueryResult<Option<PausedAdjacency>> {
    let mut cursor = match paused {
        Some(paused) => paused.resume(cx.db)?,
        None => AdjacencyCursor::open(cx.db, near, direction, context, edge_type)?,
    };
    // An either-direction step met every self-loop in its outgoing walk.
    let skip_loops = step == Direction::Both && direction == Direction::Incoming;
    let mut walked = 0;
    loop {
        let posting = cursor.next_posting()?;
        cx.meter.charge(WorkResource::GraphEdges, 1)?;
        let Some(posting) = posting else {
            return Ok(None);
        };
        let adjacent = posting.edge()?;
        walked += 1;
        if !(skip_loops && adjacent.key.source == adjacent.key.destination) && admit(adjacent.far)
        {
            let edge = EdgeRef {
                key: adjacent.key,
                id: adjacent.id,
                bag: if bag { adjacent.bag()?.map(Arc::new) } else { None },
            };
            push(cx, edge, adjacent.far)?;
        }
        if walked == postings {
            return Ok(Some(cursor.pause()?));
        }
    }
}

pub(super) struct Expand<'q> {
    input: Op<'q>,
    from: SlotId,
    edge: Option<usize>,
    to: Target,
    step: &'q StepSpec,
    /// The adjacency ranges one input row walks, in order.
    ranges: Box<[Range]>,
    /// The input row being expanded, and where its walk stands.
    current: Option<Walk>,
    /// Edges of the last refill not yet tested, with their far node.
    refill: VecDeque<(EdgeRef, EntityId)>,
    /// The refill buffer, held across pages.
    held: Held,
    /// Postings the first refill of each input row's walk takes: 1 inside
    /// an existence test, [`REFILL_POSTINGS`] elsewhere.
    first_refill: usize,
}

/// The hop from one input row.
struct Walk {
    /// The input row, with the last edge tested and its far node written
    /// in: the predicates read it in place, and only a row that passes is
    /// copied out.
    row: BindingRow,
    near: EntityId,
    /// `ExpandInto`: the node the far end must be.
    into: Option<EntityId>,
    /// The range being walked: an index into `Expand::ranges`, or past its
    /// end once every range is done.
    range: usize,
    /// Where the range's walk was set down; `None` before it starts.
    paused: Option<PausedAdjacency>,
    /// Postings the next refill takes.
    postings: usize,
}

impl<'q> Expand<'q> {
    pub(super) fn new(
        input: Op<'q>,
        from: SlotId,
        edge: Option<SlotId>,
        to: Target,
        step: &'q StepSpec,
        first_refill: usize,
    ) -> Self {
        Self {
            input,
            from,
            edge: edge.map(|slot| usize::from(slot.0)),
            to,
            step,
            ranges: ranges(step.direction, step.types.as_deref()),
            current: None,
            refill: VecDeque::new(),
            held: Held::default(),
            first_refill,
        }
    }

    /// The hop from input row `row`, or `None` when an end is `Null`: a hop
    /// from or into nothing matches nothing.
    fn walk(&self, row: BindingRow) -> QueryResult<Option<Walk>> {
        const END: &str = "an edge pattern's endpoint";
        let Some(near) = node_in(&row, self.from, END)? else {
            return Ok(None);
        };
        let into = match self.to {
            Target::Bound(slot) => match node_in(&row, slot, END)? {
                Some(into) => Some(into),
                None => return Ok(None),
            },
            Target::New(_) => None,
        };
        Ok(Some(Walk {
            row,
            near,
            into,
            range: 0,
            paused: None,
            postings: self.first_refill,
        }))
    }

    /// Walk the current range's next refill into `refill`, and move to the
    /// next range at its end.
    fn walk_refill(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<()> {
        let walk = self
            .current
            .as_mut()
            .expect("a refill walks the current row");
        let (step, into) = (self.step, walk.into);
        let (held, buffer) = (&mut self.held, &mut self.refill);
        let paused = refill(
            cx,
            walk.near,
            step.context,
            step.direction,
            self.ranges[walk.range],
            self.edge.is_some(),
            walk.paused.take(),
            walk.postings,
            |far| {
                step.far_labels
                    .as_deref()
                    .is_none_or(|labels| labels.contains(&far.collection))
                    && into.is_none_or(|into| into == far)
            },
            |cx, edge, far| {
                held.charge(cx, WorkResource::QueueEntries, 1)?;
                buffer.push_back((edge, far));
                Ok(())
            },
        )?;
        if paused.is_none() {
            walk.range += 1;
        }
        walk.paused = paused;
        walk.postings = (walk.postings * 2).min(REFILL_POSTINGS);
        Ok(())
    }
}

impl<'q> Operator<'q> for Expand<'q> {
    fn next(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<BindingRow>> {
        self.held.recharge(cx)?;
        loop {
            if let Some((edge, far)) = self.refill.pop_front() {
                self.held.release(cx, WorkResource::QueueEntries, 1);
                let walk = self
                    .current
                    .as_mut()
                    .expect("a refill belongs to the current row");
                if let Some(slot) = self.edge {
                    walk.row.slots[slot] = BindingValue::Edge(edge);
                }
                if let Target::New(slot) = self.to {
                    walk.row.slots[usize::from(slot.0)] = BindingValue::Node(NodeRef(far));
                }
                if let Some(predicate) = self.step.edge_filter {
                    if !cx.holds(predicate, &walk.row)? {
                        continue;
                    }
                }
                if let Some(predicate) = self.step.far_filter {
                    if !cx.holds(predicate, &walk.row)? {
                        continue;
                    }
                }
                cx.meter.charge(WorkResource::BindingRows, 1)?;
                return Ok(Some(walk.row.clone()));
            }
            match &self.current {
                Some(walk) if walk.range < self.ranges.len() => self.walk_refill(cx)?,
                _ => {
                    let Some(row) = self.input.next(cx)? else {
                        self.current = None;
                        return Ok(None);
                    };
                    self.current = self.walk(row)?;
                }
            }
        }
    }
}
