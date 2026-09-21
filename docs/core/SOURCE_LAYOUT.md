# Source layout

One line per module: the atomic that lives there and the contract section it
serves. Nothing in this document describes behaviour; the restructure that
produced it changed none.

The repository is three layers, one crate each, and the tables below give every
path from the repository root. `docs/LAYERS.md` is the one page that says which
layer owns what and how to run each layer's tests; this document is the module
map inside them. The crate a path belongs to reads off its first segment:
`core/kernel/` is `kernel`, `core/engine/` is `sekejap-core` (lib
`sekejap_core`), `lang/` is `sekejap-lang` (lib `sekejap_lang`), `dist/` is
`sekejap-dist`, `bench/` is `sekejap-bench`.

Contract documents referenced below: `docs/core/GRAPH_CONTRACT.md`,
`docs/lang/QL_CONTRACT.md` (the query-language contract, drafted 2026-09-20;
current headings are numbered `## 1.` through `## 7.`),
`docs/core/SPATIAL_FUNCTIONS.md`, `docs/core/COLLECTIONS.md`,
`docs/core/V2_COLLECTION_INTEGRATION.md`, `docs/core/RECOVERY_CONTRACT.md`.

## Crate root

| Module | Atomic | Contract |
| --- | --- | --- |
| `core/engine/src/lib.rs` | The row codec: `Layout`, `Kind`, dense P1 encode/decode, binary JSON, descriptors, projection. Also the module tree and the `pub use` aliases that keep every historical public path (`collection_backend`, `pagewal`, `recovery`, `spatial_geometry`, `spatial_math`) spelled as it always was. | `docs/core/FORMAT_V1.md` |

## `core/engine/src/store/` -- the storage layer

| Module | Atomic | Contract |
| --- | --- | --- |
| `core/engine/src/store/mod.rs` | The selected V2 backend: mapping a public `Config`/`ResourceLimits` onto the page-WAL and refusing what it cannot honour. Re-exported as `sekejap_core::collection_backend`. | `docs/core/V2_COLLECTION_INTEGRATION.md` |
| `core/engine/src/store/pagewal/mod.rs` | `PageWalStore`: `E4PWAL02` frames, two checkpoint metadata copies, committed-WAL overlay, publication hint, reader slots. | `docs/core/V2_COLLECTION_INTEGRATION.md` |
| `core/engine/src/store/pagewal/format.rs` | The on-disk header and create-feature bits. | `docs/core/FORMAT_FREEZE.md` |
| `core/engine/src/store/pagewal/current_reader.rs` | The read-only current-source reader used by rebuild and verification. | `docs/core/RECOVERY_CONTRACT.md` |
| `core/engine/src/store/pagewal/repair.rs` | `recover_to`: rebuilding a store from intact frames. | `docs/core/RECOVERY_CONTRACT.md` |
| `core/engine/src/store/recovery.rs` | Typed recovery above the kernel; rootless output is candidate evidence, never a published database. | `docs/core/RECOVERY_CONTRACT.md` |
| `core/engine/src/store/dense_v3.rs` | The dense-v3 row codec: direct encode, direct decode, single-field reads. | `docs/core/FORMAT_V1.md` |
| `core/engine/src/store/scalar_key.rs` | Version-1 scalar index keys: lexicographic byte order is value order within a declared kind. | `docs/core/COLLECTIONS.md` |

## `core/engine/src/collections/` -- typed collections

| Module | Atomic | Contract |
| --- | --- | --- |
| `core/engine/src/collections/mod.rs` | `Database`, `CollectionId`, `EntityId`, `Kind`-typed collections, put/get/delete, the catalog and header records, and every public re-export the crate has ever offered from `collections`. The catalog record also carries the DECLARED SQL spellings of the columns whose `Kind` does not name them (`TIMESTAMPTZ`, `DATE`: both `Kind::Int`), behind flag bit 2 of its own frozen flags byte. | `docs/core/COLLECTIONS.md`; `docs/lang/QL_CONTRACT.md` §4.2 |
| `core/engine/src/collections/catalog.rs` | The index catalog: `IndexInfo`/`IndexId`/`IndexFamily`/`IndexState`/`IndexExpr`, index create/drop, the index trees, scalar keys (`skey`) and index maintenance on write. An EXPRESSION index is a scalar index whose stored value is a closed function of its declared field (descriptor version 3, `EXPRESSION_FEATURE`). | `docs/core/COLLECTIONS.md`; `docs/lang/QL_CONTRACT.md` §4.1 |
| `core/engine/src/collections/drop_collection.rs` | Removing a collection: `DropMode`/`DropPhase`/`DropState`/`DropProgress`, `begin_drop_collection[_mode]`, `drop_collection_step`, the `DROP_FEATURE` bit and the descriptor's DROPPING tail. | `docs/lang/QL_CONTRACT.md` §2 (`DROP TABLE`); `docs/core/GRAPH_CONTRACT.md` §6.1 |
| `core/engine/src/collections/rebuild.rs` | Offline rebuild of a database into a fresh file, sorted index builds included. Public as `collections::rebuild`. | `docs/core/COLLECTIONS.md` |
| `core/engine/src/collections/verification.rs` | Whole-database verification against an independent walk of the source. Public as `collections::verification`. | `docs/core/COLLECTIONS.md` |
| `core/engine/src/collections/sort.rs` | The external sort the sorted index build runs on. | `docs/core/COLLECTIONS.md` |

