use kernel::{
    io::IoMode,
    store::{Config, Store, SyncMode},
};
#[test]
#[cfg_attr(
    not(feature = "sqlite-balance"),
    ignore = "P1 shuffled-density defect without sibling redistribution"
)]
fn shuffled_entries_use_neighbor_capacity() {
    let base = std::env::temp_dir();
    assert!(base.starts_with("<scratch>"));
    let d = tempfile::tempdir_in(base).unwrap();
    let c = Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Off,
    };
    let mut s = Store::create(d.path(), c).unwrap();
    let n = 8000u64;
    for i in 0..n {
        let id = 1 + (i * 7919 + 1237) % n;
        s.put(&id.to_be_bytes(), &[19; 180]).unwrap();
    }
    s.commit().unwrap();
    s.checkpoint().unwrap();
    let size = std::fs::metadata(d.path().join("data")).unwrap().len();
    assert!(
        size <= (n * 196 * 125) / 100,
        "file {size} exceeds 25% framing+packing allowance over {n}*196"
    );
    for id in 1..=n {
        assert_eq!(s.get(&id.to_be_bytes()).unwrap(), Some(vec![19; 180]));
    }
}

#[test]
fn neighbor_changes_preserve_snapshot_and_wal_reopen() {
    let base = std::env::temp_dir();
    assert!(base.starts_with("<scratch>"));
    let d = tempfile::tempdir_in(base).unwrap();
    let c = Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    };
    let mut s = Store::create(d.path(), c).unwrap();
    for i in 0..2000u64 {
        s.put(&(i * 4).to_be_bytes(), &vec![7; 80 + (i % 13) as usize])
            .unwrap();
    }
    s.commit().unwrap();
    s.checkpoint().unwrap();
    let snapshot = Store::open_snapshot(d.path(), c).unwrap();
    for i in 0..8000u64 {
        let id = (i * 7919 + 1237) % 8000;
        s.put(&id.to_be_bytes(), &vec![23; 160 + (id % 31) as usize])
            .unwrap();
        if i % 1000 == 999 {
            s.commit().unwrap();
        }
    }
    // WAL reopen before checkpoint tests replay of redistribution decisions.
    drop(s);
    let mut s = Store::open(d.path(), c).unwrap();
    for id in 0..8000u64 {
        assert_eq!(
            s.get(&id.to_be_bytes()).unwrap(),
            Some(vec![23; 160 + (id % 31) as usize])
        );
    }
    for i in 0..2000u64 {
        assert_eq!(
            snapshot.get(&(i * 4).to_be_bytes()).unwrap(),
            Some(vec![7; 80 + (i % 13) as usize])
        );
    }
    s.checkpoint().unwrap();
    kernel::verify::verify_published_tree(
        &d.path().join("data"),
        IoMode::Buffered,
        s.published_root(),
        1,
    )
    .unwrap();
    for i in 0..2000u64 {
        assert_eq!(
            snapshot.get(&(i * 4).to_be_bytes()).unwrap(),
            Some(vec![7; 80 + (i % 13) as usize])
        );
    }
}

#[test]
fn compact_keys_overflow_updates_bulk_and_recovery() {
    let base = std::env::temp_dir();
    assert!(base.starts_with("<scratch>"));
    let d = tempfile::tempdir_in(base).unwrap();
    let c = Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    };
    let mut s = Store::create(d.path(), c).unwrap();
    let key = |id: u16| {
        let mut k = vec![0x82];
        k.extend_from_slice(&id.to_be_bytes());
        k
    };
    for id in 0..1000u16 {
        s.put(&key(id), &vec![(id % 251) as u8; 50]).unwrap();
    }
    s.commit().unwrap();
    s.checkpoint().unwrap();
    let snapshot = Store::open_snapshot(d.path(), c).unwrap();
    for id in 0..1000u16 {
        s.put(
            &key(id),
            &vec![(id % 251) as u8; if id % 7 == 0 { 7000 } else { 70 }],
        )
        .unwrap();
    }
    s.commit().unwrap();
    drop(s);
    let mut s = Store::open(d.path(), c).unwrap();
    for id in 0..1000u16 {
        assert_eq!(
            s.get(&key(id)).unwrap(),
            Some(vec![(id % 251) as u8; if id % 7 == 0 { 7000 } else { 70 }])
        );
        assert_eq!(
            snapshot.get(&key(id)).unwrap(),
            Some(vec![(id % 251) as u8; 50])
        );
    }
    s.checkpoint().unwrap();
    drop(snapshot);
    drop(s);
    // Recovery must use the same compact-cell decoder as point reads.
    kernel::recover::recover(d.path(), c).unwrap();
    let s = Store::open(d.path(), c).unwrap();
    for id in 0..1000u16 {
        assert_eq!(
            s.get(&key(id)).unwrap(),
            Some(vec![(id % 251) as u8; if id % 7 == 0 { 7000 } else { 70 }])
        );
    }
    drop(s);
    let e = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let mut s = Store::create(e.path(), c).unwrap();
    s.bulk_load((0..1000u16).map(|id| (key(id), vec![23; 80])))
        .unwrap();
    s.commit().unwrap();
    s.checkpoint().unwrap();
    drop(s);
    let s = Store::open(e.path(), c).unwrap();
    for id in 0..1000u16 {
        assert_eq!(s.get(&key(id)).unwrap(), Some(vec![23; 80]));
    }
}

#[test]
fn recovery_refuses_damaged_overflow_without_replacing_source() {
    let base = std::env::temp_dir();
    assert!(base.starts_with("<scratch>"));
    let d = tempfile::tempdir_in(base).unwrap();
    let c = Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    };
    let mut s = Store::create(d.path(), c).unwrap();
    s.put(&[0x81, 1], &vec![123; 9000]).unwrap();
    s.commit().unwrap();
    s.checkpoint().unwrap();
    drop(s);
    let data = d.path().join("data");
    let mut bytes = std::fs::read(&data).unwrap();
    let page = bytes
        .chunks_exact(4096)
        .position(|p| u16::from_le_bytes([p[6], p[7]]) == 4)
        .unwrap();
    bytes[page * 4096 + 50] ^= 1;
    std::fs::write(&data, &bytes).unwrap();
    assert!(kernel::recover::recover(d.path(), c).is_err());
    assert_eq!(std::fs::read(&data).unwrap(), bytes);
}
