# Query language contract — sekejap 0.17

Companion to `docs/lang/INDEX_CONTRACT.md` (which indexes exist without
being asked for, and which you declare), `docs/core/GRAPH_CONTRACT.md` (engine
semantics) and
`docs/dist/OPS_CONTRACT.md` (the runtime and ops surface: service mode,
publish, statement timeout, cancellation, change notifications,
introspection, bulk load, write trace).

This document fixes WHAT the language is: the specifications adopted, every
keyword and function in one of three tiers, the dialect deviations, and the
execution guarantee behind each construct. Lean by design: a construct is in
Tier 1 or 2 only if it compiles to a named atomic with a stated cost.

The language is BUILT. `lang/` is the crate `sekejap-lang`: a lexer
(`lang/src/lexer.rs`), an AST (`lang/src/ast.rs`), a hand-written
recursive-descent parser (`lang/src/parser/`), a compiler
(`lang/src/compile/`), `EXPLAIN` (`lang/src/explain.rs`), the row and range
functions (`lang/src/functions.rs`), the catalog surface
(`lang/src/catalog.rs`) and the Tier-2/Tier-3 refusal table
(`lang/src/refuse.rs`). A caller reaches it through the `SqlDatabase` trait
(`use sekejap_lang::SqlDatabase;`), through the published crate's `Db::query`
/ `Db::execute` / `Db::stream` / `Db::explain` (`docs/dist/RUST_API.md`),
through the C ABI (`docs/dist/C_ABI.md`), or over the PostgreSQL wire
(`docs/dist/WIRE_CONTRACT.md`).

**The tiers, as this tree means them.**

- **T1** — the parser accepts it, it compiles to an atomic that exists in
  this tree, and a NAMED TEST proves it. Every T1 row below carries the test
  file and the test name that pins it, and a runnable example in §8.
- **T2** — named, with the atomic it needs named, and NOT built. A statement
  that spells it is refused with that reason; the refusal is Tier 2 so a
  caller can tell "not yet" from "never".
- **T3** — refused by name and never emulated. The refusal text is the one
  `lang/src/refuse.rs` or the compile site actually returns.

Every T2 and T3 refusal is `SqlError::Refused` or `SqlError::Unsupported`,
which the PostgreSQL wire maps to `0A000 feature_not_supported`
(`dist/src/pg/types.rs::sql_error`). `SqlError::Refused` prints as
``refused: `KEYWORD` is Tier N -- <reason>`` (`lang/src/lib.rs:214`).

**The SQLSTATE a refused example carries.** Every `sql refused` block below
names `0A000 feature_not_supported`, and that is now the whole list: a
construct this tree cannot back is refused BY NAME, and a client reads one
code for it wherever the construct was written and whichever layer raised it.

- `0A000 feature_not_supported` — a TIER refusal: the keyword or the form is
  in `lang/src/refuse.rs`, or the compile site returns `SqlError::Unsupported`
  by name, or the ENGINE refused a leaf that has no membership set. The three
  paths differ in where the sentence is written and in nothing else a client
  can see.
- `42601 syntax_error` is what is LEFT for text that is simply wrong — a
  misspelt keyword, an unclosed parenthesis, a word in neither Tier 1 nor
  `refuse::TABLE`. It is no longer what a listed construct gets for standing
  in a position nobody tested for: `lang/src/parser/mod.rs::guard_here` asks
  the table wherever a place would otherwise be named, so `INTERSECT` and
  `EXCEPT` after a SELECT and `TRAIL` / `WALK` / `SIMPLE` inside a
  `GRAPH_TABLE` pattern are refusals like any other row
  (`lang/tests/refusal_by_name.rs`).
- `XX000 internal_error` is what a client RETRIES, so no named refusal may
  carry it. Four did: a `SHOW` word that is neither a collection nor a client
  setting, a predicate on a column with no scalar index, a fold with no
  expression index, and a boolean leaf with no set. The first three are fixed
  at the CAUSE (`lang` raises `SqlError::Unsupported`, not `SqlError::Engine`);
  the fourth is raised inside the engine's own error type and is recognised by
  `dist/src/pg/types.rs::sql_error` instead. Asserted over the wire by
  `dist/tests/pg_wire_refusals.rs`.

## 0. The fixture the examples run on

Every fenced `sql` block in this document is executed by
`dist/rust/tests/doc_examples.rs` against ONE fixture database, described in
`docs/lang/EXAMPLE_FIXTURE.md`. A block tagged `sql` must answer without an
error; a block tagged `sql refused` must be refused, and its first line
names the SQLSTATE and the construct. Blocks take no parameters: every value
in an example is written as a literal, because the harness binds none.

The blocks below assume one collection, one edge type and one graph context:

| what | declaration | index |
|---|---|---|
| collection `place`, 200 rows, keys `p000`…`p199` | | |
| `_key` | the external key every row is put under | the key mapping |
| `name` | `TEXT` | `btree` |
| `body` | `TEXT` | `gin` over `to_tsvector('simple', body)` |
| `kind` | `TEXT` | `btree`, and an expression index over `lower(kind)` |
| `born` | `INT` | `btree` |
| `rating` | `DOUBLE PRECISION`, written as JSON null in some rows | `btree` |
| `active` | `BOOLEAN` | `btree` |
| `at` | `TIMESTAMPTZ` | `btree` |
| `day` | `DATE` | `btree` |
| `loc` | `GEOMETRY(Point,4326)` | `gist` |
| `area` | `GEOMETRY(Polygon,4326)` | `gist` |
| `emb` | `VECTOR(4)` | `exact`, and a second `quantized` index |
| `tag` | `TEXT`, ABSENT from some documents | `btree` |
| `note` | `TEXT` | none, so a refusal has something to name |
| edge type `near`, property `weight` `REAL` | `place` → `place`, a chain `p000 -> p001 -> …` | the graph keyspace |
| graph context | `base`, the context every edge above is written in | |

`body` holds words from a small vocabulary that includes `kebun` and
`sawah`, so a `tsquery` over it matches rows. `kind` takes the values
`depot`, `farm`, `home`, `mill`, `park`, `port`, `school`, `shop`.

`place` is created with `WITH (index: none)` and every index above is then
declared by hand. `docs/lang/INDEX_CONTRACT.md` would otherwise index every
eligible column of it, and the table above would stop being the whole truth --
`note` in particular, whose whole job is to be the column a refusal can name.
A caller writing a table of their own gets the automatic indexes and writes
none of those statements.

`docs/lang/EXAMPLE_FIXTURE.md` §5 declares this shape under THESE names --
`place`, `near`, `base`, and every column and index of the table above -- and
`dist/rust/tests/doc_examples.rs::build_place` builds it, so the names in
these blocks and the names in that document are one set of names and there is
nothing to translate. That document also declares a second shape, `posts` /
`people` / `readings`, which other documents' examples use and these blocks do
not; the two live in one database and neither touches the other's names.

**The blocks run IN DOCUMENT ORDER on one database.** Some of them are a
sequence -- §8.1 creates `ex_town`, `ex_person` and `ex_account`, indexes
them, alters them and drops what it made -- and each depends on the blocks
above it. A few of them also change `place`: one row `p900` is inserted,
updated and deleted again; one `UPDATE` writes the unindexed `note` column of
whatever `born BETWEEN 1990 AND 1991` matches; one `DELETE ... CASCADE`
removes whatever `born = 1993` matches. None of them asserts a count, so a
fixture of any size answers them; but a harness that ran the blocks in some
other order would see `ex_town` created after it was indexed. Every object a
block creates is prefixed `ex_` so the fixture's own names are never touched.

## 1. Specifications adopted

| layer | specification | dialect reference |
|---|---|---|
| container | ISO/IEC 9075:2023 SQL | PostgreSQL 19 documentation; where ISO and Postgres differ, Postgres wins |
| graph | ISO/IEC 9075-16:2023 SQL/PGQ, `GRAPH_TABLE ... COLUMNS` | PostgreSQL 19 ch. 5.15; Oracle 23ai GRAPH_TABLE; patterns per GPML (Deutsch et al., SIGMOD 2022, arXiv 2112.06217), shared with ISO/IEC 39075:2024 GQL |
| spatial | PostGIS 3.6 function names and unit semantics | `docs/core/SPATIAL_FUNCTIONS.md`, `core/engine/tests/spatial_postgis_conformance.rs` |
| vector | pgvector 0.8 type and operators; pgvectorscale knobs where they map | `tools/battle50k_pg_cases.sql` |
| text | PostgreSQL tsvector/tsquery for boolean matching; BM25 ranking; search functions in the family the prior engine shipped (typo, prefix) | `core/engine/src/index/text/mod.rs` |
| catalog | `db_*` core rows; `pg_catalog` and `information_schema` as views | **T1** (`lang/src/catalog.rs`, `lang/src/compile/rows.rs`): the rows are VIRTUAL -- computed at prepare from `list_collections` / `collection_info` / `list_indexes` / `row_count` / `graph_names` / `edge_shape` into a typed value list, with the `Rows` driver of `lang/src/compile/rows.rs` over it. Nothing is stored and no format bit was spent. The relation list, the columns and the type OIDs are `docs/dist/PG_SURFACE.md`; the bound is the catalog's size, except `db_edges`, whose graph-shape probe is capped at 65,536 descents and says so when it stops |

Not adopted: standalone GQL statements, Cypher, AQL, SurrealQL, the `FROM
MATCH` of the prior engine, Google's `RETURN` inside GRAPH_TABLE (accepted as
an alias for `COLUMNS` only if measured demand appears). "The prior engine"
throughout this contract is the release sekejap replaces -- its own SQL
surface, still in production, its sources kept on branch `e1`.

## 2. Statements

The "test" column names the file and the test function that pins a T1 row.
Test files are `lang/tests/*.rs` unless another crate is written out.

