# Spatial functions — sekejap vs PostGIS decision table

Pure geometry functions over `kernel::spatial::Geom` (lon/lat degrees,
WGS84, rings implicitly closed), implemented in
`core/engine/src/index/spatial/geometry.rs` (public as
`sekejap_core::spatial_geometry`; math helpers in
`core/engine/src/index/spatial/math.rs`, public as
`sekejap_core::spatial_math`). Unit-matched to PostGIS.

Two oracles:

- Unit table below: live server `POSTGIS="3.4.3 e365945" ... GEOS="3.9.0" PROJ="7.2.1"`
  (`SELECT PostGIS_Full_Version()` on `postgres://127.0.0.1:55432/postgres`),
  fixture `tests/fixtures/spatial_postgis.json`, suite
  `tests/spatial_geometry_postgis.rs`.
- Conformance (section below): PostGIS 3.6.4 (`POSTGIS="3.6.4 94d984b"` PGSQL=160
  GEOS=3.10.2 PROJ=8.2.1), 1,020 pairs, fixture
  `tests/fixtures/postgis_conformance.json` (seed 20260919), suite
  `tests/spatial_postgis_conformance.rs`.

The owner's requirement is "the same units as PostGIS". PostGIS gives two
different answers to almost every question depending on whether you cast to
`geography` (spheroidal, metres) or leave the value as `geometry` (planar,
degrees-as-Cartesian). The table below states, function by function, which
one PostGIS itself defaults to, and which one sekejap matches — because for three
predicates (`ST_Within`/`ST_Contains`/`ST_Crosses`) PostGIS's own "geography"
overloads do not exist; the geography value is silently cast to geometry and
answered on the **planar** lon/lat plane. sekejap matches that, deliberately,
because "same units as PostGIS" means the geography-input behaviour PostGIS
actually has, not a spheroidal predicate PostGIS never shipped.

| sekejap function | Unit | PostGIS call matched | Planar or spheroid | Notes |
|---|---|---|---|---|
| `area_m2` | m² | `ST_Area(g::geography)` | Spheroid (WGS84 ellipsoid, authalic-sphere spherical excess) | Holes (rings after the first) subtract. MultiPolygon sums parts. |
| `length_m` | m | `ST_Length(g::geography)` | Spheroid (geodesic edges) | 0 for Point/Polygon/MultiPoint, per PostGIS. |
| `perimeter_m` | m | `ST_Perimeter(g::geography)` | Spheroid (geodesic edges) | Sum of every ring (outer + holes) of every polygon part; 0 for non-areal. |
| `distance_m` | m | `ST_Distance(a::geography, b::geography)` | Spheroid (Karney geodesic) | Minimum geodesic distance over all part/edge pairs (point-to-point, point-to-segment, segment-to-segment via endpoint+closest-point sampling — see module docs for the exact method and its limits). 0 when the geometries overlap. |
| `centroid` | lon/lat degrees | `ST_Centroid(g::geography)` | Spheroid (PostGIS ≥2.x computes geography centroids on the sphere) | sekejap computes an area/length-weighted centroid in the same spirit; see module doc for the per-type formula and its divergence from a naive vertex average. |
| `centroid_distance_m` | m | `ST_Distance(ST_Centroid(a::geography)::geography, ST_Centroid(b::geography)::geography)` | Spheroid | Composition of the two functions above. |
| `within` | bool | `ST_Within(a::geometry, b::geometry)` | **Planar** | No `geography` overload: PostGIS refuses a geography argument. Vertex/edge test on raw lon/lat as Cartesian coordinates. |
| `contains` | bool | `ST_Contains(a::geometry, b::geometry)` | **Planar** | No `geography` overload either; `contains(a, b) == within(b, a)` by construction (duality). |
| `covers` | bool | `ST_Covers(a::geometry, b::geometry)` | **Planar** | Matches the geometry form. PostGIS also has a geography `ST_Covers`, which sekejap does not answer. Boundary-inclusive `contains` (a shared boundary point still counts). |
| `crosses` | bool | `ST_Crosses(a::geometry, b::geometry)` | **Planar** | No `geography` overload. Dimensionally-heterogeneous intersection with interior overlap in both directions; PostGIS defines it only for differing-dimension pairs, and returns false for same-dimension pairs (e.g. polygon/polygon) — sekejap matches that. |
| `intersects` (`intersects_geography`) | bool | `ST_Intersects(a::geography, b::geography)` | Spheroid | PostGIS *does* ship a geography overload (spheroidal edges via GEOS on a sphere). This is the geography-input default and the one sekejap's plain `intersects` matches. |
| `intersects_planar` | bool | `ST_Intersects(a::geometry, b::geometry)` | Planar | Exposed separately (named explicitly) because the geometry cast form disagrees with the geography form near geodesic edges spanning >0 curvature — callers who need PostGIS's planar answer (e.g. matching `within`/`contains`/`crosses`, which have no geography form) use this one, not `intersects`. |
| `dwithin_m` | bool | `ST_DWithin(a::geography, b::geography, radius)` | Spheroid | `distance_m(a, b) <= radius`; radius in metres. |

