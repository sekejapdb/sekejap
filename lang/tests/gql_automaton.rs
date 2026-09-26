//! GQL path patterns as the parser and the automaton compiler read them
//! (M4-A of `docs/lang/GQL_PROFILE_DESIGN.md` §4.1, §4.3-§4.5).
//!
//! What is at risk, and the test that pins it:
//!
//! * a quantifier written where none can stand, and a path variable after
//!   the selector, are syntax errors (the spellings that DO parse, and the
//!   `{m,n}` form they print, are pinned in-crate by
//!   `lang/src/gql/parse/tests.rs`);
//! * the refusals by name: a parameter as a bound (brief §7), a nested
//!   quantifier (Q10), a selector together with a path mode (§4.3), an
//!   unbounded `WALK` without a selector (§4.2), a group variable named
//!   after its quantifier, `COST` without `ANY CHEAPEST` and the reverse,
//!   `ANY CHEAPEST` over more than one edge step (`refusals_*`).
//!
//! The compiled automaton's exact shape -- states, links, repeats, groups,
//! mode -- is pinned by the unit tests of `lang/src/gql/automaton.rs`, which
//! can see the crate-private plan; the answers it gives are pinned by
//! `lang/tests/gql_paths.rs`.
//!
//! Workload names are invented: collection `site`, edge types `r`, `s`.

use sekejap_core::Kind;
use sekejap_core::collections::{Database, GraphContextId};
use sekejap_lang::{gql_refusals, parse_sql, prepare_sql, SqlError, SqlResult, SqlValue, Tier};
use serde_json::json;
use tempfile::TempDir;

mod common;
use common::cfg;

fn statement(body: &str) -> String {
    format!("SELECT * FROM GRAPH_TABLE (g {body})")
}

fn syntax_error(body: &str) -> String {
    let text = statement(body);
    match parse_sql(&text) {
        Err(SqlError::Syntax { message, .. }) => message,
        other => panic!("`{text}` was not a syntax error: {other:?}"),
    }
}

fn refused(body: &str) -> (String, Tier, &'static str) {
    let text = statement(body);
    match parse_sql(&text) {
        Err(SqlError::Refused {
            keyword,
            tier,
            reason,
        }) => (keyword, tier, reason),
        other => panic!("`{text}` was not refused by name: {other:?}"),
    }
}

/// `site` n0 -r-> n1 -s-> n2, each edge with `w`.
fn fixture(dir: &TempDir) -> Database {
    let mut db = Database::create(dir.path().join("g.sekejap"), cfg()).unwrap();
    let site = db
        .create_collection(
            "site",
            vec![("w".to_owned(), Kind::Int)],
            Default::default(),
        )
        .unwrap();
    let n: Vec<_> = (0..3)
        .map(|i| db.put(site, &format!("n{i}"), &json!({"w": i})).unwrap())
        .collect();
    db.enable_graph().unwrap();
    let r = db.create_edge_type("r").unwrap();
    let s = db.create_edge_type("s").unwrap();
    let base = GraphContextId::BASE;
    db.create_edge(base, n[0], r, n[1], &json!({"w": 1}))
        .unwrap();
    db.create_edge(base, n[1], s, n[2], &json!({"w": 2}))
        .unwrap();
    db.commit().unwrap();
    db
}

/// The rows of one GQL body, through the SQL entry point.
#[derive(Debug)]
struct Answer {
    rows: Vec<Vec<SqlValue>>,
}

fn run(db: &Database, body: &str) -> Result<Answer, SqlError> {
    let text = format!("SELECT * FROM GRAPH_TABLE (base {body})");
    match prepare_sql(db, &text, &[])?.run(db)? {
        SqlResult::Rows { rows, .. } => Ok(Answer {
            rows: rows.into_iter().map(|row| row.values).collect(),
        }),
        other => panic!("`{text}` answered {other:?}"),
    }
}

/// The compile error of a body over the fixture.
fn compile_error(db: &Database, body: &str) -> String {
    match run(db, body) {
        Ok(answer) => panic!("`{body}` compiled and ran: {:?}", answer.rows),
        Err(error) => error.to_string(),
    }
}

/// A body over the fixture that must compile and run.
fn compiles(db: &Database, body: &str) {
    run(db, body).unwrap_or_else(|error| panic!("`{body}`: {error}"));
}

// ── quantifiers ────────────────────────────────────────────────────────────

