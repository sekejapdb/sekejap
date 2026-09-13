# Peak disk space before interfaces

Completed 2026-09-10. This loop fixes a real reclamation delay and demonstrates a large checkpoint-policy space/time tradeoff. **Phase-only checkpointing fails the proposed 2× peak-space budget; frequent checkpoints help ordinary writes but are not an enforced cap.** Interfaces remain paused. No hard disk quota, automatic shrink, new on-disk format, or additional law was introduced.

## Measurement contract

Sizes are decimal MB. Logical file lengths and allocated-file peaks are labeled separately. Every arm stores the same typed mixed people fixture as its SQLite peer, including names, scalars, Point coordinates, nested binary JSON and schemaless fields. Automatic timestamps, vectors, graph edges and secondary indexes are absent. Each writer uses an 8 MiB engine cache, readers 64 KiB, 4096-byte pages, FULL sync with macOS fullfsync, mmap off and 1,000-operation commits. SQLite version is 3.46.0. All data and scratch are on scratch.

The original reference is the **closed load-only database from the phase-checkpoint run**: E4 19.386/75.072 MB and SQLite 17.519/70.963 MB at 100K/400K. The same reference is used for every policy. A policy that bloats its initial file does not get an easier expansion denominator. Raw results additionally retain each run’s own initial size. Load peak is divided by the finished load-only reference, since the empty starting file is not a meaningful denominator.

The baseline matrix checkpoints at each phase end. Updates change 30% of scattered keys with fixed-length strings. Delete/reinsert removes 10% then restores identical records. Mixed workloads update 20%, delete a disjoint 10%, then reinsert; odd cycles grow 1% of rows to 9 KB notes and another 9% to 512 bytes; even cycles restore 24-byte notes. Thus mixed peaks include real, intentional payload growth as well as version/storage overhead. Each workload runs six cycles. Short snapshots retain a cycle’s starting state through its two phases and immediately reopen for the next cycle: no checkpoint is scheduled in the unpinned gap. This intentionally exposes checkpoint starvation despite reader turnover; it is not a claim that short readers require this expansion. Long snapshots retain the initial state through two cycles, then release it.

A 1 ms requested sampler and explicit observations after each commit measure all files in the arm directory. Rebuild observations include source + destination + sort/recovery scratch simultaneously. Scheduling can delay the sampler, so these observations are **not an exact instantaneous maximum**. Per-phase checkpoint bounds additionally include the maximum main-file length while WAL and old/new freelist sidecars coexist; mutation file lengths grow monotonically between explicit checkpoints in this harness. The bound is for these normal logical-file operations, not APFS allocation, arbitrary SQL, recovery, or other writers. Allocated-file `st_blocks × 512` peaks are shown separately; neither figure proves actual free-volume consumption under APFS snapshots/clones.

## Phase-end checkpoint policy: within-operation peaks and final sizes

Peak for churn rows excludes initial load (load-only has its own row). “Bound” is the larger of the sampled peak and the conservative checkpoint bound. All database/WAL/metadata files count; final size is after close.

### 100,000 records

