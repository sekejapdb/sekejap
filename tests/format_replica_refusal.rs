//! Unsupported intact replicas must never be mistaken for damaged copies.
use e4_prototype::{collections::{Database, Error}, pagewal::PageWalStore, Kind};
use kernel::{io::IoMode, page::{seal, PageKind, PageMut, PageRef, PAGE_SIZE}, store::{Config, Store, SyncMode}};
use std::{collections::BTreeMap, fs, io::Write, path::Path};

fn cfg() -> Config {
    Config { budget_bytes: 1 << 20, io: IoMode::Buffered, sync: SyncMode::Full }
}

#[test]
fn store_version_admission_precedes_version_specific_payload_parsing() {
    use kernel::meta::{Meta, MAX_TREES, SUPPORTED_FORMAT_VERSION};
    // Future records may no longer have eight roots or today's extension
    // layout. Their intact version prefix must still forbid sibling fallback.
    for limited in [false, true] {
        for short in [false, true] {
            for extension in [false, true] {
                let version = (SUPPORTED_FORMAT_VERSION + 1) | if limited { 0x8000 } else { 0 };
                let mut record = version.to_le_bytes().to_vec();
                if !short { record.resize(2 + MAX_TREES * 4 + 16, 0); }
                let mut bytes = [0; PAGE_SIZE];
                let mut page = PageMut::init(&mut bytes, PageKind::Meta, 0, 0);
                page.insert_slot(0, &record).unwrap();
                if extension { page.insert_slot(1, b"future extension").unwrap(); }
                page.finalise(0);
                seal(&mut bytes, 0);
                let page = PageRef::open(&bytes, 0).unwrap();
                assert!(matches!(Meta::from_page(&page), Err(kernel::Error::Corrupt { why, .. })
                    if why == "database format version 3+ is newer than this engine reads; open it with the engine version that created it"),
                    "limited={limited}, short={short}, extension={extension}");
            }
        }
    }

    // A known version with malformed payload remains corruption, not an
    // unsupported-format refusal. No version can be inferred from one byte.
    for (record, extension, reason) in [
        (vec![1], false, "meta record too short for format_version"),
        (vec![1, 0], false, "meta record too short for format_version and roots"),
        (vec![1, 0], true, "invalid meta extension"),
    ] {
        let mut bytes = [0; PAGE_SIZE];
        let mut page = PageMut::init(&mut bytes, PageKind::Meta, 0, 0);
        page.insert_slot(0, &record).unwrap();
        if extension { page.insert_slot(1, b"invalid extension").unwrap(); }
        page.finalise(0);
        seal(&mut bytes, 0);
        let page = PageRef::open(&bytes, 0).unwrap();
        assert!(matches!(Meta::from_page(&page), Err(kernel::Error::Corrupt { why, .. }) if why == reason));
    }
}

// Include coordination files and directory entries, not only data/WAL bytes.
fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn visit(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            if path.is_dir() {
                out.insert(format!("{name}/"), vec![]);
                visit(root, &path, out);
            } else { out.insert(name, fs::read(path).unwrap()); }
        }
    }
    let mut out = BTreeMap::new();
    visit(dir, dir, &mut out);
    out
}

#[test]
fn one_future_typed_replica_refuses_before_any_open_mutation() {
    let tmp = tempfile::tempdir().unwrap();
    for copy in 0..3 {
        let path = tmp.path().join(format!("typed-{copy}"));
        let mut db = Database::create(&path, cfg()).unwrap();
        let c = db.create_collection("c", vec![("n".into(), Kind::Int)], Default::default()).unwrap();
        db.put(c, "k", &serde_json::json!({"n": 7})).unwrap();
        db.commit().unwrap();
        drop(db);
        let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
        let key = [0, 0, copy];
        let mut header = raw.get(&key).unwrap().unwrap();
        header[6] = b'9';
        let n = header.len();
        let crc = crc32c::crc32c(&header[..n - 4]).to_le_bytes();
        header[n - 4..].copy_from_slice(&crc);
        raw.put(&key, &header).unwrap();
        raw.commit().unwrap();
        let before = files(&path);
        assert!(matches!(Database::open_snapshot(&path, cfg()), Err(Error::Unsupported(_))));
        assert_eq!(files(&path), before, "live reader refusal must not mutate files");
        drop(raw);
        fs::OpenOptions::new().append(true).open(path.join("wal")).unwrap().write_all(b"uncommitted-tail").unwrap();
        // A quiescent reader must also refuse before reconstructing hints/slots.
        for entry in fs::read_dir(&path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy().starts_with("reader") {
                fs::remove_file(entry.path()).unwrap();
            }
        }
        let before = files(&path);
        assert!(matches!(Database::open(&path, cfg()), Err(Error::Unsupported(_))));
        assert_eq!(files(&path), before, "writer must preserve tail and file set");
        assert!(matches!(Database::open_snapshot(&path, cfg()), Err(Error::Unsupported(_))));
        assert_eq!(files(&path), before, "quiescent reader must preserve file set");
    }
}

#[test]
fn damaged_future_typed_replica_still_falls_back() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("damaged");
    drop(Database::create(&path, cfg()).unwrap());
    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    let mut header = raw.get(&[0, 0, 0]).unwrap().unwrap();
    header[6] = b'9'; // Deliberately do not reseal the metadata packet.
    raw.put(&[0, 0, 0], &header).unwrap();
    raw.commit().unwrap();
    drop(raw);
    drop(Database::open_snapshot(&path, cfg()).unwrap());
    drop(Database::open(&path, cfg()).unwrap());
}

#[test]
fn one_future_store_superblock_refuses_before_any_open_mutation() {
    let tmp = tempfile::tempdir().unwrap();
    for physical in [false, true] {
    for copy in 0..2 {
        let path = tmp.path().join(format!("store-{physical}-{copy}"));
        let mut db = Store::create(&path, cfg()).unwrap();
        db.put(b"k", b"v").unwrap();
        db.commit().unwrap();
        db.checkpoint().unwrap();
        drop(db);
        let mut data = fs::read(path.join("data")).unwrap();
        let bytes = &mut data[copy * PAGE_SIZE..(copy + 1) * PAGE_SIZE];
        let mut record = PageRef::open(bytes, copy as u32).unwrap().slot(0).to_vec();
        if !physical { record[..2].copy_from_slice(&3u16.to_le_bytes()); }
        let mut page = PageMut::init(bytes, PageKind::Meta, 0, copy as u32);
        page.insert_slot(0, &record).unwrap();
        page.finalise(0);
        if physical { bytes[4..6].copy_from_slice(&2u16.to_le_bytes()); }
        seal(bytes, 0);
        fs::write(path.join("data"), &data).unwrap();
        fs::OpenOptions::new().append(true).open(path.join("wal")).unwrap().write_all(b"tail").unwrap();
        let before = files(&path);
        let refusal = if physical { "unknown format version" } else {
            "database format version 3+ is newer than this engine reads; open it with the engine version that created it"
        };
        assert!(matches!(Store::open(&path, cfg()), Err(kernel::Error::Corrupt { why, .. }) if why == refusal));
        assert_eq!(files(&path), before);
        assert!(matches!(Store::open_snapshot(&path, cfg()), Err(kernel::Error::Corrupt { why, .. }) if why == refusal));
        assert_eq!(files(&path), before);
        // Bad CRC is damage: the independently intact older slot may win.
        data[copy * PAGE_SIZE + 36] ^= 1;
        fs::write(path.join("data"), &data).unwrap();
        drop(Store::open_snapshot(&path, cfg()).unwrap());
    }
    }
}
