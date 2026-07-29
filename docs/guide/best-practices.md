# Sekejap Best Practices

Evidence-backed guidance, measured against a real Postgres deployment — a retail
analytics schema (catalog → store → customer → order → line_item → category):
~55k-node graph, ~12k line items, disk-first + mmap paged. Numbers below are real
measurements, not illustrative.

## The one rule that matters most

> **Match the access pattern to the query shape:**
> **aggregation → indexed *fact* collection · drill-down / point lookup → *graph* traversal.**

Sekejap beats Postgres on *every* query shape (2×–23× measured) **when you use the
right pattern.** The only way it loses is using the wrong one.

| Query shape | Right tool | Measured vs PG |
|---|---|---|
| point / low-fan-out lookup (`store → supplier → warehouse`) | **traversal** | 23× (0.009 ms vs 0.21 ms) |
| shallow aggregation (order counts) | **fact + btree** | 2× (0.62 ms vs 1.28 ms) |
| medium aggregation (per-item averages) | **fact + btree** | 3.5× (1.17 ms vs 4.12 ms) |
| deep join + aggregation (per-store category totals) | traversal or fact | 8–19× (1–2.6 ms vs 20 ms) |
| deep hierarchical rollup (category→department→division) | **fact, 2-level** | 5.6× (27 ms vs 148 ms) |

## Do

1. **Aggregate over a denormalized, indexed fact collection.**
   For any COUNT / AVG / SUM / rollup, build a flat `fact` node per row with the
   ancestry inlined (`store_id`, `customer_id`, `category_id`, `amount`, …) and
   **index the columns you filter on**:
   ```sql
   CREATE INDEX ON fact USING btree (store_id);
   SELECT customer_id, category_id, AVG(amount), COUNT(*)
   FROM fact WHERE store_id = ? GROUP BY customer_id, category_id;
   ```
   The btree turns a full scan into a scoped one — it is the difference between
   1.5 ms and 15 ms. **A fact without the index barely beats Postgres.**

2. **Traverse for drill-down and point lookups.**
   When you start from a known node and follow structure (this customer's orders,
   this store's suppliers), use `MATCH`. Index-free adjacency from a known node
   is microseconds — no index probe, just pointer follows:
   ```sql
   SELECT sup._key, w._key
   FROM MATCH (s:store)-[:supplied_by]->(sup:supplier)-[:stored_at]->(w:warehouse)
   WHERE s._key = ?;
   ```

3. **Model *observed* things as nodes, *plain relations* as light edges.**
   If you analyse the thing itself (count it, group it, attach many participants) →
   **node** (an act / event). If you only traverse through it → **light edge**
   (`part_of`, `belongs_to`, `located_in`). Richness lives on nodes; edges stay fast.

4. **Bulk-load with `put_many` + `link_many`, then `compact()`.**
   Individual `link()` fsyncs per edge (~ms each → minutes for tens of thousands).
   `link_many` defers to one fsync (66k edges: 92 s → 0.16 s). Always finish a bulk
   import with `compact()` — it truncates the WAL, writes the topology files, and
   makes reopen instant.

5. **Open large data with `open_paged()`.**
   Paged mode mmaps the topology (opens in ~4 ms regardless of size; the OS page
   cache holds the hot working set). Query speed holds vs in-memory.

## Don't

1. **Don't use traversal for an aggregation.**
   Traversing `store → order → sale` and grouping materialises every path — slower
   than a `fact` GROUP BY (report: traversal 1.97 ms vs fact 0.62 ms). This is the
   single most common mistake; it's the *only* thing that made Sekejap lose to
   Postgres in testing.

2. **Don't use a naive traversal for a many-to-many *hierarchical* rollup.**
   A "flat mean" traversal over a many-to-many hierarchy (category→department→
   division, where an item belongs to several categories) **double-counts** via
   fan-out and averages at the wrong level — it is both slower-looking *and wrong*.
   Use a fact 2-level aggregation (per-leaf average, then roll up).

3. **Don't put hot query data in the edge JSON bag.**
   Edge attributes you filter/group/aggregate on should be primitive values
   (numbers, booleans, strings) so they land in the fast-lane columns — not nested
   objects or arrays, which fall to the JSON bag and parse per edge (~2.3× slower).
   Keep the JSON bag for nested / variable / display-only data.

4. **Don't forget the index on fact filter columns.**
   An un-indexed fact GROUP BY full-scans every fact and barely beats Postgres.
   The index is what wins.

## Edge quick-reference

- **Create:** `INSERT ('a')-[:type {weight: 0.9, k: v}]->('b')` — primitive
  attributes ride the fast-lane columns; nested values go to the JSON bag (slower).
  No attribute name is privileged; use whatever names fit your data.
- **Delete:** `DELETE ('a')-[:type]->('b')`
- **List edge *types* (vocabulary + counts):** `SHOW EDGES [FROM col] [TO col]`
- **List edge *values* (instances):** `SELECT a._key, b._key, r.weight
  FROM MATCH (a:x)-[r:type]->(b:y)` — alias attributes to a distinct name (`r.energy AS e`).
- **Path-aggregate an attribute:** `PATH_PRODUCT(r.weight)` (confidence decay),
  `PATH_SUM(r.cost)` (accumulated cost) along a multi-hop route.

## Deployment shape

Sekejap doesn't have to replace Postgres uniformly — but it *can* serve the whole
workload faster when each query uses the right pattern. In practice: point the
**heavy analytical / graph endpoints** at Sekejap first (that's where Postgres hurts
most — 5–19×), and either keep shallow CRUD on Postgres or serve it from Sekejap too
(still 2–23× with the right pattern).
