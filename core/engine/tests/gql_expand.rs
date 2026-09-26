//! The GQL pattern operators (`docs/lang/GQL_PROFILE_DESIGN.md` §3.1-§3.5,
//! §7): seeds, `Expand` / `ExpandInto`, `Filter`, `Let`, `Project`, and the
//! pull cursor that pages them.
//!
//! What is at risk, and the test that pins it:
//!
//! * that a pattern answer is the BAG a brute-force hop oracle computes on a
//!   random multigraph -- parallel edges, self-loops, cycles and several
//!   collections sharing an external key -- for every direction, edge-type
//!   set, far-label test and per-hop predicate (`matches_the_hop_oracle_*`);
//! * that an either-direction self-loop is ONE match and parallel edges are
//!   one match EACH (`a_self_loop_*`, `parallel_edges_*`);
//! * that a seed key with no row, a key of another collection and a `NULL`
//!   key all give an empty stream, never an error (`a_missing_seed_key_*`);
//! * that every budget refusal names its resource and its limit, the
//!   cursor is poisoned after it, and a stream cut short is never `done`
//!   (`each_budget_refusal_*`, `a_refused_page_poisons_*`, `cancellation_*`);
//! * that the pages of one cursor concatenate to the one-shot answer
//!   (`paging_equals_one_shot`);
//! * that reading an INCOMING edge's property costs its primary-posting read
//!   and an outgoing one costs nothing (`incoming_edge_property_access_*`).
//!
//! The expressions are the test's own: [`TestHost`] is a minimal
//! [`GqlHost`], standing in for the language layer.

use sekejap_core::collections::gql::{
    BindingRow, BindingValue, EdgeRef, EvalCx, ExprId, GqlBudget, GqlCursor, GqlHost, NodeRef,
    OpSpec, SeedId, SeedSource, SlotId, StepSpec, Target, Truth,
};
use sekejap_core::collections::PreparedQuery;
use sekejap_core::collections::{
    CandidateDriver, CollectionId, Database, Direction, EdgeKey, EdgeTypeId, EntityId,
    GraphContextId, IndexId, Projection, QueryBudget, QueryError, QueryFilter, QueryOrder,
    QueryRequest, QueryResult, ScalarFilter, ScalarValue, WorkResource,
};
use sekejap_core::Kind;
use serde_json::json;

mod common;
use common::cfg;

// ── a deterministic generator ─────────────────────────────────────────────

/// splitmix64: enough randomness for a fixture, reproducible from its seed.
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

// ── the test host: a tiny expression table ────────────────────────────────

