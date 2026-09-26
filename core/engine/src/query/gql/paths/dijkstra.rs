//! What `ANY CHEAPEST ... COST` adds to the selector search
//! (`docs/lang/GQL_PROFILE_DESIGN.md` §4.3, design Q4): the cost of one edge
//! crossing, validated, and the Dijkstra frontier.
//!
//! The search itself is the product-state search of `bfs.rs`; only its
//! frontier and its priority change. A state's priority is `(cost, hops,
//! sequence)`: the path cost, then the hop count, then the order the state
//! was created in. Every edge costs a positive amount and a `Same` move
//! costs nothing, so the priority never decreases along a path and the
//! first time a state is popped its priority is final (settled). Because
//! the hop counter `k` is part of the state, a cheaper arrival that used
//! more hops does not settle a dearer one that used fewer: brief §8.3's
//! bounded counterexample is answered by construction. No dominance
//! pruning is applied (§4.3). **Cost (named):** more states are settled
//! than strictly necessary, bounded by `path_states` and `queue_entries`.

use super::super::super::{invalid_query, QueryResult};
use super::super::value::{BindingValue, EdgeRef};
use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

/// The cost of crossing `edge`: `value`, the COST expression evaluated for
/// this relaxation, when it is a positive, finite number. Anything else --
/// zero, a negative number, `Null`, NaN, an infinity, or a value that is no
/// number -- is `InvalidPathCost` (Q4): never skipped, never clamped, and no
/// epsilon added. An `Int` is taken as the nearest `f64`.
pub(super) fn edge_cost(value: &BindingValue, edge: &EdgeRef) -> QueryResult<f64> {
    let cost = match value {
        BindingValue::Int(i) => Some(*i as f64),
        BindingValue::Float(f) => Some(*f),
        _ => None,
    };
    match cost {
        Some(cost) if cost.is_finite() && cost > 0.0 => Ok(cost),
        _ => Err(invalid_path_cost(
            value,
            edge,
            "is not a positive, finite number",
        )),
    }
}

/// `total` plus `cost`, unless the path's total stops being finite.
pub(super) fn add_cost(total: f64, cost: f64, edge: &EdgeRef) -> QueryResult<f64> {
    let sum = total + cost;
    if sum.is_finite() {
        Ok(sum)
    } else {
        Err(invalid_path_cost(
            &BindingValue::Float(cost),
            edge,
            "makes the path's total cost overflow",
        ))
    }
}

/// The named error (design §4.3). The wire maps an invalid input to
/// SQLSTATE `22023`.
fn invalid_path_cost(
    value: &BindingValue,
    edge: &EdgeRef,
    why: &str,
) -> super::super::super::QueryError {
    invalid_query(format!(
        "InvalidPathCost {{ value: {value:?}, edge: {:?} #{} }}: an ANY CHEAPEST COST {why}",
        edge.key, edge.id
    ))
}

/// The Dijkstra frontier: entries by `(cost, hops, walk, sequence)`, least
/// first. The sequence is the state's index in the search's record list,
/// which grows as states are created, so equal costs and hops pop in the
/// order they were found. A `walk` entry is a settled state's deferred edge
/// walk: it pops after every state of its priority (`bfs.rs`).
#[derive(Default)]
pub(super) struct CostHeap(BinaryHeap<Reverse<Entry>>);

struct Entry {
    cost: f64,
    hops: u32,
    walk: bool,
    record: usize,
}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.cost
            .total_cmp(&other.cost)
            .then(self.hops.cmp(&other.hops))
            .then(self.walk.cmp(&other.walk))
            .then(self.record.cmp(&other.record))
    }
}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Entry {}

impl CostHeap {
    pub(super) fn push(&mut self, record: usize, cost: f64, hops: u32, walk: bool) {
        self.0.push(Reverse(Entry {
            cost,
            hops,
            walk,
            record,
        }));
    }

    /// The least entry: its record, and whether it is a deferred walk.
    pub(super) fn pop(&mut self) -> Option<(usize, bool)> {
        self.0.pop().map(|Reverse(entry)| (entry.record, entry.walk))
    }

    pub(super) fn len(&self) -> usize {
        self.0.len()
    }
}
