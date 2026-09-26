//! `OPTIONAL MATCH` inside a GQL stage, through the SQL entry points (M3-F
//! of `docs/lang/GQL_PROFILE_DESIGN.md`, owner answer Q3; brief §4.2, §7
//! rule 6, §9.2 and §9.6).
//!
//! What is at risk, and the test that pins it:
//!
//! * a LEFT OUTER JOIN per input row: every match, or ONE row with the
//!   pattern's new variables `NULL` when there is none; the input row is
//!   never dropped (`several_matches_zero_matches_*`);
//! * the optional `WHERE` decides whether a match EXISTS: a `WHERE` that
//!   rejects every match gives the `NULL` row, while the same condition as a
//!   later `FILTER` rejects the `NULL`-extended row -- two different answers
//!   that no optimisation may interchange ("Optional predicate placement",
//!   `optional_predicate_placement_*`);
//! * brief §9.6 as written: a topic without qualifying evidence is returned
//!   with a `NULL` evidence key (`optional_evidence_*`);
//! * brief §9.2 as written, `OPTIONAL MATCH` after `NEXT`: a collaborator
//!   with no wider network keeps its row, and `COUNT` over the `NULL` side
//!   is 0, not 1, as over a PostgreSQL LEFT JOIN
//!   (`two_stage_collaborators_*`, `count_over_the_null_side_*`);
//! * variables bound before the `OPTIONAL MATCH` stay bound and join as in a
//!   plain `MATCH`: a bound node is the same node, a bound far end is an
//!   `ExpandInto` (`bound_variables_join_*`);
//! * functions over a `NULL` element or path give `NULL`, `IS NULL` holds,
//!   and a later `MATCH` from a `NULL` node matches nothing
//!   (`null_elements_*`);
//! * an `OPTIONAL MATCH` of a path pattern, two in a row, and one that
//!   opens a stage (`optional_path_patterns_*`, `two_optional_matches_*`,
//!   `an_optional_match_opening_*`);
//! * one pattern per `OPTIONAL MATCH` (design Q3): comma patterns are
//!   refused by name (`comma_patterns_*`);
//! * `EXPLAIN` prints the operator, its inner steps and the nullable slots
//!   (`explain_shows_*`).
//!
//! Workload names are invented: band `b1`, people `p1`-`p6`, topics
//! `t1`-`t4`, evidence `e1`-`e4`.

use sekejap_core::collections::{Database, EntityId, GraphContextId};
use sekejap_core::Kind;
use sekejap_lang::{explain_sql, prepare_sql, Param, SqlError, SqlResult, SqlValue};
use serde_json::json;
use tempfile::TempDir;

mod common;
use common::cfg;

