# Page-WAL qualification loop — 2026-09-13

The stable-page architecture remains the release candidate. This loop strengthens
its failure handling and recovery instead of starting another storage design.
It remains isolated until the F1 promotion gates pass. Accepted production E4
and the seven-law contract are unchanged.

## Concrete changes and earned evidence

1. **Torn checkpoint recovery.** The new fault matrix reproduced a refusal to
   reopen after a partial data-file extension, despite an intact committed WAL.
   Reopening now accepts missing/partial trailing pages only when the validated
   committed WAL supplies every missing page through its committed extent.
   Missing coverage still refuses. This follows the durable-WAL-before-data,
   durable-data-before-WAL-reset ordering examined in the local SQLite source
   (`sqlite-repo/src/wal.c`, `walCheckpoint`, approximately lines 2250–2278).
2. **Persisted logical disk allowance.** `set_cap` now commits its setting in the
   metadata page. It requires an already committed state and enough headroom for
   the policy transaction. Reopening restores the allowance. Metadata grows by
   eight bytes inside its existing page; ordinary benchmark file sizes do not
   gain a page. Old isolated pilot headers can still be inspected.
3. **Bounded snapshot admission.** This prototype admits at most eight snapshots
   and releases a slot on drop. Each snapshot still owns its requested cache and
   a bounded immutable WAL index. This is a stated limit, not proof of aggregate
   memory accounting or the strict zero-coordination reader law.
4. **Independent source-preserving salvage.** `pagewal_repair SOURCE NEW_DEST
   MAX_VALUE_BYTES` reads the source through read-only file handles while owning
   its existing writer lock. It never invokes the normal writer opener on the
   source. It validates committed WAL without trimming its tail, independently
   scans pages, and writes a fresh `current` database only for rows whose path
   from trustworthy metadata proves current membership. Uncertain rows go to
   `candidates.bin`, not the current database. Losses stream to `losses.jsonl`.
   It reopens and verifies current output against source values, independently
   rereads candidate framing/bytes, and checks source length/CRC fingerprints
   before writing `COMPLETE.json`. It never replaces the source.

The source-preservation tests additionally compare full source data/WAL bytes
before and after each attempt. Existing destinations and overlapping source/
destination paths refuse. Candidate records preserve page, slot, key and value;
their presence never authorizes automatic database replacement.

## Failure and recovery scope

The injected matrix covers each observed FileIo boundary in a mixed transaction
and its checkpoint: reads, writes, truncation/extent operations, and syncs.
Modes include EIO before an operation, EIO after completion, a partial write
followed by EIO, and a partial write followed by ENOSPC. Non-write operations in
the latter modes fail before the operation. Failed writer handles are poisoned.
Each case checks actual bytes after the failure and a simulated power-loss image
that discards writes/truncations since each file's last successful FULL sync.
The simulation is not a physical device power-cut test, arbitrary sector
reordering, or a complete destination-repair failure matrix.

The eight repair cases cover an intact source, uncheckpointed committed deletes,
damaged leaf, overflow, root, metadata, free page, and WAL. With 1,000 initially
inserted rows and 100 deletes, intact/free-page cases recover 900 current rows;
one damaged overflow value recovers 899 and names its affected key. Root/meta
damage yields 900 **candidates and zero current rows**. Leaf damage retains
unrelated current rows and names the damaged page as an unknown extent.
Complete corrupt-WAL frames currently cause source-preserving refusal, rather
than salvage of later independent regions. These limits remain visible gates.

The full Mac workspace suite passed with **360 distinct test entries**, including
subprocess helpers. Pi selected library, page-WAL and repair suites passed with
27 test entries, also including a helper. The 356-point/mode I/O matrix performs
712 reopen checks (ordinary failure image plus simulated loss of unsynced data).
The existing four deliberate process exits inside checkpoints also pass.

