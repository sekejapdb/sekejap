//! The LIVE ROW COUNT (tag `0x08`, feature bit `0x2000`).
//!
//! The ORACLE is a `BTreeMap` of live external keys held in the test process.
//! Every assertion compares the engine's record against that map's `len()` --
//! never against another engine call, and never against a walk the engine
//! chose. Where a walk is the oracle (the backfill) it is written here, over
//! `Database::scan`, so the two counts come from two different places.
use sekejap_core::{
    collections::{
        verification::{verify_indexed_source, VerificationLimits},
        CandidateDriver, CollectionId, CollectionOptions, Database, DeleteMode, Error, IndexId,
        QueryBudget, QueryFilter, ScalarFilter, ScalarValue, UpdatePatch, WriteAction,
        WriteCursor, WriteRequest, ROW_COUNT_FEATURE,
    },
    pagewal::PageWalStore,
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;
use std::{collections::BTreeMap, path::Path};

const ROW_COUNT_TAG: u8 = 0x08;

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn fields() -> Vec<(String, Kind)> {
    vec![("n".into(), Kind::Int), ("tag".into(), Kind::Text)]
}

fn open(path: &Path) -> (Database, CollectionId) {
    let mut db = Database::create(path, cfg()).unwrap();
    let c = db
        .create_collection("thing", fields(), CollectionOptions::default())
        .unwrap();
    db.commit().unwrap();
    (db, c)
}

/// The same database with a scalar index on each column, so the predicated
/// write passes have something to drive on.
fn open_indexed(path: &Path) -> (Database, CollectionId, IndexId, IndexId) {
    let (mut db, c) = open(path);
    let n = db.create_scalar_index(c, "ix_n", "n", false).unwrap();
    let tag = db.create_scalar_index(c, "ix_tag", "tag", false).unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(n, 256).unwrap();
    db.build_index_to_ready(tag, 256).unwrap();
    db.commit().unwrap();
    (db, c, n, tag)
}

fn eq_text<'a>(index: IndexId, value: &'a str) -> QueryFilter<'a> {
    QueryFilter::Scalar {
        index,
        predicate: ScalarFilter::Eq(ScalarValue::Text(value)),
    }
}

fn int_range(index: IndexId, lower: std::ops::Bound<i64>, upper: std::ops::Bound<i64>) -> QueryFilter<'static> {
    let map = |b: std::ops::Bound<i64>| match b {
        std::ops::Bound::Included(v) => std::ops::Bound::Included(ScalarValue::I64(v)),
        std::ops::Bound::Excluded(v) => std::ops::Bound::Excluded(ScalarValue::I64(v)),
        std::ops::Bound::Unbounded => std::ops::Bound::Unbounded,
    };
    QueryFilter::Scalar {
        index,
        predicate: ScalarFilter::Range {
            lower: map(lower),
            upper: map(upper),
        },
    }
}

/// The test's own oracle: the live external keys, and what each row holds.
#[derive(Default)]
struct Oracle(BTreeMap<String, i64>);

impl Oracle {
    fn put(&mut self, key: &str, n: i64) {
        self.0.insert(key.to_owned(), n);
    }
    fn delete(&mut self, key: &str) {
        self.0.remove(key);
    }
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
}

/// The engine's record, read from the FILE rather than from the handle, so
/// the assertion is about what a crash would leave behind.
fn record_on_disk(path: &Path, c: CollectionId) -> Option<u64> {
    let raw = PageWalStore::open_snapshot(path, 1 << 20).unwrap();
    let mut key = vec![ROW_COUNT_TAG];
    let b = u64::from(c.0).to_be_bytes();
    let start = b.iter().position(|x| *x != 0).unwrap_or(7);
    key.push(0x80 + (8 - start) as u8);
    key.extend_from_slice(&b[start..]);
    let bytes = raw.get(&key).unwrap()?;
    assert_eq!(bytes.len(), 16, "the record is 16 fixed bytes");
    Some(u64::from_be_bytes(bytes[..8].try_into().unwrap()))
}

