//! The evaluator: the language layer's half of `GqlHost`
//! (`docs/lang/GQL_PROFILE_DESIGN.md` §1.1, §1.3, §2.5).
//!
//! [`Program`] holds a compiled statement's expressions and index seeds,
//! and the engine calls back into it by [`ExprId`] and [`SeedId`] while it
//! runs the plan. Evaluation is PURE: a property is read only through the
//! charged `ElementReader` the engine hands in, and an index seed is only
//! PREPARED here -- the engine pages it and charges its walk.
//!
//! Null rules (§2.5): a property the row does not hold and a stored null
//! both read as `Null`. A comparison with `Null` is unknown, `NOT unknown`
//! is unknown, `AND` is false when either side is false and `OR` true when
//! either side is true, and a filter keeps a row only on true: SQL's
//! three-valued logic. Two values of kinds that do not compare -- text and
//! a number, say -- are an error, not a silent false.
//!
//! The M3-C pack follows PostgreSQL. An operator, a cast or a function
//! (but `concat`) is NULL when an operand is; `IS [NOT] NULL` is never
//! unknown; `x IN (list)` is true on a match, unknown when no member
//! matches and one comparison was unknown, false otherwise; `COALESCE`
//! returns its first non-NULL argument; `NULLIF(a, b)` is NULL when `a = b`
//! is true; `CASE` takes the first branch whose condition is TRUE (unknown
//! is not), and is NULL when none is and no `ELSE` is written. `COALESCE`,
//! `CASE`, `IN`, `AND` and `OR` evaluate no more than decides the answer,
//! so a branch not taken is never read and never raises. The value
//! operations themselves are `scalar.rs`.

use super::ast::Func;
use super::convert::Element;
use super::elements;
use super::expr::Ex;
use super::scalar;
use super::types;
use crate::ast::CmpOp;
use crate::sqlstate::DATATYPE_MISMATCH;
use crate::SqlError;
use sekejap_core::collections::gql::{
    BindingRow, BindingValue, EvalCx, ExprId, GqlHost, SeedId, SlotId, Truth,
};
use sekejap_core::collections::{
    CandidateDriver, CollectionId, Database, Error, IndexId, PreparedQuery, Projection, QueryError,
    QueryFilter, QueryOrder, QueryRequest, QueryResult, ScalarFilter, ScalarValue,
};
use sekejap_core::Kind;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::fmt;
use std::ops::Bound;

/// Why an evaluation stopped.
#[derive(Debug)]
pub(crate) enum Fault {
    /// The engine's own refusal -- a read that failed, a budget, a cancel --
    /// handed back to it exactly as it came.
    Engine(QueryError),
    /// PostgreSQL's error for the value, with its SQLSTATE
    /// ([`SqlError::Coded`]).
    Sql(SqlError),
}

impl From<QueryError> for Fault {
    fn from(error: QueryError) -> Self {
        Self::Engine(error)
    }
}

pub(crate) type Evaluated<T> = Result<T, Fault>;

/// A PostgreSQL error: its SQLSTATE and its message.
pub(super) fn raise(sqlstate: &'static str, message: impl fmt::Display) -> Fault {
    Fault::Sql(SqlError::coded(sqlstate, message))
}

/// A value of a kind the operation does not take: `42804`.
pub(super) fn mismatch(message: impl fmt::Display) -> Fault {
    raise(DATATYPE_MISMATCH, message)
}

/// An index-candidate seed: `field op value` over one collection's READY
/// scalar index, `value` evaluated per input row.
#[derive(Clone, Debug)]
pub(crate) struct IndexSeed {
    pub(crate) collection: CollectionId,
    pub(crate) index: IndexId,
    /// The index's key kind, which the value must be read into.
    pub(crate) kind: Kind,
    pub(crate) field: String,
    /// The comparison with the field on the LEFT; never `<>`.
    pub(crate) op: CmpOp,
    pub(crate) value: Ex,
}

/// A row as an expression reads it: the engine's row and, inside a fold's
/// argument, the one element that stands in for the list slot folded, so
/// no row is copied per element (`horizontal.rs`).
#[derive(Clone, Copy)]
pub(crate) struct View<'r> {
    row: &'r BindingRow,
    item: Option<(SlotId, &'r BindingValue)>,
}

