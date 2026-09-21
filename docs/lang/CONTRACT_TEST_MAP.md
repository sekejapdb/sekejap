# Contract test map

Every numbered rule in `docs/core/GRAPH_CONTRACT.md` and every T1 row in
`docs/lang/QL_CONTRACT.md`, mapped to the test file and test name that pins it at
HEAD, or `UNPINNED`. Grepped from `tests/`. T1 is defined as compiling to an
atomic that exists; the tests named here pin that atomic (there is no SQL
parser at HEAD). A row that names several constructs is `UNPINNED` for any
construct the tests do not pin.

Two sections at the end carry the contract rows added on 2026-09-20 when the
33 `docs/lang/E3_PARITY.md` NOT IN CONTRACT capabilities were tiered: 23 new rows
in `docs/lang/QL_CONTRACT.md` and 13 in the new `docs/dist/OPS_CONTRACT.md`. All 36 are
`UNPINNED` and all 36 are T2 — by the tier's own definition nothing behind
them is built, so there is nothing yet to pin. They are counted separately
from the T1 set so the T1 unpinned number keeps meaning what it meant.

## Graph contract (`docs/core/GRAPH_CONTRACT.md`)

| Rule | Behaviour | Tests |
| --- | --- | --- |
| 1.1 | A node is a row in any collection; it exists with or without edges; its fields and indexes serve every graph that references it | `tests/graph_collections.rs` `directed_typed_context_edges_replace_properties_and_survive_reopen` (inter-collection `member_of`); `tests/graph_collections.rs` `graph_filter_answers_what_traverse_bfs_answers`; `tests/query_multimodel.rs` `graph_scalar_spatial_text_and_vector_apply_before_ranked_top_k` |
| 1.2 | Node identity is the external key; keys are opaque ordered strings; a key prefix is one ordered range | `tests/collections.rs` `identity_upsert_delete_and_collection_isolation_follow_e3_contracts`; `tests/query_candidate_budget.rs` `a_key_range_pages_resume_in_key_order_with_deleted_keys_absent`; `tests/query_candidate_budget.rs` `a_key_prefix_is_expressed_as_a_range_and_resumes_the_same_way` |
| 1.3 | Shared vs private is a business rule in the key, never an engine rule | UNPINNED |
| 2.1 | Edge is directed, typed, two entities in any two collections, lives in exactly one context (none = base) | `tests/graph_collections.rs` `directed_typed_context_edges_replace_properties_and_survive_reopen` |
| 2.2 | Native keyspace `source, context, type, destination`; properties inline; reverse mirror always written; one source/context/type is one contiguous range | `tests/graph_collections.rs` `directed_typed_context_edges_replace_properties_and_survive_reopen`; `tests/graph_collections.rs` `neighbor_ids_answer_the_same_adjacency_as_neighbors_without_properties`; `tests/graph_admission.rs` `edge_damage_fails_the_read_that_needs_it_and_the_verifier_reports_the_rest` |
| 2.3 | Element identity: edge-id segment under an additive feature bit; parallel edges coexist | UNPINNED (`link` of the same pair replaces the posting; no edge-id in the key) |
| 2.4 | Property bag stays; an edge type MAY declare typed properties, encoded fixed-width | bag: `tests/graph_collections.rs` `directed_typed_context_edges_replace_properties_and_survive_reopen`. declared typed properties: UNPINNED |
| 2.5 | Edge types interned on first use; catalog type rows derived from written edges | `tests/graph_collections.rs` `directed_typed_context_edges_replace_properties_and_survive_reopen` (`link` with a new type name); `tests/graph_collections.rs` `cyclic_bfs_is_shortest_hop_deterministic_and_bounded` (`create_edge_type`) |
| 2.6 | Updating an edge's properties is an in-place rewrite of its posting | `tests/graph_collections.rs` `directed_typed_context_edges_replace_properties_and_survive_reopen` |
| 3.1 | A context is a named graph (id in the edge key); one context is one contiguous range that can be listed, copied or dropped as a range | listed: `tests/graph_collections.rs` `directed_typed_context_edges_replace_properties_and_survive_reopen`. copied / dropped as a range: UNPINNED (no `drop_context`) |
| 3.2 | Contexts own edges only; nodes are outside every context | `tests/graph_collections.rs` `entity_delete_cascades_both_directions_all_contexts_and_self_edges`; `tests/graph_collections.rs` `directed_typed_context_edges_replace_properties_and_survive_reopen` |
| 3.3 | A traversal runs in one context (or the base graph) | `tests/graph_collections.rs` `cyclic_bfs_is_shortest_hop_deterministic_and_bounded`; `tests/graph_collections.rs` `graph_filter_answers_what_traverse_bfs_answers` |
| 3.4 | A context descriptor (name, owner, created) is catalog data | name intern: `tests/graph_collections.rs` `directed_typed_context_edges_replace_properties_and_survive_reopen`. owner, created: UNPINNED |
| 4.1 | Budgeted BFS: seed(s), direction, type or all, min/max depth, visited and edge budgets, result limit, cancellation, pageable; a node is never revisited (ACYCLIC) | `tests/graph_collections.rs` `cyclic_bfs_is_shortest_hop_deterministic_and_bounded`; `tests/graph_collections.rs` `neighbor_and_bfs_cancellation_never_return_partial_or_poison_reads`; `tests/graph_collections.rs` `graph_filter_answers_what_traverse_bfs_answers`; `tests/query_multimodel.rs` `graph_scalar_spatial_text_and_vector_apply_before_ranked_top_k` |
| 4.2 | Traversal binds the reaching edge to each result and can read its properties | `tests/graph_hop_predicates.rs` `oracle_five_hundred_sampled_traversals_equal_a_brute_force_bfs` (the bound edge equals the stored bag); `tests/graph_hop_predicates.rs` `a_node_reached_by_two_edges_reports_the_first_admitted`; `tests/graph_hop_predicates.rs` `the_reaching_edge_projects_and_ranks_without_a_row`; `tests/graph_hop_predicates.rs` `an_incoming_hop_reads_the_primary_posting_for_its_predicate`; `lang/tests/sql_tier1.rs` `graph_table_columns_project_the_reaching_edge_and_order_by_it` |
| 4.3 | Per-hop predicates on edge properties or node fields; failing edge never followed; covered fields never read a row | `tests/graph_hop_predicates.rs` `oracle_five_hundred_sampled_traversals_equal_a_brute_force_bfs`; `tests/graph_hop_predicates.rs` `a_predicate_that_rejects_shrinks_the_frontier_and_still_counts_the_edge` (no row decode, pruned edges still counted); `tests/graph_hop_predicates.rs` `a_node_predicate_an_index_cannot_answer_is_refused_at_prepare`; `tests/graph_hop_predicates.rs` `a_node_predicate_refuses_a_row_of_another_collection`; `lang/tests/sql_tier1.rs` `graph_table_inline_edge_where_compiles_to_a_per_hop_prune`; `lang/tests/sql_tier1.rs` `graph_table_inline_node_where_compiles_to_a_membership_prune`; `lang/tests/sql_tier1.rs` `a_row_bound_inline_node_predicate_is_refused_with_its_tier`; `lang/tests/sql_explain.rs` `explain_prints_the_edge_predicates_and_the_node_membership_sets` |
| 4.4 | One predicate set applies to every hop; per-hop patterns are Phase 3 | `tests/graph_hop_predicates.rs` `oracle_five_hundred_sampled_traversals_equal_a_brute_force_bfs` (the reference applies the same set at every depth, up to three); `lang/tests/sql_tier1.rs` `graph_table_inline_edge_where_compiles_to_a_per_hop_prune` (a `{1,4}` quantifier, one predicate set) |
| 4.5 | Traversal is a query filter and a candidate driver; conjoins with scalar/text/point/geometry and any single order; seeds may come from another order | `tests/graph_collections.rs` `graph_filter_answers_what_traverse_bfs_answers`; `tests/query_multimodel.rs` `graph_scalar_spatial_text_and_vector_apply_before_ranked_top_k`; `tests/query_combinations.rs` `query_engine_surface_combinations` |
| 5.1 | Paths streamed: one accumulator per frontier entry; full path rebuilt only for returned rows | UNPINNED |
| 5.2 | A path aggregate is a Score leaf | UNPINNED |
| 5.3 | Shortest path atomic: ANY SHORTEST and ALL SHORTEST, bidirectional, unweighted first | UNPINNED (`cyclic_bfs_is_shortest_hop_deterministic_and_bounded` pins BFS hop order, not this atomic) |
| 6.1 | RESTRICT (default) refuses node delete while any edge in any context references it; CASCADE is explicit and bounded | CASCADE: `tests/graph_collections.rs` `entity_delete_cascades_both_directions_all_contexts_and_self_edges`; `tests/graph_collections.rs` `entity_delete_refuses_degree_257_before_any_published_change`. RESTRICT default, at collection granularity: `tests/drop_collection.rs` `restrict_names_the_referencing_contexts_and_cascade_removes_the_edges`; `lang/tests/sql_tier1.rs` `drop_table_restricts_on_graph_edges_and_cascades_when_asked`. RESTRICT on a SINGLE `delete(key)`: UNPINNED (that path still cascades) |
| 6.2 | Deleting an edge removes its forward and reverse postings in its context | `tests/graph_collections.rs` `snapshots_unlink_missing_endpoints_and_delete_cascade_are_transactional`; `tests/graph_collections.rs` `directed_typed_context_edges_replace_properties_and_survive_reopen` |
| 6.3 | Dropping a context removes its range; nodes are untouched | UNPINNED |
| L1 | Frontier, visited set and one accumulator per visited node, all bounded by the visited budget; never RAM ∝ graph | `tests/graph_collections.rs` `bfs_allocates_a_constant_plus_its_result_not_per_edge`; `tests/graph_collections.rs` `graph_filtered_query_allocates_like_the_traversal_it_runs` |
| L2 | Work per hop is the postings of that source in that context and type | `tests/graph_collections.rs` `traversal_reads_pay_for_leaf_pages_not_for_edges`; `tests/edge_write_budget.rs` `the_cost_of_an_edge_does_not_grow_with_the_rows_loaded_before_it` |
| L3 | RESTRICT is the default delete; CASCADE is explicit and bounded | `tests/drop_collection.rs` `restrict_names_the_referencing_contexts_and_cascade_removes_the_edges` (`begin_drop_collection` is RESTRICT, `begin_drop_collection_mode(.., Cascade)` is the opt-in); `tests/drop_collection.rs` `the_drop_removes_exactly_the_collections_records_and_nothing_else` (the cascade runs inside `budget`-bounded steps). Single-row `delete(key)`: UNPINNED (still cascades) |
| L4 | Sacrifices named: 8 bytes/edge identity; declared property fixed-width; reverse mirror doubles storage | UNPINNED (identity and declared properties are not on disk; no test measures the reverse-mirror byte cost) |
| L5 | A corrupt posting or bag is `Corrupt` on that edge, never a panic; offset reads bounds-checked | `tests/graph_admission.rs` `edge_damage_fails_the_read_that_needs_it_and_the_verifier_reports_the_rest`; `tests/graph_admission.rs` `damaged_graph_metadata_replica_falls_back_without_rewriting_source` |
| L6 | A traversal reads its snapshot; a concurrent writer is invisible to it | `tests/graph_collections.rs` `snapshots_unlink_missing_endpoints_and_delete_cascade_are_transactional`; `tests/query_multimodel.rs` `one_snapshot_stays_old_while_committed_multifamily_mutation_reopens_new` |
| L8 | Element identity and typed properties are additive feature bits; old files open unchanged | GRAPH_FEATURE: `tests/graph_admission.rs` `graph_rows_cannot_be_hidden_by_clearing_feature_and_metadata`; `tests/graph_admission.rs` `intact_future_graph_header_or_name_replica_refuses_before_source_mutation`. identity and typed-property bits: UNPINNED |

