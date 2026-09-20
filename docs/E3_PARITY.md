# e3 parity checklist — the phase-3 bar

Owner, 2026-09-20: phase 3 is the shipment path to replace e1, and the bar is
what sekejap-e3 does today. The disk format (e4-format-v1, engine 59d1cbc,
frozen under Law 8) stays; deeper performance work follows parity. This file
is the tracked target: every row is DONE, CONTRACT-T2, CONTRACT-T3, or NOT IN
CONTRACT, and a NOT IN CONTRACT row must become a contract row with a tier or
a written refusal. Regenerate the statuses when rows land; do not edit e3.

Produced 2026-09-20 from e3 `src/sql.rs`, `src/db.rs`, `src/service.rs`,
`src/exec.rs`, `src/catalog/mod.rs`, `docs/usage/*.md` against e4 HEAD
`8167b11` (`docs/QL_CONTRACT.md`, `docs/GRAPH_CONTRACT.md`, `src/sql/refuse.rs`).


Source of truth read: e3 `src/sql.rs` (grammar header lines 40–104), `src/db.rs`, `src/exec.rs`, `src/service.rs`, `src/catalog/mod.rs`, `docs/usage/*.md`; e4 `docs/QL_CONTRACT.md`, `docs/GRAPH_CONTRACT.md`, `docs/SOURCE_LAYOUT.md`, `src/sql/mod.rs`, `src/sql/refuse.rs`, `src/collections/mod.rs`.

Status key: **DONE** = accepted by e4 at HEAD (cite e4 file). **CONTRACT-T2 / T3** = named in `docs/QL_CONTRACT.md` at that tier, not built. **NOT IN CONTRACT** = e3 does it, e4's contract does not mention it at any tier.

### 1. SQL statements and DDL

| # | e3 capability | e3 (file:line) | e4 status | atomic e4 would need |
|---|---|---|---|---|
| 1 | `SELECT … FROM col [WHERE AND] [ORDER BY] [LIMIT]` | sql.rs:41-46 | DONE — `src/sql/mod.rs:23` | — |
| 2 | `FROM ALL` (every collection at once) | sql.rs:42 | NOT IN CONTRACT | a multi-collection driver; §2 names one source only |
| 3 | `INSERT INTO t (…) VALUES (…)`, multi-row, `$N` params | sql.rs:56, db.rs:10245 | DONE — `src/sql/mod.rs:79` | — |
| 4 | `UPDATE t SET … WHERE <any predicate>` | sql.rs:3681 | NOT IN CONTRACT | §2 fixes `WHERE key = $1`; predicate-driven update needs a scan-and-put loop with a row budget |
| 5 | `DELETE FROM t WHERE <predicate>` / `DELETE FROM ALL` | sql.rs:3551 | NOT IN CONTRACT | same: delete over a driver, not by key |
| 6 | `CREATE TABLE t (field type, `_key` PRIMARY KEY)` | sql.rs:3910 | DONE — `src/sql/mod.rs:80` | — |
| 7 | `WITH (hash:[…], range:[…], fulltext:[…], bm25:[…], spatial:[…])` index hints | sql.rs:66 | NOT IN CONTRACT | inline index declarations on CREATE TABLE; e4 only has separate `CREATE INDEX` |
| 8 | `TIMESTAMPTZ DEFAULT NOW()` | sql.rs:68, 3803 | NOT IN CONTRACT | column defaults in the descriptor + fill on put |
| 9 | `DEFAULT uuid4()` / `uuid5(ns, name)` | sql.rs:1228-1231 | NOT IN CONTRACT | generated key values at write time |
| 10 | `GENERATED ALWAYS AS (expr) STORED` | sql.rs:1232, 3830 | NOT IN CONTRACT | a computed-column evaluator on the write path |
| 11 | `ALTER TABLE` ADD / DROP / RENAME COLUMN / RENAME TO / ALTER TYPE | sql.rs:72-77, 4006 | NOT IN CONTRACT | §2 lists no ALTER at all, though `Database::alter_collection` exists (`collections/mod.rs:1213`) — a grammar + tier row |
| 12 | `DROP TABLE [IF EXISTS]` | sql.rs:70 | DONE — `collections/drop_collection.rs:228,333` | — |
| 13 | `DROP INDEX [IF EXISTS] ON t USING m (f)` | sql.rs:71 | DONE — `collections/catalog.rs:1869` | — |
| 14 | `CREATE INDEX … USING {btree,hash,gin,gist,bm25,spatial,vamana,search}` | sql.rs:4157-4169 | DONE — `src/sql/mod.rs:82-84` | — |
| 15 | `REINDEX` | sql.rs:450 | NOT IN CONTRACT | rebuild-in-place statement |
| 16 | `COMPACT` | sql.rs:464 | NOT IN CONTRACT | e4 has `checkpoint()` (`collections/mod.rs:1623`), no statement |
| 17 | `SHOW TABLES` / `SHOW <col>` / `SHOW CREATE TABLE` / `SHOW INDEXES` | sql.rs:79-82, db.rs:8146 | NOT IN CONTRACT | §2 covers `information_schema` only; the `SHOW` sugar has no tier |
| 18 | `CREATE [MATERIALIZED\|SEARCH] VIEW … WITH (autoindex)` + `REFRESH` | sql.rs:1075-1085, exec.rs:1623-1637 | NOT IN CONTRACT | user views are T3, but *materialized* views + refresh are not mentioned; a stored body + repopulate atomic |
| 19 | `EXPLAIN` | db.rs:11816 | DONE — `src/sql/mod.rs:506` (`sql_explain`) | — |
| 20 | `EXPLAIN ANALYZE` (measured counters) | db.rs:11824 | NOT IN CONTRACT | §2 has plain EXPLAIN only; ANALYZE = run + report `QueryWork` |
| 21 | Parsed-plan cache for repeated text | exec.rs:40-71, db.rs:541 | NOT IN CONTRACT | a bounded prepared-plan cache behind `sql_prepare` |
| 22 | `BEGIN` / `COMMIT` / `ROLLBACK` | sql.rs:461-463 | DONE — `src/sql/mod.rs:85` | — |

