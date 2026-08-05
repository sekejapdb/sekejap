# Benchmark #2 — Graph (traversal + shortest path)

Directed graph traversal: the workload where property-graph engines shine and where
columnar/relational engines (DuckDB, SQLite) can't natively compete — DuckDB is **absent**
here (no MATCH/traversal; DuckPGQ is experimental).

## Engines
- **sekejap** — via SQL `MATCH` (`*1..k` typed BFS, `MATCH SHORTEST`). In-process (embedded).
- **neo4j** — the graph gold standard; Cypher over the HTTP transaction endpoint (server).
- **arango** — multi-model graph; AQL `OUTBOUND` traversal + `SHORTEST_PATH` over HTTP (server).

## Datasets
- **LDBC** social graph: 9,892 person nodes, 180,623 `KNOWS` edges (labeled, standard).
- **SNAP Amazon** co-purchase: ~262k nodes, 925,872 edges (topology-only).
  Provenance: LDBC SNB; SNAP (`com-amazon`/`amazon0601`) — see `prepared/MANIFEST.txt`.

## Methodology (auditable)
Standard protocol, mirrors benchmark #1:
1. Load nodes + edges (sekejap `link_many` bulk; Neo4j `UNWIND` batched CREATE with an
   `:N(id)` index; Arango `/_api/import` bulk into a document + edge collection).
2. Warmup, then measure p50/p99 over N iterations. Load time reported separately, excluded
   from query latency.
3. **The harness builds its own in-memory adjacency from the CSV, picks the query
   parameters, and computes the EXPECTED answer** (BFS) — every engine's result is
   correctness-checked, not just timed. SEED = max out-degree node; DST = farthest node
   from SEED (for shortest path).

### Queries (fair, idiomatic per engine)
| id | meaning | sekejap | neo4j | arango |
|----|---------|---------|-------|--------|
| q1 1-hop | # distinct nodes within 1 hop | `MATCH (a:N)-[:E*1..1]->(b) … GROUP BY b._key` | `-[:E*1..1]-> count(DISTINCT b)` | `1..1 OUTBOUND … uniqueVertices:global` |
| q2 2-hop | … within 2 hops | `*1..2` | `*1..2` | `1..2 … global,bfs` |
| q3 3-hop | … within 3 hops | `*1..3` | `*1..3` | `1..3 … global,bfs` |
| q4 s-path | shortest-path length SEED→DST | `MATCH SHORTEST (a:N)-[r:E*]->(b:N)` `length(r)` | `shortestPath((a)-[:E*]->(b))` | `OUTBOUND SHORTEST_PATH … TO …` |

**k-hop = the within-k neighborhood (BFS visited-set, distance ≤ k)** — the standard,
efficient formulation on every engine (avoids the path-enumeration blowup that an
"exactly-k distinct" phrasing causes on Arango).

### RAM / fairness
- sekejap is embedded → in-process RSS (VmHWM). Neo4j + Arango are **servers**: latency
  includes client↔pod HTTP round-trip and RSS is the pod cgroup (reported `-1` in the CSV;
  measured separately). Disclosed, same as Postgres in #1.

## Conditions
- Single-node Linux host; one engine per process. Exact images: **`neo4j:5-community`**,
  **`arangodb:3.12`** (Community). sekejap: patched disk-first branch on tag `d82a26a`.
- Harness: `sekejap-benchmark/harness/graphbench` (Rust; Neo4j/Arango via blocking `ureq`).

---

## Results — LDBC (9,892 nodes / 180,623 edges)

All engines returned the harness-verified correct answers: **q1=814, q2=5628, q3=7185,
shortest-path=5.**

| engine | RAM (proc VmHWM†) | load | q1 1-hop | q2 2-hop | q3 3-hop | q4 s-path |
|--------|----:|-----:|---------:|---------:|---------:|----------:|
| **sekejap** (disk-first) | 53 MB† | **1.9 s** | 11.1 | 91.0 | 118.2 | **7.2** |
| **neo4j** | server | 6.2 s | 23.2 | 35.6 | **56.2** | 20.3 |
| **arango** | server | 10.7 s | **5.5** | **21.5** | 147.7 | 7.8 |

**p50 ms.** Mixed — each engine wins somewhere: Arango fastest at 1–2 hop; sekejap and Arango are
comparable at shortest-path (sekejap slightly ahead this run, 7.2 vs 7.8 ms);
Neo4j best at 3-hop; sekejap has the fastest load (1.9 s via `link_many`), ties on
shortest-path, and now beats Arango at 3-hop (118 vs 148 ms, disk-first CSR locality), but is
mid-pack on the dense small graph's neighborhood counts (2-hop the weak spot).
<br><sub>† `RAM` here is **process VmHWM** (peak, incl. the harness graph struct + build) — not
engine-only. The disk-first **engine heap adjacency** is far smaller: **12.7 MB → 0.7 MB** after
spill (see the disk-first section). Neo4j/Arango run as separate server processes (RAM `-1`).</sub>
sekejap's neighborhood-count (`*1..k` GROUP BY) is mid-pack — note its *ultra*-fast graph
path is exact-k destination **aggregation** (the `child_of*3 → GROUP BY attr` shape), which
this within-k neighborhood-count query does not exercise (candidate future q5).

## Results — SNAP Amazon (334,863 nodes / 925,872 edges)

All engines returned the harness-verified correct answers: **q1=168, q2=376, q3=713,
shortest-path=9.** (Sparser, deeper graph than LDBC → smaller neighborhoods, longer paths.)

