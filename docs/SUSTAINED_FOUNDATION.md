# Sustained SQLite foundation loop

This measurement pass completed on 2026-09-11 Melbourne time, after starting
on 2026-09-10. **Overall foundation parity is still open; SQL and interfaces
remain paused.** E4 improves snapshot-space reuse and typed read speed, but
retains more space and is slower on several mutation workloads. Neither the
prior fresh-load density result nor an observed peak establishes a capacity cap.

## Experiment contract

All database and scratch files: `<scratch>/`.
Identical deterministic mixed people rows, 1000-operation transactions, 4096-byte
pages, 8 MiB configured writer budgets, 64 KiB snapshot budgets, Buffered I/O,
FULL durability and macOS fullfsync. The inherited E4 budget assigns two thirds
to page frames; SQLite's cache_size specifies its page cache. These are equal
configured amounts, not a claim of identical resident memory. No timestamps,
vectors, graph edges or secondary indexes in either engine.

Writers stay open across 12 cycles. Full typed current-row checks run after
every phase, old snapshots are checked while held, and exact reopen checks run
at the end. Cases: fixed-length updates, mixed update/delete/grow/shrink, and
mixed churn with the initial snapshot held through two cycles. A separate
regression holds multiple snapshots for 24 publications and reopens the writer.
Verification warms the OS cache between phases. Engine cache remains fixed;
this is not a cold-volume benchmark. Benchmarks run sequentially without
compilation or test suites overlapping their timing windows.

Three repeated pairs alternate order E4/SQLite, SQLite/E4, E4/SQLite. SQLite
uses its native `wal_autocheckpoint=1000`, plus matched phase-boundary PASSIVE
checkpoints with a reader, TRUNCATE without one. Baseline E4 uses a 4 MiB WAL
threshold plus phase boundaries. Candidate accounts for copied/allocated
pages as well as WAL bytes. Both checkpoint thresholds are explicit experiment
configuration; no new default is accepted without the gate.

Peak observations sample all files beneath each arm, including WAL and reader
metadata, with a requested 1 ms interval. Logical lengths and allocated-file
blocks are distinct. Automatic checkpoint peaks can be missed between samples;
phase-end bounds do not prove a maximum for those intervening operations.
No APFS free-volume or enforced physical-cap claim follows from these samples.

## Variant names and snapshot schedules

- `baseline`: original kernel/codec, WAL-only 4 MiB trigger, separate commit and checkpoint.
- `candidate`: lifetime-aware reuse and combined WAL-or-page policy, original codec.
- `direct`: candidate plus direct dense-v3 codec and atomic reader publication.
- `matrix`: supplemental cases using the retained direct executable; the release-gap case uses the later harness executable, with identical production codec/kernel.

All mutation cases use 12 cycles. Fixed updates change 30% of rows each cycle;
isolated delete/reinsert deletes then reinserts 10%; mixed churn updates 20%,
deletes 10%, then reinserts that 10%. Odd mixed cycles grow selected JSON values
to 512/9,000 bytes; even cycles shrink them. Thus fixed updates perform 360K
changes per 100K-row run, delete/reinsert 240K, mixed 480K.

`mixed_none` has no snapshot. `mixed_long` holds the initial snapshot through
two cycles, then has ten unpinned cycles. `mixed_short` replaces its snapshot
every cycle with **no checkpoint in the release gap**; SQLite can checkpoint
pages but cannot reset its WAL while the new reader holds a read mark. This is
a rolling-reader workload, not a claim that every brief reader causes GB of WAL.
`mixed_short_gap` gives **both engines** a no-reader checkpoint before the next
snapshot; the extra maintenance is included in mutation time and peak sampling.
These schedules must remain separate in comparisons.

## Candidates under test

- Use verified source-page generation to identify the lifetime of shadow pages.
  Pages absent from both metadata roots and every registered reader can be
  promoted to ordinary reusable freelist entries. Freelist sidecar v2 persists
  birth generations, adding eight bytes per retired page. Row/page formats are
  unchanged. Old or invalid sidecars are rejected as derived state and can leak
  space, never authorize reuse. Reader ambiguity stops reuse. A cursor avoids
  revisiting the same retirement cohorts until readers change.