Open safety work: independent recovery across corrupt WAL regions, stronger
rootless current-membership evidence, damaged-schema integration, complete
repair-destination I/O and interruption tests, aggregate repair-space limits,
cross-process readers, and full resource/accounting/reader-latency gates.
Rootless candidates are useful recovery evidence, not completion of those gates.

## Benchmark interpretation

**New control correctness failure:** the first clean Mac 400K accepted-Store
arm failed verification in round 6: key 395080 contained version 4 instead of
version 6. The exact database and log are retained under
`matrix/sustain-400k-r1/e4`. It has no successful `report.json` and is excluded
from completed timing statistics. Completing other repetitions does not clear
this failure. tracker `pagewal-control-stale-row` tracks its investigation.
This is separate from the page-WAL v1/v2 arms; no production fix is claimed.

The independent read-only audit reproduced **399,999 scanned rows, three stale
values and one key-order violation** from the persisted file. Point lookup for
395080 returned the correct version 6, so this is a persisted scan/get
inconsistency, not merely a stale live cache. All source-file SHA-256 hashes
remain unchanged after auditing. The production kernel reader reproduces the
same failure in `tests/accepted_control_scan_regression.rs`; that known failing
fixture test is explicitly ignored by default and was run with `--ignored` to
record the red result. Root cause is not yet established. The earlier full
candidate-suite pass does not erase this separately reproduced control defect.

Run its preserved reproducer with:

```sh
cargo test --release --offline --features sqlite-balance,compact-cells \
  --test accepted_control_scan_regression -- --ignored --nocapture
```

Verified intact 100K-row salvage took **10.42 seconds on Mac** and **10.436
seconds on Pi**, including output build, source rechecks and independent output
verification. Pi ran under the 128 MiB address-space limit and recorded
10,976 KiB maximum RSS. These are single repair probes, not SQLite repair
comparisons or guarantees for damaged large sources. Both outputs contain
100,000 verified current rows, zero candidates/losses, and preserved source
length/CRC fingerprints. Source and repaired destination are retained.

All ordinary timing comparisons use identical raw 8-byte keys and 256-byte
values, an 8 MiB cache, buffered native FULL sync, transactions of 1,000 changes,
and native SQLite WAL autocheckpointing. SQLite uses a `WITHOUT ROWID` primary-key
table. This measures the storage primitives, not typed hybrid collections or
multimodel search indexes.

Each 100K case performs four rounds: 80,000 updates, 40,000 deletes and 40,000
fresh-key inserts. Each 400K case performs twelve rounds: 960,000 updates,
480,000 deletes and 480,000 fresh-key inserts, leaving 400,000 live rows.
Initial load is separate; mutation totals include commits and the ending
checkpoint. Full row/value checks and reopen verification run outside timing.

Four arms identify accepted E4, frozen page-WAL v1, this page-WAL v2, and native
SQLite. Final comparisons use three rotated repetitions on each platform.
The first Mac run overlapped its broader regression suite and is preserved as
`matrix-contended`; it is excluded from reported acceptance timing. The first
Pi 400K candidate probe was variable, so two additional repetitions were added
without discarding it. Pi timings retain the original services/workload; each
benchmark runs under a 128 MiB address-space limit.

## Disk-cap comparisons

These separate diagnostics insert 10K, 40K or 100K rows and request twelve full
update rounds in 1,000-operation transactions. The cap is twice each engine's
own loaded logical footprint. E4 enforces data+WAL admission before appending;
its WAL also has an independent 16 MiB maximum. Native SQLite has no equivalent
total data+WAL cap in this harness: it stops **after** a transaction boundary
crosses 2×. This is explicitly not an equal-admission or speed-parity claim.

Logical file sizes are observed after each put/commit, adding stat cost to both
arms. These observations cannot see every internal transient peak. Reported
time excludes snapshot verification and includes the ending checkpoint after
reader release. No physical free-space reservation or allocated-block cap is
claimed. Reopen checks prove exactly the acknowledged update prefix.

