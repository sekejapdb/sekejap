//! The outer `SELECT` over a GQL relation and the statement's one
//! parameter table (M3-D of `docs/lang/GQL_PROFILE_DESIGN.md`, §5.5 and §7;
//! owner answer Q13).
//!
//! What is at risk, and the test that pins it:
//!
//! * a `$n` read inside the graph body and in the outer `SELECT` is ONE
//!   slot with ONE type, and a prepared statement rebinds it without being
//!   compiled again ("Prepared rebinding", `prepared_rebinding_*`);
//! * two uses that deduce two types are refused at prepare with
//!   PostgreSQL's `42P08`, naming both uses (`a_parameter_used_as_two_*`);
//! * a prepared statement resolves its seeds under each execution: a row
//!   deleted and reinserted between runs is found under its NEW identity,
//!   and a deleted row or edge is never walked ("Delete/reinsert between
//!   executions", `a_prepared_statement_stays_correct_*`);
//! * the outer `WHERE`, `GROUP BY`, `ORDER BY`, `OFFSET` and `LIMIT` read the
//!   relation's `RETURN` columns with PostgreSQL's meaning: `NULL`s last
//!   ascending, positional keys, no implicit grouping, both `LIMIT`/`OFFSET`
//!   orders, and the `LIMIT $n` range of Q13 (`the_outer_select_*`);
//! * `EXPLAIN` prints the outer SELECT as the plan's last stage
//!   (`explain_shows_the_outer_select_*`);
//! * what the outer SELECT does not build is refused by name
//!   (`what_the_outer_select_does_not_build_*`);
//! * a list parameter feeds `FOR x IN $n` and `x IN $n`, inside the body and
//!   outside it (`list_parameters_*`).
//!
//! Workload names are invented: bands `b1` `b2`, people `p1`-`p5`, songs
//! `s1`-`s3`.

use sekejap_core::collections::{CollectionId, Database, EdgeTypeId, EntityId, GraphContextId};
use sekejap_core::Kind;
use sekejap_lang::{explain_sql, prepare_sql, Param, PreparedSql, SqlError, SqlResult, SqlValue};
use serde_json::json;
use tempfile::TempDir;

mod common;
use common::cfg;

struct Fixture {
    _dir: TempDir,
    db: Database,
    person: CollectionId,
    song: CollectionId,
    wrote: EdgeTypeId,
}

/// ```text
/// band   b1 "band one", b2 "band two"
/// person p1 "person one" 41, p2 "person two" 25, p3 "person three" 33,
///        p4 "person four" (no age), p5 "person five" 30
/// song   s1 "song one", s2 "song two", s3 "song three"
///
/// member_of  p1->b1  p2->b1  p4->b1  p5->b1  p3->b2
/// wrote      p1->s1  p1->s2  p2->s2  p3->s3
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
    let song = db
        .create_collection("song", vec![text("name")], Default::default())
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
    let s: Vec<EntityId> = ["one", "two", "three"]
        .iter()
        .enumerate()
        .map(|(i, name)| {
            db.put(song, &format!("s{}", i + 1), &json!({"name": format!("song {name}")}))
                .unwrap()
        })
        .collect();
    db.enable_graph().unwrap();
    let member_of = db.create_edge_type("member_of").unwrap();
    let wrote = db.create_edge_type("wrote").unwrap();
    let base = GraphContextId::BASE;
    for (from, edge_type, to) in [
        (p[0], member_of, b1),
        (p[1], member_of, b1),
        (p[3], member_of, b1),
        (p[4], member_of, b1),
        (p[2], member_of, b2),
        (p[0], wrote, s[0]),
        (p[0], wrote, s[1]),
        (p[1], wrote, s[1]),
        (p[2], wrote, s[2]),
    ] {
        db.create_edge(base, from, edge_type, to, &json!({})).unwrap();
    }
    db.commit().unwrap();
    Fixture {
        _dir: dir,
        db,
        person,
        song,
        wrote,
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

/// Prepare and run `sql` once.
fn query(db: &Database, sql: &str, params: &[Param]) -> Result<Vec<Vec<SqlValue>>, SqlError> {
    rows_of(db, &prepare_sql(db, sql, params)?)
}

fn rows(db: &Database, sql: &str, params: &[Param]) -> Vec<Vec<SqlValue>> {
    query(db, sql, params).unwrap_or_else(|error| panic!("`{sql}`: {error}"))
}

/// The SQLSTATE a coded error carries.
fn sqlstate(error: &SqlError) -> &'static str {
    match error {
        SqlError::Coded { sqlstate, .. } => sqlstate,
        other => panic!("not a coded error: {other:?}"),
    }
}