## Query language T1 (`docs/lang/QL_CONTRACT.md`)

### §2 Statements

| Construct | Atomic | Tests |
| --- | --- | --- |
| `SELECT ... FROM <collection> [WHERE] [ORDER BY one expr] [LIMIT]` | `prepare_query`: filters AND, one order, pages | `tests/query_scalar.rs` `scalar_json_filters_keep_exact_numbers_null_missing_and_page_order`; `tests/query_page_walk.rs` `a_limit_stops_the_candidate_walk_when_the_driver_is_already_in_rank_order`; `tests/query_combinations.rs` `query_engine_surface_combinations` |
| `SELECT ... FROM GRAPH_TABLE (...)` | traversal driver/filter | `tests/graph_collections.rs` `graph_filter_answers_what_traverse_bfs_answers`; `tests/query_multimodel.rs` `graph_scalar_spatial_text_and_vector_apply_before_ranked_top_k` |
| `INSERT INTO t (...) VALUES (...)`, `$n` params | `put` by key | `tests/collections.rs` `identity_upsert_delete_and_collection_isolation_follow_e3_contracts`; `tests/collection_pagewal.rs` `mixed_records_roundtrip_through_page_wal_commit_and_reopen` |
| `UPDATE t SET ... WHERE key = $1` | `put` replaces; partial update = read-modify-put | `tests/collections.rs` `identity_upsert_delete_and_collection_isolation_follow_e3_contracts` |
| `DELETE FROM t WHERE key = $1` | delete by key; RESTRICT/CASCADE per graph 6.1 | delete by key: `tests/collections.rs` `identity_upsert_delete_and_collection_isolation_follow_e3_contracts`; `tests/collection_pagewal.rs` `delete_then_reinsert_allocates_a_fresh_identity_and_never_reuses_after_reopen`. RESTRICT: UNPINNED (see graph 6.1) |
| `CREATE TABLE`, `CREATE INDEX ... USING {btree,gin,gist,exact,quantized,adjacency}`, `DROP TABLE [IF EXISTS] [CASCADE\|RESTRICT]`, `DROP INDEX` | catalog descriptors; `begin_drop_collection` / `drop_collection_step` | `DROP TABLE`: `tests/drop_collection.rs` `a_dropped_collection_leaves_every_keyspace_empty_and_its_name_free`, `an_interrupted_drop_resumes_to_the_state_an_uninterrupted_one_reaches`, `restrict_names_the_referencing_contexts_and_cascade_removes_the_edges`, `a_published_dropping_mark_refuses_every_reader_and_writer`, `sql_drop_table_if_exists_and_cascade_end_to_end`, `the_drop_removes_exactly_the_collections_records_and_nothing_else`; `lang/tests/sql_tier1.rs` `drop_table_restricts_on_graph_edges_and_cascades_when_asked`; `lang/tests/sql_explain.rs` `explain_drop_table_prints_the_phases_and_runs_nothing`. table: `tests/collections.rs` `undeclared_fields_roundtrip_through_collection_and_reopen`. btree: `tests/index_lifecycle.rs` `late_index_tracks_crud_and_published_snapshots`. gin: `tests/index_text.rs` `building_live_crud_snapshot_rollback_and_reopen_preserve_presence_rules`. gist (point): `tests/index_spatial.rs` `build_live_crud_snapshot_rollback_drop_and_reopen`. gist (geometry): `tests/index_spatial_geometry.rs` `build_vs_maintain_posting_identity_across_geometry_kinds`. exact: `tests/index_vector.rs` `immutable_layout_ordinals_live_crud_snapshots_rollback_and_reopen`. quantized: `tests/index_vector_quantized.rs` `validation_live_crud_snapshot_reopen_and_drop_preserve_authoritative_vectors`. DROP: `tests/index_lifecycle.rs` `drop_and_build_can_resume_after_reopen_without_claiming_ready`. adjacency as `CREATE INDEX`: UNPINNED (graph is `enable_graph` + `put_edge`, not an `IndexFamily`) |
| `BEGIN [READ ONLY]`, `COMMIT`, `ROLLBACK` | one writer, snapshot readers | `tests/collections.rs` `published_snapshots_rollback_and_streaming_cursor_are_consistent`; `tests/collection_pagewal.rs` `rollback_discards_uncommitted_work_beside_a_live_snapshot`; `tests/collection_pagewal.rs` `snapshots_serve_published_state_defer_checkpoint_and_are_bounded`; `tests/pagewal.rs` `new_snapshot_during_uncommitted_changes_sees_latest_commit` |

