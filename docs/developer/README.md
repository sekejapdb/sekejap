# Developer guide — architecture

How the engine is put together, in four diagrams, plus the repo map and the
reading order for the rest of this guide. Diagrams render directly on GitHub
(Mermaid); module names map to `src/`, so every box is a place you can read
the code.

Three terms used throughout:

- **SGQL** — the query surface: standard SQL everywhere, with standard GQL
  graph patterns inside `MATCH`. One statement can mix both.
- **disk-first** — bulk data (record payloads, full-precision vectors, text
  postings, edge lists) lives on disk; RAM holds only compact metadata and the
  hot index structures needed to find things fast. Memory stays bounded even
  when the dataset is much bigger than RAM.
- **candidate node ids** — every index family (scalar, graph, spatial, vector,
  text) answers the same way: with a set of node ids. That shared currency is
  what lets one query combine several retrieval models with plain boolean
  logic and then rank the survivors with one hybrid score.

---

## 1. Layered architecture (interfaces → query → engine/core → processors → storage)

```mermaid
flowchart TB
  %% ---------- ACCESS SURFACES ----------
  subgraph ACCESS["Access surfaces"]
    direction LR
    ATOMIC["Atomic chainable API<br/><code>db.nodes().collection(x)<br/>.where_eq(..).sort(..).take(n).collect()</code><br/><i>query.rs :: Set builder</i>"]
    SGQL["SGQL — <code>db.query(...)</code><br/>SQL + GQL MATCH<br/><i>sql.rs</i>"]
    HTTP["HTTP / JSON<br/>(sans-IO)<br/><i>serve.rs · feature=serve</i>"]
    PG["Postgres wire v3<br/>(sans-IO)<br/><i>pg.rs · feature=pg</i>"]
  end

  %% ---------- QUERY PIPELINE ----------
  subgraph QUERY["Query processing"]
    direction TB
    PARSE["SQL front-end<br/>tokenize → AST → lowering<br/><i>sql/ (parser, ast, lowering)</i>"]
    STEPS["Plan = <code>Vec&lt;Step&gt;</code> / MatchAggStmt /<br/>ShortestSelectStmt / MultiFrom"]
    EXEC["Step executor + hybrid scoring<br/><i>query.rs :: Set</i>"]
    PARSE --> STEPS --> EXEC
  end

  %% ---------- CONCURRENCY WRAPPER ----------
  subgraph ENG["Engine — concurrency wrapper (feature=engine)"]
    direction LR
    LOCK["RwLock: parallel readers,<br/>brief exclusive writes"]
    BUF["Write buffering<br/><i>engine/buffer.rs</i>"]
    SCHED["Index-build scheduler<br/><i>engine/scheduler.rs</i>"]
  end

  CORE["<b>CoreDB</b> — single-writer core<br/>put / get / link · build_*_index · get_payload<br/><i>lib.rs</i>"]

  %% ---------- PROCESSORS ----------
  subgraph PROC["Multi-model processors (each returns candidate node ids)"]
    direction LR
    SCAL["<b>Scalar</b><br/>btree / hash filters<br/>WhereEq/Gt/Between/In<br/><i>scalar.rs</i>"]
    GRAPH["<b>Graph</b><br/>CSR traversal, BFS shortest<br/>Forward/Backward/Hops<br/><i>storage/edgestore, topology</i>"]
    VEC["<b>Vector / embedding</b><br/>HNSW + int8 rerank<br/>VECTOR_NEAR<br/><i>vector/ (hnsw,quant,compact,access)</i>"]
    FTS["<b>Full-text (BM25)</b><br/>inverted index, ranking<br/>bm25_search<br/><i>bm25/, search/</i>"]
    TRI["<b>Trigram / ILIKE</b><br/>GIN / GiST<br/><i>text_index/</i>"]
    SPAT["<b>Spatial</b><br/>geodesic (PostGIS-compat)<br/>+ occupancy grid<br/><i>geo.rs</i>"]
  end

  %% ---------- STORAGE ----------
  subgraph STORE["Disk-first storage"]
    direction LR
    subgraph RAM["RAM — compact working set"]
      NDMAP["Node map: collection,<br/>payload offset/len, spatial_meta"]
      IDX["Hot indexes: field btree/hash,<br/>HNSW graph + int8 codes,<br/>BM25 dict, spatial grid,<br/>adjacency (offset,count)"]
    end
    subgraph DISK["Disk — bulk"]
      WAL["WAL<br/><i>storage/wal.rs</i>"]
      PAY["Payloads (SKBIN)<br/>payloads.bin<br/><i>storage/skbin.rs</i>"]
      TOPO["CSR adjacency (mmap)<br/>adj_fwd/rev_csr.bin"]
      VF32["f32 vectors<br/><i>storage/vecstore.rs</i>"]
      POST["BM25 postings blob"]
    end
  end

  ATOMIC --> EXEC
  SGQL --> PARSE
  HTTP --> CORE
  PG --> CORE
  EXEC --> ENG
  ENG --> CORE
  CORE --> PROC
  PROC --> STORE
  RAM -. "mmap / pread on demand" .-> DISK
```

