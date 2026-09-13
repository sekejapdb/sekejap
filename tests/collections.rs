use e4_prototype::{
    collections::{Clock, CollectionOptions, Database},
    Kind,
};
use kernel::{
    io::IoMode,
    limits::ResourceLimits,
    store::{Config, SyncMode},
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicI64, Ordering},
    Arc,
};

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}
fn fields() -> Vec<(String, Kind)> {
    vec![
        ("name".into(), Kind::Text),
        ("point".into(), Kind::Point),
        ("profile".into(), Kind::Json),
    ]
}
fn doc(n: i64) -> Value {
    json!({"name":format!("東京 {n}"),"point":{"type":"Point","coordinates":[1.25,-2.5]},"profile":{"nested":[null,true,{"n":n}]},"extra":{"unsigned":u64::MAX},"observed_at":123})
}
struct TestClock(AtomicI64);
impl Clock for TestClock {
    fn unix_seconds(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[test]
fn undeclared_fields_roundtrip_through_collection_and_reopen() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let people = db
        .create_collection("people", fields(), CollectionOptions::default())
        .unwrap();
    let id = db.put(people, "person/é\0one", &doc(1)).unwrap();
    assert_eq!(
        db.get(people, "person/é\0one").unwrap().unwrap().document,
        doc(1)
    );
    db.commit().unwrap();
    drop(db);
    let db = Database::open(&path, cfg()).unwrap();
    assert_eq!(db.collection("people").unwrap(), Some(people));
    let row = db.get_by_id(id).unwrap().unwrap();
    assert_eq!(row.key, "person/é\0one");
    assert_eq!(row.document, doc(1));
    assert!(row.document.get("_created_unix").is_none());
}

#[test]
fn identity_upsert_delete_and_collection_isolation_follow_e3_contracts() {
    let d = tempfile::tempdir().unwrap();
    let mut db = Database::create(d.path().join("db"), cfg()).unwrap();
    let a = db
        .create_collection("a", fields(), Default::default())
        .unwrap();
    let b = db
        .create_collection("b", fields(), Default::default())
        .unwrap();
    let id = db.put(a, "same", &doc(1)).unwrap();
    let other = db.put(b, "same", &doc(2)).unwrap();
    assert_ne!(id, other);
    assert_eq!(db.put(a, "same", &doc(3)).unwrap(), id);
    assert_eq!(
        db.update(a, "same", &json!({"name":"updated","nullable":null}))
            .unwrap(),
        id
    );
    let row = db.get(a, "same").unwrap().unwrap();
    assert_eq!(row.document["profile"], doc(3)["profile"]);
    assert_eq!(row.document["nullable"], Value::Null);
    assert!(db.update(a, "missing", &json!({})).is_err());
    db.commit().unwrap();
    assert!(db.delete(a, "same").unwrap());
    db.commit().unwrap();
    assert!(!db.delete(a, "same").unwrap());
    assert!(db.get_by_id(id).unwrap().is_none());
    let replacement = db.put(a, "same", &doc(4)).unwrap();
    assert!(replacement.sequence > id.sequence);
    assert_eq!(db.get_by_id(other).unwrap().unwrap().document, doc(2));
    db.commit().unwrap();
    assert!(db.delete(a, "same").unwrap());
    db.commit().unwrap();
    drop(db);
    let mut db = Database::open(d.path().join("db"), cfg()).unwrap();
    assert_eq!(db.scan(a, None).unwrap().count(), 0);
    let after_empty_reopen = db.put(a, "same", &doc(5)).unwrap();
    assert!(after_empty_reopen.sequence > replacement.sequence);
    assert_eq!(db.get_by_id(other).unwrap().unwrap().document, doc(2));
}

#[test]
fn timestamps_are_explicit_persistent_monotonic_and_managed_on_every_write() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("db");
    let clock = Arc::new(TestClock(AtomicI64::new(100)));
    let mut db = Database::create(&path, cfg()).unwrap();
    db.set_clock(clock.clone());
    let on = db
        .create_collection("on", fields(), CollectionOptions { timestamps: true })
        .unwrap();
    let off = db
        .create_collection("off", fields(), CollectionOptions { timestamps: false })
        .unwrap();
    db.put(on, "x", &doc(1)).unwrap();
    db.put(off, "x", &json!({"_updated_unix":7})).unwrap();
    clock.0.store(200, Ordering::Relaxed);
    db.put(on, "x", &doc(2)).unwrap();
    let row = db.get(on, "x").unwrap().unwrap();
    assert_eq!(row.document["_created_unix"], 100);
    assert_eq!(row.document["_updated_unix"], 200);
    clock.0.store(150, Ordering::Relaxed);
    db.update(on, "x", &json!({"name":"backward"})).unwrap();
    assert_eq!(
        db.get(on, "x").unwrap().unwrap().document["_updated_unix"],
        200
    );
    assert!(db.put(on, "bad", &json!({"_created_unix":1})).is_err());
    assert!(db.update(on, "x", &json!({"_updated_unix":1})).is_err());
    clock.0.store(300, Ordering::Relaxed);
    db.update(on, "x", &json!({})).unwrap();
    assert_eq!(
        db.get(on, "x").unwrap().unwrap().document["_updated_unix"],
        300
    );
    db.commit().unwrap();
    drop(db);
    let mut db = Database::open(&path, cfg()).unwrap();
    db.set_clock(clock);
    assert!(db.collection_info(on).unwrap().timestamps);
    assert!(!db.collection_info(off).unwrap().timestamps);
    assert_eq!(
        db.get(off, "x").unwrap().unwrap().document,
        json!({"_updated_unix":7})
    );
    db.update(on, "x", &json!({"name":"reopened"})).unwrap();
    assert_eq!(
        db.get(on, "x").unwrap().unwrap().document["_created_unix"],
        100
    );
}

