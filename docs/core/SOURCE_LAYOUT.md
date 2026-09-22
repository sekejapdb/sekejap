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
`sekejap-dist`, `dist/rust/` is `sekejap` (the PUBLISHED crate name),
`dist/ffi/` is `sekejap-capi` (lib `sekejap`, so `libsekejap` on disk),
`bench/` is `sekejap-bench`.

Contract documents referenced below: `docs/core/GRAPH_CONTRACT.md`,
`docs/lang/QL_CONTRACT.md` (the query-language contract, drafted 2026-09-20;
current headings are numbered `## 1.` through `## 7.`),
`docs/core/SPATIAL_FUNCTIONS.md`, `docs/core/COLLECTIONS.md`,
`docs/core/V2_COLLECTION_INTEGRATION.md`, `docs/core/RECOVERY_CONTRACT.md`.

## Crate root

| Module | Atomic | Contract |
| --- | --- | --- |
| `core/engine/src/lib.rs` | The row codec: `Layout`, `Kind`, dense P1 encode/decode, binary JSON, descriptors, projection. Also the module tree and the `pub use` aliases that keep every historical public path (`collection_backend`, `pagewal`, `recovery`, `spatial_geometry`, `spatial_math`) spelled as it always was, and the `FORMAT_VERSION` re-export. | `docs/core/FORMAT_V2.md` |

## `core/engine/src/store/` -- the storage layer

| Module | Atomic | Contract |
| --- | --- | --- |
| `core/engine/src/store/mod.rs` | The selected V2 backend: mapping a public `Config`/`ResourceLimits` onto the page-WAL and refusing what it cannot honour. Re-exported as `sekejap_core::collection_backend`. | `docs/core/V2_COLLECTION_INTEGRATION.md` |
| `core/engine/src/store/pagewal/mod.rs` | `PageWalStore`: `E4PWAL02` frames, two checkpoint metadata copies, committed-WAL overlay, publication hint, reader slots. | `docs/core/V2_COLLECTION_INTEGRATION.md` |
| `core/engine/src/store/pagewal/format.rs` | The on-disk header, the create-feature bits, and the disk-format stamp checked on both metadata copies before any other page is read. | `docs/core/FORMAT_V2.md` |
| `core/engine/src/store/pagewal/current_reader.rs` | The read-only current-source reader used by rebuild and verification. | `docs/core/RECOVERY_CONTRACT.md` |
| `core/engine/src/store/pagewal/repair.rs` | `recover_to`: rebuilding a store from intact frames. | `docs/core/RECOVERY_CONTRACT.md` |
| `core/engine/src/store/recovery.rs` | Typed recovery above the kernel; rootless output is candidate evidence, never a published database. | `docs/core/RECOVERY_CONTRACT.md` |
| `core/engine/src/store/dense_v3.rs` | The dense-v3 row codec: direct encode, direct decode, single-field reads. | `docs/core/FORMAT_V2.md` |
| `core/engine/src/store/scalar_key.rs` | Version-1 scalar index keys: lexicographic byte order is value order within a declared kind. | `docs/core/COLLECTIONS.md` |

## `core/engine/src/collections/` -- typed collections

