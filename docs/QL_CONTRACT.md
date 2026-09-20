# Query language contract — sekejap-e4 Phase 3 (draft for owner edit)

Drafted 2026-09-20. Companion to docs/GRAPH_CONTRACT.md (engine semantics).
This document fixes WHAT the language is: the specifications adopted, every
keyword and function in one of three tiers, the dialect deviations, and the
execution guarantee behind each construct. Lean by design: a construct is in
Tier 1 or 2 only if it compiles to a named atomic with a stated cost.

Tiers: **T1** compiles to an atomic that exists at HEAD fe0db1a. **T2** a
Phase-3 item with the atomic named, in build order. **T3** refused with the
reason in the error text; never emulated silently.

## 1. Specifications adopted

| layer | specification | dialect reference |
|---|---|---|
| container | ISO/IEC 9075:2023 SQL | PostgreSQL 19 documentation; where ISO and Postgres differ, Postgres wins |
| graph | ISO/IEC 9075-16:2023 SQL/PGQ, `GRAPH_TABLE ... COLUMNS` | PostgreSQL 19 ch. 5.15; Oracle 23ai GRAPH_TABLE; patterns per GPML (Deutsch et al., SIGMOD 2022, arXiv 2112.06217), shared with ISO/IEC 39075:2024 GQL |
| spatial | PostGIS 3.6 function names and unit semantics | docs/SPATIAL_FUNCTIONS.md, tests/spatial_postgis_conformance.rs |
| vector | pgvector 0.8 type and operators; pgvectorscale knobs where they map | tools/battle50k_pg_cases.sql |
| text | PostgreSQL tsvector/tsquery for boolean matching; BM25 ranking; search functions in the e1 family (typo, prefix) | src/index/text/mod.rs |
| catalog | `db_*` core rows; `pg_catalog` and `information_schema` as views | p2-catalog-core, p3-pg-surface |

