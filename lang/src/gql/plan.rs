//! The planner: a bound body as the engine's operator tree
//! (`docs/lang/GQL_PROFILE_DESIGN.md` §3.1-§3.3, §7; `GRAPH_CONTRACT` §4.3).
//!
//! **Seeds.** Each comma pattern starts at ONE of its nodes, chosen by the
//! cheapest seed that node has, first occurrence winning a tie:
//!
//! 1. `n._key = <expr>` -- a key lookup per label collection
//!    ([`SeedSource::Key`]);
//! 2. `n.f = <expr>`, then `n.f <, <=, >, >= <expr>`, over a READY scalar
//!    index of the node's one label collection ([`SeedSource::Index`],
//!    through [`GqlHost::open_seed`](sekejap_core::collections::gql::GqlHost));
//! 3. a variable an earlier pattern or `MATCH` already bound
//!    ([`SeedSource::Bound`]);
//! 4. otherwise a label scan ([`SeedSource::Scan`]).
//!
//! `<expr>` is a constant, a `$n`, or anything over variables bound before
//! the pattern starts; the conjunct comes from the node's inline `WHERE` or
//! from the `MATCH`'s `WHERE`, and it is CONSUMED: the seed answers it
//! exactly, so it is not evaluated again. A scan starts at a node that
//! carries no predicate of its own; when every node carries one, the
//! pattern is REFUSED, naming the index that would answer it (design Q5,
//! `QL_CONTRACT` §6): a scan with a row-bound predicate reads every row of
//! the label, and a predicate is answered by an index or not at all.
//!
//! **Hops.** From the seed the chain is walked to its right end as written,
//! then from the seed to its left end with each edge's direction reversed.
//! A far node already bound is an `ExpandInto` ([`Target::Bound`]); an edge
//! variable already bound gets a hidden slot and an identity test.
//!
//! **Predicates.** An inline element predicate is evaluated PER HOP, as the
//! element is bound -- an edge's as the hop's edge filter, a far node's as
//! its far filter (one charged row read, `GRAPH_CONTRACT` §4.3), the seed's
//! right after the seed -- or, when it names a variable bound later, after
//! the pattern. A `MATCH`'s `WHERE` is evaluated after all its patterns.
//!
//! **Paths.** A pattern with a quantifier, a subpath, a path mode, a
//! selector or a path variable is ONE [`OpSpec::PathSearch`] over the
//! automaton `automaton.rs` compiles, searched from its FIRST node, whose
//! seed is chosen as above among that node's seeds only (a scan with a
//! predicate is refused naming the index, Q5). When its LAST node has a key
//! seed, that node is seeded first, so the search runs point to point and
//! stops at it (§4.3). The search is `Enumerate` without a selector, and
//! `Any`, `Shortest` or `Cheapest` (its `COST` compiled through the same
//! evaluator) with one.
//!
//! **The node BFS.** Once every stage is planned, a `PathSearch` that
//! design §4.6's seven rules prove equivalent becomes an [`Op::Reach`],
//! the existing node-deduplicating BFS; every other keeps the first rule it
//! failed, for `EXPLAIN` (`reach.rs`).
//!
//! **Names re-resolved at open.** A graph context or an edge type no write
//! has interned yet plans as "matches nothing" ([`StepSpec::types`] =
//! `Some([])`), and an execution looks the name up again when it opens.
//!
//! **Types.** Every output column has a declared SQL spelling decided at
//! compile, never from values (design §6.1): a property of a node is its
//! field's declared type when every label collection declares it alike, and
//! `TEXT` when they differ or none declares it (Q8); an edge property is
//! undeclared, so `TEXT`; a comparison is `BOOLEAN`. Each `$n` has ONE type
//! in the whole statement, from the uses that decide one: a comparison
//! with a declared property or a typed column, a count, a list position
//! ([`GqlPlan::param_types`]); an execution checks each bound value against
//! it when it opens. All of it is decided in `types.rs`.
//!
//! **Stages.** The statements around the patterns -- `LET`, `FILTER`,
//! `FOR`, the `RETURN` with its grouping, sort and page -- and `NEXT`
//! between stages are planned by `stage.rs` into the same operator list;
//! a stage's `Project` is the boundary the next stage reads. The outer
//! `SELECT` over the relation is one more stage after the last, planned
//! the same way (design §5.5). An `OPTIONAL
//! MATCH` is planned exactly as a `MATCH` -- seeds, hops, placement, its
//! `WHERE` after its pattern -- and those operators become the inner side
//! of one [`OpSpec::OptionalApply`], so its `WHERE` decides whether a match
//! exists and never filters the row it preserves.
//!
//! **EXPLAIN.** `GqlPlan::describe` (`explain.rs`) prints the operators
//! this module built -- each seed's access, each predicate's placement, the
//! slot schema and the resources that can stop each operator -- from the
//! plan, never from the statement's text.

