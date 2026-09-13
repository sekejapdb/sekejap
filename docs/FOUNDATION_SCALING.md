# Fixed-work scaling and lean law groups — 2026-09-14

**Decision: retain the test infrastructure; foundation qualification fails.**
The page-WAL engine is unchanged except for exposing existing I/O counters.
The seven laws remain unchanged. The controlled ladder disproves a blanket
flat-insertion-latency claim and exposes a 100K scattered-packing size failure.
No query, index or collection-to-page-WAL integration was added.

## What can now run routinely

[FOUNDATION_LEAN_GROUPS.json](FOUNDATION_LEAN_GROUPS.json) maps exactly seven
laws to shared tests and explicitly lists missing coverage. Three shared
commands run once: pager/fault/repair, typed values/schema/collections, and
inherited kernel resource/work/damage/platform checks. This gives **74 passing
tests per platform**, plus four paired smoke arms. Passing these regressions
does not mark their entire law qualified; typed tests still use the older Store.

```sh
python3 tools/run_foundation.py lean <scratch>
python3 tools/run_foundation.py scale <scratch>
python3 tools/run_foundation.py large <scratch>
python3 tools/check_foundation_gate.py
```

Use a fresh `CHANGE` directory. `lean` is routine regression feedback; `scale`
is required evidence for storage/cache/packing changes; `large` confirms the
larger working set. The final command remains the separate release gate and
currently exits **1**, with 35 explicit failed/pending entries. They are not
35 new defects: the list includes historical and unimplemented qualification.

Measured complete profile times, including build where invoked, load and
verification: lean **107.47 s Mac / 84.19 s Pi**; scale **195.12 / 298.07 s**;
10M confirmation **633.17 / 1,200.16 s**. These are measured runs, not fixed
runtime promises. Pi runs at the stable address `contributor@example.invalid`.

## Fairness and exact work

Every arm starts with an ascending load of even keys. Payloads are identical
8-byte keys and 256-byte values. Both engines use 8 MiB engine caches and
native FULL durability, with SQLite WAL/fullfsync/checkpoint_fullfsync enabled,
WITHOUT ROWID storage, mmap disabled and automatic checkpoints at 1,000 pages.

Each arm then performs **1,000 inserts, 1,000 updates and 1,000 deletes** in
three separately timed transactions. Each time includes its commit and ending
checkpoint. Local insertion appends; local update/delete touch disjoint
contiguous ranges. Scattered insertion uses odd-key gaps across the original
population; scattered update/delete touch disjoint evenly spaced ranges in
deterministically permuted order. Locality arms use separate fresh databases.
The final live population equals the original population.

Each phase reopens its engine, so the engine cache starts empty. Reopen time
is separately reported. OS caches are **not flushed**; this is not a cold-device
benchmark. No process-wide cache purges or Pi services were changed. Pi
benchmark processes run under a **128 MiB address-space limit**.

10K, 100K and 1M have three repetitions with engine/size order rotation. 10M
has one confirmation run; it is not three-repetition acceptance. There are
**80 completed scaling/large arms across Mac and Pi**, plus eight lean smoke
arms. Every changed key is checked, then final reopen streams exact values,
ordering and membership. The oracle keeps only O(changes) state. All same-case
checksums match across engines, repetitions and platforms.

## Fixed mutation time

All entries below are **milliseconds for 1,000 changes**. Each cell is
**E4 / SQLite**, including commit and ending checkpoint. Smaller sizes are
three-run medians; 10M rows are single-run diagnostics.

