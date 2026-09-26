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
use super::host::{self, HostEx};
use super::scalar;
use super::types;
use crate::ast::{CmpOp, GeoArg, Literal as SqlLiteral, SpatialPredicate, TsQuery};
use crate::sqlstate::{DATATYPE_MISMATCH, DATA_EXCEPTION};
use crate::SqlError;
use sekejap_core::collections::gql::{
    BindingRow, BindingValue, EvalCx, ExprId, GqlHost, SeedId, SlotId, Truth,
};
use sekejap_core::collections::{
    CandidateDriver, CollectionId, Database, Error, IndexId, PreparedQuery, Projection, QueryError,
    GeometryFilter, PointFilter, QueryFilter, QueryOrder, QueryRequest, QueryResult, ScalarFilter,
    ScalarValue, TextMatch, VectorMetric,
    WorkResource,
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

/// `value = ANY(items)`: true on a match, unknown when none matched and a
/// comparison was unknown, false otherwise. Stops at the first match, so a
/// later item is never evaluated.
fn any_equal(
    value: &BindingValue,
    items: impl IntoIterator<Item = Evaluated<BindingValue>>,
) -> Evaluated<BindingValue> {
    let mut unknown = false;
    for item in items {
        match compare(CmpOp::Eq, value, &item?)? {
            BindingValue::Bool(true) => return Ok(BindingValue::Bool(true)),
            BindingValue::Null => unknown = true,
            _ => {}
        }
    }
    Ok(if unknown {
        BindingValue::Null
    } else {
        BindingValue::Bool(false)
    })
}

/// A value of a kind the operation does not take: `42804`.
pub(super) fn mismatch(message: impl fmt::Display) -> Fault {
    raise(DATATYPE_MISMATCH, message)
}

/// An index-candidate seed over one collection: every conjunct of the node
/// a READY index answers, which the engine intersects with its own driver
/// choice (design §3.3). Every value is evaluated when the seed opens, per
/// input row, so the plan stays rebindable.
#[derive(Clone, Debug)]
pub(crate) struct IndexSeed {
    pub(crate) collection: CollectionId,
    pub(crate) filters: Vec<SeedFilter>,
    /// The engine order the rows come in: the driver's own when `None`, an
    /// index order when a limited sort is fed by it (`lineage.rs`).
    pub(crate) order: Option<SeedOrder>,
}

/// An index order a seed is read in.
#[derive(Clone, Debug)]
pub(crate) enum SeedOrder {
    /// Ascending exact distance of the field's vectors to `query`, over its
    /// exact index; `dimension` is the field's declared width. When the
    /// execution opens under `SET LOCAL ef_search`, and the column has an
    /// `approximate` index (vamana, else quantized, as SQL prefers), the
    /// order is that index's `ef`-bounded shortlist instead: APPROXIMATE by
    /// the caller's request (design §3.6).
    Vector {
        index: IndexId,
        approximate: Option<IndexId>,
        metric: VectorMetric,
        query: Ex,
        dimension: usize,
    },
}

impl IndexSeed {
    /// The slots the seed's values read.
    pub(crate) fn refs(&self) -> Vec<SlotId> {
        self.filters
            .iter()
            .flat_map(|filter| match filter {
                SeedFilter::Scalar { value, .. } => value.refs(),
                SeedFilter::Text { .. } | SeedFilter::Spatial { .. } => Vec::new(),
            })
            .chain(self.order.iter().flat_map(|order| match order {
                SeedOrder::Vector { query, .. } => query.refs(),
            }))
            .collect()
    }
}

/// One conjunct of an [`IndexSeed`].
#[derive(Clone, Debug)]
pub(crate) enum SeedFilter {
    /// `field op value` over a scalar index.
    Scalar {
        index: IndexId,
        /// The index's key kind, which the value must be read into.
        kind: Kind,
        field: String,
        /// The comparison with the field on the LEFT; never `<>`.
        op: CmpOp,
        value: Ex,
    },
    /// A text match over the field's text index.
    Text { index: IndexId, query: TsQuery },
    /// A spatial predicate over a point index (`point`) or a geometry index.
    Spatial {
        index: IndexId,
        point: bool,
        predicate: SpatialPredicate,
        shape: GeoArg,
        metres: Option<SqlLiteral>,
    },
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

    /// Seed `id`.
    pub(crate) fn seed_at(&self, id: SeedId) -> &IndexSeed {
        &self.seeds[id.0 as usize]
    }

    /// The collection seed `id` reads.
    pub(crate) fn seed_collection(&self, id: SeedId) -> CollectionId {
        self.seeds[id.0 as usize].collection
    }

    pub(crate) fn seed_mut(&mut self, id: SeedId) -> &mut IndexSeed {
        &mut self.seeds[id.0 as usize]
    }

    /// The slots seed `id` reads, evaluated per input row.
    pub(crate) fn seed_refs(&self, id: SeedId) -> Vec<SlotId> {
        self.seeds[id.0 as usize].refs()
    }

    /// A host form (M6, `host.rs`): a text form through the node's text
    /// index, a spatial or vector form pure over the values it reads; a
    /// vector distance charges the lanes it compares, as the engine's
    /// vector scan does.
    fn host(&self, host: &HostEx, row: View<'_>, cx: &mut EvalCx<'_, '_>) -> Evaluated<BindingValue> {
        Ok(match host {
            HostEx::Text {
                node,
                indexes,
                query,
                score,
                ..
            } => match row.get(*node) {
                BindingValue::Node(node) => {
                    let (text, matching) = host::text_query(query, cx.params)?;
                    let index = host::text_index(indexes, node.0.collection).ok_or_else(|| {
                        mismatch("a text form met a node of a collection its label does not name")
                    })?;
                    if *score {
                        BindingValue::Float(cx.reader.text_score(*node, index, &text, matching, cx.meter)?)
                    } else {
                        BindingValue::Bool(cx.reader.text_matches(*node, index, &text, matching, cx.meter)?)
                    }
                }
                _ => BindingValue::Null,
            },
            HostEx::Spatial {
                predicate,
                left,
                shape,
                metres,
            } => {
                let left = self.value(left, row, cx)?;
                host::spatial_value(Some(*predicate), &left, shape, metres.as_ref(), cx.params)?
            }
            HostEx::Distance { left, shape } => {
                let left = self.value(left, row, cx)?;
                host::spatial_value(None, &left, shape, None, cx.params)?
            }
            HostEx::Vector { op, left, right } => {
                let (left, right) = (self.value(left, row, cx)?, self.value(right, row, cx)?);
                let (value, lanes) = host::vector_value(*op, &left, &right)?;
                cx.meter.charge(WorkResource::VectorLanes, lanes as u64)?;
                value
            }
        })
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
                any_equal(&value, list.iter().map(|member| self.value(member, row, cx)))?
            }
            // `x = ANY(list)`: no element is false even for a NULL `x`, and
            // a NULL list is unknown.
            Ex::Member(value, list) => match self.value(list, row, cx)? {
                BindingValue::Null => BindingValue::Null,
                BindingValue::List(list) if list.items.is_empty() => BindingValue::Bool(false),
                BindingValue::List(list) => {
                    let value = self.value(value, row, cx)?;
                    if is_null(&value) {
                        return Ok(BindingValue::Null);
                    }
                    any_equal(&value, list.items.iter().cloned().map(Ok))?
                }
                other => {
                    return Err(mismatch(format!(
                        "IN takes a list here, not {}",
                        types::described(&types::value_type(&other))
                    )))
                }
            },
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
            Ex::Host(host) => self.host(host, row, cx)?,
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
        // Each filter's values, owned; `None` when one admits no row.
        let mut built = Vec::with_capacity(seed.filters.len());
        for filter in &seed.filters {
            built.push(match filter {
                SeedFilter::Scalar {
                    index,
                    kind,
                    field,
                    op,
                    value,
                } => {
                    let value = self.value(value, View::of(row), cx)?;
                    let Some(key) = index_key(kind, field, *op, &value)? else {
                        return Ok(None);
                    };
                    Built::Scalar(*index, *op, key)
                }
                SeedFilter::Text { index, query } => {
                    let (text, matching) = host::text_query(query, cx.params)?;
                    Built::Text(*index, text, matching)
                }
                SeedFilter::Spatial {
                    index,
                    point,
                    predicate,
                    shape,
                    metres,
                } => match host::spatial_seed(*point, *predicate, shape, metres.as_ref(), cx.params)? {
                    Some(host::SpatialSeed::Point(filter)) => Built::Point(*index, filter),
                    Some(host::SpatialSeed::Geometry(filter)) => Built::Geometry(*index, filter),
                    None => return Ok(None),
                },
            });
        }
        let filters: Vec<QueryFilter<'_>> = built.iter().map(Built::filter).collect();
        let lanes;
        let order = match &seed.order {
            None => QueryOrder::Driver,
            Some(SeedOrder::Vector {
                index,
                approximate,
                metric,
                query,
                dimension,
            }) => match host::lanes(&self.value(query, View::of(row), cx)?)? {
                // A NULL query: every distance is NULL, so no order is
                // asked of the index and the sort keeps its input order.
                None => QueryOrder::Driver,
                Some(query) if query.len() != *dimension => {
                    return Err(raise(
                        DATA_EXCEPTION,
                        format!("different vector dimensions {dimension} and {}", query.len()),
                    ))
                }
                Some(query) => {
                    lanes = query;
                    match (crate::compile::ef_search(), approximate) {
                        (Some(ef), Some(approximate)) => QueryOrder::ApproximateVector {
                            index: *approximate,
                            query: &lanes,
                            metric: *metric,
                            ef,
                        },
                        _ => QueryOrder::ExactVector {
                            index: *index,
                            query: &lanes,
                            metric: *metric,
                        },
                    }
                }
            },
        };
        Ok(db
            .prepare_query(QueryRequest {
                collection: seed.collection,
                filters: &filters,
                order,
                projection: Projection::Ids,
                total_limit: None,
                driver: CandidateDriver::Auto,
            })
            .map(Some)?)
    }
}

