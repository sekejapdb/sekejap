//! A GQL statement on the PostgreSQL wire (M2-D of
//! `docs/lang/GQL_PROFILE_DESIGN.md`, §6.1 and §7), a byte at a time.
//!
//! What is at risk, and the test that pins it:
//!
//! * `Describe` answers the columns BEFORE any row exists, typed from the
//!   binding schema -- a declared text field `text`, a declared integer
//!   `int8`, a comparison `bool` -- and the rows that follow are encoded
//!   under those types, in text and in binary (`describe_*`, `binary_*`);
//! * a seed key no row holds is zero rows and `SELECT 0`, not an error
//!   (`describe_*`);
//! * an undeclared `$n` is described with the type its comparison gives it,
//!   and the statement is rebound with new values without being parsed
//!   again (`an_undeclared_parameter_*`);
//! * the simple protocol serves the same statement (`a_simple_query_*`);
//! * an `ARRAY_AGG` column (M3-B) is described as its array type and its
//!   cells are PostgreSQL arrays in text and in binary (`an_array_agg_*`).
//!
//! No socket is opened: every frame is built here from the protocol's own
//! layout and every reply parsed back the same way. Workload names are
//! invented: bands `b1` `b2`, people `p1` `p2` `p3`.

#[path = "common/mod.rs"]
mod common;

use kernel::store::Config;
use sekejap_core::collections::{Database, GraphContextId};
use sekejap_core::Kind;
use sekejap_dist::pg::{connection::Connection, types::oid};
use sekejap_dist::service::ServiceDatabase;
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;

use common::{columns, parse_message, query, sync_message, Frame};

// ── the fixture ──────────────────────────────────────────────────────────

fn config() -> Config {
    common::config()
}

struct Fixture {
    _dir: TempDir,
    service: ServiceDatabase,
}

/// ```text
/// band   b1 "band one", b2 "band two"
/// person p1 "person one" 41, p2 "person two" 25, p3 "person three" 33
/// member_of  p1->b1  p2->b1  p3->b2          (person.age is indexed)
/// ```
fn build() -> Fixture {
    let dir = TempDir::new().expect("a temp dir");
    let path = dir.path().join("db");
    {
        let mut db = Database::create(&path, config()).expect("create");
        let band = db
            .create_collection("band", vec![("name".into(), Kind::Text)], Default::default())
            .unwrap();
        let person = db
            .create_collection(
                "person",
                vec![("name".into(), Kind::Text), ("age".into(), Kind::Int)],
                Default::default(),
            )
            .unwrap();
        let b1 = db.put(band, "b1", &json!({"name": "band one"})).unwrap();
        let b2 = db.put(band, "b2", &json!({"name": "band two"})).unwrap();
        let mut people = Vec::new();
        for (key, name, age) in [
            ("p1", "person one", 41),
            ("p2", "person two", 25),
            ("p3", "person three", 33),
        ] {
            people.push(db.put(person, key, &json!({"name": name, "age": age})).unwrap());
        }
        let age = db.create_scalar_index(person, "person_age", "age", false).unwrap();
        while !db.build_index_step(age, 64).unwrap() {
            db.commit().unwrap();
        }
        db.enable_graph().unwrap();
        let member_of = db.create_edge_type("member_of").unwrap();
        for (from, to) in [(people[0], b1), (people[1], b1), (people[2], b2)] {
            db.create_edge(GraphContextId::BASE, from, member_of, to, &json!({}))
                .unwrap();
        }
        db.commit().expect("commit");
    }
    let service = ServiceDatabase::open(&path, config()).expect("open service");
    service.set_publish_interval(Duration::ZERO);
    Fixture { _dir: dir, service }
}

const MEMBERS: &str = "SELECT * FROM GRAPH_TABLE (base \
     MATCH (b IS band WHERE b._key = $1)<-[:member_of]-(p IS person) \
     RETURN p.name AS name, p.age AS age, p.age > 30 AS old)";

const BY_AGE: &str =
    "SELECT * FROM GRAPH_TABLE (base MATCH (p IS person) WHERE p.age = $1 RETURN p.name AS name)";

// ── frames this file builds, and the pieces it borrows from `common` ─────

/// A `Bind` with every parameter in text format, on the unnamed portal.
fn bind_message(statement: &str, params: &[&str], result_formats: &[i16]) -> Vec<u8> {
    let params: Vec<Option<&[u8]>> = params.iter().map(|p| Some(p.as_bytes())).collect();
    common::bind_message("", statement, &params, result_formats)
}

fn describe_statement(name: &str) -> Vec<u8> {
    common::describe_message(b'S', name)
}

fn execute_message() -> Vec<u8> {
    common::execute_message("", 0)
}

// ── frames this file reads back ──────────────────────────────────────────