impl<'r> View<'r> {
    fn of(row: &'r BindingRow) -> Self {
        Self { row, item: None }
    }

    /// Slot `slot`'s value.
    pub(super) fn get(&self, slot: SlotId) -> &'r BindingValue {
        match self.item {
            Some((at, item)) if at == slot => item,
            _ => self.row.get(slot),
        }
    }

    /// This row with `item` in slot `slot`.
    pub(super) fn with(self, slot: SlotId, item: &'r BindingValue) -> Self {
        Self {
            row: self.row,
            item: Some((slot, item)),
        }
    }
}

/// A compiled statement's expressions and seeds: what the engine's
/// `ExprId`s and `SeedId`s index.
#[derive(Clone, Debug, Default)]
pub(crate) struct Program {
    exprs: Vec<Ex>,
    seeds: Vec<IndexSeed>,
}

impl Program {
    pub(crate) fn expr(&mut self, ex: Ex) -> ExprId {
        self.exprs.push(ex);
        ExprId(self.exprs.len() as u32 - 1)
    }

    /// Expression `id`.
    pub(crate) fn get(&self, id: ExprId) -> &Ex {
        &self.exprs[id.0 as usize]
    }

    pub(crate) fn seed(&mut self, seed: IndexSeed) -> SeedId {
        self.seeds.push(seed);
        SeedId(self.seeds.len() as u32 - 1)
    }

    /// The value seed `id` compares its field with, evaluated per input row.
    pub(crate) fn seed_value(&self, id: SeedId) -> &Ex {
        &self.seeds[id.0 as usize].value
    }

