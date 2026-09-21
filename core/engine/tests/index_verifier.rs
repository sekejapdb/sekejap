use sekejap_core::{
    collections::{
        verification::{verify_indexed_source, IssueClass, VerificationLimits},
        CollectionOptions, Database,
    },
    pagewal::PageWalStore,
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;
use std::{ffi::OsString, fs, path::Path};

fn config() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}
fn inventory(path: &Path) -> Vec<(OsString, Vec<u8>)> {
    let mut names = fs::read_dir(path)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect::<Vec<_>>();
    names.sort();
    names
        .into_iter()
        .map(|n| {
            let b = fs::read(path.join(&n)).unwrap();
            (n, b)
        })
        .collect()
}
fn build(db: &mut Database, id: sekejap_core::collections::IndexId) {
    while !db.build_index_step(id, 16).unwrap() {
        db.commit().unwrap()
    }
    db.commit().unwrap()
}

/// Every tree a derived entry can live in: the primary tree, then one per
/// index that owns one. A version-1 database returns just the primary tree, so
/// these damage helpers read the same either way.
fn trees(path: &Path) -> Vec<Option<(u16, u32)>> {
    let db = Database::open(path, config()).unwrap();
    let mut out = vec![None];
    out.extend(db.index_trees().unwrap().into_iter().map(Some));
    out
}
/// Collect every key the predicate accepts, from whichever tree holds it.
fn keys_in(
    raw: &PageWalStore,
    trees: &[Option<(u16, u32)>],
    mut want: impl FnMut(&[u8]) -> bool,
) -> Vec<(Option<(u16, u32)>, Vec<u8>)> {
    let mut out = Vec::new();
    for tree in trees {
        match tree {
            None => raw
                .scan(|k, _| {
                    if want(k) {
                        out.push((None, k.to_vec()));
                    }
                    true
                })
                .unwrap(),
            Some((id, root)) => {
                for row in raw.tree_range(*id, *root, &[]).unwrap().into_iter().flatten() {
                    let (k, _) = row.unwrap();
                    if want(&k) {
                        out.push((*tree, k));
                    }
                }
            }
        }
    }
    out
}
/// Delete one entry where it actually lives. The fixtures here hold two rows,
/// so no index tree changes height and no descriptor root moves; the assert
/// keeps that assumption honest rather than silently writing a stale root.
fn delete_in(raw: &mut PageWalStore, at: Option<(u16, u32)>, key: &[u8]) -> bool {
    match at {
        None => raw.delete(key).unwrap(),
        Some((id, root)) => {
            let (found, after) = raw.tree_delete(id, root, key).unwrap();
            assert_eq!(after, root, "damage moved a per-index tree root");
            found
        }
    }
}
fn put_in(raw: &mut PageWalStore, at: Option<(u16, u32)>, key: &[u8], value: &[u8]) {
    match at {
        None => raw.put(key, value).unwrap(),
        Some((id, root)) => {
            let after = raw.tree_put(id, root, key, value).unwrap();
            assert_eq!(after, root, "damage moved a per-index tree root");
        }
    }
}

