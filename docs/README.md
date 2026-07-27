# Sekejap Documentation

Sekejap is an embedded, disk-first, graph-first multi-model database with a
PostgreSQL-style query dialect (PostGIS spatial + pgvector vector operators) and
a Cypher `MATCH` pattern for graph traversal.

Documentation is split by audience:

- **[guide/](guide/README.md)** — using sekejap: the query language and public API.
- **[internals/](internals/README.md)** — how the engine works: architecture,
  durability, foundations, benchmarks (for contributors and deep integrators).

## Start here

- New to the query language? → [guide/graph-queries.md](guide/graph-queries.md)
- Changing engine internals? → [internals/architecture.md](internals/architecture.md)
