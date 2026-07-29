//! sekejap — lite graph database engine
//!
//! HashMap-backed, minimal deps.
//! Same chainable query API as sekejap full, without spatial/vector/fulltext.
//!
//! # In-memory (ephemeral)
//! ```
//! use sekejap::CoreDB;
//!
//! let mut db = CoreDB::new();
//! db.put("alice", r#"{"name":"Alice","age":30,"_collection":"users"}"#).unwrap();
//! db.put("bob",   r#"{"name":"Bob",  "age":25,"_collection":"users"}"#).unwrap();
//! db.link("alice", "bob", "follows"); // a naked, weightless edge
//!
//! let hits = db.one("alice").forward("follows").collect();
//! assert_eq!(hits[0].slug, "bob");
//! ```
//!
//! # Persistent (WAL-backed)
//! ```no_run
//! use sekejap::CoreDB;
//!
//! let mut db = CoreDB::open("mydb").unwrap();
//! db.put("alice", r#"{"name":"Alice","_collection":"users"}"#).unwrap();
//! db.compact().unwrap();  // flush snapshot + truncate WAL
//! ```

pub mod bm25;
#[cfg(feature = "engine")]
pub mod engine;
pub mod geo;
mod query;
pub mod scalar;
pub mod search;
pub mod sql;
mod storage;
pub mod text_index;
pub mod vector;

pub use vector::{CosineDistance, Distance, DotProduct, L2Distance};

pub use query::{CmpOp, DestWhere, Hit, MathExpr, MatchAggReturn, MatchAggStart, MatchAggStmt, Set, Step, WhereValue, WithExpr, WithOutExpr, WithRow, WithStage};
pub use sql::{CompiledMutation, EdgeDelete, EdgeInsert, FieldDef, FieldType, PreparedQuery, SqlError, TableSchema};
pub use storage::edgestore::EdgeMode;
pub use storage::wal::WalFormat;

#[doc(hidden)]
pub mod wal_bench {
    pub use crate::storage::wal::{WalEntry, binary_encode, binary_decode};
}

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use storage::wal::{WalEntry, WalReader, WalWriter};
use text_index::gin::GINIndex;
use text_index::gist::GiSTIndex;

// ── Storage format version constants ─────────────────────────────────────────

/// Bump when the snapshot schema changes in a backwards-incompatible way.
/// Old binaries that encounter a higher version return an error on open().
///
/// v1 = legacy headerless JSON (pre-2026-07 builds).
/// v2 = 16-byte `[magic][version][flags]` header (see below) followed by the
///      JSON body. The header lets a future reader detect the encoding *before*
///      committing to a parser, so a later binary/compressed snapshot (v3+) is
///      an additive dispatch arm rather than a breaking change. Mirrors the WAL.
/// v3 = manifest snapshots: on disk-backed DBs the snapshot no longer carries
/// nodes/edges (they live in the topology files, the single source of truth);
/// `topology_in_files = true` marks it. v2 (full JSON topology) is still read.
const SNAPSHOT_FORMAT_VERSION: u32 = 3;

/// Friendly, actionable message when an on-disk format is NEWER than this build
/// understands. Distinguishes "version skew" from "corruption" and points at both
/// the upgrade path and the `sekejap migrate` toolkit. Used for every format /
/// version mismatch surfaced on open so users always get a clear next step.
fn newer_format_msg(kind: &str, found: u32, max: u32) -> String {
    format!(
        "this database was written by a newer sekejap ({kind} format v{found}; \
this build supports up to v{max}). Your data is intact — this is a version check, \
not corruption.\n  → Upgrade sekejap to the version that wrote it, then open normally.\n  \
→ Or open it with that newer sekejap and run `sekejap migrate <db>` to rewrite it \
in a format this build can read."
    )
}

/// Magic prefix identifying a versioned (v2+) snapshot file. Legacy v1 files
/// start with `{` (JSON) and are auto-detected as headerless.
const SNAPSHOT_MAGIC: [u8; 8] = *b"SKSNAP\0\0";
/// `[magic 8][version u32 LE][flags u32 LE]`.
const SNAPSHOT_HEADER_LEN: usize = 16;

/// Build the 16-byte snapshot header for the given format version.
fn snapshot_header_bytes(version: u32) -> [u8; SNAPSHOT_HEADER_LEN] {
    let mut h = [0u8; SNAPSHOT_HEADER_LEN];
    h[0..8].copy_from_slice(&SNAPSHOT_MAGIC);
    h[8..12].copy_from_slice(&version.to_le_bytes());
    // h[12..16] = flags, reserved (0).
    h
}

/// Inspect a snapshot's leading bytes. Returns `(format_version, body_offset)`.
/// A headerless legacy file reports `(1, 0)`.
fn snapshot_probe(head: &[u8]) -> (u32, usize) {
    if head.len() >= SNAPSHOT_HEADER_LEN && head[0..8] == SNAPSHOT_MAGIC {
        let version = u32::from_le_bytes(head[8..12].try_into().unwrap());
        (version, SNAPSHOT_HEADER_LEN)
    } else {
        (1, 0)
    }
}

/// Bump each constant when the corresponding index algorithm changes in a way
/// that makes indexes built by the previous version produce wrong results.
const GIN_INDEX_VERSION:     u32 = 2; // slot-map fix 2026-04-13
const BM25_INDEX_VERSION:    u32 = 1;
const BTREE_INDEX_VERSION:   u32 = 1;
const HNSW_INDEX_VERSION:    u32 = 1;

// ── Field index key ───────────────────────────────────────────────────────────

/// Totally-ordered wrapper for f64 (NaN sorts last, uses `total_cmp`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct OrdF64(f64);
impl Eq for OrdF64 {}
impl PartialOrd for OrdF64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrdF64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// Ordered key for a field index: null < bool < number < string.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) enum FieldKey {
    Null,
    Bool(bool),
    Number(OrdF64),
    Str(String),
}

impl FieldKey {
    pub(crate) fn from_json(v: &Value) -> Option<Self> {
        match v {
            Value::Null => Some(FieldKey::Null),
            Value::Bool(b) => Some(FieldKey::Bool(*b)),
            Value::Number(n) => n.as_f64().map(|f| FieldKey::Number(OrdF64(f))),
            Value::String(s) => Some(FieldKey::Str(s.clone())),
            _ => None,
        }
    }
    pub(crate) fn from_f64(f: f64) -> Self {
        FieldKey::Number(OrdF64(f))
    }
}

// ── Internal types ────────────────────────────────────────────────────────────

/// Hash a string with SeaHash (fast, non-cryptographic, deterministic).
pub(crate) fn sk_hash(s: &str) -> u64 {
    seahash::hash(s.as_bytes())
}

/// Payload storage backend — either an in-memory `Vec<u8>` (ephemeral DB) or
/// a memory-mapped append file `payloads.bin` (persistent DB).
///
/// For persistent databases the file is truncated to zero on every `open()`,
/// then refilled by snapshot + WAL replay. This keeps all geometry / large-JSON
/// bytes on disk and out of RAM. Only `NodeData` metadata (≈ 100 B per node)
/// stays in the `HashMap`.
pub(crate) struct PayloadStore {
    /// When true, records are re-encoded as SKBIN (binary) at compaction.
    /// Incremental writes always stay raw JSON (hot/greppable); compaction is
    /// where we own the field table and can encode the cold bulk.
    binary: bool,
    /// Shared field-name table for SKBIN. Append-only IDs: a record encoded at
    /// any time decodes against this table now or after later appends, so a
    /// superset table written before a payload swap never mis-decodes old
    /// records. Empty when no SKBIN records exist.
    field_table: storage::skbin::FieldTable,
    inner: PayloadInner,
}

// ── Read-only mmap (shared between PayloadStore and VectorStore) ─────────────
#[cfg(unix)]
use storage::mmap::MmapView;

enum PayloadInner {
    Memory { data: Vec<u8> },
    Disk {
        file: std::fs::File,
        total_len: u64,
        #[cfg(unix)]
        mmap: Option<MmapView>,
    },
    #[cfg(feature = "s3")]
    Remote {
        cache: std::sync::Mutex<engine::cache::BlockCache>,
    },
}

/// The SKBIN field table — the only shared decode state — is written to these
/// redundant, self-checksummed copies. On load the first copy that passes its
/// CRC wins; on a bad primary we recover from a backup. This protects the
/// "metadata is recoverable" guarantee the whole format rests on.
const FIELD_TABLE_COPIES: [&str; 3] =
    ["field_table.bin", "field_table.bin.1", "field_table.bin.2"];

/// Records larger than this are never compressed — the large-payload head/tail
/// extraction fast paths depend on raw bytes at arbitrary offsets, and they are
/// only taken for records above `FAST_PATH_THRESHOLD` (64 KB). Keeping the two
/// thresholds equal preserves that invariant.
const PAYLOAD_COMPRESS_MAX: usize = 64 * 1024;
/// First-byte tag of a RETIRED whole-record zstd payload. zstd was removed from
/// the payload path; this tag is now recognized only to reject such a record
/// loudly (it cannot be decoded) rather than byte-searching its compressed bytes.
/// Raw JSON starts with `{` (0x7B), SKBIN with `0x02`.
const PAYLOAD_TAG_ZSTD: u8 = 0x01;

/// Decode a stored record. Raw JSON starts with `{`; SKBIN with `0x02`; both
/// pass through here unchanged (the SKBIN body is decoded downstream). A `0x01`
/// tag is a RETIRED whole-record zstd payload — zstd was removed from the
/// payload path, so such a record can only come from a DB that opted into the
/// now-deleted `payload_compression` feature. Return `None` (a loud decode
/// failure) rather than silently mis-serving it.
fn decode_payload_record(stored: Vec<u8>) -> Option<Vec<u8>> {
    match stored.first() {
        Some(&PAYLOAD_TAG_ZSTD) => None,
        _ => Some(stored),
    }
}

impl PayloadStore {
    fn new() -> Self {
        Self { binary: false, field_table: storage::skbin::FieldTable::new(), inner: PayloadInner::Memory { data: Vec::new() } }
    }

    /// Open (or create) a disk-backed store, truncating to zero.
    fn open_file(path: &std::path::Path) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(Self { binary: false, field_table: storage::skbin::FieldTable::new(), inner: PayloadInner::Disk {
            file,
            total_len: 0,
            #[cfg(unix)]
            mmap: None,
        } })
    }

    /// Open an existing disk-backed store without truncating.
    fn open_existing(path: &std::path::Path, total_len: u64) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        #[cfg(unix)]
        let mmap = MmapView::try_new(&file, total_len as usize);
        Ok(Self { binary: false, field_table: storage::skbin::FieldTable::new(), inner: PayloadInner::Disk {
            file,
            total_len,
            #[cfg(unix)]
            mmap,
        } })
    }

    /// Create a remote-backed store that fetches blocks from S3 on demand.
    #[cfg(feature = "s3")]
    fn new_remote(
        store: std::sync::Arc<dyn object_store::ObjectStore>,
        prefix: &str,
        total_remote_len: u64,
        budget: engine::cache::CacheBudget,
    ) -> Result<Self, String> {
        let cache = engine::cache::BlockCache::new(
            store,
            prefix,
            "payloads.bin",
            total_remote_len,
            budget,
        )?;
        Ok(Self {
            // remote store is read-only; records self-describe
            binary: false,
            field_table: storage::skbin::FieldTable::new(),
            inner: PayloadInner::Remote {
                cache: std::sync::Mutex::new(cache),
            },
        })
    }

    fn is_disk(&self) -> bool {
        matches!(self.inner, PayloadInner::Disk { .. })
    }

    /// Append raw bytes; returns `(offset, len)`.
    /// Panics on disk write failure (disk-full etc.) — callers do not recover.
    fn append(&mut self, bytes: &[u8]) -> (u64, u32) {
        match &mut self.inner {
            PayloadInner::Memory { data } => {
                let offset = data.len() as u64;
                data.extend_from_slice(bytes);
                (offset, bytes.len() as u32)
            }
            PayloadInner::Disk { file, total_len, .. } => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileExt;
                    file.write_all_at(bytes, *total_len)
                        .expect("sekejap: payload disk write failed");
                }
                #[cfg(not(unix))]
                {
                    use std::io::{Seek, SeekFrom, Write};
                    file.seek(SeekFrom::Start(*total_len))
                        .expect("sekejap: payload disk seek failed");
                    file.write_all(bytes)
                        .expect("sekejap: payload disk write failed");
                }
                let offset = *total_len;
                *total_len += bytes.len() as u64;
                (offset, bytes.len() as u32)
            }
            #[cfg(feature = "s3")]
            PayloadInner::Remote { .. } => {
                panic!("sekejap: cannot write to remote payload store (read-only)");
            }
        }
    }

    fn append_batch(&mut self, items: &[&[u8]]) -> Vec<(u64, u32)> {
        if items.is_empty() { return vec![]; }
        match &mut self.inner {
            PayloadInner::Memory { data } => {
                items.iter().map(|bytes| {
                    let offset = data.len() as u64;
                    data.extend_from_slice(bytes);
                    (offset, bytes.len() as u32)
                }).collect()
            }
            PayloadInner::Disk { file, total_len, .. } => {
                let total_bytes: usize = items.iter().map(|b| b.len()).sum();
                let mut buf = Vec::with_capacity(total_bytes);
                let mut results = Vec::with_capacity(items.len());
                let base = *total_len;
                for bytes in items {
                    results.push((base + buf.len() as u64, bytes.len() as u32));
                    buf.extend_from_slice(bytes);
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileExt;
                    file.write_all_at(&buf, base)
                        .expect("sekejap: payload batch write failed");
                }
                #[cfg(not(unix))]
                {
                    use std::io::{Seek, SeekFrom, Write};
                    file.seek(SeekFrom::Start(base))
                        .expect("sekejap: payload batch seek failed");
                    file.write_all(&buf)
                        .expect("sekejap: payload batch write failed");
                }
                *total_len = base + buf.len() as u64;
                results
            }
            #[cfg(feature = "s3")]
            PayloadInner::Remote { .. } => {
                panic!("sekejap: cannot write to remote payload store (read-only)");
            }
        }
    }

    /// Decode the payload at the given position to a `Value` — the hot path for
    /// the query engine. SKBIN records decode DIRECTLY to `Value` (binary decode,
    /// no JSON round-trip — faster than parsing text); raw JSON records parse; a
    /// retired `0x01` zstd record yields `None`.
    fn get(&self, offset: u64, len: u32) -> Option<Value> {
        let stored = self.get_raw_at(offset, len as usize)?;
        if storage::skbin::is_skbin(&stored) {
            return storage::skbin::decode(&stored, &self.field_table);
        }
        decode_payload_record(stored).and_then(|b| serde_json::from_slice(&b).ok())
    }

    /// Return raw JSON bytes at the given position (owned copy), transparently
    /// decoding SKBIN. Dispatch is by the record's first byte (`0x02` SKBIN,
    /// `{` raw JSON, `0x01` = retired zstd → `None`) so mixed files need no
    /// migration.
    pub(crate) fn get_raw(&self, offset: u64, len: u32) -> Option<Vec<u8>> {
        let stored = self.get_raw_at(offset, len as usize)?;
        if storage::skbin::is_skbin(&stored) {
            // SKBIN → reconstruct JSON bytes using the shared field-name table.
            let v = storage::skbin::decode(&stored, &self.field_table)?;
            return serde_json::to_vec(&v).ok();
        }
        decode_payload_record(stored)
    }

    /// Read `read_len` bytes starting at an arbitrary absolute byte offset.
    /// Uses mmap when available (zero syscalls), falls back to pread.
    /// Remote variant fetches via block cache from S3.
    pub(crate) fn get_raw_at(&self, abs_offset: u64, read_len: usize) -> Option<Vec<u8>> {
        if read_len == 0 {
            return Some(vec![]);
        }
        match &self.inner {
            PayloadInner::Memory { data } => {
                let start = abs_offset as usize;
                let end = start.checked_add(read_len)?;
                data.get(start..end).map(|b| b.to_vec())
            }
            #[cfg(unix)]
            PayloadInner::Disk { file, mmap, .. } => {
                // Fast path: read from mmap (no syscall — just memcpy from page cache).
                if let Some(ref m) = mmap {
                    if let Some(slice) = m.slice(abs_offset as usize, read_len) {
                        return Some(slice.to_vec());
                    }
                }
                // Fallback: pread for data written after the mmap was created.
                use std::os::unix::fs::FileExt;
                let mut buf = vec![0u8; read_len];
                file.read_exact_at(&mut buf, abs_offset).ok()?;
                Some(buf)
            }
            #[cfg(not(unix))]
            PayloadInner::Disk { file, .. } => {
                let _ = (file, abs_offset, read_len);
                None
            }
            #[cfg(feature = "s3")]
            PayloadInner::Remote { cache } => {
                cache.lock().ok()?.get_raw_at(abs_offset, read_len)
            }
        }
    }

    /// Borrow a slice of the payload store without copying (zero-alloc).
    /// Returns `None` if offset/len is out of range or no mmap is available.
    #[cfg(unix)]
    fn get_slice(&self, abs_offset: u64, read_len: usize) -> Option<&[u8]> {
        if read_len == 0 { return Some(&[]); }
        match &self.inner {
            PayloadInner::Memory { data } => {
                let start = abs_offset as usize;
                let end = start.checked_add(read_len)?;
                data.get(start..end)
            }
            PayloadInner::Disk { mmap, .. } => {
                mmap.as_ref()?.slice(abs_offset as usize, read_len)
            }
            #[cfg(feature = "s3")]
            PayloadInner::Remote { .. } => None,
        }
    }

    /// Reset the slab (in-memory only — used after in-memory compaction).
    fn reset(&mut self, new_data: Vec<u8>) {
        if let PayloadInner::Memory { data } = &mut self.inner {
            *data = new_data;
        }
    }
}

#[derive(Clone)]
pub struct NodeData {
    pub slug: String,
    /// Cached `_collection` field value (empty string if no collection).
    /// Avoids parsing JSON for collection-only lookups.
    pub collection: String,
    /// Cached spatial bounding-box, computed once in `put_raw()`.
    /// `rebuild_spatial_grid()` reads from here to avoid disk reads.
    pub spatial_meta: Option<geo::SpatialMeta>,
    /// Byte offset of this node's raw JSON payload in `CoreDB.payload_store`.
    pub payload_offset: u64,
    /// Byte length of this node's raw JSON payload.
    pub payload_len: u32,
}

// EdgeEntry removed — replaced by storage::edgestore::Edge.
pub(crate) use storage::edgestore::Edge;

// ── EdgeHit ───────────────────────────────────────────────────────────────────

/// A resolved edge returned from `db.edges_from()` / `db.edges_to()`.
#[derive(Debug, Clone)]
pub struct EdgeHit {
    pub from_slug: Option<String>,
    pub to_slug: Option<String>,
    /// Human-readable edge type label (e.g. `"taught_by"`), if recorded.
    pub edge_type: Option<String>,
    pub edge_type_hash: u64,
    /// All edge attributes (fast-lane columns + JSON bag), merged. `None` if the
    /// edge is naked. A weight, if any, is a user-named attribute in here.
    pub meta: Option<Value>,
}

// ── BfsPath (internal only) ───────────────────────────────────────────────────

/// Internal result of `bfs_shortest_path`. Not part of the public API.
/// Use `db.query("SELECT … FROM MATCH SHORTEST …")` instead.
#[derive(Debug, Clone)]
pub(crate) struct BfsPath {
    pub(crate) nodes: Vec<query::Hit>,
    pub(crate) edges: Vec<EdgeHit>,
    pub(crate) length: usize,
}

// ── CoreDB ────────────────────────────────────────────────────────────────────

/// The database. Not thread-safe by itself — wrap in `Mutex<CoreDB>` if needed.
///
/// Writes take `&mut self`. Reads and query starters take `&self`.
///
/// Use [`CoreDB::new`] for an in-memory DB, or [`CoreDB::open`] for a
/// WAL-backed persistent DB.
pub struct CoreDB {
    nodes: HashMap<u64, NodeData>,
    slug_map: HashMap<String, u64>,
    /// Graph edges (forward + reverse adjacency, edge type names, metadata).
    edges: storage::edgestore::EdgeStore,
    /// Auto-compaction mode + thresholds (copied from `Config` at open).
    auto_compact: AutoCompact,
    compact_thresholds: CompactThresholds,
    compact_on_close: bool,
    /// Amortises the WAL-size stat: thresholds are checked every N writes.
    writes_since_compact_check: u32,
    /// Reentrancy guard for the on-write hook.
    autocompacting: bool,
    /// Paged-topology base (mmap'd files written at compact). `None` = resident
    /// mode (default). When `Some`, the resident maps above act as the **write
    /// overlay** since open, and the topology accessors merge overlay-over-base.
    topo_base: Option<storage::topology::MappedTopology>,
    /// collection_hash → member slug hashes
    collections: HashMap<u64, Vec<u64>>,
    /// collection_hash → collection name (for O(1) SHOW TABLES without node scan)
    collection_names_map: HashMap<u64, String>,
    /// WAL writer — `Some` when opened from disk, `None` for in-memory.
    wal: Option<WalWriter>,
    /// WAL encoding format used by this database instance.
    wal_format: WalFormat,
    /// Data directory path.
    pub(crate) data_dir: Option<PathBuf>,
    /// Grid-based spatial index for accelerating spatial queries.
    spatial_grid: Option<geo::SpatialGrid>,
    /// GiST trigram indexes for text fields (field_name -> index).
    /// Built automatically for all text fields — cheap enough to always have.
    text_indexes: HashMap<String, GiSTIndex>,
    /// GIN trigram indexes for text fields (field_name -> index).
    /// Built explicitly via build_gin_index() for exact matching (no verification).
    gin_indexes: HashMap<String, GINIndex>,
    /// BM25 full-text indexes for ranked search (field_name -> index).
    /// Built explicitly via build_bm25_index() for relevance-ranked results.
    bm25_indexes: HashMap<String, bm25::Bm25Index>,
    /// Positional search indexes: index_key → SearchIndex.
    /// Key is fields joined with "+", e.g. "title+body".
    pub(crate) search_indexes: HashMap<String, search::SearchIndex>,
    /// Table schemas (collection name -> schema).
    /// Persisted in WAL/snapshot.
    schemas: HashMap<String, sql::TableSchema>,
    /// Vector store: field_name → per-field store.
    ///
    /// Memory mode wraps `HashMap<u64, Vec<f32>>`; disk mode (Phase 4) will
    /// use an append-only binary file read via mmap.  Vectors are always
    /// serialised to the JSON snapshot for recoverability — the binary file
    /// is a performance optimisation that can be regenerated from the snapshot.
    vectors: HashMap<String, storage::vecstore::VectorStore>,
    /// HNSW approximate-NN indexes: field_name → graph.
    /// Built explicitly via [`CoreDB::build_hnsw_index`].
    /// Secondary index — never affects the main store on error.
    hnsw_indexes: HashMap<String, vector::HnswGraph>,
    /// Btree field indexes: (collection_hash, field_name) → ordered value → [node hashes].
    /// Built via `CREATE INDEX ON collection(field) USING btree`.
    /// Maintained incrementally on every put()/remove().
    field_indexes: HashMap<(u64, String), BTreeMap<FieldKey, Vec<u64>>>,
    /// Build params for each HNSW index: field → (m, ef_construction).
    /// Populated by build_hnsw_index(); used to auto-rebuild on version mismatch.
    hnsw_params: HashMap<String, (usize, usize)>,
    /// Append-only byte slab for raw JSON payloads.
    /// All `NodeData` entries index into this store via `(payload_offset, payload_len)`.
    payload_store: PayloadStore,
    /// Set to `true` during WAL replay in `open()`.
    /// Guards expensive per-entry rebuilds (e.g. HNSW entry-point check in remove_raw)
    /// that must not fire O(N) times during replay — open() handles those once at the end.
    replaying: bool,
    /// SQL transaction buffer. `Some` when a `BEGIN` has been issued;
    /// mutations are queued here until `COMMIT` (replay) or `ROLLBACK` (drop).
    pending_txn: Option<Vec<sql::CompiledMutation>>,
    /// When true, `wal_write` appends without fsync.
    /// Used by batch operations (UPDATE, DELETE, COMMIT) to coalesce syncs.
    defer_wal_sync: bool,
    /// When `Some`, `put_raw_inner` uses this timestamp instead of calling
    /// `chrono::Utc::now()` per row — set once per batch to skip N time syscalls.
    batch_now: Option<i64>,
    /// When true, expensive index rebuilds (BM25, GIN) are deferred until
    /// `flush_deferred_indexes()`. Avoids O(N²) cost of per-row BM25 rebuild
    /// during batch inserts.
    defer_index_rebuild: bool,
    /// BM25 fields needing rebuild after a deferred batch.
    dirty_bm25: HashSet<String>,
    /// GIN fields needing rebuild after a deferred batch.
    dirty_gin: HashSet<String>,
    /// Collections whose `search` index needs rebuild after a deferred batch.
    /// Search uses an immutable FST, so it can't incrementally add terms —
    /// it rebuilds the collection's index (like BM25 rebuilds a field).
    dirty_search: HashSet<String>,
    /// When true, UPDATE statements log one logical `WalEntry::Update`
    /// (compiled statement + timestamp) instead of one physical `Put` per
    /// affected row. Toggle via `SET WAL_MODE = logical|physical`.
    /// Trade-off: far less WAL volume, but the log records intent rather
    /// than final row values.
    logical_wal: bool,
    /// fsync strength for WAL syncs. Kept here so it survives WAL writer
    /// recreation (compact). Toggle via `SET WAL_SYNC = full|barrier|os`.
    wal_sync_level: storage::wal::SyncLevel,
    /// Exclusive file lock held for the lifetime of the database.
    /// Prevents concurrent access from multiple processes.
    _lock_file: Option<std::fs::File>,
}

/// Configuration for [`CoreDB::open_with_config`].
/// Auto-compaction execution mode. See [`Config::auto_compact`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoCompact {
    /// Never compact automatically (app calls `compact()` itself).
    Off,
    /// Track thresholds only; the app calls [`CoreDB::maybe_compact`] at idle.
    Manual,
    /// SQLite-style: the write that crosses a threshold compacts inline.
    OnWrite,
}

/// Thresholds that make compaction eligible.
#[derive(Clone, Copy, Debug)]
pub struct CompactThresholds {
    /// Compact when `wal.log` exceeds this many bytes (bounds reopen + disk).
    pub wal_bytes: u64,
    /// Paged mode: compact when the RAM write-overlay holds this many nodes.
    pub overlay_entries: usize,
}

impl Default for CompactThresholds {
    fn default() -> Self {
        Self { wal_bytes: 64 << 20, overlay_entries: 200_000 }
    }
}

#[derive(Clone)]
pub struct Config {
    /// How edges are stored.  [`EdgeMode::Fat`] keeps metadata in RAM
    /// (original behaviour); [`EdgeMode::Compact`] puts metadata on disk
    /// and uses ~2.7× less RAM per edge.
    pub edge_mode: EdgeMode,
    /// When `true`, skip the exclusive file lock and WAL writer.
    /// The database will not accept writes — use for read replicas.
    pub read_only: bool,
    /// WAL encoding format. New WAL files are created in this format.
    /// Existing WAL files keep their detected format (auto-detected from
    /// the file header). To switch an existing database, compact first.
    pub wal_format: WalFormat,
    /// When compaction runs automatically. `OnWrite` (default) mirrors SQLite's
    /// auto-checkpoint: a write that crosses a threshold runs `compact()` inline
    /// on that call (occasional seconds-scale stall, zero-ops). `Manual` only
    /// tracks thresholds — the app calls [`CoreDB::maybe_compact`] at idle
    /// moments (request-loop gaps, robot sleep). `Off` disables both.
    pub auto_compact: AutoCompact,
    /// Thresholds that make compaction *eligible* (used by both `OnWrite` and
    /// `maybe_compact`): WAL size bounds reopen-replay time and disk; overlay
    /// entries bounds RAM growth in paged mode.
    pub compact_thresholds: CompactThresholds,
    /// Run a final `compact()` when the database is dropped (only if the WAL is
    /// non-trivial). Off by default — drops should not stall unexpectedly.
    pub compact_on_close: bool,
    /// Re-encode payloads as SKBIN (schema-aware binary) at compaction: field
    /// names → IDs, typed values, strings literal. ~1.6× smaller, faster field
    /// reads, 1-record corruption isolation (values never leave their record).
    /// Incremental writes stay raw JSON until the next `compact()`. Default on.
    pub payload_binary: bool,
    /// **Experimental.** Serve topology (nodes + edges) from the mmap'd files
    /// written at `compact()` instead of loading it into RAM. The OS page cache
    /// keeps the hot working set resident and pages the rest — topology size is
    /// no longer bounded by RAM. Writes since open live in a RAM overlay merged
    /// with the mapped base on every read; `compact()` folds them together.
    ///
    /// Current limitations (documented, to be lifted): spatial metadata and edge
    /// metadata are not served from the base (spatial/meta-dependent queries see
    /// only overlay data); `remove`/`unlink` of base data does not take effect
    /// until tombstones land. (Compatible with `payload_binary` (SKBIN): base
    /// payload reads resolve offsets via the mmap base and decode SKBIN.)
    pub paged_topology: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            edge_mode: EdgeMode::Compact,
            read_only: false,
            wal_format: WalFormat::Binary,
            auto_compact: AutoCompact::OnWrite,
            compact_thresholds: CompactThresholds::default(),
            compact_on_close: false,
            // SKBIN Level-1 is the official default payload format: schema-aware
            // binary (~1.2–2x smaller on structured data, faster field reads,
            // 1-record corruption isolation, zero user data in shared state).
            // Fuzzed decoder, integrated across DML/DDL + resident/paged. Set
            // false for legacy raw-JSON payloads.
            payload_binary: true,
            paged_topology: false,
        }
    }
}

impl Default for CoreDB {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CoreDB {
    fn drop(&mut self) {
        // Optional final checkpoint for an instant next open. Only when opted in,
        // only when the WAL is non-trivial, and never during a panic unwind.
        if self.compact_on_close
            && self.data_dir.is_some()
            && !std::thread::panicking()
        {
            let wal_len = self
                .data_dir
                .as_ref()
                .and_then(|d| std::fs::metadata(d.join("wal.log")).ok())
                .map(|m| m.len())
                .unwrap_or(0);
            if wal_len > 4096 {
                let _ = self.compact();
            }
        }
    }
}

impl CoreDB {
    // ── Constructors ──────────────────────────────────────────────────────────

