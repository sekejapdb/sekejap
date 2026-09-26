# GQL profile

A GQL body is a graph query written inside SQL's `GRAPH_TABLE (...)`. It
matches patterns of nodes and edges, carries the matches through stages, and
returns ordinary rows that the surrounding `SELECT` can filter, group, order
and page like any other table.

```text
SELECT <columns> FROM GRAPH_TABLE (<graph> <body>) [AS g] [WHERE ...] [ORDER BY ...] [LIMIT ...]
```

`<graph>` names a graph context; `base` is the default one. The body is
written in ISO GQL's statement style (`MATCH`, `LET`, `FILTER`, `FOR`,
`RETURN`, `NEXT`), with a few forms from the Google GQL reference and the
PostgreSQL, PostGIS and pgvector spellings for text, spatial and vector
search. [`GQL_FEATURES.md`](GQL_FEATURES.md) lists every construct with the
test that pins it, where each comes from, how the profile differs from the
standards, and every construct it refuses.

Every example below runs as written on the tourism fixture of
[`EXAMPLE_FIXTURE.md`](EXAMPLE_FIXTURE.md): Bali sites joined by `route`
edges (each with its travel `minutes`), troupes and their dancers, and
travellers with the sites they `visited` (each with its `stars`).

## Patterns

A node is `(variable IS label WHERE predicate)`; an edge is
`-[variable IS type WHERE predicate]->`, `<-[...]-` or `-[...]-` (either
direction). `:label` is the same as `IS label`, and `A|B` matches either
label. Every variable a pattern binds can be returned.

```sql
-- the sites one route away from Kuta beach, with the travel time
SELECT * FROM GRAPH_TABLE (base
  MATCH (s IS site WHERE s._key = 'kuta_beach')-[r:route]->(t IS site)
  RETURN t._key AS site, r.minutes AS minutes ORDER BY minutes);

-- the dancers of each troupe: an edge read against its direction
SELECT * FROM GRAPH_TABLE (base
  MATCH (t IS troupe)<-[:member_of]-(d IS dancer)
  RETURN t._key AS troupe, d._key AS dancer ORDER BY troupe, dancer);

-- two patterns joined on a shared variable: who performs at a site a traveller rated 5
SELECT * FROM GRAPH_TABLE (base
  MATCH (v IS traveller)-[x:visited WHERE x.stars = 5]->(s IS site), (d IS dancer)-[:performs_at]->(s)
  RETURN v._key AS traveller, s._key AS site, d._key AS dancer)
```

A predicate inside a node or an edge and the `MATCH`'s own `WHERE` give the
same answer; where a predicate is written only decides where it runs.

## Stages

`LET` names a value, `FILTER` keeps rows, `FOR` unrolls a list, and
`RETURN` ends a stage with optional `DISTINCT`, `GROUP BY`, `ORDER BY`,
`OFFSET` and `LIMIT`. `NEXT` starts a new stage over the rows the last one
returned; only the returned columns cross it.

```sql
-- a grouped return: each traveller's well-rated visits and their mean
SELECT * FROM GRAPH_TABLE (base
  MATCH (v IS traveller)-[x:visited]->(s IS site)
  LET stars = x.stars
  FILTER stars >= 4
  RETURN v._key AS traveller, COUNT(s) AS good, AVG(stars) AS mean GROUP BY v ORDER BY traveller);

-- a list seeds the pattern, one key at a time
SELECT * FROM GRAPH_TABLE (base
  FOR k IN ['kuta_beach', 'ubud_market']
  MATCH (s IS site WHERE s._key = k)-[:route]->(t IS site)
  RETURN k AS from_site, t._key AS to_site ORDER BY from_site, to_site);

-- two stages: the best-rated sites, then where one route takes you from them
SELECT * FROM GRAPH_TABLE (base
  MATCH (s IS site WHERE s.rating = 5)
  RETURN s AS s
  NEXT MATCH (s)-[:route]->(t IS site)
  RETURN s._key AS from_site, t._key AS to_site ORDER BY from_site, to_site)
```

## Paths

A quantifier repeats an edge or a parenthesised subpath: `{n,m}`, `{n,}`,
`*`, `+`, `?`. A path mode says what may repeat: `WALK` (anything, the
default), `TRAIL` (no edge twice), `ACYCLIC` (no node twice). A selector
keeps one path per endpoint pair: `ANY`, `ANY SHORTEST`, or `ANY CHEAPEST`
over an edge `COST`. `p = ...` names the path for the path functions.

