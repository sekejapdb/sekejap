use e4_prototype::{collections::{CollectionOptions, Database, Error}, Kind};
use kernel::{io::IoMode, limits::ResourceLimits, store::{Config, SyncMode}};
use serde_json::json;

fn wal_bytes_limit(err: &Error) -> bool {
    matches!(err, Error::Kernel(kernel::Error::ResourceLimit("page-WAL wal_bytes allowance")))
}

// SQLite walCheckpoint clamps to the min reader mark; walRestartLog cannot
// restart the log while a reader lags — the WAL grows under a pinned snapshot.
// Page-WAL matches that: a View pins frames by WAL byte offset, so checkpoint
// cannot truncate while a reader lives and wal_bytes is the logical WAL length.
// A hard cap refuses the write with ResourceLimit; Law 6 keeps the snapshot;
// after the reader drops, checkpoint folds and writes resume. Do not re-tighten.
#[test]
fn pinned_reader_grows_wal_until_cap_then_refuses_cleanly_and_recovers() {
    let tmp = std::env::temp_dir();
    assert!(tmp.starts_with("<scratch>")
        || tmp.starts_with("<scratch>")
        || tmp.starts_with("<scratch>"));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let config = Config { budget_bytes: 1<<20, io: IoMode::Buffered, sync: SyncMode::Full };
    let limits = ResourceLimits { data_bytes: 14<<20, wal_bytes: 1<<20, tracked_pages: 1024,
        readers: 4, record_bytes: 16384, recovery_bytes: 256<<10 };
    let mut db = Database::create_limited(&path, config, limits).unwrap();
    let c = db.create_collection("embeddings", vec![("v".into(), Kind::Vector(1536)), ("revision".into(), Kind::Int)], CollectionOptions::default()).unwrap();
    let mut doc = json!({"v":vec![0.25f32;1536],"revision":0});
    for i in 0..1024 {
        db.put(c, &format!("p{i:04}"), &doc).unwrap();
        if i%64==63 {db.commit().unwrap();}
    }
    let size = || -> u64 {
        std::fs::read_dir(&path).unwrap().map(|p| {
            let m=p.unwrap().metadata().unwrap(); if m.is_file() {m.len()} else {0}
        }).sum()
    };
    let loaded = size();
    // (e) the configured envelope is tighter than 2× loaded, before any reader.
    assert!(limits.total_bytes().unwrap() < loaded*2);
    let old = Database::open_snapshot(&path, config).unwrap();
    let mut first_refusal: Option<(usize, usize)> = None;
    let mut p0000_committed_revision = 0i64;
    'pinned: for cycle in 1..=12 {
        doc["revision"] = json!(cycle);
        for i in 0..1024 {
            match db.put(c, &format!("p{i:04}"), &doc) {
                Ok(_) => {}
                Err(ref e) if wal_bytes_limit(e) => {
                    first_refusal = Some((cycle, i / 64 + 1));
                    break 'pinned;
                }
                Err(e) => panic!("pinned-reader put: expected success or ResourceLimit(\"page-WAL wal_bytes allowance\"), got {e:?}"),
            }
            if i % 64 == 63 {
                match db.commit() {
                    Ok(()) => {
                        if i == 63 { p0000_committed_revision = cycle as i64; }
                    }
                    Err(ref e) if wal_bytes_limit(e) => {
                        first_refusal = Some((cycle, i / 64 + 1));
                        break 'pinned;
                    }
                    Err(e) => panic!("pinned-reader commit: expected success or ResourceLimit(\"page-WAL wal_bytes allowance\"), got {e:?}"),
                }
            }
        }
        assert_eq!(old.get(c,"p0000").unwrap().unwrap().document["revision"], 0);
    }
    // (a) the cap is actually reached (not vacuous) and the recorded cycle/commit is > 0.
    let (refused_cycle, refused_commit) = first_refusal.expect("pinned reader must reach wal_bytes; otherwise the cap is not being exercised");
    assert!(refused_cycle > 0, "refusal cycle must be after load, got {refused_cycle}");
    assert!(refused_commit > 0, "refusal commit index must be > 0, got {refused_commit}");
    eprintln!("wal_bytes refusal at cycle {refused_cycle} commit {refused_commit}; last committed p0000 revision {p0000_committed_revision}");

    // (b) Law 6: the pinned snapshot is byte-stable through the refusal.
    assert_eq!(old.get(c,"p0000").unwrap().unwrap().document["revision"], 0);
    assert_eq!(old.scan(c,None).unwrap().count(), 1024);

    // (c) A refusal after mutation poisons this handle (Database::finish / page-WAL
    // poisoned). RESOURCE_LOOP_1.md: failed writers must reopen before further
    // entry reads or writes; the unfinished transaction is discarded on reopen.
    // Database::rollback re-inspects durable files in place, exactly as reopen
    // would, and is the collection-layer recovery without dropping the lock.
    assert!(matches!(db.get(c, "p0000"), Err(Error::Failed)), "poisoned writer must not expose a partial working tree");
    assert!(matches!(db.put(c, "p0000", &doc), Err(Error::Failed)));
    assert!(matches!(db.commit(), Err(Error::Failed)));
    db.rollback().expect("rollback recovers the last committed prefix in place");
    assert_eq!(db.get(c,"p0000").unwrap().unwrap().document["revision"], p0000_committed_revision);
    assert_eq!(db.scan(c,None).unwrap().count(), 1024);

    // (d) Drop the reader. Writes must resume on their own (SQLite folds on the
    // next commit once no reader lags). Do not call checkpoint: a writer at the
    // cap recovers when no reader is pinned. Remaining cycles complete under the cap.
    drop(old);
    for cycle in refused_cycle..=12 {
        doc["revision"] = json!(cycle);
        for i in 0..1024 {
            db.put(c, &format!("p{i:04}"), &doc).unwrap();
            if i%64==63 {db.commit().unwrap(); assert!(size() <= limits.total_bytes().unwrap());}
        }
    }
    drop(db);
    let db = Database::open(&path, config).unwrap();
    for row in db.scan(c,None).unwrap() {
        let row=row.unwrap(); assert_eq!(row.document,doc);
    }
    assert_eq!(db.scan(c,None).unwrap().count(),1024);
}
