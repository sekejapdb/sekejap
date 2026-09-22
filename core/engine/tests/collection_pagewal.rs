//! Typed collections on the V2 page-WAL path: what the public `Database`
//! contract guarantees on that backend, and what it refuses.
use sekejap_core::{
    collections::{Clock, CollectionOptions, Database, Error},
    pagewal::PageWalStore,
    Kind,
};
use kernel::{
    io::IoMode,
    limits::ResourceLimits,
    page::{seal, PageKind, PageMut, PageRef, PAGE_SIZE},
    store::{Config, Store, SyncMode},
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}
fn fields() -> Vec<(String, Kind)> {
    vec![
        ("name".into(), Kind::Text),
        ("age".into(), Kind::Int),
        ("score".into(), Kind::Real),
        ("active".into(), Kind::Bool),
        ("profile".into(), Kind::Json),
        ("position".into(), Kind::Point),
        ("embedding".into(), Kind::Vector(3)),
    ]
}
fn doc(n: i64) -> Value {
    json!({
        "name": format!("person {n}"),
        "age": 20 + n,
        "score": 0.5 * n as f64,
        "active": n % 2 == 0,
        "profile": {"roles": ["operator", n], "sensor": null, "nested": {"depth": [1, 2.5, "x"]}},
        "position": {"type": "Point", "coordinates": [144.0 + n as f64 * 0.001, -37.5]},
        "embedding": [0.5, n as f64, -2.0],
        "observed_at": 1_788_888_800 + n,
        "extra": {"unsigned": u64::MAX, "tags": ["a", "b"]}
    })
}
const COORDINATION: [&str; 9] = [
    "readers.lock", "reader-0.lock", "reader-1.lock", "reader-2.lock", "reader-3.lock",
    "reader-4.lock", "reader-5.lock", "reader-6.lock", "reader-7.lock",
];
fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.file_type().unwrap().is_file())
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                fs::read(e.path()).unwrap(),
            )
        })
        .collect()
}
fn core_files(dir: &Path) -> [Vec<u8>; 3] {
    ["data", "wal", "writer.lock"].map(|n| fs::read(dir.join(n)).unwrap())
}
fn remove_coordination(dir: &Path) {
    for n in COORDINATION {
        let _ = fs::remove_file(dir.join(n));
    }
}
fn wait_for(marker: &Path, what: &str) {
    let started = Instant::now();
    while !marker.exists() {
        assert!(started.elapsed() < Duration::from_secs(60), "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}
struct TestClock(AtomicI64);
impl Clock for TestClock {
    fn unix_seconds(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[test]
fn mixed_records_roundtrip_through_page_wal_commit_and_reopen() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let people = db
        .create_collection("people", fields(), CollectionOptions::default())
        .unwrap();
    let mut ids = Vec::new();
    for n in 0..50 {
        ids.push(db.put(people, &format!("person/{n}"), &doc(n)).unwrap());
        assert_eq!(db.get(people, &format!("person/{n}")).unwrap().unwrap().document, doc(n));
    }
    db.commit().unwrap();
    // The selected storage path is visible on disk: page-WAL frames and
    // checkpoint metadata, a 96-byte publication hint, no inherited-Store
    // files, no JSON text anywhere.
    let core = core_files(&path);
    assert!(core[0][..PAGE_SIZE].windows(8).any(|w| w == b"E4PWAL02"));
    assert!(!core[1].is_empty(), "commit publishes through the page WAL");
    assert_eq!(fs::metadata(path.join("readers.lock")).unwrap().len(), 96);
    assert!(!path.join("free").exists());
    for bytes in &core {
        assert!(!bytes.windows(7).any(|w| w == b"\"name\":"));
    }
    drop(db);
    let db = Database::open(&path, cfg()).unwrap();
    assert_eq!(db.collection("people").unwrap(), Some(people));
    for (n, id) in ids.iter().enumerate() {
        let row = db.get_by_id(*id).unwrap().unwrap();
        assert_eq!(row.key, format!("person/{n}"));
        assert_eq!(row.document, doc(n as i64));
        assert!(row.document.get("_created_unix").is_none());
    }
    let scanned: Vec<_> = db
        .scan(people, None)
        .unwrap()
        .map(|r| r.unwrap().id)
        .collect();
    assert_eq!(scanned, ids);
    let info = db.collection_info(people).unwrap();
    assert_eq!(info.layout.fields, fields());
    assert!(!info.timestamps);
}

#[test]
fn timestamps_default_off_and_explicit_on_persist_across_reopen() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("db");
    let clock = Arc::new(TestClock(AtomicI64::new(1_000)));
    let mut db = Database::create(&path, cfg()).unwrap();
    db.set_clock(clock.clone());
    let off = db
        .create_collection("off", fields(), Default::default())
        .unwrap();
    let on = db
        .create_collection("on", fields(), CollectionOptions { timestamps: true })
        .unwrap();
    db.put(off, "x", &doc(1)).unwrap();
    db.put(on, "x", &doc(1)).unwrap();
    assert!(db.get(off, "x").unwrap().unwrap().document.get("_created_unix").is_none());
    let row = db.get(on, "x").unwrap().unwrap().document;
    assert_eq!((row["_created_unix"].as_i64(), row["_updated_unix"].as_i64()), (Some(1_000), Some(1_000)));
    assert_eq!(row["observed_at"], doc(1)["observed_at"]);
    assert!(db.put(on, "y", &json!({"_updated_unix": 5})).is_err());
    db.put(off, "y", &json!({"_updated_unix": 5})).unwrap();
    db.commit().unwrap();
    drop(db);
    let mut db = Database::open(&path, cfg()).unwrap();
    db.set_clock(clock.clone());
    assert!(!db.collection_info(off).unwrap().timestamps);
    assert!(db.collection_info(on).unwrap().timestamps);
    clock.0.store(2_000, Ordering::Relaxed);
    db.update(on, "x", &json!({"age": 99})).unwrap();
    let row = db.get(on, "x").unwrap().unwrap().document;
    assert_eq!((row["_created_unix"].as_i64(), row["_updated_unix"].as_i64()), (Some(1_000), Some(2_000)));
    assert_eq!(row["age"], 99);
    assert_eq!(db.get(off, "y").unwrap().unwrap().document, json!({"_updated_unix": 5}));
}

#[test]
fn schema_change_keeps_old_rows_decodable_and_new_layout_active() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let c = db
        .create_collection("c", fields(), Default::default())
        .unwrap();
    let old = db.put(c, "old", &doc(3)).unwrap();
    db.commit().unwrap();
    let first = db.collection_info(c).unwrap().layout.id;
    let mut changed = fields();
    changed.retain(|(n, _)| n != "age");
    changed.push(("age".into(), Kind::Text));
    changed.push(("weight".into(), Kind::Real));
    let second = db.alter_collection(c, changed.clone()).unwrap();
    assert_ne!(first, second);
    assert_eq!(db.get_by_id(old).unwrap().unwrap().document, doc(3));
    let mut newer = doc(4);
    newer["age"] = json!("thirty");
    newer["weight"] = json!(70.25);
    let new = db.put(c, "new", &newer).unwrap();
    assert!(db.put(c, "bad", &doc(5)).is_err(), "int into text slot is refused");
    db.put(c, "still-usable", &json!({"weight": 1.5})).unwrap();
    db.commit().unwrap();
    drop(db);
    let db = Database::open(&path, cfg()).unwrap();
    assert_eq!(db.collection_info(c).unwrap().layout.fields, changed);
    assert_eq!(db.get_by_id(old).unwrap().unwrap().document, doc(3));
    assert_eq!(db.get_by_id(new).unwrap().unwrap().document, newer);
    assert_eq!(db.scan(c, None).unwrap().count(), 3);
}

#[test]
fn delete_then_reinsert_allocates_a_fresh_identity_and_never_reuses_after_reopen() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let c = db
        .create_collection("c", fields(), Default::default())
        .unwrap();
    let a = db.put(c, "k", &doc(1)).unwrap();
    assert_eq!(db.put(c, "k", &doc(2)).unwrap(), a, "upsert keeps identity");
    db.commit().unwrap();
    assert!(db.delete(c, "k").unwrap());
    assert!(!db.delete(c, "k").unwrap());
    db.commit().unwrap();
    let b = db.put(c, "k", &doc(3)).unwrap();
    assert!(b.sequence > a.sequence);
    assert!(db.get_by_id(a).unwrap().is_none());
    db.commit().unwrap();
    drop(db);
    let mut db = Database::open(&path, cfg()).unwrap();
    assert_eq!(db.get(c, "k").unwrap().unwrap().id, b);
    assert!(db.delete(c, "k").unwrap());
    db.commit().unwrap();
    drop(db);
    let mut db = Database::open(&path, cfg()).unwrap();
    let e = db.put(c, "k", &doc(4)).unwrap();
    assert!(e.sequence > b.sequence, "committed sequences are never reused");
}

#[test]
fn rollback_discards_uncommitted_work_beside_a_live_snapshot() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let c = db
        .create_collection("c", fields(), Default::default())
        .unwrap();
    let a = db.put(c, "a", &doc(1)).unwrap();
    db.commit().unwrap();
    let snap = Database::open_snapshot(&path, cfg()).unwrap();
    db.put(c, "a", &doc(2)).unwrap();
    let b = db.put(c, "b", &doc(3)).unwrap();
    db.alter_collection(c, vec![]).unwrap();
    db.rollback().unwrap();
    assert_eq!(db.get_by_id(a).unwrap().unwrap().document, doc(1));
    assert!(db.get_by_id(b).unwrap().is_none());
    assert_eq!(db.collection_info(c).unwrap().layout.fields, fields());
    assert_eq!(snap.get_by_id(a).unwrap().unwrap().document, doc(1));
    // A validation error never fails the handle; a storage failure does,
    // and rollback restores the durable state without a reopen.
    assert!(matches!(db.put(c, "x", &json!([1])), Err(Error::InvalidInput(_))));
    let again = db.put(c, "b", &doc(4)).unwrap();
    assert_eq!(again, b, "provisional sequences are reusable after rollback");
    db.commit().unwrap();
    assert_eq!(snap.scan(c, None).unwrap().count(), 1);
    assert_eq!(db.scan(c, None).unwrap().count(), 2);
    drop(snap);
    drop(db);
    let db = Database::open(&path, cfg()).unwrap();
    assert_eq!(db.get_by_id(b).unwrap().unwrap().document, doc(4));
}