/// ```text
/// band     b1
/// person   p1 .. p6
/// topic    t1 .. t4 (title "topic one" ...)
/// evidence e1 quality 5, e2 quality 2, e3 quality 1, e4 (no quality)
///
/// collab        b1->p1 twice  b1->p2  p3->b1  b1->p6
///               p1->p2 twice  p1->p3  p2->p4  p3->p5  p4->p1
/// supported_by  t1->e1  t1->e2  t3->e3  t4->e4       (t2 has none)
/// enables       t1->t2  t2->t3
/// ```
fn fixture(dir: &TempDir) -> Database {
    let mut db = Database::create(dir.path().join("o.sekejap"), cfg()).unwrap();
    let text = |name: &str| (name.to_owned(), Kind::Text);
    let int = |name: &str| (name.to_owned(), Kind::Int);
    let band = db
        .create_collection("band", vec![text("name")], Default::default())
        .unwrap();
    let person = db
        .create_collection("person", vec![text("name")], Default::default())
        .unwrap();
    let topic = db
        .create_collection("topic", vec![text("title")], Default::default())
        .unwrap();
    let evidence = db
        .create_collection("evidence", vec![int("quality")], Default::default())
        .unwrap();
    let b1 = db.put(band, "b1", &json!({"name": "band one"})).unwrap();
    let p: Vec<EntityId> = (1..=6)
        .map(|i| {
            db.put(
                person,
                &format!("p{i}"),
                &json!({"name": format!("person {i}")}),
            )
            .unwrap()
        })
        .collect();
    let t: Vec<EntityId> = ["one", "two", "three", "four"]
        .iter()
        .enumerate()
        .map(|(i, word)| {
            db.put(
                topic,
                &format!("t{}", i + 1),
                &json!({"title": format!("topic {word}")}),
            )
            .unwrap()
        })
        .collect();
    let e: Vec<EntityId> = [Some(5), Some(2), Some(1), None]
        .iter()
        .enumerate()
        .map(|(i, quality)| {
            let row = match quality {
                Some(q) => json!({"quality": q}),
                None => json!({}),
            };
            db.put(evidence, &format!("e{}", i + 1), &row).unwrap()
        })
        .collect();
    db.enable_graph().unwrap();
    let collab = db.create_edge_type("collab").unwrap();
    let supported_by = db.create_edge_type("supported_by").unwrap();
    let enables = db.create_edge_type("enables").unwrap();
    let base = GraphContextId::BASE;
    let edges = [
        (b1, collab, p[0]),
        (b1, collab, p[0]),
        (b1, collab, p[1]),
        (p[2], collab, b1),
        (b1, collab, p[5]),
        (p[0], collab, p[1]),
        (p[0], collab, p[1]),
        (p[0], collab, p[2]),
        (p[1], collab, p[3]),
        (p[2], collab, p[4]),
        (p[3], collab, p[0]),
        (t[0], supported_by, e[0]),
        (t[0], supported_by, e[1]),
        (t[2], supported_by, e[2]),
        (t[3], supported_by, e[3]),
        (t[0], enables, t[1]),
        (t[1], enables, t[2]),
    ];
    for (source, edge_type, destination) in edges {
        db.create_edge(base, source, edge_type, destination, &json!({}))
            .unwrap();
    }
    db.commit().unwrap();
    db
}

fn statement(body: &str) -> String {
    format!("SELECT * FROM GRAPH_TABLE ({body})")
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

fn cell(value: &SqlValue) -> String {
    match value {
        SqlValue::Text(text) => text.clone(),
        SqlValue::Int(i) => i.to_string(),
        SqlValue::Float(f) => format!("{f:?}"),
        SqlValue::Bool(b) => b.to_string(),
        SqlValue::Null => "NULL".to_owned(),
        SqlValue::Json(value) => value.to_string(),
        other => format!("{other:?}"),
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

fn key(k: &str) -> Param {
    Param::Text(k.into())
}

// ── the outer join ─────────────────────────────────────────────────────────

#[test]
fn several_matches_zero_matches_and_the_input_row_is_never_dropped() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let answer = run(
        &db,
        "base MATCH (t IS topic) \
         OPTIONAL MATCH (t)-[s:supported_by]->(ev IS evidence) \
         RETURN t._key AS topic_key, ev._key AS evidence_key \
         ORDER BY topic_key, evidence_key",
        &[],
    );
    assert_eq!(answer.columns, ["topic_key", "evidence_key"]);
    // t1 has two matches: two rows. t2 has none: ONE row, NULL evidence.
    assert_eq!(
        lines(&answer),
        ["t1|e1", "t1|e2", "t2|NULL", "t3|e3", "t4|e4"]
    );
}

#[test]
fn a_where_that_rejects_every_match_gives_the_null_row() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let answer = run(
        &db,
        "base MATCH (t IS topic WHERE t._key = 't1') \
         OPTIONAL MATCH (t)-[:supported_by]->(ev IS evidence) WHERE ev.quality > 100 \
         RETURN t._key AS topic_key, ev._key AS evidence_key",
        &[],
    );
    assert_eq!(lines(&answer), ["t1|NULL"]);
    // An inline predicate is part of the optional match too.
    let answer = run(
        &db,
        "base MATCH (t IS topic WHERE t._key = 't1') \
         OPTIONAL MATCH (t)-[:supported_by]->(ev IS evidence WHERE ev.quality > 100) \
         RETURN t._key AS topic_key, ev._key AS evidence_key",
        &[],
    );
    assert_eq!(lines(&answer), ["t1|NULL"]);
}

/// "Optional predicate placement" (brief §7 rule 6): the optional `WHERE`
/// decides whether a binding exists; a later `FILTER` removes the
/// `NULL`-extended row. The two give different answers.
#[test]
fn optional_predicate_placement_where_and_a_later_filter_differ() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let inside = run(
        &db,
        "base MATCH (t IS topic) \
         OPTIONAL MATCH (t)-[:supported_by]->(ev IS evidence) WHERE ev.quality >= 3 \
         RETURN t._key AS topic_key, ev._key AS evidence_key ORDER BY topic_key",
        &[],
    );
    // e4 has no quality: `NULL >= 3` is unknown, so it does not match
    // either, and t4 keeps its row with a NULL.
    assert_eq!(lines(&inside), ["t1|e1", "t2|NULL", "t3|NULL", "t4|NULL"]);
    let after = run(
        &db,
        "base MATCH (t IS topic) \
         OPTIONAL MATCH (t)-[:supported_by]->(ev IS evidence) \
         FILTER ev.quality >= 3 \
         RETURN t._key AS topic_key, ev._key AS evidence_key ORDER BY topic_key",
        &[],
    );
    assert_eq!(lines(&after), ["t1|e1"]);
}

