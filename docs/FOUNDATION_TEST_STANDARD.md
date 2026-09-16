# Foundation test standard F1 — 2026-09-13

This is an executable-evidence contract for the eight laws in CONTRACT.md.
The owner added Law 8 (release compatibility) on 2026-09-15; the test standard
itself is not an additional law. A fast prototype is not a law-qualified engine.
Each category is reported as PASS, FAIL, PENDING or NOT APPLICABLE, with the
exact tested scope. Pending safety/device/interface gates prevent promotion.

## Fixed fixture families

| ID | Logical content | Purpose |
|---|---|---|
| F-T32 | 32-byte deterministic text-like value, ordered 8-byte key | Small record overhead |
| F-T256 | 256-byte value, same key | Ordinary text payload and page occupancy |
| F-T8192 | 8192-byte value | Overflow chains and large-value containment |
| F-TVAR | 64, 256, 2048, 8192, 256, 64 bytes over successive versions | Growth/shrink and overflow transitions |
| F-H4 | Existing two-collection people fixture: external key, Unicode name, bool/null/number, binary JSON, point, 4 exact f32 lanes | Complete collection parity |
| F-H1536 | F-H4 with 1536 exact f32 lanes, unchanged and changed vector cases | Large-vector storage without ANN indexing |
| F-SCHEMA | Multiple immutable layouts, extra/missing/null fields, timestamps off/on | Typed semantics and recovery dependencies |

F-T fixtures are **raw key/value storage primitives**. Their values contain
deterministic slot/version bytes and printable text; they do not exercise the
full typed collection API. All engines receive identical key/value bytes.
SQLite uses a WITHOUT ROWID primary-key table with a BLOB value. F-H fixtures
retain the native SQLite columns/JSONB/unique-key comparator from collections.
Never mix the two sets of ratios or claim primitive results as collection wins.

N ladder: 1K diagnostic; 10K and 40K smoke; 100K and 400K acceptance; 1M scaling; 10M
large-data confirmation after smaller gates pass. Overflow fixtures use a
separate declared N (1K/10K/40K) so dataset size and memory pressure are clear.
Ordered and deterministically shuffled keys are separate arms. Seed, payload
generator version and source/binary hashes accompany every report.

## Workload categories

| ID | Work performed | Required outputs |
|---|---|---|
| W-LOAD | Fresh load only | Load seconds, rows/s, bytes/row, end size, peak size, reopen time |
| W-UPDATE | Update 20% per round; fixed-size records | Update seconds, unchanged/changed-row oracle, issued bytes |
| W-RESIZE | Same updates with F-TVAR | Growth/shrink time, overflow reuse, plateau |
| W-DELETE | Delete 10% per round without replacement | Delete seconds, exact absence, remaining count, retained size |
| W-REINSERT | Delete 10%, insert equal count under fresh IDs | Delete and insert seconds, no committed-ID reuse |
| W-MIXED | Per round: update 20%, delete 10%, insert 10% | Separate operation totals plus total including commits/checkpoints |
| W-SUSTAIN | W-MIXED for 12 rounds | Every round's size/time; cycles 10–12 plateau and peak |
| W-READERS | No reader, held initial reader, rolling reader | Exact snapshot answers, reader latency/I/O and writer time |
| W-CAP | Explicit total managed-file cap under sustained churn | Admission refusal before cap, old state readable, reopen exact |
| W-REPAIR | Seeded damage and interrupted repair | Source preservation, exact survivors/loss classes, repair peak |

At N=100K, four mixed rounds mean **80K updates + 40K deletes + 40K inserts**.
At N=400K, twelve rounds mean **960K updates + 480K deletes + 480K inserts**.
Surviving live population remains N. Zero-cycle W-LOAD stays separate.
Transactions: 1, 100 and 1000 operations as distinct cases; no changes to batch
size or durability within an A/B comparison. Report final checkpoint time and
include it in total job time; never hide maintenance after stopping the timer.

## Eight-law acceptance categories

| Law | Category | Required falsifiable evidence |
|---|---|---|
| 1 Disk-first | L1-MEM | Fixed cache plus bounded WAL lookup, transaction state, free-page state and reader state; hard process budget on Pi; scaling beyond RAM; no database-sized in-memory oracle |
| 2 Work proportional to change | L2-WORK | Same changed rows across N ladder, page reads/writes and metadata touched; bounded lookup/checkpoint debt; no whole-database scan at ordinary commit |
| 3 Nothing fallible may delete | L3-ATOMIC | Kill/error at append, commit barrier, checkpoint copy, verification and log reset; acknowledged state exact; failed repair leaves source hash unchanged; uncertain commit is explicit |
| 4 Name costs | L4-COST | All timing/space/RAM categories, side files, temporary files, retained versions, actual sync settings, cache limits and unsupported semantics |
| 5 Recoverability | L5-DAMAGE | CRC/identity/bounds on every unit; torn versus corrupt WAL; corrupt root/leaf/overflow/schema/free metadata; independent salvage with no stale resurrection; all-replica loss explicitly reported |
| 6 Readers | L6-READ | New readers see latest acknowledged commit; old readers remain byte-stable; concurrent writer/checkpoint; no writer lock on reads; coordination and latency/I/O effects explicitly audited, including processes |
| 7 Target usability | L7-DEVICE | Mac + authorized Pi load/live-write/reopen evidence; bulk import, late-indexing and actual indexed live-write gates remain PENDING until those features exist |
| 8 Release compatibility | L8-COMPAT | Immutable fixtures from released binaries: newer read/write/reopen and committed-WAL recovery; exact typed, schema, relationship and index-query results; minor rollback with unchanged features; unknown-format refusal leaves source files unchanged; no mandatory migration/index rebuild on update |

