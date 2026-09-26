# GQL query profile: engineering design for milestones M1-M4

Status: BUILT; the reference is `docs/lang/GQL_PROFILE.md` and the feature
registry `docs/lang/GQL_FEATURES.md`. This document is kept as the design
record: where the built profile differs from it, the reference and
`QL_CONTRACT.md` are right. Nothing here is Tier 1 unless a named test proves
it (`docs/lang/QL_CONTRACT.md`, "The tiers"). It was written so that
implementers can split the work without redesigning it, and it marks every
point that is uncertain.

Source of the requirements: the owner's GQL profile brief, v0.1, which is not
kept in this repository. Citations of the form "brief §8.3" refer to its
section numbers. Milestones M1-M4 are its §11 steps 1-4:

| Milestone | Brief §11 step | One-line goal |
| --- | --- | --- |
| M1 | 1 | Values, identity, binding schema, graph null semantics, output encoding |
| M2 | 2 | General patterns: seeds, multi-hop `Expand` / `ExpandInto`, every bound variable projectable, label alternatives, repeated variables |
| M3 | 3 | Working-table composition: expressions, `LET`, `FILTER`, `RETURN`, `NEXT`, grouping, multi-key sort, `OFFSET`/`LIMIT`, `FOR` |
| M4 | 4 | Paths: quantifiers including zero length, `WALK`/`TRAIL`/`ACYCLIC`, named paths and path functions, horizontal aggregation, `ANY`, `ANY SHORTEST`, `ANY CHEAPEST ... COST` |

Out of scope here (brief §11 steps 5-7): `EXISTS`/`NOT EXISTS`, `CALL`,
`UNION`, index lineage for text/spatial/vector seeds, and the published
feature registry. Where an M1-M4 decision constrains that later work, this
document says so. One exception is proposed in open question Q3: `OPTIONAL
MATCH` may need to move into M3.

What this document does NOT claim: ISO/IEC 39075 conformance, Google Spanner
compatibility, or PostgreSQL conformance. The profile adopts selected ISO GQL
constructs, Google's documented `GRAPH_TABLE ... RETURN` embedding and its
`ANY CHEAPEST ... COST` extension, and sekejap's existing PostgreSQL-style
host surface (brief §3). `ANY CHEAPEST` is a Google-reference addition, not
ISO GQL.

Workload names in this document are invented: band `b1`, person `p1`, topics
`t1`...`t9`.

**Owner decisions that supersede parts of this document (2026-09-25):**

1. **No legacy body.** The SQL/PGQ `GRAPH_TABLE (g MATCH ... COLUMNS (...))`
   form is NOT kept as a compatibility profile. The GQL body replaces it. The
   legacy parser and compiler path, its refusals and its tests are removed in
   M2 (task M2-E below) once GQL tests pin every property the old tests
   pinned. Every "legacy profile", "unchanged legacy" and "body-kind
   pre-scan" statement in sections 0, 2.4, 4.1, 5.1, 5.4, 9, 10 and Q15 is
   superseded by this: a `GRAPH_TABLE` body is parsed as GQL only.
2. **Nothing unused.** No dead code, no compatibility shims, no speculative
   helpers, aliases or planned spellings. A temporary item (such as a stub
   type until the real one merges) is deleted in the same milestone.
3. **No new crates or top-level folders** (section 1.2).

---

## 0. The decisions in brief

1. **Layering.** `core/engine` gets a new module `query/gql/` holding binding
   values, the operators, their state, the budgets and the path searches.
   `lang` gets a new module `gql/` holding the AST, the parser, the binder
   (scopes, slots, types), the expression IR and its evaluator, and the
   planner. The two meet through ONE trait, `GqlHost`, that core calls back
   into for expression evaluation and index-candidate seeds. Section 1.
2. **One row shape.** A `BindingRow` is a vector of typed slots
   (`BindingValue`: scalar, list, node, edge, path). A node is its `EntityId`;
   an edge is its stored key plus the new `EdgeId`; a path is a shared-prefix
   list of steps. No row claims an `EntityId` owner. Section 2.
3. **Execution.** Pull-based, one row at a time, pipelined within and across
   stages. Blocking operators (group, distinct, sort) hold state under
   memory ceilings that `QueryBudget::unlimited()` does not lift. There is no
   spill and no keyset resume across a re-prepare in v1: an answer pages from
   ONE live execution, which holds one snapshot borrow. Section 3.
4. **Paths.** The pattern compiles to an automaton. Enumeration walks
   `(node, automaton state, counter)` with the path held as a shared-prefix
   list and per-path mode checks. `ANY`/`ANY SHORTEST` use a BFS over those
   product states. `ANY CHEAPEST` uses Dijkstra over the same states. The
   existing node-deduplicating BFS is used only when a seven-rule test proves
   it equivalent. Section 4.
5. **Syntax.** `GRAPH_TABLE (graph <body>)` looks at the body's tokens BEFORE
   parsing. A body with a top-level `COLUMNS` goes to the existing parser
   unchanged (the legacy profile). A body with a top-level `RETURN` goes to
   the new GQL parser, which runs under a GQL dialect flag with its own
   refusal table. Section 5.
6. **Output.** A `GRAPH_TABLE` result is a relation, and the outer `SELECT`
   over it is compiled as one more stage of the same plan. Lists travel as
   `SqlValue::Json(array)` typed `T[]` by `PreparedSql::column_type`, so
   neither public enum gets a new variant. Section 6.
7. **Parameters.** Every `$n` is a typed slot read when an execution opens.
   So a GQL plan is always rebindable. A seed key that is not found gives an
   empty seed stream. Section 7.

---

## 1. Layering

### 1.1 The constraint

`docs/LAYERS.md`: the chain `dist/ffi -> dist/rust -> dist -> lang -> core`
is enforced by the compiler. core "decides what is on disk or what a query
answers ... the planner, the drivers, the cursors, the budget". lang "turns
TEXT into a call the engine already has. It adds no execution".

Two precedents already relax the literal reading of "lang adds no execution",
and the design builds on both:

* **Row functions.** `lang/src/compile/row.rs` (`CompiledRow`) evaluates
  §4.1/§4.2 functions per returned row, over values the engine projected.
  The cost is one evaluation per row RETURNED (`QL_CONTRACT` §6).
* **The `UPDATE ... SET` closure.** `Database::update_where` takes a
  `&mut dyn FnMut(&Value) -> Result<Value>` that lang compiled. The
  `QL_CONTRACT` §2 UPDATE row says this is so that "core holds no expression
  evaluator".

The rule this design applies, and which `docs/LAYERS.md` should state
explicitly (task M1-D below): **lang may evaluate pure expressions over values
the engine has already handed it; only core walks a keyspace, holds
cross-row state, or charges a budget.**

### 1.2 The split

**Owner constraint: no new crates and no new top-level folders.** All M1-M4
work goes into the existing crates: `core/kernel`, `core/engine`, `lang`,
`dist`, `dist/rust` and `dist/ffi`, with `bench` used only for measurement.
New code is new modules and files inside those crates. The "new" paths below
are all of that kind: `core/engine/src/query/gql/` is a module directory
inside `sekejap-core`, and `lang/src/gql/` is one inside `sekejap-lang`. The
dependency chain of `docs/LAYERS.md` is unchanged: no new `Cargo.toml`, no
new workspace member, no new edge between crates.

| Concern | Crate | Module (new unless marked) | Why there |
| --- | --- | --- | --- |
| `NodeRef`, `EdgeRef`, `PathRef`, `BindingValue`, `BindingRow`, `ListRef`, identity equality, total order, byte accounting | core | `core/engine/src/query/gql/value.rs` | Operators hold and compare these, and budgets charge their bytes. lang reads them, so they must live below lang. |
| Element property reads (node row, edge bag, incoming primary posting), labels, property names, element ids | core | `query/gql/reader.rs` | Each read is a keyspace read and must be charged (brief §4.3: row reads in a seeded traversal are allowed and charged). |
| Operators, their state, cancellation, paging (`GqlCursor`) | core | `query/gql/ops/*.rs`, `query/gql/cursor.rs` | "the drivers, the cursors, the budget". Placed under `query/` so the operators can use `WorkMeter`'s `pub(super)` charge methods without widening them. |
| Path searches (enumeration, BFS, Dijkstra, reachability reuse) | core | `query/gql/paths/*.rs` | They walk adjacency and hold frontiers. |
| Budgets and new work resources | core | `query/gql/budget.rs`; new `WorkResource` variants | The meter is core's. |
| Adjacency cursor shared with the existing traversal | core | `index/graph/adjacency.rs`; the older traversals walk through it too (`index/graph/mod.rs::Hop::walk`) | One per-edge loop for every graph walk, so no copy can drift. |
| Lexing, AST, parsing, GQL refusal table | lang | `lang/src/gql/{ast,parse/*}.rs`, `lang/src/refuse.rs` | Text to syntax. |
| Scopes, slot allocation, types, nullability, provenance (`BindingSchema`) | lang | `lang/src/gql/bind.rs`, `lang/src/gql/schema.rs` | Compile-time facts. core needs only slot counts and slot ids. |
| Expression IR, function registry, evaluator, horizontal vs vertical aggregation classification | lang | `lang/src/gql/expr.rs`, `lang/src/gql/eval.rs`, reusing `lang/src/functions.rs` | One function catalog for SQL and graph expressions (brief §6: "implement missing ones once"). |
| Planning: seed choice, predicate placement, automaton construction, BFS-equivalence proof, operator tree | lang | `lang/src/gql/plan.rs`, `lang/src/gql/automaton.rs` | Planning decides nothing about bytes on disk. It emits a core `GqlPlan` value, just as today's compiler emits a `QueryRequest`. |
| Candidate seeds from indexes | lang builds the request, core pages it | `GqlHost::open_seed` returns a core `PreparedQuery<'db>` | Reuses all of `prepare_query` (scalar today; text, spatial and vector for M6) without moving lang's owned request mirrors (`OwnedFilter`, ...) into core. `PreparedQuery<'db>` does not borrow its request (`query/plan.rs::prepare_query`), so core can hold it across pulls. |
| `EXPLAIN` text | lang | `lang/src/explain.rs` (extended) | It formats `GqlPlanDescription`, which core produces. |
| Wire types, arrays | dist | `dist/src/pg/types.rs`, `dist/src/pg/connection.rs` | |

### 1.3 The one interface between them

```rust
// core/engine/src/query/gql/host.rs  -- SKETCH, not compiled
/// What core needs from the language layer while it executes a GqlPlan.
/// lang implements it on its compiled plan. core never names a lang type.
pub trait GqlHost {
    /// Evaluate expression `expr` over `row`. Property access goes through
    /// `cx.reader`, which charges the meter; the host itself never reads a
    /// keyspace.
    fn eval(&self, expr: ExprId, row: &BindingRow, cx: &mut EvalCx<'_, '_>)
        -> QueryResult<BindingValue>;

    /// Three-valued predicate evaluation (true / false / unknown). A filter
    /// keeps a row only on `Truth::True`.
    fn test(&self, expr: ExprId, row: &BindingRow, cx: &mut EvalCx<'_, '_>)
        -> QueryResult<Truth>;

    /// An index-candidate seed for this input row: a prepared query over one
    /// collection projecting `Projection::Ids`, or `None` for an empty stream
    /// (for example a bound value that makes the predicate unsatisfiable).
    fn open_seed<'db>(&self, seed: SeedId, db: &'db Database, row: &BindingRow,
        cx: &mut EvalCx<'_, '_>) -> QueryResult<Option<PreparedQuery<'db>>>;
}

