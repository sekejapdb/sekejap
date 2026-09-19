-- battle50k_pg_cases.sql
--
-- One parameterised statement per battery case, in battery order, against
-- the schema in battle50k_pg_schema.sql. This file is the reference the
-- Rust PG driver must reproduce verbatim -- parameter order is documented
-- above each statement.
--
-- PostGIS semantics are picked to match the unit semantics E4 documents on
-- `GeometryFilter` and `PointFilter` in src/query.rs:
--   * PointFilter::Radius and GeometryFilter::{Intersects,DWithin} are
--     spheroidal (geodesic) -- run against the `geography` columns
--     directly, using ST_DWithin / ST_Intersects / the "<->" KNN operator.
--   * PointFilter::Bbox and GeometryFilter::{Within,Contains} are planar
--     (no geography overload exists for them in PostGIS) -- run against
--     `::geometry` casts, using ST_MakeEnvelope / ST_Within / ST_Contains.
--
-- Text matching: TextMatch::Any is modelled as an OR of a term's
-- whitespace-split words; TextMatch::All (and the two-term AND cases) as
-- an AND of them. Both build a `simple`-config tsquery to match the
-- `simple`-config GIN index in the schema file.

-- ============================================================
-- FILTER CASES (row-count agreement required across arms)
-- ============================================================

-- case: pt_radius kind: filter
-- params: $1 lon, $2 lat, $3 radius_metres  (radii[i])
SELECT key FROM place
WHERE ST_DWithin(loc, ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography, $3);

-- case: pt_bbox kind: filter
-- params: $1 minlon, $2 minlat, $3 maxlon, $4 maxlat
--   NOTE: queries.json stores boxes[i] as [minlon, maxlon, minlat, maxlat];
--   the Rust arm (Queries::bounds in src/bin/battle50k.rs) reorders before binding.
-- Planar containment (PointFilter::Bbox is a plain lon/lat rectangle test,
-- not a geodesic one), so this runs against the ::geometry cast.
SELECT key FROM place
WHERE ST_Within(loc::geometry, ST_MakeEnvelope($1, $2, $3, $4, 4326));

-- case: plot_within_box kind: filter
-- params: $1 minlon, $2 minlat, $3 maxlon, $4 maxlat
--   NOTE: queries.json stores boxes[i] as [minlon, maxlon, minlat, maxlat];
--   the Rust arm (Queries::bounds in src/bin/battle50k.rs) reorders before binding.
-- GeometryFilter::Within is planar.
SELECT key FROM place
WHERE ST_Within(plot::geometry, ST_MakeEnvelope($1, $2, $3, $4, 4326));

-- case: plot_contains_pt kind: filter
-- params: $1 lon, $2 lat  (points[i])
-- GeometryFilter::Contains is planar.
SELECT key FROM place
WHERE ST_Contains(plot::geometry, ST_SetSRID(ST_MakePoint($1, $2), 4326));

-- case: plot_intersects kind: filter
-- params: $1 polygon GeoJSON text  (polygons[i])
-- GeometryFilter::Intersects is spheroidal -- run on geography directly.
SELECT key FROM place
WHERE ST_Intersects(plot, ST_SetSRID(ST_GeomFromGeoJSON($1), 4326)::geography);

-- case: plot_dwithin_1km kind: filter
-- params: $1 lon, $2 lat  (points[i]); radius is fixed at 1000 metres.
-- GeometryFilter::DWithin is spheroidal.
SELECT key FROM place
WHERE ST_DWithin(plot, ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography, 1000);

-- case: plot_vs_poly_within kind: filter
-- params: $1 polygon GeoJSON text  (polygons[i])
-- Polygon-vs-polygon, still GeometryFilter::Within -- planar.
SELECT key FROM place
WHERE ST_Within(plot::geometry, ST_SetSRID(ST_GeomFromGeoJSON($1), 4326));

-- case: text_one kind: filter
-- params: $1 term  (terms[i]); TextMatch::Any -- OR of the term's words.
SELECT key FROM place
WHERE to_tsvector('simple', coalesce(name, '') || ' ' || coalesce(descr, ''))
      @@ to_tsquery('simple', regexp_replace(trim(both ' ' from $1), '\s+', ' | ', 'g'));

-- case: text_two kind: filter
-- params: $1 term_i, $2 term_(i+1)%50; TextMatch::All -- AND of both
-- terms' words.
SELECT key FROM place
WHERE to_tsvector('simple', coalesce(name, '') || ' ' || coalesce(descr, ''))
      @@ to_tsquery('simple',
            regexp_replace(trim(both ' ' from $1 || ' ' || $2), '\s+', ' & ', 'g'));

-- case: text_and_kind kind: filter
-- params: $1 term  (terms[i], Any), $2 kind  (kinds[i % 8])
SELECT key FROM place
WHERE to_tsvector('simple', coalesce(name, '') || ' ' || coalesce(descr, ''))
      @@ to_tsquery('simple', regexp_replace(trim(both ' ' from $1), '\s+', ' | ', 'g'))
  AND kind = $2;

-- case: born_range kind: filter
-- params: $1 lower born, $2 upper born  (19500101 .. 19500101 + 10000*(i%7))
SELECT key FROM place
WHERE born BETWEEN $1 AND $2;

-- case: kind_eq kind: filter
-- params: $1 kind  (kinds[i % 8])
SELECT key FROM place
WHERE kind = $1;

