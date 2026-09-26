//! The GQL stage grammar and working-table composition through the SQL
//! entry points (M3-B of `docs/lang/GQL_PROFILE_DESIGN.md`, §3.3, §2.3
//! rules 3 and 4, §7, and owner answers Q9, Q12, Q13).
//!
//! What is at risk, and the test that pins it:
//!
//! * the two-stage collaborator shape (brief §9.2, with a plain `MATCH` in
//!   stage 2): a node crosses `NEXT` as a node and seeds stage 2 as the node
//!   already bound, never re-found by name; stage 1's `LIMIT` bounds what
//!   stage 2 sees (`two_stages_*`);
//! * `FOR` over a list parameter feeds key seeds, keeping multiplicity
//!   (brief §9.4, `for_over_a_list_parameter_*`);
//! * a variable `RETURN` did not carry is out of scope after `NEXT`, and the
//!   error names the stage that dropped it (`next_dropping_*`);
//! * an aggregate in a later stage folds the WHOLE incoming table
//!   (`a_next_aggregate_*`), and over no rows it is one row of `COUNT` 0 and
//!   `NULL`s ungrouped and no row grouped (`an_empty_input_*`);
//! * grouping: explicit `GROUP BY`, implicit grouping by the non-aggregated
//!   items, and an item that is neither refused (`grouping_*`);
//! * aggregate column types follow PostgreSQL: `COUNT` BIGINT, `SUM` and
//!   `MIN`/`MAX` by argument, `AVG` DOUBLE PRECISION, `ARRAY_AGG` `T[]`
//!   (`aggregate_columns_*`);
//! * multi-key `ORDER BY` with `NULL`s last ascending and first descending,
//!   by an output column or by an expression `RETURN` does not project
//!   (`order_by_*`); a node or a list is refused as a sort key (Q12);
//! * `OFFSET` and `LIMIT` as literals and as `$n`, their edges and their
//!   parameter range (Q13) (`offset_and_limit_*`);
//! * `RETURN DISTINCT` (`return_distinct_*`);
//! * `LET` scoping: a `LET` does not see an alias of its own comma list, a
//!   later `LET` does; `FILTER` keeps only true rows (`let_*`);
//! * a stage of `RETURN` alone (`a_bare_return_*`), and `RETURN *`
//!   (`return_star_*`);
//! * `EXPLAIN` prints every stage with its schema and operators
//!   (`explain_shows_*`).
//!
//! Workload names are invented: bands `b1` `b2`, people `p1`-`p6`, topics
//! `t1`-`t4`, jobs `j1`-`j3`.

use sekejap_core::collections::{Database, EntityId, GraphContextId};
use sekejap_core::Kind;
use sekejap_lang::{explain_sql, prepare_sql, Param, SqlError, SqlResult, SqlValue};
use serde_json::json;
use tempfile::TempDir;

mod common;
use common::cfg;

