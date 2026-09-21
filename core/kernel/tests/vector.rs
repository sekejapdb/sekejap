//! 2e-vector: model, dim enforcement, rescore vs brute-force oracle, crash.

use kernel::graph::{Graph, Metric};
use kernel::io::IoMode;
use kernel::store::{Config, Store, SyncMode};

/// One vector field for these tests; to the kernel a field is an
/// opaque u64, so any constant names it.
const VF: u64 = 1;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 11
    }
    fn f(&mut self) -> f32 { (self.next() % 2000) as f32 / 1000.0 - 1.0 }
}

fn cfg() -> Config {
    Config { budget_bytes: 8 << 20, io: IoMode::Buffered, sync: SyncMode::Off }
}

fn vecs(rng: &mut Rng, n: u64, dim: usize) -> Vec<Vec<f32>> {
    (0..n).map(|_| (0..dim).map(|_| rng.f()).collect()).collect()
}

#[test]
fn vectors_round_trip_including_chained() {
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
    let mut rng = Rng(0x5EC1u64);
    let dim = 1536; // 6144 B -> a 2-page overflow chain per vector
    let vs = vecs(&mut rng, 300, dim);
    for (i, v) in vs.iter().enumerate() {
        g.set_vec(VF, i as u64 + 1, v).unwrap();
    }
    // overwrite churn
    let vs2 = vecs(&mut rng, 100, dim);
    for (i, v) in vs2.iter().enumerate() {
        g.set_vec(VF, i as u64 + 1, v).unwrap();
    }
    g.commit().unwrap();
    for i in 0..300u64 {
        let want = if i < 100 { &vs2[i as usize] } else { &vs[i as usize] };
        assert_eq!(g.get_vec(VF, i + 1).unwrap().as_deref(), Some(want.as_slice()), "vec {i}");
    }
    assert_eq!(g.get_vec(VF, 9999).unwrap(), None);

    // crash: chained vectors rebuilt from the WAL
    g.checkpoint().unwrap();
    drop(g);
    let g = Graph::new(Store::open(d.path(), cfg()).unwrap()).unwrap();
    assert_eq!(g.vec_dim(VF), dim as u64, "dim did not survive reopen");
    assert_eq!(g.get_vec(VF, 1).unwrap().as_deref(), Some(vs2[0].as_slice()));
}

#[test]
fn wrong_dimension_is_refused() {
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
    g.set_vec(VF, 1, &[1.0; 768]).unwrap();
    assert!(g.set_vec(VF, 2, &[1.0; 1536]).is_err(), "a 1536-dim row in a 768-dim field");
    assert!(g.set_vec(VF, 3, &[1.0; 767]).is_err());
    g.set_vec(VF, 4, &[2.0; 768]).unwrap();
}

#[test]
fn rescore_matches_brute_force_all_metrics() {
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
    let mut rng = Rng(0xD15C0);
    let dim = 64;
    let n = 5_000u64;
    let vs = vecs(&mut rng, n, dim);
    for (i, v) in vs.iter().enumerate() {
        g.set_vec(VF, i as u64 + 1, v).unwrap();
    }
    g.commit().unwrap();
    let q: Vec<f32> = (0..dim).map(|_| rng.f()).collect();
    let candidates: Vec<u64> = (1..=n).collect();

    for metric in [Metric::L2, Metric::Cosine, Metric::Dot, Metric::L1] {
        let got = g.rescore(VF, &candidates, &q, metric, 10).unwrap();
        // Brute-force oracle with INDEPENDENT math -- calling the engine's own
        // distance function here made the test blind to metric-definition bugs
        // (cosine-without-normalisation passed, because both sides changed).
        let dist = |v: &[f32]| -> f32 {
            let dot: f32 = v.iter().zip(&q).map(|(a, b)| a * b).sum();
            match metric {
                Metric::L2 => v.iter().zip(&q).map(|(a, b)| (a - b) * (a - b)).sum(),
                Metric::L1 => v.iter().zip(&q).map(|(a, b)| (a - b).abs()).sum(),
                Metric::Dot => -dot,
                Metric::Cosine => {
                    let na: f32 = v.iter().map(|a| a * a).sum::<f32>().sqrt();
                    let nb: f32 = q.iter().map(|b| b * b).sum::<f32>().sqrt();
                    -(dot / (na * nb).max(f32::MIN_POSITIVE))
                }
            }
        };
        let mut all: Vec<(u64, f32)> = vs.iter().enumerate()
            .map(|(i, v)| (i as u64 + 1, dist(v)))
            .collect();
        all.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        all.truncate(10);
        assert_eq!(got, all, "{metric:?} top-10 diverged from brute force");
    }

    // missing vectors skip, not fail
    let sparse: Vec<u64> = vec![1, 999_999, 2];
    assert_eq!(g.rescore(VF, &sparse, &q, Metric::L2, 10).unwrap().len(), 2);
}
