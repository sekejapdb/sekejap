# Linux allocation loop — 2026-09-14/15

**Decision: retain the measured allocation approach for a budget-aware engine
prototype; do not ship either diagnostic wrapper.** Explicit batched allocation
cuts server's worst sampled E4 footprint from **263.717 MB to 126.878 MB**, a
**51.9% reduction**, with approximately unchanged mutation time. Pi gains no
space and pays 5.2% median time. Both batched medians meet the owner's <1.5×
SQLite time target. There is no E4 runtime or file-format change in this loop.
The physical-cap gate remains **FAIL**: a separate tiny-file test demonstrates
why reservation bytes must enter admission accounting.

The accepted compact page-WAL executable from the previous Linux qualification
is the **current E4** control throughout this report. This is not the older
pre-packing baseline, nor the public typed collection Store. All variants use
the same executable per host; only the candidate process loads a diagnostic
Linux write wrapper. No Mac database tests ran.

## What consumes the extra disk space

The Pi artifacts are on **ext4**. server's local-path PVC is an **OverlayFS**
mount whose upper directory is on **XFS** (`/dev/sda5`, mounted at `/var`).
The file's apparent length and the blocks charged to it are different quantities.
XFS documents dynamic preallocation beyond EOF, with later reclamation of unused
space. [Linux XFS documentation](https://docs.kernel.org/admin-guide/xfs.html)

A standalone C control, containing no E4, SQLite or Rust code, loads 96 MiB,
then performs twelve rounds of small overwrites and 1 MiB growth. The final
file is 108 MiB. Every 4 KiB page is verified after closing and reopening.
`fstat` samples surround writes and syncs; FIEMAP observes extents without
forcing writeback. Results below are sampled peaks, not a hard bound.

| Filesystem | Ordinary writes | Reserve before growth | Trim after each sync |
|---|---:|---:|---:|
| Pi / ext4 | 108.004 MiB | 108.004 MiB | 108.004 MiB |
| server / OverlayFS-XFS | 160 MiB | 108.004 MiB | 235 MiB |

Before the plain server file's final trim, FIEMAP identifies **52 MiB of unwritten
extents beyond EOF** (`UNWRITTEN | LAST`, flags 2049). Same-size truncation
returns the final allocation to 108 MiB. It cannot undo the earlier peak.
The trim-after-sync control is therefore rejected as a peak-prevention strategy.
On Pi, the reservation control produces 97 final extents versus 8 for ordinary
writes. Reservation is not a free or universally beneficial operation.

Per-file observations in the actual E4 comparison independently locate the
excess: one server sample has a **120,082,432-byte data file charged 253,886,464
bytes**, plus a **4,656,384-byte WAL charged 8,323,072 bytes**. These sequential
100 ms observations complement the benchmark's 1 ms total samples; neither
claims to catch every transient peak. The standalone extent evidence and the
write-reservation ablation support XFS preallocation as a cause of this excess.
They do not retrospectively identify every old space spike or Mac I/O failure.

SQLite provides a useful source precedent: its Unix VFS size-hint handler rounds
growth to a configured chunk, with a `posix_fallocate` path where available.
That handler is conditional on a positive chunk size; it is not evidence that
default SQLite always reserves this way. Our SQLite comparator remains unchanged.
[SQLite `os_unix.c`, `fcntlSizeHint`](https://sqlite.org/src/doc/trunk/src/os_unix.c)

## Matched database workload

Each measured arm loads **400,000 raw KV rows**, with 8-byte keys and a
deterministic 256-byte value. It then performs twelve rounds, each containing
80,000 updates, 40,000 deletes and 40,000 fresh inserts: **1,920,000 changes**
after loading. Population remains 400,000. These are not multimodel indexes or
complete collection API measurements.

Each native binary has a **128 MiB address-space limit**, an **8 MiB engine
cache**, and `MALLOC_ARENA_MAX=1`. SQLite uses WAL, FULL synchronous durability,
automatic checkpoints at 1,000 pages, mmap disabled and a WITHOUT ROWID KV table.
Both use batches of 1,000 operations. Every reported mutation time includes
commit and ending checkpoint work; load and independent verification are
separate. Added `fstat`, allocation calls and wrapper locking are inside timing.

Three orders per experiment: E4/candidate/SQLite; SQLite/E4/candidate;
candidate/SQLite/E4. Each host runs its arms serially. We retain every trial,
including substantial timing variability. The tables use medians, never the
fastest run. Both hosts are shared systems; this is not proof of isolated-device
latency. Do not compare absolute time across hosts or between experiment waves
as an engine speedup.

Every round checks exact generated values, strict key order, membership and
count, followed by a close/reopen check. The report collector also checks matching
verification results across all arms and exact CRUD counts. **36 large arms,
four reserve smoke arms, ten 108 MiB controls and four tiny controls pass.**
The wrappers compile natively with `-O2 -Wall -Wextra -Werror` on both hosts.

## Experiment A: reserving every growing write — rejected

The exact-range wrapper performs `fstat` per positional write and reserves only
new bytes before growth, using `fallocate(FALLOC_FL_KEEP_SIZE)`. Allocation
failure propagates; there is no silent fallback. It makes **483,906 reservation
calls** across **836,807 positional writes** per large E4 arm.

| Host | Engine | Load, seconds | 1.92M changes, seconds | Worst sampled allocation, MB |
|---|---|---:|---:|---:|
| Pi | Current E4 | 8.792 | 95.146 | 125.891 |
| Pi | E4 + exact reservation | 10.542 | 126.194 | 125.968 |
| Pi | SQLite | 9.742 | 119.156 | 134.267 |
| server | Current E4 | 7.396 | 78.952 | 263.717 |
| server | E4 + exact reservation | 10.363 | 138.058 | 125.895 |
| server | SQLite | 7.589 | 73.915 | 271.745 |

server space improves, but candidate time is **1.868× SQLite**. Pi slows by
32.6% against its current E4 control without a useful space benefit. Do not
integrate this per-write policy.

## Experiment B: batching reservation — significant, still a diagnostic

The second wrapper reserves growth in **1 MiB batches**, remembers reservations
in a fixed 256-descriptor cache, and invalidates that cache at truncate/close.
It verifies identity and length on each write; higher descriptors fall back
to exact reservation. It makes **2,206 allocation calls**, down **99.54%**, with
the same 836,807 positional writes. Requested reservation bytes are cumulative
across WAL reuse, not retained file size or payload bytes written.

| Host | Engine | Load, seconds | 1.92M changes, seconds | Worst sampled allocation, MB |
|---|---|---:|---:|---:|
| Pi | Current E4 | 8.853 | 102.651 | 125.895 |
| Pi | E4 + batched reservation | 9.450 | 107.950 | 126.886 |
| Pi | SQLite | 10.325 | 118.544 | 134.267 |
| server | Current E4 | 8.389 | 58.248 | 263.717 |
| server | E4 + batched reservation | 5.158 | 58.705 | 126.878 |
| server | SQLite | 6.033 | 49.444 | 271.745 |

The batched candidate takes **0.911× SQLite time on Pi / 1.187× on server**.
Relative to current E4 in the same wave, mutation time increases **5.2% / 0.8%**.
The significance is the repeatable space reduction on server, not a speed gain.
The apparent server load improvement is not promoted as an isolated speed claim.

All experiment arms have these logical sizes (decimal MB, measured while open):

| Engine | After load | After all changes |
|---|---:|---:|
| Current E4, either E4 allocation wrapper | 110.203 MB | 121.229 MB |
| SQLite | 117.670 MB | 129.446 MB |

Batched E4's worst sampled allocation is **1.151× its loaded logical size** on
both hosts. Current E4 reaches **2.393× on server** and 1.142× on Pi; SQLite
reaches 2.309× / 1.141×. The allocation wrappers do not change logical density.
SQLite removes its 32 KiB shared-memory file on final close; that explains the
corresponding difference between the while-open report and cleanup file totals.

## Why this is not yet a physical-cap implementation

A fresh **4 KiB** write, independently verified after reopen, receives **4 KiB
allocation with exact reservation but 1 MiB with batching**, on both hosts.
The large-workload result therefore cannot justify plugging this wrapper into
the existing logical-size cap. The wrapper knows neither the configured budget
nor the other database files. It is test tooling only.

The next engine prototype must reserve through `FileIo`, charge unused reserved
space across data and WAL, clip batches to remaining allowance, and propagate
allocation refusal before publishing writes. Cached reservation state must track
truncate/reopen and partial failures. Existing committed rows and pinned snapshots
must survive refusal; recovery must retain verified WAL until checkpoint data is
verified. These require explicit cap-edge and injected-failure tests before the
matched Linux performance comparison. No platform code belongs above `io.rs`.

Even correct reservation accounting is not, by itself, a portable proof about
all filesystem metadata and transient allocation. A hard physical guarantee
needs an enforceable allocation boundary beneath logical file lengths, such as
a qualified filesystem quota or a bounded reserved backing store. Neither is
implemented here. `ALLOC-CAP` stays FAIL; the seven laws, strict Law 2, remaining
recovery/reader gates, timestamps-off default and public Store selection are
unchanged. The prototype must not silently reserve a full database-sized RAM map
or pre-scan the database to make allocation decisions.

## Reproduction and retained evidence

- [All raw controls, 40 benchmark reports, per-file samples, hashes and tiny checks](ALLOCATION_RESULTS.json).
- [Frozen source/library checks and validation scope](ALLOCATION_VALIDATION.json).
- [Cleanup evidence](ALLOCATION_CLEANUP.json): **5,609,447,424 allocated bytes**
  in 40 database directories and 14 standalone data files were removed after
  archive verification. Sources, binaries, reports, logs and failed-job history remain.
- [Filesystem mount evidence](ALLOCATION_ENVIRONMENT.json).
- Tools: `allocation_probe.c`, `allocation_interpose.c`, `allocation_chunk.c`,
  `allocation_bench.py`, `allocation_tiny.py`, `allocation_report.py`,
  `allocation_cleanup.py`, and the two bounded server Job manifests in `tools/`.
- Pi root: `<scratch>`.
- server root: `<scratch>`.
- Each root's parent retains `allocation-exact-evidence.tgz`,
  `allocation-chunk-evidence.tgz` and `allocation-complete-evidence.tgz`.
  Independent local copies are `/tmp/e4-allocation-{pi,server}-{exact,chunk,complete}-evidence.tgz`.
  Complete archive hashes and source/binary hashes are in the raw results.

Run `python3 tools/allocation_bench.py NATIVE_ROOT` for exact reservation or add
`chunk` for batching; each requires fresh output paths, a compiled standalone
probe and the previous qualified native executable. These are Linux-only
diagnostics. The original exact runner is preserved as `allocation_bench_exact.py`
in complete archives; its frozen hash differs from the later runner that adds
the optional chunk argument. No test sources or benchmark executables changed
between arms within a wave.

The first server job finished its benchmark, then its waiting shell exceeded the
three-hour job deadline. Its successful arm reports are preserved; that Job is
not recorded as successful. The second Job runs the benchmark and exits: it
completed successfully. Data cleanup occurs only after the complete metadata
archives are copied and independently verified, and after all benchmark processes
have finished. No failed database evidence or prior-loop artifacts are removed.
