# Source layout

One line per module: the atomic that lives there and the contract section it
serves. Nothing in this document describes behaviour; the restructure that
produced it changed none.

Contract documents referenced below: `docs/GRAPH_CONTRACT.md`,
`docs/QL_CONTRACT.md` (the query-language contract, drafted 2026-09-20;
current headings are numbered `## 1.` through `## 7.`),
`docs/SPATIAL_FUNCTIONS.md`, `docs/COLLECTIONS.md`,
`docs/V2_COLLECTION_INTEGRATION.md`, `docs/RECOVERY_CONTRACT.md`.

## Crate root

| Module | Atomic | Contract |
| --- | --- | --- |
| `src/lib.rs` | The row codec: `Layout`, `Kind`, dense P1 encode/decode, binary JSON, descriptors, projection. Also the module tree and the `pub use` aliases that keep every historical public path (`collection_backend`, `pagewal`, `recovery`, `spatial_geometry`, `spatial_math`) spelled as it always was. | `docs/FORMAT_V1.md` |

## `src/store/` -- the storage layer

| Module | Atomic | Contract |
| --- | --- | --- |
| `src/store/mod.rs` | The selected V2 backend: mapping a public `Config`/`ResourceLimits` onto the page-WAL and refusing what it cannot honour. Re-exported as `e4_prototype::collection_backend`. | `docs/V2_COLLECTION_INTEGRATION.md` |
| `src/store/pagewal/mod.rs` | `PageWalStore`: `E4PWAL02` frames, two checkpoint metadata copies, committed-WAL overlay, publication hint, reader slots. | `docs/V2_COLLECTION_INTEGRATION.md` |
| `src/store/pagewal/format.rs` | The on-disk header and create-feature bits. | `docs/FORMAT_FREEZE.md` |
| `src/store/pagewal/current_reader.rs` | The read-only current-source reader used by rebuild and verification. | `docs/RECOVERY_CONTRACT.md` |
| `src/store/pagewal/repair.rs` | `recover_to`: rebuilding a store from intact frames. | `docs/RECOVERY_CONTRACT.md` |
| `src/store/recovery.rs` | Typed recovery above the kernel; rootless output is candidate evidence, never a published database. | `docs/RECOVERY_CONTRACT.md` |
| `src/store/dense_v3.rs` | The dense-v3 row codec: direct encode, direct decode, single-field reads. | `docs/FORMAT_V1.md` |
| `src/store/scalar_key.rs` | Version-1 scalar index keys: lexicographic byte order is value order within a declared kind. | `docs/COLLECTIONS.md` |

## `src/collections/` -- typed collections

| Module | Atomic | Contract |
| --- | --- | --- |
| `src/collections/mod.rs` | `Database`, `CollectionId`, `EntityId`, `Kind`-typed collections, put/get/delete, the catalog and header records, and every public re-export the crate has ever offered from `collections`. | `docs/COLLECTIONS.md` |
| `src/collections/catalog.rs` | The index catalog: `IndexInfo`/`IndexId`/`IndexFamily`/`IndexState`, index create/drop, the index trees, scalar keys (`skey`) and index maintenance on write. | `docs/COLLECTIONS.md` |
| `src/collections/rebuild.rs` | Offline rebuild of a database into a fresh file, sorted index builds included. Public as `collections::rebuild`. | `docs/COLLECTIONS.md` |
| `src/collections/verification.rs` | Whole-database verification against an independent walk of the source. Public as `collections::verification`. | `docs/COLLECTIONS.md` |
| `src/collections/sort.rs` | The external sort the sorted index build runs on. | `docs/COLLECTIONS.md` |

## `src/index/` -- the index families

