# Architecture — sekejap-e4 at HEAD

One read of the engine as it stands. Behaviour is stated by
`docs/core/GRAPH_CONTRACT.md` and `docs/lang/QL_CONTRACT.md`. Module homes are
`docs/core/SOURCE_LAYOUT.md`. This document maps those contracts onto the modules
and the on-disk keyspaces. It does not add a law.

Numbers that quote a workload are labelled with metric and scale. The
multimodel numbers below are from `<scratch>`
and are **50,000 rows, this Mac, 2026-09-20**. The storage scale table is the
Mac arm of `docs/FOUNDATION_SCALING.md` (1,000-change transactions against a
loaded population).

## 1. Storage

Contract: `docs/core/V2_COLLECTION_INTEGRATION.md`, `docs/core/FORMAT_V2.md`,
`docs/core/RECOVERY_CONTRACT.md`.

### 1.1 Page WAL — `src/store/pagewal/mod.rs`

`PageWalStore` is the selected V2 backend (`src/store/mod.rs`, public as
`e4_prototype::collection_backend`). One writer per directory, exclusive
`writer.lock`. Pages are 4,096 bytes. WAL frames are 4,144 bytes (`E4PWAL02`:
32-byte header, 4,096-byte page image, 16-byte database identity, CRC32C at
offset 28). WAL cap is 16 MiB (or the persisted `wal_bytes` policy). Two
checkpoint metadata copies live in data pages 0 and 1 (`src/store/pagewal/format.rs`).

A transaction is published only after the commit frame's FULL barrier returns
and the writer records `(floor, tx, end)` in the two-copy hint `readers.lock`
(`E4PWHNT1`, 96 logical bytes). Readers beside a live writer inspect exactly
that prefix. Eight advisory slots (`reader-N.lock`) bound concurrent snapshots.
A checkpoint folds published WAL into the data file and resets the WAL; it
runs only when it can take every slot, and it defers (`false`) if any reader
in any process holds one.

Create-time feature bits (`compact-cells` and friends) are written into the
header (`src/store/pagewal/format.rs`) and are what an older binary refuses
before touching bytes (`docs/core/FORMAT_V2.md`). Repair rebuilds a store from
intact frames (`src/store/pagewal/repair.rs`). Typed recovery above the kernel
treats a rootless output as candidate evidence, never a published database
(`src/store/recovery.rs`, `docs/core/RECOVERY_CONTRACT.md`).

### 1.2 B-tree keyspaces

Every user and catalog record is a cell in a kernel B-tree (`core/kernel/src/btree.rs`)
addressed by a tagged key. The primary tree holds:

| Tag | Key shape | Payload | Module |
| --- | --- | --- | --- |
| catalog replicas | collection/layout/header packets | descriptors | `src/collections/mod.rs` |
| `0x10` | collection name bytes | collection id | `src/collections/mod.rs` |
| `0x20` | `collection \|\| external key` | `EntityId` | `src/collections/mod.rs` |
| `0x40` | `collection \|\| sequence` | dense-v3 row | `src/collections/mod.rs`, `src/store/dense_v3.rs` |
| `0x60` | `collection \|\| sequence \|\| ordinal` | f32 vector sidecar | `src/collections/mod.rs` |
| `0x70`… index tags | family-specific (section 3) | postings | `src/index/*`, `src/collections/catalog.rs` |

Scalar and point indexes may live in their own B-tree (`INDEX_TREE_FEATURE`
`0x80`, encoding version 2, `src/collections/catalog.rs`). Text, vector,
quantized-vector, geometry, graph, rows, mappings and sidecars stay in the
primary tree. `root == 0` is an empty per-index tree; the descriptor that
names the root is written in the same commit as the pages that moved it.

Lexicographic byte order is the scan order. Integer fields in keys use a
width-tagged big-endian encoding so numeric order is byte order
(`src/store/scalar_key.rs` for scalar values; `ordered` in
`src/collections/mod.rs` for ids).

### 1.3 Snapshot readers, one writer

