//! M4-E: an independent check that the path search gives the RIGHT BAG of
//! answers (`docs/lang/GQL_PROFILE_DESIGN.md` §4, task table row M4-E).
//!
//! This file's reference is written from scratch, for clarity, and shares
//! NO code with `query/gql/paths/{automaton,bfs,dijkstra,enumerate}.rs`: it
//! never calls into them, and its algorithm is the opposite order from
//! theirs. `gql_paths_enumerate.rs` (M4-B) and `gql_paths_shortest.rs`
//! (M4-C) each pick a quantifier COUNT first, expand it into one straight
//! line, and then walk that line through the graph. This oracle walks the
//! GRAPH first -- every real walk up to a hop bound, mode-checked as it
//! goes -- and only afterwards asks, for each walk, whether some quantifier
//! count would explain it (`raw_walks`, `try_match`). Two different
//! programs computing the same brief.
//!
//! Coverage:
//! * mode: WALK, TRAIL, ACYCLIC;
//! * selector: none (`Enumerate`, exact bag), ANY, ANY SHORTEST, ANY
//!   CHEAPEST (properties: one row per (start, end), minimal length or
//!   cost, and a valid path -- never a specific tie-break);
//! * quantifier shape: `{0,0}`, `{0,n}`, `?`, `*`, `+`, a single quantified
//!   edge and a two-edge quantified subpath;
//! * seeded random small multigraphs with parallel edges, self-loops and
//!   cycles.
//!
//! A mismatch is a real defect: kept as a failing `#[ignore]` test with its
//! minimal seed and graph printed (see the bottom of this file), never a
//! weakened oracle.

use sekejap_core::collections::gql::{
    BindingRow, BindingValue, EdgeRef, EdgeStep, EvalCx, ExprId, GqlBudget, GqlCursor, GqlHost,
    ListRef, NodeRef, NodeTest, OpSpec, PathAutomaton, PathLink, PathMode, PathRef, PathSearch,
    Repeat, SeedId, SeedSource, SlotId, Truth, ValueType,
};
use sekejap_core::collections::{
    CollectionId, Database, Direction, EdgeKey, EdgeTypeId, EntityId, GraphContextId,
    PreparedQuery, QueryResult,
};
use sekejap_core::Kind;
use serde_json::json;
use std::collections::HashMap;

mod common;
use common::cfg;

/// splitmix64: reproducible from its seed. A generic, published algorithm,
/// not engine code.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

// ── the test host: only what the search needs to evaluate a predicate ─────

#[derive(Clone, Debug)]
enum E {
    Slot(u16),
    /// `slot.x < k` on a node, `slot.w < k` on an edge; `Null` is unknown.
    Below(u16, &'static str, i64),
    /// The `w` property of the edge in `slot`, as the Dijkstra `COST`.
    Cost(u16),
}

#[derive(Default)]
struct TestHost {
    exprs: Vec<E>,
}

impl TestHost {
    fn expr(&mut self, e: E) -> ExprId {
        self.exprs.push(e);
        ExprId(self.exprs.len() as u32 - 1)
    }

    /// The COST expression for a pattern whose repeated edge is bound to
    /// `slot` (every `Line` built by `quantified_line` in this file binds
    /// it to slot 1).
    fn cost(&mut self, slot: u16) -> ExprId {
        self.expr(E::Cost(slot))
    }
}

impl GqlHost for TestHost {
    fn eval(
        &self,
        expr: ExprId,
        row: &BindingRow,
        cx: &mut EvalCx<'_, '_>,
    ) -> QueryResult<BindingValue> {
        Ok(match &self.exprs[expr.0 as usize] {
            E::Slot(s) => row.get(SlotId(*s)).clone(),
            E::Below(s, name, k) => {
                let v = match row.get(SlotId(*s)) {
                    BindingValue::Node(n) => cx.reader.node_property(*n, name, cx.meter)?,
                    BindingValue::Edge(e) => cx.reader.edge_property(e, name, cx.meter)?,
                    other => panic!("a predicate saw {other:?}, not a singleton element"),
                };
                match v {
                    BindingValue::Int(v) => BindingValue::Bool(v < *k),
                    _ => BindingValue::Null,
                }
            }
            E::Cost(s) => match row.get(SlotId(*s)) {
                BindingValue::Edge(e) => cx.reader.edge_property(e, "w", cx.meter)?,
                other => panic!("a COST saw {other:?}, not an edge"),
            },
        })
    }

    fn test(&self, expr: ExprId, row: &BindingRow, cx: &mut EvalCx<'_, '_>) -> QueryResult<Truth> {
        Ok(match self.eval(expr, row, cx)? {
            BindingValue::Bool(true) => Truth::True,
            BindingValue::Bool(false) => Truth::False,
            _ => Truth::Unknown,
        })
    }

    fn open_seed<'db>(
        &self,
        _: SeedId,
        _: &'db Database,
        _: &BindingRow,
        _: &mut EvalCx<'_, '_>,
    ) -> QueryResult<Option<PreparedQuery<'db>>> {
        unreachable!("these plans seed by scan")
    }
}

// ── the graph model ─────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct MNode {
    id: EntityId,
    x: Option<i64>,
}

#[derive(Clone, Debug)]
struct MEdge {
    key: EdgeKey,
    id: u64,
    w: Option<i64>,
}

struct Model {
    db: Database,
    cols: Vec<CollectionId>,
    types: Vec<EdgeTypeId>,
    nodes: Vec<MNode>,
    edges: Vec<MEdge>,
}

impl Model {
    fn node(&self, id: EntityId) -> &MNode {
        self.nodes.iter().find(|n| n.id == id).unwrap()
    }
}

/// A graph of `nodes` (collection index, `x`) and `edges` (source index,
/// destination index, type index, `w`), in collections `site_a`, `site_b`
/// and edge types `r`, `s`.
fn build(
    dir: &std::path::Path,
    nodes: &[(usize, Option<i64>)],
    edges: &[(usize, usize, usize, Option<i64>)],
) -> Model {
    let mut db = Database::create(dir.join("g.sekejap"), cfg()).unwrap();
    let mut cols = Vec::new();
    for name in ["site_a", "site_b"] {
        cols.push(
            db.create_collection(name, vec![("x".into(), Kind::Int)], Default::default())
                .unwrap(),
        );
    }
    db.enable_graph().unwrap();
    let types = vec![
        db.create_edge_type("r").unwrap(),
        db.create_edge_type("s").unwrap(),
    ];
    let mut model_nodes = Vec::new();
    for (i, &(c, x)) in nodes.iter().enumerate() {
        let doc = match x {
            Some(x) => json!({ "x": x }),
            None => json!({}),
        };
        let id = db.put(cols[c], &format!("n{i}"), &doc).unwrap();
        model_nodes.push(MNode { id, x });
    }
    let mut model_edges = Vec::new();
    for &(s, d, t, w) in edges {
        let bag = match w {
            Some(w) => json!({ "w": w }),
            None => json!({}),
        };
        let made = db
            .create_edge(
                GraphContextId::BASE,
                model_nodes[s].id,
                types[t],
                model_nodes[d].id,
                &bag,
            )
            .unwrap();
        model_edges.push(MEdge {
            key: made.key,
            id: made.id,
            w,
        });
    }
    db.commit().unwrap();
    Model {
        db,
        cols,
        types,
        nodes: model_nodes,
        edges: model_edges,
    }
}