#[test]
fn published_snapshots_rollback_and_streaming_cursor_are_consistent() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let c = db
        .create_collection("c", fields(), Default::default())
        .unwrap();
    let first = db.put(c, "a", &doc(1)).unwrap();
    db.commit().unwrap();
    let mut old = Database::open_snapshot(&path, cfg()).unwrap();
    db.put(c, "a", &doc(2)).unwrap();
    db.put(c, "b", &doc(3)).unwrap();
    assert_eq!(old.get_by_id(first).unwrap().unwrap().document, doc(1));
    assert!(old.put(c, "c", &doc(1)).is_err());
    db.rollback().unwrap();
    assert!(db.get(c, "b").unwrap().is_none());
    assert_eq!(db.get_by_id(first).unwrap().unwrap().document, doc(1));
    db.put(c, "a", &doc(4)).unwrap();
    let second = db.put(c, "b", &doc(5)).unwrap();
    db.commit().unwrap();
    let fresh = Database::open_snapshot(&path, cfg()).unwrap();
    assert_eq!(fresh.get_by_id(first).unwrap().unwrap().document, doc(4));
    assert_eq!(old.get_by_id(first).unwrap().unwrap().document, doc(1));
    let rows = fresh
        .scan(c, Some(first))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, second);
}

#[test]
fn immutable_layout_dispatch_and_vector_sidecars_survive_updates() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let c = db
        .create_collection(
            "c",
            vec![
                ("number".into(), Kind::Int),
                ("embedding".into(), Kind::Vector(3)),
            ],
            Default::default(),
        )
        .unwrap();
    let id = db
        .put(c, "old", &json!({"number":7,"embedding":[0.5,1.25,-2.0]}))
        .unwrap();
    db.commit().unwrap();
    let previous = db.collection_info(c).unwrap().layout.id;
    let next = db
        .alter_collection(c, vec![("number".into(), Kind::Text)])
        .unwrap();
    assert_ne!(previous, next);
    assert_eq!(db.get_by_id(id).unwrap().unwrap().document["number"], 7);
    db.put(c, "new", &json!({"number":"seven"})).unwrap();
    db.commit().unwrap();
    drop(db);
    let mut db = Database::open(&path, cfg()).unwrap();
    assert_eq!(
        db.get_by_id(id).unwrap().unwrap().document["embedding"],
        json!([0.5, 1.25, -2.0])
    );
    db.put(c, "old", &json!({"number":"updated"})).unwrap();
    db.commit().unwrap();
    assert_eq!(
        db.get_by_id(id).unwrap().unwrap().document,
        json!({"number":"updated"})
    );
    assert!(db
        .create_collection("c", vec![], Default::default())
        .is_err());
    assert!(db.put(c, "invalid", &json!({"number":1})).is_err());
    db.put(c, "still-usable", &json!({"number":"ok"})).unwrap();
}

