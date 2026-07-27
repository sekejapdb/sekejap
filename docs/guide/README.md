# Sekejap User Guide

How to use sekejap — the query language and public API. For engine internals and
architecture, see [`../internals/`](../internals/README.md).

Sekejap is an embedded, disk-first, graph-first multi-model database. Its query
surface is a **PostgreSQL-style dialect** (PostGIS spatial + pgvector vector
operators), with the Cypher `MATCH` pattern available inside `FROM` for graph
traversal.

## Contents

- [Graph Queries — `SELECT … FROM MATCH`](graph-queries.md) — traversal,
  direction, variable-length paths, edge properties, `GROUP BY`/aggregates,
  `DISTINCT`, hybrid scoring, `UNION`, multi-stage `WITH`.
- [Best Practices](best-practices.md) — evidence-backed guidance on schema,
  indexing, and query shape, measured against a real relational deployment.

## The dialect at a glance

| Concern | Dialect | Examples |
|---|---|---|
| Projection / filter / aggregate / order / dedup | **PostgreSQL** | `SELECT DISTINCT`, `WHERE`, `GROUP BY`, `COUNT(DISTINCT …)`, `ORDER BY`, `LIMIT` |
| Spatial | **PostGIS** | `ST_DWithin`, `ST_Contains`, `POINT(...)` |
| Vector | **pgvector** | `VECTOR_COSINE`, `<=>`, `VECTOR_NEAR` |
| Graph pattern | **Cypher** | `MATCH (a)-[r:e*1..n]->(b)`, `<-…-` |

The graph pattern is the only non-SQL syntax; everything wrapping it is
PostgreSQL.
