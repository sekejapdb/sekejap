# Known limits & future concerns

Deliberate simplifications where sekejap does less than a full server DB. Captured
for later — none are urgent, but each is a place a downstream app could
hit an edge, and a place we may invest later.

---

## 1. ~~S3 backend~~ — REMOVED in 0.17.0

Object storage support (`open_s3`, `RemoteSync`, `BlockCache`, `Manifest`) was
removed. It could only ever *read* data in place; making it writable needs the
WAL-segment-upload + manifest-swap + conditional-PUT pattern, which was judged too
costly for sekejap. Dropping it took the dependency tree from 187 crates back to 36.
Read scale-out survives without it: several processes can open the same local
directory read-only (see `docs/usage/connectivity.md`).

## 2. Spatial type system — subtype-less `GEO`, fixed WGS84/4326

This is the one to revisit if spatial becomes a serious axis.

**Today, sekejap's spatial column is intentionally simpler than PostGIS:**

- **No geometry subtype.** The column type is just `GEO`. Unlike PostGIS's
  `GEOMETRY(POINT, 4326)` (which enforces "only points"), a `GEO` column accepts
  *any* GeoJSON geometry, and **different rows may hold different geometry types**
  (a Point in one row, a MultiPolygon in another). Spatial functions dispatch on
  whatever each row actually stores. There is no `CHECK`/typmod to constrain a
  column to one geometry type.
- **No SRID — implicitly EPSG:4326 / WGS84.** `geo.rs` computes on the WGS84
  ellipsoid in metres, matching PostGIS `geography`. Coordinates are always
  lon/lat; distances/areas are metres/m². There is no per-column SRID and no
  projected-CRS support.

**Geometry-type coverage (what "multi form" actually means):**

| form | store + render (EWKB) | distance (`ST_DWithin`) | containment (`ST_Contains`) |
|---|---|---|---|
| Point | ✅ | ✅ | ✅ |
| LineString / MultiLineString | ✅ | ✅ | — (n/a) |
| Polygon / **MultiPolygon** | ✅ | ✅ | ✅ |
| MultiPoint | ✅ | ✅ | — |
| **GeometryCollection** | ✅ (renders) | ❌ falls to empty default | ❌ |
| **mixed types across rows** in one column | ✅ | ✅ (per-row dispatch) | ✅ |

So:
- **Mixing geometry types across rows** in one `GEO` column: fully supported.
- **Multi\* single values** (MultiPoint/MultiLineString/MultiPolygon): supported in
  predicates.
- **`GeometryCollection`** (a heterogeneous mix *inside one value*): **partial** —
  it stores and renders (EWKB tag 7), but the distance/containment predicates hit
  the `_ => vec![]` default (`geo.rs::distance`), so it silently drops out of
  `ST_DWithin`/`ST_Contains`. This is the real gap.

**Future concerns if taking spatial seriously:**
1. Optional PostGIS-style **type constraint** on a column (`GEO(POINT)` /
   `GEO(MULTIPOLYGON)`) with validation on insert.
2. **SRID / projected CRS** support (currently 4326-only geography).
3. **GeometryCollection predicate support** (make `distance`/containment recurse
   into `geometries[]` instead of the empty default).
4. Consider whether "geometry" (planar) vs "geography" (ellipsoidal) should be a
   choice, as in PostGIS — currently it's always geography/metres.
