# Changelog

## 0.19.2

Fixes and small PostgreSQL forms found by running a real application on
0.19.1.

- **`Db::upgrade` on an empty folder** answered `NotFound`; a folder with no
  database in it yet is now left alone, the same as a missing path.
- **A quoted literal written into a JSONB column** (`'{"a":1}'`) was stored
  as a JSON string; it is now parsed, as PostgreSQL does, and text that is
  not JSON is refused naming the column. A bound parameter is unchanged.
- **`now()`, `current_timestamp` and `current_date`** (with an optional
  interval) are values in `INSERT ... VALUES` and `UPDATE ... SET`, as they
  already were in `DEFAULT`; into a DATE column they write the day.
- **`col->>'member'` in a select list** reads the member from the row: a
  string as its text, any other value as its JSON spelling, an absent member
  or JSON null as NULL.
- **`lower(col) = v` without an expression index** is checked on each row,
  as an unindexed ILIKE is, with a notice naming the index that would answer
  it index-side. As in PostgreSQL, a `v` with an upper-case letter names no
  row, with or without the index (the index path used to fold `v` too).
- Clearer refusals: a LIKE inside OR/AND/NOT no longer says it "has no
  index" when a trigram index serves it on its own, and the OFFSET refusal
  shows the keyset rewrite to copy.

## 0.19.1

