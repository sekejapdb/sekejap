//! The aggregation atomic (`docs/QL_CONTRACT.md` §4.7): `count(*)`,
//! `count(col)`, `sum`, `min`, `max`, `avg`, `GROUP BY`, `HAVING`, `DISTINCT`.
//!
//! Nothing here is a second candidate engine. An [`AggregateRequest`] is
//! prepared by building the SAME plan a `SELECT` with those filters would
//! build -- `Database::prepare_query`, so the driver choice, the membership
//! sets and the per-candidate filters are the ones `src/query/plan.rs`,
//! `src/query/membership.rs` and `src/query/filters.rs` already make -- and
//! this file adds one thing to it: a fold over the candidates that survive.
//!
//! ## The two shapes, chosen at prepare and printed by EXPLAIN
//!
//! **STREAMING.** The group key is the DRIVING scalar index's own value. A
//! scalar posting range is `value || sequence`, so the candidates of one
//! group arrive contiguously and in ascending key order: one accumulator set
//! is alive at a time, the key comes off the posting with no row read, and a
//! page stops at a group boundary and resumes there. Memory is constant in
//! the number of groups. A grouping EXPRESSION rides here too when it is
//! MONOTONE in that value -- `born / 10000` is, because truncating division
//! by a positive divisor never decreases -- so the expression is computed
//! index-side from the posting and the groups stay contiguous.
//!
//! **HASHED.** Everything else: one accumulator set per distinct group, all
//! alive at once, so no group is final until the walk ends and the whole
//! walk happens on the first page. That memory is the thing the new
//! [`WorkResource::Groups`] budget bounds. There is no spill in this item:
//! past the cap the page is refused with `BudgetExceeded { groups }`.
//!
//! ## Where an accumulator's input comes from
//!
//! From the driving scalar index's posting when the field IS that index
//! (index-side, no row), and from the primary row otherwise -- charged as
//! `primary_reads` and printed by EXPLAIN as `row`. A membership set is not
//! a third source: it answers whether a candidate is IN a set, not what its
//! value is, and there is no per-sequence lookup into a `value || sequence`
//! keyspace. That is the whole of the deviation from the brief's wording, and
//! it is stated rather than emulated.
use super::*;
use std::collections::BTreeMap;

/// The most accumulators one request may carry. The bound exists so the
/// per-group memory in [`default_groups_cap`] is a fixed quantity.
pub const MAX_ACCUMULATORS: usize = 16;

/// The most `HAVING` predicates one request may carry.
pub const MAX_HAVING: usize = 16;

/// The aggregate functions of `docs/QL_CONTRACT.md` §4.7.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregateFn {
    /// `count(*)`: rows, whatever they hold. Takes no input.
    CountStar,
    /// `count(col)`: rows whose input is neither NULL nor missing.
    Count,
    Sum,
    Min,
    Max,
    Avg,
}

impl AggregateFn {
    fn takes_input(self) -> bool {
        !matches!(self, Self::CountStar)
    }

    fn numeric_only(self) -> bool {
        matches!(self, Self::Sum | Self::Avg)
    }

    pub fn written(self) -> &'static str {
        match self {
            Self::CountStar => "count(*)",
            Self::Count => "count",
            Self::Sum => "sum",
            Self::Min => "min",
            Self::Max => "max",
            Self::Avg => "avg",
        }
    }
}

/// Where one accumulator reads its input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregateInput<'a> {
    /// A scalar index. Read from the posting when that index is the driving
    /// one, from the row otherwise.
    Index(IndexId),
    /// A declared field with no scalar index: always read from the row.
    Field(&'a str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Accumulator<'a> {
    pub function: AggregateFn,
    /// `None` only for [`AggregateFn::CountStar`].
    pub input: Option<AggregateInput<'a>>,
}

/// What the rows are grouped by. `None` in a request is one group over the
/// whole candidate set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupKey<'a> {
    /// The value of a scalar index.
    Index(IndexId),
    /// A declared field with no scalar index: read from the row.
    Field(&'a str),
    /// `col / divisor` on an `Int` scalar index, truncating toward zero the
    /// way SQL integer division does. Monotone for a positive divisor, which
    /// is why it can stream; `divisor` must be positive and is refused
    /// otherwise.
    IndexDiv { index: IndexId, divisor: i64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupCmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl GroupCmp {
    pub fn written(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Ne => "<>",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        }
    }

    fn holds(self, ordering: Ordering) -> bool {
        match self {
            Self::Eq => ordering == Ordering::Equal,
            Self::Ne => ordering != Ordering::Equal,
            Self::Lt => ordering == Ordering::Less,
            Self::Le => ordering != Ordering::Greater,
            Self::Gt => ordering == Ordering::Greater,
            Self::Ge => ordering != Ordering::Less,
        }
    }
}

/// One `HAVING` predicate: a comparison against the value of one accumulator,
/// by its position in the request. Applied to a FINISHED group, before paging,
/// so a rejected group never occupies a row of a page.
///
/// The comparison is numeric. An accumulator whose value is text (`min`/`max`
/// over a Text column) is refused at prepare rather than compared through a
/// coercion nobody asked for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GroupPredicate {
    pub accumulator: usize,
    pub op: GroupCmp,
    pub value: f64,
}

/// The order finished groups are handed back in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupOrder {
    /// Ascending group key. Free under STREAMING -- it is the walk's own
    /// order -- and the natural order of the HASHED map.
    Key,
    /// One accumulator's value. THE ONE PLACE A SORT OVER MEMORY HAPPENS in
    /// this engine: the finished groups are sorted before they are paged.
    /// It is bounded because the thing being sorted is the group table, and
    /// the group table is bounded by [`WorkResource::Groups`] -- so the sort
    /// is over at most `groups` entries and never over the collection. It
    /// forces the HASHED shape: no group's value is final until the walk
    /// ends, so there is nothing to sort before then.
    Accumulator { at: usize, direction: SortDirection },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregateShape {
    Streaming,
    Hashed,
}

impl AggregateShape {
    pub fn written(self) -> &'static str {
        match self {
            Self::Streaming => "streaming",
            Self::Hashed => "hashed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct AggregateRequest<'a> {
    pub collection: CollectionId,
    pub filters: &'a [QueryFilter<'a>],
    pub group: Option<GroupKey<'a>>,
    pub accumulators: &'a [Accumulator<'a>],
    pub having: &'a [GroupPredicate],
    /// Deviation from the brief's field list, stated: the brief names
    /// `driver` and `total_limit` but not the group order, and
    /// `ORDER BY <aggregate alias>` has to reach the engine somewhere. It is
    /// a field rather than a second entry point.
    pub order: GroupOrder,
    pub driver: CandidateDriver,
    /// A ceiling on GROUPS returned, not on rows folded.
    pub total_limit: Option<usize>,
}

