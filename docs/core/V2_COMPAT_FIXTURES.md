# V2 typed-collection compatibility fixture helper

`bench/src/bin/v2_compat_fixture.rs` is a bounded, file-tools-produced helper for
gathering real older/newer-binary evidence toward `docs/core/FORMAT_FREEZE.md`
step 4, ahead of `collections::Database` being wired to
`e4_prototype::pagewal::PageWalStore` (see the doc comment at the top of
`src/pagewal.rs`, which points at the not-yet-written integration doc, and
`.integration-loop/compat-status.md` for this fixture's own ownership split).

It is a prototype tool, not a release-compatibility guarantee. Read
"What this does NOT prove" before citing any result it produces.

## Why a raw-projection reference, not a direct old-binary write

The accepted older commit (`64b6663`) already has a working, accepted
`PageWalStore` (page-image WAL, typed-key-compatible) but `collections::
Database` in that commit still writes through `kernel::store::Store`. There
is therefore no old binary that writes a `PageWalStore`-backed typed database
directly — the integration that would do that is the very thing being
prepared for. To get a real *older-encoder* typed file to test the *newer*
reader against, this tool:

1. Builds a small database with the old `Database` public API (old typed
   encoder, old `Store` writer) — this part is completely genuine.
2. After closing `Database`, opens the same directory read-only with
   `kernel::store::Store::open_snapshot` and scans every logical KV pair.
   `Database`'s on-disk keys (catalog, layout, sequence, name→id mapping,
   entity rows, vector sidecars) are engine-agnostic byte encodings; the
   *typed* format guarantee under test is about those bytes, not about which
   engine stores them.
3. Streams those same KV pairs into a fresh `PageWalStore` (already accepted,
   unmodified — `PageWalStore::open`/`put`/`commit`/`checkpoint`, called
   directly, not through `Database`) to produce a page-WAL file with the real
   old typed encoding inside it.

This is explicitly a **stand-in**, not the real write path: it never
exercises whatever translation code the eventual `Database`-over-`PageWalStore`
integration adds (buffering, batching, commit boundaries chosen by that
integration, etc). Treat it as bounded preparatory evidence that the typed
*encoding* survives moving to the new page-WAL container, not as proof the
integrated writer behaves identically byte-for-byte.

## Directory layout produced by `make-reference ROOT`

```
ROOT/
  old-source/                  # real old Store-backed Database, byte-preserved
  projected/
    checkpointed/               # PageWalStore: commit() then checkpoint()
    wal-pending/                 # PageWalStore: commit() only — WAL still
                                  #   holds committed frames awaiting checkpoint
  manifest.json                 # collections, layouts, live entities, deletions,
                                 # clock values — all captured from a live
                                 # Database scan right after the final commit
```

The fixture is intentionally tiny (a handful of entities): two collections
(`docs`: text/JSON/point fields, timestamps off, one deliberately
overflow-sized text value, one committed-then-deleted row; `vecs`: a vector
field, timestamps on, one `alter_collection` schema evolution, one row
updated after the evolution, one row deleted). No database-sized RAM is held
at any point — every step is a handful of small `put`/`get`/`scan` calls.

## Subcommands

```
v2_compat_fixture make-reference ROOT
```
Build against the **old** source tree (commit `64b6663` or equivalent). ROOT
must not already exist.

```
v2_compat_fixture verify-and-update ROOT --confirm-copy
```
Build against the **new**, integrated source tree. `ROOT` must already
contain `manifest.json` and the `projected/*` directories — normally an
explicit copy of a `make-reference` output, made by the parent, since this
command commits new writes into each target and must never run against the
original reference directories. It:
- opens each projected target through `Database::open`,
- checks every manifest entity/layout/collection exactly,
- performs one deterministic update, one insert, and one delete,
- commits, reopens, and checks the new state (including that the untouched
  fixture rows are unchanged).

Refuses with an explicit error if `--confirm-copy` is omitted. That flag is a
promise from the caller, not a technical guarantee that the directory really
is a copy — the parent is responsible for actually copying first.

```
v2_compat_fixture verify-raw ROOT
```
Opens each projected target directly through `PageWalStore::open` (bypassing
`Database`) and does a bounded scan. This confirms the page-WAL container
(header, WAL replay, checksums) still opens — nothing about typed decoding.
Run against a scratch copy: it never calls `put`/`delete`/`commit`, but
`PageWalStore::open` takes the writer lock and may normalize a recoverable
WAL tail or create coordination files. It is not a read-only inspection API.
Preserve originals and compare data/WAL checksums around every compatibility
step; record expected coordination-file additions separately.

```
v2_compat_fixture legacy-replay TARGET_DIR OUT_DIR --explicit-test-mode
```
Only runs with the explicit flag. Streams `TARGET_DIR`'s (typically
post-`verify-and-update`) `PageWalStore` KV pairs into a brand new legacy
`kernel::store::Store` at `OUT_DIR` (must not exist), then opens that with
the **old** `Database`/typed decoder and does a bounded read pass over the
known collections. This is an encoding-compatibility test only.

## Suggested run sequence (parent, on Linux)

1. Check out commit `64b6663` (or the frozen baseline it becomes) into tree
   `A`; build; run `A/target/.../v2_compat_fixture make-reference /path/ROOT`.
2. Hash/archive `ROOT/old-source` and `ROOT/projected/*` (parent-owned).
3. Check out the landed `Database`-over-`PageWalStore` integration into tree
   `B`; build.
4. Copy `ROOT` (or just `projected/*` + `manifest.json`) to a scratch
   location `ROOT_COPY`; run
   `B/target/.../v2_compat_fixture verify-and-update /path/ROOT_COPY --confirm-copy`.
5. Optionally, `A/target/.../v2_compat_fixture verify-raw /path/ROOT_COPY`
   (tree `A`'s binary opening tree `B`'s output) as a bounded structural check.
6. Optionally, in an explicit test invocation only,
   `A/target/.../v2_compat_fixture legacy-replay /path/ROOT_COPY/projected/checkpointed /path/scratch-out --explicit-test-mode`.

## What this does NOT prove

- Not proof of the release-frozen format from `docs/core/FORMAT_FREEZE.md`: the
  format is explicitly not yet frozen, and this fixture's baseline is
  pre-release.
- Not proof that the real `Database`-over-`PageWalStore` writer (once it
  exists) produces byte-identical files to this raw KV projection.
- `verify-raw` is not proof of old-binary *typed* reader compatibility.
- `legacy-replay` is not an automatic downgrade/migration path; it tests
  record/catalog/layout encodings only, and only runs when explicitly asked.
- Not a substitute for the lean/foundation gates, source-hash archiving, or
  full release acceptance — all of that stays with the parent per the task
  boundary that produced this tool.
