# sekejap documentation

sekejap is an embedded, disk-first, graph-first multi-model database. One
engine, one file on disk, five retrieval models — records, graph, spatial,
vector, and text — queryable together in a single SQL statement.

Two doors:

- **[usage/](usage/README.md)** — you are *using* sekejap: the query language,
  network access, and language bindings.
- **[developer/](developer/README.md)** — you are *changing* sekejap: the
  architecture, storage formats, index designs, and the invariants that keep
  it fast.

## Start here

1. Hands-on tour of the five models → the [main README](../README.md)
2. The query language, properly → [usage/queries.md](usage/queries.md) and
   [usage/graph-queries.md](usage/graph-queries.md)
3. How the engine works → [developer/README.md](developer/README.md)
4. Measurements → [../eval/](../eval/README.md): the comparative benchmarks
   (relational, graph, spatial, vector, text, hybrid) with per-category
   results and the harnesses to reproduce them
