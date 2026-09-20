# Contract test map

Every numbered rule in `docs/GRAPH_CONTRACT.md` and every T1 row in
`docs/QL_CONTRACT.md`, mapped to the test file and test name that pins it at
HEAD, or `UNPINNED`. Grepped from `tests/`. T1 is defined as compiling to an
atomic that exists; the tests named here pin that atomic (there is no SQL
parser at HEAD). A row that names several constructs is `UNPINNED` for any
construct the tests do not pin.

## Graph contract (`docs/GRAPH_CONTRACT.md`)

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
| 4.2 | Traversal binds the reaching edge to each result and can read its properties | UNPINNED (`TraversalNode` is `{entity, depth}` only) |
| 4.3 | Per-hop predicates on edge properties or node fields; failing edge never followed; covered fields never read a row | UNPINNED (`BfsRequest` has no predicate) |
| 4.4 | One predicate set applies to every hop; per-hop patterns are Phase 3 | UNPINNED (depends on 4.3) |
| 4.5 | Traversal is a query filter and a candidate driver; conjoins with scalar/text/point/geometry and any single order; seeds may come from another order | `tests/graph_collections.rs` `graph_filter_answers_what_traverse_bfs_answers`; `tests/query_multimodel.rs` `graph_scalar_spatial_text_and_vector_apply_before_ranked_top_k`; `tests/query_combinations.rs` `query_engine_surface_combinations` |
| 5.1 | Paths streamed: one accumulator per frontier entry; full path rebuilt only for returned rows | UNPINNED |
| 5.2 | A path aggregate is a Score leaf | UNPINNED |
| 5.3 | Shortest path atomic: ANY SHORTEST and ALL SHORTEST, bidirectional, unweighted first | UNPINNED (`cyclic_bfs_is_shortest_hop_deterministic_and_bounded` pins BFS hop order, not this atomic) |
| 6.1 | RESTRICT (default) refuses node delete while any edge in any context references it; CASCADE is explicit and bounded | CASCADE: `tests/graph_collections.rs` `entity_delete_cascades_both_directions_all_contexts_and_self_edges`; `tests/graph_collections.rs` `entity_delete_refuses_degree_257_before_any_published_change`. RESTRICT default, at collection granularity: `tests/drop_collection.rs` `restrict_names_the_referencing_contexts_and_cascade_removes_the_edges`; `tests/sql_tier1.rs` `drop_table_restricts_on_graph_edges_and_cascades_when_asked`. RESTRICT on a SINGLE `delete(key)`: UNPINNED (that path still cascades) |
| 6.2 | Deleting an edge removes its forward and reverse postings in its context | `tests/graph_collections.rs` `snapshots_unlink_missing_endpoints_and_delete_cascade_are_transactional`; `tests/graph_collections.rs` `directed_typed_context_edges_replace_properties_and_survive_reopen` |
| 6.3 | Dropping a context removes its range; nodes are untouched | UNPINNED |
| L1 | Frontier, visited set and one accumulator per visited node, all bounded by the visited budget; never RAM ∝ graph | `tests/graph_collections.rs` `bfs_allocates_a_constant_plus_its_result_not_per_edge`; `tests/graph_collections.rs` `graph_filtered_query_allocates_like_the_traversal_it_runs` |
| L2 | Work per hop is the postings of that source in that context and type | `tests/graph_collections.rs` `traversal_reads_pay_for_leaf_pages_not_for_edges`; `tests/edge_write_budget.rs` `the_cost_of_an_edge_does_not_grow_with_the_rows_loaded_before_it` |
| L3 | RESTRICT is the default delete; CASCADE is explicit and bounded | `tests/drop_collection.rs` `restrict_names_the_referencing_contexts_and_cascade_removes_the_edges` (`begin_drop_collection` is RESTRICT, `begin_drop_collection_mode(.., Cascade)` is the opt-in); `tests/drop_collection.rs` `the_drop_removes_exactly_the_collections_records_and_nothing_else` (the cascade runs inside `budget`-bounded steps). Single-row `delete(key)`: UNPINNED (still cascades) |
| L4 | Sacrifices named: 8 bytes/edge identity; declared property fixed-width; reverse mirror doubles storage | UNPINNED (identity and declared properties are not on disk; no test measures the reverse-mirror byte cost) |
| L5 | A corrupt posting or bag is `Corrupt` on that edge, never a panic; offset reads bounds-checked | `tests/graph_admission.rs` `edge_damage_fails_the_read_that_needs_it_and_the_verifier_reports_the_rest`; `tests/graph_admission.rs` `damaged_graph_metadata_replica_falls_back_without_rewriting_source` |
| L6 | A traversal reads its snapshot; a concurrent writer is invisible to it | `tests/graph_collections.rs` `snapshots_unlink_missing_endpoints_and_delete_cascade_are_transactional`; `tests/query_multimodel.rs` `one_snapshot_stays_old_while_committed_multifamily_mutation_reopens_new` |
| L8 | Element identity and typed properties are additive feature bits; old files open unchanged | GRAPH_FEATURE: `tests/graph_admission.rs` `graph_rows_cannot_be_hidden_by_clearing_feature_and_metadata`; `tests/graph_admission.rs` `intact_future_graph_header_or_name_replica_refuses_before_source_mutation`. identity and typed-property bits: UNPINNED |

