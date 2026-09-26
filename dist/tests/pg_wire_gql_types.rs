//! The output plumbing a GQL list column and a list parameter need
//! (`docs/lang/GQL_PROFILE_DESIGN.md` §6.2, §6.3, §7), checked a byte at a
//! time before any statement produces a list.
//!
//! A list reaches a caller as `SqlValue::Json(array)`. On the wire it is a
//! real PostgreSQL array: the column's declared spelling (`TEXT[]`,
//! `BIGINT[]`, ...) picks the array OID, and the cell is written in the array
//! text form (`{a,"b c",NULL}`) or the one-dimensional binary form. A bound
//! array parameter comes back as `Param::Json(array)`.
//!
//! The oracle is PostgreSQL's own layout, written out here by hand: the
//! quoting rules of `array_out` and the header of `array_send`. No encoder
//! output is compared against another encoder call.

use sekejap_core::collections::{CollectionId, Database, EntityId};
use sekejap_dist::pg::types::{self, oid};
use sekejap_lang::{Param, SqlError, SqlRow, SqlValue};
use serde_json::{json, Value};

// ── the oracle ───────────────────────────────────────────────────────────

/// `array_send`'s layout for a one-dimensional array, from the protocol
/// documentation: ndim, has-null flag, element OID, then (length, lower
/// bound 1), then each element as a length-prefixed value (`-1` is NULL).
/// An empty array is ndim 0 with no dimension pair.
fn binary_array(element_oid: i32, elements: &[Option<Vec<u8>>]) -> Vec<u8> {
    let mut out = Vec::new();
    if elements.is_empty() {
        out.extend_from_slice(&0i32.to_be_bytes());
        out.extend_from_slice(&0i32.to_be_bytes());
        out.extend_from_slice(&element_oid.to_be_bytes());
        return out;
    }
    let has_null = elements.iter().any(Option::is_none);
    out.extend_from_slice(&1i32.to_be_bytes());
    out.extend_from_slice(&i32::from(has_null).to_be_bytes());
    out.extend_from_slice(&element_oid.to_be_bytes());
    out.extend_from_slice(&(elements.len() as i32).to_be_bytes());
    out.extend_from_slice(&1i32.to_be_bytes());
    for element in elements {
        match element {
            None => out.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(bytes) => {
                out.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                out.extend_from_slice(bytes);
            }
        }
    }
    out
}

fn text(value: &SqlValue, type_oid: i32) -> String {
    String::from_utf8(types::encode_cell(value, type_oid, 0).expect("a non-null cell"))
        .expect("utf-8")
}

fn binary(value: &SqlValue, type_oid: i32) -> Vec<u8> {
    types::encode_cell(value, type_oid, 1).expect("a non-null cell")
}

fn decoded(bytes: &[u8], type_oid: i32, format: i16) -> Param {
    types::decode_param(Some(bytes), type_oid, format).expect("the parameter decodes")
}

/// Text elements that each exercise one `array_out` quoting rule.
fn awkward_text() -> Value {
    json!([
        "plain",
        "two words",
        null,
        "",
        "NULL",
        "null",
        "say \"hi\"",
        "back\\slash",
        "a,b",
        "{braced}",
        " padded "
    ])
}

const AWKWARD_TEXT_OUT: &str = r#"{plain,"two words",NULL,"","NULL","null","say \"hi\"","back\\slash","a,b","{braced}"," padded "}"#;

// ── the OIDs ─────────────────────────────────────────────────────────────

