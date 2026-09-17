//! Phase 2 exact-vector contract. The oracle below owns its f64 math and never
//! calls an engine vector visitor, scorer, heap, descriptor or key codec.
use e4_prototype::{
    collections::{
        CollectionOptions, Database, EntityId, Error, IndexFamily, IndexState, ScalarPredicate,
        VectorCandidates, VectorHit, VectorMetric,
    },
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;
use std::collections::BTreeMap;

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn oracle(
    rows: &BTreeMap<EntityId, Vec<f32>>,
    query: &[f32],
    metric: VectorMetric,
    candidates: Option<&[EntityId]>,
    k: usize,
) -> Vec<VectorHit> {
    let query_norm = query
        .iter()
        .fold(0.0f64, |sum, x| sum + f64::from(*x) * f64::from(*x));
    let mut out = Vec::new();
    for (id, stored) in rows {
        if candidates.is_some_and(|ids| ids.binary_search(id).is_err()) {
            continue;
        }
        let mut dot = 0.0f64;
        let mut norm = 0.0f64;
        let mut l2 = 0.0f64;
        for (a, b) in stored.iter().zip(query) {
            let a = f64::from(*a);
            let b = f64::from(*b);
            dot += a * b;
            norm += a * a;
            l2 += (a - b) * (a - b);
        }
        let distance = match metric {
            VectorMetric::Cosine if norm == 0.0 => continue,
            VectorMetric::Cosine => 1.0 - dot / (norm.sqrt() * query_norm.sqrt()),
            VectorMetric::SquaredL2 => l2,
            VectorMetric::NegativeDot => -dot,
        };
        out.push(VectorHit { id: *id, distance });
    }
    out.sort_by(|a, b| {
        a.distance
            .total_cmp(&b.distance)
            .then_with(|| a.id.cmp(&b.id))
    });
    out.truncate(k);
    out
}

fn assert_hits(actual: &[VectorHit], expected: &[VectorHit]) {
    assert_eq!(
        actual.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        expected.iter().map(|hit| hit.id).collect::<Vec<_>>()
    );
    for (actual, expected) in actual.iter().zip(expected) {
        assert!((actual.distance - expected.distance).abs() <= 1e-14);
    }
}

fn query(
    db: &Database,
    index: e4_prototype::collections::IndexId,
    vector: &[f32],
    metric: VectorMetric,
    k: usize,
    candidates: VectorCandidates<'_>,
    max_examined: usize,
) -> e4_prototype::collections::Result<Vec<VectorHit>> {
    db.query_exact_vector(index, vector, metric, k, candidates, max_examined, || false)
}

#[test]
fn tiny_workload_matches_independent_metrics_and_filters_before_top_k() {
    let t = tempfile::tempdir().unwrap();
    let mut db = Database::create(t.path().join("db"), cfg()).unwrap();
    let people = db
        .create_collection(
            "people",
            vec![
                ("age".into(), Kind::Int),
                ("active".into(), Kind::Bool),
                ("embedding".into(), Kind::Vector(2)),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    let data = [
        ("p0", 20, true, [1.0, 0.0]),
        ("p1", 30, true, [1.0, 1.0]),
        ("p2", 30, false, [0.0, 1.0]),
        ("p3", 40, true, [-1.0, 0.0]),
        ("p4", 50, false, [0.0, -1.0]),
        ("p5", 60, true, [1.0, 0.0]),
    ];
    let mut rows = BTreeMap::new();
    let mut ids = BTreeMap::new();
    for (key, age, active, vector) in data {
        let id = db
            .put(
                people,
                key,
                &json!({"age":age,"active":active,"embedding":vector,
                    "profile":{"codes":[1,2,3],"nested":{"enabled":true}}}),
            )
            .unwrap();
        ids.insert(key, id);
        rows.insert(id, vector.to_vec());
    }
    db.commit().unwrap();
    let index = db
        .create_exact_vector_index(people, "embedding_exact", "embedding")
        .unwrap();
    assert_eq!(
        db.index_info(index).unwrap().family,
        IndexFamily::ExactVector
    );
    assert!(matches!(
        db.index_info(index).unwrap().state,
        IndexState::Building { .. }
    ));
    assert!(db
        .query_scalar(index, ScalarPredicate::Eq(json!(1)), 1)
        .is_err());
    assert!(query(
        &db,
        index,
        &[1.0, 0.0],
        VectorMetric::Cosine,
        3,
        VectorCandidates::All,
        10
    )
    .is_err());
    while !db.build_index_step(index, 2).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();

    for metric in [
        VectorMetric::Cosine,
        VectorMetric::SquaredL2,
        VectorMetric::NegativeDot,
    ] {
        let actual = query(&db, index, &[1.0, 0.0], metric, 6, VectorCandidates::All, 6).unwrap();
        assert_hits(&actual, &oracle(&rows, &[1.0, 0.0], metric, None, 6));
    }
    assert_eq!(
        query(
            &db,
            index,
            &[1.0, 0.0],
            VectorMetric::Cosine,
            3,
            VectorCandidates::All,
            6
        )
        .unwrap()
        .iter()
        .map(|hit| hit.id)
        .collect::<Vec<_>>(),
        vec![ids["p0"], ids["p5"], ids["p1"]]
    );
    let filtered = [ids["p1"], ids["p3"], ids["p5"]];
    let expected = oracle(&rows, &[1.0, 0.0], VectorMetric::Cosine, Some(&filtered), 2);
    let actual = query(
        &db,
        index,
        &[1.0, 0.0],
        VectorMetric::Cosine,
        2,
        VectorCandidates::SortedUnique(&filtered),
        3,
    )
    .unwrap();
    assert_hits(&actual, &expected);
    assert_eq!(
        actual.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        vec![ids["p5"], ids["p1"]]
    );
}

#[test]
fn validation_zero_vectors_ties_and_work_bounds_are_explicit() {
    let t = tempfile::tempdir().unwrap();
    let mut db = Database::create(t.path().join("db"), cfg()).unwrap();
    let c = db
        .create_collection("c", vec![("v".into(), Kind::Vector(2))], Default::default())
        .unwrap();
    let zero = db.put(c, "zero", &json!({"v":[0.0,0.0]})).unwrap();
    let a = db.put(c, "a", &json!({"v":[1.0,0.0]})).unwrap();
    let b = db.put(c, "b", &json!({"v":[1.0,0.0]})).unwrap();
    let index = db.create_exact_vector_index(c, "v", "v").unwrap();
    assert!(db.build_index_step(index, 16).unwrap());
    assert_eq!(
        query(
            &db,
            index,
            &[1.0, 0.0],
            VectorMetric::Cosine,
            8,
            VectorCandidates::All,
            3
        )
        .unwrap()
        .iter()
        .map(|hit| hit.id)
        .collect::<Vec<_>>(),
        vec![a, b]
    );
    assert_eq!(
        query(
            &db,
            index,
            &[0.0, 0.0],
            VectorMetric::SquaredL2,
            8,
            VectorCandidates::All,
            3
        )
        .unwrap()[0]
            .id,
        zero
    );
    assert!(query(
        &db,
        index,
        &[0.0, 0.0],
        VectorMetric::Cosine,
        1,
        VectorCandidates::All,
        3
    )
    .is_err());
    assert!(query(
        &db,
        index,
        &[1.0],
        VectorMetric::SquaredL2,
        1,
        VectorCandidates::All,
        3
    )
    .is_err());
    assert!(query(
        &db,
        index,
        &[f32::NAN, 0.0],
        VectorMetric::SquaredL2,
        1,
        VectorCandidates::All,
        3
    )
    .is_err());
    assert!(matches!(
        query(
            &db,
            index,
            &[1.0, 0.0],
            VectorMetric::Cosine,
            1,
            VectorCandidates::All,
            2
        ),
        Err(Error::Kernel(kernel::Error::ResourceLimit(_)))
    ));
    let unsorted = [b, a];
    assert!(query(
        &db,
        index,
        &[1.0, 0.0],
        VectorMetric::Cosine,
        1,
        VectorCandidates::SortedUnique(&unsorted),
        2
    )
    .is_err());
    let mut calls = 0;
    assert!(matches!(
        db.query_exact_vector(
            index,
            &[1.0, 0.0],
            VectorMetric::Cosine,
            3,
            VectorCandidates::All,
            3,
            || {
                calls += 1;
                calls == 2
            }
        ),
        Err(Error::Cancelled)
    ));
}

#[test]
fn immutable_layout_ordinals_live_crud_snapshots_rollback_and_reopen() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let c = db
        .create_collection(
            "c",
            vec![("left".into(), Kind::Vector(2))],
            Default::default(),
        )
        .unwrap();
    let old = db.put(c, "old", &json!({"left":[1.0,0.0]})).unwrap();
    db.commit().unwrap();
    db.alter_collection(
        c,
        vec![
            ("right".into(), Kind::Vector(2)),
            ("left".into(), Kind::Vector(2)),
        ],
    )
    .unwrap();
    db.put(c, "missing", &json!({})).unwrap();
    db.put(c, "null", &json!({"left":null,"right":null}))
        .unwrap();
    let left = db.create_exact_vector_index(c, "left", "left").unwrap();
    let right = db.create_exact_vector_index(c, "right", "right").unwrap();
    assert!(db.build_index_step(left, 8).unwrap());
    assert!(db.build_index_step(right, 8).unwrap());
    db.commit().unwrap();
    let snapshot = Database::open_snapshot(&path, cfg()).unwrap();

    db.alter_collection(
        c,
        vec![
            ("left".into(), Kind::Vector(2)),
            ("right".into(), Kind::Vector(2)),
        ],
    )
    .unwrap();
    let new = db
        .put(c, "new", &json!({"left":[1.0,0.0],"right":[1.0,0.0]}))
        .unwrap();
    db.update(c, "old", &json!({"left":null,"right":[1.0,0.0]}))
        .unwrap();
    db.commit().unwrap();
    assert_eq!(
        query(
            &db,
            left,
            &[1.0, 0.0],
            VectorMetric::Cosine,
            8,
            VectorCandidates::All,
            8
        )
        .unwrap()
        .iter()
        .map(|hit| hit.id)
        .collect::<Vec<_>>(),
        vec![new]
    );
    assert_eq!(
        query(
            &db,
            right,
            &[1.0, 0.0],
            VectorMetric::Cosine,
            8,
            VectorCandidates::All,
            8
        )
        .unwrap()
        .iter()
        .map(|hit| hit.id)
        .collect::<Vec<_>>(),
        vec![old, new]
    );
    assert_eq!(
        query(
            &snapshot,
            left,
            &[1.0, 0.0],
            VectorMetric::Cosine,
            8,
            VectorCandidates::All,
            8
        )
        .unwrap()[0]
            .id,
        old
    );
    db.update(c, "new", &json!({"left":[0.0,1.0]})).unwrap();
    db.rollback().unwrap();
    assert_eq!(
        query(
            &db,
            left,
            &[1.0, 0.0],
            VectorMetric::Cosine,
            8,
            VectorCandidates::All,
            8
        )
        .unwrap()[0]
            .id,
        new
    );
    assert!(db.delete(c, "new").unwrap());
    let replacement = db.put(c, "new", &json!({"left":[1.0,0.0]})).unwrap();
    assert_ne!(new, replacement);
    db.commit().unwrap();
    drop(snapshot);
    drop(db);
    let db = Database::open(&path, cfg()).unwrap();
    assert_eq!(
        query(
            &db,
            left,
            &[1.0, 0.0],
            VectorMetric::Cosine,
            8,
            VectorCandidates::All,
            8
        )
        .unwrap()[0]
            .id,
        replacement
    );
}

#[test]
fn two_collections_build_and_drop_resume_without_touching_payloads() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let a = db
        .create_collection("a", vec![("v".into(), Kind::Vector(2))], Default::default())
        .unwrap();
    let b = db
        .create_collection("b", vec![("v".into(), Kind::Vector(3))], Default::default())
        .unwrap();
    for n in 0..7 {
        db.put(a, &format!("a{n}"), &json!({"v":[n as f32,1.0]}))
            .unwrap();
        db.put(b, &format!("b{n}"), &json!({"v":[n as f32,1.0,2.0]}))
            .unwrap();
    }
    db.commit().unwrap();
    let ai = db.create_exact_vector_index(a, "v", "v").unwrap();
    let bi = db.create_exact_vector_index(b, "v", "v").unwrap();
    assert!(!db.build_index_step(ai, 2).unwrap());
    assert!(!db.build_index_step(bi, 2).unwrap());
    db.commit().unwrap();
    drop(db);
    let mut db = Database::open(&path, cfg()).unwrap();
    while !db.build_index_step(ai, 2).unwrap() {
        db.commit().unwrap();
    }
    while !db.build_index_step(bi, 2).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();
    assert_eq!(
        query(
            &db,
            ai,
            &[1.0, 1.0],
            VectorMetric::SquaredL2,
            8,
            VectorCandidates::All,
            7
        )
        .unwrap()
        .len(),
        7
    );
    assert_eq!(
        query(
            &db,
            bi,
            &[1.0, 1.0, 2.0],
            VectorMetric::SquaredL2,
            8,
            VectorCandidates::All,
            7
        )
        .unwrap()
        .len(),
        7
    );
    db.begin_drop_index(ai).unwrap();
    assert!(!db.drop_index_step(ai, 2).unwrap());
    db.commit().unwrap();
    drop(db);
    let mut db = Database::open(&path, cfg()).unwrap();
    assert_eq!(db.index_info(ai).unwrap().state, IndexState::Dropping);
    while !db.drop_index_step(ai, 2).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();
    assert!(db.index_info(ai).is_err());
    assert!(db.get(a, "a1").unwrap().unwrap().document["v"].is_array());
    assert_eq!(
        query(
            &db,
            bi,
            &[1.0, 1.0, 2.0],
            VectorMetric::SquaredL2,
            1,
            VectorCandidates::All,
            7
        )
        .unwrap()
        .len(),
        1
    );
}

#[test]
fn late_build_refuses_incompatible_historical_same_name_field() {
    let t = tempfile::tempdir().unwrap();
    let mut db = Database::create(t.path().join("db"), cfg()).unwrap();
    let c = db
        .create_collection("c", vec![("v".into(), Kind::Int)], Default::default())
        .unwrap();
    db.put(c, "old", &json!({"v":7})).unwrap();
    db.commit().unwrap();
    db.alter_collection(c, vec![("v".into(), Kind::Vector(2))])
        .unwrap();
    let index = db.create_exact_vector_index(c, "v", "v").unwrap();
    db.commit().unwrap();
    assert!(matches!(
        db.build_index_step(index, 8),
        Err(Error::InvalidInput(message)) if message.contains("historical vector field")
    ));
    assert!(matches!(
        db.index_info(index).unwrap().state,
        IndexState::Building { after: 0 }
    ));
}
