//! Path selectors (`docs/lang/GQL_PROFILE_DESIGN.md` §4.3, §4.7):
//! `OpSpec::PathSearch` with `PathSearch::Any`, `PathSearch::Shortest` (a
//! BFS over product states) and `PathSearch::Cheapest` (Dijkstra over the
//! same states), over hand-built `PathAutomaton`s.
//!
//! What is at risk, and the test that pins it:
//!
//! * that each selector gives ONE witness per (start, end) pair, and that the
//!   witness is a real match of minimum hop count (SHORTEST) or minimum cost,
//!   then minimum hops (CHEAPEST) -- checked against a brute-force reference
//!   that enumerates every match within the bounds and takes the minimum, on
//!   seeded random multigraphs with parallel edges, self-loops and cycles
//!   (`*_matches_the_brute_force_minimum_*`);
//! * the brief's matrix rows: two equal shortest paths give one witness;
//!   the weighted bounded counterexample costs 12, not 4; invalid costs
//!   (zero, negative, null, NaN, infinity) raise `InvalidPathCost`; a
//!   missing seed or a disconnected target gives zero rows;
//! * that the search state is the PRODUCT state: a lower bound past a cycle
//!   and a direct edge beside a two-hop route are found, and an unbounded
//!   WALK is admitted under a selector and terminates;
//! * the zero-edge path, a fixed target (point-to-point early stop), the
//!   documented tie order (frontier order, then posting order) and the
//!   cost ties (fewer hops first, then discovery order);
//! * that `path_states`, `queue_entries`, `predecessor_arcs` and
//!   `binding_rows` refuse by name, that a refusal, an invalid cost or a
//!   cancellation poisons the cursor, that held state is charged again to
//!   each page, and that the pages of one cursor concatenate to the one-shot
//!   answer.

use sekejap_core::collections::gql::{
    BindingRow, BindingValue, EdgeRef, EdgeStep, EvalCx, ExprId, GqlBudget, GqlCursor, GqlHost, GqlWork,
    ListRef, NodeRef, NodeTest, OpSpec, PathAutomaton, PathLink, PathMode, PathRef, PathSearch,
    Repeat, SeedId, SeedSource, SlotId, Truth, ValueType,
};
use sekejap_core::collections::{
    CollectionId, Database, Direction, EdgeKey, EdgeTypeId, EntityId, GraphContextId,
    PreparedQuery, QueryError, QueryResult, WorkResource,
};
use sekejap_core::Kind;
use serde_json::json;
use std::collections::{BTreeMap, HashSet};

mod common;
use common::cfg;

/// splitmix64: reproducible from its seed.
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

// ── the test host ─────────────────────────────────────────────────────────

/// An edge weight that the cost expression turns into NaN.
const W_NAN: i64 = 9001;
/// An edge weight that the cost expression turns into +infinity.
const W_INF: i64 = 9002;

#[derive(Clone, Debug)]
enum E {
    Slot(u16),
    /// `slot.x < k` on a node, `slot.w < k` on an edge; `Null` is unknown.
    Below(u16, &'static str, i64),
    /// The COST of the edge in the slot: its `w`, as an `Int` (`W_NAN` and
    /// `W_INF` as those floats; a missing `w` is `Null`).
    Cost(u16),
    /// `w / 2` as a `Float`.
    Half(u16),
    Text(&'static str),
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
}

fn edge_w(row: &BindingRow, slot: u16, cx: &mut EvalCx<'_, '_>) -> QueryResult<BindingValue> {
    match row.get(SlotId(slot)) {
        BindingValue::Edge(e) => cx.reader.edge_property(e, "w", cx.meter),
        other => panic!("a cost saw {other:?}, not an edge"),
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
            E::Text(t) => BindingValue::Text((*t).into()),
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
            E::Cost(s) => match edge_w(row, *s, cx)? {
                BindingValue::Int(W_NAN) => BindingValue::Float(f64::NAN),
                BindingValue::Int(W_INF) => BindingValue::Float(f64::INFINITY),
                other => other,
            },
            E::Half(s) => match edge_w(row, *s, cx)? {
                BindingValue::Int(w) => BindingValue::Float(w as f64 / 2.0),
                other => other,
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
        unreachable!("these plans seed by scan or key")
    }
}

// ── graphs and their in-memory model ──────────────────────────────────────

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

    fn id(&self, i: usize) -> EntityId {
        self.nodes[i].id
    }
}

/// A graph of `nodes` (collection index, `x`) and `edges` (source index,
/// destination index, type index, `w`), in collections `site_a`, `site_b`
/// and types `r`, `s`. Node `i` has key `n<i>`.
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

/// Every node in `site_a`, edges of type `r` with weight `w`.
fn weighted(dir: &std::path::Path, nodes: usize, edges: &[(usize, usize, i64)]) -> Model {
    let es: Vec<_> = edges.iter().map(|&(s, d, w)| (s, d, 0, Some(w))).collect();
    build(dir, &vec![(0, None); nodes], &es)
}

/// A random multigraph: parallel twins and self-loops forced in. With
/// `positive`, every edge weighs 1..=9.
fn random_graph(
    dir: &std::path::Path,
    seed: u64,
    nodes: usize,
    edges: usize,
    positive: bool,
) -> Model {
    let mut rng = Rng(seed);
    let ns: Vec<(usize, Option<i64>)> = (0..nodes)
        .map(|_| {
            (
                rng.below(2) as usize,
                (rng.below(5) != 0).then(|| rng.below(10) as i64),
            )
        })
        .collect();
    let w = |rng: &mut Rng| {
        if positive {
            Some(1 + rng.below(9) as i64)
        } else {
            (rng.below(4) != 0).then(|| rng.below(10) as i64)
        }
    };
    let mut es = Vec::new();
    while es.len() < edges {
        let (s, d) = (
            rng.below(nodes as u64) as usize,
            rng.below(nodes as u64) as usize,
        );
        let t = rng.below(2) as usize;
        es.push((s, d, t, w(&mut rng)));
        if rng.below(6) == 0 {
            es.push((s, d, t, w(&mut rng)));
        }
        if rng.below(8) == 0 {
            es.push((s, s, t, w(&mut rng)));
        }
    }
    build(dir, &ns, &es)
}

// ── patterns, as the test writes them ─────────────────────────────────────

#[derive(Clone, Debug, Default)]
struct TNode {
    labels: Option<Vec<usize>>,
    below: Option<i64>,
    bind: Option<u16>,
}

#[derive(Clone, Debug)]
struct TEdge {
    direction: Direction,
    types: Option<Vec<usize>>,
    below: Option<i64>,
    bind: Option<u16>,
}

#[derive(Clone, Debug)]
enum TLink {
    Same,
    Edge(TEdge),
}

#[derive(Clone, Debug)]
struct TPat {
    states: Vec<TNode>,
    links: Vec<TLink>,
    repeats: Vec<Repeat>,
    mode: PathMode,
    path: Option<u16>,
    width: u16,
}

fn out(bind: Option<u16>) -> TEdge {
    TEdge {
        direction: Direction::Outgoing,
        types: None,
        below: None,
        bind,
    }
}

fn at(bind: Option<u16>) -> TNode {
    TNode {
        bind,
        ..TNode::default()
    }
}

/// `(s) -[e]-> {min,max} (t)`: slot 0 the start, `e` slot 1 (a group), `t`
/// slot 2, the path slot 3.
fn quantified_edge(edge: TEdge, min: u32, max: Option<u32>, mode: PathMode) -> TPat {
    TPat {
        states: vec![at(Some(0)), at(None), at(None), at(Some(2))],
        links: vec![TLink::Same, TLink::Edge(edge), TLink::Same],
        repeats: vec![Repeat {
            first: 1,
            last: 2,
            min,
            max,
        }],
        mode,
        path: Some(3),
        width: 4,
    }
}

/// How the test names a search; `Cheapest` costs the edge in slot 1.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Sel {
    Any,
    Shortest,
    Cheapest,
    /// Cheapest, with the cost `w / 2` as a float.
    CheapestHalf,
}

/// Where the search starts: every node (a scan), or the node with a key.
#[derive(Clone, Copy, Debug)]
enum Start {
    Scan,
    Key(&'static str),
}

impl TPat {
    fn state_in_repeat(&self, s: usize) -> bool {
        self.repeats
            .iter()
            .any(|r| usize::from(r.first) <= s && s <= usize::from(r.last))
    }

