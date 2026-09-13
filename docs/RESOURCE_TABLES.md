# Raspberry Pi resource comparison — 2026-09-11

One pair per mode, case and size; 4 mutation cycles. All processes ran under a
128 MiB address-space limit (RLIMIT_AS). Filesystem cache is outside this limit; swap is not disabled.
The 4M rung is load-only with ascending IDs;
100K/400K use a fixed shuffled order. Both engines receive the same order.
The 4M rung tests resource scale; it is not an order-matched timing exponent. Durability is FULL
using Linux fsync, with 1000-operation commits, 8 MiB configured writer budgets,
64 KiB snapshot caches, no mmap, and native SQLite auto-checkpoint=1000 pages.
Both engines also checkpoint at phase ends. E4 constrained commits publish
metadata every transaction; ordinary E4 uses the 4 MiB WAL-or-page trigger.

`mixed_long` pins the initial snapshot through cycles 1–2, releases it after
cycle 2, then runs cycles 3–4 without that reader. `mixed_none` has no pinned
snapshot. Each mixed cycle updates/deletes 30% of IDs, then reinserts 10%.
The fixed `updates` case changes 30% without deletes or payload growth.

The two SQLite rows are the separate paired controls, retained individually.
Ordinary pairs run E4 first; constrained pairs run SQLite first. These are
single device measurements, not a repetition-based parity claim. Peaks are
sampled. Allocated file bytes exclude directory/filesystem metadata; RSS
excludes filesystem cache. Limits are not free-space
reservations. RSS is the sampled process RSS during each arm; the two arms
share a process, so allocator retention from the first can affect the second.
MiB = 1,048,576 bytes. Initial size is the post-load footprint; peak/initial
is observed growth, not a promised safety factor. The matrix uses generous
explicit E4 limits, not an automatic 2×-initial policy.

The user rebooted the Pi to attach sensors after the eight 100K pairs and two 400K load pairs. All 20 completed databases were reverified after reboot. The interrupted 400K update pair was preserved and rerun in full; no accepted pair combines timings from opposite sides of the reboot.

Timing caveat (†): the final constrained 4M pair ran from 14:09:29 to 14:15:34. Other Pi Python jobs started at 14:10:08 and 14:12:45; the final hardware record shows load averages 5.96/3.98/2.57, with no throttling. LLM jobs were also observed later. The affected pair proves correctness/resource behavior, but its timing cannot isolate the cost of constrained commits. No other user jobs were stopped.