/// Every member of a band with an age, as the relation the outer SELECTs
/// below read.
const MEMBERS: &str = "GRAPH_TABLE (base \
     MATCH (p IS person)-[:member_of]->(b IS band) \
     RETURN b._key AS band, p._key AS person, p.age AS age) AS g";

// ── Prepared rebinding ─────────────────────────────────────────────────────

#[test]
fn prepared_rebinding_gives_one_slot_one_type_inside_and_outside_the_graph() {
    let f = fixture();
    // `$2` is read inside the pattern and by the outer WHERE: one slot, one
    // type, BIGINT from both `p.age` and the column `g.age` that carries it.
    let sql = "SELECT g.name, g.age FROM GRAPH_TABLE (base \
         MATCH (b IS band WHERE b._key = $1)<-[:member_of]-(p IS person WHERE p.age >= $2) \
         RETURN p.name AS name, p.age AS age) AS g \
         WHERE g.age <> $2 ORDER BY g.age DESC LIMIT $3";
    let mut prepared = prepare_sql(
        &f.db,
        sql,
        &[Param::Text("b1".into()), Param::Int(25), Param::Int(5)],
    )
    .unwrap();
    assert_eq!(
        prepared.param_types(),
        [Some("TEXT"), Some("BIGINT"), Some("BIGINT")]
    );
    assert!(prepared.rebindable(), "a GQL plan folds no parameter value");
    assert_eq!(prepared.columns(), ["name", "age"]);
    assert_eq!(prepared.column_type(1), Some("BIGINT"));
    assert_eq!(
        rows_of(&f.db, &prepared).unwrap(),
        [
            vec![text("person one"), SqlValue::Int(41)],
            vec![text("person five"), SqlValue::Int(30)],
        ]
    );
    for (params, expected) in [
        (
            [Param::Text("b1".into()), Param::Int(30), Param::Int(5)],
            vec![vec![text("person one"), SqlValue::Int(41)]],
        ),
        (
            [Param::Text("b2".into()), Param::Int(0), Param::Int(1)],
            vec![vec![text("person three"), SqlValue::Int(33)]],
        ),
        (
            [Param::Text("b1".into()), Param::Int(0), Param::Int(1)],
            vec![vec![text("person one"), SqlValue::Int(41)]],
        ),
        (
            [Param::Text("b9".into()), Param::Int(0), Param::Int(1)],
            vec![],
        ),
    ] {
        prepared.bind(&f.db, &params).unwrap();
        assert!(prepared.rebindable());
        assert_eq!(rows_of(&f.db, &prepared).unwrap(), expected, "{params:?}");
    }
    // A value of another kind than the slot's type is refused when the
    // execution opens, naming the slot -- never compared as text.
    prepared
        .bind(
            &f.db,
            &[Param::Text("b1".into()), Param::Text("25".into()), Param::Int(5)],
        )
        .unwrap();
    match rows_of(&f.db, &prepared) {
        Err(SqlError::Parameter(message)) => {
            assert!(message.contains("$2") && message.contains("BIGINT"), "{message}")
        }
        other => panic!("a text value in a BIGINT slot: {other:?}"),
    }
    // NULL is a value of every type.
    prepared
        .bind(&f.db, &[Param::Text("b1".into()), Param::Null, Param::Int(5)])
        .unwrap();
    assert_eq!(rows_of(&f.db, &prepared).unwrap(), Vec::<Vec<SqlValue>>::new());
}

