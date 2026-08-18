//! Correctness tests for snapshot reads (`CoreDB::snapshot_db`).
//!
//! The concurrency benchmark shows snapshots are *fast* (reads don't queue behind
//! writes). These tests show they are *correct*: a snapshot is a consistent
//! point-in-time photo — it reads the base + the overlay frozen at mint time, and
//! never sees writes that land afterward. See
//! `docs/developer/notes/snapshot-reads-design.md`.
//!
//! Every fixture here builds its store with [`Config::resident`], not the
//! default. Snapshots need a durable half that is *immutable* — compaction
//! writes a new generation and swaps it in, so a snapshot holding the old one
//! keeps reading. The default layout writes nodes and adjacency **in place**, so
//! `snapshot_db` declines and reads fall back to taking the lock. Building these
//! in the default layout would test the fallback, not the feature.

use sekejap::{Config, CoreDB};
use serde_json::json;

/// Build a disk store with `venues/v0..vN`, compact it into an immutable base,
/// then reopen in paged mode (the mode that makes a store snapshottable).
fn paged_db(n: usize) -> (CoreDB, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
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
    let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap(); // resident: topo_base = None
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
        let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
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
    let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
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
        let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
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
        let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
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

/// **Stabilisation: every index family must behave identically on a snapshot and on
/// the live database.** A snapshot clones each index; this proves the clones are
/// functional, not merely present.
#[test]
fn snapshot_matches_live_for_every_index_family() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
        db.execute(
            "CREATE TABLE p (_key TEXT PRIMARY KEY, name TEXT, cat TEXT, n INTEGER, \
             descr TEXT, geometry GEO, emb VECTOR)",
        ).unwrap();
        for i in 0..60 {
            let cat = ["cafe", "bar", "gym"][i % 3];
            let lon = 115.0 + (i as f64) * 0.001;
            let lat = -8.6 - (i as f64) * 0.001;
            db.execute(&format!(
                "INSERT INTO p (_key, name, cat, n, descr, geometry, emb) VALUES \
                 ('p{i}', 'Place {i}', '{cat}', {i}, 'grilled chicken number {i}', \
                  '{{\"type\":\"Point\",\"coordinates\":[{lon},{lat}]}}', [{}, {}, {}])",
                (i % 7) as f64 / 7.0, (i % 5) as f64 / 5.0, (i % 3) as f64 / 3.0
            )).unwrap();
        }
        // graph edges
        for i in 0..30 {
            db.execute(&format!("INSERT ('p/p{i}')-[:near]->('p/p{}')", i + 1)).unwrap();
        }
        db.execute("CREATE INDEX ON p USING btree (cat)").unwrap();
        db.execute("CREATE INDEX ON p USING gin (name)").unwrap();
        db.execute("CREATE INDEX ON p USING bm25 (descr)").unwrap();
        db.execute("CREATE INDEX ON p USING search (descr)").unwrap();
        db.execute("CREATE INDEX ON p USING spatial (geometry)").unwrap();
        db.execute("CREATE INDEX ON p USING hnsw (emb)").unwrap();
        db.compact().unwrap();
    }
    let db = CoreDB::open_paged(dir.path()).unwrap();
    let snap = db.snapshot_db().expect("snapshottable");

    let cases: Vec<(&str, &str)> = vec![
        ("scalar btree",   "SELECT _key FROM p WHERE cat = 'cafe'"),
        ("range",          "SELECT _key FROM p WHERE n > 40"),
        ("order+limit",    "SELECT _key FROM p ORDER BY n DESC LIMIT 5"),
        ("aggregate",      "SELECT cat, COUNT(*) FROM p GROUP BY cat"),
        ("graph MATCH",    "SELECT b._key AS k FROM MATCH (a:p)-[:near]->(b:p) WHERE a._key = 'p1'"),
        ("graph 3-hop",    "SELECT b._key AS k FROM MATCH (a:p)-[:near*1..3]->(b:p) WHERE a._key = 'p1'"),
        ("ILIKE (trigram)","SELECT _key FROM p WHERE name ILIKE '%lace 1%'"),
        ("BM25",           "SELECT _key FROM p WHERE BM25(descr, 'grilled chicken') > 0 LIMIT 5"),
        ("SEARCH (phrase)", "SELECT _key FROM p WHERE SEARCH('grilled chicken') LIMIT 5"),
        ("spatial",        "SELECT _key FROM p WHERE ST_DWithin(geometry, POINT(115.01 -8.61), 5000.0)"),
        ("vector",         "SELECT _key FROM p WHERE VECTOR_NEAR(emb, [0.5, 0.5, 0.5], 5)"),
    ];

    let mut broken = vec![];
    for (label, sql) in cases {
        let live = db.query(sql).map(|s| s.collect().len());
        let snp = snap.query(sql).map(|s| s.collect().len());
        match (live, snp) {
            (Ok(l), Ok(s)) if l == s => println!("  ok       {label:16} {l} rows"),
            (Ok(l), Ok(s)) => { println!("  MISMATCH {label:16} live={l} snapshot={s}"); broken.push(label); }
            (Ok(l), Err(e)) => { println!("  SNAP-ERR {label:16} live={l} snapshot error: {e}"); broken.push(label); }
            (Err(e), _) => println!("  (skip)   {label:16} unsupported on live: {e}"),
        }
    }
    assert!(broken.is_empty(), "index families broken on a snapshot: {broken:?}");
}

