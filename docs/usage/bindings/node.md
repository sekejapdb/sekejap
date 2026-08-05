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

Direct record and edge operations (`put`, `get`, `link`) work the same as in
the other bindings. Full API: [`wrappers/node/`](../../../wrappers/node/); runnable tour:
[`wrappers/node/examples/tour.js`](../../../wrappers/node/examples/tour.js).
