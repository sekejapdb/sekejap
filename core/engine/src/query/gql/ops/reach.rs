//! `Reach`: the existing node BFS in place of a path search, once per input
//! row (`docs/lang/GQL_PROFILE_DESIGN.md` §4.6).
//!
//! The walk is the query engine's own `QueryFilter::Graph` BFS
//! (`drivers.rs`, `execute_graph`), run with no node predicate and no edge
//! predicate: one visited set on NODES, each node reported once at its
//! first depth, the start reported only at depth 0. Nothing here is a new
//! traversal. The planner uses it only where it has proved that nothing
//! downstream can tell its answer from the path search's (§4.6's seven
//! rules); this operator proves nothing and states only what it gives.
//!
//! The end test -- labels, then the predicate -- is applied to each node
//! the walk reached, AFTER the walk: a node the predicate rejects is still
//! walked through (rule 6). The labels are tested before the answer is
//! held, since they read nothing; the predicate is tested per row as it is
//! handed out, since it may read the row.
//!
//! Charges: what the BFS charges (`graph_edges` per posting walked,
//! `graph_visited` per node discovered, one `primary_reads` for a start
//! with no edge at all); `queue_entries` HELD, one per end not yet handed
//! out, charged again to each page; whatever the end predicate reads;
//! `binding_rows` 1 per row out.
//!
//! **Named cost.** The walk runs whole for one input row before its first
//! row comes out, and under the BFS's own fixed ceilings: 65,536 visited
//! nodes and 1,000,000 edges per walk. Past one of them the page is refused
//! -- never truncated -- where the path search it stands in for would have
//! gone on under the caller's budget.

use super::super::super::drivers::execute_graph;
use super::super::super::membership::NodeGate;
use super::super::super::plan::OwnedBfsRequest;
use super::super::super::{QueryResult, WorkResource};
use super::super::plan::ReachSpec;
use super::super::value::{BindingRow, BindingValue, NodeRef, SlotId};
use super::{node_in, ExecCx, Held, Op, Operator};
use crate::collections::EntityId;
use crate::index::graph::{MAX_BFS_EDGES, MAX_BFS_RESULTS, MAX_BFS_VISITED};

pub(super) struct Reach<'q> {
    input: Op<'q>,
    from: SlotId,
    to: Option<usize>,
    spec: &'q ReachSpec,
    /// The input row being answered, and the ends its walk found that are
    /// still to be handed out, in ascending id order. The last end tested
    /// is written into the row in place: only a row that passes is copied
    /// out.
    current: Option<(BindingRow, std::vec::IntoIter<EntityId>)>,
    held: Held,
}

impl<'q> Reach<'q> {
    pub(super) fn new(
        input: Op<'q>,
        from: SlotId,
        to: Option<SlotId>,
        spec: &'q ReachSpec,
    ) -> Self {
        Self {
            input,
            from,
            to: to.map(|slot| usize::from(slot.0)),
            spec,
            current: None,
            held: Held::default(),
        }
    }

    /// Every node the walk from `start` reaches, in ascending id order,
    /// the ones the end's labels exclude already dropped.
    fn walk(&self, cx: &mut ExecCx<'q, '_, '_>, start: EntityId) -> QueryResult<Vec<EntityId>> {
        let spec = self.spec;
        let edge_type = match spec.types.as_deref() {
            None => None,
            Some([edge_type]) => Some(*edge_type),
            // A type no write has used: nothing to cross (two types are
            // refused when the cursor opens).
            Some(_) => {
                let ends = if spec.min_hops == 0 {
                    vec![start]
                } else {
                    Vec::new()
                };
                return Ok(self.labelled(ends));
            }
        };
        let request = OwnedBfsRequest {
            seed: start,
            direction: spec.direction,
            context: spec.context,
            edge_type,
            min_depth: spec.min_hops as usize,
            max_depth: spec.max_hops as usize,
            include_seed: true,
            // The BFS's own ceilings, the largest `execute_graph` accepts.
            max_visited: MAX_BFS_VISITED,
            max_edges: MAX_BFS_EDGES,
            result_limit: MAX_BFS_RESULTS,
            edge_where: Vec::new(),
        };
        let answer = execute_graph(cx.db, &request, &NodeGate::empty(), false, cx.meter.base())?;
        Ok(self.labelled(answer.ids))
    }

    fn labelled(&self, mut ends: Vec<EntityId>) -> Vec<EntityId> {
        if let Some(labels) = self.spec.end_labels.as_deref() {
            ends.retain(|end| labels.contains(&end.collection));
        }
        ends
    }
}

impl<'q> Operator<'q> for Reach<'q> {
    fn next(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<BindingRow>> {
        self.held.recharge(cx)?;
        loop {
            if let Some((row, ends)) = &mut self.current {
                if let Some(end) = ends.next() {
                    self.held.release(cx, WorkResource::QueueEntries, 1);
                    if let Some(slot) = self.to {
                        row.slots[slot] = BindingValue::Node(NodeRef(end));
                    }
                    if let Some(predicate) = self.spec.end_filter {
                        if !cx.holds(predicate, row)? {
                            continue;
                        }
                    }
                    cx.meter.charge(WorkResource::BindingRows, 1)?;
                    return Ok(Some(row.clone()));
                }
                self.current = None;
            }
            let Some(row) = self.input.next(cx)? else {
                return Ok(None);
            };
            let Some(start) = node_in(&row, self.from, "a path pattern's start")? else {
                continue;
            };
            let ends = self.walk(cx, start)?;
            self.held
                .charge(cx, WorkResource::QueueEntries, ends.len() as u64)?;
            self.current = Some((row, ends.into_iter()));
        }
    }
}
