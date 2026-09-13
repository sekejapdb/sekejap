# Typed collection boundary — 2026-09-11

The internal Rust API is `e4_prototype::collections::Database`. It layers a
persistent catalog, external-key index and typed CRUD over the existing pager.
This is the boundary for adapting E3 interfaces. It does not implement SQL,
HTTP, scalar indexes, graph adjacency or a deployment package. The seven laws
in CONTRACT.md are unchanged; this slice is not a claim that all have passed.

## API example

Mutations compare existing vector bytes by layout field ordinal and write only
changed sidecars. Scalar-only updates retain unchanged embeddings; validation,
stable identity and snapshot behavior remain intact. See the
[write-path benchmark and fixed-budget regression](WRITE_PATH.md).

```rust,no_run
use e4_prototype::{collections::{CollectionOptions, Database}, Kind};
use kernel::store::Config;
use serde_json::json;

# fn example() -> Result<(), Box<dyn std::error::Error>> {
let mut db = Database::create("<scratch>", Config::default())?;
let people = db.create_collection(
    "people",
    vec![
        ("name".into(), Kind::Text),
        ("profile".into(), Kind::Json),
        ("position".into(), Kind::Point),
        ("embedding".into(), Kind::Vector(3)),
    ],
    CollectionOptions { timestamps: true }, // Default::default() means OFF
)?;
let id = db.put(people, "person/alice", &json!({
    "name": "Alice",
    "profile": {"roles": ["operator"], "sensor": null},
    "position": {"type": "Point", "coordinates": [144.75, -37.5]},
    "embedding": [0.5, 1.0, -2.0],
    "observed_at": 1788888800
}))?;
db.commit()?;
db.update(people, "person/alice", &json!({"name": "Alice Tan"}))?;
db.commit()?;
assert_eq!(db.get(people, "person/alice")?.unwrap().id, id);
for entity in db.scan(people, None)? {
    let entity = entity?; // Stream one entity at a time.
    println!("{}", entity.key);
}
# Ok(()) }
```

`open`, `open_snapshot`, `create_limited`, `collection`, `collection_info`,
`alter_collection`, `get_by_id`, `delete`, `rollback` and an exclusive scan
cursor complete the current surface. Database paths must not already contain a
store when creating. A collection must exist before writing it. A single
writer owns the handle; snapshots use independent read-only handles.

## Identity and document semantics

- `CollectionId(u32)` is database-local. `EntityId { collection, sequence }`
  is a collision-free composite; it is not a hash of an external key. These
  identifiers are not globally unique across separately created databases.
- External keys are exact UTF-8, 1–1024 bytes, scoped to a collection. Unicode,
  slashes and NUL are accepted. Collection names are 1–255 UTF-8 bytes.
- `put` is full replacement/upsert. An existing external key preserves its
  entity ID. `update` shallow-merges top-level fields and refuses a missing
  entity. Null is a value, not a deletion instruction. `delete` returns false
  for a missing key. Deleting and reinserting allocates a new sequence.
- Committed IDs are never reused after deletion. IDs returned by uncommitted
  writes are provisional and can be reused after rollback. Do not publish an
  uncommitted ID to another system as a durable identity.
- A document must be an object. Declared fields use typed slots; undeclared
  fields remain typed binary values in the extras lane. Missing and null are
  distinct. JSON text is never persisted in E4.
- `__e4_key`, `_id`, `_key` and `_collection` are reserved at the top level.
  Identity belongs to the entity envelope, not a caller-supplied payload.
- Vectors have a declared dimension and f32 lane representation. Values narrow
  to f32; exact f64 vector round trips are not promised. Points use two f64
  coordinates; spatial indexing and coordinate-system policy are separate.
- Scan order is composite entity-ID order. `after` is exclusive and must belong
  to that collection. Holding an iterator borrows its database; it cannot be
  mutated through that handle during iteration.

