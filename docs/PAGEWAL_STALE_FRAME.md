# Page-WAL stale-version correctness loop — 2026-09-13

The baseline is committed as `ee98182`; the measured v2 candidate is committed
as `2c56936` on `pagewal-foundation`. This loop found and fixed a separate,
deterministically reproducible page-WAL snapshot bug. Release promotion remains
blocked by the outstanding F1 gates. The seven laws are unchanged.

## What the two failed control files establish

Both failures came from the older copy-on-write Store, not the page-WAL arm.
Each workload loaded 400,000 records with 8-byte keys and 256-byte values, then
performed rounds of 80,000 updates, 40,000 deletes and 40,000 inserts. Each
transaction contained 1,000 changes and used native FULL durability.

Regenerating the exact successful endpoints establishes:

| Failed endpoint | Only differing page | Generation in failed page | Generation in successful page |
|---|---:|---:|---:|
| Original round 6 | 26840 | 1043 | 1207 |
| Fresh reproduction, round 11 | 30960 | 1967 | 2130 |

Every other page, including every parent pointer, is byte-identical. Each file
contains 32,164 pages. The failing pages contain older, checksum-valid versions.
The earlier hypothesis of an incorrectly constructed parent reference is
superseded: the reference agrees with the successful tree, but its physical
page contains stale bytes.

This does **not** identify why the native stale pages occurred. A dropped or
subsequently reverted write is consistent with the evidence; hardware failure
and an engine-side failure to persist an image are not distinguished. The old
Store is not declared fixed. Neither original failed database has been repaired
or overwritten.

Additional exclusions and limits:

- A clean 12-round Store database exactly matches the earlier successful
  control's SHA256: `721f42ce3ebf3160973a8b3902377b3712eca11d3a7e01ddbe8f9003fccc45bf`.
- Old and clean kernel instruction listings match after normalizing LLVM's
  generated label IDs: 235,320 instructions. This comparison is not a complete
  binary-equivalence proof, but does not support a changed kernel instruction
  stream as the explanation.
- Generated small-cache and 400K B-tree probes pass. A 100-round FULL Store
  probe comparing disk reads against checksums of the last issued page writes
  also passes. These diagnostic passes do not clear the native failures.

## The page-WAL defect and fix

A new fault substitutes an older, valid frame for a committed WAL frame of the
same page. Both the page CRC and outer frame CRC are valid. V2's snapshot reader
accepted the old value because its index remembered only the frame's offset.
The regression fails on v2 before the fix.

V3 remembers the **expected frame checksum as well as its offset**, and checks
it during snapshot reads, writer reads, checkpoints and repair-overlay reads.
The 16 MiB WAL limit permits a 32-bit offset. Offset plus checksum occupies the
same eight bytes as the former 64-bit offset. There is no on-disk format change,
additional database file, or per-entry memory increase. The checksum already
exists in each frame; the read path adds a comparison, not another CRC pass.

The stale-frame regression now refuses the snapshot read and checkpoint,
poisons the writer, and proves the damaged WAL is preserved. Ordinary reopen
also refuses the corrupt transaction without truncating its evidence. Restoring
the test's injected substitution proves the acknowledged value can reopen.

A second test makes a data-page write return success while doing nothing.
Existing checkpoint read-back verification catches the mismatch **before
discarding the committed WAL**. Reopen recovers all 120 acknowledged records,
including the update/delete/insert mix. This distinguishes byte integrity from
having the correct committed version.

CRC32C remains an accidental-damage detector, not protection against an
adversary deliberately arranging matching checksums. This fix does not rebuild
missing WAL contents or provide rootless current-membership proof.

## Validation and measurements

Mac: full release workspace suite **366 passed, 0 failed, 2 ignored**. The
ignored tests are the retained failing control fixture and the large forensic
I/O probe; the latter was run explicitly for 20 rounds without FULL barriers
and 100 rounds with FULL barriers. The page-WAL fault suite passes four tests,
including the existing **356 injected cases / 712 reopen checks** and both new
stale-page cases.

Rotated v2/v3/SQLite comparisons and selected Pi validation are in progress.
Results will be recorded here before this subloop is closed. The Pi's verified
stable address is `contributor@example.invalid`.

Evidence: `<scratch>`, and the
corresponding `artifacts/pagewal-correctness-20260913` directory under the
authorized Pi task root. `compare-6.json` and `compare-11.json` describe exact
page differences. Raw logs, source archives and benchmark binaries are retained.

Corrupt-WAL-region salvage, complete repair failure/budget coverage,
cross-process readers, large-value resize parity and typed collection
integration remain open. No SQL or new multimodel indexes were implemented.
