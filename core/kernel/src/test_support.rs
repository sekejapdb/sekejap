use crate::budget::MemoryBudget;
use crate::io::{open_file, IoMode};
use crate::pool::BufferPool;
use std::sync::Arc;

/// A pool shaped like the one a real store opens.
///
/// It reserves page 0 first, because `Store::build` does and because a tree page
/// numbered 0 collides with the `next_leaf == 0` "no sibling" sentinel. Without
/// this reservation every B+tree test ran with its root at page 0 — a layout the
/// shipping engine never produces — and a test trying to point a leaf back at
/// that root was silently writing the end-of-chain marker instead of building a
/// cycle. Tests must run in the configuration that ships.
pub fn scratch_pool(frames: usize) -> (BufferPool, tempfile::TempDir) {
    let d = tempfile::tempdir().unwrap();
    let (f, _) = open_file(&d.path().join("t.db"), IoMode::Buffered).unwrap();
    let b = Arc::new(MemoryBudget::new(256 * 1024 * 1024));
    let pool = BufferPool::new(f.into(), b, frames).unwrap();
    let reserved = { let w = pool.allocate().unwrap(); w.page_no() };
    assert_eq!(reserved, 0, "the superblock must be page 0");
    (pool, d)
}
