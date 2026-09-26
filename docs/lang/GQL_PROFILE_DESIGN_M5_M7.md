# GQL query profile: engineering design for milestones M5-M7

Status: DESIGN, not built. Nothing in this document is Tier 1 until a named
test proves it (`docs/lang/QL_CONTRACT.md`, "The tiers"). It continues
`docs/lang/GQL_PROFILE_DESIGN.md` (M1-M4, "the M1-M4 design" below), keeps its
section conventions and its open-question numbering (this document starts at
Q17), and marks every point that is uncertain.

Source of the requirements: the owner's GQL profile brief, v0.1, which is not
kept in this repository. Citations of the form "brief §9" refer to its section
numbers. Milestones M5-M7 are its §11 steps 5-7:

| Milestone | Brief §11 step | One-line goal |
| --- | --- | --- |
| M5 | 5 | Composition completion: `OPTIONAL MATCH` over several patterns, `EXISTS { }` / `NOT EXISTS { }`, scoped `CALL (...) { }`, `UNION` / `UNION ALL`, seeded row predicates everywhere; the workload fixtures end to end |
| M6 | 6 | Hybrid execution and PostgreSQL integration: spatial, vector and text inside GQL through their indexes, index lineage, exact versus approximate scoring, complete-or-error accounting, parameters, portals and arrays over all of it |
| M7 | 7 | Publish the profile: a checked feature registry, the standard mapping and dialect differences, runnable examples, an EXPLAIN reference, compatibility notes, and the list of unsupported features with their named refusals |

What this document does NOT claim: ISO/IEC 39075 conformance, Google Spanner
compatibility, or PostgreSQL conformance. As in the M1-M4 design, the profile
adopts selected ISO GQL constructs, Google's documented `GRAPH_TABLE ...
RETURN` embedding, and sekejap's PostgreSQL host surface.

Workload names in this document are invented, in the tourism world the README
uses (`README.md`, "Interfaces"): sites such as a temple, a beach and a
market; dance troupes and dancers; skills and jobs; reef and beach incidents.
Public place and dance names (Uluwatu, Seminyak, Kecak, Legong) are used as
well-known terms only; no business, person or brand is named.

**Owner decisions that apply to this document (2026-09-26):**

1. **Versions stay with the owner.** No task in M5-M7 changes a version,
   tags a release or proposes one. Section 4.8 ends with a release checklist
   the owner runs.
2. **Vector search inside GQL is EXACT unless the query sets `ef_search`.**
   Section 3.6 states what that means for seeds, predicates and ordering,
   and one place where the SQL side behaves differently today.
3. **Final P0 acceptance** is the full suites on Linux, then ONE 1M paired
   run on the acceptance device (a small ARM board) plus a resource-limited
   Linux server comparator. No 48M run. Absolute times across devices are not compared as
   an engine speedup.
4. **Example and test data** use the README's tourism world with well-known
   terms, and never pork, alcohol or haram wording, or real business names.
5. **PostgreSQL semantics** wherever the behaviour is not graph-specific
   (`UNION` column rules, `NULL` ordering, error codes, parameter types).
6. **The portal lifetime across `Sync` is a parked owner decision.** No task
   here changes it. Tests pin the current behaviour only where it does not
   decide the parked question (section 3.8).

Carried from the M1-M4 design and still binding: no legacy `COLUMNS` body (it
is removed, M2-E); nothing unused (no dead code, shims, aliases or planned
spellings; a temporary item is deleted in the same milestone); no new crates,
no new `Cargo.toml`, no new top-level folders; the owner's answers to Q1-Q16
(all sixteen recommendations adopted); tests first, seen failing.

---

## 0. The decisions in brief

1. **One apply family in core.** `OPTIONAL MATCH` already runs as
   `OpSpec::OptionalApply` over an `OpSpec::Argument` leaf
   (`core/engine/src/query/gql/ops/optional.rs`). M5 adds two siblings that
   reuse the same leaf: `ExistsApply` (semi-join and anti-join, plus a "mark"
   form that writes a boolean) and `CallApply` (a lateral subquery whose inner
   side may hold blocking operators). `UNION` is a separate `Union` operator
   over branch plans. Section 2.
2. **Branches of a union are single stages; `NEXT` binds looser.**
   `A UNION B NEXT C` is `(A UNION B) NEXT C`, as in ISO GQL, where a set
   operation combines linear queries and `NEXT` chains the results.
3. **Host functions enter GQL through the parser's existing sub-parsers.**
   Pure ones (spatial predicates, vector distances, geometry constructors)
   are evaluated by lang over values core read. The ones that need a keyspace
   (`@@` against a text index, `bm25`) are evaluated by core, through new
   `ElementReader` methods. Section 3.
4. **Index seeds carry every index-answerable conjunct and an order.** The
   M2 `IndexSeed` (one scalar comparison) becomes a list of engine filters
   plus an optional engine order, prepared through the existing
   `Database::prepare_query`. The seed can also write the page's ranking
   value into a hidden slot, so a `LET` that repeats the ranking reads it
   instead of computing it again.
5. **Index lineage is a compile-time fact on each slot.** A column remembers
   "comes from property `f` of node `n`" or "is ranking expression `r` of
   node `n`" through aliases, `LET`, `RETURN`, `NEXT` and the outer
   `SELECT`. The planner uses it for exactly two things: moving a later
   conjunct into the seed when that is semantically neutral, and seeding in
   index order so a top-k sort can stop early. Section 3.5.
6. **Exact unless `ef_search`.** An approximate index is used only when the
   statement's session set `ef_search` (section 3.6).
7. **Complete or error.** A budgeted GQL answer either completes or fails by
   name. M6 closes the remaining places that could shorten an answer and
   pins the property with a ceiling sweep over every resource.
8. **The published profile is checked, not written once.** The feature
   registry, the unsupported list and the EXPLAIN reference are each
   compared with the code by a test, so they cannot drift. Section 4.

---

## 1. Where M5-M7 start: the current code

Verified against the working tree of this change (the committed `gql-0.18`
branch plus the staged M3 work).

### 1.1 What exists and is reused

| Piece | Where | Used by |
| --- | --- | --- |
| The host interface: `GqlHost::{eval, test, open_seed, out_of_range}`, `EvalCx`, `ExprId`, `SeedId`, `Truth` | `core/engine/src/query/gql/host.rs` | every M5/M6 operator; M6 extends what `open_seed` prepares, not its signature |
| The plan vocabulary: `OpSpec` (`Unit`, `Seed`, `Expand`, `Filter`, `Let`, `Project`, `Aggregate`, `Distinct`, `Sort`, `Page`, `Unnest`, `PathSearch`, `Reach`, `OptionalApply`, `Argument`), `SeedSource::{Key, Index, Bound, Scan}`, `SortKey`, `CountExpr` | `core/engine/src/query/gql/plan.rs` | M5 adds three variants; M6 adds fields to `SeedSource::Index` and `OpSpec::Sort` |
| Operator construction and the inner-side rule: `build_in(spec, params, argument)` refuses blocking operators inside an `OptionalApply`; `ExecCx::argument` carries the row to the leaf | `core/engine/src/query/gql/ops/mod.rs` | M5 generalises the rule per apply kind |
| `OptionalApply`, `Argument` | `core/engine/src/query/gql/ops/optional.rs` | template for `ExistsApply` and `CallApply` |
| Top-k sort under a `Page` | `core/engine/src/query/gql/ops/sort.rs` | M6 adds the early stop |
| Budgets: `GqlBudget`, `GqlMeter`, `GqlWork::add_page`; memory caps at `RUN_BYTES` | `core/engine/src/query/gql/budget.rs` | no new resource is needed in M5-M7 (sections 2.8, 3.9) |
| Charged element reads: `ElementReader::{node_property, node_property_names, edge_property, edge_property_names, node_label, edge_label}`; a vector property reads as `BindingValue::Vector`, a geometry as `BindingValue::Geo` | `core/engine/src/query/gql/reader.rs` | M6 adds text match and text score per node |
| The engine query path: `Database::prepare_query`, `QueryRequest`, `QueryFilter::{Scalar, Point, Geometry, Text, Ids, Any, All, Not}`, `QueryOrder::{ExactVector, ApproximateVector, Bm25, Distance, Score, Driver}`, `ScoreExpr`, `QueryRow { id, order, projected }`, `OrderValue` | `core/engine/src/query/mod.rs`, `core/engine/src/query/plan.rs` | M6 seeds and per-node text reads |
| Pure spatial functions: `dwithin_m`, `distance_m`, `within`, `contains`, `covers`, `intersects` | `core/engine/src/index/spatial/geometry.rs`, re-exported as `sekejap_core::spatial_geometry` | M6 per-row spatial predicates |
| Parser, binder, planner, evaluator: `Parser::gql_graph_table`, `gql_pipeline`, `gql_stage`, `gql_match`; `bind_match`; `Planner::{pipeline, statements, optional, matched, choose_seed, seed_for, index_seed}`; `Program`, `Host`, `IndexSeed` | `lang/src/gql/parse/{mod,stage,expr,pattern}.rs`, `lang/src/gql/{bind,stage,plan,eval}.rs` | every M5/M6 lang task |
| The SQL host-form sub-parsers: `Parser::tsquery`, `Parser::geo_argument`; the SQL compile of them: `compile/predicates.rs::{tsquery_slot, spatial}`, `compile/select.rs::vector_order`, `OwnedFilter`, `OwnedOrder`, `OwnedScore` | `lang/src/parser/expr.rs`, `lang/src/compile/{predicates,select,plan}.rs` | M6 reuses them so each host function has one spelling |
| Slot facts: `BindingSchema`, `SlotInfo { name, ty, provenance, nullable }`, `Provenance::{Element, Returned, Let, Unnest, Aggregate}` | `lang/src/gql/schema.rs` | M6 adds lineage |
| EXPLAIN: `GqlPlan::describe`, `render_gql` (the run half: rows, pages, every counter) | `lang/src/gql/plan/explain.rs`, `lang/src/explain.rs` | every milestone adds lines |
| Refusals inside a body: `refuse::GQL_TABLE`, `gql_refuse`, `gql_lookup`; public `sekejap_lang::gql_refusals()`; the guard tests `gql_parse.rs::{every_later_construct_is_refused_by_name_with_its_milestone, the_gql_table_is_well_formed_and_every_row_is_reached}` | `lang/src/refuse.rs`, `lang/src/lib.rs`, `lang/tests/gql_parse.rs` | M5/M6 delete rows; M7 checks the published list against the table |
| The documentation harness: `DOCS`, `build_fixture`, `build_place` | `dist/rust/tests/doc_examples.rs`, `docs/lang/EXAMPLE_FIXTURE.md` | M7 examples |

### 1.2 The refusals that name these milestones today

`lang/src/refuse.rs::GQL_TABLE`:

| Row | Tier | Milestone | Refused at |
| --- | --- | --- | --- |
| `OPTIONAL MATCH with comma patterns` | 2 | M5 | `parse/stage.rs::gql_match` |
| `EXISTS` | 2 | M5 | `parse/expr.rs::gql_primary` |
| `CALL` | 2 | M5 | by word, through `Parser::gql_listed` |
| `UNION` | 2 | M5 | by word, where `)` or `NEXT` is expected after a `RETURN` |
| `host function` | 2 | M6 | `parse/expr.rs` (`ST_*`, `TO_TSVECTOR`, `TO_TSQUERY`, `BM25`, and the `::vector`/`::geometry`/`::geography` casts in `gql_cast_type`) |
| `ALL SHORTEST`, `SIMPLE`, `selector with a path mode`, `nested quantifier`, `label conjunction`, `label negation`, `label wildcard`, `INTERSECT`, `EXCEPT` | 2 | P1 | parser |
| `PATH_SUM` ... `PATH_AVG`, `VERTEX_ID`, `EDGE_ID`, `COLUMNS` | 3 | not adopted | parser |

No row names M7: M7 publishes; it builds no construct.

### 1.3 Where the brief and the code disagree (found while reading)

Each is stated once here and resolved in the section named.

1. **Vector default on the SQL side.** The owner's decision says vector
   search in GQL is exact unless `ef_search` is set, "the same as the SQL
   side". The SQL side is exact only when the column has a READY exact index:
   with only a quantized or vamana index it answers APPROXIMATELY with
   `DEFAULT_EF = 100` (`lang/src/compile/mod.rs`,
   `lang/src/compile/select.rs::vector_order`), with a notice. GQL follows
   the owner's rule, not that default. Section 3.6, Q28.
2. **`ef_search` and the plan cache.** `SET LOCAL ef_search` is a
   thread-local read when a statement is COMPILED
   (`lang/src/compile/mod.rs::EF_SEARCH`), and `dist/rust/src/plans.rs` keys
   a cached plan by statement text and catalog generation only. A cached SQL
   plan therefore keeps the exactness it was compiled with. GQL reads the
   knob when an execution opens instead. Section 3.6, Q29.
3. **Scan admission.** Brief §4.3 says an indexed candidate set must not be
   compulsory for every legal small-graph query. The owner's Q5 answer
   refuses a label scan whose node carries a predicate no index answers. The
   code also ACCEPTS the same scan when the predicate is written as a later
   `FILTER` statement (`MATCH (n IS site) FILTER n.rating > 4`), because
   `Planner::seed_for` looks only at inline and `MATCH ... WHERE` conjuncts.
   This is read from `lang/src/gql/plan.rs`, not pinned by a test. Section
   2.6, Q26.
4. **Lineage has no slot yet.** The M1-M4 design sketched
   `Provenance::Property` "so an alias still carries index lineage (M6)". The
   code's `Provenance` has no such variant. Lineage is new work in M6-E.
5. **`ANY CHEAPEST` crosses exactly one edge step.** Brief §5 lists
   `ANY CHEAPEST` with `COST` as P0 without that limit. The code refuses a
   cheapest pattern with more than one edge step through
   `SqlError::unsupported` in `lang/src/gql/automaton.rs` ("a COST per step is
   not built"), not through a `GQL_TABLE` row. The brief's own weighted
   example uses one quantified edge, so P0 is met. M7 turns the refusal into
   a named P1 row (Q36).
6. **`search()` inside GQL.** `TEXT_FUNCTIONS` in `lang/src/gql/parse/expr.rs`
   lists `TO_TSVECTOR`, `TO_TSQUERY` and `BM25`. The typo-tolerant `search()`
   and `search_score()` are not listed, so inside a body they fail as an
   unknown function (`42883`) rather than by name. Their dictionary walk
   TRUNCATES with a notice (`QL_CONTRACT` §4.6), which conflicts with
   complete-or-error. Q30.
7. **`UNION DISTINCT`.** The brief writes `UNION DISTINCT`; this task's
   brief writes `UNION`. ISO GQL has both spellings, and `UNION` alone means
   `DISTINCT`. Both are accepted (section 2.5).

---

## 2. M5: composition completion

Brief §4.2 (`OptionalApply`, `SemiApply`/`AntiApply`, `Union`,
`SubqueryApply`), §5 (P0 rows), §7 rules 6 and 7, §9, §11 step 5.

### 2.1 Layer split

| Concern | Crate | Files (new unless marked) |
| --- | --- | --- |
| `ExistsApply` (filter and mark forms), `CallApply`, `Union`, and the inner-side rule per apply kind | core | `core/engine/src/query/gql/ops/exists.rs`, `ops/call.rs`, `ops/union.rs`; edits `ops/mod.rs` (`build_in`, dispatch table), `query/gql/plan.rs` (three `OpSpec` variants), `query/gql/mod.rs` (re-export `ExistsMode`) |
| Grammar: comma patterns after `OPTIONAL MATCH`, `EXISTS { }`, `CALL (...) { }`, `UNION [ALL \| DISTINCT]` | lang | edits `lang/src/gql/ast.rs`, `lang/src/gql/parse/{mod,stage,expr}.rs`, `lang/src/refuse.rs` (rows leave `GQL_TABLE`, new P1 rows enter) |
| Scope and planning of subquery bodies and union branches | lang | new `lang/src/gql/subquery.rs` (EXISTS and CALL bodies) and `lang/src/gql/union.rs`; one call site each in `lang/src/gql/stage.rs`; `Op` variants in `lang/src/gql/plan.rs` |
| EXPLAIN of the new operators | lang | edits `lang/src/gql/plan/explain.rs` |

Core evaluates nothing new: every predicate inside a body is still an
`ExprId` the host evaluates. The apply operators only move rows, which is
what `docs/LAYERS.md` already allows core to do.

### 2.2 `OPTIONAL MATCH` over several patterns

Today `Statement::Match { optional: true }` holds one pattern (design Q3), and
`gql_match` refuses a comma after it by name. M5-A lifts the refusal:

* `OPTIONAL MATCH p1, p2 [WHERE ...]` is optional TOGETHER: the inner side of
  ONE `OptionalApply` is everything `Planner::matched` plans for both patterns
  and the `WHERE`. When the two patterns share no variable they are a cross
  product inside the inner side, as in a plain `MATCH` (M1-M4 design §3.3).
* `introduced` is every slot allocated from the start of the statement, as
  `Planner::optional` computes it today (`schema.nullable_from(width)`); no
  change there.
* ISO GQL also has a block form, `OPTIONAL { MATCH ...; MATCH ... }`. It is
  not in the brief's P0 list and stays a syntax error pointing at the comma
  form; a new `GQL_TABLE` row `OPTIONAL block` (P1) names it (Q17).

Test pins: two optional patterns where only one matches give the `NULL` row
for BOTH (not a half-filled row); a shared variable between the two joins as
an `ExpandInto`; `COUNT` over either `NULL` side is 0.

### 2.3 `EXISTS { ... }` and `NOT EXISTS { ... }`

**Grammar** (`parse/expr.rs`, `parse/stage.rs`):

```text
exists := EXISTS '{' body '}'
body   := pattern (',' pattern)* [WHERE expr]             -- the short form
        | statement+ [RETURN item (',' item)*]             -- the full form