/// Brief §9.6, as written apart from the invented names.
fn evidence_body() -> &'static str {
    "base \
     MATCH (topic IS topic WHERE topic._key = $1) \
     OPTIONAL MATCH (topic)-[:supported_by]->(evidence IS evidence) \
       WHERE evidence.quality >= $2 \
     RETURN topic._key AS topic_key, topic.title AS title, \
            evidence._key AS evidence_key"
}

#[test]
fn optional_evidence_preserves_a_topic_without_evidence_brief_9_6() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let answer = run(&db, evidence_body(), &[key("t1"), Param::Int(3)]);
    assert_eq!(answer.columns, ["topic_key", "title", "evidence_key"]);
    assert_eq!(lines(&answer), ["t1|topic one|e1"]);
    // No evidence at all, and evidence below the bar: the topic, NULL.
    let answer = run(&db, evidence_body(), &[key("t2"), Param::Int(3)]);
    assert_eq!(lines(&answer), ["t2|topic two|NULL"]);
    let answer = run(&db, evidence_body(), &[key("t3"), Param::Int(3)]);
    assert_eq!(lines(&answer), ["t3|topic three|NULL"]);
    // Rebound, the same plan finds both pieces of t1's evidence.
    let mut both = lines(&run(&db, evidence_body(), &[key("t1"), Param::Int(1)]));
    both.sort();
    assert_eq!(both, ["t1|topic one|e1", "t1|topic one|e2"]);
    // The non-optional MATCH still drops a topic that does not exist.
    let answer = run(&db, evidence_body(), &[key("t9"), Param::Int(1)]);
    assert!(answer.rows.is_empty());
}

// ── after NEXT, and counting ───────────────────────────────────────────────

/// Brief §9.2 as written, apart from the invented names and the outer
/// `ORDER BY`, which is written in the last `RETURN` until the outer SELECT
/// over a GQL relation is built (M3-D).
fn collaborators() -> &'static str {
    "base \
     MATCH (band IS band WHERE band._key = $1) \
           -[c IS collab]-(person IS person) \
     RETURN person, COUNT(c) AS direct_collaborations \
     GROUP BY person \
     ORDER BY direct_collaborations DESC, person._key \
     LIMIT 10 \
     NEXT \
     OPTIONAL MATCH (person)-[:collab]-(other IS person) \
     RETURN person._key AS collaborator_key, \
            direct_collaborations, \
            COUNT(DISTINCT other) AS wider_network \
     GROUP BY person._key, direct_collaborations \
     ORDER BY direct_collaborations DESC, wider_network DESC, collaborator_key"
}

