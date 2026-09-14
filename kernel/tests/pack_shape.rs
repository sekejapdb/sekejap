//! The packed tree is an on-disk artefact. Streaming each level's separators
//! through a scratch file instead of holding them in a `Vec` must change
//! nothing a reader can observe -- not one byte of one page -- and must leave
//! no scratch behind, on the failure path as much as the success one.
//! Compact ordinary cells intentionally change the file format: their golden
//! fingerprints are separate and backed by exact independent cell-byte checks.

use std::cell::Cell;
use std::sync::Arc;

use kernel::budget::MemoryBudget;
use kernel::io::{open_file, IoMode};
use kernel::page::{checksum, PageKind, PageRef, PAGE_SIZE};
use kernel::pool::BufferPool;
use kernel::Result;

fn pool_in(dir: &std::path::Path, frames: usize) -> BufferPool {
    let (file, _) = open_file(&dir.join("data"), IoMode::Buffered).unwrap();
    let budget = Arc::new(MemoryBudget::new(frames * PAGE_SIZE + (1 << 20)));
    BufferPool::new(file.into(), budget, frames).unwrap()
}

/// A deterministic input with varying value lengths, so page boundaries do not
/// all land at the same offset and an off-by-one in the streamed lookahead
/// would move them.
fn rows(n: u64) -> impl Iterator<Item = Result<(Vec<u8>, Vec<u8>, bool)>> {
    (0..n).map(|i| Ok((i.to_be_bytes().to_vec(), vec![b'a' + (i % 7) as u8; 1 + (i % 7) as usize], false)))
}

/// Fold every page of the store into one number: contents, order and page
/// count all in a single value. Uses the crate's own page checksum so the test
/// needs no hashing dependency of its own.
fn digest(pool: &BufferPool) -> (u32, u32) {
    // The packed tree is an on-disk artefact -- digest what is ON DISK.
    pool.flush_all(kernel::io::Barrier::None).unwrap();
    let n = pool.page_count();
    let mut acc: u32 = 0;
    let mut buf = vec![0u8; PAGE_SIZE];
    for p in 0..n {
        let g = pool.get(p).unwrap();
        buf.copy_from_slice(&g[..]);
        drop(g);
        // Chain the running value into the bytes being summed, so page order
        // matters and a permutation of identical pages cannot collide.
        buf[0..4].copy_from_slice(&acc.to_le_bytes());
        acc = checksum(&buf);
    }
    (acc, n)
}

/// Walk the leaf sibling chain from `first`. `recover.rs` reconstructs lost
/// leaves from exactly this chain, so a pack that stitched it wrong breaks
/// repair, not only scans.
fn chain(pool: &BufferPool, first: u32) -> (u32, u64) {
    // Bounded by the page count. A `next_leaf` that points backwards or at
    // itself is exactly the damage this helper exists to name, and an
    // unbounded walk answers it by hanging -- a killed ten-minute CI job is a
    // worse signal than a failed assertion, and it is not even a signal about
    // the right thing.
    let cap = pool.page_count();
    let mut no = first;
    let (mut leaves, mut entries) = (0u32, 0u64);
    loop {
        assert!(leaves <= cap,
                "the sibling chain visited more than the {cap} pages that exist -- it is cyclic");
        let g = pool.get(no).unwrap();
        let p = PageRef::open(&g[..], no).unwrap();
        assert_eq!(p.kind(), PageKind::Leaf, "the sibling chain must only visit leaves");
        leaves += 1;
        entries += p.nentries() as u64;
        let nx = p.next_leaf();
        drop(g);
        if nx == 0 { break; }
        no = nx;
    }
    (leaves, entries)
}

/// Page numbers at each level, root level first. The last entry is the leaf
/// level, so `levels.len()` is the tree's height.
fn levels(pool: &BufferPool, root: u32) -> Vec<Vec<u32>> {
    // Same bound, same reason: a `child0` cycle would otherwise hang.
    let cap = pool.page_count() as usize;
    let mut out = vec![vec![root]];
    loop {
        assert!(out.len() <= cap,
                "the tree is deeper than the {cap} pages that exist -- a child pointer is cyclic");
        let cur = out.last().unwrap().clone();
        let mut next = Vec::new();
        let mut is_leaf = false;
        for no in &cur {
            let g = pool.get(*no).unwrap();
            let p = // resident pool page: checksum is stamped at the WRITE now, so a dirty
            // frame's CRC field describes the last write, not current contents.
            PageRef::open_resident(&g[..], *no).unwrap();
            if p.kind() == PageKind::Leaf { is_leaf = true; break; }
            next.push(p.child0());
            for i in 0..p.nentries() {
                let s = p.slot(i);
                let kl = u16::from_le_bytes([s[0], s[1]]) as usize;
                next.push(u32::from_le_bytes(s[2 + kl..2 + kl + 4].try_into().unwrap()));
            }
        }
        if is_leaf { return out; }
        out.push(next);
    }
}

