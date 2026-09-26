//! `EXISTS { ... }` and `NOT EXISTS { ... }` inside a GQL body, through the
//! SQL entry points (M5-C of `docs/lang/GQL_PROFILE_DESIGN_M5_M7.md` §2.3;
//! owner answers Q18, Q19; brief §4.2, §7 rule 7, §9.5).
//!
//! What is at risk, and the test that pins it:
//!
//! * an existence test never multiplies a row: a troupe with four
//!   performances is ONE row, and `NOT EXISTS` keeps exactly the rows with
//!   no match (`exists_never_multiplies_*`, `not_exists_keeps_*`);
//! * implicit correlation: a name the body shares with the outer scope IS
//!   the outer variable, and the body's pattern is seeded from it; a body
//!   that names no outer variable is re-run per input row
//!   (`correlation_*`, `an_uncorrelated_body_*`);
//! * the body's own variables are local: invisible after the `}`, never in
//!   `RETURN *`, free to be bound again later (`body_variables_*`);
//! * the filter form (a top-level conjunct) and the mark form (inside `OR`,
//!   `CASE`, `LET`, a `RETURN` item, an `ORDER BY` key) give the same
//!   answer where both apply (`mark_and_filter_forms_agree`);
//! * the short form (patterns and `WHERE`) is the full form's `MATCH`, and
//!   a `RETURN` inside is accepted and ignored, while an aggregate,
//!   `GROUP BY`, `ORDER BY`, `OFFSET` or `LIMIT` there is refused by name
//!   (Q18) (`the_short_form_*`, `a_return_inside_*`);
//! * placement: in a `MATCH`'s `WHERE`, an inline element `WHERE`, an
//!   `OPTIONAL MATCH` (where it decides whether a match exists), after a
//!   path search, and nested (`exists_in_*`, `an_inline_exists_*`,
//!   `nested_*`);
//! * refused by name: inside a quantifier, a quantified edge or a selective
//!   pattern's inline predicate (run per search state), in a grouped
//!   `RETURN` and in the outer `SELECT` (`exists_where_no_*`);
//! * the existence test stops at the first row: over 1,000 incoming edges
//!   it reads at most 2 (`an_existence_test_stops_at_the_first_edge`);
//! * brief §9.5's anti-join shape: backward cause chains, kept only where
//!   the cause has no cause of its own (`root_causes_*`).
//!
//! Workload names are invented, in the README's tourism world: troupes
//! `t1`-`t3`, dancers `d1`-`d4`, dances `k1`-`k3`, sites `s1`-`s3`,
//! incidents `i1`-`i5`.

use sekejap_core::collections::{Database, EntityId, GraphContextId};
use sekejap_core::Kind;
use sekejap_lang::{explain_sql, prepare_sql, Param, SqlError, SqlResult, SqlValue};
use serde_json::json;
use tempfile::TempDir;

mod common;
use common::cfg;