```

`statement` is the stage grammar's `MATCH`, `OPTIONAL MATCH`, `LET`, `FILTER`
and `FOR`. A `RETURN` inside `EXISTS` is accepted and its items are ignored,
as in Google's documented form; an aggregate, `GROUP BY`, `ORDER BY`,
`OFFSET` or `LIMIT` there is refused by name (Q18), because each changes
whether a row exists only in ways a later `FILTER` says more plainly.

**Scope.** A name in the body that is bound outside it is the OUTER variable
(implicit correlation). The body's own new variables are local: they are not
visible after the `}` and never appear in the outer row's schema. A body may
not re-bind an outer variable to a different element; naming it in a
pattern is a correlation (the pattern is seeded from it, `Seed::Bound`).

**Placement.** `EXISTS` is an expression, but it runs a subplan, so core must
run it. The planner turns each `EXISTS` into an `ExistsApply` placed at the
first operator after which every outer variable it names is bound, and:

* when it is a top-level conjunct of a `FILTER`, a `MATCH ... WHERE` or an
  inline element `WHERE` (outside any quantifier), or the `NOT` of one, it is
  the FILTER form: the apply itself keeps or drops the row;
* anywhere else (inside `OR`, `CASE`, a `LET`, a `RETURN` item), it is the
  MARK form: the apply writes `TRUE`/`FALSE` into a hidden `BOOLEAN` slot,
  and the expression reads that slot (`Ex::Slot`). This covers every
  position with one operator and no second evaluator (Q19);
* inside a quantified group or a selective pattern's inline predicate it is
  refused, naming the rule: those predicates run during the search, per
  iteration, where a subplan per state would be unbounded in a way no budget
  line explains.

**Core operator** (`ops/exists.rs`):

```rust
// core/engine/src/query/gql/plan.rs -- SKETCH
/// `EXISTS { ... }`: per input row, run `inner` from it (its leaf an
/// `Argument`) until its first row, then drop the inner tree.
ExistsApply { input: Box<OpSpec>, inner: Box<OpSpec>, mode: ExistsMode },