fn logical_features(path: &Path) -> u64 {
    let raw = PageWalStore::open_snapshot(path, 1 << 20).unwrap();
    let header = raw.get(&[0, 0, 0]).unwrap().unwrap();
    assert_eq!(&header[..8], b"E4COLL2\0");
    u64::from_be_bytes(header[18..26].try_into().unwrap())
}

/// Every key of the live-row-count keyspace in the file.
fn count_keyspace(path: &Path) -> Vec<Vec<u8>> {
    let raw = PageWalStore::open_snapshot(path, 1 << 20).unwrap();
    let mut out = Vec::new();
    for row in raw.range(&[ROW_COUNT_TAG]).unwrap() {
        let (key, _) = row.unwrap();
        if key.first() != Some(&ROW_COUNT_TAG) {
            break;
        }
        out.push(key);
    }
    out
}

/// The test's own walk of a collection, which is the oracle the backfill and
/// the `count(*)` answer are both held to.
fn walk(db: &Database, c: CollectionId) -> u64 {
    db.scan(c, None).unwrap().map(|row| row.unwrap()).count() as u64
}

// ── the oracle run ────────────────────────────────────────────────────────

/// Put, replace, delete, `DELETE ... WHERE`, `UPDATE ... WHERE` and a drop,
/// each against a `BTreeMap` of live keys held here. After every commit the
/// record on disk must equal the map's length, and so must the walk.
#[test]
fn the_record_equals_a_btree_map_of_live_keys_through_every_write_shape() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, c, ix_n, ix_tag) = open_indexed(&path);
    let mut oracle = Oracle::default();

    // A fresh collection holds no rows and says so.
    assert_eq!(record_on_disk(&path, c), Some(0));
    assert_eq!(db.row_count(c).unwrap(), Some(0));

    // Inserts.
    for i in 0..40i64 {
        let key = format!("k{i:03}");
        db.put(c, &key, &json!({"n": i, "tag": if i % 3 == 0 { "x" } else { "y" }}))
            .unwrap();
        oracle.put(&key, i);
    }
    db.commit().unwrap();
    assert_eq!(record_on_disk(&path, c), Some(oracle.len()));
    assert_eq!(walk(&db, c), oracle.len());

    // REPLACES change no count: the same key, a new document.
    for i in 0..10i64 {
        let key = format!("k{i:03}");
        db.put(c, &key, &json!({"n": i + 1000, "tag": "z"})).unwrap();
        oracle.put(&key, i + 1000);
    }
    db.commit().unwrap();
    assert_eq!(record_on_disk(&path, c), Some(oracle.len()));

    // An `update` is a replace too.
    db.update(c, "k000", &json!({"n": 7})).unwrap();
    oracle.put("k000", 7);
    db.commit().unwrap();
    assert_eq!(record_on_disk(&path, c), Some(oracle.len()));

    // Point deletes.
    for i in 30..40i64 {
        let key = format!("k{i:03}");
        assert!(db.delete(c, &key).unwrap());
        oracle.delete(&key);
    }
    db.commit().unwrap();
    assert_eq!(record_on_disk(&path, c), Some(oracle.len()));
    assert_eq!(walk(&db, c), oracle.len());

    // A delete of a key that is not there changes nothing.
    assert!(!db.delete(c, "k999").unwrap());
    db.commit().unwrap();
    assert_eq!(record_on_disk(&path, c), Some(oracle.len()));

    // `UPDATE ... WHERE` -- a predicated replace, so the count does not move.
    // The pass is driven explicitly by the index the patch does NOT move.
    let mut patch = UpdatePatch::new();
    patch.set("n", json!(5)).unwrap();
    let progress = db
        .write_where(
            WriteRequest {
                collection: c,
                filters: &[eq_text(ix_tag, "y")],
                action: WriteAction::Update(&patch),
                driver: CandidateDriver::Entities,
                after: WriteCursor::start(),
            },
            QueryBudget::unlimited(),
        )
        .unwrap();
    assert!(progress.rows_written > 0, "the pass wrote nothing to test");
    db.commit().unwrap();
    assert_eq!(record_on_disk(&path, c), Some(oracle.len()));

    // `DELETE ... WHERE` -- the same candidates, removed.
    let doomed: Vec<String> = oracle
        .0
        .iter()
        .filter(|(_, n)| **n >= 1000)
        .map(|(k, _)| k.clone())
        .collect();
    assert!(!doomed.is_empty());
    let removed = db
        .delete_where(
            c,
            &[int_range(
                ix_n,
                std::ops::Bound::Included(1000),
                std::ops::Bound::Unbounded,
            )],
            QueryBudget::unlimited(),
        )
        .unwrap();
    assert_eq!(removed.rows_written as usize, doomed.len());
    for key in &doomed {
        oracle.delete(key);
    }
    db.commit().unwrap();
    assert_eq!(record_on_disk(&path, c), Some(oracle.len()));
    assert_eq!(walk(&db, c), oracle.len());

    // And the whole file agrees with an independent walk.
    drop(db);
    let report = verify_indexed_source(&path, VerificationLimits::default(), |issue| {
        panic!("verification issue: {issue:?}");
    })
    .unwrap();
    assert!(report.complete && report.clean);
}

