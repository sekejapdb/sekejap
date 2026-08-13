# Phase 0 Spec — Dense-ID, Offset-Addressable Topology Format (v2)

> **Superseded (0.17.0):** object-storage support was removed; the S3 phases
> described below are no longer planned. The local mmap design still stands.
Status: **shipped**. `compact()` writes these files and `open()` reads them
(recovery when the snapshot is missing, and the paged-topology mode). The format
was locked before wiring so that mmap paging (Phase 1) and S3 paging (Phase 2)
stay read-path flips, not data migrations; forward/reverse CSR adjacency is now
also servable as memory-mapped slices.

Goal it unblocks: **1 billion nodes on 1 GB RAM** for bounded queries, by making
topology an offset-addressable file the OS/page-cache can page — the way
Neo4j/Arango/etc. already work, but able to run over local mmap *or* S3.

---

## 1. The core idea: a dense internal id

Today identity is `hash(slug) → u64`, used everywhere (incl. `Edge.other`). Sparse
hashes force a `hash → location` index lookup on **every hop** at scale, and that
index thrashes on small RAM. Fix: introduce a **dense internal id** as the
*physical* identity.

- `dense_id ∈ [0, n)` assigned sequentially at compaction.
- Physical location is arithmetic: `node_record(k) = NODES_HEADER + k * NODE_RECSIZE`.
  Traversal is **index-free** (direct seek) and cache-friendly.
- Sequential ids **never collide** → the u64-hash birthday ceiling disappears.
- **API is unchanged:** callers still `link(from_slug, to_slug)`. Dense ids are
  assigned internally at compaction. `hash(slug) → dense_id` is looked up only at
  query **roots** and at **link time** (once per endpoint, never per hop).

**Id width — no fixed ceiling.** A dense id is *logically* `u64` (unbounded node
count). It is *stored* compactly where it matters: **StreamVByte-delta in the edge
lists** (§3.2 — where ids are numerous and delta-friendly → ~1–2 bytes each) and
**fixed `u64` in `idx.bin`** (§3.3 — where binary search needs fixed width, and it's
touched only at roots). So sekejap gets Dgraph/Neo4j-class *unbounded* capacity with
*better-than-`u32`* edge size — no `wide_id` flag, no 4.29 B limit.

---

## 2. Files (all offset-addressable, all versioned)

Every file starts with the same 16-byte header as `snapshot.json` already uses:
`[magic 8B][format_version u32][flags u32]`.

| File | Contents | Indexed by |
|---|---|---|
| `payloads.bin` | SKBIN records (schema-aware binary; exists today) | `(offset, len)` from node record |
| `nodes.bin` | fixed-size node records | dense id (arithmetic) |
| `adj_fwd.bin` | CSR: offset array + edge array (outgoing) | dense id |
| `adj_rev.bin` | CSR: offset array + edge array (incoming) | dense id |
| `edgemeta.bin` | sparse edge metadata blobs (optional) | `(offset, len)` from edge record |
| `idx.bin` | sorted `(hash, dense_id)` for name resolution | binary search on hash |
| `slugs.bin` | `dense_id → slug` string store (reverse of idx) | offsets array + blob |
| `dict.bin` | collection-name and edge-type dictionaries | id → string |

`magic`s: `SKNODE\0\0`, `SKADJF\0\0`, `SKADJR\0\0`, `SKIDX\0\0\0`, `SKDICT\0\0`.

---

## 3. Record layouts (byte-level)

### 3.1 `nodes.bin` — NodeRecord (fixed 24 B), array indexed by dense id
```
off  0  payload_offset  u64   → into payloads.bin
off  8  payload_len     u32
off 12  collection_id   u32   → into dict.bin collection table (u32::MAX = none)
off 20  spatial_ref     u32   → into a spatial side-table (u32::MAX = none)
off 22  flags           u16   → bit0 deleted (tombstone), bit1 has_rev, ...
        (pad to 24)
```
Record for dense id `k` lives at `header_len + k * 24`. Deleted nodes keep their
slot (tombstone) until the next compaction reassigns ids.

