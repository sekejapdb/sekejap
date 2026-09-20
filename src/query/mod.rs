//! The query engine. This file holds the request and response vocabulary --
//! everything a caller names -- and the work meter every walk charges against.
//! The plan, the drivers, the filters, the ranking and the page loop are the
//! submodules below. See docs/QL_CONTRACT.md.
//!
//! Combined queries over scalar, graph, point, geometry, text and vector
//! families. Every constraint runs before ranked top-k; pages report their
//! selected complete driver, logical work and any explicit approximation
//! diagnostics.
use crate::collections::{
    corrupt, invalid, layout_id, ordered, ordered_into, prefix, read_ordered, reserved, row_key,
    vector_key, CollectionId, Database, EntityId, Error, IndexFamily, IndexId, IndexInfo,
    IndexState,
};
use crate::index::graph::{BfsRequest, Direction};
use crate::index::text::TextMatch;
use crate::index::vector::exact::VectorMetric;
use crate::index::vector::quantized::ApproxVectorMethod;
use crate::{Kind, Layout};
use crate::spatial_math::{
    Bounds, MAX_HILBERT_RANGES, MAX_HILBERT_VALUE, Point, WGS84_MIN_CURVATURE_RADIUS_METRES,
    bounds_hilbert_ranges, radius_candidate_bounds, wgs84_distance_metres, within_radius,
};
use crate::{dense_v3, scalar_key, spatial_geometry};
use kernel::btree::{RangeIter, ReverseRangeIter};
use kernel::spatial::{self, BoxF, cover_ranges};
use serde_json::Value;
use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashSet},
    fmt,
    ops::Bound,
    sync::Arc,
};

mod aggregate;
mod cursors;
mod drivers;
mod filters;
mod membership;
mod page;
mod plan;
mod rank;
mod rows;
mod score;
mod vector_scan;

pub use aggregate::{
    AggValue, Accumulator, AggregateFn, AggregateInput, AggregatePlanDescription, AggregateRequest,
    AggregateShape, GroupCmp, GroupKey, GroupOrder, GroupPage, GroupPredicate, GroupRow,
    PreparedAggregate,
};
pub use page::PreparedQuery;
use {cursors::*, drivers::*, filters::*, membership::*, plan::*, rank::*, rows::*, score::*};

pub use kernel::spatial::Geom;

const MAX_FILTERS: usize = 64;

const MAX_SCORE_DEPTH: usize = 32;

const MAX_SCORE_LEAVES: usize = 8;

const MAX_PROJECTION_FIELDS: usize = 64;

const MAX_PAGE_SIZE: usize = 8192;

const MAX_FIELD_BYTES: usize = 128;

const MAX_JSON_PREDICATE_BYTES: usize = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScalarValue<'a> {
    Bool(bool),
    I64(i64),
    F64(f64),
    Text(&'a str),
}