/// usable = `((4096 - 40) as f32 * 0.9) as usize` = 3650. A 40-byte key with a
/// 2-byte value makes the LEAF record need exactly 50 bytes (2 + 40 + 2 + 2 + 4)
/// (four value bytes with compact framing) and the SEPARATOR record need exactly 50 too (2 + 40 + 4 + 4) -- and
/// 3650 = 73 x 50, so both fill loops land on `used + need == usable` exactly,
/// on every page, by construction rather than by chance.
///
/// That boundary is the one the fixed-8-byte and fixed-200-byte fixtures never
/// reach, which is why the review found that `used + need <= usable` -> `<`
/// (leaf) and `used + need > usable` -> `>=` (interior) both shipped green. The
/// first round of this work mutated the interior predicate, observed no change,
/// and recorded it as an equivalent mutant; it is not equivalent, it was a
/// property of the fixture. Both mutations move this fixture's digest.
fn exact_fit(n: u64) -> impl Iterator<Item = Result<(Vec<u8>, Vec<u8>, bool)>> {
    (0..n).map(|i| {
        let mut k = i.to_be_bytes().to_vec();
        k.resize(40, b'e');
        Ok((k, vec![b'v'; if cfg!(feature = "compact-cells") { 4 } else { 2 }], false))
    })
}

