# Stable-page WAL foundation pilot — 2026-09-13

Status at start: isolated experiment, not promoted. Accepted collection/kernel
source remains unchanged. [F1](FOUNDATION_TEST_STANDARD.md) defines test categories;
CONTRACT.md retains exactly seven laws. There is no production data migration.

## Architectural scope

The prototype reuses the existing checksummed 4KiB B-tree pages, overflow codec,
compact cells and buffer pool. It adds a page-image WAL beneath that B-tree.
Existing E4 uses the unchanged Store path inside the same benchmark binary;
SQLite uses native WAL, FULL/fullfsync/checkpoint_fullfsync, 8MiB cache and a
WITHOUT ROWID (BLOB key, BLOB value) table. This is a raw KV comparison, not
typed collections, external-key mappings, vectors, graph or SQL implementation.

Stable logical page numbers eliminate snapshot-driven relocation of tree paths.
Each physical page image and commit frame is checksummed, with transaction
sequence and a transaction checksum. The writer's latest-page lookup is bounded
by a 16MiB WAL cap. A snapshot holds the last committed lookup and logical extent;
its reads do not take the writer's pager mutex. Uncommitted frames are invisible.
Normal commit flushes dirty pages into WAL and uses one FULL WAL barrier.

The initial metadata-inline freelist failed the large-value shrink test; the
limit was not widened. The replacement keeps a free-page chain in database
pages and persists its head with the root through the same WAL transaction.
Popped free pages get a WAL-visible taken marker before reuse, so a corrupt
cycle cannot reallocate an unflushed replacement. This simple chain is less
write-efficient than SQLite's packed freelist trunks: freeing/reusing pages
adds page images. That cost must remain visible in resize/churn results.

Checkpoint runs after roughly 4MiB of WAL at transaction boundaries (earlier
under a configured tighter cap), or explicitly at the end of a measured phase.
It copies the newest committed pages to the data file, syncs, independently
reads them back and compares them, then truncates/syncs WAL. That read-back is
an explicit extra cost. With a held snapshot it defers checkpointing; at the
WAL/total-byte cap it refuses further growth rather than overwriting old views.
Mutation/checkpoint errors poison the writer; reopening selects committed state.

An explicit managed-byte cap accounts conservatively for virtual data extent
plus WAL before every append. It is not a filesystem reservation or an
allocated-block cap. The pilot config is not yet persisted across reopen.

## Tests and implementation limits

Tests found and fixed: (1) metadata-inline freelist overflow after 8KiB values
shrank; (2) freeing a newly allocated overflow page before its first flush;
(3) unnecessarily refusing a new snapshot during uncommitted writes. All
diagnostics are retained. Source review also removed inherited retirement/birth
tracking from the transactional allocator path so it cannot accumulate stale
bookkeeping beside the on-disk free chain.

The tests cover complete key/value oracles, overflow resize/reopen, fresh-key
delete/reinsert, old/new snapshots, uncommitted evictions, corrupt-WAL refusal
without evidence modification, cap refusal with readable old state, completed
small-transaction updates under 2x loaded size, and four process-death points
during checkpoint. Process-death tests are not physical power-loss tests.

Promotion remains prohibited until open gates pass:

- Independent salvage after corrupted page/root/free chain/WAL is not implemented.
  A checksum error refusing to open is containment, not successful recovery.
- Readers are created through the in-process writer API. Cross-process snapshot
  opening and reader latency/work under concurrent load remain unproved. A
  surviving reader retains writer-reset ownership after its writer closes;
  a replacement writer waits for those views to close. That limitation is explicit.
- WAL/index bounds exist, but aggregate reader/cache/transaction allocation,
  persisted resource policy and the hard Pi process-budget gate are incomplete.
- The complete typed collection API and multimodel/late-indexing gates are not
  exercised by raw KV timings. No full shipping-suite pass is claimed.

SQLite inspiration is the retained source at commit
`f3b9f74d81132426dee1ccc07a67fdad2ccfeaa9`: `src/wal.c` reader algorithm and
frame publication, `src/pager.c::pagerWalFrames`, and
`src/btree.c::allocateBtreePage/freePage2`. This is an E4 Rust prototype using
those mechanisms, not SQLite underneath an E4 label.

