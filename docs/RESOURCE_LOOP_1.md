# Resource loop 1 — constrained entry storage

Entry-path implementation and device characterization completed, 2026-09-11.
This document distinguishes implemented admission from measured peaks and
the broader resource gates still open.
The seven laws and timestamp decision are unchanged. Interfaces remain paused.

**Restart audit:** the user turned off
the Raspberry Pi to install sensors after 10 complete pairs: all eight 100K
pairs and both 400K load pairs. The ordinary 400K fixed-update pair was
interrupted during SQLite's updates. All 20 completed databases passed a
read-only structural and exact-row audit (3.2 million rows), matching their
saved CRCs with source data/WAL/freelist bytes unchanged. SQLite may create an
empty WAL sidecar during read-only open; the audit permits that empty-file
transition only. The entire interrupted pair was rerun, using the
original benchmark executable; the partial run is retained separately under
`artifacts/interrupted-user-poweroff/`.
All 18 pairs completed; the reboot/sensor installation separates the two
measurement batches. No Pi boot configuration change or reboot was performed
by this agent. See [all 36 arms](RESOURCE_TABLES.md), [machine-readable results](RESOURCE_RESULTS.json)
and [independent paired audit](RESOURCE_AUDIT.json).

## Device results and interpretation

All runs used a verified 128 MiB address-space limit, with the filesystem cache
outside that limit and swap still available. Validation passed 196 current-state
checks, 62.4 million repeated row visits, 32 snapshot checks, and exact point-read
and reopen agreement in every pair. All reported E4 pages account; no sampler
errors occurred. The maximum sampled process RSS was 26.70 MiB, but arm order
and allocator retention affect that number; it is not a standalone engine budget.

| Workload | Ordinary E4 | Paired SQLite |
|---|---:|---:|
| 4M ascending load time | 65.94 s | 63.51 s |
| 4M final files | 655.58 MiB | 616.22 MiB |
| 400K fixed-update time, four cycles | 139.73 s | 149.08 s |
| 400K mixed churn, no reader | 220.88 s | 232.70 s |
| 400K no-reader sampled peak | 153.80 MiB | 134.48 MiB |
| 400K held-reader sampled peak | 225.79 MiB | 1843.35 MiB |
| 400K held-reader final files | 224.86 MiB | 126.62 MiB |

Ordered-load density is 6.39% above SQLite in this corpus. Shuffled load retains
additional reusable/fallback-protected pages; at 400K its final E4 footprint is
83.24 MiB against SQLite's 67.68 MiB. The earlier ordered population result and
this live-workload footprint measure different datasets and page lifecycles.

No-reader mixed churn settles near 153 MiB at E4 cycle ends after the first
growth cycle. The held-reader case pins the initial snapshot for cycles 1–2,
releases it after cycle 2, and continues unpinned for cycles 3–4. A held reader
makes SQLite's WAL peak much larger, but SQLite
finishes smaller after release and runs that case faster. E4 retains its file
extent for reuse. Its held-reader peak is 2.71× initial size; the configured
limits enforce an explicit envelope, not an automatic 2× promise.

Constrained 400K updates and no-reader churn took 138.54 s and 218.70 s,
versus their SQLite controls' 148.41 s and 233.39 s. Constrained held-reader
churn took 223.66 s versus 141.86 s, at 225.75/1843.35 MiB sampled peaks.
Every constrained arm remained below its declared normal-write allowance.

**Timing caveat:** constrained 4M load took 124.69 s versus its SQLite control's
64.02 s, with the same final file sizes as ordinary mode. Other Pi jobs started
during this final pair, and the final hardware record has load averages
5.96/3.98/2.57 (no throttling). LLM jobs were observed afterwards. This timing
cannot isolate the cost of publishing every constrained commit. The affected
pair is marked in the table; a quiet repeat is still needed for that comparison.
The other user's processes were not stopped. These single-pair measurements
do not establish overall SQLite parity.

## Evidence retention and cleanup

The Pi evidence archive was copied to scratch and all 116 manifest entries
were independently SHA-256 verified. Archive SHA-256:
`57f46e6a7c895dda81b1fc5b25edf97bef857d6a0bf5b19e5c71ab377e24ba37`.
Raw results, logs, hardware records, executables and source snapshots remain
under `<scratch>/` and the Pi workspace.
After report validation, all 36 accepted benchmark database arms were removed:
5,184,160,096 logical bytes / 5,184,274,432 allocated file bytes. Cleanup manifests
are retained in [RESOURCE_CLEANUP.json](RESOURCE_CLEANUP.json). Crash fixtures, the interrupted pair, uncapped diagnostics,
toolchain and source remain for investigation/reproduction.

## Implemented scope

