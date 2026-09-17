//! Spatial descriptor admission and posting corruption boundaries.
//! Database execution belongs on the authorized Linux test paths.

use e4_prototype::{
    collections::{Database, EntityId, Error, IndexId, SpatialCandidates},
    pagewal::PageWalStore,
    spatial_math::{point_hilbert, Point},
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

fn posting_key(index: IndexId, entity: EntityId, point: Point) -> Vec<u8> {
    let mut key = vec![0x74];
    key.extend(ordered(index.0));
    key.extend((point_hilbert(point) as u32).to_be_bytes());
    key.extend(ordered(entity.sequence));
    key
}

fn fixture(path: &Path) -> (IndexId, EntityId, Point) {
    let point = Point::new(144.5, -37.5).unwrap();
    let mut db = Database::create(path, cfg()).unwrap();
    let collection = db
        .create_collection(
            "points",
            vec![("position".into(), Kind::Point)],
            Default::default(),
        )
        .unwrap();
    let entity = db
        .put(
            collection,
            "one",
            &json!({"position":{"type":"Point","coordinates":[144.5,-37.5]}}),
        )
        .unwrap();
    let index = db
        .create_point_index(collection, "position", "position")
        .unwrap();
    assert!(db.build_index_step(index, 8).unwrap());
    db.commit().unwrap();
    (index, entity, point)
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
            Err(error) => panic!("wrong spatial refusal, snapshot={snapshot}: {error:?}"),
            Ok(_) => panic!("future spatial descriptor admitted, snapshot={snapshot}"),
        }
        assert_eq!(files(path), before, "refusal changed spatial source");
    }
}

#[test]
fn intact_future_spatial_family_version_or_options_refuse_without_mutation() {
    let temp = tempfile::tempdir().unwrap();
    for (damage, copy) in (0..3u8).flat_map(|copy| [(0u8, copy), (1, copy), (2, copy)]) {
        let path = temp.path().join(format!("future-spatial-{damage}-{copy}"));
        let (index, _, _) = fixture(&path);
        assert_eq!(index.0, 1);
        let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
        let key = [3, copy, 0x81, 1];
        let mut descriptor = raw.get(&key).unwrap().unwrap();
        assert_eq!(&descriptor[..8], b"E4IDX01\0");
        match damage {
            0 => descriptor[10 + 12] = 0x7f,
            1 => descriptor[10 + 13..10 + 15].copy_from_slice(&2u16.to_be_bytes()),
            2 => descriptor[10 + 18] = 1,
            _ => unreachable!(),
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
fn clearing_spatial_feature_cannot_hide_descriptor_or_postings() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("hidden-spatial-family");
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
fn posting_prefix_without_spatial_feature_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("posting-without-feature");
    let mut db = Database::create(&path, cfg()).unwrap();
    let collection = db
        .create_collection("plain", vec![], Default::default())
        .unwrap();
    let entity = db.put(collection, "one", &json!({})).unwrap();
    db.commit().unwrap();
    drop(db);

    let point = Point::new(0.0, 0.0).unwrap();
    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    let key = posting_key(IndexId(1), entity, point);
    let mut value = Vec::new();
    value.extend(point.longitude().to_le_bytes());
    value.extend(point.latitude().to_le_bytes());
    raw.put(&key, &value).unwrap();
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
fn malformed_coordinates_and_hilbert_are_corrupt_at_exact_access() {
    let temp = tempfile::tempdir().unwrap();
    for damage in 0..5u8 {
        let path = temp.path().join(format!("spatial-damage-{damage}"));
        let (index, entity, point) = fixture(&path);
        let original_key = posting_key(index, entity, point);
        let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
        let original_value = raw.get(&original_key).unwrap().unwrap();
        match damage {
            0 => raw.put(&original_key, &[0; 15]).unwrap(),
            1 => {
                let mut value = original_value.clone();
                value[..8].copy_from_slice(&f64::NAN.to_le_bytes());
                raw.put(&original_key, &value).unwrap();
            }
            2 => {
                let mut value = original_value.clone();
                value[..8].copy_from_slice(&181.0f64.to_le_bytes());
                raw.put(&original_key, &value).unwrap();
            }
            3 => {
                let mut value = original_value.clone();
                value[..8].copy_from_slice(&0.0f64.to_le_bytes());
                value[8..].copy_from_slice(&0.0f64.to_le_bytes());
                raw.put(&original_key, &value).unwrap();
            }
            4 => {
                assert!(raw.delete(&original_key).unwrap());
                let mut wrong_key = original_key.clone();
                let hilbert_at = 1 + ordered(index.0).len();
                wrong_key[hilbert_at + 3] ^= 1;
                raw.put(&wrong_key, &original_value).unwrap();
            }
            _ => unreachable!(),
        }
        raw.commit().unwrap();
        drop(raw);

        // Catalog admission remains bounded. A full exact scan validates every
        // posting; filtered mode additionally verifies it against primary data.
        let db = Database::open_snapshot(&path, cfg()).unwrap();
        assert!(matches!(
            db.query_point_nearest(
                index,
                Point::new(0.0, 0.0).unwrap(),
                1,
                SpatialCandidates::All,
                1,
                || false,
            ),
            Err(Error::Corrupt(_))
        ));
        let selected = [entity];
        assert!(matches!(
            db.query_point_nearest(
                index,
                Point::new(0.0, 0.0).unwrap(),
                1,
                SpatialCandidates::SortedUnique(&selected),
                1,
                || false,
            ),
            Err(Error::Corrupt(_))
        ));
    }
}

#[test]
fn missing_posting_is_detected_when_primary_candidates_are_supplied() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("missing-posting");
    let (index, entity, point) = fixture(&path);
    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    assert!(raw.delete(&posting_key(index, entity, point)).unwrap());
    raw.commit().unwrap();
    drop(raw);

    let db = Database::open_snapshot(&path, cfg()).unwrap();
    // With no reverse locator, a full posting scan cannot infer an omitted
    // primary. The explicit primary candidate path can and must reject it.
    assert!(db
        .query_point_nearest(
            index,
            Point::new(0.0, 0.0).unwrap(),
            1,
            SpatialCandidates::All,
            1,
            || false,
        )
        .unwrap()
        .is_empty());
    let selected = [entity];
    assert!(matches!(
        db.query_point_nearest(
            index,
            Point::new(0.0, 0.0).unwrap(),
            1,
            SpatialCandidates::SortedUnique(&selected),
            1,
            || false,
        ),
        Err(Error::Corrupt(_))
    ));
}