#[test]
fn a_parameter_used_as_two_types_is_refused_at_prepare_naming_both_uses() {
    let f = fixture();
    for (sql, first, second) in [
        // A text key inside, a number outside.
        (
            "SELECT g.age FROM GRAPH_TABLE (base MATCH (p IS person WHERE p._key = $1) \
             RETURN p.age AS age) AS g WHERE g.age = $1"
                .to_owned(),
            "TEXT",
            "BIGINT",
        ),
        // A text key inside, a count outside.
        (
            "SELECT g.age FROM GRAPH_TABLE (base MATCH (p IS person WHERE p._key = $1) \
             RETURN p.age AS age) AS g LIMIT $1"
                .to_owned(),
            "TEXT",
            "BIGINT",
        ),
        // Both inside the body.
        (
            "SELECT * FROM GRAPH_TABLE (base MATCH (b IS band WHERE b._key = $1)<-[:member_of]-(p IS person) \
             WHERE p.age = $1 RETURN p.name AS name)"
                .to_owned(),
            "TEXT",
            "BIGINT",
        ),
        // A list in a FOR, a number outside.
        (
            "SELECT g.age FROM GRAPH_TABLE (base FOR k IN $1 MATCH (p IS person WHERE p._key = k) \
             RETURN p.age AS age) AS g WHERE g.age > $1"
                .to_owned(),
            "TEXT[]",
            "BIGINT",
        ),
    ] {
        let error = prepare_sql(&f.db, &sql, &[]).err().unwrap_or_else(|| panic!("`{sql}` prepared"));
        assert_eq!(sqlstate(&error), "42P08", "`{sql}`: {error}");
        let message = error.to_string();
        assert!(message.contains("$1"), "{message}");
        assert!(
            message.contains(first) && message.contains(second),
            "`{sql}` names neither type: {message}"
        );
    }
    // An integer and a float agree, as PostgreSQL's numeric types do, and
    // an undecided use (an operand of arithmetic) takes the decided type.
    let sql = "SELECT g.person FROM GRAPH_TABLE (base MATCH (p IS person) \
         RETURN p._key AS person, p.age AS age, p.age * 1.5 AS weighted) AS g \
         WHERE g.age > $1 AND g.weighted > $1 AND g.age < $1 + 20 ORDER BY g.person";
    let prepared = prepare_sql(&f.db, sql, &[Param::Int(30)]).unwrap();
    assert_eq!(prepared.param_types(), [Some("DOUBLE PRECISION")]);
    assert_eq!(rows_of(&f.db, &prepared).unwrap(), [[text("p1")], [text("p3")]]);
}

// ── Delete/reinsert between executions ─────────────────────────────────────

#[test]
fn a_prepared_statement_stays_correct_when_rows_and_edges_are_deleted_and_reinserted() {
    let mut f = fixture();
    let sql = "SELECT g.song FROM GRAPH_TABLE (base \
         MATCH (p IS person WHERE p._key = $1)-[:wrote]->(s IS song) \
         RETURN s.name AS song) AS g ORDER BY g.song";
    let mut prepared = prepare_sql(&f.db, sql, &[Param::Text("p1".into())]).unwrap();
    assert_eq!(
        rows_of(&f.db, &prepared).unwrap(),
        [[text("song one")], [text("song two")]]
    );

    // The person goes, and its edges with it: no row, and no error.
    let old = f.db.get(f.person, "p1").unwrap().expect("p1 is stored").id;
    assert!(f.db.delete(f.person, "p1").unwrap());
    f.db.commit().unwrap();
    assert_eq!(rows_of(&f.db, &prepared).unwrap(), Vec::<Vec<SqlValue>>::new());

    // Reinserted under the same key: a NEW identity, found by the same
    // prepared statement, with only the edge written since.
    let new = f
        .db
        .put(f.person, "p1", &json!({"name": "person one", "age": 41}))
        .unwrap();
    assert_ne!(old, new, "a reinserted row is a new identity");
    let s3 = f.db.get(f.song, "s3").unwrap().expect("s3 is stored").id;
    f.db.create_edge(GraphContextId::BASE, new, f.wrote, s3, &json!({}))
        .unwrap();
    f.db.commit().unwrap();
    assert_eq!(rows_of(&f.db, &prepared).unwrap(), [[text("song three")]]);

    // The far node goes and comes back without its edge: the edge went with
    // the old identity, so nothing is reached.
    assert!(f.db.delete(f.song, "s3").unwrap());
    f.db.put(f.song, "s3", &json!({"name": "song three"})).unwrap();
    f.db.commit().unwrap();
    assert_eq!(rows_of(&f.db, &prepared).unwrap(), Vec::<Vec<SqlValue>>::new());

    // A rebind to another key still answers from the live snapshot.
    prepared.bind(&f.db, &[Param::Text("p2".into())]).unwrap();
    assert_eq!(rows_of(&f.db, &prepared).unwrap(), [[text("song two")]]);
    assert!(prepared.rebindable());
}

