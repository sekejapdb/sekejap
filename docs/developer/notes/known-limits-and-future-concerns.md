# Known limits & future concerns

Deliberate simplifications where sekejap does less than a full server DB. Captured
for later — none are urgent, but each is a place a downstream app (Zebflow) could
hit an edge, and a place we may invest later.

---

## 1. S3 backend is publish/checkpoint-write, not live-write

**Today:** `CoreDB::open_s3()` is **read-only**. Object storage can't do the random
appends/`pwrite`s a live WAL + `payloads.bin` need, so per-request writes directly
to S3 are not possible.

**How writes reach S3 now:** the write path runs on a normal writable *local* disk
(`CoreDB::open`), and `engine/remote.rs` `RemoteSync::upload()` publishes the
segment files (snapshot, payloads, …) to S3 as objects + a manifest, at
**checkpoint granularity** (on compact / interval). Read replicas `open_s3` and
serve read-only from those published segments.

Model: **one local writer → publish snapshots to S3 → many read-only replicas.**

**Future concern:** a write-capable S3 mode would need a *local WAL/payload buffer*
that flushes up to S3 on an interval — durable locally, eventually-durable on S3.
That's a design, not something that exists. It pairs naturally with the
snapshot-reads read-scale-out story (see `snapshot-reads-design.md`).

---

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