#[test]
fn snapshots_serve_published_state_defer_checkpoint_and_are_bounded() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let c = db
        .create_collection("c", fields(), Default::default())
        .unwrap();
    let a = db.put(c, "a", &doc(1)).unwrap();
    db.commit().unwrap();
    db.put(c, "pending", &doc(9)).unwrap();
    let mut early = Database::open_snapshot(&path, cfg()).unwrap();
    assert!(early.get(c, "pending").unwrap().is_none(), "unpublished work is invisible");
    assert!(matches!(early.rollback(), Err(Error::ReadOnly)));
    db.commit().unwrap();
    let later = Database::open_snapshot(&path, cfg()).unwrap();
    assert!(later.get(c, "pending").unwrap().is_some());
    db.put(c, "a", &doc(2)).unwrap();
    db.commit().unwrap();
    assert_eq!(early.get_by_id(a).unwrap().unwrap().document, doc(1));
    assert_eq!(later.get_by_id(a).unwrap().unwrap().document, doc(1));
    assert!(!db.checkpoint().unwrap(), "readers defer the checkpoint, never block on it");
    // A writer can close and reopen beside live readers; they stay stable.
    drop(db);
    let mut db = Database::open(&path, cfg()).unwrap();
    assert_eq!(db.get_by_id(a).unwrap().unwrap().document, doc(2));
    assert_eq!(early.get_by_id(a).unwrap().unwrap().document, doc(1));
    let mut views = vec![early, later];
    while views.len() < 8 {
        views.push(Database::open_snapshot(&path, cfg()).unwrap());
    }
    assert!(Database::open_snapshot(&path, cfg()).is_err(), "eight reader slots");
    views.pop();
    views.push(Database::open_snapshot(&path, cfg()).unwrap());
    drop(views);
    assert!(db.checkpoint().unwrap());
    assert_eq!(fs::metadata(path.join("wal")).unwrap().len(), 0);
    assert_eq!(
        Database::open_snapshot(&path, cfg())
            .unwrap()
            .get_by_id(a)
            .unwrap()
            .unwrap()
            .document,
        doc(2),
        "absorbed state is served from the data file"
    );
}

