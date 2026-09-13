# Mixed-record prototype P0

This is a codec/storage experiment, not the integrated e4 SQL engine. Kernel
source and tests at the P0 measurement point were copied without edits from e3 commit
`d1ef252a415fa2fdf9dcf656629364bcd6ee8e74`. No e3 data is read. The engine's
full API, wrappers, search, spatial index and ANN integration remain future work.
The later [P1 experiment](ENTRY_STORAGE.md) adds opt-in kernel changes; this
document describes the original P0 measurement.

## Wire format fixed before implementation

Record: unsigned LEB128 layout ID; two bits per declared field (0 missing,
1 null, 2 value, 3 invalid); declared values in layout order; a binary object
containing undeclared fields. Even an empty extras object has a count, so a
truncated record is distinguishable from a complete one. No alignment padding.

Text: LEB128 byte length then UTF-8. Integer: width byte (1..8), minimum-width
signed big-endian two's complement. REAL: finite f64 little-endian. BOOL: 0/1
byte (packing is deferred). JSON: recursive type-tagged binary values. GEO:
the existing kernel binary Geom codec, length-prefixed (includes subtype;
point has 16 coordinate bytes). VECTOR: no slot body; field ordinal plus
entity ID identifies raw f32 little-endian values in the vector keyspace.

Binary JSON tags: 0 null, 1 false, 2 true, 3 signed integer, 4 unsigned integer,
5 finite f64, 6 string, 7 array, 8 object. Counts/lengths are LEB128; arrays
preserve order, objects use sorted UTF-8 keys. Integers preserve i64/u64 exactly.
P0 deliberately uses serde_json's existing finite f64 fractional-number domain,
not PostgreSQL arbitrary precision NUMERIC. Decimal precision beyond f64 is NOT
proved here; exact decimal encoding remains a required format decision before
production. Nesting limit 64, validated lengths, no unbounded allocation from
untrusted counts. Missing fields differ from null; the document API cannot
express SQL NULL separately from JSON null, so that SQL contract is deferred.

On JSON output: sorted object keys, no insignificant whitespace, serde_json
number rendering, last duplicate input key wins. Declared VECTOR values are
quantized once to f32 by the common input fixture; no arm silently receives
more precise vectors. Undeclared numeric arrays remain ordinary JSON arrays.

Catalog: binary layout descriptor with format magic, ID, ordered names/types,
and CRC32C. Three replicas written once at database creation use reserved prototype SQL-metadata keys,
each padded to 2081 bytes so no two fit in one 4096-byte leaf. Reopen independently
point-probes all three known keys, checks CRC and identity, and requires valid
copies to agree. One valid descriptor suffices. No inference from row values.
All-copy loss remains fatal for dependent rows; independent physical recovery
past damaged B-tree ancestors and bounded catalog-cache accounting are NOT
claimed solved by this prototype. The physical-leaf test verifies recovery after
damaging one and then two descriptor leaves, and explicit refusal after damaging
all three. This must still be resolved before e4's full Law-5 gate. P0 uses one
layout per database; codec tests exercise distinct layout versions, but database
schema-evolution dispatch and stable field-ID allocation are not implemented.

## Fair comparison

The executable creates a new, never-overwritten run directory under
`<scratch>/`. All fixtures, databases, WALs, temporary benchmark
files and results live there. Source/build outputs live in the e4 checkout.

Same deterministic documents, vectors, two directed edges per entity, insertion
order, 1000-row commit batches, 4096-byte pages, 8 MiB engine page caches.
Kernel FULL and SQLite WAL/FULL with fullfsync enabled on macOS; checkpoint
after loading, no VACUUM or compaction for either. Final sizes count ALL database
files including any remaining WAL/shared-memory files; peak sizes sampled at
commit boundaries are reported separately. Cache budgets exclude OS cache,
codec scratch, and SQLite statement memory. Reopen is NOT a cold-cache claim.

SQLite uses native scalar columns, JSONB profile/extras and raw vector BLOBs.
Vectors are inline in SQLite's table; e4 keeps its required separate keyspace.
Both have a unique external-key lookup, a born index, and forward/reverse edges.
No ANN, spatial, or text indexes in EITHER arm: hybrid evaluation applies the
same JSON/scalar/point filter and exact vector distance in Rust. A graph hybrid
uses the matching edge indexes. This proves storage/accessor composition, not
the e3 full multimodel query-performance gate. No bulk sort fast path in either.

Input generation is outside insertion timing and streamed from the same fixture
for each arm. Insertion timing includes fixture parsing, encoding, index writes,
batch commits, and final checkpoint. Output timing fully renders/materializes
values for both arms. Untimed exhaustive reopen validation compares every field,
vector and edge to the input; hybrid results have an independent fixture oracle.
Runs alternate arm order and retain individual results; medians are descriptive,
not a significance claim. No intentional OS cache flushing.

Cases: 10K and 40K entities, 3- and 128-dimensional vectors, three repetitions.
Documents mix optional/missing/null fields, nested Unicode JSON, large JSON in
1% of rows, and typed point geometry. Bytes per entity includes every value and
index. The ~57-byte small scalar-row aspiration does not apply to this corpus.

References: SQLite record format https://www.sqlite.org/fileformat.html#record_format;
SQLite JSONB https://sqlite.org/jsonb.html; PostgreSQL JSONB
https://www.postgresql.org/docs/current/datatype-json.html; PostgreSQL TOAST
https://www.postgresql.org/docs/current/storage-toast.html.