| Mac population | Locality | Insert ms | Update ms | Delete ms |
|---|---|---:|---:|---:|
| 10K | Local | 23.28 / 27.99 | 22.88 / 25.11 | 60.48 / 36.05 |
| 100K | Local | 23.93 / 26.81 | 24.19 / 25.05 | 28.65 / 25.05 |
| 1M | Local | 25.04 / 27.38 | 25.77 / 28.63 | 30.58 / 27.36 |
| 10M* | Local | 35.51 / 25.47 | 31.23 / 28.40 | 57.66 / 27.03 |
| 10K | Scattered | 63.51 / 54.36 | 49.09 / 46.07 | 85.11 / 71.29 |
| 100K | Scattered | 260.95 / 163.70 | 126.59 / 125.39 | 99.38 / 134.97 |
| 1M | Scattered | 209.55 / 461.55 | 87.49 / 115.94 | 77.21 / 134.87 |
| 10M* | Scattered | 949.04 / 824.98 | 119.60 / 130.46 | 130.27 / 128.57 |

| Pi population | Locality | Insert ms | Update ms | Delete ms |
|---|---|---:|---:|---:|
| 10K | Local | 18.69 / 27.41 | 25.44 / 20.89 | 28.78 / 30.72 |
| 100K | Local | 26.07 / 34.65 | 21.08 / 21.45 | 29.42 / 26.41 |
| 1M | Local | 26.70 / 30.66 | 21.76 / 29.34 | 36.29 / 22.20 |
| 10M* | Local | 20.01 / 23.56 | 26.09 / 78.78 | 43.89 / 48.15 |
| 10K | Scattered | 161.31 / 141.51 | 158.96 / 122.41 | 145.51 / 123.55 |
| 100K | Scattered | 611.38 / 410.69 | 403.61 / 434.56 | 366.39 / 423.47 |
| 1M | Scattered | 1,005.16 / 424.92 | 388.50 / 424.29 | 356.42 / 419.43 |
| 10M* | Scattered | 1,807.49 / 1,570.72 | 358.53 / 860.09 | 250.21 / 1,326.32 |

The local path is approximately stable, but this is not proof of strict
constant latency. Scattered insertion from 10K to 1M rises **3.30× on Mac**
and **6.23× on Pi**, for exactly the same 1,000 insertions. At 1M on Pi,
E4 takes **2.37× SQLite time**, failing the owner's `<1.5×` target. At 100K
on Mac the scattered insertion ratio is **1.59×**, also failing. Some local
delete medians fail too; all are retained in the machine-readable matrix.
No inconvenient samples were discarded.

Timing is variable: for example Mac 100K scattered E4 insertions take
318.58, 260.95 and 157.71 ms. The reporting tool records every sample and flags
repeated growth when even the lowest larger-population time exceeds the
highest smaller-population time. It also reports ratios and descriptive
exponents. No post-result tolerance was added to redefine Law 2.

## Disk size and why one case fails

These are final **logical bytes divided by 1,000,000**, including supporting
files, after the three mutations and final reopen. Mac and Pi logical sizes
match. Allocated bytes and individual phase sizes are in the raw results.
These measurements are at phase boundaries: **no peak or 2× cap claim**.

| Population | Locality | E4 MB | SQLite MB | E4 / SQLite |
|---|---|---:|---:|---:|
| 10K | Local | 3.256 | 3.281 | 0.993× |
| 100K | Local | 29.823 | 29.749 | 1.002× |
| 1M | Local | 295.465 | 294.416 | 1.004× |
| 10M | Local | 2,951.909 | 2,941.067 | 1.004× |
| 10K | Scattered | 3.834 | 3.731 | 1.027× |
| 100K | Scattered | **33.620** | **29.450** | **1.142× — FAIL** |
| 1M | Scattered | 299.266 | 294.117 | 1.018× |
| 10M | Scattered | 2,955.706 | 2,940.772 | 1.005× |

The 100K result is an occupancy problem, not retained WAL. A read-only,
CRC/bounds-verified physical inventory finds **8,143 E4 leaf pages**, including
**2,000 pages with only 6–8 records**; the common full leaves have 14. They
contain exactly 100,000 physical leaf records, with 5,828,008 unused leaf bytes.
The WAL is empty and only one page is classified Free. SQLite dbstat finds
6,667 leaf pages, almost all holding 14 records, plus 514 interior pages and
no freelist pages. SQLite's WITHOUT ROWID tree stores records in interior
pages too, so leaf counts alone are not a fair density ratio; total file bytes
above are the comparator. The auditor is `foundation_space`.

