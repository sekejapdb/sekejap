//! Overflow chains: model test, blast radius, and the bulk path.

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

fn cfg() -> Config {
    Config { budget_bytes: 4 << 20, io: IoMode::Buffered, sync: SyncMode::Off }
}

/// Value with content derived from (id, len) so any truncation, crossed chain
/// or byte damage changes bytes a plain equality check will see.
fn val(id: u64, len: usize) -> Vec<u8> {
    let seed = id.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_le_bytes();
    seed.iter().copied().cycle().take(len).collect()
}

#[test]
fn mixed_sizes_agree_with_a_model_across_reopen() {
    let d = tempfile::TempDir::new().unwrap();
    let mut s = Store::create(d.path(), cfg()).unwrap();
    let mut m: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut rng = Rng(0x0F10);

    for i in 0..2_000u64 {
        // sizes straddle every boundary: tiny, near-threshold both sides,
        // one-chunk, multi-chunk, and big
        let len = [16usize, 4_000, 4_060, 4_100, 8_192, 20_000, 65_000][rng.below(7) as usize];
        let v = val(i, len);
        let k = i.to_be_bytes().to_vec();
        s.put(&k, &v).unwrap();
        m.insert(k, v);
    }
    // churn: overwrite big-with-small and small-with-big
    for i in 0..500u64 {
        let id = rng.below(2_000);
        let len = [8usize, 30_000][rng.below(2) as usize];
        let v = val(id + 7_777, len);
        let k = id.to_be_bytes().to_vec();
        s.put(&k, &v).unwrap();
        m.insert(k, v);
    }
    s.commit().unwrap();

    for (k, v) in &m {
        assert_eq!(s.get(k).unwrap().as_deref(), Some(v.as_slice()), "get {k:?}");
    }
    // scan must resolve chains and keep order
    let scanned: Vec<_> = s.scan(&[]).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(scanned.len(), m.len());
    for ((sk, sv), (mk, mv)) in scanned.iter().zip(m.iter()) {
        assert_eq!(sk, mk);
        assert_eq!(sv, mv, "scan value for {sk:?}");
    }
    // and the zero-alloc fold path resolves them identically
    let mut n = 0usize;
    s.scan(&[]).unwrap().for_each_ref(|k, v| {
        assert_eq!(m.get(k).map(|x| x.as_slice()), Some(v));
        n += 1; true
    }).unwrap();
    assert_eq!(n, m.len());

    s.checkpoint().unwrap();
    drop(s);
    let s = Store::open(d.path(), cfg()).unwrap();
    for (k, v) in m.iter().take(200) {
        assert_eq!(s.get(k).unwrap().as_deref(), Some(v.as_slice()), "post-reopen {k:?}");
    }
}

#[test]
fn bulk_load_spills_and_round_trips() {
    let d = tempfile::TempDir::new().unwrap();
    let mut s = Store::create(d.path(), cfg()).unwrap();
    let n = 3_000u64;
    s.bulk_load((0..n).map(|i| {
        let len = if i % 5 == 0 { 10_000 } else { 100 };
        (i.to_be_bytes().to_vec(), val(i, len))
    })).unwrap();
    for i in (0..n).step_by(97) {
        let len = if i % 5 == 0 { 10_000 } else { 100 };
        assert_eq!(s.get(&i.to_be_bytes()).unwrap().as_deref(),
                   Some(val(i, len).as_slice()), "bulk row {i}");
    }
    let count = s.scan(&[]).unwrap().count();
    assert_eq!(count as u64, n);
}