pub enum ExistsMode {
    /// Keep the input row when the inner side gives a row (`negated` false)
    /// or gives none (`negated` true).
    Filter { negated: bool },
    /// Keep every input row; write whether the inner side gave a row into
    /// `slot` as a `Bool`.
    Mark { slot: SlotId },
}
```

* It never multiplies the input row: at most one row out per row in (brief
  §7 rule 7).
* It stops at the inner side's FIRST row. The inner operators may still hold
  a refill or a path-search frontier at that point, so the operator REBUILDS
  the inner tree from its `OpSpec` for each input row instead of reusing it.
  `OptionalApply` does not need that, because it always drains its inner
  side. **Cost (named):** one small allocation per input row; no store read.
* The inner side holds streaming operators only (the `OptionalApply` rule).
  Nothing blocking can change whether a first row exists.

**Reuse of endpoint sets (brief §4.2).** `NOT EXISTS { MATCH (x)<-[:t]-() }`
asks whether `x` has any incoming edge of type `t`. Its inner side is one
`Expand` that stops at the first posting, so it costs one adjacency seek plus
at most one posting (`graph_edges` 1-2). The engine's endpoint sets
(`core/engine/src/index/graph/endpoints.rs`) could answer it with no posting
read, but that is a second physical form with its own proof. v1 does not
add it; EXPLAIN states the posting bound instead.

### 2.4 `CALL (imports) { ... }`

**Grammar** (`parse/stage.rs`, a new statement):

```text
statement := ... | CALL '(' [name (',' name)*] ')' '{' stage '}'
```

* The import list is REQUIRED; `()` is an uncorrelated call, re-run per input
  row. A bare `CALL { }` is refused naming the import list (Q20).
* The body is ONE stage: statements, then a `RETURN` with its own grouping,
  `ORDER BY`, `OFFSET` and `LIMIT`. `NEXT` inside a `CALL` body is refused by
  a new P1 row (`NEXT inside CALL`), and `OPTIONAL CALL` by another (P1, brief
  §5) (Q22).

**Scope** (brief §7 rule 7).

* Inside the body only the imported variables are visible. Naming any other
  outer variable is a bind error that names the import list.
* The body's `RETURN` columns are added to the outer row under their names.
  A name that collides with an outer variable in scope is refused at bind
  (`42712`, PostgreSQL's duplicate alias, the code `bind.rs` already uses for
  a duplicate variable).
* The result is a LATERAL INNER join: each row the body returns extends the
  input row once, and an input row whose body returns no row is DROPPED. A
  body whose `RETURN` aggregates with no grouping key returns exactly one row
  over empty input (`COUNT` 0), so the collaborator-overlap query keeps every
  outer row (brief §9.3). The left-outer form is `OPTIONAL CALL` (P1) (Q21).

**Core operator** (`ops/call.rs`):

```rust
/// `CALL (...) { ... }`: per input row, a fresh instance of `inner` runs
/// from that row (its leaf an `Argument` of the outer width); each row it
/// returns, `outputs.len()` slots wide, is written into `outputs` of a copy
/// of the input row.
CallApply { input: Box<OpSpec>, inner: Box<OpSpec>, outputs: Box<[SlotId]> },
```

* The inner side MAY hold `Aggregate`, `Distinct`, `Sort`, `Page` and
  `Project`: per-input grouping and top-k is the point of `CALL` (brief §5).
  `build_in` therefore takes an inner KIND (`Optional`, `Exists`, `Call`)
  instead of today's `argument: Option<u16>`, and refuses blocking operators
  only for the first two.
* The inner tree is rebuilt per input row, as for `ExistsApply`: a blocking
  operator's `Held` state cannot restart in place.
* The inner side's own slots live in the OUTER stage's schema (as
  `OPTIONAL MATCH` slots do today), so the `Argument` width equals the outer
  width. **Cost (named):** 16 bytes per inner slot in every outer row of the
  stage (M1-M4 design §2.2 `held_bytes`).
* Its memory is the inner operators' own `Held` charges, released when the
  inner tree is dropped at the end of each input row.

### 2.5 `UNION` and `UNION ALL`

**Grammar** (`parse/mod.rs::gql_pipeline`):

```text
body := part (NEXT part)*
part := stage ((UNION [ALL | DISTINCT]) stage)*
```

* `UNION` and `UNION DISTINCT` remove duplicate rows; `UNION ALL` keeps them
  (ISO GQL; brief §11 matrix "Union branches").
* A branch is one stage (a linear query). `A UNION B NEXT C` is
  `(A UNION B) NEXT C`.
* One chain uses ONE conjunction: mixing `UNION ALL` and `UNION` without
  parentheses is refused by a new P1 row (`mixed UNION`) (Q24). ISO's
  parenthesised composite form is not in P0.
* `INTERSECT` and `EXCEPT` stay P1 (`GQL_TABLE`).

**Column rules** (PostgreSQL where not graph-specific, Q23). Every branch
returns the same number of columns, with the same NAMES in the same ORDER
(this satisfies both GQL's by-name rule and SQL's by-position rule, so no
statement means different things under the two). Types unify per column with
`types::unify` (`Int` with `Float` gives `Float`); anything else is refused
at bind (`42804`), naming the column and both branches. A node, edge or path
column may cross a union into a later stage; DISTINCT then compares it by
identity, as `OpSpec::Distinct` does today.

**Core operator** (`ops/union.rs`):

```rust
/// The rows of each branch in turn, branch order then row order. Each
/// branch is a plan of its own whose leaf is `Unit` (a first part) or
/// `Replay` (a part after NEXT). DISTINCT is the existing `Distinct` over
/// this operator, not a second implementation.
Union { branches: Box<[OpSpec]>, width: u16 },
/// After NEXT: the incoming table, read ONCE and held, then handed to each
/// branch's `Replay` leaf in turn.
Buffered { input: Box<OpSpec>, union: Box<OpSpec> },
Replay { width: u16 },
```

After `NEXT`, every branch must see the WHOLE incoming table (a branch may
aggregate it). Running the branches per input row would change their
aggregates, so the incoming table is materialised once and charged to
`sort_bytes` (Q25). **Cost (named):** one buffered copy of the incoming table,
refused past the `sort_bytes` cap like any blocking operator. A union in the
FIRST part needs no buffer: each branch starts from its own `Unit`.

### 2.6 Seeded row predicates

Brief §11 step 5 asks for "arbitrary seeded row predicates". After M2-M4, a
row predicate is already accepted on every element reached from a seed and
charged (`far_filter`, `NodeTest::filter`, `FilterAt::AfterPattern`). What M5
settles is the admission rule for the new bodies and the one inconsistency
found in section 1.3 item 3:

1. **Inside `EXISTS`, `CALL` and `OPTIONAL MATCH`** a pattern is seeded, in the
   order `Planner::choose_seed` already uses, by a key, an index, a
   correlated (imported or outer) variable as `Seed::Bound`, or a scan. Every
   row predicate after that seed is evaluated where its variables are bound
   and charged `primary_reads`. An UNCORRELATED scan inside a body is run
   once per outer row; EXPLAIN prints "re-evaluated per input row" (M1-M4
   design §3.3) and the budget bounds it.
2. **The Q5 refusal applies inside bodies unchanged**: a label scan whose
   node carries a predicate no index answers is refused, naming the index.
3. **The `FILTER` form of a scan** (`MATCH (n IS site) FILTER n.rating > 4`) is
   KEPT as the explicit way to ask for a scan with a row-bound test, and the
   Q5 refusal message gains one clause that names it. This reconciles brief
   §4.3 (a small graph must not need an index) with the owner's Q5 (a
   predicate written ON the node is a seed request, and a seed request no
   index answers is refused). EXPLAIN already labels the seed `SCAN of <label>`
   and the filter `as written, after the pattern`. Lineage (M6-E) must never
   turn this accepted form into a refusal: it moves a conjunct into the seed
   only when an index answers it (Q26).
4. **Kept refusals, now listed as P1 rows or named errors, not silent
   limits**: an inline predicate inside a quantifier or a selective pattern
   that names a variable bound after it (`automaton.rs`, "write the condition
   in the MATCH's WHERE"). These are correct restrictions (the predicate runs
   during the search), and M5-H only pins them.

### 2.7 The workload fixtures, end to end

Brief §9 gives six acceptance queries in an application schema. They are
re-expressed here with invented data in the tourism world, keeping the SHAPE
each one tests. The fixture lives in `lang/tests/gql_workloads.rs` (new), and
each query also has an exhaustive oracle: the fixture is small enough (at most
60 nodes) that a direct Rust enumeration over the inserted edges computes the
expected rows.

| Brief | Shape tested | Re-expressed as | Milestone that makes it run |
| --- | --- | --- | --- |
| §9.1 | two relationship routes combined, duplicates removed | dances performed by troupe `troupe_a` directly, `UNION` dances performed by its members (`dancer -[:member_of]-> troupe`, `dancer -[:performs]-> dance`), ordered by title | M5 (`UNION`) |
| §9.2 | rank, then explore through `NEXT` and `OPTIONAL MATCH` | rank dancers by direct collaborations with `troupe_a`, keep ten, then count each one's wider network | M3 (already pinned in `gql_optional.rs`); re-run here on the shared fixture |
| §9.3 | per-input local aggregation, either-direction edges counted once | for each (troupe, dancer) pair, `CALL (troupe, dancer) { ... COUNT(DISTINCT other) }` shared collaborators; one stored edge per collaboration event | M5 (`CALL`) |
| §9.4 | batched seeds and bounded quantifier coverage | `FOR k IN $1` skill keys (`snorkelling`, `free_diving`, `boat_handling`), `-[:enables]->{0,6}` skills, `-[:useful_for]->` jobs in `$2` (`dive_guide`, `reef_monitor`) | M4 (expected to run today, not yet pinned); pinned here |
| §9.5 | hybrid top-k, then deep backward cause chains, anti-join, two aggregations | incidents (`reef damage near Uluwatu`, `beach erosion at Seminyak`) with `body`, `loc`, `embedding`, `realm` in `('sea', 'land')`; `cause -[:causes {confidence}]-> effect` chains up to 8 hops; `FILTER NOT EXISTS { MATCH (cause)<-[:causes]-() }` | M5 runs stages 2-4 from a list seed (`FOR k IN $1 MATCH (incident IS incident WHERE incident._key = k)`) with a stored relevance property; M6-J runs it as written, with the text, spatial and vector first stage |
| §9.6 | optional evidence keeps the node | a site with and without qualifying `review` evidence | M3 (pinned); re-run here |

The fixture rules the brief states are kept: collaboration is stored as one
edge per event with its own identity (parallel edges), and an either-direction
match must not double-count one stored edge. The ranking formula of §9.5 is
an application rule; the test states that it ranks evidence and proves no
causation.

### 2.8 Budgets and work counters

No new `WorkResource` is needed. Each new operator charges through resources
that already exist:

| Operator | Work | Memory |
| --- | --- | --- |
| `ExistsApply` | what its inner side charges, stopping at the first inner row; nothing of its own (like `Filter`, it passes input rows on) | the inner side's refill buffers, dropped per input row |
| `CallApply` | the inner side's charges; `binding_rows` 1 per row out | the inner blocking operators' `Held` (`sort_bytes`, `groups`, `list_bytes`), released when each input row's inner tree is dropped |
| `Union` | the branches' charges | none of its own; `UNION` (distinct) adds the existing `Distinct`'s `sort_bytes` |
| `Buffered` | none beyond its input | `sort_bytes` for the whole incoming table, held until the last branch has replayed it |

`GqlWork` is unchanged, so `render_gql`'s counter line is unchanged.

### 2.9 EXPLAIN

The existing numbering (`N.` for a stage operator, `N.k` for an inner step)
extends to the new operators. Target lines, to be pinned by
`lang/tests/gql_explain.rs`:

```text
3. ExistsApply NOT EXISTS: per input row, stops at the first row steps 3.1-3.2 give; keeps the row when they give none
  3.1 Seed cause: bound, correlated with the outer row -- charges binding_rows
  3.2 Expand cause <-[:causes]- (anonymous): incoming edges, stops at the first -- charges graph_edges, binding_rows
