//! The cell encoding is a property of the DATABASE, never of the build.
//!
//! Two properties are at risk here, and both are about a stored file being
//! readable by a binary that was compiled with a different cargo feature set:
//!
//! 1. Every build decodes BOTH cell families, and the fast validated path and
//!    the canonical fallible decoder agree on what the bytes mean. Today the
//!    fast helpers treat the compact-integer family as always present while
//!    `decode_record` gates both compact families behind `compact-cells`, so a
//!    build without the feature refuses pages a build with it wrote.
//! 2. A tree whose build default is the plain codec can still READ a page set
//!    written with compact cells and INSERT into it. Cell families may be
//!    mixed inside one page: `0x4000 | klen` and `FF 81..88` can never be a
//!    valid ordinary key length on a 4 KiB page, so every record carries its
//!    own family and no page-wide flag is needed.
//!
//! The records below are built by hand, exactly as `enc_leaf` lays them out,
//! so the test is independent of which encoder this build happens to have.

use std::cell::Cell;
use std::sync::Arc;

use kernel::btree::BTree;
use kernel::budget::MemoryBudget;
use kernel::io::{open_file, IoMode};
use kernel::page::{PageKind, PageMut, PageRef, PAGE_SIZE};
use kernel::pool::BufferPool;
use kernel::verify::{decode_record, DecodedRecord};

fn pool_in(dir: &std::path::Path, frames: usize) -> BufferPool {
    let (file, _) = open_file(&dir.join("data"), IoMode::Buffered).unwrap();
    let budget = Arc::new(MemoryBudget::new(frames * PAGE_SIZE + (1 << 20)));
    let pool = BufferPool::new(file.into(), budget, frames).unwrap();
    // Page 0 is the superblock everywhere the engine runs; a tree page 0 would
    // collide with the `next_leaf == 0` sentinel.
    let reserved = { let w = pool.allocate().unwrap(); w.page_no() };
    assert_eq!(reserved, 0, "the superblock must be page 0");
    pool
}

/// v1 ordinary leaf cell: u16 key length, key, u16 value length, value.
fn ordinary(key: &[u8], val: &[u8]) -> Vec<u8> {
    let mut r = (key.len() as u16).to_le_bytes().to_vec();
    r.extend_from_slice(key);
    r.extend_from_slice(&(val.len() as u16).to_le_bytes());
    r.extend_from_slice(val);
    r
}

/// Compact ordinary leaf cell: u16 `(0x4000 | key length)`, key, rest = value.
fn compact_ordinary(key: &[u8], val: &[u8]) -> Vec<u8> {
    let mut r = (0x4000u16 | key.len() as u16).to_le_bytes().to_vec();
    r.extend_from_slice(key);
    r.extend_from_slice(val);
    r
}

/// Compact integer leaf cell: `FF`, width-tagged integer key, rest = value.
/// The key INCLUDES its own `0x81..=0x88` width tag.
fn compact_integer(key: &[u8], val: &[u8]) -> Vec<u8> {
    assert!((0x81..=0x88).contains(&key[0]) && key.len() == 1 + (key[0] - 0x80) as usize);
    let mut r = vec![0xff];
    r.extend_from_slice(key);
    r.extend_from_slice(val);
    r
}

/// Three keys in ascending order, one per cell family, so one leaf holds all
/// three encodings at once. `\x01..` < `\x02..` < `\x84..`.
fn mixed_family_records() -> Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let plain_key = b"\x01ordinary".to_vec();
    let plain_val = b"value-of-the-ordinary-cell".to_vec();
    let compact_key = b"\x02compact".to_vec();
    let compact_val = b"value-of-the-compact-ordinary-cell".to_vec();
    let int_key = vec![0x84, 0x00, 0x00, 0x00, 0x07];
    let int_val = b"value-of-the-compact-integer-cell".to_vec();
    vec![
        (plain_key.clone(), plain_val.clone(), ordinary(&plain_key, &plain_val)),
        (compact_key.clone(), compact_val.clone(), compact_ordinary(&compact_key, &compact_val)),
        (int_key.clone(), int_val.clone(), compact_integer(&int_key, &int_val)),
    ]
}

/// Put `records` on one fresh leaf page and return its page number.
fn leaf_page(pool: &BufferPool, tree_id: u16, records: &[Vec<u8>]) -> u32 {
    let mut w = pool.allocate().unwrap();
    let no = w.page_no();
    let mut p = PageMut::init(w.bytes_mut(), PageKind::Leaf, tree_id, no);
    for (i, rec) in records.iter().enumerate() {
        p.insert_slot(i, rec).unwrap();
    }
    p.finalise(0);
    no
}

/// Property 1a: the canonical fallible decoder accepts every cell family in
/// every build, and returns the exact key and value that was encoded.
#[test]
fn canonical_decoder_reads_every_cell_family_in_every_build() {
    for (key, value, rec) in mixed_family_records() {
        let decoded = decode_record(&rec, 2, PageKind::Leaf)
            .unwrap_or_else(|e| panic!("cell family refused by this build: {e:?}"));
        let DecodedRecord::Leaf { key: k, value: v, overflow } = decoded else {
            panic!("leaf cell decoded as an interior record");
        };
        assert_eq!(k, &key[..], "decoded key");
        assert_eq!(v, &value[..], "decoded value");
        assert!(!overflow);
    }
}

