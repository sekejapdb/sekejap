# Phase 2 scalar r6 Linux evidence

Status: bounded scalar/index diagnostic and graph qualification evidence, not Phase 2 or public-release acceptance.

Run location: server server pod `e4-phase2-20260916-c9rp8`, namespace `sekejap-benchmark`; artifact root `<scratch>`. Both `graph-r2.exit` and `bench-r6.exit` are `0`.

## Workload and pairing

The scalar run used 100,000 deterministic rows with external key, integer age, text name and boolean active; an existing unique external-key index plus a late nonunique age index; 8 MiB configured cache/budget; FULL WAL publication; and 256-row ordinary write commits. Each CRUD run updates all rows, deletes 10% and reinserts that 10%, repeated for three rounds.

For each build, three atomic E4/SQLite pairs alternated order: E4 then SQLite, SQLite then E4, E4 then SQLite. E4 still executes 391 bounded 256-row index-build steps, but the atomic arm publishes them with one final commit, matching SQLite's one `CREATE INDEX` transaction at the publication-policy level. A separate single E4 resumable arm commits every one of its 391 steps.

The default build reports all four storage features false and creates noncompact cells. The retained build reports `compact-cells`, `sqlite-balance`, `keyspace-append` and `slotref-split` true and creates compact cells. SQLite 3.46.0 reports WAL, `synchronous=2` (FULL), `cache_size=-8192`, and `temp_store=1` (FILE). These are engine settings, not proof of identical internal work or total-memory bounds.

All 14 JSON arms have `verified=true`. Every churn round passed the independent generated-key oracle, including exact age/entity-ID order, and every query returned its expected hit count (1,250 or 3,750). Reopen count verification returned 100,000 rows. E4 and SQLite reopen timings include open/configure/name resolution and exclude the row count, but their internal resolution work still differs, so no reopen ratio is claimed.

## Atomic timing results

Cells are median seconds `[min–max]` across three runs. CRUD components are sums across the three rounds within each run; the median and range are then taken across runs. Ratio is median E4 / median SQLite.

| Build | Measurement | E4 seconds | SQLite seconds | Ratio |
|---|---|---:|---:|---:|
| default | load | 3.103 `[2.840–3.205]` | 1.628 `[1.598–1.815]` | 1.91x |
| default | atomic late index | 0.542 `[0.524–0.545]` | 0.055 `[0.052–0.067]` | 9.86x |
| default | three-round updates | 20.049 `[19.316–21.825]` | 8.779 `[7.507–11.565]` | 2.28x |
| default | three-round deletes | 1.976 `[1.788–2.287]` | 0.699 `[0.583–0.890]` | 2.83x |
| default | three-round reinserts | 1.902 `[1.899–2.201]` | 0.935 `[0.713–1.388]` | 2.04x |
| default | all three-round CRUD | 23.927 `[23.003–26.313]` | 10.413 `[8.803–13.843]` | 2.30x |
| retained | load | 2.811 `[2.630–3.090]` | 2.885 `[2.503–3.029]` | 0.97x |
| retained | atomic late index | 0.671 `[0.662–0.743]` | 0.074 `[0.049–0.079]` | 9.03x |
| retained | three-round updates | 26.662 `[23.837–27.939]` | 11.315 `[8.928–12.825]` | 2.36x |
| retained | three-round deletes | 2.068 `[1.656–3.225]` | 0.746 `[0.738–1.112]` | 2.77x |
| retained | three-round reinserts | 2.361 `[2.242–2.808]` | 1.158 `[0.898–1.256]` | 2.04x |
| retained | all three-round CRUD | 30.680 `[28.148–33.971]` | 12.958 `[10.825–15.194]` | 2.37x |

Atomic publication removes most of r5's index-build policy mismatch, but E4's late index remains about 9–10x SQLite in these runs. The retained features materially reduce E4 disk use but do not improve this scalar write/index timing set. The retained timing ranges are also broad enough that the apparent load parity should not be generalized.

## Scalar query timing

Cells are median milliseconds `[min–max]` for the indexed candidate query only. Payload/key resolution used by the oracle is outside the timer for both engines.

