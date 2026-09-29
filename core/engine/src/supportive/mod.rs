//! Section 2 of the hyper contract (`CONTRACT.md`), made physical
//! (`docs/core/SUPPORTIVE.md`): everything that describes, finds or maintains
//! the core. One module per node of the tree:
//!
//! - `carrier` -- 2.0, the Anchor and the Register every entry lives in.
//!
//! The nodes 2.a-2.g arrive as their entry kinds are built (build step 3).

pub(crate) mod carrier;

#[cfg(test)]
#[path = "carrier_tests.rs"]
mod carrier_tests;
