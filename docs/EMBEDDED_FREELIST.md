# In-file freelist experiment — 2026-09-12

This loop tests whether moving derived allocator state into the existing data
file can remove separate freelist-file and directory barriers. It is an isolated
prototype, not a deployed format change. The accepted engine and seven laws
remain unchanged during evaluation.

## Implementation and protection

The prototype uses logical format version 3. Metadata roots 1–3 hold a freelist
head page, byte length and page count; root 0 remains the user tree. Free-list
pages use the ordinary 4 KiB pager, Free page kind and reserved tree identity
0xfffe, with independent page CRCs, generation stamps and a next-page link.
The body retains the existing framed freelist CRC and generation check.

Before checkpoint, reserve new backing pages using only the already-published
reuse horizon, then retire the old backing pages and serialize allocator state.
Allocation happens before serialization so the new metadata cannot advertise
its own backing pages as free. Reservation precedes retirement to avoid an
avoidable temporary bookkeeping spike. Pages are flushed and synchronized
before the root/descriptor slot is written and synchronized. Reuse advances
only after publication. Old metadata pages retain birth/retirement protection
for fallback roots and registered snapshots.

The two data/root barriers and durable WAL truncation remain. Ordinary
checkpoints neither replace a freelist file nor change the data/WAL filenames,
so they need no additional rename-directory barrier. Create and recovery keep
their filename-publication obligations. Existing recovery initialization may
still create an unused empty sidecar; it is not read by the candidate allocator
or replaced at normal checkpoints.

On reopen, verify bounded lengths, page identities, kinds, generation, links,
page CRCs and whole-body CRC before importing any hint. Duplicate/cyclic links,
out-of-file pages, stale generations and hints listing their own backing pages
are rejected. Damage discards the whole hint and leaks reuse knowledge; data
rows do not depend on that hint. A damaged metadata slot retains the existing
fallback-read behavior and conservative refusal to overwrite the damaged slot
without explicit repair.

An inline freelist body inside the root page was considered and removed before
timing: a bad body byte would invalidate the outer root-page CRC and widen the
blast radius. Separate pages retain isolation between hint-body damage and the
data-root publication.

## Source inspiration

The retained SQLite source at commit
`f3b9f74d81132426dee1ccc07a67fdad2ccfeaa9` describes linked freelist trunk pages
and the header references in `src/btree.c:6565–6640`. PostgreSQL at commit
`a11bce64a3be38f726cdca682f1e5b723cfd34d1` explains derived free-space knowledge
and its recovery behavior in `src/backend/storage/freespace/README:168–202`.
E4 adopts in-file metadata and conservative loss of hints, not PostgreSQL's FSM
search semantics or SQLite's in-place rollback/WAL protocol.

## Costs and gates

This design adds backing pages to the data extent and retires them through the
same bounded allocator used by rows. It also maintains backing-page identities
in memory and serializes the current freelist twice while planning/reserving.
The existing full-freelist export cost remains; this is not a delta-based
allocator or a claim of complete seven-law compliance. Configured data/WAL and
bookkeeping allowances are unchanged. No larger limit is used to hide overhead.

Two existing gates currently fail:

- With `tracked_pages=4`, seeding individually committed 4,000-byte rows safely
  refuses a commit where the accepted engine completes the seed. Moving page
  reservation before retirement did not eliminate this capacity regression.
- Four overflow lifecycle cases need one more 4 KiB growth step before their
  plateau. For example, later file sizes are 2,396,160 / 2,400,256 / 2,404,352 /
  2,404,352 / 2,404,352 / 2,404,352 bytes. The original gate requires equality
  between cycles 4 and 8; it is left unchanged. Physical accounting still
  includes every page after adding the explicitly live freelist backing pages.

These failures prevent promotion regardless of the timing result. They are
safe-refusal/capacity and plateau regressions, not observed row loss. No limit
or stabilization threshold was widened to turn them green.

New tests exercise writer reopen with a held reader, multi-page freelist-body
corruption with intact current rows, and damaged-newest-metadata fallback.
Existing stale-hint and recovery tests now target the actual in-file hint.
The recovery classifier recognizes the reserved descriptor fields rather than
mistaking them for extra user trees. One scanner fault fixture explicitly
constructs its verified interior/leaf boundary because metadata pages now
separate the formerly adjacent physical generations.

## Measurement design

The Mac matrix has 24 arms: two order-reversed repetitions of batches 1/100/1000
at 1K/10K/100K people, plus 400K twelve-cycle mixed churn and 100K held-reader
churn. Every case includes accepted E4, candidate E4 and native SQLite. The Pi
matrix has twelve arms covering batches 1 and 100, 400K sustained churn and a
100K held reader, under the same 128 MiB address-space ceiling for every engine.
The Pi retains its existing services; it is not a dedicated idle machine.

Both E4 builds use identical collection harness code and 8 MiB caches. FULL
sync and buffered I/O remain. SQLite retains native WAL auto-checkpoint=1000.
The corpus includes external keys, scalar fields, binary JSON, points and four
exact f32 vector lanes across two collections. Every row is verified against
the deterministic corpus, snapshots and reopened states are checked, and SQLite
runs integrity checks. Load and churn are separate timings. Phase timings
include phase-ending checkpoints and exclude oracle verification. Batch cases
use different corpus sizes; compare engines within a case, not absolute times
between cases. Peak logical and allocated sizes include all database files,
reader release and alteration; 1 ms samples remain lower bounds, not caps.

