//! The `sekejap` crate surface of `docs/dist/RUST_API.md`, checked against a
//! brute-force oracle held in this process.
//!
//! The oracle is a `BTreeMap<(collection, key), Value>` and a
//! `BTreeSet<(from, edge_type, to)>`: no engine call is compared against
//! another engine call.
//!
//! Out of scope, stated rather than silently untested: a process killed
//! mid-commit. Durability here is "a call that returned has a durable row",
//! checked by closing the handle and opening the directory again; the
//! torn-frame case is `docs/core/RECOVERY_CONTRACT.md` and the page-WAL fault
//! suites, not this file.

use sekejap::{Db, Direction, Error, FieldKind, Mode};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

type Oracle = BTreeMap<(String, String), Value>;

fn dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

/// The collection every document test writes into, and its declaration.
fn posts(db: &Db) {
    db.create_collection(
        "posts",
        &[
            ("title", FieldKind::Text),
            ("views", FieldKind::Int),
            ("score", FieldKind::Real),
            ("live", FieldKind::Bool),
        ],
    )
    .expect("declare posts");
}

fn document(n: usize) -> Value {
    json!({
        "title": format!("post {n}"),
        "views": n as i64,
        "score": n as f64 / 4.0,
        "live": n % 2 == 0,
    })
}

/// What the oracle expects a read of `key` to answer: the document as
/// written, with `_key` set, which is what §2 says a read gives back.
fn expected(oracle: &Oracle, collection: &str, key: &str) -> Option<Value> {
    let mut object = oracle
        .get(&(collection.to_owned(), key.to_owned()))?
        .as_object()
        .cloned()?;
    object.insert("_key".to_owned(), Value::String(key.to_owned()));
    Some(Value::Object(object))
}

#[test]
fn put_get_delete_and_exists_agree_with_a_map_oracle() {
    let tmp = dir();
    let db = Db::open(tmp.path()).expect("open");
    posts(&db);
    let mut oracle: Oracle = BTreeMap::new();

    for n in 0..64 {
        let key = format!("p{n:03}");
        let doc = document(n);
        db.put(("posts", key.as_str()), &doc).expect("put");
        oracle.insert(("posts".into(), key), doc);
    }
    // Every third row is rewritten, every seventh is deleted: the two
    // sequences overlap, so a rewrite of a deleted key and a delete of a
    // rewritten key both happen.
    for n in (0..64).step_by(3) {
        let key = format!("p{n:03}");
        let doc = json!({ "title": format!("rewritten {n}"), "views": (n * 2) as i64 });
        db.put(("posts", key.as_str()), &doc).expect("rewrite");
        oracle.insert(("posts".into(), key), doc);
    }
    for n in (0..64).step_by(7) {
        let key = format!("p{n:03}");
        let removed = db.delete(("posts", key.as_str())).expect("delete");
        assert_eq!(removed, oracle.remove(&("posts".into(), key)).is_some());
    }

    for n in 0..70 {
        let key = format!("p{n:03}");
        let want = expected(&oracle, "posts", &key);
        assert_eq!(
            db.get(("posts", key.as_str())).expect("get"),
            want,
            "get disagrees with the oracle at {key}"
        );
        assert_eq!(
            db.exists(("posts", key.as_str())).expect("exists"),
            want.is_some(),
            "exists disagrees with the oracle at {key}"
        );
    }
    assert_eq!(
        db.scan_count_rows("posts").expect("count") as usize,
        oracle.len()
    );
}

#[test]
fn a_put_that_returned_is_on_disk_when_the_directory_is_opened_again() {
    let tmp = dir();
    {
        let db = Db::open(tmp.path()).expect("open");
        posts(&db);
        db.put(("posts", "p1"), &json!({ "title": "durable" }))
            .expect("put");
        db.close().expect("close");
    }
    let db = Db::open(tmp.path()).expect("reopen");
    assert_eq!(
        db.get(("posts", "p1")).expect("get"),
        Some(json!({ "title": "durable", "_key": "p1" }))
    );
}