/// **Regression: paged compaction must not destroy the base.**
///
/// `compact()` rebuilds payloads.bin and the topology from the RAM overlay. In
/// paged mode the overlay is only what has been written since the last compact, so
/// without hydrating the base first, compacting wrote a store containing just the
/// recent writes — every base-resident node was silently lost on the next open.
/// `open_as_service` uses paged mode with auto-compaction on, so this destroyed a
/// service's data at the first auto-compact.
#[test]
fn paged_compaction_preserves_base_nodes() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
        for i in 0..5 {
            db.put(&format!("v/n{i}"), &json!({"_collection":"v","_key":format!("n{i}")}).to_string()).unwrap();
        }
        db.compact().unwrap(); // all five now live in the immutable base
    }

    // (a) compacting with an EMPTY overlay must keep all five.
    let mut db = CoreDB::open_paged(dir.path()).unwrap();
    assert_eq!(db.query("SELECT _key FROM v").unwrap().collect().len(), 5);
    db.compact().unwrap();
    drop(db);
    let db = CoreDB::open_paged(dir.path()).unwrap();
    assert_eq!(db.query("SELECT _key FROM v").unwrap().collect().len(), 5,
        "compacting a paged store must not drop base nodes");
    assert!(db.get("v/n3").is_some(), "payloads must still be readable after compaction");
    drop(db);

    // (b) base + overlay writes must both survive.
    let mut db = CoreDB::open_paged(dir.path()).unwrap();
    db.put("v/extra", &json!({"_collection":"v","_key":"extra"}).to_string()).unwrap();
    assert_eq!(db.query("SELECT _key FROM v").unwrap().collect().len(), 6);
    db.compact().unwrap();
    drop(db);
    let db = CoreDB::open_paged(dir.path()).unwrap();
    assert_eq!(db.query("SELECT _key FROM v").unwrap().collect().len(), 6,
        "base and overlay must both survive compaction");
    assert!(db.get("v/extra").is_some() && db.get("v/n0").is_some());
}

/// **G4: deleting a node that lives in the immutable base actually deletes it.**
///
/// The base cannot be edited in place, so a delete records a tombstone that every
/// base-aware lookup honours until the next compaction folds it away. Previously the
/// delete was silently ignored, and after compaction it degraded into a phantom row
/// — gone from `get`, still returned by queries.
#[test]
fn deleting_a_base_node_takes_effect() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
        for i in 0..5 {
            db.put(&format!("v/n{i}"), &json!({"_collection":"v","_key":format!("n{i}")}).to_string()).unwrap();
        }
        db.compact().unwrap();
    }
    let count = |db: &CoreDB| db.query("SELECT _key FROM v").unwrap().collect().len();

    let mut db = CoreDB::open_paged(dir.path()).unwrap();
    db.remove("v/n2");

    // Immediately: invisible to get, contains, scans — and to a snapshot.
    assert!(db.get("v/n2").is_none(), "a removed base node must not be readable");
    assert!(!db.contains("v/n2"));
    assert_eq!(count(&db), 4);
    let snap = db.snapshot_db().unwrap();
    assert_eq!(count(&snap), 4, "a snapshot inherits the tombstone");
    assert!(snap.get("v/n2").is_none());
    drop(snap);

    // Durable: the tombstone survives a reopen via WAL replay.
    drop(db);
    let mut db = CoreDB::open_paged(dir.path()).unwrap();
    assert!(db.get("v/n2").is_none(), "the delete must survive a reopen");
    assert_eq!(count(&db), 4);

    // And it is folded away for real by compaction.
    db.compact().unwrap();
    drop(db);
    let db = CoreDB::open_paged(dir.path()).unwrap();
    assert!(db.get("v/n2").is_none());
    assert_eq!(count(&db), 4, "the other four survive compaction");
}