- **The upgrade as one call, everywhere.** `Db::upgrade(path)` in Rust,
  `sekejap_upgrade` in the C ABI (60 functions now), and `upgrade` in every
  binding (Python, Node, Go, Kotlin/Java, Dart, Swift, C#, Lua). It moves a
  0.18 database to the 0.19 format and keeps the original beside it as
  `<path>.v018-backup`; for a 0.19 database or a path that does not exist
  it does nothing, so an application can call it on every start, before
  opening.
- **`sekejap-upgrade --check`** no longer says a 0.19 file is readable by a
  0.18 release.
- Tests that write 0.18-format files no longer switch the format for the
  whole process, which made parallel test runs fail at random.

## 0.19.0

The versatility foundation: a new supportive format, columns with stable
ids, and the schema changes they make cheap. Search work (the skip table,
fusion, analyzers) moves to 0.21.

- **The 0.19 format** (`docs/core/SUPPORTIVE.md`, `CONTRACT.md`). Rows,
  edges and index postings keep their bytes. Everything else a database
  holds about itself -- names, tables, columns, layouts, indexes, graph
  names, counters, statistics -- is now one structure, the Register: three
  small B-trees, each entry checksummed with its key, critical entries kept
  in all three copies, and a census that lets a later release refuse a file
  it cannot read by name. This is the format Law 8 holds from 0.19 on.
- **0.19 opens no 0.18-format file until it is moved**, and says so by name
  before touching a byte.
- **Moving a 0.18 database**: `sekejap-upgrade --apply <path>` (or
  `collections::upgrade::upgrade_format` from Rust) builds the 0.19 file
  beside the original, compares and verifies it, and swaps it in; the
  original directory is kept untouched as `<path>.v018-backup`. It is
  tested on the database files the released 0.18.3 and 0.18.5 builds wrote.
- **Columns have ids.** `ALTER TABLE ... RENAME COLUMN` and `DROP COLUMN`
  now work on tables that hold rows, as one catalog write: no row is
  rewritten, every index follows a renamed column, and a dropped column's
  values never come back, even when a column of that name is added later.
- **Edge tables change like tables.** Their property columns can be added,
  renamed and dropped; the end and key columns stay. `DROP TABLE` on an
  edge table a property graph declared removes its edges in bounded steps
  and frees its label.
- **`ADD COLUMN ... DEFAULT` on a table with rows**: the rows already there
  read the default, as PostgreSQL shows it, without being rewritten.
  `ALTER COLUMN ... SET/DROP DEFAULT` and `SET/DROP NOT NULL` (checked
  against every row, SQLSTATE 23502) are new.
- **Names move**: `ALTER TABLE ... SET SCHEMA`, `ALTER INDEX ... RENAME TO`,
  `ALTER SCHEMA ... RENAME TO`.
- **Up to 1,600 columns** per table (was 256).
- **Costs, measured and named**: a Register entry that must survive damage
  is kept in three trees, so the commit that opens a block of 1,024 row ids
  writes two pages more than 0.18 did (the commits inside a block write
  none); an ascending edge load reads 6.18 pages per edge against 5.93,
  because the primary tree's leaf boundaries moved when the metadata left it.
- **Fixed**: an offline rebuild of a database with a JSON-member expression
  index (`(col->>'m')`) refused it; a rebuild of a database with a dropped
  table refused it.

- **Trigram index for `LIKE` and `ILIKE`**, as PostgreSQL's pg_trgm writes
  it: `CREATE INDEX ON place USING gin (name gin_trgm_ops)`. Any pattern
  with a 3-character piece -- `'%york-ci%'`, `ILIKE '%doe%'`, `'ne%'` --
  reads only the rows holding every piece, then checks them as before, so
  the index never changes an answer. `gist (name gist_trgm_ops)` builds the
  same index; `CREATE EXTENSION pg_trgm` is accepted and does nothing.
  Shorter patterns and `NOT LIKE` are still checked row by row. Not built:
  `similarity()`, `%` and `<->`. A database gains feature bit `0x800000`
  only when its first trigram index is created; 0.18 cannot open it after.
- **`REINDEX` and `sekejap-upgrade`**: rebuild indexes into the current
  format on purpose -- `REINDEX INDEX | TABLE | SCHEMA | DATABASE` in SQL,
  `sekejap-upgrade --check` / `--apply` for a server (backup first). An old
  database keeps working without it; an upgrade is never automatic
  (`docs/core/UPGRADE.md`).
- **Faster `LIKE` without an index**: the row check reads the text where it
  lies in the page, with no copy per row, and `count(*)` judges each row in
  place.
- **Release fixtures**: databases written by the tagged 0.18.3 build are
  kept in the repository, and every build must open them with the same
  answers (`docs/core/RELEASE_FIXTURES.md`).

## 0.18.5

Thirty correctness and robustness fixes from a two-reviewer audit of 0.18.4,
and the fix for JSON written by a build that enables serde_json's
`preserve_order`. A 0.18.4 database opens as it is; nothing needs rebuilding.

**Data and durability**

- **JSON written with `serde_json/preserve_order` enabled is readable.** Any
  crate in a build can turn that feature on. Edge properties and JSONB
  objects were then written in insertion order, and the same build's reads
  refused them. Keys are now written sorted and read in any order; a
  repeated key is still refused. Files already written that way read again.
- **A service no longer stops accepting writes after 16 MiB of log.** Its
  published read view held a reader slot at every instant, so the log could
  never be folded. The service now folds at a commit that leaves the log
  due, and a wire connection lets its snapshot go between statements while
  a fold waits. Restarting a service folds a log the last run left due.
- **A failed statement no longer leaves half its rows behind.** An
  abandoned writer's work is rolled back even with no change-feed
  subscriber, a failed wire autocommit statement leaves nothing, an
  uncommitted write can no longer become committed by a later commit after
  a crash, and a failed statement aborts a Rust `Tx` as in PostgreSQL
  (`25P02`; `commit` then rolls back).
- **A torn last log frame no longer stops the database opening.** A frame
  past the last commit whose last sector never reached the disk is the end
  of the log. A frame that did land and is damaged still refuses.

**SQL**

- `ON CONFLICT DO UPDATE SET c = EXCLUDED.c` gives a left-out column its
  DEFAULT, not NULL.
- Edge tables apply NOT NULL and DEFAULT on INSERT, UPDATE and upsert alike,
  keep an explicit NULL, and check a `PRIMARY KEY (src, dst)` conflict with
  one lookup instead of reading every edge at the source.
- `COUNT(*)` honours HAVING and `LIMIT 0`; ORDER BY a text MIN/MAX or a
  large integer aggregate orders by the value itself.
- Integer literals past 2^53 keep every digit; `right(s, n)` with the
  smallest integer answers `''` instead of panicking; an edge end naming no
  row matches no edge.
- **Behaviour changes, refused by name:** `UPDATE ... SET` of a declared
  PRIMARY KEY column; `ALTER TABLE` on an edge table other than `RENAME TO`;
  a select-list expression that is not the statement's ORDER BY expression
  (it used to report the ORDER BY value); DDL inside a transaction that has
  already written (`25001`) -- it would have committed those writes; a
  parameter number above `$65535`.

**PostgreSQL wire**

- A `BEGIN` block reads its own uncommitted writes.
- Query answers stream to the socket in 64 KiB steps instead of being held
  whole, and a portal that is described before it runs still streams.
- Describe never runs a write, and `$n` inside a comment is not a
  parameter.
- A text boolean parameter follows PostgreSQL's spellings (`True`, `yes`,
  ` on `) and anything else is an error, never `false`.
