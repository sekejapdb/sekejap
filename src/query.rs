//! Combined queries over scalar, graph, point, geometry, text and vector families.
//! Every constraint runs before ranked top-k; pages report their selected
//! complete driver, logical work and any explicit approximation diagnostics.
use super::*;
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

pub use kernel::spatial::Geom;

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
    /// Mapping-keyspace entries walked by `CandidateDriver::Keys`.
    pub key_postings: u64,
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
            output_bytes: u64::MAX,
        }
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
            WorkResource::KeyPostings => (&mut self.used.key_postings, self.limit.key_postings),
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

/// One non-driving scalar RANGE filter's posting range, walked ONCE and kept
/// for the rest of the query's pages.
///
/// An equality filter already answers a non-driving candidate from its own
/// posting (`scalar_eq_posting_matches`): the key `value || entity` is known,
/// so testing it costs one point read. A range predicate cannot do that --
/// the candidate's value is unknown until something reads it -- so the walk
/// this holds is the index-side answer Postgres gets from a bitmap AND of two
/// posting lists: every entity the range's own postings prove, collected
/// once, so a candidate afterwards is a binary search instead of a primary
/// read.
#[derive(Clone, Debug)]
enum ScalarRangeSet {
    /// This position is not a candidate for the optimization: not a Range
    /// predicate, or the position driving the query (whose own walk already
    /// answers it for free).
    Ineligible,
    /// Eligible, but no page has walked it yet.
    Unbuilt,
    /// The range's own collection span (every sequence it could ever name) is
    /// too wide for even a bitmap to fit the budget, or the walk found more
    /// entities than the plain-Vec cap allows while a bitmap was not a viable
    /// fallback either. Named sacrifice (Law 4): a range wide enough to fail
    /// this budget gets no faster than it already was -- every candidate
    /// still reads its row -- rather than holding an unbounded set in memory.
    Overflow,
    /// Every entity SEQUENCE the posting range proved, ascending, so
    /// `binary_search` answers membership. Chosen over a bitmap when the
    /// range is narrow enough, in a large enough collection, that the Vec is
    /// the smaller of the two -- e.g. one day out of decades of `born`
    /// values.
    Ids(Vec<u64>),
    /// One bit per sequence in `1..=collection_span`, set for every entity
    /// SEQUENCE the posting range proved. Chosen once the Vec representation
    /// would be bigger than this: unlike the Vec, setting a bit costs no sort
    /// and no allocation growth once the bitmap is sized, so a wide range
    /// (e.g. a whole decade of `born`) is one linear pass, no CPU cost from
    /// the postings count once past the initial allocation.
    Bitmap(Vec<u8>),
}

/// How many entity ids one [`ScalarRangeSet`] may hold as a plain `Vec`
/// before it is either converted to a [`ScalarRangeSet::Bitmap`] (when one
/// would fit the budget) or abandoned as [`ScalarRangeSet::Overflow`] (when
/// even a bitmap would not), in the same currency [`RUN_BYTES`] already
/// bounds a held run in: both are memory one page keeps beyond what it
/// returns this call.
///
/// This is also the ceiling used when a bitmap is not viable at all (the
/// collection's span alone would need more than `RUN_BYTES` of bits) -- the
/// same cap the Vec-only design used before bitmaps existed, so a collection
/// too large even for a bitmap degrades to exactly that prior behaviour
/// rather than something new.
const SCALAR_RANGE_SET_CAP: usize = RUN_BYTES / std::mem::size_of::<u64>();

/// A bitmap large enough to need more than this many bytes is not a viable
/// [`ScalarRangeSet::Bitmap`]: `RUN_BYTES` is the same per-page memory
/// currency the Vec cap above is drawn from. One bit per sequence, so a
/// collection whose span exceeds `8 * RUN_BYTES` sequences (about 67
/// million entities) never gets a bitmap here, whatever the range's own
/// selectivity.
const SCALAR_RANGE_BITMAP_CAP_BYTES: usize = RUN_BYTES;

/// The number of bytes a bitmap covering sequences `1..=span` would need.
fn scalar_range_bitmap_bytes(span: u64) -> u64 {
    span.div_ceil(8)
}

/// Sets the bit for `sequence` (1-based, as every allocated entity sequence
/// is) in a bitmap sized by [`scalar_range_bitmap_bytes`].
fn scalar_range_bitmap_set(bits: &mut [u8], sequence: u64) {
    let index = (sequence - 1) as usize;
    bits[index / 8] |= 1 << (index % 8);
}

/// Tests the bit for `sequence` (1-based). A sequence at or past the
/// bitmap's span was never allocated when the bitmap was built and so was
/// never set -- `false`, not a panic or an out-of-bounds read.
fn scalar_range_bitmap_contains(bits: &[u8], sequence: u64) -> bool {
    let index = (sequence - 1) as usize;
    bits.get(index / 8).is_some_and(|byte| byte & (1 << (index % 8)) != 0)
}

#[derive(Clone, Debug)]
enum CompiledFilter {
    Scalar {
        info: IndexInfo,
        predicate: EncodedScalarFilter,
        posting_membership: bool,
    },
    /// A scalar filter whose predicate was folded into `into` -- another
    /// position naming the SAME index -- by [`fold_same_index_scalars`].
    ///
    /// The position survives because positions are user-visible: a caller may
    /// name any of them as `CandidateDriver::Filter`, and a candidate's
    /// `satisfied_filter` is a position. What does not survive is the WORK:
    /// the surviving predicate is the conjunction, so this slot is already
    /// answered for every candidate that reaches it and it reads nothing.
    Folded {
        into: usize,
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
    Geometry {
        info: IndexInfo,
        predicate: GeometryFilter,
    },
    Text(PreparedText),
    /// A range over the external-key mapping keyspace. Reuses
    /// `EncodedScalarFilter`'s `Range`/`Empty` shape -- `scalar_key_position`
    /// and `range_or_empty` are pure byte-range logic with nothing
    /// scalar-index-specific in them, and a key has no `Eq`/`IsNull`/
    /// `IsMissing` counterpart (every mapping entry is a present real key).
    /// Meaningful only at the position `DriverPlan::Keys` certifies;
    /// `prepare_query` refuses any other placement.
    Key {
        predicate: EncodedScalarFilter,
    },
}

#[derive(Clone, Debug)]
struct PreparedText {
    info: IndexInfo,
    terms: Vec<String>,
    phrase: Option<Vec<String>>,
    /// The phrase's KMP failure function. It depends on the phrase alone, and
    /// the document scanner used to rebuild it per document.
    phrase_prefix: Vec<usize>,
    matching: TextMatch,
    dfs: Vec<u64>,
    /// The corpus half of every BM25 score this query can produce, and one
    /// inverse document frequency per term. Both are settled the moment the
    /// query is prepared; both used to be recomputed inside the per-document
    /// loop, the idf with a natural logarithm.
    weights: super::text_indexes::Bm25Weights,
    idfs: Vec<f64>,
    /// Which candidate driver, if any, hands this prepared query the term
    /// frequencies it needs -- see `TextSource`. Filled in once, after the
    /// driver is chosen, by comparing term lists; `None` means this scorer
    /// must read its own frequencies.
    driven: Option<TextSource>,
}

impl PreparedText {
    /// Would the driver's per-term frequencies answer THIS query's terms?
    ///
    /// Same index, same match mode, same terms in the same order. Nothing is
    /// hashed or assumed: a query may search one term list and rank by
    /// another (`WHERE SEARCH(body,'comet') ORDER BY BM25(body,'harbour
    /// comet')`), and taking the driver's frequencies for the wrong term list
    /// would silently score the wrong documents.
    fn same_terms(&self, other: &Self) -> bool {
        self.info.id == other.info.id && self.matching == other.matching && self.terms == other.terms
    }
}

/// Which compiled text site produced a candidate, so a scorer can tell whether
/// the frequencies riding on that candidate are the ones IT asked for.
///
/// A text query is compiled at most twice in one request -- once per `WHERE`
/// text filter, once for a BM25 order -- and the driver is exactly one of
/// those sites. The candidate carries the site, the scorer knows its own, and
/// `PreparedText::driven` records (once, at prepare time) which site's
/// frequencies it may consume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextSource {
    Filter(usize),
    Order,
}

/// How many query terms' frequencies ride on a candidate without allocating.
///
/// A text query may have up to 64 distinct terms; carrying 64 slots on every
/// candidate would cost more than the reads they save. Past this many the
/// driver stamps nothing and the scorer reads its own frequencies, which is
/// the behaviour every driver had before.
const INLINE_TEXT_TERMS: usize = 8;

/// The most distinct terms one text query may have, enforced by
/// `prepare_text`. Scoring sizes its per-document buffers from this so it can
/// keep them on the stack.
const MAX_TEXT_TERMS: usize = 64;

/// The two buffers the text scorer fills per document: one frequency and one
/// inverse document frequency per term that actually matched.
///
/// They were stack arrays sized to the 64-term bound -- 256 bytes plus 512
/// bytes, zeroed on every scored document, for a query that usually has one
/// term. A page builds this once and every document reuses it: `clear` frees
/// nothing and `push` allocates only until the first document has settled the
/// capacity.
#[derive(Default)]
struct TextRowScratch {
    frequencies: Vec<u32>,
    idfs: Vec<f64>,
}

/// Everything one page reuses across its candidates, in one place.
///
/// Each of these is a buffer whose contents belong to the row being looked at
/// and whose CAPACITY belongs to the page. Passing them as one value is what
/// keeps `filters_match` and `rank_candidate` from growing a parameter every
/// time another per-row allocation is found.
#[derive(Default)]
struct RowScratch {
    /// One decoded norm block, reused by every candidate that lands in it.
    norms: super::text_indexes::TextScratch,
    /// The per-term frequencies and idfs of the document being scored.
    text: TextRowScratch,
    /// The encoded scalar key of ONE row value.
    ///
    /// A non-driving scalar predicate encodes the row's value to compare it
    /// against its bound. Through `scalar_key::encode` that was two
    /// allocations per candidate for nine bytes that are read once and
    /// dropped.
    scalar: Vec<u8>,
    /// The phrase scanner's current token.
    token: String,
    /// How often each of the query's terms was seen in the document the phrase
    /// scanner is on. Positionally by prepared term.
    seen: Vec<u32>,
}

/// The per-term frequencies the text merge cursor decoded on its way past this
/// document.
///
/// The merge already stands on the `(document, frequency)` entry it is
/// emitting. Keeping it costs one `u32` per query term; throwing it away costs
/// a re-seek and a whole-segment re-decode per term per document, which is the
/// O(df^2) that this type removes.
#[derive(Clone, Copy, Debug)]
struct TextFrequencies {
    source: TextSource,
    len: u8,
    /// Frequency per query term, positionally. `0` means the term is absent
    /// from this document, which the posting format never stores.
    slots: [u32; INLINE_TEXT_TERMS],
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
    /// Rank by the driver's own walk key. Which key that is cannot be known
    /// while the order is being compiled -- the driver is chosen after -- so
    /// `prepare_query` fills it in once the plan is settled.
    Driver(DriverKey),
    Distance {
        info: IndexInfo,
        center: Point,
    },
}

/// The key ONE driver's own walk is sorted by, resolved at prepare time so
/// the ranking never has to ask the plan again, per row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DriverKey {
    /// The entity id and nothing else. The primary cursor walks it, the text
    /// merge ascends by document, and a graph result is sorted before it
    /// leaves the traversal.
    Entity,
    /// `value || sequence` of one scalar index's postings.
    Scalar(IndexId),
    /// `cell || sequence` of one spatial index's postings.
    Cell(IndexId),
    /// `(level, cell, sequence)` of one geometry index's first admitted posting.
    GeomCell(IndexId),
    /// The external key bytes of one mapping entry.
    Key,
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
    /// Outward-ring walk yielding postings in ascending geodesic distance.
    /// `radius_cap` is a same-index, same-centre radius filter this walk
    /// certifies; `certifies` is that filter's position.
    Nearest {
        info: IndexInfo,
        center: Point,
        radius_cap: Option<f64>,
        certifies: Option<usize>,
    },
    /// A geometry index walk. The posting `BoxF` admits a candidate; the
    /// predicate is refined against the row (T3's no-row rule does not apply).
    Geometry {
        info: IndexInfo,
        /// Held so a future cursor-side refine can read it without going
        /// back to `CompiledFilter`. Admission uses `query_bbox`; the
        /// predicate itself is refined from the row.
        #[allow(dead_code)]
        predicate: GeometryFilter,
        position: usize,
        ranges: Vec<GeomRange>,
        query_bbox: BoxF,
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
    /// Walk the external-key mapping keyspace. `predicate` is a range over
    /// the raw key bytes (`Empty`/`Range` only -- see `CompiledFilter::Key`);
    /// `position` is the filter position it certifies, or `None` when the
    /// request named no `QueryFilter::Key` at all and the whole collection's
    /// keys are walked.
    Keys {
        predicate: EncodedScalarFilter,
        position: Option<usize>,
    },
}

