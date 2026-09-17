# Phase 2 workload and semantic contract

Status: implementation target, 2026-09-16. This document specifies acceptance;
it does not claim these APIs, indexes or benchmark results already exist.
tracker `phase2` owns live progress. The immutable `e4-format-v1` foundation
and existing collection semantics remain supported. Broad SQL coverage,
network services, wrappers, explicit EXPORT/IMPORT and release packaging
remain product work after this scoped embedded interface.

## Why these queries

app uses the published `sekejap = "0.16.5"` crate
(`../app/Cargo.toml:76`), `CoreDB::open`, bound SQL parameters, typed fields,
affected-row counts, collection/schema introspection and maintenance
(`../app/src/platform/sekejap.rs`). E3 is a source reuse reference, not
proof of compatibility with that consumer or a data migration target.

The read-only app PVC backup dated 2026-09-15 contains
`repo/pipelines/experiments/retrieval/e1/tools/e1-tool-ret-search-data-and-map.zf.json`:

```sql
SELECT data_resource_id FROM e1_gold_data_resource
ORDER BY description_embedding <=> $1 ASC LIMIT 50

SELECT search_hook_id, content, target_id FROM e1_gold_search_hook
ORDER BY embedding <=> $1 ASC LIMIT 12

SELECT data_resource_id, BM25(description, 'flood') AS bd,
       BM25(keywords, 'flood') AS bk
FROM e1_gold_data_resource /* resource facets */
ORDER BY bk DESC LIMIT 200
```

The pipeline joins resource/map/hook results and combines lexical relevance,
vector rank and facet constraints in JavaScript. Phase 2 must support the
underlying combined operation through an embedded API before claiming a SQL
adapter. Backup data is historical consumer evidence, not verified live state.
Full provenance/reuse audit: [PHASE2_E3_AUDIT.md](PHASE2_E3_AUDIT.md); durable summary below.

app ingestion explicitly works around numeric JSON arrays being mistaken
for embeddings. Therefore JSON `[1,2,3]` remains JSON unless a field is
explicitly declared vector. Declared vector payloads are not evidence that an
approximate accelerator is present: the sampled app schemas have empty
`vector_fields` index lists despite vector-typed attributes.

## Semantic choices

These are defaults for the new query layer; they do not reinterpret existing
record bytes. Namespaces/encoding IDs belong to the separate format design.

- Identity is the existing collision-free `EntityId { collection, sequence }`.
  External keys remain exact, collection-scoped UTF-8, including NUL. Compare
  entity IDs for deterministic ties. Never replace identity with a hash.
- `put` replaces/upserts; `update` shallow-merges and errors if absent; delete
  of an absent key returns false. Committed delete/reinsert allocates a new ID.
  Managed timestamps remain off unless explicitly enabled at collection creation.
- Missing and null remain distinct values. Equality/range predicates do not
  match either; explicit `is_missing` and `is_null` distinguish them. Numbers
  compare mathematically without f64 rounding of large integers; booleans and
  strings never coerce to numbers. Text order is UTF-8 byte order, case-sensitive.
  Mixed-type range bounds are refused. Structured JSON equality is structural;
  unsupported JSON index operations return an explicit unsupported error.
  The low-level scalar candidate API deliberately has a null/missing bucket:
  `ScalarPredicate::Eq(null)` returns both; an unbounded range includes it.
  The combined query layer must post-filter this bucket to implement the above
  predicate semantics. This is not SQL equality-to-NULL behavior.
- Graph relationships are authoritative user data. The first graph surface is
  a directed typed relationship unique by `(context, source, type, destination)`;
  upsert replaces its properties, and different contexts permit distinct
  assertions. Context zero is the default. This does not promise parallel edges
  with the same tuple. Endpoints must exist when committed; entity deletion
  removes incident relationships atomically. Incoming traversal requires a
  maintained reverse index; if disabled, refuse it explicitly. Traversal returns
  unique entities, excludes the seed unless requested, applies an inclusive
  depth interval and terminates on cycles. A limit/cursor is part of the request.
