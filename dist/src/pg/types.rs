//! What a sekejap value, a sekejap column and a sekejap REFUSAL look like to
//! a PostgreSQL client: the `pg_type` OID table, the text and binary
//! encodings of one cell, the decoding of one bound `$n`, and the SQLSTATE
//! map.
//!
//! ## The OID table, and who owns it
//!
//! `docs/dist/PG_SURFACE.md` is where the catalog views (`pg_type`,
//! `pg_class`, `pg_attribute`) and the OIDs they publish are meant to live,
//! and worker PGCAT builds them. That document is NOT on this build's base
//! commit, so the table is defined HERE, in [`oid`], and PGCAT's views must
//! publish the same numbers. Every number below except the two synthetic
//! ones is a fixed PostgreSQL system OID and cannot differ; the two
//! synthetic ones are called out where they are declared.
//!
//! ## Text is the format; binary is what a driver asks for
//!
//! Every kind has a TEXT encoding, and that is the format this surface
//! describes itself as speaking. A `Bind` may nevertheless ask for result
//! format `1`, and the two drivers that matter most do: `rust-postgres`
//! asks for binary on every column of every extended-protocol query, and
//! pgjdbc asks for it on the types it knows. So [`encode_cell`] honours the
//! format code for the closed set of OIDs that have a binary encoding here,
//! and sends the TEXT bytes for every other OID -- which is exactly right
//! for the type the client will not know either way.

use sekejap_core::collections::{Error as CoreError, QueryError, WorkResource};
use sekejap_core::spatial_io;
use sekejap_core::Kind;
use sekejap_lang::{Param, SqlError, SqlValue};
use serde_json::Value;

use crate::service::ServiceError;

/// `pg_type.oid` for every type this surface names.
///
/// The last two are SYNTHETIC. PostGIS and pgvector both assign their type
/// OIDs per install rather than reserving one, and a client recognises them
/// by NAME through the catalog, so any stable number above the system range
/// serves. These are the numbers `docs/dist/PG_SURFACE.md` must publish for
/// `geometry` and `vector`; e1 used 18000 for geometry and it is kept.
pub mod oid {
    pub const BOOL: i32 = 16;
    pub const BYTEA: i32 = 17;
    pub const INT8: i32 = 20;
    pub const INT2: i32 = 21;
    pub const INT4: i32 = 23;
    pub const TEXT: i32 = 25;
    pub const JSON: i32 = 114;
    pub const FLOAT4: i32 = 700;
    pub const FLOAT8: i32 = 701;
    pub const VARCHAR: i32 = 1043;
    pub const DATE: i32 = 1082;
    pub const TIMESTAMP: i32 = 1114;
    pub const TIMESTAMPTZ: i32 = 1184;
    pub const NUMERIC: i32 = 1700;
    pub const JSONB: i32 = 3802;
    /// Synthetic: PostGIS `geometry`.
    pub const GEOMETRY: i32 = 18_000;
    /// Synthetic: pgvector `vector`.
    pub const VECTOR: i32 = 18_001;

    // The one-dimensional arrays a list column or a list parameter uses
    // (`docs/lang/GQL_PROFILE_DESIGN.md` §6.3): PostgreSQL's fixed `_type`
    // OIDs for the element types a list holds: text, int8, float8, bool,
    // jsonb, and the two declared times, date and timestamptz. Any other
    // `T[]` spelling names no wire type.
    pub const BOOL_ARRAY: i32 = 1000;
    pub const TEXT_ARRAY: i32 = 1009;
    pub const INT8_ARRAY: i32 = 1016;
    pub const FLOAT8_ARRAY: i32 = 1022;
    pub const DATE_ARRAY: i32 = 1182;
    pub const TIMESTAMPTZ_ARRAY: i32 = 1185;
    pub const JSONB_ARRAY: i32 = 3807;
}

/// `(array OID, element OID)` for every array in [`oid`].
const ARRAYS: &[(i32, i32)] = &[
    (oid::BOOL_ARRAY, oid::BOOL),
    (oid::TEXT_ARRAY, oid::TEXT),
    (oid::INT8_ARRAY, oid::INT8),
    (oid::FLOAT8_ARRAY, oid::FLOAT8),
    (oid::DATE_ARRAY, oid::DATE),
    (oid::TIMESTAMPTZ_ARRAY, oid::TIMESTAMPTZ),
    (oid::JSONB_ARRAY, oid::JSONB),
];

/// The element OID of an array OID, or `None` when `type_oid` is not one.
fn element_oid(type_oid: i32) -> Option<i32> {
    ARRAYS
        .iter()
        .find(|(array, _)| *array == type_oid)
        .map(|(_, element)| *element)
}

/// `pg_type.typlen` for an OID: the fixed width, or `-1` for a varlena.
pub fn type_size(type_oid: i32) -> i16 {
    match type_oid {
        oid::BOOL => 1,
        oid::INT2 => 2,
        oid::INT4 | oid::FLOAT4 | oid::DATE => 4,
        oid::INT8 | oid::FLOAT8 | oid::TIMESTAMP | oid::TIMESTAMPTZ => 8,
        _ => -1,
    }
}

