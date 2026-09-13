//! A reader closing between checkpoints must not pin the next whole write epoch.
use kernel::{
    io::IoMode,
    store::{Config, Store, SyncMode},
};
fn config() -> Config {
    Config {
        budget_bytes: 64 << 10,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}
#[test]
fn commit_refreshes_reuse_after_reader_release_without_publishing() {
    let base = std::env::temp_dir();
    assert!(base.starts_with("<scratch>"));
    let dir = tempfile::tempdir_in(base).unwrap();
    let mut db = Store::create(dir.path(), config()).unwrap();
    for id in 0u32..128 {
        db.put(&id.to_be_bytes(), &[0; 200]).unwrap();
    }
    db.commit().unwrap();
    db.checkpoint().unwrap();
    let old = Store::open_snapshot(dir.path(), config()).unwrap();
    for version in 1..=3 {
        for id in 0u32..128 {
            db.put(&id.to_be_bytes(), &[version; 200]).unwrap();
        }
        db.commit().unwrap();
        db.checkpoint().unwrap();
        for id in 0u32..128 {
            assert_eq!(old.get(&id.to_be_bytes()).unwrap(), Some(vec![0; 200]));
        }
    }
    let before = db.pool_ref().free_pages_split();
    let generation = db.generation();
    // A commit while the reader lives must not release its protected pages.
    db.commit().unwrap();
    assert_eq!(db.pool_ref().free_pages_split(), before);
    // Ambiguous live registration must still prevent reuse at commit.
    let slot = std::fs::read_dir(dir.path().join("readers"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let original = std::fs::read(&slot).unwrap();
    let mut corrupt = original.clone();
    corrupt[20] ^= 1;
    std::fs::write(&slot, corrupt).unwrap();
    db.commit().unwrap();
    assert_eq!(db.pool_ref().free_pages_split().0, 0);
    std::fs::write(&slot, original).unwrap();
    drop(old);
    db.commit().unwrap();
    let after = db.pool_ref().free_pages_split();
    assert!(
        after.0 > before.0,
        "closed reader still pins pages until checkpoint: before={before:?}, after={after:?}"
    );
    assert!(after.1 > 0, "latest fallback pages must remain protected");
    assert_eq!(
        db.generation(),
        generation,
        "commit must not publish a new snapshot"
    );
    let current = Store::open_snapshot(dir.path(), config()).unwrap();
    for id in 0u32..128 {
        db.put(&id.to_be_bytes(), &[4; 200]).unwrap();
    }
    db.commit().unwrap();
    db.checkpoint().unwrap();
    for id in 0u32..128 {
        assert_eq!(current.get(&id.to_be_bytes()).unwrap(), Some(vec![3; 200]));
    }
    drop(current);
    drop(db);
    let db = Store::open(dir.path(), config()).unwrap();
    for id in 0u32..128 {
        assert_eq!(db.get(&id.to_be_bytes()).unwrap(), Some(vec![4; 200]));
    }
}

#[test]
fn repeated_root_publication_reuses_two_trees_and_survives_one_lost_meta() {
    use std::io::{Read, Seek, SeekFrom, Write};
    let base = std::env::temp_dir();
    assert!(base.starts_with("<scratch>"));
    let dir = tempfile::tempdir_in(base).unwrap();
    let mut db = Store::create(dir.path(), config()).unwrap();
    for id in 0u32..512 {
        db.put(&id.to_be_bytes(), &[0; 200]).unwrap();
    }
    db.commit().unwrap();
    db.checkpoint().unwrap();
    let initial_pages = db.pool_ref().page_count();
    for version in 1..=6 {
        let reader = Store::open_snapshot(dir.path(), config()).unwrap();
        for id in 0u32..512 {
            db.put(&id.to_be_bytes(), &[version; 200]).unwrap();
        }
        db.commit().unwrap();
        db.checkpoint().unwrap();
        for id in 0u32..512 {
            assert_eq!(
                reader.get(&id.to_be_bytes()).unwrap(),
                Some(vec![version - 1; 200])
            );
        }
        drop(reader);
        // Experimental caller policy, not a change to default checkpoint.
        db.checkpoint().unwrap();
        assert!(
            db.pool_ref().page_count() <= 2 * initial_pages,
            "same-size updates should reuse two trees with duplicate publication"
        );
    }
    let newest = db.generation() % 2;
    drop(db);
    // Both metadata slots should point at the newest logical tree. Corrupt
    // just the newer slot; fallback must retain all latest committed values.
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.path().join("data"))
        .unwrap();
    f.seek(SeekFrom::Start(newest * 4096 + 60)).unwrap();
    let mut byte = [0];
    f.read_exact(&mut byte).unwrap();
    byte[0] ^= 1;
    f.seek(SeekFrom::Start(newest * 4096 + 60)).unwrap();
    f.write_all(&byte).unwrap();
    f.sync_all().unwrap();
    drop(f);
    let db = Store::open(dir.path(), config()).unwrap();
    for id in 0u32..512 {
        assert_eq!(db.get(&id.to_be_bytes()).unwrap(), Some(vec![6; 200]));
    }
}
