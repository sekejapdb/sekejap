//! Path search (`docs/lang/GQL_PROFILE_DESIGN.md` §4).
//!
//! `automaton` is the compiled path pattern: node positions joined by edge
//! steps, with quantified runs and one counter -- the target the language
//! layer's pattern compiler emits. `enumerate` is the search without a
//! selector: every match, depth first, with the path mode checked per path.
//! `bfs` is the selector search -- ANY and ANY SHORTEST breadth first, ANY
//! CHEAPEST by Dijkstra, whose frontier and cost validation are in
//! `dijkstra` -- one witness per end node, over product states.
//!
//! None of these searches keeps a visited set on NODES: the existing
//! node-deduplicating BFS loses paths, multiplicities and later depths, so
//! it may stand in only where the planner proves nobody observes them
//! (§4.6).

mod arrive;
mod automaton;
mod bfs;
mod dijkstra;
mod enumerate;

pub use automaton::{EdgeStep, NodeTest, PathAutomaton, PathLink, PathMode, Repeat};
pub(super) use bfs::Select;
pub(super) use enumerate::Enumerate;
