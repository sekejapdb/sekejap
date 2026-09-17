# Phase 2 exact vector index design

Status: implementation design, 2026-09-17. This is an exact, persisted
field-locator index over the existing f32 sidecars. It is not ANN, and it does
not change or duplicate Phase 1 entity/vector payloads. Approximate search
needs a different family, descriptor version, feature bit, recall contract and
acceptance evidence.

## Decision

Extend the one catalog in `src/indexes.rs`; do not create a vector registry.
`IndexId` remains the globally unique keyspace coordinate, and the existing
descriptor replicas, global registry, collection mapping, name mapping,
BUILDING/READY/DROPPING state, bounded build/drop steps and index count remain
shared by every family. Refactor `IndexInfo` to add a family discriminant while
retaining `kind`, `unique`, lifecycle and encoding version:

```text
IndexFamily::Scalar
IndexFamily::ExactVector
```

Scalar descriptors must continue to encode byte-for-byte as they do now in
`indexes.rs:77-95`; only decoding becomes a family dispatch after the common
15-byte prefix. Common creation/final-drop helpers own ID allocation and the
`03/04/05/11` mappings. Family hooks are deliberately small:

```text
validate_declared_field(layout, descriptor)
build_entry(descriptor, entity_id, row) -> optional key/value
maintain_entry(descriptor, entity_id, old/new vector locations)
entry_prefix(descriptor) -> bytes
```

`build_index_step` and `drop_index_step` remain the only public lifecycle
methods and dispatch through these hooks. `list_indexes`, `index_info`, catalog
admission and layout-change validation remain common. `alter_collection`
continues to require the same field name and `Kind`; for a vector index this
means the dimension cannot change until the index has been fully dropped.

## Format

Reserve logical feature bit 2 (`0x04`) for exact-vector-v1 and entry tag `0x73`.
Bits 0 and 1 retain catalog/scalar and graph meanings. Explicit
`create_exact_vector_index` atomically sets bit 2, allocates the ordinary
catalog descriptor and leaves it BUILDING. The bit is monotone and remains
after the last vector index is dropped. An older typed binary therefore refuses
a database that explicitly enabled this family, even after drop; ordinary open,
CRUD and checkpoint never opt in.

The `E4IDX01` vector descriptor payload is:

```text
index_id:u64be | collection_id:u32be | family:u8=2 | encoding:u16be=1 |
dimension:u32be | options:u8=0 | state:u8 | cursor:u64be |
name_len:u16be | name:utf8 | field_len:u16be | field:utf8
```

`state` and `cursor` have the existing meanings: BUILDING=0 with last scanned
sequence, READY=1 with zero cursor, DROPPING=2 with zero cursor. Dimension is
1..16384 and must equal the active declared `Kind::Vector`. Options must be
zero; metrics are query choices because the same exact bytes support all three.
Unknown family, encoding, options or state combinations are `Unsupported` at
writer and snapshot admission.

The derived entry is:

```text
key   73 | ordered(index_id) | ordered(entity_sequence)
value layout_id:u32be | physical_ordinal:u16be                 (6 bytes)
```

The descriptor supplies collection identity, so the key is collision-free
without repeating it. The value is a fixed-width pointer to the vector's
actual immutable layout and slot. Ordinals include hidden slot 0 and are at
most 255. For every entry, that layout descriptor must say
`fields[ordinal] == (descriptor.field, Vector(descriptor.dimension))`.
The referenced authoritative bytes remain solely at the frozen key:

```text
60 | ordered(collection_id) | ordered(entity_sequence) | ordered(ordinal)
```

This resolves the current layout trap: old entities keep old layout IDs and
ordinals after `alter_collection`, while replacements move to the new layout
and ordinal. Binding an index only to the collection's current ordinal would
misread historical rows. No scalar descriptor byte, dense-v3 row, layout
descriptor, vector key or f32 payload changes.

Catalog admission adds bit 2 to the supported mask, validates every vector
descriptor against its referenced collection/current layout, and performs one
bounded `0x73` prefix probe when bit 2 is absent. It does not scan a corpus at
open. Entry-to-row consistency belongs to explicit full verification/rebuild,
as with scalar derived entries; malformed keys/values or missing/malformed
sidecars encountered by build/query are corruption, never silently skipped.

