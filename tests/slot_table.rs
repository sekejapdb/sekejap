//! `slots.bin` — the indirection between a record's identity and its location.
//!
//! Today it is the identity mapping, because there is one segment. These tests pin
//! the two things that must hold before a second segment can exist: the table is
//! actually written and consulted, and a store that predates it still reads.

use sekejap::{Config, CoreDB};
use serde_json::json;

fn build(dir: &std::path::Path, n: usize) {
    // Resident. `slots.bin` maps a node to a **byte offset** in `payloads.bin`,
    // which is a fact about the append-only payload layout: paged payloads
    // address records by id and have no offsets to tabulate. Building this in the
    // default layout would be asking whether a file exists that is not supposed
    // to. `open_paged` below is `paged_topology` alone, which is the mode the
    // table is read in.
    let mut db = CoreDB::open_with_config(dir, Config::resident()).unwrap();
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER)").unwrap();
    for i in 0..n {
        db.put(&format!("p/n{i}"),
               &json!({"_collection":"p","_key":format!("n{i}"),"n":i as i64}).to_string()).unwrap();
    }
    for i in 0..n - 1 {
        db.link(&format!("p/n{i}"), &format!("p/n{}", i + 1), "next");
    }
    db.compact().unwrap();
}

fn shape(db: &CoreDB) -> (usize, usize, usize) {
    (
        db.query("SELECT _key FROM p").unwrap().collect().len(),
        db.query("SELECT b._key FROM MATCH (a:p)-[:next]->(b:p)").unwrap().collect().len(),
        db.one("p/n0").forward("next").collect().len(),
    )
}

/// Compaction must write the table, sized to the store.
#[test]
fn compaction_writes_a_slot_table() {
    let dir = tempfile::TempDir::new().unwrap();
    build(dir.path(), 40);

    let path = dir.path().join("slots.bin");
    assert!(path.exists(), "compaction did not write slots.bin");

    // header (16) + count (8) + one u64 per slot
    let len = std::fs::metadata(&path).unwrap().len() as usize;
    assert_eq!(len, 16 + 8 + 40 * 8, "table is not sized to the record count");
}

/// A database written before `slots.bin` existed must still read.
///
/// Absent, the table means the identity mapping — which is exactly what a
/// single-segment store is. This is what lets the file be introduced without a
/// migration, so it is worth pinning rather than assuming.
#[test]
fn a_store_without_a_slot_table_still_reads() {
    let dir = tempfile::TempDir::new().unwrap();
    build(dir.path(), 40);
    let expected = shape(&CoreDB::open_paged(dir.path()).unwrap());
    assert_eq!(expected, (40, 39, 1), "baseline is wrong: {expected:?}");

    // Delete it, exactly as an older database would not have one.
    std::fs::remove_file(dir.path().join("slots.bin")).unwrap();

    let db = CoreDB::open_paged(dir.path()).unwrap();
    assert_eq!(shape(&db), expected, "a store with no slot table reads differently");
    assert!(db.get("p/n7").is_some(), "point read failed without the table");
}

/// A damaged table must be refused, not half-read.
///
/// It resolves every identity in the store, so serving it from a truncated file
/// would answer with whatever bytes happen to follow. Falling back to the identity
/// mapping is correct here precisely because there is one segment.
#[test]
fn a_truncated_slot_table_does_not_corrupt_reads() {
    let dir = tempfile::TempDir::new().unwrap();
    build(dir.path(), 40);
    let expected = shape(&CoreDB::open_paged(dir.path()).unwrap());

    let path = dir.path().join("slots.bin");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.truncate(bytes.len() - 80); // lose the last ten entries
    std::fs::write(&path, &bytes).unwrap();

    let db = CoreDB::open_paged(dir.path()).unwrap();
    assert_eq!(shape(&db), expected, "a truncated table changed what the store returns");
}

/// The table has to survive the operations that rewrite the store.
#[test]
fn the_table_tracks_the_store_across_compactions() {
    let dir = tempfile::TempDir::new().unwrap();
    build(dir.path(), 40);

    let mut db = CoreDB::open_paged(dir.path()).unwrap();
    for i in 40..60 {
        db.put(&format!("p/n{i}"),
               &json!({"_collection":"p","_key":format!("n{i}"),"n":i as i64}).to_string()).unwrap();
    }
    db.execute("DELETE FROM p WHERE n IN (1,2,3)").unwrap();
    db.compact().unwrap();
    drop(db);

    let live = 40 + 20 - 3;
    let len = std::fs::metadata(dir.path().join("slots.bin")).unwrap().len() as usize;
    assert_eq!(len, 16 + 8 + live * 8, "table did not track the store through a compaction");

    let db = CoreDB::open_paged(dir.path()).unwrap();
    assert_eq!(db.query("SELECT _key FROM p").unwrap().collect().len(), live);
    assert!(db.get("p/n50").is_some(), "a row written before the compaction is gone");
    assert!(db.get("p/n2").is_none(), "a deleted row came back");
}

/// The verify-before-commit rail has to know how big the store is.
///
/// Compaction writes a new generation, reads it back from disk, and only then
/// drops the old files and truncates the log. That readback compares against a
/// count taken beforehand — so if the count is too small, the rail passes
/// whatever it is handed and the safety net is not there.
///
/// It was taken from the RAM overlay, which was correct while compaction began by
/// hydrating the base into it. Hydration was removed (Law 1: compacting a store
/// must not require holding it in memory), and the count silently became "writes
/// since the last compaction" — 20 out of 60 in the setup below.
#[test]
fn the_compaction_expectation_spans_the_base_not_just_the_overlay() {
    let dir = tempfile::TempDir::new().unwrap();
    build(dir.path(), 40);

    let mut db = CoreDB::open_paged(dir.path()).unwrap();
    // Twenty new rows and nineteen edges live in the overlay; the other forty rows
    // and thirty-nine edges are in the immutable base and are never touched, so a
    // count taken from the overlay alone sees a third of the store.
    for i in 40..60 {
        db.put(&format!("p/n{i}"),
               &json!({"_collection":"p","_key":format!("n{i}"),"n":i as i64}).to_string()).unwrap();
        if i > 40 { db.link(&format!("p/n{}", i - 1), &format!("p/n{i}"), "next"); }
    }

    let (nodes, edges) = db.compaction_expectation();
    assert_eq!(nodes, 60,
               "compaction expects {nodes} nodes where the store holds 60 — the base \
                is not being counted, so the readback cannot fail");
    assert_eq!(edges, 39 + 19,
               "compaction expects {edges} edges where the store holds 58 — the \
                base's edges are not being counted");

    // And a delete against the base has to come off the expectation, because
    // compaction is where that delete finally takes effect.
    db.execute("DELETE FROM p WHERE n IN (1,2,3)").unwrap();
    let (nodes, _) = db.compaction_expectation();
    assert_eq!(nodes, 57, "a row deleted from the base is still expected to survive");

    db.compact().unwrap();
    assert_eq!(db.query("SELECT _key FROM p").unwrap().collect().len(), 57);
}
