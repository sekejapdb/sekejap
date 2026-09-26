//! GQL path patterns end to end: parsed, bound, compiled to the engine's
//! path automaton, planned as a `PathSearch` and run (M4-A of
//! `docs/lang/GQL_PROFILE_DESIGN.md` §4), driven through `prepare_sql`.
//!
//! What is at risk, and the test that pins it (brief §11 matrix rows):
//!
//! * "Diamond graph": two paths to one end are two rows (bag semantics),
//!   and ACYCLIC keeps both (`a_diamond_*`);
//! * "Direct edge plus two-hop route": `{2,2}` still finds the two-hop
//!   route beside a direct edge (`a_direct_edge_*`);
//! * "`{0,0}`, `{0,1}`, `?`, `*`": zero iterations bind the end to the
//!   start (`zero_length_*`);
//! * path modes on a cycle: TRAIL returns to the start, ACYCLIC does not
//!   (`path_modes_*`);
//! * "Quantified endpoint labels": labels apply at their positions only,
//!   never to the intermediate nodes (`endpoint_labels_*`);
//! * "Multi-type quantified subpath": the body repeats as a whole
//!   (`a_multi_type_subpath_*`);
//! * a group variable's inline predicate prunes each iteration
//!   (`a_group_predicate_*`);
//! * ANY SHORTEST: fewest hops, one row per end, the zero-edge path, a
//!   disconnected target gives zero rows (`any_shortest_*`);
//! * "Weighted bounded counterexample": ANY CHEAPEST keeps the hop count in
//!   its state, so a costly direct edge still reaches the target within
//!   the bound (`any_cheapest_*`); an invalid cost raises
//!   `InvalidPathCost`;
//! * a named path binds without being returned; returning it is refused
//!   (`a_named_path_*`).
//!
//! Workload names are invented: collections `site_a`, `site_b`, `site_c`,
//! edge types `r`, `s`, `road`.

use sekejap_core::Kind;
use sekejap_core::collections::{CollectionId, Database, EntityId, GraphContextId};
use sekejap_lang::{prepare_sql, SqlError, SqlResult, SqlValue};
use serde_json::json;
use tempfile::TempDir;

mod common;
use common::cfg;

