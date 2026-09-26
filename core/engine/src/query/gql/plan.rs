//! The physical plan the engine executes for a GQL pattern
//! (`docs/lang/GQL_PROFILE_DESIGN.md` §3.1, §3.2).
//!
//! A plan is DATA: the language layer's planner builds an [`OpSpec`] tree and
//! [`GqlCursor::open`](super::GqlCursor::open) instantiates it, once per
//! execution. It holds no `EntityId` and no parameter value -- nodes are
//! found again under each execution's snapshot, so a plan can be run again
//! after rows are deleted and reinserted, and rebound without recompiling
//! (§7).
//!
//! The pattern operators are applied PER INPUT ROW and stream: none holds
//! more than one input row, one engine seed page (at most 256 ids) or one
//! adjacency refill (at most 256 postings). The BLOCKING operators --
//! [`OpSpec::Aggregate`], [`OpSpec::Distinct`], [`OpSpec::Sort`] -- and
//! [`OpSpec::Unnest`]'s list hold state across rows and pages, under the
//! `sort_bytes` and `list_bytes` memory caps: there is no spill, so past a
//! cap the page is refused by name (§3.2, §12).
//!
//! A row of one stage has a fixed width: [`OpSpec::Unit`] and
//! [`OpSpec::Project`] state it, every other operator keeps its input's, and
//! every [`SlotId`] an operator names must lie inside it. The cursor checks
//! that when it opens, so a planner bug is an error, never a panic.
//!
//! This vocabulary is FROZEN for the M2 binder and planner. M3 and M4 add
//! variants (aggregation, sort, paths); they do not change these.
//! [`OpSpec::PathSearch`] is M4's: a path pattern with quantifiers or
//! several steps, searched as one automaton (`paths`).
//! [`OpSpec::OptionalApply`] and its [`OpSpec::Argument`] leaf are M3-F's:
//! `OPTIONAL MATCH`. [`OpSpec::Reach`] is M4-F's: the existing node BFS,
//! where the planner has proved it answers a path pattern (§4.6).
//! [`OpSpec::ExistsApply`], [`OpSpec::CallApply`], [`OpSpec::Union`],
//! [`OpSpec::Buffered`] and [`OpSpec::Replay`] are M5-B's: `EXISTS`, `CALL`
//! and `UNION` (`docs/lang/GQL_PROFILE_DESIGN_M5_M7.md` §2.3-§2.5).
//!
//! WHERE an operator may stand is part of the vocabulary. A plan's leaves
//! are [`OpSpec::Unit`] at the top, [`OpSpec::Argument`] inside an apply's
//! inner side, and [`OpSpec::Replay`] inside a branch of a buffered union.
//! The inner side of an `OptionalApply` or an `ExistsApply` holds only
//! streaming operators; a `CallApply`'s may also hold `Project`,
//! `Aggregate`, `Distinct`, `Sort`, `Page` and `CallApply`. `Union` and
//! `Buffered` stand only at the top, outside every inner side and branch
//! after `NEXT`.

use super::host::{ExprId, SeedId};
use super::paths::PathAutomaton;
use super::value::{SlotId, ValueType};
use crate::collections::CollectionId;
use crate::index::graph::{Direction, EdgeTypeId, GraphContextId};

