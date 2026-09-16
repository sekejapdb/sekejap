# V2 foundation benchmark — 1000000 rows

Root: `<scratch>`

Checkpoint/durability policy — E4: `"page-WAL automatic fold near its internal allowance (see src/pagewal.rs); explicit Database::checkpoint() also called every phase"`. SQLite initial: `{"engine":"sqlite","explicit_checkpoint_per_phase":"PRAGMA wal_checkpoint(TRUNCATE)","journal_mode":"wal","synchronous":2,"wal_autocheckpoint_pages":1000}`; after reopen: `{"engine":"sqlite","explicit_checkpoint_per_phase":"PRAGMA wal_checkpoint(TRUNCATE)","journal_mode":"wal","synchronous":2,"wal_autocheckpoint_pages":1000}`.

| phase | E4 write s | SQLite write s | E4 ckpt s (done) | SQLite ckpt s (done) | E4 c/u/d | SQLite c/u/d | E4 bytes | SQLite bytes | E4 peak logical | SQLite peak logical |
|---|---|---|---|---|---|---|---|---|---|---|
| "load" | 50.220 | 19.915 | 0.025 (true) | 0.015 (true) | 1000000/0/0 | 1000000/0/0 | 296153184 | 271269888 | 299337216 | 275653600 |
| "update_round_1" | 9.169 | 6.769 | 0.005 (true) | 0.002 (true) | 0/200000/0 | 0/200000/0 | 296153184 | 271269888 | 300562400 | 276131520 |
| "update_round_2" | 9.528 | 5.728 | 0.005 (true) | 0.001 (true) | 0/200000/0 | 0/200000/0 | 296153184 | 271269888 | 300562400 | 276131520 |
| "delete_only" | 7.726 | 3.621 | 0.009 (true) | 0.001 (true) | 0/0/100000 | 0/0/100000 | 296153184 | 271269888 | 302132976 | 276737160 |
| "replacement_reinsert" | 4.708 | 1.894 | 0.014 (true) | 0.021 (true) | 100000/0/0 | 100000/0/0 | 328335456 | 299442176 | 332342304 | 303665208 |
| "mixed_round_1" | 23.139 | 9.714 | 0.042 (true) | 0.022 (true) | 100000/200000/100000 | 100000/200000/100000 | 362782816 | 327393280 | 367312208 | 332242552 |
| "mixed_round_2" | 21.759 | 10.150 | 0.009 (true) | 0.042 (true) | 100000/200000/100000 | 100000/200000/100000 | 393150560 | 355344384 | 397613648 | 360197776 |

Final checkpoint: E4 0.000s (completed=true) / SQLite 0.000s (completed=true). Reopen: E4 0.019s / SQLite 0.003s.

Engine-only total (writes + explicit checkpoints + final checkpoint + reopen; excludes oracle verification): E4 126.378s (2400000 ops) vs SQLite 57.898s (2400000 ops). Verification time (excluded above): E4 107.179s / SQLite 112.709s cumulative. Final logical bytes: E4 393150560 / SQLite 355344384. Final allocated bytes: E4 393154560 / SQLite 355344384.

These numbers are a raw-write/mixed-CRUD comparison of typed collections
against SQLite typed columns + JSONB. They are not a claim about combined
multimodel SELECT performance or about all eight laws; peaks are 1ms-sampled
lower bounds, not an enforced cap. See docs/V2_BENCHMARK_PROTOCOL.md.