- An untyped parameter keeps its text (`'00123'`, `'1001'` as a key), and a
  bound parameter takes the column's type as PostgreSQL reads an untyped
  literal.
- A comment before `BEGIN`/`COMMIT` no longer hides it, and an empty
  Describe or Close message is a protocol error, not a crash.

**C ABI**

- `sekejap_neighbours` takes its direction as `int32_t`; any value other
  than the three `SekejapDirection` constants is `SekejapStatus_Invalid`.
  Source compatible for C and C++.
- A `SekejapTx` runs on a worker thread of its own, so any thread may call
  it -- including a finalizer or a goroutine that moved threads.
- `sekejap_next_change` waiting on one subscription no longer holds up the
  others.

## 0.18.4

A fix to the full-text index.

- **Fixed: a search could report the database corrupt after an update and a
  delete.** On a text index built over rows that already existed, updating a
  row while keeping a word and then deleting the row -- or removing a word,
  adding it back and removing it again -- made every later search on that word
  fail with "text posting count exceeds term statistics". The rows were never
  damaged; the index had resurrected an entry it should have hidden. 0.18.4
  writes the index correctly from now on. A database already affected is
  repaired by rebuilding the index: `DROP INDEX` and `CREATE INDEX` again.

## 0.18.3

ORDER BY as PostgreSQL has it, and "load more" in plain SQL.

- **Several ORDER BY keys**: `ORDER BY city, name DESC, _key`, up to eight,
  over text, number and boolean columns, `_key`, arithmetic expressions and
  `bm25(...)`. Rows tied after the last key keep row-id order.
- **ORDER BY needs no index**: a column with no index is ranked from the
  row, with memory bounded by the page, instead of being refused. With an
  index on the first key the walk follows it and stops early. `EXPLAIN` says
  which.
- **NULL sorts where PostgreSQL puts it**: after every value ascending,
  before them descending. **Behaviour change:** before 0.18.3 an indexed
  `ORDER BY col` put NULL first ascending and last descending.
- **Row comparison** `WHERE (a, b) > (x, y)` (and `=`, `<>`, `<`, `<=`,
  `>=`), PostgreSQL's rules. With `ORDER BY a, b` it is keyset paging that
  skips no row tied on `a`: `WHERE (name, _key) > ($1, $2) ORDER BY name, _key
  LIMIT 20`.
- **A NOT NULL violation is SQLSTATE `23502`**, in PostgreSQL's words, like
  the missing key already was.
- **Property graphs are views, and `base` is the default graph.** Two names
  exist from the start: the schema `public` and the graph `base`, which
  holds every table and edge in every schema.
  - An edge table's direction is fixed once, by `CREATE PROPERTY GRAPH` or
    by `ALTER PROPERTY GRAPH base ADD EDGE TABLES (...)` with no named graph
    at all; later graphs name the table as it is.
  - A named graph is a stored definition: `AS` aliases (`usa.city AS
    usa_city`), several labels, `DEFAULT LABEL`, labels shared across
    schemas, `CREATE OR REPLACE`, `ALTER ... ADD/DROP VERTEX|EDGE TABLES`,
    `ALTER ... ALTER VERTEX|EDGE TABLE e ADD/DROP LABEL l`. Any number of
    graphs may show the same tables; none stores or deletes an edge.
  - Labels resolve through the graph: in a named graph through its
    definition, in `base` as a table name in any schema (ambiguous names
    refused, `"schema.table"` picks one). This fixes vertex labels for
    tables outside `public`.
  - Storage: a new catalog tail behind an additive feature bit; a file with
    a graph from 0.18.0-0.18.2 opens with that graph intact.
  - **Behaviour change:** in a named graph an unlabeled pattern covers only
    that graph's tables, and a label outside the graph is refused (it used
    to read the whole base graph with a notice).

