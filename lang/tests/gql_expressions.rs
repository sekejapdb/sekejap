//! The P0 scalar expression pack inside a GQL body (M3-C of
//! `docs/lang/GQL_PROFILE_DESIGN.md`), driven through `prepare_sql`.
//!
//! The semantics are PostgreSQL's, and what is at risk is where a careless
//! evaluator would differ:
//!
//! * NULL propagation: every operator and function returns NULL on a NULL
//!   input, except `COALESCE`, `NULLIF`, `CASE`, `IS [NOT] NULL` and the
//!   three-valued `AND`/`OR` (`null_propagation_*`);
//! * `CASE` without `ELSE` is NULL when no branch holds, a NULL condition is
//!   not true, and a branch not taken is never evaluated (`case_*`);
//! * `LN` of zero or a negative number and a division or modulo by zero RAISE
//!   (SQLSTATE 2201E and 22012, owner answer Q14); they are never NULL
//!   (`ln_*`, `division_*`);
//! * integer arithmetic stays integer: `/` truncates toward zero, `%` takes
//!   the dividend's sign, and overflow is an error (22003), never a wrap
//!   (`integer_*`);
//! * `CAST` and `::` to the declared types sekejap has (`casts_*`);
//! * the string functions give exactly what the SQL row functions give on
//!   the same stored values (`string_functions_*`);
//! * `IN` over a list holding NULL is unknown unless a member matches
//!   (`in_lists_*`);
//! * values of kinds that do not compare are an error, as a comparison of
//!   them already is (`incomparable_*`);
//! * the precedence of the operators is PostgreSQL's (pinned in
//!   `lang/src/gql/parse/tests.rs`, where the normal-form printer lives).
//!
//! Workload names are invented: items `i1`-`i3`.

use sekejap_core::collections::Database;
use sekejap_core::Kind;
use sekejap_lang::{prepare_sql, Param, SqlDatabase, SqlError, SqlResult, SqlValue};
use serde_json::json;
use tempfile::TempDir;

mod common;
use common::cfg;

/// ```text
/// item.n is indexed
/// item i1  name "  Mixed Case  "  n  7  x  2.5  flag true   (no note)
/// item i2  name "ünïcode"         n -7  x -0.5  flag false  note "n2"
/// item i3  name "abc"             n  0  x  0.0  (no flag)   note null
/// ```
fn fixture(dir: &TempDir) -> Database {
    let mut db = Database::create(dir.path().join("e.sekejap"), cfg()).unwrap();
    let item = db
        .create_collection(
            "item",
            vec![
                ("name".to_owned(), Kind::Text),
                ("n".to_owned(), Kind::Int),
                ("x".to_owned(), Kind::Real),
                ("flag".to_owned(), Kind::Bool),
                ("note".to_owned(), Kind::Text),
            ],
            Default::default(),
        )
        .unwrap();
    db.put(
        item,
        "i1",
        &json!({"name": "  Mixed Case  ", "n": 7, "x": 2.5, "flag": true}),
    )
    .unwrap();
    db.put(
        item,
        "i2",
        &json!({"name": "ünïcode", "n": -7, "x": -0.5, "flag": false, "note": "n2"}),
    )
    .unwrap();
    db.put(
        item,
        "i3",
        &json!({"name": "abc", "n": 0, "x": 0.0, "note": null}),
    )
    .unwrap();
    // An index on `n` gives a multi-row pattern a seed, so a `WHERE` over
    // every item is a filter after it rather than a refused scan (Q5).
    let n = db.create_scalar_index(item, "item_n", "n", false).unwrap();
    while !db.build_index_step(n, 64).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();
    db
}

/// The rows of one GQL body, through the SQL entry point.
#[derive(Debug)]
struct Answer {
    rows: Vec<Vec<SqlValue>>,
}

fn run_with(db: &Database, body: &str, params: &[Param]) -> Result<Answer, SqlError> {
    let text = format!("SELECT * FROM GRAPH_TABLE (base {body})");
    match prepare_sql(db, &text, params)?.run(db)? {
        SqlResult::Rows { rows, .. } => Ok(Answer {
            rows: rows.into_iter().map(|row| row.values).collect(),
        }),
        other => panic!("`{text}` answered {other:?}"),
    }
}