- An explicit commit policy uses both logical WAL bytes and allocated page bytes.
  When that commit publishes a checkpoint, data and metadata barriers establish
  durability, avoiding a separate preceding WAL sync. Errors must preserve the
  log and require reopening a poisoned writer.

## Gates still open

The repeated 100K comparison, paired 400K diagnostic and reader-maintenance-gap control are complete; overall parity has not passed. A true total-store disk budget needs
admission before dirty-page eviction, WAL growth and metadata publication, with
reserved replay/recovery space and all writer entry points covered. A periodic
size check or a checkpoint trigger alone is insufficient. Snapshot registration
and filesystem allocation also matter. Do not present unimplemented enforcement
as a passed gate or capacity refusals as completed throughput.

## Intermediate evidence (not a parity verdict)

The original engine failed the 24-round/two-snapshot regression at round 7:
1,890 pages from 239 initially. An in-memory lifetime prototype passed one
reopen but failed when the writer reopened every round. Persisting lifetimes
fixes both cases: 1,182 pages after 24 rounds, with every snapshot/current value
exact, including large overflow values. Corrupt/impossible birth metadata is
refused. Focused reader-release, duplicate-root fallback, byte-policy and
barrier-failure tests pass; two real SIGKILL policy paths reopen exact rows.

Completed initial candidate comparison, **three pairs each, 100K rows and 12 cycles**.
Mutation seconds are medians; peak is the largest observed logical footprint
(the three repetitions have the same reported peak at this precision). MB is decimal.

| Workload | E4 mutation s | SQLite mutation s | E4 peak MB | SQLite peak MB | E4 final MB | SQLite final MB |
|---|---:|---:|---:|---:|---:|---:|
| Fixed-size updates | 21.58 | 25.08 | 35.59 | 25.75 | 35.18 | 17.75 |
| Mixed, no snapshot | 62.04 | 44.62 | 49.68 | 40.47 | 49.03 | 32.90 |
| Mixed, snapshot held two cycles | 51.94 | 37.17 | 69.16 | 464.84 | 68.45 | 32.90 |

The old WAL-only policy was faster but used higher E4 peaks: 60.79 / 90.86 /
190.93 MB respectively. The new policy reduces those peaks by approximately
41% / 45% / 64%. It has **not** reached overall SQLite parity: no-reader mixed
churn is slower, and retained final space remains larger. Both engines show zero retained-size growth between matched even cycle ends
10 and 12. The 36,036-byte E4 difference from odd cycle 9 to even cycle 12 is
freelist variation across grow/shrink phases, not continued file growth. A
plateau in this finite workload is not a hard cap.

## Direct codec and reader publication follow-up

The allocation regression reproduced 1,902,074 bytes allocated to encode a
200,047-byte row and 1,403,577 bytes to decode it. The direct codec writes and
reads dense-v3 without reconstructing the earlier representations. Tests compare
exact row/vector bytes, 96 mixed shapes and 4,608 malformed variants against the
retained reference, and ensure invalid inline bytes never fetch external vectors.
All 308 workspace tests pass serially. Actual SIGKILL probes pass both the
committed-WAL and checkpoint publication paths. The direct encoder allocates
700,364 bytes and decoder 202,681 bytes for that same 200,047-byte row: reductions
of 63.2% and 85.6%. These are cumulative allocated bytes, not peak RSS.

A deterministic reader test reproduced a create-before-lock race: a writer could
sweep a newly created registration and leave an invisible live reader. Slots now
lock and initialize a private temporary inode before publishing it atomically,
without replacing an existing registration. A process crash before publication
can leave a small `.reader-pending-*` file; it is not a live reader registration.

After the machine restart, two full-suite tests returned OS EIO during durable
commits. Both passed in an isolated serial rerun (all four entry-density tests).
The original failure log is retained as `direct-workspace-io-error.log`; the full
serial suite subsequently passed. This does not establish a cause for the EIO.

