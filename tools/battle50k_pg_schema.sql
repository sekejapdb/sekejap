-- battle50k_pg_schema.sql
--
-- Idempotent DDL for the battle50k PostGIS/pgvector arm of the E4 vs. PG
-- battery. Run against database `e4_bench`; the `postgis`, `vector` and
-- `vectorscale` extensions are assumed already installed into it (a
-- superuser step, done once, outside this file).
--
-- Column-for-column mirror of the E4 `place` collection used by
-- bench/src/bin/popsim.rs: `loc` is a Kind::Point, `plot` is a Kind::Geo (stored
-- as a polygon here), `emb` is a Kind::Vector(32).
--
-- Every CREATE INDEX is preceded by a comment naming the stage exactly as
-- it must appear in the "stages" array of the shared JSON report contract
-- ("index:<name>"), so the harness that runs this file can time each
-- statement and log it under that name.

CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS vectorscale;

CREATE TABLE IF NOT EXISTS place (
    key   text PRIMARY KEY,
    name  text,
    descr text,
    born  int,
    kind  text,
    loc   geography(Point, 4326),
    plot  geography(Polygon, 4326),
    emb   vector(32)
);

-- index:place_fts_gin
-- Full-text search over name || ' ' || descr, tokenised with the 'simple'
-- (no-stemming) configuration -- the two-argument to_tsvector(regconfig,
-- text) form is IMMUTABLE, so it is safe as a functional GIN index.
CREATE INDEX IF NOT EXISTS place_fts_gin
    ON place
    USING gin (to_tsvector('simple', coalesce(name, '') || ' ' || coalesce(descr, '')));

-- index:place_born_btree
CREATE INDEX IF NOT EXISTS place_born_btree
    ON place (born);

-- index:place_kind_btree
CREATE INDEX IF NOT EXISTS place_kind_btree
    ON place (kind);

-- index:place_loc_gist
-- GiST over geography(Point) -- supports ST_DWithin/ST_Intersects and the
-- KNN "<->" operator (geodesic distance), matching E4's point index, which
-- is walked in ascending geodesic distance for QueryOrder::Distance.
CREATE INDEX IF NOT EXISTS place_loc_gist
    ON place
    USING gist (loc);

-- index:place_plot_gist
-- GiST over geography(Polygon) -- supports ST_Intersects/ST_DWithin
-- (spheroidal) directly; ST_Within/ST_Contains are run against the
-- ::geometry cast (planar), same split E4's GeometryFilter documents.
CREATE INDEX IF NOT EXISTS place_plot_gist
    ON place
    USING gist (plot);

-- index:place_emb_diskann
-- DiskANN (pgvectorscale) approximate index over cosine distance, used by
-- the approx cases (vec_ann_10, vec_ann_10_kind) and by the ranked cases
-- that do not require exactness (knn-by-vector inside hybrid_10 /
-- hybrid_blend_10). Exact-vector cases (vec_exact_10, vec_exact_radius)
-- disable index/bitmap scans in-session so the planner falls back to a
-- sequential scan instead of using this index -- see
-- battle50k_pg_cases.sql.
CREATE INDEX IF NOT EXISTS place_emb_diskann
    ON place
    USING diskann (emb vector_cosine_ops);

-- Alternative ANN index (not used by the battery as written; swap in by
-- dropping place_emb_diskann and uncommenting this one if a run wants the
-- pgvector HNSW arm instead of pgvectorscale DiskANN).
-- index:place_emb_hnsw
-- CREATE INDEX IF NOT EXISTS place_emb_hnsw
--     ON place
--     USING hnsw (emb vector_cosine_ops)
--     WITH (m = 16, ef_construction = 64);
