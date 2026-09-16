# V2 disk-format qualification — 2026-09-16

Status: **qualified storage candidate; retain the changes**. This report distinguishes the storage compatibility
candidate from a public release and from completion of the multimodel product.
Live continuation: [PHASE1_STATE.md](PHASE1_STATE.md).

**Subsequent completion work:** the named `e4-format-v1` baseline and preserved
source/binaries/corpus are recorded in [FORMAT_BASELINE.md](FORMAT_BASELINE.md).
The remaining-release-artifact statements below describe this earlier candidate
qualification. Its full-workspace results remain valid for the unchanged engine.

## Scope

The current storage shape is `E4PWAL02`, 4096-byte physical version-1 pages,
4144-byte WAL frames, per-database compact-cell capability, dense-v3 typed
records, immutable layout IDs, binary JSON, point values and separate vector
payloads. [FORMAT_V1.md](FORMAT_V1.md) specifies the actual envelope and
extension rules. This loop adds no persistent field, encoding or index family.

The eight laws are adopted in CONTRACT.md. Minor updates must preserve the
existing feature set and keep reading/writing earlier released representations.
Future persisted indexes need noncolliding namespaces, versioned descriptors,
explicit feature enablement and their own permanent compatibility fixtures.
Nonexistent index encodings do not need to be invented to stabilize today's
entity storage. SQL/adapters, EXPORT/IMPORT and release packaging remain
separate product work.

## Concrete defects addressed

1. **Unsupported typed metadata was masked by a supported replica.** The
   opener could then truncate an uncommitted WAL tail or recreate coordination
   state. An intact unsupported packet now refuses immediately; a packet with
   a bad checksum can still fall back to an independently intact sibling.
2. **Inherited Store had the same version-fallback defect.** Both physical
   and logical unsupported versions now refuse without modifying files.
   Logical version admission also precedes version-specific payload/extension
   parsing, so a future shape cannot be mistaken for a damaged old shape.
   This inherited Store is not the selected typed-collection backend.
3. **Missing compatibility evidence silently passed tests.** Fixtures are now
   mandatory. The suite pins the original INDEX hash, verifies manifests and
   file inventories, checks independently modelled numeric identities and all
   expected documents through get/get-by-ID/scan, then repeats the complete
   oracle after update/insert/delete/checkpoint/reopen. Refusal checks preserve
   every file and directory entry, including coordination files.

The five original candidate fixtures remain unchanged. They cover both cell
codecs, checkpointed and committed-WAL states, limited policy, earlier/current
layouts, optional timestamps, JSON, points, vectors and deleted/reinserted IDs.
They were written by a prior codec-only build; they are cross-build candidate
evidence, not falsely attributed to a released binary. Preserve them alongside
the first release's separately captured corpus.

Named cost: unsupported inherited physical metadata requires one additional
fixed 4 KiB read to distinguish an intact future version from checksum damage.
Ordinary supported metadata does not take that path. Test fixture hashing and
complete oracle scans are qualification costs, not database runtime costs.

## Linux evidence

server namespace `sekejap-benchmark`, isolated Jobs with 2 CPU/2 GiB limits,
Rust 1.97.1, release profile, two build workers and serial tests. No production
workload was changed. Exact source archives and manifests identify each run.

| Check | Result |
|---|---|
| r1 previous-code refusal controls | Both intended tests fail; damaged-copy fallback passes |
| r1 fixed refusal tests | 3 passed |
| r1 fixture suite | 7 passed |
| r1 pinned-reader/WAL-cap lifecycle | 1 passed |
| r1 full default workspace | Failed three test assumptions; superseded by r2 |
| r2 future-layout admission negative control | Expected failure reproduced on prior code |
| r2 focused tests and final fixture oracles | Refusal 4/4, fixture/identity 7/7, reader/WAL cap 1/1 passed |
| r2 full default workspace | 422 passed, 0 failed, 2 ignored; EXIT=0 |
| r2 full compact/balance workspace | 427 passed, 0 failed, 2 ignored; EXIT=0 |
| r2 full retained-feature workspace | 427 passed, 0 failed, 2 ignored; EXIT=0 |

The pre-existing ignored tests are explicit limitations, not successes. Default
mode ignores the shuffled-density defect without neighbor redistribution and
the large Store forensic probe. Compact/balance mode instead includes the
ignored historical persisted scan/get-disagreement fixture (which requires the
retained external fixture) and the same forensic probe. This loop did not add
those ignores or claim their underlying historical questions are resolved.

r1 source SHA256:
`d3625faf29644a3eb86cf1c1c7d5fc666a2a0e57ecbe0dde82624f5487552901`.
r2 source SHA256:
`0fc052d05f55ec48eb3c45ca129319a8a97cb2d0ff15de2901131dbd12a71075`.
Linux evidence roots: `<scratch>` and
`<scratch>`.

The r1 full-suite failures were investigated rather than excluded wholesale:

- The WAL-limit test did not pin a reader, so the new clean-transaction
  checkpoint legitimately freed old WAL space. The corrected test pins the
  old snapshot, retains the exact byte cap, checks two precise limit refusals
  and rollback, and verifies the snapshot survives.
- The split byte-identity corpus compared all pages successfully, then required
  a redistribution branch that is not compiled without `sqlite-balance`.
  Coverage now follows the compiled policy; byte/root/row comparisons remain.
- A density test expected balanced fill while balancing was disabled (observed
  0.503/0.529 for the lower keyspaces). Its unchanged density threshold applies
  to balancing builds. The append-only cost target likewise applies only when
  append optimization is enabled; standalone controls remain in every build.

## Existing performance evidence, not a fresh measurement

The previous team's Pi 1M append-split candidate took 146.4 and 154.8 seconds
for the complete 2.4M-operation workload; paired SQLite took 96.0 and 95.8
seconds. E4 final logical storage was 376,598,624 bytes; SQLite approximately
339 MB. The later slot-reference split improved the 300K load phase, with
effectively neutral whole-workload time. Do not extrapolate a proven combined
1M improvement or use final size as evidence of peak allocation.

This loop does not change the byte format or reopen performance research.
Prior measured performance, peak-space and wider eight-law limitations remain
visible in the foundation reports; passing compatibility tests alone does not
qualify them.

## Decision and next boundary

All three final Linux workspace commands passed: **1,276 test executions**,
zero failures, two pre-existing ignored entries per mode. The focused positive
checks also passed, and all three intended negative regressions failed against
the corresponding previous code. The 66 original fixture-corpus files remain
byte-identical. Runtime/tests/Cargo in the retained worktree match the exact
r2 source manifest.

Retain the codec policy, refusal fixes, measured split optimizations and
compatibility gate. The documented current disk shape is sufficiently
established to proceed with interface work; do not restart general insertion
optimization as a prerequisite. This is acceptance of the storage candidate,
not a public release or an assertion that all eight laws are fully qualified.
Before the first public automatic-update compatibility promise, capture the
selected release binary and its independently specified immutable corpus. Keep
the existing candidate corpus too. Future shipped indexes receive their own
versioned compatibility corpus; they do not justify rewriting existing entities.

Evidence: [phase1-evidence/README.md](phase1-evidence/README.md), raw logs,
source manifest and top-level target counts. Final Job marker
`PHASE1_R2_EXIT=0`, 2026-09-16 11:51:06Z. Exported archive SHA256
`93b1d2ad381a5db8d657939e0445a1f2603a31390974fb4a80d66665d9b79e73`
was verified locally before acceptance. Generated test databases use isolated
Linux temporary directories; retained artifacts are source, fixtures and
evidence. No production deployment or new timed benchmark was performed.