/// Property 1b: the fast validated path and the canonical decoder agree. A
/// page holding all three families is readable through the B-tree (which
/// validates through `decode_record` and then reads through the fast
/// helpers), and both report the same keys and values.
#[test]
fn fast_and_canonical_paths_agree_on_every_cell_family() {
    let dir = tempfile::tempdir().unwrap();
    let pool = pool_in(dir.path(), 32);
    let rows = mixed_family_records();
    let root = leaf_page(&pool, 1, &rows.iter().map(|r| r.2.clone()).collect::<Vec<_>>());

    let (last, hits, attempts) = (Cell::new(None), Cell::new(0), Cell::new(0));
    let tree = BTree::open(&pool, 1, root, &last, &hits, &attempts);

    for (key, value, rec) in &rows {
        let got = tree
            .get(key)
            .unwrap_or_else(|e| panic!("fast path refused a stored cell family: {e:?}"));
        assert_eq!(got.as_deref(), Some(&value[..]), "point read");
        let DecodedRecord::Leaf { key: k, value: v, .. } =
            decode_record(rec, root, PageKind::Leaf).unwrap() else { panic!() };
        assert_eq!((k, v), (&key[..], &value[..]), "canonical decode of the same cell");
    }

    let scanned: Vec<(Vec<u8>, Vec<u8>)> = tree
        .range(b"")
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    let expected: Vec<(Vec<u8>, Vec<u8>)> =
        rows.iter().map(|(k, v, _)| (k.clone(), v.clone())).collect();
    assert_eq!(scanned, expected, "range scan over mixed cell families");
}

/// Property 2: a tree running this build's codec can read a page set written
/// with compact cells and insert into it. The inserted cell may be of either
/// family; what must hold is that every record — old and new — still reads
/// back exactly, and that the page still passes the canonical decoder.
#[test]
fn a_plain_codec_tree_reads_and_extends_a_compact_page() {
    let dir = tempfile::tempdir().unwrap();
    let pool = pool_in(dir.path(), 32);
    let rows = mixed_family_records();
    let root = leaf_page(&pool, 1, &rows.iter().map(|r| r.2.clone()).collect::<Vec<_>>());

    let (last, hits, attempts) = (Cell::new(None), Cell::new(0), Cell::new(0));
    let mut tree = BTree::open(&pool, 1, root, &last, &hits, &attempts);

    let new_key = b"\x03inserted".to_vec();
    let new_val = b"written-by-the-opening-build".to_vec();
    tree.insert(&new_key, &new_val)
        .unwrap_or_else(|e| panic!("insert into a compact page refused: {e:?}"));

    let mut expected: Vec<(Vec<u8>, Vec<u8>)> =
        rows.iter().map(|(k, v, _)| (k.clone(), v.clone())).collect();
    expected.push((new_key.clone(), new_val.clone()));
    expected.sort();

    let scanned: Vec<(Vec<u8>, Vec<u8>)> =
        tree.range(b"").unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(scanned, expected, "every record survives the mixed-family insert");

    for (k, v) in &expected {
        assert_eq!(tree.get(k).unwrap().as_deref(), Some(&v[..]), "point read after insert");
    }

    // Every cell on the page, old family and new, decodes through the one
    // canonical decoder — no record was written in a shape only this build
    // can read back.
    let read = pool.get(root).unwrap();
    // A resident pool page carries its checksum only once it is written out;
    // `open_resident` is the same boundary the B-tree itself reads through.
    let page = PageRef::open_resident(&read, root).unwrap();
    assert_eq!(page.nentries(), expected.len());
    for i in 0..page.nentries() {
        decode_record(page.slot(i), root, PageKind::Leaf)
            .unwrap_or_else(|e| panic!("slot {i} is not decodable by this build: {e:?}"));
    }
}

/// The declared encoding, in both directions, in one build.
///
/// Property: whichever encoding a database declares, a tree running it reads
/// every record of a page already holding the OTHER family, and the cells it
/// writes are of the DECLARED family — not of whatever the binary was
/// compiled with. Before the fix the encoder was `#[cfg]`-selected, so one of
/// these two directions was unreachable in any single build.
#[test]
fn the_declared_codec_decides_the_written_family_in_both_directions() {
    for compact in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool_in(dir.path(), 32);
        pool.set_compact_cells(compact);
        assert_eq!(pool.compact_cells(), compact);

        let rows = mixed_family_records();
        let root = leaf_page(&pool, 1, &rows.iter().map(|r| r.2.clone()).collect::<Vec<_>>());
        let (last, hits, attempts) = (Cell::new(None), Cell::new(0), Cell::new(0));
        let mut tree = BTree::open(&pool, 1, root, &last, &hits, &attempts);

        let new_key = b"\x03inserted".to_vec();
        let new_val = b"written-in-the-declared-encoding".to_vec();
        tree.insert(&new_key, &new_val).unwrap();

        let mut expected: Vec<(Vec<u8>, Vec<u8>)> =
            rows.iter().map(|(k, v, _)| (k.clone(), v.clone())).collect();
        expected.push((new_key.clone(), new_val.clone()));
        expected.sort();
        let scanned: Vec<(Vec<u8>, Vec<u8>)> =
            tree.range(b"").unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(scanned, expected, "compact={compact}: every record survives");

        // The new cell's own bytes say which family it is.
        let read = pool.get(root).unwrap();
        let page = PageRef::open_resident(&read, root).unwrap();
        let slot = (0..page.nentries())
            .map(|i| page.slot(i))
            .find(|rec| {
                matches!(decode_record(rec, root, PageKind::Leaf).unwrap(),
                    DecodedRecord::Leaf { key, .. } if key == &new_key[..])
            })
            .expect("the inserted record is on the page");
        let is_compact = slot[1] & 0xf0 == 0x40;
        assert_eq!(is_compact, compact, "compact={compact}: written cell family");
    }
}
