# Recovery blockers loop — 2026-09-15

This loop is limited to the three pre-freeze page-WAL failures. It does not
claim Law 8 or overall foundation qualification.

## Result

The page-WAL pilot now uses `E4PWAL02` frames with a 16-byte database identity.
The durable metadata header carries the identity, feature mask, checkpoint
transaction floor, root, freelist head and disk allowance. Recovery validates
these before any WAL truncation or checkpoint mutation.

The opener separates inspection from normalization. It can inspect a candidate
without changing it; only a fully validated writer opener truncates an
uncommitted WAL tail. Metadata is written to two checkpoint copies and read back
before the WAL is removed. A checkpoint that dies during data or metadata writes
leaves the WAL available for the next opener.

## Historical focused evidence (not Linux qualification)

`cargo test --manifest-path sekejap-e4/Cargo.toml --test pagewal --test pagewal_repair --test pagewal_recovery_identity`

passed:

- 13 page-WAL lifecycle, cap, snapshot and checkpoint-fault tests;
- 1 source-preserving repair test;
- 6 identity/recovery tests covering foreign WAL, stale WAL rollback,
  interrupted reset, invalid root, damaged metadata and unsupported feature
  refusal.

The six identity tests specifically verify that:

1. a valid WAL from another database is refused before either file changes;
2. a checksum-valid older WAL cannot replace a newer checkpoint;
3. a WAL tail is preserved across an interrupted reset and can be reopened;
4. invalid roots are refused without tail cleanup;
5. one damaged metadata copy can be recovered while continuing to write; and
6. unsupported metadata/features preserve both metadata copies and the WAL.

## Earlier deferred evidence

The broader `recovery_faults` integration suite was first attempted in the
macOS sandbox, where its fixture allocator could not create
`<artifact dir>/test-tmp`. That attempt stopped during fixture
setup and was not recovery evidence. The Linux resume below supersedes this
specific deferral. The page-WAL pilot remains isolated from the collection
engine, and release fixtures across binaries are still required.

No benchmark, interface, or multimodel-index work was included in this loop.

## Linux resume evidence — 2026-09-16

A Kubernetes benchmark server job completed with `FINAL_EXIT=0` from
tested source archive SHA-256
`9729f1b5b72afff0bba0056700bedb91692f747793ba387644dcb509c02f8153`.
Both the default build and `compact-cells,sqlite-balance` build passed all six
commands:

- 13 `pagewal`, 6 `pagewal_recovery_identity`, and 1 `pagewal_repair`
  integration tests per build;
- 5 page-WAL unit fault tests per build;
- all 14 inherited-engine `recovery_faults` tests per build.

The default fault run exercised 520 damaged-header failure cases with 1,040
reopen checks, plus 384 general I/O failure cases with 768 reopen checks. The
compact build exercised 488 damaged-header failure cases with 976 reopen
checks, plus 360 general I/O failure cases with 720 reopen checks. The count
difference follows the feature-specific I/O trace length; neither build skipped
the damaged-header regression.

The retrieved Job summary, evidence checksums, and tested-source manifest are
checked in under `docs/recovery-evidence/`. All nine reported source, test and
Cargo files match the archived tested source byte-for-byte.

This is Linux evidence for the three scoped blockers: a foreign WAL is refused,
a checksum-valid stale WAL cannot roll back a newer checkpoint, and unsupported
metadata/features are refused before opener normalization mutates the source.
It also validates the checkpoint rule discovered during review: when one
metadata copy is damaged, checkpoint repairs and durably verifies that copy
before replacing the remaining valid copy.

A separate fixed-order negative control, isolated from the positive source
tree, proves that the regression detects the unsafe ordering. It changed only
`src/pagewal.rs` (SHA-256 `2a9d433f...` to `e7024c6d...`) to force metadata
writes to `[0, 1]`; the format and fault-test source hashes remained identical
to the positive archive. The exact test failed at damaged-copy case
`case-1-56-2`: an injected partial write destroyed copy 0 while copy 1 was
already damaged, and reopen refused with
`WAL ownership/history lacks checkpoint metadata`. Cargo reported 0 passed / 1
failed, as required. The r2 runner's first evidence parser stopped after
`TEST_EXIT=101` because it expected the test name and `FAILED` on one line;
that was a wrapper assertion failure after the intended Rust failure, not a
test rerun or engine result. A separate read-only PVC Job printed and validated
the retained log and hashes with `NEGATIVE_EVIDENCE=VALIDATED`. Its stdout is
preserved at `docs/recovery-evidence/negative-fixed-order.stdout.log`.

## Limited eight-law assessment

This recovery loop does not qualify the whole engine against the eight laws.
Its contribution and remaining gaps are:

1. **Disk-first:** recovery inspection retains the existing 16 MiB WAL bound,
   but this run did not measure whole-process memory across the complete engine.
2. **Cost proportional to change:** checkpoint applies WAL-indexed changed
   pages, but existing scattered-work failures and broader scaling gates remain.
3. **Nothing fallible may delete:** metadata copies are written, fully synced,
   read back and decoded one at a time before WAL deletion. The fault suites
   pass; full operator publication and repair interruption coverage remains in
   the parent recovery work.
4. **Name the sacrifice:** the v2 identity trailer adds 16 bytes per WAL frame,
   increasing it from 4,128 to 4,144 bytes (about 0.39% frame overhead), with no
   record-encoding change. The pilot already reserved two metadata pages; this
   change populates the existing page 1, so it adds no data-page allocation
   versus that baseline. Safe ordering adds one FULL barrier versus the immediately prior two-copy
   candidate: the checkpoint now has its data barrier plus one barrier per
   metadata copy, with independent read/decode. This loop did not benchmark
   that cost.
5. **No corruption unrecoverable:** the three known recovery blockers and the
   one-damaged-header checkpoint case pass on Linux in both feature modes.
   This is scoped fault evidence, not completion of the full recovery matrix.
6. **Writes never block or degrade reads:** unchanged and unqualified here;
   cross-process reader visibility and degradation gates remain open.
7. **Usable target-device ingest:** unchanged and unqualified here; no bulk,
   late-indexing, live-write, or reopen performance measurement was run.
8. **Compatibility is permanent:** unknown features are source-preservingly
   refused, and feature modes pass independently. Preserved cross-release
   fixtures, older-writer round trips, persisted indexes, typed collection
   integration, and minor-version rollback remain pending. `L8-COMPAT` must
   remain pending.

No Mac database test, broad benchmark, query/index/interface change, collection
promotion, or production mutation was performed in the resumed loop.
