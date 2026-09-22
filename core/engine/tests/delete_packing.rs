//! Bounded deletion maintenance must preserve contents, snapshots and page reuse.
use kernel::{
    io::IoMode,
    store::{Config, Store, SyncMode},
};
use std::collections::BTreeMap;
fn cfg() -> Config {
    Config {
        budget_bytes: 64 << 10,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}
fn dir() -> tempfile::TempDir {
    let tmp = std::env::temp_dir();
    tempfile::tempdir().unwrap()
}
fn key(tag: u8, i: u64) -> Vec<u8> {
    let mut k = vec![tag];
    k.extend(i.to_be_bytes());
    k
}
fn structure(s: &Store) -> (u64, u64) {
    kernel::verify::verify_published_tree(
        &s.dir().join("data"),
        IoMode::Buffered,
        s.published_root(),
        1,
    )
    .unwrap()
}
#[test]
fn sparse_leaves_merge_while_old_snapshot_keeps_all_rows() {
    let d = dir();
    let mut s = Store::create(d.path(), cfg()).unwrap();
    let n = 2100;
    for tag in [0x20, 0x40, 0x60] {
        for i in 0..n {
            s.put(&key(tag, i), &vec![tag; 180]).unwrap();
        }
    }
    s.checkpoint().unwrap();
    let (_, initial) = structure(&s);
    let old = Store::open_snapshot(d.path(), cfg()).unwrap();
    for tag in [0x20, 0x40, 0x60] {
        for j in 0..n {
            let i = (j * 7919) % n;
            if i % 7 != 0 {
                assert!(s.delete(&key(tag, i)).unwrap());
            }
            if j % 300 == 299 {
                s.checkpoint().unwrap();
            }
        }
    }
    s.checkpoint().unwrap();
    let (rows, pages) = structure(&s);
    assert_eq!(rows, 900);
    for tag in [0x20, 0x40, 0x60] {
        for i in 0..n {
            assert_eq!(old.get(&key(tag, i)).unwrap(), Some(vec![tag; 180]));
            assert_eq!(
                s.get(&key(tag, i)).unwrap(),
                (i % 7 == 0).then(|| vec![tag; 180])
            );
        }
    }
    assert!(
        pages < initial / 3,
        "sparse tree retained {pages} of {initial} pages"
    );
    drop(old);
    drop(s);
    let s = Store::open(d.path(), cfg()).unwrap();
    assert_eq!(structure(&s).0, 900);
}
#[test]
fn deleting_last_rows_collapses_all_interior_levels() {
    let d = dir();
    let mut s = Store::create(d.path(), cfg()).unwrap();
    let mut keys = Vec::new();
    for i in 0u32..1800 {
        let mut k = i.to_be_bytes().to_vec();
        k.extend(vec![b'x'; 300]);
        s.put(&k, &vec![7; if i % 29 == 0 { 9000 } else { 100 }])
            .unwrap();
        keys.push(k);
    }
    s.checkpoint().unwrap();
    assert!(structure(&s).1 > 100);
    for j in 0..1800 {
        let i = (j * 7919) % 1800;
        assert!(s.delete(&keys[i]).unwrap());
        if j % 100 == 99 {
            s.checkpoint().unwrap();
        }
    }
    s.checkpoint().unwrap();
    assert_eq!(structure(&s), (0, 1));
    drop(s);
    let mut s = Store::open(d.path(), cfg()).unwrap();
    s.put(b"new", b"value").unwrap();
    s.checkpoint().unwrap();
    assert_eq!(structure(&s), (1, 1));
}
#[test]
fn fresh_id_churn_reuses_pages_instead_of_extending_forever() {
    let d = dir();
    let mut s = Store::create(d.path(), cfg()).unwrap();
    let n = 2400;
    for i in 0..n {
        s.put(&key(0x40, i), &vec![1; 200]).unwrap();
    }
    s.checkpoint().unwrap();
    let initial = s.pool_ref().page_count();
    let mut peaks = Vec::new();
    for cycle in 1..=16 {
        for i in 0..n {
            assert!(s.delete(&key(0x40, (cycle - 1) * n + i)).unwrap());
            s.put(&key(0x40, cycle * n + i), &vec![cycle as u8; 200])
                .unwrap();
            if i % 200 == 199 {
                s.checkpoint().unwrap();
            }
        }
        s.checkpoint().unwrap();
        assert_eq!(structure(&s).0, n);
        peaks.push(s.pool_ref().page_count());
    }
    assert!(
        peaks[15] <= initial * 2,
        "initial={initial}, physical={peaks:?}"
    );
    assert!(peaks[15] <= peaks[7] + 8, "no plateau: {peaks:?}");
}
#[test]
fn variable_keys_bidirectional_scans_and_reopen_match_independent_model() {
    let d = dir();
    let mut s = Store::create(d.path(), cfg()).unwrap();
    let mut model = BTreeMap::new();
    let mut seed = 91u64;
    for step in 0..6000 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let i = (seed >> 32) % 300;
        let mut k = i.to_be_bytes().to_vec();
        k.extend(vec![b'k'; if i % 11 == 0 { 700 } else { 3 }]);
        if seed % 3 == 0 {
            assert_eq!(s.delete(&k).unwrap(), model.remove(&k).is_some());
        } else {
            let v = vec![
                (step % 256) as u8;
                match i % 13 {
                    0 => 9000,
                    1 => 2500,
                    _ => 80,
                }
            ];
            s.put(&k, &v)
                .unwrap_or_else(|e| panic!("step={step} key={} value={} {e:?}", k.len(), v.len()));
            model.insert(k, v);
        }
        if step % 200 == 199 {
            s.checkpoint().unwrap();
            let actual = s
                .scan(&[])
                .unwrap()
                .map(|r| r.unwrap())
                .collect::<BTreeMap<_, _>>();
            assert_eq!(actual, model);
            let mut rev = Vec::new();
            s.scan_reverse(&[255])
                .unwrap()
                .for_each_ref(|k, v| {
                    rev.push((k.to_vec(), v.to_vec()));
                    true
                })
                .unwrap();
            assert_eq!(
                rev,
                model
                    .iter()
                    .rev()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Vec<_>>()
            );
            structure(&s);
            drop(s);
            s = Store::open(d.path(), cfg()).unwrap();
        }
    }
}