### 2. Predicates and expressions

| # | e3 capability | e3 (file:line) | e4 status | note |
|---|---|---|---|---|
| 23 | `AND`, `= != <> > < >= <=` | sql.rs:97 | DONE — `src/sql/mod.rs:44` | — |
| 24 | `BETWEEN a AND b` | sql.rs:98, 2640 | DONE — `src/sql/mod.rs:38` | — |
| 25 | `IS NULL` / `IS NOT NULL` | sql.rs:2686-2694 | DONE — `src/sql/mod.rs:39` | e4 adds `IS MISSING` |
| 26 | `OR` | sql.rs:2513-2520 | CONTRACT-T2 — §3 `OR` | union of ranges as one membership set |
| 27 | `IN (list)` / `NOT IN` | sql.rs:2647-2663 | CONTRACT-T2 — §3 `IN` | same membership-set atomic |
| 28 | `NOT <cond>` | sql.rs:2726 | CONTRACT-T2 — §3 `NOT` | complement over a membership set |
| 29 | `LIKE 'pat'` | sql.rs:2676 | CONTRACT-T2 — §3 | prefix range; infix needs trigram family |
| 30 | `ILIKE '%x%'` via gin trigram | sql.rs:2681, db.rs:7381 | CONTRACT-T2 — §3 | trigram index family under a feature bit |
| 31 | `CASE WHEN … THEN … ELSE … END` | sql.rs:2192, 2431-2447 | **NOT IN CONTRACT** | no tier row anywhere; a projected conditional expression |
| 32 | `AGE_DAYS(f)` / `AGE_HOURS(f)` | sql.rs:6870, 1713 | CONTRACT-T2 — §4.2 `age(t)` | Postgres spelling only; e3's names unlisted |
| 33 | `NOW()` in SELECT list | sql.rs:93 | CONTRACT-T2 — §4.2 `now()` | constant folded at prepare |
| 34 | `JSON_ARRAY_LENGTH(f)` | sql.rs:6872 | **NOT IN CONTRACT** | only `->>` JSON extraction is tiered (refuse.rs `->>`) |
| 35 | `LENGTH LEN LOWER UPPER TRIM LTRIM RTRIM SUBSTRING REPLACE CONCAT` | sql.rs:1711, 2260-2279 | CONTRACT-T2 — §4.1 | row functions on projected values |
| 36 | `YEAR MONTH DAY HOUR MINUTE SECOND DOW QUARTER`, `DATE_TRUNC` | sql.rs:1712, 2341 | CONTRACT-T2 — §4.2 `EXTRACT`/`date_trunc` | rewrite to scalar Ranges |
| 37 | `ORDER BY a, b` (multi-key sort) | sql.rs:2055-2072 | CONTRACT-T3 — §5 deviation 3 | two keys are a refusal by design |
| 38 | `OFFSET n` as a skip count | sql.rs:44, 2112 | CONTRACT-T2 — §5 deviation 4 | e4 makes OFFSET a keyset continuation |

