//! The compiled path pattern the engine searches
//! (`docs/lang/GQL_PROFILE_DESIGN.md` §4.1, §4.5).
//!
//! The engine cannot depend on the language layer, so the automaton is an
//! ENGINE type: the language layer's pattern compiler (task M4-A) emits a
//! [`PathAutomaton`], and every path search (enumeration, and the ANY,
//! ANY SHORTEST and ANY CHEAPEST selectors) walks the same one.
//!
//! # Shape
//!
//! A path pattern is a line of NODE POSITIONS -- the automaton's states --
//! joined by LINKS, with some runs of it quantified:
//!
//! ```text
//! (s) -[:r]-> (a) ((b) -[:r]-> (c) -[:s]-> (d)){1,3} (e) -[]-> (t)
//!
//! states   0:s     1:a   2:b     3:c     4:d    5:e     6:t
//! links       Edge    Same   Edge    Edge    Same   Edge
//! repeats               [first 2 ..= last 4, 1..=3]
//! ```
//!
//! * A state is a [`NodeTest`]: label alternatives, an inline predicate and
//!   the slot the node binds. The test applies to the node the path stands
//!   on when it reaches that state.
//! * [`PathLink::Edge`] crosses one edge ([`EdgeStep`]: context, edge-type
//!   alternatives, direction, inline edge predicate, edge slot -- the
//!   semantics of one [`StepSpec`](super::super::StepSpec) hop; the far
//!   node's labels and predicate are the NEXT state's test, written once at
//!   that position). [`PathLink::Same`] crosses nothing: the two states it
//!   joins apply to ONE node. That is how adjacent node patterns
//!   concatenate: `(a)` and `(b)` above test the same node.
//! * A [`Repeat`] makes states `first..=last` a loop body run between `min`
//!   and `max` times (`max: None` is unbounded). The link into the body and
//!   the link out of it are `Same`: iteration `i`'s last node and iteration
//!   `i + 1`'s first node are one node (the body repeats as a whole, so a
//!   multi-type subpath `r, s` repeats as `r s r s`, never `r r`). ZERO
//!   iterations jump from the state before the body to the state after it
//!   on the same node, so both of those tests apply to it and every slot
//!   bound inside the body binds an empty list.
//! * The start is state 0, at the node the search is given. The only
//!   accepting state is the last one.
//!
//! A quantified edge `-[e]->{1,8}` is a body of two anonymous states and
//! one edge: labels written on the endpoints are the states outside the
//! body, so the intermediate nodes carry no test.
//!
//! # One counter (Q10)
//!
//! Repeats are disjoint, in order, and do not touch: each is preceded and
//! followed by a state of its own. So at most one is active at a time and
//! the search state is `(node, state, k)` with ONE counter `k`, the
//! iterations the active repeat has completed. A quantifier inside a
//! quantified group has no representation: overlapping or nested ranges
//! are refused when the execution opens.
//!
//! # Bindings
//!
//! A slot bound at a position INSIDE a repeat (a body state, or an edge
//! link of the body) is a GROUP variable: during the search it holds the
//! current iteration's element, so an inline predicate sees a singleton;
//! in the emitted row it holds the list of every iteration's element, in
//! path order, typed by [`PathAutomaton::groups`]. A slot bound OUTSIDE
//! every repeat is a singleton; binding it at a second position (or a slot
//! the input row already holds) requires the same element there -- a
//! repeated variable is an identity constraint, never a rebinding.
//!
//! # Modes
//!
//! [`PathMode`] is checked per PATH, when a step extends it: `Trail`
//! refuses an edge the path already crossed, `Acyclic` a node it already
//! visited. Without a selector an unbounded repeat is admitted only under
//! `Trail` or `Acyclic`, which terminate on a finite graph; under `Walk` it
//! is refused when the execution opens (§4.2 admission). A selector admits
//! it under every mode: its search does not enumerate walks. Budgets bound every admitted
//! form: exhausting one fails the page, it never truncates the answer.

use super::super::super::{invalid_query, QueryResult};
use super::super::host::ExprId;
use super::super::ops::{distinct, inside};
use super::super::value::{SlotId, ValueType};
use crate::collections::CollectionId;
use crate::index::graph::{Direction, EdgeTypeId, GraphContextId};

