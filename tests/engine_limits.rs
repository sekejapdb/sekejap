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
