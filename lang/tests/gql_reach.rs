//! `Reach`: when the existing node BFS may stand in for a path search
//! (`docs/lang/GQL_PROFILE_DESIGN.md` §4.6), driven through `prepare_sql`
//! and `explain_sql`.
//!
//! The node BFS keeps one visited set on NODES: it reports each node once,
//! at its first depth, never reports its start at a depth of one or more,
//! and loses every path, multiplicity and later depth. The planner puts it
//! in place of a `PathSearch` only when all seven rules hold, and `EXPLAIN`
//! says which rules admitted it or which rule failed.
//!
//! What is at risk, and the test that pins it:
//!
//! * each rule, broken on its own: `Reach` is NOT chosen, `EXPLAIN` names
//!   that rule, and the answer is the path answer, which differs from what
//!   the node BFS would give (`rule_1_*` .. `rule_7_*`);
//! * `lo = 1` with a cycle back to the start: `WALK` and `TRAIL` reach the
//!   start again and keep the path search, `ACYCLIC` does not and takes the
//!   BFS (`rule_5_*`);
//! * a diamond under a `DISTINCT` endpoint gives the same answer both ways
//!   (`a_diamond_under_distinct_*`);
//! * an endpoint predicate is a filter AFTER the BFS, never a prune of the
//!   walk (`an_endpoint_predicate_*`);
//! * every admitted query answers what the path search answers: each one is
//!   run again with an unused path variable `p = ...`, which breaks rule 1
//!   and changes no answer, and the two answers are compared -- on fixed
//!   cases and on seeded random graphs (`admitted_queries_*`).
//!
//! Rows come back in no promised order without an `ORDER BY`, so every
//! answer is compared sorted.
//!
//! Workload names are invented: collections `site_a`, `site_b`, edge types
//! `r`, `s`.

use sekejap_core::collections::{CollectionId, Database, EntityId, GraphContextId};
use sekejap_core::Kind;
use sekejap_lang::{explain_sql, prepare_sql, SqlResult, SqlValue};
use serde_json::json;
use tempfile::TempDir;

mod common;
use common::cfg;

/// One edge of a fixture: (collection, key) -> (collection, key), its
/// type and its `w`.
type Edge<'a> = ((bool, &'a str), (bool, &'a str), &'a str, i64);

