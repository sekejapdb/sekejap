# Combined-query guard qualification

`query-guards-r2` finished with exit 0 on isolated server Linux, 2026-09-17.
Both default and retained builds passed 93 checks: 19 selected integration
tests plus 74 root-library tests. The runnable multimodel example also passed
under both configurations. This is candidate correctness evidence, not a
performance or full Phase 2 acceptance result.

The changes meter graph-seed reads, check cancellation during long exact-vector
lane scans, and verify that every emitted winner still has an authoritative
entity. Projection reuses that winner lookup. Independent tests exercise
filtered approximate ranking with an effort limit that distinguishes answers,
exact/BM25 ties across pages, rollback answers, and orphan winners in all six
native candidate drivers. The complete 65,537-match pagination fixtures ran.

Ordinary queries cannot certify missing postings or every stale but valid
spatial/text posting. The full verifier and explicit rebuild remain the
integrity tools; this change does not add full candidate rescoring as a claim
of universal corruption detection.

Evidence: [query-guards-r2](phase2-evidence/query-guards-r2/), including exact
source hashes, test logs, example output and the completion marker. Native
source: `<scratch>` in server
namespace `sekejap-benchmark`. Broader lean/full workspace runs follow this
capture; larger benchmarks must identify the later guarded source explicitly.