#[test]
fn reopen_recovers_committed_wal_before_any_checkpoint() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let c = db
        .create_collection("c", fields(), Default::default())
        .unwrap();
    let ids: Vec<_> = (0..20).map(|n| db.put(c, &format!("k{n}"), &doc(n)).unwrap()).collect();
    db.commit().unwrap();
    let (data, wal) = db.storage_bytes().unwrap();
    assert!(wal > 0 && data > 0);
    assert!(db.tracked_pages().unwrap().unwrap() > 0);
    drop(db);
    assert!(fs::metadata(path.join("wal")).unwrap().len() > 0);
    let mut db = Database::open(&path, cfg()).unwrap();
    for (n, id) in ids.iter().enumerate() {
        assert_eq!(db.get_by_id(*id).unwrap().unwrap().document, doc(n as i64));
    }
    assert!(db.checkpoint().unwrap());
    assert_eq!(fs::metadata(path.join("wal")).unwrap().len(), 0);
    assert_eq!(db.tracked_pages().unwrap(), Some(0));
    assert!(matches!(db.checkpoint(), Ok(true)), "an empty WAL checkpoints trivially");
    db.put(c, "k0", &doc(100)).unwrap();
    assert!(
        matches!(db.checkpoint(), Err(Error::InvalidInput(_))),
        "checkpoint needs a committed handle; asking early is a validation error"
    );
    db.commit().unwrap();
    assert!(db.checkpoint().unwrap());
    drop(db);
    let db = Database::open(&path, cfg()).unwrap();
    assert_eq!(db.get_by_id(ids[0]).unwrap().unwrap().document, doc(100));
    assert_eq!(fs::metadata(path.join("wal")).unwrap().len(), 0);
}

