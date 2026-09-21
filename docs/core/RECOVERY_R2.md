# Schema recovery R2 — 2026-09-10

R2 makes the existing schema replicas discoverable without the root or interior
pages. It adds no bytes to normal database storage and does not freeze a new DB
format. The recovery components remain separate from entity CRUD, transaction
design, graph storage and future vector resolution.

## Result

All eight retained 10K/40K cases passed. Each fixture contains two immutable
layout IDs with three descriptors each; version 2 adds a `created` integer.
Rows mix Unicode names, integers/nulls, booleans, points, nested binary JSON,
undeclared fields including u64::MAX, and a large JSON value every 100 rows
that exercises overflow storage. Every interior page, including the root, is
damaged in every case below. Additional failures destroy descriptor leaf pages.

| Additional damage | 10K decoded / raw | 40K decoded / raw | CLI seconds, 10K / 40K |
|---|---:|---:|---:|
| None | 10,000 / 10,000 | 40,000 / 40,000 | 0.123 / 0.348 |
| One copy per layout | 10,000 / 10,000 | 40,000 / 40,000 | 0.115 / 0.302 |
| Two copies per layout | 10,000 / 10,000 | 40,000 / 40,000 | 0.116 / 0.330 |
| All three copies per layout | 0 / 9,987 | 0 / 39,987 | 0.052 / 0.099 |

**All-copy loss is explicit dependency loss.** Recovery reports the missing
layout ID for each surviving row, preserves its exact encoded value, and exports
no guessed field names. The test's independent original layouts decode every
raw survivor back to its original document, including complete overflow values.
Recovery itself is never given those oracle layouts.

The clean fixture oracle identified 13 rows sharing the final catalog leaf.
Destroying all descriptor leaves destroys those 13 rows as well. A page CRC is
the current atomic integrity unit; recovery cannot claim those records survived.
Its journal names unknown damaged pages, rather than inventing an exact lost
row count from corrupt bytes. The 13-row count comes from the clean test oracle.
This shared-page blast radius remains relevant to later format decisions.

These are single local warm-cache export runs, not ingest benchmarks. The CLI
cases run sequentially. Their outputs matched the independently tested library
exports byte-for-byte. Source `data`, `wal` and `free` SHA-256 hashes were unchanged.
Both source sizes were unchanged: **2,629,632 B** at 10K and **10,424,320 B** at 40K.

## Extension interfaces

- `kernel::recover::CandidateReader`: holds an OS read-only source handle and
  excludes cooperating writers. Streams verified leaf cells without ancestry.
  Its value reader bounds allocation before reading an overflow chain and
  validates every page, bounds and whole-value CRC. The same streamed overflow
  validator serves R1 salvage.
- `RecoveryCodec`: E4 owns keyspace classification, layout-ID extraction and
  typed decoding. A decoder receives key, page and generation context so a
  future resolver can identify external dependencies. The default `DenseV3`
  handles the current compact entity namespace; a test substitutes another
  keyspace policy without changing the scanner. Other keyspaces are not exported
  by this default adapter.
- `visit_raw_records`: verifies bounded frames and CRC before delivering a
  raw candidate to a caller. It supports later decoding when external schema
  evidence or additional codecs become available.

This is an expandable prototype interface, not a final stable SDK contract.
The default decoder does not resolve external vectors; it preserves encoded
entity bytes and reports that dependency instead of returning partial documents.

## Recovery ownership and integrity

The schema pass scans **verified catalog leaf cells**, identified by the codec's
catalog namespace, and verifies each descriptor's own CRC and structure. It
does not search arbitrary payload text for a magic string and call that a schema.
The existing three padded descriptor copies are reused; there is no new sidecar
in the authoritative database.

All descriptors are collected before any entity is decoded. Identical copies
agree; differing valid definitions for one immutable ID create a conflict marker
and retain both definitions. Dependent rows stay raw with a named conflict.
There is no first-copy-wins or majority-vote fallback.

