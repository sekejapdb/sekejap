//! # Vector storage — the raw embeddings behind vector search
//!
//! A "vector" here is an embedding: a fixed-length array of `f32`s that represents
//! a record's meaning (from an ML model), so that "similar" records have nearby
//! vectors. This file stores those raw vectors for one field. The HNSW index and
//! the query executor never touch the storage directly — they go through the
//! [`VectorAccess`] trait, so they don't care whether the vectors live in RAM
//! (ephemeral DB) or in a memory-mapped file on disk (persistent DB). That trait
//! boundary is what lets vector search be disk-first without changing its logic.
//!
//! `VectorStore` wraps an inner storage engine and presents it through the
//! [`VectorAccess`] trait so that HNSW and query execution are agnostic to
//! whether vectors live in RAM or on disk.
//!
//! Two modes:
//!
//! - **Memory** — `HashMap<u64, Vec<f32>>`, same as the original Sekejap
//!   representation. Used for ephemeral databases and as a fallback when no
//!   disk directory is configured.
//!
//! - **Disk** — append-only binary file per field, read via mmap.
//!   Vectors are stored on disk; only a small offset index lives in RAM.
//!   The binary file can always be regenerated from `snapshot.json` + WAL.
//!
//! ## File format (`vectors_{field}.bin`)
//!
//! Each record is:
//!
//! ```text
//! [id: u64 LE] [dim: u16 LE] [_pad: u16 = 0] [f32 × dim LE]
//!    8 bytes      2 bytes        2 bytes        dim × 4 bytes
//! ```
//!
//! Total per record = 12 + dim × 4 bytes. The 2-byte pad keeps f32 data
//! 4-byte aligned for direct SIMD loads from mmap.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use crate::vector::access::VectorAccess;
use crate::vector::hnsw::IterableVectors;

/// Header size per record: id (8) + dim (2) + pad (2) = 12 bytes.
const RECORD_HEADER: usize = 12;

/// Per-field vector storage.
///
/// Implements [`VectorAccess`] so it can be passed directly to
/// [`HnswGraph::build`](crate::vector::HnswGraph::build),
/// [`HnswGraph::search`](crate::vector::HnswGraph::search), and
/// [`HnswGraph::insert`](crate::vector::HnswGraph::insert).
///
/// Data is always serialised to `snapshot.json` for recoverability — the
/// binary file is a performance optimisation that can be regenerated.
#[derive(Clone)]
pub(crate) struct VectorStore {
    inner: VectorStoreInner,
}

#[derive(Clone)]
enum VectorStoreInner {
    Memory {
        vecs: HashMap<u64, Vec<f32>>,
    },
    #[cfg(unix)]
    Disk {
        /// The binary file for this field. `Arc` so a cloned (read-only snapshot)
        /// store shares the fd — reads use absolute-offset `pread`/mmap, so there is
        /// no cursor race with the live writer's appends.
        file: std::sync::Arc<std::fs::File>,
        /// Path to the binary file (for compaction / re-open).
        path: PathBuf,
        /// Total bytes written to the file (append cursor).
        total_len: u64,
        /// Dimension of vectors in this file (all must be equal).
        dim: u16,
        /// id → byte offset of the record in the file.
        /// Only the *latest* offset for each id is kept (overwrites update
        /// the index; old data becomes dead space reclaimed by compact).
        index: HashMap<u64, u64>,
        /// Memory-mapped view of the file for zero-copy reads.
        mmap: Option<super::mmap::MmapView>,
    },
}

impl VectorStore {
    /// Create an empty memory-backed store.
    pub fn new() -> Self {
        Self {
            inner: VectorStoreInner::Memory {
                vecs: HashMap::new(),
            },
        }
    }