// ── the outer SELECT ───────────────────────────────────────────────────────

#[test]
fn the_outer_select_filters_orders_and_pages_the_relation_as_postgresql_does() {
    let f = fixture();
    let db = &f.db;
    let int = SqlValue::Int;
    // WHERE and a descending key over RETURN columns.
    assert_eq!(
        rows(db, &format!("SELECT g.person, g.age FROM {MEMBERS} WHERE g.age > 26 ORDER BY g.age DESC"), &[]),
        [
            vec![text("p1"), int(41)],
            vec![text("p3"), int(33)],
            vec![text("p5"), int(30)],
        ]
    );
    // NULL last ascending and first descending, the second key breaking
    // ties; a column may be named without the alias.
    assert_eq!(
        rows(db, &format!("SELECT person FROM {MEMBERS} ORDER BY age, person"), &[]),
        [[text("p2")], [text("p5")], [text("p3")], [text("p1")], [text("p4")]]
    );
    assert_eq!(
        rows(db, &format!("SELECT person FROM {MEMBERS} ORDER BY age DESC LIMIT 2"), &[]),
        [[text("p4")], [text("p1")]]
    );
    // LIMIT and OFFSET in either order, as PostgreSQL reads them.
    for tail in ["LIMIT 2 OFFSET 1", "OFFSET 1 LIMIT 2"] {
        assert_eq!(
            rows(db, &format!("SELECT g.person FROM {MEMBERS} ORDER BY g.person {tail}"), &[]),
            [[text("p2")], [text("p3")]],
            "{tail}"
        );
    }
    // A key by position is the output column at that position.
    assert_eq!(
        rows(db, &format!("SELECT g.person, g.age FROM {MEMBERS} ORDER BY 2 DESC, 1 LIMIT 2"), &[]),
        [vec![text("p4"), SqlValue::Null], vec![text("p1"), int(41)]]
    );
    // A WHERE on the relation keeps the rows it is true for.
    assert_eq!(
        rows(db, &format!("SELECT g.person FROM {MEMBERS} WHERE g.band = 'b2'"), &[]),
        [[text("p3")]]
    );
    assert_eq!(
        rows(db, &format!("SELECT person FROM {MEMBERS} WHERE age IS NULL"), &[]),
        [[text("p4")]]
    );
    // GROUP BY with aggregates, typed as PostgreSQL types them.
    let grouped = format!(
        "SELECT g.band, count(*) AS n, max(g.age) AS oldest FROM {MEMBERS} GROUP BY g.band ORDER BY g.band"
    );
    assert_eq!(
        rows(db, &grouped, &[]),
        [
            vec![text("b1"), int(4), int(41)],
            vec![text("b2"), int(1), int(33)],
        ]
    );
    let prepared = prepare_sql(db, &grouped, &[]).unwrap();
    assert_eq!(prepared.columns(), ["band", "n", "oldest"]);
    assert_eq!(prepared.column_type(1), Some("BIGINT"));
    // An aggregate without GROUP BY folds the whole relation into one row.
    assert_eq!(
        rows(db, &format!("SELECT count(*) AS n, avg(g.age) AS mean FROM {MEMBERS}"), &[]),
        [vec![int(5), SqlValue::Float(32.25)]]
    );
    // No implicit grouping: a column beside an aggregate must be grouped,
    // PostgreSQL's 42803.
    let error = query(db, &format!("SELECT g.band, count(*) AS n FROM {MEMBERS}"), &[]).unwrap_err();
    assert_eq!(sqlstate(&error), "42803", "{error}");
    // A position past the select list is PostgreSQL's 42P10.
    let error = query(db, &format!("SELECT g.person FROM {MEMBERS} ORDER BY 2"), &[]).unwrap_err();
    assert_eq!(sqlstate(&error), "42P10", "{error}");
    // A column the relation does not return is PostgreSQL's 42703.
    let error = query(db, &format!("SELECT g.name FROM {MEMBERS}"), &[]).unwrap_err();
    assert_eq!(sqlstate(&error), "42703", "{error}");
}