Those 2,000 half-filled E4 leaves contain 14,000 rows, which would occupy
1,000 leaves at the observed full density. Their 1,000 excess pages represent
4.096 MB, explaining almost all of the 4.170 MB total gap. This is an occupancy
diagnosis, not a promise that an in-place repair can shrink the file. Preventing
avoidable growth and reusing space must be tested without hiding a rebuild.

The earlier +0.34% result remains valid for its different 400K/12-round mixed
workload. It was never proof that every insertion pattern has that overhead.
This fixed-work case captures a different packing state and must also pass.

## What the I/O counts establish

These E4 counters are identical across Mac and Pi repetitions. They count
buffered FileIo requests, **not physical-device reads**, and include the
checkpoint's verification work. SQLite cache events are separately reported;
they exclude checkpoint VFS work and are not equivalent counters.

| Existing rows | Scattered insert read calls | Write calls | Issued write MB |
|---|---:|---:|---:|
| 10K | 3,524 | 1,869 | 7.685 |
| 100K | 9,274 | 4,147 | 17.053 |
| 1M | 11,530 | 5,265 | 21.650 |
| 10M | 13,080 | 6,003 | 24.684 |

Across a 1,000× population increase, issued writes grow about **3.21×**, not
1,000×. This is evidence against a database-wide write pass in this workload;
it is not an asymptotic-complexity proof. Local insertion stays around
0.61–0.64 MB issued. The E4 source has a rightmost-leaf append shortcut and
bounded split/neighbor-balancing paths; scattered work loses the same locality
and touches more distinct leaves and ancestors. Checkpoint writes those
changed pages and reads them back before retiring WAL.

The physical inventory identifies underfilled split pages, but this loop does
not claim to have isolated why balancing leaves this distribution. The next
bounded storage task is to close this scattered packing/write-cost gap, using
the retained 100K and 1M cases. SQLite source references used for diagnosis:
`<scratch>`, `balance_nonroot` and
`balance_quick`; E4 counterparts are in `kernel/src/btree.rs`. No storage
algorithm was changed or optimistically promoted during this measurement loop.

## Validation, provenance and remaining qualification

Full Mac workspace: **363 distinct tests passed, 0 failed, 2 ignored**
(366 pass events including subprocess helpers). Lean: **74 passed on each
platform**. The initial Pi run failed seven schema-recovery tests solely
because their path guard admitted only `/Volumes/scratch`. Both fixture-path
branches now also admit the already-authorized Pi artifact root; the failed
log and passing retry remain. Full Mac validation includes this test fix.

The same frozen benchmark engine source was used on both platforms. The
subsequent schema test-path fix, read-only occupancy auditor, report/cleanup
tools and documentation do not change the timed engine. Source/file/log/binary
hashes, every raw report and cross-platform oracle checks are retained in
[results](FOUNDATION_SCALING_RESULTS.json) and
[validation](FOUNDATION_SCALING_VALIDATION.json).

Artifacts: `<scratch>`; Pi:
`<scratch>`.
The loop started on September 13 and finished on September 14 Melbourne time.
Verified cleanup removed **56 redundant databases**, accounting for
**28,920,004,608 allocated bytes (28.92 GB)** across Mac and Pi. It retains
24 third-repetition databases at all three smaller sizes, including failed
parity cases, along with every report, log and source. First/second repetitions
and the 10M confirmation databases were deleted only after report/oracle
validation and file hashing. Allocated bytes removed are not a promise of an
identical change in filesystem free space; see the
[cleanup manifest](FOUNDATION_SCALING_CLEANUP.json).

**L2-WORK is now FAIL, with evidence, rather than untested PENDING.** Recovery,
reader, resource and typed-integration gaps remain open. Lean tests make
further development repeatable; they do not authorize claiming the foundation
converged or switching the public collection API to this pager.