`Database` is a single-writer handle (`src/collections/mod.rs`,
`docs/core/COLLECTIONS.md`). `open_snapshot` opens a read-only handle on the
published prefix; it stays byte-stable for its life beside a live writer
(Law 6). `commit` publishes; `rollback` re-inspects durable files, truncates
past the last complete commit, and republishes that prefix. A new snapshot
opened after commit sees the new generation; one opened before does not.
Coordination is the slot/hint protocol in `src/store/pagewal/mod.rs`, not a
reader lock on the writer.

On **50,000 rows, this Mac, 2026-09-20**, load was 2,282.493 ms (E4) vs
2,120.315 ms (Postgres), ratio 1.076×; checkpoint 0.703 ms vs 635.421 ms;
on-disk 41.242 MiB vs 58.430 MiB.

## 2. Collections and typed fields

Contract: `docs/core/COLLECTIONS.md`. Module: `src/collections/mod.rs`
(catalog `src/collections/catalog.rs`, rebuild `src/collections/rebuild.rs`,
verification `src/collections/verification.rs`, sort `src/collections/sort.rs`).
Row codec: `src/lib.rs` (`Layout`, `Kind`) and `src/store/dense_v3.rs`.

A collection has a `CollectionId` and an immutable `Layout` of named `Kind`s:
`Text`, `Int`, `Real`, `Bool`, `Json`, `Geo`, `Point`, `Vector(n)`. Declared
fields occupy typed slots; undeclared fields sit in the extras lane. Missing
and null are distinct. Vectors are f32 lanes in `0x60` sidecars; the dense
row stores a locator, not the lanes. Points are two f64 coordinates. `Geo` is
a GeoJSON object (`src/index/spatial/geometry.rs`).

`EntityId { collection, sequence }` is the internal identity. The external
key is UTF-8, 1–1,024 bytes, mapped at `0x20`. `put` upserts and keeps the
id; delete then reinsert allocates a new sequence. Committed ids are never
reused. Scan order is entity-id order.

`create_index` records an `IndexInfo` (`IndexFamily`, field, unique, state
Building/Ready/Dropping) and, once built, maintains postings on write.
`create_limited` persists kernel `E4LIMIT1` and enforces `data_bytes`,
`wal_bytes`, `tracked_pages`, `record_bytes`, `readers`, `recovery_bytes`.

## 3. Index families

Contract sections: `docs/lang/QL_CONTRACT.md` (family hooks), `docs/core/SPATIAL_FUNCTIONS.md`
(point/geometry), `docs/core/GRAPH_CONTRACT.md` (graph). Directory:
`src/index/mod.rs`. Scalar has no directory: keys in `src/store/scalar_key.rs`,
catalog in `src/collections/catalog.rs`, walk in `src/query/drivers.rs` /
`src/query/cursors.rs`.

### 3.1 Scalar

Key: `0x70 \|\| index_id \|\| encoded_value \|\| sequence`. Posting value is
empty; the entity is the key's sequence. `encoded_value` is kind-tagged so
byte order is value order within a declared kind (`src/store/scalar_key.rs`):
tag 0 missing/null (shared first key), tag 1 bool, tag 2 i64 with sign bit
flipped, tag 3 f64, tag 4 UTF-8 text (binary order, no locale). Unique
indexes refuse a second posting for the same value.

May occupy its own B-tree. Query predicates: `Eq`, `Range` (inclusive bounds),
`IsNull`, `IsMissing` (`src/query/mod.rs` `ScalarFilter`).

### 3.2 Text — `src/index/text/mod.rs`

Analyzer v1 (`src/index/text/analyzer.rs`, tables `src/index/text/unicode_v1.rs`):
Unicode alphanumeric/lowercase, no std Unicode on the runtime path. Contract:
`docs/lang/QL_CONTRACT.md` "Full text".

| Tag | Key | Posting |
| --- | --- | --- |
| `0x75` POSTING | `index \|\| term \|\| 0 \|\| sequence` | u32 BE term frequency |
| `0x76` NORM | `index \|\| sequence` | u32 BE token length; empty value is a tombstone when segments are on |
| `0x77` TERM_STATS | `index \|\| term` | corpus df |
| `0x78` CORPUS_STATS | `index` | documents + tokens, 16 bytes |

