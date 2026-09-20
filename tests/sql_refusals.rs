//! Every construct `docs/QL_CONTRACT.md` places in Tier 2 or Tier 3 must be
//! REFUSED by name, with that tier and with a reason that names the atomic.
//!
//! This is the eighth law of `docs/FOUNDATION_TEST_STANDARD.md` under test: a
//! construct with no atomic is refused with a named reason, never emulated.
//! A parser that quietly ignored `OR`, or answered `LIKE 'a%'` with a scan it
//! did not admit to, would return rows -- and rows that look like an answer
//! are worse than an error.

#[path = "sqlslice/fixture.rs"]
mod fixture;

use e4_prototype::sql::{refusals, Param, SqlError, Tier};
use tempfile::TempDir;

fn open() -> (TempDir, fixture::Fixture) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let f = fixture::build(&path);
    (dir, f)
}

/// A statement that puts `keyword` where the parser must meet it.
fn statement_for(keyword: &str) -> String {
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

#[test]
fn the_table_itself_is_well_formed() {
    let table = refusals();
    assert!(table.len() >= 80, "the table holds {} rows", table.len());
    let mut seen: Vec<&str> = Vec::new();
    for (keyword, tier, reason) in table {
        assert_eq!(
            *keyword,
            keyword.to_ascii_uppercase(),
            "`{keyword}` is not written in upper case"
        );
        assert!(!reason.is_empty(), "`{keyword}` has no reason");
        assert!(
            reason.contains("QL_CONTRACT"),
            "`{keyword}`'s reason does not cite the contract: {reason}"
        );
        assert!(
            matches!(tier, Tier::Two | Tier::Three),
            "`{keyword}` has no tier"
        );
        assert!(
            !seen.contains(keyword),
            "`{keyword}` appears twice in the table"
        );
        seen.push(keyword);
    }
}

#[test]
fn every_listed_keyword_is_refused_by_name_with_its_tier_and_reason() {
    let (_dir, mut f) = open();
    for (keyword, tier, reason) in refusals() {
        let statement = statement_for(keyword);
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
                    "`{statement}` refused `{got}`"
                );
                assert_eq!(got_tier.number(), tier.number(), "`{statement}`");
                assert_eq!(got_reason, reason, "`{statement}`");
                assert!(!got_reason.is_empty());
                let shown = format!("{error}");
                assert!(shown.contains(&format!("Tier {}", tier.number())), "{shown}");
                assert!(shown.contains(reason), "{shown}");
            }
            other => panic!("`{statement}` produced {other:?}, not a refusal"),
        }
    }
}

/// The constructs the contract refuses that are a SHAPE rather than a
/// keyword: they have no entry in the table because there is no word to key
/// them on, and each still names its tier and its reason.
fn refused(f: &mut fixture::Fixture, statement: &str, params: &[Param]) -> (String, u8, String) {
    let error = f
        .db
        .sql(statement, params)
        .expect_err(&format!("`{statement}` was not refused"));
    match error {
        SqlError::Refused {
            keyword,
            tier,
            reason,
        } => {
            assert!(!reason.is_empty());
            (keyword, tier.number(), reason.to_owned())
        }
        other => panic!("`{statement}` produced {other:?}, not a refusal"),
    }
}

#[test]
fn two_order_by_keys_are_deviation_three() {
    let (_dir, mut f) = open();
    let (keyword, tier, reason) = refused(
        &mut f,
        "SELECT _id FROM place WHERE kind = 'park' ORDER BY born ASC, kind DESC",
        &[],
    );
    assert_eq!(keyword, "ORDER BY <two keys>");
    assert_eq!(tier, 3);
    assert!(reason.contains("deviation 3"), "{reason}");
    assert!(reason.contains("ONE key"), "{reason}");
}

#[test]
fn offset_is_deviation_four() {
    let (_dir, mut f) = open();
    let (keyword, tier, reason) =
        refused(&mut f, "SELECT _id FROM place LIMIT 10 OFFSET 10", &[]);
    assert_eq!(keyword, "OFFSET");
    assert_eq!(tier, 2);
    assert!(reason.contains("deviation 4"), "{reason}");
    assert!(reason.contains("keyset continuation"), "{reason}");
}

#[test]
fn an_inequality_is_the_complement_of_an_equality() {
    let (_dir, mut f) = open();
    for statement in [
        "SELECT _id FROM place WHERE kind <> 'park'",
        "SELECT _id FROM place WHERE kind != 'park'",
        "SELECT _id FROM place WHERE _key <> 'k00001'",
        "SELECT _id FROM place WHERE score IS NOT NULL",
    ] {
        let (_keyword, tier, _reason) = refused(&mut f, statement, &[]);
        assert_eq!(tier, 2, "{statement}");
    }
}