| engine | RAM (proc VmHWM†) | load | q1 1-hop | q2 2-hop | q3 3-hop | q4 s-path |
|--------|----:|-----:|---------:|---------:|---------:|----------:|
| **sekejap** (disk-first) | 461 MB† | **15.5 s** | **2.0** | **5.2** | **11.8** | **0.42** |
| **neo4j** | server | 45 s | 19.4 | 25.7 | 23.5 | 21.3 |
| **arango** | server | 58 s | 7.6 | 13.5 | 14.8 | 7.9 |

**p50 ms.** On the larger, sparse graph **sekejap wins EVERY query** and dominates
shortest path (0.42 ms vs 7.9/21.3 ms → 19–51×), with the fastest load (15.5 s vs 45/58 s).
<br><sub>† `RAM` = **process VmHWM** (peak, includes the harness's own in-RAM graph copy +
build spike) — not engine-only. The disk-first **engine heap adjacency** is **96.3 MB → 21.0 MB**
after spill (edges now mmap'd CSR page-cache; see the disk-first section). A future paged node
store would also move the NodeData map off-heap.</sub>

## Summary

| | LDBC (small, dense) | Amazon (large, sparse) |
|---|---|---|
| 1-hop | arango | **sekejap** |
| 2-hop | arango | **sekejap** |
| 3-hop | neo4j | **sekejap** |
| **shortest path** | sekejap ≈ arango | **sekejap (19–51×)** |
| load | **sekejap** (1.9 s) | **sekejap** (15.5 s) |

- **DuckDB is absent** — no native graph traversal. That's the headline for the paper:
  the analytical engine that dominated benchmark #1's scans simply cannot run this class.
- **sekejap is fully competitive with the graph-native engines**, wins the larger/sparse
  graph outright, and dominates **shortest path** — while also being **embedded** (Neo4j and
  Arango are heavyweight JVM/C++ servers). On the small dense graph Arango edges out the
  neighborhood counts; sekejap's mid-pack there is the `*1..k` neighborhood-count path, not
  its ultra-fast exact-k destination **aggregation** (future q5 would showcase that).
- Fair-play note: Neo4j/Arango latencies include HTTP RTT (in-process sekejap doesn't); load
  excludes it. Even so, the traversal-compute gap on Amazon is far larger than any RTT.

---

## Disk-first adjacency (low-RAM) — added 2026-08-05

Consistent with sekejap's disk-first design, the edge adjacency (the graph's bulk) is spilled to
an **mmap'd CSR file** (`adj_{fwd,rev}_csr.bin`); only a compact node→(offset,count) index stays
in heap. Traversal reads each node's edges as a zero-copy `&[Edge]` slice into the mmap (page
cache, reclaimable) — so heap RAM is bounded to the offset index, not the edge count.

| dataset | edges | heap adjacency RAM | traversal |
|---------|------:|-------------------:|-----------|
| amazon | 925,872 | **96.3 MB → 21.0 MB** (4.6×) | 1/2/3-hop + shortest-path **byte-identical**, BFS-verified |
| ldbc | 180,624 | **12.7 MB → 0.7 MB** (18×) | idem |

The edge blob (~44 MB on amazon) is now mmap'd page cache instead of heap `HashMap<u64,Vec<Edge>>`.
Query results unchanged (same correctness gate passes). Single interface: `spill_edges_to_disk()`
on a disk-backed DB; in-RAM adjacency remains only for ephemeral `CoreDB::new()`. Projected at
10M edges: ~550 MB in-RAM adjacency → a few-MB heap index + reclaimable page cache.

## Audit / correctness gate
- The harness computes the expected answer (BFS oracle) and now **exits nonzero on any
  mismatch** — a wrong answer can never be logged as a valid result (auditor requirement).
- This gate immediately caught a real bug: Neo4j's cleanup (`MATCH (n) DETACH DELETE n`)
  silently fails on a large graph (exceeds its transaction memory), so a **re-run loaded on
  top of leftovers → duplicate nodes → 2× counts.** Fixed: batched relationship-then-node
  delete; Neo4j store also wiped once to recover. **Final matrix: all 6 runs (3 engines ×
  2 datasets) exit 0, every result BFS-verified correct.**
- **Load-completeness gate (added 2026-08-05):** after ingest the harness asserts the full
  node + edge counts (not just the seed component the BFS oracle checks). **Hard assert is
  currently sekejap-only**, re-verified: **ldbc 9,892 nodes / 180,623 edges; amazon 334,863 /
  925,872** — exact dataset counts. **Neo4j/Arango full-load is corroborated (not hard-asserted)**
  by the identical result-set counts across all three engines (814/5,628/7,185 ldbc; 168/376/713
  amazon) — same answers ⇒ same graph in the queried components. Adding explicit node/edge count
  asserts to the Neo4j/Arango paths (and re-running them under it) is a small harness follow-up.

## Raw logs
Per-engine raw logs: `graph-<engine>-<dataset>.log` (6 files, benchmark environment)
+ consolidated `graph-results.csv` (24 rows). Mirrored to this repo's `results/`.

_Last updated: 2026-08-05. Status: **COMPLETE & AUDIT-VERIFIED** — LDBC ✅ + Amazon ✅
(sekejap, neo4j, arango), all runs correctness-gated (exit 0). DuckDB/SQLite excluded
(no native graph). **sekejap adjacency is now disk-first** (mmap'd CSR; heap RAM 96→21 MB amazon,
12.7→0.7 MB ldbc; traversal byte-identical) — only sekejap was re-run, competitor results unchanged._
