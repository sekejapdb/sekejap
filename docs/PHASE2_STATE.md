# Phase 2 live handoff — 2026-09-17

The full Phase 2 objective remains ACTIVE: multimodel indexing, usable embedded
queries, combined execution, compatibility/recovery and measured acceptance.
Nothing in Phase 2 is committed, released or frozen yet. Phase 1 stays complete;
its e4-format-v1 baseline59d1cbc is immutable. Read PHASE2_WORKLOAD.md for the
full acceptance matrix. Do not restart generic insertion optimization.

## Current evidence

- Graph/scalar full Linux suites PASS: default451/0/2, compact456/0/2,
  retained456/0/2 (passed/failed/ignored). Exhaustive graph write faults pass.
- Preserved Phase1 actual old/current ordinary-feature compatibility10 arms
  PASS; scalar opt-in old typed snapshot/writer refusal6 arms PASS with source
  bytes/inventory unchanged. Raw KV remains outside typed invariants.
- Graph/scalar candidate fixtures:4 sources,8 cross-build read/write/readback
  arms PASS, plus same-source/hardlink negative guards. Report at
  docs/phase2-evidence/candidate-fixtures-r1/REPORT.json. Not released fixtures.
- Scalar r6: all14 processes pass independent oracles. Full tables and raw
  evidence in PHASE2_SCALAR_R6_RESULTS.md and phase2-evidence/r6/.
  Retained100K final disk9.738MiB E4 /7.598MiB SQLite (1.28x); three-round CRUD
  medians30.680s /12.958s (2.37x). Late atomic index0.671s /0.074s (9.03x).
  This is scalar-only evidence, not multimodel acceptance or a true peak cap.
- Exact vector baseline Linux PASS default and retained:25 total checks each,
  including5 independent vector behavior tests plus graph/scalar regressions.
  Expanded vector/spatial default qualification passes45 checks, including
  admission, exhaustive write faults, corrected spatial behavior and math.
  The retained configuration also passes45/0/0. Evidence
  collected in docs/phase2-evidence/vector-spatial-r1/.
- Spatial baseline compiled after refreshing copied source mtimes;2/3 behavior
  tests pass. Remaining failure is a test expectation omitting the row inserted
  during Building from a later world-bbox snapshot. Source sequence confirms
  that row is still present; agent corrected expected [old,during_build].
  Correction and new spatial/vector fault/admission tests pass in default
  and retained modes (family-fault-r1.exit=0). Report PHASE2_VECTOR_SPATIAL_RESULTS.md.
- Fulltext text-r2 Linux PASS60/0/0 selected checks in each default/retained
  mode, including prior-family regression, analyzer/dense reader, text admission,
  exhaustive write faults and DF validation. Earlier text-r1 budget expectation
  failure preserved. Evidence docs/phase2-evidence/text-reader-r1/.
- Current reader Linux PASS7/0/0 per default/retained: current committed-root
  membership, WAL/deletions, multilevel/overflow/cycles, budgets and source
  preservation. Same evidence directory. Not yet full indexed recovery.
- Scalar/JSON query-r1 captured after exact typing/numeric/budget review fixes.
  Linux PASS default and retained; final query-r1.exit=0, source R/query-r1-src.
  Evidence docs/phase2-evidence/query-r1/.
  Tests include complete pagination beyond65536 matches and family regressions.
- Multimodel candidate fixtures Linux PASS:16 preserved sources,32 current
  default/retained semantic handoffs,96 earlier-engine typed admission checks,
  48 source/alias guards. Source files unchanged. Older Phase1 engine matches
  all59 pinned tracked source/Cargo files; old trees only add the probe harness.
  Report PHASE2_MULTIMODEL_COMPAT_RESULTS.md explains admission versus semantic
  compatibility limits. Evidence phase2-evidence/multimodel-fixtures-r1/.
- Multi-family crash-r1 Linux PASS36cases per default/retained:6 transaction
  kills,24 interrupted lifecycle cases,6 deterministic checkpoint stages.
  Exact endpoints and held snapshots verified; sources unchanged. Commit-window
  kills are timing samples, not deterministic internal failpoints. Report
  PHASE2_CRASH_RESULTS.md, raw phase2-evidence/crash-r1/.
- Verifier-r1 Linux PASS17/0 per default/retained (10integrity+7reader checks).
  Mixed-query-r2 PASS27/0 per default/retained and runnableexample inboth. R1
  failure was missing explicit enable_graph in fixture/example; test-only fix.
  Evidence verifier-mixed-r1/, report PHASE2_MIXED_VERIFIER_RESULTS.md.
- Native text/spatial query-drivers-r1 PASS28 checks per default/retained build
  plus example; complete65,537-match pagination. Evidence query-drivers-r1/,
  report PHASE2_QUERY_DRIVER_RESULTS.md.
- Multimodel-smoke-r2 completed exit0 for retained E4 and SQLite:10K people,
 32dim,3 full-population updates and10% delete/reinsert per round. Correctness
 passed, but SQLite edge deletion missed leading context in its index predicate;
 delete timing/total CRUD comparison is NOT accepted. Agent correcting query
 plan before repeated trials. Preserve original evidence, do not quote speedup.
- Explicit quantized-vector family and source-preserving derived rebuild are
 captured together as quant-rebuild-r1; session78018 currently Linuxqualifying.
 Per-create codec helper avoids process-global default races. Native results
 pending; pure quantization tests pass. Approximate recall remains unmeasured.

## Current ownership (2026-09-17)

Claude orchestrates the remaining work with delegated workers; there are no Sol
agents. tracker owns live task status. The final qualification plan: source
archive sha256 ffc29fc677ad478eb3d19b712d129866ae1845275ef1fa129a66c97b8f3db74b
(documentation edits after this hash do not change tested code); Linux jobs
prepared under `.insert-loop/p2final/` (export of the 1M matrix evidence, then
full-workspace qualification in both build modes plus preserved-corpus replay),
submitted after job e4-phase2-20260916 finishes.

Passed default and retained in family-fault-r1: tests/vector_admission.rs, src/vector_fault_tests.rs,
 tests/spatial_admission.rs, src/spatial_fault_tests.rs and cfg(test) attachments
in the two family modules. They cover source-preserving future-format refusal,
malformed postings/sidecars and exhaustive create/CRUD/build/drop write faults.
Process-kill and actual old/new executable compatibility remain separate work.

## Native source and processes

Kubeconfig <home>/, namespace
sekejap-benchmark, Job e4-phase2-20260916, pod e4-phase2-20260916-c9rp8.
Run root R=<scratch> Isolated2CPU/2Gi on server.
All DB tests/benchmarks Linux; Mac editing/compile/pure math only.

CARGO_HOME=<scratch>; CARGO_TARGET_DIR=$R/target; CARGO_BUILD_JOBS=2;
PATH includes /usr/local/cargo/bin. cargo --release --locked --offline.
TMPDIR and SQLITE_TMPDIR must use the existing guarded prefix:
<scratch>
Pinned geographiclib-rs0.2.7/libm0.2.16 are cached; no existing dependency updated.

