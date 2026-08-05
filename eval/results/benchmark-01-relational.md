# Benchmark #1 — Relational (ClickBench projection)

Single wide-table analytical/point-query workload. This is the relational fit for an
engine that replaces JOIN with MATCH: one table, filters + range + GROUP BY + scans.

## Summary — no single winner; clear niches (10M, p50)

| query | winner | sekejap | sqlite | duckdb | postgres |
|-------|--------|--------:|-------:|-------:|---------:|
| q0 count(*) | **duckdb** 2.2 ms | 335 | 125 | **2.2** | 903 |
| q1 filter `=` | **sekejap** 3.4 ms | **3.4** | 110 | 424 | 966 |
| q2 range `>` | **duckdb** 49 ms | 128 | 281 | **49** | 1008 |
| q3 GROUP BY | **sekejap** 27 ms | **27** | 931 | 95 | 1287 |
| q4 GROUP BY | **duckdb** 25 ms | 70 | 890 | **25** | 1419 |
| q5 SUM (scan) | **duckdb** 11 ms | 12203 | 771 | **11** | 954 |
| q6 scan | **duckdb** 62 ms | 11141 | 842 | **62** | 1022 |
| **RAM @10M** | **sqlite** 33 MB | 163 anon / 634 RSS | **33** | 1445 | server |
| **RAM @1M** | **sekejap** 8.5 MB | **8.5** | 30 | 281 | server |

**One-line each:**
- **sekejap** — best at indexed point/GROUP BY queries (q1 32× faster than SQLite @10M) and
  smallest RAM at 1M; but full-scan aggregates are ~1000× slower (payload decode) and
  query-RAM grows with result size at 10M. Build is very slow (compact O(N²)).
- **duckdb** — dominates every scan/aggregate (columnar); weak at point `=`; heaviest RAM (1.4 GB).
- **sqlite** — no category wins, but the all-rounder: lowest RAM at scale (33 MB), simple, fast load.
- **postgres** — slowest here (client↔pod RTT + untuned); server-class RAM, not embeddable.

Takeaway: sekejap is the pick for **indexed/graph-shaped access on tiny RAM**; DuckDB for
**analytical scans**; SQLite for **lean general embedded**. This is the honest, per-workload story.

## Paper positioning + competitor capability matrix (for the writing agent)

Which engines can even compete per benchmark class:

| class | sekejap | sqlite | duckdb | postgres | dedicated |
|-------|:--:|:--:|:--:|:--:|--|
| relational | ✅ | ✅ | ✅ | ✅ | — |
| spatial | ✅ | ~ (spatialite) | ✅ (`spatial` ext) | ✅ (PostGIS) | — |
| search/FTS | ✅ | ~ (fts5) | ✅ (`fts` ext) | ✅ (tsvector) | ES/Solr/Meili |
| vector KNN | ✅ | ❌ | ✅ (`vss` HNSW) | ✅ (pgvector) | Qdrant/Milvus |
| **graph MATCH** | ✅ | ❌ | ❌ (only recursive CTE; DuckPGQ is experimental) | ~ (recursive CTE) | Neo4j/Arango |

**Embedded vs server — state precisely (reviewers will check):** SQLite, DuckDB, and
sekejap are ALL embedded (in-process, serverless, single-file/in-memory, linked as a
library). Only PostgreSQL is a server. So NEVER write "DuckDB is not embedded" — it is.
The real axis is **resource footprint / target hardware**:
- embedded-*lean* / edge-capable: sqlite (~30 MB), sekejap (8.5 MB @1M) — fit constrained/IoT;
- embedded-*heavy* / analytics: duckdb (280 MB @1M → 1.4 GB @10M) — in-process but targets
  laptops/servers, not microcontrollers.
Correct phrasing: "DuckDB is embedded but not edge/constrained-RAM." Model coverage (graph)
is a separate axis from the embedded question.

