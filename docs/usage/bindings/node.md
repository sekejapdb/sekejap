# Node.js

Install from [npm](https://www.npmjs.com/package/sekejap) — prebuilt native
binaries for macOS (arm64/x64), Linux (x64/arm64), Windows (x64), Node ≥ 16;
no toolchain or compile step:

```bash
npm install sekejap
```

## First query

```js
const { Db } = require('sekejap');

const db = Db.open('./mydb');
db.execute('CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT)');
db.execute("INSERT INTO places (_key, name) VALUES ('a', 'Uluwatu')");

// Results come back as a JSON string — parse it.
const rows = JSON.parse(db.query('SELECT * FROM places'));
console.log(rows);
```

## Parameters and prepared statements

```js
// $1, $2, … placeholders; params are a JSON array string
const one = JSON.parse(
  db.queryParams('SELECT * FROM places WHERE _key = $1', JSON.stringify(['a']))
);

// compile once, run many
const stmt = db.prepare('SELECT * FROM places WHERE name = $1');
```

## Beyond SQL

The Node API mirrors the Rust core (camelCase; list arguments are JSON
strings, list results are JSON strings to `JSON.parse`):

```js
// Records and edges
db.put('t/k1', JSON.stringify({_collection: 't', _key: 'k1'}));
db.get('t/k1');                        // JSON string or null
db.linkMeta('t/k1', 't/k2', 'rated', '{"stars": 5}');
JSON.parse(db.edgesFrom('t/k1'));      // [{from, to, type, meta}, ...]

// Bulk loading: one disk sync for the whole batch
db.beginBulk();
db.putMany(JSON.stringify([['t/k1', '{...}'], ['t/k2', '{...}']]));
db.linkMany(JSON.stringify([['t/k1', 't/k2', 'likes']]));
db.endBulk();

// Vectors
db.putVector('t/k1', 'emb', [0.1, 0.9, 0.0]);
db.getVector('t/k1', 'emb');
db.setHnswEfSearch(200);               // recall/speed knob; null = default

// Ranked text search, introspection, maintenance
JSON.parse(db.bm25Search('body', 'grilled healthy', 10));
db.collectionNames(); db.schemaDdl('t');
JSON.parse(db.memoryReport()); db.trimMemory();

// Open modes + lifecycle
const ro = Db.openReadOnly('./mydb');
const paged = Db.openPaged('./big');   // mmap topology: fast open, small memory
db.close();                            // releases the writer lock
```

## Using sekejap with Express and Next.js

**Express** (and Fastify, Koa, plain Node servers): works as-is — open the
database once at startup and use it in your handlers. The engine runs
in-process, so there is no database service to deploy next to your server.

```js
const express = require('express');
const { Db } = require('sekejap');

const db = Db.open('./data');
const app = express();

app.get('/near', (req, res) => {
  res.json(JSON.parse(db.queryParams(
    'SELECT name FROM places WHERE ST_DWithin(geometry, POINT(115.09 -8.83), $1)',
    JSON.stringify([Number(req.query.m ?? 5000)])
  )));
});

app.listen(3000);
```

**Next.js**: works in the Node runtime — API routes, route handlers, server
components, and server actions. One config line is needed. Next.js bundles
server code at build time, but sekejap's engine is a native addon (a compiled
`.node` file) that Node must load from disk — it cannot be bundled. Tell
Next.js to leave it external:

```js
// next.config.js  (Next 15+)
module.exports = { serverExternalPackages: ['sekejap'] };
// Next 14: experimental: { serverComponentsExternalPackages: ['sekejap'] }
```

The same line is needed by every native package (better-sqlite3, sharp, …).
Two boundaries to know: the Edge runtime (middleware, edge functions) cannot
load native addons, and client components run in the browser — keep sekejap
calls in server-side code.

Full API: [`wrappers/node/`](../../../wrappers/node/) (typed —
`index.d.ts` is generated from the binding). Runnable examples:
[`tour.js`](../../../wrappers/node/examples/tour.js) (the five-stop tour),
[`express-server.js`](../../../wrappers/node/examples/express-server.js), and a
minimal [Next.js project](../../../wrappers/node/examples/nextjs/).