| Workload | Engine | Original MB | Observed peak MB | Allocated peak MB | Bound MB | Peak/original | Final MB | 2× target |
|---|---|---:|---:|---:|---:|---:|---:|---|
| Load only | e4 | 19.386 | 37.743 | 38.179 | 37.743 | 1.95× | 19.386 | within_logical_bound_only |
| Load only | sqlite | 17.519 | 418.481 | 429.658 | 418.481 | 23.89× | 17.519 | fail_observed |
| Repeated updates, fixed-length strings | e4 | 19.386 | 63.687 | 65.520 | 63.687 | 3.29× | 58.144 | fail_observed |
| Repeated updates, fixed-length strings | sqlite | 17.519 | 141.905 | 152.900 | 141.905 | 8.10× | 17.519 | fail_observed |
| Delete/reinsert identical rows | e4 | 19.386 | 60.017 | 61.325 | 60.017 | 3.10× | 58.144 | fail_observed |
| Delete/reinsert identical rows | sqlite | 17.519 | 59.505 | 60.625 | 59.505 | 3.40× | 17.519 | fail_observed |
| Mixed grow/shrink, no snapshot | e4 | 19.386 | 103.541 | 118.661 | 103.541 | 5.34× | 86.191 | fail_observed |
| Mixed grow/shrink, no snapshot | sqlite | 17.519 | 220.941 | 233.640 | 220.941 | 12.61× | 32.903 | fail_observed |
| Mixed, snapshot held one cycle | e4 | 19.386 | 108.203 | 118.870 | 108.203 | 5.58× | 90.848 | fail_observed |
| Mixed, snapshot held one cycle | sqlite | 17.519 | 1,144.384 | 1,158.644 | 1,144.388 | 65.32× | 32.903 | fail_observed |
| Mixed, snapshot held two cycles | e4 | 19.386 | 150.101 | 153.182 | 150.101 | 7.74× | 132.705 | fail_observed |
| Mixed, snapshot held two cycles | sqlite | 17.519 | 595.108 | 605.164 | 595.108 | 33.97× | 32.903 | fail_observed |

### 400,000 records

| Workload | Engine | Original MB | Observed peak MB | Allocated peak MB | Bound MB | Peak/original | Final MB | 2× target |
|---|---|---:|---:|---:|---:|---:|---:|---|
| Load only | e4 | 75.072 | 148.677 | 169.546 | 148.677 | 1.98× | 75.072 | fail_observed |
| Load only | sqlite | 70.963 | 2,406.569 | 2,436.473 | 2,406.569 | 33.91× | 70.963 | fail_observed |
| Repeated updates, fixed-length strings | e4 | 75.072 | 247.542 | 258.367 | 247.542 | 3.30× | 225.316 | fail_observed |
| Repeated updates, fixed-length strings | sqlite | 70.963 | 569.918 | 592.548 | 569.918 | 8.03× | 70.963 | fail_observed |
| Delete/reinsert identical rows | e4 | 75.072 | 232.823 | 243.372 | 232.823 | 3.10× | 225.316 | fail_observed |
| Delete/reinsert identical rows | sqlite | 70.963 | 240.318 | 257.004 | 240.318 | 3.39× | 70.963 | fail_observed |
| Mixed grow/shrink, no snapshot | e4 | 75.072 | 411.839 | 437.498 | 411.839 | 5.49× | 342.394 | fail_observed |
| Mixed grow/shrink, no snapshot | sqlite | 70.963 | 968.233 | 995.201 | 968.233 | 13.64× | 132.768 | fail_observed |
| Mixed, snapshot held one cycle | e4 | 75.072 | 416.886 | 437.756 | 416.886 | 5.55× | 347.425 | fail_observed |
| Mixed, snapshot held one cycle | sqlite | 70.963 | 4,687.480 | 4,690.383 | 4,687.710 | 66.06× | 132.768 | fail_observed |
| Mixed, snapshot held two cycles | e4 | 75.072 | 581.404 | 605.532 | 581.404 | 7.74× | 511.781 | fail_observed |
| Mixed, snapshot held two cycles | sqlite | 70.963 | 2,477.359 | 2,488.373 | 2,477.359 | 34.91× | 132.768 | fail_observed |

The budget is twice the common original logical-file reference, applied to both observed logical and allocated-file bytes. A failure means at least one observed measure already exceeded that budget; sampling uncertainty cannot turn it into a pass. `within_logical_bound_only` means the logical upper bound fits and no allocated-file observation violated the budget; allocated-space peaks still have no proven upper bound. **There is no runtime enforcement of a 2× cap.**

## Reclamation fix: refresh after a reader closes