| Module | Atomic | Contract |
| --- | --- | --- |
| `core/engine/src/collections/mod.rs` | `Database`, `CollectionId`, `EntityId`, `Kind`-typed collections, put/get/delete, the catalog and header records, and every public re-export the crate has ever offered from `collections`. The catalog record also carries the DECLARED SQL spellings of the columns whose `Kind` does not name them (`TIMESTAMPTZ`, `DATE`: both `Kind::Int`), behind flag bit 2 of its own frozen flags byte. | `docs/core/COLLECTIONS.md`; `docs/lang/QL_CONTRACT.md` §4.2 |
| `core/engine/src/collections/catalog.rs` | The index catalog: `IndexInfo`/`IndexId`/`IndexFamily`/`IndexState`/`IndexExpr`, index create/drop, the index trees, scalar keys (`skey`) and index maintenance on write. An EXPRESSION index is a scalar index whose stored value is a closed function of its declared field: `lower(col)` (descriptor version 3, `EXPRESSION_FEATURE`) and `col->>'member'` over a `JSONB` column (descriptor version 4, `JSON_EXPRESSION_FEATURE`, the member name in the descriptor tail). | `docs/core/COLLECTIONS.md`; `docs/lang/QL_CONTRACT.md` §4.1; `docs/lang/INDEX_CONTRACT.md` |
| `core/engine/src/collections/column_rules.rs` | The per-field COLUMN RULE slot: `ColumnRule`/`DefaultValue`, the catalog record's RULES tail and its `CATALOG_RULES` flag bit, the additive `COLUMN_RULES_FEATURE` bit, the write-path application (defaults filled into a MISSING field, then the NOT NULL check), and the RFC 4122 UUID generators with the SHA-1 they need. | `docs/lang/QL_CONTRACT.md` §2 (`DEFAULT`, `NOT NULL`) |
| `core/engine/src/collections/row_count.rs` | The LIVE ROW COUNT: one record per collection (key tag `0x08`, value `rows: u64 \|\| generation: u64`), the per-transaction delta the write path accumulates and `commit` writes once per touched collection, `Database::row_count`, `Database::backfill_row_counts` (the bounded, resumable build for a database that has none) and the additive `ROW_COUNT_FEATURE` bit. | `docs/core/FORMAT_V2.md`; `docs/lang/QL_CONTRACT.md` §4.7 (`count(*)`) |
| `core/engine/src/collections/drop_collection.rs` | Removing a collection: `DropMode`/`DropPhase`/`DropState`/`DropProgress`, `begin_drop_collection[_mode]`, `drop_collection_step`, the `DROP_FEATURE` bit and the descriptor's DROPPING tail. | `docs/lang/QL_CONTRACT.md` §2 (`DROP TABLE`); `docs/core/GRAPH_CONTRACT.md` §6.1 |
| `core/engine/src/collections/write_set.rs` | Bounded, resumable writes over a CANDIDATE SET: `Database::delete_where` / `update_where` / `write_where`, `WriteRequest`/`WriteAction`/`WriteProgress`, `UpdatePatch`/`PatchValue` (the `&mut dyn FnMut(&Value) -> Result<Value>` closure boundary a row expression crosses), `DeleteMode` and the per-row GRAPH_CONTRACT 6.1 RESTRICT preflight. Also the bulk write scope `begin_bulk` / `end_bulk`, whose outermost close commits. | `docs/lang/QL_CONTRACT.md` §2 (`UPDATE ... WHERE`, `DELETE ... WHERE`, `BEGIN BULK`); `docs/core/GRAPH_CONTRACT.md` §6.1; `docs/dist/OPS_CONTRACT.md` §7 |
| `core/engine/src/collections/rebuild.rs` | Offline rebuild of a database into a fresh file, sorted index builds included. Public as `collections::rebuild`. | `docs/core/COLLECTIONS.md` |
| `core/engine/src/collections/verification.rs` | Whole-database verification against an independent walk of the source. Public as `collections::verification`. | `docs/core/COLLECTIONS.md` |
| `core/engine/src/collections/sort.rs` | The external sort the sorted index build runs on. | `docs/core/COLLECTIONS.md` |

## `core/engine/src/index/` -- the index families

