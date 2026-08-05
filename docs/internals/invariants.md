# sekejap — Architectural Invariants

This document defines the non-negotiable performance contracts for sekejap.
Every code change must be evaluated against these invariants before being applied.
A change that satisfies one invariant while breaking another is not acceptable.

---

## The Three Pillars

| Pillar | Contract |
|--------|----------|
| **Fast startup** | `open()` completes in < 1s regardless of dataset size |
| **Disk-first memory** | RAM usage ∝ metadata + indexes, never ∝ payload size |
| **Lightspeed queries** | Result cost ∝ result size, not dataset size |

These are not independent goals. They share one root design:
**payloads live on disk; everything else lives in RAM.**

---

## Pillar 1 — Fast Startup

### What `open()` must do

```
1. load_snapshot()       — deserialize metadata only (no payloads)
2. replay_all()          — stream WAL one frame at a time
3. rebuild_spatial_grid()— reads NodeData.spatial_meta (RAM, no disk)
4. GIN decision          — load gin.bin or rebuild (see rules below)
5. HNSW decision         — rebuild only if vectors changed
```

### GIN decision tree (strict)

```
WAL had Put/Remove/PutVector?
  YES → rebuild_declared_gin_indexes() + save gin.bin
  NO  → load_gin_binary()
          FAIL? → rebuild_declared_gin_indexes() + save gin.bin
          OK   → done
```

**Key rule:** `Link`, `LinkMeta`, `Unlink` WAL entries do NOT trigger GIN rebuild.
Edges have zero effect on text content.

### HNSW decision

```
WAL had Put/Remove/PutVector?
  YES → rebuild_declared_hnsw_indexes()
  NO  → nothing (HNSW is already current)
```

**Key rule:** HNSW rebuild must NEVER happen inside `remove_raw()` during WAL replay.
The entry-point-deletion fix must only trigger during live mutations, not replay.
During replay, the full HNSW rebuild at the end of `open()` handles it.

### Migration threshold

- The snapshot migration (rewrite if file is large) threshold is **500 MB**.
- A normal snapshot for any realistic dataset (up to ~500k nodes) is < 150 MB.
- The legacy bloated snapshot (gin_indexes embedded as JSON) was 1–10 GB.
- **Never lower this threshold below 500 MB.**
- The migration writes with `to_vec_pretty`, which makes files larger — do not make
  the threshold so low that the migration re-triggers on its own output.

### Startup checklist (before any change to `open()`)

- [ ] Does the change read any payload from disk? If yes: it breaks Pillar 1.
- [ ] Does the change call `build_gin_index()`? Allowed only when `wal_had_payload = true`.
- [ ] Does the change save to `snapshot.json`? If yes: will the file stay < 500 MB?
- [ ] Does the change iterate all nodes? That is O(N) work, acceptable only once (spatial grid).

---

## Pillar 2 — Disk-First Memory

### Where data lives

| Data | Location | Access pattern |
|------|----------|----------------|
| Node payload (JSON) | `payloads.bin` | pread on demand |
| Node metadata (slug, offsets, collection, spatial_meta) | RAM (`HashMap<u64, NodeData>`) | always |
| Field indexes (btree) | RAM | always |
| GIN trigram index | RAM (rebuilt from `gin.bin`) | always |
| BM25 index | RAM | always |
| HNSW graph | RAM (from `snapshot.json`) | always |
| Spatial grid | RAM | always |
| Raw WAL entries | Disk | streamed, not buffered |

### Payload access rules

1. **Never load all payloads into RAM** — not at startup, not in `compact()`, not in queries.
2. **`get_payload(hash)`** — reads one payload from disk (pread). Use when you need a single node.
3. **`get_payload_raw(hash)`** — returns raw bytes + offset/len. Use for fast-path extraction.
4. **`get_payload_head_tail(hash, head, tail)`** — reads first 512B + last 16KB without full load.
   Use when: payload > 64 KB and you only need simple scalar fields.
