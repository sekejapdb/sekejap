//! The index families. One directory per family; each family owns its own
//! on-disk entry encoding, its build path and its candidate walk.
//!
//! * `text` -- BM25 postings over analysed terms
//!   (docs/lang/QL_CONTRACT.md, "Full text").
//! * `vector` -- exact and quantized nearest-neighbour (docs/lang/QL_CONTRACT.md,
//!   "Exact vector" and "Approximate vector").
//! * `spatial` -- point and geometry indexes plus the geodesic and Hilbert
//!   maths they share (docs/core/SPATIAL_FUNCTIONS.md).
//! * `graph` -- typed edges and the bounded traversals over them
//!   (docs/core/GRAPH_CONTRACT.md).
//!
//! The scalar index family has no directory of its own: its keys are the
//! `store::scalar_key` codec and its walk is `query::drivers`.
pub mod graph;
pub mod spatial;
pub mod text;
pub mod vector;
