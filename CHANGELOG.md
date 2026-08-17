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

### Fixed — a read-only database accepted writes and threw them away

`open_read_only` documented that "write operations will silently skip WAL
persistence". What happened was worse: `put` returned `Ok`, `DELETE` returned the
number of rows it claimed to have removed, reads in the same session answered
from the changed overlay — so the handle disagreed with the file it was reading —
and everything vanished at close with nothing raised.

Writes are refused now. `PermissionDenied` from the `io::Result` methods, an
error from statements, and the three that return nothing (`remove`, `link`,
`unlink`) do nothing and record the refusal, readable through
`CoreDB::write_error()`. `CoreDB::is_read_only()` lets a caller ask.

`EngineBuilder::read_only` had always documented the opposite — writes return an
error — for the same idea. Both behave that way now.

### Fixed — the compaction trigger that bounds memory never fired

Auto-compaction has two triggers: log size, and the number of rows held in the
RAM write-overlay. The second is what `CompactThresholds::overlay_entries` is
for, and it never fired in any layout — 5 500 rows against a 1 000-row bound left
`maybe_compact()` returning false. Memory grew until the 64 MB log bound fired
instead, well past the ceiling the setting promises.

Underneath it, the first compaction in a fresh process wrote its topology files
and did not adopt them, so the overlay stayed resident and nothing changed until
the database was reopened. That is the service case exactly — a long-running
service that creates its own database never restarts.

### Fixed — closing an engine threw away what it had accepted

`Engine::into_inner()` consumes the engine and returned the inner database
*without flushing the write buffer*, so anything buffered was lost with no later
opportunity to flush it. It flushes now and returns `Result<CoreDB, String>`
rather than `CoreDB`.

**`Engine` still has no `Drop`**, so a buffered engine that simply goes out of
scope loses what it holds. Close one with `flush()` or `into_inner()`.

`put_many` and `link_many` also reset the deferred-sync flag instead of restoring
it, so calling either inside a larger batch ended the outer group early and every
later write fsynced individually. Slower, not wrong — but not what the caller
asked for.

### Fixed — the default layout wrote the whole dataset twice

`snapshot.json` was 39.8 MB for a 200 000-row database — larger than the 36.4 MB
of payloads it duplicated — and it grew in step with the store. Opening that
database took 280 ms, and 81% of that was parsing the JSON.

The snapshot is supposed to be a *manifest*: schemas and index metadata, with the
rows in the files written beside it. Which shape gets written is decided by
whether the payload store is on disk, and a paged store answered that question
with "no" about files it had just written — so it took the branch meant for
in-memory databases, where the JSON is the only durable copy, and embedded every
row in it.

| | before | after |
| --- | --- | --- |
| `snapshot.json` at 200k rows | 39.8 MB | 1 032 bytes, constant |
| open at 200k rows | 280.8 ms | 5.8 ms |
| open, 20k → 200k | 8.55x | 1.03x |

**Existing databases fix themselves.** The oversized snapshot is replaced by a
manifest at the next `compact()`, and until then it is read exactly as before.
No migration, no action needed.

`tests/paged_payloads.rs` now asserts the snapshot does not grow with the row
count, so this cannot come back quietly.

### Changed — paged storage is the default

A new database is now written in the paged layout: payloads, nodes and adjacency
in slotted pages with a free list, topology served from the mapping instead of
loaded into RAM. This is the layout SQLite, LMDB and DuckDB use, and the reason
none of them has a compaction step — space comes back as it is freed rather than
being reclaimed by a rewrite.

What it buys is that compaction stops being a rebuild. At 500 000 rows it went
from 4 816 ms to 132 ms, and it stopped growing with the store: 108–130 ms all
the way to two million rows.

What it costs, plainly:

- **Disk.** About 2.3× the old adjacency files and 1.8× the old node files,
  almost all of it B+tree entries where the old format was a packed array.
