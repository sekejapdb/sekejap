# sekejap-e3

## North Star — the 7 laws

1. **Disk-first** — no operation holds RAM ∝ database. RAM ∝ change is fine.
2. **Cost ∝ change, not size** — N units of change costs O(N), not O(database). Latency must not grow with the store.
3. **Nothing fallible may delete** — write new, verify independently, *then* drop old.
4. **Name your sacrifice** — no stated cost = not analysed = rejected.
5. **No corruption unrecoverable** — checksum every unit read, damage can't propagate, bounds-check everything off disk, recovery can't need the damaged thing. State the blast radius of one bad byte.
6. **A write never blocks or degrades a read** — snapshot readers serve the newest published generation, byte-stable for their whole life, beside a live writer, with zero coordination. (Stated in full below; it was declared in phase 2f and never listed here.)
7. **Ingest must be usable on the target device** — the four named cases are
   **BULK IMPORT** (a large batch into fresh namespaces), **LATE INDEXING**
   (`CREATE INDEX` over existing unindexed data), **LIVE WRITES** (incremental
   writes with indexes already live), and **REOPEN** (opening a published
   database). Getting data IN is a first-class cost, not a setup detail.
   Building an index may be proportional to the corpus, but the constant
   matters: a person opening their data on a phone waits, and a load they will
   not sit through is a failure however correct it is. State the per-row cost,
   and state it against the device class this is for — not against a server.

## Architectural decisions

Every decision, its choice, and the measurement that earned it. A decision
without a number beside it is an opinion; these all have numbers.

STORAGE
- **D1 one file, one pager, one btree, row in the leaf** — SQLite's skeleton
  (everything incl. schema = btrees in one paged file). e1 touched 8 structures
  per write and ran 26x slower than SQLite; e3 beats SQLite on every arm.
- **D2 page size 4096** — ablated 4K/8K/16K: 4K best scattered-insert time and
  read granularity for seek+short-range (RCA's shape).
- **D3 keys big-endian, tag byte first** — byte order = numeric order, so every
  "all X of Y" is one contiguous range scan; tags never interleave.
- **D4 extensibility = new key tags, never new files/structures** — the
  anti-e1 rule. Vector/fulltext/spatial are keyspaces, not sidecars.
- **D5 overflow chains for values > ~4KB** — vlen sentinel 0xFFFF, marker
  [total|head|whole-value crc], chain = ordinary checksummed pages. One
  primitive for embeddings (1536-dim = 6KB) and big RCA payloads; per-feature
  chunking/quantization-as-limitation rejected (the e1 rot pattern). Blast
  radius: one record, tested (49/50 survive a damaged chain).

WRITE PATH
- **D6 WAL buffered 256 KiB, flush at commit in EVERY sync mode** — was one
  pwrite per record (59.4% of wall time, 2.1M syscalls/1M rows). Off promises
  no barrier, not that records stay in process RAM.
- **D7 WAL frame [klen u16|key|value-to-end]** — the old u16 vlen silently
  truncated values >64KB entering the log.
- **D8 checksums at the I/O boundary only** — verify at pool load, seal at
  page write. Was 12.34 full-page CRCs per row (34% of write path) -> 0.12.
  Where SQLite (cksumvfs) and DuckDB (block manager) put theirs.
- **D9 rightmost-append split (balance_quick)** — a 50/50 split of an
  ascending run strands every left page half-full (0.458 utilisation, file 2x
  needed size). Guard = append at rightmost leaf only; scattered keeps
  balanced splits (unguarded variants regressed amplification, caught by the
  pinned test).
- **D10 blind writes; no read-before-write** — e1 paid a full descent per
  insert. The one read-before-write op is relabel, priced separately.
- **D11 bulk load = external sort -> pack pages once -> skip WAL -> swap root**
  — DuckDB's dock. 1.72 vs 8.49 us/node on identical scattered input; each
  byte written once. It REPLACES the tree (load/rebuild, never append).
- **D12 steal, no undo; commit is the only atomicity boundary** — undo needs
  read-before-write (136us/read under direct I/O), no-steal caps transactions
  at pool size (kills D11), shadow paging adds a page copy per write (deepens
  the open amplification problem). Cost stated: an uncommitted crash can leave
  torn multi-key writes; committed data is exact (tested).

GRAPH
- **D13 dense sequential u64 ids from the allocator; uuid/slug via extkey,
  resolved once at query entry** — the id policy IS the clustering policy:
  sequential vs hashed ids = 4.2 vs 90.3 reads per trace query (21.5x). e1
  hashed slugs and could never be fast. Allocator rides the commit (crash-safe,
  never reuses an id -- tested).
- **D14 edge = (src, ty, dst) key; ty BETWEEN src and dst** — a typed hop is a
  narrower range, not a filter. Adjacency is physically contiguous: 1.1 page
  reads/hop flat across store sizes.
- **D15 reverse edges = mirrored keyspace, optional** — in_edges becomes a
  seek and remove_node finite; costs +57% edge storage (measured), so it is a
  create-time choice.
- **D16 perspectives (ctx) = SEPARATE wider tags (0x08/0x09); base graph keeps
  25B keys** — flat ctx-in-every-key grew a 5M file by 640MB and doubled 3-hop
  latency via cache pressure alone (machinery counters unchanged). ctx=0 pays
  NOTHING; a perspective's whole KG is one range; multigraph dissolves (two
  sources asserting one triple = distinct keys).

READ PATH
- **D17 adaptive scan: first 8 entries per-entry, then whole-leaf batches** —
  per-entry leaf reopen was O(entries^2)/leaf (7x behind SQLite on ranges);
  whole-leaf batching then cost graph hops 2-3x (a 3-edge hop copied a
  130-entry leaf). The threshold keeps hops at 0.002ms AND ranges fast.
- **D18 zero-alloc fold (for_each_ref) for counting/filtering scans** — two
  Vec allocs per row was the last 1.9x vs SQLite; fold = 5.5ns/key; e3 now
  wins every mini-mega query.
- **D19 property indexes = 0x0A keyspace with order-preserving 8-byte
  encodings (i64 sign-flip, f64 bit-trick, desc variants)** — equality, range
  and top-k are all range scans; caller writes index entries (blind), update
  supplies the old value.
- **D21 platform code lives in `FileIo` (io.rs) ONLY; durability primitives
  are per-OS by design, never by accident** — SQLite's VFS posture. Sync is a
  ladder each OS lies about differently (Linux fdatasync / macOS fsync +
  F_FULLFSYNC / Windows FlushFileBuffers); direct I/O = O_DIRECT / F_NOCACHE /
  FILE_FLAG_NO_BUFFERING. Everything above the trait is pure byte-offset Rust.
  Windows/Android/ARM-Linux are COMPILE-VERIFIED (cargo check per target,
  bench/check_targets.sh -- every phase leaves them green) and enforced by
  kernel/tests/platform_invariant.rs, which fails the build if platform code
  appears outside io.rs. Stated non-claim: durability on Windows/Android is
  UNVERIFIED until the suite runs on real hardware; their sync collapses to
  the strong barrier (FlushFileBuffers / sync_all) -- conservative, stated.
  Windows dir-sync is a documented no-op (NTFS journals metadata; SQLite's
  os_win.c fsyncs no directories either). FILE_FLAG_NO_BUFFERING is the
  direct-I/O upgrade path; basic posture degrades to Buffered and reports.
- **D20 no global ANN in core** — vectors serve candidate RESCORING (point
  reads by id). A resident HNSW is RAM ∝ store (e1/app wound, Law 1).
  Ceiling for a change-buffer-style alternative measured 1.83x buffered.
- **D22 vectors are catalog-fixed-dim f32-LE rows in 0x05** — one dim per
  store (CAT_VEC_DIM, set by the first set_vec), mismatch refused before
  anything reaches the WAL. f32 exact: quantization (i8/f16) is an ANN-phase
  sacrifice, not a storage default. 1536-dim = 6KB = a 2-page overflow chain,
  so ingest rides the ordinary put/bulk path and Laws 1-3 hold with zero
  vector-specific write code.