#[test]
fn every_list_spelling_maps_to_its_postgresql_array_oid() {
    let cases = [
        ("TEXT[]", 1009, oid::TEXT_ARRAY),
        ("BIGINT[]", 1016, oid::INT8_ARRAY),
        ("DOUBLE PRECISION[]", 1022, oid::FLOAT8_ARRAY),
        ("BOOLEAN[]", 1000, oid::BOOL_ARRAY),
        ("JSONB[]", 3807, oid::JSONB_ARRAY),
    ];
    for (spelling, number, constant) in cases {
        assert_eq!(constant, number, "{spelling}: the constant is PostgreSQL's number");
        assert_eq!(types::oid_for_declared(spelling), Some(number), "{spelling}");
        assert_eq!(types::type_size(number), -1, "{spelling} is a varlena");
    }
    // The scalar spellings are untouched, including the one with a space.
    assert_eq!(types::oid_for_declared("DOUBLE PRECISION"), Some(oid::FLOAT8));
    assert_eq!(types::oid_for_declared("TEXT"), Some(oid::TEXT));
    // An array of a type with no array OID here is not guessed at.
    assert_eq!(types::oid_for_declared("GEOMETRY[]"), None);
    assert_eq!(types::oid_for_declared("VECTOR[]"), None);
    // A list of times is PostgreSQL's array of times.
    assert_eq!(types::oid_for_declared("TIMESTAMPTZ[]"), Some(1185));
    assert_eq!(types::oid_for_declared("DATE[]"), Some(1182));
    assert_eq!(types::oid_for_declared("BYTEA[]"), None);
}

#[test]
fn a_list_of_times_is_a_postgresql_array_of_times_in_text_and_binary() {
    // `sekejap_lang` prints each item as the scalar column prints it (ISO
    // 8601); the wire writes those as `timestamptz` and `date` elements.
    let ats = SqlValue::Json(json!(["2020-01-02T03:04:05Z", null]));
    let days = SqlValue::Json(json!(["2020-01-02"]));
    let (timestamptz_array, date_array) = (1185, 1182);
    assert_eq!(text(&ats, timestamptz_array), "{2020-01-02T03:04:05Z,NULL}");
    assert_eq!(text(&days, date_array), "{2020-01-02}");
    // Binary: microseconds and days since 2000-01-01.
    let micros: i64 = (7_306 * 86_400 + 3 * 3_600 + 4 * 60 + 5) * 1_000_000;
    assert_eq!(
        binary(&ats, timestamptz_array),
        binary_array(oid::TIMESTAMPTZ, &[Some(micros.to_be_bytes().to_vec()), None])
    );
    assert_eq!(
        binary(&days, date_array),
        binary_array(oid::DATE, &[Some(7_306i32.to_be_bytes().to_vec())])
    );
}

// ── cells, text format ───────────────────────────────────────────────────

#[test]
fn a_text_list_is_written_with_postgresql_array_quoting() {
    let cell = SqlValue::Json(awkward_text());
    assert_eq!(text(&cell, oid::TEXT_ARRAY), AWKWARD_TEXT_OUT);
}

#[test]
fn an_empty_list_is_braces_and_a_null_list_is_a_null_cell() {
    for array_oid in [oid::TEXT_ARRAY, oid::INT8_ARRAY, oid::JSONB_ARRAY] {
        assert_eq!(text(&SqlValue::Json(json!([])), array_oid), "{}");
        assert_eq!(types::encode_cell(&SqlValue::Null, array_oid, 0), None);
        assert_eq!(types::encode_cell(&SqlValue::Null, array_oid, 1), None);
    }
}

#[test]
fn scalar_lists_are_written_in_their_element_text_form() {
    assert_eq!(
        text(&SqlValue::Json(json!([1, -2, null, 9_007_199_254_740_993i64])), oid::INT8_ARRAY),
        "{1,-2,NULL,9007199254740993}"
    );
    assert_eq!(
        text(&SqlValue::Json(json!([1.5, null, -0.25])), oid::FLOAT8_ARRAY),
        "{1.5,NULL,-0.25}"
    );
    assert_eq!(
        text(&SqlValue::Json(json!([true, false, null])), oid::BOOL_ARRAY),
        "{t,f,NULL}"
    );
    // A jsonb element is its JSON text, quoted by the array rules.
    assert_eq!(
        text(&SqlValue::Json(json!([{"a": 1}, "s", 2, null])), oid::JSONB_ARRAY),
        r#"{"{\"a\":1}","\"s\"",2,NULL}"#
    );
}