/// `expr` evaluated over item `key`, with `params`.
fn eval_with(db: &Database, key: &str, expr: &str, params: &[Param]) -> Result<SqlValue, SqlError> {
    let body = format!("MATCH (a IS item WHERE a._key = '{key}') RETURN {expr} AS v");
    let answer = run_with(db, &body, params)?;
    assert_eq!(answer.rows.len(), 1, "`{body}`");
    Ok(answer.rows[0][0].clone())
}

fn eval(db: &Database, key: &str, expr: &str) -> SqlValue {
    eval_with(db, key, expr, &[])
        .unwrap_or_else(|error| panic!("`{expr}` over {key} failed: {error}"))
}

/// The error `expr` raises over item `key`, as text.
fn raises_with(db: &Database, key: &str, expr: &str, params: &[Param]) -> String {
    match eval_with(db, key, expr, params) {
        Err(error) => error.to_string(),
        Ok(value) => panic!("`{expr}` over {key} gave {value:?}, not an error"),
    }
}

fn raises(db: &Database, key: &str, expr: &str) -> String {
    raises_with(db, key, expr, &[])
}

/// The keys of the items a body returns as `a._key`, sorted.
fn keys(db: &Database, body: &str) -> Vec<String> {
    let answer = run_with(db, body, &[]).unwrap_or_else(|error| panic!("`{body}` failed: {error}"));
    let mut out: Vec<String> = answer
        .rows
        .iter()
        .map(|row| match &row[0] {
            SqlValue::Text(key) => key.clone(),
            other => panic!("`{body}` returned {other:?} as a key"),
        })
        .collect();
    out.sort();
    out
}

/// The keys of the items `predicate` keeps, as a `MATCH ... WHERE`
/// after the index seed on `n` that admits every item.
fn filter(db: &Database, predicate: &str) -> Vec<String> {
    keys(
        db,
        &format!("MATCH (a IS item WHERE a.n > -100) WHERE {predicate} RETURN a._key"),
    )
}

fn int(i: i64) -> SqlValue {
    SqlValue::Int(i)
}

fn float(f: f64) -> SqlValue {
    SqlValue::Float(f)
}

fn text(t: &str) -> SqlValue {
    SqlValue::Text(t.to_owned())
}

fn boolean(b: bool) -> SqlValue {
    SqlValue::Bool(b)
}

// ── NULL ──────────────────────────────────────────────────────────────────