/// ```text
/// troupe   t1 .. t3            dancer d1 .. d4
/// dance    k1 "kecak", k2 "legong", k3 "fire dance"
/// site     s1 temple, s2 beach, s3 market
/// incident i1 .. i5
///
/// performs   t1->k1 three times  t1->k2  t2->k2  d1->k3   (t3 none)
/// member_of  d1->t1  d2->t1  d3->t2                       (d4 none)
/// hosts      s1->t1  s2->t2                               (s3 none)
/// causes     i1->i2  i2->i3  i4->i3                        (i5 alone)
/// ```
fn fixture(dir: &TempDir) -> Database {
    let mut db = Database::create(dir.path().join("x.sekejap"), cfg()).unwrap();
    let text = |name: &str| (name.to_owned(), Kind::Text);
    let troupe = db
        .create_collection("troupe", vec![text("name")], Default::default())
        .unwrap();
    let dancer = db
        .create_collection("dancer", vec![text("name")], Default::default())
        .unwrap();
    let dance = db
        .create_collection("dance", vec![text("title")], Default::default())
        .unwrap();
    let site = db
        .create_collection("site", vec![text("kind")], Default::default())
        .unwrap();
    let incident = db
        .create_collection("incident", vec![text("realm")], Default::default())
        .unwrap();
    let put = |db: &mut Database, collection, key: &str, row: serde_json::Value| -> EntityId {
        db.put(collection, key, &row).unwrap()
    };
    let t: Vec<EntityId> = (1..=3)
        .map(|i| {
            put(
                &mut db,
                troupe,
                &format!("t{i}"),
                json!({"name": format!("troupe {i}")}),
            )
        })
        .collect();
    let d: Vec<EntityId> = (1..=4)
        .map(|i| {
            put(
                &mut db,
                dancer,
                &format!("d{i}"),
                json!({"name": format!("dancer {i}")}),
            )
        })
        .collect();
    let k: Vec<EntityId> = ["kecak", "legong", "fire dance"]
        .iter()
        .enumerate()
        .map(|(i, title)| {
            put(
                &mut db,
                dance,
                &format!("k{}", i + 1),
                json!({"title": title}),
            )
        })
        .collect();
    let s: Vec<EntityId> = ["temple", "beach", "market"]
        .iter()
        .enumerate()
        .map(|(i, kind)| put(&mut db, site, &format!("s{}", i + 1), json!({"kind": kind})))
        .collect();
    let i: Vec<EntityId> = ["sea", "sea", "sea", "land", "land"]
        .iter()
        .enumerate()
        .map(|(n, realm)| {
            put(
                &mut db,
                incident,
                &format!("i{}", n + 1),
                json!({"realm": realm}),
            )
        })
        .collect();
    db.enable_graph().unwrap();
    let performs = db.create_edge_type("performs").unwrap();
    let member_of = db.create_edge_type("member_of").unwrap();
    let hosts = db.create_edge_type("hosts").unwrap();
    let causes = db.create_edge_type("causes").unwrap();
    let base = GraphContextId::BASE;
    let edges = [
        (t[0], performs, k[0]),
        (t[0], performs, k[0]),
        (t[0], performs, k[0]),
        (t[0], performs, k[1]),
        (t[1], performs, k[1]),
        (d[0], performs, k[2]),
        (d[0], member_of, t[0]),
        (d[1], member_of, t[0]),
        (d[2], member_of, t[1]),
        (s[0], hosts, t[0]),
        (s[1], hosts, t[1]),
        (i[0], causes, i[1]),
        (i[1], causes, i[2]),
        (i[3], causes, i[2]),
    ];
    for (source, edge_type, destination) in edges {
        db.create_edge(base, source, edge_type, destination, &json!({}))
            .unwrap();
    }
    db.commit().unwrap();
    db
}

fn statement(body: &str) -> String {
    format!("SELECT * FROM GRAPH_TABLE (base {body})")
}

struct Answer {
    columns: Vec<String>,
    rows: Vec<Vec<SqlValue>>,
}

fn run_with(db: &Database, body: &str, params: &[Param]) -> Result<Answer, SqlError> {
    let prepared = prepare_sql(db, &statement(body), params)?;
    let SqlResult::Rows { columns, rows } = prepared.run(db)? else {
        panic!("`{body}` did not answer with rows");
    };
    Ok(Answer {
        columns,
        rows: rows.into_iter().map(|row| row.values).collect(),
    })
}

fn run(db: &Database, body: &str, params: &[Param]) -> Answer {
    run_with(db, body, params).unwrap_or_else(|error| panic!("`{body}` failed: {error}"))
}

/// The rows, IN ORDER, each as `a|b|c`.
fn lines(answer: &Answer) -> Vec<String> {
    answer
        .rows
        .iter()
        .map(|row| row.iter().map(cell).collect::<Vec<_>>().join("|"))
        .collect()
}

/// The rows of `body`, in order.
fn rows(db: &Database, body: &str) -> Vec<String> {
    lines(&run(db, body, &[]))
}

fn cell(value: &SqlValue) -> String {
    match value {
        SqlValue::Text(text) => text.clone(),
        SqlValue::Int(i) => i.to_string(),
        SqlValue::Float(f) => format!("{f:?}"),
        SqlValue::Bool(b) => b.to_string(),
        SqlValue::Null => "NULL".to_owned(),
        other => format!("{other:?}"),
    }
}