Completed markers: graph-r2.exit=0, bench-r6.exit=0,
candidate-fixtures-r1.exit=0, vector-r1.exit=0.
Spatial-r1.exit=101 is a compile failure from stale Cargo artifacts;
spatial-r2.exit=101 is the explained incorrect snapshot expectation above.
Their logs remain preserved. Existing session58487 is terminal. New isolated
family-fault-r1 finished with exit0, session32852, source R/family-fault-r1-src, logs
family-fault-r1-{default,retained}.log and final family-fault-r1.exit. It includes
the corrected expectation, vector/spatial admission and exhaustive write faults,
family regressions and spatial math; source mtimes refreshed before compilation.

R/src is preserved graph/scalar source, with archive
R/graph-scalar-qualified-source.tar.gz and candidate-fixture-source.json.
R/vector-r1-src is immutable vector baseline. R/spatial-r1-src is the spatial
baseline, shared by spatial-r1/r2. Separate vector/spatial source inventories
are in R and local docs/phase2-evidence/. Never mutate active source trees.
Use fresh isolated source directories for later overlays.

IMPORTANT: copying a source tree or extracting a tar preserves mtimes. Cargo
may reuse older candidate artifacts in a shared target when those mtimes are
older than cached builds. Refresh .rs and Cargo input mtimes after overlays,
without changing bytes; preserve source hashes. This fixed missing spatial
APIs in spatial-r1. Do not overlap timed benchmarks with compilation/tests.

Poll actual process state and final markers; output timeout is not termination.
Job PID1 waits for R/complete with12hour deadline. Finish it only after all
necessary runs and evidence collection. Preserve production/E3 services.

## Format and limits

Ordinary files retain COLL1. Explicit feature creation enables COLL2; existing
physical page/WAL/entity bytes retain their meanings. Logical feature bits:
scalar/catalog1, graph2, vector4, spatial8, proposed text16. Namespaces03/04/05
are catalog,06/07/12 graph metadata,70 scalar,71 authoritative graph edges,
72 reverse,73 vector locators,74 spatial. Text proposal75 postings,76 norms,
77 df,78 corpus counts. Existing primary vectors remain tag60 f32 sidecars.

Index build/drop batches1..256; at most64 indexes/collection. Unique constraints
complete only when Ready. Scalar Eq(null) is a null/missing candidate bucket;
combined queries must distinguish them. Existing result-vector cap65536 is
not complete candidate streaming; implement cursors rather than truncate.
Vector exact cosine excludes stored zero vectors/refuses zero queries; L2/dot
are explicit. Spatial uses WGS84/Karney and conservative Hilbert candidates;
nearest still scans its population. Graph deletion refuses more than256
incident edges; BFS has explicit depth/visited/edge budgets. These are named
limits, not performance acceptance.

Text analyzer1 freezes Unicode17.0.0 generated by recorded rustc1.96.0. Do not
regenerate it on compiler update. Pure evidence text-analyzer-v1-source.json
and logs/text-analyzer-v1.log. Empty text counts as a corpus document;
null/missing do not. Fulltext build processes one term map at a time.

## Required next steps

1. text-r2,current-reader-r1,query-r1,multimodel-fixtures-r1,crash-r1,
   verifier-r1,mixed-query-r2 finished exit0. Mixed-query-r1 exit101 retained
   fixturefailure. Query-drivers-r1 completed exit0, 28 checks/build plus example, isolated source
   R/query-drivers-r1-src, final query-drivers-r1.exit. It adds native text/spatial
   drivers to passed mixed-query-r2,selectedregressions+example default/retained.
   Benchmark draft not yet uploaded/run; await reviewcorrections fromowner.
   Never restart a process solely on observation timeout.
2. Complete fulltext persistence, lifecycle, statistics and ranked search tests.
3. Implement streaming combined embedded queries and the app-like example.
4. Process-kill/recovery and explicit verified derived-index rebuild; never
   fabricate missing authoritative edge data or primary vectors.
5. Preserve per-family fixtures and actual baseline/current compatibility.
6. Fair10K/100K/representative1M mixed-model measurements, repeated at least3
   times: load/build/query/repeated CRUD/reopen/memory/final+peak logical and
   allocated disk, including no-reader/short/held-reader cases.
7. Audit every PHASE2_WORKLOAD acceptance row, record limitations and commit
   accepted files without coauthor; only then complete the full goal.

Repo branch pagewal-foundation, HEAD a69838c. Preserve historical untracked
research; stage named work only. Original inventory:
/tmp/e4-phase1-main-preservation/untracked-sha256.json. Use targeted rustfmt
with skip_children=true, never broad cargo fmt. Unrelated recursive formatting
was independently checked against formatted HEAD and restored this turn.
tracker journey sekejap-e4 owns live status, curl GET/PATCH/readback, notes <=8000
characters. Journey sekejap is E3 and remains untouched. Broad SQL/network,
wrappers, EXPORT/IMPORT and packaging remain later product deliverables.

New generic src/bin/typed_admission_probe.rs compiles locally. It supports
accept/refuse snapshot/writer COPIED_DB, exit42 only for Unsupported; must be
compiled into preserved old source copies (no engine changes), with recorded
source/binary provenance, before being used as cross-version evidence.

2026-09-17 formatting cleanup: an agent's cargo fmt invocation changed70
unrelated tracked Rust files. Parent proved each current file exactly equaled
rustfmt(HEAD), then restored original bytes; historical untracked format_audit.rs
likewise restored against pinned hash and formatting-only proof. Evidence
/var/folders/78/0h4ht2nn02b18cj8l6yjcrjrx020k3/T/e4-format-only-restore-qh_afnxt.
Only6 intended tracked files remain modified; untracked Phase2 files separate.
All agents reminded to use direct rustfmt with skip_children=true, never cargo fmt.

2026-09-17 continuation: query-drivers-r1 exit0 collected and preserved in
docs/phase2-evidence/query-drivers-r1; PHASE2_QUERY_DRIVER_RESULTS.md records
28 selected checks/default+retained and complete 65,537-match pagination.
Reviewed benchmark corrections captured separately against this passed engine.
multimodel-smoke-r2 completed exit0; source R/multimodel-smoke-r2-src,
final R/multimodel-smoke-r2.exit, logs R/logs/multimodel-smoke-r2-*.log.
10K people,32-dimensional vectors,all families,3 full-population update rounds
plus10% delete/reinsert per round; retained E4 and SQLite sequential, no reader.
This is preliminary harness qualification, not repeated acceptance timing.

Parent added private PageWalStore::create_with_compact_cells to preserve a
rebuilt database's codec without changing the process-wide creation default.
Two new pagewal unit tests cover concurrent default/explicit creates and
existing-directory refusal/source-byte preservation. Linux execution runs in quant-rebuild-r1, session78018.
Graph agent extends compatibility fixtures; vector agent owns combined query integration. Shared workspace remains candidate;
use isolated captured sources for qualification.

Current Linux candidate: R/quant-rebuild-r1-src, final R/quant-rebuild-r1.exit,
logs R/logs/quant-rebuild-r1-{default,retained,codec-*,math-*}.log. Overlay hashes
/tmp/e4-quant-rebuild-r1-overlay.json. Selected integration/codec/math tests;
quantized fault_tests module needs separate execution or full-suite run.
Do not overlap benchmarks with this compilation/tests. R2 smoke binary remains
R/multimodel-smoke-r2/bench, logs R/logs/multimodel-smoke-r2-*.log.

2026-09-17 latest queue/status (supersedes preceding active-run descriptions):
- quant-rebuild-r1.exit101: both rebuild success cases WriterLocked; duplicate
  protected opening of source data while CurrentSourceReader owns it. Fix now
  uses validated reader.compact_cells(), no second source opener.