## Completed direct-codec comparison

[All tables](SUSTAINED_TABLES.md) and [checked raw summaries](SUSTAINED_RESULTS.json)
retain every repetition, both engines, sampled allocated/logical peaks, reads,
reopen and matched cycle-end growth. At 100K / 12 cycles / three alternating pairs:

| Workload | E4 mutation s | SQLite mutation s | E4 peak MB | SQLite peak MB | E4 final MB | SQLite final MB |
|---|---:|---:|---:|---:|---:|---:|
| Fixed updates | 19.47 | 23.57 | 35.59 | 25.75 | 35.18 | 17.75 |
| Mixed, no reader | 40.86 | 36.68 | 49.68 | 40.47 | 49.03 | 32.90 |
| Mixed, initial reader held two cycles | 43.76 | 33.08 | 69.16 | 464.84 | 68.45 | 32.90 |

E4 wins fixed-update time and full typed point/scan time in these runs. It is
11.4% slower for no-reader mixed churn and 32.3% slower with the initial reader.
The before/after codec runs straddle a machine restart and wider prior timing
variance; the entire wall-time change cannot be attributed to the codec alone.
The allocation regression and exact-format equivalence isolate the buffer win.

The pure-load stage itself takes a median 5.86 s / 29.41 MB retained for E4,
versus 5.38 s / 17.55 MB for SQLite across the nine identical initial loads.
Frequent automatic checkpoints retain extra reusable CoW pages during loading.
This is a different policy from the earlier fresh bulk/phase-end density trials;
the old +2.92% result does not describe this automatic-checkpoint workload.

Measured expansion relative to each engine's post-load retained footprint is
1.21× / 1.47× for fixed updates, 1.69× / 2.31× for no-reader mixed churn, and
2.35× / 26.48× for the held-reader case (E4 / SQLite). E4's held-reader result
still exceeds the illustrative 2× target. Allocated blocks are higher than
logical lengths, and neither metric is an enforced capacity guarantee.

The new birth-generation map also consumes RAM per retired page. The inherited
freelist and its checkpoint serialization remain proportional to retained free
pages. The 8 MiB setting is a page-budget comparison, not a whole-process RAM
bound; allocator and metadata memory gates remain open. No seven-law completion
or interface/SQL readiness follows from these measurements.

## Remaining cost and next experiment

In direct no-reader repetition 1, E4 spent 36.39 of 40.82 mutation seconds
inside commit, including automatic checkpoints, plus 0.57 s in explicit
checkpoints. It issued 2.171 GB of data-page writes across 480K changes. SQLite
spent 34.54 of 36.68 s inside commit and 0.005 s in explicit checkpoints.
SQLite issued-byte counters are not instrumented; the `e4_issued_bytes` zeros
in SQLite profiles must not be interpreted as zero physical writes.

The next space experiment is [redundant root publication](REDUNDANT_CHECKPOINT_EXPERIMENT.md):
retire the second distinct metadata root sooner, with its additional durability
barrier explicitly priced. It is a proposal, not an implemented optimization.
Freelist serialization, bounded metadata RAM, and admission/recovery reservations
also remain work. No checkpoint default, weaker durability setting, extra law,
automatic timestamp default, or SQL interface was introduced by this loop.

## 400K scale diagnostic

One paired run, same 12 cycles and 1,000-operation transactions, verifies all
1.92M changes per engine. E4: **201.74 s, 161.27 MB sampled logical peak,
160.51 MB final**. SQLite: **181.06 s, 141.02 MB peak, 132.77 MB final**.
E4 remains approximately 11.4% slower; its final size premium narrows from
49.0% at 100K to 20.9% at 400K. Both engines show zero retained-size change
between even cycles 10 and 12. Both take more time per change at 400K; the
relative timing ratio alone does not establish constant per-change cost.
This is one scale diagnostic, not three-repetition evidence at 400K.