| Module | Atomic | Contract |
| --- | --- | --- |
| `core/engine/src/index/mod.rs` | The family directory itself; one directory per family. | -- |
| `core/engine/src/index/text/mod.rs` | Persisted analyzer-v1 postings, term/corpus statistics and exact BM25 search. | `docs/lang/QL_CONTRACT.md` §4.6 "Text search" |
| `core/engine/src/index/text/analyzer.rs` | Analyzer v1: pinned Unicode alphanumeric/lowercase behaviour, no std Unicode on the runtime path. | `docs/lang/QL_CONTRACT.md` §4.6 "Text search" |
| `core/engine/src/index/text/segments.rs` | The segment posting format behind the `SEGMENT_FEATURE` bit. | `docs/core/FORMAT_V2.md` |
| `core/engine/src/index/text/unicode_v1.rs` | The generated Unicode tables the analyzer is pinned to. | `docs/lang/QL_CONTRACT.md` §4.6 "Text search" |
| `core/engine/src/index/vector/exact.rs` | The exact vector index: locators, sidecar scans, `VectorMetric`, `VectorHit`. | `docs/lang/QL_CONTRACT.md` §4.5 "Vector" (exact) |
| `core/engine/src/index/vector/quantized.rs` | The quantized companion index, its candidates and its rerank. | `docs/lang/QL_CONTRACT.md` §4.5 "Vector" (approximate) |
| `core/engine/src/index/vector/quant.rs` | The quantizer itself (encode, decode, metrics). | `docs/lang/QL_CONTRACT.md` §4.5 "Vector" (approximate) |
| `core/engine/src/index/spatial/point.rs` | The point index: Hilbert postings, the `NearestWalk` ring walk, `SpatialHit`. | `docs/core/SPATIAL_FUNCTIONS.md` |
| `core/engine/src/index/spatial/geometry_index.rs` | The geometry index: cell cover entries at three levels behind `GEOMETRY_FEATURE`. | `docs/core/SPATIAL_FUNCTIONS.md` |
| `core/engine/src/index/spatial/geometry.rs` | The GeoJSON geometry model and its predicates. Public as `sekejap_core::spatial_geometry`. | `docs/core/SPATIAL_FUNCTIONS.md` |
| `core/engine/src/index/spatial/math.rs` | Geodesic distance, radius bounds, Hilbert ranges. Public as `sekejap_core::spatial_math`. | `docs/core/SPATIAL_FUNCTIONS.md` |
| `core/engine/src/index/graph/mod.rs` | Typed edges, edge/context names, the graph header, bounded neighbour and BFS traversals, the per-hop edge predicates and the reaching edge each result binds, cascade delete. | `docs/core/GRAPH_CONTRACT.md` §4.1-4.3 |
| `core/engine/src/index/graph/mod.rs` (catalog readers) | `Database::graph_names`, the name dictionary read as catalog data (one entry per interned edge type and graph context, capped at 4,096 per kind, no edge read); `Database::edge_shape`, the catalog's EDGE-TYPE ROWS -- one entry per distinct `(context, type, from collection, to collection)` derived from written edges, one descent per distinct `(source entity, context, type, destination collection)` and capped, with truncation reported. | `docs/core/GRAPH_CONTRACT.md` 2.5, 3.4; `docs/lang/QL_CONTRACT.md` §1 (catalog) |
| `core/engine/src/index/graph/endpoints.rs` | The ENDPOINT SETS (tag `0x7E`, additive `ENDPOINT_FEATURE = 0x4000`): one key per DISTINCT entity with at least one edge of a (context, type, direction), the write-path maintenance on both ends of a new and of a removed edge, `Database::backfill_endpoint_sets` for a database whose edges predate them, and the read `Database::edge_endpoints` answers a semi-join from. | `docs/core/GRAPH_CONTRACT.md` §4.1; `docs/lang/QL_CONTRACT.md` §3 (semi-join) |

The scalar index family has no directory: its keys are `store::scalar_key`,
its catalog entry is `collections::catalog`, and its walk is `query::drivers`.

## `core/engine/src/query/` -- the query engine