```sql
-- every site within three routes of Kuta beach
SELECT * FROM GRAPH_TABLE (base
  MATCH (a IS site WHERE a._key = 'kuta_beach')-[:route]->{1,3}(b IS site)
  RETURN DISTINCT b._key AS site ORDER BY site);

-- the fewest routes from Kuta beach to Amed
SELECT * FROM GRAPH_TABLE (base
  MATCH p = ANY SHORTEST (a IS site WHERE a._key = 'kuta_beach')-[:route]->{1,8}(b IS site WHERE b._key = 'amed_reef')
  RETURN PATH_LENGTH(p) AS routes);

-- the quickest way there, in minutes
SELECT * FROM GRAPH_TABLE (base
  MATCH p = ANY CHEAPEST (a IS site WHERE a._key = 'kuta_beach')-[e IS route COST e.minutes]->{1,8}(b IS site WHERE b._key = 'amed_reef')
  LET minutes = SUM(e.minutes)
  RETURN PATH_LENGTH(p) AS routes, minutes)
```

`SUM(e.minutes)` over the path's edge variable is a horizontal aggregate: it
folds the edges of one path. The same aggregate in a grouped `RETURN` folds
rows.

## Optional matches, existence, subqueries and unions

```sql
-- every site, with the dancers performing there or NULL
SELECT * FROM GRAPH_TABLE (base
  MATCH (s IS site)
  OPTIONAL MATCH (s)<-[:performs_at]-(d IS dancer)
  RETURN s._key AS site, d._key AS dancer ORDER BY site, dancer);

-- the sites no traveller has visited
SELECT * FROM GRAPH_TABLE (base
  MATCH (s IS site)
  WHERE NOT EXISTS { MATCH (s)<-[:visited]-(v IS traveller) }
  RETURN s._key AS site ORDER BY site);

-- per troupe, a subquery that counts its dancers
SELECT * FROM GRAPH_TABLE (base
  MATCH (t IS troupe)
  CALL (t) { MATCH (t)<-[:member_of]-(d IS dancer) RETURN COUNT(*) AS dancers }
  RETURN t._key AS troupe, dancers ORDER BY troupe);

-- the best-rated sites and Kuta beach, once each
SELECT * FROM GRAPH_TABLE (base
  MATCH (s IS site WHERE s.rating = 5) RETURN s._key AS site
  UNION
  MATCH (s IS site WHERE s._key = 'kuta_beach') RETURN s._key AS site)
```

`CALL` names the variables its body may see (`CALL () { ... }` sees none),
and drops an input row its body gives no row for. `UNION` branches return
the same columns by name and order; `UNION ALL` keeps duplicates.

## Text, spatial and vector search

The search forms are spelled exactly as on the SQL surface, and read the
same indexes.

```sql
-- sites whose text mentions a sunset, within 20 km of Kuta, nearest in theme to a beach first
SELECT * FROM GRAPH_TABLE (base
  MATCH (s IS site WHERE to_tsvector('simple', s.about) @@ to_tsquery('simple', 'sunset')
                     AND ST_DWithin(s.loc, ST_MakePoint(115.17, -8.72)::geography, 20000))
  RETURN s._key AS site, s.emb <-> '[1,0,0]'::vector AS distance ORDER BY distance LIMIT 3);

-- a text match ranked by BM25
SELECT * FROM GRAPH_TABLE (base
  MATCH (s IS site WHERE to_tsvector('simple', s.about) @@ to_tsquery('simple', 'reef'))
  RETURN s._key AS site, bm25(s.about, 'reef') AS score ORDER BY score DESC);

-- distance in metres from Ubud market, one route away
SELECT * FROM GRAPH_TABLE (base
  MATCH (a IS site WHERE a._key = 'ubud_market')-[:route]->(b IS site)
  RETURN b._key AS site, ST_Distance(b.loc, ST_MakePoint(115.2626, -8.5069)::geography) AS metres ORDER BY metres)
```

* `<->` is the Euclidean distance, `<=>` the cosine distance (`1 - cos`),
  `<#>` the negative inner product, as pgvector defines them.
* Distances are metres under `::geography`; a form PostgreSQL would read in
  degrees is refused with the spelling to use.
* A text form needs a READY text index on the field: there is no text
  analyzer outside an index, so a row is never re-tokenized.

**How the indexes are used.** Every condition on the pattern's first node
that an index answers exactly (a text match, a radius, an envelope, a
comparison on a B-tree field) becomes part of ONE index read, which the
engine intersects. A condition written later, in a `FILTER` or in the outer
`WHERE`, joins that read when it is about the same node and nothing in
between chooses among rows or counts them (no `ORDER BY`, `LIMIT`,
`DISTINCT`, grouping, path selector, `CALL` or `UNION`, and never out of an
`EXISTS`, `CALL` or `UNION` body). A `LIMIT` over `ORDER BY` an `<->` or
`<#>` distance of that node reads its exact vector index in order and stops
early; this needs a `NOT NULL` vector column.