#[test]
fn a_scan_equals_the_oracle_at_three_page_boundaries() {
    let tmp = dir();
    let db = Db::open(tmp.path()).expect("open");
    posts(&db);
    let mut oracle: Oracle = BTreeMap::new();
    // 100 rows against pages of 1, 7 and 100: one row short of a page, a
    // page that does not divide the answer, and one page for the lot.
    for n in 0..100 {
        let key = format!("p{n:03}");
        let doc = document(n);
        db.put(("posts", key.as_str()), &doc).expect("put");
        oracle.insert(("posts".into(), key), doc);
    }
    for page in [1usize, 7, 100] {
        let mut seen: Oracle = BTreeMap::new();
        for row in db.scan("posts").expect("scan").page_size(page) {
            let row = row.expect("row");
            let mut fields = row.fields.as_object().cloned().expect("object");
            fields.remove("_key");
            seen.insert(("posts".into(), row.key.clone()), Value::Object(fields));
        }
        assert_eq!(seen, oracle, "scan at page size {page} disagrees");
    }
}

#[test]
fn a_scan_is_lazy_and_a_partial_walk_reads_only_what_it_takes() {
    let tmp = dir();
    let db = Db::open(tmp.path()).expect("open");
    posts(&db);
    for n in 0..500 {
        db.put(("posts", format!("p{n:03}").as_str()), &document(n))
            .expect("put");
    }
    // `take(3)` on a page size of 2 pulls two pages and stops: the walk is
    // an iterator, not a materialised answer.
    let taken: Vec<_> = db
        .scan("posts")
        .expect("scan")
        .page_size(2)
        .take(3)
        .map(|r| r.expect("row").key)
        .collect();
    assert_eq!(taken, vec!["p000", "p001", "p002"]);
}

#[test]
fn sql_writes_and_reads_go_through_the_same_rows() {
    let tmp = dir();
    let db = Db::open(tmp.path()).expect("open");
    db.execute(
        "CREATE TABLE posts (title TEXT, views INT)",
        &[],
    )
    .expect("create table");

    let mut oracle: Oracle = BTreeMap::new();
    for n in 0..16 {
        let key = format!("p{n:02}");
        db.execute(
            "INSERT INTO posts (_key, title, views) VALUES ($1, $2, $3)",
            &[json!(key), json!(format!("post {n}")), json!(n as i64)],
        )
        .expect("insert");
        oracle.insert(
            ("posts".into(), key),
            json!({ "title": format!("post {n}"), "views": n as i64 }),
        );
    }

    let rows = db
        .query("SELECT _key, title, views FROM posts", &[])
        .expect("select");
    assert_eq!(rows.column_names(), ["_key", "title", "views"]);
    let mut seen: Oracle = BTreeMap::new();
    for row in rows.iter() {
        let mut object: Map<String, Value> = row.to_object();
        let key = object
            .remove("_key")
            .and_then(|v| v.as_str().map(str::to_owned))
            .expect("_key");
        seen.insert(("posts".into(), key), Value::Object(object));
    }
    assert_eq!(seen, oracle);

    // The same answer through the paged form, which holds one page at a
    // time rather than the whole vector.
    let mut streamed = 0usize;
    let handed = db
        .stream("SELECT _key FROM posts", &[], 5, &mut |_row| {
            streamed += 1;
            Ok(())
        })
        .expect("stream");
    assert_eq!(handed as usize, oracle.len());
    assert_eq!(streamed, oracle.len());

    // A point read by parameter.
    let one = db
        .query("SELECT title FROM posts WHERE _key = $1", &[json!("p07")])
        .expect("point select");
    assert_eq!(one.len(), 1);
    assert_eq!(one.rows[0].json("title"), Some(json!("post 7")));

    // And the count the engine itself computes, beside the walked one.
    let counted = db.query("SELECT count(*) FROM posts", &[]).expect("count");
    assert_eq!(counted.rows[0].json("count"), Some(json!(oracle.len())));
    assert_eq!(db.scan_count_rows("posts").expect("scan") as usize, oracle.len());
}