L8 fixtures must identify their writer release, enabled persistent features,
source/binary/file hashes and independent expected results. Cover each released
encoding and index family, both checkpointed and committed-WAL states. Preserve
original fixtures and mutate only working copies. Add public API/query/wire
compatibility cases as interfaces return. The current negative format probes
identify blockers; they are not a passing cross-release fixture suite. Until a
stable baseline and this suite exist, L8 remains PENDING.

Correctness uses an independent deterministic operation history and streams
expected records. Count alone is insufficient: compare every key/value, exact
deletes and fresh IDs, plus aggregate checksums and reopen. Fault tests start
from independent copies and preserve damaged originals. Never label a refusal
to open as successful recovery. Never label process-kill tests as power-cut tests.

## Parity and space criteria

Project targets (not laws), updated by the owner on 2026-09-13:
**<1.50x native SQLite elapsed time** is acceptable for release. Final logical
size retains its **<=1.10x SQLite** target; the 2x expansion cap and all seven
laws are unchanged. The earlier <=1.10x time target remains historical evidence,
not a reason to reject a workload now below the owner's accepted threshold.
Apply these targets to each comparable workload, with no hidden maintenance.
Report raw seconds/bytes and the ratio; acceptable performance does not mean
identical speed. Repeat acceptance arms three times in rotated engine order;
report all samples and medians. Two-run/one-run probes are diagnostic only.

Report data, WAL, allocator/reader files and temporary files together: logical
and allocated size after load, every mutation round, final checkpoint and
reopen; continuous sampled peaks; issued bytes; peak/loaded expansion factor.
File-system samples are lower bounds. An enforced cap needs synchronous
admission checks and fault tests, not merely a sampler.

The bounded expansion target is an explicit cap **2x the loaded footprint**
for stable-population churn. Declare the absolute bytes before mutation starts.
Small-fixture startup overhead must be reported separately, never concealed by
silently increasing this multiplier. Exceeding the cap must refuse safely;
completing the workload under the cap is a separate usability gate. Report
SQLite's observed peak beside E4 even if SQLite has no equivalent total-file cap.
Final plateau: cycles 10–12 must not show ongoing file growth beyond a declared
one-page/metadata rounding allowance. Rebuild/VACUUM is a separate measured arm.

## Promotion and evidence retention

### Repeatable lean groups and fixed-work scaling

`docs/FOUNDATION_LEAN_GROUPS.json` lists all eight laws, maps implemented
coverage to shared executable test groups, and explicitly lists missing
coverage. L8 currently has no executable compatibility group and remains
PENDING; an empty command list is not a pass. Each shared command runs
once. This is fast regression feedback, not a claim that all laws pass.

Run from the project root, using a fresh artifact directory each time:

```sh
python3 tools/run_foundation.py lean <scratch>
python3 tools/run_foundation.py scale <scratch>
python3 tools/run_foundation.py large <scratch>
```

`lean` runs pager/fault/repair, codec/schema/collection and inherited kernel
regressions, then four 1K-row E4/SQLite smoke arms. `scale` performs three
rotated repetitions at 10K, 100K and 1M rows; `large` is a one-run 10M
confirmation, not three-repetition acceptance. Both are runnable in the
authorized Pi artifact area; Pi benchmark processes have a 128 MiB
address-space limit. Source/log/binary hashes and structured reports accompany
each run. Existing typed tests still use the older collection Store.

The scaling workload holds work fixed: **1,000 inserts, then 1,000 updates,
then 1,000 deletes**, each as a separate transaction, against each population.
All rows have 8-byte keys and 256-byte values, 8 MiB engine caches, native FULL
durability, and explicit ending checkpoints included in each phase's time.
The base population is loaded in ascending even keys. Local arms append new
keys and update/delete contiguous existing ranges. Scattered arms insert into
distributed odd-key gaps and update/delete disjoint evenly spaced ranges in a
deterministically permuted order. Each locality has a fresh database. Engine
caches start empty at each phase; OS caches are not flushed. Reopen is priced
separately. Every changed key is checked, and final reopen streams an exact
value/order/membership oracle using O(changes) state.

Report absolute times, largest/smallest ratios, adjacent ratios and the
descriptive exponent `log(cost ratio)/log(population ratio)` for time and E4
issued reads/writes. E4 FileIo calls are buffered requests, not physical media
I/O. SQLite cache events exclude checkpoint VFS work and must not be presented
as equivalent counters. Phase-boundary sizes are not peak-space evidence.
A passing workload oracle is distinct from passing Law 2. Measured latency
growth cannot be hidden by SQLite parity or by defining an acceptable exponent
after seeing the results. The strict law remains unchanged; unresolved growth
must remain an explicit qualification failure or pending investigation.

Current E4, experimental E4 and native SQLite are named separately in every
table. One representation or backend cannot be substituted silently. Primitive
page-WAL proof does not authorize SQL, graph or multimodel-index implementation.
All applicable gates, full integration tests and Pi checks must pass before
promotion. Record unimplemented categories as PENDING, not passing by omission.
Preserve source archives, binaries, raw reports, logs, fault evidence and one
representative comparison set. Delete only verified disposable generated data
after recording its manifest. tracker owns live status.