fn frames(bytes: &[u8]) -> Vec<Frame> {
    common::frames(bytes)
}

fn types_of(frames: &[Frame]) -> String {
    common::types_of(frames).into_iter().collect()
}

fn first(frames: &[Frame], typ: u8) -> &Frame {
    common::first(frames, typ)
        .unwrap_or_else(|| panic!("no `{}` frame in {}", typ as char, types_of(frames)))
}

/// The OIDs of a `ParameterDescription`.
fn parameter_oids(frame: &Frame) -> Vec<i32> {
    let count = i16::from_be_bytes([frame.body[0], frame.body[1]]) as usize;
    (0..count)
        .map(|i| {
            let b = &frame.body[2 + 4 * i..6 + 4 * i];
            i32::from_be_bytes([b[0], b[1], b[2], b[3]])
        })
        .collect()
}

/// The raw cells of a `DataRow`. `None` is SQL NULL.
fn raw_cells(frame: &Frame) -> Vec<Option<Vec<u8>>> {
    let count = i16::from_be_bytes([frame.body[0], frame.body[1]]) as usize;
    let mut out = Vec::with_capacity(count);
    let mut at = 2usize;
    for _ in 0..count {
        let b = &frame.body[at..at + 4];
        let len = i32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        at += 4;
        if len < 0 {
            out.push(None);
            continue;
        }
        out.push(Some(frame.body[at..at + len as usize].to_vec()));
        at += len as usize;
    }
    out
}

/// Every `DataRow` as text cells, sorted: a pattern answer is a bag.
fn text_rows(frames: &[Frame]) -> Vec<Vec<Option<String>>> {
    let mut rows: Vec<_> = frames
        .iter()
        .filter(|f| f.typ == b'D')
        .map(|f| {
            raw_cells(f)
                .into_iter()
                .map(|cell| cell.map(|bytes| String::from_utf8(bytes).unwrap()))
                .collect::<Vec<_>>()
        })
        .collect();
    rows.sort();
    rows
}

fn tag(frames: &[Frame]) -> String {
    common::tag(frames)
}

fn connect(service: &ServiceDatabase) -> Connection<'_> {
    common::connect(service, common::key(7))
}

fn some(text: &str) -> Option<String> {
    Some(text.to_owned())
}

// ── the tests ────────────────────────────────────────────────────────────

#[test]
fn describe_types_the_columns_before_a_row_exists_and_a_missing_seed_is_zero_rows() {
    let fixture = build();
    let mut connection = connect(&fixture.service);
    let mut batch = parse_message("members", MEMBERS, &[]);
    batch.extend_from_slice(&describe_statement("members"));
    batch.extend_from_slice(&sync_message());
    let got = frames(&connection.feed(&batch));
    assert_eq!(types_of(&got), "1tTZ");
    assert_eq!(parameter_oids(first(&got, b't')), [oid::TEXT], "a `_key` is text");
    assert_eq!(
        columns(first(&got, b'T')),
        [
            ("name".to_owned(), oid::TEXT),
            ("age".to_owned(), oid::INT8),
            ("old".to_owned(), oid::BOOL),
        ]
    );

    let mut batch = bind_message("members", &["b1"], &[]);
    batch.extend_from_slice(&execute_message());
    batch.extend_from_slice(&sync_message());
    let got = frames(&connection.feed(&batch));
    assert_eq!(types_of(&got), "2DDCZ");
    assert_eq!(
        text_rows(&got),
        [
            [some("person one"), some("41"), some("t")],
            [some("person two"), some("25"), some("f")],
        ]
    );
    assert_eq!(tag(&got), "SELECT 2");

    // A key no band holds, and a key of ANOTHER collection: zero rows.
    for key in ["b9", "p1"] {
        let mut batch = bind_message("members", &[key], &[]);
        batch.extend_from_slice(&execute_message());
        batch.extend_from_slice(&sync_message());
        let got = frames(&connection.feed(&batch));
        assert_eq!(types_of(&got), "2CZ", "{key}");
        assert_eq!(tag(&got), "SELECT 0", "{key}");
    }
}

#[test]
fn binary_cells_are_encoded_under_the_described_types() {
    let fixture = build();
    let mut connection = connect(&fixture.service);
    let mut batch = parse_message("members", MEMBERS, &[oid::TEXT]);
    batch.extend_from_slice(&bind_message("members", &["b2"], &[1]));
    batch.extend_from_slice(&execute_message());
    batch.extend_from_slice(&sync_message());
    let got = frames(&connection.feed(&batch));
    assert_eq!(types_of(&got), "12DCZ");
    let cells = raw_cells(first(&got, b'D'));
    assert_eq!(cells[0].as_deref(), Some(&b"person three"[..]));
    assert_eq!(cells[1].as_deref(), Some(&33i64.to_be_bytes()[..]), "int8: 8 bytes");
    assert_eq!(cells[2].as_deref(), Some(&[1u8][..]), "bool: one byte");
}

