# Scattered packing ablation — 2026-09-14

**Rejected and reverted.** Compact ordinary cells, whole-cell neighbor planning,
and their combination all refuse the 1,000 scattered-insert transaction that
baseline E4 and SQLite complete. This reproduces on both Raspberry Pi and server
server, at both 100K and 1M stored rows. The failure is the existing 16 MiB WAL
limit, not disk exhaustion or damaged committed data. The storage implementation
is restored to `bcd6448`; no SQL, public collection switch, or new index is added.
Exactly seven laws remain unchanged, and timestamps remain off by default.

The [machine-readable results](SCATTERED_PACKING_RESULTS.json) contain every
valid benchmark report and all twelve refusal audits. Timing numbers below are
**one characterization per arm**, not repeated performance qualification. We
stopped acceptance testing after the deterministic transaction-capacity regression.
The planned three-rotation qualification was not run or claimed.

## What was compared

| Arm | Change from `bcd6448` |
| --- | --- |
| Baseline E4 | Retained page-WAL implementation; existing compact integer-key format |
| Compact | Ordinary inline cells omit a redundant two-byte value length; slot bounds give the end |
| Packing | Neighbor planner counts indivisible cells before selecting page boundaries |
| Combined | Both changes |
| SQLite | Native SQLite through the same harness and generated records |

Native aarch64 builds run on Pi `contributor@example.invalid`; native x86_64 builds run
in the isolated server job on `server`, limited to 2 CPUs / 2 GiB. Each timed
process additionally has a 128 MiB address-space limit. Both engines use an
8 MiB cache, 4 KiB pages, native FULL durability, and database files on the same
host filesystem. Mac timings are excluded from acceptance because it is shared
with other development. Absolute times are compared only within each host.

This is **raw key/value storage**, not a full people collection or multimodel
index benchmark. Fixed-work keys are eight-byte big-endian integers with
256-byte values. Load uses transactions of 1,000 rows. Each later phase is one
transaction of 1,000 inserts, updates, or deletes, followed by checkpoint;
each phase starts with an empty engine cache, with OS caches retained.
Successful arms check exact values, membership, ordering and final row count.

## Load only, before update or deletion

Sizes are decimal MB and include managed files. These are loaded sizes, not
final sizes for failed transactions. Both hosts produce the same logical sizes.

| Stored rows | Baseline / packing E4 | Compact / combined E4 | SQLite |
| --- | ---: | ---: | ---: |
| 100K | 29.524 MB | 27.558 MB | 29.450 MB |
| 1M | 295.170 MB | 275.493 MB | 294.117 MB |

Two bytes per cell cross a page-capacity threshold in this fixture: a
256-byte value plus eight-byte key and framing occupies 272 bytes including
its slot, versus 270 bytes in the compact candidate. A 4,056-byte usable leaf
therefore holds 14 versus 15 records. This explains why the file reduction
is about 6.67%, larger than the percentage of bytes removed from each record.
This benefit is payload-dependent and is not a universal density claim.

## The decisive failed transaction

| Host | Stored rows | Baseline E4: insert 1,000 | Compact | Packing | Combined | SQLite: insert 1,000 |
| --- | ---: | ---: | --- | --- | --- | ---: |
| Pi | 100K | 0.477 s | Refused | Refused | Refused | 0.313 s |
| Pi | 1M | 0.849 s | Refused | Refused | Refused | 0.318 s |
| server | 100K | 0.276 s | Refused | Refused | Refused | 0.788 s |
| server | 1M | 0.337 s | Refused | Refused | Refused | 0.113 s |

Complete baseline/SQLite update, delete and load times are in the results file. Failed arms have
no successful mutation-time or final-density result and are not assigned a speedup.
Baseline's 100K scattered final size remains **33.620 MB versus SQLite 29.450 MB**,
or **14.16% larger**. This loop does not close the previously identified gap.

