# sekejap (Node.js)

Embedded **graph-first, multi-model database** for Node.js: SQL + graph + vector
+ spatial in one native library. No server, no external process — the whole
engine runs in-process, in Rust, and this package talks to it over the C ABI
(`docs/dist/C_ABI.md`), not napi-rs.

- **PostgreSQL-like SQL** — `CREATE TABLE`, `INSERT`, `SELECT … WHERE`, `$n` parameters.
- **Graph** — `link`/`unlink`/`neighbours`, and `GRAPH_TABLE` in SQL for deeper walks.
- **Prepared statements and paged scans** — compile once, run many; walk a
  collection or a SELECT's answer a page at a time.
- **Transactions** — many writes under one commit barrier.
- **Documents already parsed** — every call returns a JS value, not a JSON
  string to `JSON.parse` yourself.

## Choosing the FFI route

This wrapper binds `libsekejap` through [koffi](https://koffi.dev), not
through a compiled napi-rs addon (e1's approach, and what this package used
before). Two reasons:

1. **No native build for this package.** koffi ships its own prebuilt N-API
   glue for the platforms it supports; this file does one `koffi.load()` of
   the sekejap shared library at `require()` time. There is no `cargo build`,
   no `node-gyp`, and no per-Node-ABI addon to rebuild — the wrapper itself
   is pure JS, and only `libsekejap.{dylib,so,dll}` is platform-specific.
2. **`node-ffi-napi`/`ref-napi` are gone.** They were the other candidate
   considered, but `node-ffi-napi` is unpublished from the npm registry
   (`npm view node-ffi-napi` → 404) and its `ref-napi` dependency compiles a
   native addon of its own — the opposite of what "no native build" needs.
   koffi is the smaller, actively maintained port: one dependency, prebuilt,
   with disposable types (`koffi.disposable`) that map directly onto this
   ABI's "every returned `char*` is freed once with `sekejap_string_free`"
   rule (see `index.js` §2-3).

## Install

```sh
npm install sekejap
```

The published package bundles a `native/<platform>-<arch>/lib/libsekejap.*`
per supported platform (assembled by the `publish-node` CI job from the same
`build-native-libs` artifacts every other wrapper links). Locally, or against
a library you built yourself, point at it explicitly:

```sh
export SEKEJAP_LIB_PATH=/path/to/libsekejap.dylib   # exact file, or:
export SEKEJAP_LIB_DIR=/path/to/dir/holding/it       # a directory, or:
export DYLD_LIBRARY_PATH=/path/to/dir                # (Linux: LD_LIBRARY_PATH)
```

Resolution order: `SEKEJAP_LIB_PATH` → `SEKEJAP_LIB_DIR` →
`native/<platform>-<arch>/` inside the package → the bare library name,
which the OS loader then searches its own path for.

## Quick start

```js
const { Db } = require('sekejap');

const db = Db.open('/tmp/notes'); // open (or create) a database directory

db.execute('CREATE TABLE note (title TEXT, pinned BOOL)');
db.execute("INSERT INTO note (_key, title, pinned) VALUES ('n1', 'Buy milk', true)");

// The document API — no SQL. `_key` in the value is optional; it is set
// from the `key` argument when absent.
db.put('note', 'n2', { title: 'Call Sam', pinned: false });

console.log(db.get('note', 'n1')); // { _key: 'n1', title: 'Buy milk', pinned: true }

db.execute('CREATE INDEX note_pinned ON note USING btree (pinned)');
const pinned = db.query('SELECT title FROM note WHERE pinned = $1', [true]);

const stmt = db.prepare('SELECT title FROM note WHERE pinned = $1');
for (const want of [true, false]) console.log(stmt.query([want]));
stmt.close();

db.link('note', 'n1', 'related', 'note', 'n2');
console.log(db.neighbours('note', 'n1', 'related', 'outgoing', 10));

const tx = db.transaction();
tx.put('note', 'n3', { title: 'Committed together', pinned: false });
tx.commit(); // or tx.rollback()

console.log('rows:', db.countRows('note'));
db.close();
```

More: `examples/tour.js`.

## API mapping (C function → wrapper call)

| C function | wrapper call |
|---|---|
| `sekejap_open` | `Db.open(path)` |
| `sekejap_open_with_config` | `Db.openWithConfig(path, config)` |
| `sekejap_open_service` | `Db.openService(path)` |
| `sekejap_close` | `db.close()` |
| `sekejap_version` | `version()` (module function) |
| `sekejap_format_version` | `formatVersion()` (module function) |
| `sekejap_last_error` / `sekejap_last_error_code` | folded into every thrown `SekejapError` (`.message`, `.code`, `.status`) |
| `sekejap_string_free` | never called by user code — wired as a `koffi.disposable` type, freed automatically after every C→JS string conversion |
| `sekejap_put` | `db.put(collection, key, doc)` |
| `sekejap_put_many` | `db.putMany(collection, rows)` |
| `sekejap_get` | `db.get(collection, key)` → object or `null` |
| `sekejap_exists` | `db.exists(collection, key)` → boolean |
| `sekejap_delete` | `db.delete(collection, key)` → boolean |
| `sekejap_scan_open` / `_next` / `_close` | `db.scan(collection, pageRows)` → `Scan` (`for...of`, `.rows()`, `.next()`, `.close()`) |
| `sekejap_execute` | `db.execute(sql, params)` |
| `sekejap_query` | `db.query(sql, params)` |
| `sekejap_explain` | `db.explain(sql, params)` |
| `sekejap_prepare` | `db.prepare(sql)` → `Statement` |
| `sekejap_stmt_query` | `stmt.query(params)` |
| `sekejap_stmt_execute` | `stmt.execute(params)` |
| `sekejap_stmt_rebindable` | `stmt.rebindable()` → boolean or `null` (unbound) |
| `sekejap_stmt_free` | `stmt.close()` |
| `sekejap_query_open` / `_next` / `_close` | `db.stream(sql, params, pageRows)` → `Scan` |
| `sekejap_link` | `db.link(fromCollection, fromKey, edgeType, toCollection, toKey)` |
| `sekejap_link_with` | `db.linkWith(..., properties)` |
| `sekejap_unlink` | `db.unlink(...)` → boolean |
| `sekejap_neighbours` | `db.neighbours(collection, key, edgeType, direction, limit)` |
| `sekejap_create_collection` | `db.createCollection(name, fields)` → boolean |
| `sekejap_drop_collection` | `db.dropCollection(name)` → boolean |
| `sekejap_collections` | `db.collections()` |
| `sekejap_describe` | `db.describe(collection)` → object or `null` |
| `sekejap_count_rows` | `db.countRows(collection)` |
| `sekejap_scan_count_rows` | `db.scanCountRows(collection)` |
| `sekejap_scan_count_edges` | `db.scanCountEdges()` |
| `sekejap_tx_begin` | `db.transaction()` → `Tx` |
| `sekejap_tx_put` / `_delete` / `_link` / `_execute` | `tx.put` / `tx.delete` / `tx.link` / `tx.execute` |
| `sekejap_tx_commit` / `_rollback` | `tx.commit()` / `tx.rollback()` |
| `sekejap_checkpoint` | `db.checkpoint()` → boolean (folded/deferred) |
| `sekejap_publish` | `db.publish()` |
| `sekejap_storage` | `db.storage()` |
| `sekejap_statement_timeout_ms` | `db.statementTimeoutMs(ms)` |
| `sekejap_cancel` / `_clear_interrupt` | `db.cancel()` / `db.clearInterrupt()` |
| `sekejap_subscribe` / `_next_change` / `_unsubscribe` | `db.subscribe()` / `db.nextChange(id, timeoutMs)` / `db.unsubscribe(id)` |
| `sekejap_open_memory`, `_trim_memory`, `_compact`, `_show` | `Db.openMemory()`, `db.trimMemory()`, `db.compact()`, `db.show()` — all always throw `SekejapError` with `.code === 'Refused'`, by name |

## What was removed from e1's wrapper

e1's `dist/bindings/wrappers/node/` was a napi-rs addon (`src/lib.rs`,
`Cargo.toml`/`Cargo.lock`, `build.rs`) compiled against the OLD `CoreDB`
(slug-addressed rows, `nodeCount`/`edgeCount`, a single-string `Db.open`) and
a prebuilt `sekejap.darwin-arm64.node` binary, plus a TypeScript ORM layer
(`orm/`, and its build output `dist/`) and Next.js/Express example apps built
on that ORM. All of it is gone:

