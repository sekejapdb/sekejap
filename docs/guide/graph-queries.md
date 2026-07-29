# Graph Queries: `SELECT … FROM MATCH`

Sekejap's query language is a **PostgreSQL-style dialect** (PostGIS spatial + pgvector vector operators) with **one** borrowing from Cypher: the `MATCH` graph pattern, used inside `FROM`.

```
SELECT DISTINCT b._key AS k        -- PostgreSQL: projection, DISTINCT
FROM MATCH (a:users)-[r:visited]->(b:places)   -- Cypher: the graph pattern
WHERE a._key = 'u1' AND b.rating > 4           -- PostgreSQL: filters
  AND ST_DWithin(b.geometry, POINT(144.96 -37.81), 5.0)  -- PostGIS
ORDER BY b.rating DESC LIMIT 10                 -- PostgreSQL
```

Everything except the `MATCH (...)` pattern is ordinary SQL. There is **no** bare `MATCH … RETURN` form — graph queries are always `SELECT … FROM MATCH`.

---

## Projection

| Form | Meaning |
|---|---|
| `SELECT b.*` | the whole bound node (all fields spread to top level) |
| `SELECT b.field AS alias` | one field |
| `SELECT b._key AS k` | the node's key |
| `SELECT COUNT(*) AS n` | aggregate (see below) |

```sql
-- full nodes
SELECT b.* FROM MATCH (a:users)-[:wrote]->(b:posts) WHERE a._key = 'alice'

-- specific fields
SELECT b._key AS post, b.title AS title
FROM MATCH (a:users)-[:wrote]->(b:posts) WHERE a._key = 'alice'
```

## Direction: forward and backward

The arrow direction chooses how you traverse the (fixed) edge:

```sql
-- forward  -[:e]->  : follow the arrow  (what did alice write?)
SELECT b.* FROM MATCH (a:users)-[:wrote]->(b:posts) WHERE a._key = 'alice'

-- backward <-[:e]-  : follow against the arrow  (who wrote post1?)
SELECT a.* FROM MATCH (b:posts)<-[:wrote]-(a:users) WHERE b._key = 'post1'
```

Backward shines on hierarchies — flip the arrow to go from ancestors *down* to descendants:

```sql
-- every village under a province (walk down the child_of tree)
SELECT DISTINCT v._key FROM MATCH (p:province)<-[:child_of*1..3]-(v:village)
WHERE p._key = 'vic'
```

## Variable-length paths

`*min..max` traverses a range of hops:

```sql
SELECT root.* FROM MATCH (e:events)-[:caused_by*1..5]->(root)
WHERE e._key = 'maribyrnong-flood'
```

By default this returns **one row per path** — a node reachable by more than one route appears more than once (this is graph-standard, and required for aggregation). Use `DISTINCT` for unique nodes (see below).

## Filtering with `WHERE`

Filter on node fields, edge properties, spatial, and text:

```sql
SELECT b.* FROM MATCH (a:users)-[r:visited]->(b:places)
WHERE a._key = 'u1'                              -- node field
  AND b.rating >= 4                              -- destination field
  AND r.rating > 0.5                             -- edge attribute (see below)
  AND ST_DWithin(b.geometry, POINT(144.96 -37.81), 5.0)  -- PostGIS spatial
  AND BM25(b.description, 'coffee') > 0.0         -- text relevance
```

## Edge properties

Bind the edge with a variable (`-[r:type]->`) to read its attributes. An edge carries whatever attributes you gave it when you created it — there is no privileged field. Primitive attributes (numbers, booleans, strings) are stored in fast-lane columns and read back by name; other values ride a JSON bag. Any of them is available as `r.<name>`:

```sql
-- project and filter on edge fields (any attribute name works)
SELECT b._key AS charger, s.kwh AS energy, s.rate AS weight
FROM MATCH (v:vehicles)-[s:charged_at]->(b:chargers)
WHERE v._key = 'ev-7' AND s.kwh > 40
ORDER BY s.rate DESC
```

Two path intrinsics are also available on the edge variable: `r._depth` (hop count) and `r._path_keys` (the keys along the path).

## Grouping and aggregation

`GROUP BY` + `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`:

```sql
-- total kWh delivered per charger
SELECT b._key AS charger, SUM(s.kwh) AS total_kwh, AVG(s.kwh) AS avg_kwh
FROM MATCH (v:vehicles)-[s:charged_at]->(b:chargers)
GROUP BY b._key
ORDER BY total_kwh DESC
```

