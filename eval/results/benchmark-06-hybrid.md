# Benchmark #6 — Hybrid (dense + sparse + fusion in one multi-model engine)

Dense-vector retrieval, lexical retrieval, and their **reciprocal-rank fusion (RRF)**,
each served by a **single multi-model engine** over one copy of the corpus. This is the
multi-model category: the comparison is not against single-purpose search engines but
against other engines that can do **both** dense and sparse **in one system**. The point
is the *architectural collapse* — no separate search engine + vector DB + glue.

## Engines (all single-system, same corpus, same embeddings)
- **sekejap** — embedded, disk-first. BM25 inverted index + disk-first HNSW, one `CoreDB`.
- **DuckDB** — embedded. `fts` extension (BM25) + `vss` extension (HNSW), one connection.
- **Postgres + pgvector** — one database. `tsvector`/`ts_rank` FTS + pgvector HNSW.

All engines rank the **same FiQA corpus** (57,638 docs) and the **same precomputed
`openai/text-embedding-3-small` (1536-d) embeddings** (identical vectors across engines =
fair), fuse dense+sparse with weighted RRF (`k=60`), and are scored by the **same qrels**
(nDCG@10, recall@10, identical scorer). Vectors are L2-normalized so L2 ranks as cosine.

## Data
BEIR **FiQA-2018**: 57,638 docs, 648 test queries, 1,706 graded qrels (same corpus as
Benchmark #5). Embeddings generated once and reused by every engine.

## Methodology
One engine at a time. Each ingests the corpus once with a text field (lexical index) and a
vector field (HNSW), then for each query computes the sparse ranking, the dense ranking, and
a weighted-RRF fusion. `dw` = dense weight (sparse weight fixed at 1); `dw1` is classic
equal-weight RRF. The weight sweep is built once from cached per-query rankings.

## Results — FiQA (57,638 docs, 648 queries, k=10)

| engine | lexical nDCG@10 | dense nDCG@10 | hybrid nDCG@10 (dw1 / best) | dense recall@10 | index build |
|--------|----------------:|--------------:|---------------------------:|----------------:|------------:|
| **sekejap** (embedded, disk-first) | 0.2236 (BM25) | 0.4379 | 0.3558 / **0.4300** | 0.5112 | **107.9 s** |
| **DuckDB** (fts + vss) | 0.2116 (BM25) | 0.4449 | 0.3651 / **0.4321** | 0.5184 | 169.5 s |
| **pgvector** (FTS + pgvector) | 0.0522 (ts\_rank) | 0.4462 | 0.4154 / **0.4460** | 0.5196 | 225.8 s |

"best" = best point on the dense-weight sweep (dw8). Full sweep in `hybrid-all.csv`.
pgvector build = 49.2 s copy+tsvector + 176.6 s HNSW.

## Cross-validation (the harness agrees with Benchmark #5)
- **DuckDB BM25 0.2116** is *identical* to Benchmark #5's DuckDB FTS nDCG@10 (0.2116).
- **pgvector lexical 0.0522** matches Benchmark #5's Postgres FTS (0.0521); pgvector's
  lexical arm is `ts_rank`, **not** true BM25 — hence the low score, consistent across both
  benchmarks.
- All three **dense** scores land at ≈0.44 (0.4379–0.4462) because they rank the identical
  embeddings; small gaps are ANN/HNSW approximation differences.

These matches are the correctness signal: the multi-engine harness reproduces the
independently-measured single-model numbers.

## Honest reading

**On FiQA the dense signal dominates, and fusion does not beat it — for any of the three
engines.** text-embedding-3-small scores ≈0.44 nDCG@10; BM25 scores ≈0.22 (and PG `ts_rank`
only 0.05). Reciprocal-rank fusion of a strong retriever with a much weaker one *dilutes*
the strong signal at equal weight, then recovers toward the dense ceiling as the dense arm
is up-weighted, without exceeding it:

- sekejap: 0.3558 (dw1) → 0.4300 (dw8), dense 0.4379
- DuckDB:  0.3651 (dw1) → 0.4321 (dw8), dense 0.4449
- pgvector: 0.4154 (dw1) → 0.4460 (dw8), dense 0.4462

pgvector's equal-weight hybrid barely dips only because its lexical arm (`ts_rank`, 0.05) is
so weak it hardly perturbs the dense ranking — not because fusion helped.

This is the expected, well-documented behavior: **hybrid retrieval beats its components only
when the components are comparable in strength.** FiQA is a semantic-matching dataset where
lexical overlap is weak, so BM25 has little to add that dense has not already found. Datasets
where exact-term matching matters are where fusion overtakes either signal; FiQA is not one,
and none of the three engines claim otherwise here.

## What this benchmark demonstrates

The contribution is **architectural, and it is a fair multi-model comparison**:

1. **All three do hybrid in one system** — so single-system multi-model is not unique to
   sekejap. What differs is the *deployment*: sekejap and DuckDB are **embedded** (no server);
   pgvector is a **server**. Among the embedded two, sekejap is **disk-first** (compact int8
   working set in RAM + full vectors on disk, from #4; disk-first BM25 postings from #5),
   whereas DuckDB's `vss` HNSW is RAM-resident.
2. **sekejap builds the combined index fastest** (107.9 s vs 169.5 s vs 225.8 s) while
   reaching equal hybrid quality (0.43 vs 0.43 vs 0.45, all bounded by the shared dense
   ceiling).
3. **Fusion is a cheap in-process rank merge** in all three; no network hop, no second store,
   no sync pipeline.

Takeaway for the paper: on a semantic dataset, sekejap's hybrid retrieval is **on par with a
mainstream embedded analytics engine (DuckDB) and a mainstream server stack (Postgres +
pgvector)** — at equal quality, faster combined build, and with the only disk-first,
bounded-RAM, embedded profile of the three.

## Reproduce
- sekejap: `harness/hybridbench` (weighted-RRF sweep in one `CoreDB`).
- DuckDB + pgvector: `harness/hybridmulti` (`hybridmulti duckdb` / `hybridmulti pg`), same
  FiQA corpus + same `corpus_emb.f32` / `queries_emb.f32` (raw little-endian f32, n×1536).
- Outputs: `results/hybrid-fiqa.log` (sekejap), `results/hybrid-multi.log` (DuckDB+pg),
  combined `results/hybrid-all.csv`.
