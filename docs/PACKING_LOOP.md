# Delete/page-packing loop — 2026-09-12

Ordinary point deletes now merge sparse B-tree neighbors and collapse empty
interior levels. The complete collection workload stops accumulating empty
entity/vector leaves. At 400K rows, twelve mixed cycles retain **139.769 MiB**,
down from **258.111 MiB** with the previous kernel. No rebuild, vacuum, ID reuse,
format change or durability relaxation was used.

This closes the measured empty-page growth defect. It does **not** close full
SQLite parity: E4 still takes about **3.10×** SQLite's mixed-churn time and stores
about **19.3%** more after churn. Snapshot expansion can still exceed 2×, and
the configured disk-cap/admission gate remains open. SQL is separate work.

## Paired results

MiB means 1,048,576 bytes. These are complete collection API measurements:
external keys, non-reused committed IDs, typed hybrid rows and separate vectors.
Earlier entry-only results around 3–4% overhead are a different workload.

| 400K, timestamps OFF, no pinned reader | Old E4, same harness | E4 packing, mean of two runs | SQLite, mean of two runs |
|---|---:|---:|---:|
| Load seconds | 22.866 | 22.549 | 5.791 |
| Loaded MiB | 126.684 | 126.684 | 93.266 |
| Twelve-cycle churn seconds | 113.634 | 122.774 | 39.547 |
| Sampled logical peak MiB | 258.117 | 140.027 | 121.840 |
| Final churn MiB | 258.111 | 139.769 | 117.129 |
| Logical peak / loaded size | 2.037× | 1.105× | 1.306× |

Packing reduces the old-kernel peak by **45.8%**, at an **8.0%** churn-time cost
in this comparison. The old-kernel control is one additional run; candidate and
SQLite have two runs in opposite engine orders. This is measured on one Mac,
not a statistical confidence interval. At 100K, candidate peak is 35.634 MiB
versus old E4 64.651 and SQLite 33.961 MiB.

The requested workload separation is retained below; each row is one matched
400K pair, four cycles except load-only. Peak includes reader release and
schema alteration as well as load/churn. No-reader cases use timestamps OFF
unless specified.

| Case | E4 churn s | SQLite churn s | E4 peak MiB | SQLite peak MiB | E4 final churn MiB | SQLite final churn MiB |
|---|---:|---:|---:|---:|---:|---:|
| Load only | — | — | 126.935 | 97.423 | 126.684 | 93.266 |
| Updates | 18.516 | 7.652 | 129.689 | 113.810 | 129.430 | 108.891 |
| Delete + fresh-ID reinsertion | 18.870 | 5.726 | 140.481 | 106.675 | 140.469 | 101.500 |
| Mixed, held snapshot | 38.619 | 35.932 | 267.249 | 605.492 | 266.622 | 581.628 |
| Mixed, rolling snapshot | 37.276 | 32.427 | 276.570 | 605.492 | 276.162 | 605.492 |
| Mixed, timestamps ON | 37.461 | 12.944 | 149.951 | 125.945 | 149.696 | 121.254 |

The full [Mac tables](PACKING_TABLES.md) include both 100K/400K, repetitions,
all phase times, logical and allocated peaks, and release sizes. Raw reports and
the independent pair audit are in [PACKING_RESULTS.json](PACKING_RESULTS.json).
The 34 arms include 32 candidate/SQLite arms and two old-kernel controls:
**164 matching state comparisons**, **83.5 million exact-oracle row visits**.
The engine harness checks expected fields and ordered IDs before calculating
the cross-engine CRC, which includes each exact ID; the audit also checks every old snapshot against its expected
earlier generation. Monitor errors were zero.

## Raspberry Pi counterpart

The Pi 5 runs the same six arms: 100K/400K twelve-cycle mixed churn, then
100K four-cycle held-reader churn, each paired with SQLite. All complete
under `prlimit --as=134217728`: **38 matching state comparisons** and
**17.2 million row visits**. This is one run per arm on the shared Pi, with
existing sensor and LLM services left running; no system services were stopped
and no packages or boot settings changed. Start/end load averages were
0.30/1.10 for the one-minute measure. These are validation timings, not an
isolated-device speed guarantee.

| Pi, 400K, twelve mixed cycles | E4 | SQLite |
|---|---:|---:|
| Load s | 27.396 | 8.891 |
| Churn s | 177.675 | 77.214 |
| Logical peak MiB | 140.027 | 121.840 |
| Final churn MiB | 139.769 | 117.129 |
| Peak allocated MiB | 140.035 | 121.852 |
| Maximum child RSS MiB | 10.766 | 11.625 |

