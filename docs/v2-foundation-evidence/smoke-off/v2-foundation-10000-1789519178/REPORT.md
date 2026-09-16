# V2 foundation benchmark — 10000 rows

Root: `<scratch>`

Checkpoint/durability policy — E4: `"page-WAL automatic fold near its internal allowance (see src/pagewal.rs); explicit Database::checkpoint() also called every phase"`. SQLite initial: `{"engine":"sqlite","explicit_checkpoint_per_phase":"PRAGMA wal_checkpoint(TRUNCATE)","journal_mode":"wal","synchronous":2,"wal_autocheckpoint_pages":1000}`; after reopen: `{"engine":"sqlite","explicit_checkpoint_per_phase":"PRAGMA wal_checkpoint(TRUNCATE)","journal_mode":"wal","synchronous":2,"wal_autocheckpoint_pages":1000}`.

| phase | E4 write s | SQLite write s | E4 ckpt s (done) | SQLite ckpt s (done) | E4 c/u/d | SQLite c/u/d | E4 bytes | SQLite bytes | E4 peak logical | SQLite peak logical |
|---|---|---|---|---|---|---|---|---|---|---|
| "load" | 0.404 | 0.132 | 0.030 (true) | 0.013 (true) | 10000/0/0 | 10000/0/0 | 2891872 | 2613248 | 6410128 | 5476680 |
| "update_round_1" | 0.065 | 0.035 | 0.028 (true) | 0.012 (true) | 0/2000/0 | 0/2000/0 | 2891872 | 2613248 | 4997024 | 4908120 |
| "update_round_2" | 0.075 | 0.035 | 0.025 (true) | 0.014 (true) | 0/2000/0 | 0/2000/0 | 2891872 | 2613248 | 4997024 | 4908120 |
| "delete_only" | 0.035 | 0.024 | 0.024 (true) | 0.015 (true) | 0/0/1000 | 0/0/1000 | 2891872 | 2613248 | 5755376 | 5192400 |
| "replacement_reinsert" | 0.038 | 0.015 | 0.016 (true) | 0.005 (true) | 1000/0/0 | 1000/0/0 | 3194976 | 2895872 | 3572080 | 3209024 |
| "mixed_round_1" | 0.117 | 0.066 | 0.029 (true) | 0.028 (true) | 1000/2000/1000 | 1000/2000/1000 | 3526752 | 3158016 | 7003568 | 6140928 |
| "mixed_round_2" | 0.118 | 0.054 | 0.034 (true) | 0.015 (true) | 1000/2000/1000 | 1000/2000/1000 | 3825760 | 3424256 | 7256992 | 6407168 |

Final checkpoint: E4 0.000s (completed=true) / SQLite 0.000s (completed=true). Reopen: E4 0.014s / SQLite 0.002s.

Engine-only total (writes + explicit checkpoints + final checkpoint + reopen; excludes oracle verification): E4 1.054s (24000 ops) vs SQLite 0.465s (24000 ops). Verification time (excluded above): E4 0.831s / SQLite 0.926s cumulative. Final logical bytes: E4 3825760 / SQLite 3424256. Final allocated bytes: E4 3829760 / SQLite 3424256.

These numbers are a raw-write/mixed-CRUD comparison of typed collections
against SQLite typed columns + JSONB. They are not a claim about combined
multimodel SELECT performance or about all eight laws; peaks are 1ms-sampled
lower bounds, not an enforced cap. See docs/V2_BENCHMARK_PROTOCOL.md.