/// One operator of a plan, with its input.
#[derive(Clone, Debug)]
pub enum OpSpec {
    /// One row of `width` `Null` slots: the input of a first stage.
    Unit { width: u16 },
    /// Bind slot `out` to each start node `source` gives, once per input
    /// row; each node is one output row.
    Seed {
        input: Box<OpSpec>,
        out: SlotId,
        source: SeedSource,
    },
    /// One hop from the node in `from`, per input row: one output row per
    /// admitted edge, the edge in `edge` (when the pattern names or needs
    /// it) and the far node in `to`.
    ///
    /// Parallel edges are one row EACH. An either-direction hop walks the
    /// outgoing edges, then the incoming ones, and skips a self-loop in the
    /// incoming walk: a self-loop is one match, not one per orientation.
    /// A `Null` in `from` (or in a [`Target::Bound`] slot) yields no row.
    Expand {
        input: Box<OpSpec>,
        from: SlotId,
        edge: Option<SlotId>,
        to: Target,
        step: StepSpec,
    },
    /// The input rows for which `predicate` is [`Truth::True`](super::Truth).
    Filter {
        input: Box<OpSpec>,
        predicate: ExprId,
    },
    /// Each `(slot, expr)` of `assign`, evaluated against the INPUT row --
    /// so no assignment sees another (`LET a = ..., b = ...`) -- then stored.
    Let {
        input: Box<OpSpec>,
        assign: Box<[(SlotId, ExprId)]>,
    },
    /// A new row of `width` slots: `cols[i]` evaluated into slot `i`, the
    /// rest `Null`. `RETURN`, and the `NEXT` boundary: nothing else of the
    /// input row crosses it.
    Project {
        input: Box<OpSpec>,
        cols: Box<[ExprId]>,
        width: u16,
    },
    /// Grouping (`GROUP BY`, or the implicit grouping of a `RETURN` with an
    /// aggregate): the WHOLE input is folded before the first row out. A new
    /// row of `width` slots per group -- `keys` evaluated into slots
    /// `0..keys.len()`, then one slot per `aggs` entry, the rest `Null` --
    /// in the order the groups were first seen.
    ///
    /// Keys group by identity for nodes, edges and paths and by value for
    /// scalars, `Null` with `Null` (§2.2). With no key there is exactly one
    /// group, so an empty input gives ONE row; with a key an empty input
    /// gives none.
    Aggregate {
        input: Box<OpSpec>,
        keys: Box<[ExprId]>,
        aggs: Box<[AggSpec]>,
        width: u16,
    },
    /// The input rows, each whole row once, the first occurrence kept and
    /// the input order otherwise unchanged. Rows compare slot by slot as
    /// grouping keys do.
    Distinct { input: Box<OpSpec> },
    /// The whole input, ordered by `keys` in turn. Stable: rows equal on
    /// every key keep their input order. Directly under an [`OpSpec::Page`]
    /// with a limit it keeps only the first `offset + limit` rows (top-k).
    ///
    /// `monotone_first`: the planner proved the input arrives ordered by the
    /// FIRST key, in its direction (an index-ordered seed, design §3.5 use
    /// 2). Under a limit the sort then stops pulling once it holds
    /// `offset + limit` rows and an incoming row's first key is strictly
    /// worse than the worst held row's: every row not yet pulled is at least
    /// as bad on that key, so the answer is the full sort's for any
    /// tie-break keys.
    Sort {
        input: Box<OpSpec>,
        keys: Box<[SortKey]>,
        monotone_first: bool,
    },
    /// `OFFSET` / `LIMIT`: skips `offset` rows, then stops after `limit`
    /// (`None`: no bound). Each count is an integer in `0..=i64::MAX`,
    /// checked when the execution opens (design Q13). A limit reached stops
    /// pulling the input.
    Page {
        input: Box<OpSpec>,
        offset: Option<CountExpr>,
        limit: Option<CountExpr>,
    },
    /// `FOR x IN list`: one row per element of `list`, evaluated per input
    /// row, in list order, the element in slot `out`. A `Null` list and an
    /// empty list give no row; a `Null` ELEMENT is a row.
    Unnest {
        input: Box<OpSpec>,
        list: ExprId,
        out: SlotId,
    },
    /// A path pattern searched from the node in `from`, once per input row
    /// (§4): `automaton` is the compiled pattern, `search` how its matches
    /// are chosen. Each match is one output row: the input row with the
    /// pattern's slots bound -- group slots as lists, the path slot as the
    /// path. A `Null` in `from` yields no row.
    PathSearch {
        input: Box<OpSpec>,
        from: SlotId,
        automaton: PathAutomaton,
        search: PathSearch,
    },
    /// The existing node BFS in place of a path search, once per input row
    /// (§4.6): every node reachable from the node in `from` within
    /// `spec.max_hops` hops, each ONCE, in ascending id order -- and the
    /// start itself only when `spec.min_hops` is 0, never through a cycle
    /// back to it. Each end that passes the end test is one output row, in
    /// `to` when the pattern binds it. A `Null` in `from` yields no row.
    ///
    /// The walk keeps one visited set on nodes and loses paths,
    /// multiplicities and every depth but the first, so it answers a path
    /// pattern only where the planner has proved that nothing downstream
    /// can tell the two apart; the operator itself proves nothing.
    Reach {
        input: Box<OpSpec>,
        from: SlotId,
        to: Option<SlotId>,
        spec: ReachSpec,
    },
    /// `OPTIONAL MATCH` (owner answer Q3): a left outer join, per input
    /// row. `inner` runs from each input row -- its leaf is an
    /// [`OpSpec::Argument`] -- and each row it gives is an output row; when
    /// it gives none, the input row comes out once with every slot of
    /// `introduced` `Null`. The optional pattern's `WHERE` belongs in
    /// `inner`, so it decides whether a match exists and never drops an
    /// input row. `inner` keeps the input's width and holds only streaming
    /// operators: `Seed`, `Expand`, `Filter`, `Let`, `Unnest`, `PathSearch`,
    /// `Reach`, `OptionalApply` and `ExistsApply`.
    OptionalApply {
        input: Box<OpSpec>,
        inner: Box<OpSpec>,
        introduced: Box<[SlotId]>,
    },
    /// The leaf of an apply's `inner` side ([`OpSpec::OptionalApply`],
    /// [`OpSpec::ExistsApply`], [`OpSpec::CallApply`]): the input row being
    /// joined, once. `width` is that row's; anywhere else the plan is
    /// refused.
    Argument { width: u16 },
    /// `EXISTS { ... }` and `NOT EXISTS { ... }`: per input row, `inner` runs
    /// from that row -- its leaf an [`OpSpec::Argument`] of the input's
    /// width -- until its FIRST row, and is then dropped; `mode` says what
    /// the answer does to the row. At most one row out per row in: an inner
    /// side with many matches never multiplies the row. The inner tree is
    /// built afresh for each input row, because one stopped at its first row
    /// may still hold a refill or a search frontier (a named cost: one small
    /// allocation per input row, no store read). `inner` holds the streaming
    /// operators an `OptionalApply`'s does, and `ExistsApply`.
    ExistsApply {
        input: Box<OpSpec>,
        inner: Box<OpSpec>,
        mode: ExistsMode,
    },
    /// `CALL (imports) { ... }`: a LATERAL INNER join, per input row. A fresh
    /// instance of `inner` runs from each input row (its leaf an
    /// [`OpSpec::Argument`] of the input's width); each row it returns,
    /// `outputs.len()` slots wide, is written into the `outputs` slots of a
    /// copy of the input row, which is one output row. An input row whose
    /// `inner` returns nothing is dropped. `inner` may hold blocking
    /// operators -- per-input grouping and top-k are the point of `CALL` --
    /// and what they hold is given back when each input row's tree is
    /// dropped. Charges `binding_rows` 1 per row out.
    CallApply {
        input: Box<OpSpec>,
        inner: Box<OpSpec>,
        outputs: Box<[SlotId]>,
    },
    /// `UNION ALL`: the rows of each branch in turn, branch order then row
    /// order; every branch's rows are `width` slots wide. Each branch is a
    /// plan of its own whose leaf is a [`OpSpec::Unit`] (a first part) or,
    /// under an [`OpSpec::Buffered`], a [`OpSpec::Replay`]. `UNION` (and
    /// `UNION DISTINCT`) is the existing [`OpSpec::Distinct`] over this
    /// operator, not a second implementation. Charges nothing itself.
    Union { branches: Box<[OpSpec]>, width: u16 },
    /// A union after `NEXT`: `input`, the incoming table, is read ONCE and
    /// held, then handed to each [`OpSpec::Replay`] leaf of `union` -- which
    /// must be an [`OpSpec::Union`] -- from its start, so every branch sees
    /// the whole table. The table is charged as `sort_bytes`, held until the
    /// last branch has replayed it and refused past the cap like any
    /// blocking operator (Q25). `UNION` is an [`OpSpec::Distinct`] over this
    /// operator.
    Buffered {
        input: Box<OpSpec>,
        union: Box<OpSpec>,
    },
    /// The leaf of a branch of a [`OpSpec::Buffered`] union: every row of
    /// the incoming table, in order. `width` is that table's; anywhere else
    /// the plan is refused.
    Replay { width: u16 },
}