#[test]
fn an_undeclared_parameter_takes_the_type_of_its_comparison_and_rebinds() {
    let fixture = build();
    let mut connection = connect(&fixture.service);
    let mut batch = parse_message("by_age", BY_AGE, &[]);
    batch.extend_from_slice(&describe_statement("by_age"));
    batch.extend_from_slice(&sync_message());
    let got = frames(&connection.feed(&batch));
    assert_eq!(types_of(&got), "1tTZ");
    assert_eq!(
        parameter_oids(first(&got, b't')),
        [oid::INT8],
        "`p.age` is a declared integer"
    );
    assert_eq!(columns(first(&got, b'T')), [("name".to_owned(), oid::TEXT)]);
    for (age, name) in [("25", "person two"), ("41", "person one"), ("99", "")] {
        let mut batch = bind_message("by_age", &[age], &[]);
        batch.extend_from_slice(&execute_message());
        batch.extend_from_slice(&sync_message());
        let got = frames(&connection.feed(&batch));
        let rows = text_rows(&got);
        if name.is_empty() {
            assert_eq!(types_of(&got), "2CZ", "no ParseComplete: compiled once");
            assert!(rows.is_empty());
        } else {
            assert_eq!(types_of(&got), "2DCZ", "no ParseComplete: compiled once");
            assert_eq!(rows, [[some(name)]], "each bind answers for ITS parameter");
        }
    }
}

#[test]
fn a_simple_query_serves_the_same_statement_with_the_same_types() {
    let fixture = build();
    let mut connection = connect(&fixture.service);
    let sql = MEMBERS.replace("$1", "'b1'");
    let got = frames(&connection.feed(&query(&sql)));
    assert_eq!(types_of(&got), "TDDCZ");
    assert_eq!(
        columns(first(&got, b'T')),
        [
            ("name".to_owned(), oid::TEXT),
            ("age".to_owned(), oid::INT8),
            ("old".to_owned(), oid::BOOL),
        ]
    );
    assert_eq!(text_rows(&got).len(), 2);
    // EXPLAIN of the same statement answers its plan as one text row.
    let got = frames(&connection.feed(&query(&format!("EXPLAIN {sql}"))));
    assert_eq!(types_of(&got), "TDCZ");
    let plan = text_rows(&got)[0][0].clone().unwrap();
    assert!(plan.contains("Seed b: key lookup of 'b1' in band"), "{plan}");
}

