# Native streaming query driver qualification

Candidate evidence, 2026-09-17; not Phase 2 release acceptance.

The isolated `query-drivers-r1` Linux run passed **28 tests per build** in both
ordinary and retained builds, plus the runnable mixed-model example in each.
The retained features are `compact-cells,sqlite-balance,keyspace-append,slotref-split`.

The selected suites cover graph, scalar lifecycle/oracle, exact vector, spatial,
fulltext, scalar/JSON queries and mixed queries. Native text posting merge and
spatial range drivers now pass complete pagination with **65,537 matches**:
this explicitly crosses the older 65,536-result collector limit. The example
combines graph, active status, geography, text and exact vector ranking from one
snapshot and reports the actual driver and work counters.

These are correctness results, not evidence of latency or memory parity with
SQLite. The broader performance matrix, approximate vector mode and rebuilt
index qualification remain separate work.

Source inventory, unabridged logs and exit marker are preserved under
[phase2-evidence/query-drivers-r1](phase2-evidence/query-drivers-r1/).
Native source: `<scratch>` in the
isolated server benchmark pod. Test and example logs record exact commands and
results; the source inventory binds them to the candidate used.