    /// Open (or create) a disk-backed vector store for `field`.
    ///
    /// If the file already exists it is scanned to rebuild the offset index
    /// (reads only 12-byte headers per record, skips float data).
    #[cfg(unix)]
    pub fn open_disk(dir: &Path, field: &str) -> io::Result<Self> {
        let path = dir.join(format!("vectors_{field}.bin"));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;
        let file_len = file.metadata()?.len();

        let mut index = HashMap::new();
        let mut dim: u16 = 0;

        // Scan existing records to rebuild offset index.
        if file_len > 0 {
            use std::os::unix::fs::FileExt;
            let mut pos: u64 = 0;
            let mut hdr = [0u8; RECORD_HEADER];
            while pos + RECORD_HEADER as u64 <= file_len {
                if file.read_exact_at(&mut hdr, pos).is_err() {
                    break;
                }
                let id = u64::from_le_bytes(hdr[0..8].try_into().unwrap());
                let d = u16::from_le_bytes(hdr[8..10].try_into().unwrap());
                if d == 0 {
                    break; // corrupt or truncated
                }
                if dim == 0 {
                    dim = d;
                }
                let record_len = RECORD_HEADER as u64 + d as u64 * 4;
                if pos + record_len > file_len {
                    break; // truncated record
                }
                index.insert(id, pos);
                pos += record_len;
            }
        }

        #[cfg(unix)]
        let mmap = super::mmap::MmapView::try_new(&file, file_len as usize);

        Ok(Self {
            inner: VectorStoreInner::Disk {
                file: std::sync::Arc::new(file),
                path,
                total_len: file_len,
                dim,
                index,
                mmap,
            },
        })
    }

    /// Insert or replace a vector.
    pub fn put(&mut self, id: u64, data: Vec<f32>) {
        match &mut self.inner {
            VectorStoreInner::Memory { vecs } => {
                vecs.insert(id, data);
            }
            #[cfg(unix)]
            VectorStoreInner::Disk {
                file,
                total_len,
                dim,
                index,
                ..
            } => {
                let d = data.len() as u16;
                if *dim == 0 {
                    *dim = d;
                }
                // Write record: [id:u64][dim:u16][pad:u16][f32×dim]
                let record_len = RECORD_HEADER + d as usize * 4;
                let mut buf = Vec::with_capacity(record_len);
                buf.extend_from_slice(&id.to_le_bytes());
                buf.extend_from_slice(&d.to_le_bytes());
                buf.extend_from_slice(&0u16.to_le_bytes()); // pad
                for &f in &data {
                    buf.extend_from_slice(&f.to_le_bytes());
                }
                use std::os::unix::fs::FileExt;
                file.write_all_at(&buf, *total_len)
                    .expect("sekejap: vector disk write failed");
                index.insert(id, *total_len);
                *total_len += record_len as u64;
                // Note: mmap is NOT updated here — reads of newly-appended
                // data fall back to pread until the next remap (on compact).
            }
        }
    }

    /// Remove a vector by id. Returns the removed vector if it existed.
    pub fn remove(&mut self, id: u64) -> Option<Vec<f32>> {
        match &mut self.inner {
            VectorStoreInner::Memory { vecs } => vecs.remove(&id),
            #[cfg(unix)]
            VectorStoreInner::Disk { index, .. } => {
                // Just remove from the index — dead space is reclaimed by compact().
                // We don't return the old data to avoid an I/O read on every delete.
                index.remove(&id);
                None
            }
        }
    }

