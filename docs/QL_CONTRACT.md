# Query language contract — sekejap-e4 Phase 3 (draft for owner edit)

Drafted 2026-09-20. Companion to docs/GRAPH_CONTRACT.md (engine semantics)
and docs/OPS_CONTRACT.md (the runtime and ops surface: service mode, publish,
statement timeout, cancellation, change notifications, introspection, bulk
load, write trace).
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
| `SELECT ... FROM ALL` (every collection at once) | T2 | a `Collections` concatenation driver over the catalog's collection ids in id order: each collection is one ordinary bounded driver walk, and the resume key is `(collection id, inner cursor)`. Work is proportional to the collections enumerated plus the candidates walked, and a `LIMIT` stops the concatenation at the collection it is reached in. `FROM ALL` with a ranked `ORDER BY` is **T3**: one order key across different layouts is not one key (the field may be absent, or a different `Kind`, in each collection), and merging N ordered walks holds one cursor per collection, which is no `QueryBudget` dimension. |
| `UPDATE t SET ... WHERE <any predicate>` | T2 | the prepared query's driver supplies candidates a page at a time and each one is read-modify-put, so work is proportional to the rows matched, never to the collection. Bounded by a `rows_written` `QueryBudget` dimension: over budget the statement is refused with the count it reached, never silently truncated. The walk reads the snapshot it started on, so an update never re-matches its own writes, and it resumes from the committed cursor. |
| `DELETE FROM t WHERE <any predicate>`, `DELETE FROM ALL [WHERE ...]` | T2 | the same driver walk feeding `delete`, with the graph contract 6.1 RESTRICT preflight per row and the same `rows_written` budget. `DELETE FROM ALL` rides the `FROM ALL` driver above and inherits its refusal for a ranked order. |
| `INSERT INTO GRAPH g EDGE type (source, destination, props...) VALUES` | T2 (spelling open) | put_edge |
| `UPDATE GRAPH g EDGE type SET ... WHERE source = AND destination =` | T2 | edge posting rewrite |
| `DELETE FROM GRAPH g EDGE type WHERE ...` | T2 | delete_edge |
| `CREATE TABLE`, `CREATE INDEX ... USING {btree,gin,gist,exact,quantized,adjacency}`, `DROP INDEX [IF EXISTS]` | T1 | catalog descriptors |
| `DROP TABLE [IF EXISTS] name [CASCADE\|RESTRICT]` | T1 | `begin_drop_collection` publishes a DROPPING mark the readers refuse, then `drop_collection_step(id, budget)` empties the indexes, sidecars, rows and mappings in bounded batches and removes the descriptor last; RESTRICT per graph contract 6.1 |
| `CREATE TABLE t (...) WITH (hash:[...], range:[...], fulltext:[...], bm25:[...], spatial:[...])` | T2 | sugar, no new atomic: one `create_collection` followed by one `CREATE INDEX` per named field inside the same statement. `hash` and `range` both map to `btree` with a notice (there is no separate hash family, and a `btree` answers equality); `fulltext` and `bm25` map to `gin`; `spatial` maps to `gist`. |
| column `DEFAULT now()`, `DEFAULT uuid4()`, `DEFAULT uuid5(namespace, name)` | T2 | a per-field default recorded in the collection descriptor under an additive feature bit (Law 8) and filled on the write path when the INSERT names no value for that column. The generator set is closed and each member is O(1) per row: one clock read, 16 random bytes, one hash over `namespace` and `name`. An arbitrary expression as a DEFAULT is T3 -- that is the generated-column row below, under its own keyword. |
| `GENERATED ALWAYS AS (expr) STORED` | T2 | the same descriptor slot holds a compiled row expression over other declared fields of the **same** row (the §4.1 / §4.2 row-function set), evaluated once the row is assembled and before the index-maintenance hook, so an index over a generated column is maintained exactly like an index over a written one. An expression that reads another row, an aggregate, or a subquery is T3: those are not row functions and have no per-write atomic. |
| `NOT NULL` on a column | T2 | a per-field flag in the same descriptor slot as the default, checked when the row is assembled: a value that is MISSING or NULL (e4 distinguishes them) refuses the write and names the column. `ALTER TABLE ... ADD COLUMN ... NOT NULL` with no `DEFAULT` on a non-empty collection is T3: every existing row would read MISSING, so the constraint is false the moment it is recorded. Add the column with a default, or add it nullable and fill it. |
| `ALTER TABLE t ADD COLUMN`, `DROP COLUMN`, `RENAME COLUMN old TO new`, `RENAME TO new_name` | T2 | `Database::alter_collection` (`src/collections/mod.rs:1208`) writes a new immutable `Layout` and repoints the catalog in one commit; no row is rewritten, so the cost is O(fields), not O(rows). ADD: existing rows read MISSING for the new field, which is already distinct from NULL. DROP: the slot is **tombstoned**, not removed -- dense rows are positional, so removing a slot would re-interpret every later field; the name leaves the layout, projection and predicates stop seeing it, and its index is dropped. RENAME COLUMN and RENAME TO are name records only; `CollectionId` does not change, so edges, indexes and the external-key mapping are untouched. |
| `ALTER TABLE t ALTER COLUMN c TYPE new_type` | T2 when the new type maps to the same `Kind` (`INT` <-> `BIGINT`, `REAL` <-> `DOUBLE PRECISION`); T3 when the `Kind` changes | the same-`Kind` case changes the declared spelling and leaves the encoding alone, so it is the descriptor rewrite above. A `Kind` change rewrites every row and re-encodes every scalar index key -- work proportional to the collection, with no bounded resumable atomic today. The shape it would need is the `begin_drop_collection` / `drop_collection_step` phase machine applied to a rewrite; until that exists the form is refused and names this. |
| `REINDEX [ON t] [USING method (field)]` | T2 | rebuild, no new atomic: drop the index tree and rebuild it through the sorted build (`src/collections/rebuild.rs`) under the `IndexState` Building/Ready/Dropping machine that already makes a build resumable across a reopen. |
| `COMPACT` | T2 | `Database::checkpoint` (`src/collections/mod.rs:1618`): fold the committed pages into the data file and reset the WAL. Deviation, stated: `checkpoint` returns `Ok(false)` while any reader in any process holds a slot, so `COMPACT` reports *deferred* and returns -- it never waits on a reader, and it is not Postgres `VACUUM`: nothing is reclaimed inside a table. |
| `CREATE SCHEMA`, `schema.table` | T2 | p2-schema-segment |
| `CREATE PROPERTY GRAPH name NODE TABLES (...) EDGE TYPES (...)` | T2 | optional naming of a context + label map; nothing is built |
| `BEGIN [READ ONLY]`, `COMMIT`, `ROLLBACK` | T1 | one writer, snapshot readers |
| `DECLARE c BINARY CURSOR FOR ...`, `FETCH FORWARD n`, `CLOSE` | T2 | pages over prepare_query (p3-wire) |
| `EXPLAIN` | T2 | prints the plan: driver, membership sets, order, work counters |
| `EXPLAIN ANALYZE <statement>` | T2 | the same plan, plus the statement run to completion under the caller's `QueryBudget`, with the `QueryWork` counters of every page (`src/query/mod.rs:427`) printed beside it. Deviation, stated: what is reported is logical work -- candidates, postings walked, row decodes, output bytes -- not a per-operator wall clock, because the engine keeps no per-operator timer. Wall clock appears once, as a total. |
| `SHOW TABLES`, `SHOW <collection>`, `SHOW CREATE TABLE t`, `SHOW INDEXES [ON t]` | T2 | sugar over the `db_*` catalog rows (p2-catalog-core): each is one fixed `SELECT` over one catalog view, costing O(collections), O(fields), O(fields + indexes) and O(indexes) respectively. The row-count and size-in-bytes columns e3 prints are a walk per collection; they are optional columns here and `EXPLAIN` labels them as scans (§6). |
| `SHOW EDGES [FROM t] [TO t]` | T2 | the `(from collection, edge type, to collection)` triples graph contract 2.5 derives from written edges, read from the interned edge-type records. A count per triple is a walk of the edge keyspace and is labelled a scan, exactly as above. |
| `SHOW STATUS`, `SHOW STORAGE` | T2 | `docs/OPS_CONTRACT.md` §6: the runtime facts and the per-keyspace byte report. `SHOW STORAGE` is a scan by definition and says so. |
| `CREATE MATERIALIZED VIEW name AS <select>`, `CREATE SEARCH VIEW name AS <select> WITH (autoindex)`, `REFRESH MATERIALIZED VIEW name` | T2 | the body SQL is stored in the catalog and the view **is** a derived collection, not a rewrite: populate is the prepared query's own bounded pages writing rows through `put`; `REFRESH` is the bounded, resumable clear (`begin_drop_collection` / `drop_collection_step` on the derived collection) followed by that populate. `WITH (autoindex)` is the `CREATE TABLE ... WITH (...)` sugar above, restricted to the view's TEXT fields. Stated plainly: a view is a stale copy between REFRESHes and is never incrementally maintained -- incremental maintenance is T3, because per-write delta propagation has no atomic. This is exactly why a *user* `CREATE VIEW` stays T3 while this is T2: a user view is a query rewrite at prepare time, which is a second planner path; a materialized view is rows in a collection, and every atomic that needs already exists. |
| a bounded prepared-plan cache behind `sql_prepare` | T2 | a least-recently-used map from statement text to the compiled `SelectPlan`, with three ceilings fixed at open -- entries, total cached statement bytes, and the longest statement that is cached at all -- so Law 1 holds whatever the workload does. The cache key carries the catalog generation: a `CREATE INDEX`, `ALTER TABLE` or `DROP` invalidates every plan compiled before it, rather than serving a plan built against a layout that no longer exists. |
| `SELECT version()`, `postgis_version()`, `current_schema()` | T2 | fixed rows (p3-pg-surface) |
| `WITH name AS (SELECT ...)` non-recursive | T2 | materialised once, bounded by QueryBudget rows |
| `UNION`, `WITH RECURSIVE`, `CREATE VIEW` (user), triggers, window functions | T3 | no atomic; recursion is a GRAPH_TABLE pattern |

