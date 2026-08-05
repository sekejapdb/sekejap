// A five-stop tour of sekejap from Node.js: SQL, graph, spatial, vector, hybrid.
//
//   node wrappers/node/examples/tour.js
//
// Results come back as JSON strings — JSON.parse them.
const { Db } = require('..');
const fs = require('fs');
const os = require('os');
const path = require('path');

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'sekejap-tour-'));
const db = Db.open(dir);
const show = (label, json) => {
  console.log(label);
  for (const row of JSON.parse(json)) console.log('  ', JSON.stringify(row));
};

// ── 1. Core SQL ──────────────────────────────────────────────────────────────
db.execute('CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT, area TEXT)');
for (const [key, name, area] of [
  ['uluwatu', 'Uluwatu Temple', 'south'],
  ['kuta', 'Kuta Beach', 'south'],
  ['ubud', 'Ubud Center', 'central'],
]) {
  db.execute(`INSERT INTO places (_key, name, area) VALUES ('${key}', '${name}', '${area}')`);
}
show('places per area:',
  db.query('SELECT area, COUNT(*) AS n FROM places GROUP BY area ORDER BY n DESC'));

// ── 2. Graph ─────────────────────────────────────────────────────────────────
db.execute('CREATE TABLE tourists (_key TEXT PRIMARY KEY, name TEXT)');
db.execute('CREATE TABLE flights (_key TEXT PRIMARY KEY, airline TEXT)');
db.execute("INSERT INTO tourists (_key, name) VALUES ('chloe', 'Chloe')");
db.execute("INSERT INTO flights (_key, airline) VALUES ('qf-mel', 'Qantas')");
db.execute("INSERT ('tourists/chloe')-[:flew_on]->('flights/qf-mel')");
show("Chloe's flight:",
  db.query("SELECT f.airline AS airline FROM MATCH (t:tourists)-[:flew_on]->(f:flights) WHERE t._key = 'chloe'"));

// ── 3. Spatial (radius in metres) ────────────────────────────────────────────
db.execute('CREATE TABLE spots (_key TEXT PRIMARY KEY, name TEXT, geometry GEO)');
db.execute('CREATE INDEX ON spots USING spatial (geometry)');
for (const [key, name, lon, lat] of [
  ['uluwatu', 'Uluwatu Temple', 115.087, -8.829],
  ['kuta', 'Kuta Beach', 115.168, -8.720],
  ['ubud', 'Ubud Center', 115.263, -8.507],
]) {
  db.put(`spots/${key}`, JSON.stringify({
    _collection: 'spots', _key: key, name,
    geometry: { type: 'Point', coordinates: [lon, lat] },
  }));
}
show('within 20 km of Uluwatu:',
  db.query('SELECT name FROM spots WHERE ST_DWithin(geometry, POINT(115.087 -8.829), 20000.0)'));

// ── 4. Vector ────────────────────────────────────────────────────────────────
db.execute('CREATE TABLE items (_key TEXT PRIMARY KEY, name TEXT, emb VECTOR)');
db.execute('CREATE INDEX ON items USING hnsw (emb)');
for (const [key, name, emb] of [
  ['a', 'apple', '[1.0, 0.0, 0.0]'],
  ['b', 'banana', '[0.0, 1.0, 0.0]'],
  ['c', 'cherry', '[0.9, 0.1, 0.0]'],
]) {
  db.execute(`INSERT INTO items (_key, name, emb) VALUES ('${key}', '${name}', ${emb})`);
}
show('2 nearest to [1,0,0]:',
  db.query('SELECT name FROM items WHERE VECTOR_NEAR(emb, [1.0, 0.0, 0.0], 2)'));

// ── 5. Hybrid: text + vector in one ORDER BY ─────────────────────────────────
db.execute('CREATE TABLE dishes (_key TEXT PRIMARY KEY, name TEXT, description TEXT, embedding VECTOR)');
db.execute('CREATE INDEX ON dishes USING bm25 (description)');
db.execute('CREATE INDEX ON dishes USING hnsw (embedding)');
for (const [key, name, desc, emb] of [
  ['a', 'Grilled Chicken', 'healthy grilled chicken with herbs', '[1.0, 0.0, 0.0]'],
  ['b', 'Fried Rice', 'classic fried rice street food', '[0.0, 1.0, 0.0]'],
  ['c', 'Grilled Fish', 'grilled fish, light and healthy', '[0.8, 0.2, 0.0]'],
]) {
  db.execute(`INSERT INTO dishes (_key, name, description, embedding) VALUES ('${key}', '${name}', '${desc}', ${emb})`);
}
show('ranked dishes:',
  db.query("SELECT name FROM dishes WHERE BM25(description, 'grilled healthy') > 0.0 " +
           "ORDER BY BM25_NORM(description, 'grilled healthy') * 0.6 " +
           '+ VECTOR_COSINE(embedding, [1.0, 0.0, 0.0]) * 0.4 DESC'));
