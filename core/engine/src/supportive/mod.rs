//! Section 2 of the hyper contract (`CONTRACT.md`), made physical
//! (`docs/core/SUPPORTIVE.md`): everything that describes, finds or maintains
//! the core. One module per node of the tree:
//!
//! - `carrier` -- 2.0, the byte formats of the Anchor and the Register.
//! - `register` -- 2.0.3, the Register on disk: three fixed trees.
//!
//! The nodes 2.a-2.g arrive as their entry kinds are built (build step 3).

pub(crate) mod carrier;
pub(crate) mod register;

#[cfg(test)]
#[path = "carrier_tests.rs"]
mod carrier_tests;
#[cfg(test)]
#[path = "register_tests.rs"]
mod register_tests;
