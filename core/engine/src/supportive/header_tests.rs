//! Build step 3a: the header of a Register file (`docs/core/SUPPORTIVE.md`
//! 2.i). What is at risk, one test each:
//!
//! * the legacy feature map loses nothing: every bit this build implements
//!   has exactly one census line, and the word round-trips
//!   (`every_feature_bit_has_one_census_line`);
//! * a Register file keeps its counters, features and index count across
//!   reopen, for the writer and a snapshot, and holds no 0.18 header
//!   (`a_register_file_keeps_its_header_across_reopen`);
//! * the resource policy survives as `LIMT` (`the_policy_lives_in_limt`);
//! * a transaction rolled back leaves the Register as the last commit wrote it
//!   (`rollback_rereads_the_register`).

use super::anchor::ANCHOR_MAGIC;
use super::header::*;
use crate::collections::{Database, SUPPORTED_LOGICAL_FEATURES};
use crate::Kind;
use kernel::{
    io::IoMode,
    limits::ResourceLimits,
    store::{Config, SyncMode},
};
use serde_json::json;

fn cfg() -> Config {
    Config { budget_bytes: 1 << 20, io: IoMode::Buffered, sync: SyncMode::Full }
}
fn on() {
    FORCE.with(|f| f.set(Some(true)));
}

#[test]
fn every_feature_bit_has_one_census_line() {
    let bits = LEGACY.iter().fold(1, |acc, (bit, ..)| {
        assert_eq!(acc & bit, 0, "bit {bit:#x} listed twice");
        acc | bit
    });
    assert_eq!(bits, SUPPORTED_LOGICAL_FEATURES);
    let lines = census_of(u64::MAX);
    let mut unique = lines.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), LEGACY.len(), "two bits share a census line");
    for (bit, ..) in LEGACY {
        assert_eq!(features_of(&census_of(bit)), bit);
    }
    assert_eq!(features_of(&lines) | 1, SUPPORTED_LOGICAL_FEATURES);
}

#[test]
fn a_register_file_keeps_its_header_across_reopen() {
    on();
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let raw = db.store().unwrap().get(&[0, 0, 0]).unwrap().unwrap();
    assert!(raw.starts_with(ANCHOR_MAGIC), "a Register file carries the Anchor");
    let c = db
        .create_collection("c", vec![("n".into(), Kind::Int)], Default::default())
        .unwrap();
    db.create_scalar_index(c, "by_n", "n", false).unwrap();
    db.put(c, "a", &json!({"n": 1})).unwrap();
    db.commit().unwrap();
    let header = db.header().unwrap();
    let index = db.index_header;
    assert!(index.is_some_and(|h| h.count == 1));
    drop(db);
    let db = Database::open(&path, cfg()).unwrap();
    assert_eq!(db.header().unwrap(), header);
    assert_eq!(db.index_header, index);
    assert!(db.supportive.is_some());
    let snap = Database::open_snapshot(&path, cfg()).unwrap();
    assert_eq!(snap.header().unwrap(), header);
    assert_eq!(snap.index_header, index);
    assert_eq!(snap.get(c, "a").unwrap().unwrap().document, json!({"n": 1}));
}

#[test]
fn the_policy_lives_in_limt() {
    on();
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let limits = ResourceLimits {
        data_bytes: 256 << 10,
        wal_bytes: 64 << 10,
        tracked_pages: 1024,
        readers: 4,
        record_bytes: 16 << 10,
        recovery_bytes: 64 << 10,
    };
    let db = Database::create_limited(&path, cfg(), limits).unwrap();
    let want = db.limits();
    assert!(want.is_some());
    drop(db);
    assert_eq!(Database::open(&path, cfg()).unwrap().limits(), want);
}

#[test]
fn rollback_rereads_the_register() {
    on();
    let t = tempfile::tempdir().unwrap();
    let mut db = Database::create(t.path().join("db"), cfg()).unwrap();
    let before = (db.header().unwrap(), db.index_header);
    db.create_collection("c", vec![("n".into(), Kind::Int)], Default::default())
        .unwrap();
    assert_ne!(db.header().unwrap(), before.0);
    db.rollback().unwrap();
    assert_eq!((db.header().unwrap(), db.index_header), before);
}