## Layout lookup and write path

Add one dense-v3 internal helper:

```text
locate_vector(layout, row, field, dimension)
    -> MissingOrNull | Present { layout_id, ordinal }
```

It reads the row's layout ID, state bitmap and typed stream, using the existing
`Read`/`json_skip` machinery to validate and skip inline fields. It must not
construct a `serde_json::Value`, render vector lanes, or fetch unrelated vector
sidecars. A historical layout without the field is absent. The same name with
a non-vector type or different dimension is an incompatible historical value:
the build step fails at that row and does not advance its cursor. Rewriting or
deleting that entity can make a later retry valid.

`LoadedEntity` should retain its decoded `layout_id` (or `Arc<Layout>`) beside
the already captured `(ordinal, bytes)` vector cells. Replace
`maintain_scalar_indexes` with a shared `maintain_indexes` call made at the
same point in `write_entity` (`collections.rs:882`), before sidecar and row
writes. Scalar receives old/new documents. Exact-vector receives old/new
layout plus encoded vector locations:

- READY, or BUILDING with `sequence <= cursor`: keep an unchanged locator;
  put when present/location changed; delete when it became absent.
- BUILDING with `sequence > cursor`: put the new locator whenever present,
  otherwise delete it. This is idempotent and covers writes ahead of the
  builder that may or may not already have created an entry.
- DROPPING: do no maintenance.

Changing vector bytes at the same location needs no locator rewrite: the
existing sidecar put and the unchanged locator publish in the same page-WAL
transaction. A layout reorder changes the six-byte locator. Entity deletion
deletes the vector entry in the same transaction as scalar entries, graph
cascade, sidecar, primary row and external-key mapping. Validation failure,
write fault or abort therefore publishes either the complete old or complete
new state.

Late build scans primary rows in entity-ID order, calls `locate_vector`, and
for present values validates only the referenced sidecar length/finiteness
before buffering the six-byte locator. It retains at most the existing 256
entries and one sidecar (maximum 64 KiB), then writes entries and the advanced
descriptor in one transaction. READY is published only with the last batch.
Queries refuse BUILDING and DROPPING. Drop scans the family prefix and deletes
at most 256 entries, then uses the common final catalog removal. The primary
`0x60` values survive drop and reopening cannot resurrect the descriptor.

## Exact query contract

Expose three lower-is-better metrics: `Cosine`, `SquaredL2` and `NegativeDot`.
The query is f32, must be finite and exactly the declared dimension. Cosine
rejects a zero query and excludes zero stored vectors. Missing/null vectors are
absent from the locator index. For finite lanes, accumulate dot products and
squared norms in f64 in lane order; return:

```text
Cosine     = 1 - dot / (sqrt(stored_norm2) * sqrt(query_norm2))
SquaredL2  = sum((stored - query)^2)
NegativeDot = -sum(stored * query)
```

Normalize a zero result to `+0.0`. Order by `f64::total_cmp(distance)`, then
full `EntityId` ascending. These rules make ties and results independent of
SIMD availability. They also correct E3's internal negative-cosine ordering to
the Phase 2 public `1-cosine` value.

The first API needs both whole-field and prefiltered exact top-k. Its internal
shape can be:

```text
exact_vector_top_k(index, query, metric, k,
                   candidates: All | SortedUnique(&[EntityId]), max_examined)
    -> Vec<VectorHit { id, distance }>
```

`k` and supplied candidates are capped at 65,536. Candidate IDs must belong to
the descriptor's collection. Filtering happens before scoring/top-k. `All`
streams locator entries in sequence order and point-reads each referenced
sidecar. It deliberately does not merge-scan unrelated sidecars: a sparse
target field must not scan millions of other values under a small work budget.
The filtered path point-reads the tiny locator and its sidecar for each sorted candidate, so a selective graph/scalar
result does not scan the global vector field. Both paths hold only the query,
one vector, cursor state and a `k`-entry max-heap. Exceeding `max_examined`
returns an explicit resource-limit error and no partial result.