#[test]
fn null_propagation_table() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // Item i1 holds no `note`: a missing property reads as NULL (design
    // §2.5), exactly as a stored null does (item i3).
    for key in ["i1", "i3"] {
        for expr in [
            "a.note",
            "a.note + 1",
            "1 - a.note",
            "a.note * 2",
            "a.note / 0",
            "a.note % 0",
            "-a.note",
            "a.note ^ 2",
            "a.note || 'x'",
            "'x' || a.note",
            "lower(a.note)",
            "upper(a.note)",
            "trim(a.note)",
            "length(a.note)",
            "substring(a.note, 1, 2)",
            "substring('abc', a.note)",
            "abs(a.note)",
            "sqrt(a.note)",
            "power(a.note, 2)",
            "power(2, a.note)",
            "exp(a.note)",
            "ln(a.note)",
            "CAST(a.note AS BIGINT)",
            "a.note::text",
            "a.note::date",
            "a.note = a.note",
            "a.note < 1",
            "NOT (a.note = 'x')",
            "a.note IN ('x', 'y')",
            "'x' IN (a.note)",
            "nullif(a.note, 'x')",
            "coalesce(a.note, NULL)",
            "CASE WHEN a.note = 'x' THEN 1 END",
            "CASE a.note WHEN NULL THEN 1 END",
            "a.note = 'x' AND TRUE",
            "a.note = 'x' OR FALSE",
        ] {
            assert_eq!(eval(&db, key, expr), SqlValue::Null, "`{expr}` over {key}");
        }
        // The exceptions: the null tests, COALESCE, NULLIF's second argument,
        // CASE's ELSE, and AND/OR once one side decides.
        for (expr, want) in [
            ("a.note IS NULL", boolean(true)),
            ("a.note IS NOT NULL", boolean(false)),
            ("NOT (a.note IS NULL)", boolean(false)),
            ("coalesce(a.note, NULL, 'third', 'fourth')", text("third")),
            ("coalesce(a.note, a.name)", eval(&db, key, "a.name")),
            ("nullif('x', a.note)", text("x")),
            ("CASE WHEN a.note = 'x' THEN 1 ELSE 2 END", int(2)),
            ("CASE a.note WHEN NULL THEN 1 ELSE 2 END", int(2)),
            ("a.note = 'x' AND FALSE", boolean(false)),
            ("a.note = 'x' OR TRUE", boolean(true)),
            ("concat(a.note, 'x')", text("x")),
        ] {
            assert_eq!(eval(&db, key, expr), want, "`{expr}` over {key}");
        }
    }
    assert_eq!(eval(&db, "i2", "a.note IS NOT NULL"), boolean(true));
    assert_eq!(eval(&db, "i2", "NULL IS NULL"), boolean(true));
    // A NULL in a filter keeps no row: the unknown is dropped, and so is its
    // negation.
    assert_eq!(filter(&db, "a.note = 'n2'"), ["i2"]);
    assert_eq!(filter(&db, "NOT (a.note = 'n2')"), Vec::<String>::new());
    assert_eq!(filter(&db, "a.note IS NULL"), ["i1", "i3"]);
    assert_eq!(
        keys(
            &db,
            "MATCH (a IS item WHERE a._key = 'i1' AND a.flag IS NOT NULL) RETURN a._key"
        ),
        ["i1"]
    );
    assert_eq!(
        keys(
            &db,
            "MATCH (a IS item WHERE a._key = 'i3' AND a.flag IS NOT NULL) RETURN a._key"
        ),
        Vec::<String>::new()
    );
}

// ── CASE ──────────────────────────────────────────────────────────────────

#[test]
fn case_without_else_is_null_and_takes_the_first_true_branch() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let searched = "CASE WHEN a.n > 0 THEN 'positive' WHEN a.n < 0 THEN 'negative' END";
    assert_eq!(eval(&db, "i1", searched), text("positive"));
    assert_eq!(eval(&db, "i2", searched), text("negative"));
    // No branch holds and there is no ELSE: NULL.
    assert_eq!(eval(&db, "i3", searched), SqlValue::Null);
    let with_else = "CASE WHEN a.n > 0 THEN 'positive' ELSE 'other' END";
    assert_eq!(eval(&db, "i3", with_else), text("other"));
    // The first true branch wins even when a later one holds too.
    assert_eq!(
        eval(
            &db,
            "i1",
            "CASE WHEN a.n > 5 THEN 1 WHEN a.n > 0 THEN 2 END"
        ),
        int(1)
    );
    // The simple form compares with `=`.
    let simple = "CASE a.n WHEN 7 THEN 'seven' WHEN -7 THEN 'minus seven' ELSE 'zero' END";
    assert_eq!(eval(&db, "i1", simple), text("seven"));
    assert_eq!(eval(&db, "i2", simple), text("minus seven"));
    assert_eq!(eval(&db, "i3", simple), text("zero"));
    assert_eq!(
        eval(&db, "i3", "CASE a.n WHEN 1 THEN 'one' END"),
        SqlValue::Null
    );
    // A branch that is not taken is not evaluated: no division by zero.
    assert_eq!(
        eval(&db, "i1", "CASE WHEN a.n > 0 THEN a.n ELSE a.n / 0 END"),
        int(7)
    );
    assert_eq!(
        eval(&db, "i1", "CASE WHEN a.n = 0 THEN a.n / 0 END"),
        SqlValue::Null
    );
    // In a predicate.
    assert_eq!(
        filter(&db, "CASE WHEN a.n >= 0 THEN a.x < 1 ELSE FALSE END"),
        ["i3"]
    );
    // A condition is a boolean.
    let error = raises(&db, "i1", "CASE WHEN a.n THEN 1 END");
    assert!(error.contains("a condition"), "{error}");
}

