//! Text format admission and exact-access corruption boundaries.
//! Database execution belongs on the authorized Linux test paths.
use e4_prototype::{
    collections::{Database, EntityId, Error, IndexId, TextCandidates, TextMatch},
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

fn index_prefix(tag: u8, index: IndexId) -> Vec<u8> {
    let mut key = vec![tag];
    key.extend(ordered(index.0));
    key
}

fn posting_key(index: IndexId, entity: EntityId, term: &str) -> Vec<u8> {
    let mut key = index_prefix(0x75, index);
    key.extend(term.as_bytes());
    key.push(0);
    key.extend(ordered(entity.sequence));
    key
}

fn norm_key(index: IndexId, entity: EntityId) -> Vec<u8> {
    let mut key = index_prefix(0x76, index);
    key.extend(ordered(entity.sequence));
    key
}

fn term_stats_key(index: IndexId, term: &str) -> Vec<u8> {
    let mut key = index_prefix(0x77, index);
    key.extend(term.as_bytes());
    key.push(0);
    key
}

fn fixture(path: &Path) -> (IndexId, EntityId) {
    let mut db = Database::create(path, cfg()).unwrap();
    let collection = db
        .create_collection(
            "docs",
            vec![("body".into(), Kind::Text)],
            Default::default(),
        )
        .unwrap();
    let entity = db.put(collection, "one", &json!({"body":"term"})).unwrap();
    let index = db.create_text_index(collection, "body", "body").unwrap();
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
            Err(error) => panic!("wrong text refusal, snapshot={snapshot}: {error:?}"),
            Ok(_) => panic!("future text descriptor admitted, snapshot={snapshot}"),
        }
        assert_eq!(files(path), before, "refusal changed text source");
    }
}

fn query(db: &Database, index: IndexId) -> e4_prototype::collections::Result<Vec<EntityId>> {
    db.query_text(
        index,
        "term",
        TextMatch::Any,
        8,
        TextCandidates::All,
        8,
        || false,
    )
    .map(|hits| hits.into_iter().map(|hit| hit.id).collect())
}

#[test]
fn intact_future_text_family_versions_or_options_refuse_without_mutation() {
    let temp = tempfile::tempdir().unwrap();
    for (damage, copy) in (0..3u8).flat_map(|copy| (0..6u8).map(move |damage| (damage, copy))) {
        let path = temp.path().join(format!("future-text-{damage}-{copy}"));
        let (index, _) = fixture(&path);
        assert_eq!(index.0, 1);
        let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
        let key = [3, copy, 0x81, 1];
        let mut descriptor = raw.get(&key).unwrap().unwrap();
        assert_eq!(&descriptor[..8], b"E4IDX01\0");
        match damage {
            0 => descriptor[10 + 12] = 0x7f,
            1 => descriptor[10 + 13..10 + 15].copy_from_slice(&2u16.to_be_bytes()),
            2 => descriptor[10 + 15..10 + 17].copy_from_slice(&2u16.to_be_bytes()),
            3 => descriptor[10 + 17] ^= 1,
            4 => descriptor[10 + 20..10 + 22].copy_from_slice(&2u16.to_be_bytes()),
            5 => descriptor[10 + 22] = 1,
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
fn clearing_text_feature_cannot_hide_descriptor_or_any_text_keyspace() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("hidden-text-family");
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
fn every_text_prefix_without_explicit_feature_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    for tag in [0x75, 0x76, 0x77, 0x78] {
        let path = temp.path().join(format!("text-prefix-{tag:x}"));
        let mut db = Database::create(&path, cfg()).unwrap();
        let collection = db
            .create_collection("plain", vec![], Default::default())
            .unwrap();
        db.put(collection, "one", &json!({})).unwrap();
        db.commit().unwrap();
        drop(db);

        let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
        let key = index_prefix(tag, IndexId(1));
        raw.put(&key, &[]).unwrap();
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
}

#[test]
fn malformed_posting_norm_term_and_corpus_stats_are_corrupt_at_exact_access() {
    let temp = tempfile::tempdir().unwrap();
    for damage in 0..11u8 {
        let path = temp.path().join(format!("text-damage-{damage}"));
        let (index, entity) = fixture(&path);
        let posting = posting_key(index, entity, "term");
        let norm = norm_key(index, entity);
        let term_stats = term_stats_key(index, "term");
        let corpus = index_prefix(0x78, index);
        let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
        match damage {
            0 => raw.put(&posting, &[0; 3]).unwrap(),
            1 => raw.put(&posting, &0u32.to_be_bytes()).unwrap(),
            2 => assert!(raw.delete(&posting).unwrap()),
            3 => raw.put(&norm, &[0; 3]).unwrap(),
            4 => assert!(raw.delete(&norm).unwrap()),
            5 => raw.put(&term_stats, &[0; 7]).unwrap(),
            6 => raw.put(&term_stats, &0u64.to_be_bytes()).unwrap(),
            7 => raw.put(&term_stats, &2u64.to_be_bytes()).unwrap(),
            8 => raw.put(&corpus, &[0; 15]).unwrap(),
            9 => raw.put(&corpus, &[0; 16]).unwrap(),
            10 => {
                let mut value = 1u64.to_be_bytes().to_vec();
                value.extend(0u64.to_be_bytes());
                raw.put(&corpus, &value).unwrap();
            }
            _ => unreachable!(),
        }
        raw.commit().unwrap();
        drop(raw);

        // Admission remains catalog-bounded. Exact access validates the
        // posting, norm and statistics it consumes together.
        let db = Database::open_snapshot(&path, cfg()).unwrap();
        assert!(matches!(query(&db, index), Err(Error::Corrupt(_))));
    }
}
