# Phase 2 combined embedded query design

Status: first executable combined slice, 2026-09-17 (superseded in part by
2026-09-20 HEAD; see the update below). `src/query/` (the layout restructure
at `f5e4c7e` split the former `src/query.rs` into this directory) implements
the API below over the current scalar, graph, exact-vector, quantized-vector,
point-spatial and text families. Point, text, exact-vector and quantized-vector
orders have complete native drivers; callers may retain the entity stream as
an explicit semantic cross-check. Point-distance ordering, extra score
columns, OR, NOT, joins and a SQL adapter remain unsupported work.

STATUS UPDATE (2026-09-20): point-distance ordering (`CompiledOrder::Distance`)
and score expressions (`ScoreExpr`, `src/query/mod.rs`, `src/query/score.rs`)
are both live T1 atomics now (`docs/lang/QL_CONTRACT.md` §4.4, §4.7). A Tier-1 SQL
parser and compiler exist (`lang/src/`). OR, NOT and joins remain unsupported
(`docs/lang/QL_CONTRACT.md` §7, order-of-work items 3 and 7).

## Contract and public shape

A combined query reads one immutable `Database` view. Callers that need a
committed cross-call view open it once with `Database::open_snapshot` and build
all prepared queries from that handle. The executor never opens a second
snapshot and never calls a path-based family API internally. An immutable
borrow also keeps a writer's current view stable for the duration of one call,
but pagination intended to survive other writes uses a read-only snapshot.

The first request surface is typed and AND-only. OR, NOT and joins remain
explicit unsupported cases until their duplicate and ordering rules are
specified; score expressions (`ScoreExpr`) are no longer one of them -- see
the status update above.

```rust
pub struct QueryRequest<'a> {
    pub collection: CollectionId,
    pub filters: &'a [QueryFilter<'a>],       // all must match
    pub order: QueryOrder<'a>,
    pub projection: Projection<'a>,
    pub total_limit: Option<usize>,
    pub driver: CandidateDriver,              // Auto or an exact clause hint
}

pub enum QueryFilter<'a> {
    Scalar { index: IndexId, predicate: ScalarFilter<'a> },
    JsonEq { field: &'a str, value: &'a Value },
    Graph(BfsRequest),
    Point { index: IndexId, predicate: PointFilter },
    Text { index: IndexId, query: &'a str, matching: TextMatch },
}

pub enum QueryOrder<'a> {
    EntityId,
    Scalar { index: IndexId, direction: SortDirection },
    ExactVector { index: IndexId, query: &'a [f32], metric: VectorMetric },
    ApproximateVector { index: IndexId, query: &'a [f32],
                        metric: VectorMetric, ef: usize },
    Bm25 { index: IndexId, query: &'a str, matching: TextMatch },
    Driver,                                   // the driver's own walk order
}

pub enum CandidateDriver { Auto, Entities, Filter(usize), Order }
pub enum QueryDriver {
    Entities, Scalar(IndexId), Graph { filter: usize },
    Spatial { index: IndexId, fallback_world: bool },
    Text(IndexId), ExactVector(IndexId), QuantizedVector(IndexId),
}
pub enum SortDirection { Ascending, Descending }
pub enum TextMatch { Any, All }
pub enum PointFilter { Bbox(Bounds), Radius { center: Point, radius_metres: f64 } }

pub enum Projection<'a> {
    Ids,
    Fields(&'a [&'a str]),
}

pub enum OrderValue {
    EntityId,
    Scalar(OwnedScalarValue),
    Distance(f64),
    Bm25(f64),
}

pub struct QueryRow {
    pub id: EntityId,
    pub order: OrderValue,
    pub projected: Vec<(String, ProjectedValue)>,
}

pub struct QueryBudget {
    pub candidates: u64,
    pub primary_reads: u64,
    pub scalar_postings: u64,
    pub graph_edges: u64,
    pub graph_visited: u64,
    pub spatial_postings: u64,
    pub text_postings: u64,
    pub vector_locators: u64,
    pub vector_sidecars: u64,
    pub vector_lanes: u64,
    pub output_bytes: u64,
}

pub struct QueryPage {
    pub rows: Vec<QueryRow>,
    pub done: bool,
    pub driver: QueryDriver,
    pub work: QueryWork,
    pub approximation: Option<ApproximationDiagnostics>,
}

pub struct ApproximationDiagnostics {
    pub method: ApproxVectorMethod, // SymmetricInt8ScanV1
    pub ef: usize,
    pub examined: usize,            // eligible after every filter
    pub reranked: usize,            // authoritative f32 reranks
}

let mut query = snapshot.prepare_query(request)?;
let page = query.next_page(page_size, budget, || cancelled())?;
```

