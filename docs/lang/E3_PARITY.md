# e3 parity checklist — the phase-3 bar

Owner, 2026-09-20: phase 3 is the shipment path to replace e1, and the bar is
what sekejap-e3 does today. The disk format (e4-format-v1, engine 59d1cbc,
frozen under Law 8) stays; deeper performance work follows parity. This file
is the tracked target: every row is DONE, CONTRACT-T2, CONTRACT-T3, or NOT IN
CONTRACT, and a NOT IN CONTRACT row must become a contract row with a tier or
a written refusal. Regenerate the statuses when rows land; do not edit e3.

Produced 2026-09-20 from e3 `src/sql.rs`, `src/db.rs`, `src/service.rs`,
`src/exec.rs`, `src/catalog/mod.rs`, `docs/usage/*.md` against e4 HEAD
`8167b11` (`docs/lang/QL_CONTRACT.md`, `docs/core/GRAPH_CONTRACT.md`, `lang/src/refuse.rs`).

Updated 2026-09-20 at e4 HEAD `bdbef43`: the 33 NOT IN CONTRACT rows were
tiered. 32 became CONTRACT-T2 in `docs/lang/QL_CONTRACT.md` (§2, §4.1, §4.4, §4.5,
§4.6, §5) or in the new `docs/dist/OPS_CONTRACT.md`; one (`FROM MATCH`) is a named
refusal with the capability kept under the standard spelling. NOT IN CONTRACT
is now zero. Three T2 rows carry a named T3 boundary inside them, listed under
the counts; nothing was emulated to reach a tier.


Source of truth read: e3 `src/sql.rs` (grammar header lines 40–104), `src/db.rs`, `src/exec.rs`, `src/service.rs`, `src/catalog/mod.rs`, `docs/usage/*.md`; e4 `docs/lang/QL_CONTRACT.md`, `docs/core/GRAPH_CONTRACT.md`, `docs/core/SOURCE_LAYOUT.md`, `lang/src/mod.rs`, `lang/src/refuse.rs`, `src/collections/mod.rs`.

Status key: **DONE** = accepted by e4 at HEAD (cite e4 file). **CONTRACT-T2 / T3** = named in `docs/lang/QL_CONTRACT.md` or `docs/dist/OPS_CONTRACT.md` at that tier, not built. **NOT ADOPTED** = the spelling is refused on purpose and the capability is kept under another one, with the migration written down. **NOT IN CONTRACT** = e3 does it, e4's contract does not mention it at any tier (none remain).

### 1. SQL statements and DDL

