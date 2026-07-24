# Sekejap Best Practices

Evidence-backed guidance, measured against a real Postgres deployment — an LMS-style
course-management schema (course → classroom → student → assessment → question →
answer → learning-outcome): ~55k-node graph, ~12k answers, disk-first + mmap paged.
Numbers below are real measurements, not illustrative.

## The one rule that matters most

> **Match the access pattern to the query shape:**
> **aggregation → indexed *fact* collection · drill-down / point lookup → *graph* traversal.**

Sekejap beats Postgres on *every* query shape (2×–23× measured) **when you use the
right pattern.** The only way it loses is using the wrong one.

| Query shape | Right tool | Measured vs PG |
|---|---|---|
| point / low-fan-out lookup (`classroom → lecturer → staff`) | **traversal** | 23× (0.009 ms vs 0.21 ms) |
| shallow aggregation (participation counts) | **fact + btree** | 2× (0.62 ms vs 1.28 ms) |
| medium aggregation (per-question averages) | **fact + btree** | 3.5× (1.17 ms vs 4.12 ms) |
| deep join + aggregation (classroom CLO progress) | traversal or fact | 8–19× (1–2.6 ms vs 20 ms) |
| deep hierarchical rollup (CLO→PLO→GraduateProfile) | **fact, 2-level** | 5.6× (27 ms vs 148 ms) |

## Do

1. **Aggregate over a denormalized, indexed fact collection.**
   For any COUNT / AVG / SUM / rollup, build a flat `fact` node per row with the
   ancestry inlined (`classroom_id`, `student_id`, `clo_id`, `score`, …) and
   **index the columns you filter on**:
   ```sql
   CREATE INDEX ON fact USING btree (classroom_id);
   SELECT student_id, clo_id, AVG(score), COUNT(*)
   FROM fact WHERE classroom_id = ? GROUP BY student_id, clo_id;
   ```
   The btree turns a full scan into a scoped one — it is the difference between
   1.5 ms and 15 ms. **A fact without the index barely beats Postgres.**

2. **Traverse for drill-down and point lookups.**
   When you start from a known node and follow structure (this student's answers,
   this classroom's lecturers), use `MATCH`. Index-free adjacency from a known node
   is microseconds — no index probe, just pointer follows:
   ```sql
   SELECT l._key, st._key
   FROM MATCH (c:classroom)-[:has_lecturer]->(l:lecturer)-[:is_staff]->(st:staff)
   WHERE c._key = ?;
   ```

3. **Model *observed* things as nodes, *plain relations* as light edges.**
   If you analyse the thing itself (count it, group it, attach many participants) →
   **node** (an act / event). If you only traverse through it → **light edge**
   (`part_of`, `enrolled`, `belongs_to`). Richness lives on nodes; edges stay fast.

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
   Traversing `classroom → assessment → participation` and grouping materialises
   every path — slower than a `fact` GROUP BY (dashboard: traversal 1.97 ms vs
   fact 0.62 ms). This is the single most common mistake; it's the *only* thing
   that made Sekejap lose to Postgres in testing.

2. **Don't use a naive traversal for a many-to-many *hierarchical* rollup.**
   A "flat mean" traversal over a many-to-many hierarchy (CLO→PLO→GP) **double-
   counts** via fan-out and averages at the wrong level — it is both slower-looking
   *and wrong*. Use a fact 2-level aggregation (per-leaf average, then roll up).

3. **Don't put hot query data in the edge JSON bag.**
   Edge props you filter/group/aggregate on should be the typed `strength` weight
   (or, when available, a registered typed column) — not the JSON `{…}` bag, which
   parses per edge (~2.3× slower). JSON is for nested/variable/display-only data.

4. **Don't forget the index on fact filter columns.**
   An un-indexed fact GROUP BY full-scans every fact and barely beats Postgres.
   The index is what wins.

## Edge quick-reference

- **Create:** `INSERT ('a')-[:type {strength: 0.9, k: v}]->('b')` — `strength` is the
  fast typed weight; other props go to the JSON bag (slower).
- **Delete:** `DELETE ('a')-[:type]->('b')`
- **List edge *types* (vocabulary + counts):** `SHOW EDGES [FROM col] [TO col]`
- **List edge *values* (instances):** `SELECT a._key, b._key, r.strength
  FROM MATCH (a:x)-[r:type]->(b:y)` — alias props to a distinct name (`r.energy AS e`).
- **Path-aggregate a weight:** `PATH_PRODUCT(r.strength)` (confidence decay),
  `PATH_SUM(r.strength)` (accumulated cost) along a multi-hop route.

## Deployment shape

Sekejap doesn't have to replace Postgres uniformly — but it *can* serve the whole
workload faster when each query uses the right pattern. In practice: point the
**heavy analytical / graph endpoints** at Sekejap first (that's where Postgres hurts
most — 5–19×), and either keep shallow CRUD on Postgres or serve it from Sekejap too
(still 2–23× with the right pattern).
