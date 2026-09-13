use kernel::io::IoMode;
use kernel::page::PageKind;
use kernel::store::{Config, Store, SyncMode};

fn cfg() -> Config { Config { budget_bytes: 32 << 20, io: IoMode::Buffered, sync: SyncMode::Off } }

#[test]
fn recovery_keeps_every_intact_leaf_and_names_what_it_lost() {
    let d = tempfile::tempdir().unwrap();
    let n = 50_000u64;
    { let mut s = Store::create(d.path(), cfg()).unwrap();
      s.bulk_load((0..n).map(|i| (i.to_be_bytes().to_vec(), b"v".to_vec()))).unwrap();
      s.commit().unwrap(); s.checkpoint().unwrap(); }

    // Destroy every interior page we can find, plus one leaf.
    let path = d.path().join("data");
    let mut bytes = std::fs::read(&path).unwrap();
    let ps = 4096;
    let mut wrecked_leaf = false;
    for p in 1..bytes.len() / ps {
        let kind = u16::from_le_bytes([bytes[p * ps + 6], bytes[p * ps + 7]]);
        if kind == PageKind::Interior as u16 { bytes[p * ps + 100] ^= 0xff; }
        if kind == PageKind::Leaf as u16 && !wrecked_leaf { bytes[p * ps + 50] ^= 0xff; wrecked_leaf = true; }
    }
    std::fs::write(&path, &bytes).unwrap();

    let report = kernel::recover::recover(d.path(), cfg()).unwrap();
    assert!(report.leaves_kept > 0);
    assert_eq!(report.leaves_lost, 1, "exactly one leaf was destroyed");
    assert_eq!(report.lost_ranges.len(), 1, "the loss must be named, not merely counted");

    // The store opens and serves everything that survived.
    let s = Store::open(d.path(), cfg()).unwrap();
    let survived = s.scan(&[]).unwrap().count() as u64;
    assert_eq!(survived, report.entries_recovered);
    assert!(survived > n * 9 / 10, "one lost leaf should not cost 10% of the data");
}

#[test]
fn recovery_of_an_undamaged_store_loses_nothing() {
    let d = tempfile::tempdir().unwrap();
    let n = 20_000u64;
    { let mut s = Store::create(d.path(), cfg()).unwrap();
      s.bulk_load((0..n).map(|i| (i.to_be_bytes().to_vec(), b"v".to_vec()))).unwrap();
      s.commit().unwrap(); s.checkpoint().unwrap(); }
    let r = kernel::recover::recover(d.path(), cfg()).unwrap();
    assert_eq!(r.leaves_lost, 0);
    assert_eq!(r.entries_recovered, n);
}

/// `recover` must NOT delete the write-ahead log. WAL records are logical
/// (`insert`/`delete` by key), so they replay onto a rebuilt tree exactly as
/// well as onto the old one -- and the log holds precisely the writes that
/// were committed but never checkpointed, which is to say the writes that a
/// leaf sweep of `data` could never find in the first place. Deleting the log
/// would discard exactly that committed-but-not-yet-durable-in-`data` data.
#[test]
fn recovery_preserves_committed_but_uncheckpointed_writes() {
    let d = tempfile::tempdir().unwrap();
    let base_n = 5_000u64;
    // Base data, durable in `data` via an explicit checkpoint.
    { let mut s = Store::create(d.path(), cfg()).unwrap();
      s.bulk_load((0..base_n).map(|i| (i.to_be_bytes().to_vec(), b"base".to_vec()))).unwrap();
      s.commit().unwrap(); s.checkpoint().unwrap(); }

    // New writes: committed (appended to the WAL and, per SyncMode::Off,
    // not even fsynced -- durability here rests entirely on the file still
    // existing, which `recover` must respect), but never checkpointed, so
    // `data` on disk does not contain them -- only the WAL does.
    let extra_keys: Vec<u64> = (900_000..900_020).collect();
    {
        let mut s = Store::open(d.path(), cfg()).unwrap();
        for &k in &extra_keys {
            s.put(&k.to_be_bytes(), b"uncheckpointed").unwrap();
        }
        s.commit().unwrap();
        // No checkpoint, no clean shutdown -- simulate a crash right here.
        std::mem::forget(s);
    }

    // Damage a leaf, same technique as the other tests, to confirm this
    // works together with an actual sweep loss, not only on a clean file.
    let path = d.path().join("data");
    let mut bytes = std::fs::read(&path).unwrap();
    let ps = 4096;
    let mut wrecked = false;
    for p in 1..bytes.len() / ps {
        let kind = u16::from_le_bytes([bytes[p * ps + 6], bytes[p * ps + 7]]);
        if kind == PageKind::Leaf as u16 && !wrecked {
            bytes[p * ps + 50] ^= 0xff;
            wrecked = true;
        }
    }
    assert!(wrecked, "fixture needs at least one leaf");
    std::fs::write(&path, &bytes).unwrap();

    let wal_before = std::fs::read(d.path().join("wal")).unwrap();
    let report = kernel::recover::recover(d.path(), cfg()).unwrap();
    assert_eq!(
        std::fs::read(d.path().join("wal")).unwrap(), wal_before,
        "an undamaged log must come through recovery untouched -- not shortened, not rewritten"
    );
    assert!(report.wal_quarantined.is_none(),
            "nothing to quarantine: the log was fine");

    // Reopening replays the (untouched) WAL onto the rebuilt tree. The
    // committed writes the sweep could never have found in `data` must
    // still be there.
    let s = Store::open(d.path(), cfg()).unwrap();
    for &k in &extra_keys {
        assert_eq!(
            s.get(&k.to_be_bytes()).unwrap().as_deref(),
            Some(&b"uncheckpointed"[..]),
            "a committed-but-uncheckpointed write must survive recovery, key {k}"
        );
    }
}

