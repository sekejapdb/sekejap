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