4. CallApply (troupe, dancer) -> shared: per input row, steps 4.1-4.5 run afresh from that row; each row they return extends it; a row they return nothing for is dropped -- charges binding_rows, and what steps 4.1-4.5 hold is released per row
1. Union DISTINCT of 2 branches, then Distinct -- branch 1 is steps 1.1-1.3, branch 2 is steps 2.1-2.5
2. Buffered: the rows stage 1 returned, held once for 2 branches -- charges sort_bytes
```

A mark-form `EXISTS` prints `ExistsApply EXISTS into hidden slot <n>`, and the
slot schema lists that slot as `BOOLEAN, never null, bound at stage <k>`.

### 2.10 Tests first

Each row is written first and seen failing (compile failure counts).

| Rule | Test |
| --- | --- |
| Optional comma patterns are optional together; both introduced sides are `NULL` together | `lang/tests/gql_optional.rs::two_optional_patterns_are_null_together` |
| `EXISTS` never multiplies a row; `NOT EXISTS` keeps rows with no match; correlation seeds from the outer variable | `lang/tests/gql_exists.rs` (new) |
| `EXISTS` in `OR`, `CASE`, `LET`, `RETURN` (mark form) equals the filter form where both apply | `gql_exists.rs::mark_and_filter_forms_agree` |
| `EXISTS` inside a quantifier or a selective pattern is refused by name | `gql_exists.rs` |
| `ExistsApply` stops at the first inner row: `graph_edges` for an existence test over a node with 1,000 incoming edges is at most 2 | `core/engine/tests/gql_exists.rs` (new) |
| `CALL` sees only its imports; an output alias collision is `42712`; an empty body result drops the row; an aggregate body keeps it with `COUNT` 0 | `lang/tests/gql_call.rs` (new) |
| `CALL` per-input top-k: `ORDER BY ... LIMIT 2` inside the body gives at most 2 rows per input row | `gql_call.rs` |
| The inner tree of `CallApply` releases its `sort_bytes` per input row: a body whose sort holds 60% of the cap runs over many input rows without a refusal | `core/engine/tests/gql_call.rs` (new) |
| `UNION` removes duplicate whole rows, `UNION ALL` keeps them; column names and order must match; `Int`/`Float` unify | `lang/tests/gql_union.rs` (new) |
| `A UNION B NEXT C`: `C` aggregates the union once; a union after `NEXT` sees the whole incoming table | `gql_union.rs` |
| Mixed `UNION`/`UNION ALL`, `OPTIONAL { }`, `NEXT` inside `CALL`, `OPTIONAL CALL`, bare `CALL { }` are refused by name | `lang/tests/gql_parse.rs` (the table-reach test gains the rows) |
| Brief §11 matrix "Scoped CALL and EXISTS", "Union branches" | `lang/tests/gql_workloads.rs` |
| The six workload queries equal their oracles, as bags | `gql_workloads.rs` |
| Pages of each workload query equal its one-shot answer | `gql_workloads.rs`, reusing `gql_paging.rs`'s page sizes 1, 2, 3, 7 |

### 2.11 Tasks

| Id | Task | Files | Needs | Tests |
| --- | --- | --- | --- | --- |
| M5-A | `OPTIONAL MATCH` with comma patterns; the `OPTIONAL block` P1 row | `lang/src/gql/parse/stage.rs`, `lang/src/refuse.rs`, `lang/src/gql/ast.rs` (doc comment) | M3-F | `gql_optional.rs` (extended), `gql_parse.rs` |
| M5-B | Core apply family: `ExistsApply` (filter, mark), `CallApply`, `Union`, `Buffered`, `Replay`; `build_in` inner kinds; per-row rebuild of the inner tree | new `core/engine/src/query/gql/ops/{exists,call,union}.rs`; edits `ops/mod.rs`, `query/gql/plan.rs`, `query/gql/mod.rs` | M3-F | new `core/engine/tests/{gql_exists,gql_call,gql_union}.rs` with the test host |
| M5-C | `EXISTS`: grammar, scope (implicit correlation), placement (filter vs mark), planning of the body as an inner side, EXPLAIN | new `lang/src/gql/subquery.rs`; edits `parse/expr.rs`, `ast.rs` (`Expr::Exists`), `stage.rs` (one call), `plan.rs` (`Op::Exists`), `plan/explain.rs`, `refuse.rs` (row out) | M5-B | new `lang/tests/gql_exists.rs`; `gql_explain.rs` |
| M5-D | `CALL`: grammar, import scope, output columns, alias collision, EXPLAIN; the `NEXT inside CALL`, `OPTIONAL CALL` and bare `CALL { }` rows | `lang/src/gql/subquery.rs`; `parse/stage.rs`, `ast.rs` (`Statement::Call`), `stage.rs`, `plan.rs` (`Op::Call`), `plan/explain.rs`, `refuse.rs` | M5-B, M5-C (shares `subquery.rs`) | new `lang/tests/gql_call.rs`; `gql_explain.rs` |
| M5-E | `UNION [ALL \| DISTINCT]`: grammar and precedence, column rules, buffering after `NEXT`, EXPLAIN; the `mixed UNION` row | new `lang/src/gql/union.rs`; `parse/mod.rs`, `ast.rs` (`Pipeline` parts), `stage.rs`, `plan.rs` (`Op::Union`), `plan/explain.rs`, `refuse.rs` | M5-B | new `lang/tests/gql_union.rs`; `gql_explain.rs` |
| M5-F | Seeded row predicates: admission inside bodies, the `FILTER`-scan clause in the Q5 message, pins for the kept refusals | `lang/src/gql/plan.rs` (`Planner::unindexed` message), `lang/src/gql/subquery.rs` (seed choice inside bodies uses `choose_seed`) | M5-C, M5-D | new `lang/tests/gql_predicates.rs` |
| M5-G | Workload fixtures and oracles (section 2.7); `QL_CONTRACT` §1 profile paragraph and §4.3 rows for `EXISTS`, `CALL`, `UNION`, comma `OPTIONAL MATCH`; `CONTRACT_TEST_MAP.md` rows | new `lang/tests/gql_workloads.rs`; `docs/lang/QL_CONTRACT.md`, `docs/lang/CONTRACT_TEST_MAP.md` | M5-A..F | the six queries, as bags; pages equal one-shot |

Seven tasks.

### 2.12 Waves

| Wave | Parallel worktrees | Shared-file edits |
| --- | --- | --- |
| 1 | M5-A; M5-B | M5-A: `parse/stage.rs`, `refuse.rs`. M5-B: core only. No overlap. |
| 2 | M5-C; M5-E | Both edit `ast.rs`, `stage.rs`, `plan.rs`, `plan/explain.rs` and `refuse.rs`, each by ONE enum variant, one call site and one table row. Merge order fixed in advance: M5-C first, M5-E rebases. Their new logic lives in separate new files (`subquery.rs`, `union.rs`). |
| 3 | M5-D | After M5-C: same new file `subquery.rs`. |
| 4 | M5-F | After M5-C and M5-D: it changes seed choice inside their bodies. |
| 5 | M5-G | Last: it runs everything. |

---

## 3. M6: hybrid execution and PostgreSQL integration

Brief §4.2 (`Seed` from text, spatial, vector candidates), §6 (the host
function rows), §7 (one parameter namespace), §9.5, §11 step 6 and the
matrix rows "Hybrid filter/rank", "Budget exhaustion / cancel", "Multiple
pages", "Prepared rebinding".

### 3.1 Layer split

| Concern | Crate | Where |
| --- | --- | --- |
| Parsing host forms inside a GQL body: `to_tsvector(...) @@ to_tsquery(...)`, `bm25(col, q)`, `ST_DWithin`, `ST_Within`, `ST_Intersects`, `ST_Contains`, `ST_Covers`, `ST_Distance`, the geometry constructors and literals, `<->`, `<=>`, `<#>` over vectors, `'...'::vector`, `$n::vector`, `::geography`, `::geometry` | lang | `lang/src/gql/parse/expr.rs` calls the SQL sub-parsers `Parser::tsquery` and `Parser::geo_argument` for the fixed argument shapes, so each host form keeps one spelling (M1-M4 design §5.2) |
| Expression IR and types for them | lang | new `Ex` variants in `lang/src/gql/expr.rs` (`Ex::TextMatch`, `Ex::TextScore`, `Ex::Spatial`, `Ex::VectorDistance`); `ValueType::Vector(Option<u32>)` and `ValueType::Geo` already exist in core `value.rs`; typing in `lang/src/gql/types.rs` |
| PURE per-row evaluation: spatial predicates and distances over two geometries, vector distances over two vectors | lang | `lang/src/gql/eval.rs` over `BindingValue::Geo` and `BindingValue::Vector` the reader produced, calling `sekejap_core::spatial_geometry::{dwithin_m, distance_m, within, contains, covers, intersects}`. This is the `docs/LAYERS.md` rule: pure expressions over values core handed over |
| KEYSPACE per-row evaluation: does this node's text field match a tsquery; what is its BM25 score | core | new `ElementReader::text_matches(node, index, query, matching, meter)` and `ElementReader::text_score(node, index, query, matching, meter)` in `core/engine/src/query/gql/reader.rs`. Each prepares a one-id query (`QueryFilter::Ids(&[id])` plus `QueryFilter::Text` or `QueryOrder::Bm25`) through `Database::prepare_query` and charges what it walks. The host calls them through `EvalCx::reader` |
| Index seeds with several filters and an order | lang builds, core pages | `lang/src/gql/eval.rs::IndexSeed` generalised; `Program::open_seed` builds the `QueryRequest`; `SeedSource::Index` gains `rank: Option<SlotId>`; `core/engine/src/query/gql/ops/seed.rs` writes `QueryRow::order` into it |
| Lineage | lang | new field `SlotInfo::lineage` in `lang/src/gql/schema.rs`; the rules in new `lang/src/gql/lineage.rs`; uses in `lang/src/gql/plan.rs` (seed choice) and `lang/src/gql/stage.rs` (sort) |
| Early-stopping top-k over an index-ordered seed | core | `OpSpec::Sort` gains `monotone_first: bool`; `core/engine/src/query/gql/ops/sort.rs` |
| Wire: vector and geometry parameters for GQL, parameter OIDs | dist | `dist/src/pg/connection.rs`, `dist/src/pg/types.rs` (only where a gap shows) |

