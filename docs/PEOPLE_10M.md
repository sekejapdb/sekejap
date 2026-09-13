# 10 million people: current E4 versus SQLite

Measured 2026-09-10 (Australia/Melbourne), after recovery R1/R2 work.
**All 10,000,000 rows in each of three arms verified.** Actual measurements;
no extrapolation and no storage-code changes for this benchmark.

## Results

Final disk size is the sum of database files after checkpoint and close,
rechecked after verification. GB is decimal (1,000,000,000 bytes).

| Arm | Final bytes | GB | Bytes/person | Size / SQLite | Load + checkpoint + close | Rows/sec |
|---|---:|---:|---:|---:|---:|---:|
| E4 | 1,793,167,404 | 1.793167 | 179.32 | 1.0292× | 119.61 s | 83,608 |
| E4 + automatic timestamps | 1,877,475,372 | 1.877475 | 187.75 | 1.0776× | 125.30 s | 79,807 |
| SQLite (base fields) | 1,742,336,000 | 1.742336 | 174.23 | 1.0000× | 124.62 s | 80,243 |

Plain E4 uses **2.92% more space** than SQLite:
**50,831,404 bytes (50.83 MB)** extra.
Its observed load time is **0.960× SQLite**.

Timestamped E4 uses **7.76% more space** than
base-field SQLite. Adding `_created_unix` and `_updated_unix` to E4 costs
**84,307,968 bytes**, or **8.43 bytes/person**
and **4.70%** more than plain E4.
SQLite has no timestamp columns in this test; that arm stores fewer fields.

| Arm | Creation + insert/commit | Final checkpoint/close | Verification, excluded from load | Filesystem allocated bytes |
|---|---:|---:|---:|---:|
| E4 | 119.37 s | 0.24 s | 43.75 s | 1,795,194,880 |
| E4 + automatic timestamps | 125.10 s | 0.20 s | 52.52 s | 1,879,793,664 |
| SQLite (base fields) | 113.31 s | 11.32 s | 42.75 s | 1,744,896,000 |

Allocated bytes are the sum of `st_blocks × 512`, recorded separately from
logical file length. APFS volume-level space accounting can differ.
E4 totals include its `data`, `free` and `wal` files; SQLite totals include
all files in its directory. Final per-file sizes are retained in the JSON.

## Dataset and fairness

The deterministic `person(id)` generator in `src/bin/people.rs` produced
10M synthetic names (including Unicode), integer birth date/year, booleans,
nullable income, longitude/latitude Point values, nested profile JSON,
and optional schemaless fields with Unicode and a large unsigned integer.
All arms stream the same fixture. E4 uses typed dense-v3 records and binary
JSON; SQLite uses native scalar columns, two REAL coordinates and its JSONB
for profile/extras. The fixture's JSON text is input only.

- Same scratch volume; Apple M3 Pro, 18 GiB RAM, arm64 macOS 26.5.1.
- Release build; `sqlite-balance,compact-cells`; SQLite 3.46.0 bundled by rusqlite.
- Ascending integer IDs, incremental inserts, commits every 1,000 rows.
- 4,096-byte pages; 8 MiB engine page cache per arm; Buffered I/O; mmap off.
- FULL synchronization; macOS fullfsync enabled; final checkpoint and close
  included. SQLite automatic checkpoints disabled, final WAL truncated.
- No graph edges, vectors, secondary indexes, external-key uniqueness index,
  compression, VACUUM or offline bulk packing.
- Timestamp helper samples Unix seconds per fresh insert, setting both typed
  fields. This is a benchmark helper, not a completed automatic-metadata API.

Load includes fixture reading/parsing, record encoding, insertion, commits,
checkpoint and close. Shared fixture generation took
**28.20 s** and produced **3,248,934,656 bytes**;
these input costs are excluded from each engine's load/size.

One run per arm, in E4 / timestamped E4 / SQLite order. OS caches were not
flushed; timings are observations, not confidence intervals or cold-cache
results. The 8 MiB setting bounds the engine cache, not total process/OS RAM.
A 4.25-second debug compilation for the timestamp unit test overlapped early
in the run; this run was not conducted on an otherwise isolated machine.
Sampled peak footprints in JSON are not guaranteed peaks and can miss the
SQLite data/WAL overlap during checkpoint; use the final sizes above.
This establishes fresh-entry density, not query performance, update/delete
behavior, integrated multimodel density, or completion of the seven laws.

## Verification

All three reopened stores scanned exactly **10,000,000 contiguous IDs** and
matched the fixture's canonical base-document CRC32C **3365301071**.
Sampled point reads compared exact documents, including IDs around 255/256
and 65,535/65,536. Timestamped E4 validated both timestamps on every row,
then excluded those additional fields from the common base checksum.
E4 passed the published-tree structural verifier; SQLite passed
`PRAGMA integrity_check`. Actual per-file lengths were rechecked after all
verification handles closed. The dedicated typed-timestamp roundtrip test
also passed (1 passed, 0 failed).

## Reproduce and retained evidence

```sh
cd <home>/
cargo run --release --offline --features sqlite-balance,compact-cells --bin people -- 10000000
cargo test --offline --features sqlite-balance,compact-cells --bin people
```

[Raw measured results](PEOPLE_10M_RESULTS.json) are also retained beside this
report. Run evidence directory:
`<scratch>`.

Retained: `results.json`, individual arm JSON files, `run.log`,
`post_verify_sizes.json`, `environment.json`, `source.tar.gz` (exact engine and
benchmark source plus Cargo manifests/lock), and this report. Environment
metadata includes source/binary SHA-256 hashes, toolchain versions, run order,
volume information and reproduction command. Reproduction creates a fresh
run directory; synthetic base data is deterministic, timestamps vary.

tracker: [10M task in sekejap-e4](http://127.0.0.1:5156/ui/?j=sekejap-e4#tree),
node `p3-10m`. Earlier [20M measurements](PEOPLE_20M.md) remain separate.

## Cleanup

Completed after all three arms verified and the report/results were saved.
Deleted only this run's `people.jsonl`, `e4/`, `e4_timestamps/`, `sqlite/`
and empty `tmp/`. All deletion targets were checked absent afterward.
Removed **8,661,913,432 logical bytes
(8.662 GB)** of fixture/database files;
pre-deletion allocated blocks totaled **8,671,518,720 bytes**.
This is measured removed file size, not a claim about immediate APFS free-space
reclamation. Exact deleted paths and sizes are retained in `cleanup.json`.

Kept results, logs, environment/source evidence, source archive and this report.
Previous 20M, P1 and recovery artifacts were outside the cleanup scope.