fn rewrite_pagewal_header(p: &Path, no: usize, f: impl FnOnce(&mut Vec<u8>)) {
    let mut data = fs::read(p.join("data")).unwrap();
    let b = &mut data[no * PAGE_SIZE..(no + 1) * PAGE_SIZE];
    let mut h = PageRef::open(b, no as u32).unwrap().slot(0).to_vec();
    f(&mut h);
    let mut page = PageMut::init(b, PageKind::Meta, 0, no as u32);
    page.insert_slot(0, &h).unwrap();
    page.finalise(0);
    seal(b, 1);
    PageRef::open(b, no as u32).unwrap();
    fs::write(p.join("data"), data).unwrap();
}
fn seeded(path: &Path) -> Database {
    let mut db = Database::create(path, cfg()).unwrap();
    let c = db
        .create_collection("c", fields(), Default::default())
        .unwrap();
    db.put(c, "a", &doc(1)).unwrap();
    db.commit().unwrap();
    db
}

#[test]
fn unsupported_sources_are_refused_before_any_byte_changes() {
    let d = tempfile::tempdir().unwrap();
    // 1. An inherited kernel-Store database is not a V2 collection database.
    let legacy = d.path().join("legacy");
    let mut s = Store::create(&legacy, cfg()).unwrap();
    s.put(b"k", b"v").unwrap();
    s.checkpoint().unwrap();
    drop(s);
    let before = files(&legacy);
    assert!(matches!(Database::open(&legacy, cfg()), Err(Error::Unsupported(_))));
    assert!(Database::open_snapshot(&legacy, cfg()).is_err());
    assert_eq!(files(&legacy), before);
    // 2. A raw page-WAL store without a collection catalog: refused by writer
    //    and reader, and a reader with no writer alive creates nothing.
    let raw = d.path().join("raw");
    let mut s = PageWalStore::open(&raw, true, 1 << 20).unwrap();
    s.put(b"k", b"v").unwrap();
    s.commit().unwrap();
    drop(s);
    remove_coordination(&raw);
    let before = files(&raw);
    assert!(matches!(Database::open(&raw, cfg()), Err(Error::Corrupt(_))));
    assert!(matches!(Database::open_snapshot(&raw, cfg()), Err(Error::Corrupt(_))));
    assert_eq!(files(&raw), before, "no coordination file before the typed check passes");
    // 3. A storage feature this binary does not implement, with an
    //    uncommitted WAL tail that a normal open would truncate.
    let feature = d.path().join("feature");
    let mut db = seeded(&feature);
    assert!(db.checkpoint().unwrap());
    drop(db);
    for no in 0..2 {
        rewrite_pagewal_header(&feature, no, |h| h[55] |= 0x80);
    }
    fs::OpenOptions::new().append(true).open(feature.join("wal")).unwrap().write_all(b"future-tail").unwrap();
    remove_coordination(&feature);
    let before = files(&feature);
    assert!(Database::open(&feature, cfg()).is_err());
    assert!(Database::open_snapshot(&feature, cfg()).is_err());
    assert_eq!(files(&feature), before);
    // 4. A newer typed-collection catalog header, all three copies intact,
    //    beside an uncommitted tail: refused before tail normalization, and
    //    the reader creates no coordination file either.
    let catalog = d.path().join("catalog");
    drop(seeded(&catalog));
    let mut s = PageWalStore::open(&catalog, false, 1 << 20).unwrap();
    for i in 0..3 {
        let mut h = s.get(&[0, 0, i]).unwrap().unwrap();
        assert_eq!(h.len(), 2081);
        // `E4COLL1` is the PLAIN envelope and `E4COLL2` the INDEX one; every
        // database with a collection in it carries the second, because the
        // live row-count record rides `create_collection`. The version byte
        // is what this case makes newer, whichever envelope it is in.
        assert!(&h[..6] == b"E4COLL" && h[7] == 0, "{:?}", &h[..8]);
        h[6] = b'9';
        let n = h.len();
        let crc = crc32c::crc32c(&h[..n - 4]).to_le_bytes();
        h[n - 4..].copy_from_slice(&crc);
        s.put(&[0, 0, i], &h).unwrap();
    }
    s.commit().unwrap();
    drop(s);
    fs::OpenOptions::new().append(true).open(catalog.join("wal")).unwrap().write_all(b"uncommitted-tail").unwrap();
    remove_coordination(&catalog);
    let before = files(&catalog);
    assert!(matches!(Database::open(&catalog, cfg()), Err(Error::Unsupported(_))));
    assert!(matches!(Database::open_snapshot(&catalog, cfg()), Err(Error::Unsupported(_))));
    assert_eq!(files(&catalog), before, "refusal precedes tail normalization and file creation");
    // 5. Configurations this path cannot honour are refused up front.
    // A WEAKER BARRIER IS NOT ONE OF THEM. All three `SyncMode`s are
    // honoured at every publication point of the page-WAL, and `Normal` is
    // the default of the published crate; which primitive ran is readable
    // from `IoCounters::sync_full_calls` / `sync_data_calls`
    // (`core/engine/tests/sync_modes.rs`). What is still refused is what the
    // path has no way to do: unbuffered I/O, and a pool the B-tree cannot
    // descend and split with.
    let good = d.path().join("good");
    drop(seeded(&good));
    let before = core_files(&good);
    for sync in [SyncMode::Full, SyncMode::Normal, SyncMode::Off] {
        let ok = Config { sync, ..cfg() };
        Database::open(&good, ok).unwrap();
        Database::open_snapshot(&good, ok).unwrap();
    }
    assert_eq!(core_files(&good), before, "opening under each mode changes no file");
    for bad in [
        Config { io: IoMode::Direct, ..cfg() },
        Config { budget_bytes: 4096, ..cfg() },
    ] {
        assert!(matches!(Database::open(&good, bad), Err(Error::Unsupported(_))));
        assert!(matches!(Database::open_snapshot(&good, bad), Err(Error::Unsupported(_))));
        let fresh = d.path().join("never-created");
        assert!(matches!(Database::create(&fresh, bad), Err(Error::Unsupported(_))));
        assert!(!fresh.exists());
    }
    assert_eq!(core_files(&good), before);
    assert!(Database::open(d.path().join("missing"), cfg()).is_err());
    Database::open(&good, cfg()).unwrap();
}