/// A database of `site_a` and `site_b` nodes (`true` is `site_b`), each
/// with `w` and `v`, and the given edges. `values` sets a node's `v`
/// (default 1).
fn database(dir: &TempDir, edges: &[Edge<'_>], values: &[(&str, i64)]) -> Database {
    let mut db = Database::create(dir.path().join("g.sekejap"), cfg()).unwrap();
    let fields = || vec![("w".to_owned(), Kind::Int), ("v".to_owned(), Kind::Int)];
    let site_a = db
        .create_collection("site_a", fields(), Default::default())
        .unwrap();
    let site_b = db
        .create_collection("site_b", fields(), Default::default())
        .unwrap();
    let mut ids: Vec<(CollectionId, String, EntityId)> = Vec::new();
    let mut node = |db: &mut Database, b: bool, key: &str| -> EntityId {
        let collection = if b { site_b } else { site_a };
        if let Some((_, _, id)) = ids.iter().find(|(c, k, _)| *c == collection && k == key) {
            return *id;
        }
        let v = values
            .iter()
            .find(|(k, _)| *k == key)
            .map_or(1, |(_, v)| *v);
        let id = db.put(collection, key, &json!({"w": 1, "v": v})).unwrap();
        ids.push((collection, key.to_owned(), id));
        id
    };
    let mut resolved = Vec::new();
    for ((fb, from), (tb, to), t, w) in edges {
        let source = node(&mut db, *fb, from);
        let destination = node(&mut db, *tb, to);
        resolved.push((source, *t, destination, *w));
    }
    node(&mut db, false, "z");
    db.enable_graph().unwrap();
    let r = db.create_edge_type("r").unwrap();
    let s = db.create_edge_type("s").unwrap();
    for (source, t, destination, w) in resolved {
        let edge_type = if t == "r" { r } else { s };
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

/// ```text
/// site_a, type r (w on each edge):
///   diamond   a->b (1)  a->c (2)  b->d (3)  c->d (9)
///   two-hop   p->q      p->x      x->q
///   cycle     c0->c1    c1->c2    c2->c0
///   values    f0->f1 (v 0)->f2    f0->f3
///   labels    m -> site_b mb -> m
///   chain     l0->l1->...->l69
///   isolated  z
/// ```
fn fixture(dir: &TempDir) -> Database {
    let a = |k: &'static str| (false, k);
    let mut edges: Vec<Edge<'static>> = vec![
        (a("a"), a("b"), "r", 1),
        (a("a"), a("c"), "r", 2),
        (a("b"), a("d"), "r", 3),
        (a("c"), a("d"), "r", 9),
        (a("p"), a("q"), "r", 1),
        (a("p"), a("x"), "r", 1),
        (a("x"), a("q"), "r", 1),
        (a("c0"), a("c1"), "r", 1),
        (a("c1"), a("c2"), "r", 1),
        (a("c2"), a("c0"), "r", 1),
        (a("f0"), a("f1"), "r", 1),
        (a("f1"), a("f2"), "r", 1),
        (a("f0"), a("f3"), "r", 1),
        (a("m"), (true, "mb"), "r", 1),
        ((true, "mb"), a("m"), "r", 1),
    ];
    let chain: Vec<&'static str> = (0..70)
        .map(|i| &*Box::leak(format!("l{i}").into_boxed_str()))
        .collect();
    for pair in chain.windows(2) {
        edges.push((a(pair[0]), a(pair[1]), "r", 1));
    }
    database(dir, &edges, &[("f1", 0)])
}

fn statement(body: &str) -> String {
    format!("SELECT * FROM GRAPH_TABLE (base {body})")
}

/// The answer as sorted text rows.
fn answer(db: &Database, body: &str) -> Vec<String> {
    let text = statement(body);
    let rows = match prepare_sql(db, &text, &[])
        .and_then(|prepared| prepared.run(db))
        .unwrap_or_else(|error| panic!("`{body}` failed: {error}"))
    {
        SqlResult::Rows { rows, .. } => rows,
        other => panic!("`{body}` answered {other:?}"),
    };
    let mut rows: Vec<String> = rows
        .into_iter()
        .map(|row| {
            row.values
                .iter()
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

fn explain(db: &Database, body: &str) -> String {
    explain_sql(db, &statement(body), &[])
        .unwrap_or_else(|error| panic!("EXPLAIN `{body}`: {error}"))
}

/// `body` runs as `Reach`, and answers `expected`.
fn reach(db: &Database, body: &str, expected: &[&str]) -> String {
    let plan = explain(db, body);
    assert!(
        plan.contains(". Reach from ") && !plan.contains(". PathSearch from "),
        "`{body}` is not a Reach:\n{plan}"
    );
    assert_eq!(answer(db, body), expected, "`{body}`");
    plan
}

/// `body` keeps its `PathSearch`, `EXPLAIN` names rule `rule` as the one
/// that failed, and it answers `expected`.
fn refused(db: &Database, body: &str, rule: u8, expected: &[&str]) -> String {
    let plan = explain(db, body);
    assert!(
        plan.contains(". PathSearch from ") && !plan.contains(". Reach from "),
        "`{body}` is not a PathSearch:\n{plan}"
    );
    let line = format!("not Reach: rule {rule} fails");
    assert!(plan.contains(&line), "`{body}`: no `{line}` in:\n{plan}");
    assert_eq!(answer(db, body), expected, "`{body}`");
    plan
}

/// `body` with an unused path variable on its last pattern: rule 1 fails,
/// so it is the path search's answer to the same question.
fn twisted(body: &str) -> String {
    let at = body.rfind("MATCH ").expect("a MATCH") + "MATCH ".len();
    let (head, tail) = body.split_at(at);
    format!("{head}p = {tail}")
}

/// `body` runs as `Reach` and answers exactly what the path search answers.
fn same_both_ways(db: &Database, body: &str, expected: &[&str]) {
    reach(db, body, expected);
    refused(db, &twisted(body), 1, expected);
}

const FROM_A: &str = "(s IS site_a WHERE s._key = 'a')";

#[test]
fn rule_1_shape_one_quantified_edge_and_nothing_else() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // A two-edge body: the node BFS over `r` in 1..1 hops would give b and
    // c, in 1..2 hops b, c and d; the body reaches d only.
    refused(
        &db,
        &format!("MATCH {FROM_A}((x)-[:r]->(y)-[:r]->(z)){{1,1}}(t) RETURN DISTINCT t._key AS t"),
        1,
        &["d"],
    );
    // An edge predicate: the BFS takes none that the language evaluates,
    // and without it would give b, c and d.
    refused(
        &db,
        &format!("MATCH {FROM_A}-[e:r WHERE e.w > 1]->{{1,2}}(t) RETURN DISTINCT t._key AS t"),
        1,
        &["c", "d"],
    );
    // Two edge types: the BFS walks one.
    refused(
        &db,
        &format!("MATCH {FROM_A}-[:r|s]->{{1,2}}(t) RETURN DISTINCT t._key AS t"),
        1,
        &["b", "c", "d"],
    );
    // A named path: the node BFS has no path to give it.
    refused(
        &db,
        &format!("MATCH p = {FROM_A}-[:r]->{{1,2}}(t) RETURN DISTINCT t._key AS t"),
        1,
        &["b", "c", "d"],
    );
}

#[test]
fn rule_2_an_end_bound_before_the_search_keeps_the_path_search() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // The end is the start: the walk returns to c0 in three hops, and the
    // node BFS never reports its start at a depth of one or more.
    refused(
        &db,
        "MATCH (s IS site_a WHERE s._key = 'c0')-[:r]->{1,3}(s) RETURN DISTINCT s._key AS s",
        2,
        &["c0"],
    );
}

#[test]
fn rule_3_a_group_variable_read_downstream_keeps_the_path_search() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // COUNT(e) is the path length: q is reached in one hop and in two; the
    // node BFS knows q at its first depth only.
    refused(
        &db,
        "MATCH (s IS site_a WHERE s._key = 'p')-[e:r]->{1,2}(t) LET n = COUNT(e) RETURN DISTINCT t._key AS t, n",
        3,
        &["q|1", "q|2", "x|1"],
    );
}

#[test]
fn rule_4_a_bag_of_rows_keeps_the_path_search() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // Two paths to d: two rows. The node BFS reports d once.
    refused(
        &db,
        &format!("MATCH {FROM_A}-[:r]->{{1,2}}(t) RETURN t._key AS t"),
        4,
        &["b", "c", "d", "d"],
    );
    // A row count sees the multiplicity too.
    refused(
        &db,
        &format!("MATCH {FROM_A}-[:r]->{{1,2}}(t) RETURN COUNT(*) AS n"),
        4,
        &["4"],
    );
    // A LIMIT keeps rows in the order they arrive, and the BFS delivers
    // them in another: conservatively refused.
    refused(
        &db,
        &format!("MATCH {FROM_A}-[:r]->{{1,2}}(t) RETURN DISTINCT t._key AS t ORDER BY t LIMIT 2"),
        4,
        &["b", "c"],
    );
    // ANY CHEAPEST evaluates COST on every edge it relaxes.
    refused(
        &db,
        &format!("MATCH ANY CHEAPEST {FROM_A}-[e:r COST e.w]->{{1,2}}(t) RETURN t._key AS t"),
        4,
        &["b", "c", "d"],
    );
}

#[test]
fn rule_5_lo_two_is_never_the_node_bfs() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // q is two hops away through x as well as one hop away directly: the
    // node BFS sees it at depth 1 and never at depth 2.
    refused(
        &db,
        "MATCH ACYCLIC (s IS site_a WHERE s._key = 'p')-[:r]->{2,2}(t) RETURN DISTINCT t._key AS t",
        5,
        &["q"],
    );
}

