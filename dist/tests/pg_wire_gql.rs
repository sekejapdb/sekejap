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
//!   cells are PostgreSQL arrays in text and in binary (`an_array_agg_*`);
//! * (M3-E) the slices of a portal with a row limit, and the `FETCH`es of a
//!   declared cursor, concatenate to the answer the simple protocol streams,
//!   for path searches, `OPTIONAL MATCH`, a grouped `RETURN` and an outer
//!   `ORDER BY` + `LIMIT` (`a_portal_with_a_row_limit_*`,
//!   `a_declared_cursor_*`);
//! * (M3-E) `CancelRequest` and `statement_timeout` stop a GQL walk with
//!   `57014` and the connection answers the next statement
//!   (`a_cancel_request_*`, `a_statement_timeout_*`), and a portal refused
//!   that way stays refused (`a_portal_refused_part_way_*`).
//! * an unaliased outer SELECT column is named as PostgreSQL names one
//!   (M3-D2), in RowDescription before any row exists
//!   (`an_outer_selects_unaliased_columns_are_named_*`).
//!
//! No socket is opened: every frame is built here from the protocol's own
//! layout and every reply parsed back the same way. Workload names are
//! invented: bands `b1` `b2`, people `p1` `p2` `p3` and `p00`-`p29`.

#[path = "common/mod.rs"]
mod common;

use kernel::store::Config;
use sekejap_core::collections::{Database, GraphContextId};
use sekejap_core::Kind;
use sekejap_dist::pg::{
    connection::{CancelToken, Connection},
    types::oid,
};
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

#[test]
fn an_outer_select_is_described_with_its_parameter_types_and_orders_and_limits() {
    // M3-D: one parameter table for the body and the outer SELECT. `$2` is
    // read inside the pattern and by the outer WHERE, `$3` is the outer
    // LIMIT: the ParameterDescription carries each one's real type, and the
    // rows come back in the outer ORDER BY's order, cut by its LIMIT.
    let fixture = build();
    let mut connection = connect(&fixture.service);
    let sql = "SELECT g.name, g.age FROM GRAPH_TABLE (base \
         MATCH (b IS band WHERE b._key = $1)<-[:member_of]-(p IS person WHERE p.age > $2) \
         RETURN p.name AS name, p.age AS age) AS g \
         WHERE g.age <> $2 ORDER BY g.age DESC LIMIT $3";
    let mut batch = parse_message("outer", sql, &[]);
    batch.extend_from_slice(&describe_statement("outer"));
    batch.extend_from_slice(&sync_message());
    let got = frames(&connection.feed(&batch));
    assert_eq!(types_of(&got), "1tTZ");
    assert_eq!(
        parameter_oids(first(&got, b't')),
        [oid::TEXT, oid::INT8, oid::INT8]
    );
    assert_eq!(
        columns(first(&got, b'T')),
        [("name".to_owned(), oid::TEXT), ("age".to_owned(), oid::INT8)]
    );
    // In the order the rows arrive: not sorted here.
    let ordered = |frames: &[Frame]| -> Vec<Vec<Option<String>>> {
        frames
            .iter()
            .filter(|f| f.typ == b'D')
            .map(|f| {
                raw_cells(f)
                    .into_iter()
                    .map(|cell| cell.map(|bytes| String::from_utf8(bytes).unwrap()))
                    .collect()
            })
            .collect()
    };
    for (params, expected) in [
        (
            ["b1", "0", "5"],
            vec![
                vec![some("person one"), some("41")],
                vec![some("person two"), some("25")],
            ],
        ),
        (["b1", "0", "1"], vec![vec![some("person one"), some("41")]]),
        (["b1", "41", "5"], vec![]),
    ] {
        let mut batch = bind_message("outer", &params, &[]);
        batch.extend_from_slice(&execute_message());
        batch.extend_from_slice(&sync_message());
        let got = frames(&connection.feed(&batch));
        assert_eq!(ordered(&got), expected, "{params:?}");
        assert_eq!(tag(&got), format!("SELECT {}", expected.len()));
    }
    // A LIMIT below zero is refused with the parameter's code.
    let mut batch = bind_message("outer", &["b1", "0", "-1"], &[]);
    batch.extend_from_slice(&execute_message());
    batch.extend_from_slice(&sync_message());
    let got = frames(&connection.feed(&batch));
    let (sqlstate, message) = error_fields(first(&got, b'E'));
    assert_eq!(sqlstate, "22023", "{message}");
}