## The SQL spelling

The functions above are the ENGINE's. What a statement writes is the PostGIS
name, compiled to the point or geometry index filter underneath it
(`docs/lang/QL_CONTRACT.md` §4.4). These run on
[the example fixture](../lang/EXAMPLE_FIXTURE.md), whose `posts` collection
has a `Point` column `loc` and a `Geo` column `area`, each with its own gist
index:

```sql
-- ST_DWithin on a Point column: PointFilter::Radius, geodesic metres.
-- ::geography is what makes the radius metres, in PostGIS and here.
SELECT _key, title FROM posts
  WHERE ST_DWithin(loc, ST_MakePoint(106.85, -6.25)::geography, 5000) LIMIT 5;

-- A rectangle is ST_Within against an envelope: PointFilter::Bbox.
SELECT _key FROM posts
  WHERE ST_Within(loc, ST_MakeEnvelope(106.80, -6.35, 106.95, -6.20, 4326));

-- The same predicates over a Geo column go to the geometry index.
SELECT _key FROM posts
  WHERE ST_Intersects(area, ST_MakeEnvelope(106.80, -6.35, 106.95, -6.20, 4326)::geography);

-- params: ["{\"type\":\"Point\",\"coordinates\":[106.85,-6.25]}"]
SELECT _key FROM posts WHERE ST_Contains(area, ST_GeomFromGeoJSON($1));

-- Nearest first: the ring walk out of one centre, not a sort of every row.
SELECT _key, title FROM posts
  ORDER BY loc <-> ST_MakePoint(106.85, -6.25)::geography ASC LIMIT 3;

-- ST_Distance as a ranking leaf, in geodesic metres.
SELECT _key FROM posts
  ORDER BY ST_Distance(loc, ST_MakePoint(106.85, -6.25)::geography) ASC LIMIT 3
```

## The unit is the type

PostGIS picks a distance's unit from the TYPE of its arguments. On
`geometry` it measures in the SRID's own units, which for 4326 is degrees. On
`geography` it measures metres. A column declared `GEOMETRY(Point,4326)` and
a bare `ST_MakePoint` are both geometry, and geometry is PostGIS's default,
so in PostGIS `ST_DWithin(loc, ST_MakePoint(106.85, -6.25), 5000)` means
"within 5,000 degrees" and matches every row on Earth.

sekejap measures metres only. So it accepts a distance form only when
PostGIS would ALSO read it as metres, and refuses the rest by name with the
spelling to use instead. A statement sekejap runs therefore means the same
thing when it is pasted into PostGIS. Checked against PostGIS 3.4.3:

| Written | PostGIS reads it as | sekejap |
|---|---|---|
| `ST_DWithin(geom, pt::geography, m)` | metres (the column casts to geography implicitly) | runs |
| `ST_DWithin(geom, pt, m, true)` | metres (only the geography form has a fourth argument) | runs |
| `ST_DWithin(geom, pt, m)` | degrees | refused |
| `ST_Distance(geom, pt::geography)` | metres | runs |
| `ST_Distance(geom, pt)` | degrees | refused |
| `geom <-> pt::geography` | metres | runs |
| `geom <-> pt` | degrees | refused |
| `ST_Intersects(geom, shape::geography)` | on the Earth's curve | runs |
| `ST_Intersects(geom, shape)` | on the flat lon/lat plane | refused |
| `ST_Within` / `ST_Contains` with a 4326 shape | on the flat lon/lat plane | runs |
| `ST_Within` / `ST_Contains` with `::geography` | no such function, an error | refused |
| `ST_Within` / `ST_Contains` with a bare `ST_MakePoint` | mixed SRID 0 and 4326, an error | refused |

A shape inside a geography form may leave its SRID off: PostGIS gives a
geography with no SRID 4326. Inside `ST_Within` and `ST_Contains` it may not,
so write `ST_SetSRID(ST_MakePoint(lon, lat), 4326)` or pass the SRID as
`ST_MakeEnvelope`'s fifth argument.

Two things are refused today only because sekejap cannot see them yet, and
may be accepted later without changing any answer. The first is a column
declared `GEOGRAPHY`, where PostGIS reads a bare distance as metres. sekejap
stores the two column types the same way and cannot tell them apart. The
second is the planar reading of geometry distances, which sekejap does not
compute.

One small difference remains in `<->`. PostGIS orders geography by distance on
a sphere, and sekejap orders by distance on the WGS84 spheroid. Two rows
whose distances differ by less than about half a percent can come back in
the other order.

A function in the table above that has no Tier-1 spelling is REFUSED by name,
with the reason, rather than answered from a second implementation:

```sql refused
-- refused 0A000: ST_AsText
SELECT ST_AsText(loc) FROM posts LIMIT 1
```

```sql refused
-- refused 0A000: ST_Buffer
SELECT _key FROM posts WHERE ST_Intersects(area, ST_Buffer(loc, 100))
```

## Where PostGIS's own default is planar

