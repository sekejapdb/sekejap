//! Per-hop predicates and the reaching edge: `docs/core/GRAPH_CONTRACT.md` §4.2
//! and §4.3, against a brute-force reference over a random 2,000-node graph.
//!
//! The reference is a plain BFS written here, over adjacency lists held in
//! memory, applying the same edge and node predicates as it expands. It knows
//! nothing about postings, membership sets or budgets. Every assertion below
//! is the engine's answer against that reference's answer, or a counted claim
//! about the work the engine did to reach it.
//!
//! The graph: 2,000 nodes across TWO collections (`person` and `org`, so an
//! inter-collection traversal per §2.1) in TWO contexts (the base graph and
//! `alt`), each node given up to four outgoing `knows` edges to pseudo-random
//! destinations, each edge carrying `{weight: <0..1>, rank: <0..99>,
//! tag: "even"|"odd"}`. Nodes carry `born` (an indexed Int) and `loc` (an
//! indexed Point), so a node predicate has both a scalar range and a point
//! cover to be answered from.

use sekejap_core::{
    collections::{
        BfsRequest, CandidateDriver, Cmp, CollectionId, CollectionOptions, Database, Direction,
        EdgeKey, EdgePredicate, EntityId, GraphContextId, PointFilter, Projection, ProjectedValue,
        QueryBudget, QueryError, QueryFilter, QueryOrder, QueryRequest, ScalarFilter, ScalarValue,
        SortDirection, WorkResource,
    },
    spatial_math::{within_radius, Bounds, Point},
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Bound,
};

const NODES: usize = 2_000;
/// How many `(seed, depth, edge_where, node_where)` combinations the oracle
/// walks. The brief asks for 500.
const SAMPLES: usize = 500;

// ── the fixture ───────────────────────────────────────────────────────────

/// A pseudo-random generator written here so the fixture is reproducible
/// without a dependency: SplitMix64, the same shape `lang/tests/sqlslice`'s
/// fixtures use.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
    fn unit(&mut self) -> f64 {
        (self.next() % 1_000_000) as f64 / 1_000_000.0
    }
}

/// One edge, as the reference holds it.
#[derive(Clone, Debug)]
struct RefEdge {
    source: usize,
    destination: usize,
    context: u64,
    properties: Value,
}

struct Fixture {
    db: Database,
    person: CollectionId,
    org: CollectionId,
    /// `ids[i]` is the entity the reference calls `i`.
    ids: Vec<EntityId>,
    born: Vec<i64>,
    loc: Vec<(f64, f64)>,
    edges: Vec<RefEdge>,
    /// `out[i]` and `incoming[i]` are indices into `edges`, in the order the
    /// engine's key walk produces them: ascending by the far endpoint's
    /// entity id, which for one source in one context and type is the tail of
    /// the key.
    out: Vec<Vec<usize>>,
    incoming: Vec<Vec<usize>>,
    knows: sekejap_core::collections::EdgeTypeId,
    alt: GraphContextId,
    born_index: sekejap_core::collections::IndexId,
    loc_index: sekejap_core::collections::IndexId,
    _dir: tempfile::TempDir,
}