/// ```text
/// band   b1, b2
/// person p1 age 41, p2 age 25, p3 (no age), p4 age 33, p5 age 25, p6 (no age)
/// topic  t1..t4            job j1..j3
///
/// collab      b1->p1 twice (two events)  b1->p2  p3->b1  b1->p6  b2->p4
///             p1->p2 twice  p1->p3  p2->p4  p3->p5  p4->p1
/// enables     t1->t2  t2->t3
/// useful_for  t1->j1  t2->j2  t2->j1  t3->j1  t4->j3
/// ```
fn fixture(dir: &TempDir) -> Database {
    let mut db = Database::create(dir.path().join("g.sekejap"), cfg()).unwrap();
    let text = |name: &str| (name.to_owned(), Kind::Text);
    let int = |name: &str| (name.to_owned(), Kind::Int);
    let band = db
        .create_collection("band", vec![text("name")], Default::default())
        .unwrap();
    let person = db
        .create_collection("person", vec![text("name"), int("age")], Default::default())
        .unwrap();
    let topic = db
        .create_collection("topic", vec![text("name")], Default::default())
        .unwrap();
    let job = db
        .create_collection("job", vec![text("title")], Default::default())
        .unwrap();
    let b1 = db.put(band, "b1", &json!({"name": "band one"})).unwrap();
    let b2 = db.put(band, "b2", &json!({"name": "band two"})).unwrap();
    let mut p = Vec::new();
    for (key, age) in [
        ("p1", Some(41)),
        ("p2", Some(25)),
        ("p3", None),
        ("p4", Some(33)),
        ("p5", Some(25)),
        ("p6", None),
    ] {
        let row = match age {
            Some(age) => json!({"name": format!("person {key}"), "age": age}),
            None => json!({"name": format!("person {key}")}),
        };
        p.push(db.put(person, key, &row).unwrap());
    }
    let t: Vec<EntityId> = (1..=4)
        .map(|i| {
            db.put(topic, &format!("t{i}"), &json!({"name": format!("topic {i}")}))
                .unwrap()
        })
        .collect();
    let j: Vec<EntityId> = (1..=3)
        .map(|i| {
            db.put(job, &format!("j{i}"), &json!({"title": format!("job {i}")}))
                .unwrap()
        })
        .collect();
    db.enable_graph().unwrap();
    let collab = db.create_edge_type("collab").unwrap();
    let enables = db.create_edge_type("enables").unwrap();
    let useful_for = db.create_edge_type("useful_for").unwrap();
    let base = GraphContextId::BASE;
    let edges = [
        (b1, collab, p[0]),
        (b1, collab, p[0]),
        (b1, collab, p[1]),
        (p[2], collab, b1),
        (b1, collab, p[5]),
        (b2, collab, p[3]),
        (p[0], collab, p[1]),
        (p[0], collab, p[1]),
        (p[0], collab, p[2]),
        (p[1], collab, p[3]),
        (p[2], collab, p[4]),
        (p[3], collab, p[0]),
        (t[0], enables, t[1]),
        (t[1], enables, t[2]),
        (t[0], useful_for, j[0]),
        (t[1], useful_for, j[1]),
        (t[1], useful_for, j[0]),
        (t[2], useful_for, j[0]),
        (t[3], useful_for, j[2]),
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
        rows: rows
            .into_iter()
            .map(|row| {
                assert_eq!(row.owner(), None, "a GQL row has no single owner");
                row.values
            })
            .collect(),
    })
}

fn run(db: &Database, body: &str, params: &[Param]) -> Answer {
    run_with(db, body, params).unwrap_or_else(|error| panic!("`{body}` failed: {error}"))
}

fn error(db: &Database, body: &str, params: &[Param]) -> String {
    match run_with(db, body, params) {
        Ok(answer) => panic!("`{body}` answered {} row(s) instead of failing", answer.rows.len()),
        Err(error) => error.to_string(),
    }
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

fn keys(list: &[&str]) -> Param {
    Param::Json(json!(list))
}

// ── two stages ─────────────────────────────────────────────────────────────

/// Brief §9.2, with a plain `MATCH` in stage 2 (`OPTIONAL MATCH` is M3-F):
/// rank a band's direct collaborators, then measure each one's wider
/// network.
fn collaborators(limit: &str) -> String {
    format!(
        "base MATCH (band IS band WHERE band._key = $1)-[c IS collab]-(person IS person) \
         RETURN person, COUNT(c) AS direct_collaborations \
         GROUP BY person \
         ORDER BY direct_collaborations DESC, person._key \
         LIMIT {limit} \
         NEXT \
         MATCH (person)-[:collab]-(other IS person) \
         RETURN person._key AS collaborator_key, direct_collaborations, \
                COUNT(DISTINCT other) AS wider_network \
         GROUP BY person._key, direct_collaborations \
         ORDER BY direct_collaborations DESC, wider_network DESC, collaborator_key"
    )
}

#[test]
fn two_stages_carry_nodes_across_next_and_rank_collaborators() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let answer = run(&db, &collaborators("10"), &[Param::Text("b1".into())]);
    assert_eq!(
        answer.columns,
        ["collaborator_key", "direct_collaborations", "wider_network"]
    );
    // p1 has two collaboration events with b1 (two parallel edges), p3's
    // edge is stored towards b1 and still matches the either-way hop. In
    // stage 2 p1 meets p2 over two parallel edges: one DISTINCT other. p6
    // has no person to meet, so the plain MATCH drops it.
    assert_eq!(lines(&answer), ["p1|2|3", "p2|1|2", "p3|1|2"]);

    // Stage 1's LIMIT bounds what stage 2 receives: the first two ranked.
    let answer = run(&db, &collaborators("2"), &[Param::Text("b1".into())]);
    assert_eq!(lines(&answer), ["p1|2|3", "p2|1|2"]);

    // Stage 2 starts from the node stage 1 carried: a bound seed, never a
    // lookup by name.
    let text = explain_sql(&db, &statement(&collaborators("10")), &[Param::Text("b1".into())])
        .unwrap();
    contains(&text, "Seed person: the node already bound in person");

    // A band with no collaborator: stage 1 has no group, stage 2 no row.
    let answer = run(&db, &collaborators("10"), &[Param::Text("b9".into())]);
    assert!(answer.rows.is_empty());
}