// ── cells, binary format ─────────────────────────────────────────────────

#[test]
fn a_text_list_is_sent_as_a_one_dimensional_binary_array() {
    let cell = SqlValue::Json(json!(["a", null, "two words", ""]));
    let expected = binary_array(
        oid::TEXT,
        &[
            Some(b"a".to_vec()),
            None,
            Some(b"two words".to_vec()),
            Some(Vec::new()),
        ],
    );
    assert_eq!(binary(&cell, oid::TEXT_ARRAY), expected);
}

#[test]
fn an_int8_list_is_sent_with_eight_byte_elements() {
    let cell = SqlValue::Json(json!([1, null, -3]));
    let expected = binary_array(
        oid::INT8,
        &[
            Some(1i64.to_be_bytes().to_vec()),
            None,
            Some((-3i64).to_be_bytes().to_vec()),
        ],
    );
    assert_eq!(binary(&cell, oid::INT8_ARRAY), expected);
    // No NULL element: the flag is 0.
    let cell = SqlValue::Json(json!([7]));
    assert_eq!(
        binary(&cell, oid::INT8_ARRAY),
        binary_array(oid::INT8, &[Some(7i64.to_be_bytes().to_vec())])
    );
}

#[test]
fn an_empty_list_is_sent_with_zero_dimensions() {
    assert_eq!(
        binary(&SqlValue::Json(json!([])), oid::TEXT_ARRAY),
        binary_array(oid::TEXT, &[])
    );
    assert_eq!(
        binary(&SqlValue::Json(json!([])), oid::INT8_ARRAY),
        binary_array(oid::INT8, &[])
    );
}