### 3. Text search

| # | e3 capability | e3 (file:line) | e4 status | note |
|---|---|---|---|---|
| 39 | `SEARCH('q' [, typo => N])` typo budget | sql.rs:2853-2875 | CONTRACT-T2 — §4.6 `search()` | term-dictionary prefix range + bounded Levenshtein automaton |
| 40 | `SEARCH_SCORE()` as a projectable/orderable score | sql.rs:3249, 1417 | **NOT IN CONTRACT** | the typo-search *score leaf*; §4.6 tiers only the predicate |
| 41 | `BM25(f,'q')` as filter and as ORDER BY term | sql.rs:1657, 3247 | DONE — `src/sql/mod.rs:61` (`bm25()`, `ts_rank_cd`) | — |
| 42 | `BM25_NORM(f,'q',k)` — [0,1] normalised for blends | sql.rs:3247 | **NOT IN CONTRACT** | a normalised score leaf so hybrid weights are comparable |
| 43 | Multi-field search index (`build_search_index`) | db.rs:8011 | CONTRACT-T2 — §4.6 "multi-field text index" | concatenated stored field today |
| 44 | Highlighting | *none in e3* | CONTRACT-T2 — §4.6 `highlight`/`ts_headline` | e4's contract is ahead of e3 here |

### 4. Vector

| # | e3 capability | e3 (file:line) | e4 status | note |
|---|---|---|---|---|
| 45 | `VECTOR_NEAR(field, [v], k)` as a WHERE-side kNN generator | sql.rs:2826, 3040 | DONE (different spelling) — `src/sql/mod.rs:57` `ORDER BY f <=> v LIMIT k` | e3's function spelling itself is unlisted; capability is T1 |
| 46 | `<->`, `<=>`, `<#>` and `VECTOR_L2/COSINE/DOT` | sql.rs:97, usage/queries.md:198 | DONE — `index/vector/exact.rs:813` | — |
| 47 | `<+>` / `VECTOR_L1` (Manhattan) | sql.rs:1686 | CONTRACT-T3 — refuse.rs `<+>` | no atomic; e3 loses this |
| 48 | `ef_search` knob | db.rs:11811 | DONE — `src/sql/compile.rs:965` (`SET LOCAL`) | — |
| 49 | `USING vamana (f vector_l2_ops)` index spelling | sql.rs:4157-4165 | **NOT IN CONTRACT** | e4 spells it `exact`/`quantized` + hnsw/diskann/ivfflat aliases; `vamana` is unmapped |

### 5. Spatial

| # | e3 capability | e3 (file:line) | e4 status | note |
|---|---|---|---|---|
| 50 | `ST_DWithin(geom, POINT(…), m)` | sql.rs:2930 | DONE — `src/sql/mod.rs:45` | — |
| 51 | `ST_Contains` / `ST_Within` / `ST_Intersects` | sql.rs:2943-2989 | DONE — `src/sql/mod.rs:46-47` | — |
| 52 | `ST_Distance(f, POINT(…))` as an ORDER BY term | sql.rs:3288 | DONE — `src/sql/mod.rs:62` | — |
| 53 | `ST_Area`, `ST_Length`, `ST_Perimeter` as predicates | sql.rs:3003-3016 | CONTRACT-T2 — §4.4 (listed T1 as row fns, not in the slice grammar) | projection-expression evaluator |
| 54 | `ST_Centroid(f)` | sql.rs:2231 | CONTRACT-T2 — §4.4 | same |
| 55 | `ST_AsGeoJSON(f)` in the SELECT list | sql.rs:2237-2241 | CONTRACT-T2 — refuse.rs `ST_ASTEXT` family (p3-geometry-io) | — |
| 56 | `ST_GeomFromGeoJSON('…')` inside INSERT VALUES | sql.rs:1816-1822 | CONTRACT-T2 — §4.4 I/O fns | e4 accepts it only as a WHERE-side geo literal (`src/sql/mod.rs:51`) |
| 57 | `POINT(lon lat)` / `POLYGON((…))` literal syntax | sql.rs:2931 | **NOT IN CONTRACT** | e4 writes `ST_MakePoint`/`ST_MakeEnvelope`; the WKT-literal spelling has no row |

