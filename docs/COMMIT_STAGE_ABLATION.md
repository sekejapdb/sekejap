# Commit-stage ablation — 2026-09-13

Storage foundations only: no query execution, SQL, or new multimodel indexes.
The accepted persistent-freelist engine is the baseline. No production engine
file is changed during exploration. Seven laws and timestamp policy unchanged.

## Hypothesis and acceptance gate

Measure data-page writes and barriers, root publication, reuse bookkeeping,
freelist writing/sync, and WAL reset separately in a diagnostic build. Its
per-stage logging and timing overhead exclude it from performance comparisons.
Nested measurements must not be added twice. Instrumentation is archived and
removed before building a candidate.

SQLite source retained at commit `f3b9f74d81132426dee1ccc07a67fdad2ccfeaa9`,
`src/pager.c:2123`, skips TRUNCATE journal finalization when `journalOff == 0`;
otherwise FULL mode still syncs the truncation. This suggests an E4-specific
ablation: a checkpoint whose WAL never left its memory buffer has no physical
WAL content to truncate. E4's data and root barriers remain mandatory.
SQLite's benchmark uses native WAL mode; this source reference is inspiration,
not a claim that its transaction protocol matches E4's.

The candidate must conservatively track attempted physical writes, including
partial failed writes. `flushed == 0` alone does not prove an empty file.
Reopened nonempty logs and failed resets retain their truncation/barrier
obligation. Creation, recovery and filename durability remain unchanged.
Named cost: one per-WAL state flag and associated failure-state reasoning;
no persisted format, page, cache or transaction-size changes.

Only retain a repeatable >=10% relevant gain against current E4, with SQLite
beside both engines, unchanged correctness/recovery/snapshot/resource/peak
gates and Pi confirmation. Otherwise archive and revert. Run load-only,
updates, mixed changes and small commits with common 8MiB cache, native FULL
durability, timestamps off and identical transaction sizes within each case.
Verify full rows and reopened state; report logical and allocated peaks.
Observed 1ms sampled peaks are lower bounds, not enforced filesystem caps.

Artifacts: `<scratch>/`.
Isolated source: `/tmp/e4-stage-candidate`.
tracker task: `commit-stage-ablation`, journey `sekejap-e4`.

## Diagnostic stage breakdown

These totals include load, mutation rounds, setup and final schema publication.
They are diagnostic elapsed time, not CPU time or uninstrumented benchmark
results. Each run uses 8MiB cache and FULL durability.

| Disjoint checkpoint stage | 1K people, one operation/commit s | 100K people, 1K operations/commit s |
|---|---:|---:|
| Data flush and barrier | 14.348706 | 3.269037 |
| Root write and barrier | 13.059332 | 1.783044 |
| Reader/reuse refresh and freelist encoding | 0.150600 | 0.004320 |
| Freelist file write and sync | 14.242453 | 1.799049 |
| WAL reset | 0.488464 | 0.019954 |

Nested measurements: data/root file barriers alone total **26.530 s / 4.849 s**;
freelist sync alone totals **12.368 s / 1.738 s**. These measurements overlap
the table; the page-barrier totals also include initialization's extra flush.
They must not be added to the table totals. The WAL-reset subtotal is only
**1.15% / 0.29%** of measured checkpoint stages, even before row-mutation work.
This makes a 10% end-to-end gain from the reset shortcut unlikely.

The profile identifies expensive barrier stages; it does not establish that
any data/root barrier can safely be removed. Retaining the existing freelist
durability also leaves its barrier obligation in place. No sync setting was
weakened. A future metadata publication design would need separate proof.

Focused mechanism test: baseline issued one truncate and one sync for a
buffer-only WAL and failed the zero-I/O expectation. Its three safety tests
passed. Candidate kernel run passes all **133** tests, including buffer-only
reset, spilled/reopened durable truncation, partial-write failure and failed
truncate/sync retry. These use a visible/durable file model, not a hardware
power-cut claim.

## Verdict: rejected and reverted

No workload reaches the 10% mean improvement gate. The empty-WAL reset
shortcut is rejected; the accepted persistent-freelist engine remains
unchanged. This is a negative performance result, not an observed
correctness failure. No Pi timing or larger acceptance matrix followed
the failed Mac gate. No claim of equivalent Pi gains is made.

