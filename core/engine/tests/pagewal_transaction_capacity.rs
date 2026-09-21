//! Protect the supported atomic batch that wider redistribution exhausted.
use sekejap_core::pagewal::PageWalStore;

fn value(id: u64) -> [u8; 256] {
    let mut bytes = [b'a' + (id % 26) as u8; 256];
    bytes[..8].copy_from_slice(&id.to_le_bytes());
    bytes
}

#[test]
fn scattered_thousand_insert_transaction_preserves_old_reader_and_reopens() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut db = PageWalStore::open(&path, true, 8 << 20).unwrap();
    for first in (0..100_000u64).step_by(1000) {
        for row in first..first + 1000 {
            db.put(&(2 * row).to_be_bytes(), &value(2 * row)).unwrap();
        }
        db.commit().unwrap();
    }
    assert!(db.checkpoint().unwrap());
    let old = db.snapshot().unwrap();
    for step in 0..1000 {
        let id = 200 * ((step * 619 + 37) % 1000) + 1;
        db.put(&u64::to_be_bytes(id), &value(id)).unwrap();
    }
    db.commit().unwrap();
    let mut count = 0u64;
    old.scan(|key, bytes| {
        let id = 2 * count;
        assert_eq!(key, &id.to_be_bytes());
        assert_eq!(bytes, &value(id));
        count += 1;
        true
    }).unwrap();
    assert_eq!(count, 100_000);
    drop(old);
    assert!(db.checkpoint().unwrap());
    drop(db);
    let db = PageWalStore::open(&path, false, 8 << 20).unwrap();
    let mut count = 0;
    let mut inserted = 0;
    let mut previous = None;
    db.scan(|key, bytes| {
        let id = u64::from_be_bytes(key.try_into().unwrap());
        assert!(previous.is_none_or(|p| id > p));
        previous = Some(id);
        assert!(id < 200_000);
        if id % 2 == 1 {
            assert_eq!(id % 200, 1);
            inserted += 1;
        }
        assert_eq!(bytes, &value(id));
        count += 1;
        true
    }).unwrap();
    assert_eq!((count, inserted), (101_000, 1000));
}