#[test]
fn the_outer_limit_and_offset_parameters_are_counts_from_zero_to_i64_max() {
    let f = fixture();
    let sql = format!("SELECT g.person FROM {MEMBERS} ORDER BY g.person OFFSET $1 LIMIT $2");
    let mut prepared = prepare_sql(&f.db, &sql, &[Param::Int(0), Param::Int(i64::MAX)]).unwrap();
    assert_eq!(prepared.param_types(), [Some("BIGINT"), Some("BIGINT")]);
    assert_eq!(rows_of(&f.db, &prepared).unwrap().len(), 5);
    prepared.bind(&f.db, &[Param::Int(4), Param::Int(0)]).unwrap();
    assert_eq!(rows_of(&f.db, &prepared).unwrap(), Vec::<Vec<SqlValue>>::new());
    prepared.bind(&f.db, &[Param::Int(4), Param::Int(9)]).unwrap();
    assert_eq!(rows_of(&f.db, &prepared).unwrap(), [[text("p5")]]);
    for bad in [Param::Int(-1), Param::Float(1.0), Param::Text("1".into()), Param::Null] {
        prepared.bind(&f.db, &[Param::Int(0), bad.clone()]).unwrap();
        match rows_of(&f.db, &prepared) {
            Err(SqlError::Parameter(message)) => assert!(message.contains("$2"), "{message}"),
            other => panic!("LIMIT {bad:?}: {other:?}"),
        }
    }
}

#[test]
fn explain_shows_the_outer_select_as_the_last_stage_of_one_plan() {
    let f = fixture();
    let plan = explain_sql(
        &f.db,
        &format!("SELECT g.person FROM {MEMBERS} WHERE g.age > 26 ORDER BY g.age DESC LIMIT 2"),
        &[],
    )
    .unwrap();
    for expected in [
        "2 stages",
        // The normal form: `g.person` is the relation's column `person`.
        "statement: SELECT person FROM GRAPH_TABLE (base MATCH",
        "AS g WHERE (age > 26) ORDER BY age DESC LIMIT 2\n",
        "outer SELECT: reads the rows stage 1 returned, as the relation's columns",
        "Filter as the outer WHERE, over the relation's rows (not pushed into the search)",
        "Sort by",
        "Page LIMIT 2",
        "columns: person TEXT",
        "rows: 2 in",
    ] {
        assert!(plan.contains(expected), "no `{expected}` in\n{plan}");
    }
    // The sort key the outer SELECT does not return is a hidden column.
    assert!(plan.contains("returned by stage 1"), "{plan}");
    // A bare `SELECT *` has no outer stage.
    let bare = explain_sql(&f.db, &format!("SELECT * FROM {MEMBERS}"), &[]).unwrap();
    assert!(bare.contains(", 1 stage\n"), "{bare}");
    assert!(!bare.contains("outer SELECT"), "{bare}");
}