- quant-family-r1.exit0:100 checks/default and retained (27integration+73lib),
  including quantized exhaustive write faults and codec concurrency. Evidence
  docs/phase2-evidence/quant-family-r1/, PHASE2_QUANTIZED_RESULTS.md.
- multimodel-smoke-r3.exit0: indexed SQLite deletes corrected. Actual SQLite
 3.46 plan proves active_age outer followed by repeated FTS,364ms text+active;
  this and tiny-graph timing remain unsuitable comparisons. R4 uses explicit
  candidate-first joins and EXPLAIN gates. No performance acceptance yet.
- rebuild-r2 session88481 active; default14integration+7reader passed, codec
  tests then retained qualification follow. Final R/rebuild-r2.exit.
- approx-query-r1 session4692 QUEUED after rebuild-r2. Isolated source
  R/approx-query-r1-src adds combined approximate order/pagination/diagnostics,
  exact+approx snapshot regression, strict preflight logical rebuild budget
  (control markers+coordination+pager creation included),3codec tests,example.
- multimodel-r4 session38381 QUEUED after approx-query-r1; requires exit0.
  Source R/multimodel-r4-src; binaries R/multimodel-r4-binaries/{bench,quant}.
  Steps: corrected10K smoke; quant recall32dim/10K and1536dim/2K; then27
  alternating10K trial arms (atomic E4/resumable E4/SQLite ×3reader modes×3).
  Final R/multimodel-r4.exit; intermediate smoke/quant markers only on success.
  Logs R/logs/multimodel-r4-*, quant-bench-r1-*. No overlaps by explicit waits.
- Quantized compatibility fixture overlay ready /tmp/e4-quant-fixture-r1-overlay.tar.gz,
  NOT queued yet. Capture20fixtures/40current semantic handoffs, preserved
  five-family query-drivers engine as oldmask31 typed-admission baseline.
- Graph agent now extends crash probe/driver to quantized lifecycle + transactions.
  Vector agent read-only final query correctness review; scalar agent finished
  benchmark files and reports (idle). Parent owns all native scheduling.
- New budget headroom helper and strict marker-reserve rebuild changes are
  unqualified until approx-query-r1; r2 isolates WriterLocked correction only.

Rebuild-r2 is now terminal exit0:23 checks/default and retained
(4rebuild+10verifier+7reader+2codec). Captured evidence in
docs/phase2-evidence/rebuild-r2/, report PHASE2_REBUILD_RESULTS.md.
The source-lock failure is fixed. Stricter total logical preflight remains
under approx-query-r1 qualification, not retroactively part of r2.

Queue additions after multimodel-r4: quant-fixture-r1 session24100 waits for
R/multimodel-r4.exit, then builds current default/retained fixture v2 + old
five-family query-drivers probe; report R/quantized-multimodel-fixtures-r1/REPORT.json,
final R/quant-fixture-r1.exit. Old-engine original/source hash inventories and
engine-preservation proof written before compile; byte-only harness addition.
Then quant-crash-r2 session44293 waits for fixture marker, runs84case matrix
(42/build), final R/quant-crash-r2.exit, report R/quant-crash-r2/REPORT.json.
Both require approx-query-r1 exit0 and use its immutable engine source.

Read-only query review found orphan derived entries can emit nonexistent IDs
when Projection::Ids skips primary fetch. Vector agent now implements metered
winner-only existence checks; graph seed read accounting; exact-score chunked
cancellation; discriminating filtered approximate ef2 oracle; exact/BM25 tied
pagination and rollback combined answers. These later edits are NOT in
approx-query-r1 or queued benchmark/fixture/crash source. Capture/qualify them
separately; ordinary query cannot prove missing-index completeness and full
corruption diagnosis still uses verifier. No new release claims.

Compact continuation handoff: /tmp/sekejap-e4-phase2-resume-20260917.md.

Query review fixes are now capture-ready and queued as query-guards-r2,
session23095, after quant-crash-r2 terminal; requires approx-query-r1 exit0.
Source R/query-guards-r2-src (based on approx-query-r1 plus four-file overlay),
final R/query-guards-r2.exit. New src/vector_indexes.rs cancellation helper
keeps standalone behavior; query.rs adds metered graph seed and winner primary
reads. Tests add phantom winners/all six drivers, ef2 discrimination, tied
exact/BM25 pagination, cancellation/budget retries, combined rollback. Docs
explicitly distinguish ordinary query guards from full integrity verification.
All agents currently done with assigned implementation; parent monitors queue.
No stale valid predicate rescoring or full per-candidate verification added.
Native timings before query-guards-r2 must keep their candidate identity;
final scale acceptance should use the subsequently qualified guarded source.

LATEST: approx-query-r1 exit0,101 checks per default/retained build plus
example. Evidence docs/phase2-evidence/approx-query-r1. Multimodel-r4 now
actively compiling/running; subsequent fixture/crash/query-guard jobs wait.

LATEST MEASUREMENTS: quant-bench-r1 exit0,144 pairedrecords/corpus for10Kx32
and2Kx1536. ef40 worstrecall@10=1.0 across12synthetic query/metric cases;
per-case median exact/approx speedups2.89–3.66x(32dim),2.27–2.84x(1536dim),
extra checkpointed logicaldisk26.2%/24.8%. ef10 worstrecall=.9; ef640 canbe
slowerthanexact. No SQLite ANN ratio or production recall guarantee. Report
docs/PHASE2_QUANTIZED_BENCH_RESULTS.md, raw quant-bench-r1/ evidence.

R4 smoke passed, text+active corrected SQLite4.30ms vs old364ms. R4 recursive
graph remains unfair: recursive step uses broad edge_in before SCAN r despite
correct anchor edge_out and final join order. Do not accept R4 graph timing.
R5 harness fixes recursive frontier-first CROSS JOIN edges INDEXED BY edge_out
and gates the recursive segment's source_id lookup. Compile clean, captured
/tmp/e4-multimodel-r5-overlay.tar.gz and.json, NOT yet uploaded/queued. Use it
with qualified query-guards-r2 for final10K/100K/representative1M matrix.
R4 repeated matrix remains running as candidate diagnostic evidence; its
non-graph operations are still useful, source identity/limits must be explicit.
All implementation agents now idle; parent monitors existing native handles.


2026-09-17 authoritative continuation: quant-fixture-r1 EXIT0 (20sources,40semanticarms,40older-five-familyadmissionarms=24accept+16byte-preservingrefusal,60aliasguards); quant-crash-r2 EXIT0 (42cases/build,84total,allsourceunchanged). Report docs/PHASE2_QUANTIZED_COMPAT_CRASH_RESULTS.md and local raw phase2-evidence/quant-compat-crash-r1/. query-guards-r2 native process is live, default passed (19 integration+74lib), retained compiling.
R4 report completed docs/PHASE2_MULTIMODEL_R4_RESULTS.md: all12E4short/held readerarms refused;6E4no-reader+9SQLite completed. short means entireCRUDround, not onebatch. Source review: fixed16MiBWAL_CAP and no checkpoint while a reader is held. Generic managed-byte error covers either WAL_CAP or totalcap; benchmark default totalcap is unlimited. Do not raise cap or treat refusal as completed workload. Scalar agent implements typedrefusal committed-progress ledger+independentreadonlyreopenoracle; newbatchmode only if isolated.
Graph agent adds reusable lean/full runner+8law/workload mapping; parent requested SQLITE_TMPDIR, tee-failure and zero-filtered-test guards. These are newfiles tools/phase2_qualify.sh and docs/PHASE2_TEST_GROUPS.md. Fullnative run after queryguards will qualify currentengine while benchmarkinstrumentationcontinues. R5recursiveSQLiteplanfix captured, oldR4graphnumbersexcluded. No Phase2commit/release yet.


