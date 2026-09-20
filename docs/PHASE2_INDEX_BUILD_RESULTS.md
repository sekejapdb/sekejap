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
  `build_documents`, `src/index/text/mod.rs`). Rows that already carry a norm
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

## Converged CRUD/build table, 2026-09-17

Later loop iterations continued past the fixes above. This table is the
converged state of the same benchmark: Mac, 50,000 rows, retained-feature
build (`compact-cells,sqlite-balance,keyspace-append,slotref-split`), matched
durability (SQLite `PRAGMA synchronous=FULL` plus `fullfsync=ON`, versus E4
`F_FULLFSYNC`). Bench `src/bin/phase2_multimodel_bench.rs`, runner pattern
`/tmp/e4-mm50k-fair.sh`. Ratio is E4 atomic-arm wall time ÷ SQLite wall time
(the "Wall clock, 50,000 rows" table above is an earlier point in the same
loop and is superseded by this one for the atomic arm).

| Stage | E4 atomic ÷ SQLite | Notes |
|---|---:|---|
| Spatial index build | 0.54 | E4 faster |
| Load entities | 0.92 | E4 faster |
| Load relationships | 0.95 | E4 faster |
| Updates, three rounds | 1.04 | |
| Deletes, three rounds | 1.06 | |
| Final file size | 1.12 | 37.8 MiB E4 vs 33.9 MiB SQLite |
| Reinsert + edges, three rounds | 1.29 | |
| Scalar index build | 1.67 | |
| Text index build | 1.92 | 0.17 s absolute for E4; small absolute time, noisy |
| Post-CRUD query, spatial | 0.45 | E4 faster |
| Post-CRUD query, text | 0.82 | E4 faster |
| Post-CRUD query, vector | 0.93 | E4 faster |
| Post-CRUD query, scalar | 1.17 | |

Any older single-number quote for these same stages elsewhere in the docs
(for example the R7 10K no-reader CRUD medians in
`PHASE2_ACCEPTANCE_REVIEW.md`) is superseded by this table for the converged
50K state; it is kept in place and labeled, not deleted, because it reflects
a different row count and an earlier point in the loop.

### Loop history (commits on `pagewal-foundation`, leading to this table)

- `06b16ee` — Phase 2 baseline for this loop.
- `e142091` — vector scan and edge probes.
- `76aa411` — sorted scalar/spatial builds.
- `f394bc8` — I/O counters, a 3-sync checkpoint, and a lost-truncate fault
  test.
- `c92ed3e` — checkpoint reduced to 2 syncs.
- `dbdbd71` — loop 4: text segment tier tags (`0x7A`/`0x7B`) under feature
  bit `0x40`; per-index B-trees for scalar and spatial under feature bit
  `0x80` (descriptor v2, `tree_id` plus root); zero-allocation vector scan;
  a range-graft kernel primitive plus two latent defect fixes it uncovered;
  a per-handle layout switch. Wall-time ratio (E4 ÷ SQLite) at this point in
  the loop: text build 19x to 1.4x, scalar build 5x to 1.7x, spatial 0.5x,
  vector top-10 0.9x, updates 0.9x.
- `39d52fc` — rollback now clears all per-tree append hints. Defect found by
  the rejected item F (below): after rolling back an ascending-key write
  into a per-index tree, the stale hint read past end of file.
- `a39dc8f` — the accepted index feature mask is named once, as
  `collections::SUPPORTED_LOGICAL_FEATURES` (`0xff`); the engine revision is
  recorded in the final Linux job.
- `f538d4d` — bench driver fix, not an engine change: the edge-restore loop
  had been committing every 256 edges (about every 86 people), while SQLite
  committed every 256 people; E4 now commits once per reinsert cycle at
  SQLite's rate (40 commits, down from 79). Reinsert+edges wall-time ratio
  moved from 2.05x to 1.29x (E4 ÷ SQLite) as a result.

### Rejected, not kept

Preserved under `.insert-loop/loop3/`, not merged: insert-probe, text sorted
build, encode-once, quantized candidate planner, range-graft into the shared
tree, and item F (graph edges in their own per-index trees). Item F cut pool
accesses 22.7% but left wall time, frame count and fsync count unchanged, so
it was not kept — per the loop-retrospection rule, a change that does not
move the target number is reverted unless it earns a named non-speed good.

### Root causes, established by measurement

- The update gap tracks checkpoint fsync count (4 syncs down to 2), not page
  volume and not allocation count.
- The build gaps trace to per-posting and per-term read-modify-write, and to
  one shared B-tree defeating append/pack locality — this is what fixes A/B/C
  above and the `dbdbd71` per-index trees address.
