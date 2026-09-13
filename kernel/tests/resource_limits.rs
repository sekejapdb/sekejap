use kernel::{
    io::IoMode,
    limits::ResourceLimits,
    store::{Config, Store, SyncMode},
    Error,
};

fn cfg() -> Config {
    Config {
        budget_bytes: 256 << 10,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}
fn limits() -> ResourceLimits {
    ResourceLimits {
        data_bytes: 512 << 10,
        wal_bytes: 64 << 10,
        tracked_pages: 256,
        readers: 2,
        record_bytes: 16 << 10,
        recovery_bytes: 64 << 10,
    }
}

#[test]
fn refusal_preserves_acknowledged_rows_and_old_reader_with_no_replay_growth() {
    let d = tempfile::tempdir().unwrap();
    let lim = limits();
    let mut w = Store::create_limited(&d.path().join("db"), cfg(), lim).unwrap();
    for i in 0u64..100 {
        w.put(&i.to_be_bytes(), &[7; 100]).unwrap();
    }
    w.commit().unwrap();
    let r = Store::open_snapshot(&d.path().join("db"), cfg()).unwrap();
    let mut committed = 100u64;
    'load: loop {
        for i in committed..committed + 10 {
            match w.put(&i.to_be_bytes(), &[9; 4000]) {
                Ok(()) => assert!(w.pool_ref().page_count() as u64 * 4096 <= lim.data_bytes),
                Err(Error::ResourceLimit(_)) => break 'load,
                Err(e) => panic!("unexpected {e}"),
            }
        }
        w.commit().unwrap();
        committed += 10;
        assert!(committed < 10000, "limit was not enforced");
    }
    assert!(matches!(w.commit(), Err(Error::StorePoisoned)));
    assert!(matches!(w.get(&0u64.to_be_bytes()), Err(Error::StorePoisoned)),
        "a failed working tree must not serve entry reads before reopen");
    assert!(matches!(w.scan(&[]), Err(Error::StorePoisoned)));
    assert!(matches!(w.scan_reverse(&[255]), Err(Error::StorePoisoned)));
    for i in 0u64..100 {
        assert_eq!(r.get(&i.to_be_bytes()).unwrap(), Some(vec![7; 100]));
    }
    let before = std::fs::metadata(d.path().join("db/data")).unwrap().len();
    assert!(before <= lim.data_bytes);
    drop(w);
    let w = Store::open(&d.path().join("db"), cfg()).unwrap();
    assert_eq!(w.resource_limits(), Some(lim));
    assert_eq!(w.scan(&[]).unwrap().count() as u64, committed);
    assert_eq!(
        std::fs::metadata(d.path().join("db/data")).unwrap().len(),
        before
    );
}

#[test]
fn readers_are_bounded_and_reusable_and_oversize_input_is_preflighted() {
    let d = tempfile::tempdir().unwrap();
    let mut w = Store::create_limited(&d.path().join("db"), cfg(), limits()).unwrap();
    w.put(b"a", b"old").unwrap();
    w.commit().unwrap();
    let r1 = Store::open_snapshot(&d.path().join("db"), cfg()).unwrap();
    let r2 = Store::open_snapshot(&d.path().join("db"), cfg()).unwrap();
    assert!(matches!(
        Store::open_snapshot(&d.path().join("db"), cfg()),
        Err(Error::ResourceLimit(_))
    ));
    drop(r1);
    let _r3 = Store::open_snapshot(&d.path().join("db"), cfg()).unwrap();
    assert!(matches!(
        w.put(b"big", &vec![0; 17000]),
        Err(Error::ResourceLimit(_))
    ));
    w.put(b"a", b"new").unwrap();
    w.commit().unwrap();
    assert_eq!(r2.get(b"a").unwrap(), Some(b"old".to_vec()));
    assert!(matches!(
        w.bulk_load(std::iter::empty()),
        Err(Error::ResourceLimit(_))
    ));
}

#[test]
fn memory_reservation_overflow_is_a_refusal() {
    use kernel::budget::{Class, MemoryBudget};
    let b = std::sync::Arc::new(MemoryBudget::new(usize::MAX));
    let _r = b.reserve(Class::Pool, 8).unwrap();
    assert!(matches!(
        b.reserve(Class::Query, usize::MAX),
        Err(Error::OutOfBudget)
    ));
}

#[test]
fn wal_and_bookkeeping_exhaustion_are_separate_safe_refusals() {
    for metadata in [false, true] {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("db");
        let mut lim = limits();
        lim.data_bytes = 8 << 20;
        if metadata {
            lim.tracked_pages = 4;
        }
        let mut w = Store::create_limited(&path, cfg(), lim).unwrap();
        for i in 0u64..20 {
            w.put(&i.to_be_bytes(), &[7; 4000]).unwrap();
            w.commit().unwrap();
        }
        let reader = Store::open_snapshot(&path, cfg()).unwrap();
        let reason = 'found: loop {
            for i in 0u64..20 {
                match w.put(&i.to_be_bytes(), &[8; 4000]) {
                    Ok(()) => (),
                    Err(Error::ResourceLimit(why)) => break 'found why,
                    Err(e) => panic!("unexpected: {e}"),
                }
            }
            panic!("the transaction should have hit its allowance");
        };
        assert!(
            reason.contains(if metadata { "bookkeeping" } else { "WAL" }),
            "{reason}"
        );
        let (retired, recycled) = w.pool_ref().tracked_pages();
        assert!(retired <= lim.tracked_pages as usize && recycled <= lim.tracked_pages as usize);
        assert!(std::fs::metadata(path.join("wal")).unwrap().len() <= lim.wal_bytes);
        drop(w);
        let w = Store::open(&path, cfg()).unwrap();
        for i in 0u64..20 {
            assert_eq!(reader.get(&i.to_be_bytes()).unwrap(), Some(vec![7; 4000]));
            assert_eq!(w.get(&i.to_be_bytes()).unwrap(), Some(vec![7; 4000]));
        }
    }
}

