# P0: mixed entities against SQLite — 2026-09-09

**The mixed-data storage design works for this corpus. SQLite storage parity and
hybrid-filter performance are not achieved.** This is an executable Rust codec
on e3's unchanged B-tree kernel, not the completed e4 SQL/API integration.

All fixtures, databases, WALs, query oracles and raw results are on scratch:

- Baseline (full object reconstruction for hybrid predicates):
  `<scratch>/`
- Direct typed projection:
  `<scratch>/`
- Successful initial smoke check: `<scratch>/`
- Failed initial smoke check, retained: `<scratch>/`

Each full run contains `results.json`, four shared JSONL fixtures, nine query
oracles per fixture, and twelve `n*-d*-r*` directories. Each repetition has its
own `e4/` and `sqlite/` directories. Nothing was overwritten or compacted.

## Conditions

Native bundled SQLite 3.46.0, 4096-byte pages, 8 MiB page cache in each engine,
WAL/FULL durability (macOS fullfsync enabled), commits every 1000 entities,
checkpoint after loading. Same insertion order and input fixture for both arms.
Three repetitions per case, alternating which engine runs first. Sizes below
include all database files after checkpoint and close. MB means 1,000,000 bytes.
Peak size sampled at commit boundaries is retained in the raw results too.

Records contain an external key, name, integer birth date, nested JSON with
Unicode/arrays/null/boolean/numeric values, an optional declared nickname,
undeclared fields, a point, and a vector. One percent have a ~6.4 KiB JSON text
field, exercising overflow. Each entity also has two graph edges. Vectors are
3 or 128 dimensions, quantized identically to fp32 before either arm sees them.

SQLite stores native scalar columns, JSONB profile/extras, and inline vector
BLOBs. E4 stores binary positional records and vectors in its vector keyspace.
Both have equivalent external-key, birth-date, forward-edge and reverse-edge
indexes. No ANN/spatial/text index in either arm. This is a comparison of
logical equivalent data with native storage choices, not identical page layouts.

Insertion times include shared fixture parsing, serialization, index maintenance,
commits and checkpoint. Query times consume the output. The operating-system
cache is not flushed; reopening is not claimed to be a cold-cache test. Memory
limits apply to engine page caches, not the entire process or OS cache. These
small local runs cannot establish the 48M disk-first gate or RAM scaling.

## Storage and insertion

Medians from the final direct-projection run; file sizes were identical in all
three repetitions of each case, and unchanged from the baseline.

| Entities | Vector dims | E4 MB | SQLite MB | E4/SQLite size | E4 insert s | SQLite insert s |
|---:|---:|---:|---:|---:|---:|---:|
| 10,000 | 3 | 8.966 | 3.678 | 2.438× | 0.316 | 0.259 |
| 40,000 | 3 | 35.811 | 15.073 | 2.376× | 1.170 | 0.927 |
| 10,000 | 128 | 18.514 | 9.834 | 1.883× | 0.441 | 0.385 |
| 40,000 | 128 | 74.088 | 39.358 | 1.882× | 1.724 | 1.398 |

The 128-dimensional case reaches the rough ≤2× whole-file threshold for this
corpus; the 3-dimensional case does not. Neither demonstrates SQLite parity.
Increasing N by 4 increases file size by about 4. There is no growing size ratio
in this range, but two rungs do not establish asymptotic behavior.

## Reads and hybrid queries

Times in milliseconds, median of three repetitions. Full output reconstructs
and serializes every document, including vectors. The point column is the total
for 500 deterministic lookups. The filter column is the sum of eight queries:
indexed birth-date range, JSON `preferences.quiet`, point bounding box, exact
vector L2 scoring, top 10. The graph column follows two hops from 64 seeds,
deduplicates endpoints, applies a hybrid filter and returns exact vector top 10.

| Entities / dims | Full output E4 / SQLite | 500 gets E4 / SQLite | 8 hybrid filters E4 / SQLite | Graph hybrid E4 / SQLite |
|---|---:|---:|---:|---:|
| 10K / 3 | 41.86 / 48.43 | 2.37 / 3.85 | 65.24 / 17.30 | 0.522 / 0.958 |
| 40K / 3 | 169.01 / 192.22 | 3.95 / 4.33 | 342.58 / 95.64 | 1.110 / 1.890 |
| 10K / 128 | 122.81 / 133.49 | 7.63 / 8.35 | 83.72 / 22.30 | 0.576 / 1.101 |
| 40K / 128 | 496.18 / 533.18 | 8.71 / 8.79 | 415.37 / 155.11 | 1.199 / 2.318 |

Full-output medians favor e4 by 7–14%. Point and graph-hybrid medians also favor
e4 here (the 40K/128 point difference is very small). Selective hybrid filters
remain **2.68–3.77× slower than SQLite**. This is a failure of the performance
aspiration, not something to hide behind a faster full scan.

