# Spatial: sekejap vs PostGIS (method-comparable ST_DWithin)

Reference comparison of sekejap's spatial engine against **PostGIS geography**
(the standard spatial baseline), on real Indonesian administrative-boundary
polygons. sekejap runs **embedded, paged, bounded-RAM**; PostGIS runs as a full
Postgres server. Both use nearest-boundary `ST_DWithin` in metres.

## Datasets (negeriku.id BPS 2020 admin boundaries)
- **adm3** — 7,069 district polygons (287 MB GeoJSON)
- **villages_simplified** — 83,486 village polygons (470 MB GeoJSON)

## Reproduce
```bash
# sekejap: build once, serve paged (bounded)
cargo build --release --example spatial_bench
target/release/examples/spatial_bench build <layer>.geojson /tmp/skj-db
target/release/examples/spatial_bench serve /tmp/skj-db paged     # RSS + hits + latency

# PostGIS baseline
docker run -d --name skj-postgis -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=gis \
  -p 5433:5432 postgis/postgis:16-3.4
ogr2ogr -f PostgreSQL "PG:host=localhost port=5433 dbname=gis user=postgres password=postgres" \
  <layer>.geojson -nln area -nlt PROMOTE_TO_MULTI -lco GEOMETRY_NAME=geog \
  -lco GEOM_TYPE=geography -lco SPATIAL_INDEX=GIST -overwrite
psql ... -c "SELECT count(*) FROM area WHERE ST_DWithin(geog, ST_SetSRID(ST_MakePoint(lon,lat),4326)::geography, R);"
```

## Result — hit counts match PostGIS

`ST_DWithin(geometry, POINT(lon lat), R metres)`; query points = Indonesian cities.

**adm3 (7,069 polygons):**

| query | sekejap | PostGIS |
|---|---:|---:|
| Jakarta r=1 km | 4 | 4 |
| Jakarta r=25 km | 102 | 102 |
| Jakarta r=100 km | 397 | 399 |
| Denpasar r=25 km | 28 | 29 |
| Denpasar r=100 km | 74 | 74 |

**villages_simplified (83,486 polygons):**

| query | sekejap | PostGIS |
|---|---:|---:|
| Jakarta r=1 km | 6 | 6 |
| Jakarta r=5 km | 96 | 96 |
| Jakarta r=25 km | 580 | 580 |
| Denpasar r=5 km | 46 | 46 |
| Denpasar r=25 km | 250 | 251 |

Residual ±1–3 at the exact radius boundary = sekejap's spherical (haversine)
segment distance vs PostGIS's spheroidal (WGS84 Vincenty). Upgrading
`point_to_segment_m` to Vincenty would close it to float epsilon; the *method*
(nearest-boundary, not centroid) is identical.

## Result — bounded RAM (the edge story)

sekejap **paged** post-open RSS (serve-time resident, grid built, before queries):

| dataset | sekejap paged | sekejap heap | note |
|---|---:|---:|---|
| adm3 (7,069) | **8.8 MB** | 362 MB | heap eagerly caches rings (fast, RAM-rich servers) |
| villages (83,486) | **34.8 MB** | — | sublinear; eager caching would be ~3–4 GB |

PostGIS is a server process (shared_buffers etc.), not embeddable under a fixed
edge RAM ceiling the way `open_paged` is. This is the paper's core distinction:
**server-class spatial retrieval, embedded, within an edge RAM budget.**

## Latency
Small/medium radii (the common edge case) are 2–28 ms. Large-radius queries
returning thousands of complex polygons are slower (exact nearest-boundary over
full geometry; e.g. Jakarta r=100 km / 3,456 villages ≈ 1.0 s cold) — competitive
with PostGIS cold (adm3 r=100 km: sekejap 150 ms vs PostGIS 573 ms). A bounded
LRU ring cache (planned) recovers hot-query latency without unbounding RAM.

## Engine changes behind this
- `perf(spatial)` 82ae449 — gate eager ring caching by mode (heap eager / paged lazy).
- `fix(spatial)` bd50600 — `ST_DWithin` nearest-boundary (PostGIS semantics) +
  exact-bounds short-circuit.
