const assert = require('assert');
const os = require('os'), fs = require('fs'), path = require('path');
const { Db, version } = require('./sekejap.node');

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'sekejap-node-'));
const db = Db.open(dir);

assert.ok(db.execute("CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)") >= 0);
assert.strictEqual(db.execute("INSERT INTO t (_key, v) VALUES ('a', 42)"), 1);
db.put("t/b", '{"_collection":"t","_key":"b","v":7}');
assert.strictEqual(db.nodeCount(), 2);

const rows = JSON.parse(db.query("SELECT v FROM t WHERE _key = 'a'"));
assert.strictEqual(rows.length, 1);
assert.strictEqual(rows[0].v, 42);

const p = JSON.parse(db.queryParams("SELECT _key FROM t WHERE v = $1", "[7]"));
assert.strictEqual(p.length, 1, "params query");

db.link("t/a", "t/b", "near");
assert.strictEqual(db.edgeCount(), 1);

let threw = false;
try { db.execute("SELECT bad syntax FROM"); } catch (e) { threw = true; }
assert.ok(threw, "bad statement should throw");

db.compact();
console.log("OK — sekejap-node", version(), "— all assertions passed");
