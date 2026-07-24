# sekejap — Requirements & Roadmap

Single source of truth for what sekejap must be, its non-negotiables, and the
phased plan. Cross-links the detailed docs rather than duplicating them.

## What sekejap is

> **sekejap is an embedded memory-and-reasoning engine — SQLite for the age of
> intelligent apps.** It runs inside your process (no server, zero ops) and unifies
> the five dimensions a system needs to *remember and explain*: documents (SQL),
> relationships (typed graph edges with `MATCH`), meaning (vectors), place
> (spatial), and time — under one PostgreSQL-flavoured query surface with hybrid
> scoring across all of them. Disk-first and mmap-paged, it serves 50 GB of graph
> on 1 GB of RAM while keeping bounded traversals at microseconds, so the same
> engine that ranks search results can walk a `caused_by` chain ten hops deep to
> show *why* — white-box answers, not black-box ones — on a phone, a sensor
> gateway, or a robot. Small enough for a Raspberry Pi, honest enough for a paper:
> every claim benchmarked, every byte on disk versioned and recoverable.

Founding motivations: (1) hybrid retrieval + scoring; (2) root-cause analysis &
knowledge traversal — long hops over a small, **typed** edge vocabulary
(`caused_by`, `part_of`, `requires`) with white-box path answers; (3) time — exact
now, fuzzy later (foundation reserved: registered TIMESTAMP/interval edge columns +
rebuildable sidecar indexes → additive, never a re-migration). Target: small-to-
medium apps, IoT, Android, embodied AI, humanoid memory. NOT a social-scale
dynamic-edge graph, NOT a scale-out server.

## Non-negotiables (in priority order)

An embedded DB runs in someone else's process. These come before features.

1. **Never corrupt silently.**
   - Status: ⚠️ *hardening.* Node identity = `hash(slug)` (u64) with no collision
     check yet → a hash collision silently merges two nodes. Fix: `idx.bin`
     name-resolution compares the stored slug and errors on mismatch (see roadmap
     → Identity). Dense ids (Phase 0) remove the collision risk entirely.
2. **Never kill the host.**
   - Status: ⚠️ *targeted audit.* SQL surface is already `Result`-based. Remaining
     exposure = I/O panics (e.g. disk-full `expect`) and a few executor `unwrap`s.
     Fix: convert I/O + executor panic sites to errors; reserve panics for
     internal invariants only.
3. **Crash-recoverable & durable.** *(super important)*
   - Status: ✅ *solid, one item to verify.* WAL + `replay_all` on open; `SyncLevel::Full`
     fsync by default; CRC32 per frame (torn writes discarded); atomic tmp→fsync→rename
     for snapshot/payloads/indexes; `TxnBegin/TxnEnd` for all-or-nothing transactions.
     Same model as Postgres/SQLite. **Verify:** auto-commit multi-structure inserts
     (node + its edges) are wrapped in one txn unit, not just explicit `BEGIN/COMMIT`.
     Topology files (Phase 0+) keep this: atomic-rename written, re-derivable from
     `payloads.bin` + WAL. See [durability.md](durability.md).
4. **Predictable latency.** Bounded/local queries stay µs. Hot working set never
   faults. (Cold/full-scan latency is disk-bound — accepted, universal.)
5. **Format-stable & evolvable.** Every on-disk file has a `[magic][version][flags]`
   header; readers dispatch on it; old data migrates via `compact()`-rebuild, never
   a re-import or a break. ✅ shipped for `snapshot.json` (v2). Extends to topology.

## Scaling requirements

- **50 GB of data on 1 GB RAM** — via mmap'd, offset-addressable topology + payloads;
  OS page cache holds the hot working set, pages the rest. Automatic, no config.
- **S3-scalable** — the *same* offset-addressable format served by the existing block
  cache when data is remote/unbounded.
- **~1 billion nodes on 1 GB RAM for *bounded* queries** — dense-id CSR topology,
  mmap-paged. Honest limit: genuine full-billion-node *scans* are disk-bound on 1 GB
  (true of every engine).
- Keep the embedded, zero-ops, µs-hot-path identity throughout.

## Storage strategy — THE foundational decision

**Topology = binary, mmap'd, StreamVByte-delta neighbor ids. Payloads = block-zstd.
Recovery = WAL + atomic rename.** The sweet-spot row below — fast *and* compact *and*
power-loss-safe *and* unbounded.

- **Topology is binary, never JSON** — fixed-size node records (`nodes.bin`) + CSR
  adjacency. JSON is impossible at a billion (size + parse cost).
- **Access = `mmap`** (OS page cache), *not* a hand-rolled buffer pool. Adaptive by
  construction: fits-in-RAM → resident-fast; exceeds-RAM → OS pages the cold parts.
  Raspberry-Pi-friendly (uses whatever RAM exists). S3 = same format via `BlockCache`.
- **Neighbor ids = StreamVByte-delta** — sorted, gap-encoded, SIMD-decoded (~1–2 B
  each). Compact *and* ceiling-free (logically `u64`, no `u32` limit) *and* fast
  (branch-free 4-at-a-time decode ≈ a raw array read). Payloads get **block-zstd**.
- **Compression is invisible to crash recovery.** The WAL is uncompressed; checkpoints
  (topology/payload files) are written atomically (tmp→fsync→rename) and are
  rebuildable from WAL + payloads. Blackout-safe regardless of compression.

### Speed ↔ size ↔ recovery trade-off (and where engines sit)

