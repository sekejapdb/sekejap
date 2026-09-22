// Cross-wrapper micro-benchmark, Node.js (koffi over libsekejap). Same
// shape as e1's bench.cjs so results stay comparable across wrappers: warm
// once, then time N repeats of one point lookup by key, reporting
// queries/sec and microseconds/query. Uses Db.get (sekejap_get), the direct
// collection+key read — no SQL, no index, so this measures the FFI/JSON
// round trip rather than the query planner.
//
//   SEKEJAP_LIB_DIR=/path/to/libsekejap node bench.cjs
//   N=100000 SEKEJAP_LIB_DIR=/path/to/libsekejap node bench.cjs
const os = require('os');
const fs = require('fs');
const path = require('path');
const { Db } = require('./index.js');

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'skbench-node-'));
const db = Db.open(dir);
db.createCollection('t', [{ name: 'v', kind: 'int' }]);
for (let i = 0; i < 1000; i++) db.put('t', `k${i}`, { v: i });

const n = parseInt(process.env.N || '50000', 10);

db.get('t', 'k500'); // warm

const t0 = process.hrtime.bigint();
for (let i = 0; i < n; i++) db.get('t', 'k500');
const elapsedSeconds = Number(process.hrtime.bigint() - t0) / 1e9;

console.log(`node ${(n / elapsedSeconds).toFixed(0)} ${((elapsedSeconds * 1e6) / n).toFixed(3)}`);

db.close();
