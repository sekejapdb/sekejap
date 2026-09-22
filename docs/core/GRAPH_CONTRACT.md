# Graph contract — sekejap Phase 2

Decided 2026-09-20 with the owner. This document states what the graph IS and
how a traversal behaves. It states no syntax: the query language (SGQL, the
strict SQL + GQL split, joins on or off, the spelling of MATCH) is Phase 3 and
compiles onto the atomics named here. Every rule below is pinned by a test
before the item that implements it lands.

The lens for every decision: a hop is one contiguous read, and a traversal's
result plugs into the same filters and the same Score order as every other
index. Hybrid multimodel queries and scoring are the speciality; the graph is
native, not a layer declared over tables.

## 1. Nodes

1.1 A node is a row in any collection. It exists whether or not an edge touches
    it. Its fields and their indexes (scalar, text, point, geometry, vector)
    serve every graph that references it.
1.2 Node identity is the external key. Keys are opaque, ordered strings; a
    hierarchy such as `mit/john/derivative` is an application convention the
    engine never parses, though a key prefix is listable as one ordered range.
1.3 Whether a concept is shared or private is a business rule expressed in the
    key, never an engine rule.

## 2. Edges

2.1 An edge is directed and typed and references two entities in any two
    collections (inter-collection since the first graph commit). It lives in
    exactly one context; no context means the base graph.
2.2 Storage is native: the edge keyspace `source, context, type, destination[,
    edge id]` with the properties inline in the posting, and the reverse mirror
    always written. The adjacency of one source in one context and type is one
    contiguous key range. This is the primary storage, not an index over rows.
2.3 Element identity (DECIDED): the key carries an edge id segment under an
    additive feature bit. Every insert creates a new edge; parallel edges of the
    same type between the same pair coexist in the same posting range. Files
    without the bit open with implicit ids. Cost: 8 bytes per edge, nothing per
    hop. Reason: scoring weighs how many and how strong the edges are; set
    semantics destroyed that at write time.
2.4 Properties (DECIDED): the property bag stays; an edge type MAY declare
    typed properties (`prereq(weight REAL, agree INT)`). Declared properties are
    encoded fixed-width in the posting and read by offset; undeclared ones live
    in the bag. The declaration is an additive descriptor; no format change for
    undeclared types. Reason: a predicate or a Score leaf on a declared property
    runs at scan speed, the same loop the vector scan uses.
2.5 Edge types are interned on first use. The catalog's edge-type rows (which
    collections an edge type connects) are derived from written edges, so a
    tool sees the graph shape without any declaration.