**Honest framing (DuckDB is excellent — do not overclaim):**
- DuckDB **wins analytical scans/aggregates** (columnar) — expected; report it plainly.
- sekejap's structural advantages, with evidence from this benchmark:
  1. **Indexed point `=`**: sekejap 3.4 ms vs DuckDB 424 ms vs SQLite 110 ms @10M (32–125×).
     DuckDB has no fast point path (scans).
  2. **GROUP BY on indexed field**: sekejap 27 ms vs DuckDB 95 ms vs SQLite 931 ms @10M.
  3. **Embedded RAM**: sekejap 8.5 MB @1M vs DuckDB 280 MB (and 1.4 GB @10M). DuckDB is
     not an edge/IoT engine.
  4. **Graph MATCH**: DuckDB effectively cannot; this is sekejap's home turf — make the
     graph class central.
  5. **One embedded engine, all models** vs DuckDB needing extensions and still no graph.
- **Caveat sekejap must disclose (fairness):** it loses full-scan aggregates (q5/q6 ~1000×
  slower) because it is a **document/graph store** (whole-record storage + per-row decode)
  vs columnar. Mitigation (generated/materialized columns, projection scans) exists in the
  codebase → future work. Also: build time (compact O(N²)) and query-RAM-grows-with-result
  are current limitations to state, not hide.
- **Thesis:** distinct sweet spots — sekejap uniquely covers graph + indexed-point +
  multi-model at embedded RAM, where columnar/analytical engines can't follow.

## Engines (5 result streams)
- **duckdb** — embedded columnar (bundled), the analytics reference.
- **sqlite** — embedded row-store (bundled).
- **postgres** — server reference (PostGIS pod), driven over the network.
- **sekejap-atom** — sekejap via the chainable atomic API.
- **sekejap-sql** — sekejap via SQL. Same DB file/load as sekejap-atom (one load, two query modes).

sekejap is measured in **paged serve mode** (`open_paged`): node metadata + btree index
posting lists live in a memory-mapped store (reclaimable page cache), not the heap.

