# Concurrency, snapshot reads, and scale-out

How sekejap behaves when many readers and a writer share one database — the
operational traits a server or high-traffic site needs to know. Embedded and
single-threaded users can skip most of this: the defaults already fit them, and
the opt-in features here cost nothing when left off.

## The model: one writer, many readers

sekejap is a **single-writer** engine. To share one database across threads, the
[`Engine`](../../src/engine/mod.rs) wraps it in a reader-writer lock:

- **Reads** take a *shared* lock — many run at once and don't block each other.
- **A write** takes an *exclusive* lock — it blocks other reads and writes until it
  finishes.

For short writes this is invisible. The pain appears under a **long** exclusive
hold — a bulk import, a `compact()`, an index rebuild — while a flood of reads
waits behind it. On a busy site that reads as "the app froze."

Measured on the concurrency benchmark (`benches/concurrency.rs`, 8 readers, point
reads, paged mode), reads sharing the lock with a long writer:

| readers | reads/sec | p99 | vs no-writer ceiling |
|---|---|---|---|
| classic locked reads | 28 | 13.6 s | 0% |
| snapshot reads | 3.46 M | 4.2 µs | 97% |

The first row is the frozen-app case. The second is what snapshot reads fix.

## Snapshot reads

A **snapshot** is an instant, consistent "photograph" of the database. A reader
reads from the photo instead of the live store, so it never waits for the writer —
and never sees a half-finished write. This is the same guarantee larger databases
call *snapshot isolation* (MVCC).

sekejap can take these photographs cheaply because **paged mode** already keeps the
store as an immutable memory-mapped *base* plus a small in-RAM *overlay* of writes
since the last `compact()`. A snapshot shares the base for free and copies only the
small overlay. (Right after a `compact()` the overlay is empty, so a snapshot is
almost free; it grows as writes accumulate until the next compaction — so
compaction frequency is the tuning knob.)

### Turning it on

```rust
use sekejap::engine::Engine;

let engine = Engine::builder("/var/lib/app/db")
    .snapshot_reads(true)          // opens paged mode; maintains a shared snapshot
    .build()?;

// Lock-free reads — these never queue behind a writer:
let one  = engine.get("venues/v1");     // point read
let all  = engine.scan("venues");       // whole-collection payloads
let n    = engine.count("venues");      // COUNT(*)
```

- Reads are served from a shared snapshot the writer **re-mints after commits**,
  at most once per `publish_interval` (default 5 ms). Readers only ever take an
  `Arc` of the current photo and read it — the writer swapping in a new one never
  blocks them.
- `engine.snapshot()` hands you the current photo directly if you want several
  reads at the *same* instant.

### What it covers (and doesn't, yet)

- **Covered, lock-free:** point `get`, whole-collection `scan`, and `count`.
- **Not covered:** an indexed `query("SELECT … WHERE …")` still takes the shared
  read lock (it needs the index structures, which a snapshot doesn't freeze).
  Full indexed SQL over a snapshot is future work. Use `scan`/`count` for
  list/aggregate endpoints on bounded collections; use `query` for indexed lookups.

### Trade-offs to plan for

- **Freshness is "as of" the last publish.** A read may lag the newest write by up
  to `publish_interval`. For *read-your-own-write*, call `engine.refresh_snapshot()`
  after your write (forces an immediate re-mint), or read through the locked
  `query()` path.
- **A live snapshot pins memory.** The frozen overlay and the base pages it
  references stay alive until the snapshot drops. Keep snapshots **short-lived**
  (one request); don't stash one for minutes.
- **Paged mode only, unix only.** `snapshot_reads(true)` is a no-op in resident
  mode, in read-only mode, and on non-unix — `get`/`scan`/`count` transparently
  fall back to the shared read lock there, so the same code still runs.
- **Embedded/single-threaded users: leave it off.** The write path is unchanged
  and nothing is minted, so you pay nothing. This feature is for concurrent servers.

## Read scale-out

Because the base is immutable and self-contained, it doubles as a read-replica.
Combined with primitives sekejap already has:

- `open_paged` + `open_read_only` → **many read-only workers**, each memory-mapping
  the same base locally, while one writer advances it and republishes.

This is write-once/read-many scale-out from an embedded database — see
[connectivity.md](connectivity.md) for the read-only mode.

## Operational limits to know

Honest caveats for production planning:

- **Paged base deletes land at `compact()`.** In paged mode, `remove`/`unlink` of a
  node that lives in the base takes effect when the next `compact()` rewrites the
  base. Overlay (recent) deletes are immediate. `scan`/`count` follow the same rule.
- **Spatial is WGS84/4326 only and subtype-less.** A `GEO` column accepts any
  GeoJSON geometry (point, polygon, multipolygon, …) mixed across rows — there is no
  per-column geometry-type or SRID declaration like PostGIS. `GeometryCollection` is
  stored and returned but is dropped from spatial *predicates*. See
  [queries.md](queries.md) for the spatial surface.