/// **Guard rail.** Compaction rewrites the whole store, so a bug there destroys
/// data silently — which is exactly what happened. `compact()` now counts what must
/// survive and returns an error rather than reporting success on a lossy rewrite.
/// This test pins the counting contract in both storage modes.
#[test]
fn compaction_never_reports_success_after_losing_data() {
    for paged in [false, true] {
        let dir = tempfile::TempDir::new().unwrap();
        {
            let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
            for i in 0..25 {
                let c = if i % 2 == 0 { "a" } else { "b" };
                db.put(&format!("{c}/n{i}"),
                    &json!({"_collection":c,"_key":format!("n{i}")}).to_string()).unwrap();
            }
            db.compact().unwrap();
        }
        let mut db = if paged {
            CoreDB::open_paged(dir.path()).unwrap()
        } else {
            CoreDB::open_with_config(dir.path(), Config::resident()).unwrap()
        };
        db.put("a/late", &json!({"_collection":"a","_key":"late"}).to_string()).unwrap();

        let before = db.node_count();
        assert_eq!(before, 26, "paged={paged}: node_count must see base + overlay");

        // Repeated compaction must be idempotent and lossless, and must not error.
        for round in 0..3 {
            db.compact().unwrap_or_else(|e| panic!("paged={paged} round={round}: {e}"));
            assert_eq!(db.node_count(), before, "paged={paged} round={round}: nodes lost");
        }

        // ...and it must still be true after a reopen, which is where the original
        // bug actually showed up.
        drop(db);
        let db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
        assert_eq!(db.node_count(), before, "paged={paged}: data lost across reopen");
        assert_eq!(db.collection_names(), vec!["a".to_string(), "b".to_string()]);
    }
}

/// The overlay maps are not the store in paged mode. Any accessor that enumerates
/// them directly under-reports — three public ones did (`node_count`,
/// `collection_names`, and `SHOW`'s schema inference), each returning empty for a
/// store whose data all lived in the base. Pin the parity between modes.
#[test]
fn accessors_report_the_same_in_both_storage_modes() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
        for i in 0..12 {
            let c = if i < 7 { "alpha" } else { "beta" };
            db.put(&format!("{c}/n{i}"), &json!({"_collection":c,"_key":format!("n{i}")}).to_string()).unwrap();
        }
        db.compact().unwrap();
    }
    // One writer at a time — the exclusive lock is real, so measure them in turn.
    let (r_nodes, r_colls, r_stats, r_alpha) = {
        let db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
        (db.node_count(), db.collection_names(), db.stats().nodes,
         db.query("SELECT _key FROM alpha").unwrap().collect().len())
    };
    let paged = CoreDB::open_paged(dir.path()).unwrap();

    assert_eq!(paged.node_count(), r_nodes, "node_count differs by mode");
    assert_eq!(paged.node_count(), 12);
    assert_eq!(paged.collection_names(), r_colls, "collection_names differs by mode");
    assert_eq!(paged.collection_names(), vec!["alpha".to_string(), "beta".to_string()]);
    assert_eq!(paged.stats().nodes, r_stats, "stats differ by mode");
    assert_eq!(paged.query("SELECT _key FROM alpha").unwrap().collect().len(), r_alpha);
}

