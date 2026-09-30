//! The statements a web application sends, end to end through `sekejap::Db`
//! in service mode, as one application's first weeks would send them.
//!
//! Each shape here was once a page that broke or a workaround an application
//! had to carry. What is at risk, one test each:
//!
//! * a first start: the folder the application made is empty, the upgrade
//!   call leaves it alone, and the store opens (`a_first_start_on_an_empty_folder`);
//! * a day of page traffic -- sign-in by e-mail in any case, a search box, a
//!   feed paged by keyset, a JSON field read in the select list, an upsert,
//!   timestamps written with `now()`, a follower count through the graph --
//!   answers what the rows say, before and after a reopen
//!   (`a_day_of_page_traffic_answers_what_the_rows_say`);
//! * a statement that fails -- a taken key, bad JSON, a value too long for an
//!   index -- fails alone: the handle keeps serving and nothing it half-wrote
//!   is kept (`a_failed_statement_fails_alone`).
//!
//! Tables and values are neutral; no application's data is copied here.

use sekejap::{Db, SqlValue};
use serde_json::{json, Value};

fn text(v: &SqlValue) -> String {
    match v {
        SqlValue::Text(t) => t.clone(),
        other => panic!("text, not {other:?}"),
    }
}

/// The first column of every row, in answer order.
fn column(db: &Db, sql: &str, params: &[Value]) -> Vec<String> {
    db.query(sql, params)
        .unwrap_or_else(|e| panic!("`{sql}`: {e}"))
        .iter()
        .map(|r| text(&r.values[0]))
        .collect()
}

fn count(db: &Db, sql: &str, params: &[Value]) -> i64 {
    match db.query(sql, params).unwrap_or_else(|e| panic!("`{sql}`: {e}")).rows[0].values[0] {
        SqlValue::Int(n) => n,
        ref other => panic!("a count, not {other:?}"),
    }
}

fn exec(db: &Db, sql: &str) {
    db.execute(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
}

const SCHEMA: &[&str] = &[
    "CREATE TABLE member (_key TEXT PRIMARY KEY, email TEXT, name TEXT, joined TIMESTAMPTZ DEFAULT now(), prefs JSONB)",
    "CREATE TABLE post (_key TEXT PRIMARY KEY, author TEXT, title TEXT, at TIMESTAMPTZ, meta JSONB)",
    "CREATE INDEX ON post USING gin (title gin_trgm_ops)",
    "CREATE TABLE follows (follower TEXT REFERENCES member, followee TEXT REFERENCES member, PRIMARY KEY (follower, followee))",
    "ALTER PROPERTY GRAPH base ADD EDGE TABLES (follows SOURCE KEY (follower) REFERENCES member (_key) DESTINATION KEY (followee) REFERENCES member (_key))",
];

fn seed(db: &Db) {
    for sql in SCHEMA {
        exec(db, sql);
    }
    for (key, email, name) in [
        ("m1", "Ada.Wright@Example.com", "Ada"),
        ("m2", "ben@example.com", "Ben"),
        ("m3", "cleo@example.com", "Cleo"),
        ("m4", "dan@example.com", "Dan"),
    ] {
        db.execute(
            "INSERT INTO member (_key, email, name, prefs) VALUES ($1, $2, $3, '{\"lang\": \"en\", \"digest\": true}')",
            &[json!(key), json!(email), json!(name)],
        )
        .unwrap();
    }
    for (i, (author, title)) in [
        ("m1", "Morning walk by the lagoon"),
        ("m2", "Rice terraces at dawn"),
        ("m1", "A quiet lagoon cafe"),
        ("m3", "Temple steps and sunset"),
        ("m2", "Lagoon kayaking for beginners"),
        ("m4", "Market day notes"),
        ("m3", "Night market food list"),
    ]
    .iter()
    .enumerate()
    {
        db.execute(
            "INSERT INTO post (_key, author, title, at, meta) VALUES ($1, $2, $3, $4, $5)",
            &[
                json!(format!("p{i}")),
                json!(author),
                json!(title),
                json!(format!("2026-01-{:02}T08:00:00Z", 10 + i)),
                json!({"lang": if i % 2 == 0 { "en" } else { "id" }}),
            ],
        )
        .unwrap();
    }
    for (a, b) in [("m2", "m1"), ("m3", "m1"), ("m4", "m1"), ("m1", "m2")] {
        db.execute("INSERT INTO follows (follower, followee) VALUES ($1, $2)", &[json!(a), json!(b)])
            .unwrap();
    }
}

#[test]
fn a_first_start_on_an_empty_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    std::fs::create_dir_all(&store).unwrap();
    assert_eq!(Db::upgrade(&store).unwrap(), None, "an empty folder has nothing to upgrade");
    let db = Db::open_service(&store).unwrap();
    exec(&db, "CREATE TABLE member (_key TEXT PRIMARY KEY, email TEXT)");
    db.close().unwrap();
    assert_eq!(Db::upgrade(&store).unwrap(), None, "a current store has nothing to upgrade");
    Db::open_service(&store).unwrap().close().unwrap();
}

