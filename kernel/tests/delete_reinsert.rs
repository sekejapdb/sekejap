//! Delete everything, reinsert: scan and get must agree, always.
use kernel::io::IoMode;
use kernel::store::{Config, Store, SyncMode};

#[test]
fn delete_reinsert_keeps_tree_coherent() {
    let d = tempfile::TempDir::new().unwrap();
    let cfg = Config { budget_bytes: 32 << 20, io: IoMode::Buffered, sync: SyncMode::Off };
    let mut s = Store::create(d.path(), cfg).unwrap();
    let n = 20_000u64;
    let val = vec![b'x'; 208];
    for cycle in 0..3 {
        for i in 0..n {
            s.put(&i.to_be_bytes(), &val).unwrap_or_else(|e| panic!("cycle {cycle} put({i}): {e:?}"));
        }
        s.commit().unwrap();
        let by_get = (0..n).filter(|i| s.get(&i.to_be_bytes()).unwrap().is_some()).count() as u64;
        assert_eq!(by_get, n, "cycle {cycle}: {} keys unreachable by get", n - by_get);
        for i in 0..n {
            s.delete(&i.to_be_bytes()).unwrap();
        }
        s.commit().unwrap();
        let left = s.scan(&[]).unwrap().filter_map(Result::ok).count();
        assert_eq!(left, 0, "cycle {cycle}: {left} orphans survived delete-all");
    }
}

/// 2h A4: delete_prefix vs a BTreeMap oracle -- mixed prefixes, leaf
/// boundaries, whole-leaf clears and partial trims, then reinsertion and
/// crash replay of the single WAL record.
#[test]
fn delete_prefix_agrees_with_the_model() {
    use std::collections::BTreeMap;
    let d = tempfile::TempDir::new().unwrap();
    let cfg = Config { budget_bytes: 4 << 20, io: IoMode::Buffered, sync: SyncMode::Off };
    let mut s = Store::create(d.path(), cfg).unwrap();
    let mut m: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    // three interleaved prefixes, enough rows to span many leaves
    for i in 0..30_000u64 {
        let k = match i % 3 {
            0 => [b"aa".as_ref(), &i.to_be_bytes()].concat(),
            1 => [b"ab".as_ref(), &i.to_be_bytes()].concat(),
            _ => [b"b".as_ref(), &i.to_be_bytes()].concat(),
        };
        let v = vec![(i % 251) as u8; 40];
        s.put(&k, &v).unwrap();
        m.insert(k, v);
    }
    s.commit().unwrap();

    let n = s.delete_prefix(b"ab").unwrap();
    let expect: Vec<Vec<u8>> = m.keys().filter(|k| k.starts_with(b"ab")).cloned().collect();
    assert_eq!(n, expect.len() as u64, "removed count");
    for k in expect { m.remove(&k); }
    s.commit().unwrap();

    let got: Vec<(Vec<u8>, Vec<u8>)> = s.scan(&[]).unwrap().map(|r| r.unwrap()).collect();
    let want: Vec<(Vec<u8>, Vec<u8>)> = m.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert_eq!(got.len(), want.len(), "row count after prefix delete");
    assert_eq!(got, want, "surviving rows diverged from the model");

    // reinsert into the cleared region, then crash-replay everything
    for i in 0..500u64 {
        let k = [b"ab".as_ref(), &i.to_be_bytes()].concat();
        s.put(&k, b"back").unwrap();
        m.insert(k, b"back".to_vec());
    }
    s.commit().unwrap();
    drop(s); // crash: WAL holds puts + the delete_prefix record + more puts
    let s = Store::open(d.path(), cfg).unwrap();
    let got: Vec<(Vec<u8>, Vec<u8>)> = s.scan(&[]).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(got.len(), m.len(), "replay must reproduce the delete_prefix");
    for ((gk, _), (mk, _)) in got.iter().zip(m.iter()) {
        assert_eq!(gk, mk);
    }
}
