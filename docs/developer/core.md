# Core — CoreDB's lifecycle

`CoreDB` (src/lib.rs) is the single-writer embedded core: everything else —
SQL front-end, network adapters, bindings — is a layer over it. This page is
the write path, durability, recovery, and compaction.

## The write path

Every mutation follows the same discipline:

1. **Append to the WAL** (write-ahead log) — the durability record.
2. **Update resident state** — the node map, affected indexes; payload bytes
   go to `payloads.bin`, vectors to their per-field store, edges to the edge
   store.

A single `put` costs one WAL append + one fsync at the default sync level.
That fsync dominates bulk loads, so batching paths defer it:

- `begin_bulk()` / `end_bulk()` — one fsync for the whole scope.
- `put_many` / `link_many` / `ingest` — batched variants of the same idea.
- SQL transactions (`BEGIN … COMMIT`) — batch semantics, sync at commit.

Real effect: ingesting a 57k-document corpus with 1536-d vectors dropped from
352 s (per-write sync) to 2 s (bulk scope). Per-record CRCs are kept either
way — deferral batches the sync, not the integrity checks.

## Durability levels

The WAL frames every entry with a CRC. How hard "committed" is depends on the
sync level: an OS-buffered write survives a process crash; a full fsync
(`File::sync_data`, on macOS an `F_FULLFSYNC`-class barrier) survives power
loss. Measured costs and cross-engine comparisons: 
[notes/durability-benchmarks.md](notes/durability-benchmarks.md).

## Opening a database

`open()` is a cascade, fastest source first:

1. **Snapshot present** → load the manifest (`snapshot.json`: schemas, index
   declarations, HNSW/btree metadata, view definitions) + binary topology
   sidecars, then replay whatever WAL tail postdates them.
2. **Snapshot missing, topology files present** → rebuild identity and
   adjacency from the topology sidecars (recovery path).
3. **Only the WAL** → stream-replay it entry by entry (never loading the
   whole log into memory).

Derived indexes (btree, GIN, BM25, HNSW, spatial grid) load from sidecars when
present, or rebuild from payloads + schema. They are never the only copy of
user data.

Two open modes matter:

- `CoreDB::open` — resident: node map and hot indexes in RAM.
- `CoreDB::open_paged` — paged: identity and topology served from the mmap'd
  sidecars, node map left empty; the mode behind serving 10M rows in ~8.5 MB
  of anonymous memory.

## Compaction

`compact()` rewrites live state into fresh, dense files:

- streams live payloads into a new `payloads.bin` (records ≤64 KB are
  re-encoded as SKBIN — see [storage.md](storage.md)),
- writes the topology sidecars (dense ids + CSR adjacency),
- writes a fresh `snapshot.json`,
- truncates the WAL.

It streams; it does not load the database into memory to rewrite it. Run it
after bulk imports — reopen goes from WAL-replay time to snapshot-load time.
`trim_memory()` additionally shrinks resident capacity (also runs inside
compact).

## The engine wrapper (optional)

`engine/` (feature-gated) wraps `CoreDB` for concurrent use: RwLock reads in
parallel, exclusive writes held only for the mutation, write buffering, a
result cache, an index-build scheduler, and snapshot publication to object
storage. The core stays single-writer; the wrapper only orchestrates.

## Invariants

Before touching `open()`, the write path, or compaction, read the startup and
memory checklists in [invariants.md](invariants.md).