5. **`extract_fields_by_search(bytes, fields)`** — regex-free field extraction from tail bytes.
   Safe for mid-blob bytes; returns empty for object/array values.

### Fast-path threshold

`FAST_PATH_THRESHOLD = 64 KB`. Payloads above this use head+tail reads in:
- `collect()` for SELECT field projection
- `Step::Sort` for sort key pre-computation

### Memory checklist (before any new feature)

- [ ] Does the feature hold a `Vec<Value>` or `Vec<String>` proportional to node count?
      That is an O(N × payload_size) allocation. Redesign using hashes only.
- [ ] Does the feature store anything new in `snapshot.json`?
      If it can be recomputed from payloads.bin, it should be a binary sidecar.
- [ ] Does `compact()` load all payloads at once?
      It must stream: write `payloads.bin.tmp` node-by-node, then rename.
- [ ] Does WAL `replay_all` accumulate entries?
      It must stream: one frame read → one frame applied → memory released.

---

## Pillar 3 — Lightspeed Queries

### The four query paths

```
1. Set path     (atomic chainable API + SQL SELECT FROM collection)
2. MATCH path   (SQL SELECT FROM MATCH — graph traversal + aggregation)
3. MATCH+WITH   (multi-stage graph queries with WITH chaining — row-based executor)
4. Shortest     (SQL SELECT FROM MATCH SHORTEST — BFS single path)
```

Path 2 and 3 are unified under `query()` — the WITH chaining path activates
automatically when the parser detects `WITH` stages in a `SELECT FROM MATCH` query.
The `SELECT FROM MATCH ... WITH ...` form produces the internal `MatchAggStmt`.

Each path has a fast-path that avoids payload reads where possible.

### Set path fast paths

**Index seed (btree):** `btree_seed()` in `execute()` — if a WhereEq step has a btree
index on the collection field, the entire candidate set comes from the index directly.
The WhereEq step is then skipped in the filter loop (`skip_set`).

**Index-only GROUP BY:** `try_index_only_group_by()` — if:
- GROUP BY is a single indexed field
- Every SELECT is that field or `COUNT(*)`
- A btree index exists for `(collection_hash, field)`

Then: iterate `field_indexes[(coll, field)]` directly — zero payload reads.

**COUNT(*) fast path:** If all aggregated fields are `COUNT(*)`, use `hashes.len()`
directly — zero payload reads, zero GIN/BM25 involvement.

**Large-payload SELECT:** For payloads > 64 KB with plain field references,
use `get_payload_head_tail` + `extract_fields_by_search` instead of full `get_payload`.

**Sort pre-computation:** `Step::Sort` collects sort keys in one O(N) pass
(using the fast-path for large payloads), then sorts with cached values.
**Never call `get_payload()` inside a sort comparator** — that is O(N log N) disk reads.

### MATCH path fast paths

**Topology before payloads:** `collect_raw_paths()` runs the graph traversal
(BFS/DFS) using only adjacency maps — zero payload reads. Returns `Vec<RawPath>`
containing only hashes and slugs.

**GROUP BY fast path in MATCH:** When:
- GROUP BY is present
- No SUM/AVG/MIN/MAX (COUNT only)
- Start variable is not in GROUP BY

Three-phase algorithm:
1. **Hash-keyed grouping** — group raw paths by `dest_per_hop` hash tuple. Zero payload
   reads. For 81,912 village→province paths: 34 groups, no disk I/O.
2. **dest_where filtering** — load payload for each *unique* dest hash (not per raw path),
   sorted by offset for sequential I/O. Uses `get_payload_head_tail` for large payloads
   (reads 16 KB head+tail instead of full 2–12 MB GeoJSON).
3. **Field-value merging** — group by field values from the cache. Different dest hashes
   with identical GROUP BY values are correctly merged.