E3 contracts reused here include stable upsert identity, one membership per
entity, explicit missing-update failure and snapshot immutability. E3's
`db.rs` `MembershipBatch` inspired the fixed one-collection sequence buffer.
E3 source is unchanged. E4 intentionally makes collection scope explicit;
this is an adapter boundary, not source compatibility with every E3 method.

## Transactions and failure

Creation initializes and commits the database format. Collection creation,
layout changes and entity changes then share an explicit transaction. A
successful `commit()` checkpoints and publishes one durable root, making it
visible to snapshots opened afterward. A previously opened snapshot keeps its
original generation. Every collection commit currently pays publication cost;
there is no separate WAL-only public commit in this layer.

Validation and encoding happen before mutation. Input/type/oversize validation
errors leave the handle usable. Once a mutation or commit starts, an error
marks the collection handle failed: reads, scans, writes and further commits
refuse until rollback or reopen. `rollback()` discards the working tree and
reopens the last durable root, clearing metadata caches and the sequence
buffer. If reopening or validating the header fails, the handle stays failed.
An I/O failure during publication may have an uncertain commit outcome: reopen
and inspect; do not assume the failed call proves an abort. Dropping an
uncommitted handle does not acknowledge its changes.

`create_limited` uses the existing persistent kernel limits. This does not turn
caller-owned documents, codec scratch space or filesystem allocation into a
whole-process/whole-device quota. The benchmark below uses ordinary stores;
its sampled peaks are observations, not enforced limits.

## Optional timestamps

The policy is persisted at creation and immutable through this API:

| Setting | Behavior |
|---|---|
| Omitted / `timestamps: false` | No managed slots or implicit fields. User timestamp fields are ordinary data. |
| `timestamps: true` | Two typed i64 slots: `_created_unix` and `_updated_unix`. |

Clock units are Unix seconds. On insertion both fields take the clock value.
On replacement or patch, creation time stays fixed; update time is
`max(now, previous_updated, created)`, so a backward clock cannot reduce it.
An empty/no-op patch is still a write. Two writes within one second may have
identical timestamps. Managed values, including null, are refused from callers
when enabled. There is no restore override or post-creation policy toggle yet.
A custom `Clock` can be installed; negative readings are refused before mutation.
Sensor event times such as `observed_at` remain independent application data.

## Layouts, keys and bounded state

An ordered integer is `[0x80 + width][minimal big-endian bytes]`, width 1–8.
Decoding rejects truncated and noncanonical keys. The following are keyspaces
in the same B-tree, not separate databases or per-feature files:

| Key | Value |
|---|---|
| `[00,00,copy]` | Global next collection/layout IDs |
| `[00,F0] + BE64(layout_id * 3 + copy)` | Immutable dense-v3 layout descriptor |
| `[01,copy] + ordered(collection)` | Collection ID, name, active layout, timestamp policy |
| `[02,copy] + ordered(collection)` | Next sequence |
| `[10] + name` | Collection ID |
| `[20] + ordered(collection) + external_key` | Sequence (external-key index) |
| `[40] + ordered(collection) + ordered(sequence)` | Dense-v3 typed entity |
| `[60] + ordered(collection) + ordered(sequence) + ordered(slot)` | f32 vector bytes |

Rows include a hidden first text slot, `__e4_key`. This duplicates the external
key deliberately: a surviving row can recover its name without its index.
`collection_info().layout` removes that hidden slot for schema inspection; it
is not the raw encoding layout. Vectors are separate keyspace entries so future
vector operations can read them independently. SQLite may store a small vector
inline more cheaply; the benchmark allows its native inline BLOB.

`alter_collection` allocates an immutable layout ID, changes the catalog's active
version and leaves old row bytes unchanged. Readers dispatch on each row's
original layout ID; old schemas remain available. A slot's physical identity is
`(layout_id, slot)`, not its ordinal across different layouts. Future indexes
must bind field names/types with layout versions. Replacing an old entity
encodes the new layout and removes obsolete vector entries. Metadata grows with
schema changes; no schema garbage collection is implemented.

