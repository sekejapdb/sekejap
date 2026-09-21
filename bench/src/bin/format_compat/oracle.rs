//! Fixed logical oracle for the preserved five-fixture corpus.
//! Ported from tests/format_v1_compat.rs; expected values come from its pinned
//! manifests and the original generator insertion sequence, never readback.
use sekejap_core::collections::{CollectionId, Database, EntityId};
use kernel::{
    io::IoMode,
    page::{PageRef, PAGE_SIZE},
    store::{Config, SyncMode},
};
use serde_json::Value;
use std::{collections::BTreeMap, fs, path::Path};
const FEATURES: usize = 48;
pub fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

pub fn expected_entity_id(collection: &str, key: &str) -> EntityId {
    assert_eq!(key.len(), 9, "unexpected fixture key {key}");
    let slot: u64 = key
        .strip_prefix('p')
        .expect("fixture key prefix")
        .parse()
        .unwrap();
    let (collection, sequence) = match (collection, slot) {
        ("people", 7) => (1, 201),
        ("people", 999) => (1, 202),
        ("people", 998) => (1, 203),
        ("people", 0..=199) => (1, slot + 1),
        ("events", 0..=79) => (2, slot + 1),
        ("blobs", 0..=3) => (3, slot + 1),
        _ => panic!("unexpected fixture identity {collection}/{key}"),
    };
    EntityId {
        collection: CollectionId(collection),
        sequence,
    }
}

pub fn features_on_disk(dir: &Path) -> [u64; 2] {
    let data = fs::read(dir.join("data")).unwrap();
    let mut out = [0; 2];
    for no in 0..2 {
        let b = &data[no * PAGE_SIZE..(no + 1) * PAGE_SIZE];
        let slot = PageRef::open(b, no as u32).unwrap().slot(0).to_vec();
        out[no] = u64::from_le_bytes(slot[FEATURES..FEATURES + 8].try_into().unwrap());
    }
    out
}

