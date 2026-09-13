# Lean lifecycle gate: overflow reclamation fixed

Completed 2026-09-10 (Australia/Melbourne). The scoped gate passes after fixing
an inherited overflow-page leak. The subsequent peak-space review paused
interface integration: this gate proves scoped correctness/reuse, not acceptable
peak disk expansion or completion of the seven-law gate.
The laws and `CONTRACT.md` remain unchanged.

## What the loop found and fixed

Replacing or deleting a large value removed its leaf entry but never returned
its overflow pages to the freelist. The initial 100K/400K runs returned exact
rows and reached a temporary file-size plateau: spare space allocated while
snapshots were held masked the leak. Explicit page accounting exposed it.

| Rows | Untracked pages before fix | Bytes lost to reuse before fix | Untracked pages after fix |
|---|---:|---:|---:|
| 100,000 | 9,000 | 36,864,000 | 0 |
| 400,000 | 36,000 | 147,456,000 | 0 |

Three focused regression tests failed before the fix. The change in
`kernel/src/btree.rs` validates each old overflow chain, removes its marker,
and records its pages in the existing generation-protected freelist. It covers
replacement, single-row deletion and prefix deletion. Reuse still waits for
the dual-slot fallback and live snapshot readers. The row/page format did not
change and no timestamp fields were introduced.

Named cost: replacing/deleting an overflow value now reads and checks the old
chain, taking work proportional to that old value. Retirement keeps one u32
page ID per old overflow page rather than materializing the value. Prefix
deletion collects IDs for the matching values in one leaf at a time. This is
change-sized bookkeeping; it is not a new whole-database scan. A corrupt old
chain causes a mutation error and poisons that writer; unverified pages are
never admitted for reuse. Recovery remains the path for damaged data.

These bytes were returned to **future page reuse**, not removed from the file.
The freelist sidecar grows to record them. Previously leaked pages in existing
experimental files are not retroactively discovered by this mutation fix;
the verified rebuild experiment below can reclaim those files separately.

## Scope and validation

`src/bin/lifecycle.rs` runs the same deterministic workload through E4 and
SQLite at 100K and 400K rows, without automatic timestamps or secondary indexes.
Rows contain Unicode names, booleans, nullable real values, geo Points, nested
binary JSON and schemaless extras including `u64::MAX`.

- Insert IDs through a fixed bijection, not ascending insertion.
- Six cycles: update 20% of rows, delete a disjoint 10%, checkpoint/reopen and
  verify their absence, then reinsert that 10%, checkpoint/reopen and verify.
- Odd cycles grow 1% of rows to 9KB overflow values and another 9% to 512-byte
  strings. Even cycles shrink them back to small inline rows.
- Both engines hold a real snapshot of the original dataset through the first
  two cycles. Every phase checks its exact original documents. Snapshots are
  then released; four further cycles exercise reuse.
- Every current row is checked against a closed-form document oracle, IDs are
  checked for gaps/unexpected entries, and E4/SQLite stage checksums match.
- Every phase passes E4's published-tree verifier or SQLite integrity_check.
- Every even E4 phase asserts: physical pages = 2 metadata pages + reachable
  tree pages + pages on the freelist. All current values are inline then.
- E4 and SQLite main-file lengths are constant from the end of cycle 3 through
  cycle 6. Small overflow regressions additionally run eight overwrite epochs
  after releasing the snapshot, requiring an exact file-size plateau.

Each large matrix run performs 12.4M exact current-row comparisons plus 4M
exact snapshot-row comparisons. The post-fix run passes all 52 current states
and 16 snapshot checks. Final common CRC32C: 3158378167 at 100K; 286729424 at 400K.

Two additional **actual process-kill probes** publish large values, replace
them with small values, then kill the writer after commit/before checkpoint
or after checkpoint. Each reopen recovers all 1,000 expected committed rows.
These cover the new overflow-retirement path; they are not power-loss tests or
exhaustive kill-point coverage. The initial matrix also included two committed
grow-value process-kill probes.