Held-reader SQLite timings include its native checkpoint busy waits; do not
interpret that ratio as raw mutation throughput. Pi times are single runs.
Artifact root: `<scratch>/`; Pi root:
`<scratch>/`.

## Result: speed gain proven; prototype not promoted

The active engine remains unchanged. The candidate is retained as an archived
experiment, but the isolated working source is reverted because the existing
capacity and plateau gates did not pass. Neither threshold is relaxed.

### Mac

| Case | Baseline load / churn s | Candidate load / churn s | SQLite load / churn s | Churn improvement |
|---|---:|---:|---:|---:|
| batch-1 | 31.801 / 24.919 | 15.665 / 12.553 | 7.924 / 6.134 | 49.6% |
| batch-100 | 3.605 / 4.390 | 1.908 / 2.378 | 0.908 / 1.167 | 45.8% |
| batch-1000 | 5.669 / 8.824 | 3.985 / 6.208 | 1.547 / 3.290 | 29.6% |
| mixed-400000 | 23.290 / 108.155 | 16.428 / 76.137 | 6.050 / 39.648 | 29.6% |
| held-100000 | 5.728 / 8.846 | 4.032 / 6.257 | 1.549 / 23.897 | 29.3% |

| Case | Baseline peak MiB | Candidate peak MiB | SQLite peak MiB |
|---|---:|---:|---:|
| batch-1 | 0.481 | 0.492 | 4.230 |
| batch-100 | 3.733 | 3.742 | 6.976 |
| batch-1000 | 35.180 | 35.191 | 33.961 |
| mixed-400000 | 139.596 | 139.605 | 121.840 |
| held-100000 | 65.883 | 65.988 | 149.284 |

### Pi

| Case | Baseline load / churn s | Candidate load / churn s | SQLite load / churn s | Churn improvement |
|---|---:|---:|---:|---:|
| batch-1 | 14.762 / 11.765 | 7.974 / 5.908 | 3.708 / 3.045 | 49.8% |
| batch-100 | 2.245 / 2.703 | 1.350 / 2.022 | 0.695 / 1.157 | 25.2% |
| mixed-400000 | 27.984 / 161.379 | 25.405 / 152.074 | 8.831 / 76.783 | 5.8% |
| held-100000 | 6.723 / 10.664 | 6.167 / 10.396 | 2.273 / 24.925 | 2.5% |

| Case | Baseline peak MiB | Candidate peak MiB | SQLite peak MiB |
|---|---:|---:|---:|
| batch-1 | 0.481 | 0.492 | 4.230 |
| batch-100 | 3.733 | 3.742 | 6.976 |
| mixed-400000 | 139.596 | 139.605 | 121.840 |
| held-100000 | 65.883 | 65.988 | 149.284 |

Mac batch rows are two-run means; 400K/held cases and all Pi cases are single
runs. Held-reader SQLite times include checkpoint busy waits.

The final selected Mac checks have **171 passes and 5 failures**: one unchanged
bookkeeping-cap gate and four unchanged overflow-plateau gates. The Pi subset
has **147 passes and the same bookkeeping-cap failure**. Recovery, stale-hint
rejection, freelist-body corruption, fallback reads, held snapshots, packing
fault tests and the configured 1024×1536 embedding workload pass their selected
checks. No complete shipping-suite pass is claimed. Earlier diagnostic failures
and their fixture/format fixes remain in the logs.

All 36 timing arms passed their full row oracles and reopens; 87 three-way state comparisons passed.

Audited per-run times, allocated sizes, final sizes, issued bytes and Pi RSS:
[Mac results](FREELIST_RESULTS.json), [Pi results](FREELIST_PI_RESULTS.json).

The useful conclusion is that separate allocator-file publication is a
substantial commit cost. Adoption needs a metadata-reservation design that
preserves constrained CRUD capacity and the existing stabilization gate; the
speed result alone does not earn a production change.

The Pi bulk result is materially smaller than the Mac result: 5.8% faster at
400K, and 2.5% faster with a held reader, versus roughly 30% on Mac. The small
commit benefit is repeatable on Mac and reproduced on Pi; it does not establish
a broad 30–50% gain on the deployment device.

Pi maximum RSS at 400K is 10.75 MiB baseline, 10.70 MiB candidate and 11.56 MiB
SQLite; with the held reader it is 13.53 / 14.03 / 25.56 MiB. These are measured
process peaks under the common 128 MiB virtual-memory ceiling, not filesystem
cache measurements or a proof that every allocation uses the pool ledger.

## Evidence, reversion and cleanup

The verified candidate source archive has SHA256
`3e4d9a0c94e9901a5d655ff5f96c7fe7ae8d0be3f81fd51a3f1407c1be48575e`.
Every candidate source member matches the Pi copy; its baseline source matches
the accepted engine with the same benchmark harness. Both platforms' binaries
are distinct. [Provenance](FREELIST_PROVENANCE.json) records those fingerprints.
The isolated Mac working copy was restored to the accepted source. Pi experiment
sources and binaries remain isolated; no deployed service or active engine was
changed. The seven laws remain untouched.

After audit, 21 Mac database directories were removed (402,226,756 logical /
405,659,648 allocated bytes), plus nine Pi directories (181,095,340 logical /
181,112,832 allocated bytes). All three 400K comparison databases remain on each
platform, together with reports, logs, the candidate archive and binaries.
See [Mac cleanup](FREELIST_CLEANUP.json) and [Pi cleanup](FREELIST_PI_CLEANUP.json).