#[test]
fn policy_survives_one_bad_meta_and_create_cannot_disable_it() {
    use std::io::{Read, Seek, SeekFrom, Write};
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("db");
    let mut w = Store::create_limited(&path, cfg(), limits()).unwrap();
    w.put(b"a", b"one").unwrap();
    w.commit().unwrap();
    w.put(b"b", b"two").unwrap();
    w.commit().unwrap();
    let latest = w.generation() % 2;
    drop(w);
    assert!(Store::create(&path, cfg()).is_err());
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.join("data"))
        .unwrap();
    f.seek(SeekFrom::Start(latest * 4096 + 100)).unwrap();
    let mut b = [0];
    f.read_exact(&mut b).unwrap();
    b[0] ^= 1;
    f.seek(SeekFrom::Start(latest * 4096 + 100)).unwrap();
    f.write_all(&b).unwrap();
    f.sync_all().unwrap();
    let w = Store::open(&path, cfg()).unwrap();
    assert_eq!(w.resource_limits(), Some(limits()));
    assert_eq!(w.get(b"a").unwrap(), Some(b"one".to_vec()));
    assert_eq!(w.get(b"b").unwrap(), None); // precisely named fallback loss
}

#[test]
fn constrained_source_can_be_salvaged_without_modifying_its_budgeted_files() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("db");
    let mut w = Store::create_limited(&path, cfg(), limits()).unwrap();
    w.put(b"a", &[7; 9000]).unwrap();
    w.put(b"b", b"small").unwrap();
    w.commit().unwrap();
    drop(w);
    let before: Vec<_> = ["data", "wal", "free"]
        .map(|f| std::fs::read(path.join(f)).unwrap())
        .into();
    let repair_cfg = Config {
        budget_bytes: 8 << 20,
        ..cfg()
    };
    assert!(matches!(
        kernel::recover::recover(&path, repair_cfg),
        Err(Error::ResourceLimit(_))
    ));
    let report = kernel::recover::recover_to(&path, &d.path().join("export"), repair_cfg).unwrap();
    let recovered = Store::open(&report.database, cfg()).unwrap();
    assert_eq!(recovered.get(b"a").unwrap(), Some(vec![7; 9000]));
    assert_eq!(recovered.get(b"b").unwrap(), Some(b"small".to_vec()));
    for (i, f) in ["data", "wal", "free"].iter().enumerate() {
        assert_eq!(std::fs::read(path.join(f)).unwrap(), before[i]);
    }
}

#[test]
fn concurrent_reader_admission_keeps_the_same_fixed_slot_inodes() {
    use std::sync::{Arc, Barrier};
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("db");
    let mut w = Store::create_limited(&path, cfg(), limits()).unwrap();
    w.put(b"a", b"old").unwrap();
    w.commit().unwrap();
    let entered = Arc::new(Barrier::new(9));
    let release = Arc::new(Barrier::new(9));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let (path, entered, release) = (path.clone(), entered.clone(), release.clone());
            std::thread::spawn(move || {
                let r = Store::open_snapshot(&path, cfg());
                entered.wait();
                release.wait();
                match r {
                    Ok(r) => {
                        assert_eq!(r.get(b"a").unwrap(), Some(b"old".to_vec()));
                        true
                    }
                    Err(Error::ResourceLimit(_)) => false,
                    Err(e) => panic!("unexpected reader refusal: {e}"),
                }
            })
        })
        .collect();
    entered.wait();
    for _ in 0..5 {
        w.put(b"a", b"new").unwrap();
        w.commit().unwrap();
    }
    release.wait();
    assert_eq!(
        workers
            .into_iter()
            .map(|w| w.join().unwrap() as usize)
            .sum::<usize>(),
        2
    );
    assert_eq!(std::fs::read_dir(path.join("readers")).unwrap().count(), 2);
    assert!(Store::open_snapshot(&path, cfg()).is_ok());
}

#[test]
fn duplicate_freelist_pages_are_rejected_before_installing_reuse_state() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("db");
    let mut w = Store::create_limited(&path, cfg(), limits()).unwrap();
    w.put(b"a", b"old").unwrap();
    w.commit().unwrap();
    let mut b = b"SEKFREE\0".to_vec();
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&[0; 6]);
    b.extend_from_slice(&w.generation().to_le_bytes());
    b.extend_from_slice(&w.generation().to_le_bytes());
    b.extend_from_slice(&2u32.to_le_bytes());
    for _ in 0..2 {
        b.extend_from_slice(&2u32.to_le_bytes());
        b.extend_from_slice(&0u64.to_le_bytes());
    }
    b.extend_from_slice(&crc32c::crc32c(&b).to_le_bytes());
    let before = w.pool_ref().tracked_pages();
    assert!(!w.pool_ref().import_free(&b, w.generation()));
    assert_eq!(w.pool_ref().tracked_pages(), before);
}
