//! Page-order sidecar/compact scans must return the same top-k as the
//! per-entity locator path. All-candidate search is the page-order walk;
//! SortedUnique of the whole corpus is the per-entity path.
use e4_prototype::{
    Kind,
    collections::{
        CollectionOptions, Database, EntityId, QuantizedVectorCandidates, VectorCandidates,
        VectorHit, VectorMetric,
    },
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;

fn cfg() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn lcg(state: &mut u64) -> u32 {
    *state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
    (*state >> 32) as u32
}

fn random_vector(state: &mut u64, dim: usize) -> Vec<f32> {
    let mut lanes = Vec::with_capacity(dim);
    for i in 0..dim {
        let unit = lcg(state) as f32 / u32::MAX as f32;
        let lane = unit * 2.0 - 1.0;
        lanes.push(if i == 0 { lane + 0.25 } else { lane });
    }
    lanes
}

fn assert_same_hits(page_order: &[VectorHit], per_entity: &[VectorHit]) {
    assert_eq!(
        page_order.len(),
        per_entity.len(),
        "page-order and per-entity top-k lengths differ"
    );
    for (left, right) in page_order.iter().zip(per_entity) {
        assert_eq!(left.id, right.id, "page-order and per-entity ids differ");
        assert_eq!(
            left.distance.to_bits(),
            right.distance.to_bits(),
            "page-order and per-entity distances differ for {:?}",
            left.id
        );
    }
}

#[test]
fn page_order_scan_matches_per_entity_path_on_random_corpus() {
    const N: usize = 2000;
    const DIM: usize = 8;
    const K: usize = 10;
    const EF: usize = 40;

    let temp = tempfile::tempdir().unwrap();
    let mut db = Database::create(temp.path().join("db"), cfg()).unwrap();
    let collection = db
        .create_collection(
            "vectors",
            vec![("embedding".into(), Kind::Vector(DIM))],
            CollectionOptions::default(),
        )
        .unwrap();

    let mut state = 0xE4E4_50u64;
    let mut ids = Vec::with_capacity(N);
    for i in 0..N {
        let embedding = random_vector(&mut state, DIM);
        let id = db
            .put(
                collection,
                &format!("r{i:04}"),
                &json!({ "embedding": embedding }),
            )
            .unwrap();
        ids.push(id);
        if i % 256 == 255 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    ids.sort();

    let exact = db
        .create_exact_vector_index(collection, "emb_exact", "embedding")
        .unwrap();
    db.build_index_to_ready(exact, 256).unwrap();
    let quantized = db
        .create_quantized_vector_index(collection, "emb_ann", "embedding")
        .unwrap();
    db.build_index_to_ready(quantized, 256).unwrap();
    db.commit().unwrap();

    let query = random_vector(&mut state, DIM);
    let metrics = [
        VectorMetric::Cosine,
        VectorMetric::SquaredL2,
        VectorMetric::NegativeDot,
    ];

    for metric in metrics {
        let page_order = db
            .query_exact_vector(
                exact,
                &query,
                metric,
                K,
                VectorCandidates::All,
                N,
                || false,
            )
            .unwrap();
        let per_entity = db
            .query_exact_vector(
                exact,
                &query,
                metric,
                K,
                VectorCandidates::SortedUnique(&ids),
                N,
                || false,
            )
            .unwrap();
        assert_same_hits(&page_order, &per_entity);
    }

    for metric in metrics {
        let page_order = db
            .query_quantized_vector(
                quantized,
                &query,
                metric,
                K,
                EF,
                QuantizedVectorCandidates::All,
                N,
                || false,
            )
            .unwrap();
        let per_entity = db
            .query_quantized_vector(
                quantized,
                &query,
                metric,
                K,
                EF,
                QuantizedVectorCandidates::SortedUnique(&ids),
                N,
                || false,
            )
            .unwrap();
        assert_eq!(page_order.ef, per_entity.ef);
        assert_eq!(page_order.examined, per_entity.examined);
        assert_eq!(page_order.reranked, per_entity.reranked);
        assert_same_hits(&page_order.hits, &per_entity.hits);

        let page_shortlist = db
            .query_quantized_vector(
                quantized,
                &query,
                metric,
                EF,
                EF,
                QuantizedVectorCandidates::All,
                N,
                || false,
            )
            .unwrap();
        let entity_shortlist = db
            .query_quantized_vector(
                quantized,
                &query,
                metric,
                EF,
                EF,
                QuantizedVectorCandidates::SortedUnique(&ids),
                N,
                || false,
            )
            .unwrap();
        assert_eq!(page_shortlist.reranked, entity_shortlist.reranked);
        assert_same_hits(&page_shortlist.hits, &entity_shortlist.hits);
        let _ids: &[EntityId] = &ids;
    }
}

/// A collection whose vector field MOVED still answers over all of it.
///
/// `alter_collection` mints a new layout id and rewrites no rows, so the rows
/// written before the alter keep the vector at its old ordinal and the rows
/// written after it use the new one. A sidecar-only scan that filtered on the
/// current layout's ordinal would score only the second half and return it,
/// with no error, as the whole top-k.
#[test]
fn a_moved_vector_field_still_scans_both_layouts() {
    const HALF: usize = 500;
    const DIM: usize = 8;
    const K: usize = 20;

    let temp = tempfile::tempdir().unwrap();
    let mut db = Database::create(temp.path().join("db"), cfg()).unwrap();
    let collection = db
        .create_collection(
            "vectors",
            vec![("embedding".into(), Kind::Vector(DIM))],
            CollectionOptions::default(),
        )
        .unwrap();

    // The query is (1, 0, 0, ...). The rows written BEFORE the alter lean
    // towards it and the rows written after lean away, so the whole top-k
    // lives at the old ordinal and a one-layout scan would miss all of it.
    let mut query = vec![0.0f32; DIM];
    query[0] = 1.0;
    let mut ids = Vec::with_capacity(HALF * 2);
    let mut state = 0x5EE_D111u64;

    for i in 0..HALF {
        let mut embedding = random_vector(&mut state, DIM);
        embedding[0] = 1.0 + i as f32 / HALF as f32;
        ids.push(
            db.put(
                collection,
                &format!("old{i:04}"),
                &json!({ "embedding": embedding }),
            )
            .unwrap(),
        );
    }
    db.commit().unwrap();

    let exact = db
        .create_exact_vector_index(collection, "emb_exact", "embedding")
        .unwrap();
    db.build_index_to_ready(exact, 256).unwrap();
    db.commit().unwrap();

    db.alter_collection(
        collection,
        vec![
            ("tag".into(), Kind::Text),
            ("embedding".into(), Kind::Vector(DIM)),
        ],
    )
    .unwrap();
    db.commit().unwrap();

    for i in 0..HALF {
        let mut embedding = random_vector(&mut state, DIM);
        embedding[0] = -1.0 - i as f32 / HALF as f32;
        ids.push(
            db.put(
                collection,
                &format!("new{i:04}"),
                &json!({ "tag": format!("t{i}"), "embedding": embedding }),
            )
            .unwrap(),
        );
    }
    db.commit().unwrap();
    ids.sort();

    for metric in [
        VectorMetric::Cosine,
        VectorMetric::SquaredL2,
        VectorMetric::NegativeDot,
    ] {
        let page_order = db
            .query_exact_vector(
                exact,
                &query,
                metric,
                K,
                VectorCandidates::All,
                ids.len(),
                || false,
            )
            .unwrap();
        let per_entity = db
            .query_exact_vector(
                exact,
                &query,
                metric,
                K,
                VectorCandidates::SortedUnique(&ids),
                ids.len(),
                || false,
            )
            .unwrap();
        assert_eq!(page_order.len(), K, "top-{K} over {} rows", ids.len());
        assert_same_hits(&page_order, &per_entity);
    }

    // The pre-alter half is where the answer is, so a scan that saw only the
    // current layout's ordinal would have returned a different set entirely.
    let cosine = db
        .query_exact_vector(
            exact,
            &query,
            VectorMetric::Cosine,
            K,
            VectorCandidates::All,
            ids.len(),
            || false,
        )
        .unwrap();
    let old_half: std::collections::HashSet<_> = ids[..HALF].iter().copied().collect();
    assert!(
        cosine.iter().all(|hit| old_half.contains(&hit.id)),
        "the top-{K} should all come from the rows written before the alter"
    );
}