- The reinsert gap was a benchmark commit-batching mismatch, not an engine
  cost: E4 79 commits vs SQLite 40 per cycle, 7,355 vs 4,620 frames, 92 vs 40
  fsyncs, closed by `f538d4d`.

## 2026-09-18 — B1 closes the 1M atomic-build refusal; committed head qualified Linux GREEN

Head `aeeae13` (the fixes described above, through `f538d4d`) passed Linux
final qualification attempt 4, exit 0, all stages including stage 5 release
binaries — see `PHASE2_STATE.md`, "Linux qualification attempt 4 GREEN".
That closes fix A/B/C above as qualified, not just Mac-measured. The items
below are staged on top of that head, not yet committed.

### B1: bounded index build closes the 1,000,000-row atomic refusal

Fix C above (grouped commits under a BUILDING flag) reduced the atomic
build's commit-window size but still packed the index in one pass; at
1,000,000 rows on Linux that pass still exceeded the page-WAL allowance and
the build was refused with an error type (`NotFound("index")`, see root
cause below) that the driver's own refusal-retry logic did not catch, so the
run exited 1 with no result. B1 changes the build's own structure: the
first sorted run is packed to a quarter of the page-WAL allowance, and the
remainder is appended across its own subsequent commits, so a refused build
never loses the index's registry entry — only pending USER writes are
refused by the guard, not the build's own progress commits. The
1,000,000-row build that previously died with `NotFound("index")` now
completes end to end.

Named sacrifice: the 200,000-row scalar build costs +0.19 s versus the fix
A/B/C state above, for the bounded-build safety margin.

Root cause, `.insert-loop/loop3/ROOTCAUSE-1m-index-notfound.md`: the
pre-B1 atomic build created the index and drove it to READY with no commit
in between; a refusal at scale rolled back the uncommitted CREATE along
with the failed pack, so the retry looked up a registry row that no longer
existed. 50K and 200K never exercised this path because the whole-index
pack fit the allowance at those sizes.

### T2 and Q5: further read-side index-adjacent fixes

- **T2**, the BM25 page path skips a redundant winner-existence probe:
  `bm25_one_term` 1.62x to 0.84x (E4 ÷ SQLite wall time, matched durability,
  retained build, Mac, `two_ways` 20K rows), two-term 1.47x to 0.79x,
  `bm25_common` 3.81x to 2.86x.
- **Q5**, a range posting is trusted as an existence proof the same way an
  equality posting already was, letting same-index filters fold:
  `range_two_sided` 2.07x to 0.38x, `range_open` to 0.51x, `range_closed`
  to 0.49x, `between` to 0.81x.

Full list of everything staged on top of `aeeae13` — Q2, H4, B1, Q3, Q4,
G2, T2, Q5, and the pending K1 — is in `PHASE2_STATE.md` under "Staged,
uncommitted items on top of `aeeae13`"; that is the current source for the
per-item ratios so they are not duplicated here.

### 1,000,000-row matched pair, E4 with B1, Mac

Ratios are E4 ÷ SQLite wall time. Load entities 0.99, load relationships
1.27, build scalar index 1.47, build spatial index 0.36, build text index
1.82, updates (three rounds) 0.93, deletes (three rounds) 0.91, reinsert +
edges (three rounds) 1.38, final file size 1.15, query scalar 1.40, query
spatial 1.52, query text 0.99, query vector 0.90.

This supersedes the "No claim about Linux wall-clock cost at 1,000,000
rows" line under "What this does not claim" above for the Mac side only;
the Linux 1,000,000-row run with B1 has not happened yet, since the
qualified Linux job runs from committed source and B1 is still staged. The
Linux SQLite-only reference from the pre-B1 partial 1M run remains useful as
a baseline: updates (three rounds) 1,303 s, reinsert 97.7 s, final file size
660 MiB (server, 2 CPU) — kept for comparison, not combined with the Mac E4
numbers above, which are a different machine.

### Measured and rejected, this stretch

- Per-row field offset table: would change the on-disk row format; the
  field walk it would remove is 15-21% of per-row cost against a +3.6%
  disk-size cost, and no `two_ways` case crosses the 2x gate as a result.
  Not taken.
- Per-tree edge tags for 1-hop graph reads: same B-tree height at 50K and
  1M rows by fan-out arithmetic (`.insert-loop/loop3/DECISION-graph-1hop-parity.md`);
  would reduce pages touched, not descent depth, matching item F's earlier
  measurement (-22.7% pool accesses, no wall-time change, reverted). Not
  taken.