    fn link_in_repeat(&self, l: usize) -> bool {
        self.repeats
            .iter()
            .any(|r| usize::from(r.first) <= l && l < usize::from(r.last))
    }

    fn groups(&self) -> Vec<(u16, bool)> {
        let mut groups = Vec::new();
        for (i, s) in self.states.iter().enumerate() {
            if let (Some(b), true) = (s.bind, self.state_in_repeat(i)) {
                groups.push((b, true));
            }
        }
        for (i, l) in self.links.iter().enumerate() {
            if let (TLink::Edge(e), true) = (l, self.link_in_repeat(i)) {
                if let Some(b) = e.bind {
                    groups.push((b, false));
                }
            }
        }
        groups
    }

    /// The slot of the one edge step (what the COST reads).
    fn edge_slot(&self) -> u16 {
        self.links
            .iter()
            .find_map(|l| match l {
                TLink::Edge(e) => e.bind,
                TLink::Same => None,
            })
            .expect("a cheapest pattern binds its edge")
    }

    fn automaton(&self, m: &Model, host: &mut TestHost) -> PathAutomaton {
        let states = self
            .states
            .iter()
            .map(|s| NodeTest {
                labels: s
                    .labels
                    .as_ref()
                    .map(|l| l.iter().map(|c| m.cols[*c]).collect()),
                filter: s
                    .below
                    .map(|k| host.expr(E::Below(s.bind.unwrap(), "x", k))),
                bind: s.bind.map(SlotId),
            })
            .collect();
        let links = self
            .links
            .iter()
            .map(|l| match l {
                TLink::Same => PathLink::Same,
                TLink::Edge(e) => PathLink::Edge(EdgeStep {
                    context: GraphContextId::BASE,
                    types: e
                        .types
                        .as_ref()
                        .map(|t| t.iter().map(|t| m.types[*t]).collect()),
                    direction: e.direction,
                    filter: e
                        .below
                        .map(|k| host.expr(E::Below(e.bind.unwrap(), "w", k))),
                    bind: e.bind.map(SlotId),
                }),
            })
            .collect();
        PathAutomaton {
            states,
            links,
            repeats: self.repeats.clone().into(),
            mode: self.mode,
            path: self.path.map(SlotId),
            groups: self
                .groups()
                .into_iter()
                .map(|(slot, node)| {
                    let elem = if node {
                        ValueType::Node([].into())
                    } else {
                        ValueType::Edge([].into())
                    };
                    (SlotId(slot), elem)
                })
                .collect(),
        }
    }

    fn search(&self, sel: Sel, host: &mut TestHost) -> PathSearch {
        match sel {
            Sel::Any => PathSearch::Any,
            Sel::Shortest => PathSearch::Shortest,
            Sel::Cheapest => PathSearch::Cheapest {
                cost: host.expr(E::Cost(self.edge_slot())),
            },
            Sel::CheapestHalf => PathSearch::Cheapest {
                cost: host.expr(E::Half(self.edge_slot())),
            },
        }
    }

    /// `MATCH <start> [<target seeded by key>] <selector> <pattern> RETURN *`.
    fn plan(
        &self,
        m: &Model,
        host: &mut TestHost,
        sel: Sel,
        start: Start,
        target: Option<&'static str>,
    ) -> OpSpec {
        let automaton = self.automaton(m, host);
        let search = self.search(sel, host);
        let source = match start {
            Start::Scan => SeedSource::Scan {
                labels: m.cols.clone().into(),
            },
            Start::Key(k) => SeedSource::Key {
                key: host.expr(E::Text(k)),
                labels: m.cols.clone().into(),
            },
        };
        let mut input = Box::new(OpSpec::Seed {
            input: Box::new(OpSpec::Unit { width: self.width }),
            out: SlotId(0),
            source,
        });
        if let Some(k) = target {
            let slot = self.states.last().unwrap().bind.unwrap();
            input = Box::new(OpSpec::Seed {
                input,
                out: SlotId(slot),
                source: SeedSource::Key {
                    key: host.expr(E::Text(k)),
                    labels: m.cols.clone().into(),
                },
            });
        }
        let search = Box::new(OpSpec::PathSearch {
            input,
            from: SlotId(0),
            automaton,
            search,
        });
        let cols: Vec<ExprId> = (0..self.width).map(|s| host.expr(E::Slot(s))).collect();
        OpSpec::Project {
            input: search,
            cols: cols.into(),
            width: self.width,
        }
    }
}

// ── the brute-force reference: every match, then the minimum ─────────────

#[derive(Clone, Copy, Debug)]
enum Item {
    State(usize),
    Edge(usize),
}

/// The straight pattern the automaton means when repeat `i` runs `n[i]`
/// times: consecutive `State`s are one node.
fn expand(p: &TPat, n: &[u32]) -> Vec<Item> {
    let mut items = Vec::new();
    let last = p.states.len() - 1;
    let mut s = 0;
    loop {
        items.push(Item::State(s));
        if s == last {
            return items;
        }
        if let Some(i) = p.repeats.iter().position(|r| usize::from(r.first) == s + 1) {
            let r = p.repeats[i];
            for _ in 0..n[i] {
                for t in usize::from(r.first)..=usize::from(r.last) {
                    items.push(Item::State(t));
                    if t < usize::from(r.last) {
                        if let TLink::Edge(_) = p.links[t] {
                            items.push(Item::Edge(t));
                        }
                    }
                }
            }
            s = usize::from(r.last) + 1;
            continue;
        }
        if let TLink::Edge(_) = p.links[s] {
            items.push(Item::Edge(s));
        }
        s += 1;
    }
}

fn body_edges(p: &TPat, r: &Repeat) -> usize {
    (usize::from(r.first)..usize::from(r.last))
        .filter(|l| matches!(p.links[*l], TLink::Edge(_)))
        .count()
}

/// Every count vector whose straight pattern crosses at most `longest`
/// edges.
fn counts(p: &TPat, longest: usize) -> Vec<Vec<u32>> {
    let mut all = vec![Vec::new()];
    for r in &p.repeats {
        let cap = r.max.unwrap_or((longest / body_edges(p, r)) as u32);
        let mut next = Vec::new();
        for prefix in &all {
            for n in r.min..=cap.max(r.min) {
                let mut v: Vec<u32> = prefix.clone();
                v.push(n);
                next.push(v);
            }
        }
        all = next;
    }
    all.retain(|n| {
        expand(p, n)
            .iter()
            .filter(|i| matches!(i, Item::Edge(_)))
            .count()
            <= longest
    });
    all
}

/// Every (edge, far node, forward) one hop from `near` admits.
fn hop(m: &Model, near: EntityId, e: &TEdge) -> Vec<(MEdge, EntityId, bool)> {
    let mut hops = Vec::new();
    for edge in &m.edges {
        if e.types
            .as_ref()
            .is_some_and(|t| !t.iter().any(|t| m.types[*t] == edge.key.edge_type))
        {
            continue;
        }
        let (s, d) = (edge.key.source, edge.key.destination);
        let out = matches!(e.direction, Direction::Outgoing | Direction::Both) && s == near;
        let inc = matches!(e.direction, Direction::Incoming | Direction::Both)
            && d == near
            && !(e.direction == Direction::Both && s == d);
        if out {
            hops.push((edge.clone(), d, true));
        }
        if inc {
            hops.push((edge.clone(), s, false));
        }
    }
    hops.retain(|(edge, _, _)| e.below.is_none_or(|k| edge.w.is_some_and(|w| w < k)));
    hops
}

fn node(id: EntityId) -> BindingValue {
    BindingValue::Node(NodeRef(id))
}

fn edge_ref(e: &MEdge) -> EdgeRef {
    EdgeRef {
        key: e.key,
        id: e.id,
        bag: None,
    }
}

fn list(items: Vec<BindingValue>, elem: ValueType) -> BindingValue {
    BindingValue::List(ListRef {
        items: items.into(),
        elem,
    })
}

/// One reference match: the row, its hop count and its cost (the sum of
/// `w`, `None` when an edge has none).
#[derive(Clone, Debug)]
struct Found {
    row: Vec<BindingValue>,
    hops: usize,
    cost: Option<i64>,
}

struct Walk<'a> {
    m: &'a Model,
    p: &'a TPat,
    items: Vec<Item>,
    binds: Vec<(u16, BindingValue, bool)>,
    nodes: Vec<EntityId>,
    edges: Vec<MEdge>,
}

impl Walk<'_> {
    fn go(&mut self, i: usize, at: EntityId, path: PathRef, found: &mut dyn FnMut(&Walk, PathRef)) {
        if i == self.items.len() {
            let ok = match self.p.mode {
                PathMode::Walk => true,
                PathMode::Trail => {
                    let set: HashSet<_> = self.edges.iter().map(|e| e.id).collect();
                    set.len() == self.edges.len()
                }
                PathMode::Acyclic => {
                    let set: HashSet<_> = self
                        .nodes
                        .iter()
                        .map(|n| (n.collection.0, n.sequence))
                        .collect();
                    set.len() == self.nodes.len()
                }
            };
            if ok {
                found(self, path);
            }
            return;
        }
        match self.items[i] {
            Item::State(s) => {
                let t = &self.p.states[s];
                if t.labels
                    .as_ref()
                    .is_some_and(|l| !l.iter().any(|c| self.m.cols[*c] == at.collection))
                {
                    return;
                }
                if let Some(k) = t.below {
                    if !self.m.node(at).x.is_some_and(|x| x < k) {
                        return;
                    }
                }
                if let Some(b) = t.bind {
                    self.binds.push((b, node(at), self.p.state_in_repeat(s)));
                }
                self.go(i + 1, at, path, found);
                if t.bind.is_some() {
                    self.binds.pop();
                }
            }
            Item::Edge(l) => {
                let TLink::Edge(e) = &self.p.links[l] else {
                    unreachable!()
                };
                let e = e.clone();
                for (edge, far, forward) in hop(self.m, at, &e) {
                    if let Some(b) = e.bind {
                        self.binds.push((
                            b,
                            BindingValue::Edge(edge_ref(&edge)),
                            self.p.link_in_repeat(l),
                        ));
                    }
                    self.nodes.push(far);
                    self.edges.push(edge.clone());
                    let next = path.extend(edge_ref(&edge), forward, NodeRef(far));
                    self.go(i + 1, far, next, found);
                    self.nodes.pop();
                    self.edges.pop();
                    if e.bind.is_some() {
                        self.binds.pop();
                    }
                }
            }
        }
    }
}

