'use strict';

// End-to-end test for the sekejap Node.js wrapper, over the real prebuilt
// libsekejap (no mocks). Exercises: open, create a collection, put, get,
// query with a parameter, scan, prepare + rebind, link + neighbours, tx
// commit/rollback, count_rows, an error path that surfaces last_error, and
// close — every leg of the common wrapper checklist.
//
//   SEKEJAP_LIB_DIR=/path/to/libsekejap node test.cjs
// or with the library already on the loader's search path:
//   DYLD_LIBRARY_PATH=/path/to/libsekejap node test.cjs

const assert = require('assert');
const os = require('os');
const fs = require('fs');
const path = require('path');
const { Db, SekejapError, SekejapStatus, version, formatVersion } = require('./index.js');

let passed = 0;
function step(label, fn) {
  fn();
  passed += 1;
  console.log(`ok - ${label}`);
}

console.log(`sekejap ${version()} (disk format ${formatVersion()})`);

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'sekejap-node-test-'));

// ── open ─────────────────────────────────────────────────────────────────
let db;
step('open', () => {
  db = Db.open(dir);
  assert.ok(db, 'Db.open returned a handle');
});

// ── create a collection ─────────────────────────────────────────────────
step('create a collection', () => {
  const created = db.createCollection('notes', [
    { name: 'title', kind: 'text' },
    { name: 'pinned', kind: 'bool' },
  ]);
  assert.strictEqual(created, true, 'first create_collection reports created');
  const againNotCreated = db.createCollection('notes', [{ name: 'title', kind: 'text' }]);
  assert.strictEqual(againNotCreated, false, 'second create_collection reports already-there');
  // QL_CONTRACT §1: every Tier-1 predicate on an indexed field is answered
  // index-side, so an equality WHERE needs a named index first.
  db.execute('CREATE INDEX notes_pinned ON notes USING btree (pinned)');
});

// ── put ──────────────────────────────────────────────────────────────────
step('put', () => {
  db.put('notes', 'n1', { title: 'Buy milk', pinned: true });
  db.put('notes', 'n2', { title: 'Call Sam', pinned: false });
  db.put('notes', 'n3', { title: 'Walk the dog', pinned: true });
});

// ── get ──────────────────────────────────────────────────────────────────
step('get', () => {
  const row = db.get('notes', 'n1');
  assert.strictEqual(row.title, 'Buy milk');
  assert.strictEqual(row._key, 'n1');
  const miss = db.get('notes', 'does-not-exist');
  assert.strictEqual(miss, null, 'a miss is null, not a thrown error');
});

// ── query with a parameter ──────────────────────────────────────────────
step('query with a parameter', () => {
  const rows = db.query('SELECT _key, title FROM notes WHERE pinned = $1', [true]);
  assert.strictEqual(rows.length, 2);
  assert.deepStrictEqual(
    rows.map((r) => r._key).sort(),
    ['n1', 'n3']
  );
});

// ── scan ─────────────────────────────────────────────────────────────────
step('scan', () => {
  const scan = db.scan('notes', 2); // small page size to force >1 page
  let seen = 0;
  let pages = 0;
  for (const page of scan) {
    pages += 1;
    seen += page.length;
  }
  scan.close();
  assert.strictEqual(seen, 3, 'scan visited every row');
  assert.ok(pages >= 2, `paging worked (page_rows=2 over 3 rows): ${pages} pages`);
});

// ── prepare + rebind ─────────────────────────────────────────────────────
step('prepare + rebind', () => {
  const stmt = db.prepare('SELECT _key FROM notes WHERE pinned = $1');
  assert.strictEqual(stmt.rebindable(), null, 'unbound statement reports SEKEJAP_REBIND_UNBOUND as null');
  const first = stmt.query([true]);
  assert.strictEqual(first.length, 2);
  assert.strictEqual(stmt.rebindable(), true, 'a read-only statement rebinds without recompiling');
  const second = stmt.query([false]);
  assert.strictEqual(second.length, 1);
  assert.strictEqual(second[0]._key, 'n2');
  stmt.close();
});