/// ```text
/// site_a, type r (w on each edge):
///   diamond   a->b (1)  a->c (2)  b->d (3)  c->d (9)
///   two-hop   p->q      p->x      x->q
///   cycle     c0->c1    c1->c2    c2->c0
///   shortest  h0->h1->h9           h0->k1->k2->h9
///   isolated  z
///   labels    u -> site_c m1 -> v
///   subpath   g0 -r-> g1 -s-> g2 -r-> g3 -s-> g4    g0 -r-> g5 -r-> g6
/// site_b, type road (w):
///   s->a 10   s->b 1   b->a 1   a->c 1   c->t 1     y->y1 0
/// ```
fn fixture(dir: &TempDir) -> Database {
    let mut db = Database::create(dir.path().join("g.sekejap"), cfg()).unwrap();
    let int = |name: &str| (name.to_owned(), Kind::Int);
    let site_a = db
        .create_collection("site_a", vec![int("w")], Default::default())
        .unwrap();
    let site_b = db
        .create_collection("site_b", vec![int("w")], Default::default())
        .unwrap();
    let site_c = db
        .create_collection("site_c", vec![int("w")], Default::default())
        .unwrap();
    let mut ids: Vec<(CollectionId, String, EntityId)> = Vec::new();
    let mut node = |db: &mut Database, collection: CollectionId, key: &str| -> EntityId {
        if let Some((_, _, id)) = ids.iter().find(|(c, k, _)| *c == collection && k == key) {
            return *id;
        }
        let id = db.put(collection, key, &json!({"w": 1})).unwrap();
        ids.push((collection, key.to_owned(), id));
        id
    };
    let mut edges = Vec::new();
    for (from, to, w) in [
        ("a", "b", 1),
        ("a", "c", 2),
        ("b", "d", 3),
        ("c", "d", 9),
        ("p", "q", 1),
        ("p", "x", 1),
        ("x", "q", 1),
        ("c0", "c1", 1),
        ("c1", "c2", 1),
        ("c2", "c0", 1),
        ("h0", "h1", 1),
        ("h1", "h9", 1),
        ("h0", "k1", 1),
        ("k1", "k2", 1),
        ("k2", "h9", 1),
        ("g0", "g5", 1),
        ("g5", "g6", 1),
    ] {
        edges.push((site_a, from, site_a, to, "r", w));
    }
    for (from, to, t) in [
        ("g0", "g1", "r"),
        ("g1", "g2", "s"),
        ("g2", "g3", "r"),
        ("g3", "g4", "s"),
    ] {
        edges.push((site_a, from, site_a, to, t, 1));
    }
    edges.push((site_a, "u", site_c, "m1", "r", 1));
    edges.push((site_c, "m1", site_a, "v", "r", 1));
    for (from, to, w) in [
        ("s", "a", 10),
        ("s", "b", 1),
        ("b", "a", 1),
        ("a", "c", 1),
        ("c", "t", 1),
        ("y", "y1", 0),
    ] {
        edges.push((site_b, from, site_b, to, "road", w));
    }
    let mut resolved = Vec::new();
    for (fc, from, tc, to, t, w) in edges {
        let source = node(&mut db, fc, from);
        let destination = node(&mut db, tc, to);
        resolved.push((source, t, destination, w));
    }
    node(&mut db, site_a, "z");
    db.enable_graph().unwrap();
    let r = db.create_edge_type("r").unwrap();
    let s = db.create_edge_type("s").unwrap();
    let road = db.create_edge_type("road").unwrap();
    for (source, t, destination, w) in resolved {
        let edge_type = match t {
            "r" => r,
            "s" => s,
            _ => road,
        };
        db.create_edge(
            GraphContextId::BASE,
            source,
            edge_type,
            destination,
            &json!({"w": w}),
        )
        .unwrap();
    }
    db.commit().unwrap();
    db
}

/// The rows of one GQL body, through the SQL entry point.
#[derive(Debug)]
struct Answer {
    rows: Vec<Vec<SqlValue>>,
}

fn run_with(db: &Database, body: &str) -> Result<Answer, SqlError> {
    let text = format!("SELECT * FROM GRAPH_TABLE (base {body})");
    match prepare_sql(db, &text, &[])?.run(db)? {
        SqlResult::Rows { rows, .. } => Ok(Answer {
            rows: rows.into_iter().map(|row| row.values).collect(),
        }),
        other => panic!("`{text}` answered {other:?}"),
    }
}

fn run(db: &Database, body: &str) -> Answer {
    run_with(db, body).unwrap_or_else(|error| panic!("`{body}` failed: {error}"))
}

/// The answer as sorted text rows: a pattern answer is a BAG.
fn bag(db: &Database, body: &str) -> Vec<String> {
    let answer = run(db, body);
    let mut rows: Vec<String> = answer
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| match value {
                    SqlValue::Text(text) => text.clone(),
                    SqlValue::Int(i) => i.to_string(),
                    SqlValue::Null => "NULL".to_owned(),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect();
    rows.sort();
    rows
}

#[test]
fn a_diamond_gives_two_paths_to_its_far_corner() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let far = "(s IS site_a WHERE s._key = 'a')-[:r]->{2,2}(t IS site_a WHERE t._key = 'd') RETURN t._key AS t";
    assert_eq!(bag(&db, &format!("MATCH {far}")), ["d", "d"]);
    assert_eq!(bag(&db, &format!("MATCH ACYCLIC {far}")), ["d", "d"]);
    assert_eq!(bag(&db, &format!("MATCH TRAIL {far}")), ["d", "d"]);
    // Every end within two hops, each path once.
    assert_eq!(
        bag(
            &db,
            "MATCH (s IS site_a WHERE s._key = 'a')-[:r]->{1,2}(t) RETURN t._key AS t"
        ),
        ["b", "c", "d", "d"]
    );
    // Walked backwards from the far corner.
    assert_eq!(
        bag(
            &db,
            "MATCH (t IS site_a WHERE t._key = 'd')<-[:r]-{2,2}(s) RETURN s._key AS s"
        ),
        ["a", "a"]
    );
    // A selector keeps one of them.
    assert_eq!(bag(&db, &format!("MATCH ANY {far}")), ["d"]);
}