Packed segment tier (`src/index/text/segments.rs`, `SEGMENT_FEATURE`):
`0x7A` packed postings, `0x7B` packed 256-slot norm blocks. A head row
overrides a packed block; `tf = 0` is the posting tombstone. BM25 (`k1=1.2`,
`b=0.75`, version 1) ranks; `TextMatch::{Any, All, Phrase}`. Phrase membership
is refined from the primary text.

On **50,000 rows, this Mac, 2026-09-20**, `text_one` median 1,065.625 µs (E4)
vs 8,153.083 µs (Postgres), 930,877 matching postings counted, AGREE;
`text_top10` overlap 3/10 because BM25 ≠ `ts_rank_cd`.

### 3.3 Point — `src/index/spatial/point.rs`

Contract: `docs/core/SPATIAL_FUNCTIONS.md`. Key:
`0x74 \|\| index_id \|\| cell:u32be \|\| sequence`. Posting: 16 bytes, longitude
and latitude as f64 LE. Cell is a 16-bit Hilbert value on WGS84
(`GRID_BITS=16`, `METRIC_KARNEY_V1`). The posting carries the exact point, so
bbox/radius/nearest predicates are decided from the posting; a row is not
required to certify the filter (`src/query/membership.rs`).

`NearestWalk` is an outward-ring cover (start 250 m, growth ×4, 8 ranges per
ring) yielding `(distance_m, id)` order. May occupy its own B-tree.

On **50,000 rows, this Mac, 2026-09-20**, `pt_radius` 47.750 µs vs 1,756.750 µs,
84,140 rows, AGREE; `knn_10` 62.042 µs vs 1,202.417 µs, overlap 10/10.

### 3.4 Geometry — `src/index/spatial/geometry_index.rs`

Contract: `docs/core/SPATIAL_FUNCTIONS.md`. Key:
`0x7c \|\| index_id \|\| level:u8 \|\| cell:u32be \|\| sequence`. Posting: 16-byte
`BoxF` (outward-rounded f32 bbox of the geometry, not of the cell). A geometry
covers at most `MAX_CELLS` (8) cells at the finest of three ladder levels
(`LEVEL_FINE` / `LEVEL_COARSE` / `LEVEL_WORLD`) that fits. Write cost is O(1)
postings per geometry. A dateline-crossing bbox falls through to the world
bucket (sound, pessimistic).

The `BoxF` admits a candidate; the predicate is refined from the row through
`src/index/spatial/geometry.rs` (`Intersects`/`DWithin` spheroidal,
`Within`/`Contains` planar — `docs/lang/QL_CONTRACT.md` §4.4 and
`docs/core/SPATIAL_FUNCTIONS.md`). Geometry postings do not certify the filter
without a row.

On **50,000 rows, this Mac, 2026-09-20**, `plot_intersects` 1,224.417 µs vs
3,627.791 µs, 35,189 rows, AGREE; `plot_dwithin_1km` 36.708 µs vs 1,370.042 µs,
289 rows, AGREE.

### 3.5 Exact vector — `src/index/vector/exact.rs`

Contract: `docs/lang/QL_CONTRACT.md` "Exact vector". Locator key:
`0x73 \|\| index_id \|\| sequence`. Locator value: 6 bytes, layout id u32 BE +
field ordinal u16 BE. Lanes stay in the `0x60` sidecar. Metrics: `Cosine`,
`SquaredL2`, `NegativeDot`. Search is a bounded scan of locators then sidecars
(`src/query/vector_scan.rs`); there is no graph.

On **50,000 rows, this Mac, 2026-09-20**, `vec_exact_10` 2,992.250 µs vs
9,006.292 µs, overlap 10/10. Postgres has no exact vector index on this run
(sequential scan).

### 3.6 Quantized vector — `src/index/vector/quantized.rs`

Contract: `docs/lang/QL_CONTRACT.md` "Approximate vector". Companion to exact, not
a replacement. Key: `0x79 \|\| index_id \|\| sequence`. Value: 6-byte locator
plus symmetric-int8 codes (`QUANTIZER_VERSION=1`, `src/index/vector/quant.rs`).
Scan produces an `ef`-bounded shortlist; rerank reads the authoritative f32
sidecar. Method name: `SymmetricInt8ScanV1`. It is not HNSW/DiskANN; those
spellings are T2 aliases of this family (`docs/lang/QL_CONTRACT.md` §4.5).

