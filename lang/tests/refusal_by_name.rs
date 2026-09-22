//! A listed construct is refused BY NAME where a statement actually writes
//! it -- not only where someone remembered to test for it.
//!
//! `lang/tests/sql_refusals.rs` already puts every row of `refuse::TABLE`
//! into one generic position (a bare `WHERE <word>`) and checks that the
//! refusal comes back. That position is not where SQL puts most of these
//! words, and the gap it hid was real: `INTERSECT` and `EXCEPT` stand after a
//! SELECT's `LIMIT`, `TRAIL` / `WALK` / `SIMPLE` stand inside a
//! `GRAPH_TABLE` pattern, and in those places the parser ended the statement
//! as `42601 syntax error` even though the table held a row with a tier and a
//! reason (`docs/lang/QL_CONTRACT.md` §7 item 10).
//!
//! So this file asks the harder question. It is driven FROM the table --
//! every row is visited, none is named by hand -- and each row is written
//! into the position a statement would really put it in, from
//! [`NATURAL_POSITION`], falling back to the generic embedding only for the
//! rows whose only position IS a bare predicate. A row added to the table
//! later without a parser path that reaches it fails here.
//!
//! The eighth law of `docs/core/FOUNDATION_TEST_STANDARD.md` is what is under
//! test: a construct with no atomic is refused with a named reason, never
//! emulated -- and, this file adds, never disguised as a typo.

#[path = "sqlslice/fixture.rs"]
mod fixture;

use sekejap_lang::SqlDatabase;
use sekejap_lang::{refusals, SqlError, Tier};
use tempfile::TempDir;

fn open() -> (TempDir, fixture::Fixture) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let f = fixture::build(&path);
    (dir, f)
}

/// The seed of every `GRAPH_TABLE` statement below, so the pattern under test
/// is the only thing that differs between them.
const SEED: &str = "(a:place WHERE a._key = 'k00000')";

