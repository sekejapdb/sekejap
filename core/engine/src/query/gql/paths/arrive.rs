//! What both path searches do the same way: the moves out of a state, the
//! tests of one arrival, and the tail of a witness row.
//!
//! On arrival at a state, in order: the label test (free: the collection is
//! in the node's identity), the bindings, the edge predicate, the node
//! predicate. A binding of a singleton slot already holding an element -- a
//! repeated variable, or a variable bound upstream -- is an identity test.
//! Every slot an arrival writes is logged with what it held, so the search
//! can put the row back: enumeration when it pops a frame, the selector
//! search after every arrival. The path-mode check comes before all of this
//! and is each search's own: enumeration checks its current path, the
//! selector search a record's predecessor chain.

use super::super::super::{QueryResult, WorkResource};
use super::super::ops::{ranges, ExecCx, Range};
use super::super::value::{BindingRow, BindingValue, EdgeRef, ListRef, NodeRef, PathRef};
use super::automaton::{Binding, EdgeStep, PathAutomaton, PathLink};
use crate::collections::EntityId;
use std::mem::replace;

/// The slots an arrival wrote, with what they held before, oldest first.
pub(super) type Undo = Vec<(usize, BindingValue)>;

/// One way out of a state.
#[derive(Clone, Copy)]
pub(super) enum Move {
    /// To state `to` on the same node, with counter `k`.
    Same { to: usize, k: u32 },
    /// Across the edge step of link `link`, counter unchanged.
    Edge { link: usize },
}

/// The moves out of `state` with counter `k`, in the order tried: at most
/// two, the missing ones last.
pub(super) fn moves(a: &PathAutomaton, state: usize, k: u32) -> [Option<Move>; 2] {
    if state + 1 == a.states.len() {
        return [None, None];
    }
    if let Some(r) = a.repeat_ending(state) {
        // An iteration completes here. An unbounded repeat's count saturates
        // at its minimum: past it, more iterations are indistinguishable
        // (§4.1).
        let done = k + 1;
        let again = r.max.is_none_or(|max| done < max).then(|| Move::Same {
            to: usize::from(r.first),
            k: if r.max.is_none() { done.min(r.min) } else { done },
        });
        let leave = (done >= r.min).then_some(Move::Same {
            to: state + 1,
            k: 0,
        });
        return first_two(again, leave);
    }
    if let Some(r) = a.repeat_after(state) {
        let enter = (r.max != Some(0)).then(|| Move::Same {
            to: usize::from(r.first),
            k: 0,
        });
        // Zero iterations: the state after the body, on the same node.
        let skip = (r.min == 0).then(|| Move::Same {
            to: usize::from(r.last) + 1,
            k: 0,
        });
        return first_two(enter, skip);
    }
    match a.links[state] {
        PathLink::Same => [Some(Move::Same { to: state + 1, k }), None],
        PathLink::Edge(_) => [Some(Move::Edge { link: state }), None],
    }
}

/// Two optional moves, the missing one last.
fn first_two(a: Option<Move>, b: Option<Move>) -> [Option<Move>; 2] {
    match a {
        Some(_) => [a, b],
        None => [b, None],
    }
}

pub(super) fn edge_step(a: &PathAutomaton, link: usize) -> &EdgeStep {
    match &a.links[link] {
        PathLink::Edge(step) => step,
        PathLink::Same => unreachable!("an edge move crosses an edge link"),
    }
}

pub(super) fn labels_admit(a: &PathAutomaton, state: usize, node: EntityId) -> bool {
    a.states[state]
        .labels
        .as_deref()
        .is_none_or(|labels| labels.contains(&node.collection))
}

