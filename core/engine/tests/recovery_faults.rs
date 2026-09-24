//! Deterministic, independent corruption oracles. All data stays under TMPDIR.
use kernel::{
    io::IoMode,
    store::{Config, Store, SyncMode},
};
use std::{fs, path::Path};
fn cfg() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}
fn temp() -> tempfile::TempDir {
    let p = std::env::temp_dir();
    tempfile::tempdir_in(p).unwrap()
}
fn key(id: u16) -> Vec<u8> {
    let mut k = vec![0x82];
    k.extend_from_slice(&id.to_be_bytes());
    k
}
fn make(p: &Path, n: u16, large: bool) {
    let mut s = Store::create(p, cfg()).unwrap();
    for id in 1..=n {
        s.put(
            &key(id),
            &vec![(id % 251) as u8; if large && id == 7 { 9000 } else { 80 }],
        )
        .unwrap();
    }
    s.commit().unwrap();
    s.checkpoint().unwrap();
}
fn alter(p: &Path, kind: u16, body: usize, mask: u8) -> u32 {
    let file = p.join("data");
    let mut b = fs::read(&file).unwrap();
    let no = b
        .chunks_exact(4096)
        .position(|p| {
            u16::from_le_bytes([p[6], p[7]]) == kind
                && (kind != 2 || u16::from_le_bytes([p[10], p[11]]) > 0)
        })
        .unwrap();
    b[no * 4096 + body] ^= mask;
    fs::write(file, b).unwrap();
    no as u32
}

#[test]
fn overflow_bad_page_salvages_other_rows() {
    let d = temp();
    make(d.path(), 50, true);
    alter(d.path(), 4, 50, 1);
    let output = temp();
    let before = source_bytes(d.path());
    let r = kernel::recover::recover_to(d.path(), &output.path().join("repair"), cfg())
        .expect("one bad value must not prevent unrelated salvage");
    assert_eq!(r.entries_recovered, 49);
    assert_eq!(r.known_value_losses, 1);
    assert!(fs::read_to_string(&r.loss_journal)
        .unwrap()
        .contains("key\t"));
    assert!(fs::read_to_string(&r.loss_journal)
        .unwrap()
        .contains("\t820007\t"));
    assert_eq!(source_bytes(d.path()), before);
    let s = Store::open(&r.database, cfg()).unwrap();
    for id in 1..=50 {
        if id == 7 {
            assert_eq!(s.get(&key(id)).unwrap(), None);
        } else {
            assert_eq!(s.get(&key(id)).unwrap(), Some(vec![(id % 251) as u8; 80]));
        }
    }
}

#[test]
fn damaged_leaf_kind_cannot_hide_loss() {
    let d = temp();
    make(d.path(), 500, false);
    let page = alter(d.path(), 2, 6, 1);
    let output = temp();
    let before = source_bytes(d.path());
    let r = kernel::recover::recover_to(d.path(), &output.path().join("repair"), cfg()).unwrap();
    assert_eq!(source_bytes(d.path()), before);
    assert!(
        fs::read_to_string(&r.loss_journal)
            .unwrap()
            .contains(&format!("extent\t{page}\t")),
        "damaged kind cannot make a lost page vanish: {r:?}"
    );
}

#[test]
fn repair_never_resurrects_deleted_rows() {
    let d = temp();
    make(d.path(), 500, false);
    let mut s = Store::open(d.path(), cfg()).unwrap();
    for id in 100..=199 {
        s.delete(&key(id)).unwrap();
    }
    s.commit().unwrap();
    s.checkpoint().unwrap();
    drop(s);
    let output = temp();
    let before = source_bytes(d.path());
    let r = kernel::recover::recover_to(d.path(), &output.path().join("repair"), cfg()).unwrap();
    assert_eq!(source_bytes(d.path()), before);
    let s = Store::open(&r.database, cfg()).unwrap();
    for id in 1..=500 {
        assert_eq!(
            s.get(&key(id)).unwrap(),
            if (100..=199).contains(&id) {
                None
            } else {
                Some(vec![(id % 251) as u8; 80])
            },
            "repair resurrected or lost key {id}"
        );
    }
}

