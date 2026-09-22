//! An old snapshot must retain its own pages without pinning every newer version.
use kernel::{
    io::IoMode,
    store::{Config, Store, SyncMode},
};
fn value(id: u32, version: u8) -> Vec<u8> {
    vec![version; if id % 17 == 0 { 9000 } else { 200 }]
}
fn cfg() -> Config {
    Config {
        budget_bytes: 64 << 10,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}
fn exercise(reopen_every: u8) {
    let base = std::env::temp_dir();
    let dir = tempfile::tempdir_in(base).unwrap();
    let mut writer = Store::create(dir.path(), cfg()).unwrap();
    for id in 0u32..1024 {
        writer.put(&id.to_be_bytes(), &value(id, 0)).unwrap();
    }
    writer.commit().unwrap();
    writer.checkpoint().unwrap();
    let initial = writer.pool_ref().page_count();
    let oldest = Store::open_snapshot(dir.path(), cfg()).unwrap();
    let mut middle = None;
    for version in 1u8..=24 {
        for id in 0u32..1024 {
            writer.put(&id.to_be_bytes(), &value(id, version)).unwrap();
        }
        writer.commit().unwrap();
        writer.checkpoint().unwrap();
        if version == 4 {
            middle = Some(Store::open_snapshot(dir.path(), cfg()).unwrap());
        }
        for id in 0u32..1024 {
            assert_eq!(oldest.get(&id.to_be_bytes()).unwrap(), Some(value(id, 0)));
            if let Some(reader) = &middle {
                assert_eq!(reader.get(&id.to_be_bytes()).unwrap(), Some(value(id, 4)));
            }
            assert_eq!(
                writer.get(&id.to_be_bytes()).unwrap(),
                Some(value(id, version))
            );
        }
        if version % reopen_every == 0 {
            drop(writer);
            writer = Store::open(dir.path(), cfg()).unwrap();
        }
        assert!(
            writer.pool_ref().page_count() <= initial * 7,
            "intermediate versions accumulate: version={version}, pages={}, initial={initial}",
            writer.pool_ref().page_count()
        );
    }
    println!(
        "initial_pages={initial} final_pages={} publications=24 readers=2",
        writer.pool_ref().page_count()
    );
    drop(middle);
    drop(oldest);
    drop(writer);
    let reader = Store::open(dir.path(), cfg()).unwrap();
    for id in 0u32..1024 {
        assert_eq!(reader.get(&id.to_be_bytes()).unwrap(), Some(value(id, 24)));
    }
}

#[test]
fn intermediate_versions_are_reused_with_two_live_snapshots_and_reopen() {
    exercise(12);
}
#[test]
fn every_reopen_preserves_version_lifetimes() {
    exercise(1);
}

#[test]
fn corrupt_or_impossible_persisted_lifetimes_never_enable_reuse() {
    let base = std::env::temp_dir();
    let dir = tempfile::tempdir_in(base).unwrap();
    let mut s = Store::create(dir.path(), cfg()).unwrap();
    for version in 0..3u8 {
        for id in 0..128u32 {
            s.put(&id.to_be_bytes(), &value(id, version)).unwrap();
        }
        s.commit().unwrap();
        s.checkpoint().unwrap();
    }
    let bytes = s.pool_ref().export_free(s.generation());
    assert_eq!(u16::from_le_bytes(bytes[8..10].try_into().unwrap()), 2);
    assert!(bytes.len() > 52);
    let before = s.pool_ref().free_pages_split();
    let mut bad = bytes.clone();
    bad[40] ^= 1;
    assert!(!s.pool_ref().import_free(&bad, s.generation()));
    let retired = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
    let mut impossible = bytes.clone();
    impossible[40..48].copy_from_slice(&(retired + 1).to_le_bytes());
    let end = impossible.len() - 4;
    let crc = crc32c::crc32c(&impossible[..end]);
    impossible[end..].copy_from_slice(&crc.to_le_bytes());
    assert!(
        !s.pool_ref().import_free(&impossible, s.generation()),
        "birth after retirement must fail even with a valid CRC"
    );
    let mut old = bytes;
    old[8..10].copy_from_slice(&1u16.to_le_bytes());
    let end = old.len() - 4;
    let crc = crc32c::crc32c(&old[..end]);
    old[end..].copy_from_slice(&crc.to_le_bytes());
    assert!(!s.pool_ref().import_free(&old, s.generation()));
    assert_eq!(s.pool_ref().free_pages_split(), before);
    for id in 0..128u32 {
        assert_eq!(s.get(&id.to_be_bytes()).unwrap(), Some(value(id, 2)));
    }
}