/// Irregular KEY lengths as well as value lengths. Key length is what drives
/// separator record size, so a fixture with fixed-width keys leaves every
/// separator the same size no matter how much the values vary -- the interior
/// fill loop then sees one repeating shape and page boundaries land in the same
/// place every time. `mix` is splitmix64 over a fixed seed, so the shape is
/// identical here and in the e4add4b worktree the snapshot came from.
fn mix(i: u64) -> u64 {
    let z = i.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(0x1234_5678_9ABC_DEF0);
    let mut x = z;
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn jagged(n: u64) -> impl Iterator<Item = Result<(Vec<u8>, Vec<u8>, bool)>> {
    (0..n).map(|i| {
        let h = mix(i);
        // Keys stay sorted and unique whatever the padding: the 8-byte
        // big-endian index comes first, so lexicographic order is index order.
        let mut k = i.to_be_bytes().to_vec();
        k.resize(8 + (h % 41) as usize, b'k');
        Ok((k, vec![b'v'; ((h >> 13) % 49) as usize], false))
    })
}

/// One packed tree, described by everything a reader could observe about it.
fn observe(pool: &BufferPool, root: u32) -> (u32, usize, u32, u32, u64, u32, u32) {
    let lv = levels(pool, root);
    let first_leaf = lv.last().unwrap()[0];
    let (leaves, entries) = chain(pool, first_leaf);
    let (dg, pages) = digest(pool);
    (root, lv.len(), first_leaf, leaves, entries, pages, dg)
}

fn pack_and_observe(
    it: impl Iterator<Item = Result<(Vec<u8>, Vec<u8>, bool)>>,
) -> (u32, usize, u32, u32, u64, u32, u32) {
    let d = tempfile::tempdir().unwrap();
    let pool = pool_in(d.path(), 64);
    let _ = pool.allocate().unwrap(); // page 0 is the superblock; reserve it
    // This fixed-size format fixture intentionally retains its input oracle;
    // it is not a bounded-memory benchmark. Do not use engine encoders to
    // derive the expected ordinary cell bytes.
    let expected: Vec<_> = it.map(Result::unwrap).collect();
    let root = kernel::bulk::pack_tree(&pool, 1,
        expected.iter().cloned().map(Ok), 0.9, &d.path().join("scratch")).unwrap();
    let (last, hits, tries) = (Cell::new(None), Cell::new(0), Cell::new(0));
    let tree = kernel::btree::BTree::open(&pool, 1, root, &last, &hits, &tries);
    let mut scan = tree.range(&[]).unwrap();
    for (key, value, marker) in &expected {
        assert!(!marker);
        assert_eq!(tree.get(key).unwrap().as_deref(), Some(value.as_slice()));
        assert_eq!(scan.next().unwrap().unwrap(), (key.clone(), value.clone()));
    }
    assert!(scan.next().is_none());
    drop(scan);
    pool.flush_all(kernel::io::Barrier::None).unwrap();
    kernel::verify::verify_published_tree(&d.path().join("data"), IoMode::Buffered, root, 1).unwrap();
    let leaf_ids = levels(&pool, root).pop().unwrap();
    let mut input = expected.iter().peekable();
    for (index, no) in leaf_ids.iter().enumerate() {
        let page = pool.get(*no).unwrap();
        let page = PageRef::open(&page[..], *no).unwrap();
        assert_eq!(page.next_leaf(), leaf_ids.get(index + 1).copied().unwrap_or(0));
        let mut used = 0;
        for slot in 0..page.nentries() {
            let (key, value, _) = input.next().unwrap();
            let mut bytes = Vec::new();
            let header = key.len() as u16 | if cfg!(feature = "compact-cells") { 0x4000 } else { 0 };
            bytes.extend(header.to_le_bytes());
            bytes.extend(key);
            if !cfg!(feature = "compact-cells") { bytes.extend((value.len() as u16).to_le_bytes()); }
            bytes.extend(value);
            assert_eq!(page.slot(slot), bytes, "independent cell encoding for {key:?}");
            used += bytes.len() + 4;
        }
        assert!(used <= 3650);
        if let Some((key, value, _)) = input.peek() {
            let next_size = key.len() + value.len() + if cfg!(feature = "compact-cells") { 6 } else { 8 };
            assert!(used + next_size > 3650, "premature leaf boundary");
        }
    }
    assert!(input.next().is_none());
    observe(&pool, root)
}

/// Byte-for-byte identity with the tree the level-holding implementation
/// built.
///
/// These constants were captured by running this exact fixture against the old
/// `Vec`-accumulating `pack_tree` at e4add4b, BEFORE the streaming change --
/// they are a snapshot of the previous behaviour, not of the new one, which is
/// the only way this test can say anything about a change that already
/// happened.
#[test]
fn pack_tree_produces_identical_tree() {
    // (root, height, first_leaf, leaves, entries, pages, digest), each captured
    // by running the same fixture through the Vec-accumulating `pack_tree` in a
    // worktree at e4add4b. The 8-byte row was reproduced independently by the
    // reviewer, from that worktree, and matched.
    let cases: [(&str, (u32, usize, u32, u32, u64, u32, u32)); 3] = [
        ("fixed 8-byte keys, 50k rows",
         pack_and_observe(rows(50_000))),
        ("exact-fit 40-byte keys, 60k rows",
         pack_and_observe(exact_fit(60_000))),
        ("jagged 8..48-byte keys, 60k rows",
         pack_and_observe(jagged(60_000))),
    ];
    // Digests re-taken twice, deliberately, with every STRUCTURAL field --
    // root, height, first leaf, leaf count, entries, pages -- unchanged
    // each time: (1) when allocate() stopped handing out zero frames
    // (1924792605, 1868071275, 1375923709); (2) at 2n step A, when seal
    // began stamping the publishing generation into the formerly-always-0
    // lsn field -- every page's bytes change by design, so the chained
    // page-checksum digest must move with them.
    #[cfg(not(feature = "compact-cells"))]
    let want: [(u32, usize, u32, u32, u64, u32, u32); 3] = [
        (278,  3, 1, 275, 50_000, 279,  1320246642),
        (835,  3, 1, 822, 60_000, 836,  2156132941),
        (1006, 3, 1, 994, 60_000, 1007, 1305404169),
    ];
    // Native capture after all cell bytes, point lookups, scans, page fill and
    // published-tree structure were independently checked above. Compact framing
    // removes two bytes per ordinary cell; this intentionally changes density.
    #[cfg(feature = "compact-cells")]
    let want: [(u32, usize, u32, u32, u64, u32, u32); 3] = [
        (251, 3, 1, 248, 50_000, 252, 944332828),
        (835, 3, 1, 822, 60_000, 836, 800627646),
        (973, 3, 1, 961, 60_000, 974, 522263165),
    ];
    for (i, ((name, got), want)) in cases.iter().zip(want.iter()).enumerate() {
        assert_eq!(
            got, want,
            "case {i} ({name}): the packed tree changed its feature-specific format fingerprint. Fields are (root, height, first_leaf, leaves, entries, \
             pages, digest); the digest chains the crate's own page checksum over every \
             page in the store, so contents, page order and page count are all in it. \
             Legacy constants originate at e4add4b; compact constants follow the independently checked format revision."
        );
    }
}

/// Streaming leaves into a root exercises one file. A tree deep enough to feed
/// a spilled level back in to produce ANOTHER spilled level is what actually
/// exercises level-to-level streaming -- and it is where a temp name reused
/// across levels would truncate a file still being read.
#[test]
fn pack_tree_multi_level() {
    let d = tempfile::tempdir().unwrap();
    let scratch = d.path().join("scratch");
    let pool = pool_in(d.path(), 64);
    let _ = pool.allocate().unwrap();

    // Long keys shrink the fanout, so a tree several levels deep costs 20k
    // rows instead of millions.
    let n = 20_000u64;
    let key = |i: u64| { let mut k = i.to_be_bytes().to_vec(); k.resize(200, b'x'); k };
    let it = (0..n).map(|i| Ok((key(i), i.to_le_bytes().to_vec(), false)));
    let root = kernel::bulk::pack_tree(&pool, 1, it, 0.9, &scratch).unwrap();

    let lv = levels(&pool, root);
    assert!(lv.len() >= 3, "the fixture must produce at least three levels, got {}", lv.len());
    assert!(lv[1].len() > 1, "the level under the root must have several pages, got {}", lv[1].len());

    let (leaves, entries) = chain(&pool, lv.last().unwrap()[0]);
    assert_eq!(leaves as usize, lv.last().unwrap().len(),
               "the sibling chain must visit exactly the leaves the tree points at");
    assert_eq!(entries, n, "the sibling chain must reach every entry");

    // Every level's separators must actually route: point lookup and scan have
    // to agree on all of them, through every interior level.
    let (last, hits, tries) = (Cell::new(None), Cell::new(0), Cell::new(0));
    let t = kernel::btree::BTree::open(&pool, 1, root, &last, &hits, &tries);
    for i in 0..n {
        assert_eq!(t.get(&key(i)).unwrap().as_deref(), Some(&i.to_le_bytes()[..]),
                   "key {i} unreachable by descent through a multi-level packed tree");
    }
    let scanned: Vec<Vec<u8>> = t.range(&[]).unwrap().map(|r| r.unwrap().0).collect();
    assert_eq!(scanned.len() as u64, n, "a full scan must see every key");
    assert!(scanned.windows(2).all(|w| w[0] < w[1]), "a full scan must be strictly ascending");
}

/// 30,857 fixed-size rows make exactly 204 leaves: an interior page holds 203
/// children at the 90% target, so a greedy split used to strand the final
/// child in an invalid zero-slot interior page. The independent verifier then
/// refused the packed candidate at the 100k CREATE INDEX gate.
#[test]
fn pack_tree_never_strands_a_single_interior_child() {
    let d = tempfile::tempdir().unwrap();
    let pool = pool_in(d.path(), 64);
    let _ = pool.allocate().unwrap();
    // Keep the same 203 full leaves plus one final child in both formats.
    let n = if cfg!(feature = "compact-cells") { 33_496u64 } else { 30_857u64 };
    let it = (0..n).map(|i| {
        Ok((i.to_be_bytes().to_vec(), i.to_le_bytes().to_vec(), false))
    });
    let root = kernel::bulk::pack_tree(&pool, 1, it, 0.9, &d.path().join("scratch")).unwrap();

    for level in levels(&pool, root) {
        for no in level {
            let g = pool.get(no).unwrap();
            let p = PageRef::open_resident(&g[..], no).unwrap();
            if p.kind() == PageKind::Interior {
                assert!(p.nentries() >= 1,
                        "interior page {no} has only child0 and cannot be a valid subtree");
            }
        }
    }
}

/// The Law 4 number: how much scratch the spilled separators actually cost,
/// against the tree they build.
///
/// Measured, not derived: a watcher thread polls the scratch directory while
/// the pack runs and records the largest total it ever sees there. Deriving
/// the number from the finished tree's page counts would only restate the
/// record layout back to itself -- it could not fail if the spill started
/// carrying one entry per ROW instead of one per PAGE, which is precisely the
/// regression that would make this claim false.
///
/// The fixture is sized so the level-0 file exceeds the 256 KiB write buffer
/// and therefore reaches the filesystem well before it is sealed, rather than
/// existing only in the instant between `seal` and unlink.
#[test]
fn pack_tree_spill_is_a_small_fraction_of_the_tree() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    let d = tempfile::tempdir().unwrap();
    let scratch = d.path().join("scratch");
    let pool = pool_in(d.path(), 64);
    let _ = pool.allocate().unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicU64::new(0));
    let watcher = std::thread::spawn({
        let (dir, stop, peak) = (scratch.clone(), stop.clone(), peak.clone());
        move || while !stop.load(Ordering::Relaxed) {
            let mut total = 0u64;
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() { if let Ok(m) = e.metadata() { total += m.len(); } }
            }
            peak.fetch_max(total, Ordering::Relaxed);
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
    });

    let root = kernel::bulk::pack_tree(&pool, 1, rows(2_000_000), 0.9, &scratch).unwrap();
    stop.store(true, Ordering::Relaxed);
    watcher.join().unwrap();

    let spill = peak.load(Ordering::Relaxed);
    let tree = pool.page_count() as u64 * PAGE_SIZE as u64;
    let pct = spill as f64 * 100.0 / tree as f64;
    let lv = levels(&pool, root);
    eprintln!("peak scratch {spill} B against a {tree} B tree of {} levels -- {pct:.3}%", lv.len());
    assert!(spill > 0, "the watcher never saw a level file; the measurement proves nothing");
    // The four-byte per-record checksum raises the measured spill from 0.488%
    // to 0.586%. Keep the existing 0.6% assertion: it still catches a spill
    // that grows beyond the documented framing cost.
    assert!(pct < 0.6,
            "spilled separators cost {pct:.3}% of the tree ({spill} B against {tree} B); \
             the module doc claims ~0.59% for 8-byte keys, and a spill that is per-row \
             rather than per-page is what makes that false");
}

