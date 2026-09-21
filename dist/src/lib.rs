//! The distribution layer: everything that puts the engine in front of an
//! operator or a foreign runtime. It depends on `sekejap-lang` and
//! `sekejap-core`; nothing in those two may depend on it.
//!
//! What is here today is `src/cli/`, the operator binaries declared as
//! `[[bin]]` targets of this crate, and [`service`], which is
//! `docs/dist/OPS_CONTRACT.md` §1-§5: one writer, snapshot readers, the
//! publish barrier, the statement timeout, the public cancel and the
//! commit-time change feed.
//!
//! What is declared but not built: [`pg`]. It is named here so the layer's
//! shape is visible without reading a plan; it carries no implementation.
//!
//! Foreign-language wrappers are listed in `bindings/README.md`; none is
//! built from this workspace.

pub mod pg;
pub mod service;