Each failed WAL contains **4,064 frames / 16,776,192 bytes**, just below the
16,777,216-byte ceiling; another 4,128-byte frame will not fit. There are zero
commit frames. The frames name **4,043–4,055 different pages**, so more than
99.4% of these frames are distinct page images. Rewriting duplicate WAL frames
alone would recover fewer than 22 frames here. The dominant issue is how many
different pages redistribution touches within one transaction.

Physical data+WAL allocation at refusal is **44.335 MB** for compact/combined
100K and **46.301 MB** for packing 100K. That is about **1.61× / 1.57×** their
loaded allocation, below 2×. At 1M it is about **1.061× / 1.057×**. These are
failure-boundary measurements, not continuous peak measurements. The separate
fixed WAL ceiling refuses work even though the disk has free space.

We copied each of the twelve failed databases and opened the copies with the
native engine. Every copy verified the **entire exact previously committed
100K or 1M population**, with none of the refused insertions published. Every
original source file retained its SHA-256. Original failed databases remain
available; verification copies were removed after recording the results.
This proves the tested refusal preserves committed state, not a universal
crash/corruption guarantee.

## Sustained mixed create/update/delete

Start with **400,000 records**, then perform **12 rounds**. Each round updates
80,000 records, deletes 40,000, and inserts 40,000 replacements: **1,920,000
mutations in total**. Transactions contain 1,000 mutations. Times below exclude
initial load and include commits plus each round's ending checkpoint. They
exclude verification. All arms end with exactly 400,000 verified records.

| Engine | Pi mutation time | server mutation time | Final logical size | Largest sampled logical size |
| --- | ---: | ---: | ---: | ---: |
| Baseline E4 | 75.168 s | 57.399 s | 129.888 MB | 134.854 MB |
| Compact | 73.620 s | 49.828 s | 121.229 MB | 125.886 MB |
| Packing | 74.729 s | 50.421 s | 129.888 MB | 134.854 MB |
| Combined | 72.462 s | 51.983 s | 121.229 MB | 125.886 MB |
| SQLite | 78.309 s | 36.182 s | 129.446 MB | 134.258 MB |

Baseline E4 is **0.960× SQLite time on Pi** and **1.586× on server** in these
single trials. Combined saves 3.6% versus baseline on Pi and 6.7% final space,
but its scattered transaction regression still rejects it. Packing alone
does not reduce this mixed workload's final size. The server single trial is
above the owner's 1.5× target; earlier successful Pi cases do not clear that
separate server result. These values do not supersede previous repeated medians.

Mixed-workload peak sizes are 1 ms samples and therefore lower bounds, not
hard-cap proof. Exact allocated sizes and load times are in each raw phase.
Before mutations, the 400K loaded size is **118.075 MB baseline E4 / 110.203 MB
compact or combined / 117.670 MB SQLite**. Pi load times are **6.436 / 6.292 /
6.775 seconds** for baseline, combined and SQLite; server is **5.625 / 4.503 /
4.566 seconds**. The mixed workload's largest sampled footprint is about
**1.142× its loaded footprint** for all E4 arms and **1.141× for SQLite**.

## Resize and retained readers

The 1K-row / six-round resize fixture remains a density failure: every E4 arm
ends at **3.289 MB**, versus SQLite **1.868 MB** (1.761×). Sampled logical peaks
are **6.600 MB E4 / 3.714 MB SQLite**. On Pi, mutation time is **0.420 s baseline,
0.328 s combined, 0.179 s SQLite**. These short single trials are diagnostic.
The larger logical payload after resize means comparing peak against the
original small-value load is not a fixed-live-size cap guarantee.

The explicit cap fixture uses 10K rows, 1,000-update transactions, and a hard
E4 allowance of twice the loaded managed logical footprint. All 30 native cap
arms pass their old/new-state verification. With a held reader, E4 commits
9,000 updates and safely refuses the next transaction before exceeding its cap.
Baseline peak is **5,929,408 bytes** against a **5,931,008-byte cap**; compact's
peak is **5,526,432** against **5,529,600**. SQLite commits 10,000 updates and the
harness stops after observing **6,047,176 bytes**, above its 5,971,968 comparison
threshold. SQLite does not enforce an equivalent total-file cap in this test.
Cap timing includes filesystem measurements and is not a speed gate.

