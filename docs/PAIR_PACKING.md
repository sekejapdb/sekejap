# Bounded redistribution loop — 2026-09-14

**Later evidence:** the [buffered I/O investigation](SCAN_IO_ISOLATION.md)
reproduces stale reads independently of E4 on Mac/scratch; Pi and server controls
pass. The decision below records this loop's outcome. Runtime remains reverted
while the frozen candidate awaits full native requalification; the Mac failures
alone do not establish a packing algorithm defect.

**Rejected and reverted.** The final candidate fixes the previous 1,000-insert
WAL-capacity regression, reduces file sizes and improves repeated mixed-workload
timings, but fails a broader shuffled-write correctness test. Runtime storage
code remains identical to `dd64848` / the retained `bcd6448` engine. No SQL,
public collection switch, new index, or contract change is included.

What is retained: a lean regression for atomic scattered transaction capacity,
strict ordering checks and optional preserved fixtures in the large heap test,
matching kernel test features, benchmark tooling, and the complete evidence.

## Three policies and their outcomes

| Policy | Behavior | Outcome |
| --- | --- | --- |
| V1 | One sibling, at most three output leaves | Completes Pi transactions; shuffled density test fails: 1,974,272 bytes exceeds the existing 1,960,000-byte allowance |
| V2 | Redistribute three leaves without growth; otherwise grow two to three | Density test passes, but extra distinct-page writes remain; full shuffled-write tests return extra rows or a bad page |
| V3 | Redistribute only into existing neighboring capacity; otherwise split the full leaf into two | Transaction capacity and native page-WAL oracles pass; broader Store scan correctness fails, so rejected |

Each policy is ablated with ordinary cell framing unchanged (`pair` in raw
reports) and with two-byte-smaller ordinary inline framing (`compact-pair`).
The names are artifact labels: V3 does not grow two leaves into three.

At 100K, the 1,000-scattered-insert phase issues **17.053 MB** of data+WAL writes
in baseline E4, **25.248 MB** in compact V2, and **17.024 MB** in compact V3.
V2 adds about one distinct leaf rewrite per insertion. V3 removes that added
work. These are buffered FileIo counters, not physical device write counts.

SQLite's `balance_nonroot` considers whole cells and neighboring free space;
its index B-trees also move divider records into interior pages. A separate
10K-row SQLite 3.50.2 occupancy diagnostic finds 665 leaves with 14 cells and
280 unused bytes per leaf. That spare space can accommodate another cell in
this fixture. E4's old 272-byte cells cannot use a 248-byte remainder; compact
E4 packs 15 cells tightly. Thus initial maximum density and later insertion
cost are different concerns. This diagnostic is not the native timing engine
or a proposed fillfactor change; no additional policy was implemented from it.
Sources remain the local SQLite/PostgreSQL checkouts named in
[the previous ablation](SCATTERED_PACKING.md).

## Repeated native comparison

These are **medians of three rotated runs on each host**, with baseline E4,
compact V3 and SQLite taking turns running first. Pi is the primary acceptance
device; server is a separate server comparator. Both use 8 MiB engine caches,
4 KiB pages, native FULL durability, and a 128 MiB per-process address limit.
server's isolated job has 2 CPU / 2 GiB limits. Mac elapsed times are not used
for performance acceptance. Background services remain running; the raw
results retain every repetition because Pi elapsed times vary substantially.

The layer is **raw key/value page-WAL**, with eight-byte keys and 256-byte
values, without schemas, collection mapping, vectors or secondary indexes.
The failing correctness workload below additionally exercises the existing
Store through the shared B-tree. These are distinct layers.

### Load only: 400K rows

No updates or deletes have happened in this table. Times include load and its
ending checkpoint; sizes include managed logical files. MB means 1,000,000 bytes.

| Engine | Pi load time | server load time | Loaded size |
| --- | ---: | ---: | ---: |
| Retained E4 | 9.435 s | 4.147 s | 118.075 MB |
| Rejected compact V3 | 8.768 s | 5.033 s | 110.203 MB |
| SQLite | 8.935 s | 4.123 s | 117.670 MB |

### Mixed insert, update and delete

Start with 400,000 rows, then run 12 rounds. Each round updates 80,000 rows,
deletes 40,000, and inserts 40,000 replacements: **1,920,000 mutations total**,
in 1,000-mutation transactions. Times exclude initial load and verification;
they include commits and each round's ending checkpoint. All arms end with
exactly 400,000 independently verified rows.

| Engine | Pi mutation time | server mutation time | Final logical size | Largest sampled logical size |
| --- | ---: | ---: | ---: | ---: |
| Retained E4 | 105.241 s | 56.071 s | 129.888 MB | 134.854 MB |
| Rejected compact V3 | 87.386 s | 50.363 s | 121.229 MB | 125.886 MB |
| SQLite | 112.014 s | 44.814 s | 129.446 MB | 134.258 MB |

Compact V3 is **17.0% faster on Pi / 10.2% faster on server** than retained E4
in these medians, and saves **6.67%** final E4 space. Its time is **0.780× /
1.124× SQLite** respectively. Those gains do not override an incorrect scan.
The largest sampled footprints are about 1.142× loaded size for E4 and 1.141×
for SQLite. Sampling is every 1 ms: these are lower bounds, not hard-cap proof.

### Fixed work as the stored population grows

