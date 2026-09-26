//! The normal form a parsed GQL body prints back (M2-A of
//! `docs/lang/GQL_PROFILE_DESIGN.md`): labels with `:`, every edge
//! bracketed, every binary operator parenthesised, unquoted variables
//! folded to lower case. Two spellings of one pattern print the same text,
//! and a misread precedence prints visibly different parentheses.
//!
//! These tests live inside the crate because the printer is not a public
//! entry point: a caller sees it only as the `statement:` line of a GQL
//! `EXPLAIN` (`lang/tests/gql_explain.rs`). What the parser REJECTS is
//! `lang/tests/gql_parse.rs`.
//!
//! Workload names are invented: `person`, `band`, `song`, `wrote`,
//! `performed`.

use crate::ast::Stmt;
use crate::{refusals, SqlError};

/// The normal form of the GQL body a statement's `FROM` names, or an error
/// when it names none.
fn normal_form(text: &str) -> crate::Result<String> {
    if let Stmt::Gql(graph) | Stmt::ExplainGql(graph) = crate::parser::parse(text)? {
        return Ok(graph.to_string());
    }
    Err(SqlError::unsupported("the statement has no GQL body"))
}

/// The statement around every body below; only the body differs.
fn statement(body: &str) -> String {
    format!("SELECT * FROM GRAPH_TABLE (g {body})")
}

/// The normal form of a body that must parse.
fn nf(body: &str) -> String {
    let text = statement(body);
    normal_form(&text).unwrap_or_else(|error| panic!("`{text}` did not parse: {error}"))
}

#[test]
fn colon_and_is_spell_the_same_label() {
    let colon = nf("MATCH (p:person)-[e:wrote]->(s:song) RETURN s.title AS t");
    let is = nf("MATCH (p IS person)-[e IS wrote]->(s IS song) RETURN s.title AS t");
    assert_eq!(
        colon,
        "GRAPH_TABLE (g MATCH (p:person)-[e:wrote]->(s:song) RETURN s.title AS t)"
    );
    assert_eq!(is, colon);
    // Mixed in one pattern.
    assert_eq!(
        nf("MATCH (p:person)-[e IS wrote]->(s:song) RETURN s.title AS t"),
        colon
    );
}

#[test]
fn anonymous_elements_bind_no_variable_in_either_spelling() {
    let expected = "GRAPH_TABLE (g MATCH (:person)-[:wrote]->(:song) RETURN 1 AS one)";
    assert_eq!(nf("MATCH (:person)-[:wrote]->(:song) RETURN 1 AS one"), expected);
    // An anonymous `IS` edge and an anonymous `IS` node.
    assert_eq!(
        nf("MATCH (IS person)-[IS wrote]->(IS song) RETURN 1 AS one"),
        expected
    );
    // No variable and no label at all.
    assert_eq!(
        nf("MATCH ()-[]->() RETURN 1 AS one"),
        "GRAPH_TABLE (g MATCH ()-[]->() RETURN 1 AS one)"
    );
    // An anonymous element may still carry an inline WHERE.
    assert_eq!(
        nf("MATCH (p:person)-[IS wrote WHERE p.born > 1990]->(WHERE p.born < 2000) RETURN p.name AS n"),
        "GRAPH_TABLE (g MATCH (p:person)-[:wrote WHERE (p.born > 1990)]->(WHERE (p.born < 2000)) RETURN p.name AS n)"
    );
}

#[test]
fn a_label_alternation_is_kept_in_order_on_nodes_and_edges() {
    let expected =
        "GRAPH_TABLE (g MATCH (p:person|band)-[e:wrote|performed]->(s:song) RETURN s.title AS t)";
    assert_eq!(
        nf("MATCH (p:person|band)-[e:wrote|performed]->(s:song) RETURN s.title AS t"),
        expected
    );
    assert_eq!(
        nf("MATCH (p IS person|band)-[e IS wrote|performed]->(s IS song) RETURN s.title AS t"),
        expected
    );
    assert_eq!(
        nf("MATCH (:person|band|song) RETURN 1 AS one"),
        "GRAPH_TABLE (g MATCH (:person|band|song) RETURN 1 AS one)"
    );
}

