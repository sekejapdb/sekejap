# Queries — how a statement executes

One executor serves every surface. The atomic API builds a plan directly; SQL
text is lowered to the same plan; the network adapters feed the same entry
points. This page is the path from statement to rows.

## From text to plan

```
db.query("SELECT …")
   → tokenize → AST → lowering        (src/sql.rs, src/sql/)
   → a linear Vec<Step> plan          (src/query.rs)
   → the Set executor                 (src/query.rs)
```

`Step` is a small vocabulary: starters (`One`, `Many`, `Collection`, `All`),
graph moves (`Forward`, `Backward`, `Hops`, `Leaves`, `Roots`), filters
(`WhereEq`, `WhereBetween`, `Like`, spatial and vector steps, …), shaping
(`Sort`, `Skip`, `Take`, `Select`, `GroupBy`), and set algebra (`Intersect`,
`Union`, `Subtract`). The atomic API's chain
(`db.nodes().collection(x).where_eq(…).sort(…).take(n)`) maps 1:1 onto the
same steps — which is why measuring atomic vs SQL isolates pure parse cost
(~0.5 ms on an indexed point query).

`MATCH` statements lower to their own routed forms (pattern traversal,
aggregation, shortest path, multi-stage `WITH`), all executed by the same
engine machinery.

## Selection, then ranking

The composition model, and the reason multi-model works in one statement:

1. **Selection** — every predicate reduces to a candidate set of node ids:
   scalar filters via btree/hash, graph patterns via CSR traversal, spatial
   via the grid, text via BM25/trigram, vectors via HNSW. Boolean logic is set
   intersection/union/difference over those ids.
2. **Ranking** — score expressions (`BM25(…)`, `VECTOR_COSINE(…)`,
   `ST_Distance(…)`, scalar fields, arithmetic over all of them) evaluate only
   over surviving candidates, then sort/limit.

Payloads are fetched last, for winners only.

## Fast paths that keep this honest

The executor's job is mostly to *avoid* work:

- **Topology before payloads** — a traversal that only needs structure never
  parses a record.
- **Frontier-merged aggregation** — grouped multi-hop counts propagate one
  merged map per frontier instead of materializing a row per path (an
  89k-node, 3-hop grouped count answers in ~37 ms this way).
- **Head+tail extraction** — grouped queries over oversized payloads (>64 KB)
  read a small slice and extract the needed fields, falling back to the full
  record only if a filtered field is missing from the slice.
- **Index-order selection** — spatial-first vs collection-first, grid starter
  vs exact filter, chosen from the query shape (see the plan output of
  `EXPLAIN`-style step labels in `src/query.rs`).
- **Two-stage vector search** — traverse int8, rescore f32
  (see [indexes.md](indexes.md)).

The regression checklists for these paths are in
[invariants.md](invariants.md) — read them before adding a query feature.

## Hybrid scoring

Score expressions are ordinary arithmetic trees (`ScoreExpr`): any numeric
signal can be weighted and combined in `ORDER BY`. The engine supplies the
primitives — BM25 (raw and normalized), the four vector distances, geodesic
distance, field values, `CASE` — and imposes no fixed ranking model.
Applications tune weights per query; reciprocal-rank fusion and similar
schemes are a few lines over the same primitives.
