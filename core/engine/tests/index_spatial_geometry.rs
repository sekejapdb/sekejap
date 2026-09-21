//! Item X1a: the `SpatialGeometry` index family (index side only -- build,
//! maintain, verify, rebuild). Query filters over this family are a later
//! item (X1b) and are not exercised here.
//!
//! DO(2): build-vs-maintain posting identity, and an empty/invalid geometry
//! refused at write. DO(3): delete/update retires every old posting in the
//! same transaction as the row. DO(4): the verifier names an orphan and a
//! missing posting. DO(5): rebuild reproduces the build byte-for-byte.

use sekejap_core::{
    collections::{
        rebuild::{rebuild_derived_indexes, RebuildLimits},
        verification::{verify_indexed_source, IssueClass, VerificationLimits},
        CollectionId, CollectionOptions, Database,
    },
    pagewal::PageWalStore,
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::{json, Value};
use std::path::Path;

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

/// One posting fixture: a variety of geometry kinds, deliberately including
/// a Polygon, a MultiPolygon, a LineString, a Point, and a dateline-crossing
/// Polygon (whose bbox spans nearly the whole globe under this index's
/// planar-bbox design, and so is expected to fall to the world bucket).
fn fixture_geometries() -> Vec<(&'static str, Value)> {
    vec![
        (
            "pt",
            json!({"type": "Point", "coordinates": [12.34, -5.6]}),
        ),
        (
            "line",
            json!({"type": "LineString", "coordinates": [[0.0, 0.0], [1.0, 1.0], [2.0, 0.5]]}),
        ),
        (
            "poly",
            json!({"type": "Polygon", "coordinates": [[[0.0,0.0],[4.0,0.0],[4.0,4.0],[0.0,4.0],[0.0,0.0]]]}),
        ),
        (
            "multipoly",
            json!({"type": "MultiPolygon", "coordinates": [
                [[[0.0,0.0],[1.0,0.0],[1.0,1.0],[0.0,1.0],[0.0,0.0]]],
                [[[10.0,10.0],[11.0,10.0],[11.0,11.0],[10.0,11.0],[10.0,10.0]]]
            ]}),
        ),
        (
            "dateline",
            json!({"type": "Polygon", "coordinates": [[[179.0,-1.0],[-179.0,-1.0],[-179.0,1.0],[179.0,1.0],[179.0,-1.0]]]}),
        ),
    ]
}

fn create_geo_collection(db: &mut Database) -> CollectionId {
    db.create_collection(
        "shapes",
        vec![("shape".into(), Kind::Geo)],
        CollectionOptions::default(),
    )
    .unwrap()
}

/// Every `0x7c` (geometry) posting under `index` in the raw keyspace, key
/// and value, in key order -- independent of this crate's private decode
/// helpers, so it is a faithful witness of what is actually on disk.
fn raw_geometry_postings(path: &Path, index: u64) -> Vec<(Vec<u8>, Vec<u8>)> {
    let raw = PageWalStore::open(path, false, 1 << 20).unwrap();
    let mut prefix = vec![0x7cu8];
    let n = index.to_be_bytes();
    let start = n.iter().position(|b| *b != 0).unwrap_or(7);
    prefix.push(0x80 + (8 - start) as u8);
    prefix.extend(&n[start..]);
    let mut out = Vec::new();
    for row in raw.range(&prefix).unwrap() {
        let (k, v) = row.unwrap();
        if !k.starts_with(&prefix) {
            break;
        }
        out.push((k, v));
    }
    out
}

#[test]
fn build_vs_maintain_posting_identity_across_geometry_kinds() {
    let temp = tempfile::tempdir().unwrap();
    let built_path = temp.path().join("built");
    let maintained_path = temp.path().join("maintained");

    // Path A: rows first, index (and its build) after.
    let mut db = Database::create(&built_path, cfg()).unwrap();
    let collection = create_geo_collection(&mut db);
    for (key, shape) in fixture_geometries() {
        db.put(collection, key, &json!({"shape": shape})).unwrap();
    }
    db.commit().unwrap();
    let index = db
        .create_geometry_index(collection, "by_shape", "shape")
        .unwrap();
    db.commit().unwrap();
    assert!(db.build_index_to_ready(index, 16).unwrap() > 0);
    db.commit().unwrap();
    drop(db);

    // Path B: index first (empty), rows maintained into it as they arrive.
    let mut db = Database::create(&maintained_path, cfg()).unwrap();
    let collection = create_geo_collection(&mut db);
    let index2 = db
        .create_geometry_index(collection, "by_shape", "shape")
        .unwrap();
    assert!(db.build_index_step(index2, 8).unwrap());
    db.commit().unwrap();
    for (key, shape) in fixture_geometries() {
        db.put(collection, key, &json!({"shape": shape})).unwrap();
    }
    db.commit().unwrap();
    drop(db);

    assert_eq!(index.0, index2.0, "same index id in both databases");
    let built = raw_geometry_postings(&built_path, index.0);
    let maintained = raw_geometry_postings(&maintained_path, index2.0);
    assert!(!built.is_empty());
    assert_eq!(built, maintained, "build and maintain disagree on postings");
    // The stated bound: at most 8 (MAX_CELLS) postings per entity, and this
    // fixture's 5 entities together must not exceed 5*8.
    assert!(built.len() <= 5 * 8);
}

#[test]
fn an_empty_geometry_is_refused_at_write_not_stored() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let collection = create_geo_collection(&mut db);
    db.create_geometry_index(collection, "by_shape", "shape")
        .unwrap();
    db.commit().unwrap();

    for empty in [
        json!({"type": "LineString", "coordinates": []}),
        json!({"type": "Polygon", "coordinates": [[]]}),
        json!({"type": "MultiPoint", "coordinates": []}),
    ] {
        assert!(db.put(collection, "bad", &json!({"shape": empty})).is_err());
        db.rollback().unwrap();
        assert!(db.get(collection, "bad").unwrap().is_none(), "empty geometry was stored");
    }
    // A malformed (non-GeoJSON) shape under an indexed field is refused too.
    assert!(db
        .put(collection, "bad2", &json!({"shape": {"type": "Nonsense", "coordinates": []}}))
        .is_err());
    db.rollback().unwrap();
    assert!(db.get(collection, "bad2").unwrap().is_none());

    // The collection stayed usable: a valid geometry still commits cleanly.
    assert!(db.put(collection, "good", &json!({"shape": {"type":"Point","coordinates":[1.0,1.0]}})).is_ok());
    db.commit().unwrap();
}