### 6. Graph

| # | e3 capability | e3 (file:line) | e4 status | note |
|---|---|---|---|---|
| 58 | `SELECT … FROM MATCH (a)-[r]->(b)` spelling | sql.rs:48-53 | **NOT IN CONTRACT** (explicitly rejected) — QL_CONTRACT §1 "Not adopted: … e3's `FROM MATCH`" | a documented migration note, not an atomic |
| 59 | Direction, edge type, `*a..b` var-length | sql.rs:57, usage/graph-queries.md:35-55 | DONE — `src/sql/mod.rs:70-75` (`GRAPH_TABLE`, hops, quantifiers) | — |
| 60 | `MATCH SHORTEST (a)-[r*]->(b)` | sql.rs:4771-4786 | CONTRACT-T2 — §4.3 `ANY/ALL SHORTEST` | unweighted shortest-path atomic (graph contract 5.3) |
| 61 | `PATH_AVG/SUM/MIN/MAX/PRODUCT/FIRST/LAST` | sql.rs:86-88 | CONTRACT-T2 — §4.3, graph contract 5.1 | streamed accumulators |
| 62 | Edge-property reads `r.field` in projection/WHERE | usage/graph-queries.md:79 | CONTRACT-T2 — §4.3 inline element WHERE / graph contract 4.2-4.3 | per-hop predicates are the pending item 1 |
| 63 | `INSERT ('a')-[:KIND {p: v}]->('b')` | sql.rs:57, 3417 | CONTRACT-T2 — §2 `INSERT INTO GRAPH … EDGE` | `put_edge` exists (`index/graph/mod.rs:997`); only the surface is missing |
| 64 | `DELETE ('a')-[:KIND]->('b')`, edge UPDATE | sql.rs:3564, 3723 | CONTRACT-T2 — §2 | `delete_edge` at `index/graph/mod.rs:1187` |
| 65 | `SHOW EDGES [FROM t] [TO t]` | sql.rs:80, db.rs:11892 | **NOT IN CONTRACT** | graph contract 2.5 derives edge-type rows; no statement surfaces them |
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
| 74 | `open_as_service()` — one writer, snapshot readers | service.rs:1-70 | **NOT IN CONTRACT** | no service layer in ARCHITECTURE/SOURCE_LAYOUT; `open_snapshot` (`collections/mod.rs:721`) is the raw ingredient |
| 75 | `ServiceDb::publish()` + stated staleness window | service.rs:16-26 | **NOT IN CONTRACT** | a publish cadence + read-your-writes barrier |
| 76 | `set_statement_timeout(Duration)` | db.rs:5008 | **NOT IN CONTRACT** | e4 bounds by `QueryBudget` rows/groups, not wall clock |
| 77 | `interrupt_handle()` / `cancel()` / `clear_interrupt()` | db.rs:330-334, 5014-5021 | **NOT IN CONTRACT** | internals exist (`Error::Cancelled`, `traverse_bfs_with_cancel` at `index/graph/mod.rs:1635`); no public handle or SQL surface |
| 78 | `subscribe_changes()` / `unsubscribe_changes()` — one event per committed batch | db.rs:4986-4998 | **NOT IN CONTRACT** | commit-time change feed; the `.watch()` foundation |
| 79 | `SHOW STATUS` (format, generation, counts, mode) | db.rs:8134, 8175 | **NOT IN CONTRACT** | e4 has `storage_bytes`/`io_counters`, no statement |
| 80 | `SHOW STORAGE` (live bytes per keyspace + files) | db.rs:8132, 8170 | **NOT IN CONTRACT** | same |
| 81 | `information_schema.{tables,columns,schemata,table_constraints,key_column_usage}` | catalog/mod.rs:30-36 | CONTRACT-T2 — §1 catalog row, p3-pg-surface | virtual rows over the catalog |
| 82 | `pg_indexes` | catalog/mod.rs:36, 112 | CONTRACT-T2 — p3-pg-surface | same |
| 83 | Dictionary queries compose (WHERE/ORDER/LIMIT/DISTINCT over views) | catalog/mod.rs:135-180 | CONTRACT-T2 — p3-pg-surface | plus `version()`/`current_schema()` (refuse.rs) |