#[test]
fn an_array_agg_column_is_a_postgresql_array_in_text_and_binary() {
    // M3-B: a list column. `ARRAY_AGG` of a text property is `TEXT[]`, of a
    // bigint property `BIGINT[]`, described before any row exists and
    // written as PostgreSQL arrays: `array_out`'s quoting in text,
    // `array_send`'s one-dimensional layout in binary.
    let fixture = build();
    let mut connection = connect(&fixture.service);
    let sql = "SELECT * FROM GRAPH_TABLE (base \
         MATCH (b IS band WHERE b._key = $1)<-[:member_of]-(p IS person) \
         RETURN b._key AS band, ARRAY_AGG(p.name) AS names, ARRAY_AGG(p.age) AS ages \
         GROUP BY b._key)";
    let mut batch = parse_message("lists", sql, &[]);
    batch.extend_from_slice(&describe_statement("lists"));
    batch.extend_from_slice(&bind_message("lists", &["b1"], &[]));
    batch.extend_from_slice(&execute_message());
    batch.extend_from_slice(&sync_message());
    let got = frames(&connection.feed(&batch));
    assert_eq!(types_of(&got), "1tT2DCZ");
    assert_eq!(
        columns(first(&got, b'T')),
        [
            ("band".to_owned(), oid::TEXT),
            ("names".to_owned(), oid::TEXT_ARRAY),
            ("ages".to_owned(), oid::INT8_ARRAY),
        ]
    );
    assert_eq!(
        text_rows(&got),
        [[
            some("b1"),
            some(r#"{"person one","person two"}"#),
            some("{41,25}"),
        ]]
    );

    // Binary: ndim 1, no null, the element OID, (length, lower bound 1),
    // then each element length-prefixed.
    let mut batch = bind_message("lists", &["b1"], &[1]);
    batch.extend_from_slice(&execute_message());
    batch.extend_from_slice(&sync_message());
    let got = frames(&connection.feed(&batch));
    assert_eq!(types_of(&got), "2DCZ");
    let cells = raw_cells(first(&got, b'D'));
    let header = |element: i32, count: i32| {
        let mut out = Vec::new();
        for word in [1i32, 0, element, count, 1] {
            out.extend_from_slice(&word.to_be_bytes());
        }
        out
    };
    let mut names = header(oid::TEXT, 2);
    for name in ["person one", "person two"] {
        names.extend_from_slice(&(name.len() as i32).to_be_bytes());
        names.extend_from_slice(name.as_bytes());
    }
    assert_eq!(cells[1].as_deref(), Some(&names[..]));
    let mut ages = header(oid::INT8, 2);
    for age in [41i64, 25] {
        ages.extend_from_slice(&8i32.to_be_bytes());
        ages.extend_from_slice(&age.to_be_bytes());
    }
    assert_eq!(cells[2].as_deref(), Some(&ages[..]));

    // A band with no member: no group, zero rows, the same typed columns.
    let mut batch = bind_message("lists", &["b9"], &[]);
    batch.extend_from_slice(&execute_message());
    batch.extend_from_slice(&sync_message());
    let got = frames(&connection.feed(&batch));
    assert_eq!(tag(&got), "SELECT 0");
}

/// `(SQLSTATE, message)` of an `ErrorResponse`.
fn error_fields(frame: &Frame) -> (String, String) {
    let mut sqlstate = String::new();
    let mut message = String::new();
    let mut at = 0usize;
    while at < frame.body.len() && frame.body[at] != 0 {
        let code = frame.body[at];
        at += 1;
        let start = at;
        while at < frame.body.len() && frame.body[at] != 0 {
            at += 1;
        }
        let value = String::from_utf8_lossy(&frame.body[start..at]).into_owned();
        at += 1;
        match code {
            b'C' => sqlstate = value,
            b'M' => message = value,
            _ => {}
        }
    }
    (sqlstate, message)
}

#[test]
fn a_runtime_error_reaches_the_wire_with_its_postgresql_sqlstate() {
    // PostgreSQL's own code for each error an evaluation raises, never
    // `XX000 internal_error`, the one code a client retries.
    let fixture = build();
    let mut connection = connect(&fixture.service);
    for (expr, code) in [
        ("p.age / 0", "22012"),
        ("p.age % 0", "22012"),
        ("p.age * 4611686018427387904", "22003"),
        ("CAST('x' AS BIGINT)", "22P02"),
        ("ln(p.age - 41)", "2201E"),
        ("sqrt(-1)", "2201F"),
        ("p.name + 1", "42804"),
    ] {
        let sql = format!(
            "SELECT * FROM GRAPH_TABLE (base MATCH (p IS person WHERE p._key = 'p1') RETURN {expr} AS v)"
        );
        let got = frames(&connection.feed(&query(&sql)));
        assert_eq!(types_of(&got), "EZ", "`{expr}`");
        let (sqlstate, message) = error_fields(first(&got, b'E'));
        assert_eq!(sqlstate, code, "`{expr}`: {message}");
        assert!(!message.contains("SQLSTATE"), "the code travels as data: {message}");
    }
}

#[test]
fn a_vertical_sum_past_bigint_reaches_the_wire_as_numeric_value_out_of_range() {
    // Each row's term fits a BIGINT and the total of the three does not:
    // the engine's accumulator raises it, and it is `22003`, as the
    // horizontal SUM's overflow is, never `XX000`.
    let fixture = build();
    let mut connection = connect(&fixture.service);
    let sql = "SELECT * FROM GRAPH_TABLE (base MATCH (p IS person) \
               RETURN SUM(p.age + 4611686018427387000) AS total)";
    let got = frames(&connection.feed(&query(sql)));
    assert_eq!(types_of(&got), "EZ");
    let (sqlstate, message) = error_fields(first(&got, b'E'));
    assert_eq!(sqlstate, "22003", "{message}");
    assert!(!message.contains("SQLSTATE"), "the code travels as data: {message}");
}

#[test]
fn a_binder_error_reaches_the_wire_with_its_postgresql_sqlstate() {
    // An unknown variable is `undefined_column`, a value of the wrong kind
    // `datatype_mismatch`, an unknown function or a wrong argument count
    // `undefined_function` -- never `0A000`, which says the engine lacks a
    // feature.
    let fixture = build();
    let mut connection = connect(&fixture.service);
    for (ret, code) in [
        ("q.name AS v", "42703"),
        ("p + 1 AS v", "42804"),
        ("PATH_LENGTH(p) AS v", "42804"),
        ("lowr(p.name) AS v", "42883"),
        ("abs(p.age, 2) AS v", "42883"),
    ] {
        let sql = format!(
            "SELECT * FROM GRAPH_TABLE (base MATCH (p IS person WHERE p._key = 'p1') RETURN {ret})"
        );
        let got = frames(&connection.feed(&query(&sql)));
        assert_eq!(types_of(&got), "EZ", "`{ret}`");
        let (sqlstate, message) = error_fields(first(&got, b'E'));
        assert_eq!(sqlstate, code, "`{ret}`: {message}");
    }
}
