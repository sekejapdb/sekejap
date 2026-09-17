# Phase 2 multimodel R4 results

R4 is candidate evidence captured before the corrected recursive-query guard. It is not Phase 2 acceptance. The driver captured all 27 scheduled 10K/32-dimensional arms, but only 15 completed the full workload: all nine SQLite arms and the six E4 no-reader arms. Every E4 reader arm refused at the configured page-WAL managed-byte allowance.

Raw evidence is preserved at `<scratch>` in pod `e4-phase2-20260916-c9rp8`, namespace `sekejap-benchmark`. The report SHA-256 is `313fcc5e4c65b5e30025a4a68ec731a8c4ca63505a5a9f1dee42ee8fc43b006e`; binary SHA-256 is `91a79277649dc32b87fddb6a5bb21c2805c7b866cbc3fdb09b4da8de1d1b04e4`.

All arms used input digest `crc32c:d40e53a7`, 10,000 people, 100 organizations, 30,000 relationships, 32-float vectors, 256-row commits, 8 MiB cache, FULL WAL durability and `phase2-synthetic-v1`. E4 used compact cells with `compact-cells`, `sqlite-balance`, `keyspace-append` and `slotref-split`. Values below are median `[min–max]` across three alternating trials.

R4's SQLite edge table was an ordinary rowid table with a composite-primary-key autoindex and a separate `edge_out` index over the same six columns, plus `edge_in`. That redundant forward copy charged SQLite avoidable write and disk cost. R6 changes the candidate schema to a `WITHOUT ROWID` composite primary key plus only the reverse index and guards recursive expansion on the primary-key point lookup. R4's non-graph timings and footprints therefore remain diagnostics for this superseded schema, not the final fair SQLite baseline.

Reader scopes are materially different. `none` has no concurrent reader. R4 `short` is round-held: one snapshot spans an entire round of 10,000 updates plus 1,000 delete/reinsert operations, then releases. `held` spans all three rounds. R4 did not include a one-batch reader scope.

## Completion and refusal

| Reader mode | E4 atomic | E4 resumable | SQLite atomic |
|---|---:|---:|---:|
| None | 3 completed / 0 refused | 3 completed / 0 refused | 3 completed / 0 refused |
| Round-held (`short`) | 0 completed / 3 refused | 0 completed / 3 refused | 3 completed / 0 refused |
| Fully held | 0 completed / 3 refused | 0 completed / 3 refused | 3 completed / 0 refused |

All 12 E4 refusals reported `Kernel(ResourceLimit("page-WAL managed-byte allowance"))`. R4 converted these to successful process exits through string matching and emitted no failed stage, committed progress or post-refusal state oracle. They demonstrate deterministic refusal under this workload and limit; they do not establish that the last committed state was correct. Consequently there are no matched E4/SQLite latency or footprint medians for either reader mode.

## Successful-arm timings

The E4 columns contain only no-reader results. SQLite reader columns are included to show its completed round-held and fully held behavior; they are not matched comparisons against E4.

### Load and late builds — seconds

| Metric | E4 atomic, none | E4 resumable, none | SQLite, none | SQLite, round-held | SQLite, held |
|---|---:|---:|---:|---:|---:|
| Entity load | 0.469 [0.427–0.502] | 0.383 [0.360–0.387] | 0.292 [0.247–0.317] | 0.298 [0.283–0.627] | 0.306 [0.245–0.491] |
| Graph load | 1.157 [0.972–1.262] | 0.931 [0.901–1.071] | 0.455 [0.389–0.466] | 0.588 [0.437–0.658] | 0.424 [0.409–0.634] |
| Scalar build | 0.197 [0.186–0.230] | 0.467 [0.448–0.472] | 0.023 [0.019–0.031] | 0.021 [0.020–0.028] | 0.018 [0.015–0.027] |
| Exact-vector build | 0.044 [0.041–0.053] | 0.177 [0.177–0.224] | 0.000 [0.000–0.000] | 0.000 [0.000–0.000] | 0.000 [0.000–0.000] |
| Spatial build | 0.075 [0.068–0.097] | 0.228 [0.206–0.318] | 0.095 [0.086–0.099] | 0.099 [0.085–0.100] | 0.091 [0.078–0.091] |
| Text build | 1.865 [1.703–1.967] | 2.142 [2.005–2.142] | 0.029 [0.026–0.030] | 0.034 [0.030–0.040] | 0.030 [0.029–0.033] |

