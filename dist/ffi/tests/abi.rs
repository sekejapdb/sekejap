//! The C ABI, exercised through its C signatures.
//!
//! Every call below goes through the `extern "C"` entry point with
//! `CString`/`CStr` on both sides, so what is tested is the ABI a C caller
//! reaches: the sentinels, the ownership, the JSON shapes and the error
//! codes -- not the Rust surface underneath.
//!
//! The oracle is held in the test process: a `BTreeMap` of what was written,
//! compared against what the library answers.
//!
//! This file is compiled as the crate's own test module (`src/lib.rs`), so
//! the entry points are reached by name; what is exercised is still the C
//! signature of each one. That the LINK works is `make check`, which
//! compiles `examples/smoke.c` against the built library.
//!
//! Contract: `docs/dist/C_ABI.md`. Standard:
//! `docs/core/FOUNDATION_TEST_STANDARD.md`.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::ptr;

use crate::*;
use serde_json::{json, Value};

// ── plumbing ────────────────────────────────────────────────────────────────

fn c(text: &str) -> CString {
    CString::new(text).expect("no interior NUL in a test string")
}

/// Read a `char*` the library returned and FREE it with the library's own
/// free, which is the ownership rule under test.
fn take(p: *mut c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let text = unsafe { CStr::from_ptr(p) }
        .to_str()
        .expect("the library returns UTF-8")
        .to_owned();
    unsafe { sekejap_string_free(p) };
    Some(text)
}

fn take_json(p: *mut c_char) -> Value {
    if p.is_null() {
        let (message, code) = last(ptr::null_mut());
        panic!("a JSON answer, not NULL: {code:?} {message}");
    }
    let text = take(p).expect("a JSON answer, not NULL");
    serde_json::from_str(&text).expect("the answer parses as JSON")
}

/// The message and the code for the last failure on this thread.
fn last(db: *mut SekejapDb) -> (String, SekejapStatus) {
    let code = unsafe { sekejap_last_error_code(db) };
    let message = take(unsafe { sekejap_last_error(db) }).unwrap_or_default();
    (message, code)
}

struct Fixture {
    db: *mut SekejapDb,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn open() -> Self {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = c(dir.path().to_str().expect("a UTF-8 path"));
        let db = unsafe { sekejap_open(path.as_ptr()) };
        assert!(
            !db.is_null(),
            "open answered NULL: {:?}",
            last(ptr::null_mut())
        );
        Self { db, _dir: dir }
    }

    fn service() -> Self {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = c(dir.path().to_str().expect("a UTF-8 path"));
        let db = unsafe { sekejap_open_service(path.as_ptr()) };
        assert!(!db.is_null(), "open_service answered NULL");
        Self { db, _dir: dir }
    }

    fn create(&self, name: &str, fields: &Value) -> i32 {
        unsafe {
            sekejap_create_collection(self.db, c(name).as_ptr(), c(&fields.to_string()).as_ptr())
        }
    }

    fn put(&self, collection: &str, key: &str, document: &Value) -> i32 {
        unsafe {
            sekejap_put(
                self.db,
                c(collection).as_ptr(),
                c(key).as_ptr(),
                c(&document.to_string()).as_ptr(),
            )
        }
    }

    fn get(&self, collection: &str, key: &str) -> Option<Value> {
        let p = unsafe { sekejap_get(self.db, c(collection).as_ptr(), c(key).as_ptr()) };
        take(p).map(|text| serde_json::from_str(&text).expect("a JSON document"))
    }

    fn query(&self, sql: &str, params: &Value) -> Value {
        let p =
            unsafe { sekejap_query(self.db, c(sql).as_ptr(), c(&params.to_string()).as_ptr()) };
        take_json(p)
    }