| Module | Atomic | Contract |
| --- | --- | --- |
| `core/engine/src/query/mod.rs` | The request/response vocabulary a caller names: `QueryRequest`, `QueryFilter`, `QueryOrder`, `ScoreExpr`, `Projection`, `CandidateDriver`, `QueryDriver`, `QueryPage`, `QueryRow`, `OrderValue`, `QueryBudget`, `QueryWork`, `QueryError`, `WorkResource`, `ApproximationDiagnostics`, the opaque `WriteCursor` a bounded write pass resumes from, the `MAX_*` limits, and the `WorkMeter` every walk charges against -- including its wall-clock half: `QueryBudget::deadline`, `WorkResource::Deadline` and `DEADLINE_POLL_CHARGES`, the stated 1,024-charge interval between two clock reads. | `docs/lang/QL_CONTRACT.md` §2 "Statements", §6 "Execution guarantees"; `docs/dist/OPS_CONTRACT.md` §3 |
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
| `lang/src/catalog.rs` | The CATALOG SURFACE as VIRTUAL ROWS: the relation directory (`db_tables`, `db_columns`, `db_indexes`, `db_edges`, `db_contexts`; `information_schema.{schemata,tables,columns,table_constraints,key_column_usage}`; `pg_catalog.{pg_namespace,pg_class,pg_attribute,pg_type,pg_index,pg_indexes,pg_description,pg_constraint,pg_tables}`; PostGIS `geometry_columns` and `spatial_ref_sys`), the PostgreSQL type OIDs each column is described with, the synthetic object OID, and the builders that compute every row at PREPARE from `list_collections` / `collection_info` / `list_indexes` / `row_count` / `graph_names` / `edge_shape`. Nothing is stored. | `docs/lang/QL_CONTRACT.md` §1 (catalog); `docs/dist/PG_SURFACE.md` |
| `lang/src/lexer.rs` | The tokenizer: every operator PostgreSQL and its extensions spell, so a refusal can NAME what it refuses. | `docs/lang/QL_CONTRACT.md` §1 |
| `lang/src/ast.rs` | The shape of the text: statements, predicates, order keys, row expressions. Nothing here knows about indexes. | `docs/lang/QL_CONTRACT.md` §2 |
| `lang/src/parser/mod.rs` | The token stream, the `Parser` struct, statement dispatch and the shared helpers; `flatten_and` and `lower`. | `docs/lang/QL_CONTRACT.md` §2, §3 |
| `lang/src/parser/catalog.rs` | The statements a PostgreSQL client writes that have no collection in them: the FROM-less `SELECT` of session facts (`version()`, `current_schema()`, `current_user`, `pg_backend_pid()`, `current_setting()`, `SELECT 1`), the `SHOW` family, and the closed list of client GUCs a driver `SET`s on connect with the constant each reports. | `docs/lang/QL_CONTRACT.md` §2; `docs/dist/PG_SURFACE.md` §3-§5 |
| `lang/src/parser/select.rs` | `SELECT`: select list, `GROUP BY`, `HAVING`, aggregate calls. | `docs/lang/QL_CONTRACT.md` §2, §3 |
| `lang/src/parser/expr.rs` | `WHERE`, the boolean tree, `ORDER BY` arithmetic, the §4.1/§4.2 function predicates and row expressions, literals and casts. | `docs/lang/QL_CONTRACT.md` §2, §3 |
| `lang/src/parser/graph_table.rs` | `GRAPH_TABLE` patterns, quantifiers, `COLUMNS`. | `docs/lang/QL_CONTRACT.md` §2, §3 |
| `lang/src/parser/dml.rs` | `INSERT`, `UPDATE`, `DELETE`: the key-equality forms (one point write) and the PREDICATED forms (`WHERE <any predicate>`, `[RESTRICT\|CASCADE]`, `FROM ALL`), told apart by looking at the `WHERE` and backing up. | `docs/lang/QL_CONTRACT.md` §2, §3 |
| `lang/src/parser/ddl.rs` | `CREATE TABLE` and its column clauses (`DEFAULT <generator>`, `NOT NULL`), `CREATE INDEX`, `ALTER TABLE`, `DROP`. | `docs/lang/QL_CONTRACT.md` §2, §3 |
| `lang/src/compile/mod.rs` | The `Compiler`, statement dispatch, `SET LOCAL`, value and catalog helpers. | `docs/lang/QL_CONTRACT.md` §4, §6 |
| `lang/src/compile/plan.rs` | `SelectPlan`, `OwnedFilter`/`OwnedOrder`, the borrowed-view builders, `WritePlan`. | `docs/lang/QL_CONTRACT.md` §4, §6 |
| `lang/src/compile/aggregate.rs` | `AggregatePlan` and `GROUP BY`/`DISTINCT`/accumulator compilation. | `docs/lang/QL_CONTRACT.md` §4, §6 |
| `lang/src/compile/row.rs` | Row functions over projected values (`CompiledRow`, `iso_text`). | `docs/lang/QL_CONTRACT.md` §4, §6 |
| `lang/src/compile/rows.rs` | The `Rows` DRIVER: a bounded in-memory row source the compiler builds at prepare, and the one thing the catalog surface adds to the plan. `RowsPlan`, the per-row predicate evaluator (`RowFilter`), the `ORDER BY` / `DISTINCT` / `LIMIT` over the list, the §4.1 string row functions over a catalog column, the `SHOW` forms and `SHOW CREATE TABLE`, and the EXPLAIN that names the driver and the row count. The bound is the catalog's size; nothing resumes because the whole relation fits in one answer by construction. | `docs/lang/QL_CONTRACT.md` §1 (catalog), §2 (`SHOW`); `docs/dist/PG_SURFACE.md` |
| `lang/src/compile/select.rs` | Select list, projection, `ORDER BY`, vector order, score expressions. | `docs/lang/QL_CONTRACT.md` §4, §6 |
| `lang/src/compile/boolean.rs` | `where_filter` (the boolean tree), `semi_join`, negated tsquery. | `docs/lang/QL_CONTRACT.md` §4, §6 |
| `lang/src/compile/predicates.rs` | `filter()` and its scalar, text and spatial leaves. | `docs/lang/QL_CONTRACT.md` §4, §6 |
| `lang/src/compile/functions.rs` | The §4.1/§4.2 range rewrites (`time_filter`, `text_filter`, window and year ranges). | `docs/lang/QL_CONTRACT.md` §4, §6 |
| `lang/src/compile/graph_table.rs` | `GRAPH_TABLE` compilation to one bounded traversal. | `docs/lang/QL_CONTRACT.md` §4, §6 |
| `lang/src/compile/dml.rs` | `INSERT`/`UPDATE` documents and their declared-type values; the predicated `UPDATE ... WHERE` and `DELETE ... WHERE` (their filters, their `SET` values as constants or compiled row functions, and the EXPLAIN that PREPARES the candidate query without running it); the `FROM ALL` refusal text, written once for the three statements that raise it. | `docs/lang/QL_CONTRACT.md` §2, §4, §6 |
| `lang/src/compile/ddl.rs` | `CREATE TABLE`, `CREATE INDEX`, `ALTER TABLE` and `DROP TABLE` with their EXPLAINs. | `docs/lang/QL_CONTRACT.md` §4, §6 |
| `lang/src/functions.rs` | The §4.1 string and §4.2 date/time functions as PURE functions: the proleptic-Gregorian calendar, the literal reader and the ISO printer, `date_trunc`/`EXTRACT`/`to_char`, the fixed-width interval reader, and the text-prefix successor a `LIKE 'x%'` range needs. One implementation serves both the WHERE fold and the projected row function, so the two cannot disagree. | `docs/lang/QL_CONTRACT.md` §4.1, §4.2 |
| `lang/src/explain.rs` | `EXPLAIN`: the plan, the counters, and the two function sections (`range rewrites`, `row functions`). | `docs/lang/QL_CONTRACT.md` §6 |
| `lang/src/compile/bind.rs` | Parameters as typed SLOTS: `Binder` (the compiler's value reader with the compiler taken away, so one implementation serves a PREPARE and a REBIND), the `*Fill` slots a compiled node keeps beside the value it folded, and `Rebind` -- why a statement cannot be refilled, collected by the compile itself. | `docs/lang/QL_CONTRACT.md` §2 (the reusable `PreparedSql`) |
| `lang/src/refuse.rs` | The Tier-2/Tier-3 table as data, plus `MULTI_RANGE` -- the reason a rewrite whose pre-image is a set of ranges carries. | `docs/lang/QL_CONTRACT.md` §4, `docs/core/FOUNDATION_TEST_STANDARD.md` law 8 |

## `dist/src/service/` -- the embedded service surface

`docs/dist/OPS_CONTRACT.md` §1-§5, built here because an operator surface
belongs to `dist` and because composing `Database` handles is exactly what
this layer is for. `dist/src/cli/` (the operator binaries) is listed in
`docs/LAYERS.md`; `dist/src/pg/` has its own section below.

| Module | Atomic | Contract |
| --- | --- | --- |
| `dist/src/service/mod.rs` | `ServiceDatabase`: the single writer behind a `Mutex<Database>`, the published read view behind an `RwLock<Arc<Snapshot>>`, `WriterGuard` (the recorded write path and the commit barrier), `ServiceError`, the publish rate limit, the statement-timeout setter, and `scan`, which threads the deadline and the interrupt into every `prepare_sql_with` and `next_page` the service issues. | `docs/dist/OPS_CONTRACT.md` §1, §2, §3, §5 |
| `dist/src/service/snapshot.rs` | `Snapshot`: one published read-only handle, its publication ordinal, the instant it was swapped in and what minting it cost; `PUBLISH_INTERVAL_DEFAULT` (100 ms). | `docs/dist/OPS_CONTRACT.md` §1, §2 |
| `dist/src/service/interrupt.rs` | `InterruptHandle`: the cloneable `Arc<AtomicBool>` a second thread holds, sticky until cleared, one relaxed load per charge. | `docs/dist/OPS_CONTRACT.md` §4 |
| `dist/src/service/changes.rs` | The change feed: `ChangeEvent`, `ChangedKey`, `Receiver`, the per-subscriber bounded queue (`CHANGE_QUEUE_BOUND` = 256) and the per-event key cap (`CHANGE_KEY_CAP` = 1,024), the `PendingBatch` accumulator a commit drains and a rollback drops, and `Subscribers::deliver`, which never waits for a slow subscriber. | `docs/dist/OPS_CONTRACT.md` §5 |
| `dist/tests/service.rs` | One test per rule §1-§5 states, with the oracle held in the test process: the L6 reader, the measured publish window, the `Deadline` refusal and its elapsed microseconds, the cross-thread cancel, one event per commit and none on rollback, the counted `lagged` drop, and the second-writer refusals. | `docs/dist/OPS_CONTRACT.md` §1-§5, `docs/core/FOUNDATION_TEST_STANDARD.md` L3, L4, L6 |

## `dist/src/pg/` -- the PostgreSQL wire protocol

`docs/dist/WIRE_CONTRACT.md` over `docs/dist/OPS_CONTRACT.md` §9, built here
because the surface a stock PostgreSQL client connects to is a distribution
surface. It adds no execution: every statement compiles through
`sekejap_lang` and runs on the engine's own atomics over `dist/src/service/`.

| File | What is in it | Contract |
|---|---|---|
| `dist/src/pg/connection.rs` | `Connection`, the whole protocol engine, SANS-IO: `feed(&[u8]) -> Vec<u8>`, the startup exchange, the simple and extended query flows, portals with row limits, the `DECLARE`/`FETCH`/`MOVE`/`CLOSE` cursors and the two ceilings a HELD answer is bounded by (`CURSOR_ROW_CAP`, `CURSOR_BYTES_CAP`), the transaction block over `WriterGuard`, the session GUCs, `LISTEN` over the change feed, `BackendKey` and `CancelToken`, and the named refusal `NOTIFY_REFUSAL`. | `docs/dist/WIRE_CONTRACT.md` §1, §2, §4, §5, §6, §7; `docs/dist/OPS_CONTRACT.md` §9; `docs/lang/QL_CONTRACT.md` §2 |
| `dist/src/pg/frames.rs` | The BYTES and nothing else: every backend and frontend message type, the framing writers, `FieldDescription`, `TransactionStatus`, and the `Reader` whose every COUNT is checked against `remaining()` before it is believed. | `docs/dist/WIRE_CONTRACT.md` §1 |
| `dist/src/pg/types.rs` | The `pg_type` OID table (`oid::*`, including the two SYNTHETIC ones for `geometry` and `vector`), `type_size` / `type_name`, the declared-spelling and `Kind` mappings, the text and binary encodings of one cell, `$n` decoding, and the SQLSTATE map (`wire_error`). | `docs/dist/WIRE_CONTRACT.md` §3, §8; `docs/dist/PG_SURFACE.md` |
| `dist/src/pg/server.rs` | The transport: `bind` (which refuses a non-loopback address without `allow_remote`), `serve` over `std::thread::scope`, `Backends` (the `(pid, secret)` registry a `CancelRequest` is routed through), `Shutdown`, and `SecretStream`. | `docs/dist/WIRE_CONTRACT.md` §1.1, §4, §9 |
| `dist/src/cli/pg_server.rs` | The `sekejap-pg` binary: arg parsing, one `ServiceDatabase` for the process, the publish interval set to zero so a session reads its own writes. | `docs/dist/WIRE_CONTRACT.md` §0, §7 |
| `dist/tests/pg_wire.rs` | The bytes, a frame at a time, with every frame built in the test. | `docs/dist/WIRE_CONTRACT.md` §10 |
| `dist/tests/pg_server.rs` | END-TO-END: the real binary on a free localhost port, driven by the `postgres` crate and by `psql`. | `docs/dist/WIRE_CONTRACT.md` §10 |

## `dist/rust/src/` -- the published crate

`docs/dist/RUST_API.md`, built here because a crate an application depends on
is exactly what the outermost layer is for. It adds no execution: every item is
a composition of calls `core`, `lang` and `dist` already export.

| Module | Atomic | Contract |
| --- | --- | --- |
| `dist/rust/src/lib.rs` | The crate root: the layer re-exports, `Mode`, `Addr` (a row's collection and key as ONE argument), `Document`, `Storage`, and the names re-exported so a caller needs no second dependency (`Config`, `FieldKind`, `EntityId`, `Direction`, `IndexFamily`, `SqlValue`, `Param`, `SqlError`, `Tier`). | `docs/dist/RUST_API.md` |
| `dist/rust/src/db.rs` | `Db` and `Tx`: the two backings (`Mutex<Database>`, `ServiceDatabase`), the read and write paths every call funnels through, durability per call, the document round trip through `_key`, and the three `scan_count_*` walks. | `docs/dist/RUST_API.md` §1-§7 |
| `dist/rust/src/rows.rs` | `Rows`, `Row` and the two conversions: a `serde_json::Value` into a `Param` by the stated rule, and a `SqlValue` back into JSON with MISSING omitted rather than nulled. | `docs/dist/RUST_API.md` §3 |
| `dist/rust/src/scan.rs` | `Scan`: one collection in id order, one page of rows held at a time. | `docs/dist/RUST_API.md` §2 |
| `dist/rust/src/catalog.rs` | `Collection`, `Field`, `Index`: the catalog as data. | `docs/dist/RUST_API.md` §5 |
| `dist/rust/src/error.rs` | `Error` and `Result`: one error type, with a refusal that carries both what was asked for and why there is no atomic. | `docs/dist/RUST_API.md` §8 |
| `dist/rust/src/plans.rs` | The bounded prepared-plan cache (`PlanCache`, `CacheStats`, the three ceilings fixed at open) and `Statement`, the statement a caller prepares by hand: parsed at `Db::prepare`, compiled on its first bind, rebound after. | `docs/lang/QL_CONTRACT.md` §2; `docs/dist/RUST_API.md` §3 |
| `dist/rust/tests/api.rs` | One test per section of `RUST_API.md`, against a `BTreeMap`/`BTreeSet` oracle held in the test process. | `docs/dist/RUST_API.md`, `docs/core/FOUNDATION_TEST_STANDARD.md` L1, L4, L8 |

## `dist/ffi/` -- the C ABI

`docs/dist/C_ABI.md`, built here because the surface a foreign runtime links
is the outermost layer's, and kept a separate crate because a `cdylib` and the
published `rlib` are different artifacts with different consumers. It depends
on `sekejap` and `serde_json` and on nothing else.

| Module | Atomic | Contract |
| --- | --- | --- |
| `dist/ffi/src/lib.rs` | The whole `extern "C"` surface, 59 functions: the four opaque handles (`SekejapDb`, `SekejapStmt`, `SekejapTx`, `SekejapScan`), the thread-local error slot and its total mapping from `sekejap::Error` onto the closed `SekejapStatus`, the four `catch_unwind` guards that keep a panic off the boundary, the JSON encode and decode for documents, parameters, rows, the catalog, the store configuration and the change event, and the four symbols kept as REFUSALS so a caller reaching for them gets a name and a reason. | `docs/dist/C_ABI.md`; `docs/dist/FFI_CONTRACT.md` §0, §2, §8 |
| `dist/ffi/build.rs` | Regenerates `include/sekejap.h` from the surface with cbindgen on every build; a cbindgen failure warns and the committed header stays authoritative. | `docs/dist/C_ABI.md` §5 |
| `dist/ffi/cbindgen.toml` | The header preamble -- the ownership rules and the sentinels a C reader sees first -- and the C99 style the generator emits. | `docs/dist/C_ABI.md` §1 |
| `dist/ffi/include/sekejap.h` | The generated header, committed as the artifact a consumer includes and as the fallback when cbindgen is not installed. | `docs/dist/C_ABI.md` §4 |
| `dist/ffi/Makefile`, `dist/ffi/sekejap.pc.in` | Build, `check` (compile and RUN `examples/smoke.c`), and install the library, the header and the pkg-config file a consumer resolves them with. | `docs/dist/C_ABI.md` §5 |
| `dist/ffi/examples/smoke.c` | The shortest C program that opens, declares, writes, queries, links, counts and closes, with every check reported and a non-zero exit on a failure. Run by `make check`. | `docs/dist/C_ABI.md` |
| `dist/ffi/tests/abi.rs` | The ABI exercised through its C signatures with `CString`/`CStr`: the round trip against a `BTreeMap` oracle, the JSON shapes, every error path's sentinel AND code, the scan at three page sizes, the prepared rebind, commit and rollback, the service calls refused in single mode, ten thousand strings taken and freed, and one handle serving two threads. | `docs/dist/C_ABI.md`, `docs/core/FOUNDATION_TEST_STANDARD.md` L1, L4, L8 |

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

**A new CATALOG RELATION (a `db_*` row or a `pg_catalog` view).**
1. Its column list, with one PostgreSQL type OID per column, goes in
   `lang/src/catalog.rs` beside the others, and the relation goes in
   `RELATIONS` with the schema a statement may qualify it with.
2. Its builder goes in `catalog::build`, as an arm keyed on
   `(schema, name)`. It reads `Snapshot`, which reads the catalog ONCE; a
   relation that needs a reader the snapshot does not have adds it there, not
   in the arm, so one statement pays each reader once.
3. If the relation is NOT catalog-bounded, its cap is a named constant in the
   same file and a truncated answer returns a NOTICE that says it stopped.
   `db_edges` and `EDGE_SHAPE_SEEKS` are the one example.
4. Its columns and its OIDs go in `docs/dist/PG_SURFACE.md` §8, and the
   statements a client writes against it in §10.
5. A `pg_catalog` relation sekejap will NOT have goes in `catalog::NOT_PROVIDED`
   and in `lang/src/refuse.rs` instead, so a `SELECT` naming it is refused by
   name rather than answered with an empty set that reads as a fact.

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

**A bounded, resumable WRITE over a candidate set.**
1. The pass goes in `core/engine/src/collections/write_set.rs`, beside
   `delete_where` / `update_where`: prepare an ordinary query with
   `Projection::Ids` and `QueryOrder::Driver`, take ONE page, materialise it
   as ids, drop the prepared query, then write. The page must be materialised
   before the first write (Law 3) and the prepared query must be dropped
   before the writes, because it holds `&Database`.
2. Its bound goes on `QueryBudget` in `core/engine/src/query/mod.rs` beside
   `rows_written`, with a `QueryWork` field, a `WorkResource` variant and an
   arm in `WorkMeter::slot`.
3. Its resume point is `WriteCursor` (`core/engine/src/query/mod.rs`), set on
   a fresh prepared query by `PreparedQuery::resume_from` and read back by
   `PreparedQuery::write_cursor` in `core/engine/src/query/page.rs`.
4. If the write can MOVE the key the driver walks, the preflight that refuses
   it goes beside `refuse_a_patch_that_moves_its_driver`, and the refusal
   names the index and the explicit driver that is stable.

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
