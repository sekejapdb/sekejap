# Examples — the core engine API (Rust)

Runnable, self-contained demos of sekejap's core concepts, using the `sekejap`
crate directly. These are general (language-agnostic in spirit) — for
language-specific usage see each wrapper's own examples under
[`wrappers/`](../wrappers/) (e.g. [`wrappers/c/examples/`](../wrappers/c/examples/)).

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