// ── errors that are never NULL (Q14) ──────────────────────────────────────

#[test]
fn ln_of_zero_or_a_negative_number_raises_2201e() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    assert_eq!(eval(&db, "i1", "ln(1)"), float(0.0));
    assert_eq!(eval(&db, "i1", "ln(exp(0))"), float(0.0));
    for (key, expr) in [
        ("i1", "ln(0)"),
        ("i1", "ln(-1)"),
        ("i3", "ln(a.n)"),
        ("i2", "ln(a.x)"),
    ] {
        let error = raises(&db, key, expr);
        assert!(error.contains("2201E"), "`{expr}`: {error}");
        assert!(error.contains("logarithm"), "`{expr}`: {error}");
    }
    // Inside a predicate the error is raised, not read as unknown.
    let error = run_with(
        &db,
        "MATCH (a IS item WHERE a.n > -100) WHERE ln(a.n) > 0 RETURN a._key",
        &[],
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("2201E"), "{error}");
}

#[test]
fn division_and_modulo_by_zero_raise_22012() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    for (key, expr) in [
        ("i1", "1 / 0"),
        ("i1", "a.n / 0"),
        ("i1", "a.n % 0"),
        ("i1", "a.n / a.n - 1 / 0"),
        ("i3", "1 / a.n"),
        ("i1", "1.5 / 0"),
        ("i3", "a.x / a.x"),
        ("i1", "a.x % 0"),
    ] {
        let error = raises(&db, key, expr);
        assert!(error.contains("22012"), "`{expr}`: {error}");
        assert!(error.contains("division by zero"), "`{expr}`: {error}");
    }
}

// ── arithmetic ────────────────────────────────────────────────────────────

#[test]
fn integer_arithmetic_stays_integer_and_truncates() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    for (expr, want) in [
        ("7 / 2", int(3)),
        ("-7 / 2", int(-3)),
        ("a.n / 2", int(3)),
        ("7 % 3", int(1)),
        ("-7 % 3", int(-1)),
        ("7 % -3", int(1)),
        ("a.n * a.n - 1", int(48)),
        ("-a.n", int(-7)),
        ("7 / 2.0", float(3.5)),
        ("7.0 / 2", float(3.5)),
        ("a.n + a.x", float(9.5)),
        ("a.x * 2", float(5.0)),
        ("5.5 % 2", float(1.5)),
        ("2 ^ 10", float(1024.0)),
        ("abs(-3)", int(3)),
        ("abs(-2.5)", float(2.5)),
        ("abs(a.n)", int(7)),
        ("sqrt(16)", float(4.0)),
        ("power(2, 3)", float(8.0)),
        ("power(2.0, -1)", float(0.5)),
        ("exp(0)", float(1.0)),
    ] {
        assert_eq!(eval(&db, "i1", expr), want, "`{expr}`");
    }
    assert_eq!(eval(&db, "i2", "abs(a.n)"), int(7));
    assert_eq!(eval(&db, "i2", "a.n / 2"), int(-3));
    assert_eq!(eval(&db, "i2", "a.n % 2"), int(-1));
    // Overflow is an error, never a wrap and never a float.
    let max = [Param::Int(i64::MAX)];
    let min = [Param::Int(i64::MIN)];
    for (expr, params) in [
        ("$1 + 1", &max),
        ("$1 * 2", &max),
        ("0 - $1 - 2", &max),
        ("-$1", &min),
        ("abs($1)", &min),
        ("$1 / -1", &min),
    ] {
        let error = raises_with(&db, "i1", expr, params);
        assert!(error.contains("22003"), "`{expr}`: {error}");
        assert!(error.contains("out of range"), "`{expr}`: {error}");
    }
    // PostgreSQL: the one remainder that would overflow is 0.
    assert_eq!(eval_with(&db, "i1", "$1 % -1", &min).unwrap(), int(0));
    for expr in ["1e300 * 1e300", "exp(1000)", "power(10, 400)"] {
        let error = raises(&db, "i1", expr);
        assert!(error.contains("22003"), "`{expr}`: {error}");
    }
    // The domain errors of the other functions.
    for expr in ["sqrt(-1)", "power(0, -1)", "power(-8, 0.5)"] {
        let error = raises(&db, "i1", expr);
        assert!(error.contains("2201F"), "`{expr}`: {error}");
    }
    // In a predicate, per row.
    assert_eq!(filter(&db, "a.n * 2 + 1 > 0"), ["i1", "i3"]);
    assert_eq!(
        keys(
            &db,
            "MATCH (a IS item WHERE a.n > -100 AND a.n % 2 = 0) RETURN a._key"
        ),
        ["i3"]
    );
}

