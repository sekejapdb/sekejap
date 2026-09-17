# Phase 2 multimodel 10K smoke-r2 — superseded harness evidence

Status: both E4 and SQLite arms exited successfully with the same `crc32c:d40e53a7` input at 10,000 people, 100 organizations, 30,000 relationships, 32-float vectors, 8 MiB caches, FULL WAL durability, 256-row batches, atomic index publication, and no concurrent reader. This is one unalternated trial for harness qualification. It is not benchmark acceptance or a performance conclusion.

Smoke-r2 is superseded for delete and aggregate CRUD comparisons. Its SQLite delete statement combined outgoing and incoming deletion with an `OR` while omitting the leading `context` column of both edge indexes. That plan could scan `edges` once per deleted person and explains the 4.16–4.52 s SQLite delete stages. R3 replaced it with separate context-0 outgoing and incoming deletes; SQLite delete fell to 0.367 / 0.192 / 0.274 s across the three rounds.

R3 also confirmed that the first FTS plan gate was too weak. SQLite 3.46 planned text + active as `SEARCH p USING COVERING INDEX people_active_age(active=?)` followed by `SCAN f VIRTUAL TABLE INDEX 0:=M1`, repeating FTS work from the active-person outer loop; its 364 ms result is therefore not a fair comparison with E4's explicit text driver. R4 uses candidate-first `CROSS JOIN` order for FTS, RTree, recursive graph and incoming-membership paths, asserts loop order as well as index presence, and records the complete plans. Both r2 and r3 text + active numbers are invalid as engine-performance evidence.

## One-trial observations

| Stage | E4 | SQLite | Qualification reading |
|---|---:|---:|---|
| Entity load | 0.382 s | 0.312 s | E4 1.22x SQLite |
| Graph load, both directions maintained | 0.838 s | 0.491 s | E4 1.71x SQLite |
| Scalar late build | 0.214 s | 0.024 s | E4 8.89x; structures differ |
| Spatial late build | 0.079 s | 0.094 s | E4 0.84x SQLite |
| Text late build | 1.756 s | 0.029 s | E4 60.7x; native formats differ |
| CRUD update round 1 / 2 / 3 | 3.847 / 3.369 / 3.863 s | 2.072 / 2.041 / 1.819 s | E4 1.65–2.12x |
| CRUD reinsert round 1 / 2 / 3 | 0.504 / 0.468 / 0.611 s | 0.173 / 0.203 / 0.196 s | E4 2.31–3.11x |
| Reopen | 12.43 ms | 1.34 ms | E4 9.30x |

SQLite has no native exact-vector index in this harness, so its reported zero vector-build time is N/A rather than faster. E4 scalar build creates two independent indexes while SQLite creates age and active-age B-trees. Text timings cover different native storage and ranking implementations. Update and reinsert include all maintained native indexes in E4, while SQLite scans vector blobs at query time; they are end-to-end workload stages rather than isolated equivalent primitives.

Delete timings and any total wall or aggregate CRUD value that includes them are invalid for comparison in smoke-r2. The harness also performs substantial untimed independent correctness scans, so process elapsed time is not the sum of named timed stages and is not a throughput measure.

## Query observations

| Query | E4 | SQLite | One-trial reading |
|---|---:|---:|---|
| Scalar active + age | 0.726 ms | 0.199 ms | E4 3.65x |
| Spatial bbox, exact ordered IDs | 0.395 ms | 0.990 ms | E4 2.51x faster |
| Exact vector | 11.47 ms | 6.75 ms | E4 1.70x |
| Common positive BM25 oracle semantics | 5.65 ms | 4.58 ms | E4 1.23x |
| Tiny two-hop graph + active + bbox + vector | 0.245 ms | 21.63 ms | E4 88.4x faster, but only five hits |
| Incoming members + active + spatial + vector | 1.052 ms | 0.438 ms | E4 2.40x |
| Text + active + vector | 15.71 ms | 391.65 ms | E4 24.9x faster |

The text + active + vector ratio is invalid because the recorded SQLite loop order was poor. E4 used the text candidate driver and examined 3,750 candidates before 2,499 vector sidecar reads; the corrected SQLite path must also start from FTS candidates before active refinement and exact Rust f32 ranking. The 88x tiny graph result is likewise withheld until the corrected candidate-first recursive plan runs; it covers only a six-node seed neighborhood. The incoming-members query moves in the opposite direction and remains a useful counterexample, subject to the same repeated-trial requirement.

SQLite native BM25 is reported separately because its scoring semantics differ. The common positive scorer is the only direct text-score comparison, and the independent expected answer is computed outside both storage engines. SQLite spatial queries use RTree overlap candidates followed by exact f64 longitude/latitude refinement and compare complete ordered IDs; E4 no longer truncates its correctness oracle.

## Footprint observations

| Measure | E4 | SQLite | E4 relative to SQLite |
|---|---:|---:|---:|
| Loaded logical bytes | 4,726,880 | 5,566,464 | 15.1% smaller |
| Final logical bytes | 9,085,024 | 8,437,760 | 7.7% larger |
| Sampled logical peak | 13,220,784 | 13,571,312 | 2.6% smaller |
| Sampled allocated peak | 17,408,000 | 16,826,368 | 3.5% larger |
| Process VmHWM | 21,632 KiB | 15,024 KiB | 1.44x |

Peak disk values are discrete sampled lower bounds. SQLite unlinked temporary files may be absent from directory accounting. The driver’s timed sampler can observe transient files between commit samples, but it still cannot prove the true instantaneous peak.

R3 should be treated as the first comparable delete/CRUD harness. Three alternating trials are still required for medians and ranges; held-reader and short-reader modes, scale profiles beyond 10K, 1536-dimensional coverage, resumable publication, and WAL-cap refusals remain outside this smoke.