/// What an [`OpSpec::ExistsApply`]'s answer does to its input row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExistsMode {
    /// A top-level conjunct of a filter: keep the input row when the inner
    /// side gives a row (`negated` false) or gives none (`negated` true),
    /// and drop it otherwise.
    Filter { negated: bool },
    /// Anywhere else (inside `OR`, `CASE`, `LET`, `RETURN`): keep every
    /// input row, with whether the inner side gave a row written into
    /// `slot` as a `Bool`. The expression reads that slot.
    Mark { slot: SlotId },
}

/// One [`OpSpec::Reach`]: the edges the BFS crosses, its depth bounds, and
/// the test each end passes AFTER the walk.
#[derive(Clone, Debug)]
pub struct ReachSpec {
    pub context: GraphContextId,
    /// `None` is every type; `Some` of one type walks that type; `Some` of
    /// an empty list walks nothing -- how a type or context name unknown
    /// when the execution opened is planned. The BFS walks one type range,
    /// so two types are refused.
    pub types: Option<Box<[EdgeTypeId]>>,
    pub direction: Direction,
    /// 0 or 1: whether the start is an end at zero hops.
    pub min_hops: u32,
    /// At most 64, the BFS's depth ceiling.
    pub max_hops: u32,
    /// The end's label alternatives, a test on its identity that reads
    /// nothing; `None` is any collection.
    pub end_labels: Option<Box<[CollectionId]>>,
    /// The end's predicate, evaluated per reached node with the end in
    /// `to`: a filter on the answer, never a prune of the walk -- a node it
    /// rejects still leads on to the nodes behind it.
    pub end_filter: Option<ExprId>,
}