#[test]
fn a_direct_edge_does_not_hide_the_two_hop_route() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let from = "MATCH (s IS site_a WHERE s._key = 'p')-[:r]->";
    assert_eq!(
        bag(&db, &format!("{from}{{2,2}}(t) RETURN t._key AS t")),
        ["q"]
    );
    assert_eq!(
        bag(&db, &format!("{from}{{1,1}}(t) RETURN t._key AS t")),
        ["q", "x"]
    );
    assert_eq!(
        bag(&db, &format!("{from}{{1,2}}(t) RETURN t._key AS t")),
        ["q", "q", "x"]
    );
    // ANY SHORTEST reaches q at one hop and x at one hop; with `{2,2}` the
    // lower bound makes q's two-hop witness the only one.
    assert_eq!(
        bag(
            &db,
            &format!(
                "MATCH ANY SHORTEST (s IS site_a WHERE s._key = 'p')-[:r]->{{2,2}}(t) RETURN t._key AS t"
            )
        ),
        ["q"]
    );
}

#[test]
fn zero_length_quantifiers_bind_the_end_to_the_start() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let from = "MATCH (s IS site_a WHERE s._key = 'a')-[e:r]->";
    let ends = |q: &str| {
        bag(
            &db,
            &format!("{from}{q}(t) RETURN s._key AS s, t._key AS t"),
        )
    };
    assert_eq!(ends("{0,0}"), ["a|a"]);
    assert_eq!(ends("{0}"), ["a|a"]);
    assert_eq!(ends("{0,1}"), ["a|a", "a|b", "a|c"]);
    assert_eq!(ends("?"), ["a|a", "a|b", "a|c"]);
    // `*` needs a mode that ends (or a selector).
    assert_eq!(
        bag(
            &db,
            "MATCH ACYCLIC (s IS site_a WHERE s._key = 'a')-[:r]->*(t) RETURN t._key AS t"
        ),
        ["a", "b", "c", "d", "d"]
    );
    assert_eq!(
        bag(
            &db,
            "MATCH TRAIL (s IS site_a WHERE s._key = 'a')-[:r]->+(t) RETURN t._key AS t"
        ),
        ["b", "c", "d", "d"]
    );
    // Zero iterations test the start against BOTH endpoint patterns.
    assert_eq!(
        bag(
            &db,
            "MATCH (s IS site_a WHERE s._key = 'a')-[:r]->{0,1}(t WHERE t._key = 'a') RETURN t._key AS t"
        ),
        ["a"]
    );
    assert!(
        bag(
            &db,
            "MATCH (s IS site_a WHERE s._key = 'a')-[:r]->{0,0}(t IS site_b) RETURN t._key AS t"
        )
        .is_empty()
    );
}

#[test]
fn path_modes_on_a_cycle() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let from = "(s IS site_a WHERE s._key = 'c0')-[:r]->";
    assert_eq!(
        bag(&db, &format!("MATCH TRAIL {from}+(t) RETURN t._key AS t")),
        ["c0", "c1", "c2"]
    );
    assert_eq!(
        bag(&db, &format!("MATCH ACYCLIC {from}+(t) RETURN t._key AS t")),
        ["c1", "c2"]
    );
    // WALK, bounded: round the cycle and on.
    assert_eq!(
        bag(
            &db,
            &format!("MATCH WALK {from}{{4,5}}(t) RETURN t._key AS t")
        ),
        ["c1", "c2"]
    );
    // A repeated variable closes the cycle.
    assert_eq!(
        bag(
            &db,
            "MATCH TRAIL (s IS site_a WHERE s._key = 'c0')-[:r]->+(s) RETURN s._key AS s"
        ),
        ["c0"]
    );
}