### §3 Predicates

| Construct | Atomic | Tests |
| --- | --- | --- |
| `AND` | filter conjunction | `tests/query_multimodel.rs` `graph_scalar_spatial_text_and_vector_apply_before_ranked_top_k`; `tests/query_combinations.rs` `query_engine_surface_combinations`; `tests/query_scalar_fold.rs` `folding_survives_a_third_predicate_and_a_second_index` |
| `=, <>, <, <=, >, >=` on indexed scalar | Scalar Eq/Range | `=`: `tests/query_scalar.rs` `scalar_json_filters_keep_exact_numbers_null_missing_and_page_order`. range inequalities: `tests/query_scalar_fold.rs` `folding_two_predicates_on_one_index_keeps_the_answer`; `tests/query_combinations.rs` `hand_picked_surface_combinations` (`hp07_same_index_born_fold`). `<>`: `tests/query_boolean.rs` `random_boolean_trees_match_brute_force`; `lang/tests/sql_tier1.rs` `not_equal_is_the_complement_of_an_equality` |
| `BETWEEN a AND b` | Range | `tests/query_scalar_fold.rs` `folding_two_predicates_on_one_index_keeps_the_answer`; `tests/query_candidate_budget.rs` `a_spatial_driven_pages_non_driving_range_reads_no_row` |
| `IS NULL`, `IS MISSING` | Scalar IsNull/IsMissing | `tests/query_combinations.rs` `hand_picked_surface_combinations` (`hp04_ismissing_score`, `hp05_isnull_kind`); `tests/query_scalar.rs` `scalar_json_filters_keep_exact_numbers_null_missing_and_page_order` |
| `IS NOT NULL` | complement of the nullish key: one posting range | `lang/tests/sql_tier1.rs` `is_not_null_is_the_complement_of_the_nullish_key`; `lang/tests/sql_explain.rs` `the_boolean_battery_explains_its_sets_and_its_counters` |
| `OR` on the same index, `IN (list)` | union of ranges as one membership set | `tests/query_boolean.rs` `random_boolean_trees_match_brute_force`; `lang/tests/sql_tier1.rs` `a_disjunction_of_equalities_is_one_membership_set`; `lang/tests/sql_tier1.rs` `in_a_list_is_the_same_union_written_shorter` |
| `OR` across indexes, parenthesised groups | union of the leaves' membership sets; redundant parentheses round an `AND` change nothing about the plan | `lang/tests/sql_tier1.rs` `a_disjunction_across_two_families_unions_two_sets`; `lang/tests/sql_tier1.rs` `a_parenthesised_group_binds_the_way_sql_says`; `lang/tests/sql_tier1.rs` `redundant_parentheses_do_not_change_what_compiles`; `tests/query_boolean.rs` `random_boolean_trees_match_brute_force` |
| `NOT` | complement over a leaf's own universe (the text index's own documents for a text leaf, the point postings for a point leaf, the live rows for a caller's `Ids` set) | `tests/query_boolean.rs` `a_deleted_row_is_never_in_a_complement`; `tests/query_boolean.rs` `a_complement_is_bounded_by_a_named_resource`; `tests/query_boolean.rs` `a_null_text_field_is_in_neither_the_leaf_nor_its_complement`; `lang/tests/sql_tier1.rs` `not_before_a_group_is_de_morgan`; `lang/tests/sql_tier1.rs` `not_in_a_list_is_the_complement_of_the_union` |
| `EXISTS (subquery)`, `key IN (subquery)`, `NOT EXISTS` | semi-join membership set, bounded and cancellable while the statement compiles; a non-text projected column is refused naming it | `lang/tests/sql_tier1.rs` `exists_over_an_edge_type_is_a_semi_join`; `lang/tests/sql_tier1.rs` `a_semi_join_over_a_non_text_column_is_refused_naming_it`; `lang/tests/sql_tier1.rs` `a_semi_join_is_cancellable_while_it_compiles`; `tests/query_boolean.rs` `a_semi_join_set_is_checked_not_trusted`; `lang/tests/sql_explain.rs` `a_semi_join_explains_the_set_it_built` |
| a boolean leaf with no set (geometry, traversal, `JsonEq`, a text phrase, `IS NULL`/`IS MISSING`) | refused at prepare, naming the leaf | `tests/query_boolean.rs` `a_leaf_with_no_set_is_refused_at_prepare`; `lang/tests/sql_tier1.rs` `a_disjunction_with_a_geometry_leaf_is_refused_with_its_reason`; `lang/tests/sql_refusals.rs` `an_inequality_is_the_complement_of_an_equality` |
| a disjunction as the candidate driver (`QueryDriver::Membership`) | the union set walked in entity-id order, its zero-bit scan charged and pollable every 4 KiB | `tests/query_boolean.rs` `a_union_drives_only_when_nothing_else_can`; `tests/query_boolean.rs` `a_union_pages_disjointly_and_completely`; `tests/query_boolean.rs` `a_union_walk_is_cancellable`; `tests/query_boolean.rs` `a_sparse_bitmap_scan_is_charged_and_pollable` |
| a boolean filter beside a row-bound one | the bit test is evaluated BEFORE the row read, not by disabling the batched pass | `lang/tests/sql_explain.rs` `a_boolean_filter_is_answered_before_the_row_is_read` |
| a caller's `Ids` set naming a sequence past the collection's span | dropped, never `Corrupt` | `tests/query_boolean.rs` `an_out_of_span_id_is_dropped_not_corrupt` |
| `key BETWEEN`, `key >=` (external key) | Key filter, key-order driver | `tests/query_candidate_budget.rs` `a_key_range_pages_resume_in_key_order_with_deleted_keys_absent`; `tests/query_candidate_budget.rs` `a_key_prefix_is_expressed_as_a_range_and_resumes_the_same_way`; `tests/query_candidate_budget.rs` `a_key_filter_without_the_keys_driver_is_refused`; `tests/query_multimodel.rs` `keys_driver_order_matches_an_entity_enumeration_sorted_by_key` |

