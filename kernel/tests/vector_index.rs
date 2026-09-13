//! 2g: the fingerprint similarity index, judged against independent math.
//!
//! The oracle here NEVER calls the engine's distance or encode code: exact
//! neighbours are computed with inline arithmetic over the raw vectors (the
//! 2e lesson -- an oracle that called the engine's own kernel let a
//! cosine-normalisation mutation survive).

use kernel::graph::{Graph, Metric};
use kernel::io::IoMode;
use kernel::store::{Config, Store, SyncMode};

/// One vector field for these tests; to the kernel a field is an
/// opaque u64, so any constant names it.
const VF: u64 = 1;

fn cfg() -> Config {
    Config { budget_bytes: 16 << 20, io: IoMode::Buffered, sync: SyncMode::Off }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 11
    }
    /// Roughly Gaussian via sum of uniforms -- shape matters (real
    /// embeddings are dense and centred), exact distribution does not.
    fn gauss(&mut self) -> f32 {
        let mut s = 0f32;
        for _ in 0..4 { s += (self.next() % 1000) as f32 / 999.0 - 0.5; }
        s
    }
}

fn dataset(n: u64, dim: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut r = Rng(seed);
    (0..n).map(|_| (0..dim).map(|_| r.gauss()).collect()).collect()
}

/// The HOSTILE dataset: 90% zeros, a few large spikes. Per-coordinate
/// quantization without a good mixing rotation fails on exactly this shape
/// (all information in a handful of coordinates the uniform ladder cannot
/// resolve). Gaussian data alone would let a weakened rotation pass.
fn dataset_sparse(n: u64, dim: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut r = Rng(seed);
    (0..n).map(|_| {
        let mut v = vec![0f32; dim];
        for _ in 0..dim / 10 {
            let j = (r.next() as usize) % dim;
            v[j] = r.gauss() * 8.0;
        }
        v
    }).collect()
}

/// Exact top-k by INDEPENDENT inline math. Lower score = nearer.
fn exact_topk(data: &[Vec<f32>], q: &[f32], k: usize, metric: Metric) -> Vec<u64> {
    let mut scored: Vec<(f32, u64)> = data.iter().enumerate().map(|(i, v)| {
        let dot: f32 = v.iter().zip(q).map(|(a, b)| a * b).sum();
        let n2: f32 = v.iter().map(|a| a * a).sum();
        let qn: f32 = q.iter().map(|a| a * a).sum::<f32>().sqrt();
        let l1: f32 = v.iter().zip(q).map(|(a, b)| (a - b).abs()).sum();
        let s = match metric {
            Metric::L2 => n2 - 2.0 * dot, // + |q|^2, constant
            Metric::Dot => -dot,
            Metric::Cosine => if n2 > 0.0 { -dot / (n2.sqrt() * qn) } else { 0.0 },
            Metric::L1 => l1,
        };
        (s, (i + 1) as u64)
    }).collect();
    scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    scored.truncate(k);
    scored.into_iter().map(|(_, id)| id).collect()
}

#[test]
fn nearest_recall_against_exact_bruteforce() {
    recall_case(dataset(20_000, 128, 0x5EED), "gaussian");
}

#[test]
fn nearest_recall_on_hostile_sparse_vectors() {
    recall_case(dataset_sparse(20_000, 128, 0xBAD), "sparse-spiky");
}

