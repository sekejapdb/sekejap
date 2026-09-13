use kernel::{
    io::IoMode,
    store::{Config, Store, SyncMode},
};
fn cfg() -> Config {
    Config {
        budget_bytes: 64 << 10,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}
#[test]
fn wal_and_page_triggers_have_distinct_effects_and_snapshots_remain_exact() {
    let base = std::env::temp_dir();
    assert!(base.starts_with("<scratch>"));
    let d = tempfile::tempdir_in(base).unwrap();
    let mut s = Store::create(d.path(), cfg()).unwrap();
    s.put(b"a", b"initial").unwrap();
    assert!(!s.commit_with_checkpoint(u64::MAX, u64::MAX).unwrap());
    s.checkpoint().unwrap();
    let old = Store::open_snapshot(d.path(), cfg()).unwrap();
    let g = s.generation();
    // This tiny WAL record requires a complete shadow page.
    s.put(b"a", b"small").unwrap();
    assert!(s.commit_with_checkpoint(u64::MAX, 4096).unwrap());
    assert_eq!(s.generation(), g + 1);
    assert_eq!(old.get(b"a").unwrap(), Some(b"initial".to_vec()));
    s.put(b"a", b"next").unwrap();
    assert!(s.commit_with_checkpoint(1, u64::MAX).unwrap());
    drop(s);
    let s = Store::open(d.path(), cfg()).unwrap();
    assert_eq!(s.get(b"a").unwrap(), Some(b"next".to_vec()));
}