### §4.3 Graph (SQL/PGQ)

| Construct | Atomic | Tests |
| --- | --- | --- |
| element pattern `(v IS label)` / `(v:label)`, edge `-[e IS type]->`, `<-`, `-` | direction + type on BFS | `tests/graph_collections.rs` `cyclic_bfs_is_shortest_hop_deterministic_and_bounded`; `tests/graph_collections.rs` `directed_typed_context_edges_replace_properties_and_survive_reopen` (incoming and outgoing) |
| `{n,m}`, `{n,}`, `+`, `?` quantifiers | min/max depth | `tests/graph_collections.rs` `cyclic_bfs_is_shortest_hop_deterministic_and_bounded` |
| post-pattern `WHERE` | post-filter on rows | `tests/query_multimodel.rs` `graph_scalar_spatial_text_and_vector_apply_before_ranked_top_k`; `tests/query_multimodel.rs` `phrase_graph_and_spatial_intersect_before_top_k` |
| `COLUMNS (expr AS name)` | projection | `tests/query_combinations.rs` `hand_picked_surface_combinations` (`hp26_fields_after_alter`); `tests/query_page_walk.rs` `a_projected_scan_reuses_the_walked_row_and_decodes_it_once` |
| `IS ACYCLIC` (default) | contract 4.1; other path modes T3 | `tests/graph_collections.rs` `cyclic_bfs_is_shortest_hop_deterministic_and_bounded` (cycle `d→a` does not revisit `a`) |

### §4.4 Spatial

| Construct | Atomic | Tests |
| --- | --- | --- |
| `ST_DWithin(geog, geog, m)` | Point Radius / Geometry DWithin (spheroidal) | point: `tests/index_spatial.rs` `bbox_radius_nearest_two_fields_and_collections_match_oracles`; `tests/query_point_membership.rs` `a_text_driven_pages_non_driving_radius_reads_no_row`. geometry: `tests/query_multimodel.rs` `geometry_filter_matches_a_spatial_geometry_brute_force_oracle`; `tests/query_combinations.rs` `hand_picked_surface_combinations` (`hp12_geom_dwithin`); `tests/spatial_geometry_postgis.rs` `dwithin_matches_postgis_at_every_fixture_radius` |
| `ST_Intersects`, `ST_Within`, `ST_Contains` | Geometry filters (units per `docs/core/SPATIAL_FUNCTIONS.md`) | `tests/query_multimodel.rs` `geometry_filter_matches_a_spatial_geometry_brute_force_oracle`; `tests/query_combinations.rs` `hand_picked_surface_combinations` (`hp09_geom_and_point`, `hp10_geom_contains`, `hp11_geom_within`); `tests/spatial_postgis_conformance.rs` family walks; `tests/spatial_geometry_postgis.rs` `predicates_match_postgis_exactly` |
| `<->` (kNN, `ORDER BY loc <-> pt`) | Distance order (point index) | `tests/query_multimodel.rs` `distance_order_matches_query_point_nearest_across_page_sizes`; `tests/query_nearest_filtered.rs` `nearest_ten_under_an_equality_equals_the_brute_force_sort`; `tests/index_spatial.rs` `nearest_ring_walk_matches_exhaustive_for_random_points_and_edge_centres` |
| `ST_Distance`, `ST_Area`, `ST_Length`, `ST_Perimeter`, `ST_Centroid`, `ST_Covers`, `ST_Crosses` | T1 as row functions (`spatial_geometry` pub fns) | `tests/spatial_geometry_postgis.rs` `area_length_perimeter_match_postgis_within_tolerance`; `tests/spatial_geometry_postgis.rs` `pairwise_distance_matches_postgis_within_tolerance`; `tests/spatial_geometry_postgis.rs` `centroids_match_postgis_and_report_the_planar_approximation_gap`; `tests/spatial_geometry_postgis.rs` `predicates_match_postgis_exactly`; `tests/spatial_postgis_conformance.rs` `family_h_area_length_perimeter_centroid` |