---

## 2. Data flow of one hybrid query (candidate generation → combine → score)

How a single SGQL query fans out to processors and fuses their results. Example:
`SELECT ... FROM doc WHERE VECTOR_NEAR(emb, $q, 100) AND ST_DWithin(geo, $p, 5000)`
combined with a BM25 rank and a graph filter.

```mermaid
flowchart LR
  Q["SGQL query<br/>(db.query)"] --> PL["Plan<br/>Vec&lt;Step&gt; / MatchAggStmt"]

  PL --> V["Vector processor<br/>HNSW search (RAM graph+int8)<br/>→ rerank f32 (disk pread)"]
  PL --> T["BM25 processor<br/>dict (RAM) → postings (disk)"]
  PL --> S["Spatial processor<br/>grid cell → geodesic test"]
  PL --> G["Graph processor<br/>CSR hops (mmap)"]
  PL --> F["Scalar filters<br/>btree/hash (RAM)"]

  V --> C{"Candidate node ids<br/>boolean combine<br/>∩ ∪ −"}
  T --> C
  S --> C
  G --> C
  F --> C

  C --> H["Hybrid scoring<br/>RRF / weighted fusion of<br/>text · vector · distance · graph"]
  H --> PAYLD["Fetch payloads for winners<br/>SKBIN pread (payloads.bin)"]
  PAYLD --> R["Ranked rows → Hit[]<br/>(or PG wire / HTTP JSON)"]
```

---

## 3. Disk-first split — what lives in RAM vs on disk

The thesis in one picture: RAM holds only compact metadata + hot index structures;
all bulk data is on disk and read on demand (mmap slices or `pread`).

```mermaid
flowchart TB
  subgraph RAMBOX["RAM — bounded working set"]
    direction TB
    R1["Node map — per node: collection tag,<br/>payload (offset,len), spatial_meta"]
    R2["Field indexes — btree / hash"]
    R3["HNSW graph + int8 quantized codes"]
    R4["BM25 dictionary (terms → postings offset)"]
    R5["Spatial occupancy grid"]
    R6["Adjacency index — node → (offset,count)"]
  end

  subgraph DISKBOX["Disk — bulk, read on demand"]
    direction TB
    D1["payloads.bin — SKBIN records<br/>(1 corrupt byte = 1 record blast radius)"]
    D2["f32 vectors (full precision, rerank)"]
    D3["BM25 postings blob"]
    D4["adj_fwd/rev_csr.bin — CSR edges (mmap, zero-copy)"]
    D5["WAL — durability + replay on open"]
  end

  R1 -. "pread" .-> D1
  R3 -. "rerank pread" .-> D2
  R4 -. "pread" .-> D3
  R6 -. "mmap slice" .-> D4
  RAMBOX -. "compact()/trim_memory() shrink" .-> RAMBOX
  D5 -. "replay rebuilds RAM on open()" .-> RAMBOX
```

---

## 4. Module map (directory → responsibility)

```mermaid
flowchart LR
  subgraph SRC["sekejap/src"]
    direction TB
    L["lib.rs — CoreDB core API"]
    QR["query.rs — Step enum, Set executor, Hit"]
    SQ["sql.rs + sql/ — SGQL front-end (parser, ast, lowering, executor)"]
    SC["scalar.rs — SQL scalar functions"]
    GE["geo.rs — geodesic spatial (PostGIS-compatible)"]

    subgraph M_ENG["engine/ (feature=engine)"]
      E1["mod, buffer, scheduler"]
      E2["guard, policy"]
    end
    subgraph M_VEC["vector/"]
      V1["hnsw, quant (int8 SQ)"]
      V2["compact (CSR disk graph), access"]
    end
    subgraph M_BM["bm25/ + search/"]
      B1["tokenizer, dict, postings, index"]
      B2["search: index, disk, ranking"]
    end
    subgraph M_TXT["text_index/"]
      T1["trigram, gin, gist, query"]
    end
    subgraph M_STO["storage/"]
      S1["wal, skbin (payloads)"]
      S2["edgestore, topology (graph)"]
      S3["vecstore, mmap"]
    end
    subgraph M_NET["network adapters (sans-IO)"]
      N1["serve.rs — HTTP/JSON (feature=serve)"]
      N2["pg.rs — Postgres wire v3 (feature=pg)"]
    end
  end
```