#[test]
fn rule_5_lo_one_on_a_cycle_back_to_the_start_walk_versus_acyclic() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let cycle = "(s IS site_a WHERE s._key = 'c0')-[:r]->{1,3}(t) RETURN DISTINCT t._key AS t";
    // WALK and TRAIL come back to c0 in three hops; the node BFS would not
    // report c0.
    refused(&db, &format!("MATCH {cycle}"), 5, &["c0", "c1", "c2"]);
    refused(&db, &format!("MATCH TRAIL {cycle}"), 5, &["c0", "c1", "c2"]);
    // ACYCLIC cannot return to its start: the node BFS is exact.
    same_both_ways(&db, &format!("MATCH ACYCLIC {cycle}"), &["c1", "c2"]);
    // A cycle back to a start the end's labels exclude: exact under WALK.
    same_both_ways(
        &db,
        "MATCH (s IS site_a WHERE s._key = 'm')-[:r]->{1,2}(t IS site_b) RETURN DISTINCT t._key AS t",
        &["mb"],
    );
    // lo = 0 reports the start at zero hops, as the BFS does.
    same_both_ways(
        &db,
        "MATCH (s IS site_a WHERE s._key = 'c0')-[:r]->{0,3}(t) RETURN DISTINCT t._key AS t",
        &["c0", "c1", "c2"],
    );
}