SQLite has no native vector index in this harness; its zero vector-build time is N/A, not a faster build. E4 atomic publishes each family once. E4 resumable publishes 80 scalar steps and 40 steps for each other family. SQLite publishes scalar, spatial and FTS builds once each.

### Pre-CRUD queries — milliseconds

| Metric | E4 atomic, none | E4 resumable, none | SQLite, none | SQLite, round-held | SQLite, held |
|---|---:|---:|---:|---:|---:|
| Scalar active+age | 0.93 [0.77–1.31] | 0.92 [0.85–1.01] | 0.12 [0.10–0.13] | 0.14 [0.06–0.15] | 0.12 [0.11–0.13] |
| Spatial bbox | 0.35 [0.32–0.43] | 0.47 [0.45–0.49] | 1.07 [0.97–1.09] | 1.16 [1.04–1.42] | 1.19 [1.04–3.23] |
| Exact vector cosine k=10 | 11.77 [10.25–12.15] | 14.47 [11.61–15.52] | 6.24 [5.80–7.84] | 6.93 [5.58–7.16] | 6.81 [6.73–7.49] |
| Common positive BM25 k=10 | 4.01 [3.62–4.29] | 5.12 [4.79–5.41] | 4.05 [3.93–4.63] | 4.80 [4.56–4.86] | 4.77 [4.44–7.02] |
| Text+active+vector | 12.77 [11.53–14.59] | 15.24 [12.33–16.01] | 3.14 [2.88–3.20] | 3.93 [3.10–5.48] | 2.97 [2.97–4.05] |
| Members+active+spatial+vector | 0.95 [0.88–1.01] | 0.98 [0.83–1.04] | 0.50 [0.48–0.56] | 0.53 [0.45–0.57] | 0.48 [0.44–0.73] |

SQLite native BM25, which has different score semantics, was 4.28 [3.38–4.42] ms with no reader, 4.77 [4.40–4.91] ms round-held and 4.56 [4.32–5.13] ms held. It is not compared with E4's positive BM25.

The recursive graph query is deliberately excluded. R4's recorded plan used `edge_out` for the anchor but reordered the recursive term to search broad `edge_in(context,destination_collection)` before scanning the frontier. The prior guard saw the anchor's `edge_out` and passed falsely. R5 forces frontier-first point-source `edge_out` probes and adds a recursive-term-specific guard; no R4 recursive latency should be cited.

### Three CRUD rounds — seconds

| Metric | E4 atomic, none | E4 resumable, none | SQLite, none | SQLite, round-held | SQLite, held |
|---|---:|---:|---:|---:|---:|
| Round 1 update all | 3.910 [3.857–4.800] | 3.919 [3.802–3.993] | 2.213 [2.183–2.326] | 2.035 [1.929–3.257] | 2.018 [1.891–2.087] |
| Round 1 delete 10% | 0.470 [0.463–0.544] | 0.496 [0.471–0.523] | 0.388 [0.380–0.435] | 0.346 [0.328–0.433] | 0.377 [0.362–0.415] |
| Round 1 reinsert+edges | 0.559 [0.494–0.560] | 0.526 [0.512–0.595] | 0.156 [0.150–0.174] | 0.189 [0.144–0.246] | 0.167 [0.150–0.186] |
| Round 2 update all | 4.022 [3.541–4.098] | 3.677 [3.492–3.699] | 2.148 [2.104–2.211] | 2.331 [2.161–2.340] | 2.174 [1.957–2.396] |
| Round 2 delete 10% | 0.434 [0.405–0.468] | 0.419 [0.396–0.434] | 0.192 [0.187–0.241] | 0.215 [0.185–0.215] | 0.169 [0.158–0.196] |
| Round 2 reinsert+edges | 0.514 [0.457–0.538] | 0.529 [0.481–0.564] | 0.167 [0.161–0.182] | 0.187 [0.180–0.188] | 0.167 [0.152–0.217] |
| Round 3 update all | 4.050 [3.437–4.056] | 3.702 [3.662–4.570] | 1.958 [1.953–1.970] | 1.973 [1.944–2.129] | 1.946 [1.830–2.206] |
| Round 3 delete 10% | 0.423 [0.392–0.433] | 0.435 [0.435–0.437] | 0.238 [0.228–0.242] | 0.221 [0.193–0.225] | 0.225 [0.225–0.229] |
| Round 3 reinsert+edges | 0.503 [0.497–0.519] | 0.522 [0.508–0.622] | 0.178 [0.165–0.184] | 0.211 [0.170–0.245] | 0.180 [0.175–0.317] |