| statement | tier | test | atomic / note |
|---|---|---|---|
| `SELECT ... FROM <collection> [WHERE] [ORDER BY one expr] [LIMIT]` | T1 | `sql_tier1.rs::select_star_names_every_declared_column`, `::an_indexed_scalar_order_matches_the_direct_request`, `::limit_is_a_total_limit_on_the_prepared_query` | prepare_query: filters AND, one order, pages |
| `SELECT ... FROM GRAPH_TABLE (...)` | T1 (patterns per §4.3) | `sql_tier1.rs::graph_table_compiles_to_one_bounded_traversal` | traversal driver/filter |
| `INSERT INTO t (...) VALUES (...)`, `$n` params | T1 | `sql_tier1.rs::insert_update_delete_walk_the_key`, `::a_parameter_takes_its_type_from_its_position` | put by key |
| `UPDATE t SET ... WHERE _key = $1` | T1 | `sql_dml.rs::the_key_forms_of_update_and_delete_stay_the_single_key_atomics` | put replaces the row; partial update = read-modify-put |
| `DELETE FROM t WHERE _key = $1` | T1 | `sql_dml.rs::the_key_forms_of_update_and_delete_stay_the_single_key_atomics` | delete by key; RESTRICT/CASCADE per graph contract 6.1 |
| `SELECT ... FROM ALL` (every collection at once) | T2 (parsed and REFUSED by name, `lang/src/compile/dml.rs::FROM_ALL`) | `sql_dml.rs::from_all_is_refused_by_name_in_both_statements_that_can_write_it` | a `Collections` concatenation driver over the catalog's collection ids in id order: each collection is one ordinary bounded driver walk, and the resume key is `(collection id, inner cursor)`. Work is proportional to the collections enumerated plus the candidates walked, and a `LIMIT` stops the concatenation at the collection it is reached in. `FROM ALL` with a ranked `ORDER BY` is **T3**: one order key across different layouts is not one key (the field may be absent, or a different `Kind`, in each collection), and merging N ordered walks holds one cursor per collection, which is no `QueryBudget` dimension. |
| `UPDATE t SET ... WHERE <any predicate>` | T1 | `sql_dml.rs::update_t_set_where_a_predicate_rewrites_exactly_the_rows_the_oracle_names`, `::a_set_expression_over_the_same_row_is_a_row_function_and_reads_no_other_row`, `::a_set_expression_over_the_driving_index_is_refused_and_names_the_index`, `sql_dml_adversarial.rs::a_rows_written_refusal_names_the_rows_it_already_wrote_and_they_are_pending_until_the_caller_speaks` | `Database::update_where` (`core/engine/src/collections/write_set.rs`): the prepared query's driver (`Projection::Ids`, `QueryOrder::Driver`, `CandidateDriver::Auto`) supplies candidates a PAGE at a time and each one is read-modify-put, so work is proportional to the rows matched, never to the collection. Bounded by the `rows_written` `QueryBudget` dimension: over budget the statement is refused with the count it reached, never silently truncated. Each page is materialised as ids BEFORE any write of that page (Law 3), and `WriteProgress::cursor` -- the rank key of the last row written -- resumes the next call. `SET column = <literal \| parameter \| row expression over the same row>`; a row expression is the §4.1 / §4.2 set, compiled by `lang` and applied by `core` through a `&mut dyn FnMut(&Value) -> Result<Value>` closure, so `core` holds no expression evaluator. **Deviation, stated:** an update whose SET writes the column the chosen driver's INDEX is over is REFUSED, naming the index -- writing it re-files the posting the walk is standing on, and a posting that moves forward is a row the pass would meet and update twice. The remedy the refusal names is an explicit `CandidateDriver::Entities`, which walks entity ids (a write moves none) and is a scan the caller asked for, per §6's rule that a scan is never taken silently. The statement commits nothing of its own: the caller's transaction does. **A refusal raised after the pass has begun writing NAMES the rows it already wrote.** A predicated write is not a transaction of its own: sekejap has one transaction per handle and no savepoint, so a statement cannot undo only its own rows without discarding the caller's earlier uncommitted work, which it has no right to do (Law 3). So the refusal says how many rows of the pass are already written and uncommitted, and the caller's `ROLLBACK` (which discards them with everything else uncommitted on the handle) or `COMMIT` (which publishes them) decides them. Enforced in `core/engine/src/collections/write_set.rs::refusal_names_the_rows_already_written`; a named budget and a cancellation keep their machine-readable `WorkResource` shape instead, and the `rows_written` refusal itself is raised by `lang/src/compile/plan.rs::run_write`, which holds the count in `WriteProgress`. |
| `DELETE FROM t WHERE <any predicate> [RESTRICT\|CASCADE]` | T1 | `sql_dml.rs::delete_from_t_where_a_predicate_removes_exactly_the_rows_the_oracle_names`, `::restrict_is_the_default_and_cascade_is_the_explicit_word`, `::delete_where_a_key_range_is_the_predicated_form_over_the_key_driver`, `sql_dml_adversarial.rs::delete_cascade_removes_the_edges_in_every_context_and_the_verifier_stays_clean` | `Database::delete_where` (`core/engine/src/collections/write_set.rs`): the same page-at-a-time driver walk feeding `delete`, with the same `rows_written` budget, the same Law-3 page materialisation and the same resumable cursor. Graph contract 6.1 per ROW: RESTRICT is the DEFAULT and refuses while any edge in any context references the candidate, naming those contexts (`Database::entity_edge_contexts`, one descent per (entity, context) pair with edges); CASCADE is the explicit word and removes the incident edges with the row. A `DELETE FROM t` with no predicate matches every row and is labelled a SCAN by EXPLAIN and by a notice. **A refusal raised after the pass has begun writing NAMES the rows it already wrote**, for the reason written in the `UPDATE` row above. |
| `DELETE FROM ALL [WHERE ...]` | T2 | `sql_dml.rs::from_all_is_refused_by_name_in_both_statements_that_can_write_it` | rides the `FROM ALL` driver above and is refused by the same named reason while it is unbuilt. |
| `INSERT INTO GRAPH g EDGE type (source, destination, props...) VALUES` | T2 (spelling open) | — | put_edge |
| `UPDATE GRAPH g EDGE type SET ... WHERE source = AND destination =` | T2 | — | edge posting rewrite |
| `DELETE FROM GRAPH g EDGE type WHERE ...` | T2 | — | delete_edge |
| `CREATE TABLE`, `CREATE INDEX ... USING {btree,gin,gist,exact,quantized,adjacency}`, `DROP INDEX [IF EXISTS]` | T1 | `sql_tier1.rs::create_table_and_create_index_build_a_queryable_collection`, `sql_automatic_index.rs::the_automatic_index_and_a_hand_written_create_index_for_the_same_column_do_not_produce_two_indexes` | catalog descriptors. `CREATE TABLE` also creates the AUTOMATIC indexes of `docs/lang/INDEX_CONTRACT.md` in the same statement -- every `TEXT`, `INT`/`SMALLINT`/`BIGINT`, `REAL`/`DOUBLE PRECISION`, `BOOLEAN`, `TIMESTAMPTZ`, `DATE` column a scalar `btree`, every `GEOMETRY` column the spatial family its declared shape names -- so a predicate answers immediately after it. `VECTOR(n)` and `JSONB` get nothing, for the reasons that contract states. A `CREATE INDEX` whose descriptor matches one already there (same family, same field, same expression, same uniqueness) creates NOTHING and answers with a NOTICE naming the index that is: an automatic index and a hand-written one over one column are ONE index. |
| `DROP TABLE [IF EXISTS] name [CASCADE\|RESTRICT]` | T1 | `drop_collection.rs::sql_drop_table_if_exists_and_cascade_end_to_end`, `::a_dropped_collection_leaves_every_keyspace_empty_and_its_name_free`, `::an_interrupted_drop_resumes_to_the_state_an_uninterrupted_one_reaches`, `sql_tier1.rs::drop_table_restricts_on_graph_edges_and_cascades_when_asked` | `begin_drop_collection` publishes a DROPPING mark the readers refuse, then `drop_collection_step(id, budget)` empties the indexes, sidecars, rows and mappings in bounded batches and removes the descriptor last; RESTRICT per graph contract 6.1 |
| `CREATE TABLE t (...) WITH (index:{none\|[...]}, hash:[...], range:[...], fulltext:[...], bm25:[...], spatial:[...], vector:[...], quantized:[...])` | T1 | `sql_index_sugar.rs::the_sugar_builds_exactly_the_catalog_the_long_hand_statements_build`, `::every_key_maps_to_the_family_the_contract_names_and_the_notice_says_so`, `::a_family_illegal_for_the_column_kind_is_refused_by_name`, `::an_unknown_key_is_refused_by_name_and_lists_the_keys_that_exist`, `::a_column_the_table_does_not_declare_is_refused_by_name`, `::the_generated_name_is_table_column_family_and_a_collision_is_refused_by_name`, `::a_refusal_part_way_through_the_clause_leaves_neither_the_collection_nor_an_index`, `::a_query_that_needed_one_of_those_indexes_answers_immediately_after_the_single_statement`, `::if_not_exists_over_a_collection_that_is_there_creates_no_index_and_says_so`, `::a_with_clause_that_names_no_key_is_refused_by_name_rather_than_ignored`, `sql_automatic_index.rs::with_index_none_creates_none_and_the_predicate_is_then_refused_exactly_as_before`, `::with_index_naming_one_column_creates_that_one_and_a_predicate_on_another_is_refused_naming_it`, `::an_explicit_fulltext_entry_still_wins_under_index_none` | sugar, no new atomic: one `create_collection` followed by one `CREATE INDEX` per named field inside the same statement, through the SAME `build_index` call the hand-written statement goes through (`lang/src/compile/plan.rs`). **Why it exists:** §6 refuses a predicate on an unindexed column and that is staying, so a caller who wrote only `CREATE TABLE` had five statements to write before one query answered. This removes the CEREMONY, not the EXPLICITNESS: every indexed column is still named by hand, in one place, and every index is ANNOUNCED. **Grammar:** `WITH (` *key* `:` `[` *column* [`,` *column*]… `]` [`,` *key* `:` `[`…`]`]… `)` after the closing parenthesis of the column list. `=` is accepted for `:`. The column list is BRACKETED and a bare name is a syntax error naming the form, because `hash: a, b` would otherwise read `b` as a second key. **Keys and families:** `hash` → `btree` (there is no separate hash family, and a `btree` answers equality); `range` → `btree` (one scalar family answers equality and range alike); `fulltext` → `gin` over `to_tsvector('simple', col)`; `bm25` → the same `gin` (BM25 is how that one text family SCORES, not a family of its own); `spatial` → `gist`, the point family for a `Kind::Point` column and the geometry family for a `Kind::Geo` one; `vector` → `exact`; `quantized` → `quantized`. A key whose family the column's `Kind` cannot carry — a `gist` on a TEXT column, a `gin` on an INT — is REFUSED by name with the key, the family and the `Kind`; there is no second-best family to fall back to. An unknown key is refused BY NAME with the eight that exist written out, never a bare syntax error, and PostgreSQL's storage parameters (`fillfactor` and the rest) are refused there. **The eighth key, `index:`,** is not a family for the columns it lists: it says which columns get the AUTOMATIC index of `docs/lang/INDEX_CONTRACT.md`, and its value is `none`, `all`, or a bracketed column list. It COMPOSES with the seven rather than replacing them -- an explicit `fulltext:`, `spatial:` or `vector:` entry is a declaration and is honoured whatever `index:` says -- and where a declared entry and an automatic one generate the same name (`spatial: [loc]` and the automatic point index over `loc`), that is ONE index and the declared entry's notice is the one raised. It is STATEMENT-SCOPED: no descriptor field and no feature bit, so a later `ALTER TABLE ... ADD COLUMN` of an eligible kind indexes that column whatever the `CREATE TABLE` said. `index:` written twice is refused by name, and a column it lists that the table does not declare, or whose `Kind` has no automatic family (`VECTOR`, `JSONB`), is refused by name before a byte is written. **Naming rule:** `<table>_<column>_<family>`, where `<family>` is the family the key BECAME — so `hash: [c]` and `range: [c]` generate one and the same `t_c_btree` and writing both is refused as the collision it is, rather than silently deduped. A generated name that is already an index ANYWHERE in the database is refused too, because `DROP INDEX <name>` resolves a bare name over the whole catalog. **Notices:** ONE per mapping, naming the column, the family it became, why, and the generated name; the statement answers `SqlResult::Notice`, which `dist/src/pg/frames.rs::notice_response` sends, so nothing is created that the caller was not told about. **A refusal part way through leaves NOTHING behind.** Every refusal the compiler can raise — unknown key, undeclared column, illegal family, colliding name — is raised before a byte is written. What is left is a refusal only the engine can raise (the 64-index ceiling, the 128-byte name bound, the identity space), and then the statement REMOVES what it had already committed: `begin_drop_collection_mode(Cascade)` and `drop_collection_to_end`, the same bounded phase machine `DROP TABLE` runs, taking the collection and every index of the clause with it, before the refusal is raised. That is a compensating removal and not a rollback — there is one transaction per handle and no savepoint, and an index build commits its own steps — so the caller's OWN uncommitted rows are committed before the statement begins and are never among what is removed. If the removal itself fails, the refusal says so and names the collection, which carries the DROPPING mark every reader refuses and which one `DROP TABLE ... CASCADE` finishes. |
| column `DEFAULT now()`, `DEFAULT uuid4()`, `DEFAULT uuid5(namespace, name)` | T1 | `sql_schema.rs::the_three_default_generators_fill_a_column_the_insert_does_not_name`, `::a_default_outside_the_closed_generator_set_is_refused_by_name`, `::the_rules_are_in_the_file_and_answer_after_a_reopen`; `core/engine/tests/column_rules.rs` | a per-field COLUMN RULE recorded in the collection descriptor (`ColumnRule`, `core/engine/src/collections/column_rules.rs`) behind the additive `COLUMN_RULES_FEATURE = 0x1000` (Law 8), filled on the write path when the row is assembled and the field is MISSING. An explicit NULL is a written value and defeats the default, as in PostgreSQL. The generator set is closed and each member is O(1) per row: one clock read shared by every `now()` column of that row, 16 bytes from `getrandom`, one SHA-1 over `namespace`+`name` (RFC 4122 §4.3). `now()` stores UTC microseconds; `uuid4`/`uuid5` store text. An arbitrary expression as a DEFAULT is refused by name -- that is the generated-column row below, under its own keyword. |
| `GENERATED ALWAYS AS (expr) STORED` | T2 (parsed and REFUSED by name, `lang/src/parser/ddl.rs:89`) | `sql_schema.rs::an_alter_form_with_no_atomic_is_refused_and_names_what_there_is` | the same descriptor slot holds a compiled row expression over other declared fields of the **same** row (the §4.1 / §4.2 row-function set), evaluated once the row is assembled and before the index-maintenance hook, so an index over a generated column is maintained exactly like an index over a written one. An expression that reads another row, an aggregate, or a subquery is T3: those are not row functions and have no per-write atomic. |
| `NOT NULL` on a column | T1 | `sql_schema.rs::not_null_refuses_a_missing_column_and_an_explicit_null_by_name`, `::add_column_not_null_without_a_default_is_refused_on_a_collection_with_rows`; `sql_catalog.rs::db_columns_reports_the_not_null_a_column_was_declared_with` | a per-field flag in the same descriptor slot as the default, checked when the row is assembled and AFTER the defaults are filled: a value that is MISSING or NULL (sekejap distinguishes them) refuses the write and names the column, and says both are refused. Nothing is written. `ALTER TABLE ... ADD COLUMN ... NOT NULL` with no `DEFAULT` on a non-empty collection is T3 and refused by name: every existing row would read MISSING, so the constraint is false the moment it is recorded. Add the column with a default, or add it nullable and fill it. |
| `ALTER TABLE t ADD COLUMN`, `DROP COLUMN`, `RENAME COLUMN old TO new`, `RENAME TO new_name` | T1 (`RENAME COLUMN` T1 on an empty collection, T3 on a populated one) | `sql_schema.rs::select_star_after_add_column_shows_the_new_column_and_its_default`, `::drop_column_removes_the_name_from_the_layout_and_leaves_the_rows_alone`, `::drop_column_drops_the_index_over_that_column_with_it`, `::select_after_rename_column_shows_the_new_name_and_the_rule_follows_it`, `::rename_to_moves_the_name_and_nothing_else` | `Database::alter_collection_rules` (`core/engine/src/collections/mod.rs`) writes a new immutable `Layout` and repoints the catalog in one commit, carrying the declared types and the COLUMN RULES of the surviving fields; no row is rewritten, so the cost is O(fields), not O(rows). ADD: existing rows read MISSING for the new field, which is already distinct from NULL. DROP: the name leaves the layout -- projection and predicates stop seeing it -- and the column's index is dropped with it by the ordinary bounded `drop_index_step`, committed before the layout is repointed. No row is re-interpreted, because a dense row decodes under the IMMUTABLE layout it was written with, not under the current one. That same rule is why `RENAME COLUMN` is T3 on a populated collection and says so: the old layout carries the old name, so every existing row would read MISSING under the new one, and renaming them would need a row rewrite with no bounded resumable atomic. `RENAME TO` is a name record only (`Database::rename_collection`): `CollectionId` does not change, so edges, indexes and the external-key mapping are untouched. ADD COLUMN of a kind `docs/lang/INDEX_CONTRACT.md` covers also creates that column's AUTOMATIC index, built over the rows already there and after the layout is repointed, so the predicate answers when the statement returns (`sql_automatic_index.rs::alter_table_add_column_of_an_eligible_kind_gets_its_index`). `RENAME COLUMN` on a column whose ONLY index is that automatic one drops it and re-earns it under the new name in the same statement -- the collection is empty, which the rename already required, so the index holds no entry to move; a HAND-WRITTEN index over the column is still the refusal naming it (`::a_rename_of_an_automatically_indexed_column_re_earns_the_index_under_the_new_name`). |
| `ALTER TABLE t ALTER COLUMN c TYPE new_type` | T1 when the new type maps to the same `Kind` (`INT` <-> `BIGINT`, `REAL` <-> `DOUBLE PRECISION`, `TIMESTAMPTZ` <-> `BIGINT`); T3 when the `Kind` changes, refused by name with BOTH `Kind`s in the refusal | `sql_schema.rs::alter_column_type_is_accepted_within_one_kind_and_refused_across_kinds` | the same-`Kind` case changes the declared spelling and leaves the encoding alone, so it is the descriptor rewrite above. A `Kind` change rewrites every row and re-encodes every scalar index key -- work proportional to the collection, with no bounded resumable atomic today. The shape it would need is the `begin_drop_collection` / `drop_collection_step` phase machine applied to a rewrite; until that exists the form is refused and names this. |
| `REINDEX [ON t] [USING method (field)]` | T2 | — (not a statement the parser begins; the word is a syntax error naming the place) | rebuild, no new atomic: drop the index tree and rebuild it through the sorted build (`core/engine/src/collections/rebuild.rs`) under the `IndexState` Building/Ready/Dropping machine that already makes a build resumable across a reopen. |
| `COMPACT` | T2 | — (not a statement the parser begins) | `Database::checkpoint` (`core/engine/src/collections/mod.rs:2156`): fold the committed pages into the data file and reset the WAL. It is reachable as a CALL today (`Db::checkpoint`, `sekejap_checkpoint`), not as a statement. Deviation, stated: `checkpoint` returns `Ok(false)` while any reader in any process holds a slot, so `COMPACT` would report *deferred* and return -- it never waits on a reader, and it is not Postgres `VACUUM`: nothing is reclaimed inside a table. |
| `CREATE SCHEMA` | T2 (refused by name, `lang/src/refuse.rs`) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | p2-schema-segment |
| `schema.table` | T1 for the schemas that exist | `sql_catalog.rs::a_schema_qualified_name_resolves_to_the_one_thing_it_can_mean` | there is exactly one user schema while `CREATE SCHEMA` is T2, so `public.t` IS `t` -- the qualifier is read and dropped (`lang/src/parser/mod.rs::source_name`). `pg_catalog.x` and bare `x` are the same relation, as in PostgreSQL, where `pg_catalog` is implicitly on every `search_path`; `information_schema.tables` and `.columns` resolve ONLY qualified, because their bare spellings are ordinary names a collection may have and a collection named `tables` must keep meaning itself. Three dotted segments are still the `CREATE SCHEMA` refusal. |
| `CREATE PROPERTY GRAPH name NODE TABLES (...) EDGE TYPES (...)` | T2 (refused by name) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | optional naming of a context + label map; nothing is built |
| `SET <client setting> = <value>`, `RESET <client setting>`, `SHOW <client setting>` | T1 | `sql_catalog.rs::a_client_setting_is_accepted_as_a_notice_and_read_back_from_a_constant`, `::every_statement_a_client_issues_on_connect_answers_without_an_error` | the connect-time chatter a driver sends (`client_encoding`, `DateStyle`, `TimeZone`, `application_name`, `search_path`, `extra_float_digits`, and the rest of the closed list in `docs/dist/PG_SURFACE.md` §4). A `SET` is accepted as a NOTICE that names the knob and the value and stores NOTHING -- a connection is a process here and there is no session settings object, so a silent `SET` would read as one that took effect. A `SHOW` answers from the constant this engine actually has, which is why `SHOW extra_float_digits` still says `1` after `SET extra_float_digits = 3`. The two knobs that DO change something keep `SET LOCAL` (§4.5). |
| `BEGIN [READ ONLY]`, `COMMIT`, `ROLLBACK` | T1 | `sql_tier1.rs::rollback_discards_an_uncommitted_write`; `dist/tests/pg_wire.rs::a_transaction_block_reports_its_status_and_a_rollback_leaves_no_row` | one writer, snapshot readers |
| `BEGIN BULK`, `END BULK` | T1 | `sql_dml.rs::begin_bulk_and_end_bulk_nest_and_only_the_outermost_close_commits`, `sql_dml_adversarial.rs::a_refused_predicated_write_inside_a_bulk_scope_leaves_the_counter_where_it_was`, `::a_snapshot_opened_inside_a_bulk_scope_sees_nothing_of_the_batch` | `Database::begin_bulk` / `end_bulk` (`core/engine/src/collections/write_set.rs`), `docs/dist/OPS_CONTRACT.md` §7. **The spelling is sekejap's and is fixed here:** `BEGIN BULK` / `END BULK`, not `BEGIN`/`COMMIT`, because the scope is not a transaction -- the single writer is already inside one -- it is a deferral of the DURABILITY POINT to the matching close. Scopes nest and the counter says how deep; only the OUTERMOST close calls `commit`, with the same FULL barrier and the same publication as any other commit. A close with no scope open is REFUSED rather than absorbed (the prior engine absorbed it because the call arrives from FFI; this is not that boundary), and a `ROLLBACK` discards the batch and the scope together, which is sekejap's all-or-nothing batch. |
| `DECLARE c BINARY CURSOR FOR ...`, `FETCH FORWARD n`, `CLOSE` | T2 in `lang` (refused by name); **T1 inside a wire session** | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` for the `lang` refusal; `dist/tests/pg_wire.rs::a_declared_cursor_fetches_forward_in_pages_and_then_closes` for the wire | BUILT in `dist/src/pg/connection.rs` (`declare_cursor:1286`, `fetch_cursor:1329`, `move_cursor:1352`, `close_cursor`) as a SESSION construct, not a compiled statement: a cursor is protocol state that outlives one `feed`, and `lang` compiles statements rather than holding sessions. **Deviation, stated** (`docs/dist/WIRE_CONTRACT.md` §6): a cursor pages its answer ONCE through `PreparedSql::for_each_row_with` -- so every page is charged against the budget and sees the deadline and the cancel -- and HOLDS the rows it has not handed out, because `PreparedQuery` borrows the `Database` handle for the life of the walk and has no resume cursor a protocol message could carry. The hold is bounded by `CURSOR_ROW_CAP` (65,536 rows) and `CURSOR_BYTES_CAP` (16 MiB) and an answer that passes either is REFUSED naming the ceiling, never truncated. A statement run with no row limit STREAMS and holds nothing. |
| `EXPLAIN <statement>` | T1 | `sql_explain.rs::every_battery_case_explains_its_driver_its_filters_and_its_counters`, `::five_explanations_in_full`, `::a_filter_says_how_it_is_answered`, `::the_counters_are_the_ones_the_direct_api_charges`, `::explain_drop_table_prints_the_phases_and_runs_nothing`; `sql_dml.rs::explain_of_a_predicated_write_prints_the_driver_the_bound_and_the_mode_without_running_it`; `sql_schema.rs::explain_alter_prints_the_new_layout_id_and_what_is_carried_without_running` | `lang/src/explain.rs` prints the plan: driver, filters and how each is answered, membership sets, order, range rewrites, row functions and the `QueryWork` counters the same statement charges through the direct API. `EXPLAIN DROP TABLE` prints the phases and runs nothing; `EXPLAIN ALTER TABLE` prints the new layout id and what is carried; `EXPLAIN UPDATE`/`DELETE` print the candidate driver, the bound and the mode of a PREDICATED write and run nothing. `EXPLAIN` at one key is refused by name: a point-get and a put have no candidate walk to print. **Deviation, stated:** the ROUTE decides which of these families a caller can reach. All of them answer through `SqlDatabase::sql` with the `EXPLAIN` keyword, which prepares and describes without executing. The published crate's three doors (`docs/dist/RUST_API.md` §3) carry the SELECT and aggregate families only: `Db::explain` is `explain_sql`, which RUNS the statement to explain it and refuses anything it cannot run, naming the `sql` route; `Db::query` and `Db::execute` refuse every `EXPLAIN`, because a plan is neither rows nor a count. So `EXPLAIN DROP TABLE`, `EXPLAIN ALTER TABLE` and `EXPLAIN` of a predicated write are `0A000` through `Db` today and are listed as unbuilt in §7. |
| `EXPLAIN ANALYZE <statement>`, `EXPLAIN (FORMAT ...)` | T2 (refused at the parser, `lang/src/parser/mod.rs:391`, with "EXPLAIN takes no options here") | `sql_refusals.rs::a_tier_one_statement_the_engine_refuses_is_not_a_tier_refusal` | the same plan, plus the statement run to completion under the caller's `QueryBudget`, with the `QueryWork` counters of every page (`core/engine/src/query/mod.rs:616`) printed beside it. Deviation, stated: what would be reported is logical work -- candidates, postings walked, row decodes, output bytes -- not a per-operator wall clock, because the engine keeps no per-operator timer. Wall clock would appear once, as a total. |
| `SHOW TABLES`, `SHOW <collection>`, `SHOW CREATE TABLE t`, `SHOW INDEXES [ON t]` | T1 | `sql_catalog.rs::the_show_family_is_the_db_rows_said_in_one_word`, `::show_create_table_prints_ddl_that_names_every_column_and_index` | sugar over the `db_*` catalog rows, as stated: each is one fixed `SELECT` over one catalog view (`lang/src/compile/rows.rs::show`), costing O(collections), O(fields), O(fields + indexes) and O(indexes) respectively. The row count is the LIVE ROW COUNT record (`core/engine/src/collections/row_count.rs`), one point read per collection, not the walk the prior engine paid; a size-in-bytes column is still absent and is `SHOW STORAGE` below. `SHOW <name>` is ambiguous by construction -- a collection and a client setting are both a bare word -- and is resolved against the catalog, the collection first. `SHOW CREATE TABLE` is built from the descriptor, so it cannot disagree with what is there. |
| `SHOW EDGES` | T1 | `sql_catalog.rs::db_edges_reports_the_collections_an_edge_type_connects`, `::a_database_with_no_graph_answers_the_edge_views_empty_rather_than_failing` | the `(from collection, edge type, to collection, context)` quadruples graph contract 2.5 derives from written edges, read by `Database::edge_shape` (`core/engine/src/index/graph/mod.rs`) and named by the interned edge-type records `Database::graph_names` reads. **The bound, stated:** the primary edge keyspace is `tag \| source \| context \| type \| destination` and the source's SEQUENCE sits between the collection and the context, so a quadruple is not a key prefix -- the walk pays one descent per distinct `(source entity, context, type, destination collection)` and seeks past each run, which is proportional to the source entities that have edges and never to the edges. It is capped at 65,536 descents and a truncated answer carries a NOTICE that says so, rather than being short and looking complete. `FROM t` / `TO t` are NOT accepted: the filter is a `WHERE` over `db_edges`, and the refusal says so (`lang/src/parser/catalog.rs:182`). A COUNT per quadruple is still a scan of the edge keyspace and is not built. |
| `SHOW STATUS`, `SHOW STORAGE` | T2 | — (neither is in the `SHOW` parser's word list; the parser answers "SHOW takes TABLES, EDGES, INDEXES [ON t], CREATE TABLE t, a collection or a setting name") | `docs/dist/OPS_CONTRACT.md` §6: the runtime facts and the per-keyspace byte report. The FILE half of `SHOW STORAGE` exists as a CALL and not as a statement -- `Db::storage()` (`dist/rust/src/db.rs:743`) and `sekejap_storage` (`dist/ffi/src/lib.rs:1742`) over `Database::storage_bytes` (`core/engine/src/collections/mod.rs:1110`). The per-keyspace attribution is a scan by definition and says so. |
| `CREATE MATERIALIZED VIEW name AS <select>`, `CREATE SEARCH VIEW name AS <select> WITH (autoindex)`, `REFRESH MATERIALIZED VIEW name` | T2 (`CREATE MATERIALIZED` is refused under the `CREATE VIEW` row of `lang/src/refuse.rs`) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | the body SQL is stored in the catalog and the view **is** a derived collection, not a rewrite: populate is the prepared query's own bounded pages writing rows through `put`; `REFRESH` is the bounded, resumable clear (`begin_drop_collection` / `drop_collection_step` on the derived collection) followed by that populate. `WITH (autoindex)` is the `CREATE TABLE ... WITH (...)` sugar above, restricted to the view's TEXT fields. Stated plainly: a view is a stale copy between REFRESHes and is never incrementally maintained -- incremental maintenance is T3, because per-write delta propagation has no atomic. This is exactly why a *user* `CREATE VIEW` stays T3 while this is T2: a user view is a query rewrite at prepare time, which is a second planner path; a materialized view is rows in a collection, and every atomic it needs already exists. |
| a REUSABLE `PreparedSql`: one compiled statement, re-bound with new parameters | T1 | `sql_prepared.rs` -- 27 tests, one per rebindable shape: `::a_scalar_equality_rebinds_to_a_new_value`, `::an_in_list_rebinds_every_leaf_of_its_union`, `::a_graph_pattern_rebinds_its_seed_key_at_bind_rather_than_at_prepare`, `::a_statement_whose_shape_is_decided_by_a_value_refuses_the_rebind_and_says_which`, `::a_semi_join_is_never_rebound_because_its_set_is_built_while_it_compiles`, `::a_missing_parameter_is_refused_at_bind_by_its_number`; `dist/rust/tests/api.rs::a_prepared_statement_compiles_once_and_rebinds_for_every_parameter_list` | `PreparedSql::bind` (`lang/src/lib.rs`), the typed slots of `lang/src/compile/bind.rs`. A compiled node keeps, beside the value it folded, the literal the statement wrote there -- but only when that literal is a `$n`, since a written constant cannot change. A bind refills those slots and parses, compiles and reads the catalog NOTHING. What a prepare folds instead of slotting is named: the CLOCK (`now()` / `current_date`, read once per compiled statement so every row of one answer sees the same instant), a §4.1 / §4.2 range rewrite whose pre-image is computed from the value, a semi-join's membership set, a `!term` tsquery whose COMPLEMENT shape the value decides, an INSERT/UPDATE document, a session knob. A statement that folds a parameter is marked `rebind: false`, says which `$n` and what folded it, and `EXPLAIN` prints the line; `bind` then compiles again from the statement PARSED once -- which is also where the new clock per execution comes from. Two things are always slots even when written as constants, because their value is the database's and not the text's: a scalar subquery, and a `GRAPH_TABLE` seed key (resolved to an entity id at bind, one point-get). |
| a bounded prepared-plan cache behind `sql_prepare` | T1 | `dist/rust/tests/api.rs::the_plan_cache_serves_db_query_and_a_hit_is_a_rebind`, `::the_plan_cache_evicts_at_its_entry_ceiling_least_recently_used_first`, `::a_statement_longer_than_the_statement_ceiling_is_never_cached`, `::a_ddl_statement_invalidates_every_plan_compiled_before_it`, `::the_cached_plan_answers_what_a_fresh_compile_answers_for_every_binding` | `dist/rust/src/plans.rs`: a least-recently-used map from statement text to the compiled plan, with three ceilings fixed at open -- `PLAN_CACHE_ENTRIES` (64), `PLAN_CACHE_BYTES` (256 KiB of statement text, summed) and `PLAN_CACHE_STATEMENT_BYTES` (8 KiB, the longest statement cached at all) -- so Law 1 holds whatever the workload does. `Db::query`, `Db::stream` and `Db::prepare`/`Statement` all go through it, a HIT is a REBIND, and `Db::cache_stats` reports entries, bytes, hits, misses, evictions and the ceilings. The cache key carries the catalog generation: a `CREATE`, `ALTER` or `DROP` bumps it and empties the cache, rather than serving a plan built against a layout that no longer exists. A statement that only changes ROWS does not bump it -- a plan holds no rows, and the one compiled form that holds a set of them (a semi-join) is never rebound and is compiled again instead. |
| `SELECT version()`, `db_version()`, `current_schema()`, `current_database()`, `current_user`, `pg_backend_pid()`, `current_setting('x')`, `SELECT <literal>` | T1 | `sql_catalog.rs::version_starts_with_postgresql_because_drivers_parse_it`, `::the_session_facts_a_driver_asks_for_on_connect_all_answer`; `dist/tests/pg_wire.rs::the_fixed_session_rows_a_client_reads_at_connect_are_answered` | fixed rows (`lang/src/catalog.rs`, `lang/src/parser/catalog.rs`). `version()` answers `PostgreSQL 16.0 (sekejap 0.17.0)`: drivers PARSE the string for the major number to choose their protocol features and their catalog queries, so the shape is PostgreSQL's and the honest part goes in the parenthesis PostgreSQL itself uses for the build; `db_version()` is the same fact with no costume. `current_user` is a constant and `pg_backend_pid()` is this process's own id, because sekejap has no authentication and a connection IS a process. `docs/dist/PG_SURFACE.md` §3. |
| `WITH name AS (SELECT ...)` non-recursive | T2 (refused by name) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | materialised once, bounded by QueryBudget rows |
| `UNION`, `INTERSECT`, `EXCEPT`, `WITH RECURSIVE`, `CREATE VIEW` (user), `CREATE TRIGGER`, window functions (`OVER`) | T3 (refused by name) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | no atomic; recursion is a GRAPH_TABLE pattern. All seven carry a `refuse::TABLE` row and all seven are refused where they are WRITTEN: a SELECT's tail asks the table rather than testing for words by hand (`lang/src/parser/select.rs::select`, one `guard_here`), so `UNION`, `INTERSECT` and `EXCEPT` after a `LIMIT` -- and any row added to the table later -- carry their own tier and reason, not `42601`. |

## 3. Predicates and operators (WHERE)

| construct | tier | test | atomic |
|---|---|---|---|
| `AND` | T1 | `sql_tier1.rs::a_conjunction_is_a_filter_list` | filter conjunction |
| a CONSTANT predicate at the top of a `WHERE` (`1 = 1`, `1 <> 1`) | T1 | `sql_catalog.rs::a_constant_predicate_is_folded_at_prepare_over_a_view_and_over_a_collection` | folded to its truth value by the parser: TRUE is no predicate at all and FALSE is a statement that returns no rows, which compiles to `LIMIT 0` -- a plan whose driver is never stepped. Nothing is emulated, because there is nothing left for an index to answer. pgjdbc writes `SELECT * FROM t WHERE 1<>1 LIMIT 1` to learn a result's COLUMNS without fetching a row, which is the shape this serves. A constant NESTED inside an `OR` or a `NOT` is refused by name (`lang/src/compile/predicates.rs`): there it is a boolean leaf, and a leaf that admits or rejects every candidate without reading one has no filter atomic. |
| `=, <>, <, <=, >, >=` on indexed scalar | T1 | `sql_tier1.rs::scalar_equality_matches_the_direct_request`, `::not_equal_is_the_complement_of_an_equality` | Scalar Eq/Range |
| `BETWEEN a AND b` | T1 | `sql_tier1.rs::scalar_range_and_between_match_the_direct_request` | Range |
| `IS NULL`, `IS MISSING` | T1 | `sql_tier1.rs::is_null_and_is_missing_are_different_questions` | Scalar IsNull/IsMissing (both read the row: NULL and MISSING share one nullish index key) |
| `_key BETWEEN`, `_key >=` (external key) | T1 | `sql_tier1.rs::a_key_range_walks_the_mapping_keyspace` | Key filter, key-order driver |
| `OR` on the same index, `IN (list)` | T1 | `sql_tier1.rs::a_disjunction_of_equalities_is_one_membership_set`, `::in_a_list_is_the_same_union_written_shorter` | union of ranges as one membership set (`QueryFilter::Any`, `core/engine/src/query/membership.rs`) |
| `NOT`, `<>` on an indexed scalar, `IS NOT NULL`, `NOT IN` | T1 | `sql_tier1.rs::not_equal_is_the_complement_of_an_equality`, `::is_not_null_is_the_complement_of_the_nullish_key`, `::not_before_a_group_is_de_morgan`, `::not_in_a_list_is_the_complement_of_the_union`, `::a_null_value_is_in_neither_half_of_a_complement` | complement (`QueryFilter::Not`); see the leaf rule below |
| `OR` across indexes, parenthesised groups | T1 | `sql_tier1.rs::a_disjunction_across_two_families_unions_two_sets`, `::a_parenthesised_group_binds_the_way_sql_says`, `::redundant_parentheses_do_not_change_what_compiles` | union of the leaves' membership sets |
| `EXISTS (subquery)`, `_key IN (subquery)`, `NOT EXISTS` | T1 | `sql_tier1.rs::exists_over_an_edge_type_is_a_semi_join`, `::a_semi_join_over_a_non_text_column_is_refused_naming_it`, `::a_semi_join_is_cancellable_while_it_compiles` | semi-join membership set (`QueryFilter::Ids`), and its complement; the subquery's projected column must be TEXT, because an outer row is named by its external key |
| a boolean leaf with no set: GEOMETRY, traversal, `JsonEq`, a text phrase, `IS NULL`/`IS MISSING` | T3 inside a boolean | `sql_tier1.rs::a_disjunction_with_a_geometry_leaf_is_refused_with_its_reason` | refused at prepare, naming the leaf; each is answered from the ROW, and a boolean evaluated per row reads every candidate's record. A POINT leaf is NOT one of these and composes: the point index's own postings ARE its set, which is the same universe the complement rule below takes a `NOT` against. The refusal is raised by the engine (`core/engine/src/query/plan.rs`) and reaches a client as `0A000`: `dist/src/pg/types.rs::sql_error` recognises the sentence all five leaves share, so a named refusal is never the one code a client retries. |
| `LIKE 'abc%'` | T1 | `sql_tier1.rs::a_string_function_in_where_is_a_text_key_range`, `sql_functions.rs::a_prefix_pattern_is_a_text_key_range_and_equals_the_filter` | text-key prefix range `[abc, abd)` on the column's own scalar index (`functions::prefix_successor`, §4.1); `starts_with(col, 'abc')` is the same range. A prefix whose successor is not valid UTF-8 (`'\u{ff}%'` is `C3 BF`, whose successor is `C3 C0`) is REFUSED with a named reason: a text bound is a `String`, and widening it to the replacement character would admit every value in between |
| `LIKE '%abc%'`, an interior `%` or `_`, `ILIKE` | T2 (refused with `lang/src/parser/mod.rs::LIKE_NOT_A_PREFIX`; `ILIKE` refused from `lang/src/refuse.rs`) | `sql_tier1.rs::a_multi_range_rewrite_is_refused_and_a_missing_expression_index_too`, `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | trigram index family (pg_trgm-compatible), new family under a feature bit. NOT demoted to a scan without it: §6 does not allow a scan to be taken silently, so the form is refused and names the family |
| `SIMILAR TO`, regex `~` | T3 (refused by name) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | no index atomic |
| a predicate on a column with NO index | refused for that statement (`SqlError::Unsupported`, raised at `lang/src/compile/mod.rs::index_for_expression`, so `0A000` on the wire -- it is not a tier row, but it is a named refusal and carries a named refusal's code) | `sql_refusals.rs::a_predicate_on_an_unindexed_column_says_why_it_cannot_compile` | there is no posting range to walk and §6 does not allow a scan to be taken silently; the refusal names the column and the index it would need |

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
geometry predicate keeps the plan the same query with `= 'x'` gets
(`sql_explain.rs::a_boolean_filter_is_answered_before_the_row_is_read`).

A disjunction never DRIVES by preference -- the driver is chosen from the
remaining conjuncts or from the order -- but when nothing else can walk, the
union set itself drives in ascending entity id (`QueryDriver::Membership`).
Its cost is one candidate per member and no posting and no record: the set is
already built and already charged.

## 4. Functions

### 4.1 String (Postgres names)

| function | tier | test | execution |
|---|---|---|---|
| `lower`, `upper`, `length`, `char_length`, `concat`, `\|\|`, `substring`, `substr`, `left`, `right`, `trim`, `btrim`, `split_part`, `replace`, `position`, `strpos`, `starts_with` | T1 | `sql_functions.rs::projected_row_functions_equal_rusts_own_computation`, `::concat_ignores_a_missing_value_and_the_operator_propagates_it`, `::substring_and_split_part_edges_equal_rusts_own_computation`, `::lower_equality_uses_the_expression_index_and_equals_the_filter`, `sql_tier1.rs::a_string_function_in_a_select_list_is_a_row_function` | `lang/src/functions.rs`: row functions on PROJECTED values (`CompiledRow::eval`, `lang/src/compile/row.rs`) -- one row in, one value out, no read of any other row, so the cost is proportional to the rows RETURNED and `EXPLAIN` prints each under `row functions`. In a WHERE: `col LIKE 'x%'` and `starts_with(col, 'x')` are a text-key prefix `Range` on the column's own scalar index; `lower(col) = x`, `lower(col) LIKE 'x%'` and `starts_with(lower(col), 'x')` need the EXPRESSION index `CREATE INDEX i ON t (lower(col))` (`IndexExpr::Lower`, a scalar index whose stored value is the fold, `core/engine/src/collections/catalog.rs`) and are REFUSED without it rather than scanned. `concat` ignores NULL, `\|\|` propagates it, as in Postgres. The parser's whole row-function name set is `lang/src/parser/mod.rs::ROW_FUNCTIONS`; a name outside it and outside `refuse::TABLE` is a SYNTAX ERROR naming the place, not a tier refusal |
| `CREATE INDEX i ON t (lower(col))` | T1 | `sql_functions.rs::lower_equality_uses_the_expression_index_and_equals_the_filter`, `::a_fold_without_its_expression_index_is_refused_rather_than_scanned`, `::the_verifier_is_clean_over_a_lower_index_with_mixed_case_values`, `::a_non_driving_expression_predicate_equals_the_brute_force_filter` | an EXPRESSION scalar index: descriptor version 3 behind an additive `EXPRESSION_FEATURE` bit, the same keys and the same walk as an ordinary scalar index over a value derived on the write path. The set of expressions is CLOSED (`lower` only) and each is O(value bytes) per write, which is what keeps the per-write hook bounded (Law 1). No row byte changes: the derived value lives in the index and nowhere else. Every row-side recomputation -- the verifier, a non-driving predicate, a rank key -- passes through the same expression the write path does. STATED LIMIT: Unicode lowercasing can LENGTHEN a string (`İ` is 2 bytes, its image is 3), so a value inside the 1024-byte scalar text-key limit can have an image outside it; that write is refused with a named reason at the put and at the late build, never truncated to a key that is not the expression's value |
| `ILIKE` | T2 | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | see §3 (trigram) |
| `CASE WHEN <cond> THEN <value> [WHEN ...] [ELSE <value>] END` | T2 (refused by name) | `refusal_by_name.rs::the_projection_expression_constructs_are_refused_by_name_in_both_positions`, `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | a row expression: one row in, one value out, no read of any other row. In `ORDER BY` it is one key (§5 deviation 3), so it rides the same order-expression leaf an arithmetic blend does. In `WHERE` it is row-bound -- it never becomes an index range -- and `EXPLAIN` labels it so. |
| `col->>'member'` on a `Kind::Json` field, in a `CREATE INDEX` target or a WHERE equality | T1 | `sql_json_path.rs::an_extracted_member_equality_names_exactly_the_rows_the_brute_force_extraction_names`, `::a_row_whose_member_is_absent_is_not_findable_by_an_equality_on_any_value`, `::an_extracted_member_equality_that_is_not_the_driver_equals_the_brute_force_filter`, `::a_json_member_predicate_without_its_expression_index_is_refused_naming_the_index_it_needs`, `::two_indexes_over_two_members_of_one_column_do_not_answer_each_others_predicate`, `::an_extracted_member_index_survives_a_reopen_and_still_answers`, `::the_verifier_is_clean_after_writes_that_change_the_extracted_value`, `::the_json_operators_with_no_index_atomic_are_still_refused_by_name`, `::an_extracted_member_index_over_a_column_that_is_not_json_is_refused_by_name`, `index_json_expression.rs::a_missing_member_a_null_and_a_non_scalar_each_store_the_one_null_key`, `::the_member_name_travels_in_the_descriptor_and_comes_back_across_a_reopen`, `::a_json_path_expression_file_is_unsupported_to_a_binary_that_predates_the_bit` | TWO positions and no others: `CREATE INDEX i ON t ((col->>'member'))` builds the EXPRESSION scalar index `IndexExpr::JsonText` (descriptor version 4 behind `JSON_EXPRESSION_FEATURE = 0x10000`, `core/engine/src/collections/catalog.rs`), and `WHERE col->>'member' = v` is a scalar `Eq` on exactly that index. The member is part of the index's IDENTITY, so an index over one member does not answer a predicate over another; without a matching index the predicate is REFUSED naming the index it would need, never scanned (§6). The extraction is TOTAL: a string is itself, a number and a boolean are their canonical JSON text, and JSON null, an absent member, an object, an array and an absent column all store the NULL key -- which no equality on a value can name, so a row whose member is absent is findable by none of them (`docs/lang/INDEX_CONTRACT.md`). An ordering comparison over the extracted value is refused: the rewrite the contract names is an equality |
| `->`, `#>`, `#>>`, `json_array_length` on a `Kind::Json` field, and `->>` outside the two positions above | T2 (all refused by name from `lang/src/refuse.rs`; `#>` and `#>>` are lexed as their own tokens so the operator is named rather than reported as an unexpected character) | `refusal_by_name.rs::the_projection_expression_constructs_are_refused_by_name_in_both_positions`, `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason`, `sql_json_path.rs::the_json_operators_with_no_index_atomic_are_still_refused_by_name` | row functions over the binary JSON the row codec already decodes (`core/engine/src/lib.rs`): one row, no extra read. `->` returns the JSON VALUE rather than its text and has no scalar index key, so there is nothing for an expression index to store; `#>` and `#>>` walk a path array, which is a second shape the descriptor does not carry. They wait on the PROJECTION-EXPRESSION surface (§7 item 5). |
| `regexp_replace`, `regexp_match`; `coalesce` | T3 for the `regexp_*` pair, T2 for `coalesce` (all refused by name) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | `regexp_*` has no atomic. `coalesce` is a row function on projected values; a text index spans one declared field, so the concatenation Postgres builds with `coalesce` is a stored field here |

### 4.2 Date/time (Postgres names; storage = Int microseconds, declared TIMESTAMPTZ / DATE)

| function | tier | test | execution |
|---|---|---|---|
| `EXTRACT(YEAR FROM t) <cmp> n`, `EXTRACT(YEAR FROM t) BETWEEN`, `date_trunc('unit', t) <cmp>\|BETWEEN lit`, `t::date <cmp> lit`, `t <cmp> lit`, `t BETWEEN lit AND lit` in WHERE | T1 | `sql_functions.rs::extract_year_equality_is_one_range_and_equals_the_brute_force_filter`, `::extract_year_ordering_and_between_are_one_range_each`, `::date_trunc_equality_and_between_equal_the_brute_force_filter`, `::a_bare_comparison_against_a_date_literal_is_a_range`, `::a_cast_to_date_is_one_days_range`, `::date_trunc_inequalities_with_an_off_boundary_literal_equal_the_oracle`, `::an_extract_year_literal_outside_the_representable_range_is_answered_not_wrapped`, `sql_tier1.rs::a_date_time_function_in_where_is_one_scalar_range` | ONE scalar `Range` on the column's own index, folded at prepare (`Compiler::time_filter`, `lang/src/compile/functions.rs`): the function is never evaluated per candidate and `EXPLAIN` prints the fold under `range rewrites`. A literal off the unit's boundary is its own comparison, as in Postgres: `=` is an EMPTY range (no truncation equals an interior instant), and BOTH inequalities cut at the boundary ABOVE the literal, so `date_trunc('month',t) >= '1950-01-15'` excludes January and `< '1950-01-15'` keeps it. `BETWEEN` carries that rule in its lower half only. Still one range in every case, never a refusal. An `EXTRACT(YEAR ...)` literal outside the representable year range saturates at the ends of the stored microsecond order, which is the same answer without an overflow. INSERT accepts the same ISO-8601 and Postgres date/time literal forms and stores the integer; SELECT prints the column back as an ISO-8601 string -- through `SELECT col`, `SELECT *`, `col::text`, a string function's argument, `concat`, `\|\|`, and a `GROUP BY` key, which are one rule and cannot disagree |
| `EXTRACT(MONTH\|DAY\|DOW\|HOUR\|MINUTE\|SECOND FROM t) <cmp> n`, and `<>` over any date window | T2 (refused with `lang/src/refuse.rs::MULTI_RANGE`) | `sql_functions.rs::a_rewrite_that_would_need_a_union_is_refused_with_the_named_reason`, `sql_tier1.rs::a_multi_range_rewrite_is_refused_and_a_missing_expression_index_too` | the pre-image is a SET of ranges -- one interval per period in the corpus -- which is exactly the membership-set union `OR` and `IN (list)` compile to (§3). Refused by name with that reason until the union is wired to this rewrite; never emulated by a scan (§6) |
| same in SELECT / GROUP BY | T1 in SELECT (row function, `CompiledRow`); GROUP BY over a function is T2 | `sql_functions.rs::projected_date_functions_equal_rusts_own_computation`, `::select_star_prints_a_declared_timestamp_as_iso_text`, `::a_group_by_on_a_declared_timestamp_reports_the_key_as_iso_text`, `sql_tier1.rs::a_date_time_function_in_a_select_list_is_a_row_function`, `::a_folded_answer_refuses_what_it_cannot_report` | a folded answer returns groups, not rows, so a row function in a folded select list is refused and names `GROUP BY col` as the spelling. GROUP BY streams when the index order equals the truncation order -- that shape is the §4.7 atomic and is unbuilt for a function key |
| `now()`, `current_date`, `current_timestamp`, `interval` arithmetic, `age(t)`, `t + interval` | T1 | `sql_functions.rs::a_clock_relative_predicate_is_folded_at_prepare_and_is_one_range`, `::age_and_interval_arithmetic_are_microseconds_over_one_folded_clock`, `::age_lies_between_two_clock_reads_this_test_made`, `::a_calendar_interval_is_refused_because_it_folds_to_no_constant` | constants folded ONCE at prepare, so every row of one answer sees one instant; `age` and every interval are microseconds (§5 deviation 8). A CALENDAR interval (`interval '1 month'`, `'1 year'`) is T3 in this shape: a month is 28-31 days, so there is no constant to fold, and `date_trunc('month', t)` is the calendar-aware spelling that IS accepted |
| `to_char(t, fmt)`, `to_timestamp`, `to_date` | T1 for the named templates | `sql_functions.rs::projected_date_functions_equal_rusts_own_computation`, `::an_unnamed_to_char_template_is_refused_at_prepare` | `to_char` carries `YYYY-MM-DD`, `YYYY-MM`, `YYYY`, `HH24:MI`, `HH24:MI:SS` and `YYYY-MM-DD HH24:MI:SS`, checked at PREPARE so an unnamed template is a refusal rather than an error on the first row. A general Postgres template is a formatting language of its own and is T3 |
| time zones other than UTC storage (`AT TIME ZONE`) | T3 (refused by name) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | declared TIMESTAMPTZ is stored UTC; display conversion only |

### 4.3 Graph (SQL/PGQ names)

| function / construct | tier | test | atomic |
|---|---|---|---|
| element pattern `(v IS label)` / `(v:label)`, edge `-[e IS type]->`, `<-`, `-` | T1 | `sql_tier1.rs::graph_table_compiles_to_one_bounded_traversal`, `aggregate_graph_adversarial.rs::n_sql_graph_table_forms` | direction + type on BFS |
| inline `WHERE` in element, edge (`-[r:t WHERE r.p > v]->`) | T1 | `sql_tier1.rs::graph_table_inline_edge_where_compiles_to_a_per_hop_prune`, `sql_explain.rs::explain_prints_the_edge_predicates_and_the_node_membership_sets` | per-hop edge predicates over the inline bag (graph contract 4.3) |
| inline `WHERE` in element, far node, membership-able (`=`, range, `BETWEEN`, `ST_DWithin`, `ST_Within` on a point) | T1 | `sql_tier1.rs::graph_table_inline_node_where_compiles_to_a_membership_prune`, `aggregate_graph_adversarial.rs::o_node_predicate_membership_overflow_needs_67_million_sequences` | per-hop node predicates over index postings (graph contract 4.3) |
| inline `WHERE` in element, far node, row-bound (`IS NULL`, `IS MISSING`, text, geometry, JSON) | T2/T3 | `sql_tier1.rs::a_row_bound_inline_node_predicate_is_refused_with_its_tier` | a per-hop predicate is answered index-side; these need the row, which graph contract 4.3 forbids per hop |
| `COLUMNS (r.<prop> AS name)` on the edge element | T1 | `sql_tier1.rs::graph_table_columns_project_the_reaching_edge_and_order_by_it`, `::an_edge_alias_in_columns_is_matched_without_case`, `aggregate_graph_adversarial.rs::k_reaching_edge_equals_bag_first_admitted_across_types` | the reaching edge, bound by the traversal (graph contract 4.2) |
| `ORDER BY <edge column alias>` | T1 | `sql_tier1.rs::graph_table_columns_project_the_reaching_edge_and_order_by_it`, `aggregate_graph_adversarial.rs::l_edge_order_missing_text_last_paged_refuses_entities_keys` | rank by the reaching edge's property |
| `{n,m}`, `{n,}`, `+`, `?` quantifiers | T1 | `sql_tier1.rs::graph_table_compiles_to_one_bounded_traversal`, `aggregate_graph_adversarial.rs::j_five_hundred_sampled_traversals_equal_brute_force` | min/max depth |
| label alternation `type1\|type2` | T2 | `sql_refusals.rs::graph_constructs_beyond_the_slice_name_their_tier` | multi-type hop (two ranges per hop) |
| post-pattern `WHERE` | T1 | `sql_tier1.rs::a_key_post_filter_beside_a_graph_table_keeps_the_traversal_driving` | post-filter on rows |
| `COLUMNS (expr AS name)` | T1 | `sql_tier1.rs::graph_table_columns_project_the_reaching_edge_and_order_by_it` | projection |
| `path_length()` | T2 (refused by name) | `sql_refusals.rs::graph_constructs_beyond_the_slice_name_their_tier` | frontier depth accumulator |
| `path_sum/product/min/max/avg/first/last(e.prop)` | T2 (refused by name) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | accumulators (graph contract 5.1) |
| `p = ...`, `nodes(p)`, `edges(p)` | T2 (refused by name) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | path rebuild for returned rows |
| `ANY SHORTEST`, `ALL SHORTEST` | T2 (refused by name) | `sql_refusals.rs::graph_constructs_beyond_the_slice_name_their_tier` | shortest-path atomic, unweighted |
| `VERTEX_ID(v)`, `EDGE_ID(e)` | T2 (refused by name) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | entity id; edge id (after element identity) |
| `IS ACYCLIC` (default), `TRAIL`, `WALK`, `SIMPLE` | T1 default only; others T3 (refused by name) | `sql_refusals.rs::graph_constructs_beyond_the_slice_name_their_tier` | contract 4.1. The three words are refused by name where SQL/PGQ puts them: the pattern parser asks the table before it demands the `(` (`lang/src/parser/graph_table.rs::graph_table`), so `MATCH TRAIL (...)` inside `GRAPH_TABLE` is the Tier-3 refusal and not `42601`. The same sweep fixed `ALL SHORTEST`, which one hand-written `if` had been refusing under the name `ANY SHORTEST`. |
| negative patterns, `NOT EXISTS { pattern }` | T3 | — | no atomic |
| a three-part name inside `GRAPH_TABLE`, or an element name that belongs to another element | syntax error naming the place | `sql_tier1.rs::a_graph_table_refuses_a_name_that_belongs_to_another_element` | |

### 4.4 Spatial (PostGIS names)

| function | tier | test | atomic |
|---|---|---|---|
| `ST_DWithin(geog, geog, m)` | T1 | `sql_tier1.rs::st_dwithin_on_a_point_column_is_a_radius`, `::the_four_geometry_predicates_match_the_direct_request`; `core/engine/tests/spatial_postgis_conformance.rs` | Point Radius / Geometry DWithin (spheroidal) |
| `ST_Intersects`, `ST_Within`, `ST_Contains`, `ST_Covers`, `ST_Crosses` | T1 | `sql_tier1.rs::the_four_geometry_predicates_match_the_direct_request`, `::st_within_an_envelope_on_a_point_column_is_a_bbox` | Geometry filters (units per `docs/core/SPATIAL_FUNCTIONS.md`) |
| `ST_MakePoint(lon, lat)`, `ST_Point`, `ST_SetSRID(g, 4326)`, `ST_MakeEnvelope(w, s, e, n, 4326)`, `ST_GeomFromGeoJSON(text)`, the `::geography` / `::geometry` casts | T1 **in geometry-argument position** | `sql_tier1.rs::st_dwithin_on_a_point_column_is_a_radius`, `::st_within_an_envelope_on_a_point_column_is_a_bbox`, `::the_four_geometry_predicates_match_the_direct_request`; `sql_prepared.rs::a_rectangle_over_a_point_column_rebinds_its_four_corners`, `::a_geometry_predicate_rebinds_the_geometry_it_compares_against` | the constructors a predicate's right-hand side or a distance order's centre is written with. They are NOT general row functions: the parser reads them only where a geometry is expected (`lang/src/parser/expr.rs`), so `SELECT ST_MakePoint(1,2)` is a syntax error. A SRID other than 4326 is refused |
| `&&` with `ST_MakeEnvelope` | T2 (refused by name) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | Bbox filter on point/geometry index (p3-geometry-io). `ST_Within` against an envelope is the T1 spelling of the same rectangle, and the refusal says so |
| `<->` (kNN, `ORDER BY loc <-> pt::geography`), `ST_Distance(col, pt::geography)` as an order key | T1 | `sql_tier1.rs::the_knn_operator_is_the_distance_order`; `sql_prepared.rs::a_nearest_order_rebinds_its_centre_and_keeps_its_order` | Distance order (point index) |
| a distance, `<->` or `ST_Intersects` NOT marked geography (no `::geography` on either argument, and for `ST_DWithin` no fourth `true`); `ST_Within`/`ST_Contains` WITH `::geography`, or with a shape of SRID 0 | refused by name, with the spelling to use | `sql_spatial_units.rs` (all five tests) | none: PostGIS reads the first as degrees or the flat plane, and refuses the other two, so an accepted statement means the same thing in PostGIS (`docs/core/SPATIAL_FUNCTIONS.md`, "The unit is the type") |
| `ST_AsBinary`, `ST_AsEWKB`, `ST_GeomFromWKB`, `ST_GeomFromEWKB`, `ST_AsText`, `ST_GeomFromText`, `ST_AsGeoJSON`, `ST_X`, `ST_Y`, `ST_SRID` | T2 (refused by name where `refuse::TABLE` lists the spelling) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | pure I/O functions; SRID per column (p3-geometry-io) |
| `POINT(lon lat)`, `POLYGON((lon lat, ...))` as a literal in value position | T2 | — (not a spelling the parser reads; it is a syntax error naming the place) | I/O only: the WKT text parser `ST_GeomFromText` already needs (p3-geometry-io), reached without the function name around it. Axis order is longitude then latitude -- the same order `ST_MakePoint(x, y)` takes and the same order the prior engine writes -- and the contract says so here because the reverse is the classic import bug. |
| `ST_Area`, `ST_Length`, `ST_Perimeter`, `ST_Centroid` as row functions | T2 (refused by name) | `refusal_by_name.rs::the_projection_expression_constructs_are_refused_by_name_in_both_positions`, `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` (the pure functions exist in `core/engine/src/index/spatial/geometry.rs`, re-exported as `sekejap_core::spatial_geometry`; what is missing is the projection-expression surface over them, and the refusal says so) | pure functions over the geometry the row already decodes |
| `ST_Simplify`, `ST_SnapToGrid`, `ST_RemoveRepeatedPoints` | T2 (`ST_Simplify` refused by name; the other two are a syntax error) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | pure functions (QGIS render path) |
| `ST_Transform` | T2 (refused by name; later) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | PROJ; storage stays WGS84 |
| `ST_Buffer`, `ST_Union`, `ST_Intersection`, `ST_Difference`, `ST_SimplifyPreserveTopology` | T3 (refused by name) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | GEOS overlay; no pure-Rust substitute accepted |
| raster, topology, `ST_AsMVT` | T3 (`ST_AsMVT` refused by name) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | no atomic |
| `postgis_version()` | T2 (refused by name) | `sql_catalog.rs::a_pg_catalog_relation_this_surface_does_not_have_is_refused_by_name` | a fixed row, withheld until the geometry I/O it advertises exists: a client reads it as a PROMISE that `ST_AsBinary`, `ST_GeomFromWKB` and `&&` answer. `geometry_columns` and `spatial_ref_sys` ARE answered (`sql_catalog.rs::geometry_columns_names_every_geometry_column_with_srid_4326`, `::spatial_ref_sys_holds_exactly_one_row_and_it_is_4326`) |

### 4.5 Vector (pgvector / pgvectorscale)

| construct | tier | test | atomic |
|---|---|---|---|
| `VECTOR(n)` type, `'[...]'::vector` literal | T1 | `sql_tier1.rs::create_table_and_create_index_build_a_queryable_collection`, `::the_cosine_operator_is_the_exact_vector_order`; `dist/rust/tests/api.rs::a_vector_column_round_trips_as_a_json_array` | Kind::Vector |
| `<=>` cosine, `<->` L2, `<#>` negative inner product | T1 | `sql_tier1.rs::the_cosine_operator_is_the_exact_vector_order`; `sql_prepared.rs::a_vector_order_rebinds_its_query_vector` | ExactVector / ApproximateVector order (Cosine, SquaredL2, NegativeDot) |
| `ORDER BY emb <=> $v LIMIT k` | T1 | `sql_tier1.rs::the_cosine_operator_is_the_exact_vector_order`; `sql_explain.rs::the_sql_and_api_arms_of_an_exact_vector_order_do_the_same_work`, `::an_approximate_order_reports_its_shortlist` | exact (page-order scan) or quantized (ef) by index choice |
| `SET LOCAL ef_search = n` (pgvector) / `diskann.query_search_list_size` (pgvectorscale) | T1 | `sql_tier1.rs::set_local_ef_search_turns_the_vector_order_approximate` | maps to `ef`: it is one of the two knobs §2's `SET` row keeps as `SET LOCAL`, because it is one of the two that change something |
| `USING exact (emb)`, `USING quantized (emb vector_cosine_ops)` | T1 | `sql_tier1.rs::create_table_and_create_index_build_a_queryable_collection` | index families |
| `USING vamana (emb vector_cosine_ops)`, `USING diskann (...)` | T1 — a FAMILY of its own, the Vamana/DiskANN graph | `index_vector_vamana.rs::the_graph_finds_most_of_the_true_nearest_neighbours_and_finds_more_of_them_with_a_longer_search_list`, `::the_graph_survives_a_reopen_and_answers_identically`, `::a_build_that_stops_part_way_leaves_the_index_not_ready_and_refuses_to_answer`, `::deletes_and_updates_orphan_no_node_and_strand_no_neighbour_list`, `::an_older_build_refuses_a_database_that_declares_the_vamana_bit_by_name`, `::verify_indexed_source_is_clean_after_a_build_and_after_writes`; `format_vamana_compat.rs::the_preserved_vamana_corpus_opens_and_answers_its_own_brute_force_oracle`; `sql_tier1.rs::create_table_and_create_index_build_a_queryable_collection` (`CREATE INDEX town_emb_ann ON town USING diskann (emb vector_cosine_ops)`) | `IndexFamily::VamanaGraph`: a single-layer graph over the same int8 codes, in keyspaces `0x7D` (node heads) and `0x7F` (adjacency) behind feature bit `0x8000`, searched greedily from a medoid entry point and reranked against the f32 sidecars. `ORDER BY emb <=> $1` is ANSWERED by it (`sql_vamana_order.rs::a_vector_order_is_answered_by_a_vamana_index_built_through_sql`), and when a column carries both approximate families the graph is the one that answers (`::a_column_with_both_approximate_families_is_answered_by_the_graph`). A vector order over a `BUILDING` vamana index is REFUSED (`index is not ready`), never answered from a partial graph |
| `USING hnsw`, `USING ivfflat` | T1 as ALIASES of `quantized`, with a notice | `sql_tier1.rs::create_table_and_create_index_build_a_queryable_collection` | no new family: this engine has neither a layered graph nor an inverted list, and a spelling that silently built something else would be a lie about what was created |
| distance as a filter `emb <=> $v < 0.3` | T2 | `sql_refusals.rs::a_distance_as_a_filter_is_tier_two_while_the_same_operator_orders` | approximate membership set (ef-bounded) |
| `vector_dims`, `vector_norm`, `l2_normalize` | T2 (refused by name) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | row functions |
| `<+>` L1, halfvec, sparsevec, binary quantization ops | T3 (refused by name) | `sql_refusals.rs::halfvec_and_sparsevec_are_tier_three` | no atomic |

### 4.6 Text search

| construct | tier | test | atomic |
|---|---|---|---|
| `to_tsvector('simple', col) @@ to_tsquery('simple', 'a \| b')` / `'a & b'` / `'"a b"'` | T1 | `sql_tier1.rs::a_tsquery_compiles_to_any_all_and_phrase`, `::a_one_term_tsquery_is_any_of_one_term`; `sql_prepared.rs::a_tsquery_rebinds_its_terms_and_its_match_kind` | Text filter Any / All / Phrase (analyzer v1) |
| `NOT (col @@ to_tsquery(...))`, a `!term` tsquery | T1 | `sql_tier1.rs::a_negated_tsquery_is_the_complement_of_the_text_set` | the complement of the text index's own document universe (§3's boolean rule) |
| a tsquery that MIXES `&` with `\|` | T3 for that statement | `sql_refusals.rs::a_tsquery_that_mixes_and_with_or_is_a_boolean_tree` | one tsquery is one `TextMatch`; a boolean tree of them is the §3 membership algebra over text leaves, which is not built |
| `ORDER BY ts_rank_cd(...)` | T1 | `sql_tier1.rs::ts_rank_cd_and_bm25_are_the_same_order`, `::the_ranking_value_can_be_projected_under_an_alias`; `sql_prepared.rs::a_text_rank_rebinds_the_query_it_ranks_by` | Bm25 order (formula differs from Postgres; documented, §5 deviation 5). `ts_rank_cd`'s normalisation argument is refused: the ranking here is BM25 and has no `ts_rank` knob |
| `bm25(col, 'query')` as an expression | T1 | `sql_tier1.rs::ts_rank_cd_and_bm25_are_the_same_order`, `::an_arithmetic_order_is_one_score_expression`; `sql_explain.rs::a_score_order_names_every_leaf`; `sql_prepared.rs::a_blended_score_rebinds_every_leaf_of_its_tree` | Score leaf |
| a text configuration other than `'simple'` | T3 for that statement | `sql_refusals.rs::a_text_configuration_beyond_simple_is_tier_three` | analyzer v1 is language-neutral |
| `websearch_to_tsquery`, `plainto_tsquery` | T2 (refused by name) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | parsers onto the same filter |
| `search(col, 'query')` typo-tolerant, prefix on the last token (the prior engine's family; Meilisearch-class) | T1 | `sql_search.rs::a_typo_tolerant_search_names_the_rows_a_brute_force_walk_of_the_vocabulary_names`, `::the_last_token_of_a_search_completes_as_a_prefix_and_an_earlier_token_does_not`, `::a_search_composes_with_a_scalar_filter_beside_it_and_inside_an_or`, `::explain_names_the_search_driver_its_expanded_terms_and_the_score_leaf`, `::a_truncated_dictionary_walk_reaches_the_client_as_a_notice_that_says_so`; `text_search_typo_tolerance.rs::a_brute_force_levenshtein_over_the_same_corpus_names_the_rows_the_index_walk_names`, `::the_last_token_completes_as_a_prefix_and_the_earlier_tokens_do_not`, `::the_edit_bound_is_none_up_to_four_characters_one_up_to_eight_and_two_beyond` | `TextMatch::Search`: a term-dictionary prefix range plus a BOUNDED Levenshtein walk over the dictionary (`core/engine/src/index/text/fuzzy.rs::expand`), then the posting lists the accepted terms already have. NO format change -- the dictionary is the `TERM_STATS = 0x77` run that a text index has always written, one contiguous sorted key per term, and the postings are `POSTING = 0x75`; no keyspace tag and no feature bit were added. The query's tokens are expanded one at a time and a document matches when EVERY token has one of its accepted terms, which makes this a generalisation of `All` rather than a second filter shape -- so a `search()` leaf HAS a membership set and composes inside `OR`. Two bounds: `edit_bound` spends no edit up to 4 characters, one up to 8 and two beyond (Meilisearch's rule), and the walk stops at 4,096 dictionary entries visited or 64 terms accepted. Hitting either TRUNCATES, and a truncated walk carries a NOTICE that names the cap, the way `SHOW EDGES` does |
| `search_score()` | T1 | `sql_search.rs::search_score_is_one_for_an_exact_match_and_falls_with_the_edits_and_the_completion`, `::a_search_score_order_descends_and_reaches_every_row_the_predicate_admits`, `::a_search_score_blends_with_bm25_and_a_vector_distance_in_one_order_expression`, `::search_score_without_a_search_in_the_statement_is_refused_by_name`; `text_search_typo_tolerance.rs::an_exact_term_scores_one_an_edit_scores_less_and_a_longer_prefix_scores_more` | `ScoreExpr::SearchScore`, the Score leaf of the `search()` predicate above, normalised to [0,1]: 1 for an exact term match, decreasing with the edit distance actually spent and with how much of the final token the prefix had to complete. It costs nothing extra -- the automaton settled both numbers at prepare, so per document this is one posting presence test per accepted term and one mean, and no row is read. The formula is written down the way BM25's is (§5 deviation 8). `search_score()` in a statement with no `search()`, or with more than one, is REFUSED by name rather than returning a number that means nothing |
| `bm25_norm(col, 'query', k)` | T2 | — | `bm25(col, q) / (bm25(col, q) + k)` over the existing Score leaf: one arithmetic operation, no extra pass, and strictly monotone in `bm25`, so the order it produces is the order `bm25` produces. It earns a row because a hybrid `ORDER BY` has to weigh a text term against a vector similarity, and a weight over an unbounded BM25 is not a weight; with both terms in [0,1) it is. |
| `highlight`, `ts_headline` | T2 (refused by name) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | row function |
| multi-field text index | T2 | — | index over a concatenated stored field today; declared multi-field later |
| language stemming beyond 'simple' | T3 | `sql_refusals.rs::a_text_configuration_beyond_simple_is_tier_three` | analyzer v1 is language-neutral |

### 4.7 Aggregates

| construct | tier | test | atomic |
|---|---|---|---|
| `count(*)`, `count(col)`, `sum`, `min`, `max`, `avg` | T1 | `sql_tier1.rs::count_star_with_no_filter_is_one_row_over_the_key_order_driver`, `::count_star_with_a_filter_counts_what_the_filter_admits`, `::sum_min_max_avg_by_group_with_having`, `::the_sql_aggregate_charges_exactly_what_the_api_aggregate_charges`; `aggregate_graph_adversarial.rs::a_every_accumulator_over_every_filter_shape_equals_the_fold`, `::i_count_star_no_filter_uses_keys_and_reads_no_row`; `sql_explain.rs::agg_count_all_reads_the_live_record_and_says_so` | `Database::prepare_aggregate` (`core/engine/src/query/aggregate.rs`): STREAMING when the group key is the driving scalar index's own value (groups contiguous, one accumulator set alive, the key off the posting with no row read, a page stops and resumes at a group boundary); HASHED otherwise, bounded by the `groups` QueryBudget resource. No spill: past the cap the page is `BudgetExceeded { groups }`. An accumulator's input is the driving posting's value where the field IS that index, and the primary row otherwise — charged as `primary_reads` and printed by EXPLAIN. An accumulator that needs NO row is answered from the postings alone and charges no `primary_reads` at all: `count(*)`, and `count`/`min`/`max` over the DRIVING index's own value. Where a row IS needed, a page GATHERS its candidates, sorts them by entity id and reads them through one forward pass of the primary tree instead of a point-get per candidate in posting order — the same set of rows, in the primary tree's order rather than the driver's, and bounded so that no row the row-by-row walk would not have read is read |
| `GROUP BY`, `HAVING`, `DISTINCT` | T1 | `sql_tier1.rs::group_by_an_indexed_column_streams_and_matches_the_api`, `::select_distinct_is_a_group_with_no_accumulators`, `::group_by_with_a_radius_filter_hashes_and_agrees_with_the_filter_itself`, `::the_divided_group_key_is_accepted_index_side_and_refused_otherwise`; `sql_explain.rs::agg_distinct_kind_is_a_group_with_no_accumulators`, `::agg_born_decade_computes_its_expression_key_index_side`; `aggregate_graph_adversarial.rs::d_pages_concatenate_streaming_and_hashed_having_drops_boundary_groups` | same atomic. `DISTINCT` is a group with no accumulators, and over a scalar index with no filter beside it that is a third shape, SKIP-SCAN: nothing is folded, so the walk reads the FIRST posting of a value, emits it, and seeks to the successor of that value's key prefix — ONE descent per DISTINCT VALUE, never a step over the postings in between, so `scalar_postings` counts values and not rows and `primary_reads` is zero. It needs all four of: no accumulators, no filters (a filter can reject every row of a value, and then the value is not a group), the group key the driving index's own undivided value, and that index's full forward range as the driver; failing any one of them is the STREAMING walk with the same answer. `HAVING` is a predicate on a FINISHED group's accumulator values, applied before paging. `GROUP BY col / n` is accepted only where it is computable index-side from an Int posting (truncating division by a positive divisor is monotone in that index's own order, so the groups stay contiguous); without such an index the expression form is refused and `GROUP BY col` with a range filter is the spelling. One key only: a composite key has no atomic |
| `ORDER BY <aggregate alias>` (+ `LIMIT`) | T1 | `sql_tier1.rs::order_by_an_aggregate_alias_sorts_the_finished_groups`; `aggregate_graph_adversarial.rs::f_order_by_accumulator_limit_having` | the finished groups are sorted before they are paged. THE ONE PLACE A SORT OVER MEMORY HAPPENS in this engine, and it is bounded because what it sorts is the group table, which the `groups` budget has already bounded. It forces the hashed shape: no group's value is final before the walk ends |
| `array_agg`, `string_agg`, `json_agg` | T2 (refused by name) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | bounded by row budget |
| `count(DISTINCT col)` | T2 | `sql_tier1.rs::a_folded_answer_refuses_what_it_cannot_report` | a per-group distinct set is a second unbounded structure inside each group; the bounded atomic here is one accumulator per group |
| `percentile_cont`, window functions, `GROUPING SETS`, `CUBE` | T3 (refused by name) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | no atomic |

### 4.8 Joins (essentials only)

| join | tier | test | atomic / cost |
|---|---|---|---|
| `INNER JOIN b ON a.key = b.key` (key equality) | T2 (refused by name), after GROUP BY | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | key lookup per driving row (key-order driver); cost ∝ driving rows |
| `LEFT JOIN` on key equality | T2 (refused by name) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | same, NULLs on miss |
| `USING (key)`, `NATURAL JOIN`, `RIGHT JOIN` | T2 (refused by name) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | rewrites |
| `CROSS JOIN LATERAL (SELECT ... FROM GRAPH_TABLE ...)` | T2 (refused by name) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | one bounded traversal per driving row |
| join on a non-key column, `FULL OUTER JOIN` | T3 (refused by name) | `sql_refusals.rs::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` | needs a hash join with spill |
| a pattern compiled to a join | never | — | graph contract: a hop is a posting range |

## 5. Dialect deviations (stated once, each with the reason)

1. The graph is native: `CREATE PROPERTY GRAPH` is optional and uses `EDGE TYPES`, not `EDGE TABLES`; no `SOURCE KEY ... REFERENCES` because endpoints are known from written edges.
2. Inline element `WHERE` prunes per hop by contract; the post-pattern `WHERE` filters completed matches. Both are standard syntax; the guarantee is ours. A node predicate an index cannot answer without the row (`IS NULL`, `IS MISSING`, a text search, a geometry predicate, a JSON equality) is REFUSED inline rather than demoted to a post-filter: a post-filter keeps a node in the frontier that graph contract 4.3 says must never be expanded, so it answers a different question at two hops.
3. `ORDER BY` takes one key; an expression is one key (the Score atomic). Two keys are a refusal (`sql_refusals.rs::two_order_by_keys_are_deviation_three`).
4. `OFFSET` is a keyset continuation, never a skip count; the word itself is refused as Tier 2 (`sql_refusals.rs::offset_is_deviation_four`).
5. BM25 stands behind `ts_rank_cd`; the number differs from Postgres and the docs say so.
6. `USING vamana` and `USING diskann` name the VAMANA GRAPH family, which is
   this engine's own (`core/engine/src/index/vector/graph.rs`), not pgvectorscale's
   implementation; `USING hnsw` and `USING ivfflat` remain aliases of the
   quantized family, with a notice, because this engine has neither a layered
   graph nor an inverted list.
7. A join never executes a pattern; a relation between rows is an edge.
8. `search_score()` is this engine's own number and has no PostgreSQL counterpart, so it is written down here the
   way BM25 is. Per query token `i`, the bounded automaton accepted a dictionary term having spent `edits_i` of the
   `bound_i` that token's length allowed, and the term is `found_i` characters long against the `typed_i` the token
   wrote. The token's quality is

   ```text
   quality_i = (1 - edits_i / (bound_i + 1)) * (typed_i / max(typed_i, found_i))
   ```

   and the document's `search_score()` is the MEAN over tokens of the best quality any accepted term of that token
   reached in it. Both factors lie in (0,1], so the score does: it is exactly 1 when every token matched a term
   exactly, it falls strictly with each edit spent, and it falls strictly with each character the final token's
   prefix had to complete. The second factor is 1 for every token but the last, because only the last completes.
   `bound_i` is `edit_bound(typed_i)`: 0 up to four characters, 1 up to eight, 2 beyond
   (`core/engine/src/index/text/fuzzy.rs::edit_bound`, `::quality`). A candidate the search does not admit scores
   `0.0`, which is where a `Bm25` leaf puts a non-matching candidate too.
8. Declared TIMESTAMPTZ is stored as UTC microseconds in an Int; no time-zone storage.
9. `<>` on an edge property is accepted, and so is `<>` on an indexed scalar column. The two get there by different roads, and the difference is worth stating: an edge predicate reads the property out of the posting the hop is standing on, so the complement costs exactly what the predicate costs and no set is involved; a scalar `<>` is the COMPLEMENT of an equality over the membership algebra (`sql_refusals.rs::an_inequality_is_the_complement_of_an_equality`), which is a set, bounded by `WorkResource::MembershipBytes`.
10. The reaching edge is projected under a reserved `@edge.` prefix (`@edge.weight`), which no unquoted SQL identifier can spell, so it never collides with a declared field. A `COLUMNS (r.weight AS w)` entry compiles to it, and `ORDER BY w` resolves against the pattern's edge aliases before it looks for a column of the far node. An edge column entry and an `ORDER BY` over one are matched to the pattern's edge variable WITHOUT case, as every other name in this dialect is.
11. A nullish group key (NULL or missing) sorts FIRST under `GROUP BY`, the scalar keyspace's own order; Postgres sorts NULL last and `NULLS FIRST|LAST` is refused. `HAVING` over an all-null accumulator drops the group (SQL three-valued logic); a `HAVING` over `min`/`max` of a non-numeric column is refused at prepare.
12. `DROP TABLE` is RESTRICT by default, and what restricts it is GRAPH EDGES, not foreign keys: Postgres refuses on a dependent constraint, this refuses while any edge in any context references a row of the table and names those contexts (graph contract 6.1). `CASCADE` removes those edges and nothing else -- it never reaches a second table's rows. A table with no edges on it drops under the default.
13. `DROP TABLE` is bounded and resumable, so it is not one transaction: the DROPPING mark is committed first and each bounded step after it is committed as it goes. An interrupted `DROP TABLE` leaves a collection that answers nothing and resumes from its committed cursor; it never leaves a half-emptied readable table. `ROLLBACK` does not undo a drop that has begun.
14. A statement that reads the reaching edge runs on the traversal that bound it, because no other candidate stream carries the edge (graph contract 4.2); any other driver is refused when the query is prepared, with the driver named. So a post-pattern `_key` predicate beside such a statement keeps the traversal driving and is answered from the external key the row carries, at one primary read per candidate, instead of taking the driver for the mapping walk. A `_key` predicate with no such pattern still drives the mapping walk, which is the cheaper plan and remains the default.
15. The prior engine's `FROM MATCH (a)-[r]->(b)` is not adopted (§1) and never will be. The capability is not lost and is not Tier 3: it is T1 under the standard spelling, `FROM GRAPH_TABLE (g MATCH (a)-[r]->(b) COLUMNS (...))`. Migration is mechanical -- wrap the pattern in `GRAPH_TABLE (...)`, name the graph, and move the SELECT list into `COLUMNS`. `MATCH SHORTEST` is `ANY SHORTEST` (§4.3), and a multi-FROM `..., collection AS alias` is a `CROSS JOIN LATERAL` (§4.8). This is a refusal with a named reason: the atomic exists, the spelling does not.
16. `NOT NULL` is enforced here. The prior engine parses it and does not check it, so a corpus it accepted can be refused by sekejap on the row that was always in violation. The check is a descriptor flag tested when the row is assembled; the error names the column.
17. `FROM ALL` is unordered by contract: it would concatenate the collections in catalog id order and page within each. It is not a UNION (which stays T3), it does not deduplicate, and a ranked `ORDER BY` over it is refused for the reason in its §2 row. It is not built, and is refused by name today.
18. `RETURNING` is refused by name in `UPDATE` and `DELETE`, and so is a `SET _key = ...` that would patch the external key of a row a driver is standing on (`sql_dml.rs::a_predicated_write_refuses_returning_and_a_patched_key_by_name`).

## 6. Execution guarantees the contract makes

- Every T1/T2 predicate on an indexed field is answered index-side (posting, membership set, or inline edge property); a row is read only for projection or for a predicate the plan names as row-bound. `EXPLAIN` prints which (`sql_explain.rs::a_filter_says_how_it_is_answered`).
- Work is proportional to candidates walked or rows returned, never to the collection, except for constructs whose definition is a scan (exact vector order without a filter, `count(*)` without a filter), which `EXPLAIN` labels as scans (`sql_explain.rs::a_scan_by_definition_is_labelled_as_one`).
- A boolean filter's memory is the same membership budget every other set walk is bounded by: a plain Vec while it stays smaller than a bitmap of the collection's span, then that bitmap, then a refusal. A complement is always a bitmap, so a span whose bitmap does not fit `RUN_BYTES` has no complement and the filter is refused rather than degraded. What `RUN_BYTES` bounds is what is held AT ONCE, over every boolean filter of the query together: each union and intersection folds in place into its accumulator rather than copying both sides, every live intermediate is counted against the one budget, and a tree that would hold more is refused with `WorkResource::MembershipBytes` — a resource with no `QueryBudget` field, because the ceiling is the memory promise and not a caller allowance.
- A semi-join's set is built while the statement is COMPILED, and it runs under the caller's `QueryBudget` and cancellation like any other walk: the edge-keyspace walk is charged `GraphEdges` per edge and `GraphVisited` per entity kept, and the inner collection query is an ordinary prepared query under the same budget.
- Memory per query is bounded by QueryBudget: pages, membership sets, groups (`WorkResource::Groups` — the accumulator sets an aggregate holds AT ONCE, one under the streaming shape and one per distinct group under the hashed one; its default ceiling is `RUN_BYTES` divided by what one group costs, applied even under `QueryBudget::unlimited`), frontier.
- A §4.1 / §4.2 function is in exactly one of two places and `EXPLAIN` says which: a RANGE REWRITE, folded into index bounds at prepare and never evaluated per candidate, whose cost is the candidates the range admits; or a ROW FUNCTION over the values a returned row already projected, whose cost is one evaluation per row RETURNED. A function that is neither -- because its pre-image is a set of ranges, or because the expression index it would ride does not exist -- is refused, not quietly moved into the row path (`sql_explain.rs::a_date_rewrite_is_printed_as_a_range_and_not_as_a_row_function`, `::a_statement_with_no_function_says_none_in_both_sections`).
- Every T3 refusal names the missing atomic in its error text (`sql_refusals.rs::the_table_itself_is_well_formed`, `::every_listed_keyword_is_refused_by_name_with_its_tier_and_reason`).
- A predicate on a column with NO index is refused and never demoted to a scan. That rule is unchanged by `docs/lang/INDEX_CONTRACT.md`; what changed is which columns have one without being asked, so the refusal is now what a caller meets after `WITH (index: none)` or `WITH (index: [...])` rather than what they meet on their first statement (`sql_automatic_index.rs::with_index_none_creates_none_and_the_predicate_is_then_refused_exactly_as_before`).

## 7. What is built, and what is left

Built, with the test file that pins each:

1. The parser and the compiler for §2 T1 + §3 T1 + §6's guarantees, with `EXPLAIN` — `lang/src/parser/`, `lang/src/compile/`, `lang/src/explain.rs`; `sql_tier1.rs`, `sql_explain.rs`, `sql_refusals.rs`.
2. Aggregates (§4.7) — `core/engine/src/query/aggregate.rs`; `sql_tier1.rs`, `aggregate_graph_adversarial.rs`. Date/time and string functions (§4.1, §4.2) — `lang/src/functions.rs` plus the expression scalar index (`IndexExpr::Lower`, `IndexExpr::JsonText`) and the declared-type descriptor field (`CollectionInfo::declared`); `sql_functions.rs`, `sql_json_path.rs`.
3. `OR` / `IN` / `NOT` / `EXISTS` (§3) — `core/engine/src/query/membership.rs` (`SetExpr` and the set algebra) with `QueryFilter::Any`/`All`/`Not`/`Ids` and `QueryDriver::Membership`; `sql_tier1.rs`.
4. The catalog views and the `SHOW` family (§2) — `lang/src/catalog.rs`, `lang/src/compile/rows.rs`, listed in `docs/dist/PG_SURFACE.md`; `sql_catalog.rs`.
5. The write-path schema behaviour (`ALTER TABLE`, defaults, the generated-column refusal, `NOT NULL`) and the predicate-driven `UPDATE` / `DELETE` walk — `core/engine/src/collections/column_rules.rs`, `core/engine/src/collections/write_set.rs`, `lang/src/compile/ddl.rs`, `lang/src/compile/dml.rs`; `sql_schema.rs`, `sql_dml.rs`, `sql_dml_adversarial.rs`, `core/engine/tests/column_rules.rs`.
6. `DROP TABLE` / `DROP INDEX` as bounded resumable phase machines — `core/engine/src/collections/drop_collection.rs`; `drop_collection.rs`.
7. The reusable `PreparedSql` and the bounded plan cache — `lang/src/compile/bind.rs`, `dist/rust/src/plans.rs`; `sql_prepared.rs`, `dist/rust/tests/api.rs`.
8. The PostgreSQL wire over all of it, cursors included as session state — `dist/src/pg/`; `dist/tests/pg_wire.rs`, `dist/tests/pg_server.rs`.
9. The `CREATE TABLE ... WITH (...)` INDEX SUGAR (§2) — `lang/src/parser/ddl.rs::with_clause`, `lang/src/compile/ddl.rs::with_indexes` and `::with_family`, `lang/src/compile/plan.rs::build_index` and `::unwind_create_table`; `sql_index_sugar.rs`. It adds no family, no keyspace tag and no feature bit: the seven family keys map onto the five families `CREATE INDEX` already builds, and the removal a mid-clause refusal runs is `DROP TABLE`'s own phase machine.
10. Typo-tolerant `search(col, 'query')` and `search_score()` (§4.6) — the atomic is `core/engine/src/index/text/fuzzy.rs` (a bounded Levenshtein walk of the `0x77` term dictionary that is already on disk, no new keyspace tag and no new feature bit), reached through `TextMatch::Search` and `ScoreExpr::SearchScore`; `core/engine/tests/text_search_typo_tolerance.rs`, `lang/tests/sql_search.rs`.
11. The AUTOMATIC indexes of `docs/lang/INDEX_CONTRACT.md` and the `index:` key that refuses them (§2) — `lang/src/compile/ddl.rs::automatic_index`, `::automatic_indexes` and `::same_index`, `lang/src/parser/ddl.rs::automatic_value`; `sql_automatic_index.rs`. It adds no family, no keyspace tag and no feature bit either: the scalar family already accepted exactly bool/int/real/text and the two spatial families already followed the declared shape. Measured write cost: about 1.4 µs per index per inserted row, flat in the number of columns.

Not built, in the order the atomics make sense:

1. Graph T2 (§4.3) in the graph-contract order: label alternation, the path accumulators, `ANY SHORTEST`, element identity.
2. Geometry I/O and `&&` (§4.4): the WKT/WKB readers and writers, `ST_X`/`ST_Y`, the bbox operator, `postgis_version()`.
3. The MULTI-range date rewrites (`EXTRACT(MONTH ...)`, `EXTRACT(DOW ...)`), which item 3 of the built list now makes possible: the pre-image is a set of ranges and the membership union exists.
4. Trigram index for `ILIKE` / infix `LIKE` (§3). Typo-tolerant `search()` and `search_score()` (§4.6) are BUILT and moved to the list above as item 11.
5. A PROJECTION-EXPRESSION surface: `CASE WHEN`, the JSON path operators, `json_array_length`, and `ST_Area` / `ST_Length` / `ST_Perimeter` / `ST_Centroid`. All of them are the same missing piece — a row expression in a select list that is not one of the closed `ROW_FUNCTIONS` names. The SURFACE is still unbuilt. What is no longer missing is the refusal: each spelling now has a `refuse::TABLE` row naming this item, and the expression parser consults the table when it meets a function name or a keyword it does not compile, so every one of them is a Tier-2 refusal by name in a select list and in a `WHERE` (`lang/src/refuse.rs`, `lang/src/parser/expr.rs::primary` and `::row_atom`; `lang/tests/refusal_by_name.rs::the_projection_expression_constructs_are_refused_by_name_in_both_positions`). `#>` and `#>>` needed a lexer token before they could be named at all, and have one.
6. Key-equality joins (§4.8).
7. `SHOW STATUS` / `SHOW STORAGE` (`docs/dist/OPS_CONTRACT.md` §6), `EXPLAIN ANALYZE`, `REINDEX` and `COMPACT` as
   statements; and the `EXPLAIN` families that must not be RUN to be explained -- `DROP TABLE`, `ALTER TABLE`, a
   predicated `UPDATE`/`DELETE` -- carried out of `SqlDatabase::sql` to the published crate's `Db`, which has no
   door for them today (§2, the `EXPLAIN <statement>` row).
8. `FROM ALL` and `DELETE FROM ALL` (§2), materialized and search views, non-recursive `WITH`.
9. *(closed)* The four named refusals that reached the wire as `XX000 internal_error` — the one code a client
   RETRIES — now carry `0A000 feature_not_supported`, the code their own §8 blocks claim. Three were fixed at the
   CAUSE, in `lang`: the missing scalar index and the missing expression index a predicate names (§3, §4.1) are one
   site, `lang/src/compile/mod.rs::index_for_expression`, which every index family passes through, and the
   `SHOW <word>` that is neither a collection nor a client setting (§2) is `lang/src/compile/rows.rs`. Both raise
   `SqlError::Unsupported` rather than flattening a named refusal into `SqlError::Engine` prose. The fourth, a
   boolean leaf with no membership set (§3), is raised inside the ENGINE's own error type
   (`core/engine/src/query/plan.rs`, a `QueryError::Database`), and undoing that flattening would mean a new variant
   on `sekejap_core::collections::Error`; it is recognised in `dist/src/pg/types.rs::sql_error` instead, by the
   sentence all five leaves share. Asserted over the wire, through the same `Connection::feed` path the server uses,
   by `dist/tests/pg_wire_refusals.rs`.
10. *(closed)* The `refuse::TABLE` lookups the parser never reached — `INTERSECT` and `EXCEPT` after a SELECT's
    `LIMIT`, and `TRAIL` / `WALK` / `SIMPLE` inside a `GRAPH_TABLE` pattern — are reached. The fix is one sweep
    rather than five tests: `lang/src/parser/mod.rs::guard_here` asks the table for the word OR the operator at the
    cursor, and every place that would otherwise name a position calls it first — `Parser::expect`,
    `Parser::expect_word`, the end of a statement, a SELECT's tail and a `GRAPH_TABLE` pattern's head. A row added
    to the table later is therefore refused wherever it can be written, without a new `if`; `lang/tests/
    refusal_by_name.rs::every_row_of_the_refusal_table_is_refused_where_a_statement_writes_it` drives the check
    FROM the table so a row with no parser path fails. The same sweep found `ALL SHORTEST` being refused under the
    name `ANY SHORTEST`. What stays `42601` is text that is genuinely malformed, which
    `::a_genuinely_malformed_statement_is_still_a_syntax_error` pins.

## 8. Examples

Every block below runs on §0's fixture. A block tagged `sql` answers; a block
tagged `sql refused` is refused, and its first line names the SQLSTATE and the
construct the refusal carries.

### 8.1 Statements (§2)

```sql
-- SELECT ... FROM <collection> [WHERE] [ORDER BY one expr] [LIMIT]
SELECT _key, name, born FROM place WHERE kind = 'shop' ORDER BY born DESC LIMIT 5
```

```sql
-- SELECT ... FROM GRAPH_TABLE (...)
SELECT k FROM GRAPH_TABLE (base MATCH (a:place WHERE a._key = 'p000')-[:near]->{1,3}(b:place) COLUMNS (b._key AS k))
```

```sql
-- INSERT INTO t (...) VALUES (...)
INSERT INTO place (_key, name, kind, born) VALUES ('p900', 'example one', 'depot', 1951)
```

```sql
-- UPDATE t SET ... WHERE _key = ...
UPDATE place SET name = 'example one, renamed' WHERE _key = 'p900'
```

```sql
-- DELETE FROM t WHERE _key = ...
DELETE FROM place WHERE _key = 'p900'
```

```sql
-- UPDATE t SET ... WHERE <any predicate>
UPDATE place SET note = 'seen' WHERE born BETWEEN 1990 AND 1991
```

```sql
-- DELETE FROM t WHERE <any predicate> CASCADE
DELETE FROM place WHERE born = 1993 CASCADE
```

```sql
-- CREATE TABLE, one column of every Kind. `index: none` so the statements
-- below are real creates rather than notices naming an index already there.
CREATE TABLE ex_town (name TEXT PRIMARY KEY, label TEXT, founded INT, rating DOUBLE PRECISION, active BOOLEAN, props JSONB, at TIMESTAMPTZ, day DATE, loc GEOMETRY(Point,4326), area GEOMETRY(Polygon,4326), emb VECTOR(4)) WITH (index: none)
```

```sql
-- the same table WITHOUT the override: every eligible column is indexed with
-- the collection, and the predicate answers with no CREATE INDEX anywhere
CREATE TABLE ex_auto (name TEXT, founded INT, active BOOLEAN, day DATE, loc GEOMETRY(Point,4326), props JSONB, emb VECTOR(4));
INSERT INTO ex_auto (_key, name, founded, active, day) VALUES ('a1', 'kebun', 1901, true, '1901-06-15');
SELECT _key FROM ex_auto WHERE founded BETWEEN 1900 AND 1910;
SELECT _key FROM ex_auto ORDER BY name ASC LIMIT 1
```

```sql refused
-- refused 0A000: VECTOR is the one genuine trade, so it gets NOTHING automatically and the refusal names both families
SELECT _key FROM ex_auto ORDER BY emb <=> '[1,0,0,0]' LIMIT 1
```

```sql
-- WITH (index: [...]): only the columns named, and the name is the sugar's own
CREATE TABLE ex_only (a TEXT, b TEXT) WITH (index: [a]);
SELECT _key FROM ex_only WHERE a = 'x'
```

```sql refused
-- refused 0A000: `b` was not named, so §6's refusal is what a predicate over it meets
SELECT _key FROM ex_only WHERE b = 'y'
```

```sql
-- CREATE INDEX ... USING btree
CREATE INDEX ex_town_founded ON ex_town USING btree (founded)
```

```sql
-- CREATE INDEX ... USING gin over to_tsvector
CREATE INDEX ex_town_label ON ex_town USING gin (to_tsvector('simple', label))
```

```sql
-- CREATE INDEX ... USING gist
CREATE INDEX ex_town_loc ON ex_town USING gist (loc)
```

```sql
-- CREATE INDEX ... USING exact, on a VECTOR column
CREATE INDEX ex_town_emb ON ex_town USING exact (emb)
```

```sql
-- USING diskann builds the VAMANA GRAPH family (USING vamana is the same
-- statement); it is NOT READY until its build finishes, and refuses a vector
-- order until then rather than answering from a partial graph
CREATE INDEX ex_town_emb_ann ON ex_town USING diskann (emb vector_cosine_ops)
```

```sql
-- DROP INDEX [IF EXISTS]
DROP INDEX IF EXISTS ex_town_founded
```

The four blocks above declare a table and then index it one statement at a
time. The `WITH (...)` sugar writes the same thing once. It builds the same
descriptors -- `sql_index_sugar.rs::the_sugar_builds_exactly_the_catalog_the_long_hand_statements_build`
compares them field by field against the long hand -- under the generated
names `<table>_<column>_<family>`, and it raises one NOTICE per mapping
saying which family the key became and why.

```sql
-- CREATE TABLE ... WITH (...): the table and its indexes in one statement
CREATE TABLE ex_depot (name TEXT, label TEXT, founded INT, loc GEOMETRY(Point,4326), emb VECTOR(4)) WITH (hash: [name], range: [founded], fulltext: [label], spatial: [loc], vector: [emb])
```

```sql
-- and the predicate answers immediately: `ex_depot_founded_btree` is the index it names
SELECT name FROM ex_depot WHERE founded >= 1900
```

```sql
-- what the sugar does NOT remove: `_key` was not named, so the generated names are the only ones
DROP INDEX ex_depot_emb_exact
```

```sql refused
-- refused 0A000: a WITH key outside the seven, refused BY NAME with the seven written out
CREATE TABLE ex_bad (c TEXT) WITH (gin: [c])
```

```sql refused
-- refused 0A000: a family the column's Kind cannot carry -- a gist indexes a Point or a Geo column
CREATE TABLE ex_bad (c TEXT) WITH (spatial: [c])
```

```sql
-- column DEFAULT now() / uuid4(): the closed generator set
CREATE TABLE ex_person (id TEXT PRIMARY KEY, born TIMESTAMPTZ DEFAULT now(), token TEXT DEFAULT uuid4())
```

```sql
-- NOT NULL on a column, enforced when the row is assembled
CREATE TABLE ex_account (id TEXT PRIMARY KEY, email TEXT NOT NULL)
```

```sql
-- ALTER TABLE t ADD COLUMN
ALTER TABLE ex_person ADD COLUMN nickname TEXT
```

```sql
-- ALTER TABLE t RENAME COLUMN old TO new (T1 while the collection is empty)
ALTER TABLE ex_person RENAME COLUMN nickname TO handle
```

```sql
-- ALTER TABLE t DROP COLUMN
ALTER TABLE ex_person DROP COLUMN handle
```

```sql
-- ALTER TABLE t RENAME TO new_name: a name record and nothing else
ALTER TABLE ex_account RENAME TO ex_login
```

```sql
-- ALTER TABLE t ALTER COLUMN c TYPE new_type, within one Kind
ALTER TABLE ex_town ALTER COLUMN founded TYPE BIGINT
```

```sql
-- DROP TABLE [IF EXISTS] name [CASCADE|RESTRICT]
DROP TABLE IF EXISTS ex_login RESTRICT
```

```sql
-- schema.table: one user schema, so public.t IS t
SELECT _key FROM public.place LIMIT 1
```

```sql
-- SET <client setting>: accepted as a notice, stores nothing
SET application_name = 'doc examples'
```

```sql
-- SHOW <client setting>: answered from the constant this engine has
SHOW client_encoding
```

```sql
-- BEGIN opens the transaction the single writer is already inside
BEGIN
```

```sql
-- COMMIT is the durability barrier that closes it
COMMIT
```

```sql
-- BEGIN BULK defers the durability point to the matching close
BEGIN BULK
```

```sql
-- END BULK: the outermost close is the one that commits
END BULK
```

```sql
-- EXPLAIN <statement>
EXPLAIN SELECT _key FROM place WHERE kind = 'port' ORDER BY born ASC LIMIT 5
```

```sql refused
-- refused 0A000: EXPLAIN DROP TABLE through the published crate's three doors
EXPLAIN DROP TABLE ex_town
```

```sql refused
-- refused 0A000: EXPLAIN of a PREDICATED write, through the same three doors
EXPLAIN DELETE FROM place WHERE born >= 2100
```

The two blocks above are the one place where the ROUTE decides the answer, so
it is written out rather than implied. `DROP TABLE` and a predicated
`UPDATE`/`DELETE` are explained WITHOUT being run: `SqlDatabase::sql`
(`lang/src/lib.rs`) takes them with the `EXPLAIN` keyword, prepares them and
describes the phases, the driver, the bound and the mode. The published
crate's three doors (`docs/dist/RUST_API.md` §3) take none of them today:
`Db::explain` is `explain_sql`, which RUNS the statement to explain it and so
takes a SELECT or an aggregate and names this route in its refusal; `Db::query`
and `Db::execute` refuse an `EXPLAIN` because it answers with a plan and not
with rows or a count. All three refusals are `0A000`, which is what the blocks
assert. Carrying these two EXPLAIN families out to `Db` is listed as unbuilt
in §7.

```sql
-- SHOW TABLES
SHOW TABLES
```

```sql
-- SHOW <collection>
SHOW place
```

```sql
-- SHOW CREATE TABLE t, built from the descriptor
SHOW CREATE TABLE place
```

```sql
-- SHOW INDEXES [ON t]
SHOW INDEXES ON place
```

```sql
-- SHOW EDGES: the quadruples graph contract 2.5 derives from written edges
SHOW EDGES
```

```sql
-- version() is PostgreSQL-shaped because drivers parse it
SELECT version()
```

```sql
-- db_version() is the same fact with no costume
SELECT db_version()
```

```sql
-- the session facts a driver asks for on connect
SELECT current_schema(), current_database(), current_user, pg_backend_pid()
```

```sql
-- current_setting('x') over the closed client-setting list
SELECT current_setting('client_encoding')
```

```sql
-- a FROM-less literal
SELECT 1
```

```sql
-- the catalog views are ordinary relations: WHERE, ORDER BY, LIMIT compose
SELECT name, fields FROM db_tables ORDER BY name ASC LIMIT 5
```

```sql
-- pg_catalog under PostgreSQL's own names
SELECT tablename FROM pg_tables WHERE schemaname = 'public' ORDER BY tablename ASC
```

```sql
-- information_schema resolves ONLY qualified
SELECT column_name, data_type, is_nullable FROM information_schema.columns WHERE table_name = 'place'
```

```sql refused
-- refused 0A000: FROM ALL
SELECT name FROM ALL WHERE born > 1900
```

```sql refused
-- refused 0A000: FROM ALL
DELETE FROM ALL WHERE born > 1900
```

```sql refused
-- refused 0A000: GENERATED ALWAYS
CREATE TABLE ex_generated (id TEXT PRIMARY KEY, n INT, m INT GENERATED ALWAYS AS (n * 2) STORED)
```

```sql refused
-- refused 0A000: CREATE SCHEMA
CREATE SCHEMA app
```

```sql refused
-- refused 0A000: CREATE PROPERTY GRAPH
CREATE PROPERTY GRAPH roads NODE TABLES (place) EDGE TYPES (near)
```

```sql refused
-- refused 0A000: CREATE VIEW
CREATE VIEW ex_view AS SELECT _key FROM place
```

```sql refused
-- refused 0A000: CREATE VIEW (CREATE MATERIALIZED VIEW carries the same row)
CREATE MATERIALIZED VIEW ex_mview AS SELECT _key FROM place
```

```sql refused
-- refused 0A000: CREATE TRIGGER
CREATE TRIGGER t AFTER INSERT ON place
```

```sql refused
-- refused 0A000: DECLARE (a cursor is a WIRE SESSION construct, not a compiled statement)
DECLARE c BINARY CURSOR FOR SELECT _key FROM place
```

```sql refused
-- refused 0A000: EXPLAIN ANALYZE
EXPLAIN ANALYZE SELECT _key FROM place WHERE kind = 'port'
```

```sql refused
-- refused 0A000: WITH
WITH recent AS (SELECT _key FROM place WHERE born > 1990) SELECT _key FROM recent
```

```sql refused
-- refused 0A000: UNION
SELECT _key FROM place WHERE kind = 'port' UNION SELECT _key FROM place WHERE kind = 'farm'
```

```sql refused
-- refused 0A000: INTERSECT (the SELECT tail asks refuse::TABLE, so the word is refused by name wherever it stands)
SELECT _key FROM place WHERE kind = 'port' INTERSECT SELECT _key FROM place WHERE kind = 'farm'
```

```sql refused
-- refused 0A000: EXCEPT (the same one sweep that reaches INTERSECT reaches this)
SELECT _key FROM place WHERE kind = 'port' EXCEPT SELECT _key FROM place WHERE kind = 'farm'
```

```sql refused
-- refused 0A000: OFFSET (deviation 4: OFFSET is a keyset continuation, never a skip count)
SELECT _key FROM place WHERE kind = 'port' LIMIT 5 OFFSET 5
```

```sql refused
-- refused 0A000: SHOW STATUS is not in the SHOW word list; OPS_CONTRACT §6.1
SHOW STATUS
```

```sql refused
-- refused 0A000: PG_SETTINGS
SELECT name FROM pg_settings
```

### 8.2 Predicates (§3)

```sql
-- AND: a conjunction is a filter list
SELECT _key FROM place WHERE kind = 'park' AND active = TRUE
```

```sql
-- a CONSTANT predicate at the top of a WHERE, folded at prepare
SELECT _key, name FROM place WHERE 1 <> 1 LIMIT 1
```

```sql
-- =, <>, <, <=, >, >= on an indexed scalar
SELECT _key FROM place WHERE born > 1990
```

```sql
-- BETWEEN a AND b
SELECT _key FROM place WHERE born BETWEEN 1950 AND 1960
```

```sql
-- IS NULL: the field is written, and its value is null
SELECT _key FROM place WHERE rating IS NULL
```

```sql
-- IS MISSING is a different question: the field is absent from the document
SELECT _key FROM place WHERE tag IS MISSING
```

```sql
-- an external-key range walks the mapping keyspace
SELECT _key FROM place WHERE _key BETWEEN 'p010' AND 'p020'
```

```sql
-- OR on the same index is one membership set
SELECT _key FROM place WHERE kind = 'port' OR kind = 'farm'
```

```sql
-- IN (list) is the same union written shorter
SELECT _key FROM place WHERE kind IN ('port', 'farm', 'mill')
```

```sql
-- <> on an indexed scalar is the complement of an equality
SELECT _key FROM place WHERE kind <> 'port'
```

```sql
-- IS NOT NULL is the complement of the nullish key
SELECT _key FROM place WHERE rating IS NOT NULL
```

```sql
-- NOT before a group is De Morgan's law, applied while the filter compiles
SELECT _key FROM place WHERE NOT (kind = 'port' OR kind = 'farm')
```

```sql
-- NOT IN is the complement of the union
SELECT _key FROM place WHERE kind NOT IN ('port', 'farm')
```

```sql
-- OR across indexes unions two sets
SELECT _key FROM place WHERE kind = 'port' OR born > 1990
```

```sql
-- a parenthesised group binds the way SQL says
SELECT _key FROM place WHERE active = TRUE AND (kind = 'port' OR kind = 'farm')
```

```sql
-- EXISTS (subquery) over an edge type is a semi-join
SELECT _key FROM place WHERE EXISTS (SELECT 1 FROM near WHERE source = _key)
```

```sql
-- NOT EXISTS is its complement, over the collection's live rows
SELECT _key FROM place WHERE NOT EXISTS (SELECT 1 FROM near WHERE source = _key)
```

```sql
-- _key IN (subquery): the projected column must be TEXT
SELECT _key FROM place WHERE _key IN (SELECT source FROM near)
```

```sql
-- LIKE 'abc%' is a text-key prefix range on the column's own scalar index
SELECT _key FROM place WHERE kind LIKE 'po%'
```

```sql
-- starts_with(col, 'abc') is that same range
SELECT _key FROM place WHERE starts_with(kind, 'po')
```

```sql refused
-- refused 0A000: LIKE (an interior % needs the trigram family)
SELECT _key FROM place WHERE kind LIKE '%or%'
```

```sql refused
-- refused 0A000: ILIKE
SELECT _key FROM place WHERE kind ILIKE 'PO%'
```

```sql refused
-- refused 0A000: SIMILAR TO
SELECT _key FROM place WHERE kind SIMILAR TO 'po%'
```

```sql
-- a POINT leaf inside an OR HAS a set -- the point index's own postings
SELECT _key FROM place WHERE kind = 'port' OR ST_DWithin(loc, ST_SetSRID(ST_MakePoint(106.8, -6.2), 4326)::geography, 20000)
```

```sql refused
-- refused 0A000: a boolean leaf an index cannot answer (a GEOMETRY leaf; the engine names the leaf and sql_error carries the name to the wire)
SELECT _key FROM place WHERE kind = 'port' OR ST_Contains(area::geometry, ST_SetSRID(ST_MakePoint(106.82, -6.17), 4326))
```

```sql refused
-- refused 0A000: a predicate on a column with no index is not demoted to a scan
SELECT _key FROM place WHERE note = 'x'
```

### 8.3 String and date functions (§4.1, §4.2)

```sql
-- row functions on projected values, costed per row RETURNED
SELECT _key, lower(name) AS lowered, length(name) AS n, left(name, 3) AS head FROM place LIMIT 5
```

```sql
-- concat ignores NULL; || propagates it, as in Postgres
SELECT concat(kind, '-', name) AS joined, kind || '-' || name AS piped FROM place LIMIT 5
```

```sql
-- substring, split_part, replace, trim
SELECT substring(name, 1, 4) AS part, split_part(name, ' ', 1) AS first, replace(name, ' ', '_') AS under, trim(name) AS tidy FROM place LIMIT 5
```

```sql
-- lower(col) = x rides the EXPRESSION index over lower(col)
SELECT _key FROM place WHERE lower(kind) = 'port'
```

```sql
-- lower(col) LIKE 'x%' is the same index, as a prefix range
SELECT _key FROM place WHERE lower(kind) LIKE 'po%'
```

```sql
-- CREATE INDEX i ON t (lower(col)): a closed expression set, one member
CREATE INDEX ex_town_label_lower ON ex_town (lower(label))
```

```sql
-- EXTRACT(YEAR FROM t) folds to ONE scalar range at prepare
SELECT _key FROM place WHERE EXTRACT(YEAR FROM at) = 1990
```

```sql
-- EXTRACT(YEAR ...) BETWEEN is one range too
SELECT _key FROM place WHERE EXTRACT(YEAR FROM at) BETWEEN 1990 AND 1995
```

```sql
-- date_trunc('unit', t) compared against a literal: one range
SELECT _key FROM place WHERE date_trunc('year', at) = '1990-01-01'
```

```sql
-- both inequalities cut at the boundary ABOVE an off-boundary literal
SELECT _key FROM place WHERE date_trunc('month', at) >= '1990-01-15'
```

```sql
-- a cast to date is one day's range
SELECT _key FROM place WHERE at::date = '1990-06-15'
```

```sql
-- a bare comparison against a date literal is a range
SELECT _key FROM place WHERE at >= '1990-01-01'
```

```sql
-- a declared DATE column takes the same literals
SELECT _key FROM place WHERE day = '1990-06-15'
```

```sql
-- a clock-relative predicate is folded ONCE at prepare
SELECT _key FROM place WHERE at > now() - interval '7 days'
```

```sql
-- age() and every interval are microseconds over one folded clock
SELECT _key, age(at) AS since FROM place LIMIT 5
```

```sql
-- to_char with one of the named templates, checked at PREPARE
SELECT to_char(at, 'YYYY-MM-DD') AS day_text FROM place LIMIT 5
```

```sql
-- a declared TIMESTAMPTZ prints back as ISO-8601 text through SELECT *
SELECT * FROM place WHERE _key = 'p000'
```

```sql
-- and as a GROUP BY key, which is the same rule
SELECT day, count(*) AS n FROM place GROUP BY day LIMIT 5
```

```sql refused
-- refused 0A000: EXTRACT (the pre-image is a SET of ranges, which is the membership union)
SELECT _key FROM place WHERE EXTRACT(MONTH FROM at) = 3
```

```sql refused
-- refused 0A000: interval (a calendar interval folds to no constant)
SELECT _key FROM place WHERE at > now() - interval '1 month'
```

```sql refused
-- refused 0A000: to_char (an unnamed template is refused at prepare, not on the first row)
SELECT to_char(at, 'Day, DD Mon YYYY') AS pretty FROM place LIMIT 1
```

```sql refused
-- refused 0A000: lower (a fold with no expression index is refused rather than scanned)
SELECT _key FROM place WHERE lower(name) = 'x'
```

```sql refused
-- refused 0A000: AT TIME ZONE
SELECT _key FROM place WHERE at AT TIME ZONE 'UTC' > '1990-01-01'
```

```sql refused
-- refused 0A000: COALESCE
SELECT coalesce(tag, kind) AS label FROM place LIMIT 1
```

```sql refused
-- refused 0A000: REGEXP_REPLACE
SELECT regexp_replace(name, 'a', 'b') AS x FROM place LIMIT 1
```

```sql refused
-- refused 0A000: ->> (a select list is not one of the two positions it compiles in)
SELECT tag ->> 'a' FROM place
```

`->>` over a `JSONB` column IS Tier 1, in the `CREATE INDEX` target and in the
WHERE equality that matches it, and in nothing else. The fixture's `meta`
column carries `{"author": ..., "pinned": ...}` on every row of `posts`
(`docs/lang/EXAMPLE_FIXTURE.md`):

```sql
CREATE INDEX posts_meta_author ON posts ((meta->>'author'));
-- the equality over that same expression is answered from that index
SELECT _key FROM posts WHERE meta->>'author' = 'bob';
-- a member whose value is absent, null or non-scalar stores the NULL key,
-- which no equality can name
SELECT count(*) AS n FROM posts WHERE meta->>'author' = 'nobody';
```

```sql refused
-- refused 0A000: -> (it returns the JSON value, which has no scalar index key)
SELECT _key FROM posts WHERE meta -> 'author' = 'bob'
```

### 8.4 Graph (§4.3)

```sql
-- element pattern, edge type and direction
SELECT k FROM GRAPH_TABLE (base MATCH (a:place WHERE a._key = 'p000')-[:near]->(b:place) COLUMNS (b._key AS k))
```

```sql
-- {n,m} quantifiers are min/max depth
SELECT k FROM GRAPH_TABLE (base MATCH (a:place WHERE a._key = 'p000')-[:near]->{1,4}(b:place) COLUMNS (b._key AS k))
```

```sql
-- inline WHERE on the EDGE prunes per hop, over the posting's own property
SELECT k FROM GRAPH_TABLE (base MATCH (a:place WHERE a._key = 'p000')-[r:near WHERE r.weight > 0.2]->{1,4}(b:place) COLUMNS (b._key AS k))
```

```sql
-- inline WHERE on the far NODE, membership-able, prunes over index postings
SELECT k FROM GRAPH_TABLE (base MATCH (a:place WHERE a._key = 'p000')-[:near]->{1,4}(b:place WHERE b.kind = 'port') COLUMNS (b._key AS k))
```

```sql
-- COLUMNS over the reaching edge, and ORDER BY that edge column
SELECT k, w FROM GRAPH_TABLE (base MATCH (a:place WHERE a._key = 'p000')-[r:near]->{1,6}(b:place) COLUMNS (b._key AS k, r.weight AS w)) ORDER BY w DESC LIMIT 3
```

```sql
-- a post-pattern WHERE filters completed matches and keeps the traversal driving
SELECT k, w FROM GRAPH_TABLE (base MATCH (a:place WHERE a._key = 'p000')-[r:near]->{1,6}(b:place) COLUMNS (b._key AS k, r.weight AS w)) WHERE _key <= 'p004' ORDER BY w DESC
```

```sql refused
-- refused 0A000: a row-bound inline node predicate
SELECT k FROM GRAPH_TABLE (base MATCH (a:place WHERE a._key = 'p000')-[:near]->{1,4}(b:place WHERE b.tag IS NULL) COLUMNS (b._key AS k))
```

```sql refused
-- refused 0A000: label alternation is a multi-type hop
SELECT k FROM GRAPH_TABLE (base MATCH (a:place WHERE a._key = 'p000')-[:near|far]->(b:place) COLUMNS (b._key AS k))
```

```sql refused
-- refused 0A000: ANY SHORTEST
SELECT k FROM GRAPH_TABLE (base MATCH ANY SHORTEST (a:place WHERE a._key = 'p000')-[:near]->+(b:place) COLUMNS (b._key AS k))
```

```sql refused
-- refused 0A000: PATH_LENGTH
SELECT d FROM GRAPH_TABLE (base MATCH (a:place WHERE a._key = 'p000')-[:near]->+(b:place) COLUMNS (path_length() AS d))
```

```sql refused
-- refused 0A000: TRAIL (IS ACYCLIC is the default and the only mode; the pattern head asks the table before it demands the `(`)
SELECT k FROM GRAPH_TABLE (base MATCH TRAIL (a:place WHERE a._key = 'p000')-[:near]->+(b:place) COLUMNS (b._key AS k))
```

### 8.5 Spatial (§4.4)

```sql
-- ST_DWithin on a point column is a Radius
SELECT _key FROM place WHERE ST_DWithin(loc, ST_SetSRID(ST_MakePoint(106.82, -6.17), 4326)::geography, 20000)
```

```sql
-- ST_Within against an envelope on a point column is a Bbox
SELECT _key FROM place WHERE ST_Within(loc::geometry, ST_MakeEnvelope(106.0, -7.0, 108.0, -6.0, 4326))
```

```sql
-- ST_Intersects over a polygon column, with a GeoJSON literal
SELECT _key FROM place WHERE ST_Intersects(area, ST_SetSRID(ST_GeomFromGeoJSON('{"type":"Point","coordinates":[106.82,-6.17]}'), 4326)::geography)
```

```sql
-- ST_Contains over a polygon column
SELECT _key FROM place WHERE ST_Contains(area::geometry, ST_SetSRID(ST_MakePoint(106.82, -6.17), 4326))
```

```sql
-- the kNN operator is the distance order (point index)
SELECT _key FROM place ORDER BY loc <-> ST_SetSRID(ST_MakePoint(106.82, -6.17), 4326)::geography LIMIT 5
```

```sql refused
-- refused 0A000: && (ST_Within against an envelope is the Tier-1 spelling of the same rectangle)
SELECT _key FROM place WHERE loc && ST_MakeEnvelope(106.0, -7.0, 108.0, -6.0, 4326)
```

```sql refused
-- refused 0A000: ST_ASTEXT
SELECT ST_AsText(loc) AS wkt FROM place LIMIT 1
```

```sql refused
-- refused 0A000: ST_GEOMFROMTEXT
SELECT _key FROM place WHERE ST_Within(loc::geometry, ST_GeomFromText('POLYGON((106 -7,108 -7,108 -6,106 -6,106 -7))', 4326))
```

```sql refused
-- refused 0A000: ST_X
SELECT ST_X(loc) AS lon FROM place LIMIT 1
```

```sql refused
-- refused 0A000: ST_TRANSFORM
SELECT _key FROM place WHERE ST_Within(ST_Transform(loc, 3857)::geometry, ST_MakeEnvelope(0, 0, 1, 1, 4326))
```

```sql refused
-- refused 0A000: ST_BUFFER
SELECT _key FROM place WHERE ST_Intersects(area, ST_Buffer(ST_SetSRID(ST_MakePoint(106.82, -6.17), 4326), 1))
```

```sql refused
-- refused 0A000: POSTGIS_VERSION
SELECT postgis_version()
```

### 8.6 Vector (§4.5)

```sql
-- '[...]'::vector literal and the cosine operator as an order
SELECT _key FROM place ORDER BY emb <=> '[1,0,0,0]'::vector LIMIT 5
```

```sql
-- <-> is L2 over the same column
SELECT _key FROM place ORDER BY emb <-> '[1,0,0,0]'::vector LIMIT 5
```

```sql
-- <#> is negative inner product
SELECT _key FROM place ORDER BY emb <#> '[1,0,0,0]'::vector LIMIT 5
```

```sql
-- SET LOCAL ef_search = n turns the vector order approximate
SET LOCAL ef_search = 64
```

```sql refused
-- refused 0A000: a distance as a FILTER is Tier 2 while the same operator orders
SELECT _key FROM place WHERE emb <=> '[1,0,0,0]'::vector < 0.3
```

```sql refused
-- refused 0A000: <+> L1, halfvec, sparsevec and the binary quantization ops
SELECT _key FROM place ORDER BY emb <+> '[1,0,0,0]'::vector LIMIT 5
```

```sql refused
-- refused 0A000: VECTOR_DIMS
SELECT vector_dims(emb) AS d FROM place LIMIT 1
```

### 8.7 Text search (§4.6)

```sql
-- a one-term tsquery is Any of one term
SELECT _key FROM place WHERE to_tsvector('simple', body) @@ to_tsquery('simple', 'kebun')
```

```sql
-- & is All
SELECT _key FROM place WHERE to_tsvector('simple', body) @@ to_tsquery('simple', 'kebun & sawah')
```

```sql
-- | is Any
SELECT _key FROM place WHERE to_tsvector('simple', body) @@ to_tsquery('simple', 'kebun | sawah')
```

```sql
-- a quoted term list is a Phrase
SELECT _key FROM place WHERE to_tsvector('simple', body) @@ to_tsquery('simple', '"kebun sawah"')
```

```sql
-- NOT over a text leaf is the complement of the index's own document universe
SELECT _key FROM place WHERE NOT (to_tsvector('simple', body) @@ to_tsquery('simple', 'kebun'))
```

```sql
-- ORDER BY ts_rank_cd is the Bm25 order, and the value projects under an alias
SELECT _key, ts_rank_cd(to_tsvector('simple', body), to_tsquery('simple', 'kebun')) AS score FROM place WHERE to_tsvector('simple', body) @@ to_tsquery('simple', 'kebun') ORDER BY ts_rank_cd(to_tsvector('simple', body), to_tsquery('simple', 'kebun')) DESC LIMIT 5
```

```sql
-- bm25(col, 'query') is the same Score leaf under its own name
SELECT _key FROM place WHERE to_tsvector('simple', body) @@ to_tsquery('simple', 'kebun') ORDER BY bm25(body, 'kebun') DESC LIMIT 5
```

```sql
-- an arithmetic ORDER BY is ONE key: a blend over Score leaves
SELECT _key FROM place WHERE to_tsvector('simple', body) @@ to_tsquery('simple', 'kebun') ORDER BY 0.5 * bm25(body, 'kebun') + 0.5 * (1 - (emb <=> '[1,0,0,0]'::vector)) DESC LIMIT 5
```

```sql refused
-- refused 0A000: a tsquery that mixes & with | is a boolean tree of text leaves
SELECT _key FROM place WHERE to_tsvector('simple', body) @@ to_tsquery('simple', 'kebun & (sawah | pasar)')
```

```sql refused
-- refused 0A000: a text configuration beyond 'simple'
SELECT _key FROM place WHERE to_tsvector('english', body) @@ to_tsquery('english', 'gardens')
```

```sql
-- search(col, 'query') is typo-tolerant: this is an exact term
SELECT _key FROM place WHERE search(body, 'kebun')
```

```sql
-- one edit is spent on a five-character token, so a typo still finds it
SELECT _key FROM place WHERE search(body, 'kebum')
```

```sql
-- the LAST token completes as a prefix (search-as-you-type)
SELECT _key FROM place WHERE search(body, 'keb')
```

```sql
-- an earlier token does NOT complete: it matches a whole term, typos aside
SELECT _key FROM place WHERE search(body, 'kebun saw')
```

```sql
-- search_score() is the predicate's own Score leaf, in [0,1], and projects under an alias
SELECT _key, search_score() AS score FROM place WHERE search(body, 'keb') ORDER BY search_score() DESC LIMIT 5
```

```sql
-- a blended order: one key over a bounded text score and a vector similarity
SELECT _key FROM place WHERE search(body, 'kebun') ORDER BY 0.6 * search_score() + 0.4 * (1 - (emb <=> '[1,0,0,0]'::vector)) DESC LIMIT 5
```

```sql refused
-- refused 0A000: SEARCH_SCORE with no search() to score
SELECT _key FROM place WHERE kind = 'cafe' ORDER BY search_score() DESC
```

```sql refused
-- refused 0A000: WEBSEARCH_TO_TSQUERY
SELECT _key FROM place WHERE to_tsvector('simple', body) @@ websearch_to_tsquery('simple', 'kebun sawah')
```

```sql refused
-- refused 0A000: TS_HEADLINE
SELECT ts_headline(body, 'kebun') AS snippet FROM place LIMIT 1
```

### 8.8 Aggregates (§4.7)

```sql
-- count(*) with no filter: one row off the key-order driver, reading no record
SELECT count(*) AS n FROM place
```

```sql
-- count(*) with a filter counts what the filter admits
SELECT count(*) AS n FROM place WHERE kind = 'port'
```

```sql
-- GROUP BY the driving index's own value STREAMS
SELECT kind, count(*) AS n FROM place GROUP BY kind
```

```sql
-- sum, min, max, avg by group, with HAVING over the finished group
SELECT kind, sum(born) AS total, min(born) AS first, max(born) AS last, avg(born) AS mean FROM place GROUP BY kind HAVING count(*) > 1
```

```sql
-- DISTINCT is a group with no accumulators; over a full index range it SKIP-SCANS
SELECT DISTINCT kind FROM place
```

```sql
-- GROUP BY col / n, computable index-side from an Int posting
SELECT born / 10 AS decade, count(*) AS n FROM place GROUP BY born / 10
```

```sql
-- ORDER BY an aggregate alias sorts the FINISHED groups, then pages
SELECT kind, count(*) AS n FROM place GROUP BY kind ORDER BY n DESC LIMIT 3
```

```sql refused
-- refused 0A000: count(DISTINCT col) is a second unbounded structure per group
SELECT count(DISTINCT kind) AS n FROM place
```

```sql refused
-- refused 0A000: a composite group key has no atomic
SELECT kind, count(*) AS n FROM place GROUP BY kind, born
```

```sql refused
-- refused 0A000: ARRAY_AGG
SELECT kind, array_agg(name) AS names FROM place GROUP BY kind
```

```sql refused
-- refused 0A000: OVER (window functions have no atomic)
SELECT _key, row_number() OVER (ORDER BY born) AS n FROM place
```

```sql refused
-- refused 0A000: PERCENTILE_CONT
SELECT percentile_cont(0.5) AS p FROM place
```

### 8.9 Joins (§4.8) and the one-key order

```sql refused
-- refused 0A000: JOIN
SELECT p._key FROM place AS p INNER JOIN place AS q ON p._key = q._key
```

```sql refused
-- refused 0A000: two ORDER BY keys are deviation 3
SELECT _key FROM place WHERE kind = 'port' ORDER BY born ASC, name ASC
```