pub struct EvalCx<'r, 'm> {
    pub reader: &'r mut ElementReader<'r>,   // charged property access
    pub params: &'r [BindingValue],          // the bound $n values, typed
    pub meter: &'m mut GqlMeter<'m>,         // for list-byte charges
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Truth { True, False, Unknown }
```

`ExprId` and `SeedId` are plain `u32` indices into tables that lang owns. core
treats them as opaque. The trait is object-safe, so core stores
`&dyn GqlHost`.

**Why not put the expression IR in core?** It would duplicate
`lang/src/functions.rs`, or move it. It would also make core a second
expression compiler beside the `ScoreExpr` it already has. **Cost of this
choice (named):** one dynamic call per expression evaluation, and core's own
operator tests need a small test host. `query/gql/testing.rs` provides one,
behind `#[cfg(test)]` and the existing `test-support` pattern.

---

## 2. Values, identity and the binding schema (M1)

Brief §4.1, §7 "Scope and type rules".

### 2.1 Element references

```rust
// core/engine/src/query/gql/value.rs -- SKETCH

/// A node: a row in a collection. Identity is the EntityId: collection plus
/// sequence. Two collections holding the same external key are two nodes
/// (brief §11 matrix row 1). Delete then reinsert allocates a new sequence
/// (`docs/core/ARCHITECTURE.md` §2), so a NodeRef can never silently point
/// at a reinserted row. A NodeRef is valid only inside the execution that
/// produced it: plans hold no NodeRef (section 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeRef(pub EntityId);

/// A stored edge. Identity is the stored key PLUS the edge id, so parallel
/// edges of one type between one pair are distinct (GRAPH_CONTRACT §2.3).
/// `key` is the STORED orientation (source -> destination), never the
/// traversal orientation; the traversal orientation lives on the path step
/// (`Step::forward`). `bag` caches the property bag when the operator that
/// produced the reference had already decoded it (an outgoing posting
/// carries it for free). Equality, ordering and hashing ignore `bag`.
#[derive(Clone, Debug)]
pub struct EdgeRef {
    pub key: EdgeKey,          // existing: source, context, edge_type, destination
    pub id: EdgeId,            // from the edge-identity work, see 2.6
    pub bag: Option<Arc<Value>>,
}

/// A path: a shared-prefix (cons) list of steps. Extending a path allocates
/// one node and shares the whole prefix, so a search that holds k partial
/// paths holds O(k) steps, not O(k * length). A zero-length path is one
/// `Start` with no step (brief §4.1).
#[derive(Clone, Debug)]
pub struct PathRef(Arc<PathNode>);

#[derive(Debug)]
enum PathNode {
    Start { node: NodeRef },
    Step {
        parent: Arc<PathNode>,
        edge: EdgeRef,
        /// True when the pattern crossed the edge source -> destination.
        forward: bool,
        node: NodeRef,         // the node the step arrives at
        len: u32,              // edges so far; PATH_LENGTH is O(1)
        /// Which pattern position (automaton transition) produced this step.
        /// Group-variable lists are rebuilt from this (section 4.5).
        tag: u16,
    },
}
```

`EdgeRef` shares its name with the existing `index::graph::EdgeRef`, the
"reaching edge" a BFS binds (GRAPH_CONTRACT §4.2). The two live in different
modules and neither is renamed, because renaming a public type breaks
callers. This document writes `graph::EdgeRef` for the old one.

**Labels are free.** A node's label is its collection
(brief §4.1: "a collection supplies its node label"), and `EntityId` carries
the collection. So a label test at any pattern position reads no row. An
edge's label is its type, and the edge key carries the type.

### 2.2 Binding values, rows and lists

```rust
/// One slot value. GQL null semantics apply INSIDE the profile: a missing
/// property and a stored null both read as `Null` (brief §7 rule 8). The
/// storage distinction survives in the row and is visible through
/// PROPERTY_NAMES(x), and outside GRAPH_TABLE SQL's `Missing` is unchanged.
#[derive(Clone, Debug)]
pub enum BindingValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(Arc<str>),
    Json(Arc<Value>),
    Vector(Arc<[f32]>),
    /// A stored geometry, as GeoJSON (what a geometry column stores today).
    Geo(Arc<Value>),
    Bytes(Arc<[u8]>),
    Node(NodeRef),
    Edge(EdgeRef),
    Path(PathRef),
    List(ListRef),
}

/// A typed, ordered list. Never flattened, never deduplicated implicitly
/// (brief §4.1). `elem` is the element type the binder proved; a list built
/// at run time from mixed values is refused at compile time, not coerced.
#[derive(Clone, Debug)]
pub struct ListRef {
    pub items: Arc<[BindingValue]>,
    pub elem: ValueType,        // see 2.3
}

/// A row of the working table. `slots[i]` is slot `SlotId(i)` of the stage
/// schema that produced it. The schema itself is lang's (2.3); core knows
/// only the width.
#[derive(Clone, Debug)]
pub struct BindingRow {
    pub slots: Box<[BindingValue]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotId(pub u16);
```

The semantics core implements once, in `value.rs`:

| Operation | Rule |
| --- | --- |
| `identity_eq(a, b)` | Node: `EntityId` equal. Edge: `(key, id)` equal. Path: equal step sequences (edges and orientations) and equal start. Scalars: SQL equality. `Null` is never equal (unknown). |
| `group_key` / `DISTINCT` / `COUNT(DISTINCT x)` | Hash and equality by identity for elements, and by value for scalars. `Null` groups with `Null` (SQL `GROUP BY` rule). A property bag is never compared (brief §4.1). |
| `total_order` (sort) | A total order for `SortTopK`. Within one type it is the natural order. Across types the fixed order is `Null` < Bool < numbers (Int and Float compared as numbers) < Text < Json < others by type tag. Element values order by identity; that order is deterministic but not meaningful and the documentation says so. Direction and `NULLS FIRST/LAST` are applied by the sort key, not here. See also Q12. |
| `held_bytes()` | A stated estimate used by every memory charge: 16 bytes per slot plus the heap bytes of text, json, vector, list and path (the path counts only steps not shared with an earlier held path). |

### 2.3 The binding schema (lang)

```rust
// lang/src/gql/schema.rs -- SKETCH
pub(crate) struct BindingSchema {
    pub slots: Vec<SlotInfo>,              // index = SlotId
    pub by_name: Vec<(Name, SlotId)>,      // user-visible variables only
}

pub(crate) struct SlotInfo {
    pub name: Option<Name>,                // None: anonymous element or hidden
    pub ty: ValueType,
    pub nullable: bool,                    // e.g. OPTIONAL-introduced (M5), outer-join-like
    pub provenance: Provenance,
    pub visible: bool,                     // hidden slots (path tags, sort keys) are not
}

pub enum ValueType {                       // core-visible, lives in core value.rs
    Unknown,                               // the NULL literal before unification
    Bool, Int, Float, Text, Json, Vector(Option<u32>), Geo, Bytes,
    Timestamp, Date,                       // declared spellings over Int (QL deviation 8)
    Node(LabelSet),                        // the collections it can be in
    Edge(EdgeTypeSet),                     // the edge types it can be
    Path,
    List(Box<ValueType>),
}

pub(crate) enum Provenance {
    /// Bound by a node or edge pattern at a syntactic position.
    Element { pattern: PatternId, position: u16 },
    /// A quantified variable seen outside its quantifier: a list.
    Group { quantifier: QuantId, singleton: SlotId },
    Path { pattern: PatternId },
    Let { stage: u16 },
    Aggregate { stage: u16 },
    Unnest { stage: u16 },
    /// A property read, kept so an alias still carries index lineage (M6).
    Property { of: SlotId, field: FieldRef },
    Returned { from: SlotId, stage: u16 },
}

pub struct LabelSet(SmallVec<[CollectionId; 2]>);   // resolved label alternatives
pub struct EdgeTypeSet(SmallVec<[EdgeTypeId; 2]>);  // empty = any type (unlabelled edge)
```

Rules the binder enforces (brief §7 "Scope and type rules"):

1. A fresh element variable allocates a slot. A repeated node or edge
   variable in the same `MATCH` adds an identity constraint and allocates
   nothing. An anonymous element allocates a hidden slot only when the plan
   needs one (for example a path mode check). Rule 1.
2. A variable inside a quantifier is a singleton inside it. Outside it, the
   name resolves to its `Group` slot of type `List(..)`. There is no "last
   edge" fallback anywhere. Rule 2.
3. `LET a = ..., b = ...` does not let `b` see `a`: each assignment binds
   against the scope in force before the `LET`. Rule 3.
4. `RETURN` produces the next stage's schema. After `NEXT`, only its columns
   are in scope, and naming a dropped variable is a bind error that names the
   stage that dropped it. Rule 4 and the "NEXT dropping a variable" matrix
   row.
5. Node and edge identity equality (`a = b`, `a <> b`) is typed separately
   from property equality. `a.name = b.name` compares values. Rule 5 of
   brief §4.1.
6. Final output columns must have a SQL-representable type (section 6).
   `RETURN person` is legal in an intermediate stage and refused in the last
   one, with a message naming `person._key` and `ELEMENT_ID(person)`. Rule 10.
7. A property access types as the declared kind of that field in every
   collection of the node's `LabelSet`. If the kinds disagree, or the field
   is declared in none of them (it may live in the extras lane), it types as
   `Json` for evaluation and as `TEXT` for output, and a notice names the
   collections (Q8).
8. Identifiers follow the SQL parser's rules: unquoted names compare without
   case, quoted names are exact (brief §7 rule 9). Labels resolve through the
   catalog exactly as table names do (`lang/src/lib.rs::find`).

### 2.4 How the new types relate to the existing ones

| Existing | Relation |
| --- | --- |
| `EntityId` | `NodeRef` wraps it. `ELEMENT_ID(node)` prints an opaque, versioned text derived from it (`n1:` prefix). Query-scope uniqueness is the promise (brief §6); stability across queries is not. |
| `graph::EdgeKey` | The stored orientation inside `EdgeRef`. |
| `graph::EdgeRef` (reaching edge) | Unchanged. The legacy profile and `QueryOrder::Edge` keep using it. |
| `ProjectedValue` (`Missing`/`Null`/`Value`) | `ElementReader` maps `Missing` and `Null` to `BindingValue::Null` and keeps the presence bit for `PROPERTY_NAMES`. |
| `SqlValue` | Output only: the final stage's scalars convert to `SqlValue`, and lists to `SqlValue::Json(array)` (section 6). Never an intermediate. |
| `SqlRow { id: EntityId, .. }` | Not used as an intermediate. See section 6.2 for the output row's `id`. |
| `Param` | Converted once per execution into `params: Vec<BindingValue>`, checked against the inferred `ValueType` of each slot (section 7). |
| `QueryBudget` / `WorkMeter` | Embedded unchanged in `GqlBudget` / `GqlMeter` (section 3.4). |

### 2.5 Null and missing inside the profile

* A property that is absent from the row, or stored as null, evaluates to
  `Null` (brief §7 rule 8).
* Comparisons with `Null` are `Unknown`, and `FILTER`/`WHERE` keep only
  `True`: SQL three-valued logic.
* `IS NULL` is true for both missing and null. `IS MISSING` stays an
  SQL-only predicate and is refused in GQL context with a pointer to
  `PROPERTY_NAMES`.
* The `Missing` contract of the SQL surface outside `GRAPH_TABLE` is not
  changed.

### 2.6 What this design needs from the edge-identity work

A separate worker is adding independent edge ids (parallel edges, feature bit
`0x40000`, create-edge and delete-by-id). This design does NOT specify their
storage. It relies on four properties, which that work should confirm or
correct:

1. SHIPPED as `collections::EdgeId { key: EdgeKey, id: u64 }` (feature bit
   `EDGE_ID_FEATURE = 0x40000`); the GQL `EdgeRef` holds `key` plus that
   `id`, and `(key, id)` identifies at most one stored edge. Unique within the tuple is enough; global
   uniqueness is not required.
2. An adjacency posting (primary or reverse) exposes the `EdgeId`, so the
   per-edge loop can produce `EdgeRef` without a second read.
3. The primary posting is addressable by `(EdgeKey, EdgeId)`, so an incoming
   hop that needs properties costs one point read (as today, GRAPH_CONTRACT
   §4.2).
4. A file without the bit presents every edge with one fixed implicit id
   (for example `EdgeId(0)`). This is safe because such a file cannot hold
   parallel edges.

If the id segment changes the key tail, only `index/graph/adjacency.rs`
(section 1.2) parses it, so one function absorbs the change.

---

## 3. Operators (M2-M4)

Brief §4.2, §4.3.

### 3.1 Interface: pull, one row at a time

```rust
// core/engine/src/query/gql/ops/mod.rs -- SKETCH
pub(crate) trait Operator<'db> {
    /// Called once per execution before the first `next`.
    fn open(&mut self, cx: &mut ExecCx<'db, '_>) -> QueryResult<()>;
    /// The next row, or None when the stream is complete. Must charge the
    /// meter for everything it reads and everything it holds.
    fn next(&mut self, cx: &mut ExecCx<'db, '_>) -> QueryResult<Option<BindingRow>>;
    /// For EXPLAIN: kind, parameters, and the resources that can stop it.
    fn describe(&self) -> OperatorDescription;
}

pub(crate) struct ExecCx<'db, 'x> {
    pub db: &'db Database,
    pub host: &'x dyn GqlHost,
    pub meter: &'x mut GqlMeter<'x>,
    pub reader: ElementReader<'db>,
    pub params: &'x [BindingValue],
}
```

Why one row at a time rather than batches: every operator in M1-M4 is
either a per-row apply (seed, expand, filter, let) or a blocking fold. Batches
buy nothing that `PreparedQuery`'s own pages do not already give the
index-driven seed. **Cost (named):** one virtual call per row per operator.
Batching can be added later behind the same trait (`next_batch`) without
changing semantics.

The plan is data, built by lang and instantiated by core:

```rust
// core/engine/src/query/gql/plan.rs -- SKETCH
pub struct GqlPlan {
    pub root: OpSpec,
    pub width: u16,                  // slots in the final row
    pub columns: Vec<OutputColumn>,  // name + ValueType, for the wire
}

pub enum OpSpec {
    /// One empty row: the input of a stage that has none (first stage).
    Unit,
    /// Bind `out` to start nodes, once per input row.
    Seed { input: Box<OpSpec>, out: SlotId, source: SeedSource, labels: LabelSet,
           node_filter: Option<ExprId> },
    /// One hop per input row. `to: Bound(s)` is ExpandInto.
    Expand { input: Box<OpSpec>, from: SlotId, edge: Option<SlotId>, to: Target,
             step: StepSpec },
    Filter { input: Box<OpSpec>, predicate: ExprId },
    Let { input: Box<OpSpec>, assign: Vec<(SlotId, ExprId)> },
    /// RETURN's projection and the NEXT boundary: a new row of `width` slots.
    Project { input: Box<OpSpec>, cols: Vec<ExprId>, width: u16 },
    Aggregate { input: Box<OpSpec>, keys: Vec<ExprId>, aggs: Vec<AggSpec>, width: u16 },
    Distinct { input: Box<OpSpec> },
    Sort { input: Box<OpSpec>, keys: Vec<SortKey>, keep: Option<CountExpr> },
    Page { input: Box<OpSpec>, offset: Option<CountExpr>, limit: Option<CountExpr> },
    Unnest { input: Box<OpSpec>, list: ExprId, out: SlotId },
    /// M4: quantified or multi-step path search from a bound start.
    PathSearch { input: Box<OpSpec>, spec: PathSpec },
    /// M4: the existing node-deduplicating BFS, only when proved equivalent.
    Reach { input: Box<OpSpec>, spec: ReachSpec, proof: ReachProof },
    /// Q3: pulled into M3 only if the owner agrees.
    OptionalApply { input: Box<OpSpec>, inner: Box<OpSpec>, introduced: Vec<SlotId> },
}

pub enum SeedSource {
    /// `(n IS T WHERE n._key = <expr>)`: one mapping lookup per input row.
    /// Not found -> no row (NOT an error: brief §2 difference 3).
    Key { key: ExprId },
    /// Index candidates from lang's `GqlHost::open_seed`.
    Index { seed: SeedId },
    /// The node already bound in `slot` (a variable carried across NEXT, or
    /// repeated from an earlier pattern).
    Bound { slot: SlotId },
    /// Every visible row of the label's collections. A SCAN, printed as one
    /// by EXPLAIN and subject to the admission policy (Q5).
    Scan,
}

pub struct StepSpec {
    pub context: GraphContextId,
    pub types: EdgeTypeSet,          // label alternatives: one range per type
    pub direction: Direction,        // Outgoing / Incoming / Both
    pub edge_filter: Option<ExprId>, // inline edge WHERE, evaluated per edge
    pub far_labels: LabelSet,        // free: collection check on EntityId
    pub far_filter: Option<ExprId>,  // inline node WHERE on the far node
}

pub enum Target { New(SlotId), Bound(SlotId) }
pub struct SortKey { pub expr: ExprId, pub descending: bool, pub nulls_first: bool }
pub enum CountExpr { Lit(u64), Param(usize) }  // validated when the execution opens
```

### 3.2 Each operator: behaviour and charges

The existing `QueryBudget` resources keep their meaning. The new resources
are defined in 3.4. "Held" means a memory high-water mark (like `groups` and
`membership_bytes` today). "Work" means a running count.

| Operator | Behaviour | Charges |
| --- | --- | --- |
| `Seed::Key` | Evaluates the key expression; one mapping lookup in the label's collection (one lookup per collection for a label alternation). A non-text value is a type error at bind, never a lookup. | `key_postings` 1 per lookup; `binding_rows` 1 per row out. |
| `Seed::Index` | Pages `PreparedQuery::next_page` in chunks of at most 256 ids, passing the meter's remaining `QueryBudget` and absorbing the page's `QueryWork`. | Whatever the index walk charges; `binding_rows`. |
| `Seed::Bound` | Passes the bound node through, after the label test and the node filter. | `binding_rows`. |
| `Seed::Scan` | Entity walk of each collection in the label set. | `candidates` per row; labelled SCAN. |
| `Expand` | For each input row: open an `AdjacencyCursor` per (direction, type) and emit one row per admitted edge. `Both` scans outgoing then incoming and skips, in the incoming scan, any edge whose source equals its destination: a self-loop is one match, not two orientations (brief §11 matrix, self-loop row). The far-node label test is free. `edge_filter` reads the bag; an incoming edge whose bag is needed costs one primary-posting read. `far_filter` may read the row (brief §4.3), and that read is charged. `to: Bound` compares identity and emits at most the matching edges; parallel edges give one row EACH. | `graph_edges` per posting walked and per incoming primary read (as `drivers.rs` charges today); `primary_reads` per node row read; `binding_rows` per row out. Holds one cursor refill buffer (at most 256 postings). |
| `Filter` | Keeps rows where `host.test` is `True`. | Whatever the evaluation reads. |
| `Let` | Evaluates and stores. | `list_bytes` for any list it materialises (held while the row lives in a blocking operator, otherwise transient). |
| `Project` | Builds the new row and drops everything else. This is the `NEXT` boundary, so no stage-local state crosses it. | Nothing beyond the evaluation. |
| `Aggregate` | Hash grouping by identity or value. Accumulators: `COUNT(*)`, `COUNT(x)`, `COUNT(DISTINCT x)`, `SUM`, `AVG`, `MIN`, `MAX`, `ARRAY_AGG`. With no grouping key and empty input it emits one row (`COUNT` = 0, others `Null`); with a grouping key and empty input it emits no rows (brief §11 matrix, empty-input row). | `groups` (held, existing resource and cap); `sort_bytes` held for the key and distinct-set bytes; `list_bytes` held for `ARRAY_AGG`. |
| `Distinct` | Hash set over the whole row by identity or value. | `sort_bytes` held. |
| `Sort` | Multi-key, stable: ties keep input order through a sequence number, so the tie order is deterministic for a fixed snapshot and plan. With `keep = offset + limit`, a bounded heap. Otherwise a full buffer. No spill: over the ceiling it refuses. | `sort_bytes` held. |
| `Page` | Skips `offset` rows, stops after `limit`. Values are checked when the execution opens: non-negative and at most `i64::MAX` (Q13). A `LIMIT` stops pulling from upstream, so an unsorted pipeline does no work past it. | Skipped rows were already charged upstream. The cost of `OFFSET` is therefore the skipped rows, and EXPLAIN says so. |
| `Unnest` (`FOR x IN list`) | One row per element, in list order. A `Null` list yields no rows; an empty list yields no rows. | `binding_rows`. |
| `PathSearch`, `Reach` | Section 4. | Section 4.5. |

### 3.3 Composition across stages

* A stage is `[input] -> MATCH ... -> LET/FILTER ... -> RETURN (Project |
  Aggregate) -> [Distinct] -> [Sort] -> [Page]`.
* `NEXT` feeds that stage's root into the next stage's operators as their
  `input`. Nothing is re-scanned: a node carried in a slot seeds with
  `Seed::Bound` (brief §7 rule 4).
* A `MATCH` in a later stage is applied **per input row**. When it names no
  input variable it is still evaluated per input row: a cross product, as
  GQL defines it. **Cost (named):** v1 re-runs an uncorrelated pattern once
  per input row. Materialising it once under `sort_bytes` is a later
  optimisation, and EXPLAIN prints "re-evaluated per input row".
* An aggregate in a later stage folds the WHOLE incoming table, not one input
  row at a time (brief §7 rule 5; matrix row "NEXT aggregate over many
  inputs").
* Several patterns in one `MATCH` separated by commas are joined on shared
  variables: the second pattern starts from a shared variable if it has one
  (`Seed::Bound`); otherwise it is a cross product, and EXPLAIN says so.

### 3.4 Budgets

`QueryBudget` is a public struct with public fields and no
`#[non_exhaustive]`. The repository builds it with a struct literal in 35
places. Adding fields to it would therefore break every literal. The new
ceilings live in a wrapper instead:

```rust
// core/engine/src/query/gql/budget.rs -- SKETCH
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlBudget {
    pub base: QueryBudget,        // all existing resources, and the deadline
    // work (running counts, per page like the existing resources)
    pub binding_rows: u64,
    pub path_states: u64,         // partial paths / product states created
    // memory (high-water marks over the whole execution)
    pub queue_entries: u64,       // frontier, DFS stack, Dijkstra heap entries
    pub predecessor_arcs: u64,    // retained parent pointers for witness paths
    pub sort_bytes: u64,          // sort buffers, distinct sets, group keys
    pub list_bytes: u64,          // materialised lists: group lists, ARRAY_AGG, NODES(p)
}

impl GqlBudget {
    /// Work ceilings unlimited; MEMORY ceilings at their fixed caps. Like
    /// `WorkResource::MembershipBytes` today, a memory cap is a promise
    /// (Law 1), not a caller allowance, so `unlimited()` does not lift it.
    pub fn unlimited() -> Self { /* ... */ }
    pub fn from_query_budget(base: QueryBudget) -> Self { /* ... */ }
}
```

New `WorkResource` variants: `BindingRows`, `PathStates`, `QueueEntries`,
`PredecessorArcs`, `SortBytes`, `ListBytes`. `Deadline` and
`MembershipBytes` were added to that enum the same way. `QueryWork` does not
change; `GqlWork { base: QueryWork, .. }` mirrors `GqlBudget`.

Default memory caps (Q6): `sort_bytes` and `list_bytes` each equal
`RUN_BYTES` (8 MiB, `query/rows.rs`). `queue_entries` and `predecessor_arcs`
are `RUN_BYTES` divided by the per-entry cost. Every cap is charged against
the one `RUN_BYTES` promise per execution, following the pattern of
`default_groups_cap`.

A visited-node count alone cannot bound a path enumerator (brief §4.3). That
is why `path_states` and `queue_entries` exist, and why section 4 charges them
on every extension.

### 3.5 Cancellation, errors and paging

* Cancellation and the deadline are checked on every charge, through the
  existing `WorkMeter::check_cancelled` path.
* **Execution object.** `GqlCursor<'db>` owns the instantiated operator tree
  and borrows `&'db Database`. That borrow is what makes every page of one
  answer read one snapshot: a writer needs `&mut Database`, and a service
  reader holds its own snapshot handle (`docs/core/ARCHITECTURE.md` §1.3).
  `next_page(page_rows, GqlBudget, cancel) -> QueryResult<GqlPage>` pulls up
  to `page_rows` rows from the root.
* **Decision: v1 materialises only where an operator is blocking, and it
  never resumes across executions.** No keyset continuation token is
  produced. A caller that wants page N+1 keeps the cursor alive: `Db::stream`
  does this, and so do wire portals and declared cursors, whose hold is
  already bounded by `CURSOR_ROW_CAP`/`CURSOR_BYTES_CAP`. The alternative, a
  rank-key resume like `WriteCursor`, needs a total order that survives
  re-execution through blocking operators and path searches. That order does
  not exist for `NEXT` stages with ties, so the design does not promise it.
* **Deviation from the existing page contract (named).** `docs/core/
  ARCHITECTURE.md` §4.6 says a refused page "leaves the position unchanged".
  A GQL execution that is refused part way through a blocking operator has
  partial state that cannot be rolled back cheaply. So a refused or cancelled
  GQL page POISONS its cursor: every later `next_page` returns the same
  error. Rows already returned form an INCOMPLETE stream, never a complete
  answer (brief §4.3), and `GqlPage::done` is never set after a refusal.
* **Work per page, memory per execution.** Work resources are charged
  against the budget handed to the page that did the work. The first page
  of a sorted answer therefore pays for the whole input, and that is correct
  accounting. Memory resources are high-water marks across the execution.
* Brief §11 matrix "Multiple pages": concatenating the pages of one cursor
  equals the one-shot answer. Test task M3-E.

---

## 4. Path search (M4)

Brief §4.2 (`QuantifiedExpand`, `PathSelect`), §8.

### 4.1 From pattern to automaton

lang compiles one path pattern into an automaton whose transitions are
**node tests** (label set plus optional inline predicate) and **edge steps**
(`StepSpec`), connected by epsilon moves. Quantifiers become loops with ONE
counter:

* `{m,n}`, `{m,}`, `{n}`, `?` = `{0,1}`, `*` = `{0,}`, `+` = `{1,}`. Bounds are
  integer literals only (brief §7). `{1,$2}` is a syntax error that names the
  rule. The upper bound is `Option<u32>`: an absent upper bound is
  unbounded, never 16. The legacy `MAX_GRAPH_DEPTH = 16` stays in the legacy
  parser only.
* A quantified edge `-[e]->{1,8}` and a parenthesized subpath
  `((a)-[:r]->(b)-[:s]->(c)){1,3}` compile the same way: the loop body is
  the sub-automaton. Adjacent node patterns inside the loop are
  CONCATENATED: the last node test of iteration i and the first of iteration
  i+1 apply to the same node. This is why a multi-type subpath repeats as a
  whole (brief §11 matrix, "multi-type quantified subpath").
* Labels and predicates written on an endpoint apply at that syntactic
  position only. An intermediate node inside `-[e]->{1,8}` is anonymous and
  gets no label test (brief §11 matrix, "quantified endpoint labels").
* **Zero iterations** add an epsilon path from the node test before the
  quantifier to the node test after it. Both tests then apply to the same
  node, the variables on either side bind to it, and every group variable of
  the quantifier binds to an empty list (matrix `{0,0}` / `{0,1}` / `?` / `*`
  row).
* **v1 restriction (Q10):** quantifiers do not nest. A quantifier inside a
  quantified group is refused by name. Several quantifiers in sequence are
  allowed, and only one is active at a time, so one counter suffices.

The **search state** is `(node, q, k)`: the automaton state `q`, and the
active quantifier's iteration count `k`. `k` is kept up to the larger of the
lower bound and the finite upper bound. For an unbounded quantifier, `k`
SATURATES at the lower bound, because once the minimum is met further
iterations are indistinguishable. The state space is therefore finite:
`|V| * |Q| * (max(lo, hi_finite) + 1)`.

### 4.2 Enumeration without a selector (`PathSearch::Enumerate`)

This is the default for a `MATCH` with quantifiers or several steps and no
selector.

* Depth-first over `(node, q, k)`, with the current path held as a
  `PathRef`. Each accepting state emits one match. Bag semantics: two
  distinct paths (for example the two sides of a diamond, or two parallel
  edges) are two matches (brief §2 difference 1; matrix "Diamond graph").
* Path mode is checked per path, when a step extends it:
  * `WALK`: no check.
  * `TRAIL`: the new edge (identity, `(key, id)`) must not already occur in
    the path.
  * `ACYCLIC`: the new node must not already occur in the path.
  The check walks the cons-list, O(length) per extension. **Cost (named):**
  O(L^2) per path of length L, bounded by `path_states`. A per-path bitset is
  a later optimisation.
* **Admission (brief §4.3):** an unbounded upper bound is accepted only with
  a selector (4.3) or with `TRAIL`/`ACYCLIC`, both of which terminate on a
  finite graph. `WALK` with an unbounded quantifier and no selector is
  refused at compile time, naming the rule. Budgets still bound every
  accepted form: a query that exhausts one fails; it never truncates.
* Ambiguous automata: a pattern such as `(a)-[]->{0,1}(b)-[]->{0,1}(c)` can
  match one path in two ways with DIFFERENT bindings. Those are two matches.
  Two runs that produce the SAME bindings are deduplicated per input row.
  **Uncertain:** confirm this reduction against the normative ISO text before
  claiming the feature (Q11).
* Brief §2 difference 2 (matrix "Direct edge plus two-hop route"): with
  `A->D` and `A->B->D`, `{2,2}` must still find `A->B->D`. The enumerator
  keeps `k` in the state and has no global visited set, so seeing D at
  `k = 1` does not stop D at `k = 2`.

### 4.3 Selectors

v1 accepts a selector OR a path mode at the top of a path pattern, not both
(brief §8.1: follow Google's top-level choice; combinations need a separately
verified nested form). A selector implies `WALK` semantics. The spelling is
`p = ANY SHORTEST <pattern>`, with the path binding before the selector
(brief §8.1).

The selector partitions matches by `(start node, end node)`, per input row.

**`ANY SHORTEST` (and `ANY`): BFS over product states.**

* Level-synchronous BFS over `(node, q, k)`, with a visited set on the
  product state and one predecessor arc per visited state
  (`(parent state index, EdgeRef, forward)`).
* The first time an accepting state is reached at node `v`, the path to it
  is a minimum-hop path to `v` among the paths the pattern permits. The
  witness is rebuilt from predecessor arcs only for the rows that are
  emitted.
* **Why the visited set is on product states and not nodes.** Under `WALK`,
  every prefix of a shortest walk to `(v, q, k)` is itself a shortest walk to
  its own product state, so the first visit to a product state is final.
  That argument fails for bare nodes as soon as the pattern has a lower
  bound or more than one automaton state: `(D, q, 1)` and `(D, q, 2)` are
  different questions. This is exactly the difference between the product
  BFS and the existing node BFS (4.6).
* Tie order: frontier order, then posting key order, then outgoing before
  incoming. Deterministic for a fixed snapshot. Not promised across engines
  (brief §8.1).
* `ANY` without `SHORTEST` uses the same search. A shortest witness is a
  valid "any" witness. The documentation says only that some qualifying path
  is returned.
* An end node bound in advance (`target._key = $2`, or `Seed::Bound`) turns
  the search into a point-to-point search that stops when the target
  accepts. Bidirectional search is a later optimisation (brief §8.3); v1 does
  not include it.
* `{0,32}` with source equal to target gives the zero-edge path. A
  disconnected or missing target gives zero rows (brief §8.1).

**`ANY CHEAPEST ... COST expr`: Dijkstra over the same product states.**

* The priority is the path cost. Ties break on hop count, then insertion
  sequence, so the choice is deterministic.
* A state is settled when popped. Because `k` is part of the state, a
  cheaper arrival that used more hops does NOT dominate a more expensive
  arrival with fewer hops. This handles the brief §8.3 counterexample by
  construction. With a maximum of three hops, edges s->a cost 10, s->b cost
  1 and b->a cost 1 (so s->b->a costs 2), and a->c->t cost 2: states
  `(a, k=1, 10)` and `(a, k=2, 2)` are different, and only `(a, k=1)` can
  reach `t` within three hops, giving cost 12. Acceptance test M4-C pins
  this case.
* No dominance pruning in v1. The rule "`k <= k'` and `c <= c'` dominates" is
  sound only when `k` is already at or above the lower bound, because lower
  bounds make fewer hops worse. **Cost (named):** more states are settled
  than strictly necessary, bounded by `path_states` and `queue_entries`.
* Cost contract (brief §8.2): every edge step inside the selected pattern
  must carry a `COST`, or the statement is refused at compile time. `COST`
  may reference only its own edge variable, constants and `$n`. Each value
  is evaluated once per edge relaxation. `Null`, NaN, infinity, a negative
  value, or zero (Q4) raises the named error `InvalidPathCost { value,
  edge }`, mapped to SQLSTATE `22023`. It is never skipped and never clamped.
  No epsilon is added (brief §8.2).
* Costs are minimised. A strongest-confidence chain needs a cost transform
  the application chooses. The documentation repeats the brief's warning
  that `-LN(confidence)` is zero at confidence 1 and so is not a positive
  cost.

**`ALL SHORTEST`** stays refused (P1). The predecessor-arc structure above is
what it will extend: keep every equal-distance arc and enumerate the DAG
(brief §8.3). A single witness must never be passed off as the full set.

### 4.4 Named paths and path functions

`p = <pattern>` binds a `Path` slot to the `PathRef` the search built. Path
functions are evaluated by the lang host over `PathRef` accessors that core
exposes (`len()`, `first()`, `last()`, `steps()`):

| Function | Result |
| --- | --- |
| `PATH_LENGTH(p)` | Edge count, O(1). |
| `PATH_FIRST(p)`, `PATH_LAST(p)` | Endpoint NODES, not edge-property values (brief §6). |
| `NODES(p)`, `EDGES(p)` | Ordered lists. Charged to `list_bytes` when materialised. |
| `IS_ACYCLIC(p)`, `IS_TRAIL(p)` | O(L) checks over the path. |
| `ELEMENT_ID(x)`, `SOURCE_NODE_ID(e)`, `DESTINATION_NODE_ID(e)` | Opaque ids. Source and destination are the STORED endpoints, whatever the traversal direction (brief §6). |
| `LABELS(x)`, `PROPERTY_NAMES(x)` | The collection name or edge type name. Property names include only fields present in the row or bag, so presence survives here (brief §6). |

The old planned spellings `path_sum`, `path_product`, `VERTEX_ID` and
`EDGE_ID` stay refused and are not promoted (brief §6).

### 4.5 Group variables and horizontal aggregation

* When a match completes, each group variable of each quantifier gets a
  `ListRef`, rebuilt by walking the `PathRef` steps whose `tag` belongs to
  that quantifier, in path order. The bytes are charged to `list_bytes`.
  (A lazy view over the `PathRef` would avoid the copy. It is deferred
  because groups are short in practice.)
* **Horizontal versus vertical** (brief §6, "Aggregation has two different
  axes"):
  * In `LET`, `FILTER` and a pattern `WHERE`, an aggregate whose argument
    mentions a list-typed variable (a group variable, or a list from
    `NODES`/`EDGES`/`LET`) is HORIZONTAL. It folds the list of the current
    row. Inside that argument only, property access maps over the elements:
    `SUM(e.weight)` and `ARRAY_AGG(ns._key)`.
  * In `RETURN`, an aggregate is VERTICAL: it folds rows of the working
    table. An argument that is a group variable, or that contains a
    horizontal aggregate, is refused. The refusal says "compute it in a
    `LET` first". A `RETURN` aggregate is never reinterpreted as horizontal.
  * The classification is done by the binder from types, never from the
    spelling.
* Empty and null behaviour: `COUNT` over an empty list is 0. `SUM`, `AVG`,
  `MIN` and `MAX` over an empty list, or a list of nulls, are `Null` (SQL
  aggregate rule), so `COALESCE(SUM(e.cost), 0.0)` is the zero-length idiom
  (brief §8.2). `ARRAY_AGG` over an empty group list is an empty list (Q9).
* `EXP(SUM(LN(x)))` is only as valid as `LN`. `LN` of a non-positive value is
  an error, never `Null` (Q14). The profile documentation carries the
  brief's warning that this is not a general product.

### 4.6 When the existing BFS may stand in (`Reach`)

The existing traversal (`traverse_bfs`, and the `QueryFilter::Graph` driver)
keeps one global visited set on NODES and the first reaching edge per node.
It loses paths, multiplicities, later depths and every non-first reaching
edge. Brief §4.2 allows it only when the compiler proves the query observes
none of those. The planner applies this test, and EXPLAIN prints which rule
admitted the plan or which rule failed:

1. **Shape.** The pattern is one quantified single edge
   `(s)-[e :T WHERE <edge-only predicate>]->{lo,hi}(t)`: one edge-type set,
   one direction, one context, no subpath, no named path.
2. **Start.** `s` is bound (a seed or a slot), and the search runs once per
   start.
3. **Observed outputs.** Downstream uses only `t` (identity or properties),
   `s`, or `PATH_LENGTH(p)` under `ANY SHORTEST`. It does not use `e`, the
   group list, `NODES`/`EDGES`, a path count or a vertical aggregate that
   counts rows.
4. **Consumer.** The next operator is a `Distinct` over (`s`, `t`), an
   aggregate using only `COUNT(DISTINCT t)`-like identity folds, or
   `ANY`/`ANY SHORTEST`. In the shortest case the BFS tree's parent pointers
   are a valid shortest witness, because depth in a node BFS equals shortest
   walk length for a single-edge pattern.
5. **Lower bound.** `lo = 0`, or `lo = 1` and one of these holds: the mode is
   `ACYCLIC`; the label set of `t` excludes the collection of `s`; or an
   extra `ExpandInto`-style check adds `s` when a cycle returns to it. The
   node BFS marks the seed seen at depth 0, so it never reports `s` as a
   depth-1-or-more endpoint. For `lo = 1`, `WALK` and `TRAIL` admit `s` via a
   cycle and `ACYCLIC` does not. Pinned by test M4-F. `lo >= 2` is never
   equivalent (brief §2 difference 2).
6. **No endpoint pruning.** A predicate on `t` is applied AFTER the BFS as a
   filter, never as `node_where`. The existing BFS applies `node_where` to
   every reached node and stops expanding it, but the GQL endpoint predicate
   does not constrain intermediate nodes.
7. **Bounds fit.** `hi <= MAX_BFS_DEPTH` (64), or `hi` is unbounded and
   rule 4 is a reachability consumer.

For rules 1-7, the endpoint SET is the same under `WALK`, `TRAIL` and
`ACYCLIC` (a walk within `hi` hops contains a simple path within `hi` hops),
except for the start node covered by rule 5.

A `DISTINCT` endpoint query is not automatically safe (brief §4.2). Rules 5
and 6 are the two ways it goes wrong, and each has a test.

### 4.7 Path-search charges

| Resource | Charged |
| --- | --- |
| `graph_edges` | Per adjacency posting walked, and per incoming primary read, as `Expand` charges. |
| `path_states` | Per partial path created (enumeration), or per product state first discovered (BFS, Dijkstra). |
| `queue_entries` (held) | DFS stack depth plus sibling cursors; BFS frontier size; Dijkstra heap size. |
| `predecessor_arcs` (held) | One per visited product state (BFS, Dijkstra). Enumeration holds none: the path IS the predecessor chain. |
| `binding_rows` | Per emitted match. |
| `list_bytes` | Group lists and path materialisations. |
| `primary_reads` | Node predicates that read a row. |

---

## 5. Parser and AST (M2)

### 5.1 Two body kinds, chosen before parsing

`lang/src/parser/graph_table.rs::graph_table` today parses
`GRAPH_TABLE ( <name> MATCH ... COLUMNS (...) )` directly. The change:

1. After `GRAPH_TABLE (` and the graph name, call a token pre-scan
   `body_kind()`. It walks the parser's token vector from the cursor to the
   matching `)` at depth 0 (the token vector is already materialised, and
   `peek_at`/`word_at` exist) and records which of these words occur at
   paren depth 0: `COLUMNS`, `RETURN`, `NEXT`, `LET`, `FILTER`, `FOR`.
2. The result:
   * `COLUMNS` and none of the others: **legacy**. The existing
     `graph_table()` runs UNCHANGED. Every legacy acceptance and every legacy
     refusal message stays byte-identical (brief §10 compatibility policy;
     matrix "Legacy queries"). This includes today's refusals of `TRAIL`,
     `ANY SHORTEST`, label alternation and non-key seeds.
   * `RETURN` (optionally with the others) and no `COLUMNS`: **GQL**. The
     new parser runs.
   * Both: refused by name as a `COLUMNS ... RETURN` hybrid (brief §10: "Do
     not permit ad hoc `COLUMNS ... NEXT` hybrids in v1").
   * Neither: a syntax error naming both terminators.
3. The AST gets a second variant: `Source::Gql(Box<GqlGraphTable>)` beside
   `Source::Graph(Box<GraphTable>)`, so compile and `EXPLAIN` can never route
   the two to the same code by accident (brief §10).

### 5.2 The GQL AST (lang/src/gql/ast.rs)

```rust
// SKETCH
pub(crate) struct GqlGraphTable { pub graph: Name, pub body: Pipeline, pub alias: Option<Name> }
pub(crate) struct Pipeline { pub stages: Vec<Stage> }            // separated by NEXT
pub(crate) struct Stage { pub statements: Vec<Statement>, pub ret: Return }
pub(crate) enum Statement {
    Match { patterns: Vec<PathPattern>, where_: Option<Expr>, optional: bool },
    Let(Vec<(Name, Expr)>),
    Filter(Expr),
    For { var: Name, list: Expr },
}
pub(crate) struct Return {
    pub distinct: bool,
    pub items: Vec<(Expr, Option<Name>)>,
    pub group_by: Option<Vec<Expr>>,
    pub order_by: Vec<OrderItem>,
    pub offset: Option<Expr>,   // literal or $n only
    pub limit: Option<Expr>,    // literal or $n only
}
pub(crate) struct PathPattern {
    pub name: Option<Name>,                 // p = ...
    pub prefix: PathPrefix,                 // None | Mode(Walk|Trail|Acyclic) | Selector(Any|AnyShortest|AnyCheapest)
    pub elements: Vec<PathElement>,
}
pub(crate) enum PathElement {
    Node(NodePattern),
    Edge(EdgePattern),
    Group { inner: Vec<PathElement>, quantifier: Quantifier },
}
pub(crate) struct NodePattern { pub var: Option<Name>, pub label: Option<LabelExpr>, pub where_: Option<Expr> }
pub(crate) struct EdgePattern {
    pub var: Option<Name>, pub label: Option<LabelExpr>, pub where_: Option<Expr>,
    pub cost: Option<Expr>, pub direction: EdgeDir, pub quantifier: Option<Quantifier>,
}
pub(crate) enum LabelExpr { Name(Name), Or(Vec<LabelExpr>) }  // & and ! refused (P1)
pub(crate) struct Quantifier { pub lo: u32, pub hi: Option<u32> }
pub(crate) enum Expr {  /* literals, $n, qualified names, unary/binary operators,
                           IS [NOT] NULL, IN (...), CASE, CAST/::, function calls
                           (with DISTINCT flag), list literal [..] */ }
```

The GQL expression grammar is a general precedence-climbing expression parser
(`lang/src/gql/parse/expr.rs`). It does NOT reuse `parser/expr.rs`, which is
predicate-shaped by design (every leaf names an index). It does reuse the
existing sub-parsers for the PostgreSQL host forms whose argument shapes are
fixed (`to_tsvector(...) @@ to_tsquery(...)`, `ST_*` constructors,
`'...'::vector`). This keeps one spelling for each host function.

### 5.3 Context-aware keywords

* `Parser` gets a `dialect: Dialect { Sql, Gql }` field. It is set to `Gql`
  for the length of a GQL body, including the outer `SELECT` over a GQL
  relation (5.5), and restored afterwards.
* `Parser::listed()` (the lookup behind `guard_here`, `expect`,
  `expect_word` and `qualified_name`) consults `refuse::TABLE` under `Sql`,
  and a new `refuse::GQL_TABLE` under `Gql`. So `OFFSET`, `CASE`, `COALESCE`,
  `NODES`, `EDGES`, `PATH_LENGTH`, `ARRAY_AGG`, `TRAIL` and `WALK` stop being
  refused INSIDE a GQL body. They are unchanged everywhere else. This matters
  because today `qualified_name()` refuses `NODES` even as a property name.
* GQL words are recognised by position (brief §5: "syntactic keyword
  recognition within GQL context rather than globally reserving every
  word"). `RETURN`, `NEXT`, `LET`, `FILTER`, `FOR` start statements;
  `COST`, `WHERE` and the arrows are recognised inside element patterns;
  `ANY`, `SHORTEST`, `CHEAPEST`, `WALK`, `TRAIL`, `ACYCLIC` only after
  `MATCH` or `p =`. None of them is added to the SQL keyword space.
* Audit (brief §10): accepting `CASE` or `UNION` inside a GQL body must not
  change an SQL branch. Test M2-A drives every `refuse::TABLE` row through
  the existing `refusal_by_name.rs` harness, both outside and inside a GQL
  body.

### 5.4 Refusals that change, and refusals that stay

| Construct | Legacy body and plain SQL | Inside a GQL body |
| --- | --- | --- |
| `RETURN` in `GRAPH_TABLE` | "Not adopted" (`QL_CONTRACT` §1) | Accepted (M2); §1 amended |
| Label alternation `A\|B` | T2 refusal (unchanged) | M2 |
| Non-key seeds, row-bound inline predicates | Refused (unchanged) | M2 (charged row reads; brief §4.3) |
| Two `ORDER BY` keys | Deviation 3 (unchanged for collection SELECTs) | M3, and in the outer SELECT over a GQL relation |
| `OFFSET` | Deviation 4 (unchanged) | M3: a bounded skip over the working table, cost printed |
| `CASE`, `COALESCE`, `NULLIF`, `CAST`, math functions | T2 (unchanged in SQL) | M3 |
| `COUNT(DISTINCT x)`, `ARRAY_AGG`, `ARRAY_LENGTH` | T2 (unchanged in SQL) | M3 |
| `NEXT`, `LET`, `FILTER`, `FOR` | Not SQL words | M3 |
| `{m,n}` with `m = 0`, `*`, open upper bound | Legacy clamps to `min 1` and caps at 16 (unchanged, documented as legacy) | M4, exact semantics |
| `WALK`, `TRAIL`, `ACYCLIC` | T3 (unchanged) | M4 |
| `p = ...`, `PATH_LENGTH`, `PATH_FIRST`, `PATH_LAST`, `NODES`, `EDGES` | T2 (unchanged) | M4 |
| `ANY`, `ANY SHORTEST`, `ANY CHEAPEST ... COST` | T2 (unchanged) | M4 |
| `ALL SHORTEST`, `SIMPLE`, label `&`/`!`, nested quantifiers | Refused | Still refused, GQL tier "P1", named |
| `OPTIONAL MATCH`, `EXISTS {}`, `CALL`, `UNION` inside the body | n/a | Refused by name, "M5" (unless Q3 moves `OPTIONAL` into M3) |
| `path_sum/product/...`, `VERTEX_ID`, `EDGE_ID` | T2 (unchanged) | Refused, pointing to `SUM` over a group and `ELEMENT_ID` |
| `{1,$2}` | n/a | Syntax error naming the literal-bound rule |
| Seed key not found | Engine error (legacy, unchanged) | Empty result (brief §2 difference 3) |

### 5.5 The outer SELECT over a GQL relation

`SELECT g.a, g.b FROM GRAPH_TABLE (...) AS g [WHERE ...] [GROUP BY ...]
[ORDER BY k1, k2 ...] [OFFSET n] [LIMIT n|$n]`

* `select()` pre-scans for `FROM GRAPH_TABLE (` at depth 0 with a GQL body
  BEFORE it parses the select list. A GQL relation's select list is parsed
  with the GQL expression grammar over the alias (`g.col`). A collection
  SELECT keeps the existing grammar byte for byte.
* The outer clauses compile as ONE MORE STAGE appended to the plan:
  `Filter -> [Aggregate] -> Project -> [Sort] -> [Page]`. Consequently,
  multi-key `ORDER BY` and `LIMIT $n` over a GQL relation use the same
  bounded operators as `RETURN ... ORDER BY` inside the body.
* An outer `WHERE` is NOT pushed into the search (brief §8.3: "An outer SQL
  filter after `GRAPH_TABLE` ... is not automatically a constraint on the
  search").
* `JOIN` with a GQL relation stays refused by name (`QL_CONTRACT` §4.8).

---

## 6. Output (M1, M2)

### 6.1 Rows and columns

* `compile::Plan` gets `Gql(GqlSqlPlan)` and `ExplainGql(GqlSqlPlan)`.
  `GqlSqlPlan` holds the core `GqlPlan`, lang's compiled expression tables
  (it implements `GqlHost`), the final schema and the parameter table.
* `PreparedSql::columns()` returns the final names.
  `PreparedSql::column_type(at)` returns a declared spelling for EVERY
  column, from the final schema: `TEXT`, `BIGINT`, `DOUBLE PRECISION`,
  `BOOLEAN`, `JSONB`, `TIMESTAMPTZ`, `DATE`, `VECTOR`, `GEOMETRY`, `BYTEA`,
  and for lists `TEXT[]`, `BIGINT[]`, `DOUBLE PRECISION[]`, `BOOLEAN[]`,
  `TIMESTAMPTZ[]`, `DATE[]` (each item printed as its scalar column prints
  it), `JSONB[]`. This uses the existing hook: the wire already types a column by
  `column_type` first (`dist/src/pg/connection.rs::field_descriptions`), and
  the rule that types are data-independent (decided at describe, never from
  values) is preserved. `source_collection()` returns `None`.
* `is_select()` is true and `is_aggregate()` is false (aggregation is inside
  the plan). `with_query`/`with_aggregate` refuse by name ("a GQL plan has no
  single prepared query"). `for_each_row_with` pages a `GqlCursor`, so
  `Db::stream`, portals and declared cursors work unchanged.
* A missing seed or a disconnected target gives zero rows with the same
  typed columns (matrix "Missing seed/disconnected target").

### 6.2 No fabricated owner (Q1)

`SqlRow { id: EntityId, values }` and the published `sekejap::Row { id:
EntityId, .. }` both require an id. The aggregate path already fabricates one
(`compile/aggregate.rs::group_identity`), and so does the catalog path
(`(collection 0, ordinal)`). Law 8 keeps published interfaces compatible
across minor releases, so the field cannot become `Option` in 0.18.
Recommended:

* Add `EntityId::NO_OWNER`, a documented sentinel that no stored row can
  carry: `CollectionId(0)` with `u64::MAX`. (Check that no collection id 0 is
  ever allocated; `compile/rows.rs` already uses collection 0 for virtual
  rows.)
* Add `SqlRow::owner() -> Option<EntityId>` and `sekejap::Row::owner()`,
  which return `None` for the sentinel.
* GQL rows, and in the same change aggregate and catalog rows, carry the
  sentinel. `.id` is documented as deprecated for relation rows. Making the
  field an `Option` is left to a future major version.

### 6.3 Lists

* A list result is `SqlValue::Json(Value::Array(..))`: elements are JSON
  scalars and `Null` elements are JSON null. No new `SqlValue` variant is
  added (a new variant would break exhaustive matches downstream).
  `sekejap::value_to_json` then yields a JSON array, which is natural for
  Rust and FFI callers.
* Wire: `oid_for_declared` learns the array spellings and returns
  `_text` 1009, `_int8` 1016, `_float8` 1022, `_bool` 1000, `_jsonb` 3807. The
  cell encoder writes a JSON array under an array OID as a PostgreSQL array:
  text form `{a,"b c",NULL}` with PostgreSQL quoting; binary form a
  one-dimensional array header (ndim 1, has-null flag, element OID, length,
  lower bound 1). An empty list is `{}`. A nested list in a final column is
  refused at compile time in v1, because PostgreSQL arrays are rectangular.
* Node, edge and path values never reach the wire (brief §7 rule 10).

---

## 7. Prepared statements (M3; seeds in M2)

Brief §7 "Names have distinct jobs", §7 last paragraph.

* **One parameter namespace.** The binder keeps one `ParamTable: Vec<Option<
  ValueType>>` for the whole statement: outer SELECT, every stage, every
  inline predicate. Each `$n` use unifies with the type its position needs.
  A conflict (`$1` used as a text key and as a number) fails at PREPARE with
  `SqlError::Parameter` naming both positions (matrix "Prepared rebinding").
  `PreparedSql::param_types()` (new, additive) exposes the result so the
  wire can answer `ParameterDescription` with real OIDs instead of `text`
  for undeclared positions.
* **Parameters are values only.** A graph name, label, edge type, property
  name, variable or quantifier bound written as `$n` is a syntax error that
  names the position.
* **Always rebindable.** A GQL plan folds no parameter value at prepare.
  Expressions hold `Expr::Param(n)`. When an execution opens it converts the
  bound `Param`s into `BindingValue`s once, checking each against its
  `ValueType`. `LIMIT`/`OFFSET` values must be integers from 0 to `i64::MAX`.
  `PreparedSql::bind` for a `Plan::Gql` swaps the parameter vector and
  compiles nothing. `now()` is read per EXECUTION, not per compile: every row
  of one answer still sees one instant, and a rebind gets a new instant
  without recompiling.
* **Seeds re-resolve per execution.** A plan holds no `EntityId`. A key seed
  is a mapping lookup under the execution's snapshot, once per input row, so
  a row deleted and reinserted between executions is found under its new
  identity, and a deleted row gives an empty seed stream. Brief §2 difference
  3 and the matrix "Delete/reinsert" row.
* **Empty seed stream, not an error:** a key with no row, a key of the wrong
  collection, and a `NULL` key all produce no row.
* **Catalog-dependent names.** Labels (collections) resolve at prepare. A
  missing collection is an error, and DDL bumps the plan-cache generation
  (`dist/rust/src/plans.rs`). Edge types and graph contexts are interned by
  WRITES, which do not bump the generation. So an edge type or context
  unknown at prepare compiles to "matches nothing" plus a notice, and is
  looked up again (one name lookup) each time an execution opens. A type
  created later is therefore seen without a recompile.
* **List parameters.** `FOR x IN $1` needs a list. `Param::Json(array)` binds
  it, and the wire decodes array-OID parameters to `Param::Json(array)`.
  `Param::Vector` is NOT a list: binding it in list position is a type error
  (brief §7: keep a vector parameter distinct from a list of seed keys).
  `x IN $2` with a list parameter is an identity or value membership test.
* **Plan cache.** Unchanged. A GQL statement is cached like any other, and a
  hit is a rebind.

---

## 8. EXPLAIN

`lang/src/explain.rs` renders `GqlPlanDescription`, which core builds from
`Operator::describe`. It prints:

* one block per stage, with the stage's slot schema (name, type, nullable,
  provenance);
* per operator: its kind, how its seed is accessed (key, index with the
  index name, bound, SCAN), where each predicate is evaluated (in the
  pattern, per edge, per far node, as a post-filter, or re-evaluated per
  input row), and what it holds;
* the path selector, mode and algorithm (enumeration, product BFS, Dijkstra,
  or `Reach` with the rule numbers of 4.6 that admitted it);
* the budget resources that can stop each operator, and rebind status
  ("always rebindable", plus names re-resolved at open);
* after a run, `GqlWork`.

No multi-stage plan is described as "a traversal" (brief §11).

---

## 9. Contract amendments this work carries

Each is part of the task that makes it true, not a separate task:

* `docs/LAYERS.md`: the evaluation rule of section 1.1 (M1-D).
* `docs/lang/QL_CONTRACT.md` §1: replace "Google's `RETURN` ... not adopted"
  with the profile, and add a new section "GQL profile" listing each
  construct with tier, test and atomic, in the §4.3 table style. §4.3 is
  marked as the legacy profile. §5 deviations 3 and 4 are scoped to
  collection SELECTs (M1-D, then filled per milestone).
* `docs/core/GRAPH_CONTRACT.md` §4.1 and §4.3: state that the node-dedup BFS
  is the reachability atomic, not GQL `ACYCLIC` enumeration (brief §2
  difference 1). Amend §4.3 for the GQL profile: row-bound per-element
  predicates are allowed after a seed and charged as `primary_reads`. The
  legacy refusal is unchanged (M2-C).
* `docs/lang/CONTRACT_TEST_MAP.md`: one row per new T1 construct (each
  milestone).

---

## 10. Work breakdown

### 10.1 Tasks

The paths are the files each task creates (new) or edits. **Every path is
inside an existing crate (owner constraint, section 1.2). No task creates a
crate, a `Cargo.toml`, a workspace member or a top-level folder; a task that
seems to need one is a design change and goes back to the owner.** New test
files go in the existing per-crate `tests/` directories. "Needs" lists hard
dependencies. Every task writes its tests first and sees them fail
(project rule). The acceptance tests are the brief §11 matrix rows named in
the last column.

**M1: values, identity, schema, output encoding**

| Id | Task | Files | Needs | Acceptance (matrix rows and own tests) |
| --- | --- | --- | --- | --- |
| M1-A | Core value types: `NodeRef`, `EdgeRef`, `PathRef`, `BindingValue`, `ListRef`, `BindingRow`, `SlotId`, `ValueType`; identity eq, hash, total order, `held_bytes` | new `core/engine/src/query/gql/{mod,value}.rs`; one line in `core/engine/src/query/mod.rs` | EdgeId interface (2.6); a stub `EdgeId` until it lands | "Two collections sharing `_key`": distinct, identity equality. Own: path prefix sharing, zero-length path, order and hash consistency. `core/engine/tests/gql_values.rs` |
| M1-B | Budgets: `GqlBudget`, `GqlWork`, `GqlMeter`, new `WorkResource` variants, memory caps not lifted by `unlimited()` | new `core/engine/src/query/gql/budget.rs`; `core/engine/src/query/mod.rs` (enum variants and `slot`) | M1-A (module only) | "Budget exhaustion / cancel" (unit level): each resource refuses by name with limit and attempted. `core/engine/tests/gql_budget.rs` |
| M1-C | `ElementReader`: node property read (charged `primary_reads`), edge bag (outgoing free, incoming primary read charged `graph_edges`), labels, property names with presence, `ELEMENT_ID` encoding; the `AdjacencyCursor` factored out of the two existing per-edge loops, with EdgeId | new `core/engine/src/query/gql/reader.rs`, new `core/engine/src/index/graph/adjacency.rs`; edits `core/engine/src/query/drivers.rs`, `core/engine/src/index/graph/mod.rs` (call the shared cursor; no behaviour change) | M1-A, M1-B, EdgeId work merged | "Parallel edges" (read side): both edges are produced. "Reverse-edge property access": correct bag, charged. Existing graph tests unchanged (regression gate). |
| M1-D | lang schema and conversions: `BindingSchema`, `SlotInfo`, `Provenance`; `BindingValue <-> SqlValue/Param/ProjectedValue`; contract text for `LAYERS.md` and `QL_CONTRACT.md` §1/§5 scoping | new `lang/src/gql/{mod,schema,convert}.rs`; `docs/LAYERS.md`, `docs/lang/QL_CONTRACT.md` | M1-A | Own: `Missing` becomes `Null` in GQL and stays `Missing` in SQL; list converts to a JSON array. |
| M1-E | Output plumbing: `EntityId::NO_OWNER`, `SqlRow::owner()`, `Row::owner()`; wire array OIDs, array cell encoding (text and binary), array parameter decoding; `PreparedSql::param_types()` hook | `core/engine/src/collections/mod.rs` (constant), `lang/src/lib.rs`, `dist/rust/src/rows.rs`, `dist/src/pg/types.rs`, `dist/src/pg/connection.rs` | none (independent of M1-A) | Own: `dist/tests/pg_wire_gql_types.rs`: a `TEXT[]` column round-trips in text and binary, with an empty array, null elements and quoting; an array parameter decodes to `Param::Json`. |

**M2: general patterns**

| Id | Task | Files | Needs | Acceptance |
| --- | --- | --- | --- | --- |
| M2-A | Parser: body-kind pre-scan and dispatch; `Dialect` flag; `refuse::GQL_TABLE`; GQL AST; pattern grammar (labels `IS`/`:`, alternation, directions, inline `WHERE`, comma patterns, repeated variables); M2 expression subset; outer-SELECT pre-scan | new `lang/src/gql/ast.rs`, `lang/src/gql/parse/{mod,pattern,expr,stage}.rs`; edits `lang/src/parser/{mod,graph_table,select}.rs`, `lang/src/refuse.rs`, `lang/src/ast.rs` (`Source::Gql` only) | M1-D (types only) | "Legacy queries": every existing `lang/tests` graph test and refusal is unchanged. New `lang/tests/gql_parse.rs`: both label spellings, anonymous `IS` edges, alternation, hybrid refusal, every `refuse::TABLE` row inside and outside a GQL body. |
| M2-B | Core operators: `Unit`, `Seed` (Key, Index via host, Bound, Scan), `Expand` / `ExpandInto` (types, Both with self-loop rule, far label test, filters through the host), `Filter`, `Let`, `Project`; `GqlCursor::next_page` with poison-on-error | new `core/engine/src/query/gql/{host,plan,cursor}.rs`, `core/engine/src/query/gql/ops/{mod,seed,expand,filter,project}.rs`, `core/engine/src/query/gql/testing.rs` | M1-A, M1-B, M1-C | "Parallel edges" (match side), "Self-loop" (either direction, no duplicate orientation), "Reverse-edge property access". Own: `core/engine/tests/gql_expand.rs` compares against a brute-force hop oracle on random multigraphs, as bags. |
| M2-C | Binder and planner for patterns: slot allocation, repeated-variable identity constraints, label resolution, seed choice (key, scalar index equality/range via `open_seed`, bound, scan under admission), predicate placement (inline edge and node predicates per hop; pattern `WHERE` after the pattern), unknown edge types and contexts re-resolved at open; `GRAPH_CONTRACT` §4.3 amendment | new `lang/src/gql/{bind,plan,expr,eval}.rs`; `docs/core/GRAPH_CONTRACT.md` | M2-A, M2-B interfaces | The brief §1 opening example, on neutral fixtures. "Missing seed/disconnected target". "Two collections sharing `_key`" through SQL. New `lang/tests/gql_patterns.rs`. |
| M2-E | Remove the SQL/PGQ `MATCH ... COLUMNS` body (owner decision 1): its parser, compiler, refusals and `Stmt`/AST variants; re-express every property its tests pinned (`sql_tier1.rs`, `aggregate_graph_adversarial.rs`, `sql_prepared.rs`, `sql_explain.rs` graph cases) as GQL tests; update `QL_CONTRACT.md` §4.3 and the docs' graph examples | `lang/src/parser/graph_table.rs`, `lang/src/compile/graph_table.rs`, `lang/src/ast.rs`, `lang/src/refuse.rs`, those test files, docs | M2-C, M2-D | No `COLUMNS` graph path remains; every re-expressed property passes on the GQL path; `doc_examples` green. |
| M2-D | `Plan::Gql` in `PreparedSql` (`columns`, `column_type`, `for_each_row_with`, `run`, `bind`), `EXPLAIN` for GQL, wire describe | `lang/src/lib.rs`, `lang/src/compile/{mod,plan}.rs`, `lang/src/explain.rs`, `dist/src/pg/connection.rs` | M2-C, M1-E | Own: `dist/tests/pg_wire_gql.rs`: a GQL statement through `Parse`/`Describe`/`Execute` has typed columns and zero rows for a missing seed. `lang/tests/gql_explain.rs`. |

**M3: working-table composition**

| Id | Task | Files | Needs | Acceptance |
| --- | --- | --- | --- | --- |
| M3-A | Core blocking operators: `Aggregate` (all P0 accumulators including `COUNT(DISTINCT)` and `ARRAY_AGG`), `Distinct`, `Sort` (multi-key, stable, top-k), `Page`, `Unnest`; charges and caps | new `core/engine/src/query/gql/ops/{aggregate,distinct,sort,page,unnest}.rs` | M2-B | "Empty-input aggregate". Own: `core/engine/tests/gql_blocking.rs`: every operator refuses at its cap with the named resource; stable ties. |
| M3-B | Stage grammar and binding: `LET`, `FILTER`, `FOR`, `RETURN [DISTINCT]`, implicit and explicit `GROUP BY`, `ORDER BY` multi-key, `OFFSET`, `LIMIT`, `NEXT`; scope across `NEXT`; vertical aggregates | `lang/src/gql/parse/stage.rs`, `lang/src/gql/{bind,plan}.rs` | M2-C, M3-A | "NEXT dropping a variable", "NEXT aggregate over many inputs", brief §9.2 (with plain `MATCH` in stage 2 unless Q3 is approved), §9.4 (batched seeds via `FOR`). `lang/tests/gql_pipeline.rs`. |
| M3-C | Expression pack P0: `CASE`, `COALESCE`, `NULLIF`, `CAST`/`::`, arithmetic, `ABS`/`SQRT`/`POWER`/`EXP`/`LN`, string functions reused from `lang/src/functions.rs`, `IN`, `IS [NOT] NULL`, comparisons, three-valued logic | `lang/src/gql/{expr,eval}.rs`; `lang/src/functions.rs` (share, do not fork) | M2-C | Own: `lang/tests/gql_expressions.rs`: null propagation table, `LN` domain error, `CASE` without `ELSE` gives null. |
| M3-D | Outer SELECT over a GQL relation compiled as the tail stage; parameter table with cross-scope type unification; list parameters; `LIMIT`/`OFFSET` `$n` range checks; `param_types()` | `lang/src/parser/select.rs`, `lang/src/gql/{bind,plan}.rs`, `lang/src/compile/bind.rs` | M3-B | "Prepared rebinding" (one slot, one type, inside and outside), "Delete/reinsert between executions". `lang/tests/gql_prepared.rs`. |
| M3-E | Paging and cancellation end to end: pages equal the one-shot result; poison after a refusal; wire portal, `DECLARE`/`FETCH`, `CancelRequest`, `statement_timeout` | `lang/src/lib.rs`, `dist/src/pg/connection.rs` (only if gaps show) | M3-A..D | "Multiple pages", "Budget exhaustion / cancel" (named failure plus counters; a truncated stream never reports done). `dist/tests/pg_wire_gql.rs` (extended). |
| M3-F (Q3) | `OptionalApply` plus `OPTIONAL MATCH` grammar | `core/engine/src/query/gql/ops/optional.rs`, `lang/src/gql/*` | M3-B | "Optional predicate placement", brief §9.2 as written, §9.6. |

**M4: paths**

| Id | Task | Files | Needs | Acceptance |
| --- | --- | --- | --- | --- |
| M4-A | Automaton compiler: quantifiers including zero and unbounded, quantified edges, parenthesized subpaths, concatenation rules, positional labels, path modes, selectors, `COST` coverage, admission rules, nested-quantifier refusal, group-variable typing | new `lang/src/gql/automaton.rs`; `lang/src/gql/parse/pattern.rs`; `lang/src/gql/bind.rs` | M2-C | Own: `lang/tests/gql_automaton.rs` checks automaton shapes; `WALK` with `*` and no selector is refused; `{1,$2}` is refused. |
| M4-B | `PathSearch::Enumerate`: DFS over product states, `PathRef` cons-list, per-path `TRAIL`/`ACYCLIC` checks, group lists, binding dedup (Q11) | new `core/engine/src/query/gql/paths/{mod,automaton,enumerate}.rs` | M2-B, M4-A (automaton data type only) | "Diamond graph", "Direct edge plus two-hop route", "Self-loop and directed cycle" (three mode sets), "`{0,0}`, `{0,1}`, `?`, `*`", "Multi-type quantified subpath", "Quantified endpoint labels". |
| M4-C | `PathSearch::Shortest` (product BFS, predecessor arcs, point-to-point stop) and `PathSearch::Cheapest` (Dijkstra, cost validation, deterministic ties) | new `core/engine/src/query/gql/paths/{bfs,dijkstra}.rs` | M4-B | "Two equal shortest paths" (ANY returns one witness of minimum length), "Weighted bounded counterexample" (cost 12), "Invalid cost values", brief §8.1 and §8.2 examples on neutral fixtures. |
| M4-D | Path functions and horizontal aggregation: `PATH_LENGTH`, `PATH_FIRST`, `PATH_LAST`, `NODES`, `EDGES`, `IS_ACYCLIC`, `IS_TRAIL`, `ELEMENT_ID`, `SOURCE_NODE_ID`, `DESTINATION_NODE_ID`, `LABELS`, `PROPERTY_NAMES`, `ARRAY_LENGTH`; horizontal/vertical classification | `lang/src/gql/{expr,eval,bind}.rs` | M4-B, M3-C | "Horizontal vs vertical aggregation" (path sum differs from sum over paths; no last-edge value), brief §6 horizontal example on neutral fixtures. |
| M4-E | Oracle suite: tiny exhaustive reference enumerator, plus seeded random multigraphs (parallel edges, self-loops, cycles) comparing result BAGS for every mode, selector and quantifier shape | new `core/engine/tests/gql_paths_oracle.rs`, `lang/tests/gql_paths.rs` | M4-B, M4-C | Brief §11 closing paragraph: bags as well as sets. |
| M4-F | `Reach`: the seven-rule equivalence test in the planner, `Reach` operator over the existing BFS, `EXPLAIN` of the proof; `GRAPH_CONTRACT` §4.1 terminology fix | new `core/engine/src/query/gql/ops/reach.rs`; `lang/src/gql/plan.rs`; `docs/core/GRAPH_CONTRACT.md` | M4-B | For each rule, one query where the rule fails and `Reach` is NOT chosen, and whose answer differs from what the node BFS would give. `lo = 1` with a cycle back to the seed under `WALK` versus `ACYCLIC`. A diamond under a `DISTINCT` endpoint gives the same answer both ways. |

### 10.2 Dependency order

```
EdgeId work ─┐
M1-A ─┬─ M1-B ─┬─ M1-C ─┬─ M2-B ─┬─ M3-A ─┬─ M3-B ─┬─ M3-D ─ M3-E
      │        │        │        │        │        └─ M3-F (Q3)
      └─ M1-D ─┴─ M2-A ─┴─ M2-C ─┴─ M2-D  └─ M4-B ─┬─ M4-C
                                   │               ├─ M4-D (also needs M3-C)
M1-E (independent) ────────────────┘               ├─ M4-E
                         M2-C ── M3-C              └─ M4-F
                         M2-C ── M4-A ── M4-B
```

### 10.3 What can run in parallel worktrees without conflicting edits

The rule that makes this work: **new files per task, and at most one
shared-file edit per task, named in advance.**

| Wave | Parallel worktrees | Shared-file edits (the only ones) |
| --- | --- | --- |
| 1 | M1-A + M1-B (one worktree: they share `query/mod.rs`); M1-E; M1-D | `core/engine/src/query/mod.rs` (M1-A/B: `mod gql;` and the enum variants); `lang/src/lib.rs` (M1-E: `owner()`). M1-D touches only new files and docs. |
| 2 | M1-C; M2-A | M1-C: `query/drivers.rs`, `index/graph/mod.rs`. M2-A: `lang/src/parser/*`, `refuse.rs`, one variant in `lang/src/ast.rs`. No overlap. |
| 3 | M2-B; M2-C (against M2-B's `host.rs`/`plan.rs` interface, frozen at the start of the wave) | M2-B: none (new files). M2-C: `docs/core/GRAPH_CONTRACT.md`. |
| 4 | M2-D; M3-A; M3-C; M4-A | M2-D: `lang/src/lib.rs`, `compile/{mod,plan}.rs`, `explain.rs`. M3-A, M3-C, M4-A: new files, plus `lang/src/gql/*` for M3-C and M4-A (different files: `expr`/`eval` versus `automaton`/`parse/pattern`). |
| 5 | M3-B; M4-B | M3-B: `lang/src/gql/{bind,plan}.rs`, `parse/stage.rs`. M4-B: core new files only. |
| 6 | M3-D; M4-C; M4-D (after M3-C); M4-E | M3-D and M4-D both touch `lang/src/gql/bind.rs`. Sequence them, or have M4-D add its classification in a new `lang/src/gql/horizontal.rs` and call it from one line in `bind.rs`. |
| 7 | M3-E; M3-F; M4-F | M4-F: `lang/src/gql/plan.rs`; M3-F: `lang/src/gql/{bind,plan}.rs`. These conflict, so sequence them. |

Before wave 3, one short task should freeze the interface files
(`query/gql/{value,host,plan,budget}.rs`) as compile-checked stubs. The
design stops short of doing that, to avoid running builds as part of a
design task (see the report).

### 10.4 Regression gates (every task)

The existing graph, prepared, hybrid, budget and PG-protocol tests stay
green (brief §11 closing paragraph): `lang/tests/sql_tier1.rs`,
`aggregate_graph_adversarial.rs`, `sql_prepared.rs`, `sql_refusals.rs`,
`refusal_by_name.rs`, `sql_explain.rs`, `dist/tests/pg_wire*.rs`, and the
core graph tests. The commands are the ones in `AGENTS.md`, run only when
the owner asks for test runs.

---

## 11. Open questions for the owner

Each has a recommended answer. "Uncertain" marks where the brief or the
public references may not settle it.

| # | Question | Recommendation |
| --- | --- | --- |
| Q1 | A GQL result row has no owning entity, but `SqlRow.id` and `sekejap::Row.id` are non-optional public fields. | Keep the fields (Law 8, minor release). Use the documented `EntityId::NO_OWNER` sentinel and add `owner() -> Option<EntityId>`. Apply the same to aggregate and catalog rows. Make the field optional only in a major release. |
| Q2 | How do lists reach callers? | `SqlValue::Json(array)`, typed `T[]` by `column_type`, and a PostgreSQL array on the wire. No new `SqlValue` variant in 0.18. |
| Q3 | Brief §11 step 3 asks for the two-stage collaborator example (§9.2), but that example uses `OPTIONAL MATCH`, which is step 5. | Pull `OptionalApply` (single pattern, no `EXISTS`) into M3 as M3-F. It is small, and without it the M3 gate cannot run the example as written. |
| Q4 | Is a zero `COST` an error? The brief says "positive-finite" and lists zero among the invalid values, and Dijkstra itself handles zero. | Reject zero (and negative, null, NaN and infinity) with `InvalidPathCost`, following the brief's wording. Uncertain: check Google's current `ANY CHEAPEST` documentation before publishing. |
| Q5 | A label-scan seed with a predicate no index answers (`MATCH (n IS T) WHERE n.unindexed = 1`). | Keep `QL_CONTRACT` §6: refuse it and name the missing index, as SQL does. Row-bound predicates are allowed only on elements reached from a seed (brief §4.3), and a bare scan seed with no predicate is allowed and labelled a SCAN. |
| Q6 | Default ceilings for the new resources. | Memory: `sort_bytes` = `list_bytes` = `RUN_BYTES` (8 MiB); `queue_entries` and `predecessor_arcs` = `RUN_BYTES` / entry size; not lifted by `unlimited()`. Work: unlimited by default and bounded by the caller's budget, as today. The SQL entry points pass the same `GRAPH_EDGES`-scale defaults (`lang/src/lib.rs`) as the legacy graph path. |
| Q7 | Resuming a GQL answer across re-prepares (a continuation token). | Not in v1. Pages come from one live cursor, one snapshot. Revisit only if a caller needs stateless paging. |
| Q8 | The output type of a property that is undeclared in its collection, or declared with different kinds across a label alternation. | `TEXT` on the wire (today's rule for undeclared columns) and `Json` for evaluation, with a compile notice. Alternative: `JSONB`. |
| Q9 | `ARRAY_AGG` over an empty group list (horizontal), and over zero rows (vertical). | Horizontal: an empty list, so `ARRAY_LENGTH` is 0 on a zero-length path. Vertical with no rows and no grouping: `NULL`, as in SQL. Uncertain: Google's behaviour for the horizontal case is unverified. |
| Q10 | Nested quantifiers (a quantifier inside a quantified group). | Refuse by name in v1; one active counter keeps the state small. Revisit with a verified use case. |
| Q11 | Two automaton runs that yield the same path and the same bindings. | Deduplicate per input row, and keep distinct bindings as distinct matches. Uncertain: confirm against the normative ISO text before any conformance statement. |
| Q12 | Ordering across types, and ordering of node/edge values in `ORDER BY`. | Refuse `ORDER BY` on a node, edge, path or list at compile time (make the user order by a property or by `ELEMENT_ID`). Within scalars, refuse mixed types unless they are all numeric. The total order in 2.2 is then used only internally (distinct, group, stable ties). |
| Q13 | `LIMIT`/`OFFSET` parameter range. | An integer from 0 to `i64::MAX`. A negative value, a non-integer or a null is a bind-time `SqlError::Parameter`. `LIMIT 0` is legal and returns nothing. |
| Q14 | `LN(x)` for `x <= 0`, and division by zero, inside GQL expressions. | Follow PostgreSQL: raise an error (`2201E` for a logarithm, `22012` for division by zero), never `NULL`. The existing `ScoreExpr` rule (division by zero gives NaN) stays unchanged in `ORDER BY` scores, and the difference is stated. |
| Q15 | The graph name argument: only `base`, or any named context? | Any named context, resolved like the legacy path. The node universe is every visible collection row, including isolated rows (brief §4.1). `base` is context 0. |
| Q16 | Whether `ELEMENT_ID` is stable across queries. | Promise query-scope uniqueness only (brief §6). The encoding is versioned (`n1:`/`e1:`) so it can change. |

---

## 12. Known limits of this design (stated, not hidden)

* No spill. Every blocking operator refuses past its memory cap.
* No cross-execution resume. A refused page poisons its cursor.
* An uncorrelated `MATCH` in a later stage is re-run per input row.
* Path mode checks cost O(L) per extension, and there is no dominance
  pruning in Dijkstra.
* Selector plus path mode combinations and nested quantifiers are refused.
* Index lineage (text, spatial and vector seeds through aliases and stages)
  is designed as a hook (`GqlHost::open_seed`) but lowered only for scalar
  indexes in M2. The rest is M6.
* Conformance to ISO GQL is not claimed. Section 4.2's deduplication rule
  and Q4, Q9 and Q11 need checking against normative or vendor texts before
  any feature is advertised.