## `core/engine/src/index/` -- the index families

| Module | Atomic | Contract |
| --- | --- | --- |
| `core/engine/src/index/mod.rs` | The family directory itself; one directory per family. | -- |
| `core/engine/src/index/text/mod.rs` | Persisted analyzer-v1 postings, term/corpus statistics and exact BM25 search. | `docs/lang/QL_CONTRACT.md` §4.6 "Text search" |
| `core/engine/src/index/text/analyzer.rs` | Analyzer v1: pinned Unicode alphanumeric/lowercase behaviour, no std Unicode on the runtime path. | `docs/lang/QL_CONTRACT.md` §4.6 "Text search" |
| `core/engine/src/index/text/segments.rs` | The segment posting format behind the `SEGMENT_FEATURE` bit. | `docs/core/FORMAT_FREEZE.md` |
| `core/engine/src/index/text/unicode_v1.rs` | The generated Unicode tables the analyzer is pinned to. | `docs/lang/QL_CONTRACT.md` §4.6 "Text search" |
| `core/engine/src/index/vector/exact.rs` | The exact vector index: locators, sidecar scans, `VectorMetric`, `VectorHit`. | `docs/lang/QL_CONTRACT.md` §4.5 "Vector" (exact) |
| `core/engine/src/index/vector/quantized.rs` | The quantized companion index, its candidates and its rerank. | `docs/lang/QL_CONTRACT.md` §4.5 "Vector" (approximate) |
| `core/engine/src/index/vector/quant.rs` | The quantizer itself (encode, decode, metrics). | `docs/lang/QL_CONTRACT.md` §4.5 "Vector" (approximate) |
| `core/engine/src/index/spatial/point.rs` | The point index: Hilbert postings, the `NearestWalk` ring walk, `SpatialHit`. | `docs/core/SPATIAL_FUNCTIONS.md` |
| `core/engine/src/index/spatial/geometry_index.rs` | The geometry index: cell cover entries at three levels behind `GEOMETRY_FEATURE`. | `docs/core/SPATIAL_FUNCTIONS.md` |
| `core/engine/src/index/spatial/geometry.rs` | The GeoJSON geometry model and its predicates. Public as `sekejap_core::spatial_geometry`. | `docs/core/SPATIAL_FUNCTIONS.md` |
| `core/engine/src/index/spatial/math.rs` | Geodesic distance, radius bounds, Hilbert ranges. Public as `sekejap_core::spatial_math`. | `docs/core/SPATIAL_FUNCTIONS.md` |
| `core/engine/src/index/graph/mod.rs` | Typed edges, edge/context names, the graph header, bounded neighbour and BFS traversals, the per-hop edge predicates and the reaching edge each result binds, cascade delete. | `docs/core/GRAPH_CONTRACT.md` §4.1-4.3 |

The scalar index family has no directory: its keys are `store::scalar_key`,
its catalog entry is `collections::catalog`, and its walk is `query::drivers`.

## `core/engine/src/query/` -- the query engine

