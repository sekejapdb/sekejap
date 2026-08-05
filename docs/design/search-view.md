# `SEARCH INDEX` — embedded, multi-modal, graph-sourced search (design)

## Why
Collapse the operational-DB + search-DB(+ETL) stack (Sphinx/Solr/Elasticsearch/
Meilisearch/Typesense) into one embedded engine. Those tools make you (1) *assemble*
a denormalized search document and (2) *sync* it forever via a pipeline. A `SEARCH
INDEX` turns both into a declaration: describe the projection once; sekejap derives
and maintains the docs from the operational entities. No separate server, no ETL,
no document pushing.

Uniquely feasible here because relationships are **edges** — the search doc is a
`MATCH` traversal rooted at one collection, not a JOIN + CDC pipeline.

## What it is (not a new subsystem)
It is the **existing per-collection search index, extended in two dimensions**:
1. **Input** — from "fields of this one collection" → a **projection** that may pull
   fields from *related* collections via edges (and combine them via the generated-
   column string evaluator).
2. **Refresh** — the incremental `touch_search_index` gains **backward-edge
   propagation**: a change to `artist` touches every `song` whose doc used it.
The current single-collection `USING search` becomes the **0-hop special case**.

An inverted index is already a materialization; we widen what it ingests and when it
refreshes. No general "view"/"materialized view" concept is introduced.

## Multi-modal
The materialized doc holds fields of any type, each getting the matching sub-index:
- TEXT   → BM25 / positional search
- VECTOR → HNSW (semantic)
- GEO    → spatial grid
So one `SEARCH INDEX` unifies full-text + semantic + spatial + graph-sourced — all
local to one doc, one query, no traversal. (No competitor unifies graph-sourced
denormalization with text+vector+geo in one embedded index.)

## Surface — `MATERIALIZED VIEW` only (SGQL / Postgres-faithful, honest)
```
CREATE MATERIALIZED VIEW name [WITH (autoindex = true)] AS <SELECT … FROM MATCH …>;
REFRESH MATERIALIZED VIEW name;         -- explicit rebuild (SQL command)
```
- **No `SEARCH VIEW`, no auto-refresh.** DECISION (user, 2026-07-30): automatic refresh
  isn't embedded-native (it needs a background thread, which doesn't fit the single-threaded
  core). A `SEARCH VIEW` that *implies* "stays fresh automatically" would mislead — so it's
  omitted. The dirty-tracking + `refresh_stale_views` auto-machinery was **removed** (it added
  write-path cost for no embedded payoff and changed the write calculation).
- **`WITH (autoindex = true)`** — the auto-index convenience without the auto-refresh
  implication: auto-indexes search-type fields (TEXT→bm25, VECTOR→hnsw, GEO→spatial). Reads as
  a standard Postgres `WITH (…)` storage option.
- **Freshness is explicit** via `REFRESH MATERIALIZED VIEW` (SQL), exactly like Postgres. The
  app decides when to rebuild. Honest, predictable, zero background magic.
- SGQL: pure Postgres surface (`CREATE MATERIALIZED VIEW`, `WITH (…)`, `REFRESH`); `MATCH` = GQL.
The projection (shared by both forms):
```sql
  SELECT p._key       AS id,
         p.name       AS name,
         p.description AS description,
         dish.name    AS dishes,     -- pulled across (p)-[:serves]->(dish)  (GQL, inside MATCH)
         p.geometry   AS geometry,   -- GEO    → spatial
         p.embedding  AS embedding   -- VECTOR → HNSW
  FROM MATCH (p:place)-[:serves]->(dish:dish)
```

Base form — explicit indexing (Postgres-faithful):
```sql
CREATE MATERIALIZED VIEW place_search AS <projection>;
CREATE INDEX ON place_search USING bm25 (name, description, dishes);
CREATE INDEX ON place_search USING hnsw (embedding);
CREATE INDEX ON place_search USING spatial (geometry);
```

Preset form — batteries-included one-liner:
```sql
CREATE SEARCH VIEW place_search AS <projection>;   -- auto-indexes search-type fields + auto-refresh
```
Unlike Postgres, both are **auto-maintained** (eventual) — no manual `REFRESH`. That's
the one extended behavior; the syntax stays SQL-family.
Query (normalized weighted blend — all primitives already exist):
```sql
SELECT id, name,
       BM25_NORM(name,'grilled chicken')*0.4
     + BM25_NORM(dishes,'grilled chicken')*0.3
     + VECTOR_COSINE(embedding,[q])*0.2
     - ST_DISTANCE(geometry, POINT(115.168 -8.690))*0.0001  AS score
FROM place_search
WHERE SEARCH('grilled chicken') AND ST_DWithin(geometry, POINT(115.168 -8.690), 5000.0)
ORDER BY score DESC LIMIT 20;
-- or Meili-kind automatic:  SELECT * FROM place_search WHERE SEARCH('grilled chicken');
```

