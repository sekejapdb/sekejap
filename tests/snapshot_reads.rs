//! Correctness tests for snapshot reads (`CoreDB::snapshot` → `ReadSnapshot`).
//!
//! The concurrency benchmark shows snapshots are *fast* (reads don't queue behind
//! writes). These tests show they are *correct*: a snapshot is a consistent
//! point-in-time photo — it reads the base + the overlay frozen at mint time, and
//! never sees writes that land afterward. See
//! `docs/developer/notes/snapshot-reads-design.md`.

use sekejap::CoreDB;
use serde_json::json;

/// Build a disk store with `venues/v0..vN`, compact it into an immutable base,
/// then reopen in paged mode (the mode that makes a store snapshottable).
fn paged_db(n: usize) -> (CoreDB, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        for i in 0..n {
            db.put(
                &format!("venues/v{i}"),
                &json!({"_collection":"venues","_key":format!("v{i}"),"n":i}).to_string(),
            )
            .unwrap();
        }
        db.compact().unwrap();
    }
    (CoreDB::open_paged(dir.path()).unwrap(), dir)
}

/// A snapshot reads nodes that live in the immutable base.
#[test]
fn snapshot_reads_base() {
    let (db, _dir) = paged_db(10);
    let snap = db.snapshot().expect("paged mode is snapshottable");
    let v = snap.get("venues/v3").expect("v3 is in the base");
    assert!(v.contains("\"n\":3"), "unexpected payload: {v}");
    assert!(snap.get("venues/does-not-exist").is_none());
}

/// A snapshot taken AFTER an overlay write sees it; a snapshot taken BEFORE it
/// does not. This is the core isolation guarantee.
#[test]
fn snapshot_is_point_in_time() {
    let (mut db, _dir) = paged_db(10);

    // Photo #1 — before the new write.
    let before = db.snapshot().unwrap();

    // A write lands in the overlay (not yet compacted into the base).
    db.put(
        &format!("venues/v999"),
        &json!({"_collection":"venues","_key":"v999","n":999}).to_string(),
    )
    .unwrap();

    // Photo #2 — after the write.
    let after = db.snapshot().unwrap();

    // The live DB sees it; so does the later snapshot; the earlier one does not.
    assert!(db.get("venues/v999").is_some(), "live db must see its own write");
    assert!(after.get("venues/v999").is_some(), "later snapshot sees the overlay write");
    assert!(
        before.get("venues/v999").is_none(),
        "earlier snapshot must NOT see a write that landed after it was taken"
    );

    // And the earlier snapshot still reads everything that existed when it was minted.
    assert!(before.get("venues/v0").is_some());
}

/// Two snapshots are independent — a write between them changes only the later one,
/// and mutating the live DB never mutates an already-minted snapshot.
#[test]
fn snapshots_are_independent() {
    let (mut db, _dir) = paged_db(5);

    let a = db.snapshot().unwrap();
    db.put(
        "venues/x1",
        &json!({"_collection":"venues","_key":"x1","n":1}).to_string(),
    )
    .unwrap();
    let b = db.snapshot().unwrap();
    db.put(
        "venues/x2",
        &json!({"_collection":"venues","_key":"x2","n":2}).to_string(),
    )
    .unwrap();

    // a: neither x1 nor x2.  b: x1 but not x2.  live: both.
    assert!(a.get("venues/x1").is_none() && a.get("venues/x2").is_none());
    assert!(b.get("venues/x1").is_some() && b.get("venues/x2").is_none());
    assert!(db.get("venues/x1").is_some() && db.get("venues/x2").is_some());
}

/// A snapshot outlives a `compact()` on the live DB: the old base it references
/// stays alive (held by `Arc`) even though the live DB swapped in a new one.
#[test]
fn snapshot_survives_live_compact() {
    let (mut db, _dir) = paged_db(10);
    let snap = db.snapshot().unwrap();

    // Grow the overlay, then compact the live DB (folds overlay into a NEW base and
    // rewrites payloads.bin). The snapshot must keep reading its own frozen view.
    db.put(
        "venues/late",
        &json!({"_collection":"venues","_key":"late","n":42}).to_string(),
    )
    .unwrap();
    db.compact().unwrap();

    // Snapshot still reads the base it was minted against, and still doesn't see the
    // post-snapshot write (its frozen fd/mmap point at the pre-compact inode).
    assert!(snap.get("venues/v5").is_some(), "old base still readable after live compact");
    assert!(snap.get("venues/late").is_none(), "snapshot never sees a post-mint write");
    // The live DB, of course, sees everything.
    assert!(db.get("venues/late").is_some());
}

/// A snapshot can scan a whole collection and count it — base members plus overlay
/// members — and both are point-in-time (a later insert isn't seen by an old snapshot).
#[test]
fn snapshot_scan_and_count() {
    let (mut db, _dir) = paged_db(6); // venues/v0..v5, all folded into the base

    let before = db.snapshot().unwrap();
    assert_eq!(before.count("venues"), 6);
    let payloads = before.scan("venues");
    assert_eq!(payloads.len(), 6);
    assert!(payloads.iter().any(|p| p.contains("\"n\":3")), "scan returns real payloads");

    // Overlay insert: a fresh snapshot merges base + overlay (7); the old one is
    // frozen at 6.
    db.put(
        "venues/v6",
        &json!({"_collection":"venues","_key":"v6","n":6}).to_string(),
    )
    .unwrap();
    let after = db.snapshot().unwrap();
    assert_eq!(after.count("venues"), 7, "new snapshot sees base + overlay");
    assert_eq!(before.count("venues"), 6, "old snapshot is point-in-time");
    assert!(after.scan("venues").iter().any(|p| p.contains("\"n\":6")));
    assert!(!before.scan("venues").iter().any(|p| p.contains("\"n\":6")));

    // Unknown collection → empty, not a panic.
    assert_eq!(before.count("ghosts"), 0);
    assert!(before.scan("ghosts").is_empty());
}

/// Gating / embedded safety: resident mode (`open`) has no immutable base, so it is
/// NOT snapshottable — `snapshot()` returns `None`. This is what keeps the feature
/// opt-in and zero-cost for single-threaded/embedded users.
#[test]
fn resident_mode_is_not_snapshottable() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap(); // resident: topo_base = None
    db.put(
        "venues/v0",
        &json!({"_collection":"venues","_key":"v0","n":0}).to_string(),
    )
    .unwrap();
    assert!(db.snapshot().is_none(), "resident mode must not be snapshottable");
}
