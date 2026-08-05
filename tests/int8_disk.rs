//! End-to-end test for the disk-first int8 vector index.
//! Proves: build on f32 → int8 in RAM + f32 on disk → two-stage VECTOR_NEAR
//! returns the same top-k as an exact brute-force L2 (high recall).

use sekejap::{CoreDB, VecMetric};
use std::collections::HashSet;

/// Deterministic pseudo-random vector.
fn vec_for(i: usize, dim: usize) -> Vec<f32> {
    (0..dim)
        .map(|j| {
            let seed = (i as u64)
                .wrapping_mul(6364136223846793005)
                .wrapping_add((j as u64).wrapping_mul(1442695040888963407));
            // xorshift → [0,1)
            let mut x = seed;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            ((x.wrapping_mul(0x2545F4914F6CDD1D) >> 33) as f32) / (1u64 << 31) as f32 * 100.0
        })
        .collect()
}

fn l2(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

#[test]
fn int8_disk_first_recall() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = CoreDB::open(dir.path()).expect("open disk db");

    let n = 3000usize;
    let dim = 128usize;
    let base: Vec<Vec<f32>> = (0..n).map(|i| vec_for(i, dim)).collect();

    for (i, v) in base.iter().enumerate() {
        db.put(&format!("sift/{i}"), &format!(r#"{{"_collection":"sift","_key":"{i}"}}"#)).unwrap();
        db.put_vector(&format!("sift/{i}"), "emb", v).unwrap();
    }

    // Disk-first build: graph on f32, int8 in RAM, f32 on disk.
    db.build_hnsw_index_disk("emb", 16, 200, VecMetric::L2).expect("disk build");

    let k = 10;
    let mut total_hits = 0usize;
    let queries = 50usize;
    for qi in 0..queries {
        let q = vec_for(100_000 + qi, dim);

        // Exact brute-force ground truth.
        let mut all: Vec<(usize, f32)> = base.iter().enumerate().map(|(i, v)| (i, l2(&q, v))).collect();
        all.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        let truth: HashSet<usize> = all.iter().take(k).map(|(i, _)| *i).collect();

        // Disk-first VECTOR_NEAR.
        let qstr: String = q.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",");
        let sql = format!("SELECT _key FROM sift WHERE VECTOR_NEAR(emb, [{qstr}], {k})");
        let hits = db.query(&sql).map(|s| s.collect()).unwrap_or_default();
        let got: HashSet<usize> = hits
            .iter()
            .filter_map(|h| h.slug.strip_prefix("sift/").and_then(|s| s.parse().ok()))
            .collect();

        total_hits += truth.intersection(&got).count();
    }

    let recall = total_hits as f64 / (queries * k) as f64;
    // int8 + f32 rescore should recover high recall (≥0.90 on this synthetic set).
    assert!(recall >= 0.90, "disk-first int8 recall too low: {recall:.4}");
}
