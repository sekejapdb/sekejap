use super::*;

// ── the compiled plan ─────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Scalar {
    Bool(bool),
    I64(i64),
    F64(f64),
    Text(String),
}

/// How many EDGES one semi-join over an edge type may walk before it is
/// refused. The set it produces is held in memory for the length of the
/// statement, so the walk is bounded by the same kind of stated ceiling
/// every other held set has -- named, not spilled.
pub(super) const MAX_SEMI_JOIN_EDGES: usize = 64 << 20;

/// How many OUTER IDS one semi-join may name. Eight bytes each, so this is
/// the 8 MiB `RUN_BYTES` promise every other per-query buffer is written
/// against.
pub(super) const MAX_SEMI_JOIN_IDS: usize = (8 << 20) / 8;

impl Scalar {
    fn borrowed(&self) -> ScalarValue<'_> {
        match self {
            Self::Bool(b) => ScalarValue::Bool(*b),
            Self::I64(i) => ScalarValue::I64(*i),
            Self::F64(f) => ScalarValue::F64(*f),
            Self::Text(t) => ScalarValue::Text(t),
        }
    }
}

fn borrow_bound(bound: &Bound<Scalar>) -> Bound<ScalarValue<'_>> {
    match bound {
        Bound::Included(v) => Bound::Included(v.borrowed()),
        Bound::Excluded(v) => Bound::Excluded(v.borrowed()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

fn borrow_key_bound(bound: &Bound<String>) -> Bound<&str> {
    match bound {
        Bound::Included(v) => Bound::Included(v.as_str()),
        Bound::Excluded(v) => Bound::Excluded(v.as_str()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum OwnedScalarFilter {
    Eq(Scalar),
    Range {
        lower: Bound<Scalar>,
        upper: Bound<Scalar>,
    },
    IsNull,
    IsMissing,
}

/// One compiled `SET column = ...`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CompiledSet {
    /// A constant, already checked against the column's declared `Kind`.
    Lit(Value),
    /// A §4.1 / §4.2 ROW EXPRESSION over the same row. It reaches the engine
    /// as the `UpdatePatch` closure: core calls it once per candidate with
    /// that row's document and stores what it returns.
    Row { expr: CompiledRow, kind: Kind },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum OwnedFilter {
    Scalar {
        index: IndexId,
        predicate: OwnedScalarFilter,
        /// The `$n` slots this predicate's value positions came from, empty
        /// when every value was written as a constant.
        fills: Vec<ScalarFill>,
    },
    Point {
        index: IndexId,
        predicate: PointFilter,
        fill: Option<PointFill>,
    },
    Geometry {
        index: IndexId,
        predicate: GeometryFilter,
        fill: Option<GeomFill>,
    },
    Text {
        index: IndexId,
        query: String,
        matching: TextMatch,
        /// The tsquery source, when it is a `$n`. Rebinding re-reads it and
        /// re-derives BOTH the terms and the `TextMatch`, because `a & b`
        /// and `a | b` are different matches of the same slot.
        fill: Option<TsQuery>,
    },
    Key {
        lower: Bound<String>,
        upper: Bound<String>,
        fills: Vec<KeyFill>,
    },
    /// A disjunction: one membership set, the union of its leaves'
    /// (`docs/lang/QL_CONTRACT.md` §3).
    Any(Vec<OwnedFilter>),
    /// A complement: `<>`, `NOT`, `IS NOT NULL`, `NOT EXISTS`.
    Not(Box<OwnedFilter>),
    /// A conjunction that could not be flattened into the top-level filter
    /// list: one inside a disjunction, or one under a complement.
    All(Vec<OwnedFilter>),
    /// The ids a semi-join produced, ascending and without duplicates. The
    /// subquery ran while the statement was compiled, so what the prepared
    /// query sees is a set and not a second plan.
    Ids(Vec<EntityId>),
    /// `[NOT] LIKE | ILIKE`, checked on each row the driver reaches.
    Like {
        field: String,
        pattern: String,
        escape: Option<char>,
        insensitive: bool,
        negated: bool,
    },
    RowCompare {
        fields: Vec<String>,
        values: Vec<Value>,
        op: sekejap_core::collections::Cmp,
    },
}

impl OwnedFilter {
    fn borrowed(&self) -> QueryFilter<'_> {
        match self {
            Self::Like {
                field,
                pattern,
                escape,
                insensitive,
                negated,
            } => QueryFilter::Like {
                field,
                pattern,
                escape: *escape,
                insensitive: *insensitive,
                negated: *negated,
            },
            Self::RowCompare { fields, values, op } => QueryFilter::RowCompare {
                fields,
                values,
                op: *op,
            },
            Self::Scalar {
                index, predicate, ..
            } => QueryFilter::Scalar {
                index: *index,
                predicate: match predicate {
                    OwnedScalarFilter::Eq(v) => ScalarFilter::Eq(v.borrowed()),
                    OwnedScalarFilter::Range { lower, upper } => ScalarFilter::Range {
                        lower: borrow_bound(lower),
                        upper: borrow_bound(upper),
                    },
                    OwnedScalarFilter::IsNull => ScalarFilter::IsNull,
                    OwnedScalarFilter::IsMissing => ScalarFilter::IsMissing,
                },
            },
            Self::Point {
                index, predicate, ..
            } => QueryFilter::Point {
                index: *index,
                predicate: *predicate,
            },
            Self::Geometry {
                index, predicate, ..
            } => QueryFilter::Geometry {
                index: *index,
                predicate: predicate.clone(),
            },
            Self::Text {
                index,
                query,
                matching,
                ..
            } => QueryFilter::Text {
                index: *index,
                query,
                matching: *matching,
            },
            Self::Key { lower, upper, .. } => QueryFilter::Key {
                lower: borrow_key_bound(lower),
                upper: borrow_key_bound(upper),
            },
            // A `QueryFilter::Any` holds a SLICE of borrowed filters and a
            // `Not` a reference to one, so neither can be returned from
            // here: the nodes live in stack frames that enclose the frame
            // the prepared query runs in. `with_boolean` builds them there,
            // the same shape `with_score` builds a `ScoreExpr` tree with.
            Self::Ids(ids) => QueryFilter::Ids(ids),
            Self::Any(_) | Self::All(_) | Self::Not(_) => {
                unreachable!("a boolean filter is borrowed through `with_boolean`")
            }
        }
    }
}

/// One borrowed boolean node and the ones built before it in the same list,
/// each living in its own stack frame.
///
/// The chain is what lets a list of children be collected into the contiguous
/// slice `QueryFilter::Any` needs: every node is still alive in an enclosing
/// frame when the innermost one runs, so the references are all valid there
/// at once.
struct BuiltFilter<'a> {
    node: &'a QueryFilter<'a>,
    previous: Option<&'a BuiltFilter<'a>>,
}

/// One finished boolean tree and the filter position it belongs to, chained
/// through the frames the way [`BuiltFilter`] chains a disjunction's children.
struct BuiltAt<'a> {
    at: usize,
    node: &'a QueryFilter<'a>,
    previous: Option<&'a BuiltAt<'a>>,
}

/// Build the whole borrowed filter list -- boolean trees and plain leaves
/// alike -- on this call's stack and hand it to `k`.
///
/// One entry point for `with_query`, `with_aggregate` and a WRITE
/// statement's `WHERE`, because a clause is the same clause whichever of the
/// three reads it. The borrowed view lives for the length of one
/// `Database::write_where` call or one prepared query: a `QueryFilter::Any`
/// names a SLICE and a `Not` a reference, so neither can be returned from a
/// compiled plan that outlives the statement.
pub(super) fn with_write_filters<T>(
    owned: &[OwnedFilter],
    k: &mut dyn FnMut(&[QueryFilter<'_>]) -> SqlResult2<T>,
) -> SqlResult2<T> {
    with_borrowed_filters(owned, k)
}

pub(super) fn with_borrowed_filters<T>(
    owned: &[OwnedFilter],
    k: &mut dyn FnMut(&[QueryFilter<'_>]) -> SqlResult2<T>,
) -> SqlResult2<T> {
    let boolean: Vec<usize> = owned
        .iter()
        .enumerate()
        .filter(|(_, filter)| matches!(filter, OwnedFilter::Any(_) | OwnedFilter::All(_) | OwnedFilter::Not(_)))
        .map(|(at, _)| at)
        .collect();
    with_boolean_filters(owned, &boolean, None, k)
}

fn with_boolean_filters<T>(
    owned: &[OwnedFilter],
    remaining: &[usize],
    built: Option<&BuiltAt<'_>>,
    k: &mut dyn FnMut(&[QueryFilter<'_>]) -> SqlResult2<T>,
) -> SqlResult2<T> {
    match remaining.split_first() {
        None => {
            let filters: Vec<QueryFilter<'_>> = owned
                .iter()
                .enumerate()
                .map(|(at, filter)| match filter {
                    OwnedFilter::Any(_) | OwnedFilter::All(_) | OwnedFilter::Not(_) => {
                        let mut link = built;
                        loop {
                            match link {
                                Some(entry) if entry.at == at => break entry.node.clone(),
                                Some(entry) => link = entry.previous,
                                // Unreachable: a frame was built above for
                                // every boolean position in this list.
                                None => break QueryFilter::Ids(&[]),
                            }
                        }
                    }
                    other => other.borrowed(),
                })
                .collect();
            k(&filters)
        }
        Some((at, rest)) => {
            let at = *at;
            with_boolean(&owned[at], &mut |node| {
                let link = BuiltAt {
                    at,
                    node,
                    previous: built,
                };
                with_boolean_filters(owned, rest, Some(&link), k)
            })
        }
    }
}

/// Build the borrowed `QueryFilter` tree on the stack and call `k` with it.
fn with_boolean<R>(node: &OwnedFilter, k: &mut dyn FnMut(&QueryFilter<'_>) -> R) -> R {
    match node {
        OwnedFilter::Not(inner) => {
            with_boolean(inner, &mut |child| k(&QueryFilter::Not(child)))
        }
        OwnedFilter::Any(children) => {
            with_children(children, None, &mut |built| k(&QueryFilter::Any(built)))
        }
        OwnedFilter::All(children) => {
            with_children(children, None, &mut |built| k(&QueryFilter::All(built)))
        }
        other => k(&other.borrowed()),
    }
}

/// Build every child of one disjunction, then hand them over as one slice.
fn with_children<R>(
    nodes: &[OwnedFilter],
    previous: Option<&BuiltFilter<'_>>,
    k: &mut dyn FnMut(&[QueryFilter<'_>]) -> R,
) -> R {
    match nodes.split_first() {
        None => {
            let mut out = Vec::new();
            let mut link = previous;
            while let Some(built) = link {
                out.push(built.node.clone());
                link = built.previous;
            }
            out.reverse();
            k(&out)
        }
        Some((head, rest)) => with_boolean(head, &mut |node| {
            let built = BuiltFilter { node, previous };
            with_children(rest, Some(&built), k)
        }),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum OwnedScore {
    Lit(f64),
    Scalar {
        index: IndexId,
    },
    /// A numeric column no scalar index holds, read from the row.
    Field {
        field: String,
    },
    Bm25 {
        index: IndexId,
        query: String,
        matching: TextMatch,
        fill: Option<TsQuery>,
    },
    /// `search_score()`: the [0,1] Score leaf of this statement's `search()`
    /// predicate. The index and the query are the predicate's own, copied
    /// here while the `WHERE` compiles, so the two always score the same
    /// words.
    SearchScore {
        index: IndexId,
        query: String,
    },
    /// `col <=> v` in an arithmetic ranking is a DISTANCE, and the engine's
    /// leaf is a SIMILARITY (`-distance`). The lowering below writes the
    /// negation, so `1 - (col <=> v)` is the cosine itself, which is the same
    /// quantity `0.5 * (1 + VectorSimilarity)` names in `battle50k`.
    VectorDistance {
        index: IndexId,
        query: Vec<f32>,
        metric: VectorMetric,
        fill: Option<Literal>,
    },
    Distance {
        index: IndexId,
        center: Point,
        fill: Option<PointArg>,
    },
    Add(Box<OwnedScore>, Box<OwnedScore>),
    Sub(Box<OwnedScore>, Box<OwnedScore>),
    Mul(Box<OwnedScore>, Box<OwnedScore>),
    Div(Box<OwnedScore>, Box<OwnedScore>),
    Neg(Box<OwnedScore>),
}

/// Build the borrowed `ScoreExpr` tree on the stack and call `k` with it.
///
/// The tree is references all the way down, so it cannot be returned: every
/// node lives in a frame that encloses the frame of the node above it, and
/// `k` runs in the innermost one. Depth is bounded by `MAX_SCORE_DEPTH`
/// inside `compile_score_expr`, which runs after this and refuses a tree
/// that is too deep or too wide.
fn with_score<R>(node: &OwnedScore, k: &mut dyn FnMut(&ScoreExpr<'_>) -> R) -> R {
    match node {
        OwnedScore::Lit(value) => k(&ScoreExpr::Lit(*value)),
        OwnedScore::Scalar { index } => k(&ScoreExpr::Scalar { index: *index }),
        OwnedScore::Field { field } => k(&ScoreExpr::Field { field }),
        OwnedScore::Bm25 {
            index,
            query,
            matching,
            ..
        } => k(&ScoreExpr::Bm25 {
            index: *index,
            query,
            matching: *matching,
        }),
        OwnedScore::SearchScore { index, query } => k(&ScoreExpr::SearchScore {
            index: *index,
            query,
        }),
        OwnedScore::VectorDistance {
            index,
            query,
            metric,
            ..
        } => {
            let similarity = ScoreExpr::VectorSimilarity {
                index: *index,
                query,
                metric: *metric,
            };
            k(&ScoreExpr::Neg(&similarity))
        }
        OwnedScore::Distance { index, center, .. } => k(&ScoreExpr::Distance {
            index: *index,
            center: *center,
        }),
        OwnedScore::Neg(inner) => with_score(inner, &mut |e| k(&ScoreExpr::Neg(e))),
        OwnedScore::Add(a, b) => {
            with_score(a, &mut |left| {
                with_score(b, &mut |right| k(&ScoreExpr::Add(left, right)))
            })
        }
        OwnedScore::Sub(a, b) => {
            with_score(a, &mut |left| {
                with_score(b, &mut |right| k(&ScoreExpr::Sub(left, right)))
            })
        }
        OwnedScore::Mul(a, b) => {
            with_score(a, &mut |left| {
                with_score(b, &mut |right| k(&ScoreExpr::Mul(left, right)))
            })
        }
        OwnedScore::Div(a, b) => {
            with_score(a, &mut |left| {
                with_score(b, &mut |right| k(&ScoreExpr::Div(left, right)))
            })
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum OwnedOrder {
    Driver,
    EntityId,
    Scalar {
        index: IndexId,
        direction: SortDirection,
    },
    Distance {
        index: IndexId,
        center: Point,
        fill: Option<PointArg>,
    },
    Bm25 {
        index: IndexId,
        query: String,
        matching: TextMatch,
        fill: Option<TsQuery>,
    },
    /// A vector distance order. Which index answers is decided when the
    /// statement RUNS, from `SET LOCAL ef_search` as the transaction holds it
    /// then (`compile::ef_search`), so a cached or prepared plan follows the
    /// transaction it runs in: the approximate index with that `ef` when the
    /// knob is set, else the exact index, else the approximate one at
    /// [`DEFAULT_EF`](super::DEFAULT_EF).
    Vector {
        exact: Option<IndexId>,
        approximate: Option<IndexId>,
        query: Vec<f32>,
        metric: VectorMetric,
        fill: Option<Literal>,
    },
    Score {
        expr: OwnedScore,
        direction: SortDirection,
    },
    /// Several keys, or one key no index orders (`QueryOrder::Keys`).
    Keys(Vec<OwnedSortKey>),
}

/// One key of a several-key ORDER BY.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OwnedSortKey {
    pub(crate) value: OwnedSortValue,
    pub(crate) direction: SortDirection,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum OwnedSortValue {
    Field(String),
    Key,
    Score(OwnedScore),
}

/// Lower several sort keys to the engine's borrowed `SortKey`s. Each
/// expression is borrowed inside `with_score`'s callback, so the keys are
/// built one level of callback per expression and handed to `k` whole.
fn with_sort_keys<R>(
    keys: &[OwnedSortKey],
    prefix: &[SortKey<'_>],
    k: &mut dyn FnMut(&[SortKey<'_>]) -> R,
) -> R {
    let Some((first, rest)) = keys.split_first() else {
        return k(prefix);
    };
    let direction = first.direction;
    match &first.value {
        OwnedSortValue::Field(name) => {
            let mut built = prefix.to_vec();
            built.push(SortKey {
                value: SortValue::Field(name),
                direction,
            });
            with_sort_keys(rest, &built, k)
        }
        OwnedSortValue::Key => {
            let mut built = prefix.to_vec();
            built.push(SortKey {
                value: SortValue::Key,
                direction,
            });
            with_sort_keys(rest, &built, k)
        }
        OwnedSortValue::Score(expr) => with_score(expr, &mut |compiled| {
            let mut built = prefix.to_vec();
            built.push(SortKey {
                value: SortValue::Score(compiled),
                direction,
            });
            with_sort_keys(rest, &built, k)
        }),
    }
}

/// How one output column is filled from a returned row.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Output {
    /// The E4 row identity, which every row carries.
    Id,
    /// The n-th projected field.
    Field(usize),
    /// The external key.
    ///
    /// `Projection` refuses the reserved field the key lives in
    /// (`collections::reserved`, `src/collections/mod.rs`), so there is no
    /// way to ask a page for it; the only atomic that hands a key back is
    /// `Database::get_by_id`, which is one point-get per RETURNED row. That
    /// read is named here rather than hidden: `_id` costs nothing and is
    /// what a caller that already holds a key table should select.
    Key,
    /// This statement's own ranking value.
    OrderValue,
    /// A §4.1 / §4.2 ROW function, by its position in `SelectPlan::functions`.
    /// Evaluated over the values this same row already projected, so it reads
    /// nothing extra.
    Row(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SelectPlan {
    pub(crate) collection: CollectionId,
    pub(crate) columns: Vec<String>,
    pub(crate) outputs: Vec<Output>,
    /// The fields `Projection::Fields` asks for, in order.
    pub(crate) fields: Vec<String>,
    pub(crate) filters: Vec<OwnedFilter>,
    pub(crate) order: OwnedOrder,
    pub(crate) limit: Option<usize>,
    pub(crate) driver: CandidateDriver,
    /// The statement as written, for the EXPLAIN header.
    pub(crate) text: String,
    /// The row expressions `Output::Row` indexes into.
    pub(crate) functions: Vec<CompiledRow>,
    /// One line per WHERE function that became an index RANGE, for EXPLAIN.
    pub(crate) rewrites: Vec<String>,
    /// One line per projected ROW function, for EXPLAIN.
    pub(crate) row_functions: Vec<String>,
}

impl SelectPlan {
    /// True when a column of this statement is the external key, which is
    /// one point-get per returned row (see [`Output::Key`]).
    pub(crate) fn reads_keys(&self) -> bool {
        self.outputs.iter().any(|output| *output == Output::Key)
    }

    /// The SQL type column `at` has when the source collection's declared
    /// fields cannot say: a row function that returns a `bytea`, a number.
    /// `None` leaves the caller to the declared field of the same name.
    pub(crate) fn column_type(&self, at: usize) -> Option<&'static str> {
        match self.outputs.get(at)? {
            Output::Row(function) => self.functions[*function].output_type(),
            _ => None,
        }
    }

    /// One returned row, as this statement's columns.
    pub(crate) fn row(&self, db: &Database, row: &QueryRow) -> SqlResult2<SqlRow> {
        let mut key = None;
        if self.reads_keys() {
            key = db
                .get_by_id(row.id)
                .map_err(SqlError::from)?
                .map(|entity| entity.key);
        }
        // The projected values, once, so every row function reads the same
        // list rather than re-decoding.
        let fields: Vec<SqlValue> = row
            .projected
            .iter()
            .map(|(_, value)| projected(value))
            .collect();
        let mut values = Vec::with_capacity(self.outputs.len());
        for output in &self.outputs {
            values.push(match output {
                Output::Id => SqlValue::Id(row.id),
                Output::Field(at) => fields.get(*at).cloned().unwrap_or(SqlValue::Missing),
                Output::Key => key.clone().map_or(SqlValue::Missing, SqlValue::Text),
                Output::OrderValue => order_value(&row.order),
                Output::Row(at) => self.functions[*at].eval(&fields)?,
            });
        }
        Ok(SqlRow { id: row.id, values })
    }

    /// Prepare the query this plan compiled to and hand it to `body`.
    pub(crate) fn with_query<T>(
        &self,
        db: &Database,
        body: &mut dyn FnMut(&mut sekejap_core::collections::PreparedQuery<'_>) -> SqlResult2<T>,
    ) -> SqlResult2<T> {
        // The boolean trees go on this call's stack: a `QueryFilter::Any`
        // names a slice and a `Not` a reference, and a compiled plan
        // outlives every statement it was built from.
        with_borrowed_filters(&self.filters, &mut |filters| {
                let fields: Vec<&str> = self.fields.iter().map(String::as_str).collect();
        let projection = if fields.is_empty() {
            Projection::Ids
        } else {
            Projection::Fields(&fields)
        };
        let mut run = |order: QueryOrder<'_>| -> SqlResult2<T> {
            let mut prepared = db.prepare_query(QueryRequest {
                collection: self.collection,
                filters: &filters,
                order,
                projection,
                total_limit: self.limit,
                driver: self.driver,
            })?;
            body(&mut prepared)
        };
        match &self.order {
            OwnedOrder::Driver => run(QueryOrder::Driver),
            OwnedOrder::EntityId => run(QueryOrder::EntityId),
            OwnedOrder::Scalar { index, direction } => run(QueryOrder::Scalar {
                index: *index,
                direction: *direction,
            }),
            OwnedOrder::Distance { index, center, .. } => run(QueryOrder::Distance {
                index: *index,
                center: *center,
                direction: SortDirection::Ascending,
            }),
            OwnedOrder::Bm25 {
                index,
                query,
                matching,
                ..
            } => run(QueryOrder::Bm25 {
                index: *index,
                query,
                matching: *matching,
            }),
            OwnedOrder::Vector {
                exact,
                approximate,
                query,
                metric,
                ..
            } => match (super::ef_search(), exact, approximate) {
                (Some(ef), _, Some(index)) => run(QueryOrder::ApproximateVector {
                    index: *index,
                    query,
                    metric: *metric,
                    ef,
                }),
                (_, Some(index), _) => run(QueryOrder::ExactVector {
                    index: *index,
                    query,
                    metric: *metric,
                }),
                (_, None, Some(index)) => run(QueryOrder::ApproximateVector {
                    index: *index,
                    query,
                    metric: *metric,
                    ef: super::DEFAULT_EF,
                }),
                (_, None, None) => unreachable!("a vector order names an index"),
            },
            OwnedOrder::Score { expr, direction } => with_score(expr, &mut |compiled| {
                run(QueryOrder::Score {
                    expr: compiled,
                    direction: *direction,
                })
            }),
            OwnedOrder::Keys(keys) => with_sort_keys(keys, &[], &mut |built| run(QueryOrder::Keys(built))),
        }
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CompiledIndex {
    Scalar { field: String, unique: bool },
    /// An EXPRESSION scalar index over `lower(field)`.
    LowerScalar { field: String },
    /// An EXPRESSION scalar index over `field->>'member'`, the TEXT at one
    /// member of a `JSONB` column.
    JsonScalar { field: String, member: String },
    Text { field: String },
    Point { field: String },
    Geometry { field: String },
    ExactVector { field: String },
    QuantizedVector { field: String },
    VamanaGraph { field: String },
}

/// What one `ALTER TABLE` action became: a descriptor rewrite, or a rename.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CompiledAlter {
    /// The new field list, the declared spellings and the COLUMN RULES that
    /// survive it, plus the indexes a DROP COLUMN takes with the column.
    Layout {
        fields: Vec<(String, Kind)>,
        declared: Vec<(String, String)>,
        rules: Vec<(String, ColumnRule)>,
        drop_indexes: Vec<(IndexId, String)>,
        /// The AUTOMATIC indexes of `docs/lang/INDEX_CONTRACT.md` this
        /// rewrite creates AFTER the layout is repointed: the one an
        /// `ADD COLUMN` of an eligible kind earns, and the ones a
        /// `RENAME COLUMN` re-earns under the new name. Empty for every
        /// other action, and for a column no family covers.
        create_indexes: Vec<(String, CompiledIndex)>,
    },
    /// `RENAME TO`: one name record, and the `CollectionId` does not change.
    Rename { to: String },
}

/// An INSERT's `RETURNING` columns. `time[i]` is `Some(date_only)` for a
/// declared TIMESTAMPTZ/DATE, which reads back as the ISO text a SELECT
/// prints.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Returning {
    pub(crate) columns: Vec<String>,
    pub(crate) time: Vec<Option<bool>>,
}

impl Returning {
    pub(crate) fn push(&mut self, name: &str, time: Option<bool>) {
        self.columns.push(name.to_owned());
        self.time.push(time);
    }

    fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// One stored row's values, read the way a SELECT reads them.
    fn row(&self, row: &sekejap_core::collections::Entity) -> Vec<SqlValue> {
        self.columns
            .iter()
            .zip(&self.time)
            .map(|(name, time)| {
                if crate::is_key_column(name) {
                    return SqlValue::Text(row.key.clone());
                }
                match (field_value(&row.document, name), time) {
                    (SqlValue::Int(micros), Some(false)) => {
                        SqlValue::Text(crate::functions::format_timestamp(micros))
                    }
                    (SqlValue::Int(micros), Some(true)) => {
                        SqlValue::Text(crate::functions::format_date(micros))
                    }
                    (value, _) => value,
                }
            })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum WritePlan {
    Insert {
        collection: CollectionId,
        /// Each row's key, or `None` to mint one from the key's DEFAULT when
        /// the statement runs (never at prepare: one key per execution).
        rows: Vec<(Option<String>, Value)>,
        /// The named PRIMARY KEY column, which holds a minted key too.
        key_column: Option<String>,
        /// `ON CONFLICT (_key)`: `Some(None)` is `DO NOTHING`, `Some(Some(cols))`
        /// is `DO UPDATE SET c = EXCLUDED.c` for each named column. `None` is
        /// a plain INSERT, which refuses a taken key with 23505.
        on_conflict: Option<Option<Vec<String>>>,
        /// `RETURNING`: empty for none. A row the statement did not write
        /// (`DO NOTHING` on a taken key) returns nothing, as in PostgreSQL.
        returning: Returning,
    },
    Update {
        collection: CollectionId,
        key: String,
        patch: Value,
    },
    Delete {
        collection: CollectionId,
        key: String,
    },
    /// `UPDATE t SET ... WHERE <any predicate>`: `Database::update_where`,
    /// which pages the same prepared query a `SELECT` with that predicate
    /// pages and read-modify-puts each candidate.
    UpdateWhere {
        collection: CollectionId,
        filters: Vec<OwnedFilter>,
        sets: Vec<(String, CompiledSet)>,
        /// The row fields every `CompiledSet::Row` expression indexes into,
        /// in the order `CompiledRow::Field` numbers them.
        fields: Vec<String>,
        driver: CandidateDriver,
    },
    /// `DELETE FROM t WHERE <any predicate> [RESTRICT|CASCADE]`:
    /// `Database::write_where` with `WriteAction::Delete`.
    DeleteWhere {
        collection: CollectionId,
        filters: Vec<OwnedFilter>,
        cascade: bool,
        driver: CandidateDriver,
    },
    /// `BEGIN BULK` / `END BULK` (`docs/dist/OPS_CONTRACT.md` §7).
    BeginBulk,
    EndBulk,
    CreateTable {
        name: String,
        fields: Vec<(String, Kind)>,
        /// The DECLARED spelling of the columns whose `Kind` does not carry
        /// it (`TIMESTAMPTZ`, `DATE`). Recorded in the catalog descriptor so
        /// a reopened database still knows a column is a timestamp and can
        /// print it back as an ISO string (QL_CONTRACT §4.2).
        declared: Vec<(String, String)>,
        /// The per-field COLUMN RULES: `DEFAULT <generator>` and `NOT NULL`
        /// (QL_CONTRACT §2), recorded in the same descriptor behind the
        /// additive `COLUMN_RULES_FEATURE` bit.
        rules: Vec<(String, ColumnRule)>,
        /// The `WITH (...)` INDEX SUGAR, already compiled: one
        /// `(generated name, index)` pair per column the clause named, in
        /// written order. EMPTY for every `CREATE TABLE` with no `WITH`, and
        /// that statement runs exactly the code it always ran.
        indexes: Vec<(String, CompiledIndex)>,
        /// How the key is declared: a named PRIMARY KEY column and/or a key
        /// DEFAULT. `None` records nothing, as before.
        key: Option<sekejap_core::collections::KeySpec>,
    },
    /// `ALTER TABLE t <action>`: one `alter_collection_rules` commit, or one
    /// `rename_collection` commit. Nothing here is a second rewrite path.
    AlterTable {
        collection: CollectionId,
        table: String,
        action: CompiledAlter,
    },
    CreateIndex {
        collection: CollectionId,
        name: String,
        method: CompiledIndex,
    },
    DropIndex {
        index: IndexId,
        name: String,
    },
    /// `CREATE SCHEMA`: one record, `Database::create_schema`.
    CreateSchema { name: String },
    /// `DROP SCHEMA`: `Database::drop_schema`, which refuses a schema that
    /// still holds a table.
    DropSchema { name: String },
    /// `DROP TABLE [IF EXISTS] name [CASCADE|RESTRICT]`: the DROPPING mark,
    /// then bounded steps to the end. Nothing here is a second removal path --
    /// it is `begin_drop_collection` and `drop_collection_step`, the same
    /// atomics a caller writes by hand.
    DropTable {
        collection: CollectionId,
        name: String,
        mode: DropMode,
    },
    /// `CREATE TABLE` of an EDGE TABLE (`docs/core/EDGE_TABLES.md` §2.1):
    /// the collection, then its declaration, in one statement.
    CreateEdgeTable {
        name: String,
        fields: Vec<(String, Kind)>,
        declared: Vec<(String, String)>,
        rules: Vec<(String, ColumnRule)>,
        /// Each REFERENCES column and the table it names.
        references: Vec<(String, String)>,
        key: Vec<String>,
    },
    /// `CREATE/ALTER PROPERTY GRAPH`: one `bind_edge_table` per edge table.
    BindEdgeTables {
        graph: String,
        binds: Vec<(CollectionId, String, String, String)>,
    },
    DropPropertyGraph {
        name: String,
        if_exists: bool,
    },
    InsertEdges {
        collection: CollectionId,
        rows: Vec<Value>,
        on_conflict: Option<sekejap_core::collections::OnConflict>,
    },
    UpdateEdges {
        collection: CollectionId,
        filter: Value,
        patch: Value,
    },
    DeleteEdges {
        collection: CollectionId,
        filter: Value,
    },
    Begin,
    Commit,
    Rollback,
    /// A statement that changed nothing and said so.
    Notice(String),
    /// `SET LOCAL ef_search = n` (or `= DEFAULT`, `None`): takes effect when
    /// it RUNS, for the rest of the transaction, and says so.
    SetEf(Option<usize>, String),
}

/// One row field as a row expression reads it. `Missing` and `Null` are
/// distinct in e4 and both are nullish to a row function, exactly as they are
/// to a projected value.
pub(super) fn field_value(row: &Value, name: &str) -> SqlValue {
    match row.get(name) {
        None => SqlValue::Missing,
        Some(Value::Null) => SqlValue::Null,
        Some(Value::Bool(b)) => SqlValue::Bool(*b),
        Some(Value::Number(n)) => match n.as_i64() {
            Some(i) => SqlValue::Int(i),
            None => SqlValue::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        Some(Value::String(text)) => SqlValue::Text(text.clone()),
        Some(other) => SqlValue::Json(other.clone()),
    }
}

/// What a row expression produced, checked against the column's declared
/// `Kind` before it is stored. The check is at RUN time because the value is
/// the row's, not the statement's; a literal is checked while the statement
/// compiles, where it belongs.
fn stored_value(kind: &Kind, column: &str, value: SqlValue) -> Result<Value, String> {
    let mismatch = |what: &str| {
        Err(format!(
            "UPDATE ... SET `{column}`: the column is {what} and the row expression produced a value that is not"
        ))
    };
    Ok(match value {
        SqlValue::Missing | SqlValue::Null => Value::Null,
        SqlValue::Id(_) => return mismatch("a declared column, and `_id` is the row identity"),
        SqlValue::Bool(b) => match kind {
            Kind::Bool | Kind::Json => Value::Bool(b),
            _ => return mismatch("not BOOLEAN"),
        },
        SqlValue::Int(i) => match kind {
            Kind::Int | Kind::Json => Value::from(i),
            Kind::Real => Value::from(i as f64),
            _ => return mismatch("not a number"),
        },
        SqlValue::Float(f) => match kind {
            Kind::Real | Kind::Json => Value::from(f),
            Kind::Int if f.fract() == 0.0 => Value::from(f as i64),
            _ => return mismatch("not a number"),
        },
        SqlValue::Text(text) => match kind {
            Kind::Text | Kind::Json => Value::String(text),
            _ => return mismatch("not TEXT"),
        },
        SqlValue::Json(value) => match kind {
            Kind::Json | Kind::Geo | Kind::Point => value,
            _ => return mismatch("not JSONB"),
        },
    })
}

fn engine_error(message: String) -> sekejap_core::collections::Error {
    sekejap_core::collections::Error::InvalidInput(message)
}

impl WritePlan {
    pub(crate) fn run(
        self,
        db: &mut Database,
        notices: Vec<String>,
        budget: QueryBudget,
    ) -> SqlResult2<SqlResult> {
        let notice = |extra: String| -> SqlResult<> {
            let mut all = notices.clone();
            all.push(extra);
            SqlResult::Notice(all.join("; "))
        };
        Ok(match self {
            Self::CreateEdgeTable {
                name,
                fields,
                declared,
                rules,
                references,
                key,
            } => {
                let (schema, table) = crate::split_table(&name);
                let mut resolved = Vec::with_capacity(references.len());
                for (column, target) in &references {
                    let c = crate::find(db, target)?.ok_or_else(|| {
                        SqlError::engine(format!("`{column}` REFERENCES {target}: no table named `{target}`"))
                    })?;
                    resolved.push((column.clone(), c));
                }
                let c = db.create_collection_in(
                    schema,
                    table,
                    fields,
                    declared,
                    rules,
                    CollectionOptions::default(),
                )?;
                db.declare_edge_table(c, resolved, key)?;
                db.commit()?;
                SqlResult::Affected(0)
            }
            Self::BindEdgeTables { graph, binds } => {
                for (c, source, destination, label) in &binds {
                    db.bind_edge_table(*c, source, destination, label, &graph)?;
                }
                db.commit()?;
                notice(format!(
                    "CREATE PROPERTY GRAPH {graph}: {} edge table(s) declared",
                    binds.len()
                ))
            }
            Self::DropPropertyGraph { name, if_exists } => {
                let dropped = db.drop_property_graph(&name)?;
                if dropped == 0 {
                    if !if_exists {
                        return Err(SqlError::coded(
                            "42704",
                            format!("property graph `{name}` does not exist"),
                        ));
                    }
                    return Ok(notice(format!(
                        "DROP PROPERTY GRAPH IF EXISTS {name}: no such property graph"
                    )));
                }
                db.commit()?;
                SqlResult::Affected(0)
            }
            Self::InsertEdges {
                collection,
                rows,
                on_conflict,
            } => {
                let mut affected = 0u64;
                for row in &rows {
                    match &on_conflict {
                        None => {
                            db.insert_edge_row(collection, row)?;
                            affected += 1;
                        }
                        Some(action) => {
                            if db.upsert_edge_row(collection, row, action)?.is_some() {
                                affected += 1;
                            }
                        }
                    }
                }
                SqlResult::Affected(affected)
            }
            Self::UpdateEdges {
                collection,
                filter,
                patch,
            } => SqlResult::Affected(db.update_edge_rows(collection, &filter, &patch)? as u64),
            Self::DeleteEdges { collection, filter } => {
                SqlResult::Affected(db.delete_edge_rows(collection, &filter)? as u64)
            }
            Self::Insert {
                collection,
                rows,
                key_column,
                on_conflict,
                returning,
            } => {
                let mut affected = 0u64;
                let mut returned = Vec::new();
                for (key, mut document) in rows {
                    let key = match key {
                        Some(key) => key,
                        None => {
                            let minted = db.mint_key(collection)?.ok_or_else(|| {
                                SqlError::coded("23502", "the INSERT names no key and the table declares no key DEFAULT")
                            })?;
                            if let (Some(column), Value::Object(fields)) = (&key_column, &mut document) {
                                fields.insert(column.clone(), Value::String(minted.clone()));
                            }
                            minted
                        }
                    };
                    match &on_conflict {
                        None => {
                            db.insert(collection, &key, &document)?;
                        }
                        Some(action) => {
                            if db.get(collection, &key)?.is_none() {
                                db.insert(collection, &key, &document)?;
                            } else if let Some(columns) = action {
                                let mut patch = serde_json::Map::new();
                                for column in columns {
                                    patch.insert(
                                        column.clone(),
                                        document.get(column).cloned().unwrap_or(Value::Null),
                                    );
                                }
                                db.update(collection, &key, &Value::Object(patch))?;
                            } else {
                                continue;
                            }
                        }
                    }
                    affected += 1;
                    // Read back after the write, so a column its DEFAULT
                    // filled reports the stored value.
                    if !returning.is_empty() {
                        let row = db.get(collection, &key)?.ok_or_else(|| {
                            SqlError::unsupported("INSERT ... RETURNING: the row just written is not found")
                        })?;
                        returned.push(SqlRow {
                            id: row.id,
                            values: returning.row(&row),
                        });
                    }
                }
                if returning.is_empty() {
                    SqlResult::Affected(affected)
                } else {
                    SqlResult::Rows {
                        columns: returning.columns,
                        rows: returned,
                    }
                }
            }
            Self::Update {
                collection,
                key,
                patch,
            } => {
                if db.get(collection, &key)?.is_none() {
                    return Ok(SqlResult::Affected(0));
                }
                db.update(collection, &key, &patch)?;
                SqlResult::Affected(1)
            }
            Self::Delete { collection, key } => {
                let gone = db.delete(collection, &key)?;
                SqlResult::Affected(u64::from(gone))
            }
            Self::UpdateWhere {
                collection,
                filters,
                sets,
                fields,
                driver,
            } => {
                // The row expressions become the `&mut dyn FnMut` closures
                // the engine's `UpdatePatch` takes. They are built here, on
                // this call's stack, because the patch borrows them for the
                // length of the pass -- the same reason a prepared query is
                // handed out through a callback.
                type RowFn<'a> = Box<
                    dyn FnMut(&Value) -> sekejap_core::collections::Result<Value> + 'a,
                >;
                let mut closures: Vec<RowFn<'_>> = Vec::new();
                for (column, set) in &sets {
                    if let CompiledSet::Row { expr, kind } = set {
                        let expr = expr.clone();
                        let kind = kind.clone();
                        let column = column.clone();
                        let names = fields.clone();
                        closures.push(Box::new(move |row: &Value| {
                            let values: Vec<SqlValue> =
                                names.iter().map(|name| field_value(row, name)).collect();
                            let produced = expr
                                .eval(&values)
                                .map_err(|e| engine_error(e.to_string()))?;
                            stored_value(&kind, &column, produced).map_err(engine_error)
                        }));
                    }
                }
                let mut patch = UpdatePatch::new();
                let mut rows = closures.iter_mut();
                for (column, set) in &sets {
                    match set {
                        CompiledSet::Lit(value) => patch.set(column, value.clone())?,
                        CompiledSet::Row { .. } => {
                            let f = rows.next().expect("one closure per row expression");
                            patch.set_row(column, &mut **f)?;
                        }
                    }
                }
                let patch = &patch;
                let written = with_write_filters(&filters, &mut |borrowed| {
                    run_write(
                        db,
                        WriteRequest {
                            collection,
                            filters: borrowed,
                            action: WriteAction::Update(patch),
                            driver,
                            after: WriteCursor::start(),
                        },
                        budget,
                        "UPDATE",
                    )
                })?;
                SqlResult::Affected(written)
            }
            Self::DeleteWhere {
                collection,
                filters,
                cascade,
                driver,
            } => {
                let mode = if cascade {
                    sekejap_core::collections::DeleteMode::Cascade
                } else {
                    sekejap_core::collections::DeleteMode::Restrict
                };
                let written = with_write_filters(&filters, &mut |borrowed| {
                    run_write(
                        db,
                        WriteRequest {
                            collection,
                            filters: borrowed,
                            action: WriteAction::Delete(mode),
                            driver,
                            after: WriteCursor::start(),
                        },
                        budget,
                        "DELETE",
                    )
                })?;
                SqlResult::Affected(written)
            }
            Self::BeginBulk => {
                db.begin_bulk()?;
                notice(format!(
                    "BEGIN BULK: the durability point moves to the matching END BULK; {} scope(s) open",
                    db.bulk_depth()
                ))
            }
            Self::EndBulk => {
                let committed = db.end_bulk()?;
                if committed {
                    SqlResult::Affected(0)
                } else {
                    notice(format!(
                        "END BULK: an inner scope closed and committed nothing; {} scope(s) still open",
                        db.bulk_depth()
                    ))
                }
            }
            Self::CreateTable {
                name,
                fields,
                declared,
                rules,
                indexes,
                key,
            } => {
                if indexes.is_empty() {
                    {
                        let (schema, table) = crate::split_table(&name);
                        let collection = db.create_collection_in(
                            schema,
                            table,
                            fields,
                            declared,
                            rules,
                            CollectionOptions::default(),
                        )?;
                        if let Some(key) = key {
                            db.set_key_spec(collection, key)?;
                        }
                    }
                    db.commit()?;
                    return Ok(SqlResult::Affected(0));
                }
                // The INDEX SUGAR of QL_CONTRACT §2. One `create_collection`
                // and one `create_*_index` per named column -- the same
                // atomics `CREATE TABLE` and N `CREATE INDEX` call, in the
                // same order, inside one statement.
                //
                // The caller's own uncommitted rows are published FIRST, for
                // the reason `CREATE INDEX` publishes them: an index build
                // commits its own steps, and committing someone else's
                // half-finished work on their behalf is not this statement's
                // decision. It also draws the line the UNWIND below needs --
                // everything after this commit belongs to this statement, so
                // removing it removes nothing of the caller's.
                db.commit()?;
                let count = indexes.len();
                let (schema, table) = crate::split_table(&name);
                let collection = db.create_collection_in(
                    schema,
                    table,
                    fields,
                    declared,
                    rules,
                    CollectionOptions::default(),
                )?;
                if let Some(key) = key {
                    db.set_key_spec(collection, key)?;
                }
                db.commit()?;
                let mut built = Vec::new();
                for (index, method) in indexes {
                    match build_index(db, collection, &index, &method) {
                        Ok(()) => built.push(index),
                        Err(error) => {
                            return Err(unwind_create_table(
                                db, collection, &name, &index, &built, error,
                            ))
                        }
                    }
                }
                notice(format!(
                    "CREATE TABLE {name} WITH (...): the collection and {count} index(es) were created by one statement -- {}",
                    built.join(", ")
                ))
            }
            Self::AlterTable {
                collection,
                table,
                action,
            } => {
                db.commit()?;
                match action {
                    CompiledAlter::Rename { to } => db.rename_collection(collection, &to)?,
                    CompiledAlter::Layout {
                        fields,
                        declared,
                        rules,
                        drop_indexes,
                        create_indexes,
                    } => {
                        // The contract's DROP COLUMN drops the column's index
                        // with it. It is the ordinary bounded drop, committed
                        // before the layout is repointed, so an interrupted
                        // statement leaves a dropped index and the old layout
                        // -- never a layout pointing at an index tree keyed on
                        // a field the layout has not got.
                        for (index, _) in drop_indexes {
                            db.begin_drop_index(index)?;
                            while !db.drop_index_step(index, 256)? {}
                            db.commit()?;
                        }
                        db.alter_collection_rules(collection, fields, declared, rules)?;
                        db.commit()?;
                        // And the mirror of that: a column the layout now HAS
                        // gets the index `docs/lang/INDEX_CONTRACT.md` gives
                        // it, after the layout is repointed and never before
                        // -- `validate_indexed_layout` refuses an index over a
                        // field the layout has not got. The build is the same
                        // `build_index` a `CREATE INDEX` runs.
                        for (index, method) in create_indexes {
                            build_index(db, collection, &index, &method)?;
                        }
                    }
                }
                db.commit()?;
                let _ = table;
                SqlResult::Affected(0)
            }
            Self::CreateIndex {
                collection,
                name,
                method,
            } => {
                db.commit()?;
                build_index(db, collection, &name, &method)?;
                SqlResult::Affected(0)
            }
            Self::CreateSchema { name } => {
                db.commit()?;
                db.create_schema(&name)?;
                db.commit()?;
                SqlResult::Affected(0)
            }
            Self::DropSchema { name } => {
                db.commit()?;
                db.drop_schema(&name)?;
                db.commit()?;
                SqlResult::Affected(0)
            }
            Self::DropIndex { index, name } => {
                db.commit()?;
                db.begin_drop_index(index)?;
                while !db.drop_index_step(index, 256)? {}
                db.commit()?;
                let _ = name;
                SqlResult::Affected(0)
            }
            Self::DropTable {
                collection,
                name,
                mode,
            } => {
                // The mark is committed by `begin_drop_collection_mode`
                // itself, and every step after it is committed by
                // `drop_collection_to_end`; a statement that is interrupted
                // leaves a resumable drop, never a half-removed collection.
                db.commit()?;
                db.begin_drop_collection_mode(collection, mode)?;
                let removed = db.drop_collection_to_end(
                    collection,
                    sekejap_core::collections::MAX_DROP_BATCH,
                )?;
                let _ = name;
                SqlResult::Affected(removed)
            }
            Self::Begin => notice(
                "BEGIN: the writer is single and already inside a transaction; COMMIT ends it"
                    .to_owned(),
            ),
            Self::Commit => {
                db.commit()?;
                super::end_transaction();
                SqlResult::Affected(0)
            }
            Self::Rollback => {
                db.rollback()?;
                super::end_transaction();
                SqlResult::Affected(0)
            }
            Self::Notice(text) => notice(text),
            Self::SetEf(ef, text) => {
                super::set_ef_search(ef);
                notice(text)
            }
        })
    }
}

/// One `CREATE INDEX`, created and built to READY.
///
/// The ONE place a `CompiledIndex` becomes a `create_*_index` call. Both
/// `CREATE INDEX` and the `CREATE TABLE ... WITH (...)` sugar of
/// QL_CONTRACT §2 come through here, so the sugar cannot build an index that
/// differs from the one the hand-written statement builds: same call, same
/// arguments, same build to READY.
///
/// The caller commits what it holds BEFORE calling. Postgres hands back a
/// usable index and so does this: the build is incremental underneath
/// (`build_index_step`) and is run to the end here rather than left
/// half-built.
fn build_index(
    db: &mut Database,
    collection: CollectionId,
    name: &str,
    method: &CompiledIndex,
) -> SqlResult2<()> {
    let id = match method {
        CompiledIndex::Scalar { field, unique } => {
            db.create_scalar_index(collection, name, field, *unique)?
        }
        CompiledIndex::LowerScalar { field } => {
            db.create_expression_index(collection, name, field, IndexExpr::Lower, false)?
        }
        CompiledIndex::JsonScalar { field, member } => db.create_expression_index(
            collection,
            name,
            field,
            IndexExpr::JsonText(member.clone()),
            false,
        )?,
        CompiledIndex::Text { field } => db.create_text_index(collection, name, field)?,
        CompiledIndex::Point { field } => db.create_point_index(collection, name, field)?,
        CompiledIndex::Geometry { field } => db.create_geometry_index(collection, name, field)?,
        CompiledIndex::ExactVector { field } => {
            db.create_exact_vector_index(collection, name, field)?
        }
        CompiledIndex::QuantizedVector { field } => {
            db.create_quantized_vector_index(collection, name, field)?
        }
        CompiledIndex::VamanaGraph { field } => db.create_vamana_index(collection, name, field)?,
    };
    db.commit()?;
    if let Err(built) = db.build_index_to_ready(id, build_chunk_rows(method)) {
        // A UNIQUE index over rows that already break it: nothing it
        // indexed is kept. The descriptor was committed, so it is dropped
        // with the ordinary bounded steps before the refusal is raised.
        if matches!(built, sekejap_core::collections::Error::Constraint { .. }) {
            db.rollback()?;
            db.begin_drop_index(id)?;
            while !db.drop_index_step(id, 256)? {}
            db.commit()?;
        }
        return Err(built.into());
    }
    db.commit()?;
    Ok(())
}

/// The largest build transaction the engine will ever take on: `MAX_BATCH`
/// in `core/engine/src/collections/catalog.rs`.
const BUILD_CHUNK_ROWS: usize = 256;
/// The chunk a VAMANA build takes, and why it is not that.
///
/// A chunk is not a batch size a caller tunes. It is the SMALLEST
/// transaction `build_index_to_ready` can fall back to: the driver groups
/// chunks by measuring what the last group cost and halves the group when
/// the page-WAL refuses one, and at a group of one the chunk IS the
/// transaction, so a refusal there is returned to the caller.
///
/// Every other family's build writes ONE derived record per row -- a scalar
/// entry, a quantized entry, a posting -- so `BUILD_CHUNK_ROWS` rows is a
/// transaction of `BUILD_CHUNK_ROWS` small records. The vamana graph links
/// each row into the graph instead: up to `1 + 2R` records per row, its own
/// head and adjacency plus an appended edge in every neighbour it chose and
/// every neighbour a prune displaced, and at a wide vector the head alone is
/// a page. A chunk of the same row count is therefore a transaction two
/// orders of magnitude larger, and the fall-back floor has to be lower for
/// the driver to have anywhere to fall back TO.
///
/// This is not what makes a wide build fit -- the record layout is
/// (`core/engine/src/index/vector/graph.rs`, `0x7D` heads and `0x7F`
/// adjacency). It is what stops a hardcoded 256 from being the thing that
/// decides, which is what it was: the atomic caller could pass a smaller
/// chunk and this path could not.
const VAMANA_CHUNK_ROWS: usize = 32;

fn build_chunk_rows(method: &CompiledIndex) -> usize {
    match method {
        CompiledIndex::VamanaGraph { .. } => VAMANA_CHUNK_ROWS,
        _ => BUILD_CHUNK_ROWS,
    }
}

/// What a `CREATE TABLE ... WITH (...)` does when an index of the clause is
/// refused after earlier ones were built.
///
/// It is ATOMIC IN THE CALLER'S SENSE: the collection and every index this
/// statement had already committed are REMOVED before the refusal is raised,
/// so the caller is left with the catalog they had. That is a compensating
/// removal, not a rollback -- sekejap has one transaction per handle and no
/// savepoint, and an index build commits its own steps, so there is no
/// uncommitted state to discard. It is exactly `DROP TABLE <name> CASCADE`,
/// the same bounded phase machine, run by the statement instead of by hand.
///
/// Almost nothing reaches here: an unknown key, an undeclared column, a
/// family the column's `Kind` cannot carry and a colliding generated name are
/// all decided while the statement COMPILES, before a byte is written
/// (`lang/src/compile/ddl.rs::with_indexes`). What is left is the engine
/// refusing a create or a build -- the index ceiling, or the identity space.
///
/// If the removal ITSELF fails, the refusal says so and names the collection:
/// `begin_drop_collection_mode` has published the DROPPING mark by then, so
/// the collection is one readers already refuse and one `DROP TABLE` finishes.
fn unwind_create_table(
    db: &mut Database,
    collection: CollectionId,
    table: &str,
    refused: &str,
    built: &[String],
    error: SqlError,
) -> SqlError {
    // Whatever the failed create left uncommitted goes first: the drop below
    // refuses to run over a handle with pending user writes, and those writes
    // are this statement's own.
    let _ = db.rollback();
    let removal = db
        .begin_drop_collection_mode(collection, DropMode::Cascade)
        .and_then(|()| {
            db.drop_collection_to_end(collection, sekejap_core::collections::MAX_DROP_BATCH)
        });
    let made = if built.is_empty() {
        "no index of the clause had been built".to_owned()
    } else {
        format!("the {} index(es) already built ({})", built.len(), built.join(", "))
    };
    match removal {
        Ok(_) => SqlError::unsupported(format!(
            "CREATE TABLE {table} WITH (...): `{refused}` was refused, so the statement left NOTHING behind -- the collection `{table}` and {made} were removed before this refusal was raised, and the catalog is the one the statement started from. The refusal: {error}"
        )),
        Err(second) => SqlError::engine(format!(
            "CREATE TABLE {table} WITH (...): `{refused}` was refused, and removing what the statement had already built then failed too ({second}). The collection `{table}` carries the DROPPING mark, which every reader refuses; `DROP TABLE {table} CASCADE` resumes the removal from where it stopped. The first refusal: {error}"
        )),
    }
}

/// One bounded write pass, run to completion under the caller's budget.
///
/// The pass loops inside `Database::write_where` until it is done or its
/// `rows_written` budget stops it. A statement that runs out of budget is
/// REFUSED with the count it reached, never truncated silently
/// (`docs/lang/QL_CONTRACT.md` §2): the rows it did write are in the caller's
/// uncommitted transaction and a `ROLLBACK` discards them.
fn run_write(
    db: &mut Database,
    request: WriteRequest<'_, '_>,
    budget: QueryBudget,
    statement: &'static str,
) -> SqlResult2<u64> {
    let progress = db.write_where(request, budget)?;
    if !progress.done {
        return Err(SqlError::engine(format!(
            "{statement}: the `rows_written` budget of {} was reached after {} row(s); the statement is refused rather than truncated. The rows written so far are uncommitted -- ROLLBACK discards them, or raise the budget and run it again.",
            budget.rows_written, progress.rows_written
        )));
    }
    Ok(progress.rows_written)
}

#[derive(Clone, Debug)]
pub(crate) enum Plan {
    Select(SelectPlan),
    Explain(SelectPlan),
    Aggregate(AggregatePlan),
    ExplainAggregate(AggregatePlan),
    /// A statement whose driver is the bounded in-memory row list of
    /// `rows.rs`: a catalog view, a `SHOW`, a `SELECT` with no `FROM`.
    /// `RowsPlan::explain` says whether the answer is the rows or the plan
    /// that produced them.
    Rows(RowsPlan),
    Write(WritePlan),
    /// An EXPLAIN whose statement is not a query: the text is the plan, and
    /// nothing is run to produce it.
    ExplainText(String),
    /// `SELECT * FROM GRAPH_TABLE (<graph> ... RETURN ...)`: a GQL stage
    /// (`docs/lang/GQL_PROFILE_DESIGN.md` §6.1), and its `EXPLAIN`.
    Gql(GqlSqlPlan),
    ExplainGql(GqlSqlPlan),
    /// `SELECT ... FROM <edge table>`: one end's edges, read when the
    /// statement runs (`docs/core/EDGE_TABLES.md` §5.1).
    EdgeRows(super::edges::EdgeRowsPlan),
}

/// A compiled GQL statement and the parameters its next execution reads.
///
/// A GQL plan folds no parameter value (design §7): every `$n` is read
/// when an execution opens, so a bind swaps `params` and compiles nothing.
#[derive(Clone, Debug)]
pub(crate) struct GqlSqlPlan {
    pub(crate) plan: crate::gql::plan::GqlPlan,
    pub(crate) params: Vec<crate::Param>,
}

impl GqlSqlPlan {
    /// One execution, paged: each row handed to `body` as this statement's
    /// columns, carrying [`EntityId::NO_OWNER`] -- a row of a relation has
    /// no single stored owner (design Q1). Every page runs under `budget`
    /// (with the GQL memory caps, which no budget lifts) and `cancelled`.
    pub(crate) fn for_each_row(
        &self,
        db: &Database,
        page_rows: usize,
        budget: QueryBudget,
        cancelled: &mut dyn FnMut() -> bool,
        body: &mut dyn FnMut(&SqlRow) -> SqlResult2<()>,
    ) -> SqlResult2<()> {
        self.rows(db, page_rows, budget, cancelled, &mut |row| body(&row))
    }

    /// [`Self::for_each_row`], handing each row over rather than lending it.
    fn rows(
        &self,
        db: &Database,
        page_rows: usize,
        budget: QueryBudget,
        cancelled: &mut dyn FnMut() -> bool,
        body: &mut dyn FnMut(SqlRow) -> SqlResult2<()>,
    ) -> SqlResult2<()> {
        self.plan.for_each_page(
            db,
            &self.params,
            page_rows,
            sekejap_core::collections::gql::GqlBudget::from_query_budget(budget),
            cancelled,
            &mut |page| {
                for row in &page.rows {
                    body(SqlRow {
                        id: EntityId::NO_OWNER,
                        values: self.plan.row(row)?,
                    })?;
                }
                Ok(())
            },
        )
    }

    /// One execution to exhaustion, as a SQL answer.
    pub(crate) fn answer(&self, db: &Database) -> SqlResult2<SqlResult> {
        let mut rows = Vec::new();
        self.rows(db, crate::PAGE, QueryBudget::unlimited(), &mut || false, &mut |row| {
            rows.push(row);
            Ok(())
        })?;
        Ok(SqlResult::Rows {
            columns: self.plan.columns().to_vec(),
            rows,
        })
    }
}


// ── rebinding ─────────────────────────────────────────────────────────────
//
// One pass over the compiled form, refilling every typed slot from a new
// parameter list. Nothing here parses, compiles, reads the catalog or
// chooses a driver: the plan's SHAPE is what a prepare decided, and a rebind
// only writes values into the positions the prepare marked as slots.

/// Write `value` into `bound`, keeping whether the bound is inclusive: the
/// operator is the statement's, only the value is the caller's.
fn refill_bound<T>(bound: &mut Bound<T>, value: T) {
    *bound = match bound {
        Bound::Excluded(_) => Bound::Excluded(value),
        Bound::Included(_) | Bound::Unbounded => Bound::Included(value),
    };
}

impl OwnedFilter {
    /// Refill this filter's slots. A filter with no slot is untouched.
    pub(crate) fn rebind(&mut self, binder: &Binder<'_>) -> SqlResult2<()> {
        match self {
            Self::Scalar {
                predicate, fills, ..
            } => {
                for fill in fills.iter() {
                    let value = binder.scalar(&fill.kind, &fill.literal, &fill.column)?;
                    match (&mut *predicate, fill.at) {
                        (OwnedScalarFilter::Eq(slot), ScalarAt::Eq) => *slot = value,
                        (OwnedScalarFilter::Range { lower, .. }, ScalarAt::Lower) => {
                            refill_bound(lower, value)
                        }
                        (OwnedScalarFilter::Range { upper, .. }, ScalarAt::Upper) => {
                            refill_bound(upper, value)
                        }
                        _ => {
                            return Err(SqlError::unsupported(
                                "a rebind found a scalar slot whose compiled predicate has no such position",
                            ))
                        }
                    }
                }
            }
            Self::Key {
                lower,
                upper,
                fills,
            } => {
                for fill in fills.iter() {
                    let key = binder.text_of(&fill.literal)?;
                    match fill.at {
                        KeyAt::Lower => refill_bound(lower, key),
                        KeyAt::Upper => refill_bound(upper, key),
                    }
                }
            }
            Self::Text {
                query,
                matching,
                fill,
                ..
            } => {
                if let Some(source) = fill {
                    let (text, how) = binder.tsquery(source)?;
                    *query = text;
                    *matching = how;
                }
            }
            Self::Point {
                predicate, fill, ..
            } => {
                if let Some(fill) = fill {
                    *predicate = match fill {
                        PointFill::Radius { center, metres } => PointFilter::Radius {
                            center: binder.point_of(center)?,
                            radius_metres: binder.f64_of(metres)?,
                        },
                        PointFill::Bbox(argument) => PointFilter::Bbox(binder.bounds_of(argument)?),
                        PointFill::Overlaps(argument) => {
                            PointFilter::Bbox(binder.overlap_bounds_of(argument)?)
                        }
                    };
                }
            }
            Self::Geometry {
                predicate, fill, ..
            } => {
                if let Some(fill) = fill {
                    let geometry = binder.geom_of(&fill.argument)?;
                    *predicate = match fill.predicate {
                        SpatialPredicate::Intersects => GeometryFilter::Intersects(geometry),
                        SpatialPredicate::Within => GeometryFilter::Within(geometry),
                        SpatialPredicate::Contains => GeometryFilter::Contains(geometry),
                        SpatialPredicate::Overlaps => GeometryFilter::Overlaps(geometry),
                        SpatialPredicate::DWithin => GeometryFilter::DWithin {
                            geometry,
                            metres: match &fill.metres {
                                Some(literal) => binder.f64_of(literal)?,
                                None => {
                                    return Err(SqlError::syntax("ST_DWithin needs a distance", 0))
                                }
                            },
                        },
                    };
                }
            }
            Self::Any(children) | Self::All(children) => {
                for child in children.iter_mut() {
                    child.rebind(binder)?;
                }
            }
            Self::Not(child) => child.rebind(binder)?,
            // A semi-join's set was BUILT at compile, so a statement that
            // holds one is never rebindable and this arm is never reached
            // with a changed parameter.
            // The pattern was folded when the statement compiled, which marks
            // the statement as compiled again on a bind: there is no slot.
            Self::Like { .. } => {}
            // The values were read when the statement compiled, which marks
            // it as compiled again on a bind.
            Self::RowCompare { .. } => {}
            Self::Ids(_) => {}
        }
        Ok(())
    }
}

impl OwnedScore {
    fn rebind(&mut self, binder: &Binder<'_>) -> SqlResult2<()> {
        match self {
            Self::Bm25 {
                query,
                matching,
                fill,
                ..
            } => {
                if let Some(source) = fill {
                    let (text, how) = binder.tsquery(source)?;
                    *query = text;
                    *matching = how;
                }
            }
            Self::VectorDistance { query, fill, .. } => {
                if let Some(literal) = fill {
                    *query = binder.vector_of(literal)?;
                }
            }
            Self::Distance { center, fill, .. } => {
                if let Some(point) = fill {
                    *center = binder.point_of(point)?;
                }
            }
            Self::Add(a, b) | Self::Sub(a, b) | Self::Mul(a, b) | Self::Div(a, b) => {
                a.rebind(binder)?;
                b.rebind(binder)?;
            }
            Self::Neg(inner) => inner.rebind(binder)?,
            // A `search()` with a `$n` is folded at prepare, so a statement
            // that reaches a rebind has a CONSTANT search query and this leaf
            // has nothing to refill.
            Self::Lit(_) | Self::Scalar { .. } | Self::Field { .. } | Self::SearchScore { .. } => {}
        }
        Ok(())
    }
}

impl OwnedOrder {
    fn rebind(&mut self, binder: &Binder<'_>) -> SqlResult2<()> {
        match self {
            Self::Distance { center, fill, .. } => {
                if let Some(point) = fill {
                    *center = binder.point_of(point)?;
                }
            }
            Self::Bm25 {
                query,
                matching,
                fill,
                ..
            } => {
                if let Some(source) = fill {
                    let (text, how) = binder.tsquery(source)?;
                    *query = text;
                    *matching = how;
                }
            }
            Self::Vector { query, fill, .. } => {
                if let Some(literal) = fill {
                    *query = binder.vector_of(literal)?;
                }
            }
            Self::Score { expr, .. } => expr.rebind(binder)?,
            Self::Keys(keys) => {
                for key in keys {
                    if let OwnedSortValue::Score(expr) = &mut key.value {
                        expr.rebind(binder)?;
                    }
                }
            }
            Self::Driver | Self::EntityId | Self::Scalar { .. } => {}
        }
        Ok(())
    }
}

impl SelectPlan {
    /// Refill every slot of this SELECT from a new parameter list.
    pub(crate) fn rebind(&mut self, binder: &Binder<'_>) -> SqlResult2<()> {
        for filter in self.filters.iter_mut() {
            filter.rebind(binder)?;
        }
        self.order.rebind(binder)
    }
}