The destination must be new and cannot overlap the source. Raw values are
preserved before typed decoding. Raw archives are reopened independently and
their framing, checksums and counts checked before completion. Output schema
files retain their own descriptor CRC. Re-running against an occupied destination
is refused. Failed attempts and the source remain available.

All exported records carry **candidate membership**. This pass neither deduplicates
historical leaf versions nor reconstructs WAL/current membership. JSONL is a
derived export for inspection, not E4's on-disk database representation. It is
not published over the source or merged automatically into R1's verified subset.

## Commands and artifacts

```sh
cargo build --release --offline --features sqlite-balance,compact-cells --bin recover
target/release/recover schema <scratch> <scratch>
target/release/recover verify <scratch>
```

The result contains `layouts/`, `records.raw`, `unresolved.raw`,
`decoded.jsonl`, `issues.jsonl`, `report.json` and `COMPLETE`.
Missing layouts, conflicts, damaged pages, malformed cells, value limits and
decode failures are streamed to the issue journal.

`records.raw` preserves complete verified encoded values. `unresolved.raw`
preserves stored bytes/overflow markers when a complete value cannot be read
within the limit or verified; those markers still reference the source. Both
start with `E4ROWS01`, followed by frames:

```
page:u32LE | generation:u64LE | key_length:u32LE | value_length:u32LE
overflow_marker:u8 | key | value | crc32c:u32LE
```

CRC covers header, key and value. Marker flag 1 means the value is a source
overflow marker, not a complete value. The archive reader checks lengths before
allocation and checks CRC before calling its consumer.

The `verify` command checks archive framing/CRC/counts and descriptor integrity.
It does not certify current membership or completeness, and JSONL remains a
derived export rather than the authoritative raw evidence.

Retained evidence:

- `<scratch>/`: damaged fixtures, independent
  fixture oracles and initial library exports.
- `<scratch>`: all eight command runs,
  source hashes and verification. Use these result directories with the current
  verification command. The run also flipped an export byte, confirmed rejection,
  restored it and successfully reverified.
- `<scratch>`: three guards
  disabled separately; all caused the intended runtime test failure. Original
  sources were restored after each run.

## Validation and costs

The initial three tests failed at runtime because the schema recovery operation
was absent (`recovery-r2-baseline.log`). Seven schema tests now pass, covering
the full size/damage matrix, conflicting IDs, independent descriptor CRC,
resource limits, archive corruption/bounds and replacement codec policy.

The full feature-enabled workspace passed **291 tests, 0 failures, 0 ignored**
(`recovery-r2-workspace.log`). The 7 schema and 14 R1 recovery tests also passed
with default features (`recovery-r2-default.log`). Logs are under
`<scratch>/`. Three mutation checks caught disabled descriptor
CRC, conflict handling and whole-overflow-value CRC checks. E3 remains untouched.

**Normal storage tax added by R2: 0 bytes.** Existing descriptor payload in
these two-layout fixtures is 6 × 2,081 = 12,486 bytes, or 1.249 B/entity at 10K
and 0.312 B/entity at 40K, before page/cell overhead. R2 did not add that cost.

Recovery pays two sequential source scans, overflow reads, export writes and
independent raw-export verification. Default allowance is 1MiB per encoded value,
16 cached layouts, and four 64KiB output buffers; keys are bounded to a page.
Layout evidence spills to files, and issue counts do not require an in-memory
list proportional to losses. A caller can supply different limits or a decoder;
the cache limit is explicitly bounded to 256 entries.

These limits are not a measured hard process-memory bound: JSON decoding and
custom codec behavior still need the broader resource gate. Diagnostic disk
space is intentionally extra; full decoded+raw exports are about 6.45MB at 10K
and 25.89MB at 40K in this fixture. They are not database-density measurements.

Remaining database work includes current membership after destroyed ancestry,
schema registration/version enforcement on writes, external vector resolution,
graph integration, lifecycle/snapshot stress, hard memory measurement and fuller
I/O/process-kill fault injection. R2 closes the scoped schema-discovery proof,
not the complete seven-law or format-freeze gate.
