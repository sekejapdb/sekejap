# Parity checklist against the prior engine — the phase-3 bar

**What this compares, and why the comparison still matters.** "The prior
engine" throughout this file is the release sekejap replaces: a separate
storage design with its own SQL surface, the one app still runs. Its
sources are kept on branch `e1` (`git show e1:<path>`) and are never edited.
The comparison matters because that release has live users: every capability
it ships is either served here, named in a contract at a tier, or refused in
writing -- and this ledger is where that is proved one row at a time, so
nothing is dropped silently on the way to 0.17.

Owner: phase 3 is the shipment path to that replacement, and the bar is what
the prior engine does today. The disk format (sekejap disk format v2,
frozen under Law 8) stays; deeper performance work follows parity. This file
is the tracked target: every row is DONE, CONTRACT-T2, CONTRACT-T3, or NOT IN
CONTRACT, and a NOT IN CONTRACT row must become a contract row with a tier or
a written refusal. Regenerate the statuses when rows land; do not edit the
prior engine.

Produced from its `src/sql.rs`, `src/db.rs`, `src/service.rs`, `src/exec.rs`,
`src/catalog/mod.rs`, `docs/usage/*.md`.

Re-statused against THIS tree (0.17), by reading `lang/src/`, `lang/tests/`,
`core/engine/src/`, `core/engine/tests/`, `dist/src/`, `dist/tests/`,
`dist/rust/` and `dist/ffi/`. A row is DONE only where a named test in this
tree proves it; the test is in the row. NOT IN CONTRACT is zero and has been
since the tiering pass.

The 0.17 shipment added four surfaces the prior engine has no row for at
all -- the
PostgreSQL wire, the published Rust crate, the C ABI, and the language
wrappers over it. They are section 11 at the end, outside the parity count,
because there is nothing on the other side to be at parity WITH.

Status key: **DONE** = accepted by this tree and pinned by a named test (both
cited). **CONTRACT-T2 / T3** = named in `docs/lang/QL_CONTRACT.md` or
`docs/dist/OPS_CONTRACT.md` at that tier, not built. **NOT ADOPTED** = the
spelling is refused on purpose and the capability is kept under another one,
with the migration written down. **NOT IN CONTRACT** = the prior engine does it
and sekejap's contract does not mention it at any tier (none remain).

### 1. SQL statements and DDL

