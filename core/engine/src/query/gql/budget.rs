//! The ceilings a GQL execution runs under (`docs/lang/GQL_PROFILE_DESIGN.md`
//! §3.4).
//!
//! [`QueryBudget`] is a public struct that callers build with struct
//! literals, so a new field would break every one of them. The new ceilings
//! therefore live in a wrapper, [`GqlBudget`], which carries the existing
//! budget unchanged as `base`; [`GqlMeter`] likewise wraps the existing
//! [`WorkMeter`] and charges the existing resources through it, so a
//! resource the engine already knew refuses exactly as it did.
//!
//! Two kinds of new resource:
//!
//! * WORK -- [`WorkResource::BindingRows`], [`WorkResource::PathStates`] --
//!   is a running count, bounded by the caller and unlimited by default.
//! * MEMORY -- [`WorkResource::QueueEntries`],
//!   [`WorkResource::PredecessorArcs`], [`WorkResource::SortBytes`],
//!   [`WorkResource::ListBytes`] -- is what is held AT ONCE: a charge adds to
//!   it, [`GqlMeter::release`] gives back what is no longer held, and the
//!   report is the high-water mark. Its ceiling is the `RUN_BYTES` memory
//!   promise (Law 1), not a caller allowance: a caller may ask for less,
//!   never for more, and [`GqlBudget::unlimited`] does not lift it -- the
//!   rule `WorkResource::MembershipBytes` and `QueryBudget::groups_cap`
//!   already follow.

use super::super::rows::RUN_BYTES;
use super::super::{QueryBudget, QueryError, QueryResult, QueryWork, WorkMeter, WorkResource};
use super::value::{EdgeRef, NodeRef, PathRef};
use std::mem::size_of;

/// Sort buffers, distinct sets and group keys: the whole promise.
const SORT_BYTES_CAP: u64 = RUN_BYTES as u64;

/// Materialised lists (group lists, `ARRAY_AGG`, `NODES(p)`): the whole
/// promise.
const LIST_BYTES_CAP: u64 = RUN_BYTES as u64;

/// One frontier, stack or heap entry of a path search: the node reached, the
/// automaton state and counter, and the partial path.
const QUEUE_ENTRY_BYTES: usize = size_of::<(NodeRef, u32, u32, PathRef)>();
const QUEUE_ENTRIES_CAP: u64 = (RUN_BYTES / QUEUE_ENTRY_BYTES) as u64;

/// One retained parent pointer of a witness path: the parent's index, the
/// edge crossed and the orientation it was crossed in.
const PREDECESSOR_ARC_BYTES: usize = size_of::<(usize, EdgeRef, bool)>();
const PREDECESSOR_ARCS_CAP: u64 = (RUN_BYTES / PREDECESSOR_ARC_BYTES) as u64;

/// Per-execution ceilings of a GQL answer: every existing resource and the
/// deadline in `base`, plus the six resources a pattern match and a path
/// search add.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlBudget {
    /// Every existing resource, and the deadline, exactly as a plain query.
    pub base: QueryBudget,
    /// WORK: binding rows produced by the operators.
    pub binding_rows: u64,
    /// WORK: partial paths or product states a path search creates.
    pub path_states: u64,
    /// MEMORY: frontier, DFS stack and Dijkstra heap entries held at once.
    pub queue_entries: u64,
    /// MEMORY: parent pointers retained for witness paths.
    pub predecessor_arcs: u64,
    /// MEMORY: bytes of sort buffers, distinct sets and group keys.
    pub sort_bytes: u64,
    /// MEMORY: bytes of materialised lists.
    pub list_bytes: u64,
}

impl GqlBudget {
    /// Work unlimited; memory at the fixed caps.
    pub const fn unlimited() -> Self {
        Self::from_query_budget(QueryBudget::unlimited())
    }

    /// `base` as given, the new work resources unlimited (the caller bounds
    /// work, as today), and the memory resources at their caps.
    pub const fn from_query_budget(base: QueryBudget) -> Self {
        Self {
            base,
            binding_rows: u64::MAX,
            path_states: u64::MAX,
            queue_entries: QUEUE_ENTRIES_CAP,
            predecessor_arcs: PREDECESSOR_ARCS_CAP,
            sort_bytes: SORT_BYTES_CAP,
            list_bytes: LIST_BYTES_CAP,
        }
    }
}

/// What a GQL execution spent: `base` as a plain page reports it, the work
/// resources as running totals, the memory resources as the most held at
/// once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GqlWork {
    pub base: QueryWork,
    pub binding_rows: u64,
    pub path_states: u64,
    pub queue_entries: u64,
    pub predecessor_arcs: u64,
    pub sort_bytes: u64,
    pub list_bytes: u64,
}