### §4.5 Vector

| Construct | Atomic | Tests |
| --- | --- | --- |
| `VECTOR(n)` type, `'[...]'::vector` literal | `Kind::Vector` | `tests/collections.rs` `immutable_layout_dispatch_and_vector_sidecars_survive_updates`; `tests/index_vector.rs` `tiny_workload_matches_independent_metrics_and_filters_before_top_k` |
| `<=>` cosine, `<->` L2, `<#>` negative inner product | ExactVector / ApproximateVector order (Cosine, SquaredL2, NegativeDot) | `tests/index_vector.rs` `tiny_workload_matches_independent_metrics_and_filters_before_top_k`; `tests/query_combinations.rs` `hand_picked_surface_combinations` (`hp16_vec_cos_kind`, `hp18_ann_l2`, `hp19_ann_ndot`); `tests/query_score.rs` `score_vector_leaf_equals_exact_vector_order` |
| `ORDER BY emb <=> $v LIMIT k` | exact (page-order scan) or quantized (`ef`) by index choice | `tests/vector_scan_order.rs` `page_order_scan_matches_per_entity_path_on_random_corpus`; `tests/vector_scan_paging.rs` `pages_concatenate_into_the_single_page_answer`; `tests/vector_scan_paging.rs` `approximate_pages_stop_at_ef`; `tests/index_vector_quantized.rs` `approximate_shortlist_then_exact_rerank_matches_independent_oracle`; `tests/query_combinations.rs` `hand_picked_approx_vector_equals_exact` |
| `USING exact (emb)`, `USING quantized (emb vector_cosine_ops)` | index families | `tests/index_vector.rs` `immutable_layout_ordinals_live_crud_snapshots_rollback_and_reopen`; `tests/index_vector_quantized.rs` `validation_live_crud_snapshot_reopen_and_drop_preserve_authoritative_vectors`; `tests/vector_admission.rs` `one_intact_unknown_vector_family_or_version_refuses_before_mutation` |

### §4.1 String functions and §4.2 date/time functions

| Construct | Atomic | Tests |
| --- | --- | --- |
| `EXTRACT(YEAR FROM t) <cmp> n`, `date_trunc('unit', t) <cmp>\|BETWEEN lit`, `t::date <cmp> lit`, `t <cmp> lit`, `t BETWEEN`, `t <cmp> now() \| current_date +- interval` in a WHERE | ONE `ScalarFilter::Range` on the column's own scalar index, folded at prepare (`lang/src/functions.rs`, `Compiler::time_filter` in `lang/src/compile.rs`); the declared TIMESTAMPTZ/DATE is recorded in the collection descriptor (`CollectionInfo::declared`, `src/collections/mod.rs`, catalog flag bit 2) | `lang/tests/sql_functions.rs` `extract_year_equality_is_one_range_and_equals_the_brute_force_filter`; `lang/tests/sql_functions.rs` `extract_year_ordering_and_between_are_one_range_each`; `lang/tests/sql_functions.rs` `date_trunc_equality_and_between_equal_the_brute_force_filter`; `lang/tests/sql_functions.rs` `a_bare_comparison_against_a_date_literal_is_a_range`; `lang/tests/sql_functions.rs` `a_cast_to_date_is_one_days_range`; `lang/tests/sql_functions.rs` `a_clock_relative_predicate_is_folded_at_prepare_and_is_one_range`; `lang/tests/sql_functions.rs` `the_declared_type_survives_a_reopen`; `lang/tests/sql_tier1.rs` `a_date_time_function_in_where_is_one_scalar_range`; EXPLAIN: `lang/tests/sql_explain.rs` `a_date_rewrite_is_printed_as_a_range_and_not_as_a_row_function` |
| `lower(col) = x`, `lower(col) LIKE 'x%'`, `starts_with(lower(col), 'x')` in a WHERE | `ScalarFilter::Eq` / a text-key prefix `Range` on an EXPRESSION scalar index over `lower(col)` (`IndexExpr::Lower`, descriptor version 3 behind `EXPRESSION_FEATURE`, `src/collections/catalog.rs`); without that index the form is refused, never scanned | `lang/tests/sql_functions.rs` `lower_equality_uses_the_expression_index_and_equals_the_filter`; `lang/tests/sql_functions.rs` `a_fold_without_its_expression_index_is_refused_rather_than_scanned`; `lang/tests/sql_tier1.rs` `a_string_function_in_where_is_a_text_key_range`; EXPLAIN: `lang/tests/sql_explain.rs` `a_fold_over_an_expression_index_names_the_index_it_rode` |
| `col LIKE 'x%'`, `starts_with(col, 'x')` in a WHERE | a text-key prefix `ScalarFilter::Range` `[prefix, successor)` on the column's own scalar index (`functions::prefix_successor`); any other `LIKE` pattern is refused and names the trigram family | `lang/tests/sql_functions.rs` `a_prefix_pattern_is_a_text_key_range_and_equals_the_filter`; `lang/tests/sql_functions.rs` `a_fold_without_its_expression_index_is_refused_rather_than_scanned`; `lang/tests/sql_tier1.rs` `a_string_function_in_where_is_a_text_key_range` |
| `lower`, `upper`, `length`, `concat`, `\|\|`, `substring`, `left`, `right`, `trim`, `split_part`, `replace`, `position`, `starts_with`, `EXTRACT`, `date_trunc`, `now()`, `current_date`, `interval` arithmetic, `age()`, `to_char`, `to_timestamp`, `to_date` in a SELECT list | ROW functions over the values the page already projected (`CompiledRow::eval` in `lang/src/compile.rs`, `lang/src/functions.rs`); cost proportional to the rows RETURNED, printed by `EXPLAIN` under `row functions` | `lang/tests/sql_functions.rs` `projected_row_functions_equal_rusts_own_computation`; `lang/tests/sql_functions.rs` `projected_date_functions_equal_rusts_own_computation`; `lang/tests/sql_functions.rs` `age_and_interval_arithmetic_are_microseconds_over_one_folded_clock`; `lang/tests/sql_tier1.rs` `a_string_function_in_a_select_list_is_a_row_function`; `lang/tests/sql_tier1.rs` `a_date_time_function_in_a_select_list_is_a_row_function`; EXPLAIN: `lang/tests/sql_explain.rs` `a_projected_function_is_printed_as_a_row_function_and_costs_returned_rows`, `a_statement_with_no_function_says_none_in_both_sections`, `the_projection_only_battery_case_puts_every_function_in_one_section` |
| an EXPRESSION index's value recomputed FROM THE ROW -- the verifier, a non-driving predicate, a rank key | the same `IndexExpr::apply` the write path uses (`scalar_build_key_into`, `src/collections/catalog.rs`), in `verify_expected`/`verify_actual` (`src/collections/verification.rs`) and in `indexed_value`/`persisted_scalar_key` (`src/query/filters.rs`) | `lang/tests/sql_functions.rs` `the_verifier_is_clean_over_a_lower_index_with_mixed_case_values`; `lang/tests/sql_functions.rs` `a_non_driving_expression_predicate_equals_the_brute_force_filter`; `tests/index_scalar.rs` `a_non_driving_expression_equality_over_the_entity_walk_equals_the_filter`, `a_non_driving_expression_equality_beside_a_range_equals_the_filter`, `an_overflowed_membership_set_over_an_expression_range_equals_the_filter`, `ordering_by_an_expression_index_ranks_by_the_expressions_value` |
| a declared TIMESTAMPTZ/DATE printed as ISO text on every path: `SELECT col`, `SELECT *`, `col::text`, a string function's argument, `concat`, `\|\|`, a `GROUP BY` key | one rule, `Compiler::iso_text` wrapping `CompiledRow::Iso` (`lang/src/compile.rs`), and `group_key_value` for the folded answer | `lang/tests/sql_functions.rs` `select_star_prints_a_declared_timestamp_as_iso_text`; `lang/tests/sql_functions.rs` `concat_ignores_a_missing_value_and_the_operator_propagates_it`; `lang/tests/sql_functions.rs` `a_group_by_on_a_declared_timestamp_reports_the_key_as_iso_text`; `lang/tests/sql_functions.rs` `projected_date_functions_equal_rusts_own_computation` |
| the declared-type catalog tail | additive `DECLARED_FEATURE = 0x800` in `SUPPORTED_LOGICAL_FEATURES`, set in the same transaction as the first declared pair, with the frozen flags byte (`CATALOG_DECLARED`) as the second line (`src/collections/mod.rs`) | `src/collections/mod.rs` `collections::tests::a_declared_type_file_is_unsupported_to_a_binary_that_predates_the_bit`; `collections::tests::a_declared_pair_follows_its_field_and_sets_the_feature_bit`; `collections::tests::supported_logical_feature_mask_is_the_only_definition` |
| a rewrite whose pre-image is a SET of ranges (`EXTRACT(MONTH\|DAY\|DOW\|HOUR FROM t) = n`, `<>` over any window), a calendar `interval`, an unnamed `to_char` template, `LIKE '%x%'` | no atomic in this slice; refused by name with the membership-union reason (`sql::MULTI_RANGE_REASON`) | `lang/tests/sql_functions.rs` `a_rewrite_that_would_need_a_union_is_refused_with_the_named_reason`; `lang/tests/sql_functions.rs` `a_calendar_interval_is_refused_because_it_folds_to_no_constant`; `lang/tests/sql_functions.rs` `an_unnamed_to_char_template_is_refused_at_prepare`; `lang/tests/sql_tier1.rs` `a_multi_range_rewrite_is_refused_and_a_missing_expression_index_too`; `lang/tests/sql_refusals.rs` `every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` |