/// How an [`OpSpec::PathSearch`] chooses its matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathSearch {
    /// No selector: EVERY match, as a bag -- two paths are two matches even
    /// when they bind the same variables, and two runs of the automaton
    /// with the same path and the same bindings are one match (design
    /// Q11). Depth first, with the mode checked per path (§4.2).
    Enumerate,
    /// `ANY`: one match per (input row, end node) -- some qualifying path.
    /// Today it is the witness [`PathSearch::Shortest`] picks; only "some
    /// match" is promised.
    Any,
    /// `ANY SHORTEST`: one minimum-hop match per (input row, end node), by
    /// a BFS over product states (§4.3). Ties go to the frontier order, then
    /// the step order (outgoing before incoming, types in the step's order,
    /// postings in key order): deterministic for one snapshot, not promised
    /// across engines.
    Shortest,
    /// `ANY CHEAPEST ... COST`: one minimum-cost match per (input row, end
    /// node), by Dijkstra over the same product states; cost ties go to
    /// fewer hops, then to the first found. `cost` is evaluated once per
    /// edge relaxation, over the row with the crossed edge in its slot, and
    /// must be a positive, finite number: anything else -- zero, negative,
    /// `Null`, NaN, infinity -- raises `InvalidPathCost` (Q4). The pattern
    /// crosses exactly one edge step, which binds its edge.
    Cheapest { cost: ExprId },
}

