//! The GQL body of `GRAPH_TABLE`, as the parser reads it (M2-A of
//! `docs/lang/GQL_PROFILE_DESIGN.md`).
//!
//! A `GRAPH_TABLE (...)` body is parsed as GQL only (owner decision 1, M2-E):
//! the SQL/PGQ `COLUMNS (...)` body is removed, with no compatibility alias.
//! What the GQL body accepts is pinned by the normal form the parsed tree
//! prints back (`lang/src/gql/parse/tests.rs`, inside the crate, because that
//! printer is reached from outside only as `EXPLAIN`'s `statement:` line);
//! this file pins what it REJECTS: syntax errors and refusals.
//!
//! Everything the profile will build later is REFUSED here by name, with
//! the milestone that builds it (`gql_refusals()`), never skipped and never
//! approximated. And the SQL surface outside a GQL relation is unchanged:
//! every row of the SQL refusal table is refused exactly as before, and none
//! of them is given in the outer SELECT over a relation, which is read in
//! the GQL dialect (M3-D).
//!
//! Workload names are invented: `person`, `band`, `song`, `wrote`,
//! `performed`.

use sekejap_lang::{gql_refusals, parse_sql, refusals, SqlError, Tier};

/// The statement around every body below; only the body differs.
fn statement(body: &str) -> String {
    format!("SELECT t FROM GRAPH_TABLE (g {body})")
}

/// The refusal a body must produce.
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

fn syntax_error(body: &str) -> String {
    let text = statement(body);
    match parse_sql(&text) {
        Err(SqlError::Syntax { message, .. }) => message,
        other => panic!("`{text}` was not a syntax error: {other:?}"),
    }
}

// ── directions ─────────────────────────────────────────────────────────────

#[test]
fn a_two_sided_arrow_is_not_one_of_the_three_directions() {
    let message = syntax_error("MATCH (a)<-[e]->(b) RETURN b.x AS x");
    assert!(message.contains("`-`"), "{message}");
    // `<->` is one token, pgvector's L2 operator; it is no direction here.
    syntax_error("MATCH (a)<->(b) RETURN b.x AS x");
}

// ── predicates ─────────────────────────────────────────────────────────────

#[test]
fn a_comparison_does_not_chain() {
    let message = syntax_error("MATCH (a) WHERE a.x = 1 = 2 RETURN a.x AS x");
    assert!(message.contains("chain"), "{message}");
}

#[test]
fn a_property_reference_is_one_variable_and_one_property() {
    let message = syntax_error("MATCH (a) RETURN a.b.c AS x");
    assert!(message.contains("variable.property"), "{message}");
}

// ── patterns and variables ─────────────────────────────────────────────────

#[test]
fn a_pattern_is_a_node_then_edge_node_pairs() {
    syntax_error("MATCH -[e]->(b) RETURN b.x AS x");
    syntax_error("MATCH (a)-[e]-> RETURN a.x AS x");
    syntax_error("MATCH (a)-[e]->(b), RETURN a.x AS x");
    syntax_error("MATCH (a) RETURN");
    syntax_error("MATCH (a) RETURN a.x AS");
}

// ── the SQL/PGQ COLUMNS body is removed (owner decision, M2-E) ────────────

/// M2-E: the legacy `GRAPH_TABLE (g MATCH ... COLUMNS (...))` body is gone.
/// A `COLUMNS` written where a GQL body stands -- alone, or together with a
/// `RETURN` -- is refused by name, naming `RETURN` as the replacement,
/// rather than falling through to a parser that no longer exists or ending
/// as a syntax error at whatever byte the GQL grammar happened to choke on.
#[test]
fn a_columns_body_is_refused_by_name_naming_return() {
    for body in [
        // The legacy body alone: no RETURN at all.
        "MATCH (a)-[e]->(b) COLUMNS (b.x AS y)",
        // The hybrid the profile never adopted either.
        "MATCH (a)-[e]->(b) RETURN b.x AS x COLUMNS (b.x AS y)",
        "MATCH (a)-[e]->(b) COLUMNS (b.x AS y) RETURN b.x AS x",
    ] {
        let (keyword, tier, reason) = refused(body);
        assert_eq!(keyword, "COLUMNS", "{body}");
        assert_eq!(tier, Tier::Three);
        assert!(reason.contains("RETURN"), "{reason}");
    }
}

