//! Combined queries over scalar, graph, point, text and vector families.
//! Every constraint runs before ranked top-k; pages report their selected
//! complete driver, logical work and any explicit approximation diagnostics.
use super::*;
use crate::spatial_math::{
    Bounds, MAX_HILBERT_VALUE, Point, bounds_hilbert_ranges, radius_candidate_bounds, within_radius,
};
use crate::{dense_v3, scalar_key};
use kernel::btree::RangeIter;
use serde_json::Value;
use std::{
    cmp::Ordering,
    collections::{BTreeSet, BinaryHeap},
    fmt,
    ops::Bound,
    sync::Arc,
};

const MAX_FILTERS: usize = 64;
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
    Text {
        index: IndexId,
        query: &'a str,
        matching: TextMatch,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointFilter {
    Bbox(Bounds),
    Radius { center: Point, radius_metres: f64 },
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateDriver {
    Auto,
    Entities,
    Filter(usize),
    Order,
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
    Text(IndexId),
    ExactVector(IndexId),
    QuantizedVector(IndexId),
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApproximationDiagnostics {
    pub method: ApproxVectorMethod,
    pub ef: usize,
    /// Eligible candidates examined after all filters passed.
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
            output_bytes: u64::MAX,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryWork {
    pub candidates: u64,
    pub primary_reads: u64,
    pub scalar_postings: u64,
    pub graph_edges: u64,
    pub graph_visited: u64,
    pub spatial_postings: u64,
    pub text_postings: u64,
    pub text_tokens: u64,
    pub vector_locators: u64,
    pub vector_sidecars: u64,
    pub vector_lanes: u64,
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

    pub(super) fn charge(&mut self, resource: WorkResource, amount: u64) -> QueryResult<()> {
        self.check_cancelled()?;
        let (used, limit) = match resource {
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
            WorkResource::OutputBytes => (&mut self.used.output_bytes, self.limit.output_bytes),
        };
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

#[derive(Clone, Debug)]
enum EncodedScalarFilter {
    Empty,
    Eq(Vec<u8>),
    Range {
        lower: EncodedBound,
        upper: EncodedBound,
    },
    IsNull,
    IsMissing,
}

#[derive(Clone, Debug)]
enum EncodedBound {
    Included(Vec<u8>),
    Excluded(Vec<u8>),
    Unbounded,
}

#[derive(Clone, Debug)]
enum CompiledFilter {
    Scalar {
        info: IndexInfo,
        predicate: EncodedScalarFilter,
        posting_membership: bool,
    },
    JsonEq {
        field: String,
        value: Value,
    },
    Graph {
        request: BfsRequest,
        position: usize,
    },
    Point {
        info: IndexInfo,
        predicate: PointFilter,
    },
    Text(PreparedText),
}

#[derive(Clone, Debug)]
struct PreparedText {
    info: IndexInfo,
    terms: Vec<String>,
    phrase: Option<Vec<String>>,
    matching: TextMatch,
    corpus: super::text_indexes::Corpus,
    dfs: Vec<u64>,
}

#[derive(Clone, Debug)]
enum CompiledOrder {
    EntityId,
    Scalar {
        info: IndexInfo,
        direction: SortDirection,
    },
    ExactVector {
        info: IndexInfo,
        query: Vec<f32>,
        query_norm: f64,
        metric: VectorMetric,
    },
    ApproximateVector {
        info: IndexInfo,
        query: Vec<f32>,
        query_norm: f64,
        metric: VectorMetric,
        ef: usize,
    },
    Bm25(PreparedText),
}

#[derive(Clone, Debug)]
enum DriverPlan {
    Entities,
    Scalar {
        info: IndexInfo,
        predicate: EncodedScalarFilter,
        position: Option<usize>,
    },
    Graph {
        position: usize,
    },
    Spatial {
        info: IndexInfo,
        predicate: PointFilter,
        position: usize,
        ranges: Vec<(u64, u64)>,
        fallback_world: bool,
    },
    Text {
        prepared: PreparedText,
        position: Option<usize>,
    },
    ExactVector {
        info: IndexInfo,
    },
    QuantizedVector {
        info: IndexInfo,
    },
}

impl DriverPlan {
    fn diagnostic(&self) -> QueryDriver {
        match self {
            Self::Entities => QueryDriver::Entities,
            Self::Scalar { info, .. } => QueryDriver::Scalar(info.id),
            Self::Graph { position } => QueryDriver::Graph { filter: *position },
            Self::Spatial {
                info,
                fallback_world,
                ..
            } => QueryDriver::Spatial {
                index: info.id,
                fallback_world: *fallback_world,
            },
            Self::Text { prepared, .. } => QueryDriver::Text(prepared.info.id),
            Self::ExactVector { info } => QueryDriver::ExactVector(info.id),
            Self::QuantizedVector { info } => QueryDriver::QuantizedVector(info.id),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RankValue {
    Entity,
    Scalar(Vec<u8>),
    Score(u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RankKey {
    value: RankValue,
    id: EntityId,
}

#[derive(Clone, Debug, Eq)]
struct HeapEntry {
    key: RankKey,
    descending: bool,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_rank(&self.key, &other.key, self.descending)
    }
}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug)]
struct ApproxHeapEntry {
    distance: f64,
    id: EntityId,
    locator: [u8; 6],
}

impl PartialEq for ApproxHeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.distance.to_bits() == other.distance.to_bits() && self.id == other.id
    }
}

impl Eq for ApproxHeapEntry {}

impl PartialOrd for ApproxHeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ApproxHeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.distance
            .total_cmp(&other.distance)
            .then_with(|| self.id.cmp(&other.id))
    }
}

fn compare_rank(a: &RankKey, b: &RankKey, descending: bool) -> Ordering {
    let value = match (&a.value, &b.value) {
        (RankValue::Entity, RankValue::Entity) => Ordering::Equal,
        (RankValue::Scalar(a), RankValue::Scalar(b)) => {
            if descending {
                b.cmp(a)
            } else {
                a.cmp(b)
            }
        }
        (RankValue::Score(a), RankValue::Score(b)) => {
            let a = f64::from_bits(*a);
            let b = f64::from_bits(*b);
            if descending {
                b.total_cmp(&a)
            } else {
                a.total_cmp(&b)
            }
        }
        _ => unreachable!("prepared order creates one rank-key kind"),
    };
    value.then_with(|| a.id.cmp(&b.id))
}

pub struct PreparedQuery<'db> {
    db: &'db Database,
    collection: CollectionId,
    filters: Vec<CompiledFilter>,
    order: CompiledOrder,
    projection: Vec<String>,
    driver: DriverPlan,
    total_limit: Option<usize>,
    emitted: usize,
    after: Option<RankKey>,
}

fn invalid_query(message: impl fmt::Display) -> QueryError {
    QueryError::Database(invalid(message))
}

fn corrupt_query(message: impl fmt::Display) -> QueryError {
    QueryError::Database(corrupt(message))
}

fn require_scalar_index(
    db: &Database,
    collection: CollectionId,
    id: IndexId,
) -> QueryResult<IndexInfo> {
    let info = db.index_info(id)?;
    if info.collection != collection {
        return Err(invalid_query("query index belongs to another collection"));
    }
    if info.family != IndexFamily::Scalar {
        return Err(invalid_query("query requires a scalar index"));
    }
    if info.state != IndexState::Ready {
        return Err(invalid_query("query index is not ready"));
    }
    Ok(info)
}

fn require_family_index(
    db: &Database,
    collection: CollectionId,
    id: IndexId,
    family: IndexFamily,
    label: &str,
) -> QueryResult<IndexInfo> {
    let info = db.index_info(id)?;
    if info.collection != collection {
        return Err(invalid_query("query index belongs to another collection"));
    }
    if info.family != family {
        return Err(invalid_query(format!("query requires a {label} index")));
    }
    if info.state != IndexState::Ready {
        return Err(invalid_query("query index is not ready"));
    }
    Ok(info)
}

fn prepare_text(
    db: &Database,
    collection: CollectionId,
    id: IndexId,
    query: &str,
    matching: TextMatch,
) -> QueryResult<PreparedText> {
    let info = require_family_index(db, collection, id, IndexFamily::Text, "text")?;
    super::text_indexes::descriptor(&info)?;
    let (analysis, phrase) = if matching == TextMatch::Phrase {
        let phrase = crate::text_analyzer::analyze_phrase_query(query).map_err(invalid_query)?;
        (phrase.analysis, Some(phrase.sequence))
    } else {
        (
            crate::text_analyzer::analyze(query).map_err(invalid_query)?,
            None,
        )
    };
    if analysis.terms.len() > 64 {
        return Err(invalid_query("text query exceeds 64 distinct terms"));
    }
    let terms: Vec<_> = analysis.terms.into_keys().collect();
    let corpus = super::text_indexes::read_corpus(db, id)?;
    let mut dfs = Vec::with_capacity(terms.len());
    for term in &terms {
        let df = super::text_indexes::read_df(db, id, term)?.unwrap_or(0);
        if df > corpus.documents {
            return Err(corrupt_query(
                "text document frequency exceeds corpus document count",
            ));
        }
        dfs.push(df);
    }
    Ok(PreparedText {
        info,
        terms,
        phrase,
        matching,
        corpus,
        dfs,
    })
}

fn prepare_vector(
    db: &Database,
    collection: CollectionId,
    id: IndexId,
    query: &[f32],
    metric: VectorMetric,
) -> QueryResult<(IndexInfo, Vec<f32>, f64)> {
    let info = require_family_index(db, collection, id, IndexFamily::ExactVector, "exact-vector")?;
    let dimension = super::vector_indexes::dimension(&info)?;
    if query.len() != dimension || query.iter().any(|lane| !lane.is_finite()) {
        return Err(invalid_query(
            "query vector has wrong dimension or non-finite lane",
        ));
    }
    let norm = query
        .iter()
        .fold(0.0, |sum, lane| sum + f64::from(*lane) * f64::from(*lane));
    if metric == VectorMetric::Cosine && norm == 0.0 {
        return Err(invalid_query("cosine query vector must have nonzero norm"));
    }
    Ok((info, query.to_vec(), norm))
}

fn prepare_approximate_vector(
    db: &Database,
    collection: CollectionId,
    id: IndexId,
    query: &[f32],
    metric: VectorMetric,
    ef: usize,
) -> QueryResult<(IndexInfo, Vec<f32>, f64)> {
    if !(1..=super::indexes::MAX_RESULTS).contains(&ef) {
        return Err(invalid_query("approximate vector ef requires 1..=65536"));
    }
    let info = require_family_index(
        db,
        collection,
        id,
        IndexFamily::QuantizedVector,
        "quantized-vector",
    )?;
    let dimension = super::quantized_vector_indexes::dimension(&info)?;
    if query.len() != dimension || query.iter().any(|lane| !lane.is_finite()) {
        return Err(invalid_query(
            "query vector has wrong dimension or non-finite lane",
        ));
    }
    let norm = query
        .iter()
        .fold(0.0, |sum, lane| sum + f64::from(*lane) * f64::from(*lane));
    if metric == VectorMetric::Cosine && norm == 0.0 {
        return Err(invalid_query("cosine query vector must have nonzero norm"));
    }
    Ok((info, query.to_vec(), norm))
}

fn point_ranges(predicate: PointFilter) -> QueryResult<(Vec<(u64, u64)>, bool)> {
    let bounds = match predicate {
        PointFilter::Bbox(bounds) => bounds,
        PointFilter::Radius {
            center,
            radius_metres,
        } => radius_candidate_bounds(center, radius_metres).map_err(invalid_query)?,
    };
    let ranges = bounds_hilbert_ranges(bounds);
    let fallback_world = ranges.as_slice() == [(0, MAX_HILBERT_VALUE)];
    Ok((ranges, fallback_world))
}

fn encode_scalar_value(kind: &Kind, value: ScalarValue<'_>) -> QueryResult<Vec<u8>> {
    if !matches!(
        (kind, value),
        (Kind::Bool, ScalarValue::Bool(_))
            | (Kind::Int, ScalarValue::I64(_))
            | (Kind::Real, ScalarValue::F64(_))
            | (Kind::Text, ScalarValue::Text(_))
    ) {
        return Err(invalid_query(
            "scalar predicate value does not match the index kind",
        ));
    }
    if matches!(value, ScalarValue::F64(number) if !number.is_finite()) {
        return Err(invalid_query("finite scalar Real value required"));
    }
    scalar_key::encode(kind, Some(&value.json())).map_err(QueryError::from)
}

fn encode_bound(kind: &Kind, bound: &Bound<ScalarValue<'_>>) -> QueryResult<EncodedBound> {
    match bound {
        Bound::Included(value) => Ok(EncodedBound::Included(encode_scalar_value(kind, *value)?)),
        Bound::Excluded(value) => Ok(EncodedBound::Excluded(encode_scalar_value(kind, *value)?)),
        Bound::Unbounded => Ok(EncodedBound::Unbounded),
    }
}

fn bound_bytes(bound: &EncodedBound) -> Option<&[u8]> {
    match bound {
        EncodedBound::Included(value) | EncodedBound::Excluded(value) => Some(value),
        EncodedBound::Unbounded => None,
    }
}

fn compile_scalar_filter(
    kind: &Kind,
    predicate: &ScalarFilter<'_>,
) -> QueryResult<EncodedScalarFilter> {
    match predicate {
        ScalarFilter::Eq(value) => Ok(EncodedScalarFilter::Eq(encode_scalar_value(kind, *value)?)),
        ScalarFilter::Range { lower, upper } => {
            let lower = encode_bound(kind, lower)?;
            let upper = encode_bound(kind, upper)?;
            let empty = match (bound_bytes(&lower), bound_bytes(&upper)) {
                (Some(lower_value), Some(upper_value)) if lower_value > upper_value => true,
                (Some(lower_value), Some(upper_value)) if lower_value == upper_value => {
                    matches!(lower, EncodedBound::Excluded(_))
                        || matches!(upper, EncodedBound::Excluded(_))
                }
                _ => false,
            };
            if empty {
                Ok(EncodedScalarFilter::Empty)
            } else {
                Ok(EncodedScalarFilter::Range { lower, upper })
            }
        }
        ScalarFilter::IsNull => Ok(EncodedScalarFilter::IsNull),
        ScalarFilter::IsMissing => Ok(EncodedScalarFilter::IsMissing),
    }
}

fn scalar_driver_score(predicate: &EncodedScalarFilter) -> u8 {
    match predicate {
        EncodedScalarFilter::Empty | EncodedScalarFilter::Eq(_) => 0,
        EncodedScalarFilter::Range { .. } => 1,
        EncodedScalarFilter::IsNull | EncodedScalarFilter::IsMissing => 2,
    }
}

impl Database {
    pub fn prepare_query<'db>(
        &'db self,
        request: QueryRequest<'_>,
    ) -> QueryResult<PreparedQuery<'db>> {
        self.catalog(request.collection)?;
        if request.filters.len() > MAX_FILTERS {
            return Err(invalid_query("query has more than 64 filters"));
        }
        let collection = self.collection_info(request.collection)?;
        let mut filters = Vec::with_capacity(request.filters.len());
        for (position, filter) in request.filters.iter().enumerate() {
            filters.push(match filter {
                QueryFilter::Scalar { index, predicate } => {
                    let info = require_scalar_index(self, request.collection, *index)?;
                    let predicate = compile_scalar_filter(&info.kind, predicate)?;
                    CompiledFilter::Scalar {
                        info,
                        predicate,
                        posting_membership: false,
                    }
                }
                QueryFilter::JsonEq { field, value } => {
                    if field.is_empty() || field.len() > MAX_FIELD_BYTES || reserved(field) {
                        return Err(invalid_query("invalid JSON equality field"));
                    }
                    let encoded = crate::binary_json(value).map_err(invalid_query)?;
                    if encoded.len() > MAX_JSON_PREDICATE_BYTES {
                        return Err(invalid_query("JSON equality value exceeds 1 MiB"));
                    }
                    if collection
                        .layout
                        .fields
                        .iter()
                        .find(|(name, _)| name == field)
                        .is_some_and(|(_, kind)| kind != &Kind::Json)
                    {
                        return Err(invalid_query(
                            "JSON equality requires a Json field or an undeclared extra",
                        ));
                    }
                    CompiledFilter::JsonEq {
                        field: (*field).to_owned(),
                        value: (*value).clone(),
                    }
                }
                QueryFilter::Graph(request) => CompiledFilter::Graph {
                    request: *request,
                    position,
                },
                QueryFilter::Point { index, predicate } => {
                    let info = require_family_index(
                        self,
                        request.collection,
                        *index,
                        IndexFamily::SpatialPoint,
                        "spatial-point",
                    )?;
                    super::spatial_indexes::descriptor(&info)?;
                    if let PointFilter::Radius { radius_metres, .. } = predicate {
                        if !radius_metres.is_finite() || *radius_metres < 0.0 {
                            return Err(invalid_query(
                                "point radius must be finite and non-negative",
                            ));
                        }
                    }
                    CompiledFilter::Point {
                        info,
                        predicate: *predicate,
                    }
                }
                QueryFilter::Text {
                    index,
                    query,
                    matching,
                } => CompiledFilter::Text(prepare_text(
                    self,
                    request.collection,
                    *index,
                    query,
                    *matching,
                )?),
            });
        }

        let order = match request.order {
            QueryOrder::EntityId => CompiledOrder::EntityId,
            QueryOrder::Scalar { index, direction } => CompiledOrder::Scalar {
                info: require_scalar_index(self, request.collection, index)?,
                direction,
            },
            QueryOrder::ExactVector {
                index,
                query,
                metric,
            } => {
                let (info, query, query_norm) =
                    prepare_vector(self, request.collection, index, query, metric)?;
                CompiledOrder::ExactVector {
                    info,
                    query,
                    query_norm,
                    metric,
                }
            }
            QueryOrder::ApproximateVector {
                index,
                query,
                metric,
                ef,
            } => {
                let (info, query, query_norm) =
                    prepare_approximate_vector(self, request.collection, index, query, metric, ef)?;
                CompiledOrder::ApproximateVector {
                    info,
                    query,
                    query_norm,
                    metric,
                    ef,
                }
            }
            QueryOrder::Bm25 {
                index,
                query,
                matching,
            } => CompiledOrder::Bm25(prepare_text(
                self,
                request.collection,
                index,
                query,
                matching,
            )?),
        };

        let fields = match request.projection {
            Projection::Ids => Vec::new(),
            Projection::Fields(fields) => {
                if fields.len() > MAX_PROJECTION_FIELDS {
                    return Err(invalid_query("query projects more than 64 fields"));
                }
                let mut out = Vec::with_capacity(fields.len());
                for field in fields {
                    if field.is_empty()
                        || field.len() > MAX_FIELD_BYTES
                        || reserved(field)
                        || out.iter().any(|old| old == field)
                    {
                        return Err(invalid_query("invalid or duplicate projection field"));
                    }
                    out.push((*field).to_owned());
                }
                out
            }
        };

        let filter_driver = |position: usize| -> QueryResult<DriverPlan> {
            match filters.get(position) {
                Some(CompiledFilter::Scalar {
                    info, predicate, ..
                }) => Ok(DriverPlan::Scalar {
                    info: info.clone(),
                    predicate: predicate.clone(),
                    position: Some(position),
                }),
                Some(CompiledFilter::Graph { position, .. }) => Ok(DriverPlan::Graph {
                    position: *position,
                }),
                Some(CompiledFilter::Point { info, predicate }) => {
                    let (ranges, fallback_world) = point_ranges(*predicate)?;
                    Ok(DriverPlan::Spatial {
                        info: info.clone(),
                        predicate: *predicate,
                        position,
                        ranges,
                        fallback_world,
                    })
                }
                Some(CompiledFilter::Text(prepared)) => Ok(DriverPlan::Text {
                    prepared: prepared.clone(),
                    position: Some(position),
                }),
                _ => Err(invalid_query("selected filter cannot drive candidates")),
            }
        };
        let order_driver = || -> QueryResult<DriverPlan> {
            match &order {
                CompiledOrder::Scalar { info, .. } => Ok(DriverPlan::Scalar {
                    info: info.clone(),
                    predicate: EncodedScalarFilter::Range {
                        lower: EncodedBound::Unbounded,
                        upper: EncodedBound::Unbounded,
                    },
                    position: None,
                }),
                CompiledOrder::EntityId => Ok(DriverPlan::Entities),
                CompiledOrder::ExactVector { info, .. } => {
                    Ok(DriverPlan::ExactVector { info: info.clone() })
                }
                CompiledOrder::ApproximateVector { info, .. } => {
                    Ok(DriverPlan::QuantizedVector { info: info.clone() })
                }
                CompiledOrder::Bm25(prepared) => Ok(DriverPlan::Text {
                    prepared: prepared.clone(),
                    position: None,
                }),
            }
        };
        let driver = match request.driver {
            CandidateDriver::Entities => DriverPlan::Entities,
            CandidateDriver::Filter(position) => filter_driver(position)?,
            CandidateDriver::Order => order_driver()?,
            CandidateDriver::Auto => {
                if let Some(position) = filters
                    .iter()
                    .position(|filter| matches!(filter, CompiledFilter::Graph { .. }))
                {
                    filter_driver(position)?
                } else if let Some(position) = filters.iter().position(|filter| {
                    matches!(
                        filter,
                        CompiledFilter::Scalar {
                            predicate: EncodedScalarFilter::Empty | EncodedScalarFilter::Eq(_),
                            ..
                        }
                    )
                }) {
                    filter_driver(position)?
                } else if let Some(position) = filters
                    .iter()
                    .position(|filter| matches!(filter, CompiledFilter::Text(_)))
                {
                    filter_driver(position)?
                } else if let Some(position) = filters.iter().position(|filter| {
                    matches!(filter, CompiledFilter::Point { predicate, .. } if point_ranges(*predicate).is_ok_and(|(_, world)| !world))
                }) {
                    filter_driver(position)?
                } else if let Some((_, position)) = filters
                    .iter()
                    .enumerate()
                    .filter_map(|(position, filter)| match filter {
                        CompiledFilter::Scalar { predicate, .. } => {
                            Some((scalar_driver_score(predicate), position))
                        }
                        _ => None,
                    })
                    .min()
                {
                    filter_driver(position)?
                } else if let Some(position) = filters
                    .iter()
                    .position(|filter| matches!(filter, CompiledFilter::Point { .. }))
                {
                    filter_driver(position)?
                } else {
                    order_driver()?
                }
            }
        };

        let scalar_driver_position = match &driver {
            DriverPlan::Scalar { position, .. } => *position,
            _ => None,
        };
        for (position, filter) in filters.iter_mut().enumerate() {
            if let CompiledFilter::Scalar {
                predicate,
                posting_membership,
                ..
            } = filter
            {
                *posting_membership = matches!(predicate, EncodedScalarFilter::Eq(_))
                    && scalar_driver_position != Some(position);
            }
        }

        Ok(PreparedQuery {
            db: self,
            collection: request.collection,
            filters,
            order,
            projection: fields,
            driver,
            total_limit: request.total_limit,
            emitted: 0,
            after: None,
        })
    }
}

fn validate_graph_request(db: &Database, request: BfsRequest) -> QueryResult<()> {
    if request.min_depth > request.max_depth
        || request.max_depth > 64
        || request.max_visited == 0
        || request.max_visited > 65_536
        || request.max_edges == 0
        || request.max_edges > 1_000_000
        || request.result_limit > 65_536
    {
        return Err(invalid_query("invalid BFS depth/work/result bounds"));
    }
    let header = super::graph_collections::read_graph_header(|key| {
        db.store()?.get(key).map_err(Error::from)
    })?;
    if request.context.0 >= header.next_context
        || request
            .edge_type
            .is_some_and(|edge_type| edge_type.0 == 0 || edge_type.0 >= header.next_type)
    {
        return Err(invalid_query("unknown graph context or edge type"));
    }
    Ok(())
}

fn visit_graph_direction<C: FnMut() -> bool>(
    db: &Database,
    header: super::graph_collections::GraphHeader,
    entity: EntityId,
    direction: Direction,
    request: BfsRequest,
    seen: &BTreeSet<EntityId>,
    next: &mut BTreeSet<EntityId>,
    scanned: &mut usize,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<()> {
    let tag = if direction == Direction::Outgoing {
        super::graph_collections::PRIMARY_EDGE
    } else {
        super::graph_collections::REVERSE_EDGE
    };
    let prefix = super::graph_collections::edge_prefix(
        tag,
        entity,
        Some(request.context),
        request.edge_type,
    );
    // The near entity, the context and (when the caller named one) the type
    // are the prefix itself: `starts_with` proves the row carries exactly the
    // bytes we built, so only the far endpoint has to be read back out.
    let at0 = prefix.len();
    // `for_each_ref` hands the callback borrows into the pinned leaf. The
    // allocating cursor built a key `Vec` and a value `Vec` for every edge
    // walked, for a parser that only reads them. The work meter is charged on
    // exactly the old schedule: one unit per turn of the loop, including the
    // turn that found no further row.
    let mut failure: Option<QueryError> = None;
    let mut stopped = false;
    {
        let mut step = |key: &[u8], value: &[u8]| -> QueryResult<bool> {
            meter.charge(WorkResource::GraphEdges, 1)?;
            if !key.starts_with(&prefix) {
                return Ok(false);
            }
            *scanned = scanned
                .checked_add(1)
                .ok_or_else(|| invalid_query("BFS edge work overflow"))?;
            if *scanned > request.max_edges {
                return Err(invalid_query("BFS edge work limit exceeded"));
            }
            // The SQL traversal returns entities, so it decodes no properties
            // and reads nothing across the pair: both directions are written in
            // one transaction, so a committed snapshot cannot hold half a pair,
            // and `verify_indexed_source` is the tool that checks pair
            // consistency.
            if direction == Direction::Incoming && !value.is_empty() {
                return Err(corrupt_query("nonempty reverse edge marker"));
            }
            let (_, adjacent) = super::graph_collections::adjacent_from_tail(
                key,
                at0,
                request.edge_type,
                request.context,
                header,
            )?;
            if !seen.contains(&adjacent) && !next.contains(&adjacent) {
                meter.charge(WorkResource::GraphVisited, 1)?;
                if seen.len() + next.len() == request.max_visited {
                    return Err(invalid_query("BFS visited limit exceeded"));
                }
                next.insert(adjacent);
            }
            Ok(true)
        };
        db.store()?
            .range(&prefix)
            .map_err(Error::from)?
            .for_each_ref(|key, value| match step(key, value) {
                Ok(true) => true,
                Ok(false) => {
                    stopped = true;
                    false
                }
                Err(error) => {
                    failure = Some(error);
                    stopped = true;
                    false
                }
            })
            .map_err(Error::from)?;
    }
    if let Some(error) = failure {
        return Err(error);
    }
    if !stopped {
        meter.charge(WorkResource::GraphEdges, 1)?;
    }
    Ok(())
}

fn execute_graph<C: FnMut() -> bool>(
    db: &Database,
    request: BfsRequest,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<BTreeSet<EntityId>> {
    validate_graph_request(db, request)?;
    meter.charge(WorkResource::PrimaryReads, 1)?;
    if db.store()?.get(&row_key(request.seed))?.is_none() {
        return Err(QueryError::Database(Error::NotFound("graph endpoint")));
    }
    let header = super::graph_collections::read_graph_header(|key| {
        db.store()?.get(key).map_err(Error::from)
    })?;
    meter.charge(WorkResource::GraphVisited, 1)?;
    let mut seen = BTreeSet::from([request.seed]);
    let mut results = BTreeSet::new();
    if request.include_seed && request.min_depth == 0 {
        if request.result_limit == 0 {
            return Err(invalid_query("BFS result limit exceeded"));
        }
        results.insert(request.seed);
    }
    let mut frontier = BTreeSet::from([request.seed]);
    let mut scanned = 0usize;
    for depth in 1..=request.max_depth {
        meter.check_cancelled()?;
        let mut next = BTreeSet::new();
        for entity in frontier {
            meter.check_cancelled()?;
            if matches!(request.direction, Direction::Outgoing | Direction::Both) {
                visit_graph_direction(
                    db,
                    header,
                    entity,
                    Direction::Outgoing,
                    request,
                    &seen,
                    &mut next,
                    &mut scanned,
                    meter,
                )?;
            }
            if matches!(request.direction, Direction::Incoming | Direction::Both) {
                visit_graph_direction(
                    db,
                    header,
                    entity,
                    Direction::Incoming,
                    request,
                    &seen,
                    &mut next,
                    &mut scanned,
                    meter,
                )?;
            }
        }
        seen.extend(next.iter().copied());
        if depth >= request.min_depth {
            if results.len() + next.len() > request.result_limit {
                return Err(invalid_query("BFS result limit exceeded"));
            }
            results.extend(next.iter().copied());
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    Ok(results)
}

fn execute_graph_filters<C: FnMut() -> bool>(
    db: &Database,
    filters: &[CompiledFilter],
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<Vec<Option<BTreeSet<EntityId>>>> {
    let mut results = vec![None; filters.len()];
    for filter in filters {
        if let CompiledFilter::Graph { request, position } = filter {
            results[*position] = Some(execute_graph(db, *request, meter)?);
        }
    }
    Ok(results)
}

struct Candidate {
    id: EntityId,
    row: Option<Vec<u8>>,
    scalar: Option<(IndexId, Vec<u8>)>,
    vector: Option<(IndexId, Vec<u8>)>,
    quantized: Option<(IndexId, Vec<u8>)>,
    satisfied_filter: Option<usize>,
}

struct EntityCursor<'a> {
    inner: RangeIter<'a>,
    prefix: Vec<u8>,
    /// Copy the primary row out of the leaf, or read its key only. The bytes
    /// are worth an allocation only when a filter or the ranking will decode
    /// them; a key-only scan used to allocate one per row and drop it.
    wants_row: bool,
    done: bool,
}

struct ScalarCursor<'a> {
    inner: Option<RangeIter<'a>>,
    prefix: Vec<u8>,
    info: IndexInfo,
    predicate: EncodedScalarFilter,
    /// The filter position this cursor's own walk already proves, if any --
    /// see `DriverCursor::new`.
    certifies: Option<usize>,
    /// Copy the posting's value key out of the leaf, or read the sequence
    /// only. Only a scalar ranking over this same index reads it.
    wants_scalar: bool,
    done: bool,
}

/// What one page needs from each candidate, decided once per page instead of
/// per row. Each field is something a driver can hand over for free when it is
/// wanted and must allocate for when it is not.
#[derive(Clone, Copy)]
struct CursorNeeds {
    row: bool,
    scalar_key: bool,
}

struct VectorCursor<'a> {
    inner: RangeIter<'a>,
    prefix: Vec<u8>,
    info: IndexInfo,
    done: bool,
}

struct QuantizedVectorCursor<'a> {
    inner: RangeIter<'a>,
    prefix: Vec<u8>,
    info: IndexInfo,
    done: bool,
}

struct TextPostingCursor<'a> {
    inner: super::text_indexes::TermPostings<'a>,
    head: Option<u64>,
    done: bool,
}

struct TextCursor<'a> {
    streams: Vec<TextPostingCursor<'a>>,
    collection: CollectionId,
    matching: TextMatch,
    position: Option<usize>,
    initialized: bool,
    done: bool,
}

