// Cross-wrapper micro-benchmark, Node.js (napi-rs). See bench_native.rs.
const os = require('os'), fs = require('fs'), path = require('path');
const { Db } = require('./sekejap.node');

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'skbench-node-'));
const db = Db.open(dir);
db.execute("CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)");
for (let i = 0; i < 1000; i++) db.execute(`INSERT INTO t (_key, v) VALUES ('k${i}', ${i})`);

const n = parseInt(process.env.N || '50000', 10);
const sql = "SELECT v FROM t WHERE _key = 'k500'";
// Raw result string (no JSON.parse) to match the other bindings — measures the
// binding round-trip, not language-specific JSON parsing.
db.query(sql); // warm

const t = process.hrtime.bigint();
for (let i = 0; i < n; i++) db.query(sql);
const el = Number(process.hrtime.bigint() - t) / 1e9;
console.log(`node ${(n / el).toFixed(0)} ${(el * 1e6 / n).toFixed(3)}`);
