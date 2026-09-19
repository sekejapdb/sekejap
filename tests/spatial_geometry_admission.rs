//! Item X1a, DO(1): admission for the new `SpatialGeometry` index family's
//! feature bit (`0x100`). Mirrors `tests/spatial_admission.rs`'s style for
//! the point family (`SPATIAL_FEATURE`, `0x08`) and
//! `collections::supported_logical_feature_mask_is_the_only_definition`'s
//! frozen-mask check, but scoped to the geometry family: a fresh database
//! never carries the bit until the first geometry index is created, clearing
//! the bit after one exists is refused, and a bare posting under the
//! geometry tag without the bit is refused. Database execution belongs on
//! the authorized Linux test paths.

use e4_prototype::{
    collections::{CollectionOptions, Database, EntityId, Error, IndexId},
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

/// One posting key under the geometry tag (`0x7c`), independent of this
/// crate's private `spatial_geometry_indexes` module: level, then cell (a
/// world-bucket geometry always posts at level 0, cell 0), then sequence.
fn posting_key(index: IndexId, entity: EntityId, level: u8, cell: u32) -> Vec<u8> {
    let mut key = vec![0x7c];
    key.extend(ordered(index.0));
    key.push(level);
    key.extend(cell.to_be_bytes());
    key.extend(ordered(entity.sequence));
    key
}

fn reseal(packet: &mut [u8]) {
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
                out.insert(format!("{name}/"), Vec::new());
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

fn assert_unsupported_unchanged(path: &Path) {
    let before = files(path);
    for snapshot in [false, true] {
        let result = if snapshot {
            Database::open_snapshot(path, cfg())
        } else {
            Database::open(path, cfg())
        };
        match result {
            Err(Error::Unsupported(_)) | Err(Error::Corrupt(_)) => {}
            Err(error) => panic!("wrong geometry-admission refusal, snapshot={snapshot}: {error:?}"),
            Ok(_) => panic!("bad geometry descriptor/postings admitted, snapshot={snapshot}"),
        }
        assert_eq!(files(path), before, "refusal changed geometry source");
    }
}

#[test]
fn a_fresh_database_never_carries_the_geometry_bit_until_a_geometry_index_is_created() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("no-geometry-yet");
    let mut db = Database::create(&path, cfg()).unwrap();
    let collection = db
        .create_collection(
            "shapes",
            vec![("shape".into(), Kind::Geo)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.put(
        collection,
        "one",
        &json!({"shape": {"type": "Point", "coordinates": [1.0, 2.0]}}),
    )
    .unwrap();
    db.commit().unwrap();
    drop(db);
    // No index of any family exists yet: the index header itself may not
    // even have been created. Either way, bit 0x100 is not set.
    let raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    if let Some(header) = raw.get(&[0u8, 0, 0]).unwrap() {
        let features = u64::from_be_bytes(header[10 + 8..10 + 16].try_into().unwrap());
        assert_eq!(features & 0x100, 0, "geometry bit set before any geometry index exists");
    }
    drop(raw);

    let mut db = Database::open(&path, cfg()).unwrap();
    let index = db
        .create_geometry_index(collection, "by_shape", "shape")
        .unwrap();
    assert!(db.build_index_step(index, 8).unwrap());
    db.commit().unwrap();
    drop(db);

    let raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    let header = raw.get(&[0u8, 0, 0]).unwrap().unwrap();
    let features = u64::from_be_bytes(header[10 + 8..10 + 16].try_into().unwrap());
    assert_ne!(features & 0x100, 0, "geometry bit not set after creating a geometry index");
}

#[test]
fn clearing_the_geometry_feature_cannot_hide_descriptor_or_postings() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("hidden-geometry-family");
    let mut db = Database::create(&path, cfg()).unwrap();
    let collection = db
        .create_collection(
            "shapes",
            vec![("shape".into(), Kind::Geo)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.put(
        collection,
        "one",
        &json!({"shape": {"type": "Point", "coordinates": [1.0, 2.0]}}),
    )
    .unwrap();
    let index = db
        .create_geometry_index(collection, "by_shape", "shape")
        .unwrap();
    assert!(db.build_index_step(index, 8).unwrap());
    db.commit().unwrap();
    drop(db);

    // Clear every bit but the always-set bit 0: the geometry descriptor and
    // its postings both still exist on disk, unadmitted.
    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    for copy in 0..3u8 {
        let key = [0u8, 0, copy];
        let mut header = raw.get(&key).unwrap().unwrap();
        assert_eq!(&header[..8], b"E4COLL2\0");
        header[10 + 8..10 + 16].copy_from_slice(&1u64.to_be_bytes());
        reseal(&mut header);
        raw.put(&key, &header).unwrap();
    }
    raw.commit().unwrap();
    drop(raw);
    tail_without_coordination(&path);
    assert_unsupported_unchanged(&path);
}

#[test]
fn posting_under_the_geometry_tag_without_the_feature_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("posting-without-geometry-feature");
    let mut db = Database::create(&path, cfg()).unwrap();
    let collection = db
        .create_collection("plain", vec![], CollectionOptions::default())
        .unwrap();
    let entity = db.put(collection, "one", &json!({})).unwrap();
    db.commit().unwrap();
    drop(db);

    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    let key = posting_key(IndexId(1), entity, 0, 0);
    raw.put(&key, &[0u8; 16]).unwrap();
    raw.commit().unwrap();
    drop(raw);
    tail_without_coordination(&path);
    assert_unsupported_unchanged(&path);
}
