//! The one interface between the engine and the language layer while a GQL
//! plan runs (`docs/lang/GQL_PROFILE_DESIGN.md` §1.3).
//!
//! The engine walks keyspaces, holds cross-row state and charges the budget;
//! the language layer compiles expressions and evaluates them. The engine
//! cannot name a language type -- the dependency runs the other way -- so an
//! expression reaches it as an opaque [`ExprId`], and it asks [`GqlHost`] to
//! evaluate one over a binding row. The host reads element properties only
//! through [`EvalCx::reader`], which charges [`EvalCx::meter`]: the host
//! itself never reads a keyspace, so nothing it evaluates is free.
//!
//! This trait is FROZEN for the M2 binder and planner, which implement it.
//! It is object-safe; the engine holds it as `&dyn GqlHost`. The cost of
//! that choice, named: one dynamic call per expression evaluation.

use super::super::{invalid_query, PreparedQuery, QueryError, QueryResult};
use super::budget::GqlMeter;
use super::reader::ElementReader;
use super::value::{BindingRow, BindingValue};
use crate::collections::Database;

/// An expression the language layer compiled: an index into a table the
/// host owns. The engine never looks inside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ExprId(pub u32);

/// An index-candidate seed the language layer compiled: an index into a
/// table the host owns, opened per input row by [`GqlHost::open_seed`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SeedId(pub u32);

/// A predicate's answer under SQL's three-valued logic. A filter keeps a row
/// only on `True`: `Unknown` (a comparison with `Null`) drops it, like
/// `False`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Truth {
    True,
    False,
    Unknown,
}

/// The meter one page of a GQL execution charges. The cancellation callback
/// is erased to a trait object so that [`EvalCx`], and therefore
/// [`GqlHost`], is one concrete type -- a generic method would make the
/// trait impossible to hold as `&dyn GqlHost`.
pub type ExecMeter<'m> = GqlMeter<'m, &'m mut dyn FnMut() -> bool>;

/// What an evaluation may use: the charged element reader, the execution's
/// parameter values, and the meter to charge.
pub struct EvalCx<'x, 'm> {
    /// Charged property, label and property-name reads. Pass
    /// [`EvalCx::meter`] to every call.
    pub reader: ElementReader<'x>,
    /// The bound `$n` values of this execution, converted and type-checked
    /// by the language layer when it opened the execution, in the host's own
    /// numbering: the engine only hands the slice through.
    pub params: &'x [BindingValue],
    /// The page's meter. Any materialised list is charged here as
    /// `WorkResource::ListBytes` by the operator that keeps it, not by the
    /// host.
    pub meter: &'x mut ExecMeter<'m>,
}

/// What the engine needs from the language layer while it executes a GQL
/// plan. The language layer implements it on its compiled statement.
///
/// Every method may fail. An error stops the page, and the cursor, with
/// that error: a type error the binder could not rule out, an arithmetic
/// error, or a refusal the reader or meter raised.
pub trait GqlHost {
    /// Evaluate `expr` over `row`: the value `LET` stores, `RETURN`
    /// projects, and a key seed looks up.
    fn eval(
        &self,
        expr: ExprId,
        row: &BindingRow,
        cx: &mut EvalCx<'_, '_>,
    ) -> QueryResult<BindingValue>;

    /// Evaluate predicate `expr` over `row` under three-valued logic:
    /// `FILTER`, and the per-hop edge and far-node predicates of `Expand`.
    fn test(&self, expr: ExprId, row: &BindingRow, cx: &mut EvalCx<'_, '_>) -> QueryResult<Truth>;

    /// Open index-candidate seed `seed` for input row `row`: a query the
    /// host prepares over ONE collection -- in M2 a scalar equality or range
    /// -- whose rows' ids are the seed nodes. `None` is an empty stream: for
    /// example a bound value that makes the predicate unsatisfiable, or a
    /// `NULL` compared for equality.
    ///
    /// The engine pages the query under what is left of the page's budget
    /// and charges what the walk spent; the host only prepares it. The query
    /// does not borrow the request it was prepared from, so the engine holds
    /// it across pulls.
    fn open_seed<'db>(
        &self,
        seed: SeedId,
        db: &'db Database,
        row: &BindingRow,
        cx: &mut EvalCx<'_, '_>,
    ) -> QueryResult<Option<PreparedQuery<'db>>>;

    /// The error that stops an integer `SUM` whose total leaves the 64-bit
    /// range: the engine folds the vertical aggregates itself, and the host
    /// names the error as its language does (for SQL, `22003`).
    fn out_of_range(&self) -> QueryError {
        invalid_query("SUM is out of range for a 64-bit integer")
    }
}
