#!/usr/bin/env bash
# Generates tests/fixtures/spatial_postgis.json ONCE from a live PostGIS 16/3.4
# server. Run manually when the fixture needs regenerating; the Rust test
# (tests/spatial_geometry_postgis.rs) reads the committed JSON offline.
set -euo pipefail
cd "$(dirname "$0")/.."

: "${PGHOST:=127.0.0.1}"
: "${PGPORT:=55432}"
: "${PGUSER:=postgres}"
: "${PGPASSWORD:=e4}"
: "${PGDATABASE:=postgres}"
export PGHOST PGPORT PGUSER PGPASSWORD PGDATABASE

OUT=tests/fixtures/spatial_postgis.json
mkdir -p "$(dirname "$OUT")"

psql -v ON_ERROR_STOP=1 -q -tA <<'SQL' > "$OUT"
-- ~40 geometries: points across latitudes (incl. near-poles, dateline),
-- short/long linestrings, small/large/concave polygons, a polygon with a
-- hole, multipolygons, and the popsim region (107.6, -6.9).
CREATE TEMP TABLE geoms (id text PRIMARY KEY, geog geography);
INSERT INTO geoms (id, geog) VALUES
  ('p_equator',        'SRID=4326;POINT(0 0)'),
  ('p_ny',              'SRID=4326;POINT(-74.0060 40.7128)'),
  ('p_popsim',          'SRID=4326;POINT(107.6 -6.9)'),
  ('p_popsim2',         'SRID=4326;POINT(107.62 -6.92)'),
  ('p_near_np',         'SRID=4326;POINT(10 89.9)'),
  ('p_near_sp',         'SRID=4326;POINT(10 -89.9)'),
  ('p_dateline_e',      'SRID=4326;POINT(179.9 10)'),
  ('p_dateline_w',      'SRID=4326;POINT(-179.9 10)'),
  ('p_high_lat',        'SRID=4326;POINT(170 80)'),
  ('p_antipode',        'SRID=4326;POINT(180 0)'),
  ('p_south_high',      'SRID=4326;POINT(-30 -85)'),
  ('p_on_poly_vertex',  'SRID=4326;POINT(-74.00 40.70)'),
  ('p_on_poly_edge',    'SRID=4326;POINT(-73.995 40.70)'),
  ('p_inside_hole',     'SRID=4326;POINT(107.5 -5.5)'),
  ('p_in_ring_not_hole','SRID=4326;POINT(106.2 -6.8)'),
  ('ln_short',          'SRID=4326;LINESTRING(107.60 -6.90, 107.6009 -6.9009)'),
  ('ln_long',           'SRID=4326;LINESTRING(-74.00 40.70, -73.95 40.75, -73.90 40.80)'),
  ('ln_dateline',       'SRID=4326;LINESTRING(179.5 10, -179.5 10)'),
  ('ln_cross_poly',     'SRID=4326;LINESTRING(-74.005 40.705, -73.985 40.705)'),
  ('ln_multi',          'SRID=4326;MULTILINESTRING((107.6 -6.9,107.61 -6.9),(107.6 -6.91,107.61 -6.91))'),
  ('ln_meridian',       'SRID=4326;LINESTRING(0 -10,0 10)'),
  ('ln_within_large',   'SRID=4326;LINESTRING(107.0 -7.0,107.3 -6.7)'),
  ('ln_pole_cross',     'SRID=4326;LINESTRING(0 89,10 89.5)'),
  ('poly_nyc_small',    'SRID=4326;POLYGON((-74.00 40.70,-73.99 40.70,-73.99 40.71,-74.00 40.71,-74.00 40.70))'),
  ('poly_popsim_large', 'SRID=4326;POLYGON((106.5 -7.5,108.5 -7.5,108.5 -5.5,106.5 -5.5,106.5 -7.5))'),
  ('poly_concave',      'SRID=4326;POLYGON((10 0,14 0,14 3,12 1.5,10 3,10 0))'),
  ('poly_with_hole',    'SRID=4326;POLYGON((106.0 -7.0,109.0 -7.0,109.0 -4.0,106.0 -4.0,106.0 -7.0),(107.0 -6.0,108.0 -6.0,108.0 -5.0,107.0 -5.0,107.0 -6.0))'),
  ('poly_near_pole',    'SRID=4326;POLYGON((0 85,10 85,10 89,0 89,0 85))'),
  ('poly_dateline',     'SRID=4326;POLYGON((179 -1,-179 -1,-179 1,179 1,179 -1))'),
  ('poly_small_triangle','SRID=4326;POLYGON((107.6 -6.9,107.61 -6.9,107.605 -6.89,107.6 -6.9))'),
  ('poly_inside_large', 'SRID=4326;POLYGON((107.0 -7.0,107.5 -7.0,107.5 -6.5,107.0 -6.5,107.0 -7.0))'),
  ('poly_overlap_large','SRID=4326;POLYGON((108.0 -6.0,109.5 -6.0,109.5 -4.5,108.0 -4.5,108.0 -6.0))'),
  ('poly_touch_nyc',    'SRID=4326;POLYGON((-73.99 40.70,-73.98 40.70,-73.98 40.71,-73.99 40.71,-73.99 40.70))'),
  ('mp_cluster',        'SRID=4326;MULTIPOINT((107.6 -6.9),(107.62 -6.91),(107.58 -6.89))'),
  ('mp_two',            'SRID=4326;MULTIPOINT((0 0),(1 1))'),
  ('mpoly_two_boxes',   'SRID=4326;MULTIPOLYGON(((0 0,1 0,1 1,0 1,0 0)),((2 0,3 0,3 1,2 1,2 0)))'),
  ('mpoly_popsim',      'SRID=4326;MULTIPOLYGON(((106.5 -7.5,107.5 -7.5,107.5 -6.5,106.5 -6.5,106.5 -7.5)),((108.0 -6.0,109.0 -6.0,109.0 -5.0,108.0 -5.0,108.0 -6.0)))'),
  ('mpoly_with_hole',   'SRID=4326;MULTIPOLYGON(((0 0,4 0,4 4,0 4,0 0),(1 1,2 1,2 2,1 2,1 1)),((6 0,8 0,8 2,6 2,6 0)))');