## 3. Predicates and operators (WHERE)

| construct | tier | atomic |
|---|---|---|
| `AND` | T1 | filter conjunction |
| `=, <>, <, <=, >, >=` on indexed scalar | T1 | Scalar Eq/Range |
| `BETWEEN a AND b` | T1 | Range |
| `IS NULL`, `IS MISSING` | T1 | Scalar IsNull/IsMissing (both read the row: NULL and MISSING share one nullish index key) |
| `key BETWEEN`, `key >=` (external key) | T1 | Key filter, key-order driver |
| `OR` on the same index, `IN (list)` | T1 | union of ranges as one membership set (`QueryFilter::Any`) |
| `NOT`, `<>` on an indexed scalar, `IS NOT NULL` | T1 | complement (`QueryFilter::Not`); see the leaf rule below |
| `OR` across indexes, parenthesised groups | T1 | union of the leaves' membership sets |
| `EXISTS (subquery)`, `key IN (subquery)`, `NOT EXISTS` | T1 | semi-join membership set (`QueryFilter::Ids`), and its complement; the subquery's projected column must be TEXT, because an outer row is named by its external key |
| a boolean leaf with no set: geometry, traversal, `JsonEq`, a text phrase, `IS NULL`/`IS MISSING` | T3 inside a boolean | refused at prepare, naming the leaf; each is answered from the ROW, and a boolean evaluated per row reads every candidate's record |