The old writer updated its reuse horizon at checkpoint/reopen. If a snapshot closed afterward, a whole following phase could allocate before noticing. The new commit path refreshes the same conservative horizon after a successful durable commit. The next batch can reuse eligible pages. It does not publish a root, release pages protected by a live/ambiguous reader, or bypass the dual-slot fallback. A reader closing inside a batch can still wait until its next commit.

The regression first failed with `(eligible, waiting) = (0, 24)` both before and after reader release + commit. It now passes and checks original snapshot values, fallback protection, corrupt reader-slot refusal to advance, and exact reopened current values. Named cost: one reader-directory inspection per successful commit, proportional to registered reader slots; no database-page scan.

| Rows | E4 churn peak before MB | After MB | Retained before MB | After MB | Peak reduction |
|---|---:|---:|---:|---:|---:|
| 100,000 | 182.350 | 150.101 | 164.923 | 132.705 | 17.7% |
| 400,000 | 723.304 | 581.404 | 653.543 | 511.781 | 19.6% |

These are same workload/phase-cadence comparisons using retained before/after executables. The allocation fix does not shrink already-expanded files. The earlier overflow-retirement fix is present in both arms.

## Checkpoint frequency is part of the space result

SQLite automatic checkpoints are disabled in the baseline matrix, matching the earlier lifecycle experiment. Huge WAL peaks there must not be presented as an unavoidable SQLite requirement. The following matched runs explicitly checkpoint both engines every 1,000 operations and at phase end, with no pinned reader in the load/update/mixed controls, plus a separate 100K long-reader stress arm. E4 still uses its normal one-publication checkpoint.

| Rows | Workload | Engine | Observed peak MB | Allocated peak MB | Bound MB | Peak / common original | Final MB |
|---|---|---|---:|---:|---:|---:|---:|
| 100,000 | Load only | e4 | 29.580 | 29.774 | 29.580 | 1.53× | 29.386 |
| 100,000 | Load only | sqlite | 22.697 | 23.364 | 22.697 | 1.30× | 17.519 |
| 400,000 | Load only | e4 | 87.453 | 101.827 | 87.453 | 1.16× | 87.257 |
| 400,000 | Load only | sqlite | 77.127 | 91.918 | 77.127 | 1.09× | 70.963 |
| 100,000 | Mixed grow/shrink, no snapshot | e4 | 49.588 | 50.135 | 49.588 | 2.56× | 48.983 |
| 100,000 | Mixed grow/shrink, no snapshot | sqlite | 40.393 | 41.411 | 40.393 | 2.31× | 32.903 |
| 400,000 | Mixed grow/shrink, no snapshot | e4 | 161.030 | 169.132 | 161.030 | 2.15× | 160.387 |
| 400,000 | Mixed grow/shrink, no snapshot | sqlite | 139.936 | 143.454 | 139.936 | 1.97× | 132.768 |
| 100,000 | Repeated updates, fixed-length strings | e4 | 29.580 | 30.478 | 29.580 | 1.53× | 29.386 |
| 100,000 | Repeated updates, fixed-length strings | sqlite | 21.671 | 22.790 | 21.671 | 1.24× | 17.519 |
| 400,000 | Repeated updates, fixed-length strings | e4 | 87.453 | 101.511 | 87.453 | 1.16× | 87.257 |
| 400,000 | Repeated updates, fixed-length strings | sqlite | 75.116 | 88.572 | 75.116 | 1.06× | 70.963 |
| 100,000 | Mixed, snapshot held two cycles | e4 | 434.488 | 438.407 | 434.488 | 22.41× | 433.507 |
| 100,000 | Mixed, snapshot held two cycles | sqlite | 464.843 | 470.688 | 464.843 | 26.53× | 32.903 |