#[test]
fn endpoint_labels_apply_at_their_positions_only() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // The intermediate node is a `site_c`; neither endpoint label tests it.
    assert_eq!(
        bag(
            &db,
            "MATCH (s IS site_a WHERE s._key = 'u')-[:r]->{2,2}(t IS site_a) RETURN t._key AS t"
        ),
        ["v"]
    );
    assert!(
        bag(
            &db,
            "MATCH (s IS site_a WHERE s._key = 'u')-[:r]->{2,2}(t IS site_c) RETURN t._key AS t"
        )
        .is_empty()
    );
    // A labelled node INSIDE a subpath tests every iteration.
    assert!(bag(
        &db,
        "MATCH (s IS site_a WHERE s._key = 'u')((x IS site_a)-[:r]->(y IS site_a)){2,2}(t) RETURN t._key AS t"
    )
    .is_empty());
}

#[test]
fn a_multi_type_subpath_repeats_as_a_whole() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    assert_eq!(
        bag(
            &db,
            "MATCH (s IS site_a WHERE s._key = 'g0')((x)-[:r]->(y)-[:s]->(z)){1,2}(t) RETURN t._key AS t"
        ),
        ["g2", "g4"]
    );
    // A subpath after a plain edge.
    assert_eq!(
        bag(
            &db,
            "MATCH (s IS site_a WHERE s._key = 'g0')-[:r]->((x)-[:s]->(y)-[:r]->(z)){1,1}(t) RETURN t._key AS t"
        ),
        ["g3"]
    );
}

#[test]
fn a_group_predicate_prunes_each_iteration() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // a->b (1), b->d (3), a->c (2), c->d (9): below 5 keeps the b route.
    assert_eq!(
        bag(
            &db,
            "MATCH (s IS site_a WHERE s._key = 'a')-[e:r WHERE e.w < 5]->{2,2}(t) RETURN t._key AS t"
        ),
        ["d"]
    );
    // A plain hop after the repeat is part of the same automaton, and its
    // inline predicate filters its own edge.
    assert_eq!(
        bag(
            &db,
            "MATCH (s IS site_a WHERE s._key = 'a')-[:r]->{1,1}(m)-[f:r WHERE f.w > 5]->(t) RETURN m._key AS m"
        ),
        ["c"]
    );
}

#[test]
fn any_shortest_takes_the_fewest_hops_once_per_end() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // Two routes to h9; the singleton `m` after the repeat shows the
    // witness: h1 (two hops), not k2 (three).
    assert_eq!(
        bag(
            &db,
            "MATCH p = ANY SHORTEST (s IS site_a WHERE s._key = 'h0')-[e:r]->{0,5}(m)-[:r]->(t WHERE t._key = 'h9') RETURN m._key AS m"
        ),
        ["h1"]
    );
    // One row per end, the start included (zero edges).
    assert_eq!(
        bag(
            &db,
            "MATCH ANY SHORTEST (s IS site_a WHERE s._key = 'h0')-[:r]->{0,32}(t) RETURN t._key AS t"
        ),
        ["h0", "h1", "h9", "k1", "k2"]
    );
    // The zero-edge path when source and target are one node.
    assert_eq!(
        bag(
            &db,
            "MATCH p = ANY SHORTEST (s IS site_a WHERE s._key = 'h0')-[e IS r]->{0,32}(t IS site_a WHERE t._key = 'h0') RETURN t._key AS t"
        ),
        ["h0"]
    );
    // An unbounded quantifier is admitted under a selector.
    assert_eq!(
        bag(
            &db,
            "MATCH ANY SHORTEST (s IS site_a WHERE s._key = 'c0')-[:r]->*(t) RETURN t._key AS t"
        ),
        ["c0", "c1", "c2"]
    );
}

