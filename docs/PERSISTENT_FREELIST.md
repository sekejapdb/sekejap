# Persistent freelist exploration — 2026-09-12

The preceding in-file allocator prototype reduced commit time but consumed
bounded retirement entries and delayed overflow-size stabilization. This
experiment instead reuses the existing `free` file. It adds no database pages,
changes no file format and leaves allocator capacity/plateau tests unchanged.

## Publication and failure boundaries

Ordinary checkpoint still synchronizes data pages, then publishes and
synchronizes the new root. Only afterward does it overwrite the freelist file,
truncate its stale tail and synchronize its contents. The old hint's generation
is already invalid for the newly published root. Creation synchronizes the
directory; subsequent writes retain the same filename. WAL truncation still
synchronizes the WAL, without repeating a directory barrier on every checkpoint.
Writer reopen synchronizes the directory once because opening may recreate a
missing WAL. Create and recovery keep their filename-publication obligations.

Loss, a short write, a torn body or an intact stale hint discards reuse knowledge
through the existing generation/whole-body CRC/bounds validation. Current rows
do not depend on that hint. A fallback root also rejects newer-generation hints.
Recovery still uses the existing independently verified temporary-file and
rename protocol when renumbering data; this experiment does not change it.

Sacrifices remain explicit: rewriting the complete freelist still costs in
proportion to tracked free pages, and a lost/corrupt hint can leak reusable space
until explicit repair/rebuild. This is not a delta allocator or proof of all
seven laws. Existing data/root FULL barriers, freelist `sync_all`, and durable
WAL truncation are retained; the experiment removes recurring filename work.

## Source inspiration

Retained SQLite commit `f3b9f74d81132426dee1ccc07a67fdad2ccfeaa9`:
`src/pager.c:2123` retains a journal inode in TRUNCATE/PERSIST modes, explicitly
synchronizing truncation in FULL mode; `src/os_unix.c:6546` associates journal
creation with a directory-sync obligation. This informs filename reuse, not a
port of SQLite's transaction protocol. The comparator remains native SQLite WAL
mode with its normal 1000-page auto-checkpoint.

Retained PostgreSQL commit `a11bce64a3be38f726cdca682f1e5b723cfd34d1`:
`src/backend/storage/freespace/README:168` distinguishes free-space hints from
row durability. E4 retains its own stricter generation and checksum validation;
it does not implement PostgreSQL's self-correcting FSM tree.

## Gates and experiment layout

Before implementation, a test holding an open `free` descriptor failed against
accepted E4 because the next checkpoint replaced that file. The corruption-image
test passed on the baseline. Candidate checks cover descriptor reuse with a held
snapshot, reopen/reuse, missing/truncated/mixed/stale hints, and fallback rows.
Original four-page resource admission and overflow plateau tests are unchanged.

Acceptance requires the complete workspace suite, no relaxed disk/capacity
thresholds, and repeatable gains of at least 10% on a named workload against
accepted E4, reproduced on the Pi. All timing tables show accepted E4,
experimental E4 and SQLite separately. Small-commit cases use two reversed-order
Mac repetitions; the 400K and held-reader cases are single runs. Pi timing is
under a common 128 MiB address-space limit with existing services still running.

The 400K case loads 400,000 hybrid people into two collections, then performs
12 rounds of 80,000 updates, 40,000 deletes and 40,000 replacement inserts:
1,920,000 mutations, committed in batches of 1000. Each round ends with 400,000
live records. Scalar fields, binary JSON, points, external keys and four exact
f32 vector lanes have the same logical values in all engines. Timestamps are off.
All engines use 8 MiB caches, buffered I/O and their native FULL sync settings.

Load and mutation timings include commits and ending checkpoints, excluding
correctness verification. Every current/reopened row is checked; held snapshots
must retain the initial corpus. SQLite also runs integrity checks. Held-reader
SQLite times include its checkpoint busy waits and are not raw write throughput.
Logical/allocated peaks include every database file; 1 ms samples are observed
lower bounds, not enforced caps.