**Large-payload fast path inside GROUP BY:** For payloads > 64 KB and simple returns
(Field/Count/Now), uses `get_payload_head_tail(h, 512, 16384)` +
`extract_fields_by_search` to get scalar fields without parsing the full JSON blob.
This reduces 34 × 2 MB reads (146 s cold) to 34 × 16 KB reads (< 5 ms).

**Batch multi-depth traversal:** `collect_raw_paths()` uses flat pair propagation
for depth > 1: `Vec<(partial_idx, current_hash)>` expanded in batch per BFS level.
Cost is O(edges), not O(starts × local_edges).

### Query checklist (before any new query feature)

- [ ] Does the new query path call `get_payload()` inside a loop that runs once per
      input node (not per result node)? That is O(N) disk reads. Use topology first.
- [ ] Does the new sort/order-by call any I/O inside a comparator? Pre-compute keys.
- [ ] Is there an existing btree/GIN/BM25 index that could answer this query faster?
      Check before adding a new scan.
- [ ] For GROUP BY: can the answer come from index shape alone (btree: value → count)?

---

## Index Maintenance Rules

### On `put_raw()` (insert/update)

| Index | Action |
|-------|--------|
| `nodes` HashMap | insert/replace |
| `collections` | insert hash |
| `field_indexes` (btree) | insert/replace by field value |
| `gin_indexes` | incremental update if field changed (via `is_update` flag) |
| `bm25_indexes` | delete old + insert new (incremental) |
| `spatial_grid` | incremental remove + insert |
| `hnsw_indexes` | NOT updated here — updated via `put_vector()` |
| WAL | `WalEntry::Put` written |

### On `remove_raw()` (delete)

| Index | Action |
|-------|--------|
| `nodes` HashMap | remove |
| `slug_map` | remove |
| `collections` | retain (filter out hash) |
| `field_indexes` | retain (read payload once to get old key) |
| `gin_indexes` | **not updated** — belt-and-suspenders filter in `gin_ilike()` |
| `bm25_indexes` | incremental delete via `bm25_idx.delete(hash)` |
| `adj_fwd` / `adj_rev` | cascade remove both directions |
| `spatial_grid` | `grid.remove(hash)` |
| `vectors` | remove from all field maps |
| `hnsw_indexes` | **only** if deleted node == entry point → full rebuild |
| WAL | `WalEntry::Remove` written |

**Critical rule for `remove_raw()` during WAL replay:**
The HNSW entry-point rebuild in `remove_raw()` is correct for live mutations.
But when called from `replay()` (during `open()`), it can trigger O(N log N) HNSW
rebuilds for each Remove entry in the WAL. Guard it:

```rust
// Only rebuild HNSW during live mutation (not WAL replay).
// The open() path handles HNSW rebuild once at the end.
if !self.replaying {
    // ... HNSW entry-point check
}
```

Or alternatively: make `remove_raw()` accept a `during_replay: bool` flag.

### GIN delete semantics

GIN is not incrementally updated on delete. Instead, `gin_ilike()` filters
results against `self.nodes.contains_key(h)`. This is intentional:
- GIN rebuild from scratch on every delete is O(N × payload_reads) — unacceptable.
- The filter is O(results) — cheap.
- The stale entries are cleaned up at next `compact()`.

---

## Storage Layout — Canonical

```
DB_DIR/
├── payloads.bin    — node payloads, SKBIN records (disk-first store; see payload-binary-format.md)
├── snapshot.json   — metadata ONLY: slugs, offsets, edges, schemas, hnsw, btree
├── gin.bin         — GIN trigram index (binary, RoaringBitmap, compact)
└── wal.log         — append-only WAL
```

### Rules

1. **`snapshot.json` contains ONLY** what cannot be cheaply recomputed:
   node metadata, edge adjacency, schemas, HNSW graphs, btree index shape.
2. **GIN → `gin.bin`** sidecar only. Never embed in snapshot.json.
3. **Any new index type → its own binary sidecar.** Ask: "can this be a separate file?"
   before adding any field to Snapshot struct.