### 3.2 `adj_fwd.bin` / `adj_rev.bin` — CSR adjacency (StreamVByte-delta neighbors)

Neighbor ids are **sorted, delta-encoded, and StreamVByte-packed** — compact
(typically 1–2 bytes/neighbor), SIMD-fast to decode, and **ceiling-free** (a variable
width grows as needed → no fixed node-count limit, like Neo4j's dynamic pointer
compression / Dgraph's delta posting lists). Node ids are therefore *logically* `u64`
(unbounded) but *stored* compactly. See the sidebar below for the intuition.

```
[16B header]
[sub-header: node_count u64, block_region_start u64]
[offsets:  u64 × (node_count + 1)]     // offsets[k] = byte offset of node k's edge block
[edge blocks, one per node, variable length]
```
Per-node **edge block**:
```
[count            varint]                      // number of edges for this node
[neighbor deltas  StreamVByte(count)]          // sorted absolute ids → deltas → SVB
[edge_type_ids    u32 × count]                 // parallel array → dict.bin
[attr columns     …      × count]              // parallel; one fast-lane column per primitive attribute
[meta_refs        u32 × count]                 // parallel; u32::MAX = none → edgemeta.bin
```

> **Note:** this spec originally reserved a single fixed `strength: f32` column.
> The implemented runtime (`src/storage/edgestore.rs`) generalized that to
> *fast-lane columns keyed by attribute name*, one per primitive attribute, routed
> by value type — `strength` is no longer privileged, it's just one possible column.
> The on-disk v2 layout should follow suit (a small column directory instead of a
> single `strengths` array) when this format is built.
Read node `k`'s edges: seek `offsets[k]`, read `count`, **StreamVByte-decode** the
`count` neighbor deltas (4 ids/step, no branching) and prefix-sum to absolute dense
ids, then read the parallel `type/attr/meta` arrays in the same order.

Neighbor ids get the big compression (numerous + delta-friendly). The parallel
attribute arrays stay fixed for now; dict/RLE/bitmap compression of the
`type` / attr / `meta` arrays is an optional later pass.

> **Sidebar — why StreamVByte-delta is small *and* fast.**
> A node's neighbors are sorted, so store the *gaps* not the absolute ids
> (`1000, 1005, 1012` → `1000, +5, +7`) — gaps are tiny → 1 byte each.
> Then, instead of a slow length-marker bit inside each number, **StreamVByte splits
> the stream in two**: a *control* stream (2 bits/int = "this id used 1/2/3/4 bytes")
> and a *data* stream (the bytes). The CPU reads the control byte and decodes **four
> ids at once with SIMD, branch-free** — near the speed of a plain array read, at a
> fraction of the size. Small to store, quick to read, and it never runs out of ids.

### 3.3 `idx.bin` — hash → dense id (name resolution)
```
[16B header]
[count u64]
[entries: (hash u64, dense_id u64) × count]   // sorted by hash, fixed-width for binary search
```
`resolve(slug)`: `binary_search(idx, hash(slug))`. Collisions (same hash, different
slug) are disambiguated by reading the candidate node's `slug` from its payload and
comparing — replaces the standalone collision check and makes it *loud* on mismatch.
Touched only at roots, so paging it is cheap.

**Sparse index (resident):** binary search over a 12 GB `idx.bin` (1 B nodes) is ~30
scattered page touches when cold. Keep a small **RAM-resident sparse index** — every
Nth hash (~10–50 MB for a billion) — so each resolve narrows to a single 4 KB page:
**~1–2 faults/root instead of ~30**, and it fits a Pi. Built at load from `idx.bin`.

### 3.3b `slugs.bin` — dense id → slug (the reverse mapping)

```
[16B header]
[count u64]
[offsets: u64 × (count + 1)]      // slug k = blob[offsets[k]..offsets[k+1]]
[blob: concatenated UTF-8 slugs]
```

Hashes are one-way, so the slug *string* must be stored somewhere to (a) return
`_key`/`_id` in results, (b) disambiguate hash collisions in `idx.bin`, and
(c) rebuild identity at open without parsing payloads. This is the same pattern as
Neo4j's property store / Arango's `_key`-in-document: **names live beside, never
inside, the traversal records** — `slugs.bin` is touched at result-building and
resolution time only, never during hops. Offset-addressable → mmap/page-friendly.

### 3.4 `dict.bin` — dictionaries
```
[16B header]
[collections: len-prefixed strings, id = index]
[edge_types:  len-prefixed strings, id = index]
```
Small (hundreds of entries), always resident.

---

## 4. Dense-id assignment (happens in `compact()`)

Compaction already streams live nodes to `payloads.bin.tmp`. Extend it to also
build the topology files:

1. **Assign ids.** Iterate live nodes (skip tombstones) → `dense_id = 0,1,2,…`.
   Build `slug_hash → dense_id` map in memory (or spill-sorted for huge sets).
2. **Write `nodes.bin`.** For each id in order, write its NodeRecord (payload
   offset/len from the streamed payload rewrite; collection_id via dict).
3. **Write `dict.bin`.** Intern collection + edge-type strings → ids.
4. **Write CSR (`adj_fwd`, `adj_rev`).** For each node id in order, emit its edges
   (translating each neighbor `hash → dense_id`, `type → type_id`); fill the
   offsets array as you go. Sort each node's edges by neighbor id for locality.
5. **Write `idx.bin`.** Emit `(hash, dense_id)` sorted by hash.
6. **Atomic rename** all `*.tmp → *` (same pattern as payloads/snapshot today).

Cost: one streaming pass over live nodes + edges — O(n + m), same order as today's
compaction. No extra RAM beyond the id map (which can spill-sort for billion-scale).

---

## 5. Versioning & migration

- `SNAPSHOT_FORMAT_VERSION` is currently **2** (JSON + header). This format is **v3**.
- `open()` reads the snapshot header (already implemented). Dispatch:
  - `v1/v2` → today's JSON snapshot → build in-RAM `EdgeStore` (current path).
  - `v3` → the topology files above.
- **Migration is lossless and automatic.** A v2 JSON snapshot stores edges *by slug*
  (`SnapEdge{from,to,…}`), so opening a v2 DB, then `compact()`, emits v3 topology
  files — no re-import, no data loss. Old binaries refuse v3 cleanly (header check).
- Everything in v3 is **re-derivable** from `payloads.bin` + edge list, so a future
  change (u32→u64 ids, layout tweak) is another `compact()`-rebuild, not a break.

---

## 5b. Compression & crash-safety (foundational decision)

- **Topology stays raw** (fixed-size records + CSR) — traversal is direct-seek, so
  raw = fastest. Dense `u32` ids already make it compact (4 B/neighbor). A delta/varint
  CSR to shrink it further is an *optional later* pass, not v1.
- **Payloads carry the encoding, not topology.** Payloads are the size bulk; they use
  **SKBIN** (schema-aware binary, size + recoverability). A per-record first-byte tag
  (`raw` `{` vs `0x02` SKBIN) lets `compact()` transcode incrementally and old raw records
  coexist. Whole-payload block-zstd was evaluated and dropped (worse ratio on real data,
  no faster, shared dictionary breaks the ≤1-record blast radius). See
  [payload-binary-format.md](skbin-format.md).
- **Compression is invisible to crash recovery.** The **WAL is uncompressed**; all
  checkpoint files (topology + payloads) are written **atomically** (tmp → `fsync` →
  rename) and are **rebuildable from WAL + payloads**. A blackout mid-write leaves the
  old files intact; recovery replays the uncompressed WAL. So "super compressed" and
  "blackout-safe" are not in tension here.

---

## 6. What this version ships vs. defers

**Ships (Phase 0 — now live):**
- Writes v3 topology files at `compact()`; reads them at `open()` (recovery and
  paged mode).
- Resident mode still loads them into the in-RAM `EdgeStore`/`nodes` map → same
  speed, zero user-visible change; edge spill additionally serves CSR adjacency
  straight from mmap.
- Dense-id + `idx.bin` name resolution replaces raw-hash identity internally.
- Collision handling folded into `idx.bin` resolution (loud on hash+slug mismatch).

**Defers:**
- **Phase 1 (mmap):** stop copying into RAM; read NodeRecord/CSR *directly* from the
  mmap. OS page cache → billions on 1 GB for bounded queries. Reuses payload mmap.
- **Phase 2 (S3):** back the topology block reads with the existing `BlockCache`.

---

## 7. Integration points (where the code changes)

- `src/lib.rs`: `NodeData` → gains `dense_id` / or the RAM map becomes `Vec<NodeRecord>`
  indexed by id; `compact()` writes the new files; `open()` dispatches on v3.
- `src/storage/edgestore.rs`: `EdgeStore` gains a v3 loader (from CSR files) and, in
  Phase 1, a mmap-backed variant (`fwd(k)` returns a slice of the mapped edge array).
- New module `src/storage/topology.rs`: the file formats, `NodeStore`, `AdjStore`
  (CSR), `IdIndex` (idx.bin), `Dict` — mirroring `PayloadStore`'s backend pattern.
- `src/query.rs`: `db.fwd_edges(id)` / `rev_edges(id)` become dense-id based; root
  resolution goes `slug → hash → dense_id` via `IdIndex`.

---

## 8. Decisions (locked ✅ / open)

1. ✅ **Neighbor ids = StreamVByte-delta** (§3.2) — logically `u64`/unbounded, stored
   ~1–2 B/neighbor, SIMD-decoded. No `u32` ceiling, no `wide_id` flag. `idx.bin` keeps
   fixed `u64` for binary search.
2. **Writes between compactions** — CSR is read-optimized. Options: (a) buffer new
   edges in a RAM overflow + periodic compact (simplest); (b) append-only edge log
   merged at read. Recommend (a) for Phase 0/1; WAL already gives durability. *(open)*
3. ✅ **Edge meta sparse** — `meta_ref = u32::MAX` sentinel + `edgemeta.bin`; never inline.
4. ✅ **Spatial meta → `spatial_ref` side-table** so `NodeRecord` stays fixed-size.
5. ✅ **`idx.bin` = sorted array + binary search + a resident sparse index** (§3.3);
   on-disk hash table deferred.
6. ✅ **Topology neighbor ids StreamVByte-delta; payloads SKBIN** (schema-aware binary;
   block-zstd was evaluated and dropped for payloads). Attribute arrays (type/attr/meta)
   and any further CSR compression are optional later passes.
7. ✅ **Access = `mmap`** (OS page cache), not a hand-rolled buffer pool; `BlockCache` for S3.

---

## 9. Why this is the right foundation

- It's **Neo4j's proven layout** (fixed-size records + direct offsets, index-free
  adjacency) — but generalized so the *same* format is served by local mmap **or**
  the S3 block cache, which no embedded engine does.
- It **reuses** what exists: the versioned header, `payloads.bin` mmap, the S3
  `BlockCache`, the streaming `compact()`, the WAL.
- It keeps sekejap's **API and hot-path speed** (bounded queries stay µs) while
  removing the two ceilings at once: **RAM-for-topology** (now paged) and
  **hash collisions** (now dense sequential ids).

The single irreversible decision is **the on-disk record layout in §3** — lock that
now, and Phases 1–2 are additive.
