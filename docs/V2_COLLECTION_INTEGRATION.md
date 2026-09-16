# V2 typed collections on the page-WAL store — 2026-09-16 (candidate r3)

Status: **tested on Linux (candidate r3, source archive
`ee3ae0196ae832de72d67e9c73218d071288f52578df5e07c5b8bce5652feaf1`) and
retained for continued development** — a functional-improvement
decision, not a raw-write-optimization acceptance, not a stable disk-format
freeze, and not a production release. Source is identical to tested
candidate r3; the integration commit records this tested source without a
co-author. r1 went to Linux compile-only and r2 was a diagnostic snapshot of
the same candidate line; both are superseded by r3, the source actually
tested and retained. Full test/benchmark/compatibility evidence is in
`docs/V2_FOUNDATION_LOOP.md`. This is integration toward release, not a
release qualification: it claims no law fully passed and not `L8-COMPAT`.

## What changed

`collections::Database` — the public typed API (`create`, `create_limited`,
`open`, `open_snapshot`, `create_collection`, `alter_collection`,
`collection_info`, `put`, `update`, `get`, `get_by_id`, `delete`, `scan`,
`commit`, `rollback`, `set_clock`) — stores through `PageWalStore`
(`E4PWAL02` page-image WAL, two checkpoint metadata copies, identity-bound
recovery, accepted at 64b6663). The inherited kernel `Store` is no longer a
collection backend; `src/collection_backend.rs` is the one place the page-WAL
is selected.

Unchanged: entity keys (`0x10/0x20/0x40/0x60`, catalog `0x00/0x01/0x02`),
dense-v3 records, layout descriptors, catalog/counter packets, the 8-byte
collection header payload of every ordinary database, composite `EntityId`s,
immutable layouts, timestamp policy (OFF by default, explicit opt-in),
validation-before-mutation and fail-closed handle semantics. Page, cell,
overflow, WAL frame and checkpoint header encodings are byte-identical to the
accepted pilot.

New public surface:

| Method | Meaning |
|---|---|
| `Database::checkpoint() -> Result<bool>` | Fold the published WAL into the data file. `false` = a reader in some process holds a slot; deferred, not skipped. Asking mid-transaction is a validation error. |
| `Database::limits() -> Option<ResourceLimits>` | The persisted policy of a `create_limited` database. |
| `Database::storage_bytes() -> (data, wal)` | Current page-WAL extents (the 96-byte hint file excluded). |
| `Database::tracked_pages() -> Option<usize>` | Distinct pages in the WAL index since the last checkpoint. |
| `Error::Unsupported(String)` | A format, policy or configuration refused before any byte changes. |

`PageWalStore` gained adapter API only: `open_validated`, `open_snapshot`,
`open_snapshot_validated`, `range`, `rollback`, `dir`, `is_snapshot`,
`is_dirty`, `reader_slot`, `set_runtime_limits(data, wal, tracked_pages)`,
`data_bytes`, `tracked_pages`, `managed_cap`, `root`/`page_count`/`page_bytes`
(diagnostics), and `pagewal::candidate_reader` for typed recovery.
`open`/`open_with`/`snapshot`/`commit`/`checkpoint`/`set_cap`/
`test_checkpoint_crash` signatures are unchanged.

Kernel edits (authorized, smallest possible): `kernel/src/io.rs` gained
`try_lock_shared`, `lock_shared`, `lock_exclusive` wrappers over std's file
locks beside the existing `try_lock_exclusive` (platform code stays in
`io.rs`); `kernel/src/limits.rs` made `ResourceLimits::{encode, decode}`
public so the typed header persists the kernel's own `E4LIMIT1` bytes.

## Publication (the r1 defect and its fix)

r1 let a reader beside a live writer see a complete commit frame before the
writer's barrier returned. r2 publishes explicitly:

- **Hint.** `readers.lock` carries two 48-byte copies of
  `E4PWHNT1 | identity(16) | checkpoint_tx | published_tx | published_end | crc`.
  Written by the writer only, without a barrier, copy 0 then copy 1: after
  tail truncation at open/rollback, inside `publish` **after** the commit
  frame's FULL barrier and **before** the in-memory `committed` swap, and at
  the end of a checkpoint (`end = 0`). Derived state: every writer
  incarnation rewrites it at open.
- **Live-writer admission** (`writer.lock` held exclusive by someone): only
  existing coordination files are opened, read-only. Under the gate (shared):
  take a slot, read the checkpoint floor `F`, read the hint (valid crc +
  identity; larger `published_tx` wins for a torn pair), validate
  `checkpoint_tx <= F <= published_tx`, `end % 4144 == 0`, `end <= wal.len()`,
  `end == 0 => F == published_tx`, then strictly inspect exactly `[0, end)`
  (writer posture; a torn or bad frame inside the bound is an error) and
  require the last frame to be the commit of `published_tx`. Frames at or
  beyond `end` are never read. Any failure is an explicit, retryable
  refusal (`Io(WouldBlock)` with the reason); there is no guessed prefix, no
  clamp to the WAL length, no checkpoint-only view.