/// Blast radius: damage inside ONE chain refuses THAT record and leaves every
/// other record readable.
#[test]
fn a_damaged_chain_refuses_one_record_only() {
    let d = tempfile::TempDir::new().unwrap();
    {
        let mut s = Store::create(d.path(), cfg()).unwrap();
        for i in 0..50u64 {
            s.put(&i.to_be_bytes(), &val(i, 20_000)).unwrap();
        }
        s.commit().unwrap();
        s.checkpoint().unwrap();
    }
    // find an Overflow page and flip one content byte
    let path = d.path().join("data");
    let mut bytes = std::fs::read(&path).unwrap();
    let ps = kernel::page::PAGE_SIZE;
    let mut hit = None;
    for p in (1..bytes.len() / ps).rev() {
        // kind field is at offset 6 (u16) in the header; Overflow = 4
        let off = p * ps;
        if u16::from_le_bytes([bytes[off + 6], bytes[off + 7]]) == 4 {
            bytes[off + 200] ^= 0xFF;
            hit = Some(p);
            break;
        }
    }
    let hit = hit.expect("no overflow page found; fixture broken");
    std::fs::write(&path, &bytes).unwrap();

    let s = Store::open(d.path(), cfg()).unwrap();
    let mut refused = 0;
    let mut ok = 0;
    for i in 0..50u64 {
        match s.get(&i.to_be_bytes()) {
            Ok(Some(v)) => { assert_eq!(v, val(i, 20_000), "silent corruption on {i}"); ok += 1; }
            Ok(None) => panic!("row {i} vanished"),
            Err(_) => refused += 1,
        }
    }
    assert_eq!(refused, 1, "damage in page {hit} must refuse exactly its own record");
    assert_eq!(ok, 49);
}

/// The whole-value crc's ONE job: two chains of individually-VALID pages,
/// crossed at the marker level, must be refused. Page checksums cannot see
/// this -- every page is intact; only the assembled value is wrong. Same-length
/// values so the total-length check cannot save us either.
#[test]
fn crossed_chains_of_valid_pages_are_refused() {
    let d = tempfile::TempDir::new().unwrap();
    {
        let mut s = Store::create(d.path(), cfg()).unwrap();
        s.put(b"aa", &val(1, 20_000)).unwrap();
        s.put(b"bb", &val(2, 20_000)).unwrap();
        s.commit().unwrap();
        s.checkpoint().unwrap();
    }
    // swap the two markers' head pages inside the leaf records on disk
    let path = d.path().join("data");
    let mut bytes = std::fs::read(&path).unwrap();
    let ps = kernel::page::PAGE_SIZE;
    let mut heads: Vec<(usize, u32)> = Vec::new();
    for p in 1..bytes.len() / ps {
        let off = p * ps;
        if u16::from_le_bytes([bytes[off + 6], bytes[off + 7]]) != 2 { continue; } // Leaf
        // find marker records: scan raw for vlen sentinel after 2-byte keys
        // records pack at the page TAIL; a bound of ps-20 missed one at 4078
        for i in 0..ps - 15 {
            let o = off + i;
            // rec: [klen=2][k0 k1][vlen=0xFFFF][12B marker]
            if bytes[o] == 2 && bytes[o + 1] == 0
                && bytes[o + 4] == 0xFF && bytes[o + 5] == 0xFF
                && (bytes[o + 2] == b'a' || bytes[o + 2] == b'b')
                && bytes[o + 2] == bytes[o + 3]
            {
                heads.push((o + 6 + 4, u32::from_le_bytes(bytes[o + 10..o + 14].try_into().unwrap())));
            }
        }
    }
    assert_eq!(heads.len(), 2, "expected exactly two marker records, found {}", heads.len());
    let (o1, h1) = heads[0];
    let (o2, h2) = heads[1];
    assert_ne!(h1, h2);
    bytes[o1..o1 + 4].copy_from_slice(&h2.to_le_bytes());
    bytes[o2..o2 + 4].copy_from_slice(&h1.to_le_bytes());
    // leaf page checksums now stale -- reseal both pages the way the pool does,
    // so ONLY the crossed chains distinguish right from wrong
    for p in 1..bytes.len() / ps {
        let off = p * ps;
        if u16::from_le_bytes([bytes[off + 6], bytes[off + 7]]) == 2 {
            kernel::page::seal(&mut bytes[off..off + ps], 7);
        }
    }
    std::fs::write(&path, &bytes).unwrap();

    let s = Store::open(d.path(), cfg()).unwrap();
    for k in [b"aa".as_slice(), b"bb"] {
        match s.get(k) {
            Err(_) => {}
            Ok(v) => panic!("crossed chain for {k:?} served a value ({:?} bytes) -- \
                             the whole-value crc is not doing its one job",
                            v.map(|x| x.len())),
        }
    }
}