- Vector dimensions are declared per collection/field, raw lanes remain f32,
  and non-finite lanes/dimension mismatch are refused before writes. Default
  search is exact cosine distance `1-dot/(norms)`, ascending. A zero query is
  invalid; zero stored vectors remain valid data but are excluded from cosine
  results. Explicit squared-L2 and negative-dot distances are separate choices.
  Approximate search is an explicit mode with algorithm/version/effort and
  measured recall; never silently substitute it for exact search or call the
  E3 disk navigation algorithm HNSW. Null/missing vectors are excluded.
- Spatial queries interpret an explicitly selected point/geometry field as
  WGS84 `[longitude, latitude]` degrees; radius/distances use metres. Refuse
  non-finite or out-of-range coordinates for an indexed WGS84 field. Ordinary
  pre-existing Point records remain decodable. Bounding boxes are candidate
  filters; exact geometry decides membership. A rectangle query includes its
  boundary and can explicitly cross the dateline. Radius membership is `<=`.
  Polygon predicates must specify their boundary convention before exposure.
- Initial text analyzer is Unicode-alphanumeric tokenization and lowercase,
  no stemming/stopwords/accent folding, following E3 `kernel/src/text.rs`.
  Pin analyzer version and Unicode behavior in permanent fixtures. Corpus
  scope is collection+field. Null/missing fields are absent; an explicit empty
  string is a document of zero tokens. BM25 uses K1=1.2, B=.75 and positive
  `idf=ln(1+(N-df+.5)/(df+.5))`; sum once per distinct query term with the
  `(K1+1)` numerator. Empty query returns no matches; ranking is descending,
  ties by EntityId ascending. Other analyzer/scoring semantics need explicit
  versioned creation, never implicit reinterpretation on upgrade.
- A combined exact query applies graph/scalar/spatial constraints before its
  exact ranked top-k. Filtering a global top-k afterward is not equivalent.
  Each query uses one snapshot, even when traversing and fetching payloads.
  Bounded work exhaustion returns an explicit error or explicitly identified
  partial result; it must not masquerade as a complete exact answer.
- Existing transaction error/uncertain-commit rules in [COLLECTIONS.md](COLLECTIONS.md)
  apply to entity, relationship, index and catalog writes together. Index
  declaration is separate from build/publish readiness. A failed build cannot
  publish incomplete candidates. Drop cannot resurrect after reopen.

Costs to report: reverse adjacency duplicates references; each live index adds
write/disk cost; exact vector search can scan the candidate set; full text has
corpus statistics; snapshots retain old pages; index builds need temporary
space. No latency or disk ceiling is claimed merely by this contract.

## Tiny deterministic independent fixture

Create collections `people` then `organizations`; insert each in listed order.
Capture returned IDs rather than hard-coding allocator numbers. User names and
external keys are not IDs. All positions are points; vector dimension is two.

| key | age | active | body | vector | position | organization |
|---|---:|---|---|---|---|---|
| p0 | 20 | true | flood flood river | [1,0] | [0,0] | o0 |
| p1 | 30 | true | flood road | [1,1] | [1,0] | o0 |
| p2 | 30 | false | river road | [0,1] | [0,1] | o0 |
| p3 | 40 | true | forest | [-1,0] | [2,2] | o1 |
| p4 | 50 | false | null | [0,-1] | [3,3] | o1 |
| p5 | 60 | true | empty string | [1,0] | [0,0] | o1 |

Each person also contains `profile={"codes":[1,2,3],"nested":{"enabled":true}}`;
put literal null and missing fields in separate extras. Organizations o0/o1
are named Council/Agency. Add `member_of` edges as the last column describes.
Add `knows` edges `p0->p1, p0->p2, p1->p3, p2->p3, p3->p0, p4->p5`.
Store the same `p0` external key in organizations to test collection scoping;
it is a third organization entity, not an alias to the person.

Expected answers derived directly from this table, without engine helpers:

