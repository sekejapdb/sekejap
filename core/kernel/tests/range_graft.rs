//! `BTree::graft_sorted_range` splices a packed run into an EMPTY interval of
//! a tree that already holds other keys. The properties under test are the
//! ones a wrong splice breaks quietly:
//!
//! - the ordering oracle (docs/PAIR_PACKING.md): a full scan returns every key
//!   exactly once, in byte order. Row count alone passed while ordering failed
//!   there, so the count is never the whole check here either;
//! - descent agrees with the scan: every key the scan returns is reachable by
//!   `get`, and an independent recursive walk that checks every key against
//!   the interval its ancestors' separators claim for it returns the same
//!   sequence. The divergence between the two is the graft's characteristic
//!   defect (a run anchored under a stale separator: scans see it, seeks do
//!   not). The `next_leaf` chain is deliberately NOT the oracle here: this
//!   tree copies a page on write and leaves the old neighbour's pointer
//!   naming the stale version, which is why `RangeIter` walks the parent path
//!   and why `verify_published_tree` dropped the chain requirement. What the
//!   chain must still be exact in is the freshly PACKED run, and
//!   `verify_range_pool` checks that before the splice;
//! - a non-empty interval is refused with nothing written;
//! - ordinary inserts and deletes into and around the grafted run still leave
//!   both oracles true afterwards.

use kernel::btree::BTree;
use kernel::budget::MemoryBudget;
use kernel::io::{open_file, Barrier, IoMode};
use kernel::page::{PageKind, PageRef};
use kernel::verify::{decode_record, DecodedRecord};
use kernel::pool::BufferPool;
use std::cell::Cell;
use std::sync::Arc;

const TREE: u16 = 1;

struct Fixture {
    pool: BufferPool,
    root: u32,
    last: Cell<Option<u32>>,
    hits: Cell<u64>,
    attempts: Cell<u64>,
    scratch: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let (file, _) = open_file(&dir.path().join("data"), IoMode::Buffered).unwrap();
        let file: Arc<dyn kernel::io::FileIo> = file.into();
        let frames = 64;
        let pool =
            BufferPool::new(file, Arc::new(MemoryBudget::new(frames * 4096)), frames).unwrap();
        pool.set_stamp_gen(1);
        // Page 0 is the superblock everywhere in this format; a tree page
        // numbered 0 would make the `next_leaf == 0` sentinel ambiguous.
        drop(pool.allocate().unwrap());
        let last = Cell::new(None);
        let hits = Cell::new(0);
        let attempts = Cell::new(0);
        let root = BTree::create(&pool, TREE, &last, &hits, &attempts).unwrap().root();
        let scratch = dir.path().join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        Fixture { pool, root, last, hits, attempts, scratch, _dir: dir }
    }

    fn tree(&self) -> BTree<'_> {
        BTree::open(&self.pool, TREE, self.root, &self.last, &self.hits, &self.attempts)
    }

    fn insert(&mut self, key: &[u8], value: &[u8]) {
        let mut tree = self.tree();
        tree.insert(key, value).unwrap();
        self.root = tree.root();
    }

    fn delete(&mut self, key: &[u8]) -> bool {
        let mut tree = self.tree();
        let gone = tree.delete(key).unwrap();
        self.root = tree.root();
        gone
    }

    fn graft(&mut self, rows: &[(Vec<u8>, Vec<u8>)]) -> kernel::Result<()> {
        let min = rows[0].0.clone();
        let max = rows[rows.len() - 1].0.clone();
        let stream = rows.iter().map(|(k, v)| Ok((k.clone(), v.clone(), false)));
        let mut tree = self.tree();
        let outcome =
            tree.graft_sorted_range(stream, rows.len() as u64, &min, &max, &self.scratch);
        // The root moves only on success; on a refusal it is unchanged, which
        // is half of "nothing was written".
        self.root = tree.root();
        let retired = outcome?;
        for page in retired {
            self.pool.free_page(page).unwrap();
        }
        Ok(())
    }

    /// Full scan through the parent path (what `RangeIter` does).
    fn scan(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.tree().range(&[]).unwrap().map(|row| row.unwrap()).collect()
    }

    /// Independent recursive descent: every separator inside its parent's
    /// interval, every key inside the interval its ancestors claim, keys in
    /// order. This is `verify_published_tree`'s check, run over the live pool.
    fn descent_walk(&self) -> Vec<Vec<u8>> {
        fn walk(
            pool: &BufferPool,
            page_no: u32,
            lower: Option<&[u8]>,
            upper: Option<&[u8]>,
            depth: usize,
            out: &mut Vec<Vec<u8>>,
        ) {
            assert!(depth < 32, "tree is deeper than the format bound");
            let read = pool.get(page_no).unwrap();
            let page = PageRef::open_resident(&read, page_no).unwrap();
            assert_eq!(page.tree_id(), TREE, "page {page_no} belongs to another tree");
            if page.kind() == PageKind::Leaf {
                for i in 0..page.nentries() {
                    let DecodedRecord::Leaf { key, .. } =
                        decode_record(page.slot(i), page_no, PageKind::Leaf).unwrap()
                    else {
                        panic!("leaf {page_no} holds an interior record")
                    };
                    assert!(lower.is_none_or(|b| key >= b), "key below its separator");
                    assert!(upper.is_none_or(|b| key < b), "key at or above its separator");
                    out.push(key.to_vec());
                }
                return;
            }
            assert_eq!(page.kind(), PageKind::Interior, "page {page_no} is not a tree page");
            let mut separators = Vec::new();
            for i in 0..page.nentries() {
                let DecodedRecord::Interior { key, child } =
                    decode_record(page.slot(i), page_no, PageKind::Interior).unwrap()
                else {
                    panic!("interior {page_no} holds a leaf record")
                };
                assert!(
                    separators.last().is_none_or(|(p, _): &(Vec<u8>, u32)| key > p.as_slice()),
                    "separators out of order on page {page_no}"
                );
                assert!(lower.is_none_or(|b| key >= b) && upper.is_none_or(|b| key < b),
                    "separator outside its parent's interval on page {page_no}");
                separators.push((key.to_vec(), child));
            }
            for i in 0..=separators.len() {
                let child = if i == 0 { page.child0() } else { separators[i - 1].1 };
                let child_lower = if i == 0 { lower } else { Some(separators[i - 1].0.as_slice()) };
                let child_upper =
                    if i == separators.len() { upper } else { Some(separators[i].0.as_slice()) };
                walk(pool, child, child_lower, child_upper, depth + 1, out);
            }
        }
        let mut keys = Vec::new();
        walk(&self.pool, self.root, None, None, 0, &mut keys);
        keys
    }

    /// Both oracles at once, against the exact expected key set.
    fn assert_sound(&self, expected: &[(Vec<u8>, Vec<u8>)]) {
        let scanned = self.scan();
        let keys: Vec<Vec<u8>> = scanned.iter().map(|(k, _)| k.clone()).collect();
        let mut ordered = keys.clone();
        ordered.sort();
        ordered.dedup();
        assert_eq!(keys, ordered, "scan is not in strict byte order");
        assert_eq!(scanned, expected, "scan does not hold exactly the expected rows");
        assert_eq!(self.descent_walk(), keys, "descent and the parent-path scan disagree");
        let tree = self.tree();
        for (key, value) in expected {
            assert_eq!(
                tree.get(key).unwrap().as_deref(),
                Some(value.as_slice()),
                "a key the scan returned is unreachable by descent"
            );
        }
    }
}