4. **`serde_json::from_reader` + `BufReader`** for snapshot loading.
   Never `std::fs::read` + `from_slice` (loads entire file into RAM).

---

## WAL Entry Classification

| Entry type | Sets `wal_had_payload` | Sets `wal_had_graph` | GIN rebuild? | HNSW rebuild? |
|------------|----------------------|---------------------|--------------|---------------|
| `Put` | ✓ | — | YES | YES |
| `Remove` | ✓ | — | YES | YES |
| `PutVector` | ✓ | — | NO (no text) | YES |
| `Link` | — | ✓ | NO | NO |
| `LinkMeta` | — | ✓ | NO | NO |
| `Unlink` | — | ✓ | NO | NO |
| `CreateTable` | — | — | NO | NO |
| `CreateIndex` | — | — | applied inline | applied inline |
| `AlterTable` | — | — | NO (schema only) | NO |
| `DropTable` | — | — | NO | NO |

---

## Common Regression Patterns

### "I added a field to NodeData"
- Check: does snapshot deserialization still work for old snapshots without that field?
  Use `#[serde(default)]`.
- Check: does NodeData now hold anything proportional to payload size?
  It must hold only metadata (offsets, collection, spatial_meta).

### "I added something to remove_raw()"
- Check: is it called during WAL replay? Will it trigger O(N) work per entry?
- Check: does it call `get_payload()`? That's a disk read per remove.

### "I changed the wal_had_* detection"
- Check: is the new flag as narrow as possible?
  Link/Unlink must not trigger GIN rebuild.
  PutVector must not trigger GIN rebuild (no text).

### "I added data to snapshot.json"
- Check: is this data > 1 MB for a 10k-node DB?
  If yes: it should probably be a binary sidecar, not in the snapshot.

### "I changed the migration threshold in open()"
- Check: does the new threshold trigger for normal-sized snapshots?
  A 90k-node snapshot is ~50-80 MB. Threshold must be ≥ 500 MB.

### "I changed build_gin_index() or collect_paths()"
- `build_gin_index()` reads all payloads — it is always O(N × payload_size).
  Only call it when absolutely necessary.
- `collect_paths()` must call `collect_raw_paths()` first (topology only),
  then load payloads only for the final result nodes.

---

## Performance Baselines (89k-node boundary DB, 886 MB payloads)

| Operation | Target | Mechanism |
|-----------|--------|-----------|
| `open()` — after compact | < 1s | gin.bin load, no payload reads |
| `open()` — with Link-only WAL | < 1s | wal_had_payload = false |
| `open()` — with Put WAL entries | 8–15s | GIN rebuild (unavoidable; run compact after bulk load) |
| `SELECT COUNT(*) FROM boundary` | < 1ms | hashes.len() fast path |
| `SELECT level, COUNT(*) FROM boundary GROUP BY level` | < 1ms | btree index-only scan |
| `SELECT * FROM boundary WHERE _key = 'x'` | < 1ms | slug_map lookup |
| `SELECT * FROM boundary WHERE level = 1` | < 5ms | btree seed + fast-path extraction |
| `SELECT * FROM boundary WHERE level = 1 ORDER BY name` | < 5ms | sort key pre-computation |
| `MATCH [:child_of*3..3] GROUP BY province` | < 200ms | hash grouping + head+tail field extraction |
| MATCH SHORTEST start→end | < 10ms | BFS |
| ILIKE '%Melbourne%' | < 5ms | GIN trigram |

Any regression beyond 2× of these baselines requires a root-cause explanation
and fix before merging.

---

## Compact Is Not Optional

`compact()` must be run after any bulk data load (insert, edge creation).
It clears the WAL and ensures:
1. Subsequent `open()` calls load gin.bin instead of rebuilding GIN.
2. WAL replay is O(delta since compact), not O(full history).
3. `payloads.bin` is defragmented (no orphan bytes from deleted nodes).

**In skcli:** `.compact`
**In Rust:** `db.compact().unwrap()`

After compact, `open()` should always be < 1 second for any dataset.
