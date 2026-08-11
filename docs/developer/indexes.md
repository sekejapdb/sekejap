# Indexes — the six families and their disk-first designs

Every index family answers the same way: with node ids over the same
graph-backed record space. That shared currency is what makes retrieval models
composable (see [queries.md](queries.md)). This page is each family's
structure and where its bytes live.

| family | SQL surface | resident in RAM | on disk |
|---|---|---|---|
| scalar btree/hash | `WHERE x = / < / BETWEEN`, `ORDER BY` | heap btree (resident mode) or just the mmap window (paged) | mapped field-index sidecar |
| graph adjacency | `FROM MATCH`, hops, shortest path | node → (offset, count) | forward/reverse CSR files (mmap) |
| vector HNSW | `VECTOR_NEAR`, `<->` `<=>` `<#>` `<+>` | graph + int8 codes | full-precision f32 vectors |
| text BM25 | `BM25(field, 'q')` | dictionary (term → offset), sub-linear | postings blob + doc arrays (mmap, no rebuild on open) |
| positional search | `SEARCH('q')`, `SEARCH_SCORE()` | loaded structures | rebuildable sidecar |
| trigram GIN | `ILIKE '%…%'` | postings | rebuildable sidecar |
| spatial grid | `ST_DWithin`, `ST_Contains`, … | occupancy grid + cached polygon rings | geometry in payloads |

## Vector: traverse compressed, rescore exact

The disk-first HNSW (built via `build_hnsw_index_disk`, automatic for disk
databases) splits precision from recall:

- **In RAM**: the HNSW graph in compact slot-indexed CSR form, plus int8
  scalar-quantized codes (per-dimension calibration on the 0.5/99.5
  percentiles). Roughly a quarter of f32 for the codes; ~274 B/node total.
- **On disk**: the full f32 vectors.
- **Search**: traverse the graph on int8 (SIMD kernels), collect a candidate
  pool, rescore the pool from f32 via positional reads, return exact-ranked
  top-k.

At 1M×128-d this cut process memory 3.6× (2.4 GB → 655 MB) while *improving*
tail latency — the quantized working set is cache-friendly. Recall is a knob
(`set_hnsw_ef_search`): 0.954 at 301 QPS, 0.9915 at 138 QPS on SIFT1M.
Metrics: L2, cosine, dot, L1 — fixed per index at build time, persisted.

## Graph: mmap'd CSR slices

Traversal never touches payloads. After compaction (or an explicit spill),
adjacency serves directly from the mmap'd CSR files as zero-copy edge slices;
only the (offset, count) index stays on the heap. BFS shortest path, k-hop
expansion, and the aggregation fast paths (frontier-merged counts — see
[queries.md](queries.md)) all run on this layout.

## Text: dictionary in RAM, postings on disk

The BM25 index keeps the term dictionary resident — sub-linear in corpus size by
Heaps' law, the chosen accelerator — while the postings blob is read positionally
per query term from disk. The two O(N) pieces (per-doc lengths and the
doc-id→slot map) are persisted to `bm25.bin` and, in paged mode, mmap'd rather
than rebuilt: a flat u32 length array and a sorted `(doc_id, slot)` array
binary-searched in place. So open is flat in corpus size (mmap, no rebuild) and
resident heap stays tiny — a 50k corpus serves ranked search from ~14 KB paged.
The positional `search` index and the trigram GIN follow the same sidecar
pattern and are always rebuildable.

## Spatial: grid + cached rings

Geometry metadata (centroid, bbox) is extracted at insert into the node entry;
an occupancy grid prunes candidates, and exact geodesic tests (WGS84, metres —
PostGIS-equal to float epsilon) decide. Point-in-polygon keeps parsed polygon
rings cached, which is why PIP runs in ~0.2 ms over 129 city polygons. This is
the one family whose bulk geometry is still RAM-cached; moving it to the
disk-first discipline is open work.

## Maintenance rules

Index updates ride the write path (insert/update/delete touch affected
entries); builds are explicit (`CREATE INDEX` / `build_*_index`) and
incremental HNSW insertion applies when the index is declared in the schema.
The per-write and per-build rules are in [invariants.md](invariants.md).