/// Every match from `start`, within the pattern's bounds (an unbounded
/// repeat only under TRAIL or ACYCLIC, which bound the path by the graph).
fn every_match(m: &Model, p: &TPat, start: EntityId) -> Vec<Found> {
    let longest = match p.mode {
        PathMode::Walk => usize::MAX,
        PathMode::Trail => m.edges.len(),
        PathMode::Acyclic => m.nodes.len() - 1,
    };
    let group_slots = p.groups();
    let mut all = Vec::new();
    for n in counts(p, longest) {
        let mut walk = Walk {
            m,
            p,
            items: expand(p, &n),
            binds: Vec::new(),
            nodes: vec![start],
            edges: Vec::new(),
        };
        let mut found = |w: &Walk, path: PathRef| {
            let mut row = vec![BindingValue::Null; usize::from(p.width)];
            row[0] = node(start);
            let mut assigned = vec![false; row.len()];
            assigned[0] = true;
            for (slot, is_node) in &group_slots {
                let items = w
                    .binds
                    .iter()
                    .filter(|(s, _, g)| *g && s == slot)
                    .map(|(_, v, _)| v.clone())
                    .collect();
                let elem = if *is_node {
                    ValueType::Node([].into())
                } else {
                    ValueType::Edge([].into())
                };
                row[usize::from(*slot)] = list(items, elem);
            }
            for (slot, value, group) in &w.binds {
                if *group {
                    continue;
                }
                let s = usize::from(*slot);
                if assigned[s] {
                    if row[s].identity_eq(value) != Some(true) {
                        return;
                    }
                } else {
                    row[s] = value.clone();
                    assigned[s] = true;
                }
            }
            if let Some(ps) = p.path {
                row[usize::from(ps)] = BindingValue::Path(path);
            }
            all.push(Found {
                row,
                hops: w.edges.len(),
                cost: w.edges.iter().map(|e| e.w).sum(),
            });
        };
        walk.go(0, start, PathRef::new(NodeRef(start)), &mut found);
    }
    all
}

fn end_of(row: &[BindingValue], path_slot: u16) -> EntityId {
    match &row[usize::from(path_slot)] {
        BindingValue::Path(p) => p.end().0,
        other => panic!("no path in the row: {other:?}"),
    }
}

/// Check the engine's rows for pattern `p` from every start against the
/// brute force: one row per reachable end, each a real match, of minimum
/// hops (`Shortest`), or of minimum cost and then minimum hops (`Cheapest`).
/// `Any` asks only for a real match. Returns the rows compared.
fn assert_minimal(m: &Model, p: &TPat, sel: Sel, what: &str) -> usize {
    let ps = p.path.expect("the comparison reads the path");
    let got = run(m, p, sel, Start::Scan, None);
    let mut by_pair: BTreeMap<(EntityId, EntityId), Vec<Vec<BindingValue>>> = BTreeMap::new();
    for row in got {
        let start = match &row[0] {
            BindingValue::Node(n) => n.0,
            other => panic!("{other:?}"),
        };
        by_pair
            .entry((start, end_of(&row, ps)))
            .or_default()
            .push(row);
    }
    let mut want_pairs = 0;
    for start in &m.nodes {
        let mut by_end: BTreeMap<EntityId, Vec<Found>> = BTreeMap::new();
        for f in every_match(m, p, start.id) {
            by_end.entry(end_of(&f.row, ps)).or_default().push(f);
        }
        for (end, found) in by_end {
            want_pairs += 1;
            let rows = by_pair
                .remove(&(start.id, end))
                .unwrap_or_else(|| panic!("{what}: no row to {end:?}, pattern {p:?}"));
            assert_eq!(rows.len(), 1, "{what}: one witness per pair, pattern {p:?}");
            let row = &rows[0];
            let witness = found
                .iter()
                .find(|f| &f.row == row)
                .unwrap_or_else(|| panic!("{what}: the witness {row:?} is not a match of {p:?}"));
            match sel {
                Sel::Any => {}
                Sel::Shortest => {
                    let min = found.iter().map(|f| f.hops).min().unwrap();
                    assert_eq!(witness.hops, min, "{what}: hops, pattern {p:?}");
                }
                Sel::Cheapest | Sel::CheapestHalf => {
                    let min = found.iter().map(|f| f.cost.unwrap()).min().unwrap();
                    let hops = found
                        .iter()
                        .filter(|f| f.cost == Some(min))
                        .map(|f| f.hops)
                        .min()
                        .unwrap();
                    assert_eq!(witness.cost, Some(min), "{what}: cost, pattern {p:?}");
                    assert_eq!(
                        witness.hops, hops,
                        "{what}: hops at min cost, pattern {p:?}"
                    );
                }
            }
        }
    }
    assert!(
        by_pair.is_empty(),
        "{what}: rows for pairs with no match: {by_pair:?}"
    );
    want_pairs
}