/// The type name `docs/dist/PG_SURFACE.md`'s `pg_type.typname` must carry for
/// each OID above. Present so the two documents can be diffed rather than
/// compared by eye.
pub fn type_name(type_oid: i32) -> &'static str {
    match type_oid {
        oid::BOOL => "bool",
        oid::BYTEA => "bytea",
        oid::INT8 => "int8",
        oid::INT2 => "int2",
        oid::INT4 => "int4",
        oid::TEXT => "text",
        oid::JSON => "json",
        oid::FLOAT4 => "float4",
        oid::FLOAT8 => "float8",
        oid::VARCHAR => "varchar",
        oid::DATE => "date",
        oid::TIMESTAMP => "timestamp",
        oid::TIMESTAMPTZ => "timestamptz",
        oid::NUMERIC => "numeric",
        oid::JSONB => "jsonb",
        oid::GEOMETRY => "geometry",
        oid::VECTOR => "vector",
        _ => "unknown",
    }
}

/// The OID a DECLARED column type maps to.
///
/// The declared spelling is consulted FIRST because the stored `Kind` cannot
/// answer: `TIMESTAMPTZ` and `DATE` are both `Kind::Int` (UTC microseconds,
/// `docs/lang/QL_CONTRACT.md` §5 deviation 8), so only the catalog's
/// declared pair says which a column is.
///
/// A list spelling, `T[]`, maps to the array OID of `T`'s OID (`TEXT[]` is
/// `_text`, `DOUBLE PRECISION[]` is `_float8`), and to `None` when that type
/// has no array OID here.
pub fn oid_for_declared(declared: &str) -> Option<i32> {
    let upper = declared.trim().to_ascii_uppercase();
    if let Some(element) = upper.strip_suffix("[]") {
        let element = oid_for_declared(element)?;
        return ARRAYS
            .iter()
            .find(|(_, of)| *of == element)
            .map(|(array, _)| *array);
    }
    let head = upper.split(['(', ' ']).next().unwrap_or("");
    Some(match head {
        "TEXT" | "VARCHAR" | "CHAR" | "UUID" => oid::TEXT,
        "INT" | "INTEGER" | "INT4" | "SMALLINT" | "INT2" | "BIGINT" | "INT8" => oid::INT8,
        "REAL" | "FLOAT" | "FLOAT4" | "FLOAT8" | "DOUBLE" => oid::FLOAT8,
        "BOOL" | "BOOLEAN" => oid::BOOL,
        "JSON" | "JSONB" => oid::JSONB,
        "TIMESTAMPTZ" | "TIMESTAMP" => oid::TIMESTAMPTZ,
        "DATE" => oid::DATE,
        "GEOMETRY" | "GEOGRAPHY" | "POINT" => oid::GEOMETRY,
        "VECTOR" => oid::VECTOR,
        "BYTEA" => oid::BYTEA,
        _ => return None,
    })
}

/// The OID a stored [`Kind`] maps to when no declared spelling refines it.
pub fn oid_for_kind(kind: &Kind) -> i32 {
    match kind {
        Kind::Text => oid::TEXT,
        Kind::Int => oid::INT8,
        Kind::Real => oid::FLOAT8,
        Kind::Bool => oid::BOOL,
        Kind::Json => oid::JSONB,
        Kind::Geo | Kind::Point => oid::GEOMETRY,
        Kind::Vector(_) => oid::VECTOR,
    }
}

// There is deliberately NO `oid_for_value`. A `RowDescription` is answered
// before the first row is walked -- `Describe('S')` happens before any
// parameter is bound -- and a client decodes every later row with what it
// read there, so an OID inferred from VALUES would type the same statement
// differently at `Describe` and at `Execute`. A column that is not a
// declared field of the source collection is `text`, carrying the text
// `sekejap_lang` prints. See `docs/dist/WIRE_CONTRACT.md` §3.1.

// ── one cell, out ────────────────────────────────────────────────────────

/// One value as PostgreSQL TEXT format. `None` is SQL NULL.
///
/// `_id` is `"<collection>:<sequence>"`, the same spelling
/// `dist/rust/src/rows.rs:100` gives it, so the wire and the Rust surface
/// name a row identically. A geometry column rides as GeoJSON text; a client
/// that wants the bytes asks for them with `ST_AsBinary` / `ST_AsEWKB`, whose
/// `bytea` value `sekejap_lang` prints in PostgreSQL's hex form (`\x...`).
pub fn text_of(value: &SqlValue) -> Option<Vec<u8>> {
    Some(match value {
        SqlValue::Missing | SqlValue::Null => return None,
        SqlValue::Bool(b) => (if *b { "t" } else { "f" }).as_bytes().to_vec(),
        SqlValue::Int(n) => n.to_string().into_bytes(),
        SqlValue::Float(f) => float_text(*f).into_bytes(),
        SqlValue::Text(t) => t.clone().into_bytes(),
        SqlValue::Json(v) => json_text(v).into_bytes(),
        SqlValue::Id(id) => format!("{}:{}", id.collection.0, id.sequence).into_bytes(),
    })
}