- **D23 distance runs IN the engine, on borrowed bytes** — L2/cosine/dot,
  lower-is-better (dot/cos negated), lane-decoded straight off the stored
  record; rescore is a bounded top-k heap (k entries live, never the
  candidate set). Oracle tests use INDEPENDENT inline math — an oracle that
  called the engine's own kernel let a cosine-normalisation mutation survive.
- **D24 one pool, one budget, default 64 MiB** — the CLOCK pool is THE cache
  for every family (graph/props/vector/fulltext/spatial are all key tags in
  one btree, so there is only one cache to have; SQLite's model). Configurable
  per open (`Config::default()` = 64 MiB / Buffered / Normal); no index
  family may own private resident state. Named sacrifice: families compete in
  one pool; mitigations = single-use payload reads bypass the pool (2e A1),
  and pinning-by-keyspace is a future knob INSIDE the pool, never a second
  structure.
- **D25 weighted training: constrained-first benchmarking** — the engine's
  power must emerge at the DEFAULT budget or smaller. Every gate, ablation
  and O-factor ladder runs disk-based (64 MiB or less; app gate: 8 MiB);
  an optimization that only wins when cached is rejected. Read counts (DIAG)
  are the judge, wall time the witness. RAM is the super-charge, reported
  as a separate bonus row (default-cache vs big-cache), never as the
  primary claim.
- **D26 concurrency = single writer + shadow-paged snapshot readers; MVCC
  rejected by audience derivation** — writes go through the WAL as always;
  at checkpoint, dirty pages flush to FRESH page numbers (the shadowed
  descent relocates published pages top-down, parent in hand, so no reverse
  map exists) and a dual-slot meta flip (generation g in slot g%2, page
  checksum per slot -- LMDB's dual meta plus the verification it omits)
  publishes the root. Readers = read-only opens pinned at a published
  generation: no locks, no lookaside, no WAL replay; every mutation refuses.
  The monotone never-reuse allocator (already recovery's invariant) is what
  makes published roots immutable; pool.get_mut asserts it. Scans advance
  by parent path, not sibling chain (a shadowed leaf strands its left
  neighbour's pointer; root re-descent per leaf cost 2x on ranges, rejected).
  Checkpoint = TWO barriers: data durable, then flip durable, then rotate.
  Named sacrifices: reader staleness = one checkpoint cadence; file grows
  until compact folds dead versions; CoW locality drift in hot-update
  regions until compaction. MVCC (DuckDB/Postgres) rejected: per-row
  visibility on every analytical scan + version installs on every write,
  paid ALWAYS, to serve contended multi-session OLTP -- the one audience
  sekejap does not target (RCA, hybrid, embodied, mobile/gaming, data
  science, read-heavy backend all land on single-writer). Group commit is
  the future multi-session answer; snapshot machinery is its prerequisite.
- **D27 vector similarity = data-oblivious fingerprints, scan tier first** —
  every set_vec (and the bulk dock, same sorted pass) writes a second row:
  0x0B | id -> [norm f32][4-bit codes], from a recipe derived ONLY from
  (dim, seed) in the catalog: one seeded sign-flip + fast Walsh-Hadamard
  round (power-of-two padded), per-coordinate uniform quantizer sized for
  the rotated Gaussian. NO training, NO build step, NO stale tail, no
  retrain on drift -- the index is always current (the property none of the
  dug engines ship end-to-end). nearest(q,k,metric,oversample): scan codes
  with a per-query LUT + bounded heap, then exact rescore (D23) -- approx
  can MISS, never misrank. Recall@10 = 1.000 on bench data at 2-bit/os16 (2g.2; test floor 0.90 held
  on gaussian, hostile-sparse, and all four metrics);
  identity-rotation mutation fails the sparse case at 0.555. Sacrifices
  (L4): +17% file; ingest 1.9-2.7x vs 2e (encode math, ~21us/vector
  scalar -- still 5.2-5.7x FASTER than SQLite ingesting with no index at
  all); scan tier is O(N)/query by design. Named next steps: SIMD kernels,
  and the navigation-graph tier (FACT-02) when O(N) stops being enough.

- **D28 the O-probe: exponents in minutes, big ladders once per tag** — an
  exponent needs RATIOS between doubling points, not a million rows: four
  sizes 10K->80K give every family's worst-segment k in ~4 minutes
  (bench/oprobe.py), and I/O counters stay the judge (size-independent,
  noise-immune). Probes gate every iteration; the full 100K->1M capped
  ladders run ONCE per phase tag for the record. Probes are regression
  detectors, not publishable numbers -- small-N k carries fixed-cost and
  cache-boundary artifacts (a probe k=1.89 on vec-nearest correctly
  flagged the pool-boundary crossing that big ladders confirm is linear
  past it). Benches run ONLY on the cluster, memory-capped (500Mi/2cpu,
  the memtest shape); an unchunked bench arm once demanded 15GB and froze
  the dev machine -- a benchmark obeys the same memory discipline as the
  engines it measures.

- **D29 cache shapes: every index family declares point / scan / blob** —
  the one pool (D24) serves three access shapes and must not let them
  poison each other. POINT-shaped reads (btree descents, graph hops, term
  lookups, prefix unions) use normal CLOCK and stay hot. SCAN-shaped reads
  (vector code sweeps, fulltext folds, spatial cell sweeps, analytical
  ranges) must be scan-RESISTANT: loaded without the referenced bit, so
  CLOCK reclaims them first and a 512MB sweep cannot evict the hot set —
  Postgres's seq-scan ring buffer, expressed as one flag on the pool
  (measured need: a 1M-vector nearest currently streams 8x the pool
  through it per query). BLOB-shaped reads (overflow chains: big payloads,
  vectors, polygon rings) bypass the pool entirely via coalesced uncached
  preads (shipped, 2e A1). Every FUTURE family names its shape per
  keyspace at design time; hot-set size per million rows goes in its
  phase verdict.

- **D30 full-text lives as FACT-03 said, plus three earned decisions** —
  (a) the HEAD is row-per-(term,doc): appending to packed values is
  read-modify-write (the e1 BM25 wound), rows are blind and searchable the
  instant they land; folds pack rows into value-per-term segments.
  (b) every posting CARRIES (tf, doc_len): BM25 does zero length lookups
  -- the 1M term query paid 200K cold point-gets (10.8s) before this.
  (c) `Store::delete_prefix`: one WAL record, parent-path walk, whole-match
  leaves cleared in one page write -- the fold's erase dropped from 8M row
  deletes (21 min) to O(leaves) (148s total fold at 1M). Key lesson worth
  its own line: a key SEPARATOR must sort below every payload byte -- 0xFF
  sorted "handle" before "hand" and the fuzzy walk skipped real terms;
  the separator is 0x00. Named gaps: ingest+fold 3.5x FTS5 at 1M; the
  instant tier scores its full expansion (top-k pruning is the future
  fix, it loses to FTS5 2.5x on huge prefixes).

- **D31 spatial = Hilbert cell keys, PostGIS-subset semantics (FACT-04
  held)** — typed Geom only in the kernel (binary codec, bounds-checked;
  GeoJSON stays above the SQL line). Postings (field, level, hilbert, id)
  -> outward-rounded f32 bbox; fine level 12 bits (~2.4km cells; 16-bit
  600m cells shattered a 10km radius into 50+ cold leaf reads — 72.7 ->
  7.4ms at 1M when re-celled, the scan itself was 0.6ms all along),
  coarse 8 bits, ONE world-bucket posting for continent-scale shapes
  (the corner-clip shortcut missed interior queries — oracle-caught).
  Exact tier: e1's PostGIS-parity math (Vincenty/authalic, live-PostGIS
  fixtures to 1e-6); haversine decides outside a 0.6% band, Vincenty only
  the boundary. Point fast path: degenerate bbox = the point — zero
  payload reads. set_geo refuses out-of-range coords BEFORE the WAL
  (Vincenty fed lat=145 returned 0.0 — an invalid write could have
  matched everywhere; axis-order oracle added after a lat/lon swap
  SURVIVED the regional oracle). Named deviations: vertex-min distance
  for non-points, planar containment, SRID 4326 only, GeometryCollection
  out. Named loss: 1M radius 2.2x behind SQLite rtree (cold scattered
  leaves; every other cell won).

- **D32 vector navigation = Vamana over btree rows (FACT-02's graph
  stage, phase 2k)** — nav row (0x11, id) -> [norm][2-bit code][n][u64
  neighbors]: topology and ranking data in ONE read per visited node
  (DiskANN sector layout, diskann-bftree posture). R=32, build beam
  L=64, alpha=1.2 applied SQUARED (we compare squared L2; single alpha
  quietly weakens the spread rule to sqrt(alpha) — recall 0.535 measured
  before the fix). Backlink lists run to 2R and prune to R (per-overflow
  pruning re-read ~33 full vectors per neighbour per insert: fold 37 ->
  7-14ms/vec). Estimates DISCOVER candidates; exact f32 distances PRICE
  and PRUNE them (per-insert vector cache). Fold wires ids above a
  watermark in id order (cost ∝ new vectors, Law 2); queries walk the
  graph below the watermark and scan the head above it, one exact
  rescore over the union — freshness never waits for the fold. Medoid +
  watermark are catalog rows. Deletes leave dangling links walks skip
  (missing row = dead). Named sacrifices: (1) recall is data-shaped —
  on gradient-free uniform noise the walk degrades (0.81 @ 1M-dense
  ef=1280) and the exact scan tier IS the fallback; (2) fold is
  single-threaded ~7-14ms/vector (parallel build is future work); (3)
  the fold commits once, so WAL ∝ fold batch (15GB at 1M — periodic
  commit is a known TODO); (4) estimate-guided construction concedes
  peak recall on adversarial data to exact-built graphs (Qdrant 0.985
  uniform vs our 0.81) — the price of zero training and 2-bit rows.


## The fact (staked; falsifier: traversal reads ∝ store instead of degree)

**One file, one pager, one btree, row in the leaf. A graph is a key
discipline, not a structure. Bulk load sorts and packs each page once,
skipping the WAL.** From SQLite's skeleton (everything incl. schema = btrees in
one paged file, payload in the leaf cell) + DuckDB's dock (bulk appends bypass
the WAL). Extensibility = new key tags, never new files.

## FACT-02 — vectors at scale (staked 2026-08-25 for the ANN phase; falsifier:
## an ANN needing a training pass over user data, or whose insert/fold cost or
## resident set scales with the store)

**ANN is also a key discipline, not a structure — quantized codes are rows
too.** Dug from pgvector, DiskANN (incl. diskann-bftree), Qdrant, Lance,
TurboQuant/turbovec (clones in reference/, gitignored):

- Quantize data-obliviously (TurboQuant, arXiv 2504.19874): codebook is
  UNIVERSAL, computed from (dim, bits) + a seeded rotation, never from data.
  Encoding is per-vector, write-blind, zero training — the retrain/drift
  machinery PQ forces on Lance/DiskANN does not exist. Recall beats PQ at
  equal bits; codes SIMD-scan at 3.4x FAISS FastScan (turbovec kernels).
- Candidate generation = scanning code rows; e3's existing rescore (D23) is
  already the exact tier. A navigation graph, when scan stops being enough,
  co-locates {code | fp vector | neighbor ids} in ONE fixed row per node
  (DiskANN sector layout; Qdrant CompressedWithVectors; diskann-bftree =
  Vamana over a paged btree) so a hop is one page read and the exact rescore
  rides FREE in bytes already paged in.
- Writes: new vectors land in a mutable tail served by flat scan until a fold
  builds/extends the immutable tier — Lance quantizes new rows against the
  EXISTING model; Qdrant rebuilds reusing the old graph and healing only
  around deletions (cost ∝ change). Same leveled-segment fold shape as
  BM25/search. pgvector's alternative (in-place graph edits in WAL'd pages,
  ~1+m page writes per insert) is the no-fold fallback, proven in production.
