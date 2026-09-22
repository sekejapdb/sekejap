// A tour of sekejap 0.17 from Node.js: SQL, graph, prepared statements, a
// transaction, and the document API (put/get) alongside SQL.
//
//   SEKEJAP_LIB_DIR=/path/to/libsekejap node examples/tour.js
//
// Results are already-parsed JS values — no JSON.parse needed, unlike the
// napi-rs wrapper this replaces.
const { Db } = require('..');
const fs = require('fs');
const os = require('os');
const path = require('path');

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'sekejap-tour-'));
const db = Db.open(dir);
const show = (label, rows) => {
  console.log(label);
  for (const row of rows) console.log('  ', JSON.stringify(row));
};

// ── 1. Core SQL ──────────────────────────────────────────────────────────
// `_key` is never declared: every row has one implicitly, and INSERT names
// it directly (QL_CONTRACT: names starting with `_` are reserved).
db.execute('CREATE TABLE places (name TEXT, area TEXT)');
for (const [key, name, area] of [
  ['uluwatu', 'Uluwatu Temple', 'south'],
  ['kuta', 'Kuta Beach', 'south'],
  ['ubud', 'Ubud Center', 'central'],
]) {
  db.execute('INSERT INTO places (_key, name, area) VALUES ($1, $2, $3)', [key, name, area]);
}
show('places per area:', db.query('SELECT area, COUNT(*) AS n FROM places GROUP BY area ORDER BY n DESC'));

// ── 2. The document API — no SQL, same rows CREATE TABLE describes ──────
db.put('places', 'seminyak', { name: 'Seminyak', area: 'south' });
console.log('seminyak via get():', db.get('places', 'seminyak'));

// ── 3. Graph ─────────────────────────────────────────────────────────────
db.execute('CREATE TABLE tourists (name TEXT)');
db.execute('CREATE TABLE flights (airline TEXT)');
db.execute("INSERT INTO tourists (_key, name) VALUES ('chloe', 'Chloe')");
db.execute("INSERT INTO flights (_key, airline) VALUES ('qf-mel', 'Qantas')");
db.link('tourists', 'chloe', 'flew_on', 'flights', 'qf-mel');
show("Chloe's flight (Db.neighbours):", db.neighbours('tourists', 'chloe', 'flew_on', 'outgoing', 10));

// ── 4. Prepared statement — parse once, bind many ────────────────────────
db.execute('CREATE INDEX places_area ON places USING btree (area)');
const byArea = db.prepare('SELECT name FROM places WHERE area = $1');
for (const area of ['south', 'central']) {
  show(`prepared: places in ${area}:`, byArea.query([area]));
}
byArea.close();

// ── 5. A transaction — many writes, one barrier ──────────────────────────
const tx = db.transaction();
tx.execute("INSERT INTO places (_key, name, area) VALUES ('canggu', 'Canggu', 'south')");
tx.execute("INSERT INTO places (_key, name, area) VALUES ('sanur', 'Sanur', 'south')");
tx.commit();
console.log('rows in places after the transaction:', db.countRows('places'));

db.close();