### 3.2 Host functions inside a GQL body

* `Func` stays the scalar pack. The host forms are separate `Ex` variants,
  because each has a fixed argument shape and a fixed index family, and the
  planner has to recognise them to seed through an index.
* The `host function` row leaves `GQL_TABLE` when M6-A lands. `search()` and
  `search_score()` get a row of their own (P1, Q30).
* Typing: `@@` is `BOOLEAN`; `bm25` is `DOUBLE PRECISION` and needs a READY
  text index on the column of every label collection of the node, or it is
  refused naming the index to create (as the SQL side does); `ST_*`
  predicates are `BOOLEAN`, `ST_Distance` is `DOUBLE PRECISION` in metres
  under the geography contract (`docs/core/SPATIAL_FUNCTIONS.md`); `<=>`,
  `<->`, `<#>` are `DOUBLE PRECISION` with pgvector's direction (a distance,
  or the negative inner product), never flipped into a similarity.
* The unit and CRS rules of `QL_CONTRACT` §4.4 carry over unchanged: a
  distance not marked `::geography` is refused with the spelling to use,
  exactly as `lang/tests/sql_spatial_units.rs` pins for SQL.
* A vector property compared with a vector of a different dimension is a
  type error at bind when both dimensions are known, and a run-time
  data error (`22000` class) otherwise, since the binder cannot know an
  undeclared field's width.

### 3.3 Seeds through spatial, text and vector indexes

`GqlHost::open_seed` already returns a core `PreparedQuery<'db>`, and
`Program::open_seed` builds a `QueryRequest` over ONE scalar filter. M6-D
generalises the seed without changing the trait:

```rust
// lang/src/gql/eval.rs -- SKETCH
pub(crate) struct IndexSeed {
    pub(crate) collection: CollectionId,
    /// Every conjunct of the node an index answers, as engine filters built
    /// per execution (their values are expressions over bound slots).
    pub(crate) filters: Vec<SeedFilter>,     // Scalar | Point | Geometry | Text
    /// The engine order when the planner seeds in index order (3.5):
    /// ExactVector | ApproximateVector (only with ef_search) | Bm25 |
    /// Distance | Score | Scalar. `QueryOrder::Driver` otherwise.
    pub(crate) order: Option<SeedOrder>,
}
```

* **Which conjuncts.** Every conjunct of the seed node that a READY index of
  its one label collection answers exactly, from its inline `WHERE`, the
  `MATCH ... WHERE`, or (through lineage, 3.5) a later conjunct. The engine
  intersects them (`QueryFilter` list) with its own driver choice
  (`CandidateDriver::Auto`), so one seed may be text AND point radius AND a
  scalar range. A conjunct the index answers only as a candidate test
  (`QueryFilter::Geometry`, whose box is refined against the row) is still a
  seed filter: the engine refines it.
* **What is consumed.** A conjunct the prepared query answers exactly is not
  evaluated again (as for scalar seeds today). Anything else stays a
  `Filter` right after the seed.
* **Seed ranking.** `rank: Option<SlotId>` on `SeedSource::Index` asks the
  engine to write each row's `QueryRow::order` (`OrderValue::Distance`,
  `Bm25` or `Score`) into that hidden slot as `Float`. The planner uses it
  when a `LET` or `RETURN` repeats the seed's ranking expression exactly, so
  `LET lexical = bm25(incident.body, $1)` over a text-seeded `incident` reads
  the slot instead of preparing a one-id query per row.
* **Label alternatives.** An index belongs to one collection. A node with
  several labels is seeded from an index only when every label collection
  has a matching READY index; the seed then prepares one query per
  collection, in label order, as `SeedSource::Key` does. Otherwise the node is
  not index-seedable (scan rules apply).
* **Rebinding.** Every value in a seed filter or order is an expression
  evaluated when the execution opens the seed, so a GQL plan stays always
  rebindable (M1-M4 design §7). The tsquery text, the point, the radius and
  the query vector may all be `$n`.

### 3.4 Predicates and ordering inside patterns and stages

Where a host expression stands decides how it is evaluated. There is no
position where it is silently dropped or approximated.

| Position | Evaluation | Charged |
| --- | --- | --- |
| Seed node, answered by an index | inside the seed's prepared query | the index walk (`text_postings`, `spatial_postings`, `vector_sidecars`, ...) |
| Seed node, not answered by an index | `Filter` after the seed (pure: lang; text: core reader) | `primary_reads` for the row; text reads as below |
| A far node or an edge in a hop, or a node test in a path search | per hop, as today's `far_filter` / `edge_filter` / `NodeTest::filter` | `primary_reads`; a text test adds the one-id query's `text_postings` and `text_tokens` |
| `LET`, `FILTER`, `RETURN`, outer `SELECT` | per row, by the host | as above; a vector distance charges `vector_lanes` for the lanes it compares, like the engine's vector scan |
| `ORDER BY` of a stage or of the outer `SELECT` | the existing `Sort` over the evaluated key, or an index-ordered seed plus an early-stopping `Sort` when lineage allows (3.5) | `sort_bytes` held |

A text predicate over a node whose label has no text index on that field is
refused at bind naming the index (Q27), as `bm25` is: there is no analyzer
run outside an index in this engine, and emulating one per row would be a
second tokenizer whose answers could drift from the index's.

### 3.5 Index lineage

**The fact.** Each slot of a stage schema carries an optional lineage:

```rust
// lang/src/gql/schema.rs -- SKETCH
pub(crate) enum Lineage {
    /// The value IS property `field` of the node in `node` (a slot of the
    /// same stage schema).
    Property { node: SlotId, field: Box<str> },
    /// The value IS ranking expression `rank` (a vector distance to a
    /// constant or $n, a bm25 against a constant or $n, an ST_Distance to a
    /// constant point, or an arithmetic combination the engine's ScoreExpr
    /// accepts) of the node in `node`.
    Rank { node: SlotId, rank: RankExpr },
}
```

**Propagation** (`lang/src/gql/lineage.rs`):

* `n.f` and a rankable expression over `n` START a lineage.
* An alias (`RETURN x AS y`, `SELECT g.y`), `LET y = x`, a grouping key that
  is exactly `x`, and a `NEXT` column that is exactly `x` COPY it, remapping
  `node` to the slot that carries the node in the new schema.
* Anything else ENDS it: arithmetic on a property, an aggregate, a `CASE`, a
  column whose node was not carried across `NEXT` (the node then has no slot
  in the new schema, so no operator could use the index there anyway).

**Use 1: moving a later conjunct into the seed.** A conjunct `c` of a later
`FILTER`, or of the outer `SELECT`'s `WHERE`, whose every column has lineage
to the SAME seed node `n`, is treated as a `MATCH ... WHERE` conjunct of `n`
for seed choice when:

1. an index answers `c` exactly (otherwise nothing moves: this is what keeps
   the accepted `FILTER`-scan form of section 2.6 accepted);
2. every operator between the seed and `c` is a per-row streaming operator
   that neither chooses among rows nor counts them: `Expand`, `Filter`,
   `Let`, `Project`, `Unnest`, `OptionalApply` over slots `c` does not read,
   and `PathSearch::Enumerate`. A selector (`Any`, `Shortest`, `Cheapest`),
   `Reach`, `Aggregate`, `Distinct`, `Sort`, `Page`, `Union`, `ExistsApply`
   in mark form over a slot `c` reads, and `CallApply` stop it (Q32).

Under those two conditions the moved conjunct removes exactly the rows it
would have removed later, and nothing between depended on them. Brief §8.3
("an outer SQL filter ... is not automatically a constraint on the search")
is kept: a selector stops the move. EXPLAIN prints the conjunct at the seed
with "moved from the outer WHERE by lineage", so the plan shows it.

**Use 2: seeding in index order.** A `Sort` whose FIRST key has `Rank` or
`Property` lineage to seed node `n` with an index that orders it, and which
has a `LIMIT`, is fed by a seed prepared in that index order. Every operator
between must preserve the order of its input across rows: all the streaming
operators of use 1 do, because each emits the rows of one input row before
pulling the next. The `Sort` stays (it breaks ties with the other keys and
keeps the answer exact) and gains `monotone_first: true`: once it holds
`offset + limit` rows and an incoming row's first key is strictly worse than
the worst held row's first key, it stops pulling its input. The answer is
identical to the full sort for any tie-break keys, because every row not yet
pulled is at least as bad on the first key as the one that stopped it.

**Why lineage stops at `NEXT` without the node.** The index belongs to the
node's collection; an operator can seed or order through it only where that
node is being seeded. A value whose node was not carried across `NEXT` has no
seed to attach to, and saying otherwise would imply a re-scan the profile
does not do (brief §7 rule 4).

### 3.6 Exact versus approximate scoring

Owner decision: vector search inside GQL is EXACT unless the query sets
`ef_search`.