/// A `float8` as PostgreSQL prints it: shortest round-trip, with the three
/// non-finite values spelled the way the server spells them.
fn float_text(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    let text = format!("{value}");
    // Rust prints `1` for 1.0_f64 and so does PostgreSQL, so nothing is
    // added here; this is the assertion that the two agree, kept as a
    // comment rather than a rewrite.
    text
}

/// A JSON value as `jsonb` text. A geometry is a JSON object, so this is
/// also the GeoJSON encoding.
fn json_text(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned())
}

/// Microseconds between the Unix epoch and the PostgreSQL epoch
/// (2000-01-01 00:00:00 UTC), which is what a binary `timestamptz` counts
/// from.
const PG_EPOCH_MICROS: i64 = 946_684_800_000_000;

/// One value in the format the `Bind` asked for.
///
/// `format` is `0` for text and `1` for binary. A binary request for an OID
/// with no binary encoding here gets the TEXT bytes: the client does not
/// know the type either, so the bytes it receives are the bytes it would
/// have received, and nothing is silently mis-typed.
pub fn encode_cell(value: &SqlValue, type_oid: i32, format: i16) -> Option<Vec<u8>> {
    // A list is a JSON array (`docs/lang/GQL_PROFILE_DESIGN.md` §6.3); under
    // an array OID it is written as a PostgreSQL array in either format.
    if let (Some(element), SqlValue::Json(Value::Array(items))) = (element_oid(type_oid), value) {
        return Some(if format == 0 {
            array_text(items, element)
        } else {
            array_binary(items, element)
        });
    }
    if format == 0 {
        return text_of(value);
    }
    if matches!(value, SqlValue::Missing | SqlValue::Null) {
        return None;
    }
    Some(match (type_oid, value) {
        (oid::BOOL, SqlValue::Bool(b)) => vec![u8::from(*b)],
        (oid::INT8, SqlValue::Int(n)) => n.to_be_bytes().to_vec(),
        (oid::INT8, SqlValue::Float(f)) => (*f as i64).to_be_bytes().to_vec(),
        (oid::INT4, SqlValue::Int(n)) => (*n as i32).to_be_bytes().to_vec(),
        (oid::INT2, SqlValue::Int(n)) => (*n as i16).to_be_bytes().to_vec(),
        (oid::FLOAT8, SqlValue::Float(f)) => f.to_be_bytes().to_vec(),
        (oid::FLOAT8, SqlValue::Int(n)) => (*n as f64).to_be_bytes().to_vec(),
        (oid::FLOAT4, SqlValue::Float(f)) => (*f as f32).to_be_bytes().to_vec(),
        // A declared TIMESTAMPTZ is printed back by `sekejap_lang` as an
        // ISO-8601 STRING (`lang/src/compile/row.rs`), so the binary form is
        // reached from that text; a column still carrying raw microseconds
        // is converted directly.
        (oid::TIMESTAMPTZ, SqlValue::Int(micros)) => {
            (micros - PG_EPOCH_MICROS).to_be_bytes().to_vec()
        }
        (oid::TIMESTAMPTZ, SqlValue::Text(iso)) => match iso_to_micros(iso) {
            Some(micros) => (micros - PG_EPOCH_MICROS).to_be_bytes().to_vec(),
            None => iso.clone().into_bytes(),
        },
        (oid::DATE, SqlValue::Int(micros)) => date_days(*micros),
        // A declared DATE is printed back by `sekejap_lang` as ISO-8601
        // text, as a TIMESTAMPTZ is.
        (oid::DATE, SqlValue::Text(iso)) => match iso_to_micros(iso) {
            Some(micros) => date_days(micros),
            None => iso.clone().into_bytes(),
        },
        // A `bytea` is printed by `sekejap_lang` in PostgreSQL's hex form,
        // `\x` and the digits; its binary form is the bytes themselves. This
        // is how QGIS reads `ST_AsBinary` through a binary cursor.
        (oid::BYTEA, SqlValue::Text(hex)) => match spatial_io::from_hex(hex) {
            Ok(bytes) => bytes,
            Err(_) => hex.clone().into_bytes(),
        },
        // `jsonb`'s binary form is a one-byte version stamp followed by the
        // JSON text. `json`'s is the text alone.
        (oid::JSONB, _) => {
            let mut bytes = vec![1u8];
            bytes.extend_from_slice(binary_text(value).as_bytes());
            bytes
        }
        // text, varchar, and every synthetic OID: the text bytes, which is
        // what `text`'s binary format IS.
        _ => return text_of(value),
    })
}

/// A binary `date`: days since the PostgreSQL epoch.
fn date_days(micros: i64) -> Vec<u8> {
    let days = (micros - PG_EPOCH_MICROS).div_euclid(86_400_000_000) as i32;
    days.to_be_bytes().to_vec()
}

/// The text a binary encoder reaches for when it needs the value spelled.
fn binary_text(value: &SqlValue) -> String {
    match value {
        SqlValue::Json(v) => json_text(v),
        SqlValue::Text(t) => t.clone(),
        other => String::from_utf8(text_of(other).unwrap_or_default()).unwrap_or_default(),
    }
}