#[test]
fn resource_policy_is_persisted_as_e4limit1_enforced_per_field_and_refused_when_unsupported() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("db");
    let limits = ResourceLimits {
        data_bytes: 512 << 10,
        wal_bytes: 256 << 10,
        tracked_pages: 40,
        readers: 2,
        record_bytes: 16 << 10,
        recovery_bytes: 8 << 10,
    };
    for (why, bad) in [
        ("wal above 16 MiB", ResourceLimits { wal_bytes: 32 << 20, data_bytes: 64 << 20, ..limits }),
        ("readers above eight slots", ResourceLimits { readers: 9, ..limits }),
        ("kernel-invalid", ResourceLimits { readers: 0, ..limits }),
    ] {
        assert!(Database::create_limited(&path, cfg(), bad).is_err(), "{why}");
        assert!(!path.exists(), "{why}: refused before creation");
    }
    let mut db = Database::create_limited(&path, cfg(), limits).unwrap();
    assert_eq!(db.limits(), Some(limits));
    let c = db
        .create_collection("c", fields(), Default::default())
        .unwrap();
    let id = db.put(c, "kept", &doc(1)).unwrap();
    db.commit().unwrap();
    // record_bytes is checked before mutation: the handle stays usable.
    let big = json!({"blob": "x".repeat(20_000)});
    assert!(matches!(db.put(c, "big", &big), Err(Error::InvalidInput(_))));
    db.put(c, "small", &doc(2)).unwrap();
    db.commit().unwrap();
    // readers: enforced by slot index, for readers in any process.
    let s1 = Database::open_snapshot(&path, cfg()).unwrap();
    let s2 = Database::open_snapshot(&path, cfg()).unwrap();
    assert!(Database::open_snapshot(&path, cfg()).is_err(), "persisted readers bound");
    drop(s2);
    // tracked_pages: distinct pages in the WAL index since the last
    // checkpoint; the readers keep the checkpoint deferred, so the index only
    // grows until the policy refuses, before the offending frame is written.
    assert!(db.tracked_pages().unwrap().unwrap() <= 40);
    let mut refused = None;
    for i in 0..1000 {
        db.put(c, &format!("n{i}"), &json!({"v": "x".repeat(8000)})).unwrap();
        if let Err(e) = db.commit() {
            refused = Some(e);
            break;
        }
        assert!(db.tracked_pages().unwrap().unwrap() <= 40);
    }
    assert!(matches!(refused, Some(Error::Kernel(kernel::Error::ResourceLimit(_)))), "{refused:?}");
    assert_eq!(s1.get_by_id(id).unwrap().unwrap().document, doc(1));
    db.rollback().unwrap();
    assert!(db.tracked_pages().unwrap().unwrap() <= 40);
    let (data, wal) = db.storage_bytes().unwrap();
    assert!(data <= limits.data_bytes && wal <= limits.wal_bytes);
    drop(s1);
    assert!(db.checkpoint().unwrap());
    assert_eq!(db.tracked_pages().unwrap(), Some(0));
    drop(db);
    let db = Database::open(&path, cfg()).unwrap();
    assert_eq!(db.limits(), Some(limits), "policy survives reopen");
    assert_eq!(db.get_by_id(id).unwrap().unwrap().document, doc(1));
    let total = fs::metadata(path.join("data")).unwrap().len() + fs::metadata(path.join("wal")).unwrap().len();
    assert!(total <= limits.data_bytes + limits.wal_bytes);
    assert_eq!(fs::metadata(path.join("readers.lock")).unwrap().len(), 96, "coordination logical bytes");
}