`Store::create_limited(path, config, ResourceLimits { ... })` creates a new
directory. Resource policy lives inside the checksummed metadata pages and
survives ordinary `Store::open` and snapshot opens. A format flag prevents an
older engine from interpreting this publication mode as ordinary WAL commits.
Both publications retain the policy; losing the newest metadata page falls
back to the previous publication, including its limits. That fallback can
lose the latest publication's changes, as in the existing dual-slot design.
`Store::create` now refuses an existing data file.

Limits are explicit rather than a new default:

```rust
let limits = kernel::limits::ResourceLimits {
    data_bytes: 80 << 20,
    wal_bytes: 4 << 20,
    tracked_pages: 16_384,
    readers: 8,
    record_bytes: 65_536,
    recovery_bytes: 8 << 20,
};
let store = Store::create_limited(new_directory, config, limits)?;
```

These values illustrate the API; they are not a selected Pi default. The
managed logical allowance is `data + WAL + 2 × (28 + 24 × tracked_pages) +
48 × readers + recovery_bytes`. The two freelist allowances cover the
published sidecar and its candidate simultaneously. Dirty data pages reserve
their page numbers before they can be evicted or flushed. Buffered WAL frames
are admitted against their eventual file extent before append. These checks
are independent of a sampler or checkpoint trigger.

The example totals about 92.75 MiB of managed logical allowance. For a
50 MiB dataset and a desired 100 MiB working footprint on disk, an operator
can choose an explicit envelope below 100 MiB and leave filesystem headroom.
The engine refuses growth at its configured limits; it does not promise
that every update or pinned-reader workload will fit, and it does not derive
an automatic 2× cap from initial size. The benchmark's generous envelopes
characterize completed workloads; small-limit regressions separately force
refusal and verify committed data survives.

The allocator caps both retired-page bookkeeping and recycled-page tracking.
Exhaustion returns an error; it does not silently drop reuse information.
The freelist parser bounds input before allocation, rejects duplicate page
IDs, and serialization reserves its exact output length. Maximum record
size also applies to overflow reads and retirement, before allocating from
an on-disk length. The memory reservation primitive now handles integer
overflow by refusing the request. None of this makes the page-cache setting
a whole-process RSS cap: bookkeeping counts, record/WAL buffers, caller
allocations, SQLite's C allocations and filesystem cache must be distinguished.

Fixed reader slots bound registration files and reader-table working memory.
The fixed slot inode is never unlinked, avoiding a lock/unlink race between
contenders. An unlocked slot is reusable after close or process death;
ambiguous locked contents stop page reuse conservatively.

## Commit and refusal behavior

Constrained `commit()` publishes the changed pages and metadata using the
configured durability barriers, then rotates the uncommitted WAL. It never
inserts a WAL Commit record. Consequently an acknowledged constrained commit
does not require allocating replay pages after a crash. Ordinary stores retain
their existing WAL commit behavior. The tradeoff is publication and freelist
work at every transaction boundary; timings must name this difference.

A refusal after mutation begins poisons the writer. Drop and reopen it before
retrying. The failed constrained handle also refuses point reads and both scan
directions, so it cannot expose a partially modified working tree. Previously
acknowledged rows and existing snapshots remain usable;
the unfinished transaction is discarded on reopen. Release old readers when
they prevent reuse, reduce transaction size when the WAL allowance is full,
or export to a larger store. An oversized record is rejected before mutation
and does not poison the writer. Failed checkpoint steps, including metadata
write and log rotation, now poison consistently.

Bulk sort, graft and prepared-candidate publication are explicitly refused
in constrained mode before consuming their input or creating scratch files.
Batched `put`, updates, deletes and prefix deletion remain available. There
is no implicit switch to unbounded work.

## What the allowance does not promise

This is a managed **logical file** allowance. Filesystem block rounding,
directory metadata, filesystem snapshots, unrelated files and other programs
are outside it. A free-byte allowance is not a filesystem reservation: another
program can consume the volume. Actual allocation is measured separately.

Normal writes cannot consume `recovery_bytes`; crash restart requires no new
data pages. This number does **not** prove arbitrary corruption salvage will
fit. Recognized constrained stores refuse the inherited in-place rebuild.
`recover_to` can preserve their source and export into a separately provisioned
destination. Its output, sort runs and reports are outside the source quota.
If both metadata policies are destroyed, policy reconstruction and a safe
operator-selected destination are required; the legacy recovery path is not
a policy-preserving repair mechanism. A capped repair workspace remains open.

## Evidence

Local evidence: `<scratch>/`.
Pi workspace: `<scratch>/`, on the microSD
ext4 filesystem, not its tmpfs `/tmp`.

