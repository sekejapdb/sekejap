# Phase 2 acceptance review

Status: incomplete candidate, 2026-09-17. This review uses the scope in
[PHASE2_WORKLOAD.md](PHASE2_WORKLOAD.md). tracker owns live task status.
No Phase 2 release or accepted commit is claimed here.

The working tree now includes an **unaccepted query-only scalar-membership
experiment** after preservation of the complete three-trial 100K no-reader
baseline. The full-workspace results below qualify the captured R7 runtime,
not this later experiment. Native query tests pass on the Pi in both build configurations (22 tests).
Baseline/candidate/SQLite read-only replay still must justify retaining it;
source backups permit an exact revert. All ongoing R7 scale and queued lifecycle runs use immutable baseline
source and are unaffected by the local experiment.

The format baseline remains `e4-format-v1`, engine `59d1cbc`. Phase 2 adds
explicitly enabled, versioned families; ordinary opens do not promote existing
databases. Graph edges and raw vectors are authoritative data. Derived lookup
structures can be rebuilt explicitly into a separately verified destination.

| Requirement | Current evidence | Remaining acceptance work |
|---|---|---|
| Consumer contract and E3 reuse | Workload contract, `PHASE2_E3_AUDIT.md`, runnable combined-query example | Preserve the scoped embedded API and document subsequent product interfaces |
| Catalog, scalar, graph and atomic maintenance | Independent semantics/lifecycle/admission tests; exhaustive family key-write fault sweeps; final runtime workspace 547 default / 552 retained passing tests; late-build cost and closed atomic-refusal class in `PHASE2_INDEX_BUILD_RESULTS.md` | Preserve final accepted source and evidence |
| Exact and explicit approximate vector | Independent exact oracles; quantized lifecycle/admission/write-fault tests; `PHASE2_QUANTIZED_BENCH_RESULTS.md` | Final-source scale cost; keep synthetic recall and linear-scan limits explicit |
| Spatial and full text | Independent geometry/analyzer/BM25, lifecycle and fault tests; both full workspace builds pass; late-build cost in `PHASE2_INDEX_BUILD_RESULTS.md` | Phrase complete on Mac in both builds; native qualification pending in the final job; larger workload costs remain |
| Combined embedded queries | `PHASE2_QUERY_GUARD_RESULTS.md`: 93 checks/build and example; complete pagination cases | Larger matched query/CRUD measurements |
| Compatibility | Preserved Phase 1 checks plus `PHASE2_MULTIMODEL_COMPAT_RESULTS.md` and `PHASE2_QUANTIZED_COMPAT_CRASH_RESULTS.md`; complete format registry/descriptors documented | Pi graph-free/lifecycle corpus and416rollbackcycles pass; baseline R7 frozen Phase1 passes20arms. Cycle-capable artifacts are archived and hash-verified; restoration/replay passes; qualify the final accepted runtime |
| Recovery and interruption | Verifier/rebuild reports; 84 latest crash/lifecycle/checkpoint cases; both workspace builds pass; all 12 R7 resource refusals independently verify committed state | Preserve evidence and state tested damage/resource limits precisely |
| Fair total cost | Scalar R6, quantized probe, corrected multimodel R7 10K report: 24 completed / 12 verified refusals, three trials; SQLite schema and recursive plan corrected | 100K complete (24 workloads / 12 verified refusals); separate 2K/1536-dimensional matrix complete; finish representative 1M and assess measured tradeoffs |
| Acceptance and commit | Named source/test/report artifacts exist | Resolve measured failures or explicitly assess product tradeoffs; complete scoped audit and commit accepted files without co-author |

## Issues that must remain visible

The corrected R7 10K matrix completed 24 of 36 workloads. All no-reader and
batch-reader arms completed; all 12 E4 round-held/fully-held arms refused at
the fixed 16 MiB WAL allowance. Each refusal independently verifies the exact
committed population. A successful capture or verified refusal does not mean
the requested workload completed. Snapshot lifetime remains a product limit.

R4 graph timings and its redundant SQLite edge schema are superseded by R7.
No-reader R7 medians are 13.916 seconds E4 atomic / 6.526 seconds SQLite for
three complete CRUD rounds, and 8.664 / 6.723 MiB after-close logical size.
This R7 number is at 10K rows and is kept for the record, not deleted; it is
superseded for tracking progress by the converged 50K-row CRUD/build table
dated 2026-09-17 in `PHASE2_INDEX_BUILD_RESULTS.md`, which is the current
reference. The selected graph-filtered and spatial queries favor E4; text
late build and text-plus-vector remain slower. Larger-size evidence and an
explicit product tradeoff assessment remain necessary; correctness alone is
not a speed win.

A dedicated graph-traversal bench (50K people / 150K edges, Mac, 2026-09-17)
found the traversal read path doing one unnecessary point lookup and a
discarded JSON decode per edge, against the opposite-direction copy; a
staged, uncommitted fix removes it. Full before/after numbers, cause, and
the remaining allocation-free range-walk pass (item H2) are recorded in
`PHASE2_STATE.md` under "Graph traversal read-path fix". The
members-of-organization fan-in case stays above the 2.0x E4-versus-SQLite
wall-time acceptance gate until H2 lands.

