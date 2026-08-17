# Changelog

## 0.17.0

A breaking release. It does two things: fixes data-loss and wrong-answer bugs found
by auditing the paged storage mode, and replaces build-time feature flags with a
single choice made when you open the database.

**Your data files are unchanged.** No migration is needed and no on-disk format
moved — a 0.16.x database opens directly with 0.17.0.

### Fixed — data loss and wrong answers

These are the reason to upgrade.

- **Compacting a paged database destroyed everything written before the last
  compaction.** `compact()` rebuilt the store from the in-memory write overlay only,
  so on the next open every base-resident record was gone. A database opened with
  `open_paged` and 5 records reported 0 after compact + reopen. This was reachable
  from ordinary use, because compaction also runs automatically.
- **Deleting a record that lived in the compacted base did nothing.** The record
  stayed readable; after the next compaction it became a phantom — absent from
  `get()`, still returned by queries. Deletes are now recorded and honoured
  immediately, survive a restart, and are folded away by compaction.
- **`VECTOR_NEAR` could return nothing for a collection that had matches.** The
  vector index is shared by every collection using the same field name, and the
  query took the nearest *k* across all of them before filtering by collection — so
  when the nearest rows belonged to another collection, the answer came back empty.
  Roughly 30% of runs on affected data.
- **`node_count()`, `collection_names()`, `stats()` and `SHOW` under-reported a
  paged database**, returning 0 or empty when all records lived in the base.
- **A non-empty vector index could report no nearest neighbours** on very small or
  awkwardly-connected graphs.
- **Buffered SQL never triggered automatic compaction**, so a write-buffered
  workload grew its log without bound.
- **`UPDATE` silently discarded writes.** In paged mode, updating a row that
  lived in the compacted base matched 0 rows and threw the write away — no error,
  nothing changed, nothing logged. Reachable from `open_as_service`, which uses
  paged mode.
- **Rebuilding a text index dropped most of the database.** BM25, GIN, trigram
  and positional-search rebuilds all enumerated only the in-memory write overlay,
  so in paged mode a rebuild produced an index covering just the recent writes.
  Symptom: 60 rows present, text search returning 30.
- **`ILIKE` lost rows after a write.** Writing to a paged database made the new
  trigram postings replace the memory-mapped ones for that trigram instead of
  joining them, so one unrelated insert could erase every existing match. New
  rows also reused slot numbers already owned by existing rows, and updates left
  the old text matching alongside the new.
- **A held snapshot's BM25 results could drop to nothing.** Rebuilding the index
  rewrote the postings file in place, underneath any snapshot still reading it.

`compact()` now verifies it did not lose records and returns an error instead of
reporting success if it did; the write-ahead log and previous files are left intact
when that happens.

### Faster — writing to a table that has a text index

Writing a row used to rebuild the whole index. The cost therefore grew with the
table: at 20 000 rows a single statement took a tenth of a second, and at 100 000
it would have taken most of a second. Indexes are now maintained per row.

Milliseconds per statement at 20 000 rows:

| index | insert | update | delete |
|---|---|---|---|
| trigram (`ILIKE`) | 3.0 → 3.0 | 93 → 4.1 | 93 → 4.7 |
| `BM25` | 134 → 3.3 | 108 → 2.8 | 2.8 → 2.7 |
| `SEARCH` | 144 → 3.0 | 124 → 3.8 | 1.9 → 2.6 |
| no index (for reference) | 3.0 | 3.0 | 3.0 |

Writing to an indexed table now costs the same as writing to an unindexed one,
and stops growing with the table. Results are unchanged: scores and match sets
are identical to a freshly built index, which the tests check directly.

### Faster — many writers at once

At the strongest durability setting a write is almost entirely the `fsync`: 2.94 ms
of a 2.95 ms write. Each writer used to pay for its own, even though the log is one
append-only file where a single `fsync` makes every record up to that point durable.

`open_as_service` now shares one `fsync` between writers that commit at the same
time. Throughput, all writes fully durable:

| writers | writes/sec | |
|---|---|---|
| 1 | 340 | unchanged |
| 4 | 714 | 2.1× |
| 8 | 1 412 | 4.2× |
| 16 | 2 569 | 7.6× |

Nothing is deferred and no write is acknowledged early: the record is written and
flushed before the wait begins, and the call does not return until the data is on
the disk. A crash loses exactly what it would have lost before. A single writer is
unaffected — this only helps when writes overlap.

This is separate from the existing write buffer (`EngineBuilder::buffer_size`),
which is still opt-in and still trades durability for speed: buffered statements
are not applied or logged until `flush()`.

