use kernel::{io::IoMode, store::{Config, Store, SyncMode}};
use std::{fs::File, io::{Read, Seek, SeekFrom}};

fn cfg() -> Config {
    Config { budget_bytes: 256 << 10, io: IoMode::Buffered, sync: SyncMode::Full }
}
fn fill(w: &mut Store, round: u8) {
    for i in 0..120u64 { w.put(&i.to_be_bytes(), &vec![round; 6000]).unwrap(); }
    w.checkpoint().unwrap();
}
fn rows(w: &Store, round: u8) {
    for i in 0..120u64 { assert_eq!(w.get(&i.to_be_bytes()).unwrap(), Some(vec![round; 6000])); }
}

#[test]
fn repeated_checkpoints_reuse_the_open_freelist_file() {
    let d = tempfile::tempdir().unwrap();
    let mut w = Store::create(d.path(), cfg()).unwrap();
    fill(&mut w, 1);
    let snapshot = Store::open_snapshot(d.path(), cfg()).unwrap();
    let mut standing = File::open(d.path().join("free")).unwrap();
    for round in 2..6 {
        fill(&mut w, round);
        standing.seek(SeekFrom::Start(0)).unwrap();
        let mut through_old_handle = Vec::new();
        standing.read_to_end(&mut through_old_handle).unwrap();
        assert_eq!(through_old_handle, std::fs::read(d.path().join("free")).unwrap(),
            "checkpoint replaced the file instead of reusing its durable name");
        assert_eq!(u64::from_le_bytes(through_old_handle[16..24].try_into().unwrap()), w.generation());
        rows(&snapshot, 1);
        drop(w);
        w = Store::open(d.path(), cfg()).unwrap();
        rows(&w, round);
        assert!(w.pool_ref().free_pages_pending() > 0);
    }
}

#[test]
fn interrupted_hint_replacement_preserves_current_and_fallback_rows() {
    // These persisted images model loss before creation, truncation, a partial
    // overwrite, and an intact old generation surviving a power loss. Restore
    // the same independently saved data image before every reopen/churn oracle.
    let d = tempfile::tempdir().unwrap();
    let mut w = Store::create(d.path(), cfg()).unwrap();
    fill(&mut w, 1);
    let old = std::fs::read(d.path().join("free")).unwrap();
    fill(&mut w, 2);
    let generation = w.generation();
    let current = std::fs::read(d.path().join("free")).unwrap();
    drop(w);
    let data = std::fs::read(d.path().join("data")).unwrap();
    let mut mixed = old.clone();
    let n = current.len().min(mixed.len()) / 2;
    mixed[..n].copy_from_slice(&current[..n]);
    let images = [None, Some(Vec::new()), Some(current[..17].to_vec()), Some(mixed), Some(old)];
    for image in images {
        std::fs::write(d.path().join("data"), &data).unwrap();
        std::fs::write(d.path().join("wal"), []).unwrap();
        let _ = std::fs::remove_file(d.path().join("free"));
        if let Some(bytes) = &image { std::fs::write(d.path().join("free"), bytes).unwrap(); }
        let mut w = Store::open(d.path(), cfg()).unwrap();
        assert_eq!(w.pool_ref().free_pages_pending(), 0, "bad hint was imported");
        rows(&w, 2);
        fill(&mut w, 3);
        drop(w);
        rows(&Store::open(d.path(), cfg()).unwrap(), 3);

        let mut fallback = data.clone();
        fallback[(generation as usize % 2) * 4096 + 100] ^= 1;
        std::fs::write(d.path().join("data"), fallback).unwrap();
        std::fs::write(d.path().join("wal"), []).unwrap();
        // A newer hint must never supply free pages to the older root.
        std::fs::write(d.path().join("free"), &current).unwrap();
        let r = Store::open(d.path(), cfg()).unwrap();
        assert_eq!(r.pool_ref().free_pages_pending(), 0);
        rows(&r, 1);
    }
}