2.6 Updating an edge's properties is an in-place rewrite of its posting.
2.7 ENDPOINT SETS (SHIPPED, additive `ENDPOINT_FEATURE = 0x4000`, keyspace
    tag `0x7E`): a DERIVED keyspace holding one key per DISTINCT entity that
    has at least one edge of a given (context, type, direction), keyed
    `tag | context | type | direction | entity`. It answers "which entities
    have such an edge" -- the semi-join `EXISTS (SELECT 1 FROM t WHERE
    t.source = c._key)` -- in one contiguous range with one posting per
    entity, instead of the edge walk's one seek per matched entity. It is
    maintained on the write path: a NEW edge files both its ends, a REMOVED
    edge gives up an end only when that entity's LAST edge of that (context,
    type, direction) goes, proved by one bounded range probe of the edge
    keyspace per end. A file whose edges predate the sets carries none,
    declares nothing, and takes the walk until
    `Database::backfill_endpoint_sets` builds them once. The edges stay
    authoritative; the set is rebuilt from them, never the other way round.

## 3. Contexts

3.1 A context is a named graph: an id in the edge key, so one context is one
    contiguous range that can be listed, copied or dropped as a range.
3.2 Contexts own edges only. Nodes are outside every context.
3.3 A traversal runs in one context (or the base graph). Reading a property
    from a second context is a follow-up lookup, not part of this phase.
3.4 A context descriptor (name, owner, created) is catalog data. Versions,
    forks and overlays are application business over contexts, not engine
    features.

## 4. Traversal

4.1 The atomic is a budgeted BFS: seed(s), direction (out, in, both), edge type
    or all, minimum and maximum depth, visited and edge budgets, result limit,
    cancellation, pageable. A node is never revisited (GQL ACYCLIC is the only
    path mode in this phase).
4.2 The traversal binds the reaching edge to each result and can read its
    properties. BUILT (order-of-work item 1): `TraversalNode::via` carries the
    edge, `Projection::Fields` accepts `"@edge.<property>"` and
    `QueryOrder::Edge` ranks by one. A node reachable over several edges
    reports the FIRST one the walk admitted -- edges are offered to a level in
    posting order, outgoing before incoming, and the level keeps the first
    offer per entity. An INCOMING hop's properties come from the primary
    posting of the same edge, because the reverse posting is a marker; that is
    one edge-keyspace point read per candidate edge, never a row -- counted
    in `scanned_edges` and bounded by the edge budget like every other read
    in that keyspace.
    Reading the edge requires the traversal to DRIVE: the reaching edge is
    carried by the traversal's own candidate stream and by nothing else, so a
    query that projects `@edge.<property>` or ranks by the edge under any
    other candidate driver is REFUSED when it is prepared, with the driver
    named. A key-range predicate beside such a query is therefore a
    post-filter, answered from the external key the row carries in its own
    first field, and costs a primary read per candidate.
4.3 Per-hop predicates (DECIDED): a predicate on an edge property or on a node
    field is evaluated as the frontier expands; a failing edge is never
    followed, a failing node is never expanded. Edge predicates read the inline
    posting; node predicates on indexed fields use index-side membership sets.
    A traversal never reads a row for a predicate on a covered field.
    Post-filters on completed matches are a separate, later stage.
    BUILT (order-of-work item 1): `BfsRequest::edge_where` is a conjunction
    over the inline bag, decoded once per edge visited; `BfsRequest::node_where`
    is a conjunction of query filters restricted to what an index answers
    without a row -- a scalar equality (one posting probe per node), a scalar
    range or a point predicate (one membership set, built once per prepared
    query). Every other filter kind is REFUSED when the traversal is prepared,
    with the reason, and a node set that outgrows its memory budget is refused
    rather than falling back to the row path. The SEED is not tested against
    the node predicates: it is named by the caller, not found by a hop.
    A node predicate is tested at most ONCE per distinct entity for a whole
    traversal: an admitted node joins the visited set with its level, and a
    refused one joins a refused set beside it, so no later edge -- at that
    depth or a deeper one -- probes it again. A membership set that outgrows
    its memory budget is a BUDGET refusal naming the resource it overran
    (`BudgetExceeded`), on the query path and on the traversal atomic alike,
    never prose and never a row-read fallback.
    Work: `graph_edges` counts every edge decoded, a pruned one included --
    §4.3 prunes the frontier, not the reading; `graph_visited` counts every
    node admitted.
4.4 One predicate set applies to every hop in this phase; per-hop patterns
    (typed multi-hop with different predicates per hop) are Phase 3 surface
    over the same atomic.
4.5 Composition: the traversal is a query filter and a candidate driver, so it
    conjoins with scalar, text, point, geometry filters and with any single
    order (EntityId, Scalar, Distance, Bm25, ExactVector, ApproximateVector,
    Score). Seeds may come from another order (e.g. the k nearest vectors).

## 5. Paths

5.1 Paths are streamed (DECIDED): the traversal keeps one accumulator per
    frontier entry — length and any nominated aggregate over a declared edge
    property (sum, product, min, max, average, first, last). The full path
    (nodes and edges) is reconstructed by parent pointers only for rows a page
    returns. Memory is bounded by the visited budget.
5.2 A path aggregate is a Score leaf, so `0.5*cosine + 0.3*bm25 +
    0.2*path_product(weight)` is one order.
5.3 Shortest path is its own atomic: ANY SHORTEST and ALL SHORTEST between
    seeds, bidirectional under the same budgets. Unweighted first; weighted by a
    declared property later.

## 6. Deletion

6.1 (DECIDED) Two atomics: RESTRICT refuses to delete a node while any edge in
    any context references it and names those contexts (one reverse-index seek
    per context); CASCADE removes the node's edges in every context through the
    existing bounded cascade. RESTRICT is the default. Reason: a delete that
    silently removes edges from another user's graph is a fallible delete
    (Law 3).

    BUILT at collection granularity (`begin_drop_collection`): the edge key is
    `tag | collection | sequence | context | type | far endpoint`, so a context
    cannot be a key prefix at that granularity and the probe is one range per
    edge tag instead, seeking past each (entity, context) run it has already
    recorded. That is one descent per distinct (entity, context) pair with
    edges, capped, and two descents for a collection with none. Single-row
    `delete(key)` still cascades; RESTRICT there is not built.
6.2 Deleting an edge removes its forward and reverse postings in its context.
6.3 Dropping a context removes its range; nodes are untouched. Dropping a
    COLLECTION is the mirror of it and is built: the rows go, and the edges on
    them go only under CASCADE, in every context.

## 7. Laws

L1 A traversal holds the frontier, the visited set and one accumulator per
   visited node, all bounded by the visited budget; never RAM proportional to
   the graph. It also holds the entities its node predicates REFUSED, so each
   is probed once (§4.3); a refused entity was reached by at least one edge
   the walk charged, so that set is bounded by the edge budget.
L2 Work per hop is the postings of that source in that context and type; a
   whole-graph pass never occurs inside a traversal.
L3 RESTRICT is the default delete; CASCADE is explicit and bounded.
L4 Sacrifices named: 8 bytes per edge for identity; a declared property is
   stored fixed-width in the posting (bytes per edge per declared property);
   the reverse mirror doubles edge storage. Per-hop predicates add: 12 bytes
   per FRONTIER ENTRY (a reaching-edge pointer and the offer order that makes
   "first admitted" deterministic), still bounded by the visited budget; 12
   bytes per REFUSED entity, bounded by the edge budget; and, on an INCOMING
   hop that reads a property, one primary-posting point read per candidate
   edge, because a reverse posting is a marker -- so such a hop spends up to
   twice the edge budget of the same hop that reads no property, and refuses
   rather than exceeding it.
L5 A corrupt posting or bag is a Corrupt error on that edge, never a panic; all
   offset reads are bounds-checked.
L6 A traversal reads its snapshot; a concurrent writer is invisible to it.
L8 Element identity and typed properties are additive feature bits; old files
   open unchanged.

## 8. Deferred, with the reason

- Columnar fast lane as a hidden second store, as the prior engine had: replaced by 2.4.
- Edge-property indexes: allowed by the model, not built until a measured
  query needs one.
- Per-edge-type property schemas beyond 2.4, multi-context traversal, weighted
  shortest path, TRAIL/WALK path modes, versions/overlays: not in Phase 2.

## 9. Order of work

1. 4.2 + 4.3: edge binding and per-hop pruning (no format change). BUILT.
2. 2.4: typed edge properties (additive descriptor).
3. 5.1–5.3: streamed paths, path aggregates as Score leaves, shortest path.
4. 2.3: element identity (feature bit; its own commit).
5. 6.1: RESTRICT and CASCADE.

Each item: one Opus worker in its own worktree, a Grok oracle suite over random
graphs with a brute-force reference, an Opus review before merge, counted tests
that pin work proportional to edges walked.
