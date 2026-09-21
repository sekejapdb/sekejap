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

/// `docs/core/FORMAT_V2.md`: the published crate names the disk format it
/// writes, the number is the kernel's one constant rather than a copy, and a
/// database this crate creates carries it in page bytes 18-19 of both
/// checkpoint metadata copies and of its data pages.
#[test]
fn the_crate_names_disk_format_two_and_every_page_it_writes_carries_it() {
    const PAGE: usize = 4096;
    const STAMP_AT: usize = 18;
    assert_eq!(sekejap::FORMAT_VERSION, 2);
    assert_eq!(sekejap::FORMAT_VERSION, sekejap::core::FORMAT_VERSION);

    let tmp = dir();
    let db = Db::open(tmp.path()).expect("open");
    posts(&db);
    for n in 0..400 {
        db.put(("posts", &format!("p{n:05}") as &str), &document(n))
            .expect("put");
    }
    drop(db);

    let data = std::fs::read(tmp.path().join("data")).expect("read data file");
    assert_eq!(data.len() % PAGE, 0, "the data file is whole pages");
    let pages = data.len() / PAGE;
    assert!(
        pages >= 3,
        "need both metadata copies and at least one data page; got {pages}"
    );
    for no in 0..pages {
        let at = no * PAGE + STAMP_AT;
        let stamp = u16::from_le_bytes(data[at..at + 2].try_into().unwrap());
        assert_eq!(
            stamp,
            sekejap::FORMAT_VERSION,
            "page {no} of a database this crate created must carry disk format 2"
        );
    }
}

// ── §3 prepared statements and the bounded plan cache ─────────────────────

/// One collection of `n` rows, declared and filled through SQL, with a
/// scalar index so a `WHERE` predicate has one to name.
fn prepared_fixture(db: &Db, n: i64) -> BTreeMap<i64, Vec<String>> {
    db.execute("CREATE TABLE item (label TEXT, bucket INT)", &[])
        .expect("create table");
    db.execute("CREATE INDEX item_bucket ON item USING btree(bucket)", &[])
        .expect("create index");
    let mut oracle: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    for i in 0..n {
        let key = format!("i{i:04}");
        let bucket = i % 5;
        db.execute(
            "INSERT INTO item (_key, label, bucket) VALUES ($1, $2, $3)",
            &[json!(key), json!(format!("label {i}")), json!(bucket)],
        )
        .expect("insert");
        oracle.entry(bucket).or_default().push(key);
    }
    for keys in oracle.values_mut() {
        keys.sort();
    }
    oracle
}

fn keys_of(rows: &sekejap::Rows) -> Vec<String> {
    let mut out: Vec<String> = rows
        .iter()
        .filter_map(|row| row.json("_key"))
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    out.sort();
    out
}

#[test]
fn a_prepared_statement_compiles_once_and_rebinds_for_every_parameter_list() {
    let tmp = dir();
    let db = Db::open(tmp.path()).expect("open");
    let oracle = prepared_fixture(&db, 200);

    let mut statement = db
        .prepare("SELECT _key FROM item WHERE bucket = $1")
        .expect("prepare");
    assert_eq!(statement.rebindable(), None, "nothing is compiled yet");
    for bucket in 0..5i64 {
        let rows = statement.query_with(&[json!(bucket)]).expect("query_with");
        assert_eq!(&keys_of(&rows), oracle.get(&bucket).expect("bucket"));
    }
    assert_eq!(statement.rebindable(), Some(true));
    assert_eq!(
        statement.counters(),
        (5, 1),
        "five binds, and only the first of them compiled"
    );
    assert_eq!(statement.columns(), ["_key"]);
}

