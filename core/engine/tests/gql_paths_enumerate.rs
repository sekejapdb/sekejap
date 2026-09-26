//! Path enumeration without a selector (`docs/lang/GQL_PROFILE_DESIGN.md`
//! §4.1, §4.2, §4.4, §4.5, §4.7): `OpSpec::PathSearch` with
//! `PathSearch::Enumerate` over hand-built `PathAutomaton`s.
//!
//! What is at risk, and the test that pins it:
//!
//! * that the answer is the BAG a tiny exhaustive reference computes -- it
//!   expands every quantifier count into a straight pattern and matches
//!   that edge by edge, a different algorithm from the engine's automaton
//!   search -- on fixed graphs and on seeded random multigraphs with
//!   parallel edges, self-loops and cycles, under WALK, TRAIL and ACYCLIC
//!   (`matches_the_reference_*`);
//! * the brief's matrix rows: two sides of a diamond are two matches; a
//!   direct edge does not hide a two-hop route under `{2,2}`; a self-loop and
//!   a directed cycle under each mode; `{0,0}`, `{0,1}`, `?` and `*`
//!   (zero-length paths, empty group lists); a multi-type subpath repeats
//!   as a whole; endpoint labels do not constrain intermediate nodes;
//! * that two runs with the same path and bindings are one match, and runs
//!   with different bindings are two (design Q11);
//! * that an unbounded WALK, nested or touching quantifiers, and a group
//!   that crosses no edge are refused when the execution opens;
//! * that `path_states`, `queue_entries`, `binding_rows` and `list_bytes`
//!   refuse by name, a refusal or a cancellation poisons the cursor, and
//!   the pages of one cursor concatenate to the one-shot answer.

use sekejap_core::collections::gql::{
    BindingRow, BindingValue, EdgeRef, EdgeStep, EvalCx, ExprId, GqlBudget, GqlCursor, GqlHost,
    ListRef, NodeRef, NodeTest, OpSpec, PathAutomaton, PathLink, PathMode, PathRef, PathSearch,
    Repeat, SeedId, SeedSource, SlotId, Truth, ValueType,
};
use sekejap_core::collections::{
    CollectionId, Database, Direction, EdgeKey, EdgeTypeId, EntityId, GraphContextId,
    PreparedQuery, QueryError, QueryResult, WorkResource,
};
use sekejap_core::Kind;
use serde_json::json;
use std::collections::HashSet;

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

