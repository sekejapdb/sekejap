//! A torn LAST frame of the page-WAL (finding `vuln-f04`, 2026-09-29).
//!
//! Frames are appended with no barrier until the transaction's commit, so a
//! power loss can persist a frame's full length but not all of its bytes:
//! the tail then reads as zeros or as a damaged frame of full length. The
//! opener used to refuse the whole database on the first frame that failed
//! its checksum, although every acknowledged commit lay before it. A frame
//! past the last verified commit whose last sector never landed (zeros; a
//! written frame ends with the database identity), with no later frame that
//! verifies, is the end of the log. Damage in a frame that did land -- bit
//! rot, which could be an acknowledged commit -- and damage anywhere a
//! commit depends on still refuse.
//!
//! What is at risk, one test each:
//!
//! * a tail of zeros, or a frame written only in part, after the last commit
//!   opens, every committed value intact, and the tail is gone
//!   (`a_torn_tail_after_the_last_commit_is_the_end_of_the_log`);
//! * a full-length frame damaged in the middle -- it landed, so this is not
//!   a torn write -- refuses, the files untouched
//!   (`a_landed_frame_that_is_damaged_refuses`);
//! * a damaged frame followed by one that verifies is not a torn tail and
//!   refuses, the files untouched
//!   (`damage_followed_by_a_valid_frame_refuses`);
//! * damage inside the committed prefix refuses, the files untouched
//!   (`damage_inside_the_committed_prefix_refuses`).

use sekejap_core::pagewal::PageWalStore;
use std::{fs, path::Path};

/// One WAL frame: a 4 KiB page image and its 48-byte envelope.
const FRAME: usize = 4096 + 48;

fn seed(p: &Path) -> Vec<u8> {
    let mut db = PageWalStore::open(p, true, 32 << 10).unwrap();
    for i in 0..20u8 {
        db.put(&[b'k', i], &[i; 64]).unwrap();
        db.commit().unwrap();
    }
    drop(db);
    let wal = fs::read(p.join("wal")).unwrap();
    assert!(wal.len() >= 2 * FRAME && wal.len() % FRAME == 0, "a committed log to damage");
    wal
}

fn committed_intact(p: &Path) {
    let db = PageWalStore::open(p, false, 32 << 10).unwrap();
    for i in 0..20u8 {
        assert_eq!(db.get(&[b'k', i]).unwrap(), Some(vec![i; 64]), "key {i}");
    }
}

fn refuses_unchanged(p: &Path) {
    let before = (fs::read(p.join("data")).unwrap(), fs::read(p.join("wal")).unwrap());
    assert!(PageWalStore::open(p, false, 32 << 10).is_err());
    assert_eq!(before, (fs::read(p.join("data")).unwrap(), fs::read(p.join("wal")).unwrap()));
}

#[test]
fn a_torn_tail_after_the_last_commit_is_the_end_of_the_log() {
    for torn in ["zeros", "half written"] {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("db");
        let committed = seed(&p);
        let mut wal = committed.clone();
        match torn {
            "zeros" => wal.extend_from_slice(&[0u8; FRAME]),
            _ => {
                // The first half of a real frame landed; the rest did not.
                let mut frame = committed[..FRAME].to_vec();
                frame[FRAME / 2..].fill(0);
                wal.extend_from_slice(&frame);
            }
        }
        fs::write(p.join("wal"), &wal).unwrap();
        committed_intact(&p);
        assert_eq!(fs::read(p.join("wal")).unwrap().len(), committed.len(), "{torn}: the torn tail is gone");
    }
}

#[test]
fn a_landed_frame_that_is_damaged_refuses() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("db");
    let committed = seed(&p);
    let mut wal = committed.clone();
    let mut frame = committed[..FRAME].to_vec();
    frame[100] ^= 0xFF;
    wal.extend_from_slice(&frame);
    fs::write(p.join("wal"), &wal).unwrap();
    refuses_unchanged(&p);
}

#[test]
fn damage_followed_by_a_valid_frame_refuses() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("db");
    let committed = seed(&p);
    let mut wal = committed.clone();
    wal.extend_from_slice(&[0u8; FRAME]);
    // A frame that verifies after the damage: the log went on past it.
    wal.extend_from_slice(&committed[FRAME..2 * FRAME]);
    fs::write(p.join("wal"), &wal).unwrap();
    refuses_unchanged(&p);
}

#[test]
fn damage_inside_the_committed_prefix_refuses() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("db");
    let mut wal = seed(&p);
    wal[100] ^= 0xFF;
    fs::write(p.join("wal"), &wal).unwrap();
    refuses_unchanged(&p);
}
