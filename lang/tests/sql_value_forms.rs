//! Values written the way a PostgreSQL user writes them (0.19.2, from
//! application dogfooding).
//!
//! What is at risk, one test each:
//!
//! * a quoted literal assigned to a JSONB column is PARSED, as PostgreSQL
//!   reads `'{"a":1}'` into jsonb, and text that is not JSON is refused by
//!   name rather than stored as a string
//!   (`a_quoted_literal_into_jsonb_is_parsed_and_bad_json_is_refused`);
//! * `now()` and `CURRENT_TIMESTAMP` are values an INSERT and an UPDATE can
//!   write, as `DEFAULT now()` already was, and a DATE column takes the day
//!   (`the_clock_is_a_value_an_insert_and_an_update_write`);
//! * `col->>'member'` in a select list reads the member from the row as
//!   PostgreSQL does -- a string as its text, a number as its spelling, an
//!   absent member and JSON null as NULL -- and over a column that is not
//!   JSONB it is refused by name
//!   (`an_extracted_member_is_a_projection`);
//! * `lower(col) = v` answers without an expression index, checked on each
//!   row, and names exactly the rows the expression index names once it is
//!   built; a `v` with an upper-case letter names none
//!   (`lower_equality_is_a_row_check_without_its_expression_index`);
//! * a LIKE inside OR is refused with a reason that does not claim the
//!   trigram index is missing (`a_like_under_or_is_refused_without_blaming_the_index`);
//! * a TEXT value too long for its column's btree is refused naming the
//!   column, the index and the two ways out, and writes nothing
//!   (`a_value_too_long_for_the_index_names_the_index_and_the_way_out`);
//! * a value a column DEFAULT fills is found through the column's index, for
//!   a DEFAULT declared with the table and one added by ALTER
//!   (`a_value_the_default_fills_is_found_through_the_index`).

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{SqlDatabase, SqlResult, SqlValue};
use serde_json::json;
use tempfile::TempDir;

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn run(db: &mut Database, sql: &str) -> SqlResult {
    db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"))
}

fn row(db: &mut Database, sql: &str) -> Vec<SqlValue> {
    match run(db, sql) {
        SqlResult::Rows { mut rows, .. } => rows.remove(0).values,
        other => panic!("{other:?}"),
    }
}

