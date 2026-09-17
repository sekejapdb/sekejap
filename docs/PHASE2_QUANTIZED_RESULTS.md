# Explicit quantized-vector candidate qualification

Candidate evidence, 2026-09-17. No Phase 2 release or format freeze is claimed.

The Linux `quant-family-r1` run passed **100 checks in each of the ordinary and
retained builds**: 27 selected integration tests and all 73 root-library tests.
The retained build enables compact-cells, sqlite-balance, keyspace-append and
slotref-split. Both executions used the captured `quant-rebuild-r1` source.

Coverage includes the independent approximate-shortlist/exact-rerank oracle,
three distance metrics, lifecycle, live changes, old snapshots, reopen,
historical vector ordinals, malformed values and unsupported descriptor
refusal. The root suite exercises every injected key-write failure through
quantized catalog creation, insert, update, delete, build and drop. Existing
vector, verifier and mixed-query regressions also pass. Concurrent explicit
codec creation leaves the process default unchanged; existing-directory
refusal preserves its byte inventory.

The original `quant-rebuild-r1` integration run failed two successful-rebuild
cases with `WriterLocked`. The rebuild attempted a second protected opening
of the source data file while the current-source reader already owned it.
The correction reuses physical features from that reader's validated pager
state. Rebuild qualification is separate; the 100-check result does **not**
include a passing rebuild claim. The original failure log is preserved.

The candidate stores symmetric int8 approximations separately from original
f32 vectors, scans compact entries, then reranks a bounded shortlist exactly.
Selection remains approximate. These tests prove specified behavior, not useful
recall or a speed benefit. Measured recall, extra disk/write cost, combined
approximate-query qualification, preserved older-engine fixture checks and
expanded crash checks remain required before acceptance.

Evidence and source inventory:
[phase2-evidence/quant-family-r1](phase2-evidence/quant-family-r1/).

## Combined-query and rebuild-budget follow-up

`approx-query-r1` completed successfully on Linux in both builds:
**101 tests per build** (27 selected integration checks and74 root-library
checks), plus the runnable mixed-query example. Explicit approximate order
reports method, effort, examined and reranked counts. Tests exercise filtered
shortlisting, pagination, retry and old/new snapshot answers for both exact
and approximate modes.

This candidate also passes the stricter rebuild logical-byte preflight,
including completion-marker, coordination and empty-pager creation headroom,
with persisted resource-policy preservation. It does not cap filesystem
allocation beyond file lengths. Evidence: [approx-query-r1](phase2-evidence/approx-query-r1/).

A subsequent review added stronger filtered-oracle, tied-pagination, orphan
winner, graph-read accounting and exact-score cancellation checks; these are
queued separately as query-guards-r2. This101-test result does not stand in
for that later qualification. Measured recall/performance remains pending.