#[test]
fn a_label_keeps_its_case_and_a_variable_folds() {
    assert_eq!(
        nf("MATCH (P:Person)-[E:Wrote]->(\"S\":song) RETURN P.Title AS T, \"S\".x AS y"),
        "GRAPH_TABLE (g MATCH (p:Person)-[e:Wrote]->(S:song) RETURN p.Title AS t, S.x AS y)"
    );
}

#[test]
fn the_three_directions_full_and_abbreviated() {
    for (written, printed) in [
        ("(a)-[e]->(b)", "(a)-[e]->(b)"),
        ("(a)<-[e]-(b)", "(a)<-[e]-(b)"),
        ("(a)-[e]-(b)", "(a)-[e]-(b)"),
        ("(a)->(b)", "(a)-[]->(b)"),
        ("(a)<-(b)", "(a)<-[]-(b)"),
        ("(a)-(b)", "(a)-[]-(b)"),
        ("(a)-[:wrote]->(b)", "(a)-[:wrote]->(b)"),
        ("(a)<-[:wrote]-(b)", "(a)<-[:wrote]-(b)"),
        ("(a)-[:wrote]-(b)", "(a)-[:wrote]-(b)"),
    ] {
        assert_eq!(
            nf(&format!("MATCH {written} RETURN b.x AS x")),
            format!("GRAPH_TABLE (g MATCH {printed} RETURN b.x AS x)"),
            "{written}"
        );
    }
}

#[test]
fn a_multi_hop_chain_mixes_directions() {
    assert_eq!(
        nf("MATCH (a:person)-[e:wrote]->(s:song)<-[f:performed]-(b:band)-[g2]-(c) RETURN c.x AS x"),
        "GRAPH_TABLE (g MATCH (a:person)-[e:wrote]->(s:song)<-[f:performed]-(b:band)-[g2]-(c) RETURN c.x AS x)"
    );
}

#[test]
fn inline_element_where_on_nodes_and_edges() {
    assert_eq!(
        nf("MATCH (p:person WHERE p._key = $1)-[e:wrote WHERE e.year >= 2000]->(s:song WHERE s.title <> 'x') RETURN s.title AS t"),
        "GRAPH_TABLE (g MATCH (p:person WHERE (p._key = $1))-[e:wrote WHERE (e.year >= 2000)]->(s:song WHERE (s.title <> 'x')) RETURN s.title AS t)"
    );
}

#[test]
fn a_pattern_where_follows_the_patterns_and_binds_or_below_and_below_not() {
    assert_eq!(
        nf("MATCH (a)-[e]->(b) WHERE a.x = 1 OR NOT b.y < 2 AND e.z != $2 RETURN b.y AS y"),
        "GRAPH_TABLE (g MATCH (a)-[e]->(b) WHERE ((a.x = 1) OR ((NOT (b.y < 2)) AND (e.z <> $2))) RETURN b.y AS y)"
    );
    // Parentheses override the default binding.
    assert_eq!(
        nf("MATCH (a) WHERE (a.x = 1 OR a.x = 2) AND NOT (a.y <= $1) RETURN a.x AS x"),
        "GRAPH_TABLE (g MATCH (a) WHERE (((a.x = 1) OR (a.x = 2)) AND (NOT (a.y <= $1))) RETURN a.x AS x)"
    );
    // AND and OR associate to the left.
    assert_eq!(
        nf("MATCH (a) WHERE a.x = 1 AND a.y = 2 AND a.z = 3 RETURN a.x AS x"),
        "GRAPH_TABLE (g MATCH (a) WHERE (((a.x = 1) AND (a.y = 2)) AND (a.z = 3)) RETURN a.x AS x)"
    );
}