/// Law 5 end to end, on the damage that used to be silent.
///
/// A flipped bit in the middle of a log with committed frames on both sides
/// of it. Before Task 19 this was `Ok`: the walk stopped at the bad frame,
/// `open` truncated the file to that point and 1,499 of 3,000 committed rows
/// ceased to exist without a word (measured, Task 17 final review, F4). Now
/// the open refuses -- and, because a refusal nothing can clear is its own
/// Law 5 violation, `recover()` has to be able to take exactly this image and
/// give back a store that opens, with every original byte still on disk.
#[test]
fn a_damaged_log_refuses_to_open_and_recover_clears_it() {
    let d = tempfile::tempdir().unwrap();
    let checkpointed = 500u64;
    let logged = 200u64;
    {
        let mut s = Store::create(d.path(), cfg()).unwrap();
        for i in 0..checkpointed { s.put(&i.to_be_bytes(), &i.to_le_bytes()).unwrap(); }
        s.commit().unwrap();
        s.checkpoint().unwrap();          // these rows are in the pages; the log is empty again
        // Committed one at a time, so the log holds many complete
        // transactions rather than one enormous one -- damage in the middle
        // then genuinely has committed frames behind it.
        for i in checkpointed..checkpointed + logged {
            s.put(&i.to_be_bytes(), &i.to_le_bytes()).unwrap();
            s.commit().unwrap();
        }
        std::mem::forget(s);              // the crash
    }

    // Flip one bit in the header of whichever frame straddles the midpoint,
    // found by walking the frames rather than by assuming a size.
    let path = d.path().join("wal");
    let mut bytes = std::fs::read(&path).unwrap();
    let midpoint = bytes.len() as u64 / 2;
    let hdr = 20usize;
    let mut off = 0usize;
    while (off as u64) < midpoint {
        let plen = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
        off += hdr + plen;
    }
    let victim = off;
    assert!(victim > 0 && (victim as u64) < bytes.len() as u64 - 4096,
            "the damage must have real log on both sides of it");
    bytes[victim + 4] ^= 0x01;            // one bit of the LSN: only the CRC can see it
    std::fs::write(&path, &bytes).unwrap();

    match Store::open(d.path(), cfg()) {
        Err(kernel::Error::CorruptWal { offset, .. }) => assert_eq!(offset as usize, victim),
        Err(e) => panic!("expected CorruptWal, got {e:?}"),
        Ok(_) => panic!("damage with committed frames behind it must not open silently"),
    }
    assert_eq!(std::fs::read(&path).unwrap(), bytes,
               "the refused open must not have touched a byte");

    let report = kernel::recover::recover(d.path(), cfg()).unwrap();
    let aside = report.wal_quarantined.clone().expect("the damaged log must be set aside, not left to brick the store");
    assert_eq!(std::fs::read(&aside).unwrap(), bytes,
               "the quarantined copy must be the damaged log byte for byte -- it is the only \
                evidence a repair tool will ever have");
    assert_eq!(report.wal_bytes_set_aside, bytes.len() as u64);
    assert!(report.wal_bytes_kept as usize > victim,
            "verified committed transactions after the damage must be resynchronised");

    let s2 = Store::open(d.path(), cfg())
        .expect("recover() must leave a store that opens -- a refusal with no way out is Law 5 too");
    for i in 0..checkpointed {
        assert_eq!(s2.get(&i.to_be_bytes()).unwrap().as_deref(), Some(&i.to_le_bytes()[..]),
                   "row {i} was durable in the pages before any of this");
    }
    // Everything the log could still be read for is there too: the frames in
    // FRONT of the damage replay normally.
    let kept = (victim / 60).saturating_sub(1) as u64;   // 40-byte Put + 20-byte Commit per row
    assert!(kept > 0, "sanity: the prefix must hold committed rows");
    for i in checkpointed..checkpointed + kept {
        assert_eq!(s2.get(&i.to_be_bytes()).unwrap().as_deref(), Some(&i.to_le_bytes()[..]),
                   "row {i} was committed in front of the damage and is in the readable prefix");
    }
    let recovered_logged = (checkpointed..checkpointed + logged)
        .filter(|i| s2.get(&i.to_be_bytes()).unwrap().is_some())
        .count();
    assert!(recovered_logged >= logged as usize - 2,
            "one damaged frame may cost its ambiguous transaction boundary, not the tail: \
             recovered {recovered_logged} of {logged}");
}