| # | e3 capability | e3 (file:line) | e4 status | atomic e4 would need |
|---|---|---|---|---|
| 1 | `SELECT … FROM col [WHERE AND] [ORDER BY] [LIMIT]` | sql.rs:41-46 | DONE — `lang/src/mod.rs:23` | — |
| 2 | `FROM ALL` (every collection at once) | sql.rs:42 | CONTRACT-T2 — QL §2 `FROM ALL`; PARSED and refused by name at `lang/src/compile/dml.rs::FROM_ALL`, pinned by `lang/tests/sql_dml.rs` `from_all_is_refused_by_name_in_both_statements_that_can_write_it` | a `Collections` concatenation driver over the catalog in id order, resume `(collection id, inner cursor)`. Not built: `prepare_query` compiles predicates against ONE collection (a `QueryFilter::Scalar` names an `IndexId`, and an `IndexId` belongs to a collection), so `FROM ALL` is one compiled plan PER collection plus a refusal naming the collection whose index is missing -- not one plan over a wider driver. A ranked `ORDER BY` over it stays T3: one order key across different layouts is not one key, and an N-way merge holds one cursor per collection |
| 3 | `INSERT INTO t (…) VALUES (…)`, multi-row, `$N` params | sql.rs:56, db.rs:10245 | DONE — `lang/src/mod.rs:79` | — |
| 4 | `UPDATE t SET … WHERE <any predicate>` | sql.rs:3681 | DONE — `core/engine/src/collections/write_set.rs` (`Database::update_where`), `lang/src/compile/dml.rs` (`Compiler::update_where`) | — (the driver supplies candidates a page at a time, each read-modify-put, bounded by the `rows_written` budget and resumable from `WriteProgress::cursor`; a SET over the driving index's own column is refused by name, with `CandidateDriver::Entities` as the stated remedy) |
| 5 | `DELETE FROM t WHERE <predicate>` / `DELETE FROM ALL` | sql.rs:3551 | DONE for `DELETE FROM t WHERE …` — `core/engine/src/collections/write_set.rs` (`Database::delete_where`), `lang/src/compile/dml.rs` (`Compiler::delete_where`); `DELETE FROM ALL` stays CONTRACT-T2 with row 2's refusal | the same driver walk feeding `delete`, with the graph contract 6.1 RESTRICT preflight PER ROW (`Database::entity_edge_contexts`, one descent per (entity, context) pair with edges), `CASCADE` as the explicit word, and the same budget and cursor |
| 6 | `CREATE TABLE t (field type, `_key` PRIMARY KEY)` | sql.rs:3910 | DONE — `lang/src/mod.rs:80` | — |
| 7 | `WITH (hash:[…], range:[…], fulltext:[…], bm25:[…], spatial:[…])` index hints | sql.rs:66 | CONTRACT-T2 — QL §2 | sugar over `CREATE INDEX`: hash/range → btree with a notice, fulltext/bm25 → gin, spatial → gist. No new atomic |
| 8 | `TIMESTAMPTZ DEFAULT NOW()` | sql.rs:68, 3803 | DONE — QL §2 | a per-field `ColumnRule` in the catalog descriptor behind the additive `COLUMN_RULES_FEATURE = 0x1000`, filled when the row is assembled; ONE clock read per row, shared by every `now()` column of it. `core/engine/src/collections/column_rules.rs`; `core/engine/tests/column_rules.rs`, `lang/tests/sql_schema.rs` |
| 9 | `DEFAULT uuid4()` / `uuid5(ns, name)` | sql.rs:1228-1231 | DONE — QL §2 | the same descriptor slot; the generator set is closed and each member is O(1) per row. `uuid4` is 16 bytes from `getrandom`; `uuid5` is RFC 4122 §4.3 SHA-1 over namespace+name, implemented in `column_rules.rs` with no new dependency. An arbitrary expression as a DEFAULT is refused by name |
| 10 | `GENERATED ALWAYS AS (expr) STORED` | sql.rs:1232, 3830 | CONTRACT-T2 — QL §2 | a compiled row expression over other fields of the same row, evaluated before index maintenance so an index over it is maintained normally. Cross-row, aggregate or subquery expressions are T3 |
| 11 | `ALTER TABLE` ADD / DROP / RENAME COLUMN / RENAME TO / ALTER TYPE | sql.rs:72-77, 4006 | DONE — QL §2; ALTER TYPE T1 same-`Kind`, T3 otherwise; RENAME COLUMN T3 on a populated collection | `alter_collection_rules` writes a new Layout and repoints the catalog, O(fields), carrying the declared types and the COLUMN RULES of the surviving fields; `rename_collection` is a name record. No row is rewritten because a dense row decodes under its OWN immutable layout -- which is also why RENAME COLUMN is refused on a populated collection, and why a `Kind` change is: both would need every row rewritten and no bounded resumable atomic exists. `lang/src/parser/ddl.rs`, `lang/src/compile/ddl.rs`; `lang/tests/sql_schema.rs` |
| 12 | `DROP TABLE [IF EXISTS]` | sql.rs:70 | DONE — `collections/drop_collection.rs:228,333` | — |
| 13 | `DROP INDEX [IF EXISTS] ON t USING m (f)` | sql.rs:71 | DONE — `collections/catalog.rs:1869` | — |
| 14 | `CREATE INDEX … USING {btree,hash,gin,gist,bm25,spatial,vamana,search}` | sql.rs:4157-4169 | DONE — `lang/src/mod.rs:82-84` | — |
| 15 | `REINDEX` | sql.rs:450 | CONTRACT-T2 — QL §2 | rebuild through the existing sorted build (`collections/rebuild.rs`) under the `IndexState` machine that already makes a build resumable |
| 16 | `COMPACT` | sql.rs:464 | CONTRACT-T2 — QL §2 | `checkpoint()` (`collections/mod.rs:1618`) as a statement. It reports *deferred* while a reader holds a slot and never waits; it is not Postgres `VACUUM` |
| 17 | `SHOW TABLES` / `SHOW <col>` / `SHOW CREATE TABLE` / `SHOW INDEXES` | sql.rs:79-82, db.rs:8146 | CONTRACT-T2 — QL §2 | sugar over the `db_*` catalog rows, one fixed SELECT each. The row-count and size columns are a scan and EXPLAIN says so |
| 18 | `CREATE [MATERIALIZED\|SEARCH] VIEW … WITH (autoindex)` + `REFRESH` | sql.rs:1075-1085, exec.rs:1623-1637 | CONTRACT-T2 — QL §2 | a bounded atomic can be named, so it is not refused: the body is stored in the catalog and the view is a derived collection populated by the prepared query's own pages; REFRESH is the bounded resumable clear plus that populate. Incremental maintenance is T3. A *user* view stays T3 because it is a prepare-time rewrite, which is a second planner path |
| 19 | `EXPLAIN` | db.rs:11816 | DONE — `lang/src/mod.rs:506` (`sql_explain`) | — |
| 20 | `EXPLAIN ANALYZE` (measured counters) | db.rs:11824 | CONTRACT-T2 — QL §2 | today's EXPLAIN plan plus the statement run under the caller's budget, printing each page's `QueryWork` (`query/mod.rs:427`). Logical work, not a per-operator wall clock |
| 21 | Parsed-plan cache for repeated text | exec.rs:40-71, db.rs:541 | CONTRACT-T2 — QL §2 | a bounded LRU with three ceilings fixed at open (entries, cached bytes, longest statement). The key carries the catalog generation, so DDL invalidates plans rather than serving one against a dead layout — e3 keys on text alone |
| 22 | `BEGIN` / `COMMIT` / `ROLLBACK` | sql.rs:461-463 | DONE — `lang/src/mod.rs:85` | — |

### 2. Predicates and expressions

| # | e3 capability | e3 (file:line) | e4 status | note |
|---|---|---|---|---|
| 23 | `AND`, `= != <> > < >= <=` | sql.rs:97 | DONE — `lang/src/mod.rs:44` | — |
| 24 | `BETWEEN a AND b` | sql.rs:98, 2640 | DONE — `lang/src/mod.rs:38` | — |
| 25 | `IS NULL` / `IS NOT NULL` | sql.rs:2686-2694 | DONE — `lang/src/mod.rs:39` | e4 adds `IS MISSING` |
| 26 | `OR` | sql.rs:2513-2520 | CONTRACT-T2 — §3 `OR` | union of ranges as one membership set |
| 27 | `IN (list)` / `NOT IN` | sql.rs:2647-2663 | CONTRACT-T2 — §3 `IN` | same membership-set atomic |
| 28 | `NOT <cond>` | sql.rs:2726 | CONTRACT-T2 — §3 `NOT` | complement over a membership set |
| 29 | `LIKE 'pat'` | sql.rs:2676 | CONTRACT-T2 — §3 | prefix range; infix needs trigram family |
| 30 | `ILIKE '%x%'` via gin trigram | sql.rs:2681, db.rs:7381 | CONTRACT-T2 — §3 | trigram index family under a feature bit |
| 31 | `CASE WHEN … THEN … ELSE … END` | sql.rs:2192, 2431-2447 | CONTRACT-T2 — QL §4.1 | a row expression: one row in, one value out. In ORDER BY it is one key; in WHERE it is row-bound and EXPLAIN labels it so |
| 32 | `AGE_DAYS(f)` / `AGE_HOURS(f)` | sql.rs:6870, 1713 | CONTRACT-T2 — §4.2 `age(t)` | Postgres spelling only; e3's names unlisted |
| 33 | `NOW()` in SELECT list | sql.rs:93 | CONTRACT-T2 — §4.2 `now()` | constant folded at prepare |
| 34 | `JSON_ARRAY_LENGTH(f)` | sql.rs:6872 | CONTRACT-T2 — QL §4.1 | listed with `->`, `->>`, `#>`, `#>>` as row functions over the binary JSON the row codec already decodes |
| 35 | `LENGTH LEN LOWER UPPER TRIM LTRIM RTRIM SUBSTRING REPLACE CONCAT` | sql.rs:1711, 2260-2279 | CONTRACT-T2 — §4.1 | row functions on projected values |
| 36 | `YEAR MONTH DAY HOUR MINUTE SECOND DOW QUARTER`, `DATE_TRUNC` | sql.rs:1712, 2341 | CONTRACT-T2 — §4.2 `EXTRACT`/`date_trunc` | rewrite to scalar Ranges |
| 37 | `ORDER BY a, b` (multi-key sort) | sql.rs:2055-2072 | CONTRACT-T3 — §5 deviation 3 | two keys are a refusal by design |
| 38 | `OFFSET n` as a skip count | sql.rs:44, 2112 | CONTRACT-T2 — §5 deviation 4 | e4 makes OFFSET a keyset continuation |

### 3. Text search

| # | e3 capability | e3 (file:line) | e4 status | note |
|---|---|---|---|---|
| 39 | `SEARCH('q' [, typo => N])` typo budget | sql.rs:2853-2875 | CONTRACT-T2 — §4.6 `search()` | term-dictionary prefix range + bounded Levenshtein automaton |
| 40 | `SEARCH_SCORE()` as a projectable/orderable score | sql.rs:3249, 1417 | CONTRACT-T2 — QL §4.6 | the Score leaf of `search()`, normalised to [0,1] from the edit distance spent and the prefix completed; it lands with `search()` and the automaton already knows both numbers |
| 41 | `BM25(f,'q')` as filter and as ORDER BY term | sql.rs:1657, 3247 | DONE — `lang/src/mod.rs:61` (`bm25()`, `ts_rank_cd`) | — |
| 42 | `BM25_NORM(f,'q',k)` — [0,1] normalised for blends | sql.rs:3247 | CONTRACT-T2 — QL §4.6 | `bm25/(bm25+k)` on the existing Score leaf: one operation, no extra pass, strictly monotone so the order is unchanged. A weight over an unbounded BM25 is not a weight |
| 43 | Multi-field search index (`build_search_index`) | db.rs:8011 | CONTRACT-T2 — §4.6 "multi-field text index" | concatenated stored field today |
| 44 | Highlighting | *none in e3* | CONTRACT-T2 — §4.6 `highlight`/`ts_headline` | e4's contract is ahead of e3 here |

### 4. Vector

| # | e3 capability | e3 (file:line) | e4 status | note |
|---|---|---|---|---|
| 45 | `VECTOR_NEAR(field, [v], k)` as a WHERE-side kNN generator | sql.rs:2826, 3040 | DONE (different spelling) — `lang/src/mod.rs:57` `ORDER BY f <=> v LIMIT k` | e3's function spelling itself is unlisted; capability is T1 |
| 46 | `<->`, `<=>`, `<#>` and `VECTOR_L2/COSINE/DOT` | sql.rs:97, usage/queries.md:198 | DONE — `index/vector/exact.rs:813` | — |
| 47 | `<+>` / `VECTOR_L1` (Manhattan) | sql.rs:1686 | CONTRACT-T3 — refuse.rs `<+>` | no atomic; e3 loses this |
| 48 | `ef_search` knob | db.rs:11811 | DONE — `lang/src/compile.rs:965` (`SET LOCAL`) | — |
| 49 | `USING vamana (f vector_l2_ops)` index spelling | sql.rs:4157-4165 | CONTRACT-T2 — QL §4.5 | added to the `hnsw`/`diskann`/`ivfflat` alias list for `quantized`; no new family |

### 5. Spatial

| # | e3 capability | e3 (file:line) | e4 status | note |
|---|---|---|---|---|
| 50 | `ST_DWithin(geom, POINT(…), m)` | sql.rs:2930 | DONE — `lang/src/mod.rs:45` | — |
| 51 | `ST_Contains` / `ST_Within` / `ST_Intersects` | sql.rs:2943-2989 | DONE — `lang/src/mod.rs:46-47` | — |
| 52 | `ST_Distance(f, POINT(…))` as an ORDER BY term | sql.rs:3288 | DONE — `lang/src/mod.rs:62` | — |
| 53 | `ST_Area`, `ST_Length`, `ST_Perimeter` as predicates | sql.rs:3003-3016 | CONTRACT-T2 — §4.4 (listed T1 as row fns, not in the slice grammar) | projection-expression evaluator |
| 54 | `ST_Centroid(f)` | sql.rs:2231 | CONTRACT-T2 — §4.4 | same |
| 55 | `ST_AsGeoJSON(f)` in the SELECT list | sql.rs:2237-2241 | CONTRACT-T2 — refuse.rs `ST_ASTEXT` family (p3-geometry-io) | — |
| 56 | `ST_GeomFromGeoJSON('…')` inside INSERT VALUES | sql.rs:1816-1822 | CONTRACT-T2 — §4.4 I/O fns | e4 accepts it only as a WHERE-side geo literal (`lang/src/mod.rs:51`) |
| 57 | `POINT(lon lat)` / `POLYGON((…))` literal syntax | sql.rs:2931 | CONTRACT-T2 — QL §4.4 | the `ST_GeomFromText` WKT parser (p3-geometry-io) reached without the function name; I/O only. Axis order is longitude then latitude, stated |

### 6. Graph

| # | e3 capability | e3 (file:line) | e4 status | note |
|---|---|---|---|---|
| 58 | `SELECT … FROM MATCH (a)-[r]->(b)` spelling | sql.rs:48-53 | NOT ADOPTED — QL §1, §5 deviation 11 | the capability is T1 under `FROM GRAPH_TABLE (g MATCH … COLUMNS (…))`; §5 deviation 11 writes the mechanical migration, including `MATCH SHORTEST` → `ANY SHORTEST` and multi-FROM → `CROSS JOIN LATERAL`. A refusal with a named reason: the atomic exists, the spelling does not |
| 59 | Direction, edge type, `*a..b` var-length | sql.rs:57, usage/graph-queries.md:35-55 | DONE — `lang/src/mod.rs:70-75` (`GRAPH_TABLE`, hops, quantifiers) | — |
| 60 | `MATCH SHORTEST (a)-[r*]->(b)` | sql.rs:4771-4786 | CONTRACT-T2 — §4.3 `ANY/ALL SHORTEST` | unweighted shortest-path atomic (graph contract 5.3) |
| 61 | `PATH_AVG/SUM/MIN/MAX/PRODUCT/FIRST/LAST` | sql.rs:86-88 | CONTRACT-T2 — §4.3, graph contract 5.1 | streamed accumulators |
| 62 | Edge-property reads `r.field` in projection/WHERE | usage/graph-queries.md:79 | CONTRACT-T2 — §4.3 inline element WHERE / graph contract 4.2-4.3 | per-hop predicates are the pending item 1 |
| 63 | `INSERT ('a')-[:KIND {p: v}]->('b')` | sql.rs:57, 3417 | CONTRACT-T2 — §2 `INSERT INTO GRAPH … EDGE` | `put_edge` exists (`index/graph/mod.rs:997`); only the surface is missing |
| 64 | `DELETE ('a')-[:KIND]->('b')`, edge UPDATE | sql.rs:3564, 3723 | CONTRACT-T2 — §2 | `delete_edge` at `index/graph/mod.rs:1187` |
| 65 | `SHOW EDGES [FROM t] [TO t]` | sql.rs:80, db.rs:11892 | CONTRACT-T2 — QL §2 | the `(from, type, to)` triples of graph contract 2.5, read from the interned edge-type records; a count per triple is a scan and is labelled one |
| 66 | Multi-FROM: `FROM MATCH (…), collection AS alias` | sql.rs:58 | CONTRACT-T2 — §4.8 `CROSS JOIN LATERAL` | one bounded traversal per driving row |
| 67 | `UNION` across MATCH results | usage/graph-queries.md:148 | CONTRACT-T3 — refuse.rs `UNION` | e3 loses this |
| 68 | `WITH` multi-stage traversal | usage/graph-queries.md:158 | CONTRACT-T2 — §2 non-recursive `WITH` | materialised once, row-budget bounded |
| 69 | Contexts / perspectives | *absent in e3* | e4 ahead — graph contract §3 | no parity work |

### 7. Aggregates, GROUP BY, HAVING, DISTINCT

| # | e3 capability | e3 (file:line) | e4 status | note |
|---|---|---|---|---|
| 70 | `COUNT(*) SUM AVG MIN MAX` | sql.rs:85, aggregate.rs | DONE — `src/query/aggregate.rs:534` | — |
| 71 | `GROUP BY`, `HAVING`, `DISTINCT` | sql.rs:1964-1999, 1944 | DONE — §4.7 T1, refuse.rs note | one group key only in e4 |
| 72 | `COUNT(DISTINCT x)` | sql.rs:2383 | CONTRACT-T2 — §4.7 | per-group distinct set is a second unbounded structure |
| 73 | `ORDER BY <aggregate alias>` | sql.rs:2001-2072 | DONE — §4.7 T1 | the one bounded in-memory sort |

### 8. Service and ops

| # | e3 capability | e3 (file:line) | e4 status | note |
|---|---|---|---|---|
| 74 | `open_as_service()` — one writer, snapshot readers | service.rs:1-70 | DONE — `dist/src/service/mod.rs:142` (`ServiceDatabase`), `:173` (`open`), `:209` (`writer`), `:249` (`reader`), `:504` (`close`); `dist/src/service/snapshot.rs:43` | Published view is `RwLock<Arc<Snapshot>>`, not `RwLock<Arc<Database>>`: `Database` carries per-handle `RefCell`/`Cell` caches and is `Send` but not `Sync`. A second writer process is still T3 and is refused by the page WAL's file lock |
| 75 | `ServiceDb::publish()` + stated staleness window | service.rs:16-26 | DONE — `dist/src/service/mod.rs:275` (`publish_now`), `:282`/`:289` (interval), `:666` (writer-side rate limit) | Window measured on 4,000 rows, four runs: one snapshot open 0.9–6.7 ms, one `publish_now` swap 0.9–2.0 ms, a commit visible to a new reader 100.3–108.5 ms after it under the 100 ms default. With no background thread, a reader past the interval pays the mint |
| 76 | `set_statement_timeout(Duration)` | db.rs:5008 | DONE — `core/engine/src/query/mod.rs:528` (`QueryBudget::deadline`), `:647` (`WorkResource::Deadline`), `:717` (`DEADLINE_POLL_CHARGES` = 1,024), `:778` (`check_deadline`); `dist/src/service/mod.rs:341` | The clock is read once per page and then once per 1,024 charges; no clock read at all on the no-deadline path. Still in addition to `QueryBudget`, never instead of it; a timeout never interrupts a commit (T3) |
| 77 | `interrupt_handle()` / `cancel()` / `clear_interrupt()` | db.rs:330-334, 5014-5021 | DONE — `dist/src/service/interrupt.rs:26`; `dist/src/service/mod.rs:377`/`:383`/`:388`, threaded into every `prepare_sql_with` and `next_page` by `:428` (`scan`) | Sticky until cleared, one relaxed load per charge |
| 78 | `subscribe_changes()` / `unsubscribe_changes()` — one event per committed batch | db.rs:4986-4998 | DONE — `dist/src/service/changes.rs:70` (`ChangeEvent`), `:205` (`deliver`); `dist/src/service/mod.rs:629` (`WriterGuard::commit`) | Delivered after the barrier, never on rollback. Both L1 bounds are stated constants: 256 events per subscriber (a full queue drops the NEW event and counts it in `lagged`) and 1,024 keys per event, past which the key list is dropped whole and the total reported. A durable replayable log is still T3 |
| 79 | `SHOW STATUS` (format, generation, counts, mode) | sql.rs:8134, exec.rs:519 | CONTRACT-T2 — OPS §6.1 | a `db_status` view over `storage_bytes` (`collections/mod.rs:794`), `tracked_pages` (`:798`), `io_counters` (`:947`) and the format bits. All O(1); the node and edge counts are scans and are optional columns |
| 80 | `SHOW STORAGE` (live bytes per keyspace + files) | sql.rs:8132, exec.rs:463 | CONTRACT-T2 — OPS §6.2 | a tag-attributing walk over e4's tag-prefixed keyspaces plus the O(1) file sizes. A scan by definition, and the only statement in either contract whose cost is proportional to the database on purpose |
| 81 | `information_schema.{tables,columns,schemata,table_constraints,key_column_usage}` | catalog/mod.rs:30-36 | CONTRACT-T2 — §1 catalog row, p3-pg-surface | virtual rows over the catalog |
| 82 | `pg_indexes` | catalog/mod.rs:36, 112 | CONTRACT-T2 — p3-pg-surface | same |
| 83 | Dictionary queries compose (WHERE/ORDER/LIMIT/DISTINCT over views) | catalog/mod.rs:135-180 | CONTRACT-T2 — p3-pg-surface | plus `version()`/`current_schema()` (refuse.rs) |

### 9. Types and nullability

| # | e3 capability | e3 (file:line) | e4 status | note |
|---|---|---|---|---|
| 84 | `TEXT INTEGER REAL BOOL TIMESTAMPTZ GEO VECTOR JSON` | sql.rs:1208-1217 | DONE — `lang/src/mod.rs:80-81` (`TEXT/INT/BIGINT/REAL/DOUBLE/BOOLEAN/JSONB/TIMESTAMPTZ/DATE/VECTOR(n)/GEOMETRY`) | e4 adds DATE and typed GEOMETRY(Point, 4326) |
| 85 | `NOT NULL` on ADD COLUMN (parsed, *not enforced*) | sql.rs:4029 | DONE — QL §2, §5 deviation 12 | a descriptor flag checked when the row is assembled and after the defaults are filled (MISSING and NULL are distinct in e4), refusing the write and naming the column, and saying both are refused. e4 **enforces** it where e3 parses it, so a corpus e3 accepted can be refused. `ADD COLUMN … NOT NULL` with no DEFAULT on a non-empty collection is T3 and refused by name. `core/engine/tests/column_rules.rs`, `lang/tests/sql_schema.rs` |

### 10. Other notable

| # | e3 capability | e3 (file:line) | e4 status | note |
|---|---|---|---|---|
| 86 | `write_trace` — per-phase transaction timing | write_trace.rs:1-22 | CONTRACT-T2 — OPS §8 | a feature-gated thread-local phase timer over e4's write path, one line per index family. Law 4: e4 counts the I/O (`io_counters` `:947`, `pool_counters` `:892`) and does not yet attribute the time |
| 87 | `stats()` / `memory_report()` / `trim_memory()` | db.rs:10177, 11763, 11726 | CONTRACT-T2 — OPS §6.3 | e4's honest report is short — the pool arena (a ceiling, labelled), the index and layout caches, the reader slots — because e4 holds nothing proportional to rows. That short list is Law 1 stated as a measurement. e3's rule that an absent structure is never reported as `0` is adopted verbatim |
| 88 | Bulk load: `begin_bulk`/`put_value_bulk`/`link_many` | db.rs:11922-11987 | DONE — `core/engine/src/collections/write_set.rs` (`Database::begin_bulk` / `end_bulk`), SQL `BEGIN BULK` / `END BULK` | the scope is built and nesting-counted; the outermost close calls `commit` with the same FULL barrier and publication as any other commit. The two batched entry points are NOT adopted and the reason is written in OPS §7: e4's `put`/`delete`/`put_edge` already write with no durability point of their own, so a loop inside the scope IS the batch. `rollback` makes a failed batch all-or-nothing, and clears the scope with it |
| 89 | `prepare_insert` / `insert_prepared` | db.rs:10245-10261 | DONE — `lang/src/mod.rs:501` (`sql_prepare`) | — |

---

## Counts

| status | before (HEAD `8167b11`) | tiered (HEAD `bdbef43`) | after OPS §1-§5 built |
|---|---:|---:|---:|
| DONE | 24 | 24 | **29** |
| CONTRACT-T2 | 28 | 60 | 55 |
| CONTRACT-T3 | 3 | 3 | 3 |
| NOT ADOPTED (named refusal, capability kept) | 0 | 1 | 1 |
| NOT IN CONTRACT | 33 | **0** | **0** |
| informational (e4 ahead / e3 absent) | 2 | 2 | 2 |
| **total rows** | **90** | **90** | **90** |

Rows 74-78 (`docs/dist/OPS_CONTRACT.md` §1-§5: service mode, publish, the
statement timeout, the interrupt handle and the change feed) moved
CONTRACT-T2 -> DONE. They are built in `dist/src/service/` over one additive
change in `core/engine/src/query/mod.rs`, and tested in
`dist/tests/service.rs`. The T3 boundaries inside rows 76 and 78 stand: a
timeout still never interrupts a commit, and a durable replayable change log
is still not built.

Of the 32 rows that moved to CONTRACT-T2, 23 are named in `docs/lang/QL_CONTRACT.md`
and 9 in `docs/dist/OPS_CONTRACT.md`. The 33rd, `FROM MATCH`, is NOT ADOPTED.

### T3 boundaries named inside a T2 row

The tier on a row is the tier of the capability as e3 ships it. Five rows
carry an explicit refusal for a form that has no bounded atomic, and none of
them is emulated:

| row | the T2 part | the T3 part, and why |
|---|---|---|
| 2, 5 | `FROM ALL` / `DELETE FROM ALL` as a concatenation driver | `FROM ALL` with a ranked `ORDER BY`: one order key across different layouts is not one key, and the N-way merge holds one cursor per collection, which is no budget dimension |
| 11 | ADD / DROP / RENAME COLUMN / RENAME TO, and ALTER TYPE within one `Kind` | ALTER TYPE across `Kind`s: it rewrites every row and re-encodes every scalar key, and no bounded resumable rewrite exists |
| 18 | a stored body, a bounded populate, a bounded clear | incremental maintenance: per-write delta propagation has no atomic |
| 85 | `NOT NULL` as a write-path check | `ADD COLUMN … NOT NULL` with no DEFAULT on a non-empty collection: the constraint is false the moment it is recorded |
| 76, 78 | statement timeout; the live change feed | a timeout that interrupts a commit (L3 has no fast path); a durable replayable change log (a second write path with its own retention and recovery) |

## The former NOT IN CONTRACT list (33), now tiered

Every row below was a capability e3 ships that e4's contracts did not mention
at any tier. Each now has a contract row with a tier, or a written refusal.

**DDL and statements (14) → `docs/lang/QL_CONTRACT.md` §2, all T2.** `FROM ALL`
(concatenation driver; ranked order refused); predicate-driven `UPDATE` and
`DELETE` / `DELETE FROM ALL` (driver walk, `rows_written` budget, snapshot-
consistent); `CREATE TABLE … WITH (…)` (sugar over `CREATE INDEX`);
`DEFAULT NOW()`; `DEFAULT uuid4()/uuid5()`; `GENERATED … STORED`; the five
`ALTER TABLE` forms over `alter_collection`; `REINDEX` (rebuild); `COMPACT`
(checkpoint, reports deferred); the `SHOW` family over `db_*` rows;
materialized and search views plus `REFRESH` (a bounded atomic was found, so
it is tiered rather than refused); `EXPLAIN ANALYZE` (today's EXPLAIN plus
`QueryWork`); the bounded plan cache (keyed on text **and** catalog
generation).

**Expressions (3) → §4.1, T2.** `CASE WHEN … END` as a row expression;
`JSON_ARRAY_LENGTH` with the `->`/`->>`/`#>`/`#>>` family; `NOT NULL` as a
write-path check (§2 and §5 deviation 12) — e4 enforces what e3 only parses.

**Text (2) → §4.6, T2.** `SEARCH_SCORE()` as the Score leaf of `search()`,
normalised to [0,1]; `BM25_NORM` as `bm25/(bm25+k)` on the existing leaf. Both
exist so e3's hybrid-ranking story (`docs/usage/queries.md:225`) has
comparable terms in one `ORDER BY` — a weight over an unbounded BM25 is not a
weight.

**Vector and spatial (2) → §4.5, §4.4, T2.** `USING vamana` joins the
`quantized` alias list; `POINT()`/`POLYGON()` WKT literals ride the
`ST_GeomFromText` parser, longitude before latitude, stated.

**Graph (2).** `SHOW EDGES` is T2 over graph contract 2.5's derived triples.
`FROM MATCH` is **NOT ADOPTED**: the capability is T1 under
`FROM GRAPH_TABLE (…)` and §5 deviation 11 writes the migration.

**Service and ops (7) → `docs/dist/OPS_CONTRACT.md`, T2.** `open_as_service` (§1);
`publish` and the staleness window (§2); `set_statement_timeout` (§3);
`interrupt_handle`/`cancel` (§4); `subscribe_changes` (§5); `SHOW STATUS`
(§6.1); `SHOW STORAGE` (§6.2).

**Diagnostics (3) → `docs/dist/OPS_CONTRACT.md`, T2.** `stats`/`memory_report`/
`trim_memory` (§6.3); bulk load (§7); `write_trace` (§8).

## What the tiering found

1. **The largest gap was a missing document, not missing tier rows.** Group 8
   is now `docs/dist/OPS_CONTRACT.md`: service mode, publish, timeout,
   cancellation, the change feed, introspection, bulk load and `write_trace`,
   each with e3's semantics cited, e4's ingredient cited, the atomic to build,
   the Law it must satisfy, and a tier.

2. **e4's publish barrier is one checkpoint cheaper than e3's.** e3's
   `ServiceDb::publish` must `publish_generation()` — commit *and* checkpoint
   — before minting, because an e3 snapshot opens the newest published
   generation and commits alone still live in the WAL (`service.rs:120-122`).
   e4's `commit` publishes and `open_snapshot` reads the committed-WAL
   overlay, so read-your-own-writes is a snapshot open and nothing more.

3. **e3's change feed has a Law 1 problem e4 must not copy.**
   `ChangeEvent.keys` accumulates one `String` per changed key per batch,
   deduplicated only against the previous entry. e3's own `put_value_bulk`
   reaches that with a single call. e4's contract caps the key list and
   degrades to a truncation flag and a count — and the Postgres wire arrives
   at the same answer independently, since a `NOTIFY` payload is capped at
   8,000 bytes.

4. **Write-time schema behaviour was surface-only, as suspected.**
   `Database::alter_collection` (`collections/mod.rs:1208`) already writes a
   new immutable Layout and repoints the catalog, so four of the five `ALTER
   TABLE` forms are descriptor work at O(fields). The two real constraints
   found while tiering: DROP COLUMN must **tombstone** the slot, because dense
   rows are positional and removing a slot re-interprets every later field;
   and ALTER TYPE across `Kind`s is a whole-collection rewrite with no bounded
   resumable atomic, so it is refused rather than tiered.

5. **Materialized views did not need a refusal.** Every atomic they need
   exists — catalog storage for the body, the prepared query's bounded pages,
   `put`, and the `begin_drop_collection`/`drop_collection_step` machine for
   the clear. The line that keeps `CREATE VIEW` at T3 while this is T2 is
   worth stating once: a user view is a query rewrite at prepare time, which
   is a second planner path; a materialized view is rows in a collection.

6. **Two places where e4 is stronger than e3, now written down.** A failed
   bulk batch commits nothing (e3 leaves the earlier rows stored), and
   `NOT NULL` is enforced (e3 parses it and does not check it). Both are
   migration notes, because both can refuse a workload e3 accepted.