#[test]
fn every_comparison_operator_and_every_literal() {
    assert_eq!(
        nf("MATCH (a) WHERE a.b = TRUE AND a.c <> FALSE AND a.d < -2.5 AND a.e <= 7 AND a.f > 'it''s' AND a.g >= $12 AND a.h = NULL AND a.i = 1.0 RETURN a.b AS b"),
        "GRAPH_TABLE (g MATCH (a) WHERE ((((((((a.b = TRUE) AND (a.c <> FALSE)) AND (a.d < -2.5)) AND (a.e <= 7)) AND (a.f > 'it''s')) AND (a.g >= $12)) AND (a.h = NULL)) AND (a.i = 1.0)) RETURN a.b AS b)"
    );
}

#[test]
fn parameters_are_value_expressions_in_every_predicate_position() {
    assert_eq!(
        nf("MATCH (p:person WHERE p._key = $1)-[e:wrote WHERE e.year >= $2]->(s WHERE s.rank < $3) WHERE s.x <> $4 RETURN $5 AS five, s.title AS t"),
        "GRAPH_TABLE (g MATCH (p:person WHERE (p._key = $1))-[e:wrote WHERE (e.year >= $2)]->(s WHERE (s.rank < $3)) WHERE (s.x <> $4) RETURN $5 AS five, s.title AS t)"
    );
}

#[test]
fn comma_separated_patterns_share_variables() {
    assert_eq!(
        nf("MATCH (a:person)-[:wrote]->(s:song), (b:band)-[:performed]->(s), (c) RETURN s.title AS t"),
        "GRAPH_TABLE (g MATCH (a:person)-[:wrote]->(s:song), (b:band)-[:performed]->(s), (c) RETURN s.title AS t)"
    );
}

#[test]
fn a_repeated_variable_is_kept_where_it_was_written() {
    // A cycle back to the first node, and a repeated edge variable across
    // two patterns: both are identity constraints for the binder (M2-C).
    assert_eq!(
        nf("MATCH (a)-[e]->(b)-[f]->(a), (c)-[e]->(d) RETURN a.x AS x"),
        "GRAPH_TABLE (g MATCH (a)-[e]->(b)-[f]->(a), (c)-[e]->(d) RETURN a.x AS x)"
    );
    // Folding makes two spellings one variable.
    assert_eq!(
        nf("MATCH (A)-[e]->(a) RETURN A.x AS x"),
        "GRAPH_TABLE (g MATCH (a)-[e]->(a) RETURN a.x AS x)"
    );
}

#[test]
fn two_match_statements_and_a_bare_return() {
    assert_eq!(
        nf("MATCH (a:person) MATCH (a)-[:wrote]->(s) RETURN s.title AS t"),
        "GRAPH_TABLE (g MATCH (a:person) MATCH (a)-[:wrote]->(s) RETURN s.title AS t)"
    );
    assert_eq!(
        nf("RETURN 1 AS one, 'x' AS two"),
        "GRAPH_TABLE (g RETURN 1 AS one, 'x' AS two)"
    );
}

#[test]
fn return_items_with_and_without_alias() {
    assert_eq!(
        nf("MATCH (a)-[e]->(b) RETURN a, e.w, b.name AS \"Name\", a.x = b.x AS same"),
        "GRAPH_TABLE (g MATCH (a)-[e]->(b) RETURN a, e.w, b.name AS Name, (a.x = b.x) AS same)"
    );
}

#[test]
fn the_relation_alias_follows_the_body() {
    assert_eq!(
        normal_form("SELECT * FROM GRAPH_TABLE (base MATCH (a) RETURN a.t AS t) AS g").unwrap(),
        "GRAPH_TABLE (base MATCH (a) RETURN a.t AS t) AS g"
    );
    // Without `AS`, as SQL names a relation in FROM.
    assert_eq!(
        normal_form("SELECT * FROM GRAPH_TABLE (base MATCH (a) RETURN a.t AS t) g").unwrap(),
        "GRAPH_TABLE (base MATCH (a) RETURN a.t AS t) AS g"
    );
}