    fn value(&self, ex: &Ex, row: View<'_>, cx: &mut EvalCx<'_, '_>) -> Evaluated<BindingValue> {
        Ok(match ex {
            Ex::Const(value) => value.clone(),
            Ex::Param(at) => cx.params[*at].clone(),
            Ex::Slot(slot) | Ex::Item(slot) => row.get(*slot).clone(),
            Ex::NodeProperty(slot, name) => match row.get(*slot) {
                BindingValue::Node(node) => cx.reader.node_property(*node, name, cx.meter)?,
                _ => BindingValue::Null,
            },
            Ex::EdgeProperty(slot, name) => match row.get(*slot) {
                BindingValue::Edge(edge) => cx.reader.edge_property(edge, name, cx.meter)?,
                _ => BindingValue::Null,
            },
            Ex::Compare(op, left, right) => {
                let left = self.value(left, row, cx)?;
                let right = self.value(right, row, cx)?;
                compare(*op, &left, &right)?
            }
            Ex::Not(inner) => match self.truth(inner, row, cx)? {
                Truth::True => BindingValue::Bool(false),
                Truth::False => BindingValue::Bool(true),
                Truth::Unknown => BindingValue::Null,
            },
            // The right side is not evaluated once the left decides: it
            // may be a charged read.
            Ex::And(left, right) => match self.truth(left, row, cx)? {
                Truth::False => BindingValue::Bool(false),
                left => match (left, self.truth(right, row, cx)?) {
                    (_, Truth::False) => BindingValue::Bool(false),
                    (Truth::True, Truth::True) => BindingValue::Bool(true),
                    _ => BindingValue::Null,
                },
            },
            Ex::Or(left, right) => match self.truth(left, row, cx)? {
                Truth::True => BindingValue::Bool(true),
                left => match (left, self.truth(right, row, cx)?) {
                    (_, Truth::True) => BindingValue::Bool(true),
                    (Truth::False, Truth::False) => BindingValue::Bool(false),
                    _ => BindingValue::Null,
                },
            },
            Ex::Neg(inner) => match self.value(inner, row, cx)? {
                BindingValue::Null => BindingValue::Null,
                value => scalar::negate(&value)?,
            },
            Ex::Arith(op, left, right) => {
                let left = self.value(left, row, cx)?;
                let right = self.value(right, row, cx)?;
                if is_null(&left) || is_null(&right) {
                    return Ok(BindingValue::Null);
                }
                scalar::arith(*op, &left, &right)?
            }
            Ex::Concat(left, right) => {
                let left = self.value(left, row, cx)?;
                let right = self.value(right, row, cx)?;
                if is_null(&left) || is_null(&right) {
                    return Ok(BindingValue::Null);
                }
                scalar::concat(&left, &right)?
            }
            Ex::IsNull(inner) => BindingValue::Bool(is_null(&self.value(inner, row, cx)?)),
            Ex::In(value, list) => {
                let value = self.value(value, row, cx)?;
                if is_null(&value) {
                    return Ok(BindingValue::Null);
                }
                let mut unknown = false;
                for member in list {
                    match compare(CmpOp::Eq, &value, &self.value(member, row, cx)?)? {
                        BindingValue::Bool(true) => return Ok(BindingValue::Bool(true)),
                        BindingValue::Null => unknown = true,
                        _ => {}
                    }
                }
                if unknown {
                    BindingValue::Null
                } else {
                    BindingValue::Bool(false)
                }
            }
            Ex::Case {
                operand,
                branches,
                otherwise,
            } => {
                let subject = match operand {
                    Some(operand) => Some(self.value(operand, row, cx)?),
                    None => None,
                };
                for (when, then) in branches {
                    let taken = match &subject {
                        Some(subject) => matches!(
                            compare(CmpOp::Eq, subject, &self.value(when, row, cx)?)?,
                            BindingValue::Bool(true)
                        ),
                        None => matches!(self.truth(when, row, cx)?, Truth::True),
                    };
                    if taken {
                        return self.value(then, row, cx);
                    }
                }
                match otherwise {
                    Some(otherwise) => self.value(otherwise, row, cx)?,
                    None => BindingValue::Null,
                }
            }
            Ex::Cast(inner, to) => match self.value(inner, row, cx)? {
                BindingValue::Null => BindingValue::Null,
                value => scalar::cast(&value, *to)?,
            },
            Ex::Coalesce(args) => {
                for arg in args {
                    let value = self.value(arg, row, cx)?;
                    if !is_null(&value) {
                        return Ok(value);
                    }
                }
                BindingValue::Null
            }
            Ex::Nullif(left, right) => {
                let left = self.value(left, row, cx)?;
                let right = self.value(right, row, cx)?;
                match compare(CmpOp::Eq, &left, &right)? {
                    BindingValue::Bool(true) => BindingValue::Null,
                    _ => left,
                }
            }
            Ex::Call(func, args) => {
                // Up to three arguments -- every function but `concat` --
                // are held on the stack.
                let mut few = [BindingValue::Null, BindingValue::Null, BindingValue::Null];
                let mut many = Vec::new();
                let values: &mut [BindingValue] = if args.len() <= few.len() {
                    &mut few[..args.len()]
                } else {
                    many.resize(args.len(), BindingValue::Null);
                    &mut many
                };
                for (value, arg) in values.iter_mut().zip(args) {
                    *value = self.value(arg, row, cx)?;
                }
                if *func != Func::Concat && values.iter().any(is_null) {
                    return Ok(BindingValue::Null);
                }
                scalar::call(*func, values)?
            }
            Ex::Graph(func, arg) => match self.value(arg, row, cx)? {
                BindingValue::Null => BindingValue::Null,
                value => elements::call(*func, &value, cx)?,
            },
            Ex::List(items) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.value(item, row, cx)?);
                }
                list(values)?
            }
            Ex::Fold(fold) => fold.eval(row, &mut |ex, row| self.value(ex, row, cx))?,
        })
    }

    fn truth(&self, ex: &Ex, row: View<'_>, cx: &mut EvalCx<'_, '_>) -> Evaluated<Truth> {
        match self.value(ex, row, cx)? {
            BindingValue::Bool(true) => Ok(Truth::True),
            BindingValue::Bool(false) => Ok(Truth::False),
            BindingValue::Null => Ok(Truth::Unknown),
            other => Err(mismatch(format!(
                "a condition is true, false or null, not {}",
                kind(&other)
            ))),
        }
    }
}

/// One execution's host: the program the engine calls back into, and the
/// PostgreSQL error an evaluation raised, kept here as data while the
/// engine carries the refusal back up as its own error.
pub(crate) struct Host<'p> {
    program: &'p Program,
    raised: RefCell<Option<SqlError>>,
}

impl<'p> Host<'p> {
    pub(crate) fn new(program: &'p Program) -> Self {
        Self {
            program,
            raised: RefCell::new(None),
        }
    }

    /// `evaluated` as the engine takes it: an engine refusal as it came, a
    /// PostgreSQL error kept here and handed to the engine as a refusal.
    fn hand<T>(&self, evaluated: Evaluated<T>) -> QueryResult<T> {
        evaluated.map_err(|fault| self.keep(fault))
    }