### §4.6 Text search

| Construct | Atomic | Tests |
| --- | --- | --- |
| `to_tsvector('simple', col) @@ to_tsquery('simple', 'a \| b')` / `'a & b'` / `'"a b"'` | Text filter Any / All / Phrase (analyzer v1) | `tests/index_text.rs` `tiny_ranking_any_all_and_candidate_filter_match_independent_bm25`; `tests/index_text.rs` `phrase_is_ordered_contiguous_bounded_and_snapshot_authoritative`; `tests/query_multimodel.rs` `phrase_refines_all_term_candidates_before_rank_and_across_drivers`; `tests/query_candidate_budget.rs` `a_phrase_scans_the_row_without_a_descent_or_a_term_map` |
| `ORDER BY ts_rank_cd(...)` | Bm25 order (formula differs from Postgres; documented) | `tests/query_candidate_budget.rs` `bm25_scores_match_the_definition_after_the_constants_are_hoisted`; `tests/text_score_cost.rs` `scoring_a_term_that_matches_most_of_the_corpus_costs_one_pass_not_one_per_document`; `tests/query_score.rs` `score_bm25_leaf_equals_bm25_order` |
| `bm25(col, 'query')` as an expression | Score leaf | `tests/query_score.rs` `score_bm25_leaf_equals_bm25_order`; `tests/query_score.rs` `score_hybrid_matches_row_oracle_across_page_sizes`; `tests/query_combinations.rs` `hand_picked_surface_combinations` (`hp22_score_hybrid`) |

### §4.7 Aggregates

| Construct | Atomic | Tests |
| --- | --- | --- |
| `count(*)`, `count(col)`, `sum`, `min`, `max`, `avg`, `GROUP BY`, `HAVING`, `DISTINCT` | streaming when the group key is the driving index's own value, hashed otherwise, bounded by the `groups` budget (`src/query/aggregate.rs`, `Database::prepare_aggregate`) | `tests/query_aggregate.rs` `every_filter_and_grouping_equals_a_brute_force_fold`; `tests/query_aggregate.rs` `streaming_and_hashed_produce_identical_groups`; `tests/query_aggregate.rs` `the_groups_budget_bounds_the_hashed_shape_and_streaming_holds_one`; `tests/query_aggregate.rs` `pages_of_one_three_and_seven_concatenate`; `tests/query_aggregate.rs` `a_streaming_page_stops_and_resumes_at_a_group_boundary`; `tests/query_aggregate.rs` `having_filters_finished_groups_before_they_are_paged`; `tests/query_aggregate.rs` `distinct_is_a_group_with_no_accumulators`; `tests/query_aggregate.rs` `count_all_is_one_group_over_the_key_order_driver`; `tests/query_aggregate.rs` `cancellation_mid_fold_leaves_no_state`; `tests/query_aggregate.rs` `a_divided_group_key_streams_and_equals_the_fold`; `tests/query_aggregate.rs` `an_order_by_an_accumulator_sorts_the_finished_groups`; SQL: `lang/tests/sql_tier1.rs` `count_star_with_no_filter_is_one_row_over_the_key_order_driver`, `group_by_an_indexed_column_streams_and_matches_the_api`, `sum_min_max_avg_by_group_with_having`, `select_distinct_is_a_group_with_no_accumulators`, `group_by_with_a_radius_filter_hashes_and_agrees_with_the_filter_itself`, `the_divided_group_key_is_accepted_index_side_and_refused_otherwise`, `order_by_an_aggregate_alias_sorts_the_finished_groups`; EXPLAIN: `lang/tests/sql_explain.rs` `agg_count_all_is_streaming_over_the_key_order_driver_and_reads_no_row`, `agg_count_kind_streams_off_the_driving_posting`, `agg_sum_born_by_kind_names_the_row_its_accumulators_read`, `agg_distinct_kind_is_a_group_with_no_accumulators`, `agg_count_radius_by_kind_hashes_because_the_radius_drives`, `agg_born_decade_computes_its_expression_key_index_side` |
| `array_agg`, `string_agg`, `json_agg`, `percentile_cont`, window functions, `GROUPING SETS`, `CUBE`, `count(DISTINCT col)`, a composite `GROUP BY` key, `GROUP BY <expression>` with no index-side computation | no atomic in this item; refused by name | `tests/query_aggregate.rs` `what_has_no_atomic_is_refused_by_name`; `lang/tests/sql_tier1.rs` `a_folded_answer_refuses_what_it_cannot_report`; `lang/tests/sql_refusals.rs` `every_listed_keyword_is_refused_by_name_with_its_tier_and_reason` |

