# Benchmark #3 — Spatial

Seven spatial operations over global points + city polygons, across the operations that
**all three engines support**. Every number is oracle-checked over **300 seeded, varied queries
per operation** (not a single fixed probe), so latencies are the whole distribution.

**Units are PostGIS `geography` (WGS84 ellipsoid): metres, m².** sekejap's spatial layer was
re-based on PostGIS — geodesic distance (Vincenty), geodesic area (authalic-sphere), geodesic
perimeter — so its values equal PostGIS to floating-point precision (distance rel err 5e-11,
area 1e-7). `ST_DISTANCE_KM` was removed; `ST_Length(polygon)=0`; `ST_Perimeter` added.

## Engines
- **PostGIS** — the spatial gold standard (GiST R-tree, `<->` kNN, `geography`); the geodesic
  **correctness oracle** for radius/area/perimeter.
- **DuckDB spatial** — embedded columnar + `spatial` ext (GEOS).
- **sekejap** — embedded; in-memory spatial grid; SQL `ST_*` (field-vs-literal), JOIN-free.

## Data (WGS84)
- **geonames** 2,000,000 points (terrain-filtered → clusters in mountains).
- NYC: census **blocks** 38,794 polygons, **neighborhoods (nbh)** 129, **subway** 491 pts,
  **homicides** 3,984 pts.

## Methodology
Embedded DuckDB reads parquet, reprojects NYC (26918→4326), serves geometry to PostGIS and
sekejap, and computes planar oracles. Geodesic oracles (radius/area/perimeter) come from
**live PostGIS `::geography`** (DuckDB lacks reliable geodesic area — see findings).
- p50/p99/mean over 300 seeded queries/op; load/index excluded from query latency.
- Gating: planar counts exact; geometric predicates (pip/intersects) ±1 count (boundary-touch
  DE-9IM ambiguity); geodesic (radius/area/perimeter) ±1% (cross-engine method differences).
- **Each query is oracle-checked; mismatches are counted per operation** (TRAIN mode counts,
  it does not hard-fail the run). **sekejap has 0 mismatches on all 7 operations.**

## Results — 300 seeded queries/op (p50 ms; lower better; ✗ = mismatches; 🏆 winner)

| operation | DuckDB | PostGIS | **sekejap** | winner |
|-----------|-------:|--------:|------------:|--------|
| **pip** (point-in-polygon) | 1.25 | 1.34 | **0.22** | 🏆 sekejap |
| **bbox** (coord range) | **7.98** | 245.4 | 15.6 | DuckDB |
| **radius** (ST_DWithin, geodesic) | 757 (131✗) | 2539.8 | **2.82** | 🏆 sekejap |
| **kNN** (10 nearest) | 116.6 | **1.34** | 5.39 | PostGIS |
| **intersects** (polygon×polygon) | **0.97** | 2.22 | 24.1 | DuckDB |
| **area** (ST_Area, geodesic m²) | N/A (300✗) | 12.5 | **7.77** | 🏆 sekejap |
| **perimeter** (ST_Perimeter, geodesic m) | N/A (267✗) | 11.8 | **8.85** | 🏆 sekejap |

**mean ms** (tail-sensitive):

| operation | DuckDB | PostGIS | sekejap |
|-----------|-------:|--------:|--------:|
| pip | 1.51 | 1.49 | 0.32 |
| bbox | 10.1 | 258.7 | 18.6 |
| radius | 791.6 | 2607.2 | 36.7 |
| kNN | 130.3 | 1.59 | 7.20 |
| intersects | 2.42 | 3.49 | 22.6 |
| area | N/A | 15.5 | 9.45 |
| perimeter | N/A | 12.9 | 14.2 |

**Correctness:** sekejap = **0 mismatches on all 7** — its geodesic values match the PostGIS
`geography` oracle exactly. sekejap load 64.7 s · index 49.3 s · RSS 2.0 GB (non-paged).

