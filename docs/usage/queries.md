# Queries — the SQL lane

Everything outside `MATCH` is PostgreSQL-style SQL. This page covers that
surface: schema, writes, reads, indexes, retrieval operators, views, and
transactions. Graph patterns have their own page:
[graph-queries.md](graph-queries.md).

All examples run through `db.query(...)` / `db.execute(...)` in any binding,
the CLI (`sekejap <db-path> "<SQL>"`), or the network adapters.

## Schema

```sql
CREATE TABLE places (
  _key     TEXT PRIMARY KEY,          -- omit it and a UUIDv4 key is generated
  name     TEXT NOT NULL,
  category TEXT,
  rating   REAL,
  geometry GEO,                       -- GeoJSON value
  embedding VECTOR                    -- float embedding
);
```

- `_key` is the record's stable identity. If absent from `CREATE TABLE`, it is
  auto-injected as `TEXT DEFAULT UUIDV4() PRIMARY KEY`.
- Column defaults: `DEFAULT <literal>`, `DEFAULT UUIDV4()`,
  `DEFAULT UUIDV5('<namespace>', '<name>')`.
- Generated columns: `full_text TEXT GENERATED ALWAYS AS
  (concat_ws(' ', name, category)) STORED` — computed on insert/update,
  indexable like any field.
- Evolve with `ALTER TABLE`: `ADD COLUMN`, `DROP COLUMN [IF EXISTS]`,
  `RENAME COLUMN old TO new`, `RENAME TO new_table`, `ALTER COLUMN x TYPE t`.
- Inspect with `SHOW TABLES`, `SHOW TABLE places`, `SHOW CREATE places`,
  `SHOW EDGES`.

## Writing data

```sql
INSERT INTO places (_key, name, category, rating)
VALUES ('uluwatu', 'Uluwatu Temple', 'temple', 4.7);

UPDATE places SET rating = 4.8 WHERE _key = 'uluwatu';
DELETE FROM places WHERE rating < 2.0;
```

Relationships are edges, not foreign keys. Write them in SQL or through the
API (`link` / `link_many`):

```sql
INSERT ('tourists/chloe')-[:flew_on]->('flights/qf-mel');
INSERT ('city/melbourne')-[:contains {distance: 3.2}]->('suburb/fitzroy');
```

Querying them is the `MATCH` lane — see [graph-queries.md](graph-queries.md).

For bulk loading, prefer the API's batch paths (`ingest`, `put_many`,
`link_many`, or `begin_bulk()`/`end_bulk()`): they defer the per-write disk
sync to one per batch, which turns minutes into seconds at scale.

## Reading data

The usual clauses all work and compose:

```sql
SELECT name, rating FROM places
WHERE category = 'temple' AND rating >= 4.5
ORDER BY rating DESC
LIMIT 10;

SELECT category, COUNT(*) AS n, AVG(rating) AS avg_rating
FROM places
GROUP BY category
ORDER BY n DESC;
```

Filters: `=`, `!=`/`<>`, `<`, `<=`, `>`, `>=`, `BETWEEN`, `IN (...)`,
`IS [NOT] NULL`, `LIKE` / `ILIKE` (case-insensitive), `AND`/`OR`/`NOT`.

### NULL and missing fields

A comparison against a value that is not there answers neither true nor false.
SQL calls that *unknown*, and only rows where the condition comes out **true**
are returned. sekejap follows PostgreSQL here, and a field that is absent from a
record counts as NULL — the row has nothing to say about that column.

```sql
SELECT _key FROM p WHERE status != 'open';
```

This returns rows whose status is some other value. It does **not** return rows
where `status` is NULL or missing: those are unknown, not different. The same
holds for `NOT IN`, for `NOT (...)` around any comparison, and for `>`, `<`,
`BETWEEN` and `LIKE`.

To include them, ask for them:

```sql
SELECT _key FROM p WHERE status != 'open' OR status IS NULL;
```

Two consequences worth knowing, both inherited from SQL:

- `x = NULL` and `x != NULL` return no rows whatever the data holds. `IS NULL`
  and `IS NOT NULL` are how that question is asked.
- `x NOT IN ('a', NULL)` returns no rows, because comparing against the NULL in
  the list is unknown for every row. This surprises people in PostgreSQL too.

Scalar functions in projections and filters: `NOW()`, `AGE_DAYS(ts)`,
`AGE_HOURS(ts)`, `LOWER`/`UPPER`, `CONCAT`/`CONCAT_WS`, `COALESCE`,
`JSON_ARRAY_LENGTH`, and `CASE WHEN … THEN … ELSE … END`.

Parameters use PostgreSQL placeholders — always prefer them over string
interpolation:

```python
db.query_params("SELECT * FROM places WHERE category = $1", ["temple"])
```

Repeated queries can be prepared once (`db.prepare(...)` /
`db.query_prepared(...)`) to skip re-parsing.