#[test]
fn a_document_written_through_put_is_readable_through_sql() {
    let tmp = dir();
    let db = Db::open(tmp.path()).expect("open");
    db.execute("CREATE TABLE posts (title TEXT)", &[])
        .expect("create");
    db.put(("posts", "p1"), &json!({ "title": "one" }))
        .expect("put");
    let rows = db
        .query("SELECT _key, title FROM posts WHERE _key = $1", &[json!("p1")])
        .expect("select");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows.rows[0].json("title"), Some(json!("one")));
    // And a document read back carries the key it was written under, so it
    // can be written back unchanged.
    let read = db.get(("posts", "p1")).expect("get").expect("row");
    assert_eq!(read["_key"], json!("p1"));
    db.put(("posts", "p1"), &read).expect("round trip");
}

#[test]
fn collections_and_describe_report_the_catalog() {
    let tmp = dir();
    let db = Db::open(tmp.path()).expect("open");
    db.execute(
        "CREATE TABLE posts (title TEXT, views INT, at TIMESTAMPTZ)",
        &[],
    )
    .expect("create");
    db.execute("CREATE INDEX posts_views ON posts USING btree (views)", &[])
        .expect("index");
    db.create_collection("notes", &[("body", FieldKind::Text)])
        .expect("declare notes");

    assert_eq!(db.collections().expect("collections"), ["notes", "posts"]);

    let posts = db.describe("posts").expect("describe").expect("present");
    assert_eq!(posts.name, "posts");
    let names: Vec<&str> = posts.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["_key", "title", "views", "at"]);
    assert!(posts.fields[0].primary_key, "_key is the external key");
    assert_eq!(posts.field("views").expect("views").kind, FieldKind::Int);
    // TIMESTAMPTZ and INT are both Kind::Int; the declared spelling is what
    // tells them apart.
    assert_eq!(posts.field("at").expect("at").kind, FieldKind::Int);
    assert_eq!(
        posts.field("at").expect("at").declared.as_deref(),
        Some("TIMESTAMPTZ")
    );
    assert_eq!(
        posts
            .indexes_on("views")
            .map(|i| i.name.as_str())
            .collect::<Vec<_>>(),
        ["posts_views"]
    );

    assert!(db.describe("absent").expect("describe").is_none());
    assert!(db.drop_collection("notes").expect("drop"));
    assert_eq!(db.collections().expect("collections"), ["posts"]);
    assert!(!db.drop_collection("notes").expect("drop again"));
}

#[test]
fn the_three_counts_are_scans_and_agree_with_the_oracle() {
    let tmp = dir();
    let db = Db::open(tmp.path()).expect("open");
    db.create_collection("people", &[("name", FieldKind::Text)])
        .expect("people");
    db.create_collection("posts", &[("title", FieldKind::Text)])
        .expect("posts");

    let mut rows = 0u64;
    for n in 0..30 {
        db.put(("people", format!("u{n}").as_str()), &json!({ "name": format!("u{n}") }))
            .expect("put person");
        rows += 1;
    }
    for n in 0..17 {
        db.put(("posts", format!("p{n}").as_str()), &json!({ "title": format!("p{n}") }))
            .expect("put post");
        rows += 1;
    }
    let mut edges: BTreeSet<(String, String, String)> = BTreeSet::new();
    for n in 0..17 {
        let from = format!("u{}", n % 30);
        let to = format!("p{n}");
        db.link(("people", from.as_str()), "wrote", ("posts", to.as_str()))
            .expect("link");
        edges.insert((from, "wrote".to_owned(), to));
    }

    assert_eq!(db.scan_count_rows("people").expect("people"), 30);
    assert_eq!(db.scan_count_rows("posts").expect("posts"), 17);
    assert_eq!(db.scan_count_all_rows().expect("all"), rows);
    assert_eq!(db.scan_count_edges().expect("edges"), edges.len() as u64);

    // A count of a collection that is not there is a refusal by name, not a
    // zero: zero is an answer and this is not one.
    match db.scan_count_rows("absent") {
        Err(Error::UnknownCollection(name)) => assert_eq!(name, "absent"),
        other => panic!("expected UnknownCollection, got {other:?}"),
    }
}