/// Diagnostic for the parent: the page-WAL logs page images, so one
/// `create_collection` costs as many frames as it dirties pages. The number
/// is measured here and printed; a policy that cannot hold it refuses that
/// transaction explicitly (handle failed, rollback restores) and one that can
/// succeeds. Whether the original 64 KiB policy of `tests/collections.rs`
/// holds it is reported, not assumed.
#[test]
fn metadata_transaction_cost_is_measured_and_a_policy_below_it_refuses_explicitly() {
    let d = tempfile::tempdir().unwrap();
    let mut db = Database::create(d.path().join("measure"), cfg()).unwrap();
    let (_, before) = db.storage_bytes().unwrap();
    db.create_collection("c", vec![], Default::default()).unwrap();
    db.commit().unwrap();
    let (_, after) = db.storage_bytes().unwrap();
    let cost = after - before;
    let pages = db.tracked_pages().unwrap().unwrap();
    println!("CREATE_COLLECTION_WAL_BYTES={cost}; WAL_INDEX_PAGES_AFTER_CREATE={pages}; FITS_64KIB_POLICY={}", cost <= 64 << 10);
    // Creation itself (empty tree, cap, catalog header) publishes about
    // 46 KiB before the first collection; the diagnostic needs the measured
    // cost above that to place a policy strictly between the two.
    assert!(cost > 50 << 10, "measured create_collection cost {cost} is below this diagnostic's floor");
    drop(db);
    let policy = |wal_bytes: u64| ResourceLimits {
        data_bytes: 1 << 20,
        wal_bytes,
        tracked_pages: 4096,
        readers: 4,
        record_bytes: 1024,
        recovery_bytes: 0,
    };
    let too_small = cost / 4096 * 4096;
    let path = d.path().join("too-small");
    let mut db = Database::create_limited(&path, cfg(), policy(too_small)).unwrap();
    let r = db.create_collection("c", vec![], Default::default()).and_then(|_| db.commit());
    assert!(matches!(r, Err(Error::Kernel(kernel::Error::ResourceLimit(_)))), "{r:?}");
    db.rollback().unwrap();
    assert!(db.collection("c").unwrap().is_none());
    // Room for the creation transactions as well, whether or not the
    // half-allowance auto-checkpoint folded them first.
    let enough = (cost + (48 << 10)).div_ceil(4096) * 4096;
    let path = d.path().join("enough");
    let mut db = Database::create_limited(&path, cfg(), policy(enough)).unwrap();
    db.create_collection("c", vec![], Default::default()).unwrap();
    db.commit().unwrap();
    assert!(db.collection("c").unwrap().is_some());
}