#[test]
fn a_distance_as_a_filter_is_tier_two_while_the_same_operator_orders() {
    let (_dir, mut f) = open();
    let (keyword, tier, reason) = refused(
        &mut f,
        "SELECT _id FROM place WHERE emb <=> $1::vector < 0.3",
        &[Param::Vector(fixture::query_vector())],
    );
    assert_eq!(keyword, "<=>");
    assert_eq!(tier, 2);
    assert!(reason.contains("membership set"), "{reason}");
    // The same operator in an ORDER BY is Tier 1 and answers.
    f.db.sql(
        "SELECT _id FROM place ORDER BY emb <=> $1::vector LIMIT 3",
        &[Param::Vector(fixture::query_vector())],
    )
    .unwrap();
}

#[test]
fn graph_constructs_beyond_the_slice_name_their_tier() {
    let (_dir, mut f) = open();
    for (statement, expect) in [
        (
            "SELECT k FROM GRAPH_TABLE (routes MATCH (a:place WHERE a.born = 1)-[:near]->(b:place) COLUMNS (b._key AS k))",
            "inline element WHERE",
        ),
        (
            "SELECT k FROM GRAPH_TABLE (routes MATCH (a:place WHERE a._key = 'k00000')-[e:near WHERE e.weight = 1]->(b:place) COLUMNS (b._key AS k))",
            "edge inline WHERE",
        ),
        (
            "SELECT k FROM GRAPH_TABLE (routes MATCH (a:place WHERE a._key = 'k00000')-[:near]->(b:place WHERE b.kind = 'park') COLUMNS (b._key AS k))",
            "inline element WHERE",
        ),
        (
            "SELECT k FROM GRAPH_TABLE (routes MATCH (a:place WHERE a._key = 'k00000')-[:near|far]->(b:place) COLUMNS (b._key AS k))",
            "label alternation",
        ),
        (
            "INSERT INTO GRAPH routes EDGE near (source, destination) VALUES ('a','b')",
            "INSERT INTO GRAPH",
        ),
        (
            "UPDATE GRAPH routes EDGE near SET weight = 1 WHERE source = 'a'",
            "UPDATE GRAPH",
        ),
        (
            "DELETE FROM GRAPH routes EDGE near WHERE source = 'a'",
            "DELETE FROM GRAPH",
        ),
    ] {
        let (keyword, tier, reason) = refused(&mut f, statement, &[]);
        assert_eq!(keyword, expect, "{statement}");
        assert_eq!(tier, 2, "{statement}");
        assert!(!reason.is_empty());
    }
}

#[test]
fn a_tsquery_that_mixes_and_with_or_is_a_boolean_tree() {
    let (_dir, mut f) = open();
    let (keyword, tier, _) = refused(
        &mut f,
        "SELECT _id FROM place WHERE to_tsvector('simple', text) @@ to_tsquery('simple', 'a & b | c')",
        &[],
    );
    assert_eq!(keyword, "OR");
    assert_eq!(tier, 2);
    let (keyword, tier, _) = refused(
        &mut f,
        "SELECT _id FROM place WHERE to_tsvector('simple', text) @@ to_tsquery('simple', '!a')",
        &[],
    );
    assert_eq!(keyword, "NOT");
    assert_eq!(tier, 2);
}

#[test]
fn a_text_configuration_beyond_simple_is_tier_three() {
    let (_dir, mut f) = open();
    let (keyword, tier, reason) = refused(
        &mut f,
        "SELECT _id FROM place WHERE to_tsvector('english', text) @@ to_tsquery('simple', 'a')",
        &[],
    );
    assert!(keyword.contains("english"), "{keyword}");
    assert_eq!(tier, 3);
    assert!(reason.contains("language-neutral"), "{reason}");
}

#[test]
fn halfvec_and_sparsevec_are_tier_three() {
    let (_dir, mut f) = open();
    for kind in ["HALFVEC", "SPARSEVEC"] {
        let (keyword, tier, _) = refused(
            &mut f,
            &format!("CREATE TABLE t2 (a TEXT PRIMARY KEY, b {kind}(4))"),
            &[],
        );
        assert_eq!(keyword, kind);
        assert_eq!(tier, 3);
    }
}

/// A Tier-1 construct the engine has no call for is NOT a tier refusal: it is
/// an `Unsupported` that names what is missing. `DROP TABLE` is the one this
/// slice found.
#[test]
fn a_tier_one_statement_with_no_atomic_names_what_is_missing() {
    let (_dir, mut f) = open();
    let error = f.db.sql("DROP TABLE place", &[]).unwrap_err();
    assert!(error.tier().is_none(), "{error}");
    let shown = format!("{error}");
    assert!(shown.contains("drop_collection"), "{shown}");
    assert!(shown.contains("no removal path"), "{shown}");
}

#[test]
fn a_predicate_on_an_unindexed_column_says_why_it_cannot_compile() {
    let (_dir, mut f) = open();
    let error = f
        .db
        .sql(
            "SELECT _id FROM place WHERE name = $1",
            &[Param::Text("kebun kopi".into())],
        )
        .unwrap_err();
    let shown = format!("{error}");
    assert!(shown.contains("scalar index"), "{shown}");
    assert!(shown.contains("index-side"), "{shown}");
}