#[test]
fn rule_6_a_node_predicate_inside_the_repeat_keeps_the_path_search() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // Every intermediate node must have v > 0: f1 (v 0) blocks f2. The node
    // BFS, filtering the end only, would reach f1, f2 and f3.
    refused(
        &db,
        "MATCH ACYCLIC (s IS site_a WHERE s._key = 'f0')((x)-[:r]->(y WHERE y.v > 0)){1,2}(t) RETURN DISTINCT t._key AS t",
        6,
        &["f3"],
    );
}

#[test]
fn an_endpoint_predicate_filters_after_the_bfs_and_never_prunes_it() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // f2 is reached THROUGH f1 (v 0). A BFS pruning at the endpoint
    // predicate would stop at f1 and lose f2.
    let plan = reach(
        &db,
        "MATCH ACYCLIC (s IS site_a WHERE s._key = 'f0')-[:r]->{1,2}(t WHERE t.v > 0) RETURN DISTINCT t._key AS t",
        &["f2", "f3"],
    );
    assert!(
        plan.contains("end test after the BFS, per reached node: (t.v > 0)"),
        "{plan}"
    );
    same_both_ways(
        &db,
        "MATCH ACYCLIC (s IS site_a WHERE s._key = 'f0')-[:r]->{1,2}(t WHERE t.v > 0) RETURN DISTINCT t._key AS t",
        &["f2", "f3"],
    );
}

#[test]
fn rule_7_beyond_the_bfs_depth_keeps_the_path_search() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let from = "MATCH ACYCLIC (s IS site_a WHERE s._key = 'l0')-[:r]->";
    // The node BFS stops at 64 hops: it would count 64.
    refused(
        &db,
        &format!("{from}{{1,65}}(t) RETURN COUNT(DISTINCT t) AS n"),
        7,
        &["65"],
    );
    refused(
        &db,
        &format!("{from}{{1,}}(t) RETURN COUNT(DISTINCT t) AS n"),
        7,
        &["69"],
    );
    // Within 64 hops it is exact.
    same_both_ways(
        &db,
        &format!("{from}{{1,64}}(t) RETURN COUNT(DISTINCT t) AS n"),
        &["64"],
    );
}

#[test]
fn a_diamond_under_distinct_gives_the_same_answer_both_ways() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let diamond = format!("{FROM_A}-[:r]->{{1,2}}(t) RETURN DISTINCT t._key AS t");
    same_both_ways(&db, &format!("MATCH ACYCLIC {diamond}"), &["b", "c", "d"]);
    // Under WALK, lo = 1 could come back to the start (rule 5): the path
    // search keeps it, with the same answer here.
    refused(&db, &format!("MATCH {diamond}"), 5, &["b", "c", "d"]);
    // The selector is the consumer: one row per end.
    same_both_ways(
        &db,
        &format!("MATCH ANY SHORTEST {FROM_A}-[:r]->{{0,2}}(t) RETURN t._key AS t"),
        &["a", "b", "c", "d"],
    );
    same_both_ways(
        &db,
        &format!("MATCH ANY {FROM_A}-[:r]->{{0,2}}(t) RETURN t._key AS t"),
        &["a", "b", "c", "d"],
    );
    // An identity fold, grouped by the start.
    same_both_ways(
        &db,
        &format!(
            "MATCH ACYCLIC {FROM_A}-[:r]->{{1,2}}(t) RETURN s._key AS s, COUNT(DISTINCT t) AS n"
        ),
        &["a|3"],
    );
    // Backwards, and in either direction.
    same_both_ways(
        &db,
        "MATCH ACYCLIC (s IS site_a WHERE s._key = 'd')<-[:r]-{1,2}(t) RETURN DISTINCT t._key AS t",
        &["a", "b", "c"],
    );
    same_both_ways(
        &db,
        "MATCH ACYCLIC (s IS site_a WHERE s._key = 'b')-[:r]-{1,2}(t) RETURN DISTINCT t._key AS t",
        &["a", "c", "d"],
    );
}

