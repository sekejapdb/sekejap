//! §4 of `docs/dist/OPS_CONTRACT.md`: the public cancel.
//!
//! A cloneable `Arc<AtomicBool>` in the shape of `sqlite3_interrupt`. One
//! relaxed load per charge is the whole cost, which is what makes it
//! affordable on the per-candidate path every driver already walks.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

/// A handle a second thread holds while a first thread runs a statement.
///
/// The semantics the contract states and this keeps:
///
/// * Cancellation is **per handle, and therefore per service**: setting it
///   stops every statement in flight that was issued under it.
/// * It is **sticky**. A cancel stays set until [`InterruptHandle::clear`],
///   so a caller cannot accidentally answer a query it was told to stop.
///   A statement timeout (§3) is per statement and needs no clearing, which
///   is the other half of why the two are different errors.
/// * A cancelled statement returns an error. It never returns a partial
///   answer labelled complete -- the same rule `docs/lang/QL_CONTRACT.md` §6
///   makes for a budget refusal.
#[derive(Clone, Debug)]
pub struct InterruptHandle(Arc<AtomicBool>);

impl InterruptHandle {
    pub(super) fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// Ask every statement running under this handle to stop at its next
    /// check point. Sticky: [`InterruptHandle::clear`] is what un-asks it.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether a cancel is standing. This is the load the cancellation
    /// closure performs, and the only per-charge cost §4 adds.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// Clear a standing cancel so the handle answers normally again.
    /// Returns whether one was standing.
    pub fn clear(&self) -> bool {
        self.0.swap(false, Ordering::Relaxed)
    }
}

impl Default for InterruptHandle {
    fn default() -> Self {
        Self::new()
    }
}