fn cfg() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn build() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("db"), cfg()).unwrap();
    let fields = vec![
        ("born".into(), Kind::Int),
        ("loc".into(), Kind::Point),
        ("name".into(), Kind::Text),
    ];
    let person = db
        .create_collection("person", fields.clone(), CollectionOptions::default())
        .unwrap();
    let org = db
        .create_collection("org", fields, CollectionOptions::default())
        .unwrap();
    let mut rng = Rng(0x5EED_0001);
    let mut ids = Vec::with_capacity(NODES);
    let mut born = Vec::with_capacity(NODES);
    let mut loc = Vec::with_capacity(NODES);
    for i in 0..NODES {
        // Every fifth node is an `org`, so a traversal crosses collections
        // and a node predicate on `person`'s index meets rows it does not
        // cover.
        let collection = if i % 5 == 4 { org } else { person };
        let year = 1_900 + (rng.next() % 120) as i64;
        let lon = -180.0 + rng.unit() * 360.0;
        let lat = -85.0 + rng.unit() * 170.0;
        let id = db
            .put(
                collection,
                &format!("n{i:05}"),
                &json!({
                    "born": year,
                    "loc": {"type": "Point", "coordinates": [lon, lat]},
                    "name": format!("node {i}"),
                }),
            )
            .unwrap();
        ids.push(id);
        born.push(year);
        loc.push((lon, lat));
        if i % 256 == 255 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    let born_index = db.create_scalar_index(person, "person_born", "born", false).unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(born_index, 256).unwrap();
    db.commit().unwrap();
    let loc_index = db.create_point_index(person, "person_loc", "loc").unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(loc_index, 256).unwrap();
    db.commit().unwrap();

    db.enable_graph().unwrap();
    let knows = db.create_edge_type("knows").unwrap();
    let alt = db.create_graph_context("alt").unwrap();
    let mut edges: Vec<RefEdge> = Vec::new();
    let mut written: BTreeSet<(usize, u64, usize)> = BTreeSet::new();
    for source in 0..NODES {
        let degree = 2 + rng.below(4);
        for _ in 0..degree {
            let destination = rng.below(NODES);
            if destination == source {
                continue;
            }
            // One of every four edges lives in `alt`, so a traversal in the
            // base graph must not see it and vice versa (§3.1).
            let context = u64::from(rng.below(4) == 0);
            if !written.insert((source, context, destination)) {
                // The structural identity of an edge today is
                // `(source, context, type, destination)`: a second write of
                // the same quadruple REPLACES the first, so the reference
                // must not hold two.
                continue;
            }
            let properties = json!({
                "weight": (rng.next() % 1_000) as f64 / 1_000.0,
                "rank": (rng.next() % 100) as i64,
                "tag": if rng.next() % 2 == 0 { "even" } else { "odd" },
            });
            let context_id = if context == 0 {
                GraphContextId::BASE
            } else {
                alt
            };
            db.put_edge(
                context_id,
                ids[source],
                knows,
                ids[destination],
                &properties,
            )
            .unwrap();
            edges.push(RefEdge {
                source,
                destination,
                context,
                properties,
            });
        }
        if source % 256 == 255 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();

    // The engine walks one source's adjacency in KEY order, whose tail is the
    // far endpoint's entity id, and the outgoing direction before the
    // incoming one. The reference's lists are sorted the same way so that
    // "the first admitted edge" means the same thing on both sides.
    let mut out = vec![Vec::new(); NODES];
    let mut incoming = vec![Vec::new(); NODES];
    for (at, edge) in edges.iter().enumerate() {
        out[edge.source].push(at);
        incoming[edge.destination].push(at);
    }
    let by_id = |ids: &Vec<EntityId>, list: &mut Vec<usize>, edges: &Vec<RefEdge>, far: bool| {
        list.sort_by_key(|at| {
            let edge = &edges[*at];
            let node = if far { edge.destination } else { edge.source };
            (edge.context, ids[node])
        });
    };
    for list in &mut out {
        by_id(&ids, list, &edges, true);
    }
    for list in &mut incoming {
        by_id(&ids, list, &edges, false);
    }

    Fixture {
        db,
        person,
        org,
        ids,
        born,
        loc,
        edges,
        out,
        incoming,
        knows,
        alt,
        born_index,
        loc_index,
        _dir: dir,
    }
}

// ── the reference ─────────────────────────────────────────────────────────

/// One sampled question, in the reference's own vocabulary.
#[derive(Clone, Debug)]
struct Question {
    seed: usize,
    direction: Direction,
    context: u64,
    /// The shallowest depth whose nodes the answer RETURNS. A deeper one
    /// still expands every level above it; it only withholds them, which is
    /// the arm `min_depth > 1` exercises.
    min_depth: usize,
    max_depth: usize,
    /// `(property, op, value)` over the edge bag.
    edge_where: Vec<(String, Cmp, Value)>,
    /// An inclusive `born` range, when the question has one.
    born: Option<(i64, i64)>,
    /// A `born` EQUALITY, which is the `NodeProbe::ScalarEq` arm: one index
    /// point read per visited node, no membership set.
    born_eq: Option<i64>,
    /// A bounding box on `loc`, when the question has one.
    bbox: Option<[f64; 4]>,
    /// `(lon, lat, metres)` -- a `PointFilter::Radius` on `loc`.
    radius: Option<(f64, f64, f64)>,
}

fn property_matches(properties: &Value, property: &str, op: Cmp, value: &Value) -> bool {
    let Some(found) = properties.get(property) else {
        return false;
    };
    let ordering = match (found, value) {
        (Value::Number(a), Value::Number(b)) => {
            match a.as_f64().unwrap().partial_cmp(&b.as_f64().unwrap()) {
                Some(ordering) => ordering,
                None => return false,
            }
        }
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        _ => return false,
    };
    match op {
        Cmp::Eq => ordering.is_eq(),
        Cmp::Ne => !ordering.is_eq(),
        Cmp::Lt => ordering.is_lt(),
        Cmp::Le => ordering.is_le(),
        Cmp::Gt => ordering.is_gt(),
        Cmp::Ge => ordering.is_ge(),
    }
}

impl Fixture {
    /// Does the far endpoint satisfy the question's node predicates?
    ///
    /// A node of another collection than the predicate's index fails it: the
    /// index covers `person`, and an `org` row has no entry in it. That is
    /// the engine's rule (`NodeGate::admits`) and it is stated here rather
    /// than derived, so a change on either side breaks this test.
    fn node_admits(&self, question: &Question, node: usize) -> bool {
        if question.born.is_none()
            && question.born_eq.is_none()
            && question.bbox.is_none()
            && question.radius.is_none()
        {
            return true;
        }
        if self.ids[node].collection != self.person {
            return false;
        }
        if let Some((lower, upper)) = question.born {
            if self.born[node] < lower || self.born[node] > upper {
                return false;
            }
        }
        if let Some(year) = question.born_eq {
            if self.born[node] != year {
                return false;
            }
        }
        if let Some([west, east, south, north]) = question.bbox {
            let (lon, lat) = self.loc[node];
            if lon < west || lon > east || lat < south || lat > north {
                return false;
            }
        }
        if let Some((lon, lat, metres)) = question.radius {
            let (node_lon, node_lat) = self.loc[node];
            // The same geodesic the index's own refine uses, so "inside the
            // radius" means one thing on both sides of this comparison.
            if !within_radius(
                Point::new(lon, lat).unwrap(),
                Point::new(node_lon, node_lat).unwrap(),
                metres,
            )
            .unwrap()
            {
                return false;
            }
        }
        true
    }

    fn edge_admits(&self, question: &Question, edge: &RefEdge) -> bool {
        question
            .edge_where
            .iter()
            .all(|(property, op, value)| property_matches(&edge.properties, property, *op, value))
    }

    /// The brute-force BFS: the same rules, written out.
    ///
    ///   * a node is never revisited (ACYCLIC, §4.1);
    ///   * an edge the predicate refuses is not followed (§4.3);
    ///   * a node the predicate refuses is neither emitted nor expanded;
    ///   * the SEED is not tested against the node predicates -- it is named,
    ///     not found by a hop;
    ///   * the reaching edge of a node is the FIRST edge admitted for it in
    ///     its level, walking outgoing before incoming and each in key order;
    ///   * a level shallower than `min_depth` still expands, and still
    ///     closes its nodes to later levels, but is not RETURNED.
    ///
    /// Returns `(node, depth, reaching edge index)` for every admitted node,
    /// ascending by node.
    fn reference(&self, question: &Question) -> Vec<(usize, usize, usize)> {
        let mut seen: BTreeSet<usize> = BTreeSet::new();
        seen.insert(question.seed);
        let mut frontier = vec![question.seed];
        let mut out: Vec<(usize, usize, usize)> = Vec::new();
        for depth in 1..=question.max_depth {
            // Offers in the order the engine makes them: each frontier entry
            // in ASCENDING ENTITY ID (the level it came from was sorted that
            // way), outgoing before incoming, each list in key order.
            let mut offers: Vec<(usize, usize)> = Vec::new();
            for node in &frontier {
                let mut lists: Vec<(&Vec<usize>, bool)> = Vec::new();
                if matches!(question.direction, Direction::Outgoing | Direction::Both) {
                    lists.push((&self.out[*node], true));
                }
                if matches!(question.direction, Direction::Incoming | Direction::Both) {
                    lists.push((&self.incoming[*node], false));
                }
                for (list, forward) in lists {
                    for at in list {
                        let edge = &self.edges[*at];
                        if edge.context != question.context {
                            continue;
                        }
                        let far = if forward {
                            edge.destination
                        } else {
                            edge.source
                        };
                        if seen.contains(&far) {
                            continue;
                        }
                        if !self.edge_admits(question, edge) {
                            continue;
                        }
                        if !self.node_admits(question, far) {
                            continue;
                        }
                        offers.push((far, *at));
                    }
                }
            }
            if offers.is_empty() {
                break;
            }
            // First admitted wins, and the level is then sorted by entity id.
            let mut level: BTreeMap<usize, usize> = BTreeMap::new();
            for (node, at) in offers {
                level.entry(node).or_insert(at);
            }
            let mut next: Vec<(usize, usize)> = level.into_iter().collect();
            next.sort_by_key(|(node, _)| self.ids[*node]);
            for (node, at) in &next {
                seen.insert(*node);
                if depth >= question.min_depth {
                    out.push((*node, depth, *at));
                }
            }
            frontier = next.into_iter().map(|(node, _)| node).collect();
        }
        out.sort_by_key(|(node, _, _)| self.ids[*node]);
        out
    }

    fn context_id(&self, context: u64) -> GraphContextId {
        if context == 0 {
            GraphContextId::BASE
        } else {
            self.alt
        }
    }

    /// The engine's answer to one question, through the traversal atomic.
    fn engine(&self, question: &Question) -> Vec<(EntityId, usize, Option<Value>)> {
        let edge_where: Vec<EdgePredicate<'_>> = question
            .edge_where
            .iter()
            .map(|(property, op, value)| EdgePredicate {
                property,
                op: *op,
                value: scalar_of(value),
            })
            .collect();
        let node_where = self.node_filters(question);
        let request = BfsRequest {
            seed: self.ids[question.seed],
            direction: question.direction,
            context: self.context_id(question.context),
            edge_type: Some(self.knows),
            min_depth: question.min_depth,
            max_depth: question.max_depth,
            include_seed: false,
            max_visited: 65_536,
            max_edges: 1_000_000,
            result_limit: 65_536,
            edge_where: &edge_where,
            node_where: &node_where,
        };
        let mut out: Vec<(EntityId, usize, Option<Value>)> = self
            .db
            .traverse_bfs_binding_edges(request)
            .unwrap()
            .nodes
            .into_iter()
            .map(|node| (node.entity, node.depth, node.via.map(|via| via.properties)))
            .collect();
        out.sort_unstable_by_key(|(id, _, _)| *id);
        out
    }

    fn node_filters(&self, question: &Question) -> Vec<QueryFilter<'static>> {
        let mut filters: Vec<QueryFilter<'static>> = Vec::new();
        if let Some((lower, upper)) = question.born {
            filters.push(QueryFilter::Scalar {
                index: self.born_index,
                predicate: ScalarFilter::Range {
                    lower: Bound::Included(ScalarValue::I64(lower)),
                    upper: Bound::Included(ScalarValue::I64(upper)),
                },
            });
        }
        // The `NodeProbe::ScalarEq` arm: no membership set is built, one
        // index point read answers one node.
        if let Some(year) = question.born_eq {
            filters.push(QueryFilter::Scalar {
                index: self.born_index,
                predicate: ScalarFilter::Eq(ScalarValue::I64(year)),
            });
        }
        if let Some([west, east, south, north]) = question.bbox {
            filters.push(QueryFilter::Point {
                index: self.loc_index,
                predicate: PointFilter::Bbox(Bounds::new(west, east, south, north).unwrap()),
            });
        }
        if let Some((lon, lat, radius_metres)) = question.radius {
            filters.push(QueryFilter::Point {
                index: self.loc_index,
                predicate: PointFilter::Radius {
                    center: Point::new(lon, lat).unwrap(),
                    radius_metres,
                },
            });
        }
        filters
    }
}

