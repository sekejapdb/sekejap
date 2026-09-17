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
