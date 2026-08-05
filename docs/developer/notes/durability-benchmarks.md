# sekejap — Durability, fsync, and Honest Benchmarks

## Problem Statement

`UPDATE products SET name = 'X' WHERE category = 'cat3'` (1000 rows, disk-backed):

```
sekejap  ~5 ms
SQLite   ~275 µs        (journal_mode=WAL, synchronous=FULL)
```

At face value: 20x slower. The instinct is "our engine is slow."
The instinct is wrong. Most of that gap is **two databases making different
promises about what "committed" means** — and only one of them keeps the
promise the name implies.

---

## The Sync Hierarchy on macOS

When a database says "this transaction is durable," the strength of that claim
depends on which syscall it issued. On macOS there are three levels, and they
are *not* close in cost (measured on this machine, Apple SSD, 4 KB append):

| Syscall                 | Cost        | What it actually guarantees                          |
|-------------------------|-------------|------------------------------------------------------|
| `fsync()`               | **58 µs**   | Data left the OS page cache and reached the drive. **The drive's own write cache may still lose it on power failure.** Apple's man page says so explicitly. |
| `fcntl(F_BARRIERFSYNC)` | **417 µs**  | Same as fsync, plus a write-ordering barrier: nothing written after the barrier can land before what was written before it. Cache can still lose the tail, but never reorder it. |
| `fcntl(F_FULLFSYNC)`    | **2,520 µs**| Data is flushed *through* the drive's cache to permanent storage. Survives power-cut. This is the only level where "committed" means what users think it means. |

Rust's `File::sync_data()` / `sync_all()` on macOS issue F_FULLFSYNC-class
work (~3,100 µs measured) — the standard library chooses truth over speed.

## Who Is Telling the Truth

**SQLite, at its default settings, is not.** `PRAGMA synchronous=FULL` sounds
like the strongest setting, but on macOS it issues plain `fsync()` — the 58 µs
call that does not flush the drive cache. True power-loss durability requires
`PRAGMA fullfsync=ON`, which is **off by default** and almost nobody sets.
SQLite's own docs acknowledge this; the setting exists precisely because plain
fsync is a weaker promise on Darwin.

**sekejap defaults to the truthful level.** `wal.sync()` → `sync_data()` →
F_FULLFSYNC. When sekejap returns from a write, the data survives someone
pulling the plug. That is a deliberate ~3 ms cost per statement **that we
choose to pay by default** — it is a feature, not a deficiency.

So the honest reading of the benchmark is:

```
sekejap  (durable through drive cache)   ~5 ms
SQLite   (durable to drive cache only)   ~275 µs
```

Different products. The comparison only becomes meaningful when both sides
make the same promise.

---

## Levelling the Field: SET WAL_SYNC

sekejap exposes the sync level explicitly, mirroring what SQLite hides behind
two pragmas:

```sql
SET WAL_SYNC = full      -- F_FULLFSYNC-class. Power-loss durable. DEFAULT.
SET WAL_SYNC = barrier   -- F_BARRIERFSYNC. Ordering barrier, ~7x faster.
SET WAL_SYNC = os        -- plain fsync. SQLite's actual default durability.
```

Mapping between the engines:

| Promise                          | sekejap             | SQLite                                  |
|----------------------------------|---------------------|------------------------------------------|
| Survives power-cut               | `WAL_SYNC = full` (default) | `synchronous=FULL` **+ `fullfsync=ON`** |
| Ordered, cache-resident          | `WAL_SYNC = barrier`| (no equivalent)                          |
| OS flushed, cache may lose tail  | `WAL_SYNC = os`     | `synchronous=FULL` (its actual default behaviour) |

