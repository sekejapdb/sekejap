//! `sekejap` -- the published crate, and the one name an application imports.
//!
//! This crate is the Rust distribution of the three layers (`docs/LAYERS.md`):
//! `core` (the atomics), `lang` (SQL with `GRAPH_TABLE`) and `dist` (the
//! service wrapper). It re-exports them under one root so a caller writes
//! `sekejap::core::Database`, `sekejap::lang::SqlDatabase` and
//! `sekejap::dist::service::ServiceDatabase` and takes one dependency.
//!
//! What it does NOT yet carry is the `CoreDB` surface the 0.16 series
//! published (`open`, `execute`, `query`, `put`, `get`, `remove`, `link`,
//! `show`, `collection_names`, ...): that facade is the next slice, mapped
//! function by function in `docs/dist/FFI_CONTRACT.md`, and it lands here
//! before 0.17.0 is published. Until then `publish = false`.
pub use sekejap_core as core;
pub use sekejap_dist as dist;
pub use sekejap_lang as lang;

/// The crate version, as one string for a caller that reports it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
