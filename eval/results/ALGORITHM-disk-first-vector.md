# sekejap disk-first int8 vector index — how it works

A disk-first approximate-nearest-neighbour index for an embedded database: a small
**int8-compressed working set + graph** stays in RAM; **full-precision vectors live on disk**
and are read only to re-rank a handful of final candidates. On SIFT1M (1M×128, L2) it serves the
index at **655 MB steady-state process RSS** (467 MB of which is the index data structures) — a
**3.6× cut** from a 2.4 GB all-in-RAM HNSW — across a recall↔QPS curve of **recall@10 0.954 @
301 QPS (ef=100) → 0.992 @ 138 QPS (ef=800)**. Build peak is transient (~3.0 GB VmHWM, freed after
construction). *(Patched/unreleased build — see §7.)*

## 1. Data layout

**In RAM — one compact, slot-indexed structure** (`CompactDiskIndex`), nodes numbered `0..n`:

| field | type | bytes/node | purpose |
|-------|------|-----------:|---------|
| `codes` | `[u8; n·dim]` (slot-major) | `dim` (128) | int8 quantized vectors for traversal |
| `l0_off` | `[u32; n+1]` | 4 | CSR row offsets, graph layer 0 |
| `l0_neigh` | `[u32]` | `deg·4` (~128) | layer-0 neighbour **slots** (CSR values) |
| `upper` | sparse `slot → [[u32]]` | ~0 | layers ≥1 (only ~1/M nodes) |
| `slot_to_id` | `[u64; n]` | 8 | slot → external id (results + re-rank) |
| `quantizer` | `{offset, scale}` | O(1) | affine int8 map |

Total ≈ `dim + deg·4 + 12` ≈ **274 B/node** at dim=128 (128 codes + ~128 edges + 12 index).
There is **no id→slot hash map**: the entire search runs in slot space; ids are recovered only for
the final re-rank. This is what makes the hot loop pure array indexing (no hashing).

**On disk** — full-precision f32, append-only record `[id | dim | f32×dim]`; a small `id→offset`
map (≈16 B/node) is the only disk-store RAM.

## 2. Scalar quantization (int8)

One global affine map per field, calibrated to the 0.5 %/99.5 % quantiles of a component sample
(clips outliers so they don't crush precision):

```
scale  = (q99.5 − q0.5) / 255
code_i = clamp(round((x_i − offset) / scale), 0, 255)      // u8, offset = q0.5
```

**Distance.** For L2, the squared distance in code space is `scale² · Σ(a_i − b_i)²`. `scale²` is a
positive constant, so for *ranking* during traversal the raw integer `Σ(a_i − b_i)²` is used
directly (no float on the hot path). The int8 kernel is SIMD: AVX2 widen-`madd` (`_mm256_madd_epi16`
on u8→i16 diffs) / NEON `vabd_u8`+`vmull_u8`+`vpadal`, scalar fallback.

## 3. Build (once, on ingest)

```
1. Ingest f32 vectors to the on-disk store (WAL-batched: one fsync per bulk, not per vector).
2. Build the HNSW graph on FULL-PRECISION f32   → best graph quality.
3. Calibrate the scalar quantizer from a component sample (0.5/99.5 quantile).
4. Quantize every vector → int8 codes.
5. Compact: relabel nodes to dense slots 0..n; emit CSR layer-0 + sparse upper layers;
   pack codes slot-major; build slot_to_id.
6. Drop the fat HNSW graph + the f32 snapshot from RAM; malloc_trim() returns the
   build scratch to the OS. Only CompactDiskIndex (RAM) + f32 (disk) remain.
```

Building the graph on f32 (not on int8) keeps recall high; quantization affects only the
in-RAM *search* copy, and the disk re-rank corrects its error.

## 4. Query — two-stage (traverse compressed, re-rank exact)

```
Given query q, result size k, oversample factor r (=8):
1. Quantize q → q_code (same calibration).
2. Traverse the CSR graph on int8 L2 (greedy descent through upper layers, then a
   beam search of width ef at layer 0) → top (k·r) candidate slots.
3. Re-rank: for each candidate, pread its f32 from disk, compute exact L2, keep top k.
   (pread pages go through the reclaimable kernel page cache — never the process RSS.)
```

Stage 2 is fast and RAM-resident; stage 3 touches disk for only `k·r` (≈80) vectors per query.
The exact re-rank *raises* recall above int8-only. `ef` is the speed↔recall knob.

## 5. Cost

- **RAM:** `n·(dim + deg·4 + 12)` bytes for the index + `n·16` for the disk-store offset map.
  Independent of f32 (on disk). SIFT1M: 0.47 GB engine total (incl. shared node metadata).
- **Disk:** `n·(dim·4 + 12)` bytes (SIFT1M: 500 MB).
- **Query:** one graph traversal (int8, in RAM) + `k·r` random disk reads (re-rank).
- **Build:** dominated by HNSW construction on f32 (parallel); +one linear quantize + compact pass.

## 6. Result (SIFT1M, M=16, ef_construction=200)

| ef | recall@10 | QPS | p50 ms | p99 ms |
|---:|----------:|----:|-------:|-------:|
| 100 | 0.9540 | 301.5 | 3.00 | 7.94 |
| 400 | 0.9851 | 204.6 | 4.76 | 9.80 |
| 800 | 0.9915 | 138.4 | 7.13 | 12.91 |

**RAM: 655 MB steady-state process RSS** (standalone-measured, no harness) — index data structures
467 MB (compact index 0.26 + shared node metadata 0.14 + disk-store map 0.04 + misc 0.02 GB) +
~188 MB runtime/allocator slack; full f32 (0.5 GB) on disk. **Build peak is separate: ~3.0 GB
VmHWM**, transient (the f32 snapshot + fat graph coexist during construction, freed after; `malloc_
trim` returns the arena). At matched recall ≈0.99: 138 QPS vs Qdrant's 105 — faster, at 1.5×
Qdrant's RAM (0.66 vs 0.43 GB). 2nd-lowest steady RAM of the six engines tested.

## 7. Provenance

Measured on a **patched, unreleased build** of sekejap — branch `feat/int8-disk-first-vectors`
on top of tag `d82a26a`. The source tree is dirty (modified + untracked files: `src/vector/
compact.rs`, `quant.rs`, and edits to `lib.rs`/`query.rs`/`hnsw.rs`/`vecstore.rs`), not yet
committed or released. These are valid experimental results for the disk-first design, not a
released-baseline benchmark. RAM confirmed by a standalone process-RSS harness (`examples/
rss_standalone.rs`, no DuckDB, input vectors freed post-ingest).