/// **P1.24 pre-condition.** Index maintenance is deferred during bulk writes
/// (`defer_index_rebuild` + the `dirty_*` sets, flushed at the end). A snapshot must
/// never observe that half-built state: it must either see the indexes complete, or
/// not see the writes at all — never rows present with an index that omits them.
///
/// Written BEFORE extending deferral to single writes, so the guarantee is pinned
/// first rather than assumed.
#[test]
fn snapshot_never_sees_a_half_built_index() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
        db.execute("CREATE TABLE d (_key TEXT PRIMARY KEY, body TEXT)").unwrap();
        for i in 0..30 {
            db.execute(&format!(
                "INSERT INTO d (_key, body) VALUES ('d{i}', 'grilled chicken number {i}')"
            )).unwrap();
        }
        db.execute("CREATE INDEX ON d USING bm25 (body)").unwrap();
        db.execute("CREATE INDEX ON d USING search (body)").unwrap();
        db.compact().unwrap();
    }
    let mut db = CoreDB::open_paged(dir.path()).unwrap();

    let bm25 = |db: &CoreDB| db
        .query("SELECT _key FROM d WHERE BM25(body,'grilled chicken') > 0")
        .map(|s| s.collect().len()).unwrap_or(0);
    let search = |db: &CoreDB| db
        .query("SELECT _key FROM d WHERE SEARCH('grilled chicken')")
        .map(|s| s.collect().len()).unwrap_or(0);
    let rows = |db: &CoreDB| db.query("SELECT _key FROM d").unwrap().collect().len();

    let before = db.snapshot_db().unwrap();
    assert_eq!(rows(&before), 30);
    assert_eq!(bm25(&before), 30, "baseline: every row is indexed");
    assert_eq!(search(&before), 30);

    // A bulk write — this is the path that defers index maintenance.
    let batch: Vec<(String, serde_json::Value)> = (30..60)
        .map(|i| (format!("d/d{i}"),
                  json!({"_collection":"d","_key":format!("d{i}"),"body":"grilled chicken number {i}"})))
        .collect();
    db.put_value_bulk(batch).unwrap();

    // The earlier snapshot is frozen: it must still be internally consistent.
    assert_eq!(rows(&before), 30, "old snapshot unchanged");
    assert_eq!(bm25(&before), 30, "old snapshot's index still matches its rows");

    // A snapshot taken after the bulk write must see rows and indexes agreeing —
    // this is the half-built-index guarantee.
    let after = db.snapshot_db().unwrap();
    let r = rows(&after);
    assert_eq!(r, 60, "new snapshot sees the bulk rows");
    assert_eq!(bm25(&after), r, "BM25 index covers every row the snapshot can see");
    assert_eq!(search(&after), r, "search index covers every row the snapshot can see");

    // And the live database agrees.
    assert_eq!(bm25(&db), rows(&db));
}

/// Rebuilding a text index in paged mode must not drop the base.
///
/// In paged mode `self.nodes` is only the *write overlay* — the rows written since
/// the last compaction. Every index builder that enumerated it therefore rebuilt an
/// index covering a fraction of the database and silently discarded the rest. This
/// pins all four builders (bm25, gin, trigram, positional search) to the base-aware
/// enumeration, so the fallacy cannot come back one builder at a time.
#[test]
fn rebuilding_an_index_in_paged_mode_keeps_the_base() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
        db.execute("CREATE TABLE d (_key TEXT PRIMARY KEY, body TEXT)").unwrap();
        for i in 0..30 {
            db.execute(&format!(
                "INSERT INTO d (_key, body) VALUES ('d{i}', 'grilled chicken number {i}')"
            )).unwrap();
        }
        db.compact().unwrap();
    }

    // Reopened paged: all 30 rows live in the mmap'd base, the overlay is empty.
    let mut db = CoreDB::open_paged(dir.path()).unwrap();
    assert_eq!(db.query("SELECT _key FROM d").unwrap().collect().len(), 30);

    let count = |db: &CoreDB, sql: &str| db.query(sql).map(|s| s.collect().len()).unwrap_or(0);

    db.build_bm25_index("body");
    assert_eq!(
        count(&db, "SELECT _key FROM d WHERE BM25(body,'grilled chicken') > 0"), 30,
        "BM25 rebuild kept the base documents",
    );

    db.build_gin_index("body");
    assert_eq!(
        count(&db, "SELECT _key FROM d WHERE body ILIKE '%chicken%'"), 30,
        "GIN rebuild kept the base documents",
    );

    db.build_text_indexes();
    assert_eq!(
        count(&db, "SELECT _key FROM d WHERE body ILIKE '%chicken%'"), 30,
        "trigram rebuild kept the base documents",
    );

    db.execute("CREATE INDEX ON d USING search (body)").unwrap();
    assert_eq!(
        count(&db, "SELECT _key FROM d WHERE SEARCH('grilled chicken')"), 30,
        "positional search build kept the base documents",
    );
}

