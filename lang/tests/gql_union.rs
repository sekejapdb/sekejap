//! `UNION [ALL | DISTINCT]` inside a GQL body (M5-E of
//! `docs/lang/GQL_PROFILE_DESIGN_M5_M7.md` §2.5, Q23-Q25).
//!
//! What is at risk, and the test that pins it:
//!
//! * `UNION` and `UNION DISTINCT` remove duplicate WHOLE rows, `UNION ALL`
//!   keeps them, branch order then row order (`union_removes_*`,
//!   `a_row_is_a_duplicate_*`, `union_all_gives_*`);
//! * the column rules (Q23): every branch returns the same columns, by name
//!   and in order, or the statement is refused: `42601` for a different
//!   column count, as PostgreSQL, and `42804` otherwise; `BIGINT` and
//!   `DOUBLE PRECISION` unify to `DOUBLE PRECISION`, and `1` and `1.0` are
//!   one row under `UNION` (`branches_must_*`, `integer_and_float_*`);
//! * precedence: `A UNION B NEXT C` is `(A UNION B) NEXT C`, so `C`
//!   aggregates the whole union once (`a_stage_after_next_*`);
//! * a union after `NEXT` gives EVERY branch the whole incoming table
//!   (Q25), so a branch may aggregate it (`a_union_after_next_*`);
//! * a node crosses a union into a later stage, and `UNION` compares it by
//!   identity (`a_node_crosses_*`);
//! * a branch orders and pages its own rows, even by a key it does not
//!   return, and that key never makes two equal rows differ
//!   (`a_branch_orders_*`);
//! * `EXISTS` inside a branch -- in its statements and in its `RETURN` --
//!   runs, per branch, from that branch's rows (`an_exists_in_a_branch_*`);
//! * scope after a union: a variable no branch returned is out of scope,
//!   named by the stage that dropped it (`a_variable_*`);
//! * the outer `SELECT` reads a union as one relation (`the_outer_select_*`);
//! * read page by page, a union -- buffered or not, `DISTINCT` or not --
//!   gives exactly its one-shot answer (`pages_of_a_union_*`);
//! * a path search in a branch after `NEXT` sees the incoming columns as
//!   bound, exactly as it does with no union (`a_path_search_*`); and one
//!   BEFORE a union keeps one row per path, since a branch may count them
//!   (`a_path_search_before_*`).
//!
//! The workload is invented, in the README's tourism world: troupes, their
//! dancers, and the dances they perform.

use sekejap_core::collections::{Database, GraphContextId, QueryBudget};
use sekejap_core::Kind;
use sekejap_lang::{prepare_sql, Param, SqlDatabase, SqlError, SqlResult, SqlRow, SqlValue};
use serde_json::json;
use tempfile::TempDir;

mod common;
use common::cfg;

