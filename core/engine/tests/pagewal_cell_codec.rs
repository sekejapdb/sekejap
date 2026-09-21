//! The `E4PWAL02` feature bits are the database's OWN declaration, not a
//! restatement of the build flags of whichever binary happens to open it.
//!
//! Properties at risk (Law 8 — a release must read AND write every database an
//! earlier release wrote, whatever cargo features that build had):
//!
//! - A database declaring a SUPPORTED feature set opens as a writer in any
//!   build, whether or not that build would have chosen the same set when
//!   creating a database itself.
//! - Opening, writing, committing and checkpointing never change the declared
//!   bits. A routine update must not silently raise or lower a database's
//!   minimum reader requirements.
//! - A database declaring an UNSUPPORTED bit is still refused, before any byte
//!   of the data file or the WAL is changed.

use sekejap_core::pagewal::PageWalStore;
use kernel::page::{seal, PageKind, PageMut, PageRef, PAGE_SIZE};
use std::{fs, path::Path};

/// Offset of the feature word inside the 56-byte checkpoint metadata record.
const FEATURES: usize = 48;
/// Bit 0: compact cell encodings.
const COMPACT_CELLS: u8 = 1;

fn seed(p: &Path) {
    let mut db = PageWalStore::open(p, true, 32 << 10).unwrap();
    for i in 0..64u32 {
        db.put(format!("key-{i:04}").as_bytes(), format!("base-{i}").as_bytes()).unwrap();
    }
    db.commit().unwrap();
    db.checkpoint().unwrap();
}

fn features_on_disk(p: &Path) -> [u64; 2] {
    let data = fs::read(p.join("data")).unwrap();
    let mut out = [0; 2];
    for no in 0..2 {
        let b = &data[no * PAGE_SIZE..(no + 1) * PAGE_SIZE];
        let slot = PageRef::open(b, no as u32).unwrap().slot(0).to_vec();
        out[no] = u64::from_le_bytes(slot[FEATURES..FEATURES + 8].try_into().unwrap());
    }
    out
}

/// Rewrite one checkpoint metadata copy in place, resealing it so the change
/// is a VALID header the opener must judge on its declared features alone,
/// not a corrupt page it would reject for its checksum.
fn rewrite_header(p: &Path, no: usize, f: impl FnOnce(&mut Vec<u8>)) {
    let mut data = fs::read(p.join("data")).unwrap();
    let b = &mut data[no * PAGE_SIZE..(no + 1) * PAGE_SIZE];
    let mut h = PageRef::open(b, no as u32).unwrap().slot(0).to_vec();
    f(&mut h);
    let mut page = PageMut::init(b, PageKind::Meta, 0, no as u32);
    page.insert_slot(0, &h).unwrap();
    page.finalise(0);
    seal(b, 1);
    PageRef::open(b, no as u32).unwrap();
    fs::write(p.join("data"), data).unwrap();
}

/// A database whose declared cell codec is the OPPOSITE of what this build
/// would have chosen at creation opens, serves every existing record, accepts
/// the full mutation set, and still declares exactly the bits it declared
/// before — after every step.
#[test]
fn a_database_declaring_the_other_codec_opens_reads_writes_and_keeps_its_bits() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("db");
    seed(&p);
    let created = features_on_disk(&p);
    assert_eq!(created[0], created[1], "both metadata copies declare one feature set");

    // Exactly the file the other build would have produced: the same bytes,
    // declaring the other cell codec.
    for no in 0..2 {
        rewrite_header(&p, no, |h| h[FEATURES] ^= COMPACT_CELLS);
    }
    let declared = created[0] ^ COMPACT_CELLS as u64;
    assert_eq!(features_on_disk(&p), [declared; 2]);

    let mut db = PageWalStore::open(&p, false, 32 << 10)
        .unwrap_or_else(|e| panic!("writer refused a database declaring the other codec: {e:?}"));
    for i in 0..64u32 {
        assert_eq!(
            db.get(format!("key-{i:04}").as_bytes()).unwrap().as_deref(),
            Some(format!("base-{i}").as_bytes()),
            "existing record read back exactly",
        );
    }

    db.put(b"key-0007", b"updated").unwrap();
    db.put(b"key-9999", b"inserted").unwrap();
    assert!(db.delete(b"key-0011").unwrap());
    db.commit().unwrap();
    assert_eq!(features_on_disk(&p), [declared; 2], "commit must not restamp the header");
    db.checkpoint().unwrap();
    assert_eq!(features_on_disk(&p), [declared; 2], "checkpoint must not restamp the header");
    drop(db);

    let db = PageWalStore::open(&p, false, 32 << 10).unwrap();
    assert_eq!(db.get(b"key-0007").unwrap().as_deref(), Some(b"updated".as_slice()));
    assert_eq!(db.get(b"key-9999").unwrap().as_deref(), Some(b"inserted".as_slice()));
    assert_eq!(db.get(b"key-0011").unwrap(), None);
    for i in (0..64u32).filter(|i| ![7, 11].contains(i)) {
        assert_eq!(
            db.get(format!("key-{i:04}").as_bytes()).unwrap().as_deref(),
            Some(format!("base-{i}").as_bytes()),
        );
    }
    drop(db);
    assert_eq!(features_on_disk(&p), [declared; 2], "reopen must not restamp the header");
}

/// A bit this build does not implement is still a refusal, and the refusal
/// leaves the data file and the WAL byte-for-byte as they were. The supported
/// bit, flipped either way, is NOT a refusal — that is the whole point.
#[test]
fn unknown_feature_bits_are_refused_and_supported_bits_are_not() {
    let t = tempfile::tempdir().unwrap();
    for (name, bit) in [("low", 1u8 << 7), ("high", 0)] {
        for no in 0..2 {
            let p = t.path().join(format!("unknown-{name}-{no}"));
            seed(&p);
            let before = (fs::read(p.join("data")).unwrap(), fs::read(p.join("wal")).unwrap());
            rewrite_header(&p, no, |h| if bit == 0 { h[FEATURES + 7] |= 0x80 } else { h[FEATURES] |= bit });
            let after_edit = (fs::read(p.join("data")).unwrap(), fs::read(p.join("wal")).unwrap());
            assert_ne!(before, after_edit);
            assert!(
                PageWalStore::open(&p, false, 32 << 10).is_err(),
                "an unimplemented required feature must be refused",
            );
            assert_eq!(
                (fs::read(p.join("data")).unwrap(), fs::read(p.join("wal")).unwrap()),
                after_edit,
                "a refusal must not change one byte of the source",
            );
        }
    }
}