pub fn check_expected(db: &Database, manifest: &Value, label: &str) {
    let collections = manifest["collections"].as_array().unwrap();
    for c in collections {
        let name = c["name"].as_str().unwrap();
        let id = db
            .collection(name)
            .unwrap()
            .unwrap_or_else(|| panic!("{label}: collection {name} missing"));
        let expected_id = c["id"].as_u64().unwrap() as u32;
        assert_eq!(id.0, expected_id, "{label}: collection id for {name}");
        let info = db.collection_info(id).unwrap();
        assert_eq!(
            info.timestamps,
            c["timestamps"].as_bool().unwrap(),
            "{label}: timestamps for {name}"
        );
        assert_eq!(
            info.layout.id,
            c["layout_id"].as_u64().unwrap(),
            "{label}: layout id for {name}"
        );
        let expected_fields: Vec<(String, String)> = c["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                (
                    f["name"].as_str().unwrap().to_string(),
                    f["kind"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        let actual: Vec<(String, String)> = info
            .layout
            .fields
            .iter()
            .map(|(n, k)| (n.clone(), format!("{k:?}")))
            .collect();
        assert_eq!(actual, expected_fields, "{label}: fields for {name}");
    }
    let entities = manifest["entities"].as_array().unwrap();
    for e in entities {
        let coll = e["collection"].as_str().unwrap();
        let key = e["key"].as_str().unwrap();
        let id = db.collection(coll).unwrap().unwrap();
        let found = db
            .get(id, key)
            .unwrap()
            .unwrap_or_else(|| panic!("{label}: missing {coll}/{key}"));
        let expected_id = expected_entity_id(coll, key);
        assert_eq!(found.id, expected_id, "{label}: identity {coll}/{key}");
        assert_eq!(found.key, key, "{label}: external key {coll}/{key}");
        let by_id = db
            .get_by_id(expected_id)
            .unwrap()
            .unwrap_or_else(|| panic!("{label}: missing numeric identity {expected_id:?}"));
        assert_eq!(by_id, found, "{label}: numeric lookup {coll}/{key}");
        assert_eq!(
            &found.document, &e["document"],
            "{label}: document {coll}/{key}"
        );
    }
    for d in manifest["deleted"].as_array().unwrap() {
        let coll = d["collection"].as_str().unwrap();
        let key = d["key"].as_str().unwrap();
        let id = db.collection(coll).unwrap().unwrap();
        assert!(
            db.get(id, key).unwrap().is_none(),
            "{label}: expected {coll}/{key} absent"
        );
        assert!(
            db.get_by_id(expected_entity_id(coll, key))
                .unwrap()
                .is_none(),
            "{label}: deleted numeric identity for {coll}/{key} remains"
        );
    }
    assert!(
        db.get_by_id(EntityId {
            collection: CollectionId(1),
            sequence: 8
        })
        .unwrap()
        .is_none(),
        "{label}: retired pre-reinsert identity remains"
    );
    let mut expected_rows = BTreeMap::new();
    for e in entities {
        let key = (
            e["collection"].as_str().unwrap().to_owned(),
            e["key"].as_str().unwrap().to_owned(),
        );
        assert!(
            expected_rows.insert(key, e["document"].clone()).is_none(),
            "{label}: duplicate manifest entity"
        );
    }
    let mut actual_rows = BTreeMap::new();
    for c in collections {
        let name = c["name"].as_str().unwrap();
        let id = db.collection(name).unwrap().unwrap();
        for row in db.scan(id, None).unwrap() {
            let row = row.unwrap();
            assert_eq!(
                row.id,
                expected_entity_id(name, &row.key),
                "{label}: scan identity"
            );
            assert!(
                actual_rows
                    .insert((name.to_owned(), row.key), row.document)
                    .is_none(),
                "{label}: duplicate scan entity"
            );
        }
    }
    let expected = manifest["counts"]["entities"].as_u64().unwrap() as usize;
    assert_eq!(
        actual_rows, expected_rows,
        "{label}: exact scanned entities"
    );
    assert_eq!(actual_rows.len(), expected, "{label}: live entity count");
    assert_eq!(
        entities.len(),
        expected,
        "{label}: manifest entity list vs counts.entities"
    );
}

pub fn inserted_document(stage: &str) -> Value {
    serde_json::json!({
        "fullname": if stage == "upgraded" { "Compat Insert" } else { "Rollback Insert" },
        "born": 2000,
        "income": 1.25,
        "location": {"type": "Point", "coordinates": [0.0, 0.0]},
        "profile": {},
        "note": if stage == "upgraded" { "inserted" } else { "rollback-inserted" },
    })
}

pub fn expected_state(original: &Value, stage: &str) -> Value {
    assert!(
        matches!(stage, "original" | "upgraded" | "rollback"),
        "unknown state {stage}"
    );
    let mut expected = original.clone();
    if stage == "original" {
        return expected;
    }
    let entities = expected["entities"].as_array_mut().unwrap();
    entities.retain(|e| !(e["collection"] == "people" && e["key"] == "p00000001"));
    let updated = entities
        .iter_mut()
        .find(|e| e["collection"] == "people" && e["key"] == "p00000000")
        .unwrap();
    updated["document"]["note"] = serde_json::json!(if stage == "upgraded" {
        "compat-updated"
    } else {
        "compat-rollback"
    });
    let key = if stage == "upgraded" {
        "p00000999"
    } else {
        "p00000998"
    };
    entities.push(serde_json::json!({"collection": "people", "key": key, "document": inserted_document(stage)}));
    let deleted = expected["deleted"].as_array_mut().unwrap();
    deleted.push(serde_json::json!({"collection": "people", "key": "p00000001"}));
    if stage == "rollback" {
        deleted.push(serde_json::json!({"collection": "people", "key": "p00000999"}));
    }
    let deleted_count = deleted.len();
    expected["counts"]["deleted"] = serde_json::json!(deleted_count);
    expected
}

pub fn check_limits(db: &Database, manifest: &Value) {
    if manifest["limited"].as_bool().unwrap() {
        let actual = db.limits().expect("lost persisted resource policy");
        let actual = serde_json::json!({
            "data_bytes": actual.data_bytes, "wal_bytes": actual.wal_bytes,
            "tracked_pages": actual.tracked_pages, "readers": actual.readers,
            "record_bytes": actual.record_bytes, "recovery_bytes": actual.recovery_bytes,
        });
        assert_eq!(
            actual, manifest["limits"],
            "persisted resource policy changed"
        );
    } else {
        assert!(
            db.limits().is_none(),
            "ordinary fixture acquired resource policy"
        );
    }
}
