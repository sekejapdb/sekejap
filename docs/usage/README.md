# Usage guide

How to use sekejap. New here? The [main README](../../README.md) is the
hands-on tour; these pages are the reference.

- [queries.md](queries.md) — the SQL lane: schema, writes, filters,
  aggregates, indexes, spatial/vector/text operators, hybrid ranking, views,
  transactions.
- [graph-queries.md](graph-queries.md) — the graph lane:
  `SELECT … FROM MATCH`, traversal, variable-length paths, shortest path,
  aggregation, multi-stage `WITH`.
- [connectivity.md](connectivity.md) — beyond embedded: the HTTP server and
  the Postgres wire adapter.
- [concurrency-and-snapshots.md](concurrency-and-snapshots.md) — sharing one
  database across threads: the reader/writer model, lock-free **snapshot reads**
  for high-traffic servers, read scale-out, and operational limits.
- [bindings/](bindings/README.md) — install + first query per language:
  Python, Rust, Node.js, Dart/Flutter, Kotlin/Java, Swift, Go, and C.
- [best-practices.md](best-practices.md) — evidence-backed guidance on
  schema, indexing, bulk loading, and query shape.

## The dialect at a glance

| concern | dialect | examples |
|---|---|---|
| projection / filter / aggregate / order | **PostgreSQL** | `SELECT DISTINCT`, `WHERE`, `GROUP BY`, `ORDER BY`, `LIMIT` |
| spatial | **PostGIS** (metres) | `ST_DWithin`, `ST_Contains`, `POINT(lon lat)` |
| vector | **pgvector** | `VECTOR_NEAR`, `<=>`, `VECTOR_COSINE` |
| text | search-engine style | `BM25`, `SEARCH`, `ILIKE` |
| graph pattern | **GQL / Cypher** | `MATCH (a:col)-[r:e*1..3]->(b:col)` |

The graph pattern inside `MATCH` is the only non-SQL syntax; everything
wrapping it is PostgreSQL.