| Request | Expected external keys |
|---|---|
| active and age >=30, ordered age then ID | p1,p3,p5 |
| outgoing knows from p0 | p1,p2 |
| knows depth1..2 from p0, unique, excluding seed, ID order | p1,p2,p3 |
| incoming knows to p3 | p1,p2 |
| members of o0 | p0,p1,p2 |
| rectangle [0,0]..[1,1], boundary included | p0,p1,p2,p5 |
| exact cosine to [1,0], k=3 | p0,p5,p1 |
| knows depth1..2 + active + rectangle [0,0]..[1,1], cosine k=2 | p1 |
| text flood (positive matches only) | p0,p1 |
| active + text flood + cosine to [1,0], k=2 | p0,p1 |

Independent text oracle: N=5, lengths `[3,2,2,1,0]`, average length 8/5,
`df(flood)=2`; score each matching row with the documented formula above;
p0 has tf=2 and p1 tf=1. Compare scores with a stated floating-point tolerance,
IDs exactly. The oracle must not call E4/E3 tokenizer, scorer or index encoder.
For this ASCII corpus, split/lowercase in the test language is sufficient;
separate fixed expected-token cases cover Unicode.

Mutation sequence: change p1 age to35/body toforest/vector to[0,1]/point to[5,5];
remove p2; replace p0->p1 with p0->p5; reinsert p2 under the same external key.
Update the reference map and edge set transactionally after each commit.
Reinserted p2 has a new ID and does not inherit deleted edges. Verify old
snapshot against the old map and reopened/new snapshot against the new map.
Include an aborted transaction containing all these families and invalid
vector input, then verify no partial publication. Delete/reinsert a text
match through a fold/rebuild/reopen cycle to catch stale tombstones.

## Scale fixtures and measurements

Use fixed generator version `phase2-synthetic-v1`, explicit N and seed.
People key `person/<i>`, organization key `org/<i%100>`; age `18+i%70`, active
`i%3!=0`, JSON numeric array and nested object as above; point
`[144+(i%1000)/10000,-38+((i/1000 floor)%1000)/10000]`.
Text repeats a fixed 16-token vocabulary with term multiplicity determined by
`i%4`; document generation must be shared input, never separate E4/SQLite RNGs.
Vector lane j is `(((i+1)*(j+3)+17*j)%257-128)/128` narrowed once to f32;
32 dimensions in scaling cases, plus an explicitly separate 1536-dimension
bounded sample for app-size embeddings. Add member_of and two directed
knows edges from each person to `(i+1)%N` and `(i+7)%N`. Count entities and
relationships separately. No benchmark result exists yet for these fixtures.

Run tiny correctness, then N=10K,100K and representative1M on isolated Linux.
Capture raw input digest and selected query seeds. An independent reference
map/edge set/brute-force math is fine for tiny tests; for large tests stream
reference rows with bounded heaps and use external sorting/SQLite, never
require the engine to materialize the corpus in RAM. Correctness oracles may
be slower than the measured engine and run outside timed regions.

For each N report: bulk import without indexes; late index builds by family;
live indexed writes; reopen; single-family and combined-query latencies;
individual update/delete/insert stages; at least three mixed CRUD cycles;
no-reader, short-reader and held-reader cases. Pin durability, commit batch,
cache/memory allowance, payloads, vector precision and index coverage. State
units and include E4 and SQLite side by side, with absolute times and ratios.

Disk report includes total data+WAL+metadata+temporary build/rebuild files,
logical and allocated bytes, peak during the run and final retained bytes;
state sampling interval and missed-peak limits. Report loaded size, live count,
peak/loaded factor, final/live density, RSS and refused operations. A compact
final file does not excuse an unsafe temporary expansion. Explicit rebuild
results are separate from normal-operation results.

SQLite baseline uses scalar B-trees, relational edge tables with matching
forward/reverse indexes, JSON representation explicitly stated, FTS5/RTree
only if available and configured, and identical exact vector math over stored
f32 blobs. Report SQLite version/extensions/PRAGMAs. Native SQLite has no
matching built-in ANN algorithm; don't invent a direct ANN ratio. FTS5 ranking
is not automatically the same BM25 corpus/tokenization convention: use common
semantics/oracles or label the differing capability. Correctness comparisons
must not penalize SQLite for a feature omitted from its schema.