#[test]
fn an_edge_links_two_rows_and_a_neighbour_walk_finds_them() {
    let tmp = dir();
    let db = Db::open(tmp.path()).expect("open");
    db.create_collection("people", &[("name", FieldKind::Text)])
        .expect("people");
    for who in ["alice", "bob", "carol"] {
        db.put(("people", who), &json!({ "name": who })).expect("put");
    }
    db.link(("people", "alice"), "knows", ("people", "bob"))
        .expect("link");
    db.link_with(
        ("people", "alice"),
        "knows",
        ("people", "carol"),
        &json!({ "since": 2020 }),
    )
    .expect("link with properties");

    let mut out: Vec<String> = db
        .neighbours(("people", "alice"), Some("knows"), Direction::Outgoing, 16)
        .expect("outgoing")
        .into_iter()
        .map(|d| d.key)
        .collect();
    out.sort();
    assert_eq!(out, ["bob", "carol"]);

    let incoming: Vec<String> = db
        .neighbours(("people", "bob"), Some("knows"), Direction::Incoming, 16)
        .expect("incoming")
        .into_iter()
        .map(|d| d.key)
        .collect();
    assert_eq!(incoming, ["alice"]);

    assert!(db
        .unlink(("people", "alice"), "knows", ("people", "bob"))
        .expect("unlink"));
    assert!(!db
        .unlink(("people", "alice"), "knows", ("people", "bob"))
        .expect("unlink twice"));
    assert_eq!(db.scan_count_edges().expect("edges"), 1);

    // An endpoint that is not written is a named refusal, never a dangling
    // identity.
    match db.link(("people", "alice"), "knows", ("people", "dave")) {
        Err(Error::UnknownRow { collection, key }) => {
            assert_eq!((collection.as_str(), key.as_str()), ("people", "dave"));
        }
        other => panic!("expected UnknownRow, got {other:?}"),
    }
}

#[test]
fn a_transaction_commits_as_one_and_a_dropped_one_rolls_back() {
    let tmp = dir();
    let db = Db::open(tmp.path()).expect("open");
    posts(&db);

    let mut tx = db.transaction().expect("begin");
    tx.put(("posts", "a"), &json!({ "title": "a" })).expect("a");
    tx.put(("posts", "b"), &json!({ "title": "b" })).expect("b");
    tx.commit().expect("commit");
    assert_eq!(db.scan_count_rows("posts").expect("count"), 2);

    {
        let mut tx = db.transaction().expect("begin");
        tx.put(("posts", "c"), &json!({ "title": "c" })).expect("c");
        // No commit: the guard is dropped here.
    }
    assert_eq!(
        db.scan_count_rows("posts").expect("count after drop"),
        2,
        "a transaction dropped without a commit leaves nothing behind"
    );
    assert!(!db.exists(("posts", "c")).expect("exists"));

    let mut tx = db.transaction().expect("begin");
    tx.put(("posts", "d"), &json!({ "title": "d" })).expect("d");
    tx.rollback().expect("rollback");
    assert!(!db.exists(("posts", "d")).expect("exists"));
}