fn page_traffic(db: &Db) {
    // Sign-in: the address as typed, folded by the page.
    let typed = "ada.wright@example.com";
    assert_eq!(column(db, "SELECT _key FROM member WHERE lower(email) = $1", &[json!(typed)]), ["m1"]);
    assert_eq!(column(db, "SELECT _key FROM member WHERE lower(email) = $1", &[json!("nobody@example.com")]), Vec::<String>::new());

    // Search box over one field, any case.
    assert_eq!(
        column(db, "SELECT _key FROM post WHERE title ILIKE $1 ORDER BY _key", &[json!("%lagoon%")]),
        ["p0", "p2", "p4"]
    );

    // Feed, newest first, three at a time, the next page by keyset.
    let first = db.query("SELECT _key, at FROM post ORDER BY at DESC, _key DESC LIMIT 3", &[]).unwrap();
    let keys: Vec<String> = first.iter().map(|r| text(&r.values[0])).collect();
    assert_eq!(keys, ["p6", "p5", "p4"]);
    let last = first.rows.last().unwrap();
    let next = column(
        db,
        "SELECT _key FROM post WHERE (at, _key) < ($1, $2) ORDER BY at DESC, _key DESC LIMIT 3",
        &[json!(text(&last.values[1])), json!(text(&last.values[0]))],
    );
    assert_eq!(next, ["p3", "p2", "p1"]);

    // A JSON field in the select list.
    let rows = db.query("SELECT _key, meta->>'lang' AS lang FROM post WHERE author = 'm2' ORDER BY _key", &[]).unwrap();
    let langs: Vec<(String, String)> = rows.iter().map(|r| (text(&r.values[0]), text(&r.values[1]))).collect();
    assert_eq!(langs, [("p1".to_owned(), "id".to_owned()), ("p4".to_owned(), "en".to_owned())]);
    assert_eq!(column(db, "SELECT prefs->>'lang' FROM member WHERE _key = 'm3'", &[]), ["en"]);

    // Followers of one member, counted through the graph.
    assert_eq!(
        count(
            db,
            "SELECT count(*) FROM GRAPH_TABLE (base MATCH (f:member)-[:follows]->(m:member WHERE m._key = 'm1') RETURN f._key AS who)",
            &[]
        ),
        3
    );
}

#[test]
fn a_day_of_page_traffic_answers_what_the_rows_say() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let db = Db::open_service(&store).unwrap();
    seed(&db);
    page_traffic(&db);

    // An upsert: a profile saved twice.
    for name in ["Ben", "Benjamin"] {
        db.execute(
            "INSERT INTO member (_key, email, name) VALUES ('m2', 'ben@example.com', $1) ON CONFLICT (_key) DO UPDATE SET name = EXCLUDED.name",
            &[json!(name)],
        )
        .unwrap();
    }
    assert_eq!(column(&db, "SELECT name FROM member WHERE _key = 'm2'", &[]), ["Benjamin"]);

    // Timestamps written with now(); the joined DEFAULT filled at insert.
    exec(&db, "INSERT INTO post (_key, author, title, at) VALUES ('p9', 'm4', 'Fresh post', now())");
    exec(&db, "UPDATE member SET joined = now() WHERE _key = 'm4'");
    assert_eq!(column(&db, "SELECT _key FROM post ORDER BY at DESC, _key DESC LIMIT 1", &[]), ["p9"]);
    assert_eq!(count(&db, "SELECT count(*) FROM member WHERE joined > '2026-01-01T00:00:00Z'", &[]), 4);

    // The fresh post is taken down again, so the feed reads as before.
    exec(&db, "DELETE FROM post WHERE _key = 'p9'");

    db.close().unwrap();
    let db = Db::open_service(&store).unwrap();
    page_traffic(&db);
    assert_eq!(column(&db, "SELECT name FROM member WHERE _key = 'm2'", &[]), ["Benjamin"]);
}

#[test]
fn a_failed_statement_fails_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open_service(tmp.path().join("store")).unwrap();
    seed(&db);
    let before = count(&db, "SELECT count(*) FROM post", &[]);

    for (sql, says) in [
        ("INSERT INTO post (_key, title) VALUES ('p1', 'duplicate')", "23505"),
        ("INSERT INTO post (_key, meta) VALUES ('px', '{not json}')", "JSON"),
        ("INSERT INTO member (_key, name) VALUES ('mx', $1)", "DROP INDEX"),
    ] {
        let params = if sql.contains("$1") { vec![json!("x".repeat(2_000))] } else { vec![] };
        let error = db.execute(sql, &params).expect_err(sql).to_string();
        assert!(error.contains(says), "`{sql}` said: {error}");
        // The handle keeps serving, and the failed statement left nothing.
        assert_eq!(count(&db, "SELECT count(*) FROM post", &[]), before, "after `{sql}`");
        assert_eq!(column(&db, "SELECT _key FROM member WHERE _key = 'mx'", &[]), Vec::<String>::new());
        exec(&db, "UPDATE post SET title = 'Rice terraces at dawn' WHERE _key = 'p1'");
    }

    // A long bio is stored once the column's btree is dropped, as the
    // refusal said.
    exec(&db, "DROP INDEX member_name_btree");
    let bio = "long profile text ".repeat(300);
    db.execute("INSERT INTO member (_key, name) VALUES ('mx', $1)", &[json!(bio)]).unwrap();
    assert_eq!(column(&db, "SELECT name FROM member WHERE _key = 'mx'", &[]), [bio]);
}
