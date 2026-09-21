//! Independent comparison oracle: it sorts logical values, never index bytes
//! and never E4 scalar helpers. Eq(null) here is the low-level index candidate
//! bucket (missing + null), not SQL three-valued equality semantics.
use sekejap_core::{
    collections::{CollectionId, Database, EntityId, IndexId, ScalarPredicate},
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::{json, Value};
use std::{cmp::Ordering, collections::BTreeMap};

type Rows = BTreeMap<String, (EntityId, Value)>;
fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}
fn cmp(a: Option<&Value>, b: Option<&Value>, field: &str) -> Ordering {
    let a = a.filter(|v| !v.is_null());
    let b = b.filter(|v| !v.is_null());
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(a), Some(b)) => match field {
            "i" => a.as_i64().unwrap().cmp(&b.as_i64().unwrap()),
            "r" => a
                .as_f64()
                .unwrap()
                .partial_cmp(&b.as_f64().unwrap())
                .unwrap(),
            "s" => a
                .as_str()
                .unwrap()
                .as_bytes()
                .cmp(b.as_str().unwrap().as_bytes()),
            "b" => a.as_bool().unwrap().cmp(&b.as_bool().unwrap()),
            _ => unreachable!(),
        },
    }
}
fn expected(rows: &Rows, field: &str, pred: &ScalarPredicate, limit: usize) -> Vec<EntityId> {
    let mut out: Vec<_> = rows
        .values()
        .filter(|(_, v)| match pred {
            ScalarPredicate::Eq(q) => cmp(v.get(field), Some(q), field).is_eq(),
            ScalarPredicate::Range { lower, upper } => {
                lower
                    .as_ref()
                    .is_none_or(|q| !cmp(v.get(field), Some(q), field).is_lt())
                    && upper
                        .as_ref()
                        .is_none_or(|q| !cmp(v.get(field), Some(q), field).is_gt())
            }
        })
        .collect();
    out.sort_by(|(ia, a), (ib, b)| cmp(a.get(field), b.get(field), field).then_with(|| ia.cmp(ib)));
    out.into_iter().take(limit).map(|(id, _)| *id).collect()
}
fn document(i: usize, generation: usize) -> Value {
    let ints = [
        i64::MIN,
        -9_007_199_254_740_993,
        -1,
        0,
        1,
        9_007_199_254_740_992,
        9_007_199_254_740_993,
        i64::MAX,
    ];
    let reals = [-1e200, -1.5, -0.0, 0.0, f64::MIN_POSITIVE, 1.5, 1e200];
    let texts = ["", "a", "a\0", "a\0z", "aa", "é", "e\u{301}", "東京", "🦀"];
    let j = i + generation;
    let mut v = json!({"i":ints[j%ints.len()], "r":reals[j%reals.len()],
        "s":texts[j%texts.len()], "b":j%2==0, "profile":{"codes":[1,2,3],"round":generation}});
    if j % 13 == 0 {
        for f in ["i", "r", "s", "b"] {
            v[f] = Value::Null;
        }
    } else if j % 17 == 0 {
        for f in ["i", "r", "s", "b"] {
            v.as_object_mut().unwrap().remove(f);
        }
    }
    v
}
fn verify(db: &Database, c: CollectionId, indexes: &[(&str, IndexId)], rows: &Rows) {
    let actual: Rows = db
        .scan(c, None)
        .unwrap()
        .map(|r| {
            let e = r.unwrap();
            (e.key, (e.id, e.document))
        })
        .collect();
    assert_eq!(&actual, rows, "reference document population differs");
    for &(field, index) in indexes {
        let probes = match field {
            "i" => vec![
                json!(null),
                json!(i64::MIN),
                json!(-1),
                json!(0),
                json!(9_007_199_254_740_992i64),
                json!(9_007_199_254_740_993i64),
                json!(i64::MAX),
            ],
            "r" => vec![
                json!(null),
                json!(-1e200),
                json!(-0.0),
                json!(0.0),
                json!(1.5),
                json!(1e200),
            ],
            "s" => vec![
                json!(null),
                json!(""),
                json!("a"),
                json!("a\0"),
                json!("a\0z"),
                json!("é"),
                json!("not present"),
            ],
            "b" => vec![json!(null), json!(false), json!(true)],
            _ => unreachable!(),
        };
        let mut predicates = vec![ScalarPredicate::Range {
            lower: None,
            upper: None,
        }];
        for q in &probes {
            predicates.push(ScalarPredicate::Eq(q.clone()));
        }
        for w in probes.windows(2) {
            predicates.push(ScalarPredicate::Range {
                lower: Some(w[0].clone()),
                upper: Some(w[1].clone()),
            });
            predicates.push(ScalarPredicate::Range {
                lower: Some(w[1].clone()),
                upper: Some(w[0].clone()),
            });
        }
        for pred in predicates {
            for limit in [0, 1, 7, 256] {
                assert_eq!(
                    db.query_scalar(index, pred.clone(), limit).unwrap(),
                    expected(rows, field, &pred, limit),
                    "collection {c:?}, field {field}, predicate {pred:?}, limit {limit}"
                );
            }
        }
    }
}
#[test]
fn scalar_indexes_match_independent_logical_oracle_through_repeated_crud() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let mut collections = Vec::new();
    let mut states = Vec::new();
    let mut indexes = Vec::new();
    for collection in 0..2 {
        let c = db
            .create_collection(
                &format!("c{collection}"),
                vec![
                    ("i".into(), Kind::Int),
                    ("r".into(), Kind::Real),
                    ("s".into(), Kind::Text),
                    ("b".into(), Kind::Bool),
                ],
                Default::default(),
            )
            .unwrap();
        let mut rows = Rows::new();
        for i in 0..100 {
            let key = format!("key/{i}");
            let v = document(i, collection);
            let id = db.put(c, &key, &v).unwrap();
            rows.insert(key, (id, v));
        }
        db.commit().unwrap();
        let mut ix = Vec::new();
        for field in ["i", "r", "s", "b"] {
            // Same names and fields in both collections: registry isolation.
            let id = db.create_scalar_index(c, field, field, false).unwrap();
            loop {
                let ready = db.build_index_step(id, 37).unwrap();
                db.commit().unwrap();
                if ready {
                    break;
                }
            }
            ix.push((field, id));
        }
        collections.push(c);
        states.push(rows);
        indexes.push(ix);
    }
    for n in 0..2 {
        verify(&db, collections[n], &indexes[n], &states[n]);
    }
    let old = Database::open_snapshot(&path, cfg()).unwrap();
    let old_states = states.clone();
    for generation in 1..=3 {
        for n in 0..2 {
            let c = collections[n];
            for i in (0..100).step_by(5) {
                let key = format!("key/{i}");
                if let Some((id, _)) = states[n].get(&key).cloned() {
                    let v = document(i, generation * 7 + n);
                    assert_eq!(db.put(c, &key, &v).unwrap(), id);
                    states[n].insert(key, (id, v));
                }
            }
            for i in (generation..100).step_by(11) {
                let key = format!("key/{i}");
                let prior = states[n].remove(&key);
                assert_eq!(db.delete(c, &key).unwrap(), prior.is_some());
                if i % 2 == 0 {
                    let v = document(i, generation * 13 + n);
                    let id = db.put(c, &key, &v).unwrap();
                    if let Some((previous, _)) = prior {
                        assert_ne!(id, previous);
                    }
                    states[n].insert(key, (id, v));
                }
            }
            db.commit().unwrap();
            verify(&db, c, &indexes[n], &states[n]);
        }
        for n in 0..2 {
            verify(&old, collections[n], &indexes[n], &old_states[n]);
        }
    }
    // A temporary replacement must not survive rollback in any index family.
    db.put(collections[0], "rollback-only", &document(1, 0))
        .unwrap();
    db.put(collections[0], "key/0", &document(19, 19)).unwrap();
    db.rollback().unwrap();
    for n in 0..2 {
        verify(&db, collections[n], &indexes[n], &states[n]);
    }
    drop(old);
    drop(db);
    let db = Database::open(&path, cfg()).unwrap();
    for n in 0..2 {
        verify(&db, collections[n], &indexes[n], &states[n]);
    }
}
