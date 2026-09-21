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

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum OwnedFilter {
    Scalar {
        index: IndexId,
        predicate: OwnedScalarFilter,
    },
    Point {
        index: IndexId,
        predicate: PointFilter,
    },
    Geometry {
        index: IndexId,
        predicate: GeometryFilter,
    },
    Text {
        index: IndexId,
        query: String,
        matching: TextMatch,
    },
    Graph(OwnedGraph),
    Key {
        lower: Bound<String>,
        upper: Bound<String>,
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
}

impl OwnedFilter {
    fn borrowed(&self) -> QueryFilter<'_> {
        match self {
            Self::Scalar { index, predicate } => QueryFilter::Scalar {
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
            Self::Point { index, predicate } => QueryFilter::Point {
                index: *index,
                predicate: *predicate,
            },
            Self::Geometry { index, predicate } => QueryFilter::Geometry {
                index: *index,
                predicate: predicate.clone(),
            },
            Self::Text {
                index,
                query,
                matching,
            } => QueryFilter::Text {
                index: *index,
                query,
                matching: *matching,
            },
            // Filled in by `SelectPlan::with_query`, which owns the
            // borrowed predicate slices for the length of one prepared query.
            Self::Graph(_) => unreachable!("a graph filter is borrowed through `graph_request`"),
            Self::Key { lower, upper } => QueryFilter::Key {
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

/// Build the whole borrowed filter list -- traversals, boolean trees and
/// plain leaves alike -- on this call's stack and hand it to `k`.
///
/// One entry point for both `with_query` and `with_aggregate`, because a
/// `WHERE` clause is the same clause whichever of the two reads it.
/// `graph_edges` and `graph_nodes` are the traversal's borrowed predicate
/// slices, empty when the plan has no traversal.
pub(super) fn with_borrowed_filters<T>(
    owned: &[OwnedFilter],
    graph_edges: &[Vec<EdgePredicate<'_>>],
    graph_nodes: &[Vec<QueryFilter<'_>>],
    k: &mut dyn FnMut(&[QueryFilter<'_>]) -> SqlResult2<T>,
) -> SqlResult2<T> {
    let boolean: Vec<usize> = owned
        .iter()
        .enumerate()
        .filter(|(_, filter)| matches!(filter, OwnedFilter::Any(_) | OwnedFilter::All(_) | OwnedFilter::Not(_)))
        .map(|(at, _)| at)
        .collect();
    with_boolean_filters(owned, &boolean, None, graph_edges, graph_nodes, k)
}

fn with_boolean_filters<T>(
    owned: &[OwnedFilter],
    remaining: &[usize],
    built: Option<&BuiltAt<'_>>,
    graph_edges: &[Vec<EdgePredicate<'_>>],
    graph_nodes: &[Vec<QueryFilter<'_>>],
    k: &mut dyn FnMut(&[QueryFilter<'_>]) -> SqlResult2<T>,
) -> SqlResult2<T> {
    match remaining.split_first() {
        None => {
            let filters: Vec<QueryFilter<'_>> = owned
                .iter()
                .enumerate()
                .map(|(at, filter)| match filter {
                    OwnedFilter::Graph(graph) => {
                        QueryFilter::Graph(graph.request(&graph_edges[at], &graph_nodes[at]))
                    }
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
                with_boolean_filters(owned, rest, Some(&link), graph_edges, graph_nodes, k)
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
    Bm25 {
        index: IndexId,
        query: String,
        matching: TextMatch,
    },
    /// `col <=> v` in an arithmetic ranking is a DISTANCE, and the engine's
    /// leaf is a SIMILARITY (`-distance`). The lowering below writes the
    /// negation, so `1 - (col <=> v)` is the cosine itself, which is the same
    /// quantity `0.5 * (1 + VectorSimilarity)` names in `battle50k`.
    VectorDistance {
        index: IndexId,
        query: Vec<f32>,
        metric: VectorMetric,
    },
    Distance {
        index: IndexId,
        center: Point,
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
        OwnedScore::Bm25 {
            index,
            query,
            matching,
        } => k(&ScoreExpr::Bm25 {
            index: *index,
            query,
            matching: *matching,
        }),
        OwnedScore::VectorDistance {
            index,
            query,
            metric,
        } => {
            let similarity = ScoreExpr::VectorSimilarity {
                index: *index,
                query,
                metric: *metric,
            };
            k(&ScoreExpr::Neg(&similarity))
        }
        OwnedScore::Distance { index, center } => k(&ScoreExpr::Distance {
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

/// One `GRAPH_TABLE` pattern's traversal, with the per-hop predicates of
/// `docs/core/GRAPH_CONTRACT.md` §4.3 owned by the plan.
///
/// A `BfsRequest` borrows its predicate slices, and a compiled plan outlives
/// every statement text it was built from, so the plan holds the owned forms
/// and [`SelectPlan::with_query`] builds the borrowed ones on the stack for
/// the length of one prepared query -- the same shape the term strings and
/// query vectors already have.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OwnedGraph {
    pub(crate) seed: EntityId,
    pub(crate) direction: Direction,
    pub(crate) context: GraphContextId,
    pub(crate) edge_type: Option<EdgeTypeId>,
    pub(crate) min_depth: usize,
    pub(crate) max_depth: usize,
    pub(crate) edge_where: Vec<OwnedEdgePredicate>,
    pub(crate) node_where: Vec<OwnedFilter>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OwnedEdgePredicate {
    pub(crate) property: String,
    pub(crate) op: Cmp,
    pub(crate) value: Scalar,
}

impl OwnedEdgePredicate {
    fn borrowed(&self) -> EdgePredicate<'_> {
        EdgePredicate {
            property: &self.property,
            op: self.op,
            value: self.value.borrowed(),
        }
    }
}

impl OwnedGraph {
    pub(crate) fn request<'a>(
        &'a self,
        edge_where: &'a [EdgePredicate<'a>],
        node_where: &'a [QueryFilter<'a>],
    ) -> BfsRequest<'a> {
        BfsRequest {
            seed: self.seed,
            direction: self.direction,
            context: self.context,
            edge_type: self.edge_type,
            min_depth: self.min_depth,
            max_depth: self.max_depth,
            include_seed: false,
            max_visited: GRAPH_VISITED,
            max_edges: GRAPH_EDGES,
            result_limit: GRAPH_RESULTS,
            edge_where,
            node_where,
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
    },
    Bm25 {
        index: IndexId,
        query: String,
        matching: TextMatch,
    },
    ExactVector {
        index: IndexId,
        query: Vec<f32>,
        metric: VectorMetric,
    },
    ApproximateVector {
        index: IndexId,
        query: Vec<f32>,
        metric: VectorMetric,
        ef: usize,
    },
    Score {
        expr: OwnedScore,
        direction: SortDirection,
    },
    /// `ORDER BY <edge alias>` over a property of the edge the pattern bound.
    Edge {
        property: String,
        direction: SortDirection,
    },
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
        // A traversal's per-hop predicates are BORROWED by its `BfsRequest`,
        // so the borrowed forms are built here, on this call's stack, and the
        // request that names them cannot outlive them -- the same reason this
        // is a callback rather than a returned cursor.
        let graph_edges: Vec<Vec<EdgePredicate<'_>>> = self
            .filters
            .iter()
            .map(|filter| match filter {
                OwnedFilter::Graph(graph) => graph
                    .edge_where
                    .iter()
                    .map(OwnedEdgePredicate::borrowed)
                    .collect(),
                _ => Vec::new(),
            })
            .collect();
        let graph_nodes: Vec<Vec<QueryFilter<'_>>> = self
            .filters
            .iter()
            .map(|filter| match filter {
                OwnedFilter::Graph(graph) => {
                    graph.node_where.iter().map(OwnedFilter::borrowed).collect()
                }
                _ => Vec::new(),
            })
            .collect();
        // The boolean trees go on this call's stack for the same reason the
        // traversal's predicate slices do: a `QueryFilter::Any` names a
        // slice and a `Not` a reference, and a compiled plan outlives every
        // statement it was built from.
        with_borrowed_filters(&self.filters, &graph_edges, &graph_nodes, &mut |filters| {
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
            OwnedOrder::Distance { index, center } => run(QueryOrder::Distance {
                index: *index,
                center: *center,
                direction: SortDirection::Ascending,
            }),
            OwnedOrder::Bm25 {
                index,
                query,
                matching,
            } => run(QueryOrder::Bm25 {
                index: *index,
                query,
                matching: *matching,
            }),
            OwnedOrder::ExactVector {
                index,
                query,
                metric,
            } => run(QueryOrder::ExactVector {
                index: *index,
                query,
                metric: *metric,
            }),
            OwnedOrder::ApproximateVector {
                index,
                query,
                metric,
                ef,
            } => run(QueryOrder::ApproximateVector {
                index: *index,
                query,
                metric: *metric,
                ef: *ef,
            }),
            OwnedOrder::Score { expr, direction } => with_score(expr, &mut |compiled| {
                run(QueryOrder::Score {
                    expr: compiled,
                    direction: *direction,
                })
            }),
            OwnedOrder::Edge {
                property,
                direction,
            } => run(QueryOrder::Edge {
                property,
                direction: *direction,
            }),
        }
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CompiledIndex {
    Scalar { field: String, unique: bool },
    /// An EXPRESSION scalar index over `lower(field)`.
    LowerScalar { field: String },
    Text { field: String },
    Point { field: String },
    Geometry { field: String },
    ExactVector { field: String },
    QuantizedVector { field: String },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum WritePlan {
    Insert {
        collection: CollectionId,
        rows: Vec<(String, Value)>,
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
    CreateTable {
        name: String,
        fields: Vec<(String, Kind)>,
        /// The DECLARED spelling of the columns whose `Kind` does not carry
        /// it (`TIMESTAMPTZ`, `DATE`). Recorded in the catalog descriptor so
        /// a reopened database still knows a column is a timestamp and can
        /// print it back as an ISO string (QL_CONTRACT §4.2).
        declared: Vec<(String, String)>,
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
    /// `DROP TABLE [IF EXISTS] name [CASCADE|RESTRICT]`: the DROPPING mark,
    /// then bounded steps to the end. Nothing here is a second removal path --
    /// it is `begin_drop_collection` and `drop_collection_step`, the same
    /// atomics a caller writes by hand.
    DropTable {
        collection: CollectionId,
        name: String,
        mode: DropMode,
    },
    Begin,
    Commit,
    Rollback,
    /// A statement that changed nothing and said so.
    Notice(String),
}

impl WritePlan {
    pub(crate) fn run(self, db: &mut Database, notices: Vec<String>) -> SqlResult2<SqlResult> {
        let notice = |extra: String| -> SqlResult<> {
            let mut all = notices.clone();
            all.push(extra);
            SqlResult::Notice(all.join("; "))
        };
        Ok(match self {
            Self::Insert { collection, rows } => {
                let mut affected = 0u64;
                for (key, document) in rows {
                    db.put(collection, &key, &document)?;
                    affected += 1;
                }
                SqlResult::Affected(affected)
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
            Self::CreateTable {
                name,
                fields,
                declared,
            } => {
                db.create_collection_declared(
                    &name,
                    fields,
                    declared,
                    CollectionOptions::default(),
                )?;
                db.commit()?;
                SqlResult::Affected(0)
            }
            Self::CreateIndex {
                collection,
                name,
                method,
            } => {
                db.commit()?;
                let id = match &method {
                    CompiledIndex::Scalar { field, unique } => {
                        db.create_scalar_index(collection, &name, field, *unique)?
                    }
                    CompiledIndex::LowerScalar { field } => db.create_expression_index(
                        collection,
                        &name,
                        field,
                        IndexExpr::Lower,
                        false,
                    )?,
                    CompiledIndex::Text { field } => db.create_text_index(collection, &name, field)?,
                    CompiledIndex::Point { field } => {
                        db.create_point_index(collection, &name, field)?
                    }
                    CompiledIndex::Geometry { field } => {
                        db.create_geometry_index(collection, &name, field)?
                    }
                    CompiledIndex::ExactVector { field } => {
                        db.create_exact_vector_index(collection, &name, field)?
                    }
                    CompiledIndex::QuantizedVector { field } => {
                        db.create_quantized_vector_index(collection, &name, field)?
                    }
                };
                // Postgres hands back a usable index; so does this. The build
                // is incremental underneath (`build_index_step`), and it is
                // run to READY here rather than left half-built.
                db.commit()?;
                db.build_index_to_ready(id, 256)?;
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
                EF_SEARCH.with(|ef| ef.set(None));
                SqlResult::Affected(0)
            }
            Self::Rollback => {
                db.rollback()?;
                EF_SEARCH.with(|ef| ef.set(None));
                SqlResult::Affected(0)
            }
            Self::Notice(text) => notice(text),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Plan {
    Select(SelectPlan),
    Explain(SelectPlan),
    Aggregate(AggregatePlan),
    ExplainAggregate(AggregatePlan),
    Write(WritePlan),
    /// An EXPLAIN whose statement is not a query: the text is the plan, and
    /// nothing is run to produce it.
    ExplainText(String),
}