Benchmark discipline going forward: **every cross-engine write benchmark states
the durability level, and compares like with like.** The suite now includes
`sekejap_disk_logical_os_sync` (us at SQLite's real level) and
`sqlite_disk_fullfsync` (SQLite at our level).

## Measured: The Durability Matrix (UPDATE, 1000 rows, disk)

| Variant                              | Durability promise         | Time     |
|--------------------------------------|----------------------------|----------|
| sqlite_disk (WAL, synchronous=FULL)  | drive cache only           | 271 µs   |
| sqlite_disk_fullfsync (fullfsync=ON) | **unchanged — see below**  | 274 µs   |
| sekejap logical, `WAL_SYNC = os`     | drive cache only (= SQLite)| 1.39 ms  |
| sekejap logical, `WAL_SYNC = full`   | power-loss durable         | 4.03 ms  |
| sekejap physical, `WAL_SYNC = full`  | power-loss durable         | 4.63 ms  |

**At matched durability the gap is 5.1x** (1.39 ms vs 271 µs), not 20x.

**The fullfsync surprise:** turning `PRAGMA fullfsync=ON` changed SQLite's
time by 3 µs — statistically nothing. This is consistent with SQLite's WAL
implementation syncing per-commit at `SYNC_NORMAL` strength: F_FULLFSYNC is
applied only to checkpoint operations (`PRAGMA checkpoint_fullfsync`), not to
the per-commit WAL sync. The practical consequence: **in WAL mode on macOS,
SQLite cannot be configured to make an individual commit power-loss durable.**
sekejap's default (`WAL_SYNC = full`) offers a per-commit guarantee that
SQLite's WAL mode does not offer at any setting we could measure.

---

## Decomposition of the UPDATE Statement (1000 rows, disk)

Where the original 12.8 ms went, and what removed each part:

| Cost                                   | Was      | Fix                                        | Now      |
|----------------------------------------|----------|--------------------------------------------|----------|
| 1000 individual pwrite + WAL appends   | ~5 ms    | batch payload write + batch WAL encode     | 1 pwrite + 1 flush |
| 1000 full payloads written to WAL      | ~2 ms    | `SET WAL_MODE = logical` — one ~200 B command entry | ~0 |
| 1000 wasted JSON parses in `collect()` | ~1 ms    | `collect_hashes()` — mutation paths skip Hit materialization | ~0 |
| F_FULLFSYNC                            | ~3.1 ms  | **kept by default** (the truthful promise); `SET WAL_SYNC` to trade | 3.1 ms / 0.4 ms / 0.06 ms |
| splice + btree + query (compute floor) | ~0.8 ms  | remaining honest engine cost               | ~0.8 ms  |

The same statement in-memory dropped 1.80 ms → 0.81 ms from the
`collect_hashes()` fix alone — the parse waste affected every mode.

Full journey, disk, power-loss-durable default:
12.8 ms → 7.4 ms (batch I/O) → 4.0 ms (logical WAL + collect_hashes).
Same statement at SQLite-equivalent durability (`WAL_SYNC = os`): **1.39 ms**.

## What Remains vs SQLite, and Why We Accept It

At matched durability the residual gap is roughly 3–6x, and it is structural:

1. **JSON documents vs typed B-tree pages.** SQLite updates a name by patching
   bytes inside a 4 KB page it already has in cache. sekejap rewrites the whole
   document (read → splice → append). This is the price of a schemaless,
   multi-model payload — the same bytes serve graph, spatial, vector and SQL
   access without translation.
2. **Append-only payload store vs in-place pages.** We never overwrite live
   data; crash recovery is replay, not page repair. Compaction reclaims space
   explicitly (`COMPACT`), which is the storage model we want to reason about.
3. **25 years of C micro-optimisation** on SQLite's write path.

These are paths we *chose*: explicit WAL, append-only storage, one payload
format for four query models. The benchmark gap they cost is documented here
so it is never mistaken for accidental slowness.

---

## Reproducing the Measurements

```bash
# fsync hierarchy microbenchmark (pure Rust, extern syscalls, no crates)
rustc -O /tmp/fsync_bench.rs -o /tmp/fsync_bench && /tmp/fsync_bench

# durability-matched write benchmarks (engine feature)
cargo bench --features engine --bench write_vs_sqlite
```

Measured 2026-07 on macOS (Darwin 25), Apple SSD. Numbers will differ on
Linux, where `fdatasync` is one call with one meaning and the asymmetry
largely disappears.
