import { Db } from 'sekejap';
import fs from 'fs'; import os from 'os'; import path from 'path';

// One engine per server process (globalThis survives dev-mode reloads).
const db = globalThis.__sekejap ??= (() => {
  const d = Db.open(fs.mkdtempSync(path.join(os.tmpdir(), 'sk-next-')));
  d.execute('CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT, geometry GEO)');
  d.execute('CREATE INDEX ON places USING spatial (geometry)');
  for (const [k, n, lon, lat] of [
    ['uluwatu', 'Uluwatu Temple', 115.087, -8.829],
    ['kuta', 'Kuta Beach', 115.168, -8.720],
    ['ubud', 'Ubud Center', 115.263, -8.507],
  ]) d.put(`places/${k}`, JSON.stringify({ _collection: 'places', _key: k, name: n,
      geometry: { type: 'Point', coordinates: [lon, lat] } }));
  return d;
})();

export function GET(request) {
  const m = Number(new URL(request.url).searchParams.get('m') ?? 5000);
  const rows = JSON.parse(db.queryParams(
    'SELECT name FROM places WHERE ST_DWithin(geometry, POINT(115.087 -8.829), $1)',
    JSON.stringify([m])
  ));
  return Response.json({ rows });
}