impl ScalarValue<'_> {
    fn json(self) -> Value {
        match self {
            Self::Bool(value) => Value::Bool(value),
            Self::I64(value) => Value::from(value),
            Self::F64(value) => Value::from(value),
            Self::Text(value) => Value::String(value.to_owned()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
/// Scalar predicates stay inside the index's declared domain. Preparation
/// rejects I64/F64, Bool or Text mismatches rather than coercing them. This is
/// separate from `JsonEq`, whose recursive number comparison is mathematical.
pub enum ScalarFilter<'a> {
    Eq(ScalarValue<'a>),
    Range {
        lower: Bound<ScalarValue<'a>>,
        upper: Bound<ScalarValue<'a>>,
    },
    IsNull,
    IsMissing,
}

#[derive(Clone, Debug, PartialEq)]
pub enum QueryFilter<'a> {
    Scalar {
        index: IndexId,
        predicate: ScalarFilter<'a>,
    },
    /// Structural JSON equality with mathematical numeric comparison. Missing
    /// never equals a value; an explicit JSON null matches only a present null.
    JsonEq {
        field: &'a str,
        value: &'a Value,
    },
    Graph(BfsRequest),
    Point {
        index: IndexId,
        predicate: PointFilter,
    },
    /// A predicate over a `SpatialGeometry` index. The posting's `BoxF` is
    /// only a candidate test: unlike a point posting, it does not prove the
    /// predicate, so T3's no-row rule does not apply and the row's geometry
    /// is refined through [`spatial_geometry`].
    Geometry {
        index: IndexId,
        predicate: GeometryFilter,
    },
    Text {
        index: IndexId,
        query: &'a str,
        matching: TextMatch,
    },
    /// A range (or, with equal-prefixed bounds, a prefix) over the
    /// external-key mapping keyspace (`mapping_key`, `collections.rs:382`).
    /// Meaningful only under `CandidateDriver::Keys`, which certifies it from
    /// the mapping entry itself -- see `DriverPlan::Keys`.
    Key {
        lower: Bound<&'a str>,
        upper: Bound<&'a str>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointFilter {
    Bbox(Bounds),
    Radius { center: Point, radius_metres: f64 },
}

/// Predicates over a stored `Kind::Geo` value, unit-matched to PostGIS the
/// same way [`spatial_geometry`] is: `Intersects` and `DWithin` are spheroidal
/// (`ST_Intersects`/`ST_DWithin` on `geography`); `Within` and `Contains` are
/// planar (`ST_Within`/`ST_Contains` have no geography overload).
#[derive(Clone, Debug, PartialEq)]
pub enum GeometryFilter {
    Intersects(Geom),
    Within(Geom),
    Contains(Geom),
    DWithin { geometry: Geom, metres: f64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortDirection {
    Ascending,
    Descending,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum QueryOrder<'a> {
    EntityId,
    Scalar {
        index: IndexId,
        direction: SortDirection,
    },
    ExactVector {
        index: IndexId,
        query: &'a [f32],
        metric: VectorMetric,
    },
    /// Symmetric-int8 compact scan followed by authoritative f32 reranking.
    /// `ef` bounds the approximate shortlist and therefore the total result
    /// set available across this prepared query's pages.
    ApproximateVector {
        index: IndexId,
        query: &'a [f32],
        metric: VectorMetric,
        ef: usize,
    },
    Bm25 {
        index: IndexId,
        query: &'a str,
        matching: TextMatch,
    },
    /// The rows in the order the candidate DRIVER hands them over, with no
    /// re-ranking at all.
    ///
    /// Which order that is depends on which driver the query prepared: the
    /// entity cursor walks the primary tree, so it is entity-id order; a
    /// scalar posting range is `value || sequence`, so it is that index's own
    /// ascending order; the text merge ascends by document, which is id order
    /// again; a spatial index is walked in CELL order. It is what SQLite
    /// returns for a bare index scan or an R*Tree join, which sorts nothing
    /// either.
    ///
    /// The point of it is the spatial case. Cells are not id order and not
    /// value order, so a spatial page ranked by anything else has to see
    /// every candidate in the envelope before it knows its first row -- and
    /// the next page has to see them all again. Asked in cell order, the page
    /// stops when it is full and the one after it opens at the posting this
    /// one stopped on.
    ///
    /// Pages are disjoint and complete, a LIMIT stops the walk, and a
    /// continuation seeds the driver at its own cursor position. The order is
    /// stable for a FIXED driver, which is what a prepared query has: the
    /// driver is chosen once, at prepare time, and every page of that query
    /// walks the same one. It is NOT a promise across two separately prepared
    /// queries that `CandidateDriver::Auto` might plan differently.
    Driver,
    /// Rows in ascending geodesic distance from `center` on a point index,
    /// ties broken by entity id. Descending is refused at prepare: there is
    /// no PostGIS-style reverse KNN walk here, and silently treating DESC as
    /// ASC would be a different question than the one asked.
    Distance {
        index: IndexId,
        center: Point,
        direction: SortDirection,
    },
    /// Rank by a combined arithmetic expression over index leaves.
    ///
    /// Filters still choose the candidate set. The expression is evaluated
    /// per surviving candidate. `ScoreExpr::VectorSimilarity` is higher-is-
    /// better: Cosine, NegativeDot and SquaredL2 all map the exact-index
    /// sidecar **distance** to `-distance` (Cosine distance is `1 - cos`;
    /// NegativeDot distance is `-dot`; SquaredL2 is squared Euclidean).
    Score {
        expr: &'a ScoreExpr<'a>,
        direction: SortDirection,
    },
}

/// Arithmetic score expression compiled by Phase-3 SQL `ORDER BY <expr>`.
///
/// Index leaves must belong to the request collection and the matching
/// family. Depth is capped at 32; leaf count (including literals) at 8.
/// Division by zero yields `NaN`, and `NaN` sorts last under both directions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScoreExpr<'a> {
    Lit(f64),
    /// Numeric scalar index value of the candidate. I64 and F64 coerce to
    /// `f64`; Bool is `0.0`/`1.0`; missing or null is `0.0`. Text scalars are
    /// refused at prepare.
    Scalar { index: IndexId },
    /// BM25 of `query` over `index`. A candidate that does not match scores
    /// `0.0` rather than dropping out of the ranking.
    Bm25 {
        index: IndexId,
        query: &'a str,
        matching: TextMatch,
    },
    /// Exact-vector similarity. Higher is better: the leaf is `-distance`
    /// from the authoritative f32 sidecar the ExactVector order already
    /// reads. A missing locator scores `f64::NEG_INFINITY` (the worst value
    /// under both directions); a zero-norm stored cosine vector scores `0.0`.
    VectorSimilarity {
        index: IndexId,
        query: &'a [f32],
        metric: VectorMetric,
    },
    /// Geodesic metres from `center` on a point index. A missing point
    /// scores `f64::INFINITY`.
    Distance { index: IndexId, center: Point },
    Add(&'a ScoreExpr<'a>, &'a ScoreExpr<'a>),
    Sub(&'a ScoreExpr<'a>, &'a ScoreExpr<'a>),
    Mul(&'a ScoreExpr<'a>, &'a ScoreExpr<'a>),
    Div(&'a ScoreExpr<'a>, &'a ScoreExpr<'a>),
    Neg(&'a ScoreExpr<'a>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateDriver {
    Auto,
    Entities,
    Filter(usize),
    Order,
    /// Enumerate the external-key mapping keyspace instead of the primary
    /// rows -- E4's counterpart of SQLite's automatic covering index on
    /// `(_key, rowid)`. An optional `QueryFilter::Key` in the request narrows
    /// it to a range or prefix; with none, the whole collection's keys are
    /// walked. See `DriverPlan::Keys`.
    Keys,
}

/// Candidate source selected during query preparation. This is returned with
/// every page so callers can explain which complete stream was examined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryDriver {
    Entities,
    Scalar(IndexId),
    Graph {
        filter: usize,
    },
    Spatial {
        index: IndexId,
        fallback_world: bool,
    },
    /// Ordered nearest walk over a point index (`QueryOrder::Distance`).
    Nearest {
        index: IndexId,
    },
    Geometry {
        index: IndexId,
        fallback_world: bool,
    },
    Text(IndexId),
    ExactVector(IndexId),
    QuantizedVector(IndexId),
    Keys,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Projection<'a> {
    Ids,
    Fields(&'a [&'a str]),
}

#[derive(Clone, Debug)]
pub struct QueryRequest<'a> {
    pub collection: CollectionId,
    pub filters: &'a [QueryFilter<'a>],
    pub order: QueryOrder<'a>,
    pub projection: Projection<'a>,
    pub total_limit: Option<usize>,
    pub driver: CandidateDriver,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProjectedValue {
    Missing,
    Null,
    Value(Value),
}

#[derive(Clone, Debug, PartialEq)]
pub enum OwnedScalarValue {
    Nullish,
    Bool(bool),
    I64(i64),
    F64(f64),
    Text(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum OrderValue {
    EntityId,
    Scalar(OwnedScalarValue),
    Distance(f64),
    Bm25(f64),
    /// The query asked for the driver's own walk order, so there is no
    /// ranking value to report: the row's place in the answer is the place
    /// the candidate stream gave it.
    Driver,
    Score(f64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApproximationDiagnostics {
    pub method: ApproxVectorMethod,
    pub ef: usize,
    /// Eligible candidates examined: the entries the approximate stage scored,
    /// on every plan. On the per-candidate path that is the candidates that
    /// passed every filter; on the page-order compact scan it is the entries
    /// the filters admitted. Entries the scan read and refused are charged to
    /// [`QueryWork`] as candidates and compact reads, not counted here.
    pub examined: usize,
    /// Compact-shortlisted candidates checked and reranked from authoritative
    /// f32 sidecars.
    pub reranked: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct QueryRow {
    pub id: EntityId,
    pub order: OrderValue,
    pub projected: Vec<(String, ProjectedValue)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Per-page logical work allowances. Cursor categories count every attempted
/// probe, including the terminal or prefix/range-boundary probe. They are not
/// physical disk-I/O counters; cache and B-tree metadata work is internal, and
/// the initial range-seek used to construct a cursor is setup rather than a
/// posting/entity probe.
pub struct QueryBudget {
    pub candidates: u64,
    pub primary_reads: u64,
    pub scalar_postings: u64,
    pub graph_edges: u64,
    pub graph_visited: u64,
    pub spatial_postings: u64,
    pub text_postings: u64,
    /// Authoritative primary-text tokens processed for phrase refinement.
    pub text_tokens: u64,
    pub vector_locators: u64,
    pub vector_sidecars: u64,
    pub vector_lanes: u64,
    /// Mapping-keyspace entries walked by `CandidateDriver::Keys`.
    pub key_postings: u64,
    /// Accumulator sets an aggregate holds AT ONCE (`src/query/aggregate.rs`).
    /// A STREAMING aggregate holds one whatever the collection contains; a
    /// HASHED one holds a set per distinct group, and this is the bound that
    /// makes that memory a stated quantity rather than the data's. There is
    /// no spill: past the cap the page is refused with
    /// [`WorkResource::Groups`].
    pub groups: u64,
    pub output_bytes: u64,
}

impl QueryBudget {
    pub const fn unlimited() -> Self {
        Self {
            candidates: u64::MAX,
            primary_reads: u64::MAX,
            scalar_postings: u64::MAX,
            graph_edges: u64::MAX,
            graph_visited: u64::MAX,
            spatial_postings: u64::MAX,
            text_postings: u64::MAX,
            text_tokens: u64::MAX,
            vector_locators: u64::MAX,
            vector_sidecars: u64::MAX,
            vector_lanes: u64::MAX,
            key_postings: u64::MAX,
            groups: u64::MAX,
            output_bytes: u64::MAX,
        }
    }

    /// The default ceiling on accumulator sets held at once: the same
    /// [`RUN_BYTES`](crate::query::rows) memory promise every other per-query
    /// buffer is written against, divided by what ONE group costs with
    /// `accumulators` accumulators on it.
    ///
    /// `prepare_aggregate` applies `min(caller's groups, this)` so the bound
    /// holds even under [`QueryBudget::unlimited`]: an aggregate that cannot
    /// spill must not be allowed to grow without one.
    pub fn groups_cap(accumulators: usize) -> u64 {
        aggregate::default_groups_cap(accumulators)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryWork {
    pub candidates: u64,
    pub primary_reads: u64,
    /// Complete dense-v3 row traversals. A row is one record whose fields are
    /// length-prefixed, so reaching field k means stepping over fields 0..k:
    /// there is no offset to jump to. This counts how many times a page paid
    /// that walk, which is the projection and row-filter cost a point-get
    /// count cannot see. Diagnostic only -- it has no budget, because
    /// refusing a query part-way through materialising its answer helps
    /// nobody.
    pub row_decodes: u64,
    pub scalar_postings: u64,
    pub graph_edges: u64,
    pub graph_visited: u64,
    pub spatial_postings: u64,
    pub text_postings: u64,
    pub text_tokens: u64,
    pub vector_locators: u64,
    pub vector_sidecars: u64,
    pub vector_lanes: u64,
    pub key_postings: u64,
    /// The most accumulator sets this page held at once.
    pub groups: u64,
    pub output_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkResource {
    Candidates,
    PrimaryReads,
    ScalarPostings,
    GraphEdges,
    GraphVisited,
    SpatialPostings,
    TextPostings,
    TextTokens,
    VectorLocators,
    VectorSidecars,
    VectorLanes,
    KeyPostings,
    Groups,
    OutputBytes,
}

#[derive(Debug)]
pub enum QueryError {
    Database(Error),
    Cancelled,
    BudgetExceeded {
        resource: WorkResource,
        limit: u64,
        attempted: u64,
    },
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for QueryError {}

impl From<Error> for QueryError {
    fn from(value: Error) -> Self {
        match value {
            Error::Cancelled => Self::Cancelled,
            value => Self::Database(value),
        }
    }
}

impl From<kernel::Error> for QueryError {
    fn from(value: kernel::Error) -> Self {
        Self::Database(Error::from(value))
    }
}

pub type QueryResult<T> = std::result::Result<T, QueryError>;

#[derive(Clone, Debug, PartialEq)]
pub struct QueryPage {
    pub rows: Vec<QueryRow>,
    pub done: bool,
    pub driver: QueryDriver,
    pub work: QueryWork,
    pub approximation: Option<ApproximationDiagnostics>,
}

pub struct WorkMeter<'a, C> {
    limit: QueryBudget,
    used: QueryWork,
    cancelled: &'a mut C,
}

impl<'a, C: FnMut() -> bool> WorkMeter<'a, C> {
    fn new(limit: QueryBudget, cancelled: &'a mut C) -> Self {
        Self {
            limit,
            used: QueryWork::default(),
            cancelled,
        }
    }

    pub(super) fn check_cancelled(&mut self) -> QueryResult<()> {
        if (self.cancelled)() {
            Err(QueryError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Note one complete dense-v3 row traversal. Unbudgeted on purpose: see
    /// [`QueryWork::row_decodes`].
    pub(super) fn note_row_decode(&mut self) {
        self.used.row_decodes = self.used.row_decodes.saturating_add(1);
    }

    /// This resource's running total and its ceiling, in one place, so
    /// "charge me" and "how much is left" cannot drift apart.
    fn slot(&mut self, resource: WorkResource) -> (&mut u64, u64) {
        match resource {
            WorkResource::Candidates => (&mut self.used.candidates, self.limit.candidates),
            WorkResource::PrimaryReads => (&mut self.used.primary_reads, self.limit.primary_reads),
            WorkResource::ScalarPostings => {
                (&mut self.used.scalar_postings, self.limit.scalar_postings)
            }
            WorkResource::GraphEdges => (&mut self.used.graph_edges, self.limit.graph_edges),
            WorkResource::GraphVisited => (&mut self.used.graph_visited, self.limit.graph_visited),
            WorkResource::SpatialPostings => {
                (&mut self.used.spatial_postings, self.limit.spatial_postings)
            }
            WorkResource::TextPostings => (&mut self.used.text_postings, self.limit.text_postings),
            WorkResource::TextTokens => (&mut self.used.text_tokens, self.limit.text_tokens),
            WorkResource::VectorLocators => {
                (&mut self.used.vector_locators, self.limit.vector_locators)
            }
            WorkResource::VectorSidecars => {
                (&mut self.used.vector_sidecars, self.limit.vector_sidecars)
            }
            WorkResource::VectorLanes => (&mut self.used.vector_lanes, self.limit.vector_lanes),
            WorkResource::KeyPostings => (&mut self.used.key_postings, self.limit.key_postings),
            WorkResource::Groups => (&mut self.used.groups, self.limit.groups),
            WorkResource::OutputBytes => (&mut self.used.output_bytes, self.limit.output_bytes),
        }
    }

    /// How much of one resource a walk may still spend.
    ///
    /// A scan that is handed this as a record count stops AT the budget
    /// rather than reading the whole collection and reporting the overrun
    /// afterwards, which is the difference between a budget that bounds work
    /// and one that only measures it.
    pub(super) fn remaining(&mut self, resource: WorkResource) -> u64 {
        let (used, limit) = self.slot(resource);
        limit.saturating_sub(*used)
    }

    pub(super) fn charge(&mut self, resource: WorkResource, amount: u64) -> QueryResult<()> {
        self.check_cancelled()?;
        let (used, limit) = self.slot(resource);
        let attempted = used.checked_add(amount).ok_or(QueryError::BudgetExceeded {
            resource,
            limit,
            attempted: u64::MAX,
        })?;
        if attempted > limit {
            return Err(QueryError::BudgetExceeded {
                resource,
                limit,
                attempted,
            });
        }
        *used = attempted;
        Ok(())
    }
}

fn invalid_query(message: impl fmt::Display) -> QueryError {
    QueryError::Database(invalid(message))
}

fn corrupt_query(message: impl fmt::Display) -> QueryError {
    QueryError::Database(corrupt(message))
}

// ── the plan, as a caller can read it ─────────────────────────────────────
//
// `EXPLAIN` in `crate::sql` has to say the same thing the planner decided,
// not a second opinion about it. These types are that answer, read off the
// compiled plan itself; the SQL layer formats them and adds nothing.

/// How one filter position is answered for a candidate that reaches it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterAnswer {
    /// The driving walk's own postings prove the predicate; nothing else is
    /// read for this position.
    Driver,
    /// A membership set built once from postings answers it by lookup.
    MembershipSet,
    /// Eligible for a membership set, but no page has walked it yet.
    MembershipUnbuilt,
    /// The membership walk exceeded its budget, so every candidate falls back
    /// to the row path (`MembershipSet::Overflow`).
    MembershipOverflow,
    /// Answered from index postings per candidate, without the row.
    IndexPosting,
    /// Answered from a key the candidate already carries.
    CarriedKey,
    /// The primary row is read and the predicate refined against it.
    Row,
    /// The conjunction was folded into another position, which answers both.
    Folded(usize),
    /// Answered from the traversal frontier this query already materialised.
    GraphFrontier,
}

/// One filter position, as the plan answers it.
#[derive(Clone, Debug, PartialEq)]
pub struct FilterPlan {
    pub position: usize,
    /// `scalar`, `point`, `geometry`, `text`, `json`, `graph` or `key`.
    pub family: &'static str,
    /// The index name, for the families that name one.
    pub index: Option<String>,
    pub field: Option<String>,
    /// The predicate, spelled the way the compiled form holds it.
    pub detail: String,
    pub answer: FilterAnswer,
}

/// The compiled plan of a prepared query, in the words the planner used.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryPlanDescription {
    pub driver: QueryDriver,
    /// What the driver walks: the index and the range, spelled out.
    pub driver_detail: String,
    /// True when the driver is a scan by definition rather than a walk over a
    /// bounded candidate set (`docs/QL_CONTRACT.md` §6).
    pub driver_is_a_scan: bool,
    pub filters: Vec<FilterPlan>,
    /// `entity_id`, `scalar`, `distance`, `bm25`, `exact_vector`,
    /// `approximate_vector`, `driver` or `score`.
    pub order_kind: &'static str,
    pub order_detail: String,
    /// True when the ranking, not a filter, has to read the row.
    pub order_reads_row: bool,
    /// One entry per leaf of a `QueryOrder::Score` expression, in tree order.
    pub score_leaves: Vec<String>,
    pub projection: Vec<String>,
    pub total_limit: Option<usize>,
}
