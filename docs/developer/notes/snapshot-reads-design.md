# Snapshot reads — readers that never block writers (design plan)

Status: **Phase 1 + 2 done, Phase 3 in progress.** Core `ReadSnapshot` primitive +
`Engine` integration (lock-free `get`) + snapshot **`scan`/`count`** + operational
usage docs — all correctness-tested and benchmark-proven. Remaining Phase 3:
**full indexed SQL over a snapshot** (the big lift — needs index structures frozen,
the Clone-cascade) and **query limits** (`max_rows`/`max_scan_bytes`, orthogonal
safety valve). This is the plan for the
"killer feature" for high-traffic *server* use (Zebflow): letting many reads run
at full speed while a write is in progress.

> **Phase 1 result (commit `cfd0145`/`8d6b70c`, benchmark `b24e5ba`).** Under a long
> write, classic locked reads collapse to **28 reads/sec (0%, 13.6 s max latency)**;
> snapshot (published) reads hold **97% of the reads-only ceiling (p99 4.2µs)**. The
> resident/embedded path is untouched (mega bench: no regression). See
> `eval/results/concurrency-benchmark.md` and `tests/snapshot_reads.rs`.

## Hard constraint: zero-cost for embedded (IoT / mobile)

This feature must give **server apps extra benefit without affecting or
downgrading embedded/single-threaded users at all.** Two rules make that true:

1. **Embedded uses resident mode** (`CoreDB::open`, `topo_base = None`). The
   snapshot machinery lives in the *paged* base (the immutable mmap set), which
   resident mode doesn't have — so an embedded user's hot path never touches it.
2. **Snapshots are opt-in and isolated.** A user who never calls `snapshot()` /
   never uses `SharedDB` pays **zero** cost — the normal single-writer read/write
   hot path is unchanged. Snapshots are a distinct capability that only concurrent
   server apps activate.

This constraint **rules out** any design that reworks the shared read/write merge
path for everyone (e.g. an always-on frozen-overlay check) unless that path is
provably unchanged when no snapshot is live. Prefer a mechanism that is inert
until a snapshot is taken. Every step is validated against the mega + concurrency
benchmarks to confirm the resident path doesn't regress.

---

## 1. What it is, in plain terms

Imagine the database is a whiteboard. Right now, when someone is *writing* on the
whiteboard, everyone who wants to *read* it has to wait until the writer steps
away. If that write is quick, no big deal. But if the writer is doing something
slow — a big import, rebuilding a search index — everyone reading just stands
there. On a busy website with lots of readers, that feels like the app froze.

A **snapshot read** changes the deal: instead of reading the live whiteboard, a
reader takes an instant **photograph** of it and reads from the photo. The writer
can keep scribbling on the real whiteboard the whole time; the reader's photo
doesn't change, so the reader never has to wait. Each reader that needs a fresh
view just takes a new photo.

Two important properties fall out of the "photo" idea:

- **Consistency:** a reader sees the database exactly as it was at the instant it
  took the photo — never a half-finished write. (This is the same guarantee big
  databases call *MVCC* / *snapshot isolation*.)
- **Freshness is a choice:** the photo is a moment in time. A reader that wants
  the very latest data takes a new photo. Most web reads are perfectly happy with
  "as of a few milliseconds ago."

## 2. The problem it solves today

Today sekejap is a **single-writer** engine, and an app wraps it in a lock
(`RwLock<CoreDB>`) so many threads can share it:

- **Reads** take a *shared* lock — many can read at once. Good.
- **A write** takes an *exclusive* lock — everything else waits until it finishes.

That's correct, but on a high-traffic site the pain is **long writes / index
rebuilds holding the exclusive lock** and stalling the read flood behind them.
That's the "app feels frozen" the Zebflow note describes. Snapshot reads remove
the reads from that queue entirely: they read their photo, lock-free.

## 3. Why sekejap can do this *cheaply* (and most embedded DBs can't)

Taking a "photograph" of a whole database sounds expensive — copying everything?
For sekejap it's cheap, because of how disk-first paged mode is already built:

```
  A store in paged mode  =   [ immutable BASE ]  +  [ small write OVERLAY ]
                              (mmap'd files from        (changes in RAM since
                               the last compact —         the store was opened /
                               NEVER modified)             last compacted)
```

- The **base** is a set of memory-mapped files written at the last `compact()`.
  It is *never mutated* — new writes never touch it. So every snapshot can share
  the exact same base for free (just a reference/`Arc`).