- **The napi-rs Rust glue crate** (`src/`, `Cargo.toml`, `Cargo.lock`,
  `build.rs`) — this wrapper is pure JS over koffi now; there is nothing to
  compile, so no Rust crate belongs here at all. This is also why `build-node`
  no longer runs `cargo`/`napi build`.
- **The committed `.node` binary and `node_modules/`** — a native binary and
  ~2,000 files of transitively-vendored TypeScript/React tooling had no
  business in version control; `npm install` fetches the one runtime
  dependency (`koffi`) and `.gitignore` keeps `node_modules/` out from here on.
- **`orm/` and its `dist/` build output** — a hand-rolled query builder /
  React-hooks layer over the old slug API. It does not carry over: this
  wrapper's job is the one-to-one C ABI mirror above, and a higher-level ORM
  over the 0.17 shape is a separate, later decision, not part of re-opening
  the publish channel.
- **`examples/nextjs/`, `examples/express-server.js`** — both were demos of
  the ORM layer, not of the C ABI surface. `examples/tour.js` stays,
  rewritten against the 0.17 API (`db.query()`/`db.get()` already return
  parsed values, not JSON strings; `_key` is implicit rather than a declared
  `TEXT PRIMARY KEY` column — see `lang/src/compile/ddl.rs`: names starting
  with `_` are reserved in `CREATE TABLE`).
- **`test.cjs`, `test_prepared.cjs`, `bench.cjs`** — kept as a single
  `test.cjs` (every leg of the common wrapper checklist: open, create,
  put/get, a parameterized query, scan, prepare+rebind, link+neighbours,
  tx commit/rollback, count_rows, an error path, close) and a rewritten
  `bench.cjs` (same shape, `Db.get` instead of a SQL SELECT by `_key`,
  because `_key` equality now needs a named index like every other Tier-1
  predicate — QL_CONTRACT §6 — and the point of this bench is the FFI/JSON
  round trip, not the planner).

## Gaps in the C ABI found

None. Every one of the 59 functions in `dist/ffi/include/sekejap.h` is bound
and reachable from JS (the four "refused by name" calls are bound too, so
the refusal surfaces as a named `SekejapError`, never `TypeError: ... is not
a function`).

## Tests

```sh
export SEKEJAP_LIB_DIR=/path/to/libsekejap   # or SEKEJAP_LIB_PATH
npm install
npm test
npm run bench     # optional
node examples/tour.js
```

`test.cjs` runs against the real library — no mocks — and prints one `ok -`
line per leg.