impl GqlWork {
    /// Add one page's charges to a running total over several pages: the
    /// work resources summed, the memory resources as the most any page
    /// held ([`QueryWork::add_page`] for `base`).
    pub fn add_page(&mut self, page: &GqlWork) {
        self.base.add_page(&page.base);
        self.binding_rows += page.binding_rows;
        self.path_states += page.path_states;
        self.queue_entries = self.queue_entries.max(page.queue_entries);
        self.predecessor_arcs = self.predecessor_arcs.max(page.predecessor_arcs);
        self.sort_bytes = self.sort_bytes.max(page.sort_bytes);
        self.list_bytes = self.list_bytes.max(page.list_bytes);
    }
}

/// The meter a GQL execution charges. The existing resources go to the
/// wrapped [`WorkMeter`], unchanged; the new ones are counted here. Every
/// charge, of either kind, first checks cancellation and the deadline.
pub struct GqlMeter<'a, C> {
    base: WorkMeter<'a, C>,
    limit: GqlBudget,
    /// Work: running totals. Memory: high-water marks.
    used: GqlWork,
    /// Memory held now, per memory resource.
    held_queue_entries: u64,
    held_predecessor_arcs: u64,
    held_sort_bytes: u64,
    held_list_bytes: u64,
}

impl<'a, C: FnMut() -> bool> GqlMeter<'a, C> {
    /// A meter under `limit`. `cancelled` is asked on every charge; when it
    /// says yes the charge is refused with [`QueryError::Cancelled`].
    pub fn new(limit: GqlBudget, cancelled: &'a mut C) -> Self {
        Self {
            base: WorkMeter::new(limit.base, cancelled),
            limit,
            used: GqlWork::default(),
            held_queue_entries: 0,
            held_predecessor_arcs: 0,
            held_sort_bytes: 0,
            held_list_bytes: 0,
        }
    }

    /// For a resource this meter counts itself: the counter a charge adds
    /// to, the high-water mark it raises (memory only), and the ceiling in
    /// force. `None` for a resource the wrapped meter counts.
    fn slot(&mut self, resource: WorkResource) -> Option<(&mut u64, Option<&mut u64>, u64)> {
        let limit = &self.limit;
        let used = &mut self.used;
        Some(match resource {
            WorkResource::BindingRows => (&mut used.binding_rows, None, limit.binding_rows),
            WorkResource::PathStates => (&mut used.path_states, None, limit.path_states),
            WorkResource::QueueEntries => (
                &mut self.held_queue_entries,
                Some(&mut used.queue_entries),
                limit.queue_entries.min(QUEUE_ENTRIES_CAP),
            ),
            WorkResource::PredecessorArcs => (
                &mut self.held_predecessor_arcs,
                Some(&mut used.predecessor_arcs),
                limit.predecessor_arcs.min(PREDECESSOR_ARCS_CAP),
            ),
            WorkResource::SortBytes => (
                &mut self.held_sort_bytes,
                Some(&mut used.sort_bytes),
                limit.sort_bytes.min(SORT_BYTES_CAP),
            ),
            WorkResource::ListBytes => (
                &mut self.held_list_bytes,
                Some(&mut used.list_bytes),
                limit.list_bytes.min(LIST_BYTES_CAP),
            ),
            _ => return None,
        })
    }

    /// Charge `amount` of `resource`, or refuse it by name with the ceiling
    /// and the total the charge would have reached. A refused charge counts
    /// nothing.
    pub fn charge(&mut self, resource: WorkResource, amount: u64) -> QueryResult<()> {
        if self.slot(resource).is_none() {
            return self.base.charge(resource, amount);
        }
        self.base.check_cancelled()?;
        let (count, peak, limit) = self
            .slot(resource)
            .expect("checked above: this meter counts the resource");
        let attempted = count.checked_add(amount).ok_or(QueryError::BudgetExceeded {
            resource,
            limit,
            attempted: u64::MAX,
        })?;
        if attempted > limit {
            return Err(QueryError::BudgetExceeded {
                resource,
                limit,
                attempted,
            });
        }
        *count = attempted;
        if let Some(peak) = peak {
            *peak = (*peak).max(attempted);
        }
        Ok(())
    }

    /// Give back `amount` of a MEMORY resource that is no longer held. Work
    /// cannot be given back; asking to is a bug, caught in debug builds.
    pub fn release(&mut self, resource: WorkResource, amount: u64) {
        match self.slot(resource) {
            Some((held, Some(_), _)) => *held = held.saturating_sub(amount),
            _ => debug_assert!(false, "{resource:?} is not a GQL memory resource"),
        }
    }

    /// The wrapped meter, for the engine's own readers that take one (a
    /// row projection charges its vector sidecars through it).
    pub(super) fn base(&mut self) -> &mut WorkMeter<'a, C> {
        &mut self.base
    }

    /// What this execution has spent so far.
    pub fn work(&self) -> GqlWork {
        GqlWork {
            base: self.base.used,
            ..self.used
        }
    }
}