#[test]
fn two_stage_collaborators_keep_one_without_a_wider_network_brief_9_2() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let answer = run(&db, collaborators(), &[key("b1")]);
    assert_eq!(
        answer.columns,
        ["collaborator_key", "direct_collaborations", "wider_network"]
    );
    // p1 meets p2 (twice), p3 and p4: 3 distinct others. p6 meets nobody:
    // the plain MATCH of M3-B dropped it, the OPTIONAL MATCH keeps it at 0.
    assert_eq!(lines(&answer), ["p1|2|3", "p2|1|2", "p3|1|2", "p6|1|0"]);
    // A band nobody collaborated with: stage 1 has no group, stage 2 no
    // input row, so no row at all.
    let answer = run(&db, collaborators(), &[key("b9")]);
    assert!(answer.rows.is_empty());
}

#[test]
fn count_over_the_null_side_is_zero_not_one() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let answer = run(
        &db,
        "base MATCH (t IS topic) \
         OPTIONAL MATCH (t)-[s:supported_by]->(ev IS evidence) \
         RETURN t._key AS topic_key, COUNT(ev) AS evidence, COUNT(s) AS edges, \
                COUNT(*) AS rows \
         GROUP BY t._key ORDER BY topic_key",
        &[],
    );
    // PostgreSQL's LEFT JOIN: COUNT of the NULL side is 0, COUNT(*) counts
    // the preserved row.
    assert_eq!(
        lines(&answer),
        ["t1|2|2|2", "t2|0|0|1", "t3|1|1|1", "t4|1|1|1"]
    );
}

// ── bound variables ────────────────────────────────────────────────────────

#[test]
fn bound_variables_join_as_in_a_plain_match() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let body = "base MATCH (a IS person WHERE a._key = $1), (b IS person WHERE b._key = $2) \
                OPTIONAL MATCH (a)-[e:collab]-(b) \
                RETURN a._key AS a, b._key AS b, ELEMENT_ID(e) IS NOT NULL AS linked";
    // Two parallel edges between p1 and p2: two rows.
    let answer = run(&db, body, &[key("p1"), key("p2")]);
    assert_eq!(lines(&answer), ["p1|p2|true", "p1|p2|true"]);
    // No edge between p1 and p5: the pair is kept, the edge NULL. Both ends
    // were bound before, so both stay bound.
    let answer = run(&db, body, &[key("p1"), key("p5")]);
    assert_eq!(lines(&answer), ["p1|p5|false"]);
    // The far end is bound: the hop is an ExpandInto.
    let text = explain_sql(&db, &statement(body), &[key("p1"), key("p5")]).unwrap();
    contains(&text, "already bound (ExpandInto)");
}

#[test]
fn null_elements_give_null_from_functions_and_match_nothing_later() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let answer = run(
        &db,
        "base MATCH (t IS topic WHERE t._key = 't2') \
         OPTIONAL MATCH p = (t)-[s:supported_by]->(ev IS evidence) \
         RETURN ev IS NULL AS no_evidence, ev.quality AS quality, ELEMENT_ID(ev) AS id, \
                LABELS(ev) AS labels, SOURCE_NODE_ID(s) AS source, \
                PATH_LENGTH(p) AS hops, ARRAY_LENGTH(NODES(p)) AS nodes",
        &[],
    );
    assert_eq!(lines(&answer), ["true|NULL|NULL|NULL|NULL|NULL|NULL"]);
    // A later MATCH from the NULL node matches nothing, so the row goes.
    let answer = run(
        &db,
        "base MATCH (t IS topic) \
         OPTIONAL MATCH (t)-[:supported_by]->(ev IS evidence) \
         MATCH (ev)<-[:supported_by]-(u IS topic) \
         RETURN t._key AS k, u._key AS u ORDER BY k",
        &[],
    );
    assert_eq!(lines(&answer), ["t1|t1", "t1|t1", "t3|t3", "t4|t4"]);
    // The anti-join idiom: the topics with no evidence at all.
    let answer = run(
        &db,
        "base MATCH (t IS topic) \
         OPTIONAL MATCH (t)-[:supported_by]->(ev IS evidence) \
         FILTER ev IS NULL \
         RETURN t._key AS k",
        &[],
    );
    assert_eq!(lines(&answer), ["t2"]);
}