The space plateau reproduces on aarch64; churn is **2.30×** SQLite's time.
Held-reader 100K child RSS reaches 14.078 MiB for E4 and 25.625 for SQLite.
The 128 MiB limit constrains virtual address space, not filesystem cache or
other processes; these benchmark arms do not enable persistent database disk
quotas. Linux is 6.18.34+rpt-rpi-2712, Rust 1.96.0 and SQLite 3.46.0.
See [every Pi phase and process measurement](PACKING_PI_TABLES.md) and
[audited Pi JSON](PACKING_PI_RESULTS.json). The first launch found no
`/usr/bin/time`; a Python child-resource recorder replaced it before any timed
arm ran. No system package installation was needed.

## Why space grew and what changed

Inspection of the retained old 400K fixture found 26,612 empty entity leaves
and 3,605 empty vector leaves attached to the current tree. Fresh IDs append
elsewhere after deletion, so leaving those leaves reachable prevents their
physical pages from becoming reusable. This was current-tree occupancy growth,
not mainly a large freelist or unreclaimed old snapshots.

| Published tree after twelve cycles | Previous kernel | Packing |
|---|---:|---:|
| Records, including catalog/key/vector entries | 1,200,029 | 1,200,029 |
| Reachable pages | 65,590 | 35,116 |
| Nonreachable, non-meta pages | 483 | 661 |
| Empty entity/vector leaves | 30,217 | 0 |
| Entity leaf occupancy | 34.91% | 68.95% |
| Vector leaf occupancy | 44.91% | 90.20% |
| External-key leaf occupancy | 74.90% | 74.90% |

The new final tree has no empty leaves, including mixed boundary leaves.
Occupancy measures live cell bytes plus slots against usable leaf bytes;
overflow payload pages are outside that percentage. An empty leaf is attributed
to a keyspace only when its lower and upper fences agree. Independent structural
verification precedes the occupancy walk.

At 400K, final logical sizes for cycles 7–12 range from 146,557,220 to
146,558,876 bytes: **1,656 bytes** of variation. This demonstrates a plateau
over these measured cycles, not a guarantee for every payload or workload.

Point-delete maintenance starts below one-third page occupancy. It tries either
adjacent sibling for a merge, then redistribution if a merge cannot fit. Leaf
records and interior separators are packed by bytes. It walks upward after a
merge and collapses an interior root with no separators. Local scratch holds
two child images and a parent, with bounded vectors derived from those pages;
it never loads an entire collection to compact it.

Frozen siblings are copied before rewriting. A merged-away frozen page is
retired with its verified birth generation; a writable page retired before
publication uses the current write generation. Existing reader lifetimes keep
old snapshots safe. Replacement images, retirement reservation and writable
guards are prepared before installing images; the Store refuses publication
after an error. Committed entity IDs remain monotonic and unreused.

A variable-key oracle also exposed an existing insertion failure: a large
indivisible middle row can need three leaves even when total bytes are below
two-page capacity. The bounded three-leaf fallback now handles that case and
reacquires the second separator's parent path after installing the first.

The tradeoffs are extra sibling reads, copying and separator maintenance on
deletion. A redistribution whose longer fence cannot fit its parent is declined;
it is not a universal minimum-occupancy guarantee. This loop covers ordinary
point deletion used by collection CRUD, not a redesign of bulk `delete_prefix`.
Already-empty historical pages are not swept globally. File lengths do not
automatically shrink; reusable pages serve future writes. Page/row/key formats,
timestamp policy (OFF by default, collection opt-in), and the seven laws are
unchanged.

## Expansion is still a separate resource policy

For the repeated no-reader 400K case, E4's measured logical factor is 1.105×;
allocated peak reaches 144.652 MiB and a 1.127× factor in the second run.
SQLite's corresponding factors are 1.306× logical and 1.381× allocated.
Factors use each arm's own loaded size, not the smaller engine's baseline.

Held/rolling snapshots pin older generations. At 400K, E4 reaches 2.110× /
2.183× logical size; SQLite reaches 6.492× including release/checkpoint. The
100K E4 held case reaches 2.591× **allocated** space. These cases therefore
fail a strict 2× objective despite comparing favorably with SQLite. SQLite
checkpoint busy/frame counts are retained in JSON; the held WAL cannot be
truncated while its reader needs it. Releasing the reader returns SQLite to
118.066 MiB logical, while E4 retains 266.622 MiB available for later reuse.

Every peak is a 1 ms sampled **lower bound**, not an enforced cap. A genuine
50 MiB → 100 MiB maximum needs write admission/reservation, a defined response
when readers pin too much history, and failure tests at that boundary. This
matrix did not enable the existing `create_limited` logical quotas. Those
quotas already reject managed logical growth, but are not filesystem
reservations or evidence that these workloads finish within a 2× allowance.
A paired constrained-collection gate still needs to establish completion or
safe refusal under that allowance. E4's current commit/checkpoint behavior
also differs from SQLite's native automatic WAL checkpoints; the comparison
preserves those behaviors and measures their cost.