#[test]
fn what_the_outer_select_does_not_build_is_refused_by_name() {
    let f = fixture();
    let db = &f.db;
    // A join with a GQL relation (QL_CONTRACT §4.8).
    for tail in ["JOIN band ON band._key = g.band", ", band"] {
        match prepare_sql(db, &format!("SELECT g.person FROM {MEMBERS} {tail}"), &[]) {
            Err(SqlError::Refused { keyword, .. }) => assert_eq!(keyword, "JOIN", "{tail}"),
            other => panic!("`{tail}`: {:?}", other.err()),
        }
    }
    // `SELECT DISTINCT ON (...)` stays refused by name (M3-D2, brief gap 1):
    // plain `SELECT DISTINCT` and `HAVING` are now built, and
    // `lang/tests/gql_outer.rs` covers them.
    match prepare_sql(db, &format!("SELECT DISTINCT ON (g.band) g.band FROM {MEMBERS}"), &[]) {
        Err(SqlError::Refused { keyword, .. }) => assert_eq!(keyword, "DISTINCT ON"),
        other => panic!("{:?}", other.err()),
    }
    // A relation's column is a value: a node does not become one.
    let error = prepare_sql(
        db,
        "SELECT g.p FROM GRAPH_TABLE (base MATCH (p IS person) RETURN p AS p) AS g",
        &[],
    )
    .err()
    .expect("a node column is refused");
    assert!(error.to_string().contains("node"), "{error}");
}

// ── list parameters ────────────────────────────────────────────────────────

#[test]
fn list_parameters_feed_for_and_in_inside_the_body_and_outside_it() {
    let f = fixture();
    let db = &f.db;
    let keys = |keys: &[&str]| Param::Json(json!(keys));
    // `FOR k IN $1`: one input row per element, duplicates kept.
    let sql = "SELECT g.age FROM GRAPH_TABLE (base FOR k IN $1 \
         MATCH (p IS person WHERE p._key = k) RETURN p.age AS age) AS g ORDER BY g.age DESC";
    let prepared = prepare_sql(db, sql, &[keys(&["p2", "p1", "p2"])]).unwrap();
    assert_eq!(prepared.param_types(), [Some("TEXT[]")]);
    assert_eq!(
        rows_of(db, &prepared).unwrap(),
        [[SqlValue::Int(41)], [SqlValue::Int(25)], [SqlValue::Int(25)]]
    );
    // `x IN $1` inside the body, typed as the list of what it tests.
    let sql = "SELECT * FROM GRAPH_TABLE (base \
         MATCH (b IS band WHERE b._key = $2)<-[:member_of]-(p IS person) \
         WHERE p._key IN $1 RETURN p._key AS person ORDER BY person)";
    let prepared = prepare_sql(db, sql, &[keys(&["p1", "p5", "p3"]), Param::Text("b1".into())]).unwrap();
    assert_eq!(prepared.param_types(), [Some("TEXT[]"), Some("TEXT")]);
    assert_eq!(rows_of(db, &prepared).unwrap(), [[text("p1")], [text("p5")]]);
    // ... and in the outer WHERE, negated too, over an integer column.
    let sql = format!("SELECT g.person FROM {MEMBERS} WHERE g.age IN $1 ORDER BY g.person");
    let mut prepared = prepare_sql(db, &sql, &[Param::Json(json!([25, 41]))]).unwrap();
    assert_eq!(prepared.param_types(), [Some("BIGINT[]")]);
    assert_eq!(rows_of(db, &prepared).unwrap(), [[text("p1")], [text("p2")]]);
    // Membership in no element is false even for NULL (`= ANY('{}')`), and
    // a NULL list is unknown, so no row.
    prepared.bind(db, &[Param::Json(json!([]))]).unwrap();
    assert_eq!(rows_of(db, &prepared).unwrap(), Vec::<Vec<SqlValue>>::new());
    let sql = format!("SELECT g.person FROM {MEMBERS} WHERE g.age NOT IN $1 ORDER BY g.person");
    assert_eq!(
        rows(db, &sql, &[Param::Json(json!([]))]),
        [[text("p1")], [text("p2")], [text("p3")], [text("p4")], [text("p5")]]
    );
    assert_eq!(
        rows(db, &sql, &[Param::Json(json!([25, 41]))]),
        [[text("p3")], [text("p5")]]
    );
    assert_eq!(rows(db, &sql, &[Param::Null]), Vec::<Vec<SqlValue>>::new());
    // A vector is not a list (brief §7), nor is a scalar.
    for bad in [Param::Vector(vec![25.0]), Param::Int(25)] {
        match query(db, &sql, &[bad.clone()]) {
            Err(SqlError::Parameter(message)) => assert!(message.contains("$1"), "{message}"),
            other => panic!("{bad:?}: {other:?}"),
        }
    }
}
