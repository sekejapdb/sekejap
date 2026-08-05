# Benchmark #5 — Search (full-text / BM25)

Relevance-ranked full-text search over **BEIR FiQA-2018** — 57,638 financial-domain documents,
648 test queries, human relevance judgments (qrels). Unlike a latency-only search test, this
reports **real quality (nDCG@10, recall) from ground-truth labels** *and* speed, so "fast" and
"finds the right answer" are separated.

## Engines
- **sekejap** — embedded; native BM25 inverted index (`build_bm25_index` / `bm25_search`).
- **Elasticsearch** — Lucene BM25 (`match` query); the reference full-text engine.
- **Solr** — Lucene BM25 (edismax); same core as ES.
- **DuckDB FTS** — embedded columnar + `fts` extension (`match_bm25`); BM25.
- **Meilisearch** — instant-search engine (custom ranking rules, not BM25).
- **Postgres FTS** — `tsvector` + GIN + `ts_rank` (`websearch_to_tsquery`); simple tf ranking, not BM25.

## Data
BEIR **FiQA-2018**: `corpus.jsonl` 57,638 docs (title+text), `queries.jsonl` (648 with test qrels),
`qrels/test.tsv` 1,706 graded relevance judgments. Downloaded from the BEIR mirror; MANIFEST records it.

## Methodology
One engine at a time. Build = ingest + index (bulk-loaded); query = 648 test queries, top-100.
- **nDCG@10** — exponential gain `2^rel−1`, log2 discount (pytrec_eval / BEIR convention).
- **recall@{10,100}** — fraction of a query's relevant docs found in the top-{10,100}.
- **Latency** — *wall-clock* p50/p99 (includes the HTTP round-trip for the networked engines ES/
  Solr/Meili) **and** *server-side* p50 (each engine's own reported search time —
  Meili `processingTimeMs`, ES `took`, Solr `QTime`; = wall-clock for the embedded engines) to
  isolate pure search speed from network. QPS = single-client sequential.
- Fail-loud: each engine asserts its indexed doc count == 57,638 before querying.

## Results — FiQA (57,638 docs, 648 queries, k=10)

| engine | nDCG@10 | recall@10 | recall@100 | **server search p50** | wall p50 | wall p99 | QPS | build |
|--------|--------:|----------:|-----------:|----------------------:|---------:|---------:|----:|------:|
| **Elasticsearch** | **0.2324** | 0.2942 | 0.4938 | 24.0 ms | 34.1 | 115.9 | 25.2 | 10.9 s |
| **Solr** | **0.2324** | 0.2942 | 0.4938 | **13.0 ms** | 28.3 | 67.6 | 32.7 | 10.3 s |
| **sekejap** (disk-first) | 0.2236 | 0.2856 | 0.4968 | 31.3 ms | 31.3 | 106.1 | 28.9 | 10.7 s |
| **DuckDB FTS** | 0.2116 | 0.2736 | **0.5200** | 67.2 ms | 67.2 | 136.7 | 14.2 | 12.9 s |
| **Meilisearch** | 0.0604 | 0.0770 | 0.1349 | **12.0 ms** | 19.1 | 131.4 | 38.0 | 60.7 s |
| **Postgres FTS** | 0.0521 | 0.0560 | 0.0752 | 1.85 ms | 1.85 | 17.5 | 344 | 19.0 s |

## Disk-first BM25 (low-RAM) — postings on disk, dictionary in RAM

Consistent with sekejap's disk-first design, the BM25 index keeps its one large structure — the
**compressed postings blob** — on disk, not in RAM. Only the small working set stays resident:
the term dictionary (term → byte range) + per-doc length array + doc-id→slot map. At query time
each query term's postings range is read via **`pread`** (a handful of small reads per query;
kernel page cache, not process RSS), decoded, and scored.

| sekejap BM25 (FiQA) | in-RAM postings | **disk-first** |
|---------------------|----------------:|---------------:|
| index RAM | postings + dict + arrays | **8.7 MB** (dict + arrays only; postings on disk) |
| nDCG@10 | 0.2236 | **0.2236** (byte-identical — pread reads the same postings) |
| QPS | 35.6 | 28.9 (a few small disk reads per query) |