## Query language T1 (`docs/QL_CONTRACT.md`)

### §2 Statements

| Construct | Atomic | Tests |
| --- | --- | --- |
| `SELECT ... FROM <collection> [WHERE] [ORDER BY one expr] [LIMIT]` | `prepare_query`: filters AND, one order, pages | `tests/query_scalar.rs` `scalar_json_filters_keep_exact_numbers_null_missing_and_page_order`; `tests/query_page_walk.rs` `a_limit_stops_the_candidate_walk_when_the_driver_is_already_in_rank_order`; `tests/query_combinations.rs` `query_engine_surface_combinations` |
| `SELECT ... FROM GRAPH_TABLE (...)` | traversal driver/filter | `tests/graph_collections.rs` `graph_filter_answers_what_traverse_bfs_answers`; `tests/query_multimodel.rs` `graph_scalar_spatial_text_and_vector_apply_before_ranked_top_k` |
| `INSERT INTO t (...) VALUES (...)`, `$n` params | `put` by key | `tests/collections.rs` `identity_upsert_delete_and_collection_isolation_follow_e3_contracts`; `tests/collection_pagewal.rs` `mixed_records_roundtrip_through_page_wal_commit_and_reopen` |
| `UPDATE t SET ... WHERE key = $1` | `put` replaces; partial update = read-modify-put | `tests/collections.rs` `identity_upsert_delete_and_collection_isolation_follow_e3_contracts` |
| `DELETE FROM t WHERE key = $1` | delete by key; RESTRICT/CASCADE per graph 6.1 | delete by key: `tests/collections.rs` `identity_upsert_delete_and_collection_isolation_follow_e3_contracts`; `tests/collection_pagewal.rs` `delete_then_reinsert_allocates_a_fresh_identity_and_never_reuses_after_reopen`. RESTRICT: UNPINNED (see graph 6.1) |
| `CREATE TABLE`, `CREATE INDEX ... USING {btree,gin,gist,exact,quantized,adjacency}`, `DROP TABLE [IF EXISTS] [CASCADE\|RESTRICT]`, `DROP INDEX` | catalog descriptors; `begin_drop_collection` / `drop_collection_step` | `DROP TABLE`: `tests/drop_collection.rs` `a_dropped_collection_leaves_every_keyspace_empty_and_its_name_free`, `an_interrupted_drop_resumes_to_the_state_an_uninterrupted_one_reaches`, `restrict_names_the_referencing_contexts_and_cascade_removes_the_edges`, `a_published_dropping_mark_refuses_every_reader_and_writer`, `sql_drop_table_if_exists_and_cascade_end_to_end`, `the_drop_removes_exactly_the_collections_records_and_nothing_else`; `tests/sql_tier1.rs` `drop_table_restricts_on_graph_edges_and_cascades_when_asked`; `tests/sql_explain.rs` `explain_drop_table_prints_the_phases_and_runs_nothing`. table: `tests/collections.rs` `undeclared_fields_roundtrip_through_collection_and_reopen`. btree: `tests/index_lifecycle.rs` `late_index_tracks_crud_and_published_snapshots`. gin: `tests/index_text.rs` `building_live_crud_snapshot_rollback_and_reopen_preserve_presence_rules`. gist (point): `tests/index_spatial.rs` `build_live_crud_snapshot_rollback_drop_and_reopen`. gist (geometry): `tests/index_spatial_geometry.rs` `build_vs_maintain_posting_identity_across_geometry_kinds`. exact: `tests/index_vector.rs` `immutable_layout_ordinals_live_crud_snapshots_rollback_and_reopen`. quantized: `tests/index_vector_quantized.rs` `validation_live_crud_snapshot_reopen_and_drop_preserve_authoritative_vectors`. DROP: `tests/index_lifecycle.rs` `drop_and_build_can_resume_after_reopen_without_claiming_ready`. adjacency as `CREATE INDEX`: UNPINNED (graph is `enable_graph` + `put_edge`, not an `IndexFamily`) |
| `BEGIN [READ ONLY]`, `COMMIT`, `ROLLBACK` | one writer, snapshot readers | `tests/collections.rs` `published_snapshots_rollback_and_streaming_cursor_are_consistent`; `tests/collection_pagewal.rs` `rollback_discards_uncommitted_work_beside_a_live_snapshot`; `tests/collection_pagewal.rs` `snapshots_serve_published_state_defer_checkpoint_and_are_bounded`; `tests/pagewal.rs` `new_snapshot_during_uncommitted_changes_sees_latest_commit` |