- **Traversal.** A one-hop read is roughly 0.65× the speed of the mmap'd CSR it
  replaces — a tree descent and a record read where the old layout had two array
  reads. Point reads are *faster* than before.
- **Snapshots.** `snapshot_db()` returns `None` over paged nodes or adjacency,
  because those files are written in place and a snapshot sharing them would see
  a writer's later edits appear underneath it. Reads fall back to taking the
  lock, which is correct and slower.

**An existing database is not touched and not migrated.** The open reads the
files and uses the layout they were written in, in both directions — a database
written by 0.16.x stays exactly as it is. `Config::resident()` selects the old
layout for a new store, which is the right choice for data written once and read
many times.

### Fixed — `ORDER BY` returned rows in the wrong order when a column had NULLs

The comparator answered "equal" for any two values of different types, NULL
included. That is not a weaker ordering but an inconsistent one, and Rust's sort
is entitled to do anything when it does not get a total order. With three rows
holding `3`, `NULL` and `1`:

```sql
SELECT _key, b FROM s ORDER BY b ASC;   -- returned 3, NULL, 1
SELECT _key, b FROM s ORDER BY b DESC;  -- returned 3, NULL, 1 — identical
SELECT _key, b FROM s ORDER BY b ASC LIMIT 1;  -- returned 3, the largest value
```

One NULL anywhere in the column corrupted the order of the non-NULL rows around
it. NULLs now sort last for `ASC` and first for `DESC`, as in PostgreSQL, and a
missing field counts as NULL.

Five separate code paths ordered rows using a btree index, each walking it
directly — and `NULL` is the index's lowest key, so all five led with the rows
that have no value while the scan put them last. They now share one ordered walk.

### Fixed — `LIKE` matched substrings instead of patterns

`LIKE` stripped the `%` signs from the pattern and checked that what was left
appeared somewhere in the text. That is `contains`:

```sql
'reopened' LIKE 'open'    -- was true; PostgreSQL says false
'foo'      LIKE 'o%'      -- was true; PostgreSQL says false
'open'     LIKE '_pen'    -- was false; `_` was a literal underscore
'100%'     LIKE '100\%'   -- was false; there was no escape character
anything   LIKE ''        -- was true; only the empty string matches ''
'open'     LIKE ' open'   -- was true; the pattern was trimmed of whitespace
```

Replaced with a real SQL pattern matcher: `%` for any run, `_` for exactly one
character, backslash as the escape, matching over characters rather than bytes so
`'é' LIKE '_'` is true. A GIN index now declines patterns its trigrams cannot
represent, instead of reporting that nothing matched.

### Fixed — three statements that ran as something else

The parser stopped at the last thing it recognised and dropped the rest without a
word. Three of these were found by making it refuse leftovers, and one was in our
own test suite, passing:

```sql
DELETE FROM p GARBAGE WHERE n = 1
-- ran as `DELETE FROM p` — the whole table

SELECT LENGTH(name) FROM users ORDER BY LENGTH(name) DESC
-- sorted by a column called LENGTH, which does not exist — so, unsorted

SELECT _key FROM docs ORDER BY BM25(body, 'rust') DESC, _key ASC LIMIT 3
-- sorted by the score, then dropped the tie-break AND the LIMIT — every row
```

A statement that is not understood to the end is now an error naming the part
that was not understood. `ORDER BY` accepts the single-argument scalar functions
(`LENGTH`, `LOWER`, `YEAR`, …) and actually sorts by them. A second sort key
after a scoring expression is refused rather than discarded.

`NULLS FIRST` / `NULLS LAST` are still not implemented — but the default order
already matches what they usually ask for, and being told is better than being
ignored.

### Changed — `open_as_service` keeps its snapshots

`snapshot_reads` is a promise that reads do not wait behind writes, and the only
layout that can keep it is one whose durable half is immutable. Paged nodes and
paged adjacency are written in place, so a snapshot sharing them would see a
writer's later edits appear underneath it — `snapshot_db()` declines, and reads
fall back to taking the lock.