Artifacts: `<scratch>/`.
Isolated source: `/tmp/e4-pagewal-candidate`.
Source/binary archives and exact runner arguments accompany measured results.

## Measured verdict

**Retain the isolated architecture experiment; do not promote it.** It
earns substantial gains over current E4 and near-SQLite ordinary-text density.
Mac mutation time and both platforms' resize costs miss F1 parity. The Pi
ordinary-text cases meet the observed <=1.10x time/size targets; single-run
400K and load/resize probes still lack acceptance repetitions. Independent
salvage and the remaining reader/resource gates prevent promotion.

The seven-law [machine-readable gate registry](FOUNDATION_GATES.json)
marks incomplete gates PENDING and absent independent recovery FAIL.
Ten substantive pilot scenarios plus one child helper pass on each platform,
including four deliberately terminated checkpoint children. The 129 existing
kernel unit tests pass on Mac. This is not a full shipping suite.

### Mac raw KV results

Seconds for the whole phase; changes exclude initial loading and include
commits and ending checkpoints. Three-run cases use medians; all other
rows are single-run probes. Rows contain text-like payload bytes, not
the earlier hybrid people/collection representation.

| Case | Reps | Current E4 load / changes s | Page-WAL load / changes s | SQLite load / changes s |
|---|---:|---:|---:|---:|
| load-100k | 3 | 2.291 / 0.000 | 1.251 / 0.000 | 1.129 / 0.000 |
| mixed-100k | 3 | 2.367 / 4.730 | 1.196 / 3.209 | 1.125 / 2.600 |
| resize-1k | 1 | 0.251 / 0.427 | 0.091 / 0.301 | 0.093 / 0.242 |
| sustain-400k | 1 | 9.354 / 63.024 | 5.159 / 40.892 | 4.387 / 31.046 |
| text256-40k | 1 | 0.964 / 1.947 | 0.408 / 1.447 | 0.514 / 1.210 |
| text32-10k | 1 | 0.243 / 0.489 | 0.104 / 0.262 | 0.091 / 0.219 |
| updates-100k | 3 | 2.367 / 2.405 | 1.218 / 2.073 | 1.132 / 1.811 |

| Case | Current E4 final / peak MB | Page-WAL final / peak MB | SQLite final / peak MB | Page-WAL time / size target |
|---|---:|---:|---:|---|
| load-100k | 29.549 / 29.811 | 29.524 / 33.368 | 29.450 / 33.879 | FAIL / PASS |
| mixed-100k | 34.326 / 34.326 | 32.485 / 37.443 | 32.403 / 37.203 | FAIL / PASS |
| resize-1k | 4.126 / 4.908 | 3.289 / 6.620 | 1.868 / 3.726 | FAIL / FAIL |
| sustain-400k | 131.749 / 131.749 | 129.888 / 134.854 | 129.446 / 134.258 | FAIL / PASS |
| text256-40k | 14.845 / 14.845 | 13.005 / 17.967 | 12.988 / 17.780 | FAIL / PASS |
| text32-10k | 0.890 / 0.890 | 0.553 / 1.308 | 0.631 / 1.352 | FAIL / PASS |
| updates-100k | 32.506 / 32.769 | 29.524 / 33.982 | 29.450 / 33.879 | FAIL / PASS |

MB is decimal; logical size includes side files. All raw allocated-size
and expansion-factor samples remain in the result JSON. Peak samples
are lower bounds. An observed target pass does not establish all seven laws.

### Pi raw KV results

Seconds for the whole phase; changes exclude initial loading and include
commits and ending checkpoints. Three-run cases use medians; all other
rows are single-run probes. Rows contain text-like payload bytes, not
the earlier hybrid people/collection representation.

| Case | Reps | Current E4 load / changes s | Page-WAL load / changes s | SQLite load / changes s |
|---|---:|---:|---:|---:|
| load-100k | 1 | 1.858 / 0.000 | 1.611 / 0.000 | 1.634 / 0.000 |
| mixed-100k | 3 | 1.831 / 6.050 | 1.651 / 5.736 | 1.931 / 5.823 |
| resize-1k | 1 | 0.119 / 0.302 | 0.053 / 0.392 | 0.046 / 0.201 |
| sustain-400k | 1 | 7.635 / 97.744 | 6.698 / 72.154 | 6.749 / 69.580 |

