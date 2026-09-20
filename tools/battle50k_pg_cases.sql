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

-- ============================================================
-- AGGREGATE CASES (QL_CONTRACT §4.7; group-count agreement required
-- across arms)
-- =====================================================
--
-- A folded answer has no `key` column to return, so each of these produces
-- ONE TEXT COLUMN per group: the group key, then `|name=value` per
-- accumulator. The two E4 arms build the identical line in Rust
-- (`agg_line` in src/bin/battle50k.rs), so the three reports are diffable
-- group by group and not only by their group COUNT.
--
-- `avg` is compared as `floor(avg(born))::bigint`, a whole number two
-- engines can agree on; a printed float is not. E4's own avg is an f64
-- mean over exactly-representable integers, so its floor is the same
-- number.

-- case: agg_count_all kind: filter
-- params: none
-- One group over the whole table. E4 drives this from the external-key
-- mapping keyspace (CandidateDriver::Keys), which is the same choice
-- popsim's and q7_budget's `count_all` make.
SELECT '|n=' || count(*)::text FROM place;

-- case: agg_count_kind kind: filter
-- params: none
-- GROUP BY the indexed Text column. E4 STREAMS this: `kind` is the driving
-- scalar index, so the groups arrive contiguous and no row is read.
SELECT kind || '|n=' || count(*)::text FROM place GROUP BY kind ORDER BY kind;

-- case: agg_sum_born_by_kind kind: filter
-- params: none
-- Five accumulators over `born`, which is NOT the driving index, so E4
-- reads one row per candidate and EXPLAIN says so. HAVING is applied to
-- the finished groups, before paging.
SELECT kind || '|n=' || count(*)::text
    || '|s=' || sum(born)::text
    || '|lo=' || min(born)::text
    || '|hi=' || max(born)::text
    || '|mean=' || floor(avg(born))::bigint::text
FROM place
GROUP BY kind
HAVING count(*) > 100
ORDER BY kind;

-- case: agg_distinct_kind kind: filter
-- params: none
-- DISTINCT is a group with no accumulators, which is why the line is the
-- bare key.
SELECT DISTINCT kind FROM place ORDER BY kind;

-- case: agg_count_radius_by_kind kind: filter
-- params: $1 lon, $2 lat, $3 radius_metres  (radii[i])
-- The radius drives in E4, so the group key is not the driving walk's own
-- value and the shape is HASHED -- one accumulator set per distinct kind,
-- bounded by the `groups` budget.
SELECT kind || '|n=' || count(*)::text
FROM place
WHERE ST_DWithin(loc, ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography, $3, true)
GROUP BY kind
ORDER BY kind;

-- case: agg_born_decade kind: filter
-- params: none
-- The one grouping EXPRESSION: it is accepted because E4 can compute it
-- INDEX-SIDE from the Int posting, and truncating division by a positive
-- divisor is monotone in that index's own order, so the groups stay
-- contiguous and the shape stays streaming.
SELECT (born / 10000)::text || '|n=' || count(*)::text
FROM place
GROUP BY born / 10000
ORDER BY born / 10000;
=======
-- GRAPH CASES (row-count agreement required; the `related` table is
-- written by `battle50k postgres --graph`, see below)
-- ============================================================
--
-- The edge set. `battle50k <arm> --graph` writes the SAME edges into all
-- three arms: every row is linked to its three nearest other rows by `loc`,
-- with weight = 1 / (1 + metres/1000) and since = the SOURCE row's born.
--
--   CREATE TABLE related (
--       source      text NOT NULL,
--       destination text NOT NULL,
--       weight      real NOT NULL,
--       since       int  NOT NULL
--   );
--   CREATE INDEX related_source      ON related USING btree (source);
--   CREATE INDEX related_destination ON related USING btree (destination);
--
-- `related_destination` is there because E4's reverse mirror is
-- (GRAPH_CONTRACT 2.2): an edge is written in both directions, always, so
-- an incoming hop is a range read and not a scan. graph_1hop_weight_top10
-- is the case that walks it.
--
-- DEVIATION. E4 answers these with ONE bounded breadth-first traversal --
-- a posting range per hop, the far endpoint read out of the key, the
-- predicates decided as the frontier expands (GRAPH_CONTRACT 4.2, 4.3).
-- Postgres has no such atomic, so each statement below is the
-- recursive-free form a planner produces for a bounded two-hop pattern:
-- `related` joined to itself, UNIONed with the one-hop arm. The UNION is
-- doing what E4's ACYCLIC rule does for nothing -- a node found at one hop
-- is not returned again at two -- and `d <> $1` is the rule that a seed is
-- never its own answer. WITH RECURSIVE is deliberately not used: the
-- pattern's depth is a constant, and a recursive CTE would measure the
-- recursion machinery rather than the two range reads.
--
-- The equivalence is stated FOR DEPTH <= 2, which is every case in this
-- file. It does not extend to depth >= 3: E4 refuses to expand an
-- intermediate it has already seen (GRAPH_CONTRACT 4.1, ACYCLIC), while the
-- join form has no such rule and would follow it again.