## Source analysis and named tradeoff

The local SQLite checkout is `f3b9f74d81132426dee1ccc07a67fdad2ccfeaa9` at
`<scratch>`. In `src/btree.c`, `balance_nonroot`
examines whole cells across bounded neighboring pages. The rejected E4 planner
fix addresses a real feasibility error: 43 cells of 272 bytes total 11,696
bytes, less than three pages' 12,168 bytes, but three pages hold only 42 cells.
An extracted old-policy reproduction fails; an exhaustive bounded partition
oracle and the corrected four-page case pass for the candidate.

That local packing correction changes the write workload: more sibling pages
become dirty. Compact encoding also changes which existing redistribution
paths become feasible. The native WAL inventory shows that additional distinct
pages, rather than repeated copies of one page, dominate this regression.

SQLite's `src/wal.c` can reuse an uncommitted frame for a page already written
by the same transaction, and recomputes affected checksums at commit. E4 currently
appends another frame. That difference exists, but the measured duplicate count
rules it out as the primary fix for these refusals. SQLite also does not have
this pilot's fixed 16 MiB WAL ceiling.

The PostgreSQL checkout is `a11bce64a3be38f726cdca682f1e5b723cfd34d1` at
`<scratch>`. Its `nbtsplitloc.c` evaluates real tuple
split positions and distinguishes rightmost fillfactor from ordinary balancing.
That supports making free-space/write tradeoffs explicit; it does not imply
PostgreSQL's heap-plus-index storage layout should replace E4's row-in-leaf tree.

The next useful design question is **transaction capacity versus redistribution
write cost**, with these exact failed cases as the gate. Do not silently shrink
the requested transaction, increase the disk safety factor, or retain compact
encoding based only on a smaller initial file. No additional optimization was
implemented in this loop after the rejection.

## Validation and evidence

The valid run comprises **70 attempted native benchmark arms**: 58 complete
reports and 12 resource refusals, plus twelve exact-state refusal audits.
Both Pi and server pass **74 baseline lean tests and four paired smoke arms**
each. Additional correctness results are recorded in
[validation](SCATTERED_PACKING_VALIDATION.json). Restored Mac workspace:
**366 passing test-result entries, zero failures, two ignored**; child-process
tests contribute repeated events, so this is not a distinct-test count.

Candidate correctness was not fully qualified: the Mac run exposed inspection
fixtures that decoded only the old inline format. Test-only decoder fixes were
explored, then archived and reverted with the rejected format. The retained
engine passes its full workspace suite. server fixture path allowlists and the
lean runner now support the explicitly authorized server task directory.

Invalid attempts are retained and excluded: an earlier Mach-O binary copied
to Pi never ran; the first native script reused one Cargo target across source
archives with zero mtimes, producing identical baseline binaries for all four
arms. Corrected builds use isolated target directories and assert distinct
SHA-256 values. server also required fetching locked dependencies and a test
fixture allowlist update. None of those failed attempts count as engine timing.

Artifact roots:

- Mac: `<scratch>`
- Pi: `<scratch>`
- server: `<scratch>`

Each native root retains source archives, native binary hashes, reports/logs,
the original refused databases, and `evidence.tar.gz`. Verified redundant DBs
are removed with an explicit [cleanup manifest](SCATTERED_PACKING_CLEANUP.json).
Cleanup removes **88 redundant database directories / 6,212,091,904 allocated
bytes**, retaining all twelve original refusal cases.
The current scattered-work task remains open: the ablation is finished and
rejected, while the density/work goal and wider seven-law release gates remain
unmet. No foundation convergence or production promotion is claimed.
