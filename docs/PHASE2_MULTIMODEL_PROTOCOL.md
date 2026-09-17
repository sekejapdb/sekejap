# Multimodel comparison protocol, R6/R7

This records benchmark changes after diagnostic R4. The engine format and
runtime are unchanged by these harness revisions. Results must identify the
captured source and binary; this document does not claim a completed run.

## Reader lifetime

| Mode | Snapshot lifetime |
|---|---|
| `none` | No concurrent reader |
| `batch` (new in R7) | Once per CRUD round, read the old person/1, hold through the first update commit of up to 256 people, verify the old fields, then release; do not reacquire for later batches |
| `short` | The historical R4 name: one snapshot spans the entire update/delete/reinsert round |
| `held` | One snapshot spans all three rounds |

SQLite and E4 use the same lifetime. Opening and reading the snapshot fixes
its view before writes. The batch check covers the full E4 document and the
matching SQLite age, name, active flag, text, vector bytes, point and JSON
profile. It deliberately leaves subsequent commits without a reader, allowing
normal checkpoint opportunities. It is not a continuously reacquired reader.

## SQLite storage and query plans

R4 used an ordinary rowid edge table, a composite primary-key index, and an
identical explicit forward index. R6 removes that redundant copy: `edges` is
a `WITHOUT ROWID` table with the same composite primary key and one reverse
index. Entity IDs, relationship uniqueness, payloads and directions are
unchanged. New size/write ratios must be measured; R4 is diagnostic evidence
for its old schema.

The recursive graph query places the frontier first using `CROSS JOIN` and
probes the edge primary key by source. The runtime plan guard checks the
recursive segment specifically; an indexed anchor alone is insufficient.
Existing FTS-first join-order and indexed cascade-delete guards also apply.
Bundled SQLite version and actual plans are recorded in each result.

The generated data and final relationship oracles match, but insertion-time
validation is not identical: E4 validates typed relationship endpoints, while
the SQLite harness uses valid generated IDs and explicit cascade statements
with foreign-key enforcement off. Write times include those respective paths;
the measured difference cannot be attributed solely to B-tree efficiency.

R7's Rust SQLite wrapper maintains exact top-k by repeatedly sorting a small
vector, whereas E4 uses a bounded heap. This adds avoidable work to SQLite's
ranked-query timings; it does not affect load/CRUD/disk or unranked spatial
measurements. The separate query-replay experiment uses bounded heaps for
SQLite as well, and labels its post-CRUD inputs separately from R7's pre-CRUD
queries. Do not present those two query captures as the same workload state.

## Refusals and committed-state evidence

Only the typed collection error wrapping `kernel::Error::ResourceLimit` is
classified as a resource refusal. Other errors exit nonzero. A fixed-size
ledger advances only after successful commits, retaining batch boundaries,
catalog IDs/states/build cursors and completed stage measurements. It does not
retain an O(N) copy of the corpus or graph.

After a resource refusal, the failed database handle is dropped. A fresh
read-only snapshot is compared with the deterministic committed boundary:
every entity and organization payload/ID, relationships and properties, raw
relationship count, catalog lifecycle, and selected Ready-family queries.
Any mismatch or inability to read the snapshot fails the process. Only a
successful check emits `committed_state_verified: true`. This check is timed
separately and excluded from named workload stage timings.

The engine's existing 16 MiB WAL limit is unchanged. A verified refusal proves
the tested committed state survived; it does not mean the requested workload
completed. Driver report v2 distinguishes `capture_complete` from
`workload_complete`, counts refusals explicitly, and computes ordinary workload
summaries only from completed arms. Partial-stage evidence remains in raw
refusal records. RSS and whole-process time still include harness/oracle work.

## Capture and scale

R6 smoke tests normal E4/SQLite operation plus round-held and fully-held E4
refusals. R7 adds the batch-reader smoke, then three alternating trials at
10K and 100K with all four reader modes and E4 atomic/resumable publication
reported separately. The representative 1M and separate 1536-dimensional
mixed workloads remain required after these checks. Native work is sequential
in the isolated server job; source copies and binaries are preserved. The large
queue uses the same captured binary: 2K people at 1536 dimensions, then 1M at
32 dimensions, each with four reader modes, three trials and both E4 publication
policies. The retained build enables compact-cells, sqlite-balance,
keyspace-append and slotref-split. Default-build correctness is tested separately;
these timing claims apply to the recorded retained configuration.

The existing workload contract remains authoritative for payload equivalence,
independent answers, three CRUD rounds, durability, cache, disk/RSS accounting,
and the separate approximate-vector recall experiment. Sampled peak disk is a
lower bound; neither sampling nor logical-byte admission proves a physical
filesystem allocation ceiling.
