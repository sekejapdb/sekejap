# Phase 2 index format — implementation candidate

Not yet a released/frozen index format. Phase 1 entity bytes, IDs, layouts,
vector sidecars, physical headers and WAL remain unchanged. Existing files
stay `E4COLL1` through open, ordinary writes, schema changes and checkpoint.
An explicit first index creation opts into the logical `E4COLL2` header in the
same transaction as its BUILDING descriptor. `enable_graph` also opts in and
creates the graph header in that transaction. Rollback restores COLL1. After
successful opt-in, dropping all indexes keeps COLL2; no silent downgrade.

The preserved Phase 1 **typed Database** reader/writer recognizes an intact
unknown E4COLL version and refuses before normalization or writes. The raw
PageWalStore is a byte-KV API and does not enforce collection/index invariants;
using it to edit a typed database remains unsupported. Raw salvage can preserve
all key families without interpreting their semantics. Never remove COLL2 to
make an older typed writer open a file. Future normal engine updates must keep
read/write support for every shipped descriptor and family encoding.

## Header and namespaces

Header keys remain `[00,00,copy]`, copy 0..2. The packet retains the existing
2081-byte envelope: bytes 0..8 are the eight-byte magic, bytes 8..10 are the
payload length as u16 big-endian, then payload and zero padding up to offset
2077 (exclusive), followed by CRC32C of bytes 0..2077 (exclusive) as u32
little-endian. The maximum
payload is 2067 bytes. Packet names below denote exact zero-terminated magic
bytes, including `b"E4COLL2\0"`, `b"E4IDX01\0"`, `b"E4GRF01\0"` and
`b"E4GNM01\0"`. COLL2 payload is:
`next_collection:u32be | next_layout:u32be | required_features:u64be |
next_index:u64be | live_index_count:u32be | optional E4LIMIT1 policy:56bytes`.
Only payload lengths 28/84 are admitted. The supported feature mask is `0x3f`:

| Bit | Mask | Meaning |
|---:|---:|---|
| 0 | `0x01` | common v1 index catalog and scalar family 1 |
| 1 | `0x02` | graph v1 |
| 2 | `0x04` | exact-vector family 2 |
| 3 | `0x08` | WGS84 point family 3 |
| 4 | `0x10` | analyzer-v1 text family 4 |
| 5 | `0x20` | symmetric-int8 vector family 5 |

Every secondary-index family requires bit0 as well as its family bit. Graph
enablement produces at least mask3. Feature bits are monotone after commit:
dropping the final index of a family does not clear its bit. A new incompatible
family requires a new explicit bit and admission support, not an unannounced
interpretation of a reserved byte. Unknown intact replicas are authoritative
refusals; differing valid replicas are corruption.

Ordered integers below use the frozen variable-width ordering (`0x80+width`
then minimal unsigned big-endian bytes). IDs are allocated monotonically and
never reused after commit. Failed/uncommitted allocations can be rolled back.

| Tag | Key after tag | Value |
|---|---|---|
| 00 | existing header/layout keys | unchanged |
| 01/02 | existing collection/sequence replicas | unchanged |
| 03 | copy:u8, index ID | 2081-byte replicated E4IDX01 descriptor |
| 04 | index ID | collection:u32be, global live registry |
| 05 | collection ID, index ID | empty, bounded collection lookup |
| 06 | copy:u8 | 2081-byte E4GRF01 graph-header packet |
| 07 | kind:u8, copy:u8, graph-name ID | 2081-byte E4GNM01 packet |
| 10 | existing collection name | unchanged |
| 11 | collection ID, UTF8 index name | ordered index ID |
| 12 | graph-name kind:u8, exact UTF8 name | ordered graph-name ID |
| 20 | existing collection ID, external key | unchanged |
| 40 | existing entity identity | unchanged dense-v3 row |
| 60 | existing entity identity, vector ordinal | unchanged authoritative f32-LE lane bytes |
| 70 | index ID, sortable scalar, entity sequence | empty |
| 71 | source entity, graph context, edge type, destination entity | version:u8=1 and Phase1 binary-JSON object |
| 72 | destination entity, graph context, edge type, source entity | empty reverse marker |
| 73 | index ID, entity sequence | layout:u32be, physical ordinal:u16be |
| 74 | index ID, Hilbert:u32be, entity sequence | longitude:f64le, latitude:f64le |
| 75 | index ID, UTF8 term, NUL, entity sequence | term frequency:u32be |
| 76 | index ID, entity sequence | document length:u32be |
| 77 | index ID, UTF8 term, NUL | document frequency:u64be |
| 78 | index ID | document count:u64be, total token count:u64be |
| 79 | index ID, entity sequence | locator:6 bytes, scale:f64le, dimension signed-i8 lanes |

Tags are hexadecimal; integer identities in keys use the ordered codec described
above. Graph identities and the edge property codec are specified in
`PHASE2_GRAPH_FORMAT.md`. Each derived entry begins with index identity;
collection and field scopes are part of its descriptor, never a hash convention.
E3 tags must not be copied because several collide with this registry.