// ── casts ─────────────────────────────────────────────────────────────────

#[test]
fn casts_to_the_declared_types() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // A DATE or TIMESTAMPTZ result prints as ISO-8601 text, as a declared
    // column of that type does in a collection SELECT.
    for (expr, want) in [
        ("CAST('42' AS INT)", int(42)),
        ("'42'::bigint", int(42)),
        ("' -42 '::integer", int(-42)),
        ("2.5::int", int(3)),
        ("-2.5::int", int(-3)),
        ("3.4::int", int(3)),
        ("TRUE::int", int(1)),
        ("a.x::int", int(3)),
        ("a.n::text", text("7")),
        ("1.5::text", text("1.5")),
        ("a.flag::text", text("true")),
        ("'x'::text", text("x")),
        ("7::real", float(7.0)),
        ("'1.5'::double precision", float(1.5)),
        ("CAST(a.n AS REAL)", float(7.0)),
        ("'yes'::boolean", boolean(true)),
        ("' Off '::bool", boolean(false)),
        ("'t'::boolean", boolean(true)),
        ("0::boolean", boolean(false)),
        ("a.n::boolean", boolean(true)),
        (
            "'{\"k\": [1, 2]}'::jsonb",
            SqlValue::Json(json!({"k": [1, 2]})),
        ),
        ("'2001-02-03'::date", text("2001-02-03")),
        ("'2001-02-03T04:05:06Z'::timestamptz", text("2001-02-03T04:05:06Z")),
        ("'2001-02-03 04:05:06'::timestamp", text("2001-02-03T04:05:06Z")),
        ("'2001-02-03T04:05:06Z'::timestamptz::date", text("2001-02-03")),
        ("CAST(NULL AS INT)", SqlValue::Null),
        ("a.n::bigint::text || '!'", text("7!")),
    ] {
        assert_eq!(eval(&db, "i1", expr), want, "`{expr}`");
    }
    // A text that does not spell the type is 22P02; a number that does not
    // fit is 22003.
    for expr in [
        "'4x'::int",
        "'1.5'::int",
        "'maybe'::boolean",
        "'{'::jsonb",
        "'x'::real",
    ] {
        let error = raises(&db, "i1", expr);
        assert!(error.contains("22P02"), "`{expr}`: {error}");
    }
    let error = raises(&db, "i1", "1e19::bigint");
    assert!(error.contains("22003"), "{error}");
    let error = raises(&db, "i1", "'2001-02-30'::date");
    assert!(error.contains("2001-02-30"), "{error}");
    // A cast with no meaning between the two kinds is an error naming both.
    let error = raises(&db, "i1", "1.5::boolean");
    assert!(error.contains("boolean"), "{error}");
    // A geometry is not a value of its own: it is refused when the text is
    // compiled, naming the forms that take a shape (M6-A).
    let error = run_with(&db, "MATCH (a IS item) RETURN a.name::geometry AS v", &[])
        .unwrap_err()
        .to_string();
    assert!(error.contains("not a value of its own") && error.contains("ST_DWithin"), "{error}");
}

