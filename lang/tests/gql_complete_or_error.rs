//! Complete or error (M6-H of `docs/lang/GQL_PROFILE_DESIGN_M5_M7.md` §3.7):
//! a budgeted GQL answer either completes, or its stream ends in a NAMED
//! `BudgetExceeded` after a prefix of the answer -- never a shorter answer
//! that ends well.
//!
//! For each statement of a fixed set (every seed kind -- key, scalar index,
//! text, point, vector order, scan -- a hop, a path search, grouping,
//! `OPTIONAL MATCH`, the three M5 operators, and a conjunct moved by
//! lineage), and for each caller-settable resource the statement charges in
//! an unlimited run (read from `EXPLAIN`'s `work:` line), the statement runs
//! again, paged two rows at a time, with that resource's ceiling at 1, 2,
//! ... 16 and doubling after, until it completes. The GQL-only resources
//! (`binding_rows`, `sort_bytes`, ...) have fixed caps no caller sets
//! through this API; their refusals are pinned per operator in core
//! (`core/engine/tests/gql_*.rs`).

mod bali;

use bali::fixture;
use sekejap_core::collections::{Database, QueryBudget};
use sekejap_lang::{explain_sql, prepare_sql, SqlRow, SqlValue};
use tempfile::TempDir;

/// Each caller-settable resource: its name in `EXPLAIN`'s `work:` line, its
/// name in a refusal, and its ceiling in a budget.
const RESOURCES: &[(&str, &str, fn(&mut QueryBudget) -> &mut u64)] = &[
    ("candidates", "Candidates", |b| &mut b.candidates),
    ("primary_reads", "PrimaryReads", |b| &mut b.primary_reads),
    ("scalar_postings", "ScalarPostings", |b| &mut b.scalar_postings),
    ("graph_edges", "GraphEdges", |b| &mut b.graph_edges),
    ("graph_visited", "GraphVisited", |b| &mut b.graph_visited),
    ("spatial_postings", "SpatialPostings", |b| &mut b.spatial_postings),
    ("text_postings", "TextPostings", |b| &mut b.text_postings),
    ("text_tokens", "TextTokens", |b| &mut b.text_tokens),
    ("vector_locators", "VectorLocators", |b| &mut b.vector_locators),
    ("vector_sidecars", "VectorSidecars", |b| &mut b.vector_sidecars),
    ("vector_lanes", "VectorLanes", |b| &mut b.vector_lanes),
    ("key_postings", "KeyPostings", |b| &mut b.key_postings),
    ("groups", "Groups", |b| &mut b.groups),
    ("output_bytes", "OutputBytes", |b| &mut b.output_bytes),
];

fn statements() -> Vec<(&'static str, &'static str)> {
    vec![
        ("key seed and hop", "MATCH (t IS town WHERE t._key = 'denpasar')-[:near]->(b IS beach) RETURN b._key AS k ORDER BY k"),
        ("scalar index seed", "MATCH (b IS beach WHERE b.rating >= 4) RETURN b._key AS k"),
        (
            "text seed and bm25",
            "MATCH (b IS beach WHERE to_tsvector('simple', b.about) @@ to_tsquery('simple', 'reef')) \
             RETURN b._key AS k, bm25(b.about, 'reef') AS s ORDER BY s DESC, k",
        ),
        (
            "point seed and distance",
            "MATCH (b IS beach WHERE ST_DWithin(b.loc, ST_MakePoint(115.25, -8.72)::geography, 12000)) \
             RETURN b._key AS k, ST_Distance(b.loc, ST_MakePoint(115.25, -8.72)::geography) AS m ORDER BY m",
        ),
        (
            "vector ordered seed",
            "MATCH (b IS beach) RETURN b._key AS k, b.emb <-> '[0,1,0]'::vector AS d ORDER BY d, k LIMIT 3",
        ),
        ("scan with a FILTER", "MATCH (b IS beach) FILTER (b.rating >= 4) = TRUE RETURN b._key AS k ORDER BY k"),
        (
            "quantified path",
            "MATCH (a IS beach WHERE a._key = 'kuta')-[:route]->{1,3}(z IS beach) RETURN z._key AS k ORDER BY k",
        ),
        (
            "any shortest path",
            "MATCH ANY SHORTEST (a IS beach WHERE a._key = 'kuta')-[:route]->{1,4}(z IS beach WHERE z._key = 'amed') \
             RETURN z._key AS k",
        ),
        (
            "grouping",
            "MATCH (t IS town)-[:near]->(b IS beach) RETURN t._key AS t, COUNT(b) AS n ORDER BY t",
        ),
        (
            "optional match",
            "MATCH (b IS beach) OPTIONAL MATCH (b)-[:route]->(c IS beach) RETURN b._key AS k, c._key AS c ORDER BY k, c",
        ),
        (
            "exists",
            "MATCH (b IS beach) WHERE EXISTS { MATCH (b)-[:route]->(c IS beach) } RETURN b._key AS k ORDER BY k",
        ),
        (
            "call",
            "MATCH (t IS town) CALL (t) { MATCH (t)-[:near]->(b IS beach) RETURN b._key AS first ORDER BY first LIMIT 1 } \
             RETURN t._key AS t, first ORDER BY t",
        ),
        (
            "union",
            "MATCH (b IS beach WHERE b.rating >= 5) RETURN b._key AS k UNION MATCH (b IS beach WHERE b._key = 'amed') RETURN b._key AS k",
        ),
        (
            "moved by lineage",
            "MATCH (b IS beach) RETURN b AS x NEXT FILTER to_tsvector('simple', x.about) @@ to_tsquery('simple', 'sunset') \
             RETURN x._key AS k ORDER BY k",
        ),
    ]
}