use super::ast::GqlGraphTable;
use super::automaton::{self, BoundPath};
use super::bind::{BoundMatch, BoundPattern, Chain, EdgeOcc, Labels, NodeOcc, TypeRef};
use super::convert;
use super::eval::{Host, IndexSeed, Program};
use super::expr::{Conjunct, Ex};
use super::schema::{BindingSchema, Name, SlotInfo};
use super::stage::{StageView, TableOp};
use super::union::Union;
use super::types::{self, spelling, ParamUse};
use crate::ast::CmpOp;
use crate::{Param, SqlError, SqlResult2, SqlValue};
use sekejap_core::collections::gql::{
    BindingRow, BindingValue, ExistsMode, ExprId, GqlBudget, GqlCursor, GqlPage, OpSpec,
    PathAutomaton, PathLink, PathSearch, SeedSource, SlotId, StepSpec, Target, ValueType,
};
use sekejap_core::collections::{
    CollectionId, Database, Direction, EdgeTypeId, GraphContextId, IndexFamily, IndexState,
};

mod explain;
mod reach;

pub(crate) use explain::show;

/// A GQL body, compiled: the program the engine calls back into, the
/// operator tree of all its stages, and the output columns.
#[derive(Clone, Debug)]
pub(crate) struct GqlPlan {
    program: Program,
    /// `texts[e]` is expression `ExprId(e)` as `EXPLAIN` prints it.
    texts: Vec<String>,
    /// The operators, first to last; each is the next one's input.
    ops: Vec<Op>,
    /// The tree, built at compile. `None` when a name was unknown then:
    /// every execution builds its own, looking the names up again.
    root: Option<OpSpec>,
    /// Each stage's slots and operators, for `EXPLAIN`.
    stages: Vec<StageView>,
    /// The first stage's row width.
    width: u16,
    /// The output columns' names, in order.
    columns: Vec<String>,
    /// The type of each output column, which its declared SQL spelling
    /// and the printing of its values follow.
    types: Vec<ValueType>,
    /// How many `$n` an execution must bind, and the one type the
    /// statement gives each (`None`: no use decides it).
    params: Vec<Option<ValueType>>,
    /// The `$n` (zero-based) an `OFFSET` or `LIMIT` reads, checked as a
    /// count when an execution opens (design Q13).
    counts: Vec<usize>,
    /// The graph argument as written, and how it resolved.
    graph: String,
    context: ContextRef,
    /// True when some pattern has an edge, so the graph argument is read.
    reads_graph: bool,
    /// The statement in its normal form (`GqlGraphTable`'s `Display`).
    statement: String,
}

/// One operator, as the planner holds it until the names are resolved.
#[derive(Clone, Debug)]
pub(super) enum Op {
    Seed { out: SlotId, source: SeedSource, access: Access, label: Option<String> },
    Expand { from: SlotId, edge: Option<SlotId>, to: Target, hop: Hop },
    Filter { predicate: ExprId, at: FilterAt },
    Project { cols: Box<[ExprId]>, width: u16 },
    /// `types[i]`: link `i`'s edge types as written, looked up with the
    /// graph context when the tree is built.
    ///
    /// `reach`: the first rule of design §4.6 the search failed, so the
    /// node BFS may not stand in (`EXPLAIN` prints it); `None` until the
    /// whole plan is built and [`reach::choose`] has decided.
    PathSearch {
        from: SlotId,
        automaton: PathAutomaton,
        types: Vec<Option<Vec<TypeRef>>>,
        /// `labels[i]`: link `i`'s edge label as written, for `EXPLAIN`.
        labels: Vec<Option<String>>,
        search: PathSearch,
        reach: Option<reach::Refusal>,
    },
    /// A path search the node BFS answers, proved by design §4.6's seven
    /// rules (`reach.rs`).
    Reach(reach::Reach),
    /// The working-table grammar: `LET`, `FOR`, grouping, `DISTINCT`,
    /// `ORDER BY`, `OFFSET`/`LIMIT` (`stage.rs`).
    Table(TableOp),
    /// `OPTIONAL MATCH` (M3-F): `inner`, the operators its pattern and
    /// `WHERE` planned, run from each input row; `introduced` are the
    /// slots it allocated, `Null` when nothing matched (`stage.rs`).
    Optional { inner: Vec<Op>, introduced: Box<[SlotId]> },
    /// `EXISTS { ... }` (M5-C): `inner`, the operators its body planned,
    /// run from each input row until their first row; `mode` says what that
    /// answer does to the row (`subquery.rs`).
    Exists { inner: Vec<Op>, mode: ExistsMode },
    /// `CALL (...) { ... }` (M5-D, `subquery.rs`): the body's operators, run
    /// per input row, each row they give written into `outputs`; `columns`
    /// names the body's projection for `EXPLAIN`.
    Call {
        inner: Vec<Op>,
        outputs: Box<[SlotId]>,
        columns: Vec<Option<Name>>,
    },
    /// `UNION [ALL | DISTINCT]` over stages (M5-E): each branch's
    /// operators, planned as a stage of their own (`union.rs`).
    Union(Union),
}

/// How a seed reaches its start nodes, as `EXPLAIN` names it.
#[derive(Clone, Debug)]
pub(super) enum Access {
    Key(ExprId),
    Index { name: String, field: String, op: CmpOp, value: String },
    Bound,
    Scan,
}