#[test]
fn delete_and_update_retire_every_old_posting_in_the_same_transaction() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let collection = create_geo_collection(&mut db);
    let index = db
        .create_geometry_index(collection, "by_shape", "shape")
        .unwrap();
    assert!(db.build_index_step(index, 8).unwrap());
    db.commit().unwrap();

    // A polygon too big for the fine level (12 bits/axis, ~0.09deg cells)
    // but small enough to fit the coarse level (8 bits/axis) in more than
    // one cell -- so this exercises retiring SEVERAL postings, not just one.
    let big = json!({"type": "Polygon", "coordinates": [[[0.0,0.0],[2.0,0.0],[2.0,2.0],[0.0,2.0],[0.0,0.0]]]});
    db.put(collection, "big", &json!({"shape": big})).unwrap();
    db.commit().unwrap();
    drop(db);
    let after_insert = raw_geometry_postings(&path, index.0);
    assert!(
        after_insert.len() > 1 && after_insert.len() <= 8,
        "expected several (<=8) postings for a multi-cell polygon, got {}",
        after_insert.len()
    );

    // Update to a disjoint geometry: every old posting must be gone, and it
    // must be gone in the same transaction as the row (checked immediately
    // after commit, no checkpoint in between).
    let mut db = Database::open(&path, cfg()).unwrap();
    let moved = json!({"type": "Polygon", "coordinates": [[[50.0,50.0],[51.0,50.0],[51.0,51.0],[50.0,51.0],[50.0,50.0]]]});
    db.update(collection, "big", &json!({"shape": moved}))
        .unwrap();
    db.commit().unwrap();
    drop(db);
    let after_update = raw_geometry_postings(&path, index.0);
    assert!(!after_update.is_empty());
    for (old_key, _) in &after_insert {
        assert!(
            !after_update.iter().any(|(k, _)| k == old_key),
            "an old posting survived the update: {old_key:?}"
        );
    }

    // Delete: every posting for this entity is retired.
    let mut db = Database::open(&path, cfg()).unwrap();
    db.delete(collection, "big").unwrap();
    db.commit().unwrap();
    drop(db);
    let after_delete = raw_geometry_postings(&path, index.0);
    assert!(after_delete.is_empty(), "postings survived a delete: {after_delete:?}");
}