fn sql(body: &str) -> String {
    format!("SELECT * FROM GRAPH_TABLE (base {body})")
}

/// The resources the unlimited run charged, from `EXPLAIN`'s `work:` line.
fn charged(db: &Database, sql: &str) -> Vec<(&'static str, &'static str, fn(&mut QueryBudget) -> &mut u64)> {
    let plan = explain_sql(db, sql, &[]).unwrap();
    let work = plan
        .lines()
        .find_map(|line| line.strip_prefix("work: "))
        .unwrap_or_else(|| panic!("no work line in:\n{plan}"));
    RESOURCES
        .iter()
        .copied()
        .filter(|(name, _, _)| {
            work.split(' ')
                .any(|pair| pair.strip_prefix(name).and_then(|rest| rest.strip_prefix('=')).is_some_and(|count| count != "0"))
        })
        .collect()
}

fn paged(db: &Database, sql: &str, budget: QueryBudget) -> (Vec<Vec<SqlValue>>, Result<(), String>) {
    let prepared = prepare_sql(db, sql, &[]).unwrap();
    let mut rows = Vec::new();
    let ended = prepared
        .for_each_row_with(db, 2, budget, &mut || false, &mut |row: &SqlRow| {
            rows.push(row.values.clone());
            Ok(())
        })
        .map_err(|error| error.to_string());
    (rows, ended)
}

/// The count a budget refusal says it reached.
fn attempted(error: &str) -> u64 {
    let at = error.find("attempted: ").expect("an attempted count") + "attempted: ".len();
    error[at..].chars().take_while(char::is_ascii_digit).collect::<String>().parse().unwrap()
}

#[test]
fn every_budgeted_answer_completes_or_ends_in_a_named_refusal() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let mut swept = 0;
    for (name, body) in statements() {
        let sql = sql(body);
        let (whole, ended) = paged(&db, &sql, QueryBudget::unlimited());
        ended.unwrap_or_else(|error| panic!("`{name}` unlimited: {error}"));
        let resources = charged(&db, &sql);
        assert!(!resources.is_empty(), "`{name}` charges nothing a caller can bound");
        for (resource, refused, ceiling) in resources {
            let mut limit = 1u64;
            loop {
                let mut budget = QueryBudget::unlimited();
                *ceiling(&mut budget) = limit;
                let (rows, ended) = paged(&db, &sql, budget);
                swept += 1;
                let error = match ended {
                    Ok(()) => {
                        assert_eq!(rows, whole, "`{name}` under {limit} {resource} ended well with another answer");
                        break;
                    }
                    Err(error) => error,
                };
                assert!(
                    error.contains("BudgetExceeded")
                        && error.contains(refused)
                        && error.contains(&format!("limit: {limit},"))
                        && attempted(&error) > limit,
                    "`{name}` under {limit} {resource}: the refusal names the resource, its limit and the count it reached: {error}"
                );
                assert_eq!(
                    rows,
                    whole[..rows.len().min(whole.len())],
                    "`{name}` under {limit} {resource}: the rows before the refusal are the answer's first rows"
                );
                limit = if limit < 16 { limit + 1 } else { limit * 2 };
                assert!(limit < 1 << 24, "`{name}` never completed under {resource}");
            }
        }
    }
    assert!(swept > 100, "the sweep ran {swept} budgeted executions");
}