/// ```text
/// troupe  ta "troupe a", tb "troupe b"
/// dancer  d1 age 30, d2 age 24, d3 age 41, d4 age 19
/// dance   kecak (60 min, 4.5), legong (45, 4.0), fire (30, 3.5), welcome (20, 5.0)
///
/// member_of  d1->ta  d2->ta  d3->tb
/// performs   ta->kecak  ta->legong  tb->fire
///            d1->legong  d1->fire  d2->welcome  d3->kecak
/// teaches    d1->d2 twice (two lessons)  d2->d4  d1->d4
/// ```
fn fixture(dir: &TempDir) -> Database {
    let mut db = Database::create(dir.path().join("g.sekejap"), cfg()).unwrap();
    let text = |name: &str| (name.to_owned(), Kind::Text);
    let int = |name: &str| (name.to_owned(), Kind::Int);
    let real = |name: &str| (name.to_owned(), Kind::Real);
    let troupe = db
        .create_collection("troupe", vec![text("name")], Default::default())
        .unwrap();
    let dancer = db
        .create_collection("dancer", vec![text("name"), int("age")], Default::default())
        .unwrap();
    let dance = db
        .create_collection(
            "dance",
            vec![text("title"), int("minutes"), real("rating")],
            Default::default(),
        )
        .unwrap();
    let ta = db.put(troupe, "ta", &json!({"name": "troupe a"})).unwrap();
    let tb = db.put(troupe, "tb", &json!({"name": "troupe b"})).unwrap();
    let mut d = Vec::new();
    for (key, age) in [("d1", 30), ("d2", 24), ("d3", 41), ("d4", 19)] {
        d.push(
            db.put(dancer, key, &json!({"name": format!("dancer {key}"), "age": age}))
                .unwrap(),
        );
    }
    let mut dances = Vec::new();
    for (key, title, minutes, rating) in [
        ("kecak", "kecak", 60, 4.5),
        ("legong", "legong", 45, 4.0),
        ("fire", "fire dance", 30, 3.5),
        ("welcome", "welcome dance", 20, 5.0),
    ] {
        dances.push(
            db.put(
                dance,
                key,
                &json!({"title": title, "minutes": minutes, "rating": rating}),
            )
            .unwrap(),
        );
    }
    let [kecak, legong, fire, welcome] = dances[..] else { unreachable!() };
    db.enable_graph().unwrap();
    let member_of = db.create_edge_type("member_of").unwrap();
    let performs = db.create_edge_type("performs").unwrap();
    let teaches = db.create_edge_type("teaches").unwrap();
    let base = GraphContextId::BASE;
    for (source, edge_type, destination) in [
        (d[0], member_of, ta),
        (d[1], member_of, ta),
        (d[2], member_of, tb),
        (ta, performs, kecak),
        (ta, performs, legong),
        (tb, performs, fire),
        (d[0], performs, legong),
        (d[0], performs, fire),
        (d[1], performs, welcome),
        (d[2], performs, kecak),
        (d[0], teaches, d[1]),
        (d[0], teaches, d[1]),
        (d[1], teaches, d[3]),
        (d[0], teaches, d[3]),
    ] {
        db.create_edge(base, source, edge_type, destination, &json!({}))
            .unwrap();
    }
    db.commit().unwrap();
    db
}

fn statement(body: &str) -> String {
    format!("SELECT * FROM GRAPH_TABLE (base {body})")
}

fn rows(db: &Database, sql: &str, params: &[Param]) -> Result<Vec<Vec<SqlValue>>, SqlError> {
    let prepared = prepare_sql(db, sql, params)?;
    let SqlResult::Rows { rows, .. } = prepared.run(db)? else {
        panic!("`{sql}` did not answer with rows");
    };
    Ok(rows.into_iter().map(|row| row.values).collect())
}

/// The rows of `body`, IN ORDER, each as `a|b|c`.
fn lines(db: &Database, body: &str) -> Vec<String> {
    lines_of(db, &statement(body))
}

fn lines_of(db: &Database, sql: &str) -> Vec<String> {
    rows(db, sql, &[])
        .unwrap_or_else(|error| panic!("`{sql}` failed: {error}"))
        .iter()
        .map(|row| row.iter().map(cell).collect::<Vec<_>>().join("|"))
        .collect()
}

/// The rows of `body`, sorted: an answer compared as a bag.
fn bag(db: &Database, body: &str) -> Vec<String> {
    let mut all = lines(db, body);
    all.sort();
    all
}

fn cell(value: &SqlValue) -> String {
    match value {
        SqlValue::Text(text) => text.clone(),
        SqlValue::Int(i) => i.to_string(),
        SqlValue::Float(f) => format!("{f:?}"),
        SqlValue::Null => "NULL".to_owned(),
        other => format!("{other:?}"),
    }
}

fn error(db: &Database, body: &str) -> SqlError {
    match rows(db, &statement(body), &[]) {
        Ok(rows) => panic!("`{body}` answered {} row(s) instead of failing", rows.len()),
        Err(error) => error,
    }
}

fn sqlstate(error: &SqlError) -> Option<&'static str> {
    match error {
        SqlError::Coded { sqlstate, .. } => Some(sqlstate),
        _ => None,
    }
}

