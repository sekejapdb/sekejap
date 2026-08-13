# Repo map — what every file does and how they connect

A newcomer's map of the `sekejap` codebase: the 44 source files in `src/`, grouped
by the job they do, plus how a query flows through them. Each file also carries its
own `//!` tutorial header at the top — this map is the index; those headers are the
depth. Sizes are a rough "how much is in here" hint.

## How a request flows through the code

sekejap is an embedded database: your program calls it in-process, like SQLite. A
query travels through four layers.

```
   SQL text                     "SELECT name FROM users WHERE age > 30"
      │
      ▼
  ┌─────────────┐   parse + plan
  │  sql.rs     │   turns the text into a runnable plan (SGQL)
  └─────────────┘
      │  a plan = a list of steps (filter, sort, traverse, rank…)
      ▼
  ┌─────────────┐   execute
  │  query.rs   │   the `Set` executor runs the plan; `scalar.rs` does per-value math
  └─────────────┘
      │  asks the engine for nodes, edges, index lookups
      ▼
  ┌─────────────┐   the engine core
  │  lib.rs     │   `CoreDB`: holds the data + every index, does put/get/compact/WAL
  └─────────────┘
      │  reads/writes through…
      ▼
  ┌───────────────────────────────────────────────────────────────┐
  │  storage/  (bytes on disk)      +      the specialized indexes  │
  │  mmap, WAL, payloads, topology         text · vector · spatial  │
  └───────────────────────────────────────────────────────────────┘
```

Two side notes on that flow:
- **Writes** take the same path but go through the **WAL** (write-ahead log) first,
  so a crash can't lose them, then land in an in-RAM *overlay* that `compact()` later
  folds into the memory-mapped *base*. (See `storage/wal.rs`, `storage/topology.rs`.)
- **Concurrency** (many readers + a writer) is added *on top* by the `engine/` layer,
  which wraps `CoreDB` in a lock and adds buffering, scheduling, and S3 sync. The core
  itself is single-threaded.

## The core

| file | ~size | what it is |
|---|---|---|
| `src/lib.rs` | 10460L | **The engine — `CoreDB`.** The heart: the struct that holds the data and every index, plus `put`/`get`/`compact`, the paged base+overlay model, and the registries the query layer reads. Everything else plugs into this. |

## SQL & query execution

| file | ~size | what it is |
|---|---|---|
| `src/sql.rs` | 7745L | **SGQL — text → plan.** Parses query text (Postgres-style SQL with a graph `MATCH` extension) and lowers it into a runnable plan. |
| `src/query.rs` | 8196L | **The query engine.** Home of `Step` (one plan operation — filter, sort, hop, rank…), `Set` (the executor that runs a plan over `CoreDB`), and `Hit` (a result row). This is where a plan actually runs. |
| `src/scalar.rs` | 447L | **Scalar functions.** The per-value helpers SQL expressions call (math, string, date, etc.). |

## Storage — the bytes on disk

| file | ~size | what it is |
|---|---|---|
| `src/storage/mod.rs` | 28L | Index of the storage building blocks. |
| `src/storage/mmap.rs` | 191L | **`MmapView`** — read a file as if it were a byte array (memory-mapping). The primitive the whole disk-first design rests on; the best-taught file to read first. |
| `src/storage/wal.rs` | 1050L | **Write-ahead log.** Every write is recorded here before it's applied, so a crash can't lose committed data. |
| `src/storage/skbin.rs` | 610L | **SKBIN** — the compact binary format each record's payload is stored in. |
| `src/storage/topology.rs` | 1179L | **Topology** — the graph (nodes + edges) written as memory-mappable files (the immutable "base"). |
| `src/storage/edgestore.rs` | 867L | **Edge storage** — the graph's connections (who links to whom), RAM overlay + disk spill. |
| `src/storage/fieldstore.rs` | 507L | **Scalar btree index** — makes `WHERE x = / < / BETWEEN` and `ORDER BY` fast, served from mmap on disk. |
| `src/storage/ginstore.rs` | 192L | **On-disk trigram index** — makes `ILIKE '%foo%'` fast without keeping it all in RAM. |
| `src/storage/spatialstore.rs` | 122L | **Spatial index on disk** — the mmap'd form of the location grid. |
| `src/storage/vecstore.rs` | 480L | **Vector storage** — the raw embeddings (the `f32`/`int8` arrays) behind vector search. |

## Text search

Three cooperating families — relevance ranking, positional search, and substring match.