/// Each collection carries its own record, and a DROP takes that collection's
/// record with it and touches no other.
#[test]
fn each_collection_owns_one_record_and_a_drop_removes_exactly_that_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, first) = open(&path);
    let other = db
        .create_collection("other", fields(), CollectionOptions::default())
        .unwrap();
    db.commit().unwrap();
    assert_eq!(record_on_disk(&path, first), Some(0));
    assert_eq!(record_on_disk(&path, other), Some(0));
    assert_eq!(count_keyspace(&path).len(), 2);

    for i in 0..11i64 {
        db.put(first, &format!("f{i}"), &json!({"n": i, "tag": "f"}))
            .unwrap();
    }
    for i in 0..5i64 {
        db.put(other, &format!("o{i}"), &json!({"n": i, "tag": "o"}))
            .unwrap();
    }
    db.commit().unwrap();
    assert_eq!(record_on_disk(&path, first), Some(11));
    assert_eq!(record_on_disk(&path, other), Some(5));

    db.begin_drop_collection(other).unwrap();
    db.drop_collection_to_end(other, 64).unwrap();
    assert_eq!(record_on_disk(&path, other), None);
    assert_eq!(record_on_disk(&path, first), Some(11));
    assert_eq!(
        count_keyspace(&path).len(),
        1,
        "the dropped collection left a key behind"
    );
    assert_eq!(walk(&db, first), 11);
}

/// The delta is accumulated in memory and written once per commit; a rollback
/// discards it with the rows it was counting.
#[test]
fn a_rollback_discards_the_delta_with_the_rows_it_was_counting() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, c) = open(&path);
    for i in 0..10 {
        db.put(c, &format!("k{i}"), &json!({"n": i, "tag": "a"}))
            .unwrap();
    }
    db.commit().unwrap();
    assert_eq!(record_on_disk(&path, c), Some(10));

    for i in 10..25 {
        db.put(c, &format!("k{i}"), &json!({"n": i, "tag": "a"}))
            .unwrap();
    }
    assert!(db.delete(c, "k0").unwrap());
    // Uncommitted, the handle sees its own arithmetic.
    assert_eq!(db.row_count(c).unwrap(), Some(24));
    // Nothing of it is on disk.
    assert_eq!(record_on_disk(&path, c), Some(10));

    db.rollback().unwrap();
    assert_eq!(db.row_count(c).unwrap(), Some(10));
    assert_eq!(record_on_disk(&path, c), Some(10));
    assert_eq!(walk(&db, c), 10);
}

/// A crash is a handle dropped with an open transaction. The record and the
/// rows it counts ride the same transaction, so what survives is the last
/// COMMITTED pair and never a count that is off by one.
#[test]
fn a_handle_dropped_without_a_commit_leaves_the_count_the_last_commit_left() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, c) = open(&path);
    let mut oracle = Oracle::default();
    for i in 0..30 {
        let key = format!("k{i:02}");
        db.put(c, &key, &json!({"n": i, "tag": "a"})).unwrap();
        oracle.put(&key, i);
    }
    db.commit().unwrap();

    // Uncommitted work of both signs, then the handle simply goes away.
    for i in 30..60 {
        db.put(c, &format!("k{i:02}"), &json!({"n": i, "tag": "a"}))
            .unwrap();
    }
    for i in 0..10 {
        assert!(db.delete(c, &format!("k{i:02}")).unwrap());
    }
    drop(db);

    let db = Database::open(&path, cfg()).unwrap();
    assert_eq!(record_on_disk(&path, c), Some(oracle.len()));
    assert_eq!(db.row_count(c).unwrap(), Some(oracle.len()));
    assert_eq!(walk(&db, c), oracle.len(), "the rows and the record agree");
}

