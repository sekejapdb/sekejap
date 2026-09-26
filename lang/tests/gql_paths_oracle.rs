//! M4-E, lang layer: a handful of extra path-search cases driven through
//! the real SQL entry point (`prepare_sql`), each checked against an
//! expectation computed independently of the engine -- either hand-traced
//! against the fixture below, or (for the selectors) a property, per
//! `docs/lang/GQL_PROFILE_DESIGN.md` §4 and the M4-E task row.
//!
//! The exhaustive random-graph oracle lives in
//! `core/engine/tests/gql_paths_oracle.rs`, driving `PathAutomaton`
//! directly. This file's job is narrower: confirm the FULL pipeline (GQL
//! parse, bind, plan, execute) agrees with that same independent
//! reasoning once it goes through `lang`'s automaton compiler (M4-A) too.
//!
//! Workload names are invented: collection `site_a`, edge type `r`.

use sekejap_core::collections::{Database, EntityId, GraphContextId};
use sekejap_core::Kind;
use sekejap_lang::{prepare_sql, SqlError, SqlResult, SqlValue};
use serde_json::json;
use tempfile::TempDir;

mod common;
use common::cfg;

/// A small IRREGULAR multigraph in one collection: a parallel pair of
/// edges (`n0`->`n1` twice, at different `w`), a self-loop (`n3`->`n3`)
/// and a cycle (`n0`->`n1`->`n3`->`n0`, and `n0`->`n2`->`n3`). `n4` is
/// isolated.
///
/// ```text
/// n0 --w1--> n1 --w1--> n3 --w1--> n0   (cycle through n1)
/// n0 --w5--> n1                          (parallel twin, costlier)
/// n0 --w1--> n2 --w1--> n3               (second route to n3)
/// n3 --w1--> n3                          (self-loop)
/// n4                                      (isolated)
/// ```
fn fixture(dir: &TempDir) -> Database {
    let mut db = Database::create(dir.path().join("g.sekejap"), cfg()).unwrap();
    let site_a = db
        .create_collection("site_a", vec![("w".into(), Kind::Int)], Default::default())
        .unwrap();
    let mut ids: Vec<(String, EntityId)> = Vec::new();
    let mut node = |db: &mut Database, key: &str| -> EntityId {
        if let Some((_, id)) = ids.iter().find(|(k, _)| k == key) {
            return *id;
        }
        let id = db.put(site_a, key, &json!({"w": 1})).unwrap();
        ids.push((key.to_owned(), id));
        id
    };
    let mut resolved = Vec::new();
    for (from, to, w) in [
        ("n0", "n1", 1),
        ("n0", "n1", 5), // parallel twin, costlier
        ("n0", "n2", 1),
        ("n1", "n3", 1),
        ("n2", "n3", 1),
        ("n3", "n3", 1), // self-loop
        ("n3", "n0", 1), // closes the cycle
    ] {
        let source = node(&mut db, from);
        let destination = node(&mut db, to);
        resolved.push((source, destination, w));
    }
    node(&mut db, "n4"); // isolated
    db.enable_graph().unwrap();
    let r = db.create_edge_type("r").unwrap();
    for (source, destination, w) in resolved {
        db.create_edge(
            GraphContextId::BASE,
            source,
            r,
            destination,
            &json!({"w": w}),
        )
        .unwrap();
    }
    db.commit().unwrap();
    db
}

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