More frequent checkpoints trade sync/publication work for shorter WAL history. At 400K mixed churn, E4 mutation time rose from 24.94 s to 119.08 s (4.77× observed); SQLite rose from 33.39 s to 104.84 s (3.14×). These are diagnostic single-run timings, but the additional checkpoint work is a material cost. This is not a recommendation to hard-code 1,000 rows as the production default. At 100K with the old reader held through two cycles, E4 gets worse: observed peak rises from 150.10 MB at phase-end cadence to 434.49 MB at the frequent cadence (22.41× the compact original). Its conservative oldest-reader rule retains intervening page versions. SQLite’s frequent-cadence peak is 464.84 MB. No finite safe expansion factor for an indefinitely pinned reader is established by this finite test. Full timings are recorded below.

## Experimental second publication of the same durable root

An additional E4 checkpoint can publish the same already-durable root into the other metadata slot. Both slots then point at that tree, allowing the older distinct tree to become reusable when readers permit it. The ordinary policy can retain roughly three tree-sized allocations (current, fallback, next writer); this experiment can reuse roughly two for constant-size updates. It changes no format and **is not enabled by default**. It costs a second checkpoint’s barriers and freelist publication; it does not solve old readers pinning intermediate versions or enforce a quota.

A focused test runs repeated updates with readers, checks the two-tree page-count bound, corrupts the newest metadata slot, and reopens with every latest value intact from the other slot. This is one metadata-corruption test, not a power-loss proof for a new default policy.

| Rows | Checkpoint cadence | Engine | Observed peak MB | Allocated peak MB | Bound MB | Peak / common original | Final MB |
|---|---|---|---:|---:|---:|---:|---:|
| 100,000 | Phase end | e4 | 44.287 | 45.191 | 44.287 | 2.28× | 38.763 |
| 100,000 | Phase end | sqlite | 141.905 | 156.094 | 141.905 | 8.10× | 17.519 |
| 400,000 | Phase end | e4 | 172.345 | 174.330 | 172.345 | 2.30× | 150.192 |
| 400,000 | Phase end | sqlite | 569.918 | 592.548 | 569.918 | 8.03× | 70.963 |
| 100,000 | 1,000 ops | e4 | 24.663 | 24.900 | 24.663 | 1.27× | 24.475 |
| 100,000 | 1,000 ops | sqlite | 21.671 | 22.315 | 21.671 | 1.24× | 17.519 |
| 400,000 | 1,000 ops | e4 | 81.432 | 84.439 | 81.432 | 1.08× | 81.242 |
| 400,000 | 1,000 ops | sqlite | 75.116 | 88.773 | 75.116 | 1.06× | 70.963 |

SQLite remains a single normal checkpoint per boundary; the second publication is an E4-specific experimental cost. Logical documents and verification are identical.

## Rebuild peak includes the old file

Only the long-snapshot arms are rebuilt. E4 uses source-preserving `recover_to`; SQLite uses `VACUUM INTO`. Exact rows are verified in each new database and source fingerprints remain unchanged. Nothing is automatically installed over its source.

| Rows | Engine | Source MB | Observed simultaneous peak MB | Allocated peak MB | Rebuilt database MB | Peak / original |
|---|---|---:|---:|---:|---:|---:|
| 100,000 | e4 | 132.705 | 169.660 | 172.192 | 19.599 | 8.75× |
| 100,000 | sqlite | 32.903 | 48.801 | 49.521 | 15.897 | 2.79× |
| 400,000 | e4 | 511.781 | 659.880 | 669.446 | 78.340 | 8.79× |
| 400,000 | sqlite | 132.768 | 196.376 | 197.894 | 63.607 | 2.77× |

These maintenance observations are sampled; no strict scratch-space upper bound was implemented. In particular, quoting only the rebuilt output omits the space needed while keeping the source safe.

## Timing and validation

Times are single-run diagnostics on the same machine/volume. The sampler and bound calculation are included in mutation times; reopen and exact typed verification are excluded. Some runs overlapped regression tests/compilation; benchmark arms themselves ran sequentially. Do not interpret these as isolated speed rankings.