/// The error `body` fails with.
fn error(db: &Database, body: &str) -> String {
    match run_with(db, body, &[]) {
        Ok(answer) => panic!("`{body}` answered {:?}", lines(&answer)),
        Err(error) => error.to_string(),
    }
}

fn contains(text: &str, part: &str) {
    assert!(text.contains(part), "no `{part}` in:\n{text}");
}

fn has_line(text: &str, line: &str) {
    assert!(
        text.lines().any(|l| l.trim() == line),
        "no line `{line}` in:\n{text}"
    );
}

/// A troupe that performs some dance.
const PERFORMS: &str = "EXISTS { MATCH (t)-[:performs]->(k IS dance) }";

// ── the existence test ─────────────────────────────────────────────────────

#[test]
fn exists_never_multiplies_a_row() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // The plain MATCH gives one row per performance: four for t1.
    assert_eq!(
        rows(
            &db,
            "MATCH (t IS troupe)-[:performs]->(k IS dance) RETURN t._key AS t ORDER BY t"
        ),
        ["t1", "t1", "t1", "t1", "t2"]
    );
    // The existence test gives each troupe once.
    let answer = run(
        &db,
        &format!("MATCH (t IS troupe) FILTER {PERFORMS} RETURN t._key AS t ORDER BY t"),
        &[],
    );
    assert_eq!(answer.columns, ["t"]);
    assert_eq!(lines(&answer), ["t1", "t2"]);
}

#[test]
fn not_exists_keeps_the_rows_with_no_match() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    assert_eq!(
        rows(
            &db,
            &format!("MATCH (t IS troupe) FILTER NOT {PERFORMS} RETURN t._key AS t ORDER BY t")
        ),
        ["t3"]
    );
    // A dancer who is a member of no troupe.
    assert_eq!(
        rows(
            &db,
            "MATCH (d IS dancer) FILTER NOT EXISTS { MATCH (d)-[:member_of]->(:troupe) } \
             RETURN d._key AS d"
        ),
        ["d4"]
    );
}

// ── scope ──────────────────────────────────────────────────────────────────

/// A name the body shares with the outer scope IS the outer variable: the
/// body's pattern is seeded from it, and a `$n` inside reads the statement's
/// parameters.
#[test]
fn correlation_seeds_the_body_from_the_outer_variable() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let body = "MATCH (t IS troupe) \
                FILTER EXISTS { MATCH (t)-[:performs]->(k IS dance WHERE k.title = $1) } \
                RETURN t._key AS t ORDER BY t";
    assert_eq!(
        lines(&run(&db, body, &[Param::Text("legong".into())])),
        ["t1", "t2"]
    );
    assert_eq!(
        lines(&run(&db, body, &[Param::Text("kecak".into())])),
        ["t1"]
    );
    let text = explain_sql(&db, &statement(body), &[Param::Text("kecak".into())]).unwrap();
    has_line(
        &text,
        "2.1. Seed t: the node already bound in t -- charges binding_rows",
    );
    contains(
        &text,
        "2.2. Expand t -[:performs]-> k: outgoing edges, k a new node of dance",
    );
}

/// A body that names no outer variable asks the same question of every
/// input row: it is re-run per input row, and says so.
#[test]
fn an_uncorrelated_body_is_re_run_per_input_row() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let body = |site: &str| {
        format!(
            "MATCH (t IS troupe) FILTER EXISTS {{ MATCH (s IS site WHERE s._key = '{site}') }} \
             RETURN t._key AS t ORDER BY t"
        )
    };
    assert_eq!(rows(&db, &body("s1")), ["t1", "t2", "t3"]);
    assert!(rows(&db, &body("s9")).is_empty());
    let text = explain_sql(&db, &statement(&body("s1")), &[]).unwrap();
    contains(
        &text,
        "2.1. Seed s: key lookup of 's1' in site, re-evaluated per input row",
    );
}