/// One accumulator's value, in SQL's own types.
#[derive(Clone, Debug, PartialEq)]
pub enum AggValue {
    /// `count(*)` and `count(col)`, which are never NULL.
    Count(u64),
    I64(i64),
    F64(f64),
    Text(String),
    Bool(bool),
    /// No row contributed a non-null input, which is SQL's NULL for `sum`,
    /// `min`, `max` and `avg`.
    Null,
}

impl AggValue {
    /// The value as a number, for `HAVING` and for an ordering. `None` for a
    /// text value and for NULL.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Count(n) => Some(*n as f64),
            Self::I64(v) => Some(*v as f64),
            Self::F64(v) => Some(*v),
            Self::Bool(v) => Some(if *v { 1.0 } else { 0.0 }),
            Self::Text(_) | Self::Null => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GroupRow {
    /// `None` when the request named no group key: one group over everything.
    ///
    /// Deviation from the brief's spelling, stated: the brief writes
    /// `Option<ScalarValue>`, and `ScalarValue<'a>` BORROWS its text from the
    /// request. A returned group row outlives the walk that produced it, so
    /// it owns its key -- [`OwnedScalarValue`], the type `OrderValue::Scalar`
    /// already hands a caller.
    pub key: Option<OwnedScalarValue>,
    pub values: Vec<AggValue>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GroupPage {
    pub groups: Vec<GroupRow>,
    pub done: bool,
    pub work: QueryWork,
}

/// The heap bytes a group KEY holds beyond its inline size: a Text key's
/// string. Charged against the groups budget in units of [`group_bytes`].
fn key_heap_bytes(key: &OwnedScalarValue) -> usize {
    match key {
        OwnedScalarValue::Text(t) => t.len(),
        _ => 0,
    }
}

/// What ONE group costs while it is alive: its key and its accumulators.
fn group_bytes(accumulators: usize) -> usize {
    std::mem::size_of::<OwnedScalarValue>()
        + std::mem::size_of::<GroupOrdKey>()
        + accumulators.max(1) * std::mem::size_of::<Acc>()
}

/// The default ceiling on accumulator sets held at once. See
/// [`QueryBudget::groups_cap`].
pub(super) fn default_groups_cap(accumulators: usize) -> u64 {
    (RUN_BYTES / group_bytes(accumulators)) as u64
}

// ── the compiled request ──────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct CompiledSource {
    /// The scalar index this source names, when it names one.
    info: Option<IndexInfo>,
    /// The declared field the row path reads.
    field: String,
    /// The field's declared kind, so a HAVING over a text-valued min/max is
    /// refused at prepare and not discovered mid-fold.
    kind: Kind,
    /// True when the driving walk's own posting carries the value, so nothing
    /// reads a row for it.
    index_side: bool,
}

impl CompiledSource {
    fn where_from(&self) -> &'static str {
        if self.index_side {
            "index posting: the driving walk carries the value"
        } else {
            "row (charged as primary_reads)"
        }
    }
}

#[derive(Clone, Debug)]
struct CompiledGroup {
    source: CompiledSource,
    /// `Some` for the `col / divisor` expression key.
    divisor: Option<i64>,
}

#[derive(Clone, Debug)]
struct CompiledAccumulator {
    function: AggregateFn,
    source: Option<CompiledSource>,
}

/// One live accumulator.
#[derive(Clone, Debug)]
enum Acc {
    Count(u64),
    Sum {
        ints: i128,
        floats: f64,
        saw_float: bool,
        any: bool,
    },
    Extreme {
        best: Option<OwnedScalarValue>,
        max: bool,
    },
    Avg {
        sum: f64,
        n: u64,
    },
}

impl Acc {
    fn new(function: AggregateFn) -> Self {
        match function {
            AggregateFn::CountStar | AggregateFn::Count => Self::Count(0),
            AggregateFn::Sum => Self::Sum {
                ints: 0,
                floats: 0.0,
                saw_float: false,
                any: false,
            },
            AggregateFn::Min => Self::Extreme {
                best: None,
                max: false,
            },
            AggregateFn::Max => Self::Extreme {
                best: None,
                max: true,
            },
            AggregateFn::Avg => Self::Avg { sum: 0.0, n: 0 },
        }
    }