## 0.18.2

Fixes from dogfooding sekejap under an application host, and `LIKE` / `ILIKE`
that work with no setup.

- **`LIKE` and `ILIKE` with any pattern** -- `%doe%`, `j_hn%`, `ESCAPE`,
  `NOT` -- on any text column, with no index and no extra storage: each row
  is checked, and `EXPLAIN` says so. PostgreSQL's rules, checked against its
  answers; `ILIKE` folds case beyond ASCII. A plain `LIKE 'abc%'` still uses
  the column's index.
- **`to_tsquery('john:*')`** is a prefix match, as in PostgreSQL. It was read
  as a weight and refused.
- **An INSERT's key comes from where the table declares it.** A table
  records its named `PRIMARY KEY` column and its key `DEFAULT`; an INSERT
  that gives no key is refused with `23502` instead of taking its first
  column as the key. **Breaking change:** an INSERT into a table created
  before 0.18.2 must name `_key`.
- **`_key TEXT PRIMARY KEY DEFAULT ulid()`** (or `uuid4()`), and the same on a
  named key column: the key is minted per row when an INSERT leaves it out.
- **`ORDER BY _key`** needs no index: rows are walked in key order.
- **`describe()` reports every column's SQL type** (`INT`, `REAL`,
  `VECTOR(3)`, `GEOMETRY(Point,4326)`, ...), derived from the stored kind for
  a table that recorded none, and marks a named key column as the primary key.
- **`INSERT ... RETURNING _key, *`** answers the rows the INSERT wrote, a
  minted key and DEFAULT-filled columns included, through SQL, the API
  (`Db::query`) and the PostgreSQL wire (tagged `INSERT 0 n`).
- **`ORDER BY _key DESC`** walks the keys from the highest down: newest
  first for `ulid()` keys. Keyset pages
  (`WHERE _key < $last ORDER BY _key DESC LIMIT n`) replace OFFSET, and a
  lower and an upper `_key` bound together are one range.
- **`SHOW CREATE TABLE`, `db_columns` and `describe()` report every
  column's NOT NULL and DEFAULT, the key's own included**, and
  `SHOW CREATE TABLE` names a named PRIMARY KEY column as the key, so
  replaying it builds the same table. `Field` gains `not_null` and
  `default`; `Field::primary_key` marks the one column that supplies the
  key.
- **Constant column defaults**: `DEFAULT 'member'`, `DEFAULT 0`,
  `DEFAULT true`, filled into a row that leaves the column out and printed
  back by `SHOW CREATE TABLE`. A table that uses one sets a new additive
  storage bit. Refused on a PRIMARY KEY, and on `ALTER TABLE ... ADD
  COLUMN` over a table that has rows (PostgreSQL would show the default on
  the old rows; here they would read missing).
- **`BEGIN` / `COMMIT` / `ROLLBACK` through `Db::execute` or `Db::query`**
  are refused by name. They used to answer OK and do nothing, because each
  call is already its own transaction; use `Db::transaction`.
- Storage: tables that declare a key column or a key default carry a new
  catalog tail behind an additive feature bit; other files are unchanged.

## 0.18.1

The API's catalog sees what SQL sees. Nothing in the storage format changes.

- `collections()` lists a table in a named schema as `schema.table`; a
  `public` table keeps its bare name.
- Every API call that names a collection -- `describe`, `get`, `put`,
  `count_rows`, `scan`, `create_collection` and the rest -- takes
  `schema.table`.