| Case | Current E4 final / peak MB | Page-WAL final / peak MB | SQLite final / peak MB | Page-WAL time / size target |
|---|---:|---:|---:|---|
| load-100k | 29.549 / 29.811 | 29.524 / 33.368 | 29.450 / 33.879 | PASS / PASS |
| mixed-100k | 34.326 / 34.326 | 32.485 / 37.443 | 32.403 / 37.203 | PASS / PASS |
| resize-1k | 4.126 / 4.908 | 3.289 / 6.620 | 1.868 / 3.726 | FAIL / FAIL |
| sustain-400k | 131.749 / 131.749 | 129.888 / 134.854 | 129.446 / 134.258 | PASS / PASS |

MB is decimal; logical size includes side files. All raw allocated-size
and expansion-factor samples remain in the result JSON. Peak samples
are lower bounds. An observed target pass does not establish all seven laws.

### 400K sustained work, separated by operation

Each arm performs 480K creates, 960K updates and 480K deletes after loading.
Operation timers exclude commit and final-checkpoint work; total also
includes value generation and harness overhead. Initial loading is excluded.

| Platform / engine | Creates s | Updates s | Deletes s | Commits s | Ending checkpoints s | Total changes s |
|---|---:|---:|---:|---:|---:|---:|
| Mac / e4 | 0.528 | 3.674 | 2.287 | 56.435 | 0.000 | 63.024 |
| Mac / pagewal | 0.732 | 2.173 | 2.152 | 35.550 | 0.184 | 40.892 |
| Mac / sqlite | 0.439 | 1.608 | 0.412 | 28.463 | 0.019 | 31.046 |
| Pi / e4 | 0.942 | 5.070 | 4.107 | 87.426 | 0.000 | 97.744 |
| Pi / pagewal | 1.208 | 3.327 | 3.934 | 63.338 | 0.164 | 72.154 |
| Pi / sqlite | 1.106 | 3.573 | 1.017 | 63.613 | 0.068 | 69.580 |

At 400K, all three engines plateau in cycles 10–12. Page-WAL stays at
129,888,256 logical bytes; SQLite stays at 129,445,888 bytes. This is a
single sustained probe per platform, not the repeated acceptance gate.

Pi benchmarks use `prlimit --as=134217728` for every engine. Its shared
services remain running. At 400K, maximum RSS is e4 10.70 MiB, pagewal 10.72 MiB, sqlite 11.03 MiB.
This is process RSS, not filesystem cache or complete allocation accounting.

The ordinary 256-byte rows achieve similar final density; overflow resize
does not. The existing whole-value overflow representation and page reuse
need separate investigation; no measured causal ablation isolates that gap yet.
Raw delete work also remains more expensive than SQLite. These findings
do not justify dropping checksums, read-back verification or reader protection.

Raw results: [Mac](PAGEWAL_RESULTS.json), [Pi](PAGEWAL_PI_RESULTS.json).
Source/binary hashes: [provenance](PAGEWAL_PROVENANCE.json).

Reproduce from the archived `pilot-source.tar.gz`, building
`pagewal_bench` with `--release --offline --features sqlite-balance,compact-cells`.
Arguments are `OUTPUT ENGINE N SIZE CYCLES CASE BATCH`; ENGINE is `e4`,
`pagewal` or `sqlite`. `run_pagewal_pilot.py` and `pagewal_pi.sh` retain the
exact matrix; choose a fresh authorized artifact root before rerunning.

## Cleanup and retained evidence

Removed 36 Mac and 15 Pi disposable database folders after report/archive
verification: 1,298,968,576 allocated bytes removed. The three 400K counterparts
remain on each platform; all source, binaries, raw reports and logs remain.
See [Mac cleanup](PAGEWAL_CLEANUP.json) and [Pi cleanup](PAGEWAL_PI_CLEANUP.json).
Allocated block accounting is not a guarantee of identical filesystem free-space change.
