//! # Storage — the on-disk building blocks
//!
//! This module groups the low-level pieces that put bytes on disk (and read them
//! back), which the rest of the engine composes into a database. Each submodule
//! owns one file format or access primitive:
//!
//! - [`mmap`] — memory-map a file so it reads like a `&[u8]` (the disk-first base).
//! - [`wal`] — the write-ahead log: append every change for crash recovery.
//! - [`skbin`] — the compact binary record encoding written during compaction.
//! - [`topology`] — the node/edge tables + adjacency (the graph), mmap-served.
//! - [`edgestore`] — edges (forward/reverse adjacency + attributes).
//! - [`fieldstore`] — the scalar (btree) field index, as an mmap sidecar.
//! - [`ginstore`] — the on-disk trigram (GIN) index for `ILIKE`.
//! - [`spatialstore`] — the on-disk spatial grid index.
//! - [`vecstore`] — raw per-field vectors on disk.
//!
//! Everything here follows the same rule: bulk bytes live on disk; RAM holds
//! only offsets and hot structures.

pub(crate) mod edgestore;
pub(crate) mod fieldstore;
pub(crate) mod ginstore;
pub(crate) mod mmap;
pub(crate) mod pagestore;
pub(crate) mod skbin;
pub(crate) mod slotmap;
pub(crate) mod spatialstore;
pub(crate) mod topology;
pub(crate) mod vecstore;
pub(crate) mod wal;
