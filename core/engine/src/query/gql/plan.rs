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
    Sort {
        input: Box<OpSpec>,
        keys: Box<[SortKey]>,
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
    /// `Reach` and `OptionalApply`.
    OptionalApply {
        input: Box<OpSpec>,
        inner: Box<OpSpec>,
        introduced: Box<[SlotId]>,
    },
    /// The leaf of an [`OpSpec::OptionalApply`]'s `inner` side: the input
    /// row being joined, once. `width` is that row's; anywhere else the
    /// plan is refused.
    Argument { width: u16 },
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