## Ranking: two modes over one index
- **Automatic tier** (Meili-kind): `SEARCH('q')` — default, normalized, sensible weights.
- **Custom formula** (Solr/Sphinx-kind): `BM25_NORM * w + VECTOR_COSINE * w - ST_DISTANCE * w`.
Both run on the same optimized index (no operational-data scan, no traversal).
Normalization is solved: `BM25_NORM` ∈ [0,1] (`s/(s+k)`), `VECTOR_COSINE` scoring form
is [0,1] similarity; distance signals (`ST_DISTANCE`, metres) are flipped (subtract/normalize).

## Maintenance (eventual)
On any write to node N: N is a root → mark its doc dirty; N is related → walk
`rev_edges` backward to affected roots → mark dirty; edge relinked → mark dirty.
Background pass rebuilds dirty docs. Near-real-time, like ES/Meili. No synchronous IVM.

## v1 guardrails
- Root + 1-hop related collections (multi-hop later).
- Eventual consistency only.
- Rebuild-per-root (no field-level diffing yet).

## Phasing
1. **Generated columns — DONE (2026-07-30).** `GENERATED ALWAYS AS (<expr>) STORED`
   in CREATE TABLE + ALTER TABLE ADD COLUMN. `GenExpr` evaluator: `||` concat,
   `concat_ws`, `lower`/`upper`, `coalesce`, field refs, literals. Computed at INSERT
   (overrides user value) + recomputed on UPDATE (forces slow path when a generated
   column exists). Indexable like any field. `||` tokenized as `Tok::Concat`.
   FieldDef gains `generated: Option<GenExpr>`. Tests: generated_column_* (3). The
   projection's combined-text fields reuse this evaluator.
2. **`CREATE MATERIALIZED VIEW … AS <SELECT … FROM MATCH …>`** (base) — DONE (2026-07-30,
   manual-refresh v1): parses via textual header (`parse_view_ddl`), materializes the
   projection into a derived collection (`materialize_view`), queryable + indexable like
   any collection; `REFRESH MATERIALIZED VIEW` re-runs. Combined-text columns via
   `concat_ws`/`||`/`lower`/`coalesce` over `var.field` in the MATCH SELECT (new
   `MatchAggReturn::Str(MatchStr)`). Cross-collection search proven (BM25 finds a song by
   its artist's name). `CompiledMutation::CreateView`/`RefreshView`; `CoreDB.materialized_views`.
   AUTO-REFRESH: DONE (eventual, v1) — data writes mark dependent views stale via
   `mark_all_views_dirty` (O(1), never rebuilds on the write path); `refresh_stale_views()`
   reconciles (drains the dirty set, rebuilds). Benchmarked: materialized search 48.8x
   faster than the live cross-collection query (73ms → 1.5ms), identical results (20k songs).
   MULTI-MODAL: DONE — GEO rides in the payload (materializes free; `USING spatial` works);
   VECTOR mirrored from the root collection's vector store into the view (`get_vector`→
   `put_vector` by field name; root collection extracted via `extract_root_collection`).
   SEARCH VIEW auto-indexes text→bm25, geo→spatial, vector→hnsw. Proven: one view answers
   BM25 + ST_DWithin + VECTOR_NEAR. (Vectors from RELATED collections, not just root, are a
   follow-up.)
   REMAINING: incremental rebuild (rebuild only backward-edge-affected roots, not full
   per-view — needs parsing the projection's edges for reverse-dependency resolution +
   scoped re-derivation; full-reconcile is correct meanwhile); view-def persistence across
   reopen; background refresh loop (serve calls refresh_stale_views on a timer).
2b. **`CREATE SEARCH VIEW`** — DONE: preset that auto-indexes the view's string fields
   (BM25) via `auto_index_view`. (vector/geo auto-index is the follow-up.)
3. **`CREATE VIEW`** (virtual) — completes the family; easy, useful as saved queries.
4. **Additive**: automatic ranking tier, typo tolerance (FST), multi-hop, field-level
   incremental rebuild.

## Already built (query side is real)
`BM25`, `BM25_NORM`, `SEARCH`, `SEARCH_SCORE`, `VECTOR_COSINE`/`_L2`/`_DOT`/`_L1`,
`ST_DWithin`, `ST_DISTANCE`, per-field bm25/search/hnsw/spatial indexes, graph
traversal + `rev_edges`. The new work is the projection input + cross-collection refresh.
