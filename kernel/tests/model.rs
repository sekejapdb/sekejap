//! Store vs BTreeMap oracle. Random ops; after each batch every model key must
//! read back, no phantoms, and a full scan must equal the model exactly (scan
//! walks the leaf chain, get descends separators -- divergence is THE bug class).
//!
//! Workload notes, each learned from a version that verified nothing:
//! - contiguous run-deletes + periodic full sweep: scattered deletes never EMPTY
//!   a page, and the known bugs need one.
//! - before each sweep, append a run ABOVE everything: the append fast path only
//!   engages when its hint is the rightmost leaf; without arming it the buggy
//!   branch was reached zero times.
//! - refill ascending after the sweep: offers the lowest key to the empty
//!   rightmost leaf, the exact wrong-accept case.
//! Verified: reverting the empty-leaf fix fails this test; reverting the
//! compact-retry fix fails it with TooLarge.

use kernel::io::IoMode;
use kernel::store::{Config, Store, SyncMode};
use std::collections::BTreeMap;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 11
    }
    fn below(&mut self, n: u64) -> u64 { self.next() % n }
}

fn check(s: &Store, m: &BTreeMap<Vec<u8>, Vec<u8>>, step: usize) {
    for (k, v) in m {
        assert_eq!(s.get(k).unwrap().as_deref(), Some(v.as_slice()), "step {step}: key {k:?}");
    }
    let scanned: Vec<_> = s.scan(&[]).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(scanned.len(), m.len(), "step {step}: scan/model count diverged");
    for ((sk, sv), (mk, mv)) in scanned.iter().zip(m.iter()) {
        assert_eq!(sk, mk, "step {step}: scan order/key");
        assert_eq!(sv, mv, "step {step}: scan value");
    }
}

#[test]
fn random_ops_agree_with_btreemap() {
    let d = tempfile::TempDir::new().unwrap();
    let cfg = Config { budget_bytes: 2 << 20, io: IoMode::Buffered, sync: SyncMode::Off };
    let mut s = Store::create(d.path(), cfg).unwrap();
    let mut m: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut rng = Rng(0x5EED);
    const KEYS: u64 = 4000;
    let (mut peak, mut sweeps) = (0usize, 0usize);

    for batch in 0..40usize {
        for _ in 0..400 {
            let k = rng.below(KEYS).to_be_bytes().to_vec();
            match rng.below(10) {
                0..=6 => {
                    let len = [1usize, 60, 220, 900][rng.below(4) as usize];
                    let v = vec![(k[7]).wrapping_add(len as u8); len];
                    s.put(&k, &v).unwrap();
                    m.insert(k, v);
                }
                7..=8 => {
                    let had = m.remove(&k).is_some();
                    assert_eq!(s.delete(&k).unwrap(), had, "batch {batch}: delete disagreed");
                }
                9 if rng.below(8) == 0 => {
                    let start = rng.below(KEYS);
                    for j in start..(start + 1 + rng.below(50)).min(KEYS) {
                        let rk = j.to_be_bytes().to_vec();
                        let had = m.remove(&rk).is_some();
                        assert_eq!(s.delete(&rk).unwrap(), had, "batch {batch}: run-delete at {j}");
                    }
                }
                _ => {
                    let absent = (KEYS + rng.below(KEYS)).to_be_bytes().to_vec();
                    assert_eq!(s.get(&absent).unwrap(), None, "batch {batch}: phantom");
                }
            }
        }
        if batch % 12 == 11 {
            for j in 0..600u64 {
                let ak = (KEYS + j).to_be_bytes().to_vec();
                s.put(&ak, &vec![b'a'; 200]).unwrap();
                m.insert(ak, vec![b'a'; 200]);
            }
            let all: Vec<_> = m.keys().cloned().collect();
            for k in all { assert!(s.delete(&k).unwrap()); m.remove(&k); }
            for j in 0..600u64 {
                let rk = j.to_be_bytes().to_vec();
                s.put(&rk, &vec![b'r'; 200]).unwrap();
                m.insert(rk, vec![b'r'; 200]);
            }
            sweeps += 1;
        }
        peak = peak.max(m.len());
        s.commit().unwrap();
        check(&s, &m, batch);
    }
    s.checkpoint().unwrap();
    drop(s);
    let cfg = Config { budget_bytes: 2 << 20, io: IoMode::Buffered, sync: SyncMode::Off };
    let s = Store::open(d.path(), cfg).unwrap();
    check(&s, &m, usize::MAX);
    assert!(peak > 1000, "model never exceeded {peak} keys; workload too shallow");
    assert!(sweeps >= 3, "only {sweeps} sweeps");
}