| Run | Rows | Workload | Engine | Load s | Six-cycle mutation s |
|---|---:|---|---|---:|---:|
| fixed | 100,000 | load | e4 | 2.19 | 0.00 |
| fixed | 100,000 | load | sqlite | 2.81 | 0.00 |
| fixed | 100,000 | updates | e4 | 2.16 | 3.66 |
| fixed | 100,000 | updates | sqlite | 2.81 | 5.34 |
| fixed | 100,000 | delete_reinsert | e4 | 2.04 | 2.69 |
| fixed | 100,000 | delete_reinsert | sqlite | 2.82 | 4.00 |
| fixed | 100,000 | mixed_none | e4 | 2.08 | 5.94 |
| fixed | 100,000 | mixed_none | sqlite | 2.80 | 8.28 |
| fixed | 100,000 | mixed_short | e4 | 2.08 | 6.24 |
| fixed | 100,000 | mixed_short | sqlite | 2.80 | 7.46 |
| fixed | 100,000 | mixed_long | e4 | 2.12 | 6.02 |
| fixed | 100,000 | mixed_long | sqlite | 2.79 | 8.10 |
| fixed | 400,000 | load | e4 | 9.81 | 0.00 |
| fixed | 400,000 | load | sqlite | 15.44 | 0.00 |
| fixed | 400,000 | updates | e4 | 11.14 | 15.13 |
| fixed | 400,000 | updates | sqlite | 16.58 | 23.09 |
| fixed | 400,000 | delete_reinsert | e4 | 10.12 | 10.01 |
| fixed | 400,000 | delete_reinsert | sqlite | 24.04 | 16.21 |
| fixed | 400,000 | mixed_none | e4 | 10.64 | 24.94 |
| fixed | 400,000 | mixed_none | sqlite | 15.23 | 33.39 |
| fixed | 400,000 | mixed_short | e4 | 10.03 | 26.91 |
| fixed | 400,000 | mixed_short | sqlite | 16.41 | 40.26 |
| fixed | 400,000 | mixed_long | e4 | 12.91 | 28.63 |
| fixed | 400,000 | mixed_long | sqlite | 16.66 | 33.10 |
| frequent-load-100k | 100,000 | load | e4 | 7.47 | 0.00 |
| frequent-load-100k | 100,000 | load | sqlite | 5.66 | 0.00 |
| frequent-load-400k | 400,000 | load | e4 | 42.92 | 0.00 |
| frequent-load-400k | 400,000 | load | sqlite | 34.61 | 0.00 |
| frequent-mixed-100k | 100,000 | mixed_none | e4 | 7.43 | 23.28 |
| frequent-mixed-100k | 100,000 | mixed_none | sqlite | 5.72 | 19.98 |
| frequent-mixed-400k | 400,000 | mixed_none | e4 | 43.11 | 119.08 |
| frequent-mixed-400k | 400,000 | mixed_none | sqlite | 34.88 | 104.84 |
| frequent-updates-100k | 100,000 | updates | e4 | 9.58 | 15.91 |
| frequent-updates-100k | 100,000 | updates | sqlite | 6.17 | 13.32 |
| frequent-updates-400k | 400,000 | updates | e4 | 46.70 | 96.59 |
| frequent-updates-400k | 400,000 | updates | sqlite | 36.88 | 72.34 |
| double-updates-100k | 100,000 | updates | e4 | 2.17 | 3.96 |
| double-updates-100k | 100,000 | updates | sqlite | 2.89 | 5.30 |
| double-updates-400k | 400,000 | updates | e4 | 10.27 | 15.33 |
| double-updates-400k | 400,000 | updates | sqlite | 15.69 | 21.30 |
| double-frequent-100k | 100,000 | updates | e4 | 9.40 | 17.51 |
| double-frequent-100k | 100,000 | updates | sqlite | 5.72 | 12.43 |
| double-frequent-400k | 400,000 | updates | e4 | 50.30 | 90.85 |
| double-frequent-400k | 400,000 | updates | sqlite | 35.75 | 83.32 |
| frequent-long-100k | 100,000 | mixed_long | e4 | 7.55 | 20.75 |
| frequent-long-100k | 100,000 | mixed_long | sqlite | 6.13 | 16.58 |