-- case: graph_2hop kind: filter
-- params: $1 seed key  (the row nearest points[i], resolved once for all arms)
SELECT d FROM (
    SELECT r1.destination AS d FROM related r1 WHERE r1.source = $1
    UNION
    SELECT r2.destination FROM related r1
      JOIN related r2 ON r2.source = r1.destination
     WHERE r1.source = $1
) t WHERE d <> $1;

-- case: graph_2hop_weight kind: filter
-- params: $1 seed key
-- The weight predicate is PER HOP: an edge that fails it is not followed,
-- so a second hop beyond it does not exist. That is why the predicate is
-- repeated on r1 inside the two-join arm rather than applied once at the
-- end -- a post-filter would keep paths that E4's traversal never walks.
SELECT d FROM (
    SELECT r1.destination AS d FROM related r1
     WHERE r1.source = $1 AND r1.weight > 0.5
    UNION
    SELECT r2.destination FROM related r1
      JOIN related r2 ON r2.source = r1.destination AND r2.weight > 0.5
     WHERE r1.source = $1 AND r1.weight > 0.5
) t WHERE d <> $1;

-- case: graph_2hop_born kind: filter
-- params: $1 seed key, $2 born lower, $3 born upper  (born_range(i))
-- Same shape for the NODE predicate: a node outside the range is neither
-- returned nor expanded, so the intermediate row is joined and filtered
-- before the second hop is taken.
SELECT d FROM (
    SELECT b.key AS d FROM related r1
      JOIN place b ON b.key = r1.destination AND b.born BETWEEN $2 AND $3
     WHERE r1.source = $1
    UNION
    SELECT c.key FROM related r1
      JOIN place b ON b.key = r1.destination AND b.born BETWEEN $2 AND $3
      JOIN related r2 ON r2.source = b.key
      JOIN place c ON c.key = r2.destination AND c.born BETWEEN $2 AND $3
     WHERE r1.source = $1
) t WHERE d <> $1;

-- case: graph_1hop_weight_top10 kind: ranked
-- params: $1 seed key
-- One INCOMING hop: the rows that name the seed among their three nearest.
-- E4 reads the reaching edge's `weight` out of the posting it is standing
-- on and ranks by it (GRAPH_CONTRACT 4.2); Postgres reads the same column
-- out of the joined row. Compared on top-ten overlap, never on order:
-- E4 breaks a weight tie by entity id and this statement has no tiebreak,
-- for the same reason knn_10 has none.
SELECT r.source FROM related r
 WHERE r.destination = $1
 ORDER BY r.weight DESC
 LIMIT 10;

-- ─────────────────────────────────────────────────────────────────────────
-- BOOLEAN CASES (QL_CONTRACT §3). Each one is ONE membership set in E4: a
-- union of equalities, a union across two index families, a complement
-- inside one index, a union of two covers, the complement of the nullish
-- key, and a semi-join set. No new column and no new index: `place_kind`,
-- `place_born`, the GiST on `loc` and `related_source` already exist.

-- case: bool_kind_in3 kind: filter
-- params: $1, $2, $3 kinds  (kinds[i%8], kinds[(i+1)%8], kinds[(i+2)%8])
SELECT key FROM place
WHERE kind IN ($1, $2, $3);

-- case: bool_born_or_kind kind: filter
-- params: $1 lower born, $2 upper born, $3 kind
-- A union ACROSS two indexes, which Postgres answers with a BitmapOr of the
-- two index scans. E4 unions the two membership sets the same way.
SELECT key FROM place
WHERE (born BETWEEN $1 AND $2) OR kind = $3;

-- case: bool_not_kind kind: filter
-- params: $1 kind
-- The complement of an equality. E4 takes it inside `place_kind` as the two
-- ranges either side of the value, the nullish key in neither, which is why
-- a NULL `kind` would be in no answer here -- exactly as `NULL <> x` is
-- unknown in Postgres. No row of this corpus has one.
SELECT key FROM place
WHERE kind <> $1;

-- case: bool_radius_or_radius kind: filter
-- params: $1 lon, $2 lat, $3 metres, $4 lon, $5 lat, $6 metres
SELECT key FROM place
WHERE ST_DWithin(loc, ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography, $3)
   OR ST_DWithin(loc, ST_SetSRID(ST_MakePoint($4, $5), 4326)::geography, $6);

-- case: bool_not_null_born kind: filter
-- params: none
-- Every row has a `born`, so this is the whole table -- which is the point:
-- the complement of the nullish key is a posting range, not a scan, and the
-- two arms must still agree on 50,000 rows.
SELECT key FROM place
WHERE born IS NOT NULL;

-- case: bool_exists_related kind: filter
-- params: none
-- A semi-join, and the one case in this file whose subquery is a TABLE in
-- Postgres and an EDGE TYPE in E4: `related` is a table here and the
-- `related` edges there, so E4 builds the set from the edge keyspace and
-- Postgres from `related_source`. Selected only with `--graph`, like the
-- four `graph_` cases.
SELECT key FROM place
WHERE EXISTS (SELECT 1 FROM related r WHERE r.source = place.key);