/// Ordinary keys under tag 0x10, so a grafted tag sorts left, right or inside.
fn standing(i: u64) -> Vec<u8> {
    let mut k = vec![0x10u8];
    k.extend_from_slice(&i.to_be_bytes());
    k
}

fn grafted(tag: u8, i: u64) -> Vec<u8> {
    let mut k = vec![tag];
    k.extend_from_slice(&i.to_be_bytes());
    k
}

fn run(tag: u8, n: u64) -> Vec<(Vec<u8>, Vec<u8>)> {
    (0..n).map(|i| (grafted(tag, i), format!("v{i}").into_bytes())).collect()
}

fn populated(n: u64) -> (Fixture, Vec<(Vec<u8>, Vec<u8>)>) {
    let mut f = Fixture::new();
    let mut rows = Vec::new();
    for i in 0..n {
        // Scattered, not ascending: an ascending fill would leave the tree in
        // the one shape the append split already optimises.
        let key = standing((i * 7919) % n);
        let value = format!("row {i}").into_bytes();
        f.insert(&key, &value);
        rows.push((key, value));
    }
    rows.sort();
    rows.dedup_by(|a, b| a.0 == b.0);
    (f, rows)
}

fn merged(
    standing: &[(Vec<u8>, Vec<u8>)],
    run: &[(Vec<u8>, Vec<u8>)],
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut all = standing.to_vec();
    all.extend_from_slice(run);
    all.sort();
    all
}

#[test]
fn a_run_grafts_into_an_empty_interval_in_the_middle_of_a_populated_tree() {
    let (mut f, standing_rows) = populated(2_000);
    // Tag 0x08 sorts below 0x10, 0x20 above it; 0x10 with a high suffix lands
    // strictly INSIDE the standing interval, which is the mid-tree case.
    let mut rows = Vec::new();
    for i in 0..1_500u64 {
        let mut k = vec![0x10u8];
        k.extend_from_slice(&(u64::MAX / 2 + i).to_be_bytes());
        rows.push((k, format!("mid{i}").into_bytes()));
    }
    f.graft(&rows).unwrap();
    f.assert_sound(&merged(&standing_rows, &rows));
}