| Case | GQL behaviour |
| --- | --- |
| A vector distance in `LET`, `FILTER`, `RETURN`, a pattern predicate, or a `Sort` key | computed per row from the stored f32 vector: EXACT, always |
| A vector-ordered seed (use 2 of lineage), the column has a READY exact index, no `ef_search` | `QueryOrder::ExactVector`: exact |
| Same, the column has only quantized or vamana indexes, no `ef_search` | NOT approximate. The seed is a label SCAN (under the scan rules) and the stage's `Sort` computes exact distances, with a notice that names `CREATE INDEX ... USING exact` (Q28) |
| `ef_search` set by `SET LOCAL ef_search = n` (or its two aliases) in the same transaction | `QueryOrder::ApproximateVector { ef: n }` from the vamana index if READY, else the quantized one (the same preference `vector_order` uses); EXPLAIN and a notice say `APPROXIMATE (ef=n)` and that the shortlist bounds the whole result |
| `ef_search` set, the column has only an exact index | exact; the notice says the knob is unused (the SQL side's wording) |

* **When the knob is read.** When the execution OPENS (as `now()` is), not at
  compile, so a cached GQL plan follows the knob of the transaction it runs
  in (Q29). EXPLAIN reads it when EXPLAIN runs.
* **Where the SQL side differs** (section 1.3 item 1): a SQL `ORDER BY emb
  <=> $1` over a column with only an approximate index is approximate by
  default. This design does not change the SQL side; the difference is
  written into the published dialect notes (section 4.3) and flagged to the
  owner (Q28).
* Brief §9.5 asks for exact scoring over the admitted candidates by default;
  that is the first row of the table.

### 3.7 Complete-or-error work accounting

The rule: a budgeted GQL answer either completes, or its stream ends in a
NAMED error with the counters, and no page after a refusal reports `done`
(`GqlPage::done`, M1-M4 design §3.5). M3-E pinned it for `graph_edges` over
seven statements (`lang/tests/gql_paging.rs`). M6 closes what is left:

| Place | Risk | Rule |
| --- | --- | --- |
| Every `WorkResource` a GQL plan can charge | a resource whose refusal is swallowed or mapped to an empty page | a ceiling sweep per resource, per operator family (below) |
| Hybrid seeds | a prepared query's own `total_limit` or page cap passed off as the end of the seed stream | `Seed` pages with `total_limit: None` and treats only the engine's completed page as the end, as today; pinned for text, point, geometry and vector seeds |
| Text `search()` | its dictionary walk truncates with a notice | not admitted in GQL in P0 (Q30) |
| Approximate vector order | the `ef` shortlist bounds the result | admitted only with `ef_search`, labelled APPROXIMATE in EXPLAIN and a notice: approximate by the user's request, not a short exact answer |
| One-id text reads in `ElementReader` | a refusal inside the helper query turned into "no match" | the helper returns the engine error unchanged |
| The wire | a refused portal re-run on the next `Execute` | fixed in M3-E; M6 re-pins it for hybrid statements without touching the parked `Sync` lifetime decision |

**The sweep** (`lang/tests/gql_complete_or_error.rs`, new): for each statement
of a fixed set (every operator kind, every seed kind, every path search, the
three M5 operators) and for each resource the statement charges in an
unlimited run (read from `GqlWork`), run again with that resource's ceiling
at every value from 1 to the unlimited run's count (sampled geometrically
above 64). Every run must either return the whole answer or end in
`BudgetExceeded { resource, limit, attempted > limit }` after a prefix of the
answer. Memory resources are swept by lowering their ceiling below the cap
(a caller may ask for less, never more).

### 3.8 Parameters, portals and arrays

* **One namespace, typed once.** `types::typed_param` and
  `Planner::param_types` learn `ValueType::Vector(dim)` and `ValueType::Geo`:
  `$5::vector` and a `$n` compared with a vector property type as a vector,
  and `ST_MakePoint($2, $3)` types both as `DOUBLE PRECISION`. A conflict
  between two uses of one `$n` is `42P08` at prepare, as today.
* **Wire.** `PreparedSql::param_types` already feeds `ParameterDescription`;
  the new spellings reach the existing `oid_for_declared` (`VECTOR`,
  `GEOMETRY`). A parameter typed `VECTOR` and sent as text `'[0.1, ...]'`
  already decodes to `Param::Vector` (`dist/src/pg/types.rs`), as on the SQL
  side. A list of vectors is refused at bind in
  v1: PostgreSQL arrays of the pgvector type are not in the wire surface.
* **Lists.** `FOR x IN $1` and `x IN $2` keep their M3 meaning; a
  `Param::Vector` in list position stays a type error (brief §7, M1-M4 design
  §7).
* **Portals and cursors.** Hybrid statements get the same pinning M3-E gave
  path statements: `Execute(max_rows)` slices, `DECLARE`/`FETCH`,
  `CancelRequest`, `statement_timeout`, and a refused portal that stays
  refused. The portal lifetime across `Sync` is the owner's parked decision:
  no test asserts either answer to it, and no task changes
  `dist/src/pg/connection.rs` there.

### 3.9 Budgets, EXPLAIN, tests and tasks

**Budgets.** No new resource. The hybrid seed charges what `prepare_query`
charges (absorbed into `GqlMeter` through the existing seed path); the one-id
text reads charge `text_postings`, `text_tokens` and `candidates`; a
per-row vector distance charges `vector_lanes`; a spatial predicate charges
the row read only.

**EXPLAIN target lines** (`lang/tests/gql_explain.rs`, `lang/tests/gql_hybrid.rs`):

```text
1. Seed incident: index `incident_body` on incident (text, all of $1) and index `incident_loc` (radius $4 m around ($2, $3)) -- ordered by score (bm25 of body, cosine distance of embedding to $5) EXACT, ranking into hidden slot 7 -- charges text_postings, spatial_postings, vector_sidecars, candidates, binding_rows
4. Sort by relevance DESC, incident._key: first key follows the seed's order (lineage: relevance <- score of incident), stops pulling once 10 rows are held and the next row is worse
5. Filter as the outer WHERE: moved to the seed by lineage (incident.realm = 'sea')
notice: vector order on `embedding` is APPROXIMATE (ef=64, SET LOCAL ef_search): the shortlist bounds the whole result of this execution
```

**Tests first.**

| Rule | Test |
| --- | --- |
| Each host form parses in a body with the SQL spelling, and the units rules refuse the same statements SQL refuses | `lang/tests/gql_hybrid.rs` (new) |
| Text, point, geometry and vector seeds return exactly the rows a brute-force per-row evaluation returns, as bags | `gql_hybrid.rs`, brute force over every row of the label |
| A multi-filter seed equals the intersection of its single-filter seeds | `gql_hybrid.rs` |
| A predicate on a far node (not the seed) gives the same answer as the seed form, with `primary_reads` per hop | `gql_hybrid.rs` |
| Lineage use 1: the moved conjunct's answer equals the unmoved one, and a selector between stops the move | `lang/tests/gql_lineage.rs` (new) |
| Lineage use 2: the early-stopping `Sort` equals the full `Sort` on randomised ties, and reads fewer seed rows | `gql_lineage.rs`; `core/engine/tests/gql_blocking.rs` (extended, `monotone_first`) |
| Exact by default: with only a quantized index and no `ef_search`, the top-k equals brute-force exact distances; with `ef_search` EXPLAIN says APPROXIMATE | `lang/tests/gql_hybrid.rs::vector_is_exact_unless_ef_search` |
| `ef_search` is read when the execution opens: one cached plan, two transactions, two exactness labels | `dist/rust/tests/api.rs` (extended) |
| Complete-or-error ceiling sweep | new `lang/tests/gql_complete_or_error.rs` |
| Vector and geometry parameters over the wire, typed OIDs in `ParameterDescription`, portal slices of a hybrid statement | `dist/tests/pg_wire_gql.rs`, `dist/tests/pg_wire_gql_types.rs` (extended) |
| Brief §9.5 as written, on the tourism fixture, equals its oracle; its first stage ranks exactly | `lang/tests/gql_workloads.rs` (extended) |

**Tasks.**

| Id | Task | Files | Needs | Tests |
| --- | --- | --- | --- | --- |
| M6-A | Host forms in the GQL grammar and IR: parse through `Parser::tsquery` / `Parser::geo_argument`, `Ex` variants, typing, unit and CRS refusals; `host function` row out, `search()` row in | `lang/src/gql/parse/expr.rs`, `lang/src/gql/{ast,expr,types}.rs`, `lang/src/refuse.rs` | M5 merged | `gql_hybrid.rs` (parse, type, refusal), `gql_parse.rs` |
| M6-B | Pure per-row evaluation: spatial predicates and distance, vector distances, `vector_lanes` charge | `lang/src/gql/eval.rs`, `lang/src/gql/scalar.rs` | M6-A | `gql_hybrid.rs` (brute-force equality) |
| M6-C | Core per-node text reads: `ElementReader::{text_matches, text_score}` over one-id prepared queries | `core/engine/src/query/gql/reader.rs` | none (core only) | `core/engine/tests/gql_reader.rs` (extended): equals the collection-level text query restricted to the id; charges |
| M6-D | Hybrid index seeds: `IndexSeed` with filters and order, per-collection seeding for label alternatives, `SeedSource::Index::rank`, EXPLAIN of seeds | `lang/src/gql/{eval,plan}.rs`, `lang/src/gql/plan/explain.rs`; `core/engine/src/query/gql/{plan.rs,ops/seed.rs}` | M6-A, M6-C | `gql_hybrid.rs` (seeds), `core/engine/tests/gql_expand.rs` or a new `gql_seed.rs` for `rank` |
| M6-E | Lineage: `SlotInfo::lineage`, propagation, use 1 (conjunct move) | new `lang/src/gql/lineage.rs`; `lang/src/gql/{schema,stage,plan}.rs` | M6-D | new `lang/tests/gql_lineage.rs` |
| M6-F | Lineage use 2: index-ordered seed and `Sort { monotone_first }` early stop | `core/engine/src/query/gql/{plan.rs,ops/sort.rs,ops/mod.rs}`; `lang/src/gql/stage.rs::sort` | M6-E | `gql_lineage.rs`, `gql_blocking.rs` |
| M6-G | Exact versus approximate: the table of 3.6, `ef_search` read at open, notices, EXPLAIN | `lang/src/gql/{plan,eval}.rs`, `lang/src/gql/plan/explain.rs`; reads `lang/src/compile/mod.rs::EF_SEARCH` (made `pub(crate)` there, its one edit) | M6-D, M6-F | `gql_hybrid.rs::vector_is_exact_unless_ef_search`, `dist/rust/tests/api.rs` |
| M6-H | Complete-or-error sweep and every fix it finds | new `lang/tests/gql_complete_or_error.rs`; fixes where found | M6-D..G | the sweep |
| M6-I | Parameters, portals, arrays over hybrid statements: vector and geometry `$n` typing, OIDs, wire decoding, portal and cursor pins | `lang/src/gql/types.rs`; `dist/src/pg/{connection,types}.rs` only where a gap shows | M6-A, M6-D | `pg_wire_gql.rs`, `pg_wire_gql_types.rs` |
| M6-J | Brief §9.5 as written on the tourism fixture; `QL_CONTRACT` §4.3-§4.6 GQL rows, §5 dialect note on the vector default, `CONTRACT_TEST_MAP.md` rows | `lang/tests/gql_workloads.rs`; `docs/lang/QL_CONTRACT.md`, `docs/lang/CONTRACT_TEST_MAP.md` | M6-A..I | the workload oracle |