// ── running ───────────────────────────────────────────────────────────────

fn never() -> bool {
    false
}

fn run_paged(
    m: &Model,
    host: &TestHost,
    plan: &OpSpec,
    page_rows: usize,
    budget: GqlBudget,
) -> QueryResult<Vec<Vec<BindingValue>>> {
    let mut cursor = GqlCursor::open(&m.db, host, plan, Vec::new())?;
    let mut rows = Vec::new();
    loop {
        let page = cursor.next_page(page_rows, budget, never)?;
        rows.extend(page.rows.into_iter().map(|r| r.slots.into_vec()));
        if page.done {
            return Ok(rows);
        }
    }
}

fn try_run(
    m: &Model,
    p: &TPat,
    sel: Sel,
    start: Start,
    target: Option<&'static str>,
) -> QueryResult<Vec<Vec<BindingValue>>> {
    let mut host = TestHost::default();
    let plan = p.plan(m, &mut host, sel, start, target);
    run_paged(m, &host, &plan, 64, GqlBudget::unlimited())
}

fn run(
    m: &Model,
    p: &TPat,
    sel: Sel,
    start: Start,
    target: Option<&'static str>,
) -> Vec<Vec<BindingValue>> {
    try_run(m, p, sel, start, target).unwrap()
}

fn path_of(m: &Model, start: usize, steps: &[(usize, bool)]) -> BindingValue {
    let mut path = PathRef::new(NodeRef(m.id(start)));
    for &(e, forward) in steps {
        let edge = &m.edges[e];
        let far = if forward {
            edge.key.destination
        } else {
            edge.key.source
        };
        path = path.extend(edge_ref(edge), forward, NodeRef(far));
    }
    BindingValue::Path(path)
}

fn edges_list(m: &Model, es: &[usize]) -> BindingValue {
    list(
        es.iter()
            .map(|e| BindingValue::Edge(edge_ref(&m.edges[*e])))
            .collect(),
        ValueType::Edge([].into()),
    )
}

/// The rows ending at node `end`.
fn to(rows: &[Vec<BindingValue>], end: EntityId) -> Vec<Vec<BindingValue>> {
    rows.iter().filter(|r| r[2] == node(end)).cloned().collect()
}

fn hops(row: &[BindingValue]) -> u32 {
    match &row[3] {
        BindingValue::Path(p) => p.len(),
        other => panic!("{other:?}"),
    }
}

// ── matrix rows ───────────────────────────────────────────────────────────

/// Diamond A->B->D, A->C->D, and a longer A->E->F->D: ANY SHORTEST gives
/// ONE witness to D, of two hops, and the tie goes to the first route in
/// posting order (via B, whose edge was written first). ANY gives one too.
#[test]
fn two_equal_shortest_paths_give_one_witness() {
    let dir = tempfile::tempdir().unwrap();
    let m = build(
        dir.path(),
        &[(0, None); 6],
        &[
            (0, 1, 0, None),
            (0, 2, 0, None),
            (1, 3, 0, None),
            (2, 3, 0, None),
            (0, 4, 0, None),
            (4, 5, 0, None),
            (5, 3, 0, None),
        ],
    );
    let p = quantified_edge(out(Some(1)), 1, Some(5), PathMode::Walk);
    for sel in [Sel::Shortest, Sel::Any] {
        let rows = run(&m, &p, sel, Start::Key("n0"), None);
        // One row per end: B, C, D, E, F.
        assert_eq!(rows.len(), 5, "{sel:?}");
        let d = to(&rows, m.id(3));
        assert_eq!(
            d,
            vec![vec![
                node(m.id(0)),
                edges_list(&m, &[0, 2]),
                node(m.id(3)),
                path_of(&m, 0, &[(0, true), (2, true)]),
            ]],
            "{sel:?}"
        );
        // Repeatable: the same witness every time.
        assert_eq!(run(&m, &p, sel, Start::Key("n0"), None), rows);
    }
    assert_minimal(&m, &p, Sel::Shortest, "diamond");
}

/// Brief §8.3: s->a costs 10; s->b->a costs 2; a->c->t costs 2. Within
/// three hops the only route to t is s->a->c->t, cost 12: the cheaper
/// arrival at `a` has spent two hops and cannot finish. Within four hops
/// the route through b costs 4.
#[test]
fn the_weighted_bounded_counterexample_costs_12() {
    let dir = tempfile::tempdir().unwrap();
    // s=0, a=1, b=2, c=3, t=4.
    let m = weighted(
        dir.path(),
        5,
        &[(0, 1, 10), (0, 2, 1), (2, 1, 1), (1, 3, 1), (3, 4, 1)],
    );
    let bounded = quantified_edge(out(Some(1)), 1, Some(3), PathMode::Walk);
    let rows = run(&m, &bounded, Sel::Cheapest, Start::Key("n0"), Some("n4"));
    assert_eq!(
        rows,
        vec![vec![
            node(m.id(0)),
            edges_list(&m, &[0, 3, 4]),
            node(m.id(4)),
            path_of(&m, 0, &[(0, true), (3, true), (4, true)]),
        ]]
    );
    let four = quantified_edge(out(Some(1)), 1, Some(4), PathMode::Walk);
    let rows = run(&m, &four, Sel::Cheapest, Start::Key("n0"), Some("n4"));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][1], edges_list(&m, &[1, 2, 3, 4]));
    // Every end, from every start, against the brute force.
    assert_minimal(&m, &bounded, Sel::Cheapest, "counterexample");
    assert_minimal(&m, &four, Sel::Cheapest, "counterexample, four hops");
}

/// Zero, negative, null, NaN and infinite costs raise `InvalidPathCost` --
/// never skipped, never clamped -- and poison the cursor. A bad cost on an
/// edge the search never relaxes raises nothing.
#[test]
fn invalid_cost_values_raise_invalid_path_cost() {
    for bad in [Some(0), Some(-3), None, Some(W_NAN), Some(W_INF)] {
        let dir = tempfile::tempdir().unwrap();
        // 0 -> 1 costs 2, 1 -> 2 costs `bad`.
        let m = build(
            dir.path(),
            &[(0, None); 3],
            &[(0, 1, 0, Some(2)), (1, 2, 0, bad)],
        );
        let p = quantified_edge(out(Some(1)), 1, Some(3), PathMode::Walk);
        let mut host = TestHost::default();
        let plan = p.plan(&m, &mut host, Sel::Cheapest, Start::Key("n0"), None);
        let mut cursor = GqlCursor::open(&m.db, &host, &plan, Vec::new()).unwrap();
        let first = format!("{:?}", cursor.next_page(64, GqlBudget::unlimited(), never));
        assert!(first.contains("InvalidPathCost"), "{bad:?}: {first}");
        let again = format!("{:?}", cursor.next_page(64, GqlBudget::unlimited(), never));
        assert_eq!(again, first, "{bad:?}: poisoned");

        // From node 2 the bad edge is never relaxed.
        let rows = run(&m, &p, Sel::Cheapest, Start::Key("n2"), None);
        assert!(rows.is_empty(), "{bad:?}");
    }

    // A positive float cost is valid: 3/2 + 5/2.
    let dir = tempfile::tempdir().unwrap();
    let m = weighted(dir.path(), 3, &[(0, 1, 3), (1, 2, 5), (0, 2, 9)]);
    let p = quantified_edge(out(Some(1)), 1, Some(2), PathMode::Walk);
    let rows = run(&m, &p, Sel::CheapestHalf, Start::Key("n0"), Some("n2"));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][1], edges_list(&m, &[0, 1]));
}

