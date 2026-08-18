//! The scan caps on a shared engine — the guard that keeps one request from
//! reading a whole collection into memory.
//!
//! `EngineBuilder::max_scan_rows` and `max_scan_bytes` are what stand between a
//! list endpoint and an out-of-memory kill on a service. They had no tests.
//!
//! The byte cap carries a promise that is easy to get wrong in either direction:
//! it must bound the total, **and** it must always return at least one row, so a
//! single payload larger than the whole budget comes back rather than vanishing.
//! A cap that silently returns nothing for a big row is a wrong answer; a cap
//! that ignores the budget is not a cap.

use sekejap::engine::Engine;
use sekejap::CoreDB;

/// 40 rows, each a little over 1 KB, plus one deliberately enormous one.
fn fixture(dir: &std::path::Path) {
    let mut db = CoreDB::open(dir).unwrap();
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, body TEXT)").unwrap();
    for i in 0..40 {
        db.put(&format!("p/n{i}"), &serde_json::json!({
            "_collection": "p", "_key": format!("n{i}"), "body": "x".repeat(1_024),
        }).to_string()).unwrap();
    }
    db.execute("CREATE TABLE big (_key TEXT PRIMARY KEY, body TEXT)").unwrap();
    db.put("big/whale", &serde_json::json!({
        "_collection": "big", "_key": "whale", "body": "y".repeat(200_000),
    }).to_string()).unwrap();
    db.compact().unwrap();
}

#[test]
fn the_row_cap_bounds_a_scan() {
    let dir = tempfile::TempDir::new().unwrap();
    fixture(dir.path());
    let path = dir.path().to_str().unwrap();

    // One engine at a time: the directory lock is exclusive, and holding two
    // open at once is what it exists to prevent.
    {
        let uncapped = Engine::builder(path).build().unwrap();
        assert_eq!(uncapped.scan("p").len(), 40, "an uncapped scan must return everything");
    }
    {
        let capped = Engine::builder(path).max_scan_rows(10).build().unwrap();
        assert_eq!(capped.scan("p").len(), 10, "the row cap did not bound the scan");
    }
    {
        // A cap above the collection size is not a floor.
        let generous = Engine::builder(path).max_scan_rows(1_000).build().unwrap();
        assert_eq!(generous.scan("p").len(), 40);
    }
}

#[test]
fn the_byte_cap_bounds_a_scan() {
    let dir = tempfile::TempDir::new().unwrap();
    fixture(dir.path());
    let path = dir.path().to_str().unwrap();

    // Roughly five rows' worth. The exact count depends on payload framing, so
    // the assertion is on the property — bounded, and not empty — rather than on
    // a number that would break the first time a field is added.
    let capped = Engine::builder(path).max_scan_bytes(5_500).build().unwrap();
    let got = capped.scan("p");
    assert!(!got.is_empty(), "the byte cap returned nothing at all");
    assert!(got.len() < 40, "the byte cap did not bound the scan: {} rows", got.len());
    let total: usize = got.iter().map(|s| s.len()).sum();
    assert!(total <= 5_500 + 2_048,
            "the scan returned {total} bytes against a 5 500-byte cap");
}

/// **One row larger than the entire budget still comes back.**
///
/// The alternative is a list endpoint that silently returns nothing for a
/// collection whose only row is big — a wrong answer dressed as an empty result.
/// The guard is a single `!out.is_empty()` in the bytes check, and nothing tested
/// that it was there.
#[test]
fn a_single_oversized_row_is_not_silently_dropped() {
    let dir = tempfile::TempDir::new().unwrap();
    fixture(dir.path());
    let path = dir.path().to_str().unwrap();

    // A budget far smaller than the one row in the collection.
    let capped = Engine::builder(path).max_scan_bytes(64).build().unwrap();
    let got = capped.scan("big");
    assert_eq!(got.len(), 1,
               "a row bigger than the whole byte budget was dropped instead of \
                returned — the caller sees an empty collection");
    assert!(got[0].len() > 100_000, "the row came back truncated");
}

/// The caps must behave the same whether the read is served from a published
/// snapshot or from under the lock — those are two different code paths in
/// `scan`, and a cap that only applies to one of them is not a cap.
#[cfg(unix)]
#[test]
fn the_caps_apply_on_the_snapshot_path_too() {
    let dir = tempfile::TempDir::new().unwrap();
    fixture(dir.path());
    let path = dir.path().to_str().unwrap();

    {
        let locked = Engine::builder(path).max_scan_rows(7).build().unwrap();
        assert_eq!(locked.scan("p").len(), 7);
    }
    {
        let snapshot = Engine::builder(path)
            .snapshot_reads(true)
            .max_scan_rows(7)
            .build()
            .unwrap();
        assert_eq!(snapshot.scan("p").len(), 7,
                   "the row cap is not applied when reads are served from a snapshot");
    }
}

// ── the snapshot the service reads through ──────────────────────────────────