// ── shapes ─────────────────────────────────────────────────────────────────

#[test]
fn optional_path_patterns_search_per_input_row() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let answer = run(
        &db,
        "base MATCH (t IS topic) \
         OPTIONAL MATCH p = ANY SHORTEST (t)-[:enables]->{1,3}(u IS topic WHERE u._key = 't3') \
         RETURN t._key AS k, PATH_LENGTH(p) AS hops ORDER BY k",
        &[],
    );
    assert_eq!(lines(&answer), ["t1|2", "t2|1", "t3|NULL", "t4|NULL"]);
}

#[test]
fn two_optional_matches_in_a_row() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let answer = run(
        &db,
        "base MATCH (t IS topic) \
         OPTIONAL MATCH (t)-[:enables]->(next IS topic) \
         OPTIONAL MATCH (next)-[:supported_by]->(ev IS evidence) \
         RETURN t._key AS k, next._key AS next, ev._key AS ev ORDER BY k",
        &[],
    );
    // t1 enables t2, which has no evidence; t2 enables t3, which has e3;
    // t3 and t4 enable nothing, so the second OPTIONAL starts from NULL.
    assert_eq!(
        lines(&answer),
        ["t1|t2|NULL", "t2|t3|e3", "t3|NULL|NULL", "t4|NULL|NULL"]
    );
}

#[test]
fn an_optional_match_opening_a_stage_keeps_the_one_empty_row() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let answer = run(
        &db,
        "base OPTIONAL MATCH (t IS topic WHERE t._key = $1) RETURN t._key AS k",
        &[key("t9")],
    );
    assert_eq!(lines(&answer), ["NULL"]);
    let answer = run(
        &db,
        "base OPTIONAL MATCH (t IS topic WHERE t._key = $1) RETURN t._key AS k",
        &[key("t2")],
    );
    assert_eq!(lines(&answer), ["t2"]);
}

#[test]
fn comma_patterns_in_one_optional_match_are_refused_by_name() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let error = run_with(
        &db,
        "base MATCH (t IS topic) \
         OPTIONAL MATCH (t)-[:supported_by]->(ev IS evidence), (t)-[:enables]->(u IS topic) \
         RETURN t._key AS k",
        &[],
    )
    .err()
    .expect("refused");
    match error {
        SqlError::Refused {
            keyword, reason, ..
        } => {
            assert_eq!(keyword, "OPTIONAL MATCH with comma patterns");
            assert!(reason.contains("M5"), "{reason}");
        }
        other => panic!("not refused by name: {other}"),
    }
}

// ── EXPLAIN ────────────────────────────────────────────────────────────────

#[test]
fn explain_shows_the_optional_apply_its_inner_steps_and_nullable_slots() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let text = explain_sql(
        &db,
        &statement(evidence_body()),
        &[key("t1"), Param::Int(3)],
    )
    .unwrap();
    has_line(
        &text,
        "0 topic: node of topic, never null, bound at pattern 1 position 0",
    );
    has_line(
        &text,
        "1 evidence: node of evidence, nullable, bound at pattern 2 position 2",
    );
    contains(
        &text,
        "2. OptionalApply: per input row, every row steps 2.1-2.3 give from it, or that row once with evidence NULL when they give none",
    );
    has_line(
        &text,
        "2.1. Seed topic: the node already bound in topic -- charges binding_rows",
    );
    contains(&text, "2.2. Expand topic -[:supported_by]-> evidence");
    contains(
        &text,
        "2.3. Filter after the pattern: (evidence.quality >= $2)",
    );
    // The optional variable crosses NEXT as nullable.
    let text = explain_sql(&db, &statement(collaborators()), &[key("b1")]).unwrap();
    has_line(
        &text,
        "2 other: node of person, nullable, bound at pattern 1 position 2",
    );
}