/// A key that finds no start gives no row; a target the start cannot reach
/// gives no row; both pages say `done`.
#[test]
fn a_missing_seed_or_a_disconnected_target_gives_zero_rows() {
    let dir = tempfile::tempdir().unwrap();
    let m = weighted(dir.path(), 4, &[(0, 1, 1), (1, 2, 1)]);
    let p = quantified_edge(out(Some(1)), 0, Some(32), PathMode::Walk);
    for sel in [Sel::Any, Sel::Shortest, Sel::Cheapest] {
        assert!(run(&m, &p, sel, Start::Key("missing"), None).is_empty());
        // Node 3 is isolated.
        assert!(run(&m, &p, sel, Start::Key("n0"), Some("n3")).is_empty());
        assert!(run(&m, &p, sel, Start::Key("n0"), Some("missing")).is_empty());
        // Against the direction of the edges.
        assert!(run(&m, &p, sel, Start::Key("n2"), Some("n0")).is_empty());
    }
}

// ── the product state, zero-length paths, fixed targets ───────────────────

/// `{0,32}` with the target equal to the start: the zero-edge path, bound
/// to the start, with an empty group list. From a scan, every node reaches
/// itself at zero hops.
#[test]
fn the_zero_edge_path_when_source_is_target() {
    let dir = tempfile::tempdir().unwrap();
    let m = weighted(dir.path(), 3, &[(0, 1, 1), (1, 0, 1), (1, 2, 1)]);
    let p = quantified_edge(out(Some(1)), 0, Some(32), PathMode::Walk);
    for sel in [Sel::Any, Sel::Shortest, Sel::Cheapest] {
        let rows = run(&m, &p, sel, Start::Key("n0"), Some("n0"));
        assert_eq!(
            rows,
            vec![vec![
                node(m.id(0)),
                edges_list(&m, &[]),
                node(m.id(0)),
                BindingValue::Path(PathRef::new(NodeRef(m.id(0)))),
            ]],
            "{sel:?}"
        );
        let all = run(&m, &p, sel, Start::Scan, None);
        for n in &m.nodes {
            let own: Vec<_> = all
                .iter()
                .filter(|r| r[0] == node(n.id) && r[2] == node(n.id))
                .collect();
            assert_eq!(own.len(), 1, "{sel:?}");
            assert_eq!(hops(own[0]), 0, "{sel:?}");
        }
    }
}

/// The search state is (node, automaton state, counter), not the node: on
/// the cycle A->B->C->A, `{4,}` reaches B at four hops (A B C A B), C at
/// five and A at six -- a node BFS would have marked all three seen at
/// hop three. An unbounded WALK is admitted under a selector and ends.
#[test]
fn a_lower_bound_past_a_cycle_needs_the_product_state() {
    let dir = tempfile::tempdir().unwrap();
    let m = weighted(dir.path(), 3, &[(0, 1, 1), (1, 2, 1), (2, 0, 1)]);
    let p = quantified_edge(out(Some(1)), 4, None, PathMode::Walk);
    for sel in [Sel::Shortest, Sel::Cheapest] {
        let rows = run(&m, &p, sel, Start::Key("n0"), None);
        assert_eq!(rows.len(), 3, "{sel:?}");
        for (end, want) in [(1, 4), (2, 5), (0, 6)] {
            let r = to(&rows, m.id(end));
            assert_eq!(r.len(), 1, "{sel:?}");
            assert_eq!(hops(&r[0]), want, "{sel:?} to {end}");
        }
    }
    // A direct edge beside a two-hop route: `{2,2}` still finds the route.
    let dir = tempfile::tempdir().unwrap();
    let m = weighted(dir.path(), 3, &[(0, 2, 1), (0, 1, 1), (1, 2, 1)]);
    let two = quantified_edge(out(Some(1)), 2, Some(2), PathMode::Walk);
    let rows = run(&m, &two, Sel::Shortest, Start::Key("n0"), None);
    assert_eq!(
        rows,
        vec![vec![
            node(m.id(0)),
            edges_list(&m, &[1, 2]),
            node(m.id(2)),
            path_of(&m, 0, &[(1, true), (2, true)]),
        ]]
    );
}

/// An earlier binding is part of the product state. `(s)-[]->(a)-[e]->{2,2}(a)`
/// asks for a two-hop cycle back to `a`: s->a1 and s->a2 both reach x at
/// the same counter, but only x->a2 closes the cycle, so the arrival from
/// a2 must not be discarded as a repeat of the arrival from a1.
#[test]
fn an_earlier_binding_is_part_of_the_product_state() {
    let dir = tempfile::tempdir().unwrap();
    // s=0, a1=1, a2=2, x=3.
    let m = weighted(
        dir.path(),
        4,
        &[(0, 1, 1), (0, 2, 1), (1, 3, 1), (2, 3, 1), (3, 2, 1)],
    );
    let p = TPat {
        states: vec![at(Some(0)), at(Some(1)), at(None), at(None), at(Some(1))],
        links: vec![
            TLink::Edge(out(None)),
            TLink::Same,
            TLink::Edge(out(Some(2))),
            TLink::Same,
        ],
        repeats: vec![Repeat {
            first: 2,
            last: 3,
            min: 2,
            max: Some(2),
        }],
        mode: PathMode::Walk,
        path: Some(3),
        width: 4,
    };
    for sel in [Sel::Shortest, Sel::Any] {
        let rows = run(&m, &p, sel, Start::Key("n0"), None);
        assert_eq!(
            rows,
            vec![vec![
                node(m.id(0)),
                node(m.id(2)),
                edges_list(&m, &[3, 4]),
                path_of(&m, 0, &[(1, true), (3, true), (4, true)]),
            ]],
            "{sel:?}"
        );
    }
}

/// A fixed target turns the search point-to-point: it stops when the
/// target accepts, so it creates fewer states than the search for every
/// end, and gives the same witness.
#[test]
fn a_fixed_target_stops_the_search_early() {
    let dir = tempfile::tempdir().unwrap();
    // A chain 0 -> 1 -> ... -> 11, plus 0 -> 11 directly.
    let mut edges: Vec<(usize, usize, i64)> = (0..11).map(|i| (i, i + 1, 1)).collect();
    edges.push((0, 11, 1));
    let m = weighted(dir.path(), 12, &edges);
    let p = quantified_edge(out(Some(1)), 1, Some(20), PathMode::Walk);
    for sel in [Sel::Shortest, Sel::Cheapest] {
        let states = |target| {
            let mut host = TestHost::default();
            let plan = p.plan(&m, &mut host, sel, Start::Key("n0"), target);
            let mut cursor = GqlCursor::open(&m.db, &host, &plan, Vec::new()).unwrap();
            let page = cursor.next_page(64, GqlBudget::unlimited(), never).unwrap();
            assert!(page.done);
            (page.rows.len(), page.work.path_states)
        };
        let (all_rows, all_states) = states(None);
        let (fixed_rows, fixed_states) = states(Some("n1"));
        assert_eq!(all_rows, 11, "{sel:?}");
        assert_eq!(fixed_rows, 1, "{sel:?}");
        // Node 1 is one hop out: the search stops in the first levels, long
        // before the chain's far end.
        assert!(
            fixed_states * 3 < all_states,
            "{sel:?}: {fixed_states} states for one target, {all_states} for all"
        );
        let direct = run(&m, &p, sel, Start::Key("n0"), Some("n11"));
        assert_eq!(direct.len(), 1);
        assert_eq!(hops(&direct[0]), 1, "{sel:?}");
    }
}

