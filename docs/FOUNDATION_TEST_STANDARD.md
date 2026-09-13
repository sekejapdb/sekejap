# Foundation test standard F1 — 2026-09-13

This is an executable-evidence contract, not an eighth law. CONTRACT.md keeps
exactly seven laws. A fast prototype is not a seven-law-qualified engine.
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

N ladder: 1K diagnostic; 10K and 40K smoke; 100K and 400K acceptance; 10M
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

## Seven-law acceptance categories

| Law | Category | Required falsifiable evidence |
|---|---|---|
| 1 Disk-first | L1-MEM | Fixed cache plus bounded WAL lookup, transaction state, free-page state and reader state; hard process budget on Pi; scaling beyond RAM; no database-sized in-memory oracle |
| 2 Work proportional to change | L2-WORK | Same changed rows across N ladder, page reads/writes and metadata touched; bounded lookup/checkpoint debt; no whole-database scan at ordinary commit |
| 3 Nothing fallible may delete | L3-ATOMIC | Kill/error at append, commit barrier, checkpoint copy, verification and log reset; acknowledged state exact; failed repair leaves source hash unchanged; uncertain commit is explicit |
| 4 Name costs | L4-COST | All timing/space/RAM categories, side files, temporary files, retained versions, actual sync settings, cache limits and unsupported semantics |
| 5 Recoverability | L5-DAMAGE | CRC/identity/bounds on every unit; torn versus corrupt WAL; corrupt root/leaf/overflow/schema/free metadata; independent salvage with no stale resurrection; all-replica loss explicitly reported |
| 6 Readers | L6-READ | New readers see latest acknowledged commit; old readers remain byte-stable; concurrent writer/checkpoint; no writer lock on reads; coordination and latency/I/O effects explicitly audited, including processes |
| 7 Target usability | L7-DEVICE | Mac + authorized Pi load/live-write/reopen evidence; bulk import, late-indexing and actual indexed live-write gates remain PENDING until those features exist |

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

Current E4, experimental E4 and native SQLite are named separately in every
table. One representation or backend cannot be substituted silently. Primitive
page-WAL proof does not authorize SQL, graph or multimodel-index implementation.
All applicable gates, full integration tests and Pi checks must pass before
promotion. Record unimplemented categories as PENDING, not passing by omission.
Preserve source archives, binaries, raw reports, logs, fault evidence and one
representative comparison set. Delete only verified disposable generated data
after recording its manifest. tracker owns live status.
