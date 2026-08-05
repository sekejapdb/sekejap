# Dataset provenance

Exactly where each dataset came from, how it was acquired and prepared, and
its license. Raw files land in `data/datasets/`, canonical prepared files in
`data/prepared/` (see [README.md](README.md) for the pipeline). Verified row
counts as of 2026-08-03.

| Dataset | Class | Real source used | Acquisition | License | Prepared output (rows) |
|---|---|---|---|---|---|
| **TPC-H SF1** | relational | *generated* via DuckDB `tpch` extension (`INSTALL tpch; CALL dbgen(sf=1); EXPORT DATABASE`) | generated locally | TPC (free for non-audited research) | `relational/tpch_schema.sql` + 8 CSVs — lineitem 6,001,215 · orders 1,500,000 · partsupp 800,000 · part 200,000 · customer 150,000 · supplier 10,000 · nation 25 · region 5 |
| **ClickBench** | relational | `https://datasets.clickhouse.com/hits_compatible/hits.parquet` | duckdb httpfs partial read, `LIMIT 10000000` (capped mem 5 GB + disk spill to avoid OOM) | Apache-2.0 (ClickBench) | `relational/hits_10m.parquet` — **10,000,000** rows × 105 cols (1.4 GB) |
| **GeoNames** | spatial | `https://download.geonames.org/export/dump/allCountries.zip` | curl + unzip; subset first 2M rows via duckdb | CC BY 4.0 | `spatial/geonames.parquet` — **2,000,000** points |
| **NYC PostGIS workshop** | spatial | `https://s3.amazonaws.com/s3.cleverelephant.ca/postgis-workshop-2020.zip` | curl + unzip; shp → WKT parquet via duckdb `spatial` (`ST_Read`/`ST_AsText`) | public domain / workshop data | `spatial/nyc_*.parquet` — census_blocks 38,794 · streets 19,091 · homicides 3,984 · subway_stations 491 · neighborhoods 129 (+ census_blocks_2000) |
| | | *NOTE:* the bare `s3.cleverelephant.ca/...` host is **dead**; the `s3.amazonaws.com/s3.cleverelephant.ca/...` form works. | | | |
| **SNAP com-amazon** | graph | `https://snap.stanford.edu/data/bigdata/communities/com-amazon.ungraph.txt.gz` | curl + gunzip; strip `#` comments → CSV | SNAP terms (research) | `graph/snap_amazon_edges.csv` — **925,872** edges |
| **LDBC SNB SF1** | graph | `https://repository.surfsara.nl/datasets/cwi/snb/files/social_network-csv_basic/social_network-csv_basic-sf1.tar.zst` | curl **`-k`** (SURF TLS chain may not be in default trust stores) + zstd extract | Apache-2.0 (LDBC datagen) | `graph/ldbc_person.csv` 9,892 · `graph/ldbc_knows.csv` **180,623** |
| | | *NOTE:* the widely-cited `swat.daanvdn.nl/ldbc/...` mirror is **dead (does not resolve)**; SURF is the canonical host. Found via `ldbcouncil.org/data-sets-surf-repository/`. | | | |
| **SIFT1M** | vector | `http://ann-benchmarks.com/sift-128-euclidean.hdf5` | curl; HDF5 → parquet via h5py+pyarrow | ann-benchmarks (open) | `vector/sift_{base,queries,groundtruth}.parquet` — base **1,000,000** ×128 |
| **GloVe-100** | vector | `http://ann-benchmarks.com/glove-100-angular.hdf5` | curl; HDF5 → parquet | PDDL v1.0 (GloVe) | `vector/glove_{base,queries,groundtruth}.parquet` — base **1,183,514** ×100 |
| **Cohere 1M** | vector/hybrid | `https://assets.zilliz.com/benchmark/cohere_medium_1m/train.parquet` (+ `.../test.parquet`) | curl (public Zilliz CDN) | CC BY-SA 3.0 / Apache-2.0 | `datasets/cohere/train.parquet` — **1,000,000** rows (`id`, `emb` 768-d). *No text/title filter columns — synthesize an attribute for filtered-ANN.* |
| **BEIR** (SciFact / NFCorpus / FiQA) | search | `https://public.ukp.informatik.tu-darmstadt.de/thakur/BEIR/datasets/{scifact,nfcorpus,fiqa}.zip` | curl + unzip (already canonical JSONL) | per-dataset (BEIR toolkit Apache-2.0) | corpus 5,183 / 3,633 / 57,638 · queries 1,109 / 3,237 / 6,648 |
| **Yelp Open Dataset** | hybrid/unified | **https://www.yelp.com/dataset** (license form → "Download JSON") | **manual** download of `Yelp-JSON.zip` (license form), then place under `data/datasets/yelp/` | Yelp Open Dataset License (academic/non-commercial) | `datasets/yelp/Yelp-JSON.zip` (4.35 GB → `Yelp JSON/yelp_dataset.tar`, ~11 GB raw) — unzip+untar at load |

## Acquisition quirks worth remembering
- **ClickBench** must be memory-capped (`SET memory_limit='5GB'; SET temp_directory=...`) or duckdb OOMs the orchestrator on the 10M×105 slice.
- **LDBC** needs `curl -k` (SURF cert chain) and a `zstd` binary on PATH.
- **NYC** and **LDBC** "verified" links from LLM research were dead mirrors; the working canonical hosts are recorded above.
- **Yelp** is the only fully manual, license-gated one — cannot be scripted.
