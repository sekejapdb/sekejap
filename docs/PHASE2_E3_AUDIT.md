# E4 Phase 2: bounded E3 and consumer source audit

2026-09-16. Read-only source inspection; no benchmark or runtime correctness claims. Paths below relative to `<home>/` unless stated. No AGENTS.md found in E3/app trees or workspace ancestors; read E4 AGENTS.md. E3 source is inspiration, not a legacy file compatibility target.

## Main recommendation

Keep the frozen E4 page/WAL/typed-record foundation. Port small algorithms behind E4 transactions, snapshots, budget and versioned catalog; do not transplant E3's Store/Graph persistence machinery or all of its 12K-line Db facade. First contract must distinguish authoritative records/edges from rebuildable postings, and namespace every persisted index by collection + field(s) + family + version/generation. Existing released index versions must remain usable across minor upgrades; adding version fields alone does not prove this.

## Reusable pieces and their boundaries

| Capability | Concrete source | Useful behavior / caution |
|---|---|---|
| Graph | `sekejap-e3/core/kernel/src/graph.rs:219` add_edge, out_edges/in_edges; :1235 bfs; `core/kernel/src/keys.rs` | Typed adjacency ranges `(src,type,dst)`, reverse mirror, separate perspective namespaces. Graph ctx=0 has compact keys; nonzero ctx uses separate tags. E4 should atomically maintain forward/reverse postings and entity deletion. Define multiedge identity and dangling-edge policy before shipping. |
| Scalar | `sekejap-e3/src/db.rs:51` OrdF64, :75 FieldKey, :111 json_numbers_cmp; `core/kernel/src/keys.rs` fieldidx_key | Ordered numeric/string index encodings; exact integers outside f64 precision retained, -0/+0 equivalence. Port independent boundary tests; don't collapse u64/i64 into f64 or confuse null with absent. `src/scalar.rs` is SQL scalar functions, not index implementation. |
| Vector | `core/kernel/src/graph.rs:345` set_vec, :583 nearest; `core/kernel/src/vecquant.rs`; `core/kernel/src/nav.rs` | Raw f32 vectors separate from small codes/navigation, field-scoped dimension/recipe, exact bounded-top-k rescore; approximate retrieval must expose recall/effort. `src/vector.rs:38` HnswGraph is an unreachable compatibility stub: NOT a reusable HNSW implementation. Disk navigation is in kernel/nav.rs. |
| Spatial | `core/kernel/src/spatial.rs:33` Hilbert; :123 cover_cells; :293 Geom; `core/kernel/src/geomath.rs`; `src/geo.rs` | Typed WGS84 geometry, GeoJSON lon/lat; bounded multilevel cell postings, outward rounded bbox to avoid false negatives, exact refinement on geometry. Port axis order/radius-units, holes, dateline, near-pole and invalid-coordinate tests; bbox-only answer is insufficient. |
| Full text | `core/kernel/src/text.rs:30` tokenize; :2107 replace_text; :2185 delete_text; :2669 text_search; :2801 fold_text; `src/text_index.rs` | Mutable head plus immutable packed postings; per-field corpus stats and document generations/tombstones; BM25 K1=1.2 B=.75; prefix/fuzzy helpers. Must prove delete/reinsert/fold/reopen equivalence and actual bounded memory. |
| Combined queries | `core/kernel/src/score.rs:33` ScoreExpr; `src/query.rs:303` Step; `src/sql.rs`; `src/exec.rs` | Candidate retrieval separate from scoring. Hybrid score point-evaluates BM25/vector/geo for supplied IDs, deterministic ID tie-break, bounded top-k. Step represents scalar filters, typed graph traversal, spatial, vector, projection/order/limit. Reuse semantic inventory before attempting wholesale parser/executor port. |
| Management API | `src/catalog/mod.rs`; `src/db.rs` | SHOW TABLES/INDEXES, information_schema/pg_indexes, read-only mode, prepared inserts, cancellation, export/import, explicit commit/error behavior matter to app. They are API work above atomic storage. |

## Namespace/collision hazards (from current source)

E3 tags in `core/kernel/src/keys.rs`: catalog00, node01, label02, edge03, reverse04, vector05, external06, perspective-edge08/09, property0A, vector-code0B, text0C, text-norm0D, text-meta0E, spatial0F, geometry10, nav11, SQL metadata12; further scalar/search keyspaces follow. Do not copy tag values into E4 without checking its reserved vocabulary.

`src/db.rs:32` uses seahash for externally visible slug_hash and hashed identities; `put_row` stores node by hash(slug), collection membership separately (:4154). E4 must decide collection-local keys versus globally named graph identity explicitly; a hash is not collision-proof identity. Avoid accepting embedded-NUL names if NUL-separated hashing is reused without length encoding.

**Specific current source hazard:** `src/db.rs:5719 index_reg_hash_for` uses `index_reg_hash("\\x01vector", field)` for vector/vamana and ignores collection, while `vector_field_id` (:9920) correctly hashes vector+collection+field. Registration caller :10090 passes ordinary collection/field. This is an apparent persistent registry collision for equal vector field names across collections, even though data postings are scoped correctly. No runtime reproduction performed. E4's catalog should never encode identity this way.