`prepare_query` validates that every referenced index is READY, belongs to the
output collection where applicable, has the requested family and field kind,
and that the clause count, query-term count, vector dimension and page/total
limits are supported. A graph seed may belong to another collection; graph
results are intersected with `request.collection`.

`PreparedQuery` borrows the same `Database` for its lifetime and owns its
pagination position. This makes a cursor impossible to apply accidentally to
another snapshot or request. A successful page advances the position to the
last emitted `(sort key, EntityId)`. Cancellation, corruption or budget
exhaustion returns an error, no page, and leaves the position unchanged so the
caller can retry. A serializable resume token can be added only after it binds
the request fingerprint and committed snapshot generation; an ID-only token is
not sufficient for ranked results.

For ranked pagination, every page scans the complete eligible stream and keeps
the best `page_size + 1` rows strictly after the prior sort key. The extra row
determines `done`. This is exact and bounded but later pages repeat work. Family
search-after cursors can optimize it later without changing semantics. The
sort keys are:

- entity order: `EntityId` ascending;
- scalar order: the scalar family's canonical encoded value, then `EntityId`;
- exact-vector distance: `f64::total_cmp` ascending, then `EntityId`;
- approximate-vector authoritative rerank distance: `f64::total_cmp`
  ascending, then `EntityId`;
- BM25: score descending, then `EntityId` ascending;
- driver order: the key the chosen driver's own walk is sorted by, then
  `EntityId` -- the entity id for the primary cursor, the text merge and a
  graph result, the posting's encoded value for a scalar index, and the
  Hilbert cell for a spatial index.

`Driver` asks for the rows in the order the candidate driver yields them,
with no re-ranking: pages are disjoint and complete, a `total_limit` stops
the walk, and the continuation seeds the driver at its own cursor position.
It is what SQLite returns for a bare index scan or an R*Tree join, which
sorts nothing either, and it is the only order in which a SPATIAL answer can
be paged -- cells are neither id order nor value order, so a spatial page
ranked by anything else must see every candidate in the envelope before it
knows its first row, and the page after it must see them all again. The
order is stable for a fixed driver, which a prepared query has (the driver is
chosen once, at prepare time); it is not a promise across two separately
prepared queries that `CandidateDriver::Auto` may plan differently. A driver
with no walk order of its own -- the approximate-vector shortlist, whose
entries are in locator order and whose answer is in distance order -- is
refused with `InvalidInput`.

`ApproximateVector` scans compact symmetric-int8 entries and retains at most
`ef` approximate candidates only after every graph/scalar/spatial/text/JSON
filter has passed. It then reloads and validates the existing authoritative
f32 sidecars, re-encodes the compact entry to detect stale derived data, and
reranks the shortlist with exact family math. Pagination rescans the same
snapshot and same `ef` shortlist, then applies exact-distance search-after;
there are at most `ef` results across all pages. Distances are exact for the
chosen shortlist, while recall remains explicitly approximate. This compact
linear scan is not described as HNSW, graph ANN or sublinear search.

The page heap, projection output and request terms are explicitly bounded.
Only graph traversal may retain more than page-sized candidate state: its
deduplication set is bounded by the caller's `BfsRequest::max_visited`. The
`graph_visited` budget separately meters each retained ID. Crossing
that allowance is an error. No family silently cuts its candidates at 65,536.

## Scalar semantics

The combined API does not take untyped `serde_json::Value` bounds. It uses:

```rust
pub enum ScalarValue<'a> { Bool(bool), I64(i64), F64(f64), Text(&'a str) }
pub enum ScalarFilter<'a> {
    Eq(ScalarValue<'a>),
    Range { lower: Bound<ScalarValue<'a>>,
            upper: Bound<ScalarValue<'a>> },
    IsNull,
    IsMissing,
}
```

`I64` is valid only for `Kind::Int`; comparison and ordering use the existing
sign-bit-flipped i64 encoding. `F64` is valid only for `Kind::Real`, must be
finite, and uses the existing IEEE sortable transform. The executor never
converts an i64 bound through f64, so values around `2^53`, `i64::MIN` and
`i64::MAX` retain exact ordering. Bool and text never coerce. Mixed kinds and an
integer bound against a Real index are refused. Persisted Real values are the
declared f64 domain.