#[test]
fn large_middle_record_splits_into_three_without_losing_either_run() {
    let d = dir();
    let mut s = Store::create(d.path(), cfg()).unwrap();
    for i in 0..19 {
        s.put(&(i * 2u64).to_be_bytes(), &vec![1; 180]).unwrap();
    }
    s.checkpoint().unwrap();
    let old = Store::open_snapshot(d.path(), cfg()).unwrap();
    s.put(&19u64.to_be_bytes(), &vec![2; 2500]).unwrap();
    s.checkpoint().unwrap();
    assert_eq!(structure(&s).0, 20);
    for i in 0..19 {
        assert_eq!(
            s.get(&(i * 2u64).to_be_bytes()).unwrap(),
            Some(vec![1; 180])
        );
        assert_eq!(
            old.get(&(i * 2u64).to_be_bytes()).unwrap(),
            Some(vec![1; 180])
        );
    }
    assert_eq!(s.get(&19u64.to_be_bytes()).unwrap(), Some(vec![2; 2500]));
    assert!(old.get(&19u64.to_be_bytes()).unwrap().is_none());
}

#[test]
fn packing_crash_child() {
    use std::io::{Read, Write};
    let Ok(path) = std::env::var("E4_PACKING_CRASH_PATH") else {
        return;
    };
    let mode = std::env::var("E4_PACKING_CRASH_STAGE").unwrap();
    let mut s = Store::create(std::path::Path::new(&path), cfg()).unwrap();
    for i in 0u64..1000 {
        s.put(&i.to_be_bytes(), &vec![1; 200]).unwrap();
    }
    s.checkpoint().unwrap();
    for i in 0u64..700 {
        s.delete(&i.to_be_bytes()).unwrap();
    }
    for i in 1000u64..1400 {
        s.put(&i.to_be_bytes(), &vec![2; 200]).unwrap();
    }
    if mode != "staged" {
        s.checkpoint().unwrap();
    }
    if mode == "later-staged" {
        for i in 700u64..1400 {
            s.delete(&i.to_be_bytes()).unwrap();
        }
    }
    println!("PACKING_READY");
    std::io::stdout().flush().unwrap();
    let mut byte = [0];
    let _ = std::io::stdin().read_exact(&mut byte);
}
#[test]
fn killed_merge_writer_reopens_exactly_the_last_published_generation() {
    use std::{
        io::{BufRead, BufReader},
        process::{Command, Stdio},
        sync::mpsc,
        time::Duration,
    };
    for mode in ["staged", "committed", "later-staged"] {
        let d = dir();
        let path = d.path().join("db");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "packing_crash_child", "--nocapture"])
            .env("E4_PACKING_CRASH_PATH", &path)
            .env("E4_PACKING_CRASH_STAGE", mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if line.unwrap().contains("PACKING_READY") {
                    let _ = tx.send(());
                    return;
                }
            }
        });
        let ready = rx.recv_timeout(Duration::from_secs(30));
        let _ = child.kill();
        let status = child.wait().unwrap();
        reader.join().unwrap();
        ready.unwrap();
        assert!(!status.success());
        let s = Store::open(&path, cfg()).unwrap();
        for i in 0u64..1400 {
            let expected = if mode == "staged" {
                (i < 1000).then(|| vec![1; 200])
            } else if (700..1000).contains(&i) {
                Some(vec![1; 200])
            } else if i >= 1000 {
                Some(vec![2; 200])
            } else {
                None
            };
            assert_eq!(s.get(&i.to_be_bytes()).unwrap(), expected, "{mode} key {i}");
        }
        assert_eq!(structure(&s).0, if mode == "staged" { 1000 } else { 700 });
    }
}

