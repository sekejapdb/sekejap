# Page-edit foundation exploration — 2026-09-13

Scope: how existing B-tree pages are changed within an existing transaction.
No query execution, SQL interface or new multimodel index is part of this loop.
The baseline is the accepted persistent-freelist engine, archived at
`<scratch>` with SHA256
`2afe410f8410ccd0a56ba773ca1d8eb4812021e320830e79f33c6ce06ccc88c6`.

## Measurement before implementation

A separate 400K two-cycle baseline run was sampled for three seconds immediately
after initial load and verification. Its full row/reopen oracle passed. The Mac
main thread had 2,377 samples, of which 1,563 (65.8%) were inside commit. Explicit
`PageMut::compact` frames accounted for 48 samples (2.0%), and write descents for
81 (3.4%). Inlining and short sampling limit attribution; these are diagnostic
wall-time observations including waits, not exact CPU percentages or benchmark
results. They make a large Mac mixed-write gain from repacking alone unlikely.
The diagnostic is kept separate from all timing comparisons.

Source review found one allocation per surviving cell during page repacking,
and complete remove/repack/reinsert work for every equal-size replacement.
The allocation test reproduces 121 allocations for 120 surviving cells.

## Two independently measured primitives

1. **Scratch:** repack from one fixed 4 KiB stack image, rather than a vector of
   separately allocated records. Record ordering, resulting offsets, checksums
   and serialized formats follow the existing packing rule. Cost: one 4 KiB
   temporary stack image per active repack; no database-sized resident state.
2. **Cell:** when the fully encoded replacement has the same size as its old
   cell, overwrite that cell in the already shadowed writable page. Other
   records and the slot directory stay in place. Different sizes use the
   existing path. Old overflow chains are still independently verified and
   retired; new chains still use ordinary checked overflow storage. Published
   snapshot pages are never edited. Cost: an equal-size check on replacements;
   existing unrelated holes wait until an insertion needs compaction.

SQLite source at retained commit `f3b9f74d81132426dee1ccc07a67fdad2ccfeaa9`
informs these primitives: `src/btree.c:1716` repacks through pager scratch space,
and `src/btree.c:9391` overwrites a matching cell. E4 retains its own shadow-page
ownership, checksums, durability barriers, WAL and recovery protocol.

Scratch and cell are each compared independently, then together, against current
E4 and native SQLite. The benchmark uses two reversed-order repetitions of
100K people with four mutation rounds: mixed rounds contain 20K updates, 10K
deletes and 10K replacement inserts; update-only rounds contain 20K replacements.
Each database has two collections, external keys, scalar/binary-JSON/point values
and four exact f32 vector lanes. Both start with the same logical records.

All arms retain 1,000 operations per transaction, 8 MiB cache, buffered I/O,
timestamps off, native FULL settings and SQLite's normal WAL autocheckpoint.
Load and mutation time are separate, including phase-ending checkpoint work
and excluding complete row verification. Logical/allocated peaks include all
database files; sampled peaks remain lower bounds, not enforced caps.

Acceptance requires repeatable meaningful end-to-end gain (at least 10% on a
named workload), device validation and unchanged capacity/corruption/snapshot/
plateau gates. Red tests establish per-record allocation and equal-size cell
relocation before their respective implementations. Changing-size and overflow
oracles exercise the fallback path and pinned readers across writer reopen.

Artifacts: `<scratch>/`.
Isolated source: `/tmp/e4-batch-candidate`. The accepted production source is
unchanged while the experiment runs. Final decision and audited numbers follow.

## Verdict: rejected and reverted

The primitives passed their selected safety checks but did not meet the
10% end-to-end significance gate. All three prototypes are rejected for now.
The accepted E4 engine remains the persistent-freelist version from the prior
loop; its 347-test Mac / 182-test Pi validation is unchanged. No query or
multimodel-index code was changed.

| Engine / prototype | Initial load, mixed case s | Mixed mutations s | Initial load, update case s | Update-only mutations s |
|---|---:|---:|---:|---:|
| Accepted E4 | 4.766 | 7.343 | 4.606 | 3.267 |
| Scratch-only — rejected | 4.621 | 7.134 | 4.596 | 3.087 |
| Cell-only — rejected | 4.680 | 7.005 | 4.794 | 3.215 |
| Both — rejected | 4.561 | 7.045 | 4.766 | 3.218 |
| SQLite | 1.505 | 3.249 | 1.626 | 2.046 |

Each timing is the mean of two reversed-order repetitions, in seconds. The
mixed case performs 160,000 mutations total against 100,000 initial people;
the update-only case performs 80,000 replacements. Initial load is excluded
from mutation time. SQLite remains faster in both comparisons.

| Prototype | Mixed reduction vs E4 | Update-only reduction vs E4 |
|---|---:|---:|
| scratch | 2.84% | 5.52% |
| cell | 4.60% | 1.59% |
| combined | 4.06% | 1.48% |

The largest mean improvement is 5.52% for scratch-only updates; it varies
from about 2.5% to 8.5% across the two repetitions. Combining both changes does
not produce a larger end-to-end gain. This is a negative performance gate,
not a claim of observed data loss.

| Case | All E4 variants peak / final MiB | SQLite peak / final MiB |
|---|---:|---:|
| mixed | 35.174 / 35.174 | 33.961 / 29.258 |
| updates | 33.085 / 33.085 | 32.099 / 27.188 |

Logical peaks and final sizes are identical between all E4 variants. There
is no disk-density benefit. Allocated-size variation remains in the raw report.
All 20 timing arms passed full row/reopen checks; 24 five-way state comparisons
agree. The combined prototype passed 176 selected kernel, resource, overflow,
corruption and packing checks; no full shipping-suite pass is claimed for it.

No Pi timing matrix or larger 400K acceptance run was launched after the
Mac gate failed. The Pi was queried only for profiling support; `perf` was not
installed and nothing was installed or reconfigured. This result does not prove
the same percentages on Pi.

The three-second Mac profile remains the useful direction signal: commit
work dominated that sample, so fewer temporary allocations cannot remove most
of the observed time. A future commit-protocol experiment needs its own proof
of durability, snapshot visibility, bounded metadata and peak-space behavior.
Increasing transaction size or weakening durability is not this result.

See [audited results](PAGE_EDIT_RESULTS.json) and [source/binary provenance](PAGE_EDIT_PROVENANCE.json).
The combined source archive plus `scratch-page.rs`, `cell-page.rs` and
`combined-btree.rs` reproduce the individual ablations. Their retained binaries
were built with identical benchmark harnesses and are fingerprinted separately.
The isolated source is restored to the accepted engine after archiving.

## Evidence retention and cleanup

Rejected prototypes and binaries, raw reports, logs, profile and hashes
remain under `<scratch>/`. The first
mixed run retains accepted E4, cell-only prototype and SQLite databases.
The other 17 timing databases and the diagnostic profile database were
deleted after report and source-archive verification: 744,787,968 allocated bytes removed (file block accounting; filesystem free-space changes can differ).
See [cleanup manifest](PAGE_EDIT_CLEANUP.json).