2026-09-17 query-guards-r2 EXIT0:93checks perbuild(default+retained)=19integration+74lib, plus runnableexample. docs/PHASE2_QUERY_GUARD_RESULTS.md; raw phase2-evidence/query-guards-r2. Inventory comparison: every currentengine file matches; tests/index_vector.rs differs only by rustfmt (formatted capturedsource compared byte-identical).
Started phase2-workspace-r1 native session17722, queued after queryguardterminal then captured queryguardengine+quantfixture/crashharnesses+reviewedlean/fullrunner. SourceR/phase2-workspace-r1-src; markerR/phase2-workspace-r1.exit, separate lean/full.exit and summaries under phase2-workspace-r1-{lean,full}. Current process live compiling lean. Must never overlap timedbenchmark. Native source does not include ongoing benchledger changes. Estuaryphase2/compat/recovery/hybridstorage updated+readbackverified with latestresults/R4refusals.


2026-09-17 lean workspace-r1 PASS92checks/build,184total,18groups. Fullworkspace remains running session17722. Reviewed R6 benchmark refusal ledger+oracle captured andqueued session55926 afterworkspace; 4smokes then explicitreview beforematrix. SQLiteR4 baseline duplicatedforwardPK/edge_out; R6 removesduplication usingWITHOUTROWID PK+reverse, newrecursiveplan guard. Agent now adds explicitone-update-batch readermode, preservingoldshort/held. PHASE2_WORKSPACE_RESULTS.md and PHASE2_TEXT_COST_REVIEW.md are new evidence/design records; no runtime optimization or acceptancecommit.


2026-09-17 fullworkspace-r1 EXIT0:default547/0/2,retained552/0/2; lean184totalchecks. Exactevidence localworkspace-r1/. R6session55926 activelycompiling afterworkspacepass; R7session45056 queues batchsmoke+three-trial10K/100K4readermodes afterR6pass. Finalformataudit identifiedstaledocs+missinglifecycle/graphfreefeaturefixtures, notruntimecodec mismatch; graphdocs andseparatevectorfixturehelper active. Offlinev2 reporttoolreviewed includingmissingraw/returncoderefusal; docs PHASE2_MULTIMODEL_PROTOCOL.md recordsfairnessandreadersemantics.


## 2026-09-17 R7 10K evidence and serialized continuation

The 10K R7 matrix is complete: 36 runs, 24 complete workloads, 12 verified
resource refusals, zero process failures. Current report:
`docs/PHASE2_MULTIMODEL_R7_RESULTS.md`; raw report, per-arm logs, smoke logs,
source inventory, binary hash and full tables in
`docs/phase2-evidence/multimodel-r7-10k/`.
No-reader three-round CRUD medians: E4 atomic 13.916 s, resumable 13.833 s,
SQLite 6.526 s. After-close logical sizes 8.664 / 8.664 / 6.723 MiB. R7 replaces
R4 SQLite schema/graph-plan comparisons. Long-held E4 snapshots refuse after
5,888 confirmed updates; all 12 committed-state oracles pass. Physical peak
ceiling is not proven. See protocol for endpoint-validation differences.
Report tool now distinguishes in-process/after-close size, totals all three
CRUD rounds, and reports peak/loaded factors. SHA256:
703a877ea963e77694061bfd7fe67a3525941bb71b201a947f8c26876dfed259.

All native paths below use R=<scratch>,
pod e4-phase2-20260916-c9rp8 in sekejap-benchmark. Existing kubeconfig and
resource limits remain unchanged; job lifetime extended from 12 to 24 hours
(activeDeadlineSeconds=86400) for the full serialized large matrix.

- R7 original session 45056 is LIVE: 100K matrix, last observed 8/36 captured
  and no refusals/failures so far. 10K complete marker is 0. Final marker
  R/multimodel-r7.exit must become terminal before compilation begins.
- Lifecycle session 25473 is QUEUED after R7 terminal. Script
  /tmp/e4-lifecycle-r1-linux.sh (also R/lifecycle-r1-linux.sh), final marker
  R/lifecycle-r1.exit. Copies immutable R7 source, adds only new helper/driver,
  verifies source/overlay hashes, builds both codecs, runs 20 frozen-Phase1
  semantic arms and 80-source/160-semantic/160-admission lifecycle matrix.
  Expected reports R/lifecycle-r1-phase1-{default,retained}/REPORT.json and
  R/lifecycle-r1/REPORT.json. Native lifecycle has NOT passed yet.
- Large matrix session 32855 is QUEUED after lifecycle terminal and R7 pass.
  It uses the same R7 binary, 2K people at1536 dimensions first, then 1M at32,
  all four reader modes, three alternating trials and both E4 build policies.
  Script /tmp/e4-multimodel-r7-large-linux.sh; final marker
  R/multimodel-r7-large.exit; per-size markers multimodel-r7-{1536,1000000}.exit;
  reports R/multimodel-r7-{1536,1000000}/report.json. Refusals remain explicit.
  Never restart a job because a poll times out; inspect exact process/marker.

Lifecycle review fixes are captured: persistent next_index/live count, full
catalog identity, exact BUILDING bytes after CRUD before resuming, unchanged
DROPPING derived bytes after CRUD, independent WAL-boundary checks and Python
optimized-mode rejection. Final captured SHA256:
helper 2d6c3cc8887594fe9d06c37479b1a32421667525ab30fad19709d748f45ba8b0;
driver ee57a5f8aad72c8ded73114a7bb7a8de3cc54cc69b61d084c2e458923be627c8.
Original unexecuted package preserved locally with .pre-review suffix; remote
staged package updated before its first run. No engine runtime changed.

tracker phase2/hybrid-storage/index-compatibility updated and readback checked
using /tmp/e4-tracker-r7-update.py. Acceptance matrix and test-group documentation
updated with actual full workspace/R7 results and explicit remaining work.
Graph agent is doing a read-only task-status evidence audit; vector fixture and
scalar report-tool agents finished. Phase 2 remains active, not accepted,
committed, released or frozen. Next: inspect native markers/results, preserve
large evidence, assess measured tradeoffs, close compatibility fixtures, then
complete acceptance audit and named commit without co-author.


Latest tracking audit: /tmp/e4-tracker-phase2-status-audit.py marks 13 qualified
contract/catalog/atomic/scalar/graph/spatial/query/recovery tasks or rollups Done,
with evidence and readback. Vector, multimodel rollup, compatibility, hybrid
measurements, validation and Phase2 remain Ongoing. Acceptance is Paused while
its prerequisites execute. Fulltext is Paused despite passing term/BM25 tests:
its tracker success criterion explicitly mentions token-position coverage, which
is not implemented. Parent sent an asynchronous scope question to the user:
first Phase2 term+BM25 with phrase later (recommended), or exact phrase now.
Do not silently claim phrase/positional support. Other qualification continues
independently; this is not a global blocker. The request has no answer yet.
All current agents finished; no active implementation work remains at this
checkpoint. Queued lifecycle helper includes all reviewed fixes and its overlay
hash matches the current files. Linux execution still waits for R7 100K.

