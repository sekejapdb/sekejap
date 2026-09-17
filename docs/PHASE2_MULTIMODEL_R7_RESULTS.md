# Phase 2 R7: corrected SQLite comparison

Candidate evidence, 2026-09-17. The **10K / 32-dimensional capture is complete**:
36 scheduled runs, 24 completed workloads, 12 verified resource refusals, and
zero nonzero process exits. This does not mean all requested workloads completed.
The complete 100K and separate 2K-person/1536-dimensional captures are
reported below; representative 1M is running. No Phase 2 acceptance, commit or freeze is
claimed by this report.

## What was measured

Each database contains 10,000 people, 100 organizations and 30,000 directed
relationships, plus scalar fields, JSON, points, text and 32-lane f32 vectors.
After loading and indexing, each of three rounds updates every person and
reasserts its three relationships, deletes 1,000 people, then reinserts them
and restores relationships. Update, delete and reinsert times are separate;
the total below excludes initial loading, index building, queries and oracles.

Three trials alternate engine order for each reader mode. E4 atomic and
resumable differ in late-index publication, not the subsequent CRUD batch
size. SQLite uses atomic index DDL, FTS5, RTree, scalar B-trees, and the same
exact vector math over f32 blobs. Its edge table is now WITHOUT ROWID, with a
composite primary key and one reverse index. The recursive query plan is
checked for frontier-first primary-key lookups. These corrections supersede
R4's redundant SQLite index and invalid graph timing comparison.

See the [protocol](PHASE2_MULTIMODEL_PROTOCOL.md) for cache, durability,
reader lifetimes, validation and the explicit capability differences.

## No-reader comparison

Medians of three completed trials on the same isolated server Linux job:

| Measurement | E4 atomic | E4 resumable | SQLite |
|---|---:|---:|---:|
| Entity load, seconds | 0.451 | 0.460 | 0.290 |
| Relationship load, seconds | 0.903 | 0.978 | 0.343 |
| Text late build, seconds | 1.837 | 2.202 | 0.026 |
| Three update/delete/reinsert rounds, seconds | 13.916 | 13.833 | 6.526 |
| Final logical size after process close, MiB | 8.664 | 8.664 | 6.723 |
| Sampled peak logical size, MiB | 13.308 | 13.004 | 12.231 |
| Sampled peak allocated size, MiB | 16.617 | 16.605 | 14.754 |
| Graph + active + bounding box + vector, milliseconds | 0.206 | 0.254 | 0.423 |
| Bounding-box query, milliseconds | 0.492 | 0.455 | 1.109 |
| Text + active + vector, milliseconds | 15.219 | 14.657 | 2.998 |

E4 is roughly twice SQLite's time for this repeated indexed CRUD workload,
and its final files are about 29% larger. The selected graph-filtered and
spatial queries are faster; text construction and text-plus-vector are clear
cost weaknesses. These observations do not establish a universal multimodel
query advantage. The [bounded cost review](PHASE2_TEXT_COST_REVIEW.md) identifies
format-preserving candidates to assess after the 100K baseline is preserved.

The complete [generated tables](phase2-evidence/multimodel-r7-10k/TABLES.md)
include every stage, all query cases, medians/ranges, matched-trial ratios,
memory and peak/load factors. In-process SQLite final size includes its live
shared-memory file (6.754 MiB); after-close size is 6.723 MiB. Both observations
are retained and labelled separately.

## Reader limits and temporary space

All 18 no-reader and batch-reader runs completed. Batch mode holds one old
snapshot across the first 256-person update commit of each round, verifies its
old payload, then releases it. Across three rounds its median CRUD totals are
13.352 seconds E4 atomic, 13.162 seconds E4 resumable, and 6.564 seconds SQLite.