/// A seed filter's values, owned while the engine's borrowed filter is built.
enum Built {
    Scalar(IndexId, CmpOp, Key),
    Text(IndexId, String, TextMatch),
    Point(IndexId, PointFilter),
    Geometry(IndexId, GeometryFilter),
}

impl Built {
    fn filter(&self) -> QueryFilter<'_> {
        match self {
            Self::Scalar(index, op, key) => QueryFilter::Scalar {
                index: *index,
                predicate: match (op, key.scalar()) {
                    (CmpOp::Eq, value) => ScalarFilter::Eq(value),
                    (CmpOp::Lt, value) => range(Bound::Unbounded, Bound::Excluded(value)),
                    (CmpOp::Le, value) => range(Bound::Unbounded, Bound::Included(value)),
                    (CmpOp::Gt, value) => range(Bound::Excluded(value), Bound::Unbounded),
                    (CmpOp::Ge, value) => range(Bound::Included(value), Bound::Unbounded),
                    (CmpOp::Ne, _) => unreachable!("the planner never seeds from `<>`"),
                },
            },
            Self::Text(index, query, matching) => QueryFilter::Text {
                index: *index,
                query,
                matching: *matching,
            },
            Self::Point(index, predicate) => QueryFilter::Point {
                index: *index,
                predicate: *predicate,
            },
            Self::Geometry(index, predicate) => QueryFilter::Geometry {
                index: *index,
                predicate: predicate.clone(),
            },
        }
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

/// `value` as a key of an index of `kind`, so that the index walk admits
/// exactly the rows `field op value` is true for. `None`: no row can be
/// (a `NULL`, or a fraction compared for equality with an integer field).
fn index_key(kind: &Kind, field: &str, op: CmpOp, value: &BindingValue) -> Evaluated<Option<Key>> {
    Ok(Some(match (kind, value) {
        (_, BindingValue::Null) => return Ok(None),
        (Kind::Int, BindingValue::Int(i)) => Key::Int(*i),
        (Kind::Int, BindingValue::Float(f)) if f.is_finite() && f.abs() < 9.2e18 => {
            // An integer field against a fraction: `x < 2.5` is `x < 3`,
            // `x <= 2.5` is `x <= 2`, `x > 2.5` is `x > 2`, `x >= 2.5` is
            // `x >= 3`, and `x = 2.5` holds for no integer.
            let bound = match op {
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
                field,
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