## Resumed native qualification and preserved-input packaging (2026-09-17)

Direct process inspection confirms R7 100K is live; latest report has 23/36
captures, including independently verified resource refusals in reader-held
arms. Lifecycle, large-scale and query replay wrappers are live and waiting on
predecessor markers. No timeout-triggered restart occurred.

The unaccepted query-only membership candidate is captured and queued as
query-membership-r1 (session 46104), after the larger R7 matrix and lifecycle
PASS. It tests both codec configurations and compares baseline E4, candidate
E4 and SQLite against preserved post-three-CRUD databases. Independent primary
oracles, bounded top-k heaps, three process trials, three seeds and five repeats
per process; all original bytes must remain unchanged. Compilation on Mac is
not native qualification. Keep/revert still requires the Linux results.

`tools/phase2_preserved_compat.py` provides the subsequent immutable-corpus
upgrade/readback gate without fixture generation. Its native execution remains
pending the accepted engine decision. Graph v1, multimodel v1/v2 and lifecycle
reports have explicit known schemas; current helpers must match each preserved
harness version. The current v2 helper cannot masquerade as a v1 reader.

Actual original corpora and binaries still reside on the server PVC. An archive
job is now queued behind query replay (including a failed replay terminal),
using /tmp/e4-preserve-phase2-r1.py and its Linux wrapper. It validates complete
reports and source inventories, reconstructs only hash-matching historical
source files, preserves original executable bytes, rereads every compressed
archive member and hashes original databases again. Four archive families
cover 4 graph, 16 older multimodel, 20 quantized multimodel and 80 lifecycle
sources. Target: R/preserved-phase2-r1/INDEX.json. These are candidate artifacts,
not released-version proof. Download and local hash verification remain pending.

The first archival wrapper invocation exited 127 before work because sandboxed
staging had failed. No archive/database mutation occurred. Staging was repeated
through authorized network access; the verified terminal attempt remains in
preserve-phase2-r1.exit/log. The replacement attempt uses separate marker
R/preserve-phase2-r2.exit and log R/logs/preserve-phase2-r2-run.log. No benchmark
or compatibility execution was restarted.

### Preserved candidate replay queued

The next compatibility job is now live as a waiting wrapper, session44979,
script /tmp/e4-preserved-replay-r1-linux.sh (also R/preserved-replay-r1-linux.sh),
terminal marker R/preserved-replay-r1.exit. It waits for archive attempt r2
and query replay PASS, then copies the exact query candidate source. Its only
extra Rust file is the byte-identical archived multimodel-v1 fixture helper,
renamed as a separate binary and linked against the current candidate engine.
Source-inventory checks refuse any other change; native builds use recorded
source-derived revision strings and both default/retained configurations.

Planned qualification: four preserved corpus generations (4/16/20/80 sources),
two current build modes, 240 original read-only verifications and 960 independent
fresh-copy writer/reader handoffs. Earlier graph/multimodel binaries are actual
preserved earlier candidate engines, not rebuilt stand-ins. The lifecycle
baseline differs only in the subsequent query experiment, so do not present
that family as broad historical coverage. No fixture generation is called.
Expected reports R/preserved-replay-r1-{family}-{mode}/REPORT.json and aggregate
R/preserved-replay-r1-summary.json; binaries R/preserved-replay-r1-binaries/;
source inventory R/preserved-replay-r1-source.json. Counts are planned, not
passes. Candidate performance acceptance and released-format declaration remain
separate decisions. A rejected query candidate must not be presented as the
final accepted engine merely because this compatibility job passed.

## Complete 100K capture; lifecycle oracle fix; real rollback-cycle gap

R7 100K is complete and locally preserved at
`docs/phase2-evidence/multimodel-r7-100k/` (77 raw files plus tables/download
hashes):36runs,24completeworkloads,12verifiedresource refusals,0nonzeroexits.
No-reader three-round CRUD:139.557s E4atomic,141.140s resumable,108.666s SQLite;
final86.098/86.098/66.680MiB. All E4 long-reader cases refuse after4352 committed
updates inround1, independently verified. Largest allocatedpeak completed
none/batch221.941MiB E4 versus141.281SQLite; physicalcap remains unproven.
Full report and unfavorable query/build costs now in PHASE2_MULTIMODEL_R7_RESULTS.

Lifecycle-r1 ended1 after11sources/20semantic arms, beforeadmission. The new raw
BUILDING oracle expected vector locator slot2; actualslot3 is correct because
immutable layouts prepend internal __e4_key (public collection introspection
hides it). The same wrong ordinal appeared in the helper's raw sidecar key.
No engine defect identified. The20 actual frozenPhase1semantic arms already
passed in bothbuilds. Preserve13rawfailure/Phase1files in
`docs/phase2-evidence/lifecycle-r1/`. Never relabel its partial matrix PASS.

Minimal repair is independently captured from the original r1 helper, changing
ONLY locator/sidecar2->3 with explanatory comment; helperhash
4bf66b26122cba10b375a1cff61b0bda2d4ac8a7606090da0ab95ba147cf7aeb.
It excludes all ongoing third-state helper changes. A serialized replacement
chain is live as session64931: /tmp/e4-post-large-repair-r2-linux.sh, stagedRcopy,
markerR/post-large-repair-r2.exit. It waits for current largebenchmark and the
old dependency chain to terminate; old query/archive/pairwise wrappers must
refuse failed lifecycle-r1 before replacement. It then runs lifecycle-r2,
query-membership-r2, preserve-phase2-r3, preserved-replay-r2, each with a distinct
terminal marker/log. Original attempts/captured source remain untouched.
Query/archive/pairwise data directories retain their r1 names because the old
attempts refuse before creating them; explicit absence checks prevent overwrite.
Captured replacement package and JSON hashes:
/tmp/e4-post-large-repair-r2-package.{tar,json}. R2 performs no engine change;
previous frozenPhase1passes are reused honestly, not rerun to inflate counts.
Separate1536dim matrix is active; representative1M follows, no timing overlap.

Law8 audit by graph_slice confirmed a different acceptance gap: independent
fresh-copy handoffs cannot prove an old writer commits on current-written
bytes. Phase1's specified cycle requires that exact SAME-copy transition.
Both agents implemented cycle-capable helpers without engine edits:
- graph/scalar helper5e9b823ca66bb9f4f53d045d883951023dac97d7031ce61c516fdc7769b59016;
- multimodel helper7f508ccbe6ff7aaa35475740f6c869408dbc30763607749c85dd2cc2100e3501;
- lifecycle helper1e9a2ee87aee51b9d258580cce6032d35f11aec94b77e9a2d0fa79065573ffd3.
They add deterministic Roundtrip state, guarded continue-upgrade, snapshot
verification, guarded verify-writer and rollback_cycle_version1. Original and
Updated semantics/harness IDs remain. All three compile in default/retained;
no native cycle run yet. Archived earlier binaries cannot be retrofitted and
remain explicitly pairwise evidence.