/// A level file that comes back damaged must be refused, not packed.
///
/// The spill put a new intermediate on disk, and the review showed what an
/// unchecked one costs: a level-0 file short by ONE 20-byte separator leaves an
/// entire subtree with no parent pointing at it, so `get` misses 138 of 50,000
/// keys while `scan` still returns all 50,000 -- because the leaf sibling chain
/// is stitched independently of the separator file. `bulk_load` returned `Ok`.
/// That is the point-lookup/scan divergence `Error::DuplicateKey` exists to
/// refuse, and a full-scan row count -- the check the 209M-row load was
/// verified with -- cannot see it.
///
/// This damages the level-0 file from inside the input iterator, once the
/// 256 KiB write buffer has flushed a prefix of it to the filesystem. The
/// fixture uses 3600-byte values so every row gets its own leaf and therefore
/// its own separator, which is what makes the file exceed the buffer within a
/// tractable number of rows.
///
/// Note what this does and does not reach. Removing bytes from the flushed
/// prefix while the writer is still appending leaves a hole rather than a short
/// file -- the writer's offset does not move -- so this exercises `read_sep`'s
/// refusal of a record whose value is not a 4-byte page number. A genuinely
/// SHORT level file needs the truncation to land after `seal` and before
/// `reader`, a window with no test-reachable edge; the `seen != level.count`
/// guard covering that case was verified by injecting the truncation into
/// `Separators::reader` directly, which reproduced the review's silent
/// divergence with the guard removed and `Err("a separator level came back with
/// 233 of 234 entries")` with it in place. Both refusals are on the same read
/// path; this test is the standing half.
#[test]
fn a_damaged_level_file_is_refused_not_packed() {
    let d = tempfile::tempdir().unwrap();
    let scratch = d.path().join("scratch");
    let pool = pool_in(d.path(), 64);
    let _ = pool.allocate().unwrap();

    let cut_at = 13_800u64;
    let observed = std::cell::Cell::new(0u64);
    let it = (0..14_500u64).map(|i| {
        if i == cut_at {
            let f = std::fs::read_dir(&scratch).unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| p.extension().is_some_and(|x| x == "tmp"))
                .expect("a level file to damage");
            let h = std::fs::OpenOptions::new().write(true).open(&f).unwrap();
            let len = h.metadata().unwrap().len();
            observed.set(len);
            h.set_len(len - 20).unwrap();
        }
        Ok((i.to_be_bytes().to_vec(), vec![b'v'; 3600], false))
    });
    let err = kernel::bulk::pack_tree(&pool, 1, it, 0.9, &scratch).unwrap_err();

    // The injection has to have bitten something. If the write buffer had not
    // flushed yet the file would be empty and this test would be asserting
    // nothing -- the same "test that cannot fail" shape it exists to close.
    assert!(observed.get() >= 262_140,
            "the fixture must damage a flushed prefix, but the level file was only {} B",
            observed.get());
    assert!(matches!(err, kernel::Error::Io(ref e) if e.kind() == std::io::ErrorKind::InvalidData),
            "a damaged level file must be refused as invalid data, not packed into a tree \
             that answers `get` and `range` differently: {err:?}");
}