| Strategy | Size | Hot traverse | Cold traverse | Recovery | Nodes @1 GB | Engine |
|---|---|---|---|---|---|---|
| All-in-RAM (HashMaps) | — | ⚡⚡⚡ | n/a | WAL replay | ~15–20 M | **sekejap today** |
| mmap raw binary (fixed-id CSR) | big | ⚡⚡⚡ | fast (SSD fault) | WAL + atomic | 34 B (legacy) | Neo4j |
| **mmap + StreamVByte-delta CSR + zstd payloads** | **small** | **⚡⚡⚡** | fast (fewer faults) | **WAL + atomic** | **unbounded** | **← sekejap target** |
| LSM KV (RocksDB/Badger) | small | ⚡ (KV lookup/hop) | slower | LSM WAL | 2⁶⁴ / unbounded | Arango, Surreal, Dgraph |

The open corner nobody occupies: **Neo4j-class traversal speed + Dgraph-class compact &
unbounded size + embedded + S3-scalable.** That is sekejap's target — StreamVByte makes
the topology *smaller than Neo4j's raw records* while removing the node ceiling.

### Addressing a billion nodes (three layers)
1. **Location** — `dense_id → offset` is arithmetic (`nodes.bin[HEADER + k×24]`), no
   index. Ids are logically `u64` (unbounded); stored StreamVByte-delta in edges.
2. **Adjacency** — CSR: seek `offsets[k]`, **StreamVByte-decode** the neighbor deltas
   (4/step, branch-free), prefix-sum to dense ids → follow. Index-free per hop.
3. **Name resolution** — sorted `hash→id` (`idx.bin`), binary-searched **only at query
   roots**. A **RAM-resident sparse index** (~10–50 MB for a billion) narrows each
   lookup to ~1 page fault. After the root, traversal touches no index.

At 1 B nodes / ~5 edges each: ~124 GB topology on disk (24 GB nodes + 8 GB offsets +
80 GB edges + 12 GB idx), all mmap'd; a bounded query's working set ≈ MB → fits 1 GB.

## Roadmap (phased)

### Storage — the path to billions on 1 GB
- **Phase 0 — lock the format** (pre-launch, irreversible part). Dense internal ids +
  offset-addressable, versioned topology files (`nodes.bin`, `adj_fwd/rev.bin`,
  `idx.bin`, `dict.bin`). Still loads into RAM this version → no behavior change.
  Spec: [topology-format-v2.md](topology-format-v2.md).
- **Phase 1 — mmap flip.** Read topology directly from the mmap; OS pages it →
  50 GB / billions on 1 GB for bounded queries.
- **Phase 2 — S3 flip.** Topology block source = existing `BlockCache` → unbounded.

### Identity
- **Now:** collision check (loud error on hash+slug mismatch) — folded into `idx.bin`.
- **Phase 0:** dense sequential internal ids become the physical identity →
  collisions impossible (no birthday limit), index-free traversal, half-size edges.
  API keeps `hash(slug)` linking (no caller-visible change). **128-bit not needed.**

### Analytics engine (make live aggregation beat Postgres)
- ✅ **Selective materialization** — MATCH aggregate skips payloads for traversal-only
  hop vars (~1.7× on LMS-shaped rollups). Shipped.
- **btree for aggregated numeric fields** — `AVG/SUM(x)` reads values from the btree
  index instead of parsing payloads (extend plain-SELECT `index_agg` to MATCH).
- **Finer `EXPLAIN ANALYZE`** — per-phase timing (traverse/materialize/extract/group).
- **Smallest-bitmap-first AND ordering** — order predicates by exact `RoaringBitmap::len()`
  (cheap; a differentiator PG's estimates can't match). See [foundations.md](foundations.md).
- *(Later, if measured need)* typed column cache + dense group-by kernel.

### Correctness hardening (pre-publication)
- Collision check (above). I/O + executor panic → error pass. Verify auto-commit
  multi-structure atomicity.

### Features (deferred — see [../../TODO.md](../../TODO.md))
- Constraints: `NOT NULL`, `CHECK`, `UNIQUE` (write-path, pay-per-declaration).
- `VAGUE` fuzzy-match operator; UUID auto-key / dual-MATCH-INSERT phases.
- Multi-language bindings: Node (napi-rs), then a `sekejap-capi` C ABI for Go/Java.

## Decisions locked (don't re-litigate)

- **Embedded, not a server.** In-process library is the identity.
- **SQL + MATCH only** — no bare `MATCH … RETURN`; `SELECT … FROM MATCH` is the graph surface.
- **PostgreSQL dialect**, MATCH the single Cypher-ish piece.
- **Versioned headers on every on-disk file**; format changes are `compact()` migrations.
- **Topology is binary (not JSON): fixed-size node records + CSR adjacency.**
- **Neighbor ids = StreamVByte-delta** — sorted/gap-encoded, SIMD-decoded, ~1–2 B each.
  Logically `u64` → **unbounded node count** (no `u32` ceiling); collision-free
  (sequential); index-free traversal; smaller *and* faster than fixed-width.
- **Access = `mmap` (OS page cache), not a hand-rolled buffer pool.** One format,
  swappable block source: mmap for local, `BlockCache` for S3.
- **Payloads get block-zstd (size).** Edge attribute (type/strength/meta) compression
  is an optional later pass.
- **Name resolution = sorted `hash→id` + a resident sparse index** (~1 fault/root).
- **Recovery is compression-invisible:** WAL uncompressed + atomic tmp→fsync→rename.
- **Resident load stays the default** until mmap ships; no perf regression is a hard gate.

## Current version

`0.12.1`. Shipped since 0.12.0: unified `SELECT … FROM MATCH` surface, plain-SELECT
`CASE`/`COUNT(DISTINCT)`, Python `EXPLAIN`, versioned snapshot header, selective
materialization. See [architecture.md](architecture.md).