---

### Reading the diagrams

- **Many surfaces, one core.** The atomic API and SGQL run in-process;
  `serve` (HTTP) and `pg` (Postgres wire) are thin transports with no sockets
  of their own — the caller owns all I/O — and they call the same library
  service. However you reach it, there is one engine underneath.
- **The engine layer is optional.** `CoreDB` alone is a complete single-writer
  embedded database; the `engine` module only adds concurrency, write
  buffering, and caching on top. If you do not need those, you do not pay for
  them.
- **Composition is the point.** Each processor returns candidate node ids;
  boolean set logic combines them; hybrid scoring ranks what survives. That is
  how one statement can filter by scalar fields, walk the graph, match text,
  and rank by vector similarity at the same time.
- **The RAM/disk split (diagram 3) is the core promise.** The dashed arrows
  (positional reads, mmap slices) are how bulk data stays on disk until the
  moment it is needed.
- Feature-gated modules — `engine`, `serve`, `pg` — are opt-in: the
  minimal build is just the embedded core.

---

## Repo map

For a **per-file** index — every one of the 44 `src/` files with a one-line purpose
and how they connect — see [repo-map.md](repo-map.md). The tree below is the
directory-level summary.

```
sekejap/
├── src/
│   ├── lib.rs            CoreDB — the embedded core (open/put/get/link,
│   │                     index builds, compaction, recovery)
│   ├── query.rs          Step plan + Set executor + hybrid scoring
│   ├── sql.rs, sql/      SGQL front-end: tokenize → AST → lowering
│   ├── scalar.rs         SQL scalar functions
│   ├── geo.rs            geodesic spatial (PostGIS-compatible, metres)
│   ├── vector/           HNSW, int8 quantization, compact disk index
│   ├── bm25/             tokenizer, dictionary, postings, index
│   ├── search/           positional search + ranking
│   ├── text_index/       trigram GIN for ILIKE
│   ├── storage/          WAL, SKBIN payloads, topology (CSR), vector
│   │                     stores, mmap field indexes
│   ├── engine/           optional concurrency wrapper (feature=engine)
│   ├── serve.rs          HTTP adapter, sans-IO (feature=serve)
│   └── pg.rs             Postgres-wire adapter, sans-IO (feature=pg)
├── skcli/                the command-line tool (query runner, serve, pg)
├── wrappers/             python · node · dart · kotlin · swift · go · c
├── benches/              criterion benchmarks (see mega_benchmark.rs)
├── eval/                 comparative benchmark harnesses + results
├── tests/                integration tests
└── docs/                 this documentation
```

## Build and test

```sh
cargo build                    # core library
cargo build --all-features     # + engine, serve, pg
cargo test                     # full suite (unit + integration)
cargo bench --bench mega_benchmark   # 20-scenario local benchmark vs SQLite
```

## Where to read next

- [repo-map.md](repo-map.md) — the per-file index: what each of the 44 `src/` files
  does and how a query flows through them (start here to get oriented)
- [repo-outline.md](repo-outline.md) — **generated**: every type and function with
  its line number, so you can jump straight to `file:line`. Regenerate with
  `scripts/repo-outline.sh` (the pre-commit hook keeps it fresh)
- [core.md](core.md) — the write path, durability, recovery, compaction
- [storage.md](storage.md) — what's on disk and why it looks that way
- [indexes.md](indexes.md) — the six index families and their disk-first designs
- [queries.md](queries.md) — how a query executes
- [invariants.md](invariants.md) — **read before changing anything**: the
  checklists that keep startup, memory, and query speed intact
- [roadmap.md](roadmap.md) — where the engine and wrappers are going next
- [notes/](notes/) — design history and rationale (archive)
- [../../eval/](../../eval/README.md) — the comparative benchmark suite:
  harnesses, datasets, and measured results per category