fn scalar_of(value: &Value) -> ScalarValue<'_> {
    match value {
        Value::Bool(value) => ScalarValue::Bool(*value),
        Value::String(value) => ScalarValue::Text(value),
        Value::Number(number) => match number.as_i64() {
            Some(value) => ScalarValue::I64(value),
            None => ScalarValue::F64(number.as_f64().unwrap()),
        },
        other => panic!("not a scalar: {other}"),
    }
}

/// The 500 sampled questions.
fn questions(fixture: &Fixture) -> Vec<Question> {
    let mut rng = Rng(0xC0FF_EE00);
    let mut out = Vec::with_capacity(SAMPLES);
    while out.len() < SAMPLES {
        let seed = rng.below(NODES);
        let direction = match rng.below(3) {
            0 => Direction::Outgoing,
            1 => Direction::Incoming,
            _ => Direction::Both,
        };
        let context = u64::from(rng.below(4) == 0);
        let max_depth = 1 + rng.below(3);
        let mut edge_where = Vec::new();
        match rng.below(4) {
            0 => {}
            1 => edge_where.push((
                "weight".to_owned(),
                Cmp::Gt,
                json!(rng.unit()),
            )),
            2 => {
                edge_where.push(("rank".to_owned(), Cmp::Le, json!(rng.below(100) as i64)));
                edge_where.push((
                    "tag".to_owned(),
                    if rng.below(2) == 0 { Cmp::Eq } else { Cmp::Ne },
                    json!("even"),
                ));
            }
            _ => edge_where.push((
                "weight".to_owned(),
                Cmp::Ge,
                json!(rng.unit() * 0.5),
            )),
        }
        // A `born` predicate on one question in three, and one in three of
        // THOSE is an equality -- `NodeProbe::ScalarEq`, the one arm that
        // answers a node with an index point read instead of a set.
        let (mut born, mut born_eq) = (None, None);
        if rng.below(3) == 0 {
            if rng.below(3) == 0 {
                born_eq = Some(1_900 + rng.below(120) as i64);
            } else {
                let lower = 1_900 + rng.below(100) as i64;
                born = Some((lower, lower + rng.below(40) as i64));
            }
        }
        // A `loc` predicate on one question in four, half of them a RADIUS
        // (the geodesic refine) rather than a bounding box. A radius is
        // centred on a node the fixture actually holds, so it admits rows.
        let (mut bbox, mut radius) = (None, None);
        if rng.below(4) == 0 {
            if rng.below(2) == 0 {
                let (lon, lat) = fixture.loc[rng.below(NODES)];
                radius = Some((lon, lat, 300_000.0 + rng.unit() * 2_000_000.0));
            } else {
                let west = -180.0 + rng.unit() * 300.0;
                let south = -85.0 + rng.unit() * 140.0;
                bbox = Some([west, west + 40.0, south, south + 30.0]);
            }
        }
        // One question in three that reaches three hops withholds the first
        // level: it is still expanded, and still closes its nodes to the
        // levels below it, but it is not returned.
        let min_depth = if max_depth == 3 && rng.below(3) == 0 {
            2
        } else {
            1
        };
        let _ = &fixture.ids;
        out.push(Question {
            seed,
            direction,
            context,
            min_depth,
            max_depth,
            edge_where,
            born,
            born_eq,
            bbox,
            radius,
        });
    }
    out
}

// ── the tests ─────────────────────────────────────────────────────────────