fn geo_fixture_ready(path: &Path) -> u64 {
    let mut db = Database::create(path, cfg()).unwrap();
    let collection = create_geo_collection(&mut db);
    for (key, shape) in fixture_geometries() {
        db.put(collection, key, &json!({"shape": shape})).unwrap();
    }
    db.commit().unwrap();
    let index = db
        .create_geometry_index(collection, "by_shape", "shape")
        .unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(index, 16).unwrap();
    db.commit().unwrap();
    drop(db);
    index.0
}

#[test]
fn verifier_reports_a_missing_geometry_posting() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let index = geo_fixture_ready(&path);
    let postings = raw_geometry_postings(&path, index);
    assert!(!postings.is_empty());

    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    assert!(raw.delete(&postings[0].0).unwrap());
    raw.commit().unwrap();
    drop(raw);

    let mut issues = Vec::new();
    let report =
        verify_indexed_source(&path, VerificationLimits::default(), |i| issues.push(i.clone()))
            .unwrap();
    assert!(!report.clean);
    assert!(issues.iter().any(|i| i.class == IssueClass::Derived
        && i.message.contains("spatial geometry posting")));
}

#[test]
fn verifier_reports_an_orphan_geometry_posting() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let index = geo_fixture_ready(&path);
    let postings = raw_geometry_postings(&path, index);
    assert!(!postings.is_empty());

    // A posting for a real entity but with the wrong bbox value: byte-for-byte
    // extra/mismatched, which is exactly what an un-retired stale posting
    // would look like after a geometry change the index failed to clean up.
    let (key, mut value) = postings[0].clone();
    value[0] ^= 0xff;
    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    raw.put(&key, &value).unwrap();
    raw.commit().unwrap();
    drop(raw);

    let mut issues = Vec::new();
    let report =
        verify_indexed_source(&path, VerificationLimits::default(), |i| issues.push(i.clone()))
            .unwrap();
    assert!(!report.clean);
    assert!(issues.iter().any(|i| i.class == IssueClass::Derived
        && i.message.contains("extra/mismatched spatial geometry posting")));
}

#[test]
fn rebuild_reproduces_the_geometry_build_byte_for_byte() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("rebuilt");
    let index = geo_fixture_ready(&source);

    let report = rebuild_derived_indexes(&source, &destination, RebuildLimits::default()).unwrap();
    assert_eq!(report.indexes, 1);

    let verified = verify_indexed_source(&destination, VerificationLimits::default(), |_| {}).unwrap();
    assert!(verified.complete && verified.clean);

    let source_postings = raw_geometry_postings(&source, index);
    let rebuilt_postings = raw_geometry_postings(&destination, index);
    assert_eq!(source_postings, rebuilt_postings, "rebuild changed the geometry postings");
}