    /// Create a new in-memory database (no persistence).
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            slug_map: HashMap::new(),
            auto_compact: AutoCompact::Off, // memory DBs have nothing to compact
            compact_thresholds: CompactThresholds::default(),
            compact_on_close: false,
            writes_since_compact_check: 0,
            autocompacting: false,
            topo_base: None,
            edges: storage::edgestore::EdgeStore::new_fat(),
            collections: HashMap::new(),
            collection_names_map: HashMap::new(),
            wal: None,
            wal_format: WalFormat::Json,
            data_dir: None,
            spatial_grid: None,
            text_indexes: HashMap::new(),
            gin_indexes: HashMap::new(),
            bm25_indexes: HashMap::new(),
            search_indexes: HashMap::new(),
            schemas: HashMap::new(),
            vectors: HashMap::new(),
            hnsw_indexes: HashMap::new(),
            field_indexes: HashMap::new(),
            hnsw_params: HashMap::new(),
            payload_store: PayloadStore::new(),
            replaying: false,
            pending_txn: None,
            defer_wal_sync: false,
            batch_now: None,
            defer_index_rebuild: false,
            dirty_bm25: HashSet::new(),
            dirty_gin: HashSet::new(),
            dirty_search: HashSet::new(),
            logical_wal: false,
            wal_sync_level: storage::wal::SyncLevel::Full,
            _lock_file: None,
        }
    }

    /// Open (or create) a persistent database in `dir`.
    ///
    /// Uses [`EdgeMode::Compact`] by default (disk-first edge metadata).
    /// For the original all-in-RAM behaviour, use
    /// [`open_with_config`](Self::open_with_config) with [`EdgeMode::Fat`].
    pub fn open(dir: impl AsRef<Path>) -> io::Result<Self> {
        Self::open_with_config(dir, Config::default())
    }

    /// **Experimental.** Open with paged topology: nodes + edges are served from
    /// the mmap'd files written at the last `compact()` (OS page cache holds the
    /// hot working set), while writes since open live in a RAM overlay. Falls
    /// back to a normal resident open when the topology files are absent.
    /// See [`Config::paged_topology`] for current limitations.
    pub fn open_paged(dir: impl AsRef<Path>) -> io::Result<Self> {
        Self::open_with_config(dir, Config { paged_topology: true, ..Config::default() })
    }

    /// Open a database in read-only mode (no lock, no WAL writer).
    ///
    /// Suitable for read replicas that sync their local directory from S3.
    /// Write operations will silently skip WAL persistence.
    pub fn open_read_only(dir: impl AsRef<Path>) -> io::Result<Self> {
        Self::open_with_config(
            dir,
            Config { read_only: true, ..Config::default() },
        )
    }

    /// Open a read-only database backed by S3 remote storage.
    ///
    /// Downloads only the snapshot (node index, ~100 B/node) and loads it
    /// into RAM. Payloads stay on S3 — each `get_payload()` call fetches
    /// the relevant 64 KB block via `GET_RANGE` and caches it in a bounded
    /// LRU. No local `payloads.bin` file is needed.
    ///
    /// This allows querying a 1 TB dataset from a machine with 50 GB of disk:
    /// the node index stays in RAM (~hundreds of MB), the block cache keeps
    /// hot payload blocks on local storage, and cold blocks are fetched on
    /// demand from S3.
    /// Open a read-only database backed by S3.
    ///
    /// Payloads are fetched on demand via S3 `GET_RANGE` and cached in an
    /// LRU cache bounded by `cache_budget`.
    ///
    /// - Without `cache_dir`: budget controls RAM cache size. Evicted blocks
    ///   are discarded.
    /// - With `cache_dir`: budget controls disk cache size (RAM tier is 256 MB).
    ///   Evicted blocks are spilled to disk and survive restarts.
    #[cfg(feature = "s3")]
    pub fn open_s3(
        remote: &engine::remote::RemoteSync,
        cache_budget: engine::cache::CacheBudget,
        cache_dir: Option<&std::path::Path>,
    ) -> Result<Self, String> {
        Self::open_s3_inner(remote, cache_budget, cache_dir)
    }

    #[cfg(feature = "s3")]
    fn open_s3_inner(
        remote: &engine::remote::RemoteSync,
        cache_budget: engine::cache::CacheBudget,
        cache_dir: Option<&std::path::Path>,
    ) -> Result<Self, String> {
        let manifest = remote
            .get_manifest()?
            .ok_or("no manifest found on remote")?;

        // Find payloads.bin size from manifest.
        let payload_size = manifest
            .segments
            .iter()
            .find(|s| s.name == "payloads.bin")
            .map(|s| s.size)
            .unwrap_or(0);

        // Fetch snapshot.json via RemoteSync (reuses its existing Runtime/connection).
        let snap_bytes = remote.fetch_file("snapshot.json")?;

        // Strip the versioned header (v2+) if present; legacy files start at 0.
        let (fmt_version, body_offset) = snapshot_probe(&snap_bytes);
        if fmt_version > SNAPSHOT_FORMAT_VERSION {
            return Err(newer_format_msg("snapshot", fmt_version, SNAPSHOT_FORMAT_VERSION));
        }
        let snap: Snapshot = serde_json::from_slice(&snap_bytes[body_offset..])
            .map_err(|e| format!("parsing snapshot: {e}"))?;

        let mut block_cache = if cache_dir.is_some() {
            // Disk-cached mode: budget controls disk tier, RAM is default 256MB.
            engine::cache::BlockCache::new(
                remote.store(),
                remote.prefix(),
                "payloads.bin",
                payload_size,
                cache_budget,
            )?
        } else {
            // RAM-only mode: budget controls RAM tier, no disk.
            engine::cache::BlockCache::new(
                remote.store(),
                remote.prefix(),
                "payloads.bin",
                payload_size,
                engine::cache::CacheBudget::new(0),
            )?
            .with_ram_cap(cache_budget.max_bytes() as usize)
        };

        if let Some(dir) = cache_dir {
            let payload_cache_dir = dir.join("payloads");
            block_cache = block_cache.with_cache_dir(payload_cache_dir)?;
        }

        let mut db = Self::new();
        db.payload_store = PayloadStore {
            binary: false,
            field_table: storage::skbin::FieldTable::new(),
            inner: PayloadInner::Remote {
                cache: std::sync::Mutex::new(block_cache),
            },
        };

        if snap.topology_in_files {
            // v3 manifest: fetch the topology files (small vs payloads) and
            // rebuild the resident graph from them. Payloads stay remote.
            let fetch = |name: &str| -> Result<Vec<u8>, String> {
                remote.fetch_file(name).map_err(|e| format!("fetching {name}: {e}"))
            };
            let blob = storage::topology::TopologyBlob {
                nodes: fetch("nodes.bin")?,
                fwd: fetch("adj_fwd.bin")?,
                rev: fetch("adj_rev.bin")?,
                idx: fetch("idx.bin")?,
                slugs: fetch("slugs.bin")?,
                dict: fetch("dict.bin")?,
                spat: fetch("spatial.bin").unwrap_or_default(),
                emeta: fetch("edgemeta.bin").unwrap_or_default(),
                colls: fetch("collections.bin").unwrap_or_default(),
            };
            db.load_snapshot_parts(snap, /*load_topology=*/ false);
            db.load_topology_blob(&blob)
                .map_err(|e| format!("loading topology files: {e}"))?;
        } else {
            db.load_snapshot(snap);
        }

        // Download small index files (GIN, search) if they exist on remote,
        // then load them to restore full-text search capability.
        let has_gin = manifest.segments.iter().any(|s| s.name == "gin.bin" && s.size > 12);
        let has_search = manifest.segments.iter().any(|s| s.name == "search.bin" && s.size > 12);

        if has_gin {
            if let Ok(gin_bytes) = remote.fetch_file("gin.bin") {
                let tmp = std::env::temp_dir().join(format!("sekejap_gin_{}.bin", std::process::id()));
                if std::fs::write(&tmp, &gin_bytes).is_ok() {
                    db.load_gin_binary(&tmp);
                    let _ = std::fs::remove_file(&tmp);
                }
            }
        }

        if has_search {
            if let Ok(search_bytes) = remote.fetch_file("search.bin") {
                let tmp = std::env::temp_dir().join(format!("sekejap_search_{}.bin", std::process::id()));
                if std::fs::write(&tmp, &search_bytes).is_ok() {
                    db.load_search_binary(&tmp);
                    let _ = std::fs::remove_file(&tmp);
                }
            }
        }

        // Rebuild indexes that aren't covered by sidecar files.
        // BM25 is always rebuilt from data (no sidecar).
        db.rebuild_declared_bm25_indexes();
        if !has_gin {
            db.rebuild_declared_gin_indexes();
        }
        if !has_search {
            db.rebuild_declared_search_indexes();
        }

        // Rebuild spatial grid for geo queries.
        db.rebuild_spatial_grid();

        // Rebuild HNSW from vectors loaded via snapshot.
        db.rebuild_declared_hnsw_indexes();

        Ok(db)
    }

    /// Open (or create) a persistent database with explicit configuration.
    ///
    /// On startup:
    /// 1. Loads the latest snapshot (if any).
    /// 2. Replays WAL entries written after the snapshot.
    /// 3. Opens the WAL for subsequent writes.
    ///
    /// If the WAL contains a corrupted frame, recovery stops at that frame —
    /// all entries before it are intact. A warning is printed to stderr.
    ///
    /// # Errors
    /// Returns an error if the directory cannot be created, the snapshot
    /// cannot be parsed, or the WAL file cannot be opened.
    pub fn open_with_config(dir: impl AsRef<Path>, config: Config) -> io::Result<Self> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)?;

        // Acquire exclusive file lock to prevent concurrent access.
        // Skipped in read-only mode (read replicas don't need exclusion).
        let lock_file = if config.read_only {
            None
        } else {
            let f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .open(dir.join("db.lock"))?;
            f.try_lock().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "database is locked by another process",
                )
            })?;
            Some(f)
        };

        let mut db = Self::new();
        db.data_dir = Some(dir.to_path_buf());
        db._lock_file = lock_file;

        // Apply edge storage mode from config.
        #[cfg(unix)]
        match config.edge_mode {
            EdgeMode::Compact => {
                db.edges = storage::edgestore::EdgeStore::open_compact(dir)?;
            }
            EdgeMode::Fat => { /* new() already created a Fat store */ }
        }
        #[cfg(not(unix))]
        let _ = &config;

        // 1. Load snapshot (peek before touching payloads.bin).
        //    Disk-backed snapshots store only metadata — payloads stay in payloads.bin.
        //    We must NOT truncate payloads.bin in that case.
        let snap_path = dir.join("snapshot.json");
        // Measure size before parsing — used later to detect legacy bloated snapshots.
        let snap_file_size = if snap_path.exists() {
            std::fs::metadata(&snap_path).map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };
        let snap: Option<Snapshot> = if snap_path.exists() {
            // Stream-parse rather than loading the whole file into RAM.
            // This handles legacy snapshots that embedded gin_indexes (multi-GB).
            // serde_json::from_reader reads incrementally; IgnoredAny skips gin_indexes
            // without allocating, so a 2.3GB legacy snapshot costs <1 MB to parse.
            let mut file = std::fs::File::open(&snap_path)?;
            // Probe the fixed header (16 bytes) before committing to a parser, so
            // a future binary/compressed snapshot is refused cleanly rather than
            // mis-parsed. Legacy headerless JSON reports (1, 0) and streams from 0.
            use std::io::{Read, Seek, SeekFrom};
            let mut head = [0u8; SNAPSHOT_HEADER_LEN];
            let n = file.read(&mut head).unwrap_or(0);
            let (fmt_version, body_offset) = snapshot_probe(&head[..n]);
            if fmt_version > SNAPSHOT_FORMAT_VERSION {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    newer_format_msg("snapshot", fmt_version, SNAPSHOT_FORMAT_VERSION),
                ));
            }
            file.seek(SeekFrom::Start(body_offset as u64))?;
            // Stream-parse rather than loading the whole file into RAM.
            // This handles legacy snapshots that embedded gin_indexes (multi-GB).
            // serde_json::from_reader reads incrementally; IgnoredAny skips gin_indexes
            // without allocating, so a 2.3GB legacy snapshot costs <1 MB to parse.
            match serde_json::from_reader::<_, Snapshot>(std::io::BufReader::new(file)) {
                Ok(s) if s.version > SNAPSHOT_FORMAT_VERSION => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        newer_format_msg("snapshot", s.version, SNAPSHOT_FORMAT_VERSION),
                    ));
                }
                Ok(s) => Some(s),
                Err(_) => None, // corrupt snapshot — fall back to full WAL replay
            }
        } else {
            None
        };

        // Phase 0 recovery: if the snapshot is missing/corrupt but the topology
        // files written at compact() exist, we can rebuild nodes + edges from them
        // (plus payloads.bin). Without this, a lost snapshot after compact() meant
        // data loss (the WAL was truncated). The healthy-snapshot path is unchanged.
        let topo_recovery = snap.is_none() && dir.join("nodes.bin").exists();

        // Paged topology (experimental, opt-in): mmap the topology files as the
        // read base instead of loading nodes+edges into RAM. Requires the files
        // from a prior compact(); falls back to resident loading when absent.
        let paged = config.paged_topology
            && snap.is_some()
            && dir.join("nodes.bin").exists()
            && dir.join("collections.bin").exists();

        // Open payload store: preserve existing payloads.bin for disk-backed snapshots
        // (and for topology recovery, which reads payloads in place), truncate to zero
        // otherwise (WAL replay or legacy snapshot will refill it).
        let pay_path = dir.join("payloads.bin");
        let preserve      = snap.as_ref().map_or(false, |s| s.is_disk_backed) || topo_recovery;
        let has_vec_files = snap.as_ref().map_or(false, |s| s.has_vector_files);
        if preserve && pay_path.exists() {
            let existing_len = std::fs::metadata(&pay_path)?.len();
            db.payload_store = PayloadStore::open_existing(&pay_path, existing_len)?;
        } else {
            db.payload_store = PayloadStore::open_file(&pay_path)?;
        }
        db.payload_store.binary = config.payload_binary;
        // Load the SKBIN field table if a prior compaction wrote one. Try each
        // redundant copy in turn; the first that passes its CRC wins (a corrupt
        // primary is recovered from a backup). Absent → no SKBIN records yet.
        for name in FIELD_TABLE_COPIES {
            if let Ok(bytes) = std::fs::read(dir.join(name)) {
                if let Some(ft) = storage::skbin::FieldTable::from_frame(&bytes) {
                    db.payload_store.field_table = ft;
                    break;
                }
            }
        }
        db.auto_compact = config.auto_compact;
        db.compact_thresholds = config.compact_thresholds;
        db.compact_on_close = config.compact_on_close;

        if let Some(snap) = snap {
            if paged {
                // Attach the mmap'd base; the snapshot supplies everything else
                // (schemas, vectors, HNSW, btree indexes). Nodes + edges are NOT
                // loaded into RAM — the resident maps stay empty and act as the
                // write overlay. WAL replay below adds post-compact writes to it.
                db.topo_base = Some(storage::topology::MappedTopology::open(dir)?);
                db.load_snapshot_parts(snap, /*load_topology=*/ false);
            } else if snap.topology_in_files {
                // v3 manifest: nodes + edges live in the topology files.
                db.load_snapshot_parts(snap, /*load_topology=*/ false);
                db.load_topology_files(dir)?;
            } else {
                db.load_snapshot(snap);
            }
        } else if topo_recovery {
            // Best-effort: a failure here degrades to the old behavior (WAL replay).
            let _ = db.load_topology_files(dir);
        }

        // Open disk-backed vector stores directly from .bin files.
        // When has_vector_files is set, load_snapshot() skipped parsing vectors
        // from JSON — instead we mmap the binary files (header scan only, no
        // float data loaded into RAM).  This must happen BEFORE WAL replay so
        // that PutVector entries append to the existing disk stores.
        #[cfg(unix)]
        if has_vec_files {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if let Some(field) = name
                        .strip_prefix("vectors_")
                        .and_then(|s| s.strip_suffix(".bin"))
                    {
                        if !field.is_empty() && !db.vectors.contains_key(field) {
                            let store =
                                storage::vecstore::VectorStore::open_disk(dir, field)?;
                            db.vectors.insert(field.to_string(), store);
                        }
                    }
                }
            }
        }

        // One-time migration: if the snapshot was large (legacy had embedded gin_indexes),
        // rewrite it immediately as a clean compact snapshot so subsequent opens are fast.
        // A normal disk-backed snapshot with 89k nodes is ~50-80 MB (pretty-printed).
        // The legacy bloated variant (gin_indexes embedded as JSON) was 1-10 GB.
        // Use 500 MB as the threshold — safely above any real snapshot, far below bloated ones.
        if snap_file_size > 500 * 1024 * 1024 {
            // v3 snapshots are manifests — the topology files must exist first.
            if db.write_topology_files(dir).is_ok() {
            if let Ok(snap_json) = serde_json::to_vec(&db.build_snapshot()) {
                let snap_tmp = snap_path.with_extension("json.tmp");
                if let Ok(mut sf) = std::fs::File::create(&snap_tmp) {
                    if std::io::Write::write_all(&mut sf, &snapshot_header_bytes(SNAPSHOT_FORMAT_VERSION)).is_ok()
                        && std::io::Write::write_all(&mut sf, &snap_json).is_ok()
                        && sf.sync_all().is_ok()
                    {
                        let _ = std::fs::rename(&snap_tmp, &snap_path);
                    }
                    }
                }
            }
        }

        // 2. Replay WAL — stream one entry at a time to avoid loading all
        //    payloads into RAM simultaneously (critical for large datasets).
        //    Track two separate flags:
        //    - wal_had_payload: Put/Remove/PutVector — affects GIN text indexes
        //    - wal_had_graph:   Link/LinkMeta/Unlink — only affects graph topology
        //    GIN rebuild is expensive (reads every payload from disk); we must not
        //    trigger it for edge-only WAL entries.
        let wal_path = dir.join("wal.log");
        let mut wal_had_payload = false;
        let mut wal_had_graph   = false;
        if wal_path.exists() {
            db.replaying = true;
            // Transaction-aware replay: entries between TxnBegin and TxnEnd
            // are buffered and applied together. If TxnEnd is missing (crash
            // during COMMIT), the entire group is discarded.
            let mut txn_buf: Option<Vec<WalEntry>> = None;
            let corrupted = WalReader::open(&wal_path)?.replay_all(|entry| {
                match &entry {
                    WalEntry::TxnBegin => {
                        txn_buf = Some(Vec::new());
                        return;
                    }
                    WalEntry::TxnEnd => {
                        if let Some(buf) = txn_buf.take() {
                            for e in buf {
                                match &e {
                                    WalEntry::Put { .. }
                                    | WalEntry::Remove { .. }
                                    | WalEntry::Update { .. }
                                    | WalEntry::PutVector { .. } => wal_had_payload = true,
                                    WalEntry::Link { .. }
                                    | WalEntry::LinkMeta { .. }
                                    | WalEntry::Unlink { .. } => wal_had_graph = true,
                                    _ => {}
                                }
                                db.replay(e);
                            }
                        }
                        return;
                    }
                    _ => {}
                }
                if let Some(buf) = &mut txn_buf {
                    buf.push(entry);
                } else {
                    match &entry {
                        WalEntry::Put { .. }
                        | WalEntry::Remove { .. }
                        | WalEntry::Update { .. }
                        | WalEntry::PutVector { .. } => wal_had_payload = true,
                        WalEntry::Link { .. }
                        | WalEntry::LinkMeta { .. }
                        | WalEntry::Unlink { .. } => wal_had_graph = true,
                        _ => {}
                    }
                    db.replay(entry);
                }
            });
            if txn_buf.is_some() {
                eprintln!(
                    "sekejap: WAL at `{}` had an incomplete transaction — \
                     discarded uncommitted entries.",
                    wal_path.display()
                );
            }
            db.replaying = false;
            if corrupted {
                eprintln!(
                    "sekejap: WAL at `{}` had a corrupted frame — \
                     replayed up to last good entry. Run compact() to clean up.",
                    wal_path.display()
                );
            }
        }

        // Remap edge metadata mmap so reads cover data written during
        // snapshot load + WAL replay.
        #[cfg(unix)]
        db.edges.remap_meta();

        // 3. Open WAL in append mode (skip for read-only replicas).
        if !config.read_only {
            let wal = WalWriter::open_with_format(&wal_path, config.wal_format)?;
            db.wal_format = wal.format();
            db.wal = Some(wal);
        }

        // 4. Build spatial index from loaded data
        db.rebuild_spatial_grid();

        // 5. Rebuild GIN and HNSW when WAL added new data, or load GIN from the
        //    binary sidecar gin.bin (compact, fast — no JSON parsing overhead).
        //    GIN: only rebuild when payload-mutating entries (Put/Remove) were in WAL.
        //    HNSW: rebuild when any data changed (payloads or vectors).
        let gin_bin_path = dir.join("gin.bin");
        let search_bin_path = dir.join("search.bin");
        if wal_had_payload {
            // Payload changed — rebuild all declared indexes from current data.
            // BM25/GIN/HNSW/Search builds are skipped during replay (apply_index guards
            // on self.replaying) so we must rebuild them all here, once.
            db.rebuild_declared_bm25_indexes();
            db.rebuild_declared_gin_indexes();
            db.rebuild_declared_hnsw_indexes();
            db.rebuild_declared_search_indexes();
            let _ = db.save_gin_binary(&gin_bin_path);
            let _ = db.save_search_binary(&search_bin_path);
        } else {
            // No payload changes — try loading GIN from gin.bin. If missing or
            // stale, rebuild once (covers first open after CREATE INDEX, etc.).
            if !db.load_gin_binary(&gin_bin_path) {
                db.rebuild_declared_gin_indexes();
                let _ = db.save_gin_binary(&gin_bin_path);
            }
            if !db.load_search_binary(&search_bin_path) {
                db.rebuild_declared_search_indexes();
                let _ = db.save_search_binary(&search_bin_path);
            }
            // HNSW: rebuild only when vectors changed (PutVector is part of wal_had_payload,
            // so here vectors are unchanged — no rebuild needed).
        }
        let _ = wal_had_graph; // used only to determine topology was replayed (no index rebuild needed)

        // 6. Migrate in-memory vectors to disk-backed stores.
        //    Stores already opened as disk (from .bin files) are left alone.
        //    Only memory-mode stores (from legacy snapshot or WAL-only fields)
        //    are written out to binary files and switched to disk mode.
        #[cfg(unix)]
        {
            let fields: Vec<String> = db.vectors.keys().cloned().collect();
            for field in fields {
                if db.vectors.get(&field).map_or(false, |s| s.is_disk()) {
                    continue; // already disk-backed — nothing to migrate
                }
                if let Some(mem_store) = db.vectors.remove(&field) {
                    let mut disk_store =
                        storage::vecstore::VectorStore::open_disk(dir, &field)?;
                    for (id, data) in mem_store.iter() {
                        disk_store.put(id, data.to_vec());
                    }
                    disk_store.remap();
                    db.vectors.insert(field, disk_store);
                }
            }
        }

        Ok(db)
    }

    // ── Raw internals (no WAL write — used during replay and open) ────────────

    fn put_raw(&mut self, slug: &str, payload_json: &str) -> Result<u64, serde_json::Error> {
        let payload: Value = serde_json::from_str(payload_json)?;
        self.put_raw_inner(slug, payload_json.as_bytes(), payload)
    }

    fn put_raw_inner(&mut self, slug: &str, raw: &[u8], payload: Value) -> Result<u64, serde_json::Error> {
        let hash = sk_hash(slug);
        // In a batch, all rows share one timestamp — skip a per-row time syscall.
        let now = self.batch_now.unwrap_or_else(|| chrono::Utc::now().timestamp_millis());

        if !payload.is_object() {
            return Err(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "payload must be a JSON object",
            )));
        }

        // Collision guard: node identity is `sk_hash(slug)` (u64). Two *different*
        // slugs that hash to the same value must never share a node — that would
        // silently merge two unrelated entities and their edges. The slug is
        // already stored per node, so this is a cheap lookup + string compare that
        // turns a rare-but-catastrophic silent merge into a loud, recoverable error.
        // (Re-putting the *same* slug is a normal update and passes through.)
        // `node_data` (not `self.nodes.get`) so both checks also cover the mapped
        // base in paged mode — collisions with base nodes must be caught, and
        // updates of base nodes must see their old collection/offsets.
        if let Some(existing) = self.node_data(hash) {
            if existing.slug != slug {
                return Err(serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!(
                        "hash collision: '{slug}' and existing '{}' both hash to {hash}; \
                         refusing to overwrite (rename one key)",
                        existing.slug
                    ),
                )));
            }
        }

        let old_info: Option<(String, u64, u32)> = self
            .node_data(hash)
            .map(|n| (n.collection.clone(), n.payload_offset, n.payload_len));

        // Splice timestamps into raw bytes (avoids re-serialize).
        let now_str = now.to_string();
        let mut buf = raw.to_vec();
        buf = query::splice_json_field(&buf, "_updated_unix", now_str.as_bytes())
            .unwrap_or(buf);

        if payload.get("_created_unix").is_none() {
            let created_str = old_info.as_ref()
                .and_then(|(_, off, len)| {
                    let old_raw = self.payload_store.get_raw(*off, *len)?;
                    let map = query::extract_fields_by_search(
                        &old_raw, &["_created_unix".to_string()],
                    );
                    map.get("_created_unix").and_then(|v| v.as_i64())
                })
                .map(|v| v.to_string())
                .unwrap_or_else(|| now_str.clone());
            buf = query::splice_json_field(&buf, "_created_unix", created_str.as_bytes())
                .unwrap_or(buf);
        }

        // Ensure identity fields are present in the payload so `_key`/`_id` are
        // always filterable/projectable (derived from the slug), matching what
        // SQL INSERT stores. `slug` = "<collection>/<key>".
        if payload.get("_id").is_none() {
            if let Ok(idv) = serde_json::to_string(slug) {
                buf = query::splice_json_field(&buf, "_id", idv.as_bytes()).unwrap_or(buf);
            }
        }
        if payload.get("_key").is_none() {
            let key = slug.split_once('/').map(|(_, k)| k).unwrap_or(slug);
            if let Ok(kv) = serde_json::to_string(key) {
                buf = query::splice_json_field(&buf, "_key", kv.as_bytes()).unwrap_or(buf);
            }
        }

        let spatial_meta = geo::extract_spatial_meta(&payload);

        // Remove old collection + field-index entries for this hash (if updating)
        if let Some((ref old_coll, old_off, old_len)) = old_info {
            if !old_coll.is_empty() {
                let coll_hash = sk_hash(old_coll);
                if let Some(members) = self.collections.get_mut(&coll_hash) {
                    members.retain(|&h| h != hash);
                }
                let has_fi = self.field_indexes.keys().any(|(c, _)| *c == coll_hash);
                if has_fi {
                    let old_payload = self.payload_store.get(old_off, old_len)
                        .unwrap_or(Value::Null);
                    for ((idx_coll, idx_field), btree) in &mut self.field_indexes {
                        if *idx_coll == coll_hash {
                            if let Some(key) = FieldKey::from_json(
                                old_payload.get(idx_field.as_str()).unwrap_or(&Value::Null)
                            ) {
                                if let Some(ids) = btree.get_mut(&key) {
                                    ids.retain(|&id| id != hash);
                                    if ids.is_empty() { btree.remove(&key); }
                                }
                            }
                        }
                    }
                }
            }
        }

        if let Some(coll) = payload.get("_collection").and_then(|v| v.as_str()) {
            let coll_hash = sk_hash(coll);
            let members = self.collections.entry(coll_hash).or_default();
            if !members.contains(&hash) {
                members.push(hash);
            }
            self.collection_names_map.entry(coll_hash).or_insert_with(|| coll.to_string());
            for ((idx_coll, idx_field), btree) in &mut self.field_indexes {
                if *idx_coll == coll_hash {
                    if let Some(key) = FieldKey::from_json(
                        payload.get(idx_field.as_str()).unwrap_or(&Value::Null)
                    ) {
                        let ids = btree.entry(key).or_default();
                        if !ids.contains(&hash) { ids.push(hash); }
                    }
                }
            }
        }

        let bm25_fields: Vec<String> = if self.bm25_indexes.is_empty() {
            Vec::new()
        } else {
            self.bm25_indexes
                .keys()
                .filter(|f| {
                    payload.get(f.as_str()).and_then(|v| v.as_str()).is_some()
                })
                .cloned()
                .collect()
        };

        // Store spliced bytes directly — no re-serialize.
        let (offset, len) = self.payload_store.append(&buf);

        let collection_str = payload.get("_collection")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        self.slug_map.insert(slug.to_string(), hash);
        self.nodes.insert(hash, NodeData {
            slug: slug.to_string(),
            collection: collection_str,
            spatial_meta: spatial_meta.clone(),
            payload_offset: offset,
            payload_len: len,
        });

        if self.defer_index_rebuild {
            for field in bm25_fields {
                self.dirty_bm25.insert(field);
            }
        } else {
            for field in bm25_fields {
                self.build_bm25_index(&field);
            }
        }

        if let Some(grid) = &mut self.spatial_grid {
            grid.remove(hash);
            if let Some(meta) = spatial_meta {
                grid.insert(hash, meta);
            }
        }

        // Search index: immutable FST → rebuild the collection's index
        // (deferred in a batch). Keeps new docs searchable, matching BM25.
        // Skipped during WAL replay — open() rebuilds search once at the end.
        if !self.replaying {
            let coll_for_search = self.nodes.get(&hash).map(|n| n.collection.clone());
            if let Some(coll) = coll_for_search {
                self.touch_search_index(&coll);
            }
        }

        Ok(hash)
    }

    fn remove_raw(&mut self, slug: &str) {
        let hash = sk_hash(slug);
        if let Some(node) = self.nodes.remove(&hash) {
            self.slug_map.remove(slug);
            if !node.collection.is_empty() {
                let coll_hash = sk_hash(&node.collection);
                if let Some(members) = self.collections.get_mut(&coll_hash) {
                    members.retain(|&h| h != hash);
                    if members.is_empty() {
                        self.collection_names_map.remove(&coll_hash);
                    }
                }
                // Remove from field indexes (read old payload from slab for key lookup)
                let has_fi = self.field_indexes.keys().any(|(c, _)| *c == coll_hash);
                if has_fi {
                    let old_payload = self.payload_store
                        .get(node.payload_offset, node.payload_len)
                        .unwrap_or(Value::Null);
                    for ((idx_coll, idx_field), btree) in &mut self.field_indexes {
                        if *idx_coll == coll_hash {
                            if let Some(key) = FieldKey::from_json(
                                old_payload.get(idx_field.as_str()).unwrap_or(&Value::Null)
                            ) {
                                if let Some(ids) = btree.get_mut(&key) {
                                    ids.retain(|&id| id != hash);
                                    if ids.is_empty() { btree.remove(&key); }
                                }
                            }
                        }
                    }
                }
            }
            // Cascade-delete edges involving this node (both directions).
            self.edges.remove_node(hash);

            if let Some(grid) = &mut self.spatial_grid {
                grid.remove(hash);
            }

            // Keep vector index consistent with main data: remove all field
            // entries for this node so orphan vectors never accumulate.
            for field_vecs in self.vectors.values_mut() {
                field_vecs.remove(hash);
            }

            // Incrementally remove the node from every HNSW graph: unlinks it
            // from neighbours' adjacency lists and re-selects the entry point if
            // needed. Prevents an orphan graph node from pointing at a vector we
            // just deleted (which would let search navigate to / return it).
            // Skipped during WAL replay — open() rebuilds HNSW once at the end.
            if !self.replaying {
                for graph in self.hnsw_indexes.values_mut() {
                    graph.remove(hash);
                }
            }

            // Incrementally update BM25 indexes: adjusts the running
            // `num_docs` and `sum_doc_len` counters and drops the
            // `doc_id_to_idx` entry so the deleted node is invisible
            // to subsequent searches immediately — without a full
            // rebuild.  The corresponding `doc_lengths` slot becomes a
            // harmless 4-byte orphan until the next rebuild.
            for bm25_idx in self.bm25_indexes.values_mut() {
                bm25_idx.delete(hash);
            }

            // Search + GIN have no incremental delete (immutable FST / trigram
            // bitmaps) → rebuild affected indexes, deferred inside a batch.
            // Skipped during replay: open() rebuilds everything once at the end.
            if !self.replaying {
                let coll = node.collection.clone();
                self.touch_search_index(&coll);

                if !self.gin_indexes.is_empty() {
                    let gin_fields: Vec<String> = self.gin_indexes.keys().cloned().collect();
                    if self.defer_index_rebuild {
                        for f in gin_fields { self.dirty_gin.insert(f); }
                    } else {
                        for f in gin_fields { self.build_gin_index(&f); }
                    }
                }
            }
        }
    }

    /// Drop an entire collection: removes all its nodes (cascading all edges),
    /// clears the declared schema, and removes the collection-level btree index
    /// entries. Returns the number of nodes deleted.
    fn drop_table_raw(&mut self, collection: &str) -> usize {
        let col_hash = sk_hash(collection);

        // Build a set of node hashes belonging to this collection
        let member_hashes: std::collections::HashSet<u64> = self.collections
            .get(&col_hash)
            .into_iter()
            .flat_map(|v| v.iter().copied())
            .collect();

        // Collect slugs (cannot hold borrow while mutating)
        let slugs: Vec<String> = self.slug_map
            .iter()
            .filter(|(_, h)| member_hashes.contains(h))
            .map(|(s, _)| s.clone())
            .collect();

        let count = slugs.len();

        // Defer per-node index rebuilds — otherwise deleting N nodes would
        // rebuild search/GIN N times (O(N²)). We rebuild once at the end.
        let was_deferring = self.defer_index_rebuild;
        self.defer_index_rebuild = true;
        for slug in slugs {
            self.remove_raw(&slug); // cascades edges, cleans per-node indexes
        }

        // The collection is gone — drop its search index outright rather than
        // rebuild it from (now absent) members, and forget any dirty entry.
        let search_key = Self::search_index_key(collection);
        self.search_indexes.remove(&search_key);
        self.dirty_search.remove(collection);

        // Remove the now-empty collection btree index entries
        self.field_indexes.retain(|(c, _), _| *c != col_hash);

        // Remove declared schema (if any)
        self.schemas.remove(collection);

        // Rebuild GIN/BM25 dirtied by the deletes (from remaining data), unless
        // we were already inside a larger deferred batch that will flush later.
        if !was_deferring {
            self.flush_deferred_indexes();
        }

        count
    }

    /// Apply an ALTER TABLE operation in-memory (no WAL write).
    /// Used by both execute() (which writes WAL after) and replay().
    fn alter_table_raw(&mut self, collection: &str, op: sql::AlterTableOp) -> Result<usize, sql::SqlError> {
        use sql::AlterTableOp;
        match op {
            // ── ADD COLUMN ────────────────────────────────────────────────────
            AlterTableOp::AddColumn { def } => {
                let schema = self.schemas.get_mut(collection).ok_or_else(|| {
                    sql::SqlError::InvalidValue(format!("table '{collection}' does not exist"))
                })?;
                if schema.fields.iter().any(|f| f.name == def.name) {
                    return Err(sql::SqlError::InvalidValue(format!(
                        "column '{}' already exists in '{collection}'",
                        def.name
                    )));
                }
                schema.fields.push(def);
                Ok(0) // schema-only; no rows touched
            }

            // ── DROP COLUMN ───────────────────────────────────────────────────
            AlterTableOp::DropColumn { name, if_exists } => {
                let (had_fulltext, had_bm25, had_hnsw) = {
                    let schema = self.schemas.get_mut(collection).ok_or_else(|| {
                        sql::SqlError::InvalidValue(format!("table '{collection}' does not exist"))
                    })?;
                    let idx = schema.fields.iter().position(|f| f.name == name);
                    match idx {
                        None if if_exists => return Ok(0),
                        None => return Err(sql::SqlError::InvalidValue(format!(
                            "column '{name}' does not exist in '{collection}'"
                        ))),
                        Some(i) => { schema.fields.remove(i); }
                    }
                    // Remove field from every index hint list so WAL replay
                    // doesn't try to rebuild an index for a dropped column.
                    let ix = &mut schema.indexes;
                    ix.range.retain(|f| f != &name);
                    ix.hash.retain(|f| f != &name);
                    let had_fulltext = ix.fulltext.iter().any(|f| f == &name);
                    ix.fulltext.retain(|f| f != &name);
                    let had_bm25 = ix.bm25.iter().any(|f| f == &name);
                    ix.bm25.retain(|f| f != &name);
                    ix.spatial.retain(|f| f != &name);
                    let had_hnsw = ix.vector.iter().any(|f| f == &name);
                    ix.vector.retain(|f| f != &name);
                    (had_fulltext, had_bm25, had_hnsw)
                }; // release schema borrow — returns tuple of global-index flags

                // Drop the btree index data for this field (no longer valid).
                let col_hash = sk_hash(collection);
                self.field_indexes.remove(&(col_hash, name.clone()));

                // Remove field from all nodes in the collection.
                // This must happen BEFORE rebuilding global indexes so the rebuild
                // naturally sees the field absent from this collection's nodes.
                let node_meta: Vec<(u64, u64, u32)> = self.collections
                    .get(&col_hash).into_iter().flatten()
                    .filter_map(|&h| self.nodes.get(&h).map(|n| (h, n.payload_offset, n.payload_len)))
                    .collect();
                let mut count = 0usize;
                let mut node_updates: Vec<(u64, u64, u32)> = Vec::new();
                for (h, off, len) in node_meta {
                    if let Some(mut p) = self.payload_store.get(off, len) {
                        if p.as_object_mut().map(|o| o.remove(&name).is_some()).unwrap_or(false) {
                            let new_json = serde_json::to_string(&p)
                                .unwrap_or_else(|_| "{}".to_string());
                            let (new_off, new_len) = self.payload_store.append(new_json.as_bytes());
                            node_updates.push((h, new_off, new_len));
                            count += 1;
                        }
                    }
                }
                for (h, new_off, new_len) in node_updates {
                    if let Some(node) = self.nodes.get_mut(&h) {
                        node.payload_offset = new_off;
                        node.payload_len = new_len;
                    }
                }

                // Rebuild global indexes from remaining data (nodes for the dropped
                // collection no longer carry the field, so the rebuild is naturally clean).
                // Only rebuild if the in-memory structure actually exists.
                if had_fulltext && self.gin_indexes.contains_key(&name)  { self.rebuild_gin_for_remaining(&name); }
                if had_bm25    && self.bm25_indexes.contains_key(&name)  { self.rebuild_bm25_for_remaining(&name); }
                if had_hnsw    && self.hnsw_indexes.contains_key(&name)  { self.rebuild_hnsw_for_remaining(&name); }

                Ok(count)
            }

            // ── RENAME COLUMN ─────────────────────────────────────────────────
            AlterTableOp::RenameColumn { old_name, new_name } => {
                {
                    let schema = self.schemas.get_mut(collection).ok_or_else(|| {
                        sql::SqlError::InvalidValue(format!("table '{collection}' does not exist"))
                    })?;
                    let idx = schema.fields.iter().position(|f| f.name == old_name)
                        .ok_or_else(|| sql::SqlError::InvalidValue(format!(
                            "column '{old_name}' does not exist in '{collection}'"
                        )))?;
                    if schema.fields.iter().any(|f| f.name == new_name) {
                        return Err(sql::SqlError::InvalidValue(format!(
                            "column '{new_name}' already exists in '{collection}'"
                        )));
                    }
                    schema.fields[idx].name = new_name.clone();
                } // release schema borrow

                // Rename the field key in every node of the collection
                let col_hash = sk_hash(collection);
                let node_meta: Vec<(u64, u64, u32)> = self.collections
                    .get(&col_hash).into_iter().flatten()
                    .filter_map(|&h| self.nodes.get(&h).map(|n| (h, n.payload_offset, n.payload_len)))
                    .collect();
                let mut count = 0usize;
                let mut node_updates: Vec<(u64, u64, u32)> = Vec::new();
                for (h, off, len) in node_meta {
                    if let Some(mut p) = self.payload_store.get(off, len) {
                        if let Some(obj) = p.as_object_mut() {
                            if let Some(val) = obj.remove(&old_name) {
                                obj.insert(new_name.clone(), val);
                                let new_json = serde_json::to_string(&p)
                                    .unwrap_or_else(|_| "{}".to_string());
                                let (new_off, new_len) = self.payload_store.append(new_json.as_bytes());
                                node_updates.push((h, new_off, new_len));
                                count += 1;
                            }
                        }
                    }
                }
                for (h, new_off, new_len) in node_updates {
                    if let Some(node) = self.nodes.get_mut(&h) {
                        node.payload_offset = new_off;
                        node.payload_len = new_len;
                    }
                }

                // Move the btree index data from old field name to new field name
                if let Some(btree) = self.field_indexes.remove(&(col_hash, old_name.clone())) {
                    self.field_indexes.insert((col_hash, new_name.clone()), btree);
                }

                // Update field name inside every index hint list so WAL replay
                // rebuilds the index under the new name.
                if let Some(schema) = self.schemas.get_mut(collection) {
                    for list in [
                        &mut schema.indexes.range,
                        &mut schema.indexes.hash,
                        &mut schema.indexes.fulltext,
                        &mut schema.indexes.bm25,
                        &mut schema.indexes.spatial,
                        &mut schema.indexes.vector,
                    ] {
                        for entry in list.iter_mut() {
                            if *entry == old_name {
                                *entry = new_name.clone();
                            }
                        }
                    }
                }

                Ok(count)
            }

            // ── RENAME TABLE ──────────────────────────────────────────────────
            // Note: existing slugs (e.g. "old_col/key") remain unchanged.
            // Only the logical _collection metadata and index buckets are moved.
            AlterTableOp::RenameTable { new_name } => {
                if self.schemas.contains_key(&new_name) {
                    return Err(sql::SqlError::InvalidValue(format!(
                        "table '{new_name}' already exists"
                    )));
                }
                let mut schema = self.schemas.remove(collection).ok_or_else(|| {
                    sql::SqlError::InvalidValue(format!("table '{collection}' does not exist"))
                })?;
                schema.collection = new_name.clone();
                self.schemas.insert(new_name.clone(), schema);

                // Move collection bucket to new hash
                let old_hash = sk_hash(collection);
                let new_hash = sk_hash(&new_name);
                let node_hashes: Vec<u64> =
                    self.collections.remove(&old_hash).unwrap_or_default();
                let count = node_hashes.len();
                self.collections.insert(new_hash, node_hashes.clone());
                // Update the O(1) name map
                self.collection_names_map.remove(&old_hash);
                self.collection_names_map.insert(new_hash, new_name.clone());

                // Update _collection field in every node payload + cached collection field
                let node_meta: Vec<(u64, u64, u32)> = node_hashes.iter()
                    .filter_map(|&h| self.nodes.get(&h).map(|n| (h, n.payload_offset, n.payload_len)))
                    .collect();
                let mut node_updates: Vec<(u64, u64, u32)> = Vec::new();
                for (h, off, len) in node_meta {
                    if let Some(mut p) = self.payload_store.get(off, len) {
                        if let Some(obj) = p.as_object_mut() {
                            obj.insert("_collection".to_string(), serde_json::json!(new_name));
                        }
                        let new_json = serde_json::to_string(&p)
                            .unwrap_or_else(|_| "{}".to_string());
                        let (new_off, new_len) = self.payload_store.append(new_json.as_bytes());
                        node_updates.push((h, new_off, new_len));
                    }
                }
                for (h, new_off, new_len) in node_updates {
                    if let Some(node) = self.nodes.get_mut(&h) {
                        node.collection = new_name.clone();
                        node.payload_offset = new_off;
                        node.payload_len = new_len;
                    }
                }

                // Move field_indexes from old collection hash to new
                let old_keys: Vec<(u64, String)> = self.field_indexes.keys()
                    .filter(|(c, _)| *c == old_hash)
                    .cloned()
                    .collect();
                for (_, field) in old_keys {
                    if let Some(btree) = self.field_indexes.remove(&(old_hash, field.clone())) {
                        self.field_indexes.insert((new_hash, field), btree);
                    }
                }

                Ok(count)
            }

            // ── ALTER COLUMN TYPE ─────────────────────────────────────────────
            // Schema annotation updated; existing data is not coerced.
            // If a btree index exists for this field it is rebuilt from scratch
            // so FieldKey variants match the new type (mirrors PostgreSQL REINDEX).
            AlterTableOp::AlterColumnType { name, ty } => {
                let has_btree = {
                    let schema = self.schemas.get_mut(collection).ok_or_else(|| {
                        sql::SqlError::InvalidValue(format!("table '{collection}' does not exist"))
                    })?;
                    let field = schema.fields.iter_mut().find(|f| f.name == name)
                        .ok_or_else(|| sql::SqlError::InvalidValue(format!(
                            "column '{name}' does not exist in '{collection}'"
                        )))?;
                    field.ty = ty;
                    schema.indexes.range.contains(&name)
                };

                if has_btree {
                    // Drop stale btree entries, then rebuild from current node data.
                    let col_hash = sk_hash(collection);
                    self.field_indexes.remove(&(col_hash, name.clone()));
                    self.build_field_index(collection, &name);
                }

                Ok(0)
            }
        }
    }

    /// Remove one index from a collection, then rebuild any global structure
    /// (GIN / BM25 / HNSW) from only the collections that still hold that index.
    ///
    /// Returns `true` when the index existed and was removed, `false` otherwise.
    fn drop_index_raw(&mut self, collection: &str, method: &sql::IndexMethod, field: &str) -> bool {
        use sql::IndexMethod;

        // Remove the hint from this collection's schema.
        let removed = if let Some(schema) = self.schemas.get_mut(collection) {
            if matches!(method, IndexMethod::Search) {
                let before = schema.indexes.search.len();
                schema.indexes.search.retain(|fields| !fields.contains(&field.to_string()));
                schema.indexes.search.len() < before
            } else {
                let list = match method {
                    IndexMethod::Btree                  => &mut schema.indexes.range,
                    IndexMethod::Hash                   => &mut schema.indexes.hash,
                    IndexMethod::Gin | IndexMethod::Gist => &mut schema.indexes.fulltext,
                    IndexMethod::Bm25                   => &mut schema.indexes.bm25,
                    IndexMethod::Spatial                => &mut schema.indexes.spatial,
                    IndexMethod::Hnsw                   => &mut schema.indexes.vector,
                    IndexMethod::Search                 => unreachable!(),
                };
                let before = list.len();
                list.retain(|f| f != field);
                list.len() < before
            }
        } else {
            false
        };

        if !removed {
            return false;
        }

        let col_hash = sk_hash(collection);

        match method {
            // ── Per-collection indexes: drop directly ─────────────────────────
            IndexMethod::Btree => {
                self.field_indexes.remove(&(col_hash, field.to_string()));
            }
            IndexMethod::Hash => {
                // Hint-only — nothing to drop.
            }

            // ── Global indexes: rebuild from remaining indexed collections ─────
            // After removing this collection's hint, re-scan only the collections
            // whose schema still lists this field in the relevant hint.
            // Only rebuild if the in-memory index actually exists — if CREATE INDEX
            // was called before any data was inserted, there is no structure to
            // clean up, and creating one here would produce stale truncated IDs.
            IndexMethod::Gin | IndexMethod::Gist => {
                if self.gin_indexes.contains_key(field) {
                    self.rebuild_gin_for_remaining(field);
                }
            }
            IndexMethod::Bm25 => {
                if self.bm25_indexes.contains_key(field) {
                    self.rebuild_bm25_for_remaining(field);
                }
            }
            IndexMethod::Hnsw => {
                if self.hnsw_indexes.contains_key(field) {
                    self.rebuild_hnsw_for_remaining(field);
                }
            }
            IndexMethod::Spatial => {
                // Spatial grid covers all GEO nodes regardless of collection;
                // removing the hint is sufficient — no rebuild needed.
            }
            IndexMethod::Search => {
                // Clear all search indexes for this collection, then rebuild
                // only those still declared in the schema.
                let key = Self::search_index_key(collection);
                self.search_indexes.remove(&key);
                self.rebuild_declared_search_indexes();
            }
        }

        true
    }

    /// Rebuild `gin_indexes[field]` from only the collections whose schema
    /// still declares a `fulltext` index on this field.
    fn rebuild_gin_for_remaining(&mut self, field: &str) {
        let col_hashes: Vec<u64> = self.schemas.values()
            .filter(|s| s.indexes.fulltext.contains(&field.to_string()))
            .map(|s| sk_hash(&s.collection))
            .collect();

        if col_hashes.is_empty() {
            self.gin_indexes.remove(field);
            return;
        }

        let values: Vec<(u64, String)> = col_hashes.iter()
            .flat_map(|ch| self.collections.get(ch).into_iter().flatten().copied())
            .filter_map(|hash| {
                let node = self.nodes.get(&hash)?;
                let payload = self.payload_store.get(node.payload_offset, node.payload_len)?;
                payload.get(field)?.as_str().map(|s| (hash, s.to_string()))
            })
            .collect();

        if values.is_empty() {
            self.gin_indexes.remove(field);
        } else {
            let refs: Vec<(u64, &str)> = values.iter().map(|(h, s)| (*h, s.as_str())).collect();
            let index = text_index::gin::GINIndex::build(refs.into_iter(), field);
            self.gin_indexes.insert(field.to_string(), index);
        }
    }

    /// Rebuild `bm25_indexes[field]` from only the collections whose schema
    /// still declares a `bm25` index on this field.
    fn rebuild_bm25_for_remaining(&mut self, field: &str) {
        let col_hashes: Vec<u64> = self.schemas.values()
            .filter(|s| s.indexes.bm25.contains(&field.to_string()))
            .map(|s| sk_hash(&s.collection))
            .collect();

        if col_hashes.is_empty() {
            self.bm25_indexes.remove(field);
            return;
        }

        let values: Vec<(u64, String)> = col_hashes.iter()
            .flat_map(|ch| self.collections.get(ch).into_iter().flatten().copied())
            .filter_map(|hash| {
                let node = self.nodes.get(&hash)?;
                let payload = self.payload_store.get(node.payload_offset, node.payload_len)?;
                payload.get(field)?.as_str().map(|s| (hash, s.to_string()))
            })
            .collect();

        if values.is_empty() {
            self.bm25_indexes.remove(field);
        } else {
            let refs: Vec<(u64, &str)> = values.iter().map(|(h, s)| (*h, s.as_str())).collect();
            let index = bm25::Bm25Index::build(field, refs.into_iter());
            self.bm25_indexes.insert(field.to_string(), index);
        }
    }

    /// Rebuild `hnsw_indexes[field]` from only the collections whose schema
    /// still declares a `vector` index on this field.
    fn rebuild_hnsw_for_remaining(&mut self, field: &str) {
        let col_hashes: Vec<u64> = self.schemas.values()
            .filter(|s| s.indexes.vector.contains(&field.to_string()))
            .map(|s| sk_hash(&s.collection))
            .collect();

        if col_hashes.is_empty() {
            self.hnsw_indexes.remove(field);
            return;
        }

        let member_hashes: std::collections::HashSet<u64> = col_hashes.iter()
            .flat_map(|ch| self.collections.get(ch).into_iter().flatten().copied())
            .collect();

        if let Some(field_vecs) = self.vectors.get(field) {
            let filtered: HashMap<u64, Vec<f32>> = field_vecs.iter()
                .filter(|(h, _)| member_hashes.contains(h))
                .map(|(h, v)| (h, v.to_vec()))
                .collect();

            if filtered.is_empty() {
                self.hnsw_indexes.remove(field);
            } else {
                use vector::CosineDistance;
                let graph = vector::HnswGraph::build::<CosineDistance, _>(&filtered, 16, 200);
                self.hnsw_indexes.insert(field.to_string(), graph);
            }
        } else {
            self.hnsw_indexes.remove(field);
        }
    }

    fn link_raw(&mut self, from: &str, to: &str, edge_type: &str) {
        let from_h = sk_hash(from);
        let to_h = sk_hash(to);
        let type_h = sk_hash(edge_type);
        self.edges.link(from_h, to_h, type_h, edge_type);
    }

    fn link_meta_raw(
        &mut self,
        from: &str,
        to: &str,
        edge_type: &str,
        meta_json: &str,
    ) -> Result<(), serde_json::Error> {
        let meta: Value = serde_json::from_str(meta_json)?;
        let from_h = sk_hash(from);
        let to_h = sk_hash(to);
        let type_h = sk_hash(edge_type);
        // Route by value type: primitives → fast-lane columns, the rest → JSON
        // bag. This is the ONE routing site — the public `link_meta`, `link_attr`,
        // the SQL edge-insert, and WAL replay all funnel through here, so a routed
        // edge rebuilds identically on reopen (persistence for free).
        match meta {
            Value::Object(m) => {
                let (cols, json) =
                    Self::route_edge_attrs(m.into_iter().collect());
                self.edges
                    .link_with_attrs(from_h, to_h, type_h, edge_type, &cols, json);
            }
            // Non-object meta (rare): keep whole in the JSON bag as before.
            other => {
                self.edges
                    .link_meta(from_h, to_h, type_h, edge_type, other);
            }
        }
        Ok(())
    }

    fn unlink_raw(&mut self, from: &str, to: &str, edge_type: &str) {
        let from_h = sk_hash(from);
        let to_h = sk_hash(to);
        let type_h = sk_hash(edge_type);
        self.edges.unlink(from_h, to_h, type_h);
    }

    // ── WAL helpers ───────────────────────────────────────────────────────────

    fn wal_write(&mut self, entry: WalEntry) {
        if let Some(wal) = &mut self.wal {
            wal.append(&entry)
                .expect("sekejap: WAL write failed — disk error");
            if !self.defer_wal_sync {
                wal.sync()
                    .expect("sekejap: WAL fsync failed — disk error");
            }
        }
    }

    /// SQLite-style inline auto-compaction: the write that crosses a threshold
    /// pays for the compact. MUST only be called at the END of a fully-applied
    /// public mutation (maps + WAL both updated) — never from inside `wal_write`,
    /// where the WAL entry precedes the map update and an inline compact would
    /// snapshot pre-mutation state while truncating the entry (data loss; caught
    /// by test auto_compact_on_write_fires_and_truncates_wal). Guards: bulk loads
    /// and transactions run with `defer_wal_sync = true` and are never
    /// interrupted; reentrancy is blocked; the WAL stat is amortised to every
    /// 64th write.
    fn autocompact_after_write(&mut self) {
        if self.auto_compact != AutoCompact::OnWrite
            || self.defer_wal_sync
            || self.autocompacting
        {
            return;
        }
        self.writes_since_compact_check += 1;
        if self.writes_since_compact_check < 64 {
            return;
        }
        self.writes_since_compact_check = 0;
        if self.compact_eligible() {
            self.autocompacting = true;
            // Best-effort: a failed auto-compact leaves the WAL intact (safe);
            // persistent failures surface on the next explicit compact().
            let _ = self.compact();
            self.autocompacting = false;
        }
    }

    /// Have the auto-compaction thresholds been crossed?
    fn compact_eligible(&self) -> bool {
        let Some(dir) = &self.data_dir else { return false };
        if self.auto_compact == AutoCompact::Off {
            return false;
        }
        let wal_len = std::fs::metadata(dir.join("wal.log"))
            .map(|m| m.len())
            .unwrap_or(0);
        if wal_len > self.compact_thresholds.wal_bytes {
            return true;
        }
        // Paged mode: the resident maps are the RAM write-overlay.
        self.topo_base.is_some() && self.nodes.len() >= self.compact_thresholds.overlay_entries
    }

    /// Compact **iff** the auto-compaction thresholds are crossed. The idle-time
    /// companion to [`Config::auto_compact`]`::Manual`: the engine decides *if*,
    /// the app decides *when* (call this in request-loop gaps / device idle).
    /// Returns `Ok(true)` when a compaction ran.
    pub fn maybe_compact(&mut self) -> io::Result<bool> {
        if !self.compact_eligible() {
            return Ok(false);
        }
        self.compact()?;
        Ok(true)
    }

    fn wal_flush(&mut self) {
        if let Some(wal) = &mut self.wal {
            wal.sync()
                .expect("sekejap: WAL fsync failed — disk error");
        }
    }

    fn flush_deferred_indexes(&mut self) {
        let bm25_fields: Vec<String> = self.dirty_bm25.drain().collect();
        for field in bm25_fields {
            self.build_bm25_index(&field);
        }
        let gin_fields: Vec<String> = self.dirty_gin.drain().collect();
        for field in gin_fields {
            self.build_gin_index(&field);
        }
        let search_colls: Vec<String> = self.dirty_search.drain().collect();
        for coll in search_colls {
            self.rebuild_search_for_collection(&coll);
        }
        self.defer_index_rebuild = false;
    }

    /// Does `collection` have at least one declared `search` index?
    fn collection_has_search_index(&self, collection: &str) -> bool {
        self.schemas.get(collection)
            .map_or(false, |s| !s.indexes.search.is_empty())
    }

    /// Rebuild every declared `search` index for a single collection.
    fn rebuild_search_for_collection(&mut self, collection: &str) {
        let field_sets: Vec<Vec<String>> = match self.schemas.get(collection) {
            Some(s) => s.indexes.search.clone(),
            None => return,
        };
        for fields in field_sets {
            self.build_search_index(collection, &fields);
        }
    }

    /// Mark a collection's search index for rebuild — deferred inside a batch,
    /// immediate otherwise. No-op if the collection has no search index.
    fn touch_search_index(&mut self, collection: &str) {
        if collection.is_empty() || !self.collection_has_search_index(collection) {
            return;
        }
        if self.defer_index_rebuild {
            self.dirty_search.insert(collection.to_string());
        } else {
            self.rebuild_search_for_collection(collection);
        }
    }

    /// UPDATE fast path: byte-level splice, batch payload write, batch WAL.
    ///
    /// Shared by `execute()` (which captures `now_ms` at statement time) and
    /// WAL replay of logical `WalEntry::Update` entries (which passes the
    /// stored timestamp so `_updated_unix` is reproduced exactly). During
    /// replay `self.wal` is `None`, so no log entries are written.
    fn update_fast_path(
        &mut self,
        steps: Vec<Step>,
        updates: &[(String, Value)],
        now_ms: i64,
    ) -> Result<usize, SqlError> {
        // Logical WAL: serialize the compiled statement BEFORE `steps` is
        // consumed by the filter pipeline. Written only after the statement
        // succeeds and matched at least one row.
        let logical_entry = if self.logical_wal && self.wal.is_some() {
            Some(WalEntry::Update {
                steps_json: serde_json::to_string(&steps)
                    .map_err(|e| SqlError::InvalidValue(e.to_string()))?,
                updates_json: serde_json::to_string(updates)
                    .map_err(|e| SqlError::InvalidValue(e.to_string()))?,
                now_ms,
            })
        } else {
            None
        };

        // Hashes only — collect() would parse every payload into a Value we
        // immediately discard (we splice raw bytes below).
        let hits: Vec<(String, u64, Vec<u8>)> = Set::from_steps(self, steps)
            .collect_hashes()
            .into_iter()
            .filter_map(|hash| {
                let n = self.nodes.get(&hash)?;
                let raw = self.payload_store.get_raw(n.payload_offset, n.payload_len)?;
                Some((n.slug.clone(), hash, raw))
            })
            .collect();
        let count = hits.len();
        if count == 0 { return Ok(0); }

        // Schema validation (once for the batch)
        let coll_name = self.nodes.get(&hits[0].1)
            .map(|n| n.collection.clone()).unwrap_or_default();
        if let Some(schema) = self.schemas.get(&coll_name) {
            if let Some(err) = validate_updates_against_schema(schema, updates) {
                return Err(err);
            }
        }
        let coll_hash = if !coll_name.is_empty() { Some(sk_hash(&coll_name)) } else { None };

        // Pre-serialize each update value once (not per row)
        let update_bytes: Vec<(&str, Vec<u8>)> = updates.iter()
            .map(|(f, v)| (f.as_str(), serde_json::to_vec(v).unwrap()))
            .collect();

        // Which updated fields have btree indexes?
        let indexed_fields: Vec<&str> = if let Some(ch) = coll_hash {
            updates.iter()
                .filter(|(f, _)| self.field_indexes.contains_key(&(ch, f.clone())))
                .map(|(f, _)| f.as_str())
                .collect()
        } else {
            vec![]
        };

        let now_bytes = now_ms.to_string().into_bytes();

        // ── Phase 1: splice bytes + btree updates (no I/O) ───────
        let field_names: Vec<String> = indexed_fields.iter().map(|f| f.to_string()).collect();
        let mut batch: Vec<(String, u64, Vec<u8>)> = Vec::with_capacity(count);

        for (slug, hash, raw) in hits {
            // Remove old btree entries for indexed fields being updated
            if let Some(ch) = coll_hash {
                let extracted = crate::query::extract_fields_by_search(&raw, &field_names);
                for &field in &indexed_fields {
                    if let Some(old_val) = extracted.get(field) {
                        if let Some(old_key) = FieldKey::from_json(old_val) {
                            if let Some(btree) = self.field_indexes.get_mut(&(ch, field.to_string())) {
                                if let Some(ids) = btree.get_mut(&old_key) {
                                    ids.retain(|&id| id != hash);
                                    if ids.is_empty() { btree.remove(&old_key); }
                                }
                            }
                        }
                    }
                }
            }

            // Splice each field + _updated_unix directly in raw bytes
            let mut buf = raw;
            for (field, val_bytes) in &update_bytes {
                if let Some(spliced) = crate::query::splice_json_field(&buf, field, val_bytes) {
                    buf = spliced;
                }
            }
            if let Some(spliced) = crate::query::splice_json_field(&buf, "_updated_unix", &now_bytes) {
                buf = spliced;
            }

            // Add new btree entries for indexed fields
            if let Some(ch) = coll_hash {
                for &field in &indexed_fields {
                    if let Some((_, new_val)) = updates.iter().find(|(f, _)| f == field) {
                        if let Some(new_key) = FieldKey::from_json(new_val) {
                            if let Some(btree) = self.field_indexes.get_mut(&(ch, field.to_string())) {
                                let ids = btree.entry(new_key).or_default();
                                if !ids.contains(&hash) { ids.push(hash); }
                            }
                        }
                    }
                }
            }

            batch.push((slug, hash, buf));
        }

        // ── Phase 2: batch payload write (one syscall) ───────────
        let offsets = {
            let refs: Vec<&[u8]> = batch.iter()
                .map(|(_, _, buf)| buf.as_slice()).collect();
            self.payload_store.append_batch(&refs)
        };

        // ── Phase 3: update node metadata ────────────────────────
        for (i, (_, hash, _)) in batch.iter().enumerate() {
            if let Some(node) = self.nodes.get_mut(hash) {
                node.payload_offset = offsets[i].0;
                node.payload_len = offsets[i].1;
            }
        }

        // ── Phase 4: WAL ─────────────────────────────────────────
        // Logical mode: one command entry for the whole statement.
        // Physical mode: one Put per row, batch-encoded, one flush.
        // Memory mode (wal = None): skip entry construction entirely.
        if self.wal.is_some() {
            if let Some(entry) = logical_entry {
                self.wal_write(entry);
            } else {
                let mut wal_entries = Vec::with_capacity(batch.len());
                for (slug, _, buf) in batch {
                    let payload = String::from_utf8(buf)
                        .expect("spliced JSON bytes were not valid UTF-8");
                    wal_entries.push(WalEntry::Put { slug, payload });
                }
                if let Some(wal) = &mut self.wal {
                    wal.append_batch(&wal_entries)
                        .expect("sekejap: WAL batch write failed");
                    if !self.defer_wal_sync {
                        wal.sync().expect("sekejap: WAL fsync failed");
                    }
                }
            }
        }

        // Rebuild GIN/BM25 once for the whole batch (not per row).
        for (field, _) in updates {
            if self.gin_indexes.contains_key(field.as_str()) {
                self.build_gin_index(field);
            }
            if self.bm25_indexes.contains_key(field.as_str()) {
                self.build_bm25_index(field);
            }
        }
        // Search index spans the whole collection — rebuild once if present.
        if !coll_name.is_empty() {
            self.touch_search_index(&coll_name);
        }

        Ok(count)
    }

    fn replay(&mut self, entry: WalEntry) {
        match entry {
            WalEntry::Put { slug, payload } => {
                let _ = self.put_raw(&slug, &payload);
            }
            WalEntry::Remove { slug } => self.remove_raw(&slug),
            WalEntry::Link {
                from,
                to,
                edge_type,
            } => {
                self.link_raw(&from, &to, &edge_type);
            }
            WalEntry::LinkMeta {
                from,
                to,
                edge_type,
                meta,
            } => {
                let _ = self.link_meta_raw(&from, &to, &edge_type, &meta);
            }
            WalEntry::Unlink {
                from,
                to,
                edge_type,
            } => {
                self.unlink_raw(&from, &to, &edge_type);
            }
            WalEntry::CreateTable {
                collection: _,
                schema_json,
            } => {
                if let Ok(schema) = serde_json::from_str::<sql::TableSchema>(&schema_json) {
                    self.schemas.insert(schema.collection.clone(), schema);
                }
            }
            WalEntry::PutVector { slug, field, data } => {
                let hash = sk_hash(&slug);
                self.vectors.entry(field).or_default().put(hash, data);
            }
            WalEntry::CreateIndex { collection, method, fields } => {
                use sql::IndexMethod;
                let m = match method.as_str() {
                    "btree"   => IndexMethod::Btree,
                    "hash"    => IndexMethod::Hash,
                    "gin"     => IndexMethod::Gin,
                    "gist"    => IndexMethod::Gist,
                    "bm25"    => IndexMethod::Bm25,
                    "spatial" => IndexMethod::Spatial,
                    "hnsw"    => IndexMethod::Hnsw,
                    _ => return,
                };
                // WAL replay is fault-tolerant — ignore build failures.
                let _ = self.apply_index(&collection, &m, &fields);
            }
            WalEntry::DropTable { collection } => {
                self.drop_table_raw(&collection);
            }
            WalEntry::DropIndex { collection, method, field } => {
                use sql::IndexMethod;
                let m = match method.as_str() {
                    "btree"   => IndexMethod::Btree,
                    "hash"    => IndexMethod::Hash,
                    "gin"     => IndexMethod::Gin,
                    "gist"    => IndexMethod::Gist,
                    "bm25"    => IndexMethod::Bm25,
                    "spatial" => IndexMethod::Spatial,
                    "hnsw"    => IndexMethod::Hnsw,
                    _ => return,
                };
                self.drop_index_raw(&collection, &m, &field);
            }
            WalEntry::AlterTable { collection, op_json } => {
                if let Ok(op) = serde_json::from_str::<sql::AlterTableOp>(&op_json) {
                    let _ = self.alter_table_raw(&collection, op);
                }
            }
            WalEntry::Update { steps_json, updates_json, now_ms } => {
                // Logical UPDATE: re-execute the compiled statement against
                // the replayed-so-far state (identical to what it saw at
                // runtime). The stored timestamp reproduces _updated_unix.
                if let (Ok(steps), Ok(updates)) = (
                    serde_json::from_str::<Vec<Step>>(&steps_json),
                    serde_json::from_str::<Vec<(String, Value)>>(&updates_json),
                ) {
                    let _ = self.update_fast_path(steps, &updates, now_ms);
                }
            }
            // Transaction markers are handled by the replay loop in open_with_config(),
            // not by individual entry replay. If they reach here, skip them.
            WalEntry::TxnBegin | WalEntry::TxnEnd => {}
            WalEntry::Unknown => { /* forward-compat: skip entries from newer binaries */ }
        }
    }

    // ── Writes ────────────────────────────────────────────────────────────────

    /// Insert or update a node. The `_collection` field in the payload
    /// registers the node in a named collection for `db.collection()` queries.
    ///
    /// Returns the slug hash on success.
    pub fn put(&mut self, slug: &str, payload_json: &str) -> Result<u64, serde_json::Error> {
        let payload: Value = serde_json::from_str(payload_json)?;

        self.wal_write(WalEntry::Put {
            slug: slug.to_string(),
            payload: payload_json.to_string(),
        });

        let node_hash = sk_hash(slug);
        let is_update = self.nodes.contains_key(&node_hash);

        // Pre-collect GIN info from the parsed Value before put_raw_inner consumes it.
        let gin_updates: Vec<(String, Option<String>)> =
            if let Some(coll) = payload.get("_collection").and_then(|v| v.as_str()) {
                let coll_hash = sk_hash(coll);
                self.schemas.values()
                    .filter(|s| sk_hash(&s.collection) == coll_hash)
                    .flat_map(|s| s.indexes.fulltext.iter().map(|f| {
                        let text = payload.get(f.as_str())
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        (f.clone(), text)
                    }))
                    .collect()
            } else {
                Vec::new()
            };

        let hash = self.put_raw_inner(slug, payload_json.as_bytes(), payload)?;

        for (gin_field, text_opt) in gin_updates {
            if self.defer_index_rebuild {
                self.dirty_gin.insert(gin_field);
            } else if is_update {
                self.build_gin_index(&gin_field);
            } else if let Some(text) = text_opt {
                if let Some(gin_idx) = self.gin_indexes.get_mut(gin_field.as_str()) {
                    gin_idx.insert_doc(hash, &text);
                } else {
                    self.build_gin_index(&gin_field);
                }
            }
        }

        self.autocompact_after_write();
        Ok(hash)
    }

    /// Bulk insert. Stops and returns the first error encountered.
    ///
    /// Defers WAL fsync and expensive index rebuilds (BM25, GIN) until
    /// the entire batch is inserted, then flushes once — O(N) total
    /// instead of O(N²).
    pub fn put_many<'a>(
        &mut self,
        items: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Vec<u64>, serde_json::Error> {
        self.defer_wal_sync = true;
        self.defer_index_rebuild = true;
        let result: Result<Vec<u64>, _> = items
            .into_iter()
            .map(|(slug, json)| self.put(slug, json))
            .collect();
        self.defer_wal_sync = false;
        self.wal_flush();
        self.flush_deferred_indexes();
        self.autocompact_after_write();
        result
    }

    /// Insert/update from an already-parsed `Value` — skips the `from_str` parse
    /// that [`put`](Self::put) does. Serializes once (for the WAL + payload store).
    /// The fast path for prepared inserts / programmatic writes.
    pub fn put_value(&mut self, slug: &str, payload: Value) -> Result<u64, serde_json::Error> {
        let raw = serde_json::to_string(&payload)?;
        self.wal_write(WalEntry::Put { slug: slug.to_string(), payload: raw.clone() });
        self.put_raw_inner(slug, raw.as_bytes(), payload)
    }


    /// True bulk insert for prepared/programmatic writes — the IoT ingest fast path.
    /// One shared timestamp, one batched WAL write + single fsync, one batched
    /// payload write, and — crucially — no per-row JSON splicing and no O(N)
    /// `contains` scan of collection membership (new keys are appended directly).
    /// Payloads are enriched via cheap Map inserts, not byte-rewrites.
    ///
    /// Fast path applies to index-free collections; if the collection has
    /// BM25/GIN/search indexes, per-row rebuild markers are set (rebuilt once at
    /// the end), matching `put_many` semantics.
    pub fn put_value_bulk(&mut self, rows: Vec<(String, Value)>) -> Result<usize, serde_json::Error> {
        if rows.is_empty() {
            return Ok(0);
        }
        let now = self.batch_now.unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let now_val: Value = now.into();

        let mut wal_entries: Vec<WalEntry> = Vec::with_capacity(rows.len());
        // (slug, hash, collection, spatial_meta, is_new)
        let mut metas: Vec<(String, u64, String, Option<geo::SpatialMeta>, bool)> =
            Vec::with_capacity(rows.len());

        for (slug, mut val) in rows {
            let hash = sk_hash(&slug);
            let is_new = match self.node_data(hash) {
                None => true,
                Some(e) => {
                    if e.slug != slug {
                        return Err(serde_json::Error::io(std::io::Error::new(
                            std::io::ErrorKind::AlreadyExists,
                            format!("hash collision: '{slug}' and existing '{}' both hash to {hash}", e.slug),
                        )));
                    }
                    false
                }
            };
            // Preserve the original _created_unix on update (matches put semantics).
            let old_created: Option<Value> = if is_new {
                None
            } else {
                self.payload_loc(hash)
                    .and_then(|(o, l)| self.payload_store.get(o, l))
                    .and_then(|v| v.get("_created_unix").cloned())
            };
            let coll = match val {
                Value::Object(ref mut m) => {
                    m.entry("_id".to_string()).or_insert_with(|| Value::String(slug.clone()));
                    let key = slug.split_once('/').map(|(_, k)| k).unwrap_or(&slug).to_string();
                    m.entry("_key".to_string()).or_insert_with(|| Value::String(key));
                    match old_created {
                        Some(c) => { m.insert("_created_unix".to_string(), c); }
                        None => { m.entry("_created_unix".to_string()).or_insert_with(|| now_val.clone()); }
                    }
                    m.insert("_updated_unix".to_string(), now_val.clone());
                    m.get("_collection").and_then(|v| v.as_str()).unwrap_or("").to_string()
                }
                _ => return Err(serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData, "payload must be a JSON object"))),
            };
            let spatial_meta = geo::extract_spatial_meta(&val);
            // Serialize once; the String is MOVED into the WAL entry (no clone) and
            // its bytes are borrowed for the payload append below.
            let s = serde_json::to_string(&val)?;
            wal_entries.push(WalEntry::Put { slug: slug.clone(), payload: s });
            metas.push((slug, hash, coll, spatial_meta, is_new));
        }

        // WAL first (durable), then payloads (payloads.bin is rebuilt from the WAL
        // on open, so this ordering is crash-safe). One batch each, one fsync.
        if let Some(wal) = &mut self.wal {
            wal.append_batch(&wal_entries).expect("sekejap: WAL batch append failed");
            if !self.defer_wal_sync {
                wal.sync().expect("sekejap: WAL fsync failed");
            }
        }
        // Payload bytes borrowed straight from the WAL entries — no extra copy.
        let refs: Vec<&[u8]> = wal_entries.iter().map(|e| match e {
            WalEntry::Put { payload, .. } => payload.as_bytes(),
            _ => &[][..],
        }).collect();
        let offsets = self.payload_store.append_batch(&refs);

        // Which index kinds need refreshing? Track touched collections only when a
        // search index exists (zero overhead for the common index-free IoT case).
        let have_bm25_gin = !self.bm25_indexes.is_empty() || !self.gin_indexes.is_empty();
        let has_any_search = self.schemas.values().any(|s| !s.indexes.search.is_empty());
        let mut colls_touched: HashSet<String> = HashSet::new();

        for (i, (slug, hash, coll, spatial_meta, is_new)) in metas.into_iter().enumerate() {
            let (offset, len) = offsets[i];
            if !coll.is_empty() {
                let coll_hash = sk_hash(&coll);
                let members = self.collections.entry(coll_hash).or_default();
                // New key ⇒ provably absent (collision-checked) ⇒ skip the O(N) scan.
                if is_new || !members.contains(&hash) {
                    members.push(hash);
                }
                self.collection_names_map.entry(coll_hash).or_insert_with(|| coll.clone());
                if has_any_search {
                    colls_touched.insert(coll.clone());
                }
            }
            self.slug_map.insert(slug.clone(), hash);
            self.nodes.insert(hash, NodeData {
                slug,
                collection: coll,
                spatial_meta: spatial_meta.clone(),
                payload_offset: offset,
                payload_len: len,
            });
            if let Some(grid) = &mut self.spatial_grid {
                grid.remove(hash);
                if let Some(m) = spatial_meta {
                    grid.insert(hash, m);
                }
            }
        }

        // Index freshness: mark dirty once; rebuild now unless a nesting caller
        // (InsertBatch / txn) deferred it. Covers BM25, GIN, and the positional
        // search index (matching put_raw_inner's per-row touch_search_index).
        if have_bm25_gin {
            let bm: Vec<String> = self.bm25_indexes.keys().cloned().collect();
            for f in bm { self.dirty_bm25.insert(f); }
            let gin: Vec<String> = self.gin_indexes.keys().cloned().collect();
            for f in gin { self.dirty_gin.insert(f); }
        }
        if has_any_search {
            for c in &colls_touched {
                if self.collection_has_search_index(c) {
                    self.dirty_search.insert(c.clone());
                }
            }
        }
        if (have_bm25_gin || has_any_search) && !self.defer_index_rebuild {
            self.flush_deferred_indexes();
        }

        // Only autocompact when standalone — a nesting caller (e.g. InsertBatch,
        // a transaction) that set defer_wal_sync finalizes/compacts itself.
        if !self.defer_wal_sync {
            self.autocompact_after_write();
        }
        Ok(wal_entries.len())
    }

    /// Execute several SQL statements as ONE group-commit: defer the per-statement
    /// WAL fsync and sync exactly once at the end. This is the concurrent-writer /
    /// IoT throughput lever — N buffered writes cost 1 fsync instead of N.
    ///
    /// Durability is unchanged in kind: the batch is the durability unit, exactly
    /// like a single statement (a crash before the final sync loses the whole
    /// un-synced batch — the same all-or-nothing guarantee, per-record CRC intact).
    /// Nesting-safe: the previous defer state is saved and restored, so an inner
    /// batch (e.g. a multi-row INSERT) never prematurely flushes an outer group.
    #[cfg(feature = "engine")]
    pub(crate) fn execute_batch_grouped(&mut self, stmts: &[String]) -> Result<usize, SqlError> {
        let prev = self.defer_wal_sync;
        self.defer_wal_sync = true;
        let mut total = 0usize;
        let mut result = Ok(());
        for s in stmts {
            match self.execute(s) {
                Ok(n) => total += n,
                Err(e) => { result = Err(e); break; }
            }
        }
        self.defer_wal_sync = prev;
        if !prev {
            self.wal_flush(); // single fsync for the whole group
        }
        result.map(|_| total)
    }

    /// Bulk edge insert — the edge counterpart of [`put_many`](Self::put_many).
    /// Defers the per-edge WAL fsync and flushes once at the end, turning an
    /// O(N) fsync storm into a single sync. Essential for graph bulk-load: on a
    /// disk DB, individual `link()` calls fsync each edge (correct for
    /// incremental durability, but ~ms/edge → minutes for tens of thousands).
    pub fn link_many<'a>(
        &mut self,
        edges: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>,
    ) {
        self.defer_wal_sync = true;
        for (from, to, edge_type) in edges {
            self.link(from, to, edge_type);
        }
        self.defer_wal_sync = false;
        self.wal_flush();
        self.autocompact_after_write();
    }

    /// Remove a node by slug. Also removes its collection membership and edges.
    pub fn remove(&mut self, slug: &str) {
        self.wal_write(WalEntry::Remove {
            slug: slug.to_string(),
        });
        self.remove_raw(slug);
        self.autocompact_after_write();
    }

    /// Create a directed edge: `from` → `to` with a type label. The edge is a
    /// naked connector — no weight. Nodes do not need to exist before linking.
    /// For a weighted or attributed edge use [`link_attr`] or [`link_meta`].
    pub fn link(&mut self, from: &str, to: &str, edge_type: &str) {
        self.wal_write(WalEntry::Link {
            from: from.to_string(),
            to: to.to_string(),
            edge_type: edge_type.to_string(),
        });
        self.link_raw(from, to, edge_type);
        self.autocompact_after_write();
    }

    /// Like `link` but attaches a JSON metadata object to the edge. Primitive
    /// attributes route to the fast lane; the rest ride the JSON bag.
    pub fn link_meta(
        &mut self,
        from: &str,
        to: &str,
        edge_type: &str,
        meta_json: &str,
    ) -> Result<(), serde_json::Error> {
        serde_json::from_str::<Value>(meta_json)?;
        self.wal_write(WalEntry::LinkMeta {
            from: from.to_string(),
            to: to.to_string(),
            edge_type: edge_type.to_string(),
            meta: meta_json.to_string(),
        });
        self.link_meta_raw(from, to, edge_type, meta_json)?;
        self.autocompact_after_write();
        Ok(())
    }

    /// Remove all directed edges from → to with the given type.
    pub fn unlink(&mut self, from: &str, to: &str, edge_type: &str) {
        self.wal_write(WalEntry::Unlink {
            from: from.to_string(),
            to: to.to_string(),
            edge_type: edge_type.to_string(),
        });
        self.unlink_raw(from, to, edge_type);
        self.autocompact_after_write();
    }

    // ── Persistence ───────────────────────────────────────────────────────────

    /// Compact the database: write a full snapshot then truncate the WAL.
    ///
    /// Returns the current WAL encoding format.
    pub fn wal_format(&self) -> WalFormat {
        self.wal_format
    }

    /// After compaction the WAL is empty and `snapshot.json` contains the
    /// complete current state. All previous WAL entries are discarded.
    ///
    /// In-memory (`CoreDB::new()`) databases silently ignore this call.
    pub fn compact(&mut self) -> io::Result<()> {
        let dir = match self.data_dir.clone() {
            Some(d) => d,
            None => return Ok(()),
        };

        // 1. Compact payload store: rebuild from live nodes only.
        // Must happen BEFORE build_snapshot() so the snapshot records the
        // new (post-compaction) offsets, not the pre-compaction ones.
        // Memory DB: rebuild Vec<u8> in-place.
        // Disk DB: streaming rewrite to payloads.bin.tmp then atomic rename.
        // Neither approach loads all payloads into RAM simultaneously.
        let node_keys: Vec<u64> = self.nodes.keys().copied().collect();
        if self.payload_store.is_disk() {
            // Disk-backed: stream each live node's bytes through a temp file.
            let pay_tmp  = dir.join("payloads.bin.tmp");
            let pay_path = dir.join("payloads.bin");
            let mut node_new_offsets: Vec<(u64, u64, u32)> = Vec::new(); // (hash, off, len)
            let mut write_cursor = 0u64;

            {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileExt;
                    let tmp_file = std::fs::OpenOptions::new()
                        .read(true).write(true).create(true).truncate(true)
                        .open(&pay_tmp)?;
                    for &h in &node_keys {
                        let (off, len) = match self.nodes.get(&h) {
                            Some(n) => (n.payload_offset, n.payload_len),
                            None => continue,
                        };
                        if let Some(bytes) = self.payload_store.get_raw(off, len) {
                            // get_raw decoded to JSON; re-encode under the current
                            // policy. SKBIN for records ≤ threshold (huge records
                            // stay raw to preserve head/tail extraction); field
                            // names intern into the append-only shared table.
                            let stored: Vec<u8> = if self.payload_store.binary
                                && bytes.len() <= PAYLOAD_COMPRESS_MAX
                            {
                                match serde_json::from_slice::<Value>(&bytes) {
                                    Ok(v) => storage::skbin::encode(
                                        &v, &mut self.payload_store.field_table),
                                    Err(_) => bytes, // not valid JSON → store raw
                                }
                            } else {
                                bytes // huge or non-binary → store raw JSON
                            };
                            tmp_file.write_all_at(&stored, write_cursor)?;
                            node_new_offsets.push((h, write_cursor, stored.len() as u32));
                            write_cursor += stored.len() as u64;
                        }
                    }
                    tmp_file.sync_all()?;
                }
                #[cfg(not(unix))]
                let _ = write_cursor; // non-unix fallback — no-op
            }
            // Apply the new offsets now that tmp_file is closed.
            for &(h, new_off, new_len) in &node_new_offsets {
                if let Some(node) = self.nodes.get_mut(&h) {
                    node.payload_offset = new_off;
                    node.payload_len    = new_len;
                }
            }
            // Persist the SKBIN field table DURABLY before swapping payloads.
            // IDs are append-only, so this (superset) table decodes both the old
            // and new records — a crash between this write and the rename can
            // never mis-decode anything.
            if self.payload_store.binary && !self.payload_store.field_table.is_empty() {
                let frame = self.payload_store.field_table.to_frame();
                for name in FIELD_TABLE_COPIES {
                    Self::write_atomic(&dir, name, &frame)?;
                }
            }
            // Atomically replace file, then reopen (preserving policy + table).
            std::fs::rename(&pay_tmp, &pay_path)?;
            let keep_binary = self.payload_store.binary;
            let keep_ft = std::mem::take(&mut self.payload_store.field_table);
            self.payload_store = PayloadStore::open_existing(&pay_path, write_cursor)?;
            self.payload_store.binary = keep_binary;
            self.payload_store.field_table = keep_ft;
        } else {
            // Memory DB: rebuild Vec<u8> without touching disk.
            let mut new_slab: Vec<u8> = Vec::new();
            for h in node_keys {
                if let Some(node) = self.nodes.get(&h) {
                    let old_off = node.payload_offset;
                    let old_len = node.payload_len;
                    if let Some(bytes) = self.payload_store.get_raw(old_off, old_len) {
                        let new_off = new_slab.len() as u64;
                        new_slab.extend_from_slice(&bytes);
                        if let Some(n) = self.nodes.get_mut(&h) {
                            n.payload_offset = new_off;
                            n.payload_len    = old_len;
                        }
                    }
                }
            }
            self.payload_store.reset(new_slab);
        }

        // 2. Compact disk-backed vector stores (reclaim dead space from
        //    overwrites and deletes).
        #[cfg(unix)]
        for store in self.vectors.values_mut() {
            store.compact()?;
        }

        // 3. Write snapshot atomically (tmp → rename) — AFTER payload compaction
        //    so disk-backed SnapNode offsets match the new payloads.bin layout.
        // Topology files FIRST — the v3 snapshot is a manifest that assumes they
        // exist. Crash between the two leaves the OLD snapshot + NEW topology
        // files, which reopens fine (old snapshot is still self-sufficient or
        // points at the previous, still-valid files; WAL not yet truncated).
        self.write_topology_files(&dir)?;

        let snap_json = serde_json::to_vec(&self.build_snapshot())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let snap_tmp = dir.join("snapshot.json.tmp");
        let snap_path = dir.join("snapshot.json");
        {
            let mut sf = std::fs::File::create(&snap_tmp)?;
            std::io::Write::write_all(&mut sf, &snapshot_header_bytes(SNAPSHOT_FORMAT_VERSION))?;
            std::io::Write::write_all(&mut sf, &snap_json)?;
            sf.sync_all()?;
        }
        std::fs::rename(&snap_tmp, &snap_path)?;

        // 3. Truncate WAL: close current writer → rename → open fresh → delete old
        self.wal = None;
        let wal_path = dir.join("wal.log");
        let wal_old = dir.join("wal.old");
        if wal_path.exists() {
            std::fs::rename(&wal_path, &wal_old)?;
        }
        let mut new_wal = WalWriter::open_with_format(&wal_path, self.wal_format)?;
        new_wal.set_sync_level(self.wal_sync_level);
        self.wal = Some(new_wal);
        if wal_old.exists() {
            std::fs::remove_file(&wal_old)?;
        }

        // Regenerate gin.bin so the next open loads GIN instantly.
        if let Some(ref gin_bin_path) = self.data_dir.as_ref().map(|d| d.join("gin.bin")) {
            let _ = self.save_gin_binary(gin_bin_path);
        }
        // Regenerate search.bin so the next open loads search indexes instantly.
        if let Some(ref search_bin_path) = self.data_dir.as_ref().map(|d| d.join("search.bin")) {
            let _ = self.save_search_binary(search_bin_path);
        }

        // Reclaim excess RAM capacity as part of compaction (so auto-compact also
        // trims memory automatically, not just disk).
        self.shrink_maps();

        Ok(())
    }

    /// Return excess CAPACITY of the in-RAM index maps to the allocator. Pure
    /// reclaim: no data and no index is dropped, so query results are unchanged and
    /// correctness is unaffected. Run automatically at the end of `compact()` and
    /// on demand via [`trim_memory`](Self::trim_memory).
    fn shrink_maps(&mut self) {
        self.nodes.shrink_to_fit();
        self.slug_map.shrink_to_fit();
        self.collections.shrink_to_fit();
        for members in self.collections.values_mut() {
            members.shrink_to_fit();
        }
        self.collection_names_map.shrink_to_fit();
        self.field_indexes.shrink_to_fit();
        for bt in self.field_indexes.values_mut() {
            for ids in bt.values_mut() {
                ids.shrink_to_fit();
            }
        }
    }

    /// Downstream-callable RAM reclaim WITHOUT a full compaction: hands excess
    /// capacity of the in-memory index maps back to the allocator (e.g. after a
    /// large batch of deletes/updates left the maps over-allocated). Cheap and
    /// SAFE — never drops data or indexes, so query results are unchanged.
    ///
    /// For deeper reclaim (payload/vector dead space + WAL truncation), call
    /// [`compact`](Self::compact) — which also shrinks the maps. Disk-backed
    /// payloads/vectors are mmap'd, so their cold pages are already reclaimed by
    /// the OS under memory pressure.
    pub fn trim_memory(&mut self) {
        self.shrink_maps();
    }

    /// Phase 0: write the offset-addressable topology files from the live graph.
    /// Builds `TopoNode`/`TopoEdge` from `self.nodes` + `self.edges` (dense ids
    /// assigned in hash order for deterministic output), then writes the five files
    /// atomically. Not yet read by `open()` — see `docs/internals/topology-format-v2.md`.
    fn write_topology_files(&self, dir: &Path) -> io::Result<()> {
        use storage::topology::{self, TopoEdge, TopoNode};

        // Nodes — sorted by hash so dense-id assignment is deterministic.
        let mut topo_nodes: Vec<TopoNode> = self
            .nodes
            .iter()
            .map(|(&h, n)| TopoNode {
                hash: h,
                slug: n.slug.clone(),
                collection: n.collection.clone(),
                payload_offset: n.payload_offset,
                payload_len: n.payload_len,
                spatial: n.spatial_meta.as_ref().map(|m| [
                    m.centroid_lat, m.centroid_lon,
                    m.bbox_min_lat, m.bbox_min_lon,
                    m.bbox_max_lat, m.bbox_max_lon,
                ]),
            })
            .collect();
        topo_nodes.sort_by_key(|n| n.hash);

        // Edges — forward adjacency; skip any dangling endpoints.
        let mut topo_edges: Vec<TopoEdge> = Vec::new();
        for (&from_h, edge_list) in self.edges.iter_fwd() {
            if !self.nodes.contains_key(&from_h) {
                continue;
            }
            for e in edge_list {
                if !self.nodes.contains_key(&e.other) {
                    continue;
                }
                let edge_type = self
                    .edges
                    .type_name(e.edge_type)
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("{:016x}", e.edge_type));
                topo_edges.push(TopoEdge {
                    from_hash: from_h,
                    to_hash: e.other,
                    edge_type,
                    // Fold fast-lane columns back into the JSON meta so they ride
                    // edgemeta.bin. On load the same routing re-splits primitives
                    // into columns — so columns survive compaction without a
                    // topology-format change. (Disk-columnar is a later optimisation.)
                    meta: self.edge_all_attrs(e).map(|v| v.to_string()),
                });
            }
        }

        let blob = topology::build(&topo_nodes, &topo_edges);
        Self::write_atomic(dir, "nodes.bin", &blob.nodes)?;
        Self::write_atomic(dir, "adj_fwd.bin", &blob.fwd)?;
        Self::write_atomic(dir, "adj_rev.bin", &blob.rev)?;
        Self::write_atomic(dir, "idx.bin", &blob.idx)?;
        Self::write_atomic(dir, "slugs.bin", &blob.slugs)?;
        Self::write_atomic(dir, "dict.bin", &blob.dict)?;
        Self::write_atomic(dir, "spatial.bin", &blob.spat)?;
        Self::write_atomic(dir, "edgemeta.bin", &blob.emeta)?;
        Self::write_atomic(dir, "collections.bin", &blob.colls)?;
        Ok(())
    }

    /// Phase 0 read path: rebuild the resident graph (nodes + edges + collections)
    /// from the topology files written by `write_topology_files`. Used at `open()`
    /// when the snapshot is missing/corrupt (recovery). Schemas / vectors / HNSW are
    /// not stored in topology files and are not recovered here; GIN/search load from
    /// their own sidecars as usual.
    fn load_topology_files(&mut self, dir: &Path) -> io::Result<()> {
        use storage::topology::TopologyBlob;
        let rd = |name: &str| std::fs::read(dir.join(name));
        let blob = TopologyBlob {
            nodes: rd("nodes.bin")?,
            fwd: rd("adj_fwd.bin")?,
            rev: rd("adj_rev.bin")?,
            idx: rd("idx.bin")?,
            slugs: rd("slugs.bin")?,
            dict: rd("dict.bin")?,
            // Older Phase-0 dirs may lack these — recovery tolerates their absence
            // (collections are derived from node records; metadata is then empty).
            spat: rd("spatial.bin").unwrap_or_default(),
            emeta: rd("edgemeta.bin").unwrap_or_default(),
            colls: rd("collections.bin").unwrap_or_default(),
        };
        self.load_topology_blob(&blob)
    }

    /// Rebuild the resident graph from an in-memory set of topology file bytes.
    /// Used by local open (manifest snapshots + recovery) and the S3 open path.
    fn load_topology_blob(&mut self, blob: &storage::topology::TopologyBlob) -> io::Result<()> {
        use storage::topology::TopologyView;
        let view = TopologyView::new(blob)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        // Nodes: identity + payload location from the records; spatial metadata is
        // re-derived from the payload (recovery-only cost, not a hot path).
        for id in 0..view.node_count() as u64 {
            let (Some(slug), Some(rec)) = (view.slug(id), view.node_record(id)) else {
                continue;
            };
            let hash = sk_hash(slug);
            let coll = view
                .collection_name(rec.collection_id)
                .unwrap_or("")
                .to_string();
            // Side-table first (48-byte read); payload parse only for legacy dirs
            // written before spatial.bin existed.
            let spatial_meta = storage::topology::spatial_at(&blob.spat, rec.spatial_ref)
                .map(|v| geo::SpatialMeta {
                    centroid_lat: v[0], centroid_lon: v[1],
                    bbox_min_lat: v[2], bbox_min_lon: v[3],
                    bbox_max_lat: v[4], bbox_max_lon: v[5],
                })
                .or_else(|| {
                    if !blob.spat.is_empty() {
                        return None; // side-table present: NO_ID really means no geometry
                    }
                    self.payload_store
                        .get_raw(rec.payload_offset, rec.payload_len)
                        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                        .and_then(|p| geo::extract_spatial_meta(&p))
                });
            self.nodes.insert(hash, NodeData {
                slug: slug.to_string(),
                collection: coll.clone(),
                spatial_meta,
                payload_offset: rec.payload_offset,
                payload_len: rec.payload_len,
            });
            if !coll.is_empty() {
                let coll_hash = sk_hash(&coll);
                self.collections.entry(coll_hash).or_default().push(hash);
                self.collection_names_map
                    .entry(coll_hash)
                    .or_insert_with(|| coll.clone());
            }
        }

        // Edges: forward adjacency only — link_raw rebuilds both directions.
        // Edge metadata (if the blob is present) is restored via link_meta_raw.
        for id in 0..view.node_count() as u64 {
            let Some(from_slug) = view.slug(id) else { continue };
            let from_slug = from_slug.to_string();
            for e in view.fwd_edges(id) {
                let (Some(to_slug), Some(ty)) =
                    (view.slug(e.neighbor), view.edge_type_name(e.edge_type_id))
                else {
                    continue;
                };
                let (to_slug, ty) = (to_slug.to_string(), ty.to_string());
                let meta_json = storage::topology::emeta_bytes_at(&blob.emeta, e.meta_ref)
                    .and_then(|b| std::str::from_utf8(b).ok().map(|s| s.to_string()));
                match meta_json {
                    Some(m) => {
                        let _ = self.link_meta_raw(&from_slug, &to_slug, &ty, &m);
                    }
                    None => self.link_raw(&from_slug, &to_slug, &ty),
                }
            }
        }
        Ok(())
    }

    /// Write `bytes` to `dir/name` durably: tmp file → fsync → atomic rename.
    fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
        let path = dir.join(name);
        let tmp = dir.join(format!("{name}.tmp"));
        {
            let mut f = std::fs::File::create(&tmp)?;
            std::io::Write::write_all(&mut f, bytes)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Save all current GIN indexes to a compact binary sidecar `gin.bin`.
    ///
    /// The file format uses RoaringBitmap's native binary serialization, which
    /// is ~10-50× smaller and faster to load than JSON integer arrays.
    /// Called automatically after GIN is rebuilt so future opens skip the rebuild.
    fn save_gin_binary(&self, path: &Path) -> io::Result<()> {
        use std::io::Write;
        let tmp = path.with_extension("bin.tmp");
        let mut f = std::io::BufWriter::new(
            std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?
        );
        // Magic header
        f.write_all(b"SKGIN001")?;
        // Number of GIN indexes
        f.write_all(&(self.gin_indexes.len() as u32).to_le_bytes())?;
        for gin in self.gin_indexes.values() {
            gin.write_binary(&mut f, GIN_INDEX_VERSION)?;
        }
        f.flush()?;
        f.get_ref().sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Load GIN indexes from the binary sidecar `gin.bin`.
    ///
    /// Returns `true` if the file was successfully loaded (all indexes had
    /// matching versions), `false` if missing, corrupt, or version-mismatched
    /// (caller should then call `rebuild_declared_gin_indexes` + `save_gin_binary`).
    fn load_gin_binary(&mut self, path: &Path) -> bool {
        use std::io::Read;
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(_) => return false,
        };
        if data.len() < 12 || &data[..8] != b"SKGIN001" {
            return false;
        }
        let mut cursor = std::io::Cursor::new(&data[8..]);
        let mut count_buf = [0u8; 4];
        if cursor.read_exact(&mut count_buf).is_err() { return false; }
        let count = u32::from_le_bytes(count_buf) as usize;
        let mut loaded = HashMap::new();
        for _ in 0..count {
            match GINIndex::read_binary(&mut cursor, GIN_INDEX_VERSION) {
                Ok((field, idx)) => { loaded.insert(field, idx); }
                Err(_) => return false,
            }
        }
        // Only accept if all declared fields are present
        let declared_ok = self.schemas.values()
            .flat_map(|s| s.indexes.fulltext.iter())
            .all(|f| loaded.contains_key(f));
        if !declared_ok {
            return false;
        }
        for (field, idx) in loaded {
            self.record_index_version("gin", &field, GIN_INDEX_VERSION);
            self.gin_indexes.insert(field, idx);
        }
        true
    }

    /// Force WAL data to reach disk (fsync).
    /// By default writes are flushed to the OS buffer but not fsynced.
    /// Call this after a critical batch of writes if you need guaranteed
    /// on-disk durability before the OS flushes on its own schedule.
    pub fn sync(&mut self) -> io::Result<()> {
        if let Some(wal) = &mut self.wal {
            wal.sync()?;
        }
        Ok(())
    }

    // ── Snapshot helpers ──────────────────────────────────────────────────────

    fn build_snapshot(&self) -> Snapshot {
        let is_disk = self.payload_store.is_disk();
        let nodes: Vec<SnapNode> = if is_disk {
            // Disk-backed → MANIFEST snapshot (v3): nodes + edges live in the
            // topology files written alongside (`write_topology_files`, always
            // called before the snapshot at compact). The snapshot carries only
            // schemas / vectors / HNSW / btree metadata.
            Vec::new()
        } else {
            self.nodes
                .values()
                .filter_map(|n| {
                    self.payload_store
                        .get(n.payload_offset, n.payload_len)
                        .map(|payload| SnapNode {
                            slug: n.slug.clone(),
                            payload: Some(payload),
                            payload_offset: None,
                            payload_len:    None,
                            collection:     None,
                            spatial_meta:   None,
                        })
                })
                .collect()
        };

        let mut edges: Vec<SnapEdge> = Vec::new();
        for (&from_h, edge_list) in self.edges.iter_fwd() {
            if is_disk {
                break; // manifest: edges live in the topology files
            }
            let from_slug = match self.nodes.get(&from_h) {
                Some(n) => n.slug.clone(),
                None => continue, // dangling edge, skip
            };
            for e in edge_list {
                let to_slug = match self.nodes.get(&e.other) {
                    Some(n) => n.slug.clone(),
                    None => continue,
                };
                let edge_type = self
                    .edges
                    .type_name(e.edge_type)
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("{:016x}", e.edge_type));
                edges.push(SnapEdge {
                    from: from_slug.clone(),
                    to: to_slug,
                    edge_type,
                    meta: self.edge_all_attrs(e),
                });
            }
        }

        // Collect vectors — only for hashes that still resolve to a live node.
        // This auto-prunes any orphan entries left by bugs or direct HashMap
        // manipulation; main data is always the authority.
        //
        // When all vector stores are disk-backed, vectors live in
        // `vectors_{field}.bin` files — skip serialising them into JSON
        // (shrinks the snapshot from ~100 MB to ~10 MB for typical datasets).
        let has_vector_files = !self.vectors.is_empty()
            && self.vectors.values().all(|s| s.is_disk());
        let mut snap_vectors: Vec<SnapVector> = Vec::new();
        if !has_vector_files {
            for (field, field_vecs) in &self.vectors {
                for (hash, data) in field_vecs.iter() {
                    if let Some(node) = self.nodes.get(&hash) {
                        snap_vectors.push(SnapVector {
                            slug: node.slug.clone(),
                            field: field.clone(),
                            data: data.to_vec(),
                        });
                    }
                }
            }
        }

        let snap_hnsw: Vec<SnapHnsw> = self
            .hnsw_indexes
            .iter()
            .map(|(field, graph)| {
                let (m, ef) = self.hnsw_params.get(field).copied().unwrap_or((16, 200));
                SnapHnsw {
                    field: field.clone(),
                    version: HNSW_INDEX_VERSION,
                    m,
                    ef_construction: ef,
                    graph: graph.clone(),
                }
            })
            .collect();

        // Persist btree indexes for disk-backed snapshots (avoids re-scan on reload).
        let snap_btree: Option<Vec<SnapBtree>> = if is_disk && !self.field_indexes.is_empty() {
            Some(self.field_indexes.iter().map(|((coll_hash, field), btree)| {
                SnapBtree {
                    collection_hash: *coll_hash,
                    field: field.clone(),
                    entries: btree.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                }
            }).collect())
        } else {
            None
        };

        Snapshot {
            version: SNAPSHOT_FORMAT_VERSION,
            is_disk_backed: is_disk,
            topology_in_files: is_disk,
            has_vector_files,
            nodes,
            edges,
            schemas: Some(self.schemas.values().cloned().collect()),
            vectors: if snap_vectors.is_empty() { None } else { Some(snap_vectors) },
            hnsw_indexes: if snap_hnsw.is_empty() { None } else { Some(snap_hnsw) },
            btree_indexes: snap_btree,
            gin_indexes: Ignored,
        }
    }

    fn load_snapshot(&mut self, snap: Snapshot) {
        self.load_snapshot_parts(snap, true)
    }

    /// Load a snapshot, optionally skipping nodes + edges (`load_topology =
    /// false`) — used by paged mode, where topology is served from the mmap'd
    /// files and the snapshot only supplies schemas / vectors / HNSW / btrees.
    fn load_snapshot_parts(&mut self, snap: Snapshot, load_topology: bool) {
        let nodes = if load_topology { snap.nodes } else { Vec::new() };
        let edges = if load_topology { snap.edges } else { Vec::new() };
        for n in nodes {
            if snap.is_disk_backed {
                // Disk-backed: restore NodeData from metadata; payload bytes are
                // already in payloads.bin at the stored offset.
                if let (Some(offset), Some(len)) = (n.payload_offset, n.payload_len) {
                    let hash = sk_hash(&n.slug);
                    let coll = n.collection.clone().unwrap_or_default();
                    let coll_hash = if coll.is_empty() { 0 } else { sk_hash(&coll) };
                    self.nodes.insert(hash, NodeData {
                        slug:           n.slug.clone(),
                        collection:     coll.clone(),
                        spatial_meta:   n.spatial_meta,
                        payload_offset: offset,
                        payload_len:    len,
                    });
                    if !coll.is_empty() {
                        self.collections.entry(coll_hash).or_default().push(hash);
                        self.collection_names_map.entry(coll_hash)
                            .or_insert_with(|| coll.clone());
                    }
                }
            } else if let Some(payload) = n.payload {
                let _ = self.put_raw(&n.slug, &payload.to_string());
            }
        }
        for e in edges {
            if let Some(meta) = e.meta {
                let _ =
                    self.link_meta_raw(&e.from, &e.to, &e.edge_type, &meta.to_string());
            } else {
                self.link_raw(&e.from, &e.to, &e.edge_type);
            }
        }
        if let Some(schemas) = snap.schemas {
            for schema in schemas {
                self.schemas.insert(schema.collection.clone(), schema);
            }
        }
        // Restore vector index from snapshot — WAL replay will add anything
        // written after the snapshot was taken.
        // When has_vector_files is set, vectors live in .bin files — skip JSON
        // loading (they'll be opened as disk stores before WAL replay).
        if !snap.has_vector_files {
            if let Some(vecs) = snap.vectors {
                for sv in vecs {
                    let hash = sk_hash(&sv.slug);
                    self.vectors.entry(sv.field).or_default().put(hash, sv.data);
                }
            }
        }
        // Restore HNSW graphs — rebuild if the stored version doesn't match.
        if let Some(hnsw_list) = snap.hnsw_indexes {
            for sh in hnsw_list {
                if sh.version == HNSW_INDEX_VERSION {
                    self.hnsw_params.insert(sh.field.clone(), (sh.m, sh.ef_construction));
                    self.hnsw_indexes.insert(sh.field, sh.graph);
                } else {
                    // Version mismatch — rebuild from stored vectors.
                    let _ = self.build_hnsw_index(&sh.field, sh.m, sh.ef_construction);
                }
            }
        }
        // Restore persisted btree indexes (disk-backed snapshots only).
        // This avoids re-scanning payloads.bin to rebuild them.
        let has_snap_btree = snap.btree_indexes.is_some();
        if let Some(btrees) = snap.btree_indexes {
            for sb in btrees {
                let btmap: std::collections::BTreeMap<FieldKey, Vec<u64>> =
                    sb.entries.into_iter().collect();
                self.field_indexes.insert((sb.collection_hash, sb.field), btmap);
            }
        }

        // Rebuild btree field indexes — only when stored version mismatches,
        // or when no btree snapshot was present (legacy snapshot or new index).
        let btree_rebuild: Vec<(String, String)> = self
            .schemas
            .values()
            .flat_map(|s| s.indexes.range.iter().map(|f| {
                let v = s.indexes.build_versions.get(&format!("btree:{f}")).copied().unwrap_or(0);
                (s.collection.clone(), f.clone(), v)
            }))
            .filter(|(c, f, v)| {
                if has_snap_btree {
                    // Already restored from snapshot — only rebuild on version mismatch
                    *v != BTREE_INDEX_VERSION
                } else {
                    // No btree snapshot — rebuild everything
                    let _ = (c, f);
                    true
                }
            })
            .map(|(c, f, _)| (c, f))
            .collect();
        for (coll, field) in btree_rebuild {
            self.build_field_index(&coll, &field);
        }

        // Rebuild BM25 indexes — only when stored version mismatches.
        let bm25_rebuild: Vec<String> = {
            let mut seen = std::collections::HashSet::new();
            self.schemas.values()
                .flat_map(|s| s.indexes.bm25.iter().filter(|f| {
                    s.indexes.build_versions.get(&format!("bm25:{f}")).copied().unwrap_or(0)
                        != BM25_INDEX_VERSION
                }).cloned())
                .filter(|f| seen.insert(f.clone()))
                .collect()
        };
        for field in bm25_rebuild { self.build_bm25_index(&field); }

        // Rebuild GIN indexes — only when stored version mismatches.
        let gin_rebuild: Vec<String> = {
            let mut seen = std::collections::HashSet::new();
            self.schemas.values()
                .flat_map(|s| s.indexes.fulltext.iter().filter(|f| {
                    s.indexes.build_versions.get(&format!("gin:{f}")).copied().unwrap_or(0)
                        != GIN_INDEX_VERSION
                }).cloned())
                .filter(|f| seen.insert(f.clone()))
                .collect()
        };
        for field in gin_rebuild { self.build_gin_index(&field); }
    }

    // ── Reads ─────────────────────────────────────────────────────────────────

    /// Get raw JSON payload for a slug. Returns `None` if not found.
    pub fn get(&self, slug: &str) -> Option<String> {
        let (off, len) = self.payload_loc(sk_hash(slug))?;
        self.payload_store
            .get_raw(off, len)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    /// Where a node's payload lives in the payload store — overlay first, then
    /// the mapped base. Lean (no string materialization): this sits on the
    /// payload-read hot path.
    pub(crate) fn payload_loc(&self, hash: u64) -> Option<(u64, u32)> {
        if let Some(n) = self.nodes.get(&hash) {
            return Some((n.payload_offset, n.payload_len));
        }
        let base = self.topo_base.as_ref()?;
        let rec = base.node_record(base.resolve(hash)?)?;
        Some((rec.payload_offset, rec.payload_len))
    }

    /// Parse and return the JSON payload for a node hash. Returns `None` if
    /// the node does not exist or the payload cannot be parsed.
    pub(crate) fn get_payload(&self, hash: u64) -> Option<Value> {
        let (off, len) = self.payload_loc(hash)?;
        self.payload_store.get(off, len)
    }

    /// Extract ONE field from a stored (undecoded) payload record, dispatching on
    /// its encoding: SKBIN → `get_field` skip-scan (no full decode); raw JSON →
    /// byte-search. The filter fast paths read stored bytes in batches, so this
    /// keeps single-field filters fast on SKBIN without materialising the record.
    pub(crate) fn extract_stored_field(&self, stored: &[u8], field: &str) -> Option<Value> {
        if storage::skbin::is_skbin(stored) {
            storage::skbin::get_field(stored, field, &self.payload_store.field_table)
        } else if stored.first() == Some(&PAYLOAD_TAG_ZSTD) {
            // Retired whole-record zstd (0x01): decode_payload_record returns None,
            // so this yields nothing rather than byte-searching compressed bytes.
            let raw = decode_payload_record(stored.to_vec())?;
            let fq = [field.to_string()];
            query::extract_fields_by_search(&raw, &fq).remove(field)
        } else {
            let fq = [field.to_string()];
            query::extract_fields_by_search(stored, &fq).remove(field)
        }
    }

    /// Extract SEVERAL fields from a stored (undecoded) FULL-record payload,
    /// dispatching on encoding: SKBIN → per-field skip-scan (no full materialise);
    /// raw JSON → byte-search. Returned map is keyed by raw field name (same shape
    /// as `extract_fields_by_search`). `stored` MUST be a whole record (starts at
    /// the frame header) — do not pass a tail slice.
    pub(crate) fn extract_stored_fields(
        &self,
        stored: &[u8],
        fields: &[String],
    ) -> serde_json::Map<String, Value> {
        if storage::skbin::is_skbin(stored) {
            let mut m = serde_json::Map::new();
            for f in fields {
                if let Some(v) =
                    storage::skbin::get_field(stored, f, &self.payload_store.field_table)
                {
                    m.insert(f.clone(), v);
                }
            }
            m
        } else if stored.first() == Some(&PAYLOAD_TAG_ZSTD) {
            // Retired whole-record zstd (0x01): can't decode → yield no fields
            // (never byte-search the compressed bytes).
            match decode_payload_record(stored.to_vec()) {
                Some(raw) => query::extract_fields_by_search(&raw, fields),
                None => serde_json::Map::new(),
            }
        } else {
            query::extract_fields_by_search(stored, fields)
        }
    }

    /// If node `hash`'s stored payload is a SKBIN record, extract `fields` from it
    /// (reading the full record — SKBIN records are always ≤ 64 KB). Returns
    /// `None` for non-SKBIN records so callers keep their raw-JSON tail-slice fast
    /// path for large geometry blobs.
    pub(crate) fn try_skbin_node_fields(
        &self,
        hash: u64,
        fields: &[String],
    ) -> Option<serde_json::Map<String, Value>> {
        let (off, len) = self.payload_loc(hash)?;
        // Cheap SKBIN probe: check the first byte before reading the whole record.
        if self.payload_store.get_raw_at(off, 1)?.first() != Some(&storage::skbin::TAG_SKBIN) {
            return None;
        }
        let full = self.payload_store.get_raw_at(off, len as usize)?;
        let mut m = serde_json::Map::new();
        for f in fields {
            if let Some(v) = storage::skbin::get_field(&full, f, &self.payload_store.field_table) {
                m.insert(f.clone(), v);
            }
        }
        Some(m)
    }


    /// For large payloads, read just a head slice and a tail slice to extract fields
    /// without loading the full payload (e.g. avoids reading a 12 MB geometry blob).
    pub(crate) fn get_payload_head_tail(
        &self,
        hash: u64,
        head_bytes: usize,
        tail_bytes: usize,
    ) -> Option<(Vec<u8>, Vec<u8>)> {
        let (p_off, p_len) = self.payload_loc(hash)?;
        let len = p_len as usize;
        let off = p_off;
        let head_size = head_bytes.min(len);
        let tail_size = tail_bytes.min(len);
        // If the ranges overlap (small payload), just read the full thing once.
        if head_size + tail_size >= len {
            let full = self.payload_store.get_raw(off, len as u32)?;
            return Some((full.clone(), full));
        }
        let head = self.payload_store.get_raw_at(off, head_size)?;
        let tail_off = off + (len - tail_size) as u64;
        let tail = self.payload_store.get_raw_at(tail_off, tail_size)?;
        Some((head, tail))
    }

    /// Zero-copy tail slice for a single node (mmap path only).
    #[cfg(unix)]
    pub(crate) fn payload_tail_slice(&self, hash: u64, tail_bytes: usize) -> Option<&[u8]> {
        let (p_off, p_len) = self.payload_loc(hash)?;
        let len = p_len as usize;
        if len <= tail_bytes {
            self.payload_store.get_slice(p_off, len)
        } else {
            let tail_off = p_off + (len - tail_bytes) as u64;
            self.payload_store.get_slice(tail_off, tail_bytes)
        }
    }

    /// Read raw JSON bytes for multiple nodes with minimal I/O syscalls.
    ///
    /// Sorts hashes by `payload_offset`, groups nodes whose payloads are
    /// close together (gap ≤ `MAX_GAP`) into one batch, and issues a single
    /// `pread` per batch instead of one syscall per node.
    ///
    /// For sequentially-inserted data (the common case), all payloads in a
    /// collection are contiguous in `payloads.bin`, so the entire collection
    /// can be read in **one** syscall rather than O(N).
    ///
    /// Returns a `HashMap<u64, Vec<u8>>` of raw JSON bytes keyed by node hash.
    pub(crate) fn read_raw_payloads_batched(&self, hashes: &[u64]) -> HashMap<u64, Vec<u8>> {
        /// Bridge gaps between payload regions up to this many bytes.
        const MAX_GAP: u64 = 16 * 1024;
        /// Cap each batch read at 32 MB to keep peak RAM bounded.
        const MAX_BATCH: usize = 32 * 1024 * 1024;

        // Sort candidates by payload_offset for sequential I/O.
        // Resolve via payload_loc (not self.nodes) so paged-topology opens — where
        // nodes live in the mmap base, not the resident map — still batch-read.
        let mut sorted: Vec<(u64, u64, u32)> = hashes
            .iter()
            .filter_map(|&h| self.payload_loc(h).map(|(off, len)| (h, off, len)))
            .collect();
        sorted.sort_unstable_by_key(|&(_, off, _)| off);

        let mut result = HashMap::with_capacity(hashes.len());
        let mut i = 0;

        while i < sorted.len() {
            let batch_off = sorted[i].1;
            let mut j = i + 1;
            let mut batch_end = sorted[i].1 + sorted[i].2 as u64;

            // Extend batch while gap and size constraints hold.
            while j < sorted.len() {
                let (_, next_off, next_len) = sorted[j];
                if next_off.saturating_sub(batch_end) > MAX_GAP {
                    break;
                }
                let cand_end = next_off + next_len as u64;
                if (cand_end.saturating_sub(batch_off)) as usize > MAX_BATCH {
                    break;
                }
                batch_end = batch_end.max(cand_end);
                j += 1;
            }

            // One read for the entire contiguous region.
            let batch_len = (batch_end - batch_off) as usize;
            if let Some(buf) = self.payload_store.get_raw_at(batch_off, batch_len) {
                for &(hash, off, len) in &sorted[i..j] {
                    let start = (off - batch_off) as usize;
                    let end = start + len as usize;
                    if end <= buf.len() {
                        result.insert(hash, buf[start..end].to_vec());
                    }
                }
            } else {
                // Fallback: read each node individually on I/O error.
                for &(hash, off, len) in &sorted[i..j] {
                    if let Some(raw) = self.payload_store.get_raw(off, len) {
                        result.insert(hash, raw);
                    }
                }
            }
            i = j;
        }

        result
    }

    /// Check if a node exists.
    pub fn contains(&self, slug: &str) -> bool {
        let h = sk_hash(slug);
        self.nodes.contains_key(&h)
            || self.topo_base.as_ref().map_or(false, |b| b.resolve(h).is_some())
    }

    /// Total number of nodes.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Every node's slug (resident overlay ∪ mmap base). Order is unspecified.
    /// Used by tooling (e.g. `sekejap migrate`) to iterate + verify all records.
    pub fn all_slugs(&self) -> Vec<String> {
        self.all_hashes()
            .into_iter()
            .filter_map(|h| self.node_data(h).map(|n| n.slug.clone()))
            .collect()
    }

    /// Returns the number of directed edges currently stored.
    pub fn edge_count(&self) -> usize {
        self.edges.edge_count()
    }

    /// Returns all distinct collection names present in the graph, sorted.
    ///
    /// Includes collections that have nodes but no explicit `CREATE TABLE` schema.
    pub fn collection_names(&self) -> Vec<String> {
        let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for node in self.nodes.values() {
            if !node.collection.is_empty() {
                names.insert(node.collection.clone());
            }
        }
        names.into_iter().collect()
    }

    /// Returns a `CREATE TABLE` DDL string for a collection if a schema was declared.
    /// Returns `None` if no `CREATE TABLE` was issued for that collection.
    pub fn schema_ddl(&self, collection: &str) -> Option<String> {
        let schema = self.schemas.get(collection)?;
        let mut ddl = format!("CREATE TABLE {} (", schema.collection);
        let parts: Vec<String> = schema.fields.iter().map(|f| {
            let ty = match f.ty {
                sql::FieldType::Text        => "TEXT",
                sql::FieldType::Integer     => "INTEGER",
                sql::FieldType::Real        => "REAL",
                sql::FieldType::Bool        => "BOOLEAN",
                sql::FieldType::Timestamptz => "TIMESTAMPTZ",
                sql::FieldType::Geo         => "GEO",
                sql::FieldType::Vector      => "VECTOR",
                sql::FieldType::Json        => "JSON",
            };
            if f.is_primary_key {
                format!("{} {} PRIMARY KEY", f.name, ty)
            } else {
                format!("{} {}", f.name, ty)
            }
        }).collect();
        ddl.push_str(&parts.join(", "));
        ddl.push(')');
        Some(ddl)
    }

    /// Return the structured schema for a collection, if one was declared via
    /// `CREATE TABLE`.  Returns `None` for schemaless collections.
    pub fn table_schema(&self, collection: &str) -> Option<&TableSchema> {
        self.schemas.get(collection)
    }

    /// Get all outgoing edges from a node, resolved to slugs where available.
    pub fn edges_from(&self, slug: &str) -> Vec<EdgeHit> {
        let hash = sk_hash(slug);
        self.edges
            .fwd_edges(hash)
            .map(|edges| {
                edges
                    .iter()
                    .map(|e| EdgeHit {
                        from_slug: Some(slug.to_string()),
                        to_slug: self.nodes.get(&e.other).map(|n| n.slug.clone()),
                        edge_type: self.edges.type_name(e.edge_type).map(|s| s.to_string()),
                        edge_type_hash: e.edge_type,
                        meta: self.edge_all_attrs(e),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get all incoming edges to a node, resolved to slugs where available.
    pub fn edges_to(&self, slug: &str) -> Vec<EdgeHit> {
        let hash = sk_hash(slug);
        self.edges
            .rev_edges(hash)
            .map(|edges| {
                edges
                    .iter()
                    .map(|e| EdgeHit {
                        from_slug: self.nodes.get(&e.other).map(|n| n.slug.clone()),
                        to_slug: Some(slug.to_string()),
                        edge_type: self.edges.type_name(e.edge_type).map(|s| s.to_string()),
                        edge_type_hash: e.edge_type,
                        meta: self.edge_all_attrs(e),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// List all outgoing edges from every node in `from_collection`.
    pub fn edges_from_collection(&self, from_collection: &str) -> Vec<EdgeHit> {
        let col_h = sk_hash(from_collection);
        let mut result = Vec::new();
        for (&node_h, node) in &self.nodes {
            if node.collection.is_empty() || sk_hash(&node.collection) != col_h { continue; }
            if let Some(edges) = self.edges.fwd_edges(node_h) {
                for e in edges {
                    result.push(EdgeHit {
                        from_slug: Some(node.slug.clone()),
                        to_slug: self.nodes.get(&e.other).map(|n| n.slug.clone()),
                        edge_type: self.edges.type_name(e.edge_type).map(|s| s.to_string()),
                        edge_type_hash: e.edge_type,
                        meta: self.edge_all_attrs(e),
                    });
                }
            }
        }
        result
    }

    /// List edges that go from nodes in `from_collection` to nodes in `to_collection`.
    pub fn edges_between(&self, from_collection: &str, to_collection: &str) -> Vec<EdgeHit> {
        let to_col_h = sk_hash(to_collection);
        self.edges_from_collection(from_collection)
            .into_iter()
            .filter(|e| {
                e.to_slug.as_deref()
                    .and_then(|s| self.slug_map.get(s))
                    .and_then(|h| self.nodes.get(h))
                    .map(|n| !n.collection.is_empty() && sk_hash(&n.collection) == to_col_h)
                    .unwrap_or(false)
            })
            .collect()
    }

    /// Distinct edge type labels on outgoing edges from a single node.
    ///
    /// ```
    /// # use sekejap::CoreDB;
    /// # let mut db = CoreDB::new();
    /// # db.put("cls/math", r#"{"_collection":"classrooms"}"#).unwrap();
    /// # db.put("lec/ali",  r#"{"_collection":"lecturers"}"#).unwrap();
    /// # db.link("cls/math", "lec/ali", "taught_by");
    /// let types = db.edge_types_from("cls/math");
    /// assert_eq!(types, vec!["taught_by"]);
    /// ```
    pub fn edge_types_from(&self, slug: &str) -> Vec<String> {
        let hash = sk_hash(slug);
        let mut seen = std::collections::HashSet::new();
        let mut types = Vec::new();
        if let Some(edges) = self.edges.fwd_edges(hash) {
            for e in edges {
                if let Some(label) = self.edges.type_name(e.edge_type) {
                    if seen.insert(e.edge_type) {
                        types.push(label.to_string());
                    }
                }
            }
        }
        types.sort();
        types
    }

    /// Distinct edge type labels on outgoing edges from any node in a collection.
    ///
    /// ```
    /// # use sekejap::CoreDB;
    /// # let mut db = CoreDB::new();
    /// # db.put("cls/math", r#"{"_collection":"classrooms"}"#).unwrap();
    /// # db.put("lec/ali",  r#"{"_collection":"lecturers"}"#).unwrap();
    /// # db.link("cls/math", "lec/ali", "taught_by");
    /// let types = db.edge_types_from_collection("classrooms");
    /// assert_eq!(types, vec!["taught_by"]);
    /// ```
    pub fn edge_types_from_collection(&self, collection: &str) -> Vec<String> {
        let col_h = sk_hash(collection);
        let mut seen = std::collections::HashSet::new();
        let mut types = Vec::new();
        for (&node_h, node) in &self.nodes {
            if node.collection.is_empty() || sk_hash(&node.collection) != col_h { continue; }
            if let Some(edges) = self.edges.fwd_edges(node_h) {
                for e in edges {
                    if let Some(label) = self.edges.type_name(e.edge_type) {
                        if seen.insert(e.edge_type) {
                            types.push(label.to_string());
                        }
                    }
                }
            }
        }
        types.sort();
        types
    }

    /// Full graph schema: distinct `(from_collection, edge_type, to_collection)` triples.
    ///
    /// Tells you what relationships actually exist between collections in the data.
    ///
    /// ```
    /// # use sekejap::CoreDB;
    /// # let mut db = CoreDB::new();
    /// # db.put("cls/math", r#"{"_collection":"classrooms"}"#).unwrap();
    /// # db.put("lec/ali",  r#"{"_collection":"lecturers"}"#).unwrap();
    /// # db.link("cls/math", "lec/ali", "taught_by");
    /// let schema = db.edge_schema();
    /// assert_eq!(schema, vec![("classrooms".into(), "taught_by".into(), "lecturers".into())]);
    /// ```
    pub fn edge_schema(&self) -> Vec<(String, String, String)> {
        let mut seen = std::collections::HashSet::new();
        let mut triples = Vec::new();
        for (&from_h, node) in &self.nodes {
            if node.collection.is_empty() { continue; }
            let from_col = node.collection.clone();
            if let Some(edges) = self.edges.fwd_edges(from_h) {
                for e in edges {
                    let edge_label = match self.edges.type_name(e.edge_type) {
                        Some(l) => l.to_string(),
                        None => continue,
                    };
                    let to_col = match self.nodes.get(&e.other) {
                        Some(n) if !n.collection.is_empty() => n.collection.clone(),
                        _ => continue,
                    };
                    let key = (from_col.clone(), edge_label.clone(), to_col.clone());
                    if seen.insert(key.clone()) {
                        triples.push(key);
                    }
                }
            }
        }
        triples.sort();
        triples
    }

    // ── Query starters ────────────────────────────────────────────────────────

    /// Start a query from a single node.
    pub fn one(&self, slug: &str) -> Set<'_> {
        Set::new(self, Step::One(sk_hash(slug)))
    }

    /// Start a query from a set of nodes.
    pub fn many<'a>(&self, slugs: impl IntoIterator<Item = &'a str>) -> Set<'_> {
        Set::new(self, Step::Many(slugs.into_iter().map(sk_hash).collect()))
    }

    /// Start a query over all nodes.
    pub fn all(&self) -> Set<'_> {
        Set::new(self, Step::All)
    }

    /// Start a query over all nodes in a named collection.
    pub fn collection(&self, name: &str) -> Set<'_> {
        Set::new(self, Step::Collection(sk_hash(name)))
    }

    /// Execute a SQL query and return a lazy [`Set`].
    ///
    /// Accepts all SekejapQL query forms:
    ///
    /// ```text
    /// -- Standard SELECT
    /// SELECT * FROM collection [WHERE ...] [ORDER BY ...] [LIMIT n]
    ///
    /// -- Graph aggregate
    /// SELECT b._key AS name, SUM(r.weight) AS total
    /// FROM MATCH (a:col)-[r:edge]->(b:col) [WHERE a._key = 'val']
    /// [GROUP BY b._key] [ORDER BY total DESC] [LIMIT n]
    ///
    /// -- Graph aggregate with WITH chaining (multi-stage)
    /// SELECT c.name AS city, COUNT(*) AS friends
    /// FROM MATCH (a:users)-[:knows*1..3]->(b:users)
    /// WHERE a._key = 'alice'
    /// WITH b
    /// MATCH (b)-[:lives_in]->(c:cities)
    /// WHERE c.population > 100000
    /// GROUP BY c.name ORDER BY friends DESC LIMIT 10
    ///
    /// -- MATCH...RETURN (Cypher-style, routed through query())
    /// MATCH (a:col)-[:edge]->(b:col) RETURN a._key AS name, b.score AS val
    /// MATCH (a:col)-[:e]->(b) WITH b MATCH (b)-[:e2]->(c) RETURN c._key AS dest
    ///
    /// -- Shortest path (0 rows = unreachable, 1 row = found)
    /// SELECT a.field AS from_f, b.field AS to_f, r.length AS hops
    /// FROM MATCH SHORTEST (a)-[r*]->(b)
    /// WHERE a._key = 'start/slug' AND b._key = 'end/slug'
    /// [AND ANY(n IN nodes(r) WHERE n.field op val)]
    ///
    /// -- Multi-FROM cross-join
    /// SELECT a.field AS af, b.field AS bf
    /// FROM MATCH (a:col)-[:edge]->(b), collection_name AS alias
    ///
    /// -- Supported return expressions
    /// var.field | COUNT(*) | SUM(math) | AVG(math) | MIN(math) | MAX(math)
    /// PATH_AVG(var.field) | PATH_SUM | PATH_MIN | PATH_MAX | PATH_PRODUCT
    /// PATH_FIRST(var.field) | PATH_LAST(var.field)
    /// CASE WHEN var.field op literal THEN literal [WHEN ...] [ELSE literal] END
    /// AGE_DAYS(var.field) | AGE_HOURS(var.field) | NOW()
    /// JSON_ARRAY_LENGTH(var.field)
    /// ```
    ///
    /// # Errors
    /// Returns [`SqlError`] if the SQL is syntactically invalid.
    ///
    /// # Example
    /// ```
    /// # use sekejap::CoreDB;
    /// let mut db = CoreDB::new();
    /// db.put("alice", r#"{"name":"Alice","_collection":"users"}"#).unwrap();
    /// let hits = db.query("SELECT * FROM users WHERE name = 'Alice'")
    ///     .unwrap().collect();
    /// assert_eq!(hits[0].slug, "alice");
    /// ```
    pub fn query(&self, sql: &str) -> Result<Set<'_>, SqlError> {
        // `SHOW TABLES` / `SHOW EDGES …` / `SHOW <table>` return metadata rather than
        // rows — route them to the show path so `query("SHOW TABLES")` just works.
        if sql
            .trim_start()
            .split_whitespace()
            .next()
            .map_or(false, |w| w.eq_ignore_ascii_case("show"))
        {
            return Ok(Set::from_hits(self, self.show(sql)?));
        }
        Ok(self.run_plan(sql::parse_match_or_agg(sql)?))
    }

    /// Execute an already-parsed plan. Shared by `query`, `query_params`, and the
    /// prepared-statement path so all three run identically.
    fn run_plan(&self, plan: sql::MatchOrAgg) -> Set<'_> {
        match plan {
            sql::MatchOrAgg::Agg(stmt) => Set::from_hits(self, query::execute_match_agg(self, stmt)),
            sql::MatchOrAgg::AggUnion(stmts) => {
                Set::from_hits(self, query::execute_match_agg_union(self, stmts))
            }
            sql::MatchOrAgg::Shortest(stmt) => {
                Set::from_hits(self, query::execute_shortest_select(self, stmt))
            }
            sql::MatchOrAgg::MultiFrom(stmt) => {
                Set::from_hits(self, query::execute_multi_from(self, stmt))
            }
            sql::MatchOrAgg::Steps(steps) => Set::from_steps(self, steps),
        }
    }

    /// Compile a query once for repeated execution — a **prepared statement**.
    /// The SQL is tokenized and validated now; each [`query_prepared`](Self::query_prepared)
    /// call re-lowers the cached tokens with fresh `$N` parameters, skipping
    /// re-tokenization. Best for the same query shape run many times (with
    /// different parameter values). Also the injection-safe way to run user input.
    ///
    /// ```
    /// # use sekejap::CoreDB;
    /// # use serde_json::json;
    /// let mut db = CoreDB::new();
    /// db.put("u/a", r#"{"_collection":"u","_key":"a","age":30}"#).unwrap();
    /// let stmt = db.prepare("SELECT _key FROM u WHERE age = $1").unwrap();
    /// let hits = db.query_prepared(&stmt, &[json!(30)]).unwrap().collect();
    /// assert_eq!(hits.len(), 1);
    /// ```
    pub fn prepare(&self, sql: &str) -> Result<sql::PreparedQuery, SqlError> {
        sql::PreparedQuery::compile(sql)
    }

    /// Run a [`prepare`](Self::prepare)d query, binding `$1`, `$2`, … to `params`.
    pub fn query_prepared(
        &self,
        prepared: &sql::PreparedQuery,
        params: &[Value],
    ) -> Result<Set<'_>, SqlError> {
        Ok(self.run_plan(prepared.lower(params.to_vec())?))
    }

    /// Parameterized SELECT / MATCH query.
    ///
    /// Values are bound to `$1`, `$2`, … placeholders in the SQL string.
    /// Parameters are resolved at parse time — the execution layer is unchanged.
    ///
    /// # Example
    /// ```
    /// # use sekejap::CoreDB;
    /// # use serde_json::json;
    /// let mut db = CoreDB::new();
    /// db.put("users/alice", r#"{"name":"Alice","age":30,"_collection":"users"}"#).unwrap();
    /// let hits = db.query_params(
    ///     "SELECT * FROM users WHERE name = $1",
    ///     &[json!("Alice")],
    /// ).unwrap().collect();
    /// assert_eq!(hits[0].slug, "users/alice");
    /// ```
    pub fn query_params(&self, sql: &str, params: &[Value]) -> Result<Set<'_>, SqlError> {
        Ok(self.run_plan(sql::parse_match_or_agg_params(sql, params.to_vec())?))
    }

    /// `EXPLAIN SELECT ...` — return the query plan as result rows.
    pub fn explain(&self, sql: &str) -> Result<Vec<query::Hit>, SqlError> {
        match sql::parse_match_or_agg(sql)? {
            sql::MatchOrAgg::Steps(steps) => {
                Ok(query::explain_steps(self, &steps))
            }
            sql::MatchOrAgg::Agg(stmt) => {
                let mut rows = Vec::new();
                let mut map = serde_json::Map::new();
                map.insert("step".into(), serde_json::json!("MATCH Aggregate"));
                map.insert("detail".into(), serde_json::json!(format!(
                    "hops: {}, returns: {}, group_by: {}",
                    stmt.hops.len(),
                    stmt.returns.len(),
                    stmt.group_by.as_ref().map_or(0, |g| g.len()),
                )));
                rows.push(query::Hit {
                    slug: String::new(), slug_hash: 0,
                    payload: Some(Value::Object(map)),
                });
                Ok(rows)
            }
            sql::MatchOrAgg::AggUnion(stmts) => {
                let mut map = serde_json::Map::new();
                map.insert("step".into(), serde_json::json!("MATCH Aggregate UNION"));
                map.insert("detail".into(), serde_json::json!(format!("arms: {}", stmts.len())));
                Ok(vec![query::Hit {
                    slug: String::new(), slug_hash: 0,
                    payload: Some(Value::Object(map)),
                }])
            }
            sql::MatchOrAgg::Shortest(_) => {
                let mut map = serde_json::Map::new();
                map.insert("step".into(), serde_json::json!("Shortest Path"));
                Ok(vec![query::Hit {
                    slug: String::new(), slug_hash: 0,
                    payload: Some(Value::Object(map)),
                }])
            }
            sql::MatchOrAgg::MultiFrom(_) => {
                let mut map = serde_json::Map::new();
                map.insert("step".into(), serde_json::json!("Multi-FROM Join"));
                Ok(vec![query::Hit {
                    slug: String::new(), slug_hash: 0,
                    payload: Some(Value::Object(map)),
                }])
            }
        }
    }

    /// `EXPLAIN ANALYZE SELECT ...` — run the query and return plan + actual timing.
    pub fn explain_analyze(&self, sql: &str) -> Result<Vec<query::Hit>, SqlError> {
        let t0 = std::time::Instant::now();
        let result = self.query(sql)?;
        let hits = result.collect();
        let elapsed = t0.elapsed();
        let mut rows = self.explain(sql)?;
        // Append actual execution stats.
        let mut map = serde_json::Map::new();
        map.insert("step".into(), serde_json::json!("Actual Results"));
        map.insert("rows".into(), serde_json::json!(hits.len()));
        map.insert("time_ms".into(), serde_json::json!(
            format!("{:.3}", elapsed.as_secs_f64() * 1000.0)
        ));
        rows.push(query::Hit {
            slug: String::new(), slug_hash: 0,
            payload: Some(Value::Object(map)),
        });
        Ok(rows)
    }

    // ── Graph path queries ────────────────────────────────────────────────────

    /// BFS from `start` to `end`, tracking the parent pointer and edge used at
    /// each hop so the path can be reconstructed.
    ///
    /// Returns `None` when no path exists.
    /// Returns a zero-hop `BfsPath` when `start == end`.
    pub(crate) fn bfs_shortest_path(&self, start: u64, end: u64) -> Option<BfsPath> {
        use std::collections::{HashMap, VecDeque};

        // Sentinel: parent for the start node points to itself with a zero
        // edge_type hash so we can detect "we are at the root" during
        // reconstruction without a separate visited set.
        // (from_hash, edge_type_hash, meta)
        let mut parent: HashMap<u64, (u64, u64, Option<Value>)> = HashMap::new();

        // Same-node degenerate case
        if start == end {
            if let Some(node) = self.nodes.get(&start) {
                let hit = query::Hit {
                    slug: node.slug.clone(),
                    slug_hash: start,
                    payload: self.payload_store.get(node.payload_offset, node.payload_len),
                };
                return Some(BfsPath { nodes: vec![hit], edges: vec![], length: 0 });
            } else {
                return None; // start node doesn't exist
            }
        }

        // The start node must exist
        if !self.nodes.contains_key(&start) {
            return None;
        }

        parent.insert(start, (start, 0, None)); // sentinel
        let mut queue: VecDeque<u64> = VecDeque::new();
        queue.push_back(start);

        while let Some(current) = queue.pop_front() {
            if let Some(edges) = self.edges.fwd_edges(current) {
                for e in edges {
                    if parent.contains_key(&e.other) {
                        continue; // already visited
                    }
                    parent.insert(e.other, (current, e.edge_type, self.edges.edge_meta(e)));
                    if e.other == end {
                        // Reconstruct path: walk parent map from end → start, then reverse.
                        let mut node_hashes: Vec<u64> = Vec::new();
                        let mut cur = end;
                        loop {
                            node_hashes.push(cur);
                            let (prev, _, _) = parent[&cur];
                            if prev == cur {
                                break; // reached the sentinel (start node)
                            }
                            cur = prev;
                        }
                        node_hashes.reverse();

                        // Build Hit list from the ordered hashes
                        let nodes: Vec<query::Hit> = node_hashes
                            .iter()
                            .filter_map(|&h| {
                                self.nodes.get(&h).map(|n| query::Hit {
                                    slug: n.slug.clone(),
                                    slug_hash: h,
                                    payload: self.payload_store.get(n.payload_offset, n.payload_len),
                                })
                            })
                            .collect();

                        // Build EdgeHit list: edges[i] connects nodes[i] → nodes[i+1]
                        let edges: Vec<EdgeHit> = node_hashes
                            .windows(2)
                            .map(|w| {
                                let (_, edge_type_hash, meta) = parent[&w[1]].clone();
                                EdgeHit {
                                    from_slug: self.nodes.get(&w[0]).map(|n| n.slug.clone()),
                                    to_slug: self.nodes.get(&w[1]).map(|n| n.slug.clone()),
                                    edge_type: self.edges.type_name(edge_type_hash).map(|s| s.to_string()),
                                    edge_type_hash,
                                    meta,
                                }
                            })
                            .collect();

                        let length = edges.len();
                        return Some(BfsPath { nodes, edges, length });
                    }
                    queue.push_back(e.other);
                }
            }
        }

        None // no path found
    }

    /// Execute a `SHOW` introspection statement.
    ///
    /// Syntax:
    /// ```text
    /// SHOW TABLES
    ///     → [{name, count}, ...]  — all collections with row counts (includes declared-empty tables)
    ///
    /// SHOW EDGES
    ///     → [{from, type, to, count}, ...]  — full graph schema with edge counts
    ///
    /// SHOW EDGES FROM collection
    ///     → [{from, type, count}, ...]  — edge types leaving that collection + counts
    ///
    /// SHOW EDGES FROM col1 TO col2
    ///     → [{from, type, to, count}, ...]  — edge types between two collections + counts
    ///
    /// SHOW CREATE TABLE collection
    ///     → [{ddl: "CREATE TABLE ..."}]  — DDL that recreates the declared schema
    ///
    /// SHOW collection
    ///     → [{field, type, primary_key?, source}, ...]
    ///       Uses declared schema if CREATE TABLE was issued; otherwise infers
    ///       types from actual node data. source = "declared" | "inferred".
    /// ```
    pub fn show(&self, sql: &str) -> Result<Vec<query::Hit>, SqlError> {
        let stmt = sql::parse_show(sql)?;

        let make_hit = |payload: serde_json::Value| query::Hit {
            slug: String::new(),
            slug_hash: 0,
            payload: Some(payload),
        };

        match stmt {
            // ── SHOW TABLES ───────────────────────────────────────────────────
            sql::ShowStmt::Tables => {
                // Use collection_names_map (O(1) per collection) — no node scan needed.
                // Insert actual counts first, then seed declared-but-empty schemas with 0.
                let mut stats: std::collections::BTreeMap<String, (usize, u64)> =
                    std::collections::BTreeMap::new();
                for (hash, name) in &self.collection_names_map {
                    let (count, size) = self.collections.get(hash)
                        .map(|members| {
                            let c = members.len();
                            let s: u64 = members.iter()
                                .filter_map(|h| self.nodes.get(h).map(|n| n.payload_len as u64))
                                .sum();
                            (c, s)
                        })
                        .unwrap_or((0, 0));
                    stats.insert(name.clone(), (count, size));
                }
                for name in self.schemas.keys() {
                    stats.entry(name.clone()).or_insert((0, 0));
                }
                Ok(stats.into_iter()
                    .map(|(name, (count, size_bytes))| make_hit(serde_json::json!({
                        "name": name, "count": count, "size_bytes": size_bytes
                    })))
                    .collect())
            }

            // ── SHOW EDGES ────────────────────────────────────────────────────
            sql::ShowStmt::Edges(e) => {
                match (e.from_col, e.to_col) {
                    (None, _) => {
                        // Full schema — count all edges per (from, type, to) triple
                        let mut counts: std::collections::HashMap<(String, String, String), usize> =
                            std::collections::HashMap::new();
                        for (&from_h, node) in &self.nodes {
                            let from_col = if node.collection.is_empty() {
                                continue;
                            } else {
                                node.collection.clone()
                            };
                            if let Some(edges) = self.edges.fwd_edges(from_h) {
                                for edge in edges {
                                    let label = match self.edges.type_name(edge.edge_type) {
                                        Some(l) => l.to_string(),
                                        None => continue,
                                    };
                                    let to_col = match self.nodes.get(&edge.other)
                                        .map(|n| &n.collection)
                                    {
                                        Some(c) if !c.is_empty() => c.clone(),
                                        _ => continue,
                                    };
                                    *counts.entry((from_col.clone(), label, to_col)).or_insert(0) += 1;
                                }
                            }
                        }
                        let mut hits: Vec<_> = counts.into_iter()
                            .map(|((from, kind, to), count)| make_hit(serde_json::json!({
                                "from": from, "type": kind, "to": to, "count": count
                            })))
                            .collect();
                        hits.sort_by(|a, b| {
                            let ka = a.payload.as_ref().and_then(|p| p["from"].as_str()).unwrap_or("");
                            let kb = b.payload.as_ref().and_then(|p| p["from"].as_str()).unwrap_or("");
                            ka.cmp(kb)
                        });
                        Ok(hits)
                    }
                    (Some(from_col), None) => {
                        // Types leaving one collection + counts
                        let col_h = sk_hash(&from_col);
                        let mut counts: std::collections::HashMap<String, usize> =
                            std::collections::HashMap::new();
                        for (&node_h, node) in &self.nodes {
                            if !node.collection.is_empty() && sk_hash(&node.collection) == col_h {
                                if let Some(edges) = self.edges.fwd_edges(node_h) {
                                    for edge in edges {
                                        if let Some(label) = self.edges.type_name(edge.edge_type) {
                                            *counts.entry(label.to_string()).or_insert(0) += 1;
                                        }
                                    }
                                }
                            }
                        }
                        let mut hits: Vec<_> = counts.into_iter()
                            .map(|(kind, count)| make_hit(serde_json::json!({
                                "from": from_col, "type": kind, "count": count
                            })))
                            .collect();
                        hits.sort_by(|a, b| {
                            let ka = a.payload.as_ref().and_then(|p| p["type"].as_str()).unwrap_or("");
                            let kb = b.payload.as_ref().and_then(|p| p["type"].as_str()).unwrap_or("");
                            ka.cmp(kb)
                        });
                        Ok(hits)
                    }
                    (Some(from_col), Some(to_col)) => {
                        // Types between two collections + counts
                        let from_h = sk_hash(&from_col);
                        let to_col_h = sk_hash(&to_col);
                        let mut counts: std::collections::HashMap<String, usize> =
                            std::collections::HashMap::new();
                        for (&node_h, node) in &self.nodes {
                            if !node.collection.is_empty() && sk_hash(&node.collection) == from_h {
                                if let Some(edges) = self.edges.fwd_edges(node_h) {
                                    for edge in edges {
                                        let in_to = self.nodes.get(&edge.other)
                                            .map(|n| !n.collection.is_empty() && sk_hash(&n.collection) == to_col_h)
                                            .unwrap_or(false);
                                        if in_to {
                                            if let Some(label) = self.edges.type_name(edge.edge_type) {
                                                *counts.entry(label.to_string()).or_insert(0) += 1;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        let mut hits: Vec<_> = counts.into_iter()
                            .map(|(kind, count)| make_hit(serde_json::json!({
                                "from": from_col, "type": kind, "to": to_col, "count": count
                            })))
                            .collect();
                        hits.sort_by(|a, b| {
                            let ka = a.payload.as_ref().and_then(|p| p["type"].as_str()).unwrap_or("");
                            let kb = b.payload.as_ref().and_then(|p| p["type"].as_str()).unwrap_or("");
                            ka.cmp(kb)
                        });
                        Ok(hits)
                    }
                }
            }

            // ── SHOW CREATE TABLE <collection> ───────────────────────────────
            sql::ShowStmt::CreateTable(collection) => {
                let ddl = self.schema_ddl(&collection).unwrap_or_else(|| {
                    format!("-- no CREATE TABLE declared for '{collection}'")
                });
                Ok(vec![make_hit(serde_json::json!({ "ddl": ddl }))])
            }

            // ── SHOW <collection> ─────────────────────────────────────────────
            sql::ShowStmt::Collection(collection) => {
                // Declared schema takes priority
                if let Some(schema) = self.schemas.get(&collection) {
                    let hits = schema.fields.iter().map(|f| {
                        let ty = match f.ty {
                            sql::FieldType::Text        => "TEXT",
                            sql::FieldType::Integer     => "INTEGER",
                            sql::FieldType::Real        => "REAL",
                            sql::FieldType::Bool        => "BOOLEAN",
                            sql::FieldType::Timestamptz => "TIMESTAMPTZ",
                            sql::FieldType::Geo         => "GEO",
                            sql::FieldType::Vector      => "VECTOR",
                            sql::FieldType::Json        => "JSON",
                        };
                        make_hit(serde_json::json!({
                            "field": f.name,
                            "type": ty,
                            "primary_key": f.is_primary_key,
                            "source": "declared",
                        }))
                    }).collect();
                    return Ok(hits);
                }

                // Inferred from data — scan nodes in collection
                let col_h = sk_hash(&collection);
                const SKIP: &[&str] = &["_collection", "_id", "_created_unix", "_updated_unix"];
                let mut field_types: std::collections::BTreeMap<String, &'static str> =
                    std::collections::BTreeMap::new();

                for node in self.nodes.values() {
                    if !node.collection.is_empty() && sk_hash(&node.collection) == col_h {
                        if let Some(payload) = self.payload_store.get(node.payload_offset, node.payload_len) {
                            if let serde_json::Value::Object(map) = payload {
                                for (k, v) in &map {
                                    if SKIP.contains(&k.as_str()) { continue; }
                                    let inferred = match v {
                                        serde_json::Value::String(_) => "TEXT",
                                        serde_json::Value::Number(n)
                                            if n.is_i64() || n.is_u64() => "INTEGER",
                                        serde_json::Value::Number(_) => "REAL",
                                        serde_json::Value::Bool(_) => "BOOLEAN",
                                        serde_json::Value::Array(a)
                                            if a.iter().all(|x| x.is_number()) => "VECTOR",
                                        serde_json::Value::Array(_)
                                        | serde_json::Value::Object(_) => "JSON",
                                        serde_json::Value::Null => continue,
                                    };
                                    field_types.entry(k.clone()).or_insert(inferred);
                                }
                            }
                        }
                    }
                }

                Ok(field_types.into_iter()
                    .map(|(field, ty)| make_hit(serde_json::json!({
                        "field": field,
                        "type": ty,
                        "source": "inferred",
                    })))
                    .collect())
            }
        }
    }

    /// Execute a mutation SQL statement.
    ///
    /// Returns the number of rows affected.
    ///
    /// Supports: `INSERT INTO`, `INSERT (edge)`, `DELETE FROM`, `DELETE (edge)`,
    /// `UPDATE`.
    ///
    /// # Errors
    /// Returns [`SqlError`] if the SQL is invalid.
    pub fn execute(&mut self, sql: &str) -> Result<usize, SqlError> {
        let mutation = sql::parse_mutation(sql)?;
        self.execute_mutation(mutation)
    }

    /// Parameterized mutation (INSERT / UPDATE / DELETE).
    ///
    /// Values are bound to `$1`, `$2`, … placeholders in the SQL string.
    ///
    /// # Example
    /// ```
    /// # use sekejap::CoreDB;
    /// # use serde_json::json;
    /// let mut db = CoreDB::new();
    /// db.execute("CREATE TABLE users (_key TEXT PRIMARY KEY, name TEXT, age INTEGER)").unwrap();
    /// let n = db.execute_params(
    ///     "INSERT INTO users (_key, name, age) VALUES ($1, $2, $3)",
    ///     &[json!("u1"), json!("Bob"), json!(30)],
    /// ).unwrap();
    /// assert_eq!(n, 1);
    /// ```
    pub fn execute_params(&mut self, sql: &str, params: &[Value]) -> Result<usize, SqlError> {
        // Re-parse with params, then delegate to the same execution arms.
        // We cannot just call self.execute() because it would re-parse without params.
        match sql::parse_mutation_params(sql, params.to_vec())? {
            m => {
                // Build a temporary SQL string? No — reuse the mutation directly.
                // We need to inline the same execute() body. Instead, factor via a helper.
                self.execute_mutation(m)
            }
        }
    }

    /// Internal: execute an already-parsed mutation.
    fn execute_mutation(&mut self, mutation: sql::CompiledMutation) -> Result<usize, SqlError> {
        // ── Transaction control ──────────────────────────────────────
        match &mutation {
            sql::CompiledMutation::Begin => {
                if self.pending_txn.is_some() {
                    return Err(SqlError::TransactionError(
                        "BEGIN inside an active transaction (nested BEGIN not supported)".into(),
                    ));
                }
                self.pending_txn = Some(Vec::new());
                return Ok(0);
            }
            sql::CompiledMutation::Commit => {
                let buf = self.pending_txn.take().ok_or_else(|| {
                    SqlError::TransactionError("COMMIT without an active transaction".into())
                })?;
                self.defer_wal_sync = true;
                self.defer_index_rebuild = true;
                self.wal_write(WalEntry::TxnBegin);
                let mut total = 0usize;
                for op in buf {
                    total += self.execute_mutation(op)?;
                }
                self.wal_write(WalEntry::TxnEnd);
                self.defer_wal_sync = false;
                self.wal_flush();
                self.flush_deferred_indexes();
                return Ok(total);
            }
            sql::CompiledMutation::Rollback => {
                if self.pending_txn.take().is_none() {
                    return Err(SqlError::TransactionError(
                        "ROLLBACK without an active transaction".into(),
                    ));
                }
                return Ok(0);
            }
            sql::CompiledMutation::Compact => {
                self.compact().map_err(|e| SqlError::TransactionError(
                    format!("COMPACT failed: {e}"),
                ))?;
                return Ok(0);
            }
            sql::CompiledMutation::SetWalFormat(fmt) => {
                self.wal_format = *fmt;
                self.compact().map_err(|e| SqlError::TransactionError(
                    format!("SET WAL_FORMAT failed: {e}"),
                ))?;
                return Ok(0);
            }
            sql::CompiledMutation::SetWalMode(logical) => {
                self.logical_wal = *logical;
                return Ok(0);
            }
            sql::CompiledMutation::SetWalSync(level) => {
                self.wal_sync_level = *level;
                if let Some(wal) = &mut self.wal {
                    wal.set_sync_level(*level);
                }
                return Ok(0);
            }
            _ => {}
        }

        // If inside a transaction, buffer the mutation instead of executing it.
        if let Some(buf) = &mut self.pending_txn {
            buf.push(mutation);
            return Ok(0);
        }

        match mutation {
            sql::CompiledMutation::Insert { collection, mut slug, payload_json, mut vectors } => {
                let payload_json = if let Some(schema) = self.schemas.get(&collection).cloned() {
                    let mut payload: Value = serde_json::from_str(&payload_json)
                        .map_err(|e| SqlError::InvalidValue(e.to_string()))?;
                    if let Value::Object(ref mut map) = payload {
                        // Rescue GEO fields that were mis-routed to vectors
                        vectors.retain(|(field, data)| {
                            let is_geo = schema.fields.iter().any(|f| {
                                f.name == *field && matches!(f.ty, sql::FieldType::Geo)
                            });
                            if is_geo {
                                let arr: Vec<Value> = data.iter()
                                    .map(|&f| serde_json::Number::from_f64(f as f64)
                                        .map(Value::Number).unwrap_or(Value::Null))
                                    .collect();
                                map.insert(field.clone(), Value::Array(arr));
                                false // remove from vectors
                            } else {
                                true // keep as vector
                            }
                        });
                        for field in &schema.fields {
                            if map.contains_key(&field.name) {
                                continue;
                            }
                            if field.default_uuid4 {
                                map.insert(
                                    field.name.clone(),
                                    Value::String(crate::scalar::uuid_v4()),
                                );
                            } else if let Some((ns, nm)) = &field.default_uuid5 {
                                map.insert(
                                    field.name.clone(),
                                    Value::String(crate::scalar::uuid_v5(ns, nm)),
                                );
                            }
                        }
                        if slug.is_empty() {
                            match map.get("_key").and_then(|v| v.as_str()) {
                                Some(key_val) => {
                                    slug = format!("{}/{}", collection, key_val);
                                    map.insert("_id".into(), Value::String(slug.clone()));
                                }
                                None => {
                                    return Err(SqlError::MissingField { field: "_key" });
                                }
                            }
                        }
                    }
                    if let Some(err) = validate_payload_against_schema(&schema, &payload) {
                        return Err(err);
                    }
                    serde_json::to_string(&payload)
                        .map_err(|e| SqlError::InvalidValue(e.to_string()))?
                } else if slug.is_empty() {
                    return Err(SqlError::MissingField { field: "_key" });
                } else {
                    payload_json
                };
                self.put(&slug, &payload_json)
                    .map_err(|e| SqlError::InvalidValue(e.to_string()))?;
                for (field, data) in vectors {
                    self.put_vector(&slug, &field, &data)
                        .map_err(|e| SqlError::InvalidValue(e.to_string()))?;
                }
                Ok(1)
            }
            sql::CompiledMutation::InsertBatch { collection, items } => {
                let schema = self.schemas.get(&collection).cloned();
                let mut affected_vec_fields: HashSet<String> = HashSet::new();
                let count = items.len();
                // Process each row to its final payload Value + slug, collecting into
                // a single bulk write (skips per-row splice + the O(N) collection
                // membership scan) instead of N slow `put` calls. Vectors are applied
                // after the nodes exist. defer_wal_sync = one fsync for the batch.
                self.defer_wal_sync = true;
                self.defer_index_rebuild = true;
                let mut bulk_rows: Vec<(String, Value)> = Vec::with_capacity(count);
                let mut vector_ops: Vec<(String, String, Vec<f32>)> = Vec::new();
                for (mut slug, payload_json, mut vectors) in items {
                    let payload: Value = if let Some(ref schema) = schema {
                        let mut payload: Value = serde_json::from_str(&payload_json)
                            .map_err(|e| SqlError::InvalidValue(e.to_string()))?;
                        if let Value::Object(ref mut map) = payload {
                            // Rescue GEO fields that were mis-routed to vectors
                            vectors.retain(|(field, data)| {
                                let is_geo = schema.fields.iter().any(|f| {
                                    f.name == *field && matches!(f.ty, sql::FieldType::Geo)
                                });
                                if is_geo {
                                    let arr: Vec<Value> = data.iter()
                                        .map(|&f| serde_json::Number::from_f64(f as f64)
                                            .map(Value::Number).unwrap_or(Value::Null))
                                        .collect();
                                    map.insert(field.clone(), Value::Array(arr));
                                    false
                                } else {
                                    true
                                }
                            });
                            for field in &schema.fields {
                                if map.contains_key(&field.name) {
                                    continue;
                                }
                                if field.default_uuid4 {
                                    map.insert(field.name.clone(), Value::String(crate::scalar::uuid_v4()));
                                } else if let Some((ns, nm)) = &field.default_uuid5 {
                                    map.insert(field.name.clone(), Value::String(crate::scalar::uuid_v5(ns, nm)));
                                }
                            }
                            if slug.is_empty() {
                                match map.get("_key").and_then(|v| v.as_str()) {
                                    Some(key_val) => {
                                        slug = format!("{}/{}", collection, key_val);
                                        map.insert("_id".into(), Value::String(slug.clone()));
                                    }
                                    None => return Err(SqlError::MissingField { field: "_key" }),
                                }
                            }
                        }
                        if let Some(err) = validate_payload_against_schema(schema, &payload) {
                            return Err(err);
                        }
                        payload
                    } else if slug.is_empty() {
                        return Err(SqlError::MissingField { field: "_key" });
                    } else {
                        serde_json::from_str(&payload_json)
                            .map_err(|e| SqlError::InvalidValue(e.to_string()))?
                    };
                    for (field, data) in vectors {
                        vector_ops.push((slug.clone(), field.clone(), data));
                        affected_vec_fields.insert(field);
                    }
                    bulk_rows.push((slug, payload));
                }
                // Fast bulk payload write (deferred sync — one fsync for the batch).
                self.put_value_bulk(bulk_rows)
                    .map_err(|e| SqlError::InvalidValue(e.to_string()))?;
                // Vectors: nodes now exist; WAL + store, HNSW rebuilt once below.
                for (slug, field, data) in vector_ops {
                    self.wal_write(WalEntry::PutVector { slug: slug.clone(), field: field.clone(), data: data.clone() });
                    let hash = sk_hash(&slug);
                    self.ensure_vector_store(&field);
                    self.vectors.get_mut(&field).unwrap().put(hash, data);
                }
                self.defer_wal_sync = false;
                self.wal_flush();
                self.flush_deferred_indexes();
                for field in &affected_vec_fields {
                    let hnsw_declared = self.schemas.values()
                        .any(|s| s.indexes.vector.contains(field));
                    if hnsw_declared {
                        let (m, ef) = self.hnsw_params.get(field.as_str()).copied().unwrap_or((16, 200));
                        let _ = self.build_hnsw_index(field, m, ef);
                    }
                }
                Ok(count)
            }
            sql::CompiledMutation::Delete(steps) => {
                let slugs: Vec<String> = Set::from_steps(self, steps)
                    .collect()
                    .into_iter()
                    .map(|h| h.slug)
                    .collect();
                let count = slugs.len();
                self.defer_wal_sync = true;
                self.defer_index_rebuild = true;
                for slug in &slugs {
                    self.remove(slug);
                }
                self.defer_wal_sync = false;
                self.wal_flush();
                self.flush_deferred_indexes();
                Ok(count)
            }
            sql::CompiledMutation::InsertEdge(edges) => {
                let count = edges.len();
                self.defer_wal_sync = true;
                for edge in edges {
                    // Props route by value type inside link_meta_raw (primitives →
                    // fast lane, rest → JSON bag); the WAL LinkMeta carries the full
                    // props so replay re-routes identically.
                    match edge.props_json {
                        Some(json) => self
                            .link_meta(&edge.from, &edge.to, &edge.edge_type, &json)
                            .map_err(|e| SqlError::InvalidValue(e.to_string()))?,
                        None => self.link(&edge.from, &edge.to, &edge.edge_type),
                    }
                }
                self.defer_wal_sync = false;
                self.wal_flush();
                Ok(count)
            }
            sql::CompiledMutation::DeleteEdge(edges) => {
                let count = edges.len();
                self.defer_wal_sync = true;
                for edge in edges {
                    self.unlink(&edge.from, &edge.to, &edge.edge_type);
                }
                self.defer_wal_sync = false;
                self.wal_flush();
                Ok(count)
            }
            sql::CompiledMutation::MatchInsert {
                match_steps,
                target,
                edge_type,
                props,
            } => {
                let source_slugs: Vec<String> = Set::from_steps(self, match_steps.clone())
                    .collect()
                    .into_iter()
                    .map(|h| h.slug)
                    .collect();
                let count = source_slugs.len();
                self.defer_wal_sync = true;
                for src_slug in source_slugs {
                    match &props {
                        Some(json) => {
                            self.link_meta(&src_slug, &target, &edge_type, json)
                                .map_err(|e| SqlError::InvalidValue(e.to_string()))?;
                        }
                        None => {
                            self.link(&src_slug, &target, &edge_type);
                        }
                    }
                }
                self.defer_wal_sync = false;
                self.wal_flush();
                Ok(count)
            }
            sql::CompiledMutation::Update { steps, updates } => {
                // Decide: splice fast path (no vector/geo field updates) or full-parse slow path
                let has_vec = updates.iter().any(|(_, v)| value_as_f32_vec(v).is_some());
                let has_geo = !has_vec && self.schemas.values().any(|schema| {
                    updates.iter().any(|(field, _)| {
                        schema.fields.iter().any(|f| &f.name == field && matches!(f.ty, sql::FieldType::Geo))
                    })
                });

                if !has_vec && !has_geo {
                    let now_ms = chrono::Utc::now().timestamp_millis();
                    self.update_fast_path(steps, &updates, now_ms)
                } else {
                    // ── SLOW PATH: full parse (vector/geo field updates) ──────────
                    let hits: Vec<(String, Value)> = Set::from_steps(self, steps)
                        .collect()
                        .into_iter()
                        .filter_map(|h| {
                            let n = self.nodes.get(&h.slug_hash)?;
                            let payload = self.payload_store.get(n.payload_offset, n.payload_len)?;
                            Some((n.slug.clone(), payload))
                        })
                        .collect();
                    let count = hits.len();
                    self.defer_wal_sync = true;
                    self.defer_index_rebuild = true;
                    let mut affected_vec_fields: HashSet<String> = HashSet::new();
                    for (slug, mut payload) in hits {
                        let mut geo_fields: Vec<&str> = Vec::new();
                        if let Some(coll) = payload.get("_collection").and_then(|v| v.as_str()) {
                            if let Some(schema) = self.schemas.get(coll) {
                                if let Some(err) = validate_updates_against_schema(schema, &updates) {
                                    return Err(err);
                                }
                                for f in &schema.fields {
                                    if matches!(f.ty, sql::FieldType::Geo) {
                                        geo_fields.push(&f.name);
                                    }
                                }
                            }
                        }
                        let mut vec_updates: Vec<(String, Vec<f32>)> = Vec::new();
                        for (field, value) in &updates {
                            let is_geo = geo_fields.iter().any(|g| g == field);
                            if !is_geo {
                                if let Some(floats) = value_as_f32_vec(value) {
                                    vec_updates.push((field.clone(), floats));
                                    continue;
                                }
                            }
                            if let Value::Object(ref mut map) = payload {
                                map.insert(field.clone(), value.clone());
                            }
                        }
                        let json = serde_json::to_string(&payload)
                            .map_err(|e| SqlError::InvalidValue(e.to_string()))?;
                        self.put(&slug, &json)
                            .map_err(|e| SqlError::InvalidValue(e.to_string()))?;
                        for (field, data) in vec_updates {
                            self.wal_write(WalEntry::PutVector {
                                slug: slug.clone(),
                                field: field.clone(),
                                data: data.clone(),
                            });
                            let hash = sk_hash(&slug);
                            self.ensure_vector_store(&field);
                            self.vectors.get_mut(&field).unwrap().put(hash, data);
                            affected_vec_fields.insert(field);
                        }
                    }
                    for field in &affected_vec_fields {
                        let hnsw_declared = self.schemas.values()
                            .any(|s| s.indexes.vector.contains(field));
                        if hnsw_declared {
                            let (m, ef) = self.hnsw_params.get(field.as_str()).copied().unwrap_or((16, 200));
                            let _ = self.build_hnsw_index(field, m, ef);
                        }
                    }
                    self.defer_wal_sync = false;
                    self.wal_flush();
                    self.flush_deferred_indexes();
                    Ok(count)
                }
            }
            sql::CompiledMutation::CreateTable { collection, schema } => {
                let schema_json = serde_json::to_string(&schema)
                    .map_err(|e| SqlError::InvalidValue(e.to_string()))?;
                self.wal_write(WalEntry::CreateTable { collection: collection.clone(), schema_json });
                self.schemas.insert(collection, schema.clone());
                Ok(1)
            }
            sql::CompiledMutation::CreateIndex { name: _, collection, method, fields } => {
                self.wal_write(WalEntry::CreateIndex {
                    collection: collection.clone(),
                    method: method.to_string(),
                    fields: fields.clone(),
                });
                self.apply_index(&collection, &method, &fields)?;
                Ok(1)
            }
            sql::CompiledMutation::Reindex { collection, method, fields } => {
                self.apply_index(&collection, &method, &fields)?;
                Ok(1)
            }
            sql::CompiledMutation::DropTable { collection, if_exists } => {
                let has_schema = self.schemas.contains_key(&collection);
                let has_nodes  = self.collections
                    .get(&sk_hash(&collection))
                    .map(|v| !v.is_empty())
                    .unwrap_or(false);

                if !has_schema && !has_nodes {
                    if if_exists {
                        return Ok(0);
                    } else {
                        return Err(sql::SqlError::InvalidValue(
                            format!("table '{collection}' does not exist")
                        ));
                    }
                }

                self.wal_write(WalEntry::DropTable { collection: collection.clone() });
                let count = self.drop_table_raw(&collection);
                Ok(count)
            }
            sql::CompiledMutation::DropIndex { collection, method, field, if_exists } => {
                let removed = self.drop_index_raw(&collection, &method, &field);
                if !removed && !if_exists {
                    return Err(sql::SqlError::InvalidValue(format!(
                        "index on '{field}' does not exist for table '{collection}'"
                    )));
                }
                if removed {
                    self.wal_write(WalEntry::DropIndex {
                        collection,
                        method: method.to_string(),
                        field,
                    });
                }
                Ok(if removed { 1 } else { 0 })
            }
            sql::CompiledMutation::AlterTable { collection, op } => {
                let op_json = serde_json::to_string(&op)
                    .map_err(|e| sql::SqlError::InvalidValue(e.to_string()))?;
                self.wal_write(WalEntry::AlterTable { collection: collection.clone(), op_json });
                let count = self.alter_table_raw(&collection, op)?;
                Ok(count)
            }
            // Handled by the transaction control block above; unreachable here.
            sql::CompiledMutation::Begin
            | sql::CompiledMutation::Commit
            | sql::CompiledMutation::Rollback
            | sql::CompiledMutation::Compact
            | sql::CompiledMutation::SetWalFormat(_)
            | sql::CompiledMutation::SetWalMode(_)
            | sql::CompiledMutation::SetWalSync(_) => unreachable!(),
        }
    }

    // ── Internal accessors for the query executor ─────────────────────────────

    // Topology accessors return `Cow` so the backing can be either the resident
    // HashMaps (`Cow::Borrowed` — zero cost, current default) or, in the upcoming
    // paged mode, values decoded on demand from the mmap'd topology files
    // (`Cow::Owned`). The whole query executor goes through these — no direct
    // `self.nodes` / `self.edges` access outside lib.rs.

    pub(crate) fn node_data(&self, hash: u64) -> Option<std::borrow::Cow<'_, NodeData>> {
        // Overlay first: anything written since open (or everything, in resident
        // mode) lives in the resident map and wins over the mapped base.
        if let Some(n) = self.nodes.get(&hash) {
            return Some(std::borrow::Cow::Borrowed(n));
        }
        let base = self.topo_base.as_ref()?;
        let id = base.resolve(hash)?;
        let rec = base.node_record(id)?;
        Some(std::borrow::Cow::Owned(NodeData {
            slug: base.slug_of(id)?.to_string(),
            collection: base
                .collection_name(rec.collection_id)
                .unwrap_or("")
                .to_string(),
            spatial_meta: base.spatial(id).map(|v| geo::SpatialMeta {
                centroid_lat: v[0], centroid_lon: v[1],
                bbox_min_lat: v[2], bbox_min_lon: v[3],
                bbox_max_lat: v[4], bbox_max_lon: v[5],
            }),
            payload_offset: rec.payload_offset,
            payload_len: rec.payload_len,
        }))
    }

    pub(crate) fn collection_name(&self, coll_hash: u64) -> Option<&str> {
        if let Some(s) = self.collection_names_map.get(&coll_hash) {
            return Some(s.as_str());
        }
        self.topo_base.as_ref()?.collection_name_by_hash(coll_hash)
    }

    pub(crate) fn all_hashes(&self) -> Vec<u64> {
        match &self.topo_base {
            None => self.nodes.keys().copied().collect(),
            Some(base) => {
                // Base ∪ overlay (the overlay may hold updates of base nodes —
                // dedup keeps each hash once).
                let mut set: HashSet<u64> = base.all_hashes().into_iter().collect();
                set.extend(self.nodes.keys().copied());
                set.into_iter().collect()
            }
        }
    }

    pub(crate) fn fwd_edges(&self, hash: u64) -> Option<std::borrow::Cow<'_, [Edge]>> {
        Self::merged_edges(self.edges.fwd_edges(hash), || {
            self.topo_base.as_ref().and_then(|b| b.fwd_by_hash(hash))
        })
    }

    pub(crate) fn rev_edges(&self, hash: u64) -> Option<std::borrow::Cow<'_, [Edge]>> {
        Self::merged_edges(self.edges.rev_edges(hash), || {
            self.topo_base.as_ref().and_then(|b| b.rev_by_hash(hash))
        })
    }

    /// Merge overlay edges (resident, written since open) with base edges (mapped).
    /// Resident-only mode short-circuits to a zero-copy borrow.
    fn merged_edges<'a>(
        overlay: Option<&'a [Edge]>,
        base: impl FnOnce() -> Option<Vec<storage::topology::MappedEdge>>,
    ) -> Option<std::borrow::Cow<'a, [Edge]>> {
        let base_edges = base();
        match (overlay, base_edges) {
            (Some(o), None) => Some(std::borrow::Cow::Borrowed(o)),
            (overlay, Some(b)) => {
                let mut v: Vec<Edge> = b
                    .into_iter()
                    .map(|e| Edge::from_base(
                        e.other_hash, e.edge_type_hash, e.meta_ref,
                    ))
                    .collect();
                if let Some(o) = overlay {
                    v.extend_from_slice(o);
                }
                Some(std::borrow::Cow::Owned(v))
            }
            (None, None) => None,
        }
    }

    pub(crate) fn resolve_edge_type(&self, hash: u64) -> Option<String> {
        self.edges.type_name(hash).map(|s| s.to_string())
    }

    /// All of an edge's attributes merged: fast-lane columns + JSON bag, as one
    /// object. This is what read-side views expose — routing is internal.
    pub(crate) fn edge_all_attrs(&self, edge: &Edge) -> Option<Value> {
        let mut map = match self.edge_meta(edge) {
            Some(Value::Object(m)) => m,
            _ => serde_json::Map::new(),
        };
        for (k, v) in self.edges.edge_cols(edge) {
            map.insert(k, v);
        }
        if map.is_empty() { None } else { Some(Value::Object(map)) }
    }

    pub(crate) fn edge_meta(&self, edge: &Edge) -> Option<Value> {
        // Base edges carry an edgemeta.bin reference (high bit); overlay edges
        // resolve through the resident meta store.
        if let Some(meta_ref) = edge.base_meta_ref() {
            let bytes = self.topo_base.as_ref()?.edge_meta_bytes(meta_ref)?;
            return serde_json::from_slice(bytes).ok();
        }
        self.edges.edge_meta(edge)
    }

    /// Look up a single forward edge `from → to` of the given type (`0` = any)
    /// and return its JSON metadata.  Used by the MATCH executor to expose
    /// per-edge properties (`r.field`) in `SELECT … FROM MATCH`.  Reads edge
    /// metadata lazily — only called when a query actually references an
    /// edge-bound variable's field.
    /// Locate one forward edge ONCE and return its `(fast-lane slot, JSON meta)`.
    /// The slot indexes the resident columns directly (`None` for a base/paged
    /// edge, whose attributes live in the JSON meta). One adjacency scan, so the
    /// hot path pays a single locate per edge instead of one per attribute source.
    pub(crate) fn edge_locate(
        &self,
        from: u64,
        to: u64,
        edge_type_hash: u64,
    ) -> Option<(Option<u32>, Option<Value>)> {
        let edges = self.fwd_edges(from)?;
        for e in edges.iter() {
            if e.other == to && (edge_type_hash == 0 || e.edge_type == edge_type_hash) {
                return Some((e.attr_slot(), self.edge_meta(e)));
            }
        }
        None
    }

    /// Create an edge carrying attributes, auto-routed by value type: primitives
    /// (number/bool) → the columnar FAST LANE; everything else → the JSON bag.
    pub fn link_attr(
        &mut self,
        from: &str,
        to: &str,
        edge_type: &str,
        attrs: Vec<(String, Value)>,
    ) {
        // Persist through the WAL like every other edge write: serialise the attrs
        // to a JSON object and hand them to link_meta, which routes primitives to
        // the fast lane and the rest to the JSON bag. On reopen the WAL LinkMeta
        // replays through the same routing, so the columns rebuild exactly.
        let obj: serde_json::Map<String, Value> = attrs.into_iter().collect();
        let json = Value::Object(obj).to_string();
        let _ = self.link_meta(from, to, edge_type, &json);
    }

    /// Auto-route edge attributes by value type: number/bool → fast-lane columns,
    /// everything else → the JSON bag. Shared by the API and the SQL edge-insert.
    fn route_edge_attrs(
        attrs: Vec<(String, Value)>,
    ) -> (Vec<(String, storage::edgestore::ColVal)>, Option<Value>) {
        use storage::edgestore::ColVal;
        let mut cols: Vec<(String, ColVal)> = Vec::new();
        let mut bag = serde_json::Map::new();
        for (name, v) in attrs {
            match v {
                Value::Number(n) => {
                    if let Some(f) = n.as_f64() {
                        cols.push((name, ColVal::Num(f)));
                    }
                }
                Value::Bool(b) => cols.push((name, ColVal::Bool(b))),
                other => {
                    bag.insert(name, other);
                }
            }
        }
        let json = if bag.is_empty() { None } else { Some(Value::Object(bag)) };
        (cols, json)
    }

    pub(crate) fn collection_members(&self, hash: u64) -> Option<std::borrow::Cow<'_, [u64]>> {
        let overlay = self.collections.get(&hash);
        let base = self
            .topo_base
            .as_ref()
            .and_then(|b| b.members_by_coll_hash(hash));
        match (overlay, base) {
            (Some(o), None) => Some(std::borrow::Cow::Borrowed(o.as_slice())),
            (overlay, Some(mut b)) => {
                if let Some(o) = overlay {
                    // Updates of base nodes re-appear in the overlay — dedup so a
                    // collection scan sees each node once.
                    let seen: HashSet<u64> = b.iter().copied().collect();
                    for &h in o {
                        if !seen.contains(&h) {
                            b.push(h);
                        }
                    }
                }
                Some(std::borrow::Cow::Owned(b))
            }
            (None, None) => None,
        }
    }

    /// Return the btree index for `(collection_hash, field)` if one exists.
    /// Used by the query executor for index-only scans (GROUP BY, DISTINCT, etc.).
    pub(crate) fn field_index(
        &self,
        coll_hash: u64,
        field: &str,
    ) -> Option<&BTreeMap<FieldKey, Vec<u64>>> {
        self.field_indexes.get(&(coll_hash, field.to_string()))
    }

    /// Convert a `FieldKey` to a `serde_json::Value` for result projection.
    pub(crate) fn field_key_to_value(key: &FieldKey) -> Value {
        match key {
            FieldKey::Null        => Value::Null,
            FieldKey::Bool(b)     => Value::Bool(*b),
            FieldKey::Number(OrdF64(f)) => {
                serde_json::Number::from_f64(*f)
                    .map(Value::Number)
                    .unwrap_or(Value::Null)
            }
            FieldKey::Str(s)      => Value::String(s.clone()),
        }
    }

    pub(crate) fn spatial_grid(&self) -> Option<&geo::SpatialGrid> {
        self.spatial_grid.as_ref()
    }

    // ── Spatial index ─────────────────────────────────────────────────────────

    /// Build (or rebuild) the spatial grid index from all current nodes.
    ///
    /// Call this after bulk inserts on in-memory databases to enable
    /// grid-accelerated spatial queries. For persistent databases opened
    /// with [`CoreDB::open`], the grid is built automatically.
    pub fn build_spatial_index(&mut self) {
        self.rebuild_spatial_grid();
    }

    fn rebuild_spatial_grid(&mut self) {
        let mut items: Vec<(u64, geo::SpatialMeta)> = self.nodes.iter()
            .filter_map(|(&hash, node)| node.spatial_meta.clone().map(|m| (hash, m)))
            .collect();
        // Paged mode: base nodes live in the mmap, not in self.nodes — pull their
        // spatial records from the side-table (48 B each; only geometry nodes).
        if let Some(base) = &self.topo_base {
            for id in 0..base.node_count() as u64 {
                if let Some(v) = base.spatial(id) {
                    if let Some(h) = base.hash_of(id) {
                        if !self.nodes.contains_key(&h) {
                            items.push((h, geo::SpatialMeta {
                                centroid_lat: v[0], centroid_lon: v[1],
                                bbox_min_lat: v[2], bbox_min_lon: v[3],
                                bbox_max_lat: v[4], bbox_max_lon: v[5],
                            }));
                        }
                    }
                }
            }
        }
        self.spatial_grid = Some(geo::SpatialGrid::build(items.into_iter()));
    }

    // ── Text index ─────────────────────────────────────────────────────────────

    /// Build (or rebuild) GiST trigram indexes for all text fields.
    ///
    /// Automatically detects all string fields across all nodes and builds
    /// a GiST bitmap signature index for each. This is cheap enough to always
    /// have enabled (~12MB/1M docs).
    ///
    /// Call this after bulk inserts to enable ILIKE acceleration.
    /// For persistent databases opened with [`CoreDB::open`], call this manually
    /// after bulk loading data.
    pub fn build_text_indexes(&mut self) {
        self.rebuild_text_indexes();
    }

    fn rebuild_text_indexes(&mut self) {
        let mut field_values: HashMap<String, Vec<(u64, String)>> = HashMap::new();

        for (&hash, node) in &self.nodes {
            if let Some(payload) = self.payload_store.get(node.payload_offset, node.payload_len) {
                extract_string_fields(&payload, "", &mut field_values, hash);
            }
        }

        // Build into a fresh map first, then replace atomically.
        // This ensures that if GiSTIndex::build panics, self.text_indexes
        // retains its previous state and is never left half-cleared.
        let mut new_indexes = HashMap::new();
        for (field, values) in field_values {
            let values_ref: Vec<(u64, &str)> =
                values.iter().map(|(id, s)| (*id, s.as_str())).collect();
            let index = GiSTIndex::build(values_ref.into_iter(), &field);
            new_indexes.insert(field, index);
        }
        self.text_indexes = new_indexes;
    }

    /// Get ILIKE candidate doc IDs from text index for a field.
    ///
    /// Returns `Some(candidates)` if an index exists for this field,
    /// or `None` if no index is available.
    ///
    /// Candidates are unverified — use [`Self::ilike_verify`] to confirm matches.
    pub fn text_index_candidates(&self, field: &str, pattern: &str) -> Option<Vec<u64>> {
        self.text_indexes
            .get(field)
            .map(|idx| idx.ilike_candidates(pattern, None))
    }

    pub fn text_index_candidates_with_limit(
        &self,
        field: &str,
        pattern: &str,
        limit: Option<usize>,
    ) -> Option<Vec<u64>> {
        self.text_indexes
            .get(field)
            .map(|idx| idx.ilike_candidates(pattern, limit))
    }

    /// Get ILIKE candidate doc IDs from text index for a field.
    ///
    /// Returns candidates that MAY match (GiST is lossy — verification needed).
    /// Use [`Self::ilike_verify`] to confirm actual matches.
    pub fn ilike_candidates(&self, field: &str, pattern: &str) -> Vec<u64> {
        self.text_indexes
            .get(field)
            .map(|idx| idx.ilike_candidates(pattern, None))
            .unwrap_or_default()
    }

    /// Verify ILIKE candidates against actual stored text.
    pub fn ilike_verify(&self, field: &str, pattern: &str, candidates: &[u64]) -> Vec<u64> {
        use text_index::query::ilike_matches;
        let mut results = Vec::new();
        for &hash in candidates {
            if let Some(payload) = self.get_payload(hash) {
                if let Some(text) = payload.get(field).and_then(|v| v.as_str()) {
                    if ilike_matches(text, pattern) {
                        results.push(hash);
                    }
                }
            }
        }
        results
    }

    /// Execute an ILIKE query using the text index.
    ///
    /// This is a convenience method that:
    /// 1. Looks up candidates from the GiST index
    /// 2. Verifies each candidate against the actual ILIKE pattern
    /// 3. Returns the verified matching doc IDs
    ///
    /// If no text index exists for the field, returns an empty result.
    ///
    /// # Arguments
    /// * `field` - The field name to search (e.g., "name" or "description")
    /// * `pattern` - ILIKE pattern (e.g., "%Alpha%" or "%foo_bar%")
    /// * `limit` - Maximum results to return (None for all)
    ///
    /// # Example
    /// ```
    /// # use sekejap::CoreDB;
    /// let mut db = CoreDB::new();
    /// db.put("alice", r#"{"name":"Alice Smith","_collection":"users"}"#).unwrap();
    /// db.put("bob",   r#"{"name":"Bob Jones",  "_collection":"users"}"#).unwrap();
    /// db.build_text_indexes();
    ///
    /// let matches = db.ilike("name", "%Alice%", None);
    /// assert_eq!(matches.len(), 1);
    /// ```
    pub fn ilike(&self, field: &str, pattern: &str, limit: Option<usize>) -> Vec<u64> {
        // Prefer GIN (exact) over GiST (lossy) when available
        if let Some(results) = self.gin_indexes.get(field) {
            let mut r = results.ilike(pattern, None);
            if let Some(l) = limit {
                r.truncate(l);
            }
            return r;
        }
        // Fall back to GiST + verification
        let candidates = self.ilike_candidates(field, pattern);
        let verified = self.ilike_verify(field, pattern, &candidates);
        match limit {
            Some(l) => verified.into_iter().take(l).collect(),
            None => verified,
        }
    }

    /// Build a GIN trigram index for a specific field.
    ///
    /// GIN provides exact trigram matching (no verification step needed) but
    /// uses more memory than GiST (~100MB/1M docs vs ~12MB/1M docs).
    ///
    /// Use this when you need exact ILIKE matching without the false-positive
    /// verification step of GiST.
    ///
    /// # Arguments
    /// * `field` - The field name to index (e.g., "name")
    ///
    /// # Example
    /// ```
    /// # use sekejap::CoreDB;
    /// let mut db = CoreDB::new();
    /// db.put("a1", r#"{"name":"Alpha","_collection":"items"}"#).unwrap();
    /// db.put("a2", r#"{"name":"Beta","_collection":"items"}"#).unwrap();
    /// db.build_gin_index("name");
    ///
    /// // GIN is exact — no verification step needed
    /// let matches = db.gin_ilike("name", "%Alpha%", None);
    /// assert_eq!(matches.len(), 1);
    /// ```
    ///
    /// **Durability:** this builds an in-RAM index only — it is **not** persisted
    /// and will be gone after reopen. For a durable index use the SQL DDL
    /// `CREATE INDEX ON <collection> USING gin (<field>)`, which records the
    /// declaration and rebuilds on open.
    pub fn build_gin_index(&mut self, field: &str) {
        let owned: Vec<(u64, String)> = self
            .nodes
            .iter()
            .filter_map(|(&hash, node)| {
                let payload = self.payload_store.get(node.payload_offset, node.payload_len)?;
                payload.get(field)?.as_str().map(|s| (hash, s.to_string()))
            })
            .collect();
        if !owned.is_empty() {
            let refs: Vec<(u64, &str)> = owned.iter().map(|(h, s)| (*h, s.as_str())).collect();
            let index = GINIndex::build(refs.into_iter(), field);
            self.gin_indexes.insert(field.to_string(), index);
        }
        self.record_index_version("gin", field, GIN_INDEX_VERSION);
    }

    /// Execute ILIKE using GIN index (exact — no verification needed).
    ///
    /// Returns exact matching doc IDs directly from the GIN index.
    /// If no GIN index exists for the field, returns an empty result.
    ///
    /// # Arguments
    /// * `field` - The field name to search
    /// * `pattern` - ILIKE pattern (e.g., "%Alpha%")
    /// * `limit` - Maximum results (None for all)
    pub fn gin_ilike(&self, field: &str, pattern: &str, limit: Option<usize>) -> Vec<u64> {
        // Belt-and-suspenders: filter out hashes whose nodes were deleted after
        // the GIN index was last built.  Mirrors the same guard in bm25_search().
        self.gin_indexes
            .get(field)
            .map(|idx| idx.ilike(pattern, limit))
            .unwrap_or_default()
            .into_iter()
            .filter(|h| self.nodes.contains_key(h))
            .collect()
    }

    // ── BM25 full-text search ───────────────────────────────────────────────

    /// Build a BM25 index for a specific text field.
    ///
    /// BM25 provides relevance-ranked results (like Google search) instead of
    /// exact substring matching. The index is compressed with varint encoding.
    ///
    /// # Arguments
    /// * `field` - The text field to index (e.g., "name", "description")
    ///
    /// # Storage
    /// Approximately 100-150 MB per 1M documents for a typical text field.
    ///
    /// # Example
    /// ```
    /// # use sekejap::CoreDB;
    /// let mut db = CoreDB::new();
    /// db.put("a1", r#"{"name":"Rust Tutorial for Beginners","_collection":"tutorials"}"#).unwrap();
    /// db.put("a2", r#"{"name":"Python Tutorial for Beginners","_collection":"tutorials"}"#).unwrap();
    /// db.build_bm25_index("name");
    ///
    /// let results = db.bm25_search("name", "rust tutorial", 10);
    /// assert!(results.len() >= 1);
    /// // The top result should be the doc that best matches all query terms.
    /// ```
    /// **Durability:** builds an in-RAM index only — **not** persisted, gone
    /// after reopen. For a durable index use `CREATE INDEX ON <collection>
    /// USING bm25 (<field>)`, which records the declaration and rebuilds on open.
    pub fn build_bm25_index(&mut self, field: &str) {
        let owned: Vec<(u64, String)> = self
            .nodes
            .iter()
            .filter_map(|(&hash, node)| {
                let payload = self.payload_store.get(node.payload_offset, node.payload_len)?;
                payload.get(field)?.as_str().map(|s| (hash, s.to_string()))
            })
            .collect();
        let refs: Vec<(u64, &str)> = owned.iter().map(|(h, s)| (*h, s.as_str())).collect();
        let index = bm25::Bm25Index::build(field, refs.into_iter());
        self.bm25_indexes.insert(field.to_string(), index);
        self.record_index_version("bm25", field, BM25_INDEX_VERSION);
    }

    /// Search the BM25 index for `field` and return the top-`top_k`
    /// results ranked by relevance score (highest first).
    ///
    /// Requires [`build_bm25_index`](Self::build_bm25_index) to have
    /// been called for `field`.  Returns an empty `Vec` if the index
    /// does not exist or the query produces no matches.
    ///
    /// # Deletion safety
    ///
    /// Two complementary guards ensure deleted documents never appear:
    ///
    /// 1. **Inside the index** — [`Bm25Index::delete`] is called by
    ///    [`remove`](Self::remove) and removes the document's entry
    ///    from `doc_id_to_idx`, so it can never score in `search`.
    /// 2. **Here** — results are filtered through `self.nodes` as a
    ///    belt-and-suspenders check covering any narrow window between
    ///    a node deletion and the BM25 index update.
    ///
    /// # Returns
    ///
    /// `Vec<(doc_id, score)>` — `doc_id` is `sk_hash(slug)`.
    ///
    /// # Example
    ///
    /// ```
    /// # use sekejap::CoreDB;
    /// let mut db = CoreDB::new();
    /// db.put("a1", r#"{"name":"Rust Tutorial for Beginners","_collection":"tutorials"}"#).unwrap();
    /// db.put("a2", r#"{"name":"Python Tutorial for Beginners","_collection":"tutorials"}"#).unwrap();
    /// db.build_bm25_index("name");
    ///
    /// let results = db.bm25_search("name", "rust tutorial", 10);
    /// // results[0] is the most relevant doc — deleted docs never appear
    /// ```
    pub fn bm25_search(&self, field: &str, query: &str, top_k: usize) -> Vec<(u64, f64)> {
        self.bm25_indexes
            .get(field)
            .map(|idx| {
                idx.search(query, top_k)
                    .into_iter()
                    // Belt-and-suspenders: exclude any doc not present in
                    // the live node map, covering the narrow window between
                    // node deletion and BM25 index update.
                    .filter(|hit| self.nodes.contains_key(&hit.doc_id))
                    .map(|hit| (hit.doc_id, hit.score))
                    .collect()
            })
            .unwrap_or_default()
    }

    // ── Vector storage ─────────────────────────────────────────────────────────

    /// Store a vector for a node under a named field.
    ///
    /// The vector is indexed by `sk_hash(slug)` and persisted in the WAL
    /// when the database is opened from disk.
    ///
    /// Returns the slug hash on success.
    pub fn put_vector(&mut self, slug: &str, field: &str, data: &[f32]) -> Result<u64, serde_json::Error> {
        self.wal_write(WalEntry::PutVector {
            slug: slug.to_string(),
            field: field.to_string(),
            data: data.to_vec(),
        });
        let hash = sk_hash(slug);
        self.ensure_vector_store(field);
        self.vectors.get_mut(field).unwrap().put(hash, data.to_vec());
        #[cfg(unix)]
        self.vectors.get_mut(field).unwrap().remap();
        let hnsw_declared = self.schemas.values()
            .any(|s| s.indexes.vector.contains(&field.to_string()));
        if hnsw_declared {
            let (m, ef) = self.hnsw_params.get(field).copied().unwrap_or((16, 200));
            let field_vecs = self.vectors.get(field).unwrap();
            let graph = self.hnsw_indexes
                .entry(field.to_string())
                .or_insert_with(|| vector::HnswGraph::empty(m));
            graph.insert::<CosineDistance, _>(hash, field_vecs, ef);
        }
        Ok(hash)
    }

    /// Retrieve the stored vector for a node under a named field.
    ///
    /// Returns `None` if the node has no vector for that field.
    pub fn get_vector(&self, slug: &str, field: &str) -> Option<&[f32]> {
        let hash = sk_hash(slug);
        use crate::vector::VectorAccess;
        self.vectors.get(field)?.get(hash)
    }

    /// Access all vectors for a given field (used by the query executor).
    pub(crate) fn vector_field(&self, field: &str) -> Option<&storage::vecstore::VectorStore> {
        self.vectors.get(field)
    }

    /// Access the HNSW index for a field (used by the query executor).
    pub(crate) fn hnsw_index(&self, field: &str) -> Option<&vector::HnswGraph> {
        self.hnsw_indexes.get(field)
    }

    /// Ensure a VectorStore exists for `field`. Creates a disk-backed store
    /// when a data directory is configured, otherwise a memory-backed one.
    fn ensure_vector_store(&mut self, field: &str) {
        if self.vectors.contains_key(field) {
            return;
        }
        #[cfg(unix)]
        if let Some(ref dir) = self.data_dir {
            if let Ok(store) = storage::vecstore::VectorStore::open_disk(dir, field) {
                self.vectors.insert(field.to_string(), store);
                return;
            }
        }
        self.vectors.insert(field.to_string(), storage::vecstore::VectorStore::new());
    }

    // ── Btree field index ──────────────────────────────────────────────────────

    /// Build (or rebuild) a btree field index for a specific collection and field.
    ///
    /// Scans all collection members and builds an ordered BTreeMap from field
    /// value → `[node_hash, …]`. Called automatically by
    /// `CREATE INDEX ON coll(field) USING btree`.
    ///
    /// Incrementally maintained by every subsequent `put()` / `remove()`.
    pub fn build_field_index(&mut self, collection: &str, field: &str) {
        let coll_hash = sk_hash(collection);
        let members: Vec<u64> = self.collections.get(&coll_hash).cloned().unwrap_or_default();
        let mut btree: BTreeMap<FieldKey, Vec<u64>> = BTreeMap::new();
        for hash in members {
            if let Some(node) = self.nodes.get(&hash) {
                let payload = self.payload_store.get(node.payload_offset, node.payload_len)
                    .unwrap_or(Value::Null);
                if let Some(fk) = FieldKey::from_json(payload.get(field).unwrap_or(&Value::Null)) {
                    btree.entry(fk).or_default().push(hash);
                }
            }
        }
        self.field_indexes.insert((coll_hash, field.to_string()), btree);
        self.record_index_version("btree", field, BTREE_INDEX_VERSION);
    }

    /// Record the build version for an index in every schema that declares it.
    ///
    /// Key format: `"method:field"` (e.g. `"gin:name"`, `"btree:price"`).
    fn record_index_version(&mut self, method: &str, field: &str, version: u32) {
        for schema in self.schemas.values_mut() {
            let declares = match method {
                "gin"   => schema.indexes.fulltext.contains(&field.to_string()),
                "bm25"  => schema.indexes.bm25.contains(&field.to_string()),
                "btree" => schema.indexes.range.contains(&field.to_string()),
                _       => false,
            };
            if declares {
                schema.indexes.build_versions
                    .insert(format!("{}:{}", method, field), version);
            }
        }
    }

    /// Try to seed the candidate list for a `Collection` step from a btree index.
    ///
    /// Looks ahead in `remaining` for the first filter step that has a btree
    /// index on this collection. Returns `(candidates, skip_idx)` on a hit,
    /// where `skip_j` is the index in `remaining` of the step that was consumed
    /// (so the caller can skip it in the main pipeline loop). The optional third
    /// element is a second consumed step index (e.g. the upper-bound companion
    /// for a two-sided range like `WhereGt + WhereLte`). Returns `None` to fall
    /// back to a full collection scan.
    pub(crate) fn btree_seed(
        &self,
        coll_hash: u64,
        remaining: &[Step],
    ) -> Option<(Vec<u64>, usize, Option<usize>)> {
        use std::ops::Bound;
        for (j, step) in remaining.iter().enumerate() {
            match step {
                Step::WhereEq(field, value) => {
                    if let Some(idx) = self.field_indexes.get(&(coll_hash, field.clone())) {
                        if let Some(fk) = FieldKey::from_json(value) {
                            return Some((idx.get(&fk).cloned().unwrap_or_default(), j, None));
                        }
                    }
                }
                Step::WhereNeq(field, value) => {
                    if let Some(idx) = self.field_indexes.get(&(coll_hash, field.clone())) {
                        if let Some(fk) = FieldKey::from_json(value) {
                            // Set-difference: all collection members minus those matching value.
                            let excluded: std::collections::HashSet<u64> = idx
                                .get(&fk)
                                .map(|ids| ids.iter().copied().collect())
                                .unwrap_or_default();
                            let all = self.collections
                                .get(&coll_hash)
                                .cloned()
                                .unwrap_or_default();
                            return Some((
                                all.into_iter().filter(|h| !excluded.contains(h)).collect(),
                                j,
                                None,
                            ));
                        }
                    }
                }
                Step::WhereGt(field, lo) => {
                    if let Some(idx) = self.field_indexes.get(&(coll_hash, field.clone())) {
                        let fk_lo = FieldKey::from_f64(*lo);
                        // Look ahead: combine with WhereLte/WhereLt on same field into
                        // a single btree range scan, consuming both steps.
                        let upper = remaining[j + 1..].iter().enumerate().find_map(|(k, s)| {
                            match s {
                                Step::WhereLte(f2, hi) if f2 == field =>
                                    Some((j + 1 + k, Bound::Included(FieldKey::from_f64(*hi)))),
                                Step::WhereLt(f2, hi) if f2 == field =>
                                    Some((j + 1 + k, Bound::Excluded(FieldKey::from_f64(*hi)))),
                                _ => None,
                            }
                        });
                        return if let Some((pair_j, upper_bound)) = upper {
                            Some((
                                idx.range((Bound::Excluded(fk_lo), upper_bound))
                                    .flat_map(|(_, ids)| ids.iter().copied())
                                    .collect(),
                                j,
                                Some(pair_j),
                            ))
                        } else {
                            Some((
                                idx.range((Bound::Excluded(fk_lo), Bound::Unbounded))
                                    .flat_map(|(_, ids)| ids.iter().copied())
                                    .collect(),
                                j,
                                None,
                            ))
                        };
                    }
                }
                Step::WhereLt(field, hi) => {
                    if let Some(idx) = self.field_indexes.get(&(coll_hash, field.clone())) {
                        let fk_hi = FieldKey::from_f64(*hi);
                        // Look ahead for lower bound on same field.
                        let lower = remaining[j + 1..].iter().enumerate().find_map(|(k, s)| {
                            match s {
                                Step::WhereGte(f2, lo) if f2 == field =>
                                    Some((j + 1 + k, Bound::Included(FieldKey::from_f64(*lo)))),
                                Step::WhereGt(f2, lo) if f2 == field =>
                                    Some((j + 1 + k, Bound::Excluded(FieldKey::from_f64(*lo)))),
                                _ => None,
                            }
                        });
                        return if let Some((pair_j, lower_bound)) = lower {
                            Some((
                                idx.range((lower_bound, Bound::Excluded(fk_hi)))
                                    .flat_map(|(_, ids)| ids.iter().copied())
                                    .collect(),
                                j,
                                Some(pair_j),
                            ))
                        } else {
                            Some((
                                idx.range(..fk_hi)
                                    .flat_map(|(_, ids)| ids.iter().copied())
                                    .collect(),
                                j,
                                None,
                            ))
                        };
                    }
                }
                Step::WhereGte(field, lo) => {
                    if let Some(idx) = self.field_indexes.get(&(coll_hash, field.clone())) {
                        let fk_lo = FieldKey::from_f64(*lo);
                        let upper = remaining[j + 1..].iter().enumerate().find_map(|(k, s)| {
                            match s {
                                Step::WhereLte(f2, hi) if f2 == field =>
                                    Some((j + 1 + k, Bound::Included(FieldKey::from_f64(*hi)))),
                                Step::WhereLt(f2, hi) if f2 == field =>
                                    Some((j + 1 + k, Bound::Excluded(FieldKey::from_f64(*hi)))),
                                _ => None,
                            }
                        });
                        return if let Some((pair_j, upper_bound)) = upper {
                            Some((
                                idx.range((Bound::Included(fk_lo), upper_bound))
                                    .flat_map(|(_, ids)| ids.iter().copied())
                                    .collect(),
                                j,
                                Some(pair_j),
                            ))
                        } else {
                            Some((
                                idx.range(fk_lo..)
                                    .flat_map(|(_, ids)| ids.iter().copied())
                                    .collect(),
                                j,
                                None,
                            ))
                        };
                    }
                }
                Step::WhereLte(field, hi) => {
                    if let Some(idx) = self.field_indexes.get(&(coll_hash, field.clone())) {
                        let fk_hi = FieldKey::from_f64(*hi);
                        let lower = remaining[j + 1..].iter().enumerate().find_map(|(k, s)| {
                            match s {
                                Step::WhereGte(f2, lo) if f2 == field =>
                                    Some((j + 1 + k, Bound::Included(FieldKey::from_f64(*lo)))),
                                Step::WhereGt(f2, lo) if f2 == field =>
                                    Some((j + 1 + k, Bound::Excluded(FieldKey::from_f64(*lo)))),
                                _ => None,
                            }
                        });
                        return if let Some((pair_j, lower_bound)) = lower {
                            Some((
                                idx.range((lower_bound, Bound::Included(fk_hi)))
                                    .flat_map(|(_, ids)| ids.iter().copied())
                                    .collect(),
                                j,
                                Some(pair_j),
                            ))
                        } else {
                            Some((
                                idx.range(..=fk_hi)
                                    .flat_map(|(_, ids)| ids.iter().copied())
                                    .collect(),
                                j,
                                None,
                            ))
                        };
                    }
                }
                Step::WhereBetween(field, lo, hi) => {
                    if let Some(idx) = self.field_indexes.get(&(coll_hash, field.clone())) {
                        let fk_lo = FieldKey::from_f64(*lo);
                        let fk_hi = FieldKey::from_f64(*hi);
                        return Some((
                            idx.range(fk_lo..=fk_hi)
                                .flat_map(|(_, ids)| ids.iter().copied())
                                .collect(),
                            j,
                            None,
                        ));
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Try to seed the candidate list for a `Collection` step using an ORDER BY index scan.
    ///
    /// Applies when there are **no filter steps** between `Collection` and `Sort`, the
    /// sort is single-column, and a btree index exists on that field.  Returns the
    /// pre-sorted candidates **and** the index of the `Sort` step in `remaining`
    /// (so the caller can add it to `skip_set` — data is already sorted, no
    /// payload-reading re-sort needed).
    pub(crate) fn btree_sorted_seed_from_steps(
        &self,
        coll_hash: u64,
        remaining: &[Step],
    ) -> Option<(Vec<u64>, usize)> {
        // Find the first Sort step
        let sort_pos = remaining.iter().position(|s| matches!(s, Step::Sort(_)))?;

        // Only valid when there are no filter/traversal steps before Sort
        let pre_sort = &remaining[..sort_pos];
        if pre_sort.iter().any(|s| is_filter_or_traversal(s)) {
            return None;
        }

        let Step::Sort(cols) = &remaining[sort_pos] else {
            return None;
        };
        // Only single-column sort qualifies (multi-column can't use a single btree)
        if cols.len() != 1 {
            return None;
        }
        let (field, asc) = &cols[0];

        let idx = self.field_indexes.get(&(coll_hash, field.clone()))?;

        // Look ahead for a Take limit — enables O(k) extraction instead of O(N)
        let take_n = remaining[sort_pos + 1..]
            .iter()
            .find_map(|s| if let Step::Take(n) = s { Some(*n) } else { None });

        let result: Vec<u64> = if *asc {
            idx.values().flat_map(|ids| ids.iter().copied()).collect()
        } else {
            idx.values().rev().flat_map(|ids| ids.iter().copied()).collect()
        };

        let candidates = match take_n {
            Some(n) => result.into_iter().take(n).collect(),
            None => result,
        };
        Some((candidates, sort_pos))
    }

    /// Build (or rebuild) an HNSW approximate-NN index for a vector field.
    ///
    /// The index is constructed entirely in a local value and only stored after
    /// successful completion — the main store (`self.vectors`, `self.nodes`)
    /// is never modified by this call.
    ///
    /// # Parameters
    /// - `field`: the vector field name (must have been populated via `put_vector`)
    /// - `m`: max connections per node (8–32; 16 is a good default)
    /// - `ef_construction`: beam width during build (100–400; 200 is a good default)
    ///
    /// Returns `Err` if `field` has no stored vectors.
    pub fn build_hnsw_index(
        &mut self,
        field: &str,
        m: usize,
        ef_construction: usize,
    ) -> Result<(), String> {
        // Ensure mmap covers any recently-appended vectors.
        #[cfg(unix)]
        if let Some(store) = self.vectors.get_mut(field) {
            store.remap();
        }
        let field_vecs = self
            .vectors
            .get(field)
            .ok_or_else(|| format!("no vectors stored for field '{field}'"))?;

        // Build entirely into a local — zero writes to self until this line.
        let graph =
            vector::HnswGraph::build::<CosineDistance, _>(field_vecs, m, ef_construction);

        // Atomic replace: old index (if any) is dropped here.
        self.hnsw_indexes.insert(field.to_string(), graph);
        self.hnsw_params.insert(field.to_string(), (m, ef_construction));
        Ok(())
    }

    // ── CREATE INDEX executor ──────────────────────────────────────────────────

    /// Build the in-memory index for a `CREATE INDEX` statement and update
    /// the collection schema's index hints.
    fn apply_index(
        &mut self,
        collection: &str,
        method: &sql::IndexMethod,
        fields: &[String],
    ) -> Result<(), sql::SqlError> {
        use sql::IndexMethod;

        // Update schema index hints so introspection always reflects reality.
        let schema = self.schemas
            .entry(collection.to_string())
            .or_insert_with(|| sql::TableSchema {
                collection: collection.to_string(),
                fields: vec![],
                indexes: sql::IndexHint::default(),
            });
        if matches!(method, IndexMethod::Search) {
            let field_list: Vec<String> = fields.to_vec();
            if !schema.indexes.search.contains(&field_list) {
                schema.indexes.search.push(field_list);
            }
        } else {
            for field in fields {
                let list = match method {
                    IndexMethod::Bm25    => &mut schema.indexes.bm25,
                    IndexMethod::Hnsw    => &mut schema.indexes.vector,
                    IndexMethod::Spatial => &mut schema.indexes.spatial,
                    IndexMethod::Gin | IndexMethod::Gist => &mut schema.indexes.fulltext,
                    IndexMethod::Btree   => &mut schema.indexes.range,
                    IndexMethod::Hash    => &mut schema.indexes.hash,
                    IndexMethod::Search  => unreachable!(),
                };
                if !list.contains(field) {
                    list.push(field.clone());
                }
            }
        }

        // Build the actual in-memory index structure.
        //
        // During WAL replay, skip HNSW / GIN / BM25 / spatial / gist builds.
        // open() rebuilds those once at the end (rebuild_declared_hnsw_indexes,
        // rebuild_declared_gin_indexes, rebuild_spatial_grid).
        // Without this guard, each CreateIndex(hnsw) WAL entry would rebuild
        // the entire HNSW graph — with N tables sharing the same field name
        // that's N redundant full rebuilds on the same vectors.
        //
        // Btree and Hash must still build during replay because put_raw()
        // maintains them incrementally and needs the field_indexes populated.
        match method {
            IndexMethod::Hnsw => {
                if !self.replaying {
                    for field in fields {
                        let _ = self.build_hnsw_index(field, 16, 200);
                    }
                }
            }
            IndexMethod::Bm25 => {
                if !self.replaying {
                    for field in fields {
                        self.build_bm25_index(field);
                    }
                }
            }
            IndexMethod::Gin => {
                if !self.replaying {
                    for field in fields {
                        self.build_gin_index(field);
                    }
                }
            }
            IndexMethod::Gist => {
                if !self.replaying {
                    self.rebuild_text_indexes();
                }
            }
            IndexMethod::Spatial => {
                if !self.replaying {
                    self.rebuild_spatial_grid();
                }
            }
            IndexMethod::Btree => {
                for field in fields {
                    self.build_field_index(collection, field);
                }
            }
            IndexMethod::Hash => {
                for field in fields {
                    self.build_field_index(collection, field);
                }
            }
            IndexMethod::Search => {
                if !self.replaying {
                    self.build_search_index(collection, fields);
                }
            }
        }

        Ok(())
    }

    /// Rebuild all declared GIN indexes from all currently loaded nodes.
    /// Rebuild all declared BM25 indexes from current data.
    ///
    /// Called after WAL replay in `open()` because BM25 builds are skipped
    /// during replay (apply_index guards on self.replaying).
    fn rebuild_declared_bm25_indexes(&mut self) {
        let fields: Vec<String> = {
            let mut seen = std::collections::HashSet::new();
            self.schemas.values()
                .flat_map(|s| s.indexes.bm25.iter().cloned())
                .filter(|f| seen.insert(f.clone()))
                .collect()
        };
        for field in fields {
            self.build_bm25_index(&field);
        }
    }

    ///
    /// Called after WAL replay in `open()` to ensure GIN is fresh regardless
    /// of the order in which WAL entries were written (e.g. CreateIndex before Put).
    fn rebuild_declared_gin_indexes(&mut self) {
        let fields: Vec<String> = {
            let mut seen = std::collections::HashSet::new();
            self.schemas.values()
                .flat_map(|s| s.indexes.fulltext.iter().cloned())
                .filter(|f| seen.insert(f.clone()))
                .collect()
        };
        for field in fields {
            self.build_gin_index(&field);
        }
    }

    /// Canonical key for a search index: fields joined with "+".
    pub(crate) fn search_index_key(coll_name: &str) -> String {
        // We store one search index per collection, keyed by collection name.
        // If multiple USING search indexes exist on the same collection with
        // different field sets, later ones overwrite earlier ones.
        coll_name.to_string()
    }

    /// Build a positional search index for a collection.
    fn build_search_index(&mut self, collection: &str, fields: &[String]) {
        let coll_hash = sk_hash(collection);
        let members = match self.collections.get(&coll_hash) {
            Some(m) => m.clone(),
            None => return,
        };

        let docs = members.iter().filter_map(|&hash| {
            let node = self.nodes.get(&hash)?;
            let payload = self.get_payload(hash)?;
            let _ = node; // ensure node exists
            let field_values: Vec<String> = fields.iter().map(|f| {
                payload.get(f)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            }).collect();
            Some(search::index::DocFields { hash, field_values })
        });

        let idx = search::SearchIndex::build(fields.to_vec(), docs);
        let key = Self::search_index_key(collection);
        self.search_indexes.insert(key, idx);
    }

    /// Rebuild all declared search indexes from current data.
    fn rebuild_declared_search_indexes(&mut self) {
        let declared: Vec<(String, Vec<String>)> = self.schemas.values()
            .flat_map(|s| {
                s.indexes.search.iter().map(|fields| (s.collection.clone(), fields.clone()))
            })
            .collect();
        for (coll, fields) in declared {
            self.build_search_index(&coll, &fields);
        }
    }

    fn save_search_binary(&self, path: &std::path::Path) -> io::Result<()> {
        use std::io::Write;
        if self.search_indexes.is_empty() {
            return Ok(());
        }
        let tmp = path.with_extension("bin.tmp");
        let mut f = std::io::BufWriter::new(
            std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?
        );
        f.write_all(b"SKSRCH01")?;
        f.write_all(&(self.search_indexes.len() as u32).to_le_bytes())?;
        for (key, idx) in &self.search_indexes {
            let key_bytes = key.as_bytes();
            f.write_all(&(key_bytes.len() as u16).to_le_bytes())?;
            f.write_all(key_bytes)?;
            idx.write_binary(&mut f)?;
        }
        f.flush()?;
        f.get_ref().sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    fn load_search_binary(&mut self, path: &std::path::Path) -> bool {
        use std::io::Read;
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(_) => return false,
        };
        if data.len() < 12 || &data[..8] != b"SKSRCH01" {
            return false;
        }
        let mut cursor = std::io::Cursor::new(&data[8..]);
        let mut count_buf = [0u8; 4];
        if cursor.read_exact(&mut count_buf).is_err() { return false; }
        let count = u32::from_le_bytes(count_buf) as usize;
        let mut loaded = HashMap::new();
        for _ in 0..count {
            let mut key_len_buf = [0u8; 2];
            if cursor.read_exact(&mut key_len_buf).is_err() { return false; }
            let key_len = u16::from_le_bytes(key_len_buf) as usize;
            let mut key_buf = vec![0u8; key_len];
            if cursor.read_exact(&mut key_buf).is_err() { return false; }
            let key = match String::from_utf8(key_buf) {
                Ok(k) => k,
                Err(_) => return false,
            };
            match search::SearchIndex::read_binary(&mut cursor) {
                Ok(idx) => { loaded.insert(key, idx); }
                Err(_) => return false,
            }
        }
        for (key, idx) in loaded {
            self.search_indexes.insert(key, idx);
        }
        true
    }

    /// Rebuild all declared HNSW indexes from all currently loaded vectors.
    ///
    /// Called after WAL replay in `open()` so that vectors written after the
    /// original `CREATE INDEX` are incorporated.
    fn rebuild_declared_hnsw_indexes(&mut self) {
        let params: Vec<(String, usize, usize)> = {
            let mut seen = std::collections::HashSet::new();
            self.schemas.values()
                .flat_map(|s| s.indexes.vector.iter().cloned())
                .filter(|f| seen.insert(f.clone()))
                .map(|f| {
                    let (m, ef) = self.hnsw_params.get(&f).copied().unwrap_or((16, 200));
                    (f, m, ef)
                })
                .collect()
        };
        for (field, m, ef) in params {
            let _ = self.build_hnsw_index(&field, m, ef);
        }
    }
}

// ── Schema validation ─────────────────────────────────────────────────────────

/// Returns true when `v` is compatible with the declared field type.
/// NULL is always accepted; the check is intentionally lenient (e.g. any
/// JSON number passes for both Integer and Real).
fn field_type_matches(ty: &sql::FieldType, v: &Value) -> bool {
    if v.is_null() {
        return true;
    }
    match ty {
        sql::FieldType::Text        => v.is_string(),
        sql::FieldType::Integer     => v.is_number(),
        sql::FieldType::Real        => v.is_number(),
        sql::FieldType::Timestamptz => v.is_string() || v.is_number(),
        sql::FieldType::Bool        => v.is_boolean(),
        sql::FieldType::Geo         => v.is_object() || v.is_array(),
        sql::FieldType::Vector      => v.is_array(),
        sql::FieldType::Json        => true,
    }
}

/// Validate all fields in `payload` that have a matching declaration in `schema`.
/// Unknown/missing fields are silently ignored (lenient / open-world).
/// Returns `Some(SqlError)` on the first type mismatch; `None` when valid.
fn validate_payload_against_schema(schema: &sql::TableSchema, payload: &Value) -> Option<SqlError> {
    let obj = payload.as_object()?;
    for field_def in &schema.fields {
        if let Some(v) = obj.get(&field_def.name) {
            if !field_type_matches(&field_def.ty, v) {
                return Some(SqlError::InvalidValue(format!(
                    "field '{}': expected {:?}, got {}",
                    field_def.name,
                    field_def.ty,
                    v,
                )));
            }
        }
    }
    None
}

/// Validate the (field, value) pairs being written by an UPDATE statement.
/// Only fields declared in the schema are checked; unknown fields are ignored.
fn validate_updates_against_schema(
    schema: &sql::TableSchema,
    updates: &[(String, Value)],
) -> Option<SqlError> {
    for (field, value) in updates {
        if let Some(field_def) = schema.fields.iter().find(|f| &f.name == field) {
            if !field_type_matches(&field_def.ty, value) {
                return Some(SqlError::InvalidValue(format!(
                    "field '{}': expected {:?}, got {}",
                    field_def.name,
                    field_def.ty,
                    value,
                )));
            }
        }
    }
    None
}

/// Returns true for any step that narrows, reorders, or re-sources the candidate list.
/// Used to detect whether a btree ORDER BY index scan is safe for `Collection → Sort`.
fn is_filter_or_traversal(s: &Step) -> bool {
    matches!(
        s,
        Step::WhereEq(..)
            | Step::WhereNeq(..)
            | Step::WhereGt(..)
            | Step::WhereLt(..)
            | Step::WhereGte(..)
            | Step::WhereLte(..)
            | Step::WhereBetween(..)
            | Step::WhereIn(..)
            | Step::Like(..)
            | Step::WhereNot(..)
            | Step::WhereOr(..)
            | Step::WhereIsNull(..)
            | Step::Forward(..)
            | Step::Backward(..)
            | Step::Hops(..)
            | Step::HopsTyped { .. }
            | Step::Leaves
            | Step::Roots
            | Step::StDWithin(..)
            | Step::StContainsPoint(..)
            | Step::StWithin(..)
            | Step::StContains(..)
            | Step::StIntersects(..)
            | Step::StDistance(..)
            | Step::StLength(..)
            | Step::StArea(..)
            | Step::VectorNear { .. }
            | Step::Bm25Filter(..)
            | Step::Intersect(..)
            | Step::Union(..)
            | Step::Subtract(..)
    )
}

// ── Transaction ───────────────────────────────────────────────────────────────

/// A buffered write transaction. Writes are visible **only after [`commit`](Transaction::commit)**.
///
/// Obtained from [`CoreDB::begin`]. Drop to roll back silently.
///
/// # Example
/// ```
/// # use sekejap::CoreDB;
/// let mut db = CoreDB::new();
/// let mut txn = db.begin();
/// txn.put("users/alice", r#"{"_collection":"users","name":"Alice"}"#).unwrap();
/// txn.put("users/bob",   r#"{"_collection":"users","name":"Bob"}"#).unwrap();
/// txn.commit().unwrap();
/// assert_eq!(db.collection("users").count(), 2);
/// ```
pub struct Transaction<'db> {
    db: &'db mut CoreDB,
    ops: Vec<TxnOp>,
}

enum TxnOp {
    Put(String, String),
    Remove(String),
    Link(String, String, String),
    LinkMeta(String, String, String, String),
    Unlink(String, String, String),
    PutVector(String, String, Vec<f32>),
}

impl CoreDB {
    /// Begin a new transaction. Writes are buffered until [`Transaction::commit`].
    ///
    /// Dropping the returned `Transaction` without calling `commit` is a silent rollback.
    pub fn begin(&mut self) -> Transaction<'_> {
        Transaction { db: self, ops: Vec::new() }
    }
}

impl<'db> Transaction<'db> {
    /// Queue a node insert/update. Validates JSON immediately; returns error on bad JSON.
    pub fn put(&mut self, slug: &str, payload_json: &str) -> Result<(), serde_json::Error> {
        serde_json::from_str::<Value>(payload_json)?;
        self.ops.push(TxnOp::Put(slug.to_string(), payload_json.to_string()));
        Ok(())
    }

    /// Queue a node removal.
    pub fn remove(&mut self, slug: &str) {
        self.ops.push(TxnOp::Remove(slug.to_string()));
    }

    /// Queue an edge creation.
    pub fn link(&mut self, from: &str, to: &str, edge_type: &str) {
        self.ops.push(TxnOp::Link(
            from.to_string(), to.to_string(), edge_type.to_string(),
        ));
    }

    /// Queue an edge creation with JSON metadata. Validates JSON immediately.
    pub fn link_meta(
        &mut self,
        from: &str,
        to: &str,
        edge_type: &str,
        meta_json: &str,
    ) -> Result<(), serde_json::Error> {
        serde_json::from_str::<Value>(meta_json)?;
        self.ops.push(TxnOp::LinkMeta(
            from.to_string(), to.to_string(), edge_type.to_string(), meta_json.to_string(),
        ));
        Ok(())
    }

    /// Queue an edge removal.
    pub fn unlink(&mut self, from: &str, to: &str, edge_type: &str) {
        self.ops.push(TxnOp::Unlink(
            from.to_string(), to.to_string(), edge_type.to_string(),
        ));
    }

    /// Queue a vector store.
    pub fn put_vector(&mut self, slug: &str, field: &str, data: Vec<f32>) {
        self.ops.push(TxnOp::PutVector(slug.to_string(), field.to_string(), data));
    }

    /// Commit all queued writes atomically: apply to in-memory store then flush to WAL.
    ///
    /// Returns the number of operations committed.
    ///
    /// # Errors
    /// Only fails if a queued `Put` payload is invalid JSON (shouldn't happen if
    /// the `put()` helper was used, since it validates eagerly).
    pub fn commit(self) -> Result<usize, serde_json::Error> {
        let count = self.ops.len();
        // Apply all ops to in-memory store in order
        for op in &self.ops {
            match op {
                TxnOp::Put(slug, json) => { self.db.put_raw(slug, json)?; }
                TxnOp::Remove(slug) => { self.db.remove_raw(slug); }
                TxnOp::Link(from, to, et) => {
                    self.db.link_raw(from, to, et);
                }
                TxnOp::LinkMeta(from, to, et, meta) => {
                    self.db.link_meta_raw(from, to, et, meta)?;
                }
                TxnOp::Unlink(from, to, et) => { self.db.unlink_raw(from, to, et); }
                TxnOp::PutVector(slug, field, data) => {
                    let hash = sk_hash(slug);
                    self.db.ensure_vector_store(field);
                    self.db.vectors.get_mut(field).unwrap().put(hash, data.clone());
                }
            }
        }
        // Write all ops to the WAL as one batch with a SINGLE fsync at the end.
        // A transaction is atomic-or-nothing, so per-op fsync is both wrong
        // (partial durability) and catastrophically slow (fsync/edge). Defer the
        // sync across the batch, then flush once — crash before the flush loses
        // the whole transaction, which is exactly the contract.
        self.db.defer_wal_sync = true;
        for op in self.ops {
            match op {
                TxnOp::Put(slug, payload) => {
                    self.db.wal_write(WalEntry::Put { slug, payload });
                }
                TxnOp::Remove(slug) => {
                    self.db.wal_write(WalEntry::Remove { slug });
                }
                TxnOp::Link(from, to, edge_type) => {
                    self.db.wal_write(WalEntry::Link { from, to, edge_type });
                }
                TxnOp::LinkMeta(from, to, edge_type, meta) => {
                    self.db.wal_write(WalEntry::LinkMeta { from, to, edge_type, meta });
                }
                TxnOp::Unlink(from, to, edge_type) => {
                    self.db.wal_write(WalEntry::Unlink { from, to, edge_type });
                }
                TxnOp::PutVector(slug, field, data) => {
                    self.db.wal_write(WalEntry::PutVector { slug, field, data });
                }
            }
        }
        self.db.defer_wal_sync = false;
        self.db.wal_flush();
        Ok(count)
    }

    /// Discard all queued writes. Equivalent to dropping the `Transaction`.
    pub fn rollback(self) {
        // Nothing to do — ops were never applied.
    }
}

/// Extract all string fields from a JSON value recursively.
fn extract_string_fields(
    value: &Value,
    prefix: &str,
    out: &mut HashMap<String, Vec<(u64, String)>>,
    doc_id: u64,
) {
    match value {
        Value::String(s) => {
            let key = if prefix.is_empty() {
                "<root>".to_string()
            } else {
                prefix.to_string()
            };
            out.entry(key).or_default().push((doc_id, s.clone()));
        }
        Value::Object(map) => {
            for (k, v) in map {
                let new_prefix = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{}.{}", prefix, k)
                };
                extract_string_fields(v, &new_prefix, out, doc_id);
            }
        }
        Value::Array(arr) => {
            for (i, v) in arr.iter().enumerate() {
                let new_prefix = format!("{}[{}]", prefix, i);
                extract_string_fields(v, &new_prefix, out, doc_id);
            }
        }
        _ => {}
    }
}

// ── Spatial helpers ───────────────────────────────────────────────────────────

impl CoreDB {
    /// Extract centroid (lat, lon) from a node's geometry field.
    ///
    /// Returns `None` if the node doesn't exist or has no valid geometry.
    ///
    /// # Arguments
    /// * `slug` - The node slug (e.g., "places/pt1")
    ///
    /// # Example
    /// ```
    /// # use sekejap::CoreDB;
    /// let mut db = CoreDB::new();
    /// db.put("zones/z1", r#"{
    ///     "name": "Zone 1",
    ///     "geometry": {"type": "Polygon", "coordinates": [[[144.95,-37.80],[144.98,-37.80],[144.98,-37.83],[144.95,-37.83],[144.95,-37.80]]]}
    /// }"#).unwrap();
    /// if let Some((lat, lon)) = db.centroid("zones/z1") {
    ///     println!("Centroid: ({lat}, {lon})");
    /// }
    /// ```
    pub fn centroid(&self, slug: &str) -> Option<(f64, f64)> {
        let hash = *self.slug_map.get(slug)?;
        let (off, len) = self.payload_loc(hash)?;
        let payload = self.payload_store.get(off, len)?;
        geo::extract_centroid(&payload)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// If a `serde_json::Value::Array` contains only numbers, return them as `Vec<f32>`.
/// Used by the SQL executor to detect vector literals in INSERT/UPDATE values.
fn value_as_f32_vec(v: &Value) -> Option<Vec<f32>> {
    let arr = v.as_array()?;
    if arr.is_empty() {
        return None;
    }
    arr.iter()
        .map(|x| x.as_f64().map(|f| f as f32))
        .collect()
}

// ── Snapshot format ───────────────────────────────────────────────────────────

/// Serde visitor that tokenizes and discards any JSON value without allocating.
/// Used to skip legacy fields (e.g. `gin_indexes`) that were written by older
/// binaries but are no longer needed.
#[derive(Default)]
struct Ignored;
impl<'de> serde::Deserialize<'de> for Ignored {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        serde::de::IgnoredAny::deserialize(d)?;
        Ok(Ignored)
    }
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    /// true = disk-backed snapshot: payloads are in payloads.bin, SnapNode has
    /// offset/len/collection/spatial_meta but no payload field.
    #[serde(default)]
    is_disk_backed: bool,
    /// true = vectors live in per-field `vectors_{field}.bin` files, not in the
    /// `vectors` JSON array.  On open(), the binary files are mmap'd directly
    /// instead of parsing vectors from JSON and migrating to disk.
    #[serde(default)]
    has_vector_files: bool,
    /// true (v3+) = manifest snapshot: nodes/edges live in the topology files;
    /// the arrays below are empty. v2 snapshots deserialize as false.
    #[serde(default)]
    topology_in_files: bool,
    nodes: Vec<SnapNode>,
    edges: Vec<SnapEdge>,
    #[serde(skip_serializing_if = "Option::is_none")]
    schemas: Option<Vec<sql::TableSchema>>,
    /// Vector data is stored alongside node data so compact() + reload loses nothing.
    /// `None` on old snapshots is safe — WAL replay fills the gap.
    #[serde(skip_serializing_if = "Option::is_none")]
    vectors: Option<Vec<SnapVector>>,
    /// HNSW graphs — persisted so they don't need rebuilding on startup.
    #[serde(skip_serializing_if = "Option::is_none")]
    hnsw_indexes: Option<Vec<SnapHnsw>>,
    /// Btree field indexes — stored in disk-backed snapshots so they don't need
    /// to be rebuilt by scanning payloads.bin on every open.
    #[serde(skip_serializing_if = "Option::is_none")]
    btree_indexes: Option<Vec<SnapBtree>>,
    /// Legacy field written by older builds — never serialised, silently consumed
    /// during deserialisation to avoid allocating a multi-GB serde_json Value.
    /// Read-only-by-design: it exists purely to absorb the old on-disk key.
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    gin_indexes: Ignored,
}

#[derive(Serialize, Deserialize)]
struct SnapHnsw {
    field: String,
    #[serde(default)]
    version: u32,
    #[serde(default = "default_hnsw_m")]
    m: usize,
    #[serde(default = "default_hnsw_ef")]
    ef_construction: usize,
    graph: vector::HnswGraph,
}
fn default_hnsw_m()  -> usize { 16 }
fn default_hnsw_ef() -> usize { 200 }

#[derive(Serialize, Deserialize)]
struct SnapNode {
    slug: String,
    /// Full payload — used by in-memory (non-disk-backed) snapshots.
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<Value>,
    /// Disk-backed snapshot fields — offset/len into payloads.bin plus cached metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    payload_offset: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload_len: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    collection: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    spatial_meta: Option<geo::SpatialMeta>,
}

#[derive(Serialize, Deserialize)]
struct SnapEdge {
    from: String,
    to: String,
    edge_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    meta: Option<Value>,
}

#[derive(Serialize, Deserialize)]
struct SnapVector {
    slug: String,
    field: String,
    data: Vec<f32>,
}

/// Persisted btree field index for fast disk-backed snapshot reload.
#[derive(Serialize, Deserialize)]
struct SnapBtree {
    collection_hash: u64,
    field: String,
    /// Sorted (key, Vec<node_hash>) pairs — reconstructs the BTreeMap directly.
    entries: Vec<(FieldKey, Vec<u64>)>,
}

#[cfg(test)]
mod hybrid_query_tests {
    use super::*;

    #[test]
    fn hash_collision_is_rejected_not_silently_merged() {
        let mut db = CoreDB::new();

        // Re-putting the SAME slug is a normal update, never a collision.
        db.put("things/alpha", r#"{"_collection":"things","_key":"alpha","v":1}"#).unwrap();
        db.put("things/alpha", r#"{"_collection":"things","_key":"alpha","v":2}"#)
            .expect("re-putting the same slug must be a normal update");

        // Simulate a hash collision: make the slot at alpha's hash claim a *different*
        // slug (in reality this happens when two distinct slugs hash to the same u64).
        let h = sk_hash("things/alpha");
        db.nodes.get_mut(&h).unwrap().slug = "things/beta".to_string();

        // A real put of "things/alpha" now lands on a slot owned by a different slug.
        // It must be a LOUD error, not a silent overwrite/merge.
        let err = db
            .put("things/alpha", r#"{"_collection":"things","_key":"alpha","v":3}"#)
            .expect_err("a hash collision must be rejected, not silently merged");
        assert!(
            err.to_string().contains("collision"),
            "error must mention collision, got: {err}"
        );

        // And the existing (different-slug) node is untouched — no merge happened.
        assert_eq!(db.nodes.get(&h).unwrap().slug, "things/beta");
    }

    #[test]
    fn compact_writes_readable_topology_files() {
        use crate::storage::topology::{TopologyBlob, TopologyView};

        let dir = tempfile::tempdir().unwrap();
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            db.put("tourist/chloe", r#"{"_collection":"tourist","_key":"chloe"}"#).unwrap();
            db.put("place/uluwatu", r#"{"_collection":"place","_key":"uluwatu"}"#).unwrap();
            db.put("place/ubud", r#"{"_collection":"place","_key":"ubud"}"#).unwrap();
            db.link("tourist/chloe", "place/uluwatu", "visited");
            db.link("tourist/chloe", "place/ubud", "visited");
            db.compact().unwrap();
        }

        // Read the topology files written by compact() and verify they round-trip.
        let rd = |name: &str| std::fs::read(dir.path().join(name)).unwrap();
        let blob = TopologyBlob {
            nodes: rd("nodes.bin"),
            fwd: rd("adj_fwd.bin"),
            rev: rd("adj_rev.bin"),
            idx: rd("idx.bin"),
            slugs: rd("slugs.bin"),
            dict: rd("dict.bin"),
            spat: rd("spatial.bin"),
            emeta: rd("edgemeta.bin"),
            colls: rd("collections.bin"),
        };
        let view = TopologyView::new(&blob).unwrap();
        assert_eq!(view.node_count(), 3);
        // Reverse mapping: dense id → slug.
        let chloe_id = view.resolve(sk_hash("tourist/chloe")).unwrap();
        assert_eq!(view.slug(chloe_id), Some("tourist/chloe"));

        let chloe = view.resolve(sk_hash("tourist/chloe")).expect("resolve chloe");
        let rec = view.node_record(chloe).unwrap();
        assert_eq!(view.collection_name(rec.collection_id), Some("tourist"));

        let ulu = view.resolve(sk_hash("place/uluwatu")).unwrap();
        let ubud = view.resolve(sk_hash("place/ubud")).unwrap();
        let out = view.fwd_edges(chloe);
        assert_eq!(out.len(), 2, "chloe visited two places");
        let neigh: std::collections::HashSet<u64> = out.iter().map(|e| e.neighbor).collect();
        assert_eq!(neigh, [ulu, ubud].into_iter().collect());
        for e in &out {
            assert_eq!(view.edge_type_name(e.edge_type_id), Some("visited"));
        }

        // Reverse adjacency: uluwatu has exactly one incoming edge, from chloe.
        let rin = view.rev_edges(ulu);
        assert_eq!(rin.len(), 1);
        assert_eq!(rin[0].neighbor, chloe);
    }

    #[test]
    fn mapped_topology_equivalent_to_resident_graph() {
        use crate::storage::topology::MappedTopology;

        let dir = tempfile::tempdir().unwrap();
        let mut db = CoreDB::open(dir.path()).unwrap();
        // A small but non-trivial Bali graph: multiple collections, edge types,
        // fan-out and fan-in.
        for (slug, coll) in [
            ("tourist/chloe", "tourist"), ("tourist/milan", "tourist"),
            ("place/uluwatu", "place"), ("place/ubud", "place"), ("place/canggu", "place"),
            ("area/south", "area"),
        ] {
            let key = slug.split('/').nth(1).unwrap();
            db.put(slug, &format!(r#"{{"_collection":"{coll}","_key":"{key}"}}"#)).unwrap();
        }
        db.link("tourist/chloe", "place/uluwatu", "visited");
        db.link("tourist/chloe", "place/ubud", "visited");
        db.link("tourist/milan", "place/ubud", "visited");
        db.link("tourist/milan", "place/canggu", "stayed_at");
        db.link("place/uluwatu", "area/south", "in_area");
        db.link("place/canggu", "area/south", "in_area");
        db.compact().unwrap();

        let mapped = MappedTopology::open(dir.path()).unwrap();
        assert_eq!(mapped.node_count(), db.nodes.len());

        // For EVERY node: identity, slug, payload location, and both edge
        // directions must match the resident graph exactly.
        for (&hash, node) in &db.nodes {
            let id = mapped.resolve(hash).expect("mapped resolve");
            assert_eq!(mapped.slug_of(id), Some(node.slug.as_str()));
            assert_eq!(mapped.hash_of(id), Some(hash));
            let rec = mapped.node_record(id).unwrap();
            assert_eq!(rec.hash, hash);
            assert_eq!(rec.payload_offset, node.payload_offset);
            assert_eq!(rec.payload_len, node.payload_len);
            assert_eq!(
                mapped.collection_name(rec.collection_id).unwrap_or(""),
                node.collection
            );

            // Edge sets (other, type) as multisets.
            let to_set = |edges: Option<Vec<crate::storage::topology::MappedEdge>>| {
                let mut v: Vec<(u64, u64)> = edges
                    .unwrap_or_default()
                    .into_iter()
                    .map(|e| (e.other_hash, e.edge_type_hash))
                    .collect();
                v.sort_unstable();
                v
            };
            let resident = |edges: Option<&[crate::storage::edgestore::Edge]>| {
                let mut v: Vec<(u64, u64)> = edges
                    .unwrap_or(&[])
                    .iter()
                    .map(|e| (e.other, e.edge_type))
                    .collect();
                v.sort_unstable();
                v
            };
            assert_eq!(
                to_set(mapped.fwd_by_hash(hash)),
                resident(db.fwd_edges(hash).as_deref()),
                "fwd mismatch for {}",
                node.slug
            );
            assert_eq!(
                to_set(mapped.rev_by_hash(hash)),
                resident(db.rev_edges(hash).as_deref()),
                "rev mismatch for {}",
                node.slug
            );
        }

        // Unknown hash resolves to nothing.
        assert!(mapped.resolve(sk_hash("nope/nope")).is_none());
        assert!(mapped.fwd_by_hash(sk_hash("nope/nope")).is_none());
    }

    #[test]
    fn paged_open_query_results_match_resident() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            for (slug, coll, extra) in [
                ("tourist/chloe", "tourist", r#""home":"Melbourne""#),
                ("tourist/milan", "tourist", r#""home":"Toronto""#),
                ("place/uluwatu", "place", r#""city":"Bali","kind":"temple""#),
                ("place/ubud", "place", r#""city":"Bali","kind":"town""#),
                ("place/canggu", "place", r#""city":"Bali","kind":"beach""#),
            ] {
                let key = slug.split('/').nth(1).unwrap();
                db.put(slug, &format!(r#"{{"_collection":"{coll}","_key":"{key}",{extra}}}"#)).unwrap();
            }
            db.link("tourist/chloe", "place/uluwatu", "visited");
            db.link("tourist/chloe", "place/ubud", "visited");
            db.link("tourist/milan", "place/canggu", "visited");
            db.compact().unwrap();
        }

        // Identical results for point reads, scans, filters and MATCH traversal.
        // (Sequential opens — the exclusive file lock allows one writer at a time.)
        let queries = [
            "SELECT * FROM place ORDER BY _key ASC",
            "SELECT * FROM place WHERE kind = 'temple'",
            "SELECT _key FROM tourist ORDER BY _key ASC",
            "SELECT b._key AS k FROM MATCH (a:tourist)-[:visited]->(b:place) \
             WHERE a._key='chloe' ORDER BY k ASC",
            "SELECT a._key AS k FROM MATCH (a:tourist)-[:visited]->(b:place) \
             WHERE b._key='canggu'",
            "SELECT COUNT(*) AS n FROM MATCH (a:tourist)-[:visited]->(b:place)",
        ];
        let (resident_results, resident_ubud) = {
            let resident = CoreDB::open(dir.path()).unwrap();
            let results: Vec<Vec<_>> = queries.iter().map(|q| {
                resident.query(q).unwrap().collect()
                    .iter().map(|h| h.payload.clone()).collect()
            }).collect();
            (results, resident.get("place/ubud"))
        };

        let paged = CoreDB::open_paged(dir.path()).unwrap();
        assert!(paged.topo_base.is_some(), "paged open must attach the mmap base");
        assert!(paged.nodes.is_empty(), "paged open must not load nodes into RAM");
        for (q, expected) in queries.iter().zip(&resident_results) {
            let p: Vec<_> = paged.query(q).unwrap().collect()
                .iter().map(|h| h.payload.clone()).collect();
            assert_eq!(expected, &p, "paged != resident for: {q}");
        }
        // Point get through the payload store.
        assert_eq!(resident_ubud, paged.get("place/ubud"));
    }

    #[test]
    fn paged_open_writes_merge_with_base() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            db.put("tourist/chloe", r#"{"_collection":"tourist","_key":"chloe","v":1}"#).unwrap();
            db.put("place/uluwatu", r#"{"_collection":"place","_key":"uluwatu"}"#).unwrap();
            db.link("tourist/chloe", "place/uluwatu", "visited");
            db.compact().unwrap();
        }

        let mut db = CoreDB::open_paged(dir.path()).unwrap();

        // 1. New node + new edge land in the overlay and merge with base edges.
        db.put("place/ubud", r#"{"_collection":"place","_key":"ubud"}"#).unwrap();
        db.link("tourist/chloe", "place/ubud", "visited");
        let hits = db.query(
            "SELECT b._key AS k FROM MATCH (a:tourist)-[:visited]->(b:place) \
             WHERE a._key='chloe' ORDER BY k ASC"
        ).unwrap().collect();
        let keys: Vec<&str> = hits.iter()
            .map(|h| h.payload.as_ref().unwrap()["k"].as_str().unwrap()).collect();
        assert_eq!(keys, vec!["ubud", "uluwatu"], "base + overlay edges must merge");

        // 2. Collection scan sees base + overlay members, deduped.
        assert_eq!(db.query("SELECT * FROM place").unwrap().collect().len(), 2);

        // 3. Updating a BASE node: overlay version wins, no duplicate in scans.
        db.put("tourist/chloe", r#"{"_collection":"tourist","_key":"chloe","v":2}"#).unwrap();
        let chloe: Value = serde_json::from_str(&db.get("tourist/chloe").unwrap()).unwrap();
        assert_eq!(chloe["v"].as_f64().unwrap(), 2.0, "overlay update must win over base");
        assert_eq!(db.query("SELECT * FROM tourist").unwrap().collect().len(), 1,
            "updated base node must not appear twice");

        // 4. Collision guard also fires against the mapped base.
        let base_hash = sk_hash("place/uluwatu");
        assert!(db.nodes.get(&base_hash).is_none(), "uluwatu must live only in the base");
        // (a real collision can't be forged against the base without breaking the
        // idx invariant, so we just assert same-slug base update passes)
        db.put("place/uluwatu", r#"{"_collection":"place","_key":"uluwatu","u":1}"#).unwrap();
    }

    #[test]
    fn sql_edge_insert_routes_props_to_fast_lane() {
        let mut db = CoreDB::new();
        db.put("a/x", r#"{"_collection":"a","_key":"x"}"#).unwrap();
        db.put("b/y", r#"{"_collection":"b","_key":"y"}"#).unwrap();
        // SQL edge insert with mixed props: number+bool -> fast lane, string -> JSON
        db.execute("INSERT ('a/x')-[:rel {confidence: 0.9, active: true, note: 'hi'}]->('b/y')").unwrap();
        let r = db.query(
            "SELECT r.confidence AS c, r.active AS act, r.note AS n \
             FROM MATCH (a:a)-[r:rel]->(b:b) WHERE a._key='x'"
        ).unwrap().collect();
        let p = r[0].payload.as_ref().unwrap();
        assert!((p["c"].as_f64().unwrap() - 0.9).abs() < 1e-9);
        assert_eq!(p["act"].as_bool().unwrap(), true);
        assert_eq!(p["n"].as_str().unwrap(), "hi");
    }

    #[test]
    fn edge_fast_lane_routes_by_type_and_reads() {
        let mut db = CoreDB::new();
        db.put("a/x", r#"{"_collection":"a","_key":"x"}"#).unwrap();
        db.put("b/y", r#"{"_collection":"b","_key":"y"}"#).unwrap();
        // number -> fast lane, bool -> fast lane, string -> JSON bag
        db.link_attr("a/x", "b/y", "rel", vec![
            ("confidence".to_string(), serde_json::json!(0.8)),
            ("active".to_string(), serde_json::json!(true)),
            ("note".to_string(), serde_json::json!("hello")),
        ]);
        let r = db.query(
            "SELECT b._key AS k, r.confidence AS c, r.active AS act, r.note AS n \
             FROM MATCH (a:a)-[r:rel]->(b:b) WHERE a._key='x'"
        ).unwrap().collect();
        assert_eq!(r.len(), 1);
        let p = r[0].payload.as_ref().unwrap();
        assert!((p["c"].as_f64().unwrap() - 0.8).abs() < 1e-9, "number column read");
        assert_eq!(p["act"].as_bool().unwrap(), true, "bool column read");
        assert_eq!(p["n"].as_str().unwrap(), "hello", "string -> JSON bag read");
        // aggregate over a fast-lane column across DISTINCT edges (parallel-edge
        // attribute disambiguation is a separate per-edge-identity concern).
        db.put("b/z", r#"{"_collection":"b","_key":"z"}"#).unwrap();
        db.link_attr("a/x", "b/z", "rel", vec![("confidence".to_string(), serde_json::json!(0.4))]);
        let agg = db.query(
            "SELECT AVG(r.confidence) AS avg_c, COUNT(*) AS n \
             FROM MATCH (a:a)-[r:rel]->(b:b) WHERE a._key='x'"
        ).unwrap().collect();
        let ap = agg[0].payload.as_ref().unwrap();
        assert_eq!(ap["n"].as_i64().unwrap(), 2);
        assert!((ap["avg_c"].as_f64().unwrap() - 0.6).abs() < 1e-9, "avg over column = (0.8+0.4)/2");
    }

    #[test]
    fn stream_edge_agg_matches_semantics() {
        let mut db = CoreDB::new();
        db.put("n/a", r#"{"_collection":"n","_key":"a"}"#).unwrap();
        for (k, s) in [("x", 0.2), ("y", 0.8), ("z", 0.5)] {
            db.put(&format!("n/{k}"), &format!(r#"{{"_collection":"n","_key":"{k}"}}"#)).unwrap();
            db.link_meta("n/a", &format!("n/{k}"), "rated", &format!(r#"{{"score":{s}}}"#)).unwrap();
        }
        // SUM/AVG/MIN/MAX/COUNT over an edge attribute, no GROUP BY → streaming path.
        let hits = db.query(
            "SELECT COUNT(*) AS c, SUM(r.score) AS s, AVG(r.score) AS a, \
                    MIN(r.score) AS mn, MAX(r.score) AS mx \
             FROM MATCH (a:n)-[r:rated]->(b:n)"
        ).unwrap().collect();
        let p = hits[0].payload.as_ref().unwrap();
        assert_eq!(p["c"].as_i64().unwrap(), 3);
        assert!((p["s"].as_f64().unwrap() - 1.5).abs() < 1e-9);
        assert!((p["a"].as_f64().unwrap() - 0.5).abs() < 1e-9);
        assert!((p["mn"].as_f64().unwrap() - 0.2).abs() < 1e-9);
        assert!((p["mx"].as_f64().unwrap() - 0.8).abs() < 1e-9);

        // An arithmetic expression over the edge var also streams.
        let doubled = db.query(
            "SELECT SUM(r.score * 2) AS s2 FROM MATCH (a:n)-[r:rated]->(b:n)"
        ).unwrap().collect();
        assert!((doubled[0].payload.as_ref().unwrap()["s2"].as_f64().unwrap() - 3.0).abs() < 1e-9);

        // Empty match → Count 0, Sum 0, Avg/Min/Max Null (matches general path).
        let empty = db.query(
            "SELECT COUNT(*) AS c, SUM(r.score) AS s, AVG(r.score) AS a, MIN(r.score) AS mn \
             FROM MATCH (a:n)-[r:no_such_type]->(b:n)"
        ).unwrap().collect();
        let ep = empty[0].payload.as_ref().unwrap();
        assert_eq!(ep["c"].as_i64().unwrap(), 0);
        assert!((ep["s"].as_f64().unwrap() - 0.0).abs() < 1e-9);
        assert!(ep["a"].is_null());
        assert!(ep["mn"].is_null());
    }

    #[test]
    fn edge_fast_lane_columns_survive_reopen_and_compact() {
        let dir = tempfile::tempdir().unwrap();
        let q = "SELECT r.confidence AS c, r.active AS act, r.note AS n \
                 FROM MATCH (a:a)-[r:rel]->(b:b) WHERE a._key='x'";
        let check = |db: &CoreDB| {
            let r = db.query(q).unwrap().collect();
            assert_eq!(r.len(), 1, "edge must survive");
            let p = r[0].payload.as_ref().unwrap();
            assert!((p["c"].as_f64().unwrap() - 0.9).abs() < 1e-9, "number column");
            assert_eq!(p["act"].as_bool().unwrap(), true, "bool column");
            assert_eq!(p["n"].as_str().unwrap(), "hi", "string -> JSON bag");
        };

        // Write with fast-lane attrs, DON'T compact — forces WAL replay on reopen.
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            db.put("a/x", r#"{"_collection":"a","_key":"x"}"#).unwrap();
            db.put("b/y", r#"{"_collection":"b","_key":"y"}"#).unwrap();
            db.execute("INSERT ('a/x')-[:rel {confidence: 0.9, active: true, note: 'hi'}]->('b/y')").unwrap();
            check(&db);
        }
        // Reopen via WAL replay: link_meta_raw re-routes primitives into columns.
        {
            let db = CoreDB::open(dir.path()).unwrap();
            check(&db); // WAL-replay path
        }
        // Compact folds columns into edgemeta.bin, truncates WAL, then reopen.
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            db.compact().unwrap();
            check(&db);
        }
        // Resident reopen after compact (loads from topology files, no WAL).
        {
            let db = CoreDB::open(dir.path()).unwrap();
            check(&db);
        }
        // Paged reopen after compact (columns served from edgemeta.bin base).
        {
            let db = CoreDB::open_paged(dir.path()).unwrap();
            assert!(db.nodes.is_empty(), "paged must serve from base");
            check(&db);
        }
    }

    #[test]
    fn paged_mode_serves_edge_metadata_from_base() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            db.put("tourist/chloe", r#"{"_collection":"tourist","_key":"chloe"}"#).unwrap();
            db.put("place/uluwatu", r#"{"_collection":"place","_key":"uluwatu"}"#).unwrap();
            db.put("place/ubud", r#"{"_collection":"place","_key":"ubud"}"#).unwrap();
            db.link_meta("tourist/chloe", "place/uluwatu", "visited",
                r#"{"days":3,"season":"dry"}"#).unwrap();
            db.link("tourist/chloe", "place/ubud", "visited"); // no meta
            db.compact().unwrap();
        }

        let paged = CoreDB::open_paged(dir.path()).unwrap();
        assert!(paged.nodes.is_empty(), "must be served from base");

        // r.<meta field> from the mapped base.
        let hits = paged.query(
            "SELECT b._key AS k, r.days AS d \
             FROM MATCH (a:tourist)-[r:visited]->(b:place) \
             WHERE a._key='chloe' ORDER BY k ASC"
        ).unwrap().collect();
        assert_eq!(hits.len(), 2);
        let ubud = hits[0].payload.as_ref().unwrap();
        let ulu = hits[1].payload.as_ref().unwrap();
        assert_eq!(ulu["k"].as_str().unwrap(), "uluwatu");
        assert_eq!(ulu["d"].as_i64().or(ulu["d"].as_f64().map(|f| f as i64)).unwrap(), 3,
            "edge metadata must be served from edgemeta.bin");
        assert!(ubud["d"].is_null(), "meta-less edge stays meta-less");

        // And recovery (snapshot lost) restores edge metadata too.
        drop(paged);
        std::fs::remove_file(dir.path().join("snapshot.json")).unwrap();
        let recovered = CoreDB::open(dir.path()).unwrap();
        let hits = recovered.query(
            "SELECT r.season AS se FROM MATCH (a:tourist)-[r:visited]->(b:place) \
             WHERE a._key='chloe' AND b._key='uluwatu'"
        ).unwrap().collect();
        assert_eq!(hits[0].payload.as_ref().unwrap()["se"].as_str().unwrap(), "dry",
            "recovery must restore edge metadata from edgemeta.bin");
    }

    #[test]
    fn paged_mode_serves_spatial_from_side_table() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            // Two Bali places with geometry, one without.
            db.put("place/uluwatu", r#"{"_collection":"place","_key":"uluwatu",
                "geometry":{"type":"Point","coordinates":[115.0849,-8.8291]}}"#).unwrap();
            db.put("place/ubud", r#"{"_collection":"place","_key":"ubud",
                "geometry":{"type":"Point","coordinates":[115.2625,-8.5069]}}"#).unwrap();
            db.put("place/nowhere", r#"{"_collection":"place","_key":"nowhere"}"#).unwrap();
            db.put("tourist/chloe", r#"{"_collection":"tourist","_key":"chloe"}"#).unwrap();
            db.link("tourist/chloe", "place/uluwatu", "visited");
            db.link("tourist/chloe", "place/ubud", "visited");
            db.compact().unwrap();
        }

        // Resident results first (sequential opens — single-writer lock).
        let q_grid = "SELECT _key FROM place WHERE ST_DWithin(geometry, POINT(115.08 -8.83), 5.0) ORDER BY _key ASC";
        let q_match = "SELECT b._key AS k FROM MATCH (a:tourist)-[:visited]->(b:place) \
                       WHERE a._key='chloe' AND ST_DWithin(b.geometry, POINT(115.08 -8.83), 5.0)";
        let (r_grid, r_match) = {
            let resident = CoreDB::open(dir.path()).unwrap();
            let g: Vec<_> = resident.query(q_grid).unwrap().collect()
                .iter().map(|h| h.payload.clone()).collect();
            let m: Vec<_> = resident.query(q_match).unwrap().collect()
                .iter().map(|h| h.payload.clone()).collect();
            (g, m)
        };
        assert!(!r_grid.is_empty(), "resident spatial query must match uluwatu");

        // Paged: same results, spatial served from spatial.bin (nodes map empty).
        let paged = CoreDB::open_paged(dir.path()).unwrap();
        assert!(paged.nodes.is_empty());
        let p_grid: Vec<_> = paged.query(q_grid).unwrap().collect()
            .iter().map(|h| h.payload.clone()).collect();
        let p_match: Vec<_> = paged.query(q_match).unwrap().collect()
            .iter().map(|h| h.payload.clone()).collect();
        assert_eq!(r_grid, p_grid, "grid-path spatial must match resident");
        assert_eq!(r_match, p_match, "MATCH-filter spatial must match resident");
    }

    #[test]
    fn manifest_snapshot_shrinks_and_reopens_complete() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            db.execute("CREATE TABLE place (_key TEXT PRIMARY KEY, city TEXT)").unwrap();
            db.put("tourist/chloe", r#"{"_collection":"tourist","_key":"chloe"}"#).unwrap();
            db.execute("INSERT INTO place (_key, city) VALUES ('uluwatu', 'Bali')").unwrap();
            db.link_meta("tourist/chloe", "place/uluwatu", "visited", r#"{"days":3}"#).unwrap();
            db.compact().unwrap();
        }

        // v3 manifest: no nodes/edges arrays in the snapshot.
        let snap = std::fs::read(dir.path().join("snapshot.json")).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&snap[16..]).unwrap();
        assert_eq!(body["topology_in_files"], serde_json::json!(true));
        assert!(body["nodes"].as_array().unwrap().is_empty(), "manifest must not carry nodes");
        assert!(body["edges"].as_array().unwrap().is_empty(), "manifest must not carry edges");

        // Reopen resident: everything intact — nodes, edges, edge meta, schema.
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            assert_eq!(db.query("SELECT * FROM place").unwrap().collect().len(), 1);
            let hits = db.query(
                "SELECT b.city AS c, r.days AS d FROM MATCH (a:tourist)-[r:visited]->(b:place) \
                 WHERE a._key='chloe'"
            ).unwrap().collect();
            let p = hits[0].payload.as_ref().unwrap();
            assert_eq!(p["c"].as_str().unwrap(), "Bali");
            assert_eq!(p["d"].as_f64().unwrap() as i64, 3, "edge meta must survive manifest reopen");
            assert!(db.schema_ddl("place").is_some(), "schema must survive (still in snapshot)");

            // Post-compact write → WAL → survives another reopen.
            db.put("place/ubud", r#"{"_collection":"place","_key":"ubud","city":"Bali"}"#).unwrap();
        }
        {
            let db = CoreDB::open(dir.path()).unwrap();
            assert_eq!(db.query("SELECT * FROM place").unwrap().collect().len(), 2,
                "WAL write after manifest compact must survive reopen");
        }

        // And paged open over the same manifest dir.
        let paged = CoreDB::open_paged(dir.path()).unwrap();
        assert_eq!(paged.query("SELECT * FROM place").unwrap().collect().len(), 2);
    }

#[test]
    fn skbin_field_table_recovers_from_corrupt_primary_copy() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config { payload_binary: true, ..Config::default() };
        {
            let mut db = CoreDB::open_with_config(dir.path(), cfg.clone()).unwrap();
            for i in 0..20 {
                db.put(&format!("orders/o{i}"),
                    &format!(r#"{{"_collection":"orders","_key":"o{i}","amount":{i},"note":"row {i}"}}"#)).unwrap();
            }
            db.compact().unwrap();
        }
        // All three redundant copies must exist.
        for name in FIELD_TABLE_COPIES {
            assert!(dir.path().join(name).exists(), "{name} must be written");
        }
        // Corrupt the PRIMARY copy (flip a byte in its payload).
        {
            let p = dir.path().join("field_table.bin");
            let mut bytes = std::fs::read(&p).unwrap();
            let last = bytes.len() - 1;
            bytes[last] ^= 0xff;
            std::fs::write(&p, &bytes).unwrap();
        }
        // Reopen must recover from a backup copy and read every record intact.
        let db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
        for i in 0..20 {
            let v: Value = serde_json::from_str(&db.get(&format!("orders/o{i}")).unwrap()).unwrap();
            assert_eq!(v["amount"], i, "record o{i} must survive a corrupt primary field table");
            assert_eq!(v["note"], format!("row {i}"));
        }
    }

    #[test]
    fn skbin_payload_roundtrips_shrinks_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config { payload_binary: true, ..Config::default() };
        // Realistic records exercising every value type.
        let mk = |i: usize| format!(
            r#"{{"_collection":"orders","_key":"ord-{i:05}","customer":"cust-{}","qty":{},"price":{}.{},"active":{},"tags":["a","b","c"],"note":"order number {i} shipped","ts":1700000000}}"#,
            i % 1000, (i % 5) + 1, i % 100, i % 10, i % 2 == 0
        );
        // Capture each record as stored (with put()'s injected fields) BEFORE
        // compaction, so we can prove SKBIN reproduces it exactly afterwards.
        let mut before: Vec<Value> = Vec::new();
        {
            let mut db = CoreDB::open_with_config(dir.path(), cfg.clone()).unwrap();
            for i in 0..100 { db.put(&format!("orders/ord-{i:05}"), &mk(i)).unwrap(); }
            db.link("orders/ord-00000", "orders/ord-00001", "next");
            for i in 0..100 {
                before.push(serde_json::from_str(&db.get(&format!("orders/ord-{i:05}")).unwrap()).unwrap());
            }
            db.compact().unwrap();
        }
        assert!(dir.path().join("field_table.bin").exists(), "SKBIN must persist the field table");
        let bin_size = std::fs::metadata(dir.path().join("payloads.bin")).unwrap().len();

        // Reopen (SKBIN store) — EVERY record must decode back byte-identical to
        // what was stored, across every value type.
        {
            let db = CoreDB::open_with_config(dir.path(), cfg.clone()).unwrap();
            for i in 0..100 {
                let got: Value = serde_json::from_str(&db.get(&format!("orders/ord-{i:05}")).unwrap()).unwrap();
                assert_eq!(got, before[i], "record {i} must roundtrip through SKBIN");
            }
            // Queries read SKBIN payloads transparently — numeric, boolean, and
            // string filters all go through the SKBIN-aware field extractor.
            assert_eq!(db.query("SELECT * FROM orders WHERE qty >= 3").unwrap().collect().len(), 60);
            assert_eq!(db.query("SELECT * FROM orders WHERE active = true").unwrap().collect().len(), 50);
            assert_eq!(db.query("SELECT _key, qty FROM orders WHERE customer = 'cust-7'").unwrap().collect().len(), 1);
            let hits = db.query(
                "SELECT b._key AS k FROM MATCH (a:orders)-[:next]->(b:orders) WHERE a._key='ord-00000'"
            ).unwrap().collect();
            assert_eq!(hits[0].payload.as_ref().unwrap()["k"].as_str().unwrap(), "ord-00001");
        }

        // Mixed: a NEW record written after reopen is raw JSON; reads alongside SKBIN.
        {
            let mut db = CoreDB::open_with_config(dir.path(), cfg.clone()).unwrap();
            db.put("orders/fresh", r#"{"_collection":"orders","_key":"fresh","qty":9}"#).unwrap();
            let old: Value = serde_json::from_str(&db.get("orders/ord-00050").unwrap()).unwrap(); // SKBIN
            assert_eq!(old["_key"], "ord-00050");
            let new: Value = serde_json::from_str(&db.get("orders/fresh").unwrap()).unwrap();      // raw
            assert_eq!(new["qty"], 9);
            db.compact().unwrap(); // folds the raw record into SKBIN too
        }
        {
            let db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
            assert_eq!(db.query("SELECT * FROM orders").unwrap().collect().len(), 101);
            assert_eq!(serde_json::from_str::<Value>(&db.get("orders/fresh").unwrap()).unwrap()["qty"], 9);
        }

        // Size: strictly smaller than the same data stored raw.
        let dir2 = tempfile::tempdir().unwrap();
        {
            // Explicit RAW baseline (default is now SKBIN) for the size comparison.
            let mut db = CoreDB::open_with_config(dir2.path(), Config { payload_binary: false, ..Config::default() }).unwrap();
            for i in 0..100 { db.put(&format!("orders/ord-{i:05}"), &mk(i)).unwrap(); }
            db.compact().unwrap();
        }
        let raw_size = std::fs::metadata(dir2.path().join("payloads.bin")).unwrap().len();
        assert!(bin_size < raw_size, "SKBIN payloads must be smaller than raw ({bin_size} vs {raw_size})");
    }

    #[test]
    fn auto_compact_on_write_fires_and_truncates_wal() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            auto_compact: AutoCompact::OnWrite,
            compact_thresholds: CompactThresholds { wal_bytes: 2048, overlay_entries: usize::MAX },
            ..Config::default()
        };
        let mut db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
        // >64 writes (check cadence) with WAL well past 2 KB → must auto-compact.
        for i in 0..150 {
            db.put(&format!("t/n{i}"),
                &format!(r#"{{"_collection":"t","_key":"n{i}","pad":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}}"#)).unwrap();
        }
        let wal_len = std::fs::metadata(dir.path().join("wal.log")).unwrap().len();
        assert!(wal_len < 2048 + 4096,
            "auto-compact must have truncated the WAL (len = {wal_len})");
        assert!(dir.path().join("nodes.bin").exists(), "compaction wrote topology files");
        // All data intact after the inline compaction(s).
        assert_eq!(db.query("SELECT * FROM t").unwrap().collect().len(), 150);
        drop(db);
        let db = CoreDB::open(dir.path()).unwrap();
        assert_eq!(db.query("SELECT * FROM t").unwrap().collect().len(), 150);
    }

    #[test]
    fn maybe_compact_manual_mode() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            auto_compact: AutoCompact::Manual,
            compact_thresholds: CompactThresholds { wal_bytes: 512, overlay_entries: usize::MAX },
            ..Config::default()
        };
        let mut db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
        for i in 0..20 {
            db.put(&format!("t/n{i}"), &format!(r#"{{"_collection":"t","_key":"n{i}"}}"#)).unwrap();
        }
        // Manual mode: nothing fired automatically…
        let wal_before = std::fs::metadata(dir.path().join("wal.log")).unwrap().len();
        assert!(wal_before > 512, "WAL must have grown past the threshold");
        // …but the idle call compacts, and a second call is a no-op.
        assert!(db.maybe_compact().unwrap(), "thresholds crossed → must compact");
        assert!(!db.maybe_compact().unwrap(), "fresh WAL → no-op");
        assert_eq!(db.query("SELECT * FROM t").unwrap().collect().len(), 20);
    }

    #[test]
    fn compact_on_close_checkpoints() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config { compact_on_close: true, auto_compact: AutoCompact::Off, ..Config::default() };
        {
            let mut db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
            for i in 0..80 {
                db.put(&format!("t/n{i}"),
                    &format!(r#"{{"_collection":"t","_key":"n{i}","pad":"yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy"}}"#)).unwrap();
            }
        } // drop → final checkpoint
        let wal_len = std::fs::metadata(dir.path().join("wal.log")).unwrap().len();
        assert!(wal_len <= 16, "close must have checkpointed the WAL (len = {wal_len})");
        assert!(dir.path().join("nodes.bin").exists());
        let db = CoreDB::open(dir.path()).unwrap();
        assert_eq!(db.query("SELECT * FROM t").unwrap().collect().len(), 80);
    }

    #[test]
    fn open_recovers_from_topology_files_when_snapshot_lost() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            db.put("tourist/chloe", r#"{"_collection":"tourist","_key":"chloe","name":"Chloe"}"#).unwrap();
            db.put("place/uluwatu", r#"{"_collection":"place","_key":"uluwatu","city":"Bali"}"#).unwrap();
            db.link("tourist/chloe", "place/uluwatu", "visited");
            db.compact().unwrap(); // WAL truncated → snapshot + topology files hold everything
        }

        // Disaster: snapshot.json is lost. Pre-Phase 0 this meant total data loss
        // (empty WAL + no snapshot → empty DB). Now open() rebuilds from topology
        // files + payloads.bin.
        std::fs::remove_file(dir.path().join("snapshot.json")).unwrap();

        let db = CoreDB::open(dir.path()).unwrap();
        // Nodes + payloads intact.
        let chloe: Value = serde_json::from_str(&db.get("tourist/chloe").unwrap()).unwrap();
        assert_eq!(chloe["name"].as_str().unwrap(), "Chloe");
        // Collections intact (SQL scan works).
        assert_eq!(db.query("SELECT * FROM place").unwrap().collect().len(), 1);
        // Edges intact, both directions, with type.
        let hits = db.query(
            "SELECT b.city AS c FROM MATCH (a:tourist)-[:visited]->(b:place) WHERE a._key='chloe'"
        ).unwrap().collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].payload.as_ref().unwrap()["c"].as_str().unwrap(), "Bali");
    }

    #[test]
    fn test_hybrid_graph_spatial_range_query() {
        let mut db = CoreDB::new();

        // 100 shops with geometry
        for i in 0..100 {
            db.put(
                &format!("places/shop{}", i),
                &serde_json::json!({
                    "_collection": "places",
                    "_key": format!("shop{}", i),
                    "name": format!("Shop {i}"),
                    "category": if i % 3 == 0 { "electronics" } else { "food" },
                    "price": 10.0 + (i as f64 * 1.5),
                    "geometry": {
                        "type": "Point",
                        "coordinates": [144.9 + (i as f64 * 0.001), -37.8 + (i as f64 * 0.001)]
                    }
                })
                .to_string(),
            )
            .unwrap();
        }

        // Graph edges: shop0 -> shop1 -> shop2 ...
        for i in 0..99 {
            db.link(
                &format!("places/shop{}", i),
                &format!("places/shop{}", i + 1),
                "nearby",
            );
        }

        // Build indexes
        db.build_spatial_index();

        // Graph traversal + spatial + range filter + sort
        let results = db
            .one("places/shop0")
            .forward("nearby")
            .st_dwithin(-37.81, 144.95, 1.0)
            .where_gt("price", 50.0)
            .sort("price", true)
            .take(10)
            .collect();

        assert!(results.len() <= 10);
        println!("Hybrid query returned {} results", results.len());
    }

    #[test]
    fn test_hybrid_with_scalar_functions() {
        let mut db = CoreDB::new();

        for i in 0..10 {
            db.put(
                &format!("users/user{}", i),
                &serde_json::json!({
                    "_collection": "users",
                    "_key": format!("user{}", i),
                    "name": format!("  User {i}  "),
                    "email": format!("user{}@example.com", i),
                    "created_at": format!("2024-01-{:02}T12:00:00Z", i + 1)
                })
                .to_string(),
            )
            .unwrap();
        }

        let results = db
            .query(
                "SELECT LENGTH(name), LOWER(email), YEAR(created_at), MONTH(created_at)
             FROM users 
             ORDER BY LENGTH(name) DESC",
            )
            .unwrap()
            .collect();

        assert!(!results.is_empty());
        println!("Scalar function query returned {} results", results.len());
    }

    #[test]
    fn test_auto_timestamps() {
        let mut db = CoreDB::new();

        // Insert without timestamps
        db.put(
            "users/alice",
            r#"{"name": "Alice", "_collection": "users"}"#,
        )
        .unwrap();

        // Verify timestamps were auto-added
        let hash = *db.slug_map.get("users/alice").unwrap();
        let payload = db.get_payload(hash).unwrap();

        // _created_unix and _updated_unix should exist
        assert!(
            payload.get("_created_unix").is_some(),
            "should have _created_unix"
        );
        assert!(
            payload.get("_updated_unix").is_some(),
            "should have _updated_unix"
        );

        // Values should be integers (unix timestamp millis)
        let created = payload.get("_created_unix").unwrap().as_i64().unwrap();
        let updated = payload.get("_updated_unix").unwrap().as_i64().unwrap();
        assert!(created > 0, "_created_unix should be positive");
        assert!(updated > 0, "_updated_unix should be positive");
        assert_eq!(created, updated, "created == updated on insert");

        // Now update
        std::thread::sleep(std::time::Duration::from_millis(10));
        db.put(
            "users/alice",
            r#"{"name": "Alice Updated", "_collection": "users"}"#,
        )
        .unwrap();

        let hash = *db.slug_map.get("users/alice").unwrap();
        let payload = db.get_payload(hash).unwrap();

        // _created_unix should be preserved, _updated_unix should change
        let created2 = payload.get("_created_unix").unwrap().as_i64().unwrap();
        let updated2 = payload.get("_updated_unix").unwrap().as_i64().unwrap();
        assert_eq!(
            created, created2,
            "_created_unix should be preserved on update"
        );
        assert!(updated2 > updated, "_updated_unix should change on update");

        println!("Auto-timestamps: created={}, updated={}", created, updated);
    }

    #[test]
    fn test_match_insert() {
        let mut db = CoreDB::new();

        // Create people
        for i in 0..10 {
            db.put(
                &format!("people/p{}", i),
                &serde_json::json!({
                    "_collection": "people",
                    "_key": format!("p{}", i),
                    "name": format!("Person {}", i),
                    "grade": 50 + i * 5  // 50, 55, 60, 65, 70, 75, 80, 85, 90, 95
                })
                .to_string(),
            )
            .unwrap();
        }

        // Create classroom
        db.put(
            "classroom/A",
            r#"{"_collection": "classroom", "_key": "A", "name": "Classroom A"}"#,
        )
        .unwrap();

        // MATCH INSERT: link people with grade < 80 to classroom/A
        let count = db
            .execute("MATCH (p:people) WHERE p.grade < 80 INSERT (p)-[:member_of]->(classroom/A)")
            .unwrap();

        assert_eq!(count, 6, "Should link 6 people (grade 50-75)");

        println!("MATCH INSERT created {} edges", count);
    }

    #[test]
    fn test_put_get_vector() {
        let mut db = CoreDB::new();
        db.put(
            "articles/a1",
            r#"{"_collection":"articles","_key":"a1","title":"Rust"}"#,
        )
        .unwrap();
        db.put_vector("articles/a1", "embedding", &[0.1, 0.2, 0.3, 0.4])
            .unwrap();

        let v = db.get_vector("articles/a1", "embedding").unwrap();
        assert_eq!(v, &[0.1f32, 0.2, 0.3, 0.4]);

        // Non-existent field returns None
        assert!(db.get_vector("articles/a1", "other_field").is_none());
        // Non-existent slug returns None
        assert!(db.get_vector("articles/missing", "embedding").is_none());
    }

    #[test]
    fn test_vector_near_api() {
        let mut db = CoreDB::new();
        db.put(
            "articles/a1",
            r#"{"_collection":"articles","_key":"a1","title":"Rust"}"#,
        )
        .unwrap();
        db.put(
            "articles/a2",
            r#"{"_collection":"articles","_key":"a2","title":"Python"}"#,
        )
        .unwrap();
        db.put(
            "articles/a3",
            r#"{"_collection":"articles","_key":"a3","title":"Go"}"#,
        )
        .unwrap();

        db.put_vector("articles/a1", "embedding", &[1.0, 0.0, 0.0, 0.0])
            .unwrap();
        db.put_vector("articles/a2", "embedding", &[0.0, 1.0, 0.0, 0.0])
            .unwrap();
        db.put_vector("articles/a3", "embedding", &[0.0, 0.0, 1.0, 0.0])
            .unwrap();

        // Query closest to a1's embedding
        let results = db
            .collection("articles")
            .vector_near("embedding", vec![1.0, 0.0, 0.0, 0.0], 1)
            .collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].slug, "articles/a1");

        // Top-k=2 should return a1 and one of the orthogonal ones
        let results = db
            .collection("articles")
            .vector_near("embedding", vec![0.1, 0.2, 0.3, 0.4], 5)
            .collect();
        assert_eq!(results.len(), 3, "should return all 3 articles");
    }

    #[test]
    fn test_vector_near_as_starter() {
        let mut db = CoreDB::new();
        db.put("docs/d1", r#"{"_collection":"docs"}"#).unwrap();
        db.put("docs/d2", r#"{"_collection":"docs"}"#).unwrap();

        db.put_vector("docs/d1", "emb", &[1.0, 0.0]).unwrap();
        db.put_vector("docs/d2", "emb", &[0.0, 1.0]).unwrap();

        // VectorNear used as a starter (no prior step besides collection)
        let results = db
            .collection("docs")
            .vector_near("emb", vec![1.0, 0.0], 5)
            .collect();
        assert_eq!(results.len(), 2);
        // d1 should be first (distance = 0)
        assert_eq!(results[0].slug, "docs/d1");
    }

    // ── DDL + vector full flow ─────────────────────────────────────────────────

    #[test]
    fn test_create_table_with_vector_then_insert_and_search() {
        let mut db = CoreDB::new();

        // DDL — table definition
        db.execute(
            "CREATE TABLE articles (
                _key       TEXT PRIMARY KEY,
                title      TEXT,
                embedding  VECTOR
            )",
        )
        .unwrap();

        // CREATE INDEX on an empty table must always succeed — schema hint is recorded
        // and the index will be built automatically when vectors are inserted.
        db.execute("CREATE INDEX ON articles USING hnsw (embedding)").unwrap();

        let schema = db.schemas.get("articles").expect("schema must exist");
        assert!(
            schema.indexes.vector.contains(&"embedding".to_string()),
            "embedding must be in indexes.vector after CREATE INDEX"
        );

        // INSERT with vector — HNSW is rebuilt automatically.
        db.execute(
            "INSERT INTO articles (_key, title, embedding) \
             VALUES ('a1', 'Rust', [1.0, 0.0, 0.0, 0.0])",
        )
        .unwrap();

        // Query works without an explicit REINDEX.
        let results = db
            .query("SELECT * FROM articles WHERE VECTOR_NEAR(embedding, [1.0, 0.0, 0.0, 0.0], 5)")
            .unwrap()
            .collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].slug, "articles/a1");
    }

    // ── SQL vector INSERT / UPDATE ─────────────────────────────────────────────

    #[test]
    fn test_sql_insert_with_vector() {
        let mut db = CoreDB::new();
        db.execute(
            "INSERT INTO articles (_key, title, embedding) \
             VALUES ('a1', 'Rust', [1.0, 0.0, 0.0, 0.0])",
        )
        .unwrap();

        // Node must exist
        assert!(db.contains("articles/a1"));

        // Vector must be queryable
        let v = db.get_vector("articles/a1", "embedding").expect("vector must be stored");
        assert_eq!(v, &[1.0_f32, 0.0, 0.0, 0.0]);

        // Must show up in VECTOR_NEAR
        let results = db
            .query("SELECT * FROM articles WHERE VECTOR_NEAR(embedding, [1.0, 0.0, 0.0, 0.0], 5)")
            .unwrap()
            .collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].slug, "articles/a1");
    }

    #[test]
    fn test_sql_update_vector() {
        let mut db = CoreDB::new();
        db.execute(
            "INSERT INTO articles (_key, title, embedding) \
             VALUES ('a1', 'Rust', [1.0, 0.0, 0.0, 0.0])",
        )
        .unwrap();

        // Update the vector
        db.execute(
            "UPDATE articles SET embedding = [0.0, 1.0, 0.0, 0.0] WHERE _key = 'a1'",
        )
        .unwrap();

        let v = db.get_vector("articles/a1", "embedding").expect("vector must survive update");
        assert_eq!(v, &[0.0_f32, 1.0, 0.0, 0.0]);

        // Search with the new vector — must return a1
        let results = db
            .collection("articles")
            .vector_near("embedding", vec![0.0, 1.0, 0.0, 0.0], 5)
            .collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].slug, "articles/a1");
    }

    // ── Vector guardrail tests ─────────────────────────────────────────────────

    /// compact() must survive with vectors intact. Before this fix, compact()
    /// silently dropped all vector data (snapshot had no vectors field).
    #[test]
    fn test_vectors_survive_compact_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        // Write node + vector, then compact
        {
            let mut db = CoreDB::open(path).unwrap();
            db.put("docs/d1", r#"{"_collection":"docs"}"#).unwrap();
            db.put_vector("docs/d1", "emb", &[1.0_f32, 0.0, 0.0]).unwrap();
            db.compact().unwrap();
        }

        // Cold open — must see the vector
        {
            let db = CoreDB::open(path).unwrap();
            let v = db.get_vector("docs/d1", "emb").expect("vector must survive compact");
            assert_eq!(v, &[1.0_f32, 0.0, 0.0]);
        }
    }

    /// WAL entries after the last compact() must also survive a cold open.
    #[test]
    fn test_vectors_survive_cold_restart_via_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        // Write and drop — no compact, so data lives only in WAL
        {
            let mut db = CoreDB::open(path).unwrap();
            db.put("docs/d1", r#"{"_collection":"docs"}"#).unwrap();
            db.put_vector("docs/d1", "emb", &[0.5_f32, 0.5]).unwrap();
        }

        // Cold reload — WAL replay must restore the vector
        {
            let db = CoreDB::open(path).unwrap();
            let v = db.get_vector("docs/d1", "emb").expect("vector must survive WAL replay");
            assert_eq!(v, &[0.5_f32, 0.5]);
        }
    }

    /// `ST_AsGeoJSON(field)` in SELECT must return the geometry value as a
    /// JSON text string (Value::String), matching PostGIS semantics.
    /// Without AS alias the output key is the inner field name.
    #[test]
    fn test_st_asgeojson_select() {
        let mut db = CoreDB::new();
        db.put(
            "places/mel",
            r#"{"_collection":"places","name":"Melbourne",
                "geometry":{"type":"Point","coordinates":[144.9631,-37.8136]}}"#,
        )
        .unwrap();

        // Without alias — output key should be "geometry"
        let hits: Vec<_> = db
            .query("SELECT ST_AsGeoJSON(geometry) FROM places")
            .unwrap()
            .collect();
        assert_eq!(hits.len(), 1);
        let payload = hits[0].payload.as_ref().unwrap();
        let geom_str = payload["geometry"].as_str()
            .expect("ST_AsGeoJSON must return a string value");
        let geom: serde_json::Value = serde_json::from_str(geom_str).unwrap();
        assert_eq!(geom["type"], "Point");
        assert_eq!(geom["coordinates"][0].as_f64().unwrap(), 144.9631);

        // With alias — output key should be "geom"
        let hits: Vec<_> = db
            .query("SELECT ST_AsGeoJSON(geometry) AS geom FROM places")
            .unwrap()
            .collect();
        let payload = hits[0].payload.as_ref().unwrap();
        assert!(payload["geom"].is_string(), "aliased column must be present as string");
    }

    /// `ST_GeomFromGeoJSON('...')` in INSERT VALUES must store the geometry as
    /// a proper JSON object (not a raw string) in the node payload.
    #[test]
    fn test_st_geomfromgeojson_insert() {
        let mut db = CoreDB::new();
        db.execute(
            r#"INSERT INTO places (_key, name, geometry)
               VALUES ('fitzroy', 'Fitzroy',
                       ST_GeomFromGeoJSON('{"type":"Point","coordinates":[144.9775,-37.7963]}'))"#,
        )
        .unwrap();

        let hits: Vec<_> = db
            .query("SELECT * FROM places WHERE _key = 'fitzroy'")
            .unwrap()
            .collect();
        assert_eq!(hits.len(), 1);
        let payload = hits[0].payload.as_ref().unwrap();
        // geometry must be a JSON object, not a raw string
        assert!(
            payload["geometry"].is_object(),
            "geometry must be stored as a JSON object, not a string"
        );
        assert_eq!(payload["geometry"]["type"], "Point");
        assert_eq!(
            payload["geometry"]["coordinates"][0].as_f64().unwrap(),
            144.9775
        );
    }

    /// Deleting a node must remove its vector from the index immediately.
    /// A subsequent search must not see the deleted node.
    #[test]
    fn test_remove_node_removes_vector() {
        let mut db = CoreDB::new();
        db.put("docs/d1", r#"{"_collection":"docs"}"#).unwrap();
        db.put("docs/d2", r#"{"_collection":"docs"}"#).unwrap();
        db.put_vector("docs/d1", "emb", &[1.0_f32, 0.0]).unwrap();
        db.put_vector("docs/d2", "emb", &[0.0_f32, 1.0]).unwrap();

        db.remove("docs/d1");

        // Direct get must return None
        assert!(db.get_vector("docs/d1", "emb").is_none(), "vector must be gone after remove");

        // Search must not return d1
        let results = db
            .collection("docs")
            .vector_near("emb", vec![1.0_f32, 0.0], 10)
            .collect();
        assert!(results.iter().all(|h| h.slug != "docs/d1"), "d1 must not appear in search after remove");
    }

    /// compact() must only persist vectors whose node still exists.
    /// Orphan entries (removed node, stale vector) must be pruned silently.
    #[test]
    fn test_compact_prunes_orphan_vectors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        {
            let mut db = CoreDB::open(path).unwrap();
            db.put("docs/d1", r#"{"_collection":"docs"}"#).unwrap();
            db.put_vector("docs/d1", "emb", &[1.0_f32]).unwrap();
            db.remove("docs/d1"); // node deleted, vector cleaned up
            db.compact().unwrap();
        }

        // Reload — neither node nor vector should exist
        {
            let db = CoreDB::open(path).unwrap();
            assert!(!db.contains("docs/d1"));
            assert!(db.get_vector("docs/d1", "emb").is_none());
        }
    }

    #[test]
    fn test_vector_near_sql() {
        let mut db = CoreDB::new();
        db.put(
            "articles/a1",
            r#"{"_collection":"articles","_key":"a1","title":"Rust"}"#,
        )
        .unwrap();
        db.put_vector("articles/a1", "embedding", &[0.1, 0.2, 0.3, 0.4])
            .unwrap();

        let results = db
            .query("SELECT * FROM articles WHERE VECTOR_NEAR(embedding, [0.1, 0.2, 0.3, 0.4], 5)")
            .unwrap()
            .collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].slug, "articles/a1");
    }
}