/// A reopen reads the record; nothing is recomputed at open.
#[test]
fn a_reopen_reads_the_record_the_last_commit_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, c) = open(&path);
    for i in 0..17 {
        db.put(c, &format!("k{i}"), &json!({"n": i, "tag": "a"}))
            .unwrap();
    }
    db.commit().unwrap();
    drop(db);
    let db = Database::open(&path, cfg()).unwrap();
    assert_eq!(db.row_count(c).unwrap(), Some(17));
    let snapshot = Database::open_snapshot(&path, cfg()).unwrap();
    assert_eq!(snapshot.row_count(c).unwrap(), Some(17));
}

// ── a database written before the bit ─────────────────────────────────────

/// Turn a database this build wrote into one a binary that predates the
/// feature would have written: the records removed and the bit cleared.
fn strip_row_counts(path: &Path) {
    let mut raw = PageWalStore::open(path, false, 1 << 20).unwrap();
    let keys: Vec<Vec<u8>> = {
        let mut out = Vec::new();
        for row in raw.range(&[ROW_COUNT_TAG]).unwrap() {
            let (key, _) = row.unwrap();
            if key.first() != Some(&ROW_COUNT_TAG) {
                break;
            }
            out.push(key);
        }
        out
    };
    for key in keys {
        raw.delete(&key).unwrap();
    }
    for copy in 0..3u8 {
        let key = [0, 0, copy];
        let mut header = raw.get(&key).unwrap().unwrap();
        let features = u64::from_be_bytes(header[18..26].try_into().unwrap());
        header[18..26].copy_from_slice(&(features & !ROW_COUNT_FEATURE).to_be_bytes());
        let end = header.len() - 4;
        let checksum = crc32c::crc32c(&header[..end]).to_le_bytes();
        header[end..].copy_from_slice(&checksum);
        raw.put(&key, &header).unwrap();
    }
    raw.commit().unwrap();
    raw.checkpoint().unwrap();
}

/// A database whose bit is CLEAR keeps no records, so an ordinary write pays
/// nothing and leaves nothing: the keyspace stays empty and the declared
/// feature word does not move.
#[test]
fn a_database_whose_bit_is_clear_gains_no_key_and_no_feature_from_a_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, c) = open(&path);
    for i in 0..20 {
        db.put(c, &format!("k{i:02}"), &json!({"n": i, "tag": "a"}))
            .unwrap();
    }
    db.commit().unwrap();
    db.checkpoint().unwrap();
    drop(db);
    strip_row_counts(&path);
    let before = logical_features(&path);
    assert_eq!(before & ROW_COUNT_FEATURE, 0);
    assert!(count_keyspace(&path).is_empty());

    let mut db = Database::open(&path, cfg()).unwrap();
    assert_eq!(db.row_count(c).unwrap(), None, "no record, so no answer");
    db.put(c, "k100", &json!({"n": 100, "tag": "a"})).unwrap();
    db.put(c, "k00", &json!({"n": 0, "tag": "b"})).unwrap();
    assert!(db.delete(c, "k01").unwrap());
    db.write_where(
        WriteRequest {
            collection: c,
            filters: &[],
            action: WriteAction::Delete(DeleteMode::Restrict),
            driver: CandidateDriver::Entities,
            after: WriteCursor::start(),
        },
        QueryBudget {
            rows_written: 3,
            ..QueryBudget::unlimited()
        },
    )
    .unwrap();
    db.commit().unwrap();
    drop(db);

    assert_eq!(logical_features(&path), before, "a write set the bit");
    assert!(
        count_keyspace(&path).is_empty(),
        "a write wrote into a keyspace the file does not declare"
    );
    let report = verify_indexed_source(&path, VerificationLimits::default(), |issue| {
        panic!("verification issue on a bit-clear database: {issue:?}");
    })
    .unwrap();
    assert!(report.complete && report.clean);
}

