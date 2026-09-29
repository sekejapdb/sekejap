//! Section 2 of the hyper contract (`CONTRACT.md`), made physical
//! (`docs/core/SUPPORTIVE.md`): everything that describes, finds or maintains
//! the core. One module per node of the tree:
//!
//! - `anchor` -- 2.0.2, the Anchor in the header copies.
//! - `carrier` -- 2.0, the byte formats of the Anchor and the Register.
//! - `register` -- 2.0.3, the Register on disk: three fixed trees.
//! - `verify` -- every copy of every entry, checked before it is trusted.
//!
//! The nodes 2.a-2.g arrive as their entry kinds are built (build step 3).

// Build step 2 (the carrier) lands before the open path uses it (build step
// 3), so nothing outside the tests calls it yet.
#![allow(dead_code)]

pub(crate) mod anchor;
pub(crate) mod carrier;
pub(crate) mod header;
pub(crate) mod register;
pub(crate) mod schema;
pub(crate) mod verify;

#[cfg(test)]
#[path = "anchor_tests.rs"]
mod anchor_tests;
#[cfg(test)]
#[path = "carrier_tests.rs"]
mod carrier_tests;
#[cfg(test)]
#[path = "header_tests.rs"]
mod header_tests;
#[cfg(test)]
#[path = "register_tests.rs"]
mod register_tests;
#[cfg(test)]
#[path = "schema_tests.rs"]
mod schema_tests;
#[cfg(test)]
#[path = "verify_tests.rs"]
mod verify_tests;