/// A random multigraph, small on purpose: parallel twins, a self-loop and a
/// cycle are forced in. `weighted`: every edge carries a positive weight
/// (needed for ANY CHEAPEST; otherwise weights are sometimes absent).
fn random_graph(dir: &std::path::Path, seed: u64, weighted: bool) -> Model {
    let mut rng = Rng(seed);
    let n = 5 + rng.below(2) as usize; // 5 or 6 nodes
    let ns: Vec<(usize, Option<i64>)> = (0..n)
        .map(|_| {
            (
                rng.below(2) as usize,
                (rng.below(5) != 0).then(|| rng.below(10) as i64),
            )
        })
        .collect();
    let w = |rng: &mut Rng| -> Option<i64> {
        if weighted {
            Some(1 + rng.below(9) as i64)
        } else {
            (rng.below(4) != 0).then(|| rng.below(10) as i64)
        }
    };
    let mut es: Vec<(usize, usize, usize, Option<i64>)> = Vec::new();
    // A cycle through every node, so there is always at least one cycle.
    for i in 0..n {
        es.push((i, (i + 1) % n, rng.below(2) as usize, w(&mut rng)));
    }
    // A forced parallel twin and a forced self-loop.
    let (s, d, t) = (
        rng.below(n as u64) as usize,
        rng.below(n as u64) as usize,
        0,
    );
    es.push((s, d, t, w(&mut rng)));
    es.push((s, d, t, w(&mut rng)));
    let sl = rng.below(n as u64) as usize;
    es.push((sl, sl, rng.below(2) as usize, w(&mut rng)));
    // A few more random edges.
    for _ in 0..(3 + rng.below(3) as usize) {
        es.push((
            rng.below(n as u64) as usize,
            rng.below(n as u64) as usize,
            rng.below(2) as usize,
            w(&mut rng),
        ));
    }
    build(dir, &ns, &es)
}

// ── the surface pattern the test writes, and its lowering to the engine's
//    line automaton (`PathAutomaton`) ──────────────────────────────────────

#[derive(Clone, Debug, Default)]
struct St {
    labels: Option<Vec<usize>>,
    below: Option<i64>,
    bind: Option<u16>,
}

fn at(bind: Option<u16>) -> St {
    St {
        bind,
        ..St::default()
    }
}

#[derive(Clone, Debug)]
struct Ed {
    dir: Direction,
    types: Option<Vec<usize>>,
    below: Option<i64>,
    cost: bool,
    bind: Option<u16>,
}

/// An edge with no predicate, no cost and no bind.
fn plain(dir: Direction, types: Option<Vec<usize>>) -> Ed {
    Ed {
        dir,
        types,
        below: None,
        cost: false,
        bind: None,
    }
}

/// A quantified run: `body.states.len()` positions, `body.states.len() - 1`
/// edges, repeated `min..=max` times (`max: None` unbounded).
#[derive(Clone, Debug)]
struct Body {
    states: Vec<St>,
    edges: Vec<Ed>,
    min: u32,
    max: Option<u32>,
}

/// The whole pattern: a fixed prefix, an optional quantified body, a fixed
/// suffix. `start_slot`/`end_slot` name the first and last node's binds, so
/// the oracle can group matches by (start, end) for the selector checks.
#[derive(Clone, Debug)]
struct Line {
    prefix_states: Vec<St>,
    prefix_edges: Vec<Ed>,
    repeat: Option<Body>,
    suffix_states: Vec<St>,
    suffix_edges: Vec<Ed>,
    mode: PathMode,
    path: Option<u16>,
    width: u16,
    start_slot: u16,
    end_slot: u16,
}

/// `(s) -[e]->{min,max} (t)`: slot 0 the start, `e` slot 1 (a group), `t`
/// slot 2, the path slot 3.
fn quantified_line(
    dir: Direction,
    types: Option<Vec<usize>>,
    edge_below: Option<i64>,
    cost: bool,
    min: u32,
    max: Option<u32>,
    mode: PathMode,
    end_labels: Option<Vec<usize>>,
    end_below: Option<i64>,
) -> Line {
    Line {
        prefix_states: vec![at(Some(0))],
        prefix_edges: vec![],
        repeat: Some(Body {
            states: vec![St::default(), St::default()],
            edges: vec![Ed {
                dir,
                types,
                below: edge_below,
                cost,
                bind: Some(1),
            }],
            min,
            max,
        }),
        suffix_states: vec![St {
            labels: end_labels,
            below: end_below,
            bind: Some(2),
        }],
        suffix_edges: vec![],
        mode,
        path: Some(3),
        width: 4,
        start_slot: 0,
        end_slot: 2,
    }
}

/// `(s) ((x)-[:r]->(y)-[:s]->(z)){min,max} (t)`: a two-edge quantified
/// subpath. `y` (slot 1) is a group; the path is slot 3.
fn subpath_line(min: u32, max: Option<u32>, mode: PathMode, bind_mid: bool) -> Line {
    Line {
        prefix_states: vec![at(Some(0))],
        prefix_edges: vec![],
        repeat: Some(Body {
            states: vec![St::default(), at(bind_mid.then_some(1)), St::default()],
            edges: vec![
                plain(Direction::Outgoing, Some(vec![0])),
                plain(Direction::Outgoing, Some(vec![1])),
            ],
            min,
            max,
        }),
        suffix_states: vec![at(Some(2))],
        suffix_edges: vec![],
        mode,
        path: Some(3),
        width: 4,
        start_slot: 0,
        end_slot: 2,
    }
}

impl Line {
    fn automaton(&self, m: &Model, host: &mut TestHost) -> PathAutomaton {
        let node_test = |s: &St, host: &mut TestHost| NodeTest {
            labels: s
                .labels
                .as_ref()
                .map(|l| l.iter().map(|c| m.cols[*c]).collect()),
            filter: s
                .below
                .map(|k| host.expr(E::Below(s.bind.unwrap(), "x", k))),
            bind: s.bind.map(SlotId),
        };
        let edge_step = |e: &Ed, host: &mut TestHost| EdgeStep {
            context: GraphContextId::BASE,
            types: e
                .types
                .as_ref()
                .map(|t| t.iter().map(|t| m.types[*t]).collect()),
            direction: e.dir,
            filter: e
                .below
                .map(|k| host.expr(E::Below(e.bind.unwrap(), "w", k))),
            bind: e.bind.map(SlotId),
        };
        let mut states = Vec::new();
        let mut links = Vec::new();
        let mut groups: Vec<(SlotId, ValueType)> = Vec::new();
        for s in &self.prefix_states {
            states.push(node_test(s, host));
        }
        for e in &self.prefix_edges {
            links.push(PathLink::Edge(edge_step(e, host)));
        }
        let repeat = self.repeat.as_ref().map(|body| {
            let first = states.len() as u16;
            links.push(PathLink::Same);
            for s in &body.states {
                if let Some(slot) = s.bind {
                    groups.push((SlotId(slot), ValueType::Node([].into())));
                }
                states.push(node_test(s, host));
            }
            for e in &body.edges {
                if let Some(slot) = e.bind {
                    groups.push((SlotId(slot), ValueType::Edge([].into())));
                }
                links.push(PathLink::Edge(edge_step(e, host)));
            }
            let last = states.len() as u16 - 1;
            links.push(PathLink::Same);
            Repeat {
                first,
                last,
                min: body.min,
                max: body.max,
            }
        });
        for s in &self.suffix_states {
            states.push(node_test(s, host));
        }
        for e in &self.suffix_edges {
            links.push(PathLink::Edge(edge_step(e, host)));
        }
        PathAutomaton {
            states: states.into(),
            links: links.into(),
            repeats: repeat.into_iter().collect::<Vec<_>>().into(),
            mode: self.mode,
            path: self.path.map(SlotId),
            groups: groups.into(),
        }
    }

