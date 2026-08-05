# Best practices

Guidance from measurements on a real dataset: a retail schema
(catalog → store → customer → order → line item → category), about 55k nodes,
disk-first with memory-mapped paging. The numbers below are measured, not
illustrative.

## The one rule that matters most

> **Match the access pattern to the query shape:**
> aggregation → an indexed *fact* collection.
> Drill-down or point lookup → *graph* traversal.

The same question can be many times faster or slower depending only on which
pattern you choose. Every guideline below follows from this rule.

| query shape | right pattern | measured |
|---|---|---|
| point lookup, small fan-out (`store → supplier → warehouse`) | traversal | 0.009 ms |
| shallow aggregation (order counts) | fact + btree | 0.62 ms |
| medium aggregation (per-item averages) | fact + btree | 1.17 ms |
| deep join + aggregation (per-store category totals) | traversal or fact | 1–2.6 ms |
| deep hierarchical rollup (category → department → division) | fact, two levels | 27 ms |

## Do

1. **Aggregate over a flat, indexed fact collection.**
   For COUNT / AVG / SUM / rollups, store one flat `fact` record per row with
   the ancestry inlined (`store_id`, `customer_id`, `category_id`, `amount`),
   and index the columns you filter on:
   ```sql
   CREATE INDEX ON fact USING btree (store_id);
   SELECT customer_id, category_id, AVG(amount), COUNT(*)
   FROM fact WHERE store_id = ? GROUP BY customer_id, category_id;
   ```
   The index turns a full scan into a scoped one — measured, that is the
   difference between 1.5 ms and 15 ms.

2. **Traverse for drill-down and point lookups.**
   When you start from a known record and follow structure (this customer's
   orders, this store's suppliers), use `MATCH`. Following edges from a known
   node takes microseconds — no index probe, just pointer follows:
   ```sql
   SELECT sup._key, w._key
   FROM MATCH (s:store)-[:supplied_by]->(sup:supplier)-[:stored_at]->(w:warehouse)
   WHERE s._key = ?;
   ```

3. **Model observed things as nodes, plain relations as light edges.**
   If you analyze the thing itself (count it, group it, attach participants),
   make it a node (an act, an event). If you only pass through it, make it a
   light edge (`part_of`, `belongs_to`, `located_in`). Detail lives on nodes;
   edges stay small and fast.

4. **Bulk-load with `put_many` + `link_many`, then `compact()`.**
   A single `link()` syncs to disk per edge (milliseconds each — minutes for
   tens of thousands). `link_many` syncs once per batch: 66k edges went from
   92 s to 0.16 s. Finish a bulk import with `compact()` — it truncates the
   WAL (write-ahead log), writes the topology files, and makes reopen instant.

5. **Open large data with `open_paged()`.**
   Paged mode memory-maps the topology: it opens in about 4 ms regardless of
   database size, and the OS page cache keeps the hot working set in memory.
   Query speed holds compared to the resident mode.

## Don't

1. **Don't use traversal for an aggregation.**
   Traversing `store → order → sale` and grouping builds every path in memory.
   Measured on the same question: 1.97 ms as a traversal, 0.62 ms as a fact
   GROUP BY. This is the most common mistake.

2. **Don't roll up a many-to-many hierarchy with a plain traversal.**
   When an item belongs to several categories, a "flat mean" traversal counts
   it once per path and averages at the wrong level — the answer is wrong, not
   just slow. Aggregate in two steps instead: per-leaf average first, then
   roll up over facts.

3. **Don't put hot query data in the edge JSON bag.**
   Edge attributes you filter, group, or aggregate on should be primitive
   values (numbers, booleans, strings) — those are stored in fast columns.
   Nested objects and arrays go to the JSON bag, which is parsed per edge
   (measured about 2.3× slower). Keep the bag for nested or display-only data.

4. **Don't forget the index on fact filter columns.**
   Without it, every fact GROUP BY scans the whole collection. The index is
   what makes the fact pattern fast.

## Edge quick-reference

- **Create:** `INSERT ('a')-[:type {weight: 0.9, k: v}]->('b')` — primitive
  attributes ride the fast columns; nested values go to the JSON bag (slower).
  No attribute name is special; use whatever names fit your data.
- **Delete:** `DELETE ('a')-[:type]->('b')`
- **List edge types (vocabulary + counts):** `SHOW EDGES [FROM col] [TO col]`
- **List edge values:** `SELECT a._key, b._key, r.weight
  FROM MATCH (a:x)-[r:type]->(b:y)` — alias attributes to distinct names.
- **Aggregate along a path:** `PATH_PRODUCT(r.weight)` (decay),
  `PATH_SUM(r.cost)` (accumulated cost) over a multi-hop route.