/// Only the projection `COLUMNS (...)` is the removed body: a variable or a
/// property that happens to be called `columns` is ordinary GQL.
#[test]
fn a_variable_or_property_named_columns_is_not_the_removed_body() {
    for body in [
        "MATCH (columns) RETURN columns.x AS x",
        "MATCH (a) RETURN a.columns AS c",
        "MATCH (a WHERE a.columns = 1) RETURN a.x AS x",
    ] {
        let text = statement(body);
        parse_sql(&text).unwrap_or_else(|error| panic!("`{text}`: {error}"));
    }
}

// ── refusals ───────────────────────────────────────────────────────────────

/// Every construct the profile will build later, written where a statement
/// puts it, with the keyword and the milestone its refusal must name.
const REFUSED: &[(&str, &str, &str)] = &[
    // P1: after the P0 release
    ("MATCH (a) WHERE search(a.y, 'kuta') RETURN a.y AS y", "search()", "P1"),
    ("MATCH (a) RETURN search_score() AS s", "search()", "P1"),
    (
        "OPTIONAL { MATCH (a)-[e]->(b) } RETURN b.y AS y",
        "OPTIONAL block",
        "P1",
    ),
    (
        "MATCH (a) OPTIONAL CALL (a) { MATCH (a)-[e]->(b) RETURN b.y AS z } RETURN a.y AS y",
        "OPTIONAL CALL",
        "P1",
    ),
    (
        "MATCH (a) CALL (a) { MATCH (a)-[e]->(b) RETURN b.y AS z NEXT RETURN z } RETURN a.y AS y",
        "NEXT inside CALL",
        "P1",
    ),
    ("MATCH ALL SHORTEST (a)-[e]->(b) RETURN b.y AS y", "ALL SHORTEST", "P1"),
    ("MATCH SIMPLE (a)-[e]->(b) RETURN b.y AS y", "SIMPLE", "P1"),
    ("MATCH ANY SHORTEST TRAIL (a)-[e]->*(b) RETURN b.y AS y", "selector with a path mode", "P1"),
    ("MATCH ((a)-[e]->{1,2}(b)){1,2} RETURN b.y AS y", "nested quantifier", "P1"),
    ("MATCH (a:person&band) RETURN a.y AS y", "label conjunction", "P1"),
    ("MATCH (a:!person) RETURN a.y AS y", "label negation", "P1"),
    ("MATCH (a:%) RETURN a.y AS y", "label wildcard", "P1"),
    ("MATCH (a) RETURN a.y AS y INTERSECT MATCH (b) RETURN b.y AS y", "INTERSECT", "P1"),
    ("MATCH (a) RETURN a.y AS y EXCEPT MATCH (b) RETURN b.y AS y", "EXCEPT", "P1"),
    (
        "MATCH (a) RETURN a.y AS y UNION MATCH (b) RETURN b.y AS y UNION ALL MATCH (c) RETURN c.y AS y",
        "mixed UNION",
        "P1",
    ),
    // Not adopted: another spelling exists.
    ("MATCH (a) RETURN path_sum(a) AS n", "PATH_SUM", "not adopted"),
    ("MATCH (a) RETURN path_product(a) AS n", "PATH_PRODUCT", "not adopted"),
    ("MATCH (a) RETURN path_min(a) AS n", "PATH_MIN", "not adopted"),
    ("MATCH (a) RETURN path_max(a) AS n", "PATH_MAX", "not adopted"),
    ("MATCH (a) RETURN path_avg(a) AS n", "PATH_AVG", "not adopted"),
    ("MATCH (a) RETURN vertex_id(a) AS n", "VERTEX_ID", "not adopted"),
    ("MATCH (a)-[e]->(b) RETURN edge_id(e) AS n", "EDGE_ID", "not adopted"),
    ("MATCH (a)-[e]->(b) RETURN b.x AS x COLUMNS (b.x AS y)", "COLUMNS", "not adopted"),
];

