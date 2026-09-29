//! The Register verifier. What is at risk, one test each:
//!
//! * a clean Register verifies and counts its entries by class
//!   (`a_clean_register_verifies`);
//! * a damaged copy, a missing critical copy, copies that disagree, an
//!   ignorable entry outside copy 0 and an entry the census does not list are
//!   each named as corruption (`each_kind_of_damage_is_named`).

use super::carrier::*;
use super::register::Register;
use super::verify::{verify, Verified};
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

fn census() -> Vec<CensusLine> {
    let mut c = vec![
        CensusLine { kind: Kind::new(b"COLM").unwrap(), version: 1, variant: 0 },
        CensusLine { kind: Kind::new(b"rCNT").unwrap(), version: 1, variant: 0 },
    ];
    c.sort();
    c
}

fn filled(store: &mut crate::store::Backend) -> Register {
    let mut reg = Register::create(store).unwrap();
    for id in 1..=3 {
        reg.put(store, &key(Node::C, b"COLM", id), 1, b"col").unwrap();
    }
    reg.put(store, &key(Node::G, b"rCNT", 0), 1, b"42").unwrap();
    reg
}

#[test]
fn a_clean_register_verifies() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("db"), cfg()).unwrap();
    let store = db.writer().unwrap();
    let reg = filled(store);
    let anchor = Anchor { roots: reg.roots(), census: census() };
    assert_eq!(verify(store, &reg, &anchor).unwrap(), Verified { critical: 3, ignorable: 1 });
}

#[test]
fn each_kind_of_damage_is_named() {
    let cases: [(&str, fn(&mut crate::store::Backend, &mut [u32; 3])); 5] = [
        ("damaged copy", |s, r| {
            let k = key(Node::C, b"COLM", 2).encode().unwrap();
            r[1] = s.tree_put(REGISTER_TREES[1], r[1], &k, b"garbage").unwrap();
        }),
        ("missing copy", |s, r| {
            let k = key(Node::C, b"COLM", 2).encode().unwrap();
            r[2] = s.tree_delete(REGISTER_TREES[2], r[2], &k).unwrap().1;
        }),
        ("disagree", |s, r| {
            let k = key(Node::C, b"COLM", 2).encode().unwrap();
            let v = encode_value(&k, 1, b"other").unwrap();
            r[1] = s.tree_put(REGISTER_TREES[1], r[1], &k, &v).unwrap();
        }),
        ("ignorable outside copy 0", |s, r| {
            let k = key(Node::G, b"rCNT", 0).encode().unwrap();
            let v = encode_value(&k, 1, b"42").unwrap();
            r[1] = s.tree_put(REGISTER_TREES[1], r[1], &k, &v).unwrap();
        }),
        ("not in the census", |s, r| {
            let k = key(Node::C, b"COLM", 9).encode().unwrap();
            let v = encode_value(&k, 2, b"v2").unwrap();
            for i in 0..3 {
                r[i] = s.tree_put(REGISTER_TREES[i], r[i], &k, &v).unwrap();
            }
        }),
    ];
    for (what, damage) in cases {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Database::create(dir.path().join("db"), cfg()).unwrap();
        let store = db.writer().unwrap();
        let reg = filled(store);
        let mut roots = reg.roots();
        damage(store, &mut roots);
        let reg = Register::open(roots);
        let anchor = Anchor { roots, census: census() };
        match verify(store, &reg, &anchor) {
            Err(Error::Corrupt(m)) => assert!(!m.is_empty(), "{what}"),
            other => panic!("{what}: {other:?}"),
        }
    }
}