    /// `fault` as the engine takes it, a PostgreSQL error kept here.
    fn keep(&self, fault: Fault) -> QueryError {
        match fault {
            Fault::Engine(error) => error,
            Fault::Sql(error) => {
                let refusal = QueryError::Database(Error::InvalidInput(error.to_string()));
                *self.raised.borrow_mut() = Some(error);
                refusal
            }
        }
    }

    /// The error an execution under this host stopped with: the PostgreSQL
    /// error an evaluation raised, with its SQLSTATE, or else the engine's.
    pub(crate) fn error(&self, error: QueryError) -> SqlError {
        self.raised.borrow_mut().take().unwrap_or_else(|| error.into())
    }
}

impl GqlHost for Host<'_> {
    fn eval(&self, expr: ExprId, row: &BindingRow, cx: &mut EvalCx<'_, '_>) -> QueryResult<BindingValue> {
        self.hand(self.program.value(&self.program.exprs[expr.0 as usize], View::of(row), cx))
    }

    fn test(&self, expr: ExprId, row: &BindingRow, cx: &mut EvalCx<'_, '_>) -> QueryResult<Truth> {
        self.hand(self.program.truth(&self.program.exprs[expr.0 as usize], View::of(row), cx))
    }

    fn open_seed<'db>(
        &self,
        seed: SeedId,
        db: &'db Database,
        row: &BindingRow,
        cx: &mut EvalCx<'_, '_>,
    ) -> QueryResult<Option<PreparedQuery<'db>>> {
        self.hand(self.program.open_seed(seed, db, row, cx))
    }

    /// The vertical `SUM`'s overflow is `22003`, as the horizontal one's.
    fn out_of_range(&self) -> QueryError {
        self.keep(scalar::bigint_out_of_range())
    }
}

impl Program {
    fn open_seed<'db>(
        &self,
        seed: SeedId,
        db: &'db Database,
        row: &BindingRow,
        cx: &mut EvalCx<'_, '_>,
    ) -> Evaluated<Option<PreparedQuery<'db>>> {
        let seed = &self.seeds[seed.0 as usize];
        let value = self.value(&seed.value, View::of(row), cx)?;
        let Some(key) = index_key(seed, &value)? else {
            return Ok(None);
        };
        let predicate = match (seed.op, key.scalar()) {
            (CmpOp::Eq, value) => ScalarFilter::Eq(value),
            (CmpOp::Lt, value) => range(Bound::Unbounded, Bound::Excluded(value)),
            (CmpOp::Le, value) => range(Bound::Unbounded, Bound::Included(value)),
            (CmpOp::Gt, value) => range(Bound::Excluded(value), Bound::Unbounded),
            (CmpOp::Ge, value) => range(Bound::Included(value), Bound::Unbounded),
            (CmpOp::Ne, _) => unreachable!("the planner never seeds from `<>`"),
        };
        let filters = [QueryFilter::Scalar {
            index: seed.index,
            predicate,
        }];
        Ok(db
            .prepare_query(QueryRequest {
                collection: seed.collection,
                filters: &filters,
                order: QueryOrder::Driver,
                projection: Projection::Ids,
                total_limit: None,
                driver: CandidateDriver::Auto,
            })
            .map(Some)?)
    }
}

fn range<'a>(lower: Bound<ScalarValue<'a>>, upper: Bound<ScalarValue<'a>>) -> ScalarFilter<'a> {
    ScalarFilter::Range { lower, upper }
}

/// A value read into an index's key kind.
enum Key {
    Int(i64),
    Real(f64),
    Text(String),
    Bool(bool),
}

impl Key {
    fn scalar(&self) -> ScalarValue<'_> {
        match self {
            Self::Int(i) => ScalarValue::I64(*i),
            Self::Real(f) => ScalarValue::F64(*f),
            Self::Text(t) => ScalarValue::Text(t),
            Self::Bool(b) => ScalarValue::Bool(*b),
        }
    }
}