The scalar posting's `00` bucket remains only a coarse driver for null/missing.
`IsNull` and `IsMissing` inspect the primary dense-v3 state for each candidate
before acceptance. `Eq` does not accept null. This prevents the low-level
`query_scalar(Eq(null))` bucket from leaking into the combined semantics.
Structured JSON equality has no scalar index in this phase; it needs an
explicit primary-field predicate and bounded entity scan. The final acceptance
surface must implement structural JSON equality through `dense_v3::read_field`;
an incremental build may return `Unsupported` only until that accessor lands.

## Execution pipeline

Preparation compiles every clause into one of three roles:

1. one **driver** yields unique `EntityId`s without materializing the corpus;
2. **membership predicates** point-test each driven ID;
3. one **order scorer** produces a sort key only after every filter passes.

The scorer is deliberately last. A graph/scalar/spatial/text constraint is
therefore applied before an entity enters the vector, point-distance or BM25
top-k heap. Filtering a global top-k afterward is never used.

`CandidateDriver::Auto` chooses the first graph filter, scalar equality, text
filter, non-world spatial filter, remaining scalar filter, world spatial
filter, then the order driver. Exact-vector and BM25 order stream their native
locators/postings. A caller may explicitly select any graph, scalar, point or
text filter, the complete entity stream, or the order driver. There is no
silent truncation or early stop based on selectivity.

Every implemented driver guarantees unique IDs. Text Any merges equal heads
from at most 64 term cursors; Text All intersects them. Spatial ranges are
sorted, merged and disjoint. Graph traversal deduplicates through its
explicitly bounded visited set. A future OR driver must merge or add an
explicitly budgeted deduplication set before admission.

For each candidate the executor:

1. charges `candidates` and checks cancellation;
2. rejects the wrong output collection;
3. applies graph-set membership and point tests for scalar, spatial and text
   filters not supplied by the driver;
4. computes the order key, if the entity has the ranked value;
5. compares it with the prepared pagination position and offers it to the
   bounded page heap;
6. after the complete stream succeeds, point-gets each winner's authoritative
   primary row, then reads requested projections from those same bytes.

The winner point-get is charged to `primary_reads`, rejects an orphan derived
entry even for `Projection::Ids`, and is reused by projection. Projection runs
only for winners. The dense-v3 targeted reader validates the whole row while
allocating only the selected inline value, and a requested vector fetches only
its ordinal sidecar. `output_bytes` is checked before a page advances the
prepared cursor.

Normal index consistency relies on the engine's typed atomic writes. This query
path validates malformed native entries it reads and orphan IDs it would emit.
An ordinary indexed query cannot prove that an entry is absent from an index,
and winner-only validation does not detect every stale but well-formed text or
spatial posting. Detecting those cases requires the full index verifier/rebuild
workflow. Re-reading and rescoring every driven primary row would remove the
intended indexed-query work reduction, so it is not an implicit query-time
corruption guarantee.

## Shared work and cancellation

Family calls must receive one mutable `WorkMeter`; they must not each reset a
private `max_examined`. The meter checks a category before the corresponding
read or unit of CPU work. It checks cancellation before every posting/candidate
and periodically while decoding a maximal vector or text document. Exact-vector
scoring charges one sidecar and the declared lane count. A quantized page
charges compact probes/layout validation, approximate lanes, persisted-entry
rechecks, re-encoding lanes, sidecar reads and exact rerank lanes. Spatial range fallback
to the world remains exact but charges every posting. Text merging charges each
posting advanced, not merely each document emitted. Graph charges every edge
examined and every unique entity retained.

Budget exhaustion returns a typed resource-limit error with the exhausted
category and observed limit. Cancellation returns `Error::Cancelled` across
all families; graph's current string-valued cancellation error should be
normalized. Exact requests never return partial rows or set `done=true` after
an incomplete scan. Counters use checked u64/usize arithmetic.

Budgets are per `next_page` call and the returned `QueryWork` reports actual
use. Repeated ranked pages therefore expose their repeated scan cost. The
prepared cursor advances only after projection and output accounting succeed.

## Family hooks

The existing public family query methods remain useful bounded convenience
APIs, but they cannot be composed for broad exact queries: scalar, vector and
spatial candidate slices/results are capped at 65,536. The executor directly
uses source-level descriptor/key/scoring hooks for complete entity, scalar,
exact-vector, spatial and text streams.
following crate-private hooks. Public methods should later delegate to the same
hooks so validation and math have one implementation.

### Collection and scalar

- `EntityCursor(collection, after_id)` streams primary IDs/rows in ID order.
- `ScalarCursor(index, predicate, after_storage_key)` streams every matching
  posting, validating canonical key/value bytes, with no result vector.