#[test]
fn next_dropping_a_variable_is_a_bind_error_naming_the_stage() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let dropped = "base MATCH (b IS band WHERE b._key = 'b1')-[:collab]-(p IS person) \
                   RETURN p NEXT RETURN b._key AS band";
    let message = error(&db, dropped, &[]);
    contains(&message, "`b`");
    contains(&message, "stage 1");
    let message = error(
        &db,
        "base MATCH (b IS band WHERE b._key = 'b1')-[:collab]-(p IS person) \
         RETURN p NEXT FILTER b.name = 'band one' RETURN p._key AS k",
        &[],
    );
    contains(&message, "`b`");
    // What RETURN carried is intact: the same nodes, as nodes.
    let answer = run(
        &db,
        "base MATCH (b IS band WHERE b._key = 'b1')-[:collab]-(p IS person) \
         RETURN DISTINCT p NEXT RETURN p._key AS k, p.age AS age ORDER BY k",
        &[],
    );
    assert_eq!(lines(&answer), ["p1|41", "p2|25", "p3|NULL", "p6|NULL"]);
    // A stage's columns must be nameable by the next stage.
    let message = error(
        &db,
        "base MATCH (p IS person) RETURN p.age + 1 NEXT RETURN 1 AS one",
        &[],
    );
    contains(&message, "AS");
    let message = error(
        &db,
        "base MATCH (p IS person)-[:collab]->(q IS person) RETURN p.name, q.name NEXT RETURN 1 AS one",
        &[],
    );
    contains(&message, "`name`");
}

#[test]
fn a_next_aggregate_folds_the_whole_incoming_table() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // Stage 1 hands on five rows (p1 twice: two events); stage 2 folds all
    // of them into ONE row, not one per input row.
    let answer = run(
        &db,
        "base MATCH (b IS band WHERE b._key = 'b1')-[c:collab]-(p IS person) RETURN p, c \
         NEXT RETURN COUNT(*) AS rows, COUNT(DISTINCT p) AS people, COUNT(c) AS events",
        &[],
    );
    assert_eq!(lines(&answer), ["5|4|5"]);
    // Grouped, it folds per group across the whole table.
    let answer = run(
        &db,
        "base MATCH (b IS band WHERE b._key = 'b1')-[c:collab]-(p IS person) RETURN p, c \
         NEXT RETURN p._key AS k, COUNT(*) AS events ORDER BY k",
        &[],
    );
    assert_eq!(lines(&answer), ["p1|2", "p2|1", "p3|1", "p6|1"]);
}