    fn execute(&self, sql: &str, params: &Value) -> i64 {
        unsafe {
            sekejap_execute(self.db, c(sql).as_ptr(), c(&params.to_string()).as_ptr()) as i64
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        unsafe { sekejap_close(self.db) };
    }
}

fn text_field(name: &str) -> Value {
    json!({ "name": name, "kind": "text" })
}

// ── §1 identity ─────────────────────────────────────────────────────────────

#[test]
fn the_version_string_is_static_and_the_format_version_is_two() {
    let version = unsafe { CStr::from_ptr(sekejap_version()) }
        .to_str()
        .expect("UTF-8");
    assert_eq!(version, env!("CARGO_PKG_VERSION"));
    // Called twice, it is the SAME pointer: static program data, which is
    // why the header forbids passing it to sekejap_string_free.
    assert_eq!(sekejap_version(), sekejap_version());
    assert_eq!(sekejap_format_version(), 2);
}

// ── §2 documents against a BTreeMap oracle ──────────────────────────────────

#[test]
fn every_document_written_through_the_abi_reads_back_exactly_as_the_oracle_holds_it() {
    let db = Fixture::open();
    assert_eq!(
        db.create(
            "posts",
            &json!([text_field("title"), {"name": "rank", "kind": "int"}])
        ),
        1
    );

    let mut oracle: BTreeMap<String, Value> = BTreeMap::new();
    for n in 0..64u32 {
        let key = format!("p{n:03}");
        let document = json!({ "_key": key, "title": format!("post {n}"), "rank": n });
        assert_eq!(db.put("posts", &key, &document), 0, "put {key}");
        oracle.insert(key, document);
    }

    for (key, expected) in &oracle {
        let found = db.get("posts", key).expect("a written row reads back");
        assert_eq!(&found, expected, "round trip of {key}");
    }

    // A miss is NULL with the status left Ok: that is how a C caller tells a
    // miss from a failure without parsing the message.
    assert!(db.get("posts", "absent").is_none());
    let (message, code) = last(db.db);
    assert_eq!(code, SekejapStatus::Ok, "a miss is not an error: {message}");

    assert_eq!(
        unsafe { sekejap_exists(db.db, c("posts").as_ptr(), c("p000").as_ptr()) },
        1
    );
    assert_eq!(
        unsafe { sekejap_exists(db.db, c("posts").as_ptr(), c("absent").as_ptr()) },
        0
    );

    // Delete answers whether the row was there, and the count follows.
    assert_eq!(
        unsafe { sekejap_delete(db.db, c("posts").as_ptr(), c("p000").as_ptr()) },
        1
    );
    assert_eq!(
        unsafe { sekejap_delete(db.db, c("posts").as_ptr(), c("p000").as_ptr()) },
        0
    );
    oracle.remove("p000");
    assert_eq!(
        unsafe { sekejap_count_rows(db.db, c("posts").as_ptr()) },
        oracle.len() as i64
    );
    assert_eq!(
        unsafe { sekejap_scan_count_rows(db.db, c("posts").as_ptr()) },
        oracle.len() as i64
    );
}

#[test]
fn put_many_writes_one_batch_and_answers_how_many_rows_it_wrote() {
    let db = Fixture::open();
    db.create("item", &json!([text_field("name")]));

    let rows: Vec<Value> = (0..25)
        .map(|n| json!({ "key": format!("i{n}"), "doc": { "name": format!("item {n}") } }))
        .collect();
    let written = unsafe {
        sekejap_put_many(
            db.db,
            c("item").as_ptr(),
            c(&Value::Array(rows).to_string()).as_ptr(),
        )
    };
    assert_eq!(written, 25);
    assert_eq!(unsafe { sekejap_count_rows(db.db, c("item").as_ptr()) }, 25);
    assert_eq!(
        db.get("item", "i7")
            .and_then(|d| d["name"].as_str().map(str::to_owned)),
        Some("item 7".to_owned())
    );
}

// ── §3 the scan, at three page sizes ────────────────────────────────────────

#[test]
fn a_scan_at_three_page_sizes_delivers_the_same_rows_in_the_same_order() {
    let db = Fixture::open();
    db.create("row", &json!([{"name": "n", "kind": "int"}]));
    let rows: Vec<Value> = (0..37)
        .map(|n| json!({ "key": format!("r{n:02}"), "doc": { "n": n } }))
        .collect();
    assert_eq!(
        unsafe {
            sekejap_put_many(
                db.db,
                c("row").as_ptr(),
                c(&Value::Array(rows).to_string()).as_ptr(),
            )
        },
        37
    );

    let walk = |page_rows: usize| -> (Vec<String>, usize) {
        let scan = unsafe { sekejap_scan_open(db.db, c("row").as_ptr(), page_rows) };
        assert!(!scan.is_null());
        let mut keys = Vec::new();
        let mut pages = 0usize;
        loop {
            let page = unsafe { sekejap_scan_next(scan) };
            if page.is_null() {
                // The end of a walk is NULL with the status left Ok.
                assert_eq!(unsafe { sekejap_last_error_code(db.db) }, SekejapStatus::Ok);
                break;
            }
            pages += 1;
            let page: Value = serde_json::from_str(&take(page).expect("a page")).expect("JSON");
            let page = page.as_array().expect("a JSON array of documents").clone();
            assert!(page.len() <= page_rows, "a page never exceeds page_rows");
            for document in page {
                keys.push(document["_key"].as_str().expect("_key").to_owned());
            }
        }
        unsafe { sekejap_scan_close(scan) };
        (keys, pages)
    };

    let (keys_of_1, pages_of_1) = walk(1);
    let (keys_of_8, pages_of_8) = walk(8);
    let (keys_of_1000, pages_of_1000) = walk(1_000);

    let expected: Vec<String> = (0..37).map(|n| format!("r{n:02}")).collect();
    assert_eq!(keys_of_1, expected);
    assert_eq!(keys_of_8, expected);
    assert_eq!(keys_of_1000, expected);
    // The page size is what changes, and only the page count with it: 37
    // rows at 1 row per page is 37 pages, at 8 rows is 5, at 1,000 is 1.
    assert_eq!((pages_of_1, pages_of_8, pages_of_1000), (37, 5, 1));
}

// ── §4 SQL and the JSON shapes ──────────────────────────────────────────────

#[test]
fn a_query_answers_a_json_array_of_objects_keyed_by_column_name() {
    let db = Fixture::open();
    db.create(
        "book",
        &json!([text_field("title"), {"name": "year", "kind": "int"}]),
    );
    // QL_CONTRACT §6: a Tier-1 predicate on a field is answered index-side,
    // so the field needs an index to name.
    assert_eq!(
        db.execute("CREATE INDEX book_year ON book USING btree(year)", &json!([])),
        0
    );
    assert_eq!(
        db.execute(
            "INSERT INTO book (_key, title, year) VALUES ($1, $2, $3)",
            &json!(["b1", "Dune", 1965])
        ),
        1
    );
    assert_eq!(
        db.execute(
            "INSERT INTO book (_key, title, year) VALUES ($1, $2, $3)",
            &json!(["b2", "Solaris", 1961])
        ),
        1
    );

    let answer = db.query(
        "SELECT _key, title, year FROM book WHERE year > $1 ORDER BY year",
        &json!([1900]),
    );
    assert_eq!(
        answer,
        json!([
            { "_key": "b2", "title": "Solaris", "year": 1961 },
            { "_key": "b1", "title": "Dune", "year": 1965 },
        ]),
        "one object per row, keyed by the select list"
    );

    // A NULL params pointer is "no parameters", not an error.
    let all = take_json(unsafe {
        sekejap_query(db.db, c("SELECT _key FROM book").as_ptr(), ptr::null())
    });
    assert_eq!(all.as_array().expect("an array").len(), 2);

    // EXPLAIN is its own call and returns a plan, not rows.
    let plan = take(unsafe {
        sekejap_explain(db.db, c("SELECT _key FROM book").as_ptr(), ptr::null())
    })
    .expect("a plan");
    assert!(!plan.is_empty(), "the plan is text, not an empty string");
}

#[test]
fn a_paged_query_hands_back_the_same_rows_as_one_query_in_pages_of_the_stated_size() {
    let db = Fixture::open();
    db.create("point", &json!([{"name": "n", "kind": "int"}]));
    assert_eq!(
        db.execute("CREATE INDEX point_n ON point USING btree(n)", &json!([])),
        0
    );
    let rows: Vec<Value> = (0..20)
        .map(|n| json!({ "key": format!("k{n:02}"), "doc": { "n": n } }))
        .collect();
    unsafe {
        sekejap_put_many(
            db.db,
            c("point").as_ptr(),
            c(&Value::Array(rows).to_string()).as_ptr(),
        )
    };

    let whole = db.query("SELECT _key, n FROM point ORDER BY n", &json!([]));
    let whole = whole.as_array().expect("an array").clone();
    assert_eq!(whole.len(), 20);

    let cursor = unsafe {
        sekejap_query_open(
            db.db,
            c("SELECT _key, n FROM point ORDER BY n").as_ptr(),
            ptr::null(),
            6,
        )
    };
    assert!(!cursor.is_null());
    let mut paged: Vec<Value> = Vec::new();
    let mut sizes: Vec<usize> = Vec::new();
    loop {
        let page = unsafe { sekejap_query_next(cursor) };
        if page.is_null() {
            break;
        }
        let page: Value = serde_json::from_str(&take(page).expect("a page")).expect("JSON");
        let page = page.as_array().expect("an array").clone();
        sizes.push(page.len());
        paged.extend(page);
    }
    unsafe { sekejap_query_close(cursor) };
    assert_eq!(sizes, vec![6, 6, 6, 2], "20 rows at 6 rows per page");
    assert_eq!(paged, whole);
}

#[test]
fn a_prepared_statement_is_rebindable_for_reads_and_says_it_is_not_for_writes() {
    let db = Fixture::open();
    db.create("item", &json!([text_field("bucket")]));
    assert_eq!(
        db.execute(
            "CREATE INDEX item_bucket ON item USING btree(bucket)",
            &json!([])
        ),
        0
    );
    for n in 0..6 {
        db.put(
            "item",
            &format!("i{n}"),
            &json!({ "bucket": if n % 2 == 0 { "even" } else { "odd" } }),
        );
    }

    let read =
        unsafe { sekejap_prepare(db.db, c("SELECT _key FROM item WHERE bucket = $1").as_ptr()) };
    assert!(!read.is_null());
    // Parsed at prepare, compiled by the first bind: before that there is
    // nothing to answer, and that is not a failure.
    assert_eq!(
        unsafe { sekejap_stmt_rebindable(read) },
        SEKEJAP_REBIND_UNBOUND
    );

    let even =
        take_json(unsafe { sekejap_stmt_query(read, c(&json!(["even"]).to_string()).as_ptr()) });
    assert_eq!(even.as_array().expect("an array").len(), 3);
    assert_eq!(unsafe { sekejap_stmt_rebindable(read) }, 1);

    // The rebind answers the other half, and the plan is not compiled again.
    let odd =
        take_json(unsafe { sekejap_stmt_query(read, c(&json!(["odd"]).to_string()).as_ptr()) });
    assert_eq!(odd.as_array().expect("an array").len(), 3);
    assert_eq!(unsafe { sekejap_stmt_rebindable(read) }, 1);
    unsafe { sekejap_stmt_free(read) };

    let write = unsafe {
        sekejap_prepare(
            db.db,
            c("INSERT INTO item (_key, bucket) VALUES ($1, $2)").as_ptr(),
        )
    };
    assert!(!write.is_null());
    assert_eq!(
        unsafe { sekejap_stmt_execute(write, c(&json!(["i9", "even"]).to_string()).as_ptr()) },
        1
    );
    // A write folds its document at compile, so it is never rebindable and
    // says so rather than pretending.
    assert_eq!(unsafe { sekejap_stmt_rebindable(write) }, 0);
    unsafe { sekejap_stmt_free(write) };
    assert_eq!(unsafe { sekejap_count_rows(db.db, c("item").as_ptr()) }, 7);
}

// ── §5 edges ────────────────────────────────────────────────────────────────

#[test]
fn a_link_is_visible_to_neighbours_in_the_direction_it_was_made_and_an_unlink_removes_it() {
    let db = Fixture::open();
    db.create("people", &json!([text_field("name")]));
    for (key, name) in [("alice", "Alice"), ("bob", "Bob"), ("carol", "Carol")] {
        db.put("people", key, &json!({ "name": name }));
    }

    let link = |from: &str, to: &str| unsafe {
        sekejap_link(
            db.db,
            c("people").as_ptr(),
            c(from).as_ptr(),
            c("knows").as_ptr(),
            c("people").as_ptr(),
            c(to).as_ptr(),
        )
    };
    assert_eq!(link("alice", "bob"), 0);
    assert_eq!(
        unsafe {
            sekejap_link_with(
                db.db,
                c("people").as_ptr(),
                c("alice").as_ptr(),
                c("knows").as_ptr(),
                c("people").as_ptr(),
                c("carol").as_ptr(),
                c(&json!({ "since": 2020 }).to_string()).as_ptr(),
            )
        },
        0
    );
    assert_eq!(unsafe { sekejap_scan_count_edges(db.db) }, 2);

    let neighbours = |key: &str, direction: SekejapDirection| -> Vec<String> {
        let answer = take_json(unsafe {
            sekejap_neighbours(
                db.db,
                c("people").as_ptr(),
                c(key).as_ptr(),
                c("knows").as_ptr(),
                direction,
                16,
            )
        });
        let mut keys: Vec<String> = answer
            .as_array()
            .expect("an array")
            .iter()
            .map(|row| {
                assert_eq!(row["collection"], "people", "the collection is named");
                assert!(row["document"].is_object(), "the document is an object");
                row["key"].as_str().expect("key").to_owned()
            })
            .collect();
        keys.sort();
        keys
    };
    assert_eq!(
        neighbours("alice", SekejapDirection::Outgoing),
        ["bob", "carol"]
    );
    assert_eq!(
        neighbours("alice", SekejapDirection::Incoming),
        [] as [&str; 0]
    );
    assert_eq!(neighbours("bob", SekejapDirection::Incoming), ["alice"]);

    let unlink = |from: &str, to: &str| unsafe {
        sekejap_unlink(
            db.db,
            c("people").as_ptr(),
            c(from).as_ptr(),
            c("knows").as_ptr(),
            c("people").as_ptr(),
            c(to).as_ptr(),
        )
    };
    assert_eq!(unlink("alice", "bob"), 1, "the edge was there");
    assert_eq!(unlink("alice", "bob"), 0, "and is not any more");
    assert_eq!(neighbours("alice", SekejapDirection::Outgoing), ["carol"]);
}

// ── §6 catalog ──────────────────────────────────────────────────────────────

#[test]
fn describe_answers_the_declared_fields_with_key_first_and_collections_lists_them_in_key_order() {
    let db = Fixture::open();
    assert_eq!(
        db.create(
            "doc",
            &json!([
                text_field("title"),
                { "name": "score", "kind": "real" },
                { "name": "embedding", "kind": "vector", "dimension": 4 },
            ])
        ),
        1,
        "created"
    );
    assert_eq!(db.create("doc", &json!([])), 0, "already there");
    assert_eq!(db.create("aaa", &json!([text_field("x")])), 1);

    let names = take_json(unsafe { sekejap_collections(db.db) });
    assert_eq!(names, json!(["aaa", "doc"]), "in key order");

    let shape = take_json(unsafe { sekejap_describe(db.db, c("doc").as_ptr()) });
    assert_eq!(shape["name"], "doc");
    let fields = shape["fields"].as_array().expect("fields").clone();
    assert_eq!(fields[0]["name"], "_key");
    assert_eq!(fields[0]["kind"], "text");
    assert_eq!(fields[0]["primary_key"], true);
    let named: Vec<(&str, &str)> = fields
        .iter()
        .map(|f| (f["name"].as_str().unwrap(), f["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(
        named,
        vec![
            ("_key", "text"),
            ("title", "text"),
            ("score", "real"),
            ("embedding", "vector")
        ]
    );
    assert_eq!(
        fields[3]["dimension"], 4,
        "a vector field carries its dimension"
    );

    // No such collection is NULL with the status left Ok, as a miss is.
    assert!(take(unsafe { sekejap_describe(db.db, c("nope").as_ptr()) }).is_none());
    assert_eq!(unsafe { sekejap_last_error_code(db.db) }, SekejapStatus::Ok);

    assert_eq!(
        unsafe { sekejap_drop_collection(db.db, c("aaa").as_ptr()) },
        1
    );
    assert_eq!(
        unsafe { sekejap_drop_collection(db.db, c("aaa").as_ptr()) },
        0
    );
    assert_eq!(
        take_json(unsafe { sekejap_collections(db.db) }),
        json!(["doc"])
    );
}

#[test]
fn storage_answers_the_two_files_and_their_total_and_a_checkpoint_reports_whether_it_folded() {
    let db = Fixture::open();
    db.create("x", &json!([text_field("v")]));
    db.put("x", "one", &json!({ "v": "a" }));

    let storage = take_json(unsafe { sekejap_storage(db.db) });
    let data = storage["data_bytes"].as_u64().expect("data_bytes");
    let wal = storage["wal_bytes"].as_u64().expect("wal_bytes");
    assert_eq!(storage["total_bytes"].as_u64(), Some(data + wal));
    assert!(data > 0, "the data file exists once a row is committed");

    // 1 folded, 0 deferred; both are success, and neither is -1.
    let folded = unsafe { sekejap_checkpoint(db.db) };
    assert!(folded == 0 || folded == 1, "checkpoint answered {folded}");
    // Single mode has no published view to swap, so publish succeeds having
    // done nothing.
    assert_eq!(unsafe { sekejap_publish(db.db) }, 0);
}

// ── §7 transactions ─────────────────────────────────────────────────────────

#[test]
fn a_committed_transaction_keeps_every_write_and_a_rolled_back_one_keeps_none() {
    let db = Fixture::open();
    db.create("account", &json!([{"name": "balance", "kind": "int"}]));
    db.put("account", "a", &json!({ "balance": 100 }));
    db.put("account", "b", &json!({ "balance": 0 }));

    let tx = unsafe { sekejap_tx_begin(db.db) };
    assert!(!tx.is_null());
    assert_eq!(
        unsafe {
            sekejap_tx_put(
                tx,
                c("account").as_ptr(),
                c("a").as_ptr(),
                c(&json!({ "balance": 40 }).to_string()).as_ptr(),
            )
        },
        0
    );
    assert_eq!(
        unsafe {
            sekejap_tx_put(
                tx,
                c("account").as_ptr(),
                c("b").as_ptr(),
                c(&json!({ "balance": 60 }).to_string()).as_ptr(),
            )
        },
        0
    );
    assert_eq!(
        unsafe {
            sekejap_tx_link(
                tx,
                c("account").as_ptr(),
                c("a").as_ptr(),
                c("paid").as_ptr(),
                c("account").as_ptr(),
                c("b").as_ptr(),
            )
        },
        0
    );
    assert_eq!(unsafe { sekejap_tx_commit(tx) }, 0);

    assert_eq!(db.get("account", "a").unwrap()["balance"], 40);
    assert_eq!(db.get("account", "b").unwrap()["balance"], 60);
    assert_eq!(unsafe { sekejap_scan_count_edges(db.db) }, 1);

    let tx = unsafe { sekejap_tx_begin(db.db) };
    assert_eq!(
        unsafe {
            sekejap_tx_put(
                tx,
                c("account").as_ptr(),
                c("a").as_ptr(),
                c(&json!({ "balance": 0 }).to_string()).as_ptr(),
            )
        },
        0
    );
    assert_eq!(
        unsafe { sekejap_tx_delete(tx, c("account").as_ptr(), c("b").as_ptr()) },
        1
    );
    assert_eq!(
        unsafe {
            sekejap_tx_execute(
                tx,
                c("INSERT INTO account (_key, balance) VALUES ($1, $2)").as_ptr(),
                c(&json!(["c", 5]).to_string()).as_ptr(),
            )
        },
        1
    );
    assert_eq!(unsafe { sekejap_tx_rollback(tx) }, 0);

    assert_eq!(db.get("account", "a").unwrap()["balance"], 40, "unchanged");
    assert!(
        db.get("account", "b").is_some(),
        "the delete was rolled back"
    );
    assert!(db.get("account", "c").is_none(), "the insert was rolled back");
}

// ── §8 every error path ─────────────────────────────────────────────────────

#[test]
fn every_failure_returns_its_sentinel_and_leaves_a_message_and_a_code_on_this_thread() {
    let db = Fixture::open();
    db.create("t", &json!([text_field("v")]));

    // A null required argument.
    assert_eq!(
        unsafe { sekejap_put(db.db, ptr::null(), ptr::null(), ptr::null()) },
        -1
    );
    let (message, code) = last(db.db);
    assert_eq!(code, SekejapStatus::Invalid);
    assert!(message.contains("collection"), "names the argument: {message}");

    // A null handle.
    assert_eq!(
        unsafe { sekejap_count_rows(ptr::null_mut(), c("t").as_ptr()) },
        -1
    );
    assert_eq!(last(ptr::null_mut()).1, SekejapStatus::Invalid);

    // JSON that does not parse.
    assert_eq!(
        unsafe {
            sekejap_put(
                db.db,
                c("t").as_ptr(),
                c("k").as_ptr(),
                c("{not json").as_ptr(),
            )
        },
        -1
    );
    let (message, code) = last(db.db);
    assert_eq!(code, SekejapStatus::Invalid);
    assert!(message.contains("not JSON"), "{message}");

    // A collection that is not in the catalog.
    assert_eq!(
        unsafe {
            sekejap_put(
                db.db,
                c("absent").as_ptr(),
                c("k").as_ptr(),
                c("{}").as_ptr(),
            )
        },
        -1
    );
    let (message, code) = last(db.db);
    assert_eq!(code, SekejapStatus::Invalid);
    assert!(message.contains("absent"), "names the collection: {message}");

    // A syntax error, reported at prepare.
    assert!(unsafe { sekejap_prepare(db.db, c("SELEKT 1").as_ptr()) }.is_null());
    assert_eq!(last(db.db).1, SekejapStatus::Invalid);

    // A Tier-2/Tier-3 construct: REFUSED by name, never an empty answer.
    assert!(
        unsafe { sekejap_query(db.db, c("SELECT * FROM t FOR UPDATE").as_ptr(), ptr::null()) }
            .is_null()
    );
    let (message, code) = last(db.db);
    assert!(
        code == SekejapStatus::Refused
            || code == SekejapStatus::Invalid
            || code == SekejapStatus::Unsupported,
        "a construct with no atomic is refused, not answered: {code:?} {message}"
    );
    assert!(!message.is_empty(), "a refusal always carries a reason");

    // An edge whose endpoint does not exist: UnknownRow, its own code.
    db.put("t", "here", &json!({ "v": "x" }));
    assert_eq!(
        unsafe {
            sekejap_link(
                db.db,
                c("t").as_ptr(),
                c("here").as_ptr(),
                c("knows").as_ptr(),
                c("t").as_ptr(),
                c("nowhere").as_ptr(),
            )
        },
        -1
    );
    let (message, code) = last(db.db);
    assert_eq!(code, SekejapStatus::UnknownRow, "{message}");

    // A neighbour walk past the complete-or-error bound.
    assert!(unsafe {
        sekejap_neighbours(
            db.db,
            c("t").as_ptr(),
            c("here").as_ptr(),
            ptr::null(),
            SekejapDirection::Outgoing,
            100_000,
        )
    }
    .is_null());
    assert_eq!(last(db.db).1, SekejapStatus::Refused);

    // A field kind that is not one of the eight.
    assert_eq!(
        db.create("bad", &json!([{ "name": "x", "kind": "blob" }])),
        -1
    );
    let (message, code) = last(db.db);
    assert_eq!(code, SekejapStatus::Invalid);
    assert!(message.contains("blob"), "{message}");

    // A vector field with no dimension.
    assert_eq!(
        db.create("bad", &json!([{ "name": "x", "kind": "vector" }])),
        -1
    );
    assert_eq!(last(db.db).1, SekejapStatus::Invalid);

    // A store setting that is not one of the three.
    let dir = tempfile::tempdir().expect("a directory");
    assert!(unsafe {
        sekejap_open_with_config(
            c(dir.path().to_str().unwrap()).as_ptr(),
            c(&json!({ "nonsense": 1 }).to_string()).as_ptr(),
        )
    }
    .is_null());
    let (message, code) = last(ptr::null_mut());
    assert_eq!(code, SekejapStatus::Invalid);
    assert!(message.contains("nonsense"), "{message}");

    // A success clears both halves of the slot.
    assert_eq!(db.put("t", "ok", &json!({ "v": "y" })), 0);
    let (message, code) = last(db.db);
    assert_eq!(code, SekejapStatus::Ok);
    assert!(message.is_empty(), "no message after a success: {message}");
}

#[test]
fn the_four_calls_sekejap_has_no_atomic_for_refuse_by_name_and_answer_nothing_else() {
    let db = Fixture::open();

    assert!(sekejap_open_memory().is_null());
    let (message, code) = last(ptr::null_mut());
    assert_eq!(code, SekejapStatus::Refused);
    assert!(message.contains("disk-first"), "{message}");

    assert_eq!(unsafe { sekejap_trim_memory(db.db) }, -1);
    let (message, code) = last(db.db);
    assert_eq!(code, SekejapStatus::Refused);
    assert!(message.contains("trim"), "{message}");

    assert_eq!(unsafe { sekejap_compact(db.db) }, -1);
    let (message, code) = last(db.db);
    assert_eq!(code, SekejapStatus::Refused);
    assert!(message.contains("checkpoint"), "{message}");

    assert!(unsafe { sekejap_show(db.db, c("SHOW TABLES").as_ptr()) }.is_null());
    let (message, code) = last(db.db);
    assert_eq!(code, SekejapStatus::Refused);
    assert!(message.contains("SHOW"), "{message}");
}

#[test]
fn the_service_calls_are_refused_by_name_on_a_handle_opened_in_single_mode() {
    let db = Fixture::open();

    let refused = |answer: i64, call: &str| {
        assert_eq!(answer, -1, "{call} answered {answer} in single mode");
        let (message, code) = last(ptr::null_mut());
        assert_eq!(code, SekejapStatus::Refused, "{call}: {message}");
        assert!(
            message.contains(call) && message.contains("single mode"),
            "{call} refuses by name with a reason: {message}"
        );
    };

    refused(
        unsafe { sekejap_statement_timeout_ms(db.db, 50) } as i64,
        "sekejap_statement_timeout_ms",
    );
    refused(unsafe { sekejap_cancel(db.db) } as i64, "sekejap_cancel");
    refused(
        unsafe { sekejap_clear_interrupt(db.db) } as i64,
        "sekejap_clear_interrupt",
    );
    refused(unsafe { sekejap_subscribe(db.db) }, "sekejap_subscribe");
    refused(
        unsafe { sekejap_unsubscribe(db.db, 0) } as i64,
        "sekejap_unsubscribe",
    );
    assert!(unsafe { sekejap_next_change(db.db, 0, 0) }.is_null());
    assert_eq!(last(db.db).1, SekejapStatus::Refused);
}

// ── §9 service mode ─────────────────────────────────────────────────────────

#[test]
fn a_service_handle_answers_the_timeout_the_cancel_and_one_change_event_per_commit() {
    let db = Fixture::service();
    db.create("m", &json!([text_field("v")]));

    assert_eq!(unsafe { sekejap_statement_timeout_ms(db.db, 250) }, 0);
    assert_eq!(
        unsafe { sekejap_statement_timeout_ms(db.db, 0) },
        0,
        "cleared"
    );

    // The cancel is sticky until it is cleared, and says which it did.
    assert_eq!(unsafe { sekejap_cancel(db.db) }, 0);
    assert_eq!(
        unsafe { sekejap_clear_interrupt(db.db) },
        1,
        "one was standing"
    );
    assert_eq!(unsafe { sekejap_clear_interrupt(db.db) }, 0, "none now");

    let subscription = unsafe { sekejap_subscribe(db.db) };
    assert!(subscription >= 0, "a subscription id");

    // Nothing committed yet: polling answers NULL with the status left Ok.
    assert!(unsafe { sekejap_next_change(db.db, subscription, 0) }.is_null());
    assert_eq!(unsafe { sekejap_last_error_code(db.db) }, SekejapStatus::Ok);

    assert_eq!(db.put("m", "one", &json!({ "v": "a" })), 0);
    let event = take_json(unsafe { sekejap_next_change(db.db, subscription, 1_000) });
    assert_eq!(event["sequence"], 1, "the first delivered event");
    assert_eq!(event["keys_total"], 1);
    assert_eq!(event["keys_truncated"], false);
    let keys = event["keys"].as_array().expect("keys").clone();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["key"], "one");
    assert_eq!(keys[0]["kind"], "put");
    assert!(!event["collections"]
        .as_array()
        .expect("collections")
        .is_empty());

    assert_eq!(
        unsafe { sekejap_delete(db.db, c("m").as_ptr(), c("one").as_ptr()) },
        1
    );
    let event = take_json(unsafe { sekejap_next_change(db.db, subscription, 1_000) });
    assert_eq!(event["keys"][0]["kind"], "delete");

    assert_eq!(unsafe { sekejap_unsubscribe(db.db, subscription) }, 1);
    assert_eq!(
        unsafe { sekejap_unsubscribe(db.db, subscription) },
        0,
        "the second close of one subscription: it was not open, which is not a failure"
    );
    assert_eq!(unsafe { sekejap_last_error_code(db.db) }, SekejapStatus::Ok);

    // In service mode the published read view holds a reader slot for its
    // whole life, so a checkpoint is DEFERRED rather than refused.
    assert_eq!(unsafe { sekejap_checkpoint(db.db) }, 0);
    assert_eq!(unsafe { sekejap_publish(db.db) }, 0);
}

// ── §10 ownership and lifetimes ─────────────────────────────────────────────

#[test]
fn every_returned_string_is_freed_by_the_library_and_every_close_is_null_safe() {
    // Null-safe frees and closes, called on NULL and called once on a live
    // handle: the whole ownership rule the header states.
    unsafe {
        sekejap_string_free(ptr::null_mut());
        sekejap_close(ptr::null_mut());
        sekejap_stmt_free(ptr::null_mut());
        sekejap_scan_close(ptr::null_mut());
        sekejap_query_close(ptr::null_mut());
    }

    let dir = tempfile::tempdir().expect("a directory");
    let path = c(dir.path().to_str().unwrap());
    let db = unsafe { sekejap_open(path.as_ptr()) };
    assert!(!db.is_null());
    unsafe {
        sekejap_create_collection(db, c("s").as_ptr(), c("[]").as_ptr());
        sekejap_put(db, c("s").as_ptr(), c("k").as_ptr(), c("{}").as_ptr());
    }

    // Ten thousand answers taken and freed: a leak or a double free here is
    // what the allocator or a sanitizer reports, and the count is what makes
    // either loud.
    for _ in 0..10_000 {
        let answer = unsafe { sekejap_get(db, c("s").as_ptr(), c("k").as_ptr()) };
        assert!(take(answer).is_some());
    }

    // The derived handles are freed BEFORE the database, which is the
    // ordering rule the header states.
    let scan = unsafe { sekejap_scan_open(db, c("s").as_ptr(), 4) };
    let stmt = unsafe { sekejap_prepare(db, c("SELECT _key FROM s").as_ptr()) };
    let tx = unsafe { sekejap_tx_begin(db) };
    assert_eq!(unsafe { sekejap_tx_rollback(tx) }, 0);
    unsafe {
        sekejap_stmt_free(stmt);
        sekejap_scan_close(scan);
        sekejap_close(db);
    }

    // The database reopens with the row still in it: a close is a close, not
    // a discard of committed work.
    let db = unsafe { sekejap_open(path.as_ptr()) };
    assert!(!db.is_null());
    assert_eq!(unsafe { sekejap_count_rows(db, c("s").as_ptr()) }, 1);
    unsafe { sekejap_close(db) };
}

#[test]
fn one_database_handle_serves_two_threads_at_once_and_each_keeps_its_own_error_slot() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = c(dir.path().to_str().unwrap());
    let db = unsafe { sekejap_open(path.as_ptr()) };
    assert!(!db.is_null());
    unsafe {
        sekejap_create_collection(
            db,
            c("shared").as_ptr(),
            c(&json!([{ "name": "n", "kind": "int" }]).to_string()).as_ptr(),
        )
    };
    for n in 0..50 {
        unsafe {
            sekejap_put(
                db,
                c("shared").as_ptr(),
                c(&format!("k{n:02}")).as_ptr(),
                c(&json!({ "n": n }).to_string()).as_ptr(),
            )
        };
    }

    // `Db` is Send + Sync, so the handle may be shared. The raw pointer is
    // not, which is why it is carried as an address and rebuilt per thread.
    let address = db as usize;
    let reader = std::thread::spawn(move || {
        let db = address as *mut SekejapDb;
        let mut seen = 0usize;
        for _ in 0..200 {
            let answer =
                unsafe { sekejap_query(db, c("SELECT _key FROM shared").as_ptr(), ptr::null()) };
            let answer = take_json(answer);
            seen += answer.as_array().expect("an array").len();
        }
        seen
    });
    let failer = std::thread::spawn(move || {
        let db = address as *mut SekejapDb;
        // This thread only ever fails, and reads its OWN slot back.
        for _ in 0..200 {
            assert_eq!(unsafe { sekejap_count_rows(db, c("nope").as_ptr()) }, -1);
            assert_eq!(
                unsafe { sekejap_last_error_code(db) },
                SekejapStatus::Invalid
            );
        }
    });
    assert_eq!(reader.join().expect("the reader thread"), 200 * 50);
    failer.join().expect("the failing thread");

    // This thread's slot was never touched by either of them.
    assert_eq!(unsafe { sekejap_last_error_code(db) }, SekejapStatus::Ok);
    unsafe { sekejap_close(db) };
}
