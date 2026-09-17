# R6 committed-state refusal smoke

The Linux R6 smoke finished with exit 0 on 2026-09-17. Two no-reader workloads
completed all three CRUD rounds (E4 atomic and SQLite atomic). Two E4 workloads
refused at the fixed WAL allowance; both independently verified the last
committed state after closing the failed writer and reopening read-only.

| E4 refusal case | Last committed update boundary | Independently verified state |
|---|---:|---|
| Atomic index publication, snapshot held for all rounds | Round 1, 5,888 people updated | 10,000 people, 100 organizations, 30,000 relationships, five catalog indexes and selected Ready-family query answers |
| Resumable index publication, snapshot held for one whole round | Round 1, 5,888 people updated | Same exact committed boundary and verification scope |

The following update batch was not counted as committed. All entity and
organization IDs/payloads and relationship properties matched the deterministic
committed boundary. Catalog state and scalar, exact-vector, spatial and text
query checks passed. Each post-refusal oracle took approximately 0.5 seconds,
outside named workload timings. Partial retained logical size was 24,090,464
bytes in both cases. This is evidence of safe refusal in these cases, not
completion of the requested workload or exhaustive corruption detection.

The corrected SQLite `WITHOUT ROWID` edge schema passed its runtime plan
guards. Its recursive term scans the frontier, then searches the composite
primary key with source ID bound. Forward cascade deletion uses the primary
key, and incoming deletion/membership uses the reverse index. The duplicate
forward index used by R4 is absent.

These are single smoke runs, not three-trial performance results. The compact
ledger's pure partial-delete/reinsert/edge-restoration test also passed on
Linux. The R7 batch-reader smoke and repeated 10K/100K comparisons follow.

Evidence: [multimodel-smoke-r6](phase2-evidence/multimodel-smoke-r6/), including
raw JSON outputs, exact source inventory, binary hash, unit-test log and
completion markers. Reader lifetimes, schema changes and result semantics are
specified in [PHASE2_MULTIMODEL_PROTOCOL.md](PHASE2_MULTIMODEL_PROTOCOL.md).