fn source_bytes(p: &Path) -> Vec<(String, Vec<u8>)> {
    ["data", "wal", "free"]
        .into_iter()
        .filter_map(|name| fs::read(p.join(name)).ok().map(|b| (name.to_owned(), b)))
        .collect()
}

fn repair(p: &Path, out: &Path) -> kernel::recover::SafeRecoveryReport {
    let before = source_bytes(p);
    let r = kernel::recover::recover_to(p, &out.join("repair"), cfg()).unwrap();
    assert_eq!(source_bytes(p), before);
    assert!(r.destination.join("COMPLETE").is_file());
    r
}

fn mutate_overflow(p: &Path, mutation: impl FnOnce(&mut [u8], &[u32])) {
    let data = p.join("data");
    let mut bytes = fs::read(&data).unwrap();
    let pages: Vec<u32> = bytes
        .chunks_exact(4096)
        .enumerate()
        .filter(|(_, b)| b[6] == 4)
        .map(|(i, _)| i as u32)
        .collect();
    mutation(&mut bytes, &pages);
    fs::write(data, bytes).unwrap();
}

#[test]
fn overflow_crossed_chain_is_one_known_loss() {
    let d = temp();
    make(d.path(), 50, true);
    let mut s = Store::open(d.path(), cfg()).unwrap();
    s.put(&key(8), &vec![88; 9000]).unwrap();
    s.commit().unwrap();
    s.checkpoint().unwrap();
    drop(s);
    // Two individually valid chains; cross key 7's next pointer to key 8's tail.
    // Reseal the modified page so only whole-value validation catches this.
    mutate_overflow(d.path(), |b, pages| {
        assert_eq!(pages.len(), 6);
        let p = &mut b[pages[0] as usize * 4096..(pages[0] as usize + 1) * 4096];
        p[40..44].copy_from_slice(&pages[4].to_le_bytes());
        let gen = u64::from_le_bytes(p[24..32].try_into().unwrap());
        kernel::page::seal(p, gen);
    });
    let out = temp();
    let r = repair(d.path(), out.path());
    assert_eq!(r.known_value_losses, 1);
    assert_eq!(r.entries_recovered, 49);
    let s = Store::open(&r.database, cfg()).unwrap();
    assert_eq!(s.get(&key(7)).unwrap(), None);
    assert_eq!(s.get(&key(8)).unwrap(), Some(vec![88; 9000]));
}

#[test]
fn overflow_cycle_and_wrong_identity_are_contained() {
    for cycle in [true, false] {
        let d = temp();
        make(d.path(), 50, true);
        mutate_overflow(d.path(), |b, pages| {
            let p = &mut b[pages[0] as usize * 4096..(pages[0] as usize + 1) * 4096];
            if cycle {
                p[40..44].copy_from_slice(&pages[0].to_le_bytes());
            } else {
                p[12..16].copy_from_slice(&999999u32.to_le_bytes());
            }
            let gen = u64::from_le_bytes(p[24..32].try_into().unwrap());
            kernel::page::seal(p, gen);
        });
        let out = temp();
        let r = repair(d.path(), out.path());
        assert_eq!(r.entries_recovered, 49);
        assert_eq!(r.known_value_losses, 1);
    }
}

#[test]
fn overflow_missing_tail_is_one_known_loss() {
    let d = temp();
    make(d.path(), 50, true);
    mutate_overflow(d.path(), |b, pages| {
        let p = &mut b[pages[0] as usize * 4096..(pages[0] as usize + 1) * 4096];
        p[40..44].copy_from_slice(&999999u32.to_le_bytes());
        let gen = u64::from_le_bytes(p[24..32].try_into().unwrap());
        kernel::page::seal(p, gen);
    });
    let out = temp();
    let r = repair(d.path(), out.path());
    assert_eq!(r.entries_recovered, 49);
    assert_eq!(r.known_value_losses, 1);
}