// ── costs and ties ────────────────────────────────────────────────────────

/// Two parallel edges A->B of cost 5 and 2: CHEAPEST crosses the cost-2
/// edge; SHORTEST takes the first in posting order.
#[test]
fn parallel_edges_with_different_costs() {
    let dir = tempfile::tempdir().unwrap();
    let m = weighted(dir.path(), 3, &[(0, 1, 5), (0, 1, 2), (1, 2, 1)]);
    let p = quantified_edge(out(Some(1)), 1, Some(2), PathMode::Walk);
    let cheap = run(&m, &p, Sel::Cheapest, Start::Key("n0"), None);
    assert_eq!(to(&cheap, m.id(1))[0][1], edges_list(&m, &[1]));
    assert_eq!(to(&cheap, m.id(2))[0][1], edges_list(&m, &[1, 2]));
    let short = run(&m, &p, Sel::Shortest, Start::Key("n0"), None);
    assert_eq!(to(&short, m.id(1))[0][1], edges_list(&m, &[0]));
    assert_minimal(&m, &p, Sel::Cheapest, "parallel");
    assert_minimal(&m, &p, Sel::Shortest, "parallel");
}

/// Cost ties break on hops, then on discovery order. To t (node 5):
/// 0->1->2->5 costs 1+1+2 = 4 in three hops, found first; 0->3->5 costs
/// 3+1 = 4 in two hops, found later. Fewer hops wins. To u (node 6): 0->1->6
/// and 0->4->6 both cost 2 in two hops; the first found (via 1) wins.
#[test]
fn cost_ties_prefer_fewer_hops_then_discovery_order() {
    let dir = tempfile::tempdir().unwrap();
    let m = weighted(
        dir.path(),
        7,
        &[
            (0, 1, 1),
            (1, 2, 1),
            (2, 5, 2),
            (0, 3, 3),
            (3, 5, 1),
            (1, 6, 1),
            (0, 4, 1),
            (4, 6, 1),
        ],
    );
    // Under WALK the two arrivals at t's accepting state are one product
    // state, and the one with fewer hops replaces the other; under TRAIL
    // there is no visited set, and the frontier's order alone decides.
    for mode in [PathMode::Walk, PathMode::Trail] {
        let p = quantified_edge(out(Some(1)), 1, Some(4), mode);
        let rows = run(&m, &p, Sel::Cheapest, Start::Key("n0"), None);
        assert_eq!(
            to(&rows, m.id(5))[0][1],
            edges_list(&m, &[3, 4]),
            "{mode:?}"
        );
        assert_eq!(
            to(&rows, m.id(6))[0][1],
            edges_list(&m, &[0, 5]),
            "{mode:?}"
        );
        assert_minimal(&m, &p, Sel::Cheapest, "ties");
    }
}

/// A path mode is checked per PATH: under ACYCLIC the self-loop cannot
/// start the two-hop route to B, so B is reached via C; under TRAIL a
/// route may revisit A but not recross an edge.
#[test]
fn a_path_mode_is_checked_per_path() {
    let dir = tempfile::tempdir().unwrap();
    // A=0 loop, A->B, A->C, C->B.
    let m = weighted(dir.path(), 3, &[(0, 0, 1), (0, 1, 1), (0, 2, 1), (2, 1, 1)]);
    let walk = quantified_edge(out(Some(1)), 2, Some(2), PathMode::Walk);
    let rows = run(&m, &walk, Sel::Shortest, Start::Key("n0"), Some("n1"));
    assert_eq!(rows[0][1], edges_list(&m, &[0, 1]));
    let acyclic = quantified_edge(out(Some(1)), 2, Some(2), PathMode::Acyclic);
    let rows = run(&m, &acyclic, Sel::Shortest, Start::Key("n0"), Some("n1"));
    assert_eq!(rows[0][1], edges_list(&m, &[2, 3]));
    for mode in [PathMode::Walk, PathMode::Trail, PathMode::Acyclic] {
        for sel in [Sel::Shortest, Sel::Cheapest] {
            let max = if mode == PathMode::Walk {
                Some(3)
            } else {
                None
            };
            let p = quantified_edge(out(Some(1)), 1, max, mode);
            assert_minimal(&m, &p, sel, &format!("{mode:?} {sel:?}"));
        }
    }
}

// ── random multigraphs against the brute force ────────────────────────────

fn random_edge(rng: &mut Rng, bind: Option<u16>) -> TEdge {
    TEdge {
        direction: [Direction::Outgoing, Direction::Incoming, Direction::Both]
            [rng.below(3) as usize],
        types: match rng.below(4) {
            0 => Some(vec![0]),
            1 => Some(vec![1, 0]),
            _ => None,
        },
        below: bind.and_then(|_| (rng.below(3) == 0).then(|| 1 + rng.below(9) as i64)),
        bind,
    }
}

fn random_node(rng: &mut Rng, bind: Option<u16>) -> TNode {
    TNode {
        labels: match rng.below(4) {
            0 => Some(vec![0]),
            1 => Some(vec![1]),
            _ => None,
        },
        below: bind.and_then(|_| (rng.below(4) == 0).then(|| rng.below(10) as i64)),
        bind,
    }
}

/// A random pattern, always with a path slot. With `one_edge` (CHEAPEST's
/// one COST): a single quantified edge, bound, with random endpoint tests
/// and a random fixed state before and after. Otherwise a leading hop and
/// one or two repeats (a quantified edge or a two-hop subpath).
fn random_pattern(rng: &mut Rng, one_edge: bool) -> TPat {
    let mode = [PathMode::Walk, PathMode::Trail, PathMode::Acyclic][rng.below(3) as usize];
    let mut width = 1u16;
    let mut slot = |rng: &mut Rng, force: bool| {
        (force || rng.below(2) == 0).then(|| {
            width += 1;
            width - 1
        })
    };
    let mut states = vec![TNode {
        bind: Some(0),
        ..random_node(rng, None)
    }];
    let mut links = Vec::new();
    let mut repeats = Vec::new();
    if !one_edge && rng.below(2) == 0 {
        let b = slot(rng, false);
        links.push(TLink::Edge(random_edge(rng, b)));
        let b = slot(rng, false);
        states.push(random_node(rng, b));
    }
    let n_repeats = if one_edge { 1 } else { 1 + rng.below(2) };
    for _ in 0..n_repeats {
        let b = slot(rng, false);
        links.push(TLink::Same);
        let first = states.len() as u16;
        states.push(random_node(rng, b));
        let hops = if one_edge { 1 } else { 1 + rng.below(2) };
        for _ in 0..hops {
            let b = slot(rng, one_edge);
            links.push(TLink::Edge(random_edge(rng, b)));
            let b = slot(rng, false);
            states.push(random_node(rng, b));
        }
        let last = states.len() as u16 - 1;
        let min = rng.below(3) as u32;
        let max = if mode != PathMode::Walk && rng.below(3) == 0 {
            None
        } else {
            Some(min + rng.below(3) as u32)
        };
        repeats.push(Repeat {
            first,
            last,
            min,
            max,
        });
        links.push(TLink::Same);
        let b = slot(rng, false);
        states.push(random_node(rng, b));
    }
    let path = slot(rng, true);
    TPat {
        states,
        links,
        repeats,
        mode,
        path,
        width,
    }
}

