//! Catalog admission must precede source mutation, including WAL-tail cleanup.
//! Run database tests on the authorized Linux test paths, never on Mac by default.
use e4_prototype::{
    collections::{CollectionId, Database, EntityId, Error, IndexId, IndexState, ScalarPredicate},
    pagewal::PageWalStore,
    Kind,
};
use kernel::{
    io::IoMode,
    limits::ResourceLimits,
    store::{Config, SyncMode},
};
use serde_json::json;
use std::{collections::BTreeMap, fs, io::Write, path::Path};

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}
fn fixture(path: &Path) -> (CollectionId, IndexId, EntityId) {
    let mut db = Database::create(path, cfg()).unwrap();
    let c = db
        .create_collection(
            "people",
            vec![("name".into(), Kind::Text)],
            Default::default(),
        )
        .unwrap();
    let entity = db.put(c, "one", &json!({"name":"Alice"})).unwrap();
    let index = db.create_scalar_index(c, "by_name", "name", false).unwrap();
    assert_eq!(index.0, 1);
    assert!(db.build_index_step(index, 8).unwrap());
    db.commit().unwrap();
    (c, index, entity)
}
fn files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn visit(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in fs::read_dir(dir).unwrap() {
            let path = e.unwrap().path();
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if path.is_dir() {
                out.insert(format!("{name}/"), vec![]);
                visit(root, &path, out);
            } else {
                out.insert(name, fs::read(path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}
fn reseal(packet: &mut [u8]) {
    let n = packet.len();
    assert_eq!(n, 2081);
    let crc = crc32c::crc32c(&packet[..n - 4]).to_le_bytes();
    packet[n - 4..].copy_from_slice(&crc);
}
fn tail_without_coordination(path: &Path) {
    fs::OpenOptions::new()
        .append(true)
        .open(path.join("wal"))
        .unwrap()
        .write_all(b"incomplete-uncommitted-tail")
        .unwrap();
    // Only after all handles are dropped. A refused open must not reconstruct
    // missing publication hints/reader slots, or normalize the retained tail.
    for e in fs::read_dir(path).unwrap() {
        let e = e.unwrap();
        if e.file_name().to_string_lossy().starts_with("reader") {
            fs::remove_file(e.path()).unwrap();
        }
    }
}
fn assert_refused_unchanged(path: &Path, unsupported: bool) {
    let before = files(path);
    for snapshot in [false, true] {
        let result = if snapshot {
            Database::open_snapshot(path, cfg())
        } else {
            Database::open(path, cfg())
        };
        match result {
            Err(Error::Unsupported(_)) => {}
            Err(Error::Corrupt(_)) if !unsupported => {}
            Err(error) => panic!("wrong refusal, snapshot={snapshot}: {error:?}"),
            Ok(_) => panic!("invalid index catalog admitted, snapshot={snapshot}"),
        }
        assert_eq!(
            files(path),
            before,
            "refusal changed bytes/inventory, snapshot={snapshot}"
        );
    }
}

#[test]
fn one_intact_future_index_encoding_replica_refuses_before_mutation() {
    let temp = tempfile::tempdir().unwrap();
    for copy in 0..3u8 {
        let path = temp.path().join(format!("future-index-{copy}"));
        fixture(&path);
        let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
        let key = [3, copy, 0x81, 1];
        let mut descriptor = raw.get(&key).unwrap().unwrap();
        assert_eq!(&descriptor[..8], b"E4IDX01\0");
        // Packet header is ten bytes; encoding version follows id/u32/family.
        descriptor[23..25].copy_from_slice(&2u16.to_be_bytes());
        reseal(&mut descriptor);
        raw.put(&key, &descriptor).unwrap();
        raw.commit().unwrap();
        let before = files(&path);
        assert!(matches!(
            Database::open_snapshot(&path, cfg()),
            Err(Error::Unsupported(_))
        ));
        assert_eq!(
            files(&path),
            before,
            "live snapshot modified unknown descriptor source"
        );
        drop(raw);
        tail_without_coordination(&path);
        assert_refused_unchanged(&path, true);
    }
}

#[test]
fn one_intact_unknown_logical_feature_replica_refuses_before_mutation() {
    let temp = tempfile::tempdir().unwrap();
    for copy in 0..3u8 {
        let path = temp.path().join(format!("future-feature-{copy}"));
        fixture(&path);
        let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
        let key = [0, 0, copy];
        let mut header = raw.get(&key).unwrap().unwrap();
        assert_eq!(&header[..8], b"E4COLL2\0");
        header[18..26].copy_from_slice(&(1u64 | (1 << 63)).to_be_bytes());
        reseal(&mut header);
        raw.put(&key, &header).unwrap();
        raw.commit().unwrap();
        drop(raw);
        tail_without_coordination(&path);
        assert_refused_unchanged(&path, true);
    }
}

#[test]
fn missing_registry_or_intact_orphan_descriptor_is_not_silently_ignored() {
    let temp = tempfile::tempdir().unwrap();
    for missing_registry in [false, true] {
        let path = temp.path().join(format!("orphan-{missing_registry}"));
        fixture(&path);
        let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
        if missing_registry {
            assert!(raw.delete(&[4, 0x81, 1]).unwrap());
        } else {
            let mut descriptor = raw.get(&[3, 0, 0x81, 1]).unwrap().unwrap();
            // A checksum-valid descriptor for a new ID outside the registry.
            descriptor[10..18].copy_from_slice(&2u64.to_be_bytes());
            reseal(&mut descriptor);
            raw.put(&[3, 0, 0x81, 2], &descriptor).unwrap();
        }
        raw.commit().unwrap();
        drop(raw);
        tail_without_coordination(&path);
        assert_refused_unchanged(&path, false);
    }
}

#[test]
fn damaged_future_descriptor_replica_falls_back_to_intact_siblings() {
    let temp = tempfile::tempdir().unwrap();
    for copy in 0..3u8 {
        let path = temp.path().join(format!("damaged-{copy}"));
        let (_, index, entity) = fixture(&path);
        let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
        let key = [3, copy, 0x81, 1];
        let mut descriptor = raw.get(&key).unwrap().unwrap();
        descriptor[23..25].copy_from_slice(&2u16.to_be_bytes());
        // Deliberately stale packet checksum: this is damage, not authoritative
        // evidence that the file requires an unsupported encoding.
        raw.put(&key, &descriptor).unwrap();
        raw.commit().unwrap();
        drop(raw);
        let before = files(&path);
        let snapshot = Database::open_snapshot(&path, cfg()).unwrap();
        assert_eq!(
            snapshot
                .query_scalar(index, ScalarPredicate::Eq(json!("Alice")), 8)
                .unwrap(),
            vec![entity]
        );
        drop(snapshot);
        assert_eq!(files(&path), before, "read-only fallback changed source");
        let mut db = Database::open(&path, cfg()).unwrap();
        assert_eq!(db.index_info(index).unwrap().state, IndexState::Ready);
        db.update(entity.collection, "one", &json!({"name":"Beatrice"}))
            .unwrap();
        db.commit().unwrap();
        drop(db);
        let db = Database::open_snapshot(&path, cfg()).unwrap();
        assert_eq!(
            db.query_scalar(index, ScalarPredicate::Eq(json!("Alice")), 8)
                .unwrap(),
            vec![]
        );
        assert_eq!(
            db.query_scalar(index, ScalarPredicate::Eq(json!("Beatrice")), 8)
                .unwrap(),
            vec![entity]
        );
    }
}

#[test]
fn limited_policy_survives_index_opt_in_rollback_and_last_drop() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("limited");
    let limits = ResourceLimits {
        data_bytes: 4 << 20,
        wal_bytes: 2 << 20,
        tracked_pages: 256,
        readers: 1,
        record_bytes: 4096,
        recovery_bytes: 1 << 20,
    };
    let mut db = Database::create_limited(&path, cfg(), limits).unwrap();
    let c = db
        .create_collection(
            "people",
            vec![("name".into(), Kind::Text)],
            Default::default(),
        )
        .unwrap();
    db.put(c, "one", &json!({"name":"Alice"})).unwrap();
    db.commit().unwrap();
    let abandoned = db.create_scalar_index(c, "name", "name", false).unwrap();
    db.rollback().unwrap();
    assert_eq!(db.limits(), Some(limits));
    assert!(matches!(db.index_info(abandoned), Err(Error::NotFound(_))));
    let index = db.create_scalar_index(c, "name", "name", false).unwrap();
    assert!(db.build_index_step(index, 8).unwrap());
    db.commit().unwrap();
    assert!(db.checkpoint().unwrap());
    drop(db);
    let snapshot = Database::open_snapshot(&path, cfg()).unwrap();
    assert_eq!(snapshot.limits(), Some(limits));
    assert!(
        Database::open_snapshot(&path, cfg()).is_err(),
        "persisted one-reader limit lost"
    );
    drop(snapshot);
    let mut db = Database::open(&path, cfg()).unwrap();
    assert_eq!(db.limits(), Some(limits));
    assert!(db
        .put(c, "large", &json!({"name":"x".repeat(8192)}))
        .is_err());
    db.rollback().unwrap();
    db.begin_drop_index(index).unwrap();
    assert!(db.drop_index_step(index, 8).unwrap());
    db.commit().unwrap();
    drop(db);
    let db = Database::open(&path, cfg()).unwrap();
    assert_eq!(db.limits(), Some(limits));
    assert!(db.list_indexes(c).unwrap().is_empty());
}

#[test]
fn building_unique_index_cannot_become_ready_with_an_unseen_duplicate() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("unique-build");
    let mut db = Database::create(&path, cfg()).unwrap();
    let c = db
        .create_collection(
            "people",
            vec![("name".into(), Kind::Text)],
            Default::default(),
        )
        .unwrap();
    db.put(c, "a", &json!({"name":"A"})).unwrap();
    let b = db.put(c, "b", &json!({"name":"B"})).unwrap();
    db.commit().unwrap();
    let index = db.create_scalar_index(c, "name", "name", true).unwrap();
    assert!(!db.build_index_step(index, 1).unwrap());
    db.commit().unwrap();
    // B has not been visited by the build, so BUILDING does not yet promise
    // table-wide uniqueness. The live write is indexed but cannot make READY.
    db.put(c, "duplicate", &json!({"name":"B"})).unwrap();
    db.commit().unwrap();
    assert!(matches!(
        db.build_index_step(index, 8),
        Err(Error::AlreadyExists)
    ));
    assert!(
        db.commit().is_err(),
        "failed build step allowed partial commit"
    );
    db.rollback().unwrap();
    assert!(matches!(
        db.index_info(index).unwrap().state,
        IndexState::Building { .. }
    ));
    assert!(db
        .query_scalar(index, ScalarPredicate::Eq(json!("B")), 8)
        .is_err());
    db.delete(c, "duplicate").unwrap();
    db.commit().unwrap();
    assert!(db.build_index_step(index, 8).unwrap());
    db.commit().unwrap();
    drop(db);
    let db = Database::open_snapshot(&path, cfg()).unwrap();
    assert_eq!(
        db.query_scalar(index, ScalarPredicate::Eq(json!("B")), 8)
            .unwrap(),
        vec![b]
    );
}
