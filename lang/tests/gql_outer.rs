//! PostgreSQL parity for the outer `SELECT` over a GQL relation (M3-D2 of
//! `docs/lang/GQL_PROFILE_DESIGN.md` §5.5): the three gaps M3-D left.
//!
//! What is at risk, and the test that pins it:
//!
//! * `SELECT DISTINCT` over the relation is the same `Distinct` operator
//!   `RETURN DISTINCT` uses inside the body, and it still refuses a hidden
//!   `ORDER BY` key exactly as `RETURN DISTINCT` does
//!   (`distinct_over_the_relation_*`); `SELECT DISTINCT ON (...)` stays
//!   refused by name (`lang/tests/gql_prepared.rs`).
//! * `HAVING` is a `Filter` right after the outer `Aggregate`, reading only
//!   a group key or an aggregate -- one it names on its own, not only one
//!   the select list returns -- and PostgreSQL's `42803` otherwise
//!   (`having_*`); with no `GROUP BY` it folds the whole relation into one
//!   group, as PostgreSQL does (`having_with_no_group_by_*`).
//! * An unaliased column of the outer `SELECT` is named as PostgreSQL
//!   names one: an aggregate by its function, a function call by its
//!   function name, a cast by its declared type, anything else
//!   `?column?` -- unlike a body `RETURN`, which keeps its own rule
//!   (`unaliased_outer_columns_*`).
//!
//! Workload names are invented: bands `b1` `b2`, people `p1`-`p5`.

use sekejap_core::collections::{CollectionId, Database, EdgeTypeId, GraphContextId};
use sekejap_core::Kind;
use sekejap_lang::{explain_sql, prepare_sql, Param, PreparedSql, SqlError, SqlResult, SqlValue};
use serde_json::json;
use tempfile::TempDir;

mod common;
use common::cfg;

struct Fixture {
    _dir: TempDir,
    db: Database,
    #[allow(dead_code)]
    person: CollectionId,
    #[allow(dead_code)]
    member_of: EdgeTypeId,
}

/// ```text
/// band   b1 "band one", b2 "band two"
/// person p1 "person one" 41, p2 "person two" 25, p3 "person three" 33,
///        p4 "person four" (no age), p5 "person five" 30
///
/// member_of  p1->b1  p2->b1  p4->b1  p5->b1  p3->b2
/// ```
fn fixture() -> Fixture {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("g.sekejap"), cfg()).unwrap();
    let text = |name: &str| (name.to_owned(), Kind::Text);
    let band = db
        .create_collection("band", vec![text("name")], Default::default())
        .unwrap();
    let person = db
        .create_collection(
            "person",
            vec![text("name"), ("age".to_owned(), Kind::Int)],
            Default::default(),
        )
        .unwrap();
    let b1 = db.put(band, "b1", &json!({"name": "band one"})).unwrap();
    let b2 = db.put(band, "b2", &json!({"name": "band two"})).unwrap();
    let mut p = Vec::new();
    for (key, name, age) in [
        ("p1", "person one", Some(41)),
        ("p2", "person two", Some(25)),
        ("p3", "person three", Some(33)),
        ("p4", "person four", None),
        ("p5", "person five", Some(30)),
    ] {
        let row = match age {
            Some(age) => json!({"name": name, "age": age}),
            None => json!({"name": name}),
        };
        p.push(db.put(person, key, &row).unwrap());
    }
    db.enable_graph().unwrap();
    let member_of = db.create_edge_type("member_of").unwrap();
    let base = GraphContextId::BASE;
    for (from, to) in [(p[0], b1), (p[1], b1), (p[3], b1), (p[4], b1), (p[2], b2)] {
        db.create_edge(base, from, member_of, to, &json!({})).unwrap();
    }
    db.commit().unwrap();
    Fixture {
        _dir: dir,
        db,
        person,
        member_of,
    }
}

fn text(value: &str) -> SqlValue {
    SqlValue::Text(value.to_owned())
}

