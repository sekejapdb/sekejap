//! The `compact-cells` cargo feature is now only the DEFAULT for databases a
//! build creates. It no longer decides what a build can open.
//!
//! Property: a database created declaring either supported cell encoding is
//! fully usable — created, written, reopened, written again — by this one
//! binary, and keeps the bits it was created with. That is the only way a
//! single build can stand in for "the other build" of the same release.
//!
//! This file holds ONE test on purpose: `set_create_compact_cells` is a
//! process-wide default, so a second test creating databases in parallel in
//! the same binary could observe the override.

use sekejap_core::{
    collections::Database,
    pagewal::{create_compact_cells, set_create_compact_cells, PageWalStore},
    Kind,
};
use kernel::{
    io::IoMode,
    page::{PageRef, PAGE_SIZE},
    store::{Config, SyncMode},
};
use serde_json::json;
use std::{fs, path::Path};

const FEATURES: usize = 48;

fn cfg() -> Config {
    Config { budget_bytes: 1 << 20, io: IoMode::Buffered, sync: SyncMode::Full }
}

fn declared(p: &Path) -> u64 {
    let data = fs::read(p.join("data")).unwrap();
    let slot = PageRef::open(&data[..PAGE_SIZE], 0).unwrap().slot(0).to_vec();
    u64::from_le_bytes(slot[FEATURES..FEATURES + 8].try_into().unwrap())
}

#[test]
fn either_created_encoding_round_trips_and_keeps_its_declared_bits() {
    let t = tempfile::tempdir().unwrap();
    let original = create_compact_cells();

    for compact in [false, true] {
        set_create_compact_cells(compact).unwrap();
        assert_eq!(create_compact_cells(), compact);
        let p = t.path().join(format!("created-{compact}"));

        let mut db = PageWalStore::open(&p, true, 32 << 10).unwrap();
        for i in 0..200u32 {
            db.put(&i.to_be_bytes(), format!("row-{i}").as_bytes()).unwrap();
        }
        db.commit().unwrap();
        db.checkpoint().unwrap();
        drop(db);

        let want = if compact { 1 } else { 0 };
        assert_eq!(declared(&p), want, "a created database declares its own encoding");

        // Reopen and mutate with the create-time default set the OTHER way:
        // the file's declaration must win over the process default.
        set_create_compact_cells(!compact).unwrap();
        let mut db = PageWalStore::open(&p, false, 32 << 10).unwrap();
        for i in 0..200u32 {
            assert_eq!(
                db.get(&i.to_be_bytes()).unwrap().as_deref(),
                Some(format!("row-{i}").as_bytes()),
            );
        }
        db.put(&7u32.to_be_bytes(), b"updated").unwrap();
        db.put(&9999u32.to_be_bytes(), b"appended").unwrap();
        assert!(db.delete(&11u32.to_be_bytes()).unwrap());
        db.commit().unwrap();
        db.checkpoint().unwrap();
        drop(db);
        assert_eq!(declared(&p), want, "a routine write must not restamp the header");

        let db = PageWalStore::open(&p, false, 32 << 10).unwrap();
        assert_eq!(db.get(&7u32.to_be_bytes()).unwrap().as_deref(), Some(b"updated".as_slice()));
        assert_eq!(db.get(&9999u32.to_be_bytes()).unwrap().as_deref(), Some(b"appended".as_slice()));
        assert_eq!(db.get(&11u32.to_be_bytes()).unwrap(), None);
        for i in (0..200u32).filter(|i| ![7, 11].contains(i)) {
            assert_eq!(
                db.get(&i.to_be_bytes()).unwrap().as_deref(),
                Some(format!("row-{i}").as_bytes()),
            );
        }
        drop(db);
        assert_eq!(declared(&p), want, "reopen must not restamp the header");
    }

    // The same promise one layer up: typed collections created in either
    // encoding survive a reopen by a process whose default is the other one.
    for compact in [false, true] {
        set_create_compact_cells(compact).unwrap();
        let p = t.path().join(format!("typed-{compact}"));
        let mut db = Database::create(&p, cfg()).unwrap();
        let c = db
            .create_collection("c", vec![("n".into(), Kind::Int), ("s".into(), Kind::Text)], Default::default())
            .unwrap();
        for i in 0..100i64 {
            db.put(c, &format!("k{i}"), &json!({"n": i, "s": format!("row-{i}")})).unwrap();
        }
        db.commit().unwrap();
        drop(db);
        let want = if compact { 1 } else { 0 };
        assert_eq!(declared(&p), want);

        set_create_compact_cells(!compact).unwrap();
        let mut db = Database::open(&p, cfg()).unwrap();
        db.update(c, "k7", &json!({"s": "updated"})).unwrap();
        db.delete(c, "k11").unwrap();
        db.put(c, "k999", &json!({"n": 999, "s": "appended"})).unwrap();
        db.commit().unwrap();
        drop(db);
        assert_eq!(declared(&p), want, "a typed write must not restamp the header");

        let db = Database::open(&p, cfg()).unwrap();
        assert_eq!(db.get(c, "k7").unwrap().unwrap().document, json!({"n": 7, "s": "updated"}));
        assert_eq!(db.get(c, "k11").unwrap(), None);
        assert_eq!(db.get(c, "k999").unwrap().unwrap().document, json!({"n": 999, "s": "appended"}));
        for i in (0..100i64).filter(|i| ![7, 11].contains(i)) {
            assert_eq!(
                db.get(c, &format!("k{i}")).unwrap().unwrap().document,
                json!({"n": i, "s": format!("row-{i}")}),
            );
        }
    }

    set_create_compact_cells(original).unwrap();
    assert_eq!(create_compact_cells(), original);
}