For round-held (`short`) and three-round-held (`held`) readers, all six SQLite
runs completed, while all 12 E4 runs refused at the fixed 16 MiB WAL allowance.
Every E4 refusal followed exactly 5,888 confirmed updates in the first round;
no deletes or reinserts had committed. A fresh read-only oracle verified all
10K people, 100 organizations, 30K relationships, catalog state and selected
ready-index query answers against those committed boundaries. These are safe
refusals, not successful completion of the requested changes.

Refused E4 files occupied 22.974 MiB logically. SQLite's median sampled logical
peak was 114.027 MiB for round-held and 112.707 MiB for fully-held readers.
Their different completed work makes an E4/SQLite peak-efficiency ratio invalid.

Physical allocation also varies: the largest sampled allocated peak among
completed no-reader/batch E4 runs was 24.566 MiB, versus SQLite's 14.754 MiB.
No 2x physical-space ceiling is established. Loaded-size denominators precede
late index creation, so peak/load includes adding indexes as well as churn.
Polling every 50 ms can miss transients; unlinked SQLite temporary files are
not visible to the directory sampler. These peaks are lower bounds.

## Reproducible evidence

- [Raw report](phase2-evidence/multimodel-r7-10k/multimodel-r7-10000/report.json),
  per-run stdout/stderr and three batch-reader smoke logs are preserved locally.
- [Source hashes](phase2-evidence/multimodel-r7-10k/multimodel-r7-source.json)
  and binary hash `5034ad305932ecf60b28a1c9a61be5a08935cd56f2beeb09807ca24addb8e71e`
  identify the capture. This is the qualified R7 baseline. The working tree
  now contains a separate, unaccepted query-membership experiment in
  `src/query.rs` and its two query test files; R7 does not measure that change.
  The known `tests/index_vector.rs` difference is formatting only, previously
  verified.
- Native source, binary and databases remain under
  `<scratch>*` on the benchmark PVC.
  Captured source is immutable. Full workspace tests passed before timing;
  no compilation or test suite overlapped these runs.

Regenerate the detailed table with:

```sh
python3 tools/phase2_multimodel_report.py \
  docs/phase2-evidence/multimodel-r7-10k/multimodel-r7-10000/report.json \
  /tmp/phase2-r7-10k-tables.md
```

## Complete 100K / 32-dimensional capture

The full 36-run 100K matrix is now preserved. It has 24 completed workloads,
12 verified E4 resource refusals and zero nonzero process exits. All no-reader
and one-batch-reader runs completed. Each round updates 100,000 people and
reasserts 300,000 relationships, deletes 10,000 people, then reinserts them
and restores their relationships. These totals include all three rounds.

| Measurement | E4 atomic | E4 resumable | SQLite |
|---|---:|---:|---:|
| Entity load, seconds | 4.118 | 4.499 | 2.618 |
| Relationship load, seconds | 10.429 | 10.671 | 5.034 |
| Text late build, seconds | 18.791 | 20.554 | 0.313 |
| Three CRUD rounds, no reader, seconds | 139.557 | 141.140 | 108.666 |
| Three CRUD rounds, batch reader, seconds | 141.885 | 144.101 | 100.527 |
| Final logical size after close, MiB | 86.098 | 86.098 | 66.680 |
| No-reader sampled peak logical size, MiB | 91.484 | 91.484 | 77.842 |
| No-reader sampled peak allocated size, MiB | 157.816 | 221.754 | 121.031 |
| Graph + active + bounding box + vector, milliseconds | 0.577 | 0.529 | 0.518 |
| Members + active + spatial + vector, milliseconds | 29.744 | 29.800 | 8.927 |
| Scalar active + age, milliseconds | 19.528 | 18.026 | 0.239 |
| Bounding-box query, milliseconds | 7.531 | 5.165 | 15.553 |
| Text + active + vector, milliseconds | 228.606 | 207.172 | 67.823 |
| Positive-BM25 top-10, milliseconds | 58.278 | 52.818 | 79.744 |
| SQLite native BM25 top-10, milliseconds | N/A | N/A | 41.704 |
| Exact cosine top-10, milliseconds | 165.632 | 166.728 | 75.842 |
| RSS high-water, MiB | 23.223 | 23.422 | 17.711 |

