# sekejap

Embedded **graph-first, multi-model database** for Node.js — SQL + graph + vector +
spatial in one native library. No server, no external process; the whole engine runs
in-process on a Rust core.

- **PostgreSQL-like SQL** — `CREATE TABLE`, `INSERT`, `SELECT … WHERE`, parameters (`$1`).
- **Graph** — link records and traverse relationships.
- **Prepared statements** — compile once, run many with varying parameters.
- **No native build** — the prebuilt binaries for every platform ship inside the
  package. No Rust toolchain, no `node-gyp`, no compile step.

## Install

```sh
npm install sekejap
```

Works on macOS (arm64/x64), Linux (x64/arm64), and Windows (x64), Node ≥ 16.

## Quick start

```js
const { Db, version } = require('sekejap');

const db = Db.open('/tmp/notes');           // open (or create) a database directory

// Define a table and insert rows with SQL.
db.execute('CREATE TABLE note (_key TEXT PRIMARY KEY, title TEXT, pinned INTEGER)');
db.execute("INSERT INTO note (_key, title, pinned) VALUES ('n1', 'Buy milk', 1)");

// Insert a record directly (no SQL) as a JSON payload.
db.put('note/n2', JSON.stringify({ _collection: 'note', _key: 'n2', title: 'Call Sam', pinned: 0 }));

// Query. Results come back as a JSON string — JSON.parse it.
const notes = JSON.parse(db.query('SELECT * FROM note'));
console.log(notes);
// [ { _key: 'n1', title: 'Buy milk', pinned: 1, ... }, { _key: 'n2', ... } ]

// Parameterized query ($1, $2, …) — injection-safe. Params are a JSON array string.
const pinned = JSON.parse(db.queryParams('SELECT title FROM note WHERE pinned = $1', JSON.stringify([1])));

// Prepared statement — compile once, run many.
const byPin = db.prepare('SELECT _key FROM note WHERE pinned = $1');
for (const p of [0, 1]) {
  console.log(JSON.parse(db.queryPrepared(byPin, JSON.stringify([p]))));
}

// Graph: link two records.
db.link('note/n1', 'note/n2', 'related');
console.log('nodes:', db.nodeCount(), 'edges:', db.edgeCount());

// Flush + compact to disk after heavy writes.
db.compact();
```

## API

| Method | Purpose |
|---|---|
| `Db.open(path)` | Open (or create) a database at a directory path |
| `db.execute(sql)` | Run DDL/DML (`CREATE`/`INSERT`/`UPDATE`/`DELETE`) → rows affected |
| `db.query(sql)` | Run a `SELECT`/graph query → **JSON string** |
| `db.queryParams(sql, paramsJson)` | Parameterized query; `paramsJson` is a JSON array string |
| `db.prepare(sql)` / `db.queryPrepared(stmt, paramsJson)` | Prepared statements |
| `db.put(slug, payloadJson)` | Insert/replace one record by slug with a JSON payload |
| `db.link(from, to, edgeType)` | Create a graph edge |
| `db.nodeCount()` / `db.edgeCount()` | Counts |
| `db.compact()` | Flush WAL, rewrite payloads, reclaim RAM |
| `version()` | Library version |

`query` / `queryParams` / `queryPrepared` return a **JSON string** — decode with
`JSON.parse(...)`. It's an array of row objects.

### TypeScript

Types ship with the package (`index.d.ts`) — no `@types` needed.

```ts
import { Db } from 'sekejap';
const db = Db.open('/tmp/notes');
```

## License

Dual-licensed under **MIT OR Apache-2.0**.