/// Updating a row that lives in the compacted base must take effect.
///
/// The planner matched such rows correctly, but the update path then looked them up
/// in `self.nodes` — the write overlay — and dropped every one of them. The
/// statement reported 0 rows matched and the write was lost, with no error. This is
/// the same base/overlay fallacy as the accessor and index-builder bugs, in the one
/// place where it destroys data the caller believed was written.
#[test]
fn updating_a_base_node_takes_effect() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
        db.execute("CREATE TABLE d (_key TEXT PRIMARY KEY, body TEXT, n INTEGER)").unwrap();
        for i in 0..5 {
            db.execute(&format!(
                "INSERT INTO d (_key, body, n) VALUES ('d{i}', 'alpha {i}', {i})"
            )).unwrap();
        }
        db.compact().unwrap();
    }

    // Every row now lives in the immutable base; the overlay is empty.
    let mut db = CoreDB::open_paged(dir.path()).unwrap();
    let matched = db.execute("UPDATE d SET body = 'quebec' WHERE _key = 'd0'").unwrap();
    assert_eq!(matched, 1, "UPDATE matched no rows in paged mode");
    assert!(db.get("d/d0").unwrap().contains("quebec"), "the row still reads as it was");

    // A multi-row update over base rows, and one that must survive a restart.
    let matched = db.execute("UPDATE d SET body = 'sierra' WHERE n >= 3").unwrap();
    assert_eq!(matched, 2, "range UPDATE matched the wrong number of base rows");
    db.compact().unwrap();
    drop(db);

    let db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
    assert!(db.get("d/d0").unwrap().contains("quebec"), "update lost across a reopen");
    assert!(db.get("d/d4").unwrap().contains("sierra"), "range update lost across a reopen");
    assert!(db.get("d/d1").unwrap().contains("alpha 1"), "an untouched row changed");
}

/// The graph surface must answer the same in both storage modes.
///
/// Paged mode keeps compacted rows in an immutable mmap and only recent writes in
/// RAM. Read paths that consult the RAM map alone see a fraction of the database
/// and report it as the whole truth — `SHORTEST` found no path, edge introspection
/// came back empty, and none of it raised an error. This runs the same graph
/// queries against both modes and requires identical answers.
#[test]
fn the_graph_surface_agrees_in_both_storage_modes() {
    use serde_json::json;

    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
        for i in 0..6 {
            db.put(&format!("p/n{i}"), &json!({
                "_collection": "p", "_key": format!("n{i}"), "name": format!("node {i}")
            }).to_string()).unwrap();
        }
        for i in 0..5 {
            db.link(&format!("p/n{i}"), &format!("p/n{}", i + 1), "next");
        }
        db.compact().unwrap();   // everything now lives in the base
    }

    let measure = |db: &CoreDB| -> (usize, usize, usize, usize, usize, usize) {
        let one_hop = db.one("p/n0").forward("next").collect().len();
        let five_hop = db.one("p/n0").forward("next").forward("next").forward("next")
            .forward("next").forward("next").collect().len();
        let shortest = db
            .query("SELECT b._key FROM MATCH SHORTEST (a)-[r*]->(b) \
                    WHERE a._key = 'p/n0' AND b._key = 'p/n5'")
            .map(|s| s.collect().len()).unwrap_or(0);
        (one_hop, five_hop, shortest,
         db.edge_schema().len(),
         db.edge_types_from("p/n0").len(),
         db.edges_from_collection("p").len())
    };

    let resident = measure(&CoreDB::open_with_config(dir.path(), Config::resident()).unwrap());
    let paged = measure(&CoreDB::open_paged(dir.path()).unwrap());

    assert_eq!(resident, paged, "paged mode answers differently from resident");
    assert_eq!(resident, (1, 1, 1, 1, 1, 5), "the baseline itself is wrong: {resident:?}");

    // SHOW must agree too — it walks the same structures.
    let r = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap().show("SHOW EDGES").unwrap().len();
    let p = CoreDB::open_paged(dir.path()).unwrap().show("SHOW EDGES").unwrap().len();
    assert_eq!(r, p, "SHOW EDGES differs between modes");
    assert!(r > 0, "SHOW EDGES found nothing even in resident mode");
}