/// Where a `Filter` stands.
#[derive(Clone, Copy, Debug)]
pub(super) enum FilterAt {
    /// The seed node's own residual predicate, right after the seed.
    Seed,
    /// The `MATCH`'s `WHERE`, and inline predicates that name a variable
    /// bound later, after the `MATCH`'s patterns.
    AfterPattern,
    /// A `FILTER` statement, where it is written.
    Statement,
    /// The outer `SELECT`'s `WHERE`, over the relation's rows.
    Outer,
    /// The outer `SELECT`'s `HAVING`, right after its `Aggregate`, over the
    /// finished group (M3-D2, brief gap 2).
    Having,
}

#[derive(Clone, Debug)]
pub(super) struct Hop {
    types: Option<Vec<TypeRef>>,
    /// The edge label and the far node's label as written, for `EXPLAIN`.
    label: Option<String>,
    far_label: Option<String>,
    direction: Direction,
    edge_filter: Option<ExprId>,
    far_labels: Option<Box<[CollectionId]>>,
    far_filter: Option<ExprId>,
}

/// The graph argument (design Q15): `base` is the base graph, any other
/// name a named context, interned by writes.
#[derive(Clone, Debug)]
enum ContextRef {
    Id(GraphContextId),
    Named(String),
}

impl GqlPlan {
    /// Bind and plan `graph` against `db`'s catalog.
    pub(crate) fn compile(
        db: &Database,
        graph: &GqlGraphTable,
        notices: &mut Vec<String>,
    ) -> SqlResult2<Self> {
        let mut planner = Planner {
            db,
            schema: BindingSchema::default(),
            program: Program::default(),
            texts: Vec::new(),
            ops: Vec::new(),
            bound: Vec::new(),
            everything: None,
            params: 0,
            uses: Vec::new(),
            counts: Vec::new(),
            stages: Vec::new(),
            has_edges: false,
        };
        let output = planner.pipeline(&graph.body, graph.outer.as_ref(), notices)?;
        reach::choose(&mut planner.ops, &planner.program, &planner.stages);
        let reads_graph = planner.has_edges;
        let context = if graph.graph.eq_ignore_ascii_case("base") || !reads_graph {
            ContextRef::Id(GraphContextId::BASE)
        } else {
            match db.graph_context(&graph.graph).map_err(SqlError::from)? {
                Some(id) => ContextRef::Id(id),
                None => {
                    notices.push(format!(
                        "graph context `{}` has no edge yet: every hop matches nothing until one is written, and the name is looked up again each time the statement runs",
                        graph.graph
                    ));
                    ContextRef::Named(graph.graph.clone())
                }
            }
        };
        let unresolved = matches!(context, ContextRef::Named(_)) || names_unknown(&planner.ops);
        let width = planner.stages[0].schema.row_width();
        let root = if unresolved {
            None
        } else {
            Some(tree(db, &context, width, &planner.ops)?)
        };
        let params = planner.param_types();
        Ok(GqlPlan {
            program: planner.program,
            texts: planner.texts,
            ops: planner.ops,
            root,
            stages: planner.stages,
            width,
            columns: output.columns.iter().map(ToString::to_string).collect(),
            types: output.types,
            params,
            counts: planner.counts,
            graph: graph.graph.clone(),
            context,
            reads_graph,
            statement: graph.to_string(),
        })
    }

    /// The operator tree built at compile, for the unit tests that read
    /// the automata it holds (`automaton.rs`).
    #[cfg(test)]
    pub(crate) fn root(&self) -> Option<&OpSpec> {
        self.root.as_ref()
    }

    /// The output columns' names, in order.
    pub(crate) fn columns(&self) -> &[String] {
        &self.columns
    }

    /// The declared SQL spelling of output column `at`.
    pub(crate) fn column_type(&self, at: usize) -> Option<&'static str> {
        self.types.get(at).map(spelling)
    }

    /// The SQL type the statement gives each `$n`: entry `i` is `$i+1`,
    /// `None` where no use decides it.
    pub(crate) fn param_types(&self) -> Vec<Option<&'static str>> {
        self.params.iter().map(|ty| ty.as_ref().map(spelling)).collect()
    }

    /// Run one execution under `params`, handing `body` each page as it is
    /// produced, until the answer is complete or `body` fails. Every page
    /// runs under `budget` and stops when `cancelled` says so.
    pub(crate) fn for_each_page(
        &self,
        db: &Database,
        params: &[Param],
        page_rows: usize,
        budget: GqlBudget,
        cancelled: &mut dyn FnMut() -> bool,
        body: &mut dyn FnMut(&GqlPage) -> SqlResult2<()>,
    ) -> SqlResult2<()> {
        if params.len() < self.params.len() {
            return Err(SqlError::Parameter(format!(
                "${} is not bound: the statement reads {} parameter(s) and {} were given",
                params.len() + 1,
                self.params.len(),
                params.len()
            )));
        }
        for &at in &self.counts {
            if !matches!(params[at], Param::Int(n) if n >= 0) {
                return Err(SqlError::Parameter(format!(
                    "${} is an OFFSET or LIMIT count: an integer from 0 to 9223372036854775807, not {:?}",
                    at + 1,
                    params[at]
                )));
            }
        }
        let values = params
            .iter()
            .enumerate()
            .map(|(at, param)| match self.params.get(at) {
                Some(Some(ty)) => types::typed_param(param, ty, at + 1),
                _ => Ok(convert::from_param(param)),
            })
            .collect::<SqlResult2<Vec<BindingValue>>>()?;
        let fresh;
        let root = match &self.root {
            Some(root) => root,
            None => {
                fresh = tree(db, &self.context, self.width, &self.ops)?;
                &fresh
            }
        };
        let host = Host::new(&self.program);
        let mut cursor =
            GqlCursor::open(db, &host, root, values).map_err(|error| host.error(error))?;
        loop {
            let page = cursor
                .next_page(page_rows, budget, &mut *cancelled)
                .map_err(|error| host.error(error))?;
            body(&page)?;
            if page.done {
                return Ok(());
            }
        }
    }

    /// A row of the final stage as SQL values, one per column, each as its
    /// column's type prints it (`convert::to_sql`).
    pub(crate) fn row(&self, row: &BindingRow) -> SqlResult2<Vec<SqlValue>> {
        self.columns
            .iter()
            .zip(&self.types)
            .zip(row.slots.iter())
            .map(|((name, ty), value)| convert::to_sql(value, ty, name))
            .collect()
    }
}