#[test]
fn an_empty_input_aggregate_is_one_row_ungrouped_and_no_row_grouped() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let missing = "base MATCH (b IS band WHERE b._key = 'b9')-[:collab]-(p IS person) ";
    let answer = run(
        &db,
        &format!(
            "{missing} RETURN COUNT(*) AS n, COUNT(p) AS np, SUM(p.age) AS s, AVG(p.age) AS a, \
             MIN(p.age) AS lo, ARRAY_AGG(p._key) AS ks"
        ),
        &[],
    );
    assert_eq!(lines(&answer), ["0|0|NULL|NULL|NULL|NULL"]);
    let answer = run(&db, &format!("{missing} RETURN b._key AS k, COUNT(*) AS n"), &[]);
    assert!(answer.rows.is_empty(), "implicit grouping, empty input: no group");
    let answer = run(
        &db,
        &format!("{missing} RETURN COUNT(*) AS n GROUP BY b._key"),
        &[],
    );
    assert!(answer.rows.is_empty(), "explicit grouping, empty input: no group");
}

// ── FOR ────────────────────────────────────────────────────────────────────

/// Brief §9.4 on one hop (quantifiers are M4): batch the starting topics
/// through a list parameter.
const COVERAGE: &str = "base FOR seed_key IN $1 \
     MATCH (topic IS topic WHERE topic._key = seed_key)-[:enables]->(downstream IS topic) \
           -[:useful_for]->(job IS job) \
     RETURN topic._key AS topic_key, COUNT(DISTINCT job) AS relevant_jobs, \
            COUNT(DISTINCT downstream) AS downstream_topics, COUNT(*) AS matches \
     GROUP BY topic._key \
     ORDER BY relevant_jobs DESC, downstream_topics DESC, topic_key";

#[test]
fn for_over_a_list_parameter_seeds_each_key() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // t1 -> t2 -> {j2, j1}; t2 -> t3 -> {j1}; t9 is no topic: no row.
    let answer = run(&db, COVERAGE, &[keys(&["t1", "t2", "t9"])]);
    assert_eq!(lines(&answer), ["t1|2|1|2", "t2|1|1|1"]);
    // FOR keeps multiplicity: a key given twice is two seeds.
    let answer = run(&db, COVERAGE, &[keys(&["t1", "t1"])]);
    assert_eq!(lines(&answer), ["t1|2|1|4"]);
    // An empty list and a NULL list give no row.
    assert!(run(&db, COVERAGE, &[keys(&[])]).rows.is_empty());
    assert!(run(&db, COVERAGE, &[Param::Null]).rows.is_empty());
    // Each key is a key lookup, per element.
    let text = explain_sql(&db, &statement(COVERAGE), &[keys(&["t1"])]).unwrap();
    contains(&text, "Unnest seed_key");
    contains(&text, "Seed topic: key lookup of seed_key in topic");
    // A vector is not a list, and neither is a scalar.
    let message = error(&db, COVERAGE, &[Param::Vector(vec![1.0, 2.0])]);
    contains(&message, "$1");
    let message = error(&db, COVERAGE, &[Param::Text("t1".into())]);
    contains(&message, "$1");
    // FOR over a list a stage built, and a stage of FOR alone.
    let answer = run(
        &db,
        "base MATCH (p IS person) RETURN ARRAY_AGG(p.age) AS ages \
         NEXT FOR age IN ages RETURN age, COUNT(*) AS n GROUP BY age ORDER BY age",
        &[],
    );
    assert_eq!(lines(&answer), ["25|2", "33|1", "41|1", "NULL|2"]);
    let answer = run(&db, "base FOR k IN $1 RETURN k", &[keys(&["x", "y"])]);
    assert_eq!(lines(&answer), ["x", "y"]);
}

// ── grouping and aggregates ────────────────────────────────────────────────

