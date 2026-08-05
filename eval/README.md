# sekejap — benchmark reproduction (CIDR 2027)

Harnesses, datasets, and results behind the paper's evaluation: six categories
(relational, graph, spatial, vector, full-text, hybrid), sekejap vs specialist
systems, one constrained node.

- `harness/` — one Rust harness per category (plus `hybridmulti` for the
  DuckDB / Postgres+pgvector hybrid competitors). Each is a standalone crate
  that depends on the sekejap crate at the repo root, so building here
  benchmarks exactly the checked-out engine (tag `cidr2027-submission` is the
  exact paper snapshot).
- `results/` — the measured numbers cited in the paper: one
  `benchmark-0N-<category>.md` per category plus the raw CSVs.
- `scripts/` — dataset download + preprocessing helpers.

## Environment

Numbers in `results/` were collected on a single node (12 cores, 47 GiB RAM,
Linux) with each server-based competitor in its own container under explicit
CPU/memory limits. sekejap, SQLite, and DuckDB run in-process; the server
engines (Postgres/PostGIS/pgvector, Neo4j, ArangoDB, Qdrant, Redis Stack,
Elasticsearch, Solr, Meilisearch) pay a localhost round trip — the per-category
docs state this wherever it matters.

## Datasets

All public. Three steps, all rooted at `$SEKEJAP_DATA` (default `./data`):

1. `scripts/download-datasets.sh` — fetch/generate the raw datasets
   (+ `scripts/fetch-manual-datasets.sh` for the externally-verified links);
2. `scripts/preprocess.sh` — normalize raw data into the canonical
   `prepared/<category>/` files the harnesses load (uses the `duckdb` CLI,
   override with `DUCKDB_BIN`); writes `prepared/MANIFEST.txt` with row counts;
3. run the harnesses (below).

The per-category docs list exact sizes; [datasets.md](datasets.md) is the
full provenance table — exact download URLs, acquisition steps, licenses, and
verified row counts (including notes on dead mirrors and which hosts actually
work).

| category | dataset | source |
|---|---|---|
| relational | ClickBench `hits` (1M/10M slice) | github.com/ClickHouse/ClickBench |
| graph | LDBC SNB SF1, SNAP com-Amazon | ldbcouncil.org, snap.stanford.edu |
| spatial | GeoNames 2M points, NYC (PostGIS workshop) | geonames.org, postgis.net workshop data |
| vector | SIFT1M (1M × 128-d, L2) + published ground truth | corpus-texmex.irisa.fr |
| search | BEIR FiQA-2018 (57,638 docs, 648 test queries, qrels) | github.com/beir-cellar/beir |
| hybrid | FiQA + `text-embedding-3-small` (1536-d) embeddings | `scripts/generate_embeddings.py` |

Hybrid embeddings are not shipped (340 MB); regenerate with
`OPENROUTER_API_KEY=... python3 scripts/generate_embeddings.py` (~$0.25 of API
usage, writes raw little-endian f32 `corpus_emb.f32` / `queries_emb.f32` with
matching `*_ids.txt`).

## Running

Each harness expects its inputs under `data/prepared/<category>/` relative to
the working directory (symlink or set up as you like), and writes scratch DBs
under `data/runs/`.

```sh
cd eval/harness/<name>
cargo build --release
./target/release/<name> [--engine <engine>]   # see each main.rs header
```

- `relbench` — `--engine sekejap|sqlite|duckdb|postgres`, ClickBench queries.
- `graphbench` — `--engine sekejap|neo4j|arango`, datasets `ldbc|amazon`.
- `spatialbench` — `--engine sekejap|duckdb|postgis`; `TRAIN=300` runs 300
  seeded queries per operation, every result oracle-checked (geodesic ops
  against live PostGIS `geography`); mismatches are counted per op.
- `vecbench` — `--engine sekejap|duckdb|pgvector|qdrant|es|weaviate|redis`;
  `DISK=1` selects sekejap's disk-first index; recall@10 against SIFT ground
  truth; fail-loud (exact stored-count asserts, full-index waits).
- `searchbench` — `--engine sekejap|es|solr|duckdb|meili|pgfts`; nDCG@10 +
  recall from FiQA qrels (BEIR convention, exponential gain).
- `hybridbench` — sekejap-only: BM25, dense, and weighted-RRF fusion in one
  engine over identical embeddings.
- `hybridmulti` — `duckdb|pg`: the same three modes inside DuckDB (fts+vss)
  and Postgres (tsvector + pgvector), same corpus, same embeddings, same
  scorer.

Server engines are expected at their default hostnames/ports (see each
`main.rs`; override via the env vars noted there). Correctness is enforced:
harnesses assert exact indexed counts before querying and oracle-check query
results; a mismatch is reported, not silently averaged away.

## Reading the results

Each `results/benchmark-0N-*.md` is self-contained: engines, dataset, method,
the results table, and an honest reading (including where sekejap loses —
e.g. columnar full-scan aggregates, polygon×polygon intersection, and dense
small-graph neighborhood counts). The CSVs are the single source of truth the
paper tables were generated from.