/// The engine's operator tree of `ops`, over rows `width` slots wide, with
/// every name looked up under `db`.
fn tree(db: &Database, context: &ContextRef, width: u16, ops: &[Op]) -> SqlResult2<OpSpec> {
    let context = match context {
        ContextRef::Id(id) => Some(*id),
        ContextRef::Named(name) => db.graph_context(name).map_err(SqlError::from)?,
    };
    chain(db, context, OpSpec::Unit { width }, width, ops)
}

/// `ops` in turn over `root`, whose rows are `width` slots wide.
pub(super) fn chain(
    db: &Database,
    context: Option<GraphContextId>,
    mut root: OpSpec,
    mut width: u16,
    ops: &[Op],
) -> SqlResult2<OpSpec> {
    for op in ops {
        let input = Box::new(root);
        root = match op {
            Op::Seed { out, source, .. } => OpSpec::Seed {
                input,
                out: *out,
                source: source.clone(),
            },
            Op::Expand { from, edge, to, hop } => OpSpec::Expand {
                input,
                from: *from,
                edge: *edge,
                to: *to,
                step: StepSpec {
                    context: context.unwrap_or(GraphContextId::BASE),
                    // A context not interned holds no edge: the hop
                    // walks nothing.
                    types: match context {
                        None => Some(Box::new([])),
                        Some(_) => types(db, &hop.types)?,
                    },
                    direction: hop.direction,
                    edge_filter: hop.edge_filter,
                    far_labels: hop.far_labels.clone(),
                    far_filter: hop.far_filter,
                },
            },
            Op::Filter { predicate, .. } => OpSpec::Filter {
                input,
                predicate: *predicate,
            },
            Op::Project { cols, width: to } => {
                width = *to;
                OpSpec::Project {
                    input,
                    cols: cols.clone(),
                    width: *to,
                }
            }
            Op::Optional { inner, introduced } => OpSpec::OptionalApply {
                input,
                inner: Box::new(chain(db, context, OpSpec::Argument { width }, width, inner)?),
                introduced: introduced.clone(),
            },
            Op::Exists { inner, mode } => OpSpec::ExistsApply {
                input,
                inner: Box::new(chain(db, context, OpSpec::Argument { width }, width, inner)?),
                mode: *mode,
            },
            Op::Call { inner, outputs, .. } => OpSpec::CallApply {
                input,
                inner: Box::new(chain(db, context, OpSpec::Argument { width }, width, inner)?),
                outputs: outputs.clone(),
            },
            Op::Reach(reach) => reach.spec(db, context, input)?,
            Op::PathSearch {
                from,
                automaton,
                types: written,
                search,
                ..
            } => {
                let mut automaton = automaton.clone();
                for (link, written) in automaton.links.iter_mut().zip(written) {
                    if let PathLink::Edge(step) = link {
                        step.context = context.unwrap_or(GraphContextId::BASE);
                        step.types = match context {
                            None => Some(Box::new([])),
                            Some(_) => types(db, written)?,
                        };
                    }
                }
                OpSpec::PathSearch {
                    input,
                    from: *from,
                    automaton,
                    search: *search,
                }
            }
            Op::Table(op) => op.spec(input),
            Op::Union(union) => {
                let spec = union.spec(db, context, input, width)?;
                width = union.width;
                spec
            }
        };
    }
    Ok(root)
}

/// True when an operator of `ops` -- an `ExistsApply`'s inner steps
/// ([`flat`]) and a union's branches included -- names an edge type no
/// write has interned yet.
fn names_unknown(ops: &[Op]) -> bool {
    let named = |types: &Option<Vec<TypeRef>>| {
        types.iter().flatten().any(|t| matches!(t, TypeRef::Named(_)))
    };
    flat(ops).into_iter().any(|op| match op {
        Op::Expand { hop, .. } => named(&hop.types),
        Op::PathSearch { types, .. } => types.iter().any(named),
        Op::Reach(reach) => named(reach.types()),
        Op::Union(union) => union.branches.iter().any(|branch| names_unknown(&branch.ops)),
        Op::Call { inner, .. } => names_unknown(inner),
        _ => false,
    })
}

