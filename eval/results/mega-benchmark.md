# Mega benchmark — history

Compact results of `cargo bench --bench mega_benchmark`: **sekejap vs in-memory
SQLite** across 20 scenarios (filtering, sort, graph, spatial, vector, hybrid) on
20k venues + a graph + 64-dim embeddings.

This log is **run on demand, not per release** — the benchmark is heavy, so we
capture a snapshot when it's worth comparing (e.g. before/after a feature like
snapshot reads). Each entry is dated and tied to the commit it ran at, so a
feature's impact is visible across entries. Newest first.

To capture a run:
```bash
cargo bench --bench mega_benchmark        # run it
scripts/mega-bench-capture.sh --prepend   # prepend a compact entry here, then commit
```

`sekejap` column = the faster of the SQL / atomic surfaces. `sqlite` runs
in-memory with all applicable indexes + R*Tree (so several rows are disk-vs-RAM,
not apples-to-apples — noted where it matters). `vs sqlite` > 1 = sekejap faster.

<!-- entries -->

## 2026-08-13 — `6e1948c` (bench: reliable mega-bench (SyncMode::Normal setup) + commit)

mode: resident (`CoreDB::open`) · vs in-memory SQLite · 20k venues + graph/spatial/vector

| scenario | sekejap | sqlite | vs sqlite |
|---|---|---|---|
| 01_eq_filter | 596ns | 462.6µs | 775.8x |
| 02_neq_filter | 275.6µs | 1.34ms | 4.9x |
| 03_range_filter | 10.7µs | 2.42ms | 225.9x |
| 04_sort_limit | 10.0µs | 11.2µs | 1.1x |
| 05_point_lookup | 70ns | 784ns | 11.2x |
| 06_compound_filter | 56.2µs | 475.5µs | 8.5x |
| 07_compound_sort_limit | 56.7µs | 520.6µs | 9.2x |
| 08_graph_1hop | 297ns | 936ns | 3.2x |
| 09_graph_5hop_bfs | 5.6µs | 224.9µs | 40.0x |
| 10_root_cause_bfs_leaves | 1.0µs | 9.1µs | 8.9x |
| 11_shortest_path | 5.4µs | 6.0µs | 1.1x |
| 12_st_dwithin_5km | 678.3µs | 1.27ms | 1.9x |
| 13_st_within_polygon | 14.4µs | — | sekejap-only |
| 14_spatial_category_filter | 60.5µs | 523.7µs | 8.7x |
| 15_vector_hnsw_top20 | 104.8µs | — | sekejap-only |
| 16_hybrid_spatial_vector | 1.09ms | 1.74ms | 1.6x |
| 17_hybrid_spatial_graph | 882.1µs | 2.09ms | 2.4x |
| 18_hybrid_graph_vector | 335.2µs | 2.65ms | 7.9x |
| 19_hybrid_ilike_vector_rag | 9.14ms | 5.03ms | 1.8x SLOWER |
| 20_holy_trinity_spatial_graph_vector | 402.4µs | 1.12ms | 2.8x |

**head-to-head: 17 wins / 1 loss** (+ sekejap-only cases)