The retained runs verify **458 current states (106.88M row comparisons)** and **88 snapshot states (20.8M row comparisons)**, plus rebuilt outputs. Full feature-enabled release workspace: **297 passed, zero failed/ignored**. The later focused policy suite passes both reader-release and duplicate-publication tests, adding one new regression: **298 distinct passing tests**. Retained-evidence checker validates all paired phase checksums, exact row oracles, snapshots, even-cycle E4 page accounting, maintenance source fingerprints and red/green regression evidence. Both focused reader/policy tests also pass with default features and with debug assertions. Two actual SIGKILL probes (committed overflow replacement before checkpoint and after checkpoint) each reopen with all 1,000 expected rows. This loop does not claim exhaustive power-loss/ENOSPC coverage.

## Decision and remaining work

1. Keep the measured reader-release fix. It is a bounded change at the commit boundary and reduces avoidable allocation after readers close.
2. Keep interfaces paused while selecting a bounded checkpoint policy and enforcing its budget. Phase-only checkpointing fails the proposed 2× budget; the frequent-checkpoint controls show which costs are avoidable without changing the storage format. Passing a measured workload is not an enforced cap.
3. Prefer resource-aware checkpoint cadence as the first operational control: a row-count interval is only a benchmark proxy, so production must account for bytes, dirty pages and pinned readers. Evaluate the optional repeated-root publication against durability/write-cost requirements before selecting a default; do not equate two tree allocations with a twofold total-store cap, because WAL and metadata also need space.
4. A hard budget needs admission/reservation before writes, including WAL, dirty-page flushes, metadata and recovery headroom. When a reader prevents reclamation, refuse or pause writes before the budget is exhausted; preserve committed rows and that reader. This API and its failure/reopen tests remain unimplemented.
5. Long-lived-reader retention still needs finer version-lifetime accounting; safe tail truncation/interior relocation and the rebuilt packing gap remain separate work. A full rebuild is not the default answer to ordinary churn.

## Reproduction and retained evidence

```sh
cargo build --release --offline --features sqlite-balance,compact-cells --bin lifecycle
cargo run --release --offline --features sqlite-balance,compact-cells --bin lifecycle -- --space-check <scratch>
# Optional arguments: rows case checkpoint_operations e4_publications
cargo run --release --offline --features sqlite-balance,compact-cells --bin lifecycle -- --space-check <scratch> 400000 updates 1000 2
python3 tools/check_peak_space.py <scratch>
```

Cases: `load`, `updates`, `delete_reinsert`, `mixed_none`, `mixed_short`, `mixed_long`. Use a fresh directory. `checkpoint_operations=0` means phase-end only. Publication count defaults to one and only affects E4. Benchmark helpers remain outside the production interface.

Evidence root: `<scratch>/`. Raw stage JSON/logs, before/after executables, source snapshots/patch, test logs, summary and cleanup manifest are retained there. [Compact results](PEAK_SPACE_RESULTS.json). tracker: `peak-space-loop`. The seven-law `CONTRACT.md` hash remains `3b89f8b0c170d71df225b5c566d5289fe97f05220a1a371f791dc39da35524c1`.

## Cleanup

After evidence verification, removed **6,033,036,149 logical bytes (6.033 GB)** from 50 generated large benchmark directories. Retained all raw results/logs, source and executables, maintenance manifests, and small smoke/crash fixtures. Exact paths are in `cleanup.json` and `cleanup-plan.json`. The retained-evidence checker passes without those databases. Logical removed bytes are not a promise of immediate APFS free-volume return.
