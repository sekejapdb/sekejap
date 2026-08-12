//! # `VectorAccess` — one read interface, RAM or disk behind it
//!
//! A Rust *trait* is a shared interface: code written against the trait works
//! with any type that implements it. [`VectorAccess`] is that interface for
//! reading vectors, so the HNSW graph and query executor never learn whether the
//! vectors sit in a RAM `HashMap` (ephemeral DB) or in a memory-mapped file
//! (persistent DB) — swapping the backend doesn't touch the search code. "Zero-
//! copy" means a read hands back a borrowed slice into the existing bytes rather
//! than copying them out.
//!
//! Decouples HNSW and query execution from any specific storage backend.
//! Implemented by both in-memory `HashMap<u64, Vec<f32>>` (ephemeral DB)
//! and the disk-backed mmap vector store (persistent DB).

use std::collections::HashMap;

/// Zero-copy read access to vectors for a single field.
///
/// Every function in [`HnswGraph`](super::HnswGraph) that needs to read
/// vectors is generic over this trait, allowing the same graph code to
/// work with in-memory HashMaps, mmap-backed files, or any future backend.
pub trait VectorAccess {
    /// Return the vector for `id`, or `None` if absent.
    ///
    /// For mmap-backed stores, the returned slice points directly into the
    /// memory-mapped region (zero-copy).
    fn get(&self, id: u64) -> Option<&[f32]>;

    /// Number of vectors stored.
    fn len(&self) -> usize;

    /// Returns `true` if no vectors are stored.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl VectorAccess for HashMap<u64, Vec<f32>> {
    #[inline]
    fn get(&self, id: u64) -> Option<&[f32]> {
        HashMap::get(self, &id).map(|v| v.as_slice())
    }

    #[inline]
    fn len(&self) -> usize {
        HashMap::len(self)
    }
}

/// Zero-copy read access to **int8-quantized** codes for a single field.
///
/// The disk-first counterpart to [`VectorAccess`]: HNSW traversal reads compact
/// u8 codes (resident in RAM) through this trait and ranks with the integer L2
/// kernel, while full-precision f32 stays on disk for the final re-rank.
pub trait QuantAccess {
    /// Return the u8 code vector for `id`, or `None` if absent.
    fn code(&self, id: u64) -> Option<&[u8]>;

    /// Number of code vectors stored.
    fn len(&self) -> usize;

    /// Returns `true` if no codes are stored.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