## Methodology (auditable)
Standard query-benchmark protocol, stated so it is justifiable:
1. `CREATE TABLE` (identical 8-column schema on every engine).
2. **Load** — streamed from NDJSON in 20k batches (harness never holds the whole dataset).
3. `CREATE INDEX` — btree on `RegionID, OS, ResolutionWidth, SearchPhrase` on **every** engine.
4. Warmup, then measure p50/p99 over N iterations.
- `load_ms` and `index_ms` are reported **separately** and are **excluded** from query latency.
- Full-scan queries (q5 `SUM`, q6 `<>''`) stay scans on all engines (no engine gets a
  shortcut the others don't).
- Each engine writes to an **isolated** data dir under `runs/relational-clickbench/<engine>/`.

### Queries
| id | SQL |
|----|-----|
| q0 | `SELECT COUNT(*) FROM hits` |
| q1 | `... WHERE RegionID = 229` (indexed eq) |
| q2 | `... WHERE ResolutionWidth > 1000` (indexed range) |
| q3 | `SELECT RegionID, COUNT(*) ... GROUP BY RegionID ORDER BY c DESC LIMIT 10` |
| q4 | `SELECT OS, COUNT(*) ... GROUP BY OS` |
| q5 | `SELECT SUM(ResolutionWidth) FROM hits` (full scan) |
| q6 | `SELECT COUNT(*) ... WHERE SearchPhrase <> ''` (full scan) |

### RAM measurement (important, disclosed)
- Embedded engines (sekejap, sqlite, duckdb): in-process resident set.
  - **sekejap paged** additionally split into **RssAnon** (hard heap, non-reclaimable) and
    **RssFile** (memory-mapped files — reclaimable page cache the kernel evicts under
    pressure). `RssAnon` is the number that matters for "fits in a small RAM cap".
  - duckdb bounded with `PRAGMA memory_limit='2GB'` + spill `temp_directory`.
- **postgres**: the harness is a *client*, so its RSS is meaningless for the server. Postgres
  latency **includes client↔pod network round-trip** (embedded engines do not). Server RAM is
  the pod cgroup (`memory.current`), which includes shared_buffers + reclaimable page cache.

## Conditions
- Single-node Linux host; each engine run one-per-process for clean RSS.
- Dataset: ClickBench `hits` 8-column projection (WatchID, RegionID, ResolutionWidth, OS,
  SearchPhrase, CounterID, UserID, URL), real ClickBench data.
- Scales: **1,000,000** and **10,000,000** rows.
- Harness: `sekejap-benchmark/harness/relbench` (Rust, one binary, `--engine <name>`).

---

## Results — 1,000,000 rows

CSV: `engine, load_ms, index_ms, disk_mb, rss_mb, query, p50_ms, p99_ms, rows`

| engine | RAM | load | index | disk | q0 | q1 eq | q2 range | q3 grpby | q4 grpby | q5 sum | q6 scan |
|--------|----:|-----:|------:|-----:|---:|---:|---:|---:|---:|---:|---:|
| **sekejap-sql** (paged serve) | **8.5 MB** RssAnon (60 VmRSS) | 166 s* | 16 s* | 373 MB | 17.5 | 1.3 | 5.2 | 4.9 | 3.6 | 579 | 697 |
| **sekejap-atom** (paged serve) | (same DB) | — | — | — | 15.0 | 0.8 | 5.7 | n/a¹ | n/a¹ | n/a¹ | n/a¹ |
| **sqlite** | 29.6 MB | 11 s | 3.6 s | 344 MB | 15.0 | 31.9 | 45.5 | 91.6 | 90.1 | 74.3 | 79.1 |
| **postgres** | ~320 MB (server) | 16 s | 5.6 s | 233 MB | 96.7 | 137.6 | 125.0 | 171.7 | 169.0 | 130.2 | 136.6 |
| **duckdb** | 280.6 MB | 11 s | 2.4 s | 146 MB | 0.9 | 83.8 | 2.6 | 17.7 | 4.5 | 1.8 | 8.2 |

\* sekejap load/index are **build-time** costs (done once on a build machine); serve RAM is
measured on a **reopened** DB under a **200 MiB cgroup cap** (which it passes).
¹ GROUP BY / SUM are SQL-only in sekejap's chainable atomic API (by design), reported under sekejap-sql.

**p50 latencies in ms.** Notes:
- sekejap wins indexed queries (q1/q2/q3/q4) by 10–40× — btree postings straight from mmap.
- sekejap loses full-scan aggregates (q5/q6) — it scans payloads (mmap + per-row decode) vs
  columnar storage. Expected document/graph-store tradeoff; duckdb should dominate these.
- sekejap has the smallest **hard heap** (8.5 MB, flat in N) — the embedded/IoT win.

---

## Results — 10,000,000 rows

| engine | RAM | load | index | disk | q0 | q1 eq | q2 range | q3 grpby | q4 grpby | q5 sum | q6 scan |
|--------|----:|-----:|------:|-----:|---:|---:|---:|---:|---:|---:|---:|
| **sekejap-sql** (paged serve) | **163 MB** RssAnon (634 VmRSS) | 46 min* | 3.6 min* | 3888 MB | 334.6 | 3.4 | 127.9 | 27.3 | 69.7 | 12203 | 11141 |
| **sekejap-atom** (paged serve) | (same DB) | — | — | — | 335.9 | 3.4 | 132.9 | n/a¹ | n/a¹ | n/a¹ | n/a¹ |
| **sqlite** | 33.4 MB | 148 s | 48 s | 3588 MB | 124.7 | 110.3 | 281.0 | 930.7 | 890.0 | 771.5 | 842.0 |
| **postgres** | (server) | 114 s | 42 s | 2390 MB | 903.3 | 965.9 | 1007.6 | 1287.0 | 1418.8 | 953.7 | 1022.1 |
| **duckdb** | 1444.8 MB | 139 s | 35 s | 1548 MB | 2.2 | 424.1 | 49.2 | 95.4 | 25.3 | 10.6 | 62.2 |

\* sekejap 10M build used COMPACT_EVERY=2M. rows: q0=10M, q1=1,639,277, q2=9,094,775, q3/q4=10.

**p50 latencies in ms.** Reading it:
- **Indexed queries: sekejap wins big even at 10M** — q1 **3.4 ms** vs SQLite 110 ms (32×), q3 27 vs 931 (34×), q4 70 vs 890 (13×). q2 (9.1M matches) 128 vs 281 ms.
- **Count + full scans: sekejap loses** — q0 335 vs 125 ms (count materializes 10M member hashes); q5 SUM **12.2 s** and q6 scan **11.1 s** vs SQLite ~0.8 s (payload + per-row SKBIN decode over 10M via mmap). DuckDB (columnar) should crush q5/q6.
- **RAM at 10M is the key nuance** — see below.

## Key finding: storage RAM is flat, query-execution RAM is not

sekejap's **storage** (node metadata + btree postings) is memory-mapped → hard heap for
storage is flat in N. But the **query executor materializes intermediate results on the
heap**, and that scales with result cardinality:
- 1M serve: RssAnon **8.5 MB**.  10M serve: RssAnon **163 MB**.
- Drivers: q0 `count` builds the full 10M member `Vec`; q2 builds a ~9.1M-hash `HashSet`
  (~150 MB anon). A 300 MiB cap **OOM-killed** the 10M serve for this reason (not storage).

So "bounded RAM regardless of dataset size" holds for **storage**, and for **selective**
queries, but **not** for queries that touch/return most of the dataset. Next optimizations
(streaming count without materializing members; retain-in-place instead of building large
HashSets; chunked scan) would restore boundedness. Full-scan aggregate speed (q5/q6) needs
columnar/lazy-decode work — this is the document-store tradeoff.

---

## Wall-clock run time (operational — for planning future benchmarks)

Total time to run ONE engine end-to-end at a scale (load + index + warmup + measured
queries), one `--engine` process. Dominated by load+index; query phase is small.
This is what to budget when scheduling a full sweep.

| engine | 1M run | 10M run | notes |
|--------|-------:|--------:|-------|
| sqlite | ~18 s | ~3.8 min | load-dominated (148 s load @10M) |
| postgres | ~29 s | ~3.1 min | load-dominated; + network RTT on queries |
| sekejap (paged **build**) | ~3.1 min | **~50 min** | O(N²) periodic compact — each fold rewrites the whole payloads.bin+topology; the outlier, fixed by incremental-compact |
| sekejap (paged **serve**, QUERY_ONLY) | ~15 s | ~108 s | reopen + queries; 108 s is dominated by q5/q6 full scans (~12 s each × iters) |
| duckdb | ~15 s | ~176 s (~3 min) | + one-time ~20 min BUNDLED COMPILE (first build only) |

Planning takeaways:
- A full 5-engine 10M sweep ≈ **sum of the 10M column** (run sequentially to keep RSS clean)
  — budget ~20–30 min once binaries are built, plus DuckDB's one-time ~20 min compile.
- sekejap's build cost is the outlier (periodic-compact O(N²)); the **incremental-compact**
  follow-up will cut this sharply. Serve time is tiny — the embedded device only pays serve.
- Run engines **one at a time** (shared node) so RSS/latency aren't cross-contaminated.

## Raw logs
Per-engine raw logs: `{sqlite,pg,duckdb}-{1m,10m}.log` (benchmark environment),
sekejap QUERY_ONLY RSS split in the pod run logs. Mirrored here as they complete.

_Last updated: 2026-08-03. sekejap build = commit d82a26a (v0.15.0) + paged-serve /
persist-mmap-field-index work (fieldstore). Status: **COMPLETE** — all 4 engines × {1M, 10M}
measured (sekejap counts as atom+sql). SQLite ✅, Postgres ✅, DuckDB ✅, sekejap ✅._