/// Two sequential `?` (`{0,1}`) edges over a graph with exactly ONE real
/// edge x->y: the walk can be explained as (first edge taken, second not)
/// or (first not, second taken). Design Q11: unobservable, they dedup to
/// one match; once `b` (the node between them) is projected, the two
/// interpretations bind it differently (`b = x` or `b = y`) and both are
/// kept.
///
/// The THIRD, zero-edge interpretation (neither `?` taken, so `a = b = c`)
/// is a real candidate match too, but `c`'s own predicate (`c._key = 'y'`)
/// rules it out here, since `x != y` -- so only the dedup question is on
/// display, independently of the zero-length case (which the core file's
/// oracle and `gql_paths.rs`'s `zero_length_quantifiers_*` already cover).
#[test]
fn two_sequential_optional_edges_dedup_only_identical_bindings() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("g.sekejap"), cfg()).unwrap();
    let site_a = db
        .create_collection("site_a", vec![("w".into(), Kind::Int)], Default::default())
        .unwrap();
    let x = db.put(site_a, "x", &json!({"w": 1})).unwrap();
    let y = db.put(site_a, "y", &json!({"w": 1})).unwrap();
    db.enable_graph().unwrap();
    let r = db.create_edge_type("r").unwrap();
    db.create_edge(GraphContextId::BASE, x, r, y, &json!({"w": 1}))
        .unwrap();
    db.commit().unwrap();

    // A truly anonymous middle position (no variable at all): both
    // interpretations bind nothing observable, so they dedup to one match.
    let anon = "(a IS site_a WHERE a._key = 'x')-[:r]->?()-[:r]->?(c IS site_a WHERE c._key = 'y')";
    assert_eq!(
        bag(
            &db,
            &format!("MATCH {anon} RETURN a._key AS a, c._key AS c")
        ),
        ["x|y"],
        "no variable in the middle: the two interpretations must dedup to one match (Q11)"
    );
    // Naming the middle position (`b`) allocates it a slot, whether or not
    // `b` is projected: the two interpretations then bind that slot
    // differently (`b = x` or `b = y`), so both are distinct matches --
    // observable via the pattern's OWN binding, not only via RETURN.
    let named =
        "(a IS site_a WHERE a._key = 'x')-[:r]->?(b)-[:r]->?(c IS site_a WHERE c._key = 'y')";
    assert_eq!(
        bag(
            &db,
            &format!("MATCH {named} RETURN a._key AS a, b._key AS b, c._key AS c")
        ),
        ["x|x|y", "x|y|y"],
        "b observed: the two interpretations bind it differently and both must be kept (Q11)"
    );
}

/// `{3,3}` from `n0`: under WALK and TRAIL every one of the 3 two-hop
/// routes to `n3` extends twice more (self-loop, or back to `n0`), giving
/// 6 three-hop matches; under ACYCLIC every one of those 6 repeats a node
/// (`n3` via the self-loop, or `n0` via the cycle edge), so NONE survive.
/// Hand-traced against the fixture; independent of the engine's search.
#[test]
fn walk_trail_acyclic_disagree_on_a_graph_with_a_self_loop_and_a_cycle() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let from = "(s IS site_a WHERE s._key = 'n0')-[:r]->{3,3}";
    assert_eq!(
        bag(&db, &format!("MATCH {from}(t) RETURN t._key AS t")),
        ["n0", "n0", "n0", "n3", "n3", "n3"],
        "WALK: 3 self-loop closures, 3 cycle closures"
    );
    assert_eq!(
        bag(&db, &format!("MATCH TRAIL {from}(t) RETURN t._key AS t")),
        ["n0", "n0", "n0", "n3", "n3", "n3"],
        "TRAIL: none of the three edges used repeats within one path"
    );
    assert!(
        bag(&db, &format!("MATCH ACYCLIC {from}(t) RETURN t._key AS t")).is_empty(),
        "ACYCLIC: every three-hop route from n0 revisits a node"
    );
}

/// ANY SHORTEST from `n0`: exactly the reachable set (`n0` itself at zero
/// hops, `n1`/`n2` at one, `n3` at two), each once -- `n4` is isolated and
/// gives no row. Multiplicity from the parallel edge and the two routes to
/// `n3` must NOT appear; that is the property, not a specific witness.
#[test]
fn any_shortest_gives_the_reachable_set_once_each() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    assert_eq!(
        bag(
            &db,
            "MATCH ANY SHORTEST (s IS site_a WHERE s._key = 'n0')-[:r]->{0,10}(t) RETURN t._key AS t"
        ),
        ["n0", "n1", "n2", "n3"]
    );
}

/// ANY CHEAPEST must pick the cheaper of the two parallel `n0`->`n1`
/// edges (weight 1, not weight 5), even though both are one hop.
#[test]
fn any_cheapest_picks_the_cheaper_parallel_edge() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // A plain (unquantified) edge: `e` is a singleton here, not a group
    // variable, so `RETURN e.w` is projectable.
    assert_eq!(
        bag(
            &db,
            "MATCH ANY CHEAPEST (s IS site_a WHERE s._key = 'n0')-[e:r COST e.w]->(t IS site_a WHERE t._key = 'n1') RETURN e.w AS w"
        ),
        ["1"]
    );
}
