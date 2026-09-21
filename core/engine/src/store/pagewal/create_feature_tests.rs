//! Explicit rebuild creation must not alter unrelated database defaults.
use super::*;
use std::{fs, sync::Barrier as ThreadBarrier, thread};

#[test]
fn creation_cap_headroom_covers_both_codecs_and_cache_sizes() {
    let root = tempfile::tempdir().unwrap();
    for compact in [false, true] {
        for cache in [2 * PAGE, 64 << 10] {
            let path = root.path().join(format!("{compact}-{cache}"));
            let mut db = PageWalStore::create_with_compact_cells(&path, cache, compact).unwrap();
            let cap = PageWalStore::creation_cap_headroom();
            db.set_cap(cap).unwrap();
            let managed = fs::metadata(path.join("data")).unwrap().len()
                + fs::metadata(path.join("wal")).unwrap().len();
            assert!(managed <= cap);
            assert_eq!(db.managed_cap(), Some(cap));
            drop(db);
            let db = PageWalStore::open(&path, false, cache).unwrap();
            assert_eq!(db.managed_cap(), Some(cap));
        }
    }
}

#[test]
fn explicit_codecs_are_independent_of_concurrent_default_creates() {
    let root = tempfile::tempdir().unwrap();
    let original = create_features();
    let barrier = Arc::new(ThreadBarrier::new(3));
    thread::scope(|scope| {
        for mode in [None, Some(false), Some(true)] {
            let root = root.path().to_path_buf();
            let barrier = barrier.clone();
            scope.spawn(move || {
                barrier.wait();
                for round in 0..8 {
                    let path = root.join(format!("{mode:?}-{round}"));
                    let mut db = match mode {
                        Some(compact) => {
                            PageWalStore::create_with_compact_cells(&path, 64 << 10, compact)
                        }
                        None => PageWalStore::open(&path, true, 64 << 10),
                    }
                    .unwrap();
                    let expected =
                        mode.map_or(original, |compact| if compact { COMPACT_CELLS } else { 0 });
                    assert_eq!(
                        db.pager.as_ref().unwrap().state.lock().unwrap().features,
                        expected
                    );
                    assert_eq!(create_features(), original);
                    db.put(b"key", b"value").unwrap();
                    db.commit().unwrap();
                    db.checkpoint().unwrap();
                    drop(db);
                    let db = PageWalStore::open(&path, false, 64 << 10).unwrap();
                    assert_eq!(
                        db.pager.as_ref().unwrap().state.lock().unwrap().features,
                        expected
                    );
                    assert_eq!(
                        db.get(b"key").unwrap().as_deref(),
                        Some(b"value".as_slice())
                    );
                    assert_eq!(create_features(), original);
                }
            });
        }
    });
}

#[test]
fn explicit_create_refuses_existing_directory_without_changing_bytes_or_default() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("db");
    let original = create_features();
    let mut db = PageWalStore::open(&path, true, 64 << 10).unwrap();
    db.put(b"key", b"value").unwrap();
    db.commit().unwrap();
    drop(db);
    let inventory = || {
        let mut files: Vec<_> = fs::read_dir(&path)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), fs::read(entry.path()).unwrap())
            })
            .collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files
    };
    let before = inventory();
    assert!(PageWalStore::create_with_compact_cells(&path, 64 << 10, original == 0).is_err());
    assert_eq!(inventory(), before);
    assert_eq!(create_features(), original);
}