/// Dances troupe `ta` performs itself, and dances its members perform
/// (brief §9.1, re-expressed): `legong` is reached both ways.
const DIRECT: &str = "MATCH (t IS troupe WHERE t._key = 'ta')-[:performs]->(d IS dance) \
                      RETURN d.title AS title";
const BY_MEMBERS: &str = "MATCH (t IS troupe WHERE t._key = 'ta')<-[:member_of]-(m IS dancer)\
                          -[:performs]->(d IS dance) RETURN d.title AS title";

#[test]
fn union_removes_duplicate_whole_rows_and_union_all_keeps_them() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let distinct = ["fire dance", "kecak", "legong", "welcome dance"];
    for conjunction in ["UNION", "UNION DISTINCT"] {
        assert_eq!(
            lines(&db, &format!("{DIRECT} {conjunction} {BY_MEMBERS} NEXT RETURN title ORDER BY title")),
            distinct,
            "{conjunction}"
        );
    }
    assert_eq!(
        lines(&db, &format!("{DIRECT} UNION ALL {BY_MEMBERS} NEXT RETURN title ORDER BY title")),
        ["fire dance", "kecak", "legong", "legong", "welcome dance"]
    );
    // The last part of the body may be the union itself.
    assert_eq!(bag(&db, &format!("{DIRECT} UNION {BY_MEMBERS}")), distinct);
    assert_eq!(bag(&db, &format!("{DIRECT} UNION ALL {BY_MEMBERS}")).len(), 5);
}

#[test]
fn a_row_is_a_duplicate_only_when_every_column_is_equal() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let body = "MATCH (t IS troupe WHERE t._key = 'ta')-[:performs]->(d IS dance) \
                RETURN d.title AS title, 'direct' AS route \
                UNION MATCH (t IS troupe WHERE t._key = 'ta')<-[:member_of]-(m IS dancer)\
                -[:performs]->(d IS dance) RETURN d.title AS title, 'member' AS route";
    assert_eq!(
        bag(&db, body),
        [
            "fire dance|member",
            "kecak|direct",
            "legong|direct",
            "legong|member",
            "welcome dance|member",
        ]
    );
    // Duplicates inside ONE branch go too: UNION is DISTINCT over the whole
    // answer, not a merge of two sets.
    let twice = "MATCH (m IS dancer)-[:performs]->(d IS dance) RETURN d.title AS title \
                 UNION MATCH (d IS dance WHERE d._key = 'welcome') RETURN d.title AS title";
    assert_eq!(
        bag(&db, twice),
        ["fire dance", "kecak", "legong", "welcome dance"]
    );
}

#[test]
fn union_all_gives_branch_order_then_row_order() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let body = "MATCH (d IS dance WHERE d._key = 'welcome') RETURN d.title AS title \
                UNION ALL MATCH (d IS dance WHERE d._key = 'kecak') RETURN d.title AS title \
                UNION ALL MATCH (d IS dance WHERE d._key = 'welcome') RETURN d.title AS title";
    assert_eq!(lines(&db, body), ["welcome dance", "kecak", "welcome dance"]);
    // UNION keeps each row's first occurrence, in that order.
    let distinct = body.replace("UNION ALL", "UNION");
    assert_eq!(lines(&db, &distinct), ["welcome dance", "kecak"]);
}

#[test]
fn branches_must_return_the_same_columns_by_name_and_in_order() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let kecak = "MATCH (d IS dance WHERE d._key = 'kecak')";
    for (body, says, code) in [
        // A different count is PostgreSQL's syntax_error.
        (
            format!("{kecak} RETURN d.title AS title UNION {kecak} RETURN d.title AS title, d.minutes AS minutes"),
            "2 columns",
            "42601",
        ),
        (
            format!("{kecak} RETURN d.title AS title UNION {kecak} RETURN d.title AS name"),
            "`name`",
            "42804",
        ),
        (
            format!(
                "{kecak} RETURN d.title AS title, d.minutes AS minutes \
                 UNION ALL {kecak} RETURN d.minutes AS minutes, d.title AS title"
            ),
            "`minutes`",
            "42804",
        ),
    ] {
        let error = error(&db, &body);
        assert_eq!(sqlstate(&error), Some(code), "`{body}`: {error}");
        let text = error.to_string();
        assert!(text.contains("branch 2"), "`{body}`: {text}");
        assert!(text.contains(says), "`{body}`: no {says} in {text}");
    }
}