#[test]
fn a_quantifier_written_where_none_can_stand_is_a_syntax_error() {
    // On a node pattern.
    assert!(syntax_error("MATCH (a){2}-[e]->(b) RETURN b._key AS k").contains("quantifier"));
    // A parenthesised subpath without one.
    assert!(syntax_error("MATCH (s)((a)-[e]->(b))(t) RETURN t._key AS k").contains("quantifier"));
    // Upper bound below the lower.
    assert!(syntax_error("MATCH (a)-[e]->{3,1}(b) RETURN b._key AS k").contains("upper bound"));
    // Not an integer.
    assert!(
        syntax_error("MATCH (a)-[e]->{1.5,2}(b) RETURN b._key AS k").contains("integer literal")
    );
    assert!(
        syntax_error("MATCH (a)-[e]->{-1,2}(b) RETURN b._key AS k").contains("integer literal")
    );
}

// ── prefixes ───────────────────────────────────────────────────────────────

#[test]
fn the_path_variable_goes_before_the_selector() {
    let message = syntax_error("MATCH ANY SHORTEST p = (a)-[e]->*(b) RETURN b._key AS k");
    assert!(message.contains("before the selector"), "{message}");
}

// ── refusals ───────────────────────────────────────────────────────────────

#[test]
fn refusals_a_parameter_is_not_a_quantifier_bound() {
    for bound in ["{1,$2}", "{$1,3}", "{$1}", "{$1,}"] {
        let message = syntax_error(&format!("MATCH (a)-[e]->{bound}(b) RETURN b._key AS k"));
        assert!(
            message.contains("integer literal") && message.contains("parameter"),
            "`{bound}`: {message}"
        );
    }
}

#[test]
fn refusals_a_nested_quantifier_is_refused_by_name() {
    for body in [
        "MATCH ((a)-[e]->{1,2}(b)){1,2} RETURN b._key AS k",
        "MATCH (((a)-[e]->(b)){2}){1,2} RETURN b._key AS k",
        "MATCH (s)((a)-[e]->*(b)-[f]->(c))+(t) RETURN t._key AS k",
    ] {
        let (keyword, tier, reason) = refused(body);
        assert_eq!(keyword, "nested quantifier", "`{body}`");
        assert_eq!(tier, Tier::Two);
        assert!(reason.contains("P1") && reason.contains("Q10"), "{reason}");
    }
}

#[test]
fn refusals_a_selector_and_a_path_mode_together_are_refused_by_name() {
    for body in [
        "MATCH ANY SHORTEST TRAIL (a)-[e]->*(b) RETURN b._key AS k",
        "MATCH p = ANY ACYCLIC (a)-[e]->*(b) RETURN b._key AS k",
        "MATCH TRAIL ANY SHORTEST (a)-[e]->*(b) RETURN b._key AS k",
    ] {
        let (keyword, tier, reason) = refused(body);
        assert_eq!(keyword, "selector with a path mode", "`{body}`");
        assert_eq!(tier, Tier::Two);
        assert!(reason.contains("P1"), "{reason}");
    }
}

#[test]
fn refusals_the_m4_a_rows_are_gone_and_the_new_rows_are_p1() {
    for gone in [
        "quantifier",
        "parenthesized path pattern",
        "named path",
        "WALK",
        "TRAIL",
        "ACYCLIC",
        "ANY",
        "ANY SHORTEST",
        "ANY CHEAPEST",
        "COST",
    ] {
        assert!(
            gql_refusals().iter().all(|(name, _, _)| *name != gone),
            "`{gone}` is built and is no longer a refusal"
        );
    }
    assert!(
        gql_refusals()
            .iter()
            .all(|(_, _, reason)| !reason.contains("M4-A"))
    );
}

#[test]
fn refusals_an_unbounded_walk_without_a_selector_is_refused_naming_the_rule() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    for body in [
        "MATCH (a WHERE a._key = 'n0')-[e]->*(b) RETURN b._key AS k",
        "MATCH WALK (a WHERE a._key = 'n0')-[e]->{2,}(b) RETURN b._key AS k",
        "MATCH (a WHERE a._key = 'n0')((x)-[:r]->(y))+(b) RETURN b._key AS k",
    ] {
        let message = compile_error(&db, body);
        assert!(
            message.contains("unbounded")
                && message.contains("TRAIL")
                && message.contains("selector"),
            "`{body}`: {message}"
        );
    }
    // Each of the three ways out compiles.
    for body in [
        "MATCH TRAIL (a WHERE a._key = 'n0')-[e]->*(b) RETURN b._key AS k",
        "MATCH ACYCLIC (a WHERE a._key = 'n0')-[e]->*(b) RETURN b._key AS k",
        "MATCH ANY SHORTEST (a WHERE a._key = 'n0')-[e]->*(b) RETURN b._key AS k",
    ] {
        compiles(&db, body);
    }
}