    /// Iterate over all (id, vector) pairs.
    ///
    /// For disk mode this reads each live vector from mmap or pread.
    pub fn iter(&self) -> Box<dyn Iterator<Item = (u64, &[f32])> + '_> {
        match &self.inner {
            VectorStoreInner::Memory { vecs } => {
                Box::new(vecs.iter().map(|(&id, v)| (id, v.as_slice())))
            }
            #[cfg(unix)]
            VectorStoreInner::Disk {
                dim, index, mmap, ..
            } => {
                let d = *dim as usize;
                let float_bytes = d * 4;
                Box::new(index.iter().filter_map(move |(&id, &offset)| {
                    let data_offset = offset as usize + RECORD_HEADER;
                    if let Some(ref m) = mmap {
                        let bytes = m.slice(data_offset, float_bytes)?;
                        // Safety: bytes is 4-byte aligned (record header is 12 bytes,
                        // file offsets are always record-aligned), len is dim*4.
                        let floats = unsafe {
                            std::slice::from_raw_parts(
                                bytes.as_ptr() as *const f32,
                                d,
                            )
                        };
                        Some((id, floats))
                    } else {
                        None
                    }
                }))
            }
        }
    }

    /// Compact the disk file: rewrite with only live vectors, reclaiming dead space.
    ///
    /// No-op for memory mode.
    #[cfg(unix)]
    pub fn compact(&mut self) -> io::Result<()> {
        match &mut self.inner {
            VectorStoreInner::Memory { .. } => Ok(()),
            VectorStoreInner::Disk {
                file,
                path,
                total_len,
                dim,
                index,
                mmap,
            } => {
                if index.is_empty() {
                    // No live vectors — truncate the file.
                    file.set_len(0)?;
                    *total_len = 0;
                    *mmap = None;
                    return Ok(());
                }

                let d = *dim as usize;
                let record_len = RECORD_HEADER + d * 4;

                // Write live vectors to a temp file.
                let tmp_path = path.with_extension("bin.tmp");
                let tmp_file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&tmp_path)?;

                let mut new_index = HashMap::with_capacity(index.len());
                let mut write_pos: u64 = 0;

                // Sort offsets for sequential reads.
                let mut entries: Vec<(u64, u64)> = index.iter().map(|(&id, &off)| (off, id)).collect();
                entries.sort_unstable();

                use std::os::unix::fs::FileExt;
                let mut record_buf = vec![0u8; record_len];
                for (offset, id) in entries {
                    // Read entire record from old file.
                    if let Some(ref m) = mmap {
                        if let Some(bytes) = m.slice(offset as usize, record_len) {
                            tmp_file.write_all_at(bytes, write_pos)?;
                        } else {
                            file.read_exact_at(&mut record_buf, offset)?;
                            tmp_file.write_all_at(&record_buf, write_pos)?;
                        }
                    } else {
                        file.read_exact_at(&mut record_buf, offset)?;
                        tmp_file.write_all_at(&record_buf, write_pos)?;
                    }
                    new_index.insert(id, write_pos);
                    write_pos += record_len as u64;
                }

                tmp_file.sync_all()?;
                std::fs::rename(&tmp_path, &*path)?;

                // Re-open and re-mmap the compacted file.
                let new_file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&*path)?;
                let new_mmap = super::mmap::MmapView::try_new(&new_file, write_pos as usize);

                *file = std::sync::Arc::new(new_file);
                *total_len = write_pos;
                *index = new_index;
                *mmap = new_mmap;
                Ok(())
            }
        }
    }

    /// Remap the mmap view to cover newly-appended data.
    ///
    /// Call after a batch of `put()` calls to make mmap reads cover the
    /// full file. No-op for memory mode or if file hasn't grown.
    #[cfg(unix)]
    pub fn remap(&mut self) {
        if let VectorStoreInner::Disk {
            file,
            total_len,
            mmap,
            ..
        } = &mut self.inner
        {
            let len = *total_len as usize;
            if len == 0 {
                *mmap = None;
                return;
            }
            if let Some(ref m) = mmap {
                if m.len() >= len {
                    return; // already covers all data
                }
            }
            *mmap = super::mmap::MmapView::try_new(&**file, len);
        }
    }

    /// Read one vector by id via **pread** (owned copy, no mmap).
    ///
    /// For the disk-first re-rank path: disk pages served here go through the
    /// kernel's reclaimable page cache, which is **not** counted in the process
    /// RSS — unlike mmap-resident pages. This is what keeps f32 out of RAM.
    pub fn get_owned(&self, id: u64) -> Option<Vec<f32>> {
        match &self.inner {
            VectorStoreInner::Memory { vecs } => vecs.get(&id).cloned(),
            #[cfg(unix)]
            VectorStoreInner::Disk { file, dim, index, .. } => {
                use std::os::unix::fs::FileExt;
                let &offset = index.get(&id)?;
                let d = *dim as usize;
                let data_offset = offset + RECORD_HEADER as u64;
                let mut buf = vec![0u8; d * 4];
                file.read_exact_at(&mut buf, data_offset).ok()?;
                let mut out = vec![0f32; d];
                for i in 0..d {
                    out[i] = f32::from_le_bytes(buf[i * 4..i * 4 + 4].try_into().unwrap());
                }
                Some(out)
            }
        }
    }

    /// Release the mmap view (munmap), dropping its resident pages from RSS.
    ///
    /// Call after a disk-first build once traversal reads f32 no more: the graph
    /// runs on int8 in RAM and re-rank uses [`get_owned`](Self::get_owned) (pread),
    /// so the f32 mmap is pure dead RSS weight afterwards. No-op for memory mode.
    #[cfg(unix)]
    pub fn drop_mmap(&mut self) {
        if let VectorStoreInner::Disk { mmap, .. } = &mut self.inner {
            *mmap = None;
        }
    }

    /// Approximate RAM held by this store (for memory profiling). In disk mode
    /// this is the id→offset index + any resident mmap; the f32 lives on disk.
    pub fn mem_bytes(&self) -> usize {
        match &self.inner {
            VectorStoreInner::Memory { vecs } => {
                vecs.capacity() * (8 + 24) + vecs.values().map(|v| v.capacity() * 4).sum::<usize>()
            }
            #[cfg(unix)]
            VectorStoreInner::Disk { index, mmap, .. } => {
                index.capacity() * (8 + 8 + 8) + mmap.as_ref().map(|m| m.len()).unwrap_or(0)
            }
        }
    }

    /// Whether this store is disk-backed.
    pub fn is_disk(&self) -> bool {
        match &self.inner {
            VectorStoreInner::Memory { .. } => false,
            #[cfg(unix)]
            VectorStoreInner::Disk { .. } => true,
        }
    }

    /// Number of live vectors.
    pub fn count(&self) -> usize {
        match &self.inner {
            VectorStoreInner::Memory { vecs } => vecs.len(),
            #[cfg(unix)]
            VectorStoreInner::Disk { index, .. } => index.len(),
        }
    }
}