- Residency is a PLACEMENT choice over one layout, never a rebuild (Qdrant
  Cold/Cached/Pinned; turbovec's disk block units are byte-identical to the
  search layout). RAM floor = whatever code tier is pinned; everything else
  pages. Named sacrifice if codes are pinned: ~dim*bits/8 B/vector resident
  (1536-dim @4b = 768B) — cheap, but it IS ∝ store, so pinning must stay a
  knob, not a default.

## FACT-03 — full-text (staked 2026-08-25; falsifier: any structure that must
## be rebuilt wholesale per batch, or resident state ∝ documents)

**The term dictionary IS the btree; postings are values; segments are key
ranges.** Dug from Meilisearch/milli (LMDB = our shape), tantivy, typesense
(clones in reference/):

- Keys `(field, seg, term)` — the sorted keyspace is prefix search (milli
  itself serves prefixes by btree range scan when its prefix cache misses)
  AND a virtual trie: typo tolerance = bounded OSA/Levenshtein DP row
  walked by SEEKING over sorted term keys (typesense's trie-DP algorithm
  minus the resident ART; caps: 0/1/2 typos by token length, ≤10 combos).
  NO monolithic words-FST value (milli's is merge-rewritten every batch —
  Law 2 inverted on a 4KB pager).
- Postings per (field, term): roaring-style bitmap values with milli's
  ≤7-id raw fast path for filtering; tantivy's 128-doc bitpacked blocks
  (≤512B, page-friendly) + 2B/block block-max (fieldnorm, max_tf) where
  BM25/WAND ranking is needed; 1B/doc log fieldnorms sidecar. PER-FIELD
  keys are non-negotiable: hybrid weighting (title*0.4 + lyric*0.5) needs
  per-field BM25 with per-field length stats.
- Instant tier for search-as-you-type: precomputed prefix-union values for
  short hot prefixes only (milli: ≤4 chars, ≥100 hits) with DELTA
  recomputation — only prefixes whose words changed are rebuilt (cost ∝
  change, verbatim); ranking for this tier = typesense's bit-packed u64
  rank word (words/typos/proximity/exactness/offset) — zero index cost,
  pure query-time arithmetic.
- Segments: immutable key-range namespaces per (field, seg); flush =
  sorted bulk write of a fresh seg prefix; fold = k-way merge to seg N+1,
  metadata flip, THEN delete old prefixes (Law 3); tantivy's LogMerge
  constants (level 0.75, ≥8 segments, 10K floor) transplant verbatim.
  Deletes: ONE alive-bitmap value per segment, folded away at merge —
  never per-posting tombstone keys.
- From Manticore (the Sphinx successor, the mature disk school): the
  PAYLOAD-EXPANSION mechanism — a wildcard/typo expansion never
  materializes N query terms; it collects the N matched terms' posting
  extents from the dictionary scan, sorts them by physical offset, streams
  them as ONE union pseudo-term, and clips the low-frequency tail
  (expansion_limit: rare matches are "likely misspellings"). In our btree:
  one range scan collects matching term keys, postings then read in key
  order — near-zero structure, caps the worst-case fuzzy/prefix cost.
  Also adopted: skiplists only above a posting-length threshold (short
  postings pay zero skip structure). Also learned: immediacy needs no
  RAM-segment LSM (Manticore's 32-segment commit design blocks on its
  merger and fans every query across all segments) — our mutable head
  segment lives IN the btree, searchable the instant it is written,
  folded by policy. Refused from Manticore: attributes resident in RAM
  (MMAP_PREREAD — Law 1 inverted; their inherited Sphinx sacrifice).
- Named rejections: pair-proximity keyspace (milli's own write-amp and
  space monster — proximity comes from positions when asked); typesense's
  architecture entirely (all-RAM, 165MB per 1M short docs, O(store)
  startup rebuild — the exact sacrifice our laws forbid; its UX targets
  transplant, its structures do not).

## FACT-04 — spatial (staked 2026-08-25; falsifier: discretionary placement —
## any structure where WHERE a row lives is a decision, not a key)

**Space is a key discipline too: Hilbert-curve cell keys in the btree; the
R-tree is refused, on PostGIS's own evidence.** Dug from PostGIS, SQLite
rtree, e1's grid:

- PostGIS's fast index build sorts geometries by a HILBERT CURVE of the
  bbox centroid (their own sortable hash) — the R-tree camp's best build
  order is curve order. SQLite's R-tree is already rows-in-btree-tables
  (nodeno -> blob), proving D4 could host one — but its ~700 lines of
  penalty/picksplit exist only because R-tree placement is discretionary;
  curve keys delete the whole problem class and its churn-degradation.