## Query language T2 added 2026-09-20 (`docs/lang/QL_CONTRACT.md`)

T2 by definition has no built atomic, so every row here is `UNPINNED`. The
Atomic column is what the test must pin once the row lands.

### §2 Statements

| Construct | Atomic to pin | Tests |
| --- | --- | --- |
| `SELECT ... FROM ALL` | `Collections` concatenation driver in catalog id order; resume `(collection id, inner cursor)`; `LIMIT` stops inside the collection it is reached in; a ranked `ORDER BY` over it is refused | UNPINNED |
| `UPDATE t SET ... WHERE <any predicate>` | driver walk feeding read-modify-put; `rows_written` budget refuses rather than truncates; the walk does not re-match its own writes; resume from the committed cursor | UNPINNED |
| `DELETE FROM t WHERE <any predicate>`, `DELETE FROM ALL` | the same walk feeding `delete`, with the graph contract 6.1 RESTRICT preflight per row | UNPINNED |
| `CREATE TABLE t (...) WITH (hash/range/fulltext/bm25/spatial)` | expansion to `create_collection` + one `CREATE INDEX` per field; hash/range → btree, fulltext/bm25 → gin, spatial → gist | UNPINNED |
| column `DEFAULT now()`, `DEFAULT uuid4()`, `DEFAULT uuid5(ns, name)` | per-field default in the descriptor under an additive feature bit; filled on the write path only when the INSERT names no value; old files without the bit open unchanged (L8) | UNPINNED |
| `GENERATED ALWAYS AS (expr) STORED` | compiled row expression over fields of the same row, evaluated before index maintenance so an index over the generated column is maintained | UNPINNED |
| `NOT NULL` on a column | descriptor flag tested at row assembly; MISSING and NULL both refuse and the error names the column; `ADD COLUMN ... NOT NULL` without DEFAULT on a non-empty collection is refused | UNPINNED |
| `ALTER TABLE` ADD / DROP / RENAME COLUMN / RENAME TO | `alter_collection` writes a new `Layout`, no row rewritten; ADD reads MISSING; DROP tombstones the slot so old rows decode unchanged; RENAME keeps `CollectionId`, edges and key mappings | UNPINNED |
| `ALTER TABLE ... ALTER COLUMN ... TYPE` | same-`Kind` change is the descriptor rewrite; a `Kind` change is refused with the rewrite named | UNPINNED |
| `REINDEX` | drop + sorted rebuild under the `IndexState` Building/Ready/Dropping machine, resumable across a reopen | UNPINNED |
| `COMPACT` | `Database::checkpoint`; reports *deferred* on `Ok(false)` while a reader slot is held and never waits | UNPINNED |
| `SHOW TABLES`, `SHOW <collection>`, `SHOW CREATE TABLE`, `SHOW INDEXES` | one fixed SELECT over a `db_*` catalog view each; the count and size columns are labelled scans by `EXPLAIN` | UNPINNED |
| `SHOW EDGES [FROM t] [TO t]` | the `(from, type, to)` triples of graph contract 2.5 from the interned edge-type records; per-triple counts labelled scans | UNPINNED |
| `SHOW STATUS`, `SHOW STORAGE` | `docs/dist/OPS_CONTRACT.md` §6 — pinned in the OPS section below | UNPINNED |
| `CREATE [MATERIALIZED\|SEARCH] VIEW`, `REFRESH MATERIALIZED VIEW` | stored body in the catalog; populate = the prepared query's bounded pages through `put`; REFRESH = bounded resumable clear (`begin_drop_collection`/`drop_collection_step`) then populate; the view is stale between refreshes and never incrementally maintained | UNPINNED |
| `EXPLAIN ANALYZE <statement>` | the plan plus each page's `QueryWork`, run under the caller's `QueryBudget`; logical counters, one total wall clock | UNPINNED |
| bounded prepared-plan cache behind `sql_prepare` | LRU with three ceilings fixed at open; the key carries the catalog generation, so DDL invalidates plans instead of serving one against a dead layout | UNPINNED |

### §4.1 Row expressions

| Construct | Atomic to pin | Tests |
| --- | --- | --- |
| `CASE WHEN ... THEN ... [ELSE ...] END` | row expression; one key in `ORDER BY`; row-bound in `WHERE` and labelled so by `EXPLAIN` | UNPINNED |
| `->`, `->>`, `#>`, `#>>`, `json_array_length` on a `Kind::Json` field | row functions over the decoded binary JSON; no index range without an expression index | UNPINNED |

### §4.4 Spatial

| Construct | Atomic to pin | Tests |
| --- | --- | --- |
| `POINT(lon lat)`, `POLYGON((...))` as a literal | the `ST_GeomFromText` WKT parser reached without the function name; longitude before latitude | UNPINNED |

### §4.5 Vector

| Construct | Atomic to pin | Tests |
| --- | --- | --- |
| `USING vamana` | alias of `quantized` with a notice, beside `hnsw`/`diskann`/`ivfflat`; no new family | UNPINNED |

### §4.6 Text search

| Construct | Atomic to pin | Tests |
| --- | --- | --- |
| `search_score()` | Score leaf of `search()`, in [0,1] from the edit distance spent and the prefix completed; lands with `search()` | UNPINNED |
| `bm25_norm(col, 'query', k)` | `bm25/(bm25+k)` on the existing Score leaf; one operation, no extra pass, strictly monotone so the `bm25` order is unchanged | UNPINNED |

## Operations contract (`docs/dist/OPS_CONTRACT.md`)

Every row is T2 and `UNPINNED`. The Law column is the one the test must
falsify, not decorate.

