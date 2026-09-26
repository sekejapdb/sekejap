# GQL profile: features, mapping and differences

This file is checked. `lang/src/gql/registry.rs` (a test, no runtime code)
reads its tables: every registry row names a test that exists, every
function and statement keyword of the profile has a row, and the unsupported
list equals the refusal table the engine uses (`sekejap_lang::gql_refusals()`),
row for row and word for word. The guide with runnable examples is
[`GQL_PROFILE.md`](GQL_PROFILE.md).

## Registry

One row per supported construct or function. `class` says where the
construct comes from: `ISO GQL` (ISO/IEC 39075), `SQL/PGQ shared` (also in
SQL/PGQ, ISO/IEC 9075-16), `Google reference` (the Spanner Graph GQL
reference), `sekejap host (PostgreSQL-style)` (PostgreSQL, PostGIS or
pgvector spelling inside a GQL body), `sekejap` (this engine's own). `T1`
means built and tested; the test named is one that pins it.

| construct | class | tier | test |
| --- | --- | --- | --- |
| `MATCH <pattern>, ...` with `IS label`, `:label` and label alternation `A\|B` | ISO GQL | T1 | `lang/tests/gql_patterns.rs::label_alternation_on_edges_and_nodes` |
| inline `WHERE` in a node or edge, and the `MATCH`'s `WHERE` | ISO GQL | T1 | `lang/tests/gql_patterns.rs::inline_and_pattern_where_agree_on_the_answer_and_differ_in_work` |
| comma patterns joined on a shared variable | ISO GQL | T1 | `lang/tests/gql_patterns.rs::comma_patterns_join_on_a_shared_variable` |
| `OPTIONAL MATCH <patterns> [WHERE]` | ISO GQL | T1 | `lang/tests/gql_optional.rs::several_matches_zero_matches_and_the_input_row_is_never_dropped` |
| `LET x = <expr>, ...` | ISO GQL | T1 | `lang/tests/gql_pipeline.rs::let_does_not_see_its_own_list_and_a_later_let_does` |
| `FILTER <predicate>` | ISO GQL | T1 | `lang/tests/gql_optional.rs::optional_predicate_placement_where_and_a_later_filter_differ` |
| `FOR x IN <list>` | ISO GQL | T1 | `lang/tests/gql_pipeline.rs::for_over_a_list_parameter_seeds_each_key` |
| `RETURN [DISTINCT] ... [GROUP BY] [ORDER BY] [OFFSET] [LIMIT]`, `RETURN *` | ISO GQL | T1 | `lang/tests/gql_pipeline.rs::return_distinct_keeps_each_row_once` |
| `NEXT` between stages | ISO GQL | T1 | `lang/tests/gql_pipeline.rs::two_stages_carry_nodes_across_next_and_rank_collaborators` |
| `CALL (imports) { stage }` | ISO GQL | T1 | `lang/tests/gql_call.rs::the_body_sees_only_its_imports` |
| `EXISTS { ... }`, `NOT EXISTS { ... }` | ISO GQL | T1 | `lang/tests/gql_exists.rs::not_exists_keeps_the_rows_with_no_match` |
| `<stage> UNION [ALL \| DISTINCT] <stage>` | ISO GQL | T1 | `lang/tests/gql_union.rs::union_removes_duplicate_whole_rows_and_union_all_keeps_them` |
| quantifiers `{n,m}`, `{n,}`, `*`, `+`, `?` on an edge or a subpath | ISO GQL | T1 | `lang/tests/gql_paths.rs::a_multi_type_subpath_repeats_as_a_whole` |
| path modes `WALK`, `TRAIL`, `ACYCLIC` | ISO GQL | T1 | `lang/tests/gql_paths.rs::path_modes_on_a_cycle` |
| selectors `ANY`, `ANY SHORTEST` | ISO GQL | T1 | `lang/tests/gql_paths.rs::any_shortest_takes_the_fewest_hops_once_per_end` |
| `ANY CHEAPEST` with an edge `COST` | Google reference | T1 | `lang/tests/gql_paths.rs::any_cheapest_keeps_the_hop_count_in_its_state` |
| a named path `p = ...` | ISO GQL | T1 | `lang/tests/gql_paths.rs::a_named_path_binds_and_is_not_a_column` |
| `GRAPH_TABLE (<graph> <body>)` inside `SELECT`, with an outer `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY`, `LIMIT` | SQL/PGQ shared | T1 | `lang/tests/gql_prepared.rs::the_outer_select_filters_orders_and_pages_the_relation_as_postgresql_does` |
| `$n` parameters, typed once for the whole statement | sekejap | T1 | `lang/tests/gql_prepared.rs::a_parameter_used_as_two_types_is_refused_at_prepare_naming_both_uses` |
| arithmetic `+ - * / % ^`, comparisons, `AND`/`OR`/`NOT`, `IS [NOT] NULL`, `IN`, `\|\|` | ISO GQL | T1 | `lang/tests/gql_expressions.rs::null_propagation_table` |
| `CASE`, `COALESCE`, `NULLIF` | ISO GQL | T1 | `lang/tests/gql_expressions.rs::case_coalesce_and_nullif_over_integers_and_floats_are_double_precision` |
| `CAST(x AS t)`, `x::t` | ISO GQL | T1 | `lang/tests/gql_expressions.rs::casts_to_the_declared_types` |
| `ABS`, `SQRT`, `POWER`, `EXP`, `LN` | ISO GQL | T1 | `lang/tests/gql_expressions.rs::ln_of_zero_or_a_negative_number_raises_2201e` |
| `LOWER`, `UPPER`, `TRIM`, `LENGTH`, `SUBSTRING`, `CONCAT` | ISO GQL | T1 | `lang/tests/gql_expressions.rs::string_functions_match_the_sql_row_functions` |
| `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, `ARRAY_AGG` over rows | ISO GQL | T1 | `lang/tests/gql_pipeline.rs::aggregate_columns_are_typed_as_postgresql_types_them` |
| the same aggregates over a path's group variable (horizontal) | ISO GQL | T1 | `lang/tests/gql_functions.rs::horizontal_a_path_sum_differs_from_a_sum_over_paths` |
| list literals `[a, b]` | ISO GQL | T1 | `lang/tests/gql_functions.rs::list_literals_are_typed_at_compile_time` |
| `PATH_LENGTH`, `PATH_FIRST`, `PATH_LAST`, `NODES`, `EDGES` | ISO GQL | T1 | `lang/tests/gql_functions.rs::path_functions_read_the_path_the_search_built` |
| `IS_ACYCLIC`, `IS_TRAIL` | sekejap | T1 | `lang/tests/gql_functions.rs::path_functions_is_acyclic_and_is_trail_inspect_repetition` |
| `ELEMENT_ID` | ISO GQL | T1 | `lang/tests/gql_functions.rs::element_functions_element_id_is_unique_within_a_query` |
| `SOURCE_NODE_ID`, `DESTINATION_NODE_ID` | sekejap | T1 | `lang/tests/gql_functions.rs::element_functions_source_and_destination_are_the_stored_endpoints` |
| `LABELS`, `PROPERTY_NAMES` | sekejap | T1 | `lang/tests/gql_functions.rs::element_functions_labels_and_property_names_keep_presence` |
| `ARRAY_LENGTH` | sekejap host (PostgreSQL-style) | T1 | `lang/tests/gql_functions.rs::horizontal_aggregates_fold_scalar_lists_too` |
| `to_tsvector('simple', n.f) @@ to_tsquery('simple', q)` | sekejap host (PostgreSQL-style) | T1 | `lang/tests/gql_host.rs::a_text_match_keeps_what_sql_matches` |
| `bm25(n.f, q)` | sekejap host (PostgreSQL-style) | T1 | `lang/tests/gql_host.rs::bm25_scores_the_matching_nodes` |
| `ST_DWithin`, `ST_Intersects`, `ST_Within`, `ST_Contains`, `ST_Distance` | sekejap host (PostgreSQL-style) | T1 | `lang/tests/gql_host.rs::the_spatial_forms_answer_in_metres_and_keep_the_unit_rules` |
| `<->`, `<=>`, `<#>`, `x::vector` | sekejap host (PostgreSQL-style) | T1 | `lang/tests/gql_host.rs::the_vector_distances_are_pgvectors` |
| index seeds: every conjunct of the seed node an index answers | sekejap | T1 | `lang/tests/gql_host.rs::every_index_answered_conjunct_seeds_the_node` |
| index lineage: a later `FILTER` or outer `WHERE` moves into the seed | sekejap | T1 | `lang/tests/gql_host.rs::a_later_conjunct_moves_into_the_seed_by_lineage` |
| top-k by `<->` / `<#>` read in the exact vector index's order | sekejap | T1 | `lang/tests/gql_host.rs::a_top_k_by_distance_reads_the_index_order_and_stops_early` |
| `SET LOCAL ef_search` read when an execution opens | sekejap | T1 | `lang/tests/gql_host.rs::ef_search_is_read_when_the_execution_opens` |
| `EXPLAIN` of a GQL statement | sekejap | T1 | `lang/tests/gql_explain.rs::every_operator_is_printed_with_its_seed_and_where_each_predicate_runs` |
| a budget: complete, or a named refusal after a prefix | sekejap | T1 | `lang/tests/gql_complete_or_error.rs::every_budgeted_answer_completes_or_ends_in_a_named_refusal` |

## Mapping to the standards

Nothing here claims conformance. The ISO/IEC 39075 feature identifiers are
not listed: none has been checked against the normative text, and a guessed
identifier would read as a claim.

| area | where it is in this profile | note |
| --- | --- | --- |
| graph pattern matching (node and edge patterns, label expressions, quantified paths, path modes, selectors) | `MATCH`, `OPTIONAL MATCH` | label conjunction, negation and wildcard, `SIMPLE` and `ALL SHORTEST` are refused (see the unsupported list) |
| linear query statements (`LET`, `FILTER`, `FOR`, `RETURN`, `NEXT`) | the stages of a body | one statement per clause, as ISO writes them |
| composite queries (`UNION`, `CALL`, `EXISTS`) | inside a body | `INTERSECT`, `EXCEPT`, `OPTIONAL CALL` are refused |
| SQL/PGQ `GRAPH_TABLE` | the only way a body is written | the SQL/PGQ `COLUMNS` clause is not adopted; a body ends in `RETURN` |
| cheapest paths | `ANY CHEAPEST ... COST` | the Google reference's form, one `COST` per pattern |
| text, spatial and vector functions | the host forms | PostgreSQL, PostGIS and pgvector spellings, the same as the SQL surface |

## Differences from the standards and from PostgreSQL

| difference | reason |
| --- | --- |
| the graph argument names a graph context; `base` is context 0, and the node universe is every visible collection row | the engine stores graphs as named contexts over its collections |
| a label is a collection (on a node) or an edge type (on an edge) | the storage model: one collection per node label |
| `ELEMENT_ID` is unique within one query only | only query-scope uniqueness is promised; the encoding is versioned (`n1:`, `e1:`) so it can change |
| lists travel as PostgreSQL arrays | the wire and the SQL surface are PostgreSQL's |
| a `COST` of zero, a negative, `NULL`, NaN or infinity is an error (`InvalidPathCost`) | the reference asks for a positive finite cost; not yet checked against Google's current documentation |
| `ARRAY_AGG` over zero rows is `NULL`; over a zero-length path's group variable it is an empty list | SQL's rule for the vertical aggregate; the horizontal case is unverified against the Google reference |
| two automaton runs that give the same path and the same bindings are one match (per input row); distinct bindings stay distinct matches | not yet confirmed against the normative ISO text |
| a path pattern takes a selector or a path mode, not both | the combined form needs a separately verified nested pattern (P1) |
| `ANY CHEAPEST` crosses one edge step, which carries the one `COST` | the cheapest search takes one cost (P1 for several) |
| `CALL` names its imports: `CALL (a, b) { ... }` or `CALL () { ... }` | a body sees only what it imports; there is no implicit import |
| `UNION` branches match their columns by name and position | the result has one column list |
| a vector order inside a body is exact unless `SET LOCAL ef_search` is set | owner decision; the SQL surface's default over an approximate-only column is approximate |
| a text form needs a READY text index on the field | there is no text analyzer outside an index; the row is never re-tokenized |
| inside a body, an absent property and a stored null are both `NULL`; `IS MISSING` is SQL-only, and `PROPERTY_NAMES` tells whether a property is present | brief rule 8: a missing property evaluates to null in GQL; the SQL surface's missing contract is unchanged |

## Unsupported

Every construct the profile refuses by name, with its tier (`T2`: planned,
the reason names when; `T3`: not adopted) and the reason the refusal
carries.

| construct | tier | reason |
| --- | --- | --- |
| `ANY CHEAPEST over several edge steps` | T2 | GQL profile P1: the cheapest search takes ONE COST, so an ANY CHEAPEST pattern crosses exactly one edge step -- one edge, quantified or not -- which carries it (design Q36). A COST per step over several edge steps is a P1 construct. |
| `search()` | T2 | GQL profile P1: the typo-tolerant `search()` and `search_score()` inside a GQL body are P1 constructs (design Q30). A GQL body matches text through the index as SQL spells it, `to_tsvector('simple', n.field) @@ to_tsquery('simple', q)`, and ranks with `bm25(n.field, q)`. |
| `ALL SHORTEST` | T2 | GQL profile P1: ALL SHORTEST is a P1 construct, after the P0 release. |
| `SIMPLE` | T2 | GQL profile P1: the SIMPLE path mode is a P1 construct, after the P0 release. |
| `selector with a path mode` | T2 | GQL profile P1: a path pattern takes a selector (ANY, ANY SHORTEST, ANY CHEAPEST, which search walks) OR a path mode (WALK, TRAIL, ACYCLIC), not both; the combined form needs a separately verified nested pattern, a P1 construct. |
| `nested quantifier` | T2 | GQL profile P1: a quantifier inside a quantified subpath is a P1 construct (design Q10): v1 keeps one active counter per path search. Quantifiers written one after another are accepted. |
| `label conjunction` | T2 | GQL profile P1: label conjunction `&` is a P1 construct; a label test is a name or an alternation `A\|B`. |
| `label negation` | T2 | GQL profile P1: label negation `!` is a P1 construct; a label test is a name or an alternation `A\|B`. |
| `label wildcard` | T2 | GQL profile P1: the label wildcard `%` is a P1 construct; write no label to match any element. |
| `INTERSECT` | T2 | GQL profile P1: INTERSECT inside a GRAPH_TABLE body is a P1 construct. |
| `EXCEPT` | T2 | GQL profile P1: EXCEPT inside a GRAPH_TABLE body is a P1 construct. |
| `OPTIONAL block` | T2 | GQL profile P1: ISO's block form OPTIONAL { MATCH ...; MATCH ... } is a P1 construct (design Q17). OPTIONAL MATCH p1, p2 [WHERE ...] already matches every pattern optional together. |
| `OPTIONAL CALL` | T2 | GQL profile P1: OPTIONAL CALL, the left-outer form that keeps an input row its body gives no row, is a P1 construct (design Q21). CALL (...) { ... } drops such a row, as an inner join does. |
| `NEXT inside CALL` | T2 | GQL profile P1: a CALL body is one stage (design Q22); NEXT inside it is a P1 construct. Chain stages after the CALL instead. |
| `mixed UNION` | T2 | GQL profile P1: one UNION chain takes one conjunction (design Q24); mixing UNION ALL with UNION or UNION DISTINCT needs ISO's parenthesised composite form, a P1 construct. |
| `PATH_SUM` | T3 | GQL profile, not adopted: a path accumulator is SUM over the path's group variable in a LET (horizontal aggregation, `LET total = SUM(e.cost)`). |
| `PATH_PRODUCT` | T3 | GQL profile, not adopted: a path product is EXP(SUM(LN(x))) over the path's group variable in a LET (horizontal aggregation), for strictly positive values. |
| `PATH_MIN` | T3 | GQL profile, not adopted: a path accumulator is MIN over the path's group variable in a LET (horizontal aggregation). |
| `PATH_MAX` | T3 | GQL profile, not adopted: a path accumulator is MAX over the path's group variable in a LET (horizontal aggregation). |
| `PATH_AVG` | T3 | GQL profile, not adopted: a path accumulator is AVG over the path's group variable in a LET (horizontal aggregation). |
| `VERTEX_ID` | T3 | GQL profile, not adopted: an element's id is ELEMENT_ID. |
| `EDGE_ID` | T3 | GQL profile, not adopted: an element's id is ELEMENT_ID. |
| `COLUMNS` | T3 | GQL profile, not adopted: the SQL/PGQ GRAPH_TABLE ... COLUMNS (...) body is removed, with no compatibility alias (owner decision, docs/lang/GQL_PROFILE_DESIGN.md). GRAPH_TABLE takes a GQL body: write RETURN ... instead of COLUMNS (...). |
