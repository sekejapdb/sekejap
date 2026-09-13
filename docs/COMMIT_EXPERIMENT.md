# Commit coalescing experiment — 2026-09-12

The user requested trying a commit optimization and reverting it if the gain
was not significant. Acceptance was set before timing: a repeatable 10%
end-to-end improvement, with no material regressions or weaker durability.
The experiment runs in isolated source copies; the accepted E4 implementation
is never replaced during evaluation.

## Hypothesis and source inspection

The earlier 400K profile spent about 54% of main-thread samples in collection
commit call sites. Data and metadata barriers, freelist publication and the
directory barrier dominate those stacks. This is sampled evidence from the
preceding loop, not an exact phase timer for this candidate.

Sources inspected at the commits recorded in WRITE_PATH_PROVENANCE.json:

- SQLite `src/wal.c:2350–2420` writes checkpoint pages through a page iterator
  and preserves WAL-before-data synchronization. Its Unix VFS
  `src/os_unix.c:3900–3957` performs directory synchronization when flagged.
- PostgreSQL `src/backend/storage/buffer/bufmgr.c:2950–3030` gathers dirty
  checkpoint buffers and sorts them to reduce random I/O. Its
  `IssuePendingWritebacks` combines consecutive writeback requests; that is
  distinct from combining the actual page write calls tested here.

E4 replaces a freelist file at every checkpoint, so its directory barrier
cannot simply be removed by copying SQLite's one-time flag. This experiment
instead sorts dirty pages within windows of 64 cache frames and combines
adjacent pages into one aligned write. Each page retains its CRC and generation.
All data, metadata, freelist, WAL and directory barriers remain unchanged.

Named costs: extra copying and sorting; up to 256 KiB temporary aligned scratch
plus 64 frame descriptors. This scratch is outside the current pool reservation
ledger. The prototype is not a shipped quota or whole-process memory guarantee.
Combining only within each window bounds scratch and sorting work, but misses
adjacent pages that lie in different windows. Eviction writes are unchanged.

## Validation and measurement

A new regression independently reads and validates 32 written pages and checks
their one Full barrier. Baseline uses 32 writes and fails the one-write assertion;
candidate uses one write and passes. The candidate passes 129 kernel unit tests.
One existing fault matrix was updated from three physical write positions to
two because coalescing removes a write call; its refusal, poisoning, old-reader
and reopen assertions remain in place. These are preliminary experiment tests,
not the complete acceptance suite for shipping a kernel change.

Both binaries use the same extended collection harness. Only batch size and
the minimum fixture size change: batches 1/100/1000 use 1K/10K/100K people and
2/3/4 mixed-churn cycles, respectively. Each phase verifies every row against
the independent corpus, and every arm checks reopened contents. Two collections
contain external keys, typed scalar/JSON/point fields and four exact f32 vector
lanes. Updates and delete/reinsert together issue 0.4 operations per initial row
per churn cycle. Load and churn are reported separately.

Three repetitions rotate the order of baseline E4, candidate E4 and SQLite.
Each arm starts fresh. Cache is 8 MiB; I/O is buffered; sync is Full, with native
macOS strong barriers. SQLite retains native WAL auto-checkpoint=1000. No change
to transaction size or sync settings is used to manufacture a candidate win.
Batch sizes use different corpus sizes, so compare engines within each row,
not absolute times between rows. Timings exclude oracle verification and include
phase-ending checkpoint work; they are aggregate phase times, not p95 latency.
Disk peaks include all files and alteration, sampled at 1 ms (lower bounds).

Artifacts: `<scratch>/`.
Baseline source: the accepted `write-path-20260912/write-final-source.tar.gz`.
Scripts: `tools/run_commit_experiment.sh`, `tools/record_commit_experiment.py`.

## Decision: rejected and reverted

The third batch-1000 candidate run has unusually slow early churn cycles
(6.275 / 3.985 / 2.877 / 2.088 seconds), versus roughly two seconds per cycle
for baseline. No simultaneous host tracing established its cause, so the mean
27.4% slowdown must not be attributed entirely to the implementation. The run
is retained in the table. The first two batch-1000 comparisons already show
no win (candidate 3.54% and 0.05% slower), so excluding this outlier would not
change the rejection. Across the nine paired churn comparisons, the best
candidate improvement is only 1.08%; eight comparisons are slower.

All mean improvements fall below the predeclared 10% threshold. The candidate
was reverted in both isolated checkouts; production source never changed.
No Pi run or broader shipping suite was justified after this negative performance gate.

| Batch / rows | Baseline load / churn s | Candidate load / churn s | SQLite load / churn s | Candidate churn improvement |
|---|---:|---:|---:|---:|
| 1 / 1,000 | 31.410 / 24.366 | 31.229 / 24.711 | 7.474 / 5.878 | -1.41% |
| 100 / 10,000 | 3.380 / 4.229 | 3.491 / 4.390 | 0.874 / 1.106 | -3.82% |
| 1000 / 100,000 | 5.537 / 8.625 | 5.712 / 10.992 | 1.505 / 3.191 | -27.43% |

| Batch | Baseline peak MiB | Candidate peak MiB | SQLite peak MiB |
|---|---:|---:|---:|
| 1 | 0.481 | 0.481 | 4.230 |
| 100 | 3.733 | 3.733 | 6.976 |
| 1000 | 35.180 | 35.180 | 33.961 |

Means of three repetitions; positive improvement means faster. Per-run times,
logical/allocated peaks and file sizes are in [audited results](COMMIT_EXPERIMENT_RESULTS.json).

All 27 arms and 45 three-way state comparisons passed.

The one-write microtest is a real syscall improvement, but unchanged barriers
still dominate the complete operation. Avoiding unnecessary work must be judged
at the collection boundary, not only inside the page writer.

Removed 24 audited disposable database directories: 245,569,456 logical bytes / 250,081,280 allocated bytes. Main 100K comparisons and evidence remain.

See [cleanup](COMMIT_EXPERIMENT_CLEANUP.json) and [source/revert provenance](COMMIT_EXPERIMENT_PROVENANCE.json).