## Findings
Framing per the paper: *practical enough while embedded + disk-first, not "we beat PostGIS."*
Yet on the shared operation set sekejap now leads on **4 of 7**:
- **Point-in-polygon (0.22 ms) — fastest, ~6× ahead** of both. Collection-first exact filter
  over cached polygon rings (zero payload reads).
- **Radius (2.82 ms) — fastest by a wide margin: ~270× vs DuckDB, ~900× vs PostGIS.** Grid
  candidate prune + exact geodesic (Vincenty) filter. PostGIS `geography` radius does a full
  geodesic scan (2.5 s); DuckDB's spheroid distance is both slow and diverges (131 ✗).
- **Area (7.77 ms) & Perimeter (8.85 ms) — fastest, and correct.** sekejap's geodesic values
  equal PostGIS. **DuckDB has no working geodesic area/perimeter in this build**
  (`ST_Area_Spheroid`/`ST_Transform` fail → N/A) — a real capability gap, not just slowness.
- **kNN (5.39 ms) — 22× faster than DuckDB; ~4× behind PostGIS's GiST `<->`** (its home turf).
- **bbox (15.6 ms) — 16× faster than PostGIS; ~2× behind DuckDB's columnar SIMD scan.**
- **intersects (24.1 ms) — the one clear weak spot.** Polygon×polygon runs a full 129-row scan
  re-parsing each polygon (no cached-ring path for `ST_Intersects` yet, unlike PIP). DuckDB
  (0.97 ms) and PostGIS (2.2 ms) use spatial indexes. Fixable by extending the PIP ring cache
  to `ST_Intersects`; deferred.

## Operations sekejap does NOT support yet (roadmap)
Benchmarked only the intersection all three engines support. sekejap's spatial surface is still
missing (PostGIS/DuckDB have these):
- **Measurement:** `ST_Centroid`, `ST_PointOnSurface`
- **Topology (DE-9IM):** `ST_Overlaps`, `ST_Touches`, `ST_Crosses`, `ST_Disjoint`, `ST_Equals`,
  `ST_Covers`/`ST_CoveredBy`
- **Proximity:** nearest-neighbour **join** (lateral), `ST_HausdorffDistance`, `ST_FrechetDistance`
- **Aggregation/clustering:** `ST_Union`/dissolve, `ST_ClusterDBSCAN`, `ST_ClusterKMeans`,
  spatial GROUP-BY density grid, `ST_Extent`/`ST_Envelope`
- **Construction/transform:** `ST_Buffer`, `ST_Intersection`, `ST_Difference`, `ST_ConvexHull`,
  `ST_Simplify`, `ST_Transform`
- **Note:** the point↔region **spatial join** (`ST_Contains` join) is intentionally expressed
  the graph-first way — build containment as edges once, then `MATCH (a)-[:in]->(b)` — see the
  graph/hybrid evaluation; not a JOIN on a JOIN-free engine.

## Next
- **`ST_Intersects` cached-ring path** — close the one perf gap (24 ms → sub-ms), same technique
  as PIP.
- **Disk-first STR-packed R-tree** — sub-ms kNN parity with PostGIS + bounds the 2 GB RAM.
- Implement the roadmap topology/measurement functions to widen the comparable surface.

## Raw
CSV: `results/spatial-7op.csv` · log: `results/spatial-7op.log` (both mirrored to
benchmark environment). Harness: `harness/spatialbench` (`TRAIN=N` seeded; geodesic ops
oracle-checked vs live PostGIS geography).

_Last updated: 2026-08-04. Status: **7-op comparison DONE.** sekejap re-based on PostGIS
geography (values match to float precision, 0 mismatches). **sekejap wins 4 of 7 shared spatial
operations (pip/radius/area/perimeter) and is a practical embedded spatial engine** — not "beats
PostGIS overall"; it trails the specialists on their turf (kNN → GiST R-tree, bbox → columnar)
and has one perf gap on intersects._