#[test]
fn body_variables_are_local_to_the_body() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // Invisible after the `}`.
    let message = error(
        &db,
        &format!("MATCH (t IS troupe) FILTER {PERFORMS} RETURN k._key AS k"),
    );
    contains(&message, "`k`");
    contains(&message, "42703");
    // Never in RETURN *: the next stage sees `t` and not `k`.
    let star = format!("MATCH (t IS troupe WHERE t._key = 't1') FILTER {PERFORMS} RETURN * NEXT");
    assert_eq!(rows(&db, &format!("{star} RETURN t._key AS t")), ["t1"]);
    let message = error(&db, &format!("{star} RETURN k._key AS k"));
    contains(&message, "`k`");
    contains(&message, "42703");
    // Free to be bound again, as a new variable, after the body.
    assert_eq!(
        rows(
            &db,
            &format!(
                "MATCH (t IS troupe WHERE t._key = 't1') FILTER {PERFORMS} \
                 MATCH (t)-[:performs]->(k IS dance) RETURN DISTINCT k._key AS k ORDER BY k"
            )
        ),
        ["k1", "k2"]
    );
    // An outer node named as an edge in the body is one variable used as
    // two kinds of element.
    let message = error(
        &db,
        "MATCH (t IS troupe) FILTER EXISTS { MATCH (x)-[t]->(y) } RETURN t._key AS t",
    );
    contains(&message, "`t`");
    // A LET inside the body may not re-bind an outer name.
    let message = error(
        &db,
        "MATCH (t IS troupe) FILTER EXISTS { MATCH (s IS site) LET t = 1 } RETURN t._key AS t",
    );
    contains(&message, "`t`");
    contains(&message, "42712");
}

// ── the two forms ──────────────────────────────────────────────────────────

/// Design §2.3 and Q19: a top-level conjunct is the filter form; inside `OR`,
/// `CASE`, a `LET`, a `RETURN` item or an `ORDER BY` key it is the mark form,
/// a hidden `BOOLEAN` slot. Where both apply, they agree.
#[test]
fn mark_and_filter_forms_agree() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let filtered = rows(
        &db,
        &format!("MATCH (t IS troupe) FILTER {PERFORMS} RETURN t._key AS t ORDER BY t"),
    );
    assert_eq!(filtered, ["t1", "t2"]);
    // Inside OR.
    assert_eq!(
        rows(
            &db,
            &format!(
                "MATCH (t IS troupe) FILTER {PERFORMS} OR t._key = 'none' RETURN t._key AS t ORDER BY t"
            )
        ),
        filtered
    );
    // In a MATCH's WHERE, both forms.
    assert_eq!(
        rows(
            &db,
            &format!("MATCH (t IS troupe) WHERE {PERFORMS} RETURN t._key AS t ORDER BY t")
        ),
        filtered
    );
    assert_eq!(
        rows(
            &db,
            &format!(
                "MATCH (t IS troupe) WHERE t._key = 'none' OR {PERFORMS} RETURN t._key AS t ORDER BY t"
            )
        ),
        filtered
    );
    let marked = ["t1|true", "t2|true", "t3|false"];
    // In a LET.
    assert_eq!(
        rows(
            &db,
            &format!("MATCH (t IS troupe) LET has = {PERFORMS} RETURN t._key AS t, has ORDER BY t")
        ),
        marked
    );
    // A RETURN item.
    assert_eq!(
        rows(
            &db,
            &format!("MATCH (t IS troupe) RETURN t._key AS t, {PERFORMS} AS has ORDER BY t")
        ),
        marked
    );
    // CASE.
    assert_eq!(
        rows(
            &db,
            &format!(
                "MATCH (t IS troupe) RETURN t._key AS t, \
                 CASE WHEN {PERFORMS} THEN 'yes' ELSE 'no' END AS has ORDER BY t"
            )
        ),
        ["t1|yes", "t2|yes", "t3|no"]
    );
    // NOT of the mark.
    assert_eq!(
        rows(
            &db,
            &format!("MATCH (t IS troupe) RETURN t._key AS t, NOT {PERFORMS} AS idle ORDER BY t")
        ),
        ["t1|false", "t2|false", "t3|true"]
    );
    // An ORDER BY key: the troupes that perform first.
    assert_eq!(
        rows(
            &db,
            &format!("MATCH (t IS troupe) RETURN t._key AS t ORDER BY {PERFORMS} DESC, t DESC")
        ),
        ["t2", "t1", "t3"]
    );
    // Aggregated per group, and as a grouping key.
    assert_eq!(
        rows(
            &db,
            &format!("MATCH (t IS troupe) RETURN COUNT(CASE WHEN {PERFORMS} THEN 1 END) AS n")
        ),
        ["2"]
    );
    assert_eq!(
        rows(
            &db,
            &format!("MATCH (t IS troupe) RETURN {PERFORMS} AS has, COUNT(*) AS n ORDER BY has")
        ),
        ["false|1", "true|2"]
    );
}