**Exact unless asked.** Inside a body, a vector order is exact. With
`SET LOCAL ef_search = n` in the same transaction, and an approximate
(vamana or quantized) index on the column, it becomes approximate with that
shortlist, and `EXPLAIN` says so. The setting is read when a statement runs,
so one prepared statement follows the transaction it runs in.

## Parameters

`$n` parameters may appear anywhere a value may, and each has one type for
the whole statement, decided by where it is used.

```sql
-- params: ["reef", 115.25, -8.72, 15000, "[0,1,0]"]
SELECT * FROM GRAPH_TABLE (base
  MATCH (s IS site WHERE to_tsvector('simple', s.about) @@ to_tsquery('simple', $1)
                     AND ST_DWithin(s.loc, ST_MakePoint($2, $3)::geography, $4))
  RETURN s._key AS site, s.emb <-> $5::vector AS distance ORDER BY distance)
```

## The outer SELECT

The body's `RETURN` is a relation. The `SELECT` around it reads its columns
by name, and may filter, group, order and page them.

```sql
-- routes out of each site, counted outside the body
SELECT g.from_site, COUNT(*) AS routes
FROM GRAPH_TABLE (base MATCH (a IS site)-[:route]->(b IS site) RETURN a._key AS from_site, b._key AS to_site) AS g
GROUP BY g.from_site
ORDER BY routes DESC, g.from_site
LIMIT 3
```

## Reading EXPLAIN

`EXPLAIN` runs the statement and prints its operators first to last, each
with how it reads and what it charges against the query's budget. A budget
that runs out stops the query with a named error after the rows already
handed out; the answer is never passed off as complete.

| operator | what it does | what can stop it |
| --- | --- | --- |
| `Seed x: key lookup` | starts from one key | `key_postings` |
| `Seed x: index ...` | starts from one index read, several intersected | `scalar_postings`, `text_postings`, `spatial_postings`, `vector_sidecars`, `candidates` |
| `Seed x: SCAN of` | reads every row of a label | `candidates` |
| `Seed x: the node already bound` | re-enters a bound node | `binding_rows` |
| `Expand` | follows one edge step | `graph_edges` |
| `PathSearch` | a quantified pattern, a path mode, a selector | `graph_edges`, `path_states`, `queue_entries`, `predecessor_arcs` |
| `Reach` | a quantified pattern answered by a node BFS, when the seven rules it prints allow | `graph_edges`, `graph_visited` |
| `Filter`, `Let`, `Project` | a predicate, a value, the returned columns | `primary_reads` for the properties read |
| `Unnest` | `FOR x IN list` | `list_bytes` |
| `Aggregate`, `Distinct`, `Sort`, `Page` | grouping, `DISTINCT`, `ORDER BY`, `OFFSET`/`LIMIT` | `groups`, `sort_bytes` |
| `OptionalApply`, `ExistsApply`, `CallApply` | `OPTIONAL MATCH`, `EXISTS`, `CALL`, with their steps numbered under them | what their steps charge |
| `Union` | the branches, then `Distinct` for `UNION` | what their steps charge |

```sql explain
SELECT * FROM GRAPH_TABLE (base MATCH (s IS site WHERE s._key = 'kuta_beach')-[r:route]->(t IS site) RETURN t._key AS k, r.minutes AS m ORDER BY m LIMIT 3)
-- expect: 1. Seed s: key lookup of 'kuta_beach' in site
-- expect: 2. Expand s -[r:route]-> t: outgoing edges
-- expect: 4. Sort by m
-- expect: 5. Page LIMIT 3
```

```sql explain
SELECT * FROM GRAPH_TABLE (base MATCH (s IS site WHERE to_tsvector('simple', s.about) @@ to_tsquery('simple', 'sunset') AND ST_DWithin(s.loc, ST_MakePoint(115.17, -8.72)::geography, 20000)) RETURN s._key AS k, s.emb <-> '[1,0,0]'::vector AS d ORDER BY d LIMIT 3)
-- expect: 1. Seed s: indexes on site, intersected: `site_about` (to_tsvector('simple', s.about) @@ to_tsquery('simple', 'sunset')) AND `site_loc`
-- expect: 3. Sort by d -- stable; holds sort_bytes (only offset + limit rows under a LIMIT); fed in the seed's index order
```