/// Bind `value` at a position of `row`: a group slot takes it (and, when
/// `events` is given, lists it there); a singleton slot takes it when
/// empty, and otherwise must already hold the same element. False when
/// that identity test fails. A slot written is logged in `undo`.
fn bind(
    row: &mut BindingRow,
    undo: &mut Undo,
    events: Option<&mut Undo>,
    binding: Binding,
    value: BindingValue,
) -> bool {
    let held = &mut row.slots[binding.slot];
    if binding.group {
        if let Some(events) = events {
            events.push((binding.slot, value.clone()));
        }
    } else if !matches!(held, BindingValue::Null) {
        return held.identity_eq(&value) == Some(true);
    }
    undo.push((binding.slot, replace(held, value)));
    true
}

/// The tests of arriving at `state` on `node`, crossing `via` = (link,
/// edge) when given, over `row`: the label, the bindings (written into
/// `row` and logged in `undo`, group elements also in `events`), the edge
/// predicate, the node predicate. Whether every one holds; the writes stay
/// logged either way.
#[allow(clippy::too_many_arguments)]
pub(super) fn admit(
    cx: &mut ExecCx<'_, '_, '_>,
    a: &PathAutomaton,
    row: &mut BindingRow,
    undo: &mut Undo,
    mut events: Option<&mut Undo>,
    state: usize,
    node: EntityId,
    via: Option<(usize, &EdgeRef)>,
) -> QueryResult<bool> {
    if !labels_admit(a, state, node) {
        return Ok(false);
    }
    let mut edge_filter = None;
    if let Some((link, edge)) = via {
        let step = edge_step(a, link);
        edge_filter = step.filter;
        if let Some(binding) = a.edge_binding(link, step) {
            let value = BindingValue::Edge(edge.clone());
            if !bind(row, undo, events.as_deref_mut(), binding, value) {
                return Ok(false);
            }
        }
    }
    if let Some(binding) = a.node_binding(state) {
        if !bind(row, undo, events, binding, BindingValue::Node(NodeRef(node))) {
            return Ok(false);
        }
    }
    for predicate in [edge_filter, a.states[state].filter].into_iter().flatten() {
        if !cx.holds(predicate, row)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Put back what `undo` logged after its first `keep` entries, latest
/// first.
pub(super) fn restore(row: &mut BindingRow, undo: &mut Undo, keep: usize) {
    for (slot, old) in undo.drain(keep..).rev() {
        row.slots[slot] = old;
    }
}

/// The tail of a witness row: each group slot's list -- `lists` gives the
/// items of each of `a.groups`, in order -- and the path. The lists live
/// only while the row is built here, so their `list_bytes` are charged and
/// given back; an operator that keeps the row charges what it keeps.
pub(super) fn complete_row(
    cx: &mut ExecCx<'_, '_, '_>,
    a: &PathAutomaton,
    row: &mut BindingRow,
    lists: impl IntoIterator<Item = Vec<BindingValue>>,
    path: PathRef,
) -> QueryResult<()> {
    let mut list_bytes = 0;
    for (items, (slot, elem)) in lists.into_iter().zip(a.groups.iter()) {
        let list = BindingValue::List(ListRef {
            items: items.into(),
            elem: elem.clone(),
        });
        list_bytes += list.held_bytes();
        row.slots[usize::from(slot.0)] = list;
    }
    if let Some(slot) = a.path {
        row.slots[usize::from(slot.0)] = BindingValue::Path(path);
    }
    if list_bytes > 0 {
        cx.meter.charge(WorkResource::ListBytes, list_bytes)?;
        cx.meter.release(WorkResource::ListBytes, list_bytes);
    }
    Ok(())
}

/// Per link of `a`, the adjacency ranges its edge step walks, in order
/// (none for a `Same` link): computed once, not per walk.
pub(super) fn link_ranges(a: &PathAutomaton) -> Box<[Box<[Range]>]> {
    a.links
        .iter()
        .map(|link| match link {
            PathLink::Edge(step) => ranges(step.direction, step.types.as_deref()),
            PathLink::Same => Box::default(),
        })
        .collect()
}