Regression results:

- Pre-change full workspace: **291 passed**, no failures/ignored tests.
- Final workspace, compact features enabled, release build: **296 passed**,
  no failures/ignored tests. Includes inherited model, snapshot, recycling,
  durability, graph/index and corruption tests, plus R1/R2 recovery tests.
- New overflow suite with default features: **5 passed**.
- New overflow suite with compact features and debug assertions: **5 passed**.
- Formatting and the retained-evidence checker pass.

The five new tests cover replacement, single delete, full-prefix delete,
partial-leaf prefix delete, and refusal to retire a checksum-invalid chain.
The initial corruption test assumed an empty freelist; its refined check
inspects actual retired page IDs, permitting legitimate prior leaf retirement.
Both that test-refinement log and the original three real leak failures remain.

## Storage and maintenance cost

Final closed-store bytes include all database files. MB below is decimal.
This deliberately stressful workload holds old snapshots across repeated
updates and large-value growth; it differs from the fresh 10M people benchmark.

| Rows | E4 after six cycles | SQLite after six cycles | E4 rebuilt into new store | SQLite VACUUM INTO |
|---|---:|---:|---:|---:|
| 100K | 164.923 MB | 32.903 MB | 19.599 MB | 15.897 MB |
| 400K | 653.543 MB | 132.768 MB | 78.340 MB | 63.607 MB |

The final-size comparison above omits temporary history during the run. At
400K, just before releasing the old reader at the end of cycle 2, total files
were **506.681 MB for E4 and 1,867.623 MB for SQLite** (including its WAL).
Neither is a safe peak bound for the whole run. The follow-up
[peak-space loop](PEAK_SPACE.md) measures within-operation expansion and
checkpoint policy separately.

**E4 still retains substantially more physical space after this snapshot/churn
workload.** Recycling prevents continued leakage; checkpoint does not shrink
the file or repack sparse leaves. This remains a maintenance cost, not a claim
that live-operation density has converged with SQLite. After an explicit
rebuild, E4 is about 1.23× SQLite's rebuilt size on this fixture. Recovery uses
the existing 90%-fill packer; these are maintenance-path measurements.

The rebuild experiment uses existing `kernel::recover::recover_to` and SQLite
`VACUUM INTO`. Both write new destinations; neither publishes over its source.
All rebuilt rows verify exactly, with zero E4 known losses or unknown extents,
and source file size/CRC fingerprints remain equal. Source-preserving rebuild
is proven here; a user-facing shrink/publish/rollback API remains future work.
The first SQLite maintenance probe created WAL/SHM sidecars on a read-only
open; the final probe uses immutable reads on closed, checkpointed fixtures.
The original probe and its failure log are retained.

| Rows | E4 six-cycle mutation time | SQLite six-cycle mutation time | E4 rebuild | SQLite VACUUM INTO |
|---|---:|---:|---:|---:|
| 100K | 5.51 s | 7.46 s | 0.20 s | 0.11 s |
| 400K | 21.10 s | 36.85 s | 0.76 s | 0.26 s |

Mutation time includes encoding, writes, commits and checkpoints, but excludes
reopen and verification (recorded separately in raw results). Rebuild timing
includes each mechanism's intrinsic work; exact typed verification follows.
These are single sequential runs with warm/unflushed OS caches. Compilation
and regression tests overlapped parts of the runs, so timing is diagnostic,
not an isolated speed comparison or a measured causal speedup from the fix.
New `mutation_data_io` counters exclude preceding verification reads; older
`counters` accumulate since reopen and must not be mistaken for mutation-only
read counts. SQLite I/O counters are not instrumented here.

