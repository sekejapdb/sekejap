# Phase 2 late-build results

Candidate evidence, 2026-09-17. Uncommitted change on engine HEAD `a69838c`
in the Phase 2 tree. No Phase 2 acceptance, commit or freeze is claimed here.

## What changed

Three format-neutral fixes to late index construction — the build that runs
over an already-loaded collection, not the always-on live maintenance path
that runs on insert/update/delete. Index bytes are unchanged; only how they
get written changed.

- **Scalar (fix A):** the build used to decode the whole row, including
  every vector sidecar, to read one indexed field. It now reads only the
  indexed field (`dense_v3::read_field`, `src/indexes.rs`,
  `scalar_build_key`). Each 256-row chunk is sorted ascending before
  insertion, and uniqueness is checked in that sorted order.
- **Text (fix B):** the build used to do a get+put of the posting, a
  get+put of the shared per-term document-frequency row, and a get+put of
  the corpus row and the norm row, once per document. It now accumulates a
  chunk (postings, per-term stats, norms, corpus delta) in a `BTreeMap` and
  flushes once per chunk, in key order (`analyze_row_bytes` /
  `build_documents`, `src/text_indexes.rs`). Rows that already carry a norm
  fall back to the old per-row `apply_transition` path. The per-posting
  absence probe (Law 5) was kept; removing it measured a further 1.65x and
  was not taken. Live maintenance is unchanged.
- **Atomic publication (fix C):** the build used to run every chunk inside
  one open transaction with zero intermediate commits. At 1,000,000 rows
  that transaction exceeded the page-WAL managed-byte allowance on Linux,
  and all three atomic-policy arms of the R7 benchmark were refused; at
  10K/100K it completed. `Database::build_index_to_ready` (`src/indexes.rs`)
  now commits chunks under a BUILDING flag in groups of 16, halving the
  group and rolling back to the last committed cursor on a refusal
  (verified 16 -> 8 -> 4 -> 2 -> 1). The final chunk and the READY flip
  share one transaction, so a query still never observes a partially built
  index. The bench's atomic arm (`src/bin/phase2_multimodel_bench.rs`) uses
  this path; the resumable policy is unchanged.

Diagnostics added to support this work: `Database::raw_for_each`,
`Database::pool_accesses`, `PageWalStore::pool_accesses` (doc-hidden, not
part of the public interface).

## Correctness oracle

`tests/index_build_equivalence.rs` (634 lines) loads 2,000 people and 1,000
organizations, builds scalar (`age`, `active`), text (`bio`) and spatial
(`home`) indexes, and takes a SHA-256 over every persisted key and value
under each index prefix.

| Index | Entries | SHA-256 |
|---|---:|---|
| scalar age | 2,000 | `6b7205dc…db63` |
| scalar active | 2,000 | `5349a371…0d09` |
| text bio | 15,782 | `e1b15c14…5dc1` |
| spatial home | 2,000 | `195919c4…c863` |

The digest is identical before the fixes, after the fixes, and after
reopening the database. Spatial and vector builders were not changed and
serve as a control.

## Page accesses per indexed row (Mac, 2,000 rows, 64-lane vector)

| Index | Before | After |
|---|---:|---:|
| scalar | 6.689 | 3.349 |
| text | 106.608 (109.591 before both fixes) | 52.341 |

## Wall clock, 200,000 rows, dim 32, atomic policy (Opus, Mac)

| Index build | Before | After |
|---|---:|---:|
| scalar (2 indexes) | 21.16 s | 2.96 s |
| text | 28.72 s | 6.24 s |
| spatial | 8.29 s | 1.51 s |
| vector | 7.47 s | 0.92 s |

Per-chunk commits made the full-fsync barrier dominant on its own: the 200K
scalar build took 20.8 s at one commit per chunk versus 3.0 s at one commit
per 16 chunks, over byte-identical indexes.

## Wall clock, 50,000 rows, dim 32, orchestrator (Mac, three arms, no reader)

| Stage | Atomic before | Atomic after | Resumable before | Resumable after | SQLite |
|---|---:|---:|---:|---:|---:|
| scalar, s | 0.531 | 0.705 | 5.179 | 4.907 | 0.047 |
| spatial, s | 0.268 | 0.383 | 2.131 | 2.117 | 0.268 |
| text, s | 4.493 | 1.429 | 6.991 | 3.712 | 0.061 (FTS5) |
| vector, s | 0.118 | 0.235 | 1.872 | 1.865 | n/a |

Final logical size moved from 42.9 to 42.8 MiB; loads, CRUD and query stages
were unchanged within noise. On macOS every commit is an `F_FULLFSYNC`; the
old atomic path made zero intermediate commits, and the new grouped-commit
path adds roughly 12 at this size, which raises the atomic scalar/spatial/
vector numbers above even though fix A and fix C reduce the work done. The
Linux run needed to isolate that fsync effect from the fixes has not been
done yet.

## Tests

90/0 integration, 75/0 lib, in both the default build and
`compact-cells,sqlite-balance,keyspace-append,slotref-split`. A full
workspace re-run was in progress at capture time.

## Named sacrifices

- **A:** a corrupt vector sidecar is no longer noticed by a scalar build.
  Spatial already made this trade; entity reads, vector builders and the
  verifier still catch it.
- **B:** RAM for one text build chunk is proportional to 256 documents, not
  to the whole store.
- **C:** build-level crash atomicity is replaced by resumable-style
  BUILDING progress. Nothing reads a non-READY index entry; a build either
  resumes or is cancelled with `begin_drop_index`. The group size of 16 is
  a starting guess — a wrong guess costs one rolled-back group, not the
  whole build.

## What this does not claim

- No claim about Linux wall-clock cost at 1,000,000 rows — only that the
  atomic-refusal class the pre-fix engine hit there is closed by this
  mechanism, verified so far at 50K and 200K rows on the Mac.
- No claim about live insert/update/delete cost; only the late-build path
  changed.
- No claim of an index format change; the oracle shows the bytes match.
- The 50K macOS timings are not a clean before/after read of fix A and fix
  C's CPU savings, because of the fsync asymmetry described above.

## Reproducible evidence

- `<scratch>/` (before) and
  `p2-mm-50k-after/` (after), each `arm-<engine>-<policy>/stdout.json`.
- `.insert-loop/p2final/multimodel-r7-1000000-partial-9arms.json`: the
  9-arm partial 1M Linux report against the pre-fix engine, showing the
  atomic refusals.