/// A compiled path pattern: the target the language layer's automaton
/// compiler (M4-A) emits.
#[derive(Clone, Debug)]
pub struct PathAutomaton {
    /// The node positions, in pattern order: state 0 is where the search
    /// starts, the last state is the one that accepts.
    pub states: Box<[NodeTest]>,
    /// `links[i]` leads from `states[i]` to `states[i + 1]`, so there is one
    /// link fewer than there are states.
    pub links: Box<[PathLink]>,
    /// The quantified runs, in pattern order.
    pub repeats: Box<[Repeat]>,
    pub mode: PathMode,
    /// The named path's slot (`p = ...`): the whole path the match crossed.
    pub path: Option<SlotId>,
    /// Every GROUP slot -- a slot bound inside a repeat -- with the type of
    /// its list's elements as the binder proved it. Exactly the slots bound
    /// inside repeats; checked when the execution opens.
    pub groups: Box<[(SlotId, ValueType)]>,
}

/// The test at one node position. Everything it names is optional.
#[derive(Clone, Debug, Default)]
pub struct NodeTest {
    /// The node's label alternatives: a test on its identity that reads
    /// nothing. `None` is any collection.
    pub labels: Option<Box<[CollectionId]>>,
    /// Inline node predicate, evaluated with the node in `bind` (so it needs
    /// one) after every edge predicate of the step that reached it. Reading
    /// the node's row costs one primary read.
    pub filter: Option<ExprId>,
    /// The slot the node binds.
    pub bind: Option<SlotId>,
}

/// One edge crossing: the hop semantics of a
/// [`StepSpec`](super::super::StepSpec).
#[derive(Clone, Debug)]
pub struct EdgeStep {
    pub context: GraphContextId,
    /// The edge-type alternatives, walked in this order; `None` is every
    /// type, `Some` of an empty list matches nothing. A type named twice is
    /// refused.
    pub types: Option<Box<[EdgeTypeId]>>,
    /// `Outgoing`, `Incoming` or `Both`. `Both` crosses a self-loop once,
    /// forward.
    pub direction: Direction,
    /// Inline edge predicate, evaluated with the edge in `bind` (so it needs
    /// one) and the far node in its state's slot. Reading an incoming edge's
    /// property costs one primary-posting read.
    pub filter: Option<ExprId>,
    /// The slot the edge binds.
    pub bind: Option<SlotId>,
}

/// What joins two adjacent node positions.
#[derive(Clone, Debug)]
pub enum PathLink {
    /// One node: both positions' tests apply to it (concatenation).
    Same,
    /// One edge from the first position's node to the second's.
    Edge(EdgeStep),
}

/// A quantified run: states `first..=last` repeated `min..=max` times.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Repeat {
    pub first: u16,
    pub last: u16,
    pub min: u32,
    /// `None`: unbounded, admitted only under `Trail` or `Acyclic`.
    pub max: Option<u32>,
}

/// How a path may revisit what it crossed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathMode {
    /// Anything may repeat.
    Walk,
    /// No edge twice.
    Trail,
    /// No node twice.
    Acyclic,
}

/// Where a position binds: its slot, and whether that slot is a group.
#[derive(Clone, Copy, Debug)]
pub(super) struct Binding {
    pub(super) slot: usize,
    pub(super) group: bool,
}

impl PathAutomaton {
    /// Is state `state` inside a repeat's body?
    pub(super) fn state_in_repeat(&self, state: usize) -> bool {
        self.repeats
            .iter()
            .any(|r| usize::from(r.first) <= state && state <= usize::from(r.last))
    }

    /// Is link `link` inside a repeat's body?
    pub(super) fn link_in_repeat(&self, link: usize) -> bool {
        self.repeats
            .iter()
            .any(|r| usize::from(r.first) <= link && link < usize::from(r.last))
    }

    /// The repeat whose body starts right after state `state`.
    pub(super) fn repeat_after(&self, state: usize) -> Option<&Repeat> {
        self.repeats
            .iter()
            .find(|r| usize::from(r.first) == state + 1)
    }

    /// The repeat whose body ends at state `state`.
    pub(super) fn repeat_ending(&self, state: usize) -> Option<&Repeat> {
        self.repeats.iter().find(|r| usize::from(r.last) == state)
    }

    /// The node binding of state `state`.
    pub(super) fn node_binding(&self, state: usize) -> Option<Binding> {
        self.states[state].bind.map(|slot| Binding {
            slot: usize::from(slot.0),
            group: self.state_in_repeat(state),
        })
    }

    /// The edge binding of link `link`, which must be an edge.
    pub(super) fn edge_binding(&self, link: usize, step: &EdgeStep) -> Option<Binding> {
        step.bind.map(|slot| Binding {
            slot: usize::from(slot.0),
            group: self.link_in_repeat(link),
        })
    }