#[test]
fn integer_and_float_columns_unify_to_double_precision() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let body = "MATCH (d IS dance WHERE d._key = 'kecak') RETURN d.minutes AS v \
                UNION ALL MATCH (d IS dance WHERE d._key = 'legong') RETURN d.rating AS v";
    let prepared = prepare_sql(&db, &statement(body), &[]).unwrap();
    assert_eq!(prepared.column_type(0), Some("DOUBLE PRECISION"));
    let numbers: Vec<f64> = rows(&db, &statement(body), &[])
        .unwrap()
        .into_iter()
        .map(|row| match row[0] {
            SqlValue::Int(i) => i as f64,
            SqlValue::Float(f) => f,
            ref other => panic!("not a number: {other:?}"),
        })
        .collect();
    assert_eq!(numbers, [60.0, 4.0]);
    // `1` and `1.0` are one value, so one row under UNION.
    assert_eq!(lines(&db, "RETURN 1 AS v UNION RETURN 1.0 AS v").len(), 1);
    assert_eq!(lines(&db, "RETURN 1 AS v UNION ALL RETURN 1.0 AS v").len(), 2);
    // Anything else does not unify: 42804, naming the column.
    let error = error(
        &db,
        "MATCH (d IS dance WHERE d._key = 'kecak') RETURN d.minutes AS v \
         UNION MATCH (d IS dance WHERE d._key = 'kecak') RETURN d.title AS v",
    );
    assert_eq!(sqlstate(&error), Some("42804"), "{error}");
    assert!(error.to_string().contains("`v`"), "{error}");
}

#[test]
fn a_stage_after_next_aggregates_the_whole_union_once() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    assert_eq!(
        lines(&db, &format!("{DIRECT} UNION ALL {BY_MEMBERS} NEXT RETURN COUNT(*) AS n")),
        ["5"]
    );
    assert_eq!(
        lines(&db, &format!("{DIRECT} UNION {BY_MEMBERS} NEXT RETURN COUNT(*) AS n")),
        ["4"]
    );
}

#[test]
fn a_union_after_next_gives_every_branch_the_whole_incoming_table() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // Four dancers come in; three of them belong to a troupe.
    let body = "MATCH (m IS dancer) RETURN m \
                NEXT MATCH (m)-[:member_of]->(t IS troupe) RETURN COUNT(*) AS n \
                UNION ALL RETURN COUNT(*) AS n";
    assert_eq!(lines(&db, body), ["3", "4"]);
    // And the union's rows feed the next stage as one table.
    let summed = format!("{body} NEXT RETURN SUM(n) AS total");
    assert_eq!(lines(&db, &summed), ["7"]);
    // A key seed in a branch after NEXT runs per incoming row.
    let per_row = "MATCH (m IS dancer) RETURN m \
                   NEXT MATCH (d IS dance WHERE d._key = 'kecak') RETURN d.title AS title \
                   UNION RETURN 'none' AS title";
    assert_eq!(bag(&db, per_row), ["kecak", "none"]);
    let all = per_row.replace("UNION", "UNION ALL");
    assert_eq!(bag(&db, &all).len(), 8);
}

#[test]
fn a_node_crosses_a_union_into_a_later_stage() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let body = "MATCH (t IS troupe WHERE t._key = 'ta')-[:performs]->(d IS dance) RETURN d \
                UNION MATCH (m IS dancer WHERE m._key = 'd1')-[:performs]->(d IS dance) RETURN d \
                NEXT RETURN d.title AS title ORDER BY title";
    assert_eq!(lines(&db, body), ["fire dance", "kecak", "legong"]);
    // A node of either branch's label: the column is a node of both.
    let mixed = "MATCH (t IS troupe WHERE t._key = 'tb') RETURN t AS x \
                 UNION ALL MATCH (d IS dance WHERE d._key = 'fire') RETURN d AS x \
                 NEXT RETURN x._key AS k";
    assert_eq!(bag(&db, mixed), ["fire", "tb"]);
}

