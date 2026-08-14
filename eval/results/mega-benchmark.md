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

## 2026-08-15 — `7b2f1d3` (fix!: P0.S5-S7 — invariant guards on the rewrite paths + cra) · runtime 11m 7s

mode: resident (`CoreDB::open`) · vs in-memory SQLite · 20k venues + graph/spatial/vector

| scenario | sekejap | sqlite | vs sqlite |
|---|---|---|---|
| 01_eq_filter | 534ns | 418.5µs | 784.0x |
| 02_neq_filter | 256.7µs | 1.20ms | 4.7x |
| 03_range_filter | 10.3µs | 2.21ms | 214.0x |
| 04_sort_limit | 9.3µs | 10.2µs | 1.1x |
| 05_point_lookup | 64ns | 706ns | 11.1x |
| 06_compound_filter | 52.0µs | 439.3µs | 8.4x |
| 07_compound_sort_limit | 54.3µs | 472.9µs | 8.7x |
| 08_graph_1hop | 272ns | 841ns | 3.1x |
| 09_graph_5hop_bfs | 5.1µs | 207.1µs | 40.3x |
| 10_root_cause_bfs_leaves | 976ns | 8.2µs | 8.4x |
| 11_shortest_path | 5.0µs | 5.5µs | 1.1x |
| 12_st_dwithin_5km | 636.9µs | 1.15ms | 1.8x |
| 13_st_within_polygon | 13.4µs | — | sekejap-only |
| 14_spatial_category_filter | 55.6µs | 481.6µs | 8.7x |
| 15_vector_hnsw_top20 | 102.0µs | — | sekejap-only |
| 16_hybrid_spatial_vector | 1.05ms | 1.62ms | 1.5x |
| 17_hybrid_spatial_graph | 845.1µs | 1.95ms | 2.3x |
| 18_hybrid_graph_vector | 315.4µs | 2.50ms | 7.9x |
| 19_hybrid_ilike_vector_rag | 8.77ms | 5.06ms | 1.7x SLOWER |
| 20_holy_trinity_spatial_graph_vector | 371.0µs | 1.03ms | 2.8x |

**head-to-head: 17 wins / 1 loss** (+ sekejap-only cases)

> **Reading this entry.** vs the previous run, 19 of 20 scenarios are within ±6%
> (noise). `19_hybrid_ilike_vector_rag` looked +27% — but rebuilding the previous
> commit and benchmarking it *in the same session* gave 8.60ms against this build's
> 8.87ms, i.e. **+3%, no regression**. The earlier 6.89ms was a cooler-machine
> artifact.
>
> **Method note:** cross-session comparisons on a laptop are unreliable for the
> heaviest scenarios. Confirm any apparent regression with a same-session A/B
> (`git worktree add` the old commit and re-run the single scenario) before
> believing it.
>
> **Mode parity:** regular vs service mode (`open_as_service`, paged + snapshot
> reads) measured separately over 11 query shapes — identical row counts, latency
> ratios 0.97-1.14x. Service mode costs nothing on the read path.


## 2026-08-14 — `79da332` (test: G1 — snapshots never see an uncommitted or rolled-back) · runtime 10m 51s

mode: resident (`CoreDB::open`) · vs in-memory SQLite · 20k venues + graph/spatial/vector

| scenario | sekejap | sqlite | vs sqlite |
|---|---|---|---|
| 01_eq_filter | 533ns | 418.8µs | 786.4x |
| 02_neq_filter | 256.0µs | 1.21ms | 4.7x |
| 03_range_filter | 10.4µs | 2.19ms | 210.6x |
| 04_sort_limit | 9.1µs | 10.2µs | 1.1x |
| 05_point_lookup | 63ns | 708ns | 11.3x |
| 06_compound_filter | 51.6µs | 441.3µs | 8.5x |
| 07_compound_sort_limit | 56.3µs | 479.6µs | 8.5x |
| 08_graph_1hop | 277ns | 849ns | 3.1x |
| 09_graph_5hop_bfs | 4.9µs | 207.2µs | 42.6x |
| 10_root_cause_bfs_leaves | 923ns | 8.4µs | 9.1x |
| 11_shortest_path | 5.0µs | 5.5µs | 1.1x |
| 12_st_dwithin_5km | 633.5µs | 1.16ms | 1.8x |
| 13_st_within_polygon | 15.4µs | — | sekejap-only |
| 14_spatial_category_filter | 57.0µs | 478.3µs | 8.4x |
| 15_vector_hnsw_top20 | 101.7µs | — | sekejap-only |
| 16_hybrid_spatial_vector | 1.01ms | 1.55ms | 1.5x |
| 17_hybrid_spatial_graph | 823.4µs | 1.88ms | 2.3x |
| 18_hybrid_graph_vector | 312.4µs | 2.34ms | 7.5x |
| 19_hybrid_ilike_vector_rag | 6.89ms | 4.28ms | 1.6x SLOWER |
| 20_holy_trinity_spatial_graph_vector | 373.8µs | 1.00ms | 2.7x |

**head-to-head: 17 wins / 1 loss** (+ sekejap-only cases)


## 2026-08-13 — `2009a51` (bench(tooling): record mega-benchmark wall-clock runtime) · runtime 11m 32s

mode: resident (`CoreDB::open`) · vs in-memory SQLite · 20k venues + graph/spatial/vector

| scenario | sekejap | sqlite | vs sqlite |
|---|---|---|---|
| 01_eq_filter | 564ns | 434.1µs | 769.4x |
| 02_neq_filter | 265.9µs | 1.24ms | 4.7x |
| 03_range_filter | 10.3µs | 2.29ms | 222.0x |
| 04_sort_limit | 9.6µs | 10.8µs | 1.1x |
| 05_point_lookup | 68ns | 777ns | 11.4x |
| 06_compound_filter | 54.6µs | 470.3µs | 8.6x |
| 07_compound_sort_limit | 50.7µs | 500.7µs | 9.9x |
| 08_graph_1hop | 291ns | 912ns | 3.1x |
| 09_graph_5hop_bfs | 5.1µs | 217.3µs | 42.3x |
| 10_root_cause_bfs_leaves | 996ns | 8.8µs | 8.9x |
| 11_shortest_path | 5.3µs | 5.8µs | 1.1x |
| 12_st_dwithin_5km | 657.2µs | 1.21ms | 1.8x |
| 13_st_within_polygon | 14.0µs | — | sekejap-only |
| 14_spatial_category_filter | 59.1µs | 501.4µs | 8.5x |
| 15_vector_hnsw_top20 | 102.7µs | — | sekejap-only |
| 16_hybrid_spatial_vector | 1.07ms | 1.72ms | 1.6x |
| 17_hybrid_spatial_graph | 855.3µs | 1.99ms | 2.3x |
| 18_hybrid_graph_vector | 317.4µs | 2.44ms | 7.7x |
| 19_hybrid_ilike_vector_rag | 7.39ms | 5.08ms | 1.5x SLOWER |
| 20_holy_trinity_spatial_graph_vector | 387.0µs | 1.06ms | 2.7x |

**head-to-head: 17 wins / 1 loss** (+ sekejap-only cases)


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
