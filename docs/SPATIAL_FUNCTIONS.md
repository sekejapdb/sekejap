# Spatial functions — E4 vs PostGIS decision table

G1: pure geometry functions over `kernel::spatial::Geom` (lon/lat degrees,
WGS84, rings implicitly closed), unit-matched to PostGIS. Live server used for
the fixture and cross-checks: `POSTGIS="3.4.3 e365945" ... GEOS="3.9.0" PROJ="7.2.1"`
(from `SELECT PostGIS_Full_Version()` on `postgres://127.0.0.1:55432/postgres`).

The owner's requirement is "the same units as PostGIS". PostGIS gives two
different answers to almost every question depending on whether you cast to
`geography` (spheroidal, metres) or leave the value as `geometry` (planar,
degrees-as-Cartesian). The table below states, function by function, which
one PostGIS itself defaults to, and which one E4 matches — because for three
predicates (`ST_Within`/`ST_Contains`/`ST_Crosses`) PostGIS's own "geography"
overloads do not exist; the geography value is silently cast to geometry and
answered on the **planar** lon/lat plane. E4 matches that, deliberately,
because "same units as PostGIS" means the geography-input behaviour PostGIS
actually has, not a spheroidal predicate PostGIS never shipped.

| E4 function | Unit | PostGIS call matched | Planar or spheroid | Notes |
|---|---|---|---|---|
| `area_m2` | m² | `ST_Area(g::geography)` | Spheroid (WGS84 ellipsoid, authalic-sphere spherical excess) | Holes (rings after the first) subtract. MultiPolygon sums parts. |
| `length_m` | m | `ST_Length(g::geography)` | Spheroid (geodesic edges) | 0 for Point/Polygon/MultiPoint, per PostGIS. |
| `perimeter_m` | m | `ST_Perimeter(g::geography)` | Spheroid (geodesic edges) | Sum of every ring (outer + holes) of every polygon part; 0 for non-areal. |
| `distance_m` | m | `ST_Distance(a::geography, b::geography)` | Spheroid (Karney geodesic) | Minimum geodesic distance over all part/edge pairs (point-to-point, point-to-segment, segment-to-segment via endpoint+closest-point sampling — see module docs for the exact method and its limits). 0 when the geometries overlap. |
| `centroid` | lon/lat degrees | `ST_Centroid(g::geography)` | Spheroid (PostGIS ≥2.x computes geography centroids on the sphere) | E4 computes an area/length-weighted centroid in the same spirit; see module doc for the per-type formula and its divergence from a naive vertex average. |
| `centroid_distance_m` | m | `ST_Distance(ST_Centroid(a::geography)::geography, ST_Centroid(b::geography)::geography)` | Spheroid | Composition of the two functions above. |
| `within` | bool | `ST_Within(a::geometry, b::geometry)` | **Planar** | No `geography` overload in PostGIS — casts silently. Vertex/edge test on raw lon/lat as Cartesian coordinates. |
| `contains` | bool | `ST_Contains(a::geometry, b::geometry)` | **Planar** | Same cast-to-geometry behaviour; `contains(a, b) == within(b, a)` by construction (duality). |
| `covers` | bool | `ST_Covers(a::geometry, b::geometry)` | **Planar** | No `geography` overload. Boundary-inclusive `contains` (a shared boundary point still counts). |
| `crosses` | bool | `ST_Crosses(a::geometry, b::geometry)` | **Planar** | No `geography` overload. Dimensionally-heterogeneous intersection with interior overlap in both directions; PostGIS defines it only for differing-dimension pairs, and returns false for same-dimension pairs (e.g. polygon/polygon) — E4 matches that. |
| `intersects` (`intersects_geography`) | bool | `ST_Intersects(a::geography, b::geography)` | Spheroid | PostGIS *does* ship a geography overload (spheroidal edges via GEOS on a sphere). This is the geography-input default and the one E4's plain `intersects` matches. |
| `intersects_planar` | bool | `ST_Intersects(a::geometry, b::geometry)` | Planar | Exposed separately (named explicitly) because the geometry cast form disagrees with the geography form near geodesic edges spanning >0 curvature — callers who need PostGIS's planar answer (e.g. matching `within`/`contains`/`crosses`, which have no geography form) use this one, not `intersects`. |
| `dwithin_m` | bool | `ST_DWithin(a::geography, b::geography, radius)` | Spheroid | `distance_m(a, b) <= radius`; radius in metres. |

## Where PostGIS's own default is planar

`ST_Within`, `ST_Contains`, `ST_Covers`, and `ST_Crosses` have **no**
`geography` overload at all in PostGIS 3.4. Calling them with two
`geography` arguments compiles (implicit cast) but silently answers on the
**planar** lon/lat plane, not the spheroid — the same lon/lat values treated
as flat Cartesian coordinates. This is a real PostGIS behaviour, not a
limitation E4 is working around, and worth the owner knowing: a polygon that
"contains" a point near the antimeridian or a pole under `ST_Contains` may
disagree with what `ST_DWithin`/`ST_Intersects(geography)` say about the same
pair, because those two families are quietly using different math. E4
reproduces this split exactly (`within`/`contains`/`covers`/`crosses` planar,
`intersects`/`dwithin_m`/`distance_m`/`area_m2`/`length_m`/`perimeter_m`
spheroidal) rather than "fixing" it, since the owner's ask is unit-for-unit
parity with PostGIS as it actually behaves.

## Tolerances (owner spec)

- distances / lengths / perimeters: 1e-6 relative or 1 mm absolute, whichever
  is looser
- areas: 1e-6 relative
- centroids: 1e-9 degrees
- predicates: exact (no tolerance — boolean)