fn rows_of(db: &Database, prepared: &PreparedSql) -> Result<Vec<Vec<SqlValue>>, SqlError> {
    let SqlResult::Rows { rows, .. } = prepared.run(db)? else {
        panic!("a GQL relation answers with rows")
    };
    Ok(rows.into_iter().map(|row| row.values).collect())
}

fn query(db: &Database, sql: &str, params: &[Param]) -> Result<Vec<Vec<SqlValue>>, SqlError> {
    rows_of(db, &prepare_sql(db, sql, params)?)
}

fn rows(db: &Database, sql: &str, params: &[Param]) -> Vec<Vec<SqlValue>> {
    query(db, sql, params).unwrap_or_else(|error| panic!("`{sql}`: {error}"))
}

fn sqlstate(error: &SqlError) -> &'static str {
    match error {
        SqlError::Coded { sqlstate, .. } => sqlstate,
        other => panic!("not a coded error: {other:?}"),
    }
}

/// Every member of a band, as the relation the outer SELECTs below read.
const MEMBERS: &str = "GRAPH_TABLE (base \
     MATCH (p IS person)-[:member_of]->(b IS band) \
     RETURN b._key AS band, p._key AS person, p.age AS age) AS g";

// ── SELECT DISTINCT ─────────────────────────────────────────────────────────

#[test]
fn distinct_over_the_relation_dedups_the_projected_row_like_return_distinct_does() {
    let f = fixture();
    let db = &f.db;
    // Four people share b1: DISTINCT collapses them to one row per band.
    assert_eq!(
        rows(db, &format!("SELECT DISTINCT g.band FROM {MEMBERS} ORDER BY g.band"), &[]),
        [[text("b1")], [text("b2")]]
    );
    // DISTINCT over the whole row: two different (band, person) pairs never
    // collapse into one.
    assert_eq!(
        rows(
            db,
            &format!("SELECT DISTINCT g.band, g.person FROM {MEMBERS} ORDER BY g.band, g.person"),
            &[]
        )
        .len(),
        5
    );
    // Under DISTINCT, a sort key is a returned column -- DISTINCT compares
    // whole returned rows -- exactly as `RETURN DISTINCT` inside the body
    // refuses a hidden key.
    let error = query(db, &format!("SELECT DISTINCT g.band FROM {MEMBERS} ORDER BY g.age"), &[])
        .unwrap_err();
    match error {
        SqlError::Unsupported(message) => assert!(message.contains("RETURN DISTINCT"), "{message}"),
        other => panic!("{other:?}"),
    }
}

// ── HAVING ───────────────────────────────────────────────────────────────

#[test]
fn having_filters_the_finished_group_naming_a_group_key_or_an_aggregate() {
    let f = fixture();
    let db = &f.db;
    // b1 has 4 members, b2 has 1: HAVING keeps only the finished groups
    // whose accumulator passes.
    assert_eq!(
        rows(
            db,
            &format!("SELECT g.band, count(*) AS n FROM {MEMBERS} GROUP BY g.band HAVING count(*) > 1"),
            &[]
        ),
        [vec![text("b1"), SqlValue::Int(4)]]
    );
    // HAVING may name an aggregate the select list never returns: b1's
    // average age is (41+25+30)/3 = 32.0 (p4 has no age, SQL AVG skips
    // NULL), b2's is 33.0.
    assert_eq!(
        rows(
            db,
            &format!("SELECT g.band FROM {MEMBERS} GROUP BY g.band HAVING avg(g.age) > 32.5 ORDER BY g.band"),
            &[]
        ),
        [[text("b2")]]
    );
    // Anything else in HAVING is PostgreSQL's 42803: `g.age` is neither the
    // group key nor an aggregate.
    let error = query(
        db,
        &format!("SELECT g.band, count(*) AS n FROM {MEMBERS} GROUP BY g.band HAVING g.age > 10"),
        &[],
    )
    .unwrap_err();
    assert_eq!(sqlstate(&error), "42803", "{error}");
}