/// A successful pack must leave the scratch directory empty, and so must a
/// failed one -- 50 GB of separators left behind after an error is its own
/// outage.
#[test]
fn pack_tree_cleans_up_temp_files() {
    let empty = |p: &std::path::Path| -> Vec<String> {
        std::fs::read_dir(p).unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect()
    };

    // Success, on a tree deep enough that several level files were created.
    let d = tempfile::tempdir().unwrap();
    let scratch = d.path().join("scratch");
    {
        let pool = pool_in(d.path(), 64);
        let _ = pool.allocate().unwrap();
        let key = |i: u64| { let mut k = i.to_be_bytes().to_vec(); k.resize(200, b'x'); k };
        let it = (0..20_000u64).map(|i| Ok((key(i), b"v".to_vec(), false)));
        kernel::bulk::pack_tree(&pool, 1, it, 0.9, &scratch).unwrap();
    }
    assert!(empty(&scratch).is_empty(), "a successful pack left scratch behind: {:?}", empty(&scratch));

    // Failure partway through the LEAF level: a duplicate key at row 30,000.
    let d = tempfile::tempdir().unwrap();
    let scratch = d.path().join("scratch");
    {
        let pool = pool_in(d.path(), 64);
        let _ = pool.allocate().unwrap();
        let it = (0..50_000u64).map(|i| {
            let k = if i == 30_000 { 29_999u64 } else { i };
            Ok((k.to_be_bytes().to_vec(), b"v".to_vec(), false))
        });
        let e = kernel::bulk::pack_tree(&pool, 1, it, 0.9, &scratch).unwrap_err();
        assert!(matches!(e, kernel::Error::DuplicateKey), "expected DuplicateKey, got {e:?}");
    }
    assert!(empty(&scratch).is_empty(), "a failed pack left scratch behind: {:?}", empty(&scratch));

    // Failure partway through an INTERIOR level, which is the case that has a
    // sealed level file open for reading AND a new one open for writing. A
    // 4048-byte key fits a leaf record (4052 + 4 == capacity) but its
    // separator does not (4054 + 4 > capacity), so this fails only once the
    // leaf level is complete and the interior build has started.
    let d = tempfile::tempdir().unwrap();
    let scratch = d.path().join("scratch");
    {
        let pool = pool_in(d.path(), 64);
        let _ = pool.allocate().unwrap();
        let key = |i: u8| { let mut k = vec![i]; k.resize(4048, b'z'); k };
        let it = (0..2u8).map(|i| Ok((key(i), Vec::new(), false)));
        let e = kernel::bulk::pack_tree(&pool, 1, it, 0.9, &scratch).unwrap_err();
        assert!(matches!(e, kernel::Error::TooLarge),
                "the fixture must fail in the interior build, got {e:?}");
    }
    assert!(empty(&scratch).is_empty(),
            "a pack that failed between two live level files left scratch behind: {:?}", empty(&scratch));
}

#[test]
fn an_overflow_marker_must_be_exactly_twelve_bytes() {
    for len in [11usize, 13] {
        let d = tempfile::tempdir().unwrap();
        let scratch = d.path().join("scratch");
        let pool = pool_in(d.path(), 16);
        let _ = pool.allocate().unwrap();
        let item = std::iter::once(Ok((b"marker".to_vec(), vec![0; len], true)));
        assert!(matches!(
            kernel::bulk::pack_tree(&pool, 1, item, 0.9, &scratch),
            Err(kernel::Error::Io(ref e)) if e.kind() == std::io::ErrorKind::InvalidData
        ), "a {len}-byte overflow marker must be refused rather than sliced");
    }
}