On **50,000 rows, this Mac, 2026-09-20**, `vec_ann_10` at `ef=20` recall 1.000,
median 2,565.0 µs; Postgres `diskann` at `search_list_size=100` + rescore 400
recall 0.788, median 3,610.4 µs. `ef` and `search_list_size` are not the same
knob; both sides sweep.

### 3.7 Graph — `src/index/graph/mod.rs`

Contract: `docs/core/GRAPH_CONTRACT.md` §§1–3, 6. Feature bit `GRAPH_FEATURE=2`.
Not an `IndexFamily`; adjacency is primary storage, not an index over rows
(§2.2).

| Tag | Key | Posting |
| --- | --- | --- |
| `0x71` PRIMARY | `source \|\| context \|\| type \|\| destination` | JSON property bag |
| `0x72` REVERSE | `destination \|\| context \|\| type \|\| source` | empty marker |
| `0x06` header | two replicas | next type/context, counts |
| `0x07` / `0x12` | name descriptor / lookup | interned type and context names |

The adjacency of one source in one context and type is one contiguous range.
The reverse mirror is always written. Edge types and named contexts intern on
first `link` / `create_edge_type` / `create_graph_context` (max 4,096 names,
128 bytes each). Properties are an undeclared JSON bag, rewritten in place on
`put_edge` of the same pair (§2.6). There is no edge-id segment and no
declared typed-property descriptor at HEAD (§2.3, §2.4 remain decided, not
on disk). Parallel edges of the same type between the same pair collapse:
`link` replaces the posting.

Deletion of a node cascades incident edges in every context, bounded at 256
(`MAX_CASCADE`); a 257th incident edge refuses before any published change.
RESTRICT as the default delete (§6.1) is not implemented. There is no
`drop_context` range delete (§6.3).

## 4. Query engine

Contract: `docs/lang/QL_CONTRACT.md` "Contract and public shape", "Execution
pipeline", "Shared work and cancellation", "Family hooks", "Scalar semantics".
Vocabulary: `src/query/mod.rs`. Prepare: `src/query/plan.rs`. Drivers:
`src/query/drivers.rs`. Cursors: `src/query/cursors.rs`. Membership:
`src/query/membership.rs`. Per-candidate tests: `src/query/filters.rs`. Rank:
`src/query/rank.rs`. Score: `src/query/score.rs`. Pages: `src/query/page.rs`.
Row reads: `src/query/rows.rs`. Vector scans: `src/query/vector_scan.rs`.

### 4.1 Request shape

```
QueryRequest { collection, filters, order, projection, total_limit, driver }
```

Filters are AND (`MAX_FILTERS=64`). One order. `CandidateDriver::{Auto,
Entities, Filter(i), Order, Keys}`. Projection is `Ids` or named fields
(`MAX_PROJECTION_FIELDS=64`). `prepare_query` refuses a missing, non-READY,
wrong-family or wrong-collection index, a descending `Distance` order, a
`Key` filter without `CandidateDriver::Keys`, and a Score tree deeper than 32
or with more than 8 leaves.

`QueryFilter`: `Scalar`, `JsonEq`, `Graph(BfsRequest)`, `Point`, `Geometry`,
`Text`, `Key`. `QueryOrder`: `EntityId`, `Scalar`, `ExactVector`,
`ApproximateVector { ef }`, `Bm25`, `Driver`, `Distance`, `Score { expr }`.

### 4.2 Drivers

`DriverPlan` (and the `QueryDriver` a page reports): `Entities`, `Scalar`,
`Graph`, `Spatial`, `Nearest`, `Geometry`, `Text`, `ExactVector`,
`QuantizedVector`, `Keys`. Auto picks a driving clause; `nearest_drives_better`
and `order_index_drives_better` prefer a walk that is already in rank order.
A vector driver has no `DriverKey`: its walk is locator order and its answer
is distance order, so `QueryOrder::Driver` does not ride it.

Each cursor's `next` lives in `src/query/cursors.rs`. Graph traversal as a
driver/filter is `docs/core/GRAPH_CONTRACT.md` §4.5.