/// The short form is the full form's `MATCH`; a `RETURN` inside is accepted
/// and its items are ignored (design §2.3, Q18).
#[test]
fn the_short_form_is_the_full_forms_match() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let expected = ["t1"];
    for exists in [
        "EXISTS { (t)-[:performs]->(k IS dance) WHERE k.title = 'kecak' }",
        "EXISTS { MATCH (t)-[:performs]->(k IS dance) WHERE k.title = 'kecak' }",
        "EXISTS { MATCH (t)-[:performs]->(k IS dance) FILTER k.title = 'kecak' }",
        "EXISTS { MATCH (t)-[:performs]->(k IS dance) WHERE k.title = 'kecak' RETURN k }",
        "EXISTS { MATCH (t)-[:performs]->(k IS dance) LET n = k.title FILTER n = 'kecak' RETURN * }",
    ] {
        assert_eq!(
            rows(
                &db,
                &format!("MATCH (t IS troupe) FILTER {exists} RETURN t._key AS t ORDER BY t")
            ),
            expected,
            "{exists}"
        );
    }
}

/// Q18: whether a row exists is all the body answers; a clause that
/// changes that only in ways a FILTER says more plainly is refused by name.
#[test]
fn a_return_inside_exists_refuses_aggregates_grouping_ordering_and_paging() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    for (ret, clause) in [
        ("RETURN COUNT(k) AS n", "an aggregate"),
        ("RETURN k GROUP BY k", "GROUP BY"),
        ("RETURN k ORDER BY k.title", "ORDER BY"),
        ("RETURN k OFFSET 1", "OFFSET"),
        ("RETURN k LIMIT 1", "LIMIT"),
    ] {
        let message = error(
            &db,
            &format!(
                "MATCH (t IS troupe) FILTER EXISTS {{ MATCH (t)-[:performs]->(k) {ret} }} RETURN t._key AS t"
            ),
        );
        contains(&message, clause);
        contains(&message, "EXISTS");
    }
}

// ── placement ──────────────────────────────────────────────────────────────

/// An inline element `WHERE`, a `MATCH`'s `WHERE` and a `FILTER` after the
/// `MATCH` give one answer; the existence test runs where its variables are
/// bound.
#[test]
fn an_inline_exists_equals_the_where_and_the_filter() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let busy = "EXISTS { MATCH (t)-[:performs]->(:dance) }";
    let expected = ["d1|t1", "d2|t1", "d3|t2"];
    for body in [
        format!(
            "MATCH (d IS dancer)-[:member_of]->(t IS troupe WHERE {busy}) \
             RETURN d._key AS d, t._key AS t ORDER BY d"
        ),
        format!(
            "MATCH (d IS dancer)-[:member_of]->(t IS troupe) WHERE {busy} \
             RETURN d._key AS d, t._key AS t ORDER BY d"
        ),
        format!(
            "MATCH (d IS dancer)-[:member_of]->(t IS troupe) FILTER {busy} \
             RETURN d._key AS d, t._key AS t ORDER BY d"
        ),
    ] {
        assert_eq!(rows(&db, &body), expected, "{body}");
    }
    // A dancer whose troupe performs, AND who performs a dance of their own.
    assert_eq!(
        rows(
            &db,
            &format!(
                "MATCH (d IS dancer WHERE EXISTS {{ MATCH (d)-[:performs]->() }})\
                 -[:member_of]->(t IS troupe WHERE {busy}) RETURN d._key AS d"
            )
        ),
        ["d1"]
    );
}

