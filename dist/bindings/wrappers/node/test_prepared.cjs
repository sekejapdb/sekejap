const assert = require('assert');
const os = require('os'), fs = require('fs'), path = require('path');
const { Db } = require('./index.js');
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'sk-node-prep-'));
const db = Db.open(dir);
db.execute("CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)");
for (let i=0;i<5;i++) db.execute(`INSERT INTO t (_key, v) VALUES ('k${i}', ${i})`);
const stmt = db.prepare("SELECT _key FROM t WHERE v = $1");
for (let i=0;i<5;i++) {
  const rows = JSON.parse(db.queryPrepared(stmt, `[${i}]`));
  assert.strictEqual(rows.length, 1, `param ${i}`);
  assert.strictEqual(rows[0]._key, `k${i}`);
}
console.log("OK — node prepared statements work");