#[test]
fn any_shortest_to_a_disconnected_or_missing_target_gives_zero_rows() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    for target in ["z", "nobody"] {
        assert!(
            bag(
                &db,
                &format!("MATCH ANY SHORTEST (s IS site_a WHERE s._key = 'a')-[:r]->{{0,32}}(t IS site_a WHERE t._key = '{target}') RETURN t._key AS t")
            )
            .is_empty(),
            "`{target}`"
        );
        assert!(
            bag(
                &db,
                &format!("MATCH ACYCLIC (s IS site_a WHERE s._key = 'a')-[:r]->*(t IS site_a WHERE t._key = '{target}') RETURN t._key AS t")
            )
            .is_empty(),
            "`{target}`"
        );
    }
}

#[test]
fn any_cheapest_keeps_the_hop_count_in_its_state() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // Within three hops only s->a (10) ->c ->t (cost 12) reaches t: the
    // cheaper arrival at a (s->b->a, cost 2) uses one hop too many. A
    // search that let the cheaper arrival dominate would find no row.
    let within = |hops: u32| {
        bag(
            &db,
            &format!(
                "MATCH p = ANY CHEAPEST (s IS site_b WHERE s._key = 's')-[e IS road COST e.w]->{{1,{hops}}}(t IS site_b WHERE t._key = 't') RETURN t._key AS t"
            ),
        )
    };
    assert_eq!(within(3), ["t"]);
    assert_eq!(within(4), ["t"]);
    assert!(within(2).is_empty());
    // One row per end.
    assert_eq!(
        bag(
            &db,
            "MATCH ANY CHEAPEST (s IS site_b WHERE s._key = 's')-[e IS road COST e.w]->{1,4}(t) RETURN t._key AS t"
        ),
        ["a", "b", "c", "t"]
    );
}

#[test]
fn any_cheapest_refuses_an_invalid_cost() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let error = run_with(
        &db,
        "MATCH ANY CHEAPEST (s IS site_b WHERE s._key = 'y')-[e IS road COST e.w]->{1,2}(t) RETURN t._key AS t",
    )
    .unwrap_err();
    assert!(error.to_string().contains("InvalidPathCost"), "{error}");
    // A constant COST is a hop count.
    assert_eq!(
        bag(
            &db,
            "MATCH ANY CHEAPEST (s IS site_b WHERE s._key = 'y')-[IS road COST 1]->{1,2}(t) RETURN t._key AS t"
        ),
        ["y1"]
    );
}

#[test]
fn a_point_to_point_cheapest_search_stops_before_crossing_an_edge_it_does_not_need() {
    // `t` is bound before the search, so the search ends at its first
    // witness. The zero-hop path already reaches `y`, so the invalid COST on
    // `y->y1` is never part of any answer and must not be evaluated.
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    assert_eq!(
        bag(
            &db,
            "MATCH (t IS site_b WHERE t._key = 'y'), ANY CHEAPEST (s IS site_b WHERE s._key = 'y')-[e IS road COST e.w]->{0,2}(t) RETURN t._key AS t"
        ),
        ["y"]
    );
}

#[test]
fn a_named_path_binds_and_is_not_a_column() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    assert_eq!(
        bag(
            &db,
            "MATCH p = (s IS site_a WHERE s._key = 'p')-[:r]->(t) RETURN t._key AS t"
        ),
        ["q", "x"]
    );
    let error = run_with(
        &db,
        "MATCH p = (s IS site_a WHERE s._key = 'p')-[:r]->{1,2}(t) RETURN p",
    )
    .unwrap_err();
    assert!(error.to_string().contains("a path"), "{error}");
}