/// The backfill is the bounded, resumable atomic that gives such a database
/// its records. The oracle is this file's own walk.
#[test]
fn a_backfill_on_a_bit_clear_database_equals_the_walk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, c) = open(&path);
    let second = db
        .create_collection("second", fields(), CollectionOptions::default())
        .unwrap();
    db.commit().unwrap();
    for i in 0..250 {
        db.put(c, &format!("k{i:03}"), &json!({"n": i, "tag": "a"}))
            .unwrap();
    }
    for i in 0..37 {
        db.put(second, &format!("s{i:03}"), &json!({"n": i, "tag": "b"}))
            .unwrap();
    }
    db.commit().unwrap();
    // Some deletes, so the count is not simply the allocator.
    for i in 0..40 {
        assert!(db.delete(c, &format!("k{i:03}")).unwrap());
    }
    db.commit().unwrap();
    db.checkpoint().unwrap();
    drop(db);
    strip_row_counts(&path);

    let mut db = Database::open(&path, cfg()).unwrap();
    let expected_first = walk(&db, c);
    let expected_second = walk(&db, second);
    assert_eq!(expected_first, 210);
    assert_eq!(expected_second, 37);

    // Bounded: no call walks more than its budget, and the work is resumed.
    let mut calls = 0;
    let mut walked = 0u64;
    loop {
        let progress = db.backfill_row_counts(32).unwrap();
        db.commit().unwrap();
        walked += progress.rows_walked;
        assert!(progress.rows_walked <= 32, "the budget was exceeded");
        calls += 1;
        assert!(calls < 100, "the backfill did not terminate");
        if progress.done {
            break;
        }
    }
    assert!(calls > 4, "the budget did not actually bound the walk");
    assert_eq!(walked, expected_first + expected_second);
    assert_eq!(db.row_count(c).unwrap(), Some(expected_first));
    assert_eq!(db.row_count(second).unwrap(), Some(expected_second));
    assert_ne!(logical_features(&path) & ROW_COUNT_FEATURE, 0);

    // A second backfill has nothing to do.
    let again = db.backfill_row_counts(32).unwrap();
    assert!(again.done && again.rows_walked == 0 && again.records_written == 0);

    // From here the write path maintains what the backfill built.
    db.put(c, "fresh", &json!({"n": 1, "tag": "a"})).unwrap();
    assert!(db.delete(c, "k100").unwrap());
    assert!(db.delete(c, "k101").unwrap());
    db.commit().unwrap();
    assert_eq!(db.row_count(c).unwrap(), Some(expected_first - 1));
    assert_eq!(walk(&db, c), expected_first - 1);
    drop(db);

    let report = verify_indexed_source(&path, VerificationLimits::default(), |issue| {
        panic!("verification issue after a backfill: {issue:?}");
    })
    .unwrap();
    assert!(report.complete && report.clean);
}

/// Writes INTERLEAVED with a half-finished backfill still land on the walk's
/// number: an insert lands above the cursor and the resumed walk counts it,
/// and a delete behind the cursor is taken off the running total.
#[test]
fn a_write_during_a_half_finished_backfill_still_lands_on_the_walk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, c) = open(&path);
    for i in 0..200 {
        db.put(c, &format!("k{i:03}"), &json!({"n": i, "tag": "a"}))
            .unwrap();
    }
    db.commit().unwrap();
    db.checkpoint().unwrap();
    drop(db);
    strip_row_counts(&path);

    let mut db = Database::open(&path, cfg()).unwrap();
    // Half the collection.
    let progress = db.backfill_row_counts(100).unwrap();
    db.commit().unwrap();
    assert!(!progress.done && progress.records_written == 0);

    // A delete BEHIND the cursor and an insert AHEAD of every row.
    assert!(db.delete(c, "k000").unwrap());
    assert!(db.delete(c, "k001").unwrap());
    db.put(c, "late", &json!({"n": 1, "tag": "a"})).unwrap();
    db.commit().unwrap();

    loop {
        let progress = db.backfill_row_counts(100).unwrap();
        db.commit().unwrap();
        if progress.done {
            break;
        }
    }
    let expected = walk(&db, c);
    assert_eq!(expected, 199);
    assert_eq!(db.row_count(c).unwrap(), Some(expected));
    drop(db);
    let report = verify_indexed_source(&path, VerificationLimits::default(), |issue| {
        panic!("verification issue: {issue:?}");
    })
    .unwrap();
    assert!(report.complete && report.clean);
}