So the two entry points make opposite trades, and say so:

- `sekejap::open()` — the paged layout. Compaction stops being a rebuild.
- `sekejap::open_as_service()` — paged topology only. Lock-free reads survive.

Either can be overridden with `EngineBuilder::config()`, and an **existing**
database is opened in the layout it was written in regardless of which you call.

### Fixed — aggregates

- An aggregate over **zero rows produced no column at all**: `SELECT COUNT(n)`
  came back as `{}` rather than `0`, and adding a second aggregate made even
  `COUNT(*)` disappear. Now `COUNT` is `0` and the rest are NULL, and the column
  is always present.
- `SUM` over no rows returned `0`. A total of zero is a claim about the data;
  "there were no rows" is not. It returns NULL.
- `MIN`/`MAX` read only numbers, so over a text column they returned NULL. They
  now order by the same rule `ORDER BY` uses — `MIN(name)` is the name that would
  come first.
- `COUNT` of a text column returned `0` **once the column was indexed**, and
  `MIN`/`MAX` over text returned NULL for the same reason: the index path had its
  own numeric-only copy of the accumulator. There is one accumulator now.
- `SUM` of an integer column returned a float, and an indexed `GROUP BY` returned
  its key as `1.0` where the row holds `1`. Whole numbers stay whole.

### Fixed — `DISTINCT` counted NULL and a missing field as two values

The deduplication key was the serialized row, so `{"s": null}` and `{}` were
different strings. In SQL they are the same value and `DISTINCT` returns it once.

### Fixed — the parser ignored what it did not understand

Parsing stopped at the last thing it recognised and dropped the rest without a
word, so a clause that was not implemented read as though it had been applied:

```sql
SELECT _key FROM p ORDER BY n ASC NULLS LAST   -- returned the plain ASC order
SELECT _key FROM p WHERE a > 1 GARBAGE HERE    -- ran
```

Both are now syntax errors naming the part that was not understood. `NULLS FIRST`
and `NULLS LAST` are still not implemented — but the default ordering already
matches what they would ask for in the common cases, and being told is better
than being ignored.

### Fixed — a full disk crashed the process instead of failing the write

Eighteen places on the write path called `.expect()` on a disk operation: the
write-ahead log, the payload store, edge metadata and the vector store. A full
disk or an I/O error aborted the process — and aborted it *mid-write*, which is
precisely the crash the log exists to survive.

Writes now return errors. Where the signature had nowhere to put one — the log
append happens after the in-memory maps have already changed — the failure is
recorded on the database and reported by `CoreDB::write_error()`, and `compact()`
refuses to run while it is set. Folding the overlay into the base and dropping an
incomplete log is how a write that failed becomes a row that is gone.

### Fixed — a compaction had no crash-atomic commit point

Every durable file is published by renaming a temporary over it. The temporary
was fsynced; the **directory** never was — and the rename is a change to the
directory, so until it is synced a crash can undo it. Recovery could restore the
old `nodes.bin` while keeping the new `snapshot.json` that describes the new one.

The directory is now synced after every rename, and the whole new generation is
made durable *before* the write-ahead log is rotated. The log is the only record
of the writes the new base is supposed to contain; discarding it while the base
is still in the page cache traded a recoverable state for an unrecoverable one.

### Fixed — a deleted row could be counted twice after a restart

Deleting a row and writing the same key again left its hash in the btree index
twice: once from the immutable base the index was materialised from, once from
the new write. Row counts stayed correct, so it surfaced as `SUM` disagreeing
with the rows it was summing — and only after the database was reopened.

### Fixed — silent truncation on oversized values

A vector with more than 65 535 dimensions had its recorded dimension wrap to a
smaller number while every float was still written, so the next vector was
written into the middle of it. It is refused now. A collection name longer than
65 535 bytes desynchronised the whole name dictionary; it is refused at write
time.

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