fn recall_case(data: Vec<Vec<f32>>, label: &str) {
    const N: u64 = 20_000;
    const K: usize = 10;
    let d = tempfile::TempDir::new().unwrap();
    let s = Store::create(d.path(), cfg()).unwrap();
    let mut g = Graph::new(s).unwrap();
    for (i, v) in data.iter().enumerate() {
        g.set_vec(VF, i as u64 + 1, v).unwrap();
    }
    g.commit().unwrap();

    for metric in [Metric::L2, Metric::Dot, Metric::Cosine, Metric::L1] {
        let mut hit = 0usize;
        let mut total = 0usize;
        let mut rq = Rng(0xABCD);
        for t in 0..20 {
            // Query = a dataset vector plus noise: neighbourhoods exist.
            let base = &data[(t * 997) % N as usize];
            let q: Vec<f32> = base.iter().map(|x| x + 0.1 * rq.gauss()).collect();
            let truth = exact_topk(&data, &q, K, metric);
            let got: Vec<u64> = g.nearest(VF, &q, K, metric, 8).unwrap()
                .into_iter().map(|(id, _)| id).collect();
            hit += got.iter().filter(|id| truth.contains(id)).count();
            total += K;
        }
        let recall = hit as f64 / total as f64;
        eprintln!("recall@10 {label} {metric:?} = {recall:.3}");
        assert!(recall >= 0.90,
                "{label} {metric:?}: recall@10 = {recall:.3}, below the 0.90 floor");
    }
}

#[test]
fn fingerprints_are_deterministic_and_survive_reopen() {
    let d = tempfile::TempDir::new().unwrap();
    let data = dataset(500, 96, 7);
    {
        let s = Store::create(d.path(), cfg()).unwrap();
        let mut g = Graph::new(s).unwrap();
        for (i, v) in data.iter().enumerate() { g.set_vec(VF, i as u64 + 1, v).unwrap(); }
        g.commit().unwrap();
        // crash: no checkpoint
    }
    let s = Store::open(d.path(), cfg()).unwrap();
    let g = Graph::new(s).unwrap();
    // The recipe is (dim, seed) only: re-encoding every stored vector must
    // reproduce the stored fingerprint byte for byte after the crash.
    let q: Vec<f32> = data[3].clone();
    let got = g.nearest(VF, &q, 5, Metric::L2, 8).unwrap();
    assert_eq!(got[0].0, 4, "the vector itself must be its own nearest neighbour");
}

#[test]
fn a_deleted_vector_leaves_the_index_too() {
    let d = tempfile::TempDir::new().unwrap();
    let data = dataset(1_000, 64, 3);
    let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
    for (i, v) in data.iter().enumerate() { g.set_vec(VF, i as u64 + 1, v).unwrap(); }
    g.commit().unwrap();
    let q = data[7].clone();
    assert_eq!(g.nearest(VF, &q, 1, Metric::L2, 8).unwrap()[0].0, 8);
    assert!(g.delete_vec(VF, 8).unwrap());
    g.commit().unwrap();
    assert_ne!(g.nearest(VF, &q, 1, Metric::L2, 8).unwrap()[0].0, 8,
               "a deleted vector must vanish from search");
    assert_eq!(g.get_vec(VF, 8).unwrap(), None);
    // crash-consistency: both rows go or neither (delete then crash pre-commit)
    let mut g2 = {
        drop(g);
        Graph::new(Store::open(d.path(), cfg()).unwrap()).unwrap()
    };
    assert!(g2.delete_vec(VF, 9).unwrap());
    drop(g2); // crash: uncommitted
    let g3 = Graph::new(Store::open(d.path(), cfg()).unwrap()).unwrap();
    assert!(g3.get_vec(VF, 9).unwrap().is_some(),
            "uncommitted delete must be undone for BOTH rows");
    assert_eq!(g3.nearest(VF, &data[8], 1, Metric::L2, 8).unwrap()[0].0, 9,
               "the fingerprint must still be searchable after the rollback");
}

#[test]
fn parallel_nearest_agrees_with_serial() {
    let d = tempfile::TempDir::new().unwrap();
    let data = dataset(20_000, 128, 0x7777);
    let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
    for (i, v) in data.iter().enumerate() { g.set_vec(VF, i as u64 + 1, v).unwrap(); }
    g.commit().unwrap();
    g.checkpoint().unwrap(); // parallel readers see the published generation
    for t in [2, 4] {
        for q in 0..5u64 {
            let qv: Vec<f32> = data[(q as usize * 331) % 20_000].iter()
                .map(|x| x + 0.05).collect();
            let a = g.nearest(VF, &qv, 10, Metric::L2, 16).unwrap();
            let b = g.nearest_par(VF, &qv, 10, Metric::L2, 16, t).unwrap();
            assert_eq!(a, b, "threads={t} q={q}: parallel result diverged");
        }
    }
}