    /// Fold one candidate in. `input` is `None` for `count(*)`, which has no
    /// input at all; a `Nullish` input is skipped by every function, which is
    /// SQL's rule for NULL in an aggregate.
    fn fold(&mut self, input: Option<&OwnedScalarValue>) -> QueryResult<usize> {
        let value = match input {
            None => {
                if let Self::Count(n) = self {
                    *n = n.saturating_add(1);
                }
                return Ok(0);
            }
            Some(OwnedScalarValue::Nullish) => return Ok(0),
            Some(value) => value,
        };
        match self {
            Self::Count(n) => *n = n.saturating_add(1),
            Self::Sum {
                ints,
                floats,
                saw_float,
                any,
            } => {
                *any = true;
                match value {
                    OwnedScalarValue::I64(v) => *ints += i128::from(*v),
                    OwnedScalarValue::F64(v) => {
                        *saw_float = true;
                        *floats += *v;
                    }
                    OwnedScalarValue::Bool(_) | OwnedScalarValue::Text(_) => {
                        return Err(invalid_query("sum requires a numeric input"))
                    }
                    OwnedScalarValue::Nullish => {}
                }
            }
            Self::Extreme { best, max } => {
                let replace = match best {
                    None => true,
                    Some(current) => {
                        let ordering = compare_scalar(value, current);
                        if *max {
                            ordering == Ordering::Greater
                        } else {
                            ordering == Ordering::Less
                        }
                    }
                };
                if replace {
                    // Bytes newly held: a longer text extreme grows the
                    // group's footprint, and the groups budget is a BYTE
                    // bound (QL_CONTRACT §6), so the caller charges it.
                    let before = match best {
                        Some(OwnedScalarValue::Text(t)) => t.len(),
                        _ => 0,
                    };
                    let after = match value {
                        OwnedScalarValue::Text(t) => t.len(),
                        _ => 0,
                    };
                    *best = Some(value.clone());
                    return Ok(after.saturating_sub(before));
                }
            }
            Self::Avg { sum, n } => {
                let number = match value {
                    OwnedScalarValue::I64(v) => *v as f64,
                    OwnedScalarValue::F64(v) => *v,
                    OwnedScalarValue::Bool(_) | OwnedScalarValue::Text(_) => {
                        return Err(invalid_query("avg requires a numeric input"))
                    }
                    OwnedScalarValue::Nullish => return Ok(0),
                };
                *sum += number;
                *n = n.saturating_add(1);
            }
        }
        Ok(0)
    }

    fn finish(&self) -> QueryResult<AggValue> {
        Ok(match self {
            Self::Count(n) => AggValue::Count(*n),
            Self::Sum {
                ints,
                floats,
                saw_float,
                any,
            } => {
                if !*any {
                    AggValue::Null
                } else if *saw_float {
                    AggValue::F64(*ints as f64 + *floats)
                } else {
                    AggValue::I64(i64::try_from(*ints).map_err(|_| {
                        invalid_query("sum exceeds the range of a 64-bit integer")
                    })?)
                }
            }
            Self::Extreme { best, .. } => match best {
                None => AggValue::Null,
                Some(OwnedScalarValue::I64(v)) => AggValue::I64(*v),
                Some(OwnedScalarValue::F64(v)) => AggValue::F64(*v),
                Some(OwnedScalarValue::Text(v)) => AggValue::Text(v.clone()),
                Some(OwnedScalarValue::Bool(v)) => AggValue::Bool(*v),
                Some(OwnedScalarValue::Nullish) => AggValue::Null,
            },
            Self::Avg { sum, n } => {
                if *n == 0 {
                    AggValue::Null
                } else {
                    AggValue::F64(*sum / *n as f64)
                }
            }
        })
    }
}

/// A total order over group keys, so the group table is a `BTreeMap` and its
/// iteration order IS ascending key order -- no sort, and no hash of an
/// `f64`. Types are ordered before values: nullish, then bool, then numbers,
/// then text, which is the order the scalar keyspace itself files them in.
#[derive(Clone, Debug)]
struct GroupOrdKey(OwnedScalarValue);

fn type_rank(value: &OwnedScalarValue) -> u8 {
    match value {
        OwnedScalarValue::Nullish => 0,
        OwnedScalarValue::Bool(_) => 1,
        OwnedScalarValue::I64(_) | OwnedScalarValue::F64(_) => 2,
        OwnedScalarValue::Text(_) => 3,
    }
}

fn compare_scalar(left: &OwnedScalarValue, right: &OwnedScalarValue) -> Ordering {
    match (left, right) {
        (OwnedScalarValue::I64(a), OwnedScalarValue::I64(b)) => a.cmp(b),
        (OwnedScalarValue::F64(a), OwnedScalarValue::F64(b)) => a.total_cmp(b),
        (OwnedScalarValue::I64(a), OwnedScalarValue::F64(b)) => (*a as f64).total_cmp(b),
        (OwnedScalarValue::F64(a), OwnedScalarValue::I64(b)) => a.total_cmp(&(*b as f64)),
        (OwnedScalarValue::Text(a), OwnedScalarValue::Text(b)) => a.cmp(b),
        (OwnedScalarValue::Bool(a), OwnedScalarValue::Bool(b)) => a.cmp(b),
        (a, b) => type_rank(a).cmp(&type_rank(b)),
    }
}

impl PartialEq for GroupOrdKey {
    fn eq(&self, other: &Self) -> bool {
        compare_scalar(&self.0, &other.0) == Ordering::Equal
    }
}
impl Eq for GroupOrdKey {}
impl PartialOrd for GroupOrdKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for GroupOrdKey {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_scalar(&self.0, &other.0)
    }
}