`ST_Within`, `ST_Contains` and `ST_Crosses` have **no** `geography` overload
in PostGIS. Called with a `geography` argument, PostGIS refuses it as a
missing function (checked on 3.4.3: `function st_within(geography, geography)
does not exist`), so they only ever answer on the **planar** lon/lat plane,
the same lon/lat values treated as flat Cartesian coordinates. `ST_Covers`
is the exception: PostGIS does have a geography `ST_Covers`, and sekejap's
`covers` matches only its planar geometry form. This is worth knowing: a
polygon that "contains" a point near the antimeridian or a pole under
`ST_Contains` may disagree with what `ST_DWithin`/`ST_Intersects(geography)`
say about the same pair, because those two families use different math. sekejap
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

The 3.6.4 conformance suite uses the same distance/length/perimeter rule, an
area fallback of 1e-3 m² so a 0 vs 1e-9 pair does not fire, 1e-6 deg for
point/line centroids, and 0.01 deg (~1.1 km) for areal centroids so the
documented 0.48 deg hole-centroid gap remains a disagreement under that
threshold (`tests/spatial_postgis_conformance.rs`).

## Conformance — PostGIS 3.6.4, 1,020 pairs

Oracle: `tests/fixtures/postgis_conformance.json`, generated by
`tools/postgis_conformance_fixture.py` (seed 20260919) against live PostGIS
3.6.4 (`POSTGIS="3.6.4 94d984b"`). The test file never talks to Postgres.
Module under test: `src/index/spatial/geometry.rs`.

1,020 pairs (920 pairwise + 100 singles):

| family | n | contents |
| --- | ---: | --- |
| a nearby polygons | 300 | adjacent / overlapping / centimetre-gap polygons |
| b holes | 100 | polygons with holes vs points on shell, hole, boundary |
| c antimeridian + poles | 150 | wrapping strips, polar squares |
| d degenerate | 100 | unclosed, bowtie, spike, collinear, winding |
| e dwithin | 100 | distance plus dwithin at several radii |
| f multi / linestring | 100 | MultiPolygon, MultiPoint, LineString |
| g large + pole | 70 | 90°+ spans, polar triangles, ±180 rectangles |
| h measures | 100 | area, length, perimeter, centroid (singles) |

Nine disagreement classes. Five were sekejap's own and are **fixed** (the named
`repro_*` tests now assert PostGIS's answer). Four are **documented** (sekejap does
not copy them; each is a named `documented_*` test, and `documented_deviation`
routes the family walks around exactly those four so a green family test means
"nothing disagrees except these four").

| ID | Class | Status | Pin |
| --- | --- | --- | --- |
| C1 | Centimetre meridian gap (~7.4 cm). PostGIS geography snaps to a touch (distance 0); Karney reports 0.074 m. sekejap keeps the metre. | documented | `documented_cm_meridian_gap_postgis_reports_touch` |
| C2 | Constant-latitude edge midpoint. A geography edge is a great circle that bulges poleward of the parallel; the north-edge midpoint is 2.73 cm inside the ring. | fixed | `repro_constant_latitude_north_edge_midpoint`; `documented_constant_latitude_edge_midpoints` (all four edges) |
| C3 | Antimeridian far-point distance (~0.5 % / ~80 km on 15,700 km). | fixed | `repro_antimeridian_far_point_distance` |
| C4 | Unclosed ring. sekejap closes implicitly (`core/kernel/src/spatial.rs`); PostGIS `ST_IsValid` is false and measures three sides. | documented | `documented_unclosed_ring_e4_closes_postgis_does_not` |
| C5 | Invalid rings. GEOS answers Contains on a bowtie; PostGIS drops one edge of a collapsed spike. sekejap does not copy invalid-input behaviour; it measures every named edge. | documented | `documented_bowtie_point_in_self_touching_ring`; `documented_collapsed_spike_ring_distance` |
| C6 | Large-span geodesic interior (117° × 35° rectangle vs a planar-interior square). The small-angle bulge series is not an upper bound past ~30°. | fixed | `repro_large_span_geodesic_interior` |
| C7 | Polar triangle (vertices 120° apart on one parallel) contains the pole under geography. | fixed | `repro_polar_triangle_contains_pole` |
| C8 | ±180 polar rectangle. PostGIS collapses ±180 to one meridian (a polar sliver); a point at (0, 89.999) is outside. | fixed | `repro_dateline_polar_rectangle` |
| C9 | Areal centroid: sekejap planar area-weighted shoelace (holes subtracted by position) vs PostGIS spherical geography centroid. Documented hole gap 0.4777 deg. | documented | `documented_hole_centroid_gap` |

### Great-circle-edge fact

A PostGIS `geography` edge is the **great circle** through its two vertices
(`(lon, lat)` read straight onto a sphere), not the WGS84 geodesic. Only
distances between points are spheroidal. On family G's 117.48°-wide south
edge at latitude 29.503844920576796°, `ST_ClosestPoint` sits at
47.475329257577954° — the great circle's own maximum latitude — and
`ST_Distance` is 25,246.58142148 m. The WGS84 geodesic between the same
vertices tops out 10.2 km further north and would give 35,223.56 m (39 %
error). Pinned by `documented_postgis_edges_are_great_circles`.
`point_to_segment_geodesic_m` interpolates on that great circle.