- Keys: (field, cell) with Hilbert-ordered cells at a small fixed set of
  levels; large/skinny geometries post to multiple cells (the R-tree's one
  genuine edge, recovered). Values carry PostGIS's 16-byte
  outward-rounded f32 bbox — box filtering never touches the geometry
  payload (their recheck=false trick); exact math only on survivors.
- Exact tier: e1's geo.rs ports wholesale (WGS84 Vincenty + authalic area,
  PostGIS-geography metres, equal to PostGIS to float precision — already
  battle-tested). PIP: bbox filter, then rings read from payload rows via
  UNCACHED chain reads; per-polygon interval tree built per QUERY, never
  resident (e1 cached parsed rings resident — RAM ∝ polygons — refused).
- kNN: best-first expanding cell rings, exact Vincenty rescore — same
  two-tier shape as vectors (cheap tier narrows, exact tier answers).
- e1's grid already converged here the hard way: its packed cell file
  folded at O(N^1.95) and was retrofitted onto btree records + overlay;
  e3 starts where e1 arrived.

## The hybrid scoring contract (e1 semantics, target for 3-sql)

e1's `ScoreExpr` is the pipeline's last element and e3 adopts it as-is: an
arithmetic expression tree (+,-,*,/) over per-candidate atoms — BM25(field,
q), BM25_NORM (saturation s/(s+k), bounded [0,1] to blend with cosine),
SEARCH_SCORE, VECTOR_{COSINE,L2,DOT,L1}, ST_DISTANCE (PostGIS-geography
metres), payload fields, literals — evaluated per candidate in ORDER BY:
`BM25(title,q)*0.4 + BM25(lyric,q)*0.5 + VECTOR_COSINE(emb,v)*0.3`. The
structural obligation on every family: expose score(id, query) as a cheap
point evaluation once candidates exist, and per-FIELD statistics where the
atom is per-field. Candidates may come from any family; scoring never
re-runs retrieval. GROUP BY: e1 already ships PG-semantics aggregation
(COUNT/SUM/AVG/MIN/MAX, HAVING, the PG "must appear in GROUP BY" error,
PATH_* aggregates) — it PORTS in 3-sql; e3's implementation preference:
stream aggregation over index order when the group key is a key prefix
(RAM O(1)), hash-grouping (RAM ∝ groups, named) otherwise.

## The demo track (iot-*, outside the phase ladder)

The paper demo (a sub-1B LLM assistant on Pi-5-class hardware) advances
on its own track in the research workspace, never gating engine phases.
iot-1 (2026-08-26): a TEMPORARY pyo3 binding over this kernel -- kept
OUT of this repo because e3 inherits the public repo structure
(wrappers/, Actions) from e1 at replacement time. Gate passed in the
linux/arm64 Pi-5 sim (4cpu/2GB): all five demo tools through the real
engine; plus a zero-training YOLO11n vision probe (7.5 fps CPU) writing
sightings into the store and recalling them by text query.

## Strategy

Goal: RCA → hybrid query → shared-node multi-perspective KG. My needs first.
- Phase 1 ✓ base: pager, WAL, btree, bulk. Gate: 50K→5M vs SQLite.
- Phase 2 ✓ graph layer. Gate: 4 arms below.
- Phase 2b: ctx migration, property indexes, vector, fulltext, spatial — each a
  keyspace, each gated.
- Phase 3: port e1's SGQL on top. Non-goals until then: global ANN, wrappers,
  server, SQL.
Loop discipline: ≤5 ablations per phase, one variable each; every test proven
to fail on a planted bug before trusted; stale-binary check before any A/B.

## Graph contract

One btree; big-endian so byte order = numeric order; the key layout IS the
query plan.

    tag   key                          value              role
    0x00  catalog(field)               engine state       id allocator, options
    0x01  id(8)                        label(8)++props    node, row-in-leaf
    0x02  label(8) │ id(8)             ∅                  membership range
    0x03  src │ ty │ dst               edge props         BASE graph (ctx=0), 25B
    0x04  dst │ ty │ src               ∅                  base mirror
    0x08  ctx │ src │ ty │ dst         edge props         perspective edges, 33B
    0x09  ctx │ dst │ ty │ src         ∅                  perspective mirror
    0x05  id(8)                        embedding          vectors (2b)
    0x0A  prop │ value │ id             ∅                  property index, 25B
    0x06  hash(ext)(8)                 id(8)++ext         uuid/slug→id, once
    0x07  src │ ty │ dst │ ctx         ∅                  cross-ctx (if ever hot)