// ── strings ───────────────────────────────────────────────────────────────

#[test]
fn string_functions_match_the_sql_row_functions() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    // Each SQL row function over a column, and the same function over the
    // bound item's property.
    let pairs = [
        ("lower(name)", "lower(a.name)"),
        ("upper(name)", "upper(a.name)"),
        ("trim(name)", "trim(a.name)"),
        ("length(name)", "length(a.name)"),
        ("substring(name, 2, 3)", "substring(a.name, 2, 3)"),
        ("substring(name FROM 3)", "substring(a.name FROM 3)"),
        ("substring(name, -1, 3)", "substring(a.name, -1, 3)"),
        ("name || '!'", "a.name || '!'"),
        ("n || name", "a.n || a.name"),
        ("concat(name, n, note)", "concat(a.name, a.n, a.note)"),
        ("upper(n)", "upper(a.n)"),
    ];
    let sql = format!(
        "SELECT _key, {} FROM item",
        pairs
            .iter()
            .map(|(sql, _)| *sql)
            .collect::<Vec<_>>()
            .join(", ")
    );
    let SqlResult::Rows { rows, .. } = db.sql(&sql, &[]).unwrap() else {
        panic!("`{sql}` returned no rows");
    };
    let mut sql_rows: Vec<Vec<SqlValue>> = rows.into_iter().map(|row| row.values).collect();
    sql_rows.sort_by(|a, b| format!("{:?}", a[0]).cmp(&format!("{:?}", b[0])));
    assert_eq!(sql_rows.len(), 3);

    let body = format!(
        "MATCH (a IS item) RETURN a._key AS k, {}",
        pairs
            .iter()
            .enumerate()
            .map(|(at, (_, gql))| format!("{gql} AS c{at}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let answer = run_with(&db, &body, &[]).unwrap_or_else(|error| panic!("`{body}`: {error}"));
    let mut gql_rows = answer.rows.clone();
    gql_rows.sort_by(|a, b| format!("{:?}", a[0]).cmp(&format!("{:?}", b[0])));
    assert_eq!(gql_rows, sql_rows, "`{body}`");
    // And the values are the ones PostgreSQL gives.
    assert_eq!(gql_rows[0][1], text("  mixed case  "));
    assert_eq!(gql_rows[0][3], text("Mixed Case"));
    assert_eq!(gql_rows[1][2], text("ÜNÏCODE"));
    assert_eq!(gql_rows[1][4], int(7));
    assert_eq!(gql_rows[1][5], text("nïc"));
    assert_eq!(gql_rows[2][7], text("a"));
}

// ── IN ────────────────────────────────────────────────────────────────────

#[test]
fn in_lists_with_null() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    for (expr, want) in [
        ("a.n IN (1, 7)", boolean(true)),
        ("a.n IN (1, 2)", boolean(false)),
        ("a.n IN (7.0)", boolean(true)),
        ("a.n IN (1, NULL)", SqlValue::Null),
        ("a.n IN (NULL, 7)", boolean(true)),
        ("a.n NOT IN (1, 2)", boolean(true)),
        ("a.n NOT IN (7, NULL)", boolean(false)),
        ("a.n NOT IN (1, NULL)", SqlValue::Null),
        ("a.note IN (1, 2)", SqlValue::Null),
        ("a.n IN (a.n - 1, a.n)", boolean(true)),
        ("a._key IN ('i2', 'i1')", boolean(true)),
    ] {
        assert_eq!(eval(&db, "i1", expr), want, "`{expr}`");
    }
    // In a filter, unknown keeps no row -- and neither does NOT unknown.
    assert_eq!(filter(&db, "a.n IN (7, 0, NULL)"), ["i1", "i3"]);
    assert_eq!(filter(&db, "a.n NOT IN (7, NULL)"), Vec::<String>::new());
    assert_eq!(filter(&db, "a.n NOT IN (7)"), ["i2", "i3"]);
    // A member of a kind the value does not compare with is an error.
    let error = raises(&db, "i1", "a.name IN ('x', 1)");
    assert!(error.contains("does not compare"), "{error}");
}

// ── typing ────────────────────────────────────────────────────────────────

#[test]
fn incomparable_kinds_are_an_error() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    for expr in [
        "a.name + 1",
        "a.flag * 2",
        "-a.name",
        "abs(a.name)",
        "ln('x')",
    ] {
        let error = raises(&db, "i1", expr);
        assert!(
            error.contains("text") || error.contains("boolean"),
            "`{expr}`: {error}"
        );
    }
    let error = raises(&db, "i1", "nullif(a.n, 'x')");
    assert!(error.contains("does not compare"), "{error}");
    // A node is not a value an operator or a function takes.
    let error = run_with(&db, "MATCH (a IS item) RETURN a + 1 AS v", &[])
        .unwrap_err()
        .to_string();
    assert!(error.contains("`a` is a node"), "{error}");
    let error = run_with(&db, "MATCH (a IS item) RETURN coalesce(a, a) AS v", &[])
        .unwrap_err()
        .to_string();
    assert!(error.contains("`a` is a node"), "{error}");
    // An element's null test is allowed: a bound element is never null.
    assert_eq!(eval(&db, "i1", "a IS NULL"), boolean(false));
    // `IS MISSING` stays SQL's.
    let error = run_with(
        &db,
        "MATCH (a IS item) WHERE a.note IS MISSING RETURN a._key",
        &[],
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("PROPERTY_NAMES"), "{error}");
    // A function the pack does not hold is named.
    let error = run_with(&db, "MATCH (a IS item) RETURN lowr(a.name) AS v", &[])
        .unwrap_err()
        .to_string();
    assert!(error.contains("lowr"), "{error}");
}