#[test]
fn a_run_grafts_at_the_far_right_of_a_populated_tree() {
    let (mut f, standing_rows) = populated(2_000);
    let rows = run(0x20, 1_500);
    f.graft(&rows).unwrap();
    f.assert_sound(&merged(&standing_rows, &rows));
}

#[test]
fn a_run_grafts_at_the_far_left_of_a_populated_tree() {
    let (mut f, standing_rows) = populated(2_000);
    let rows = run(0x08, 1_500);
    f.graft(&rows).unwrap();
    f.assert_sound(&merged(&standing_rows, &rows));
}

#[test]
fn a_run_grafts_into_an_empty_tree() {
    let mut f = Fixture::new();
    let rows = run(0x20, 900);
    f.graft(&rows).unwrap();
    f.assert_sound(&rows);
}

#[test]
fn grafting_into_a_non_empty_interval_is_refused_before_any_page_changes() {
    let (mut f, standing_rows) = populated(1_000);
    let rows = run(0x20, 400);
    f.graft(&rows).unwrap();
    f.assert_sound(&merged(&standing_rows, &rows));

    let root = f.root;
    let pages = f.pool.page_count();
    // The same interval, now occupied.
    let again = run(0x20, 400);
    let error = f.graft(&again).unwrap_err();
    assert!(
        matches!(error, kernel::Error::RangeNotEmpty),
        "expected RangeNotEmpty, got {error:?}"
    );
    assert_eq!(f.root, root, "a refused graft moved the root");
    assert_eq!(f.pool.page_count(), pages, "a refused graft allocated pages");
    f.assert_sound(&merged(&standing_rows, &rows));

    // Even one standing key inside the interval is enough to refuse it.
    let mut f2 = Fixture::new();
    f2.insert(&grafted(0x20, 17), b"in the way");
    let error = f2.graft(&run(0x20, 400)).unwrap_err();
    assert!(matches!(error, kernel::Error::RangeNotEmpty), "got {error:?}");
}

#[test]
fn writes_into_and_around_a_grafted_run_stay_correct() {
    let (mut f, standing_rows) = populated(1_500);
    let rows = run(0x20, 1_200);
    f.graft(&rows).unwrap();

    let mut expected: std::collections::BTreeMap<Vec<u8>, Vec<u8>> =
        merged(&standing_rows, &rows).into_iter().collect();

    // Deterministic pseudo-random traffic INSIDE the grafted interval, on its
    // two edges, and outside it.
    let mut state = 0x243f_6a88_85a3_08d3u64;
    for step in 0..3_000u64 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let pick = state >> 33;
        let key = match pick % 4 {
            0 => grafted(0x20, pick % 1_200),          // inside the run
            1 => grafted(0x20, 1_200 + pick % 400),    // just past its right edge
            2 => grafted(0x1f, pick % 400),            // just before its left edge
            _ => standing(pick % 1_500),               // the standing keyspace
        };
        if step % 3 == 2 {
            let removed = f.delete(&key);
            assert_eq!(removed, expected.remove(&key).is_some());
        } else {
            let value = format!("w{step}").into_bytes();
            f.insert(&key, &value);
            expected.insert(key, value);
        }
    }
    let expected: Vec<(Vec<u8>, Vec<u8>)> = expected.into_iter().collect();
    f.assert_sound(&expected);
    f.pool.flush_all(Barrier::None).unwrap();
}

#[test]
fn a_second_run_grafts_beside_the_first() {
    // Consecutive grafts are how a bounded-group index build lands: each
    // group's interval is empty because the previous group's keys are all
    // strictly to its left, and its boundary leaf is the previous group's
    // final packed leaf.
    let (mut f, standing_rows) = populated(1_000);
    let mut all = standing_rows;
    for group in 0..6u64 {
        let rows: Vec<(Vec<u8>, Vec<u8>)> = (0..500u64)
            .map(|i| {
                let n = group * 500 + i;
                (grafted(0x20, n), format!("g{n}").into_bytes())
            })
            .collect();
        f.graft(&rows).unwrap();
        all = merged(&all, &rows);
        f.assert_sound(&all);
    }
}