Both writers use 8 MiB engine caches, snapshot readers 64 KiB, 4096-byte pages,
FULL synchronization, macOS fullfsync, mmap off, and 1,000-operation commits.
SQLite uses native scalar columns, two REAL coordinates and JSONB; E4 uses
dense-v3 and `sqlite-balance,compact-cells`. SQLite is 3.46.0. While its snapshot
is held, SQLite checkpoints PASSIVE and retains WAL; afterward it checkpoints
TRUNCATE. E4 keeps old pages behind its reader/generation horizon. The shared
device is scratch on the Apple M3 Pro Mac. There is no hard process-memory cap
or claim of exhaustive memory/I/O scaling from this lean test.

## Future interface slice (paused): reuse E3 code through typed rows

E3 already supplies the kernel, graph/index algorithms, SQL parser, executor,
service/snapshot framework and catalog/introspection contracts. Reuse those
components and their tests. The storage-coupled portions need adaptation:

1. Bring up a thin collection/CRUD interface: create, put, get, update, delete,
   commit and reopen, with persistent layouts and external-key identity.
2. Replace E3's `StoredNode`/`split_row` JSON framing and `put_row` byte-splicing
   with typed encoding and field access. JSON stays at the input/output boundary.
   Apply [optional timestamps](TIMESTAMPS.md), OFF by default, through the
   collection policy instead of carrying over E3's unconditional stamping.
3. Feed the existing scalar index/query paths from those typed accessors, then
   extend to graph, spatial, text and vector integration with their old tests.

Concrete E3 pointers: `src/db.rs` (`StoredNode`, `put_row`, `build_field_index`),
`src/sql.rs`, `src/query.rs`, `src/exec.rs`, `src/catalog/mod.rs`, `src/service.rs`.
E3's `Db::compact()` currently commits/checkpoints; it does **not** shrink the
file. Keep that distinction explicit when exposing E4 maintenance.

The benchmark's compact population key scheme is provisional. Integrated
node/collection/external-key and index storage must be measured together;
the fresh 10M density ratio cannot be assumed for the finished multimodel DB.
Broader syscall/power-loss coverage, hard resource caps, long-duration churn,
current-membership recovery after lost ancestry and full interface/index gates
remain open in tracker. E3 source was read only; no E3 data migration is involved.

## Reproduce and evidence

```sh
cargo run --release --offline --features sqlite-balance,compact-cells --bin lifecycle -- <scratch>
cargo run --release --offline --features sqlite-balance,compact-cells --bin lifecycle -- --repack-check <scratch> 100000
cargo run --release --offline --features sqlite-balance,compact-cells --bin lifecycle -- --repack-check <scratch> 400000
TMPDIR=<scratch> cargo test --release --offline --workspace --features sqlite-balance,compact-cells
python3 tools/check_lifecycle.py <scratch>
```

Use a fresh run path. The matrix creates its own temp directory on scratch;
unit tests use TMPDIR. `--crash-check NEW_DIRECTORY` runs the two standalone
overflow-retirement process-kill probes. The checker validates this recorded
before/after experiment, including its exact suite counts, from retained logs
and JSON even after disposable databases are deleted.

Evidence root: `<scratch>/`.
`measured/` records the pre-fix matrix; `fixed/` records the corrected matrix
and both rebuilds; `crash-retirement/` records the final interruption probes.
See [compact summary](LEAN_FOUNDATION_RESULTS.json), `summary.json`, source
snapshot, kernel patch, logs and cleanup manifest in the evidence root.
tracker task: `sekejap-e4` / `lean-lifecycle`.

## Cleanup

Completed after verifying and recording the evidence: removed only this loop's
large 100K/400K matrix databases and rebuilt database directories,
**2,181,033,116 logical bytes (2.181 GB)**.
All selected paths were checked absent afterward. This is removed file size,
not a guarantee of immediate APFS free-space reclamation.

Kept the small original failing fixture, small passing and process-kill
fixtures, all JSON/logs/reports, source snapshot, kernel patch and cleanup
manifest. Earlier 20M, density and recovery artifacts were untouched.
Exact deleted paths and sizes are in the evidence root's `cleanup.json`.