- `scalar_matches(index, entity, predicate, optional_row, meter)` point-checks
  equality by posting when possible and uses a targeted dense-v3 scalar/state
  reader for ranges and null-versus-missing.
- `scalar_sort_key(index, entity, optional_row, meter)` returns the canonical
  encoded key without converting numeric types.
- `dense_v3::read_field` returns `Missing | Null | Present(TypedValueRef)` and
  validates the entire row, like `locate_vector`, without constructing the
  complete JSON document.

The present `query_scalar(..., limit)` must not be called repeatedly with
65,536 as pseudo-pagination: it has no cursor and broad ranges are ordered by
value then ID. `ScalarCursor` is the completeness primitive.

### Graph

- `graph_candidates(request, meter)` reuses deterministic BFS but returns a
  sorted unique ID vector (or a set with iteration) bounded by the request's
  explicit visited/result allowances. Exceeding either bound is an error.
- Direct-neighbor and BFS paths accept the shared meter and return
  `Error::Cancelled` consistently.

Graph deduplication inherently needs memory proportional to visited vertices;
this is allowed only because the caller states that bound. The current hard
maximum of 65,536 may remain an implementation policy initially, provided a
larger exact traversal is refused rather than truncated or described as a
complete result.

### Exact vector

- `VectorLocatorCursor(index)` streams all `0x73` locators.
- `score_exact_vector(index, entity, query, metric, meter)` point-gets that
  entity's locator and authoritative `0x60` sidecar, verifies immutable
  layout/ordinal/field consistency, and returns `None` for absent or zero-norm
  cosine data.
- `visit_exact_vector(index, candidate_iterator, query, metric, meter, visit)`
  scores arbitrary unique candidate iterators. It does not require a sorted
  65,536-ID slice and does not own a top-k heap.

The existing f64 lane-order accumulation, finite checks, zero-vector rule and
distance/ID ordering remain unchanged. Whole-index mode still point-gets the
sidecar named by each locator; it must not merge-walk unrelated `0x60` fields.

### Approximate vector

- `QuantizedVectorCursor(index)` streams all `0x79` compact entries without a
  corpus-sized candidate collection.
- Selective graph/scalar/spatial/text/entity drivers point-get one compact
  entry only after the candidate passes every filter.
- The bounded heap stores only `(approximate distance, EntityId, locator)` for
  at most `ef` rows. It does not retain vector payloads.
- Reranking point-gets the referenced `0x60` sidecar, validates its historical
  layout/ordinal, re-encodes symmetric-int8 bytes, and applies exact f64
  lane-order metric math. Missing, malformed or stale bytes are corruption.

`ef` is validated as `1..=65536`. Cosine refuses a zero query and excludes a
stored zero vector exactly as the exact family does. Cancellation or any work
budget failure returns no page and leaves search-after unchanged for retry.

### Spatial point

- The posting/range cursor spends from the shared meter instead of collecting
  a limited result and reports a whole-world fallback in `QueryDriver`.
- `point_matches(index, entity, predicate, meter)` performs exact bbox or
  GeographicLib radius refinement for one ID.
- `point_distance(index, entity, center, meter)` returns the exact WGS84
  distance for ordering.
- Add a targeted dense-v3 Point reader so filtered point tests do not decode a
  whole primary JSON object merely to reconstruct a posting.

Hilbert cells remain candidate filters. World fallback, dateline aliases and
radius envelopes never authorize an early incomplete result.

### Full text

The text driver uses these internal primitives:

- `TextCursor(index, analyzed_terms, Any | All)` holds at most the bounded
  query-term posting cursors, merge-advances by entity sequence, and yields one
  document with the term frequencies needed for BM25.
- `text_matches(index, entity, terms, mode, meter)` point-probes postings.
- `bm25_score(index, entity_or_merged_posting, terms, meter)` loads corpus and
  document statistics, uses each distinct term once, and returns no match for
  missing/null/empty-query cases.

Analyzer-v1 terms are deduplicated before cursors open and limited to 64.
BM25 merge order is an execution detail; final order is score descending then
ID. A corpus-sized score map is forbidden.

## Workload and app examples

The tiny fixture's mixed vector request becomes:

```rust
QueryRequest {
    collection: people,
    filters: &[
        QueryFilter::Graph(knows_depth_1_to_2_from_p0),
        QueryFilter::Scalar { index: active, predicate: ScalarFilter::Eq(true.into()) },
        QueryFilter::Point { index: position,
            predicate: PointFilter::Bbox(Bounds::new(0.0, 1.0, 0.0, 1.0)?) },
    ],
    order: QueryOrder::ExactVector {
        index: embedding,
        query: &[1.0, 0.0],
        metric: VectorMetric::Cosine,
    },
    projection: Projection::Ids,
    total_limit: Some(2),
    driver: CandidateDriver::Filter(0),
}
```