#[test]
fn grouping_is_explicit_or_by_the_non_aggregated_items() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // Implicit: the non-aggregated item is the key.
    let answer = run(
        &db,
        "base MATCH (p IS person) RETURN p.age AS age, COUNT(*) AS n ORDER BY age",
        &[],
    );
    assert_eq!(lines(&answer), ["25|2", "33|1", "41|1", "NULL|2"]);
    // Explicit, with an expression over a key and an aggregate.
    let answer = run(
        &db,
        "base MATCH (p IS person) RETURN p.age AS age, COUNT(*) * 10 AS n GROUP BY p.age \
         ORDER BY n DESC, age",
        &[],
    );
    assert_eq!(lines(&answer), ["25|20", "NULL|20", "33|10", "41|10"]);
    // An item that is neither a key nor inside an aggregate is refused.
    let message = error(
        &db,
        "base MATCH (p IS person) RETURN p.name AS name, COUNT(*) AS n GROUP BY p.age",
        &[],
    );
    contains(&message, "`p`");
    contains(&message, "GROUP BY");
    // An aggregate outside RETURN is refused.
    let message = error(
        &db,
        "base MATCH (p IS person) FILTER COUNT(*) > 1 RETURN p._key AS k",
        &[],
    );
    contains(&message, "COUNT");
    // So is an aggregate inside an aggregate.
    let message = error(&db, "base MATCH (p IS person) RETURN MAX(COUNT(*)) AS m", &[]);
    contains(&message, "COUNT");
}

#[test]
fn aggregate_columns_are_typed_as_postgresql_types_them() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let body = "base MATCH (p IS person) \
         RETURN COUNT(*) AS n, SUM(p.age) AS s, AVG(p.age) AS a, MIN(p.name) AS lo, \
                MAX(p.age) AS hi, ARRAY_AGG(p._key) AS ks, ARRAY_AGG(p.age) AS ages, \
                COUNT(DISTINCT p.age) AS ds";
    let prepared = prepare_sql(&db, &statement(body), &[]).unwrap();
    let types: Vec<_> = (0..8).map(|at| prepared.column_type(at).unwrap()).collect();
    assert_eq!(
        types,
        [
            "BIGINT",
            "BIGINT",
            "DOUBLE PRECISION",
            "TEXT",
            "BIGINT",
            "TEXT[]",
            "BIGINT[]",
            "BIGINT"
        ]
    );
    let answer = run(&db, body, &[]);
    // A scan reads the collection in id order, which is insertion order.
    assert_eq!(
        lines(&answer),
        [r#"6|124|31.0|person p1|41|["p1","p2","p3","p4","p5","p6"]|[41,25,null,33,25,null]|3"#]
    );
}

// ── ORDER BY, OFFSET, LIMIT, DISTINCT ──────────────────────────────────────

#[test]
fn order_by_several_keys_puts_nulls_last_ascending_and_first_descending() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let answer = run(
        &db,
        "base MATCH (p IS person) RETURN p._key AS k, p.age AS age ORDER BY age DESC, k",
        &[],
    );
    assert_eq!(
        lines(&answer),
        ["p3|NULL", "p6|NULL", "p1|41", "p4|33", "p2|25", "p5|25"]
    );
    let answer = run(
        &db,
        "base MATCH (p IS person) RETURN p._key AS k, p.age AS age ORDER BY age ASC, k DESC",
        &[],
    );
    assert_eq!(
        lines(&answer),
        ["p5|25", "p2|25", "p4|33", "p1|41", "p6|NULL", "p3|NULL"]
    );
    // By an expression RETURN does not project: the answer keeps its
    // columns.
    let answer = run(
        &db,
        "base MATCH (p IS person) RETURN p._key AS k ORDER BY p.age DESC, p._key",
        &[],
    );
    assert_eq!(answer.columns, ["k"]);
    assert_eq!(lines(&answer), ["p3", "p6", "p1", "p4", "p2", "p5"]);
    // By an aggregate RETURN does not project.
    let answer = run(
        &db,
        "base MATCH (p IS person)-[:collab]-(q IS person) RETURN p._key AS k \
         ORDER BY COUNT(DISTINCT q) DESC, k LIMIT 2",
        &[],
    );
    assert_eq!(lines(&answer), ["p1", "p2"]);
}

#[test]
fn order_by_a_node_or_a_list_is_refused() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let message = error(
        &db,
        "base MATCH (p IS person) RETURN p ORDER BY p NEXT RETURN p._key AS k",
        &[],
    );
    contains(&message, "ELEMENT_ID(p)");
    contains(&message, "p._key");
    let message = error(
        &db,
        "base MATCH (p IS person) RETURN ARRAY_AGG(p.age) AS ages ORDER BY ages",
        &[],
    );
    contains(&message, "list");
}

