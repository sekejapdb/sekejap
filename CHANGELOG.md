# Changelog

## 0.17.0 (unreleased)

0.17.0 replaces the engine. The storage layer, the data model, the API and the
on-disk format are all new, and the release opens three surfaces sekejap did not
have before: SQL, the PostgreSQL wire protocol, and a C ABI with eight language
bindings over it.

**A database written by 0.16.x or earlier cannot be opened by 0.17.0.** There is
no migration path in the engine and none is planned: the formats share no
structure. Move data across by reading it with 0.16.x and writing it with
0.17.0.

### The format

The on-disk envelope is **sekejap disk format v2**, and it is stable the way
SQLite's file format is stable: later releases may change how fast they read and
write it, not what they write. Page header bytes 18 and 19 carry the format
version, every 0.17.x reads and writes v2, and a file without the stamp is
refused rather than guessed at. New capabilities arrive as a new keyspace behind
an additive feature bit, so an older reader refuses a file it does not fully
understand instead of misreading it.

### The data model and the Rust API

- A document is addressed by **collection and key**, not by one slug string:
  `db.put(("dishes", "laksa"), &value)`. Collections declare typed fields, and a
  row on disk is a typed positional record. JSON is a wire format at the API
  boundary only; no JSON text is stored.
- One crate, `sekejap`, is the whole database: `Db::open`, `put`, `get`,
  `delete`, `scan`, `query`, `prepare`, `execute`, `explain`, `transaction`,
  `link`, `neighbours`, `describe`, `count_rows`, `checkpoint`, `publish`.
- Service mode (`Db::open_service`) keeps one writer with snapshot readers.
- Removed with the old engine: `CoreDB`, `open_paged`, the slug-addressed
  `put`/`get`/`link`, `trim_memory`, `memory_report`, `compact()` and the
  build-time storage feature flags. Store configuration is chosen once, at open.

### SQL

- `SELECT`, `INSERT`, `UPDATE`, `DELETE` over collections, with `WHERE`,
  one `ORDER BY` expression, `LIMIT`, and `$n` parameters.
- Prepared statements, re-bindable across executions, with a plan cache.
- Aggregates, boolean filters, scalar functions, and `SELECT ... FROM
  GRAPH_TABLE (...)` for graph patterns.
- `EXPLAIN` answers the plan as text.
- A construct the engine cannot back is **refused by name**, with its SQLSTATE
  and the reason, and never emulated silently.

### Catalog and the PostgreSQL wire

- `pg_catalog` and `information_schema` are views over the catalog rows, computed
  on demand.
- `sekejap-pg` serves the PostgreSQL frontend/backend protocol: startup, the
  simple and the extended query flows. `psql` connects, and DBeaver's connect
  sequence is answered.

### C ABI and language bindings

- `libsekejap` is a C dynamic and static library with a cbindgen-generated
  header, published for Linux x64/arm64, macOS x64/arm64 and Windows x64.
- Eight bindings call that ABI and nothing else: C#, Dart, Go, Kotlin, Lua,
  Node, Python and Swift. Each one dropped the Rust glue crate it used to carry.
- The typed-model layers that sat on the old engine are gone with it: the Node
  ORM with its React hooks, the Kotlin KSP ORM and its Android archive, the Dart
  `flutter_rust_bridge` bridge with cargokit and the code generator, and the
  Python PyO3 module. They can be rebuilt over the new bindings; they are not in
  this release.

### Engine work behind the surfaces

- Live per-collection row counts, so `count(*)` answers from a record.
- Graph endpoint sets, so a semi-join over edges reads one keyspace.
- Vector, spatial and full-text indexes, and approximate nearest neighbour
  search over quantized vectors.

### Measured against PostgreSQL and SQLite

50,000 rows, 43 query cases, four arms: the embedded API, the same queries as
SQL, PostgreSQL 19 with PostGIS and pgvector, and SQLite with FTS5 and R-tree.

All four arms issue the same class of commit barrier: an ordinary `fsync`, not
the macOS drive-cache barrier, which SQLite leaves off by default and
PostgreSQL does not use either.

| measure | result |
|---|---|
| cases where every arm returned the same rows | 35 of 35 |
| cases where sekejap is faster than PostgreSQL | 40 of 43 |
| cases where sekejap is faster than SQLite | 32 of 43 |
| store size against PostgreSQL | 61.6 MiB vs 121.2 MiB |

Where it is behind: bulk vector writes against SQLite (12.5 ms against 1.9 ms
per 1,000 rows), loading 50,000 graph edges (9.4 s against PostgreSQL's 3.3 s
and SQLite's 1.5 s), and three cases against PostgreSQL, the largest being an
existence test over related rows at 1.26 times PostgreSQL's median.

### Durability

Every commit is published with an ordinary data barrier, which is what SQLite
and PostgreSQL do by default. A committed write survives a process kill and an
operating-system panic. It is not guaranteed against a power cut on a drive
whose cache lied about writing.

For the stronger guarantee, open with `SyncMode::Full`, which issues
`fcntl(F_FULLFSYNC)` on macOS and `fsync` elsewhere:

```rust
use sekejap::{Config, Db, SyncMode};
let db = Db::open_with("app.db", Config { sync: SyncMode::Full, ..Db::config() })?;
```

It costs 11.9 ms against 1.45 ms per barrier on the volume these measurements
were taken on, so at bulk sizes it is most of a write's wall time. Through the
C ABI and every language binding, the same choice is `{"sync": "full"}` in the
configuration passed to open.

### Not in this release

The React Native binding is not ported. The Kotlin Android artifact needs a JNI
and NDK lane. Geometry is not yet read or written as WKB or EWKB, so QGIS cannot
round-trip it. `JOIN` is refused, which is what DBeaver's schema tree asks for.
`NOTIFY` is not implemented.

## 0.16.5 and earlier

The 0.16.x line is a different engine and is not continued. Its history is on
branch `e1`, including a 0.17.0 entry that was written for that line and never
released.