    /// Two or more repeats whose iteration count may vary: only then can two
    /// runs of the automaton cross the SAME path (each iteration crosses a
    /// fixed, positive number of edges, so with one variable count the path
    /// length fixes it). Only then must identical matches be deduplicated.
    pub(super) fn ambiguous(&self) -> bool {
        self.repeats.iter().filter(|r| r.max != Some(r.min)).count() > 1
    }

    /// Refuse, when the execution opens, an automaton the search cannot run:
    /// slots outside a row of `width`, a malformed line, repeats that
    /// overlap, touch or cross no edge, an unbounded `Walk` without a
    /// `selector` (a selector's search keeps a visited set on product
    /// states, so it ends on a finite graph), a predicate
    /// with nowhere to bind its element, a group slot that is not declared
    /// (or declared and not bound inside a repeat), or a group slot bound
    /// twice.
    pub(in super::super) fn check(&self, width: u16, selector: bool) -> QueryResult<()> {
        let refuse = |why: &str| Err(invalid_query(format!("a path pattern {why}")));
        let states = self.states.len();
        if states == 0 || self.links.len() + 1 != states {
            return refuse("needs one link fewer than it has node positions");
        }
        let mut previous_last: Option<usize> = None;
        for r in self.repeats.iter() {
            let (first, last) = (usize::from(r.first), usize::from(r.last));
            if first == 0 || first >= last || last + 1 >= states {
                return refuse("repeats a run that has no position before and after it");
            }
            if previous_last.is_some_and(|p| first < p + 2) {
                return refuse("nests or joins quantifiers, which v1 refuses (Q10)");
            }
            previous_last = Some(last);
            if !matches!(self.links[first - 1], PathLink::Same)
                || !matches!(self.links[last], PathLink::Same)
            {
                return refuse("enters or leaves a repeat across an edge");
            }
            if !self.links[first..last]
                .iter()
                .any(|link| matches!(link, PathLink::Edge(_)))
            {
                return refuse("repeats a group that crosses no edge");
            }
            if r.max.is_some_and(|max| max < r.min) {
                return refuse("has a quantifier whose upper bound is below its lower bound");
            }
            if r.max.is_none() && self.mode == PathMode::Walk && !selector {
                return refuse(
                    "has an unbounded quantifier under WALK: use TRAIL, ACYCLIC or a selector",
                );
            }
        }

        // Every bound slot fits the row; each group slot is declared, bound
        // once, and never also bound outside a repeat or as the path.
        let mut singles = Vec::new();
        let mut grouped = Vec::new();
        let mut bind = |slot: SlotId, group: bool| -> QueryResult<()> {
            inside(width, slot)?;
            if group {
                grouped.push(slot)
            } else {
                singles.push(slot)
            }
            Ok(())
        };
        for (i, test) in self.states.iter().enumerate() {
            if let Some(labels) = &test.labels {
                distinct(labels)?;
            }
            match test.bind {
                Some(slot) => bind(slot, self.state_in_repeat(i))?,
                None if test.filter.is_some() => {
                    return refuse("has a node predicate with no slot for its node");
                }
                None => {}
            }
        }
        for (i, link) in self.links.iter().enumerate() {
            let PathLink::Edge(step) = link else { continue };
            if let Some(types) = &step.types {
                distinct(types)?;
            }
            match step.bind {
                Some(slot) => bind(slot, self.link_in_repeat(i))?,
                None if step.filter.is_some() => {
                    return refuse("has an edge predicate with no slot for its edge");
                }
                None => {}
            }
        }
        if let Some(path) = self.path {
            bind(path, false)?;
            if singles.iter().filter(|s| **s == path).count() > 1 || grouped.contains(&path) {
                return refuse("binds its path slot to an element too");
            }
        }
        let mut declared: Vec<SlotId> = self.groups.iter().map(|(slot, _)| *slot).collect();
        declared.sort_unstable();
        let mut bound = grouped.clone();
        bound.sort_unstable();
        if bound.windows(2).any(|pair| pair[0] == pair[1]) {
            return refuse("binds one group variable at two positions");
        }
        if declared != bound {
            return refuse("declares group slots other than the ones bound inside repeats");
        }
        if grouped.iter().any(|slot| singles.contains(slot)) {
            return refuse("binds one slot inside and outside a repeat");
        }
        Ok(())
    }
}