#[test]
fn oracle_five_hundred_sampled_traversals_equal_a_brute_force_bfs() {
    let fixture = build();
    let mut with_edge_predicates = 0usize;
    let mut with_node_predicates = 0usize;
    let mut with_scalar_eq = 0usize;
    let mut with_radius = 0usize;
    let mut deep_only = 0usize;
    let mut nonempty = 0usize;
    for question in questions(&fixture) {
        let reference = fixture.reference(&question);
        let engine = fixture.engine(&question);
        assert_eq!(
            engine.len(),
            reference.len(),
            "row count for {question:?}: engine {} vs reference {}",
            engine.len(),
            reference.len()
        );
        for ((id, depth, properties), (node, reference_depth, at)) in
            engine.iter().zip(reference.iter())
        {
            assert_eq!(*id, fixture.ids[*node], "node identity for {question:?}");
            assert_eq!(*depth, *reference_depth, "depth for {question:?}");
            // §4.2: the reaching edge's properties ARE the stored bag.
            assert_eq!(
                properties.as_ref(),
                Some(&fixture.edges[*at].properties),
                "reaching edge for {question:?} at node {node}"
            );
        }
        if !question.edge_where.is_empty() {
            with_edge_predicates += 1;
        }
        if question.born.is_some()
            || question.born_eq.is_some()
            || question.bbox.is_some()
            || question.radius.is_some()
        {
            with_node_predicates += 1;
        }
        if question.born_eq.is_some() {
            with_scalar_eq += 1;
        }
        if question.radius.is_some() {
            with_radius += 1;
        }
        if question.min_depth > 1 {
            deep_only += 1;
        }
        if !engine.is_empty() {
            nonempty += 1;
        }
    }
    // The sample is only evidence if it actually exercised the two halves.
    assert!(
        with_edge_predicates >= 200,
        "only {with_edge_predicates} of {SAMPLES} questions carried an edge predicate"
    );
    assert!(
        with_node_predicates >= 100,
        "only {with_node_predicates} of {SAMPLES} questions carried a node predicate"
    );
    assert!(
        nonempty >= 200,
        "only {nonempty} of {SAMPLES} questions returned any row at all"
    );
    // Each named arm of `NodeGate` carries its own cost and its own code
    // path, so the sample is only evidence for an arm it actually walked.
    assert!(
        with_scalar_eq >= 30,
        "only {with_scalar_eq} of {SAMPLES} questions probed NodeProbe::ScalarEq"
    );
    assert!(
        with_radius >= 30,
        "only {with_radius} of {SAMPLES} questions used a PointFilter::Radius"
    );
    assert!(
        deep_only >= 40,
        "only {deep_only} of {SAMPLES} questions pruned a level below min_depth"
    );
}

/// Pruning is real: a predicate that rejects anything makes `graph_visited`
/// STRICTLY smaller than the same walk with no predicate, and the pruned
/// edges are still counted as edges decoded.
#[test]
fn a_predicate_that_rejects_shrinks_the_frontier_and_still_counts_the_edge() {
    let fixture = build();
    // A seed with a wide two-hop reach, so there is something to prune.
    let mut best = (0usize, 0u64);
    for seed in 0..NODES {
        let (plain, _) = work_of(&fixture, seed, 2, &[], &[]);
        if plain.graph_visited > best.1 {
            best = (seed, plain.graph_visited);
        }
    }
    let (seed, _) = best;
    let (plain, _) = work_of(&fixture, seed, 2, &[], &[]);
    let (pruned, pruned_rows) = work_of(
        &fixture,
        seed,
        2,
        &[EdgePredicate {
            property: "weight",
            op: Cmp::Gt,
            value: ScalarValue::F64(0.5),
        }],
        &[],
    );
    assert!(
        pruned.graph_visited < plain.graph_visited,
        "edge predicate visited {} of {}",
        pruned.graph_visited,
        plain.graph_visited
    );
    // A pruned edge is still an edge that was read and decoded: §4.3 prunes
    // the frontier, not the reading.
    assert!(
        pruned.graph_edges > 0,
        "a pruned walk still decodes edges: {pruned:?}"
    );
    // No row is opened FOR A PREDICATE (§4.3's no-row rule). What a graph
    // page still owes is one existence probe per RETURNED row -- the orphan
    // refusal `winner_needs_no_row` keeps for every graph shape -- and
    // nothing is decoded out of those bytes, which `row_decodes` says.
    assert_eq!(
        pruned.primary_reads, pruned_rows,
        "a pruned traversal read {} rows for {pruned_rows} winners",
        pruned.primary_reads
    );
    assert_eq!(pruned.row_decodes, 0, "a per-hop predicate decoded a row");
    assert!(
        pruned.primary_reads < plain.primary_reads,
        "pruning returned as many rows as the unpruned walk"
    );

    let (node_pruned, _) = work_of(
        &fixture,
        seed,
        2,
        &[],
        &[QueryFilter::Scalar {
            index: fixture.born_index,
            predicate: ScalarFilter::Range {
                lower: Bound::Included(ScalarValue::I64(1_950)),
                upper: Bound::Included(ScalarValue::I64(1_960)),
            },
        }],
    );
    assert!(
        node_pruned.graph_visited < plain.graph_visited,
        "node predicate visited {} of {}",
        node_pruned.graph_visited,
        plain.graph_visited
    );
    assert_eq!(node_pruned.row_decodes, 0, "a node predicate decoded a row");
    assert!(
        node_pruned.primary_reads < plain.primary_reads,
        "node pruning returned as many rows as the unpruned walk"
    );
}