/// `value` as a key of `seed`'s index, so that the index walk admits
/// exactly the rows `field op value` is true for. `None`: no row can be
/// (a `NULL`, or a fraction compared for equality with an integer field).
fn index_key(seed: &IndexSeed, value: &BindingValue) -> Evaluated<Option<Key>> {
    Ok(Some(match (&seed.kind, value) {
        (_, BindingValue::Null) => return Ok(None),
        (Kind::Int, BindingValue::Int(i)) => Key::Int(*i),
        (Kind::Int, BindingValue::Float(f)) if f.is_finite() && f.abs() < 9.2e18 => {
            // An integer field against a fraction: `x < 2.5` is `x < 3`,
            // `x <= 2.5` is `x <= 2`, `x > 2.5` is `x > 2`, `x >= 2.5` is
            // `x >= 3`, and `x = 2.5` holds for no integer.
            let bound = match seed.op {
                CmpOp::Lt | CmpOp::Ge => f.ceil(),
                CmpOp::Le | CmpOp::Gt => f.floor(),
                _ if f.fract() == 0.0 => *f,
                _ => return Ok(None),
            };
            Key::Int(bound as i64)
        }
        (Kind::Real, BindingValue::Int(i)) => Key::Real(*i as f64),
        (Kind::Real, BindingValue::Float(f)) => Key::Real(*f),
        (Kind::Text, BindingValue::Text(t)) => Key::Text(t.to_string()),
        (Kind::Bool, BindingValue::Bool(b)) => Key::Bool(*b),
        (kind, other) => {
            return Err(mismatch(format!(
                "`{}` is indexed as {kind:?} and is compared with {}",
                seed.field,
                self::kind(other)
            )))
        }
    }))
}

/// A comparison under three-valued logic.
fn compare(op: CmpOp, left: &BindingValue, right: &BindingValue) -> Evaluated<BindingValue> {
    use BindingValue as V;
    let order = match (left, right) {
        (V::Null, _) | (_, V::Null) => return Ok(V::Null),
        (V::Int(_) | V::Float(_), V::Int(_) | V::Float(_))
        | (V::Text(_), V::Text(_))
        | (V::Bool(_), V::Bool(_)) => left.cmp(right),
        // Identity, or equality of whole values: `=` and `<>` only.
        (V::Node(_), V::Node(_))
        | (V::Edge(_), V::Edge(_))
        | (V::Json(_), V::Json(_))
        | (V::Geo(_), V::Geo(_))
        | (V::Vector(_), V::Vector(_))
        | (V::Bytes(_), V::Bytes(_))
            if matches!(op, CmpOp::Eq | CmpOp::Ne) =>
        {
            left.cmp(right)
        }
        _ => {
            return Err(mismatch(format!(
                "{} {} {} does not compare",
                kind(left),
                op.written(),
                kind(right)
            )))
        }
    };
    Ok(V::Bool(match op {
        CmpOp::Eq => order == Ordering::Equal,
        CmpOp::Ne => order != Ordering::Equal,
        CmpOp::Lt => order == Ordering::Less,
        CmpOp::Le => order != Ordering::Greater,
        CmpOp::Gt => order == Ordering::Greater,
        CmpOp::Ge => order != Ordering::Less,
    }))
}

/// A list literal's value: its items share one kind (design §2.2), an
/// integer among floats read as a float, as the binder typed the list.
fn list(items: Vec<BindingValue>) -> Evaluated<BindingValue> {
    types::list_of(items).map_err(|(a, b)| {
        mismatch(format!(
            "a list literal holds values of one kind, and this one holds {} and {}",
            types::described(&a),
            types::described(&b)
        ))
    })
}

fn is_null(value: &BindingValue) -> bool {
    matches!(value, BindingValue::Null)
}

/// A value's kind, as an error names it.
pub(super) fn kind(value: &BindingValue) -> &'static str {
    match value {
        BindingValue::Null => "NULL",
        BindingValue::Bool(_) => "a boolean",
        BindingValue::Int(_) => "an integer",
        BindingValue::Float(_) => "a float",
        BindingValue::Text(_) => "text",
        BindingValue::Json(_) => "JSON",
        BindingValue::Vector(_) => "a vector",
        BindingValue::Geo(_) => "a geometry",
        BindingValue::Bytes(_) => "bytes",
        BindingValue::Node(_) => Element::Node.written(),
        BindingValue::Edge(_) => Element::Edge.written(),
        BindingValue::Path(_) => Element::Path.written(),
        BindingValue::List(_) => "a list",
    }
}