Not adopted: standalone GQL statements, Cypher, AQL, SurrealQL, e3's `FROM
MATCH`, Google's `RETURN` inside GRAPH_TABLE (accepted as an alias for
`COLUMNS` only if measured demand appears).

## 2. Statements

| statement | tier | atomic / note |
|---|---|---|
| `SELECT ... FROM <collection> [WHERE] [ORDER BY one expr] [LIMIT]` | T1 | prepare_query: filters AND, one order, pages |
| `SELECT ... FROM GRAPH_TABLE (...)` | T1 (patterns per §6) | traversal driver/filter |
| `INSERT INTO t (...) VALUES (...)`, `$n` params | T1 | put by key |
| `UPDATE t SET ... WHERE key = $1` | T1 | put replaces the row; partial update = read-modify-put |
| `DELETE FROM t WHERE key = $1` | T1 | delete by key; RESTRICT/CASCADE per graph contract 6.1 |
| `INSERT INTO GRAPH g EDGE type (source, destination, props...) VALUES` | T2 (spelling open) | put_edge |
| `UPDATE GRAPH g EDGE type SET ... WHERE source = AND destination =` | T2 | edge posting rewrite |
| `DELETE FROM GRAPH g EDGE type WHERE ...` | T2 | delete_edge |
| `CREATE TABLE`, `CREATE INDEX ... USING {btree,gin,gist,exact,quantized,adjacency}`, `DROP INDEX [IF EXISTS]` | T1 | catalog descriptors |
| `DROP TABLE [IF EXISTS] name [CASCADE\|RESTRICT]` | T1 | `begin_drop_collection` publishes a DROPPING mark the readers refuse, then `drop_collection_step(id, budget)` empties the indexes, sidecars, rows and mappings in bounded batches and removes the descriptor last; RESTRICT per graph contract 6.1 |
| `CREATE SCHEMA`, `schema.table` | T2 | p2-schema-segment |
| `CREATE PROPERTY GRAPH name NODE TABLES (...) EDGE TYPES (...)` | T2 | optional naming of a context + label map; nothing is built |
| `BEGIN [READ ONLY]`, `COMMIT`, `ROLLBACK` | T1 | one writer, snapshot readers |
| `DECLARE c BINARY CURSOR FOR ...`, `FETCH FORWARD n`, `CLOSE` | T2 | pages over prepare_query (p3-wire) |
| `EXPLAIN` | T2 | prints the plan: driver, membership sets, order, work counters |
| `SELECT version()`, `postgis_version()`, `current_schema()` | T2 | fixed rows (p3-pg-surface) |
| `WITH name AS (SELECT ...)` non-recursive | T2 | materialised once, bounded by QueryBudget rows |
| `UNION`, `WITH RECURSIVE`, `CREATE VIEW` (user), triggers, window functions | T3 | no atomic; recursion is a GRAPH_TABLE pattern |

## 3. Predicates and operators (WHERE)

| construct | tier | atomic |
|---|---|---|
| `AND` | T1 | filter conjunction |
| `=, <>, <, <=, >, >=` on indexed scalar | T1 | Scalar Eq/Range |
| `BETWEEN a AND b` | T1 | Range |
| `IS NULL`, `IS NOT NULL`, `IS MISSING` | T1 | Scalar IsNull/IsMissing |
| `key BETWEEN`, `key >=` (external key) | T1 | Key filter, key-order driver |
| `OR` on the same index, `IN (list)` | T2 | union of ranges as one membership set |
| `NOT` | T2 | complement over a membership set; row path otherwise |
| `OR` across indexes | T2 | union of two membership sets |
| `EXISTS (subquery)`, `IN (subquery)` | T2 | membership set from the subquery (semi/anti join) |
| `LIKE 'abc%'` | T2 | text-key prefix range |
| `LIKE '%abc%'`, `ILIKE` | T2 | trigram index family (pg_trgm-compatible), new family under a feature bit; without the index: full scan, cost printed by EXPLAIN |
| `SIMILAR TO`, regex `~` | T3 | no index atomic |

## 4. Functions

### 4.1 String (Postgres names)

| function | tier | execution |
|---|---|---|
| `lower`, `upper`, `length`, `concat`, `||`, `substring`, `left`, `right`, `trim`, `split_part`, `replace`, `position`, `starts_with` | T2 | row functions on projected values; `lower(col) = x` and `starts_with` rewrite to an index range when an expression index `lower(col)` exists |
| `ILIKE` | T2 | see §3 (trigram) |
| `regexp_*` | T3 | no atomic |

### 4.2 Date/time (Postgres names; storage = Int microseconds, declared TIMESTAMPTZ / DATE)

| function | tier | execution |
|---|---|---|
| `EXTRACT(YEAR|MONTH|DAY|DOW|HOUR FROM t)`, `date_trunc('unit', t)` in WHERE | T2 | rewritten to one or more scalar Ranges (index-usable) |
| same in SELECT / GROUP BY | T2 | row function; GROUP BY streams when the index order equals the truncation order |
| `now()`, `current_date`, `interval` arithmetic, `age(t)`, `t + interval` | T2 | constants folded at prepare; row arithmetic |
| `to_char(t, fmt)`, `to_timestamp`, `to_date` | T2 | row functions |
| time zones other than UTC storage | T3 | declared TIMESTAMPTZ is stored UTC; display conversion only |

### 4.3 Graph (SQL/PGQ names)

| function / construct | tier | atomic |
|---|---|---|
| element pattern `(v IS label)` / `(v:label)`, edge `-[e IS type]->`, `<-`, `-` | T1 | direction + type on BFS |
| inline `WHERE` in element (per-hop prune) | T2 | graph contract 4.3 |
| `{n,m}`, `{n,}`, `+`, `?` quantifiers | T1 | min/max depth |
| label alternation `type1|type2` | T2 | multi-type hop (two ranges per hop) |
| post-pattern `WHERE` | T1 | post-filter on rows |
| `COLUMNS (expr AS name)` | T1 | projection |
| `path_length()` | T2 | frontier depth accumulator |
| `path_sum/product/min/max/avg/first/last(e.prop)` | T2 | accumulators (graph contract 5.1) |
| `p = ...`, `nodes(p)`, `edges(p)` | T2 | path rebuild for returned rows |
| `ANY SHORTEST`, `ALL SHORTEST` | T2 | shortest-path atomic, unweighted |
| `VERTEX_ID(v)`, `EDGE_ID(e)` | T2 | entity id; edge id (after element identity) |
| `IS ACYCLIC` (default), `TRAIL`, `WALK`, `SIMPLE` | T1 default only; others T3 | contract 4.1 |
| negative patterns, `NOT EXISTS { pattern }` | T3 | |

### 4.4 Spatial (PostGIS names; the essentials, all already atoms unless marked)

| function | tier | atomic |
|---|---|---|
| `ST_DWithin(geog, geog, m)` | T1 | Point Radius / Geometry DWithin (spheroidal) |
| `ST_Intersects`, `ST_Within`, `ST_Contains` | T1 | Geometry filters (units per docs/SPATIAL_FUNCTIONS.md) |
| `&&` with `ST_MakeEnvelope` | T2 | Bbox filter on point/geometry index (p3-geometry-io) |
| `<->` (kNN, `ORDER BY loc <-> pt`) | T1 | Distance order (point index) |
| `ST_Distance`, `ST_Area`, `ST_Length`, `ST_Perimeter`, `ST_Centroid`, `ST_Covers`, `ST_Crosses` | T1 as row functions | spatial_geometry pub fns |
| `ST_AsBinary`, `ST_AsEWKB`, `ST_GeomFromWKB(bytea, srid)`, `ST_GeomFromEWKB`, `ST_AsText`, `ST_GeomFromText`, `ST_AsGeoJSON`, `ST_GeomFromGeoJSON`, `ST_MakePoint`, `ST_SetSRID`, `ST_SRID` | T2 | pure I/O functions; SRID per column (p3-geometry-io) |
| `ST_Simplify`, `ST_SnapToGrid`, `ST_RemoveRepeatedPoints` | T2 | pure functions (QGIS render path) |
| `ST_Transform` | T2 (later) | PROJ; storage stays WGS84 |
| `ST_Buffer`, `ST_Union`, `ST_Intersection`, `ST_Difference`, `ST_SimplifyPreserveTopology` | T3 | GEOS overlay; no pure-Rust substitute accepted |
| raster, topology, `ST_AsMVT` | T3 | |

### 4.5 Vector (pgvector / pgvectorscale)

| construct | tier | atomic |
|---|---|---|
| `VECTOR(n)` type, `'[...]'::vector` literal | T1 | Kind::Vector |
| `<=>` cosine, `<->` L2, `<#>` negative inner product | T1 | ExactVector / ApproximateVector order (Cosine, SquaredL2, NegativeDot) |
| `ORDER BY emb <=> $v LIMIT k` | T1 | exact (page-order scan) or quantized (ef) by index choice |
| `SET LOCAL ef_search = n` (pgvector) / `diskann.query_search_list_size` (pgvectorscale) | T2 | maps to `ef` |
| `USING exact (emb)`, `USING quantized (emb vector_cosine_ops)` | T1 | index families |
| `USING hnsw`, `USING diskann`, `USING ivfflat` | T2 | accepted as aliases of `quantized` with a notice; no new family |
| distance as a filter `emb <=> $v < 0.3` | T2 | approximate membership set (ef-bounded) |
| `vector_dims`, `vector_norm`, `l2_normalize` | T2 | row functions |
| `<+>` L1, halfvec, sparsevec, binary quantization ops | T3 | |