#[test]
fn rootless_repair_does_not_claim_current_membership() {
    let d = temp();
    make(d.path(), 500, false);
    let mut s = Store::open(d.path(), cfg()).unwrap();
    for id in 100..=199 {
        s.delete(&key(id)).unwrap();
    }
    s.commit().unwrap();
    s.checkpoint().unwrap();
    let root = s.published_root();
    drop(s);
    let data = d.path().join("data");
    let mut b = fs::read(&data).unwrap();
    b[root as usize * 4096 + 50] ^= 1;
    fs::write(data, b).unwrap();
    let out = temp();
    let r = repair(d.path(), out.path());
    assert_eq!(
        r.entries_recovered, 0,
        "rootless cells must not be published as current"
    );
    assert!(r.unknown_extents > 0);
    assert!(r.raw_candidate_records >= 400);
    assert!(r.candidate_archive.is_some());
    let s = Store::open(&r.database, cfg()).unwrap();
    for id in 100..=199 {
        assert_eq!(s.get(&key(id)).unwrap(), None);
    }
}

#[test]
fn malformed_cell_does_not_abort_unrelated_salvage() {
    let d = temp();
    make(d.path(), 500, false);
    let data = d.path().join("data");
    let mut b = fs::read(&data).unwrap();
    let no = b
        .chunks_exact(4096)
        .position(|p| p[6] == 2 && p[10] > 0)
        .unwrap();
    let p = &mut b[no * 4096..(no + 1) * 4096];
    let off = u16::from_le_bytes(p[40..42].try_into().unwrap()) as usize;
    // Remove compact sentinel; generic key length now exceeds the slot.
    p[off..off + 2].copy_from_slice(&65534u16.to_le_bytes());
    let gen = u64::from_le_bytes(p[24..32].try_into().unwrap());
    kernel::page::seal(p, gen);
    fs::write(data, b).unwrap();
    let out = temp();
    let r = repair(d.path(), out.path());
    assert!(r.entries_recovered > 400 && r.entries_recovered < 500);
    assert!(r.unknown_extents > 0);
}

#[test]
fn repair_destination_failure_keeps_source() {
    let d = temp();
    make(d.path(), 50, false);
    let out = temp();
    let dest = out.path().join("occupied");
    fs::create_dir(&dest).unwrap();
    fs::write(dest.join("evidence"), b"retain").unwrap();
    let before = source_bytes(d.path());
    assert!(kernel::recover::recover_to(d.path(), &dest, cfg()).is_err());
    assert_eq!(source_bytes(d.path()), before);
    assert_eq!(fs::read(dest.join("evidence")).unwrap(), b"retain");
    assert!(!dest.join("COMPLETE").exists());
}

#[test]
fn repair_rejects_source_aliases_and_active_writers() {
    let d = temp();
    make(d.path(), 50, false);
    assert!(kernel::recover::recover_to(d.path(), &d.path().join("inside"), cfg()).is_err());
    let out = temp();
    let _writer = Store::open(d.path(), cfg()).unwrap();
    assert!(matches!(
        kernel::recover::recover_to(d.path(), &out.path().join("repair"), cfg()),
        Err(kernel::Error::WriterLocked)
    ));
    assert!(!out.path().join("repair").exists());
}

#[test]
fn committed_wal_delete_and_insert_survive_repair() {
    let d = temp();
    make(d.path(), 50, false);
    let mut s = Store::open(d.path(), cfg()).unwrap();
    s.delete(&key(7)).unwrap();
    s.put(&key(51), b"wal only").unwrap();
    s.commit().unwrap();
    drop(s);
    let out = temp();
    let r = repair(d.path(), out.path());
    assert_eq!(r.entries_recovered, 50);
    let s = Store::open(&r.database, cfg()).unwrap();
    assert_eq!(s.get(&key(7)).unwrap(), None);
    assert_eq!(s.get(&key(51)).unwrap(), Some(b"wal only".to_vec()));
}