| Build | Range and hits | E4 ms | SQLite ms | Ratio |
|---|---|---:|---:|---:|
| default | age 38, 1,250 | 0.263 `[0.259–0.428]` | 0.337 `[0.303–0.489]` | 0.78x |
| default | age 47–49, 3,750 | 0.765 `[0.649–1.118]` | 0.612 `[0.489–0.674]` | 1.25x |
| default | age 90–92, 3,750 | 0.773 `[0.702–0.930]` | 0.671 `[0.601–0.731]` | 1.15x |
| retained | age 38, 1,250 | 0.272 `[0.263–0.301]` | 0.363 `[0.362–0.674]` | 0.75x |
| retained | age 47–49, 3,750 | 0.520 `[0.518–0.602]` | 1.192 `[0.761–12.980]` | 0.44x |
| retained | age 90–92, 3,750 | 0.545 `[0.485–0.575]` | 0.603 `[0.601–0.627]` | 0.90x |

These submillisecond samples do not establish a query-speed advantage. In particular, the retained SQLite middle-range arm has a 12.980 ms outlier and a wide spread.

## Disk results

Cells are median MiB `[min–max]` across the three atomic runs. All three samples for each displayed cell were byte-identical. Sizes recursively include regular files under each dedicated root, including its SQLite temp directory.

| Build | Measurement | E4 MiB | SQLite MiB | Ratio |
|---|---|---:|---:|---:|
| default | final logical | 14.836 `[14.836–14.836]` | 7.598 `[7.598–7.598]` | 1.95x |
| default | final allocated | 14.840 `[14.840–14.840]` | 7.598 `[7.598–7.598]` | 1.95x |
| default | sampled peak logical | 18.828 `[18.828–18.828]` | 12.230 `[12.230–12.230]` | 1.54x |
| default | sampled peak allocated | 22.777 `[22.777–22.777]` | 15.629 `[15.629–15.629]` | 1.46x |
| retained | final logical | 9.738 `[9.738–9.738]` | 7.598 `[7.598–7.598]` | 1.28x |
| retained | final allocated | 9.742 `[9.742–9.742]` | 7.598 `[7.598–7.598]` | 1.28x |
| retained | sampled peak logical | 13.738 `[13.738–13.738]` | 12.230 `[12.230–12.230]` | 1.12x |
| retained | sampled peak allocated | 17.680 `[17.680–17.680]` | 15.629 `[15.629–15.629]` | 1.13x |

These are sampled lower bounds, not true peaks or hard space guarantees. Sampling occurs after commits and phases; it can miss sub-commit maxima and unlinked SQLite temporary files. The overall sampled peak may occur during later churn rather than index construction.

## Atomic versus resumable E4 publication

The atomic column is the three-run median and range. The resumable arm is one separate run and therefore has no spread.

| Build | Atomic late index seconds | Resumable seconds | Resumable / atomic | Commits atomic / resumable | Late-stage sampled peak MiB, logical / allocated (atomic → resumable) |
|---|---:|---:|---:|---:|---:|
| default | 0.542 `[0.524–0.545]` | 4.181 | 7.71x | 1 / 391 | 13.879 / 13.883 → 17.553 / 21.008 |
| retained | 0.671 `[0.662–0.743]` | 4.528 | 6.75x | 1 / 391 | 9.293 / 10.918 → 13.066 / 17.039 |

The resumable policy costs 3.639 seconds over the default atomic median and 3.856 seconds over the retained atomic median here. This is an operational durability/resumption tradeoff, separate from the one-publication E4/SQLite index comparison.

## Graph qualification

The graph script's focused fault-boundary preflight passed 1 test (0 failed, 0 ignored; the named integration binaries were filtered by the command's name filter). Full release workspace totals, summed from each log's `test result` records, were:

| Suite | Passed | Failed | Ignored | Measured |
|---|---:|---:|---:|---:|
| default | 451 | 0 | 2 | 0 |
| compact (`compact-cells,sqlite-balance`) | 456 | 0 | 2 | 0 |
| retained (all four features) | 456 | 0 | 2 | 0 |

This qualifies the tested graph/scalar workspace source on this Linux run. It does not establish graph performance, all multimodel workload coverage, recovery/compatibility completion, or public acceptance.

## Preserved evidence

Raw JSON, graph/full-suite logs, build logs, scripts, binary hashes, exact r6 harness source and exit markers are in `docs/phase2-evidence/r6/`. The preserved harness SHA-256 is `93da7f4cc48892ff0949e6fb06ab8f221939aeb779f839e7b41e3e4ec6bd0033` and matches `src/bin/phase2_scalar_bench.rs` at collection time. Binary SHA-256 values are preserved in `bench-r6-binaries.sha256`.

This evidence covers one 100K scalar lifecycle workload on one Linux server. It does not cover 10K/representative 1M scale, readers, graph/vector/spatial/text performance, full peak capture, or the remaining Phase 2 acceptance matrix.
