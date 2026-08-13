# Repo map — the file tree, with what each file does

A newcomer's map of the codebase: every folder and file with a one-line
description. Each source file also carries its own `//!` tutorial header — this map
is the index, those headers are the depth.

Looking for a specific type or function? [repo-outline.md](repo-outline.md) is the
generated companion: every struct/enum/trait/fn with its line number, so you can
jump straight to `file:line`. Regenerate it with `scripts/repo-outline.sh`.

## How a request flows

sekejap is an embedded database: your program calls it in-process, like SQLite.
A query passes through four layers.

```
  "SELECT name FROM users WHERE age > 30"
        │
        ▼   parse + plan
   sql.rs ──────────────►  turns query text into a runnable plan
        │
        ▼   execute
  query.rs ─────────────►  the Set executor runs the plan, step by step
        │
        ▼   ask the engine
   lib.rs ──────────────►  CoreDB: the data + every index
        │
        ▼   read/write bytes
  storage/ + the indexes    mmap · WAL · payloads · text/vector/spatial/graph
```

- **Writes** take the same path, but go through the **WAL** first (so a crash can't
  lose them), then sit in an in-RAM *overlay* that `compact()` folds into the
  memory-mapped *base*.
- **Concurrency** is added on top by `engine/`, which wraps `CoreDB` in a lock. The
  core itself is single-writer.

## The tree

```
sekejap/
│
├── src/                          THE ENGINE — 39 files
│   │
│   ├── lib.rs                    ★ CoreDB: the heart. Holds the data + every index;
│   │                               put/get/compact, WAL, paged base+overlay
│   ├── query.rs                  ★ The query engine: Step (one plan operation),
│   │                               Set (the executor), Hit (a result row)
│   ├── sql.rs                    ★ SGQL: parses query text → a runnable plan
│   │                               (Postgres-style SQL + graph MATCH)
│   ├── scalar.rs                 Scalar functions — per-value helpers SQL calls
│   ├── geo.rs                    Spatial math + location index — distance,
│   │                               containment, nearest (PostGIS-style, metres)
│   ├── serve.rs                  HTTP/JSON surface — routing (sans-IO)
│   ├── pg.rs                     PostgreSQL wire protocol, so PG clients connect
│   │
│   ├── storage/                  BYTES ON DISK — the disk-first foundation
│   │   ├── mod.rs                Index of the storage building blocks
│   │   ├── mmap.rs               ★ MmapView: read a file as if it were a byte
│   │   │                           array. Best-taught file — read this first
│   │   ├── wal.rs                Write-ahead log — how a crash can't lose data
│   │   ├── skbin.rs              SKBIN — compact binary format for payloads
│   │   ├── topology.rs           The graph (nodes+edges) as mmap'able files
│   │   ├── edgestore.rs          Edge storage — the graph's connections
│   │   ├── fieldstore.rs         Btree field index — fast WHERE =/</BETWEEN,
│   │   │                           ORDER BY (served from mmap)
│   │   ├── ginstore.rs           On-disk trigram index — fast ILIKE '%foo%'
│   │   ├── spatialstore.rs       Spatial index on disk (mmap'd location grid)
│   │   └── vecstore.rs           Vector storage — the raw embeddings
│   │
│   ├── bm25/                     TEXT SEARCH — relevance ranking
│   │   ├── mod.rs                BM25 — ranking docs by relevance to a query
│   │   ├── index.rs              Building the index + scoring queries
│   │   ├── dict.rs               Term dictionary — the index's table of contents
│   │   ├── postings.rs           Postings — compressed "which docs have this term"
│   │   └── tokenizer.rs          Turning raw text into searchable terms
│   │
│   ├── search/                   TEXT SEARCH — positional (phrase-aware)
│   │   ├── mod.rs                The SEARCH() index — knows word positions
│   │   ├── index.rs              Words, positions, and ranking
│   │   ├── disk.rs               Persist it once, mmap it back
│   │   └── ranking.rs            Notes on how results are ordered (cascade)
│   │
│   ├── text_index/               TEXT SEARCH — substring (ILIKE)
│   │   ├── mod.rs                Trigram-based ILIKE acceleration
│   │   ├── trigram.rs            Chopping text into 3-character shingles
│   │   ├── gin.rs                GIN trigram index — in-memory builder
│   │   ├── gist.rs               GiST — smaller, lossier alternative to GIN
│   │   └── query.rs              Running an ILIKE query against the index
│   │
│   ├── vector/                   VECTOR SEARCH — nearest embeddings
│   │   ├── mod.rs                Finding the nearest embeddings
│   │   ├── hnsw.rs               ★ HNSW — near neighbours without comparing
│   │   │                           against every vector
│   │   ├── quant.rs              Quantization — f32 → int8 to save RAM
│   │   ├── compact.rs            Compact index — flat arrays, not hash maps
│   │   └── access.rs             One read interface, RAM or disk behind it
│   │
│   └── engine/                   CONCURRENCY & OPS (optional, feature=engine)
│       ├── mod.rs                Engine — the front door: shares one CoreDB
│       │                           across threads; query/execute; snapshot reads
│       ├── guard.rs              The read/write lock (many readers OR one writer)
│       └── buffer.rs             Batching writes so the lock is taken once
│
├── tests/                        Integration tests, one file per area
│                                   (snapshot_reads, crash_recovery, persistence,
│                                    dump_restore, graph_disk, stress, fuzz)
├── benches/                      Benchmarks — mega_benchmark (vs SQLite),
│                                   concurrency (reads under write), vs SurrealDB
├── eval/                         Committed benchmark result history
│
├── wrappers/                     LANGUAGE BINDINGS
│   ├── c/                        The C ABI — unlocks Swift/Kotlin/Go/C++
│   ├── python/  node/  dart/     Per-language bindings, each idiomatic to
│   ├── kotlin/  swift/  go/        its own language (not name-unified)
│   └── csharp/  lua/  react-native/
│
├── skcli/                        The command-line tool: sekejap <db> "<SQL>"
├── docs/
│   ├── usage/                    How to use it (queries, graph, concurrency…)
│   └── developer/                How it's built (this map, design notes)
└── scripts/                      Helpers (benchmark capture, version sync…)
```

★ = the load-bearing files.

## Where to start reading

| goal | path through the code |
|---|---|
| Get the core idea | `storage/mmap.rs` — small, and teaches the memory-mapping trick the whole design rests on |
| Follow a query end-to-end | `sql.rs` (parse) → `query.rs` (execute) → `lib.rs` (CoreDB) |
| Understand durability | `storage/wal.rs` → `storage/topology.rs` → `compact()` in `lib.rs` |
| Understand concurrency | `engine/guard.rs` (the lock) → `engine/mod.rs` (Engine + snapshot reads) |

The biggest files (`lib.rs`, `query.rs`, `sql.rs`, `geo.rs`, `vector/hnsw.rs`) hold
the most behavior — skim their `//!` headers before diving in.