#[test]
fn a_branch_orders_and_limits_its_own_rows() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // Each branch sorts by a key it does not return, then keeps its top.
    let body = "MATCH (d IS dance) RETURN d.title AS title ORDER BY d.minutes DESC LIMIT 1 \
                UNION ALL MATCH (d IS dance) RETURN d.title AS title ORDER BY d.minutes LIMIT 1";
    assert_eq!(lines(&db, body), ["kecak", "welcome dance"]);
    // The hidden sort key never makes two equal rows differ.
    let distinct = "MATCH (d IS dance) RETURN d.title AS title ORDER BY d.minutes DESC LIMIT 1 \
                    UNION MATCH (d IS dance) RETURN d.title AS title ORDER BY d.rating DESC LIMIT 2";
    assert_eq!(lines(&db, distinct), ["kecak", "welcome dance"]);
    // And a later stage sees only the returned columns.
    let next = format!("{distinct} NEXT RETURN COUNT(*) AS n");
    assert_eq!(lines(&db, &next), ["2"]);
}

#[test]
fn a_variable_no_branch_returned_is_out_of_scope_after_the_union() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let error = error(
        &db,
        "MATCH (d IS dance WHERE d._key = 'kecak') RETURN d.title AS title \
         UNION MATCH (d IS dance WHERE d._key = 'fire') RETURN d.title AS title \
         NEXT RETURN d.minutes AS m",
    );
    assert_eq!(sqlstate(&error), Some("42703"), "{error}");
    assert!(error.to_string().contains("stage 1"), "{error}");
}

#[test]
fn the_outer_select_reads_a_union_as_one_relation() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let sql = format!(
        "SELECT r.title FROM GRAPH_TABLE (base {DIRECT} UNION {BY_MEMBERS}) AS r \
         WHERE r.title <> 'kecak' ORDER BY r.title DESC"
    );
    assert_eq!(lines_of(&db, &sql), ["welcome dance", "legong", "fire dance"]);
}

#[test]
fn a_path_search_in_a_branch_after_next_sees_the_incoming_columns_as_bound() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    // `t` comes in bound to `tb`; d1 belongs to `ta`, so the search from d1
    // must end nowhere -- it may not rebind `t` to the troupe it reaches.
    let first = "MATCH (m IS dancer WHERE m._key = 'd1'), (t IS troupe WHERE t._key = 'tb') \
                 RETURN m, t NEXT ";
    let search = "MATCH (m)-[:member_of]->{1,2}(t IS troupe) RETURN DISTINCT t.name AS name";
    assert_eq!(lines(&db, &format!("{first}{search}")), Vec::<String>::new());
    assert_eq!(
        lines(&db, &format!("{first}{search} UNION ALL RETURN t.name AS name")),
        ["troupe b"]
    );
    // The branch's search is planned exactly as the stage's alone.
    let alone = explain(&mut db, &format!("{first}{search}"));
    let branch = explain(&mut db, &format!("{first}{search} UNION ALL RETURN t.name AS name"));
    let reach = |text: &str| {
        text.lines()
            .find(|line| line.contains("node BFS"))
            .map(|line| line.trim().split_once(' ').map_or("", |(_, rest)| rest).to_owned())
    };
    assert_eq!(reach(&branch), reach(&alone), "\n{alone}\n{branch}");
}

fn explain(db: &mut Database, body: &str) -> String {
    match db
        .sql(&format!("EXPLAIN {}", statement(body)), &[])
        .unwrap_or_else(|error| panic!("EXPLAIN `{body}`: {error}"))
    {
        SqlResult::Explain(text) => text,
        other => panic!("not an explanation: {other:?}"),
    }
}