#[test]
fn refusals_a_group_variable_is_a_list_outside_its_quantifier() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    for body in [
        // An inline predicate AFTER the repeat (owner decision).
        "MATCH (a WHERE a._key = 'n0')-[e]->{1,2}(b WHERE b.w = e.w) RETURN b._key AS k",
        // A predicate before it.
        "MATCH (a WHERE a._key = 'n0' AND a.w < e.w)-[e]->{1,2}(b) RETURN b._key AS k",
        // The MATCH's WHERE.
        "MATCH (a WHERE a._key = 'n0')-[e]->{1,2}(b) WHERE e.w > 1 RETURN b._key AS k",
        // RETURN.
        "MATCH (a WHERE a._key = 'n0')-[e]->{1,2}(b) RETURN e.w AS w",
        // A node group variable of a subpath.
        "MATCH (a WHERE a._key = 'n0')((x)-[:r]->(y)){1,2}(b WHERE b.w > x.w) RETURN b._key AS k",
    ] {
        let message = compile_error(&db, body);
        assert!(
            message.contains("group variable") && message.contains("horizontal aggregate"),
            "`{body}`: {message}"
        );
    }
    // Inside its own quantifier it is one element per iteration.
    for body in [
        "MATCH (a WHERE a._key = 'n0')-[e WHERE e.w > 0]->{1,2}(b) RETURN b._key AS k",
        "MATCH (a WHERE a._key = 'n0')((x)-[f:r WHERE f.w > x.w - 5]->(y WHERE y.w > x.w)){1,2}(b) RETURN b._key AS k",
    ] {
        compiles(&db, body);
    }
}

#[test]
fn refusals_a_group_variable_is_bound_at_one_position() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    for body in [
        "MATCH (a WHERE a._key = 'n0')-[e]->{1,2}(b)-[e]->(c) RETURN c._key AS k",
        "MATCH (a WHERE a._key = 'n0')-[e]->(b)-[e]->{1,2}(c) RETURN c._key AS k",
        "MATCH (a WHERE a._key = 'n0')((x)-[]->(x)){1,2}(c) RETURN c._key AS k",
    ] {
        let message = compile_error(&db, body);
        assert!(message.contains("group variable"), "`{body}`: {message}");
    }
}

#[test]
fn refusals_cost_belongs_to_any_cheapest() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let cases = [
        // COST without the selector that reads it.
        (
            "MATCH (a WHERE a._key = 'n0')-[e COST e.w]->{1,2}(b) RETURN b._key AS k",
            "ANY CHEAPEST",
        ),
        // ... on a plain chain of hops too, which weighs nothing.
        (
            "MATCH (a WHERE a._key = 'n0')-[e COST e.w]->(b) RETURN b._key AS k",
            "ANY CHEAPEST",
        ),
        (
            "MATCH ANY SHORTEST (a WHERE a._key = 'n0')-[e COST e.w]->{1,2}(b) RETURN b._key AS k",
            "ANY CHEAPEST",
        ),
        // The selector without a COST.
        (
            "MATCH ANY CHEAPEST (a WHERE a._key = 'n0')-[e]->{1,2}(b) RETURN b._key AS k",
            "COST",
        ),
        // More than one edge step (M4-C: one COST): the P1 row of
        // `refuse::GQL_TABLE` (design Q36), refused by name.
        (
            "MATCH ANY CHEAPEST (a WHERE a._key = 'n0')-[e COST e.w]->{1,2}(b)-[f COST f.w]->(c) RETURN c._key AS k",
            "ANY CHEAPEST over several edge steps",
        ),
        // A COST that reads another element.
        (
            "MATCH ANY CHEAPEST (a WHERE a._key = 'n0')-[e COST a.w]->{1,2}(b) RETURN b._key AS k",
            "its own edge",
        ),
    ];
    for (body, names) in cases {
        let message = compile_error(&db, body);
        assert!(message.contains(names), "`{body}`: {message}");
    }
}