#[test]
fn offset_and_limit_take_literals_and_parameters_in_range() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let ordered = "base MATCH (p IS person) RETURN p._key AS k ORDER BY k";
    let page = |tail: &str, params: &[Param]| lines(&run(&db, &format!("{ordered} {tail}"), params));
    assert_eq!(page("OFFSET 2 LIMIT 2", &[]), ["p3", "p4"]);
    assert_eq!(page("LIMIT 0", &[]), Vec::<String>::new());
    assert_eq!(page("OFFSET 10", &[]), Vec::<String>::new());
    assert_eq!(page("OFFSET 5 LIMIT 10", &[]), ["p6"]);
    assert_eq!(page("OFFSET 0", &[]).len(), 6);
    // The lexer reads a number as a double, so a literal count is read
    // exactly only below 2^53; from there it is refused and `$n` reaches
    // i64::MAX.
    assert_eq!(page("LIMIT 9007199254740991", &[]).len(), 6);
    assert_eq!(page("LIMIT $1", &[Param::Int(i64::MAX)]).len(), 6);
    assert_eq!(page("LIMIT $1", &[Param::Int(3)]), ["p1", "p2", "p3"]);
    assert_eq!(
        page("OFFSET $1 LIMIT $2", &[Param::Int(4), Param::Int(1)]),
        ["p5"]
    );
    // A rebind is a new page, compiled once.
    let mut prepared = prepare_sql(&db, &statement(&format!("{ordered} LIMIT $1")), &[Param::Int(1)])
        .unwrap();
    prepared.bind(&db, &[Param::Int(2)]).unwrap();
    let SqlResult::Rows { rows, .. } = prepared.run(&db).unwrap() else { panic!() };
    assert_eq!(rows.len(), 2);
    // Q13: an integer from 0 to i64::MAX; anything else fails at bind.
    for bad in [Param::Int(-1), Param::Null, Param::Text("2".into()), Param::Float(1.5)] {
        let err = run_with(&db, &format!("{ordered} LIMIT $1"), &[bad.clone()]).err();
        assert!(
            matches!(err, Some(SqlError::Parameter(_))),
            "LIMIT {bad:?}: {err:?}"
        );
    }
    // A literal count is an integer literal.
    for bad in ["LIMIT -1", "LIMIT 1.5", "OFFSET 'a'", "LIMIT 9007199254740992", "LIMIT x"] {
        let err = run_with(&db, &format!("{ordered} {bad}"), &[]).err();
        assert!(err.is_some(), "`{bad}` was accepted");
    }
    // Without ORDER BY a LIMIT still bounds the answer.
    assert_eq!(
        run(&db, "base MATCH (p IS person) RETURN p._key AS k LIMIT 2", &[]).rows.len(),
        2
    );
}

#[test]
fn return_distinct_keeps_each_row_once() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let reached = "base MATCH (b IS band WHERE b._key = 'b1')-[:collab]-(p IS person) ";
    let all = run(&db, &format!("{reached} RETURN p._key AS k ORDER BY k"), &[]);
    assert_eq!(lines(&all), ["p1", "p1", "p2", "p3", "p6"]);
    let distinct = run(&db, &format!("{reached} RETURN DISTINCT p._key AS k ORDER BY k"), &[]);
    assert_eq!(lines(&distinct), ["p1", "p2", "p3", "p6"]);
    let ages = run(
        &db,
        "base MATCH (p IS person) RETURN DISTINCT p.age AS age ORDER BY age",
        &[],
    );
    assert_eq!(lines(&ages), ["25", "33", "41", "NULL"]);
    // Under DISTINCT a sort key must be a returned column, as in SQL.
    let message = error(
        &db,
        "base MATCH (p IS person) RETURN DISTINCT p.age AS age ORDER BY p._key",
        &[],
    );
    contains(&message, "DISTINCT");
}