#[test]
fn an_outer_selects_unaliased_columns_are_named_as_postgresql_names_them() {
    // M3-D2, brief gap 3: an unaliased outer SELECT column is named as
    // PostgreSQL names one -- an aggregate by its function, a function
    // call by its function name, a cast by its declared type, and
    // anything else `?column?` -- described in RowDescription before any
    // row exists, exactly like every other column type.
    let fixture = build();
    let mut connection = connect(&fixture.service);
    let sql = "SELECT count(*), avg(g.age), g.age::text, LOWER(g.name), g.age + 1 \
         FROM GRAPH_TABLE (base \
         MATCH (b IS band WHERE b._key = $1)<-[:member_of]-(p IS person) \
         RETURN p.name AS name, p.age AS age) AS g \
         GROUP BY g.age::text, LOWER(g.name), g.age + 1";
    let mut batch = parse_message("named", sql, &[]);
    batch.extend_from_slice(&describe_statement("named"));
    batch.extend_from_slice(&sync_message());
    let got = frames(&connection.feed(&batch));
    assert_eq!(types_of(&got), "1tTZ");
    let names: Vec<String> = columns(first(&got, b'T')).into_iter().map(|(name, _)| name).collect();
    assert_eq!(names, ["count", "avg", "text", "lower", "?column?"]);
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

// ── failed transaction block, and duplicate cursor names (M3-W) ─────────
//
// Where behaviour is not graph-specific, this surface matches PostgreSQL: a
// GQL statement is a statement like any other to `BEGIN`'s block state and
// to `DECLARE`'s cursor names, both of `docs/dist/WIRE_CONTRACT.md` §1.3/§6.

/// A GQL read issued after an earlier statement in the same `BEGIN` block
/// failed is refused `25P02`, not answered -- the same rule §1.3 states for
/// any statement, proved here over the graph path rather than the plain-SQL
/// one `dist/tests/pg_wire.rs` proves it over.
#[test]
fn a_gql_read_in_a_failed_transaction_block_is_refused_25p02() {
    let fixture = build();
    let mut connection = connect(&fixture.service);
    let sql = MEMBERS.replace("$1", "'b1'");

    let _ = connection.feed(&query("BEGIN"));
    let _ = connection.feed(&query("SELECT id FROM nowhere")); // aborts the block

    let got = frames(&connection.feed(&query(&sql)));
    assert_eq!(types_of(&got), "EZ", "the GQL read was answered instead of refused");
    let (sqlstate, message) = error_fields(first(&got, b'E'));
    assert_eq!(sqlstate, "25P02", "{message}");

    let _ = connection.feed(&query("ROLLBACK"));
    let got = frames(&connection.feed(&query(&sql)));
    assert_eq!(types_of(&got), "TDDCZ", "after ROLLBACK the same GQL statement answers again");
}

/// `DECLARE <name> CURSOR FOR` a GQL statement, when `<name>` is already
/// open, is `42P03 duplicate_cursor` rather than a silent replace, and the
/// original cursor keeps its own answer -- the same rule proved over the
/// plain-SQL path in `dist/tests/pg_wire.rs`.
#[test]
fn declaring_a_gql_cursor_with_a_name_already_open_is_refused_and_leaves_it_untouched() {
    let fixture = build();
    let mut connection = connect(&fixture.service);
    let members_b1 = MEMBERS.replace("$1", "'b1'");
    let members_b2 = MEMBERS.replace("$1", "'b2'");

    let got = frames(&connection.feed(&query(&format!("DECLARE g CURSOR FOR {members_b1}"))));
    assert_eq!(tag(&got), "DECLARE CURSOR");

    // A second DECLARE of the same name, over a DIFFERENT seed, is refused.
    let got = frames(&connection.feed(&query(&format!("DECLARE g CURSOR FOR {members_b2}"))));
    let (sqlstate, message) = error_fields(first(&got, b'E'));
    assert_eq!(sqlstate, "42P03");
    assert!(message.contains("\"g\""), "{message}");

    // The original cursor still answers band b1's two members, not b2's one.
    let got = frames(&connection.feed(&query("FETCH ALL FROM g")));
    assert_eq!(
        text_rows(&got),
        [
            [some("person one"), some("41"), some("t")],
            [some("person two"), some("25"), some("f")],
        ]
    );
}

// ── paging and cancellation on the wire (M3-E) ───────────────────────────
//
// A portal given a row limit and a declared cursor both page ONE execution
// (`dist/src/pg/connection.rs`, "What a suspended portal holds"): the
// slices a client reads, concatenated, are the answer the simple protocol
// streams, in the same order. A portal whose execution was refused stays
// refused -- the next `Execute` is an error, never rows or a completion --
// and `CancelRequest` and `statement_timeout` stop a GQL walk with `57014`
// and leave the connection usable.

/// ```text
/// person p00..p29 (name, age 20 + i)
/// knows  p_i -> p_(i+1),  p_i -> p_(i+2),  p29 -> p00
/// ```
fn build_chain() -> Fixture {
    let dir = TempDir::new().expect("a temp dir");
    let path = dir.path().join("db");
    {
        let mut db = Database::create(&path, config()).expect("create");
        let person = db
            .create_collection(
                "person",
                vec![("name".into(), Kind::Text), ("age".into(), Kind::Int)],
                Default::default(),
            )
            .unwrap();
        let people: Vec<_> = (0..30)
            .map(|i| {
                db.put(
                    person,
                    &format!("p{i:02}"),
                    &json!({"name": format!("person {i}"), "age": 20 + i}),
                )
                .unwrap()
            })
            .collect();
        db.enable_graph().unwrap();
        let knows = db.create_edge_type("knows").unwrap();
        for i in 0..30 {
            for step in [1, 2] {
                if i + step < 30 {
                    db.create_edge(GraphContextId::BASE, people[i], knows, people[i + step], &json!({}))
                        .unwrap();
                }
            }
        }
        db.create_edge(GraphContextId::BASE, people[29], knows, people[0], &json!({}))
            .unwrap();
        db.commit().expect("commit");
    }
    let service = ServiceDatabase::open(&path, config()).expect("open service");
    service.set_publish_interval(Duration::ZERO);
    Fixture { _dir: dir, service }
}

/// GQL statements whose answers span several slices of [`SLICE`] rows.
const PAGED: [&str; 5] = [
    "SELECT * FROM GRAPH_TABLE (base MATCH p = (s IS person WHERE s._key = 'p00')\
     -[:knows]->{1,5}(t) RETURN t._key AS t, PATH_LENGTH(p) AS n)",
    "SELECT * FROM GRAPH_TABLE (base MATCH p = ANY SHORTEST (s IS person WHERE s._key = 'p00')\
     -[:knows]->{1,20}(t) RETURN t._key AS t, PATH_LENGTH(p) AS n)",
    "SELECT * FROM GRAPH_TABLE (base MATCH (a IS person) \
     OPTIONAL MATCH (a)-[:knows]->(b IS person WHERE b.age > 40) RETURN a._key AS a, b._key AS b)",
    "SELECT * FROM GRAPH_TABLE (base MATCH (a IS person)-[:knows]->{1,3}(t) \
     RETURN a._key AS a, COUNT(*) AS n)",
    "SELECT t, n FROM GRAPH_TABLE (base MATCH p = (s IS person WHERE s._key = 'p00')\
     -[:knows]->{1,5}(t) RETURN t._key AS t, PATH_LENGTH(p) AS n) AS g \
     ORDER BY n DESC, t LIMIT 40",
];

/// Rows one `Execute` or `FETCH` asks for.
const SLICE: usize = 4;

/// A GQL walk long enough to outrun a one-millisecond timeout and to reach
/// the deadline's poll: every path of up to ten hops from every person.
const LONG_WALK: &str = "SELECT * FROM GRAPH_TABLE (base MATCH (s IS person)-[:knows]->{1,10}(t) \
                         RETURN COUNT(*) AS n)";

/// Every `DataRow` as text cells, in the order they arrived.
fn arrived_rows(frames: &[Frame]) -> Vec<Vec<Option<String>>> {
    frames
        .iter()
        .filter(|f| f.typ == b'D')
        .map(|f| {
            raw_cells(f)
                .into_iter()
                .map(|cell| cell.map(|bytes| String::from_utf8(bytes).unwrap()))
                .collect()
        })
        .collect()
}

/// The simple protocol's answer: one streamed execution, in order.
fn streamed(connection: &mut Connection<'_>, sql: &str) -> Vec<Vec<Option<String>>> {
    let got = frames(&connection.feed(&query(sql)));
    assert_eq!(types_of(&got).chars().next(), Some('T'), "`{sql}`: {}", types_of(&got));
    arrived_rows(&got)
}

/// One `Execute(portal, max_rows)` and a `Sync`.
fn execute_slice(connection: &mut Connection<'_>, portal: &str, max_rows: i32) -> Vec<Frame> {
    let mut batch = common::execute_message(portal, max_rows);
    batch.extend_from_slice(&sync_message());
    frames(&connection.feed(&batch))
}

/// Parse, bind and describe `sql` as the named portal `portal`.
fn open_portal(connection: &mut Connection<'_>, portal: &str, sql: &str) -> Vec<Frame> {
    let mut batch = parse_message(portal, sql, &[]);
    batch.extend_from_slice(&common::bind_message(portal, portal, &[], &[]));
    batch.extend_from_slice(&sync_message());
    frames(&connection.feed(&batch))
}

#[test]
fn a_portal_with_a_row_limit_pages_a_gql_answer_into_the_one_shot_answer() {
    let fixture = build_chain();
    let mut connection = connect(&fixture.service);
    for sql in PAGED {
        let whole = streamed(&mut connection, sql);
        assert!(whole.len() > 2 * SLICE, "`{sql}` spans several slices: {}", whole.len());
        assert_eq!(types_of(&open_portal(&mut connection, "p1", sql)), "12Z");
        let mut paged = Vec::new();
        loop {
            let got = execute_slice(&mut connection, "p1", SLICE as i32);
            let rows = arrived_rows(&got);
            let kinds = types_of(&got);
            paged.extend(rows.iter().cloned());
            if kinds.ends_with("sZ") {
                assert_eq!(rows.len(), SLICE, "`{sql}`: a suspended slice is full");
                continue;
            }
            assert!(kinds.ends_with("CZ"), "`{sql}`: {kinds}");
            assert_eq!(tag(&got), format!("SELECT {}", rows.len()), "`{sql}`");
            break;
        }
        assert_eq!(paged, whole, "`{sql}`: the slices are not the one-shot answer");
    }
}

#[test]
fn a_declared_cursor_fetches_a_gql_answer_in_pages_equal_to_the_one_shot_answer() {
    let fixture = build_chain();
    let mut connection = connect(&fixture.service);
    for sql in PAGED {
        let whole = streamed(&mut connection, sql);
        let got = frames(&connection.feed(&query(&format!("DECLARE c CURSOR FOR {sql}"))));
        assert_eq!(tag(&got), "DECLARE CURSOR", "`{sql}`: {}", types_of(&got));
        let mut paged = Vec::new();
        loop {
            let got = frames(&connection.feed(&query(&format!("FETCH {SLICE} FROM c"))));
            let rows = arrived_rows(&got);
            assert_eq!(tag(&got), format!("FETCH {}", rows.len()), "`{sql}`");
            if rows.is_empty() {
                break;
            }
            paged.extend(rows);
        }
        assert_eq!(paged, whole, "`{sql}`: the FETCHes are not the one-shot answer");
        let got = frames(&connection.feed(&query("CLOSE c")));
        assert_eq!(tag(&got), "CLOSE CURSOR");
    }
}

#[test]
fn a_cancel_request_stops_a_gql_walk_with_57014_and_the_connection_stays_usable() {
    let fixture = build_chain();
    let token = CancelToken::new();
    let mut connection = Connection::new(&fixture.service, common::key(8), token.clone());
    let _ = connection.feed(&common::startup());
    let expected = streamed(&mut connection, LONG_WALK);

    // The simple protocol. The token is sticky, so the walk stops at its
    // first check point rather than racing a second thread.
    token.cancel();
    let got = frames(&connection.feed(&query(LONG_WALK)));
    assert_eq!(types_of(&got), "EZ", "no row and no completion");
    let (sqlstate, message) = error_fields(first(&got, b'E'));
    assert_eq!(sqlstate, "57014", "{message}");
    assert!(message.contains("canceling statement"), "{message}");
    assert!(!token.is_cancelled(), "the ReadyForQuery cleared the cancel");
    assert_eq!(streamed(&mut connection, LONG_WALK), expected, "usable again");

    // The extended protocol, through a portal with a row limit.
    assert_eq!(types_of(&open_portal(&mut connection, "p1", PAGED[0])), "12Z");
    token.cancel();
    let got = execute_slice(&mut connection, "p1", SLICE as i32);
    assert_eq!(types_of(&got), "EZ");
    assert_eq!(error_fields(first(&got, b'E')).0, "57014");
    assert!(!token.is_cancelled(), "the Sync cleared the cancel");
    assert_eq!(streamed(&mut connection, LONG_WALK), expected, "usable again");
}

#[test]
fn a_statement_timeout_stops_a_gql_walk_with_57014_and_the_connection_stays_usable() {
    let fixture = build_chain();
    let mut connection = connect(&fixture.service);
    let expected = streamed(&mut connection, LONG_WALK);

    let got = frames(&connection.feed(&query("SET statement_timeout = '1ms'")));
    assert_eq!(tag(&got), "SET");
    let got = frames(&connection.feed(&query(LONG_WALK)));
    assert_eq!(types_of(&got), "EZ", "no row and no completion");
    let (sqlstate, message) = error_fields(first(&got, b'E'));
    assert_eq!(sqlstate, "57014", "{message}");
    assert!(
        message.contains("statement timeout") && message.contains("microseconds"),
        "the timeout's message, not the cancel's: {message}"
    );

    // A declared cursor: the DECLARE is what runs the walk, so it is the
    // statement refused, and no cursor is left to FETCH from.
    let got = frames(&connection.feed(&query(&format!("DECLARE c CURSOR FOR {LONG_WALK}"))));
    assert_eq!(error_fields(first(&got, b'E')).0, "57014");
    let got = frames(&connection.feed(&query("FETCH 1 FROM c")));
    assert_eq!(error_fields(first(&got, b'E')).0, "34000", "no cursor was opened");

    let _ = connection.feed(&query("SET statement_timeout = 0"));
    assert_eq!(streamed(&mut connection, LONG_WALK), expected, "cleared, the walk completes");
}

#[test]
fn a_portal_refused_part_way_stays_refused_and_never_completes() {
    // A refused execution is an INCOMPLETE answer (design §3.5): the next
    // `Execute` on the same portal is an error again -- never rows, never
    // a `CommandComplete` that would pass the answer off as whole.
    let fixture = build_chain();
    let token = CancelToken::new();
    let mut connection = Connection::new(&fixture.service, common::key(9), token.clone());
    let _ = connection.feed(&common::startup());

    // Cancelled.
    assert_eq!(types_of(&open_portal(&mut connection, "p1", PAGED[0])), "12Z");
    token.cancel();
    let got = execute_slice(&mut connection, "p1", SLICE as i32);
    assert_eq!(types_of(&got), "EZ");
    assert_eq!(error_fields(first(&got, b'E')).0, "57014");
    for _ in 0..2 {
        let got = execute_slice(&mut connection, "p1", SLICE as i32);
        assert_eq!(types_of(&got), "EZ", "the refused portal answered rows or a completion");
        assert_eq!(error_fields(first(&got, b'E')).0, "57014", "the same refusal again");
    }

    // Timed out.
    let _ = connection.feed(&query("SET statement_timeout = '1ms'"));
    assert_eq!(types_of(&open_portal(&mut connection, "p2", LONG_WALK)), "12Z");
    let got = execute_slice(&mut connection, "p2", 1);
    assert_eq!(error_fields(first(&got, b'E')).0, "57014");
    let _ = connection.feed(&query("SET statement_timeout = 0"));
    let got = execute_slice(&mut connection, "p2", 1);
    assert_eq!(types_of(&got), "EZ", "the timed-out portal answered rows or a completion");
    assert_eq!(error_fields(first(&got, b'E')).0, "57014");

    // A new portal of the same statement is a new execution, and runs.
    assert_eq!(types_of(&open_portal(&mut connection, "p3", PAGED[0])), "12Z");
    let got = execute_slice(&mut connection, "p3", 0);
    assert!(types_of(&got).ends_with("CZ"), "{}", types_of(&got));
}