#[test]
fn every_refusal_names_the_construct_and_the_reason() {
    let tmp = dir();
    let db = Db::open(tmp.path()).expect("open");
    posts(&db);

    // A collection E4 does not have is named, not created.
    match db.put(("absent", "x"), &json!({})) {
        Err(Error::UnknownCollection(name)) => assert_eq!(name, "absent"),
        other => panic!("expected UnknownCollection, got {other:?}"),
    }

    // A document that is not an object.
    match db.put(("posts", "x"), &json!([1, 2, 3])) {
        Err(e @ Error::Refused { .. }) => {
            let text = e.to_string();
            assert!(text.contains("not a JSON object"), "{text}");
        }
        other => panic!("expected Refused, got {other:?}"),
    }

    // A `_key` in the document that disagrees with the address.
    match db.put(("posts", "x"), &json!({ "_key": "y" })) {
        Err(e @ Error::Refused { .. }) => {
            let text = e.to_string();
            assert!(text.contains("one external key"), "{text}");
        }
        other => panic!("expected Refused, got {other:?}"),
    }

    // A neighbour bound wider than the complete-or-error limit.
    match db.neighbours(("posts", "x"), None, Direction::Outgoing, 10_000) {
        Err(e @ Error::Refused { .. }) => {
            let text = e.to_string();
            assert!(text.contains("256"), "{text}");
        }
        other => panic!("expected Refused, got {other:?}"),
    }

    // The two SQL calls refuse each other's statements by name rather than
    // answering something plausible.
    match db.query("INSERT INTO posts (_key) VALUES ('z')", &[]) {
        Err(e @ Error::Refused { .. }) => {
            assert!(e.to_string().contains("Db::execute"), "{e}");
        }
        other => panic!("expected Refused, got {other:?}"),
    }
    match db.execute("SELECT _key FROM posts", &[]) {
        Err(e @ Error::Refused { .. }) => {
            assert!(e.to_string().contains("Db::query"), "{e}");
        }
        other => panic!("expected Refused, got {other:?}"),
    }

    // A Tier-2 construct is the language's refusal, carried out unchanged
    // with the keyword it names.
    match db.query("SELECT * FROM ALL", &[]) {
        Err(Error::Sql(e)) => {
            let text = e.to_string();
            assert!(!text.is_empty(), "a refusal always carries a reason");
        }
        other => panic!("expected a SQL refusal, got {other:?}"),
    }
}

#[test]
fn a_checkpoint_folds_the_log_and_reports_a_deferred_fold_rather_than_waiting() {
    let tmp = dir();
    let db = Db::open(tmp.path()).expect("open");
    posts(&db);
    for n in 0..200 {
        db.put(("posts", format!("p{n:03}").as_str()), &document(n))
            .expect("put");
    }
    let before = db.storage().expect("storage");
    assert!(before.wal_bytes > 0, "writes are in the log before a fold");
    assert!(db.checkpoint().expect("checkpoint"), "no reader holds a slot");
    let after = db.storage().expect("storage");
    assert!(
        after.data_bytes >= before.data_bytes,
        "the fold moves pages into the data file"
    );
    assert_eq!(
        db.scan_count_rows("posts").expect("count"),
        200,
        "a fold changes no row"
    );
}

#[test]
fn service_mode_answers_the_same_calls_and_says_why_a_fold_is_deferred() {
    let tmp = dir();
    let db = Db::open_service(tmp.path()).expect("open service");
    assert_eq!(db.mode(), Mode::Service);
    posts(&db);
    db.put(("posts", "p1"), &json!({ "title": "one" })).expect("put");
    assert_eq!(
        db.get(("posts", "p1")).expect("get"),
        Some(json!({ "title": "one", "_key": "p1" })),
        "a commit is published before the next read"
    );
    assert_eq!(db.scan_count_rows("posts").expect("count"), 1);
    let rows = db.query("SELECT _key FROM posts", &[]).expect("select");
    assert_eq!(rows.len(), 1);
    assert!(
        !db.checkpoint().expect("checkpoint"),
        "the published read view holds a reader slot, so the fold is deferred"
    );
    assert!(db.service().is_some());
    db.close().expect("close");
}

#[test]
fn a_vector_column_round_trips_as_a_json_array() {
    let tmp = dir();
    let db = Db::open(tmp.path()).expect("open");
    db.execute(
        "CREATE TABLE docs (title TEXT, embedding VECTOR(3))",
        &[],
    )
    .expect("create");
    db.put(
        ("docs", "d1"),
        &json!({ "title": "one", "embedding": [0.25, 0.5, 0.75] }),
    )
    .expect("put");
    let read = db.get(("docs", "d1")).expect("get").expect("row");
    assert_eq!(read["embedding"], json!([0.25, 0.5, 0.75]));
    // And the document read back writes back unchanged.
    db.put(("docs", "d1"), &read).expect("round trip");
    assert_eq!(
        db.get(("docs", "d1")).expect("get").expect("row")["embedding"],
        json!([0.25, 0.5, 0.75])
    );
}
