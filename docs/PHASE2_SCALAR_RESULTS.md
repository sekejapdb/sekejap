# Phase 2 scalar benchmark — initial Linux diagnostic

Run: server server, isolated job `e4-phase2-20260916`, source based on a69838c with the uncommitted scalar implementation. Single run per engine/size, E4 first. These are diagnostics, not repeated performance acceptance. Graph was not included.

Same payload (external key, age, name, active), 8 MiB configured cache, FULL WAL, 256 rows per ordinary write commit, unique external key plus a nonunique age index. Each churn round updates every row, deletes 10%, then reinserts that 10%. Three rounds finish with the original row count. Independent key oracles run after each round.

| Rows | Measurement | E4 | SQLite | E4 / SQLite |
|---:|---|---:|---:|---:|
| 10,000 | Load seconds | 0.253 | 0.180 | 1.41x |
| 10,000 | Late index seconds* | 0.261 | 0.010 | 26.72x |
| 10,000 | Three CRUD rounds seconds | 2.008 | 0.753 | 2.67x |
| 10,000 | Loaded logical MB | 1.008 | 0.643 | 1.57x |
| 10,000 | Final logical MB | 1.466 | 0.758 | 1.94x |
| 10,000 | Sampled peak logical MB | 5.478 | 5.026 | 1.09x |
| 10,000 | Sampled peak allocated MB | 5.591 | 9.048 | 0.62x |
| 100,000 | Load seconds | 2.599 | 1.702 | 1.53x |
| 100,000 | Late index seconds* | 3.161 | 0.053 | 59.94x |
| 100,000 | Three CRUD rounds seconds | 23.156 | 16.429 | 1.41x |
| 100,000 | Loaded logical MB | 9.937 | 6.328 | 1.57x |
| 100,000 | Final logical MB | 15.557 | 7.967 | 1.95x |
| 100,000 | Sampled peak logical MB | 19.742 | 12.824 | 1.54x |
| 100,000 | Sampled peak allocated MB | 23.884 | 16.388 | 1.46x |

*Late indexing currently compares different publication policies: E4 commits every bounded 256-row build step; SQLite publishes one CREATE INDEX transaction. This measures E4 resumability overhead as well as indexing. An additional one-final-commit E4 arm is required before attributing the gap to index insertion itself. Ordinary CRUD uses the same commit batch for both engines.

Peak samples occur after commits/phases, so they do not establish the true within-commit disk maximum. Logical bytes and allocated filesystem blocks differ; both are retained in the JSON. No claim of a hard disk safety factor is made. Sub-millisecond query timings are single observations, insufficient for a query-speed claim.

100K final disk is 1.95x SQLite on this small scalar workload. This needs explanation/measurement before acceptance; older near-parity mixed-payload foundation results do not establish parity once a new index is included. No index representation is frozen by this diagnostic.

Evidence: `phase2-evidence/logs/bench-r5-*.json`. Default scalar workspace suite: 440 passed, 0 failed, 2 existing ignored (`suite-default-r4.log`). Preserved Phase 1 ordinary-feature read/write/rollback comparison: 10 arms passed (`compat-default.log`). New-feature old-binary refusal is separately running; do not infer it from these ten arms.
