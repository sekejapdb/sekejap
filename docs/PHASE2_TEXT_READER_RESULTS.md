# Phase 2 text and current-reader Linux qualification

Candidate results, 2026-09-17. These selected checks pass; Phase 2 is not yet
accepted, committed, or released. Physical page/WAL/record format remains the
Phase 1 baseline. Explicit text-index creation uses a new logical family.

| Linux qualification | Default | Retained packing |
|---|---:|---:|
| Text family plus existing family regressions, admission, analyzer/field reader and fault checks | 60 passed, 0 failed | 60 passed, 0 failed |
| Source-preserving current-root reader | 7 passed, 0 failed | 7 passed, 0 failed |

Both runs used the existing isolated server pod, 2 CPU / 2 GiB limits, release
builds and offline locked dependencies. Retained enables `compact-cells`,
`sqlite-balance`, `keyspace-append`, and `slotref-split`. Commands, complete logs,
source inventories and final exit markers are in
[phase2-evidence/text-reader-r1](phase2-evidence/text-reader-r1/).
Counts are distinct selected test entries per configuration, not benchmark
samples or a claim of full workspace coverage.

Text coverage includes live and late-built postings, corpus/term statistics,
Unicode analyzer semantics, exact Any/All BM25 queries, updates/deletes,
snapshots, rollback, reopening, admission refusal and exhaustive injected write
failures. Analyzer v1 pins Unicode 17.0.0 behavior. Explicit empty strings count
as zero-token documents; null and missing fields do not count.

The initial text-r1 trial passed three of four behavioral tests. Its failing
budget expectation omitted the term probe required for each present empty or
punctuation-only document. Four candidates require six probes: two each for
those present documents, one each for missing/null. The corrected test requires
budget five to refuse and six to succeed. This changes the test expectation,
not the query's completeness. Earlier review also removed a DF-based early
stop: the actual posting-prefix boundary establishes exhaustion, and counts
are checked rather than trusted to truncate results.

The current reader acquires existing source locks and uses read-only source
handles. It follows committed roots and WAL overlays, validates reachable
pages/overflow chains and fingerprints source inventory. Tests cover committed
updates and deletion, a multilevel tree, overflow replacement, corrupt pages,
a checksum-valid cycle, active writers, resource limits and inventory change.
It does not infer live membership from obsolete rootless records.

Remaining acceptance work includes full mixed-model execution, process-kill
publication schedules, cross-version fixtures, catalog/index integrity
verification, explicit derived-index rebuilding, and matched SQLite workload
measurements. Reader tests alone do not prove complete indexed recovery.