Reader cases are none, held for the workload, replaced after a complete N-row
round (`rolling`), and replaced every 5,000 updates (`short`). A long reader can
force refusal before one complete round, so it cannot reach its replacement
point. Releasing it earlier permits progress. The limit protects disk use by
refusing more writes; it does not guarantee unlimited writer progress beside
an indefinitely pinned snapshot.

## Evidence and reproduction

Mac evidence: `<scratch>`.
Isolated source: `/tmp/e4-pagewal-qualify`.
Pi evidence: `<scratch>`.
Pi source: `<scratch>`.

`source.tar.gz` and `provenance.json` freeze the engine and initial harness.
The later short-reader diagnostic changes only `src/bin/pagewal_cap.rs`;
its source and executable are retained separately. The engine and ordinary
benchmark source stay byte-identical. Use fresh output directories when
reproducing; one-shot runners deliberately refuse existing benchmark outputs.

The entry points are `tools/run_pagewal_qualification.py` and
`tools/pagewal_qualification_pi.sh`. `tools/check_foundation_gate.py` must still
refuse promotion while the remaining law, fixture and parity gates are open.
tracker task `pagewal-qualification` remains ongoing; this measured subloop
does not claim to finish that task's broader shipping criteria.

## Final measured results

Times below are seconds. Medians use three repetitions unless the control failed.

| Platform / case | Current E4 | Page-WAL v1 | Page-WAL v2 | SQLite |
|---|---:|---:|---:|---:|
| Mac / mixed-100k | 4.611 | 3.375 | 3.497 | 2.788 |
| Mac / sustain-400k | **FAIL 1/3**; completed range 64.236–72.560 | 43.622 | 41.290 | 32.423 |
| Pi / mixed-100k | 6.272 | 6.350 | 6.060 | 6.003 |
| Pi / sustain-400k | 100.301 | 73.282 | 73.661 | 70.216 |

Initial load is separate:

| Platform / case | Current E4 | Page-WAL v1 | Page-WAL v2 | SQLite |
|---|---:|---:|---:|---:|
| Mac / mixed-100k | 2.362 | 1.427 | 1.362 | 1.205 |
| Mac / sustain-400k | 9.053 | 5.164 | 4.996 | 4.338 |
| Pi / mixed-100k | 1.867 | 1.795 | 1.640 | 1.713 |
| Pi / sustain-400k | 7.547 | 6.662 | 6.660 | 6.712 |

Final/observed-peak logical sizes below are decimal MB, medians across completed runs. A sampled peak is a lower bound.

| Platform / 400K arm | Final MB | Observed peak MB |
|---|---:|---:|
| Mac / e4 | 131.749 | 131.749 |
| Mac / pagewal-v1 | 129.888 | 134.854 |
| Mac / pagewal-v2 | 129.888 | 134.854 |
| Mac / sqlite | 129.446 | 134.258 |
| Pi / e4 | 131.749 | 131.749 |
| Pi / pagewal-v1 | 129.888 | 134.854 |
| Pi / pagewal-v2 | 129.888 | 134.854 |
| Pi / sqlite | 129.446 | 134.258 |

Cap diagnostics: completed updates / requested updates; observed peak divided by loaded logical size.

