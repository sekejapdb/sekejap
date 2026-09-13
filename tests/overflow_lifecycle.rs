//! Retired large values must be accounted for, including while snapshots pin them.
use kernel::{
    io::IoMode,
    store::{Config, Store, SyncMode},
};

fn cfg() -> Config {
    Config {
        budget_bytes: 64 << 10,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}
fn key(id: u16) -> [u8; 3] {
    let b = id.to_be_bytes();
    [0x82, b[0], b[1]]
}
fn run(mode: u8) {
    let base = std::env::temp_dir();
    assert!(base.starts_with("<scratch>")
        || base.starts_with("<scratch>"));
    let d = tempfile::tempdir_in(base).unwrap();
    let mut s = Store::create(d.path(), cfg()).unwrap();
    for id in 1..=64 {
        s.put(&key(id), &vec![id as u8; 9000]).unwrap();
    }
    if mode == 3 {
        s.put(&key(257), b"keep outside the prefix").unwrap();
    }
    s.commit().unwrap();
    s.checkpoint().unwrap();
    let old = Store::open_snapshot(d.path(), cfg()).unwrap();
    match mode {
        0 => {
            // Replacement by another chain, then shrink back into the leaf.
            for id in 1..=64 {
                s.put(&key(id), &vec![id as u8 + 64; 12000]).unwrap();
            }
            s.commit().unwrap();
            s.checkpoint().unwrap();
            for id in 1..=64 {
                s.put(&key(id), &[id as u8; 80]).unwrap();
            }
        }
        1 => {
            for id in 1..=64 {
                assert!(s.delete(&key(id)).unwrap());
            }
        }
        2 => {
            assert_eq!(s.delete_prefix(&[0x82]).unwrap(), 64);
        }
        3 => {
            assert_eq!(s.delete_prefix(&[0x82, 0]).unwrap(), 64);
            assert_eq!(
                s.get(&key(257)).unwrap().as_deref(),
                Some(b"keep outside the prefix".as_slice())
            );
        }
        _ => unreachable!(),
    }
    s.commit().unwrap();
    s.checkpoint().unwrap();
    for id in 1..=64 {
        assert_eq!(old.get(&key(id)).unwrap(), Some(vec![id as u8; 9000]));
    }
    drop(old);
    drop(s);
    let mut s = Store::open(d.path(), cfg()).unwrap();
    // The remaining tree has no overflow values. Every physical page is
    // therefore meta, reachable tree, or recorded on the retirement freelist.
    let (_, reachable) = kernel::verify::verify_published_tree(
        &d.path().join("data"),
        IoMode::Buffered,
        s.published_root(),
        1,
    )
    .unwrap();
    let physical = std::fs::metadata(d.path().join("data")).unwrap().len() / 4096;
    assert_eq!(
        physical,
        2 + reachable + s.pool_ref().free_pages_pending() as u64,
        "mode {mode}: unreachable overflow pages leaked outside the freelist"
    );
    let mut sizes = Vec::new();
    for round in 0..8 {
        for id in 1..=64 {
            s.put(&key(id), &vec![(id + round) as u8; 9000]).unwrap();
        }
        s.commit().unwrap();
        s.checkpoint().unwrap();
        for id in 1..=64 {
            assert_eq!(
                s.get(&key(id)).unwrap(),
                Some(vec![(id + round) as u8; 9000])
            );
        }
        sizes.push(std::fs::metadata(d.path().join("data")).unwrap().len());
    }
    assert_eq!(
        sizes[3], sizes[7],
        "overflow overwrite file must plateau: {sizes:?}"
    );
}
#[test]
fn replaced_overflow_pages_are_recycled_after_snapshot_release() {
    run(0)
}
#[test]
fn deleted_overflow_pages_are_recycled_after_snapshot_release() {
    run(1)
}
#[test]
fn prefix_deleted_overflow_pages_are_recycled_after_snapshot_release() {
    run(2)
}

#[test]
fn partially_trimmed_leaf_retires_only_matching_overflow() {
    run(3)
}

#[test]
fn corrupt_old_chain_cannot_enter_the_recycling_freelist() {
    // Page CRC is valid, but the whole-value CRC is wrong. All three mutation
    // paths must refuse instead of retiring the unverified chain.
    for mode in 0..3 {
        let base = std::env::temp_dir();
        assert!(base.starts_with("<scratch>")
            || base.starts_with("<scratch>"));
        let d = tempfile::tempdir_in(base).unwrap();
        let mut s = Store::create(d.path(), cfg()).unwrap();
        s.put(&key(1), &[7; 9000]).unwrap();
        s.put(&key(257), b"healthy").unwrap();
        s.commit().unwrap();
        s.checkpoint().unwrap();
        drop(s);
        let path = d.path().join("data");
        let mut bytes = std::fs::read(&path).unwrap();
        let overflow_ids: Vec<u32> = bytes
            .chunks_exact(4096)
            .enumerate()
            .filter(|(_, p)| u16::from_le_bytes([p[6], p[7]]) == 4)
            .map(|(i, _)| i as u32)
            .collect();
        let page = bytes
            .chunks_exact_mut(4096)
            .find(|p| u16::from_le_bytes([p[6], p[7]]) == 4)
            .unwrap();
        let generation = u64::from_le_bytes(page[24..32].try_into().unwrap());
        page[46] ^= 1;
        kernel::page::seal(page, generation);
        std::fs::write(&path, &bytes).unwrap();
        let mut s = Store::open(d.path(), cfg()).unwrap();
        let result = match mode {
            0 => s.put(&key(1), b"small"),
            1 => s.delete(&key(1)).map(|_| ()),
            _ => s.delete_prefix(&[0x82, 0]).map(|_| ()),
        };
        assert!(
            matches!(result, Err(kernel::Error::Corrupt { .. })),
            "mode {mode}: {result:?}"
        );
        assert!(matches!(s.commit(), Err(kernel::Error::StorePoisoned)));
        // Existing/just-shadowed tree pages may legitimately be retired.
        // Inspect the actual page IDs, rather than assuming an empty freelist.
        let free = s.pool_ref().export_free(s.generation() + 1);
        let mut at = 24;
        while at < free.len() - 4 {
            let n = u32::from_le_bytes(free[at + 8..at + 12].try_into().unwrap()) as usize;
            at += 12;
            for p in free[at..at + n * 12].chunks_exact(12) {
                let no = u32::from_le_bytes(p[..4].try_into().unwrap());
                assert!(
                    !overflow_ids.contains(&no),
                    "corrupt overflow page {no} was retired"
                );
            }
            at += n * 12;
        }
        assert_eq!(
            s.get(&key(257)).unwrap().as_deref(),
            Some(b"healthy".as_slice())
        );
    }
}
