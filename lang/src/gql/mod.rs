//! The GQL query profile's language half (`docs/lang/GQL_PROFILE_DESIGN.md`).
//!
//! `schema` holds the compile-time facts about a working table: which slot a
//! variable lives in, what type the binder proved for it, and where it was
//! bound. The engine never sees these; it knows a row only by its width.
//!
//! `convert` moves values across the profile's edge. Inside the profile the
//! engine's `BindingValue` is the only value; a bound `$n` is converted
//! once on the way in, and a final output column once on the way out. A
//! stored property never passes through here: the engine's `ElementReader`
//! reads it straight into a `BindingValue`, and it is there that a property
//! absent from its row reads as `Null`, while the SQL surface outside the
//! profile keeps `Missing` (`docs/lang/QL_CONTRACT.md` §5).
//!
//! `ast` is the parsed GQL body and `parse` the parser that builds it on the
//! SQL parser's cursor (M2-A).
//!
//! `bind` resolves a parsed stage against the catalog and its own scope:
//! slots, labels, repeated variables. `expr` is the expression IR it lowers
//! to, and `eval` evaluates that IR -- the language layer's side of the
//! engine's `GqlHost` -- with `scalar` holding the value operations of the
//! M3-C pack (arithmetic, casts, functions). `plan` chooses seeds, places
//! predicates and builds the engine's operator tree (M2-C), which
//! `plan/explain.rs` prints for `EXPLAIN`; `types` decides every value's
//! type. `automaton` lays every pattern out as a line and compiles a path
//! pattern -- quantifiers, subpaths, path modes, selectors, a path
//! variable -- into the engine's path automaton (M4-A). `stage` binds
//! and plans the stage grammar around the patterns -- `LET`, `FILTER`,
//! `FOR`, `RETURN` with grouping, `DISTINCT`, `ORDER BY`, `OFFSET`,
//! `LIMIT` -- and `NEXT` between stages (M3-B), plans an `OPTIONAL
//! MATCH` as one `OptionalApply` over its pattern's operators (M3-F), and
//! plans the outer SELECT over the relation as one more stage (M3-D).
//! `elements` holds the path, element and list functions and `horizontal`
//! the classification and the fold of horizontal aggregates, over a list
//! per row (M4-D). `subquery` plans an `EXISTS { ... }` as the inner side
//! of one `ExistsApply`: its scope, its placement, its filter and mark
//! forms (M5-C). `union` binds and plans `UNION [ALL | DISTINCT]` over the
//! stages of one part of the body (M5-E).

pub(crate) mod ast;
mod automaton;
mod bind;
pub(crate) mod convert;
mod elements;
mod eval;
mod expr;
mod horizontal;
mod host;
mod lineage;
mod parse;
mod registry;
pub(crate) mod plan;
mod scalar;
pub(crate) mod scope;
pub(crate) mod schema;
mod stage;
mod subquery;
mod types;
mod union;