// ── Law 8 and Law 5 ───────────────────────────────────────────────────────

/// Law 8. A file that declares the bit is refused WHOLE, as `Unsupported`, by
/// a binary that does not implement the keyspace. The probe here is the
/// `0x8000` bit, which no build implements yet,
/// so the refusal is this build's own production decision and not a
/// re-implementation of it. `collections::tests::
/// a_row_count_file_is_unsupported_to_a_binary_that_predates_the_bit` runs
/// the same rule with `0x2000` itself taken out of the mask.
#[test]
fn a_file_declaring_a_bit_this_binary_does_not_implement_is_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, c) = open(&path);
    db.put(c, "k", &json!({"n": 1, "tag": "a"})).unwrap();
    db.commit().unwrap();
    db.checkpoint().unwrap();
    drop(db);
    assert_ne!(logical_features(&path) & ROW_COUNT_FEATURE, 0);

    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    for copy in 0..3u8 {
        let key = [0, 0, copy];
        let mut header = raw.get(&key).unwrap().unwrap();
        let features = u64::from_be_bytes(header[18..26].try_into().unwrap());
        header[18..26].copy_from_slice(&(features | 0x8000).to_be_bytes());
        let end = header.len() - 4;
        let checksum = crc32c::crc32c(&header[..end]).to_le_bytes();
        header[end..].copy_from_slice(&checksum);
        raw.put(&key, &header).unwrap();
    }
    raw.commit().unwrap();
    drop(raw);

    assert!(
        matches!(Database::open(&path, cfg()), Err(Error::Unsupported(_))),
        "an unimplemented bit must be Unsupported, never Corrupt"
    );
}

/// Law 5. Verification compares every record against an INDEPENDENT walk, so
/// a record someone edited is a named finding rather than a silent answer.
#[test]
fn verification_names_a_tampered_record_against_its_own_walk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, c) = open(&path);
    for i in 0..12 {
        db.put(c, &format!("k{i:02}"), &json!({"n": i, "tag": "a"}))
            .unwrap();
    }
    db.commit().unwrap();
    db.checkpoint().unwrap();
    drop(db);
    assert_eq!(record_on_disk(&path, c), Some(12));

    let key = count_keyspace(&path).remove(0);
    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    let mut bytes = raw.get(&key).unwrap().unwrap();
    bytes[..8].copy_from_slice(&999u64.to_be_bytes());
    raw.put(&key, &bytes).unwrap();
    raw.commit().unwrap();
    drop(raw);

    let mut issues = Vec::new();
    let report = verify_indexed_source(&path, VerificationLimits::default(), |issue| {
        issues.push(issue.message.clone());
    })
    .unwrap();
    assert!(report.complete, "verification must finish and report");
    assert!(!report.clean, "a tampered record passed verification");
    assert!(
        issues
            .iter()
            .any(|m| m.contains("live row count record says 999") && m.contains("found 12")),
        "the finding does not name the two numbers: {issues:?}"
    );

    // And a record for a collection the catalog does not hold is its own
    // finding.
    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    raw.put(&bytes_key(97), &[0u8; 16]).unwrap();
    raw.commit().unwrap();
    drop(raw);
    let mut issues = Vec::new();
    verify_indexed_source(&path, VerificationLimits::default(), |issue| {
        issues.push(issue.message.clone());
    })
    .unwrap();
    assert!(
        issues
            .iter()
            .any(|m| m.contains("collection the catalog does not hold")),
        "{issues:?}"
    );
}

fn bytes_key(id: u64) -> Vec<u8> {
    let mut key = vec![ROW_COUNT_TAG];
    let b = id.to_be_bytes();
    let start = b.iter().position(|x| *x != 0).unwrap_or(7);
    key.push(0x80 + (8 - start) as u8);
    key.extend_from_slice(&b[start..]);
    key
}

