//! `CALL (imports) { ... }` inside a GQL body (M5-D of
//! `docs/lang/GQL_PROFILE_DESIGN_M5_M7.md` §2.4; owner answers Q20-Q22).
//!
//! What is at risk, and the test that pins it:
//!
//! * per input row the body runs from that row and each row its `RETURN`
//!   gives extends the input row -- per-row top-k is the point
//!   (`a_call_body_ranks_and_pages_per_input_row`);
//! * a body whose `RETURN` aggregates with no key gives one row even over
//!   nothing, so every input row is kept with `COUNT` 0
//!   (`an_aggregating_body_keeps_every_input_row`), while a body that gives
//!   no row drops its input row, an inner join
//!   (`an_empty_body_drops_its_input_row`);
//! * the body sees only its imports, `()` importing nothing
//!   (`the_body_sees_only_its_imports`), and a returned column may not reuse
//!   a name in scope (`a_returned_column_may_not_reuse_a_name_in_scope`);
//! * the refusals: a bare `CALL { }`, a CALL inside an EXISTS
//!   (`the_refusals_name_the_rule`);
//! * `EXPLAIN` prints the `CallApply` and its steps
//!   (`explain_prints_the_call_apply`).
//!
//! Workload names are invented, from the README's tourism world: troupes,
//! dancers and the dances they perform.

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::{Database, EntityId, GraphContextId};
use sekejap_core::Kind;
use sekejap_lang::{explain_sql, prepare_sql, SqlError, SqlResult, SqlValue};
use serde_json::json;
use tempfile::TempDir;

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

/// ```text
/// troupe t1 -member-> d1, d2, d3     t2 -member-> d4     t3 (no member)
/// d1 -performs-> kecak, legong       d2 -performs-> kecak
/// d4 -performs-> fire dance          d3 performs nothing
/// ```
fn fixture(dir: &TempDir) -> Database {
    let mut db = Database::create(dir.path().join("call.sekejap"), cfg()).unwrap();
    let text = |name: &str| (name.to_owned(), Kind::Text);
    let troupe = db.create_collection("troupe", vec![text("name")], Default::default()).unwrap();
    let dancer = db.create_collection("dancer", vec![text("name")], Default::default()).unwrap();
    let dance = db.create_collection("dance", vec![text("title")], Default::default()).unwrap();
    let put = |db: &mut Database, c, key: &str, row: serde_json::Value| -> EntityId { db.put(c, key, &row).unwrap() };
    let t: Vec<EntityId> = (1..=3).map(|i| put(&mut db, troupe, &format!("t{i}"), json!({"name": format!("troupe {i}")}))).collect();
    let d: Vec<EntityId> = (1..=4).map(|i| put(&mut db, dancer, &format!("d{i}"), json!({"name": format!("dancer {i}")}))).collect();
    let k: Vec<EntityId> = ["kecak", "legong", "fire dance"]
        .iter()
        .enumerate()
        .map(|(i, title)| put(&mut db, dance, &format!("k{}", i + 1), json!({"title": title})))
        .collect();
    db.enable_graph().unwrap();
    let member = db.create_edge_type("member").unwrap();
    let performs = db.create_edge_type("performs").unwrap();
    let base = GraphContextId::BASE;
    for (from, to) in [(t[0], d[0]), (t[0], d[1]), (t[0], d[2]), (t[1], d[3])] {
        db.create_edge(base, from, member, to, &json!({})).unwrap();
    }
    for (from, to) in [(d[0], k[0]), (d[0], k[1]), (d[1], k[0]), (d[3], k[2])] {
        db.create_edge(base, from, performs, to, &json!({})).unwrap();
    }
    db.commit().unwrap();
    db
}

fn run(db: &Database, body: &str) -> Result<Vec<Vec<SqlValue>>, SqlError> {
    let text = format!("SELECT * FROM GRAPH_TABLE (base {body})");
    match prepare_sql(db, &text, &[])?.run(db)? {
        SqlResult::Rows { rows, .. } => Ok(rows.into_iter().map(|row| row.values).collect()),
        other => panic!("`{text}` answered {other:?}"),
    }
}

