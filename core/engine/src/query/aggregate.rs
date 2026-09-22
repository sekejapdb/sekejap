//! The aggregation atomic (`docs/lang/QL_CONTRACT.md` §4.7): `count(*)`,
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
//! **POSTING JOIN.** The group key is the driving scalar index's own value
//! AND every accumulator that would otherwise read a row has its OWN scalar
//! index of a numeric kind. Then no row is read at all: pass 1 walks the
//! driving index and turns each value's contiguous run of postings into one
//! group with a bounded id BITMAP; pass 2 walks each accumulated column's
//! index in key order and folds every `(value, id)` into the group whose
//! bitmap claims the id. The cost is `rows x (1 + accumulated columns)`
//! posting steps and ZERO `primary_reads`, against one random row read per
//! candidate -- 50,000 point-gets into a 61 MB tree was the whole of
//! `agg_sum_born_by_kind`'s 115 ms.
//!
//! Its two bounds are not refusals. A collection whose groups x bitmap would
//! hold more than `MEMBERSHIP_BYTES_CAP`, or more accumulator sets than the
//! CALLER's `groups` budget allows, gives way to the fold that ran before
//! this shape existed -- STREAMING, or HASHED where the order is over an
//! accumulator. That is deliberate and it is `QL_CONTRACT` §6: a request
//! that answered inside one accumulator set yesterday must not become a
//! `BudgetExceeded` today because the engine learnt a new shape. `EXPLAIN`
//! prints the shape that RAN and the reason the join gave way.
//!
//! **SKIP-SCAN.** A group with no accumulators at all: see
//! [`PreparedAggregate::skip_page`].
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

/// The aggregate functions of `docs/lang/QL_CONTRACT.md` §4.7.
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

/// Where a `count(*)` with no filter and no group got its number.
///
/// A count over a whole collection is the one aggregate whose answer is
/// already written down: `collections/row_count.rs` keeps a live record per
/// collection, maintained by the write path inside the same transaction as
/// the rows. When the collection has one, the answer is ONE get -- no
/// candidate, no posting, no primary read. When it has not (a database
/// written before the feature, or a collection the backfill has not reached),
/// the walk is the one this engine always took, unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CountSource {
    /// The complete enumeration of the collection: the external-key mapping
    /// keyspace, one posting step per row.
    Walk,
    /// The live row-count record: one get.
    LiveRecord,
}

