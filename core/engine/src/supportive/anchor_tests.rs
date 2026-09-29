//! The Anchor on disk (`docs/core/SUPPORTIVE.md` 2.0.2). What is at risk,
//! one test each:
//!
//! * it round-trips through the three header copies, survives one damaged
//!   copy, and all copies damaged is corruption
//!   (`the_anchor_survives_one_damaged_copy`);
//! * a Register format this build does not know is refused by name, never
//!   read as damage (`a_newer_register_format_is_refused_by_name`);
//! * the RELEASED 0.18.5 binary refuses a file carrying the Anchor, naming it
//!   newer, and changes no byte -- run when `SEKEJAP_0185_OPEN_CHECK` names
//!   `open_check` built from the v0.18.5 tag, skipped otherwise
//!   (`the_released_0185_binary_refuses_an_anchored_file`).

use super::anchor::{read_anchor, write_anchor, ANCHOR_MAGIC};
use super::carrier::{Anchor, CensusLine, Kind};
use crate::collections::{packet, Database, Error};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};

fn cfg() -> Config {
    Config { budget_bytes: 1 << 20, io: IoMode::Buffered, sync: SyncMode::Full }
}

fn anchor() -> Anchor {
    Anchor {
        roots: [5, 6, 7],
        census: vec![CensusLine { kind: Kind::new(b"COLM").unwrap(), version: 1, variant: 0 }],
    }
}

#[test]
fn the_anchor_survives_one_damaged_copy() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("db"), cfg()).unwrap();
    let store = db.writer().unwrap();
    write_anchor(store, &anchor()).unwrap();
    assert_eq!(read_anchor(store).unwrap(), anchor());
    store.put(&[0, 0, 1], b"damaged").unwrap();
    assert_eq!(read_anchor(store).unwrap(), anchor(), "copies 0 and 2 win");
    store.put(&[0, 0, 0], b"damaged").unwrap();
    store.put(&[0, 0, 2], b"damaged").unwrap();
    assert!(matches!(read_anchor(store), Err(Error::Corrupt(_))));
}

#[test]
fn a_newer_register_format_is_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("db"), cfg()).unwrap();
    let store = db.writer().unwrap();
    write_anchor(store, &anchor()).unwrap();
    let mut payload = anchor().encode().unwrap();
    payload[..2].copy_from_slice(&2u16.to_be_bytes());
    store.put(&[0, 0, 0], &packet(ANCHOR_MAGIC, &payload).unwrap()).unwrap();
    match read_anchor(store) {
        Err(Error::Unsupported(m)) => assert!(m.contains("Register format 2"), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_released_0185_binary_refuses_an_anchored_file() {
    let Ok(open_check) = std::env::var("SEKEJAP_0185_OPEN_CHECK") else {
        eprintln!("skipped: SEKEJAP_0185_OPEN_CHECK is not set");
        return;
    };
    use files::snapshot;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    {
        let mut db = Database::create(&path, cfg()).unwrap();
        let store = db.writer().unwrap();
        write_anchor(store, &anchor()).unwrap();
        db.commit().unwrap();
        db.checkpoint().unwrap();
    }
    let before = snapshot(&path);
    let out = std::process::Command::new(&open_check).arg(&path).output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "0.18.5 opened an anchored file: {said}");
    assert!(said.contains("newer than this binary"), "{said}");
    assert_eq!(snapshot(&path), before, "0.18.5 changed a byte of the file it refused");
}

/// Every file of a database directory, name and bytes.
mod files {
    pub(super) fn snapshot(dir: &std::path::Path) -> Vec<(String, Vec<u8>)> {
        let mut out: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.is_file())
            .map(|p| (p.file_name().unwrap().to_string_lossy().into_owned(), std::fs::read(&p).unwrap()))
            .collect();
        out.sort();
        out
    }
}