#[test]
fn a_snapshot_reader_searches_vectors_too() {
    let d = tempfile::TempDir::new().unwrap();
    let data = dataset(2_000, 64, 11);
    let mut w = {
        let s = Store::create(d.path(), cfg()).unwrap();
        let mut g = Graph::new(s).unwrap();
        for (i, v) in data.iter().enumerate() { g.set_vec(VF, i as u64 + 1, v).unwrap(); }
        g.commit().unwrap();
        g
    };
    w.checkpoint().unwrap();

    let r = Store::open_snapshot(d.path(), cfg()).unwrap();
    let gr = Graph::new(r).unwrap();
    let q = &data[42];
    let from_reader = gr.nearest(VF, q, 10, Metric::L2, 8).unwrap();
    let from_writer = w.nearest(VF, q, 10, Metric::L2, 8).unwrap();
    assert_eq!(from_reader, from_writer,
               "reader and writer must agree at the same generation");
    assert_eq!(from_reader[0].0, 43);
}

/// 2k: the navigation tier vs brute force. Fold builds the graph; the
/// walked answer must reach the same neighbourhoods the oracle finds.
#[test]
fn nav_recall_against_bruteforce() {
    let d = tempfile::TempDir::new().unwrap();
    let data = dataset(10_000, 64, 0xA5A5);
    let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
    for (i, v) in data.iter().enumerate() { g.set_vec(VF, i as u64 + 1, v).unwrap(); }
    g.commit().unwrap();
    let n = g.fold_nav(VF).unwrap();
    assert_eq!(n, 10_000, "every vector must be wired");

    let mut hit = 0usize; let mut total = 0usize;
    let mut rq = Rng(0x77);
    for t in 0..15 {
        let base = &data[(t * 613) % 10_000];
        let q: Vec<f32> = base.iter().map(|x| x + 0.1 * rq.gauss()).collect();
        let truth = exact_topk(&data, &q, 10, Metric::L2);
        let got: Vec<u64> = g.nearest_nav(VF, &q, 10, Metric::L2, 8).unwrap()
            .into_iter().map(|(id, _)| id).collect();
        hit += got.iter().filter(|id| truth.contains(id)).count();
        total += 10;
    }
    let recall = hit as f64 / total as f64;
    eprintln!("nav recall@10 = {recall:.3}");
    assert!(recall >= 0.90, "nav recall {recall:.3} below floor");
}

/// Fresh vectors above the watermark are served by the head scan and
/// merged with the walk -- searchable instantly, no rebuild.
#[test]
fn nav_head_merges_with_the_walk() {
    let d = tempfile::TempDir::new().unwrap();
    let data = dataset(2_000, 64, 0xFEED);
    let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
    for (i, v) in data.iter().enumerate() { g.set_vec(VF, i as u64 + 1, v).unwrap(); }
    g.commit().unwrap();
    g.fold_nav(VF).unwrap();
    // a brand-new vector, identical to the query: must be rank 1 WITHOUT
    // any fold having wired it
    let q: Vec<f32> = data[500].iter().map(|x| x * 1.001).collect();
    g.set_vec(VF, 9_999, &q).unwrap();
    g.commit().unwrap();
    let got = g.nearest_nav(VF, &q, 3, Metric::L2, 8).unwrap();
    assert_eq!(got[0].0, 9_999, "the unfolded head vector must win: {got:?}");
    // after the fold it still wins, now through the graph
    g.fold_nav(VF).unwrap();
    let got = g.nearest_nav(VF, &q, 3, Metric::L2, 8).unwrap();
    assert_eq!(got[0].0, 9_999);
}