Sampled no-reader median logical peaks are 13.308 / 12.231 MiB. The largest
allocated peak across completed no-reader/batch runs is 24.566 MiB E4 versus
14.754 MiB SQLite. This does not establish a physical-space ceiling. Refused
reader arms must retain their own footprint and incomplete-work accounting.
See [R7 results and full tables](PHASE2_MULTIMODEL_R7_RESULTS.md).

The pre-fix engine refused all three atomic arms of the partial 9-arm 1M
Linux run at the fixed WAL allowance, during late index build. A
grouped-commit build driver closes that refusal class at 50K and 200K rows
on the Mac; see [late-build results](PHASE2_INDEX_BUILD_RESULTS.md). Linux
confirmation at 1M has not run yet.

Linux final qualification (server job `e4-phase2-final-20260917`) is now on
its third attempt. Attempt 1 on `dbdbd71` failed only on packaging (an
archiver fix, now applied); attempt 2 on `dbdbd71` passed both full-workspace
build stages and failed only on a lifecycle-replay harness mismatch (stale
feature mask in fixture binaries, not an engine defect); attempt 3, on
`f538d4d`, is running now and its result belongs in `PHASE2_STATE.md` under
"Linux final qualification status" once it completes. Remaining before
acceptance: that result, item H2 above, a matched 1,000,000-row run on Linux
for the converged CRUD/build table, and the in-progress two-arm lean bench
as a post-change smoke check.

Known limits that are stated rather than fixed: a held reader lets the WAL
grow to its fixed allowance and then writes are refused with committed state
verified, matching SQLite's behavior under a long-held reader; the text
head-to-segment fold is not implemented; packed text entries stay tombstoned
until an explicit rebuild; quantized-vector recall has only been measured on
synthetic data; each index created consumes tree-id space from a `u16`
range. See `PHASE2_STATE.md`, "Known limits, stated rather than fixed", for
the same list alongside the structural format limits.

The reusable [test-group map](PHASE2_TEST_GROUPS.md) distinguishes lean,
full-workspace, preserved compatibility, interruption and scale evidence.
No single group proves all eight laws. Broad SQL, network services, wrappers,
EXPORT/IMPORT and public packaging remain subsequent product work, as already
specified by the Phase 2 workload contract.

## Final format audit follow-up

A source/document audit found no key-tag collision or implemented codec
mismatch, but the central registry and spatial/text documents were stale.
They now describe all six feature bits, five index families, and exact
descriptor/value encodings. Graph properties normatively bind to the Phase 1
binary-JSON grammar. These are documentation fixes, not format changes.

The earlier Ready fixtures omitted graph-independent masks 1/5/9/17/33 and
Building/Dropping/post-drop catalogs. The new ARM corpus now covers these
states, including nonzero build cursors, intermediate maintenance, allocator
state, complete descriptors and retained feature bits after drop. It passes
80 lifecycle sources and 320 full cycles; graph and other multimodel fixtures
bring the complete matrix to 104 sources and 416 cycles. The failed server
oracle expected physical slot 2 instead of 3 and remains preserved as a failed
attempt; the engine did not require a fix.

The earlier preserved-input replay used independent copies for the two writer
handoffs. The new test commits a distinct third state on the same copy and
verifies current snapshot/writer reopen, closing that test-behavior gap. See
[rollback evidence](PHASE2_ROLLBACK_RESULTS.md). These same-revision cross-build
cycles prepare the first preserved baseline, not historical cross-release proof.

The actual cycle-capable corpora, binaries, source and reports are now archived
on scratch, with all 2,029 members hash-verified after download. Restoration to a
different Linux directory also passes all 416 cycles without regenerating
fixtures; all restored inputs remain unchanged. Final engine acceptance remains
pending the query keep/revert decision and final-source qualification; the
archive itself does not declare a stable Phase 2 release.

## Exact phrase requirement

tracker's phrase and token-position cases are implemented via `TextMatch::Phrase`:
existing all-term postings give candidates, then contiguous ordered analyzer-v1
tokens are verified against the authoritative primary text on the same snapshot,
before ranking/top-k, metered by a dedicated TextTokens budget with cancellation
checks. No persisted positions or encoding changes were needed.

Tests: `tests/index_text.rs` and `tests/query_multimodel.rs`, including
`phrase_is_ordered_contiguous_bounded_and_snapshot_authoritative`,
`phrase_query_text_hits_max_examined_mid_scan`,
`phrase_refines_all_term_candidates_before_rank_and_across_drivers`,
`phrase_graph_and_spatial_intersect_before_top_k`, and
`phrase_pages_are_disjoint_complete_stable_and_snapshot_isolated`. Mac result
2026-09-17: 20/20 passing in both the default build and the
`compact-cells,sqlite-balance,keyspace-append,slotref-split` build. Native Linux
qualification is part of the final Phase 2 job and has not yet run. Observed
cost: phrase refinement meters every token of every all-term candidate, so it
needs a larger token budget than an equivalent all-term query.