Node — `Node "venue-7" (cafe) {suburb, rating, price}`:

    0x01│id=1            → cafe_hash ++ opaque props (caller's codec)
    0x02│cafe│id=1       → ∅
    0x06│h("venue-7")    → id=1 ++ "venue-7"     (ext stored: collisions detected)

- id = dense sequential u64 from the allocator. **The id policy is the
  clustering policy** (measured 21.5x on trace queries vs hashed ids).
- label is indexed via 0x02; any OTHER property the caller chooses to index
  goes through 0x0A with an order-preserving 8-byte encoding (enc_i64/enc_f64
  and desc variants), so equality/range/top-k are all range scans. The caller
  writes index entries (set_prop / update_prop-with-old-value: blind writes;
  a wrong old value leaves a stale entry -- Law 4). Unindexed props stay
  opaque bytes.
- Nodes are SHARED across contexts; perspectives disagree about edges only.

Edge — `lect55 asserts: ml ──part_of──▶ data_learning {w:0.9}`:

    0x03│lect55│ml│part_of│data_learning → {w:0.9}
    0x04│lect55│data_learning│part_of│ml → ∅
    0x03│lect67│ml│part_of│data_learning → {w:0.4}     ← other perspective

- ctx = perspective / named graph; **ctx=0 = base graph and routes to the
  25-byte 0x03/0x04 space — it pays NOTHING for the feature**. Measured: a flat
  +8B/key grew a 5M-node file 640 MB and doubled 3-hop latency via OS-cache
  pressure alone; split tags returned it exactly to pre-ctx numbers.
- Edge identity = full key; re-assert overwrites (set semantics per ctx).
  Two ctxs never collide → multigraph dissolved, no edge-id needed.
- ty between src and dst: a typed hop is a narrower range, not a filter.
- Props on the forward key; mirror is a marker.
- STATUS: ctx is code. Isolation, set-semantics and perspective-range are
  tested (mutation-checked); base-graph gates re-run and identical.

Query costs (each ∝ answer/neighbourhood, never ∝ store — Law 2 for reads):

    hop            0x03│ctx│src│ty…   1 seek + degree
    reverse hop    0x04│ctx│dst…      1 seek + in-degree
    hybrid filter  props already in the pinned leaf        +0 I/O
    perspective KG 0x03│ctx…          1 range ∝ that KG
    label scan     0x02│l…            1 range ∝ members
    by uuid        0x06               1 point read
    ctxs-of-edge   scan (0x07 if ever needed)              ✗ the one bad one

Sacrifices (Law 4): perspective edges +8B/key both directions (base pays 0) · mirror doubles edge
writes (+57% measured, optional) · props opaque until 2b · BFS visited-set ∝
reachable (inherent; caller bounds depth).

## Big values (the e1 lesson, decided before it repeats)

The kernel refuses records over ~4KB (one page). Two core needs break that
ceiling: **embeddings** (1536-dim f32 = 6,144B; app uses >1024 dims) and
**RCA payloads** (a log/trace event exceeds 4KB routinely). This is not a
vector problem -- it is a kernel gap that vector merely hits first.

Per-feature workarounds (chunking in the graph layer, quantization as a FORCED
policy, truncation) are exactly how e1 rotted: local hacks every reader must
know about. Rejected.

Both reference engines solved it in core, verified in source:
- SQLite `btreeInt.h`: payload beyond a threshold spills to an OVERFLOW CHAIN
  -- the cell stores "first page of the overflow chain", each overflow page
  starts with "page number of next overflow page".
- DuckDB `string_uncompressed.hpp`: `BIG_STRING_MARKER` + overflow blocks; a
  marker (block_id, offset) points out of the segment.

DECISION: overflow chains in the e3 kernel (phase 2c), before any keyspace
grows around the limitation. Law 5: overflow pages are ordinary pages --
checksummed, blast radius stays one record. Quantization (f16/i8) then becomes
an optimization a caller chooses, never a limitation they obey.

## Law 6 — snapshot reads (IMPLEMENTED, phase 2f)

A write never blocks or degrades a read. Mechanism: D26. `open_snapshot`
serves the newest published generation, byte-stable for its whole life,
beside a live writer, zero coordination. The gate in counter form: a pinned
reader's answers AND disk-read counts are identical solo vs interleaved
with writer epochs updating the very keys being read (kernel/tests/
snapshot.rs, mutation-checked). Wall-clock witness with machine-contention
control in kernel/src/bin/snapbench.rs.

## Phases (revised after the big-values contemplation)

    2c-overflow ✓ kernel overflow chains    gate PASSED: any-size round-trip, crossed-chain
                                            refusal, blast radius 1 record, laws flat
    2d-io ✓       FileIo trait (D21)        gate PASSED: win/android/arm compile-verified,
                                            platform-leak invariant test
    2e-vector ✓   0x05 keyspace, cat dim    gate PASSED: app RAM-flat, ingest ladder won
                                            every rung, hybrid RCA 4x, ablations 1/5
    2f-snapshot ✓ CoW epochs + dual meta    gate PASSED: zero added reads (counter form),
                                            byte-stable readers, CoW write tax unmeasurable
    2g-vec-index ✓ 0x0B fingerprints (D27)  gate PASSED: nearest 7-12x SQLite to 1M,
                                            recall 0.90-1.00, regressions unchanged
                  (2g.2 ✓ scan tier: recall 1.000, 2-3x faster, L1, delete_vec, par search)
    2h-fulltext ✓ FACT-03 + D30             gate PASSED: every ranked query beats FTS5
                                            (term 7x/1.5x, multi 8.4x/2.2x at 100K/1M),
                                            typo search FTS5 lacks, laws flat
    2i-spatial ✓  FACT-04 + D31             gate PASSED: 7 of 8 cells vs SQLite rtree
                                            (ingest 4.6-5.5x, bbox 3.8-4.3x; radius@1M
                                            the named 2.2x loss), demo flagship real
    2k-vector-nav ✓ FACT-02 + D32           gate PASSED: capped 1M walk 191ms @ 0.910
                                            (target 1.5s: 7.8x headroom), rematch won
                                            on latency at every recall point
    2l-hybrid_score ✓ D33 ScoreExpr         gate PASSED: 4-atom expression 231.7ms capped
                                            vs SQLite's 3-atom CTE 4039.8ms (17.4x)
    2m-atomic-checkpoint ✓ known-good state gate PASSED: every family gate green on one
                                            binary at 100K+1M; sizes measured vs FTS5/
                                            rtree/SQLite/sqlite-vec/Qdrant/Kuzu; fold WAL
                                            bounded (15GB -> saw 0-600MB); found+queued
                                            the freelist gap
    2n-freelist ✓ D34 + the 2h bug find     gate PASSED: overwrite churn byte-flat; text
                                            1M queries up to 15x faster (dead pages were
                                            a QUERY TAX); starved-reader pin gate green
    3-sql ✓       port e1 SGQL             gates PASSED 2026-08-28: integration 362/367
                                            (5 = physical-format, ignore-gated); e1-CLI
                                            differential ZERO ASYMMETRY (78 stmts); venue
                                            workload THROUGH SQL, COUNTS AGREE at 100K+1M
                                            capped; kernel 182/0; frozen files byte-equal
    4a-repo-shape adopt e1's layout          layout adopted; core+kernel green in it, and
                                             WINDOWS PORTABILITY RESTORED (the CI shape
                                             e1 carries caught it immediately); wrappers,
                                             CLI and C ABI staged, blocked on two named
                                             architectural gaps below
    4b..4f        skcli / capi / wrappers   per-language, each gated on its own e1 tests

## The keyspace invariant (added 2026-08-29, after it was violated)

**No index keyspace may be keyed by ITEM alone. The key begins with the
identity of the index the entry belongs to.**

Every family obeys it -- text by field, spatial by field, property indexes by
(collection, field), search by index id, labels by collection, edges by
context. The vector family did not: written in 2e, when "the vector store"
was singular and there was no SQL layer able to declare a second one, it
keyed by item alone --

    vec_key(id)    vcode_key(id)    nav_key(id)     -- node id only
    CAT_VEC_DIM    -- ONE dimension slot for the whole database

-- and the cost was larger than one index. Because the dimension was global
too, a 384-dim text embedding and a 512-dim image embedding could not coexist
AT ALL, which is the ordinary case for anything multi-modal and precisely the
demo this engine is built for. The visible symptom was a refusal in the SQL
layer ("one vector field per store"), which read as a policy but was the
layer being honest about a missing coordinate. Found by running the
predecessor's own Python tour: it declares an embedding on two different
tables, and stop 5 fails.

It now keys `(field, id)` like the rest, with dim, seed, code width and the
navigation state in a per-field catalog record. Two things the repair taught,
both worth having in advance:

* The recipe row's TAG matters as much as the key's shape. Parked above the
  fingerprint keyspace, it stopped fingerprint writes from being appends --
  the leaf splitter only closes a left page full when the growth point is the
  rightmost leaf -- and leaves settled at 4 entries where they had held 7:
  5% more file, read by every scan. It belongs in the catalog, which sorts
  ahead of every data keyspace and disturbs no space's growth point.
* The coordinate is not free: eight more bytes on every vector, fingerprint
  and navigation key cost ~4% on the nav query ladder at 20K x 1536-dim,
  recall unchanged. That is the price of the invariant, and it is the same
  price every other family already pays.

The invariant is cheap to check by eye at design time and expensive to fix
after release, since it is the on-disk layout. State it before adding a
family, not after.

## The coordinate's price, measured (2026-08-29)

Making the vector family obey the keyspace invariant costs **6-8% of query
latency**, measured interleaved against the pre-change binary (two rounds,
same machine, same minute -- the machine drifts enough that a saved baseline
is not trustworthy for a number this size):

    os=64    25.18 / 25.50 ms  ->  27.39 / 27.51 ms   +8.2%
    os=256   55.94 / 55.50 ms  ->  59.18 / 59.46 ms   +6.4%
    recall   identical at every oversample
    on disk  +0.21%

The cause is arithmetic: eight more bytes on every vector, fingerprint and
navigation key means fewer entries per page and more pages touched. It is
the same price every other family already pays for being addressable.

This MISSED the <=5% gate set before the work, and is recorded rather than
rounded. The way back is to stop paying u64 for a coordinate that needs far
less: intern the field name to a small SLOT (the search family already does
exactly this -- `searchslot_key(idx, kind, id)`), and the key grows by 2
bytes instead of 8. That should recover most of it and is a self-contained
follow-up.

A second finding from the repair, worth as much as the fix: the *tag* of the
recipe row matters as much as the shape of the key. Putting the per-field
vector metadata under a NEW tag (0x15) cost 11% and 5.2% file growth,
because every keyspace shares one B-tree and a leaf is only closed full when
the growth point is the rightmost leaf -- permanent rows sorting ABOVE the
vector data turned an append into a split, and vcode leaves settled at 4
entries where they had held 7. Under the catalog tag, which sorts ahead of
all data, the fill returned to 7 and the cost vanished.

## Phase 4 findings (what adopting the shape revealed)

Two blockers, both architectural, both discovered by compiling e1's real
wrapper code against this core rather than by reading it:

1. **The core is not `Send + Sync`.** The graph sits behind
   `Rc<RefCell<Graph>>` -- a phase-3 porting device (index handles needed
   engine access while the executor stayed byte-frozen), never a decision.
   Every wrapper needs it gone: pyo3 classes must be `Send`, the C ABI's
   handle table is shared, and the CLI's `serve`/`pg` listeners hand the
   database to request threads. The change (Rc->Arc, RefCell->RwLock,
   OnceCell->OnceLock, plus the edge-slot table) is mechanical but it is
   NOT free and it is NOT only a build fix: uncontended atomics and lock
   acquisition land on the single-user main line, which is ~70% of the
   audience and must never regress. It therefore needs the atomic-vs-SQL
   bench re-run as its gate, and a decision on lock granularity, before
   anyone writes it.
2. **The service surface does not exist here** (`sekejap::engine`,
   `::serve`, `::pg`). The C ABI imports `engine`; the CLI ships an HTTP
   server and a PostgreSQL wire listener behind default-on features. This
   is the engine-pack work, and it depends on (1).

After the boundary methods landed, the python wrapper's error list is
exactly four items and every one is named: two `Send` bounds (blocker 1)
and the two `explain` entry points we deliberately do not ship. Nothing
unaccounted for remains between this core and e1's wrapper code -- which
is the real deliverable of 4a: the gap is now a decision list, not a
discovery problem. When the python slice runs, its `explain` methods come
off the wrapper rather than a stub going into the core.

Smaller findings, resolved in 4a:

- **Windows**: the kernel did not compile off Unix -- the 2n reader table
  called `libc::flock` directly. Replaced with std's file lock (same
  contract, and the only form that exists on Windows). `cargo check -p
  sekejap -p kernel --target x86_64-pc-windows-msvc` is now clean; the
  kernel suite is unchanged at 182/0. Without the reader table a writer
  must assume a reader at generation 0 and page recycling stops for good,
  so this was not a cosmetic gap.