/// The work one traversal charges, run through the query engine so the
/// counters are the engine's own.
fn work_of(
    fixture: &Fixture,
    seed: usize,
    max_depth: usize,
    edge_where: &[EdgePredicate<'_>],
    node_where: &[QueryFilter<'_>],
) -> (sekejap_core::collections::QueryWork, u64) {
    let filters = [QueryFilter::Graph(BfsRequest {
        seed: fixture.ids[seed],
        direction: Direction::Outgoing,
        context: GraphContextId::BASE,
        edge_type: Some(fixture.knows),
        min_depth: 1,
        max_depth,
        include_seed: false,
        max_visited: 65_536,
        max_edges: 1_000_000,
        result_limit: 65_536,
        edge_where,
        node_where,
    })];
    let mut prepared = fixture
        .db
        .prepare_query(QueryRequest {
            collection: fixture.person,
            filters: &filters,
            order: QueryOrder::Driver,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let mut total = sekejap_core::collections::QueryWork::default();
    let mut rows = 0u64;
    loop {
        let page = prepared
            .next_page(4_096, QueryBudget::unlimited(), || false)
            .unwrap();
        total.candidates += page.work.candidates;
        total.primary_reads += page.work.primary_reads;
        total.row_decodes += page.work.row_decodes;
        total.graph_edges += page.work.graph_edges;
        total.graph_visited += page.work.graph_visited;
        total.scalar_postings += page.work.scalar_postings;
        total.spatial_postings += page.work.spatial_postings;
        rows += page.rows.len() as u64;
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    (total, rows)
}

/// A node reached by two edges reports the FIRST one admitted, and "first"
/// is the key order the walk reads the postings in.
#[test]
fn a_node_reached_by_two_edges_reports_the_first_admitted() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("db"), cfg()).unwrap();
    let c = db
        .create_collection(
            "n",
            vec![("born".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    // `hub` has two outgoing edges into `target`? No -- the edge identity is
    // the quadruple, so parallel edges are impossible today. Two DIFFERENT
    // sources in the same frontier level reach one target instead, which is
    // the shape §4.2's rule is about.
    let seed = db.put(c, "seed", &json!({"born": 1})).unwrap();
    let left = db.put(c, "a-left", &json!({"born": 2})).unwrap();
    let right = db.put(c, "b-right", &json!({"born": 3})).unwrap();
    let target = db.put(c, "target", &json!({"born": 4})).unwrap();
    db.commit().unwrap();
    db.enable_graph().unwrap();
    let knows = db.create_edge_type("knows").unwrap();
    for (from, to, mark) in [
        (seed, left, "seed->left"),
        (seed, right, "seed->right"),
        (left, target, "left->target"),
        (right, target, "right->target"),
    ] {
        db.put_edge(GraphContextId::BASE, from, knows, to, &json!({"mark": mark}))
            .unwrap();
    }
    db.commit().unwrap();
    let request = BfsRequest {
        seed,
        direction: Direction::Outgoing,
        context: GraphContextId::BASE,
        edge_type: Some(knows),
        min_depth: 2,
        max_depth: 2,
        include_seed: false,
        max_visited: 64,
        max_edges: 1_024,
        result_limit: 64,
        edge_where: &[],
        node_where: &[],
    };
    let result = db.traverse_bfs_binding_edges(request).unwrap();
    assert_eq!(result.nodes.len(), 1);
    let via = result.nodes[0].via.as_ref().expect("a reached node binds its edge");
    // `left` was allocated before `right`, so its sequence is lower, so the
    // level expands `left` first and its edge is the first admitted.
    assert_eq!(via.properties["mark"], json!("left->target"));
    assert_eq!(
        via.key,
        EdgeKey {
            source: left,
            context: GraphContextId::BASE,
            edge_type: knows,
            destination: target,
        }
    );
    // With the first edge refused, the SECOND becomes the first admitted --
    // the rule is about admission, not about existence.
    let refused = [EdgePredicate {
        property: "mark",
        op: Cmp::Ne,
        value: ScalarValue::Text("left->target"),
    }];
    let result = db
        .traverse_bfs_binding_edges(BfsRequest {
            edge_where: &refused,
            ..request
        })
        .unwrap();
    assert_eq!(result.nodes.len(), 1);
    assert_eq!(
        result.nodes[0].via.as_ref().unwrap().properties["mark"],
        json!("right->target")
    );
}

/// An incoming hop's properties come from the primary posting, which the
/// reverse marker does not carry -- so the same predicate decides the same
/// edges walked either way.
#[test]
fn an_incoming_hop_reads_the_primary_posting_for_its_predicate() {
    let fixture = build();
    let mut checked = 0usize;
    for seed in 0..NODES {
        if fixture.incoming[seed].len() < 3 {
            continue;
        }
        let question = Question {
            seed,
            direction: Direction::Incoming,
            context: 0,
            min_depth: 1,
            max_depth: 1,
            edge_where: vec![("weight".to_owned(), Cmp::Gt, json!(0.4))],
            born: None,
            born_eq: None,
            bbox: None,
            radius: None,
        };
        let reference = fixture.reference(&question);
        let engine = fixture.engine(&question);
        assert_eq!(engine.len(), reference.len(), "seed {seed}");
        for ((_, _, properties), (_, _, at)) in engine.iter().zip(reference.iter()) {
            assert_eq!(properties.as_ref(), Some(&fixture.edges[*at].properties));
        }
        checked += 1;
        if checked == 40 {
            break;
        }
    }
    assert!(checked > 0, "the fixture has no fan-in to test");
}

/// `@edge.<name>` projects the reaching edge, and `QueryOrder::Edge` ranks by
/// it -- both without opening a row.
#[test]
fn the_reaching_edge_projects_and_ranks_without_a_row() {
    let fixture = build();
    let seed = (0..NODES)
        .max_by_key(|node| {
            fixture.out[*node]
                .iter()
                .filter(|at| fixture.edges[**at].context == 0)
                .count()
        })
        .unwrap();
    let filters = [QueryFilter::Graph(BfsRequest {
        seed: fixture.ids[seed],
        direction: Direction::Outgoing,
        context: GraphContextId::BASE,
        edge_type: Some(fixture.knows),
        min_depth: 1,
        max_depth: 1,
        include_seed: false,
        max_visited: 65_536,
        max_edges: 1_000_000,
        result_limit: 65_536,
        edge_where: &[],
        node_where: &[],
    })];
    let fields = ["@edge.weight", "@edge.tag"];
    let mut prepared = fixture
        .db
        .prepare_query(QueryRequest {
            collection: fixture.person,
            filters: &filters,
            order: QueryOrder::Edge {
                property: "weight",
                direction: SortDirection::Descending,
            },
            projection: Projection::Fields(&fields),
            total_limit: Some(10),
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let page = prepared
        .next_page(64, QueryBudget::unlimited(), || false)
        .unwrap();
    assert!(!page.rows.is_empty(), "the widest seed reached nothing");
    // Descending by the edge's own property.
    let weights: Vec<f64> = page
        .rows
        .iter()
        .map(|row| match &row.projected[0].1 {
            ProjectedValue::Value(value) => value.as_f64().unwrap(),
            other => panic!("edge weight projected as {other:?}"),
        })
        .collect();
    assert!(
        weights.windows(2).all(|pair| pair[0] >= pair[1]),
        "not descending: {weights:?}"
    );
    // The values are the stored bag's, matched against the reference.
    for row in &page.rows {
        let node = fixture
            .ids
            .iter()
            .position(|id| *id == row.id)
            .expect("a returned row is a fixture node");
        let edge = fixture.out[seed]
            .iter()
            .map(|at| &fixture.edges[*at])
            .find(|edge| edge.context == 0 && edge.destination == node)
            .expect("the reference has the edge the engine reported");
        assert_eq!(row.projected[0].0, "@edge.weight");
        assert_eq!(
            row.projected[0].1,
            ProjectedValue::Value(edge.properties["weight"].clone())
        );
        assert_eq!(
            row.projected[1].1,
            ProjectedValue::Value(edge.properties["tag"].clone())
        );
    }
    // Nothing was DECODED out of a row: the projected values are the
    // traversal's own. The reads that remain are one existence probe per
    // returned row, the orphan refusal a graph page owes whatever it
    // projects (`winner_needs_no_row`).
    assert_eq!(page.work.row_decodes, 0, "an @edge projection decoded a row");
    assert!(
        page.work.primary_reads <= page.rows.len() as u64,
        "an @edge projection read {} rows for {} winners",
        page.work.primary_reads,
        page.rows.len()
    );
    // A property the bag does not carry is MISSING, not an error.
    let absent = ["@edge.nosuch"];
    let mut prepared = fixture
        .db
        .prepare_query(QueryRequest {
            collection: fixture.person,
            filters: &filters,
            order: QueryOrder::Driver,
            projection: Projection::Fields(&absent),
            total_limit: Some(1),
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let page = prepared
        .next_page(4, QueryBudget::unlimited(), || false)
        .unwrap();
    assert_eq!(page.rows[0].projected[0].1, ProjectedValue::Missing);
}

/// The refusals: a node predicate an index cannot answer without a row is
/// refused at prepare, with the reason, and never emulated.
#[test]
fn a_node_predicate_an_index_cannot_answer_is_refused_at_prepare() {
    let fixture = build();
    let refused: Vec<QueryFilter<'static>> = vec![
        QueryFilter::Scalar {
            index: fixture.born_index,
            predicate: ScalarFilter::IsNull,
        },
        QueryFilter::Scalar {
            index: fixture.born_index,
            predicate: ScalarFilter::IsMissing,
        },
        QueryFilter::JsonEq {
            field: "name",
            value: &Value::Null,
        },
    ];
    for filter in refused {
        let node_where = [filter];
        let filters = [QueryFilter::Graph(BfsRequest {
            seed: fixture.ids[0],
            direction: Direction::Outgoing,
            context: GraphContextId::BASE,
            edge_type: Some(fixture.knows),
            min_depth: 1,
            max_depth: 1,
            include_seed: false,
            max_visited: 64,
            max_edges: 1_024,
            result_limit: 64,
            edge_where: &[],
            node_where: &node_where,
        })];
        let error = match fixture.db.prepare_query(QueryRequest {
            collection: fixture.person,
            filters: &filters,
            order: QueryOrder::Driver,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        }) {
            Ok(_) => panic!("a row-bound node predicate is refused"),
            Err(error) => error,
        };
        let text = error.to_string();
        assert!(
            text.contains("node predicate"),
            "refusal does not name the construct: {text}"
        );
    }
}

/// A budget stops a traversal and leaves nothing behind: the same prepared
/// query, asked again with room, answers exactly what an unbudgeted one does.
#[test]
fn a_budget_or_a_cancellation_leaves_no_state() {
    let fixture = build();
    let seed = (0..NODES)
        .max_by_key(|node| fixture.out[*node].len())
        .unwrap();
    let edge_where = [EdgePredicate {
        property: "weight",
        op: Cmp::Ge,
        value: ScalarValue::F64(0.0),
    }];
    let filters = [QueryFilter::Graph(BfsRequest {
        seed: fixture.ids[seed],
        direction: Direction::Outgoing,
        context: GraphContextId::BASE,
        edge_type: Some(fixture.knows),
        min_depth: 1,
        max_depth: 2,
        include_seed: false,
        max_visited: 65_536,
        max_edges: 1_000_000,
        result_limit: 65_536,
        edge_where: &edge_where,
        node_where: &[],
    })];
    let request = || QueryRequest {
        collection: fixture.person,
        filters: &filters,
        order: QueryOrder::Driver,
        projection: Projection::Ids,
        total_limit: None,
        driver: CandidateDriver::Auto,
    };
    let mut prepared = fixture.db.prepare_query(request()).unwrap();
    let mut budget = QueryBudget::unlimited();
    budget.graph_edges = 1;
    let error = prepared
        .next_page(64, budget, || false)
        .expect_err("one edge of budget cannot answer this traversal");
    assert!(matches!(
        error,
        QueryError::BudgetExceeded {
            resource: WorkResource::GraphEdges,
            ..
        }
    ));
    // The same prepared query, given room, answers as a fresh one does.
    let recovered = collect(&mut prepared);
    let mut fresh = fixture.db.prepare_query(request()).unwrap();
    assert_eq!(recovered, collect(&mut fresh));

    // A cancellation is the same statement about a different stop.
    let mut prepared = fixture.db.prepare_query(request()).unwrap();
    let error = prepared
        .next_page(64, QueryBudget::unlimited(), || true)
        .expect_err("a cancelled page returns no rows");
    assert!(matches!(error, QueryError::Cancelled));
    assert_eq!(collect(&mut prepared), recovered);
}

fn collect(prepared: &mut sekejap_core::collections::PreparedQuery<'_>) -> Vec<EntityId> {
    let mut out = Vec::new();
    loop {
        let page = prepared
            .next_page(64, QueryBudget::unlimited(), || false)
            .unwrap();
        out.extend(page.rows.iter().map(|row| row.id));
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    out
}

/// A traversal that crosses collections meets rows a node predicate's index
/// does not cover, and refuses them rather than reading their rows.
#[test]
fn a_node_predicate_refuses_a_row_of_another_collection() {
    let fixture = build();
    let question = Question {
        seed: 0,
        direction: Direction::Both,
        context: 0,
        min_depth: 1,
        max_depth: 2,
        edge_where: Vec::new(),
        born: Some((1_900, 2_100)),
        born_eq: None,
        bbox: None,
        radius: None,
    };
    let engine = fixture.engine(&question);
    assert!(
        engine.iter().all(|(id, _, _)| id.collection == fixture.person),
        "a node predicate on person's index admitted an org row"
    );
    assert_eq!(engine.len(), fixture.reference(&question).len());
    // Without the predicate the same walk does reach org rows, so the
    // assertion above is about the predicate and not about the fixture.
    let open = Question {
        born: None,
        ..question
    };
    assert!(
        fixture
            .engine(&open)
            .iter()
            .any(|(id, _, _)| id.collection == fixture.org),
        "the fixture's traversal never crosses into org"
    );
    let _ = fixture.alt;
}

// ── the reaching edge needs the traversal to drive (GRAPH_CONTRACT 4.2) ────

/// `Candidate.edge` is filled in by the traversal's own cursor and by nothing
/// else, so a query that reads the reaching edge under any other driver would
/// see `edge: None` on every row: a `Missing` projection and a total tie in an
/// `@edge` ranking, with no error. Prepare refuses it instead, naming the
/// driver that cannot answer it.
#[test]
fn reading_the_reaching_edge_requires_the_traversal_to_drive() {
    let fixture = build();
    let seed = (0..NODES)
        .max_by_key(|node| fixture.out[*node].len())
        .unwrap();
    let graph = QueryFilter::Graph(BfsRequest {
        seed: fixture.ids[seed],
        direction: Direction::Outgoing,
        context: GraphContextId::BASE,
        edge_type: Some(fixture.knows),
        min_depth: 1,
        max_depth: 1,
        include_seed: false,
        max_visited: 65_536,
        max_edges: 1_000_000,
        result_limit: 65_536,
        edge_where: &[],
        node_where: &[],
    });
    let born = QueryFilter::Scalar {
        index: fixture.born_index,
        predicate: ScalarFilter::Range {
            lower: Bound::Included(ScalarValue::I64(1_900)),
            upper: Bound::Included(ScalarValue::I64(2_100)),
        },
    };
    let filters = [graph, born];
    let fields = ["@edge.weight"];
    // Both spellings of "this query reads the edge": the projection and the
    // ranking. Each is refused under every driver but the traversal's.
    let shapes: [(QueryOrder<'_>, Projection<'_>); 2] = [
        (QueryOrder::Driver, Projection::Fields(&fields)),
        (
            QueryOrder::Edge {
                property: "weight",
                direction: SortDirection::Descending,
            },
            Projection::Ids,
        ),
    ];
    for (order, projection) in shapes {
        for (driver, named) in [
            (CandidateDriver::Entities, "Entities"),
            // Position 1 is the `born` range: a filter that CAN drive, and
            // whose candidates carry no edge.
            (CandidateDriver::Filter(1), "Scalar"),
            (CandidateDriver::Keys, "Keys"),
        ] {
            let error = match fixture.db.prepare_query(QueryRequest {
                collection: fixture.person,
                filters: &filters,
                order,
                projection,
                total_limit: None,
                driver,
            }) {
                Ok(_) => panic!("{driver:?} carries no reaching edge and was accepted"),
                Err(error) => error.to_string(),
            };
            assert!(
                error.contains("reaching edge"),
                "refusal does not name the construct: {error}"
            );
            assert!(
                error.contains(named),
                "refusal does not name the driver {named}: {error}"
            );
        }
        // The traversal itself drives, under `Auto` and when named outright.
        for driver in [CandidateDriver::Auto, CandidateDriver::Filter(0)] {
            let mut prepared = fixture
                .db
                .prepare_query(QueryRequest {
                    collection: fixture.person,
                    filters: &filters,
                    order,
                    projection,
                    total_limit: Some(4),
                    driver,
                })
                .unwrap();
            let page = prepared
                .next_page(8, QueryBudget::unlimited(), || false)
                .unwrap();
            assert!(!page.rows.is_empty(), "the widest seed reached nothing");
        }
    }
}

// ── one gate probe per distinct node (GRAPH_CONTRACT 4.3) ─────────────────

/// A fan-in whose every edge at the second hop lands on ONE node: the gate is
/// a scalar equality, so each refusal is an index point read, and the walk
/// must pay exactly one of them for that node rather than one per incident
/// edge.
#[test]
fn the_node_gate_is_probed_once_per_distinct_entity() {
    const FAN: usize = 200;
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("db"), cfg()).unwrap();
    let c = db
        .create_collection(
            "n",
            vec![("born".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    let seed = db.put(c, "a-seed", &json!({"born": 1})).unwrap();
    let middles: Vec<EntityId> = (0..FAN)
        .map(|i| db.put(c, &format!("b{i:04}"), &json!({"born": 1})).unwrap())
        .collect();
    // One node every middle reaches, and the only one the gate refuses.
    let target = db.put(c, "c-target", &json!({"born": 9})).unwrap();
    db.commit().unwrap();
    let born_index = db.create_scalar_index(c, "n_born", "born", false).unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(born_index, 256).unwrap();
    db.commit().unwrap();
    db.enable_graph().unwrap();
    let knows = db.create_edge_type("knows").unwrap();
    for middle in &middles {
        db.put_edge(GraphContextId::BASE, seed, knows, *middle, &json!({}))
            .unwrap();
        db.put_edge(GraphContextId::BASE, *middle, knows, target, &json!({}))
            .unwrap();
    }
    db.commit().unwrap();

    let node_where = [QueryFilter::Scalar {
        index: born_index,
        predicate: ScalarFilter::Eq(ScalarValue::I64(1)),
    }];
    let filters = [QueryFilter::Graph(BfsRequest {
        seed,
        direction: Direction::Outgoing,
        context: GraphContextId::BASE,
        edge_type: Some(knows),
        min_depth: 1,
        max_depth: 2,
        include_seed: false,
        max_visited: 65_536,
        max_edges: 1_000_000,
        result_limit: 65_536,
        edge_where: &[],
        node_where: &node_where,
    })];
    let mut prepared = db
        .prepare_query(QueryRequest {
            collection: c,
            filters: &filters,
            order: QueryOrder::Driver,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let mut postings = 0u64;
    let mut rows = 0usize;
    loop {
        let page = prepared
            .next_page(4_096, QueryBudget::unlimited(), || false)
            .unwrap();
        postings += page.work.scalar_postings;
        rows += page.rows.len();
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    // The middles are admitted and returned; the target is refused.
    assert_eq!(rows, FAN);
    // `scalar_eq_posting_matches` charges exactly one posting per probe, so
    // this count IS the number of probes: one per middle, plus ONE for the
    // target, which every one of the 200 second-hop edges reaches.
    assert_eq!(
        postings,
        FAN as u64 + 1,
        "the gate was probed {postings} times for {} distinct nodes",
        FAN + 1
    );

    // The atomic runs the same rule with no meter, so the evidence there is
    // the answer: the target is absent however many edges reach it.
    let result = db
        .traverse_bfs(BfsRequest {
            seed,
            direction: Direction::Outgoing,
            context: GraphContextId::BASE,
            edge_type: Some(knows),
            min_depth: 1,
            max_depth: 2,
            include_seed: false,
            max_visited: 65_536,
            max_edges: 1_000_000,
            result_limit: 65_536,
            edge_where: &[],
            node_where: &node_where,
        })
        .unwrap();
    assert_eq!(result.nodes.len(), FAN);
    assert!(result.nodes.iter().all(|node| node.entity != target));
}

// ── max_edges bounds the incoming hop's primary-posting reads (4.2, L4) ───

/// A reverse posting is a marker, so an incoming hop that reads a property
/// reads the primary posting of the same edge back. That is an edge-keyspace
/// read like the range walk's own: it is counted in `scanned_edges` and the
/// edge budget refuses it.
#[test]
fn an_incoming_hops_primary_posting_reads_are_bounded_by_max_edges() {
    const FAN: usize = 64;
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("db"), cfg()).unwrap();
    let c = db
        .create_collection(
            "n",
            vec![("born".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    let target = db.put(c, "z-target", &json!({"born": 1})).unwrap();
    let sources: Vec<EntityId> = (0..FAN)
        .map(|i| db.put(c, &format!("s{i:04}"), &json!({"born": 1})).unwrap())
        .collect();
    db.commit().unwrap();
    db.enable_graph().unwrap();
    let knows = db.create_edge_type("knows").unwrap();
    for source in &sources {
        db.put_edge(
            GraphContextId::BASE,
            *source,
            knows,
            target,
            &json!({"weight": 0.5}),
        )
        .unwrap();
    }
    db.commit().unwrap();
    let request = |max_edges: usize| BfsRequest {
        seed: target,
        direction: Direction::Incoming,
        context: GraphContextId::BASE,
        edge_type: Some(knows),
        min_depth: 1,
        max_depth: 1,
        include_seed: false,
        max_visited: 65_536,
        max_edges,
        result_limit: 65_536,
        edge_where: &[],
        node_where: &[],
    };
    // A membership-only walk reads the reverse postings and nothing else.
    let plain = db.traverse_bfs(request(FAN)).unwrap();
    assert_eq!(plain.nodes.len(), FAN);
    assert_eq!(plain.scanned_edges, FAN);
    // Binding the edge reads each primary posting back, so the same walk
    // costs twice the edge-keyspace reads -- and the budget that fitted the
    // first one refuses this one rather than quietly doubling.
    let error = db
        .traverse_bfs_binding_edges(request(FAN))
        .expect_err("the second pass is not free");
    assert!(
        format!("{error}").contains("edge work limit"),
        "unexpected refusal: {error}"
    );
    let bound = db.traverse_bfs_binding_edges(request(2 * FAN)).unwrap();
    assert_eq!(bound.nodes.len(), FAN);
    assert_eq!(bound.scanned_edges, 2 * FAN);
    assert!(bound
        .nodes
        .iter()
        .all(|node| node.via.as_ref().unwrap().properties["weight"] == json!(0.5)));
}

// ── a page that reads the edge holds no run (L1, rows.rs RUN_BYTES) ───────

/// `RUN_ROWS` is derived from `size_of::<HeapEntry>()` on the premise that a
/// held entry is a rank key and nothing else. A query projecting only
/// `@edge.*` has an EMPTY ROW projection while every held entry owns the
/// edge's whole property bag, so a run bounded in rows would not be bounded
/// in bytes: such a page re-walks instead, which its second page's edge count
/// is the evidence for.
#[test]
fn a_page_that_reads_the_edge_holds_no_ranked_run() {
    let fixture = build();
    let seed = (0..NODES)
        .max_by_key(|node| {
            fixture.out[*node]
                .iter()
                .filter(|at| fixture.edges[**at].context == 0)
                .count()
        })
        .unwrap();
    let filters = [QueryFilter::Graph(BfsRequest {
        seed: fixture.ids[seed],
        direction: Direction::Outgoing,
        context: GraphContextId::BASE,
        edge_type: Some(fixture.knows),
        min_depth: 1,
        max_depth: 2,
        include_seed: false,
        max_visited: 65_536,
        max_edges: 1_000_000,
        result_limit: 65_536,
        edge_where: &[],
        node_where: &[],
    })];
    let fields = ["@edge.weight"];
    let request = QueryRequest {
        collection: fixture.person,
        filters: &filters,
        order: QueryOrder::Edge {
            property: "weight",
            direction: SortDirection::Descending,
        },
        projection: Projection::Fields(&fields),
        total_limit: None,
        driver: CandidateDriver::Auto,
    };
    let mut prepared = fixture.db.prepare_query(request).unwrap();
    let first = prepared
        .next_page(4, QueryBudget::unlimited(), || false)
        .unwrap();
    assert_eq!(first.rows.len(), 4, "the seed reached too little to page");
    assert!(first.work.graph_edges > 0);
    let second = prepared
        .next_page(4, QueryBudget::unlimited(), || false)
        .unwrap();
    assert!(!second.rows.is_empty());
    assert!(
        second.work.graph_edges > 0,
        "a page that reads the edge held a run of ranked rows instead of re-walking"
    );
    // Re-walking is not re-answering: the pages stay disjoint and descending.
    let mut weights: Vec<f64> = Vec::new();
    for row in first.rows.iter().chain(second.rows.iter()) {
        match &row.projected[0].1 {
            ProjectedValue::Value(value) => weights.push(value.as_f64().unwrap()),
            other => panic!("edge weight projected as {other:?}"),
        }
    }
    assert!(
        weights.windows(2).all(|pair| pair[0] >= pair[1]),
        "not descending across the page boundary: {weights:?}"
    );
    let ids: BTreeSet<EntityId> = first
        .rows
        .iter()
        .chain(second.rows.iter())
        .map(|row| row.id)
        .collect();
    assert_eq!(ids.len(), first.rows.len() + second.rows.len());
}

// ── a node set that outgrows its budget names the resource ────────────────

/// A traversal's node membership set that exceeds its memory budget is a
/// BUDGET refusal with the resource named, not an invalid query with prose --
/// and the traversal atomic, whose error type is the database one, carries
/// the same three fields rather than flattening them into a string.
///
/// The overflow itself needs a collection whose sequence span is wider than
/// one bit per byte of `RUN_BYTES` (about 67 million entities), which no test
/// fixture reaches; what is pinned here is that the carrier is typed on both
/// sides and survives the conversion between them.
#[test]
fn a_budget_refusal_keeps_its_resource_across_the_atomic_boundary() {
    let raised = sekejap_core::collections::Error::BudgetExceeded {
        resource: WorkResource::ScalarPostings,
        limit: 1_048_576,
        attempted: 1_048_577,
    };
    let carried = QueryError::from(raised);
    assert!(
        matches!(
            carried,
            QueryError::BudgetExceeded {
                resource: WorkResource::ScalarPostings,
                limit: 1_048_576,
                attempted: 1_048_577,
            }
        ),
        "a named budget refusal lost its resource: {carried:?}"
    );
}