/// Compaction must not destroy the graph.
///
/// Adjacency lives in two places the resident maps do not cover: the memory-mapped
/// topology base, and the spilled CSR files that reads are served from while the
/// RAM maps sit empty. Compaction rebuilt the topology from the RAM maps alone, so
/// it wrote a graph with no edges and destroyed every one of them on disk —
/// permanently, in both modes, and `dump_sql` lost them too. The node guard rail
/// passed the whole time, because every node was still there.
#[test]
fn paged_compaction_preserves_base_edges() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER)").unwrap();
        for i in 0..10 {
            db.execute(&format!("INSERT INTO p (_key, n) VALUES ('n{i}', {i})")).unwrap();
        }
        for i in 0..9 {
            db.link(&format!("p/n{i}"), &format!("p/n{}", i + 1), "next");
        }
        db.compact().unwrap();
    }

    let edges = |db: &CoreDB| db
        .query("SELECT b._key FROM MATCH (a:p)-[:next]->(b:p)")
        .map(|s| s.collect().len()).unwrap_or(0);
    let dumped = |db: &CoreDB| db.dump_sql().lines()
        .filter(|l| l.starts_with("INSERT")).count();

    let mut db = CoreDB::open_paged(dir.path()).unwrap();
    assert_eq!(edges(&db), 9, "baseline");
    assert_eq!(dumped(&db), 19, "baseline dump carries nodes and edges");

    // Write, then compact — twice, because a service does this continuously.
    for round in 0..2 {
        let lo = 10 + round * 5;
        for i in lo..lo + 5 {
            db.execute(&format!("INSERT INTO p (_key, n) VALUES ('n{i}', {i})")).unwrap();
        }
        db.compact().unwrap();
        assert_eq!(edges(&db), 9, "edges lost at compaction round {round}");
    }
    assert_eq!(dumped(&db), 29, "dump_sql lost edges");
    db.compact().unwrap();
    drop(db);

    // And they must still be there for a fresh handle, in either mode.
    assert_eq!(edges(&CoreDB::open_paged(dir.path()).unwrap()), 9, "lost across a paged reopen");
    assert_eq!(edges(&CoreDB::open_with_config(dir.path(), Config::resident()).unwrap()), 9, "lost across a resident reopen");
}

/// Writing a key again after deleting it must bring the row back.
///
/// A delete against the immutable base records a tombstone, and every base-aware
/// lookup honours it. Nothing retired that tombstone when the key was written
/// again, so the new row was logged and stored but invisible — `get` returned
/// nothing, `contains` said false, the count was short — and then the next
/// compaction cleared the tombstone and the row silently reappeared.
#[test]
fn rewriting_a_deleted_base_key_brings_it_back() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, v TEXT)").unwrap();
        for i in 0..5 {
            db.execute(&format!("INSERT INTO p (_key, v) VALUES ('n{i}', 'original')")).unwrap();
        }
        db.compact().unwrap();      // all five live in the base
    }

    let mut db = CoreDB::open_paged(dir.path()).unwrap();
    db.remove("p/n2");
    assert!(db.get("p/n2").is_none(), "the delete did not take effect");
    assert_eq!(db.node_count(), 4);

    db.execute("INSERT INTO p (_key, v) VALUES ('n2', 'rewritten')").unwrap();
    let back = db.get("p/n2").expect("the rewritten row is invisible");
    assert!(back.contains("rewritten"), "stale content: {back}");
    assert!(db.contains("p/n2"), "contains() still says the row is absent");
    assert_eq!(db.node_count(), 5, "node_count did not see the rewrite");
    assert_eq!(db.query("SELECT _key FROM p").unwrap().collect().len(), 5);

    // And it must survive, rather than depend on a later compaction to appear.
    db.compact().unwrap();
    drop(db);
    let db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
    assert!(db.get("p/n2").unwrap().contains("rewritten"), "lost across a reopen");
    assert_eq!(db.node_count(), 5);
}
