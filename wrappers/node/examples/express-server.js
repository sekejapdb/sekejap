// sekejap + Express — an embedded multi-model database behind a web API.
// The engine runs in-process with the server: no database service to deploy.
//
//   npm install sekejap express
//   node express-server.js
//
//   curl 'localhost:3000/near?m=20000'
//   curl 'localhost:3000/similar'
//   curl 'localhost:3000/search?q=grilled+healthy'
const express = require('express');
const { Db } = require('sekejap');
const fs = require('fs'), os = require('os'), path = require('path');

// In-memory demo dir; use a fixed path like './data' in a real app.
const db = Db.open(fs.mkdtempSync(path.join(os.tmpdir(), 'sekejap-express-')));

db.execute('CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT, description TEXT, geometry GEO, emb VECTOR)');
db.execute('CREATE INDEX ON places USING spatial (geometry)');
db.execute('CREATE INDEX ON places USING hnsw (emb)');
db.execute('CREATE INDEX ON places USING bm25 (description)');

db.beginBulk();
for (const [key, name, desc, lon, lat, emb] of [
  ['uluwatu', 'Uluwatu Temple', 'clifftop sea temple, sunset kecak dance', 115.087, -8.829, [1.0, 0.0]],
  ['kuta', 'Kuta Beach', 'long sandy beach, surf schools, sunsets', 115.168, -8.720, [0.0, 1.0]],
  ['ubud', 'Ubud Center', 'rice terraces, healthy cafes, yoga', 115.263, -8.507, [0.9, 0.1]],
]) {
  db.put(`places/${key}`, JSON.stringify({
    _collection: 'places', _key: key, name, description: desc,
    geometry: { type: 'Point', coordinates: [lon, lat] },
  }));
  db.putVector(`places/${key}`, 'emb', emb);
}
db.endBulk();

const app = express();

// Places within m metres of Uluwatu.
app.get('/near', (req, res) => {
  res.json(JSON.parse(db.queryParams(
    'SELECT name FROM places WHERE ST_DWithin(geometry, POINT(115.087 -8.829), $1)',
    JSON.stringify([Number(req.query.m ?? 5000)])
  )));
});

// The 2 places most similar to the [1, 0] embedding.
app.get('/similar', (_req, res) => {
  res.json(JSON.parse(db.query('SELECT name FROM places WHERE VECTOR_NEAR(emb, [1.0,0.0], 2)')));
});

// Ranked text search over descriptions.
app.get('/search', (req, res) => {
  res.json(JSON.parse(db.bm25Search('description', String(req.query.q ?? ''), 10)));
});

app.listen(3000, () => console.log('sekejap + express on :3000'));