- `describe()` reports the table's `schema`, and for an edge table an `edge`
  section: its REFERENCES columns and tables, key, source and destination,
  label and property graph. An edge table's fields no longer include a
  `_key` it does not have.
- The C ABI's `sekejap_describe` JSON carries `schema` and `edge`, so every
  binding sees them; the Dart `CollectionDescription` and the Node
  `CollectionDescriptor` types have them as fields.
- Rust: `Collection` gains the `schema` and `edge` fields, which breaks code
  that builds a `Collection` by hand; code that reads one is unaffected.

## 0.18.0

0.18.0 opens a 0.17 database as it is, with no migration and no index
rebuild. A database that uses a new 0.18 feature (edge tables, named schemas)
is refused by 0.17 by name, never misread.

### Graph queries are ISO GQL

- A `GRAPH_TABLE (...)` body is now written in ISO GQL (ISO/IEC 39075):
  `MATCH`, `LET`, `FILTER`, `FOR`, `RETURN` and `NEXT`, with quantifiers,
  path modes (`WALK`, `TRAIL`, `ACYCLIC`), `ANY`, `ANY SHORTEST` and
  `ANY CHEAPEST`, `OPTIONAL MATCH`, `EXISTS`, `CALL` subqueries and `UNION`.
  The guide is [`docs/lang/GQL_PROFILE.md`](docs/lang/GQL_PROFILE.md); every
  construct and the test that pins it is in
  [`docs/lang/GQL_FEATURES.md`](docs/lang/GQL_FEATURES.md).
- Text, spatial and vector search work inside a pattern, and a node's
  predicate can start the walk from an index. `EXPLAIN` shows which.
- The surrounding `SELECT` filters, groups, orders and pages a graph result
  like any other table, over SQL, the PostgreSQL wire and the C ABI.

**Breaking changes for graph queries:**

- The SQL/PGQ `COLUMNS (...)` body is removed. End the body with `RETURN`
  instead: `... RETURN f.airline AS airline)`. A `COLUMNS` body is refused
  with an error naming `RETURN`.
- Each matching path is one row. A node reached by two paths is returned
  twice; `RETURN DISTINCT` returns it once.
- `{0,n}` includes the starting node, and an unbounded quantifier is no
  longer capped at 16 steps.

### Edges are written in SQL, through edge tables

- The PostgreSQL 19, Oracle 23ai and Spanner way of writing a property
  graph's edges: `CREATE TABLE` with `REFERENCES` columns naming the two rows
  an edge joins, `CREATE PROPERTY GRAPH ... EDGE TABLES (t SOURCE KEY (a)
  REFERENCES v (_key) DESTINATION KEY (b) REFERENCES w (_key))`, then plain
  `INSERT`, `UPDATE`, `DELETE` and `SELECT`. See
  [`docs/core/EDGE_TABLES.md`](docs/core/EDGE_TABLES.md).
- An edge table is a view over the graph's own edges, not a table of rows:
  nothing is stored twice, and `GRAPH_TABLE` walks the same edges.
- The edge table's columns type the edges' properties, and its
  `PRIMARY KEY` decides how many edges one pair may have: one per pair, one
  per value of extra key columns, or one per source. A taken key is `23505`,
  an end that names no row is `23503`, and `INSERT ... ON CONFLICT` is the
  upsert. A plain `INSERT` never overwrites.
- A `WHERE` on an edge table names an end, because the read is one node's
  edges; there is no index over edge properties yet.
- `ALTER PROPERTY GRAPH ... ADD` and `DROP PROPERTY GRAPH`, which keeps every
  edge. A table that an edge table references cannot be dropped.
- Over the PostgreSQL wire, a refused edge write carries PostgreSQL's own
  SQLSTATE.

### Keys and UNIQUE, as PostgreSQL enforces them

**Breaking change:** a plain SQL `INSERT` of a key that already exists is now
refused with `23505` instead of replacing the row. Write
`INSERT ... ON CONFLICT (_key) DO UPDATE SET col = EXCLUDED.col` (or
`DO NOTHING`) for an upsert. The Rust and C `put` calls are unchanged.