#[test]
fn a_prepared_statement_streams_and_writes_through_the_same_handle() {
    let tmp = dir();
    let db = Db::open(tmp.path()).expect("open");
    let oracle = prepared_fixture(&db, 64);

    let mut statement = db
        .prepare("SELECT _key FROM item WHERE bucket = $1")
        .expect("prepare");
    let mut seen: Vec<String> = Vec::new();
    let handed = statement
        .stream_with(&[json!(2i64)], 8, &mut |row| {
            if let Some(Value::String(key)) = row.json("_key") {
                seen.push(key);
            }
            Ok(())
        })
        .expect("stream_with");
    seen.sort();
    assert_eq!(&seen, oracle.get(&2).expect("bucket 2"));
    assert_eq!(handed as usize, seen.len());

    // A writing statement prepared by hand: never rebindable, because its
    // document is folded at compile -- but still parsed once.
    let mut insert = db
        .prepare("INSERT INTO item (_key, label, bucket) VALUES ($1, $2, $3)")
        .expect("prepare insert");
    for i in 0..4i64 {
        let n = insert
            .execute_with(&[json!(format!("x{i}")), json!("extra"), json!(9i64)])
            .expect("execute_with");
        assert_eq!(n, 1);
    }
    assert_eq!(insert.rebindable(), Some(false));
    assert!(insert
        .rebind_refusal()
        .expect("a refusal names its cause")
        .contains("folded at prepare"));
    let rows = db
        .query("SELECT _key FROM item WHERE bucket = $1", &[json!(9i64)])
        .expect("select");
    assert_eq!(rows.len(), 4);
}

#[test]
fn a_syntax_error_is_refused_by_prepare_before_any_parameter_is_bound() {
    let tmp = dir();
    let db = Db::open(tmp.path()).expect("open");
    prepared_fixture(&db, 8);
    let error = match db.prepare("SELECT _key FROM item WHERE") {
        Err(e) => e,
        Ok(_) => panic!("a truncated statement is a syntax error"),
    };
    assert!(
        matches!(error, Error::Refused { .. } | Error::Sql(_)),
        "{error}"
    );
}

#[test]
fn the_plan_cache_serves_db_query_and_a_hit_is_a_rebind() {
    let tmp = dir();
    let db = Db::open(tmp.path()).expect("open");
    let oracle = prepared_fixture(&db, 120);

    let before = db.cache_stats();
    assert_eq!(before.entries, 0);
    assert_eq!(before.entry_ceiling, sekejap::PLAN_CACHE_ENTRIES);
    assert_eq!(before.byte_ceiling, sekejap::PLAN_CACHE_BYTES);
    assert_eq!(before.statement_ceiling, sekejap::PLAN_CACHE_STATEMENT_BYTES);

    let sql = "SELECT _key FROM item WHERE bucket = $1";
    for bucket in 0..5i64 {
        let rows = db.query(sql, &[json!(bucket)]).expect("query");
        assert_eq!(&keys_of(&rows), oracle.get(&bucket).expect("bucket"));
    }
    let stats = db.cache_stats();
    assert_eq!(stats.entries, 1, "one statement text, one entry");
    assert_eq!(stats.bytes, sql.len());
    assert_eq!(stats.misses, 1, "only the first execution compiled");
    assert_eq!(stats.hits, 4);
    assert_eq!(stats.evictions, 0);
}

#[test]
fn the_plan_cache_evicts_at_its_entry_ceiling_least_recently_used_first() {
    let tmp = dir();
    let db = Db::open(tmp.path()).expect("open");
    prepared_fixture(&db, 40);

    // One distinct statement text per entry, more of them than the ceiling
    // allows. The alias makes each text distinct without changing what any
    // of them asks.
    let texts: Vec<String> = (0..sekejap::PLAN_CACHE_ENTRIES + 8)
        .map(|n| format!("SELECT _key AS c{n} FROM item WHERE bucket = $1"))
        .collect();
    for text in &texts {
        db.query(text, &[json!(1i64)]).expect("query");
    }
    let stats = db.cache_stats();
    assert_eq!(
        stats.entries,
        sekejap::PLAN_CACHE_ENTRIES,
        "the cache holds its ceiling and not one more"
    );
    assert!(stats.bytes <= sekejap::PLAN_CACHE_BYTES);
    assert_eq!(stats.evictions, 8, "the eight oldest were dropped");
    assert_eq!(stats.misses as usize, texts.len());
    assert_eq!(stats.hits, 0);

    // Least-recently-used first: the first text is gone and the last is not.
    db.query(&texts[texts.len() - 1], &[json!(1i64)])
        .expect("query");
    assert_eq!(db.cache_stats().hits, 1, "the newest entry is still there");
    db.query(&texts[0], &[json!(1i64)]).expect("query");
    assert_eq!(
        db.cache_stats().hits,
        1,
        "the oldest entry was evicted and had to compile again"
    );
}