`SQLMETA_COLLECTION_GENERATION=7` at :29 and `index_build_key` at :5732 both use SQL metadata kind7 with differently derived hashes: another reason to allocate distinct semantic namespaces instead of sharing a hash domain. Scalar build/drop use a shared registry helper; E3 history includes stale definitions resurrecting indexes on reopen.

E3 `DEFECTS.md` is historical, not a list of proven still-open bugs: current BM25 field IDs and geometry field selection already differ from it. Its failure scenarios are valuable regression tests, not current defect claims.

## Real consumer evidence

app `Cargo.toml:76` currently declares `sekejap="0.16.5"`, not a local E3 dependency. Therefore E3 API reuse alone does not prove current app compatibility. `app/src/platform/sekejap.rs:1439` execute_sql dispatches execute/execute_params for writes and query paths for reads, enforces read_only, exposes affected rows, renders Hit.payload into SQL result rows. It uses CoreDB::open, schema introspection, collection access, maintenance sync/compact and typed FieldType including JSON/GEO/VECTOR. Keep `$1` bound JSON/vector parameters, named fields, numeric precision and explicit errors in compatibility fixtures.

app files are from the **2026-09-15 PVC backup**, not verified live deployment state:
`<scratch>/`.

`pipelines/experiments/retrieval/e1/tools/e1-tool-ret-search-data-and-map.zf.json` actually uses:

```sql
SELECT data_resource_id FROM e1_gold_data_resource
ORDER BY description_embedding <=> $1 ASC LIMIT 50

SELECT search_hook_id, content, target_id FROM e1_gold_search_hook
ORDER BY embedding <=> $1 ASC LIMIT 12

SELECT data_resource_id, label, description, spatial_granularity,
       is_regional_admin_boundary,
       BM25(description, 'query') AS bd, BM25(keywords, 'query') AS bk
FROM e1_gold_data_resource /* plus dynamic facet filters */
ORDER BY bk DESC LIMIT 200
```

Pipeline combines lexical results, description-vector ranks, hook-vector ranks and resource/map associations in JavaScript. This is a useful native hybrid-query target; current score combines keyword BM25 + adjusted description BM25*0.4 + reciprocal resource-vector rank*0.8 + hook rank*0.6, with facets applied. Do not pretend that copying this scoring formula establishes retrieval quality.

Schemas `schemas/sekejap/tables/e1_gold_data_resource.json` and `e1_gold_search_hook.json` declare vector-typed attributes, but top-level vector_fields/fulltext_fields are empty; `_key` is the only listed hash index. A declared VECTOR payload is not proof a persistent accelerator exists.

`pipelines/app/functions/e1/management/e1-management-profile-manual-catalog-resource.zf.json` description explicitly says it avoids numeric JSON arrays because Sekejap confuses ordinary JSON with VECTOR. E4 must keep numeric JSON arrays as JSON unless explicitly typed vector. This is a concrete consumer regression fixture.

app help `src/platform/help/db/sekejap.md` documents graph `SELECT ... FROM MATCH (u:users)-[:follows]->(friend:users)` and variable-length `[:caused_by*1..5]`; `$1` parameters; schema index families; `_key`. It also documents numeric-only ranges and missing DDL constraints/defaults in the current installed API. Treat these as documented consumer limitations, not desirable E4 semantics. Precise E4 guarantees require executor tests, not inheriting this help text. `docs/lang/QL_CONTRACT.md` §1 has since settled this: e3's `FROM MATCH` is explicitly not adopted, so this help text is consumer-migration context only, not an open question.

## Minimum new regression/acceptance fixtures

1. Two collections reuse the same field/index names with different vector widths and text corpora; build/drop/reopen one leaves other untouched. Embedded delimiter/NUL names handled explicitly.
2. Mixed typed entity: scalar extremes, nested JSON and ordinary numeric arrays, explicit vector, named geopoint; two spatial/vector fields in one entity; timestamps off by default.
3. One transaction changes entity plus all index families/edge mirrors; injected failure yields wholly before or after; read snapshot never sees mixed versions; failed dimensions/geometry validation writes nothing.
4. Update/delete/reinsert changes candidates, counts and text corpus stats; reopen/fold/rebuild agrees with independent full-scan oracle; dropping an index cannot resurrect it.
5. Exact scalar/graph/geometry tests versus independent expected results; vector exact top-k versus brute force; ANN recall separate from timing. Combined graph+facet+text+vector/spatial results with deterministic ties and explicit candidate budget.
6. Unknown catalog/index version refusal without modifying source; old readers/writers behavior pinned; query/open never silently rebuilds an index. Explicit rebuild has temporary disk accounting, cancellation, atomic publication and recovery.
7. Resource-constrained Linux comparison: initial load, separate update/delete/insert passes and mixed churn; total live + temporary + WAL disk high-water, final size, time, memory. SQLite scalar/JSON/transaction baseline is valid; don't label core SQLite as native-vector/graph comparator without documenting extension/algorithm.

No code was modified and no production/backup data was opened by a database engine.