- The only thing that changes over time is the **overlay** (writes since the last
  compact). And it's *small* — bounded by how often you compact.

So a "photograph" is really just: *keep the shared immutable base + freeze the
small overlay*. That's the whole trick. A page-locking DB like SQLite would have
to build a real MVCC engine to get this; sekejap mostly has to **expose the
structure it already has.** That is exactly why this is the high-leverage bet.

Bonus corollary: **right after a `compact()`, the overlay is empty**, so a
snapshot is *free* (just the base). As writes accumulate the overlay grows, so the
snapshot cost grows — until the next compact resets it. Compaction frequency is
the tuning knob.

## 4. How it affects operation (the honest trade-offs)

Good:
- **Reads never block writes and writes never block reads.** The read flood on a
  busy site runs at full speed regardless of what the writer is doing.
- **Consistent point-in-time reads** — no torn/half-applied state.
- **Composes into read scale-out** (see §6): one writer, many read workers.

Costs / things to know:
- **A snapshot is "as of" its creation time.** A reader holding a snapshot won't
  see writes that landed after it. For web reads this is almost always fine; for
  "read-your-own-write" flows you take a fresh snapshot after writing.
- **A live snapshot pins memory.** Because the frozen overlay (and the base pages
  it references) must stay alive until the snapshot is dropped, a *long-lived*
  snapshot holds onto that memory. Rule: snapshots are short-lived (one request),
  taken and dropped quickly. Don't stash one for minutes.
- **Cheap in paged mode; expensive in resident mode.** The whole "free base"
  property needs paged mode (`open_paged`). In today's default **resident** mode
  there is no immutable base — everything is one big mutable RAM structure, so a
  snapshot would mean copying it. **Zebflow currently uses resident mode**, so
  adopting this feature pairs naturally with moving Zebflow to paged mode (already
  on the table for RAM reasons). See `docs/developer/storage.md`.

## 5. Where it lives: core vs engine (answering "is this extra in engine or core?")

**Both — split by responsibility:**

- **CORE (`CoreDB`, `lib.rs`) owns the primitive.** The base+overlay lives inside
  `CoreDB` (`topo_base` + the resident maps). Only core can freeze the overlay and
  build a read view over `base + frozen-overlay`. So core gains:
  - a `Snapshot` type (an immutable read view: shared base + a frozen overlay), and
  - `CoreDB::snapshot(&self) -> Snapshot` to mint one, plus the read/query path
    that runs against a `Snapshot` instead of the live `self`.
  This is the hard, valuable part, and it belongs in core because it touches
  internal data structures.

- **ENGINE (`engine` module, the `SharedDB` façade) owns the orchestration.** It
  coordinates the single writer and hands snapshots to concurrent readers so the
  app never writes its own `Mutex<CoreDB>`. It's a thin layer *over* the core
  primitive:
  ```rust
  let db = SharedDB::open_paged(path)?;
  // read: lock-free, runs on a fresh snapshot
  let rows = db.read(|snap| snap.query("SELECT ...")).await?;
  // write: exclusive, brief
  db.write(|core| core.execute("INSERT ...")).await?;
  ```

Rule of thumb: **the snapshot mechanism is core; the "many readers, one writer"
policy is engine.** A single-threaded user can even use `db.snapshot()` directly
for a stable read view; the engine just makes it ergonomic under concurrency.

## 6. How it scales out (the high-traffic payoff)

Because the base is immutable and self-contained, a snapshot is basically a
*read-replica in a box*. Combined with primitives sekejap already has:

- `open_read_only` + `open_paged` → **many read-only workers** each `mmap`-serving
  the same immutable base (locally), while one writer advances it.
- `open_s3` (already exists) → those read workers can serve the base **from object
  storage**, so a 1 TB dataset is queryable from small machines.

That is horizontal **read scale-out from an embedded database** — the pattern a
high-traffic, read-heavy site actually needs (write-once, read-many). Pair it with
**deferred index maintenance** (engine `IndexScheduler`) so the write side holds
its exclusive lock only briefly, and the "frozen app" problem is gone from both
the read and the write side.

## 7. The plan (phased)

**Phase 0 — foundation (already done this session):** paged mode is real and
tested (immutable base + overlay). This is the substrate.