#[test]
fn jsonb_elements_carry_the_version_stamp() {
    let cell = SqlValue::Json(json!(["s", {"k": true}]));
    let mut first = vec![1u8];
    first.extend_from_slice(br#""s""#);
    let mut second = vec![1u8];
    second.extend_from_slice(br#"{"k":true}"#);
    assert_eq!(
        binary(&cell, oid::JSONB_ARRAY),
        binary_array(oid::JSONB, &[Some(first), Some(second)])
    );
}

// ── the round trip, through the parameter decoder ────────────────────────

#[test]
fn a_text_list_round_trips_in_text_and_binary() {
    let lists = [awkward_text(), json!([]), json!([null]), json!(["only"])];
    for list in lists {
        let cell = SqlValue::Json(list.clone());
        let as_text = types::encode_cell(&cell, oid::TEXT_ARRAY, 0).expect("text");
        assert_eq!(decoded(&as_text, oid::TEXT_ARRAY, 0), Param::Json(list.clone()));
        let as_binary = binary(&cell, oid::TEXT_ARRAY);
        assert_eq!(decoded(&as_binary, oid::TEXT_ARRAY, 1), Param::Json(list));
    }
}

#[test]
fn an_int8_list_round_trips_in_text_and_binary() {
    let lists = [json!([1, null, -3, i64::MAX, i64::MIN]), json!([])];
    for list in lists {
        let cell = SqlValue::Json(list.clone());
        let as_text = types::encode_cell(&cell, oid::INT8_ARRAY, 0).expect("text");
        assert_eq!(decoded(&as_text, oid::INT8_ARRAY, 0), Param::Json(list.clone()));
        let as_binary = binary(&cell, oid::INT8_ARRAY);
        assert_eq!(decoded(&as_binary, oid::INT8_ARRAY, 1), Param::Json(list));
    }
}

// ── array parameters ─────────────────────────────────────────────────────

#[test]
fn an_array_parameter_decodes_to_a_json_array() {
    // What a client writes by hand, spaces and case included.
    assert_eq!(
        decoded(br#"{ a , "b c" ,NULL, null,"NULL"}"#, oid::TEXT_ARRAY, 0),
        Param::Json(json!(["a", "b c", null, null, "NULL"]))
    );
    assert_eq!(
        decoded(b"{1,2,NULL}", oid::INT8_ARRAY, 0),
        Param::Json(json!([1, 2, null]))
    );
    assert_eq!(
        decoded(b"{1.5,-2}", oid::FLOAT8_ARRAY, 0),
        Param::Json(json!([1.5, -2.0]))
    );
    assert_eq!(
        decoded(b"{t,f,NULL}", oid::BOOL_ARRAY, 0),
        Param::Json(json!([true, false, null]))
    );
    assert_eq!(
        decoded(br#"{"{\"a\":1}",2}"#, oid::JSONB_ARRAY, 0),
        Param::Json(json!([{"a": 1}, 2]))
    );
    assert_eq!(decoded(b"{}", oid::INT8_ARRAY, 0), Param::Json(json!([])));
    // An unquoted element keeps a backslash-escaped character.
    assert_eq!(
        decoded(br"{a\,b}", oid::TEXT_ARRAY, 0),
        Param::Json(json!(["a,b"]))
    );
}

#[test]
fn a_binary_array_parameter_decodes_to_a_json_array() {
    let bytes = binary_array(
        oid::FLOAT8,
        &[Some(0.5f64.to_be_bytes().to_vec()), None],
    );
    assert_eq!(
        decoded(&bytes, oid::FLOAT8_ARRAY, 1),
        Param::Json(json!([0.5, null]))
    );
    let bytes = binary_array(oid::BOOL, &[Some(vec![1]), Some(vec![0])]);
    assert_eq!(
        decoded(&bytes, oid::BOOL_ARRAY, 1),
        Param::Json(json!([true, false]))
    );
}

fn refused(bytes: &[u8], type_oid: i32, format: i16) -> String {
    match types::decode_param(Some(bytes), type_oid, format) {
        Err(SqlError::Parameter(message)) => message,
        other => panic!("expected a parameter refusal, got {other:?}"),
    }
}

#[test]
fn a_malformed_array_parameter_is_refused_by_name() {
    // PostgreSQL arrays are rectangular; a list here is one-dimensional.
    assert!(refused(b"{{1,2},{3,4}}", oid::INT8_ARRAY, 0).contains("one-dimensional"));
    assert!(refused(b"{1,2", oid::INT8_ARRAY, 0).contains("array"));
    assert!(refused(b"1,2", oid::INT8_ARRAY, 0).contains("array"));
    assert!(refused(br#"{"open}"#, oid::TEXT_ARRAY, 0).contains("array"));
    assert!(refused(b"{1,x}", oid::INT8_ARRAY, 0).contains("x"));
    // Binary: a truncated header, a two-dimensional header, and an element
    // OID that is not the declared one.
    assert!(refused(&[0, 0, 0, 1], oid::INT8_ARRAY, 1).contains("truncated"));
    let mut two_dims = Vec::new();
    for word in [2i32, 0, oid::INT8, 1, 1, 1, 1] {
        two_dims.extend_from_slice(&word.to_be_bytes());
    }
    assert!(refused(&two_dims, oid::INT8_ARRAY, 1).contains("one-dimensional"));
    let wrong = binary_array(oid::TEXT, &[Some(b"a".to_vec())]);
    assert!(refused(&wrong, oid::INT8_ARRAY, 1).contains("element"));
}

#[test]
fn a_non_array_parameter_decodes_exactly_as_before() {
    // Undeclared text shaped like an array is still read by shape: a text.
    assert_eq!(decoded(b"{1,2}", 0, 0), Param::Text("{1,2}".to_owned()));
    assert_eq!(decoded(b"42", 0, 0), Param::Int(42));
    assert_eq!(decoded(b"[1,2]", oid::JSONB, 0), Param::Json(json!([1, 2])));
}

// ── the owner of a row ───────────────────────────────────────────────────

#[test]
fn owner_is_none_for_the_sentinel_and_some_for_a_stored_row() {
    assert_eq!(
        EntityId::NO_OWNER,
        EntityId {
            collection: CollectionId(0),
            sequence: u64::MAX
        }
    );
    let relation = SqlRow {
        id: EntityId::NO_OWNER,
        values: vec![SqlValue::Int(1)],
    };
    assert_eq!(relation.owner(), None);
    let stored = EntityId {
        collection: CollectionId(3),
        sequence: 7,
    };
    let row = SqlRow {
        id: stored,
        values: Vec::new(),
    };
    assert_eq!(row.owner(), Some(stored));
    // Only the exact sentinel is "no owner": the virtual rows of collection 0
    // with an ordinary sequence keep the identity they had.
    let virtual_row = SqlRow {
        id: EntityId {
            collection: CollectionId(0),
            sequence: 4,
        },
        values: Vec::new(),
    };
    assert!(virtual_row.owner().is_some());
}

/// A database with one small collection, for the tests that prepare SQL.
fn item_db() -> (tempfile::TempDir, Database) {
    let dir = tempfile::TempDir::new().expect("a temp dir");
    let config = kernel::store::Config {
        budget_bytes: 8 << 20,
        io: kernel::io::IoMode::Buffered,
        sync: kernel::store::SyncMode::Full,
    };
    let mut db = Database::create(dir.path().join("db"), config).expect("create");
    {
        use sekejap_lang::SqlDatabase;
        db.sql("CREATE TABLE item (id TEXT PRIMARY KEY, n INT)", &[])
            .expect("create table");
        db.sql("INSERT INTO item (_key, id, n) VALUES ('i1', 'i1', 1)", &[])
            .expect("insert");
        db.sql("INSERT INTO item (_key, id, n) VALUES ('i2', 'i2', 2)", &[])
            .expect("insert");
    }
    (dir, db)
}

fn answer_rows(db: &mut Database, text: &str) -> Vec<SqlRow> {
    use sekejap_lang::SqlDatabase;
    match db.sql(text, &[]).expect(text) {
        sekejap_lang::SqlResult::Rows { rows, .. } => rows,
        other => panic!("`{text}` answered {other:?}, not rows"),
    }
}

#[test]
fn only_a_stored_row_has_an_owner_through_sql() {
    let (_dir, mut db) = item_db();
    // A row SELECT: every row names the stored row it came from.
    let stored = answer_rows(&mut db, "SELECT id FROM item");
    assert_eq!(stored.len(), 2);
    for row in &stored {
        let owner = row.owner().expect("a stored row has an owner");
        assert_ne!(owner.collection, CollectionId(0));
    }
    // A group, a whole-table aggregate and a catalog row have none.
    for text in [
        "SELECT n, COUNT(*) FROM item GROUP BY n",
        "SELECT COUNT(*) FROM item",
        "SELECT * FROM pg_catalog.pg_type",
    ] {
        let rows = answer_rows(&mut db, text);
        assert!(!rows.is_empty(), "`{text}` answered rows");
        for row in &rows {
            assert_eq!(row.owner(), None, "`{text}`");
            assert_eq!(row.id, EntityId::NO_OWNER, "`{text}`");
        }
    }
}

// ── the parameter-type hook ──────────────────────────────────────────────

#[test]
fn param_types_names_no_position_for_a_statement_that_types_none() {
    let (_dir, db) = item_db();
    let prepared = sekejap_lang::prepare_sql(
        &db,
        "SELECT id FROM item WHERE n = $1",
        &[Param::Int(1)],
    )
    .expect("prepare");
    // Today's plans type no `$n` themselves; the wire keeps answering
    // `text` for an undeclared position, exactly as before.
    assert!(prepared.param_types().iter().all(Option::is_none));
}