The 400K shell wrapper reported unexpected EOF after printing completion,
following an edit to the active runner. Both engine arms and final results were
already complete; the independent checker confirms all paired states and reopen
checks. The saved runner passes `bash -n`. This orchestration error is retained
here separately from the earlier OS EIO failures and storage-test outcomes.

1M/10M runs are deferred: the remaining cap, metadata-memory and snapshot-policy
gaps should be addressed before a larger resource gate. Automatic timestamps
remain OFF by default and the seven laws are unchanged.

## Rolling readers and the maintenance-gap control

At 100K / 12 cycles, rolling readers without a checkpoint in the gap produced
E4 **43.48 s / 71.39 MB peak**, SQLite **14.30 s / 2,162.72 MB peak**.
SQLite's WAL continued growing by 339.43 MB between cycles 10 and 12 even though
the current dataset had stopped growing. Closing the final connection ultimately
left a 32.90 MB database; that final size does not describe the expansion risk.

Giving **both engines** a no-reader checkpoint before each new snapshot changes
the comparison to E4 **44.02 s / 71.39 MB peak**, SQLite **15.18 s / 294.32 MB
peak**, with zero growth between matched even cycle ends. Extra maintenance is
included in those times. Final sizes are E4 70.68 MB and SQLite 32.90 MB.
These are single paired diagnostics. E4 retains a large peak-space advantage,
but is about **2.9× slower** in this snapshot-heavy schedule. The no-reader
11% timing gap must not be generalized to all snapshot workloads.

## Validation and reproduction

- `direct-workspace-tests.log`: 308 passed, zero failures/ignored, serial workspace feature suite.
- `codec-allocation-red.log` and `reader-registration-red.log`: both defects reproduced before their fixes.
- `direct-allocations.log`: measured allocation reductions; unit tests preserve exact row/vector bytes and malformed-input acceptance against the reference.
- `version-red.log`, `reopen-lifetime-red.log`, focused reuse/policy logs: lifetime reuse, repeated reopen and corrupt sidecar evidence.
- `direct-policy-crashes.log`: two actual SIGKILL paths reopen exact committed data.
- `gap-smoke.log`: the later harness-only gap variant passes exact current/snapshot and paired checks before its 100K run. Production kernel/codec are unchanged after the full suite.

Primary logs and retained executables are in
`<scratch>`. Source archives retain the old
baseline and successive candidates. The pre-direct candidate snapshot includes
the no-op hook added for the reader-race red test; its retained executable is the
artifact used for the historical candidate timings. These are macOS process/I/O
tests, not hardware power-loss tests or validation on every platform.

For a fresh run directory (the runner refuses existing output):

```sh
cargo build --release --offline --features sqlite-balance,compact-cells --bin lifecycle
bash tools/run_sustained_matrix.sh target/release/lifecycle <scratch> 100000 12 'updates mixed_none mixed_long mixed_short mixed_short_gap delete_reinsert' 3 4194304 1
python3 tools/check_sustained.py <scratch>
```

`tools/record_sustained.py` regenerates the tables and checked JSON from completed
matrices. `tools/cleanup_sustained.py` validates all results before removing only
their generated arm directories, writing an exact cleanup manifest first.
Results, event logs, test/crash logs, retained binaries and source archives remain.

## Retention and cleanup

The final checker matched **1,456 current-state checks covering 144,894,000 row
visits**, plus **216 snapshot checks**, across retained baselines, candidates,
smokes and supplemental runs. These are repeated visits, not distinct entities.
All paired answers, reopen results and recorded page-accounting checks agree.

After verification, **72 generated arm directories containing 3,214,689,424
logical bytes (3.215 GB)** were removed. [Cleanup manifests](SUSTAINED_CLEANUP.json)
list exact files, lengths and result hashes. Logs, JSON results, event streams,
crash evidence, executable/source archives and [artifact hashes](SUSTAINED_ARTIFACTS.json)
remain. Allocated-block totals are recorded separately and do not assert how
much APFS volume space was reclaimed. Earlier E4 loops and E3 were untouched.

tracker journey `sekejap-e4` records this pass and keeps the broader parity, disk-budget and accessor tasks ongoing. The E3 journey was not changed.