Each phase performs 1,000 operations in one transaction, including commit and
checkpoint, after reopening with an empty engine cache. OS caches are retained.
The table shows the scattered **insertion** phase; all update/delete timings,
10K and local cases are in the [raw results](PAIR_PACKING_RESULTS.json).

| Host / stored rows | Retained E4 | Rejected compact V3 | SQLite |
| --- | ---: | ---: | ---: |
| Pi / 100K | 1.257 s | 0.725 s | 0.995 s |
| Pi / 1M | 1.849 s | 0.672 s | 0.588 s |
| server / 100K | 0.200 s | 0.221 s | 0.076 s |
| server / 1M | 0.336 s | 0.280 s | 0.153 s |

After all three phases, 100K final size is **33.620 MB retained E4 / 31.654 MB
compact V3 / 29.450 MB SQLite**. The candidate changes the size difference
from +14.16% to +7.48%, passing that particular 10% size gate. At 1M the sizes
are **299.266 / 279.589 / 294.117 MB**. server scattered insertion still exceeds
the 1.5× SQLite time target. Strict Law 2 is not cleared by these measurements.

### Resize and reader caps

The 1K-row / six-round resize fixture ends at **3.289 MB for both E4 arms /
1.868 MB SQLite**, still 1.761× SQLite size. Pi mutation medians are **0.363 /
0.342 / 0.273 s** for baseline/candidate/SQLite; server is **0.225 / 0.208 /
0.102 s**. The short resize timings are less stable than the sustained mixed
case. Sampled logical peaks are **6.600 MB E4 / 3.714 MB SQLite**.

Both platforms complete their nine reader-cap arms with exact state checks.
The cap workload separately enforces E4's managed allowance of twice loaded
size. A held reader still causes safe refusal after 9,000 committed updates;
SQLite is stopped by the harness after crossing the comparison threshold.
The SQLite harness stop is not an equivalent engine-enforced cap. Complete
none/held/rolling results and per-operation footprint measurements are retained.

## Why the storage change is rejected

The existing heap test inserts 200K, then 800K deterministic permuted eight-byte
keys with 200-byte values into separate Stores. It uses a 16 MiB cache and
SyncMode::Off, commits each population, then scans it. Its correctness assertions
are independent of the timing benchmark and cannot be waived as timing noise.

- V2's full suite returned **200,003 rows for 200,000 inserts**. A frozen-source
  focused reproduction later refused a write with bad page magic at page 31,216;
  its fixtures are preserved. That reproduction used test-support off.
- V3 initially passed the heap test, but a later full suite returned **799,999
  rows for 800,000 inserts**. That temporary fixture was removed by the original
  test's TempDir cleanup; its source and log remain.
- Three focused V3 checks passed with test-support off. Cargo fingerprint
  inspection showed the full workspace also enables `kernel/test-support`.
  Those passes were therefore not matching-feature reproductions.
- With matching features and the strengthened oracle, frozen V3 returned
  **800,000 rows with two ordering violations**. That failed database is
  preserved at `v3-law1-full-features/rows-800000` under the local artifact root.
- Reopening a copy returned an ordered 800,000-row scan, correct values, and
  **zero incorrect results across all 800,000 expected point lookups**. Source
  hashes remained unchanged. **The copy's data file changed during reopening**,
  so this is successful recovery, not proof of a purely memory-resident defect.

The root cause remains unresolved. The test-support feature is a reproduction
condition, not an established cause. The existing Store already has an open
CONTROL-SCAN blocker; these observations must not be asserted to share its
previous stale-page cause without tracing the new evidence.

All runtime candidate changes, compact-format fixture adaptations and candidate
format tests are reverted. The stronger ordering oracle is retained because
the matched reproduction shows row count alone can pass while ordering fails.
`E4_LAW1_ARTIFACTS` now allows explicitly preserving large fixtures under the
authorized artifact roots. A new lean test protects a 1,000-scattered-insert
transaction at 100K, its old reader, and exact reopen. Lean kernel tests now
explicitly enable test-support to match the full workspace.

## Validation, preservation and scope

The native ablation has **40 probe arms** (Pi V1/V2/V3; server V2/V3), followed
by **162 qualification arms**, all with matching successful state oracles.
This does not qualify the broader Store correctness failures above.

The restored Mac workspace passes **367 test-result entries, zero failures,
two ignored**, including child-process output. Native baseline lean and
matching-feature kernel results are recorded in
[validation](PAIR_PACKING_VALIDATION.json). Artifact roots:

- Mac: `<scratch>`
- Pi: `<scratch>`
- server: `<scratch>`

Each policy has frozen source archives and distinct native binaries. V3 reuses
dependency compilation in separate target copies, explicitly cleans both
workspace packages, and requires both to recompile before benchmarking. No
shared-target candidate reuse is counted. Evidence archives precede deletion
of verified redundant databases; see [cleanup](PAIR_PACKING_CLEANUP.json).
Failed Mac fixtures, source snapshots and healthy controls remain available.
Cleanup reclaimed **24,008,060,928 allocated bytes (24.01 GB)** across the
three hosts. Each native baseline passed 75 lean tests and four paired smoke
arms, followed by 16 matching-feature kernel test entries. The release checker
still correctly refuses promotion with 35 outstanding gate entries.

The next prerequisite is to trace the committed Store scan/reopen discrepancy
using the preserved 800K case and matching features. Further packing gains
remain parked until that correctness gap is understood. The seven laws remain
seven; timestamps remain off by default; foundation promotion remains blocked.