### 4.3 Membership sets

A non-driving scalar range or point filter can be collected once into a
`MembershipSet` (`Ids` vec, `Bitmap`, or `Overflow` when even a bitmap would
exceed the page-memory cap). Equality is a point get on `value \|\| entity`,
not a set. A point posting carries coordinates, so a built point set certifies
the filter without a row. A geometry posting does not. Overflow falls back to
row reads (Law 4: a predicate too wide for the budget is no faster than it
was, rather than holding an unbounded set).

### 4.4 Per-candidate filters

`filters_match` / `batch_filters_match` (`src/query/filters.rs`) apply every
filter the driver did not certify. Scalar/text/point/JSON compare the posting
or the decoded field. Geometry refines through `spatial_geometry`. Graph
membership is the BFS result set intersected with the request collection.

### 4.5 Single order and Score

One order key, ties broken by `EntityId`. Two keys are a refusal
(`docs/lang/QL_CONTRACT.md` dialect deviation 3). `ScoreExpr` leaves: `Lit`,
`Scalar`, `Bm25`, `VectorSimilarity` (`-distance` of the exact sidecar),
`Distance` (geodesic metres); combinators `Add`/`Sub`/`Mul`/`Div`/`Neg`.
Division by zero is `NaN`; `NaN` sorts last under both directions. Score
never drives: it ranks every candidate the filters admit.

On **50,000 rows, this Mac, 2026-09-20**, `hybrid_10` 831.750 µs vs
143,266.334 µs, overlap 10/10; `hybrid_blend_10` 849.167 µs vs 2,661.875 µs,
overlap 7/10 (BM25 vs `ts_rank_cd`, compared on overlap, not order).

### 4.6 Pages, resume, budgets

`PreparedQuery::next_page` (`src/query/page.rs`) emits up to `page_size`
(`MAX_PAGE_SIZE=8,192`). When the driver walk is already in rank order, a
page stops when full and the next page resumes at the last `(sort key, id)`.
When it is not, the page keeps a bounded run of already-ranked winners so
later pages do not re-walk (bounded by `RUN_BYTES` / `RUN_ROWS`). Cancellation
or budget exhaustion returns no page and leaves the position unchanged.

`QueryBudget` / `QueryWork` charge candidates, primary reads, scalar/graph/
spatial/text/vector/key postings, text tokens, vector sidecars and lanes,
output bytes. `row_decodes` is counted, not budgeted. Work is proportional to
candidates walked or rows returned except for named scans (exact vector
without a filter; `docs/lang/QL_CONTRACT.md` §6).

## 5. Graph: contexts, edges, traversal

Contract: `docs/core/GRAPH_CONTRACT.md`. Module: `src/index/graph/mod.rs`. Query
hooks: `src/query/drivers.rs` (section 4), `src/query/cursors.rs`.

**Nodes (§1).** A node is a row in any collection. Identity is the external
key. Indexes on that row serve every graph that references it.

**Edges (§2).** Directed, typed, inter-collection, in exactly one context
(empty name = base graph, id 0). Storage is the primary/reverse pair in
section 3.7. `link` interns type and context names. Updating properties
rewrites the posting. Element identity and declared typed properties are
decided in the contract and not present at HEAD.

**Contexts (§3).** A context is an id in the edge key, so one context is one
contiguous range. Contexts own edges only. A traversal runs in one context.
The catalog stores interned names; owner/created/forks/overlays are not
engine features.

**Traversal (§4).** The atomic is budgeted BFS (`BfsRequest`: seed, direction
out/in/both, optional type, min/max depth, visited and edge budgets, result
limit, cancellation). A node is never revisited (GQL ACYCLIC). Caps:
`MAX_BFS_DEPTH=64`, `MAX_BFS_VISITED=65,536`, `MAX_BFS_EDGES=1,000,000`,
`MAX_BFS_RESULTS=65,536`, `MAX_NEIGHBORS=256`. The result is entity+depth;
the reaching edge is not bound (§4.2) and there is no per-hop predicate
(§4.3). Composition (§4.5) is implemented: `QueryFilter::Graph` is a
membership set and a candidate driver, and it conjoins with the other
filters and any single order.

