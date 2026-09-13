# P1 entry storage definition

This is the concrete compact-entry candidate measured in [ENTRY_RESULTS.md](ENTRY_RESULTS.md).
E4 is a clean store; there is no reader or migration for e3 JSON documents.
The P0 and intermediate codecs remain explicitly selected benchmark ablations,
not automatic fallback readers. The full E4 API has not been integrated.

## Identity, row, and catalog

```text
entity B+tree key = [0x80 + integer-byte-width][unsigned ID, minimal-width BE]
inline leaf cell = [0xff][entity key][typed record]
page slot        = [cell offset u16][cell length u16]

typed record     = [LEB128 (layout_id << 2 | flags)]
                   [optional missing/null/value states: 2 bits per field]
                   [integer widths: 3 bits per declared integer]
                   [present typed fields, in layout order]
                   [optional binary extras object]
```

`flags & 1` means a state bitmap is present. If omitted, every declared field
has a value. States retain missing / null / value distinctions, with `3`
rejected. `flags & 2` means the binary extras map is present; omission means
an empty map, not missing declared fields. Each integer-width code is width
minus one, allowing **all 1–8-byte signed values**, with no narrowing of i64.
Unused directory bits do not carry logical data. Layout IDs are bounded u32.
At layout ID 1, the packed layout/flags header takes one byte.

Declared text is UTF-8 with a varint byte length; integers use minimal-width
signed big-endian two's complement; real is finite f64 LE; bool is one byte;
a declared Point is exactly two f64 LE (16 bytes). General geometry retains
the existing binary geometry encoding. JSON and undeclared extras use the
[P0 binary value tags](PROTOTYPE.md), preserving nested arrays/objects,
Unicode, null, booleans and i64/u64 values. JSON text remains an API boundary
format. Declared vectors retain the separate-keyspace reference design; they
are not populated in the P1 benchmark. Points here are values, not a spatial
index. Fractional JSON values use f64, not arbitrary precision decimal.

Example scalar schema:

```text
layout 1: _key TEXT, fullname TEXT, born INT, born_year INT, location POINT
```

On the measured scalar fixture the typed record takes 50 bytes. At IDs up
to 40K, the key is at most three bytes and compact cell framing is one byte.
The existing four-byte slot directory remains. Leaf headers, interior pages,
three padded catalog replicas, meta pages and bookkeeping are counted in
whole-store sizes; the payload number alone is not the density claim.

Catalog keys sort before entity keys, avoiding a permanent catalog leaf
after the entity range that defeated the inherited dense append path. Three
checksummed, 2081-byte descriptors ensure copies cannot share a 4KiB leaf.
P1's timing harness reads copy zero on an intact file; descriptor redundancy
and damaged-copy behavior are tested separately by the P0 storage tests.
Loss of every descriptor still loses interpretation of dependent rows.
The complete independent catalog-recovery/Law-5 gate remains open. Row values
cannot reconstruct field names or exact type declarations by inference.

## Existing kernel, two opt-in changes

`sqlite-balance` adds bounded leaf-neighbor redistribution. A full leaf and
up to two siblings are repacked, adding one new leaf if needed. Only one
parent's children participate. If whole records cannot fit or the parent
would overflow, ordinary split propagation handles the case. Ordered
rightmost appends keep the existing dense append path.

Frozen sibling pages are copied before mutation to preserve snapshots.
Replacement leaf and parent images are built before installation and
fallible guards are acquired first. The temporary neighborhood is bounded
by four leaves plus a parent, independent of database size. This does not
sort or rewrite the table. It pays more page-copy/write work on some inserts
for substantially higher occupancy on shuffled input.

`compact-cells` removes redundant key/value lengths for width-tagged integer
keys: the key tag defines its end and the page slot defines the value's end.
The reserved `ff 81..88` prefix cannot be a legal old two-byte key length
inside a 4KiB page. Other key types and overflow markers keep the generic
cell format. Normal reads, verification, bulk packing and recovery share
the cell decoder/encoder. Page checksums and page identities remain intact.
Meta format version **2** prevents a version-1 build from opening a new
compact-cell store as if it understood the new cells. This is a physical
format gate, not a legacy entity payload fallback.

Both features are off by default to preserve explicit benchmark ablations.
Run the `entry` binary with both features and stage 5 for the measured
candidate. The original mixed P0 harness remains the default binary.

## Overflow and verification

Large values retain a leaf-resident key and 12-byte marker containing total
length, overflow head page, and whole-value CRC32C. A new regression found
an inherited recovery bug even with both features disabled: recovery packed
source-file overflow addresses directly into a different output file.

E4 recovery now streams each surviving overflow chain into the rebuild pool,
validates every page, bounds chain length, verifies total length and whole
value checksum, and writes a relocated marker. It needs one page of scratch
and at most one extra write guard, not a full-value buffer. The independently
verified rebuild is published only after success. A damaged overflow chain
currently refuses recovery and preserves the original file; salvage of the
remaining records around that damaged chain is still future work.

New tests cover shuffled density, live snapshots during neighboring-page
changes, WAL reopen before checkpoint, integer-key updates with overflow,
bulk packing, recovery round trips, and damaged-overflow refusal preserving
the source bytes. Codec tests cover optional fields, full integer ranges,
layout variants and truncation. Existing kernel suites remain required.
Two old assertions were made feature-aware: neighbor balance may attempt
the append hint fewer times (still within the original 117-attempt bound),
and incremental packing now fits within bulk packing on its 100K fixture.
The original expectations remain for builds without neighbor balancing.

## How this grows into graph storage

Node IDs remain stable u64 values; edges can refer to them without decoding
node payloads. The compact positional record is independent of adjacency,
vector, text and spatial indexes. The pager still has multi-root metadata,
so keeping independently growing node/edge/index trees is a concrete next
candidate to avoid cross-keyspace append interference. Root allocation and
field-ID/catalog dispatch are not wired into P1.

The next storage experiment must restore external-key uniqueness, graph
edges and indexes symmetrically in both engines, then vectors and hybrid
access. Measure each addition's file-size and latency cost. No entry-only
result establishes graph, ANN, service, query, device-memory or 48M gates.
