//! Shared reservation arithmetic. Currently the page arena uses this ledger;
//! it is not a whole-process RSS cap. Constrained stores additionally bound
//! WAL/record sizes and allocator bookkeeping counts through ResourceLimits.
//! Exhaustion is fallible, including arithmetic overflow.

use crate::{Error, Result};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class { Pool, Wal, Sort, Query }

pub struct MemoryBudget { total: usize, used: AtomicUsize }

impl MemoryBudget {
    pub fn new(total: usize) -> Self { MemoryBudget { total, used: AtomicUsize::new(0) } }
    pub fn used(&self) -> usize { self.used.load(Ordering::Relaxed) }
    pub fn total(&self) -> usize { self.total }

    pub fn reserve(self: &Arc<Self>, class: Class, bytes: usize) -> Result<Reservation> {
        let mut cur = self.used.load(Ordering::Relaxed);
        loop {
            let next = cur.checked_add(bytes).filter(|n| *n <= self.total)
                .ok_or(Error::OutOfBudget)?;
            match self.used.compare_exchange_weak(
                cur, next, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => return Ok(Reservation { budget: self.clone(), bytes, class }),
                Err(actual) => cur = actual,
            }
        }
    }
}

/// `class` is not read today -- `Drop` only needs `bytes` to release the
/// reservation -- but every `reserve` call already names one, so it is
/// carried here rather than discarded, for a future per-class usage
/// breakdown or a same-class-releases-what-it-reserved assertion.
pub struct Reservation { budget: Arc<MemoryBudget>, bytes: usize, #[allow(dead_code)] class: Class }

impl Drop for Reservation {
    fn drop(&mut self) { self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn reservations_are_returned_when_dropped() {
        let b = Arc::new(MemoryBudget::new(1000));
        { let _r = b.reserve(Class::Sort, 800).unwrap(); assert_eq!(b.used(), 800); }
        assert_eq!(b.used(), 0);
    }

    #[test]
    fn exhaustion_is_an_error_never_a_panic_and_never_a_kill() {
        let b = Arc::new(MemoryBudget::new(1000));
        let _a = b.reserve(Class::Sort, 900).unwrap();
        assert!(matches!(b.reserve(Class::Query, 200), Err(crate::Error::OutOfBudget)));
    }
}
