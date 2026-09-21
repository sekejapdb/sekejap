//! Explicit Phase-2 opt-in on an external COPY of an immutable Phase-1 fixture.
//! The fixed manifest oracle, not engine readback, supplies all expected rows.
#[path = "format_compat/oracle.rs"]
mod oracle;
use sekejap_core::collections::{Database, EntityId, IndexId, ScalarPredicate};
use oracle::*;
use serde_json::Value;
use std::{collections::BTreeMap, env, fs, path::Path};

fn check_index(db: &Database, index: IndexId, expected: &Value) {
    let mut values = BTreeMap::<String, Vec<EntityId>>::new();
    for e in expected["entities"].as_array().unwrap() {
        if e["collection"] != "people" {
            continue;
        }
        values
            .entry(e["document"]["note"].as_str().unwrap().to_owned())
            .or_default()
            .push(expected_entity_id("people", e["key"].as_str().unwrap()));
    }
    for ids in values.values_mut() {
        ids.sort();
    }
    for (note, ids) in &values {
        assert_eq!(
            db.query_scalar(
                index,
                ScalarPredicate::Eq(Value::String(note.clone())),
                1024
            )
            .unwrap(),
            *ids,
            "scalar equality for {note:?}"
        );
    }
    let expected: Vec<_> = values.into_values().flatten().collect();
    assert_eq!(
        db.query_scalar(
            index,
            ScalarPredicate::Range {
                lower: None,
                upper: None
            },
            1024
        )
        .unwrap(),
        expected,
        "complete scalar order/membership"
    );
}
fn prepare(manifest: &Value, dir: &Path, boundary: &str) {
    assert!(matches!(boundary, "checkpointed" | "wal-pending"));
    let declared = features_on_disk(dir);
    assert_eq!(
        declared,
        [u64::from(manifest["compact_cells"].as_bool().unwrap()); 2]
    );
    let mut db = Database::open(dir, cfg()).unwrap();
    check_expected(&db, manifest, "before-explicit-index");
    check_limits(&db, manifest);
    let people = db.collection("people").unwrap().unwrap();
    // The original snapshot both proves pre-opt-in visibility and pins WAL.
    let pin = Database::open_snapshot(dir, cfg()).unwrap();
    let index = db
        .create_scalar_index(people, "compat_note", "note", false)
        .unwrap();
    assert_eq!(index, IndexId(1));
    while !db.build_index_step(index, 32).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();
    check_index(&db, index, manifest);
    assert_eq!(
        db.update(
            people,
            "p00000000",
            &serde_json::json!({"note":"compat-updated"})
        )
        .unwrap(),
        expected_entity_id("people", "p00000000")
    );
    assert_eq!(
        db.put(people, "p00000999", &inserted_document("upgraded"))
            .unwrap(),
        expected_entity_id("people", "p00000999")
    );
    assert!(db.delete(people, "p00000001").unwrap());
    db.commit().unwrap();
    let expected = expected_state(manifest, "upgraded");
    check_expected(&db, &expected, "indexed-CRUD");
    check_index(&db, index, &expected);
    check_limits(&db, manifest);
    check_expected(&pin, manifest, "pinned-before-opt-in");
    assert!(pin.list_indexes(people).unwrap().is_empty());
    drop(pin);
    if boundary == "checkpointed" {
        assert!(db.checkpoint().unwrap());
    }
    drop(db);
    assert_eq!(
        fs::metadata(dir.join("wal")).unwrap().len() == 0,
        boundary == "checkpointed"
    );
    assert_eq!(
        features_on_disk(dir),
        declared,
        "explicit logical opt-in changed physical features"
    );
}
fn verify(manifest: &Value, dir: &Path) {
    let db = Database::open_snapshot(dir, cfg()).unwrap();
    let expected = expected_state(manifest, "upgraded");
    check_expected(&db, &expected, "indexed-reopen");
    check_index(&db, IndexId(1), &expected);
    check_limits(&db, manifest);
    assert_eq!(
        features_on_disk(dir),
        [u64::from(manifest["compact_cells"].as_bool().unwrap()); 2]
    );
}
fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    assert!(args.len() == 3 || args.len() == 4,
        "usage: index_format_fixture prepare MANIFEST COPIED_DB checkpointed|wal-pending\n       index_format_fixture verify MANIFEST COPIED_DB");
    let source = fs::canonicalize(&args[1]).unwrap();
    let dir = fs::canonicalize(&args[2]).unwrap();
    let corpus = source.parent().unwrap().parent().unwrap();
    assert!(
        !dir.starts_with(corpus),
        "refusing source corpus; supply an external copy"
    );
    let manifest: Value = serde_json::from_slice(&fs::read(source).unwrap()).unwrap();
    match args[0].as_str() {
        "prepare" if args.len() == 4 => prepare(&manifest, &dir, &args[3]),
        "verify" if args.len() == 3 => verify(&manifest, &dir),
        _ => panic!("unsupported command"),
    }
    println!(
        "{}",
        serde_json::json!({"result":"PASS","command":args[0],"database":dir})
    );
}