// ── link + neighbours ────────────────────────────────────────────────────
step('link + neighbours', () => {
  db.createCollection('tags', [{ name: 'label', kind: 'text' }]);
  db.put('tags', 'errand', { label: 'errand' });
  db.link('notes', 'n1', 'tagged', 'tags', 'errand');
  db.link('notes', 'n3', 'tagged', 'tags', 'errand');
  const neighbours = db.neighbours('tags', 'errand', 'tagged', 'incoming', 10);
  assert.strictEqual(neighbours.length, 2);
  const keys = neighbours.map((n) => n.key).sort();
  assert.deepStrictEqual(keys, ['n1', 'n3']);
  const removed = db.unlink('notes', 'n1', 'tagged', 'tags', 'errand');
  assert.strictEqual(removed, true);
  const removedAgain = db.unlink('notes', 'n1', 'tagged', 'tags', 'errand');
  assert.strictEqual(removedAgain, false, 'a second unlink of the same edge answers false, not an error');
});

// ── tx commit/rollback ───────────────────────────────────────────────────
step('tx commit', () => {
  const tx = db.transaction();
  tx.put('notes', 'n4', { title: 'Committed note', pinned: false });
  tx.put('notes', 'n5', { title: 'Also committed', pinned: false });
  tx.commit();
  assert.ok(db.exists('notes', 'n4'));
  assert.ok(db.exists('notes', 'n5'));
});

step('tx rollback', () => {
  const tx = db.transaction();
  tx.put('notes', 'n6', { title: 'Never should land', pinned: false });
  tx.rollback();
  assert.strictEqual(db.exists('notes', 'n6'), false, 'a rolled-back write never lands');
});

// ── count_rows ───────────────────────────────────────────────────────────
step('count_rows', () => {
  const live = db.countRows('notes');
  const walked = db.scanCountRows('notes');
  assert.strictEqual(live, 5, 'n1..n5 (n6 was rolled back)');
  assert.strictEqual(walked, live, 'the live record and the walk agree');
  const edges = db.scanCountEdges();
  assert.strictEqual(edges, 1, 'one edge remains: n3 -> errand');
});

// ── an error path that surfaces last_error ──────────────────────────────
step('an error path surfaces last_error', () => {
  let threw = null;
  try {
    db.execute('SELECT this is not valid SQL');
  } catch (e) {
    threw = e;
  }
  assert.ok(threw instanceof SekejapError, 'a SQL syntax error throws SekejapError');
  assert.ok(threw.message.length > 0, 'the message is non-empty');
  assert.strictEqual(threw.status, SekejapStatus.Invalid, 'a syntax error maps to Invalid');
  assert.strictEqual(threw.code, 'Invalid');

  // A missing endpoint on link() is UnknownRow, exercised as a second,
  // differently-coded error path.
  let linkErr = null;
  try {
    db.link('notes', 'n1', 'tagged', 'tags', 'does-not-exist');
  } catch (e) {
    linkErr = e;
  }
  assert.ok(linkErr instanceof SekejapError);
  assert.strictEqual(linkErr.code, 'UnknownRow');

  // A refusal by name (no atomic underneath) is Refused, never an empty answer.
  let refusalErr = null;
  try {
    db.compact();
  } catch (e) {
    refusalErr = e;
  }
  assert.ok(refusalErr instanceof SekejapError);
  assert.strictEqual(refusalErr.code, 'Refused');
});

// ── stream (paged SELECT) ────────────────────────────────────────────────
step('stream', () => {
  const scan = db.stream('SELECT _key FROM notes', undefined, 2);
  const keys = [...scan.rows()].map((r) => r._key).sort();
  scan.close();
  assert.deepStrictEqual(keys, ['n1', 'n2', 'n3', 'n4', 'n5']);
});

// ── storage / checkpoint / publish ──────────────────────────────────────
step('storage + checkpoint + publish', () => {
  const storage = db.storage();
  assert.ok(storage.totalBytes > 0);
  db.publish(); // single mode: succeeds having done nothing
  db.checkpoint(); // true (folded) or false (deferred) are both success
});

// ── refused-by-name calls surface a named reason, never a fake answer ────
step('open_memory is refused by name', () => {
  let threw = null;
  try {
    Db.openMemory();
  } catch (e) {
    threw = e;
  }
  assert.ok(threw instanceof SekejapError);
  assert.strictEqual(threw.code, 'Refused');
});

// ── close ────────────────────────────────────────────────────────────────
step('close', () => {
  db.close();
  // A second close is null-safe.
  db.close();
});

console.log(`\n${passed} steps passed — sekejap-node ${version()} over ${dir}`);