**The boolean rule.** A disjunction is ONE membership set: the union of its
leaves' own index-side sets, built once and answered afterwards by one binary
search or one bit per candidate. A conjunction inside a disjunction is their
intersection. A complement is taken against the leaf's OWN universe, and the
universe is always the one that makes a null field UNKNOWN rather than a
member:

- a scalar or key complement is a union of at most two ranges over the same
  keyspace, with the nullish key in neither (SQL's rule that `NULL <> x` is
  unknown);
- a POINT complement is the point index's own postings, so a row whose `loc`
  is null or missing is in neither the leaf nor its complement;
- a TEXT complement is the text index's own DOCUMENT UNIVERSE -- the documents
  it holds a norm for -- for exactly the same reason: `NOT (name @@ 'x')` over
  a null `name` is unknown, and an unknown row is not returned;
- an explicit `Ids` set (a semi-join's) has no index of its own, so its
  complement is the collection's live rows. That is the only universe that
  exists for a set the caller named, and a deleted row is not in it.

A complement is always a bitmap of the span, bounded by `span / 8` bytes and
refused when a bitmap that size does not fit the membership budget. `NOT` over
a union is De Morgan's law, applied while the filter is compiled, so every
complement stands over a LEAF and there is never a universe to guess at.

A caller's `Ids` set may name a sequence the collection has never issued. It
is DROPPED -- it can never be a member of anything -- not reported as
corruption.

A leaf whose set OVERFLOWS the membership budget refuses the whole boolean
rather than falling back to the row path: an OR evaluated per candidate reads
every candidate's record, which is the work the set exists to avoid. The
refusal names `WorkResource::MembershipBytes` and the byte cap it stopped at,
which is the currency the cap is stated in.

A boolean filter is a pure in-memory bit test, so it is evaluated BEFORE the
row is read -- before a batched row pass gathers and before a borrowed row is
taken -- and never by disabling either. A query with `IN (...)` beside a
geometry predicate keeps the plan the same query with `= 'x'` gets.

A disjunction never DRIVES by preference -- the driver is chosen from the
remaining conjuncts or from the order -- but when nothing else can walk, the
union set itself drives in ascending entity id (`QueryDriver::Membership`).
Its cost is one candidate per member and no posting and no record: the set is
already built and already charged.
| `LIKE 'abc%'` | T1 | text-key prefix range `[abc, abd)` on the column's own scalar index (`functions::prefix_successor`, §4.1); `starts_with(col, 'abc')` is the same range. A prefix whose successor is not valid UTF-8 (`'\u{ff}%'` is `C3 BF`, whose successor is `C3 C0`) is REFUSED with a named reason: a text bound is a `String`, and widening it to the replacement character would admit every value in between |
| `LIKE '%abc%'`, an interior `%` or `_`, `ILIKE` | T2 | trigram index family (pg_trgm-compatible), new family under a feature bit. NOT demoted to a scan without it: §6 does not allow a scan to be taken silently, so the form is refused and names the family |
| `SIMILAR TO`, regex `~` | T3 | no index atomic |

## 4. Functions

### 4.1 String (Postgres names)

| function | tier | execution |
|---|---|---|
| `lower`, `upper`, `length`, `concat`, `||`, `substring`, `left`, `right`, `trim`, `split_part`, `replace`, `position`, `starts_with` | T1 | `src/sql/functions.rs`: row functions on PROJECTED values (`CompiledRow::eval`, `src/sql/compile.rs`) -- one row in, one value out, no read of any other row, so the cost is proportional to the rows RETURNED and `EXPLAIN` prints each under `row functions`. In a WHERE: `col LIKE 'x%'` and `starts_with(col, 'x')` are a text-key prefix `Range` on the column's own scalar index; `lower(col) = x`, `lower(col) LIKE 'x%'` and `starts_with(lower(col), 'x')` need the EXPRESSION index `CREATE INDEX i ON t (lower(col))` (`IndexExpr::Lower`, a scalar index whose stored value is the fold, `src/collections/catalog.rs`) and are REFUSED without it rather than scanned. `concat` ignores NULL, `||` propagates it, as in Postgres |
| `CREATE INDEX i ON t (lower(col))` | T1 | an EXPRESSION scalar index: descriptor version 3 behind an additive `EXPRESSION_FEATURE` bit, the same keys and the same walk as an ordinary scalar index over a value derived on the write path. The set of expressions is CLOSED (`lower` only) and each is O(value bytes) per write, which is what keeps the per-write hook bounded (Law 1). No row byte changes: the derived value lives in the index and nowhere else. Every row-side recomputation -- the verifier, a non-driving predicate, a rank key -- passes through the same expression the write path does. STATED LIMIT: Unicode lowercasing can LENGTHEN a string (`İ` is 2 bytes, its image is 3), so a value inside the 1024-byte scalar text-key limit can have an image outside it; that write is refused with a named reason at the put and at the late build, never truncated to a key that is not the expression's value |
| `ILIKE` | T2 | see §3 (trigram) |
| `CASE WHEN <cond> THEN <value> [WHEN ...] [ELSE <value>] END` | T2 | a row expression: one row in, one value out, no read of any other row. In `ORDER BY` it is one key (§5 deviation 3), so it rides the same order-expression leaf an arithmetic blend does. In `WHERE` it is row-bound -- it never becomes an index range -- and `EXPLAIN` labels it so. |
| `->`, `->>`, `#>`, `#>>`, `json_array_length` on a `Kind::Json` field | T2 | row functions over the binary JSON the row codec already decodes (`src/lib.rs`): one row, no extra read. None of them becomes an index range without an expression index over the same path, which is the `lower(col)` rule above. |
| `regexp_*` | T3 | no atomic |

### 4.2 Date/time (Postgres names; storage = Int microseconds, declared TIMESTAMPTZ / DATE)

| function | tier | execution |
|---|---|---|
| `EXTRACT(YEAR FROM t) <cmp> n`, `EXTRACT(YEAR FROM t) BETWEEN`, `date_trunc('unit', t) <cmp>\|BETWEEN lit`, `t::date <cmp> lit`, `t <cmp> lit`, `t BETWEEN lit AND lit` in WHERE | T1 | ONE scalar `Range` on the column's own index, folded at prepare (`Compiler::time_filter`, `src/sql/compile.rs`): the function is never evaluated per candidate and `EXPLAIN` prints the fold under `range rewrites`. A literal off the unit's boundary is its own comparison, as in Postgres: `=` is an EMPTY range (no truncation equals an interior instant), and BOTH inequalities cut at the boundary ABOVE the literal, so `date_trunc('month',t) >= '1950-01-15'` excludes January and `< '1950-01-15'` keeps it. `BETWEEN` carries that rule in its lower half only. Still one range in every case, never a refusal. An `EXTRACT(YEAR ...)` literal outside the representable year range saturates at the ends of the stored microsecond order, which is the same answer without an overflow. INSERT accepts the same ISO-8601 and Postgres date/time literal forms and stores the integer; SELECT prints the column back as an ISO-8601 string -- through `SELECT col`, `SELECT *`, `col::text`, a string function's argument, `concat`, `||`, and a `GROUP BY` key, which are one rule and cannot disagree |
| `EXTRACT(MONTH\|DAY\|DOW\|HOUR\|MINUTE\|SECOND FROM t) <cmp> n`, and `<>` over any date window | T2 | the pre-image is a SET of ranges -- one interval per period in the corpus -- which is exactly the membership-set union `OR` and `IN (list)` compile to (§3). Refused by name with that reason until the union is built; never emulated by a scan (§6) |
| same in SELECT / GROUP BY | T1 in SELECT (row function, `CompiledRow`); GROUP BY over a function is T2 | a folded answer returns groups, not rows, so a row function in a folded select list is refused and names `GROUP BY col` as the spelling. GROUP BY streams when the index order equals the truncation order -- that shape is the §4.7 atomic and is unbuilt for a function key |
| `now()`, `current_date`, `interval` arithmetic, `age(t)`, `t + interval` | T1 | constants folded ONCE at prepare, so every row of one answer sees one instant; `age` and every interval are microseconds (§5 deviation 8). A CALENDAR interval (`interval '1 month'`, `'1 year'`) is T3 in this shape: a month is 28-31 days, so there is no constant to fold, and `date_trunc('month', t)` is the calendar-aware spelling that IS accepted |
| `to_char(t, fmt)`, `to_timestamp`, `to_date` | T1 for the named templates | `to_char` carries `YYYY-MM-DD`, `YYYY-MM`, `YYYY`, `HH24:MI`, `HH24:MI:SS` and `YYYY-MM-DD HH24:MI:SS`, checked at PREPARE so an unnamed template is a refusal rather than an error on the first row. A general Postgres template is a formatting language of its own and is T3 |
| time zones other than UTC storage | T3 | declared TIMESTAMPTZ is stored UTC; display conversion only |

### 4.3 Graph (SQL/PGQ names)

| function / construct | tier | atomic |
|---|---|---|
| element pattern `(v IS label)` / `(v:label)`, edge `-[e IS type]->`, `<-`, `-` | T1 | direction + type on BFS |
| inline `WHERE` in element, edge (`-[r:t WHERE r.p > v]->`) | T1 | per-hop edge predicates over the inline bag (graph contract 4.3) |
| inline `WHERE` in element, far node, membership-able (`=`, range, `BETWEEN`, `ST_DWithin`, `ST_Within` on a point) | T1 | per-hop node predicates over index postings (graph contract 4.3) |
| inline `WHERE` in element, far node, row-bound (`IS NULL`, `IS MISSING`, text, geometry, JSON) | T2/T3 | a per-hop predicate is answered index-side; these need the row, which graph contract 4.3 forbids per hop |
| `COLUMNS (r.<prop> AS name)` on the edge element | T1 | the reaching edge, bound by the traversal (graph contract 4.2) |
| `ORDER BY <edge column alias>` | T1 | rank by the reaching edge's property |
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
| `POINT(lon lat)`, `POLYGON((lon lat, ...))` as a literal in value position | T2 | I/O only: the WKT text parser `ST_GeomFromText` already needs (p3-geometry-io), reached without the function name around it. Axis order is longitude then latitude -- the same order `ST_MakePoint(x, y)` takes and the same order e3 writes -- and the contract says so here because the reverse is the classic import bug. |
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
| `USING hnsw`, `USING diskann`, `USING ivfflat`, `USING vamana` | T2 | accepted as aliases of `quantized` with a notice; no new family. `vamana` is e3's and pgvectorscale's spelling of the DiskANN graph and joins the same alias list for the same reason. |
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
| `search_score()` | T2 | the Score leaf of the `search()` predicate above, normalised to [0,1]: 1 for an exact term match, decreasing with the edit distance actually spent and with how much of the final token the prefix had to complete. It lands with `search()` and costs nothing extra -- the automaton already knows both numbers. The formula is written down the way BM25's is (§5 deviation 5). |
| `bm25_norm(col, 'query', k)` | T2 | `bm25(col, q) / (bm25(col, q) + k)` over the existing Score leaf: one arithmetic operation, no extra pass, and strictly monotone in `bm25`, so the order it produces is the order `bm25` produces. It earns a row because a hybrid `ORDER BY` has to weigh a text term against a vector similarity, and a weight over an unbounded BM25 is not a weight; with both terms in [0,1) it is. |
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
2. Inline element `WHERE` prunes per hop by contract; the post-pattern `WHERE` filters completed matches. Both are standard syntax; the guarantee is ours. A node predicate an index cannot answer without the row (`IS NULL`, `IS MISSING`, a text search, a geometry predicate, a JSON equality) is REFUSED inline rather than demoted to a post-filter: a post-filter keeps a node in the frontier that graph contract 4.3 says must never be expanded, so it answers a different question at two hops.
9. `<>` on an edge property is accepted, unlike `<>` on a scalar column (§3). The §3 refusal is about an INDEX -- the complement of an equality is not a posting range, so it is not over a membership set. An edge predicate reads the property out of the posting the hop is standing on, so the complement costs exactly what the predicate costs and no set is involved.
10. The reaching edge is projected under a reserved `@edge.` prefix (`@edge.weight`), which no unquoted SQL identifier can spell, so it never collides with a declared field. A `COLUMNS (r.weight AS w)` entry compiles to it, and `ORDER BY w` resolves against the pattern's edge aliases before it looks for a column of the far node. An edge column entry and an `ORDER BY` over one are matched to the pattern's edge variable WITHOUT case, as every other name in this dialect is.
13. A statement that reads the reaching edge runs on the traversal that bound it, because no other candidate stream carries the edge (graph contract 4.2); any other driver is refused when the query is prepared, with the driver named. So a post-pattern `_key` predicate beside such a statement keeps the traversal driving and is answered from the external key the row carries, at one primary read per candidate, instead of taking the driver for the mapping walk. A `_key` predicate with no such pattern still drives the mapping walk, which is the cheaper plan and remains the default.
3. `ORDER BY` takes one key; an expression is one key (the Score atomic). Two keys are a refusal.
4. `OFFSET` is a keyset continuation, never a skip count.
5. BM25 stands behind `ts_rank_cd`; the number differs from Postgres and the docs say so.
6. `USING hnsw|diskann|ivfflat` are aliases of the quantized family.
7. A join never executes a pattern; a relation between rows is an edge.
8. Declared TIMESTAMPTZ is stored as UTC microseconds in an Int; no time-zone storage.
11. A nullish group key (NULL or missing) sorts FIRST under `GROUP BY`, the scalar keyspace's own order; Postgres sorts NULL last and `NULLS FIRST|LAST` is refused. `HAVING` over an all-null accumulator drops the group (SQL three-valued logic); a `HAVING` over `min`/`max` of a non-numeric column is refused at prepare.
9. `DROP TABLE` is RESTRICT by default, and what restricts it is GRAPH EDGES, not foreign keys: Postgres refuses on a dependent constraint, this refuses while any edge in any context references a row of the table and names those contexts (graph contract 6.1). `CASCADE` removes those edges and nothing else -- it never reaches a second table's rows. A table with no edges on it drops under the default.
10. `DROP TABLE` is bounded and resumable, so it is not one transaction: the DROPPING mark is committed first and each bounded step after it is committed as it goes. An interrupted `DROP TABLE` leaves a collection that answers nothing and resumes from its committed cursor; it never leaves a half-emptied readable table. `ROLLBACK` does not undo a drop that has begun.
11. e3's `FROM MATCH (a)-[r]->(b)` is not adopted (§1) and never will be. The
    capability is not lost and is not Tier 3: it is T1 under the standard
    spelling, `FROM GRAPH_TABLE (g MATCH (a)-[r]->(b) COLUMNS (...))`.
    Migration is mechanical -- wrap the pattern in `GRAPH_TABLE (...)`, name
    the graph, and move the SELECT list into `COLUMNS`. `MATCH SHORTEST` is
    `ANY SHORTEST` (§4.3), and a multi-FROM `..., collection AS alias` is a
    `CROSS JOIN LATERAL` (§4.8). This is a refusal with a named reason: the
    atomic exists, the spelling does not.
12. `NOT NULL` is enforced here. e3 parses it and does not check it, so a
    corpus e3 accepted can be refused by e4 on the row that was always in
    violation. The check is a descriptor flag tested when the row is
    assembled; the error names the column.
13. `FROM ALL` is unordered by contract: it concatenates the collections in
    catalog id order and pages within each. It is not a UNION (which stays
    T3), it does not deduplicate, and a ranked `ORDER BY` over it is refused
    for the reason in its §2 row.

## 6. Execution guarantees the contract makes

- Every T1/T2 predicate on an indexed field is answered index-side (posting, membership set, or inline edge property); a row is read only for projection or for a predicate the plan names as row-bound. `EXPLAIN` prints which.
- Work is proportional to candidates walked or rows returned, never to the collection, except for constructs whose definition is a scan (exact vector order without a filter, `count(*)` without a filter), which `EXPLAIN` labels as scans.
- A boolean filter's memory is the same membership budget every other set walk is bounded by: a plain Vec while it stays smaller than a bitmap of the collection's span, then that bitmap, then a refusal. A complement is always a bitmap, so a span whose bitmap does not fit `RUN_BYTES` has no complement and the filter is refused rather than degraded. What `RUN_BYTES` bounds is what is held AT ONCE, over every boolean filter of the query together: each union and intersection folds in place into its accumulator rather than copying both sides, every live intermediate is counted against the one budget, and a tree that would hold more is refused with `WorkResource::MembershipBytes` — a resource with no `QueryBudget` field, because the ceiling is the memory promise and not a caller allowance.
- A semi-join's set is built while the statement is COMPILED, and it runs under the caller's `QueryBudget` and cancellation like any other walk: the edge-keyspace walk is charged `GraphEdges` per edge and `GraphVisited` per entity kept, and the inner collection query is an ordinary prepared query under the same budget.
- Memory per query is bounded by QueryBudget: pages, membership sets, groups (`WorkResource::Groups` — the accumulator sets an aggregate holds AT ONCE, one under the streaming shape and one per distinct group under the hashed one; its default ceiling is `RUN_BYTES` divided by what one group costs, applied even under `QueryBudget::unlimited`), frontier.
- A §4.1 / §4.2 function is in exactly one of two places and `EXPLAIN` says which: a RANGE REWRITE, folded into index bounds at prepare and never evaluated per candidate, whose cost is the candidates the range admits; or a ROW FUNCTION over the values a returned row already projected, whose cost is one evaluation per row RETURNED. A function that is neither -- because its pre-image is a set of ranges, or because the expression index it would ride does not exist -- is refused, not quietly moved into the row path.
- Every T3 refusal names the missing atomic in its error text.

## 7. Order of Phase-3 work

1. Parser for §2 T1 + §3 T1 + §6 guarantees, with `EXPLAIN`.
2. Aggregates (§4.7) — DONE, `src/query/aggregate.rs`; date/time and string
   functions (§4.1, §4.2) — DONE, `src/sql/functions.rs` plus the expression
   scalar index (`IndexExpr::Lower`) and the declared-type descriptor field
   (`CollectionInfo::declared`). What is left of §4.2 is the MULTI-range
   rewrites (`EXTRACT(MONTH ...)`, `EXTRACT(DOW ...)`), which item 3's
   membership union now makes possible: a follow-up, not yet written.
3. `OR`/`IN`/`NOT`/`EXISTS` (§3) — DONE, `src/query/membership.rs`
   (`SetExpr` and the set algebra) with `QueryFilter::Any`/`All`/`Not`/`Ids`
   and `QueryDriver::Membership`.
4. Graph T2 (§4.3) in the graph-contract order.
5. Geometry I/O and `&&` (§4.4), catalog views and wire (p3-pg-surface, p3-wire).
6. Trigram index for `ILIKE` / infix `LIKE`; typo-tolerant `search()` (§4.6).
7. Key-equality joins (§4.8).
8. The write-path schema behaviour (`ALTER TABLE`, defaults, generated
   columns, `NOT NULL`) and the predicate-driven `UPDATE` / `DELETE` walk:
   surface over atomics that exist, so it is cheap and it unblocks migration.
9. The `SHOW` family, `EXPLAIN ANALYZE` and the plan cache, alongside the
   catalog views of p3-pg-surface.
10. `docs/OPS_CONTRACT.md` in its own order: service mode and publish, then
    the statement timeout and the interrupt handle, then the change feed.