fn fixture(path: &Path) {
    let mut db = Database::create(path, config()).unwrap();
    let c = db
        .create_collection(
            "docs",
            vec![
                ("age".into(), Kind::Int),
                ("embedding".into(), Kind::Vector(2)),
                ("position".into(), Kind::Point),
                ("body".into(), Kind::Text),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    let a = db
        .put(
            c,
            "a",
            &json!({"age":7,"embedding":[1.0,2.0],
        "position":{"type":"Point","coordinates":[144.9,-37.8]},"body":"rust database database"}),
        )
        .unwrap();
    let b = db
        .put(
            c,
            "b",
            &json!({"age":9,"embedding":[2.0,3.0],
        "position":{"type":"Point","coordinates":[145.0,-37.7]},"body":"bounded search"}),
        )
        .unwrap();
    db.commit().unwrap();
    let scalar = db.create_scalar_index(c, "age", "age", true).unwrap();
    build(&mut db, scalar);
    let vector = db.create_exact_vector_index(c, "vec", "embedding").unwrap();
    build(&mut db, vector);
    let quantized = db
        .create_quantized_vector_index(c, "vec-int8", "embedding")
        .unwrap();
    build(&mut db, quantized);
    let point = db.create_point_index(c, "place", "position").unwrap();
    build(&mut db, point);
    let text = db.create_text_index(c, "words", "body").unwrap();
    build(&mut db, text);
    db.enable_graph().unwrap();
    db.link(a, "knows", b, "", &json!({"weight":u64::MAX}))
        .unwrap();
    db.commit().unwrap();
    drop(db);
}

#[test]
fn clean_all_family_source_is_complete_and_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    fixture(&path);
    let before = inventory(&path);
    let mut streamed = 0;
    let report =
        verify_indexed_source(&path, VerificationLimits::default(), |_| streamed += 1).unwrap();
    assert!(report.complete && report.clean);
    assert_eq!(streamed, 0);
    assert!(report.primary_rows >= 2);
    assert_eq!(inventory(&path), before);
}

#[test]
fn two_way_damage_classifies_derived_and_authoritative_loss_without_writes() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    fixture(&path);
    let trees = trees(&path);
    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    let tagged = keys_in(&raw, &trees, |k| {
        matches!(k.first(), Some(0x60 | 0x70 | 0x71 | 0x77))
    });
    for tag in [0x60, 0x70, 0x71] {
        let (at, key) = tagged.iter().find(|(_, k)| k[0] == tag).unwrap().clone();
        assert!(delete_in(&mut raw, at, &key));
    }
    let (at, stat) = tagged.iter().find(|(_, k)| k[0] == 0x77).unwrap().clone();
    put_in(&mut raw, at, &stat, &999u64.to_be_bytes());
    raw.commit().unwrap();
    drop(raw);
    let before = inventory(&path);
    let mut issues = Vec::new();
    let report = verify_indexed_source(&path, VerificationLimits::default(), |i| {
        issues.push(i.clone())
    })
    .unwrap();
    assert!(report.complete && !report.clean);
    assert!(
        report.primary_issues >= 2,
        "vector sidecar and graph primary are authoritative"
    );
    assert!(
        report.derived_issues >= 2,
        "scalar posting and text statistic are derived"
    );
    assert!(issues
        .iter()
        .any(|i| i.class == IssueClass::Primary && i.message.contains("sidecar")));
    assert!(issues
        .iter()
        .any(|i| i.class == IssueClass::Primary && i.message.contains("primary edge")));
    assert_eq!(inventory(&path), before);
}

#[test]
fn budget_exhaustion_is_an_error_and_preserves_source() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    fixture(&path);
    let before = inventory(&path);
    let mut limits = VerificationLimits::default();
    limits.max_rows = 1;
    let error = verify_indexed_source(&path, limits, |_| {}).unwrap_err();
    assert!(matches!(
        error,
        sekejap_core::collections::Error::Kernel(kernel::Error::ResourceLimit(_))
    ));
    assert_eq!(inventory(&path), before);
}

#[test]
fn orphan_namespace_and_missing_header_replica_are_not_false_clean() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    fixture(&path);
    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    assert!(raw.delete(&[0, 0, 2]).unwrap());
    let mut orphan = vec![0x70, 0x82, 0x03, 0xe7];
    orphan.extend([0, 0x81, 1]);
    raw.put(&orphan, &[]).unwrap();
    raw.commit().unwrap();
    drop(raw);
    let before = inventory(&path);
    let report = verify_indexed_source(&path, VerificationLimits::default(), |_| {}).unwrap();
    assert!(!report.clean);
    assert!(report.catalog_issues >= 1);
    assert!(report.derived_issues >= 1);
    assert_eq!(inventory(&path), before);
}