**Paths (§5).** Streamed path accumulators, path aggregates as Score leaves,
and shortest path are not at HEAD.

**Deletion (§6).** `delete_edge` removes both postings. Entity delete
cascades incident edges (bounded). RESTRICT default and context-range drop
are not at HEAD.

## 6. Eight laws

The laws are `CONTRACT.md` / `docs/core/FOUNDATION_TEST_STANDARD.md`. One line
each on how the engine meets it, and what is unqualified.

| Law | How the engine meets it | Unqualified |
| --- | --- | --- |
| 1 Disk-first | Page-WAL, 8 reader slots, WAL index bounded by tracked pages, membership sets and BFS frontier capped; no database-sized RAM oracle. | Process-wide Pi address-space gate is not re-run this month. |
| 2 Cost ∝ change | Ordinary commit writes the transaction's page images, not the collection. Index maintenance is per changed row. Traversal work is the postings of that source/context/type. | Scattered-insert exponent 0.24 (descriptive `log(cost ratio)/log(population ratio)` on the fixed-work 1,000-insert arm). Local insert on the Mac scale table stays 23.28–35.51 ms for 1,000 rows from 10K to 10M population (`docs/FOUNDATION_SCALING.md`). |
| 3 Nothing fallible may delete | Write new page images, CRC, publish after FULL barrier; checkpoint only with every reader slot; failed repair leaves the source; cascade preflights then deletes pairs. | RESTRICT is the contract default (`docs/core/GRAPH_CONTRACT.md` §6.1) and is not the implemented delete. |
| 4 Name your sacrifice | Named: 8 bytes/edge if identity lands; reverse mirror doubles edge storage; geometry ≤8 cell postings; quantized codes plus f32 rerank; membership Overflow; `QueryBudget` counters. | Edge identity (8 bytes) is decided, not on disk. |
| 5 Recoverability | CRC/identity/bounds on frames, cells, packets; corrupt posting is `Corrupt`, not a panic; offset reads bounds-checked; unknown feature refused before mutation. | L5-DAMAGE full matrix is not claimed passed. |
| 6 Readers | Snapshot readers on the published prefix, byte-stable, eight slots, no writer lock on reads. | Cross-process latency/I/O audit is evidence in `tests/collection_pagewal.rs`, not a device qualification. |
| 7 Target usability | Bulk load, late index build, live writes with indexes, reopen are implemented paths (`src/collections/{mod,catalog,rebuild}.rs`). | Pi untested this month. |
| 8 Compatibility | Additive feature bits (`GRAPH_FEATURE`, `TEXT_FEATURE`, `SEGMENT_FEATURE`, `GEOMETRY_FEATURE`, `INDEX_TREE_FEATURE`, …); unknown bits refuse; old files without a bit open with the prior encoding. | `L8-COMPAT` remains PENDING (`docs/core/FOUNDATION_TEST_STANDARD.md`); element identity and typed edge properties are specified as future additive bits. |

Scale table (Mac, 1,000 changes including commit and ending checkpoint, from
`docs/FOUNDATION_SCALING.md`), milliseconds E4 / SQLite:

| Population | Locality | Insert | Update | Delete |
| --- | --- | ---: | ---: | ---: |
| 10K | Local | 23.28 / 27.99 | 22.88 / 25.11 | 60.48 / 36.05 |
| 100K | Local | 23.93 / 26.81 | 24.19 / 25.05 | 28.65 / 25.05 |
| 1M | Local | 25.04 / 27.38 | 25.77 / 28.63 | 30.58 / 27.36 |
| 10M | Local | 35.51 / 25.47 | 31.23 / 28.40 | 57.66 / 27.03 |
| 10K | Scattered | 63.51 / 54.36 | 49.09 / 46.07 | 85.11 / 71.29 |
| 100K | Scattered | 260.95 / 163.70 | 126.59 / 125.39 | 99.38 / 134.97 |
| 1M | Scattered | 209.55 / 461.55 | 87.49 / 115.94 | 77.21 / 134.87 |
| 10M | Scattered | 949.04 / 824.98 | 119.60 / 130.46 | 130.27 / 128.57 |