/// For each construct whose POSITION is the whole point, the statement a user
/// would really write. Every key must be a row of `refuse::TABLE`, which
/// `every_natural_position_names_a_row_the_table_still_holds` asserts, so a
/// row that is renamed or removed cannot leave a stale statement behind.
const NATURAL_POSITION: &[(&str, &str)] = &[
    // ── §2: a set operator stands AFTER a complete SELECT ─────────────────
    (
        "UNION",
        "SELECT _id FROM place WHERE kind = 'park' UNION SELECT _id FROM place WHERE kind = 'port'",
    ),
    (
        "INTERSECT",
        "SELECT _id FROM place WHERE kind = 'park' INTERSECT SELECT _id FROM place WHERE kind = 'port'",
    ),
    (
        "EXCEPT",
        "SELECT _id FROM place WHERE kind = 'park' EXCEPT SELECT _id FROM place WHERE kind = 'port'",
    ),
    (
        "OFFSET",
        "SELECT _id FROM place WHERE kind = 'park' LIMIT 5 OFFSET 5",
    ),
    (
        "JOIN",
        "SELECT _id FROM place JOIN place2 ON place._id = place2._id",
    ),
    ("WITH", "WITH t AS (SELECT _id FROM place) SELECT _id FROM t"),
    ("VALUES", "VALUES (1), (2)"),
    (
        "DECLARE",
        "DECLARE c BINARY CURSOR FOR SELECT _id FROM place WHERE kind = 'park'",
    ),
    ("FETCH", "FETCH FORWARD 10 FROM c"),
    ("CLOSE", "CLOSE c"),
    (
        "CREATE VIEW",
        "CREATE VIEW v AS SELECT _id FROM place WHERE kind = 'park'",
    ),
    ("CREATE SCHEMA", "CREATE SCHEMA warehouse"),
    // ── §4.3: a pattern MODE word stands between MATCH and the pattern ────
    (
        "TRAIL",
        "SELECT k FROM GRAPH_TABLE (routes MATCH TRAIL (a:place WHERE a._key = 'k00000')-[:near]->(b:place) COLUMNS (b._key AS k))",
    ),
    (
        "WALK",
        "SELECT k FROM GRAPH_TABLE (routes MATCH WALK (a:place WHERE a._key = 'k00000')-[:near]->(b:place) COLUMNS (b._key AS k))",
    ),
    (
        "SIMPLE",
        "SELECT k FROM GRAPH_TABLE (routes MATCH SIMPLE (a:place WHERE a._key = 'k00000')-[:near]->(b:place) COLUMNS (b._key AS k))",
    ),
    (
        "ANY SHORTEST",
        "SELECT k FROM GRAPH_TABLE (routes MATCH ANY SHORTEST (a:place WHERE a._key = 'k00000')-[:near]->(b:place) COLUMNS (b._key AS k))",
    ),
    (
        "ALL SHORTEST",
        "SELECT k FROM GRAPH_TABLE (routes MATCH ALL SHORTEST (a:place WHERE a._key = 'k00000')-[:near]->(b:place) COLUMNS (b._key AS k))",
    ),
    // ── §4.7: an aggregate or a window function stands in the select list ─
    ("OVER", "SELECT count(*) OVER () FROM place"),
    ("ARRAY_AGG", "SELECT array_agg(kind) FROM place"),
    ("STRING_AGG", "SELECT string_agg(kind, ',') FROM place"),
    ("JSON_AGG", "SELECT json_agg(kind) FROM place"),
    (
        "PERCENTILE_CONT",
        "SELECT percentile_cont(0.5) FROM place",
    ),
    ("ROW_NUMBER", "SELECT row_number() FROM place"),
    ("RANK", "SELECT rank() FROM place"),
    ("DENSE_RANK", "SELECT dense_rank() FROM place"),
    ("LAG", "SELECT lag(born) FROM place"),
    ("LEAD", "SELECT lead(born) FROM place"),
    ("NTILE", "SELECT ntile(4) FROM place"),
    (
        "GROUPING SETS",
        "SELECT kind, count(*) FROM place GROUP BY GROUPING SETS ((kind))",
    ),
    ("CUBE", "SELECT kind, count(*) FROM place GROUP BY CUBE (kind)"),
    // ── §4.1: a row function stands in the select list ───────────────────
    (
        "CASE",
        "SELECT CASE WHEN born > 1950 THEN 'late' ELSE 'early' END FROM place",
    ),
    ("COALESCE", "SELECT coalesce(tag, 'none') FROM place"),
    (
        "REGEXP_REPLACE",
        "SELECT regexp_replace(name, 'a', 'b') FROM place",
    ),
    ("REGEXP_MATCH", "SELECT regexp_match(name, 'a') FROM place"),
    (
        "JSON_ARRAY_LENGTH",
        "SELECT json_array_length(tag) FROM place",
    ),
    // ── §4.1: the JSON path operators stand between a column and a key ───
    ("->", "SELECT _id FROM place WHERE tag -> 'a' = 'b'"),
    ("->>", "SELECT _id FROM place WHERE tag ->> 'a' = 'b'"),
    ("#>", "SELECT _id FROM place WHERE tag #> 'a' = 'b'"),
    ("#>>", "SELECT _id FROM place WHERE tag #>> 'a' = 'b'"),
    // ── §4.4: the geometry functions stand in the select list ────────────
    ("ST_AREA", "SELECT ST_Area(plot) FROM place"),
    ("ST_LENGTH", "SELECT ST_Length(plot) FROM place"),
    ("ST_PERIMETER", "SELECT ST_Perimeter(plot) FROM place"),
    ("ST_CENTROID", "SELECT ST_Centroid(plot) FROM place"),
    ("ST_ASTEXT", "SELECT ST_AsText(plot) FROM place"),
    ("ST_ASBINARY", "SELECT ST_AsBinary(plot) FROM place"),
    ("ST_X", "SELECT ST_X(loc) FROM place"),
    ("ST_Y", "SELECT ST_Y(loc) FROM place"),
    ("ST_SIMPLIFY", "SELECT ST_Simplify(plot, 0.1) FROM place"),
    ("ST_BUFFER", "SELECT ST_Buffer(plot, 0.1) FROM place"),
    ("ST_UNION", "SELECT ST_Union(plot, plot) FROM place"),
    (
        "ST_INTERSECTION",
        "SELECT ST_Intersection(plot, plot) FROM place",
    ),
    (
        "ST_DIFFERENCE",
        "SELECT ST_Difference(plot, plot) FROM place",
    ),
    (
        "ST_SIMPLIFYPRESERVETOPOLOGY",
        "SELECT ST_SimplifyPreserveTopology(plot, 0.1) FROM place",
    ),
    ("ST_TRANSFORM", "SELECT ST_Transform(plot, 3857) FROM place"),
    ("ST_ASMVT", "SELECT ST_AsMVT(plot) FROM place"),
    (
        "ST_GEOMFROMTEXT",
        "SELECT ST_GeomFromText('POINT(1 2)') FROM place",
    ),
    ("ST_GEOMFROMWKB", "SELECT ST_GeomFromWKB(name) FROM place"),
    ("POSTGIS_VERSION", "SELECT postgis_version()"),
    // ── §4.5 / §4.6: row functions and query parsers ─────────────────────
    ("VECTOR_DIMS", "SELECT vector_dims(emb) FROM place"),
    ("VECTOR_NORM", "SELECT vector_norm(emb) FROM place"),
    ("L2_NORMALIZE", "SELECT l2_normalize(emb) FROM place"),
    ("TS_HEADLINE", "SELECT ts_headline(text, 'kebun') FROM place"),
    ("HIGHLIGHT", "SELECT highlight(text, 'kebun') FROM place"),
    (
        "WEBSEARCH_TO_TSQUERY",
        "SELECT _id FROM place WHERE to_tsvector('simple', text) @@ websearch_to_tsquery('kebun')",
    ),
    (
        "PLAINTO_TSQUERY",
        "SELECT _id FROM place WHERE to_tsvector('simple', text) @@ plainto_tsquery('kebun')",
    ),
    // ── §2: a pg_catalog relation this surface does not have ─────────────
    ("PG_PROC", "SELECT proname FROM pg_proc"),
    ("PG_SETTINGS", "SELECT name FROM pg_settings"),
    ("PG_ROLES", "SELECT rolname FROM pg_roles"),
    ("PG_AUTHID", "SELECT rolname FROM pg_authid"),
    ("PG_DATABASE", "SELECT datname FROM pg_database"),
    ("PG_ENUM", "SELECT enumlabel FROM pg_enum"),
    ("PG_OPERATOR", "SELECT oprname FROM pg_operator"),
    ("PG_AM", "SELECT amname FROM pg_am"),
    ("PG_TRIGGER", "SELECT tgname FROM pg_trigger"),
    ("PG_REWRITE", "SELECT rulename FROM pg_rewrite"),
    ("PG_STAT_ACTIVITY", "SELECT pid FROM pg_stat_activity"),
];