/// One row field, as a scalar. A missing field and an explicit null are the
/// same group -- the scalar keyspace gives both the one nullish key, so the
/// row path has to agree with the index path or the two shapes would answer
/// differently.
fn field_scalar(value: dense_v3::FieldValue) -> QueryResult<OwnedScalarValue> {
    Ok(match value {
        dense_v3::FieldValue::Missing | dense_v3::FieldValue::Null => OwnedScalarValue::Nullish,
        dense_v3::FieldValue::Vector { .. } => {
            return Err(invalid_query("an aggregate input is a vector, not a scalar"))
        }
        dense_v3::FieldValue::Inline(value) => match value {
            Value::Null => OwnedScalarValue::Nullish,
            Value::Bool(v) => OwnedScalarValue::Bool(v),
            Value::Number(v) => match v.as_i64() {
                Some(v) => OwnedScalarValue::I64(v),
                None => OwnedScalarValue::F64(
                    v.as_f64()
                        .ok_or_else(|| corrupt_query("aggregate input is not a finite number"))?,
                ),
            },
            Value::String(v) => OwnedScalarValue::Text(v),
            Value::Array(_) | Value::Object(_) => {
                return Err(invalid_query(
                    "an aggregate input must be a scalar field, not an array or an object",
                ))
            }
        },
    })
}

// ── preparing ─────────────────────────────────────────────────────────────

impl Database {
    /// Prepare an aggregate over the plan a `SELECT` with these filters would
    /// have built.
    pub fn prepare_aggregate<'db>(
        &'db self,
        request: AggregateRequest<'_>,
    ) -> QueryResult<PreparedAggregate<'db>> {
        if request.accumulators.len() > MAX_ACCUMULATORS {
            return Err(invalid_query("an aggregate has more than 16 accumulators"));
        }
        if request.having.len() > MAX_HAVING {
            return Err(invalid_query("an aggregate has more than 16 HAVING predicates"));
        }
        for predicate in request.having {
            if predicate.accumulator >= request.accumulators.len() {
                return Err(invalid_query("a HAVING predicate names no accumulator"));
            }
            if !predicate.value.is_finite() {
                return Err(invalid_query("a HAVING value must be finite"));
            }
        }
        if let GroupOrder::Accumulator { at, .. } = request.order {
            if at >= request.accumulators.len() {
                return Err(invalid_query("the group order names no accumulator"));
            }
        }

        let collection = self.collection_info(request.collection)?;
        let declared = |field: &str| -> QueryResult<Kind> {
            if field.is_empty() || reserved(field) {
                return Err(invalid_query("an aggregate names an invalid field"));
            }
            if let Some((_, kind)) = collection
                .layout
                .fields
                .iter()
                .find(|(name, _)| name == field)
            {
                Ok(kind.clone())
            } else {
                Err(invalid_query(
                    "an aggregate names a field this collection does not declare",
                ))
            }
        };

        // The group key, before the driver is known. `index_side` is filled
        // in once the plan has chosen one.
        let mut group = match request.group {
            None => None,
            Some(GroupKey::Index(index)) => {
                let info = require_scalar_index(self, request.collection, index)?;
                Some(CompiledGroup {
                    source: CompiledSource {
                        field: info.field.clone(),
                        kind: info.kind.clone(),
                        info: Some(info),
                        index_side: false,
                    },
                    divisor: None,
                })
            }
            Some(GroupKey::IndexDiv { index, divisor }) => {
                let info = require_scalar_index(self, request.collection, index)?;
                if info.kind != Kind::Int {
                    return Err(invalid_query(
                        "a divided group key requires an Int scalar index",
                    ));
                }
                if divisor <= 0 {
                    return Err(invalid_query(
                        "a divided group key requires a positive divisor: only then is the expression monotone in the index's own order, which is what lets it stream",
                    ));
                }
                Some(CompiledGroup {
                    source: CompiledSource {
                        field: info.field.clone(),
                        kind: info.kind.clone(),
                        info: Some(info),
                        index_side: false,
                    },
                    divisor: Some(divisor),
                })
            }
            Some(GroupKey::Field(field)) => {
                let kind = declared(field)?;
                Some(CompiledGroup {
                    source: CompiledSource {
                        kind,
                        info: None,
                        field: field.to_owned(),
                        index_side: false,
                    },
                    divisor: None,
                })
            }
        };

        let mut accumulators = Vec::with_capacity(request.accumulators.len());
        for accumulator in request.accumulators {
            let source = match (accumulator.function.takes_input(), accumulator.input) {
                (false, None) => None,
                (false, Some(_)) => {
                    return Err(invalid_query("count(*) takes no input"))
                }
                (true, None) => {
                    return Err(invalid_query("this aggregate function requires an input"))
                }
                (true, Some(AggregateInput::Index(index))) => {
                    let info = require_scalar_index(self, request.collection, index)?;
                    if accumulator.function.numeric_only()
                        && !matches!(info.kind, Kind::Int | Kind::Real)
                    {
                        return Err(invalid_query(
                            "sum and avg require a numeric input column",
                        ));
                    }
                    Some(CompiledSource {
                        field: info.field.clone(),
                        kind: info.kind.clone(),
                        info: Some(info),
                        index_side: false,
                    })
                }
                (true, Some(AggregateInput::Field(field))) => {
                    let kind = declared(field)?;
                    Some(CompiledSource {
                        kind,
                        info: None,
                        field: field.to_owned(),
                        index_side: false,
                    })
                }
            };
            accumulators.push(CompiledAccumulator {
                function: accumulator.function,
                source,
            });
        }

        // HAVING compares numbers. A min/max over a Text (or Bool) column
        // yields text, so a HAVING on it is refused HERE, at prepare, and not
        // discovered after a whole hashed walk.
        for predicate in request.having {
            let accumulator = &accumulators[predicate.accumulator];
            if matches!(accumulator.function, AggregateFn::Min | AggregateFn::Max)
                && accumulator
                    .source
                    .as_ref()
                    .is_some_and(|source| !matches!(source.kind, Kind::Int | Kind::Real))
            {
                return Err(invalid_query(
                    "HAVING compares numbers; min/max over a non-numeric column yields text or bool",
                ));
            }
        }

        // `count(*)` over the whole collection with no filters is the one
        // case that names no index at all, and the mapping keyspace is the
        // cheapest complete enumeration of a collection -- the same choice
        // `popsim`'s and `q7_budget`'s `count_all` make
        // (`CandidateDriver::Keys`, `src/query/plan.rs`'s `keys_driver`).
        let count_all = request.filters.is_empty()
            && request.group.is_none()
            && !accumulators.is_empty()
            && accumulators
                .iter()
                .all(|accumulator| accumulator.function == AggregateFn::CountStar);
        let driver = match (request.driver, count_all) {
            (CandidateDriver::Auto, true) => CandidateDriver::Keys,
            (driver, _) => driver,
        };

        // A group key over a scalar index asks for that index's own order, so
        // `CandidateDriver::Auto` can choose it when nothing cheaper drives --
        // which is exactly when the groups arrive contiguous. With a filter
        // that drives better, Auto picks the filter and the shape is hashed.
        let group_index = group
            .as_ref()
            .and_then(|group| group.source.info.as_ref().map(|info| info.id));
        let query = match group_index {
            Some(index) => self.prepare_query(QueryRequest {
                collection: request.collection,
                filters: request.filters,
                order: QueryOrder::Scalar {
                    index,
                    direction: SortDirection::Ascending,
                },
                projection: Projection::Ids,
                total_limit: None,
                driver,
            })?,
            None => self.prepare_query(QueryRequest {
                collection: request.collection,
                filters: request.filters,
                order: QueryOrder::Driver,
                projection: Projection::Ids,
                total_limit: None,
                driver,
            })?,
        };

        if matches!(query.driver, DriverPlan::Nearest { .. }) {
            // Unreachable as the planner stands -- a nearest walk is chosen
            // only under `QueryOrder::Distance`, which an aggregate never
            // asks for -- and stated here rather than left for a future
            // planner to discover: the walk is stateful across pages and this
            // file does not carry it.
            return Err(invalid_query(
                "an aggregate cannot be driven by the ordered nearest walk",
            ));
        }

        // Which index, if any, the driving walk carries a value for.
        let carried = match (&query.driver, query.cursor_needs().scalar_key) {
            (DriverPlan::Scalar { info, .. }, true) => Some(info.id),
            _ => None,
        };
        let index_side = |source: &mut CompiledSource| {
            source.index_side = matches!(
                (&source.info, carried),
                (Some(info), Some(driving)) if info.id == driving
            );
        };
        if let Some(group) = group.as_mut() {
            index_side(&mut group.source);
        }
        for accumulator in &mut accumulators {
            if let Some(source) = accumulator.source.as_mut() {
                index_side(source);
            }
        }

        let streaming_key = match (&group, request.order) {
            (None, GroupOrder::Key) => true,
            (Some(group), GroupOrder::Key) => group.source.index_side,
            // A ranking over an accumulator cannot be answered before the
            // walk has finished every group, so it is hashed by definition.
            (_, GroupOrder::Accumulator { .. }) => false,
        };
        let shape = if streaming_key {
            AggregateShape::Streaming
        } else {
            AggregateShape::Hashed
        };

        let groups_cap = default_groups_cap(accumulators.len());
        Ok(PreparedAggregate {
            query,
            group,
            accumulators,
            having: request.having.to_vec(),
            order: request.order,
            shape,
            total_limit: request.total_limit,
            groups_cap,
            groups_seen: 0,
            emitted: 0,
            after: None,
            done: false,
            pending: None,
        })
    }
}