### 4.6 Text search

| construct | tier | atomic |
|---|---|---|
| `to_tsvector('simple', col) @@ to_tsquery('simple', 'a | b')` / `'a & b'` / `'"a b"'` | T1 | Text filter Any / All / Phrase (analyzer v1) |
| `ORDER BY ts_rank_cd(...)` | T1 | Bm25 order (formula differs from Postgres; documented) |
| `bm25(col, 'query')` as an expression | T1 | Score leaf |
| `websearch_to_tsquery`, `plainto_tsquery` | T2 | parsers onto the same filter |
| `search(col, 'query')` typo-tolerant, prefix on the last token (e1 family; Meilisearch-class) | T2 | term-dictionary prefix range + bounded Levenshtein automaton over the dictionary; new atomic, no format change |
| `highlight`, `ts_headline` | T2 | row function |
| multi-field text index | T2 | index over a concatenated stored field today; declared multi-field later |
| language stemming beyond 'simple' | T3 | analyzer v1 is language-neutral |

### 4.7 Aggregates

| construct | tier | atomic |
|---|---|---|
| `count(*)`, `count(col)`, `sum`, `min`, `max`, `avg` | T1 | `Database::prepare_aggregate` (`src/query/aggregate.rs`): STREAMING when the group key is the driving scalar index's own value (groups contiguous, one accumulator set alive, the key off the posting with no row read, a page stops and resumes at a group boundary); HASHED otherwise, bounded by the `groups` QueryBudget resource. No spill: past the cap the page is `BudgetExceeded { groups }`. An accumulator's input is the driving posting's value where the field IS that index, and the primary row otherwise — charged as `primary_reads` and printed by EXPLAIN |
| `GROUP BY`, `HAVING`, `DISTINCT` | T1 | same atomic. `DISTINCT` is a group with no accumulators; `HAVING` is a predicate on a FINISHED group's accumulator values, applied before paging. `GROUP BY col / n` is accepted only where it is computable index-side from an Int posting (truncating division by a positive divisor is monotone in that index's own order, so the groups stay contiguous); without such an index the expression form is refused and `GROUP BY col` with a range filter is the spelling. One key only: a composite key has no atomic |
| `ORDER BY <aggregate alias>` (+ `LIMIT`) | T1 | the finished groups are sorted before they are paged. THE ONE PLACE A SORT OVER MEMORY HAPPENS in this engine, and it is bounded because what it sorts is the group table, which the `groups` budget has already bounded. It forces the hashed shape: no group's value is final before the walk ends |
| `array_agg`, `string_agg`, `json_agg` | T2 (after the above) | bounded by row budget |
| `count(DISTINCT col)` | T2 | a per-group distinct set is a second unbounded structure inside each group; the bounded atomic here is one accumulator per group |
| `percentile_cont`, window functions, `GROUPING SETS`, `CUBE` | T3 | |