// Cross-process reader: spawned as a child of this test binary. It opens a
// snapshot by path, proves the committed state, holds its slot until told
// to go, and proves the state never moved while the writer kept committing.
#[test]
fn reader_child() {
    let Ok(p) = std::env::var("E4_COLL_READER_PATH") else { return };
    let signal = PathBuf::from(std::env::var("E4_COLL_READER_SIGNAL").unwrap());
    let snap = Database::open_snapshot(Path::new(&p), cfg()).unwrap();
    let c = snap.collection("c").unwrap().unwrap();
    let check = |snap: &Database| {
        let rows: Vec<_> = snap.scan(c, None).unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(rows.len(), 3);
        for (n, row) in rows.iter().enumerate() {
            assert_eq!(row.document, doc(n as i64));
        }
    };
    check(&snap);
    fs::write(signal.join("ready"), b"").unwrap();
    wait_for(&signal.join("go"), "parent never released the reader");
    check(&snap);
    assert!(snap.get(c, "late").unwrap().is_none());
    fs::write(signal.join("child-ok"), b"").unwrap();
}

#[test]
fn reader_in_another_process_excludes_checkpoint_and_stays_stable() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("db");
    let signal = d.path().join("signal");
    fs::create_dir(&signal).unwrap();
    let mut db = Database::create(&path, cfg()).unwrap();
    let c = db
        .create_collection("c", fields(), Default::default())
        .unwrap();
    for n in 0..3 {
        db.put(c, &format!("k{n}"), &doc(n)).unwrap();
    }
    db.commit().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "reader_child", "--nocapture"])
        .env("E4_COLL_READER_PATH", &path)
        .env("E4_COLL_READER_SIGNAL", &signal)
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !signal.join("ready").exists() {
        assert!(started.elapsed() < Duration::from_secs(60), "child reader never became ready");
        if let Some(status) = child.try_wait().unwrap() {
            panic!("child reader exited early: {status}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // The writer keeps working beside the foreign reader; only the fold waits.
    db.put(c, "k0", &doc(10)).unwrap();
    db.put(c, "late", &doc(11)).unwrap();
    db.commit().unwrap();
    assert!(!db.checkpoint().unwrap(), "a reader in another process defers the checkpoint");
    assert!(fs::metadata(path.join("wal")).unwrap().len() > 0);
    fs::write(signal.join("go"), b"").unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "child reader failed: {status}");
    assert!(signal.join("child-ok").exists());
    assert!(db.checkpoint().unwrap(), "released slot lets the checkpoint fold");
    assert_eq!(fs::metadata(path.join("wal")).unwrap().len(), 0);
    assert_eq!(db.get(c, "k0").unwrap().unwrap().document, doc(10));
}

// Cross-process writer: opens the database while the parent holds a path
// reader, commits, finds its checkpoint deferred, and exits without one.
#[test]
fn writer_child() {
    let Ok(p) = std::env::var("E4_COLL_WRITER_PATH") else { return };
    let signal = PathBuf::from(std::env::var("E4_COLL_WRITER_SIGNAL").unwrap());
    let mut db = Database::open(Path::new(&p), cfg()).unwrap();
    let c = db.collection("c").unwrap().unwrap();
    db.put(c, "k0", &doc(10)).unwrap();
    db.put(c, "late", &doc(11)).unwrap();
    db.commit().unwrap();
    assert!(!db.checkpoint().unwrap(), "the parent's reader defers this writer's checkpoint");
    fs::write(signal.join("writer-ok"), b"").unwrap();
}

#[test]
fn writer_restart_in_another_process_beside_a_pinned_reader() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("db");
    let signal = d.path().join("signal");
    fs::create_dir(&signal).unwrap();
    let mut db = Database::create(&path, cfg()).unwrap();
    let c = db
        .create_collection("c", fields(), Default::default())
        .unwrap();
    for n in 0..3 {
        db.put(c, &format!("k{n}"), &doc(n)).unwrap();
    }
    db.commit().unwrap();
    // A quiescent reader: no writer alive, state derived from the files.
    drop(db);
    let reader = Database::open_snapshot(&path, cfg()).unwrap();
    assert_eq!(reader.scan(c, None).unwrap().count(), 3);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "writer_child", "--nocapture"])
        .env("E4_COLL_WRITER_PATH", &path)
        .env("E4_COLL_WRITER_SIGNAL", &signal)
        .status()
        .unwrap();
    assert!(status.success(), "child writer failed: {status}");
    assert!(signal.join("writer-ok").exists());
    assert_eq!(reader.get(c, "k0").unwrap().unwrap().document, doc(0), "pinned across a foreign writer's life");
    assert!(reader.get(c, "late").unwrap().is_none());
    // A second writer incarnation in this process: hint rewritten, tail kept,
    // checkpoint still deferred by the reader, then folded.
    let mut db = Database::open(&path, cfg()).unwrap();
    assert_eq!(db.get(c, "late").unwrap().unwrap().document, doc(11));
    assert!(!db.checkpoint().unwrap());
    let fresh = Database::open_snapshot(&path, cfg()).unwrap();
    assert_eq!(fresh.get(c, "k0").unwrap().unwrap().document, doc(10));
    assert_eq!(reader.get(c, "k0").unwrap().unwrap().document, doc(0));
    drop((reader, fresh));
    assert!(db.checkpoint().unwrap());
    assert_eq!(fs::metadata(path.join("wal")).unwrap().len(), 0);
}

#[test]
fn coordination_files_are_advisory_and_recreated_by_a_writer_or_a_validated_quiescent_reader() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("db");
    drop(seeded(&path));
    remove_coordination(&path);
    let snap = Database::open_snapshot(&path, cfg()).unwrap();
    assert_eq!(fs::metadata(path.join("reader-0.lock")).unwrap().len(), 0);
    assert_eq!(fs::metadata(path.join("readers.lock")).unwrap().len(), 0, "readers never write a hint");
    let mut db = Database::open(&path, cfg()).unwrap();
    assert_eq!(fs::metadata(path.join("readers.lock")).unwrap().len(), 96, "the writer publishes a hint at open");
    assert!(!db.checkpoint().unwrap());
    drop(snap);
    assert!(db.checkpoint().unwrap());
}