// ── the prepared aggregate ────────────────────────────────────────────────

pub struct PreparedAggregate<'db> {
    query: PreparedQuery<'db>,
    group: Option<CompiledGroup>,
    accumulators: Vec<CompiledAccumulator>,
    having: Vec<GroupPredicate>,
    order: GroupOrder,
    shape: AggregateShape,
    total_limit: Option<usize>,
    /// The engine's own ceiling on live accumulator sets, applied on top of
    /// whatever the caller's budget says.
    groups_cap: u64,
    /// Distinct groups this prepared aggregate has opened so far, for EXPLAIN.
    groups_seen: u64,
    /// Groups handed out so far, against which `total_limit` is spent.
    emitted: usize,
    /// Where a STREAMING page resumes: the rank key of the last candidate of
    /// the last group it emitted. Committed only once the page is built.
    after: Option<RankKey>,
    done: bool,
    /// A HASHED aggregate's finished groups, in order, front first. `Some`
    /// once the single fold has run.
    pending: Option<Vec<GroupRow>>,
}

/// What one group is while it is being folded.
struct OpenGroup {
    key: Option<OwnedScalarValue>,
    accumulators: Vec<Acc>,
    /// The rank key of the last candidate folded into it, which is where a
    /// STREAMING page resumes when this group is the last one it emits.
    last: Option<RankKey>,
}