#[test]
fn damaged_meta_is_membership_uncertain() {
    let d = temp();
    make(d.path(), 50, false);
    let data = d.path().join("data");
    let mut b = fs::read(&data).unwrap();
    b[50] ^= 1;
    fs::write(data, b).unwrap();
    let out = temp();
    let r = repair(d.path(), out.path());
    assert_eq!(r.class, kernel::recover::RecoveryClass::MembershipUncertain);
    assert!(r.unknown_extents > 0);
    assert!(r.raw_candidate_records >= 50);
}

#[test]
fn overflow_truncated_tail_is_reported() {
    let d = temp();
    make(d.path(), 50, false);
    // Allocate the large value last so truncation removes an overflow tail,
    // while its naming leaf and unrelated rows remain on complete pages.
    let mut s = Store::open(d.path(), cfg()).unwrap();
    s.put(&key(51), &vec![51; 16000]).unwrap();
    s.commit().unwrap();
    s.checkpoint().unwrap();
    drop(s);
    let data = d.path().join("data");
    let mut b = fs::read(&data).unwrap();
    let tail = b
        .chunks_exact(4096)
        .position(|p| p[6] == 4 && p[40..44] == [0, 0, 0, 0])
        .unwrap();
    let previous = b
        .chunks_exact(4096)
        .position(|p| p[6] == 4 && u32::from_le_bytes(p[40..44].try_into().unwrap()) == tail as u32)
        .unwrap();
    let new_tail = (b.len() / 4096) as u32;
    let mut moved = b[tail * 4096..(tail + 1) * 4096].to_vec();
    moved[12..16].copy_from_slice(&new_tail.to_le_bytes());
    let generation = u64::from_le_bytes(moved[24..32].try_into().unwrap());
    kernel::page::seal(&mut moved, generation);
    let previous = &mut b[previous * 4096..(previous + 1) * 4096];
    previous[40..44].copy_from_slice(&new_tail.to_le_bytes());
    let generation = u64::from_le_bytes(previous[24..32].try_into().unwrap());
    kernel::page::seal(previous, generation);
    b.extend_from_slice(&moved);
    fs::write(&data, &b).unwrap();
    let s = Store::open(d.path(), cfg()).unwrap();
    assert_eq!(s.get(&key(51)).unwrap(), Some(vec![51; 16000]));
    drop(s);
    fs::OpenOptions::new()
        .write(true)
        .open(&data)
        .unwrap()
        .set_len((b.len() - 100) as u64)
        .unwrap();
    let out = temp();
    let r = repair(d.path(), out.path());
    assert_eq!(r.entries_recovered, 50);
    assert_eq!(r.known_value_losses, 1);
    assert!(r.unknown_extents >= 1);
    assert!(fs::read_to_string(&r.loss_journal)
        .unwrap()
        .contains("truncated file tail"));
}

#[test]
fn damaged_wal_keeps_source_and_verified_commits() {
    let d = temp();
    make(d.path(), 50, false);
    let mut s = Store::open(d.path(), cfg()).unwrap();
    for id in 51..=60 {
        s.put(&key(id), &[id as u8]).unwrap();
        s.commit().unwrap();
    }
    drop(s);
    let wal = d.path().join("wal");
    let mut b = fs::read(&wal).unwrap();
    let mut off = 0;
    let mut puts = Vec::new();
    while off + 20 <= b.len() {
        if b[off + 12] == 3 {
            puts.push(off);
        }
        off += 20 + u32::from_le_bytes(b[off..off + 4].try_into().unwrap()) as usize;
    }
    assert_eq!(puts.len(), 10);
    b[puts[4] + 4] ^= 1;
    fs::write(&wal, &b).unwrap();
    let out = temp();
    let r = repair(d.path(), out.path());
    assert_eq!(r.class, kernel::recover::RecoveryClass::MembershipUncertain);
    assert_eq!(fs::read(r.wal.wal_quarantined.unwrap()).unwrap(), b);
    let s = Store::open(&r.database, cfg()).unwrap();
    assert_eq!(s.get(&key(55)).unwrap(), None);
    for id in (51..=60).filter(|id| *id != 55) {
        assert_eq!(s.get(&key(id)).unwrap(), Some(vec![id as u8]));
    }
    assert_eq!(r.entries_recovered, 59);
}