| Module | Atomic | Contract |
| --- | --- | --- |
| `core/engine/src/query/mod.rs` | The request/response vocabulary a caller names: `QueryRequest`, `QueryFilter`, `QueryOrder`, `ScoreExpr`, `Projection`, `CandidateDriver`, `QueryDriver`, `QueryPage`, `QueryRow`, `OrderValue`, `QueryBudget`, `QueryWork`, `QueryError`, `WorkResource`, `ApproximationDiagnostics`, the `MAX_*` limits, and the `WorkMeter` every walk charges against. | `docs/lang/QL_CONTRACT.md` §2 "Statements", §6 "Execution guarantees" |
| `core/engine/src/query/plan.rs` | `Database::prepare_query`; the compiled forms (`CompiledFilter`, `CompiledOrder`, `PreparedText`); the `require_*` index checks; the scalar and geometry predicate encodings the index keyspace understands. | `docs/lang/QL_CONTRACT.md` §3 "Predicates and operators" |
| `core/engine/src/query/drivers.rs` | `DriverPlan` and driver selection (the Auto chain, `nearest_plan`, `nearest_drives_better`, `approximate_scan_drives`, `order_index_drives_better`); the bounded graph traversals and their per-hop pruning (`execute_graph`, `GraphAnswer`); the carried-key `Candidate`; every cursor's state and the `DriverCursor` keyspace helpers. | `docs/lang/QL_CONTRACT.md` §6 "Execution guarantees"; `docs/core/GRAPH_CONTRACT.md` §4.1-4.3 |
| `core/engine/src/query/cursors.rs` | One walk per driver: `DriverCursor` dispatch plus the per-cursor `next` (entities, scalar, keys, text, spatial, nearest, geometry, vector, quantized vector). | `docs/lang/QL_CONTRACT.md` §4 "Functions" (per-family dispatch) |
| `core/engine/src/query/membership.rs` | `MembershipSet`, `MembershipBudget`, `build_scalar_range_set`, `build_point_set`, the posting probes and `ensure_membership_sets`; the BOOLEAN set algebra (`SetExpr`, the union, the intersection, the complement and the universes they are taken against); `NodeGate`, the per-hop node predicates of a traversal, and `StandaloneNodeGate` for the traversal atomic. | `docs/lang/QL_CONTRACT.md` §3 "Predicates and operators", §6 "Execution guarantees"; `docs/core/GRAPH_CONTRACT.md` §4.3 |
| `core/engine/src/query/filters.rs` | The per-candidate tests: `filters_match`, `batch_filters_match`, `geometry_predicate_matches`, the scalar/text/point/JSON comparisons. `indexed_value` is the one place a row value is carried to an EXPRESSION index's stored value, and `scalar_filter_matches` / `persisted_scalar_key` both go through it. | `docs/lang/QL_CONTRACT.md` §3 "Predicates and operators" |
| `core/engine/src/query/rank.rs` | `RankKey`/`RankValue`/`HeapEntry`/`Winners`, `compare_rank`, `rank_candidate`, and the scores a rank key is built from: `text_score`, `vector_score`, `approximate_vector_score`, distance. | `docs/lang/QL_CONTRACT.md` §4.5 "Vector", §4.6 "Text search" |
| `core/engine/src/query/score.rs` | `CompiledScoreExpr`, `compile_score_expr`, `eval_score_expr`. | `docs/lang/QL_CONTRACT.md` §4.7 "Aggregates" |
| `core/engine/src/query/vector_scan.rs` | `unfiltered_vector_scan`, `filtered_vector_scan`, `scan_ceiling`, `vector_scan_progress`, `filtered_vector_scan_progress`, `vector_after`. | `docs/lang/QL_CONTRACT.md` §4.5 "Vector" (exact) |
| `core/engine/src/query/rows.rs` | Reading primary rows: `RowData`, the lockstep `PrimaryRows` reader, `read_batch_rows`, and the projection that turns a row into returned values. | `docs/lang/QL_CONTRACT.md` §6 "Execution guarantees" |
| `core/engine/src/query/page.rs` | `PreparedQuery`, `next_page`, `emit_rows`, `finish_page`, the shape questions (`keeps_a_run`, `batches_row_reads`, `winner_needs_no_row`, `a_filter_reads_the_row`, ...) and the resume state a page commits once its rows are final. | `docs/lang/QL_CONTRACT.md` §6 "Execution guarantees" |

## `lang/src/` -- the query language