impl PreparedAggregate<'_> {
    pub fn shape(&self) -> AggregateShape {
        self.shape
    }

    /// The groups this prepared aggregate has opened so far. Zero before the
    /// first page, like every membership set the plan reports.
    pub fn groups_seen(&self) -> u64 {
        self.groups_seen
    }

    fn finish_group(&mut self, open: OpenGroup) -> QueryResult<Option<GroupRow>> {
        let mut values = Vec::with_capacity(open.accumulators.len());
        for accumulator in &open.accumulators {
            values.push(accumulator.finish()?);
        }
        for predicate in &self.having {
            // An all-nullish group yields NULL, and `NULL <op> value` is
            // neither true nor false: the group is dropped, as SQL drops it.
            // A text value cannot reach here -- prepare refused it.
            let Some(number) = values[predicate.accumulator].as_f64() else {
                return Ok(None);
            };
            if !predicate.op.holds(number.total_cmp(&predicate.value)) {
                return Ok(None);
            }
        }
        Ok(Some(GroupRow {
            key: open.key,
            values,
        }))
    }

    fn open_group(&mut self, key: Option<OwnedScalarValue>) -> OpenGroup {
        OpenGroup {
            key,
            accumulators: self
                .accumulators
                .iter()
                .map(|accumulator| Acc::new(accumulator.function))
                .collect(),
            last: None,
        }
    }

    /// The group key and every accumulator input of one candidate, read from
    /// the posting where the driving walk carries it and from the row where
    /// it does not.
    #[allow(clippy::too_many_arguments)]
    fn inputs<'a, C: FnMut() -> bool>(
        &self,
        db: &'a Database,
        rows: &mut PrimaryRows<'a>,
        candidate: &Candidate,
        row: &mut Option<RowData>,
        encoded: &mut Option<Vec<u8>>,
        meter: &mut WorkMeter<'_, C>,
        inputs: &mut Vec<Option<OwnedScalarValue>>,
    ) -> QueryResult<Option<OwnedScalarValue>> {
        let read = |source: &CompiledSource,
                    row: &mut Option<RowData>,
                    encoded: &mut Option<Vec<u8>>,
                    rows: &mut PrimaryRows<'a>,
                    meter: &mut WorkMeter<'_, C>|
         -> QueryResult<OwnedScalarValue> {
            if source.index_side {
                let info = source
                    .info
                    .as_ref()
                    .ok_or_else(|| corrupt_query("an index-side aggregate input names no index"))?;
                let key = candidate
                    .scalar(info.id)
                    .ok_or_else(|| corrupt_query("an aggregate lost its scalar posting key"))?;
                return scalar_order_value(info, key);
            }
            ensure_row_seq(db, rows, candidate.id, row, encoded, meter)?;
            meter.note_row_decode();
            let value = selected_field(
                row.as_ref().expect("ensure_row_seq filled the row"),
                &source.field,
            )?;
            field_scalar(value)
        };

        let key = match &self.group {
            None => None,
            Some(group) => {
                let value = read(&group.source, row, encoded, rows, meter)?;
                Some(match (group.divisor, &value) {
                    (Some(divisor), OwnedScalarValue::I64(number)) => {
                        // Truncating division, which is what SQL's integer
                        // `/` does; the divisor is positive by construction.
                        OwnedScalarValue::I64(number / divisor)
                    }
                    (Some(_), OwnedScalarValue::Nullish) => OwnedScalarValue::Nullish,
                    (Some(_), _) => {
                        return Err(corrupt_query(
                            "a divided group key read a non-integer value",
                        ))
                    }
                    (None, _) => value,
                })
            }
        };

        inputs.clear();
        for accumulator in &self.accumulators {
            match &accumulator.source {
                None => inputs.push(None),
                Some(source) => inputs.push(Some(read(source, row, encoded, rows, meter)?)),
            }
        }
        Ok(key)
    }

    pub fn next_page<C: FnMut() -> bool>(
        &mut self,
        page_size: usize,
        budget: QueryBudget,
        mut cancelled: C,
    ) -> QueryResult<GroupPage> {
        if page_size == 0 || page_size > MAX_PAGE_SIZE {
            return Err(invalid_query("an aggregate page size requires 1..8192 groups"));
        }
        let mut budget = budget;
        budget.groups = budget.groups.min(self.groups_cap);
        let mut meter = WorkMeter::new(budget, &mut cancelled);
        meter.check_cancelled()?;
        self.query.ensure_membership_sets(&mut meter)?;

        let remaining = self
            .total_limit
            .map_or(usize::MAX, |limit| limit.saturating_sub(self.emitted));
        if remaining == 0 {
            return Ok(GroupPage {
                groups: Vec::new(),
                done: true,
                work: meter.used,
            });
        }
        let wanted = page_size.min(remaining);

        let (groups, done) = match self.shape {
            AggregateShape::Hashed => {
                if self.pending.is_none() {
                    // One fold, once. Nothing is committed until it has
                    // succeeded: a cancelled or budget-refused fold leaves
                    // this aggregate exactly as it found it, so a retry asks
                    // the identical question.
                    let folded = self.hashed_fold(&mut meter)?;
                    self.pending = Some(folded);
                }
                let pending = self.pending.as_mut().expect("the fold just filled it");
                let take = wanted.min(pending.len());
                let groups: Vec<GroupRow> = pending.drain(..take).collect();
                (groups, pending.is_empty())
            }
            AggregateShape::Streaming => {
                if self.done {
                    (Vec::new(), true)
                } else {
                    let (groups, done, after) = self.stream_page(wanted, &mut meter)?;
                    self.after = after;
                    self.done = done;
                    (groups, done)
                }
            }
        };

        self.emitted = self
            .emitted
            .checked_add(groups.len())
            .ok_or_else(|| invalid_query("aggregate group count overflow"))?;
        let hit_limit = self.total_limit.is_some_and(|limit| self.emitted >= limit);
        Ok(GroupPage {
            groups,
            done: done || hit_limit,
            work: meter.used,
        })
    }

    /// The whole candidate stream, folded into one group table.
    fn hashed_fold<C: FnMut() -> bool>(
        &mut self,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Vec<GroupRow>> {
        let db = self.query.db;
        let collection = self.query.collection;
        let graph = execute_graph_filters(db, &self.query.filters, false, meter)?;
        let needs = self.query.cursor_needs();
        let mut cursor = DriverCursor::new(
            db,
            collection,
            &self.query.driver,
            &graph,
            needs,
            None,
            false,
            None,
            HashSet::new(),
            &self.query.membership,
        )?;
        let mut rows = PrimaryRows::new(db, self.query.driver_walks_ids_ascending());
        let mut scratch = RowScratch::default();
        let mut inputs: Vec<Option<OwnedScalarValue>> = Vec::new();
        let mut table: BTreeMap<GroupOrdKey, Vec<Acc>> = BTreeMap::new();
        let mut ungrouped: Option<Vec<Acc>> = None;
        let mut opened = 0u64;
        // The groups budget is a byte bound: text keys and text extremes are
        // charged in units of one inline group beyond the group itself.
        let unit = group_bytes(self.accumulators.len()).max(1);
        let mut held_extra = 0usize;

        while let Some(mut candidate) = cursor.next(meter)? {
            meter.charge(WorkResource::Candidates, 1)?;
            if candidate.id.collection != collection {
                return Err(corrupt_query("an aggregate driver crossed collection boundary"));
            }
            let mut encoded = candidate.row.take();
            let mut row = None;
            if !self.query.filters.is_empty()
                && !filters_match(
                    db,
                    &mut rows,
                    &self.query.filters,
                    &self.query.membership,
                    &candidate,
                    &mut row,
                    &mut encoded,
                    &graph,
                    &mut scratch,
                    meter,
                )?
            {
                continue;
            }
            let key = self.inputs(
                db,
                &mut rows,
                &candidate,
                &mut row,
                &mut encoded,
                meter,
                &mut inputs,
            )?;
            let slot = match key {
                None => {
                    if ungrouped.is_none() {
                        meter.charge(WorkResource::Groups, 1)?;
                        opened += 1;
                        ungrouped = Some(
                            self.accumulators
                                .iter()
                                .map(|accumulator| Acc::new(accumulator.function))
                                .collect(),
                        );
                    }
                    ungrouped.as_mut().expect("just opened")
                }
                Some(key) => {
                    let key = GroupOrdKey(key);
                    if !table.contains_key(&key) {
                        // One charge per accumulator set that becomes live,
                        // plus one per `unit` bytes a text key holds on the
                        // heap. This is a MEMORY bound, so it counts sets
                        // held at once -- under this shape every distinct
                        // group.
                        let heap = key_heap_bytes(&key.0);
                        meter.charge(WorkResource::Groups, 1 + (heap / unit) as u64)?;
                        opened += 1;
                        table.insert(
                            key.clone(),
                            self.accumulators
                                .iter()
                                .map(|accumulator| Acc::new(accumulator.function))
                                .collect(),
                        );
                    }
                    table.get_mut(&key).expect("just inserted")
                }
            };
            for (accumulator, input) in slot.iter_mut().zip(inputs.iter()) {
                held_extra += accumulator.fold(input.as_ref())?;
            }
            while held_extra >= unit {
                meter.charge(WorkResource::Groups, 1)?;
                held_extra -= unit;
            }
        }

        self.groups_seen = self.groups_seen.saturating_add(opened);
        let mut out = Vec::with_capacity(table.len() + usize::from(ungrouped.is_some()));
        if let Some(accumulators) = ungrouped {
            if let Some(row) = self.finish_group(OpenGroup {
                key: None,
                accumulators,
                last: None,
            })? {
                out.push(row);
            }
        }
        for (key, accumulators) in table {
            if let Some(row) = self.finish_group(OpenGroup {
                key: Some(key.0),
                accumulators,
                last: None,
            })? {
                out.push(row);
            }
        }
        // THE ONE SORT OVER MEMORY (see `GroupOrder::Accumulator`). The map
        // already hands the groups back in ascending key order, so `Key`
        // sorts nothing; a ranking over an accumulator sorts the finished
        // table, which `WorkResource::Groups` has already bounded.
        if let GroupOrder::Accumulator { at, direction } = self.order {
            out.sort_by(|left, right| {
                let ordering = match (left.values[at].as_f64(), right.values[at].as_f64()) {
                    (Some(a), Some(b)) => a.total_cmp(&b),
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => Ordering::Equal,
                };
                let ordering = match direction {
                    SortDirection::Ascending => ordering,
                    SortDirection::Descending => ordering.reverse(),
                };
                // Ties break on the key, so the order is total and a page
                // boundary cannot move between two runs of the same request.
                ordering.then_with(|| match (&left.key, &right.key) {
                    (Some(a), Some(b)) => compare_scalar(a, b),
                    _ => Ordering::Equal,
                })
            });
        }
        Ok(out)
    }

    /// One page of a STREAMING aggregate: fold until `wanted` groups have
    /// closed, then stop at that group's boundary.
    fn stream_page<C: FnMut() -> bool>(
        &mut self,
        wanted: usize,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<(Vec<GroupRow>, bool, Option<RankKey>)> {
        let db = self.query.db;
        let collection = self.query.collection;
        let graph = execute_graph_filters(db, &self.query.filters, false, meter)?;
        let needs = self.query.cursor_needs();
        let resume = self.after.clone();
        let mut cursor = DriverCursor::new(
            db,
            collection,
            &self.query.driver,
            &graph,
            needs,
            resume.as_ref(),
            false,
            None,
            HashSet::new(),
            &self.query.membership,
        )?;
        let mut rows = PrimaryRows::new(db, self.query.driver_walks_ids_ascending());
        let mut scratch = RowScratch::default();
        let mut inputs: Vec<Option<OwnedScalarValue>> = Vec::new();
        let mut out: Vec<GroupRow> = Vec::with_capacity(wanted.min(64));
        let mut open: Option<OpenGroup> = None;
        let mut after = self.after.clone();
        // One accumulator set alive at a time, whatever the collection holds.
        let mut charged = false;
        let unit = group_bytes(self.accumulators.len()).max(1);
        let mut held_extra = 0usize;
        // Groups opened by THIS page, committed to `groups_seen` only when the
        // page returns Ok: a cancelled or refused page leaves the count alone.
        let mut opened = 0u64;

        while let Some(mut candidate) = cursor.next(meter)? {
            meter.charge(WorkResource::Candidates, 1)?;
            if candidate.id.collection != collection {
                return Err(corrupt_query("an aggregate driver crossed collection boundary"));
            }
            // The resumed candidate is handed over once more by design (see
            // `DriverCursor::new`), so a page that resumes drops everything
            // at or before the key it resumed from.
            let key = self.candidate_key(&candidate)?;
            if let (Some(key), Some(resume)) = (key.as_ref(), resume.as_ref()) {
                if compare_rank(key, resume, false) != Ordering::Greater {
                    continue;
                }
            }
            let mut encoded = candidate.row.take();
            let mut row = None;
            if !self.query.filters.is_empty()
                && !filters_match(
                    db,
                    &mut rows,
                    &self.query.filters,
                    &self.query.membership,
                    &candidate,
                    &mut row,
                    &mut encoded,
                    &graph,
                    &mut scratch,
                    meter,
                )?
            {
                continue;
            }
            let group_key = self.inputs(
                db,
                &mut rows,
                &candidate,
                &mut row,
                &mut encoded,
                meter,
                &mut inputs,
            )?;
            let boundary = match (&open, &group_key) {
                (None, _) => false,
                (Some(current), _) => match (&current.key, &group_key) {
                    (Some(a), Some(b)) => compare_scalar(a, b) != Ordering::Equal,
                    (None, None) => false,
                    _ => return Err(corrupt_query("an aggregate group key changed shape")),
                },
            };
            if boundary {
                let closed = open.take().expect("a boundary has an open group");
                let last = closed.last.clone();
                if let Some(row) = self.finish_group(closed)? {
                    out.push(row);
                    if out.len() >= wanted {
                        // The page is full at a group BOUNDARY: the next page
                        // resumes after the last candidate of the group just
                        // closed, so the candidate in hand -- the first of the
                        // next group -- is walked again and nothing is lost.
                        after = last;
                        self.groups_seen = self.groups_seen.saturating_add(opened);
                        return Ok((out, false, after));
                    }
                }
                after = last;
            }
            if open.is_none() {
                if !charged {
                    meter.charge(WorkResource::Groups, 1)?;
                    charged = true;
                }
                open = Some(self.open_group(group_key));
                opened += 1;
            }
            let current = open.as_mut().expect("an open group");
            for (accumulator, input) in current.accumulators.iter_mut().zip(inputs.iter()) {
                held_extra += accumulator.fold(input.as_ref())?;
            }
            while held_extra >= unit {
                meter.charge(WorkResource::Groups, 1)?;
                held_extra -= unit;
            }
            current.last = key;
        }

        if let Some(closed) = open.take() {
            if let Some(row) = self.finish_group(closed)? {
                out.push(row);
            }
        }
        self.groups_seen = self.groups_seen.saturating_add(opened);
        Ok((out, true, after))
    }

    /// The rank key of one candidate under the walk this aggregate drives:
    /// the scalar posting's `value || sequence`, which is what a STREAMING
    /// page resumes on. `None` where the walk has no key of its own, which is
    /// the ungrouped single-group fold -- it never resumes, because its one
    /// group closes only when the walk ends.
    fn candidate_key(&self, candidate: &Candidate) -> QueryResult<Option<RankKey>> {
        let Some(group) = &self.group else {
            return Ok(None);
        };
        let Some(info) = &group.source.info else {
            return Ok(None);
        };
        if !group.source.index_side {
            return Ok(None);
        }
        let key = candidate
            .scalar(info.id)
            .ok_or_else(|| corrupt_query("a streaming aggregate lost its scalar posting key"))?;
        Ok(Some(RankKey {
            value: RankValue::Scalar(key.to_vec()),
            id: candidate.id,
        }))
    }

    /// The compiled plan, in the planner's own terms, plus what this file
    /// added to it. `EXPLAIN` formats it and adds nothing.
    pub fn describe(&self) -> AggregatePlanDescription {
        AggregatePlanDescription {
            query: self.query.describe(),
            shape: self.shape,
            group: self.group.as_ref().map(|group| {
                let field = match group.divisor {
                    Some(divisor) => format!("{} / {divisor}", group.source.field),
                    None => group.source.field.clone(),
                };
                (field, group.source.where_from().to_owned())
            }),
            accumulators: self
                .accumulators
                .iter()
                .map(|accumulator| match &accumulator.source {
                    // `count(*)` names no column, so it is already written in
                    // full.
                    None => (
                        accumulator.function.written().to_owned(),
                        "nothing is read: count(*) counts candidates".to_owned(),
                    ),
                    Some(source) => (
                        format!("{}({})", accumulator.function.written(), source.field),
                        source.where_from().to_owned(),
                    ),
                })
                .collect(),
            having: self
                .having
                .iter()
                .map(|predicate| {
                    format!(
                        "accumulator {} {} {}",
                        predicate.accumulator,
                        predicate.op.written(),
                        predicate.value
                    )
                })
                .collect(),
            groups_seen: self.groups_seen,
            groups_cap: self.groups_cap,
            total_limit: self.total_limit,
        }
    }
}

/// The plan of a prepared aggregate: the query plan it reuses, plus the shape,
/// the group key, the accumulator sources and the groups it has seen.
#[derive(Clone, Debug, PartialEq)]
pub struct AggregatePlanDescription {
    pub query: QueryPlanDescription,
    pub shape: AggregateShape,
    /// The group key and where its value comes from.
    pub group: Option<(String, String)>,
    /// One `(accumulator, source)` pair per accumulator, in request order.
    pub accumulators: Vec<(String, String)>,
    pub having: Vec<String>,
    pub groups_seen: u64,
    /// The engine's own ceiling on live accumulator sets.
    pub groups_cap: u64,
    pub total_limit: Option<usize>,
}