/// **`refresh_snapshot` makes a write visible, and without it a read may not be.**
///
/// Snapshot reads are the point of the service mode: a `get` does not queue
/// behind a writer. The cost is staleness — the published snapshot is re-minted
/// on a debounce, so a write is not immediately visible through it. That is the
/// trade, and `refresh_snapshot` is the escape hatch the docs point at for
/// read-your-own-write.
///
/// Nothing tested either half. A `refresh_snapshot` that did not actually
/// re-mint would leave a caller reading stale data forever with the documented
/// remedy in hand and no way to tell it was not working.
#[cfg(unix)]
#[test]
fn refresh_snapshot_makes_a_write_visible() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        // Resident: the layout that can be snapshotted. `CoreDB::open` gives the
        // paged layout, whose files are written in place and therefore cannot be
        // shared with a reader — the engine would fall back to locked reads and
        // this test would be exercising the fallback.
        let mut db = CoreDB::open_with_config(dir.path(), sekejap::Config::resident()).unwrap();
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER)").unwrap();
        db.put("p/a", r#"{"_collection":"p","_key":"a","n":1}"#).unwrap();
        db.compact().unwrap();
    }

    let engine = Engine::builder(dir.path().to_str().unwrap())
        .snapshot_reads(true)
        // A long debounce, so the snapshot cannot re-mint on its own and the
        // test is measuring `refresh_snapshot` rather than a timer.
        .publish_interval(std::time::Duration::from_secs(3_600))
        .build()
        .unwrap();

    assert!(engine.get("p/a").is_some(), "the pre-existing row is not visible");

    engine.execute("INSERT INTO p (_key, n) VALUES ('b', 2)").unwrap();

    // Whether `b` is visible before the refresh is a timing question and not
    // something to assert on. What must hold is that it *is* visible after.
    engine.refresh_snapshot();
    assert!(engine.get("p/b").is_some(),
            "a row inserted through the engine is still not visible after \
             refresh_snapshot — the documented remedy for read-your-own-write \
             does not work");

    // The write is real, not just visible through the snapshot.
    assert_eq!(engine.count("p"), 2, "count disagrees with get after a refresh");
}

// ── public API that nothing had ever called ─────────────────────────────────

/// `Engine::memory()` — the ephemeral engine. A whole constructor with no test
/// behind it, which is the state in which a constructor quietly stops working.
#[test]
fn an_in_memory_engine_works() {
    let m = Engine::memory();
    m.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER)").unwrap();
    for i in 0..5 {
        m.execute(&format!("INSERT INTO p (_key, n) VALUES ('n{i}', {i})")).unwrap();
    }
    assert_eq!(m.count("p"), 5);
    assert!(m.get("p/n3").is_some());
    assert_eq!(m.scan("p").len(), 5);
}

/// `begin_bulk` / `end_bulk` defer the per-write log sync and flush once. The
/// deferral is the whole point, so what has to be true is that it *ends*: the
/// batch is durable after `end_bulk`, not merely present in memory.
#[test]
fn a_bulk_batch_is_durable_after_end_bulk() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER)").unwrap();
        db.begin_bulk();
        for i in 0..50 {
            db.put(&format!("p/n{i}"),
                   &format!(r#"{{"_collection":"p","_key":"n{i}","n":{i}}}"#)).unwrap();
        }
        db.end_bulk();
    }
    let db = CoreDB::open(dir.path()).unwrap();
    assert_eq!(db.query("SELECT _key FROM p").unwrap().collect().len(), 50,
               "a batch written between begin_bulk and end_bulk did not survive");
}

/// `link_meta_many` — the batch edge writer that carries attributes, and one of
/// the three writers that ignored read-only until this branch. It had no test of
/// its own: that the edges land, that a `None` takes the naked-edge lane, and
/// that the attributes survive a restart.
#[test]
fn link_meta_many_writes_edges_and_their_attributes() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER)").unwrap();
        for i in 0..4 {
            db.put(&format!("p/n{i}"),
                   &format!(r#"{{"_collection":"p","_key":"n{i}","n":{i}}}"#)).unwrap();
        }
        db.link_meta_many(vec![
            ("p/n0", "p/n1", "next", Some(r#"{"w":1}"#)),
            ("p/n1", "p/n2", "next", None),          // the naked-edge lane
            ("p/n2", "p/n3", "next", Some(r#"{"w":3}"#)),
        ]).unwrap();
    }
    let db = CoreDB::open(dir.path()).unwrap();
    assert_eq!(db.edges_from("p/n0").len(), 1);
    assert_eq!(db.edges_from("p/n1").len(), 1);
    assert_eq!(db.edges_from("p/n2").len(), 1);
    let meta = db.edges_from("p/n0").first().and_then(|e| e.meta.clone())
        .expect("the attributes on the first edge did not survive a restart");
    assert_eq!(meta["w"].as_i64(), Some(1));
    // The `None` entry is a real edge with no attributes, not a missing one.
    assert!(db.edges_from("p/n1").first().is_some_and(|e| e.meta.is_none()),
            "the naked-edge lane invented attributes");
}