- **`kernel::Error` had no `Display`** -- it is what a wrapper user
  ultimately reads, so every variant now says what failed and where the
  way out is (Law 5 is mostly met through these strings).
- **The predecessor's lab equipment is not carried over**: 58 examples and
  26 benches measured ITS internals (compaction phases, residency of
  structures that no longer exist). This engine has its own under
  `bench/`, `kernel/src/bin/` and `examples/`. Keeping harnesses that
  cannot compile, or "fixing" them to measure something else under the
  same name, would both be worse than not shipping them.
- **`docs/usage/` survives unchanged** (it documents the query surface,
  which is identical); `docs/developer/` describes the previous storage
  engine and now says so at the top of each affected page, pending a
  rewrite.
- **CI is honest**: the four agreement fuzzers named in the previous
  workflow do not exist here. Three of them (index, compaction, replay)
  state invariants this engine owes just as much and are worth porting;
  the fourth compared two payload encodings this engine does not have.
  Naming them in CI would have made it green on tests that do not exist.

Awaiting a decision (not blocking):

- The `USING hnsw` index keyword is kept for source compatibility, but the
  implementation is Vamana; the shipped README still explains it as HNSW.
  Rename, alias, or reword -- a naming call, not an engine one.
- Three package files still point at a retired homepage
  (`sekejap.<retired>.com`) and one at a retired Maven coordinate, while
  the live ones are `sekejap.life` and `life.sekejap`. Stale in the
  predecessor too. Copyright holders left untouched -- that is a legal
  identity, not a build detail.

## Phase 3 named deviations (e1 parity ledger)

Method: e1's sql.rs + query.rs are BYTE-FROZEN copies; every difference
in behaviour must therefore live at the storage boundary and be named
here. e1's own 46 test files are the parity oracle (integration.rs =
367 tests, e1's full surface).

Accepted, by design (the engine underneath IS the difference):
- SET WAL_MODE takes logical|physical (e1: json|binary). Both accepted;
  the kernel WAL satisfies both contracts, the knob renames honestly.
- hnsw_* build knobs (m, ef_construction) are accepted and ignored: the
  vector tier is Vamana over btree rows (D32), which has no such knobs.
  The METRIC argument is honoured exactly (per-field registry, kernel
  rescore) -- an L2 build must rank by L2, and does.
- One vector field per store in 0.17 (e1 allowed several); refused with
  a message, not silently merged.
- CREATE INDEX ... USING gist is REFUSED with directions to gin. e3 has
  no GiST machinery; aliasing gin under the name would be dishonest.
- SEARCH typo => n forces the per-token edit budget past the kernel's
  length ladder (e1 semantics; the ladder alone gives 4-char tokens 0).
- 5 integration tests are unportable BY DESIGN, not failures: they open
  e1's physical artifacts (snapshot.json headers, wal.log format, vector
  .bin sidecars) which do not exist in e3's single-file page format:
  snapshot_v2_header_present_and_reopens, snapshot_legacy_headerless_
  still_opens, logical_wal_smaller_than_physical, sql_txn_wal_
  incomplete_discarded, disk_vector_store_phase6_compact_skips_json_
  vectors. Recovery/atomicity obligations they guard are covered by
  kernel gates (recovery dedup, freelist, torn-frame tests).

Awaiting sign-off (recorded, not yet decided):
- RENAME TABLE rewrites slugs; hashes change, so old-hash edges and
  vectors are not re-pointed (e1 shares the weakness -- verify).
- Edge-attribute BINDINGS inside MATCH patterns: VERIFIED 2026-08-28 by
  the CLI differential (bind, project e.year, filter e.year > n, read
  back after UPDATE-by-predicate -- all byte-identical to e1).
- EXPLAIN / EXPLAIN ANALYZE: REFUSED honestly (user decision 2026-08-28,
  same posture as gist): e3 has no explain entry points and we ship only
  what really exists -- no anticipatory surface. skcli's EXPLAIN arm gets
  a refusal message with directions, not a stub.

## Verdicts (numbers live in bench/)

- 3-sql (2026-08-28): e1's SGQL surface runs on the e3 kernel with sql.rs
  and query.rs BYTE-FROZEN -- every behavioural difference had to surface
  at the storage boundary, and each one is either fixed or named in the
  phase-3 ledger above. Three oracles agreed before the phase closed:
  e1's own 46 test files (362/367, the 5 = physical format), e1's real
  CLI over a 78-statement corpus (zero asymmetry, skcli's renderer
  verbatim on both sides), and SQLite on the venue workload through SQL
  (COUNTS AGREE at 100K and 1M, capped 500Mi/2cpu). Bugs the port
  unearthed IN THE KERNEL: vecquant code_len truncated to zero for dims
  <= 2 (pad floor 64 fixed it); the instant-search edit ladder needed a
  forced-budget override for e1's typo => n. Named perf losses, phase 5:
  aggregate COUNTs 47-186x behind SQLite at 1M capped and sort_limit 42s
  (try_index_order_limit probes node_data() per index entry -- a kernel
  point-get, random order, 1M of them under a 500Mi cache; e1 answers
  the same probe from a RAM map); venue store 1.06GB vs SQLite 138MB
  (JSON rows + 4 indexes + dual edge rows). Point 0.85ms / 1-hop 0.16ms
  / 3-hop 0.75ms at 1M capped stay healthy.

