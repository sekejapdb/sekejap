//! Exact-vector format admission and read-time corruption boundaries.
//! Database execution belongs on the authorized Linux test paths.
use sekejap_core::{
    collections::{Database, EntityId, Error, IndexId, VectorCandidates, VectorMetric},
    pagewal::PageWalStore,
    Kind,
};
use kernel::{
    io::IoMode,
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

fn ordered(n: u64) -> Vec<u8> {
    let bytes = n.to_be_bytes();
    let start = bytes.iter().position(|byte| *byte != 0).unwrap_or(7);
    let mut out = vec![0x80 + (8 - start) as u8];
    out.extend_from_slice(&bytes[start..]);
    out
}

fn locator_key(index: IndexId, entity: EntityId) -> Vec<u8> {
    let mut key = vec![0x73];
    key.extend(ordered(index.0));
    key.extend(ordered(entity.sequence));
    key
}

fn sidecar_key(entity: EntityId, ordinal: usize) -> Vec<u8> {
    let mut key = vec![0x60];
    key.extend(ordered(entity.collection.0.into()));
    key.extend(ordered(entity.sequence));
    key.extend(ordered(ordinal as u64));
    key
}

fn fixture(path: &Path) -> (IndexId, EntityId) {
    let mut db = Database::create(path, cfg()).unwrap();
    let collection = db
        .create_collection(
            "vectors",
            vec![("embedding".into(), Kind::Vector(2))],
            Default::default(),
        )
        .unwrap();
    let entity = db
        .put(collection, "one", &json!({"embedding":[1.0,2.0]}))
        .unwrap();
    let index = db
        .create_exact_vector_index(collection, "embedding", "embedding")
        .unwrap();
    assert!(db.build_index_step(index, 8).unwrap());
    db.commit().unwrap();
    (index, entity)
}

fn files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn visit(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
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
    assert_eq!(packet.len(), 2081);
    let end = packet.len() - 4;
    let checksum = crc32c::crc32c(&packet[..end]).to_le_bytes();
    packet[end..].copy_from_slice(&checksum);
}

fn tail_without_coordination(path: &Path) {
    fs::OpenOptions::new()
        .append(true)
        .open(path.join("wal"))
        .unwrap()
        .write_all(b"incomplete-uncommitted-tail")
        .unwrap();
    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name().to_string_lossy().starts_with("reader") {
            fs::remove_file(entry.path()).unwrap();
        }
    }
}

fn assert_unsupported_unchanged(path: &Path) {
    let before = files(path);
    for snapshot in [false, true] {
        let result = if snapshot {
            Database::open_snapshot(path, cfg())
        } else {
            Database::open(path, cfg())
        };
        match result {
            Err(Error::Unsupported(_)) => {}
            Err(error) => panic!("wrong vector refusal, snapshot={snapshot}: {error:?}"),
            Ok(_) => panic!("future vector descriptor admitted, snapshot={snapshot}"),
        }
        assert_eq!(files(path), before, "refusal changed vector source");
    }
}

#[test]
fn one_intact_unknown_vector_family_or_version_refuses_before_mutation() {
    let temp = tempfile::tempdir().unwrap();
    for (future_family, copy) in (0..3u8).flat_map(|copy| [(false, copy), (true, copy)]) {
        let path = temp
            .path()
            .join(format!("future-vector-{future_family}-{copy}"));
        let (index, _) = fixture(&path);
        assert_eq!(index.0, 1);
        let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
        let key = [3, copy, 0x81, 1];
        let mut descriptor = raw.get(&key).unwrap().unwrap();
        assert_eq!(&descriptor[..8], b"E4IDX01\0");
        if future_family {
            descriptor[10 + 12] = 0x7f;
        } else {
            descriptor[10 + 13..10 + 15].copy_from_slice(&2u16.to_be_bytes());
        }
        reseal(&mut descriptor);
        raw.put(&key, &descriptor).unwrap();
        raw.commit().unwrap();
        drop(raw);
        tail_without_coordination(&path);
        assert_unsupported_unchanged(&path);
    }
}