fn refusal(db: &mut Database, sql: &str) -> String {
    match db.sql(sql, &[]) {
        Ok(result) => panic!("`{sql}` answered {result:?}"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn a_quoted_literal_into_jsonb_is_parsed_and_bad_json_is_refused() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(&dir.path().join("j"), cfg()).unwrap();
    run(&mut db, "CREATE TABLE posts (_key TEXT PRIMARY KEY, meta JSONB)");
    run(&mut db, r#"INSERT INTO posts (_key, meta) VALUES ('a', '{"lang": "en", "n": 1}')"#);
    run(&mut db, r#"INSERT INTO posts (_key, meta) VALUES ('b', '"just text"')"#);
    run(&mut db, "INSERT INTO posts (_key, meta) VALUES ('c', '[1, 2]')");
    assert_eq!(
        row(&mut db, "SELECT meta FROM posts WHERE _key = 'a'"),
        [SqlValue::Json(json!({"lang": "en", "n": 1}))]
    );
    assert_eq!(row(&mut db, "SELECT meta FROM posts WHERE _key = 'b'"), [SqlValue::Text("just text".into())]);
    assert_eq!(row(&mut db, "SELECT meta FROM posts WHERE _key = 'c'"), [SqlValue::Json(json!([1, 2]))]);

    run(&mut db, r#"UPDATE posts SET meta = '{"lang": "id"}' WHERE _key = 'b'"#);
    assert_eq!(
        row(&mut db, "SELECT meta FROM posts WHERE _key = 'b'"),
        [SqlValue::Json(json!({"lang": "id"}))]
    );

    let message = refusal(&mut db, "INSERT INTO posts (_key, meta) VALUES ('d', '{lang: en}')");
    assert!(message.contains("meta") && message.contains("JSON"), "{message}");
    let message = refusal(&mut db, "INSERT INTO posts (_key, meta) VALUES ('d', 'plain words')");
    assert!(message.contains("meta") && message.contains("JSON"), "{message}");
}

/// A TIMESTAMPTZ or DATE as SELECT prints it (`2026-01-02T03:04:05.123456Z`
/// or `2026-01-02`), as microseconds since the epoch.
fn micros(value: &SqlValue) -> i64 {
    let SqlValue::Text(text) = value else { panic!("a printed timestamp, not {value:?}") };
    let (y, m, d): (i64, i64, i64) = (text[0..4].parse().unwrap(), text[5..7].parse().unwrap(), text[8..10].parse().unwrap());
    // Days from civil (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    let mut micros = days * 86_400_000_000;
    if text.len() > 10 {
        let (h, mi): (i64, i64) = (text[11..13].parse().unwrap(), text[14..16].parse().unwrap());
        let seconds: f64 = text[17..].trim_end_matches('Z').parse().unwrap();
        micros += (h * 3600 + mi * 60) * 1_000_000 + (seconds * 1e6).round() as i64;
    }
    micros
}

fn wall_micros() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_micros() as i64
}

#[test]
fn the_clock_is_a_value_an_insert_and_an_update_write() {
    const DAY: i64 = 86_400_000_000;
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(&dir.path().join("t"), cfg()).unwrap();
    run(
        &mut db,
        "CREATE TABLE contacts (_key TEXT PRIMARY KEY, created_at TIMESTAMPTZ, updated_at TIMESTAMPTZ, seen DATE)",
    );
    let before = wall_micros();
    run(&mut db, "INSERT INTO contacts (_key, created_at, seen) VALUES ('a', now(), CURRENT_DATE)");
    run(&mut db, "INSERT INTO contacts (_key, created_at, seen) VALUES ('b', CURRENT_TIMESTAMP - interval '1 day', now())");
    run(&mut db, "UPDATE contacts SET updated_at = now() WHERE _key = 'a'");
    let after = wall_micros();

    let a = row(&mut db, "SELECT created_at, updated_at, seen FROM contacts WHERE _key = 'a'");
    assert!((before..=after).contains(&micros(&a[0])), "{a:?}");
    assert!((before..=after).contains(&micros(&a[1])), "{a:?}");
    assert_eq!(micros(&a[2]) % DAY, 0, "a DATE is midnight UTC");
    let b = row(&mut db, "SELECT created_at, seen FROM contacts WHERE _key = 'b'");
    assert!((before - DAY..=after - DAY).contains(&micros(&b[0])), "{b:?}");
    assert_eq!(micros(&b[1]) % DAY, 0, "now() into a DATE is its day");
}

#[test]
fn an_extracted_member_is_a_projection() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(&dir.path().join("p"), cfg()).unwrap();
    run(&mut db, "CREATE TABLE posts (_key TEXT PRIMARY KEY, title TEXT, meta JSONB)");
    run(
        &mut db,
        r#"INSERT INTO posts (_key, title, meta) VALUES ('a', 'One', '{"lang": "en", "n": 3, "tags": ["x"], "gone": null}')"#,
    );
    run(&mut db, "INSERT INTO posts (_key, title) VALUES ('b', 'Two')");
    assert_eq!(
        row(
            &mut db,
            "SELECT _key, meta->>'lang' AS lang, meta->>'n', meta->>'tags', meta->>'gone', meta->>'absent' FROM posts WHERE _key = 'a'"
        ),
        [
            SqlValue::Text("a".into()),
            SqlValue::Text("en".into()),
            SqlValue::Text("3".into()),
            SqlValue::Text(r#"["x"]"#.into()),
            SqlValue::Null,
            SqlValue::Null,
        ]
    );
    assert_eq!(
        row(&mut db, "SELECT meta->>'lang' || '/' || title FROM posts WHERE _key = 'a'"),
        [SqlValue::Text("en/One".into())]
    );
    assert_eq!(row(&mut db, "SELECT meta->>'lang' FROM posts WHERE _key = 'b'"), [SqlValue::Null]);
    let message = refusal(&mut db, "SELECT title->>'x' FROM posts WHERE _key = 'a'");
    assert!(message.contains("JSONB"), "{message}");
}

fn keys(db: &mut Database, sql: &str) -> Vec<String> {
    match run(db, sql) {
        SqlResult::Rows { rows, .. } => {
            let mut keys: Vec<String> = rows
                .into_iter()
                .map(|r| match &r.values[0] {
                    SqlValue::Text(t) => t.clone(),
                    other => panic!("{other:?}"),
                })
                .collect();
            keys.sort();
            keys
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn lower_equality_is_a_row_check_without_its_expression_index() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(&dir.path().join("l"), cfg()).unwrap();
    run(&mut db, "CREATE TABLE contacts (_key TEXT PRIMARY KEY, email TEXT)");
    for (key, email) in [
        ("a", "Ada@Example.com"),
        ("b", "ada@example.com"),
        ("c", "ben@example.com"),
        ("d", "ada_x@example.com"),
        ("e", "adaXx@example.com"),
    ] {
        run(&mut db, &format!("INSERT INTO contacts (_key, email) VALUES ('{key}', '{email}')"));
    }
    run(&mut db, "INSERT INTO contacts (_key) VALUES ('f')");
    let cases = [
        ("SELECT _key FROM contacts WHERE LOWER(email) = 'ada@example.com'", vec!["a", "b"]),
        ("SELECT _key FROM contacts WHERE lower(email) = 'ada_x@example.com'", vec!["d"]),
        ("SELECT _key FROM contacts WHERE lower(email) = 'Ada@example.com'", vec![]),
        ("SELECT _key FROM contacts WHERE lower(email) = 'nobody@example.com'", vec![]),
    ];
    for (sql, expected) in &cases {
        assert_eq!(keys(&mut db, sql), *expected, "{sql}");
    }
    let bound = match db.sql(
        "SELECT _key FROM contacts WHERE lower(email) = $1",
        &[sekejap_lang::Param::Text("ben@example.com".into())],
    ) {
        Ok(SqlResult::Rows { rows, .. }) => rows.len(),
        other => panic!("{other:?}"),
    };
    assert_eq!(bound, 1);

    run(&mut db, "CREATE INDEX contacts_email_lower ON contacts (lower(email))");
    for (sql, expected) in &cases {
        assert_eq!(keys(&mut db, sql), *expected, "with the expression index: {sql}");
    }
}

#[test]
fn a_like_under_or_is_refused_without_blaming_the_index() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(&dir.path().join("o"), cfg()).unwrap();
    run(&mut db, "CREATE TABLE people (_key TEXT PRIMARY KEY, name TEXT)");
    run(&mut db, "CREATE INDEX ON people USING gin (name gin_trgm_ops)");
    run(&mut db, "INSERT INTO people (_key, name) VALUES ('a', 'Hanna')");
    assert_eq!(keys(&mut db, "SELECT _key FROM people WHERE name ILIKE '%nna%'"), ["a"]);
    let message = refusal(&mut db, "SELECT _key FROM people WHERE name ILIKE '%nna%' OR name ILIKE '%obb%'");
    assert!(!message.contains("has no index"), "{message}");
    assert!(message.contains("trigram index"), "{message}");
}

#[test]
fn a_value_too_long_for_the_index_names_the_index_and_the_way_out() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(&dir.path().join("b"), cfg()).unwrap();
    run(&mut db, "CREATE TABLE profiles (_key TEXT PRIMARY KEY, bio TEXT)");
    let long = "a".repeat(1_500);
    let message = refusal(&mut db, &format!("INSERT INTO profiles (_key, bio) VALUES ('p', '{long}')"));
    for needle in ["bio", "profiles_bio_btree", "1024", "DROP INDEX", "WITH (index:"] {
        assert!(message.contains(needle), "`{needle}` missing from: {message}");
    }
    // A raw handle waits for the caller's ROLLBACK after a failed write, so
    // nothing half-written can be committed (`sekejap::Db` rolls back itself).
    run(&mut db, "ROLLBACK");
    assert_eq!(keys(&mut db, "SELECT _key FROM profiles WHERE _key = 'p'"), Vec::<String>::new());
    // The way out the message names works.
    run(&mut db, "DROP INDEX profiles_bio_btree");
    run(&mut db, &format!("INSERT INTO profiles (_key, bio) VALUES ('p', '{long}')"));
    assert_eq!(row(&mut db, "SELECT bio FROM profiles WHERE _key = 'p'"), [SqlValue::Text(long)]);
}

#[test]
fn a_value_the_default_fills_is_found_through_the_index() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(&dir.path().join("d"), cfg()).unwrap();
    run(&mut db, "CREATE TABLE pass (_key TEXT PRIMARY KEY, owner TEXT, tier TEXT DEFAULT 'basic')");
    run(&mut db, "INSERT INTO pass (_key, owner) VALUES ('a', 'Made')");
    run(&mut db, "INSERT INTO pass (_key, owner, tier) VALUES ('b', 'Putu', 'gold')");
    assert_eq!(keys(&mut db, "SELECT _key FROM pass WHERE tier = 'basic'"), ["a"], "declared with the table");

    run(&mut db, "CREATE TABLE later (_key TEXT PRIMARY KEY, owner TEXT)");
    run(&mut db, "INSERT INTO later (_key, owner) VALUES ('old', 'Made')");
    run(&mut db, "ALTER TABLE later ADD COLUMN tier TEXT DEFAULT 'basic'");
    run(&mut db, "INSERT INTO later (_key, owner) VALUES ('new', 'Wayan')");
    assert_eq!(
        row(&mut db, "SELECT tier FROM later WHERE _key = 'new'"),
        [SqlValue::Text("basic".into())]
    );
    assert_eq!(keys(&mut db, "SELECT _key FROM later WHERE tier = 'basic'"), ["new", "old"], "added later");
}