// ── SQLSTATEs, as data ────────────────────────────────────────────────────

/// The SQLSTATE an error carries as data, if it carries one.
fn sqlstate(error: &SqlError) -> Option<&'static str> {
    match error {
        SqlError::Coded { sqlstate, .. } => Some(sqlstate),
        _ => None,
    }
}

#[test]
fn a_runtime_error_carries_its_sqlstate_as_data() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    for (key, expr, code) in [
        ("i1", "1 / 0", "22012"),
        ("i1", "a.n % 0", "22012"),
        ("i1", "a.n * 4611686018427387904", "22003"),
        ("i1", "-(-4611686018427387904 * 2)", "22003"),
        ("i1", "CAST('x' AS BIGINT)", "22P02"),
        ("i1", "CAST('someday' AS DATE)", "22007"),
        ("i3", "ln(a.n)", "2201E"),
        ("i2", "sqrt(a.x)", "2201F"),
        ("i1", "a.name + 1", "42804"),
        ("i1", "a.name < 1", "42804"),
    ] {
        let error = eval_with(&db, key, expr, &[]).unwrap_err();
        assert_eq!(sqlstate(&error), Some(code), "`{expr}`: {error}");
    }
    // A horizontal SUM past `i64` is the same `22003`.
    let error = run_with(
        &db,
        "MATCH (a IS item WHERE a._key = 'i1') \
         LET xs = [4611686018427387904, 4611686018427387904] LET total = SUM(xs) RETURN total",
        &[],
    )
    .unwrap_err();
    assert_eq!(sqlstate(&error), Some("22003"), "{error}");
}

#[test]
fn a_binder_error_carries_its_postgresql_sqlstate() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    for (body, code) in [
        // an unknown variable: undefined_column
        ("MATCH (a IS item) RETURN b.name AS v", "42703"),
        ("MATCH (a IS item) RETURN a._key AS k NEXT RETURN a AS v", "42703"),
        ("MATCH (a IS item WHERE a.n > -100) LET x = 1, y = x RETURN y", "42703"),
        // a value of the wrong kind: datatype_mismatch
        ("MATCH (a IS item) RETURN a + 1 AS v", "42804"),
        ("MATCH (a IS item WHERE a.n > -100) WHERE a RETURN a._key", "42804"),
        ("MATCH (a IS item) RETURN PATH_LENGTH(a) AS v", "42804"),
        ("MATCH (a IS item) RETURN [1, 'x'] AS v", "42804"),
        // an unknown function, or a wrong argument count: undefined_function
        ("MATCH (a IS item) RETURN lowr(a.name) AS v", "42883"),
        ("MATCH (a IS item) RETURN abs(1, 2) AS v", "42883"),
        ("MATCH (a IS item) RETURN PATH_LENGTH(a, a) AS v", "42883"),
    ] {
        let error = run_with(&db, body, &[]).unwrap_err();
        assert_eq!(sqlstate(&error), Some(code), "`{body}`: {error}");
    }
}

