//! Correctness tests for snapshot reads (`CoreDB::snapshot_db`).
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
    let snap = db.snapshot_db().expect("paged mode is snapshottable");
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
    let before = db.snapshot_db().unwrap();

    // A write lands in the overlay (not yet compacted into the base).
    db.put(
        &format!("venues/v999"),
        &json!({"_collection":"venues","_key":"v999","n":999}).to_string(),
    )
    .unwrap();

    // Photo #2 — after the write.
    let after = db.snapshot_db().unwrap();

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

    let a = db.snapshot_db().unwrap();
    db.put(
        "venues/x1",
        &json!({"_collection":"venues","_key":"x1","n":1}).to_string(),
    )
    .unwrap();
    let b = db.snapshot_db().unwrap();
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
    let snap = db.snapshot_db().unwrap();

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

/// A snapshot sees base + overlay members, and stays point-in-time: a later insert
/// is invisible to a snapshot minted before it.
#[test]
fn snapshot_scan_and_count() {
    let (mut db, _dir) = paged_db(6); // venues/v0..v5, all folded into the base
    let count = |snap: &std::sync::Arc<CoreDB>| {
        snap.query("SELECT _key FROM venues").unwrap().collect().len()
    };

    let before = db.snapshot_db().unwrap();
    assert_eq!(count(&before), 6);
    assert!(before.get("venues/v3").unwrap().contains("\"n\":3"), "payloads are readable");

    // Overlay insert: a fresh snapshot merges base + overlay (7); the old one is
    // frozen at 6.
    db.put(
        "venues/v6",
        &json!({"_collection":"venues","_key":"v6","n":6}).to_string(),
    )
    .unwrap();
    let after = db.snapshot_db().unwrap();
    assert_eq!(count(&after), 7, "new snapshot sees base + overlay");
    assert_eq!(count(&before), 6, "old snapshot is point-in-time");
    assert!(after.get("venues/v6").is_some());
    assert!(before.get("venues/v6").is_none());

    // Unknown collection → empty, not a panic.
    assert!(before.query("SELECT _key FROM ghosts").map(|s| s.collect().len()).unwrap_or(0) == 0);
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
    assert!(db.snapshot_db().is_none(), "resident mode must not be snapshottable");
}

/// The payoff: a snapshot `CoreDB` runs the **full indexed query surface** — not
/// just `get`/`scan`/`count` — and stays frozen while the live database moves on.
#[test]
fn snapshot_db_runs_indexed_sql() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("CREATE TABLE venues (_key TEXT PRIMARY KEY, name TEXT, cat TEXT, n INTEGER)")
            .unwrap();
        for i in 0..50 {
            let cat = ["cafe", "bar", "gym"][i % 3];
            db.execute(&format!(
                "INSERT INTO venues (_key, name, cat, n) VALUES ('v{i}', 'Venue {i}', '{cat}', {i})"
            ))
            .unwrap();
        }
        db.execute("CREATE INDEX ON venues USING btree (cat)").unwrap();
        db.execute("CREATE INDEX ON venues USING btree (n)").unwrap();
        db.execute("CREATE INDEX ON venues USING bm25 (name)").unwrap();
        db.compact().unwrap();
    }
    let mut db = CoreDB::open_paged(dir.path()).unwrap();

    let snap = db.snapshot_db().expect("paged mode is snapshottable");

    // Indexed filter, ordering and aggregation all run on the snapshot.
    let cafes = snap.query("SELECT _key FROM venues WHERE cat = 'cafe'").unwrap().collect();
    assert!(!cafes.is_empty(), "indexed WHERE must return rows on a snapshot");
    let all = snap.query("SELECT _key FROM venues").unwrap().collect();
    assert_eq!(all.len(), 50);
    let sorted = snap.query("SELECT _key FROM venues ORDER BY n DESC LIMIT 3").unwrap().collect();
    assert_eq!(sorted.len(), 3);

    // Now the live database moves on...
    db.execute("INSERT INTO venues (_key, name, cat, n) VALUES ('v999', 'Late', 'cafe', 999)")
        .unwrap();

    // ...and the snapshot does not see it (point-in-time), while the live one does.
    let after_snap = snap.query("SELECT _key FROM venues").unwrap().collect();
    assert_eq!(after_snap.len(), 50, "snapshot must stay frozen at 50 rows");
    let after_live = db.query("SELECT _key FROM venues").unwrap().collect();
    assert_eq!(after_live.len(), 51, "live db sees its own write");
}

/// A snapshot must never mutate or compact the real database — its `Drop` in
/// particular (CoreDB::drop compacts when `compact_on_close` + `data_dir` are set).
#[test]
fn snapshot_db_is_inert_on_drop() {
    let (mut db, dir) = paged_db(10);
    db.put("venues/extra", &json!({"_collection":"venues","_key":"extra"}).to_string()).unwrap();

    let wal_before = std::fs::metadata(dir.path().join("wal.log")).map(|m| m.len()).unwrap_or(0);
    {
        let snap = db.snapshot_db().unwrap();
        assert_eq!(snap.query("SELECT _key FROM venues").unwrap().collect().len(), 11);
    } // snapshot dropped here — must not compact or write anything

    let wal_after = std::fs::metadata(dir.path().join("wal.log")).map(|m| m.len()).unwrap_or(0);
    assert_eq!(wal_before, wal_after, "dropping a snapshot must not touch the WAL");
    // The live database is untouched and still usable.
    assert_eq!(db.query("SELECT _key FROM venues").unwrap().collect().len(), 11);
}