## Common index descriptor

Each index has three `E4IDX01` replicas in the 2081-byte packet envelope. The
payload begins `index:u64be | collection:u32be | family:u8 | encoding:u16be=1`,
then exactly one family segment:

| Family | Family segment before lifecycle |
|---:|---|
| 1 scalar | `scalar-kind:u8 (bool1/int2/real3/text4) | unique:u8 (0/1)` |
| 2 exact vector | `dimension:u32be | options:u8=0` |
| 3 WGS84 point | `grid-bits:u8=16 | CRS:u8=1 | metric:u8=1 | options:u8=0` |
| 4 text | `analyzer:u16be=1 | Unicode-major:u8=17 | minor:u8=0 | patch:u8=0 | BM25:u16be=1 | options:u8=0` |
| 5 quantized vector | `dimension:u32be | quantizer:u8=1 | options:u8=0` |

The common suffix is `state:u8 | cursor:u64be | name-length:u16be | name:utf8 |
field-length:u16be | field:utf8`. State is BUILDING=0, READY=1, DROPPING=2.
BUILDING stores the last scanned entity sequence in `cursor`; READY and DROPPING
require cursor zero. Both strings are 1..128 bytes. Vector dimensions are
1..16384. Families 2..5 are non-unique; their family segment therefore has no
unique byte. Unknown family, encoding, option, analyzer, Unicode, metric,
quantizer or lifecycle combinations are refused. The referenced field is
explicitly declared with the descriptor's kind and dimension where applicable.
Removing or retyping it requires finishing explicit index drop first.

Scalar keys use null/missing=00, bool=01+byte, signed integer=02+sign-bit-flipped
u64be, real=03+IEEE sortable transform (finite only, -0 normalized to +0),
text=04+UTF8 with NUL escaped 00FF and terminator 0000. Text is at most 1024
UTF8 bytes, binary collation, no normalization. Values of different kinds are
not coerced except numeric JSON input for an explicitly Real field. Null and
missing share the lookup bucket and do not conflict in unique indexes.

## Lifecycle, bounds and sacrifices

Creation publishes a BUILDING descriptor; queries refuse it until READY. A
caller advances 1..256 entities per build step and commits explicitly. Every
ordinary write maintains BUILDING and READY entries, so mutations behind or
ahead of the cursor cannot leave a stale final index. Unique constraints are
fully established only when build succeeds: duplicates in an unscanned tail
can cause a later build step to fail. Failure poisons a partly modified writer;
rollback recovers its last committed state. Cancel uses begin_drop_index and
bounded drop steps, not deletion of primary records.

Drop publishes DROPPING before cleanup; queries refuse it and writes stop
maintaining it. Each step deletes at most 256 entries. The final entry cleanup,
three descriptors, registry/name mappings and count change share one transaction.
A snapshot already holding READY continues to see its earlier entries.

There are at most 64 live/building/dropping indexes per collection. Catalog
validation on open streams all live descriptors and referenced layouts before
any normalization: RAM is bounded, open work is proportional to index definitions,
not entity count. Mutations touch indexes of the affected collection only.
Each build step captures at most256 entity IDs; family-specific buffering is
bounded separately. Query results are capped at65,536 IDs. The raw scalar query
returns the first caller-requested limit in value/ID order; combined-query
cursor behavior is a separate API contract and does not change persisted bytes.
Each batch is also subject to the existing WAL/resource allowances; 256 is an
upper bound, not a guarantee that the batch fits every configured disk cap.

Metadata costs three 2081-byte packets per index plus mappings. Shared B-tree and
single cache stay unchanged. Family-specific costs and query contracts are in
the vector, spatial, text and quantized design documents. Index-only damage is
handled only by the explicit source-preserving verifier/rebuild path; primary
loss is not recoverable from derived entries.

Candidate Linux qualification and preserved-format evidence are recorded in
`PHASE2_VECTOR_SPATIAL_RESULTS.md`, `PHASE2_TEXT_READER_RESULTS.md`,
`PHASE2_QUANTIZED_RESULTS.md`, `PHASE2_MULTIMODEL_COMPAT_RESULTS.md`,
`PHASE2_QUANTIZED_COMPAT_CRASH_RESULTS.md`, `PHASE2_REBUILD_RESULTS.md` and
`PHASE2_WORKSPACE_RESULTS.md`. This evidence does not freeze or release the
format. The preserved ARM corpus now covers BUILDING descriptors with nonzero
cursors, DROPPING, post-drop retained features, scalar-only mask 1 and
graph-independent family masks 5/9/17/33. Its 104 sources pass 416 same-copy
rollback cycles and portable archive restoration; see
[rollback results](PHASE2_ROLLBACK_RESULTS.md). These are same-revision
cross-build baseline preparation, not historical released-version evidence.
Later revisions must replay the preserved binaries and original bytes.