- 2n freelist (D34): the loop's reuse probe unearthed a DATA-LOSS BUG
  shipped since 2h: delete_prefix's whole-leaf clear re-initialised the
  header, zeroing next_leaf; a MIDDLE leaf then looked rightmost to the
  append fast path and later inserts orphaned committed subtrees — a
  fold+insert+fold cycle silently lost every folded text segment
  (103 rows -> 0, reproduced at the phase2 tag; dormant because every
  test and bench folded exactly once). Fixed; the regression test runs
  three full cycles and re-queries round-1 data. THE FREELIST: gates
  green — overwrite churn holds the file BYTE-IDENTICAL across rounds;
  fold churn 27.7 -> 14.5MB at 30K docs (residual slope ~= live
  growth); a snapshot reader on a 64KB starved pool pinned under 8
  epochs of recycling reads byte-stable; freelist survives reopen;
  corrupt sidecar leaks only. Mutations caught: no-pop, ignore-readers
  (required starving the reader's cache to make the gate falsifiable),
  no-import, alpha... plus the walk-index shift (skipping a child left
  70/10000 rows; child0 is now never detached). THE SURPRISE: dead
  pages were a query tax, not just disk — capped re-measure: text 1M
  term/multi/prefix/fuzzy 383/881/1602/469 -> 47/240/107/92ms; text
  100K prefix 83.9 -> 3.7ms; hybrid one-pass 100K 235 -> 19.0ms and
  1M 6415 -> 146.9ms (SQLite: 4.0s / ~87-174s); geo 1M file 406 ->
  218MB. One-shot text 1M file ~unchanged (1.51GB; frees pending,
  nothing reuses them before exit — vacuum is phase 5's shrink).
  Every 2m speed number is thereby superseded in e3's favour.

- 2m atomic-checkpoint: one binary, capped pod, 100K+1M, sizes in every
  cell. TIME (1M): base 3.8/27.2s seq/scat; graph build 23.6s, 3-hop x200
  193ms; text term/multi/fuzzy 383/881/469ms (FTS5 526/1585/none); geo
  radius/knn/bbox 6.9/0.21/0.35ms (rtree 2.6/0.16/0.96 -- radius loss
  stands); hybrid 4-atom 6.42s (SQLite 3-atom ~87-174s contended, 4.04s
  clean at 100K). Vector 1M: fold STOPPED at ~40% on user call, ~5.2h
  extrapolated; walk numbers cited from 2k's capped run of the identical
  path (191ms @ 0.910). Snapshot gate: pinned-cost ratio 1.95->2.15x,
  busy ratio improved 7.0->5.4x; the headline C/D 1.13->5.94x is a
  CONTROL artifact (the 2f control run was machine-contended, flagged in
  its own file); counter-form zero-added-reads test green. DISK, the
  phase's real finding: files track bytes-EVER-written -- the allocator
  never reuses freed pages. Measured: text live 64MB vs 1.52GB file at
  1M (95% dead; live is SMALLER than FTS5's 127.6MB); reuse_probe grows
  ~3.9MB per 5K-doc fold round, linearly, forever. Vector 100K sizes,
  same data: sqlite-vec 632MB (no index), e3 1.25GB (2.0x raw, three
  tiers INCLUDING the dead space; ~800MB projected post-freelist),
  Qdrant ~1.17GB (collection delta; its storage also held 2.9GB of
  residue from deleted collections), Kuzu 2.33GB uncapped (its capped
  ingest OOMed at two pool sizes; uncapped it answered 20ms @ 0.650 at
  defaults). Fix designed, LMDB freeDB shape: free-with-generation at
  checkpoint, reader table for oldest-live-gen, allocate freelist-first
  -- inline accounting, NO periodic job (the e1 compaction is not coming
  back). Harness bugs fixed en route: graphbench materialized 8M edge
  pairs (~700MB -- the pod OOM was the BENCH); a concurrent sizes pod
  contaminated one nav sweep (protocol violated, arm re-run). Fold WAL
  bound (D32 note): 25K-insert checkpoint interval, wal saws 0-600MB at
  1M vs 15GB before; mutation-tested.

- 2l hybrid-score (D33): capped pod, 100K docs (two text fields,
  384-dim vector, point geometry; word-for-word mirrored corpus).
  Candidates: one body search, top 2000. Expression: Bm25(title)*0.4 +
  Bm25Norm(body,k=1)*0.3 + cosine*0.2 + 1000/(1000+ST_DISTANCE)*0.1.
  e3 one-pass 231.7ms; the naive app-side shape (two full searches
  materialized + per-candidate atoms) 266.8ms and the two arms' top-10
  AGREE (the naive arm is a second oracle at scale). SQLite, same
  corpus and shape minus the vector atom it cannot express (FTS5 bm25
  CTEs + in-SQL haversine): 4039.8ms — 17.4x. Cost profile: the
  ~4000 per-candidate vector+geometry row reads dominate; the
  candidate-filtered postings decode is a few ms. Laws: zero resident
  state after scoring (counting allocator; a forgotten column trips
  the test), scores byte-stable on a pinned reader during writer
  churn, fold-invariant (head rows and packed segments score
  identically). Oracle: independent-math BM25+cosine; mutations
  caught: idf dropped, saturation unbounded, cosine denormalized,
  head postings skipped, column leak. Ablations 0/5 — the first
  design met the gate.

- 2k vector-nav (D32): capped pod 500Mi/2cpu, 100K/1M x 1536-dim,
  exact generator ground truth, identical data both engines (bit-equal
  rust/numpy generators, verified). Clustered 100K (614MB raw > pod
  RAM, 64MB engine cache): walk 27.5ms @ 0.940 (os=32) vs scan 168ms @
  1.000 vs Qdrant 308ms @ 0.980 (full 500Mi) vs sqlite-vec 1393ms @
  1.000; LanceDB ingested but its IVF_PQ build OOMed (2 configs);
  Kuzu's ingest OOMed (2 buffer pools). Constant-density 1M capped:
  walk 191ms @ 0.910 / 336ms @ 0.970 vs scan 1823ms @ 1.000 — the
  2g.2 missed target (capped 1M <= 1.5s) closes with 7.8x headroom.
  Dense 1M (~977 near-ties/cluster): scan COLLAPSES to 0.300 @ 1.7s
  while the walk reaches 0.805 @ 485ms — each tier covers the other's
  failure mode (walk: dense; scan: uniform 1.000 where the walk gets
  0.81). Uniform capped: e3 scan 481ms @ 1.000 vs Qdrant 4823ms @
  0.985. Cache knob 64->256MB: uniform ~1.8x latency, recall
  bit-identical. SIMD (autovec + LANE4 table): encode 49.6->28us/vec,
  scan 1993->496ns/code. Ablations 3/5: autovec restructure, LANE4,
  true-Vamana (alpha^2 + 2R slack). Bench-methodology lessons paid
  for in blood: (1) the additive generator emitted exact duplicates
  every 40 ids — recall 0.000 was two CORRECT answers with disjoint
  tie-classes; (2) the scan tier as recall reference drifts at 1M
  (its own recall is 0.300 on dense data) — ground truth must be
  exact and external; (3) f32 sortable keys flipped at u64 width
  ranked negative estimates worst (latent, test-pinned); (4) recall
  monotone in ef is an ORACLE — any inversion is a bug or a broken
  reference, never noise. Fold 14.0ms/vec at 1M-CD single-threaded
  (Qdrant capped ingest+index: 15-25ms/vec).

- 2i spatial (D31): capped pod, identical coordinates. e3 vs SQLite rtree:
  ingest 0.4/11.0s vs 2.2/50.7s (5.5x/4.6x); bbox 0.03/0.29ms vs
  0.13/1.10ms; kNN 0.09/0.17ms vs 0.17/0.17ms; radius 0.30/6.80ms vs
  0.54/3.08ms — wins at 100K, the ONE loss at 1M (2.2x, cold scattered
  leaves at query sites; candidates scan itself measured 0.6ms).
  Laws: heap 0.25->0.26MiB across 4x geometries; snapshot readers'
  spatial answers immovable during churn; crash replay restores postings
  and geometry together. Demo flagship REAL: radius -> serves edges ->
  BM25 -> true metres in demo_parity. PostGIS parity: live fixtures to
  1e-6 relative. Ablations 3/5 (range budget 32->256, haversine band,
  fine level 16->12). Oracle catches on the way: corner-clip fallback
  missed interior queries (world bucket now); a lat/lon swap survived the
  regional oracle (asymmetric axis test now); Vincenty returned 0.0 for
  invalid latitudes (coordinate validation before the WAL now).
- 2h fulltext (D30): capped pod, identical docs. Ranked queries beat FTS5
  at both scales: term 6.8 vs 48ms (100K), 380 vs 578ms (1M); multi-term
  18.6 vs 157 / 822 vs 1771ms. Fuzzy search 9.8ms/441ms where FTS5 has
  NOTHING; search is instant after every insert (no flush). Laws: heap
  0.40->0.86MiB across 4x corpus (8MiB pool); fold publishes before
  erasing (Law 3); hybrid RAG composition (text narrows, vector ranks)
  hand-oracle-tested. Ablations 4/5: A1 norm-memo + A2 keys-during-pack
  (both superseded), A3 dl-in-postings (term query 28x), A4 leaf-batched
  delete_prefix (fold 8.8x, and a generic primitive every family gets).
  Bugs the oracles caught: 0xFF separator sorting longer terms before
  their prefixes (fuzzy skipped real words); searched-not-positional
  separator split corrupted on docids containing 0xFF; score drift across
  folds from dead docs' tokens lingering in totals. Named gaps: ingest+
  fold 3.5x FTS5 at 1M; instant tier scores its whole expansion.
  Regressions unchanged. FTS5 multi-term semantics note: MATCH defaults
  AND; the gate uses OR for parity.
- 2g.2 scan tier (capped pod 500Mi/2cpu unless noted): search 100K
  499 -> 218ms (t=2), 1M 6.8 -> 3.27s (t=2) / 1.17s at t=8 uncapped;
  recall RAISED to 1.000 (2-bit codes + oversample 16 beat 4-bit/os8's
  0.900 while halving code bytes to 512B, 8% file overhead). L1 metric
  added (exact in rescore; scan stage uses the L2 proxy with a 4x wider
  pool -- rotation preserves L2, never L1 -- recall 0.97-0.99). delete_vec
  (both rows, one commit boundary, crash-tested). ParallelSearcher: pinned
  snapshot readers, one id-slice each (per-query opens LOST to serial --
  measured 482 vs 335ms -- reuse won 4.2x); partition bound rides the
  commit as CAT_VEC_MAX after the equality test caught next_id covering
  nothing. MISSED TARGETS, named: ingest tax still 1.9x vs 2e (target
  1.5x) and capped 1M search 3.27s (target 1.5s; met only uncapped) --
  both are the scalar FWHT encode/scan math; SIMD kernels move to the
  navigation-tier phase. Ablations 4/5 (affine no-lookup kernel, 2-bit,
  oversample, reader reuse).
- 2g vector index (D27): all four gates on the cluster, HARD CAP 500Mi/2cpu
  (the constrained-first discipline, D25). Vector-first nearest-10 at 1536-d,
  identical bytes: e3 0.50/1.98/3.35/6.80s at 100K/250K/500K/1M vs SQLite
  chunked+numpy 6.1/14.5/31.5/66.2s = 7-12x, recall 0.900; 4x faster than
  e3 own exact scan. Ingest A/B same pod: index costs 1.9x incr / 2.7x bulk
  vs 2e (encode math), file +17% -- still 5.2-5.7x faster than SQLite
  ingesting WITHOUT an index. Graph-query regressions unchanged (3-hop
  0.019ms, hybrid 0.233s/100 -- both at-or-better). Ablations 3/5: encoder
  hoist (signs/PRNG per vector -> once), rotation rounds 3->1 (hostile-
  sparse recall case added first; identity-rotation mutation fails it),
  query-LUT + 4-accumulator scan kernel (serial add chain was 75% of
  search; 303 -> 208ms/query on dev hw). Incident logged: the first SQLite
  search arm fetched 6GB unchunked and froze the dev machine -- benches now
  run ONLY on the cluster, and the arm streams in constant memory.
- 2f snapshot (Law 6): single writer + shadow-paged snapshot readers (D26).
  CoW write tax on checkpointed 250K x 1536-dim ingest: unmeasurable
  (A/B same machine, pre 5.0-5.9s vs post 5.0-5.1s); file +49KB/epoch of
  path copies. Reads at baseline after the parent-stack cursor + validated
  bit (megamini range 0.524 vs 0.526 base; 3-hop 0.026 -> 0.016ms, the bit
  made hops FASTER). Deterministic gate: pinned reader's answers and disk
  READ COUNTS identical solo vs interleaved with epochs updating the read
  keys; mutation-checked (no-shadow, older-slot-wins, frozen-hint, flip
  order -- all caught). Wall-clock witness vs an identically-built control
  store ingesting to a separate file: engine-added read cost 1.13x
  (fresh-open readers; pinned near solo), write 0.95x -- machine sharing
  alone costs 2.4-6x on the dev laptop. Two-barrier checkpoint pinned by
  trace test. Torn-newest-slot falls back one generation. Pre-2f files
  refused loudly (page 1 became meta slot B). Ablations: 1 of 5 (the
  per-residency validated bit). Found on the way: an empty control store
  controlled nothing; cache-warm readers made a no-shadowing mutation
  survive two gate versions -- both harness bugs fixed and documented.
- 2e vector: ingest ladder 50K-500K x 1536-dim vs SQLite blob column, e3 wins
  every rung (incr 31.9s vs 187.4s @500K = 5.9x, bulk 27.1s; file 7% smaller).
  Exponent stated honestly: e3's I/O is exactly linear (writes and bytes 2x
  per rung, reads=1) but wall time is N^1.5 because the SSD's sustained-write
  rate decays (raw dd alone is N^1.57 on the same range); SQLite's N^1.04 is
  CPU-bound at 22MB/s, below the device knee. Hybrid RCA query (bfs depth-3
  -> prop filter -> rescore top-10, 200K nodes, 100 seeds, checksums
  identical both engines): e3 22ms/query vs SQLite+numpy 88ms = 4.0x; build
  21.5s vs 58.3s. app gate: 600MB of vectors through an 8MiB pool, live
  heap flat 0.25MiB at 25K and 100K alike; rescore PEAK-delta < 1MiB, PEAK
  tracked inside alloc() after point-sampling missed a 30MB transient.
  Ablations: 1 of 5 (coalesced uncached chain pread, 245->217us per cold 6KB
  get; remaining floor is SSD latency). 1M/5M rungs run on a second machine
  (12c/47GB Linux server, bench/VEC_RESULT_SERVER.txt): 5M x 1536-dim = 39GB
  in 172.6s bulk / 229.4s incr vs SQLite 3065.3s = 17.8x/13.4x, exponents
  ~N^1 BOTH engines there -- no SLC knee on that device, confirming the Mac
  curve was the device, not the engine.
- Phase 1 gate: e3 beats SQLite every arm (seq 2.5x, scat 2.1x, bulk N^0.98);
  DuckDB+PK OOMs at 1M under the shared 64MiB cap (Law 1 result).
- Phase 2 gate: (a) 1.21→1.47 reads/hop across 4x store ✓ (b) 3-hop RCA vs
  SQLite recursive CTE, warm both: equal-or-better everywhere, 1.3x @5M ✓
  (c) sequential vs hashed ids 21.5x ✓ (d) model/crash/Law-1 tests ✓
- 2c overflow chains: values of any size via chained pages; whole-value crc
  refuses crossed chains of individually-valid pages; blast radius 1 record
  (49/50 survive damage); 20KB value crosses a crash via WAL chain rebuild;
  heap flat 0.27/0.30 MiB over 4x. Found: WAL frame's u16 vlen silently
  truncated >64KB values entering the log. Ladder re-run 10K..5M: seq N^0.91,
  scat N^1.21, both better than 2b ref; bulk-5M slowdown bisected to disk
  space (42GB->11GB free), not code. Ablations: 0 of 5.
- 2b property index: e3 now wins EVERY mini-mega query. sort 29.5ms->0.005
  (par), compound 25.2->0.047 (5.8x FASTER than SQLite), range 27.4->0.45
  (3.2x faster), eq 4.2->0.19 (2.8x faster); graph untouched 0.002/0.023.
  The last 1.9x was two Vec allocs per row: for_each_ref folds borrows
  straight off the pinned leaf (5.5ns/key), equivalence-tested against the
  iterator at 50 random widths, hi-bound mutation caught.
  Iterator machinery fixed on the way: per-entry leaf re-open
  was O(entries^2)/leaf; whole-leaf batching fixed ranges then cost graph hops
  2-3x; adaptive threshold (first 8 entries per-entry, then batch) restored
  hops to 0.002/0.023ms exactly while keeping the range wins. 5M 3-hop parity
  holds. Batching bug caught by tests in seconds: done-flag returned with a
  full buffer, dropping the last leaf. Ablations: 2 of 5.
- 2a.2 ctx migration: flat ctx-in-key REGRESSED 5M 3-hop 2.2x (file +640MB ->
  OS-cache pressure; machinery counters unchanged) -- caught by re-running the
  gate, fixed by splitting base/perspective tags, numbers restored exactly.
  Ablations spent: 2 of 5.
- mini-mega (e1's workload, 200K, vs SQLite-indexed & Kùzu): e3 wins all
  graph/point ops (1-hop 276x vs Kùzu; 3-hop 3x vs SQLite, 44x vs Kùzu);
  loses property filters to SQLite's indexes where e3 has none yet (2b targets:
  range 18x, sort 4900x, compound 92x). Kùzu wins only the OLAP range scan.