### §3 Predicates

| Construct | Atomic | Tests |
| --- | --- | --- |
| `AND` | filter conjunction | `tests/query_multimodel.rs` `graph_scalar_spatial_text_and_vector_apply_before_ranked_top_k`; `tests/query_combinations.rs` `query_engine_surface_combinations`; `tests/query_scalar_fold.rs` `folding_survives_a_third_predicate_and_a_second_index` |
| `=, <>, <, <=, >, >=` on indexed scalar | Scalar Eq/Range | `=`: `tests/query_scalar.rs` `scalar_json_filters_keep_exact_numbers_null_missing_and_page_order`. range inequalities: `tests/query_scalar_fold.rs` `folding_two_predicates_on_one_index_keeps_the_answer`; `tests/query_combinations.rs` `hand_picked_surface_combinations` (`hp07_same_index_born_fold`). `<>`: UNPINNED (no `NotEq`; `NOT` is T2) |
| `BETWEEN a AND b` | Range | `tests/query_scalar_fold.rs` `folding_two_predicates_on_one_index_keeps_the_answer`; `tests/query_candidate_budget.rs` `a_spatial_driven_pages_non_driving_range_reads_no_row` |
| `IS NULL`, `IS NOT NULL`, `IS MISSING` | Scalar IsNull/IsMissing | `IS NULL` / `IS MISSING`: `tests/query_combinations.rs` `hand_picked_surface_combinations` (`hp04_ismissing_score`, `hp05_isnull_kind`); `tests/query_scalar.rs` `scalar_json_filters_keep_exact_numbers_null_missing_and_page_order`. `IS NOT NULL`: UNPINNED |
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
| `ST_Intersects`, `ST_Within`, `ST_Contains` | Geometry filters (units per `docs/SPATIAL_FUNCTIONS.md`) | `tests/query_multimodel.rs` `geometry_filter_matches_a_spatial_geometry_brute_force_oracle`; `tests/query_combinations.rs` `hand_picked_surface_combinations` (`hp09_geom_and_point`, `hp10_geom_contains`, `hp11_geom_within`); `tests/spatial_postgis_conformance.rs` family walks; `tests/spatial_geometry_postgis.rs` `predicates_match_postgis_exactly` |
| `<->` (kNN, `ORDER BY loc <-> pt`) | Distance order (point index) | `tests/query_multimodel.rs` `distance_order_matches_query_point_nearest_across_page_sizes`; `tests/query_nearest_filtered.rs` `nearest_ten_under_an_equality_equals_the_brute_force_sort`; `tests/index_spatial.rs` `nearest_ring_walk_matches_exhaustive_for_random_points_and_edge_centres` |
| `ST_Distance`, `ST_Area`, `ST_Length`, `ST_Perimeter`, `ST_Centroid`, `ST_Covers`, `ST_Crosses` | T1 as row functions (`spatial_geometry` pub fns) | `tests/spatial_geometry_postgis.rs` `area_length_perimeter_match_postgis_within_tolerance`; `tests/spatial_geometry_postgis.rs` `pairwise_distance_matches_postgis_within_tolerance`; `tests/spatial_geometry_postgis.rs` `centroids_match_postgis_and_report_the_planar_approximation_gap`; `tests/spatial_geometry_postgis.rs` `predicates_match_postgis_exactly`; `tests/spatial_postgis_conformance.rs` `family_h_area_length_perimeter_centroid` |

### §4.5 Vector