/// One Hilbert range of a geometry cover, at one ladder level. Walked in
/// on-disk key order: `LEVEL_WORLD` (0), then `LEVEL_COARSE` (8), then
/// `LEVEL_FINE` (12), because the posting key is `tag||index||level||cell||seq`.
#[derive(Clone, Debug)]
struct GeomRange {
    level: u8,
    lo: u64,
    hi: u64,
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
            Self::Nearest { info, .. } => QueryDriver::Nearest { index: info.id },
            Self::Geometry {
                info,
                fallback_world,
                ..
            } => QueryDriver::Geometry {
                index: info.id,
                fallback_world: *fallback_world,
            },
            Self::Text { prepared, .. } => QueryDriver::Text(prepared.info.id),
            Self::ExactVector { info } => QueryDriver::ExactVector(info.id),
            Self::QuantizedVector { info } => QueryDriver::QuantizedVector(info.id),
            Self::Keys { .. } => QueryDriver::Keys,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RankValue {
    Entity,
    Scalar(Vec<u8>),
    Score(u64),
    /// One spatial posting's Hilbert cell. Four bytes in the key, and the
    /// value they hold rather than the bytes: a spatial answer can be
    /// millions of rows, and a `Vec` per row to carry a `u32` is an
    /// allocation per row.
    Cell(u32),
    /// One geometry posting's `(level, cell)` — the first posting that
    /// admitted the entity. Level precedes cell, matching the on-disk key.
    GeomCell { level: u8, cell: u32 },
    /// One mapping entry's external-key bytes.
    Key(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RankKey {
    value: RankValue,
    id: EntityId,
}

/// How much of the query's rank order the driver's own walk already provides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RankWalk {
    /// None of it: the walk order and the rank order are unrelated.
    No,
    /// All of it, key for key. A page may stop the moment its heap is full,
    /// and the next page may resume at this page's last key.
    Exact,
    /// The VALUE half only, and monotonically. A descending scalar order is
    /// ranked (value descending, entity id ASCENDING) while the reverse walk
    /// hands a tie group over id descending, so a full heap is not yet an
    /// answer: the walk has to finish the boundary value's tie group before
    /// anything later can be ruled out. It still stops long before the end,
    /// and it still resumes rather than restarting.
    ByValue,
}

/// One ranked candidate the page is still holding, and -- when the page is
/// going to project fields out of it -- the primary row the walk already read.
///
/// Carrying the bytes is what stops a projected scan reading every row twice:
/// once to walk it and once to fetch the winner back. Bounded by the heap's
/// capacity, so it is the page's own bound, not the collection's.
struct HeapEntry {
    key: RankKey,
    descending: bool,
    /// Boxed on purpose. Every query pays this struct's size on every heap
    /// push, pop and sift -- a key-only scan of 20,000 rows sifts an 8,192
    /// entry heap -- so the pointer stays here and the row lives off to one
    /// side. Inlining `RowData` here cost a measured 25-30% on `scan/full_keys`
    /// and `filter/eq_indexed_many`, which carry no rows at all.
    row: Option<Box<RowData>>,
}

/// The carried row plays no part in the ordering, so equality is the rank
/// comparison and nothing else.
impl Eq for HeapEntry {}

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

/// The candidates one page is still holding.
///
/// A page needs a HEAP only once it is full: until then every candidate is
/// kept, so the ordering the heap maintains on the way in is thrown away by
/// the final sort. Pushing 8,192 entries into a max-heap in ASCENDING order --
/// which is exactly what a key-only scan does -- is the heap's worst case, one
/// full sift to the root per row; measured at 61.9 ns/row against 3.6 ns/row
/// for pushing the same entries onto a `Vec`.
///
/// So the page fills a `Vec`, and becomes a heap in one `O(n)` heapify the
/// first time it has to name its WORST held entry -- which can only happen
/// once it is full and a candidate has to displace something. A page whose
/// answer fits (every non-ranked case here, and every ranked one under 8,192
/// hits) never heapifies at all.
///
/// The `Vec` also reserves nothing until its first entry. `capacity` is the
/// page size, not the answer size, so an EMPTY answer used to allocate 8,193
/// entries -- 459 KB -- and drop them untouched, which was most of what
/// `filter/eq_no_match` cost.
enum Winners {
    Filling(Vec<HeapEntry>),
    Full(BinaryHeap<HeapEntry>),
}

impl Winners {
    fn new() -> Self {
        Self::Filling(Vec::new())
    }

    fn len(&self) -> usize {
        match self {
            Self::Filling(kept) => kept.len(),
            Self::Full(heap) => heap.len(),
        }
    }

    /// Keep one more. Only ever called with fewer than `capacity` held, or
    /// straight after [`Winners::pop_worst`].
    #[inline]
    fn push(&mut self, capacity: usize, entry: HeapEntry) {
        match self {
            Self::Filling(kept) => {
                if kept.capacity() == 0 {
                    kept.reserve_exact(capacity);
                }
                kept.push(entry);
            }
            Self::Full(heap) => heap.push(entry),
        }
    }

    /// The worst entry held, which is the one a better candidate displaces.
    /// Asking the question is what turns the page into a heap.
    fn worst(&mut self) -> Option<&HeapEntry> {
        if let Self::Filling(kept) = self {
            *self = Self::Full(BinaryHeap::from(std::mem::take(kept)));
        }
        match self {
            Self::Full(heap) => heap.peek(),
            Self::Filling(_) => unreachable!("the page was just heapified"),
        }
    }

    fn pop_worst(&mut self) {
        if let Self::Full(heap) = self {
            heap.pop();
        }
    }

    /// True once the page has had to order itself, so its entries are in heap
    /// order and not in the order the walk handed them over.
    fn heaped(&self) -> bool {
        matches!(self, Self::Full(_))
    }

    fn into_vec(self) -> Vec<HeapEntry> {
        match self {
            Self::Filling(kept) => kept,
            Self::Full(heap) => heap.into_vec(),
        }
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

#[inline(always)]
fn compare_rank(a: &RankKey, b: &RankKey, descending: bool) -> Ordering {
    compare_rank_value(&a.value, &b.value, descending).then_with(|| a.id.cmp(&b.id))
}

/// The VALUE half of the rank comparison, without the entity-id tie-break.
/// A descending walk is monotone in this and not in the whole key, so this is
/// what decides whether anything still ahead of the cursor can outrank what
/// the page already holds.
#[inline(always)]
fn compare_rank_value(a: &RankValue, b: &RankValue, descending: bool) -> Ordering {
    match (a, b) {
        (RankValue::Entity, RankValue::Entity) => Ordering::Equal,
        (RankValue::Scalar(a), RankValue::Scalar(b)) => {
            if descending {
                b.cmp(a)
            } else {
                a.cmp(b)
            }
        }
        (RankValue::Cell(a), RankValue::Cell(b)) => {
            if descending {
                b.cmp(a)
            } else {
                a.cmp(b)
            }
        }
        (
            RankValue::GeomCell {
                level: la,
                cell: ca,
            },
            RankValue::GeomCell {
                level: lb,
                cell: cb,
            },
        ) => {
            let a = (*la, *ca);
            let b = (*lb, *cb);
            if descending {
                b.cmp(&a)
            } else {
                a.cmp(&b)
            }
        }
        (RankValue::Key(a), RankValue::Key(b)) => {
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
    }
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
    /// Rows this query has already ranked but has not handed out yet, in rank
    /// order, LAST FIRST so `pop` takes the next one.
    ///
    /// A driver whose walk order is unrelated to the ranking -- a spatial cell
    /// walk under an entity-id ranking -- cannot stop early and cannot resume:
    /// to know which rows come next it has to see every candidate again. (A
    /// RANGE over a value-ordered index was in this list until the query
    /// asked for it in that index's own order, a SPATIAL walk until
    /// `QueryOrder::Driver` let a query ask for cell order, and a text merge
    /// until it was recognised as already ascending by document; all three
    /// resume now.) So page k+1 re-opened the whole candidate stream and
    /// discarded everything page 1..k had already returned. That is one full
    /// pass PER PAGE, and an answer of R rows in pages of P costs R^2/P --
    /// invisible while the answer fits one page, and the whole of why
    /// `popsim`'s `born_decade` took 1,011 s to return 5.58M rows at 48M
    /// while the same case at 200K took 11 ms.
    ///
    /// A page under such a driver walks to the end of the stream whatever it
    /// does, so the rows past this page are rows it has ALREADY ranked. It
    /// keeps them here instead of dropping them, and the pages after it are
    /// served from here without walking at all.
    run: Vec<HeapEntry>,
    /// Whether the walk that filled `run` was cut off by [`RUN_ROWS`]. When it
    /// was, emptying the run is not the end of the answer and the next page
    /// walks again, from the last row handed out.
    run_bounded: bool,
    /// One [`ScalarRangeSet`] per filter position, built at most once and
    /// reused by every page and by resume -- see `ensure_scalar_range_sets`.
    scalar_ranges: Vec<ScalarRangeSet>,
    /// The resumable nearest walk, when the driver is [`DriverPlan::Nearest`].
    /// Kept on the prepared query so page N+1 continues the ring the previous
    /// page stopped in rather than re-walking from the centre: a `PreparedQuery`
    /// already owns per-query state (C1's sets), and the walk's `held`/`ready`
    /// buffers and Hilbert cover are the same kind of thing. Re-walking rings
    /// up to the previous page's last `(distance, id)` would be correct and
    /// bounded by that ring, but it would re-examine every posting already
    /// charged on earlier pages.
    nearest: Option<super::spatial_indexes::NearestWalk>,
    /// Sequences a geometry driver has already admitted, carried across
    /// Driver-order pages so a later posting of an already-emitted entity
    /// (at most 8 cells) is not re-yielded after a resume. Cleared each page
    /// under EntityId order, which re-walks from the start.
    geometry_seen: HashSet<u64>,
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
    let info = db.index_info_cached(id)?;
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
    let info = db.index_info_cached(id)?;
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
    let weights = super::text_indexes::bm25_weights(corpus)?;
    let mut idfs = Vec::with_capacity(terms.len());
    for &df in &dfs {
        idfs.push(if df == 0 {
            // No posting carries this term, so nothing will ever be scored
            // against it; a document that claims one is refused by
            // `bm25_scored`'s own bounds and never reaches this weight.
            0.0
        } else {
            super::text_indexes::bm25_idf(corpus, df)?
        });
    }
    Ok(PreparedText {
        info,
        terms,
        phrase_prefix: phrase
            .as_deref()
            .map(crate::text_analyzer::phrase_prefix)
            .unwrap_or_default(),
        phrase,
        matching,
        dfs,
        weights,
        idfs,
        driven: None,
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

/// Degree margin matching `radius_candidate_bounds`'s private `OUTWARD_DEGREES`.
const GEOM_OUTWARD_DEGREES: f64 = 1e-10;

fn geometry_query_geom(predicate: &GeometryFilter) -> &Geom {
    match predicate {
        GeometryFilter::Intersects(g)
        | GeometryFilter::Within(g)
        | GeometryFilter::Contains(g) => g,
        GeometryFilter::DWithin { geometry, .. } => geometry,
    }
}

fn geometry_query_bbox(predicate: &GeometryFilter) -> QueryResult<(f64, f64, f64, f64)> {
    let geom = geometry_query_geom(predicate);
    // The same rule the index files geometries under: a query geometry that
    // crosses the antimeridian asks about the whole longitude range.
    let bbox = super::spatial_geometry_indexes::indexed_bbox(geom)
        .ok_or_else(|| invalid_query("geometry filter requires a non-empty geometry"))?;
    match predicate {
        GeometryFilter::DWithin { metres, .. } => {
            if !metres.is_finite() || *metres < 0.0 {
                return Err(invalid_query(
                    "geometry dwithin metres must be finite and non-negative",
                ));
            }
            Ok(expand_bbox_by_metres(bbox, *metres))
        }
        _ => Ok(bbox),
    }
}

/// Expand a query bbox by a geodesic radius so every stored geometry within
/// `metres` of any point in the original box has a bbox that overlaps the
/// expansion. Same conservative envelope as [`radius_candidate_bounds`]:
///
/// - `angular = metres / WGS84_MIN_CURVATURE_RADIUS_METRES` (the smallest
///   WGS84 curvature radius, so the angle is an over-estimate);
/// - latitude expands by `angular.to_degrees() + 1e-10` on both sides;
/// - longitude expands by `asin(sin(angular) / cos(φ)) + 1e-10` at the
///   **poleward** latitude of the expanded band (smallest `cos(φ)` → most
///   degrees). If that band reaches a pole, longitude is the whole world.
///
/// Never misses; may over-cover. A dateline wrap becomes a world longitude.
fn expand_bbox_by_metres(
    (xmin, xmax, ymin, ymax): (f64, f64, f64, f64),
    metres: f64,
) -> (f64, f64, f64, f64) {
    let angular = metres / WGS84_MIN_CURVATURE_RADIUS_METRES;
    if angular >= std::f64::consts::PI {
        return (-180.0, 180.0, -90.0, 90.0);
    }
    let lat_delta = angular.to_degrees() + GEOM_OUTWARD_DEGREES;
    let south = (ymin - lat_delta).max(-90.0);
    let north = (ymax + lat_delta).min(90.0);
    let poleward = south.abs().max(north.abs()).to_radians();
    if poleward + angular >= std::f64::consts::FRAC_PI_2 {
        return (-180.0, 180.0, south, north);
    }
    let ratio = (angular.sin() / poleward.cos()).clamp(-1.0, 1.0);
    let lon_delta = ratio.asin().to_degrees() + GEOM_OUTWARD_DEGREES;
    let west = xmin - lon_delta;
    let east = xmax + lon_delta;
    if west < -180.0 || east > 180.0 || west > east {
        (-180.0, 180.0, south, north)
    } else {
        (west, east, south, north)
    }
}

/// Cover ranges at one ladder level, budgeted like the point driver
/// (`MAX_HILBERT_RANGES` = 64). A cover that will not fit becomes the whole
/// level (never a miss).
fn geometry_level_ranges(
    xmin: f64,
    xmax: f64,
    ymin: f64,
    ymax: f64,
    bits: u8,
) -> (Vec<(u64, u64)>, bool) {
    let max_h = if bits == 0 {
        0
    } else {
        (1u64 << (2 * bits)) - 1
    };
    if bits == 0 {
        return (vec![(0, 0)], false);
    }
    let mut ranges = cover_ranges(xmin, xmax, ymin, ymax, bits, MAX_HILBERT_RANGES);
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (lo, hi) in ranges {
        if lo > hi || hi > max_h {
            return (vec![(0, max_h)], true);
        }
        match merged.last_mut() {
            Some(last) if lo <= last.1.saturating_add(1) => last.1 = last.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    if merged.is_empty() || merged.len() > MAX_HILBERT_RANGES {
        (vec![(0, max_h)], true)
    } else {
        (merged, false)
    }
}

fn geometry_ranges(predicate: &GeometryFilter) -> QueryResult<(Vec<GeomRange>, BoxF, bool)> {
    let (xmin, xmax, ymin, ymax) = geometry_query_bbox(predicate)?;
    let query_bbox = BoxF::from_f64(xmin, xmax, ymin, ymax);
    let mut out = Vec::new();
    let mut fallback_world = false;
    // On-disk key order: level byte first, so WORLD (0), COARSE (8), FINE (12).
    for level in [spatial::LEVEL_WORLD, spatial::LEVEL_COARSE, spatial::LEVEL_FINE] {
        let (ranges, world) = geometry_level_ranges(xmin, xmax, ymin, ymax, level);
        fallback_world |= world;
        for (lo, hi) in ranges {
            out.push(GeomRange {
                level,
                lo,
                hi,
            });
        }
    }
    Ok((out, query_bbox, fallback_world))
}

fn geometry_predicate_matches(predicate: &GeometryFilter, row: &Geom) -> bool {
    match predicate {
        GeometryFilter::Intersects(query) => spatial_geometry::intersects(row, query),
        GeometryFilter::Within(query) => spatial_geometry::within(row, query),
        GeometryFilter::Contains(query) => spatial_geometry::contains(row, query),
        GeometryFilter::DWithin {
            geometry: query,
            metres,
        } => spatial_geometry::dwithin_m(row, query, *metres),
    }
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

/// A key bound as its raw UTF-8 bytes -- `mapping_key` appends a key's bytes
/// directly with no order-preserving transform (`collections.rs:382`), so
/// byte order already is the bound's order and there is no `Kind` to encode
/// against, unlike a scalar value.
fn encode_key_bound(bound: &Bound<&str>) -> EncodedBound {
    match bound {
        Bound::Included(value) => EncodedBound::Included(value.as_bytes().to_vec()),
        Bound::Excluded(value) => EncodedBound::Excluded(value.as_bytes().to_vec()),
        Bound::Unbounded => EncodedBound::Unbounded,
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
        ScalarFilter::Range { lower, upper } => Ok(range_or_empty(
            encode_bound(kind, lower)?,
            encode_bound(kind, upper)?,
        )),
        ScalarFilter::IsNull => Ok(EncodedScalarFilter::IsNull),
        ScalarFilter::IsMissing => Ok(EncodedScalarFilter::IsMissing),
    }
}

/// A pair of bounds as a predicate: `Range` when a value can sit between them,
/// `Empty` when nothing can. Crossed bounds, and bounds that meet on a value
/// at least one side excludes, admit nothing.
fn range_or_empty(lower: EncodedBound, upper: EncodedBound) -> EncodedScalarFilter {
    let empty = match (bound_bytes(&lower), bound_bytes(&upper)) {
        (Some(lower_value), Some(upper_value)) if lower_value > upper_value => true,
        (Some(lower_value), Some(upper_value)) if lower_value == upper_value => {
            matches!(lower, EncodedBound::Excluded(_)) || matches!(upper, EncodedBound::Excluded(_))
        }
        _ => false,
    };
    if empty {
        EncodedScalarFilter::Empty
    } else {
        EncodedScalarFilter::Range { lower, upper }
    }
}

/// The tighter of two LOWER bounds. Unbounded is the loosest; on the same
/// value the excluding bound is the tighter one.
fn tighter_lower(left: EncodedBound, right: EncodedBound) -> EncodedBound {
    match (bound_bytes(&left), bound_bytes(&right)) {
        (None, _) => right,
        (_, None) => left,
        (Some(a), Some(b)) => match a.cmp(b) {
            Ordering::Greater => left,
            Ordering::Less => right,
            Ordering::Equal => {
                if matches!(left, EncodedBound::Excluded(_)) {
                    left
                } else {
                    right
                }
            }
        },
    }
}

/// The tighter of two UPPER bounds, by the same rule mirrored.
fn tighter_upper(left: EncodedBound, right: EncodedBound) -> EncodedBound {
    match (bound_bytes(&left), bound_bytes(&right)) {
        (None, _) => right,
        (_, None) => left,
        (Some(a), Some(b)) => match a.cmp(b) {
            Ordering::Less => left,
            Ordering::Greater => right,
            Ordering::Equal => {
                if matches!(left, EncodedBound::Excluded(_)) {
                    left
                } else {
                    right
                }
            }
        },
    }
}

/// Two predicates on ONE scalar index, as the single predicate they mean.
///
/// The whole point is that an index answers a conjunction over itself without
/// help: a posting key that survives the folded predicate has satisfied both,
/// so nothing has to open the row to ask the second question. The rules, in
/// full, with `Empty` absorbing everything:
///
/// * `Range ∩ Range` -- the tighter bound on each side. Bounds that cross, or
///   meet on a value one side excludes, give `Empty`: the answer is empty and
///   the tree is never touched.
/// * `Eq ∩ Range` -- `Eq` when the value lies inside the range, otherwise
///   `Empty`. `Eq` is a subset of any range that contains it.
/// * `Eq ∩ Eq` -- the same `Eq` when the encodings agree, otherwise `Empty`.
/// * `IsNull ∩ IsNull`, `IsMissing ∩ IsMissing` -- unchanged. Mixed, `Empty`:
///   a field is not both absent and present-and-null.
/// * `IsNull` or `IsMissing` against `Eq` or `Range` -- `Empty`. Null and
///   missing share the nullish posting key, which sorts below every real
///   value, and `scalar_filter_matches` refuses both for `Eq` and for `Range`.
///   So no row is both nullish and inside a value predicate, and folding to
///   `Empty` says exactly what the per-row evaluation already said.
fn fold_scalar_predicates(
    left: &EncodedScalarFilter,
    right: &EncodedScalarFilter,
) -> EncodedScalarFilter {
    use EncodedScalarFilter as F;
    match (left, right) {
        (F::Empty, _) | (_, F::Empty) => F::Empty,
        (F::IsNull, F::IsNull) => F::IsNull,
        (F::IsMissing, F::IsMissing) => F::IsMissing,
        (F::IsNull | F::IsMissing, _) | (_, F::IsNull | F::IsMissing) => F::Empty,
        (F::Eq(a), F::Eq(b)) => {
            if a == b {
                F::Eq(a.clone())
            } else {
                F::Empty
            }
        }
        (F::Eq(value), range @ F::Range { .. }) | (range @ F::Range { .. }, F::Eq(value)) => {
            if scalar_key_position(range, value) == Ordering::Equal {
                F::Eq(value.clone())
            } else {
                F::Empty
            }
        }
        (
            F::Range {
                lower: left_lower,
                upper: left_upper,
            },
            F::Range {
                lower: right_lower,
                upper: right_upper,
            },
        ) => range_or_empty(
            tighter_lower(left_lower.clone(), right_lower.clone()),
            tighter_upper(left_upper.clone(), right_upper.clone()),
        ),
    }
}

/// Fold every group of scalar filters that name the SAME index into one
/// predicate, held by one surviving position.
///
/// Which position survives is user-visible, so the rule is fixed and narrow:
///
/// * if the request names one of the group as its candidate driver
///   (`CandidateDriver::Filter(position)`), THAT position keeps the folded
///   predicate, so an explicitly chosen driver still drives;
/// * otherwise the LOWEST position in the group keeps it.
///
/// Every other position in the group becomes [`CompiledFilter::Folded`],
/// which matches every candidate and reads nothing. The positions themselves
/// never move: naming a folded position as the driver drives the survivor,
/// and a candidate the survivor certifies still reports the survivor's
/// position in `satisfied_filter`.
fn fold_same_index_scalars(filters: &mut [CompiledFilter], driver: CandidateDriver) {
    let named = match driver {
        CandidateDriver::Filter(position) => Some(position),
        _ => None,
    };
    let mut groups: Vec<(IndexId, Vec<usize>)> = Vec::new();
    for (position, filter) in filters.iter().enumerate() {
        if let CompiledFilter::Scalar { info, .. } = filter {
            match groups.iter_mut().find(|(id, _)| *id == info.id) {
                Some((_, members)) => members.push(position),
                None => groups.push((info.id, vec![position])),
            }
        }
    }
    for (_, members) in groups {
        if members.len() < 2 {
            continue;
        }
        let keeper = named
            .filter(|position| members.contains(position))
            .unwrap_or(members[0]);
        let mut folded = match &filters[members[0]] {
            CompiledFilter::Scalar { predicate, .. } => predicate.clone(),
            _ => continue,
        };
        for position in members.iter().skip(1) {
            if let CompiledFilter::Scalar { predicate, .. } = &filters[*position] {
                folded = fold_scalar_predicates(&folded, predicate);
            }
        }
        for position in &members {
            if *position == keeper {
                if let CompiledFilter::Scalar { predicate, .. } = &mut filters[*position] {
                    *predicate = folded.clone();
                }
            } else {
                filters[*position] = CompiledFilter::Folded { into: keeper };
            }
        }
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
                QueryFilter::Geometry { index, predicate } => {
                    let info = self.index_info_cached(*index)?;
                    if info.collection != request.collection {
                        return Err(invalid_query("query index belongs to another collection"));
                    }
                    if info.family == IndexFamily::SpatialPoint {
                        return Err(invalid_query(
                            "a Geometry filter requires a spatial geometry index, not a point index",
                        ));
                    }
                    if info.family != IndexFamily::SpatialGeometry {
                        return Err(invalid_query(
                            "a Geometry filter requires a spatial geometry index",
                        ));
                    }
                    if info.state != IndexState::Ready {
                        return Err(invalid_query("query index is not ready"));
                    }
                    super::spatial_geometry_indexes::descriptor(&info)?;
                    // Touch the bbox now so a DWithin with a bad radius, or
                    // an empty query geometry, is refused at prepare.
                    let _ = geometry_query_bbox(predicate)?;
                    CompiledFilter::Geometry {
                        info,
                        predicate: predicate.clone(),
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
                QueryFilter::Key { lower, upper } => CompiledFilter::Key {
                    predicate: range_or_empty(encode_key_bound(lower), encode_key_bound(upper)),
                },
            });
        }
        if filters
            .iter()
            .filter(|filter| matches!(filter, CompiledFilter::Key { .. }))
            .count()
            > 1
        {
            return Err(invalid_query("query has more than one key filter"));
        }

        // Two predicates on ONE scalar index are one predicate. Folding them
        // here, once, is the difference between an index that answers the
        // conjunction from its own postings and one that answers half of it
        // and opens the row for the rest -- a primary read per candidate for
        // a question the posting key had already settled.
        fold_same_index_scalars(&mut filters, request.driver);

        let mut order = match request.order {
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
            // The key is the DRIVER's, and the driver is chosen below. This
            // stands in until it is; `driver_key` replaces it.
            QueryOrder::Driver => CompiledOrder::Driver(DriverKey::Entity),
            QueryOrder::Distance {
                index,
                center,
                direction,
            } => {
                if matches!(direction, SortDirection::Descending) {
                    return Err(invalid_query("distance order is ascending only"));
                }
                let info = require_family_index(
                    self,
                    request.collection,
                    index,
                    IndexFamily::SpatialPoint,
                    "spatial-point",
                )?;
                super::spatial_indexes::descriptor(&info)?;
                CompiledOrder::Distance {
                    info,
                    center,
                }
            }
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
            // A folded position names the survivor that holds its predicate,
            // so naming either one as the driver drives the same walk.
            let position = match filters.get(position) {
                Some(CompiledFilter::Folded { into }) => *into,
                _ => position,
            };
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
                Some(CompiledFilter::Geometry { info, predicate }) => {
                    let (ranges, query_bbox, fallback_world) = geometry_ranges(predicate)?;
                    Ok(DriverPlan::Geometry {
                        info: info.clone(),
                        predicate: predicate.clone(),
                        position,
                        ranges,
                        query_bbox,
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
                // Driver order names no index, so there is no order index to
                // drive: the entity cursor is the candidate stream, and its
                // own order is the answer's. This is also what a driver-
                // ordered query with no filter at all lands on.
                CompiledOrder::Driver(_) => Ok(DriverPlan::Entities),
                CompiledOrder::Distance { info, center } => {
                    Ok(nearest_plan(info, *center, &filters))
                }
            }
        };
        // `CandidateDriver::Keys` is not `CandidateDriver::Filter(position)`
        // because its filter is OPTIONAL: with none, the whole collection's
        // keys are walked (what `count_all` needs); with one, that position
        // is the predicate and is certified the same way a driving scalar
        // range is.
        let keys_driver = |filters: &[CompiledFilter]| -> QueryResult<DriverPlan> {
            let found = filters.iter().enumerate().find_map(|(position, filter)| {
                match filter {
                    CompiledFilter::Key { predicate } => Some((position, predicate.clone())),
                    _ => None,
                }
            });
            Ok(match found {
                Some((position, predicate)) => DriverPlan::Keys {
                    predicate,
                    position: Some(position),
                },
                None => DriverPlan::Keys {
                    predicate: EncodedScalarFilter::Range {
                        lower: EncodedBound::Unbounded,
                        upper: EncodedBound::Unbounded,
                    },
                    position: None,
                },
            })
        };
        let driver = match request.driver {
            CandidateDriver::Entities => DriverPlan::Entities,
            CandidateDriver::Filter(position) => filter_driver(position)?,
            CandidateDriver::Order => order_driver()?,
            CandidateDriver::Keys => keys_driver(&filters)?,
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
                    // An equality filter is the default driver -- unless the
                    // query also names a scalar order on ANOTHER ready index
                    // and the filter is broad enough that walking that index
                    // in rank order is the cheaper plan. See
                    // `order_index_drives_better`.
                    let ordered_elsewhere = match (&order, filters.get(position)) {
                        (
                            CompiledOrder::Scalar {
                                info: ranked,
                                direction,
                            },
                            Some(CompiledFilter::Scalar {
                                info,
                                predicate: EncodedScalarFilter::Eq(value),
                                ..
                            }),
                        ) if ranked.id != info.id && request.total_limit.is_some() => {
                            order_index_drives_better(
                                self,
                                ranked,
                                matches!(direction, SortDirection::Descending),
                                info,
                                value,
                            )?
                        }
                        _ => false,
                    };
                    if ordered_elsewhere {
                        order_driver()?
                    } else {
                        filter_driver(position)?
                    }
                } else if let Some(position) = filters
                    .iter()
                    .position(|filter| matches!(filter, CompiledFilter::Text(_)))
                {
                    filter_driver(position)?
                } else if let CompiledOrder::Distance { info, center } = &order {
                    if distance_can_drive(info.id, *center, &filters) {
                        nearest_plan(info, *center, &filters)
                    } else if let Some(position) = filters.iter().position(|filter| {
                        matches!(filter, CompiledFilter::Point { predicate, .. } if point_ranges(*predicate).is_ok_and(|(_, world)| !world))
                    }) {
                        filter_driver(position)?
                    } else if let Some(position) = filters
                        .iter()
                        .position(|filter| matches!(filter, CompiledFilter::Point { .. }))
                    {
                        filter_driver(position)?
                    } else {
                        nearest_plan(info, *center, &filters)
                    }
                } else if let Some(position) = filters.iter().position(|filter| {
                    matches!(filter, CompiledFilter::Point { predicate, .. } if point_ranges(*predicate).is_ok_and(|(_, world)| !world))
                }) {
                    filter_driver(position)?
                } else if let Some(position) = filters.iter().position(|filter| {
                    matches!(filter, CompiledFilter::Geometry { predicate, .. } if geometry_ranges(predicate).is_ok_and(|(_, _, world)| !world))
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
                } else if let Some(position) = filters
                    .iter()
                    .position(|filter| matches!(filter, CompiledFilter::Geometry { .. }))
                {
                    filter_driver(position)?
                } else {
                    order_driver()?
                }
            }
        };

        // A `QueryFilter::Key` is certified by the mapping-entry walk that
        // produced the candidate (`CompiledFilter::Key`'s doc); it has no row-
        // level fallback the way a scalar predicate does. So it is only
        // meaningful at the position `CandidateDriver::Keys` actually
        // certifies -- named by a DIFFERENT driver, or left uncertified
        // because the request chose `CandidateDriver::Keys` but a second key
        // filter lost the (at most one) slot, it would reach `filters_match`
        // with no way to answer itself.
        if let Some(key_position) = filters
            .iter()
            .position(|filter| matches!(filter, CompiledFilter::Key { .. }))
        {
            let certified = matches!(
                &driver,
                DriverPlan::Keys { position: Some(position), .. } if *position == key_position
            );
            if !certified {
                return Err(invalid_query(
                    "a key filter requires CandidateDriver::Keys to drive it",
                ));
            }
        }

        // Driver order ranks by the key the chosen driver's own walk is
        // sorted by, which is knowable only now.
        if matches!(order, CompiledOrder::Driver(_)) {
            order = CompiledOrder::Driver(driver_key(&driver)?);
        }

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

        // A non-driving RANGE filter's candidate's value is unknown until
        // something reads it -- unlike an equality filter, its posting alone
        // cannot answer one candidate at a time. `ensure_scalar_range_sets`
        // walks it once instead, on first use, and every position marked here
        // is what that walk fills in.
        let mut scalar_ranges = filters.iter().map(|_| ScalarRangeSet::Ineligible).collect::<Vec<_>>();
        for (position, filter) in filters.iter().enumerate() {
            if let CompiledFilter::Scalar { predicate, .. } = filter {
                if matches!(predicate, EncodedScalarFilter::Range { .. })
                    && scalar_driver_position != Some(position)
                {
                    scalar_ranges[position] = ScalarRangeSet::Unbuilt;
                }
            }
        }

        // Which scorers may read the frequencies the text driver decodes.
        //
        // Decided once, here, by comparing term lists -- not per candidate,
        // and never by a hash. A text driver produces the frequencies of ITS
        // terms; a scorer over a different term list on the same index must go
        // on reading its own, or it would score the wrong words.
        if let DriverPlan::Text {
            prepared: driving,
            position,
        } = &driver
        {
            let source = match position {
                Some(position) => TextSource::Filter(*position),
                None => TextSource::Order,
            };
            let carries = driving.terms.len() <= INLINE_TEXT_TERMS;
            for filter in filters.iter_mut() {
                if let CompiledFilter::Text(prepared) = filter {
                    prepared.driven = (carries && prepared.same_terms(driving)).then_some(source);
                }
            }
            if let CompiledOrder::Bm25(prepared) = &mut order {
                prepared.driven = (carries && prepared.same_terms(driving)).then_some(source);
            }
        }

        let nearest = match &driver {
            DriverPlan::Nearest {
                info,
                center,
                radius_cap,
                ..
            } => Some(super::spatial_indexes::NearestWalk::new(
                info.clone(),
                *center,
                request.total_limit.unwrap_or(0),
                *radius_cap,
                usize::MAX,
            )),
            _ => None,
        };

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
            run: Vec::new(),
            run_bounded: false,
            scalar_ranges,
            nearest,
            geometry_seen: HashSet::new(),
        })
    }
}

/// The key one driver's own walk is ordered by.
///
/// Every driver the planner can choose for a driver-ordered query has one,
/// and it is a key the walk is already standing on: the primary cursor is in
/// id order, a scalar posting range is `value || sequence`, the text merge
/// ascends by document and refuses a posting that does not advance, a
/// traversal hands its ids over sorted, and a spatial walk ascends by
/// `(cell, sequence)` across a sorted, merged range list.
///
/// A vector driver has none. Its entries are in locator order and its answer
/// is in distance order, and driver order asked for neither -- an approximate
/// shortlist is a RANKING wearing a walk's clothes, and returning it as
/// though its walk were the answer's order would be a page that cannot be
/// resumed and an order nobody asked for. It is unreachable as the planner
/// stands (no filter drives a vector index, and `order_driver` hands driver
/// order the entity cursor), so this is the rule written down where the other
/// cases are rather than left for a future driver to rediscover.
fn driver_key(driver: &DriverPlan) -> QueryResult<DriverKey> {
    match driver {
        DriverPlan::Entities | DriverPlan::Text { .. } | DriverPlan::Graph { .. } => {
            Ok(DriverKey::Entity)
        }
        DriverPlan::Scalar { info, .. } => Ok(DriverKey::Scalar(info.id)),
        DriverPlan::Spatial { info, .. } => Ok(DriverKey::Cell(info.id)),
        DriverPlan::Geometry { info, .. } => Ok(DriverKey::GeomCell(info.id)),
        DriverPlan::Keys { .. } => Ok(DriverKey::Key),
        DriverPlan::ExactVector { .. }
        | DriverPlan::QuantizedVector { .. }
        | DriverPlan::Nearest { .. } => Err(invalid_query(
            "driver order needs a driver that walks in an order of its own",
        )),
    }
}

/// True when `QueryOrder::Distance` on `order_index` can itself be the
/// candidate driver: every spatial filter is a same-index, same-centre
/// radius (the common "nearest k within R" shape). A bbox, or a radius
/// around a different point, is not monotone in distance from `center`, so
/// stopping the ordered walk at `k` would drop in-filter rows that sit
/// farther than k out-of-filter neighbours.
fn distance_can_drive(order_index: IndexId, center: Point, filters: &[CompiledFilter]) -> bool {
    filters.iter().all(|filter| match filter {
        CompiledFilter::Point { info, predicate } if info.id == order_index => {
            matches!(predicate, PointFilter::Radius { center: c, .. } if *c == center)
        }
        CompiledFilter::Point { .. } => false,
        _ => true,
    })
}

fn nearest_plan(info: &IndexInfo, center: Point, filters: &[CompiledFilter]) -> DriverPlan {
    let mut radius_cap = None;
    let mut certifies = None;
    for (position, filter) in filters.iter().enumerate() {
        if let CompiledFilter::Point {
            info: filter_info,
            predicate: PointFilter::Radius {
                center: filter_center,
                radius_metres,
            },
        } = filter
        {
            if filter_info.id == info.id && *filter_center == center {
                radius_cap = Some(*radius_metres);
                certifies = Some(position);
                break;
            }
        }
    }
    DriverPlan::Nearest {
        info: info.clone(),
        center,
        radius_cap,
        certifies,
    }
}

/// How many postings of the ORDER index the planner walks before deciding.
const ORDER_DRIVE_PROBE: u64 = 64;
/// The density the probe must find: one accepted row per this many walked.
/// It is the crossover of the two plans -- see [`order_index_drives_better`].
const ORDER_DRIVE_MIN_DENSITY: u64 = 16;

/// Should an equality filter plus a scalar order on a DIFFERENT index, under a
/// LIMIT, be driven from the ORDER index rather than from the filter?
///
/// Driving from the equality posting hands candidates over in entity-id order,
/// which is not the order asked for. Every match then has to give up its order
/// value, and that value is not in the posting being walked -- it is in the
/// primary row, one point-get and one dense-v3 walk per match -- and the whole
/// set has to be sorted before the first row can be returned.
///
/// Driving from the ORDER index inverts that. The walk is already in rank
/// order, so the LIMIT becomes a stop condition, the order key falls out of the
/// posting the cursor is standing on, and the equality filter is answered from
/// ITS posting as a membership probe -- no row at all, which is the same
/// authority `scalar_eq_posting_matches` already carries.
///
/// The price is walking over the rows the filter rejects, `1/density` of them
/// per accepted row, each costing a sequential posting step plus a membership
/// probe. Against the other plan's random primary point-get plus row walk per
/// match, the two meet around one accepted row in sixteen. A LIMIT is required:
/// without one the order walk crosses the whole index and pays a membership
/// probe on every row of it, which is strictly more work than the equality plan
/// ever does.
///
/// WHICH density, though, is the whole question. An earlier version of this
/// sampled the equality posting's own sequence spread -- how thinly its matches
/// are scattered through the collection -- and it was WRONG, measurably: with
/// `cat` and `rating` correlated, the highest-rated 3,500 rows of the
/// benchmark's collection contain no `cafe` at all, the equality filter looks
/// perfectly broad in sequence space, and the order walk still crosses
/// thousands of rejected rows before its first hit. The query got 1.5x SLOWER.
///
/// So the probe walks the ORDER index itself, in the direction the real walk
/// would take, for a bounded prefix, and counts how many of those postings the
/// equality filter accepts. That is the statistic the plan actually depends on,
/// and correlation cannot hide from it. It costs [`ORDER_DRIVE_PROBE`] posting
/// steps and the same number of membership probes, once per prepared query.
fn order_index_drives_better(
    db: &Database,
    order: &IndexInfo,
    descending: bool,
    filter: &IndexInfo,
    expected: &[u8],
) -> QueryResult<bool> {
    let prefix = scalar_prefix(order.id);
    let mut walk = if descending {
        match db
            .index_range_reverse(order, &prefix_successor(&prefix))
            .map_err(QueryError::from)?
        {
            Some(iter) => ScalarWalk::Reverse(iter),
            None => return Ok(false),
        }
    } else {
        match db.index_range(order, &prefix).map_err(QueryError::from)? {
            Some(iter) => ScalarWalk::Forward(iter),
            None => return Ok(false),
        }
    };
    let mut probed = 0u64;
    let mut accepted = 0u64;
    while probed < ORDER_DRIVE_PROBE {
        let sequence = {
            let Some((key, _)) = walk.peek().map_err(Error::from).map_err(QueryError::from)? else {
                break;
            };
            if !key.starts_with(&prefix) {
                break;
            }
            let suffix = &key[prefix.len()..];
            let value_len = scalar_key::width(&order.kind, suffix)?;
            let mut at = prefix.len() + value_len;
            read_ordered(key, &mut at)?
        };
        walk.step();
        probed += 1;
        if db
            .index_get(filter, &super::indexes::skey(filter, expected, sequence))
            .map_err(QueryError::from)?
            .is_some()
        {
            accepted += 1;
        }
    }
    // A short index is walked end to end either way; only a real prefix says
    // anything about the rest.
    if probed < ORDER_DRIVE_PROBE {
        return Ok(false);
    }
    Ok(accepted.saturating_mul(ORDER_DRIVE_MIN_DENSITY) >= probed)
}

/// Checks the request against the graph's identities and hands the header
/// back, because the walk needs the same header this read.
///
/// The header used to be read from the store TWICE per traversal -- once
/// here and once at the top of `execute_graph` -- through
/// `read_graph_header`, which reads three replica keys and verifies them,
/// while the graph code keeps a decoded copy in `graph_header_cache` for
/// exactly this question.
fn validate_graph_request(
    db: &Database,
    request: BfsRequest,
) -> QueryResult<super::graph_collections::GraphHeader> {
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
    let header = db.graph_header()?;
    if request.context.0 >= header.next_context
        || request
            .edge_type
            .is_some_and(|edge_type| edge_type.0 == 0 || edge_type.0 >= header.next_type)
    {
        return Err(invalid_query("unknown graph context or edge type"));
    }
    Ok(header)
}

fn visit_graph_direction<C: FnMut() -> bool>(
    db: &Database,
    header: super::graph_collections::GraphHeader,
    entity: EntityId,
    direction: Direction,
    request: &BfsRequest,
    seen: &[EntityId],
    next: &mut super::graph_collections::Frontier,
    scanned: &mut usize,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<()> {
    let incoming = direction == Direction::Incoming;
    let tag = if incoming {
        super::graph_collections::REVERSE_EDGE
    } else {
        super::graph_collections::PRIMARY_EDGE
    };
    // The prefix is built into a stack buffer. It used to be a `Vec` per
    // direction per entity, which on a wide frontier is one heap allocation
    // per step of the walk for bytes that never leave this function.
    let mut buffer = [0u8; super::graph_collections::MAX_EDGE_PREFIX];
    let at0 = super::graph_collections::edge_prefix_into(
        &mut buffer,
        tag,
        entity,
        Some(request.context),
        request.edge_type,
    );
    let prefix = &buffer[..at0];
    // The near entity, the context and (when the caller named one) the type
    // are the prefix itself: `starts_with` proves the row carries exactly the
    // bytes we built, so only the far endpoint has to be read back out.
    //
    // `for_each_ref` hands the callback borrows into the pinned leaf. The
    // allocating cursor built a key `Vec` and a value `Vec` for every edge
    // walked, for a parser that only reads them. The work meter is charged on
    // exactly the old schedule: one unit per turn of the loop, including the
    // turn that found no further row.
    let (pinned, context, max_edges) = (request.edge_type, request.context, request.max_edges);
    let mut failure: Option<QueryError> = None;
    let mut stopped = false;
    {
        let mut step = |key: &[u8], value: &[u8]| -> QueryResult<bool> {
            meter.charge(WorkResource::GraphEdges, 1)?;
            if !key.starts_with(prefix) {
                return Ok(false);
            }
            *scanned = scanned
                .checked_add(1)
                .ok_or_else(|| invalid_query("BFS edge work overflow"))?;
            if *scanned > max_edges {
                return Err(invalid_query("BFS edge work limit exceeded"));
            }
            // The SQL traversal returns entities, so it decodes no properties
            // and reads nothing across the pair: both directions are written in
            // one transaction, so a committed snapshot cannot hold half a pair,
            // and `verify_indexed_source` is the tool that checks pair
            // consistency.
            if incoming && !value.is_empty() {
                return Err(corrupt_query("nonempty reverse edge marker"));
            }
            let (_, adjacent) = super::graph_collections::adjacent_from_tail(
                key, at0, pinned, context, header,
            )?;
            // The visited set is a SORTED VECTOR and the level being built is
            // a `Frontier`, the same two structures `traverse_bfs` uses. The
            // three `BTreeSet`s this walk used to keep -- visited, level and
            // answer -- allocated a heap cell every few entities to answer a
            // question a binary search answers for nothing.
            if seen.binary_search(&adjacent).is_err() {
                next.offer(adjacent)
                    .map_err(|_| invalid_query("BFS visited limit exceeded"))?;
            }
            Ok(true)
        };
        db.store()?
            .range(prefix)
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

/// The breadth-first walk behind `QueryFilter::Graph`, answering with the
/// distinct entities it reached in ascending entity order.
///
/// It is the walk `Database::traverse_bfs` runs, built out of the same
/// pieces: a sorted visited vector merged one level at a time, a `Frontier`
/// for the level being discovered, prefixes built on the stack, and the graph
/// header the graph code already has in hand. What it adds is the query
/// engine's work meter, charged on the same schedule as before -- one unit
/// per turn of the edge loop, and one per DISTINCT entity a level discovers,
/// which is what the old per-edge charge added up to.
fn execute_graph<C: FnMut() -> bool>(
    db: &Database,
    request: BfsRequest,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<Vec<EntityId>> {
    let header = validate_graph_request(db, request)?;
    meter.charge(WorkResource::GraphVisited, 1)?;
    let mut seen = vec![request.seed];
    let mut merged: Vec<EntityId> = Vec::new();
    let mut results: Vec<EntityId> = Vec::new();
    if request.include_seed && request.min_depth == 0 {
        if request.result_limit == 0 {
            return Err(invalid_query("BFS result limit exceeded"));
        }
        results.push(request.seed);
    }
    let mut frontier = vec![request.seed];
    let mut scanned = 0usize;
    for depth in 1..=request.max_depth {
        meter.check_cancelled()?;
        let mut next = super::graph_collections::Frontier::new(request.max_visited - seen.len());
        for entity in frontier {
            meter.check_cancelled()?;
            if matches!(request.direction, Direction::Outgoing | Direction::Both) {
                visit_graph_direction(
                    db,
                    header,
                    entity,
                    Direction::Outgoing,
                    &request,
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
                    &request,
                    &seen,
                    &mut next,
                    &mut scanned,
                    meter,
                )?;
            }
        }
        let next = next.into_sorted();
        meter.charge(WorkResource::GraphVisited, next.len() as u64)?;
        if seen.len() + next.len() > request.max_visited {
            return Err(invalid_query("BFS visited limit exceeded"));
        }
        super::graph_collections::merge_sorted_disjoint(&mut seen, &next, &mut merged);
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
    // The seed-existence refusal, paid only when it can still be the answer.
    // An edge cannot outlive its endpoints -- a write validates both and a
    // delete cascades -- so an edge already walked is itself the proof that
    // the seed's row is there. This read used to be paid by every traversal,
    // before the walk, whether or not anything was going to need it; it is
    // the same rule `traverse_bfs` and `neighbors` already apply.
    if scanned == 0 {
        meter.charge(WorkResource::PrimaryReads, 1)?;
        if db.store()?.get(&row_key(request.seed))?.is_none() {
            return Err(QueryError::Database(Error::NotFound("graph endpoint")));
        }
    }
    // Each level is sorted, but the levels were appended in discovery order.
    // Membership is asked once per candidate and the driver promises
    // ascending ids, so the answer is sorted once here.
    results.sort_unstable();
    Ok(results)
}

fn execute_graph_filters<C: FnMut() -> bool>(
    db: &Database,
    filters: &[CompiledFilter],
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<Vec<Option<Vec<EntityId>>>> {
    let mut results = vec![None; filters.len()];
    for filter in filters {
        if let CompiledFilter::Graph { request, position } = filter {
            results[*position] = Some(execute_graph(db, *request, meter)?);
        }
    }
    Ok(results)
}

/// An index key a driver carried over from the entry it was standing on.
///
/// Exactly one driver kind ever fills this in, and each consumer proves the
/// key is the one IT asked for by matching the index id as well as the
/// variant. As three separate `Option<(IndexId, Vec<u8>)>` fields this cost 96
/// bytes on EVERY candidate -- including the 20,000 of a key-only scan, which
/// carry none of them -- and a candidate is built, moved and dropped once per
/// row.
enum CarriedKey {
    /// A scalar posting's value key, read only by a ranking over that index.
    Scalar(IndexId, Vec<u8>),
    /// An exact-vector locator.
    Vector(IndexId, Vec<u8>),
    /// A quantized-vector entry.
    Quantized(IndexId, Vec<u8>),
    /// The Hilbert cell a spatial posting is filed under, read only by a
    /// ranking in that index's own walk order. The point is the posting's
    /// own coordinates, so a Distance ranking over a spatial driver does
    /// not open the row.
    Cell(IndexId, u32, Point),
    /// The `(level, cell)` of the first geometry posting that admitted this
    /// entity, read only by a ranking in that index's own walk order.
    GeomCell(IndexId, u8, u32),
    /// A mapping entry's external-key bytes, read only by a ranking in the
    /// keys driver's own walk order.
    Key(Vec<u8>),
    /// A nearest-walk posting: coordinates and the geodesic distance already
    /// computed from the centre the walk was opened at.
    Distance(IndexId, Point, f64),
}

struct Candidate {
    id: EntityId,
    row: Option<Vec<u8>>,
    carried: Option<CarriedKey>,
    satisfied_filter: Option<usize>,
    /// What the batched row pass already decided about this candidate's
    /// FILTERS. `Some(true)` means every filter passed and there is nothing
    /// left to evaluate; `Some(false)` means one refused it. `None` means the
    /// pass did not settle it -- no batch, or a row it could not reach -- and
    /// the walk decides as it always did.
    row_filtered: Option<bool>,
    /// What the text merge cursor already knew about this document. Only the
    /// text driver fills it in; every other driver leaves it `None` and every
    /// scorer that cannot prove the frequencies are its own ignores it.
    text: Option<TextFrequencies>,
}

impl Candidate {
    /// One candidate that carries nothing but its identity.
    fn bare(id: EntityId) -> Self {
        Self {
            id,
            row: None,
            carried: None,
            satisfied_filter: None,
            row_filtered: None,
            text: None,
        }
    }

    /// The scalar value key this candidate came in on, if it came in on
    /// `index`'s postings.
    fn scalar(&self, index: IndexId) -> Option<&[u8]> {
        match &self.carried {
            Some(CarriedKey::Scalar(id, key)) if *id == index => Some(key),
            _ => None,
        }
    }

    fn vector(&self, index: IndexId) -> Option<&[u8]> {
        match &self.carried {
            Some(CarriedKey::Vector(id, key)) if *id == index => Some(key),
            _ => None,
        }
    }

    fn quantized(&self, index: IndexId) -> Option<&[u8]> {
        match &self.carried {
            Some(CarriedKey::Quantized(id, key)) if *id == index => Some(key),
            _ => None,
        }
    }

    /// The spatial cell this candidate came in on, if it came in on
    /// `index`'s postings.
    fn cell(&self, index: IndexId) -> Option<u32> {
        match &self.carried {
            Some(CarriedKey::Cell(id, cell, _)) if *id == index => Some(*cell),
            _ => None,
        }
    }

    fn point(&self, index: IndexId) -> Option<Point> {
        match &self.carried {
            Some(CarriedKey::Cell(id, _, point) | CarriedKey::Distance(id, point, _))
                if *id == index =>
            {
                Some(*point)
            }
            _ => None,
        }
    }

    fn distance_metres(&self, index: IndexId) -> Option<f64> {
        match &self.carried {
            Some(CarriedKey::Distance(id, _, distance)) if *id == index => Some(*distance),
            _ => None,
        }
    }

    fn geom_cell(&self, index: IndexId) -> Option<(u8, u32)> {
        match &self.carried {
            Some(CarriedKey::GeomCell(id, level, cell)) if *id == index => Some((*level, *cell)),
            _ => None,
        }
    }

    /// The external-key bytes this candidate came in on, if it came from the
    /// keys driver's mapping-entry walk.
    fn key(&self) -> Option<&[u8]> {
        match &self.carried {
            Some(CarriedKey::Key(key)) => Some(key),
            _ => None,
        }
    }
}

struct EntityCursor<'a> {
    inner: RangeIter<'a>,
    prefix: Vec<u8>,
    /// The collection the prefix was built from. A key that matched the
    /// prefix has already proved its collection; carrying it here is what
    /// lets the id decode read only the sequence.
    collection: CollectionId,
    /// Copy the primary row out of the leaf, or read its key only. The bytes
    /// are worth an allocation only when a filter or the ranking will decode
    /// them; a key-only scan used to allocate one per row and drop it.
    wants_row: bool,
    done: bool,
}

/// One posting range, walked in one direction. The two cursors are the same
/// tree read the two ways round: ascending from the predicate's lower bound,
/// descending from one key past its upper bound.
enum ScalarWalk<'a> {
    /// Nothing can match -- the `Empty` predicate, or an index whose tree has
    /// never been written.
    Nothing,
    Forward(RangeIter<'a>),
    Reverse(ReverseRangeIter<'a>),
}

impl ScalarWalk<'_> {
    fn peek(&mut self) -> kernel::Result<Option<(&[u8], &[u8])>> {
        match self {
            Self::Nothing => Ok(None),
            Self::Forward(iter) => iter.peek_ref(),
            Self::Reverse(iter) => iter.peek_ref(),
        }
    }

    fn step(&mut self) {
        match self {
            Self::Nothing => {}
            Self::Forward(iter) => iter.step(),
            Self::Reverse(iter) => iter.step(),
        }
    }

    /// True when the walk runs from high keys to low ones, which inverts what
    /// "before the predicate" and "past the predicate" mean.
    fn descending(&self) -> bool {
        matches!(self, Self::Reverse(_))
    }
}

struct ScalarCursor<'a> {
    walk: ScalarWalk<'a>,
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
    key: bool,
}

/// One mapping-keyspace range, walked ascending. Unlike `ScalarWalk` there is
/// no reverse variant: `QueryOrder::Driver` has no direction of its own (see
/// `DriverKey`'s doc), so nothing ever asks this cursor to open backwards --
/// stated, not implemented, per item KD's scope.
struct KeysCursor<'a> {
    inner: RangeIter<'a>,
    prefix: Vec<u8>,
    collection: CollectionId,
    predicate: EncodedScalarFilter,
    /// The filter position this cursor's own walk already proves, if any.
    /// Unlike a scalar posting there is no nullish sentinel to leave
    /// uncertified: every mapping entry this cursor yields is a real,
    /// present key inside the predicate, full stop.
    certifies: Option<usize>,
    wants_key: bool,
    done: bool,
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
    /// The posting the merge is standing on: document AND term frequency.
    /// `TermPostings::next` decodes both; the frequency used to be dropped
    /// here and re-read, per document, by the scorer.
    head: Option<(u64, u32)>,
    done: bool,
}

struct TextCursor<'a> {
    streams: Vec<TextPostingCursor<'a>>,
    collection: CollectionId,
    matching: TextMatch,
    position: Option<usize>,
    /// Which compiled text site this cursor is, stamped onto every candidate
    /// it emits so only the scorer that asked for these terms reads them.
    source: TextSource,
    /// Are there few enough query terms to carry their frequencies inline?
    carries: bool,
    /// The last document emitted, to prove the merge ascends.
    previous: Option<u64>,
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
    /// Where the FIRST range this cursor opens starts, when the page is
    /// resuming: the exact posting the previous page stopped on. `None` opens
    /// at the range's own low cell, which is what an unresumed walk does and
    /// what every page used to do.
    start: Option<Vec<u8>>,
    inner: Option<RangeIter<'a>>,
    done: bool,
}

/// Walks a geometry index's cover ranges, admitting a posting when its `BoxF`
/// overlaps the query bbox. Dedup is a `HashSet` of sequences: an entity
/// posts at most 8 cells at one level, and other entities interleave, so a
/// consecutive-run exploit is not sound. Under Driver order the set is
/// cloned onto the next page so a later posting of an already-emitted
/// entity is not re-yielded after a resume.
///
/// The box is only a candidate test. This cursor does NOT certify the
/// filter (`satisfied_filter` stays `None`): T3's no-row rule does not
/// apply, and the row's geometry is refined through `spatial_geometry`.
struct GeometryCursor<'a> {
    db: &'a Database,
    info: IndexInfo,
    prefix: Vec<u8>,
    ranges: Vec<GeomRange>,
    range: usize,
    start: Option<Vec<u8>>,
    inner: Option<RangeIter<'a>>,
    query_bbox: BoxF,
    seen: HashSet<u64>,
    done: bool,
}

enum DriverCursor<'a> {
    Entities(EntityCursor<'a>),
    Scalar(ScalarCursor<'a>),
    Spatial(SpatialCursor<'a>),
    Nearest(NearestCursor<'a>),
    Geometry(GeometryCursor<'a>),
    Text(TextCursor<'a>),
    Vector(VectorCursor<'a>),
    QuantizedVector(QuantizedVectorCursor<'a>),
    Keys(KeysCursor<'a>),
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

/// The successor of a prefix: the shortest key above every key that starts
/// with it. A scalar prefix begins with the family tag `0x70`, so the loop
/// always finds a byte to raise.
fn prefix_successor(prefix: &[u8]) -> Vec<u8> {
    let mut key = prefix.to_vec();
    while let Some(last) = key.last_mut() {
        if *last == u8::MAX {
            key.pop();
        } else {
            *last += 1;
            return key;
        }
    }
    vec![u8::MAX; 64]
}

/// One key past every posting of `value`. Postings are
/// `prefix || value || ordered(sequence)`, so the widest of them is the one
/// with the maximum sequence, and one byte beyond that sits above the whole
/// group and below the next value.
fn past_scalar_value(info: &IndexInfo, value: &[u8]) -> Vec<u8> {
    let mut key = super::indexes::skey(info, value, u64::MAX);
    key.push(0);
    key
}

/// Where a DESCENDING walk of this predicate opens. `range_reverse` yields
/// keys strictly below its argument, so this is the exclusive upper edge of
/// the predicate's range rather than its last member.
fn scalar_reverse_start(
    info: &IndexInfo,
    prefix: &[u8],
    predicate: &EncodedScalarFilter,
) -> Option<Vec<u8>> {
    match predicate {
        EncodedScalarFilter::Empty => None,
        EncodedScalarFilter::Eq(value) => Some(past_scalar_value(info, value)),
        EncodedScalarFilter::IsNull | EncodedScalarFilter::IsMissing => {
            Some(past_scalar_value(info, NULLISH_SCALAR_KEY))
        }
        EncodedScalarFilter::Range { upper, .. } => Some(match upper {
            EncodedBound::Included(value) => past_scalar_value(info, value),
            // Every posting of `value` sorts after the bare `prefix || value`,
            // so opening there excludes the whole group, which is what an
            // exclusive upper bound means.
            EncodedBound::Excluded(value) => {
                let mut key = prefix.to_vec();
                key.extend_from_slice(value);
                key
            }
            EncodedBound::Unbounded => prefix_successor(prefix),
        }),
    }
}

/// Where a resumed DESCENDING walk opens.
///
/// A descending query ranks by (value descending, entity id ASCENDING) -- the
/// id half never flips -- while the reverse walk hands a tie group over id
/// DESCENDING. So the previous page's last key is NOT a point the walk can
/// resume from: the rows still owed inside that tie group lie *earlier* in
/// the walk, not later. Resuming at the top of the boundary value's group
/// re-walks that one group and lets `next_page`'s `after` comparison drop the
/// members it already emitted. The re-walk is bounded by one value's tie
/// group per page; opening at the start of the index is bounded by everything
/// emitted so far.
fn resume_scalar_reverse_key(info: &IndexInfo, after: &RankKey) -> Option<Vec<u8>> {
    match &after.value {
        RankValue::Scalar(value) => Some(past_scalar_value(info, value)),
        _ => None,
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

/// The mapping key a resumed keys walk should open at: the previous page's
/// last key, re-yielded and dropped by `next_page`'s own `after` comparison --
/// the same shape `EntityCursor`'s resume uses. One function serves both
/// directions here (unlike `resume_scalar_key`/`resume_scalar_reverse_key`)
/// because a mapping entry is unique per key: there is no tie group whose
/// still-owed members lie on the far side of it.
fn resume_key_walk(prefix: &[u8], after: &RankKey) -> Option<Vec<u8>> {
    match &after.value {
        RankValue::Key(key) => {
            let mut start = prefix.to_vec();
            start.extend_from_slice(key);
            Some(start)
        }
        _ => None,
    }
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
                EncodedBound::Included(value) => key < value.as_slice(),
                EncodedBound::Excluded(value) => key <= value.as_slice(),
                EncodedBound::Unbounded => false,
            };
            if below {
                return Ordering::Less;
            }
            let above = match upper {
                EncodedBound::Included(value) => key > value.as_slice(),
                EncodedBound::Excluded(value) => key >= value.as_slice(),
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
        graph: &[Option<Vec<EntityId>>],
        needs: CursorNeeds,
        resume: Option<&RankKey>,
        descending: bool,
        nearest: Option<&'a mut super::spatial_indexes::NearestWalk>,
        geometry_seen: HashSet<u64>,
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
                    collection,
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
                let walk = if descending {
                    match scalar_reverse_start(info, &prefix, predicate) {
                        None => ScalarWalk::Nothing,
                        Some(mut start) => {
                            if let Some(key) =
                                resume.and_then(|after| resume_scalar_reverse_key(info, after))
                            {
                                start = key;
                            }
                            match db.index_range_reverse(info, &start).map_err(QueryError::from)? {
                                Some(iter) => ScalarWalk::Reverse(iter),
                                None => ScalarWalk::Nothing,
                            }
                        }
                    }
                } else if matches!(predicate, EncodedScalarFilter::Empty) {
                    ScalarWalk::Nothing
                } else {
                    let mut start = prefix.clone();
                    if let Some(lower) = scalar_lower(predicate) {
                        start.extend_from_slice(lower);
                    }
                    if let Some(key) = resume.and_then(|after| resume_scalar_key(info, predicate, after))
                    {
                        start = key;
                    }
                    match db.index_range(info, &start).map_err(QueryError::from)? {
                        Some(iter) => ScalarWalk::Forward(iter),
                        None => ScalarWalk::Nothing,
                    }
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
                    walk,
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
                    // A traversal hands its result over sorted, so under
                    // driver order a resumed page opens past the row the last
                    // one ended on: the same walk with its head cut off.
                    .filter(|id| match resume {
                        Some(after) => *id > after.id,
                        None => true,
                    })
                    .collect::<Vec<_>>();
                Ok(Self::Ids(ids.into_iter()))
            }
            DriverPlan::Spatial {
                info,
                predicate,
                position,
                ranges,
                ..
            } => {
                // A spatial posting is `prefix || cell || sequence` and the
                // merged Hilbert ranges are walked low cell to high, so the
                // walk ascends by `(cell, sequence)` from end to end. Under
                // driver order that IS the rank key, so the previous page's
                // last key is a posting key this cursor can open on: the
                // ranges wholly below it are finished and skipped, and the
                // one holding it opens at the posting itself. That posting is
                // yielded once more and dropped by the `after` comparison in
                // `next_page` -- one candidate per page, where re-opening at
                // the envelope's first cell costs a whole pass, and a whole
                // geodesic refine of it, per page.
                let prefix = super::spatial_indexes::posting_prefix(info.id);
                let mut range = 0usize;
                let mut start = None;
                if let Some(RankKey {
                    value: RankValue::Cell(cell),
                    id,
                }) = resume
                {
                    while ranges
                        .get(range)
                        .is_some_and(|(_, hi)| *hi < u64::from(*cell))
                    {
                        range += 1;
                    }
                    // A cell that falls in the GAP between two ranges leaves
                    // the walk at the next range's own start; only a cell
                    // inside a range names a posting to open on.
                    if ranges
                        .get(range)
                        .is_some_and(|(lo, _)| *lo <= u64::from(*cell))
                    {
                        start = Some(super::spatial_indexes::posting_key_at(
                            info.id,
                            *cell,
                            id.sequence,
                        ));
                    }
                }
                Ok(Self::Spatial(SpatialCursor {
                    db,
                    info: info.clone(),
                    predicate: *predicate,
                    position: *position,
                    prefix,
                    ranges: ranges.clone(),
                    range,
                    start,
                    inner: None,
                    done: false,
                }))
            }
            DriverPlan::Nearest {
                info, certifies, ..
            } => {
                let walk = nearest.ok_or_else(|| {
                    corrupt_query("nearest walk missing from prepared query")
                })?;
                // The page before this one kept one hit past what it returned;
                // hand it over again so the `after` comparison can drop it.
                if let Some(RankKey {
                    value: RankValue::Score(bits),
                    id,
                }) = resume
                {
                    walk.rewind_past(f64::from_bits(*bits), *id);
                }
                Ok(Self::Nearest(NearestCursor {
                    db,
                    index: info.id,
                    certifies: *certifies,
                    walk,
                }))
            }
            DriverPlan::Geometry {
                info,
                position: _,
                ranges,
                query_bbox,
                ..
            } => {
                let prefix = super::spatial_geometry_indexes::posting_prefix(info.id);
                let mut range = 0usize;
                let mut start = None;
                if let Some(RankKey {
                    value: RankValue::GeomCell { level, cell },
                    id,
                }) = resume
                {
                    while ranges.get(range).is_some_and(|r| {
                        r.level < *level || (r.level == *level && r.hi < u64::from(*cell))
                    }) {
                        range += 1;
                    }
                    if ranges.get(range).is_some_and(|r| {
                        r.level == *level && r.lo <= u64::from(*cell)
                    }) {
                        start = Some(super::spatial_geometry_indexes::posting_key_at(
                            info.id,
                            *level,
                            *cell,
                            id.sequence,
                        ));
                    }
                }
                Ok(Self::Geometry(GeometryCursor {
                    db,
                    info: info.clone(),
                    prefix,
                    ranges: ranges.clone(),
                    range,
                    start,
                    inner: None,
                    query_bbox: *query_bbox,
                    seen: geometry_seen,
                    done: false,
                }))
            }
            DriverPlan::Text { prepared, position } => {
                // The merge hands documents over in strictly ascending
                // sequence -- it says so and refuses a round that does not
                // advance -- so under an id ranking the previous page's last
                // key is a document number every term stream can be opened
                // at. Each stream seeks to it; nothing walks the postings the
                // earlier pages already emitted.
                let from = resume.map(|after| after.id.sequence);
                let mut streams = Vec::with_capacity(prepared.terms.len());
                for (term, expected) in prepared.terms.iter().zip(&prepared.dfs) {
                    streams.push(TextPostingCursor {
                        inner: match from {
                            Some(from) => super::text_indexes::TermPostings::open_from(
                                db,
                                prepared.info.id,
                                term,
                                *expected,
                                from,
                            )?,
                            None => super::text_indexes::TermPostings::open(
                                db,
                                prepared.info.id,
                                term,
                                *expected,
                            )?,
                        },
                        head: None,
                        done: false,
                    });
                }
                let carries = streams.len() <= INLINE_TEXT_TERMS;
                Ok(Self::Text(TextCursor {
                    streams,
                    collection: prepared.info.collection,
                    matching: prepared.matching,
                    position: *position,
                    source: match position {
                        Some(position) => TextSource::Filter(*position),
                        None => TextSource::Order,
                    },
                    carries,
                    previous: None,
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
            DriverPlan::Keys { predicate, position } => {
                let prefix = prefix(0x20, collection);
                let mut start = prefix.clone();
                if let Some(lower) = scalar_lower(predicate) {
                    start.extend_from_slice(lower);
                }
                if let Some(key) = resume.and_then(|after| resume_key_walk(&prefix, after)) {
                    start = key;
                }
                let inner = db
                    .store()?
                    .range(&start)
                    .map_err(Error::from)
                    .map_err(QueryError::from)?;
                // A posting-membership-style certification: every entry this
                // cursor yields already passed `scalar_key_position` against
                // the predicate, so there is no candidate left for
                // `filters_match` to re-check -- see `CompiledFilter::Key`.
                let certifies = match (position, predicate) {
                    (Some(position), EncodedScalarFilter::Range { .. }) => Some(*position),
                    _ => None,
                };
                Ok(Self::Keys(KeysCursor {
                    inner,
                    prefix,
                    collection,
                    predicate: predicate.clone(),
                    certifies,
                    wants_key: needs.key,
                    done: matches!(predicate, EncodedScalarFilter::Empty),
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
            Self::Nearest(cursor) => cursor.next(meter),
            Self::Geometry(cursor) => cursor.next(meter),
            Self::Text(cursor) => cursor.next(meter),
            Self::Vector(cursor) => cursor.next(meter),
            Self::QuantizedVector(cursor) => cursor.next(meter),
            Self::Keys(cursor) => cursor.next(meter),
            Self::Ids(ids) => Ok(ids.next().map(Candidate::bare)),
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
            if !super::has_prefix(key, &self.prefix) {
                self.done = true;
                return Ok(None);
            }
            (
                super::row_id_after_prefix(key, self.prefix.len(), self.collection)?,
                if self.wants_row {
                    Some(value.to_vec())
                } else {
                    None
                },
            )
        };
        self.inner.step();
        Ok(Some(Candidate { row, ..Candidate::bare(id) }))
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
        // Which side of the predicate the walk has NOT reached yet, and which
        // side means it is finished. An ascending walk approaches from below;
        // a descending one approaches from above.
        let past = if self.walk.descending() {
            Ordering::Less
        } else {
            Ordering::Greater
        };
        loop {
            meter.charge(WorkResource::ScalarPostings, 1)?;
            // Same pull-cursor shape as the entity walk: peek into the pinned
            // leaf, decide, then step. The key and the (always empty) value of
            // a posting were a `Vec` each before.
            let decoded = {
                let Some((key, value)) = self
                    .walk
                    .peek()
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
                let value_len = scalar_key::width(&self.info.kind, suffix)?;
                let encoded = suffix
                    .get(..value_len)
                    .ok_or_else(|| corrupt_query("truncated scalar value key"))?;
                let position = scalar_key_position(&self.predicate, encoded);
                if position == past {
                    self.done = true;
                    return Ok(None);
                }
                match position {
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
                    // Not yet inside the predicate: keep walking.
                    _ => None,
                }
            };
            self.walk.step();
            let Some((sequence, encoded, proves_predicate)) = decoded else {
                continue;
            };
            return Ok(Some(Candidate {
                carried: encoded.map(|key| CarriedKey::Scalar(self.info.id, key)),
                satisfied_filter: self.certifies.filter(|_| proves_predicate),
                ..Candidate::bare(EntityId {
                    collection: self.info.collection,
                    sequence,
                })
            }));
        }
    }
}

impl KeysCursor<'_> {
    fn next<C: FnMut() -> bool>(
        &mut self,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Option<Candidate>> {
        if self.done {
            return Ok(None);
        }
        loop {
            meter.charge(WorkResource::KeyPostings, 1)?;
            let decoded = {
                let Some((key, value)) = self
                    .inner
                    .peek_ref()
                    .map_err(Error::from)
                    .map_err(QueryError::from)?
                else {
                    self.done = true;
                    return Ok(None);
                };
                if !super::has_prefix(key, &self.prefix) {
                    self.done = true;
                    return Ok(None);
                }
                let suffix = &key[self.prefix.len()..];
                let position = scalar_key_position(&self.predicate, suffix);
                if position == Ordering::Greater {
                    self.done = true;
                    return Ok(None);
                }
                match position {
                    Ordering::Equal => {
                        let mut at = 0;
                        let sequence = read_ordered(value, &mut at)?;
                        if at != value.len() || sequence == 0 {
                            return Err(corrupt_query("external-key mapping entry"));
                        }
                        Some((
                            sequence,
                            if self.wants_key {
                                Some(suffix.to_vec())
                            } else {
                                None
                            },
                        ))
                    }
                    // Below the predicate's lower bound: keep walking.
                    Ordering::Less => None,
                    Ordering::Greater => unreachable!("handled above"),
                }
            };
            self.inner.step();
            let Some((sequence, key_bytes)) = decoded else {
                continue;
            };
            return Ok(Some(Candidate {
                carried: key_bytes.map(CarriedKey::Key),
                satisfied_filter: self.certifies,
                ..Candidate::bare(EntityId {
                    collection: self.collection,
                    sequence,
                })
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
            .next(&mut || meter.charge(WorkResource::TextPostings, 1))?;
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
        // The frequencies this document's streams are standing on, harvested
        // before the streams are advanced off them.
        let mut slots = [0u32; INLINE_TEXT_TERMS];
        let sequence = match self.matching {
            TextMatch::Any => {
                let Some(sequence) = self
                    .streams
                    .iter()
                    .filter_map(|stream| stream.head.map(|(sequence, _)| sequence))
                    .min()
                else {
                    self.done = true;
                    return Ok(None);
                };
                for (position, stream) in self.streams.iter_mut().enumerate() {
                    if stream.head.is_some_and(|(at, _)| at == sequence) {
                        if let Some(slot) = slots.get_mut(position) {
                            *slot = stream.head.unwrap().1;
                        }
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
                    .filter_map(|stream| stream.head.map(|(sequence, _)| sequence))
                    .max()
                    .ok_or_else(|| corrupt_query("initialized text stream has no head"))?;
                for stream in &mut self.streams {
                    while stream.head.is_some_and(|(sequence, _)| sequence < target) {
                        stream.advance(meter)?;
                    }
                }
                if self.streams.iter().any(|stream| stream.done) {
                    continue;
                }
                if self
                    .streams
                    .iter()
                    .all(|stream| stream.head.is_some_and(|(at, _)| at == target))
                {
                    for (position, stream) in self.streams.iter_mut().enumerate() {
                        if let Some(slot) = slots.get_mut(position) {
                            *slot = stream.head.unwrap().1;
                        }
                        stream.advance(meter)?;
                    }
                    break target;
                }
            },
        };
        // The merge emits documents in strictly ascending sequence -- every
        // stream is ascending (`TermPostings` refuses a posting that does not
        // advance) and each round takes the smallest or the common head and
        // then steps past it. The scorer's segment and norm windows amortize
        // against exactly this; if it ever stopped holding, they would go on
        // answering correctly but at the old per-document cost, so it is
        // cheaper to state it here than to discover it in a profile.
        if self.previous.is_some_and(|previous| previous >= sequence) {
            return Err(corrupt_query("text merge did not advance"));
        }
        self.previous = Some(sequence);
        Ok(Some(Candidate {
            // Phrase postings establish only distinct all-term candidacy. The
            // filter remains pending until authoritative primary refinement.
            satisfied_filter: if self.matching == TextMatch::Phrase {
                None
            } else {
                self.position
            },
            text: self.carries.then(|| TextFrequencies {
                source: self.source,
                len: self.streams.len() as u8,
                slots,
            }),
            ..Candidate::bare(EntityId {
                collection: self.collection,
                sequence,
            })
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
                // `start` is the resumed posting, and only the first range
                // this cursor opens can have one.
                let start = match self.start.take() {
                    Some(key) => key,
                    None => {
                        let mut start = self.prefix.clone();
                        start.extend((lo as u32).to_be_bytes());
                        start
                    }
                };
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
            let cell = key
                .get(self.prefix.len()..self.prefix.len() + 4)
                .ok_or_else(|| corrupt_query("spatial point posting key"))?;
            let cell = u32::from_be_bytes(cell.try_into().unwrap());
            let hilbert = u64::from(cell);
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
                satisfied_filter: Some(self.position),
                // The cell this posting is filed under. A page ranked in the
                // driver's own order ranks by it; handing it over costs
                // nothing, because the key it came out of is decoded already.
                carried: Some(CarriedKey::Cell(self.info.id, cell, point)),
                ..Candidate::bare(EntityId {
                    collection: self.info.collection,
                    sequence,
                })
            }));
        }
    }
}

struct NearestCursor<'a> {
    db: &'a Database,
    index: IndexId,
    certifies: Option<usize>,
    walk: &'a mut super::spatial_indexes::NearestWalk,
}

impl NearestCursor<'_> {
    fn next<C: FnMut() -> bool>(
        &mut self,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Option<Candidate>> {
        let mut budget_err = None;
        let hit = {
            let mut extra = || match meter.charge(WorkResource::SpatialPostings, 1) {
                Ok(()) => Ok(()),
                Err(QueryError::Database(err)) => Err(err),
                Err(QueryError::Cancelled) => Err(Error::Cancelled),
                Err(err) => {
                    budget_err = Some(err);
                    Err(invalid("query budget"))
                }
            };
            self.walk
                .next(self.db, &mut || false, &mut extra)
                .map_err(QueryError::from)
        };
        if let Some(err) = budget_err {
            return Err(err);
        }
        let Some(hit) = hit? else {
            return Ok(None);
        };
        Ok(Some(Candidate {
            satisfied_filter: self.certifies,
            carried: Some(CarriedKey::Distance(
                self.index,
                hit.point,
                hit.distance_metres,
            )),
            ..Candidate::bare(hit.id)
        }))
    }
}

impl GeometryCursor<'_> {
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
            let GeomRange { level, lo, hi } = self.ranges[self.range];
            if self.inner.is_none() {
                let start = match self.start.take() {
                    Some(key) => key,
                    None => {
                        let mut start = self.prefix.clone();
                        start.push(level);
                        start.extend((lo as u32).to_be_bytes());
                        start
                    }
                };
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
            let (post_level, cell, sequence, bbox) =
                super::spatial_geometry_indexes::decode_posting(&self.prefix, &key, &value)?;
            if post_level != level {
                self.inner = None;
                self.range += 1;
                continue;
            }
            let hilbert = u64::from(cell);
            if hilbert > hi {
                self.inner = None;
                self.range += 1;
                continue;
            }
            if !bbox.intersects(&self.query_bbox) {
                continue;
            }
            if !self.seen.insert(sequence) {
                continue;
            }
            return Ok(Some(Candidate {
                // BoxF overlap is a candidate test, not a proof: the filter
                // is refined against the row. Do not certify.
                satisfied_filter: None,
                carried: Some(CarriedKey::GeomCell(self.info.id, level, cell)),
                ..Candidate::bare(EntityId {
                    collection: self.info.collection,
                    sequence,
                })
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
            carried: Some(CarriedKey::Vector(self.info.id, value)),
            ..Candidate::bare(EntityId {
                collection: self.info.collection,
                sequence,
            })
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
            carried: Some(CarriedKey::Quantized(self.info.id, value)),
            ..Candidate::bare(EntityId {
                collection: self.info.collection,
                sequence,
            })
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

/// How far ahead the lockstep cursor will walk before it gives up on itself.
///
/// `peek_at_or_after` reaches a key past its pinned leaf by binary-searching
/// the leaf it is on and then stepping to the next leaf, one at a time --
/// cheap for a row a few slots away, ruinous for a row a hundred leaves away.
/// A page whose winners are SPARSE in the primary tree (a nine-row spatial
/// answer, a one-row graph hop) would walk the whole collection's leaves to
/// collect them, and it did: `win/spatial_tiny` went 30 -> 109 us and
/// `graph/hop1_project` 11 -> 47 us before this bound.
///
/// The bound is a number of ROWS but the thing it is protecting against is
/// LEAVES. At 32 it was a fraction of one leaf of a narrow collection, so a
/// walk that is dense in runs and sparse between them -- `price > 400` matches
/// nine rows out of every forty-nine, so it steps 1,1,...,1,40 -- gave the
/// whole page back to the point-get at its FIRST gap, and then paid a full
/// root-to-leaf descent per winner: measured at 4.0 pager accesses and 6
/// allocations per row on `filter/range_open`.
///
/// How many leaves a row-gap spans is a property of the DATA, not of the plan:
/// the same gap of a hundred sequences is a fraction of a leaf in a collection
/// of forty-byte rows and a dozen leaves in one of five-hundred-byte rows. So
/// this stays a cheap pre-filter -- a gap wider than this cannot be worth
/// stepping under any row width, and the first reach is what it bounds -- and
/// what the page actually decides on is [`LOCKSTEP_LEAVES`], which the cursor
/// measures.
const LOCKSTEP_REACH: u64 = 256;

/// How many LEAVES one reach may cross before the page gives the rest of
/// itself back to the point-get.
///
/// `RangeIter::advance` climbs the parent path and re-descends the leftmost
/// spine for every leaf it steps, so a reach across a dozen leaves costs
/// several times the root-to-leaf descent it was replacing. A reach that stays
/// inside the pinned leaf costs nothing at all, and one that crosses a leaf or
/// two is still cheaper than a descent; past that the cursor is not paying for
/// itself and the reader stops pretending it is.
///
/// The cost is only knowable AFTER the reach, so the page pays one expensive
/// one and then stops -- the same shape as the row pre-filter above, which is
/// what keeps that pre-filter necessary: it is the bound on how bad that one
/// reach can be.
const LOCKSTEP_LEAVES: u32 = 2;

/// How a page reads primary rows.
///
/// When rows are asked for in ascending entity id -- an equality posting is
/// ordered by sequence, a graph result is sorted, the entity walk is the
/// primary tree, and an id ranking returns winners in that order too -- the
/// keys are ascending primary keys. One forward cursor can then step through
/// them, paying one pinned leaf for every row that lives on it, instead of
/// descending the tree from the root for each. Any other order keeps the
/// point-get it always did, and the cursor is opened only when something
/// actually reads a row.
struct PrimaryRows<'a> {
    db: &'a Database,
    ascending: bool,
    cursor: Option<RangeIter<'a>>,
    last: u64,
    /// The seek key, rebuilt in place. One `Vec` for the page rather than one
    /// per row read.
    key: Vec<u8>,
}

/// Which way one row is going to be reached.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// Forward from where the cursor is parked.
    Lockstep,
    /// A fresh descent from the root.
    PointGet,
}

impl<'a> PrimaryRows<'a> {
    fn new(db: &'a Database, ascending: bool) -> Self {
        Self {
            db,
            ascending,
            cursor: None,
            last: 0,
            key: Vec::new(),
        }
    }

    /// Point the reusable key at `id` and say how it will be reached, moving
    /// the cursor's own state along with the decision.
    fn plan(&mut self, id: EntityId) -> Reach {
        // `ordered` RETURNS a `Vec`, so building the key out of it allocated
        // twice -- nine bytes each -- for every row any page read, on every
        // path: the counting allocator put two of `filter/two_ranges`' six
        // allocations per candidate right here, for a key whose buffer is
        // already owned and already the right size. `ordered_into` is the same
        // frozen encoding appended to a buffer the caller keeps.
        self.key.clear();
        self.key.push(0x40);
        ordered_into(&mut self.key, u64::from(id.collection.0));
        ordered_into(&mut self.key, id.sequence);
        if !self.ascending {
            return Reach::PointGet;
        }
        if self.cursor.is_some() && id.sequence <= self.last {
            // The cursor only moves forward. A target behind it would make
            // `peek_at_or_after` return the record the cursor is parked on,
            // whose key does not match, and a present row would be reported
            // missing. Callers ascend by construction; this is the guard that
            // makes that a property of the reader rather than of every caller.
            return Reach::PointGet;
        }
        if self.cursor.is_some() && id.sequence.saturating_sub(self.last) > LOCKSTEP_REACH {
            // The rows this page wants are SPARSE in the primary tree, so the
            // cursor is not paying for itself: reaching each one costs a
            // re-descent anyway, and a cursor re-descent builds cursor state a
            // point-get does not. Give the rest of the page back to the
            // point-get. One page decides this once, from the first gap it
            // sees, and the next page decides again.
            self.ascending = false;
            self.cursor = None;
            return Reach::PointGet;
        }
        Reach::Lockstep
    }

    /// Point the reader at a fresh ascending run.
    ///
    /// The sparse bail-out in `plan` is a decision the reader makes ONCE and
    /// then keeps: it drops the cursor and never opens another. That is right
    /// for a page that reads its rows in one pass, and wrong for one that
    /// reads them in BATCHES -- each batch ascends from its own smallest id,
    /// so a batch that was sparse says nothing about the next one.
    fn restart(&mut self, ascending: bool) {
        self.ascending = ascending;
        self.cursor = None;
        self.last = 0;
    }

    /// What the reach the cursor just made actually cost, in leaves.
    ///
    /// A page that crossed more than [`LOCKSTEP_LEAVES`] to reach one row is
    /// sparse in the primary tree however close together its SEQUENCES looked,
    /// so it gives the rest of itself back to the point-get exactly as
    /// `plan`'s row pre-filter does.
    fn charge(&mut self, stepped: u32) {
        if stepped > LOCKSTEP_LEAVES {
            self.ascending = false;
            self.cursor = None;
        }
    }

    /// Park the cursor on the first record at or after the planned key.
    fn seek(&mut self, id: EntityId) -> QueryResult<()> {
        if self.cursor.is_none() {
            self.cursor = Some(
                self.db
                    .store()?
                    .range(&self.key)
                    .map_err(Error::from)
                    .map_err(QueryError::from)?,
            );
        }
        self.last = id.sequence;
        Ok(())
    }

    fn read(&mut self, id: EntityId) -> QueryResult<Option<Vec<u8>>> {
        if self.plan(id) == Reach::PointGet {
            return self
                .db
                .store()?
                .get(&self.key)
                .map_err(Error::from)
                .map_err(QueryError::from);
        }
        self.seek(id)?;
        let key = &self.key;
        let cursor = self.cursor.as_mut().expect("the cursor was just opened");
        let before = cursor.leaves_stepped();
        let found = match cursor
            .peek_at_or_after(key)
            .map_err(Error::from)
            .map_err(QueryError::from)?
        {
            Some((found, value)) if found == key.as_slice() => Some(value.to_vec()),
            _ => None,
        };
        let stepped = cursor.leaves_stepped() - before;
        self.charge(stepped);
        Ok(found)
    }

    /// Look at one row WITHOUT copying it out of the leaf.
    ///
    /// A page that reads a row only to answer a predicate -- `price > 100 AND
    /// rating < 3.0` reads 15,912 rows to return 7,956 -- copied every one of
    /// them into a `Vec` that the filter read one field out of and dropped.
    /// The lockstep cursor is standing on the record, so the bytes can be
    /// borrowed straight out of the pinned leaf; the point-get path has no
    /// such borrow to offer and materialises as it always did.
    fn with_row<R>(
        &mut self,
        id: EntityId,
        f: impl FnOnce(Option<&[u8]>) -> QueryResult<R>,
    ) -> QueryResult<R> {
        if self.plan(id) == Reach::PointGet {
            let bytes = self
                .db
                .store()?
                .get(&self.key)
                .map_err(Error::from)
                .map_err(QueryError::from)?;
            return f(bytes.as_deref());
        }
        self.seek(id)?;
        let key = &self.key;
        let cursor = self.cursor.as_mut().expect("the cursor was just opened");
        let before = cursor.leaves_stepped();
        let value = match cursor
            .peek_at_or_after(key)
            .map_err(Error::from)
            .map_err(QueryError::from)?
        {
            Some((found, value)) if found == key.as_slice() => f(Some(value)),
            _ => f(None),
        };
        let stepped = cursor.leaves_stepped() - before;
        self.charge(stepped);
        value
    }

    /// Is the row still there? The same reach, without the copy.
    ///
    /// A key-only page asks the primary tree for a winner's row only to refuse
    /// an orphan -- a posting can outlive the record it names. It then decodes
    /// nothing and drops the bytes, so copying a whole row out of the pinned
    /// leaf to answer a yes/no was one allocation and one row-sized memcpy per
    /// returned row, on every page that projects no field.
    fn exists(&mut self, id: EntityId) -> QueryResult<bool> {
        if self.plan(id) == Reach::PointGet {
            return Ok(self
                .db
                .store()?
                .get(&self.key)
                .map_err(Error::from)
                .map_err(QueryError::from)?
                .is_some());
        }
        self.seek(id)?;
        let key = &self.key;
        let cursor = self.cursor.as_mut().expect("the cursor was just opened");
        let before = cursor.leaves_stepped();
        let present = matches!(
            cursor
                .peek_at_or_after(key)
                .map_err(Error::from)
                .map_err(QueryError::from)?,
            Some((found, _)) if found == key.as_slice()
        );
        let stepped = cursor.leaves_stepped() - before;
        self.charge(stepped);
        Ok(present)
    }
}

/// How many candidates one batch of row reads holds.
///
/// The batch exists to turn a random point-get per candidate into one forward
/// pass of the primary tree, so it wants to be large; it holds a row per entry
/// while it does, so it cannot be unbounded. A page never gathers more than it
/// could return, and never more than this.
/// How many ranked rows one page may HOLD BACK for the pages after it.
///
/// A page whose driver walks in an order unrelated to the ranking reads the
/// whole candidate stream whatever it does (see [`PreparedQuery::run`]), so
/// the rows past this page are rows it has already ranked. Keeping them turns
/// the answer's cost from one pass PER PAGE into one pass per this many rows:
/// an answer of R rows costs `R / RUN_ROWS` passes instead of `R / page_size`.
/// At 48M rows `popsim`'s `born_decade` returned 5.58M rows in pages of 8,192
/// -- 681 passes over a 5.58M-posting range, which is what made it take
/// 1,011 s.
///
/// What is LEFT here is a walk whose order the RANKING does not share: a
/// spatial driver under an id or scalar ranking, whose cells arrive in no
/// order that ranking knows. Each of the big cases has since been asked in
/// the order its own driver produces instead -- `born_decade` in its index's
/// ascending order (`popsim` deviation 8), the radius and bbox cases in cell
/// order (deviation 9, `QueryOrder::Driver`) -- and a text-driven page never
/// needed the run, because its merge ascends by document. A query that still
/// asks for a re-ranking it cannot walk in keeps the run, and pays one pass
/// per this many rows rather than one per page.
///
/// The bound is in BYTES because what is held is a rank key each and the
/// promise has to mean the same thing whatever a rank key weighs. It is the
/// same order as the default buffer pool, it is transient -- it lives on the
/// prepared query and goes when the query does -- and it is only ever reached
/// by an answer large enough to have paid far more than this in re-walking.
const RUN_BYTES: usize = 8 << 20;

const RUN_ROWS: usize = {
    let rows = RUN_BYTES / std::mem::size_of::<HeapEntry>();
    if rows < MAX_PAGE_SIZE {
        MAX_PAGE_SIZE
    } else {
        rows
    }
};

const ROW_BATCH: usize = 4_096;

/// ... and how many bytes of row those entries may hold.
///
/// Rows are whatever the caller stored. The count bound alone would let a
/// batch of 4,096 megabyte blobs hold four gigabytes, so the read stops
/// filling at this many bytes and the candidates past it are read one at a
/// time exactly as before -- slower, and bounded.
const ROW_BATCH_BYTES: usize = 4 << 20;

/// Read one batch of candidates' rows in ASCENDING ENTITY ORDER.
///
/// The candidates arrive in the driver's order, which for a range posting is
/// `(value, sequence)`: the ids do not ascend, so the page's lockstep reader
/// gives up on its first gap and every candidate pays a fresh root-to-leaf
/// descent -- measured at 4.0 pager accesses and 6.0 allocations per candidate
/// on `filter/two_ranges`. Filters are pure functions of the row, so the ORDER
/// they are evaluated in cannot change the answer: the batch is sorted by id
/// here, read through one forward cursor, and handed back to the walk in its
/// original order with the rows already in hand.
///
/// A candidate whose row is absent is left alone rather than refused. A
/// posting can outlive the record it names, and whether that is an error is a
/// question for the filter that asked for the row -- not for a reader that is
/// only moving the read earlier.
fn read_batch_rows<'a, C: FnMut() -> bool>(
    db: &'a Database,
    rows: &mut PrimaryRows<'a>,
    filters: &[CompiledFilter],
    ranges: &[ScalarRangeSet],
    keep_rows: bool,
    batch: &mut [Candidate],
    order: &mut Vec<(u64, u32)>,
    scratch: &mut RowScratch,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<()> {
    order.clear();
    // The SEQUENCE travels with the index rather than being looked up through
    // it. Sorting indices alone makes every one of a 4,096-entry sort's ~49,000
    // comparisons an indirect load into a 64-byte-per-entry array; the
    // collection is the same for every candidate here (the walk refuses one
    // that crosses), so what orders them is one `u64` each.
    for (at, candidate) in batch.iter().enumerate() {
        if candidate.row.is_none() {
            order.push((
                candidate.id.sequence,
                u32::try_from(at).map_err(|_| invalid_query("query batch overflow"))?,
            ));
        }
    }
    order.sort_unstable();
    rows.restart(true);
    let mut held = 0usize;
    for (_, at) in order.iter() {
        meter.check_cancelled()?;
        let at = *at as usize;
        meter.charge(WorkResource::PrimaryReads, 1)?;
        let id = batch[at].id;
        let satisfied = batch[at].satisfied_filter;
        // Whether this row has to survive the pass as BYTES. A page that
        // projects fields, or ranks by a key only the row holds, keeps it; a
        // key-only page reads it to answer a predicate and then wants nothing
        // from it, and copying it out of the leaf for that was one allocation
        // and a row-sized memcpy per candidate.
        let (verdict, kept) = rows.with_row(id, |bytes| {
            let Some(bytes) = bytes else {
                return Ok((None, None));
            };
            let verdict = batch_filters_match(db, filters, ranges, satisfied, id, bytes, scratch, meter)?;
            let kept = if keep_rows && verdict != Some(false) && held < ROW_BATCH_BYTES {
                Some(bytes.to_vec())
            } else {
                None
            };
            Ok((verdict, kept))
        })?;
        if let Some(bytes) = kept {
            held = held.saturating_add(bytes.len());
            batch[at].row = Some(bytes);
        }
        batch[at].row_filtered = verdict;
    }
    Ok(())
}

/// Every filter of a BATCHED page, against one borrowed row.
///
/// `batches_row_reads` has already established that each of these is a pure
/// function of the row -- no graph set, no posting probe, no text merge -- so
/// evaluating them here rather than in driver order changes nothing but the
/// order the rows are read in. `None` means one of them was not of that kind
/// after all and the walk must decide; the gate makes that unreachable, and it
/// is a fallback rather than an assertion because being merely slow is the
/// right failure for a plan predicate that drifts.
fn batch_filters_match<C: FnMut() -> bool>(
    db: &Database,
    filters: &[CompiledFilter],
    ranges: &[ScalarRangeSet],
    satisfied: Option<usize>,
    id: EntityId,
    bytes: &[u8],
    scratch: &mut RowScratch,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<Option<bool>> {
    let layout = db.layout(layout_id(bytes)?)?;
    for (position, filter) in filters.iter().enumerate() {
        meter.check_cancelled()?;
        if satisfied == Some(position) {
            continue;
        }
        let matches = match filter {
            CompiledFilter::Scalar {
                info,
                predicate,
                posting_membership,
            } => match &ranges[position] {
                ScalarRangeSet::Ids(ids) => ids.binary_search(&id.sequence).is_ok(),
                ScalarRangeSet::Bitmap(bits) => scalar_range_bitmap_contains(bits, id.sequence),
                _ => {
                    if *posting_membership && matches!(predicate, EncodedScalarFilter::Eq(_)) {
                        return Ok(None);
                    }
                    meter.note_row_decode();
                    scalar_filter_matches(
                        info,
                        predicate,
                        selected_field_in(&layout, bytes, &info.field)?,
                        &mut scratch.scalar,
                    )?
                }
            },
            CompiledFilter::JsonEq { field, value } => {
                meter.note_row_decode();
                json_filter_matches(selected_field_in(&layout, bytes, field)?, value)?
            }
            CompiledFilter::Point { info, predicate } => {
                meter.charge(WorkResource::SpatialPostings, 1)?;
                meter.note_row_decode();
                match point_from_field(selected_field_in(&layout, bytes, &info.field)?)? {
                    Some(point) => match predicate {
                        PointFilter::Bbox(bounds) => bounds.contains(point),
                        PointFilter::Radius {
                            center,
                            radius_metres,
                        } => within_radius(*center, point, *radius_metres).map_err(corrupt_query)?,
                    },
                    None => false,
                }
            }
            CompiledFilter::Geometry { info, predicate } => {
                meter.note_row_decode();
                match geom_from_field(selected_field_in(&layout, bytes, &info.field)?)? {
                    Some(geom) => geometry_predicate_matches(predicate, &geom),
                    None => false,
                }
            }
            // Already answered by the position it folded into.
            CompiledFilter::Folded { .. } => true,
            CompiledFilter::Graph { .. } | CompiledFilter::Text(_) | CompiledFilter::Key { .. } => {
                return Ok(None)
            }
        };
        if !matches {
            return Ok(Some(false));
        }
    }
    Ok(Some(true))
}

/// [`ensure_row`] through the page's own primary reader.
fn ensure_row_seq<'a, C: FnMut() -> bool>(
    db: &'a Database,
    rows: &mut PrimaryRows<'a>,
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
        rows.read(id)?
            .ok_or_else(|| corrupt_query("query candidate points to a missing entity"))?
    };
    *row = Some(decode_row(db, bytes)?);
    Ok(())
}

/// One field of one candidate's row.
///
/// The layout is not re-validated here. A `RowData` gets its layout from
/// `Database::layout`, which produces one only through
/// `Layout::from_descriptor` or from a layout this handle itself wrote, and
/// both of those validate -- see [`dense_v3::read_field_in`]. This is the
/// per-candidate path of every non-driving field predicate, so the check was
/// being repaid once per candidate for an answer fixed when the collection was
/// created.
fn selected_field(row: &RowData, field: &str) -> QueryResult<dense_v3::FieldValue> {
    selected_field_in(&row.layout, &row.bytes, field)
}

/// [`selected_field`] for a row whose bytes are BORROWED.
fn selected_field_in(
    layout: &Layout,
    bytes: &[u8],
    field: &str,
) -> QueryResult<dense_v3::FieldValue> {
    // A predicate reads committed, checksum-valid pages: the trusted reader
    // stops at the field it wants and steps over the rest by length. The
    // sacrifice is named on `read_field_in_trusted`.
    dense_v3::read_field_in_trusted(layout, bytes, field)
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
    out: &mut Vec<u8>,
) -> QueryResult<bool> {
    match predicate {
        EncodedScalarFilter::Empty => Ok(false),
        EncodedScalarFilter::IsNull => Ok(matches!(value, dense_v3::FieldValue::Null)),
        EncodedScalarFilter::IsMissing => Ok(matches!(value, dense_v3::FieldValue::Missing)),
        EncodedScalarFilter::Eq(expected) => match value {
            dense_v3::FieldValue::Inline(value) => {
                scalar_key::encode_into(&info.kind, Some(&value), out)
                    .map_err(|error| corrupt_query(format!("indexed scalar row value: {error}")))?;
                Ok(out.as_slice() == expected.as_slice())
            }
            dense_v3::FieldValue::Missing | dense_v3::FieldValue::Null => Ok(false),
            dense_v3::FieldValue::Vector { .. } => {
                Err(corrupt_query("scalar index field is a historical vector"))
            }
        },
        EncodedScalarFilter::Range { .. } => match value {
            dense_v3::FieldValue::Inline(value) => {
                scalar_key::encode_into(&info.kind, Some(&value), out)
                    .map_err(|error| corrupt_query(format!("indexed scalar row value: {error}")))?;
                Ok(scalar_key_position(predicate, out) == Ordering::Equal)
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

/// Walk one non-driving scalar RANGE's postings once and collect every
/// entity SEQUENCE it proves, ascending.
///
/// Same shape as `ScalarCursor::next`'s own forward walk -- open at the
/// predicate's lower bound, stop the first key `scalar_key_position` puts
/// past the upper one -- but with no candidate to build and no direction to
/// choose: this collects the whole range instead of stopping at a page size.
/// The nullish entry a NULL and a MISSING field share is excluded exactly as
/// `proves_predicate` excludes it there, so a set this returns answers a
/// range predicate exactly as `scalar_filter_matches` answers it from the row
/// -- neither ever matches null or missing.
///
/// `Overflow` abandons whatever it collected rather than handing back a
/// partial set: a binary search over less than the whole range would answer
/// "not found" for members the row-read path would have kept.
fn build_scalar_range_set<C: FnMut() -> bool>(
    db: &Database,
    info: &IndexInfo,
    predicate: &EncodedScalarFilter,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<ScalarRangeSet> {
    let prefix = scalar_prefix(info.id);
    let mut start = prefix.clone();
    if let Some(lower) = scalar_lower(predicate) {
        start.extend_from_slice(lower);
    }
    let mut walk = match db.index_range(info, &start).map_err(QueryError::from)? {
        Some(iter) => iter,
        None => return Ok(ScalarRangeSet::Ids(Vec::new())),
    };
    // The collection's span bounds a bitmap without a scan: every sequence a
    // posting in this walk can name is below it. A Vec is kept only while it
    // would stay smaller than that bitmap; once it would not, converting
    // loses nothing (every id collected so far still fits the bitmap by
    // construction) and every posting after that is one bit, not a growing
    // allocation. When the bitmap itself would not fit the budget, `vec_cap`
    // falls back to the plain per-element cap the Vec-only design used, and
    // the walk is abandoned exactly as it was before bitmaps existed.
    let span = db.collection_span(info.collection).map_err(QueryError::from)?;
    let bitmap_bytes = scalar_range_bitmap_bytes(span);
    let bitmap_viable = bitmap_bytes <= SCALAR_RANGE_BITMAP_CAP_BYTES as u64;
    let vec_cap = if bitmap_viable {
        (bitmap_bytes / std::mem::size_of::<u64>() as u64) as usize
    } else {
        SCALAR_RANGE_SET_CAP
    };
    let mut ids: Vec<u64> = Vec::new();
    let mut bits: Option<Vec<u8>> = None;
    loop {
        meter.check_cancelled()?;
        meter.charge(WorkResource::ScalarPostings, 1)?;
        let sequence = {
            let Some((key, value)) = walk.peek_ref().map_err(Error::from).map_err(QueryError::from)?
            else {
                break;
            };
            if !key.starts_with(&prefix) {
                break;
            }
            let suffix = &key[prefix.len()..];
            let value_len = scalar_key::width(&info.kind, suffix)?;
            let encoded = suffix
                .get(..value_len)
                .ok_or_else(|| corrupt_query("truncated scalar value key"))?;
            match scalar_key_position(predicate, encoded) {
                Ordering::Greater => break,
                Ordering::Less => None,
                Ordering::Equal if encoded == NULLISH_SCALAR_KEY => None,
                Ordering::Equal => {
                    let mut at = prefix.len() + value_len;
                    let sequence = read_ordered(key, &mut at)?;
                    if at != key.len() || sequence == 0 || !value.is_empty() {
                        return Err(corrupt_query("scalar index entry"));
                    }
                    Some(sequence)
                }
            }
        };
        walk.step();
        if let Some(sequence) = sequence {
            match bits.as_mut() {
                Some(bits) => scalar_range_bitmap_set(bits, sequence),
                None if ids.len() == vec_cap => {
                    if !bitmap_viable {
                        return Ok(ScalarRangeSet::Overflow);
                    }
                    let mut fresh = vec![0u8; bitmap_bytes as usize];
                    for &s in &ids {
                        scalar_range_bitmap_set(&mut fresh, s);
                    }
                    scalar_range_bitmap_set(&mut fresh, sequence);
                    ids = Vec::new();
                    bits = Some(fresh);
                }
                None => ids.push(sequence),
            }
        }
    }
    if let Some(bits) = bits {
        return Ok(ScalarRangeSet::Bitmap(bits));
    }
    ids.sort_unstable();
    Ok(ScalarRangeSet::Ids(ids))
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

fn geom_from_field(value: dense_v3::FieldValue) -> QueryResult<Option<Geom>> {
    let value = match value {
        dense_v3::FieldValue::Missing | dense_v3::FieldValue::Null => return Ok(None),
        dense_v3::FieldValue::Inline(value) => value,
        dense_v3::FieldValue::Vector { .. } => {
            return Err(corrupt_query("indexed geometry field is a historical vector"));
        }
    };
    super::spatial_geometry_indexes::geom_from_value(&value)
        .map(Some)
        .map_err(QueryError::from)
}

fn text_score<'a, C: FnMut() -> bool>(
    db: &'a Database,
    rows: &mut PrimaryRows<'a>,
    prepared: &PreparedText,
    id: EntityId,
    driven: Option<&TextFrequencies>,
    row: &mut Option<RowData>,
    encoded: &mut Option<Vec<u8>>,
    scratch: &mut RowScratch,
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
        super::text_indexes::read_norm_cached(db, prepared.info.id, id.sequence, &mut scratch.norms)?
            .length
    else {
        return Ok(None);
    };
    // Did the candidate driver already decode these very frequencies? It did
    // whenever the driver is the merge over THIS query's terms: `driven` on
    // the prepared query names that site, and the candidate carries the site
    // it came from. Anything else -- a scalar or spatial driver, or a text
    // driver over different terms -- reads its own, through the per-query
    // term window so that each segment is decoded once rather than once per
    // document.
    let merged = driven.filter(|frequencies| {
        prepared.driven == Some(frequencies.source)
            && usize::from(frequencies.len) == prepared.terms.len()
            && usize::from(frequencies.len) <= INLINE_TEXT_TERMS
    });
    // The page's own buffers, not this document's: a pair of heap vectors per
    // scored document would be one allocation per document per query, and a
    // pair of 64-slot stack arrays -- which is what this was -- is 768 bytes
    // zeroed per document to hold, usually, one term.
    if prepared.terms.len() > MAX_TEXT_TERMS {
        return Err(corrupt_query("prepared text query exceeds its term bound"));
    }
    scratch.text.frequencies.clear();
    scratch.text.idfs.clear();
    let segments_on = super::text_indexes::segments_enabled(db);
    for (position, (term, &idf)) in prepared.terms.iter().zip(&prepared.idfs).enumerate() {
        let frequency = match merged {
            // Already charged to `TextPostings` by the merge that decoded it.
            Some(merged) => {
                let frequency = merged.slots[position];
                (frequency != 0).then_some(frequency)
            }
            None => {
                meter.charge(WorkResource::TextPostings, 1)?;
                super::text_indexes::point_posting(
                    db,
                    prepared.info.id,
                    term,
                    id.sequence,
                    segments_on,
                    &mut scratch.norms,
                )?
            }
        };
        if let Some(frequency) = frequency {
            scratch.text.frequencies.push(frequency);
            scratch.text.idfs.push(idf);
        }
    }
    let frequencies = scratch.text.frequencies.as_slice();
    let idfs = scratch.text.idfs.as_slice();
    let matched = frequencies.len();
    if matched == 0
        || (matches!(prepared.matching, TextMatch::All | TextMatch::Phrase)
            && matched != prepared.terms.len())
    {
        return Ok(None);
    }
    if let Some(phrase) = prepared.phrase.as_deref() {
        // Through the PAGE's reader, not a fresh point-get. The text merge
        // hands documents over in ascending sequence, so the rows a phrase
        // re-reads ascend with it and one forward cursor serves the page: this
        // was 8.0 pager accesses per candidate on `text/match_phrase`, most of
        // them a root-to-leaf descent for a row the cursor was standing near.
        ensure_row_seq(db, rows, id, row, encoded, meter)?;
        let held = row.as_ref().expect("the row was just ensured");
        // Borrowed out of the row, not copied out of it: an owned `String` per
        // candidate was an allocation and a memcpy of the whole field for text
        // that is scanned once and dropped.
        let text = match dense_v3::read_text_field_in(&held.layout, &held.bytes, &prepared.info.field)
            .map_err(|error| corrupt_query(format!("dense-v3 row: {error}")))?
        {
            dense_v3::TextFieldRef::Text(text) => text,
            dense_v3::TextFieldRef::Missing | dense_v3::TextFieldRef::Null => {
                return Err(corrupt_query(
                    "text posting points to absent authoritative primary text",
                ));
            }
            // A text index over a field this layout does not declare as Text:
            // the value is in the extras object, so it is read the general way
            // and the same three cases decide.
            dense_v3::TextFieldRef::Elsewhere => {
                match selected_field(held, &prepared.info.field)? {
                    dense_v3::FieldValue::Inline(Value::String(_)) => {
                        return Err(corrupt_query(
                            "text index field is not a declared text column",
                        ));
                    }
                    dense_v3::FieldValue::Missing | dense_v3::FieldValue::Null => {
                        return Err(corrupt_query(
                            "text posting points to absent authoritative primary text",
                        ));
                    }
                    _ => {
                        return Err(corrupt_query(
                            "text posting points to non-text authoritative primary field",
                        ));
                    }
                }
            }
        };
        let scanned = crate::text_analyzer::scan_phrase_document(
            text,
            phrase,
            &prepared.phrase_prefix,
            &prepared.terms,
            &mut scratch.seen,
            &mut scratch.token,
            |event| match event {
                crate::text_analyzer::PhraseScanEvent::Poll => meter.check_cancelled(),
                crate::text_analyzer::PhraseScanEvent::Token => {
                    meter.charge(WorkResource::TextTokens, 1)
                }
            },
        );
        let (scanned_length, matched) = match scanned {
            Ok(result) => result,
            Err(crate::text_analyzer::PhraseScanError::Analysis(error)) => {
                return Err(corrupt_query(format!(
                    "authoritative text violates analyzer bounds: {error}"
                )));
            }
            Err(crate::text_analyzer::PhraseScanError::Callback(error)) => return Err(error),
        };
        if scanned_length != length {
            return Err(corrupt_query(
                "text norm disagrees with authoritative primary text",
            ));
        }
        // The same cross-check as before, positionally: `frequencies` holds
        // the MATCHED terms in prepared order, and a phrase requires every
        // prepared term to have matched, so the two run in step.
        if frequencies.len() != prepared.terms.len() || scratch.seen.len() != prepared.terms.len() {
            return Err(corrupt_query(
                "text posting frequency disagrees with authoritative primary text",
            ));
        }
        for (at, frequency) in frequencies.iter().enumerate() {
            if scratch.seen[at] != *frequency {
                return Err(corrupt_query(
                    "text posting frequency disagrees with authoritative primary text",
                ));
            }
        }
        if !matched {
            return Ok(None);
        }
    }
    super::text_indexes::bm25_scored(prepared.weights, length, frequencies, idfs)
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
    let locator = candidate.vector(info.id).map(<[u8]>::to_vec);
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
    let value = candidate.quantized(info.id).map(<[u8]>::to_vec);
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

fn filters_match<'a, C: FnMut() -> bool>(
    db: &'a Database,
    rows: &mut PrimaryRows<'a>,
    filters: &[CompiledFilter],
    ranges: &[ScalarRangeSet],
    candidate: &Candidate,
    row: &mut Option<RowData>,
    encoded: &mut Option<Vec<u8>>,
    graph: &[Option<Vec<EntityId>>],
    scratch: &mut RowScratch,
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
            } => match &ranges[position] {
                ScalarRangeSet::Ids(ids) => ids.binary_search(&id.sequence).is_ok(),
                ScalarRangeSet::Bitmap(bits) => scalar_range_bitmap_contains(bits, id.sequence),
                _ => match (
                    *posting_membership,
                    predicate,
                    row.is_none() && encoded.is_none(),
                ) {
                    (true, EncodedScalarFilter::Eq(expected), true) => {
                        scalar_eq_posting_matches(db, info, expected, id, meter)?
                    }
                    _ => {
                        ensure_row_seq(db, rows, id, row, encoded, meter)?;
                        let row = row.as_ref().unwrap();
                        meter.note_row_decode();
                        scalar_filter_matches(
                            info,
                            predicate,
                            selected_field(row, &info.field)?,
                            &mut scratch.scalar,
                        )?
                    }
                },
            },
            CompiledFilter::JsonEq { field, value } => {
                ensure_row_seq(db, rows, id, row, encoded, meter)?;
                let row = row.as_ref().unwrap();
                meter.note_row_decode();
                json_filter_matches(selected_field(row, field)?, value)?
            }
            CompiledFilter::Graph { position, .. } => graph
                .get(*position)
                .and_then(Option::as_ref)
                // Sorted, so membership is a binary search rather than a walk
                // down a tree whose nodes were allocated to answer this.
                .is_some_and(|ids| ids.binary_search(&id).is_ok()),
            CompiledFilter::Point { info, predicate } => {
                let point = if let Some(point) = candidate.point(info.id) {
                    point
                } else {
                    ensure_row_seq(db, rows, id, row, encoded, meter)?;
                    let row = row.as_ref().unwrap();
                    meter.charge(WorkResource::SpatialPostings, 1)?;
                    meter.note_row_decode();
                    let Some(point) = point_from_field(selected_field(row, &info.field)?)? else {
                        return Ok(false);
                    };
                    point
                };
                match predicate {
                    PointFilter::Bbox(bounds) => bounds.contains(point),
                    PointFilter::Radius {
                        center,
                        radius_metres,
                    } => within_radius(*center, point, *radius_metres).map_err(corrupt_query)?,
                }
            }
            CompiledFilter::Geometry { info, predicate } => {
                ensure_row_seq(db, rows, id, row, encoded, meter)?;
                let row = row.as_ref().unwrap();
                meter.note_row_decode();
                let Some(geom) = geom_from_field(selected_field(row, &info.field)?)? else {
                    return Ok(false);
                };
                geometry_predicate_matches(predicate, &geom)
            }
            // Already answered by the position it folded into.
            CompiledFilter::Folded { .. } => true,
            CompiledFilter::Text(prepared) => text_score(
                db,
                rows,
                prepared,
                id,
                candidate.text.as_ref(),
                row,
                encoded,
                scratch,
                meter,
            )?
            .is_some(),
            // Reached only if a candidate arrived here uncertified, which
            // `prepare_query` refuses to compile: a key filter exists only at
            // the position `CandidateDriver::Keys` certifies, and `KeysCursor`
            // never yields an entry outside its own predicate.
            CompiledFilter::Key { .. } => {
                unreachable!("a key filter is always certified by CandidateDriver::Keys")
            }
        };
        if !matches {
            return Ok(false);
        }
    }
    Ok(true)
}

fn rank_candidate<'a, C: FnMut() -> bool>(
    db: &'a Database,
    rows: &mut PrimaryRows<'a>,
    order: &CompiledOrder,
    candidate: &Candidate,
    row: &mut Option<RowData>,
    encoded: &mut Option<Vec<u8>>,
    scratch: &mut RowScratch,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<Option<RankKey>> {
    let value = match order {
        CompiledOrder::EntityId => RankValue::Entity,
        CompiledOrder::Scalar { info, .. } => {
            let key = candidate.scalar(info.id).map(<[u8]>::to_vec);
            let key = match key {
                Some(key) => key,
                None => {
                    ensure_row_seq(db, rows, candidate.id, row, encoded, meter)?;
                    meter.note_row_decode();
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
        CompiledOrder::Distance { info, center } => {
            let distance = if let Some(distance) = candidate.distance_metres(info.id) {
                distance
            } else if let Some(point) = candidate.point(info.id) {
                wgs84_distance_metres(*center, point)
            } else {
                ensure_row_seq(db, rows, candidate.id, row, encoded, meter)?;
                meter.note_row_decode();
                let Some(point) =
                    point_from_field(selected_field(row.as_ref().unwrap(), &info.field)?)?
                else {
                    return Ok(None);
                };
                wgs84_distance_metres(*center, point)
            };
            RankValue::Score(distance.to_bits())
        }
        // Driver order takes the key the driver's own walk is sorted by, and
        // every one of them is already in hand: the candidate's id, the
        // scalar posting's value key, or the spatial posting's cell. Ranking
        // a driver-ordered page reads nothing and allocates nothing.
        CompiledOrder::Driver(DriverKey::Entity) => RankValue::Entity,
        CompiledOrder::Driver(DriverKey::Scalar(index)) => RankValue::Scalar(
            candidate
                .scalar(*index)
                .ok_or_else(|| corrupt_query("driver order lost its scalar posting key"))?
                .to_vec(),
        ),
        CompiledOrder::Driver(DriverKey::Cell(index)) => RankValue::Cell(
            candidate
                .cell(*index)
                .ok_or_else(|| corrupt_query("driver order lost its spatial cell"))?,
        ),
        CompiledOrder::Driver(DriverKey::GeomCell(index)) => {
            let (level, cell) = candidate
                .geom_cell(*index)
                .ok_or_else(|| corrupt_query("driver order lost its geometry cell"))?;
            RankValue::GeomCell { level, cell }
        }
        CompiledOrder::Driver(DriverKey::Key) => RankValue::Key(
            candidate
                .key()
                .ok_or_else(|| corrupt_query("driver order lost its mapping key"))?
                .to_vec(),
        ),
        CompiledOrder::Bm25(prepared) => {
            let Some(score) = text_score(
                db,
                rows,
                prepared,
                candidate.id,
                candidate.text.as_ref(),
                row,
                encoded,
                scratch,
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

/// Materialise every projected field of one winner in ONE dense-v3 walk.
///
/// A row is a single record; the number of columns asked for changes what
/// comes out of the walk, not how many walks there are.
fn project_fields<C: FnMut() -> bool>(
    db: &Database,
    id: EntityId,
    row: &RowData,
    projection: &[String],
    scratch: &mut ProjectionScratch,
    out: &mut Vec<(String, ProjectedValue)>,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<()> {
    if projection.is_empty() {
        return Ok(());
    }
    meter.note_row_decode();
    scratch
        .ensure_plan(&row.layout, projection)
        .map_err(|error| corrupt_query(format!("dense-v3 row: {error}")))?;
    let ProjectionScratch { plan, values } = scratch;
    let plan = &plan.as_ref().expect("the plan was just built").1;
    dense_v3::read_fields(&row.layout, &row.bytes, projection, plan, values)
        .map_err(|error| corrupt_query(format!("dense-v3 row: {error}")))?;
    for (field, value) in projection.iter().zip(values.drain(..)) {
        meter.check_cancelled()?;
        out.push((field.clone(), project_value_or_sidecar(db, id, value, meter)?));
    }
    Ok(())
}

/// The two buffers a projected page reuses across its rows: the ordinal map
/// for the layout it is on, and the vector the decoder fills. Rows of one
/// collection share a layout in the ordinary case, and the map is rebuilt only
/// where they do not.
#[derive(Default)]
struct ProjectionScratch {
    plan: Option<(u64, dense_v3::FieldPlan)>,
    values: Vec<dense_v3::FieldValue>,
}

impl ProjectionScratch {
    fn ensure_plan(
        &mut self,
        layout: &Layout,
        projection: &[String],
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        if !self.plan.as_ref().is_some_and(|(id, _)| *id == layout.id) {
            self.plan = Some((layout.id, dense_v3::FieldPlan::new(layout, projection)?));
        }
        Ok(())
    }
}

/// A projected value, fetching the authoritative vector sidecar when the field
/// is a historical vector.
fn project_value_or_sidecar<C: FnMut() -> bool>(
    db: &Database,
    id: EntityId,
    value: dense_v3::FieldValue,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<ProjectedValue> {
    match value {
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

/// A `Write` that keeps the length and throws the bytes away.
struct ByteCount(u64);

impl std::io::Write for ByteCount {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(buf.len() as u64);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// How many bytes one projected value would serialise to.
///
/// This is charged against the output budget for every projected value of
/// every returned row, and it used to be measured by serialising the value
/// into a fresh `Vec` and reading `len()` -- a heap allocation and a full
/// serialisation per value, whose only product was an integer. A five-column
/// page of 8,192 rows did that 40,960 times and threw all of it away.
fn json_size(value: &Value) -> QueryResult<u64> {
    let mut count = ByteCount(0);
    serde_json::to_writer(&mut count, value).map_err(invalid_query)?;
    Ok(count.0)
}

#[inline]
fn checked_output_size(row: &QueryRow) -> QueryResult<u64> {
    // The commonest row the engine emits: an id-ordered or driver-ordered row
    // with nothing projected. Its size is the 12 bytes of identity plus the
    // one byte every row is charged, and there is no arithmetic that can
    // overflow on the way to saying so.
    if row.projected.is_empty() && matches!(row.order, OrderValue::EntityId | OrderValue::Driver) {
        return Ok(13);
    }
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
        // Neither carries a value the caller is charged for: an id-ordered
        // row's key is its id, and a driver-ordered row's place is the
        // candidate stream's, not a value in the row.
        OrderValue::EntityId | OrderValue::Driver => 0,
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
            ProjectedValue::Value(value) => json_size(value)?,
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
    /// The text merge is the same statement about a different structure: its
    /// term streams ascend by document, so their merge does, and `EntityId`
    /// ranks by exactly that.
    ///
    /// Everything else is excluded on purpose: a descending order walks
    /// against its ranking, a ranked order (BM25, vector distance) has no
    /// relation to any tree's order and must see every candidate before it
    /// knows its top k, and a range or nullish driver under an id order walks
    /// by value while ranking by id.
    fn driver_walks_in_rank_order(&self) -> RankWalk {
        match (&self.driver, &self.order) {
            (DriverPlan::Entities, CompiledOrder::EntityId) => RankWalk::Exact,
            (
                DriverPlan::Scalar {
                    predicate: EncodedScalarFilter::Eq(_),
                    ..
                },
                CompiledOrder::EntityId,
            ) => RankWalk::Exact,
            // The text merge yields documents in strictly ascending
            // sequence -- every term stream ascends and each round takes the
            // smallest or the common head and steps past it, which
            // `TextCursor::next` asserts and refuses to violate -- and an id
            // ranking is that same order. So a text-driven page ranked by id
            // stops on a full heap like any other, and resumes by seeking
            // every term stream to the document the last page ended on
            // (`TermPostings::open_from`). Spatial is deliberately NOT here:
            // its cells are walked in cell order, which is not id order.
            (DriverPlan::Text { .. }, CompiledOrder::EntityId) => RankWalk::Exact,
            (DriverPlan::Nearest { .. }, CompiledOrder::Distance { .. }) => RankWalk::Exact,
            // Driver order IS the driver's walk order -- that is the whole of
            // what it means -- so every driver that has one walks in rank
            // order by construction. The spatial cell walk is the case that
            // exists for: it ascends by `(cell, sequence)` over a sorted,
            // merged range list, which is exactly the rank key
            // `DriverKey::Cell` produces, and `DriverCursor::new` opens it on
            // the posting the last page stopped on. `driver_key` refused the
            // drivers that have no order of their own before this could be
            // asked.
            (
                DriverPlan::Entities
                | DriverPlan::Scalar { .. }
                | DriverPlan::Spatial { .. }
                | DriverPlan::Geometry { .. }
                | DriverPlan::Text { .. }
                | DriverPlan::Graph { .. }
                | DriverPlan::Keys { .. },
                CompiledOrder::Driver(_),
            ) => RankWalk::Exact,
            (
                DriverPlan::Scalar { info, .. },
                CompiledOrder::Scalar {
                    info: order,
                    direction,
                },
            ) if info.id == order.id => match direction {
                SortDirection::Ascending => RankWalk::Exact,
                SortDirection::Descending => RankWalk::ByValue,
            },
            _ => RankWalk::No,
        }
    }

    /// True when the scalar driver has to be walked backwards: the order is
    /// descending and it is that order's own index doing the driving.
    fn scalar_driver_descends(&self) -> bool {
        matches!(self.driver_walks_in_rank_order(), RankWalk::ByValue)
    }

    /// What the page will actually read off each candidate. A driver holding
    /// borrowed bytes copies them only for something that will be read.
    fn cursor_needs(&self) -> CursorNeeds {
        CursorNeeds {
            // The entity cursor's row bytes save `ensure_row` a point-get, but
            // only if something decodes them. With no filters and an id
            // ranking, nothing does.
            // A text filter that is not a phrase, and a BM25 ranking, are
            // answered from the postings and the norm blocks; only a phrase
            // has to re-read the authoritative text. Copying the row for
            // them cost one row per scored candidate for nothing.
            row: self.filters.iter().any(|filter| {
                !matches!(filter, CompiledFilter::Text(prepared) if prepared.phrase.is_none())
            }) || !matches!(
                self.order,
                CompiledOrder::EntityId
                    | CompiledOrder::Bm25(_)
                    | CompiledOrder::Driver(_)
                    | CompiledOrder::Distance { .. }
            ) || self.distance_order_needs_the_row()
                // A projection reads the row as surely as a filter does, and
                // the cursor is standing on it: copying it here costs one
                // allocation, fetching it back costs a whole point-get.
                || !self.projection.is_empty(),
            // The posting's value key is read only by a scalar ranking over
            // the very index that produced it.
            scalar_key: match (&self.driver, &self.order) {
                (DriverPlan::Scalar { info, .. }, CompiledOrder::Scalar { info: order, .. }) => {
                    info.id == order.id
                }
                // A driver-ordered scalar walk ranks by the very key the
                // cursor is standing on.
                (DriverPlan::Scalar { info, .. }, CompiledOrder::Driver(DriverKey::Scalar(order))) => {
                    info.id == *order
                }
                _ => false,
            },
            // The mapping entry's key bytes are read only by a driver-ordered
            // ranking over the keys walk itself.
            key: matches!(self.order, CompiledOrder::Driver(DriverKey::Key)),
        }
    }

    /// Whether a winner's identity is already established without going back
    /// to the primary tree.
    ///
    /// SQLite answers a key-only query out of a covering index and never
    /// touches the row table. The same holds here when the projection is empty
    /// AND the driver's own stream is the authority for the row: the entity
    /// cursor read the primary record itself, and a scalar posting is the
    /// membership record this engine already trusts elsewhere --
    /// `scalar_eq_posting_matches` answers a non-driving equality filter from
    /// the posting alone, without a row.
    ///
    /// A RANGE walk over that same index reads the same records. The entry
    /// `value || sequence` is written and retired by one maintenance path,
    /// whichever predicate later reads it, so trusting it when the predicate
    /// is `price = 490` and distrusting it when the predicate is
    /// `price >= 490 AND price < 500` would be a distinction in the question,
    /// not in the evidence. The one entry a range walk sees that an equality
    /// walk cannot is the nullish key, which null and missing share: that
    /// candidate is never certified, so `filters_match` opens its row and the
    /// predicate rejects it there. No candidate reaches the heap on a nullish
    /// posting, and every one that does came from a real value entry.
    ///
    /// Every other driver stays as it was, with two more exceptions below the
    /// text and spatial ones just described. An order scalar walk, graph ids,
    /// a vector locator can each name a row that is no longer there, and
    /// those still fetch it and still refuse an orphan -- unless the RANKING
    /// already proved the winner present, which is the BM25 case below, or
    /// the DRIVER's own stream already proved it, which is the text and
    /// spatial cases below that.
    ///
    /// Graph ids stay on the probing side even though an edge cannot outlive
    /// its endpoints: `Database::delete` cascades every incident edge
    /// (`cascade_graph_delete`, both primary and reverse markers) before it
    /// touches the row or the other indexes (`collections.rs:1466-1471`), so
    /// an edge the traversal still walks in this snapshot does prove its far
    /// endpoint alive. But `execute_graph` also seeds its result with
    /// `request.seed` unconditionally when `include_seed && min_depth == 0`
    /// (`query.rs`, the `include_seed` branch near the top of
    /// `execute_graph`) -- no edge is walked to reach that entity, so the
    /// traversal proves nothing about it. A caller can name any entity id as
    /// a seed; the probe is the only thing standing between that and an
    /// orphan reaching the page. So graph keeps the probe for every shape,
    /// not just the ones this function already declined to touch.
    ///
    /// Named sacrifice (Law 4): a store-level orphan -- a primary row removed
    /// behind the scalar index's back, which no supported write can do -- is
    /// no longer refused by a key-only page driven by a range filter, exactly
    /// as it has not been refused by one driven by an equality filter.
    /// `verify_index` refuses it outright either way.
    fn winner_needs_no_row(&self) -> bool {
        if !self.projection.is_empty() {
            return false;
        }
        // A BM25 page has no orphan left to refuse. Every candidate it ranks
        // goes through `text_score`, and the FIRST thing `text_score` does is
        // read the document's `0x76` norm: a document that is not in the text
        // index has no length there, the score is `None`, and the candidate is
        // dropped before it can reach the heap.
        //
        // That lookup is a liveness proof because text maintenance retires a
        // document in the SAME transaction as its row. The head tier deletes
        // the norm row outright; the packed tier cannot cut one document out
        // of a `0x7B` block, so the delete writes the EMPTY head value, which
        // overrides the block and decodes as "not in the index"
        // (`text_indexes::apply_transition`, `decode_norm`). Either way a
        // deleted document scores `None`, so a winner of a ranked text page
        // has already been proved present -- and probing the primary tree for
        // it again was a whole root-to-leaf reach, or a cursor step and a key
        // comparison, per RETURNED row.
        //
        // Named sacrifice (Law 4): a store-level orphan -- a primary row
        // removed behind the index's back, which no supported write can do --
        // is no longer refused by a BM25 page; it is refused by the norm, and
        // `verify_index` still refuses it outright. Every other page shape
        // keeps the probe.
        if matches!(self.order, CompiledOrder::Bm25(_)) {
            return true;
        }
        // A text-driven page has no orphan left to refuse either, ranked by
        // BM25 or not. `TermPostings::next` (text_indexes.rs:850 and 863)
        // never emits a posting whose live term frequency is `0` -- that is
        // exactly the tombstone form a delete or an update writes when it
        // retires a posting that a packed segment will not be rewritten to
        // drop, and it is written in the SAME transaction as the row: the
        // text family of `maintain_indexes` (`indexes.rs:973-975`) runs
        // inside the one `Database::delete` closure that goes on to remove
        // the row itself (`collections.rs:1467,1471`), committed or failed as
        // one frame (`collections.rs:1476`). So a document the merge still
        // hands over in this snapshot is a document whose row was alive when
        // the snapshot was taken -- the same guarantee the BM25 case above
        // reaches through the norm, proved one layer down instead, in the
        // posting stream every matching mode (Any, All, Phrase) reads from.
        // A phrase filter already carries the row forward for its own
        // adjacency check, so this arm changes nothing for phrase; it is
        // Any/All, which certify from the posting alone, that stop paying the
        // probe.
        //
        // Named sacrifice (Law 4): a store-level orphan -- a primary row
        // removed behind the text index's back, which no supported write can
        // do -- is no longer refused by a key-only page driven by the text
        // merge, exactly as it is not refused by a BM25 page. `verify_index`
        // refuses it outright either way.
        if matches!(self.driver, DriverPlan::Text { .. }) {
            return true;
        }
        // A key-driven page reads the same guarantee off the mapping entry
        // itself, unconditionally (no order restriction needed, unlike
        // spatial below): `Database::delete` removes a collection's mapping
        // entry (`self.writer()?.delete(&mapping_key(c, key))?`,
        // `collections.rs:1502`) in the SAME closure that removes its row
        // (`collections.rs:1501`), committed or failed as one frame
        // (`collections.rs:1476`, via `self.finish`). So a mapping entry
        // `KeysCursor` still walks in this snapshot names a row that was
        // alive when the snapshot was taken -- there is no "packed tier"
        // complication here the way there is for text: one entry, one key,
        // retired exactly once.
        //
        // Named sacrifice (Law 4): a store-level orphan -- a primary row
        // removed behind the mapping keyspace's back, which no supported
        // write can do -- is no longer refused by a key-only page. No
        // supported write can produce one; `verify_index`-style consistency
        // checking is out of this item's scope.
        if matches!(self.driver, DriverPlan::Keys { .. }) {
            return true;
        }
        // A spatial-driven page reads the same guarantee off the cell
        // posting. `maintain_point` retires a point's old cell posting in the
        // SAME transaction as the row that carried it: on a delete (or a move
        // to a different cell), `db.index_delete(i, &old.key)`
        // (spatial_indexes.rs:194) runs inside the one `Database::delete`
        // closure that also removes the row (`collections.rs:1467,1471`), and
        // the whole closure commits or fails as one frame
        // (`collections.rs:1476`, via `self.finish`). So a cell posting the
        // spatial cursor still walks in this snapshot is a document whose row
        // was alive when the snapshot was taken.
        //
        // That only covers the shapes where nothing ELSE in the page needs
        // the row either: under `QueryOrder::EntityId` or a driver-ordered
        // walk (`DriverKey::Cell`), the cell the cursor is standing on is the
        // whole of the ranking, the same way the entity cursor and the text
        // merge are the whole of theirs. A spatial driver ranked by anything
        // else already reads the row for the ranking (`order_needs_the_row`),
        // and this function's guard for that is unchanged.
        //
        // Named sacrifice (Law 4): a store-level orphan -- a primary row
        // removed behind the spatial index's back, which no supported write
        // can do -- is no longer refused by a bbox/radius page under those
        // two orders. `verify_index` refuses it outright either way.
        if matches!(self.driver, DriverPlan::Nearest { .. })
            && matches!(
                self.order,
                CompiledOrder::Distance { .. }
                    | CompiledOrder::EntityId
                    | CompiledOrder::Driver(_)
            )
        {
            return true;
        }
        if matches!(self.driver, DriverPlan::Spatial { .. })
            && matches!(
                self.order,
                CompiledOrder::EntityId
                    | CompiledOrder::Driver(DriverKey::Cell(_))
                    | CompiledOrder::Distance { .. }
            )
        {
            return true;
        }
        match &self.driver {
            DriverPlan::Entities => true,
            DriverPlan::Scalar {
                predicate: EncodedScalarFilter::Eq(_) | EncodedScalarFilter::Range { .. },
                position: Some(_),
                ..
            } => true,
            // The ORDER index driving the walk is not itself an authority on
            // membership -- a posting can outlive its row and an orphan must
            // still be refused. But when this plan was chosen precisely
            // BECAUSE an equality filter is riding along as a posting
            // membership probe (`order_index_drives_better`), that probe is
            // the same record the equality DRIVER is trusted for one arm up,
            // and a candidate reached the heap only by passing it. Narrow on
            // purpose: every other driver, and this one without such a filter,
            // still goes back for the row.
            DriverPlan::Scalar { position: None, .. } => self.filters.iter().any(|filter| {
                matches!(
                    filter,
                    CompiledFilter::Scalar {
                        posting_membership: true,
                        ..
                    }
                )
            }),
            _ => false,
        }
    }

    /// True when the driver hands candidates over in ascending entity id, so
    /// the rows they ask for are ascending primary keys and one forward cursor
    /// can serve the whole page. An equality posting is `value || sequence` for
    /// one value, so it IS sequence order; a graph result is sorted before it
    /// leaves the traversal; the entity walk is the primary tree. A range or order walk
    /// is in value order and a spatial or vector walk in neither, so those keep
    /// the point-get.
    fn driver_walks_ids_ascending(&self) -> bool {
        matches!(
            self.driver,
            DriverPlan::Entities
                | DriverPlan::Graph { .. }
                // The text merge emits documents in STRICTLY ascending
                // sequence and refuses a posting that does not advance -- it
                // says so, and returns `text merge did not advance` if it ever
                // stops holding. A phrase re-reads the authoritative text of
                // every document it scores, and was paying a root-to-leaf
                // descent for each: 8.0 pager accesses per candidate on
                // `text/match_phrase`.
                | DriverPlan::Text { .. }
                | DriverPlan::Scalar {
                    predicate: EncodedScalarFilter::Eq(_),
                    ..
                }
        )
    }

    /// True when the RANKING, not a filter, has to read the row: a scalar
    /// order whose key the driver's postings do not carry.
    fn order_needs_the_row(&self) -> bool {
        match &self.order {
            CompiledOrder::Scalar { .. } => !self.cursor_needs().scalar_key,
            CompiledOrder::Distance { .. } => self.distance_order_needs_the_row(),
            CompiledOrder::EntityId
            | CompiledOrder::ExactVector { .. }
            | CompiledOrder::ApproximateVector { .. }
            | CompiledOrder::Bm25(_)
            // Every driver-order key is carried by the candidate: its id, the
            // scalar posting's value, or the spatial posting's cell.
            | CompiledOrder::Driver(_) => false,
        }
    }

    /// Distance ranking reads a row only when the driver did not already
    /// hand over that index's point (the nearest walk and a spatial cell
    /// walk both do).
    fn distance_order_needs_the_row(&self) -> bool {
        match (&self.order, &self.driver) {
            (
                CompiledOrder::Distance { info, .. },
                DriverPlan::Nearest { info: driving, .. } | DriverPlan::Spatial { info: driving, .. },
            ) if driving.id == info.id => false,
            (CompiledOrder::Distance { .. }, _) => true,
            _ => false,
        }
    }

    /// True when the page should GATHER its candidates and read their rows in
    /// tree order rather than one at a time in driver order.
    ///
    /// Four things have to hold, and each one is a correctness statement:
    ///
    ///   * the walk has no stop condition (`RankWalk::No`), so a batch can
    ///     never read past the point a stopping walk would have reached. This
    ///     is the whole of the bound rule: where a page CAN stop early, its
    ///     driver is already handing candidates over in rank order and the
    ///     row-reading question is a different one;
    ///   * the driver does not already ascend by id -- when it does, the
    ///     lockstep reader serves the page in one pass without gathering
    ///     anything;
    ///   * something actually reads a row per candidate, or there is nothing
    ///     to gather for;
    ///   * and nothing can REJECT a candidate before that read. A batch reads
    ///     rows before any filter runs, so a filter that answers from a
    ///     posting or from a graph set -- and would have rejected the
    ///     candidate for free -- must not be sitting in front of the one that
    ///     needs the row.
    fn batches_row_reads(&self) -> bool {
        if self.driver_walks_in_rank_order() != RankWalk::No || self.driver_walks_ids_ascending() {
            return false;
        }
        if !(self.walk_reads_every_row() || self.order_needs_the_row()) {
            return false;
        }
        self.filters_are_row_pure()
    }

    /// True when every filter this page still has to evaluate is a pure
    /// function of the row -- no posting probe, no graph set, no text merge --
    /// so the whole decision can be made against BORROWED row bytes and
    /// nothing has to be copied out of the leaf to make it.
    ///
    /// The DRIVING filter is judged like any other. It is usually certified
    /// by the driver and skipped per candidate, but whether it is certified is
    /// a property of the CANDIDATE and this is a property of the plan, so
    /// exempting it here would let a phrase -- whose driver certifies nothing
    /// -- through, and a phrase decided against borrowed bytes alone is no
    /// decision at all.
    fn filters_are_row_pure(&self) -> bool {
        self.filters.iter().all(|filter| match filter {
            CompiledFilter::Scalar {
                posting_membership, ..
            } => !*posting_membership,
            CompiledFilter::JsonEq { .. }
            | CompiledFilter::Point { .. }
            | CompiledFilter::Geometry { .. }
            | CompiledFilter::Folded { .. }
            | CompiledFilter::Key { .. } => true,
            // A text filter rejects from its postings before it looks at a
            // row, so it is a cheap refusal standing in front of the expensive
            // one -- and a PHRASE is not a pure function of the row at all: it
            // needs the merge's frequencies first. A graph filter is a
            // membership test over a set the traversal already built.
            CompiledFilter::Text(_) | CompiledFilter::Graph { .. } => false,
        })
    }

    /// True when EVERY candidate that survives to the heap has already had its
    /// primary record read, so the winner stage's existence re-fetch is asking
    /// a question the walk has answered.
    ///
    /// It is a property of the plan, not of the candidate: a candidate reaches
    /// the heap only by passing every filter, so if any filter is one that has
    /// to read the row, every heap candidate's row was read. The entity cursor
    /// is the other case -- it walks the primary tree itself, so a key it
    /// yielded is a record that is there.
    ///
    /// The filter the DRIVER certifies is excluded: that one is skipped
    /// outright and reads nothing.
    fn walk_reads_every_row(&self) -> bool {
        // The entity cursor walks the primary tree itself, so a key it yielded
        // is a record it has already read.
        matches!(self.driver, DriverPlan::Entities) || self.a_filter_reads_the_row()
    }

    /// The half of [`walk_reads_every_row`] that is about the FILTERS: does
    /// one of them have to go to the primary tree for every candidate?
    ///
    /// The entity driver makes `walk_reads_every_row` true without any filter
    /// asking for a row, which is the right answer to "has this candidate's
    /// record been read" and the wrong one to "is there a read here worth
    /// moving".
    fn a_filter_reads_the_row(&self) -> bool {
        let driving = match &self.driver {
            DriverPlan::Scalar { position, .. }
            | DriverPlan::Text { position, .. }
            | DriverPlan::Keys { position, .. } => *position,
            DriverPlan::Spatial { position, .. } | DriverPlan::Graph { position } => Some(*position),
            DriverPlan::Nearest { certifies, .. } => *certifies,
            DriverPlan::Spatial { position, .. }
            | DriverPlan::Geometry { position, .. }
            | DriverPlan::Graph { position } => Some(*position),
            _ => None,
        };
        self.filters.iter().enumerate().any(|(position, filter)| match filter {
            // A phrase is the one filter the DRIVER does not certify: its
            // postings establish all-term candidacy and the ordered adjacency
            // is settled against the authoritative primary text. So every
            // candidate that passes it -- driving or not -- has had its row
            // read, and the winner stage's re-fetch is asking a question this
            // walk has answered.
            CompiledFilter::Text(prepared) => prepared.phrase.is_some(),
            // A geometry posting's BoxF is only a candidate test. Driving or
            // not, the row's geometry is refined through spatial_geometry;
            // T3's no-row rule does not apply.
            CompiledFilter::Geometry { .. } => true,
            _ if Some(position) == driving => false,
            // A non-driving equality answered from its posting reads no row,
            // and neither does a non-driving RANGE once its own posting walk
            // has been collected into a set (`ScalarRangeSet::Ids` or
            // `ScalarRangeSet::Bitmap`); every other scalar predicate does.
            CompiledFilter::Scalar {
                posting_membership, ..
            } => {
                !*posting_membership
                    && !matches!(
                        self.scalar_ranges[position],
                        ScalarRangeSet::Ids(_) | ScalarRangeSet::Bitmap(_)
                    )
            }
            CompiledFilter::JsonEq { .. } | CompiledFilter::Point { .. } => true,
            CompiledFilter::Graph { .. } | CompiledFilter::Folded { .. } => false,
            // Certified straight from the mapping entry, at the driving
            // position `_ if Some(position) == driving` already caught above;
            // reached only if it were somehow not driving, which
            // `prepare_query` refuses to compile.
            CompiledFilter::Key { .. } => false,
        })
    }

    /// True when this page should HOLD BACK the rows it ranked but could not
    /// return, instead of dropping them and walking for them again.
    ///
    /// Three things have to hold, and each one is a correctness statement:
    ///
    ///   * the walk is not in rank order (`RankWalk::No`), so it has neither a
    ///     stop condition nor a resume: it reads the whole candidate stream
    ///     whatever it does, and everything past this page is something it has
    ///     already ranked. A walk that CAN resume holds nothing, exactly as
    ///     before -- it never ranked those rows in the first place;
    ///   * the page projects no fields, so what is held is a rank key and
    ///     nothing else. A projected page can carry a whole primary record per
    ///     entry, and a bound in rows would not be a bound in bytes. Named
    ///     sacrifice (Law 4): a PROJECTED answer over a value-ordered walk
    ///     still re-walks its driver once per page;
    ///   * the order is not the approximate-vector one, whose `ef` already
    ///     bounds the entire result set to one shortlist, and whose page
    ///     reports approximation diagnostics that a held row does not carry.
    fn keeps_a_run(&self) -> bool {
        self.projection.is_empty()
            && self.driver_walks_in_rank_order() == RankWalk::No
            && !matches!(self.order, CompiledOrder::ApproximateVector { .. })
    }

    /// True when this page hands its winners back in ascending entity id, so
    /// the rows they still owe can be lifted out by one forward cursor rather
    /// than a root-to-leaf descent each.
    ///
    /// An id ranking is the obvious one. Driver order is the other: over the
    /// entity cursor, the text merge or a traversal its key IS the id, and
    /// the page is as ascending as an id-ranked one. Over a scalar or spatial
    /// walk it is not, and those pages lift their rows out in the tree's
    /// order first, as every other ranked page does.
    fn winners_ascend_by_id(&self) -> bool {
        matches!(
            self.order,
            CompiledOrder::EntityId | CompiledOrder::Driver(DriverKey::Entity)
        )
    }

    /// The bookkeeping every page ends with, however its winners were found:
    /// where the next page resumes, how many rows the query has emitted, and
    /// whether there is anything left.
    fn finish_page(
        &mut self,
        rows: Vec<QueryRow>,
        winners: &[HeapEntry],
        has_more: bool,
        approximation: Option<ApproximationDiagnostics>,
        work: QueryWork,
    ) -> QueryResult<QueryPage> {
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
            work,
            approximation,
        })
    }

    /// Turn a page's ranked winners into its rows: the existence proof a
    /// driver that is not its own authority still owes, and the projected
    /// fields. Shared by the page that walked for these winners and the page
    /// that took them out of the held run.
    fn emit_rows<C: FnMut() -> bool>(
        &self,
        winners: &mut [HeapEntry],
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Vec<QueryRow>> {
        let db = self.db;
        // Whether the page will project anything out of the rows it holds.
        let wants_rows = !self.projection.is_empty();
        let winner_needs_no_row = self.winner_needs_no_row();
        let walk_reads_every_row = self.walk_reads_every_row();
        let mut scratch = ProjectionScratch::default();
        // An id ranking returns winners in ascending primary-key order, so the
        // rows they still need can be lifted out by one forward cursor. Any
        // other ranking hands them over in an order the primary tree knows
        // nothing about, and each one is a fresh descent as before.
        let mut winner_rows = PrimaryRows::new(db, self.winners_ascend_by_id());
        // A RANKED page hands its winners over in score order; the primary
        // tree is in id order. Reading them as they are ranked descends from
        // the root once per returned row -- a BM25 page over 900 matching
        // documents paid 900 descents and 900 buffers, where the very same
        // page ranked by id paid one cursor. So a ranked page lifts its rows
        // out FIRST, in the tree's order, and hands them back to the ranked
        // winners by index. Nothing about the answer or its order changes,
        // only the order the rows are read in; an id ranking already ascends
        // and keeps streaming them one at a time, holding none.
        let ranked_rows_read =
            !winner_needs_no_row && !walk_reads_every_row && !self.winners_ascend_by_id();
        if ranked_rows_read {
            let mut ascending = PrimaryRows::new(db, true);
            if wants_rows {
                let mut by_id: Vec<usize> = (0..winners.len())
                    .filter(|at| winners[*at].row.is_none())
                    .collect();
                by_id.sort_unstable_by_key(|at| winners[*at].key.id);
                for at in by_id {
                    meter.charge(WorkResource::PrimaryReads, 1)?;
                    let bytes = ascending
                        .read(winners[at].key.id)?
                        .ok_or_else(|| corrupt_query("query winner is missing its entity"))?;
                    winners[at].row = Some(Box::new(decode_row(db, bytes)?));
                }
            } else {
                // A key-only page wanted this read for one thing: the proof
                // that the entity is still there. It keeps nothing, so it
                // sorts the SEQUENCES and not indices into the winners --
                // every comparison of `sort_unstable_by_key(|at|
                // winners[*at]...)` is an indirect load into a
                // 56-byte-per-entry array, and a 3,716-document BM25 page
                // makes about 44,000 of them. The collection is the same for
                // every candidate (the walk refuses one that crosses), so what
                // is sorted is one `u64` each.
                let mut sequences: Vec<u64> =
                    winners.iter().map(|winner| winner.key.id.sequence).collect();
                sequences.sort_unstable();
                for sequence in sequences {
                    meter.charge(WorkResource::PrimaryReads, 1)?;
                    if !ascending.exists(EntityId {
                        collection: self.collection,
                        sequence,
                    })? {
                        return Err(corrupt_query("query winner is missing its entity"));
                    }
                }
            }
        }
        let mut rows = Vec::with_capacity(winners.len());
        for winner in winners.iter_mut() {
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
                (CompiledOrder::Distance { .. }, RankValue::Score(score)) => {
                    OrderValue::Distance(f64::from_bits(*score))
                }
                // The driver's key is the walk's own bookkeeping, not an
                // answer about the row: a cell number is not a distance and a
                // sequence is already `id`.
                (CompiledOrder::Driver(_), _) => OrderValue::Driver,
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
            // The row the candidate walk already had, if it kept one.
            let carried = winner.row.take();
            if carried.is_none() && !winner_needs_no_row && !walk_reads_every_row && !ranked_rows_read
            {
                meter.charge(WorkResource::PrimaryReads, 1)?;
                if self.projection.is_empty() {
                    // Nothing is decoded from these bytes -- the read is here
                    // to refuse an orphan -- so do not copy them out of the
                    // leaf.
                    if !winner_rows.exists(winner.key.id)? {
                        return Err(corrupt_query("query winner is missing its entity"));
                    }
                } else {
                    let bytes = winner_rows
                        .read(winner.key.id)?
                        .ok_or_else(|| corrupt_query("query winner is missing its entity"))?;
                    let row = decode_row(self.db, bytes)?;
                    project_fields(
                        self.db,
                        winner.key.id,
                        &row,
                        &self.projection,
                        &mut scratch,
                        &mut projected,
                        meter,
                    )?;
                }
            } else if let Some(row) = carried {
                project_fields(
                    self.db,
                    winner.key.id,
                    &row,
                    &self.projection,
                    &mut scratch,
                    &mut projected,
                    meter,
                )?;
            }
            let row = QueryRow {
                id: winner.key.id,
                order,
                projected,
            };
            meter.charge(WorkResource::OutputBytes, checked_output_size(&row)?)?;
            rows.push(row);
        }
        Ok(rows)
    }

    /// Walk every not-yet-built [`ScalarRangeSet`] once, so this call's
    /// `work.scalar_postings` pays for it and every later page -- including a
    /// resumed one -- finds it already there.
    ///
    /// Idempotent: a position the loop has already resolved, in this call or
    /// an earlier one, is `Ineligible`, `Overflow`, or `Ids` and is skipped.
    fn ensure_scalar_range_sets<C: FnMut() -> bool>(
        &mut self,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<()> {
        for position in 0..self.filters.len() {
            if !matches!(self.scalar_ranges[position], ScalarRangeSet::Unbuilt) {
                continue;
            }
            let (info, predicate) = match &self.filters[position] {
                CompiledFilter::Scalar { info, predicate, .. } => (info.clone(), predicate.clone()),
                _ => unreachable!("only a scalar filter's position is marked Unbuilt"),
            };
            self.scalar_ranges[position] = match build_scalar_range_set(self.db, &info, &predicate, meter)
            {
                Ok(set) => set,
                // The row-read path this filter used before this walk existed
                // never spent `ScalarPostings` -- only `PrimaryReads` -- so a
                // caller whose budget cannot afford even one posting of this
                // walk must get exactly that path back, not a new way for the
                // same query to fail. Postings already charged before this
                // one stay charged; nothing here refunds real reads.
                Err(QueryError::BudgetExceeded {
                    resource: WorkResource::ScalarPostings,
                    ..
                }) => ScalarRangeSet::Overflow,
                Err(other) => return Err(other),
            };
        }
        Ok(())
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
        // How many ranked rows this page will hold on to. A page that can
        // resume holds exactly what it returns; one that cannot holds a whole
        // run, because the rows past this page are rows it is about to rank
        // anyway and dropping them is what makes the next page walk again.
        let held = if self.keeps_a_run() {
            capacity.max(RUN_ROWS.min(remaining))
        } else {
            capacity
        };
        let descending = matches!(
            self.order,
            CompiledOrder::Scalar {
                direction: SortDirection::Descending,
                ..
            } | CompiledOrder::Bm25(_)
        );
        let mut meter = WorkMeter::new(budget, &mut cancelled);
        meter.check_cancelled()?;
        self.ensure_scalar_range_sets(&mut meter)?;
        // Rows an earlier page's walk already ranked and could not return.
        // They are in rank order, last first, so this takes the next ones off
        // the end -- and the whole walk is skipped, which is the point.
        if !self.run.is_empty() {
            let take = wanted.min(self.run.len());
            let mut winners = Vec::with_capacity(take);
            for _ in 0..take {
                winners.push(self.run.pop().expect("the run was just measured"));
            }
            let has_more = !self.run.is_empty() || self.run_bounded;
            let rows = self.emit_rows(&mut winners, &mut meter)?;
            let work = meter.used;
            return self.finish_page(rows, &winners, has_more, None, work);
        }
        // One decoded `0x7B` norm block held for the page. Candidates that
        // arrive in ascending sequence -- the text and entity cursors -- reuse
        // it 255 times out of 256; one that does not simply re-decodes.
        let mut scratch = RowScratch::default();
        let graph = execute_graph_filters(self.db, &self.filters, &mut meter)?;
        let in_rank_order = self.driver_walks_in_rank_order();
        let needs = self.cursor_needs();
        let reverse = self.scalar_driver_descends();
        let resume = self
            .after
            .clone()
            .filter(|_| in_rank_order != RankWalk::No);
        let mut nearest_walk = self.nearest.take();
        let result = (|| {
        let geometry_seen = if in_rank_order == RankWalk::Exact {
            self.geometry_seen.clone()
        } else {
            HashSet::new()
        };
        let mut driver = DriverCursor::new(
            self.db,
            self.collection,
            &self.driver,
            &graph,
            needs,
            resume.as_ref(),
            reverse,
            nearest_walk.as_mut(),
            geometry_seen,
        )?;
        // Whether a kept candidate should carry its row into the heap.
        let wants_rows = !self.projection.is_empty();
        let db = self.db;
        let mut rows = PrimaryRows::new(db, self.driver_walks_ids_ascending());
        let mut winners = Winners::new();
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
                        db,
                        &mut rows,
                        &self.filters,
                        &self.scalar_ranges,
                        &candidate,
                        &mut row,
                        &mut encoded,
                        &graph,
                        &mut scratch,
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
                        row: None,
                    };
                    if winners.len() < capacity {
                        winners.push(capacity, entry);
                    } else if winners
                        .worst()
                        .is_some_and(|worst| entry.cmp(worst) == Ordering::Less)
                    {
                        winners.pop_worst();
                        winners.push(capacity, entry);
                    }
                }
                Some(ApproximationDiagnostics {
                    method: ApproxVectorMethod::SymmetricInt8ScanV1,
                    ef: *ef,
                    examined,
                    reranked,
                })
            } else {
                // Does this page gather its candidates before reading their
                // rows? THE BOUND RULE, stated once: a batch is taken only
                // where the walk has no stop condition at all, and it is never
                // larger than the page could return. So the rows a batch reads
                // are rows the row-by-row walk would have read too -- the same
                // set, in the primary tree's order instead of the driver's.
                let batched = self.batches_row_reads();
                let keep_batch_rows = wants_rows || self.order_needs_the_row();
                // A page whose driver ALREADY ascends gathers nothing -- one
                // forward cursor serves it in a single pass -- but it was
                // still copying each row out of the leaf to look at one field
                // of it and then dropping the copy: the last allocation per
                // candidate on `filter/and_half_indexed`. It can borrow
                // instead, on exactly the terms a batch can.
                let borrowed = !batched
                    && !keep_batch_rows
                    && self.a_filter_reads_the_row()
                    && self.filters_are_row_pure();
                let batch_bound = capacity.min(ROW_BATCH);
                let mut batch: Vec<Candidate> = Vec::new();
                let mut batch_order: Vec<(u64, u32)> = Vec::new();
                'walk: loop {
                    let mut candidate = if batched {
                        if batch.is_empty() {
                            while batch.len() < batch_bound {
                                let Some(candidate) = driver.next(&mut meter)? else {
                                    break;
                                };
                                meter.charge(WorkResource::Candidates, 1)?;
                                if candidate.id.collection != self.collection {
                                    return Err(corrupt_query(
                                        "query driver crossed collection boundary",
                                    ));
                                }
                                batch.push(candidate);
                            }
                            if batch.is_empty() {
                                break 'walk;
                            }
                            read_batch_rows(
                                db,
                                &mut rows,
                                &self.filters,
                                &self.scalar_ranges,
                                keep_batch_rows,
                                &mut batch,
                                &mut batch_order,
                                &mut scratch,
                                &mut meter,
                            )?;
                            // `pop` takes from the end, so reversing hands the
                            // candidates back in the driver's own order: the
                            // rows were read in another order, nothing else
                            // was.
                            batch.reverse();
                        }
                        batch.pop().expect("the batch was just filled")
                    } else {
                        let Some(candidate) = driver.next(&mut meter)? else {
                            break 'walk;
                        };
                        meter.charge(WorkResource::Candidates, 1)?;
                        if candidate.id.collection != self.collection {
                            return Err(corrupt_query("query driver crossed collection boundary"));
                        }
                        candidate
                    };
                    let mut encoded = candidate.row.take();
                    let mut row = None;
                    if borrowed && encoded.is_none() && candidate.row_filtered.is_none() {
                        meter.charge(WorkResource::PrimaryReads, 1)?;
                        let id = candidate.id;
                        let satisfied = candidate.satisfied_filter;
                        let filters = &self.filters;
                        let ranges = &self.scalar_ranges;
                        let scratch = &mut scratch;
                        let meter = &mut meter;
                        candidate.row_filtered = rows.with_row(id, |bytes| match bytes {
                            Some(bytes) => batch_filters_match(
                                db, filters, ranges, satisfied, id, bytes, scratch, meter,
                            ),
                            None => Err(corrupt_query(
                                "query candidate points to a missing entity",
                            )),
                        })?;
                    }
                    match candidate.row_filtered {
                        // The batched pass read this candidate's row and ran
                        // every filter against it; there is nothing here to
                        // repeat.
                        Some(true) => {}
                        Some(false) => continue,
                        // `filters_match` over an empty slice can only say
                        // yes, and saying it costs a nine-argument call per
                        // row: 5.6% of a key-only enumeration that has no
                        // filters to evaluate at all.
                        None if self.filters.is_empty() => {}
                        None => {
                            if !filters_match(
                                db,
                                &mut rows,
                                &self.filters,
                                &self.scalar_ranges,
                                &candidate,
                                &mut row,
                                &mut encoded,
                                &graph,
                                &mut scratch,
                                &mut meter,
                            )? {
                                continue;
                            }
                        }
                    }
                    let Some(key) = rank_candidate(
                        db,
                        &mut rows,
                        &self.order,
                        &candidate,
                        &mut row,
                        &mut encoded,
                        &mut scratch,
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
                    // A walk that is only VALUE-monotone (a descending scalar
                    // order, whose reverse walk hands each tie group over id
                    // descending while the rank wants id ascending) cannot
                    // stop on a full heap: the rest of the boundary value's
                    // tie group still outranks what is held. It CAN stop the
                    // moment a candidate's value falls strictly past the worst
                    // held one, because the walk never comes back up.
                    if in_rank_order == RankWalk::ByValue
                        && winners.len() >= capacity
                        && winners.worst().is_some_and(|worst| {
                            compare_rank_value(&key.value, &worst.key.value, descending)
                                == Ordering::Greater
                        })
                    {
                        break;
                    }
                    let mut entry = HeapEntry {
                        key,
                        descending,
                        row: None,
                    };
                    // The bytes this candidate's row was read from are still
                    // in hand -- the entity cursor copied them out of the leaf
                    // it was standing on, or a filter fetched them. Hand them
                    // to the heap ONLY if the page will project something and
                    // ONLY if the entry is being kept, so a losing candidate
                    // costs nothing and a key-only page carries nothing.
                    let keep = |entry: &mut HeapEntry,
                                    row: &mut Option<RowData>,
                                    encoded: &mut Option<Vec<u8>>|
                     -> QueryResult<()> {
                        if wants_rows {
                            entry.row = match row.take() {
                                Some(row) => Some(Box::new(row)),
                                None => match encoded.take() {
                                    Some(bytes) => Some(Box::new(decode_row(self.db, bytes)?)),
                                    None => None,
                                },
                            };
                        }
                        Ok(())
                    };
                    // `held` is the page's own hold, which is the page size
                    // unless the walk cannot resume -- then it is a whole run,
                    // and the entries past this page are kept for the pages
                    // after it instead of being walked for again. `capacity`
                    // stays the RESERVE: a ten-row answer must not reserve a
                    // run's worth of entries to hold ten.
                    if winners.len() < held {
                        keep(&mut entry, &mut row, &mut encoded)?;
                        winners.push(capacity, entry);
                    } else if winners
                        .worst()
                        .is_some_and(|worst| entry.cmp(worst) == Ordering::Less)
                    {
                        winners.pop_worst();
                        keep(&mut entry, &mut row, &mut encoded)?;
                        winners.push(capacity, entry);
                    }
                    // The page is full and the walk is already in rank order,
                    // so every candidate still ahead ranks after everything
                    // held. Without this, `LIMIT 10` reads the whole
                    // collection to answer with ten rows, and a page of a scan
                    // reads to the end of the collection to fill 8,192 rows.
                    if in_rank_order == RankWalk::Exact && winners.len() >= capacity {
                        break;
                    }
                }
                None
            };

        let geometry_driven = in_rank_order == RankWalk::Exact
            && matches!(driver, DriverCursor::Geometry(_));
        if let (true, DriverCursor::Geometry(cursor)) = (geometry_driven, &driver) {
            self.geometry_seen.clone_from(&cursor.seen);
        }

        // A page that never had to name its worst entry is still in the order
        // the walk handed it over, and an EXACT walk hands it over in rank
        // order. That page is already sorted and sorting it again is 8,192
        // comparisons over 459 KB for an answer that cannot change.
        let ordered = in_rank_order == RankWalk::Exact && !winners.heaped();
        let mut winners = winners.into_vec();
        if !ordered {
            // A rank key ends in the entity id, so no two entries compare
            // equal and a stable sort is ordering something that cannot be
            // observed -- while allocating a scratch buffer the size of the
            // page to do it.
            winners.sort_unstable_by(|left, right| compare_rank(&left.key, &right.key, descending));
        }
        debug_assert!(
            winners
                .windows(2)
                .all(|pair| compare_rank(&pair[0].key, &pair[1].key, descending)
                    != Ordering::Greater),
            "a page returns its winners in rank order"
        );
        let has_more = winners.len() > wanted;
        // A geometry-driven page kept one candidate past what it returns, to
        // learn whether more exist. The next page re-opens at the last
        // RETURNED posting and must be allowed to admit that extra entity
        // again, so it must not count as seen.
        if geometry_driven {
            for extra in winners.iter().skip(wanted) {
                self.geometry_seen.remove(&extra.key.id.sequence);
            }
        }
        // Everything this walk ranked past the page it is returning. It was
        // ranked; the pages after this one take it from here rather than
        // opening the whole candidate stream again. `run_bounded` records
        // whether the walk filled the hold -- if it did, emptying the run is
        // not the end of the answer and a later page walks once more, from the
        // last row handed out.
        if winners.len() > wanted && self.keeps_a_run() {
            self.run_bounded = winners.len() == held;
            self.run = winners.split_off(wanted);
            self.run.reverse();
        }
        winners.truncate(wanted);
        let rows = self.emit_rows(&mut winners, &mut meter)?;
        let work = meter.used;
        self.finish_page(rows, &winners, has_more, approximation, work)
        })();
        self.nearest = nearest_walk;
        result
    }
}