#[test]
fn reach_inside_optional_match_and_after_next() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // z reaches nothing: the NULL row, both ways.
    same_both_ways(
        &db,
        "MATCH (s IS site_a WHERE s._key = 'z') OPTIONAL MATCH ACYCLIC (s)-[:r]->{1,2}(t) RETURN DISTINCT s._key AS s, t._key AS t",
        &["z|NULL"],
    );
    // The consumer in a later stage.
    same_both_ways(
        &db,
        &format!(
            "MATCH ACYCLIC {FROM_A}-[:r]->{{1,2}}(t) RETURN t._key AS k NEXT RETURN DISTINCT k"
        ),
        &["b", "c", "d"],
    );
}

#[test]
fn explain_prints_the_rules_that_admitted_reach_or_the_one_that_failed() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let plan = reach(
        &db,
        &format!("MATCH ACYCLIC {FROM_A}-[:r]->{{1,2}}(t) RETURN DISTINCT t._key AS t"),
        &["b", "c", "d"],
    );
    assert!(
        plan.contains("2. Reach from s to t: the node BFS"),
        "{plan}"
    );
    assert!(
        plan.contains("admitted by rules 1, 2, 3, 4 (a Distinct), 5 (ACYCLIC), 6, 7 of GQL_PROFILE_DESIGN §4.6"),
        "{plan}"
    );
    let plan = refused(
        &db,
        &format!("MATCH {FROM_A}-[:r]->{{2,3}}(t) RETURN DISTINCT t._key AS t"),
        5,
        &["d"],
    );
    assert!(plan.contains("2. PathSearch from s: every path"), "{plan}");
}

// ── seeded random graphs ──────────────────────────────────────────────────

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn admitted_queries_answer_what_the_path_search_answers_on_random_graphs() {
    let keys: Vec<String> = (0..12).map(|i| format!("n{i}")).collect();
    let mut checked = 0;
    for seed in 1..=6u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let dir = TempDir::new().unwrap();
        let mut edges: Vec<Edge<'_>> = Vec::new();
        for _ in 0..30 {
            let from = rng.below(12) as usize;
            let to = rng.below(12) as usize;
            let t = if rng.below(3) == 0 { "s" } else { "r" };
            edges.push((
                (from >= 8, keys[from].as_str()),
                (to >= 8, keys[to].as_str()),
                t,
                1 + rng.below(4) as i64,
            ));
        }
        let values: Vec<(&str, i64)> = keys
            .iter()
            .map(|k| (k.as_str(), rng.below(3) as i64))
            .collect();
        let db = database(&dir, &edges, &values);
        for _ in 0..12 {
            let arrow = ["-[:r]->", "<-[:r]-", "-[:r]-", "-[]->"][rng.below(4) as usize];
            let lo = rng.below(2);
            let hi = lo + rng.below(4);
            let consumer = rng.below(3);
            let predicate = if rng.below(2) == 0 {
                " WHERE t.v > 0"
            } else {
                ""
            };
            // Rule 5: lo = 1 needs ACYCLIC or an end the start cannot be; a
            // selector runs under WALK, so it needs the end's label.
            let labelled = rng.below(2) == 0 || (lo == 1 && consumer == 2);
            let end = if labelled { "t IS site_b" } else { "t" };
            let mode = match (lo == 1 && !labelled, rng.below(3)) {
                (true, _) | (false, 1) => "ACYCLIC ",
                (false, 0) => "",
                _ => "TRAIL ",
            };
            let pattern = format!("(s IS site_a){arrow}{{{lo},{hi}}}({end}{predicate})");
            let body = match consumer {
                0 => format!("MATCH {mode}{pattern} RETURN DISTINCT s._key AS s, t._key AS t"),
                1 => format!("MATCH {mode}{pattern} RETURN s._key AS s, COUNT(DISTINCT t) AS n"),
                _ => format!("MATCH ANY SHORTEST {pattern} RETURN s._key AS s, t._key AS t"),
            };
            let expected = answer(&db, &twisted(&body));
            let expected: Vec<&str> = expected.iter().map(String::as_str).collect();
            same_both_ways(&db, &body, &expected);
            checked += 1;
        }
    }
    assert_eq!(checked, 72);
}