struct SpatialCursor<'a> {
    db: &'a Database,
    info: IndexInfo,
    predicate: PointFilter,
    position: usize,
    prefix: Vec<u8>,
    ranges: Vec<(u64, u64)>,
    range: usize,
    inner: Option<RangeIter<'a>>,
    done: bool,
}

enum DriverCursor<'a> {
    Entities(EntityCursor<'a>),
    Scalar(ScalarCursor<'a>),
    Spatial(SpatialCursor<'a>),
    Text(TextCursor<'a>),
    Vector(VectorCursor<'a>),
    QuantizedVector(QuantizedVectorCursor<'a>),
    Ids(std::vec::IntoIter<EntityId>),
}

fn scalar_prefix(id: IndexId) -> Vec<u8> {
    let mut out = vec![super::indexes::SCALAR];
    out.extend(ordered(id.0));
    out
}

fn scalar_lower(predicate: &EncodedScalarFilter) -> Option<&[u8]> {
    match predicate {
        EncodedScalarFilter::Empty => None,
        EncodedScalarFilter::Eq(value) => Some(value),
        EncodedScalarFilter::Range { lower, .. } => bound_bytes(lower),
        EncodedScalarFilter::IsNull | EncodedScalarFilter::IsMissing => Some(&[0]),
    }
}

/// The posting key a resumed scalar walk should open at: the exact entry the
/// previous page stopped on. Both shapes of rank key that a scalar driver can
/// produce in its own walk order are covered -- a scalar ranking carries the
/// value key, and an id ranking over an equality driver has one value for the
/// whole range. Anything else returns `None` and the cursor opens where it
/// always did.
fn resume_scalar_key(
    info: &IndexInfo,
    predicate: &EncodedScalarFilter,
    after: &RankKey,
) -> Option<Vec<u8>> {
    let value = match (&after.value, predicate) {
        (RankValue::Scalar(key), _) => key.as_slice(),
        (RankValue::Entity, EncodedScalarFilter::Eq(value)) => value.as_slice(),
        _ => return None,
    };
    Some(super::indexes::skey(info, value, after.id.sequence))
}

/// The nullish scalar key. A NULL field and a MISSING one share it, so a
/// posting carrying it proves neither.
const NULLISH_SCALAR_KEY: &[u8] = &[0];

fn scalar_key_position(predicate: &EncodedScalarFilter, key: &[u8]) -> Ordering {
    match predicate {
        EncodedScalarFilter::Empty => Ordering::Greater,
        EncodedScalarFilter::Eq(value) => key.cmp(value),
        EncodedScalarFilter::Range { lower, upper } => {
            let below = match lower {
                EncodedBound::Included(value) => key < value,
                EncodedBound::Excluded(value) => key <= value,
                EncodedBound::Unbounded => false,
            };
            if below {
                return Ordering::Less;
            }
            let above = match upper {
                EncodedBound::Included(value) => key > value,
                EncodedBound::Excluded(value) => key >= value,
                EncodedBound::Unbounded => false,
            };
            if above {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }
        EncodedScalarFilter::IsNull | EncodedScalarFilter::IsMissing => key.cmp(&[0]),
    }
}

impl<'a> DriverCursor<'a> {
    /// Open the candidate stream for one page.
    ///
    /// `resume` is the previous page's last rank key, and is passed ONLY when
    /// the caller has established that this driver walks in the query's rank
    /// order (`PreparedQuery::driver_walks_in_rank_order`). Then the cursor
    /// opens at that key instead of at the start of the collection or posting
    /// range, so page k+1 costs what it emits rather than everything emitted
    /// so far. The resumed row is yielded once more and dropped by the
    /// `after` comparison in `next_page`: one candidate per page, where
    /// re-opening from the start costs one whole pass per page.
    fn new(
        db: &'a Database,
        collection: CollectionId,
        plan: &DriverPlan,
        graph: &[Option<BTreeSet<EntityId>>],
        needs: CursorNeeds,
        resume: Option<&RankKey>,
    ) -> QueryResult<Self> {
        match plan {
            DriverPlan::Entities => {
                let prefix = prefix(0x40, collection);
                let start = match resume {
                    Some(after) => row_key(after.id),
                    None => prefix.clone(),
                };
                let inner = db
                    .store()?
                    .range(&start)
                    .map_err(Error::from)
                    .map_err(QueryError::from)?;
                Ok(Self::Entities(EntityCursor {
                    inner,
                    prefix,
                    wants_row: needs.row,
                    done: false,
                }))
            }
            DriverPlan::Scalar {
                info,
                predicate,
                position,
            } => {
                let prefix = scalar_prefix(info.id);
                let inner = if matches!(predicate, EncodedScalarFilter::Empty) {
                    None
                } else {
                    let mut start = prefix.clone();
                    if let Some(lower) = scalar_lower(predicate) {
                        start.extend_from_slice(lower);
                    }
                    if let Some(key) = resume.and_then(|after| resume_scalar_key(info, predicate, after))
                    {
                        start = key;
                    }
                    db.index_range(info, &start)
                        .map_err(QueryError::from)?
                };
                // A posting key IS the predicate's proof: the walk yields an
                // entry only when `scalar_key_position` puts its value inside
                // the predicate, so re-reading the row to ask the same
                // question again costs a primary point-get per matched row and
                // can only give the same answer. The one value that does not
                // prove itself is the nullish sentinel, which a NULL and a
                // MISSING field share -- `scalar_filter_matches` tells those
                // apart and the index cannot -- so that key is left uncertified
                // per candidate below.
                let certifies = match (position, predicate) {
                    (
                        Some(position),
                        EncodedScalarFilter::Eq(_) | EncodedScalarFilter::Range { .. },
                    ) => Some(*position),
                    _ => None,
                };
                Ok(Self::Scalar(ScalarCursor {
                    inner,
                    prefix,
                    info: info.clone(),
                    predicate: predicate.clone(),
                    certifies,
                    wants_scalar: needs.scalar_key,
                    done: false,
                }))
            }
            DriverPlan::Graph { position } => {
                let ids = graph
                    .get(*position)
                    .and_then(Option::as_ref)
                    .ok_or_else(|| corrupt_query("missing prepared graph result"))?
                    .iter()
                    .copied()
                    .filter(|id| id.collection == collection)
                    .collect::<Vec<_>>();
                Ok(Self::Ids(ids.into_iter()))
            }
            DriverPlan::Spatial {
                info,
                predicate,
                position,
                ranges,
                ..
            } => Ok(Self::Spatial(SpatialCursor {
                db,
                info: info.clone(),
                predicate: *predicate,
                position: *position,
                prefix: super::spatial_indexes::posting_prefix(info.id),
                ranges: ranges.clone(),
                range: 0,
                inner: None,
                done: false,
            })),
            DriverPlan::Text { prepared, position } => {
                let mut streams = Vec::with_capacity(prepared.terms.len());
                for (term, expected) in prepared.terms.iter().zip(&prepared.dfs) {
                    streams.push(TextPostingCursor {
                        inner: super::text_indexes::TermPostings::open(
                            db,
                            prepared.info.id,
                            term,
                            *expected,
                        )?,
                        head: None,
                        done: false,
                    });
                }
                Ok(Self::Text(TextCursor {
                    streams,
                    collection: prepared.info.collection,
                    matching: prepared.matching,
                    position: *position,
                    initialized: false,
                    done: false,
                }))
            }
            DriverPlan::ExactVector { info } => {
                let prefix = super::vector_indexes::locator_prefix(info.id);
                let inner = db.store()?.range(&prefix).map_err(Error::from)?;
                Ok(Self::Vector(VectorCursor {
                    inner,
                    prefix,
                    info: info.clone(),
                    done: false,
                }))
            }
            DriverPlan::QuantizedVector { info } => {
                let prefix = super::quantized_vector_indexes::entry_prefix(info.id);
                let inner = db.store()?.range(&prefix).map_err(Error::from)?;
                Ok(Self::QuantizedVector(QuantizedVectorCursor {
                    inner,
                    prefix,
                    info: info.clone(),
                    done: false,
                }))
            }
        }
    }

    fn next<C: FnMut() -> bool>(
        &mut self,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Option<Candidate>> {
        match self {
            Self::Entities(cursor) => cursor.next(meter),
            Self::Scalar(cursor) => cursor.next(meter),
            Self::Spatial(cursor) => cursor.next(meter),
            Self::Text(cursor) => cursor.next(meter),
            Self::Vector(cursor) => cursor.next(meter),
            Self::QuantizedVector(cursor) => cursor.next(meter),
            Self::Ids(ids) => Ok(ids.next().map(|id| Candidate {
                id,
                row: None,
                scalar: None,
                vector: None,
                quantized: None,
                satisfied_filter: None,
            })),
        }
    }
}

impl EntityCursor<'_> {
    fn next<C: FnMut() -> bool>(
        &mut self,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Option<Candidate>> {
        if self.done {
            return Ok(None);
        }
        meter.charge(WorkResource::PrimaryReads, 1)?;
        // Borrow the record out of the pinned leaf rather than draining a key
        // `Vec` and a value `Vec` per row out of it. The pinned-leaf borrow
        // ends with this block so the cursor can step.
        let (id, row) = {
            let Some((key, value)) = self
                .inner
                .peek_ref()
                .map_err(Error::from)
                .map_err(QueryError::from)?
            else {
                self.done = true;
                return Ok(None);
            };
            if !key.starts_with(&self.prefix) {
                self.done = true;
                return Ok(None);
            }
            (
                row_id(key)?,
                if self.wants_row {
                    Some(value.to_vec())
                } else {
                    None
                },
            )
        };
        self.inner.step();
        Ok(Some(Candidate {
            id,
            row,
            scalar: None,
            vector: None,
            quantized: None,
            satisfied_filter: None,
        }))
    }
}

impl ScalarCursor<'_> {
    fn next<C: FnMut() -> bool>(
        &mut self,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Option<Candidate>> {
        if self.done {
            return Ok(None);
        }
        let Some(inner) = self.inner.as_mut() else {
            self.done = true;
            return Ok(None);
        };
        loop {
            meter.charge(WorkResource::ScalarPostings, 1)?;
            // Same pull-cursor shape as the entity walk: peek into the pinned
            // leaf, decide, then step. The key and the (always empty) value of
            // a posting were a `Vec` each before.
            let decoded = {
                let Some((key, value)) = inner
                    .peek_ref()
                    .map_err(Error::from)
                    .map_err(QueryError::from)?
                else {
                    self.done = true;
                    return Ok(None);
                };
                if !key.starts_with(&self.prefix) {
                    self.done = true;
                    return Ok(None);
                }
                let suffix = &key[self.prefix.len()..];
                let (_, value_len) = scalar_key::decode(&self.info.kind, suffix)?;
                let encoded = suffix
                    .get(..value_len)
                    .ok_or_else(|| corrupt_query("truncated scalar value key"))?;
                match scalar_key_position(&self.predicate, encoded) {
                    Ordering::Less => None,
                    Ordering::Greater => {
                        self.done = true;
                        return Ok(None);
                    }
                    Ordering::Equal => {
                        let mut at = self.prefix.len() + value_len;
                        let sequence = read_ordered(key, &mut at)?;
                        if at != key.len() || sequence == 0 || !value.is_empty() {
                            return Err(corrupt_query("scalar index entry"));
                        }
                        Some((
                            sequence,
                            if self.wants_scalar {
                                Some(encoded.to_vec())
                            } else {
                                None
                            },
                            encoded != NULLISH_SCALAR_KEY,
                        ))
                    }
                }
            };
            inner.step();
            let Some((sequence, encoded, proves_predicate)) = decoded else {
                continue;
            };
            return Ok(Some(Candidate {
                id: EntityId {
                    collection: self.info.collection,
                    sequence,
                },
                row: None,
                scalar: encoded.map(|key| (self.info.id, key)),
                vector: None,
                quantized: None,
                satisfied_filter: self.certifies.filter(|_| proves_predicate),
            }));
        }
    }
}

impl TextPostingCursor<'_> {
    fn advance<C: FnMut() -> bool>(&mut self, meter: &mut WorkMeter<'_, C>) -> QueryResult<()> {
        if self.done {
            return Ok(());
        }
        // One stream over both tiers: the packed segments a late build wrote
        // and the head rows written since, with head rows overriding.
        self.head = self
            .inner
            .next(&mut || meter.charge(WorkResource::TextPostings, 1))?
            .map(|(sequence, _)| sequence);
        if self.head.is_none() {
            self.done = true;
        }
        Ok(())
    }
}

impl TextCursor<'_> {
    fn drain<C: FnMut() -> bool>(&mut self, meter: &mut WorkMeter<'_, C>) -> QueryResult<()> {
        for stream in &mut self.streams {
            while !stream.done {
                stream.advance(meter)?;
            }
        }
        Ok(())
    }

    fn next<C: FnMut() -> bool>(
        &mut self,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Option<Candidate>> {
        if self.done {
            return Ok(None);
        }
        if !self.initialized {
            for stream in &mut self.streams {
                stream.advance(meter)?;
            }
            self.initialized = true;
        }
        if self.streams.is_empty() {
            self.done = true;
            return Ok(None);
        }
        let sequence = match self.matching {
            TextMatch::Any => {
                let Some(sequence) = self.streams.iter().filter_map(|stream| stream.head).min()
                else {
                    self.done = true;
                    return Ok(None);
                };
                for stream in &mut self.streams {
                    if stream.head == Some(sequence) {
                        stream.advance(meter)?;
                    }
                }
                sequence
            }
            TextMatch::All | TextMatch::Phrase => loop {
                if self.streams.iter().any(|stream| stream.done) {
                    self.drain(meter)?;
                    self.done = true;
                    return Ok(None);
                }
                let target = self
                    .streams
                    .iter()
                    .filter_map(|stream| stream.head)
                    .max()
                    .ok_or_else(|| corrupt_query("initialized text stream has no head"))?;
                for stream in &mut self.streams {
                    while stream.head.is_some_and(|sequence| sequence < target) {
                        stream.advance(meter)?;
                    }
                }
                if self.streams.iter().any(|stream| stream.done) {
                    continue;
                }
                if self
                    .streams
                    .iter()
                    .all(|stream| stream.head == Some(target))
                {
                    for stream in &mut self.streams {
                        stream.advance(meter)?;
                    }
                    break target;
                }
            },
        };
        Ok(Some(Candidate {
            id: EntityId {
                collection: self.collection,
                sequence,
            },
            row: None,
            scalar: None,
            vector: None,
            quantized: None,
            // Phrase postings establish only distinct all-term candidacy. The
            // filter remains pending until authoritative primary refinement.
            satisfied_filter: if self.matching == TextMatch::Phrase {
                None
            } else {
                self.position
            },
        }))
    }
}

impl SpatialCursor<'_> {
    fn next<C: FnMut() -> bool>(
        &mut self,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Option<Candidate>> {
        if self.done {
            return Ok(None);
        }
        loop {
            if self.range == self.ranges.len() {
                self.done = true;
                return Ok(None);
            }
            let (lo, hi) = self.ranges[self.range];
            if self.inner.is_none() {
                let mut start = self.prefix.clone();
                start.extend((lo as u32).to_be_bytes());
                // An index whose tree is still empty has no postings at all,
                // in any range: finish rather than walk the remaining ranges.
                let Some(iter) = self.db.index_range(&self.info, &start)? else {
                    self.done = true;
                    return Ok(None);
                };
                self.inner = Some(iter);
            }
            meter.charge(WorkResource::SpatialPostings, 1)?;
            let Some(row) = self.inner.as_mut().unwrap().next() else {
                self.inner = None;
                self.range += 1;
                continue;
            };
            let (key, value) = row.map_err(Error::from)?;
            if !key.starts_with(&self.prefix) {
                self.inner = None;
                self.range += 1;
                continue;
            }
            let hilbert = key
                .get(self.prefix.len()..self.prefix.len() + 4)
                .ok_or_else(|| corrupt_query("spatial point posting key"))?;
            let hilbert = u64::from(u32::from_be_bytes(hilbert.try_into().unwrap()));
            if hilbert > hi {
                self.inner = None;
                self.range += 1;
                continue;
            }
            let (_, sequence, point) =
                super::spatial_indexes::decode_posting(&self.prefix, &key, &value)?;
            let matches = match self.predicate {
                PointFilter::Bbox(bounds) => bounds.contains(point),
                PointFilter::Radius {
                    center,
                    radius_metres,
                } => within_radius(center, point, radius_metres).map_err(corrupt_query)?,
            };
            if !matches {
                continue;
            }
            return Ok(Some(Candidate {
                id: EntityId {
                    collection: self.info.collection,
                    sequence,
                },
                row: None,
                scalar: None,
                vector: None,
                quantized: None,
                satisfied_filter: Some(self.position),
            }));
        }
    }
}

impl VectorCursor<'_> {
    fn next<C: FnMut() -> bool>(
        &mut self,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Option<Candidate>> {
        if self.done {
            return Ok(None);
        }
        meter.charge(WorkResource::VectorLocators, 1)?;
        let Some(row) = self.inner.next() else {
            self.done = true;
            return Ok(None);
        };
        let (key, value) = row.map_err(Error::from)?;
        if !key.starts_with(&self.prefix) {
            self.done = true;
            return Ok(None);
        }
        let mut at = self.prefix.len();
        let sequence = read_ordered(&key, &mut at)?;
        if at != key.len() || sequence == 0 {
            return Err(corrupt_query("exact vector locator key"));
        }
        Ok(Some(Candidate {
            id: EntityId {
                collection: self.info.collection,
                sequence,
            },
            row: None,
            scalar: None,
            vector: Some((self.info.id, value)),
            quantized: None,
            satisfied_filter: None,
        }))
    }
}

impl QuantizedVectorCursor<'_> {
    fn next<C: FnMut() -> bool>(
        &mut self,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Option<Candidate>> {
        if self.done {
            return Ok(None);
        }
        // One charge covers the compact entry probe and its referenced layout
        // validation. Terminal and prefix-boundary probes are charged too.
        meter.charge(WorkResource::VectorLocators, 1)?;
        let Some(row) = self.inner.next() else {
            self.done = true;
            return Ok(None);
        };
        let (key, value) = row.map_err(Error::from)?;
        if !key.starts_with(&self.prefix) {
            self.done = true;
            return Ok(None);
        }
        let mut at = self.prefix.len();
        let sequence = read_ordered(&key, &mut at)?;
        if at != key.len() || sequence == 0 {
            return Err(corrupt_query("quantized vector entry key"));
        }
        Ok(Some(Candidate {
            id: EntityId {
                collection: self.info.collection,
                sequence,
            },
            row: None,
            scalar: None,
            vector: None,
            quantized: Some((self.info.id, value)),
            satisfied_filter: None,
        }))
    }
}

struct RowData {
    layout: Arc<Layout>,
    bytes: Vec<u8>,
}

fn decode_row(db: &Database, bytes: Vec<u8>) -> QueryResult<RowData> {
    let id = layout_id(&bytes)?;
    Ok(RowData {
        layout: db.layout(id)?,
        bytes,
    })
}

fn ensure_row<C: FnMut() -> bool>(
    db: &Database,
    id: EntityId,
    row: &mut Option<RowData>,
    encoded: &mut Option<Vec<u8>>,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<()> {
    if row.is_some() {
        return Ok(());
    }
    let bytes = if let Some(bytes) = encoded.take() {
        bytes
    } else {
        meter.charge(WorkResource::PrimaryReads, 1)?;
        db.store()?
            .get(&row_key(id))
            .map_err(Error::from)
            .map_err(QueryError::from)?
            .ok_or_else(|| corrupt_query("query candidate points to a missing entity"))?
    };
    *row = Some(decode_row(db, bytes)?);
    Ok(())
}

fn selected_field(row: &RowData, field: &str) -> QueryResult<dense_v3::FieldValue> {
    dense_v3::read_field(&row.layout, &row.bytes, field)
        .map_err(|error| corrupt_query(format!("dense-v3 row: {error}")))
}

fn persisted_scalar_key(
    info: &IndexInfo,
    value: dense_v3::FieldValue,
) -> QueryResult<Option<Vec<u8>>> {
    match value {
        dense_v3::FieldValue::Missing | dense_v3::FieldValue::Null => Ok(Some(vec![0])),
        dense_v3::FieldValue::Inline(value) => scalar_key::encode(&info.kind, Some(&value))
            .map(Some)
            .map_err(|error| corrupt_query(format!("indexed scalar row value: {error}"))),
        dense_v3::FieldValue::Vector { .. } => {
            Err(corrupt_query("scalar index field is a historical vector"))
        }
    }
}

fn scalar_filter_matches(
    info: &IndexInfo,
    predicate: &EncodedScalarFilter,
    value: dense_v3::FieldValue,
) -> QueryResult<bool> {
    match predicate {
        EncodedScalarFilter::Empty => Ok(false),
        EncodedScalarFilter::IsNull => Ok(matches!(value, dense_v3::FieldValue::Null)),
        EncodedScalarFilter::IsMissing => Ok(matches!(value, dense_v3::FieldValue::Missing)),
        EncodedScalarFilter::Eq(expected) => match value {
            dense_v3::FieldValue::Inline(value) => {
                let actual = scalar_key::encode(&info.kind, Some(&value))
                    .map_err(|error| corrupt_query(format!("indexed scalar row value: {error}")))?;
                Ok(&actual == expected)
            }
            dense_v3::FieldValue::Missing | dense_v3::FieldValue::Null => Ok(false),
            dense_v3::FieldValue::Vector { .. } => {
                Err(corrupt_query("scalar index field is a historical vector"))
            }
        },
        EncodedScalarFilter::Range { .. } => match value {
            dense_v3::FieldValue::Inline(value) => {
                let actual = scalar_key::encode(&info.kind, Some(&value))
                    .map_err(|error| corrupt_query(format!("indexed scalar row value: {error}")))?;
                Ok(scalar_key_position(predicate, &actual) == Ordering::Equal)
            }
            dense_v3::FieldValue::Missing | dense_v3::FieldValue::Null => Ok(false),
            dense_v3::FieldValue::Vector { .. } => {
                Err(corrupt_query("scalar index field is a historical vector"))
            }
        },
    }
}

fn scalar_eq_posting_matches<C: FnMut() -> bool>(
    db: &Database,
    info: &IndexInfo,
    expected: &[u8],
    id: EntityId,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<bool> {
    meter.charge(WorkResource::ScalarPostings, 1)?;
    let value = db
        .index_get(info, &super::indexes::skey(info, expected, id.sequence))
        .map_err(QueryError::from)?;
    match value {
        None => Ok(false),
        Some(value) if value.is_empty() => Ok(true),
        Some(_) => Err(corrupt_query("scalar index entry")),
    }
}

fn number_parts(number: &serde_json::Number) -> std::result::Result<i128, f64> {
    if number.is_f64() {
        return Err(number.as_f64().unwrap_or(0.0));
    }
    if let Some(value) = number.as_i64() {
        return Ok(i128::from(value));
    }
    if let Some(value) = number.as_u64() {
        return Ok(i128::from(value));
    }
    Err(number.as_f64().unwrap_or(0.0))
}

/// Compare an exact JSON integer to binary64 without first rounding the
/// integer. JSON's integer domain fits in i64/u64.
fn compare_integer_float(integer: i128, float: f64) -> Ordering {
    if float.is_nan() {
        return Ordering::Less;
    }
    if float >= 18_446_744_073_709_551_616.0 {
        return Ordering::Less;
    }
    if float < -9_223_372_036_854_775_808.0 {
        return Ordering::Greater;
    }
    let whole = float as i128;
    match integer.cmp(&whole) {
        Ordering::Equal if float.fract() > 0.0 => Ordering::Less,
        Ordering::Equal if float.fract() < 0.0 => Ordering::Greater,
        order => order,
    }
}

fn json_numbers_equal(left: &serde_json::Number, right: &serde_json::Number) -> bool {
    match (number_parts(left), number_parts(right)) {
        (Ok(left), Ok(right)) => left == right,
        (Ok(left), Err(right)) => compare_integer_float(left, right).is_eq(),
        (Err(left), Ok(right)) => compare_integer_float(right, left).is_eq(),
        (Err(left), Err(right)) => (left == 0.0 && right == 0.0) || left.total_cmp(&right).is_eq(),
    }
}

fn json_structural_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(left), Value::Bool(right)) => left == right,
        (Value::Number(left), Value::Number(right)) => json_numbers_equal(left, right),
        (Value::String(left), Value::String(right)) => left == right,
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| json_structural_equal(left, right))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, left)| {
                    right
                        .get(key)
                        .is_some_and(|right| json_structural_equal(left, right))
                })
        }
        _ => false,
    }
}

fn json_filter_matches(value: dense_v3::FieldValue, expected: &Value) -> QueryResult<bool> {
    match value {
        dense_v3::FieldValue::Missing => Ok(false),
        dense_v3::FieldValue::Null => Ok(expected.is_null()),
        dense_v3::FieldValue::Inline(actual) => Ok(json_structural_equal(&actual, expected)),
        dense_v3::FieldValue::Vector { .. } => {
            Err(corrupt_query("JSON equality field is a historical vector"))
        }
    }
}

fn point_from_field(value: dense_v3::FieldValue) -> QueryResult<Option<Point>> {
    let value = match value {
        dense_v3::FieldValue::Missing | dense_v3::FieldValue::Null => return Ok(None),
        dense_v3::FieldValue::Inline(value) => value,
        dense_v3::FieldValue::Vector { .. } => {
            return Err(corrupt_query("spatial point field is a historical vector"));
        }
    };
    let object = value
        .as_object()
        .ok_or_else(|| corrupt_query("indexed point is not an object"))?;
    if object.len() != 2 || object.get("type").and_then(Value::as_str) != Some("Point") {
        return Err(corrupt_query("indexed point is not a GeoJSON Point"));
    }
    let coordinates = object
        .get("coordinates")
        .and_then(Value::as_array)
        .filter(|coordinates| coordinates.len() == 2)
        .ok_or_else(|| corrupt_query("indexed point coordinate count"))?;
    let longitude = coordinates[0]
        .as_f64()
        .ok_or_else(|| corrupt_query("indexed point longitude"))?;
    let latitude = coordinates[1]
        .as_f64()
        .ok_or_else(|| corrupt_query("indexed point latitude"))?;
    Point::new(longitude, latitude)
        .map(Some)
        .map_err(corrupt_query)
}

fn text_score<C: FnMut() -> bool>(
    db: &Database,
    prepared: &PreparedText,
    id: EntityId,
    row: &mut Option<RowData>,
    encoded: &mut Option<Vec<u8>>,
    norms: &mut super::text_indexes::NormCache,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<Option<f64>> {
    if prepared.terms.is_empty() {
        return Ok(None);
    }
    meter.charge(WorkResource::TextPostings, 1)?;
    // Head row first, then the packed `0x7B` block -- the same lookup, and the
    // same decoding of the EMPTY head value, that every other norm reader
    // performs. A text index built after its corpus writes NO head row at all,
    // so reading `norm_key` alone drops every document it scores.
    let Some(length) =
        super::text_indexes::read_norm_cached(db, prepared.info.id, id.sequence, norms)?.length
    else {
        return Ok(None);
    };
    let mut frequencies = Vec::with_capacity(prepared.terms.len());
    let mut dfs = Vec::with_capacity(prepared.terms.len());
    let segments_on = super::text_indexes::segments_enabled(db);
    for (term, &df) in prepared.terms.iter().zip(&prepared.dfs) {
        meter.charge(WorkResource::TextPostings, 1)?;
        if let Some(frequency) = super::text_indexes::point_posting(
            db,
            prepared.info.id,
            term,
            id.sequence,
            segments_on,
        )? {
            frequencies.push(frequency);
            dfs.push(df);
        }
    }
    if frequencies.is_empty()
        || (matches!(prepared.matching, TextMatch::All | TextMatch::Phrase)
            && frequencies.len() != prepared.terms.len())
    {
        return Ok(None);
    }
    if let Some(phrase) = prepared.phrase.as_deref() {
        ensure_row(db, id, row, encoded, meter)?;
        let text = match selected_field(row.as_ref().unwrap(), &prepared.info.field)? {
            dense_v3::FieldValue::Inline(Value::String(text)) => text,
            dense_v3::FieldValue::Missing | dense_v3::FieldValue::Null => {
                return Err(corrupt_query(
                    "text posting points to absent authoritative primary text",
                ));
            }
            dense_v3::FieldValue::Inline(_) | dense_v3::FieldValue::Vector { .. } => {
                return Err(corrupt_query(
                    "text posting points to non-text authoritative primary field",
                ));
            }
        };
        let scanned = crate::text_analyzer::analyze_phrase_document(
            &text,
            phrase,
            |event| match event {
                crate::text_analyzer::PhraseScanEvent::Poll => meter.check_cancelled(),
                crate::text_analyzer::PhraseScanEvent::Token => {
                    meter.charge(WorkResource::TextTokens, 1)
                }
            },
        );
        let (analysis, matched) = match scanned {
            Ok(result) => result,
            Err(crate::text_analyzer::PhraseScanError::Analysis(error)) => {
                return Err(corrupt_query(format!(
                    "authoritative text violates analyzer bounds: {error}"
                )));
            }
            Err(crate::text_analyzer::PhraseScanError::Callback(error)) => return Err(error),
        };
        if analysis.length != length {
            return Err(corrupt_query(
                "text norm disagrees with authoritative primary text",
            ));
        }
        for (term, frequency) in prepared.terms.iter().zip(&frequencies) {
            if analysis.terms.get(term).copied() != Some(*frequency) {
                return Err(corrupt_query(
                    "text posting frequency disagrees with authoritative primary text",
                ));
            }
        }
        if !matched {
            return Ok(None);
        }
    }
    super::text_indexes::bm25(prepared.corpus, length, &frequencies, &dfs)
        .map(Some)
        .map_err(QueryError::from)
}

fn vector_score<C: FnMut() -> bool>(
    db: &Database,
    candidate: &Candidate,
    info: &IndexInfo,
    query: &[f32],
    query_norm: f64,
    metric: VectorMetric,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<Option<f64>> {
    let locator = candidate
        .vector
        .as_ref()
        .filter(|(index, _)| *index == info.id)
        .map(|(_, locator)| locator.clone());
    let locator = match locator {
        Some(locator) => locator,
        None => {
            meter.charge(WorkResource::VectorLocators, 1)?;
            let Some(locator) = db.store()?.get(&super::vector_indexes::locator_key(
                info.id,
                candidate.id.sequence,
            ))?
            else {
                return Ok(None);
            };
            locator
        }
    };
    meter.charge(WorkResource::VectorSidecars, 1)?;
    meter.charge(
        WorkResource::VectorLanes,
        u64::try_from(super::vector_indexes::dimension(info)?).map_err(invalid_query)?,
    )?;
    db.score_locator_cancelled(
        info,
        candidate.id,
        &locator,
        query,
        query_norm,
        metric,
        &mut || (meter.cancelled)(),
    )
    .map(|hit| hit.map(|hit| hit.distance))
    .map_err(QueryError::from)
}

fn quantized_metric(metric: VectorMetric) -> crate::vector_quant::Metric {
    match metric {
        VectorMetric::Cosine => crate::vector_quant::Metric::Cosine,
        VectorMetric::SquaredL2 => crate::vector_quant::Metric::SquaredL2,
        VectorMetric::NegativeDot => crate::vector_quant::Metric::NegativeDot,
    }
}

fn approximate_vector_score<C: FnMut() -> bool>(
    db: &Database,
    candidate: &Candidate,
    info: &IndexInfo,
    query: &[f32],
    metric: VectorMetric,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<Option<(f64, [u8; 6])>> {
    let value = candidate
        .quantized
        .as_ref()
        .filter(|(index, _)| *index == info.id)
        .map(|(_, value)| value.clone());
    let value = match value {
        Some(value) => value,
        None => {
            meter.charge(WorkResource::VectorLocators, 1)?;
            let Some(value) = db
                .store()?
                .get(&super::quantized_vector_indexes::entry_key(
                    info.id,
                    candidate.id.sequence,
                ))?
            else {
                return Ok(None);
            };
            value
        }
    };
    let dimension = super::quantized_vector_indexes::dimension(info)?;
    meter.charge(
        WorkResource::VectorLanes,
        u64::try_from(dimension).map_err(invalid_query)?,
    )?;
    let (locator, decoded) = super::quantized_vector_indexes::decode_entry(&value, dimension)?;
    super::quantized_vector_indexes::validate_locator(db, info, &locator)?;
    let score = decoded.score(query, quantized_metric(metric), || (meter.cancelled)());
    match score {
        Ok(Some(distance)) => Ok(Some((distance, locator))),
        Ok(None) => Ok(None),
        Err(crate::vector_quant::Error::Cancelled) => Err(QueryError::Cancelled),
        Err(crate::vector_quant::Error::Invalid(_)) => {
            Err(corrupt_query("quantized vector approximate scoring"))
        }
    }
}

fn rerank_quantized_vector<C: FnMut() -> bool>(
    db: &Database,
    info: &IndexInfo,
    candidate: &ApproxHeapEntry,
    query: &[f32],
    query_norm: f64,
    metric: VectorMetric,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<Option<f64>> {
    let dimension = super::quantized_vector_indexes::dimension(info)?;
    // This probe covers historical layout/ordinal validation and the compact
    // entry re-read used for the authoritative consistency check.
    meter.charge(WorkResource::VectorLocators, 1)?;
    let ordinal = super::quantized_vector_indexes::validate_locator(db, info, &candidate.locator)?;
    meter.charge(WorkResource::VectorSidecars, 1)?;
    let raw = db
        .store()?
        .get(&vector_key(candidate.id, ordinal))?
        .ok_or_else(|| corrupt_query("quantized vector locator points to missing sidecar"))?;
    let persisted = db
        .store()?
        .get(&super::quantized_vector_indexes::entry_key(
            info.id,
            candidate.id.sequence,
        ))?
        .ok_or_else(|| corrupt_query("quantized shortlist entry disappeared"))?;
    meter.charge(
        WorkResource::VectorLanes,
        u64::try_from(dimension).map_err(invalid_query)?,
    )?;
    if super::quantized_vector_indexes::encode_entry(candidate.locator, &raw, dimension)?
        != persisted
    {
        return Err(corrupt_query(
            "quantized vector entry differs from authoritative sidecar",
        ));
    }
    meter.charge(
        WorkResource::VectorLanes,
        u64::try_from(dimension).map_err(invalid_query)?,
    )?;
    super::quantized_vector_indexes::exact_score(
        &raw,
        dimension,
        query,
        query_norm,
        metric,
        &mut || (meter.cancelled)(),
    )
    .map_err(QueryError::from)
}

fn filters_match<C: FnMut() -> bool>(
    db: &Database,
    filters: &[CompiledFilter],
    candidate: &Candidate,
    row: &mut Option<RowData>,
    encoded: &mut Option<Vec<u8>>,
    graph: &[Option<BTreeSet<EntityId>>],
    norms: &mut super::text_indexes::NormCache,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<bool> {
    for (position, filter) in filters.iter().enumerate() {
        meter.check_cancelled()?;
        if candidate.satisfied_filter == Some(position) {
            continue;
        }
        let id = candidate.id;
        let matches = match filter {
            CompiledFilter::Scalar {
                info,
                predicate,
                posting_membership,
            } => match (
                *posting_membership,
                predicate,
                row.is_none() && encoded.is_none(),
            ) {
                (true, EncodedScalarFilter::Eq(expected), true) => {
                    scalar_eq_posting_matches(db, info, expected, id, meter)?
                }
                _ => {
                    ensure_row(db, id, row, encoded, meter)?;
                    let row = row.as_ref().unwrap();
                    scalar_filter_matches(info, predicate, selected_field(row, &info.field)?)?
                }
            },
            CompiledFilter::JsonEq { field, value } => {
                ensure_row(db, id, row, encoded, meter)?;
                let row = row.as_ref().unwrap();
                json_filter_matches(selected_field(row, field)?, value)?
            }
            CompiledFilter::Graph { position, .. } => graph
                .get(*position)
                .and_then(Option::as_ref)
                .is_some_and(|ids| ids.contains(&id)),
            CompiledFilter::Point { info, predicate } => {
                ensure_row(db, id, row, encoded, meter)?;
                let row = row.as_ref().unwrap();
                meter.charge(WorkResource::SpatialPostings, 1)?;
                let Some(point) = point_from_field(selected_field(row, &info.field)?)? else {
                    return Ok(false);
                };
                match predicate {
                    PointFilter::Bbox(bounds) => bounds.contains(point),
                    PointFilter::Radius {
                        center,
                        radius_metres,
                    } => within_radius(*center, point, *radius_metres).map_err(corrupt_query)?,
                }
            }
            CompiledFilter::Text(prepared) => {
                text_score(db, prepared, id, row, encoded, norms, meter)?.is_some()
            }
        };
        if !matches {
            return Ok(false);
        }
    }
    Ok(true)
}

fn rank_candidate<C: FnMut() -> bool>(
    db: &Database,
    order: &CompiledOrder,
    candidate: &Candidate,
    row: &mut Option<RowData>,
    encoded: &mut Option<Vec<u8>>,
    norms: &mut super::text_indexes::NormCache,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<Option<RankKey>> {
    let value = match order {
        CompiledOrder::EntityId => RankValue::Entity,
        CompiledOrder::Scalar { info, .. } => {
            let key = candidate
                .scalar
                .as_ref()
                .filter(|(index, _)| *index == info.id)
                .map(|(_, key)| key.clone());
            let key = match key {
                Some(key) => key,
                None => {
                    ensure_row(db, candidate.id, row, encoded, meter)?;
                    persisted_scalar_key(info, selected_field(row.as_ref().unwrap(), &info.field)?)?
                        .ok_or_else(|| corrupt_query("missing scalar order key"))?
                }
            };
            RankValue::Scalar(key)
        }
        CompiledOrder::ExactVector {
            info,
            query,
            query_norm,
            metric,
        } => {
            let Some(distance) =
                vector_score(db, candidate, info, query, *query_norm, *metric, meter)?
            else {
                return Ok(None);
            };
            RankValue::Score(distance.to_bits())
        }
        CompiledOrder::ApproximateVector { .. } => {
            unreachable!("approximate order uses shortlist then exact rerank")
        }
        CompiledOrder::Bm25(prepared) => {
            let Some(score) = text_score(
                db,
                prepared,
                candidate.id,
                row,
                encoded,
                norms,
                meter,
            )? else {
                return Ok(None);
            };
            RankValue::Score(score.to_bits())
        }
    };
    Ok(Some(RankKey {
        value,
        id: candidate.id,
    }))
}

fn scalar_order_value(info: &IndexInfo, key: &[u8]) -> QueryResult<OwnedScalarValue> {
    let (value, consumed) = scalar_key::decode(&info.kind, key)?;
    if consumed != key.len() {
        return Err(corrupt_query("scalar order key has a suffix"));
    }
    match (&info.kind, value) {
        (_, Value::Null) => Ok(OwnedScalarValue::Nullish),
        (Kind::Bool, Value::Bool(value)) => Ok(OwnedScalarValue::Bool(value)),
        (Kind::Int, Value::Number(value)) => value
            .as_i64()
            .map(OwnedScalarValue::I64)
            .ok_or_else(|| corrupt_query("scalar integer order value")),
        (Kind::Real, Value::Number(value)) => value
            .as_f64()
            .map(OwnedScalarValue::F64)
            .ok_or_else(|| corrupt_query("scalar real order value")),
        (Kind::Text, Value::String(value)) => Ok(OwnedScalarValue::Text(value)),
        _ => Err(corrupt_query("scalar order value kind")),
    }
}

fn project_value(value: dense_v3::FieldValue) -> QueryResult<ProjectedValue> {
    match value {
        dense_v3::FieldValue::Missing => Ok(ProjectedValue::Missing),
        dense_v3::FieldValue::Null => Ok(ProjectedValue::Null),
        dense_v3::FieldValue::Inline(value) => Ok(ProjectedValue::Value(value)),
        dense_v3::FieldValue::Vector { .. } => unreachable!("vector projection needs sidecar"),
    }
}

fn project_field<C: FnMut() -> bool>(
    db: &Database,
    id: EntityId,
    row: &RowData,
    field: &str,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<ProjectedValue> {
    match selected_field(row, field)? {
        dense_v3::FieldValue::Vector { ordinal, dimension } => {
            meter.charge(WorkResource::VectorSidecars, 1)?;
            meter.charge(
                WorkResource::VectorLanes,
                u64::try_from(dimension).map_err(invalid_query)?,
            )?;
            let bytes = db
                .store()?
                .get(&vector_key(id, ordinal))?
                .ok_or_else(|| corrupt_query("projected vector sidecar is missing"))?;
            super::vector_indexes::validate_vector(&bytes, dimension)?;
            Ok(ProjectedValue::Value(
                crate::vector_json(&bytes, dimension).map_err(corrupt_query)?,
            ))
        }
        value => project_value(value),
    }
}

fn checked_output_size(row: &QueryRow) -> QueryResult<u64> {
    let mut size = 12u64; // collection u32 + sequence u64
    size = size.checked_add(1).ok_or_else(|| {
        QueryError::Database(Error::InvalidInput("query output size overflow".into()))
    })?;
    let order_bytes = match &row.order {
        OrderValue::Scalar(value) => match value {
            OwnedScalarValue::Nullish => 1,
            OwnedScalarValue::Bool(_) => 2,
            OwnedScalarValue::I64(_) | OwnedScalarValue::F64(_) => 9,
            OwnedScalarValue::Text(value) => 1u64
                .checked_add(u64::try_from(value.len()).map_err(invalid_query)?)
                .ok_or_else(|| invalid_query("query output size overflow"))?,
        },
        OrderValue::Distance(_) | OrderValue::Bm25(_) => 9,
        OrderValue::EntityId => 0,
    };
    if order_bytes != 0 {
        size = size
            .checked_add(order_bytes)
            .ok_or_else(|| invalid_query("query output size overflow"))?;
    }
    for (field, value) in &row.projected {
        size = size
            .checked_add(u64::try_from(field.len()).map_err(invalid_query)?)
            .ok_or_else(|| invalid_query("query output size overflow"))?;
        size = size
            .checked_add(1)
            .ok_or_else(|| invalid_query("query output size overflow"))?;
        let bytes = match value {
            ProjectedValue::Missing | ProjectedValue::Null => 0,
            ProjectedValue::Value(value) => {
                u64::try_from(serde_json::to_vec(value).map_err(invalid_query)?.len())
                    .map_err(invalid_query)?
            }
        };
        size = size
            .checked_add(bytes)
            .ok_or_else(|| invalid_query("query output size overflow"))?;
    }
    Ok(size)
}

impl PreparedQuery<'_> {
    /// True when the driver's own walk hands candidates over in exactly the
    /// order this query ranks them by. Two things follow, and only under this
    /// condition:
    ///
    ///   * a page may STOP the moment its heap is full -- nothing later in the
    ///     walk can outrank what is already there -- which is what turns a
    ///     LIMIT from a filter over N rows into a stop condition;
    ///   * the next page may RESUME at this page's last key instead of
    ///     re-walking everything already emitted.
    ///
    /// The cases are the ones where the key the tree is sorted by and the key
    /// the query sorts by are the same key. The entity cursor walks the
    /// primary tree in id order and `EntityId` ranks by id. An EQUALITY
    /// posting range holds one value for all its entries, so it is ordered by
    /// sequence alone -- also id order. An ascending scalar order driven by
    /// that same index walks `value || sequence`, which is `(value, id)`.
    ///
    /// Everything else is excluded on purpose: a descending order walks
    /// against its ranking, a ranked order (BM25, vector distance) has no
    /// relation to any tree's order and must see every candidate before it
    /// knows its top k, and a range or nullish driver under an id order walks
    /// by value while ranking by id.
    fn driver_walks_in_rank_order(&self) -> bool {
        match (&self.driver, &self.order) {
            (DriverPlan::Entities, CompiledOrder::EntityId) => true,
            (
                DriverPlan::Scalar {
                    predicate: EncodedScalarFilter::Eq(_),
                    ..
                },
                CompiledOrder::EntityId,
            ) => true,
            (
                DriverPlan::Scalar { info, .. },
                CompiledOrder::Scalar {
                    info: order,
                    direction: SortDirection::Ascending,
                },
            ) => info.id == order.id,
            _ => false,
        }
    }

    /// What the page will actually read off each candidate. A driver holding
    /// borrowed bytes copies them only for something that will be read.
    fn cursor_needs(&self) -> CursorNeeds {
        CursorNeeds {
            // The entity cursor's row bytes save `ensure_row` a point-get, but
            // only if something decodes them. With no filters and an id
            // ranking, nothing does.
            row: !self.filters.is_empty() || !matches!(self.order, CompiledOrder::EntityId),
            // The posting's value key is read only by a scalar ranking over
            // the very index that produced it.
            scalar_key: match (&self.driver, &self.order) {
                (DriverPlan::Scalar { info, .. }, CompiledOrder::Scalar { info: order, .. }) => {
                    info.id == order.id
                }
                _ => false,
            },
        }
    }

    /// Whether a winner's identity is already established without going back
    /// to the primary tree.
    ///
    /// SQLite answers a key-only query out of a covering index and never
    /// touches the row table. The same holds here when the projection is empty
    /// AND the driver's own stream is the authority for the row: the entity
    /// cursor read the primary record itself, and an equality posting is the
    /// membership record this engine already trusts elsewhere --
    /// `scalar_eq_posting_matches` answers a non-driving equality filter from
    /// the posting alone, without a row.
    ///
    /// Every other driver stays as it was. A range or order scalar walk, graph
    /// ids, a text posting, a spatial cell, a vector locator can each name a
    /// row that is no longer there, and those still fetch it and still refuse
    /// an orphan.
    fn winner_needs_no_row(&self) -> bool {
        self.projection.is_empty()
            && match &self.driver {
                DriverPlan::Entities => true,
                DriverPlan::Scalar {
                    predicate: EncodedScalarFilter::Eq(_),
                    position: Some(_),
                    ..
                } => true,
                _ => false,
            }
    }

    pub fn next_page<C: FnMut() -> bool>(
        &mut self,
        page_size: usize,
        budget: QueryBudget,
        mut cancelled: C,
    ) -> QueryResult<QueryPage> {
        if page_size == 0 || page_size > MAX_PAGE_SIZE {
            return Err(invalid_query("query page size requires 1..8192 rows"));
        }
        let remaining = self
            .total_limit
            .map_or(usize::MAX, |limit| limit.saturating_sub(self.emitted));
        if remaining == 0 {
            return Ok(QueryPage {
                rows: Vec::new(),
                done: true,
                driver: self.driver.diagnostic(),
                work: QueryWork::default(),
                approximation: None,
            });
        }
        let wanted = page_size.min(remaining);
        let needs_extra = remaining > wanted;
        let capacity = wanted + usize::from(needs_extra);
        let descending = matches!(
            self.order,
            CompiledOrder::Scalar {
                direction: SortDirection::Descending,
                ..
            } | CompiledOrder::Bm25(_)
        );
        let mut meter = WorkMeter::new(budget, &mut cancelled);
        meter.check_cancelled()?;
        // One decoded `0x7B` norm block held for the page. Candidates that
        // arrive in ascending sequence -- the text and entity cursors -- reuse
        // it 255 times out of 256; one that does not simply re-decodes.
        let mut norms = super::text_indexes::NormCache::default();
        let graph = execute_graph_filters(self.db, &self.filters, &mut meter)?;
        let in_rank_order = self.driver_walks_in_rank_order();
        let mut driver = DriverCursor::new(
            self.db,
            self.collection,
            &self.driver,
            &graph,
            self.cursor_needs(),
            self.after.as_ref().filter(|_| in_rank_order),
        )?;
        let mut heap = BinaryHeap::with_capacity(capacity);
        let approximation =
            if let CompiledOrder::ApproximateVector {
                info,
                query,
                query_norm,
                metric,
                ef,
            } = &self.order
            {
                let mut examined = 0usize;
                let mut shortlist = BinaryHeap::with_capacity((*ef).min(1024));
                while let Some(mut candidate) = driver.next(&mut meter)? {
                    meter.charge(WorkResource::Candidates, 1)?;
                    if candidate.id.collection != self.collection {
                        return Err(corrupt_query("query driver crossed collection boundary"));
                    }
                    let mut encoded = candidate.row.take();
                    let mut row = None;
                    if !filters_match(
                        self.db,
                        &self.filters,
                        &candidate,
                        &mut row,
                        &mut encoded,
                        &graph,
                        &mut norms,
                        &mut meter,
                    )? {
                        continue;
                    }
                    examined = examined
                        .checked_add(1)
                        .ok_or_else(|| invalid_query("approximate examined count overflow"))?;
                    let Some((distance, locator)) = approximate_vector_score(
                        self.db, &candidate, info, query, *metric, &mut meter,
                    )?
                    else {
                        continue;
                    };
                    shortlist.push(ApproxHeapEntry {
                        distance,
                        id: candidate.id,
                        locator,
                    });
                    if shortlist.len() > *ef {
                        shortlist.pop();
                    }
                }

                let reranked = shortlist.len();
                for candidate in shortlist {
                    let Some(distance) = rerank_quantized_vector(
                        self.db,
                        info,
                        &candidate,
                        query,
                        *query_norm,
                        *metric,
                        &mut meter,
                    )?
                    else {
                        continue;
                    };
                    let key = RankKey {
                        value: RankValue::Score(distance.to_bits()),
                        id: candidate.id,
                    };
                    if self
                        .after
                        .as_ref()
                        .is_some_and(|after| compare_rank(&key, after, false) != Ordering::Greater)
                    {
                        continue;
                    }
                    let entry = HeapEntry {
                        key,
                        descending: false,
                    };
                    if heap.len() < capacity {
                        heap.push(entry);
                    } else if heap
                        .peek()
                        .is_some_and(|worst| entry.cmp(worst) == Ordering::Less)
                    {
                        heap.pop();
                        heap.push(entry);
                    }
                }
                Some(ApproximationDiagnostics {
                    method: ApproxVectorMethod::SymmetricInt8ScanV1,
                    ef: *ef,
                    examined,
                    reranked,
                })
            } else {
                while let Some(mut candidate) = driver.next(&mut meter)? {
                    meter.charge(WorkResource::Candidates, 1)?;
                    if candidate.id.collection != self.collection {
                        return Err(corrupt_query("query driver crossed collection boundary"));
                    }
                    let mut encoded = candidate.row.take();
                    let mut row = None;
                    if !filters_match(
                        self.db,
                        &self.filters,
                        &candidate,
                        &mut row,
                        &mut encoded,
                        &graph,
                        &mut norms,
                        &mut meter,
                    )? {
                        continue;
                    }
                    let Some(key) = rank_candidate(
                        self.db,
                        &self.order,
                        &candidate,
                        &mut row,
                        &mut encoded,
                        &mut norms,
                        &mut meter,
                    )?
                    else {
                        continue;
                    };
                    if self.after.as_ref().is_some_and(|after| {
                        compare_rank(&key, after, descending) != Ordering::Greater
                    }) {
                        continue;
                    }
                    let entry = HeapEntry { key, descending };
                    if heap.len() < capacity {
                        heap.push(entry);
                    } else if heap
                        .peek()
                        .is_some_and(|worst| entry.cmp(worst) == Ordering::Less)
                    {
                        heap.pop();
                        heap.push(entry);
                    }
                    // The page is full and the walk is already in rank order,
                    // so every candidate still ahead ranks after everything
                    // held. Without this, `LIMIT 10` reads the whole
                    // collection to answer with ten rows, and a page of a scan
                    // reads to the end of the collection to fill 8,192 rows.
                    if in_rank_order && heap.len() >= capacity {
                        break;
                    }
                }
                None
            };

        let mut winners = heap.into_vec();
        winners.sort_by(|left, right| compare_rank(&left.key, &right.key, descending));
        let has_more = winners.len() > wanted;
        winners.truncate(wanted);
        let winner_needs_no_row = self.winner_needs_no_row();
        let mut rows = Vec::with_capacity(winners.len());
        for winner in &winners {
            let order = match (&self.order, &winner.key.value) {
                (CompiledOrder::EntityId, RankValue::Entity) => OrderValue::EntityId,
                (CompiledOrder::Scalar { info, .. }, RankValue::Scalar(key)) => {
                    OrderValue::Scalar(scalar_order_value(info, key)?)
                }
                (CompiledOrder::ExactVector { .. }, RankValue::Score(score)) => {
                    OrderValue::Distance(f64::from_bits(*score))
                }
                (CompiledOrder::ApproximateVector { .. }, RankValue::Score(score)) => {
                    OrderValue::Distance(f64::from_bits(*score))
                }
                (CompiledOrder::Bm25(_), RankValue::Score(score)) => {
                    OrderValue::Bm25(f64::from_bits(*score))
                }
                _ => unreachable!("prepared order and rank key agree"),
            };
            // Every returned ID must still have an authoritative primary row.
            // This remains winner-only so native index scans do not pay a
            // primary point-get for every rejected candidate -- and it is
            // skipped entirely when the driver is already that authority and
            // no field is projected (`winner_needs_no_row`), because then the
            // fetch decodes nothing and only re-proves what the candidate
            // stream proved.
            let mut projected = Vec::with_capacity(self.projection.len());
            if !winner_needs_no_row {
                meter.charge(WorkResource::PrimaryReads, 1)?;
                let bytes = self
                    .db
                    .store()?
                    .get(&row_key(winner.key.id))
                    .map_err(Error::from)
                    .map_err(QueryError::from)?
                    .ok_or_else(|| corrupt_query("query winner is missing its entity"))?;
                if !self.projection.is_empty() {
                    let row = decode_row(self.db, bytes)?;
                    for field in &self.projection {
                        meter.check_cancelled()?;
                        projected.push((
                            field.clone(),
                            project_field(self.db, winner.key.id, &row, field, &mut meter)?,
                        ));
                    }
                }
            }
            let row = QueryRow {
                id: winner.key.id,
                order,
                projected,
            };
            meter.charge(WorkResource::OutputBytes, checked_output_size(&row)?)?;
            rows.push(row);
        }

        let next_after = winners.last().map(|winner| winner.key.clone());
        let next_emitted = self
            .emitted
            .checked_add(rows.len())
            .ok_or_else(|| invalid_query("query total output overflow"))?;
        let hit_total_limit = self.total_limit.is_some_and(|limit| next_emitted >= limit);
        self.after = next_after.or_else(|| self.after.clone());
        self.emitted = next_emitted;
        Ok(QueryPage {
            rows,
            done: hit_total_limit || !has_more,
            driver: self.driver.diagnostic(),
            work: meter.used,
            approximation,
        })
    }
}
