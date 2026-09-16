# V2 foundation benchmark — 10000 rows

Root: `<scratch>`

Checkpoint/durability policy — E4: `"page-WAL automatic fold near its internal allowance (see src/pagewal.rs); explicit Database::checkpoint() also called every phase"`. SQLite initial: `{"engine":"sqlite","explicit_checkpoint_per_phase":"PRAGMA wal_checkpoint(TRUNCATE)","journal_mode":"wal","synchronous":2,"wal_autocheckpoint_pages":1000}`; after reopen: `{"engine":"sqlite","explicit_checkpoint_per_phase":"PRAGMA wal_checkpoint(TRUNCATE)","journal_mode":"wal","synchronous":2,"wal_autocheckpoint_pages":1000}`.

| phase | E4 write s | SQLite write s | E4 ckpt s (done) | SQLite ckpt s (done) | E4 c/u/d | SQLite c/u/d | E4 bytes | SQLite bytes | E4 peak logical | SQLite peak logical |
|---|---|---|---|---|---|---|---|---|---|---|
| "load" | 0.426 | 0.149 | 0.040 (true) | 0.018 (true) | 10000/0/0 | 10000/0/0 | 3002464 | 2744320 | 6628464 | 5739592 |
| "update_round_1" | 0.066 | 0.035 | 0.032 (true) | 0.014 (true) | 0/2000/0 | 0/2000/0 | 3002464 | 2744320 | 5215360 | 5171032 |
| "update_round_2" | 0.074 | 0.057 | 0.022 (true) | 0.015 (true) | 0/2000/0 | 0/2000/0 | 3002464 | 2744320 | 5215360 | 5171032 |
| "delete_only" | 0.029 | 0.019 | 0.031 (true) | 0.011 (true) | 0/0/1000 | 0/0/1000 | 3002464 | 2744320 | 5977856 | 5455312 |
| "replacement_reinsert" | 0.039 | 0.019 | 0.020 (true) | 0.009 (true) | 1000/0/0 | 1000/0/0 | 3317856 | 3026944 | 3711536 | 3340096 |
| "mixed_round_1" | 0.137 | 0.050 | 0.037 (true) | 0.013 (true) | 1000/2000/1000 | 1000/2000/1000 | 3670112 | 3305472 | 7283680 | 6436704 |
| "mixed_round_2" | 0.134 | 0.064 | 0.040 (true) | 0.013 (true) | 1000/2000/1000 | 1000/2000/1000 | 3977312 | 3584000 | 7541152 | 6711112 |

Final checkpoint: E4 0.000s (completed=true) / SQLite 0.000s (completed=true). Reopen: E4 0.011s / SQLite 0.001s.

Engine-only total (writes + explicit checkpoints + final checkpoint + reopen; excludes oracle verification): E4 1.139s (24000 ops) vs SQLite 0.486s (24000 ops). Verification time (excluded above): E4 0.921s / SQLite 0.996s cumulative. Final logical bytes: E4 3977312 / SQLite 3584000. Final allocated bytes: E4 3981312 / SQLite 3584000.

These numbers are a raw-write/mixed-CRUD comparison of typed collections
against SQLite typed columns + JSONB. They are not a claim about combined
multimodel SELECT performance or about all eight laws; peaks are 1ms-sampled
lower bounds, not an enforced cap. See docs/V2_BENCHMARK_PROTOCOL.md.