#[test]
fn the_outer_select_reads_alias_columns_and_its_clauses_in_either_page_order() {
    // M3-D: `alias.column` is the relation's column; LIMIT and OFFSET in
    // either order print in one; a key may be a select-list position.
    let expected = "SELECT t AS x, COUNT(*) FROM GRAPH_TABLE (base MATCH (a) RETURN a.t AS t) AS g \
                    WHERE (t > 1) GROUP BY t ORDER BY 2 DESC, t OFFSET 2 LIMIT $1";
    for tail in ["LIMIT $1 OFFSET 2", "OFFSET 2 LIMIT $1"] {
        assert_eq!(
            normal_form(&format!(
                "SELECT g.t AS x, count(*) FROM GRAPH_TABLE (base MATCH (a) RETURN a.t AS t) AS g \
                 WHERE g.t > 1 GROUP BY g.t ORDER BY 2 DESC, t {tail}"
            ))
            .unwrap(),
            expected,
            "{tail}"
        );
    }
    // An alias without `AS` in the select list.
    assert_eq!(
        normal_form("SELECT g.t x FROM GRAPH_TABLE (base MATCH (a) RETURN a.t AS t) AS g").unwrap(),
        "SELECT t AS x FROM GRAPH_TABLE (base MATCH (a) RETURN a.t AS t) AS g"
    );
}

// M2-E deleted `a_columns_body_still_takes_the_old_path`: it pinned that a
// `COLUMNS` body still took the pre-M2-E dual-path parser (`parse_sql(...)`
// succeeded, `normal_form(...)` failed because the tree was not GQL's). That
// dual path is gone -- a `GRAPH_TABLE (...)` body is parsed as GQL only --
// so a `COLUMNS` body is refused by name now, pinned by
// `lang/tests/gql_parse.rs::a_columns_body_is_refused_by_name_naming_return`.

#[test]
fn an_sql_refusal_word_is_an_ordinary_property_name_inside_the_body() {
    for (keyword, _, _) in refusals() {
        let word = keyword.to_ascii_lowercase();
        if !word.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let body = format!("MATCH (a WHERE a.{word} = 1) RETURN a.{word} AS v");
        assert_eq!(
            nf(&body),
            format!("GRAPH_TABLE (g MATCH (a WHERE (a.{word} = 1)) RETURN a.{word} AS v)"),
        );
    }
}

/// Operator precedence and the scalar pack print as PostgreSQL parses them
/// (M3-C; moved here with the normal-form printer).
#[test]
fn precedence_is_postgresql_s() {
    for (written, parsed) in [
        ("1 + 2 * 3", "(1 + (2 * 3))"),
        ("(1 + 2) * 3", "((1 + 2) * 3)"),
        ("10 - 2 - 3", "((10 - 2) - 3)"),
        ("2 * 3 ^ 2", "(2 * (3 ^ 2))"),
        ("2 ^ 3 ^ 2", "((2 ^ 3) ^ 2)"),
        ("-a.x ^ 2", "((-a.x) ^ 2)"),
        ("-2 ^ 2", "(-2 ^ 2)"),
        ("a.x || 'b' || 'c'", "((a.x || 'b') || 'c')"),
        ("a.x + 1 || 'b'", "((a.x + 1) || 'b')"),
        ("-a.x::int", "(-CAST(a.x AS BIGINT))"),
        ("a.x + 1 = 2", "((a.x + 1) = 2)"),
        ("a.x = 1 IS NULL", "((a.x = 1) IS NULL)"),
        ("a.x IN (1, 2) = TRUE", "((a.x IN (1, 2)) = TRUE)"),
        ("NOT a.x IS NULL", "(NOT (a.x IS NULL))"),
        ("a.x NOT IN (1)", "(a.x NOT IN (1))"),
        (
            "CASE a.x WHEN 1 THEN 'a' ELSE 'b' END",
            "CASE a.x WHEN 1 THEN 'a' ELSE 'b' END",
        ),
        (
            "cast(a.x AS double precision)",
            "CAST(a.x AS DOUBLE PRECISION)",
        ),
        ("coalesce(a.x, 0)", "COALESCE(a.x, 0)"),
        ("substring(a.x FROM 2 FOR 3)", "SUBSTRING(a.x, 2, 3)"),
    ] {
        let text = format!("SELECT * FROM GRAPH_TABLE (g MATCH (a) RETURN {written} AS v)");
        let got = normal_form(&text).unwrap_or_else(|error| panic!("`{written}`: {error}"));
        assert_eq!(
            got,
            format!("GRAPH_TABLE (g MATCH (a) RETURN {parsed} AS v)"),
            "`{written}`"
        );
    }
}