    fn plan(&self, m: &Model, host: &mut TestHost, sel: Sel) -> OpSpec {
        let automaton = self.automaton(m, host);
        let search = match sel {
            Sel::Enumerate => PathSearch::Enumerate,
            Sel::Any => PathSearch::Any,
            Sel::Shortest => PathSearch::Shortest,
            Sel::Cheapest => PathSearch::Cheapest {
                cost: host.cost(self.group_edge_slot()),
            },
        };
        let seed = Box::new(OpSpec::Seed {
            input: Box::new(OpSpec::Unit { width: self.width }),
            out: SlotId(self.start_slot),
            source: SeedSource::Scan {
                labels: m.cols.clone().into(),
            },
        });
        let search_op = Box::new(OpSpec::PathSearch {
            input: seed,
            from: SlotId(self.start_slot),
            automaton,
            search,
        });
        let cols: Vec<ExprId> = (0..self.width).map(|s| host.expr(E::Slot(s))).collect();
        OpSpec::Project {
            input: search_op,
            cols: cols.into(),
            width: self.width,
        }
    }

    /// The one bound edge slot inside the repeat, for reconstructing a
    /// witness path from an engine row (selector checks).
    fn group_edge_slot(&self) -> u16 {
        self.repeat
            .as_ref()
            .and_then(|b| b.edges.iter().find_map(|e| e.bind))
            .expect("a selector test's pattern binds its repeated edge")
    }

    /// Does any edge of this pattern carry a `COST`? If so, a match always
    /// has a cost, even a zero-edge one (cost 0, the empty sum) -- COST is
    /// evaluated per relaxation, and a match with no relaxations relaxes
    /// nothing, so its cost is the identity of `+`.
    fn declares_cost(&self) -> bool {
        self.prefix_edges.iter().any(|e| e.cost)
            || self
                .repeat
                .as_ref()
                .is_some_and(|b| b.edges.iter().any(|e| e.cost))
            || self.suffix_edges.iter().any(|e| e.cost)
    }
}

// ── running the engine ──────────────────────────────────────────────────

/// Which search the plan runs, kept apart from `PathSearch` so a `Cheapest`
/// COST expression can be built on the SAME host the rest of the plan
/// uses (see `Line::plan`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sel {
    Enumerate,
    Any,
    Shortest,
    Cheapest,
}

fn never() -> bool {
    false
}

fn run(m: &Model, host: &TestHost, plan: &OpSpec) -> Vec<Vec<BindingValue>> {
    let mut cursor = GqlCursor::open(&m.db, host, plan, Vec::new()).unwrap();
    let mut rows = Vec::new();
    loop {
        let page = cursor.next_page(64, GqlBudget::unlimited(), never).unwrap();
        rows.extend(page.rows.into_iter().map(|r| r.slots.into_vec()));
        if page.done {
            return rows;
        }
    }
}

fn engine_rows(m: &Model, line: &Line, sel: Sel) -> Vec<Vec<BindingValue>> {
    let mut host = TestHost::default();
    let plan = line.plan(m, &mut host, sel);
    run(m, &host, &plan)
}

/// Like `engine_rows`, but also returns the response to the FIRST page,
/// formatted, for tests that expect a refusal (`InvalidPathCost`).
fn engine_first_page(m: &Model, line: &Line, sel: Sel) -> String {
    let mut host = TestHost::default();
    let plan = line.plan(m, &mut host, sel);
    let mut cursor = GqlCursor::open(&m.db, &host, &plan, Vec::new()).unwrap();
    format!("{:?}", cursor.next_page(64, GqlBudget::unlimited(), never))
}

// ═══════════════════════════════════════════════════════════════════════
// THE ORACLE: independent of the engine's search entirely.
// ═══════════════════════════════════════════════════════════════════════

/// A real walk in the graph: `nodes[0]` the start, `edges[i]` the `i`th hop
/// (an index into `Model::edges`, and whether it was crossed source to
/// destination).
#[derive(Clone, Debug)]
struct Walk {
    nodes: Vec<EntityId>,
    edges: Vec<(usize, bool)>,
}

/// Every walk from `start`, up to `max_hops` edges, that a path of `mode`
/// may take -- EVERY prefix length is recorded, not only the longest. Mode
/// is the only pattern-independent constraint, so it is the only thing
/// checked here; direction, labels, types and predicates are the
/// pattern's business (`try_match`).
fn raw_walks(m: &Model, start: EntityId, max_hops: usize, mode: PathMode) -> Vec<Walk> {
    let mut out = Vec::new();
    let mut nodes = vec![start];
    let mut edges: Vec<(usize, bool)> = Vec::new();
    extend_walk(m, &mut nodes, &mut edges, max_hops, mode, &mut out);
    out
}

fn extend_walk(
    m: &Model,
    nodes: &mut Vec<EntityId>,
    edges: &mut Vec<(usize, bool)>,
    remaining: usize,
    mode: PathMode,
    out: &mut Vec<Walk>,
) {
    out.push(Walk {
        nodes: nodes.clone(),
        edges: edges.clone(),
    });
    if remaining == 0 {
        return;
    }
    let at = *nodes.last().unwrap();
    for (idx, e) in m.edges.iter().enumerate() {
        let mut candidates = Vec::new();
        if e.key.source == at {
            candidates.push((e.key.destination, true));
        }
        if e.key.destination == at {
            candidates.push((e.key.source, false));
        }
        for (far, forward) in candidates {
            if mode == PathMode::Trail && edges.iter().any(|&(i, _)| i == idx) {
                continue;
            }
            if mode == PathMode::Acyclic && nodes.contains(&far) {
                continue;
            }
            nodes.push(far);
            edges.push((idx, forward));
            extend_walk(m, nodes, edges, remaining - 1, mode, out);
            edges.pop();
            nodes.pop();
        }
    }
}

fn node_val(id: EntityId) -> BindingValue {
    BindingValue::Node(NodeRef(id))
}

fn edge_val(e: &MEdge) -> BindingValue {
    BindingValue::Edge(EdgeRef {
        key: e.key,
        id: e.id,
        bag: None,
    })
}

fn bind_singleton(row: &mut [BindingValue], slot: u16, val: BindingValue) -> bool {
    match &row[slot as usize] {
        BindingValue::Null => {
            row[slot as usize] = val;
            true
        }
        existing => existing.identity_eq(&val) == Some(true),
    }
}