Neither path reads or renders the primary JSON document. If measurement shows
the filtered path's `Backend::get` vector copy matters, add a small
`with_value(key, FnOnce(&[u8]))` wrapper over an exact `RangeIter::for_each_ref`;
the format and query semantics do not depend on it.

Reuse the lane-decoding shape, `Scored` ordering and bounded `offer_score` heap
from E3 `kernel/src/graph.rs:1439-1525`. Do not reuse its fingerprint scan,
navigation graph, f32/architecture-dependent accumulation, zero-vector cosine
fallback, or `src/vector.rs::HnswGraph` stub. No code from `vecquant.rs` or
`nav.rs` belongs in this exact family.

## Test-first implementation order

1. Freeze current scalar descriptor packets and scalar lifecycle results, then
   refactor to family dispatch. Assert old scalar fixtures remain byte-identical.
   Add unknown family/vector encoding/options and missing-feature prefix probes
   to writer/snapshot refusal tests.
2. Unit-test `locate_vector` with the tiny fixture's two-lane vectors plus
   missing/null, reordered vector fields, old layouts, large JSON extras and
   malformed rows. Assert it never invokes JSON rendering or an unrelated
   sidecar callback.
3. Load the `PHASE2_WORKLOAD.md` tiny people fixture without an index; create
   the exact index, observe BUILDING query refusal, build in batches of two,
   commit each batch, reopen READY, and compare all hits to a test-local brute
   force oracle. The oracle owns its own `BTreeMap<EntityId, Vec<f32>>`, f64
   formulas, sort and heap; it must not call E4/E3 vector visitors, metrics,
   catalog codecs or top-k helpers.
4. Pin expected tiny-fixture orders for query `[1,0]`: cosine k=3 is
   `p0,p5,p1`; squared-L2 begins `p0,p5,p1`; negative-dot begins `p0,p1,p5`.
   For candidates `active && age>=30` (`p1,p3,p5`), cosine k=2 is `p5,p1`.
   This last case would return only `p5` if global k=2 were filtered afterward,
   so it proves filter-before-top-k. Also verify the workload's graph+active+
   rectangle candidate set returns only `p1`.
5. Run the workload mutation sequence: p1 vector to `[0,1]`, delete p2,
   replace the p0->p1 edge, and reinsert p2 under the same key/new ID. After
   every commit compare index results with the independent map. A snapshot
   opened before mutation retains old ranks; current and reopened handles see
   new ranks/ID ties. Include scalar-only updates, vector null/missing, field
   reorder, rollback and injected failure across entity/scalar/vector/graph.
6. Begin drop, prove queries refuse, reclaim in small committed steps, reopen
   after final removal, prove no descriptor/entries resurrect and prove the
   authoritative vector payload is still readable. Recreate gets a fresh ID.
   Repeat same index/field names in two collections with different dimensions.
7. Add focused zero-query, zero-stored-vector, nonfinite, wrong-dimension,
   malformed locator, wrong-layout/ordinal and missing-sidecar tests. Run these
   and the Phase 2 suite on Linux under the existing resource policy; do not
   use Mac database results for qualification.

Law 8 admission is a release gate, not a unit-test label: preserve a pre-vector
binary and checkpointed/WAL-pending fixtures. The new binary must keep old
scalar databases readable/writable without rebuild or descriptor rewrite; the
old binary must refuse bit-2 files before any byte/inventory change; future
vector family/encoding/options must likewise refuse source-preservingly. Once
exact-vector-v1 ships, subsequent binaries must read and write these descriptor
and locator bytes without automatic rebuild or semantic reinterpretation.

Named costs are one six-byte locator value plus key/B-tree overhead per present
indexed vector, one catalog descriptor, write work when presence/location
changes, O(field population) exact search, a bounded sidecar point lookup per candidate, and temporary build/drop WAL space. Peak/final disk,
RSS, build/live CRUD/reopen latency and filtered/unfiltered query work remain
measurements for the Linux workload; this design makes no performance claim.