#[test]
fn case_coalesce_and_nullif_over_integers_and_floats_are_double_precision() {
    // PostgreSQL resolves `bigint` and `double precision` branches to
    // `double precision`. An integer value in such a column encodes
    // exactly as a float8 (design §6.1), so the value is only checked as a
    // number.
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    for (expr, value, ty) in [
        ("coalesce(a.n, 0.5)", 7.0, "DOUBLE PRECISION"),
        ("coalesce(a.x, a.n)", 2.5, "DOUBLE PRECISION"),
        ("CASE WHEN a.flag THEN a.n ELSE a.x END", 7.0, "DOUBLE PRECISION"),
        ("CASE a.n WHEN 7 THEN 1 ELSE 2.5 END", 1.0, "DOUBLE PRECISION"),
        ("nullif(a.n, 0.5)", 7.0, "DOUBLE PRECISION"),
        ("CASE WHEN a.flag THEN 1 END", 1.0, "BIGINT"),
        ("nullif(a.n, 0)", 7.0, "BIGINT"),
    ] {
        let body = format!("MATCH (a IS item WHERE a._key = 'i1') RETURN {expr} AS v");
        let text = format!("SELECT * FROM GRAPH_TABLE (base {body})");
        let prepared = prepare_sql(&db, &text, &[]).unwrap();
        assert_eq!(prepared.column_type(0), Some(ty), "`{expr}`");
        let number = match eval(&db, "i1", expr) {
            SqlValue::Int(i) => i as f64,
            SqlValue::Float(f) => f,
            other => panic!("`{expr}` gave {other:?}"),
        };
        assert_eq!(number, value, "`{expr}`");
    }
    // Branches that share no type are still TEXT (Q8).
    let text = "SELECT * FROM GRAPH_TABLE (base MATCH (a IS item WHERE a._key = 'i1') \
                RETURN coalesce(a.name, a.n) AS v)";
    assert_eq!(prepare_sql(&db, text, &[]).unwrap().column_type(0), Some("TEXT"));
}

#[test]
fn every_lowering_position_counts_its_parameter() {
    // A `$n` read anywhere an expression is lowered is one the execution
    // must bind: the count never loses one.
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    for body in [
        "MATCH (a IS item WHERE a.n > $2) RETURN a._key",
        "MATCH (a IS item WHERE a.n > -100) WHERE a.n > $2 RETURN a._key",
        "MATCH (a IS item WHERE a.n > -100) FILTER a.n > $2 RETURN a._key",
        "MATCH (a IS item WHERE a.n > -100) LET x = a.n + $2 RETURN x",
        "MATCH (a IS item WHERE a.n > -100) FOR y IN [$2] RETURN y",
        "MATCH (a IS item WHERE a.n > -100) RETURN a.n + $2 AS v",
        "MATCH (a IS item WHERE a.n > -100) RETURN a.n + $2 AS k, COUNT(*) AS c",
        "MATCH (a IS item WHERE a.n > -100) RETURN SUM(a.n + $2) AS s",
        "MATCH (a IS item WHERE a.n > -100) RETURN a._key ORDER BY a.n + $2",
    ] {
        let text = format!("SELECT * FROM GRAPH_TABLE (base {body})");
        let prepared = prepare_sql(&db, &text, &[]).unwrap_or_else(|e| panic!("`{body}`: {e}"));
        assert_eq!(prepared.param_types().len(), 2, "`{body}`");
    }
}