#[test]
fn a_statement_longer_than_the_statement_ceiling_is_never_cached() {
    let tmp = dir();
    let db = Db::open(tmp.path()).expect("open");
    prepared_fixture(&db, 8);
    // A long alias, so the text passes the per-statement ceiling without
    // asking anything unusual.
    let padding = "c".repeat(sekejap::PLAN_CACHE_STATEMENT_BYTES);
    let sql = format!("SELECT _key AS {padding} FROM item WHERE bucket = $1");
    assert!(sql.len() > sekejap::PLAN_CACHE_STATEMENT_BYTES);
    for _ in 0..3 {
        db.query(&sql, &[json!(1i64)]).expect("query");
    }
    let stats = db.cache_stats();
    assert_eq!(stats.entries, 0, "nothing that long is held");
    assert_eq!(stats.misses, 3);
    assert_eq!(stats.too_long, 3);
    assert_eq!(stats.hits, 0);
}

#[test]
fn a_ddl_statement_invalidates_every_plan_compiled_before_it() {
    let tmp = dir();
    let db = Db::open(tmp.path()).expect("open");
    prepared_fixture(&db, 40);
    let sql = "SELECT _key FROM item WHERE bucket = $1";
    db.query(sql, &[json!(1i64)]).expect("query");
    db.query(sql, &[json!(2i64)]).expect("query");
    assert_eq!(db.cache_stats().hits, 1);

    // A second index on the same column changes the catalog the plan names.
    db.execute("CREATE INDEX item_label ON item USING btree(label)", &[])
        .expect("create index");
    assert_eq!(db.cache_stats().entries, 0, "the cache was emptied");
    let hits_before = db.cache_stats().hits;
    db.query(sql, &[json!(1i64)]).expect("query");
    assert_eq!(
        db.cache_stats().hits,
        hits_before,
        "the plan compiled before the DDL is never served"
    );

    // An INSERT does not: a plan holds no rows.
    db.execute(
        "INSERT INTO item (_key, label, bucket) VALUES ($1, $2, $3)",
        &[json!("zzz"), json!("late"), json!(1i64)],
    )
    .expect("insert");
    let hits_before = db.cache_stats().hits;
    let rows = db.query(sql, &[json!(1i64)]).expect("query");
    assert_eq!(
        db.cache_stats().hits,
        hits_before + 1,
        "the cached plan was reused"
    );
    assert!(
        keys_of(&rows).contains(&"zzz".to_owned()),
        "and it sees the row written after it was compiled"
    );
}

#[test]
fn the_cached_plan_answers_what_a_fresh_compile_answers_for_every_binding() {
    let tmp = dir();
    let db = Db::open(tmp.path()).expect("open");
    let oracle = prepared_fixture(&db, 200);
    let sql = "SELECT _key FROM item WHERE bucket = $1";

    // Interleave a cached path and an uncached one (the plan cache is a
    // property of `Db`, so a second handle on the same directory has its own
    // empty one) and require the same answer from both, for every binding.
    for bucket in 0..5i64 {
        let cached = db.query(sql, &[json!(bucket)]).expect("cached");
        let fresh = Db::open(tmp.path());
        // A second handle cannot open the same directory while the first
        // holds it, so the fresh side is the explicit statement instead:
        // parsed and compiled here, bound once, never reused.
        drop(fresh);
        let mut once = db.prepare(sql).expect("prepare");
        let explicit = once.query_with(&[json!(bucket)]).expect("query_with");
        assert_eq!(keys_of(&cached), keys_of(&explicit));
        assert_eq!(&keys_of(&cached), oracle.get(&bucket).expect("bucket"));
    }
}