/// Every operator of `ops` that one row flow passes, an `OptionalApply`'s
/// or an `ExistsApply`'s inner steps after it. A union's branches are flows
/// of their own, not listed here.
fn flat(ops: &[Op]) -> Vec<&Op> {
    let mut all = Vec::with_capacity(ops.len());
    for op in ops {
        all.push(op);
        if let Op::Optional { inner, .. } | Op::Exists { inner, .. } = op {
            all.extend(flat(inner));
        }
    }
    all
}

/// A hop's edge types under `db`: a name still not interned is dropped,
/// and a label none of whose names is interned matches nothing.
fn types(db: &Database, hop: &Option<Vec<TypeRef>>) -> SqlResult2<Option<Box<[EdgeTypeId]>>> {
    let Some(written) = hop else { return Ok(None) };
    let mut ids = Vec::new();
    for t in written {
        let id = match t {
            TypeRef::Id(id) => Some(*id),
            TypeRef::Named(name) => db.edge_type(name).map_err(SqlError::from)?,
        };
        if let Some(id) = id {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    Ok(Some(ids.into()))
}

/// A seed a node could start its pattern from, cheapest first.
enum SeedChoice {
    Key { at: Taken, key: Ex },
    Index { at: Taken, seed: IndexSeed, name: String, equality: bool },
    Bound,
    Scan,
}

impl SeedChoice {
    fn rank(&self) -> u8 {
        match self {
            Self::Key { .. } => 0,
            Self::Index { equality: true, .. } => 1,
            Self::Index { .. } => 2,
            Self::Bound => 3,
            Self::Scan => 4,
        }
    }
}

/// Where a consumed conjunct lives: a node occurrence's inline list, or the
/// `MATCH`'s `WHERE`.
#[derive(Clone, Copy)]
enum Taken {
    Inline { node: usize, at: usize },
    Where { at: usize },
}

pub(super) struct Planner<'a> {
    pub(super) db: &'a Database,
    /// The schema of the stage being planned (or, while a grouped `RETURN`
    /// or a sort is bound, of the row it reads).
    pub(super) schema: BindingSchema,
    program: Program,
    pub(super) texts: Vec<String>,
    pub(super) ops: Vec<Op>,
    /// Slots an operator has filled so far.
    pub(super) bound: Vec<SlotId>,
    /// Every collection, for an unlabelled node's key lookup or scan.
    everything: Option<Box<[CollectionId]>>,
    /// How many `$n` the statement reads: the highest `n`.
    pub(super) params: usize,
    /// What the statement's uses of each `$n` say about its type.
    pub(super) uses: Vec<ParamUse>,
    /// The `$n` (zero-based) read as `OFFSET`/`LIMIT` counts.
    pub(super) counts: Vec<usize>,
    /// The stages planned so far.
    pub(super) stages: Vec<StageView>,
    /// True once a pattern has an edge, so the graph argument is read.
    has_edges: bool,
}

impl Planner<'_> {
    fn is_bound(&self, slot: SlotId) -> bool {
        self.bound.contains(&slot)
    }

    pub(super) fn bind_slot(&mut self, slot: SlotId) {
        if !self.is_bound(slot) {
            self.bound.push(slot);
        }
    }

    /// One `Filter` for the conjunction of `predicates`, if there are any.
    pub(super) fn filter(&mut self, predicates: Vec<Ex>, at: FilterAt) {
        if let Some(predicate) = self.conjunction(predicates) {
            self.ops.push(Op::Filter { predicate, at });
        }
    }

    /// Add `ex` to the program, keeping how `EXPLAIN` prints it.
    pub(super) fn expr(&mut self, ex: Ex) -> ExprId {
        self.texts.push(show(&ex, &self.schema));
        self.program.expr(ex)
    }

    /// One `MATCH`: each pattern, then the rest of its `WHERE` after all of
    /// them.
    pub(super) fn matched(&mut self, matched: BoundMatch) -> SqlResult2<()> {
        self.has_edges |= matched.patterns.iter().any(|pattern| match pattern {
            BoundPattern::Chain(chain) => !chain.edges.is_empty(),
            BoundPattern::Path(path) => path.has_edges(),
        });
        let mut exprs: Vec<Ex> = matched.where_.iter().map(|c| c.ex.clone()).collect();
        for pattern in &matched.patterns {
            match pattern {
                BoundPattern::Chain(chain) => {
                    for node in &chain.nodes {
                        exprs.extend(node.inline.iter().map(|c| c.ex.clone()));
                    }
                    for edge in &chain.edges {
                        exprs.extend(edge.inline.iter().map(|c| c.ex.clone()));
                    }
                }
                BoundPattern::Path(path) => exprs.extend(path.exprs().cloned()),
            }
        }
        for ex in &exprs {
            self.note_params(ex)?;
        }
        let mut where_: Vec<Option<Conjunct>> = matched.where_.into_iter().map(Some).collect();
        let mut deferred = Vec::new();
        for pattern in matched.patterns {
            match pattern {
                BoundPattern::Chain(chain) => self.pattern(chain, &mut where_, &mut deferred)?,
                BoundPattern::Path(path) => self.path(path, &mut where_, &mut deferred)?,
            }
        }
        deferred.extend(where_.into_iter().flatten());
        self.filter(deferred.into_iter().map(|c| c.ex).collect(), FilterAt::AfterPattern);
        Ok(())
    }

    fn pattern(
        &mut self,
        mut pattern: Chain,
        where_: &mut [Option<Conjunct>],
        deferred: &mut Vec<Conjunct>,
    ) -> SqlResult2<()> {
        let (start, choice) = self.choose_seed(&pattern, where_)?;
        self.seed(&mut pattern.nodes, start, choice, where_)?;
        let inline = std::mem::take(&mut pattern.nodes[start].inline);
        let now = self.place(inline, deferred);
        self.filter(now, FilterAt::Seed);
        // Right of the seed as written, then left of it reversed.
        let Chain { mut nodes, mut edges } = pattern;
        for at in start..edges.len() {
            let from = nodes[at].slot;
            self.hop(from, &mut edges[at], &mut nodes[at + 1], false, deferred)?;
        }
        for at in (0..start).rev() {
            let from = nodes[at + 1].slot;
            self.hop(from, &mut edges[at], &mut nodes[at], true, deferred)?;
        }
        Ok(())
    }

    /// Seed `nodes[at]` as `choice` says, consuming the conjunct it
    /// answers.
    fn seed(
        &mut self,
        nodes: &mut [NodeOcc],
        at: usize,
        choice: SeedChoice,
        where_: &mut [Option<Conjunct>],
    ) -> SqlResult2<()> {
        let slot = nodes[at].slot;
        let labels = nodes[at].labels.as_ref().map(|l| l.ids.clone());
        let label = nodes[at].labels.as_ref().map(|l| l.written.clone());
        let (source, access) = match choice {
            SeedChoice::Key { at, key } => {
                take(at, nodes, where_);
                let key = self.expr(key);
                let labels = self.labels_or_all(labels)?;
                (SeedSource::Key { key, labels }, Access::Key(key))
            }
            SeedChoice::Index { at, seed, name, .. } => {
                take(at, nodes, where_);
                let access = Access::Index {
                    name,
                    field: seed.field.clone(),
                    op: seed.op,
                    value: show(&seed.value, &self.schema),
                };
                let seed = self.program.seed(seed);
                (SeedSource::Index { seed }, access)
            }
            SeedChoice::Bound => (SeedSource::Bound { slot, labels }, Access::Bound),
            SeedChoice::Scan => {
                let labels = self.labels_or_all(labels)?;
                (SeedSource::Scan { labels }, Access::Scan)
            }
        };
        self.ops.push(Op::Seed {
            out: slot,
            source,
            access,
            label,
        });
        self.bind_slot(slot);
        Ok(())
    }

    /// A path pattern: its seed or seeds, then one path search.
    fn path(
        &mut self,
        mut path: BoundPath,
        where_: &mut [Option<Conjunct>],
        deferred: &mut Vec<Conjunct>,
    ) -> SqlResult2<()> {
        let last = path.nodes.len() - 1;
        let (start, end) = (path.nodes[0].slot, path.nodes[last].slot);
        // A key on the far end: seed it first, and the search runs point to
        // point, stopping when it reaches that node.
        let mut end_seeded = false;
        if end != start && !self.is_bound(end) {
            if let choice @ SeedChoice::Key { .. } = self.seed_for(last, &path.nodes[last], where_)? {
                self.seed(&mut path.nodes, last, choice, where_)?;
                end_seeded = true;
            }
        }
        let choice = self.seed_for(0, &path.nodes[0], where_)?;
        if matches!(choice, SeedChoice::Scan) {
            if let Some(predicate) = own_predicate(&path.nodes[0], where_) {
                return Err(self.unindexed(&path.nodes[0], predicate));
            }
        }
        self.seed(&mut path.nodes, 0, choice, where_)?;
        let compiled = path.compile(self, end_seeded)?;
        self.filter(compiled.start, FilterAt::Seed);
        deferred.extend(compiled.deferred);
        for slot in automaton::binds(&compiled.automaton) {
            self.bind_slot(slot);
        }
        self.ops.push(Op::PathSearch {
            from: start,
            automaton: compiled.automaton,
            types: compiled.types,
            labels: compiled.labels,
            search: compiled.search,
            reach: None,
        });
        Ok(())
    }

    /// The conjuncts whose slots are all bound now, ANDed; the rest go to
    /// `deferred`, evaluated after the pattern.
    fn place(&self, conjuncts: Vec<Conjunct>, deferred: &mut Vec<Conjunct>) -> Vec<Ex> {
        let mut now = Vec::new();
        for conjunct in conjuncts {
            if conjunct.refs.iter().all(|slot| self.is_bound(*slot)) {
                now.push(conjunct.ex);
            } else {
                deferred.push(conjunct);
            }
        }
        now
    }

    fn hop(
        &mut self,
        from: SlotId,
        edge: &mut EdgeOcc,
        far: &mut NodeOcc,
        reversed: bool,
        deferred: &mut Vec<Conjunct>,
    ) -> SqlResult2<()> {
        let mut identity = None;
        let edge_slot = match edge.var {
            Some(slot) if self.is_bound(slot) => {
                // The same edge again: a hidden slot, and the identity test.
                let hidden = self.hidden(self.schema.slot(slot).ty.clone(), edge)?;
                identity = Some(Ex::Compare(
                    CmpOp::Eq,
                    Box::new(Ex::Slot(hidden)),
                    Box::new(Ex::Slot(slot)),
                ));
                Some(hidden)
            }
            Some(slot) => Some(slot),
            // An anonymous edge with a predicate still needs its slot.
            None if !edge.inline.is_empty() => Some(self.hidden(ValueType::Edge(Box::new([])), edge)?),
            None => None,
        };
        let to = if self.is_bound(far.slot) {
            Target::Bound(far.slot)
        } else {
            Target::New(far.slot)
        };
        if let Some(slot) = edge_slot {
            self.bind_slot(slot);
        }
        self.bind_slot(far.slot);
        let mut edge_now = self.place(std::mem::take(&mut edge.inline), deferred);
        edge_now.extend(identity);
        let far_now = self.place(std::mem::take(&mut far.inline), deferred);
        let edge_filter = self.conjunction(edge_now);
        let far_filter = self.conjunction(far_now);
        self.ops.push(Op::Expand {
            from,
            edge: edge_slot,
            to,
            hop: Hop {
                types: edge.types.clone(),
                label: edge.label.clone(),
                far_label: far.labels.as_ref().map(|l| l.written.clone()),
                direction: edge.direction.engine(reversed),
                edge_filter,
                far_labels: far.labels.as_ref().map(|l| l.ids.clone()),
                far_filter,
            },
        });
        Ok(())
    }

    pub(super) fn conjunction(&mut self, predicates: Vec<Ex>) -> Option<ExprId> {
        let ex = predicates.into_iter().fold(None, |acc, ex| Some(Ex::and(acc, ex)))?;
        Some(self.expr(ex))
    }

    /// A hidden slot for an edge occurrence the plan must hold but no
    /// variable names: an anonymous edge with a predicate, or a variable's
    /// second occurrence.
    fn hidden(&mut self, ty: ValueType, edge: &EdgeOcc) -> SqlResult2<SlotId> {
        self.schema.add(SlotInfo::hidden(ty, edge.provenance.clone()))
    }

    fn labels_or_all(&mut self, labels: Option<Box<[CollectionId]>>) -> SqlResult2<Box<[CollectionId]>> {
        if let Some(labels) = labels {
            return Ok(labels);
        }
        if self.everything.is_none() {
            let mut all = Vec::new();
            for (schema, name) in self.db.list_qualified_collections().map_err(SqlError::from)? {
                if let Some(id) = self.db.collection_in(&schema, &name).map_err(SqlError::from)? {
                    all.push(id);
                }
            }
            self.everything = Some(all.into());
        }
        Ok(self.everything.clone().expect("filled above"))
    }

    /// The node this pattern starts at, and how.
    fn choose_seed(
        &self,
        pattern: &Chain,
        where_: &[Option<Conjunct>],
    ) -> SqlResult2<(usize, SeedChoice)> {
        let mut best: Option<(usize, SeedChoice)> = None;
        for (index, node) in pattern.nodes.iter().enumerate() {
            let choice = self.seed_for(index, node, where_)?;
            if best.as_ref().is_none_or(|(_, b)| choice.rank() < b.rank()) {
                best = Some((index, choice));
            }
        }
        let (start, choice) = best.expect("a pattern has a node");
        if !matches!(choice, SeedChoice::Scan) {
            return Ok((start, choice));
        }
        // A scan: from a node with no predicate of its own, if there is one.
        if let Some(free) = pattern
            .nodes
            .iter()
            .position(|node| own_predicate(node, where_).is_none())
        {
            return Ok((free, SeedChoice::Scan));
        }
        let node = &pattern.nodes[0];
        let predicate = own_predicate(node, where_).expect("every node has a predicate");
        Err(self.unindexed(node, predicate))
    }

    /// The best seed `node` offers.
    fn seed_for(
        &self,
        index: usize,
        node: &NodeOcc,
        where_: &[Option<Conjunct>],
    ) -> SqlResult2<SeedChoice> {
        // A variable an earlier pattern bound is one node already: it is
        // re-seeded as itself, and its key or index conjunct stays a test.
        if self.is_bound(node.slot) {
            return Ok(SeedChoice::Bound);
        }
        let mut best = SeedChoice::Scan;
        let candidates = node
            .inline
            .iter()
            .enumerate()
            .map(|(at, c)| (Taken::Inline { node: index, at }, c))
            .chain(
                where_
                    .iter()
                    .enumerate()
                    .filter_map(|(at, c)| c.as_ref().map(|c| (Taken::Where { at }, c))),
            );
        for (at, conjunct) in candidates {
            let Some((op, field, value)) = self.seedable(node.slot, &conjunct.ex) else {
                continue;
            };
            let choice = if field == crate::KEY_COLUMN {
                if op != CmpOp::Eq {
                    continue;
                }
                if let Ex::Const(value) = value {
                    if !matches!(value, BindingValue::Text(_) | BindingValue::Null) {
                        return Err(SqlError::unsupported(format!(
                            "`_key` is text and is compared with {value:?}: a key seed looks up a text key"
                        )));
                    }
                }
                SeedChoice::Key {
                    at,
                    key: value.clone(),
                }
            } else {
                match self.index_seed(node, op, field, value)? {
                    Some((seed, name)) => SeedChoice::Index {
                        at,
                        seed,
                        name,
                        equality: op == CmpOp::Eq,
                    },
                    None => continue,
                }
            };
            if choice.rank() < best.rank() {
                best = choice;
            }
        }
        Ok(best)
    }

    /// `node.field op value`, with the field on the left, when `ex` is one
    /// and `value` can be evaluated before the node is bound.
    fn seedable<'e>(&self, node: SlotId, ex: &'e Ex) -> Option<(CmpOp, &'e str, &'e Ex)> {
        let Ex::Compare(op, left, right) = ex else { return None };
        let (op, field, value) = match (&**left, &**right) {
            (Ex::NodeProperty(slot, field), value) if *slot == node => (*op, field, value),
            (value, Ex::NodeProperty(slot, field)) if *slot == node => (flip(*op), field, value),
            _ => return None,
        };
        if op == CmpOp::Ne || !value.refs().iter().all(|slot| self.is_bound(*slot)) {
            return None;
        }
        Some((op, field, value))
    }

    /// A READY scalar index answering `node.field op value`: only over a
    /// node of ONE label collection, since an index is one collection's.
    fn index_seed(
        &self,
        node: &NodeOcc,
        op: CmpOp,
        field: &str,
        value: &Ex,
    ) -> SqlResult2<Option<(IndexSeed, String)>> {
        let Some(Labels { ids, .. }) = &node.labels else { return Ok(None) };
        let [collection] = **ids else { return Ok(None) };
        let indexes = self.db.list_indexes(collection).map_err(SqlError::from)?;
        Ok(indexes
            .into_iter()
            .find(|info| {
                info.field == field
                    && info.family == IndexFamily::Scalar
                    && info.expression.is_none()
                    && info.state == IndexState::Ready
            })
            .map(|info| {
                let seed = IndexSeed {
                    collection,
                    index: info.id,
                    kind: info.kind,
                    field: field.to_owned(),
                    op,
                    value: value.clone(),
                };
                (seed, info.name)
            }))
    }

    /// The refusal of a label scan whose every node carries a predicate no
    /// seed answers (design Q5): it names the label, the field and the index
    /// that would answer it.
    fn unindexed(&self, node: &NodeOcc, predicate: &Ex) -> SqlError {
        let label = node
            .labels
            .as_ref()
            .map_or_else(|| "every collection".to_owned(), |l| format!("`{}`", l.written));
        let field = first_property(node.slot, predicate);
        let create = match (&node.labels, field) {
            (Some(labels), Some(field)) if labels.ids.len() == 1 => format!(
                "CREATE INDEX ON {} USING btree ({field}) answers it",
                labels.written
            ),
            _ => "a scalar index on one label's field answers it".to_owned(),
        };
        SqlError::unsupported(format!(
            "a label scan of {label} with a predicate on {}: no index answers it, and a scan that reads every row to test it is refused (GQL profile Q5, QL_CONTRACT §6). {create}; or start the pattern from a key (`_key = ...`) or from a bound variable; or, to scan on purpose, test it after the pattern with FILTER (`MATCH (n IS site) FILTER n.rating > 4` reads every row of the label and is budgeted, design §2.6)",
            field.map_or_else(|| "the node".to_owned(), |f| format!("`{f}`"))
        ))
    }
}

/// A predicate of `node`'s own: its inline `WHERE`, or a `WHERE` conjunct
/// that reads no other variable.
fn own_predicate<'p>(node: &'p NodeOcc, where_: &'p [Option<Conjunct>]) -> Option<&'p Ex> {
    node.inline
        .iter()
        .chain(where_.iter().flatten().filter(|c| c.refs == [node.slot]))
        .map(|c| &c.ex)
        .next()
}

/// `a op b` read as `b op' a`.
fn flip(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
        other => other,
    }
}

/// The first property of `slot` that `ex` reads.
fn first_property(slot: SlotId, ex: &Ex) -> Option<&str> {
    match ex {
        Ex::NodeProperty(s, field) if *s == slot => Some(field),
        other => other
            .children()
            .into_iter()
            .find_map(|child| first_property(slot, child)),
    }
}

/// Remove a consumed conjunct from where it lives.
fn take(at: Taken, nodes: &mut [NodeOcc], where_: &mut [Option<Conjunct>]) {
    match at {
        Taken::Inline { node, at } => {
            nodes[node].inline.remove(at);
        }
        Taken::Where { at } => where_[at] = None,
    }
}