| # | prior-engine capability | prior engine (file:line) | sekejap status | atomic sekejap would need |
|---|---|---|---|---|
| 1 | `SELECT … FROM col [WHERE AND] [ORDER BY] [LIMIT]` | sql.rs:41-46 | DONE — `lang/src/lib.rs:23` | — |
| 2 | `FROM ALL` (every collection at once) | sql.rs:42 | CONTRACT-T2 — QL §2 `FROM ALL`; PARSED and refused by name at `lang/src/compile/dml.rs::FROM_ALL`, pinned by `lang/tests/sql_dml.rs` `from_all_is_refused_by_name_in_both_statements_that_can_write_it` | a `Collections` concatenation driver over the catalog in id order, resume `(collection id, inner cursor)`. Not built: `prepare_query` compiles predicates against ONE collection (a `QueryFilter::Scalar` names an `IndexId`, and an `IndexId` belongs to a collection), so `FROM ALL` is one compiled plan PER collection plus a refusal naming the collection whose index is missing -- not one plan over a wider driver. A ranked `ORDER BY` over it stays T3: one order key across different layouts is not one key, and an N-way merge holds one cursor per collection |
| 3 | `INSERT INTO t (…) VALUES (…)`, multi-row, `$N` params | sql.rs:56, db.rs:10245 | DONE — `lang/src/lib.rs:79` | — |
| 4 | `UPDATE t SET … WHERE <any predicate>` | sql.rs:3681 | DONE — `core/engine/src/collections/write_set.rs` (`Database::update_where`), `lang/src/compile/dml.rs` (`Compiler::update_where`) | — (the driver supplies candidates a page at a time, each read-modify-put, bounded by the `rows_written` budget and resumable from `WriteProgress::cursor`; a SET over the driving index's own column is refused by name, with `CandidateDriver::Entities` as the stated remedy) |
| 5 | `DELETE FROM t WHERE <predicate>` / `DELETE FROM ALL` | sql.rs:3551 | DONE for `DELETE FROM t WHERE …` — `core/engine/src/collections/write_set.rs` (`Database::delete_where`), `lang/src/compile/dml.rs` (`Compiler::delete_where`); `DELETE FROM ALL` stays CONTRACT-T2 with row 2's refusal | the same driver walk feeding `delete`, with the graph contract 6.1 RESTRICT preflight PER ROW (`Database::entity_edge_contexts`, one descent per (entity, context) pair with edges), `CASCADE` as the explicit word, and the same budget and cursor |
| 6 | `CREATE TABLE t (field type, `_key` PRIMARY KEY)` | sql.rs:3910 | DONE — `lang/src/lib.rs:80` | — |
| 7 | `WITH (hash:[…], range:[…], fulltext:[…], bm25:[…], spatial:[…])` index hints | sql.rs:66 | CONTRACT-T2 — QL §2 | sugar over `CREATE INDEX`: hash/range → btree with a notice, fulltext/bm25 → gin, spatial → gist. No new atomic |
| 8 | `TIMESTAMPTZ DEFAULT NOW()` | sql.rs:68, 3803 | DONE — QL §2 | a per-field `ColumnRule` in the catalog descriptor behind the additive `COLUMN_RULES_FEATURE = 0x1000`, filled when the row is assembled; ONE clock read per row, shared by every `now()` column of it. `core/engine/src/collections/column_rules.rs`; `core/engine/tests/column_rules.rs`, `lang/tests/sql_schema.rs` |
| 9 | `DEFAULT uuid4()` / `uuid5(ns, name)` | sql.rs:1228-1231 | DONE — QL §2 | the same descriptor slot; the generator set is closed and each member is O(1) per row. `uuid4` is 16 bytes from `getrandom`; `uuid5` is RFC 4122 §4.3 SHA-1 over namespace+name, implemented in `column_rules.rs` with no new dependency. An arbitrary expression as a DEFAULT is refused by name |
| 10 | `GENERATED ALWAYS AS (expr) STORED` | sql.rs:1232, 3830 | CONTRACT-T2 — QL §2 | a compiled row expression over other fields of the same row, evaluated before index maintenance so an index over it is maintained normally. Cross-row, aggregate or subquery expressions are T3 |
| 11 | `ALTER TABLE` ADD / DROP / RENAME COLUMN / RENAME TO / ALTER TYPE | sql.rs:72-77, 4006 | DONE — QL §2; ALTER TYPE T1 same-`Kind`, T3 otherwise; RENAME COLUMN T3 on a populated collection | `alter_collection_rules` writes a new Layout and repoints the catalog, O(fields), carrying the declared types and the COLUMN RULES of the surviving fields; `rename_collection` is a name record. No row is rewritten because a dense row decodes under its OWN immutable layout -- which is also why RENAME COLUMN is refused on a populated collection, and why a `Kind` change is: both would need every row rewritten and no bounded resumable atomic exists. `lang/src/parser/ddl.rs`, `lang/src/compile/ddl.rs`; `lang/tests/sql_schema.rs` |
| 12 | `DROP TABLE [IF EXISTS]` | sql.rs:70 | DONE — `core/engine/src/collections/drop_collection.rs` | — |
| 13 | `DROP INDEX [IF EXISTS] ON t USING m (f)` | sql.rs:71 | DONE — `core/engine/src/collections/catalog.rs` | — |
| 14 | `CREATE INDEX … USING {btree,hash,gin,gist,bm25,spatial,vamana,search}` | sql.rs:4157-4169 | DONE — `lang/src/lib.rs:82-84` | — |
| 15 | `REINDEX` | sql.rs:450 | CONTRACT-T2 — QL §2 | rebuild through the existing sorted build (`collections/rebuild.rs`) under the `IndexState` machine that already makes a build resumable |
| 16 | `COMPACT` | sql.rs:464 | CONTRACT-T2 as a STATEMENT; the operation itself is DONE as a CALL — `Database::checkpoint` (`core/engine/src/collections/mod.rs:2156`), `Db::checkpoint`, `sekejap_checkpoint`; `dist/rust/tests/api.rs` `a_checkpoint_folds_the_log_and_reports_a_deferred_fold_rather_than_waiting`; `dist/ffi/tests/abi.rs` `storage_answers_the_two_files_and_their_total_and_a_checkpoint_reports_whether_it_folded` | `checkpoint()` as a statement. It reports *deferred* while a reader holds a slot and never waits; it is not Postgres `VACUUM` |
| 17 | `SHOW TABLES` / `SHOW <col>` / `SHOW CREATE TABLE` / `SHOW INDEXES` | sql.rs:79-82, db.rs:8146 | DONE — `lang/src/compile/rows.rs::show`, `lang/src/parser/catalog.rs`; `lang/tests/sql_catalog.rs` `the_show_family_is_the_db_rows_said_in_one_word`, `show_create_table_prints_ddl_that_names_every_column_and_index` | sugar over the `db_*` catalog rows, one fixed SELECT each. The row count is the LIVE ROW COUNT record, one point read per collection, not the walk the prior engine paid; a size-in-bytes column is still absent and is `SHOW STORAGE` (row 80) |
| 18 | `CREATE [MATERIALIZED\|SEARCH] VIEW … WITH (autoindex)` + `REFRESH` | sql.rs:1075-1085, exec.rs:1623-1637 | CONTRACT-T2 — QL §2 | a bounded atomic can be named, so it is not refused: the body is stored in the catalog and the view is a derived collection populated by the prepared query's own pages; REFRESH is the bounded resumable clear plus that populate. Incremental maintenance is T3. A *user* view stays T3 because it is a prepare-time rewrite, which is a second planner path |
| 19 | `EXPLAIN` | db.rs:11816 | DONE — `lang/src/lib.rs:506` (`sql_explain`) | — |
| 20 | `EXPLAIN ANALYZE` (measured counters) | db.rs:11824 | CONTRACT-T2 — QL §2 | today's EXPLAIN plan plus the statement run under the caller's budget, printing each page's `QueryWork` (`core/engine/src/query/mod.rs`). Logical work, not a per-operator wall clock |
| 21 | Parsed-plan cache for repeated text | exec.rs:40-71, db.rs:541 | DONE — `dist/rust/src/plans.rs`; `dist/rust/tests/api.rs` `the_plan_cache_serves_db_query_and_a_hit_is_a_rebind`, `the_plan_cache_evicts_at_its_entry_ceiling_least_recently_used_first`, `a_ddl_statement_invalidates_every_plan_compiled_before_it` | a bounded LRU with three ceilings fixed at open (entries, cached bytes, longest statement). The key carries the catalog generation, so DDL invalidates plans rather than serving one against a dead layout — the prior engine keys on text alone |
| 22 | `BEGIN` / `COMMIT` / `ROLLBACK` | sql.rs:461-463 | DONE — `lang/src/lib.rs:85` | — |

### 2. Predicates and expressions

| # | prior-engine capability | prior engine (file:line) | sekejap status | note |
|---|---|---|---|---|
| 23 | `AND`, `= != <> > < >= <=` | sql.rs:97 | DONE — `lang/src/lib.rs:44` | — |
| 24 | `BETWEEN a AND b` | sql.rs:98, 2640 | DONE — `lang/src/lib.rs:38` | — |
| 25 | `IS NULL` / `IS NOT NULL` | sql.rs:2686-2694 | DONE — `lang/src/lib.rs:39` | sekejap adds `IS MISSING` |
| 26 | `OR` | sql.rs:2513-2520 | DONE — `core/engine/src/query/membership.rs`; `lang/tests/sql_tier1.rs` `a_disjunction_of_equalities_is_one_membership_set`, `a_disjunction_across_two_families_unions_two_sets` | union of ranges as one membership set |
| 27 | `IN (list)` / `NOT IN` | sql.rs:2647-2663 | DONE — `lang/tests/sql_tier1.rs` `in_a_list_is_the_same_union_written_shorter`, `not_in_a_list_is_the_complement_of_the_union` | same membership-set atomic |
| 28 | `NOT <cond>` | sql.rs:2726 | DONE — `lang/tests/sql_tier1.rs` `not_before_a_group_is_de_morgan`, `is_not_null_is_the_complement_of_the_nullish_key`, `a_null_value_is_in_neither_half_of_a_complement`; `core/engine/tests/query_boolean.rs` `a_complement_is_bounded_by_a_named_resource` | complement over a membership set, always a bitmap, bounded by `WorkResource::MembershipBytes` |
| 29 | `LIKE 'pat'` | sql.rs:2676 | DONE for the PREFIX form — `lang/src/compile/functions.rs`, `functions::prefix_successor`; `lang/tests/sql_functions.rs` `a_prefix_pattern_is_a_text_key_range_and_equals_the_filter`. An interior `%` or `_` stays CONTRACT-T2 | prefix range `[abc, abd)` on the column's own scalar index; infix needs the trigram family under a feature bit, and is refused by name rather than scanned |
| 30 | `ILIKE '%x%'` via gin trigram | sql.rs:2681, db.rs:7381 | CONTRACT-T2 — §3 | trigram index family under a feature bit |
| 31 | `CASE WHEN … THEN … ELSE … END` | sql.rs:2192, 2431-2447 | CONTRACT-T2 — QL §4.1 | a row expression: one row in, one value out. In ORDER BY it is one key; in WHERE it is row-bound and EXPLAIN labels it so |
| 32 | `AGE_DAYS(f)` / `AGE_HOURS(f)` | sql.rs:6870, 1713 | DONE under the Postgres spelling — `age(t)` in `lang/src/functions.rs`; `lang/tests/sql_functions.rs` `age_and_interval_arithmetic_are_microseconds_over_one_folded_clock`, `age_lies_between_two_clock_reads_this_test_made`. The prior engine's two names are not adopted | `age` and every interval are microseconds over one folded clock (§5 deviation 8); a caller divides |
| 33 | `NOW()` in SELECT list | sql.rs:93 | DONE — `lang/tests/sql_functions.rs` `a_clock_relative_predicate_is_folded_at_prepare_and_is_one_range`, `projected_date_functions_equal_rusts_own_computation` | constant folded ONCE at prepare, so every row of one answer sees one instant |
| 34 | `JSON_ARRAY_LENGTH(f)` | sql.rs:6872 | CONTRACT-T2 — QL §4.1 | listed with `->`, `->>`, `#>`, `#>>` as row functions over the binary JSON the row codec already decodes |
| 35 | `LENGTH LEN LOWER UPPER TRIM LTRIM RTRIM SUBSTRING REPLACE CONCAT` | sql.rs:1711, 2260-2279 | DONE — `lang/src/functions.rs`, the closed `ROW_FUNCTIONS` set of `lang/src/parser/mod.rs`; `lang/tests/sql_functions.rs` `projected_row_functions_equal_rusts_own_computation`, `substring_and_split_part_edges_equal_rusts_own_computation`, `concat_ignores_a_missing_value_and_the_operator_propagates_it` | row functions on projected values; `lower(col)` in a WHERE needs the expression index and is refused without it |
| 36 | `YEAR MONTH DAY HOUR MINUTE SECOND DOW QUARTER`, `DATE_TRUNC` | sql.rs:1712, 2341 | DONE for `EXTRACT(YEAR ...)` and `date_trunc('unit', t)` in a WHERE, and for every one of them in a SELECT list — `lang/tests/sql_functions.rs` `extract_year_equality_is_one_range_and_equals_the_brute_force_filter`, `date_trunc_equality_and_between_equal_the_brute_force_filter`, `projected_date_functions_equal_rusts_own_computation`. `EXTRACT(MONTH\|DAY\|DOW\|HOUR ...)` in a WHERE stays CONTRACT-T2 | ONE scalar Range folded at prepare. The multi-period forms are a SET of ranges -- the membership union now exists (row 26) but is not wired to this rewrite, so they are refused by name with that reason |
| 37 | `ORDER BY a, b` (multi-key sort) | sql.rs:2055-2072 | CONTRACT-T3 — §5 deviation 3 | two keys are a refusal by design |
| 38 | `OFFSET n` as a skip count | sql.rs:44, 2112 | CONTRACT-T2 — §5 deviation 4 | sekejap makes OFFSET a keyset continuation |

### 3. Text search

| # | prior-engine capability | prior engine (file:line) | sekejap status | note |
|---|---|---|---|---|
| 39 | `SEARCH('q' [, typo => N])` typo budget | sql.rs:2853-2875 | CONTRACT-T2 — §4.6 `search()` | term-dictionary prefix range + bounded Levenshtein automaton |
| 40 | `SEARCH_SCORE()` as a projectable/orderable score | sql.rs:3249, 1417 | CONTRACT-T2 — QL §4.6 | the Score leaf of `search()`, normalised to [0,1] from the edit distance spent and the prefix completed; it lands with `search()` and the automaton already knows both numbers |
| 41 | `BM25(f,'q')` as filter and as ORDER BY term | sql.rs:1657, 3247 | DONE — `lang/src/parser/expr.rs` (`bm25()`, `ts_rank_cd`); `lang/tests/sql_tier1.rs` `ts_rank_cd_and_bm25_are_the_same_order`, `the_ranking_value_can_be_projected_under_an_alias` | — |
| 42 | `BM25_NORM(f,'q',k)` — [0,1] normalised for blends | sql.rs:3247 | CONTRACT-T2 — QL §4.6 | `bm25/(bm25+k)` on the existing Score leaf: one operation, no extra pass, strictly monotone so the order is unchanged. A weight over an unbounded BM25 is not a weight |
| 43 | Multi-field search index (`build_search_index`) | db.rs:8011 | CONTRACT-T2 — §4.6 "multi-field text index" | concatenated stored field today |
| 44 | Highlighting | *none in the prior engine* | CONTRACT-T2 — §4.6 `highlight`/`ts_headline` | sekejap's contract is ahead here |

### 4. Vector

| # | prior-engine capability | prior engine (file:line) | sekejap status | note |
|---|---|---|---|---|
| 45 | `VECTOR_NEAR(field, [v], k)` as a WHERE-side kNN generator | sql.rs:2826, 3040 | DONE (different spelling) — `lang/src/lib.rs:57` `ORDER BY f <=> v LIMIT k` | the prior engine's function spelling itself is unlisted; capability is T1 |
| 46 | `<->`, `<=>`, `<#>` and `VECTOR_L2/COSINE/DOT` | sql.rs:97, usage/queries.md:198 | DONE — `core/engine/src/index/vector/exact.rs` | — |
| 47 | `<+>` / `VECTOR_L1` (Manhattan) | sql.rs:1686 | CONTRACT-T3 — refuse.rs `<+>` | no atomic; the prior engine loses this |
| 48 | `ef_search` knob | db.rs:11811 | DONE — `lang/src/compile/mod.rs` (`SET LOCAL`) | — |
| 49 | `USING vamana (f vector_l2_ops)` index spelling | sql.rs:4157-4165 | DONE — `lang/src/parser/ddl.rs`; `lang/tests/sql_tier1.rs` `create_table_and_create_index_build_a_queryable_collection` | an alias of `quantized` beside `hnsw`/`diskann`/`ivfflat`, with a notice; no new family |

### 5. Spatial

| # | prior-engine capability | prior engine (file:line) | sekejap status | note |
|---|---|---|---|---|
| 50 | `ST_DWithin(geom, POINT(…), m)` | sql.rs:2930 | DONE — `lang/src/lib.rs:45` | — |
| 51 | `ST_Contains` / `ST_Within` / `ST_Intersects` | sql.rs:2943-2989 | DONE — `lang/src/lib.rs:46-47` | — |
| 52 | `ST_Distance(f, POINT(…))` as an ORDER BY term | sql.rs:3288 | DONE — `lang/src/lib.rs:62` | — |
| 53 | `ST_Area`, `ST_Length`, `ST_Perimeter` as predicates | sql.rs:3003-3016 | CONTRACT-T2 — §4.4 (listed T1 as row fns, not in the slice grammar) | projection-expression evaluator |
| 54 | `ST_Centroid(f)` | sql.rs:2231 | CONTRACT-T2 — §4.4 | same |
| 55 | `ST_AsGeoJSON(f)` in the SELECT list | sql.rs:2237-2241 | CONTRACT-T2 — refuse.rs `ST_ASTEXT` family (p3-geometry-io) | — |
| 56 | `ST_GeomFromGeoJSON('…')` inside INSERT VALUES | sql.rs:1816-1822 | DONE as a WHERE-SIDE geometry argument — `lang/src/parser/expr.rs`; `lang/tests/sql_tier1.rs` `the_four_geometry_predicates_match_the_direct_request`, `lang/tests/sql_prepared.rs` `a_geometry_predicate_rebinds_the_geometry_it_compares_against`. Inside `INSERT ... VALUES` it stays CONTRACT-T2 | an INSERT writes a geometry as its GeoJSON document today; the function form in value position needs the I/O surface (p3-geometry-io) |
| 57 | `POINT(lon lat)` / `POLYGON((…))` literal syntax | sql.rs:2931 | CONTRACT-T2 — QL §4.4 | the `ST_GeomFromText` WKT parser (p3-geometry-io) reached without the function name; I/O only. Axis order is longitude then latitude, stated |

### 6. Graph

| # | prior-engine capability | prior engine (file:line) | sekejap status | note |
|---|---|---|---|---|
| 58 | `SELECT … FROM MATCH (a)-[r]->(b)` spelling | sql.rs:48-53 | NOT ADOPTED — QL §1, §5 deviation 11 | the capability is T1 under `FROM GRAPH_TABLE (g MATCH … COLUMNS (…))`; §5 deviation 11 writes the mechanical migration, including `MATCH SHORTEST` → `ANY SHORTEST` and multi-FROM → `CROSS JOIN LATERAL`. A refusal with a named reason: the atomic exists, the spelling does not |
| 59 | Direction, edge type, `*a..b` var-length | sql.rs:57, usage/graph-queries.md:35-55 | DONE — `lang/src/lib.rs:70-75` (`GRAPH_TABLE`, hops, quantifiers) | — |
| 60 | `MATCH SHORTEST (a)-[r*]->(b)` | sql.rs:4771-4786 | CONTRACT-T2 — §4.3 `ANY/ALL SHORTEST` | unweighted shortest-path atomic (graph contract 5.3) |
| 61 | `PATH_AVG/SUM/MIN/MAX/PRODUCT/FIRST/LAST` | sql.rs:86-88 | CONTRACT-T2 — §4.3, graph contract 5.1 | streamed accumulators |
| 62 | Edge-property reads `r.field` in projection/WHERE | usage/graph-queries.md:79 | CONTRACT-T2 — §4.3 inline element WHERE / graph contract 4.2-4.3 | per-hop predicates are the pending item 1 |
| 63 | `INSERT ('a')-[:KIND {p: v}]->('b')` | sql.rs:57, 3417 | CONTRACT-T2 — §2 `INSERT INTO GRAPH … EDGE` | `put_edge` exists (`core/engine/src/index/graph/mod.rs`); only the surface is missing |
| 64 | `DELETE ('a')-[:KIND]->('b')`, edge UPDATE | sql.rs:3564, 3723 | CONTRACT-T2 — §2 | `delete_edge` at `core/engine/src/index/graph/mod.rs` |
| 65 | `SHOW EDGES [FROM t] [TO t]` | sql.rs:80, db.rs:11892 | DONE for `SHOW EDGES` — `lang/src/compile/rows.rs::show`, `Database::edge_shape`; `lang/tests/sql_catalog.rs` `db_edges_reports_the_collections_an_edge_type_connects`, `a_database_with_no_graph_answers_the_edge_views_empty_rather_than_failing`. `FROM t` / `TO t` are NOT ADOPTED and refused by name (`lang/src/parser/catalog.rs:182`) | the `(from, type, to, context)` quadruples of graph contract 2.5, derived from written edges, capped at 65,536 descents with a NOTICE when it stops. The filter is a `WHERE` over `db_edges`, and the refusal says so. A count per quadruple is a scan and is not built |
| 66 | Multi-FROM: `FROM MATCH (…), collection AS alias` | sql.rs:58 | CONTRACT-T2 — §4.8 `CROSS JOIN LATERAL` | one bounded traversal per driving row |
| 67 | `UNION` across MATCH results | usage/graph-queries.md:148 | CONTRACT-T3 — refuse.rs `UNION` | the prior engine loses this |
| 68 | `WITH` multi-stage traversal | usage/graph-queries.md:158 | CONTRACT-T2 — §2 non-recursive `WITH` | materialised once, row-budget bounded |
| 69 | Contexts / perspectives | *absent in the prior engine* | sekejap ahead — graph contract §3 | no parity work |

### 7. Aggregates, GROUP BY, HAVING, DISTINCT

| # | prior-engine capability | prior engine (file:line) | sekejap status | note |
|---|---|---|---|---|
| 70 | `COUNT(*) SUM AVG MIN MAX` | sql.rs:85, aggregate.rs | DONE — `core/engine/src/query/aggregate.rs` | — |
| 71 | `GROUP BY`, `HAVING`, `DISTINCT` | sql.rs:1964-1999, 1944 | DONE — §4.7 T1, refuse.rs note | one group key only in sekejap |
| 72 | `COUNT(DISTINCT x)` | sql.rs:2383 | CONTRACT-T2 — §4.7 | per-group distinct set is a second unbounded structure |
| 73 | `ORDER BY <aggregate alias>` | sql.rs:2001-2072 | DONE — §4.7 T1 | the one bounded in-memory sort |

### 8. Service and ops

| # | prior-engine capability | prior engine (file:line) | sekejap status | note |
|---|---|---|---|---|
| 74 | `open_as_service()` — one writer, snapshot readers | service.rs:1-70 | DONE — `dist/src/service/mod.rs:142` (`ServiceDatabase`), `:173` (`open`), `:209` (`writer`), `:249` (`reader`), `:504` (`close`); `dist/src/service/snapshot.rs:43` | Published view is `RwLock<Arc<Snapshot>>`, not `RwLock<Arc<Database>>`: `Database` carries per-handle `RefCell`/`Cell` caches and is `Send` but not `Sync`. A second writer process is still T3 and is refused by the page WAL's file lock |
| 75 | `ServiceDb::publish()` + stated staleness window | service.rs:16-26 | DONE — `dist/src/service/mod.rs:275` (`publish_now`), `:282`/`:289` (interval), `:666` (writer-side rate limit) | Window measured on 4,000 rows, four runs: one snapshot open 0.9–6.7 ms, one `publish_now` swap 0.9–2.0 ms, a commit visible to a new reader 100.3–108.5 ms after it under the 100 ms default. With no background thread, a reader past the interval pays the mint |
| 76 | `set_statement_timeout(Duration)` | db.rs:5008 | DONE — `core/engine/src/query/mod.rs:528` (`QueryBudget::deadline`), `:647` (`WorkResource::Deadline`), `:717` (`DEADLINE_POLL_CHARGES` = 1,024), `:778` (`check_deadline`); `dist/src/service/mod.rs:341` | The clock is read once per page and then once per 1,024 charges; no clock read at all on the no-deadline path. Still in addition to `QueryBudget`, never instead of it; a timeout never interrupts a commit (T3) |
| 77 | `interrupt_handle()` / `cancel()` / `clear_interrupt()` | db.rs:330-334, 5014-5021 | DONE — `dist/src/service/interrupt.rs:26`; `dist/src/service/mod.rs:377`/`:383`/`:388`, threaded into every `prepare_sql_with` and `next_page` by `:428` (`scan`) | Sticky until cleared, one relaxed load per charge |
| 78 | `subscribe_changes()` / `unsubscribe_changes()` — one event per committed batch | db.rs:4986-4998 | DONE — `dist/src/service/changes.rs:70` (`ChangeEvent`), `:205` (`deliver`); `dist/src/service/mod.rs:629` (`WriterGuard::commit`) | Delivered after the barrier, never on rollback. Both L1 bounds are stated constants: 256 events per subscriber (a full queue drops the NEW event and counts it in `lagged`) and 1,024 keys per event, past which the key list is dropped whole and the total reported. A durable replayable log is still T3 |
| 79 | `SHOW STATUS` (format, generation, counts, mode) | sql.rs:8134, exec.rs:519 | CONTRACT-T2 — OPS §6.1 (the rest of the `SHOW` family is DONE: `lang/src/compile/rows.rs::show`) | a `db_status` view over `storage_bytes` (`core/engine/src/collections/mod.rs`), `tracked_pages` (`:798`), `io_counters` (`io_counters`) and the format bits. All O(1); the node and edge counts are scans and are optional columns |
| 80 | `SHOW STORAGE` (live bytes per keyspace + files) | sql.rs:8132, exec.rs:463 | CONTRACT-T2 — OPS §6.2 (the rest of the `SHOW` family is DONE: `lang/src/compile/rows.rs::show`) | a tag-attributing walk over sekejap's tag-prefixed keyspaces plus the O(1) file sizes. A scan by definition, and the only statement in either contract whose cost is proportional to the database on purpose |
| 81 | `information_schema.{tables,columns,schemata,table_constraints,key_column_usage}` | catalog/mod.rs:30-36 | DONE — `lang/src/catalog.rs` (the five relations, their standard column names and order), `lang/src/compile/rows.rs` (the `Rows` driver), `docs/dist/PG_SURFACE.md` §8 | VIRTUAL rows over the catalog, exactly as planned: computed at prepare from `list_collections` / `collection_info` / `list_indexes` / `row_count`, no storage and no format bit. The PRIMARY KEY both constraint views name is the external key `_key`, which is the one key a collection has |
| 82 | `pg_indexes` | catalog/mod.rs:36, 112 | DONE — `lang/src/catalog.rs`; `lang/tests/sql_catalog.rs` `pg_index_and_pg_indexes_describe_the_same_indexes_db_indexes_does` | same, plus `pg_class`, `pg_attribute`, `pg_type`, `pg_namespace`, `pg_index`, `pg_constraint`, `pg_tables` and an empty `pg_description`; `indexdef` is built from the descriptor so it cannot disagree with the index it describes. The ten `pg_catalog` relations sekejap does NOT have are refused BY NAME rather than answered empty (`lang/src/refuse.rs`, `docs/dist/PG_SURFACE.md` §9) |
| 83 | Dictionary queries compose (WHERE/ORDER/LIMIT/DISTINCT over views) | catalog/mod.rs:135-180 | DONE — `lang/src/compile/rows.rs`; `lang/tests/sql_catalog.rs` `a_db_relation_composes_with_where_order_limit_and_distinct` | the clauses are the ones the parser already produced, applied over the bounded list the compiler built. `version()`, `db_version()`, `current_schema()`, `current_database()`, `current_user`, `pg_backend_pid()` and `current_setting()` are DONE as fixed rows; `postgis_version()` stays refused until the geometry I/O it advertises exists (p3-geometry-io) |

### 9. Types and nullability

| # | prior-engine capability | prior engine (file:line) | sekejap status | note |
|---|---|---|---|---|
| 84 | `TEXT INTEGER REAL BOOL TIMESTAMPTZ GEO VECTOR JSON` | sql.rs:1208-1217 | DONE — `lang/src/lib.rs:80-81` (`TEXT/INT/BIGINT/REAL/DOUBLE/BOOLEAN/JSONB/TIMESTAMPTZ/DATE/VECTOR(n)/GEOMETRY`) | sekejap adds DATE and typed GEOMETRY(Point, 4326) |
| 85 | `NOT NULL` on ADD COLUMN (parsed, *not enforced*) | sql.rs:4029 | DONE — QL §2, §5 deviation 12 | a descriptor flag checked when the row is assembled and after the defaults are filled (MISSING and NULL are distinct in sekejap), refusing the write and naming the column, and saying both are refused. sekejap **enforces** it where the prior engine only parses it, so a corpus that release accepted can be refused. `ADD COLUMN … NOT NULL` with no DEFAULT on a non-empty collection is T3 and refused by name. `core/engine/tests/column_rules.rs`, `lang/tests/sql_schema.rs` |

### 10. Other notable

| # | prior-engine capability | prior engine (file:line) | sekejap status | note |
|---|---|---|---|---|
| 86 | `write_trace` — per-phase transaction timing | write_trace.rs:1-22 | CONTRACT-T2 — OPS §8 | a feature-gated thread-local phase timer over sekejap's write path, one line per index family. Law 4: sekejap counts the I/O (`io_counters` `:947`, `pool_counters` `:892`) and does not yet attribute the time |
| 87 | `stats()` / `memory_report()` / `trim_memory()` | db.rs:10177, 11763, 11726 | CONTRACT-T2 — OPS §6.3 | sekejap's honest report is short — the pool arena (a ceiling, labelled), the index and layout caches, the reader slots — because sekejap holds nothing proportional to rows. That short list is Law 1 stated as a measurement. The prior engine's rule that an absent structure is never reported as `0` is adopted verbatim |
| 88 | Bulk load: `begin_bulk`/`put_value_bulk`/`link_many` | db.rs:11922-11987 | DONE — `core/engine/src/collections/write_set.rs` (`Database::begin_bulk` / `end_bulk`), SQL `BEGIN BULK` / `END BULK` | the scope is built and nesting-counted; the outermost close calls `commit` with the same FULL barrier and publication as any other commit. The two batched entry points are NOT adopted and the reason is written in OPS §7: sekejap's `put`/`delete`/`put_edge` already write with no durability point of their own, so a loop inside the scope IS the batch. `rollback` makes a failed batch all-or-nothing, and clears the scope with it |
| 89 | `prepare_insert` / `insert_prepared` | db.rs:10245-10261 | DONE — `lang/src/lib.rs:501` (`sql_prepare`) | — |

---

## Counts

Recounted against this tree by reading the status column of all 89 rows.

| status | at the tiering pass | at 0.17 (this tree) |
|---|---:|---:|
| DONE | 29 | **52** |
| CONTRACT-T2 | 55 | 32 |
| CONTRACT-T3 | 3 | 3 |
| NOT ADOPTED (named refusal, capability kept) | 1 | 1 |
| NOT IN CONTRACT | 0 | **0** |
| informational (sekejap ahead / prior engine absent) | 1 | 1 |
| **total rows** | **89** | **89** |

Twenty-three rows moved CONTRACT-T2 -> DONE since the tiering pass: the
membership algebra (26, 27, 28 and the prefix half of 29), the string and
date/time functions (32, 33, 35 and the single-range half of 36), the
catalog and `SHOW` family (17, 65, 81, 82, 83), the write-path schema rows
(8, 9, 11, 85), the predicated writes and the bulk scope (4, 5, 88), the
plan cache (21), `EXPLAIN` (19), `USING vamana` (49), the WHERE-side geometry
constructors (56), and OPS §1-§5 (74-78).

Five rows are DONE only in PART, and each says which part: 16 (`COMPACT` as a
call, not as a statement), 29 (`LIKE` prefix, not infix), 36 (`EXTRACT(YEAR)`
and `date_trunc`, not the multi-period forms), 56 (a geometry argument in a
WHERE, not inside `INSERT ... VALUES`), 65 (`SHOW EDGES`, without `FROM`/`TO`).

Still CONTRACT-T2, grouped by what they wait on:

- **the projection-expression surface (3):** 31 `CASE WHEN`, 34
  `JSON_ARRAY_LENGTH` with the `->`/`->>` family, 53-54 the PostGIS
  measurement functions. Stated plainly in `docs/lang/QL_CONTRACT.md` §7
  item 5: these are a SYNTAX ERROR today rather than a named Tier-2 refusal,
  which is the one place the "refused by name" rule is not yet kept.
- **the geometry I/O surface (4):** 55, 57, and the I/O half of 53-54.
- **graph T2 (8):** 60-64, 66, 67 (T3), 68.
- **the trigram family (2):** 30 `ILIKE`, the infix half of 29.
- **the typo-tolerant text family (4):** 39, 40, 42, 43, 44.
- **statements with a named atomic and no surface (8):** 7, 10, 15, 16, 18,
  20, 38, 72.
- **OPS (4):** 79, 80 (`SHOW STATUS` / `SHOW STORAGE`), 86 (`write_trace`),
  87 (`stats` / `memory_report` / `trim_memory`).

CONTRACT-T3 (3): 37 multi-key `ORDER BY`, 47 `<+>` / `VECTOR_L1`, 67 `UNION`
across MATCH results. NOT ADOPTED (1): 58 `FROM MATCH`, whose capability is
T1 under `FROM GRAPH_TABLE (...)`.

### T3 boundaries named inside a T2 row

The tier on a row is the tier of the capability as the prior engine ships it.
Five rows carry an explicit refusal for a form that has no bounded atomic, and
none of them is emulated:

| row | the T2 part | the T3 part, and why |
|---|---|---|
| 2, 5 | `FROM ALL` / `DELETE FROM ALL` as a concatenation driver | `FROM ALL` with a ranked `ORDER BY`: one order key across different layouts is not one key, and the N-way merge holds one cursor per collection, which is no budget dimension |
| 11 | ADD / DROP / RENAME COLUMN / RENAME TO, and ALTER TYPE within one `Kind` | ALTER TYPE across `Kind`s: it rewrites every row and re-encodes every scalar key, and no bounded resumable rewrite exists |
| 18 | a stored body, a bounded populate, a bounded clear | incremental maintenance: per-write delta propagation has no atomic |
| 85 | `NOT NULL` as a write-path check | `ADD COLUMN … NOT NULL` with no DEFAULT on a non-empty collection: the constraint is false the moment it is recorded |
| 76, 78 | statement timeout; the live change feed | a timeout that interrupts a commit (L3 has no fast path); a durable replayable change log (a second write path with its own retention and recovery) |

## The former NOT IN CONTRACT list (33), now tiered

Historical record of the tiering pass. Of these 33, eleven have since LANDED
and their rows above say DONE: the `SHOW` family, `SHOW EDGES`, the plan
cache, `DEFAULT now()`/`uuid4()`/`uuid5()`, the five `ALTER TABLE` forms,
`NOT NULL`, the predicated `UPDATE`/`DELETE`, `USING vamana`, and OPS §1-§5.
The wording below is left as it was written, because it says what each row
NEEDED, and that is still the record of why the tier was what it was.

Every row below was a capability the prior engine ships that sekejap's
contracts did not mention at any tier. Each now has a contract row with a tier,
or a written refusal.

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
write-path check (§2 and §5 deviation 12) — sekejap enforces what the prior engine only parses.

**Text (2) → §4.6, T2.** `SEARCH_SCORE()` as the Score leaf of `search()`,
normalised to [0,1]; `BM25_NORM` as `bm25/(bm25+k)` on the existing leaf. Both
exist so the prior engine's hybrid-ranking story (`docs/usage/queries.md:225`) has
comparable terms in one `ORDER BY` — a weight over an unbounded BM25 is not a
weight.

**Vector and spatial (2) → §4.5, §4.4, T2.** `USING vamana` joins the
`quantized` alias list; `POINT()`/`POLYGON()` WKT literals ride the
`ST_GeomFromText` parser, longitude before latitude, stated.

**Graph (2).** `SHOW EDGES` was T2 over graph contract 2.5's derived triples; it is DONE now.
`FROM MATCH` is **NOT ADOPTED**: the capability is T1 under
`FROM GRAPH_TABLE (…)` and §5 deviation 11 writes the migration.

**Service and ops (7) → `docs/dist/OPS_CONTRACT.md`.** Five of the seven are DONE: `open_as_service` (§1);
`publish` and the staleness window (§2); `set_statement_timeout` (§3);
`interrupt_handle`/`cancel` (§4); `subscribe_changes` (§5); `SHOW STATUS`
(§6.1); `SHOW STORAGE` (§6.2).

**Diagnostics (3) → `docs/dist/OPS_CONTRACT.md`, T2.** `stats`/`memory_report`/
`trim_memory` (§6.3); bulk load (§7); `write_trace` (§8).

## What the tiering found

1. **The largest gap was a missing document, not missing tier rows.** Group 8
   is now `docs/dist/OPS_CONTRACT.md`: service mode, publish, timeout,
   cancellation, the change feed, introspection, bulk load and `write_trace`,
   each with the prior engine's semantics cited, sekejap's ingredient cited,
   the atomic to build, the Law it must satisfy, and a tier.

2. **sekejap's publish barrier is one checkpoint cheaper.** The prior
   engine's `ServiceDb::publish` must `publish_generation()` — commit *and*
   checkpoint
   — before minting, because a snapshot there opens the newest published
   generation and commits alone still live in the WAL (`service.rs:120-122`).
   sekejap's `commit` publishes and `open_snapshot` reads the committed-WAL
   overlay, so read-your-own-writes is a snapshot open and nothing more.

3. **The prior engine's change feed has a Law 1 problem sekejap must not
   copy.**
   `ChangeEvent.keys` accumulates one `String` per changed key per batch,
   deduplicated only against the previous entry. Its own `put_value_bulk`
   reaches that with a single call. sekejap's contract caps the key list and
   degrades to a truncation flag and a count — and the Postgres wire arrives
   at the same answer independently, since a `NOTIFY` payload is capped at
   8,000 bytes.

4. **Write-time schema behaviour was surface-only, as suspected.**
   `Database::alter_collection` (`core/engine/src/collections/mod.rs`) already writes a
   new immutable Layout and repoints the catalog, so four of the five `ALTER
   TABLE` forms are descriptor work at O(fields). The two real constraints
   found while tiering: DROP COLUMN cannot renumber the slots, because dense
   rows are positional and removing a slot would re-interpret every later
   field. What SHIPPED solves it the other way round and more cheaply: the
   name leaves the new layout and no row is rewritten at all, because a dense
   row decodes under the IMMUTABLE layout it was written with, not under the
   current one. The same rule is why RENAME COLUMN is refused on a populated
   collection;
   and ALTER TYPE across `Kind`s is a whole-collection rewrite with no bounded
   resumable atomic, so it is refused rather than tiered.

5. **Materialized views did not need a refusal.** Every atomic they need
   exists — catalog storage for the body, the prepared query's bounded pages,
   `put`, and the `begin_drop_collection`/`drop_collection_step` machine for
   the clear. The line that keeps `CREATE VIEW` at T3 while this is T2 is
   worth stating once: a user view is a query rewrite at prepare time, which
   is a second planner path; a materialized view is rows in a collection.

6. **Two places where sekejap is stronger, now written down.** A failed
   bulk batch commits nothing (the prior engine leaves the earlier rows
   stored), and `NOT NULL` is enforced (it parses the constraint and does not
   check it). Both are migration notes, because both can refuse a workload
   that release accepted.

### 11. What 0.17 ships that the prior engine has no row for

Outside the parity count on purpose: it has no equivalent, so there is
nothing to be at parity with. Each line names what is built and the test file
that pins it.

| surface | what it is | built where | tests |
|---|---|---|---|
| SQL, as a language | a lexer, an AST, a recursive-descent parser, a compiler, `EXPLAIN`, a row/range function set, a catalog surface and a Tier-2/Tier-3 refusal table -- and NO second engine: every statement compiles to `prepare_query`, `prepare_aggregate`, `put`, `delete`, `create_*`, an alter step or a drop step | `lang/` (`sekejap-lang`) | `lang/tests/` -- `sql_tier1.rs`, `sql_explain.rs`, `sql_functions.rs`, `sql_prepared.rs`, `sql_refusals.rs`, `sql_schema.rs`, `sql_dml.rs`, `sql_dml_adversarial.rs`, `sql_catalog.rs`, `drop_collection.rs`, `aggregate_graph_adversarial.rs` |
| the catalog, as relations | `db_tables`, `db_columns`, `db_indexes`, `db_edges`, `db_contexts`, and the `pg_catalog` / `information_schema` / PostGIS views over them. VIRTUAL: computed at prepare, nothing stored, no format bit spent. A `pg_catalog` relation sekejap does not have is REFUSED BY NAME rather than answered empty | `lang/src/catalog.rs`, `lang/src/compile/rows.rs`; listed in `docs/dist/PG_SURFACE.md` | `lang/tests/sql_catalog.rs` (36 tests) |
| the PostgreSQL wire | SANS-IO: `pg::Connection::feed(bytes) -> bytes` is the whole protocol engine and touches no socket; `pg::server` is the `std::net` adapter; `sekejap-pg` is the binary. Simple and extended query, portals, cursors, `LISTEN`, `CancelRequest`, `statement_timeout`, and a closed SQLSTATE map in which every tier refusal is `0A000` | `dist/src/pg/`; `docs/dist/WIRE_CONTRACT.md` | `dist/tests/pg_wire.rs` (21 tests), `dist/tests/pg_server.rs` (9 tests, one of them against a real `psql` when installed) |
| the published Rust crate | `sekejap` 0.17.0: one handle (`Db`), one error, documents as `serde_json::Value`, SQL with `$n` parameters, edges through the graph atomics, `Tx` for many writes under one barrier, and the three counts NAMED as walks. It adds no execution | `dist/rust/`; `docs/dist/RUST_API.md` | `dist/rust/tests/api.rs` (23 tests) |
| the C ABI | `libsekejap` 0.17.0: 59 `extern "C"` functions over `Db`, for Swift, Kotlin, Dart, Go and C/C++. Opaque handles, JSON text for documents and rows, `NULL`/`-1` sentinels, a thread-local error with a CLOSED code, no panic across the boundary. It adds no execution either | `dist/ffi/` (`sekejap-capi`, lib name `sekejap`); header `dist/ffi/include/sekejap.h`; `docs/dist/C_ABI.md` | `dist/ffi/tests/abi.rs` (17 tests); `cd dist/ffi && make check` compiles and runs `examples/smoke.c` |
| the service surface | one writer, snapshot readers, `publish`, a statement timeout, an interrupt handle and one change event per committed batch | `dist/src/service/`; `docs/dist/OPS_CONTRACT.md` §1-§5 | `dist/tests/service.rs` (16 tests) |
| the language wrappers | C#, Dart, Go, Java/Kotlin, Lua, Node, Python and Swift, all over `dist/ffi/include/sekejap.h`. None is built from this workspace; `dist/bindings/README.md` is the index and says per target what is ported, what is published and what is not | `dist/bindings/wrappers/` | each wrapper's own suite, run in its own toolchain; React Native is NOT ported at 0.17 and not published |

Two of these change the shape of the parity question rather than answering a
row of it. The wire means a client that speaks PostgreSQL reaches sekejap with
no driver of ours at all, which the prior engine has no story for. The C ABI
means the wrappers stop being eight ports of an engine and become eight thin
bindings over one, which is why `docs/dist/FFI_CONTRACT.md` §8 had to write
down what became of every 0.16 symbol.
