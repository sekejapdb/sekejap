# Typed collection boundary — 2026-09-11, backend switched 2026-09-16

The internal Rust API is `e4_prototype::collections::Database`. It layers a
persistent catalog, external-key index and typed CRUD over the V2 page-WAL
store (`pagewal::PageWalStore`, `E4PWAL02`; selected in `src/store/mod.rs` (the
layout restructure at `f5e4c7e` moved this out of `src/collection_backend.rs`;
re-exported under that name still), contract in
[V2_COLLECTION_INTEGRATION.md](V2_COLLECTION_INTEGRATION.md)). The inherited
kernel `Store` is no longer a collection backend. Statements below that name
the old engine's behaviour were rewritten in the 2026-09-16 pass; the key,
record, layout and catalog encodings are unchanged. This is the boundary for
adapting the prior engine's interfaces -- the release sekejap replaces, whose
sources are kept on branch `e1`. It does not implement HTTP or a deployment
package; Tier-1 SQL now has a parser and compiler (`lang/src/`,
`docs/lang/QL_CONTRACT.md`), and scalar indexes and graph adjacency are both
live (`src/collections/catalog.rs`, `src/index/graph/mod.rs`). The eight laws
in CONTRACT.md are unchanged; this slice is not a claim that all have passed,
and the backend switch is unvalidated until the parent's Linux run.

## API example

Mutations compare existing vector bytes by layout field ordinal and write only
changed sidecars. Scalar-only updates retain unchanged embeddings; validation,
stable identity and snapshot behavior remain intact. See the
[write-path benchmark and fixed-budget regression](WRITE_PATH.md).

<!-- doc_example: collections_typed_collection -->
```rust
use sekejap::core::collections::{CollectionOptions, Database};
use sekejap::core::Kind;
use sekejap::Db;
use serde_json::json;

let dir = std::env::temp_dir().join("sekejap-collections-example");
let _ = std::fs::remove_dir_all(&dir);
// `Db::config()` is `SyncMode::Full`, which is the one barrier the
// page-WAL store accepts: a weaker mode is refused, never downgraded.
let mut db = Database::create(&dir, Db::config())?;

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
// A field the declaration does not name -- `observed_at` -- is kept in
// the row's extras map: a declaration is a floor, not a fence.
let id = db.put(
    people,
    "person/alice",
    &json!({
        "name": "Alice",
        "profile": { "roles": ["operator"], "sensor": null },
        "position": { "type": "Point", "coordinates": [144.75, -37.5] },
        "embedding": [0.5, 1.0, -2.0],
        "observed_at": 1788888800,
    }),
)?;
db.commit()?;

// A shallow merge. Identity survives it, and the unchanged vector
// sidecar is not rewritten.
db.update(people, "person/alice", &json!({ "name": "Alice Tan" }))?;
db.commit()?;
let row = db.get(people, "person/alice")?.expect("the row");
assert_eq!(row.id, id);
assert_eq!(row.document["name"], json!("Alice Tan"));
assert_eq!(row.document["embedding"], json!([0.5, 1.0, -2.0]));

// The managed fields are the engine's, written because this collection
// was created with `timestamps: true`.
assert!(row.document.get("_created_unix").is_some());
assert!(row.document.get("_updated_unix").is_some());

// One entity at a time: a scan never holds the collection.
for entity in db.scan(people, None)? {
    let entity = entity?;
    println!("{}", entity.key);
}
Ok(())
```

`open`, `open_snapshot`, `create_limited`, `collection`, `collection_info`,
`alter_collection`, `get_by_id`, `delete`, `rollback`, `checkpoint`,
`limits`, `storage_bytes`, `tracked_pages` and an exclusive scan cursor
complete the current surface. Database paths must not already contain a
store when creating; `open` refuses a missing path. `Config` must request
`IoMode::Buffered`, `SyncMode::Full` and at least a 64 KiB budget — anything
else is refused (`Error::Unsupported`) before any file is touched, so every
run is FULL-barrier by declaration. A collection must exist before writing
it. A single writer owns the handle; snapshots are independent read-only
handles that may be opened by path in this or another process.

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
  distinct. JSON text is never persisted here.