/// Stats must report real numbers, not placeholders: sizes reflect the data,
/// counters move when work happens, and snapshot timing is recorded.
#[test]
fn stats_report_reality() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE v (_key TEXT PRIMARY KEY, cat TEXT)").unwrap();
    for i in 0..20 {
        db.execute(&format!("INSERT INTO v (_key, cat) VALUES ('v{i}', 'cafe')")).unwrap();
    }
    db.execute("CREATE INDEX ON v USING btree (cat)").unwrap();

    let s = db.stats();
    assert_eq!(s.nodes, 20, "node count reflects the data");
    assert_eq!(s.collections, 1);
    assert!(s.writes >= 20, "durable writes are counted (got {})", s.writes);
    assert!(s.field_indexes >= 1, "the btree index is reported");
    assert!(s.wal_bytes > 0, "wal size is measured");
    assert!(!s.paged, "a plain open is resident");
    assert_eq!(s.compactions, 0);

    let before = db.stats().queries;
    let _ = db.query("SELECT _key FROM v").unwrap().collect();
    assert_eq!(db.stats().queries, before + 1, "queries are counted");

    db.compact().unwrap();
    let s = db.stats();
    assert_eq!(s.compactions, 1, "compaction is counted");
    assert!(s.last_compact_us > 0, "compaction duration is recorded");
    assert!(s.payload_bytes > 0, "payload size is measured after compact");

    // Paged reopen: snapshot minting is timed, and the overlay is visible.
    drop(db);
    let mut db = CoreDB::open_paged(dir.path()).unwrap();
    db.put("v/extra", &json!({"_collection":"v","_key":"extra"}).to_string()).unwrap();
    let _snap = db.snapshot_db().unwrap();
    let s = db.stats();
    assert!(s.paged, "reopened paged");
    assert_eq!(s.snapshots, 1, "snapshot mints are counted");
    assert!(s.overlay_nodes >= 1, "the write overlay is reported (got {})", s.overlay_nodes);
}

/// **G1 — the transaction boundary.** A snapshot must capture a *committed* state,
/// never a half-applied multi-statement transaction. Writes buffer in `pending_txn`
/// until COMMIT, so a snapshot taken mid-transaction must show none of them, and one
/// taken after COMMIT must show all of them.
#[test]
fn snapshot_never_sees_an_uncommitted_transaction() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("CREATE TABLE v (_key TEXT PRIMARY KEY, n INTEGER)").unwrap();
        db.execute("INSERT INTO v (_key, n) VALUES ('base', 0)").unwrap();
        db.compact().unwrap();
    }
    let mut db = CoreDB::open_paged(dir.path()).unwrap();

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO v (_key, n) VALUES ('t1', 1)").unwrap();
    db.execute("INSERT INTO v (_key, n) VALUES ('t2', 2)").unwrap();

    // Mid-transaction snapshots — neither kind may show the buffered rows.
    let mid_db = db.snapshot_db().expect("snapshottable");
    
    let rows = mid_db.query("SELECT _key FROM v").unwrap().collect();
    assert_eq!(rows.len(), 1, "snapshot_db must not see an open transaction (saw {rows:?})");

    db.execute("COMMIT").unwrap();

    // After COMMIT a fresh snapshot sees everything; the old ones stay frozen.
    let after = db.snapshot_db().unwrap();
    assert_eq!(after.query("SELECT _key FROM v").unwrap().collect().len(), 3);
    assert_eq!(mid_db.query("SELECT _key FROM v").unwrap().collect().len(), 1,
        "a snapshot taken earlier stays frozen after the commit");
    assert_eq!(db.query("SELECT _key FROM v").unwrap().collect().len(), 3);
}

/// A rolled-back transaction must never appear in any snapshot either.
#[test]
fn snapshot_never_sees_a_rolled_back_transaction() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("CREATE TABLE v (_key TEXT PRIMARY KEY, n INTEGER)").unwrap();
        db.execute("INSERT INTO v (_key, n) VALUES ('base', 0)").unwrap();
        db.compact().unwrap();
    }
    let mut db = CoreDB::open_paged(dir.path()).unwrap();

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO v (_key, n) VALUES ('gone', 1)").unwrap();
    db.execute("ROLLBACK").unwrap();

    let snap = db.snapshot_db().unwrap();
    assert_eq!(snap.query("SELECT _key FROM v").unwrap().collect().len(), 1,
        "a rolled-back row must never appear in a snapshot");
    assert_eq!(db.query("SELECT _key FROM v").unwrap().collect().len(), 1);
}