Parent added tools/phase2_rollback_compat.py: samecopy comparisonwrite -> preserved
write -> comparisonread/writerreopen -> preservedread, allfour WALboundary pairs.
Graph agent reviewed it; provenance gaps fixed: one revision perbuildgroup,
exact pinned binaries for cross-build, distinct build hashes, mode profiles,
independently pinned build/source manifest for cross-revision. Two valid/seven
invalid pure provenance checks PASS; native helper/driver/corpus capture and
execution still required. Intended first-baseline preparation is104sources
(graph4+multimodel20+lifecycle80) x4 =416cycles, explicitly same-revision
cross-build rather than invented historical release evidence. This new cycle
work is NOT in any existing native source snapshot or queuedjob yet.

tracker hybrid/compatibility notes updated and verified. Compatibility notes
were consolidated to current evidence/action, with previous full notes saved
/tmp/e4-tracker-before-summary-p2-index-compatibility.json. Task remainsOngoing.
No Phase2 release/freeze/acceptedcommit. Term-vs-phrase owner question still open.

Latest native state:1536dim capture COMPLETE36/24workloads/12verifiedrefusals/0nonzero;
local77files+tables in phase2-evidence/multimodel-r7-1536/. No-reader CRUD medians
6.513/5.817/2.807s, final17.281/17.281/16.484MiB (E4atomic/resumable/SQLite).
Representative1M LIVE, last firstarmcaptured and SQLitearm001running. No 1M1536claim.

The new cycle source IS NOW CAPTURED AND QUEUED, superseding the previous
not-yet-captured note: session45616, /tmp/e4-rollback-r1-linux.sh, remoteRcopy,
markerR/rollback-r1.exit. Waits post-large-repair-r2PASS. Copies exact captured
query candidate engine, replaces only3fixturehelpers, verifies source/overlay
hashes, builds default/retained, generates104cycle-capable originals with existing
strict fixture drivers, and runs416same-copy rollbackcycles. Source inventory
R/rollback-r1-source.json, binariesR/rollback-r1-binaries, per-family fixture and
cycle reports R/rollback-r1-{graph,multimodel,lifecycle}-{fixtures,cycles}/REPORT.json,
aggregateR/rollback-r1-summary.json. Build manifests per-family pin binaries,
versions and source_files_sha256 for future cross-revision replay. Package
/tmp/e4-rollback-r1-package.tar, hashes/tmp/e4-rollback-r1-overlay.json. Driverhash
23f06d39d659e78a4f7b40609beedcdd4bfba45486eb4b23236e364ba53ce3fa.
These remain candidate cross-build preparation pending native passes and the
query keep/revert decision; not a format freeze. The resulting cycle-capable
corpora/binaries must be archived separately for future releases; the earlier
120-source archives cannot substitute for this new rollback-capable baseline.

## Independent ARM qualification started on the authorized Pi

The first1M E4atomic arm safely refuses atomic late-index creation after loading
all1Mpeople/100organizations/3Mrelationships; freshoracle confirms committedstate
and0publishedindexes. SQLitearm001 is live (last18minutes elapsed). Do not infer
resumable outcomes yet. server queued source and timers remain untouched.

Pi contributor@example.invalid is reachable under the trusted .43 hostkey alias, running
Linuxaarch64/Rust1.96.0. An independent correctness service is now active under
<scratch> (P below).
Unit e4-phase2-native-20260917-r1.service; invocation5c6fe7f1644d4aac875ea8d30089bebb;
initialMainPID273974. Its source is a472-file pinned local capture (including
currentquerycandidate+cyclehelpers), plus159exactfiles from actual earlier
five-family engine, copied from server's preserved quant-fixture-old-five-src.
Source hashes verified, Cargo.lock identical, dependencies vendored offline.
No Mac database runtime and no server benchmark interference.

Source/package artifacts local:/tmp/e4-phase2-pi-current-source.{tar.gz,json},
/tmp/e4-phase2-pi-old-five-source.tar.gz,/tmp/e4-phase2-pi-vendor-r1.tar.gz,
/tmp/e4-phase2-pi-package-r1.tar,/tmp/e4-phase2-pi-run-r1.sh. P has samearchives,
current-source.json,old-five-source.json,package.sha256 and immutable src/old-five-src.
Original source revisions and compilefeature modes are explicitly recorded.
Stages:build actualolderARMprobe;build3helpers default+retained;generate104sources
with olderadmission checks;run416samecopycycles;run query_scalar/query_multimodel
bothconfigs. Success requires P/run.exit0 and per-report inspection, not merely
an active service or default systemdResult. At lastcheck old-five-build PASS
(3m17s), currenthelper build active; no nativequery/cyclepass claimed yet.

Resource evidence: CPUQuota100% is genuinely enforced (cpu.max100000100000),
Nice15/IOWeight10, one Cargo job, service timeout2h. Although systemd accepts
MemoryMax1536M/MemorySwapMax0, Pi kernel exposes only cpuset/cpu/io/pids controllers:
THERE IS NO KERNEL MEMORY/SWAP CAP. Corrected the initial configured-limit claim.
A separate sampled guard e4-phase2-memory-watch-20260917-r1.service checks summed
processRSS every0.25s and stops ONLY our exact service invocation if >1.5GiB or
MemAvailable<512MiB. This is sampled protection, not a strictphysicalceiling.
P/memory-watch.json records samples/peak/stopdecision; guard code local
/tmp/e4-phase2-pi-memory-watch.py and P/memory-watch.py. Initialguardpeak414MB,
available~3.1GB; it began after firstcompiler activity, so isn't a whole-runpeak.
Existing Pi services were neither stopped nor reconfigured.

Eight permanent pure rollback-provenance tests pass; tests/log+driver/testhashes
in docs/phase2-evidence/rollback-provenance-r1/. They cover criss-cross/mixed
revisions, provenance labels, protocols, capability and buildprofiles; no DBs.
This test-only addition occurs after the Pi source capture and changes noengine
or capturedqualificationdriver. Native rollback sources remain as queued.

## Pi qualification and portable restoration complete — 2026-09-17

The native Pi service is terminal: MainPID 0, inactive/dead, Result success,
run.exit 0, all 11 stages PASS. Current candidate source remains unchanged.
Graph/multimodel/lifecycle qualification passes 104 original sources, 208
pairwise handoffs and 200 actual earlier-engine admission checks. Full same-copy
rollback passes 416 cycles and 2,508 recorded commands; every source covers all
four checkpointed/pending-WAL handoff pairs. Default and retained query suites
also pass: 8 multimodel + 3 scalar per build, 22 total, no failures/ignores.
Reports: PHASE2_ROLLBACK_RESULTS.md and PHASE2_PI_QUERY_RESULTS.md. Raw evidence:
phase2-evidence/pi-cycles-r1/ and pi-query-r1/.

The actual original databases, ARM executables, both captured source snapshots,
vendored dependencies, provenance and reports are preserved on scratch:
<scratch>
55,689,410 bytes; SHA-256
5f2554a6e1a57aafaebd97c39e3bef06a15b6b82be6503f956b6150f69f5891d.
All 2,029 member hashes/sizes/modes were independently checked after download.
ARCHIVE.json and receipt are adjacent. Generated mutation copies and Cargo
build targets are excluded. This preserves an unaccepted query candidate,
not a released/frozen runtime or historical cross-release qualification.

Restoration into a different native Pi directory also PASS:
<scratch>
All 416 cycles rerun with relocated preserved executables and original fixtures;
no fixture generation, source archive and all 2,029 restored inputs unchanged.
Raw reports/script in phase2-evidence/pi-restore-r1/. This repetition proves
archive portability, not additional compatibility scope. Original Pi run remains
intact. Both archive/restore jobs ended successfully; no Pi research job remains
active. Memory guard never stopped the correctness run; sampled peak 738,312,192
bytes, not a whole-run/hard bound. End reason DIFFERENT_INVOCATION follows the
successful monitored service termination.