- `__e4_key`, `_id`, `_key` and `_collection` are reserved at the top level.
  Identity belongs to the entity envelope, not a caller-supplied payload.
- Vectors have a declared dimension and f32 lane representation. Values narrow
  to f32; exact f64 vector round trips are not promised. Points use two f64
  coordinates; spatial indexing and coordinate-system policy are separate.
- Scan order is composite entity-ID order. `after` is exclusive and must belong
  to that collection. Holding an iterator borrows its database; it cannot be
  mutated through that handle during iteration.

Contracts reused from the prior engine include stable upsert identity, one
membership per entity, explicit missing-update failure and snapshot
immutability. Its `db.rs` `MembershipBatch` inspired the fixed one-collection
sequence buffer. That source is unchanged. sekejap intentionally makes
collection scope explicit; this is an adapter boundary, not source
compatibility with every method the earlier design had.

## Transactions and failure

Creation initializes and commits the database format. Collection creation,
layout changes and entity changes then share an explicit transaction. A
successful `commit()` writes the transaction's page images to the page-WAL,
issues a FULL barrier, then publishes it through the two-copy hint in
`readers.lock`; only then is it visible to snapshots opened afterward. A
previously opened snapshot keeps its exact published prefix for its whole
life. `checkpoint()` folds the published WAL into the data file and resets
the WAL; it runs automatically at 4 MiB of WAL (or half an allowance) and
returns `false`, deferring, while any reader in any process holds a slot.
One transaction's page images must fit the WAL bound (16 MiB, or the
policy's `wal_bytes`); a larger one refuses and fails the handle.

Validation and encoding happen before mutation. Input/type/oversize validation
errors leave the handle usable. Once a mutation or commit starts, an error
marks the collection handle failed: reads, scans, writes and further commits
refuse until rollback or reopen. `rollback()` discards the working tree in
place: it re-inspects the durable files exactly as a reopen would, truncates
only bytes past the last complete commit, barriers and republishes that
prefix, and clears metadata caches and the sequence buffer. Live snapshots
are unaffected. If validating the header fails afterwards, the handle stays
failed. An I/O failure during publication may have an uncertain commit
outcome, but always a coherent one: if the barrier or the first hint copy
failed, nobody sees the transaction until `rollback()` or reopen republish
the complete durable frame; if the first hint copy landed and the second
failed, every reader and the writer's own bookkeeping already see it and only
the failed `commit()` call is left to resolve. The failed call is not proof
of an abort. Dropping an uncommitted handle does not acknowledge its changes.

`create_limited` persists the exact `ResourceLimits` (kernel `E4LIMIT1`
record) in the collection header and enforces every field on the page-WAL
path: `data_bytes` and `wal_bytes` before each page write/append (their sum
is also the persisted page-WAL cap), `tracked_pages` at every distinct
addition to the WAL index between checkpoints, `record_bytes` before
mutation, `readers` by reader slot index across processes, and
`recovery_bytes` by never being written into. `wal_bytes > 16 MiB` and
`readers > 8` are refused before the directory exists. Runtime allowances
are reinstalled at every open and after rollback. The 96 logical bytes of
`readers.lock` are charged against the policy's freelist allowance; the slot
files are zero bytes (inode cost only). This does not turn caller-owned
documents, codec scratch space or filesystem allocation into a
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
| `[00,00,copy]` | Global next collection/layout IDs (8-byte payload; `create_limited` databases append the 56-byte `E4LIMIT1` policy; any other length is refused as unsupported before any write) |
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

```rust,signatures
recover_typed_candidates(source, destination, &CollectionRecovery, options)?;
```

The existing R2 exporter with `CollectionRecovery` reads the page-WAL source
through `pagewal::candidate_reader`, which overlays the committed WAL on the
data file read-only (rows acknowledged but not yet checkpointed are
candidates too; an uninterpretable WAL falls back to the bare data file and
is recorded in `issues.jsonl`). It decodes scalar rows into a candidate
envelope containing numeric collection ID, sequence, external key and
document. It preserves exact entity bytes in `records.raw` when a layout
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
`<artifact dir>/collections-20260911/`.

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