- **Quiescent admission** (`try_lock_shared(writer.lock)` acquired, so no
  writer is alive or can start while it is held): strict inspection of the
  whole files (incomplete uncommitted tail ignored, never truncated), typed
  check on a temporary view, and only then are missing coordination files
  created, the gate taken shared, a slot taken, and both locks released. A
  writer arriving during that bounded window gets an immediate
  `WriterLocked` — transient, one scan long, no retry loop in this pass.
- **Checkpoint** keeps the conservative gate of r1 for this loop: the gate
  exclusive plus every slot, non-blocking, held across the whole fold, data
  barrier, metadata copies and WAL reset; any held slot defers it
  (`Ok(false)`). It also requires the last hint write to have succeeded
  (`hint_current`), and rewrites the hint after the reset. Admission may
  therefore wait for a checkpoint in flight (bounded); reads after admission
  do no coordination. Phase-A/phase-B unlocked copying is deliberately not
  implemented.
- **Failure at each copy boundary.** A failed barrier, or a failure writing
  hint copy 0, publishes nothing: readers, in-process snapshots and the
  writer's bookkeeping stay on the previous transaction. If copy 0 landed and
  copy 1 failed, readers can already see the transaction (the newer valid
  copy wins), so the writer's bookkeeping swaps as well and only the caller's
  `commit` reports the uncertain outcome — coherent old-everywhere or
  new-everywhere, never old-here-new-there. Either way the writer is poisoned
  and `checkpoint` refuses until `rollback`/reopen, which re-inspect the
  durable files, truncate only bytes past the last complete commit, barrier
  the recovered prefix and rewrite both copies — a complete frame is reported
  as committed (durable truth), never hidden and never truncated under a
  surviving reader.
- **Writer startup / recovery window.** A crash can leave a valid but stale
  hint beside a later durable, acknowledged commit (the hint is derived and
  never barriered). The opener therefore takes an existing gate exclusively
  *before* claiming `writer.lock` and holds it through inspection, the typed
  check, tail normalization and republication; a reader that finds
  `writer.lock` busy waits on the gate and is admitted only on the recovered
  hint, or is refused explicitly if the coordination files do not exist yet
  (fail closed). Lock order gate → `writer.lock` with a non-blocking ownership
  claim cannot deadlock a quiescent admission (`writer.lock` shared → gate
  shared): the writer never waits while holding the gate. An unknown format
  still creates nothing (the gate is only opened if it already exists). Reader
  death releases its slot with the fd; no cleanup.
- **Reader bound.** The typed check returns the persisted `readers` bound
  and the page-WAL enforces it on the slot index after the slot is held, on
  both admission paths, without re-running the check; a refused admission
  releases its slot. Nothing is clamped.

Tests: `src/pagewal_hint_tests.rs` (hint-write failure at each copy
boundary, barrier failure after frame write, damaged/stale/foreign hints
incl. torn pair and end past WAL, writer recovery window with a valid stale
hint and a racing live reader, writer restart beside a pinned reader,
quiescent strictness and no-create-before-validation, reader bound on both
paths, runtime quotas) and `tests/collection_pagewal.rs`
(child-process reader deferring a checkpoint; child-process writer restart
beside a pinned quiescent reader; unsupported sources with coordination files
removed).

## Limits (`create_limited`)

The exact six fields are persisted with the kernel's 56-byte `E4LIMIT1`
record appended to the collection header payload (8 bytes ordinary, 64 bytes
limited; other lengths `Unsupported` before any write). Enforced:

| Field | Enforcement |
|---|---|
| `data_bytes` | Pre-write check on data-extent growth (runtime, reinstalled at every open and after rollback). |
| `wal_bytes` | Pre-append check on the WAL; bounds one transaction's page images. Auto-checkpoint at half of it. |
| `data + wal` | Persisted page-WAL cap (`set_cap`), checked before every append. |
| `tracked_pages` | Distinct pages in the WAL index between checkpoints, checked at the actual addition before the frame is written (not a worst-case upfront bound). |
| `record_bytes` | Before mutation; the handle stays usable. |
| `readers` | Slot index: a reader whose slot ≥ `readers` is refused, cross-process. |
| `recovery_bytes` | Excluded from every allowance; never written into. |

Refused at creation, before the directory exists: `wal_bytes > 16 MiB`
(page-WAL bound) and `readers > 8` (slots this release ships). The r1 floors
(`wal_bytes ≥ 256 KiB`, `data_bytes ≥ 128 KiB`) are removed as unevidenced;
`tests/collections.rs` keeps its original 64 KiB policy as a diagnostic, and
`metadata_transaction_cost_is_measured_and_a_policy_below_it_refuses_explicitly`
measures the page-image cost of one `create_collection`, prints it
(`CREATE_COLLECTION_WAL_BYTES`, `FITS_64KIB_POLICY`), and proves a policy
below it refuses explicitly while one above it succeeds. If the 64 KiB
diagnostic fails on Linux, the measured number is the evidence to judge.