| Platform | Rows / reader | E4 completed updates; peak | SQLite completed updates; peak |
|---|---|---:|---:|
| Mac | 10000 / none | 120,000 / 120,000; 1.418× | 10,000 / 120,000; 2.025× |
| Mac | 10000 / held | 9,000 / 120,000; 1.999× | 10,000 / 120,000; 2.025× |
| Mac | 10000 / rolling | 9,000 / 120,000; 1.999× | 120,000 / 120,000; 2.025× |
| Mac | 10000 / short | 120,000 / 120,000; 1.522× | 120,000 / 120,000; 1.515× |
| Mac | 40000 / none | 480,000 / 480,000; 1.338× | 480,000 / 480,000; 1.365× |
| Mac | 40000 / held | 38,000 / 480,000; 2.000× | 39,000 / 480,000; 2.014× |
| Mac | 40000 / rolling | 38,000 / 480,000; 2.000× | 39,000 / 480,000; 2.014× |
| Mac | 40000 / short | 480,000 / 480,000; 1.131× | 480,000 / 480,000; 1.131× |
| Mac | 100000 / none | 1,200,000 / 1,200,000; 1.135× | 1,200,000 / 1,200,000; 1.146× |
| Mac | 100000 / held | 54,000 / 1,200,000; 1.568× | 96,000 / 1,200,000; 2.002× |
| Mac | 100000 / rolling | 54,000 / 1,200,000; 1.568× | 96,000 / 1,200,000; 2.002× |
| Mac | 100000 / short | 1,200,000 / 1,200,000; 1.052× | 1,200,000 / 1,200,000; 1.052× |
| Pi | 10000 / none | 120,000 / 120,000; 1.418× | 10,000 / 120,000; 2.025× |
| Pi | 10000 / held | 9,000 / 120,000; 1.999× | 10,000 / 120,000; 2.025× |
| Pi | 10000 / rolling | 9,000 / 120,000; 1.999× | 120,000 / 120,000; 2.025× |
| Pi | 10000 / short | 120,000 / 120,000; 1.522× | 120,000 / 120,000; 1.515× |
| Pi | 40000 / none | 480,000 / 480,000; 1.338× | 480,000 / 480,000; 1.365× |
| Pi | 40000 / held | 38,000 / 480,000; 2.000× | 39,000 / 480,000; 2.014× |
| Pi | 40000 / rolling | 38,000 / 480,000; 2.000× | 39,000 / 480,000; 2.014× |
| Pi | 40000 / short | 480,000 / 480,000; 1.131× | 480,000 / 480,000; 1.131× |
| Pi | 100000 / none | 1,200,000 / 1,200,000; 1.135× | 1,200,000 / 1,200,000; 1.146× |
| Pi | 100000 / held | 54,000 / 1,200,000; 1.568× | 96,000 / 1,200,000; 2.002× |
| Pi | 100000 / rolling | 54,000 / 1,200,000; 1.568× | 96,000 / 1,200,000; 2.002× |
| Pi | 100000 / short | 1,200,000 / 1,200,000; 1.052× | 1,200,000 / 1,200,000; 1.052× |

## Loop decision and cleanup

Retain page-WAL v2 as the isolated architectural candidate. Its ordinary-row
density is unchanged, and the safety additions preserve the previous pilot's
performance at the measured scale. The owner subsequently accepted elapsed
time below 1.50× SQLite: the measured ordinary workloads pass on both Mac
(400K: 1.273×) and Pi (1.049×). Their former Mac time rejection used the old
1.10× threshold and no longer applies. Resize/overflow and full hybrid
collection qualification remain separate, unfinished gates.

The control's persistent scan/get inconsistency is an additional release blocker,
with its failing fixture and explicit ignored regression preserved. Neither
successful repetitions nor the full candidate-suite pass clear that failure.
No production implementation, SQL/service interface or new multimodel index
was shipped. The seven laws remain unchanged.

Removed 56 verified disposable Mac databases, totalling **2,533,228,544 allocated
bytes** (2,483,456,848 logical bytes). Retained the failed control, four final
400K engines, the 100K repair source/destination, long-reader cap evidence,
all 111 reports, logs, source archives and binaries. Allocated-file accounting
is not a promise of an identical filesystem free-space change.

Pi tests, all comparisons and repair completed, and their results were copied
before SSH became unreachable. Follow-up after the owner supplied stable
address `.40`: **41 verified redundant Pi databases were deleted**, freeing
**1,699,307,520 allocated bytes** (1,699,265,316 logical bytes). Final engines,
repair and cap evidence remain. See `PAGEWAL_QUALIFICATION_CLEANUP.json` for
both exact deletion manifests.