| Rows | Case | Engine / paired mode | Load s | Mutation s | Initial MiB | Peak MiB | Peak / initial | Allocated peak MiB | Final MiB | RSS MiB |
|---:|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 100,000 | load | E4 constrained | 21.69 | 0.00 | 28.04 | 28.07 | 1.00× | 28.08 | 28.04 | 18.58 |
| 100,000 | load | SQLite (limited pair) | 20.12 | 0.00 | 16.74 | 24.56 | 1.47× | 24.57 | 16.71 | 12.94 |
| 100,000 | load | E4 ordinary | 20.15 | 0.00 | 28.04 | 28.25 | 1.01× | 28.25 | 28.04 | 9.50 |
| 100,000 | load | SQLite (ordinary pair) | 20.42 | 0.00 | 16.74 | 24.56 | 1.47× | 24.57 | 16.71 | 13.45 |
| 100,000 | mixed_long | E4 constrained | 21.70 | 45.77 | 28.04 | 65.91 | 2.35× | 65.92 | 65.28 | 25.34 |
| 100,000 | mixed_long | SQLite (limited pair) | 20.35 | 31.40 | 16.74 | 443.31 | 26.48× | 443.32 | 31.38 | 23.02 |
| 100,000 | mixed_long | E4 ordinary | 19.88 | 46.15 | 28.04 | 65.96 | 2.35× | 65.96 | 65.28 | 10.19 |
| 100,000 | mixed_long | SQLite (ordinary pair) | 20.25 | 30.81 | 16.74 | 443.31 | 26.48× | 443.32 | 31.38 | 24.08 |
| 100,000 | mixed_none | E4 constrained | 22.02 | 45.97 | 28.04 | 47.33 | 1.69× | 47.34 | 46.76 | 18.69 |
| 100,000 | mixed_none | SQLite (limited pair) | 20.08 | 50.15 | 16.74 | 38.60 | 2.31× | 38.60 | 31.38 | 13.08 |
| 100,000 | mixed_none | E4 ordinary | 20.17 | 45.87 | 28.04 | 47.38 | 1.69× | 47.38 | 46.76 | 9.81 |
| 100,000 | mixed_none | SQLite (ordinary pair) | 20.08 | 50.47 | 16.74 | 38.60 | 2.31× | 38.60 | 31.38 | 13.70 |
| 100,000 | updates | E4 constrained | 22.14 | 30.01 | 28.04 | 28.07 | 1.00× | 28.08 | 28.04 | 18.44 |
| 100,000 | updates | SQLite (limited pair) | 20.46 | 31.90 | 16.74 | 24.56 | 1.47× | 24.57 | 16.71 | 13.00 |
| 100,000 | updates | E4 ordinary | 19.57 | 25.61 | 28.04 | 33.95 | 1.21× | 33.95 | 33.55 | 9.56 |
| 100,000 | updates | SQLite (ordinary pair) | 20.16 | 31.82 | 16.74 | 24.56 | 1.47× | 24.57 | 16.71 | 13.53 |
| 400,000 | load | E4 constrained | 136.39 | 0.00 | 83.24 | 83.27 | 1.00× | 83.28 | 83.24 | 18.56 |
| 400,000 | load | SQLite (limited pair) | 141.33 | 0.00 | 67.71 | 75.43 | 1.11× | 75.44 | 67.68 | 12.92 |
| 400,000 | load | E4 ordinary | 133.46 | 0.00 | 83.24 | 83.45 | 1.00× | 83.45 | 83.24 | 9.53 |
| 400,000 | load | SQLite (ordinary pair) | 141.75 | 0.00 | 67.71 | 75.43 | 1.11× | 75.44 | 67.68 | 13.47 |
| 400,000 | mixed_long | E4 constrained | 138.77 | 223.66 | 83.24 | 225.75 | 2.71× | 225.76 | 224.86 | 26.70 |
| 400,000 | mixed_long | SQLite (limited pair) | 142.48 | 141.86 | 67.71 | 1843.35 | 27.23× | 1843.36 | 126.62 | 24.91 |
| 400,000 | mixed_long | E4 ordinary | 133.99 | 220.71 | 83.24 | 225.79 | 2.71× | 225.80 | 224.86 | 12.09 |
| 400,000 | mixed_long | SQLite (ordinary pair) | 145.61 | 170.91 | 67.71 | 1843.35 | 27.23× | 1843.36 | 126.62 | 25.53 |
| 400,000 | mixed_none | E4 constrained | 138.37 | 218.70 | 83.24 | 153.75 | 1.85× | 153.76 | 153.07 | 18.67 |
| 400,000 | mixed_none | SQLite (limited pair) | 144.05 | 233.39 | 67.71 | 134.48 | 1.99× | 134.49 | 126.62 | 13.05 |
| 400,000 | mixed_none | E4 ordinary | 134.35 | 220.88 | 83.24 | 153.80 | 1.85× | 153.81 | 153.07 | 10.44 |
| 400,000 | mixed_none | SQLite (ordinary pair) | 141.84 | 232.70 | 67.71 | 134.48 | 1.99× | 134.48 | 126.62 | 13.70 |
| 400,000 | updates | E4 constrained | 137.64 | 138.54 | 83.24 | 83.27 | 1.00× | 83.28 | 83.24 | 18.64 |
| 400,000 | updates | SQLite (limited pair) | 141.69 | 148.41 | 67.71 | 75.43 | 1.11× | 75.43 | 67.68 | 13.00 |
| 400,000 | updates | E4 ordinary | 133.71 | 139.73 | 83.24 | 83.45 | 1.00× | 83.45 | 83.24 | 9.58 |
| 400,000 | updates | SQLite (ordinary pair) | 141.86 | 149.08 | 67.71 | 75.43 | 1.11× | 75.44 | 67.68 | 13.56 |
| 4,000,000 | load | E4 constrained † | 124.69 | 0.00 | 655.58 | 655.58 | 1.00× | 655.59 | 655.58 | 18.61 |
| 4,000,000 | load | SQLite (limited pair) † | 64.02 | 0.00 | 616.25 | 620.34 | 1.01× | 620.34 | 616.22 | 12.97 |
| 4,000,000 | load | E4 ordinary | 65.94 | 0.00 | 655.58 | 659.27 | 1.01× | 659.28 | 655.58 | 9.19 |
| 4,000,000 | load | SQLite (ordinary pair) | 63.51 | 0.00 | 616.25 | 620.34 | 1.01× | 620.34 | 616.22 | 13.34 |

Validation: 196 current-state checks, 62,400,000 repeated row visits,
32 snapshot checks, exact point-read and reopen agreement in every pair.
Timestamps, vectors, graph edges and secondary indexes are off in both engines.
The corpus contains Unicode names, scalar values, a geo point, nested binary
JSON and undeclared extras. Mixed cycles alternate large and small values.