### 9. Types and nullability

| # | e3 capability | e3 (file:line) | e4 status | note |
|---|---|---|---|---|
| 84 | `TEXT INTEGER REAL BOOL TIMESTAMPTZ GEO VECTOR JSON` | sql.rs:1208-1217 | DONE — `src/sql/mod.rs:80-81` (`TEXT/INT/BIGINT/REAL/DOUBLE/BOOLEAN/JSONB/TIMESTAMPTZ/DATE/VECTOR(n)/GEOMETRY`) | e4 adds DATE and typed GEOMETRY(Point, 4326) |
| 85 | `NOT NULL` on ADD COLUMN (parsed, *not enforced*) | sql.rs:4029 | **NOT IN CONTRACT** | no constraint row at any tier; e3 does not enforce it either, so parity is cheap |

### 10. Other notable

| # | e3 capability | e3 (file:line) | e4 status | note |
|---|---|---|---|---|
| 86 | `write_trace` — per-phase transaction timing | write_trace.rs:1-22 | **NOT IN CONTRACT** | e4 has `io_counters`/`pool_counters` (`collections/mod.rs:897,952`); no write-path breakdown |
| 87 | `stats()` / `memory_report()` / `trim_memory()` | db.rs:10177, 11763, 11726 | **NOT IN CONTRACT** | an introspection surface, no tier |
| 88 | Bulk load: `begin_bulk`/`put_value_bulk`/`link_many` | db.rs:11922-11987 | **NOT IN CONTRACT** | batched write path below the statement layer |
| 89 | `prepare_insert` / `insert_prepared` | db.rs:10245-10261 | DONE — `src/sql/mod.rs:501` (`sql_prepare`) | — |

---

## Counts

| status | count |
|---|---|
| DONE | 24 |
| CONTRACT-T2 | 28 |
| CONTRACT-T3 | 3 |
| NOT IN CONTRACT | 33 |
| informational (e4 ahead / e3 absent) | 2 |
| **total rows** | **90** |

## The NOT IN CONTRACT list (33)

These are the e3 capabilities e4's `docs/QL_CONTRACT.md` does not mention at *any* tier — each needs either a new contract row or an explicit "dropped, here's why".

**DDL/statements (13):** `FROM ALL`; `UPDATE … WHERE <predicate>`; `DELETE … WHERE <predicate>` / `DELETE FROM ALL`; `CREATE TABLE … WITH (…)` index hints; `DEFAULT NOW()`; `DEFAULT uuid4()/uuid5()`; `GENERATED ALWAYS AS … STORED`; all five `ALTER TABLE` forms; `REINDEX`; `COMPACT`; `SHOW TABLES/INDEXES/CREATE TABLE/<col>`; materialized & search views + `REFRESH`; `EXPLAIN ANALYZE`; plan cache.

**Expressions (3):** `CASE WHEN … END`; `JSON_ARRAY_LENGTH`; (nullability `NOT NULL`).

**Text (2):** `SEARCH_SCORE()` as a score leaf; `BM25_NORM` normalised score — both matter because e3's hybrid-ranking story (`docs/usage/queries.md:225`) depends on comparable [0,1] terms in one `ORDER BY`.

**Vector/spatial (2):** `USING vamana` spelling; `POINT()/POLYGON()` WKT literals.

**Graph (2):** `FROM MATCH` spelling (rejected on purpose — needs a migration note, not an atomic); `SHOW EDGES`.

**Service and ops (7):** `open_as_service`; `publish`/staleness contract; `set_statement_timeout`; `interrupt_handle`/`cancel`; `subscribe_changes`; `SHOW STATUS`; `SHOW STORAGE`.

**Diagnostics (3):** `write_trace`; `stats`/`memory_report`; bulk-load API.

The single largest gap is **group 8**: e3 ships a long-running service form with publish semantics, a statement timeout, a cancellation handle and a commit-time change feed; e4's contract covers the *query language* and never states a runtime/ops surface. That is a missing contract document, not a missing tier row. The second cluster is **write-time schema behaviour** (defaults, generated columns, ALTER) — `Database::alter_collection` already exists at `src/collections/mod.rs:1213`, so this is surface-only work whose absence from §2 looks like an oversight rather than a decision.