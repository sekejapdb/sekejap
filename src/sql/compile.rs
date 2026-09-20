//! The AST, turned into calls the crate already has.
//!
//! Every SELECT becomes one `QueryRequest`; every write becomes `put`,
//! `update` or `delete`; every DDL statement becomes a `create_*` or a
//! catalog drop. Nothing here executes anything itself, and nothing here
//! invents a predicate the engine does not already have -- a shape with no
//! atomic is refused with the atomic named.
//!
//! The plan OWNS what the request borrows. A `QueryRequest` is a borrowed
//! view (`&str` terms, `&[f32]` query vectors, a `ScoreExpr` tree of
//! references), so the compiled form holds owned values and hands out the
//! borrowed view for the length of one page loop.

use super::ast::*;
use super::functions::{self, TimeUnit};
use super::{
    collection, is_key_column, order_value, projected, refuse, SqlError, SqlResult, SqlResult2,
    SqlRow, SqlValue, Tier, GRAPH_EDGES, GRAPH_RESULTS, GRAPH_VISITED, ID_COLUMN, KEY_COLUMN,
    MAX_GRAPH_DEPTH,
};
use crate::collections::{
    Accumulator, AggValue, AggregateFn, AggregateInput, AggregateRequest, BfsRequest,
    CandidateDriver, Cmp, CollectionId, CollectionOptions, Database, Direction, DropMode,
    DropPhase, EdgePredicate, EdgeTypeId, EntityId, Geom, GeometryFilter, GraphContextId,
    GroupCmp, GroupKey, GroupOrder, GroupPredicate, GroupRow, IndexExpr, IndexFamily, IndexId,
    IndexInfo, IndexState, OwnedScalarValue, PointFilter, ProjectedValue, Projection, QueryBudget,
    QueryFilter, QueryOrder, QueryRequest, QueryRow, ScalarFilter, ScalarValue, ScoreExpr,
    SortDirection, TextMatch, VectorMetric,
};
use crate::query::EDGE_FIELD_PREFIX;
use crate::spatial_math::{Bounds, Point};
use crate::Kind;
use serde_json::{Map, Value};
use std::{cell::Cell, ops::Bound};

/// The approximate shortlist width a vector order gets when the only index on
/// the column is quantized and no `SET LOCAL` named one. pgvector's own
/// default is 40; this is battle50k's `EF`, which is the number this tree's
/// approximate measurements are written against.
const DEFAULT_EF: usize = 100;

thread_local! {
    /// `SET LOCAL ef_search` / `diskann.query_search_list_size`, remembered
    /// between statements.
    ///
    /// `Database` has no session object -- a connection is a process here --
    /// so the session knob lives in the thread that ran the statement, and
    /// COMMIT or ROLLBACK clears it, which is what LOCAL means.
    static EF_SEARCH: Cell<Option<usize>> = const { Cell::new(None) };
}

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
const MAX_SEMI_JOIN_EDGES: usize = 64 << 20;