### Post-CRUD queries and reopen — milliseconds

| Metric | E4 atomic, none | E4 resumable, none | SQLite, none | SQLite, round-held | SQLite, held |
|---|---:|---:|---:|---:|---:|
| Scalar age | 0.12 [0.09–0.12] | 0.10 [0.10–0.11] | 0.10 [0.09–0.10] | 0.13 [0.08–0.15] | 0.09 [0.09–0.09] |
| Spatial | 0.67 [0.53–0.68] | 0.53 [0.52–0.57] | 1.27 [1.02–1.32] | 1.15 [1.05–1.66] | 1.17 [0.95–2.06] |
| Text | 4.46 [4.34–5.18] | 4.89 [4.86–5.17] | 5.64 [4.55–5.80] | 6.12 [5.02–7.51] | 5.39 [5.35–6.71] |
| Exact vector | 12.15 [11.84–13.61] | 13.06 [11.33–16.42] | 6.58 [6.25–7.62] | 6.93 [6.60–8.33] | 6.51 [5.71–6.70] |
| Reopen | 10.58 [10.32–10.61] | 11.82 [10.59–14.57] | 1.37 [1.26–1.52] | 1.74 [1.21–2.51] | 1.08 [1.05–1.09] |

## Footprint

Peak disk is the larger of the harness's post-commit/stage samples and the driver's 50 ms samples. It remains a lower bound: files can grow between samples, and unlinked SQLite temporary files are invisible. VmHWM is the kernel process high-water mark.

| Metric (MiB) | E4 atomic, none | E4 resumable, none | SQLite, none | SQLite, round-held | SQLite, held |
|---|---:|---:|---:|---:|---:|
| Final logical | 8.66 [8.66–8.66] | 8.66 [8.66–8.66] | 8.05 [8.05–8.05] | 8.27 [8.27–8.27] | 8.27 [8.27–8.27] |
| Final allocated | 8.67 [8.67–8.67] | 8.67 [8.67–8.67] | 8.05 [8.05–8.05] | 8.45 [8.45–8.45] | 8.45 [8.45–8.45] |
| Best observed peak logical | 13.31 [13.00–13.31] | 13.31 [13.00–13.31] | 12.94 [12.94–12.94] | 131.56 [131.56–131.75] | 131.40 [130.38–131.75] |
| Best observed peak allocated | 16.61 [16.61–23.38] | 19.94 [19.69–21.19] | 16.05 [16.05–23.66] | 179.55 [179.55–179.55] | 134.82 [134.82–134.83] |
| VmHWM | 20.96 [20.77–20.96] | 20.93 [20.84–21.15] | 14.57 [14.54–14.57] | 14.70 [14.66–14.71] | 14.95 [14.95–14.99] |

The SQLite reader arms show the durability cost of retained snapshots clearly: observed logical peak grew from 12.94 MiB without a reader to about 131 MiB with either reader scope. This does not excuse the E4 refusals; it establishes that the matched comparison must report admission and transient disk behavior, not latency alone.

## Findings and limits

- E4 no-reader final storage was 8.66 MiB versus SQLite's 8.05 MiB, about 7.6% larger. No-reader logical sampled peaks were close, while allocated peaks varied materially across only three trials.
- E4 atomic spatial build and spatial queries were faster in these arms. Exact-vector query was about 1.9× SQLite's exact Rust scan, and text+active+vector was about 4.1× SQLite after R4 fixed SQLite's FTS-first join order.
- E4 text late build was about 64× SQLite FTS5 population under atomic publication. Scalar build was about 8.6× SQLite. Resumable publication adds the expected FULL-durability commit cost and must remain separate from atomic results.
- E4 full-population updates were about 1.8–2.1× SQLite in no-reader medians. Reinsertion plus edge recreation was roughly 3× SQLite. Indexed SQLite delete performance is now in the same broad range; R2's multi-second deletes were a superseded harness-plan defect.
- All successful arms passed the harness's independent result, mutation and reopen assertions. The report has one stable input digest and no nonzero subprocess exits. The 12 resource refusals lack a committed-state oracle, so they are neither completed workloads nor correctness passes.
- `complete: true` in the driver report means all scheduled processes returned evidence. It must not be read as 27 successful workload completions.
- R4 covers one synthetic 10K/32-dimensional profile. It does not establish 100K/1M behavior, 1536-dimensional multimodel performance, true instantaneous peak disk, public acceptance or full Phase 2 coverage.