Time near 1.75x SQLite for writes can be acceptable if measured multimodel query
benefits justify it; it is not an unconditional pass for every operation.
No arbitrary new veto threshold is introduced here. Record query gains and
memory/disk tradeoffs, reject regressions without product benefit, and keep
reliability/compatibility gates mandatory. Repeat comparative timings at least
three times, alternating engine order; publish medians and spread.

## Acceptance matrix (tracker IDs)

| Work | Evidence required before Done |
|---|---|
| p2-workload-contract | Consumer/reuse audit, semantic choices and independent deterministic fixture specification in this document; executable fixture/oracle proof before query acceptance. |
| keyspace-layout, p2-index-catalog | Collision-free family/collection/field identity, version/refusal, lifecycle and reopen tests; frozen foundation fixtures unchanged. |
| p2-atomic-index-maintenance | Entity+all affected postings/edges atomic under validation failure, rollback, snapshot and process kill. |
| p2-scalar-index | Exact numeric/text boundaries, null/missing, repeated CRUD, indexed-vs-independent scan equivalence. |
| p2-graph-storage, p2-graph-traversal | Both directions, properties/context isolation, cycle/depth/dedup, endpoint deletion, degree/resource bounds. |
| p2-vector-index | Exact independent top-k, field dimensions, explicit approximate recall/effort, update/delete/reopen. |
| p2-spatial-index | Named field, exact refinement, boundary/axis/metre tests, dateline/invalid inputs, no candidate false negatives. |
| p2-fulltext-index | Fixed analyzer/ranking, corpus isolation, statistics, empty/missing/null, delete/reinsert/fold/reopen. |
| p2-query-interface, p2-combined-query | Usable embedded typed requests/results/errors/limits, supported E3 examples, one-snapshot mixed queries, deterministic independent answers. |
| p2-index-compatibility | Preserved previous/new binary fixtures for every shipped family; old indexes remain readable/writable, no implicit rebuild or format promotion. |
| p2-index-recovery | Corrupt each catalog/posting/authoritative relationship family; source preservation, explicit damage and recovery limits, verified rebuild only for derived data. |
| hybrid-storage | 10K/100K/representative1M matched benchmark tables covering query/CRUD/memory/peak+final disk; artifacts and reproducible commands retained. |
| p2-acceptance | All scoped evidence above, working app-like example, limitations and public-release followups, accepted commits without co-author. |

## Reuse and remaining consumer uncertainty

Port E3 small algorithms from `kernel/{graph,text,spatial,geomath,vecquant,nav,score}.rs`
and exact scalar comparison from `src/db.rs`; adapt them to E4 transactions and
typed accessors. Do not import E3 pager/WAL or hashed identity. In particular
E3 `index_reg_hash_for` omits collection for vector registry keys although its
vector payload field IDs include collection; avoid this asymmetry. Its
`src/vector.rs::HnswGraph` is an unreachable facade, not an implementation.
Historical DEFECTS.md scenarios are regression inputs, not proof of current bugs.

The first embedded request surface needs collection/field references, index
create/build/drop/list, typed equality/range, directed/depth traversal, exact
and explicit approximate vector search, spatial filtering, text search and
combined top-k plus limits. Build on the existing Database API; exact Rust
names are settled by implementation and executable examples, not pseudocode
that users might mistake for available methods.

Still to confirm from consumers before broad compatibility claims: production
embedding dimensions/model normalization, required multiedge semantics, spatial
geometry families/CRS and dateline usage, multilingual analyzer expectations,
SQL range/date behavior clients rely on, and acceptable query budgets/recall.
Synthetic fixtures above allow implementation to proceed without guessing that
every historical E1 quirk is required. Unsupported capabilities must stay
explicit until implemented and tested.