#[test]
fn damaged_merge_sibling_refuses_publication_and_preserves_healthy_leaf() {
    use kernel::page::{PageKind, PageRef};
    use std::io::{Read, Seek, SeekFrom, Write};
    let d = dir();
    let mut s = Store::create(d.path(), cfg()).unwrap();
    for i in 0u64..200 {
        s.put(&i.to_be_bytes(), &vec![9; 180]).unwrap();
    }
    s.checkpoint().unwrap();
    let (healthy, damaged) = {
        let r = s.pool_ref().get(s.published_root()).unwrap();
        let p = PageRef::open_resident(&r, s.published_root()).unwrap();
        assert_eq!(p.kind(), PageKind::Interior);
        let rec = p.slot(0);
        (
            p.child0(),
            u32::from_le_bytes(rec[rec.len() - 4..].try_into().unwrap()),
        )
    };
    let keys = {
        let r = s.pool_ref().get(healthy).unwrap();
        let p = PageRef::open_resident(&r, healthy).unwrap();
        (0..p.nentries())
            .map(|i| {
                let rec = p.slot(i);
                let raw = u16::from_le_bytes(rec[..2].try_into().unwrap());
                let n = if cfg!(feature = "compact-cells") && raw & 0xf000 == 0x4000 {
                    (raw & 0x0fff) as usize
                } else { raw as usize };
                rec[2..2 + n].to_vec()
            })
            .collect::<Vec<_>>()
    };
    drop(s);
    let file = d.path().join("data");
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&file)
        .unwrap();
    f.seek(SeekFrom::Start(u64::from(damaged) * 4096 + 50))
        .unwrap();
    let mut b = [0];
    f.read_exact(&mut b).unwrap();
    b[0] ^= 1;
    f.seek(SeekFrom::Start(u64::from(damaged) * 4096 + 50))
        .unwrap();
    f.write_all(&b).unwrap();
    f.sync_all().unwrap();
    drop(f);
    let mut s = Store::open(d.path(), cfg()).unwrap();
    let old = Store::open_snapshot(d.path(), cfg()).unwrap();
    let mut refused = false;
    for k in &keys {
        if let Err(e) = s.delete(k) {
            assert!(matches!(e, kernel::Error::Corrupt { .. }));
            refused = true;
            break;
        }
    }
    assert!(refused);
    assert!(s.commit().is_err());
    assert!(s.checkpoint().is_err());
    for k in &keys {
        assert_eq!(old.get(k).unwrap(), Some(vec![9; 180]));
    }
    drop(old);
    drop(s);
    let s = Store::open(d.path(), cfg()).unwrap();
    for k in &keys {
        assert_eq!(s.get(k).unwrap(), Some(vec![9; 180]));
    }
}