/// The generic position, for a row whose construct IS a bare predicate word
/// or a bare operator. The same embedding `sql_refusals.rs` uses, kept here
/// so this file does not depend on that file's private helper.
fn generic(keyword: &str) -> String {
    if let Some(rest) = keyword.strip_prefix("CREATE ") {
        return format!("CREATE {rest} whatever");
    }
    if keyword
        .chars()
        .next()
        .is_some_and(|c| !c.is_ascii_alphanumeric())
    {
        return format!("SELECT _id FROM place WHERE name {keyword} 'x'");
    }
    format!("SELECT _id FROM place WHERE {keyword}")
}

fn natural(keyword: &str) -> Option<&'static str> {
    NATURAL_POSITION
        .iter()
        .find(|(name, _)| *name == keyword)
        .map(|(_, statement)| *statement)
}

/// Every row of `refuse::TABLE`, written where a statement really puts it,
/// comes back as that row's own refusal -- its keyword, its tier, its reason
/// -- and never as a syntax error.
#[test]
fn every_row_of_the_refusal_table_is_refused_where_a_statement_writes_it() {
    let (_dir, mut f) = open();
    let mut positioned = 0usize;
    for (keyword, tier, reason) in refusals() {
        let statement = match natural(keyword) {
            Some(text) => {
                positioned += 1;
                text.to_owned()
            }
            None => generic(keyword),
        };
        let error = f
            .db
            .sql(&statement, &[])
            .expect_err(&format!("`{statement}` was not refused"));
        match &error {
            SqlError::Refused {
                keyword: got,
                tier: got_tier,
                reason: got_reason,
            } => {
                assert_eq!(
                    got.to_ascii_uppercase(),
                    *keyword,
                    "`{statement}` refused `{got}`, not `{keyword}`"
                );
                assert_eq!(
                    got_tier.number(),
                    tier.number(),
                    "`{statement}` refused at the wrong tier"
                );
                assert_eq!(got_reason, reason, "`{statement}` gave another reason");
            }
            SqlError::Syntax { message, at } => panic!(
                "`{statement}` is `{keyword}`, which the table lists as Tier {}, and it came back \
                 as a SYNTAX ERROR at byte {at}: {message}",
                tier.number()
            ),
            other => panic!("`{statement}` produced {other:?}, not a refusal"),
        }
    }
    println!(
        "{} row(s) in refuse::TABLE, {positioned} of them written where SQL puts them",
        refusals().len()
    );
}