```sql explain
SELECT * FROM GRAPH_TABLE (base MATCH (s IS site) RETURN s AS s NEXT FILTER s.rating = 5 RETURN s._key AS k)
-- expect: 1. Seed s: index `site_rating` on site ((s.rating = 5) -- moved from FILTER by lineage)
-- expect: NEXT: stage 2 reads the rows stage 1 returned
```

```sql explain
SELECT * FROM GRAPH_TABLE (base MATCH (a IS site WHERE a._key = 'kuta_beach')-[:route]->{1,3}(b IS site) RETURN DISTINCT b._key AS k)
-- expect: 2. PathSearch from a: every path
-- expect: not Reach: rule 5 fails
-- expect: 4. Distinct
```

```sql explain
SELECT * FROM GRAPH_TABLE (base MATCH (t IS troupe) CALL (t) { MATCH (t)<-[:member_of]-(d IS dancer) RETURN COUNT(*) AS dancers } RETURN t._key AS t, dancers)
-- expect: 1. Seed t: SCAN of troupe
-- expect: 2. CallApply: per input row
-- expect: 2.2. Expand t <-[:member_of]- d: incoming edges
```

## Compatibility notes

These are the places where this profile answers differently from the graph
body sekejap had before it.

* **`COLUMNS` is gone.** A body ends in `RETURN`; there is no alias for the
  old SQL/PGQ `COLUMNS (...)` clause.

```sql refused
-- refused 0A000: the SQL/PGQ COLUMNS body is not adopted; a body ends in RETURN
SELECT * FROM GRAPH_TABLE (base MATCH (a IS site WHERE a._key = 'kuta_beach')-[:route]->(b IS site) COLUMNS (b._key AS k))
```

* **Paths are counted, not endpoints.** Two routes to one site are two
  rows; `RETURN DISTINCT` gives each site once.

```sql
-- Besakih is reached from Ubud market two ways within two routes, directly and
-- through Tegallalang: two rows
SELECT * FROM GRAPH_TABLE (base
  MATCH (a IS site WHERE a._key = 'ubud_market')-[:route]->{1,2}(b IS site WHERE b._key = 'besakih')
  RETURN b._key AS site)
```

* **A zero lower bound includes the start.** `{0,2}` returns the starting
  site itself as a zero-route match.

```sql
SELECT * FROM GRAPH_TABLE (base
  MATCH (a IS site WHERE a._key = 'besakih')-[:route]->{0,2}(b IS site)
  RETURN b._key AS site ORDER BY site)
```

* **A missing seed key is an empty answer**, not an error.

```sql
SELECT * FROM GRAPH_TABLE (base MATCH (a IS site WHERE a._key = 'no_such_site')-[:route]->(b) RETURN b._key AS site)
```

* **An unbounded quantifier is unbounded**, not capped at 16 steps. Under
  `WALK` it needs a selector or a path mode that ends on a finite graph.

```sql
SELECT * FROM GRAPH_TABLE (base
  MATCH ACYCLIC (a IS site WHERE a._key = 'kuta_beach')-[:route]->+(b IS site)
  RETURN DISTINCT b._key AS site ORDER BY site)
```

* **A later `FILTER` is not pushed through a selector.** It filters the
  paths the selector chose; it does not change which path is chosen.

```sql
SELECT * FROM GRAPH_TABLE (base
  MATCH p = ANY SHORTEST (a IS site WHERE a._key = 'kuta_beach')-[:route]->{1,8}(b IS site)
  FILTER PATH_LENGTH(p) >= 3
  RETURN b._key AS site ORDER BY site)
```

## What is refused

A construct the profile does not build is refused by name, with its reason
and, where it is planned, the milestone; the full list is in
[`GQL_FEATURES.md`](GQL_FEATURES.md#unsupported).

```sql refused
-- refused 0A000: label conjunction is a P1 construct
SELECT * FROM GRAPH_TABLE (base MATCH (a IS site & troupe) RETURN a._key AS k)
```

```sql refused
-- refused 0A000: a path accumulator is not adopted; it is a horizontal aggregate
SELECT * FROM GRAPH_TABLE (base MATCH (a IS site WHERE a._key = 'kuta_beach')-[e:route]->{1,3}(b IS site) RETURN PATH_SUM(e.minutes) AS m)
```

```sql refused
-- refused 0A000: search() inside a GQL body is a P1 construct
SELECT * FROM GRAPH_TABLE (base MATCH (s IS site) WHERE search(s.about, 'sunset') RETURN s._key AS k)
```
