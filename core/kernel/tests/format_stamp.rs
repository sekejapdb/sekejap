//! The disk-format stamp: page bytes 18-19 carry `FORMAT_VERSION` = 2 on
//! every page this build creates or rewrites, and an intact page claiming
//! anything else is refused by name with the file untouched.
//!
//! What this file can prove is what the KERNEL owns: the page header itself,
//! the two metadata slots of a kernel `Store` (pages 0 and 1), and its data
//! pages. The page-WAL store's own metadata copies and its WAL frame images
//! are `sekejap-core`, one layer up, and are proved in
//! `core/engine/tests/format_v2_compat.rs`; a kernel test cannot name that
//! crate without inverting the dependency the compiler enforces.
//!
//! The contract is `docs/core/FORMAT_V2.md`.

use kernel::{
    io::IoMode,
    page::{self, PageKind, PageMut, PageRef, PAGE_SIZE},
    store::{Config, Store, SyncMode},
    Error, FORMAT_VERSION,
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

const STAMP_AT: usize = 18;

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn tempdir() -> tempfile::TempDir {
    let base = std::env::temp_dir();
    tempfile::Builder::new()
        .prefix("format-stamp-")
        .tempdir_in(base)
        .unwrap()
}

fn stamp_of(page: &[u8]) -> u16 {
    u16::from_le_bytes(page[STAMP_AT..STAMP_AT + 2].try_into().unwrap())
}

fn file_bytes(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fs::read_dir(dir)
        .unwrap()
        .filter(|e| e.as_ref().unwrap().file_type().unwrap().is_file())
        .map(|e| {
            let e = e.unwrap();
            (e.path(), fs::read(e.path()).unwrap())
        })
        .collect()
}

/// Build a small store whose data file has both metadata slots and at least
/// one ordinary data page, then close it with everything checkpointed.
fn populated_store(dir: &Path) {
    let mut s = Store::create(dir, cfg()).unwrap();
    for i in 0..500u64 {
        s.put(&i.to_be_bytes(), &[7u8; 64]).unwrap();
    }
    s.commit().unwrap();
    s.checkpoint().unwrap();
}

/// Rewrite bytes 18-19 of one page and restore its checksum, so the opener
/// judges the CLAIM and not a damaged page.
fn restamp(path: &Path, page_no: usize, claim: u16) {
    let mut data = fs::read(path).unwrap();
    let page = &mut data[page_no * PAGE_SIZE..(page_no + 1) * PAGE_SIZE];
    page[STAMP_AT..STAMP_AT + 2].copy_from_slice(&claim.to_le_bytes());
    let crc = page::checksum(page);
    page[36..40].copy_from_slice(&crc.to_le_bytes());
    assert_eq!(page::checksum(page), crc, "restamped page must verify");
    fs::write(path, data).unwrap();
}

#[test]
fn the_disk_format_this_build_reads_and_writes_is_two() {
    assert_eq!(FORMAT_VERSION, 2);
    assert_eq!(kernel::page::FORMAT_VERSION, FORMAT_VERSION);
}

#[test]
fn every_page_kind_a_fresh_page_is_initialised_as_carries_the_stamp_at_bytes_18_and_19() {
    for kind in [
        PageKind::Free,
        PageKind::Meta,
        PageKind::Leaf,
        PageKind::Interior,
        PageKind::Overflow,
    ] {
        let mut b = vec![0u8; PAGE_SIZE];
        let mut p = PageMut::init(&mut b, kind, 3, 11);
        p.insert_slot(0, b"payload").unwrap();
        p.finalise(0);
        assert_eq!(
            stamp_of(&b),
            FORMAT_VERSION,
            "init must stamp {kind:?} before anything is written to it"
        );
        assert_eq!(page::format_version(&b), FORMAT_VERSION);
        page::seal(&mut b, 4);
        assert_eq!(stamp_of(&b), FORMAT_VERSION, "seal must keep {kind:?} stamped");
        PageRef::open(&b, 11).unwrap();
    }
}

#[test]
fn sealing_a_page_that_reached_this_build_unstamped_rewrites_the_stamp() {
    // The rewrite path: a page image reopened rather than initialised. `seal`
    // is the one place bytes leave for the medium, so it is the one place
    // that has to be right for every mutation site at once.
    let mut b = vec![0u8; PAGE_SIZE];
    let mut p = PageMut::init(&mut b, PageKind::Leaf, 1, 5);
    p.insert_slot(0, b"row").unwrap();
    p.finalise(0);
    b[STAMP_AT..STAMP_AT + 2].copy_from_slice(&0u16.to_le_bytes());
    assert_eq!(stamp_of(&b), 0, "the page was deliberately unstamped");
    PageMut::reopen(&mut b).finalise(0);
    page::seal(&mut b, 9);
    assert_eq!(stamp_of(&b), FORMAT_VERSION);
    PageRef::open(&b, 5).unwrap();
}

#[test]
fn a_created_file_carries_the_stamp_on_both_metadata_pages_and_on_a_data_page() {
    let d = tempdir();
    populated_store(d.path());
    let data = fs::read(d.path().join("data")).unwrap();
    assert_eq!(data.len() % PAGE_SIZE, 0, "data file is whole pages");
    assert!(
        data.len() / PAGE_SIZE >= 3,
        "need both metadata pages and at least one data page; got {} pages",
        data.len() / PAGE_SIZE
    );
    for no in 0..data.len() / PAGE_SIZE {
        let page = &data[no * PAGE_SIZE..(no + 1) * PAGE_SIZE];
        assert_eq!(
            stamp_of(page),
            FORMAT_VERSION,
            "page {no} of a file this build created must carry disk format 2"
        );
    }
    // Named explicitly, because these three are the ones the rule is about:
    // metadata copy 0, metadata copy 1, and an ordinary data page.
    for no in [0usize, 1, 2] {
        assert_eq!(stamp_of(&data[no * PAGE_SIZE..(no + 1) * PAGE_SIZE]), 2);
    }
}

#[test]
fn a_stamp_of_one_or_three_on_either_metadata_copy_is_refused_by_name_and_changes_no_byte() {
    for claim in [1u16, 3] {
        for copy in [0usize, 1] {
            let d = tempdir();
            populated_store(d.path());
            restamp(&d.path().join("data"), copy, claim);
            let after_edit = file_bytes(d.path());

            let error = Store::open(d.path(), cfg())
                .err()
                .unwrap_or_else(|| panic!("a file claiming disk format {claim} on copy {copy} must be refused"));
            assert!(
                matches!(error, Error::UnsupportedFormat { found } if found == claim),
                "refusal must name the disk format, not something else: {error}"
            );
            assert_eq!(
                error.to_string(),
                format!("sekejap disk format {claim}; this build reads v2"),
                "the refusal text is the contract's sentence"
            );
            assert_eq!(
                file_bytes(d.path()),
                after_edit,
                "a refused open must leave every file byte and the inventory unchanged"
            );

            let error = Store::open_snapshot(d.path(), cfg())
                .err()
                .unwrap_or_else(|| panic!("a snapshot of disk format {claim} on copy {copy} must be refused"));
            assert!(
                matches!(error, Error::UnsupportedFormat { found } if found == claim),
                "snapshot refusal must name the disk format: {error}"
            );
            assert_eq!(
                file_bytes(d.path()),
                after_edit,
                "a refused snapshot must leave every file byte and the inventory unchanged"
            );
        }
    }
}

#[test]
fn an_intact_data_page_that_claims_another_disk_format_is_refused_by_name() {
    // Not only the metadata copies: any page that arrives from the medium is
    // judged, so a file cannot be half v2.
    let mut b = vec![0u8; PAGE_SIZE];
    let mut p = PageMut::init(&mut b, PageKind::Leaf, 1, 6);
    p.insert_slot(0, b"row").unwrap();
    p.finalise(0);
    page::seal(&mut b, 1);
    PageRef::open(&b, 6).expect("a stamped page opens");

    for claim in [0u16, 1, 3, u16::MAX] {
        let mut damaged = b.clone();
        damaged[STAMP_AT..STAMP_AT + 2].copy_from_slice(&claim.to_le_bytes());
        let crc = page::checksum(&damaged);
        damaged[36..40].copy_from_slice(&crc.to_le_bytes());
        let error = PageRef::open(&damaged, 6).err().expect("must be refused");
        assert!(
            matches!(error, Error::UnsupportedFormat { found } if found == claim),
            "claim {claim} refused for the wrong reason: {error}"
        );
        assert_eq!(
            error.to_string(),
            format!("sekejap disk format {claim}; this build reads v2")
        );
    }
}

#[test]
fn a_damaged_page_is_damage_and_not_a_foreign_disk_format() {
    // The stamp is read after the checksum, so a page whose bytes are wrong
    // is reported as what it is. Getting this backwards would relabel every
    // torn page as a format from another product.
    let mut b = vec![0u8; PAGE_SIZE];
    let mut p = PageMut::init(&mut b, PageKind::Leaf, 1, 6);
    p.insert_slot(0, b"row").unwrap();
    p.finalise(0);
    page::seal(&mut b, 1);
    b[STAMP_AT..STAMP_AT + 2].copy_from_slice(&1u16.to_le_bytes()); // no re-checksum
    let error = PageRef::open(&b, 6).err().expect("must be refused");
    assert!(
        matches!(error, Error::Corrupt { why: "checksum mismatch", .. }),
        "a torn page must be reported as damage: {error}"
    );
}