| file | ~size | what it is |
|---|---|---|
| `src/bm25/mod.rs` | 57L | **BM25** — ranking documents by relevance to a query (the classic search score). |
| `src/bm25/index.rs` | 843L | Building the BM25 index and scoring queries against it. |
| `src/bm25/dict.rs` | 75L | The term dictionary — the index's table of contents (term → where its data lives). |
| `src/bm25/postings.rs` | 185L | Postings lists — the compressed "which documents contain this term" data. |
| `src/bm25/tokenizer.rs` | 273L | Turning raw text into searchable terms (tokenizing). |
| `src/search/mod.rs` | 18L | **Positional full-text `SEARCH()`** — like BM25 but also knows word *positions* (for phrase queries). |
| `src/search/index.rs` | 827L | The positional search index — words, positions, and ranking. |
| `src/search/disk.rs` | 335L | Persisting that index — write it once, memory-map it back. |
| `src/search/ranking.rs` | 14L | Notes on how search results are ordered (cascade ranking). |
| `src/text_index/mod.rs` | 147L | **Trigram `ILIKE` acceleration** — the in-RAM builder side of the trigram index. |
| `src/text_index/trigram.rs` | 288L | Chopping text into 3-character shingles ("trigrams"). |
| `src/text_index/gin.rs` | 455L | GIN trigram index — the in-memory builder. |
| `src/text_index/gist.rs` | 298L | GiST trigram index — a smaller, lossier alternative to GIN. |
| `src/text_index/query.rs` | 351L | Running an `ILIKE` query against the trigram index. |

## Vector search

| file | ~size | what it is |
|---|---|---|
| `src/vector/mod.rs` | 391L | **Vector search** — finding the nearest embeddings to a query vector. |
| `src/vector/hnsw.rs` | 1223L | **HNSW** — the graph algorithm that finds near neighbours without comparing against every vector. |
| `src/vector/quant.rs` | 295L | **Quantization** — shrinking vectors from `f32` to `int8` to cut RAM. |
| `src/vector/compact.rs` | 434L | The compact vector index — flat arrays instead of hash maps (disk-friendly). |
| `src/vector/access.rs` | 66L | `VectorAccess` — one read interface, whether the vectors are in RAM or on disk. |

## Spatial

| file | ~size | what it is |
|---|---|---|
| `src/geo.rs` | 1503L | **Spatial math + location index** — distances, containment, nearest, PostGIS-style (metres, WGS84). The disk form lives in `storage/spatialstore.rs`. |

## Concurrency & operations (the `engine/` wrapper)

Optional layer (`engine` feature) that turns the single-threaded core into a shared,
operable engine. Nothing here changes the core; it wraps it.

| file | ~size | what it is |
|---|---|---|
| `src/engine/mod.rs` | 1129L | **`Engine`** — the friendly front door: shares one `CoreDB` across threads, exposes `query`/`execute`, and (recently) lock-free snapshot reads. |
| `src/engine/guard.rs` | 81L | The read/write lock (`ReadWriteGuard`) — many readers or one writer. |
| `src/engine/buffer.rs` | 102L | Batching writes so the lock is taken once for many (the write buffer). |
| `src/engine/scheduler.rs` | 125L | When to rebuild secondary indexes (the index scheduler). |
| `src/engine/policy.rs` | 71L | When to auto-compact the WAL (the compaction policy). |
| `src/engine/cache.rs` | 500L | Two-tier LRU block cache for payloads fetched from remote (S3) storage. |
| `src/engine/manifest.rs` | 40L | Tracks which segment files exist on remote storage. |
| `src/engine/remote.rs` | 595L | S3 sync — upload/download the database files to/from object storage. |

## Network surfaces

| file | ~size | what it is |
|---|---|---|
| `src/serve.rs` | 227L | The HTTP/JSON surface — routing requests (sans-IO: the logic, not the socket). |
| `src/pg.rs` | 1227L | Speaking the PostgreSQL wire protocol, so Postgres clients can connect (sans-IO). |

## Outside `src/` (brief)

| path | what it is |
|---|---|
| `tests/` | Integration tests — one file per area: `snapshot_reads`, `crash_recovery`, `persistence`, `dump_restore`, `graph_disk`, `s3_*`, `stress`, `sql_fuzz`, … |
| `benches/` | Benchmarks — `mega_benchmark` (vs SQLite), `concurrency` (reads-under-write), `mega_vs_surreal`, and per-subsystem ones. |
| `wrappers/` | Language bindings: `python`, `dart`, `c` (the C ABI that unlocks Swift/Kotlin/Go/C++), `node`, `kotlin`, `go`, `swift`, `csharp`, `lua`, `react-native`. |
| `skcli/` | The command-line tool (`sekejap <db> "<SQL>"`). |
| `docs/` | Documentation — `usage/` (how to use it) and `developer/` (design notes, this map). |
| `eval/` | Benchmark result logs (the committed history of mega/concurrency runs). |
| `scripts/` | Helper scripts (benchmark capture, wrapper version sync, …). |

## Where to start reading

- **The one file to read first:** `src/storage/mmap.rs` — small, and its tutorial
  header teaches the memory-mapping idea the whole disk-first design depends on.
- **To understand a query end-to-end:** `sql.rs` (parse) → `query.rs` (the `Set`
  executor + `Step`) → `lib.rs` (`CoreDB`).
- **To understand persistence:** `storage/wal.rs` (crash safety) →
  `storage/topology.rs` (the mmap base) → `lib.rs`'s `compact()`.
- **The largest files** (`lib.rs`, `query.rs`, `sql.rs`, `geo.rs`, `hnsw.rs`) are the
  ones with the most behavior — skim their `//!` headers before diving in.