#[test]
fn clearing_vector_feature_cannot_hide_vector_descriptor_or_locators() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("hidden-vector-family");
    fixture(&path);
    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    for copy in 0..3u8 {
        let key = [0, 0, copy];
        let mut header = raw.get(&key).unwrap().unwrap();
        assert_eq!(&header[..8], b"E4COLL2\0");
        header[10 + 8..10 + 16].copy_from_slice(&1u64.to_be_bytes());
        reseal(&mut header);
        raw.put(&key, &header).unwrap();
    }
    raw.commit().unwrap();
    drop(raw);
    tail_without_coordination(&path);
    let before = files(&path);
    for snapshot in [false, true] {
        let result = if snapshot {
            Database::open_snapshot(&path, cfg())
        } else {
            Database::open(&path, cfg())
        };
        assert!(matches!(result, Err(Error::Corrupt(_))));
        assert_eq!(files(&path), before);
    }
}

#[test]
fn locator_prefix_without_explicit_vector_feature_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("locator-without-feature");
    let mut db = Database::create(&path, cfg()).unwrap();
    let collection = db
        .create_collection("plain", vec![], Default::default())
        .unwrap();
    let entity = db.put(collection, "one", &json!({})).unwrap();
    db.commit().unwrap();
    drop(db);

    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    let mut key = vec![0x73];
    key.extend(ordered(1));
    key.extend(ordered(entity.sequence));
    raw.put(&key, &[0, 0, 0, 1, 0, 1]).unwrap();
    raw.commit().unwrap();
    drop(raw);
    tail_without_coordination(&path);
    let before = files(&path);
    for snapshot in [false, true] {
        let result = if snapshot {
            Database::open_snapshot(&path, cfg())
        } else {
            Database::open(&path, cfg())
        };
        assert!(matches!(result, Err(Error::Corrupt(_))));
        assert_eq!(files(&path), before);
    }
}

#[test]
fn malformed_locator_or_authoritative_sidecar_is_reported_at_exact_query() {
    let temp = tempfile::tempdir().unwrap();
    for damage in 0..6u8 {
        let path = temp.path().join(format!("vector-damage-{damage}"));
        let (index, entity) = fixture(&path);
        let locator = locator_key(index, entity);
        let sidecar = sidecar_key(entity, 1); // hidden external-key slot is ordinal zero
        let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
        match damage {
            0 => raw.put(&locator, &[0; 5]).unwrap(),
            1 => raw.put(&locator, &[0; 6]).unwrap(),
            2 => {
                let mut value = raw.get(&locator).unwrap().unwrap();
                value[4..].copy_from_slice(&0u16.to_be_bytes());
                raw.put(&locator, &value).unwrap();
            }
            3 => assert!(raw.delete(&sidecar).unwrap()),
            4 => raw.put(&sidecar, &[0; 7]).unwrap(),
            5 => raw
                .put(
                    &sidecar,
                    &[f32::NAN.to_le_bytes(), 0.0f32.to_le_bytes()].concat(),
                )
                .unwrap(),
            _ => unreachable!(),
        }
        raw.commit().unwrap();
        drop(raw);

        // Admission is intentionally catalog-bounded. Exact access validates
        // the locator, referenced layout and authoritative sidecar together.
        let db = Database::open_snapshot(&path, cfg()).unwrap();
        let selected = [entity];
        for candidates in [
            VectorCandidates::All,
            VectorCandidates::SortedUnique(&selected),
        ] {
            assert!(matches!(
                db.query_exact_vector(
                    index,
                    &[1.0, 0.0],
                    VectorMetric::SquaredL2,
                    1,
                    candidates,
                    1,
                    || false,
                ),
                Err(Error::Corrupt(_))
            ));
        }
    }
}