## Indexes

Declare the access paths your queries need; the engine picks them up
automatically:

```sql
CREATE INDEX ON places USING btree   (rating);     -- equality/range/sort
CREATE INDEX ON places USING hash    (category);   -- equality only
CREATE INDEX ON places USING gin     (name);       -- fast ILIKE '%…%' (trigram)
CREATE INDEX ON places USING bm25    (description);-- ranked text relevance
CREATE INDEX ON places USING search  (description);-- positional/phrase search
CREATE INDEX ON places USING hnsw    (embedding);  -- approximate nearest neighbor
CREATE INDEX ON places USING spatial (geometry);   -- location predicates
```

`DROP INDEX [IF EXISTS] …` removes one. Indexes are sidecars: they can always
be rebuilt from the data, and queries still work (slower) without them.

## Spatial

PostGIS `geography` semantics: distances in **metres**, areas in **m²**,
measured on the WGS84 ellipsoid. Coordinates are GeoJSON `[lon, lat]`; the
`POINT(lon lat)` literal follows PostGIS order.

```sql
-- Everything within 5 km of a point
SELECT name FROM places
WHERE ST_DWithin(geometry, POINT(115.168 -8.690), 5000.0);

-- Point-in-polygon, polygon relationships, measures
SELECT name FROM places WHERE ST_Contains(geometry, POINT(115.26 -8.51));
SELECT name FROM regions WHERE ST_Intersects(geometry, ST_GeomFromGeoJSON('…'));
SELECT _key, ST_Area(geometry) AS m2, ST_Perimeter(geometry) AS m FROM regions;
```

Available: `ST_DWithin`, `ST_Distance`, `ST_Contains`, `ST_Within`,
`ST_Intersects`, `ST_Area`, `ST_Perimeter`, `ST_Length`, `ST_Centroid`,
`ST_AsGeoJSON`, `ST_GeomFromGeoJSON`.

## Vector search

`VECTOR_NEAR(field, [query…], k)` retrieves the k approximate nearest
neighbors via the HNSW index. Distance operators and functions follow pgvector:

```sql
SELECT _key FROM docs
WHERE VECTOR_NEAR(embedding, [0.12, 0.98, …], 10);

-- explicit metric, nearest-first
SELECT _key FROM docs ORDER BY embedding <-> [0.12, 0.98, …] ASC LIMIT 10;
```

| operator | function | metric |
|---|---|---|
| `<->` | `VECTOR_L2(f, [v])` | Euclidean |
| `<=>` | `VECTOR_COSINE(f, [v])` | cosine |
| `<#>` | `VECTOR_DOT(f, [v])` | inner product (not negated) |
| `<+>` | `VECTOR_L1(f, [v])` | Manhattan |

## Text search

```sql
-- ranked relevance (BM25 index)
SELECT _key FROM docs
WHERE BM25(body, 'emergency flood') > 0
ORDER BY BM25(body, 'emergency flood') DESC LIMIT 20;

-- automatic multi-field search (search index), with a score
SELECT _key, SEARCH_SCORE() AS s FROM docs
WHERE SEARCH('grilled chicken') ORDER BY s DESC;

-- substring match, accelerated by the gin (trigram) index
SELECT _key FROM docs WHERE body ILIKE '%maribyrnong%';
```

`BM25_NORM(...)` is the [0,1]-normalized variant, useful for weighted blends.

## Hybrid ranking

Every retrieval signal is an ordinary numeric term, so one `ORDER BY` can
blend them with plain arithmetic:

```sql
SELECT _key FROM docs
WHERE VECTOR_NEAR(embedding, [q…], 100)
ORDER BY BM25(body, 'emergency flood') * 0.4
       + VECTOR_COSINE(embedding, [q…]) * 0.6 DESC
LIMIT 10;
```

The same works with `ST_Distance(...)` (subtract it — closer is better) and
scalar fields. Selection (`WHERE`) narrows candidates; ranking orders them.

## Views

```sql
CREATE MATERIALIZED VIEW place_search
WITH (autoindex = true) AS
SELECT p._key AS id,
       concat_ws(' ', p.name, d.name) AS text,
       p.geometry AS geometry,
       p.embedding AS embedding
FROM MATCH (p:places)-[:serves]->(d:dishes);

REFRESH MATERIALIZED VIEW place_search;   -- explicit rebuild
```

A materialized view is a real collection: query and index it like any other.
`WITH (autoindex = true)` builds the right index for each search-typed field
(text → bm25, vector → hnsw, geometry → spatial). Freshness is explicit —
rebuild with `REFRESH` when your write policy requires it.

## Transactions

```sql
BEGIN;
INSERT INTO places (_key, name) VALUES ('a', 'A');
UPDATE places SET rating = 5 WHERE _key = 'a';
COMMIT;   -- or ROLLBACK;
```

A transaction batches its writes and syncs once at commit.