// ── quantifiers and path prefixes (M4-A) ─────────────────────────────────

/// The pattern part of a body's normal form: between `MATCH ` and ` RETURN`.
fn pattern(body: &str) -> String {
    let full = nf(body);
    let start = full.find("MATCH ").expect("a MATCH") + "MATCH ".len();
    let end = full.find(" RETURN").expect("a RETURN");
    full[start..end].to_owned()
}

#[test]
fn quantifiers_on_an_edge_print_as_one_bound_pair() {
    for (written, normal) in [
        ("{1,3}", "{1,3}"),
        ("{ 0 , 32 }", "{0,32}"),
        ("{2}", "{2,2}"),
        ("{0}", "{0,0}"),
        ("{2,}", "{2,}"),
        ("?", "{0,1}"),
        ("*", "{0,}"),
        ("+", "{1,}"),
    ] {
        assert_eq!(
            pattern(&format!(
                "MATCH TRAIL (a)-[e:r]->{written}(b) RETURN b._key AS k"
            )),
            format!("TRAIL (a)-[e:r]->{normal}(b)"),
            "`{written}`"
        );
    }
    // Every direction, and the abbreviated edges.
    assert_eq!(
        pattern("MATCH (a)<-[e]-{1,2}(b)-[f]-?(c)->{3}(d)<-+(x) RETURN a._key AS k"),
        "(a)<-[e]-{1,2}(b)-[f]-{0,1}(c)-[]->{3,3}(d)<-[]-{1,}(x)"
    );
}

#[test]
fn quantifiers_on_a_parenthesised_subpath_repeat_the_whole_subpath() {
    assert_eq!(
        pattern("MATCH (s)((a)-[:r]->(b)-[:s]->(c)){1,3}(t) RETURN t._key AS k"),
        "(s)((a)-[:r]->(b)-[:s]->(c)){1,3}(t)"
    );
    // A subpath may open and close the pattern, and follow or precede an
    // edge; its quantifier takes every spelling.
    assert_eq!(
        pattern("MATCH ((a)-[e]->(b))* RETURN a._key AS k"),
        "((a)-[e]->(b)){0,}"
    );
    assert_eq!(
        pattern("MATCH (s)-[:r]->((a)-[e]->(b)){2}-[:s]->(t) RETURN t._key AS k"),
        "(s)-[:r]->((a)-[e]->(b)){2,2}-[:s]->(t)"
    );
}