| § | Surface | Atomic to pin | Law | Tests |
| --- | --- | --- | --- | --- |
| 1 | Service mode | one writer behind a mutex, one `Arc<Database>` snapshot behind an RwLock; no read takes the writer lock; a reader slot defers a checkpoint and never blocks the writer; past the persisted `readers` bound the service refuses rather than blocks | L6 | UNPINNED |
| 2 | `publish()` and the staleness window | a publish is a snapshot open and an `Arc` swap, with no checkpoint on the path; a read is stale by at most the interval plus one open; a failed mint leaves the served view in place | L6, L3 | UNPINNED |
| 3 | Statement timeout | a deadline in `WorkMeter`, polled on a counted interval of charges; `QueryBudget` remains the bound the contract guarantees; a timeout never interrupts a commit | L1, L4 | UNPINNED |
| 4 | Interrupt handle | a public `Arc<AtomicBool>` wired in as the default cancellation closure; per handle and therefore per snapshot; a cancel is sticky until cleared; a cancelled query errors and never returns a partial answer as complete | L6 | UNPINNED |
| 5 | Change notifications | exactly one event per committed batch, delivered inside `commit` after the barrier; a rolled-back transaction delivers none; collections and edge types in full; the key list capped, degrading to a truncation flag and a count | L6, L1 | UNPINNED |
| 6.1 | `SHOW STATUS` | a `db_status` view over `storage_bytes`, `tracked_pages`, `io_counters` and the format bits, all O(1); node and edge counts are optional columns labelled scans | L4 | UNPINNED |
| 6.2 | `SHOW STORAGE` | a tag-attributing walk over the keyspaces plus O(1) file sizes; reports itself as a scan | L4 | UNPINNED |
| 6.3 | `stats`, `memory_report`, `trim_memory` | a report over the structures that exist — pool arena as a labelled ceiling, index and layout caches, reader slots — never a `0` for an absent structure; `trim_memory` changes no answer | L4, evidencing L1 | UNPINNED |
| 7 | Bulk load | a nesting-counted write scope whose outermost close calls `commit`, with the same FULL barrier as any other commit; a failed batch commits nothing | L2, L3 | UNPINNED |
| 8 | `write_trace` | a `cfg(feature)` thread-local phase timer, one line per index family; zero cost when off | L4 | UNPINNED |
| 9.1 | `statement_timeout` GUC | `SET [LOCAL] statement_timeout` on the existing `SET LOCAL` dispatch; `0` is no limit; `SHOW` returns it | L4 | UNPINNED |
| 9.2 | `CancelRequest` | the protocol's backend-id/secret pair selects the handle of §4; both timeout and cancel return Postgres `57014 query_canceled`, distinguished by message, not code | L6 | UNPINNED |
| 9.3 | `LISTEN` / `NOTIFY` | one notification per committed batch per listening channel, delivered at end of transaction, none on rollback, duplicates collapsed, payload within the 8,000-byte cap that §5's key bound already satisfies | L6, L1 | UNPINNED |

## Counts

| Set | Rows | Fully pinned | Contain UNPINNED |
| --- | ---: | ---: | ---: |
| GRAPH_CONTRACT numbered rules (1.1–6.3 and L1–L8; no L7 in that document) | 31 | 15 | 16 |
| QL_CONTRACT T1 table rows | 29 | 25 | 4 |
| **Total** | **60** | **40** | **20** |

Unpinned count (rows whose Tests cell contains `UNPINNED`): **20**. The count
did not move with `DROP TABLE`: the two rows it touches (6.1 and the
| QL_CONTRACT T1 table rows | 28 | 24 | 4 |
| **Subtotal (pinnable at HEAD)** | **59** | **39** | **20** |
| QL_CONTRACT T2 rows added 2026-09-20 | 23 | 0 | 23 |
| OPS_CONTRACT rows (§1–9, all T2) | 13 | 0 | 13 |
| **Total** | **95** | **39** | **56** |

Unpinned count over the pinnable set (rows whose Tests cell contains
`UNPINNED`): **20**, unchanged. The 36 rows added on 2026-09-20 are all T2
and all unpinned, which is what T2 means -- they are counted apart so that
this number keeps measuring what is built but untested, rather than mixing it
with what is specified and unbuilt. Total unpinned across every set: **56**.
The pinnable count did not move with `DROP TABLE`: the two rows it touches (6.1 and the
`CREATE TABLE ... DROP` row) were partial before and are partial still, for
the constructs that remain -- single-row RESTRICT, and `adjacency` as a
`CREATE INDEX` method.

GRAPH fully unpinned (10): 1.3, 2.3, 4.2, 4.3, 4.4, 5.1, 5.2, 5.3, 6.3, L4.
GRAPH partial (6): 2.4, 3.1, 3.4, 6.1, L3, L8. L3 moved from fully unpinned to
partial with `DROP TABLE`: RESTRICT is now the default of a real delete path
and CASCADE its explicit, bounded opposite, so what is left unpinned in that
row is only the single-row `delete(key)`, which still cascades.
T1 partial (2): `DELETE` RESTRICT; `CREATE INDEX ... adjacency` (the
`DROP TABLE` half of that row is now pinned by six tests). `<>` and
`IS NOT NULL` moved off this list with the boolean atomics
(`tests/query_boolean.rs`), which also added seven T1 rows to §3. No T1 row
is fully unpinned.
| GRAPH_CONTRACT numbered rules (1.1–6.3 and L1–L8; no L7 in that document) | 31 | 18 | 13 |
| QL_CONTRACT T1 table rows | 39 | 37 | 2 |
| **Total** | **70** | **55** | **15** |

Unpinned count (rows whose Tests cell contains `UNPINNED`): **15**.

GRAPH fully unpinned (8): 1.3, 2.3, 5.1, 5.2, 5.3, 6.3, L3, L4.
GRAPH partial (5): 2.4, 3.1, 3.4, 6.1, L8.
T1 partial (2): `DELETE` RESTRICT; `CREATE INDEX ... adjacency`. No T1 row is
fully unpinned. The seven §3 boolean rows added with the membership-set
algebra are pinned by `tests/query_boolean.rs`, `lang/tests/sql_tier1.rs` and
`lang/tests/sql_explain.rs`.

The four rows the T1 table gained with GRAPH_CONTRACT 4.2 and 4.3 (the two
inline element `WHERE` forms that are now T1, `COLUMNS (r.<prop>)` and
`ORDER BY <edge alias>`) are pinned by `lang/tests/sql_tier1.rs`'s four
`graph_table_*` tests and `lang/tests/sql_explain.rs`'s
`explain_prints_the_edge_predicates_and_the_node_membership_sets`.