/// The map above cannot rot: every statement in it claims a keyword, and that
/// keyword has to still be a row of the table.
#[test]
fn every_natural_position_names_a_row_the_table_still_holds() {
    for (keyword, statement) in NATURAL_POSITION {
        assert!(
            refusals().iter().any(|(name, _, _)| name == keyword),
            "`{statement}` claims `{keyword}`, which refuse::TABLE no longer holds"
        );
        assert!(
            statement.to_ascii_uppercase().contains(
                &keyword
                    .trim_start_matches("CREATE ")
                    .to_ascii_uppercase()
            ) || keyword.starts_with("PG_"),
            "`{statement}` does not actually write `{keyword}`"
        );
    }
}

/// The four constructs `QL_CONTRACT` §7 item 5 owes a PROJECTION-EXPRESSION
/// surface, in BOTH positions a statement can write them: a select list and a
/// predicate. Before this slice each was a syntax error naming a byte, which
/// is the one thing the contract's "refused by name" rule forbids.
#[test]
fn the_projection_expression_constructs_are_refused_by_name_in_both_positions() {
    let (_dir, mut f) = open();
    for (statement, keyword, tier) in [
        (
            "SELECT CASE WHEN born > 1950 THEN 'late' ELSE 'early' END FROM place",
            "CASE",
            2u8,
        ),
        (
            "SELECT _id FROM place WHERE CASE WHEN born > 1950 THEN 1 ELSE 0 END = 1",
            "CASE",
            2,
        ),
        ("SELECT json_array_length(tag) FROM place", "JSON_ARRAY_LENGTH", 2),
        (
            "SELECT _id FROM place WHERE json_array_length(tag) = 1",
            "JSON_ARRAY_LENGTH",
            2,
        ),
        ("SELECT ST_Area(plot) FROM place", "ST_AREA", 2),
        ("SELECT _id FROM place WHERE ST_Area(plot) > 1", "ST_AREA", 2),
        ("SELECT ST_Length(plot) FROM place", "ST_LENGTH", 2),
        ("SELECT ST_Perimeter(plot) FROM place", "ST_PERIMETER", 2),
        ("SELECT ST_Centroid(plot) FROM place", "ST_CENTROID", 2),
        ("SELECT _id FROM place WHERE tag -> 'a' = 'b'", "->", 2),
        ("SELECT _id FROM place WHERE tag ->> 'a' = 'b'", "->>", 2),
        ("SELECT _id FROM place WHERE tag #> 'a' = 'b'", "#>", 2),
        ("SELECT _id FROM place WHERE tag #>> 'a' = 'b'", "#>>", 2),
    ] {
        let error = f
            .db
            .sql(statement, &[])
            .expect_err(&format!("`{statement}` was not refused"));
        match &error {
            SqlError::Refused {
                keyword: got,
                tier: got_tier,
                reason,
            } => {
                assert_eq!(got.to_ascii_uppercase(), keyword, "`{statement}`");
                assert_eq!(got_tier.number(), tier, "`{statement}`");
                assert!(
                    reason.contains("QL_CONTRACT"),
                    "`{statement}`'s reason cites the contract: {reason}"
                );
            }
            other => panic!("`{statement}` produced {other:?}, not a refusal by name"),
        }
    }
}

