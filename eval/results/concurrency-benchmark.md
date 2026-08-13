# Concurrency benchmark — history

Results of `cargo bench --bench concurrency`: **read throughput + latency while a
writer/compaction runs**, on the `Arc<RwLock<CoreDB>>` model apps use to share one
engine (8 reader threads, 3 s window, 20k nodes, point reads).

This is the scoreboard for **snapshot reads** (`docs/developer/notes/snapshot-reads-design.md`):
the mega benchmark runs one query at a time and shows *zero* difference for that
feature; this one shows the whole gap. Run on demand; newest first.

`vs ceiling` = throughput as a % of the reads-only ceiling. A big drop under
writer/compact = the app "freezing" behind an exclusive lock — the pain snapshot
reads removes.

<!-- entries -->

## 2026-08-13 — `cfd0145` (snapshot reads landed — paged mode)

Mode is now **paged** (`open_paged`) — snapshots need the immutable base. Readers
run in three ways: `locked` (classic shared-lock `get`), `published` (a publisher
re-mints one shared snapshot every ~5 ms; readers `Arc`-bump it and read lock-free
— the engine/`SharedDB` pattern), `held` (one snapshot for the whole window — the
pure lock-free ceiling). Disturbers: `writer` (brief exclusive `put`s), `burst`
(4,000 `put`s per lock hold — a long exclusive hold, the paged stand-in for
"an index rebuild / bulk import froze the app").

| condition | reader | reads/sec | p50 | p99 | max | vs ceiling |
|---|---|---|---|---|---|---|
| reads only | locked (ceiling) | 3,573,310 | 2.0µs | 4.5µs | 8.87ms | 100% |
| reads + writer | locked | 385,866 | 1.8µs | 25.9µs | 6.79ms | 11% |
| reads + writer | **published** | 3,542,443 | 2.1µs | 4.0µs | 1.94ms | **99%** |
| reads + burst-writer | locked | **28** | 41.9µs | **13591ms** | 13591ms | **0%** |
| reads + burst-writer | **published** | 3,455,038 | 2.2µs | 4.2µs | 2.82ms | **97%** |
| reads + burst-writer | held | 5,984,204 | 1.1µs | 2.0µs | 2.92ms | 167% |

Reading of the result — snapshot reads do exactly what the baseline said they should:
- **Locked reads still collapse behind a long write:** under the burst-writer,
  classic locked reads fall to **28 reads/sec (0%)** with a **13.6 s** max latency —
  the same "app froze" pathology the baseline's `+compact` row showed (0% / 301 ms).
- **Snapshot (published) reads stay at the ceiling:** **97–99%** of reads-only
  throughput under both a steady writer and a burst-writer, p99 in **µs** — readers
  never take the write lock, so the writer is invisible to them. The overlay copy
  happens once per publish (off the read path), shared by all readers.
- **`held` exceeds 100%** because it's pure lock-free (no `RwLock` at all) on a
  single frozen photo — the theoretical upper bound, not a mode an app would use
  (maximally stale).

The gap between the `locked` and `published` rows under a disturber is the frozen-app
pain that snapshot reads remove. (Not directly comparable to the resident baseline
below: that ran resident mode with a `compact` disturber; paged mode doesn't run
`compact` live, so `burst` is the long-hold stand-in.)

## 2026-08-13 — `8b8db5e` (pre snapshot-reads baseline)

| condition | reads/sec | p50 | p99 | max | vs ceiling |
|---|---|---|---|---|---|
| reads only (ceiling) | 3,390,314 | 2.0µs | 4.7µs | 5.24ms | 100% |
| reads + writer | 2,053,795 | 1.4µs | 66.0µs | 3.82ms | 61% |
| reads + compact | **726** | 4.1µs | **301.14ms** | 301.14ms | **0%** |

Reading of the baseline:
- A **steady writer** costs ~39% of read throughput and spikes p99 latency **14×**
  (4.7µs → 66µs) — readers repeatedly queue behind the writer's brief exclusive locks.
- A **`compact()`** (a long exclusive hold) is catastrophic: reads collapse to
  **0%** of ceiling and p99/max hit **301 ms** — a 2µs read waits a third of a
  second. This is the "an index rebuild / big write froze the whole app" case.

**Target after snapshot reads:** the `+compact` and `+writer` rows should stay near
the reads-only ceiling (≈100%, p99 in µs), because readers run on an immutable
snapshot and never take the write lock.