### 4.8 Joins (essentials only)

| join | tier | atomic / cost |
|---|---|---|
| `INNER JOIN b ON a.key = b.key` (key equality) | T2, after GROUP BY | key lookup per driving row (key-order driver); cost ∝ driving rows |
| `LEFT JOIN` on key equality | T2 | same, NULLs on miss |
| `USING (key)`, `NATURAL JOIN`, `RIGHT JOIN` | T2 | rewrites |
| `CROSS JOIN LATERAL (SELECT ... FROM GRAPH_TABLE ...)` | T2 | one bounded traversal per driving row |
| join on a non-key column, `FULL OUTER JOIN` | T3 | needs a hash join with spill |
| a pattern compiled to a join | never | graph contract: a hop is a posting range |

## 5. Dialect deviations (stated once, each with the reason)

1. The graph is native: `CREATE PROPERTY GRAPH` is optional and uses `EDGE TYPES`, not `EDGE TABLES`; no `SOURCE KEY ... REFERENCES` because endpoints are known from written edges.
2. Inline element `WHERE` prunes per hop by contract; the post-pattern `WHERE` filters completed matches. Both are standard syntax; the guarantee is ours.
3. `ORDER BY` takes one key; an expression is one key (the Score atomic). Two keys are a refusal.
4. `OFFSET` is a keyset continuation, never a skip count.
5. BM25 stands behind `ts_rank_cd`; the number differs from Postgres and the docs say so.
6. `USING hnsw|diskann|ivfflat` are aliases of the quantized family.
7. A join never executes a pattern; a relation between rows is an edge.
8. Declared TIMESTAMPTZ is stored as UTC microseconds in an Int; no time-zone storage.
11. A nullish group key (NULL or missing) sorts FIRST under `GROUP BY`, the scalar keyspace's own order; Postgres sorts NULL last and `NULLS FIRST|LAST` is refused. `HAVING` over an all-null accumulator drops the group (SQL three-valued logic); a `HAVING` over `min`/`max` of a non-numeric column is refused at prepare.
9. `DROP TABLE` is RESTRICT by default, and what restricts it is GRAPH EDGES, not foreign keys: Postgres refuses on a dependent constraint, this refuses while any edge in any context references a row of the table and names those contexts (graph contract 6.1). `CASCADE` removes those edges and nothing else -- it never reaches a second table's rows. A table with no edges on it drops under the default.
10. `DROP TABLE` is bounded and resumable, so it is not one transaction: the DROPPING mark is committed first and each bounded step after it is committed as it goes. An interrupted `DROP TABLE` leaves a collection that answers nothing and resumes from its committed cursor; it never leaves a half-emptied readable table. `ROLLBACK` does not undo a drop that has begun.

## 6. Execution guarantees the contract makes

- Every T1/T2 predicate on an indexed field is answered index-side (posting, membership set, or inline edge property); a row is read only for projection or for a predicate the plan names as row-bound. `EXPLAIN` prints which.
- Work is proportional to candidates walked or rows returned, never to the collection, except for constructs whose definition is a scan (exact vector order without a filter, `count(*)` without a filter), which `EXPLAIN` labels as scans.
- Memory per query is bounded by QueryBudget: pages, membership sets, groups (`WorkResource::Groups` — the accumulator sets an aggregate holds AT ONCE, one under the streaming shape and one per distinct group under the hashed one; its default ceiling is `RUN_BYTES` divided by what one group costs, applied even under `QueryBudget::unlimited`), frontier.
- Every T3 refusal names the missing atomic in its error text.

## 7. Order of Phase-3 work

1. Parser for §2 T1 + §3 T1 + §6 guarantees, with `EXPLAIN`.
2. Aggregates (§4.7) — DONE, `src/query/aggregate.rs`; then date/time and string functions (§4.1, §4.2).
3. `OR`/`IN`/`NOT`/`EXISTS` (§3).
4. Graph T2 (§4.3) in the graph-contract order.
5. Geometry I/O and `&&` (§4.4), catalog views and wire (p3-pg-surface, p3-wire).
6. Trigram index for `ILIKE` / infix `LIKE`; typo-tolerant `search()` (§4.6).
7. Key-equality joins (§4.8).