/// A filter-form test in a `MATCH` runs right after the operator that binds
/// the last variable its body names, after the filters there: here, after
/// the seed `d` and its own predicate, before the hop to `t`.
#[test]
fn a_filter_form_test_runs_where_its_variables_are_bound() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let body = "MATCH (d IS dancer WHERE d._key = $1 AND d.name <> 'nobody')\
                -[:member_of]->(t IS troupe) \
                WHERE EXISTS { MATCH (d)-[:performs]->() } RETURN d._key AS d, t._key AS t";
    let key = |k: &str| [Param::Text(k.into())];
    assert_eq!(lines(&run(&db, body, &key("d1"))), ["d1|t1"]);
    assert!(lines(&run(&db, body, &key("d2"))).is_empty());
    let text = explain_sql(&db, &statement(body), &key("d1")).unwrap();
    contains(&text, "1. Seed d: key lookup of $1 in dancer");
    contains(&text, "2. Filter right after the seed: (d.name <> 'nobody')");
    contains(&text, "3. ExistsApply EXISTS: per input row, steps 3.1-3.2 run from it");
    contains(&text, "4. Expand d -[:member_of]-> t");
}

/// In an `OPTIONAL MATCH` the existence test is part of whether a match
/// exists: a site whose troupe does not qualify keeps its row, `NULL`.
#[test]
fn exists_in_an_optional_match_decides_whether_a_match_exists() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    assert_eq!(
        rows(
            &db,
            "MATCH (s IS site) \
             OPTIONAL MATCH (s)-[:hosts]->(t IS troupe) \
             WHERE EXISTS { MATCH (t)-[:performs]->(k IS dance WHERE k.title = 'kecak') } \
             RETURN s._key AS s, t._key AS t ORDER BY s"
        ),
        ["s1|t1", "s2|NULL", "s3|NULL"]
    );
}

/// After a path search the existence test reads the search's end; an
/// inline test on an end outside the quantifier of a pattern with no
/// selector is the same test after the search.
#[test]
fn exists_after_a_path_search_tests_its_end() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let last = "NOT EXISTS { MATCH (b)-[:causes]->() }";
    for body in [
        format!(
            "MATCH (a IS incident WHERE a._key = 'i1')-[:causes]->{{1,3}}(b IS incident) \
             FILTER {last} RETURN b._key AS b"
        ),
        format!(
            "MATCH (a IS incident WHERE a._key = 'i1')-[:causes]->{{1,3}}(b IS incident WHERE {last}) \
             RETURN b._key AS b"
        ),
    ] {
        assert_eq!(rows(&db, &body), ["i3"], "{body}");
    }
}

/// An `EXISTS` inside a body: a troupe with a member who performs.
#[test]
fn nested_exists_correlates_to_every_scope_around_it() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    assert_eq!(
        rows(
            &db,
            "MATCH (t IS troupe) \
             FILTER EXISTS { MATCH (t)<-[:member_of]-(d IS dancer) \
                             FILTER EXISTS { MATCH (d)-[:performs]->(:dance) } } \
             RETURN t._key AS t"
        ),
        ["t1"]
    );
}

/// Brief §9.5's anti-join: backward cause chains up to 8 hops, kept only
/// where the cause has no cause of its own. The test ranks nothing and
/// proves no causation: it pins the shape.
#[test]
fn root_causes_are_the_causes_with_no_cause_brief_9_5() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // i3 is caused by i2 (caused by i1) and by i4; i5 has no cause at all.
    let answer = run(
        &db,
        "FOR k IN $1 \
         MATCH (incident IS incident WHERE incident._key = k)<-[:causes]-{1,8}(cause IS incident) \
         FILTER NOT EXISTS { MATCH (cause)<-[:causes]-() } \
         RETURN DISTINCT incident._key AS incident, cause._key AS cause ORDER BY incident, cause",
        &[Param::Json(json!(["i3", "i5"]))],
    );
    assert_eq!(lines(&answer), ["i3|i1", "i3|i4"]);
}

