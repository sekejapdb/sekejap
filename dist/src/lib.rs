//! The distribution layer: everything that puts the engine in front of an
//! operator or a foreign runtime. It depends on `sekejap-lang` and
//! `sekejap-core`; nothing in those two may depend on it.
//!
//! What is here today is `src/cli/`: the operator binaries, declared as
//! `[[bin]]` targets of this crate.
//!
//! What is declared but not built: [`service`] and [`pg`]. Both are named
//! here so the layer's shape is visible without reading a plan; neither
//! carries an implementation.
//!
//! Foreign-language wrappers are listed in `bindings/README.md`; none is
//! built from this workspace.

pub mod pg;
pub mod service;