#[test]
fn deleting_around_a_grafted_run_is_not_refused() {
    // The delete path merges or redistributes two adjacent children of one
    // parent and required both to be the same KIND. A grafted run is a
    // subtree under one separator, so an underfull leaf beside it has an
    // interior sibling, and an ordinary delete came back
    // `Corrupt { why: "delete left sibling identity" }` with nothing corrupt.
    // Found by the 50K multimodel bench, not by the fixtures above: it needs a
    // node to fall UNDER the occupancy floor right next to the graft.
    let (mut f, standing_rows) = populated(2_000);
    let rows = run(0x20, 1_500);
    f.graft(&rows).unwrap();

    let mut expected: std::collections::BTreeMap<Vec<u8>, Vec<u8>> =
        merged(&standing_rows, &rows).into_iter().collect();

    // Empty the standing keyspace from the right, so leaf after leaf next to
    // the grafted subtree falls below the merge threshold.
    let mut keys: Vec<Vec<u8>> = standing_rows.iter().map(|(k, _)| k.clone()).collect();
    keys.reverse();
    for key in keys {
        f.delete(&key);
        expected.remove(&key);
    }
    // And from inside the run's own right edge, which empties the packed
    // leaves themselves.
    for i in (1_000..1_500u64).rev() {
        let key = grafted(0x20, i);
        f.delete(&key);
        expected.remove(&key);
    }
    let expected: Vec<(Vec<u8>, Vec<u8>)> = expected.into_iter().collect();
    f.assert_sound(&expected);
}

impl Fixture {
    /// Distance from the root to every leaf, deduplicated. A B-tree that still
    /// deserves the name has exactly one entry here.
    fn leaf_depths(&self) -> Vec<usize> {
        fn walk(pool: &BufferPool, page_no: u32, depth: usize, out: &mut Vec<usize>) {
            let read = pool.get(page_no).unwrap();
            let page = PageRef::open_resident(&read, page_no).unwrap();
            if page.kind() == PageKind::Leaf {
                out.push(depth);
                return;
            }
            walk(pool, page.child0(), depth + 1, out);
            for i in 0..page.nentries() {
                let DecodedRecord::Interior { child, .. } =
                    decode_record(page.slot(i), page_no, PageKind::Interior).unwrap()
                else {
                    panic!("interior holds a leaf record")
                };
                walk(pool, child, depth + 1, out);
            }
        }
        let mut depths = Vec::new();
        walk(&self.pool, self.root, 0, &mut depths);
        depths.sort_unstable();
        depths.dedup();
        depths
    }
}

/// A build lands in several committed groups, and consecutive groups are
/// strictly ascending. Group N+1 must therefore be attached BESIDE group N's
/// run, at the same depth -- not inside it. Splicing each group in at its
/// boundary leaf nested one wrapper level per group, so the tree stopped being
/// uniformly deep and every later descent paid for it (measured: the text and
/// vector builds that follow a grafted index got 27-37% slower).
#[test]
#[ignore = "DEFECT: a packed run enters the shared tree as a subtree whose height is set by the RUN, while the hole it fills sits at a depth set by the TREE. Consecutive groups additionally nest, each inside the previous run. Neither is fixable by attaching at a matching level in general, because the two heights need not coincide; the fix is a per-index tree (descriptor root field = format change). Measured cost of the non-uniformity: the text and vector builds that follow a grafted index ran 27-37% slower. The index build no longer uses the graft."]
fn consecutive_groups_graft_as_siblings_not_nested() {
    let (mut f, standing_rows) = populated(2_000);
    let mut all = standing_rows;
    let before = f.leaf_depths();
    assert_eq!(before.len(), 1, "fixture is not uniformly deep: {before:?}");

    for group in 0..8u64 {
        let rows: Vec<(Vec<u8>, Vec<u8>)> = (0..400u64)
            .map(|i| {
                let n = group * 400 + i;
                (grafted(0x20, n), format!("g{n}").into_bytes())
            })
            .collect();
        f.graft(&rows).unwrap();
        all = merged(&all, &rows);
        f.assert_sound(&all);
        let depths = f.leaf_depths();
        assert_eq!(
            depths.len(), 1,
            "after group {group} the tree is no longer uniformly deep: {depths:?}"
        );
    }
}

/// The same keys, one run or K runs, must leave the same shape -- that is what
/// "grafted as a sibling" means, and it is the property the nesting broke.
#[test]
#[ignore = "DEFECT: K grafted groups nest, so they leave a deeper tree than the same keys grafted as one run. See the note on the test above."]
fn k_groups_leave_the_same_depth_as_one_run() {
    let rows: Vec<(Vec<u8>, Vec<u8>)> = (0..3_200u64)
        .map(|n| (grafted(0x20, n), format!("g{n}").into_bytes()))
        .collect();

    let (mut one, standing_one) = populated(2_000);
    one.graft(&rows).unwrap();
    one.assert_sound(&merged(&standing_one, &rows));
    let single = one.leaf_depths();
    assert_eq!(single.len(), 1, "a one-run graft is not uniformly deep: {single:?}");

    let (mut many, standing_many) = populated(2_000);
    for chunk in rows.chunks(400) {
        many.graft(chunk).unwrap();
    }
    many.assert_sound(&merged(&standing_many, &rows));
    assert_eq!(many.leaf_depths(), single, "K groups left a different depth than one run");
}