-- Relevant pairs: point/point, point/polygon (incl. vertex, edge, hole),
-- polygon/polygon (within/contains/covers/overlap/adjacent), line/polygon
-- (crosses, contains), multipolygon/polygon, near-pole and dateline pairs.
CREATE TEMP TABLE pairs (a text, b text);
INSERT INTO pairs (a, b) VALUES
  ('p_equator','p_ny'),
  ('p_popsim','p_popsim2'),
  ('p_near_np','p_near_sp'),
  ('p_dateline_e','p_dateline_w'),
  ('p_equator','p_antipode'),
  ('p_high_lat','p_south_high'),
  ('p_popsim','poly_popsim_large'),
  ('p_in_ring_not_hole','poly_with_hole'),
  ('p_inside_hole','poly_with_hole'),
  ('p_on_poly_vertex','poly_nyc_small'),
  ('p_on_poly_edge','poly_nyc_small'),
  ('p_equator','poly_concave'),
  ('p_dateline_e','poly_dateline'),
  ('p_equator','poly_dateline'),
  ('poly_inside_large','poly_popsim_large'),
  ('poly_overlap_large','poly_popsim_large'),
  ('poly_nyc_small','poly_touch_nyc'),
  ('poly_popsim_large','poly_with_hole'),
  ('poly_nyc_small','poly_popsim_large'),
  ('ln_cross_poly','poly_nyc_small'),
  ('ln_within_large','poly_popsim_large'),
  ('ln_short','poly_popsim_large'),
  ('ln_long','poly_nyc_small'),
  ('ln_pole_cross','poly_near_pole'),
  ('mpoly_popsim','poly_popsim_large'),
  ('mpoly_two_boxes','poly_inside_large'),
  ('mp_cluster','poly_popsim_large'),
  ('mp_two','mpoly_two_boxes'),
  ('poly_concave','poly_popsim_large'),
  ('ln_dateline','p_dateline_e'),
  ('ln_multi','poly_popsim_large'),
  ('mpoly_with_hole','p_equator'),
  ('poly_near_pole','p_near_np'),
  ('poly_dateline','poly_popsim_large'),
  ('p_popsim','mpoly_popsim'),
  ('poly_with_hole','poly_touch_nyc'),
  -- reversed direction of the point/polygon pairs above, so `contains`/`covers`
  -- (asymmetric: big-covers-small) get real True cases, not just `within`.
  ('poly_popsim_large','p_popsim'),
  ('poly_nyc_small','p_on_poly_vertex'),
  ('poly_nyc_small','p_on_poly_edge'),
  ('poly_with_hole','p_in_ring_not_hole'),
  ('poly_with_hole','p_inside_hole'),
  ('poly_popsim_large','ln_short'),
  ('poly_popsim_large','mp_cluster'),
  ('poly_popsim_large','poly_inside_large'),
  ('mpoly_two_boxes','p_equator');

SELECT jsonb_build_object(
  'postgis_version', (SELECT PostGIS_Full_Version()),
  'generated_at', now()::text,
  'singles', (
    SELECT jsonb_agg(jsonb_build_object(
      'id', id,
      'wkt', ST_AsText(geog::geometry),
      'geojson', ST_AsGeoJSON(geog::geometry)::jsonb,
      'geom_type', GeometryType(geog::geometry),
      'area_m2', ST_Area(geog),
      'length_m', ST_Length(geog),
      'perimeter_m', ST_Perimeter(geog),
      'centroid_lon', ST_X(ST_Centroid(geog)::geometry),
      'centroid_lat', ST_Y(ST_Centroid(geog)::geometry)
    ) ORDER BY id)
    FROM geoms
  ),
  'pairs', (
    SELECT jsonb_agg(jsonb_build_object(
      'a', p.a, 'b', p.b,
      'distance_m', ST_Distance(ga.geog, gb.geog),
      'centroid_distance_m', ST_Distance(ST_Centroid(ga.geog)::geography, ST_Centroid(gb.geog)::geography),
      'within', ST_Within(ga.geog::geometry, gb.geog::geometry),
      'contains', ST_Contains(ga.geog::geometry, gb.geog::geometry),
      'covers', ST_Covers(ga.geog::geometry, gb.geog::geometry),
      'crosses', ST_Crosses(ga.geog::geometry, gb.geog::geometry),
      'intersects_geography', ST_Intersects(ga.geog, gb.geog),
      'intersects_geometry', ST_Intersects(ga.geog::geometry, gb.geog::geometry),
      'dwithin_m', jsonb_build_object(
        '1', ST_DWithin(ga.geog, gb.geog, 1),
        '100', ST_DWithin(ga.geog, gb.geog, 100),
        '1000', ST_DWithin(ga.geog, gb.geog, 1000),
        '50000', ST_DWithin(ga.geog, gb.geog, 50000),
        '500000', ST_DWithin(ga.geog, gb.geog, 500000)
      )
    ) ORDER BY p.a, p.b)
    FROM pairs p
    JOIN geoms ga ON ga.id = p.a
    JOIN geoms gb ON gb.id = p.b
  )
);
SQL

python3 -c "import json,sys; json.load(open('$OUT'))" \
  && echo "OK: $OUT is valid JSON ($(wc -l < "$OUT") lines)"
