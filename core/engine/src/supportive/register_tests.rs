//! The Register on disk (`docs/core/SUPPORTIVE.md` 2.0.3). What is at risk,
//! one test each:
//!
//! * a critical entry lives in all three fixed trees and an ignorable one in
//!   copy 0 only, and both read back after a commit and a reopen
//!   (`critical_entries_live_in_three_copies_ignorable_in_one`);
//! * copy 0 answers while it is intact; when it is damaged the other copies
//!   answer and must agree, and no intact copy at all is corruption
//!   (`a_damaged_copy_loses_and_a_disagreement_behind_it_is_corruption`;
//!   disagreement between intact copies is the verifier's to find);
//! * a damaged ignorable entry reads as missing, never as corruption
//!   (`a_damaged_ignorable_entry_reads_as_missing`);
//! * delete removes every copy, and a node lists only its own entries, in key
//!   order (`delete_removes_every_copy_and_a_node_lists_its_own`).

use super::carrier::*;
use super::register::Register;
use crate::collections::{Database, Error};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};

fn cfg() -> Config {
    Config { budget_bytes: 1 << 20, io: IoMode::Buffered, sync: SyncMode::Full }
}

fn key(node: Node, kind: &[u8; 4], id: u64) -> Key {
    Key { node, owner_class: OwnerClass::Table, owner_id: 2, kind: Kind::new(kind).unwrap(), item: Item::Id(id) }
}

#[test]
fn critical_entries_live_in_three_copies_ignorable_in_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let roots;
    {
        let mut db = legacy_create(&path, cfg()).unwrap();
        let store = db.writer().unwrap();
        let mut reg = Register::create(store).unwrap();
        reg.put(store, &key(Node::C, b"COLM", 7), 1, b"column seven").unwrap();
        reg.put(store, &key(Node::G, b"rCNT", 0), 1, b"42 rows").unwrap();
        let colm = key(Node::C, b"COLM", 7).encode().unwrap();
        let rcnt = key(Node::G, b"rCNT", 0).encode().unwrap();
        for (i, tree) in REGISTER_TREES.iter().enumerate() {
            assert!(store.tree_get(*tree, reg.roots()[i], &colm).unwrap().is_some(), "COLM in copy {i}");
            assert_eq!(store.tree_get(*tree, reg.roots()[i], &rcnt).unwrap().is_some(), i == 0, "rCNT in copy {i}");
        }
        roots = reg.roots();
        db.commit().unwrap();
    }
    let mut db = Database::open(&path, cfg()).unwrap();
    let store = db.writer().unwrap();
    let reg = Register::open(roots);
    assert_eq!(reg.get(store.store(), &key(Node::C, b"COLM", 7)).unwrap(), Some((1, b"column seven".to_vec())));
    assert_eq!(reg.get(store.store(), &key(Node::G, b"rCNT", 0)).unwrap(), Some((1, b"42 rows".to_vec())));
    assert_eq!(reg.get(store.store(), &key(Node::C, b"COLM", 8)).unwrap(), None);
}

#[test]
fn a_damaged_copy_loses_and_a_disagreement_behind_it_is_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = legacy_create(dir.path().join("db"), cfg()).unwrap();
    let store = db.writer().unwrap();
    let mut reg = Register::create(store).unwrap();
    let k = key(Node::C, b"COLM", 7);
    reg.put(store, &k, 1, b"good").unwrap();
    let kb = k.encode().unwrap();
    let mut roots = reg.roots();
    // Copy 1 damaged: the other two win.
    roots[1] = store.tree_put(REGISTER_TREES[1], roots[1], &kb, b"garbage").unwrap();
    let reg = Register::open(roots);
    assert_eq!(reg.get(store.store(), &k).unwrap(), Some((1, b"good".to_vec())));
    // Copy 0 damaged: copies 1 and 2 answer, and must agree. Copy 1 is
    // damaged too, so copy 2 answers alone.
    roots[0] = store.tree_put(REGISTER_TREES[0], roots[0], &kb, b"garbage").unwrap();
    let reg = Register::open(roots);
    assert_eq!(reg.get(store.store(), &k).unwrap(), Some((1, b"good".to_vec())));
    // Copies 1 and 2 intact but different, with copy 0 damaged: corruption.
    let other = encode_value(&kb, 1, b"other").unwrap();
    roots[1] = store.tree_put(REGISTER_TREES[1], roots[1], &kb, &other).unwrap();
    let reg = Register::open(roots);
    assert!(matches!(reg.get(store.store(), &k), Err(Error::Corrupt(_))));
    // Every copy damaged: corruption.
    for i in 0..3 {
        roots[i] = store.tree_put(REGISTER_TREES[i], roots[i], &kb, b"garbage").unwrap();
    }
    let reg = Register::open(roots);
    assert!(matches!(reg.get(store.store(), &k), Err(Error::Corrupt(_))));
}

#[test]
fn a_damaged_ignorable_entry_reads_as_missing() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = legacy_create(dir.path().join("db"), cfg()).unwrap();
    let store = db.writer().unwrap();
    let mut reg = Register::create(store).unwrap();
    let k = key(Node::G, b"rCNT", 0);
    reg.put(store, &k, 1, b"42").unwrap();
    let mut roots = reg.roots();
    roots[0] = store.tree_put(REGISTER_TREES[0], roots[0], &k.encode().unwrap(), b"garbage").unwrap();
    assert_eq!(Register::open(roots).get(store.store(), &k).unwrap(), None);
}

#[test]
fn delete_removes_every_copy_and_a_node_lists_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = legacy_create(dir.path().join("db"), cfg()).unwrap();
    let store = db.writer().unwrap();
    let mut reg = Register::create(store).unwrap();
    for id in [10u64, 2, 7] {
        reg.put(store, &key(Node::C, b"COLM", id), 1, format!("c{id}").as_bytes()).unwrap();
    }
    reg.put(store, &key(Node::B, b"NEXT", 1), 1, b"n").unwrap();
    reg.put(store, &key(Node::D, b"INDX", 1), 1, b"i").unwrap();
    let listed: Vec<u64> = reg
        .scan_node(store.store(), Node::C)
        .unwrap()
        .into_iter()
        .map(|(k, _, _)| match k.item { Item::Id(id) => id, other => panic!("{other:?}") })
        .collect();
    assert_eq!(listed, [2, 7, 10], "node C only, in key order");
    assert!(reg.delete(store, &key(Node::C, b"COLM", 7)).unwrap());
    let kb = key(Node::C, b"COLM", 7).encode().unwrap();
    for (i, tree) in REGISTER_TREES.iter().enumerate() {
        assert!(store.tree_get(*tree, reg.roots()[i], &kb).unwrap().is_none(), "copy {i}");
    }
    assert!(!reg.delete(store, &key(Node::C, b"COLM", 7)).unwrap());
}

/// These tests build the carrier by hand on a 0.18-format file, whose
/// header keys and tree ids are free.
fn legacy_create(path: impl AsRef<std::path::Path>, config: Config) -> crate::collections::Result<Database> {
    super::header::FORCE.with(|f| f.set(Some(false)));
    Database::create(path, config)
}