Artifacts: `<scratch>/`.
Isolated source: `/tmp/e4-reuse-candidate`. Production code remains unchanged
until the verdict is recorded below. The seven laws remain unchanged.

## Verdict: retained in E4

The change is retained for its repeatable small-commit benefit: **30.3% on Mac
and 28.9% on Pi against accepted E4 before this loop**. The two individual
small-commit gains exceed 10% on each machine. It does not establish SQLite
write-speed parity. Pi 400K mutation time improves only 4.4%.

All paired E4 final logical sizes are identical. Candidate sampled logical
peaks are slightly lower because ordinary checkpoints no longer hold `free`
and `free.tmp` together. No extra data pages or retirement entries are added.

### Mac

| Workload | E4 before load / mutations s | E4 after load / mutations s | SQLite load / mutations s | Mutation reduction vs E4 before |
|---|---:|---:|---:|---:|
| batch-1 | 30.615 / 24.829 | 22.115 / 17.298 | 7.158 / 5.679 | 30.3% |
| batch-100 | 3.359 / 4.115 | 2.557 / 3.209 | 0.871 / 1.147 | 22.0% |
| batch-1000 | 5.464 / 8.319 | 4.592 / 6.918 | 1.494 / 3.216 | 16.9% |
| mixed-400000 | 21.721 / 104.578 | 18.823 / 87.304 | 5.839 / 38.426 | 16.5% |
| held-100000 | 5.551 / 8.461 | 4.611 / 6.965 | 1.510 / 24.518 | 17.7% |

| Workload | E4 before peak / final MiB | E4 after peak / final MiB | SQLite peak / final MiB |
|---|---:|---:|---:|
| batch-1 | 0.481 / 0.481 | 0.481 / 0.481 | 4.230 / 0.348 |
| batch-100 | 3.733 / 3.731 | 3.732 / 3.731 | 6.976 / 2.973 |
| batch-1000 | 35.180 / 35.174 | 35.174 / 35.174 | 33.961 / 29.258 |
| mixed-400000 | 139.596 / 139.589 | 139.589 / 139.589 | 121.840 / 117.129 |
| held-100000 | 65.883 / 65.787 | 65.787 / 65.787 | 149.284 / 29.477 |

### Pi

| Workload | E4 before load / mutations s | E4 after load / mutations s | SQLite load / mutations s | Mutation reduction vs E4 before |
|---|---:|---:|---:|---:|
| batch-1 | 14.640 / 11.494 | 10.802 / 8.174 | 3.554 / 2.879 | 28.9% |
| batch-100 | 2.017 / 2.598 | 1.698 / 2.271 | 0.700 / 1.193 | 12.6% |
| mixed-400000 | 27.855 / 163.089 | 26.292 / 155.995 | 8.887 / 76.565 | 4.4% |
| held-100000 | 6.751 / 11.103 | 7.143 / 10.761 | 2.148 / 24.861 | 3.1% |

| Workload | E4 before peak / final MiB | E4 after peak / final MiB | SQLite peak / final MiB |
|---|---:|---:|---:|
| batch-1 | 0.481 / 0.481 | 0.481 / 0.481 | 4.230 / 0.348 |
| batch-100 | 3.733 / 3.731 | 3.732 / 3.731 | 6.976 / 2.973 |
| mixed-400000 | 139.596 / 139.589 | 139.589 / 139.589 | 121.840 / 117.129 |
| held-100000 | 65.883 / 65.787 | 65.787 / 65.787 | 149.284 / 29.477 |

Batch-1 = 1,000 initial people, two rounds of 400 mutations, one operation per
commit. Batch-100 = 10,000 people, three rounds of 4,000 mutations. Batch-1000
= 100,000 people, four rounds of 40,000 mutations. Each round updates 20%,
deletes 10% and inserts 10%; comparisons are within a workload. Both platforms
use two reversed-order repetitions for batch-1/batch-100; Mac also repeats
batch-1000. 400K and held-reader results are single runs.