/// The rows as sorted text, `|`-joined: a GQL answer is a bag.
fn bag(db: &Database, body: &str) -> Vec<String> {
    let mut rows: Vec<String> = run(db, body)
        .unwrap_or_else(|error| panic!("`{body}`: {error}"))
        .into_iter()
        .map(|row| {
            row.iter()
                .map(|value| match value {
                    SqlValue::Text(text) => text.clone(),
                    SqlValue::Int(i) => i.to_string(),
                    SqlValue::Null => "NULL".to_owned(),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect();
    rows.sort();
    rows
}

#[test]
fn a_call_body_ranks_and_pages_per_input_row() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // Each troupe's first dancer by name: a per-row top-1, not a global one.
    assert_eq!(
        bag(
            &db,
            "MATCH (t IS troupe) \
             CALL (t) { MATCH (t)-[:member]->(d IS dancer) RETURN d.name AS first ORDER BY d.name LIMIT 1 } \
             RETURN t._key AS troupe, first"
        ),
        ["t1|dancer 1", "t2|dancer 4"]
    );
}

#[test]
fn an_aggregating_body_keeps_every_input_row() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    assert_eq!(
        bag(
            &db,
            "MATCH (t IS troupe) \
             CALL (t) { MATCH (t)-[:member]->(d IS dancer) RETURN COUNT(*) AS members } \
             RETURN t._key AS troupe, members"
        ),
        ["t1|3", "t2|1", "t3|0"]
    );
}

#[test]
fn an_empty_body_drops_its_input_row() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // d3 performs nothing: the body gives no row, so d3 is gone.
    assert_eq!(
        bag(
            &db,
            "MATCH (d IS dancer) \
             CALL (d) { MATCH (d)-[:performs]->(k IS dance) RETURN k.title AS dance } \
             RETURN d._key AS dancer, dance"
        ),
        ["d1|kecak", "d1|legong", "d2|kecak", "d4|fire dance"]
    );
}

#[test]
fn the_body_sees_only_its_imports() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // `t` is not imported: inside the body it is not bound.
    let error = run(
        &db,
        "MATCH (t IS troupe) CALL () { MATCH (t)-[:member]->(d IS dancer) RETURN d.name AS n } RETURN n",
    );
    // A body that names `t` without importing it binds a NEW `t`: every
    // dancer's troupe, per input troupe (an uncorrelated call).
    let rows = error.expect("an uncorrelated body binds its own t");
    assert_eq!(rows.len(), 3 * 4);
    // Importing a name that is not bound is PostgreSQL's 42703.
    let error = run(&db, "MATCH (t IS troupe) CALL (x) { RETURN 1 AS one } RETURN one").unwrap_err();
    assert!(matches!(&error, SqlError::Coded { sqlstate: "42703", .. }), "{error:?}");
}

#[test]
fn a_returned_column_may_not_reuse_a_name_in_scope() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let error = run(
        &db,
        "MATCH (t IS troupe) CALL (t) { MATCH (t)-[:member]->(d IS dancer) RETURN d.name AS t } RETURN t",
    )
    .unwrap_err();
    assert!(matches!(&error, SqlError::Coded { sqlstate: "42712", .. }), "{error:?}");
}

#[test]
fn the_refusals_name_the_rule() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let bare = run(&db, "MATCH (t IS troupe) CALL { RETURN 1 AS one } RETURN one").unwrap_err();
    assert!(bare.to_string().contains("import list"), "{bare}");
    let inside = run(
        &db,
        "MATCH (t IS troupe) FILTER EXISTS { CALL (t) { MATCH (t)-[:member]->(d) RETURN d.name AS n } } RETURN t._key AS k",
    )
    .unwrap_err();
    assert!(inside.to_string().contains("CALL inside an EXISTS body"), "{inside}");
}

#[test]
fn explain_prints_the_call_apply() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let text = explain_sql(
        &db,
        "SELECT * FROM GRAPH_TABLE (base MATCH (t IS troupe) \
         CALL (t) { MATCH (t)-[:member]->(d IS dancer) RETURN d.name AS first ORDER BY d.name LIMIT 1 } \
         RETURN t._key AS troupe, first)",
        &[],
    )
    .unwrap();
    assert!(text.contains("CallApply: per input row"), "{text}");
    assert!(text.contains("an input row they give none is dropped"), "{text}");
}