/// One JSON list element as the cell value its element type encodes. A
/// `jsonb` element is the JSON itself (a string element is the JSON string
/// `"s"`, not the text `s`); every other element is the scalar it spells.
fn element_value(item: &Value, element: i32) -> SqlValue {
    match item {
        Value::Null => SqlValue::Null,
        _ if element == oid::JSONB => SqlValue::Json(item.clone()),
        Value::Bool(b) => SqlValue::Bool(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => SqlValue::Int(i),
            None => SqlValue::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        Value::String(s) => SqlValue::Text(s.clone()),
        other => SqlValue::Json(other.clone()),
    }
}

/// A list in `array_out`'s text form: `{a,"b c",NULL}`, `{}` when empty.
/// An element is double-quoted when it is empty, when it reads as `NULL` in
/// any case, or when it holds a brace, a comma, a quote, a backslash or
/// white space; inside the quotes a quote and a backslash are escaped with
/// a backslash.
fn array_text(items: &[Value], element: i32) -> Vec<u8> {
    let mut out = String::from("{");
    for (at, item) in items.iter().enumerate() {
        if at > 0 {
            out.push(',');
        }
        let Some(bytes) = text_of(&element_value(item, element)) else {
            out.push_str("NULL");
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        let quote = text.is_empty()
            || text.eq_ignore_ascii_case("NULL")
            || text
                .chars()
                .any(|c| matches!(c, '{' | '}' | ',' | '"' | '\\') || is_array_space(c));
        if quote {
            out.push('"');
            for c in text.chars() {
                if matches!(c, '"' | '\\') {
                    out.push('\\');
                }
                out.push(c);
            }
            out.push('"');
        } else {
            out.push_str(&text);
        }
    }
    out.push('}');
    out.into_bytes()
}

/// The white space `array_in` skips around an unquoted element.
fn is_array_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{0b}' | '\u{0c}')
}

/// A list in `array_send`'s binary form: ndim, has-null flag, element OID,
/// then (length, lower bound 1) and each element length-prefixed, `-1` for
/// NULL. An empty list has zero dimensions and no dimension pair, as
/// PostgreSQL sends `'{}'`.
fn array_binary(items: &[Value], element: i32) -> Vec<u8> {
    let cells: Vec<Option<Vec<u8>>> = items
        .iter()
        .map(|item| encode_cell(&element_value(item, element), element, 1))
        .collect();
    let has_null = cells.iter().any(Option::is_none);
    let mut out = Vec::new();
    out.extend_from_slice(&i32::from(!cells.is_empty()).to_be_bytes());
    out.extend_from_slice(&i32::from(has_null).to_be_bytes());
    out.extend_from_slice(&element.to_be_bytes());
    if cells.is_empty() {
        return out;
    }
    out.extend_from_slice(&(cells.len() as i32).to_be_bytes());
    out.extend_from_slice(&1i32.to_be_bytes());
    for cell in cells {
        match cell {
            None => out.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(bytes) => {
                out.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                out.extend_from_slice(&bytes);
            }
        }
    }
    out
}

/// `YYYY-MM-DDTHH:MM:SS[.ffffff][Z]` and the space-separated spelling, to
/// UTC microseconds. `None` when the text is not that shape, and the caller
/// then sends the text unchanged rather than a wrong number.
fn iso_to_micros(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
    let year: i64 = text.get(0..4)?.parse().ok()?;
    let month: i64 = text.get(5..7)?.parse().ok()?;
    let day: i64 = text.get(8..10)?.parse().ok()?;
    let (mut hour, mut minute, mut second, mut micros) = (0i64, 0i64, 0i64, 0i64);
    if bytes.len() >= 19 {
        hour = text.get(11..13)?.parse().ok()?;
        minute = text.get(14..16)?.parse().ok()?;
        second = text.get(17..19)?.parse().ok()?;
        if bytes.len() > 20 && bytes[19] == b'.' {
            let frac: String = text[20..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if !frac.is_empty() {
                let scaled = format!("{frac:0<6}");
                micros = scaled.get(0..6)?.parse().ok()?;
            }
        }
    }
    Some(
        (days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
            * 1_000_000
            + micros,
    )
}

/// Days from 1970-01-01 to `(year, month, day)`, proleptic Gregorian.
/// Howard Hinnant's `days_from_civil`, which is exact for every date this
/// surface can be handed.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ── one `$n`, in ─────────────────────────────────────────────────────────

/// Decode one bound parameter into a [`Param`].
///
/// `type_oid` is what the `Parse` DECLARED for this position, or `0` when it
/// declared nothing. A declared OID is believed; an undeclared one is read
/// by shape -- a whole number, then a number, then a boolean word, then
/// text -- because `Param`'s type is read from WHERE it is used
/// (`sekejap_lang::Param`), and handing `Param::Text("42")` to an `INT`
/// column refuses where `Param::Int(42)` does not.
pub fn decode_param(bytes: Option<&[u8]>, type_oid: i32, format: i16) -> Result<Param, SqlError> {
    let Some(bytes) = bytes else {
        return Ok(Param::Null);
    };
    if format == 1 {
        return decode_binary_param(bytes, type_oid);
    }
    let text = String::from_utf8_lossy(bytes).into_owned();
    if let Some(element) = element_oid(type_oid) {
        return decode_array_text(&text, element);
    }
    Ok(match type_oid {
        oid::BOOL => Param::Bool(matches!(
            text.as_str(),
            "t" | "true" | "TRUE" | "y" | "yes" | "on" | "1"
        )),
        oid::INT2 | oid::INT4 | oid::INT8 => Param::Int(text.trim().parse().map_err(|_| {
            SqlError::Parameter(format!("`{text}` is not the whole number its type declares"))
        })?),
        oid::FLOAT4 | oid::FLOAT8 | oid::NUMERIC => {
            Param::Float(text.trim().parse().map_err(|_| {
                SqlError::Parameter(format!("`{text}` is not the number its type declares"))
            })?)
        }
        oid::JSON | oid::JSONB => Param::Json(serde_json::from_str(&text).map_err(|e| {
            SqlError::Parameter(format!("parameter declared jsonb is not JSON: {e}"))
        })?),
        oid::VECTOR => Param::Vector(parse_vector_text(&text)?),
        oid::TEXT | oid::VARCHAR | oid::GEOMETRY => Param::Text(text),
        _ => sniff(text),
    })
}

fn decode_binary_param(bytes: &[u8], type_oid: i32) -> Result<Param, SqlError> {
    let short = || SqlError::Parameter(format!("binary parameter of type {type_oid} is truncated"));
    if let Some(element) = element_oid(type_oid) {
        return decode_array_binary(bytes, type_oid, element);
    }
    Ok(match type_oid {
        oid::BOOL => Param::Bool(bytes.first().copied().unwrap_or(0) != 0),
        oid::INT2 => Param::Int(i16::from_be_bytes(bytes.try_into().map_err(|_| short())?).into()),
        oid::INT4 => Param::Int(i32::from_be_bytes(bytes.try_into().map_err(|_| short())?).into()),
        oid::INT8 => Param::Int(i64::from_be_bytes(bytes.try_into().map_err(|_| short())?)),
        oid::FLOAT4 => {
            Param::Float(f32::from_be_bytes(bytes.try_into().map_err(|_| short())?).into())
        }
        oid::FLOAT8 => Param::Float(f64::from_be_bytes(bytes.try_into().map_err(|_| short())?)),
        oid::TIMESTAMPTZ | oid::TIMESTAMP => Param::Int(
            i64::from_be_bytes(bytes.try_into().map_err(|_| short())?)
                .saturating_add(PG_EPOCH_MICROS),
        ),
        oid::DATE => Param::Int(
            i64::from(i32::from_be_bytes(bytes.try_into().map_err(|_| short())?))
                .saturating_mul(86_400_000_000)
                .saturating_add(PG_EPOCH_MICROS),
        ),
        oid::JSONB => {
            // The one-byte version stamp, then the JSON text.
            let body = if bytes.first() == Some(&1) { &bytes[1..] } else { bytes };
            Param::Json(serde_json::from_slice(body).map_err(|e| {
                SqlError::Parameter(format!("binary jsonb parameter is not JSON: {e}"))
            })?)
        }
        oid::JSON => Param::Json(serde_json::from_slice(bytes).map_err(|e| {
            SqlError::Parameter(format!("binary json parameter is not JSON: {e}"))
        })?),
        // A binary `bytea` is the bytes; `sekejap_lang` reads a `bytea` in
        // its hex text form, which is what a text-format `bytea` arrives as.
        oid::BYTEA => Param::Text(format!("\\x{}", spatial_io::to_hex(bytes))),
        // text, varchar, unknown, and the synthetic OIDs: the bytes ARE the
        // text.
        _ => Param::Text(String::from_utf8_lossy(bytes).into_owned()),
    })
}

/// A decoded list element as the JSON element of `Param::Json(array)`.
fn element_json(param: Param) -> Result<Value, SqlError> {
    Ok(match param {
        Param::Null => Value::Null,
        Param::Bool(b) => Value::Bool(b),
        Param::Int(i) => Value::from(i),
        Param::Float(f) => serde_json::Number::from_f64(f).map(Value::Number).ok_or_else(|| {
            SqlError::Parameter(format!("array element {f} is not a finite number"))
        })?,
        Param::Text(s) => Value::String(s),
        Param::Json(v) => v,
        Param::Vector(v) => Value::from(v),
    })
}

/// An array parameter in `array_in`'s text form, `{a,"b c",NULL}`, to
/// `Param::Json(array)`. Each element is decoded as a parameter of the
/// element type would be; an unquoted `NULL` in any case is a null element.
/// Only one dimension is accepted, because a list is one-dimensional.
fn decode_array_text(text: &str, element: i32) -> Result<Param, SqlError> {
    let malformed = || {
        SqlError::Parameter(format!(
            "`{text}` is not a PostgreSQL array literal of the form {{a,\"b c\",NULL}}"
        ))
    };
    let chars: Vec<char> = text.trim().chars().collect();
    if chars.first() != Some(&'{') {
        return Err(malformed());
    }
    let mut at = 1;
    let skip_space = |at: &mut usize| {
        while *at < chars.len() && is_array_space(chars[*at]) {
            *at += 1;
        }
    };
    let mut items = Vec::new();
    skip_space(&mut at);
    if chars.get(at) == Some(&'}') {
        at += 1;
    } else {
        loop {
            skip_space(&mut at);
            let (raw, quoted_or_escaped) = match chars.get(at) {
                None => return Err(malformed()),
                Some('{') => {
                    return Err(SqlError::Parameter(format!(
                        "`{text}` has more than one dimension; a list parameter is a \
                         one-dimensional array"
                    )))
                }
                Some('"') => {
                    at += 1;
                    let mut raw = String::new();
                    loop {
                        match chars.get(at) {
                            None => return Err(malformed()),
                            Some('"') => break,
                            Some('\\') => {
                                raw.push(*chars.get(at + 1).ok_or_else(malformed)?);
                                at += 2;
                                continue;
                            }
                            Some(c) => raw.push(*c),
                        }
                        at += 1;
                    }
                    at += 1;
                    skip_space(&mut at);
                    (raw, true)
                }
                Some(_) => {
                    let mut raw = String::new();
                    let mut escaped = false;
                    // The length of `raw` up to its last character that is not
                    // trailing white space (an escaped space is kept).
                    let mut kept = 0;
                    loop {
                        match chars.get(at) {
                            None => return Err(malformed()),
                            Some(',' | '}') => break,
                            Some('{' | '"') => return Err(malformed()),
                            Some('\\') => {
                                raw.push(*chars.get(at + 1).ok_or_else(malformed)?);
                                kept = raw.len();
                                escaped = true;
                                at += 2;
                                continue;
                            }
                            Some(c) => {
                                raw.push(*c);
                                if !is_array_space(*c) {
                                    kept = raw.len();
                                }
                            }
                        }
                        at += 1;
                    }
                    raw.truncate(kept);
                    if raw.is_empty() {
                        return Err(malformed());
                    }
                    (raw, escaped)
                }
            };
            items.push(if !quoted_or_escaped && raw.eq_ignore_ascii_case("NULL") {
                Value::Null
            } else {
                element_json(decode_param(Some(raw.as_bytes()), element, 0)?)?
            });
            match chars.get(at) {
                Some(',') => at += 1,
                Some('}') => {
                    at += 1;
                    break;
                }
                _ => return Err(malformed()),
            }
        }
    }
    if at != chars.len() {
        return Err(malformed());
    }
    Ok(Param::Json(Value::Array(items)))
}

/// An array parameter in `array_send`'s binary form to `Param::Json(array)`:
/// ndim (0 or 1), has-null flag, element OID (must be the declared one),
/// then for one dimension (length, lower bound) and each element
/// length-prefixed, `-1` for NULL.
fn decode_array_binary(bytes: &[u8], type_oid: i32, element: i32) -> Result<Param, SqlError> {
    let short = || SqlError::Parameter(format!("binary parameter of type {type_oid} is truncated"));
    let mut at = 0usize;
    let word = |at: &mut usize| -> Result<i32, SqlError> {
        let slice = bytes.get(*at..*at + 4).ok_or_else(short)?;
        *at += 4;
        Ok(i32::from_be_bytes(slice.try_into().map_err(|_| short())?))
    };
    let ndim = word(&mut at)?;
    let _has_null = word(&mut at)?;
    let sent_element = word(&mut at)?;
    if sent_element != element {
        return Err(SqlError::Parameter(format!(
            "binary array parameter of type {type_oid} carries element type {sent_element}, \
             not {element}"
        )));
    }
    let mut items = Vec::new();
    match ndim {
        0 => {}
        1 => {
            let len = word(&mut at)?;
            let _lower_bound = word(&mut at)?;
            // Every element costs at least its four-byte length, so a count
            // the frame cannot hold is refused before anything is reserved.
            if len < 0 || (len as usize).saturating_mul(4) > bytes.len() - at {
                return Err(short());
            }
            items.reserve(len as usize);
            for _ in 0..len {
                let size = word(&mut at)?;
                if size < 0 {
                    items.push(Value::Null);
                    continue;
                }
                let cell = bytes.get(at..at + size as usize).ok_or_else(short)?;
                at += size as usize;
                items.push(element_json(decode_binary_param(cell, element)?)?);
            }
        }
        _ => {
            return Err(SqlError::Parameter(format!(
                "binary array parameter of type {type_oid} has {ndim} dimensions; a list \
                 parameter is a one-dimensional array"
            )))
        }
    }
    if at != bytes.len() {
        return Err(SqlError::Parameter(format!(
            "binary array parameter of type {type_oid} has bytes after its last element"
        )));
    }
    Ok(Param::Json(Value::Array(items)))
}

/// An undeclared text parameter, read by shape.
fn sniff(text: String) -> Param {
    if let Ok(n) = text.trim().parse::<i64>() {
        return Param::Int(n);
    }
    if let Ok(f) = text.trim().parse::<f64>() {
        if f.is_finite() {
            return Param::Float(f);
        }
    }
    Param::Text(text)
}

/// pgvector's text form, `[a,b,c]`.
fn parse_vector_text(text: &str) -> Result<Vec<f32>, SqlError> {
    let body = text.trim().trim_start_matches('[').trim_end_matches(']');
    if body.trim().is_empty() {
        return Ok(Vec::new());
    }
    body.split(',')
        .map(|part| {
            part.trim().parse::<f32>().map_err(|_| {
                SqlError::Parameter(format!("`{text}` is not pgvector's `[a,b,c]` spelling"))
            })
        })
        .collect()
}

// ── refusals, out ────────────────────────────────────────────────────────

/// A SQLSTATE and the message that goes with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WireError {
    pub sqlstate: &'static str,
    pub message: String,
}

impl WireError {
    pub fn new(sqlstate: &'static str, message: impl Into<String>) -> Self {
        Self {
            sqlstate,
            message: message.into(),
        }
    }
}

/// `0A000 feature_not_supported`: a Tier-2 or Tier-3 construct, with the
/// contract's own named reason.
pub const FEATURE_NOT_SUPPORTED: &str = "0A000";
/// `42601 syntax_error`.
pub const SYNTAX_ERROR: &str = "42601";
/// `42P01 undefined_table`.
pub const UNDEFINED_TABLE: &str = "42P01";
/// `XX001 data_corrupted`.
pub const DATA_CORRUPTED: &str = "XX001";
/// `57014 query_canceled` -- BOTH the §3 deadline and the §4 cancel, per
/// `docs/dist/OPS_CONTRACT.md` §9.2, which are told apart by the message.
pub const QUERY_CANCELED: &str = "57014";
/// `54000 program_limit_exceeded`: a NAMED `QueryBudget` resource.
pub const PROGRAM_LIMIT_EXCEEDED: &str = "54000";
/// `25006 read_only_sql_transaction`: a write issued against a snapshot.
pub const READ_ONLY: &str = "25006";
/// `55006 object_in_use`: another connection holds the single writer. The
/// ONE stated departure from "a service refusal is 0A000": this one is
/// transient, and 55006 is the code a pooler retries on where 0A000 is the
/// code it gives up on.
pub const OBJECT_IN_USE: &str = "55006";
/// `42P07 duplicate_table`.
pub const DUPLICATE_TABLE: &str = "42P07";
/// `22023 invalid_parameter_value`.
pub const INVALID_PARAMETER: &str = "22023";
/// `26000 invalid_sql_statement_name`: no prepared statement by that name.
pub const INVALID_STATEMENT_NAME: &str = "26000";
/// `34000 invalid_cursor_name`: no portal or cursor by that name.
pub const INVALID_CURSOR_NAME: &str = "34000";
/// `08P01 protocol_violation`: a frame this surface cannot read.
pub const PROTOCOL_VIOLATION: &str = "08P01";
/// `XX000 internal_error`: the store or the kernel refused for a reason
/// none of the above names.
pub const INTERNAL_ERROR: &str = "XX000";

/// What a cancel says. One sentence, in one place, because §9.2 makes the
/// MESSAGE -- not the code -- what tells a cancel from a timeout.
pub const CANCELLED_MESSAGE: &str =
    "canceling statement due to a CancelRequest (OPS_CONTRACT §4: the interrupt handle)";

/// What a statement timeout says, with the two microsecond readings §3
/// promises.
pub fn deadline_message(limit: u64, attempted: u64) -> String {
    format!(
        "canceling statement due to statement timeout: the page was allowed {limit} \
         microseconds and had spent {attempted} microseconds when the clock was read \
         (OPS_CONTRACT §3)"
    )
}

/// The first whole number after `key` in `text`.
fn field_after(text: &str, key: &str) -> Option<u64> {
    let at = text.find(key)? + key.len();
    let digits: String = text[at..].chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The marker `sekejap_lang` writes when a statement names a collection the
/// catalog does not have (`lang/src/lib.rs:880`, `lang/src/compile/ddl.rs:16`).
const NO_COLLECTION_NAMED: &str = "no collection named";

/// The marker the ENGINE writes when a boolean leaf has no membership set
/// (`core/engine/src/query/plan.rs`): a geometry predicate, a traversal, a
/// JSON equality, a text phrase, `IS NULL` / `IS MISSING`. All five spell the
/// refusal with this phrase and each names its own reason after it.
const NO_SET_FOR_LEAF: &str = "cannot be a boolean leaf";

/// Map one service refusal onto the SQLSTATE a PostgreSQL client expects.
pub fn wire_error(error: &ServiceError) -> WireError {
    match error {
        ServiceError::Sql(sql) => sql_error(sql),
        ServiceError::Query(query) => query_error(query),
        ServiceError::Core(core) => core_error(core),
        ServiceError::Refused(reason) => {
            // The service's own refusals. The single-writer one is transient
            // and gets `object_in_use`; every other one is a named refusal
            // and gets `feature_not_supported` with the reason as written.
            if reason.contains("single-writer") {
                WireError::new(OBJECT_IN_USE, reason.clone())
            } else {
                WireError::new(FEATURE_NOT_SUPPORTED, reason.clone())
            }
        }
    }
}

fn sql_error(error: &SqlError) -> WireError {
    match error {
        SqlError::Refused { .. } | SqlError::Unsupported(_) => {
            WireError::new(FEATURE_NOT_SUPPORTED, error.to_string())
        }
        SqlError::Syntax { .. } => WireError::new(SYNTAX_ERROR, error.to_string()),
        SqlError::Parameter(_) => WireError::new(INVALID_PARAMETER, error.to_string()),
        // PostgreSQL's own code, carried as data from where it was raised.
        SqlError::Coded { sqlstate, message } => WireError::new(sqlstate, message.clone()),
        SqlError::Engine(message) => {
            if message.contains(NO_COLLECTION_NAMED) {
                WireError::new(UNDEFINED_TABLE, error.to_string())
            } else if message.contains("Corrupt") {
                WireError::new(DATA_CORRUPTED, error.to_string())
            } else if message.contains("AlreadyExists") {
                WireError::new(DUPLICATE_TABLE, error.to_string())
            } else if message.contains("ReadOnly") {
                WireError::new(READ_ONLY, error.to_string())
            } else if message.contains("Cancelled") {
                WireError::new(QUERY_CANCELED, CANCELLED_MESSAGE)
            } else if message.contains("Deadline") {
                // `From<QueryError> for SqlError` flattens the structured
                // refusal into prose on the way up, so the two microsecond
                // readings §3 promises are read back out of it rather than
                // lost: they are what tells a TIMEOUT from a CANCEL, which
                // share this SQLSTATE by contract (OPS_CONTRACT §9.2).
                let limit = field_after(message, "limit: ").unwrap_or(0);
                let attempted = field_after(message, "attempted: ").unwrap_or(0);
                WireError::new(QUERY_CANCELED, deadline_message(limit, attempted))
            } else if message.contains("BudgetExceeded") {
                WireError::new(PROGRAM_LIMIT_EXCEEDED, error.to_string())
            } else if message.contains(NO_SET_FOR_LEAF) {
                // A boolean leaf with no membership SET: a geometry
                // predicate, a traversal, a JSON equality, a text phrase,
                // `IS NULL` / `IS MISSING`. Each is a NAMED refusal -- the
                // sentence says which leaf and why its postings are a
                // candidate test rather than a set (QL_CONTRACT §3) -- and a
                // named refusal must never reach a client as
                // `XX000 internal_error`, the one code a client RETRIES.
                //
                // Matched on the message rather than on a variant because the
                // cause is `core/engine/src/query/plan.rs`, where the refusal
                // is a `QueryError::Database(Error::InvalidInput)` and
                // `From<QueryError> for SqlError` flattens it into prose;
                // undoing that flattening means a new variant on the ENGINE's
                // error type, which is outside this slice. The other three
                // refusals of QL_CONTRACT §7 item 9 were fixed at the cause,
                // in `lang`, and no longer need an arm here.
                WireError::new(FEATURE_NOT_SUPPORTED, error.to_string())
            } else {
                WireError::new(INTERNAL_ERROR, error.to_string())
            }
        }
    }
}

fn query_error(error: &QueryError) -> WireError {
    match error {
        QueryError::Cancelled => WireError::new(QUERY_CANCELED, CANCELLED_MESSAGE),
        QueryError::BudgetExceeded {
            resource: WorkResource::Deadline,
            limit,
            attempted,
        } => WireError::new(QUERY_CANCELED, deadline_message(*limit, *attempted)),
        QueryError::BudgetExceeded {
            resource,
            limit,
            attempted,
        } => WireError::new(
            PROGRAM_LIMIT_EXCEEDED,
            format!("query budget {resource:?} exceeded: limit {limit}, attempted {attempted}"),
        ),
        QueryError::Database(core) => core_error(core),
    }
}

fn core_error(error: &CoreError) -> WireError {
    match error {
        CoreError::Corrupt(what) => {
            WireError::new(DATA_CORRUPTED, format!("the store refused: {what}"))
        }
        CoreError::Cancelled => WireError::new(QUERY_CANCELED, CANCELLED_MESSAGE),
        CoreError::BudgetExceeded {
            resource: WorkResource::Deadline,
            limit,
            attempted,
        } => WireError::new(QUERY_CANCELED, deadline_message(*limit, *attempted)),
        CoreError::BudgetExceeded {
            resource,
            limit,
            attempted,
        } => WireError::new(
            PROGRAM_LIMIT_EXCEEDED,
            format!("query budget {resource:?} exceeded: limit {limit}, attempted {attempted}"),
        ),
        CoreError::NotFound(what) => {
            WireError::new(UNDEFINED_TABLE, format!("no such {what}"))
        }
        CoreError::AlreadyExists => {
            WireError::new(DUPLICATE_TABLE, "it already exists".to_owned())
        }
        CoreError::ReadOnly => WireError::new(
            READ_ONLY,
            "this connection reads a published snapshot; a write takes the service's single \
             writer (OPS_CONTRACT §1)"
                .to_owned(),
        ),
        CoreError::Unsupported(what) => WireError::new(FEATURE_NOT_SUPPORTED, what.clone()),
        CoreError::InvalidInput(what) => WireError::new(INVALID_PARAMETER, what.clone()),
        CoreError::Kernel(e) => {
            let text = e.to_string();
            if text.contains("Corrupt") {
                WireError::new(DATA_CORRUPTED, text)
            } else {
                WireError::new(INTERNAL_ERROR, text)
            }
        }
        CoreError::Failed => {
            WireError::new(INTERNAL_ERROR, "the store refused without a reason".to_owned())
        }
    }
}