**Phase 1 — core `Snapshot` primitive (the real work):**
1. Make the write overlay *snapshottable*. Two candidate mechanisms:
   - **Freeze-and-swap (simplest):** on `snapshot()`, wrap the current overlay in
     an `Arc` and start a fresh overlay for subsequent writes. Readers hold the
     `Arc`'d frozen overlay; the writer appends to the new one. Cheap when the
     overlay is small (i.e. compact regularly).
   - **Copy-on-write maps (more general):** back the overlay with persistent/
     immutable maps so a snapshot is an O(1) structural share. Bigger change,
     mode-independent.
   Recommend starting with freeze-and-swap in paged mode.
2. Add `pub struct Snapshot` (shared `Arc<base>` + `Arc<frozen overlay>`), and
   route the existing read/query executor to accept a `Snapshot` view.
3. `CoreDB::snapshot(&self) -> Snapshot`.
4. Tests: a snapshot taken before a write does NOT see the write; two snapshots
   are independent; dropping a snapshot frees its overlay.

**Phase 2 — engine integration (DONE, point reads).** Implemented *inside the
existing `Engine`* rather than a parallel `SharedDB` type — `Engine` already owns
the `ReadWriteGuard` and is the "one shared engine, no hand-rolled `Arc<RwLock>`"
layer, so a second façade would only duplicate it. What shipped (commit `076a3bd`):
- `EngineBuilder::snapshot_reads(true)` → opens paged mode, seeds a shared
  **published snapshot** (unix + writable only; a no-op otherwise, so nothing
  changes for embedded/read-only users).
- `Engine::get(slug)` reads that snapshot **lock-free**; `Engine::snapshot()` hands
  out the current photo; `Engine::refresh_snapshot()` forces a re-mint (RYOW).
- Republish is **write-debounced** (`publish_interval`, default 5 ms) so the
  overlay-copy cost stays off the per-write path and off readers; `compact()`
  re-mints immediately. **Thread-free** — no background publisher; republish
  piggybacks on the write path.
- Scope: **point reads only** (that's `ReadSnapshot`'s current surface). Full
  snapshot SQL (scans/graph/spatial/vector) is Phase 3.

**Phase 3 — richer reads + scale-out + companions (in progress):**
1. **Snapshot `scan` + `count` (DONE, commit `cee17b3`).** `ReadSnapshot::scan`/
   `count` and `Engine::scan`/`count` — whole-collection reads, lock-free, base +
   overlay merged like `collection_members`. Unindexed (reads every member's
   payload); for list/aggregate endpoints on bounded collections.
2. **Scale-out + operational docs (DONE, commit `5ec207a`).**
   `docs/usage/concurrency-and-snapshots.md` — the reader/writer model, snapshot
   reads, the `open_paged`+`open_read_only`+`open_s3` read-replica pattern, and
   operational limits (S3 publish-only, paged base deletes at compact, GEO WGS84/
   subtype-less).
3. **Full indexed SQL over a snapshot (TODO — the big lift).** Route the query
   executor (scans/filters/graph/spatial/vector) over a snapshot's base+overlay
   view instead of live `self`. Blocked on freezing the index structures too
   (`Bm25Index`/`SearchIndex`/`EdgeStore`/`SpatialGrid`/`VectorStore`/GIN/GiST don't
   derive `Clone`) — either make them `Arc`-shareable (like the base/payload mmap
   already are) or serve them from the mmap'd base + overlay deltas.
4. **Query limits (TODO).** `max_rows` / `max_scan_bytes` as a safety valve so one
   bad scan can't hurt the shared engine (orthogonal; pairs with `scan`).

## 8. Risks & open questions

- **Mid-transaction writes:** `snapshot()` must capture a *committed* boundary,
  not a half-applied multi-statement transaction. Tie snapshot creation to the
  same commit boundary the change-feed already uses.
- **Memory from long-lived snapshots:** enforce short-lived usage (per request);
  consider a soft cap / warning if a snapshot outlives N seconds.
- **Resident mode:** decide whether to support cheap snapshots there at all, or to
  make snapshot reads a paged-mode feature (and push paged adoption).
- **WAL/compaction interaction:** a `compact()` while snapshots are live must not
  free base pages a snapshot still references — keep the old base alive until the
  last referencing snapshot drops (`Arc` refcount handles this naturally).

## 9. One-line summary

Snapshot reads = cheap point-in-time "photographs" of the store so reads never
queue behind writes. sekejap can do it cheaply because paged mode already keeps an
immutable base + a small overlay — the primitive belongs in **core** (`Snapshot`),
the concurrency policy in **engine** (`SharedDB`), and together with
`open_paged`/`open_s3` it gives an embedded DB real read scale-out.