#[test]
fn every_later_construct_is_refused_by_name_with_its_milestone() {
    for (body, keyword, milestone) in REFUSED {
        let (got, tier, reason) = refused(body);
        assert_eq!(got, *keyword, "`{body}` was refused as `{got}`: {reason}");
        assert!(
            reason.contains(milestone),
            "`{body}`: the reason for `{keyword}` does not name {milestone}: {reason}"
        );
        let expected_tier = if *milestone == "not adopted" {
            Tier::Three
        } else {
            Tier::Two
        };
        assert_eq!(tier, expected_tier, "`{body}`");
        let row = gql_refusals()
            .iter()
            .find(|(name, _, _)| name == keyword)
            .unwrap_or_else(|| panic!("`{keyword}` is not a row of the GQL table"));
        assert_eq!(row.2, reason, "`{keyword}`");
    }
}

/// The rows a statement reaches only when it is BOUND (they need the
/// pattern's shape, not its words), with the test that reaches each.
const BOUND: &[(&str, &str)] = &[(
    "ANY CHEAPEST over several edge steps",
    "gql_automaton.rs::refusals_cost_belongs_to_any_cheapest",
)];

#[test]
fn the_gql_table_is_well_formed_and_every_row_is_reached() {
    let mut seen: Vec<&str> = Vec::new();
    for (keyword, tier, reason) in gql_refusals() {
        assert!(!seen.contains(keyword), "`{keyword}` appears twice");
        seen.push(keyword);
        assert!(reason.starts_with("GQL profile"), "`{keyword}`: {reason}");
        let named = ["M5", "M6", "P1"]
            .iter()
            .any(|m| reason.contains(m));
        match tier {
            Tier::Two => assert!(named, "`{keyword}` names no milestone: {reason}"),
            Tier::Three => assert!(reason.contains("not adopted"), "`{keyword}`: {reason}"),
        }
        assert!(
            REFUSED.iter().any(|(_, name, _)| name == keyword) || BOUND.iter().any(|(name, _)| name == keyword),
            "no statement above reaches the GQL row `{keyword}`"
        );
    }
}

// ── the SQL surface is unchanged ───────────────────────────────────────────

/// The generic plain-SQL position `sql_refusals.rs` puts a row in.
fn sql_tail(keyword: &str) -> String {
    if keyword
        .chars()
        .next()
        .is_some_and(|c| !c.is_ascii_alphanumeric())
    {
        return format!("WHERE name {keyword} 'x'");
    }
    format!("WHERE {keyword}")
}

#[test]
fn every_sql_refusal_is_unchanged_in_sql_and_never_given_in_the_outer_select_of_a_gql_relation() {
    // A row refused at PARSE is refused as itself; a row refused later (at
    // compile, where the catalog decides) parses. `sql_refusals.rs` and
    // `refusal_by_name.rs` pin the full refusal of every row.
    //
    // The outer SELECT over a GQL relation is read in the GQL dialect
    // (design §5.3, §5.5; M3-D): its WHERE is a GQL expression, where a
    // word the SQL table lists is an ordinary name -- or the GQL table's
    // refusal -- and never the SQL table's reason, which describes the
    // collection SELECT's atomics, not the relation's. (An operator GQL
    // does not have, `name ~ 'x'`, ends the WHERE expression before it;
    // what follows the outer SELECT is the statement's own tail, which the
    // SQL table still reads.)
    let mut refused_at_parse = 0;
    for (keyword, tier, reason) in refusals() {
        if keyword.starts_with("CREATE ") {
            continue;
        }
        let tail = sql_tail(keyword);
        let plain = parse_sql(&format!("SELECT _id FROM place {tail}"));
        if let Err(SqlError::Refused {
            keyword: got,
            tier: got_tier,
            reason: got_reason,
        }) = &plain
        {
            assert_eq!((got.as_str(), *got_tier, *got_reason), (*keyword, *tier, *reason));
            refused_at_parse += 1;
        }
        let after_gql = parse_sql(&format!(
            "SELECT t FROM GRAPH_TABLE (g MATCH (a) RETURN a.t AS t) AS g {tail}"
        ));
        let operator = !keyword.starts_with(|c: char| c.is_ascii_alphanumeric());
        if let (false, Err(SqlError::Refused { reason: got, .. })) = (operator, &after_gql) {
            assert!(
                gql_refusals().iter().any(|(_, _, gql)| gql == got),
                "`{tail}` after a GQL relation gave an SQL-table reason: {got}"
            );
        }
    }
    assert!(refused_at_parse > 40, "only {refused_at_parse} rows refused at parse");
}