#[test]
fn building_index_is_reported_as_incomplete_lifecycle() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut db = Database::create(&path, config()).unwrap();
    let c = db
        .create_collection(
            "c",
            vec![("age".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.put(c, "a", &json!({"age":1})).unwrap();
    db.commit().unwrap();
    db.create_scalar_index(c, "age", "age", false).unwrap();
    db.commit().unwrap();
    drop(db);
    let before = inventory(&path);
    let report = verify_indexed_source(&path, VerificationLimits::default(), |_| {}).unwrap();
    assert!(report.complete && !report.clean);
    assert!(report.catalog_issues >= 1);
    assert_eq!(inventory(&path), before);
}

#[test]
fn missing_each_derived_family_entry_and_graph_reverse_is_reported() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    fixture(&path);
    let trees = trees(&path);
    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    let keys = keys_in(&raw, &trees, |key| {
        matches!(
            key.first(),
            Some(0x73 | 0x74 | 0x75 | 0x76 | 0x78 | 0x79 | 0x72)
        )
    });
    for tag in [0x73, 0x74, 0x75, 0x76, 0x78, 0x79, 0x72] {
        let (at, key) = keys.iter().find(|(_, key)| key[0] == tag).unwrap().clone();
        assert!(delete_in(&mut raw, at, &key));
    }
    raw.commit().unwrap();
    drop(raw);
    let before = inventory(&path);
    let report = verify_indexed_source(&path, VerificationLimits::default(), |_| {}).unwrap();
    assert!(report.complete && !report.clean);
    assert!(report.derived_issues >= 7);
    assert_eq!(inventory(&path), before);
}

#[test]
fn ordinary_collection_metadata_and_external_mapping_are_verified() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut db = Database::create(&path, config()).unwrap();
    let collection = db
        .create_collection(
            "plain",
            vec![("value".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.put(collection, "external", &json!({"value": 4}))
        .unwrap();
    db.commit().unwrap();
    drop(db);
    let clean = verify_indexed_source(&path, VerificationLimits::default(), |_| {}).unwrap();
    assert!(clean.complete && clean.clean);

    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    let mut layout = None;
    let mut mapping = None;
    raw.scan(|key, _| {
        if key.starts_with(&[0, 240]) && layout.is_none() {
            layout = Some(key.to_vec());
        }
        if key.first() == Some(&0x20) {
            mapping = Some(key.to_vec());
        }
        true
    })
    .unwrap();
    assert!(raw.delete(&layout.unwrap()).unwrap());
    assert!(raw.delete(&mapping.unwrap()).unwrap());
    raw.commit().unwrap();
    drop(raw);
    let before = inventory(&path);
    let report = verify_indexed_source(&path, VerificationLimits::default(), |_| {}).unwrap();
    assert!(report.complete && !report.clean);
    assert!(report.catalog_issues >= 1);
    assert!(report.derived_issues >= 1);
    assert_eq!(inventory(&path), before);
}

#[test]
fn duplicate_unique_scalar_value_is_detected_by_ordered_adjacency() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    fixture(&path);
    let trees = trees(&path);
    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    let mut scalar = keys_in(&raw, &trees, |key| key.first() == Some(&0x70));
    assert_eq!(scalar.len(), 2);
    let (at, second) = scalar.pop().unwrap();
    let (_, first) = scalar.pop().unwrap();
    assert_eq!(first.len(), second.len());
    let mut duplicate = first[..first.len() - 2].to_vec();
    duplicate.extend_from_slice(&second[second.len() - 2..]);
    assert!(delete_in(&mut raw, at, &second));
    put_in(&mut raw, at, &duplicate, &[]);
    raw.commit().unwrap();
    drop(raw);
    let before = inventory(&path);
    let mut saw_unique = false;
    let report = verify_indexed_source(&path, VerificationLimits::default(), |issue| {
        saw_unique |= issue.message.contains("unique scalar");
    })
    .unwrap();
    assert!(report.complete && !report.clean && saw_unique);
    assert_eq!(inventory(&path), before);
}

#[test]
fn damaged_replicas_fall_back_and_are_classified() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    fixture(&path);
    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    let mut graph = None;
    let mut descriptor = None;
    raw.scan(|key, value| {
        if key.first() == Some(&6) && graph.is_none() {
            graph = Some((key.to_vec(), value.to_vec()));
        }
        if key.first() == Some(&3) && descriptor.is_none() {
            descriptor = Some((key.to_vec(), value.to_vec()));
        }
        true
    })
    .unwrap();
    for (key, mut value) in [graph.unwrap(), descriptor.unwrap()] {
        let last = value.len() - 1;
        value[last] ^= 1;
        raw.put(&key, &value).unwrap();
    }
    raw.commit().unwrap();
    drop(raw);
    let before = inventory(&path);
    let report = verify_indexed_source(&path, VerificationLimits::default(), |_| {}).unwrap();
    assert!(report.complete && !report.clean);
    assert!(report.catalog_issues >= 2);
    assert_eq!(inventory(&path), before);
}

#[test]
fn malformed_authoritative_and_derived_values_keep_their_damage_class() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    fixture(&path);
    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    let mut selected = Vec::new();
    raw.scan(|key, value| {
        if matches!(key.first(), Some(0x60 | 0x71 | 0x74 | 0x75 | 0x79))
            && !selected
                .iter()
                .any(|(old, _): &(Vec<u8>, Vec<u8>)| old[0] == key[0])
        {
            selected.push((key.to_vec(), value.to_vec()));
        }
        true
    })
    .unwrap();
    for (key, mut value) in selected {
        match key[0] {
            0x60 => value.truncate(1),
            0x71 => value = vec![1, 255],
            0x74 => value[..8].copy_from_slice(&f64::NAN.to_le_bytes()),
            0x75 => value = vec![0],
            0x79 => value = vec![0],
            _ => unreachable!(),
        }
        raw.put(&key, &value).unwrap();
    }
    raw.commit().unwrap();
    drop(raw);
    let before = inventory(&path);
    let mut saw_primary_malformed = false;
    let mut saw_derived_malformed = false;
    let report = verify_indexed_source(&path, VerificationLimits::default(), |issue| {
        saw_primary_malformed |= issue.class == IssueClass::Primary
            && issue.kind == sekejap_core::collections::verification::IssueKind::Malformed;
        saw_derived_malformed |= issue.class == IssueClass::Derived
            && issue.kind == sekejap_core::collections::verification::IssueKind::Malformed;
    })
    .unwrap();
    assert!(report.complete && !report.clean);
    assert!(saw_primary_malformed && saw_derived_malformed);
    assert_eq!(inventory(&path), before);
}