| Module | Atomic | Contract |
| --- | --- | --- |
| `src/index/mod.rs` | The family directory itself; one directory per family. | -- |
| `src/index/text/mod.rs` | Persisted analyzer-v1 postings, term/corpus statistics and exact BM25 search. | `docs/QL_CONTRACT.md` §4.6 "Text search" |
| `src/index/text/analyzer.rs` | Analyzer v1: pinned Unicode alphanumeric/lowercase behaviour, no std Unicode on the runtime path. | `docs/QL_CONTRACT.md` §4.6 "Text search" |
| `src/index/text/segments.rs` | The segment posting format behind the `SEGMENT_FEATURE` bit. | `docs/FORMAT_FREEZE.md` |
| `src/index/text/unicode_v1.rs` | The generated Unicode tables the analyzer is pinned to. | `docs/QL_CONTRACT.md` §4.6 "Text search" |
| `src/index/vector/exact.rs` | The exact vector index: locators, sidecar scans, `VectorMetric`, `VectorHit`. | `docs/QL_CONTRACT.md` §4.5 "Vector" (exact) |
| `src/index/vector/quantized.rs` | The quantized companion index, its candidates and its rerank. | `docs/QL_CONTRACT.md` §4.5 "Vector" (approximate) |
| `src/index/vector/quant.rs` | The quantizer itself (encode, decode, metrics). | `docs/QL_CONTRACT.md` §4.5 "Vector" (approximate) |
| `src/index/spatial/point.rs` | The point index: Hilbert postings, the `NearestWalk` ring walk, `SpatialHit`. | `docs/SPATIAL_FUNCTIONS.md` |
| `src/index/spatial/geometry_index.rs` | The geometry index: cell cover entries at three levels behind `GEOMETRY_FEATURE`. | `docs/SPATIAL_FUNCTIONS.md` |
| `src/index/spatial/geometry.rs` | The GeoJSON geometry model and its predicates. Public as `e4_prototype::spatial_geometry`. | `docs/SPATIAL_FUNCTIONS.md` |
| `src/index/spatial/math.rs` | Geodesic distance, radius bounds, Hilbert ranges. Public as `e4_prototype::spatial_math`. | `docs/SPATIAL_FUNCTIONS.md` |
| `src/index/graph/mod.rs` | Typed edges, edge/context names, the graph header, bounded neighbour and BFS traversals, cascade delete. | `docs/GRAPH_CONTRACT.md` §4.1-4.2 (§4.3 per-hop predicates PENDING, order-of-work item 1) |

The scalar index family has no directory: its keys are `store::scalar_key`,
its catalog entry is `collections::catalog`, and its walk is `query::drivers`.

## `src/query/` -- the query engine

| Module | Atomic | Contract |
| --- | --- | --- |
| `src/query/mod.rs` | The request/response vocabulary a caller names: `QueryRequest`, `QueryFilter`, `QueryOrder`, `ScoreExpr`, `Projection`, `CandidateDriver`, `QueryDriver`, `QueryPage`, `QueryRow`, `OrderValue`, `QueryBudget`, `QueryWork`, `QueryError`, `WorkResource`, `ApproximationDiagnostics`, the `MAX_*` limits, and the `WorkMeter` every walk charges against. | `docs/QL_CONTRACT.md` §2 "Statements", §6 "Execution guarantees" |
| `src/query/plan.rs` | `Database::prepare_query`; the compiled forms (`CompiledFilter`, `CompiledOrder`, `PreparedText`); the `require_*` index checks; the scalar and geometry predicate encodings the index keyspace understands. | `docs/QL_CONTRACT.md` §3 "Predicates and operators" |
| `src/query/drivers.rs` | `DriverPlan` and driver selection (the Auto chain, `nearest_plan`, `nearest_drives_better`, `approximate_scan_drives`, `order_index_drives_better`); the bounded graph traversals; the carried-key `Candidate`; every cursor's state and the `DriverCursor` keyspace helpers. | `docs/QL_CONTRACT.md` §6 "Execution guarantees"; `docs/GRAPH_CONTRACT.md` §4.1-4.2 (§4.3 PENDING -- see `execute_graph`'s doc comment) |
| `src/query/cursors.rs` | One walk per driver: `DriverCursor` dispatch plus the per-cursor `next` (entities, scalar, keys, text, spatial, nearest, geometry, vector, quantized vector). | `docs/QL_CONTRACT.md` §4 "Functions" (per-family dispatch) |
| `src/query/membership.rs` | `MembershipSet`, `MembershipBudget`, `build_scalar_range_set`, `build_point_set`, the posting probes and `ensure_membership_sets`. | `docs/QL_CONTRACT.md` §6 "Execution guarantees" |
| `src/query/filters.rs` | The per-candidate tests: `filters_match`, `batch_filters_match`, `geometry_predicate_matches`, the scalar/text/point/JSON comparisons. | `docs/QL_CONTRACT.md` §3 "Predicates and operators" |
| `src/query/rank.rs` | `RankKey`/`RankValue`/`HeapEntry`/`Winners`, `compare_rank`, `rank_candidate`, and the scores a rank key is built from: `text_score`, `vector_score`, `approximate_vector_score`, distance. | `docs/QL_CONTRACT.md` §4.5 "Vector", §4.6 "Text search" |
| `src/query/score.rs` | `CompiledScoreExpr`, `compile_score_expr`, `eval_score_expr`. | `docs/QL_CONTRACT.md` §4.7 "Aggregates" |
| `src/query/vector_scan.rs` | `unfiltered_vector_scan`, `filtered_vector_scan`, `scan_ceiling`, `vector_scan_progress`, `filtered_vector_scan_progress`, `vector_after`. | `docs/QL_CONTRACT.md` §4.5 "Vector" (exact) |
| `src/query/rows.rs` | Reading primary rows: `RowData`, the lockstep `PrimaryRows` reader, `read_batch_rows`, and the projection that turns a row into returned values. | `docs/QL_CONTRACT.md` §6 "Execution guarantees" |
| `src/query/page.rs` | `PreparedQuery`, `next_page`, `emit_rows`, `finish_page`, the shape questions (`keeps_a_run`, `batches_row_reads`, `winner_needs_no_row`, `a_filter_reads_the_row`, ...) and the resume state a page commits once its rows are final. | `docs/QL_CONTRACT.md` §6 "Execution guarantees" |

