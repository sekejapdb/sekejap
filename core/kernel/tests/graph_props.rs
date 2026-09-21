//! Property index vs a BTreeMap oracle, and the encoders' one load-bearing
//! claim: byte order == numeric order, for i64 and f64 including negatives,
//! zero, subnormals and infinities. If an encoder breaks order, every range
//! query silently returns a fragment -- so the encoders get exhaustive-ish
//! pairs, and the index gets a randomized model test with update/delete churn.

use kernel::graph::Graph;
use kernel::io::IoMode;
use kernel::keys::{enc_f64, enc_f64_desc, enc_i64};
use kernel::store::{Config, Store, SyncMode};
use std::collections::BTreeSet;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 11
    }
    fn below(&mut self, n: u64) -> u64 { self.next() % n }
}

#[test]
fn encoders_preserve_order() {
    let is: Vec<i64> = vec![i64::MIN, -1_000_000, -1, 0, 1, 42, 1_000_000, i64::MAX];
    for w in is.windows(2) {
        assert!(enc_i64(w[0]) < enc_i64(w[1]), "i64 order broken at {:?}", w);
    }
    let fs: Vec<f64> = vec![f64::NEG_INFINITY, -1e300, -2.5, -1.0, -1e-308, -0.0,
                            0.0, 1e-308, 0.5, 1.0, 120.0, 1e300, f64::INFINITY];
    for w in fs.windows(2) {
        if w[0] == w[1] { continue; } // -0.0 == 0.0: equal is fine either way
        assert!(enc_f64(w[0]) < enc_f64(w[1]), "f64 order broken at {:?}", w);
        assert!(enc_f64_desc(w[0]) > enc_f64_desc(w[1]), "desc order broken at {:?}", w);
    }
}

#[test]
fn prop_index_agrees_with_a_model() {
    let d = tempfile::TempDir::new().unwrap();
    let cfg = Config { budget_bytes: 4 << 20, io: IoMode::Buffered, sync: SyncMode::Off };
    let mut g = Graph::new(Store::create(d.path(), cfg).unwrap()).unwrap();
    let mut rng = Rng(0xF00D);
    let price = 42u64;

    // model: set of (encoded_value, id); mirror of the keyspace
    let mut m: BTreeSet<(u64, u64)> = BTreeSet::new();
    let mut current: std::collections::HashMap<u64, u64> = Default::default();

    for id in 1..=2_000u64 {
        g.add_node(None, 7, b"n").unwrap();
        let v = enc_f64((rng.below(5000) as f64) / 10.0);
        g.set_prop(price, v, id).unwrap();
        m.insert((v, id));
        current.insert(id, v);
    }
    // churn: updates with caller-supplied old value, exactly the API contract
    for _ in 0..3_000 {
        let id = 1 + rng.below(2_000);
        let old = current[&id];
        let new = enc_f64((rng.below(5000) as f64) / 10.0);
        g.update_prop(price, old, new, id).unwrap();
        m.remove(&(old, id));
        m.insert((new, id));
        current.insert(id, new);
    }
    g.commit().unwrap();

    // range queries at many widths must agree exactly, values in order
    for _ in 0..50 {
        let lo = enc_f64(rng.below(4000) as f64 / 10.0);
        let hi = enc_f64((rng.below(1000) + 4000) as f64 / 10.0);
        let got: Vec<(u64, u64)> =
            g.prop_range(price, lo, hi).unwrap().map(|r| r.unwrap()).collect();
        let want: Vec<(u64, u64)> =
            m.range((lo, 0)..=(hi, u64::MAX)).copied().collect();
        assert_eq!(got, want, "range [{lo:#x},{hi:#x}] diverged");
    }
    // equality
    let (&(v, _), _) = (m.iter().next().unwrap(), ());
    let got: BTreeSet<u64> = g.prop_eq(price, v).unwrap().map(|r| r.unwrap().1).collect();
    let want: BTreeSet<u64> =
        m.range((v, 0)..=(v, u64::MAX)).map(|&(_, i)| i).collect();
    assert_eq!(got, want);
    // a range that matches nothing
    assert!(g.prop_range(price, u64::MAX - 1, u64::MAX).unwrap().next().is_none());

    // the zero-alloc count path must agree with the allocating iterator on the
    // same churned index, at many widths -- it re-implements the bounds, and a
    // bound error here is silent otherwise.
    let mut rng = Rng(0xC0DE);
    for _ in 0..50 {
        let lo = enc_f64(rng.below(4500) as f64 / 10.0);
        let hi = enc_f64((rng.below(1000) + 4000) as f64 / 10.0);
        let it = g.prop_range(price, lo, hi).unwrap().count();
        let fold = g.count_prop_range(price, lo, hi).unwrap();
        assert_eq!(fold, it, "fold vs iterator diverged on [{lo:#x},{hi:#x}]");
    }
    assert_eq!(g.count_prop_range(price, u64::MAX - 1, u64::MAX).unwrap(), 0);
}