| Module | Atomic | Contract |
| --- | --- | --- |
| `lang/src/lexer.rs` | The tokenizer: every operator PostgreSQL and its extensions spell, so a refusal can NAME what it refuses. | `docs/lang/QL_CONTRACT.md` §1 |
| `lang/src/ast.rs` | The shape of the text: statements, predicates, order keys, row expressions. Nothing here knows about indexes. | `docs/lang/QL_CONTRACT.md` §2 |
| `lang/src/parser/mod.rs` | The token stream, the `Parser` struct, statement dispatch and the shared helpers; `flatten_and` and `lower`. | `docs/lang/QL_CONTRACT.md` §2, §3 |
| `lang/src/parser/select.rs` | `SELECT`: select list, `GROUP BY`, `HAVING`, aggregate calls. | `docs/lang/QL_CONTRACT.md` §2, §3 |
| `lang/src/parser/expr.rs` | `WHERE`, the boolean tree, `ORDER BY` arithmetic, the §4.1/§4.2 function predicates and row expressions, literals and casts. | `docs/lang/QL_CONTRACT.md` §2, §3 |
| `lang/src/parser/graph_table.rs` | `GRAPH_TABLE` patterns, quantifiers, `COLUMNS`. | `docs/lang/QL_CONTRACT.md` §2, §3 |
| `lang/src/parser/dml.rs` | `INSERT`, `UPDATE`, `DELETE`. | `docs/lang/QL_CONTRACT.md` §2, §3 |
| `lang/src/parser/ddl.rs` | `CREATE TABLE`, `CREATE INDEX`, `DROP`. | `docs/lang/QL_CONTRACT.md` §2, §3 |
| `lang/src/compile/mod.rs` | The `Compiler`, statement dispatch, `SET LOCAL`, value and catalog helpers. | `docs/lang/QL_CONTRACT.md` §4, §6 |
| `lang/src/compile/plan.rs` | `SelectPlan`, `OwnedFilter`/`OwnedOrder`, the borrowed-view builders, `WritePlan`. | `docs/lang/QL_CONTRACT.md` §4, §6 |
| `lang/src/compile/aggregate.rs` | `AggregatePlan` and `GROUP BY`/`DISTINCT`/accumulator compilation. | `docs/lang/QL_CONTRACT.md` §4, §6 |
| `lang/src/compile/row.rs` | Row functions over projected values (`CompiledRow`, `iso_text`). | `docs/lang/QL_CONTRACT.md` §4, §6 |
| `lang/src/compile/select.rs` | Select list, projection, `ORDER BY`, vector order, score expressions. | `docs/lang/QL_CONTRACT.md` §4, §6 |
| `lang/src/compile/boolean.rs` | `where_filter` (the boolean tree), `semi_join`, negated tsquery. | `docs/lang/QL_CONTRACT.md` §4, §6 |
| `lang/src/compile/predicates.rs` | `filter()` and its scalar, text and spatial leaves. | `docs/lang/QL_CONTRACT.md` §4, §6 |
| `lang/src/compile/functions.rs` | The §4.1/§4.2 range rewrites (`time_filter`, `text_filter`, window and year ranges). | `docs/lang/QL_CONTRACT.md` §4, §6 |
| `lang/src/compile/graph_table.rs` | `GRAPH_TABLE` compilation to one bounded traversal. | `docs/lang/QL_CONTRACT.md` §4, §6 |
| `lang/src/compile/dml.rs` | `INSERT`/`UPDATE` documents and their declared-type values. | `docs/lang/QL_CONTRACT.md` §4, §6 |
| `lang/src/compile/ddl.rs` | `CREATE TABLE`, `CREATE INDEX`, `DROP TABLE` and its EXPLAIN. | `docs/lang/QL_CONTRACT.md` §4, §6 |
| `lang/src/functions.rs` | The §4.1 string and §4.2 date/time functions as PURE functions: the proleptic-Gregorian calendar, the literal reader and the ISO printer, `date_trunc`/`EXTRACT`/`to_char`, the fixed-width interval reader, and the text-prefix successor a `LIKE 'x%'` range needs. One implementation serves both the WHERE fold and the projected row function, so the two cannot disagree. | `docs/lang/QL_CONTRACT.md` §4.1, §4.2 |
| `lang/src/explain.rs` | `EXPLAIN`: the plan, the counters, and the two function sections (`range rewrites`, `row functions`). | `docs/lang/QL_CONTRACT.md` §6 |
| `lang/src/refuse.rs` | The Tier-2/Tier-3 table as data, plus `MULTI_RANGE` -- the reason a rewrite whose pre-image is a set of ranges carries. | `docs/lang/QL_CONTRACT.md` §4, `docs/core/FOUNDATION_TEST_STANDARD.md` law 8 |

## `core/engine/src/faults/` -- in-crate fault injection

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
1. The variant goes on `QueryFilter` in `core/engine/src/query/mod.rs`, next to the kind it
   most resembles; raise `MAX_FILTERS` only if the limit is what blocks it.
2. Its compiled form goes on `CompiledFilter` in `core/engine/src/query/plan.rs`, and the
   arm that builds it goes in `Database::prepare_query` there. If the filter
   needs a new encoding into index-key space, that helper lives in
   `core/engine/src/query/plan.rs` beside `encode_scalar_value`/`geometry_ranges`.