server 1M is still active: first SQLite arm completed (1,400.459 seconds total
process time), first E4 atomic late-index build safely refused after loading all
entities/edges; resumable E4 arm-002 PID80672 confirmed live, last 22 minutes.
These are partial individual trials, not accepted medians or a complete matrix.
No benchmark restart, protocol change or overlapping server work. Repair and
rollback queues remain waiting on the same live benchmark. Final candidate
keep/revert, accepted-source qualification, positional-text scope and full
Phase 2 acceptance/commit remain outstanding.

## Phrase requirement closed (implementation and tests)

tracker p2-fulltext-index's success criteria explicitly include phrase and
token-position cases. This is now implemented as `TextMatch::Phrase` in
`src/index/text/mod.rs` and `src/query/` (the layout restructure at `f5e4c7e`
split the former `src/query.rs` into `src/query/*`), with KMP refinement in
`src/index/text/analyzer.rs`: candidates come from all-term postings, then a
contiguous ordered check against the authoritative primary text on the same
snapshot runs before ranking/top-k, metered by the TextTokens budget with
cancellation checks. Persisted encoding and existing BM25/Any/All behavior are
unchanged; no positions are persisted and no automatic rebuild was introduced.

Tests: `tests/index_text.rs` and `tests/query_multimodel.rs` (see the test
names in `PHASE2_ACCEPTANCE_REVIEW.md`). Mac result 2026-09-17: 20/20 passing
in both the default build and the
`compact-cells,sqlite-balance,keyspace-append,slotref-split` build. Observed
cost: phrase refinement meters every token of every all-term candidate, so it
needs a larger token budget than an equivalent all-term query; this is
documented behaviour, not a defect.

Native Linux qualification is pending, as part of the final Phase 2 job
described above.

## Graph traversal read-path fix — 2026-09-17

A new bench group measures graph traversal directly: 50,000 people, 150,000
edges, 100 spread seeds, warm cache, on the Mac; each seed's answer is
asserted against the generator's own oracle, and SQLite's recursive-CTE plan
is pinned to its indexes with the plan itself asserted. Ratio below is E4
wall time ÷ SQLite wall time (lower favors E4); bench is the same
`phase2_multimodel_bench.rs` family as the CRUD table in
`PHASE2_INDEX_BUILD_RESULTS.md`.

Before the fix: outgoing 1-hop 3.10 (11.9 vs 3.8 microseconds, median),
incoming 1-hop 1.42, 3-hop BFS 1.74 (65 vs 38 microseconds), and a
members-of-organization fan-in over 500 edges 13.2 (488 vs 38 microseconds).

Cause: for every edge visited, the read path did a point lookup into that
edge's opposite-direction copy, then decoded its JSON properties and
discarded the result. Both directions of an edge are written in the same
transaction, so a committed snapshot can never hold half a pair; the
explicit read-only verifier already reports both damage cases (a missing
primary or a missing reverse row) independently of traversal. The lookup
was therefore redundant work, not a correctness need — see the corrected
sentence in `PHASE2_GRAPH_FORMAT.md`.

After the fix (staged, uncommitted): members-of-organization fan-in 2.59
(98 microseconds), outgoing 1-hop 1.57 (7.0 microseconds), 3-hop BFS 0.79
(31 microseconds — E4 faster than SQLite here), incoming 1-hop 1.72 (6.6
microseconds; this is a 2-row read and the change is within noise of the
before number). Page accesses: a BFS touching 300 incoming edges fell from
924 to 24; a neighbours call over 200 outgoing edges fell from 1,226 to 26.

A further pass — an allocation-free range walk, tracked as item H2 — is in
progress. Until it lands, treat the members-of-organization fan-in case as
still above the 2.0x acceptance gate.

Separately: the pre-existing single-shot combined-query timings
(`scalar_active_age`, `members_active_spatial_vector`, `text_active_vector`,
`combined_graph_active_bbox_vector`) are each one cold execution, including
query planning and first page touches — SQLite recorded 11,995 cache misses
during that one stage. They measure first-execution latency only and must
not be used to rank the two engines against each other.

## Linux final qualification status — 2026-09-17

server job `e4-phase2-final-20260917`. Attempt 1, on `dbdbd71`, failed only on
packaging: the macOS `tar` shipped AppleDouble `._` files alongside the
frozen fixtures; the archive step now strips them before packaging. Attempt
2, on `dbdbd71`: stage 1 (default build, full workspace) PASS; stage 2
(retained build, full workspace) PASS; stage 4 (lifecycle replay) failed on
a harness mismatch only — the fixture binaries hardcoded feature mask 63
against the engine's `0xff`, and the tarball builds carried no engine
revision. Attempt 3, on `f538d4d`, is running now.

Attempt 3 result: in progress — record the outcome here once it completes.

## Known limits, stated rather than fixed

Beyond the structural limits already listed under "Format and limits"
above: a held reader lets the WAL grow to its fixed allowance, after which
writes are refused with committed state verified (matching SQLite's
behavior under a long-held reader) — snapshot lifetime is a product limit,
not a defect. The text head-to-segment fold is not implemented (design in
`PHASE2_TEXT_DESIGN.md`). Packed text entries stay tombstoned until an
explicit rebuild. Quantized-vector recall has only been shown on synthetic
data. Each index created consumes tree-id space from a `u16` range.

## Remaining before acceptance, 2026-09-17

- The Linux qualification attempt 3 result (above).
- Item H2, the allocation-free range walk for graph traversal.
- One larger matched run, 1,000,000 rows, on Linux, for the converged
  CRUD/build table (`PHASE2_INDEX_BUILD_RESULTS.md`).