// ── LET and FILTER ─────────────────────────────────────────────────────────

#[test]
fn let_does_not_see_its_own_list_and_a_later_let_does() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let p1 = "base MATCH (p IS person WHERE p._key = 'p1') ";
    let message = error(&db, &format!("{p1} LET a = p.age, b = a + 1 RETURN b"), &[]);
    contains(&message, "`a`");
    contains(&message, "LET");
    let answer = run(
        &db,
        &format!("{p1} LET a = p.age LET b = a + 1 FILTER b > 40 RETURN a, b"),
        &[],
    );
    assert_eq!(answer.columns, ["a", "b"]);
    assert_eq!(lines(&answer), ["41|42"]);
    let answer = run(&db, &format!("{p1} LET a = p.age FILTER a > 50 RETURN a"), &[]);
    assert!(answer.rows.is_empty());
    // FILTER keeps only TRUE: an unknown comparison drops the row.
    let answer = run(
        &db,
        "base MATCH (p IS person) LET a = p.age FILTER a < 30 OR a > 40 RETURN p._key AS k ORDER BY k",
        &[],
    );
    assert_eq!(lines(&answer), ["p1", "p2", "p5"]);
    // A statement sees only what the statements before it bound.
    let message = error(
        &db,
        "base LET a = q.age MATCH (q IS person WHERE q._key = 'p1') RETURN a",
        &[],
    );
    contains(&message, "`q`");
    // A LET name is one variable of the stage.
    let message = error(&db, &format!("{p1} LET p = 1 RETURN p"), &[]);
    contains(&message, "`p`");
}

#[test]
fn a_bare_return_stage_needs_no_match() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let answer = run(&db, "base RETURN 1 AS one, 'x' AS t", &[]);
    assert_eq!(lines(&answer), ["1|x"]);
    let answer = run(&db, "base RETURN COUNT(*) AS n", &[]);
    assert_eq!(lines(&answer), ["1"]);
    let answer = run(&db, "base LET x = $1 RETURN x * 2 AS y", &[Param::Int(21)]);
    assert_eq!(lines(&answer), ["42"]);
}

#[test]
fn return_star_projects_every_variable_in_scope() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let answer = run(&db, "base LET a = 1 LET b = 'x' RETURN *", &[]);
    assert_eq!(answer.columns, ["a", "b"]);
    assert_eq!(lines(&answer), ["1|x"]);
    // Through NEXT, elements included; an anonymous element is no column.
    let answer = run(
        &db,
        "base MATCH (b IS band WHERE b._key = 'b1')-[:collab]-(p IS person) RETURN * \
         NEXT RETURN DISTINCT b._key AS band, p._key AS k ORDER BY k",
        &[],
    );
    assert_eq!(lines(&answer), ["b1|p1", "b1|p2", "b1|p3", "b1|p6"]);
    // In the last stage a node is no SQL value.
    let message = error(&db, "base MATCH (p IS person) RETURN *", &[]);
    contains(&message, "ELEMENT_ID(p)");
}

// ── EXPLAIN ────────────────────────────────────────────────────────────────

#[test]
fn explain_shows_every_stage_with_its_schema_and_operators() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let text = explain_sql(&db, &statement(&collaborators("10")), &[Param::Text("b1".into())])
        .unwrap();
    has_line(&text, "GQL plan over graph `base` (the base graph), 2 stages");
    has_line(&text, "stage 1:");
    has_line(&text, "stage 2:");
    // Stage 2's schema starts with what stage 1 returned.
    has_line(&text, "0 person: node of person, never null, returned by stage 1");
    has_line(
        &text,
        "1 direct_collaborations: BIGINT, never null, returned by stage 1",
    );
    contains(&text, "Aggregate by person: direct_collaborations := COUNT(c)");
    contains(&text, "Sort by direct_collaborations DESC, person._key");
    contains(&text, "Page LIMIT 10");
    contains(&text, "NEXT");
    contains(&text, "COUNT(DISTINCT other)");
    has_line(&text, "rows: 3 in 1 page(s)");
}
