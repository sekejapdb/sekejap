# V2 foundation benchmark — 1000000 rows

Root: `<scratch>`

Checkpoint/durability policy — E4: `"page-WAL automatic fold near its internal allowance (see src/pagewal.rs); explicit Database::checkpoint() also called every phase"`. SQLite initial: `{"engine":"sqlite","explicit_checkpoint_per_phase":"PRAGMA wal_checkpoint(TRUNCATE)","journal_mode":"wal","synchronous":2,"wal_autocheckpoint_pages":1000}`; after reopen: `{"engine":"sqlite","explicit_checkpoint_per_phase":"PRAGMA wal_checkpoint(TRUNCATE)","journal_mode":"wal","synchronous":2,"wal_autocheckpoint_pages":1000}`.

| phase | E4 write s | SQLite write s | E4 ckpt s (done) | SQLite ckpt s (done) | E4 c/u/d | SQLite c/u/d | E4 bytes | SQLite bytes | E4 peak logical | SQLite peak logical |
|---|---|---|---|---|---|---|---|---|---|---|
| "load" | 50.001 | 17.580 | 0.018 (true) | 0.011 (true) | 1000000/0/0 | 1000000/0/0 | 285298784 | 257851392 | 288382928 | 262226864 |
| "update_round_1" | 12.168 | 5.376 | 0.030 (true) | 0.001 (true) | 0/200000/0 | 0/200000/0 | 285298784 | 257851392 | 290540944 | 262441104 |
| "update_round_2" | 9.693 | 5.698 | 0.008 (true) | 0.001 (true) | 0/200000/0 | 0/200000/0 | 285298784 | 257851392 | 290540944 | 262441104 |
| "delete_only" | 7.150 | 3.156 | 0.008 (true) | 0.001 (true) | 0/0/100000 | 0/0/100000 | 285298784 | 257851392 | 291067232 | 263050864 |
| "replacement_reinsert" | 5.647 | 3.459 | 0.054 (true) | 0.025 (true) | 100000/0/0 | 100000/0/0 | 316379232 | 286023680 | 320216576 | 290254952 |
| "mixed_round_1" | 17.461 | 8.136 | 0.007 (true) | 0.013 (true) | 100000/200000/100000 | 100000/200000/100000 | 350036064 | 312635392 | 354412128 | 317233344 |
| "mixed_round_2" | 19.063 | 9.676 | 0.006 (true) | 0.018 (true) | 100000/200000/100000 | 100000/200000/100000 | 380416096 | 339243008 | 384738288 | 343845080 |

Final checkpoint: E4 0.000s (completed=true) / SQLite 0.000s (completed=true). Reopen: E4 0.019s / SQLite 0.003s.

Engine-only total (writes + explicit checkpoints + final checkpoint + reopen; excludes oracle verification): E4 121.336s (2400000 ops) vs SQLite 53.153s (2400000 ops). Verification time (excluded above): E4 93.255s / SQLite 98.748s cumulative. Final logical bytes: E4 380416096 / SQLite 339243008. Final allocated bytes: E4 380420096 / SQLite 339243008.

These numbers are a raw-write/mixed-CRUD comparison of typed collections
against SQLite typed columns + JSONB. They are not a claim about combined
multimodel SELECT performance or about all eight laws; peaks are 1ms-sampled
lower bounds, not an enforced cap. See docs/V2_BENCHMARK_PROTOCOL.md.
