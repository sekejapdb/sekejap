//! Opt-in quantized-scan contract. The oracle implements the frozen int8
//! approximation and authoritative f32 rerank independently of engine codecs.
use sekejap_core::{
    Kind,
    collections::{
        ApproxVectorMethod, CollectionOptions, Database, EntityId, Error, IndexFamily, IndexId,
        IndexState, QuantizedVectorCandidates, VectorHit, VectorMetric,
    },
    pagewal::PageWalStore,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;
use std::{cmp::Ordering, collections::BTreeMap, fs, io::Write, path::Path};

fn cfg() -> Config {
    Config {
        budget_bytes: 4 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn finish_build(db: &mut Database, index: IndexId, batch: usize) {
    while !db.build_index_step(index, batch).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();
}

fn ordered(number: u64) -> Vec<u8> {
    let bytes = number.to_be_bytes();
    let start = bytes.iter().position(|byte| *byte != 0).unwrap_or(7);
    let mut out = vec![0x80 + (8 - start) as u8];
    out.extend_from_slice(&bytes[start..]);
    out
}

fn entry_key(index: IndexId, entity: EntityId) -> Vec<u8> {
    let mut key = vec![0x79];
    key.extend(ordered(index.0));
    key.extend(ordered(entity.sequence));
    key
}

fn sidecar_key(entity: EntityId, ordinal: usize) -> Vec<u8> {
    let mut key = vec![0x60];
    key.extend(ordered(entity.collection.0.into()));
    key.extend(ordered(entity.sequence));
    key.extend(ordered(ordinal as u64));
    key
}

fn ready_fixture(path: &std::path::Path) -> (IndexId, EntityId) {
    let mut db = Database::create(path, cfg()).unwrap();
    let collection = db
        .create_collection(
            "vectors",
            vec![("embedding".into(), Kind::Vector(2))],
            CollectionOptions::default(),
        )
        .unwrap();
    let entity = db
        .put(collection, "one", &json!({"embedding":[1.0,2.0]}))
        .unwrap();
    db.commit().unwrap();
    let index = db
        .create_quantized_vector_index(collection, "embedding_int8", "embedding")
        .unwrap();
    finish_build(&mut db, index, 8);
    (index, entity)
}

fn reseal(packet: &mut [u8]) {
    assert_eq!(packet.len(), 2081);
    let end = packet.len() - 4;
    let checksum = crc32c::crc32c(&packet[..end]).to_le_bytes();
    packet[end..].copy_from_slice(&checksum);
}

fn files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn visit(root: &Path, directory: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if path.is_dir() {
                out.insert(format!("{name}/"), vec![]);
                visit(root, &path, out);
            } else {
                out.insert(name, fs::read(path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}

fn force_fresh_admission(path: &Path) {
    fs::OpenOptions::new()
        .append(true)
        .open(path.join("wal"))
        .unwrap()
        .write_all(b"uncommitted-quantized-admission-tail")
        .unwrap();
    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name().to_string_lossy().starts_with("reader") {
            fs::remove_file(entry.path()).unwrap();
        }
    }
}

fn score(stored: &[f32], query: &[f32], metric: VectorMetric) -> Option<f64> {
    let mut dot = 0.0;
    let mut stored_norm = 0.0;
    let mut query_norm = 0.0;
    let mut l2 = 0.0;
    for (&stored, &query) in stored.iter().zip(query) {
        let stored = f64::from(stored);
        let query = f64::from(query);
        dot += stored * query;
        stored_norm += stored * stored;
        query_norm += query * query;
        l2 += (stored - query) * (stored - query);
    }
    let distance = match metric {
        VectorMetric::SquaredL2 => l2,
        VectorMetric::NegativeDot => -dot,
        VectorMetric::Cosine if stored_norm == 0.0 => return None,
        VectorMetric::Cosine => 1.0 - dot / (stored_norm.sqrt() * query_norm.sqrt()),
    };
    Some(if distance == 0.0 { 0.0 } else { distance })
}

fn approximate_score(stored: &[f32], query: &[f32], metric: VectorMetric) -> Option<f64> {
    let maximum = stored
        .iter()
        .map(|lane| f64::from(*lane).abs())
        .fold(0.0f64, f64::max);
    let scale = maximum / 127.0;
    let mut dot = 0.0;
    let mut stored_norm = 0.0;
    let mut query_norm = 0.0;
    let mut l2 = 0.0;
    for (&stored, &query) in stored.iter().zip(query) {
        let stored = if scale == 0.0 {
            0.0
        } else {
            let code = (f64::from(stored) / scale).round().clamp(-127.0, 127.0) as i8;
            f64::from(code) * scale
        };
        let query = f64::from(query);
        dot += stored * query;
        stored_norm += stored * stored;
        query_norm += query * query;
        l2 += (stored - query) * (stored - query);
    }
    let distance = match metric {
        VectorMetric::SquaredL2 => l2,
        VectorMetric::NegativeDot => -dot,
        VectorMetric::Cosine if stored_norm == 0.0 => return None,
        VectorMetric::Cosine => 1.0 - dot / (stored_norm.sqrt() * query_norm.sqrt()),
    };
    Some(if distance == 0.0 { 0.0 } else { distance })
}

fn oracle(
    rows: &BTreeMap<EntityId, Vec<f32>>,
    query: &[f32],
    metric: VectorMetric,
    candidates: Option<&[EntityId]>,
    k: usize,
    ef: usize,
) -> Vec<VectorHit> {
    let mut approximate_hits = Vec::new();
    for (id, vector) in rows {
        if candidates.is_some_and(|ids| ids.binary_search(id).is_err()) {
            continue;
        }
        if let Some(distance) = approximate_score(vector, query, metric) {
            approximate_hits.push(VectorHit { id: *id, distance });
        }
    }
    approximate_hits.sort_by(|left, right| {
        left.distance
            .total_cmp(&right.distance)
            .then_with(|| left.id.cmp(&right.id))
    });
    approximate_hits.truncate(ef);
    let mut exact: Vec<_> = approximate_hits
        .into_iter()
        .filter_map(|hit| {
            score(&rows[&hit.id], query, metric).map(|distance| VectorHit {
                id: hit.id,
                distance,
            })
        })
        .collect();
    exact.sort_by(|left, right| {
        left.distance
            .total_cmp(&right.distance)
            .then_with(|| left.id.cmp(&right.id))
    });
    exact.truncate(k);
    exact
}

fn assert_hits(actual: &[VectorHit], expected: &[VectorHit]) {
    assert_eq!(
        actual.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        expected.iter().map(|hit| hit.id).collect::<Vec<_>>()
    );
    for (actual, expected) in actual.iter().zip(expected) {
        assert_eq!(
            actual.distance.total_cmp(&expected.distance),
            Ordering::Equal
        );
    }
}

#[test]
fn approximate_shortlist_then_exact_rerank_matches_independent_oracle() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Database::create(temp.path().join("db"), cfg()).unwrap();
    let people = db
        .create_collection(
            "people",
            vec![("embedding".into(), Kind::Vector(3))],
            CollectionOptions::default(),
        )
        .unwrap();
    let values = [
        ("a", [1.0, 0.0, 0.0]),
        ("b", [0.9, 0.1, 0.0]),
        ("c", [0.0, 1.0, 0.0]),
        ("d", [-1.0, 0.0, 0.0]),
        ("e", [0.0, 0.0, 0.0]),
        ("f", [f32::from_bits(1), 0.0, 0.0]),
    ];
    let mut rows = BTreeMap::new();
    let mut ids = BTreeMap::new();
    for (key, vector) in values {
        let id = db.put(people, key, &json!({"embedding":vector})).unwrap();
        ids.insert(key, id);
        rows.insert(id, vector.to_vec());
    }
    db.commit().unwrap();
    let index = db
        .create_quantized_vector_index(people, "embedding_int8", "embedding")
        .unwrap();
    assert_eq!(
        db.index_info(index).unwrap().family,
        IndexFamily::QuantizedVector
    );
    assert!(matches!(
        db.index_info(index).unwrap().state,
        IndexState::Building { .. }
    ));
    finish_build(&mut db, index, 2);

    assert!(
        db.query_quantized_vector(
            index,
            &[1.0, 0.0, 0.0],
            VectorMetric::Cosine,
            1,
            1,
            QuantizedVectorCandidates::All,
            0,
            || false,
        )
        .is_err()
    );
    assert!(
        db.query_quantized_vector(
            index,
            &[1.0, 0.0, 0.0],
            VectorMetric::Cosine,
            1,
            1,
            QuantizedVectorCandidates::All,
            rows.len(),
            || true,
        )
        .is_err()
    );

    for metric in [
        VectorMetric::Cosine,
        VectorMetric::SquaredL2,
        VectorMetric::NegativeDot,
    ] {
        let expected = oracle(&rows, &[1.0, 0.0, 0.0], metric, None, 3, 4);
        let result = db
            .query_quantized_vector(
                index,
                &[1.0, 0.0, 0.0],
                metric,
                3,
                4,
                QuantizedVectorCandidates::All,
                rows.len(),
                || false,
            )
            .unwrap();
        assert_eq!(result.method, ApproxVectorMethod::SymmetricInt8ScanV1);
        assert_eq!(result.ef, 4);
        assert_eq!(result.examined, rows.len());
        assert!(result.reranked <= 4);
        assert_hits(&result.hits, &expected);
    }

    let filtered = [ids["b"], ids["d"], ids["f"]];
    let expected = oracle(
        &rows,
        &[1.0, 0.0, 0.0],
        VectorMetric::Cosine,
        Some(&filtered),
        2,
        2,
    );
    let result = db
        .query_quantized_vector(
            index,
            &[1.0, 0.0, 0.0],
            VectorMetric::Cosine,
            2,
            2,
            QuantizedVectorCandidates::SortedUnique(&filtered),
            filtered.len(),
            || false,
        )
        .unwrap();
    assert_eq!(result.examined, filtered.len());
    assert_hits(&result.hits, &expected);
}

#[test]
fn validation_live_crud_snapshot_reopen_and_drop_preserve_authoritative_vectors() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let collection = db
        .create_collection(
            "vectors",
            vec![("embedding".into(), Kind::Vector(2))],
            CollectionOptions::default(),
        )
        .unwrap();
    let a = db
        .put(collection, "a", &json!({"embedding":[1.0,0.0]}))
        .unwrap();
    db.put(collection, "null", &json!({"embedding":null}))
        .unwrap();
    db.put(collection, "missing", &json!({})).unwrap();
    db.commit().unwrap();
    let index = db
        .create_quantized_vector_index(collection, "embedding_int8", "embedding")
        .unwrap();
    finish_build(&mut db, index, 1);
    let old = Database::open_snapshot(&path, cfg()).unwrap();

    assert!(
        db.query_quantized_vector(
            index,
            &[0.0, 0.0],
            VectorMetric::Cosine,
            1,
            1,
            QuantizedVectorCandidates::All,
            8,
            || false,
        )
        .is_err()
    );
    assert!(
        db.query_quantized_vector(
            index,
            &[f32::NAN, 0.0],
            VectorMetric::SquaredL2,
            1,
            1,
            QuantizedVectorCandidates::All,
            8,
            || false,
        )
        .is_err()
    );
    assert!(
        db.query_quantized_vector(
            index,
            &[1.0, 0.0],
            VectorMetric::SquaredL2,
            2,
            1,
            QuantizedVectorCandidates::All,
            8,
            || false,
        )
        .is_err()
    );

    db.update(collection, "a", &json!({"embedding":[0.0,1.0]}))
        .unwrap();
    assert!(db.delete(collection, "null").unwrap());
    let b = db
        .put(collection, "b", &json!({"embedding":[1.0,0.0]}))
        .unwrap();
    db.commit().unwrap();

    let old_result = old
        .query_quantized_vector(
            index,
            &[1.0, 0.0],
            VectorMetric::SquaredL2,
            1,
            2,
            QuantizedVectorCandidates::All,
            8,
            || false,
        )
        .unwrap();
    assert_eq!(old_result.hits[0].id, a);
    let reopened = Database::open_snapshot(&path, cfg()).unwrap();
    let new_result = reopened
        .query_quantized_vector(
            index,
            &[1.0, 0.0],
            VectorMetric::SquaredL2,
            1,
            2,
            QuantizedVectorCandidates::All,
            8,
            || false,
        )
        .unwrap();
    assert_eq!(new_result.hits[0].id, b);

    drop(old);
    drop(reopened);
    db.begin_drop_index(index).unwrap();
    db.commit().unwrap();
    while !db.drop_index_step(index, 2).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();
    assert!(db.index_info(index).is_err());
}

#[test]
fn late_build_tracks_historical_vector_ordinals_across_layout_reorder() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Database::create(temp.path().join("db"), cfg()).unwrap();
    let collection = db
        .create_collection(
            "history",
            vec![
                ("embedding".into(), Kind::Vector(2)),
                ("label".into(), Kind::Text),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    let old = db
        .put(
            collection,
            "old",
            &json!({"embedding":[1.0,0.0],"label":"old"}),
        )
        .unwrap();
    db.commit().unwrap();
    db.alter_collection(
        collection,
        vec![
            ("label".into(), Kind::Text),
            ("embedding".into(), Kind::Vector(2)),
        ],
    )
    .unwrap();
    let new = db
        .put(
            collection,
            "new",
            &json!({"embedding":[0.0,1.0],"label":"new"}),
        )
        .unwrap();
    db.put(
        collection,
        "null",
        &json!({"embedding":null,"label":"null"}),
    )
    .unwrap();
    db.commit().unwrap();

    let index = db
        .create_quantized_vector_index(collection, "embedding_int8", "embedding")
        .unwrap();
    finish_build(&mut db, index, 1);
    let result = db
        .query_quantized_vector(
            index,
            &[1.0, 0.0],
            VectorMetric::SquaredL2,
            2,
            2,
            QuantizedVectorCandidates::All,
            3,
            || false,
        )
        .unwrap();
    assert_eq!(
        result.hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        vec![old, new]
    );

    db.update(collection, "old", &json!({"embedding":[0.0,-1.0]}))
        .unwrap();
    db.commit().unwrap();
    let result = db
        .query_quantized_vector(
            index,
            &[0.0, -1.0],
            VectorMetric::SquaredL2,
            1,
            2,
            QuantizedVectorCandidates::All,
            3,
            || false,
        )
        .unwrap();
    assert_eq!(result.hits[0].id, old);
}

#[test]
fn unknown_quantizer_options_family_or_version_refuses_before_source_mutation() {
    let temp = tempfile::tempdir().unwrap();
    for damage in 0..4u8 {
        let path = temp.path().join(format!("unknown-quantized-{damage}"));
        let (index, _) = ready_fixture(&path);
        assert_eq!(index.0, 1);
        let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
        for copy in 0..3u8 {
            let key = [3, copy, 0x81, 1];
            let mut descriptor = raw.get(&key).unwrap().unwrap();
            assert_eq!(&descriptor[..8], b"E4IDX01\0");
            match damage {
                0 => descriptor[10 + 19] = 2,
                1 => descriptor[10 + 20] = 1,
                2 => descriptor[10 + 12] = 0x7f,
                3 => descriptor[10 + 13..10 + 15].copy_from_slice(&2u16.to_be_bytes()),
                _ => unreachable!(),
            }
            reseal(&mut descriptor);
            raw.put(&key, &descriptor).unwrap();
        }
        raw.commit().unwrap();
        drop(raw);
        force_fresh_admission(&path);
        let before = files(&path);
        for snapshot in [false, true] {
            let opened = if snapshot {
                Database::open_snapshot(&path, cfg())
            } else {
                Database::open(&path, cfg())
            };
            assert!(matches!(opened, Err(Error::Unsupported(_))));
            assert_eq!(files(&path), before, "refusal changed source files");
        }
    }
}

#[test]
fn malformed_compact_entries_and_authoritative_sidecars_are_corruption() {
    let temp = tempfile::tempdir().unwrap();
    for damage in 0..10u8 {
        let path = temp.path().join(format!("quantized-corrupt-{damage}"));
        let (index, entity) = ready_fixture(&path);
        let compact_key = entry_key(index, entity);
        let sidecar = sidecar_key(entity, 1); // hidden external-key slot is ordinal zero
        let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
        let original = raw.get(&compact_key).unwrap().unwrap();
        assert_eq!(original.len(), 16);
        match damage {
            0 => raw.put(&compact_key, &original[..15]).unwrap(),
            1 => {
                let mut value = original.clone();
                value[..4].fill(0);
                raw.put(&compact_key, &value).unwrap();
            }
            2 => {
                let mut value = original.clone();
                value[4..6].copy_from_slice(&0u16.to_be_bytes());
                raw.put(&compact_key, &value).unwrap();
            }
            3 => {
                let mut value = original.clone();
                value[6..14].copy_from_slice(&f64::NAN.to_le_bytes());
                raw.put(&compact_key, &value).unwrap();
            }
            4 => {
                let mut value = original.clone();
                value[14] = 128;
                raw.put(&compact_key, &value).unwrap();
            }
            5 => {
                let mut value = original.clone();
                value[14..].copy_from_slice(&[1, 1]);
                raw.put(&compact_key, &value).unwrap();
            }
            6 => assert!(raw.delete(&sidecar).unwrap()),
            7 => raw.put(&sidecar, &[0; 7]).unwrap(),
            8 => raw
                .put(
                    &sidecar,
                    &[f32::NAN.to_le_bytes(), 0.0f32.to_le_bytes()].concat(),
                )
                .unwrap(),
            9 => {
                let mut value = original.clone();
                assert_eq!(value[15], 127);
                value[14] = value[14].saturating_sub(1);
                raw.put(&compact_key, &value).unwrap();
            }
            _ => unreachable!(),
        }
        raw.commit().unwrap();
        drop(raw);

        let db = Database::open_snapshot(&path, cfg()).unwrap();
        let selected = [entity];
        for candidates in [
            QuantizedVectorCandidates::All,
            QuantizedVectorCandidates::SortedUnique(&selected),
        ] {
            assert!(matches!(
                db.query_quantized_vector(
                    index,
                    &[1.0, 2.0],
                    VectorMetric::SquaredL2,
                    1,
                    1,
                    candidates,
                    1,
                    || false,
                ),
                Err(Error::Corrupt(_))
            ));
        }
    }
}