| Workload | Current E4 load / changes s | Candidate load / changes s | SQLite load / changes s | Candidate change-time reduction |
|---|---:|---:|---:|---:|
| load | 5.218 / 0.000 | 5.106 / 0.000 | 1.563 / 0.000 | 2.14% (load) |
| updates | 4.813 / 3.205 | 4.728 / 3.177 | 1.449 / 1.968 | 0.88% |
| mixed | 4.828 / 7.238 | 5.625 / 7.996 | 1.595 / 3.414 | -10.48% |
| small | 22.240 / 17.399 | 22.109 / 17.109 | 7.682 / 5.935 | 1.66% |

Mean of two repetitions with engine order reversed. Negative reductions
mean slower. All timings are whole-phase seconds, including commits and
ending checkpoints, excluding full-row/reopen verification. Load-only:
100K initial people, zero mutations. Updates: 100K people, 80K total
replacements. Mixed: 100K people, 80K updates + 40K deletes + 40K inserts.
Small: 1K people, 400 updates + 200 deletes + 200 inserts, one operation
per commit. All other cases use 1000 operations per commit.

These are two repetitions on a shared Mac. In particular, candidate mixed
time varies from 7.283 to 8.709 seconds; the measured slowdown does not isolate
its cause. Neither repetition demonstrates a mixed-workload improvement.
No timing win is inferred from the small positive means in other cases.

| Workload | Current E4 peak / final MiB | Candidate peak / final MiB | SQLite peak / final MiB |
|---|---:|---:|---:|
| load | 30.957591 / 30.707355 | 30.957591 / 30.707355 | 27.395088 / 23.281250 |
| updates | 33.085453 / 33.085384 | 33.085453 / 33.085384 | 32.098953 / 27.187500 |
| mixed | 35.173855 / 35.173580 | 35.173855 / 35.173580 | 33.961021 / 29.257812 |
| small | 0.480701 / 0.480633 | 0.480701 / 0.480633 | 4.230019 / 0.347656 |

These are sampled logical peaks and final file sizes. Allocated peaks and
each repetition remain in the [raw results](COMMIT_STAGE_RESULTS.json).
Sampling variation is not an enforced maximum expansion guarantee.

**161 selected tests pass**: 133 kernel tests (including four new reset
tests), 5 durability, 2 persistent-freelist, 8 resource-limit, 6 snapshot
and 7 collection tests. All 24 benchmark arms pass full row/reopen
oracles and all 36 three-way state comparisons agree. This rejected
candidate did not run the full shipping suite or physical power cuts.

The significant finding is where time goes: data/root and freelist
barriers dominate; avoiding an already-empty WAL reset cannot remove
much elapsed time. Any future reduction in barrier count needs an
independently validated publication design, not a weaker sync setting.

## Retention and cleanup

All binaries, source archives, profile stages, logs and reports remain in
the scratch artifact directory. The first mixed run retains current E4,
candidate and SQLite databases. Removed 21 other matrix databases and
two diagnostic databases: **526,221,312 allocated bytes**
(file block accounting; actual filesystem free-space changes may differ).
See [cleanup](COMMIT_STAGE_CLEANUP.json) and
[source/binary provenance](COMMIT_STAGE_PROVENANCE.json).

## Reproduction

The retained `baseline` and `candidate` executables use the identical collection
benchmark harness. To rebuild the candidate, extract `candidate-source.tar.gz`
into a fresh isolated source directory and run `cargo build --release --offline
--features sqlite-balance,compact-cells --bin collections`. The diagnostic build
has its own `profile-source.tar.gz`; it must not be used as a timing arm.

For each engine, run `collections OUTPUT ENGINE ROWS off CYCLES CASE none` with
`TMPDIR` and `SQLITE_TMPDIR` under a fresh scratch artifact directory. Set
`COLLECTION_BATCH=1000` for `(ROWS,CYCLES,CASE)` values `(100000,0,load)`,
`(100000,4,updates)` and `(100000,4,mixed)`; use `COLLECTION_BATCH=1` with
`(1000,2,mixed)`. `ENGINE` is `e4` for either E4 binary and `sqlite` for the
SQLite comparator built into the baseline binary. Give each arm a distinct
output directory, run serially, then repeat in reversed engine order.

The retained Python runner records the exact argument matrix, source/binary
hashes and independent three-way report checks. Its setup paths are fixed to
this historical run and intentionally refuse reuse; change them to fresh
authorized artifact/source paths before invoking it again.