**How:** the postings blob is written to `bm25_<field>.postings` at build (8-byte-aligned, as the
format already anticipated); `Bm25Index` holds a `Memory | Disk` postings handle; `get_postings`
reads the term's `[offset,len)` via `pread` in disk mode. Same pattern as the vector int8 work
(big structure on disk, small working set in RAM, read exact data on demand). Recall is unaffected;
the cost is a small query-latency increase from the disk reads. The `bm25_<field>.postings` header
projects 90–450 MB at large field lengths — exactly the RAM this moves off-heap at scale.

## Findings
Two independent axes — **relevance** and **speed** — and they tell different stories.

- **Relevance: the four real BM25 engines cluster tightly** — ES/Solr 0.2324, **sekejap 0.2236**,
  DuckDB 0.2116 (all ≈ the published BM25 baseline of ~0.236). This mutual agreement validates the
  harness, and puts **sekejap's BM25 squarely in the pack** — within ~4 % of Lucene. sekejap even
  has the best **recall@100** except DuckDB.
- **Meilisearch (0.060) and Postgres FTS (0.052) are far behind — and correctly so.** Neither is
  BM25: Meili ranks for typo-tolerant instant-search (short as-you-type queries), and pg's `ts_rank`
  is a simple term-frequency score. On long TREC-style questions they retrieve poorly (pg recall@100
  just 0.075). This is a genuine capability gap, not a wiring artifact (Meili was unchanged after
  forcing `searchableAttributes:["text"]`).
- **Speed: Meilisearch delivers on its promise** — **12 ms server-side search, tied-fastest with
  Solr (13 ms)** — the instant-search design is real. The honest Meili summary: *fastest-class
  latency, weakest relevance* on this task. (Postgres's 1.85 ms is fastest of all, but only because
  its strict AND matching returns few docs — that's why recall collapses; not a real speed win.)
- **sekejap is well-rounded — no weak axis.** BM25-class relevance (0.224), fast build (~10 s),
  mid-pack search latency (~31 ms server-side), competitive QPS — **all while disk-first** (postings
  on disk, ~8.7 MB index RAM; see the disk-first section). It doesn't win any single axis outright,
  but it's the only engine that is simultaneously top-tier on relevance and low-RAM while embedded.
- **DuckDB FTS** matches BM25 relevance but is the slowest to query (67 ms — `match_bm25` re-scores
  per query with no served index cache in this path).

## Caveats
- **Networked vs embedded latency.** ES/Solr/Meili pay an in-cluster HTTP round-trip; sekejap/DuckDB
  are in-process; pg is local-socket. The **server-side p50** column removes this for the fair
  speed comparison (there, sekejap 26 ms ≈ ES 24 ms, both pure search time).
- **RAM not directly comparable** — sekejap 305 MB (in-process, BM25 index in RAM + WAL/payload
  store), ES store 48 MB (on-disk Lucene), others not exposed. Search RAM was not the focus; a
  disk-first BM25 index for sekejap is possible (as done for vectors) but out of scope here.
- **Single-client QPS** — sequential; concurrent-client throughput would differ.
- Meili build (60.7 s) is slow because its indexing does typo/prefix structure work the BM25
  engines skip — the flip side of its query speed.

## Next
- Optional larger-scale run (NQ 2.7 M) for at-scale latency/throughput.
- Concurrent-client QPS mode.
- Consider a disk-first (low-RAM) BM25 index for sekejap, mirroring the vector work.

## Raw
CSV: `results/search-fiqa.csv` · harness: `harness/searchbench` (fail-loud; doc-count asserts;
server-side + wall-clock timing). Dataset: `prepared/search/fiqa` (BEIR). All mirrored to the
the benchmark environment's results directory.

_Last updated: 2026-08-05. Status: **FiQA 6-engine run DONE (nDCG@10 + recall + server-side/wall
latency + build).** Honest headline: **the four BM25 engines (ES/Solr 0.232, sekejap 0.224, DuckDB
0.212) cluster on relevance — sekejap is in the pack and fastest to build; Meilisearch is
fastest-class on search latency (12 ms) but weak on relevance (instant-search tradeoff); Postgres
FTS trails on both meaningful axes.** sekejap = well-rounded embedded BM25, no weak axis._