#[test]
fn shortest_matches_the_brute_force_minimum_on_random_multigraphs() {
    let mut pairs = 0;
    for seed in 0..16u64 {
        let dir = tempfile::tempdir().unwrap();
        let m = random_graph(dir.path(), seed, 6, 8, false);
        let mut rng = Rng(seed ^ 0x5EED);
        for i in 0..10 {
            let p = random_pattern(&mut rng, false);
            for sel in [Sel::Shortest, Sel::Any] {
                pairs += assert_minimal(&m, &p, sel, &format!("seed {seed} pattern {i} {sel:?}"));
            }
        }
    }
    assert!(
        pairs > 400,
        "the random patterns reached {pairs} pairs only"
    );
}

#[test]
fn cheapest_matches_the_brute_force_minimum_on_random_multigraphs() {
    let mut pairs = 0;
    for seed in 0..16u64 {
        let dir = tempfile::tempdir().unwrap();
        let m = random_graph(dir.path(), seed, 6, 9, true);
        let mut rng = Rng(seed ^ 0xC057);
        for i in 0..10 {
            let p = random_pattern(&mut rng, true);
            pairs += assert_minimal(&m, &p, Sel::Cheapest, &format!("seed {seed} pattern {i}"));
        }
    }
    assert!(
        pairs > 400,
        "the random patterns reached {pairs} pairs only"
    );
}

// ── refusals when the execution opens ─────────────────────────────────────

fn open_error(m: &Model, p: &TPat, sel: Sel) -> String {
    let mut host = TestHost::default();
    let plan = p.plan(m, &mut host, sel, Start::Scan, None);
    match GqlCursor::open(&m.db, &host, &plan, Vec::new()).map(|_| ()) {
        Ok(()) => panic!("opened: {p:?}"),
        Err(e) => format!("{e:?}"),
    }
}

/// CHEAPEST's one COST reads the edge in its slot: an edge step with no
/// slot, or a second edge step, is refused when the execution opens.
#[test]
fn cheapest_needs_one_bound_edge_step() {
    let dir = tempfile::tempdir().unwrap();
    let m = weighted(dir.path(), 2, &[(0, 1, 1)]);
    let mut host = TestHost::default();
    let p = quantified_edge(out(None), 1, Some(2), PathMode::Walk);
    let automaton = p.automaton(&m, &mut host);
    let plan = OpSpec::PathSearch {
        input: Box::new(OpSpec::Unit { width: 4 }),
        from: SlotId(0),
        automaton,
        search: PathSearch::Cheapest {
            cost: host.expr(E::Cost(1)),
        },
    };
    let refused = GqlCursor::open(&m.db, &host, &plan, Vec::new()).map(|_| ());
    assert!(format!("{refused:?}").contains("COST"), "{refused:?}");

    let mut two = quantified_edge(out(Some(1)), 1, Some(2), PathMode::Walk);
    two.states.push(at(None));
    two.links.push(TLink::Edge(out(Some(4))));
    two.width = 5;
    assert!(open_error(&m, &two, Sel::Cheapest).contains("COST"));

    // The same patterns are fine for SHORTEST.
    run(&m, &two, Sel::Shortest, Start::Scan, None);
}

// ── budgets, cancellation, paging ─────────────────────────────────────────

fn refusal(result: QueryResult<impl std::fmt::Debug>) -> (WorkResource, u64) {
    match result {
        Err(QueryError::BudgetExceeded {
            resource, limit, ..
        }) => (resource, limit),
        other => panic!("expected a budget refusal, got {other:?}"),
    }
}

#[test]
fn each_path_budget_refuses_by_name_and_poisons_the_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let m = random_graph(dir.path(), 7, 8, 16, true);
    let unlimited = GqlBudget::unlimited();
    for sel in [Sel::Shortest, Sel::Cheapest] {
        let p = quantified_edge(
            TEdge {
                direction: Direction::Both,
                ..out(Some(1))
            },
            1,
            None,
            PathMode::Walk,
        );
        let mut host = TestHost::default();
        let plan = p.plan(&m, &mut host, sel, Start::Scan, None);

        let all = run_paged(&m, &host, &plan, 64, unlimited).unwrap();
        assert!(all.len() > 20, "{} rows", all.len());

        for (budget, resource, limit) in [
            (
                GqlBudget {
                    path_states: 10,
                    ..unlimited
                },
                WorkResource::PathStates,
                10,
            ),
            (
                GqlBudget {
                    queue_entries: 3,
                    ..unlimited
                },
                WorkResource::QueueEntries,
                3,
            ),
            (
                GqlBudget {
                    predecessor_arcs: 10,
                    ..unlimited
                },
                WorkResource::PredecessorArcs,
                10,
            ),
            (
                GqlBudget {
                    binding_rows: 20,
                    ..unlimited
                },
                WorkResource::BindingRows,
                20,
            ),
        ] {
            let mut cursor = GqlCursor::open(&m.db, &host, &plan, Vec::new()).unwrap();
            let first = cursor.next_page(8192, budget, never);
            assert_eq!(refusal(first), (resource, limit), "{sel:?}");
            assert_eq!(
                refusal(cursor.next_page(8192, unlimited, never)),
                (resource, limit),
                "{sel:?}: poisoned"
            );
        }

        let mut cursor = GqlCursor::open(&m.db, &host, &plan, Vec::new()).unwrap();
        let page = cursor.next_page(8192, unlimited, never).unwrap();
        assert!(page.done);
        assert_eq!(
            page.work.binding_rows as usize,
            m.nodes.len() + page.rows.len(),
            "{sel:?}: one row per seed, one per witness"
        );
        assert!(page.work.path_states >= page.rows.len() as u64);
        assert!(page.work.queue_entries > 0);
        assert!(page.work.predecessor_arcs > 0);
        assert!(page.work.base.graph_edges > 0);
    }
}

#[test]
fn cancellation_stops_the_search_and_poisons_it() {
    let dir = tempfile::tempdir().unwrap();
    let m = random_graph(dir.path(), 3, 8, 16, true);
    let p = quantified_edge(out(Some(1)), 1, None, PathMode::Walk);
    for sel in [Sel::Shortest, Sel::Cheapest] {
        let mut host = TestHost::default();
        let plan = p.plan(&m, &mut host, sel, Start::Scan, None);
        let mut cursor = GqlCursor::open(&m.db, &host, &plan, Vec::new()).unwrap();
        let mut calls = 0;
        let cancel_later = || {
            calls += 1;
            calls > 25
        };
        assert!(matches!(
            cursor.next_page(8192, GqlBudget::unlimited(), cancel_later),
            Err(QueryError::Cancelled)
        ));
        assert!(matches!(
            cursor.next_page(8192, GqlBudget::unlimited(), never),
            Err(QueryError::Cancelled)
        ));
    }
}

#[test]
fn paging_equals_one_shot() {
    let dir = tempfile::tempdir().unwrap();
    let m = random_graph(dir.path(), 11, 6, 10, true);
    let mut rng = Rng(99);
    for i in 0..8 {
        let one_edge = i % 2 == 0;
        let p = random_pattern(&mut rng, one_edge);
        for sel in [Sel::Shortest, Sel::Cheapest] {
            if sel == Sel::Cheapest && !one_edge {
                continue;
            }
            let mut host = TestHost::default();
            let plan = p.plan(&m, &mut host, sel, Start::Scan, None);
            let one_shot = run_paged(&m, &host, &plan, 8192, GqlBudget::unlimited()).unwrap();
            for page_rows in [1, 2, 3, 7] {
                let paged = run_paged(&m, &host, &plan, page_rows, GqlBudget::unlimited()).unwrap();
                assert_eq!(paged, one_shot, "page size {page_rows}, {sel:?} {p:?}");
            }
        }
    }
}