Quota accounting: `ResourceLimits::total_bytes = data + wal +
2·freelist_bytes + 48·readers + recovery`. The page-WAL keeps no freelist
file; the 96 logical bytes of `readers.lock` are charged against the
`2·freelist_bytes ≥ 104` allowance and slot files cost 0 logical bytes, so
logical managed bytes are `data + wal + 96 ≤ total − recovery`. Allocated /
inode cost, outside the logical quota: one filesystem block for
`readers.lock`, one inode each for `writer.lock`, `readers.lock` and eight
slot files.

## Named deviations and limits (Law 4)

- `Config`: only `IoMode::Buffered`, `SyncMode::Full`, `budget_bytes ≥ 64 KiB`
  are accepted; others are refused up front. Every run is FULL by declaration.
- One transaction is bounded by `min(16 MiB, wal_bytes)` of page images and by
  `tracked_pages` distinct pages; a larger one refuses, fails the handle, and
  `rollback` restores.
- A long-lived reader defers checkpoints; the WAL then grows toward its bound
  and the writer refuses until the reader drops. `checkpoint()` returns
  `false` so callers see it.
- A writer starting during a quiescent admission gets `WriterLocked` (T2 of
  the plan; no retry loop). A reader admitted during a writer's `open` before
  its `finish_open` may be refused ("no reader table") if the directory has no
  coordination files yet; both are transient.
- A reader beside a live writer sees at most the newest *published*
  transaction; during a concurrent commit that is one publication behind the
  writer's newest (snapshot semantics).
- `Database::open` refuses a missing path (the old `Store::open` created one).
- Directory layout gains zero-byte advisory files `reader-0..7.lock` and the
  96-byte `readers.lock`, created by a writer at open or by a quiescent reader
  after validation; absence is fine. Fixture hashing should cover `data` and
  `wal` and list these as expected additions.
- Platform assumptions: advisory locks are per open-file-description
  (in-process handles contend); read-only handles suffice for shared and
  exclusive locks; a filesystem refusing locks fails admission closed.
- Cross-binary fixtures are the compat helper's job; nothing here proves
  release compatibility.

## Recovery

`recovery::recover_typed_candidates` reads through `pagewal::candidate_reader`:
the committed WAL index is overlaid read-only on the data file, so rows
acknowledged but not yet checkpointed are exported as candidates. If the WAL
cannot be interpreted, the bare data file is scanned and `issues.jsonl`
records `committed_wal_overlay_skipped`; `report.json` carries
`committed_wal_overlay`. The in-module rootless test damages every interior
page in the data file, keeps a row that exists only in the WAL, and expects
both folded and WAL-resident scalar rows.

## Files

- `src/pagewal.rs`, `src/pagewal_hint_tests.rs` (new), `src/pagewal/repair.rs`
  (`Source` visibility + `plain`), `src/recovery.rs` (reader source + one
  issue/summary field), `src/lib.rs`, `src/collection_backend.rs`,
  `src/collections.rs`, `src/bin/collection_inspect.rs` (now a V2 diagnostic:
  reads through the overlay, reports WAL/hint bytes), `docs/COLLECTIONS.md`,
  `tests/collections.rs` (original 64 KiB policy restored),
  `tests/collection_pagewal.rs`, `kernel/src/io.rs`, `kernel/src/limits.rs`.

## Linux validation (run against candidate r3; results in docs/V2_FOUNDATION_LOOP.md)

The commands below were the requested validation set for this integration and
have now been run on Linux against candidate r3 (default and compact-cells
feature builds, plus the 1M/10K benchmark and compatibility fixtures); see
`docs/V2_FOUNDATION_LOOP.md` for the actual pass counts, hashes and numbers.
They remain listed here as the exact reproduction commands:

```
cargo test --manifest-path sekejap-e4/Cargo.toml --test collections --test collection_pagewal
cargo test --manifest-path sekejap-e4/Cargo.toml --lib collections::
cargo test --manifest-path sekejap-e4/Cargo.toml --lib pagewal::
cargo test --manifest-path sekejap-e4/Cargo.toml --test pagewal --test pagewal_recovery_identity --test pagewal_repair --test pagewal_transaction_capacity
cargo test --manifest-path sekejap-e4/Cargo.toml --test schema_recovery --test recovery_faults
cargo test --manifest-path sekejap-e4/Cargo.toml -p kernel
cargo build --manifest-path sekejap-e4/Cargo.toml --bins
```
Repeat with `--features compact-cells,sqlite-balance`. The in-module
collection tests assert `TMPDIR` is under an authorized artifact root.
`reader_in_another_process_*` and `writer_restart_in_another_process_*` spawn
the test binary itself as a child and need `std::env::current_exe()` to be
executable inside the Job; run the `collection_pagewal` binary with
`--nocapture` to capture `CREATE_COLLECTION_WAL_BYTES`.