The handle caches at most one catalog and one layout, plus one pending sequence
counter. Switching collections flushes that counter; commit flushes it too.
These collection-layer caches do not grow with collection count. This bounds state but alternating
collections and reading many layouts incur extra metadata I/O and cloning.

## Corruption and recovery contract

Header, catalog, counter and layout each have three copies. Each descriptor is
2081 bytes with its own CRC32C, in addition to pager checksums; two cannot fit
in one 4096-byte leaf. The new header/catalog/counter packets contain an 8-byte
magic, BE16 payload length, bounded payload, zero padding and LE32 CRC.
Normal reads accept agreeing intact copies. One or two damaged copies can be
bypassed when their tree paths remain reachable. Conflicting valid copies are
an error, not a majority vote. Lost high-water counters are never guessed,
which avoids silently reusing committed IDs. No automatic metadata repair runs.

One bad byte invalidates its physical page (or an overflow value), potentially
losing all cells on that page. A damaged ancestor can prevent normal tree
lookup even when leaf contents survive. Multiple copies do not guarantee normal
opening after an arbitrary ancestor failure. Root-independent recovery uses
verified leaves without those ancestors:

```rust,ignore
recover_typed_candidates(source, destination, &CollectionRecovery, options)?;
```

The existing R2 exporter with `CollectionRecovery` decodes scalar rows into a
candidate envelope containing numeric collection ID, sequence, external key
and document. It preserves exact entity bytes in `records.raw` when a layout
is unavailable or a decode fails, and never modifies the source. Exports are
candidates: obsolete rows can coexist, and committed membership is not inferred.
Collection names/policy/current catalog generation are separate evidence in the
preserved source; this adapter does not rebuild or export those catalog records.
Its output must not be opened directly as a repaired live database.

**Current vector limitation:** candidate-aware vector resolution is unfinished.
Vector-bearing rows are reported unresolved with their raw row preserved;
vector cells remain in the preserved source, not in this adapter's row archive.
The underlying `CandidateReader` can inspect those cells independently. Losing
all layout copies loses automatic field-name/type decoding for affected rows.
Losing every catalog/name copy can lose the collection name/policy, while row
numeric identity and external keys remain recoverable with intact layouts.
Recovery of current membership and a bounded repair workspace remain broader
gates. This limitation is recorded rather than hidden behind the density claim.

## Validation and benchmark

**330 workspace tests pass** (0 failed, 0 ignored), including 12 new
collection tests and the inherited kernel suite. Tests cover a 1200-operation independent two-collection model; snapshots,
rollback and cursor order; timestamp ownership and backward clocks; immutable
row bytes during schema change; vector cleanup; quota failure; descriptor loss
and conflicts; and root-independent source-preserving candidate export.
Deterministic write-fault injection covers 34 positions across insert,
replacement, deletion, collection creation, layout alteration and counter
flush. Those injections operate at the Store write boundary, not arbitrary
physical writes or power-cut timing. Existing kernel checkpoint/crash tests
remain separate evidence.

The first empty implementation failed the public round-trip test. A subsequent
fault test exposed rollback leaving a damaged reopened handle usable; it failed
before the fail-closed fix and passes afterward. Red/green logs are retained in
`<scratch>/`.

See [paired collection benchmark results](COLLECTION_RESULTS.md) for the full
cost of two collection catalogs, stable IDs, external keys, JSON, points,
vectors and timestamps, including mixed-churn time and sampled disk peaks for
both engines. Earlier entry-only percentages do not describe this API's total
storage footprint.

The subsequent [delete/page-packing loop](PACKING_LOOP.md) adds bounded local
merging/redistribution to ordinary deletes and collapses empty interior roots.
Committed identity, row/layout formats and timestamp policy are unchanged.
Old snapshots retain their versions; freed pages become reusable after reader
protection permits it. This does not automatically shrink file lengths or sweep
all historical empty pages. The full collection twelve-cycle footprint now
plateaus; see the report for SQLite timing, allocated peaks, fault tests and
the remaining snapshot-budget and full-API performance gaps.
