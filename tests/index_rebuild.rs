use e4_prototype::{
    collections::{
        rebuild::{rebuild_derived_indexes, RebuildLimits},
        verification::{verify_indexed_source, VerificationLimits},
        CollectionOptions, Database, ScalarPredicate,
    },
    pagewal::PageWalStore,
    Kind,
};
use kernel::{
    io::IoMode,
    limits::ResourceLimits,
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
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let bytes = fs::read(path.join(&name)).unwrap();
            (name, bytes)
        })
        .collect()
}

fn build(db: &mut Database, id: e4_prototype::collections::IndexId) {
    while !db.build_index_step(id, 16).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();
}

fn fixture(
    path: &Path,
) -> (
    e4_prototype::collections::CollectionId,
    Vec<e4_prototype::collections::IndexId>,
) {
    let mut db = Database::create(path, config()).unwrap();
    let docs = db
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
    let people = db
        .create_collection(
            "people",
            vec![("name".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    let a = db
        .put(
            docs,
            "a",
            &json!({"age":7,"embedding":[1.0,2.0],
                "position":{"type":"Point","coordinates":[144.9,-37.8]},
                "body":"rust database database"}),
        )
        .unwrap();
    let b = db.put(people, "b", &json!({"name":"Ada"})).unwrap();
    db.commit().unwrap();
    let scalar = db.create_scalar_index(docs, "age", "age", true).unwrap();
    build(&mut db, scalar);
    let vector = db
        .create_exact_vector_index(docs, "embedding", "embedding")
        .unwrap();
    build(&mut db, vector);
    let quantized = db
        .create_quantized_vector_index(docs, "embedding-int8", "embedding")
        .unwrap();
    build(&mut db, quantized);
    let point = db.create_point_index(docs, "position", "position").unwrap();
    build(&mut db, point);
    let text = db.create_text_index(docs, "body", "body").unwrap();
    build(&mut db, text);
    db.enable_graph().unwrap();
    db.link(a, "knows", b, "work", &json!({"weight":u64::MAX}))
        .unwrap();
    db.commit().unwrap();
    drop(db);
    (docs, vec![scalar, vector, quantized, point, text])
}

#[test]
fn rebuilds_all_derived_families_and_catalog_references_without_touching_source() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("rebuilt");
    let (docs, indexes) = fixture(&source);
    let mut raw = PageWalStore::open(&source, false, 1 << 20).unwrap();
    let mut remove = Vec::new();
    raw.scan(|key, _| {
        if matches!(
            key.first(),
            Some(
                4 | 5
                    | 0x10
                    | 0x11
                    | 0x12
                    | 0x20
                    | 0x70
                    | 0x72
                    | 0x73
                    | 0x74
                    | 0x75
                    | 0x76
                    | 0x77
                    | 0x78
                    | 0x79
            )
        ) {
            remove.push(key.to_vec());
        }
        true
    })
    .unwrap();
    for key in remove {
        raw.delete(&key).unwrap();
    }
    raw.commit().unwrap();
    drop(raw);

    let before = inventory(&source);
    let report = rebuild_derived_indexes(&source, &destination, RebuildLimits::default()).unwrap();
    assert_eq!(report.indexes, 5);
    assert_eq!(report.primary_rows, 2);
    assert_eq!(report.vector_sidecars, 1);
    assert_eq!(report.primary_edges, 1);
    assert!(destination.join("COMPLETE").is_file());
    assert!(!destination.join("REBUILD_INCOMPLETE").exists());
    assert_eq!(inventory(&source), before);

    let verified =
        verify_indexed_source(&destination, VerificationLimits::default(), |_| {}).unwrap();
    assert!(verified.complete && verified.clean);
    let db = Database::open(&destination, config()).unwrap();
    assert_eq!(db.get(docs, "a").unwrap().unwrap().document["age"], 7);
    assert_eq!(
        db.query_scalar(indexes[0], ScalarPredicate::Eq(json!(7)), 8)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn authoritative_row_vector_graph_or_declaration_loss_refuses_publication() {
    for tag in [0x40, 0x60, 0x71, 3, 6] {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("rebuilt");
        fixture(&source);
        let mut raw = PageWalStore::open(&source, false, 1 << 20).unwrap();
        let mut keys = Vec::new();
        raw.scan(|candidate, _| {
            if candidate.first() == Some(&tag) {
                keys.push(candidate.to_vec());
            }
            true
        })
        .unwrap();
        assert!(!keys.is_empty());
        if tag == 3 {
            let identity = keys[0][2..].to_vec();
            keys.retain(|key| key[2..] == identity);
        } else if tag != 6 {
            keys.truncate(1);
        }
        for key in keys {
            assert!(raw.delete(&key).unwrap());
        }
        raw.commit().unwrap();
        drop(raw);
        let before = inventory(&source);
        assert!(rebuild_derived_indexes(&source, &destination, RebuildLimits::default()).is_err());
        assert!(!destination.join("COMPLETE").exists());
        assert_eq!(inventory(&source), before);
    }
}

#[test]
fn damaged_single_metadata_replicas_are_normalized_in_the_new_destination() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("rebuilt");
    fixture(&source);
    let mut raw = PageWalStore::open(&source, false, 1 << 20).unwrap();
    let mut selected = Vec::new();
    raw.scan(|key, value| {
        if matches!(key.first(), Some(0 | 1 | 2 | 3 | 6 | 7))
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
        let last = value.len() - 1;
        value[last] ^= 1;
        raw.put(&key, &value).unwrap();
    }
    raw.commit().unwrap();
    drop(raw);
    let before = inventory(&source);
    rebuild_derived_indexes(&source, &destination, RebuildLimits::default()).unwrap();
    let verified =
        verify_indexed_source(&destination, VerificationLimits::default(), |_| {}).unwrap();
    assert!(verified.complete && verified.clean);
    assert_eq!(inventory(&source), before);
}

#[test]
fn lifecycle_overlap_existing_and_disk_budget_refusals_never_publish() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("rebuilt");
    let mut db = Database::create(&source, config()).unwrap();
    let collection = db
        .create_collection(
            "c",
            vec![("age".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.put(collection, "a", &json!({"age": 1})).unwrap();
    db.commit().unwrap();
    db.create_scalar_index(collection, "age", "age", false)
        .unwrap();
    db.commit().unwrap();
    drop(db);
    let before = inventory(&source);
    assert!(rebuild_derived_indexes(&source, &destination, RebuildLimits::default()).is_err());
    assert!(!destination.exists());
    assert!(rebuild_derived_indexes(&source, &source, RebuildLimits::default()).is_err());
    fs::create_dir(&destination).unwrap();
    assert!(rebuild_derived_indexes(&source, &destination, RebuildLimits::default()).is_err());
    assert_eq!(inventory(&source), before);

    let source = temp.path().join("budget-source");
    let destination = temp.path().join("budget-target");
    fixture(&source);
    let before = inventory(&source);
    let mut work_limits = RebuildLimits::default();
    work_limits.max_records = 1;
    let work_destination = temp.path().join("work-target");
    assert!(rebuild_derived_indexes(&source, &work_destination, work_limits).is_err());
    assert!(!work_destination.exists());
    assert_eq!(inventory(&source), before);

    let mut limits = RebuildLimits::default();
    limits.max_destination_logical_bytes = 4 * kernel::page::PAGE_SIZE as u64;
    assert!(rebuild_derived_indexes(&source, &destination, limits).is_err());
    assert!(!destination.exists());
    assert!(!destination.join("COMPLETE").exists());
    assert_eq!(inventory(&source), before);
}

#[test]
fn persisted_resource_policy_and_control_bytes_are_preflighted() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("limited-source");
    let refused = temp.path().join("refused-target");
    let destination = temp.path().join("rebuilt-target");
    let policy = ResourceLimits {
        data_bytes: 4 << 20,
        wal_bytes: 2 << 20,
        tracked_pages: 8192,
        readers: 8,
        record_bytes: 32 << 10,
        recovery_bytes: 64 << 10,
    };
    let mut db = Database::create_limited(&source, config(), policy).unwrap();
    let collection = db
        .create_collection(
            "limited",
            vec![("age".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.put(collection, "a", &json!({"age": 7})).unwrap();
    db.commit().unwrap();
    let index = db
        .create_scalar_index(collection, "age", "age", false)
        .unwrap();
    build(&mut db, index);
    drop(db);
    let before = inventory(&source);

    let mut too_small = RebuildLimits::default();
    too_small.max_destination_logical_bytes = policy.data_bytes + policy.wal_bytes;
    assert!(rebuild_derived_indexes(&source, &refused, too_small).is_err());
    assert!(
        !refused.exists(),
        "control reserve must be checked before create"
    );
    assert_eq!(inventory(&source), before);

    let mut enough = RebuildLimits::default();
    enough.max_destination_logical_bytes = policy.data_bytes + policy.wal_bytes + (64 << 10);
    rebuild_derived_indexes(&source, &destination, enough).unwrap();
    assert!(destination.join("COMPLETE").is_file());
    let reopened = Database::open(&destination, config()).unwrap();
    assert_eq!(reopened.limits(), Some(policy));
    assert_eq!(inventory(&source), before);
}