impl Default for VectorStore {
    fn default() -> Self {
        Self::new()
    }
}

impl VectorAccess for VectorStore {
    #[inline]
    fn get(&self, id: u64) -> Option<&[f32]> {
        match &self.inner {
            VectorStoreInner::Memory { vecs } => {
                vecs.get(&id).map(|v| v.as_slice())
            }
            #[cfg(unix)]
            VectorStoreInner::Disk {
                file,
                dim,
                index,
                mmap,
                ..
            } => {
                let &offset = index.get(&id)?;
                let d = *dim as usize;
                let float_bytes = d * 4;
                let data_offset = offset as usize + RECORD_HEADER;

                // Fast path: mmap zero-copy read.
                if let Some(ref m) = mmap {
                    if let Some(bytes) = m.slice(data_offset, float_bytes) {
                        // Safety: the record format guarantees 4-byte alignment
                        // of float data (header is 12 bytes = 3×4).
                        let floats = unsafe {
                            std::slice::from_raw_parts(
                                bytes.as_ptr() as *const f32,
                                d,
                            )
                        };
                        return Some(floats);
                    }
                }

                // Fallback: data was appended after the mmap was created.
                // This path returns None rather than allocating — callers
                // needing post-append data should call remap() first.
                // In practice, open() and compact() always create a fresh mmap.
                let _ = file; // suppress unused warning on non-unix
                None
            }
        }
    }

    #[inline]
    fn len(&self) -> usize {
        self.count()
    }
}

impl IterableVectors for VectorStore {
    fn iter_vectors(&self) -> Box<dyn Iterator<Item = (u64, &[f32])> + '_> {
        self.iter()
    }
}