| Construct | Atomic | Tests |
| --- | --- | --- |
| `VECTOR(n)` type, `'[...]'::vector` literal | `Kind::Vector` | `tests/collections.rs` `immutable_layout_dispatch_and_vector_sidecars_survive_updates`; `tests/index_vector.rs` `tiny_workload_matches_independent_metrics_and_filters_before_top_k` |
| `<=>` cosine, `<->` L2, `<#>` negative inner product | ExactVector / ApproximateVector order (Cosine, SquaredL2, NegativeDot) | `tests/index_vector.rs` `tiny_workload_matches_independent_metrics_and_filters_before_top_k`; `tests/query_combinations.rs` `hand_picked_surface_combinations` (`hp16_vec_cos_kind`, `hp18_ann_l2`, `hp19_ann_ndot`); `tests/query_score.rs` `score_vector_leaf_equals_exact_vector_order` |
| `ORDER BY emb <=> $v LIMIT k` | exact (page-order scan) or quantized (`ef`) by index choice | `tests/vector_scan_order.rs` `page_order_scan_matches_per_entity_path_on_random_corpus`; `tests/vector_scan_paging.rs` `pages_concatenate_into_the_single_page_answer`; `tests/vector_scan_paging.rs` `approximate_pages_stop_at_ef`; `tests/index_vector_quantized.rs` `approximate_shortlist_then_exact_rerank_matches_independent_oracle`; `tests/query_combinations.rs` `hand_picked_approx_vector_equals_exact` |
| `USING exact (emb)`, `USING quantized (emb vector_cosine_ops)` | index families | `tests/index_vector.rs` `immutable_layout_ordinals_live_crud_snapshots_rollback_and_reopen`; `tests/index_vector_quantized.rs` `validation_live_crud_snapshot_reopen_and_drop_preserve_authoritative_vectors`; `tests/vector_admission.rs` `one_intact_unknown_vector_family_or_version_refuses_before_mutation` |

### §4.6 Text search

| Construct | Atomic | Tests |
| --- | --- | --- |
| `to_tsvector('simple', col) @@ to_tsquery('simple', 'a \| b')` / `'a & b'` / `'"a b"'` | Text filter Any / All / Phrase (analyzer v1) | `tests/index_text.rs` `tiny_ranking_any_all_and_candidate_filter_match_independent_bm25`; `tests/index_text.rs` `phrase_is_ordered_contiguous_bounded_and_snapshot_authoritative`; `tests/query_multimodel.rs` `phrase_refines_all_term_candidates_before_rank_and_across_drivers`; `tests/query_candidate_budget.rs` `a_phrase_scans_the_row_without_a_descent_or_a_term_map` |
| `ORDER BY ts_rank_cd(...)` | Bm25 order (formula differs from Postgres; documented) | `tests/query_candidate_budget.rs` `bm25_scores_match_the_definition_after_the_constants_are_hoisted`; `tests/text_score_cost.rs` `scoring_a_term_that_matches_most_of_the_corpus_costs_one_pass_not_one_per_document`; `tests/query_score.rs` `score_bm25_leaf_equals_bm25_order` |
| `bm25(col, 'query')` as an expression | Score leaf | `tests/query_score.rs` `score_bm25_leaf_equals_bm25_order`; `tests/query_score.rs` `score_hybrid_matches_row_oracle_across_page_sizes`; `tests/query_combinations.rs` `hand_picked_surface_combinations` (`hp22_score_hybrid`) |

## Counts

| Set | Rows | Fully pinned | Contain UNPINNED |
| --- | ---: | ---: | ---: |
| GRAPH_CONTRACT numbered rules (1.1–6.3 and L1–L8; no L7 in that document) | 31 | 15 | 16 |
| QL_CONTRACT T1 table rows | 28 | 24 | 4 |
| **Total** | **59** | **39** | **20** |

Unpinned count (rows whose Tests cell contains `UNPINNED`): **20**. The count
did not move with `DROP TABLE`: the two rows it touches (6.1 and the
`CREATE TABLE ... DROP` row) were partial before and are partial still, for
the constructs that remain -- single-row RESTRICT, and `adjacency` as a
`CREATE INDEX` method.

GRAPH fully unpinned (10): 1.3, 2.3, 4.2, 4.3, 4.4, 5.1, 5.2, 5.3, 6.3, L4.
GRAPH partial (6): 2.4, 3.1, 3.4, 6.1, L3, L8. L3 moved from fully unpinned to
partial with `DROP TABLE`: RESTRICT is now the default of a real delete path
and CASCADE its explicit, bounded opposite, so what is left unpinned in that
row is only the single-row `delete(key)`, which still cascades.
T1 partial (4): `DELETE` RESTRICT; `CREATE INDEX ... adjacency` (the
`DROP TABLE` half of that row is now pinned by six tests); `<>`;
`IS NOT NULL`. No T1 row is fully unpinned.