// ── refused positions ──────────────────────────────────────────────────────

/// Inside a quantifier, a quantified edge or a selective pattern, an inline
/// predicate runs during the search, once per search state; in a grouped
/// `RETURN` a row is a group; the outer `SELECT` reads the relation's
/// columns only. Each is refused, naming the place to write it instead.
#[test]
fn exists_where_no_subquery_runs_per_row_is_refused_by_name() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let tail = "EXISTS { MATCH (y)-[:causes]->() }";
    for body in [
        format!(
            "MATCH (a IS incident WHERE a._key = 'i1')((x)-[:causes]->(y WHERE {tail})){{1,3}}(z) \
             RETURN z._key AS z"
        ),
        format!(
            "MATCH (a IS incident WHERE a._key = 'i1')-[e:causes WHERE {tail}]->{{1,3}}(y) \
             RETURN y._key AS y"
        ),
        format!(
            "MATCH ANY SHORTEST (a IS incident WHERE a._key = 'i1')-[:causes]->{{1,3}}(y WHERE {tail}) \
             RETURN y._key AS y"
        ),
    ] {
        let message = error(&db, &body);
        contains(&message, "during the path search");
        contains(&message, "FILTER");
    }
    let message = error(
        &db,
        &format!("MATCH (t IS troupe) RETURN {PERFORMS} AS has, COUNT(*) AS n GROUP BY t._key"),
    );
    contains(&message, "grouped RETURN");
    let message = match prepare_sql(
        &db,
        &format!(
            "SELECT t FROM GRAPH_TABLE (base MATCH (t IS troupe) RETURN t._key AS t) AS g \
             WHERE {PERFORMS}"
        ),
        &[],
    ) {
        Ok(_) => panic!("EXISTS in the outer SELECT was accepted"),
        Err(error) => error.to_string(),
    };
    contains(&message, "outer SELECT");
}

// ── the budget ─────────────────────────────────────────────────────────────

/// Design §2.3: an existence test over a node with 1,000 incoming edges
/// stops at the first; `graph_edges` is at most 2.
#[test]
fn an_existence_test_stops_at_the_first_edge() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("b.sekejap"), cfg()).unwrap();
    let site = db
        .create_collection(
            "site",
            vec![("kind".to_owned(), Kind::Text)],
            Default::default(),
        )
        .unwrap();
    let visitor = db
        .create_collection(
            "visitor",
            vec![("name".to_owned(), Kind::Text)],
            Default::default(),
        )
        .unwrap();
    let temple = db.put(site, "temple", &json!({"kind": "temple"})).unwrap();
    let visitors: Vec<EntityId> = (0..1000)
        .map(|n| {
            db.put(visitor, &format!("v{n:04}"), &json!({"name": "visitor"}))
                .unwrap()
        })
        .collect();
    db.enable_graph().unwrap();
    let visits = db.create_edge_type("visits").unwrap();
    for v in visitors {
        db.create_edge(GraphContextId::BASE, v, visits, temple, &json!({}))
            .unwrap();
    }
    db.commit().unwrap();
    let body = "MATCH (s IS site WHERE s._key = 'temple') \
                FILTER EXISTS { MATCH (s)<-[:visits]-() } RETURN s._key AS s";
    assert_eq!(rows(&db, body), ["temple"]);
    let text = explain_sql(&db, &statement(body), &[]).unwrap();
    let work = text
        .lines()
        .find(|line| line.starts_with("work: "))
        .unwrap_or_else(|| panic!("no work line:\n{text}"));
    let edges: u64 = work
        .split_whitespace()
        .find_map(|field| field.strip_prefix("graph_edges="))
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no graph_edges in {work}"));
    assert!(
        edges <= 2,
        "an existence test over 1,000 edges read {edges}: {work}"
    );
}