Ten tasks.

**Waves.**

| Wave | Parallel worktrees | Shared-file edits |
| --- | --- | --- |
| 1 | M6-A; M6-C | M6-A: lang parse, IR, types, `refuse.rs`. M6-C: core `reader.rs` only. |
| 2 | M6-B; M6-D (core half first: `plan.rs`, `ops/seed.rs`) | M6-B: `eval.rs` evaluation arms, `scalar.rs`. M6-D: `eval.rs` `IndexSeed`/`Program::open_seed` and `plan.rs` seed choice. Both touch `eval.rs` in different items; merge M6-B first. |
| 3 | M6-E; M6-I | M6-E: `schema.rs`, `stage.rs`, `plan.rs`, new `lineage.rs`. M6-I: `types.rs`, dist. No overlap. |
| 4 | M6-F | core `sort.rs`, `plan.rs`, `ops/mod.rs`; lang `stage.rs::sort`. After M6-E. |
| 5 | M6-G | lang `plan.rs`, `eval.rs`, explain; `compile/mod.rs` one visibility edit. |
| 6 | M6-H | fixes wherever the sweep lands; must run alone. |
| 7 | M6-J | last. |

---

## 4. M7: publish the profile

Brief §5 (P0/P1 classification), §10 compatibility policy, §11 step 7, §12
("a feature registry").

### 4.1 Deliverables