#[derive(Clone, Debug)]
enum E {
    Slot(u16),
    /// `slot.x < k` on a node, `slot.w < k` on an edge; `Null` is unknown.
    Below(u16, &'static str, i64),
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
}

/// A graph of `nodes` (collection index, `x`) and `edges` (source index,
/// destination index, type index, `w`), in collections `site_a`, `site_b`
/// and types `r`, `s`.
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

/// A random multigraph: parallel twins and self-loops forced in.
fn random_graph(dir: &std::path::Path, seed: u64, nodes: usize, edges: usize) -> Model {
    let mut rng = Rng(seed);
    let ns: Vec<(usize, Option<i64>)> = (0..nodes)
        .map(|_| {
            (
                rng.below(2) as usize,
                (rng.below(5) != 0).then(|| rng.below(10) as i64),
            )
        })
        .collect();
    let mut es = Vec::new();
    while es.len() < edges {
        let (s, d) = (
            rng.below(nodes as u64) as usize,
            rng.below(nodes as u64) as usize,
        );
        let t = rng.below(2) as usize;
        let w = |rng: &mut Rng| (rng.below(4) != 0).then(|| rng.below(10) as i64);
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

    /// `MATCH (s IS site_a | site_b) <pattern> RETURN *`: a scan seed into
    /// slot 0, then the search from it.
    fn plan(&self, m: &Model, host: &mut TestHost) -> OpSpec {
        let automaton = self.automaton(m, host);
        let seed = Box::new(OpSpec::Seed {
            input: Box::new(OpSpec::Unit { width: self.width }),
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
        let cols: Vec<ExprId> = (0..self.width).map(|s| host.expr(E::Slot(s))).collect();
        OpSpec::Project {
            input: search,
            cols: cols.into(),
            width: self.width,
        }
    }
}

// ── the reference: expand every count, match straight lines ──────────────

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

struct Walk<'a> {
    m: &'a Model,
    p: &'a TPat,
    items: Vec<Item>,
    /// (slot, value, group?) in path order.
    binds: Vec<(u16, BindingValue, bool)>,
    nodes: Vec<EntityId>,
    edges: Vec<EdgeRef>,
}

impl Walk<'_> {
    fn go(
        &mut self,
        i: usize,
        at: EntityId,
        path: PathRef,
        found: &mut dyn FnMut(&PathRef, &[(u16, BindingValue, bool)]),
    ) {
        if i == self.items.len() {
            let ok = match self.p.mode {
                PathMode::Walk => true,
                PathMode::Trail => {
                    let set: HashSet<_> = self.edges.iter().collect();
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
                found(&path, &self.binds);
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
                    self.edges.push(edge_ref(&edge));
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

/// The reference answer for a scan over every node: per start node, the SET
/// of distinct (path, row) matches over every count vector, as rows.
fn reference(m: &Model, p: &TPat) -> Vec<Vec<BindingValue>> {
    let longest = match p.mode {
        PathMode::Walk => usize::MAX,
        PathMode::Trail => m.edges.len(),
        PathMode::Acyclic => m.nodes.len() - 1,
    };
    let group_slots = p.groups();
    let mut rows = Vec::new();
    for start in &m.nodes {
        let mut seen: HashSet<(PathRef, Vec<BindingValue>)> = HashSet::new();
        for n in counts(p, longest) {
            let mut walk = Walk {
                m,
                p,
                items: expand(p, &n),
                binds: Vec::new(),
                nodes: vec![start.id],
                edges: Vec::new(),
            };
            let mut found = |path: &PathRef, binds: &[(u16, BindingValue, bool)]| {
                let mut row = vec![BindingValue::Null; usize::from(p.width)];
                row[0] = node(start.id);
                let mut assigned = vec![false; row.len()];
                assigned[0] = true;
                for (slot, is_node) in &group_slots {
                    let items = binds
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
                for (slot, value, group) in binds {
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
                    row[usize::from(ps)] = BindingValue::Path(path.clone());
                }
                if seen.insert((path.clone(), row.clone())) {
                    rows.push(row);
                }
            };
            walk.go(0, start.id, PathRef::new(NodeRef(start.id)), &mut found);
        }
    }
    rows
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

fn run(m: &Model, p: &TPat) -> Vec<Vec<BindingValue>> {
    let mut host = TestHost::default();
    let plan = p.plan(m, &mut host);
    run_paged(m, &host, &plan, 64, GqlBudget::unlimited()).unwrap()
}

fn bag(mut rows: Vec<Vec<BindingValue>>) -> Vec<Vec<BindingValue>> {
    rows.sort();
    rows
}

fn assert_matches_reference(m: &Model, p: &TPat, what: &str) -> usize {
    let got = bag(run(m, p));
    let want = bag(reference(m, p));
    assert_eq!(got.len(), want.len(), "{what}: row count, pattern {p:?}");
    assert_eq!(got, want, "{what}: pattern {p:?}");
    got.len()
}

/// The rows that start at node `start`.
fn from(rows: Vec<Vec<BindingValue>>, start: EntityId) -> Vec<Vec<BindingValue>> {
    rows.into_iter().filter(|r| r[0] == node(start)).collect()
}

fn path_of(m: &Model, start: usize, steps: &[(usize, bool)]) -> BindingValue {
    let mut path = PathRef::new(NodeRef(m.nodes[start].id));
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

// ── matrix rows ───────────────────────────────────────────────────────────

/// Diamond A->B->D, A->C->D: `{2,2}` gives the two sides as two matches,
/// and without a path slot as two IDENTICAL rows (bag, not set).
#[test]
fn a_diamond_gives_two_matches() {
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
    let p = quantified_edge(out(Some(1)), 2, Some(2), PathMode::Walk);
    let rows = from(run(&m, &p), m.nodes[0].id);
    assert_eq!(
        bag(rows),
        bag(vec![
            vec![
                node(m.nodes[0].id),
                edges_list(&m, &[0, 2]),
                node(m.nodes[3].id),
                path_of(&m, 0, &[(0, true), (2, true)]),
            ],
            vec![
                node(m.nodes[0].id),
                edges_list(&m, &[1, 3]),
                node(m.nodes[3].id),
                path_of(&m, 0, &[(1, true), (3, true)]),
            ],
        ])
    );
    assert_matches_reference(&m, &p, "diamond");

    // No path, no edge variable: the two matches are equal rows, both kept.
    let bare = TPat {
        path: None,
        states: vec![at(Some(0)), at(None), at(None), at(Some(1))],
        links: vec![TLink::Same, TLink::Edge(out(None)), TLink::Same],
        width: 2,
        ..p
    };
    let rows = from(run(&m, &bare), m.nodes[0].id);
    assert_eq!(
        rows,
        vec![
            vec![node(m.nodes[0].id), node(m.nodes[3].id)],
            vec![node(m.nodes[0].id), node(m.nodes[3].id)],
        ]
    );
}

/// A->D directly and A->B->D: `{2,2}` still finds A->B->D -- seeing D at one
/// hop does not stop D at two -- and `{1,2}` finds D twice.
#[test]
fn a_direct_edge_does_not_hide_the_two_hop_route() {
    let dir = tempfile::tempdir().unwrap();
    let m = build(
        dir.path(),
        &[(0, None); 3],
        &[(0, 2, 0, None), (0, 1, 0, None), (1, 2, 0, None)],
    );
    let a = m.nodes[0].id;
    let two = quantified_edge(out(Some(1)), 2, Some(2), PathMode::Walk);
    assert_eq!(
        from(run(&m, &two), a),
        vec![vec![
            node(a),
            edges_list(&m, &[1, 2]),
            node(m.nodes[2].id),
            path_of(&m, 0, &[(1, true), (2, true)]),
        ]]
    );
    let one_two = quantified_edge(out(Some(1)), 1, Some(2), PathMode::Walk);
    let to_d = from(run(&m, &one_two), a)
        .into_iter()
        .filter(|r| r[2] == node(m.nodes[2].id))
        .count();
    assert_eq!(to_d, 2);
    assert_matches_reference(&m, &two, "direct plus two-hop {2,2}");
    assert_matches_reference(&m, &one_two, "direct plus two-hop {1,2}");
}

/// A self-loop on A and the cycle A->B->A, `{1,3}`, under each mode.
#[test]
fn a_self_loop_and_a_cycle_under_each_mode() {
    let dir = tempfile::tempdir().unwrap();
    let m = build(
        dir.path(),
        &[(0, None); 2],
        &[(0, 0, 0, None), (0, 1, 0, None), (1, 0, 0, None)],
    );
    let a = m.nodes[0].id;
    let count = |mode, max| {
        let p = quantified_edge(out(Some(1)), 1, max, mode);
        assert_matches_reference(&m, &p, &format!("{mode:?} {max:?}"));
        from(run(&m, &p), a).len()
    };
    // WALK within three hops: 2 + 3 + 5 walks from A.
    assert_eq!(count(PathMode::Walk, Some(3)), 10);
    // TRAIL: each edge once -- loop, AB, loop AB, AB BA, loop AB BA,
    // AB BA loop.
    assert_eq!(count(PathMode::Trail, Some(3)), 6);
    assert_eq!(count(PathMode::Trail, None), 6);
    // ACYCLIC: no node twice -- only AB; the loop revisits A at once.
    assert_eq!(count(PathMode::Acyclic, Some(3)), 1);
    assert_eq!(count(PathMode::Acyclic, None), 1);

    // Either direction, one hop: the loop once (forward), AB forward, BA
    // backward.
    let both = TEdge {
        direction: Direction::Both,
        ..out(Some(1))
    };
    let p = quantified_edge(both, 1, Some(1), PathMode::Walk);
    let rows = bag(from(run(&m, &p), a));
    assert_eq!(
        rows,
        bag(vec![
            vec![
                node(a),
                edges_list(&m, &[0]),
                node(a),
                path_of(&m, 0, &[(0, true)])
            ],
            vec![
                node(a),
                edges_list(&m, &[1]),
                node(m.nodes[1].id),
                path_of(&m, 0, &[(1, true)])
            ],
            vec![
                node(a),
                edges_list(&m, &[2]),
                node(m.nodes[1].id),
                path_of(&m, 0, &[(2, false)])
            ],
        ])
    );
    for mode in [PathMode::Walk, PathMode::Trail, PathMode::Acyclic] {
        let both = TEdge {
            direction: Direction::Both,
            ..out(Some(1))
        };
        let max = if mode == PathMode::Walk {
            Some(3)
        } else {
            None
        };
        assert_matches_reference(&m, &quantified_edge(both, 0, max, mode), "both");
    }
}

/// `{0,0}`: only the zero-length path, `t` bound to the start and `e` to an
/// empty list. `{0,1}` = `?` adds the one-hop paths; `*` = `{0,}` under
/// TRAIL.
#[test]
fn zero_length_quantifiers_bind_the_start_and_empty_lists() {
    let dir = tempfile::tempdir().unwrap();
    let m = build(
        dir.path(),
        &[(0, None), (1, None), (0, None)],
        &[(0, 1, 0, None), (1, 2, 1, None), (2, 0, 0, None)],
    );
    let zero = quantified_edge(out(Some(1)), 0, Some(0), PathMode::Walk);
    let rows = bag(run(&m, &zero));
    let want = bag(m
        .nodes
        .iter()
        .map(|n| {
            vec![
                node(n.id),
                edges_list(&m, &[]),
                node(n.id),
                BindingValue::Path(PathRef::new(NodeRef(n.id))),
            ]
        })
        .collect());
    assert_eq!(rows, want);
    assert_matches_reference(&m, &zero, "{0,0}");

    let optional = quantified_edge(out(Some(1)), 0, Some(1), PathMode::Walk);
    assert_eq!(assert_matches_reference(&m, &optional, "{0,1}"), 3 + 3);
    let star = quantified_edge(out(Some(1)), 0, None, PathMode::Trail);
    // Around the 3-cycle: from each node the paths of 0, 1, 2 and 3 edges.
    assert_eq!(assert_matches_reference(&m, &star, "*"), 3 * 4);
    let star = quantified_edge(out(Some(1)), 0, None, PathMode::Acyclic);
    assert_eq!(assert_matches_reference(&m, &star, "* acyclic"), 3 * 3);

    // `(s) -[e]->? (t IS site_b)`: the endpoint label applies to the node
    // the zero-length path stands on too.
    let mut labelled = quantified_edge(out(Some(1)), 0, Some(1), PathMode::Walk);
    labelled.states[3].labels = Some(vec![1]);
    let rows = run(&m, &labelled);
    assert_eq!(rows.len(), 2, "n1 by zero hops, and n0 -> n1");
    assert_matches_reference(&m, &labelled, "? with an endpoint label");
}

/// `(s) ((a) -[:r]-> (b) -[:s]-> (c)){1,3} (t)`: the body repeats as a
/// whole, r s r s, never r r; the group variables list every iteration.
#[test]
fn a_multi_type_subpath_repeats_as_a_whole() {
    let dir = tempfile::tempdir().unwrap();
    // 0 -r-> 1 -s-> 2 -r-> 3 -s-> 4, and 0 -r-> 5 -r-> 6 -s-> 7.
    let m = build(
        dir.path(),
        &[(0, None); 8],
        &[
            (0, 1, 0, None),
            (1, 2, 1, None),
            (2, 3, 0, None),
            (3, 4, 1, None),
            (0, 5, 0, None),
            (5, 6, 0, None),
            (6, 7, 1, None),
        ],
    );
    let typed = |t: usize, bind| TEdge {
        types: Some(vec![t]),
        ..out(bind)
    };
    let p = TPat {
        states: vec![at(Some(0)), at(Some(1)), at(None), at(Some(2)), at(Some(3))],
        links: vec![
            TLink::Same,
            TLink::Edge(typed(0, None)),
            TLink::Edge(typed(1, None)),
            TLink::Same,
        ],
        repeats: vec![Repeat {
            first: 1,
            last: 3,
            min: 1,
            max: Some(3),
        }],
        mode: PathMode::Walk,
        path: None,
        width: 4,
    };
    let nodes = |ids: &[usize]| {
        list(
            ids.iter().map(|i| node(m.nodes[*i].id)).collect(),
            ValueType::Node([].into()),
        )
    };
    let rows = bag(from(run(&m, &p), m.nodes[0].id));
    assert_eq!(
        rows,
        bag(vec![
            vec![
                node(m.nodes[0].id),
                nodes(&[0]),
                nodes(&[2]),
                node(m.nodes[2].id)
            ],
            vec![
                node(m.nodes[0].id),
                nodes(&[0, 2]),
                nodes(&[2, 4]),
                node(m.nodes[4].id)
            ],
        ])
    );
    assert_matches_reference(&m, &p, "multi-type subpath");
}

/// `(a IS site_a) -[]->{1,3} (b IS site_a)`: the intermediate nodes are
/// anonymous, so a `site_b` node may sit between the two endpoints.
#[test]
fn quantified_endpoint_labels_constrain_only_the_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let m = build(
        dir.path(),
        &[(0, None), (1, None), (0, None)],
        &[(0, 1, 0, None), (1, 2, 0, None)],
    );
    let mut p = quantified_edge(out(Some(1)), 1, Some(3), PathMode::Walk);
    p.states[0].labels = Some(vec![0]);
    p.states[3].labels = Some(vec![0]);
    let rows = run(&m, &p);
    assert_eq!(
        rows,
        vec![vec![
            node(m.nodes[0].id),
            edges_list(&m, &[0, 1]),
            node(m.nodes[2].id),
            path_of(&m, 0, &[(0, true), (1, true)]),
        ]]
    );
    assert_matches_reference(&m, &p, "endpoint labels");
}

/// `(s) -[]->{0,1} (m) -[]->{0,1} (t)`: a one-edge path matches with `m` at
/// either end -- two bindings, two matches -- and when `m` is not bound
/// the two runs are one match (design Q11).
#[test]
fn identical_matches_are_deduplicated_and_distinct_bindings_are_not() {
    let dir = tempfile::tempdir().unwrap();
    let m = build(dir.path(), &[(0, None); 2], &[(0, 1, 0, None)]);
    let p = TPat {
        states: vec![
            at(Some(0)),
            at(None),
            at(None),
            at(Some(1)),
            at(None),
            at(None),
            at(Some(2)),
        ],
        links: vec![
            TLink::Same,
            TLink::Edge(out(None)),
            TLink::Same,
            TLink::Same,
            TLink::Edge(out(None)),
            TLink::Same,
        ],
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
        ],
        mode: PathMode::Walk,
        path: None,
        width: 3,
    };
    let (a, b) = (node(m.nodes[0].id), node(m.nodes[1].id));
    assert_eq!(
        bag(run(&m, &p)),
        bag(vec![
            vec![a.clone(), a.clone(), a.clone()],
            vec![a.clone(), a.clone(), b.clone()],
            vec![a.clone(), b.clone(), b.clone()],
            vec![b.clone(), b.clone(), b.clone()],
        ])
    );
    assert_matches_reference(&m, &p, "two optional hops, m bound");

    let mut unbound = p.clone();
    unbound.states[3].bind = None;
    unbound.width = 3;
    assert_eq!(
        bag(run(&m, &unbound)),
        bag(vec![
            vec![a.clone(), BindingValue::Null, a.clone()],
            vec![a.clone(), BindingValue::Null, b.clone()],
            vec![b.clone(), BindingValue::Null, b.clone()],
        ])
    );
    assert_matches_reference(&m, &unbound, "two optional hops, m anonymous");
}

/// `(a) -[]-> (b) -[]-> (a)`: a repeated singleton variable is an identity
/// constraint -- only the two-cycles back to the start match.
#[test]
fn a_repeated_variable_is_an_identity_constraint() {
    let dir = tempfile::tempdir().unwrap();
    let m = build(
        dir.path(),
        &[(0, None); 3],
        &[
            (0, 1, 0, None),
            (1, 0, 0, None),
            (1, 2, 0, None),
            (2, 2, 0, None),
        ],
    );
    let p = TPat {
        states: vec![at(Some(0)), at(Some(1)), at(Some(0))],
        links: vec![TLink::Edge(out(None)), TLink::Edge(out(None))],
        repeats: vec![],
        mode: PathMode::Walk,
        path: None,
        width: 2,
    };
    let (n0, n1, n2) = (
        node(m.nodes[0].id),
        node(m.nodes[1].id),
        node(m.nodes[2].id),
    );
    assert_eq!(
        bag(run(&m, &p)),
        bag(vec![
            vec![n0.clone(), n1.clone()],
            vec![n1, n0],
            vec![n2.clone(), n2]
        ])
    );
    assert_matches_reference(&m, &p, "repeated variable");
}

// ── random multigraphs ────────────────────────────────────────────────────

fn random_edge(rng: &mut Rng, bind: Option<u16>) -> TEdge {
    TEdge {
        direction: [Direction::Outgoing, Direction::Incoming, Direction::Both]
            [rng.below(3) as usize],
        types: match rng.below(4) {
            0 => Some(vec![0]),
            1 => Some(vec![1, 0]),
            _ => None,
        },
        below: bind.and_then(|_| (rng.below(3) == 0).then(|| rng.below(10) as i64)),
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

/// A random pattern: a leading hop, then one or two repeats (a quantified
/// edge, or a two-hop subpath) with random bounds, joined by hops or
/// directly, and a random mode.
fn random_pattern(rng: &mut Rng) -> TPat {
    let mode = [PathMode::Walk, PathMode::Trail, PathMode::Acyclic][rng.below(3) as usize];
    let mut width = 1u16;
    let mut slot = |rng: &mut Rng| {
        (rng.below(2) == 0).then(|| {
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
    if rng.below(2) == 0 {
        let b = slot(rng);
        links.push(TLink::Edge(random_edge(rng, b)));
        let b = slot(rng);
        states.push(random_node(rng, b));
    }
    let n_repeats = 1 + rng.below(2);
    for _ in 0..n_repeats {
        let b = slot(rng);
        links.push(TLink::Same);
        let first = states.len() as u16;
        states.push(random_node(rng, b));
        let hops = 1 + rng.below(2);
        for _ in 0..hops {
            let b = slot(rng);
            links.push(TLink::Edge(random_edge(rng, b)));
            let b = slot(rng);
            states.push(random_node(rng, b));
        }
        let last = states.len() as u16 - 1;
        let min = rng.below(2) as u32;
        let max = if mode != PathMode::Walk && rng.below(3) == 0 {
            None
        } else {
            Some(min + rng.below(2) as u32)
        };
        repeats.push(Repeat {
            first,
            last,
            min,
            max,
        });
        links.push(TLink::Same);
        let b = slot(rng);
        states.push(random_node(rng, b));
    }
    let path = (rng.below(2) == 0).then(|| {
        width += 1;
        width - 1
    });
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
fn matches_the_reference_on_random_multigraphs_as_bags() {
    let (mut total, mut ambiguous) = (0, 0);
    for seed in 0..20u64 {
        let dir = tempfile::tempdir().unwrap();
        let m = random_graph(dir.path(), seed, 6, 8);
        let mut rng = Rng(seed ^ 0x5EED);
        for i in 0..12 {
            let p = random_pattern(&mut rng);
            let found = assert_matches_reference(&m, &p, &format!("seed {seed} pattern {i}"));
            total += found;
            // Two repeats of variable count: the deduplicating search.
            if found > 0 && p.repeats.iter().filter(|r| r.max != Some(r.min)).count() > 1 {
                ambiguous += 1;
            }
        }
    }
    assert!(
        total > 1000,
        "the random patterns found {total} matches only"
    );
    assert!(
        ambiguous > 10,
        "only {ambiguous} ambiguous patterns matched anything"
    );
}

#[test]
fn matches_the_reference_on_fixed_graphs_for_every_mode_and_direction() {
    let dir = tempfile::tempdir().unwrap();
    // A diamond with a parallel twin, a self-loop and a back edge.
    let m = build(
        dir.path(),
        &[(0, Some(1)), (1, Some(5)), (0, None), (1, Some(2))],
        &[
            (0, 1, 0, Some(1)),
            (0, 1, 0, Some(7)),
            (0, 2, 1, Some(2)),
            (1, 3, 0, None),
            (2, 3, 1, Some(3)),
            (3, 3, 0, Some(1)),
            (3, 0, 1, Some(4)),
        ],
    );
    for mode in [PathMode::Walk, PathMode::Trail, PathMode::Acyclic] {
        for direction in [Direction::Outgoing, Direction::Incoming, Direction::Both] {
            for (min, max) in [
                (0, Some(0)),
                (0, Some(1)),
                (1, Some(2)),
                (2, Some(3)),
                (0, None),
            ] {
                if max.is_none() && mode == PathMode::Walk {
                    continue;
                }
                let e = TEdge {
                    direction,
                    ..out(Some(1))
                };
                let p = quantified_edge(e, min, max, mode);
                assert_matches_reference(&m, &p, &format!("{mode:?} {direction:?} {min} {max:?}"));
                let e = TEdge {
                    direction,
                    below: Some(5),
                    ..out(Some(1))
                };
                let mut p = quantified_edge(e, min, max, mode);
                p.states[3].below = Some(4);
                assert_matches_reference(&m, &p, "with predicates");
            }
        }
    }
}

/// A hub whose adjacency outgrows one refill of the walk (256 postings).
#[test]
fn a_hub_larger_than_one_refill_is_walked_whole() {
    let dir = tempfile::tempdir().unwrap();
    let mut edges = Vec::new();
    for i in 0..300 {
        edges.push((0, 1 + i % 3, 0, None));
    }
    edges.push((1, 2, 0, None));
    let m = build(dir.path(), &[(0, None); 4], &edges);
    let p = quantified_edge(out(Some(1)), 1, Some(2), PathMode::Trail);
    assert_eq!(from(run(&m, &p), m.nodes[0].id).len(), 300 + 100);
    assert_matches_reference(&m, &p, "hub");
}

// ── refusals when the execution opens ─────────────────────────────────────

fn open_error(m: &Model, p: &TPat) -> String {
    let mut host = TestHost::default();
    let plan = p.plan(m, &mut host);
    let opened = GqlCursor::open(&m.db, &host, &plan, Vec::new()).map(|_| ());
    match opened {
        Ok(()) => panic!("opened: {p:?}"),
        Err(e) => format!("{e:?}"),
    }
}

#[test]
fn malformed_or_unbounded_walk_automata_are_refused_at_open() {
    let dir = tempfile::tempdir().unwrap();
    let m = build(dir.path(), &[(0, None); 2], &[(0, 1, 0, None)]);

    let walk = quantified_edge(out(Some(1)), 1, None, PathMode::Walk);
    assert!(open_error(&m, &walk).contains("WALK"));

    // A repeat inside a repeat: overlapping ranges (Q10).
    let mut nested = quantified_edge(out(Some(1)), 1, Some(2), PathMode::Walk);
    nested.repeats.push(Repeat {
        first: 1,
        last: 2,
        min: 1,
        max: Some(2),
    });
    assert!(open_error(&m, &nested).contains("Q10"));

    // A group that crosses no edge would loop in place.
    let mut still = quantified_edge(out(Some(1)), 1, Some(2), PathMode::Walk);
    still.links[1] = TLink::Same;
    assert!(open_error(&m, &still).contains("crosses no edge"));

    // A repeat entered across an edge.
    let mut across = quantified_edge(out(Some(1)), 1, Some(2), PathMode::Walk);
    across.links[0] = TLink::Edge(out(None));
    assert!(open_error(&m, &across).contains("across an edge"));

    // A lower bound above the upper.
    let upside = quantified_edge(out(Some(1)), 3, Some(2), PathMode::Walk);
    assert!(open_error(&m, &upside).contains("below its lower bound"));

    // A slot outside the row.
    let mut wide = quantified_edge(out(Some(1)), 1, Some(2), PathMode::Walk);
    wide.path = Some(9);
    assert!(open_error(&m, &wide).contains("outside a row"));

    // A group slot not declared as one.
    let mut host = TestHost::default();
    let good = quantified_edge(out(Some(1)), 1, Some(2), PathMode::Walk);
    let mut automaton = good.automaton(&m, &mut host);
    automaton.groups = [].into();
    let plan = OpSpec::PathSearch {
        input: Box::new(OpSpec::Unit { width: 4 }),
        from: SlotId(0),
        automaton,
        search: PathSearch::Enumerate,
    };
    assert!(GqlCursor::open(&m.db, &host, &plan, Vec::new()).is_err());
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
    let m = random_graph(dir.path(), 7, 6, 12);
    let p = quantified_edge(out(Some(1)), 1, None, PathMode::Trail);
    let mut host = TestHost::default();
    let plan = p.plan(&m, &mut host);
    let unlimited = GqlBudget::unlimited();

    // The answer this pattern gives must be big enough to hit each limit.
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
                binding_rows: 20,
                ..unlimited
            },
            WorkResource::BindingRows,
            20,
        ),
        (
            GqlBudget {
                list_bytes: 64,
                ..unlimited
            },
            WorkResource::ListBytes,
            64,
        ),
    ] {
        let mut cursor = GqlCursor::open(&m.db, &host, &plan, Vec::new()).unwrap();
        let first = cursor.next_page(8192, budget, never);
        assert_eq!(refusal(first), (resource, limit));
        // Poisoned: the same refusal again, and never `done`.
        assert_eq!(
            refusal(cursor.next_page(8192, unlimited, never)),
            (resource, limit)
        );
    }

    // The work a search reports: states, rows, and the stack it held.
    let mut cursor = GqlCursor::open(&m.db, &host, &plan, Vec::new()).unwrap();
    let page = cursor.next_page(8192, unlimited, never).unwrap();
    assert!(page.done);
    assert_eq!(
        page.work.binding_rows as usize,
        m.nodes.len() + page.rows.len(),
        "one row per seed, one per match"
    );
    assert!(page.work.path_states >= page.rows.len() as u64);
    assert!(page.work.queue_entries > 0);
    assert!(page.work.base.graph_edges > 0);
}

#[test]
fn cancellation_stops_the_search_and_poisons_it() {
    let dir = tempfile::tempdir().unwrap();
    let m = random_graph(dir.path(), 3, 6, 12);
    let p = quantified_edge(out(Some(1)), 1, None, PathMode::Trail);
    let mut host = TestHost::default();
    let plan = p.plan(&m, &mut host);
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

#[test]
fn paging_equals_one_shot() {
    let dir = tempfile::tempdir().unwrap();
    let m = random_graph(dir.path(), 11, 6, 10);
    let mut rng = Rng(99);
    for _ in 0..6 {
        let p = random_pattern(&mut rng);
        let mut host = TestHost::default();
        let plan = p.plan(&m, &mut host);
        let one_shot = run_paged(&m, &host, &plan, 8192, GqlBudget::unlimited()).unwrap();
        for page_rows in [1, 2, 3, 7] {
            let paged = run_paged(&m, &host, &plan, page_rows, GqlBudget::unlimited()).unwrap();
            assert_eq!(paged, one_shot, "page size {page_rows}, pattern {p:?}");
        }
    }
}

/// The DFS stack and its sibling buffer are held across pages: a page that
/// cannot hold them again is refused before it does anything.
#[test]
fn the_held_stack_is_charged_again_to_each_page() {
    let dir = tempfile::tempdir().unwrap();
    let m = random_graph(dir.path(), 5, 6, 12);
    let p = quantified_edge(out(Some(1)), 1, None, PathMode::Trail);
    let mut host = TestHost::default();
    let plan = p.plan(&m, &mut host);
    let mut cursor = GqlCursor::open(&m.db, &host, &plan, Vec::new()).unwrap();
    let first = cursor.next_page(1, GqlBudget::unlimited(), never).unwrap();
    assert!(!first.done);
    assert!(first.work.queue_entries > 1);
    // No room for what is held, and no room for any new work: the held
    // stack must be what refuses, before the search takes a step.
    let tight = GqlBudget {
        queue_entries: 1,
        path_states: 0,
        binding_rows: 0,
        ..GqlBudget::unlimited()
    };
    assert_eq!(
        refusal(cursor.next_page(1, tight, never)),
        (WorkResource::QueueEntries, 1)
    );
}