/// A complete damaged frame near EOF used to fall under the "one maximum
/// frame" heuristic and be truncated together with every valid transaction
/// behind it. Recovery must resynchronise, quarantine the original bytes,
/// and retain each independently committed transaction after the damage.
#[test]
fn one_flipped_byte_in_a_complete_frame_keeps_later_committed_frames() {
    let d = tempfile::tempdir().unwrap();
    {
        let mut s = Store::create(d.path(), cfg()).unwrap();
        s.put(b"base", b"durable").unwrap();
        s.commit().unwrap();
        s.checkpoint().unwrap();
        for i in 0..12u64 {
            s.put(&i.to_be_bytes(), b"logged").unwrap();
            s.commit().unwrap();
        }
    }

    let wal = d.path().join("wal");
    let mut bytes = std::fs::read(&wal).unwrap();
    let mut offsets = Vec::new();
    let mut off = 0usize;
    while off < bytes.len() {
        offsets.push(off);
        let plen = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
        off += 20 + plen;
    }
    let victim = offsets[6]; // put for transaction 3; complete commits follow it
    // Corrupt the LENGTH, not merely the payload/LSN. The new bounded length
    // still points past EOF, exactly like an interrupted physical write; the
    // independently valid frames behind it are what prove this is damage.
    bytes[victim + 2] ^= 1;
    assert!(bytes.len() - victim < kernel::page::MAX_RECORD_LEN,
            "fixture must exercise the old short-tail deletion branch");
    std::fs::write(&wal, &bytes).unwrap();

    assert!(matches!(Store::open(d.path(), cfg()), Err(kernel::Error::CorruptWal { .. })),
            "verified frames behind a corrupt length prove damage, not a clean short write");
    assert_eq!(std::fs::read(&wal).unwrap(), bytes,
               "ordinary open must preserve every byte of the unverifiable tail");

    let report = kernel::recover::recover(d.path(), cfg()).unwrap();
    let aside = report.wal_quarantined.expect("complete CRC damage must be quarantined");
    assert_eq!(std::fs::read(aside).unwrap(), bytes,
               "the original damaged log must remain available byte-for-byte");

    let s = Store::open(d.path(), cfg()).unwrap();
    assert_eq!(s.get(b"base").unwrap().as_deref(), Some(&b"durable"[..]));
    for i in 4..12u64 {
        assert_eq!(s.get(&i.to_be_bytes()).unwrap().as_deref(), Some(&b"logged"[..]),
                   "committed transaction {i} after the damaged frame was lost");
    }
}