### Changed — NULL now behaves the way PostgreSQL says it does

`!=`, `<>` and `NOT IN` used to return rows where the column was NULL or the
field was absent. PostgreSQL drops those rows, and now so does sekejap.

```sql
-- four rows: 'open', 'shut', an explicit null, and no status field at all
SELECT _key FROM p WHERE status != 'open';
-- before: shut, null-row, no-field-row
-- now:    shut          (what PostgreSQL returns)
```

Nothing errored before; the result was simply larger than the query asked for,
which is the kind of difference that is found in production rather than in
testing. Comparing a value that is not there answers neither true nor false —
SQL calls it *unknown*, and only rows where the condition is **true** come back.

This applies throughout, not just to `!=`:

| condition | rows where the value is NULL or absent |
| --- | --- |
| `x != 'a'`, `x <> 'a'`, `NOT IN (...)` | dropped |
| `NOT (x = 'a')`, `NOT (x LIKE 'a%')` | dropped — `NOT unknown` is still unknown |
| `x > 1`, `x BETWEEN 1 AND 2`, `x LIKE 'a%'` | dropped |
| `x = NULL`, `x != NULL` | no rows at all; use `IS NULL` / `IS NOT NULL` |
| `x IN ('a', NULL)` | matches `'a'` only — a NULL in the list never matches |
| `x NOT IN ('a', NULL)` | no rows at all, as in PostgreSQL |
| `IS NULL`, `IS NOT NULL` | unchanged |

**If you were relying on the old behaviour**, `WHERE x != 'a'` becomes
`WHERE x != 'a' OR x IS NULL`.

Two bugs came out of the same work. `IN` and `BETWEEN` were missing from one of
the two condition evaluators, so `NOT IN (...)` nested inside an `OR` answered
the opposite of `NOT IN (...)` on its own. And `IN ('a', NULL)` matched every row
whose field was missing, because a missing field is stored in the index as NULL
and the literal NULL in the list looked it up. Both are fixed, and the two
evaluators are now one.

### Changed — how you open a database

Which behaviour you get is now chosen at runtime instead of at build time:

```rust
sekejap::open("./mydb")                  // part of your app: starts and stops with it
sekejap::open_as_service("/var/lib/db")  // long-lived: server, robot, IoT gateway
```

`open_as_service` gives reads that don't wait behind writes, memory that stays
bounded over long runs, and self-compaction. There is still no separate server
process.

### Removed

| removed | what to do instead |
|---|---|
| `engine`, `serve`, `pg` **feature flags** | delete them from `Cargo.toml`; the code is always compiled now. `Engine` is available unconditionally, and the C ABI always exports its handle |
| **S3 / object storage**: `CoreDB::open_s3`, `RemoteSync`, `BlockCache`, `CacheBudget`, `Manifest`, the `s3` feature, and Python's `DB.open_s3` | keep the database on local disk. Reading a dataset *in place* on object storage was read-only, and making it writable needed machinery out of scope for sekejap. Removing it also drops the dependency tree from 187 crates to 36 |
| `ReadSnapshot` and `CoreDB::snapshot()` | use `CoreDB::snapshot_db()`, which returns a read-only `CoreDB` and can run the **whole** query surface — indexed filters, graph, spatial, vector, text — not just point reads |
| `WalPolicy`, `EngineBuilder::wal_policy` | use core's `CoreDB::set_auto_compact` and `CompactThresholds`, which already did the same job and also watch the paged overlay |
| `RebuildStrategy`, `IndexScheduler`, `EngineBuilder::rebuild_strategy` | they were never wired to anything; core tracks dirty indexes itself |

### Added

- `sekejap::open` / `sekejap::open_as_service`, and `Engine::open_as_service`.
- `CoreDB::snapshot_db()` — a read-only, point-in-time `CoreDB` that reads
  lock-free while the live database keeps taking writes.
- `CoreDB::stats()` and `Engine::stats()` — record and index counts, write-overlay
  size, payload and log sizes, query/write/compaction/snapshot counters, and
  compaction and snapshot timings. The CLI's `.stats` reports all of it.
- `EngineBuilder::max_scan_rows` / `max_scan_bytes` — caps so one scan cannot
  allocate without bound.

### Notes

- `sekejap serve` and `sekejap pg` are unchanged; they remain CLI features, since
  that is where the HTTP server and Postgres listener actually live.
- Snapshot reads currently require paged mode on Unix. `open_as_service` handles
  that for you; elsewhere reads transparently fall back to a shared lock.