/// The frontier and the predecessor arcs outlive a page: a page that cannot
/// hold them again is refused before the search takes a step.
#[test]
fn held_search_state_is_charged_again_to_each_page() {
    let dir = tempfile::tempdir().unwrap();
    // A cycle with a chord: the first row (node 0 to itself) is emitted
    // while the states one hop out are still queued.
    let m = weighted(
        dir.path(),
        4,
        &[(0, 1, 1), (1, 2, 1), (2, 3, 1), (3, 0, 1), (0, 2, 3)],
    );
    let p = quantified_edge(out(Some(1)), 0, None, PathMode::Walk);
    for sel in [Sel::Shortest, Sel::Cheapest] {
        let mut host = TestHost::default();
        let plan = p.plan(&m, &mut host, sel, Start::Scan, None);
        for (tight, resource) in [
            (
                GqlBudget {
                    queue_entries: 1,
                    path_states: 0,
                    binding_rows: 0,
                    ..GqlBudget::unlimited()
                },
                WorkResource::QueueEntries,
            ),
            (
                GqlBudget {
                    predecessor_arcs: 1,
                    path_states: 0,
                    binding_rows: 0,
                    ..GqlBudget::unlimited()
                },
                WorkResource::PredecessorArcs,
            ),
        ] {
            let mut cursor = GqlCursor::open(&m.db, &host, &plan, Vec::new()).unwrap();
            let first = cursor.next_page(1, GqlBudget::unlimited(), never).unwrap();
            assert!(!first.done);
            assert!(first.work.queue_entries > 1, "{sel:?}");
            assert!(first.work.predecessor_arcs > 1, "{sel:?}");
            assert_eq!(
                refusal(cursor.next_page(1, tight, never)),
                (resource, 1),
                "{sel:?}"
            );
        }
    }
}

// ── the work a search does, pinned ────────────────────────────────────────

/// The counters one search spends: rows out, product states, the most
/// frontier entries and predecessor arcs held, list and duplicate-set
/// bytes, edges walked and rows read.
fn counted(work: &GqlWork) -> [u64; 8] {
    [
        work.binding_rows,
        work.path_states,
        work.queue_entries,
        work.predecessor_arcs,
        work.list_bytes,
        work.sort_bytes,
        work.base.graph_edges,
        work.base.primary_reads,
    ]
}

/// The work of every representative search -- enumeration, ANY, ANY
/// SHORTEST, ANY CHEAPEST -- over random patterns (predicates, groups,
/// every direction and mode) is pinned, counter by counter: a change to the
/// searches' inner loops may spend less, never more, and these numbers say
/// which. Taken before the M4 review's refactors (task R-C).
#[test]
fn the_work_of_representative_searches_is_pinned() {
    let dir = tempfile::tempdir().unwrap();
    let m = random_graph(dir.path(), 5, 6, 9, true);
    let mut rng = Rng(0x12C);
    let mut spent = Vec::new();
    // Random patterns, then one that reads on every arrival: an
    // either-direction edge predicate (an incoming edge's bag is one more
    // read) and an end-node predicate (one primary read).
    let mut read = quantified_edge(
        TEdge {
            direction: Direction::Both,
            types: None,
            below: Some(7),
            bind: Some(1),
        },
        1,
        Some(3),
        PathMode::Trail,
    );
    read.states[3].below = Some(6);
    let patterns = (0..10)
        .map(|i| (random_pattern(&mut rng, i % 2 == 0), i % 2 == 0))
        .chain([(read, true)]);
    for (p, one_edge) in patterns {
        let mut sels = vec![None, Some(Sel::Any), Some(Sel::Shortest)];
        if one_edge {
            sels.push(Some(Sel::Cheapest));
        }
        for sel in sels {
            let mut host = TestHost::default();
            let mut plan = p.plan(&m, &mut host, sel.unwrap_or(Sel::Shortest), Start::Scan, None);
            if sel.is_none() {
                let OpSpec::Project { input, .. } = &mut plan else {
                    unreachable!("a plan ends in its projection")
                };
                let OpSpec::PathSearch { search, .. } = &mut **input else {
                    unreachable!("under a path search")
                };
                *search = PathSearch::Enumerate;
            }
            let mut cursor = GqlCursor::open(&m.db, &host, &plan, Vec::new()).unwrap();
            let page = cursor.next_page(8192, GqlBudget::unlimited(), never).unwrap();
            assert!(page.done);
            spent.push(counted(&page.work));
        }
    }
    // Per pattern: enumeration, ANY, ANY SHORTEST, then ANY CHEAPEST for a
    // one-edge pattern.
    let pinned: &[[u64; 8]] = &[
        [41, 85, 11, 0, 906, 0, 44, 8],
        [16, 43, 6, 14, 585, 0, 36, 8],
        [16, 43, 6, 14, 585, 0, 36, 8],
        [16, 43, 7, 17, 585, 0, 36, 8],
        [7, 19, 9, 0, 160, 0, 67, 8],
        [7, 19, 4, 8, 160, 0, 67, 8],
        [7, 19, 4, 8, 160, 0, 67, 8],
        [6, 14, 6, 0, 0, 0, 12, 8],
        [6, 14, 4, 5, 0, 0, 12, 8],
        [6, 14, 4, 5, 0, 0, 12, 8],
        [6, 14, 4, 5, 0, 0, 12, 8],
        [6, 6, 4, 0, 0, 0, 18, 8],
        [6, 6, 3, 1, 0, 0, 18, 8],
        [6, 6, 3, 1, 0, 0, 18, 8],
        [6, 2, 1, 0, 0, 0, 0, 8],
        [6, 2, 1, 1, 0, 0, 0, 8],
        [6, 2, 1, 1, 0, 0, 0, 8],
        [6, 2, 1, 1, 0, 0, 0, 8],
        [6, 19, 8, 0, 0, 0, 43, 8],
        [6, 19, 4, 6, 0, 0, 43, 8],
        [6, 19, 4, 6, 0, 0, 43, 8],
        [6, 4, 2, 0, 0, 0, 2, 8],
        [6, 4, 1, 2, 0, 0, 2, 8],
        [6, 4, 1, 2, 0, 0, 2, 8],
        [6, 4, 1, 2, 0, 0, 2, 8],
        [6, 13, 8, 0, 0, 0, 27, 8],
        [6, 13, 4, 4, 0, 0, 27, 8],
        [6, 13, 4, 4, 0, 0, 27, 8],
        [16, 26, 8, 0, 232, 0, 12, 8],
        [10, 15, 2, 6, 88, 0, 9, 8],
        [10, 15, 2, 6, 88, 0, 9, 8],
        [10, 15, 3, 8, 88, 0, 13, 8],
        [6, 6, 1, 0, 0, 0, 0, 14],
        [6, 6, 1, 1, 0, 0, 0, 14],
        [6, 6, 1, 1, 0, 0, 0, 14],
        [14, 96, 13, 0, 514, 0, 237, 56],
        [10, 96, 9, 26, 409, 0, 237, 56],
        [10, 96, 9, 26, 409, 0, 237, 56],
        [10, 96, 8, 26, 409, 0, 260, 56],
    ];
    assert_eq!(spent, pinned, "{spent:?}");
}