/// An expression over a binding row. `NodeProp` and `EdgeProp` read through
/// the charged element reader, as the language layer's evaluator will.
#[derive(Clone, Debug)]
enum E {
    Lit(BindingValue),
    Param(usize),
    Slot(u16),
    NodeProp(u16, &'static str),
    EdgeProp(u16, &'static str),
    /// Three-valued `<`: `Null` on either side is unknown.
    Lt(Box<E>, Box<E>),
}

/// An index-candidate seed: scalar equality of `value` on `index`.
struct IndexSeed {
    collection: CollectionId,
    index: IndexId,
    value: E,
}

#[derive(Default)]
struct TestHost {
    exprs: Vec<E>,
    seeds: Vec<IndexSeed>,
}

impl TestHost {
    fn expr(&mut self, e: E) -> ExprId {
        self.exprs.push(e);
        ExprId(self.exprs.len() as u32 - 1)
    }

    fn value(&self, e: &E, row: &BindingRow, cx: &mut EvalCx<'_, '_>) -> QueryResult<BindingValue> {
        Ok(match e {
            E::Lit(v) => v.clone(),
            E::Param(n) => cx.params[*n].clone(),
            E::Slot(s) => row.get(SlotId(*s)).clone(),
            E::NodeProp(s, name) => match row.get(SlotId(*s)) {
                BindingValue::Node(n) => cx.reader.node_property(*n, name, cx.meter)?,
                _ => BindingValue::Null,
            },
            E::EdgeProp(s, name) => match row.get(SlotId(*s)) {
                BindingValue::Edge(edge) => cx.reader.edge_property(edge, name, cx.meter)?,
                _ => BindingValue::Null,
            },
            E::Lt(a, b) => {
                let (a, b) = (self.value(a, row, cx)?, self.value(b, row, cx)?);
                match (a, b) {
                    (BindingValue::Int(a), BindingValue::Int(b)) => BindingValue::Bool(a < b),
                    _ => BindingValue::Null,
                }
            }
        })
    }
}

impl GqlHost for TestHost {
    fn eval(
        &self,
        expr: ExprId,
        row: &BindingRow,
        cx: &mut EvalCx<'_, '_>,
    ) -> QueryResult<BindingValue> {
        self.value(&self.exprs[expr.0 as usize], row, cx)
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
        seed: SeedId,
        db: &'db Database,
        row: &BindingRow,
        cx: &mut EvalCx<'_, '_>,
    ) -> QueryResult<Option<PreparedQuery<'db>>> {
        let seed = &self.seeds[seed.0 as usize];
        let BindingValue::Int(value) = self.value(&seed.value, row, cx)? else {
            return Ok(None);
        };
        let filters = [QueryFilter::Scalar {
            index: seed.index,
            predicate: ScalarFilter::Eq(ScalarValue::I64(value)),
        }];
        db.prepare_query(QueryRequest {
            collection: seed.collection,
            filters: &filters,
            order: QueryOrder::Driver,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .map(Some)
    }
}

// ── the fixture and its in-memory model ───────────────────────────────────

#[derive(Clone, Debug)]
struct ModelNode {
    id: EntityId,
    key: String,
    x: Option<i64>,
}

#[derive(Clone, Debug)]
struct ModelEdge {
    key: EdgeKey,
    id: u64,
    w: Option<i64>,
}

struct Graph {
    db: Database,
    /// Three collections; `a` and `b` share keys `k0..k3`, `c` shares
    /// `k2..k5` with `a`.
    cols: [CollectionId; 3],
    types: [EdgeTypeId; 2],
    x_index: IndexId,
    nodes: Vec<ModelNode>,
    edges: Vec<ModelEdge>,
}

fn keys_of(collection: usize) -> std::ops::Range<usize> {
    match collection {
        0 => 0..6,
        1 => 0..4,
        _ => 2..7,
    }
}

/// A random multigraph: every node may get edges to any node (itself
/// included) of any collection, with parallel edges and self-loops forced in.
/// `hub` more edges touch the first node, so its adjacency outgrows one
/// refill of the `Expand` walk.
fn graph(dir: &std::path::Path, seed: u64, edges: usize, hub: usize) -> Graph {
    let mut rng = Rng(seed);
    let mut db = Database::create(dir.join("g.sekejap"), cfg()).unwrap();
    let mut cols = Vec::new();
    for name in ["site_a", "site_b", "site_c"] {
        cols.push(
            db.create_collection(name, vec![("x".into(), Kind::Int)], Default::default())
                .unwrap(),
        );
    }
    let cols = [cols[0], cols[1], cols[2]];
    db.enable_graph().unwrap();
    let types = [
        db.create_edge_type("t1").unwrap(),
        db.create_edge_type("t2").unwrap(),
    ];
    let mut nodes = Vec::new();
    for (c, &collection) in cols.iter().enumerate() {
        for k in keys_of(c) {
            let key = format!("k{k}");
            // One node in five has no `x` at all.
            let x = (rng.below(5) != 0).then(|| rng.below(10) as i64);
            let doc = match x {
                Some(x) => json!({ "x": x }),
                None => json!({}),
            };
            let id = db.put(collection, &key, &doc).unwrap();
            nodes.push(ModelNode { id, key, x });
        }
    }
    let x_index = db
        .create_scalar_index(cols[0], "site_a_x", "x", false)
        .unwrap();
    while !db.build_index_step(x_index, 64).unwrap() {
        db.commit().unwrap();
    }
    let mut model = Vec::new();
    let mut add =
        |db: &mut Database, src: EntityId, dst: EntityId, t: EdgeTypeId, rng: &mut Rng| {
            let w = (rng.below(4) != 0).then(|| rng.below(10) as i64);
            let bag = match w {
                Some(w) => json!({ "w": w }),
                None => json!({}),
            };
            let made = db
                .create_edge(GraphContextId::BASE, src, t, dst, &bag)
                .unwrap();
            model.push(ModelEdge {
                key: made.key,
                id: made.id,
                w,
            });
        };
    for _ in 0..edges {
        let src = nodes[rng.below(nodes.len() as u64) as usize].id;
        let dst = nodes[rng.below(nodes.len() as u64) as usize].id;
        let t = types[rng.below(2) as usize];
        add(&mut db, src, dst, t, &mut rng);
        // One edge in six gets a parallel twin, one in eight a self-loop.
        if rng.below(6) == 0 {
            add(&mut db, src, dst, t, &mut rng);
        }
        if rng.below(8) == 0 {
            add(&mut db, src, src, t, &mut rng);
        }
    }
    for _ in 0..hub {
        let other = nodes[rng.below(nodes.len() as u64) as usize].id;
        let t = types[rng.below(2) as usize];
        let (src, dst) = if rng.below(2) == 0 {
            (nodes[0].id, other)
        } else {
            (other, nodes[0].id)
        };
        add(&mut db, src, dst, t, &mut rng);
    }
    db.commit().unwrap();
    Graph {
        db,
        cols,
        types,
        x_index,
        nodes,
        edges: model,
    }
}

impl Graph {
    fn node(&self, id: EntityId) -> &ModelNode {
        self.nodes.iter().find(|n| n.id == id).unwrap()
    }
}

// ── the oracle ────────────────────────────────────────────────────────────

/// One hop's pattern, as the oracle reads it.
#[derive(Clone)]
struct Hop {
    direction: Direction,
    types: Option<Vec<EdgeTypeId>>,
    far_labels: Option<Vec<CollectionId>>,
    /// `e.w < k`.
    edge_below: Option<i64>,
    /// `far.x < k`.
    far_below: Option<i64>,
}

/// Every (edge, far node) one hop from `near` admits, by brute force over
/// the edge list: an either-direction walk sees a self-loop once.
fn oracle_hop(g: &Graph, near: EntityId, hop: &Hop) -> Vec<(ModelEdge, EntityId)> {
    let mut out = Vec::new();
    for e in &g.edges {
        let (s, d) = (e.key.source, e.key.destination);
        let mut fars = Vec::new();
        match hop.direction {
            Direction::Outgoing if s == near => fars.push(d),
            Direction::Incoming if d == near => fars.push(s),
            Direction::Both => {
                if s == near {
                    fars.push(d);
                }
                if d == near && s != d {
                    fars.push(s);
                }
            }
            _ => {}
        }
        for far in fars {
            if hop
                .types
                .as_ref()
                .is_some_and(|t| !t.contains(&e.key.edge_type))
            {
                continue;
            }
            if hop
                .far_labels
                .as_ref()
                .is_some_and(|l| !l.contains(&far.collection))
            {
                continue;
            }
            if let Some(k) = hop.edge_below {
                if !e.w.is_some_and(|w| w < k) {
                    continue;
                }
            }
            if let Some(k) = hop.far_below {
                if !g.node(far).x.is_some_and(|x| x < k) {
                    continue;
                }
            }
            out.push((e.clone(), far));
        }
    }
    out
}

fn node(id: EntityId) -> BindingValue {
    BindingValue::Node(NodeRef(id))
}

fn edge(e: &ModelEdge) -> BindingValue {
    BindingValue::Edge(EdgeRef {
        key: e.key,
        id: e.id,
        bag: None,
    })
}

/// Rows as a sorted bag: equal rows stay, one per occurrence.
fn bag(mut rows: Vec<Vec<BindingValue>>) -> Vec<Vec<BindingValue>> {
    rows.sort();
    rows
}

// ── plan helpers ──────────────────────────────────────────────────────────

fn step(host: &mut TestHost, hop: &Hop, edge_slot: u16, far_slot: u16) -> StepSpec {
    StepSpec {
        context: GraphContextId::BASE,
        types: hop.types.clone().map(Vec::into_boxed_slice),
        direction: hop.direction,
        edge_filter: hop.edge_below.map(|k| {
            host.expr(E::Lt(
                Box::new(E::EdgeProp(edge_slot, "w")),
                Box::new(E::Lit(BindingValue::Int(k))),
            ))
        }),
        far_labels: hop.far_labels.clone().map(Vec::into_boxed_slice),
        far_filter: hop.far_below.map(|k| {
            host.expr(E::Lt(
                Box::new(E::NodeProp(far_slot, "x")),
                Box::new(E::Lit(BindingValue::Int(k))),
            ))
        }),
    }
}

fn unit(width: u16) -> Box<OpSpec> {
    Box::new(OpSpec::Unit { width })
}

fn key_seed(input: Box<OpSpec>, out: u16, key: ExprId, labels: &[CollectionId]) -> Box<OpSpec> {
    Box::new(OpSpec::Seed {
        input,
        out: SlotId(out),
        source: SeedSource::Key {
            key,
            labels: labels.into(),
        },
    })
}

fn expand(input: Box<OpSpec>, from: u16, edge: u16, to: Target, step: StepSpec) -> Box<OpSpec> {
    Box::new(OpSpec::Expand {
        input,
        from: SlotId(from),
        edge: Some(SlotId(edge)),
        to,
        step,
    })
}

/// `RETURN` every slot `0..width`, in order.
fn project_all(host: &mut TestHost, input: Box<OpSpec>, width: u16) -> OpSpec {
    let cols: Vec<ExprId> = (0..width).map(|s| host.expr(E::Slot(s))).collect();
    OpSpec::Project {
        input,
        cols: cols.into(),
        width,
    }
}

fn never() -> bool {
    false
}

/// Every row of one cursor, pulled in pages of `page_rows`.
fn run(
    db: &Database,
    host: &TestHost,
    plan: &OpSpec,
    params: Vec<BindingValue>,
    page_rows: usize,
) -> QueryResult<Vec<Vec<BindingValue>>> {
    let mut cursor = GqlCursor::open(db, host, plan, params)?;
    let mut rows = Vec::new();
    loop {
        let page = cursor.next_page(page_rows, GqlBudget::unlimited(), never)?;
        assert!(page.rows.len() <= page_rows);
        rows.extend(page.rows.into_iter().map(|r| r.slots.into_vec()));
        if page.done {
            return Ok(rows);
        }
    }
}

fn text(s: &str) -> BindingValue {
    BindingValue::Text(s.into())
}

// ── the oracle comparisons ────────────────────────────────────────────────

fn hop_choices(g: &Graph, rng: &mut Rng) -> Hop {
    let direction =
        [Direction::Outgoing, Direction::Incoming, Direction::Both][rng.below(3) as usize];
    let types = match rng.below(4) {
        0 => None,
        1 => Some(vec![g.types[0]]),
        2 => Some(vec![g.types[1], g.types[0]]),
        _ => Some(vec![g.types[1]]),
    };
    let far_labels = match rng.below(3) {
        0 => None,
        1 => Some(vec![g.cols[1]]),
        _ => Some(vec![g.cols[2], g.cols[0]]),
    };
    Hop {
        direction,
        types,
        far_labels,
        edge_below: (rng.below(3) == 0).then(|| rng.below(10) as i64),
        far_below: (rng.below(3) == 0).then(|| rng.below(10) as i64),
    }
}

/// `MATCH (a IS <labels> WHERE a._key = $1) -[e1]- (b) -[e2]- (c)
///  RETURN a, e1, b, e2, c`, for random hop shapes, against the oracle.
#[test]
fn matches_the_hop_oracle_on_random_multigraphs_as_bags() {
    for seed in 0..6u64 {
        let dir = tempfile::tempdir().unwrap();
        let g = graph(dir.path(), seed, 40, 0);
        let mut rng = Rng(seed ^ 0xA5A5);
        for _ in 0..12 {
            let labels: Vec<CollectionId> = match rng.below(3) {
                0 => vec![g.cols[0]],
                1 => vec![g.cols[0], g.cols[1]],
                _ => vec![g.cols[2], g.cols[1], g.cols[0]],
            };
            let key = format!("k{}", rng.below(7));
            let (h1, h2) = (hop_choices(&g, &mut rng), hop_choices(&g, &mut rng));

            let mut host = TestHost::default();
            let k = host.expr(E::Param(0));
            let s1 = step(&mut host, &h1, 1, 2);
            let s2 = step(&mut host, &h2, 3, 4);
            let plan = key_seed(unit(5), 0, k, &labels);
            let plan = expand(plan, 0, 1, Target::New(SlotId(2)), s1);
            let plan = expand(plan, 2, 3, Target::New(SlotId(4)), s2);
            let plan = project_all(&mut host, plan, 5);
            let got = run(&g.db, &host, &plan, vec![text(&key)], 64).unwrap();

            let mut want = Vec::new();
            for &c in &labels {
                for a in g
                    .nodes
                    .iter()
                    .filter(|n| n.id.collection == c && n.key == key)
                {
                    for (e1, b) in oracle_hop(&g, a.id, &h1) {
                        for (e2, c) in oracle_hop(&g, b, &h2) {
                            want.push(vec![node(a.id), edge(&e1), node(b), edge(&e2), node(c)]);
                        }
                    }
                }
            }
            assert_eq!(bag(got), bag(want), "seed {seed}, key {key}");
        }
    }
}

/// `ExpandInto`: both ends bound by key seeds, then the edges between them.
/// Parallel edges give a row each.
#[test]
fn matches_the_hop_oracle_for_expand_into() {
    for seed in 10..14u64 {
        let dir = tempfile::tempdir().unwrap();
        let g = graph(dir.path(), seed, 60, 0);
        let mut rng = Rng(seed);
        for _ in 0..16 {
            let (ka, kc) = (format!("k{}", rng.below(7)), format!("k{}", rng.below(7)));
            let h = Hop {
                far_labels: None,
                ..hop_choices(&g, &mut rng)
            };
            let mut host = TestHost::default();
            let (pa, pc) = (host.expr(E::Param(0)), host.expr(E::Param(1)));
            let s = step(&mut host, &h, 1, 2);
            let plan = key_seed(unit(3), 0, pa, &g.cols);
            let plan = key_seed(plan, 2, pc, &g.cols);
            let plan = expand(plan, 0, 1, Target::Bound(SlotId(2)), s);
            let plan = project_all(&mut host, plan, 3);
            let got = run(&g.db, &host, &plan, vec![text(&ka), text(&kc)], 16).unwrap();

            let mut want = Vec::new();
            for a in g.nodes.iter().filter(|n| n.key == ka) {
                for c in g.nodes.iter().filter(|n| n.key == kc) {
                    for (e, far) in oracle_hop(&g, a.id, &h) {
                        if far == c.id {
                            want.push(vec![node(a.id), edge(&e), node(c.id)]);
                        }
                    }
                }
            }
            assert_eq!(bag(got), bag(want), "seed {seed}, {ka} -> {kc}");
        }
    }
}

/// A label scan seed and an index seed agree with the oracle, and so do a
/// `Bound` re-seed with a label test, `LET` and `FILTER` after the pattern.
#[test]
fn matches_the_hop_oracle_for_scan_index_and_bound_seeds() {
    for seed in 20..24u64 {
        let dir = tempfile::tempdir().unwrap();
        let g = graph(dir.path(), seed, 40, 0);
        let mut rng = Rng(seed);
        let h = hop_choices(&g, &mut rng);

        // MATCH (a IS site_a | site_c) -[e]- (b) RETURN a, e, b
        let mut host = TestHost::default();
        let s = step(&mut host, &h, 1, 2);
        let scan = Box::new(OpSpec::Seed {
            input: unit(3),
            out: SlotId(0),
            source: SeedSource::Scan {
                labels: [g.cols[0], g.cols[2]].into(),
            },
        });
        let plan = expand(scan, 0, 1, Target::New(SlotId(2)), s);
        let plan = project_all(&mut host, plan, 3);
        let got = run(&g.db, &host, &plan, vec![], 7).unwrap();
        let mut want = Vec::new();
        for a in g
            .nodes
            .iter()
            .filter(|n| n.id.collection == g.cols[0] || n.id.collection == g.cols[2])
        {
            for (e, b) in oracle_hop(&g, a.id, &h) {
                want.push(vec![node(a.id), edge(&e), node(b)]);
            }
        }
        assert_eq!(bag(got), bag(want), "scan, seed {seed}");

        // MATCH (a IS site_a WHERE a.x = $1) -[e]- (b) RETURN a, e, b, via
        // the index on site_a.x.
        for x in 0..10i64 {
            let mut host = TestHost::default();
            host.seeds.push(IndexSeed {
                collection: g.cols[0],
                index: g.x_index,
                value: E::Param(0),
            });
            let s = step(&mut host, &h, 1, 2);
            let seeded = Box::new(OpSpec::Seed {
                input: unit(3),
                out: SlotId(0),
                source: SeedSource::Index { seed: SeedId(0) },
            });
            let plan = expand(seeded, 0, 1, Target::New(SlotId(2)), s);
            let plan = project_all(&mut host, plan, 3);
            let got = run(&g.db, &host, &plan, vec![BindingValue::Int(x)], 5).unwrap();
            let mut want = Vec::new();
            for a in g
                .nodes
                .iter()
                .filter(|n| n.id.collection == g.cols[0] && n.x == Some(x))
            {
                for (e, b) in oracle_hop(&g, a.id, &h) {
                    want.push(vec![node(a.id), edge(&e), node(b)]);
                }
            }
            assert_eq!(bag(got), bag(want), "index x = {x}, seed {seed}");
        }

        // MATCH (a WHERE a._key = $1) ... (a IS site_b) LET v = a.x, w = v
        // FILTER v < 5 RETURN a, v, w: the re-seed keeps only the site_b
        // node, LET reads x -- and `w` does not see `v`, so it is null --
        // and FILTER drops a null or large `v`.
        for k in 0..7 {
            let key = format!("k{k}");
            let mut host = TestHost::default();
            let p = host.expr(E::Param(0));
            let plan = key_seed(unit(3), 0, p, &g.cols);
            let plan = Box::new(OpSpec::Seed {
                input: plan,
                out: SlotId(0),
                source: SeedSource::Bound {
                    slot: SlotId(0),
                    labels: Some([g.cols[1]].into()),
                },
            });
            let x = host.expr(E::NodeProp(0, "x"));
            let v = host.expr(E::Slot(1));
            let plan = Box::new(OpSpec::Let {
                input: plan,
                assign: [(SlotId(1), x), (SlotId(2), v)].into(),
            });
            let below = host.expr(E::Lt(
                Box::new(E::Slot(1)),
                Box::new(E::Lit(BindingValue::Int(5))),
            ));
            let plan = Box::new(OpSpec::Filter {
                input: plan,
                predicate: below,
            });
            let plan = project_all(&mut host, plan, 3);
            let got = run(&g.db, &host, &plan, vec![text(&key)], 3).unwrap();
            let want: Vec<Vec<BindingValue>> = g
                .nodes
                .iter()
                .filter(|n| n.id.collection == g.cols[1] && n.key == key)
                .filter_map(|n| {
                    n.x.filter(|x| *x < 5)
                        .map(|x| vec![node(n.id), BindingValue::Int(x), BindingValue::Null])
                })
                .collect();
            assert_eq!(bag(got), bag(want), "bound {key}, seed {seed}");
        }
    }
}

// ── the matrix rows, one each ─────────────────────────────────────────────

struct Small {
    db: Database,
    person: CollectionId,
    knows: EdgeTypeId,
    p1: EntityId,
    parallel: [u64; 2],
    looped: u64,
}

/// `p1 -knows-> p2` twice (parallel), and `p1 -knows-> p1` once.
fn small(dir: &std::path::Path) -> Small {
    let mut db = Database::create(dir.join("s.sekejap"), cfg()).unwrap();
    let person = db
        .create_collection("person", vec![("x".into(), Kind::Int)], Default::default())
        .unwrap();
    db.enable_graph().unwrap();
    let knows = db.create_edge_type("knows").unwrap();
    let p1 = db.put(person, "p1", &json!({ "x": 1 })).unwrap();
    let p2 = db.put(person, "p2", &json!({ "x": 2 })).unwrap();
    let a = db
        .create_edge(GraphContextId::BASE, p1, knows, p2, &json!({ "w": 1 }))
        .unwrap();
    let b = db
        .create_edge(GraphContextId::BASE, p1, knows, p2, &json!({ "w": 2 }))
        .unwrap();
    let l = db
        .create_edge(GraphContextId::BASE, p1, knows, p1, &json!({ "w": 3 }))
        .unwrap();
    db.commit().unwrap();
    Small {
        db,
        person,
        knows,
        p1,
        parallel: [a.id, b.id],
        looped: l.id,
    }
}

/// `MATCH (a WHERE a._key = $1) -[e]-(dir) (b) RETURN a, e, b`.
fn one_hop(s: &Small, host: &mut TestHost, direction: Direction, edge_filter: bool) -> OpSpec {
    let k = host.expr(E::Param(0));
    let hop = Hop {
        direction,
        types: Some(vec![s.knows]),
        far_labels: None,
        edge_below: edge_filter.then_some(100),
        far_below: None,
    };
    let st = step(host, &hop, 1, 2);
    let plan = key_seed(unit(3), 0, k, &[s.person]);
    let plan = expand(plan, 0, 1, Target::New(SlotId(2)), st);
    project_all(host, plan, 3)
}

fn edge_ids(rows: &[Vec<BindingValue>]) -> Vec<u64> {
    let mut ids: Vec<u64> = rows
        .iter()
        .map(|r| match &r[1] {
            BindingValue::Edge(e) => e.id,
            other => panic!("not an edge: {other:?}"),
        })
        .collect();
    ids.sort_unstable();
    ids
}

#[test]
fn a_self_loop_matches_once_in_either_direction() {
    let dir = tempfile::tempdir().unwrap();
    let s = small(dir.path());
    let mut host = TestHost::default();
    let plan = one_hop(&s, &mut host, Direction::Both, false);
    let rows = run(&s.db, &host, &plan, vec![text("p1")], 64).unwrap();
    let mut want = vec![s.parallel[0], s.parallel[1], s.looped];
    want.sort_unstable();
    // Two parallel edges out, the loop ONCE -- not once per orientation.
    assert_eq!(edge_ids(&rows), want, "{rows:?}");
    let loops = rows.iter().filter(|r| r[2] == node(s.p1)).count();
    assert_eq!(loops, 1);
}

/// An edge type unknown when the execution opened is planned as an empty
/// type list: it matches nothing, and walks nothing.
#[test]
fn an_empty_type_list_matches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let s = small(dir.path());
    let mut host = TestHost::default();
    let k = host.expr(E::Param(0));
    let hop = Hop {
        direction: Direction::Both,
        types: Some(vec![]),
        far_labels: None,
        edge_below: None,
        far_below: None,
    };
    let st = step(&mut host, &hop, 1, 2);
    let plan = key_seed(unit(3), 0, k, &[s.person]);
    let plan = expand(plan, 0, 1, Target::New(SlotId(2)), st);
    let plan = project_all(&mut host, plan, 3);
    let mut cursor = GqlCursor::open(&s.db, &host, &plan, vec![text("p1")]).unwrap();
    let page = cursor.next_page(8, GqlBudget::unlimited(), never).unwrap();
    assert!(page.rows.is_empty() && page.done);
    assert_eq!(page.work.base.graph_edges, 0);
}

#[test]
fn parallel_edges_are_one_match_each() {
    let dir = tempfile::tempdir().unwrap();
    let s = small(dir.path());
    let mut host = TestHost::default();
    let plan = one_hop(&s, &mut host, Direction::Incoming, false);
    let rows = run(&s.db, &host, &plan, vec![text("p2")], 64).unwrap();
    assert_eq!(edge_ids(&rows), s.parallel.to_vec());
    assert!(rows.iter().all(|r| r[2] == node(s.p1)));
}

#[test]
fn a_missing_seed_key_gives_an_empty_stream_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let s = small(dir.path());
    for key in [text("nobody"), BindingValue::Null] {
        let mut host = TestHost::default();
        let plan = one_hop(&s, &mut host, Direction::Outgoing, false);
        let mut cursor = GqlCursor::open(&s.db, &host, &plan, vec![key.clone()]).unwrap();
        let page = cursor.next_page(8, GqlBudget::unlimited(), never).unwrap();
        assert!(page.rows.is_empty(), "{key:?}");
        assert!(page.done, "{key:?}");
    }
}

#[test]
fn a_key_of_another_collection_gives_an_empty_stream() {
    let dir = tempfile::tempdir().unwrap();
    let g = graph(dir.path(), 99, 10, 0);
    // `k5` is in site_a and site_c, never in site_b.
    let mut host = TestHost::default();
    let k = host.expr(E::Param(0));
    let plan = key_seed(unit(1), 0, k, &[g.cols[1]]);
    let plan = project_all(&mut host, plan, 1);
    assert!(run(&g.db, &host, &plan, vec![text("k5")], 4)
        .unwrap()
        .is_empty());
    // And with the alternation, one node per collection that holds it.
    let mut host = TestHost::default();
    let k = host.expr(E::Param(0));
    let plan = key_seed(unit(1), 0, k, &g.cols);
    let plan = project_all(&mut host, plan, 1);
    let rows = run(&g.db, &host, &plan, vec![text("k3")], 4).unwrap();
    let collections: Vec<CollectionId> = rows
        .iter()
        .map(|r| match &r[0] {
            BindingValue::Node(n) => n.0.collection,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(collections, g.cols.to_vec());
}

#[test]
fn incoming_edge_property_access_is_charged_and_outgoing_is_free() {
    let dir = tempfile::tempdir().unwrap();
    let s = small(dir.path());
    let work = |direction: Direction, key: &str| {
        let mut host = TestHost::default();
        let plan = one_hop(&s, &mut host, direction, true);
        let mut cursor = GqlCursor::open(&s.db, &host, &plan, vec![text(key)]).unwrap();
        let page = cursor.next_page(64, GqlBudget::unlimited(), never).unwrap();
        assert!(page.done);
        assert_eq!(page.rows.len(), 2);
        page.work
    };
    // Into p2: the two parallel edges.
    let incoming = work(Direction::Incoming, "p2");
    // Two postings, the terminal probe, and one primary-posting read per
    // incoming edge whose `w` the filter reads.
    assert_eq!(incoming.base.graph_edges, 2 + 1 + 2);
    assert_eq!(incoming.base.primary_reads, 0);
    assert_eq!(incoming.base.key_postings, 1);
    assert_eq!(incoming.binding_rows, 1 + 2);

    // Outgoing from p1 walks three postings (two parallel, one loop): the
    // bag rides in the posting, so the filter reads nothing more.
    let mut host = TestHost::default();
    let plan = one_hop(&s, &mut host, Direction::Outgoing, true);
    let mut cursor = GqlCursor::open(&s.db, &host, &plan, vec![text("p1")]).unwrap();
    let page = cursor.next_page(64, GqlBudget::unlimited(), never).unwrap();
    assert_eq!(page.rows.len(), 3);
    assert_eq!(page.work.base.graph_edges, 3 + 1);
}

// ── budgets, poisoning, cancellation, paging ──────────────────────────────

fn refusal(result: QueryResult<impl std::fmt::Debug>) -> (WorkResource, u64) {
    match result {
        Err(QueryError::BudgetExceeded {
            resource, limit, ..
        }) => (resource, limit),
        other => panic!("expected a budget refusal, got {other:?}"),
    }
}

#[test]
fn each_budget_refusal_names_its_resource_and_limit() {
    let dir = tempfile::tempdir().unwrap();
    let s = small(dir.path());
    let first = |budget: GqlBudget, far_filter: bool| {
        let mut host = TestHost::default();
        let k = host.expr(E::Param(0));
        let hop = Hop {
            direction: Direction::Outgoing,
            types: None,
            far_labels: None,
            edge_below: None,
            far_below: far_filter.then_some(100),
        };
        let st = step(&mut host, &hop, 1, 2);
        let plan = key_seed(unit(3), 0, k, &[s.person]);
        let plan = expand(plan, 0, 1, Target::New(SlotId(2)), st);
        let plan = project_all(&mut host, plan, 3);
        let mut cursor = GqlCursor::open(&s.db, &host, &plan, vec![text("p1")]).unwrap();
        cursor.next_page(64, budget, never).map(|p| p.rows.len())
    };
    let base = |f: fn(&mut QueryBudget)| {
        let mut b = QueryBudget::unlimited();
        f(&mut b);
        GqlBudget::from_query_budget(b)
    };
    assert_eq!(
        refusal(first(
            GqlBudget {
                binding_rows: 2,
                ..GqlBudget::unlimited()
            },
            false
        )),
        (WorkResource::BindingRows, 2)
    );
    assert_eq!(
        refusal(first(base(|b| b.graph_edges = 2), false)),
        (WorkResource::GraphEdges, 2)
    );
    assert_eq!(
        refusal(first(base(|b| b.key_postings = 0), false)),
        (WorkResource::KeyPostings, 0)
    );
    // The far-node predicate reads the far row: one primary read per edge.
    assert_eq!(
        refusal(first(base(|b| b.primary_reads = 1), true)),
        (WorkResource::PrimaryReads, 1)
    );
    // Unrefused, the same query answers all three edges.
    assert_eq!(first(GqlBudget::unlimited(), true).unwrap(), 3);

    // A scan seed pages the engine's own entity walk under what is LEFT of
    // the page's budget; its refusal still names the whole page's limit.
    let mut host = TestHost::default();
    let plan = Box::new(OpSpec::Seed {
        input: unit(1),
        out: SlotId(0),
        source: SeedSource::Scan {
            labels: [s.person].into(),
        },
    });
    let plan = project_all(&mut host, plan, 1);
    let mut cursor = GqlCursor::open(&s.db, &host, &plan, vec![]).unwrap();
    assert_eq!(
        refusal(cursor.next_page(64, base(|b| b.primary_reads = 1), never)),
        (WorkResource::PrimaryReads, 1)
    );

    // The same when the page had spent some of it BEFORE the engine walk:
    // scan site_a, read each row's `x`, then scan site_b -- whose walk is
    // refused against the page's limit, not against the remainder it was
    // handed.
    let dir = tempfile::tempdir().unwrap();
    let g = graph(dir.path(), 5, 0, 0);
    let mut host = TestHost::default();
    let plan = Box::new(OpSpec::Seed {
        input: unit(2),
        out: SlotId(0),
        source: SeedSource::Scan {
            labels: [g.cols[0], g.cols[1]].into(),
        },
    });
    let x = host.expr(E::NodeProp(0, "x"));
    let plan = Box::new(OpSpec::Let {
        input: plan,
        assign: [(SlotId(1), x)].into(),
    });
    let plan = project_all(&mut host, plan, 2);
    let mut cursor = GqlCursor::open(&g.db, &host, &plan, vec![]).unwrap();
    let page = cursor.next_page(64, GqlBudget::unlimited(), never).unwrap();
    let spent = page.work.base.primary_reads;
    assert_eq!(page.rows.len(), 10);
    // site_b's four rows are read last, after its walk: a limit four
    // short, and two more, falls inside that walk.
    let limit = spent - 6;
    let mut cursor = GqlCursor::open(&g.db, &host, &plan, vec![]).unwrap();
    match cursor.next_page(64, base_primary(limit), never) {
        Err(QueryError::BudgetExceeded {
            resource: WorkResource::PrimaryReads,
            limit: named,
            attempted,
        }) => {
            assert_eq!(named, limit);
            assert!(attempted > limit, "{attempted} <= {limit}");
        }
        other => panic!("expected a primary-read refusal, got {other:?}"),
    }
}

fn base_primary(limit: u64) -> GqlBudget {
    GqlBudget::from_query_budget(QueryBudget {
        primary_reads: limit,
        ..QueryBudget::unlimited()
    })
}

#[test]
fn a_refused_page_poisons_the_cursor_and_the_stream_is_never_done() {
    let dir = tempfile::tempdir().unwrap();
    let s = small(dir.path());
    let mut host = TestHost::default();
    let plan = one_hop(&s, &mut host, Direction::Outgoing, false);
    let mut cursor = GqlCursor::open(&s.db, &host, &plan, vec![text("p1")]).unwrap();
    let page = cursor.next_page(1, GqlBudget::unlimited(), never).unwrap();
    assert_eq!(page.rows.len(), 1);
    assert!(!page.done);
    let tight = GqlBudget {
        binding_rows: 0,
        ..GqlBudget::unlimited()
    };
    let first = refusal(cursor.next_page(1, tight, never));
    assert_eq!(first, (WorkResource::BindingRows, 0));
    // Later pages, even with a generous budget, repeat the refusal: the rows
    // already returned are an incomplete answer, and nothing says done.
    for _ in 0..2 {
        assert_eq!(
            refusal(cursor.next_page(64, GqlBudget::unlimited(), never)),
            first
        );
    }
}

#[test]
fn cancellation_stops_the_cursor_and_poisons_it() {
    let dir = tempfile::tempdir().unwrap();
    let g = graph(dir.path(), 7, 60, 0);
    let mut host = TestHost::default();
    let hop = Hop {
        direction: Direction::Both,
        types: None,
        far_labels: None,
        edge_below: None,
        far_below: None,
    };
    let st = step(&mut host, &hop, 1, 2);
    let scan = Box::new(OpSpec::Seed {
        input: unit(3),
        out: SlotId(0),
        source: SeedSource::Scan {
            labels: g.cols.into(),
        },
    });
    let plan = expand(scan, 0, 1, Target::New(SlotId(2)), st);
    let plan = project_all(&mut host, plan, 3);
    let mut cursor = GqlCursor::open(&g.db, &host, &plan, vec![]).unwrap();
    let mut asked = 0;
    let result = cursor.next_page(8192, GqlBudget::unlimited(), || {
        asked += 1;
        asked > 5
    });
    assert!(matches!(result, Err(QueryError::Cancelled)), "{result:?}");
    assert!(matches!(
        cursor.next_page(8192, GqlBudget::unlimited(), never),
        Err(QueryError::Cancelled)
    ));
}

/// `MATCH (hub WHERE hub._key = 'k0') -[e1]- (b) -[e2:t1]-> (c IS site_b)`:
/// the hub's adjacency spans several refills of the first hop, and the
/// second hop pauses inside the first one's.
#[test]
fn paging_equals_one_shot() {
    let directions = [Direction::Outgoing, Direction::Incoming, Direction::Both];
    for (seed, direction) in (30..33u64).zip(directions) {
        let dir = tempfile::tempdir().unwrap();
        let g = graph(dir.path(), seed, 200, 700);
        let mut rng = Rng(seed);
        // Every type in one range: each direction of the hub holds ~350
        // postings, more than one refill.
        let h1 = Hop {
            direction,
            types: None,
            ..hop_choices(&g, &mut rng)
        };
        let h2 = Hop {
            direction: Direction::Outgoing,
            types: Some(vec![g.types[0]]),
            far_labels: Some(vec![g.cols[1]]),
            edge_below: None,
            far_below: None,
        };
        let mut host = TestHost::default();
        let k = host.expr(E::Lit(text("k0")));
        let s1 = step(&mut host, &h1, 1, 2);
        let s2 = step(&mut host, &h2, 3, 4);
        let plan = key_seed(unit(5), 0, k, &[g.cols[0]]);
        let plan = expand(plan, 0, 1, Target::New(SlotId(2)), s1);
        let plan = expand(plan, 2, 3, Target::New(SlotId(4)), s2);
        let plan = project_all(&mut host, plan, 5);
        let one_shot = run(&g.db, &host, &plan, vec![], 8192).unwrap();
        assert!(one_shot.len() > 256, "{} rows", one_shot.len());
        for page_rows in [1, 3, 256, 257] {
            assert_eq!(
                run(&g.db, &host, &plan, vec![], page_rows).unwrap(),
                one_shot,
                "seed {seed}, pages of {page_rows}"
            );
        }
        // Against the oracle too: a refill neither drops nor repeats a
        // posting.
        let hub = g.nodes[0].id;
        let mut want = Vec::new();
        for (e1, b) in oracle_hop(&g, hub, &h1) {
            for (e2, c) in oracle_hop(&g, b, &h2) {
                want.push(vec![node(hub), edge(&e1), node(b), edge(&e2), node(c)]);
            }
        }
        assert_eq!(bag(one_shot), bag(want), "seed {seed}");
    }
}

#[test]
fn a_malformed_plan_is_refused_at_open() {
    let dir = tempfile::tempdir().unwrap();
    let s = small(dir.path());
    let host = TestHost::default();
    // Slot 3 in a row of width 2.
    let plan = OpSpec::Seed {
        input: unit(2),
        out: SlotId(3),
        source: SeedSource::Scan {
            labels: [s.person].into(),
        },
    };
    assert!(GqlCursor::open(&s.db, &host, &plan, vec![]).is_err());
    // An edge predicate with no edge slot to read it from.
    let mut host = TestHost::default();
    let f = host.expr(E::Lit(BindingValue::Bool(true)));
    let plan = OpSpec::Expand {
        input: Box::new(OpSpec::Seed {
            input: unit(2),
            out: SlotId(0),
            source: SeedSource::Scan {
                labels: [s.person].into(),
            },
        }),
        from: SlotId(0),
        edge: None,
        to: Target::New(SlotId(1)),
        step: StepSpec {
            context: GraphContextId::BASE,
            types: None,
            direction: Direction::Outgoing,
            edge_filter: Some(f),
            far_labels: None,
            far_filter: None,
        },
    };
    assert!(GqlCursor::open(&s.db, &host, &plan, vec![]).is_err());
}

/// The work of random two-hop patterns -- every direction, type list, far
/// label and predicate -- is pinned, counter by counter: rows out, edges
/// walked, rows read and key lookups. A change to the hop's inner loop may
/// spend less, never more. Taken before the M4 review's refactors (task
/// R-C); the refill buffer's `queue_entries` is pinned by `gql_budget.rs`.
#[test]
fn the_work_of_random_hops_is_pinned() {
    let dir = tempfile::tempdir().unwrap();
    let g = graph(dir.path(), 3, 40, 0);
    let mut rng = Rng(0x12C);
    let mut spent = Vec::new();
    for _ in 0..12 {
        let key = format!("k{}", rng.below(7));
        let (h1, h2) = (hop_choices(&g, &mut rng), hop_choices(&g, &mut rng));
        let mut host = TestHost::default();
        let k = host.expr(E::Param(0));
        let s1 = step(&mut host, &h1, 1, 2);
        let s2 = step(&mut host, &h2, 3, 4);
        let plan = key_seed(unit(5), 0, k, &g.cols);
        let plan = expand(plan, 0, 1, Target::New(SlotId(2)), s1);
        let plan = expand(plan, 2, 3, Target::New(SlotId(4)), s2);
        let plan = project_all(&mut host, plan, 5);
        let mut cursor = GqlCursor::open(&g.db, &host, &plan, vec![text(&key)]).unwrap();
        let page = cursor.next_page(8192, GqlBudget::unlimited(), never).unwrap();
        assert!(page.done);
        let w = page.work;
        spent.push([
            w.binding_rows,
            w.base.graph_edges,
            w.base.primary_reads,
            w.base.key_postings,
        ]);
    }
    let pinned: &[[u64; 4]] = &[
        [2, 2, 0, 3],
        [2, 2, 0, 3],
        [2, 5, 1, 3],
        [9, 40, 12, 3],
        [3, 7, 0, 3],
        [1, 3, 0, 3],
        [1, 5, 3, 3],
        [2, 2, 0, 3],
        [2, 5, 0, 3],
        [2, 2, 0, 3],
        [15, 55, 1, 3],
        [2, 8, 0, 3],
    ];
    assert_eq!(spent, pinned, "{spent:?}");
}
