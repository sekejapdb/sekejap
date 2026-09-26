//! The GQL query profile's engine half (`docs/lang/GQL_PROFILE_DESIGN.md`).
//!
//! `value` holds what a binding row is made of -- nodes, edges, paths,
//! scalars and lists -- with the identity equality, the grouping hash, the
//! internal total order and the byte estimate every memory charge uses.
//! `budget` holds the ceilings a GQL execution runs under: the existing
//! [`QueryBudget`](super::QueryBudget) wrapped unchanged, plus the work and
//! memory resources a pattern match and a path search add.
//! `reader` turns a bound node or edge into what it holds -- properties,
//! property names, labels, `ELEMENT_ID` -- charging every store read.
//! `host` is the one interface the language layer implements, so the engine
//! can evaluate its expressions without naming its types. `plan` is the
//! operator tree the language layer's planner builds; `ops` instantiates
//! and runs it, and `cursor` pages one execution of it. `paths` holds the
//! compiled path pattern (the automaton) and the searches that walk it.

mod budget;
mod cursor;
mod host;
mod ops;
mod paths;
mod plan;
mod reader;
mod value;

pub use budget::{GqlBudget, GqlMeter, GqlWork};
pub use cursor::{GqlCursor, GqlPage};
pub use host::{EvalCx, ExecMeter, ExprId, GqlHost, SeedId, Truth};
pub use paths::{EdgeStep, NodeTest, PathAutomaton, PathLink, PathMode, Repeat};
pub use plan::{
    AggSpec, CountExpr, ExistsMode, OpSpec, PathSearch, ReachSpec, SeedSource, SortKey, StepSpec,
    Target,
};
pub use reader::ElementReader;
pub use value::{BindingRow, BindingValue, EdgeRef, ListRef, NodeRef, PathRef, SlotId, ValueType};