- `UNIQUE` on a column, `UNIQUE (col)` as a table constraint and
  `ALTER TABLE ... ADD [CONSTRAINT name] UNIQUE (col)`: a second equal value
  is `23505`; NULL values never collide.
- `CREATE UNIQUE INDEX ... USING btree (col)` built a non-unique index; it is
  unique now, and `CREATE UNIQUE INDEX ... ON t (col)` is accepted.
- A unique violation reaches a PostgreSQL client as `23505`, not `42P07`.

### pgcrypto-compatible functions

- `digest` (md5, sha1, sha224, sha256, sha384, sha512), `hmac`,
  `gen_random_bytes`, `gen_salt('bf' | 'md5' [, rounds])` and `crypt`, with
  PostgreSQL's `encode` / `decode` (`hex`, `base64`). Every answer matches
  PostgreSQL 16 with pgcrypto, and errors carry its SQLSTATEs.
- They are values: in `INSERT`, `UPDATE ... SET` and a `SELECT` with no
  `FROM`, so a password is stored with
  `crypt($1, gen_salt('bf'))` and checked with `SELECT crypt($1, $2) = $2`.
- The DES and extended-DES `crypt` formats are refused by name rather than
  emulated.

### Vector ordering follows PostgreSQL

- Under a vector `ORDER BY`, a row with no vector (missing, or written
  `NULL`) is returned last instead of being dropped, as PostgreSQL sorts
  `NULL`. The cosine distance to an all-zero vector is `NaN`, sorted after
  every number and before `NULL`, as pgvector answers it.
- `SET LOCAL ef_search` takes effect when it runs and ends with the
  transaction, as PostgreSQL scopes it. A prepared statement reads it when
  it runs.
- A GQL vector order is exact unless `ef_search` asks for an approximate
  one, also over a column that has only an approximate index.
- Rust: `Tx::query` reads inside a transaction.

### Geometry I/O

- Shapes are read and written in the forms PostGIS uses: `ST_AsBinary`,
  `ST_AsEWKB`, `ST_AsText`, `ST_AsEWKT`, `ST_AsGeoJSON`, `ST_X`, `ST_Y` and
  `ST_SRID` in a select list; `ST_GeomFromWKB`, `ST_GeomFromEWKB`,
  `ST_GeomFromText` and `ST_GeomFromEWKT` in a predicate and as a value in
  `INSERT` and `UPDATE`. A quoted WKT, hex EWKB or GeoJSON literal is read the
  way PostgreSQL reads a `geometry` literal. Every byte and string is checked
  against PostGIS 3.4.
- `col && <shape>` compares bounding boxes on the spatial index, with PostGIS's
  float4 edges.
- Over the PostgreSQL wire, `ST_AsBinary` is a `bytea` column, sent as raw bytes
  in a binary result, and a `bytea` parameter is accepted.
- `postgis_version()` answers `3.4 USE_GEOS=0 USE_PROJ=0 USE_STATS=0`.
- Storage does not change.

### Schemas

- `CREATE SCHEMA` and `DROP SCHEMA` (RESTRICT), and `schema.table` wherever a
  statement names a table. The same table name can exist in two schemas.
- `pg_namespace`, `information_schema` and `geometry_columns` report each
  table's schema, so a GIS or database client lists a layer under its schema.
- A bare name resolves in `public`; `SET search_path` is still a notice.
- A database that never names a schema is unchanged. The first schema sets a
  new feature bit, so a release older than this one refuses that file as
  unsupported rather than misreading it.
- A quoted column name is now accepted as a function argument
  (`ST_AsBinary("geom", 'NDR')`), which is how QGIS writes it.

## 0.17.0

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
and NDK lane. `JOIN` is refused, which is what DBeaver's schema tree asks for.
`NOTIFY` is not implemented.

## 0.16.5 and earlier

The 0.16.x line is a different engine and is not continued. Its history is on
branch `e1`, including a 0.17.0 entry that was written for that line and never
released.