/// The set operators and the pattern modes, said once more on their own,
/// because they are the three the owner asked for by name and the three the
/// contract wrote down as open gaps (§7 item 10).
#[test]
fn the_set_operators_and_the_pattern_modes_are_tier_three_not_a_syntax_error() {
    let (_dir, mut f) = open();
    for (statement, keyword) in [
        (
            "SELECT _id FROM place WHERE kind = 'park' INTERSECT SELECT _id FROM place WHERE kind = 'port'",
            "INTERSECT",
        ),
        (
            "SELECT _id FROM place WHERE kind = 'park' EXCEPT SELECT _id FROM place WHERE kind = 'port'",
            "EXCEPT",
        ),
        (
            "SELECT _id FROM place WHERE kind = 'park' LIMIT 3 INTERSECT SELECT _id FROM place WHERE kind = 'port'",
            "INTERSECT",
        ),
        (
            "SELECT kind, count(*) FROM place GROUP BY kind INTERSECT SELECT kind, count(*) FROM place GROUP BY kind",
            "INTERSECT",
        ),
        (
            &format!(
                "SELECT k FROM GRAPH_TABLE (routes MATCH TRAIL {SEED}-[:near]->(b:place) COLUMNS (b._key AS k))"
            ),
            "TRAIL",
        ),
        (
            &format!(
                "SELECT k FROM GRAPH_TABLE (routes MATCH WALK {SEED}-[:near]->(b:place) COLUMNS (b._key AS k))"
            ),
            "WALK",
        ),
        (
            &format!(
                "SELECT k FROM GRAPH_TABLE (routes MATCH SIMPLE {SEED}-[:near]->(b:place) COLUMNS (b._key AS k))"
            ),
            "SIMPLE",
        ),
    ] {
        let error = f
            .db
            .sql(statement, &[])
            .expect_err(&format!("`{statement}` was not refused"));
        match &error {
            SqlError::Refused {
                keyword: got, tier, ..
            } => {
                assert_eq!(got.to_ascii_uppercase(), keyword, "`{statement}`");
                assert!(
                    matches!(tier, Tier::Three),
                    "`{statement}` is Tier {} and the contract says 3",
                    tier.number()
                );
            }
            other => panic!("`{statement}` produced {other:?}, not the Tier-3 refusal"),
        }
    }
}

/// The other half of the fix, and the one that keeps it honest: text that is
/// simply WRONG is still a syntax error naming the place. A parser that
/// answered every unrecognised word with a tier refusal would pass every test
/// above and be useless, because a typo would read as a promise of a feature.
#[test]
fn a_genuinely_malformed_statement_is_still_a_syntax_error() {
    let (_dir, mut f) = open();
    for statement in [
        // A misspelt keyword.
        "SELCT _id FROM place WHERE kind = 'park'",
        "SELECT _id FRM place WHERE kind = 'park'",
        "SELECT _id FROM place WHRE kind = 'park'",
        // A word that is in neither Tier 1 nor the table.
        "FLUMMOX place SET kind = 'park'",
        "SELECT _id FROM place WHERE kind = 'park' BANANA 5",
        // Punctuation that does not close.
        "SELECT _id FROM place WHERE kind = 'park' AND (born > 1900",
        "SELECT _id FROM place WHERE kind = ",
        // A statement that really does continue after the first.
        "SELECT _id FROM place WHERE kind = 'park' SELECT _id FROM place",
        // A character the lexer does not know.
        "SELECT _id FROM place WHERE kind = 'park' $$ 3",
    ] {
        let error = f
            .db
            .sql(statement, &[])
            .expect_err(&format!("`{statement}` answered"));
        assert!(
            matches!(error, SqlError::Syntax { .. }),
            "`{statement}` is malformed text, and it produced {error:?} rather than a syntax error"
        );
        assert!(
            error.tier().is_none(),
            "`{statement}` is malformed text and carries a tier"
        );
    }
}