The original missing-policy metadata failure is retained in
`initial-resource-red.log`. Focused refusal and source-preservation results
are in `resource-tests.log`; complete workspace validation is in
`workspace-tests.log`. The complete workspace pass contains 316 passing tests; two later focused
regressions bring coverage to 318 distinct tests. After restoring mutations,
all 127 kernel unit tests and all 8 resource tests passed again. Both guard
removal mutations failed at runtime (data admission and duplicate freelist
rejection), with source restored. All 8 resource tests and 2 commit-failure
tests passed in the initial Pi validation. Final Pi validation reran all 127
kernel units and all 8 resource tests successfully after the failed-writer
read fix. Three real SIGKILL scenarios pass on both hosts:
after commit, after checkpoint, and during uncommitted flushed updates.
Pi crash checks and the 1K paired smoke pass, but their requested 64 MiB
cgroup limit was NOT enforced. The first matrix check caught missing
`memory.max` and `memory.swap.max`; the kernel boots with
`cgroup_disable=memory`, and `cgroup.controllers` lacks memory. The initial
100K load pair is retained as uncapped evidence and the matrix stopped.
A boot-change proposal remains unapplied. The completed separate
matrix used a verified 128 MiB `RLIMIT_AS` address-space limit. This includes
process mappings, not filesystem cache; swap remains available. It is narrower
evidence than a memory cgroup, and those result sets must not be combined.
The completed device matrix is: 100K
and 400K load/fixed-update/mixed/held-reader cases, plus 4M load-only, each
with ordinary E4 and constrained E4 paired against SQLite. One pair per
mode/case/size; these are device characterization, not a repeatability gate.
The 4M load uses ascending IDs in both engines; 100K/400K use a fixed shuffled
order. The 4M rung is not an order-matched timing scaling comparison.

The failed-writer read regression was first observed failing in
`poisoned-read-red.log`; `poisoned-read-green.log` records the subsequent pass
of all 127 kernel units and 8 resource tests after adding read guards.
The final Pi SIGKILL rerun is launched under `prlimit --as=134217728` and
passes all three cases, preserving exactly 1000 mixed rows and three layout
copies. Its records are in `artifacts/final-crashes/results.json`.

Hardware verified through SSH: Raspberry Pi 5 Model B Rev 1.1, 4 GiB RAM,
64-bit Linux, roughly 59 GiB microSD. The Pi's DNS lookup failed, so Rust
1.96.0 and locked vendored dependencies are transferred for an offline build.
Installer SHA-256 on both machines:
`371eadcca97062219cbd8593628eb5d2802bc370515d085fedce1b56b2baed57`.

No full seven-law pass, whole-process memory bound, overall SQLite parity,
automatic repair budget or production-ready Pi default is claimed here.
The held-reader benchmarks check snapshot contents between write phases;
they do not establish a concurrent-reader latency SLA. Continuous read-latency
isolation and bounded corruption-repair workspace remain separate gates.

## Reproduction and retained identity

The final benchmark source snapshot is `pi-source-final.tar.gz`, SHA-256
`13da55ac5384639aef852869f70b00993959526324ccefb362de5f1d63efa312`,
under the local evidence directory. Pi executable SHA-256:
`03fe9026ddfb32fd5fab03f3313360f9c2910dc047cb5494c1fc197f8f7c3625`.
Final Mac executable SHA-256:
`270f298d5591d2cae4e87000b79cbe609d54cdb68c2fb997b17f98844edcdbea`.
Reporting and archive helpers can evolve independently of this frozen engine.
The post-reboot audit source is retained separately as
`postboot-verifier-source.tar.gz`, SHA-256
`cf1c69bbe7e7da07d0057da1fb04300529a916a1b410e05d5843321b28557356`.
Its `--verify-saved` entry point is used only for the read-only restart audit;
the original `lifecycle-pi` executable continues to produce benchmark timings.

The isolated Pi controller is [resource_pi_final.sh](../tools/resource_pi_final.sh):
offline native release tests, a release benchmark build, three constrained
SIGKILL checks under `prlimit --as=134217728`, then the address-space matrix.
[resource_pi_matrix.sh](../tools/resource_pi_matrix.sh) refuses an existing
output matrix directory and validates the actual limit recorded in every pair.
It deliberately runs one engine at a time, with no compilation or tests during
timing. Reproduction needs a fresh output directory; do not overwrite retained
evidence. For memory-cgroup runs, enable and verify the controller first and
use the separate `cgroup` mode.

Local final regressions:

```sh
TMPDIR=<scratch> \
  cargo test -p kernel --features sqlite-balance,compact-cells \
  --lib --test resource_limits -- --test-threads=1
```

The result validators are [check_sustained.py](../tools/check_sustained.py)
and [record_resource.py](../tools/record_resource.py). They require exact
cross-engine state, point-read and reopen agreement, intact snapshots, complete
page accounting, zero sampler errors and consistent actual process limits.
The latter also checks each constrained E4 sampled peak against its declared
normal-write allowance. Guard-removal failures provide separate evidence that
admission tests can detect missing guards; peak sampling itself is not that proof.