**Aggregate without `GROUP BY`** collapses to exactly **one row** (PostgreSQL semantics) — even when nothing matches (`COUNT` → 0, others → `NULL`):

```sql
SELECT COUNT(*) AS n, AVG(s.kwh) AS avg_kwh
FROM MATCH (v:vehicles)-[s:charged_at]->(b:chargers) WHERE v._key = 'ev-7'
```

## `DISTINCT` and `COUNT(DISTINCT …)`

By default a multi-hop match returns **path rows** (duplicates when a node is reachable multiple ways). `DISTINCT` de-duplicates to unique rows; `COUNT(DISTINCT field)` counts unique values:

```sql
-- unique reachable nodes (not paths)
SELECT DISTINCT x._key FROM MATCH (a:n)-[:e*1..2]->(x) WHERE a._key = 'a'

-- how many songs, and how many distinct genres among them
SELECT COUNT(*) AS songs, COUNT(DISTINCT s.genre) AS genres
FROM MATCH (u:users)-[:likes]->(s:songs) WHERE u._key = 'u1'
```

## Ordering and scoring

`ORDER BY` accepts a projected field or a **hybrid score expression**:

```sql
-- rank by a blend of text relevance and vector similarity
SELECT b._key FROM MATCH (u:users)-[:visited]->(b:places)
WHERE u._key = 'u1'
ORDER BY BM25_NORM(b.description, 'coffee') * 0.5
       + VECTOR_COSINE(b.embedding, [0.2, 0.7, 0.1]) * 0.5 DESC
LIMIT 10
```

Scoring operators mirror pgvector: `VECTOR_COSINE` (`<=>`), `VECTOR_L2` (`<->`), `VECTOR_DOT` (`<#>`), `VECTOR_L1` (`<+>`); `BM25` / `BM25_NORM` for text; `ST_DISTANCE_KM` for spatial.

## `UNION`

Combine two matches (de-duplicated):

```sql
SELECT g.* FROM MATCH (a:artist)-[:has_genre]->(g:genre) WHERE a._key = 'x'
UNION
SELECT c.* FROM MATCH (a:artist)-[:origin]->(c:city)    WHERE a._key = 'x'
```

## Multi-stage traversal with `WITH`

`WITH` carries a binding into a follow-on `MATCH` that continues from it:

```sql
SELECT d.name AS dish, COUNT(*) AS orders
FROM MATCH (c:tourists)-[:similar_taste]->(peer:tourists)
WHERE c._key = 'chloe'
WITH peer
MATCH (peer)-[:ate]->(d:dishes)
GROUP BY d.name
ORDER BY orders DESC
```

A chain of relationships can also be written as a single pattern, without a stage:

```sql
SELECT d.name AS dish, COUNT(*) AS orders
FROM MATCH (c:tourists)-[:similar_taste]->(peer:tourists)-[:ate]->(d:dishes)
WHERE c._key = 'chloe'
GROUP BY d.name
```

## Not supported

- **Bare `MATCH … RETURN`** — banned; always write `SELECT … FROM MATCH`.
- **Undirected `-[:e]-`** (either direction) — use forward or backward explicitly.
- A **second `WITH` stage that starts a *new* collection joined by a prior alias
  field** (`WITH q.owner AS owner … MATCH (o:owners WHERE _key = owner)…`) — under-binds;
  continue from a bound variable instead, or decompose into separate queries.
- **`BETWEEN`** inside a MATCH `WHERE` — use `x >= a AND x <= b`.
- **Functions in plain `SELECT` projection** (`AGE_DAYS`, `NOW`, `CASE`) — available in
  `SELECT … FROM MATCH`, not bare `SELECT col, f(col) FROM table`.
- **Variable-length edge metadata** — `r.field` reads on fixed single hops, not `*a..b`.

## Semantics cheat-sheet

| Query | Result |
|---|---|
| `SELECT x._key FROM MATCH (a)-[:e*1..2]->(x)` | one row **per path** (may repeat a node) |
| `SELECT DISTINCT x._key FROM MATCH …` | one row **per unique node** |
| `SELECT COUNT(*) … GROUP BY x._key` | path count per node |
| `SELECT COUNT(DISTINCT x._key) …` | number of unique nodes |
| `SELECT COUNT(*), AVG(v) FROM MATCH …` (no GROUP BY) | exactly **one row** (even if empty) |