#[test]
fn a_path_search_before_a_union_keeps_one_row_per_path_for_the_branches() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    // Five paths from d1 over `teaches`, to two dancers: each branch counts
    // the paths, and the union's Distinct after them does not make the
    // search's rows a set.
    let body = "MATCH ACYCLIC (m IS dancer WHERE m._key = 'd1')-[:teaches]->{1,2}(x IS dancer) \
                RETURN x NEXT RETURN COUNT(*) AS n UNION RETURN COUNT(*) AS n";
    assert_eq!(lines(&db, body), ["5"]);
    let text = explain(&mut db, body);
    assert!(
        text.contains("not Reach: rule 4 fails -- a UNION after it reads its rows in branches"),
        "{text}"
    );
}

#[test]
fn pages_of_a_union_concatenate_to_its_one_shot_answer() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    for body in [
        format!("{DIRECT} UNION {BY_MEMBERS}"),
        format!("{DIRECT} UNION ALL {BY_MEMBERS} NEXT RETURN title ORDER BY title"),
        "MATCH (m IS dancer) RETURN m \
         NEXT MATCH (m)-[:performs]->(d IS dance) RETURN d.title AS title \
         UNION ALL MATCH (m)-[:member_of]->(t IS troupe) RETURN t.name AS title \
         UNION ALL RETURN m.name AS title"
            .to_owned(),
        "MATCH (m IS dancer) RETURN m \
         NEXT MATCH (m)-[:performs]->(d IS dance) RETURN d.title AS title \
         UNION MATCH (m)-[:member_of]->(t IS troupe)-[:performs]->(d IS dance) RETURN d.title AS title"
            .to_owned(),
    ] {
        let sql = statement(&body);
        let whole = rows(&db, &sql, &[]).unwrap_or_else(|e| panic!("`{body}`: {e}"));
        assert!(whole.len() > 3, "`{body}` is too small to page: {}", whole.len());
        let prepared = prepare_sql(&db, &sql, &[]).unwrap();
        for page_rows in [1, 2, 3, 7] {
            let mut paged = Vec::new();
            prepared
                .for_each_row_with(&db, page_rows, QueryBudget::unlimited(), &mut || false, &mut |row: &SqlRow| {
                    paged.push(row.values.clone());
                    Ok(())
                })
                .unwrap_or_else(|e| panic!("`{body}` at {page_rows} per page: {e}"));
            assert_eq!(paged, whole, "`{body}` at {page_rows} per page");
        }
    }
}

#[test]
fn an_exists_in_a_branch_runs_from_that_branch_s_rows() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // Filter form in a branch's MATCH: dancers who belong to a troupe,
    // UNION ALL dancers who belong to none.
    let body = "MATCH (m IS dancer) FILTER EXISTS { MATCH (m)-[:member_of]->(:troupe) } \
                RETURN m._key AS k, 'member' AS kind \
                UNION ALL MATCH (m IS dancer) FILTER NOT EXISTS { MATCH (m)-[:member_of]->() } \
                RETURN m._key AS k, 'free' AS kind";
    assert_eq!(
        bag(&db, body),
        ["d1|member", "d2|member", "d3|member", "d4|free"]
    );
    // Mark form in a branch's RETURN: the test is a column of the union.
    let marked = "MATCH (d IS dance WHERE d._key = 'welcome') \
                  RETURN d.title AS title, EXISTS { MATCH (d)<-[:performs]-(:troupe) } AS by_troupe \
                  UNION MATCH (d IS dance WHERE d._key = 'kecak') \
                  RETURN d.title AS title, EXISTS { MATCH (d)<-[:performs]-(:troupe) } AS by_troupe";
    let prepared = prepare_sql(&db, &statement(marked), &[]).unwrap();
    assert_eq!(prepared.column_type(1), Some("BOOLEAN"));
    assert_eq!(lines(&db, marked), ["welcome dance|Bool(false)", "kecak|Bool(true)"]);
    // After NEXT, a branch's EXISTS reads the incoming row.
    let after = "MATCH (m IS dancer) RETURN m \
                 NEXT FILTER EXISTS { MATCH (m)-[:teaches]->() } RETURN m._key AS k \
                 UNION RETURN 'any' AS k";
    assert_eq!(bag(&db, after), ["any", "d1", "d2"]);
}