impl CountSource {
    pub fn written(self) -> &'static str {
        match self {
            Self::Walk => "walk",
            Self::LiveRecord => "live record",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregateShape {
    Streaming,
    Hashed,
    /// A group with NO accumulators over the DRIVING scalar index: a
    /// SKIP-SCAN. See `PreparedAggregate::skip_page`.
    Skip,
    /// Two index passes and no row: see `PreparedAggregate::posting_join_fold`.
    PostingJoin,
}

impl AggregateShape {
    pub fn written(self) -> &'static str {
        match self {
            Self::Streaming => "streaming",
            Self::Hashed => "hashed",
            Self::Skip => "skip-scan",
            Self::PostingJoin => "posting-join",
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

/// The compiled POSTING JOIN: which index each of the two passes walks, and
/// which accumulator each posting feeds.
#[derive(Clone, Debug)]
struct PostingJoin {
    /// The DRIVING scalar index. Pass 1 walks it whole, and its value IS the
    /// group key -- so one value's contiguous run of postings is one group.
    driving: IndexInfo,
    /// The accumulator positions pass 1 folds off the driving posting, with
    /// `true` where the accumulator takes that value as its input and `false`
    /// for `count(*)`, which takes none. Neither reads a row today either;
    /// they ride along so pass 2 walks only the columns that need it.
    driving_folds: Vec<(usize, bool)>,
    /// One entry per DISTINCT accumulated index, in request order: the index
    /// pass 2 walks, and every accumulator position that reads it. `sum`,
    /// `min`, `max` and `avg` over one column share ONE walk.
    columns: Vec<(IndexInfo, Vec<usize>)>,
    /// The fold that runs when a bound gives way: the shape this request had
    /// before the join existed. Never a refusal (`QL_CONTRACT` §6).
    fallback: AggregateShape,
}

/// Is this request a POSTING JOIN, and what does each pass walk?
///
/// Every condition is what makes the join SOUND or makes it a WIN, and a
/// request that fails one gets the shape it always got, with the same answer:
///
///   * NO FILTERS -- pass 1 is the driving index's own full forward walk, and
///     a filter would have to reject candidates the bitmaps have already
///     claimed (some of them by reading the row this shape exists to avoid);
///   * the group key is the DRIVING scalar index's own value, UNDIVIDED, so a
///     value is a group and the postings of one group are contiguous;
///   * the driver is that index's forward walk over the whole range;
///   * every accumulator either folds off the driving posting (`count(*)`, or
///     the driving column itself) or has its OWN scalar index of a NUMERIC
///     kind, with `sum` and `avg` narrowed to `Int`. The narrowing is the
///     ANSWER, not the plan: this shape folds a column in its INDEX's key
///     order where every other shape folds it in entity order, `f64` addition
///     is not associative, and an engine that answered `avg(price)` with a
///     different last bit depending on which fold it chose would be wrong in
///     a way no counter shows. Whole numbers are summed in `i128`, extremes
///     and `count` do not care what order they see, so everything the join
///     takes it answers BIT FOR BIT as the fold that reads the rows does;
///   * at least ONE accumulator would otherwise read a row. With none, the
///     streaming fold already costs zero `primary_reads` and holds ONE
///     accumulator set, so the join would spend a bitmap per group to buy
///     nothing.
fn posting_join_plan(
    driver: &DriverPlan,
    group: Option<&CompiledGroup>,
    accumulators: &[CompiledAccumulator],
    filters: &[QueryFilter<'_>],
    skip: bool,
    streaming_key: bool,
) -> Option<PostingJoin> {
    if skip || accumulators.is_empty() || !filters.is_empty() {
        return None;
    }
    let group = group?;
    if group.divisor.is_some() || !group.source.index_side {
        return None;
    }
    let driving = group.source.info.clone()?;
    match driver {
        DriverPlan::Scalar {
            info,
            predicate:
                EncodedScalarFilter::Range {
                    lower: EncodedBound::Unbounded,
                    upper: EncodedBound::Unbounded,
                },
            ..
        } if info.id == driving.id => {}
        _ => return None,
    }
    let mut driving_folds = Vec::new();
    let mut columns: Vec<(IndexInfo, Vec<usize>)> = Vec::new();
    for (at, accumulator) in accumulators.iter().enumerate() {
        match &accumulator.source {
            None => driving_folds.push((at, false)),
            Some(source) if source.index_side => driving_folds.push((at, true)),
            Some(source) => {
                let info = source.info.clone()?;
                let admitted = match accumulator.function {
                    AggregateFn::Sum | AggregateFn::Avg => info.kind == Kind::Int,
                    _ => matches!(info.kind, Kind::Int | Kind::Real),
                };
                if !admitted {
                    return None;
                }
                match columns.iter_mut().find(|(own, _)| own.id == info.id) {
                    Some((_, positions)) => positions.push(at),
                    None => columns.push((info, vec![at])),
                }
            }
        }
    }
    if columns.is_empty() {
        return None;
    }
    Some(PostingJoin {
        driving,
        driving_folds,
        columns,
        fallback: if streaming_key {
            AggregateShape::Streaming
        } else {
            AggregateShape::Hashed
        },
    })
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
    /// The same integer/float split [`Acc::Sum`] carries, and for the same
    /// reason ONE step further on: `f64` addition is not associative, so an
    /// average accumulated as a running `f64` depends on the ORDER its rows
    /// arrive in -- and the POSTING JOIN folds a column in its index's key
    /// order while every other shape folds it in entity order. Whole numbers
    /// are summed in `i128`, which has no such freedom, so the two shapes
    /// answer bit for bit alike.
    Avg {
        ints: i128,
        floats: f64,
        saw_float: bool,
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
            AggregateFn::Avg => Self::Avg {
                ints: 0,
                floats: 0.0,
                saw_float: false,
                n: 0,
            },
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
            Self::Avg {
                ints,
                floats,
                saw_float,
                n,
            } => {
                match value {
                    OwnedScalarValue::I64(v) => *ints += i128::from(*v),
                    OwnedScalarValue::F64(v) => {
                        *saw_float = true;
                        *floats += *v;
                    }
                    OwnedScalarValue::Bool(_) | OwnedScalarValue::Text(_) => {
                        return Err(invalid_query("avg requires a numeric input"))
                    }
                    OwnedScalarValue::Nullish => return Ok(0),
                }
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
            Self::Avg {
                ints,
                floats,
                saw_float,
                n,
            } => {
                if *n == 0 {
                    AggValue::Null
                } else {
                    let total = if *saw_float {
                        *ints as f64 + *floats
                    } else {
                        *ints as f64
                    };
                    AggValue::F64(total / *n as f64)
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

/// The least key strictly greater than every key that begins with `prefix`.
///
/// `None` when there is none: every byte is `0xFF`, so the prefix is the last
/// one byte order holds and there is nothing after it to seek to.
fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut out = prefix.to_vec();
    while let Some(last) = out.pop() {
        if last != u8::MAX {
            out.push(last + 1);
            return Some(out);
        }
    }
    None
}

/// Is this candidate STRICTLY after the key the page resumed from?
///
/// The walk ascends in `(value, sequence)`, which is exactly what
/// `compare_rank` orders a `RankValue::Scalar` by, so this is that comparison
/// without building the key: a resumed page drops everything at or before its
/// cursor, and the FIRST candidate past it ends the dropping, because nothing
/// behind it can come back.
fn past_resume(bytes: &[u8], id: EntityId, resume: Option<&RankKey>) -> bool {
    match resume {
        Some(RankKey {
            value: RankValue::Scalar(at),
            id: after,
        }) => match bytes.cmp(at.as_slice()) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => id > *after,
        },
        _ => true,
    }
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

        // ... and it is also the one case whose answer is already written
        // down. `collections/row_count.rs` keeps a live count per collection,
        // maintained by the write path in the same transaction as the rows,
        // so a collection that has a record answers this request with ONE
        // get: no candidate is walked, no posting is read and no row is
        // touched. A collection with no record takes the walk above,
        // unchanged. Anything with a filter or a group never reaches here.
        let live_count = if count_all {
            self.row_count(request.collection)?
        } else {
            None
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
        // A SKIP-SCAN, which is DISTINCT and every other group that
        // accumulates nothing. Four things have to hold, and each of them is
        // what makes skipping the postings between two values SOUND:
        //
        //   * NO ACCUMULATORS -- nothing is folded, so the postings between
        //     two values carry nothing the answer needs;
        //   * NO FILTERS -- a filter can reject every row of a value, and
        //     then the value is not a group; with one, the rows have to be
        //     seen and the ordinary streaming walk sees them;
        //   * the group key is the DRIVING scalar index's own value, undivided
        //     -- so a value IS a group and the keyspace's own order is the
        //     answer's;
        //   * the driver is that index's forward walk over the whole range,
        //     which is what a seek to a value's successor can re-open.
        //
        // Nothing is REFUSED by this: a request that fails any of the four
        // gets the shape it always got, with the same answer.
        let skip = request.accumulators.is_empty()
            && request.filters.is_empty()
            && matches!(request.order, GroupOrder::Key)
            && group.as_ref().is_some_and(|group| {
                group.divisor.is_none() && group.source.index_side
            })
            && match (&query.driver, &group) {
                (
                    DriverPlan::Scalar {
                        info,
                        predicate:
                            EncodedScalarFilter::Range {
                                lower: EncodedBound::Unbounded,
                                upper: EncodedBound::Unbounded,
                            },
                        ..
                    },
                    Some(group),
                ) => group.source.info.as_ref().is_some_and(|own| own.id == info.id),
                _ => false,
            };
        let posting = posting_join_plan(
            &query.driver,
            group.as_ref(),
            &accumulators,
            request.filters,
            skip,
            streaming_key,
        );
        let shape = if skip {
            AggregateShape::Skip
        } else if posting.is_some() {
            AggregateShape::PostingJoin
        } else if streaming_key {
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
            posting,
            fell_back: None,
            total_limit: request.total_limit,
            groups_cap,
            groups_seen: 0,
            emitted: 0,
            after: None,
            done: false,
            pending: None,
            count_all,
            live_count,
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
    /// The shape that RUNS. Chosen at prepare; the one thing that can change
    /// it afterwards is a POSTING JOIN giving way to its fallback, which is
    /// why `EXPLAIN` reads it after the walk and not before.
    shape: AggregateShape,
    /// The compiled posting join, kept even after a fallback so `EXPLAIN` can
    /// still name the two passes that were planned.
    posting: Option<PostingJoin>,
    /// Why the posting join gave way, if it did.
    fell_back: Option<String>,
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
    /// True when this request is `count(*)` over a whole collection: no
    /// filter, no group, and every accumulator a `count(*)`. It is the one
    /// request whose answer can be read rather than walked.
    count_all: bool,
    /// The live row-count record's number, read once at prepare. `Some` only
    /// when `count_all` holds AND the collection has a record.
    live_count: Option<u64>,
}

/// What one group is while it is being folded.
struct OpenGroup {
    key: Option<OwnedScalarValue>,
    accumulators: Vec<Acc>,
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
        }
    }

    /// ONE source of one candidate, read from the posting where the driving
    /// walk carries the value and from the row where it does not.
    ///
    /// `row` and `encoded` are the caller's, so the row a group key forced is
    /// the row every accumulator then reads: one read per candidate, whatever
    /// the request names.
    fn read_source<'a, C: FnMut() -> bool>(
        db: &'a Database,
        source: &CompiledSource,
        candidate: &Candidate,
        row: &mut Option<RowData>,
        encoded: &mut Option<Vec<u8>>,
        rows: &mut PrimaryRows<'a>,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<OwnedScalarValue> {
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
    }

    /// The GROUP KEY of one candidate. `None` when the request named no group.
    fn group_value<'a, C: FnMut() -> bool>(
        &self,
        db: &'a Database,
        rows: &mut PrimaryRows<'a>,
        candidate: &Candidate,
        row: &mut Option<RowData>,
        encoded: &mut Option<Vec<u8>>,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Option<OwnedScalarValue>> {
        let Some(group) = &self.group else {
            return Ok(None);
        };
        let value = Self::read_source(db, &group.source, candidate, row, encoded, rows, meter)?;
        Ok(Some(match (group.divisor, &value) {
            (Some(divisor), OwnedScalarValue::I64(number)) => {
                // Truncating division, which is what SQL's integer `/` does;
                // the divisor is positive by construction.
                OwnedScalarValue::I64(number / divisor)
            }
            (Some(_), OwnedScalarValue::Nullish) => OwnedScalarValue::Nullish,
            (Some(_), _) => {
                return Err(corrupt_query(
                    "a divided group key read a non-integer value",
                ))
            }
            (None, _) => value,
        }))
    }

    /// Every accumulator input of one candidate, in request order.
    #[allow(clippy::too_many_arguments)]
    fn accumulator_inputs<'a, C: FnMut() -> bool>(
        &self,
        db: &'a Database,
        rows: &mut PrimaryRows<'a>,
        candidate: &Candidate,
        row: &mut Option<RowData>,
        encoded: &mut Option<Vec<u8>>,
        meter: &mut WorkMeter<'_, C>,
        inputs: &mut Vec<Option<OwnedScalarValue>>,
    ) -> QueryResult<()> {
        inputs.clear();
        for accumulator in &self.accumulators {
            match &accumulator.source {
                None => inputs.push(None),
                Some(source) => inputs.push(Some(Self::read_source(
                    db, source, candidate, row, encoded, rows, meter,
                )?)),
            }
        }
        Ok(())
    }

    /// The driving scalar index whose POSTING carries the group key, when
    /// there is one. `Some` for a plain indexed group key under the streaming
    /// shape; `None` for no group, for a group read from the row, and for the
    /// DIVIDED expression key -- whose posting bytes are not the group key,
    /// because many values map to one group.
    fn carried_group_key(&self) -> Option<IndexId> {
        let group = self.group.as_ref()?;
        if !group.source.index_side || group.divisor.is_some() {
            return None;
        }
        group.source.info.as_ref().map(|info| info.id)
    }

    /// The driving scalar index whose posting this walk can RESUME on: the
    /// carried key, and the divided key too -- the posting bytes are the
    /// resume point even where they are not the group key.
    fn resume_index(&self) -> Option<IndexId> {
        let group = self.group.as_ref()?;
        if !group.source.index_side {
            return None;
        }
        group.source.info.as_ref().map(|info| info.id)
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

        // The live record, when there is one. The number was read at prepare,
        // so this page walks nothing: every counter it reports is zero, which
        // is the whole point of the record. It is one group, once, and then
        // the aggregate is done.
        if let Some(rows) = self.live_count {
            if self.done {
                return Ok(GroupPage {
                    groups: Vec::new(),
                    done: true,
                    work: meter.used,
                });
            }
            self.done = true;
            self.groups_seen = 1;
            self.emitted = self.emitted.saturating_add(1);
            return Ok(GroupPage {
                groups: vec![GroupRow {
                    key: None,
                    values: self
                        .accumulators
                        .iter()
                        .map(|_| AggValue::Count(rows))
                        .collect(),
                }],
                done: true,
                work: meter.used,
            });
        }

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

        // The POSTING JOIN runs before the dispatch below, because the one
        // thing it can do besides answer is HAND THE QUESTION BACK: past its
        // bounds the shape becomes the fold that ran before it existed, and
        // that fold is what this page then takes.
        if self.shape == AggregateShape::PostingJoin && self.pending.is_none() {
            match self.posting_join_fold(&mut meter)? {
                Some(folded) => self.pending = Some(folded),
                None => {
                    let fallback = self
                        .posting
                        .as_ref()
                        .map_or(AggregateShape::Hashed, |plan| plan.fallback);
                    self.fell_back = Some(format!(
                        "the posting join's id bitmaps would have held more than the {MEMBERSHIP_BYTES_CAP} bytes a page promises, or more accumulator sets than the caller's groups budget allows; the {} fold ran instead -- the fold this request had before the join existed, so a bound here is not a new refusal (QL_CONTRACT section 6)",
                        fallback.written()
                    ));
                    self.shape = fallback;
                }
            }
        }

        let (groups, done) = match self.shape {
            AggregateShape::Hashed | AggregateShape::PostingJoin => {
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
            AggregateShape::Streaming | AggregateShape::Skip => {
                if self.done {
                    (Vec::new(), true)
                } else {
                    let (groups, done, after) = if self.shape == AggregateShape::Skip {
                        self.skip_page(wanted, &mut meter)?
                    } else {
                        self.stream_page(wanted, &mut meter)?
                    };
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
            let kept = if self.query.filters.is_empty() {
                // `filters_match` over an empty slice can only say yes, and
                // saying it costs a nine-argument call per candidate.
                true
            } else {
                filters_match(
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
            };
            if !kept {
                continue;
            }
            let key = self.group_value(db, &mut rows, &candidate, &mut row, &mut encoded, meter)?;
            self.accumulator_inputs(
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
            })? {
                out.push(row);
            }
        }
        for (key, accumulators) in table {
            if let Some(row) = self.finish_group(OpenGroup {
                key: Some(key.0),
                accumulators,
            })? {
                out.push(row);
            }
        }
        self.sort_finished_groups(&mut out);
        Ok(out)
    }

    /// THE ONE SORT OVER MEMORY (see [`GroupOrder::Accumulator`]).
    ///
    /// Both whole-walk folds hand their groups back in ascending key order
    /// already -- the hashed table is a `BTreeMap`, and the posting join
    /// opens its groups in the driving index's own key order -- so `Key`
    /// sorts nothing. A ranking over an accumulator sorts the finished
    /// table, which [`WorkResource::Groups`] has already bounded. Shared by
    /// the two so they cannot order the same answer differently.
    fn sort_finished_groups(&self, out: &mut [GroupRow]) {
        let GroupOrder::Accumulator { at, direction } = self.order else {
            return;
        };
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


    /// THE WHOLE ANSWER, FROM POSTINGS ALONE: the POSTING JOIN.
    ///
    /// **Pass 1** walks the DRIVING index in key order. A scalar posting is
    /// `value || sequence`, so one value is a contiguous run and that run is
    /// a GROUP: the group opens at the first posting of a value and every
    /// posting of it sets one bit in that group's own bounded id BITMAP
    /// ([`MembershipSet::Bitmap`], `ceil(span / 8)` bytes, charged to
    /// [`WorkResource::MembershipBytes`] -- 6.25 KB per group at 50,000
    /// rows). `count(*)` and any accumulator over the driving value itself
    /// fold here, off the posting.
    ///
    /// **Pass 2** walks each accumulated column's index in key order and
    /// folds every `(value, id)` into the group whose bitmap claims the id.
    /// A NULL or MISSING field is filed under the one nullish key and is
    /// skipped, which is exactly SQL's rule for `sum`, `avg`, `min`, `max`
    /// and `count(col)`. The walk is ascending, so `min` is the first value
    /// a group is handed and `max` the last -- the same `Acc` fold, on the
    /// same values, as the row path.
    ///
    /// No row is read: `primary_reads == 0`, and `scalar_postings` is
    /// `rows x (1 + accumulated columns)`.
    ///
    /// `Ok(None)` is a BOUND, not a refusal: the groups would hold more than
    /// [`MEMBERSHIP_BYTES_CAP`] of bitmap, or more accumulator sets than the
    /// caller's `groups` budget allows. The caller then runs
    /// [`PostingJoin::fallback`], which is the fold this request had before
    /// the join existed. Nothing this pass charged for MEMORY is still held
    /// when that happens, so it is given back.
    fn posting_join_fold<C: FnMut() -> bool>(
        &mut self,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Option<Vec<GroupRow>>> {
        let db = self.query.db;
        let plan = self
            .posting
            .clone()
            .ok_or_else(|| corrupt_query("a posting join names no plan"))?;
        // One bit per sequence in `1..=span`, exactly as a membership set
        // sizes its bitmap: the collection's highest issued sequence bounds
        // every posting either pass can name.
        let span = db
            .collection_span(self.query.collection)
            .map_err(QueryError::from)?;
        let bitmap_bytes = membership_bitmap_bytes(span) as usize;
        let unit = group_bytes(self.accumulators.len()).max(1);

        struct JoinGroup {
            key: OwnedScalarValue,
            bits: Vec<u8>,
            accumulators: Vec<Acc>,
        }

        let mut groups: Vec<JoinGroup> = Vec::new();
        let mut held_extra = 0usize;
        // Charged as the pass goes, and given BACK if the pass gives way:
        // sets it no longer holds are not sets the fallback holds too.
        let mut charged_groups = 0u64;
        let mut live_bytes = 0usize;
        let mut gave_way = false;

        // ── pass 1: the driving index, one group per value ────────────────
        let prefix = scalar_prefix(plan.driving.id);
        let mut open_bytes: Vec<u8> = Vec::new();
        let mut have_open = false;
        if let Some(mut walk) = db
            .index_range(&plan.driving, &prefix)
            .map_err(QueryError::from)?
        {
            'pass_one: loop {
                meter.check_cancelled()?;
                meter.charge(WorkResource::ScalarPostings, 1)?;
                let step = {
                    let Some((key, value)) = walk
                        .peek_ref()
                        .map_err(Error::from)
                        .map_err(QueryError::from)?
                    else {
                        break 'pass_one;
                    };
                    if !key.starts_with(&prefix) {
                        break 'pass_one;
                    }
                    let suffix = &key[prefix.len()..];
                    let width = scalar_key::width(&plan.driving.kind, suffix)?;
                    let encoded = suffix
                        .get(..width)
                        .ok_or_else(|| corrupt_query("truncated scalar value key"))?;
                    let mut at = prefix.len() + width;
                    let sequence = read_ordered(key, &mut at)?;
                    if at != key.len() || sequence == 0 || !value.is_empty() {
                        return Err(corrupt_query("scalar index entry"));
                    }
                    (encoded.to_vec(), sequence)
                };
                walk.step();
                let (encoded, sequence) = step;
                meter.charge(WorkResource::Candidates, 1)?;
                if !have_open || open_bytes != encoded {
                    // A posting range is `value || sequence`, so one value is
                    // a contiguous run: a value left behind never reopens.
                    let key = scalar_order_value(&plan.driving, &encoded)?;
                    let want = live_bytes.saturating_add(bitmap_bytes);
                    if want > MEMBERSHIP_BYTES_CAP {
                        gave_way = true;
                        break 'pass_one;
                    }
                    // The same charge per live accumulator set the hashed
                    // fold makes, in the same units.
                    let charge = 1 + (key_heap_bytes(&key) / unit) as u64;
                    if meter.would_exceed(WorkResource::Groups, charge) {
                        gave_way = true;
                        break 'pass_one;
                    }
                    meter.charge(WorkResource::Groups, charge)?;
                    charged_groups += charge;
                    live_bytes = want;
                    open_bytes.clear();
                    open_bytes.extend_from_slice(&encoded);
                    have_open = true;
                    groups.push(JoinGroup {
                        key,
                        bits: vec![0u8; bitmap_bytes],
                        accumulators: self
                            .accumulators
                            .iter()
                            .map(|accumulator| Acc::new(accumulator.function))
                            .collect(),
                    });
                }
                let JoinGroup {
                    key,
                    bits,
                    accumulators,
                } = groups.last_mut().expect("a posting opened its group");
                membership_bitmap_set(bits, sequence)?;
                for &(at, takes_value) in &plan.driving_folds {
                    let input = if takes_value { Some(&*key) } else { None };
                    held_extra += accumulators[at].fold(input)?;
                }
                while held_extra >= unit {
                    meter.charge(WorkResource::Groups, 1)?;
                    charged_groups += 1;
                    held_extra -= unit;
                }
            }
        }
        if gave_way {
            // Nothing is committed: the bitmaps are dropped here, the group
            // charge is given back, and the fallback fold starts from the
            // same place a first page always starts from.
            meter.release(WorkResource::Groups, charged_groups);
            return Ok(None);
        }
        meter.note_membership_bytes(live_bytes as u64);

        // The bitmaps, in the engine's own membership representation, so the
        // probe in pass 2 is the same `contains` a boolean filter uses. The
        // bits are MOVED, not copied: `live_bytes` stays the truth.
        let sets: Vec<MembershipSet> = groups
            .iter_mut()
            .map(|group| MembershipSet::Bitmap(Arc::new(std::mem::take(&mut group.bits))))
            .collect();

        // ── pass 2: each accumulated column, folded into the claiming group ─
        for (info, positions) in &plan.columns {
            let prefix = scalar_prefix(info.id);
            let Some(mut walk) = db.index_range(info, &prefix).map_err(QueryError::from)? else {
                continue;
            };
            loop {
                meter.check_cancelled()?;
                meter.charge(WorkResource::ScalarPostings, 1)?;
                let step = {
                    let Some((key, value)) = walk
                        .peek_ref()
                        .map_err(Error::from)
                        .map_err(QueryError::from)?
                    else {
                        break;
                    };
                    if !key.starts_with(&prefix) {
                        break;
                    }
                    let suffix = &key[prefix.len()..];
                    let width = scalar_key::width(&info.kind, suffix)?;
                    let encoded = suffix
                        .get(..width)
                        .ok_or_else(|| corrupt_query("truncated scalar value key"))?;
                    let mut at = prefix.len() + width;
                    let sequence = read_ordered(key, &mut at)?;
                    if at != key.len() || sequence == 0 || !value.is_empty() {
                        return Err(corrupt_query("scalar index entry"));
                    }
                    (encoded.to_vec(), sequence)
                };
                walk.step();
                let (encoded, sequence) = step;
                // A NULL and a MISSING field share the one nullish key, and
                // neither counts for `sum`, `avg`, `min`, `max` or
                // `count(col)`: SQL's own rule, and the same one `Acc::fold`
                // applies to a nullish value read off a row.
                if encoded == NULLISH_SCALAR_KEY {
                    continue;
                }
                let mut claimed = None;
                for (at, set) in sets.iter().enumerate() {
                    if set.contains(sequence)? {
                        claimed = Some(at);
                        break;
                    }
                }
                // A posting no group claims names a row this walk never saw:
                // nothing to fold it into, and nothing the answer loses.
                let Some(at_group) = claimed else {
                    continue;
                };
                let value = scalar_order_value(info, &encoded)?;
                let accumulators = &mut groups[at_group].accumulators;
                for &at in positions {
                    held_extra += accumulators[at].fold(Some(&value))?;
                }
                while held_extra >= unit {
                    meter.charge(WorkResource::Groups, 1)?;
                    held_extra -= unit;
                }
            }
        }

        self.groups_seen = self.groups_seen.saturating_add(groups.len() as u64);
        let mut out = Vec::with_capacity(groups.len());
        for group in groups {
            if let Some(row) = self.finish_group(OpenGroup {
                key: Some(group.key),
                accumulators: group.accumulators,
            })? {
                out.push(row);
            }
        }
        self.sort_finished_groups(&mut out);
        Ok(Some(out))
    }

    /// One page of a SKIP-SCAN: a group with NO accumulators over the driving
    /// scalar index.
    ///
    /// Nothing but the EXISTENCE of each value matters, so there is nothing
    /// to fold: the walk reads the FIRST posting of a value, emits that value
    /// as a group, and then seeks to the successor of that value's key prefix
    /// -- one root-to-leaf descent per DISTINCT VALUE, never a step over the
    /// postings in between. `QueryWork::scalar_postings` therefore counts
    /// distinct values, not rows, which is Law 2 for this shape: the work is
    /// proportional to the ANSWER.
    ///
    /// A value whose only posting names a deleted row is a group here, which
    /// is what the streaming walk does with the same posting: neither reads a
    /// row, so neither can tell.
    fn skip_page<C: FnMut() -> bool>(
        &mut self,
        wanted: usize,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<(Vec<GroupRow>, bool, Option<RankKey>)> {
        let db = self.query.db;
        let info = self
            .group
            .as_ref()
            .and_then(|group| group.source.info.clone())
            .ok_or_else(|| corrupt_query("a skip-scan names no scalar index"))?;
        let prefix = scalar_prefix(info.id);
        // Where this page opens: the start of the index, or the successor of
        // the value the last page ended on.
        let mut start = match &self.after {
            Some(RankKey {
                value: RankValue::Scalar(value),
                ..
            }) => {
                let mut bound = prefix.clone();
                bound.extend_from_slice(value);
                match prefix_successor(&bound) {
                    Some(next) => next,
                    None => return Ok((Vec::new(), true, self.after.clone())),
                }
            }
            Some(_) => return Err(corrupt_query("a skip-scan cursor is not a scalar key")),
            None => prefix.clone(),
        };
        let mut out: Vec<GroupRow> = Vec::with_capacity(wanted.min(64));
        let mut after = self.after.clone();
        let mut opened = 0u64;
        let mut charged = false;
        let unit = group_bytes(0).max(1);
        // The walk is finished only when it RAN OFF the end of the index; a
        // page that filled has more to give.
        let mut done = false;
        while out.len() < wanted {
            meter.check_cancelled()?;
            meter.charge(WorkResource::ScalarPostings, 1)?;
            let Some(mut walk) = db.index_range(&info, &start).map_err(QueryError::from)? else {
                done = true;
                break;
            };
            let value = {
                let Some((key, value)) = walk
                    .peek_ref()
                    .map_err(Error::from)
                    .map_err(QueryError::from)?
                else {
                    // Nothing at or after the seek: the index is finished.
                    done = true;
                    break;
                };
                if !key.starts_with(&prefix) {
                    // Past this index's own keyspace.
                    done = true;
                    break;
                }
                let suffix = &key[prefix.len()..];
                let width = scalar_key::width(&info.kind, suffix)?;
                let encoded = suffix
                    .get(..width)
                    .ok_or_else(|| corrupt_query("truncated scalar value key"))?;
                let mut at = prefix.len() + width;
                let sequence = read_ordered(key, &mut at)?;
                if at != key.len() || sequence == 0 || !value.is_empty() {
                    return Err(corrupt_query("scalar index entry"));
                }
                encoded.to_vec()
            };
            drop(walk);
            // One candidate: the one posting this value was proved by.
            meter.charge(WorkResource::Candidates, 1)?;
            if !charged {
                meter.charge(WorkResource::Groups, 1)?;
                charged = true;
            }
            let key = scalar_order_value(&info, &value)?;
            let heap = key_heap_bytes(&key);
            if heap >= unit {
                meter.charge(WorkResource::Groups, (heap / unit) as u64)?;
            }
            opened += 1;
            out.push(GroupRow {
                key: Some(key),
                values: Vec::new(),
            });
            after = Some(RankKey {
                value: RankValue::Scalar(value.clone()),
                // The resume is a VALUE, not a row: every posting of this
                // value is behind the page, so the sequence half of the key
                // is the last one there can be.
                id: EntityId {
                    collection: info.collection,
                    sequence: u64::MAX,
                },
            });
            let mut bound = prefix.clone();
            bound.extend_from_slice(&value);
            match prefix_successor(&bound) {
                Some(next) => start = next,
                // The value was the last key byte order holds: there is no
                // successor to seek to, so the index is finished.
                None => {
                    done = true;
                    break;
                }
            }
        }
        self.groups_seen = self.groups_seen.saturating_add(opened);
        Ok((out, done, after))
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
        // The group key as the DRIVING POSTING'S OWN BYTES. Where the walk
        // carries the key, a boundary is a byte comparison against the open
        // group's key and the value is decoded ONCE per group -- not a
        // `String` per candidate, which on a Text group key was one
        // allocation per row for a key eight of them shared.
        let carried = self.carried_group_key();
        let group_info = self
            .group
            .as_ref()
            .and_then(|group| group.source.info.clone());
        // WHERE THE ROWS OF ONE GROUP LIE. A scalar posting is
        // `value || sequence`, so the candidates of one carried group arrive
        // in STRICTLY ASCENDING entity id: that run is exactly what the
        // lockstep reader is for, and the old walk told the reader the
        // opposite (`driver_walks_ids_ascending` is a question about the WHOLE
        // walk, and across a group boundary the sequences reset), so every row
        // of every group paid a fresh root-to-leaf descent. The reader is
        // restarted at each boundary, which is where the run restarts.
        let ascends = carried.is_some() || self.query.driver_walks_ids_ascending();
        let mut rows = PrimaryRows::new(db, ascends);
        // The index whose posting bytes a page RESUMES on. The divided
        // expression key resumes here too: many values map to one group, but
        // the posting is still where the walk left off.
        let resume_index = self.resume_index();
        let mut open_bytes: Option<Vec<u8>> = None;
        // The last folded candidate's posting key, in a buffer this page
        // reuses: what a closing group's resume point is built from.
        let mut last_bytes: Vec<u8> = Vec::new();
        let mut last_seq = 0u64;
        let mut have_last = false;

        // The resumed candidate is handed over once more by design (see
        // `DriverCursor::new`), so a page that resumes drops everything at or
        // before the key it resumed from. The walk ascends, so the FIRST
        // candidate past that key ends the skipping: nothing behind it can
        // come back.
        let mut skipping = resume.is_some();

        while let Some(mut candidate) = cursor.next(meter)? {
            meter.charge(WorkResource::Candidates, 1)?;
            if candidate.id.collection != collection {
                return Err(corrupt_query("an aggregate driver crossed collection boundary"));
            }
            let mut encoded = candidate.row.take();
            let mut row = None;
            let posting: Option<&[u8]> = match resume_index {
                Some(index) => Some(candidate.scalar(index).ok_or_else(|| {
                    corrupt_query("a streaming aggregate lost its scalar posting key")
                })?),
                None => None,
            };
            if skipping {
                match posting {
                    Some(bytes) if !past_resume(bytes, candidate.id, resume.as_ref()) => continue,
                    _ => skipping = false,
                }
            }
            let kept = if self.query.filters.is_empty() {
                true
            } else {
                filters_match(
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
            };
            if !kept {
                continue;
            }
            // The decoded group key, where the posting's bytes are NOT it:
            // the divided expression, and the single group over everything.
            let decoded = match carried {
                Some(_) => None,
                None => Some(self.group_value(
                    db,
                    &mut rows,
                    &candidate,
                    &mut row,
                    &mut encoded,
                    meter,
                )?),
            };
            let boundary = match (&open, carried) {
                (None, _) => false,
                (Some(_), Some(_)) => open_bytes.as_deref() != posting,
                (Some(current), None) => {
                    match (&current.key, decoded.as_ref().expect("decoded where not carried")) {
                        (Some(a), Some(b)) => compare_scalar(a, b) != Ordering::Equal,
                        (None, None) => false,
                        _ => return Err(corrupt_query("an aggregate group key changed shape")),
                    }
                }
            };
            if boundary {
                let last = if have_last {
                    Some(RankKey {
                        value: RankValue::Scalar(last_bytes.clone()),
                        id: EntityId {
                            collection,
                            sequence: last_seq,
                        },
                    })
                } else {
                    None
                };
                let closed = open.take().expect("a boundary has an open group");
                open_bytes = None;
                if ascends {
                    // A new group is a new ascending run, from a sequence
                    // BEHIND the one the reader is parked on.
                    rows.restart(true);
                }
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
                let key = match carried {
                    Some(_) => {
                        let bytes = posting.expect("a carried key is a posting");
                        open_bytes = Some(bytes.to_vec());
                        let info = group_info
                            .as_ref()
                            .ok_or_else(|| corrupt_query("a carried group key names no index"))?;
                        Some(scalar_order_value(info, bytes)?)
                    }
                    None => decoded.clone().expect("decoded where not carried"),
                };
                open = Some(self.open_group(key));
                opened += 1;
            }
            let current = open.as_mut().expect("an open group");
            self.accumulator_inputs(
                db,
                &mut rows,
                &candidate,
                &mut row,
                &mut encoded,
                meter,
                &mut inputs,
            )?;
            for (accumulator, input) in current.accumulators.iter_mut().zip(inputs.iter()) {
                held_extra += accumulator.fold(input.as_ref())?;
            }
            while held_extra >= unit {
                meter.charge(WorkResource::Groups, 1)?;
                held_extra -= unit;
            }
            if let Some(bytes) = posting {
                last_bytes.clear();
                last_bytes.extend_from_slice(bytes);
                last_seq = candidate.id.sequence;
                have_last = true;
            }
        }

        if let Some(closed) = open.take() {
            if let Some(row) = self.finish_group(closed)? {
                out.push(row);
            }
        }
        self.groups_seen = self.groups_seen.saturating_add(opened);
        Ok((out, true, after))
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
                .enumerate()
                .map(|(at, accumulator)| match &accumulator.source {
                    // `count(*)` names no column, so it is already written in
                    // full.
                    None => (
                        accumulator.function.written().to_owned(),
                        "nothing is read: count(*) counts candidates".to_owned(),
                    ),
                    Some(source) => (
                        format!("{}({})", accumulator.function.written(), source.field),
                        self.where_accumulator_reads(at, source),
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
            count: self.count_all.then(|| {
                if self.live_count.is_some() {
                    CountSource::LiveRecord
                } else {
                    CountSource::Walk
                }
            }),
            groups_seen: self.groups_seen,
            groups_cap: self.groups_cap,
            total_limit: self.total_limit,
            passes: self.posting_passes(),
            fell_back: self.fell_back.clone(),
        }
    }

    /// Where one accumulator ACTUALLY read its input, for `EXPLAIN`.
    ///
    /// [`CompiledSource::where_from`] knows only whether the DRIVING walk
    /// carried the value, and under the posting join that is not the whole
    /// truth: a column with its own index is read from that index's postings
    /// in pass 2, and no row is read for it either. The shape is the shape
    /// that RAN, so a join that gave way prints the row it then read.
    fn where_accumulator_reads(&self, at: usize, source: &CompiledSource) -> String {
        if self.shape == AggregateShape::PostingJoin {
            if let Some(plan) = self.posting.as_ref() {
                if plan
                    .columns
                    .iter()
                    .any(|(_, positions)| positions.contains(&at))
                {
                    return "index posting: its own scalar index, walked in pass 2".to_owned();
                }
            }
        }
        source.where_from().to_owned()
    }

    /// The two index passes of a POSTING JOIN, written out. Empty for every
    /// other shape, and kept after a fallback so `EXPLAIN` can still say what
    /// the join would have walked.
    fn posting_passes(&self) -> Vec<String> {
        let Some(plan) = self.posting.as_ref() else {
            return Vec::new();
        };
        let folded = |positions: &[usize]| -> String {
            positions
                .iter()
                .map(|at| match &self.accumulators[*at].source {
                    None => self.accumulators[*at].function.written().to_owned(),
                    Some(source) => format!(
                        "{}({})",
                        self.accumulators[*at].function.written(),
                        source.field
                    ),
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut out = vec![format!(
            "pass 1: walk the {} index in key order -- one GROUP per value, one bounded id bitmap per group (charged to membership_bytes){}",
            plan.driving.field,
            if plan.driving_folds.is_empty() {
                String::new()
            } else {
                format!(
                    "; folds {} off the posting",
                    folded(
                        &plan
                            .driving_folds
                            .iter()
                            .map(|(at, _)| *at)
                            .collect::<Vec<_>>()
                    )
                )
            }
        )];
        for (info, positions) in &plan.columns {
            out.push(format!(
                "pass 2: walk the {} index in key order -- each (value, id) folds into the group whose bitmap claims the id; folds {}; the nullish key counts for nothing",
                info.field,
                folded(positions)
            ));
        }
        out
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
    /// Where a whole-collection `count(*)` got its number. `None` for every
    /// other aggregate, which is unchanged by the live record.
    pub count: Option<CountSource>,
    pub groups_seen: u64,
    /// The engine's own ceiling on live accumulator sets.
    pub groups_cap: u64,
    pub total_limit: Option<usize>,
    /// The POSTING JOIN's two index passes, one line each. Empty for every
    /// other shape.
    pub passes: Vec<String>,
    /// Why the POSTING JOIN gave way to another fold, if it did. `shape` is
    /// then the fold that RAN, not the one prepare chose.
    pub fell_back: Option<String>,
}