All values are medians of three runs; except the explicitly labelled batch
row, they use no-reader arms. Query rows are before CRUD. E4 atomic takes
about 1.28 times SQLite's median three-round CRUD time and retains about 29%
more logical space. Selected spatial and common-formula BM25 cases favor E4;
other query shapes remain slower, particularly scalar filtering and text plus
vector. SQLite's separately measured native BM25 has its own scoring semantics;
the common-formula win is not a blanket claim against native FTS5 ranking.
The detailed tables include matched-trial ratios and ranges.

All 12 round-held/fully-held E4 runs stop after exactly 4,352 confirmed updates
in round one, before any deletion or reinsertion. Independent fresh read-only
verification confirms all 100,000 people, 100 organizations, 300,000 edges,
catalogs and selected ready-index answers at that boundary. Refused files retain
84.127 MiB logically. SQLite completes those cases and reaches a median sampled
logical peak of 2,263.691 MiB. Different completed work prevents an efficiency
ratio between these reader-held outcomes.

The largest sampled allocated peak across completed no-reader/batch runs is
221.941 MiB E4 versus 141.281 MiB SQLite. Filesystem allocation remains much
larger than logical bytes in some samples; a 2x physical-space cap is not proven.
Peak/load also includes late index creation. Preserve these limitations when
assessing the small final-file difference.

[Full tables](phase2-evidence/multimodel-r7-100k/TABLES.md) and
[raw report](phase2-evidence/multimodel-r7-100k/multimodel-r7-100000/report.json)
are saved with 72 per-run stdout/stderr logs, source/binary hashes and the exact
runner script. Report SHA-256:
`7d587bd42d4b087e63194bcc83aab75a91d3931e475612c29d1f2ee277df6f1e`.
The earlier partial 11-arm snapshot remains historical evidence, superseded by
this complete capture. The separate 1536-dimensional matrix has also completed (below);
representative 1M is running. The query membership experiment remains
unaccepted and is not included in these baseline measurements.

## Separate 2K-person / 1536-dimensional capture

This bounded embedding-width sample is complete:36runs,24completedworkloads,
12verifiedresource refusals,0nonzeroexits. It is not a 1M-person/1536-dimensional
result. No-reader medians of three trials:

| Measurement | E4 atomic | E4 resumable | SQLite |
|---|---:|---:|---:|
| Three update/delete/reinsert rounds, seconds | 6.513 | 5.817 | 2.807 |
| Final logical size after close, MiB | 17.281 | 17.281 | 16.484 |
| Sampled peak logical size, MiB | 23.956 | 23.956 | 21.502 |
| Sampled peak allocated size, MiB | 57.191 | 52.316 | 32.031 |
| Graph + active + bounding box + vector, milliseconds | 0.373 | 0.345 | 0.778 |
| Text + active + vector, milliseconds | 11.965 | 11.412 | 6.431 |
| Exact cosine top-10, milliseconds | 38.559 | 35.407 | 31.022 |

Final E4 logical files are about 4.8% larger here, while writes and several
query shapes still cost more. Timing variability is visible: E4 atomic CRUD
ranges from5.756to15.802s, SQLite from2.341to5.342s. Use the
[full tables and ranges](phase2-evidence/multimodel-r7-1536/TABLES.md), not a
single sample or cross-device comparison. Physical allocation peaks remain
separate from final logical density and no physical ceiling is established.
The [raw report](phase2-evidence/multimodel-r7-1536/multimodel-r7-1536/report.json),
72run logs and capture hashes are preserved locally. Report SHA-256:
`f21b5260001c7e55da41a609b4ae117c8af8acf0a7b4cb36690a4fc1efcf702f`.
The representative 1M/32-dimensional matrix is now running.