#[test]
fn prefixes_path_variable_modes_and_selectors() {
    for (written, normal) in [
        (
            "p = ANY SHORTEST (a)-[e IS r]->{0,32}(b)",
            "p = ANY SHORTEST (a)-[e:r]->{0,32}(b)",
        ),
        (
            "p = any cheapest (a)-[e IS r COST e.w]->{0,32}(b)",
            "p = ANY CHEAPEST (a)-[e:r COST e.w]->{0,32}(b)",
        ),
        (
            "ANY CHEAPEST (a)-[e WHERE e.w > 0 COST e.w * 2]->{1,}(b)",
            "ANY CHEAPEST (a)-[e WHERE (e.w > 0) COST (e.w * 2)]->{1,}(b)",
        ),
        ("ANY (a)-[e]->*(b)", "ANY (a)-[e]->{0,}(b)"),
        ("WALK (a)-[e]->{1,2}(b)", "WALK (a)-[e]->{1,2}(b)"),
        ("TRAIL (a)-[e]->+(b)", "TRAIL (a)-[e]->{1,}(b)"),
        ("p = ACYCLIC (a)-[e]->*(b)", "p = ACYCLIC (a)-[e]->{0,}(b)"),
        ("p = (a)-[e]->(b)", "p = (a)-[e]->(b)"),
    ] {
        assert_eq!(
            pattern(&format!("MATCH {written} RETURN b._key AS k")),
            normal,
            "`{written}`"
        );
    }
    // Comma patterns each carry their own prefix.
    assert_eq!(
        pattern("MATCH p = ANY (a)-[e]->*(b), TRAIL (b)-[f]->+(c) RETURN c._key AS k"),
        "p = ANY (a)-[e]->{0,}(b), TRAIL (b)-[f]->{1,}(c)"
    );
}

#[test]
fn prefix_words_are_keywords_by_position_only() {
    // A variable may be called `any`, `trail`, `cost` would be structural.
    assert_eq!(
        pattern("MATCH (any)-[trail]->(shortest) RETURN shortest._key AS k"),
        "(any)-[trail]->(shortest)"
    );
    assert_eq!(
        pattern("MATCH walk = WALK (acyclic)-[e]->{1,2}(b) RETURN b._key AS k"),
        "walk = WALK (acyclic)-[e]->{1,2}(b)"
    );
}

/// The stage grammar (M3-B) prints in one spelling: `ASC` is the default
/// and is not printed, keywords are upper case, and `NEXT` joins stages.
#[test]
fn the_stage_grammar_prints_in_one_spelling() {
    assert_eq!(
        nf("FOR k IN $1 MATCH (a WHERE a._key = k) LET x = a.n, y = 2 LET z = x + y \
            filter z > 1 RETURN distinct a, count(*) AS n, count(distinct a.m) AS m, array_agg(x) AS xs \
            group by a order by n desc, a.n asc offset 1 limit $2 \
            next return sum(n) AS total"),
        "GRAPH_TABLE (g FOR k IN $1 MATCH (a WHERE (a._key = k)) LET x = a.n, y = 2 LET z = (x + y) \
         FILTER (z > 1) RETURN DISTINCT a, COUNT(*) AS n, COUNT(DISTINCT a.m) AS m, ARRAY_AGG(x) AS xs \
         GROUP BY a ORDER BY n DESC, a.n OFFSET 1 LIMIT $2 NEXT RETURN SUM(n) AS total)"
    );
    assert_eq!(nf("MATCH (a) RETURN *"), "GRAPH_TABLE (g MATCH (a) RETURN *)");
}

/// `OPTIONAL MATCH` (M3-F) prints as one statement, its `WHERE` with it; a
/// variable may still be called `optional`.
#[test]
fn optional_match_prints_with_its_where() {
    assert_eq!(
        nf("match (a) optional match (a)-[e:wrote]->(s:song) where s.year > 2000 \
            optional match p = any shortest (s)-[:performed]-{1,2}(b) RETURN s, b"),
        "GRAPH_TABLE (g MATCH (a) OPTIONAL MATCH (a)-[e:wrote]->(s:song) WHERE (s.year > 2000) \
         OPTIONAL MATCH p = ANY SHORTEST (s)-[:performed]-{1,2}(b) RETURN s, b)"
    );
    assert_eq!(
        nf("MATCH (optional)-[e]->(b) RETURN optional._key AS k"),
        "GRAPH_TABLE (g MATCH (optional)-[e]->(b) RETURN optional._key AS k)"
    );
}