3. The per-candidate test goes in `core/engine/src/query/filters.rs`, in `filters_match`
   and (if it can be answered from a batched row read) `batch_filters_match`.
4. If the filter can be answered from postings rather than rows, add its set
   builder in `core/engine/src/query/membership.rs` and the arm in `ensure_membership_sets`.
5. If the filter can drive, add its `DriverPlan` variant and its selection rule
   in `core/engine/src/query/drivers.rs`, and its cursor in `core/engine/src/query/cursors.rs`.
6. `PreparedQuery::a_filter_reads_the_row` and `filters_are_row_pure` in
   `core/engine/src/query/page.rs` decide whether the new filter forces a row read; both
   must name it.

**A new `QueryOrder`.**
1. The variant goes on `QueryOrder` in `core/engine/src/query/mod.rs`.
2. Its compiled form goes on `CompiledOrder` in `core/engine/src/query/plan.rs`, with the
   `require_*` check that refuses it without the index it needs.
3. The rank key it produces goes in `core/engine/src/query/rank.rs`: a `RankValue` arm,
   its `compare_rank_value` arm, and its arm in `rank_candidate`.
4. `core/engine/src/query/page.rs` decides the walk: `driver_walks_in_rank_order`,
   `order_needs_the_row` and `winners_ascend_by_id` each need an arm, and
   `resume_*` in `core/engine/src/query/drivers.rs` needs one if the order can resume.

**A new `ScoreExpr` leaf.**
1. The variant goes on `ScoreExpr` in `core/engine/src/query/mod.rs`.
2. `CompiledScoreExpr`, `compile_score_expr` and `eval_score_expr` in
   `core/engine/src/query/score.rs` each take one arm; `MAX_SCORE_LEAVES` and
   `MAX_SCORE_DEPTH` in `core/engine/src/query/mod.rs` still bound the whole tree.
3. If the leaf reads a score that does not exist yet, the score function goes
   in `core/engine/src/query/rank.rs` next to `text_score`/`vector_score`.

**A new index family.**
1. A new directory under `core/engine/src/index/`, with `mod.rs` declaring it in
   `core/engine/src/index/mod.rs`.
2. Its entry key tag, its feature bit and its `IndexFamily` variant go in
   `core/engine/src/collections/catalog.rs`; the feature bit must also be listed in the
   known-features mask in `core/engine/src/collections/mod.rs`.
3. Its build and its per-write maintenance hook go in
   `core/engine/src/collections/catalog.rs` (`maintain_*`), its offline build in
   `core/engine/src/collections/rebuild.rs`, and its independent check in
   `core/engine/src/collections/verification.rs`.
4. Its `pub use` re-export goes in `core/engine/src/collections/mod.rs` so the family's
   public names keep leaving the crate through `sekejap_core::collections`.
5. Its fault-injection suite goes in `core/engine/src/faults/`, declared `cfg(test)` from
   the family's own `mod.rs`.

**A new collection-lifecycle phase.**
1. The phase goes on `DropPhase` in `core/engine/src/collections/drop_collection.rs`, with
   its byte, its `name` and its place in `next`; the byte is persisted, so a
   phase is appended, never renumbered.
2. Its step goes beside `drop_step_prefix` / `drop_step_rows` there, and its
   arm in `drop_collection_step`.
3. Its range probe goes in `drop_step_descriptor`'s list, which is what proves
   the keyspace empty before the descriptor is removed.
4. `lang/src/compile/ddl.rs`'s `explain_drop_table` prints the phase list, so it
   needs the arm too.

**A new driver.**
1. The `CandidateDriver` and `QueryDriver` variants go in `core/engine/src/query/mod.rs`
   (a caller can ask for it, and a page reports it).
2. The `DriverPlan` variant, the `DriverKey`, and the selection rule that picks
   it over the alternatives go in `core/engine/src/query/drivers.rs`, together with the
   cursor's state struct and its arm on `DriverCursor`.
3. The walk itself goes in `core/engine/src/query/cursors.rs`.
4. `core/engine/src/query/page.rs` must answer for it: `driver_walks_in_rank_order`,
   `driver_walks_ids_ascending`, `cursor_needs`, `keeps_a_run` and
   `winner_needs_no_row` each have one arm per driver.
5. If the driver can resume, its resume key goes in `core/engine/src/query/drivers.rs`
   beside `resume_scalar_key`/`resume_key_walk`.