/// How many OUTER IDS one semi-join may name. Eight bytes each, so this is
/// the 8 MiB `RUN_BYTES` promise every other per-query buffer is written
/// against.
const MAX_SEMI_JOIN_IDS: usize = (8 << 20) / 8;

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
    /// (`docs/QL_CONTRACT.md` §3).
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
fn with_borrowed_filters<T>(
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
/// `docs/GRAPH_CONTRACT.md` §4.3 owned by the plan.
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
        body: &mut dyn FnMut(&mut crate::collections::PreparedQuery<'_>) -> SqlResult2<T>,
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

// ── the compiled aggregate ────────────────────────────────────────────────

/// Where one column of an aggregate's answer comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AggOutput {
    /// The group key itself.
    Key,
    /// One accumulator, by its position in the request.
    Value(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum OwnedAggInput {
    Index(IndexId),
    Field(String),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OwnedAccumulator {
    pub(crate) function: AggregateFn,
    pub(crate) input: Option<OwnedAggInput>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum OwnedGroupKey {
    Index(IndexId),
    Field(String),
    IndexDiv { index: IndexId, divisor: i64 },
}

/// A compiled `GROUP BY` / `DISTINCT` / aggregate statement. It owns what the
/// borrowed [`AggregateRequest`] points at, exactly as [`SelectPlan`] owns
/// what a `QueryRequest` points at.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AggregatePlan {
    pub(crate) collection: CollectionId,
    pub(crate) columns: Vec<String>,
    pub(crate) outputs: Vec<AggOutput>,
    pub(crate) filters: Vec<OwnedFilter>,
    pub(crate) group: Option<OwnedGroupKey>,
    pub(crate) accumulators: Vec<OwnedAccumulator>,
    pub(crate) having: Vec<GroupPredicate>,
    pub(crate) order: GroupOrder,
    pub(crate) driver: CandidateDriver,
    pub(crate) limit: Option<usize>,
    /// `Some(date_only)` when the group key is a declared TIMESTAMPTZ/DATE
    /// column. See [`group_key_value`].
    pub(crate) key_iso: Option<bool>,
    pub(crate) text: String,
}

/// An aggregate row is not a row of the collection: no entity produced it, so
/// there is no id to report. `SqlRow` carries one, so a group reports
/// sequence 0, which `Database::put` never issues (sequences are one-based).
fn group_identity(collection: CollectionId) -> EntityId {
    EntityId {
        collection,
        sequence: 0,
    }
}

fn agg_value(value: &AggValue) -> SqlValue {
    match value {
        AggValue::Count(n) => SqlValue::Int(*n as i64),
        AggValue::I64(v) => SqlValue::Int(*v),
        AggValue::F64(v) => SqlValue::Float(*v),
        AggValue::Text(v) => SqlValue::Text(v.clone()),
        AggValue::Bool(v) => SqlValue::Bool(*v),
        AggValue::Null => SqlValue::Null,
    }
}

/// The group key as the answer reports it.
///
/// `iso` is `Some(date_only)` when the key is a declared TIMESTAMPTZ or DATE
/// column: the declared type belongs to the column, so `GROUP BY born_ts`
/// prints the same ISO text `SELECT born_ts` does rather than the decimal of
/// its microseconds. A DIVIDED key (`GROUP BY col / n`) is a bucket number
/// and not an instant, so it stays an integer and never reaches here as
/// `Some`.
fn group_key_value(value: Option<&OwnedScalarValue>, iso: Option<bool>) -> SqlValue {
    match value {
        None | Some(OwnedScalarValue::Nullish) => SqlValue::Null,
        Some(OwnedScalarValue::Bool(v)) => SqlValue::Bool(*v),
        Some(OwnedScalarValue::I64(v)) => match iso {
            Some(true) => SqlValue::Text(functions::format_date(*v)),
            Some(false) => SqlValue::Text(functions::format_timestamp(*v)),
            None => SqlValue::Int(*v),
        },
        Some(OwnedScalarValue::F64(v)) => SqlValue::Float(*v),
        Some(OwnedScalarValue::Text(v)) => SqlValue::Text(v.clone()),
    }
}

impl AggregatePlan {
    pub(crate) fn row(&self, group: &GroupRow) -> SqlRow {
        let values = self
            .outputs
            .iter()
            .map(|output| match output {
                AggOutput::Key => group_key_value(group.key.as_ref(), self.key_iso),
                AggOutput::Value(at) => group
                    .values
                    .get(*at)
                    .map_or(SqlValue::Missing, agg_value),
            })
            .collect();
        SqlRow {
            id: group_identity(self.collection),
            values,
        }
    }

    /// Prepare the aggregate this plan compiled to and hand it to `body`.
    pub(crate) fn with_aggregate<T>(
        &self,
        db: &Database,
        body: &mut dyn FnMut(&mut crate::collections::PreparedAggregate<'_>) -> SqlResult2<T>,
    ) -> SqlResult2<T> {
        let accumulators: Vec<Accumulator<'_>> = self
            .accumulators
            .iter()
            .map(|accumulator| Accumulator {
                function: accumulator.function,
                input: accumulator.input.as_ref().map(|input| match input {
                    OwnedAggInput::Index(index) => AggregateInput::Index(*index),
                    OwnedAggInput::Field(field) => AggregateInput::Field(field.as_str()),
                }),
            })
            .collect();
        let group = self.group.as_ref().map(|group| match group {
            OwnedGroupKey::Index(index) => GroupKey::Index(*index),
            OwnedGroupKey::Field(field) => GroupKey::Field(field.as_str()),
            OwnedGroupKey::IndexDiv { index, divisor } => GroupKey::IndexDiv {
                index: *index,
                divisor: *divisor,
            },
        });
        with_borrowed_filters(&self.filters, &[], &[], &mut |filters| {
            let mut prepared = db.prepare_aggregate(AggregateRequest {
                collection: self.collection,
                filters,
                group,
                accumulators: &accumulators,
                having: &self.having,
                order: self.order,
                driver: self.driver,
                total_limit: self.limit,
            })?;
            body(&mut prepared)
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
                    crate::collections::MAX_DROP_BATCH,
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

// ── row functions over projected values (QL_CONTRACT §4.1, §4.2) ──────────

/// A row expression with every name resolved and every constant folded.
///
/// `Field(at)` is a position in the plan's `Projection::Fields` list, so
/// evaluating one costs a read of a value the page already produced: the cost
/// is proportional to the rows RETURNED, which is what §4.1 and §4.2 promise
/// and what `EXPLAIN` prints under "row functions".
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CompiledRow {
    /// The n-th projected field, with the declared spelling that decides how
    /// a stored integer prints.
    Field { at: usize, time: bool },
    Lit(SqlValue),
    /// The clock, already folded: every row of one answer sees one instant.
    Micros(i64),
    Extract { unit: TimeUnit, arg: Box<CompiledRow> },
    Trunc { unit: TimeUnit, arg: Box<CompiledRow> },
    Age { left: Box<CompiledRow>, right: Box<CompiledRow> },
    ToChar { arg: Box<CompiledRow>, format: String },
    ToTimestamp(Box<CompiledRow>),
    ToDate(Box<CompiledRow>),
    CastDate(Box<CompiledRow>),
    CastText(Box<CompiledRow>),
    /// A declared TIMESTAMPTZ/DATE printed back as an ISO-8601 string.
    Iso { arg: Box<CompiledRow>, date_only: bool },
    Str { func: StrFunc, args: Vec<CompiledRow> },
    Add(Box<CompiledRow>, Box<CompiledRow>),
    Sub(Box<CompiledRow>, Box<CompiledRow>),
    Concat(Box<CompiledRow>, Box<CompiledRow>),
}

/// NULL propagates the way SQL says it does: any NULL or MISSING input makes
/// the whole expression NULL, and no function is called on it.
fn nullish(value: &SqlValue) -> bool {
    matches!(value, SqlValue::Null | SqlValue::Missing)
}

fn want_int(value: &SqlValue, what: &str) -> SqlResult2<i64> {
    match value {
        SqlValue::Int(n) => Ok(*n),
        SqlValue::Float(f) if f.fract() == 0.0 => Ok(*f as i64),
        other => Err(SqlError::Parameter(format!(
            "{what} takes a whole number and the row holds {other:?}"
        ))),
    }
}

fn want_text(value: &SqlValue, what: &str) -> SqlResult2<String> {
    match value {
        SqlValue::Text(text) => Ok(text.clone()),
        SqlValue::Int(n) => Ok(n.to_string()),
        SqlValue::Float(f) => Ok(f.to_string()),
        SqlValue::Bool(b) => Ok(if *b { "true" } else { "false" }.to_owned()),
        other => Err(SqlError::Parameter(format!(
            "{what} takes text and the row holds {other:?}"
        ))),
    }
}

impl CompiledRow {
    /// One value, from one row's projected values. Reads no other row.
    pub(crate) fn eval(&self, values: &[SqlValue]) -> SqlResult2<SqlValue> {
        Ok(match self {
            Self::Field { at, time } => {
                let value = values.get(*at).cloned().unwrap_or(SqlValue::Missing);
                let _ = time;
                value
            }
            Self::Lit(value) => value.clone(),
            Self::Micros(n) => SqlValue::Int(*n),
            Self::Extract { unit, arg } => {
                let value = arg.eval(values)?;
                if nullish(&value) {
                    return Ok(SqlValue::Null);
                }
                SqlValue::Int(functions::extract(
                    *unit,
                    want_int(&value, "EXTRACT(... FROM t)")?,
                ))
            }
            Self::Trunc { unit, arg } => {
                let value = arg.eval(values)?;
                if nullish(&value) {
                    return Ok(SqlValue::Null);
                }
                SqlValue::Int(functions::date_trunc(
                    *unit,
                    want_int(&value, "date_trunc(u, t)")?,
                )?)
            }
            Self::Age { left, right } => {
                let (a, b) = (left.eval(values)?, right.eval(values)?);
                if nullish(&a) || nullish(&b) {
                    return Ok(SqlValue::Null);
                }
                SqlValue::Int(
                    want_int(&a, "age(a, b)")?.saturating_sub(want_int(&b, "age(a, b)")?),
                )
            }
            Self::ToChar { arg, format } => {
                let value = arg.eval(values)?;
                if nullish(&value) {
                    return Ok(SqlValue::Null);
                }
                SqlValue::Text(functions::to_char(want_int(&value, "to_char(t, f)")?, format)?)
            }
            Self::ToTimestamp(arg) => {
                let value = arg.eval(values)?;
                if nullish(&value) {
                    return Ok(SqlValue::Null);
                }
                match value {
                    // `to_timestamp(seconds)` per Postgres; a text argument is
                    // the literal reader, which is `to_date`'s job but the
                    // same grammar.
                    SqlValue::Text(text) => SqlValue::Int(functions::parse_timestamp(&text)?),
                    other => SqlValue::Int(
                        want_int(&other, "to_timestamp(seconds)")?
                            .saturating_mul(functions::MICROS_PER_SECOND),
                    ),
                }
            }
            Self::ToDate(arg) | Self::CastDate(arg) => {
                let value = arg.eval(values)?;
                if nullish(&value) {
                    return Ok(SqlValue::Null);
                }
                let micros = match value {
                    SqlValue::Text(text) => functions::parse_timestamp(&text)?,
                    other => want_int(&other, "to_date(t)")?,
                };
                SqlValue::Int(functions::date_trunc(TimeUnit::Day, micros)?)
            }
            Self::CastText(arg) => {
                let value = arg.eval(values)?;
                if nullish(&value) {
                    return Ok(SqlValue::Null);
                }
                SqlValue::Text(want_text(&value, "::text")?)
            }
            Self::Iso { arg, date_only } => {
                let value = arg.eval(values)?;
                if nullish(&value) {
                    return Ok(SqlValue::Null);
                }
                let micros = want_int(&value, "a declared timestamp")?;
                SqlValue::Text(if *date_only {
                    functions::format_date(micros)
                } else {
                    functions::format_timestamp(micros)
                })
            }
            Self::Str { func, args } => {
                let mut evaluated = Vec::with_capacity(args.len());
                for arg in args {
                    let value = arg.eval(values)?;
                    // `concat` is the one Postgres function that IGNORES
                    // NULLs rather than propagating them, and it is the one
                    // exception here too.
                    if nullish(&value) && *func != StrFunc::Concat {
                        return Ok(SqlValue::Null);
                    }
                    evaluated.push(value);
                }
                return string_function(*func, &evaluated);
            }
            Self::Add(a, b) | Self::Sub(a, b) => {
                let (x, y) = (a.eval(values)?, b.eval(values)?);
                if nullish(&x) || nullish(&y) {
                    return Ok(SqlValue::Null);
                }
                let (x, y) = (want_int(&x, "date arithmetic")?, want_int(&y, "date arithmetic")?);
                SqlValue::Int(if matches!(self, Self::Add(_, _)) {
                    x.saturating_add(y)
                } else {
                    x.saturating_sub(y)
                })
            }
            Self::Concat(a, b) => {
                let (x, y) = (a.eval(values)?, b.eval(values)?);
                // `||` propagates NULL, unlike `concat`.
                if nullish(&x) || nullish(&y) {
                    return Ok(SqlValue::Null);
                }
                SqlValue::Text(format!("{}{}", want_text(&x, "||")?, want_text(&y, "||")?))
            }
        })
    }
}

/// The §4.1 string functions, over already-evaluated arguments.
fn string_function(func: StrFunc, args: &[SqlValue]) -> SqlResult2<SqlValue> {
    let arity = |want: std::ops::RangeInclusive<usize>| -> SqlResult2<()> {
        if want.contains(&args.len()) {
            Ok(())
        } else {
            Err(SqlError::unsupported(format!(
                "{}() takes {}..={} arguments and was given {}",
                func.written(),
                want.start(),
                want.end(),
                args.len()
            )))
        }
    };
    let text = |at: usize| want_text(&args[at], func.written());
    let int = |at: usize| want_int(&args[at], func.written());
    Ok(match func {
        StrFunc::Lower => {
            arity(1..=1)?;
            SqlValue::Text(text(0)?.to_lowercase())
        }
        StrFunc::Upper => {
            arity(1..=1)?;
            SqlValue::Text(text(0)?.to_uppercase())
        }
        StrFunc::Length => {
            arity(1..=1)?;
            SqlValue::Int(functions::length(&text(0)?))
        }
        StrFunc::Trim => {
            arity(1..=1)?;
            SqlValue::Text(text(0)?.trim().to_owned())
        }
        StrFunc::Concat => {
            let mut out = String::new();
            for (at, value) in args.iter().enumerate() {
                if nullish(value) {
                    continue;
                }
                out.push_str(&want_text(value, func.written()).map_err(|_| {
                    SqlError::Parameter(format!("concat() argument {} is not text", at + 1))
                })?);
            }
            SqlValue::Text(out)
        }
        StrFunc::Substring => {
            arity(2..=3)?;
            let count = if args.len() == 3 { Some(int(2)?) } else { None };
            SqlValue::Text(functions::substring(&text(0)?, int(1)?, count)?)
        }
        StrFunc::Left => {
            arity(2..=2)?;
            SqlValue::Text(functions::left(&text(0)?, int(1)?))
        }
        StrFunc::Right => {
            arity(2..=2)?;
            SqlValue::Text(functions::right(&text(0)?, int(1)?))
        }
        StrFunc::SplitPart => {
            arity(3..=3)?;
            SqlValue::Text(functions::split_part(&text(0)?, &text(1)?, int(2)?)?)
        }
        StrFunc::Replace => {
            arity(3..=3)?;
            SqlValue::Text(text(0)?.replace(&text(1)?, &text(2)?))
        }
        StrFunc::Position => {
            arity(2..=2)?;
            SqlValue::Int(functions::position(&text(0)?, &text(1)?))
        }
        StrFunc::StartsWith => {
            arity(2..=2)?;
            SqlValue::Bool(text(0)?.starts_with(&text(1)?))
        }
    })
}

// ── the compiler ──────────────────────────────────────────────────────────

struct Compiler<'a> {
    db: &'a Database,
    params: &'a [super::Param],
    notices: &'a mut Vec<String>,
    /// The index registry of each collection this statement names, read
    /// ONCE per statement. `list_indexes` is a catalog walk (one range plus
    /// several point reads per index); a statement with five predicates on
    /// one collection used to pay it five times.
    index_lists: std::cell::RefCell<Vec<(CollectionId, Vec<IndexInfo>)>>,
    /// The budget and the cancellation this statement's caller handed in.
    ///
    /// COMPILING is not free here: a semi-join's set is built while the
    /// statement is compiled, and it is an index walk or a whole inner query.
    /// Both used to run under `QueryBudget::unlimited()` and a cancel closure
    /// that always said no, so `Ctrl-C` was inert and no resource was charged
    /// for work the caller had asked to bound.
    budget: QueryBudget,
    cancelled: &'a mut dyn FnMut() -> bool,
    /// One line per WHERE function folded into an index range, and one per
    /// projected row function. They are the two EXPLAIN sections
    /// `docs/QL_CONTRACT.md` §4.1 and §4.2 ask for: a rewrite is index-side
    /// and costs candidates, a row function is per RETURNED row.
    rewrites: Vec<String>,
    row_functions: Vec<String>,
    /// `now()` and `current_date`, folded ONCE for the whole statement.
    clock: i64,
}

pub(crate) fn compile(
    db: &Database,
    statement: Stmt,
    params: &[super::Param],
    notices: &mut Vec<String>,
    budget: QueryBudget,
    cancelled: &mut dyn FnMut() -> bool,
) -> SqlResult2<Plan> {
    let mut compiler = Compiler {
        index_lists: std::cell::RefCell::new(Vec::new()),
        rewrites: Vec::new(),
        row_functions: Vec::new(),
        clock: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX)),
        db,
        params,
        notices,
        budget,
        cancelled,
    };
    compiler.statement(statement)
}

impl Compiler<'_> {
    fn statement(&mut self, statement: Stmt) -> SqlResult2<Plan> {
        Ok(match statement {
            Stmt::Select(select) => match self.aggregate(&select)? {
                Some(plan) => Plan::Aggregate(plan),
                None => Plan::Select(self.select(*select)?),
            },
            Stmt::Explain(select) => match self.aggregate(&select)? {
                Some(plan) => Plan::ExplainAggregate(plan),
                None => Plan::Explain(self.select(*select)?),
            },
            Stmt::Insert {
                table,
                columns,
                rows,
            } => Plan::Write(self.insert(&table, &columns, &rows)?),
            Stmt::Update {
                table,
                assignments,
                key,
            } => Plan::Write(self.update(&table, &assignments, &key)?),
            Stmt::Delete { table, key } => {
                let collection = collection(self.db, &table)?;
                let key = self.text_of(&key)?;
                Plan::Write(WritePlan::Delete { collection, key })
            }
            Stmt::CreateTable { table, columns } => Plan::Write(self.create_table(table, columns)?),
            Stmt::CreateIndex {
                name,
                table,
                method,
            } => Plan::Write(self.create_index(name, &table, method)?),
            Stmt::DropTable {
                table,
                if_exists,
                cascade,
            } => match self.drop_table(&table, if_exists, cascade)? {
                Some(plan) => Plan::Write(plan),
                None => Plan::Write(WritePlan::Notice(format!(
                    "DROP TABLE IF EXISTS {table}: no such collection"
                ))),
            },
            Stmt::ExplainDropTable {
                table,
                if_exists,
                cascade,
            } => Plan::ExplainText(self.explain_drop_table(&table, if_exists, cascade)?),
            Stmt::DropIndex { name, if_exists } => {
                let Some(index) = self.index_named(&name)? else {
                    if if_exists {
                        return Ok(Plan::Write(WritePlan::Notice(format!(
                            "DROP INDEX IF EXISTS {name}: no such index"
                        ))));
                    }
                    return Err(SqlError::engine(format!("no index named `{name}`")));
                };
                Plan::Write(WritePlan::DropIndex { index, name })
            }
            Stmt::Begin => Plan::Write(WritePlan::Begin),
            Stmt::Commit => Plan::Write(WritePlan::Commit),
            Stmt::Rollback => Plan::Write(WritePlan::Rollback),
            Stmt::SetLocal { name, value } => Plan::Write(self.set_local(&name, &value)?),
        })
    }

    /// `DROP TABLE`. `Ok(None)` is `IF EXISTS` on a name that is not there.
    fn drop_table(
        &mut self,
        table: &str,
        if_exists: bool,
        cascade: bool,
    ) -> SqlResult2<Option<WritePlan>> {
        let found = self.db.collection(table).map_err(SqlError::from);
        let collection = match found {
            Ok(Some(id)) => id,
            Ok(None) if if_exists => return Ok(None),
            Ok(None) => {
                return Err(SqlError::engine(format!("no collection named `{table}`")))
            }
            Err(e) => return Err(e),
        };
        Ok(Some(WritePlan::DropTable {
            collection,
            name: table.to_owned(),
            mode: if cascade {
                DropMode::Cascade
            } else {
                DropMode::Restrict
            },
        }))
    }

    /// `EXPLAIN DROP TABLE`. The one EXPLAIN that does not run: it prints the
    /// phases, the bound each one honours and what the collection holds, and
    /// leaves the collection there.
    fn explain_drop_table(
        &mut self,
        table: &str,
        if_exists: bool,
        cascade: bool,
    ) -> SqlResult2<String> {
        let Some(plan) = self.drop_table(table, if_exists, cascade)? else {
            return Ok(format!(
                "drop: nothing -- IF EXISTS and no collection named `{table}`
"
            ));
        };
        let WritePlan::DropTable { collection, mode, .. } = plan else {
            unreachable!("drop_table builds only a DropTable plan")
        };
        let indexes = self.db.list_indexes(collection).map_err(SqlError::from)?;
        let mut out = String::new();
        out.push_str(&format!(
            "statement: DROP TABLE {table} {}
",
            match mode {
                DropMode::Cascade => "CASCADE",
                DropMode::Restrict => "RESTRICT (the default)",
            }
        ));
        out.push_str(
            "note:  this EXPLAIN does not run its statement. Every other EXPLAIN here runs, because a plan printed without running says nothing about the counters; running a DROP would be the drop.
",
        );
        out.push_str(&format!(
            "mark:  begin_drop_collection publishes DROPPING in the catalog descriptor and commits it before one entry is removed (Law 3); {} refuses while any graph edge in any context references a row of `{table}`, naming those contexts
",
            match mode {
                DropMode::Cascade => "CASCADE does not refuse -- RESTRICT",
                DropMode::Restrict => "RESTRICT",
            }
        ));
        out.push_str("phases:
");
        for phase in [
            DropPhase::Indexes,
            DropPhase::Sidecars,
            DropPhase::Rows,
            DropPhase::Mappings,
            DropPhase::Descriptor,
        ] {
            let detail = match phase {
                DropPhase::Indexes => format!(
                    "{} index(es): {}",
                    indexes.len(),
                    if indexes.is_empty() {
                        "none".to_owned()
                    } else {
                        indexes
                            .iter()
                            .map(|i| i.name.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                ),
                DropPhase::Sidecars => "prefix 0x60 | collection -- vector cells".to_owned(),
                DropPhase::Rows => format!(
                    "prefix 0x40 | collection -- primary rows{}",
                    if mode == DropMode::Cascade {
                        ", each row's incident edges first through cascade_graph_delete (at most 256 per row)"
                    } else {
                        ""
                    }
                ),
                DropPhase::Mappings => {
                    "prefix 0x20 | collection -- the external-key mapping".to_owned()
                }
                DropPhase::Descriptor => {
                    "name, catalog replicas, sequence replicas, layout replicas -- after a range probe proves every keyspace above is empty".to_owned()
                }
            };
            out.push_str(&format!("  {} -- {detail}
", phase.name()));
        }
        out.push_str(&format!(
            "bound: drop_collection_step(id, budget) removes at most `budget` entries per step, budget in 1..={}; the committed cursor is the phase byte in the descriptor plus the surviving keys, so a crash resumes without a scan
",
            crate::collections::MAX_DROP_BATCH
        ));
        Ok(out)
    }

    fn set_local(&mut self, name: &str, value: &Literal) -> SqlResult2<WritePlan> {
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            "ef_search" | "hnsw.ef_search" | "diskann.query_search_list_size" => {
                // `= DEFAULT` clears the knob, which is what RESET means and
                // what COMMIT does at the end of a transaction.
                if matches!(value, Literal::Str(text) if text.eq_ignore_ascii_case("default")) {
                    EF_SEARCH.with(|cell| cell.set(None));
                    return Ok(WritePlan::Notice(format!(
                        "SET LOCAL {name} = DEFAULT: the approximate shortlist bound is cleared, so a vector order takes the exact family when the column has one"
                    )));
                }
                let ef = self.i64_of(value)?;
                if ef <= 0 {
                    return Err(SqlError::unsupported(format!(
                        "SET LOCAL {name} = {ef}: an approximate shortlist has a positive width"
                    )));
                }
                EF_SEARCH.with(|cell| cell.set(Some(ef as usize)));
                Ok(WritePlan::Notice(format!(
                    "SET LOCAL {name} = {ef}: read as `ef`, the approximate shortlist bound of QueryOrder::ApproximateVector (QL_CONTRACT §4.5; the two knobs are not the same algorithm -- battle50k deviation 12)"
                )))
            }
            _ => Ok(WritePlan::Notice(format!(
                "SET LOCAL {name}: no such knob here, and nothing was changed. The planner has no enable_indexscan / enable_bitmapscan: a query's driver is chosen from the indexes the statement's own predicates name (`src/query/drivers.rs`), and exactness is which vector index the column has, not a planner setting"
            ))),
        }
    }

    // ── values ───────────────────────────────────────────────────────────

    fn param(&self, n: usize) -> SqlResult2<&super::Param> {
        self.params.get(n - 1).ok_or_else(|| {
            SqlError::Parameter(format!(
                "${n} is not bound; {} parameter(s) were given",
                self.params.len()
            ))
        })
    }

    /// A literal, as JSON. A subquery runs here, at compile time, because by
    /// the time the outer statement runs it is a constant.
    fn value_of(&self, literal: &Literal) -> SqlResult2<Value> {
        Ok(match literal {
            Literal::Null => Value::Null,
            Literal::Bool(b) => Value::Bool(*b),
            Literal::Num(v, exact) => {
                if *exact && v.fract() == 0.0 && v.abs() < 9.0e18 {
                    Value::from(*v as i64)
                } else {
                    Value::from(*v)
                }
            }
            Literal::Str(s) => Value::String(s.clone()),
            Literal::Param(n) => match self.param(*n)? {
                super::Param::Null => Value::Null,
                super::Param::Bool(b) => Value::Bool(*b),
                super::Param::Int(i) => Value::from(*i),
                super::Param::Float(f) => Value::from(*f),
                super::Param::Text(t) => Value::String(t.clone()),
                super::Param::Vector(v) => Value::from(v.clone()),
                super::Param::Json(v) => v.clone(),
            },
            Literal::Subquery(query) => {
                let collection = collection(self.db, &query.table)?;
                let key = self.text_of(&query.key)?;
                let entity = self
                    .db
                    .get(collection, &key)
                    .map_err(SqlError::from)?
                    .ok_or_else(|| {
                        SqlError::engine(format!(
                            "scalar subquery: `{}` has no row at key `{key}`",
                            query.table
                        ))
                    })?;
                if query.column == KEY_COLUMN {
                    Value::String(entity.key)
                } else {
                    entity
                        .document
                        .get(&query.column)
                        .cloned()
                        .unwrap_or(Value::Null)
                }
            }
        })
    }

    fn text_of(&self, literal: &Literal) -> SqlResult2<String> {
        match self.value_of(literal)? {
            Value::String(s) => Ok(s),
            other => Err(SqlError::Parameter(format!(
                "expected text, found {other}"
            ))),
        }
    }

    fn f64_of(&self, literal: &Literal) -> SqlResult2<f64> {
        match self.value_of(literal)? {
            Value::Number(n) => n
                .as_f64()
                .ok_or_else(|| SqlError::Parameter("number is not finite".into())),
            other => Err(SqlError::Parameter(format!(
                "expected a number, found {other}"
            ))),
        }
    }

    fn i64_of(&self, literal: &Literal) -> SqlResult2<i64> {
        match self.value_of(literal)? {
            Value::Number(n) => n
                .as_i64()
                .ok_or_else(|| SqlError::Parameter("expected a whole number".into())),
            other => Err(SqlError::Parameter(format!(
                "expected a whole number, found {other}"
            ))),
        }
    }

    /// pgvector's text form `[a,b,c]`, a JSON array, or a bound
    /// `Param::Vector`.
    fn vector_of(&self, literal: &Literal) -> SqlResult2<Vec<f32>> {
        if let Literal::Param(n) = literal {
            if let super::Param::Vector(v) = self.param(*n)? {
                return Ok(v.clone());
            }
        }
        match self.value_of(literal)? {
            Value::String(text) => parse_vector_literal(&text),
            Value::Array(items) => items
                .iter()
                .map(|item| {
                    item.as_f64()
                        .map(|v| v as f32)
                        .ok_or_else(|| SqlError::Parameter("vector holds a non-number".into()))
                })
                .collect(),
            other => Err(SqlError::Parameter(format!(
                "expected a vector literal, found {other}"
            ))),
        }
    }

    fn point_of(&self, point: &PointArg) -> SqlResult2<Point> {
        let lon = self.f64_of(&point.lon)?;
        let lat = self.f64_of(&point.lat)?;
        Point::new(lon, lat).map_err(|e| SqlError::engine(format!("ST_MakePoint({lon}, {lat}): {e}")))
    }

    fn geom_of(&self, argument: &GeoArg) -> SqlResult2<Geom> {
        Ok(match argument {
            GeoArg::Point(point) => {
                let p = self.point_of(point)?;
                Geom::Point(p.longitude(), p.latitude())
            }
            GeoArg::Envelope {
                minlon,
                minlat,
                maxlon,
                maxlat,
            } => {
                let (w, s, e, n) = (
                    self.f64_of(minlon)?,
                    self.f64_of(minlat)?,
                    self.f64_of(maxlon)?,
                    self.f64_of(maxlat)?,
                );
                Geom::Polygon(vec![vec![[w, s], [e, s], [e, n], [w, n], [w, s]]])
            }
            GeoArg::GeoJson(literal) => {
                let value = self.value_of(literal)?;
                let document = match value {
                    Value::String(text) => serde_json::from_str::<Value>(&text)
                        .map_err(|e| SqlError::Parameter(format!("GeoJSON: {e}")))?,
                    other => other,
                };
                geom_from_json(&document)?
            }
        })
    }

    fn bounds_of(&self, argument: &GeoArg) -> SqlResult2<Bounds> {
        match argument {
            GeoArg::Envelope {
                minlon,
                minlat,
                maxlon,
                maxlat,
            } => {
                let (w, s, e, n) = (
                    self.f64_of(minlon)?,
                    self.f64_of(minlat)?,
                    self.f64_of(maxlon)?,
                    self.f64_of(maxlat)?,
                );
                Bounds::new(w, e, s, n)
                    .map_err(|err| SqlError::engine(format!("ST_MakeEnvelope: {err}")))
            }
            _ => Err(SqlError::unsupported(
                "a rectangle over a Point column is ST_Within(col, ST_MakeEnvelope(...)): PointFilter::Bbox is a lon/lat rectangle and has no other shape",
            )),
        }
    }

    // ── the catalog ──────────────────────────────────────────────────────

    fn kind_of(&self, c: CollectionId, field: &str) -> SqlResult2<Kind> {
        let info = self.db.collection_info(c).map_err(SqlError::from)?;
        info.layout
            .fields
            .iter()
            .find(|(name, _)| name == field)
            .map(|(_, kind)| kind.clone())
            .ok_or_else(|| SqlError::engine(format!("no column `{field}` in this collection")))
    }

    /// The DECLARED SQL spelling of `field`, when the catalog records one.
    ///
    /// `TIMESTAMPTZ` and `DATE` are both `Kind::Int` (UTC microseconds,
    /// §5 deviation 8), so this is what tells a date/time rewrite from an
    /// ordinary integer comparison, and what makes a projected column print
    /// back as an ISO string.
    fn declared_of(&self, c: CollectionId, field: &str) -> SqlResult2<Option<String>> {
        let info = self.db.collection_info(c).map_err(SqlError::from)?;
        Ok(info
            .declared
            .iter()
            .find(|(name, _)| name == field)
            .map(|(_, declared)| declared.clone()))
    }

    /// `Some(declared)` when `field` is a declared TIMESTAMPTZ/DATE stored as
    /// `Kind::Int`.
    fn time_column(&self, c: CollectionId, field: &str) -> SqlResult2<Option<String>> {
        if !matches!(self.kind_of(c, field)?, Kind::Int) {
            return Ok(None);
        }
        Ok(self
            .declared_of(c, field)?
            .filter(|declared| functions::is_time_type(declared)))
    }

    fn declared_fields(&self, c: CollectionId) -> SqlResult2<Vec<String>> {
        let info = self.db.collection_info(c).map_err(SqlError::from)?;
        Ok(info
            .layout
            .fields
            .iter()
            .map(|(name, _)| name.clone())
            .filter(|name| !name.starts_with("__e4"))
            .collect())
    }

    /// The READY index of `family` over `field`, or an error that names what
    /// is missing. An index that exists but is still BUILDING is named too:
    /// the fix is different.
    fn index_for(
        &self,
        c: CollectionId,
        field: &str,
        family: IndexFamily,
        what: &str,
    ) -> SqlResult2<IndexId> {
        self.index_for_expression(c, field, family, None, what)
    }

    /// The same lookup, matching the index's EXPRESSION as well as its field.
    ///
    /// An expression index records the SOURCE field, so `kind = 'Home'` and
    /// `lower(kind) = 'home'` both name `kind` and must not pick each other's
    /// index: the first would be answered from keys that hold `'home'`, the
    /// second from keys that hold `'Home'`. The expression is part of the
    /// identity of the index a predicate names.
    fn index_for_expression(
        &self,
        c: CollectionId,
        field: &str,
        family: IndexFamily,
        expression: Option<IndexExpr>,
        what: &str,
    ) -> SqlResult2<IndexId> {
        let mut lists = self.index_lists.borrow_mut();
        let position = match lists.iter().position(|(id, _)| *id == c) {
            Some(position) => position,
            None => {
                lists.push((c, self.db.list_indexes(c).map_err(SqlError::from)?));
                lists.len() - 1
            }
        };
        let indexes = &lists[position].1;
        let mut building = false;
        for info in indexes {
            if info.field == field && info.family == family && info.expression == expression {
                match info.state {
                    IndexState::Ready => return Ok(info.id),
                    IndexState::Building { .. } => building = true,
                    IndexState::Dropping => {}
                }
            }
        }
        Err(SqlError::engine(if building {
            format!("{what} on `{field}` is still building; run it to READY before a query can use it")
        } else {
            format!(
                "{what} on `{field}` does not exist. QL_CONTRACT §6: every Tier-1 predicate on an indexed field is answered index-side, so the predicate compiles to a filter that NAMES an index; without one there is nothing to name"
            )
        }))
    }

    fn index_named(&self, name: &str) -> SqlResult2<Option<IndexId>> {
        for n in 1..=64u64 {
            if let Ok(info) = self.db.index_info(IndexId(n)) {
                if info.name == name {
                    return Ok(Some(info.id));
                }
            }
        }
        Ok(None)
    }

    // ── SELECT ───────────────────────────────────────────────────────────

    fn select(&mut self, statement: SelectStmt) -> SqlResult2<SelectPlan> {
        let (c, graph_filter, graph_columns) = match &statement.source {
            Source::Table(name) => (collection(self.db, name)?, None, None),
            Source::Graph(graph) => {
                let (target, filter) = self.graph_table(graph)?;
                (target, Some(filter), Some(graph.columns.clone()))
            }
        };

        let mut filters: Vec<OwnedFilter> = Vec::new();
        if let Some(filter) = graph_filter {
            filters.push(filter);
        }
        for expr in &statement.predicates {
            filters.push(self.where_filter(c, expr)?);
        }

        // An alias a `COLUMNS` entry gave to an EDGE property. `ORDER BY` and
        // the select list resolve against this before they look for a column
        // of the far node, because the two namespaces are distinct and the
        // pattern is what bound the edge one.
        let edge_aliases: Vec<(String, String)> = graph_columns
            .iter()
            .flatten()
            .filter_map(|(item, alias)| match item {
                GraphColumn::Edge(property) => Some((alias.clone(), property.clone())),
                GraphColumn::Node(_) => None,
            })
            .collect();

        let order = match &statement.order {
            None => OwnedOrder::Driver,
            Some(OrderKey::Column { column, descending }) => {
                match edge_aliases
                    .iter()
                    .find(|(alias, _)| alias == column)
                    .map(|(_, property)| property.clone())
                {
                    Some(property) => OwnedOrder::Edge {
                        property,
                        direction: if *descending {
                            SortDirection::Descending
                        } else {
                            SortDirection::Ascending
                        },
                    },
                    None => self.order(c, &OrderKey::Column {
                        column: column.clone(),
                        descending: *descending,
                    })?,
                }
            }
            Some(key) => self.order(c, key)?,
        };

        // The select list. `_id` is free (a row carries its id); a named
        // column is a projected field; an expression is this statement's own
        // ranking value.
        // `COLUMNS` entries that read the EDGE become projection fields under
        // the `@edge.` spelling the engine resolves from the traversal; the
        // rest are ordinary select items over the far node's row. Both keep
        // the position the statement wrote them in.
        let items: Vec<(GraphColumn, Option<String>)> = match graph_columns {
            Some(columns) => columns
                .into_iter()
                .map(|(item, alias)| (item, Some(alias)))
                .collect(),
            None => statement
                .items
                .clone()
                .into_iter()
                .map(|(item, alias)| (GraphColumn::Node(item), alias))
                .collect(),
        };
        let mut columns = Vec::new();
        let mut outputs = Vec::new();
        let mut fields: Vec<String> = Vec::new();
        let mut functions: Vec<CompiledRow> = Vec::new();
        let push_field = |field: String, fields: &mut Vec<String>| -> Output {
            match fields.iter().position(|existing| *existing == field) {
                Some(at) => Output::Field(at),
                None => {
                    fields.push(field);
                    Output::Field(fields.len() - 1)
                }
            }
        };
        for (item, alias) in items {
            let item = match item {
                GraphColumn::Node(item) => item,
                GraphColumn::Edge(property) => {
                    // `@edge.<property>` is the engine's projection spelling
                    // for the reaching edge (`query::EDGE_FIELD_PREFIX`); it
                    // reads no row and collides with no declared field.
                    let field = format!("{EDGE_FIELD_PREFIX}{property}");
                    columns.push(alias.unwrap_or_else(|| property.clone()));
                    outputs.push(push_field(field, &mut fields));
                    continue;
                }
            };
            match item {
                SelectItem::Star => {
                    for field in self.declared_fields(c)? {
                        columns.push(field.clone());
                        // `SELECT *` prints a declared TIMESTAMPTZ/DATE the
                        // same way `SELECT born_ts` does: the declared type
                        // belongs to the COLUMN, not to the spelling that
                        // named it, so the two cannot disagree.
                        match self.time_column(c, &field)? {
                            None => outputs.push(push_field(field, &mut fields)),
                            Some(declared) => {
                                let Output::Field(at) = push_field(field.clone(), &mut fields)
                                else {
                                    unreachable!("push_field returns a field position");
                                };
                                self.row_functions.push(format!(
                                    "{field} -> ISO-8601 text (declared {declared}, stored Int microseconds)"
                                ));
                                functions.push(CompiledRow::Iso {
                                    arg: Box::new(CompiledRow::Field { at, time: true }),
                                    date_only: declared == "DATE",
                                });
                                outputs.push(Output::Row(functions.len() - 1));
                            }
                        }
                    }
                }
                SelectItem::Id => {
                    columns.push(alias.clone().unwrap_or_else(|| ID_COLUMN.to_owned()));
                    outputs.push(Output::Id);
                }
                SelectItem::Key => {
                    columns.push(alias.clone().unwrap_or_else(|| KEY_COLUMN.to_owned()));
                    outputs.push(Output::Key);
                }
                SelectItem::Column(name) => {
                    self.kind_of(c, &name)?;
                    columns.push(alias.clone().unwrap_or_else(|| name.clone()));
                    // A declared TIMESTAMPTZ/DATE is stored as an integer and
                    // PRINTS as an ISO-8601 string (QL_CONTRACT §4.2): the
                    // declared type in the catalog descriptor is what says so,
                    // and the conversion is a row function like any other.
                    match self.time_column(c, &name)? {
                        None => outputs.push(push_field(name.clone(), &mut fields)),
                        Some(declared) => {
                            let Output::Field(at) = push_field(name.clone(), &mut fields) else {
                                unreachable!("push_field returns a field position");
                            };
                            self.row_functions.push(format!(
                                "{name} -> ISO-8601 text (declared {declared}, stored Int microseconds)"
                            ));
                            functions.push(CompiledRow::Iso {
                                arg: Box::new(CompiledRow::Field { at, time: true }),
                                date_only: declared == "DATE",
                            });
                            outputs.push(Output::Row(functions.len() - 1));
                        }
                    }
                }
                SelectItem::Function(expr) => {
                    let compiled = self.row_function(c, &expr, &mut fields)?;
                    let written = expr.written();
                    self.row_functions.push(format!(
                        "{written} -> evaluated over this row's projected values, after the index-side stage (cost is proportional to the rows RETURNED)"
                    ));
                    columns.push(alias.clone().unwrap_or(written));
                    functions.push(compiled);
                    outputs.push(Output::Row(functions.len() - 1));
                }
                SelectItem::OrderValue(_) | SelectItem::Divided { .. } => {
                    let what = match &item {
                        SelectItem::Divided { column, divisor } => {
                            format!("`{column} / {divisor}`")
                        }
                        SelectItem::OrderValue(what) => what.clone(),
                        _ => unreachable!("this arm took both"),
                    };
                    if statement.order.is_none() {
                        return Err(SqlError::unsupported(format!(
                            "{what} in a select list: the only expression a row can report here is this statement's own ranking value, and this statement has no ORDER BY"
                        )));
                    }
                    columns.push(alias.clone().unwrap_or_else(|| "score".to_owned()));
                    outputs.push(Output::OrderValue);
                }
                // `Compiler::aggregate` has already taken every statement
                // that names one; reaching here would mean this path was
                // asked to return ROWS for a folded answer.
                SelectItem::Aggregate { function, .. } => {
                    return Err(SqlError::unsupported(format!(
                        "{}() in a select list that is not a folded answer",
                        function.written()
                    )))
                }
            }
        }

        if outputs.iter().any(|output| *output == Output::Key) {
            self.notices.push(format!(
                "selecting `{KEY_COLUMN}` costs one `get_by_id` per returned row: a page cannot project the reserved field the external key lives in, so the key is fetched after the walk. `{ID_COLUMN}` is free"
            ));
        }
        // `docs/GRAPH_CONTRACT.md` §4.2: the reaching edge is carried only by
        // the traversal's own candidate stream, so a statement that reads it
        // must run on the graph driver. When a `_key` predicate is a POST-
        // FILTER beside such a traversal, the traversal keeps the driver and
        // the key range is answered from the external key the row carries
        // (`plan.rs`, `filters.rs`) -- the alternative would be an answer
        // ordered by nothing with every edge column `Missing`.
        let reads_the_edge = matches!(order, OwnedOrder::Edge { .. })
            || fields
                .iter()
                .any(|field| field.starts_with(EDGE_FIELD_PREFIX));
        let drives_the_graph = reads_the_edge
            && filters
                .iter()
                .any(|filter| matches!(filter, OwnedFilter::Graph(_)));
        let driver = if drives_the_graph {
            CandidateDriver::Auto
        } else if filters
            .iter()
            .any(|filter| matches!(filter, OwnedFilter::Key { .. }))
        {
            // A key filter is meaningful only under the driver that
            // certifies it from the mapping entry itself.
            CandidateDriver::Keys
        } else {
            CandidateDriver::Auto
        };

        Ok(SelectPlan {
            collection: c,
            columns,
            outputs,
            fields,
            filters,
            order,
            limit: statement.limit,
            driver,
            text: String::new(),
            functions,
            rewrites: std::mem::take(&mut self.rewrites),
            row_functions: std::mem::take(&mut self.row_functions),
        })
    }

    /// A parsed `RowExpr` with every column resolved to a projection slot and
    /// every constant folded.
    ///
    /// Resolving a column APPENDS it to the projection list, so a function
    /// over a column the select list does not otherwise name still costs one
    /// projected field and no extra read: the page already decodes the row it
    /// returns.
    /// A row expression whose value should be printed as a declared
    /// timestamp's ISO text rather than as the decimal of its microseconds.
    ///
    /// `Some(date_only)` exactly when `expr` is a bare column whose declared
    /// type is TIMESTAMPTZ or DATE, so `born_day` prints `1940-01-03` and
    /// `born_ts` prints `1940-01-03T05:06:00Z` on EVERY string path -- the
    /// two `||` sides, a string function's argument, and `::text`. Stated
    /// once so those three cannot disagree.
    fn iso_text(&mut self, c: CollectionId, expr: &RowExpr) -> SqlResult2<Option<bool>> {
        let RowExpr::Column(name) = expr else {
            return Ok(None);
        };
        Ok(self
            .time_column(c, name)?
            .map(|declared| declared == "DATE"))
    }

    fn row_function(
        &mut self,
        c: CollectionId,
        expr: &RowExpr,
        fields: &mut Vec<String>,
    ) -> SqlResult2<CompiledRow> {
        Ok(match expr {
            RowExpr::Column(name) => {
                self.kind_of(c, name)?;
                let at = match fields.iter().position(|existing| existing == name) {
                    Some(at) => at,
                    None => {
                        fields.push(name.clone());
                        fields.len() - 1
                    }
                };
                CompiledRow::Field {
                    at,
                    time: self.time_column(c, name)?.is_some(),
                }
            }
            RowExpr::Lit(literal) => CompiledRow::Lit(match self.value_of(literal)? {
                Value::Null => SqlValue::Null,
                Value::Bool(b) => SqlValue::Bool(b),
                Value::Number(n) => match n.as_i64() {
                    Some(i) => SqlValue::Int(i),
                    None => SqlValue::Float(n.as_f64().unwrap_or(f64::NAN)),
                },
                Value::String(text) => SqlValue::Text(text),
                other => SqlValue::Json(other),
            }),
            RowExpr::Now => CompiledRow::Micros(self.clock_micros()),
            RowExpr::CurrentDate => {
                CompiledRow::Micros(functions::date_trunc(TimeUnit::Day, self.clock_micros())?)
            }
            RowExpr::Interval(micros) => CompiledRow::Micros(*micros),
            RowExpr::Extract { unit, arg } => CompiledRow::Extract {
                unit: *unit,
                arg: Box::new(self.row_function(c, arg, fields)?),
            },
            RowExpr::Trunc { unit, arg } => CompiledRow::Trunc {
                unit: *unit,
                arg: Box::new(self.row_function(c, arg, fields)?),
            },
            RowExpr::Age { left, right } => CompiledRow::Age {
                left: Box::new(match right {
                    // `age(t)` is `now() - t`, so the clock is the LEFT side.
                    None => CompiledRow::Micros(self.clock_micros()),
                    Some(_) => self.row_function(c, left, fields)?,
                }),
                right: Box::new(match right {
                    None => self.row_function(c, left, fields)?,
                    Some(right) => self.row_function(c, right, fields)?,
                }),
            },
            RowExpr::ToChar { arg, format } => {
                // The template is checked HERE, at prepare, so a template
                // this slice does not carry is a refusal rather than an error
                // on the first row.
                functions::to_char(0, format)?;
                CompiledRow::ToChar {
                    arg: Box::new(self.row_function(c, arg, fields)?),
                    format: format.clone(),
                }
            }
            RowExpr::ToTimestamp(arg) => {
                CompiledRow::ToTimestamp(Box::new(self.row_function(c, arg, fields)?))
            }
            RowExpr::ToDate(arg) => {
                CompiledRow::ToDate(Box::new(self.row_function(c, arg, fields)?))
            }
            RowExpr::CastDate(arg) => {
                CompiledRow::CastDate(Box::new(self.row_function(c, arg, fields)?))
            }
            RowExpr::CastText(arg) => {
                let inner = self.row_function(c, arg, fields)?;
                // A declared timestamp cast to text is its ISO spelling, not
                // the decimal of its microseconds.
                match self.iso_text(c, arg)? {
                    Some(date_only) => CompiledRow::Iso {
                        arg: Box::new(inner),
                        date_only,
                    },
                    None => CompiledRow::CastText(Box::new(inner)),
                }
            }
            RowExpr::Str { func, args } => {
                let mut compiled = Vec::with_capacity(args.len());
                for arg in args {
                    compiled.push(self.row_function(c, arg, fields)?);
                }
                // A declared timestamp handed to a STRING function is its ISO
                // spelling: `upper(born_ts)` reads the text a SELECT prints,
                // not the integer underneath it.
                for at in 0..compiled.len() {
                    if let Some(date_only) = self.iso_text(c, &args[at])? {
                        compiled[at] = CompiledRow::Iso {
                            arg: Box::new(compiled[at].clone()),
                            date_only,
                        };
                    }
                }
                CompiledRow::Str {
                    func: *func,
                    args: compiled,
                }
            }
            RowExpr::Add(a, b) => CompiledRow::Add(
                Box::new(self.row_function(c, a, fields)?),
                Box::new(self.row_function(c, b, fields)?),
            ),
            RowExpr::Sub(a, b) => CompiledRow::Sub(
                Box::new(self.row_function(c, a, fields)?),
                Box::new(self.row_function(c, b, fields)?),
            ),
            RowExpr::Concat(a, b) => {
                // `||` is a string operator, so a declared timestamp on
                // either side of it is its ISO spelling -- the same rule
                // `concat(born_ts, '')` and `born_ts::text` already follow.
                // Without this `born_ts || ''` printed the decimal of its
                // microseconds while `concat(born_ts, '')` printed the text.
                let mut left = self.row_function(c, a, fields)?;
                let mut right = self.row_function(c, b, fields)?;
                if let Some(date_only) = self.iso_text(c, a)? {
                    left = CompiledRow::Iso {
                        arg: Box::new(left),
                        date_only,
                    };
                }
                if let Some(date_only) = self.iso_text(c, b)? {
                    right = CompiledRow::Iso {
                        arg: Box::new(right),
                        date_only,
                    };
                }
                CompiledRow::Concat(Box::new(left), Box::new(right))
            }
        })
    }

    /// The READY scalar index over `field`, if there is one. Unlike
    /// [`Compiler::index_for`] a missing index is not an error here: an
    /// aggregate over an unindexed column reads the ROW, which is a stated
    /// cost, not a refusal.
    fn scalar_index_opt(&self, c: CollectionId, field: &str) -> SqlResult2<Option<IndexId>> {
        let mut lists = self.index_lists.borrow_mut();
        let position = match lists.iter().position(|(id, _)| *id == c) {
            Some(position) => position,
            None => {
                lists.push((c, self.db.list_indexes(c).map_err(SqlError::from)?));
                lists.len() - 1
            }
        };
        Ok(lists[position]
            .1
            .iter()
            .find(|info| {
                info.field == field
                    && info.family == IndexFamily::Scalar
                    && info.state == IndexState::Ready
            })
            .map(|info| info.id))
    }

    // ── GROUP BY / DISTINCT / the aggregate functions (QL_CONTRACT §4.7) ──

    /// `Some` when this statement is an aggregate: it names an aggregate
    /// function, a `GROUP BY`, a `HAVING` or a `DISTINCT`. `None` leaves it
    /// to [`Compiler::select`], which is the ordinary row path.
    fn aggregate(&mut self, statement: &SelectStmt) -> SqlResult2<Option<AggregatePlan>> {
        let has_function = statement
            .items
            .iter()
            .any(|(item, _)| matches!(item, SelectItem::Aggregate { .. }));
        if !has_function
            && !statement.distinct
            && statement.group.is_none()
            && statement.having.is_empty()
        {
            return Ok(None);
        }
        let Source::Table(table) = &statement.source else {
            return Err(SqlError::unsupported(
                "an aggregate over GRAPH_TABLE: the aggregate atomic folds the candidates of ONE collection's plan; a traversal's COLUMNS are rows, and folding them is the path-accumulator item (QL_CONTRACT §4.3)",
            ));
        };
        let c = collection(self.db, table)?;

        let mut filters: Vec<OwnedFilter> = Vec::new();
        for expr in &statement.predicates {
            filters.push(self.where_filter(c, expr)?);
        }

        // The group key. `DISTINCT col` IS `GROUP BY col` with no
        // accumulators, so it lands on the same field.
        let group_expr = match (&statement.group, statement.distinct) {
            (Some(_), true) => {
                return Err(SqlError::unsupported(
                    "SELECT DISTINCT with GROUP BY: DISTINCT is a group with no accumulators, so writing both names the group twice",
                ))
            }
            (Some(group), false) => Some(group.clone()),
            (None, true) => {
                let mut named = None;
                for (item, _) in &statement.items {
                    match item {
                        SelectItem::Column(name) if named.is_none() => {
                            named = Some(name.clone());
                        }
                        SelectItem::Column(_) => {
                            return Err(SqlError::Refused {
                                keyword: "DISTINCT <two columns>".into(),
                                tier: Tier::Three,
                                reason: "QL_CONTRACT §4.7: DISTINCT is a group with no accumulators, and GROUP BY takes ONE key; a composite key has no atomic.",
                            })
                        }
                        _ => {
                            return Err(SqlError::unsupported(
                                "SELECT DISTINCT takes one column: it is a group with no accumulators",
                            ))
                        }
                    }
                }
                Some(GroupExpr {
                    column: named.ok_or_else(|| {
                        SqlError::unsupported("SELECT DISTINCT names no column")
                    })?,
                    divisor: None,
                })
            }
            (None, false) => None,
        };

        let group = match &group_expr {
            None => None,
            Some(GroupExpr { column, divisor }) => {
                let kind = self.kind_of(c, column)?;
                let index = self.scalar_index_opt(c, column)?;
                Some(match (divisor, index) {
                    (Some(divisor), Some(index)) if kind == Kind::Int => {
                        OwnedGroupKey::IndexDiv {
                            index,
                            divisor: *divisor,
                        }
                    }
                    (Some(_), _) => {
                        // The brief's fallback, said out loud: the expression
                        // group key rides the posting, so without an Int
                        // scalar index there is nothing to compute it from
                        // index-side and it is REFUSED rather than emulated
                        // over rows.
                        return Err(SqlError::Refused {
                            keyword: "GROUP BY <expression>".into(),
                            tier: Tier::Two,
                            reason: "QL_CONTRACT §4.7: `GROUP BY col / n` is accepted only when it can be computed INDEX-SIDE from the posting -- an Int scalar index on the column, whose own order the truncating division is monotone in. Without one, write `GROUP BY col` with a range filter instead; an expression folded over rows would be a scan wearing a group key's clothes.",
                        });
                    }
                    (None, Some(index)) => OwnedGroupKey::Index(index),
                    (None, None) => OwnedGroupKey::Field(column.clone()),
                })
            }
        };

        // A declared TIMESTAMPTZ/DATE group key prints as ISO text, the same
        // way the same column does in a row answer. A DIVIDED key is a bucket
        // number rather than an instant, so it stays an integer.
        let key_iso = match &group_expr {
            Some(GroupExpr {
                column,
                divisor: None,
            }) => self
                .time_column(c, column)?
                .map(|declared| declared == "DATE"),
            _ => None,
        };

        // The select list: the group key, and the aggregate functions.
        let mut columns: Vec<String> = Vec::new();
        let mut outputs: Vec<AggOutput> = Vec::new();
        let mut accumulators: Vec<OwnedAccumulator> = Vec::new();
        // The written form of each accumulator, so HAVING and ORDER BY can
        // find the one they name.
        let mut written: Vec<(AggFunc, AggArg, Option<String>)> = Vec::new();
        for (item, alias) in &statement.items {
            match item {
                SelectItem::Function(expr) => {
                    return Err(SqlError::unsupported(format!(
                        "`{}` in a folded answer: QL_CONTRACT §4.1 and §4.2 make a function over projected values a ROW function, and a folded answer returns groups, not rows. Group by the value the function computes (`GROUP BY col`), or select the function without folding",
                        expr.written()
                    )))
                }
                SelectItem::Aggregate { function, argument } => {
                    let accumulator = self.accumulator(c, *function, argument)?;
                    let at = accumulators.len();
                    accumulators.push(accumulator);
                    written.push((*function, argument.clone(), alias.clone()));
                    columns.push(alias.clone().unwrap_or_else(|| function.written().to_owned()));
                    outputs.push(AggOutput::Value(at));
                }
                SelectItem::Column(name) => {
                    let Some(GroupExpr { column, .. }) = &group_expr else {
                        return Err(SqlError::unsupported(format!(
                            "`{name}` is neither an aggregate nor a GROUP BY key: a statement that folds rows can only report what is the same for every row of a group"
                        )));
                    };
                    if name != column {
                        return Err(SqlError::unsupported(format!(
                            "`{name}` is not the GROUP BY key `{column}`: a statement that folds rows can only report the key it grouped by"
                        )));
                    }
                    columns.push(alias.clone().unwrap_or_else(|| name.clone()));
                    outputs.push(AggOutput::Key);
                }
                SelectItem::Divided { column, divisor } => {
                    let Some(group) = &group_expr else {
                        return Err(SqlError::unsupported(format!(
                            "`{column} / {divisor}` is neither an aggregate nor a GROUP BY key"
                        )));
                    };
                    if group.column != *column || group.divisor != Some(*divisor) {
                        return Err(SqlError::unsupported(format!(
                            "`{column} / {divisor}` is not the GROUP BY key: a statement that folds rows can only report the key it grouped by"
                        )));
                    }
                    columns.push(alias.clone().unwrap_or_else(|| column.clone()));
                    outputs.push(AggOutput::Key);
                }
                SelectItem::Star => {
                    return Err(SqlError::unsupported(
                        "SELECT * with an aggregate: a folded answer has no row to expand",
                    ))
                }
                SelectItem::Id | SelectItem::Key => {
                    return Err(SqlError::unsupported(
                        "an entity id or external key with an aggregate: a group is not a row and has neither",
                    ))
                }
                SelectItem::OrderValue(what) => {
                    return Err(SqlError::unsupported(format!(
                        "{what} with an aggregate: the only expressions a folded answer reports are its group key and its accumulators"
                    )))
                }
            }
        }
        if group_expr.is_some() && !outputs.contains(&AggOutput::Key) && accumulators.is_empty() {
            // `SELECT DISTINCT col` always names the key; a bare
            // `GROUP BY col` with nothing selected has nothing to report.
            return Err(SqlError::unsupported(
                "GROUP BY with an empty select list reports nothing",
            ));
        }

        // HAVING. A predicate may name an aggregate the select list does not
        // report, exactly as Postgres allows; that one becomes a HIDDEN
        // accumulator -- it is folded, it is not a column.
        let mut having = Vec::new();
        for predicate in &statement.having {
            let at = match written.iter().position(|(function, argument, _)| {
                *function == predicate.function && *argument == predicate.argument
            }) {
                Some(at) => at,
                None => {
                    let accumulator =
                        self.accumulator(c, predicate.function, &predicate.argument)?;
                    accumulators.push(accumulator);
                    written.push((predicate.function, predicate.argument.clone(), None));
                    accumulators.len() - 1
                }
            };
            if matches!(predicate.function, AggFunc::Min | AggFunc::Max) {
                let column = match &predicate.argument {
                    AggArg::Column(name) => name.clone(),
                    AggArg::Star => String::new(),
                };
                if !column.is_empty() && self.kind_of(c, &column)? == Kind::Text {
                    return Err(SqlError::unsupported(format!(
                        "HAVING {}({column}) compares numbers, and this accumulator's value is text",
                        predicate.function.written()
                    )));
                }
            }
            having.push(GroupPredicate {
                accumulator: at,
                op: match predicate.op {
                    CmpOp::Eq => GroupCmp::Eq,
                    CmpOp::Ne => GroupCmp::Ne,
                    CmpOp::Lt => GroupCmp::Lt,
                    CmpOp::Le => GroupCmp::Le,
                    CmpOp::Gt => GroupCmp::Gt,
                    CmpOp::Ge => GroupCmp::Ge,
                },
                value: self.f64_of(&predicate.value)?,
            });
        }

        // ORDER BY: the group key, or one accumulator by its alias.
        let order = match &statement.order {
            None => GroupOrder::Key,
            Some(OrderKey::Column { column, descending }) => {
                let is_key = group_expr
                    .as_ref()
                    .is_some_and(|group| group.column == *column)
                    || outputs.iter().zip(&columns).any(|(output, name)| {
                        *output == AggOutput::Key && name == column
                    });
                if is_key {
                    if *descending {
                        return Err(SqlError::unsupported(
                            "ORDER BY <group key> DESC: the groups arrive in the driving index's own ASCENDING order, and reversing them would mean holding every group to turn it round -- which is the hashed shape's sort, and it sorts by an accumulator, not by the key",
                        ));
                    }
                    GroupOrder::Key
                } else {
                    let at = written
                        .iter()
                        .position(|(function, _, alias)| {
                            alias.as_deref() == Some(column.as_str())
                                || (alias.is_none() && function.written() == column)
                        })
                        .ok_or_else(|| {
                            SqlError::unsupported(format!(
                                "ORDER BY `{column}`: a folded answer is ordered by its group key or by one of its own aggregate aliases"
                            ))
                        })?;
                    GroupOrder::Accumulator {
                        at,
                        direction: if *descending {
                            SortDirection::Descending
                        } else {
                            SortDirection::Ascending
                        },
                    }
                }
            }
            Some(_) => {
                return Err(SqlError::unsupported(
                    "ORDER BY on a folded answer takes the group key or an aggregate alias; a distance, a vector or a BM25 ranking ranks ROWS",
                ))
            }
        };

        let driver = if filters
            .iter()
            .any(|filter| matches!(filter, OwnedFilter::Key { .. }))
        {
            CandidateDriver::Keys
        } else {
            CandidateDriver::Auto
        };

        Ok(Some(AggregatePlan {
            collection: c,
            columns,
            outputs,
            filters,
            group,
            accumulators,
            having,
            order,
            driver,
            limit: statement.limit,
            key_iso,
            text: String::new(),
        }))
    }

    fn accumulator(
        &mut self,
        c: CollectionId,
        function: AggFunc,
        argument: &AggArg,
    ) -> SqlResult2<OwnedAccumulator> {
        Ok(match (function, argument) {
            (AggFunc::Count, AggArg::Star) => OwnedAccumulator {
                function: AggregateFn::CountStar,
                input: None,
            },
            (_, AggArg::Star) => {
                return Err(SqlError::unsupported(format!(
                    "{}(*) is not a function",
                    function.written()
                )))
            }
            (function, AggArg::Column(column)) => {
                let kind = self.kind_of(c, column)?;
                if matches!(function, AggFunc::Sum | AggFunc::Avg)
                    && !matches!(kind, Kind::Int | Kind::Real)
                {
                    return Err(SqlError::unsupported(format!(
                        "{}({column}): sum and avg take a numeric column",
                        function.written()
                    )));
                }
                let input = match self.scalar_index_opt(c, column)? {
                    Some(index) => OwnedAggInput::Index(index),
                    None => OwnedAggInput::Field(column.clone()),
                };
                OwnedAccumulator {
                    function: match function {
                        AggFunc::Count => AggregateFn::Count,
                        AggFunc::Sum => AggregateFn::Sum,
                        AggFunc::Min => AggregateFn::Min,
                        AggFunc::Max => AggregateFn::Max,
                        AggFunc::Avg => AggregateFn::Avg,
                    },
                    input: Some(input),
                }
            }
        })
    }

    /// One conjunct of a `WHERE` clause, as a filter.
    ///
    /// The tree is handed to the engine as it was written: `Any` is a union
    /// of membership sets, `All` an intersection and `Not` a complement, and
    /// which leaves an index can answer is the engine's question to refuse,
    /// not this one's. What is decided here is only the SQL spelling --
    /// `<>` is a complement of an equality, `IN` a union of them.
    fn where_filter(&mut self, c: CollectionId, expr: &Expr) -> SqlResult2<OwnedFilter> {
        Ok(match expr {
            Expr::Leaf(predicate) => self.filter(c, predicate)?,
            Expr::Not(inner) => OwnedFilter::Not(Box::new(self.where_filter(c, inner)?)),
            Expr::Or(parts) => OwnedFilter::Any(
                parts
                    .iter()
                    .map(|part| self.where_filter(c, part))
                    .collect::<SqlResult2<Vec<_>>>()?,
            ),
            Expr::And(parts) => OwnedFilter::All(
                parts
                    .iter()
                    .map(|part| self.where_filter(c, part))
                    .collect::<SqlResult2<Vec<_>>>()?,
            ),
        })
    }

    /// The outer ids a semi-join names (`docs/QL_CONTRACT.md` §3).
    ///
    /// Two sources, because this database has two kinds of thing a subquery
    /// can name. An EDGE TYPE is one walk of the edge keyspace: `related` is
    /// not a collection here, it is the `related` edges, and its `source`
    /// column is every entity with an outgoing one. A COLLECTION is
    /// §4.8's join shape -- one key lookup per driving row -- with the
    /// driving rows read through an ordinary prepared query, so the
    /// subquery's own cost is a plan a caller can see rather than a hidden
    /// scan.
    ///
    /// Both sources run under the STATEMENT's own budget and cancellation and
    /// both are bounded by `MAX_SEMI_JOIN_IDS`, which is the size of the set
    /// this returns and holds. The edge walk used to have neither the cap nor
    /// the cancel; the collection walk ran under `QueryBudget::unlimited()`.
    fn semi_join(
        &mut self,
        c: CollectionId,
        table: &str,
        column: &str,
    ) -> SqlResult2<Vec<EntityId>> {
        let db = self.db;
        let budget = self.budget;
        if let Some(edge_type) = db.edge_type(table).ok().flatten() {
            let direction = if column.eq_ignore_ascii_case("source") {
                Direction::Outgoing
            } else if column.eq_ignore_ascii_case("destination") {
                Direction::Incoming
            } else {
                return Err(SqlError::unsupported(format!(
                    "a semi-join on `{table}.{column}`: an edge type's columns are `source` and `destination`, which are the two ends the edge keyspace is filed by"
                )));
            };
            let cancelled = &mut *self.cancelled;
            return Ok(db.edge_endpoints(
                c,
                GraphContextId::BASE,
                edge_type,
                direction,
                MAX_SEMI_JOIN_EDGES,
                MAX_SEMI_JOIN_IDS,
                budget,
                || cancelled(),
            )?);
        }
        let inner = collection(db, table)?;
        let fields = [column];
        let mut prepared = db
            .prepare_query(QueryRequest {
                collection: inner,
                filters: &[],
                order: QueryOrder::Driver,
                projection: Projection::Fields(&fields),
                total_limit: None,
                driver: CandidateDriver::Auto,
            })
            .map_err(SqlError::from)?;
        let cancelled = &mut *self.cancelled;
        let mut ids: Vec<EntityId> = Vec::new();
        loop {
            let page = prepared
                .next_page(1024, budget, || cancelled())
                .map_err(SqlError::from)?;
            for row in &page.rows {
                match row.projected.first() {
                    Some((_, ProjectedValue::Value(serde_json::Value::String(key)))) => {
                        if let Some(entity) = db.get(c, key).map_err(SqlError::from)? {
                            ids.push(entity.id);
                        }
                    }
                    // A row whose projected column is NULL or absent names no
                    // outer row, which is SQL's own answer: `x IN (SELECT c
                    // ...)` is never TRUE because of a NULL `c`.
                    None
                    | Some((_, ProjectedValue::Missing))
                    | Some((_, ProjectedValue::Null))
                    | Some((_, ProjectedValue::Value(serde_json::Value::Null))) => {}
                    // Anything else is a column this join cannot use, and
                    // silently dropping every row of it returned the EMPTY
                    // set with no diagnostic. An outer row is named by its
                    // external key, which is text.
                    Some((_, ProjectedValue::Value(other))) => {
                        let kind = match other {
                            serde_json::Value::Bool(_) => "a boolean",
                            serde_json::Value::Number(_) => "a number",
                            serde_json::Value::Array(_) => "an array",
                            serde_json::Value::Object(_) => "an object",
                            serde_json::Value::Null | serde_json::Value::String(_) => {
                                unreachable!("null and text are answered above")
                            }
                        };
                        return Err(SqlError::unsupported(format!(
                            "a semi-join over `{table}` projects `{column}`, which holds {kind}: an outer row is named by its external key, which is text, so there is no id this column could name"
                        )));
                    }
                }
            }
            if ids.len() > MAX_SEMI_JOIN_IDS {
                return Err(SqlError::engine(format!(
                    "a semi-join over `{table}` named more than {MAX_SEMI_JOIN_IDS} outer rows: the set is held in memory and is bounded, not spilled"
                )));
            }
            if page.done {
                break;
            }
        }
        ids.sort_unstable_by_key(|id| id.sequence);
        ids.dedup();
        Ok(ids)
    }

    /// True when a tsquery is a bare `!term` -- one leading `!` and no other
    /// operator -- which is the one negated tsquery this slice compiles.
    fn negated_tsquery(&self, query: &TsQuery) -> SqlResult2<bool> {
        if !query.tsquery_syntax {
            return Ok(false);
        }
        let text = self.text_of(&query.source)?;
        let trimmed = text.trim();
        Ok(trimmed.starts_with('!')
            && !trimmed[1..].contains('!')
            && !trimmed.contains('|')
            && !trimmed.contains('&'))
    }
    // ── §4.1 / §4.2 range rewrites (QL_CONTRACT §4.1, §4.2) ──────────────

    /// The instant `now()` folds to for one statement.
    ///
    /// Read ONCE per compiled statement, so every row of one answer sees the
    /// same clock and a page that resumes does not drift. `docs/QL_CONTRACT.md`
    /// §4.2: "constants folded at prepare".
    fn clock_micros(&self) -> i64 {
        self.clock
    }

    /// A [`TimeValue`] folded to stored microseconds.
    fn time_value(&self, value: &TimeValue, column: &str) -> SqlResult2<i64> {
        Ok(match value {
            TimeValue::Lit(literal) => self.time_literal(literal, column)?,
            TimeValue::Clock { date_only, offset } => {
                let base = self.clock_micros();
                let base = if *date_only {
                    functions::date_trunc(TimeUnit::Day, base)?
                } else {
                    base
                };
                base.checked_add(*offset).ok_or_else(|| {
                    SqlError::unsupported("the folded clock arithmetic overflows i64 microseconds")
                })?
            }
        })
    }

    /// A written literal read as stored microseconds: a string is an ISO-8601
    /// or Postgres date/time literal, a whole number is already the stored
    /// integer.
    fn time_literal(&self, literal: &Literal, column: &str) -> SqlResult2<i64> {
        match self.value_of(literal)? {
            Value::String(text) => functions::parse_timestamp(&text),
            Value::Number(n) => n.as_i64().ok_or_else(|| {
                SqlError::Parameter(format!(
                    "`{column}` stores whole microseconds and {n} is not a whole number"
                ))
            }),
            other => Err(SqlError::Parameter(format!(
                "`{column}` is a declared timestamp and {other} is neither a date/time literal nor a whole number of microseconds"
            ))),
        }
    }

    /// A whole number written beside an `EXTRACT`.
    fn whole(&self, literal: &Literal, what: &str) -> SqlResult2<i64> {
        match self.value_of(literal)? {
            Value::Number(n) => n.as_i64().ok_or_else(|| {
                SqlError::Parameter(format!("{what} compares against a whole number, not {n}"))
            }),
            other => Err(SqlError::Parameter(format!(
                "{what} compares against a whole number, not {other}"
            ))),
        }
    }

    /// One half-open range `[lower, upper)` over the stored microseconds, as
    /// the scalar filter spells it.
    fn micro_range(lower: Option<i64>, upper: Option<i64>) -> OwnedScalarFilter {
        OwnedScalarFilter::Range {
            lower: lower.map_or(Bound::Unbounded, |v| Bound::Included(Scalar::I64(v))),
            upper: upper.map_or(Bound::Unbounded, |v| Bound::Excluded(Scalar::I64(v))),
        }
    }

    /// The empty range: a predicate whose pre-image holds no instant at all
    /// (`date_trunc('year', t) = '1950-06-01'`). It is still ONE range, so it
    /// is answered by the index with no candidates walked rather than
    /// refused: the statement is well formed and its answer is no rows.
    fn empty_range() -> OwnedScalarFilter {
        OwnedScalarFilter::Range {
            lower: Bound::Excluded(Scalar::I64(i64::MAX)),
            upper: Bound::Excluded(Scalar::I64(i64::MAX)),
        }
    }

    /// The bounds a comparison against one folded instant produces.
    fn compare_range(op: CmpOp, at: i64, what: &str) -> SqlResult2<OwnedScalarFilter> {
        Ok(match op {
            CmpOp::Eq => Self::micro_range(Some(at), at.checked_add(1)),
            CmpOp::Lt => Self::micro_range(None, Some(at)),
            CmpOp::Le => Self::micro_range(None, at.checked_add(1)),
            CmpOp::Gt => OwnedScalarFilter::Range {
                lower: Bound::Excluded(Scalar::I64(at)),
                upper: Bound::Unbounded,
            },
            CmpOp::Ge => Self::micro_range(Some(at), None),
            CmpOp::Ne => return Err(refuse::multi_range(what)),
        })
    }

    /// A `[start, end)` window compared against: `= window` is the window,
    /// `< window` is everything below its start, and so on. This is what
    /// makes `EXTRACT(YEAR FROM t) = 1950` and `date_trunc('year', t) = lit`
    /// ONE range each.
    fn window_range(op: CmpOp, start: i64, end: i64, what: &str) -> SqlResult2<OwnedScalarFilter> {
        Ok(match op {
            CmpOp::Eq => Self::micro_range(Some(start), Some(end)),
            CmpOp::Lt => Self::micro_range(None, Some(start)),
            CmpOp::Le => Self::micro_range(None, Some(end)),
            CmpOp::Gt => Self::micro_range(Some(end), None),
            CmpOp::Ge => Self::micro_range(Some(start), None),
            // The complement of a window is TWO ranges: below it and above
            // it. That is a union.
            CmpOp::Ne => return Err(refuse::multi_range(what)),
        })
    }

    /// The same comparison when the literal is NOT on the unit's boundary --
    /// `date_trunc('month', t) >= '1950-01-15'`, `t::date < '1950-01-15 12:00'`.
    ///
    /// The left-hand side only ever takes boundary values, so an interior
    /// literal moves every cut to the boundary ABOVE it, which is the window's
    /// own `end`: `>= lit` and `> lit` are both `t >= end` (January is
    /// excluded, because `1950-01-01 >= 1950-01-15` is false), `< lit` and
    /// `<= lit` are both `t < end` (January is kept, because
    /// `1950-01-01 < 1950-01-15` is true), and `= lit` holds for no instant.
    /// Postgres answers each of these the same way. `<>` stays refused with
    /// the window reason rather than becoming a second spelling of "every
    /// row": one shape, one refusal.
    fn offset_window_range(op: CmpOp, end: i64, what: &str) -> SqlResult2<OwnedScalarFilter> {
        Ok(match op {
            CmpOp::Eq => Self::empty_range(),
            CmpOp::Lt | CmpOp::Le => Self::micro_range(None, Some(end)),
            CmpOp::Gt | CmpOp::Ge => Self::micro_range(Some(end), None),
            CmpOp::Ne => return Err(refuse::multi_range(what)),
        })
    }

    /// 1 January of `year`, in stored microseconds, saturating at the ends of
    /// the representable range.
    ///
    /// `EXTRACT(YEAR FROM t) = 300000` is a legal literal a user can write,
    /// and `days_from_civil(n, 1, 1) * MICROS_PER_DAY` leaves i64 somewhere
    /// past year 294,000. The release profile sets no `overflow-checks`
    /// (`Cargo.toml`), so the unchecked form wraps to a garbage window in
    /// release and panics in a test build, on a literal.
    ///
    /// Saturating is not a clamp of the ANSWER. Every instant a column can
    /// hold lies inside `[i64::MIN, i64::MAX]` microseconds, so a year above
    /// the range makes `= n` and `>= n` empty and `< n` everything, and a
    /// year below it makes `<= n` empty and `> n` everything -- which is
    /// what those comparisons mean. The year is bounded first, because
    /// `days_from_civil` multiplies the year itself.
    fn year_start(year: i64) -> i64 {
        const BOUND: i64 = 400_000;
        if year > BOUND {
            return i64::MAX;
        }
        if year < -BOUND {
            return i64::MIN;
        }
        match functions::days_from_civil(year, 1, 1).checked_mul(functions::MICROS_PER_DAY) {
            Some(micros) => micros,
            None if year > 0 => i64::MAX,
            None => i64::MIN,
        }
    }

    /// The `[start, end)` window `EXTRACT(<unit> FROM t) = n` names.
    ///
    /// `YEAR` is the one unit whose equality is contiguous over the stored
    /// integer: every instant of year `n` lies between 1 January `n` and
    /// 1 January `n + 1`, and nothing else does. `MONTH`, `DAY`, `DOW`,
    /// `HOUR`, `MINUTE` and `SECOND` repeat, so their pre-image is one
    /// interval PER period in the corpus -- a set of ranges, which is the
    /// membership-set union `OR` compiles to.
    fn extract_window(unit: TimeUnit, n: i64, what: &str) -> SqlResult2<(i64, i64)> {
        match unit {
            TimeUnit::Year => Ok((
                Self::year_start(n),
                Self::year_start(n.saturating_add(1)),
            )),
            TimeUnit::Epoch => Ok((
                n.saturating_mul(functions::MICROS_PER_SECOND),
                n.saturating_add(1).saturating_mul(functions::MICROS_PER_SECOND),
            )),
            _ => Err(refuse::multi_range(what)),
        }
    }

    /// A `Predicate::Time` folded into ONE scalar range on `column`'s index.
    fn time_filter(
        &mut self,
        c: CollectionId,
        column: &str,
        shape: &TimeShape,
    ) -> SqlResult2<OwnedFilter> {
        let what = shape.written(column);
        if self.time_column(c, column)?.is_none() {
            return Err(SqlError::unsupported(format!(
                "{what}: `{column}` is not a declared TIMESTAMPTZ or DATE. QL_CONTRACT §4.2 folds a date/time function over a column whose declared type says it holds UTC microseconds; over an untyped Int there is nothing to fold"
            )));
        }
        let index = self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?;
        let predicate = match shape {
            TimeShape::Extract { unit, op, value } => {
                let n = self.whole(value, &what)?;
                let (start, end) = Self::extract_window(*unit, n, &what)?;
                Self::window_range(*op, start, end, &what)?
            }
            TimeShape::ExtractBetween { unit, lower, upper } => {
                let low = self.whole(lower, &what)?;
                let high = self.whole(upper, &what)?;
                if high < low {
                    Self::empty_range()
                } else {
                    let (start, _) = Self::extract_window(*unit, low, &what)?;
                    let (_, end) = Self::extract_window(*unit, high, &what)?;
                    Self::micro_range(Some(start), Some(end))
                }
            }
            TimeShape::Trunc { unit, op, value } => {
                let at = self.time_literal(value, column)?;
                let start = functions::date_trunc(*unit, at)?;
                let end = functions::next_unit(*unit, start)?;
                // An off-boundary literal is its own comparison: `= v` holds
                // for no instant at all, and both inequalities cut at `end`
                // rather than at `start`. Postgres answers the same.
                if start == at {
                    Self::window_range(*op, start, end, &what)?
                } else {
                    Self::offset_window_range(*op, end, &what)?
                }
            }
            TimeShape::TruncBetween { unit, lower, upper } => {
                let low_at = self.time_literal(lower, column)?;
                let high_at = self.time_literal(upper, column)?;
                let low = functions::date_trunc(*unit, low_at)?;
                let high = functions::date_trunc(*unit, high_at)?;
                // BETWEEN is `>= lower AND <= upper`. The lower half carries
                // the off-boundary rule above: no truncated instant lies
                // between `low` and an interior `low_at`, so the window starts
                // at the next boundary. The upper half does not: `<= high_at`
                // and `<= high` admit the same truncated instants either way.
                let start = if low == low_at {
                    low
                } else {
                    functions::next_unit(*unit, low)?
                };
                let end = functions::next_unit(*unit, high)?;
                if end <= start {
                    Self::empty_range()
                } else {
                    Self::micro_range(Some(start), Some(end))
                }
            }
            TimeShape::CastDate { op, value } => {
                let at = self.time_literal(value, column)?;
                let start = functions::date_trunc(TimeUnit::Day, at)?;
                let end = functions::next_unit(TimeUnit::Day, start)?;
                // `t::date` is a truncation to the day, so it takes the same
                // off-boundary rule as `date_trunc('day', t)`.
                if start == at {
                    Self::window_range(*op, start, end, &what)?
                } else {
                    Self::offset_window_range(*op, end, &what)?
                }
            }
            TimeShape::Clock { op, value } => {
                let at = self.time_value(value, column)?;
                Self::compare_range(*op, at, &what)?
            }
            TimeShape::ClockBetween { lower, upper } => {
                let low = self.time_value(lower, column)?;
                let high = self.time_value(upper, column)?;
                if high < low {
                    Self::empty_range()
                } else {
                    Self::micro_range(Some(low), high.checked_add(1))
                }
            }
        };
        self.rewrites.push(format!(
            "{what} -> scalar range on `{column}` (index-side; the function is folded at prepare and never evaluated per candidate)"
        ));
        Ok(OwnedFilter::Scalar { index, predicate })
    }

    /// A `Predicate::TextFn` folded into ONE text-key range.
    fn text_filter(
        &mut self,
        c: CollectionId,
        column: &str,
        shape: &TextShape,
    ) -> SqlResult2<OwnedFilter> {
        let what = shape.written(column);
        if !matches!(self.kind_of(c, column)?, Kind::Text) {
            return Err(SqlError::unsupported(format!(
                "{what}: `{column}` is not a TEXT column, and a text-key range is over text keys"
            )));
        }
        let lowered = matches!(shape, TextShape::LowerEq { .. } | TextShape::LowerPrefix { .. });
        let index = if lowered {
            self.index_for_expression(
                c,
                column,
                IndexFamily::Scalar,
                Some(IndexExpr::Lower),
                "an expression index `CREATE INDEX ... ON t (lower(col))`",
            )?
        } else {
            self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?
        };
        // The bound is folded the way the INDEX stores it: an expression
        // index over lower(col) holds folded keys, so the literal is folded
        // to match. Without that the range would be over a different
        // alphabet than the keys it walks.
        let literal = |compiler: &Self, value: &Literal| -> SqlResult2<String> {
            let text = compiler.text_of(value)?;
            Ok(if lowered { text.to_lowercase() } else { text })
        };
        let predicate = match shape {
            TextShape::LowerEq { value } => OwnedScalarFilter::Eq(Scalar::Text(literal(self, value)?)),
            TextShape::LowerPrefix { value, .. } | TextShape::Prefix { value, .. } => {
                let raw = literal(self, value)?;
                let prefix = match shape {
                    TextShape::Prefix { written: "LIKE", .. }
                    | TextShape::LowerPrefix { written: "LIKE", .. } => {
                        functions::like_prefix(&raw)
                            .ok_or_else(|| SqlError::Refused {
                                keyword: "LIKE".into(),
                                tier: Tier::Two,
                                reason: super::parser::LIKE_NOT_A_PREFIX,
                            })?
                            .to_owned()
                    }
                    _ => raw,
                };
                if prefix.is_empty() {
                    return Err(SqlError::unsupported(format!(
                        "{what}: an empty prefix admits every row, which is a scan, and §6 does not allow one to be taken silently"
                    )));
                }
                if prefix.contains('\0') {
                    return Err(SqlError::unsupported(format!(
                        "{what}: a NUL inside a prefix has no successor in the escaped text key encoding (`src/store/scalar_key.rs`)"
                    )));
                }
                let upper = match functions::prefix_successor(&prefix) {
                    functions::PrefixSuccessor::Bound(next) => {
                        Bound::Excluded(Scalar::Text(next))
                    }
                    functions::PrefixSuccessor::Unbounded => Bound::Unbounded,
                    // The bound travels as text and this one is not text.
                    // Widening it to the replacement character would admit
                    // every value in between, and the residual filter uses
                    // the same bound, so nothing downstream would catch it.
                    functions::PrefixSuccessor::NotUtf8 => {
                        return Err(SqlError::unsupported(format!(
                            "{what}: this prefix's upper bound is a byte string that is not valid UTF-8 (incrementing the last byte of `{prefix}` leaves one), and a text range bound is text. QL_CONTRACT §3: there is no byte-valued text bound in this slice, so the prefix is refused rather than answered from a wider range than the one asked for"
                        )))
                    }
                };
                OwnedScalarFilter::Range {
                    lower: Bound::Included(Scalar::Text(prefix.clone())),
                    upper,
                }
            }
        };
        self.rewrites.push(format!(
            "{what} -> {} on `{column}`{} (index-side)",
            match predicate {
                OwnedScalarFilter::Eq(_) => "scalar equality",
                _ => "text-key prefix range",
            },
            if lowered {
                " through the expression index over lower(col)"
            } else {
                ""
            }
        ));
        Ok(OwnedFilter::Scalar { index, predicate })
    }

    fn filter(&mut self, c: CollectionId, predicate: &Predicate) -> SqlResult2<OwnedFilter> {
        Ok(match predicate {
            Predicate::Compare { column, op, value } => {
                // `t >= '1950-01-01'` over a declared TIMESTAMPTZ/DATE is a
                // §4.2 rewrite: the literal is read to stored microseconds
                // and the predicate is the ordinary scalar Range. Without the
                // declared type there is no literal grammar to read it with.
                if self.time_column(c, column)?.is_some()
                    && matches!(self.value_of(value)?, Value::String(_))
                {
                    return self.time_filter(
                        c,
                        column,
                        &TimeShape::Clock {
                            op: *op,
                            value: TimeValue::Lit(value.clone()),
                        },
                    );
                }
                let kind = self.kind_of(c, column)?;
                let index = self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?;
                let scalar = self.scalar(&kind, value, column)?;
                let predicate = match op {
                    CmpOp::Eq => OwnedScalarFilter::Eq(scalar),
                    // `<>` is the complement of an equality, which the
                    // engine takes inside the index itself: a union of the
                    // postings below the value and the postings above it,
                    // the nullish key in neither. See `QueryFilter::Not`.
                    CmpOp::Ne => {
                        return Ok(OwnedFilter::Not(Box::new(OwnedFilter::Scalar {
                            index,
                            predicate: OwnedScalarFilter::Eq(scalar),
                        })))
                    }
                    CmpOp::Lt => OwnedScalarFilter::Range {
                        lower: Bound::Unbounded,
                        upper: Bound::Excluded(scalar),
                    },
                    CmpOp::Le => OwnedScalarFilter::Range {
                        lower: Bound::Unbounded,
                        upper: Bound::Included(scalar),
                    },
                    CmpOp::Gt => OwnedScalarFilter::Range {
                        lower: Bound::Excluded(scalar),
                        upper: Bound::Unbounded,
                    },
                    CmpOp::Ge => OwnedScalarFilter::Range {
                        lower: Bound::Included(scalar),
                        upper: Bound::Unbounded,
                    },
                };
                OwnedFilter::Scalar { index, predicate }
            }
            Predicate::Between {
                column,
                lower,
                upper,
            } => {
                if self.time_column(c, column)?.is_some()
                    && matches!(self.value_of(lower)?, Value::String(_))
                {
                    return self.time_filter(
                        c,
                        column,
                        &TimeShape::ClockBetween {
                            lower: TimeValue::Lit(lower.clone()),
                            upper: TimeValue::Lit(upper.clone()),
                        },
                    );
                }
                let kind = self.kind_of(c, column)?;
                let index = self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?;
                OwnedFilter::Scalar {
                    index,
                    predicate: OwnedScalarFilter::Range {
                        lower: Bound::Included(self.scalar(&kind, lower, column)?),
                        upper: Bound::Included(self.scalar(&kind, upper, column)?),
                    },
                }
            }
            Predicate::IsNull { column, negated } => {
                let index = self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?;
                let leaf = OwnedFilter::Scalar {
                    index,
                    predicate: OwnedScalarFilter::IsNull,
                };
                // `IS NOT NULL` is the complement of the nullish key inside
                // the index, which is every other posting: one range, no
                // bitmap and no universe walk. See `QueryFilter::Not`.
                if *negated {
                    OwnedFilter::Not(Box::new(leaf))
                } else {
                    leaf
                }
            }
            Predicate::IsMissing { column } => {
                let index = self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?;
                OwnedFilter::Scalar {
                    index,
                    predicate: OwnedScalarFilter::IsMissing,
                }
            }
            Predicate::KeyCompare { op, value } => {
                let key = self.text_of(value)?;
                let (lower, upper) = match op {
                    CmpOp::Eq => (Bound::Included(key.clone()), Bound::Included(key)),
                    CmpOp::Lt => (Bound::Unbounded, Bound::Excluded(key)),
                    CmpOp::Le => (Bound::Unbounded, Bound::Included(key)),
                    CmpOp::Gt => (Bound::Excluded(key), Bound::Unbounded),
                    CmpOp::Ge => (Bound::Included(key), Bound::Unbounded),
                    // The complement of a one-key range, which the engine
                    // takes as the two mapping ranges either side of it.
                    CmpOp::Ne => {
                        return Ok(OwnedFilter::Not(Box::new(OwnedFilter::Key {
                            lower: Bound::Included(key.clone()),
                            upper: Bound::Included(key),
                        })))
                    }
                };
                OwnedFilter::Key { lower, upper }
            }
            Predicate::KeyBetween { lower, upper } => OwnedFilter::Key {
                lower: Bound::Included(self.text_of(lower)?),
                upper: Bound::Included(self.text_of(upper)?),
            },
            // `col IN (v1, v2, ...)`: one equality per value, unioned into
            // one membership set. `docs/QL_CONTRACT.md` §3.
            Predicate::InList { column, values } => {
                let kind = self.kind_of(c, column)?;
                let index = self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?;
                let mut leaves = Vec::with_capacity(values.len());
                for value in values {
                    let scalar = self.scalar(&kind, value, column)?;
                    leaves.push(OwnedFilter::Scalar {
                        index,
                        predicate: OwnedScalarFilter::Eq(scalar),
                    });
                }
                if leaves.len() == 1 {
                    leaves.pop().ok_or_else(|| {
                        SqlError::syntax("IN needs at least one value", 0)
                    })?
                } else {
                    OwnedFilter::Any(leaves)
                }
            }
            Predicate::KeyInList { values } => {
                let mut leaves = Vec::with_capacity(values.len());
                for value in values {
                    let key = self.text_of(value)?;
                    leaves.push(OwnedFilter::Key {
                        lower: Bound::Included(key.clone()),
                        upper: Bound::Included(key),
                    });
                }
                if leaves.len() == 1 {
                    leaves.pop().ok_or_else(|| {
                        SqlError::syntax("IN needs at least one value", 0)
                    })?
                } else {
                    OwnedFilter::Any(leaves)
                }
            }
            Predicate::Semi { table, column } => OwnedFilter::Ids(self.semi_join(c, table, column)?),
            // `to_tsquery('simple','!comet')`: the complement of the text
            // set. Written alone, because a `!` inside a larger tsquery is a
            // boolean tree over one index's postings rather than one leaf.
            Predicate::Text { column, query } if self.negated_tsquery(query)? => {
                let stripped = TsQuery {
                    source: Literal::Str(
                        self.text_of(&query.source)?
                            .trim()
                            .trim_start_matches('!')
                            .trim()
                            .to_owned(),
                    ),
                    tsquery_syntax: query.tsquery_syntax,
                };
                OwnedFilter::Not(Box::new(self.filter(
                    c,
                    &Predicate::Text {
                        column: column.clone(),
                        query: stripped,
                    },
                )?))
            }
            Predicate::Text { column, query } => {
                let index = self.index_for(c, column, IndexFamily::Text, "a text index")?;
                let (query, matching) = self.tsquery(query)?;
                OwnedFilter::Text {
                    index,
                    query,
                    matching,
                }
            }
            Predicate::Spatial {
                predicate,
                column,
                argument,
                metres,
            } => self.spatial(c, *predicate, column, argument, metres.as_ref())?,
            Predicate::Time { column, shape } => self.time_filter(c, column, shape)?,
            Predicate::TextFn { column, shape } => self.text_filter(c, column, shape)?,
        })
    }

    fn scalar(&self, kind: &Kind, literal: &Literal, column: &str) -> SqlResult2<Scalar> {
        let value = self.value_of(literal)?;
        Ok(match (kind, &value) {
            (Kind::Int, Value::Number(n)) => Scalar::I64(n.as_i64().ok_or_else(|| {
                SqlError::Parameter(format!("`{column}` is INT and {n} is not a whole number"))
            })?),
            (Kind::Real, Value::Number(n)) => Scalar::F64(n.as_f64().ok_or_else(|| {
                SqlError::Parameter(format!("`{column}` is REAL and {n} is not a number"))
            })?),
            (Kind::Text, Value::String(s)) => Scalar::Text(s.clone()),
            (Kind::Bool, Value::Bool(b)) => Scalar::Bool(*b),
            _ => {
                return Err(SqlError::Parameter(format!(
                    "`{column}` is declared {kind:?} and the value is {value}; a scalar predicate stays inside the index's declared domain rather than coercing (`ScalarFilter`, src/query/mod.rs)"
                )))
            }
        })
    }

    /// The tsquery text, split into E4's `TextMatch`. A tsquery that mixes
    /// `&` and `|` is an AND/OR tree, which is Tier 2.
    fn tsquery(&self, query: &TsQuery) -> SqlResult2<(String, TextMatch)> {
        let text = self.text_of(&query.source)?;
        if !query.tsquery_syntax {
            return Ok((text, TextMatch::Any));
        }
        let trimmed = text.trim();
        if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
            return Ok((
                trimmed[1..trimmed.len() - 1].trim().to_owned(),
                TextMatch::Phrase,
            ));
        }
        if trimmed.contains('\'') {
            let inner = trimmed.trim_matches('\'').trim();
            if inner.split_whitespace().count() > 1 {
                return Ok((inner.to_owned(), TextMatch::Phrase));
            }
        }
        let has_or = trimmed.contains('|');
        let has_and = trimmed.contains('&');
        if has_or && has_and {
            return Err(SqlError::Refused {
                keyword: "tsquery & |".into(),
                tier: Tier::Two,
                reason: "QL_CONTRACT §4.6: a tsquery that mixes `&` and `|` is a boolean TREE inside one index's postings; the Tier-1 tsquery is one operator, and a union ACROSS predicates is written with SQL's own OR.",
            });
        }
        if trimmed.contains('!') {
            return Err(SqlError::Refused {
                keyword: "tsquery !".into(),
                tier: Tier::Two,
                reason: "QL_CONTRACT §4.6: a tsquery `!` inside a larger tsquery is a boolean TREE over one index's postings; the Tier-1 spelling is `!term` alone, which is NOT over the text set.",
            });
        }
        if trimmed.contains("<->") {
            return Err(SqlError::Refused {
                keyword: "tsquery <->".into(),
                tier: Tier::Two,
                reason: "QL_CONTRACT §4.6: a tsquery distance operator is a positional constraint; the Tier-1 phrase atomic is a quoted phrase (TextMatch::Phrase).",
            });
        }
        let separator = if has_or { '|' } else { '&' };
        let terms: Vec<&str> = trimmed
            .split(separator)
            .map(str::trim)
            .filter(|term| !term.is_empty())
            .collect();
        if terms.iter().any(|term| term.contains(':')) {
            return Err(SqlError::Refused {
                keyword: "tsquery weight".into(),
                tier: Tier::Three,
                reason: "QL_CONTRACT §4.6: tsvector weights have no atomic; analyzer v1 stores one weight per token.",
            });
        }
        let matching = if has_or || terms.len() == 1 {
            TextMatch::Any
        } else {
            TextMatch::All
        };
        Ok((terms.join(" "), matching))
    }

    fn spatial(
        &mut self,
        c: CollectionId,
        predicate: SpatialPredicate,
        column: &str,
        argument: &GeoArg,
        metres: Option<&Literal>,
    ) -> SqlResult2<OwnedFilter> {
        let kind = self.kind_of(c, column)?;
        match kind {
            Kind::Point => {
                let index = self.index_for(c, column, IndexFamily::SpatialPoint, "a point index")?;
                match predicate {
                    SpatialPredicate::DWithin => {
                        let GeoArg::Point(point) = argument else {
                            return Err(SqlError::unsupported(
                                "ST_DWithin on a Point column takes a point: PointFilter::Radius is a centre and a radius",
                            ));
                        };
                        let center = self.point_of(point)?;
                        let radius_metres = self.f64_of(metres.ok_or_else(|| {
                            SqlError::syntax("ST_DWithin needs a distance", 0)
                        })?)?;
                        Ok(OwnedFilter::Point {
                            index,
                            predicate: PointFilter::Radius {
                                center,
                                radius_metres,
                            },
                        })
                    }
                    SpatialPredicate::Within => Ok(OwnedFilter::Point {
                        index,
                        predicate: PointFilter::Bbox(self.bounds_of(argument)?),
                    }),
                    other => Err(SqlError::unsupported(format!(
                        "{other:?} on a Point column: the point atomics are PointFilter::Bbox (ST_Within against an envelope) and PointFilter::Radius (ST_DWithin)"
                    ))),
                }
            }
            Kind::Geo => {
                let index =
                    self.index_for(c, column, IndexFamily::SpatialGeometry, "a geometry index")?;
                let geometry = self.geom_of(argument)?;
                let predicate = match predicate {
                    SpatialPredicate::DWithin => GeometryFilter::DWithin {
                        geometry,
                        metres: self.f64_of(metres.ok_or_else(|| {
                            SqlError::syntax("ST_DWithin needs a distance", 0)
                        })?)?,
                    },
                    SpatialPredicate::Intersects => GeometryFilter::Intersects(geometry),
                    SpatialPredicate::Within => GeometryFilter::Within(geometry),
                    SpatialPredicate::Contains => GeometryFilter::Contains(geometry),
                };
                Ok(OwnedFilter::Geometry { index, predicate })
            }
            other => Err(SqlError::unsupported(format!(
                "`{column}` is declared {other:?}; a spatial predicate needs a Point or a Geo column"
            ))),
        }
    }

    // ── ORDER BY ─────────────────────────────────────────────────────────

    fn order(&mut self, c: CollectionId, key: &OrderKey) -> SqlResult2<OwnedOrder> {
        Ok(match key {
            OrderKey::Column { column, descending } => {
                if column == ID_COLUMN {
                    if *descending {
                        return Err(SqlError::unsupported(
                            "ORDER BY _id DESC: QueryOrder::EntityId ascends; the primary tree is walked forwards",
                        ));
                    }
                    return Ok(OwnedOrder::EntityId);
                }
                let index = self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?;
                OwnedOrder::Scalar {
                    index,
                    direction: if *descending {
                        SortDirection::Descending
                    } else {
                        SortDirection::Ascending
                    },
                }
            }
            OrderKey::Distance {
                column,
                point,
                descending,
            } => {
                if *descending {
                    return Err(SqlError::unsupported(
                        "ORDER BY <-> DESC: QueryOrder::Distance refuses Descending at prepare -- there is no reverse KNN walk, and treating DESC as ASC would answer a different question",
                    ));
                }
                let index = self.index_for(c, column, IndexFamily::SpatialPoint, "a point index")?;
                OwnedOrder::Distance {
                    index,
                    center: self.point_of(point)?,
                }
            }
            OrderKey::Vector {
                column,
                query,
                op,
                descending,
            } => {
                if *descending {
                    return Err(SqlError::unsupported(
                        "ORDER BY <=> DESC: a vector order ascends by distance; the nearest is first",
                    ));
                }
                let metric = match op {
                    VecOp::Cosine => VectorMetric::Cosine,
                    VecOp::L2 => VectorMetric::SquaredL2,
                    VecOp::NegativeDot => VectorMetric::NegativeDot,
                };
                let vector = self.vector_of(query)?;
                self.vector_order(c, column, vector, metric)?
            }
            OrderKey::Bm25 {
                column,
                query,
                descending,
            } => {
                if !*descending {
                    self.notices.push(
                        "ORDER BY a text rank ASC: BM25 ranks best-first, so this page returns the WORST matches first"
                            .to_owned(),
                    );
                }
                let index = self.index_for(c, column, IndexFamily::Text, "a text index")?;
                let (query, matching) = self.tsquery(query)?;
                if !*descending {
                    return Err(SqlError::unsupported(
                        "ORDER BY ts_rank_cd(...) ASC: QueryOrder::Bm25 ranks best-first and has no ascending form; write DESC",
                    ));
                }
                OwnedOrder::Bm25 {
                    index,
                    query,
                    matching,
                }
            }
            OrderKey::Score { expr, descending } => OwnedOrder::Score {
                expr: self.score(c, expr)?,
                direction: if *descending {
                    SortDirection::Descending
                } else {
                    SortDirection::Ascending
                },
            },
        })
    }

    /// Exact or approximate, decided by the indexes the column has and by a
    /// `SET LOCAL` the session saw -- never by a planner knob.
    fn vector_order(
        &mut self,
        c: CollectionId,
        column: &str,
        query: Vec<f32>,
        metric: VectorMetric,
    ) -> SqlResult2<OwnedOrder> {
        let indexes = self.db.list_indexes(c).map_err(SqlError::from)?;
        let ready = |family: IndexFamily| -> Option<IndexId> {
            indexes
                .iter()
                .find(|info| {
                    info.field == column && info.family == family && info.state == IndexState::Ready
                })
                .map(|info| info.id)
        };
        let exact = ready(IndexFamily::ExactVector);
        let quantized = ready(IndexFamily::QuantizedVector);
        let ef = EF_SEARCH.with(Cell::get);
        match (exact, quantized, ef) {
            (Some(index), _, None) => Ok(OwnedOrder::ExactVector {
                index,
                query,
                metric,
            }),
            (_, Some(index), ef) => {
                let ef = ef.unwrap_or(DEFAULT_EF);
                self.notices.push(format!(
                    "ORDER BY a vector distance on `{column}` is APPROXIMATE (ef={ef}): the quantized index answers it, and the shortlist bounds the whole result set of this prepared query"
                ));
                Ok(OwnedOrder::ApproximateVector {
                    index,
                    query,
                    metric,
                    ef,
                })
            }
            (Some(index), None, Some(ef)) => {
                self.notices.push(format!(
                    "SET LOCAL ef_search = {ef} was seen but `{column}` has only an exact vector index; the answer is exact and the shortlist bound is unused"
                ));
                Ok(OwnedOrder::ExactVector {
                    index,
                    query,
                    metric,
                })
            }
            (None, None, _) => Err(SqlError::engine(format!(
                "no vector index on `{column}`: a vector order names either the exact family (page-order sidecar scan) or the quantized one (compact scan then f32 rerank)"
            ))),
        }
    }

    fn score(&mut self, c: CollectionId, node: &ScoreNode) -> SqlResult2<OwnedScore> {
        Ok(match node {
            ScoreNode::Lit(value) => OwnedScore::Lit(*value),
            ScoreNode::Column(column) => {
                let kind = self.kind_of(c, column)?;
                if matches!(kind, Kind::Text) {
                    return Err(SqlError::unsupported(format!(
                        "`{column}` is TEXT: a Score leaf is numeric, and a text scalar is refused at prepare (`ScoreExpr::Scalar`, src/query/mod.rs)"
                    )));
                }
                OwnedScore::Scalar {
                    index: self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?,
                }
            }
            ScoreNode::Bm25 { column, query } => {
                let index = self.index_for(c, column, IndexFamily::Text, "a text index")?;
                let (query, matching) = self.tsquery(query)?;
                OwnedScore::Bm25 {
                    index,
                    query,
                    matching,
                }
            }
            ScoreNode::VecDistance { column, query, op } => {
                let index =
                    self.index_for(c, column, IndexFamily::ExactVector, "an exact vector index")?;
                OwnedScore::VectorDistance {
                    index,
                    query: self.vector_of(query)?,
                    metric: match op {
                        VecOp::Cosine => VectorMetric::Cosine,
                        VecOp::L2 => VectorMetric::SquaredL2,
                        VecOp::NegativeDot => VectorMetric::NegativeDot,
                    },
                }
            }
            ScoreNode::Distance { column, point } => OwnedScore::Distance {
                index: self.index_for(c, column, IndexFamily::SpatialPoint, "a point index")?,
                center: self.point_of(point)?,
            },
            ScoreNode::Add(a, b) => OwnedScore::Add(
                Box::new(self.score(c, a)?),
                Box::new(self.score(c, b)?),
            ),
            ScoreNode::Sub(a, b) => OwnedScore::Sub(
                Box::new(self.score(c, a)?),
                Box::new(self.score(c, b)?),
            ),
            ScoreNode::Mul(a, b) => OwnedScore::Mul(
                Box::new(self.score(c, a)?),
                Box::new(self.score(c, b)?),
            ),
            ScoreNode::Div(a, b) => OwnedScore::Div(
                Box::new(self.score(c, a)?),
                Box::new(self.score(c, b)?),
            ),
            ScoreNode::Neg(a) => OwnedScore::Neg(Box::new(self.score(c, a)?)),
        })
    }

    // ── GRAPH_TABLE ──────────────────────────────────────────────────────

    /// A pattern compiles to ONE bounded traversal filter plus the collection
    /// the far element names. The seed is a key equality, which is a point
    /// lookup, not a scan.
    fn graph_table(&mut self, graph: &GraphTable) -> SqlResult2<(CollectionId, OwnedFilter)> {
        let seed_collection = collection(self.db, &graph.seed_collection)?;
        let target = collection(self.db, &graph.target_collection)?;
        let key = self.text_of(&graph.seed_key)?;
        let seed = self
            .db
            .get(seed_collection, &key)
            .map_err(SqlError::from)?
            .ok_or_else(|| {
                SqlError::engine(format!(
                    "GRAPH_TABLE seed: `{}` has no row at key `{key}`",
                    graph.seed_collection
                ))
            })?
            .id;
        // `base` is the base graph (GRAPH_CONTRACT 3.1: "no context means the
        // base graph"), which is context 0 and is never a NAMED context, so
        // it is resolved here rather than looked up and then special-cased
        // after the lookup has already failed.
        let context = if graph.context.eq_ignore_ascii_case("base") {
            GraphContextId::BASE
        } else {
            self.db
                .graph_context(&graph.context)
                .map_err(SqlError::from)?
                .ok_or_else(|| {
                    SqlError::engine(format!("no graph context named `{}`", graph.context))
                })?
        };
        let edge_type = match &graph.hop.edge_type {
            None => None,
            Some(name) => Some(
                self.db
                    .edge_type(name)
                    .map_err(SqlError::from)?
                    .ok_or_else(|| SqlError::engine(format!("no edge type named `{name}`")))?,
            ),
        };
        if graph.hop.max_depth > MAX_GRAPH_DEPTH {
            return Err(SqlError::unsupported(format!(
                "a quantifier of {} hops: a traversal is bounded by contract and this slice's bound is {MAX_GRAPH_DEPTH}",
                graph.hop.max_depth
            )));
        }
        if target != seed_collection {
            self.notices.push(format!(
                "GRAPH_TABLE walks from `{}` into `{}`: the traversal itself is untyped by collection, and the far element's label is checked by the collection the outer statement selects from",
                graph.seed_collection, graph.target_collection
            ));
        }
        self.notices.push(
            "GRAPH_TABLE: a WHERE written after the pattern is a POST-FILTER on completed matches (QL_CONTRACT §4.3, Tier 1); an inline element WHERE is the PER-HOP prune (GRAPH_CONTRACT 4.3) and compiles to the traversal's own edge and node predicates"
                .to_owned(),
        );
        // The edge element's inline WHERE. Each comparison is against one
        // property of the edge's own inline bag, so it is typed by the value
        // as written -- there is no declared kind to coerce it to, and an
        // edge property bag is untyped JSON (GRAPH_CONTRACT 2.4's declared
        // properties are a later item).
        let mut edge_where = Vec::with_capacity(graph.hop.predicates.len());
        for predicate in &graph.hop.predicates {
            edge_where.push(OwnedEdgePredicate {
                property: predicate.property.clone(),
                op: match predicate.op {
                    CmpOp::Eq => Cmp::Eq,
                    CmpOp::Ne => Cmp::Ne,
                    CmpOp::Lt => Cmp::Lt,
                    CmpOp::Le => Cmp::Le,
                    CmpOp::Gt => Cmp::Gt,
                    CmpOp::Ge => Cmp::Ge,
                },
                value: self.edge_value(&predicate.value, &predicate.property)?,
            });
        }
        // The far element's inline WHERE. Compiled exactly as the outer
        // WHERE's predicates are, and then narrowed to the kinds an index
        // answers without a row -- anything else is REFUSED, never demoted to
        // a post-filter, because a post-filter is a different question: it
        // keeps a node in the frontier that §4.3 says must never be expanded.
        let mut node_where = Vec::with_capacity(graph.node_predicates.len());
        for predicate in &graph.node_predicates {
            let filter = self.filter(target, predicate)?;
            match &filter {
                OwnedFilter::Scalar { predicate, .. } => match predicate {
                    OwnedScalarFilter::Eq(_) | OwnedScalarFilter::Range { .. } => {}
                    OwnedScalarFilter::IsNull | OwnedScalarFilter::IsMissing => {
                        return Err(SqlError::Refused {
                            keyword: "inline element WHERE IS NULL".into(),
                            tier: Tier::Three,
                            reason: "QL_CONTRACT §4.3: a per-hop node predicate is answered from index postings (GRAPH_CONTRACT 4.3, `a traversal never reads a row for a predicate on a covered field`). NULL and MISSING share one nullish index key, so only the row tells them apart; write it after COLUMNS as a post-filter on completed matches.",
                        })
                    }
                },
                OwnedFilter::Point { .. } => {}
                _ => {
                    return Err(SqlError::Refused {
                        keyword: "inline element WHERE".into(),
                        tier: Tier::Two,
                        reason: "QL_CONTRACT §4.3: a per-hop node predicate is answered from index postings, so it is a scalar equality, a scalar range or a point predicate (bbox or radius). A text, geometry or JSON predicate is refined from the row, which GRAPH_CONTRACT 4.3 forbids per hop; write it after COLUMNS as a post-filter on completed matches.",
                    })
                }
            }
            node_where.push(filter);
        }
        Ok((
            target,
            OwnedFilter::Graph(OwnedGraph {
                seed,
                direction: match graph.hop.direction {
                    GraphDirection::Outgoing => Direction::Outgoing,
                    GraphDirection::Incoming => Direction::Incoming,
                    GraphDirection::Both => Direction::Both,
                },
                context,
                edge_type,
                min_depth: graph.hop.min_depth,
                max_depth: graph.hop.max_depth,
                edge_where,
                node_where,
            }),
        ))
    }

    /// One edge-property predicate's value. An edge bag is untyped JSON, so
    /// the literal decides the type: a number without a fraction is an
    /// integer, one with a fraction a real, and both compare mathematically
    /// against whatever the bag holds (`edge_properties_match`).
    fn edge_value(&mut self, literal: &Literal, property: &str) -> SqlResult2<Scalar> {
        Ok(match self.value_of(literal)? {
            Value::Bool(value) => Scalar::Bool(value),
            Value::String(value) => Scalar::Text(value),
            Value::Number(number) => match number.as_i64() {
                Some(value) => Scalar::I64(value),
                None => Scalar::F64(number.as_f64().ok_or_else(|| {
                    SqlError::Parameter(format!("`{property}`'s value is not a number"))
                })?),
            },
            Value::Null => {
                return Err(SqlError::unsupported(format!(
                    "`{property} = NULL` on an edge element: an absent or null property satisfies no comparison, so the predicate would refuse every edge"
                )))
            }
            other => {
                return Err(SqlError::unsupported(format!(
                    "an edge property compares against a scalar literal, found {other}"
                )))
            }
        })
    }

    // ── writes ───────────────────────────────────────────────────────────

    fn insert(
        &mut self,
        table: &str,
        columns: &[String],
        values: &[Vec<Literal>],
    ) -> SqlResult2<WritePlan> {
        let c = collection(self.db, table)?;
        let key_at = match columns.iter().position(|name| is_key_column(name)) {
            Some(at) => at,
            None => {
                // With no `_key` column the FIRST column supplies the key,
                // which is where a `TEXT PRIMARY KEY` is written. It must be
                // TEXT, because an external key is a string.
                let first = columns
                    .first()
                    .ok_or_else(|| SqlError::syntax("INSERT names no columns", 0))?;
                if !matches!(self.kind_of(c, first)?, Kind::Text) {
                    return Err(SqlError::unsupported(format!(
                        "INSERT INTO {table} names no `{KEY_COLUMN}` and its first column `{first}` is not TEXT: `Database::put` takes a string key, so either name `{KEY_COLUMN}` or put the TEXT PRIMARY KEY first"
                    )));
                }
                self.notices.push(format!(
                    "INSERT INTO {table}: the external key comes from `{first}`, the first column; the value is ALSO stored as that declared field (battle50k deviation 13)"
                ));
                0
            }
        };
        let mut rows = Vec::with_capacity(values.len());
        for row in values {
            let key = self.text_of(&row[key_at])?;
            let mut document = Map::new();
            for (at, column) in columns.iter().enumerate() {
                if is_key_column(column) {
                    continue;
                }
                let kind = self.kind_of(c, column)?;
                // A declared TIMESTAMPTZ/DATE column accepts an ISO-8601 or
                // Postgres date/time LITERAL and stores the integer
                // (QL_CONTRACT §4.2); the same column still accepts the
                // integer itself.
                let value = match self.time_column(c, column)? {
                    Some(declared) => self.time_document_value(&row[at], column, &declared)?,
                    None => self.document_value(kind, &row[at], column)?,
                };
                document.insert(column.clone(), value);
            }
            rows.push((key, Value::Object(document)));
        }
        Ok(WritePlan::Insert {
            collection: c,
            rows,
        })
    }

    fn update(
        &mut self,
        table: &str,
        assignments: &[(String, Literal)],
        key: &Literal,
    ) -> SqlResult2<WritePlan> {
        let c = collection(self.db, table)?;
        let mut patch = Map::new();
        for (column, literal) in assignments {
            if is_key_column(column) {
                return Err(SqlError::unsupported(
                    "UPDATE ... SET _key = ...: the external key is the row's identity; a new key is a new row (INSERT) and the old one is a DELETE",
                ));
            }
            let kind = self.kind_of(c, column)?;
            let value = match self.time_column(c, column)? {
                Some(declared) => self.time_document_value(literal, column, &declared)?,
                None => self.document_value(kind, literal, column)?,
            };
            patch.insert(column.clone(), value);
        }
        Ok(WritePlan::Update {
            collection: c,
            key: self.text_of(key)?,
            patch: Value::Object(patch),
        })
    }

    /// One written value, checked against the column's declared `Kind`.
    /// A written value for a declared TIMESTAMPTZ/DATE column, stored as the
    /// integer microseconds `docs/QL_CONTRACT.md` §5 deviation 8 pins.
    ///
    /// A `DATE` is midnight UTC of its day, so a literal that carries a time
    /// of day is REFUSED rather than silently truncated: a statement that
    /// wrote one meant a timestamp and the column is not one.
    fn time_document_value(
        &self,
        literal: &Literal,
        column: &str,
        declared: &str,
    ) -> SqlResult2<Value> {
        let value = self.value_of(literal)?;
        if value.is_null() {
            return Ok(Value::Null);
        }
        let micros = match &value {
            Value::String(text) => functions::parse_timestamp(text)?,
            Value::Number(n) => n.as_i64().ok_or_else(|| {
                SqlError::Parameter(format!(
                    "`{column}` is declared {declared} and stores whole microseconds; {n} is not a whole number"
                ))
            })?,
            other => {
                return Err(SqlError::Parameter(format!(
                    "`{column}` is declared {declared} and {other} is neither a date/time literal nor a whole number of microseconds"
                )))
            }
        };
        if declared == "DATE" && micros != functions::date_trunc(TimeUnit::Day, micros)? {
            return Err(SqlError::Parameter(format!(
                "`{column}` is declared DATE, which is midnight UTC of its day; `{}` carries a time of day and would be truncated silently",
                value
            )));
        }
        Ok(Value::from(micros))
    }

    fn document_value(&self, kind: Kind, literal: &Literal, column: &str) -> SqlResult2<Value> {
        let value = self.value_of(literal)?;
        if value.is_null() {
            return Ok(Value::Null);
        }
        Ok(match kind {
            Kind::Vector(dimensions) => {
                let vector = self.vector_of(literal)?;
                if vector.len() != dimensions {
                    return Err(SqlError::Parameter(format!(
                        "`{column}` is VECTOR({dimensions}) and the value has {} dimension(s)",
                        vector.len()
                    )));
                }
                Value::from(vector)
            }
            Kind::Point | Kind::Geo => {
                let document = match value {
                    Value::String(text) => serde_json::from_str::<Value>(&text)
                        .map_err(|e| SqlError::Parameter(format!("`{column}`: GeoJSON: {e}")))?,
                    other => other,
                };
                // Parsed once here so a bad geometry is refused by the
                // statement rather than by the index maintenance behind it.
                let geom = geom_from_json(&document)?;
                if kind == Kind::Point && !matches!(geom, Geom::Point(_, _)) {
                    return Err(SqlError::Parameter(format!(
                        "`{column}` is GEOMETRY(Point,4326) and the value is not a Point"
                    )));
                }
                document
            }
            Kind::Int => match &value {
                Value::Number(n) if n.is_i64() => value,
                other => {
                    return Err(SqlError::Parameter(format!(
                        "`{column}` is an integer column and the value is {other}"
                    )))
                }
            },
            Kind::Real => match &value {
                Value::Number(_) => value,
                other => {
                    return Err(SqlError::Parameter(format!(
                        "`{column}` is REAL and the value is {other}"
                    )))
                }
            },
            Kind::Text => match &value {
                Value::String(_) => value,
                other => {
                    return Err(SqlError::Parameter(format!(
                        "`{column}` is TEXT and the value is {other}"
                    )))
                }
            },
            Kind::Bool => match &value {
                Value::Bool(_) => value,
                other => {
                    return Err(SqlError::Parameter(format!(
                        "`{column}` is BOOLEAN and the value is {other}"
                    )))
                }
            },
            Kind::Json => value,
        })
    }

    fn create_table(&mut self, table: String, columns: Vec<ColumnDef>) -> SqlResult2<WritePlan> {
        let mut fields = Vec::with_capacity(columns.len());
        let mut declared: Vec<(String, String)> = Vec::new();
        let mut keys = 0usize;
        for column in &columns {
            if column.name.starts_with('_') {
                return Err(SqlError::unsupported(format!(
                    "column `{}`: names beginning with `_` are reserved (`_id`, `_key`, `_collection`)",
                    column.name
                )));
            }
            if column.primary_key {
                keys += 1;
                if !matches!(column.kind, Kind::Text) {
                    return Err(SqlError::unsupported(format!(
                        "PRIMARY KEY on `{}`: an external key is a string, so the primary-key column is TEXT",
                        column.name
                    )));
                }
            }
            if column.declared == "TIMESTAMPTZ" || column.declared == "DATE" {
                self.notices.push(format!(
                    "`{}` is declared {} and stored as Kind::Int: UTC microseconds, no time-zone storage (QL_CONTRACT §5 deviation 8)",
                    column.name, column.declared
                ));
            }
            if functions::is_time_type(&column.declared) {
                declared.push((column.name.clone(), column.declared.clone()));
            }
            fields.push((column.name.clone(), column.kind.clone()));
        }
        if keys > 1 {
            return Err(SqlError::unsupported(
                "two PRIMARY KEY columns: a row has one external key",
            ));
        }
        if keys == 1 {
            self.notices.push(format!(
                "PRIMARY KEY on `{table}`: the column is stored as a declared field AND supplies the external key `Database::put` maps, so the key is held twice (battle50k deviation 13)"
            ));
        }
        Ok(WritePlan::CreateTable {
            name: table,
            fields,
            declared,
        })
    }

    fn create_index(
        &mut self,
        name: String,
        table: &str,
        method: IndexMethod,
    ) -> SqlResult2<WritePlan> {
        let c = collection(self.db, table)?;
        let method = match method {
            IndexMethod::Btree(field) => {
                self.kind_of(c, &field)?;
                CompiledIndex::Scalar {
                    field,
                    unique: false,
                }
            }
            IndexMethod::LowerBtree(field) => {
                if !matches!(self.kind_of(c, &field)?, Kind::Text) {
                    return Err(SqlError::unsupported(format!(
                        "lower({field}): the expression QL_CONTRACT §4.1 names folds a TEXT column"
                    )));
                }
                self.notices.push(format!(
                    "an expression index over lower({field}) stores the FOLDED value: `{field} = 'X'` still needs the plain index over `{field}`, and `lower({field}) = 'x'` needs this one"
                ));
                CompiledIndex::LowerScalar { field }
            }
            IndexMethod::Gin(field) => {
                if !matches!(self.kind_of(c, &field)?, Kind::Text) {
                    return Err(SqlError::unsupported(format!(
                        "gin(to_tsvector('simple', {field})): a text index spans one declared TEXT field (battle50k deviation 1)"
                    )));
                }
                CompiledIndex::Text { field }
            }
            IndexMethod::Gist(field) => match self.kind_of(c, &field)? {
                Kind::Point => CompiledIndex::Point { field },
                Kind::Geo => CompiledIndex::Geometry { field },
                other => {
                    return Err(SqlError::unsupported(format!(
                        "gist({field}): `{field}` is {other:?}; the spatial families index a Point or a Geo column"
                    )))
                }
            },
            IndexMethod::Exact(field) => {
                if !matches!(self.kind_of(c, &field)?, Kind::Vector(_)) {
                    return Err(SqlError::unsupported(format!(
                        "exact({field}): the exact family indexes a VECTOR column"
                    )));
                }
                CompiledIndex::ExactVector { field }
            }
            IndexMethod::Quantized { column, alias } => {
                if !matches!(self.kind_of(c, &column)?, Kind::Vector(_)) {
                    return Err(SqlError::unsupported(format!(
                        "quantized({column}): the quantized family indexes a VECTOR column"
                    )));
                }
                if let Some(alias) = alias {
                    self.notices.push(format!(
                        "USING {} is an alias of `quantized` and builds no new family (QL_CONTRACT §5 deviation 6): a symmetric int8 companion index with an f32 rerank, not a graph",
                        alias.to_ascii_lowercase()
                    ));
                }
                CompiledIndex::QuantizedVector { field: column }
            }
        };
        Ok(WritePlan::CreateIndex {
            collection: c,
            name,
            method,
        })
    }
}

/// pgvector's text form: `[a, b, c]`.
fn parse_vector_literal(text: &str) -> SqlResult2<Vec<f32>> {
    let trimmed = text.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .ok_or_else(|| {
            SqlError::Parameter("a vector literal is written `[a,b,c]`, as pgvector writes it".into())
        })?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|part| {
            part.trim()
                .parse::<f32>()
                .map_err(|e| SqlError::Parameter(format!("vector literal: {e}")))
        })
        .collect()
}

/// A GeoJSON geometry, as `kernel::spatial::Geom`.
fn geom_from_json(value: &Value) -> SqlResult2<Geom> {
    let ty = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| SqlError::Parameter("GeoJSON has no `type`".into()))?;
    let coordinates = value
        .get("coordinates")
        .cloned()
        .ok_or_else(|| SqlError::Parameter("GeoJSON has no `coordinates`".into()))?;
    let bad = |e: serde_json::Error| SqlError::Parameter(format!("GeoJSON coordinates: {e}"));
    Ok(match ty {
        "Point" => {
            let p: [f64; 2] = serde_json::from_value(coordinates).map_err(bad)?;
            Geom::Point(p[0], p[1])
        }
        "LineString" => Geom::LineString(serde_json::from_value(coordinates).map_err(bad)?),
        "Polygon" => Geom::Polygon(serde_json::from_value(coordinates).map_err(bad)?),
        "MultiPoint" => Geom::MultiPoint(serde_json::from_value(coordinates).map_err(bad)?),
        "MultiLineString" => {
            Geom::MultiLineString(serde_json::from_value(coordinates).map_err(bad)?)
        }
        "MultiPolygon" => Geom::MultiPolygon(serde_json::from_value(coordinates).map_err(bad)?),
        other => {
            return Err(SqlError::unsupported(format!(
                "GeoJSON type `{other}`: the stored geometries are Point, LineString, Polygon and their Multi forms (docs/SPATIAL_FUNCTIONS.md)"
            )))
        }
    })
}