- The two-arm lean bench (a port of e3's `bench/three_ways`), in progress,
  as the post-change smoke check.
- The owner's commit.

## 2026-09-18 — Linux qualification attempt 4 GREEN; further query and build items landed

Everything below the previous "Remaining before acceptance" list is now
answered for the committed head; the items that came after are separate,
staged work the owner will commit tomorrow morning. This section is the
current status. Older numbers elsewhere in this file that these results
supersede are left in place and marked superseded, not deleted.

### Linux final qualification: attempt 4, GREEN

server job `e4-phase2-final-20260917`, attempt 4, is on the committed head
`aeeae13` (eight commits `d0cbc7b`..`aeeae13`, landed the morning of
2026-09-18). Result: exit 0, all stages passed — stage 1 (default build,
full workspace) PASS, stage 2 (retained-feature build, full workspace) PASS,
stage 4 (lifecycle replay) PASS with 404 passing results including guard
checks, stage 5 (release binaries) PASS. This closes attempt 3, which was
still running when the previous entry in this file was written, and
supersedes the earlier lifecycle-replay harness-mismatch failure from
attempt 2 (that mismatch was in the fixture binaries' hardcoded feature
mask, not an engine defect, and is not reproduced here).

### Staged, uncommitted items on top of `aeeae13` (owner to commit)

The owner kept the following items from the loop; they are staged in the
working tree, not yet committed, pending the owner's own commit pass. Each
ratio below is E4 wall time ÷ SQLite wall time (lower favors E4), matched
durability, retained-feature build, this Mac unless noted; case names are
from the lean bench `src/bin/two_ways.rs` at 20,000 rows. Per-item detail
notes with full numbers live on the tracker nodes and in
`.insert-loop/loop3/`.

- **Q2 — descending scalar cursor.** Drives an `ORDER BY ... DESC LIMIT`
  from the order index when a bounded probe of that index says the index
  path pays, with no winner re-fetch: one dense-v3 walk per projection,
  lockstep with row reach. `ORDER BY DESC LIMIT 50`: 13.1x to 0.62x.
  Equality filter plus order, `LIMIT 10`: 4.9x to 0.11x. Full star scan:
  8.3x to 2.2x.
- **H4 — graph filter on the shared BFS.** The query engine's graph filter
  path is rewired onto the shared BFS implementation. 1-hop with
  projection: 5.7x to 1.11x.
- **B1 — bounded index build.** The late-build atomic policy no longer
  packs a whole scalar/vector/text index in one uncommitted transaction: the
  first sorted run is packed to a quarter of the page-WAL allowance, and the
  remainder is appended in its own subsequent commits. A refused build no
  longer loses the index registry entry (see the root-cause note below); the
  guard refuses only pending USER writes, not the build's own commits. The
  1,000,000-row build that used to die with `Error: NotFound("index")` now
  completes. Named sacrifice: the 200K scalar build costs +0.19 s.
- **Q3 — id-ordered and existence-only paths.** No heap allocation for
  id-ordered pages, no page reserved for an empty answer, a per-handle
  descriptor cache, and key-only existence answered by key compare instead
  of a row fetch. Empty-result queries: 4.3x to 0.8x. Key scan: 2.4x to
  1.2x. Range: 2.5x to 1.2x. Ascending indexed order: 1.7x to 0.9x.
- **Q4 — batched row reads.** Row-needing predicates now batch their row
  reads, work on borrowed leaf bytes, and a phrase query with no term map
  skips building one. `two_ranges`: 3.07x to 1.55x. `match_phrase`: 4.87x
  to 2.88x.
- **G2 — graph write existence window.** The existence-proof window used
  for a graph write (previously a fixed 65,536-entry window) is replaced by
  a per-collection allocated range: 25 to 8.75 page accesses per edge, flat
  in N. 300,000-relationship load: 31.5 s to 24.3 s.
- **T2 — BM25 winner-probe skip.** The BM25 page path skips a redundant
  winner-existence probe. `bm25_one_term`: 1.62x to 0.84x. Two terms: 1.47x
  to 0.79x. `bm25_common`: 3.81x to 2.86x.
- **Q5 — range posting as existence proof.** A range posting is now
  trusted as an existence proof the same way an equality posting already
  was, letting a same-index filter fold. `range_two_sided`: 2.07x to 0.38x.
  `range_open`: to 0.51x. `range_closed`: to 0.49x. `between`: to 0.81x.

**Pending, not claimed:** item K1 (a kernel per-collection append hint
intended to cut page accesses per edge write by roughly half, I/O
byte-identical to the unhinted path) needs a byte-identity proof before it
can be listed as done. Status: pending.

### Root cause behind B1, for the record

`.insert-loop/loop3/ROOTCAUSE-1m-index-notfound.md` traces the pre-B1
failure precisely: the atomic build path created an index and drove it
straight to READY inside the same open transaction as the surrounding
loads, with no intermediate commit. At 1,000,000 rows the whole-index pack
exceeded the page-WAL's fixed allowance and was refused; the build driver's
recovery then rolled back everything uncommitted since the last real
commit, including the index's own CREATE, so the retry looked up a registry
row that no longer existed and got `NotFound("index")` — a different error
type than the `ResourceLimit` the driver's retry logic and the bench's
refusal handler were both written to catch. 50K and 200K never hit this
because the whole-index pack fit inside the allowance at those sizes. B1's
bounded-build design (above) closes this class by committing progress
inside the build itself.

### Lean bench standing after T2, before Q5 (20,000 rows, Mac)

`src/bin/two_ways.rs`: 29 cases E4-slower, 19 cases E4-faster, 0
disagreements (row count or key sequence mismatches) — up from the first
run of this bench, 42 slower / 6 faster. Graph is faster than SQLite on
every measured case at this point: 2-hop 0.17x, ... 1-hop with projection
1.06x, 1-hop 0.41x.

Cases still above the 2x acceptance gate, and why, as of this standing
(after Q5 several of these move further; see
`PHASE2_INDEX_BUILD_RESULTS.md` for the case-by-case table once posted):

- `bm25_common` 2.86x — the query returns all 20,000 rows; the remaining
  cost is per-returned-row output, not per-candidate scoring.
- `scan full_one_col` 3.4x — 4 of the 5 allocations per row are shape
  required by the public API, not the storage path.
- `match_phrase` 2.9x — tokenizes per candidate.
- `three_and` 2.3x and `paginate filter_order_limit` 2.0x — the row decoder
  validates every field it steps over to reach the one it needs; skipping
  that validation on a committed-snapshot read is a read-contract decision
  for the owner, not taken in this loop.

### 1,000,000-row same-machine pair, E4 with B1 vs SQLite

Ratios are E4 ÷ SQLite wall time unless marked otherwise.

Mac, matched pair: load entities 0.99, load relationships 1.27, build
scalar index 1.47, build spatial index 0.36, build text index 1.82, updates
(three rounds) 0.93, deletes (three rounds) 0.91, reinsert + edges (three
rounds) 1.38, final file size 1.15, query scalar 1.40, query spatial 1.52,
query text 0.99, query vector 0.90.

Linux, SQLite reference only (server, 2 CPU): updates (three rounds) 1,303 s,
reinsert 97.7 s, final file size 660 MiB. The matching E4 Linux 1M run with
B1 has not run yet — it is pending the owner's commit of B1, since the
qualified Linux job runs against committed source only.

### Measured and rejected in this stretch of the loop

- **Per-row field offset table.** Would change the on-disk row format. The
  field walk this would remove is 15-21% of per-row cost, the change costs
  +3.6% on-disk size, and no lean-bench case crosses the 2x gate as a
  result. Not taken.
- **Per-tree edge tags for 1-hop reads.** `.insert-loop/loop3/DECISION-graph-1hop-parity.md`
  shows the shared and a dedicated per-collection edge tree have the same
  B-tree height at both 50K and 1M rows by fan-out arithmetic (fan-out
  ~170; height-reducing boundaries sit at ~29K and ~4.9M leaves, neither
  matched size lands on one). A dedicated tree would reduce distinct pages
  touched per traversal but not descent depth — item F already measured
  this shape (-22.7% pool accesses, no wall-time change) and was reverted
  for the same reason. Not taken; 1-hop reads stay at rough SQLite parity,
  revisit only if a real workload makes 1-hop dominate.

### Owner decisions still pending, unchanged from before

(a) Whether to skip validation of skipped fields on a committed-snapshot
predicate read (affects `three_and`, `filter_order_limit`, and others
above). (b) Whether the projected-field-name `String` clone in the public
API can change. (c) Whether `OFFSET` should be added to `QueryRequest`; it
is unsupported today.

### Known limits, unchanged

The limits already listed above under "Known limits, stated rather than
fixed" are unchanged by this stretch of work: snapshot lifetime is bounded
by the WAL allowance, the text head-to-segment fold is not implemented,
packed text entries stay tombstoned until an explicit rebuild, and
quantized-vector recall is measured on synthetic data only.