fn check_node(
    m: &Model,
    spec: &St,
    at: EntityId,
    lists: Option<&mut HashMap<u16, Vec<BindingValue>>>,
    row: &mut [BindingValue],
) -> bool {
    if let Some(labels) = &spec.labels {
        if !labels.iter().any(|c| m.cols[*c] == at.collection) {
            return false;
        }
    }
    if let Some(k) = spec.below {
        if !m.node(at).x.is_some_and(|x| x < k) {
            return false;
        }
    }
    if let Some(slot) = spec.bind {
        let val = node_val(at);
        match lists {
            Some(lists) => lists.entry(slot).or_default().push(val),
            None => {
                if !bind_singleton(row, slot, val) {
                    return false;
                }
            }
        }
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn check_edge(
    m: &Model,
    spec: &Ed,
    walk: &Walk,
    ei: usize,
    lists: Option<&mut HashMap<u16, Vec<BindingValue>>>,
    row: &mut [BindingValue],
    cost: &mut f64,
) -> bool {
    let (idx, forward) = walk.edges[ei];
    let edge = &m.edges[idx];
    let self_loop = edge.key.source == edge.key.destination;
    let ok_dir = match spec.dir {
        Direction::Outgoing => forward,
        Direction::Incoming => !forward,
        Direction::Both => forward || !self_loop,
    };
    if !ok_dir {
        return false;
    }
    if let Some(types) = &spec.types {
        if !types.iter().any(|t| m.types[*t] == edge.key.edge_type) {
            return false;
        }
    }
    if let Some(k) = spec.below {
        if !edge.w.is_some_and(|w| w < k) {
            return false;
        }
    }
    if spec.cost {
        *cost += edge
            .w
            .expect("an oracle CHEAPEST test always uses positive weights") as f64;
    }
    if let Some(slot) = spec.bind {
        let val = edge_val(edge);
        match lists {
            Some(lists) => lists.entry(slot).or_default().push(val),
            None => {
                if !bind_singleton(row, slot, val) {
                    return false;
                }
            }
        }
    }
    true
}

fn walk_path(m: &Model, walk: &Walk) -> PathRef {
    let mut path = PathRef::new(NodeRef(walk.nodes[0]));
    for (i, &(idx, forward)) in walk.edges.iter().enumerate() {
        let edge = &m.edges[idx];
        path = path.extend(
            EdgeRef {
                key: edge.key,
                id: edge.id,
                bag: None,
            },
            forward,
            NodeRef(walk.nodes[i + 1]),
        );
    }
    path
}

/// Does `line` explain `walk`, and if so, with which bindings and at what
/// cost (if `line`'s repeat carries a `COST`)? Because `Line` has at most
/// one repeat, the iteration count a walk of a given length would need is
/// unique -- there is exactly one candidate to try, never several to
/// choose among (unlike an automaton with two or more repeats, where the
/// SAME walk can split between them in more than one way; that ambiguity
/// -- design Q11 -- is covered separately, by a hand-built two-repeat
/// automaton, not through `Line`).
fn try_match(m: &Model, line: &Line, walk: &Walk) -> Option<(Vec<BindingValue>, Option<f64>)> {
    let prefix_edges = line.prefix_edges.len();
    let suffix_edges = line.suffix_edges.len();
    let fixed = prefix_edges + suffix_edges;
    let total = walk.edges.len();
    if total < fixed {
        return None;
    }
    let remaining = total - fixed;
    let k = match &line.repeat {
        Some(body) => {
            let per = body.edges.len();
            if per == 0 || remaining % per != 0 {
                return None;
            }
            let k = remaining / per;
            if k < body.min as usize || body.max.is_some_and(|mx| k > mx as usize) {
                return None;
            }
            k
        }
        None => {
            if remaining != 0 {
                return None;
            }
            0
        }
    };

    let mut row = vec![BindingValue::Null; line.width as usize];
    let mut lists: HashMap<u16, Vec<BindingValue>> = HashMap::new();
    let mut kinds: HashMap<u16, bool> = HashMap::new(); // slot -> is_node
    if let Some(body) = &line.repeat {
        for s in &body.states {
            if let Some(slot) = s.bind {
                lists.entry(slot).or_default();
                kinds.insert(slot, true);
            }
        }
        for e in &body.edges {
            if let Some(slot) = e.bind {
                lists.entry(slot).or_default();
                kinds.insert(slot, false);
            }
        }
    }
    let mut cost = 0f64;

    if !check_node(m, &line.prefix_states[0], walk.nodes[0], None, &mut row) {
        return None;
    }
    for i in 0..prefix_edges {
        if !check_edge(m, &line.prefix_edges[i], walk, i, None, &mut row, &mut cost) {
            return None;
        }
        if !check_node(
            m,
            &line.prefix_states[i + 1],
            walk.nodes[i + 1],
            None,
            &mut row,
        ) {
            return None;
        }
    }
    let mut ni = line.prefix_states.len() - 1;
    let mut ei = prefix_edges;

    if let Some(body) = &line.repeat {
        for _ in 0..k {
            if !check_node(
                m,
                &body.states[0],
                walk.nodes[ni],
                Some(&mut lists),
                &mut row,
            ) {
                return None;
            }
            for j in 0..body.edges.len() {
                if !check_edge(
                    m,
                    &body.edges[j],
                    walk,
                    ei,
                    Some(&mut lists),
                    &mut row,
                    &mut cost,
                ) {
                    return None;
                }
                ei += 1;
                ni += 1;
                if !check_node(
                    m,
                    &body.states[j + 1],
                    walk.nodes[ni],
                    Some(&mut lists),
                    &mut row,
                ) {
                    return None;
                }
            }
        }
        if !check_node(m, &line.suffix_states[0], walk.nodes[ni], None, &mut row) {
            return None;
        }
        for i in 0..suffix_edges {
            if !check_edge(
                m,
                &line.suffix_edges[i],
                walk,
                ei,
                None,
                &mut row,
                &mut cost,
            ) {
                return None;
            }
            ei += 1;
            ni += 1;
            if !check_node(
                m,
                &line.suffix_states[i + 1],
                walk.nodes[ni],
                None,
                &mut row,
            ) {
                return None;
            }
        }
    }
    debug_assert_eq!(ei, walk.edges.len());
    debug_assert_eq!(ni, walk.nodes.len() - 1);

    for (slot, items) in lists {
        let elem = if kinds[&slot] {
            ValueType::Node([].into())
        } else {
            ValueType::Edge([].into())
        };
        row[slot as usize] = BindingValue::List(ListRef {
            items: items.into(),
            elem,
        });
    }
    if let Some(slot) = line.path {
        row[slot as usize] = BindingValue::Path(walk_path(m, walk));
    }
    Some((row, line.declares_cost().then_some(cost)))
}

struct Match {
    row: Vec<BindingValue>,
    hops: usize,
    cost: Option<f64>,
}

/// Every match of `line`, from every node of `m`, as a scan seed gives the
/// engine. `max_hops` must be at least as large as the longest walk `line`
/// could possibly need; see `max_hops_for`.
fn oracle_matches(m: &Model, line: &Line, max_hops: usize) -> Vec<Match> {
    let mut out = Vec::new();
    for start in &m.nodes {
        for walk in raw_walks(m, start.id, max_hops, line.mode) {
            if let Some((row, cost)) = try_match(m, line, &walk) {
                out.push(Match {
                    row,
                    hops: walk.edges.len(),
                    cost,
                });
            }
        }
    }
    out
}

/// A hop bound no real match of `line` could exceed on `m`.
fn max_hops_for(m: &Model, line: &Line) -> usize {
    let fixed = line.prefix_edges.len() + line.suffix_edges.len();
    match &line.repeat {
        None => fixed,
        Some(body) => {
            // A cap no real match could need on `m`: under WALK a shortest
            // or cheapest witness never revisits a product state, and
            // there are at most `nodes * a small constant` of them for
            // this file's tiny automata; TRAIL and ACYCLIC are bounded by
            // the number of edges or nodes outright. A pattern's OWN
            // quantifier bound is honoured only up to that safety cap, so
            // a loose bound (`{0,32}` on a five-node graph, used by the
            // fixed ANY SHORTEST matrix test) cannot make this file's
            // brute-force walk enumeration explode.
            // A shortest (or cheapest, then shortest) walk never needs to
            // revisit a node, so its hop count is at most `nodes - 1`; a
            // little slack (`+ 2 * body edges`) covers the quantifier's
            // own bound not lining up exactly with that distance.
            let natural_edges = match line.mode {
                PathMode::Walk => m.nodes.len() + 2 * body.edges.len(),
                PathMode::Trail => m.edges.len(),
                PathMode::Acyclic => m.nodes.len().saturating_sub(1),
            };
            let bound_edges = body.max.map(|mx| mx as usize * body.edges.len());
            let cap_edges = bound_edges.map_or(natural_edges, |b| {
                b.min(natural_edges.max(body.edges.len()))
            });
            fixed + cap_edges
        }
    }
}

fn bag(mut rows: Vec<Vec<BindingValue>>) -> Vec<Vec<BindingValue>> {
    rows.sort();
    rows
}

/// The exact-bag check: no selector, so every match is an answer.
fn assert_enumerate_matches_oracle(m: &Model, line: &Line, what: &str) {
    let max_hops = max_hops_for(m, line);
    let engine = bag(engine_rows(m, line, Sel::Enumerate));
    let oracle = bag(oracle_matches(m, line, max_hops)
        .into_iter()
        .map(|x| x.row)
        .collect());
    assert_eq!(engine.len(), oracle.len(), "{what}: row count differs");
    assert_eq!(engine, oracle, "{what}");
}

/// The property check for a selector: one row per (start, end), of minimal
/// length (ANY, ANY SHORTEST) or minimal cost then length (ANY CHEAPEST),
/// and every row is a genuine match -- re-verified through `try_match`,
/// independently of the engine's own bookkeeping.
fn assert_selector_matches_oracle(m: &Model, line: &Line, sel: Sel, what: &str) {
    let max_hops = max_hops_for(m, line);
    let cheapest = sel == Sel::Cheapest;
    let mut best: HashMap<(BindingValue, BindingValue), (usize, Option<f64>)> = HashMap::new();
    for mtc in oracle_matches(m, line, max_hops) {
        let key = (
            mtc.row[line.start_slot as usize].clone(),
            mtc.row[line.end_slot as usize].clone(),
        );
        let cand = (mtc.hops, mtc.cost);
        best.entry(key)
            .and_modify(|cur| {
                if better(cand, *cur) {
                    *cur = cand;
                }
            })
            .or_insert(cand);
    }

    let engine = engine_rows(m, line, sel);
    let mut seen: HashMap<(BindingValue, BindingValue), Vec<BindingValue>> = HashMap::new();
    for row in &engine {
        let key = (
            row[line.start_slot as usize].clone(),
            row[line.end_slot as usize].clone(),
        );
        assert!(
            seen.insert(key.clone(), row.clone()).is_none(),
            "{what}: more than one row for the group {key:?}"
        );
        // The row must be a genuine match: reconstruct its walk from the
        // group edge list and the start node, then ask the independent
        // oracle matcher, which knows nothing about how the engine found
        // it.
        let walk = reconstruct_walk(m, line, row);
        let (reproduced, cost) = try_match(m, line, &walk)
            .unwrap_or_else(|| panic!("{what}: the engine's own witness for {key:?} is not a match the oracle recognises: {walk:?}"));
        assert_eq!(
            reproduced[line.end_slot as usize], row[line.end_slot as usize],
            "{what}: reconstructed witness ends elsewhere"
        );
        let want = best[&key];
        assert_eq!(
            walk.edges.len(),
            want.0,
            "{what}: {key:?} witness has {} hops, the oracle's minimum is {}",
            walk.edges.len(),
            want.0
        );
        if cheapest {
            let got_cost =
                cost.unwrap_or_else(|| panic!("{what}: {key:?} witness carries no cost"));
            let want_cost = want.1.unwrap();
            assert!(
                (got_cost - want_cost).abs() < 1e-9,
                "{what}: {key:?} witness costs {got_cost}, the oracle's minimum is {want_cost}"
            );
        }
    }
    for key in best.keys() {
        assert!(
            seen.contains_key(key),
            "{what}: the engine has no row for the reachable group {key:?}"
        );
    }
    for key in seen.keys() {
        assert!(
            best.contains_key(key),
            "{what}: the engine answered for an unreachable group {key:?}"
        );
    }
}

fn better(a: (usize, Option<f64>), b: (usize, Option<f64>)) -> bool {
    match (a.1, b.1) {
        (Some(ca), Some(cb)) => ca < cb || (ca == cb && a.0 < b.0),
        _ => a.0 < b.0,
    }
}

/// Rebuilds the walk an engine row's `p`ath/edge-group implies, so it can
/// be handed to the independent `try_match` for a from-scratch check.
fn reconstruct_walk(m: &Model, line: &Line, row: &[BindingValue]) -> Walk {
    let start_slot = line.start_slot;
    let edge_slot = line.group_edge_slot();
    // Family A's one repeated edge step, whose own `Direction` breaks the
    // self-loop tie below (node identity alone cannot: both endpoints are
    // the same node either way).
    let dir = line
        .repeat
        .as_ref()
        .expect("a selector test's pattern has one quantified edge")
        .edges[0]
        .dir;
    let start = match &row[start_slot as usize] {
        BindingValue::Node(NodeRef(id)) => *id,
        other => panic!("slot {start_slot} held {other:?}, not a node"),
    };
    let refs: Vec<EdgeRef> = match &row[edge_slot as usize] {
        BindingValue::List(l) => l
            .items
            .iter()
            .map(|v| match v {
                BindingValue::Edge(e) => e.clone(),
                other => panic!("a group edge slot held {other:?}"),
            })
            .collect(),
        other => panic!("slot {edge_slot} held {other:?}, not a list"),
    };
    let mut nodes = vec![start];
    let mut edges = Vec::new();
    let mut at = start;
    for e in refs {
        let idx = m
            .edges
            .iter()
            .position(|me| me.id == e.id)
            .expect("the engine returned an edge id the model never made");
        let me = &m.edges[idx];
        let self_loop = me.key.source == me.key.destination;
        let forward = if self_loop {
            // `Both` crosses a self-loop once, forward (design §4.2); a
            // plain `Incoming` step crosses it as incoming.
            !matches!(dir, Direction::Incoming)
        } else if me.key.source == at {
            true
        } else if me.key.destination == at {
            false
        } else {
            panic!("the engine's path does not continue from the current node");
        };
        at = if forward {
            me.key.destination
        } else {
            me.key.source
        };
        nodes.push(at);
        edges.push((idx, forward));
    }
    Walk { nodes, edges }
}

// ═══════════════════════════════════════════════════════════════════════
// Fixed matrix rows (brief §11 / design §4)
// ═══════════════════════════════════════════════════════════════════════

/// Diamond A->B->D, A->C->D: `{2,2}` gives two matches, under every mode,
/// and ANY keeps one.
#[test]
fn a_diamond_bag_matches_the_oracle_under_every_mode() {
    let dir = tempfile::tempdir().unwrap();
    let m = build(
        dir.path(),
        &[(0, None); 4],
        &[
            (0, 1, 0, None),
            (0, 2, 0, None),
            (1, 3, 0, None),
            (2, 3, 0, None),
        ],
    );
    for mode in [PathMode::Walk, PathMode::Trail, PathMode::Acyclic] {
        let line = quantified_line(
            Direction::Outgoing,
            None,
            None,
            false,
            2,
            Some(2),
            mode,
            None,
            None,
        );
        assert_enumerate_matches_oracle(&m, &line, &format!("diamond, {mode:?}"));
    }
    let any = quantified_line(
        Direction::Outgoing,
        None,
        None,
        false,
        2,
        Some(2),
        PathMode::Walk,
        None,
        None,
    );
    assert_selector_matches_oracle(&m, &any, Sel::Any, "diamond, ANY");
    assert_selector_matches_oracle(&m, &any, Sel::Shortest, "diamond, ANY SHORTEST");

    // Bare rows (no group, no path): the two matches are identical rows,
    // both kept -- a BAG, not a set.
    let bare = Line {
        path: None,
        repeat: any.repeat.clone().map(|mut b| {
            b.edges[0].bind = None;
            b
        }),
        ..any
    };
    let got = engine_rows(&m, &bare, Sel::Enumerate);
    let a = m.nodes[0].id;
    let d = m.nodes[3].id;
    let from_a: Vec<_> = got.into_iter().filter(|r| r[0] == node_val(a)).collect();
    let row = vec![
        node_val(a),
        BindingValue::Null,
        node_val(d),
        BindingValue::Null,
    ];
    assert_eq!(
        from_a,
        vec![row.clone(), row],
        "two identical matches, both kept"
    );
}

/// A->D directly and A->B->D: `{2,2}` must still find A->B->D.
#[test]
fn a_direct_edge_does_not_hide_the_two_hop_route() {
    let dir = tempfile::tempdir().unwrap();
    let m = build(
        dir.path(),
        &[(0, None); 3],
        &[(0, 2, 0, None), (0, 1, 0, None), (1, 2, 0, None)],
    );
    let two = quantified_line(
        Direction::Outgoing,
        None,
        None,
        false,
        2,
        Some(2),
        PathMode::Walk,
        None,
        None,
    );
    assert_enumerate_matches_oracle(&m, &two, "direct plus two-hop {2,2}");
    let one_two = quantified_line(
        Direction::Outgoing,
        None,
        None,
        false,
        1,
        Some(2),
        PathMode::Walk,
        None,
        None,
    );
    assert_enumerate_matches_oracle(&m, &one_two, "direct plus two-hop {1,2}");
}

/// `{0,0}`, `{0,1}` and unbounded quantifiers under a mode that ends: zero
/// iterations bind the end to the start, with an empty group list.
#[test]
fn zero_length_quantifiers_match_the_oracle() {
    let dir = tempfile::tempdir().unwrap();
    let m = build(
        dir.path(),
        &[(0, None); 4],
        &[(0, 1, 0, None), (1, 2, 0, None), (2, 0, 0, None)],
    );
    for (min, max) in [(0, Some(0)), (0, Some(1))] {
        let line = quantified_line(
            Direction::Outgoing,
            None,
            None,
            false,
            min,
            max,
            PathMode::Walk,
            None,
            None,
        );
        assert_enumerate_matches_oracle(&m, &line, &format!("{{{min},{max:?}}}"));
    }
    for mode in [PathMode::Trail, PathMode::Acyclic] {
        let star = quantified_line(
            Direction::Outgoing,
            None,
            None,
            false,
            0,
            None,
            mode,
            None,
            None,
        );
        assert_enumerate_matches_oracle(&m, &star, &format!("* under {mode:?}"));
    }
    // The zero-edge path itself: an edge that is bound gets an empty list.
    let zero = quantified_line(
        Direction::Outgoing,
        None,
        None,
        false,
        0,
        Some(0),
        PathMode::Walk,
        None,
        None,
    );
    let rows = engine_rows(&m, &zero, Sel::Enumerate);
    for r in &rows {
        assert_eq!(r[0], r[2], "zero iterations bind the end to the start");
        assert_eq!(
            r[1],
            BindingValue::List(ListRef {
                items: [].into(),
                elem: ValueType::Edge([].into()),
            })
        );
    }
}

/// TRAIL returns to the start of a cycle (no edge repeats, but the start
/// node may reappear); ACYCLIC never does.
#[test]
fn path_modes_on_a_cycle_match_the_oracle() {
    let dir = tempfile::tempdir().unwrap();
    let m = build(
        dir.path(),
        &[(0, None); 3],
        &[(0, 1, 0, None), (1, 2, 0, None), (2, 0, 0, None)],
    );
    for mode in [PathMode::Trail, PathMode::Acyclic] {
        let plus = quantified_line(
            Direction::Outgoing,
            None,
            None,
            false,
            1,
            None,
            mode,
            None,
            None,
        );
        assert_enumerate_matches_oracle(&m, &plus, &format!("+ on a cycle, {mode:?}"));
    }
    let bounded = quantified_line(
        Direction::Outgoing,
        None,
        None,
        false,
        4,
        Some(5),
        PathMode::Walk,
        None,
        None,
    );
    assert_enumerate_matches_oracle(&m, &bounded, "{4,5} on a cycle, WALK");
}

/// A two-edge quantified subpath repeats as a whole, never a single edge
/// twice.
#[test]
fn a_multi_type_subpath_matches_the_oracle() {
    let dir = tempfile::tempdir().unwrap();
    let m = build(
        dir.path(),
        &[(0, None); 5],
        &[
            (0, 1, 0, None), // r
            (1, 2, 1, None), // s
            (2, 3, 0, None), // r
            (3, 4, 1, None), // s
        ],
    );
    for (min, max) in [(1u32, Some(2u32)), (0, Some(2))] {
        let line = subpath_line(min, max, PathMode::Walk, true);
        assert_enumerate_matches_oracle(&m, &line, &format!("subpath {{{min},{max:?}}}"));
    }
}

/// A group predicate prunes each iteration on a random-ish weighted graph.
#[test]
fn a_group_predicate_prunes_each_iteration() {
    let dir = tempfile::tempdir().unwrap();
    // a->b(1), b->d(3), a->c(2), c->d(9): below 5 keeps only the b route.
    let m = build(
        dir.path(),
        &[(0, None); 4],
        &[
            (0, 1, 0, Some(1)),
            (1, 3, 0, Some(3)),
            (0, 2, 0, Some(2)),
            (2, 3, 0, Some(9)),
        ],
    );
    let line = quantified_line(
        Direction::Outgoing,
        None,
        Some(5),
        false,
        2,
        Some(2),
        PathMode::Walk,
        None,
        None,
    );
    assert_enumerate_matches_oracle(&m, &line, "group predicate w < 5");
}

/// ANY SHORTEST: fewest hops, once per end, on a fixed graph with two
/// routes of different length to the same node.
#[test]
fn any_shortest_property_holds_on_a_fixed_graph() {
    let dir = tempfile::tempdir().unwrap();
    // h0->h1->h9 (two hops) and h0->k1->k2->h9 (three hops).
    let m = build(
        dir.path(),
        &[(0, None); 5],
        &[
            (0, 1, 0, None),
            (1, 4, 0, None),
            (0, 2, 0, None),
            (2, 3, 0, None),
            (3, 4, 0, None),
        ],
    );
    let line = quantified_line(
        Direction::Outgoing,
        None,
        None,
        false,
        0,
        Some(32),
        PathMode::Walk,
        None,
        None,
    );
    assert_selector_matches_oracle(&m, &line, Sel::Shortest, "two routes, ANY SHORTEST");
    assert_selector_matches_oracle(&m, &line, Sel::Any, "two routes, ANY");
}

/// The weighted bounded counterexample (design §4.3, brief §8.3): s->a
/// costs 10, s->b->a costs 2 (one hop more), and a->c->t costs 2 more. The
/// PRODUCT state keeps `(a, k=1, cost 10)` and `(a, k=2, cost 2)` apart, so
/// within three hops only the costlier arrival can finish in time, giving
/// cost 12, not 4.
#[test]
fn any_cheapest_weighted_counterexample_matches_the_oracle() {
    let dir = tempfile::tempdir().unwrap();
    let m = build(
        dir.path(),
        &[(0, None); 5],
        &[
            (0, 1, 0, Some(10)), // s -> a, 10
            (0, 2, 0, Some(1)),  // s -> b, 1
            (2, 1, 0, Some(1)),  // b -> a, 1
            (1, 3, 0, Some(1)),  // a -> c, 1
            (3, 4, 0, Some(1)),  // c -> t, 1
        ],
    );
    let (s, t) = (m.nodes[0].id, m.nodes[4].id);
    let within = |max: u32| -> (Line, Vec<Vec<BindingValue>>) {
        let line = quantified_line(
            Direction::Outgoing,
            None,
            None,
            true,
            1,
            Some(max),
            PathMode::Walk,
            None,
            None,
        );
        let rows = engine_rows(&m, &line, Sel::Cheapest);
        (line, rows)
    };
    let (bounded3, rows3) = within(3);
    assert!(
        rows3
            .iter()
            .any(|r| r[0] == node_val(s) && r[2] == node_val(t)),
        "t must be reachable from s within 3 hops, at cost 12"
    );
    assert_selector_matches_oracle(&m, &bounded3, Sel::Cheapest, "counterexample within 3 hops");

    let (bounded2, rows2) = within(2);
    assert!(
        !rows2
            .iter()
            .any(|r| r[0] == node_val(s) && r[2] == node_val(t)),
        "t is not reachable from s within two hops"
    );
    assert_selector_matches_oracle(&m, &bounded2, Sel::Cheapest, "counterexample within 2 hops");

    // Widen the bound: the cheaper 4-hop route (cost 4) becomes reachable
    // and wins over the 3-hop route (cost 12).
    let (bounded4, _) = within(4);
    assert_selector_matches_oracle(&m, &bounded4, Sel::Cheapest, "counterexample within 4 hops");
}

/// A hand-built two-repeat automaton (design §4.2 "ambiguous automata",
/// Q11): `(a)-[]->{0,1}(b)-[]->{0,1}(c)`. With one real edge a->c, the walk
/// can be explained as (k1=1, k2=0) or (k1=0, k2=1); if `b` is unbound both
/// give the SAME row (deduplicated to one match); if `b` IS bound, the two
/// interpretations bind it to different nodes (a vs c), so they are two
/// distinct matches, per Q11.
#[test]
fn an_ambiguous_two_repeat_automaton_dedups_only_identical_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let m = build(dir.path(), &[(0, None); 2], &[(0, 1, 0, None)]);
    let automaton_with = |bind_b: bool| PathAutomaton {
        states: vec![
            NodeTest {
                labels: None,
                filter: None,
                bind: Some(SlotId(0)),
            },
            NodeTest::default(),
            NodeTest::default(),
            NodeTest {
                labels: None,
                filter: None,
                bind: bind_b.then_some(SlotId(1)),
            },
            NodeTest::default(),
            NodeTest::default(),
            NodeTest {
                labels: None,
                filter: None,
                bind: Some(SlotId(2)),
            },
        ]
        .into(),
        links: vec![
            PathLink::Same,
            PathLink::Edge(EdgeStep {
                context: GraphContextId::BASE,
                types: None,
                direction: Direction::Outgoing,
                filter: None,
                bind: None,
            }),
            PathLink::Same,
            PathLink::Same,
            PathLink::Edge(EdgeStep {
                context: GraphContextId::BASE,
                types: None,
                direction: Direction::Outgoing,
                filter: None,
                bind: None,
            }),
            PathLink::Same,
        ]
        .into(),
        repeats: vec![
            Repeat {
                first: 1,
                last: 2,
                min: 0,
                max: Some(1),
            },
            Repeat {
                first: 4,
                last: 5,
                min: 0,
                max: Some(1),
            },
        ]
        .into(),
        mode: PathMode::Walk,
        path: None,
        groups: [].into(),
    };
    let width = 3u16;
    let plan_of = |automaton: PathAutomaton, m: &Model, host: &mut TestHost| -> OpSpec {
        let seed = Box::new(OpSpec::Seed {
            input: Box::new(OpSpec::Unit { width }),
            out: SlotId(0),
            source: SeedSource::Scan {
                labels: m.cols.clone().into(),
            },
        });
        let search = Box::new(OpSpec::PathSearch {
            input: seed,
            from: SlotId(0),
            automaton,
            search: PathSearch::Enumerate,
        });
        let cols: Vec<ExprId> = (0..width).map(|s| host.expr(E::Slot(s))).collect();
        OpSpec::Project {
            input: search,
            cols: cols.into(),
            width,
        }
    };

    let (a, c) = (m.nodes[0].id, m.nodes[1].id);

    // `b` unbound: the zero-edge match (k1=0, k2=0, since both repeats
    // admit zero iterations) and the one-edge match are two rows; the
    // one-edge match's two interpretations (k1=1,k2=0 and k1=0,k2=1) are
    // identical once `b` is not observable, so they dedup to one (Q11).
    let mut host = TestHost::default();
    let plan = plan_of(automaton_with(false), &m, &mut host);
    let rows = run(&m, &host, &plan);
    let mut from_a: Vec<_> = rows.into_iter().filter(|r| r[0] == node_val(a)).collect();
    from_a.sort();
    let mut want = vec![
        vec![node_val(a), BindingValue::Null, node_val(a)], // zero edges
        vec![node_val(a), BindingValue::Null, node_val(c)], // one edge, deduplicated
    ];
    want.sort();
    assert_eq!(
        from_a, want,
        "identical bindings from the same path must be deduplicated (Q11)"
    );

    // `b` bound: the zero-edge match binds `b = a`; the one-edge match's
    // two interpretations bind `b` differently (`b = c` via k1=1,k2=0,
    // `b = a` via k1=0,k2=1), so both are kept as distinct matches (Q11).
    let mut host = TestHost::default();
    let plan = plan_of(automaton_with(true), &m, &mut host);
    let rows = run(&m, &host, &plan);
    let mut from_a: Vec<_> = rows.into_iter().filter(|r| r[0] == node_val(a)).collect();
    from_a.sort();
    let mut want = vec![
        vec![node_val(a), node_val(a), node_val(a)], // zero edges
        vec![node_val(a), node_val(c), node_val(c)], // one edge, via repeat 1
        vec![node_val(a), node_val(a), node_val(c)], // one edge, via repeat 2
    ];
    want.sort();
    assert_eq!(
        from_a, want,
        "different bindings from the same path must both be kept (Q11)"
    );
}

/// `ANY CHEAPEST` refuses a missing (Null) cost -- an edge with no `w`.
/// Zero, negative, NaN and infinite costs are exhaustively pinned by M4-C
/// (`gql_paths_shortest.rs::invalid_cost_values_raise_invalid_path_cost`);
/// this file's job is the BAG, so one representative case is enough to
/// confirm the plan this oracle builds still raises the documented error.
#[test]
fn any_cheapest_refuses_an_invalid_cost() {
    let dir = tempfile::tempdir().unwrap();
    let m = build(dir.path(), &[(0, None); 2], &[(0, 1, 0, None)]);
    let line = quantified_line(
        Direction::Outgoing,
        None,
        None,
        true,
        1,
        Some(1),
        PathMode::Walk,
        None,
        None,
    );
    let first = engine_first_page(&m, &line, Sel::Cheapest);
    assert!(first.contains("InvalidPathCost"), "{first}");
}

// ═══════════════════════════════════════════════════════════════════════
// Seeded random small multigraphs: every mode, every selector.
// ═══════════════════════════════════════════════════════════════════════

fn random_direction(rng: &mut Rng) -> Direction {
    match rng.below(3) {
        0 => Direction::Outgoing,
        1 => Direction::Incoming,
        _ => Direction::Both,
    }
}

fn random_types(rng: &mut Rng) -> Option<Vec<usize>> {
    if rng.below(2) == 0 {
        None
    } else {
        Some(vec![rng.below(2) as usize])
    }
}

/// A modestly bounded quantifier: `min` is 0 or 1, `max` is `min` plus one
/// or two more. Kept small and always bounded so the oracle's exhaustive
/// graph walk (`raw_walks`) stays fast; the fixed matrix tests above cover
/// unbounded quantifiers on hand-picked small graphs instead.
fn random_bounds(rng: &mut Rng) -> (u32, u32) {
    let min = rng.below(2) as u32;
    let max = min + 1 + rng.below(2) as u32;
    (min, max)
}

/// One of the two pattern families, with randomised bounds, direction,
/// type and endpoint constraints. `mode` is chosen by the caller, since
/// admission differs between enumeration (WALK needs a bound, which
/// `random_bounds` always gives) and a selector (WALK is the only mode
/// lang would ever emit it with, but the engine accepts TRAIL/ACYCLIC too,
/// M4-C's deviation 4).
/// `subpath`: whether a two-edge quantified subpath is one of the shapes
/// offered. Selector tests keep this false -- their group-edge-slot
/// reconstruction (`Line::group_edge_slot`, `reconstruct_walk`) needs the
/// single quantified edge shape `quantified_line` always gives.
fn random_line(rng: &mut Rng, mode: PathMode, cost: bool, subpath: bool) -> Line {
    let (min, max) = random_bounds(rng);
    if subpath && !cost && rng.below(2) == 0 {
        subpath_line(min, Some(max), mode, rng.below(2) == 0)
    } else {
        let dir = random_direction(rng);
        let types = random_types(rng);
        let edge_below = if cost || rng.below(2) == 0 {
            None
        } else {
            Some(3 + rng.below(6) as i64)
        };
        let end_labels = if rng.below(2) == 0 {
            None
        } else {
            Some(vec![rng.below(2) as usize])
        };
        let end_below = if rng.below(2) == 0 {
            None
        } else {
            Some(3 + rng.below(6) as i64)
        };
        quantified_line(
            dir,
            types,
            edge_below,
            cost,
            min,
            Some(max),
            mode,
            end_labels,
            end_below,
        )
    }
}

fn random_mode(rng: &mut Rng) -> PathMode {
    match rng.below(3) {
        0 => PathMode::Walk,
        1 => PathMode::Trail,
        _ => PathMode::Acyclic,
    }
}

const GRAPHS: u64 = 12;
const PATTERNS: u64 = 6;

#[test]
fn random_graphs_enumerate_matches_the_oracle() {
    for seed in 0..GRAPHS {
        let dir = tempfile::tempdir().unwrap();
        let m = random_graph(dir.path(), 1_000 + seed, false);
        let mut rng = Rng(2_000 + seed);
        for i in 0..PATTERNS {
            let mode = random_mode(&mut rng);
            let line = random_line(&mut rng, mode, false, true);
            assert_enumerate_matches_oracle(
                &m,
                &line,
                &format!("seed {seed} pattern {i}: {line:?}"),
            );
        }
    }
}

#[test]
fn random_graphs_any_and_any_shortest_match_the_oracle() {
    for seed in 0..GRAPHS {
        let dir = tempfile::tempdir().unwrap();
        let m = random_graph(dir.path(), 3_000 + seed, false);
        let mut rng = Rng(4_000 + seed);
        for i in 0..PATTERNS {
            // Mostly WALK, as lang would emit; occasionally TRAIL/ACYCLIC,
            // which the engine also supports under a selector (M4-C
            // deviation 4).
            let mode = if rng.below(4) == 0 {
                random_mode(&mut rng)
            } else {
                PathMode::Walk
            };
            let line = random_line(&mut rng, mode, false, false);
            assert_selector_matches_oracle(
                &m,
                &line,
                Sel::Any,
                &format!("seed {seed} pattern {i} ANY: {line:?}"),
            );
            assert_selector_matches_oracle(
                &m,
                &line,
                Sel::Shortest,
                &format!("seed {seed} pattern {i} ANY SHORTEST: {line:?}"),
            );
        }
    }
}

#[test]
fn random_graphs_any_cheapest_matches_the_oracle() {
    for seed in 0..GRAPHS {
        let dir = tempfile::tempdir().unwrap();
        let m = random_graph(dir.path(), 5_000 + seed, true);
        let mut rng = Rng(6_000 + seed);
        for i in 0..PATTERNS {
            let mode = if rng.below(4) == 0 {
                random_mode(&mut rng)
            } else {
                PathMode::Walk
            };
            let line = random_line(&mut rng, mode, true, false);
            assert_selector_matches_oracle(
                &m,
                &line,
                Sel::Cheapest,
                &format!("seed {seed} pattern {i} ANY CHEAPEST: {line:?}"),
            );
        }
    }
}
