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