The direct projection ablation avoids constructing skipped binary-JSON values.
It reduces aggregate e4 hybrid-filter time by **1.59–2.01×** versus the retained
baseline. All answers and stored bytes are unchanged. The baseline reconstructed
whole objects, whereas SQLite already used native `json_extract`; the final
projection comparison is the more useful accessor test. P0 still walks preceding
slots and nested JSON tags; no offset directory or JSON path index exists.

## Where the bytes go

At 40K entities:

- The entity record averages **268.13 bytes**, independent of vector dimension.
  The same documents without vectors occupy **389.15 bytes/entity** as compact
  UTF-8 JSON (in-memory measurement, not a third stored database): the typed
  payload is **31.1% smaller** on this nested-JSON-heavy corpus.
- Vector values add exactly **12 or 512 bytes/entity**. They are not hidden
  from the whole-file accounting.
- All live keys together add **161.00 bytes/entity**: graph edge keys alone
  account for 100 bytes (two forward and two reverse, 25 bytes each).
- Live keys + values total **449.29 or 949.29 bytes/entity**, before B-tree
  cell framing, page headers, slack, and overflow-page overhead.
- Three padded layout descriptors total 6,243 value bytes for the whole DB.
- The allocated leaf pages in the 40K/3D file average **55.6% occupied**,
  counting slot-directory bytes. This diagnostic scans all allocated pages;
  it does not claim a full live-page reachability audit. Their entry count
  equals the logical stored record count (320,003).

The whole-file gap therefore cannot be attributed only to the payload codec.
Key widths, separate records and page utilization need their own controlled
experiments. No index was removed and no page packing/compaction was performed
to improve the reported ratio. The ~57-byte scalar benchmark target is not a
target for these much richer records.

## Correctness and tests

Every run checks every reopened document against the shared input, including
missing versus null, JSON integers up to u64::MAX, Unicode, spatial coordinates,
and fp32 vector values. Every outgoing edge is checked. E4 external-key,
birth-date and reverse-edge entries are also verified. SQLite passes
`integrity_check`. Both full-output byte counts and CRC32C checksums agree.
All nine top-10 lists match an independently fixture-derived candidate oracle,
with a shared exact-distance function and deterministic ID tie breaking.
This proves exact scoring in this harness, not ANN recall or SQL compatibility.

- First six codec tests were observed failing before implementation, then passed.
- Initial smoke validation exposed serde_json default parsing changing a
  promoted fp32 value by one f64 bit. A failing regression test pinned it;
  enabling `float_roundtrip` for both arms fixed it without relaxing equality.
- The inherited kernel suite: **246 passed**, no failures or ignored tests.
- Final prototype suite: **14 passed** (12 codec + 2 storage), no failures or
  ignored tests. An earlier combined run passed 257 tests before the three
  final projection/storage tests were added.
- Physical corruption check finds three descriptor replicas on distinct leaves,
  damages them sequentially, recovers the layout with one or two damaged leaves,
  and explicitly refuses after all three are damaged.
- Reopen test includes ~150 KiB nested JSON and a 1,536-dimensional external
  vector, exercising multiple overflow chains.

Full combined baseline log: `p0-1788947910/test-results.log`.
Final prototype log: `p0-1788949188/test-results.log`.
At the P0 measurement point, kernel source was byte-identical to the e3 seed.
The later [P1 experiment](ENTRY_STORAGE.md) adds targeted kernel changes.
E3 itself is unchanged.

## Limits and next evidence needed

P0 has one layout per database and document-level JSON null semantics. Database
schema evolution, SQL NULL versus JSON null, exact decimals beyond f64, the
catalog's complete Law-5 recovery contract, per-field memory accounting,
statement execution, all specialized indexes and wrappers remain unproved.
The typed codec does support selecting an explicit old/new layout in isolation.
The catalog cannot be reconstructed from nameless row values if all descriptors
are lost. No claim of production readiness or completion of the seven-law gate.

The next useful experiments are targeted hybrid projection/late materialization
and a controlled page-density/key-overhead comparison using the existing kernel.
Only then does a larger disk-first benchmark become persuasive.

## Reproduce

From the e4 checkout (each invocation creates a new directory):

```sh
cargo run --release --offline -p e4-prototype -- <scratch> --smoke
cargo run --release --offline -p e4-prototype -- <scratch>
cargo run --release --offline -p e4-prototype -- <scratch> --project
TMPDIR=<scratch> cargo test --release --offline --workspace -- --test-threads=2
```

The baseline flag omits direct JSON-path projection; `--project` enables it.
Both configurations use the same codec, file format, indexes and correctness
checks. See [PROTOTYPE.md](PROTOTYPE.md) for the wire format and detailed scope.