#[test]
fn having_with_no_group_by_folds_the_whole_relation_into_one_group() {
    let f = fixture();
    let db = &f.db;
    // Five members total: the implicit one group passes.
    assert_eq!(
        rows(db, &format!("SELECT count(*) AS n FROM {MEMBERS} HAVING count(*) > 3"), &[]),
        [[SqlValue::Int(5)]]
    );
    // The same implicit group fails: zero rows, not an error.
    assert_eq!(
        rows(db, &format!("SELECT count(*) AS n FROM {MEMBERS} HAVING count(*) > 10"), &[]),
        Vec::<Vec<SqlValue>>::new()
    );
    // HAVING alone still makes the query grouped: an ungrouped column in
    // the select list is PostgreSQL's 42803, exactly as it would be beside
    // an explicit aggregate.
    let error = query(db, &format!("SELECT g.band FROM {MEMBERS} HAVING count(*) > 0"), &[]).unwrap_err();
    assert_eq!(sqlstate(&error), "42803", "{error}");
}

#[test]
fn explain_shows_distinct_and_having_as_their_own_operators() {
    let f = fixture();
    let plan = explain_sql(
        &f.db,
        &format!("SELECT DISTINCT g.band FROM {MEMBERS} GROUP BY g.band HAVING count(*) > 1"),
        &[],
    )
    .unwrap();
    for expected in ["Aggregate", "as the outer HAVING, over the finished group", "Distinct"] {
        assert!(plan.contains(expected), "no `{expected}` in\n{plan}");
    }
    // The normal form carries HAVING right after GROUP BY.
    assert!(
        plan.contains("GROUP BY band HAVING (COUNT(*) > 1)"),
        "{plan}"
    );
}

// ── unaliased column names ────────────────────────────────────────────────

#[test]
fn unaliased_outer_columns_are_named_as_postgresql_names_them() {
    let f = fixture();
    let db = &f.db;
    // Aggregates, unaliased, fold `b1`'s members into one row -- no plain
    // column beside them, so no GROUP BY is needed.
    let sql = format!("SELECT count(*), avg(g.age) FROM {MEMBERS} WHERE g.band = 'b1'");
    let prepared = prepare_sql(db, &sql, &[]).unwrap();
    assert_eq!(prepared.columns(), ["count", "avg"]);
    // A cast and a scalar function call, unaliased, over one plain row.
    let sql = format!("SELECT g.age::text, LOWER(g.band), g.band FROM {MEMBERS} WHERE g.band = 'b1' LIMIT 1");
    let prepared = prepare_sql(db, &sql, &[]).unwrap();
    assert_eq!(prepared.columns(), ["text", "lower", "band"]);
    // A cast is named by the type's PostgreSQL internal name, and CASE,
    // COALESCE and NULLIF by their keyword, as PostgreSQL names them.
    let sql = format!(
        "SELECT g.age::bigint, g.age::double precision, (g.age > 1)::boolean, \
         COALESCE(g.band, 'x'), NULLIF(g.band, 'x'), CASE WHEN g.age > 1 THEN 1 ELSE 0 END \
         FROM {MEMBERS} WHERE g.band = 'b1' LIMIT 1"
    );
    let prepared = prepare_sql(db, &sql, &[]).unwrap();
    assert_eq!(prepared.columns(), ["int8", "float8", "bool", "coalesce", "nullif", "case"]);
    // Anything else -- here, an arithmetic expression -- keeps PostgreSQL's
    // own catch-all.
    let sql = format!("SELECT g.age + 1 FROM {MEMBERS} WHERE g.band = 'b1' LIMIT 1");
    let prepared = prepare_sql(db, &sql, &[]).unwrap();
    assert_eq!(prepared.columns(), ["?column?"]);
    // A body RETURN keeps its own rule (a property by the property, a
    // variable by itself, anything else `?column?`), unaffected by the
    // outer SELECT's PostgreSQL rule: `count(*)` inside the body is still
    // `?column?` unaliased, never `count`.
    let inside = "SELECT * FROM GRAPH_TABLE (base MATCH (p IS person)-[:member_of]->(b IS band) \
         RETURN b._key AS band, count(*) GROUP BY b._key)";
    let prepared = prepare_sql(db, inside, &[]).unwrap();
    assert_eq!(prepared.columns(), ["band", "?column?"]);
}