| Deliverable | File | Checked by |
| --- | --- | --- |
| The user guide: what a GQL body is, runnable examples, the EXPLAIN reference, compatibility notes | new `docs/lang/GQL_PROFILE.md`, added to `DOCS` in `dist/rust/tests/doc_examples.rs` | the documentation harness runs every `sql` block and every `sql refused` block; a new `sql explain` fence (4.5) |
| The feature registry, the standard mapping, the dialect differences, the unsupported list | new `docs/lang/GQL_FEATURES.md`, added to `DOCS` | a lang unit test that parses its tables (4.2, 4.7) |
| Contract rows | `docs/lang/QL_CONTRACT.md` §1 (the profile paragraph points at the two new files and drops "under construction"), §4.3; `docs/lang/CONTRACT_TEST_MAP.md` | the registry test (every T1 row's test exists) |
| The M1-M4 and M5-M7 designs | stay as design records; their status lines change to "BUILT; the reference is `GQL_PROFILE.md`" | review |

Two files, not one, because the registry is a machine-checked table and the
guide is prose with runnable blocks; keeping them apart lets two tasks write
them in parallel (4.9).

### 4.2 The feature registry

One table in `GQL_FEATURES.md`, one row per supported construct or function:

```text
| construct | class | tier | test |
| `OPTIONAL MATCH <patterns> [WHERE]` | ISO GQL | T1 | lang/tests/gql_optional.rs::several_matches_zero_matches_and_the_input_row_is_never_dropped |
| `ANY CHEAPEST ... COST` | Google reference | T1 | lang/tests/gql_paths.rs::<name> |
| `bm25(col, q)` inside a body | sekejap host (PostgreSQL-style) | T1 | lang/tests/gql_hybrid.rs::<name> |
```

`class` is one of `ISO GQL`, `Google reference`, `SQL/PGQ shared`,
`sekejap host (PostgreSQL-style)`, `sekejap` (brief §5: classify honestly).

**The check** (`lang/src/gql/registry.rs`, `#[cfg(test)]` only, so it adds no
public surface and nothing unused ships):

1. Every `test` cell names a file that exists under the repository and a
   `fn` with that name in it.
2. Every name in `Func::ALL`, `GraphFunc::ALL` and `AggFunc::ALL`
   (`lang/src/gql/ast.rs`) has a registry row, and every function row names
   one of them or a host form. This is why the check lives inside the crate:
   those tables are `pub(crate)`.
3. Every statement keyword the GQL parser accepts by position (`MATCH`,
   `OPTIONAL MATCH`, `LET`, `FILTER`, `FOR`, `RETURN`, `NEXT`, `CALL`,
   `EXISTS`, `UNION`, `WALK`, `TRAIL`, `ACYCLIC`, `ANY`, `SHORTEST`,
   `CHEAPEST`, `COST`) has a row. The list is written once in the test and is
   the one place a new keyword must be added.

A row with no test is not allowed: a construct without a test is not T1
(`QL_CONTRACT`, "The tiers").

### 4.3 Standard mapping and dialect differences

Two tables in `GQL_FEATURES.md`:

* **Mapping.** For each construct: the ISO/IEC 39075 feature it corresponds
  to where one is known, or "Google reference" / "sekejap" where it is not.
  Feature identifiers are copied only from public listings and marked
  "unverified against the normative text" until someone with the standard
  checks them (M1-M4 design §12). Nothing claims conformance.
* **Differences**, each with its reason, in the style of `QL_CONTRACT` §5:
  the graph argument is a named context and `base` is context 0 (Q15);
  labels are collections and edge types (brief §4.1); `ELEMENT_ID` is unique
  per query only (Q16); lists travel as PostgreSQL arrays (Q2); a zero `COST`
  is an error (Q4); `ARRAY_AGG` over zero rows is `NULL` (Q9); binding
  deduplication (Q11); a selector OR a path mode, not both; one `COST` step
  per cheapest pattern (section 1.3 item 5); `CALL` requires an import list;
  `UNION` branches match by name and position; vector exactness differs from
  the SQL surface's default (section 3.6); `IS MISSING` stays SQL-only
  (M1-M4 design §2.5).

### 4.4 Runnable examples

* **Fixture.** A third shape in `dist/rust/tests/doc_examples.rs`
  (`build_tourism`, new) and in `docs/lang/EXAMPLE_FIXTURE.md` (a new
  section): about 40 nodes and 80 edges in the tourism world, with a text
  index, a point index, an exact vector index and one quantized index, so
  every example of the guide runs on it. It touches none of the other
  shapes' names (the harness rule). Keys use public place names in lower case
  (`uluwatu`, `tanah_lot`, `seminyak_beach`, `ubud_market`) and invented
  troupe, dancer and traveller keys (`troupe_a`, `dancer_1`, `traveller_1`).
* **Examples.** At least one runnable block per registry section: patterns,
  stages and `NEXT`, paths and selectors, `OPTIONAL MATCH`, `EXISTS`,
  `CALL`, `UNION`, hybrid seeds, prepared statements (`-- params:`), and one
  `sql refused` block per refusal class (P1, not adopted).
* The examples are new text written for the fixture; none is copied from the
  owner's brief.

### 4.5 The EXPLAIN reference

A section of `GQL_PROFILE.md` lists every operator line EXPLAIN prints, what
each clause means, and which budget resource can stop it. It is checked by a
new harness fence:

````text
```sql explain
SELECT * FROM GRAPH_TABLE (base MATCH (s IS site WHERE s._key = 'uluwatu')-[:near]->(t IS site) RETURN t._key AS k)
-- expect: 1. Seed s: key lookup of 'uluwatu' in site
-- expect: 2. Expand s -[:near]-> t: outgoing edges
```
````

The harness runs `EXPLAIN <statement>` and asserts that each `-- expect:`
line is a PREFIX of some line of the output. Prefixes, so a counter value can
change without a documentation edit, while an operator or a clause cannot
disappear unnoticed. The fence kind is added to the table at the top of
`doc_examples.rs`.

### 4.6 Compatibility notes

A section of `GQL_PROFILE.md`, with a runnable pair of statements for each
point (brief §10 asks for published examples of cardinality and zero-hop
changes):

* the removed `COLUMNS` body and its replacement (`RETURN`), with no alias
  (owner decision, M2-E);
* bag semantics: a diamond returns two paths where the removed body's node
  BFS returned one end once;
* zero-hop quantifiers (`{0,n}`, `*`) include the start; the removed body
  clamped the lower bound to 1;
* a missing seed key is an empty answer, not an error;
* an unbounded quantifier is unbounded, not 16;
* a later `FILTER` is not pushed through a selector.

### 4.7 The unsupported list

A table in `GQL_FEATURES.md`: construct, tier, reason, exactly as
`sekejap_lang::gql_refusals()` returns them. The registry check (4.2) also
asserts that the table's `(construct, tier)` pairs EQUAL the rows of
`refuse::GQL_TABLE`, both ways. A new refusal therefore cannot ship
unpublished, and a published one cannot outlive its row. After M5 and M6 the
expected rows are the P1 constructs (`ALL SHORTEST`, `SIMPLE`, `selector with
a path mode`, `nested quantifier`, the three label forms, `INTERSECT`,
`EXCEPT`, and the new `OPTIONAL block`, `NEXT inside CALL`, `OPTIONAL CALL`,
`mixed UNION`, `search`, `ANY CHEAPEST over several edge steps`) and the
not-adopted ones (`PATH_*`, `VERTEX_ID`, `EDGE_ID`, `COLUMNS`). The cheapest
refusal moves from `SqlError::unsupported` in `automaton.rs` into a
`GQL_TABLE` row in M7-E (Q36).

### 4.8 Acceptance and the release checklist

**Acceptance (owner decision 3).**

1. Full suites on Linux, with the four retained kernel features, as
   `AGENTS.md` lists them: `sekejap-core`, `sekejap-lang`, `sekejap`,
   `sekejap-capi`, `sekejap-kernel`, `sekejap-dist`, and the doc harness.
   The known pre-existing failure
   `edge_write_budget::writing_one_edge_allocates_a_bounded_number_of_times`
   is reported, not hidden.
2. The eight-law lean gate (`tools/run_foundation.py lean`) into a new
   directory.
3. ONE 1M paired run on the acceptance device (a small ARM board) and a resource-limited Linux
server comparator, following `docs/core/V2_BENCHMARK_PROTOCOL.md`,
   with the GQL graph and hybrid cases (`bench/src/bin/battle50k.rs` runs its
   graph cases as GQL today; Q35 asks which workload the owner wants at 1M).
   Each number names its metric, operation, scale, device and baseline. No
   48M run.

**Release checklist (the owner runs it; no task changes a version).**

- [ ] every `GQL_TABLE` row is published (4.7 check green);
- [ ] every registry row's test exists and passes (4.2 check green);
- [ ] every doc block runs (`doc_examples.rs` green);
- [ ] `QL_CONTRACT.md` and `CONTRACT_TEST_MAP.md` rows updated;
- [ ] the acceptance runs above recorded with their artifacts outside the
      repository;
- [ ] the privacy scan of the release diff (no home paths, hosts, addresses,
      personal names or credentials);
- [ ] the version and changelog decisions: owner only.

### 4.9 Tests, tasks and waves

| Rule | Test |
| --- | --- |
| Every registry test exists; every function and keyword has a row | `lang/src/gql/registry.rs` (`#[cfg(test)]`) |
| The unsupported table equals `GQL_TABLE` | same |
| Every example runs; every refused example refuses with its SQLSTATE | `dist/rust/tests/doc_examples.rs` |
| Every EXPLAIN reference line is printed | `doc_examples.rs` (`sql explain` fence) |

| Id | Task | Files | Needs | Tests |
| --- | --- | --- | --- | --- |
| M7-A | Registry format and its check | new `docs/lang/GQL_FEATURES.md` (registry section); new `lang/src/gql/registry.rs` (test only); one `mod` line in `lang/src/gql/mod.rs` | M5, M6 merged | the check |
| M7-B | Standard mapping and dialect differences | `docs/lang/GQL_FEATURES.md` (two sections) | M7-A | review; the registry check covers its construct names |
| M7-C | Tourism fixture shape and the harness's `sql explain` fence | `dist/rust/tests/doc_examples.rs`, `docs/lang/EXAMPLE_FIXTURE.md` | M5, M6 merged | the harness |
| M7-D | The user guide: examples, EXPLAIN reference, compatibility notes | new `docs/lang/GQL_PROFILE.md`; `DOCS` list | M7-C | the harness |
| M7-E | The unsupported list and its equality check; the cheapest refusal as a `GQL_TABLE` row | `docs/lang/GQL_FEATURES.md`, `lang/src/refuse.rs`, `lang/src/gql/automaton.rs`, `lang/tests/gql_parse.rs` | M7-A | the check; `gql_parse.rs` |
| M7-F | Contract text: `QL_CONTRACT` §1 and §4.3, `CONTRACT_TEST_MAP.md`, the two design status lines | `docs/lang/{QL_CONTRACT,CONTRACT_TEST_MAP,GQL_PROFILE_DESIGN,GQL_PROFILE_DESIGN_M5_M7}.md` | M7-A..E | the harness (QL_CONTRACT blocks run) |
| M7-G | Acceptance runs and the release checklist, run when the owner asks | artifacts outside the repository; a results note for the owner | M7-F | section 4.8 |

Seven tasks.

| Wave | Parallel worktrees | Shared-file edits |
| --- | --- | --- |
| 1 | M7-A; M7-C | M7-A: `GQL_FEATURES.md`, `lang/src/gql/{registry,mod}.rs`. M7-C: `doc_examples.rs`, `EXAMPLE_FIXTURE.md`. No overlap. |
| 2 | M7-B; M7-D; M7-E | M7-B and M7-E both edit `GQL_FEATURES.md`, in different sections: merge M7-E first (it changes `refuse.rs` too). M7-D edits only its new file and the `DOCS` list. |
| 3 | M7-F | contract text; after everything it describes. |
| 4 | M7-G | the owner's runs. |

---

## 5. Order across the three milestones

```
M5-A ─┐
M5-B ─┼─ M5-C ─┬─ M5-D ─ M5-F ─┐
      └─ M5-E ─┘               └─ M5-G ─┐
                                        │
M6-C ──────────────┐                    │
M6-A (after M5) ─┬─ M6-D ─ M6-E ─ M6-F ─ M6-G ─ M6-H ─ M6-J
                 ├─ M6-B                        │
                 └─ M6-I ───────────────────────┘
                                                 │
M7-A ─┬─ M7-B, M7-E ─┐                          (M7 starts after M6-J)
M7-C ─┴─ M7-D ───────┴─ M7-F ─ M7-G
```

M6-C (core text reads) depends on nothing in M5 and can start in M5's first
wave. M6-A waits for M5 because both edit `parse/expr.rs`, `ast.rs` and
`refuse.rs`, and the M5 changes there are the larger ones.

Before M5 wave 2, one short step should freeze the three new `OpSpec`
variants and `ExistsMode` as compile-checked code (M5-B delivers exactly
that), so M5-C, M5-D and M5-E plan against a fixed interface.

Regression gates for every task are the M1-M4 design's §10.4 list plus the
new `gql_*` suites of the milestone before. They are run only when the owner
asks for test runs.

---

## 6. Open questions for the owner

Each has a recommended default. Where an owner decision already answers the
question, the default is that decision.

| # | Question | Recommended default |
| --- | --- | --- |
| Q17 | ISO's block form `OPTIONAL { ... }` besides `OPTIONAL MATCH p1, p2`. | Not in P0: a named P1 row. The comma form covers "optional together". |
| Q18 | What an `EXISTS { }` body may contain. | The short form (patterns and `WHERE`) and the full form (statements, then an optional plain `RETURN` whose items are ignored). An aggregate, `GROUP BY`, `ORDER BY`, `OFFSET` or `LIMIT` inside is refused by name. |
| Q19 | `EXISTS` outside a top-level conjunct (inside `OR`, `CASE`, `LET`, `RETURN`). | Accept, through the mark form of `ExistsApply` (a hidden `BOOLEAN` slot). Refuse only inside a quantifier or a selective pattern's inline predicate. |
| Q20 | `CALL` without an import list. | Require `( ... )`; `()` is an uncorrelated call re-run per input row, and EXPLAIN says so. A bare `CALL { }` is refused naming the list. Uncertain: Google's current grammar should be re-checked before publishing (M7-B). |
| Q21 | A `CALL` body that returns no row. | Drop the input row (lateral inner join). `OPTIONAL CALL` (P1) is the outer form. |
| Q22 | `NEXT` inside a `CALL` body. | Refused by a named P1 row; the body is one stage in P0. |
| Q23 | How `UNION` matches columns. | Same count, same names, same order; numeric types unify to `DOUBLE PRECISION`; anything else `42804`. This satisfies both the GQL by-name and the SQL by-position reading. |
| Q24 | Mixing `UNION` and `UNION ALL` in one chain. | Refused by a named P1 row until the parenthesised composite form is built. |
| Q25 | A union after `NEXT`, whose branches must each see the whole incoming table. | Buffer the incoming table once under `sort_bytes` (`Buffered`/`Replay`), refused past the cap like any blocking operator. |
| Q26 | The scan rule: brief §4.3 (no compulsory index on a small graph) versus the owner's Q5 (refuse a label scan whose node carries an unindexed predicate), and the `FILTER` form the code already accepts. | Keep Q5 as decided, and keep `MATCH (n IS L) FILTER <predicate>` as the explicit, EXPLAIN-labelled scan; the Q5 refusal message names it. Lineage moves a conjunct into a seed only when an index answers it, so it never turns this accepted form into a refusal. |
| Q27 | A text predicate or `bm25` over a node whose field has no text index. | Refuse at bind naming the index to create, as SQL does. No per-row analyzer outside the index. |
| Q28 | Owner decision "exact unless `ef_search`, the same as the SQL side", while the SQL side answers approximately (ef 100) when the column has only an approximate index. | GQL follows the decision: with no `ef_search` and no exact index, a vector-ordered seed is a SCAN plus an exact `Sort`, with a notice naming `USING exact`. The SQL default is unchanged in 0.18 and written down as a dialect difference; the owner decides separately whether the SQL side should change. |
| Q29 | When GQL reads `ef_search`. | When an execution opens, so a cached plan follows the transaction it runs in. Flag to the owner that a cached SQL plan keeps the knob it was compiled with (`dist/rust/src/plans.rs` keys by text and catalog generation). |
| Q30 | `search()` and `search_score()` inside GQL, whose dictionary walk truncates with a notice. | Not in P0: a named P1 row. Admitting them would put a notice-level truncation inside a complete-or-error profile. |
| Q31 | The early-stopping `Sort` (lineage use 2) when the index orders only the first key. | Use it whenever the first key is index-ordered and there is a `LIMIT`; the remaining keys are still sorted, so the answer equals the full sort. |
| Q32 | Which operators a conjunct may be moved across by lineage (use 1). | Per-row streaming operators that neither choose among rows nor count them: `Expand`, `Filter`, `Let`, `Project`, `Unnest`, `PathSearch::Enumerate`, and `OptionalApply` when the conjunct reads no slot it introduces. Never across a selector, `Reach`, a blocking operator, `Union` or `CallApply`. |
| Q33 | The registry format. | A Markdown table in `docs/lang/GQL_FEATURES.md`, parsed by a `#[cfg(test)]` module in `lang`. No JSON file, no public API added for the check. |
| Q34 | The documentation fixture for GQL examples. | A third, small tourism graph shape in `doc_examples.rs` and `EXAMPLE_FIXTURE.md`; the `place`/`near` shape stays as it is. |
| Q35 | Which workload the one 1M paired run uses. | The GQL graph cases of `bench/src/bin/battle50k.rs` plus the M6 hybrid seeds, at 1M rows, under `docs/core/V2_BENCHMARK_PROTOCOL.md`, on the acceptance device first and then the server comparator with explicit resource limits. The owner confirms before the run. |
| Q36 | `ANY CHEAPEST` over more than one edge step (one `COST` per step). | Stays out of P0; M7-E moves its refusal from `SqlError::unsupported` in `automaton.rs` to a named `GQL_TABLE` row (P1), so it is published. |

The parked portal lifetime across `Sync` is NOT a question here: it is the
owner's, and no task in this design decides it.

---

## 7. Known limits of this design (stated, not hidden)

* `EXISTS` and `CALL` rebuild their inner operator tree per input row: an
  allocation per row, no store read.
* A union after `NEXT` buffers the incoming table; past `sort_bytes` it is
  refused, with no spill.
* Endpoint sets are not used to answer `EXISTS`; an existence test reads at
  most one posting past the seek.
* A text predicate needs a text index on the field; there is no per-row
  analyzer.
* Lineage stops at a selector, a blocking operator and a `NEXT` that drops
  the node; a conjunct is moved into a seed only when an index answers it.
* The feature-identifier mapping to ISO/IEC 39075 is unverified against the
  normative text, and says so.
* `ALL SHORTEST`, `SIMPLE`, label conjunction and negation, nested
  quantifiers, `INTERSECT`, `EXCEPT`, `OPTIONAL CALL`, `NEXT` inside `CALL`,
  the `OPTIONAL { }` block, mixed union chains, `search()` in GQL and
  multi-step `ANY CHEAPEST` are P1, each refused by name.