## Validation and reproduction

Red tests first showed retained empty pages, failure to collapse the root,
16-cycle physical growth from 138 to 2,321 pages, and the large-middle-row
insertion error. New coverage checks sparse merges with old readers, complete
deletion of a multi-level tree with overflow, long churn, a 6,000-operation
independent variable-key model with forward/reverse scans and reopen, and the
three-leaf insertion case.

Failure coverage includes 12 injected data-page write positions during forced
merges; each refuses subsequent publication and reopens the last published
412-row state. Three child-process SIGKILL stages cover before publication,
after publication and later uncommitted merges. A damaged adjacent leaf causes
merge failure without publishing partial changes, preserving the healthy leaf
in an old snapshot and after reopen. These are specified fault models, not
proof of every possible power loss or multi-page corruption. Existing recovery,
overflow, resource and version-reuse tests remain part of workspace validation.

The final Mac workspace run passes **339 tests**, zero failed or ignored.
All eight delete-packing tests also pass with default features, separately from
the benchmark configuration (`sqlite-balance,compact-cells`).
Pi validation passes **158 tests**: 128 kernel unit tests, eight resource-limit
tests, seven collection unit tests, seven collection integration tests and
eight delete-packing tests. The delete-packing count includes the child-process
harness; its parent exercises three actual SIGKILL scenarios.

Mac artifacts: `<scratch>/`.
The original collection baseline fixture is preserved in the sibling
`collections-20260911/sustained/` directory. The candidate and old kernel use
the same extended benchmark harness; the old kernel comes from the prior
collection source archive, SHA-256
`c6f2161845ca85c84ad446e3c358cd2c993d24ab4090dd36494c2208dae57c13`.

Host: Apple M3 Pro, 18 GiB, macOS 26.5.1; Rust 1.96.0; bundled SQLite 3.46.0.
Both arms use buffered I/O, FULL sync with platform-native barriers, 8 MiB
database cache, 1,000 public mutations per transaction and full row verification
outside mutation timing. SQLite uses a WITHOUT ROWID composite primary key,
unique external key, JSONB and an inline four-f32 BLOB. E4 keeps the vector
keyspace. Two collections receive names, nullable numeric/boolean fields,
points, nested binary JSON, undeclared u64 extras and four exact vector lanes.
One percent of applicable updates alternate short/long notes. Updates touch
20% of rows; reinsertion deletes and recreates 10%; mixed combines both for
40% public operations per cycle. Timestamp arms use the same fixed clock.

```sh
TMPDIR=<scratch> \
SQLITE_TMPDIR=<scratch> \
cargo test --workspace --release --features sqlite-balance,compact-cells -- --test-threads=1
cargo build --release --features sqlite-balance,compact-cells --bin collections --bin collection_inspect
```

`tools/run_packing_matrix.sh` expects isolated candidate/baseline binaries at
the artifact root and refuses an existing matrix directory. Build the old
source in a separate checkout with the current `src/bin/collections.rs`, copy
both binaries, then run the script with no competing benchmark/build load.
`python3 tools/record_packing.py` audits and regenerates the Mac tables.
`tools/packing_pi.sh` documents the isolated Pi validation and 128 MiB
address-space benchmark; `tools/packing_time.py` records child RSS and CPU
usage without installing system packages.

The Pi-tested source matches the local kernel, collection API, benchmark,
inspector and packing tests by SHA-256. Source and binary provenance are
recorded in `PACKING_PROVENANCE.json`; archived source, binaries, raw logs,
occupancy reports and red-test evidence remain on scratch.

After auditing, 34 disposable Mac databases were removed: **3,045,717,024
logical bytes / 3,117,449,216 allocated bytes**. Four Pi databases were removed:
**167,547,528 logical bytes / 167,567,360 allocated bytes**. Reports and fault
fixtures remain. The new `matrix/mixed-400000-r1` Mac pair and Pi
`matrix/mixed-400000` pair remain for profiling, as does the original previous
loop's baseline pair. See [Mac cleanup](PACKING_CLEANUP.json) and
[Pi cleanup](PACKING_PI_CLEANUP.json). Report generation still works after
cleanup because JSON evidence is stored outside the removed database folders.

Next full-API work should profile repeated vector deletion/reinsertion,
external-key lookups and publication overhead. Stable packing is useful
progress; write-speed parity, strict expansion admission, comprehensive
membership recovery and the remaining interface/index gates remain open.