At 400K, E4 after takes **2.27× SQLite time on Mac / 2.04× on Pi**. Pi held-case
initial load was 7.143 s after versus 6.751 s before, a 5.8% regression in that
single run; its cause is not established. The shared Pi is not an idle dedicated
benchmark device. Do not generalize the small-commit gain to all workloads.

Pi 400K maximum RSS is 10.70 MiB before / 10.75 MiB after / 12.11 MiB SQLite
under the common 128 MiB address-space limit. These are measured process peaks,
not filesystem-cache measurements or whole-process allocation-ledger proofs.
Mac allocated size can exceed logical size and fluctuate between runs; exact
allocated peaks and issued bytes remain in the result JSON.

**347 distinct Mac checks pass**: the complete 346-test workspace run plus
the subsequently added missing-WAL regression, with all 129 kernel tests rerun
and the 19 path-adjusted lifecycle/recovery checks rerun. **182 selected Pi
checks pass**, including capacity, overflow plateaus, corruption recovery,
snapshots, process-killed merge writers and the constrained embedding case.
The initial Pi runner targeted one test in the wrong package; the next run
found 19 Mac-only artifact-path guards. Both diagnostics are retained. Only
the allowed artifact paths changed; no data-size, bookkeeping, plateau or
correctness assertion was relaxed.

The new missing-WAL crash-model test was also mutation-checked: removing
its directory-publication barrier loses the acknowledged row; restoring the
barrier passes. Lost/torn freelist checks model persisted crash images, not
physical power-cut testing. A lost hint still sacrifices reuse knowledge.
Writer reopen now pays one directory barrier, including when no WAL recreation
was needed; repeated-open latency was not separately benchmarked.

All 42 benchmark arms pass complete row/reopen oracles, with 96 three-way state comparisons.

Raw reports: [Mac](REUSE_RESULTS.json), [Pi](REUSE_PI_RESULTS.json).
Source/binary fingerprints and exact promoted files: [provenance](REUSE_PROVENANCE.json).
The benchmark batch-size override remains in the archived experimental harness;
production benchmark source is unchanged. No service, SQL interface, deployment
or seven-law contract was changed.

## Evidence retention and cleanup

Removed only 21 audited disposable Mac database directories (401,931,400
logical / 436,686,848 allocated bytes) and 15 Pi directories
(193,110,168 logical / 193,150,976 allocated bytes).
Total allocated storage released: 629,837,824 bytes.
All three 400K comparisons remain on each machine, along with reports, logs,
source archives and binaries. See [Mac cleanup](REUSE_CLEANUP.json) and
[Pi cleanup](REUSE_PI_CLEANUP.json).

The verified final experimental archive is `candidate-final-source.tar.gz`,
SHA256 `aa99d6c1a591ab936fb41d4c1f0f138ca5002c00fc4d0ef8395b11a031123182`.
It includes the identical batch-size benchmark harness for reproduction.
`candidate-source.tar.gz` preserves the earlier Mac-only test-path guards for
diagnostic history. The accepted production archive is recorded separately in
REUSE_PROVENANCE.json because its benchmark harness stays unchanged.

To reproduce, extract the final candidate archive into a fresh isolated source
directory; build `collections` with `--release --offline --features
sqlite-balance,compact-cells`. Place that binary and the retained accepted
`baseline` binary under a new scratch artifact directory with a `tmp` child.
Run `tools/run_reuse_experiment.sh` with `REUSE_ARTIFACT_ROOT` naming that fresh
directory. The script refuses an existing matrix. Pi setup uses
`tools/reuse_pi.sh` and `tools/reuse_pi_matrix.sh`, the retained isolated
toolchain/vendor and the same final candidate archive. All database paths must
stay within the authorized scratch or Pi artifact root.