## `src/faults/` -- in-crate fault injection

Test-only modules, `cfg(test)` exactly as before. Each is declared by the
module it exercises (so it keeps that module's private surface) and the file
lives here so every fault suite is in one place:
`pagewal_fault_tests.rs` and `pagewal_hint_tests.rs` (declared by
`store/pagewal/mod.rs`), `index_fault_tests.rs` (`collections/catalog.rs`),
`graph_fault_tests.rs` (`index/graph/mod.rs`),
`spatial_fault_tests.rs` (`index/spatial/point.rs`),
`text_fault_tests.rs` (`index/text/mod.rs`),
`vector_fault_tests.rs` (`index/vector/exact.rs`).

## Where to add X

**A new `QueryFilter` kind.**
1. The variant goes on `QueryFilter` in `src/query/mod.rs`, next to the kind it
   most resembles; raise `MAX_FILTERS` only if the limit is what blocks it.
2. Its compiled form goes on `CompiledFilter` in `src/query/plan.rs`, and the
   arm that builds it goes in `Database::prepare_query` there. If the filter
   needs a new encoding into index-key space, that helper lives in
   `src/query/plan.rs` beside `encode_scalar_value`/`geometry_ranges`.
3. The per-candidate test goes in `src/query/filters.rs`, in `filters_match`
   and (if it can be answered from a batched row read) `batch_filters_match`.
4. If the filter can be answered from postings rather than rows, add its set
   builder in `src/query/membership.rs` and the arm in `ensure_membership_sets`.
5. If the filter can drive, add its `DriverPlan` variant and its selection rule
   in `src/query/drivers.rs`, and its cursor in `src/query/cursors.rs`.
6. `PreparedQuery::a_filter_reads_the_row` and `filters_are_row_pure` in
   `src/query/page.rs` decide whether the new filter forces a row read; both
   must name it.

**A new `QueryOrder`.**
1. The variant goes on `QueryOrder` in `src/query/mod.rs`.
2. Its compiled form goes on `CompiledOrder` in `src/query/plan.rs`, with the
   `require_*` check that refuses it without the index it needs.
3. The rank key it produces goes in `src/query/rank.rs`: a `RankValue` arm,
   its `compare_rank_value` arm, and its arm in `rank_candidate`.
4. `src/query/page.rs` decides the walk: `driver_walks_in_rank_order`,
   `order_needs_the_row` and `winners_ascend_by_id` each need an arm, and
   `resume_*` in `src/query/drivers.rs` needs one if the order can resume.

**A new `ScoreExpr` leaf.**
1. The variant goes on `ScoreExpr` in `src/query/mod.rs`.
2. `CompiledScoreExpr`, `compile_score_expr` and `eval_score_expr` in
   `src/query/score.rs` each take one arm; `MAX_SCORE_LEAVES` and
   `MAX_SCORE_DEPTH` in `src/query/mod.rs` still bound the whole tree.
3. If the leaf reads a score that does not exist yet, the score function goes
   in `src/query/rank.rs` next to `text_score`/`vector_score`.

**A new index family.**
1. A new directory under `src/index/`, with `mod.rs` declaring it in
   `src/index/mod.rs`.
2. Its entry key tag, its feature bit and its `IndexFamily` variant go in
   `src/collections/catalog.rs`; the feature bit must also be listed in the
   known-features mask in `src/collections/mod.rs`.
3. Its build and its per-write maintenance hook go in
   `src/collections/catalog.rs` (`maintain_*`), its offline build in
   `src/collections/rebuild.rs`, and its independent check in
   `src/collections/verification.rs`.
4. Its `pub use` re-export goes in `src/collections/mod.rs` so the family's
   public names keep leaving the crate through `e4_prototype::collections`.
5. Its fault-injection suite goes in `src/faults/`, declared `cfg(test)` from
   the family's own `mod.rs`.

**A new driver.**
1. The `CandidateDriver` and `QueryDriver` variants go in `src/query/mod.rs`
   (a caller can ask for it, and a page reports it).
2. The `DriverPlan` variant, the `DriverKey`, and the selection rule that picks
   it over the alternatives go in `src/query/drivers.rs`, together with the
   cursor's state struct and its arm on `DriverCursor`.
3. The walk itself goes in `src/query/cursors.rs`.
4. `src/query/page.rs` must answer for it: `driver_walks_in_rank_order`,
   `driver_walks_ids_ascending`, `cursor_needs`, `keeps_a_run` and
   `winner_needs_no_row` each have one arm per driver.
5. If the driver can resume, its resume key goes in `src/query/drivers.rs`
   beside `resume_scalar_key`/`resume_key_walk`.