/// A CASCADE write pass removes rows through the same call the point delete
/// does, so it is counted by the same arithmetic.
#[test]
fn a_cascade_write_pass_moves_the_count_like_every_other_delete() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, c) = open(&path);
    let mut oracle = Oracle::default();
    for i in 0..50 {
        let key = format!("k{i:02}");
        db.put(c, &key, &json!({"n": i, "tag": "a"})).unwrap();
        oracle.put(&key, i);
    }
    db.commit().unwrap();

    let progress = db
        .write_where(
            WriteRequest {
                collection: c,
                filters: &[],
                action: WriteAction::Delete(DeleteMode::Cascade),
                driver: CandidateDriver::Entities,
                after: WriteCursor::start(),
            },
            QueryBudget {
                rows_written: 20,
                ..QueryBudget::unlimited()
            },
        )
        .unwrap();
    assert_eq!(progress.rows_written, 20);
    for i in 0..20 {
        oracle.delete(&format!("k{i:02}"));
    }
    db.commit().unwrap();
    assert_eq!(record_on_disk(&path, c), Some(oracle.len()));
    assert_eq!(walk(&db, c), oracle.len());
}

// ── what `count(*)` costs, both ways ──────────────────────────────────────

fn count_all(db: &Database, c: CollectionId) -> (u64, sekejap_core::collections::QueryWork) {
    use sekejap_core::collections::{
        Accumulator, AggregateFn, AggregateRequest, AggValue, GroupOrder,
    };
    let accumulators = [Accumulator {
        function: AggregateFn::CountStar,
        input: None,
    }];
    let mut prepared = db
        .prepare_aggregate(AggregateRequest {
            collection: c,
            filters: &[],
            group: None,
            accumulators: &accumulators,
            having: &[],
            order: GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        })
        .unwrap();
    let page = prepared
        .next_page(64, QueryBudget::unlimited(), || false)
        .unwrap();
    assert!(page.done);
    assert_eq!(page.groups.len(), 1);
    let n = match &page.groups[0].values[0] {
        AggValue::Count(n) => *n,
        other => panic!("count(*) is a count, not {other:?}"),
    };
    (n, page.work)
}

/// `count(*)` with no filter and no group reads the record: one get, and
/// every counter a walk would charge stays at zero.
#[test]
fn a_count_star_reads_the_live_record_and_charges_no_walk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, c) = open(&path);
    let mut oracle = Oracle::default();
    for i in 0..137i64 {
        let key = format!("k{i:03}");
        db.put(c, &key, &json!({"n": i, "tag": "a"})).unwrap();
        oracle.put(&key, i);
    }
    for i in 0..17i64 {
        let key = format!("k{i:03}");
        assert!(db.delete(c, &key).unwrap());
        oracle.delete(&key);
    }
    db.commit().unwrap();

    let (n, work) = count_all(&db, c);
    assert_eq!(n, oracle.len());
    assert_eq!(work.primary_reads, 0);
    assert_eq!(work.row_decodes, 0);
    assert_eq!(work.key_postings, 0, "no enumeration happened");
    assert_eq!(work.scalar_postings, 0);
    assert_eq!(work.candidates, 0);
}

/// The same request on a database whose bit is CLEAR takes the walk it always
/// took: the complete enumeration of the external-key mapping keyspace, one
/// posting per row plus the peek that runs off its end.
#[test]
fn a_count_star_on_a_bit_clear_database_still_walks_the_mapping_keyspace() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, c) = open(&path);
    let mut oracle = Oracle::default();
    for i in 0..137i64 {
        let key = format!("k{i:03}");
        db.put(c, &key, &json!({"n": i, "tag": "a"})).unwrap();
        oracle.put(&key, i);
    }
    db.commit().unwrap();
    db.checkpoint().unwrap();
    drop(db);
    strip_row_counts(&path);

    let db = Database::open(&path, cfg()).unwrap();
    let (n, work) = count_all(&db, c);
    assert_eq!(n, oracle.len());
    assert_eq!(work.primary_reads, 0, "the walk reads no row either");
    assert_eq!(
        work.key_postings,
        oracle.len() + 1,
        "the mapping keyspace is the enumeration, plus the peek that runs off its end"
    );
}