#[test]
fn resource_refusal_cannot_publish_a_partial_entity_or_key_mapping() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("db");
    let limits = ResourceLimits {
        data_bytes: 256 << 10,
        wal_bytes: 64 << 10,
        tracked_pages: 1024,
        readers: 4,
        record_bytes: 16 << 10,
        recovery_bytes: 64 << 10,
    };
    let mut db = Database::create_limited(&path, cfg(), limits).unwrap();
    let c = db
        .create_collection("c", vec![], Default::default())
        .unwrap();
    let id = db.put(c, "kept", &json!({"v":1})).unwrap();
    db.commit().unwrap();
    let old = Database::open_snapshot(&path, cfg()).unwrap();
    let mut refused = false;
    for i in 0..1000 {
        if db
            .put(c, &format!("new-{i}"), &json!({"v":"x".repeat(8000)}))
            .is_err()
        {
            refused = true;
            break;
        }
    }
    assert!(refused);
    assert!(db.get_by_id(id).is_err());
    assert!(db.commit().is_err());
    assert_eq!(old.get_by_id(id).unwrap().unwrap().document, json!({"v":1}));
    db.rollback().unwrap();
    assert_eq!(db.scan(c, None).unwrap().count(), 1);
    assert_eq!(db.get_by_id(id).unwrap().unwrap().document, json!({"v":1}));
}

#[test]
fn deterministic_model_covers_commits_rollbacks_and_collection_switches() {
    use std::collections::BTreeMap;
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let cs = [
        db.create_collection("a", fields(), Default::default())
            .unwrap(),
        db.create_collection("b", fields(), Default::default())
            .unwrap(),
    ];
    db.commit().unwrap();
    let mut model = BTreeMap::new();
    let mut durable = BTreeMap::new();
    let mut seed = 731u64;
    for step in 0..1200 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let c = cs[(seed >> 32) as usize % 2];
        let key = format!("key-{}", (seed >> 40) % 40);
        let mk = (c, key.clone());
        match (seed >> 16) % 8 {
            0..=2 => {
                let d = doc(step);
                let id = db.put(c, &key, &d).unwrap();
                if let Some((old, _)) = model.get(&mk) {
                    assert_eq!(*old, id);
                }
                model.insert(mk, (id, d));
            }
            3 => {
                assert_eq!(db.delete(c, &key).unwrap(), model.remove(&mk).is_some());
            }
            4 => {
                let r = db.update(c, &key, &json!({"name":"patched"}));
                if let Some((id, d)) = model.get_mut(&mk) {
                    assert_eq!(r.unwrap(), *id);
                    d["name"] = json!("patched");
                } else {
                    assert!(r.is_err());
                }
            }
            5 => {
                db.commit().unwrap();
                durable = model.clone();
                let snap = Database::open_snapshot(&path, cfg()).unwrap();
                for c in cs {
                    let actual = snap
                        .scan(c, None)
                        .unwrap()
                        .map(|r| {
                            let e = r.unwrap();
                            ((c, e.key), (e.id, e.document))
                        })
                        .collect::<BTreeMap<_, _>>();
                    let expected = durable
                        .iter()
                        .filter(|((id, _), _)| *id == c)
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect::<BTreeMap<_, _>>();
                    assert_eq!(actual, expected);
                }
            }
            6 => {
                db.rollback().unwrap();
                model = durable.clone();
            }
            _ => {
                assert_eq!(
                    db.get(c, &key).unwrap().map(|e| (e.id, e.document)),
                    model.get(&mk).cloned()
                );
            }
        }
        if step % 37 == 0 {
            for c in cs {
                let actual = db
                    .scan(c, None)
                    .unwrap()
                    .map(|r| {
                        let e = r.unwrap();
                        ((c, e.key), (e.id, e.document))
                    })
                    .collect::<BTreeMap<_, _>>();
                let expected = model
                    .iter()
                    .filter(|((id, _), _)| *id == c)
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<BTreeMap<_, _>>();
                assert_eq!(actual, expected);
            }
        }
    }
    db.commit().unwrap();
    drop(db);
    let db = Database::open(&path, cfg()).unwrap();
    for ((c, key), (id, d)) in model {
        let e = db.get(c, &key).unwrap().unwrap();
        assert_eq!(e.id, id);
        assert_eq!(e.document, d);
    }
}