/// One accumulator of an [`OpSpec::Aggregate`]. Its argument is an
/// expression the host evaluates per input row; `Null` arguments are skipped
/// except by `ARRAY_AGG`, which keeps them in the list as SQL does.
///
/// Over a group with no non-null argument, `COUNT` is 0 and every other
/// accumulator is `Null`; `ARRAY_AGG` over zero rows is `Null` (design Q9).
#[derive(Clone, Debug)]
pub enum AggSpec {
    /// `COUNT(*)`: rows.
    CountRows,
    /// `COUNT(x)`, or `COUNT(DISTINCT x)` counting each identity or value
    /// once.
    Count { arg: ExprId, distinct: bool },
    /// `SUM(x)`: an `Int` while every argument is one (a sum past `i64` is
    /// an error), a `Float` once any is.
    Sum(ExprId),
    /// `AVG(x)`: a `Float`.
    Avg(ExprId),
    /// `MIN(x)`, `MAX(x)`: under the internal total order (§2.2), which is
    /// the natural one within the one type the binder admits.
    Min(ExprId),
    Max(ExprId),
    /// `ARRAY_AGG(x)`: the arguments in input order, as a list of `elem`.
    ArrayAgg { arg: ExprId, elem: ValueType },
}

/// One `ORDER BY` key. `Null` sorts as PostgreSQL sorts it: LAST ascending,
/// FIRST descending. Other values compare under the internal total order
/// (§2.2); the binder admits only keys of one comparable type (design Q12).
#[derive(Clone, Copy, Debug)]
pub struct SortKey {
    pub expr: ExprId,
    pub descending: bool,
}

/// An `OFFSET` or `LIMIT` count: a literal, or parameter `$n` (the
/// execution's `params[n]`, which must be an `Int`).
#[derive(Clone, Copy, Debug)]
pub enum CountExpr {
    Lit(u64),
    Param(usize),
}

/// Where a [`OpSpec::Seed`] finds its start nodes.
#[derive(Clone, Debug)]
pub enum SeedSource {
    /// `(n IS L1|L2 WHERE n._key = <key>)`: `key` is evaluated per input
    /// row and looked up in each collection of `labels`, in order -- one
    /// mapping lookup each, charged as `key_postings`. A key no collection
    /// holds, and a `Null` key, give no row: an empty stream, never an error.
    /// A value that is not text is an error (the binder refuses it first).
    Key {
        key: ExprId,
        labels: Box<[CollectionId]>,
    },
    /// Index candidates: [`GqlHost::open_seed`](super::GqlHost::open_seed)
    /// prepares a query per input row, and the engine pages it under what is
    /// left of the page's budget. Its collection is the node's label.
    Index { seed: SeedId },
    /// The node already bound in `slot` -- carried across `NEXT`, or
    /// repeated from an earlier pattern -- when it is in one of `labels`
    /// (`None`: any collection). `slot` may be `out`.
    Bound {
        slot: SlotId,
        labels: Option<Box<[CollectionId]>>,
    },
    /// Every row of each collection of `labels`, in order and in id order
    /// within one: a SCAN. The planner admits it (design Q5); the engine
    /// walks it through the query engine's own entity driver and charges
    /// what that walk charges.
    Scan { labels: Box<[CollectionId]> },
}

/// Where an [`OpSpec::Expand`] puts its far node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// A fresh variable: the far node is stored in this slot.
    New(SlotId),
    /// `ExpandInto`: the far node must BE the node already in this slot.
    /// Only the edges between the two ends are walked into rows.
    Bound(SlotId),
}

/// One hop's pattern.
#[derive(Clone, Debug)]
pub struct StepSpec {
    pub context: GraphContextId,
    /// The edge-type alternatives, one adjacency range each, walked in this
    /// order; `None` is every type, in one range. `Some` of an empty list
    /// matches nothing -- how a type or context name unknown when the
    /// execution opened is planned. A type named twice is refused.
    pub types: Option<Box<[EdgeTypeId]>>,
    /// `Outgoing`, `Incoming` or `Both`.
    pub direction: Direction,
    /// Inline edge predicate, evaluated per edge with the edge and the far
    /// node already in their slots. Needs an `edge` slot. Reading an
    /// incoming edge's property costs one primary-posting read
    /// (`graph_edges`); an outgoing edge carries its bag.
    pub edge_filter: Option<ExprId>,
    /// The far node's label alternatives, a test on its identity that reads
    /// nothing; `None` is any collection.
    pub far_labels: Option<Box<[CollectionId]>>,
    /// Inline far-node predicate, evaluated per edge after `edge_filter`.
    /// Reading the far node's row costs one primary read.
    pub far_filter: Option<ExprId>,
}
