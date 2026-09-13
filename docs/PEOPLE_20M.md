# 20 million people: E4, E4 with timestamps, SQLite

Completed **2026-09-10 (Australia/Melbourne)**: 20,000,000 people per arm.
All three stores passed the complete base-document checksum, contiguous-ID,
sampled exact point-read and structural/integrity checks. These are actual
full-size measurements, not extrapolations.

## Results

Disk GB below means decimal GB (1,000,000,000 bytes). Load time includes the
final checkpoint and close. Each arm ran once, sequentially.

| Arm | Load time | Rows/sec | Final bytes | Disk GB | Bytes/person | Disk / SQLite |
|---|---:|---:|---:|---:|---:|---:|
| E4 | 265.46 s | 75,340 | 3,587,301,420 | 3.587 | 179.37 | 1.0282× |
| E4 + automatic timestamps | 359.98 s | 55,559 | 3,759,722,540 | 3.760 | 187.99 | 1.0776× |
| SQLite (base fields) | 364.56 s | 54,860 | 3,488,817,152 | 3.489 | 174.44 | 1.0000× |

Plain E4 used **2.82% more disk** than SQLite and
completed the load in **27.18% less time**
in this run. Timestamped E4 used **7.76% more disk**
than SQLite; its load time was nearly the same (359.98 vs
364.56 seconds). These are single-run observations with warm
OS caches, not statistically established performance margins.

Adding both automatic timestamps to E4 cost:

- **172,421,120 bytes** on disk (4.81%, or
  8.62 bytes/person).
- **182,000,000 record-body bytes**, or
  9.10 bytes/person: 8 bytes
  for the two integer values, extra width-directory bits, and an extra state
  byte on rows with null income. Whole-file overhead differs slightly because
  changed record sizes alter page packing.
- **94.52 seconds** of additional observed
  load time (35.61%). The single fixed-order
  run does not isolate timestamp CPU cost from time-dependent system effects.

| Arm | Insert phase through last commit | Final checkpoint + close | Verification (excluded from load) |
|---|---:|---:|---:|
| E4 | 265.23 s | 0.23 s | 94.62 s |
| E4 + automatic timestamps | 359.73 s | 0.25 s | 111.07 s |
| SQLite (base fields) | 330.80 s | 33.76 s | 94.27 s |

SQLite's final checkpoint is a material part of its result, so it is included
in the comparison. E4 writes data pages during cache eviction throughout
loading; its final checkpoint has comparatively little buffered data left.
Both engines finish with their strongest configured durability barrier.

The retained SQLite version is **3.46.0**. Final E4 directories each contain
`data`, a 44-byte `free` file, and an empty WAL; SQLite contains `data.sqlite`.
Sizes were checked again against the actual directories after verification.
The shared input is **6,497,879,025 bytes**, generated in **53.82 seconds**;
those bytes/time are excluded from the per-engine storage/load totals.

## Dataset and protocol

The source is a deterministic, synthetic population of 20,000,000 people,
stored once in a shared `people.jsonl` fixture. Each row contains:

- `_key`: unique person key, with an independent ascending integer primary ID.
- `fullname`: combinations of synthetic first/last names, including Unicode.
- `born` and `born_year`: integer calendar date/year, with valid day 1–28.
- `active`: boolean; `income`: numeric, with 10% nulls.
- `location`: Point with longitude/latitude as two f64, spanning the globe.
- `profile`: nested JSON objects and arrays, language preferences, booleans,
  fractional scores, household size, strings and null.
- Optional undeclared `source` and `extra` fields, including Unicode and
  a large unsigned integer to check binary-JSON preservation.

This is the next entry-storage experiment. No graph edges, vectors, secondary
indexes or external-key uniqueness index are added in any arm. This measures
storing hybrid values; it does not measure spatial queries or indexing.

| Arm | Stored fields | Timestamp behavior |
|---|---|---|
| E4 | Base person fields | None |
| E4 + timestamps | Same base fields plus two typed integer slots | Automatically read wall clock for each fresh insert |
| SQLite | Base person fields | None |

E3's `src/db.rs` uses `_created_unix` and `_updated_unix` in Unix seconds.
The fresh-row path supplies both from the same clock sample; P2 follows that
behavior. The typed E4 variant stores those fields as integers, without JSON
text or per-row field names. The benchmark starts with a new database and
known-new IDs, matching E3's fresh-namespace optimization: it does not add an
old-row read for creation-time preservation. General upsert/update behavior
and E3's other automatic metadata are outside this fresh-load measurement.
The automatic-stamping helper is in the benchmark, not a completed public
E4 API. SQLite is intentionally the base-field control, isolating timestamp
storage overhead between the two E4 arms.

All arms use the same fixture, ascending integer IDs, prepared/incremental
inserts, 1,000-row transactions, 4096-byte pages, and 8MiB engine page caches.
E4 uses the existing kernel with `sqlite-balance,compact-cells` and dense-v3
records. SQLite uses native scalar columns, two REAL coordinates and JSONB
profile/extras columns. All arms use FULL durability; SQLite enables macOS
fullfsync and checkpoint_fullfsync. mmap is disabled. Checkpoint is performed
at the end; SQLite automatic checkpoint is disabled. There is no VACUUM,
whole-table sort, bulk packing or compression phase in any arm.

Load time includes database creation, fixture reading and JSON parsing,
record/JSONB encoding, all inserts and commits, final checkpoint, and close.
It excludes fixture generation, compilation, and the later verification pass.
Disk size sums all files in each database directory after checkpoint/close.
Shared fixture and benchmark-result files are not database storage. Peak
footprint is sampled every 250K rows and immediately before checkpoint; it is
a sampled maximum, not a guaranteed high-water mark of transient usage.
In particular, these samples can miss the overlap of SQLite data and WAL
files during its final checkpoint, so they are not a measured peak-space
comparison between engines.

This is one full-size run per arm, in E4 / timestamped E4 / SQLite order.
OS cache is not flushed; engine page budgets do not include OS caching,
JSON codec memory or statement allocations. Timing differences are measured
observations, not confidence intervals or cold-cache latency claims.
The host is arm64 macOS 26.5.1; data resides on the same scratch volume.

## Verification

A canonical full-document CRC32C is computed during fixture generation.
After loading, every arm is reopened and scans all rows in ID order. Every ID
must be contiguous and the full base-document checksum must match the fixture.
The timestamp arm additionally validates both timestamps on every row, then
removes them before computing the shared base checksum. Sampled point reads
compare exact documents, including ID-width boundaries at 255/256,
65,535/65,536 and 16,777,215/16,777,216. E4 runs the published-tree structural
verifier; SQLite runs integrity_check. Verification time is reported separately.

The preflight 10K run passed all three arms:
`<scratch>`.
A dedicated test checks typed timestamp round trips and ID boundaries.
The storage kernel is unchanged from the preceding 267-test passing P1 build.

## Reproduce and artifacts

```sh
cargo run --release --offline --features sqlite-balance,compact-cells --bin people -- 20000000
```

Full-size run directory:
`<scratch>`.
Progress log: `<scratch>`.
Each arm has its own database subdirectory and JSON result; `results.json`
collects verified arm results. All fixture, WAL, database, and temporary files
are on scratch. Existing benchmark directories are retained.