-- case: radius_and_born kind: filter
-- params: $1 lon, $2 lat, $3 radius_metres, $4 born_lower, $5 born_upper
SELECT key FROM place
WHERE ST_DWithin(loc, ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography, $3)
  AND born BETWEEN $4 AND $5;

-- ============================================================
-- RANKED CASES (top-k, k = 10; first_keys compared, overlap reported)
-- ============================================================

-- case: knn_10 kind: ranked
-- params: $1 lon, $2 lat  (points[i])
-- "<->" on a geography column is geodesic KNN, matching E4's
-- QueryOrder::Distance (ascending geodesic distance, ties by entity id).
SELECT key FROM place
ORDER BY loc <-> ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography, key
LIMIT 10;

-- case: knn_10_kind kind: ranked
-- params: $1 lon, $2 lat  (points[i]), $3 kind  (kinds[i % 8])
SELECT key FROM place
WHERE kind = $3
ORDER BY loc <-> ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography, key
LIMIT 10;

-- case: text_top10 kind: ranked
-- params: $1 term  (terms[i], Any-style tsquery, ranked by ts_rank_cd)
SELECT key,
       ts_rank_cd(
           to_tsvector('simple', coalesce(name, '') || ' ' || coalesce(descr, '')),
           to_tsquery('simple', regexp_replace(trim(both ' ' from $1), '\s+', ' | ', 'g'))
       ) AS score
FROM place
WHERE to_tsvector('simple', coalesce(name, '') || ' ' || coalesce(descr, ''))
      @@ to_tsquery('simple', regexp_replace(trim(both ' ' from $1), '\s+', ' | ', 'g'))
ORDER BY score DESC, key
LIMIT 10;

-- case: vec_exact_10 kind: ranked
-- params: $1 emb vector literal text, e.g. '[0.1,0.2,...]'  (vectors[i])
-- Exact cosine KNN: disable index/bitmap scans so the planner cannot use
-- place_emb_diskann (an approximate structure) and instead sequential-
-- scans + sorts exactly, inside a transaction so the SET LOCAL is scoped
-- to this statement only.
BEGIN;
SET LOCAL enable_indexscan = off;
SET LOCAL enable_bitmapscan = off;
SELECT key FROM place
ORDER BY emb <=> $1::vector, key
LIMIT 10;
COMMIT;

-- case: vec_exact_radius kind: ranked
-- params: $1 emb vector literal text (vectors[i]), $2 lon, $3 lat,
-- $4 radius_metres  (radii[i]) -- vec_exact_10 restricted to pt_radius.
BEGIN;
SET LOCAL enable_indexscan = off;
SET LOCAL enable_bitmapscan = off;
SELECT key FROM place
WHERE ST_DWithin(loc, ST_SetSRID(ST_MakePoint($2, $3), 4326)::geography, $4)
ORDER BY emb <=> $1::vector, key
LIMIT 10;
COMMIT;

-- case: hybrid_10 kind: ranked
-- params: $1 term  (terms[i], Any), $2 lon, $3 lat, $4 radius_metres
-- (radii[i]), $5 emb vector literal text  (vectors[i]).
-- text_one filter AND pt_radius filter, ranked by vector cosine distance
-- alone (no blending) -- E4 has no combined score today, so this is the
-- direct counterpart of E4's hybrid_10.
SELECT key FROM place
WHERE to_tsvector('simple', coalesce(name, '') || ' ' || coalesce(descr, ''))
      @@ to_tsquery('simple', regexp_replace(trim(both ' ' from $1), '\s+', ' | ', 'g'))
  AND ST_DWithin(loc, ST_SetSRID(ST_MakePoint($2, $3), 4326)::geography, $4)
ORDER BY emb <=> $5::vector, key
LIMIT 10;

-- case: hybrid_blend_10 kind: ranked
-- PG-ONLY case (deviation: E4 has no score expression -- its arm reports
-- this case with median_us null). Same params and same WHERE as hybrid_10;
-- ORDER BY differs.
-- params: $1 term, $2 lon, $3 lat, $4 radius_metres, $5 emb vector literal
SELECT key FROM place
WHERE to_tsvector('simple', coalesce(name, '') || ' ' || coalesce(descr, ''))
      @@ to_tsquery('simple', regexp_replace(trim(both ' ' from $1), '\s+', ' | ', 'g'))
  AND ST_DWithin(loc, ST_SetSRID(ST_MakePoint($2, $3), 4326)::geography, $4)
ORDER BY
    0.5 * ts_rank_cd(
              to_tsvector('simple', coalesce(name, '') || ' ' || coalesce(descr, '')),
              to_tsquery('simple', regexp_replace(trim(both ' ' from $1), '\s+', ' | ', 'g'))
          )
  + 0.5 * (1 - (emb <=> $5::vector)) DESC,
    key
LIMIT 10;

-- ============================================================
-- APPROX CASES (recall_at_k reported per arm against that arm's own
-- exact answer)
-- ============================================================

-- case: vec_ann_10 kind: approx
-- params: $1 emb vector literal text  (vectors[i])
-- Plain index scan -- the planner is free to use place_emb_diskann.
SELECT key FROM place
ORDER BY emb <=> $1::vector, key
LIMIT 10;

-- case: vec_ann_10_kind kind: approx
-- params: $1 emb vector literal text  (vectors[i]), $2 kind  (kinds[i % 8])
SELECT key FROM place
WHERE kind = $2
ORDER BY emb <=> $1::vector, key
LIMIT 10;