## 2026-09-18 — Linux qualification GREEN on committed head; further items staged

### Linux final qualification: attempt 4, GREEN

The Linux final qualification job (`e4-phase2-final-20260917`) reached a
GREEN attempt 4 on committed head `aeeae13` (eight commits `d0cbc7b`..
`aeeae13`, landed the morning of 2026-09-18): stage 1 (default build, full
workspace) PASS, stage 2 (retained-feature build, full workspace) PASS,
stage 4 (lifecycle replay) PASS with 404 passing results including guard
checks, stage 5 (release binaries) PASS, exit 0. This resolves the "attempt
3 result" line this document previously listed as remaining, and the phrase
native-Linux-qualification-pending line above — phrase's tests are part of
this same full-workspace run and are included in the passing stage 1/2
counts.

This is the first Linux qualification pass to reach stage 5 (release
binaries). It qualifies the committed head only; the query and build items
below are staged on top of it and are not part of this qualified result.

### Staged, uncommitted query and build items (owner to commit)

On top of `aeeae13`, the owner is keeping eight further items from the same
loop: Q2 (descending scalar cursor), H4 (graph filter on the shared BFS),
B1 (bounded index build, closes the 1,000,000-row atomic-build refusal),
Q3 (id-ordered and existence-only query paths), Q4 (batched row reads), G2
(graph write existence window), T2 (BM25 winner-probe skip), and Q5 (range
posting as existence proof). Full per-item ratios are recorded once in
`PHASE2_STATE.md` under "Staged, uncommitted items on top of `aeeae13`" and
are not repeated here. Item K1 (a kernel per-collection append hint) is
pending a byte-identity proof and is not claimed as done.

Net effect on the lean two-arm bench (`src/bin/two_ways.rs`, 20,000 rows,
Mac, matched durability): after T2 and before Q5, 29 cases were E4-slower
and 19 E4-faster with 0 disagreements, up from the first run of this bench
at 42 slower / 6 faster. Graph reads are faster than SQLite on every
measured case at that point, 2-hop 0.17x through 1-hop with projection
1.06x, 1-hop 0.41x. Remaining cases above the 2x acceptance gate and their
named cause are listed in `PHASE2_STATE.md`; none of them is a graph case.

### 1,000,000-row matched pair with B1, Mac

Ratios are E4 ÷ SQLite wall time (Mac, matched pair, B1 staged): load
entities 0.99, load relationships 1.27, build scalar index 1.47, build
spatial index 0.36, build text index 1.82, updates (three rounds) 0.93,
deletes (three rounds) 0.91, reinsert + edges (three rounds) 1.38, final
file size 1.15, query scalar 1.40, query spatial 1.52, query text 0.99,
query vector 0.90. This is a same-machine pair, not the qualified Linux 1M
matrix; the Linux 1,000,000-row run with B1 has not happened yet, since the
qualified Linux job runs against committed source only and B1 is still
staged. It supersedes, for the Mac side only, the earlier "1M has not run
yet" line under "Remaining before acceptance" above.

### Row updated in the requirement table

For "Fair total cost": the representative-1M line above is now measured on
the Mac with B1; the qualified Linux 1M run with these staged items remains
outstanding, alongside the tradeoff assessment already listed.

### Decisions taken this stretch (measured and rejected, not open)

- Per-row field offset table: would change the on-disk row format for a
  15-21% share of per-row cost, against +3.6% disk size, with no
  `two_ways` case crossing the 2x gate as a result. Not taken.
- Per-tree edge tags for 1-hop graph reads: shown to leave B-tree descent
  depth unchanged at both 50K and 1M rows by fan-out arithmetic
  (`.insert-loop/loop3/DECISION-graph-1hop-parity.md`); 1-hop stays at
  rough SQLite parity by design, not oversight.

### Owner decisions still pending

Unchanged from the existing "Known limits" list: (a) whether to skip
validation of skipped fields on a committed-snapshot predicate read, (b)
whether the projected-field-name `String` clone in the public API can
change, (c) whether `OFFSET` is added to `QueryRequest`.

### Remaining before acceptance, updated 2026-09-18

- The owner's commit of the eight staged items above (Q2, H4, B1, Q3, Q4,
  G2, T2, Q5) on top of the now-qualified `aeeae13`.
- Item K1's byte-identity proof.
- A qualified Linux 1,000,000-row run of the converged CRUD/build table
  once the staged items are committed.
- The three owner decisions above.
- The known limits already listed (snapshot lifetime bound by the WAL
  allowance, text head-to-segment fold not implemented, packed text
  entries tombstoned until rebuild, quantized-vector recall synthetic
  only) remain unchanged by this stretch of work.