The graph result is `p1,p2,p3`; active and rectangle tests leave `p1`; only
then is `p1` scored. The active + text-flood + cosine request uses the text
merge as driver, tests `active`, then scores `p0,p1`, producing `p0,p1`.

app can use the same snapshot for its independently ranked sources without
pretending their scores share a natural scale:

```rust
let snapshot = Database::open_snapshot(path, config)?;

let resources = snapshot.prepare_query(QueryRequest {
    collection: data_resources,
    filters: &resource_facet_filters,
    order: QueryOrder::ExactVector {
        index: description_embedding,
        query: &embedding,
        metric: VectorMetric::Cosine,
    },
    projection: Projection::Fields(&["data_resource_id"]),
    total_limit: Some(50),
    driver: CandidateDriver::Auto,
})?.next_page(50, vector_budget, || cancelled())?;

let hooks = snapshot.prepare_query(QueryRequest {
    collection: search_hooks,
    filters: &[],
    order: QueryOrder::ExactVector {
        index: hook_embedding,
        query: &embedding,
        metric: VectorMetric::Cosine,
    },
    projection: Projection::Fields(&["search_hook_id", "content", "target_id"]),
    total_limit: Some(12),
    driver: CandidateDriver::Auto,
})?.next_page(12, hook_budget, || cancelled())?;

let lexical = snapshot.prepare_query(QueryRequest {
    collection: data_resources,
    filters: &resource_facet_filters,
    order: QueryOrder::Bm25 { index: keywords_text, query: "flood",
                             matching: TextMatch::Any },
    projection: Projection::Fields(&["data_resource_id"]),
    total_limit: Some(200),
    driver: CandidateDriver::Auto,
})?.next_page(200, text_budget, || cancelled())?;
```

All three see the same pinned committed root. The application may reproduce
its documented reciprocal-rank weighting from these explicit result lists.
The database should not freeze that product-specific formula or combine raw
BM25 and cosine values under an undocumented normalization. A later general
score expression needs versioned arithmetic, missing-component and pagination
semantics plus independent oracles.

`QueryOrder::ApproximateVector` is the separately named symmetric-int8 scan
described above. Its method, `ef`, examined count and reranked count are present
on every returned page. Qualification must report recall against an independent
exact oracle at stated `ef` values. The exact locator index is never advertised
as ANN or HNSW, and the compact scan is never advertised as sublinear.

## Incremental implementation and acceptance

1. **Implemented:** `WorkMeter`, `EntityCursor`, `ScalarCursor`, typed scalar filters
   and entity/scalar ordering. Prove unbounded-by-result-count scans with more
   than 65,536 matches under a sufficient work budget, and exact i64 boundary,
   null/missing and pagination behavior.
2. **Implemented:** `PreparedQuery`, point predicates and the page heap.
   Verify cancellation/budget errors leave its cursor unchanged and old
   snapshots retain pages across concurrent commits.
3. **Implemented:** graph traversal uses the shared meter and its explicit bounded
   candidate set. Test cycles, cross-collection seeds and bound exhaustion.
4. **Implemented:** exact vector traversal/scoring is split from its public collector;
   filter-before-top-k on the tiny independent oracle and broad streaming data.
5. **Implemented:** targeted Point decoding, exact bbox/radius refinement and a
   complete spatial posting cursor, including dateline and world-fallback cases.
6. **Implemented:** indexed text point scoring and the complete bounded Any/All
   posting merge support BM25 without a corpus-sized map.
7. **Implemented:** quantized-vector candidates are filtered before a bounded
   symmetric-int8 shortlist and authoritative f32 exact rerank. Diagnostics,
   retry semantics and independent tiny-fixture math are covered; Linux recall
   measurement remains a qualification deliverable.
8. **Implemented for correctness:** winner-only projection, work/driver
   diagnostics, the app-shaped example and tiny multimodel fixture. Measure each
   driver choice, repeated-page work, peak RSS and page output bytes at 10K,
   100K and representative 1M on Linux.

Acceptance compares every combined result with an independent fixture oracle,
including post-mutation snapshots, rollback/reopen, deletion/reinsertion IDs,
ties and page concatenation. It records the actual work counters and injects
corruption/cancellation/budget failures in every family position. Passing the
single-family suites is necessary but does not prove filter order, snapshot
coherence or pagination completeness.
