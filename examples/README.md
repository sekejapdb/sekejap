# Examples — the core engine API (Rust)

Runnable, self-contained demos of sekejap's core concepts, using the `sekejap`
crate directly. The same five-stop tour also exists per language:

| language | tour |
|---|---|
| Python | [`wrappers/python/examples/tour.py`](../wrappers/python/examples/tour.py) |
| Node.js | [`wrappers/node/examples/tour.js`](../wrappers/node/examples/tour.js) |
| Go | [`wrappers/go/examples/tour/`](../wrappers/go/examples/tour/) |
| C | [`wrappers/c/examples/tour.c`](../wrappers/c/examples/tour.c) (`make tour`) |
| Dart / Flutter | [`wrappers/dart/example/`](../wrappers/dart/example/) (full app) |

Kotlin and Swift quickstarts live in the
[bindings pages](../docs/usage/bindings/README.md).

Run any of them with `cargo run --example <name>`:

| Example | Shows |
|---|---|
| [`sql_basics`](sql_basics.rs) | `CREATE` / `INSERT` / `SELECT` / `GROUP BY` |
| [`graph_match`](graph_match.rs) | edges + `SELECT ... FROM MATCH` traversal |
| [`vector_search`](vector_search.rs) | `VECTOR` column + HNSW + `VECTOR_NEAR` |
| [`spatial`](spatial.rs) | `GEO` column + spatial index + `ST_DWithin` |
| [`hybrid_ranking`](hybrid_ranking.rs) | combine BM25 text + vector similarity in one `ORDER BY` |

```bash
cargo run --example sql_basics
cargo run --example hybrid_ranking
```

Each uses an in-memory database (`CoreDB::new()`), so nothing is written to disk.
