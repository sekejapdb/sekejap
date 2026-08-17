//! # sekejap core — the database engine (`CoreDB`)
//!
//! This is the heart of sekejap. Everything else (the SQL parser, the wrappers,
//! the server) is a thin layer over the one type defined here: [`CoreDB`]. It is
//! an **embedded** database — it runs inside your program like SQLite, not as a
//! separate server you connect to. And it is **multi-model**: the same store
//! holds records (like a document DB), edges between them (like a graph DB), and
//! specialized indexes for text, geo, and vector search — all queried together.
//!
//! ## The one idea to hold on to: disk-first
//!
//! A record's bytes (its "payload") live **on disk**, not in RAM. What stays in
//! memory is small: for each node, its identity, where its payload sits on disk
//! (`payload_offset` + `payload_len`), and the compact index structures needed to
//! *find* things fast. So memory usage tracks the number of records and their
//! indexes — never the total size of the data. A database far bigger than RAM
//! still opens instantly and answers queries with a bounded memory footprint.
//!
//! ## How a request flows through
//!
//! **Writing** (`put` / `link`): the change is first appended to the
//! write-ahead log (WAL) on disk so a crash can't lose it, then applied to the
//! in-memory maps and the on-disk payload store. `compact()` later folds the WAL
//! into a clean snapshot so the next open is fast.
//!
//! **Reading** (`query` / the chainable builder): a query becomes a list of
//! [`Step`]s — `Collection`, `WhereEq`, `Forward` (an edge hop), `Sort`, `Take`,
//! and so on. The executor ([`Set`] in `query.rs`) runs those steps, using an
//! index to *seed* a small candidate set whenever it can, and only reads payloads
//! from disk for the rows that survive. The SQL surface (`sql.rs`) is just a
//! front-end that compiles text into the very same `Vec<Step>`.
//!
//! ## Core components in this file
//!
//! - [`CoreDB`] — owns everything: the node map, the edge store, the payload
//!   store, and every index. All reads and writes go through it.
//! - `NodeData` — the small in-RAM record for one node: its slug (`collection/key`),
//!   its cached collection name, spatial metadata, and the offset/length of its
//!   payload on disk. Deliberately does **not** hold the payload itself.
//! - `PayloadStore` — the on-disk record bytes (`payloads.bin`), read on demand
//!   by offset. Ephemeral in-memory databases keep the bytes in RAM instead.
//! - The index families (scalar/btree, graph adjacency, spatial grid, text,
//!   vector) — each answers a query with a set of node ids, the shared currency
//!   that lets one query combine several of them.
//!
//! See `docs/developer/` for the architecture in depth, and
//! `docs/developer/invariants.md` for the rules this engine must never break.
//!
//! ## Quick start
//!
//! In-memory (ephemeral — nothing is written to disk):
//! ```
//! use sekejap::CoreDB;
//!
//! let mut db = CoreDB::new();
//! db.put("alice", r#"{"name":"Alice","age":30,"_collection":"users"}"#).unwrap();
//! db.put("bob",   r#"{"name":"Bob",  "age":25,"_collection":"users"}"#).unwrap();
//! db.link("alice", "bob", "follows"); // a naked, weightless edge
//!
//! // Start at alice, hop along "follows" edges, collect where we land.
//! let hits = db.one("alice").forward("follows").collect();
//! assert_eq!(hits[0].slug, "bob");
//! ```
//!
//! Persistent (WAL-backed — survives restarts):
//! ```no_run
//! use sekejap::CoreDB;
//!
//! let mut db = CoreDB::open("mydb").unwrap();
//! db.put("alice", r#"{"name":"Alice","_collection":"users"}"#).unwrap();
//! db.compact().unwrap();  // fold the WAL into a snapshot so the next open is fast
//! ```

pub mod bm25;
pub mod engine;
pub mod geo;
pub mod pg;
mod query;
pub mod scalar;
pub mod search;
pub mod serve;
pub mod sql;
mod storage;
pub mod text_index;
pub mod vector;

pub use vector::{CosineDistance, Distance, DotProduct, L2Distance};

// ── The two ways to open a database ──────────────────────────────────────────
//
// Which one you want depends on a single question: is the database part of your
// app, or is it running as a service?

/// Open a database that lives **inside your app** — it starts and stops with the
/// program. A mobile app, a game, an analysis script.
///
/// One process, one writer, nothing extra running. This is what the defaults are
/// tuned for: fast startup, small memory, quick single-threaded queries.
///
/// Creates the directory if it doesn't exist.
///
/// ```rust,no_run
/// let mut db = sekejap::open("./mydb")?;
/// db.put("users/alice", r#"{"_collection":"users","_key":"alice"}"#)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// Use [`open_as_service`] instead if the database keeps running and serves others.
pub fn open(path: impl AsRef<Path>) -> io::Result<CoreDB> {
    CoreDB::open(path)
}

/// Open a database that **runs as a service** — long-lived, looking after its own
/// data: a small server, a robot, an IoT gateway. Even when it serves one person.
///
/// Compared with [`open`], reads don't wait behind writes, memory stays bounded
/// over long runs, and it compacts itself. See [`engine::Engine::open_as_service`]
/// for the details and the trade-offs.
///
/// ```rust,no_run
/// let db = sekejap::open_as_service("/var/lib/app/db")?;
/// let row = db.get("venues/v1");   // lock-free, even while a write is running
/// # Ok::<(), String>(())
/// ```
pub fn open_as_service(path: impl AsRef<Path>) -> Result<engine::Engine, String> {
    let p = path.as_ref().to_str().ok_or("database path is not valid UTF-8")?;
    engine::Engine::open_as_service(p)
}

pub use query::{CmpOp, DestWhere, Hit, MathExpr, MatchAggReturn, MatchAggStart, MatchAggStmt, Set, Step, VecMetric, WhereValue, WithExpr, WithOutExpr, WithRow, WithStage};
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

// ── Ring-2 store-format migration framework ────────────────────────────────────
//
// `SNAPSHOT_FORMAT_VERSION` is the authoritative *whole-store* format version:
// the snapshot manifest is the first file read on open and gates the store, so a
// store-format bump is a snapshot-version bump. Ring 2 (source-of-truth files —
// payloads, topology, raw vectors, WAL) is a real compatibility surface (see
// docs/developer/invariants.md, Pillar 4). Three outcomes on open:
//   • store == code → read normally.
//   • store  > code → fail loud (`newer_format_msg`) — never corrupt.
//   • store  < code → apply registered migrations in sequence, else fall back to
//                     backward-compatible reading (older snapshots parse via serde
//                     defaults; derived accelerators rebuild).

/// One source-of-truth (Ring 2) format migration: upgrades a store directory in
/// place from format `from` to `from + 1`. Registered (ascending, contiguous) in
/// `STORE_MIGRATIONS` and applied in sequence on open.
struct StoreMigration {
    /// The store version this migration upgrades *from* (it produces `from + 1`).
    from: u32,
    /// One-line description of the on-disk change (for logs / tooling).
    #[allow(dead_code)]
    describe: &'static str,
    /// Transform the store directory in place from `from` to `from + 1`.
    run: fn(dir: &std::path::Path) -> io::Result<()>,
}

/// Registered store-format migrations. **Empty today** — the format is at v3 and
/// older snapshots still parse via backward-compatible defaults, so no explicit
/// upgrade is due. The *next* Ring-2 format change registers its reader here so an
/// old store is upgraded on open rather than stuck at the fail-loud safety floor.
const STORE_MIGRATIONS: &[StoreMigration] = &[];

/// Upgrade a store directory from format `from` toward `to` by applying every
/// registered migration covering `[from, to)` in ascending order. Returns the
/// version actually reached: `to` if the chain was complete, or the first version
/// with no registered migration (the caller then reads with backward-compatible
/// parsing). Errors only if a registered migration itself fails.
fn apply_store_migrations(
    dir: &std::path::Path,
    from: u32,
    to: u32,
    migrations: &[StoreMigration],
) -> io::Result<u32> {
    let mut v = from;
    while v < to {
        match migrations.iter().find(|m| m.from == v) {
            Some(m) => {
                (m.run)(dir)?;
                v += 1;
            }
            None => break, // no explicit migration for this step → backward-compat read
        }
    }
    Ok(v)
}

/// Bump each constant when the corresponding index algorithm changes in a way
/// that makes indexes built by the previous version produce wrong results.
const GIN_INDEX_VERSION:     u32 = 3; // sorted trigram dir + blob (mmap-served) 2026-08
/// How many rows may sit in a search index's delta before the collection's index
/// is rebuilt. The delta is itself rebuilt per write, so its cost is `O(delta)`;
/// this bounds that, and the amortised rebuild works out far below the `O(rows)`
/// it replaces.
const SEARCH_DELTA_MERGE_DOCS: usize = 256;

/// Above this many rows in one UPDATE, rebuilding a text index beats re-indexing
/// the rows one by one.
const UPDATE_INCREMENTAL_ROWS: usize = 256;

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
    /// The open ends of a *numeric* range, in this ordering.
    ///
    /// Keys sort `Null < Bool < Number < Str`, so an unbounded end on a numeric
    /// predicate sweeps straight out of the numbers: `WHERE age > 20` answered from
    /// a btree included rows whose `age` is the string "25", because `Str` outranks
    /// every `Number`. A scan of the same predicate excluded them, since a string
    /// is not greater than twenty — so the answer depended on whether an index
    /// existed. These bracket the numbers and nothing else.
    pub(crate) fn numbers_start() -> Self { FieldKey::Bool(true) }
    pub(crate) fn numbers_end() -> Self { FieldKey::Str(String::new()) }

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

    /// Encode for the on-disk field index (`storage::fieldstore`): 1 tag byte +
    /// payload. Order is recovered by decode-and-compare (Ord), so the byte
    /// layout need not be order-preserving.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        match self {
            FieldKey::Null => out.push(0),
            FieldKey::Bool(b) => { out.push(1); out.push(*b as u8); }
            FieldKey::Number(OrdF64(f)) => { out.push(2); out.extend_from_slice(&f.to_le_bytes()); }
            FieldKey::Str(s) => { out.push(3); out.extend_from_slice(s.as_bytes()); }
        }
    }

    /// Decode a key written by [`FieldKey::encode`]. Returns `Null` on malformed
    /// input (defensive — a corrupt key sorts first, never panics a query).
    pub(crate) fn decode(bytes: &[u8]) -> FieldKey {
        match bytes.first() {
            Some(0) => FieldKey::Null,
            Some(1) => FieldKey::Bool(bytes.get(1).copied().unwrap_or(0) != 0),
            Some(2) if bytes.len() >= 9 => {
                let mut a = [0u8; 8];
                a.copy_from_slice(&bytes[1..9]);
                FieldKey::Number(OrdF64(f64::from_le_bytes(a)))
            }
            Some(3) => FieldKey::Str(String::from_utf8_lossy(&bytes[1..]).into_owned()),
            _ => FieldKey::Null,
        }
    }
}

// ── Internal types ────────────────────────────────────────────────────────────

/// A btree field-index handle over either the heap overlay or the mmap'd base.
/// Unifies `get`/`range`/`iter` so the query executor shares one code path for
/// paged and in-memory indexes. Mapped lookups decode postings into transient
/// owned `Vec`s; the retained bytes stay in reclaimable mmap page cache.
/// The compacted store as an ordered list of immutable segments, oldest first.
///
/// Today there is exactly one, and every accessor below behaves precisely as the
/// single `Option<Arc<MappedTopology>>` it replaces. The type exists so that
/// "consult the base" becomes "consult the segments" at every call site *before*
/// there is more than one — the alternative is changing the meaning of a field
/// while its type stays the same, which is the mistake that produced twelve
/// silent bugs in this file already.
#[derive(Clone, Default)]
pub(crate) struct Segments {
    segs: Vec<std::sync::Arc<storage::topology::MappedTopology>>,
    /// `slot -> (segment, local id)`. Absent means the identity mapping, which is
    /// what a single-segment store is — so every database written before this file
    /// existed reads correctly without migration.
    slots: Option<std::sync::Arc<storage::slotmap::MappedSlots>>,
}

/// Edge type names, as a file of their own.
///
/// `dict.bin` carries them normally, but it is built from the edge list a
/// compaction writes — and paged adjacency deliberately writes no edge list, so
/// every edge came back nameless on reopen and `SHOW EDGES` was empty. The names
/// are the one part of the graph that is not in the paged store: an edge holds its
/// type as a hash, and a hash does not turn back into a word by itself.
///
/// There are a handful of them, so this is a few hundred bytes rewritten whenever
/// the set changes, not something whose cost tracks the store.
///
/// Layout: `[magic 8][count u32][ (hash u64, len u16, bytes) x count ]`.
mod edge_type_names {
    use std::io;
    use std::path::Path;

    const MAGIC: &[u8; 8] = b"SKETYP\0\0";
    pub(super) const FILE: &str = "edge_types.bin";
    /// The same table, for collection names. Identical shape, different file — a
    /// paged store keys collections by hash and keeps the name in each node
    /// record, so without this `SHOW TABLES` had to read every node in the
    /// database to learn what the tables were called.
    pub(super) const COLL_FILE: &str = "coll_names.bin";

    pub(super) fn write(path: &Path, types: &[(u64, &str)]) -> io::Result<()> {
        let mut out = Vec::with_capacity(16 + types.len() * 24);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(types.len() as u32).to_le_bytes());
        for (hash, name) in types {
            out.extend_from_slice(&hash.to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(name.as_bytes());
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &out)?;
        std::fs::rename(&tmp, path)
    }

    /// Read the table back. Anything malformed yields the entries read so far
    /// rather than an error: a lost name makes `SHOW EDGES` less informative, and
    /// refusing to open the database over it would be the worse failure.
    pub(super) fn read(path: &Path) -> Vec<(u64, String)> {
        let Ok(b) = std::fs::read(path) else { return Vec::new() };
        if b.len() < 12 || &b[0..8] != MAGIC { return Vec::new() }
        let count = u32::from_le_bytes(b[8..12].try_into().unwrap()) as usize;
        let mut out = Vec::with_capacity(count);
        let mut at = 12usize;
        for _ in 0..count {
            if at + 10 > b.len() { break }
            let hash = u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
            let len = u16::from_le_bytes(b[at + 8..at + 10].try_into().unwrap()) as usize;
            at += 10;
            if at + len > b.len() { break }
            let Ok(name) = std::str::from_utf8(&b[at..at + len]) else { break };
            out.push((hash, name.to_string()));
            at += len;
        }
        out
    }
}

/// The durable graph, forward and reverse, in slotted pages keyed by slug hash.
///
/// This is the base a read falls through to when `Config::paged_adjacency` is on,
/// in place of a segment's `adj_fwd.bin` / `adj_rev.bin`. The two are the same
/// data and answer the same questions; the difference is that CSR can only be
/// rebuilt and this can be written.
///
/// Both directions are kept because both are asked for — `edges_from` walks one
/// and `edges_to` the other — and neither can be derived from the other without a
/// scan. An edge is therefore stored twice, which is what CSR does too.
pub(crate) struct PagedAdjacency {
    fwd: storage::adjstore::AdjStore,
    rev: storage::adjstore::AdjStore,
    /// Edge attributes, one record each, addressed by the record id the edge
    /// carries. A plain record store rather than an indexed one: the record id
    /// *is* the reference, so there is nothing to look up and no id to allocate.
    ///
    /// Separate from the edges themselves because an edge is reached through a
    /// `storage::edgestore::Edge`, which carries a single reference and no way
    /// back to the owner whose record the attributes would otherwise sit in.
    meta: storage::recordstore::RecordStore,
}

impl PagedAdjacency {
    fn open(dir: &Path) -> io::Result<Self> {
        let page = storage::pagestore::DEFAULT_PAGE_SIZE;
        let meta_path = dir.join("adjp_meta.rec");
        let meta = match storage::recordstore::RecordStore::open(&meta_path)? {
            Some(m) => m,
            None => storage::recordstore::RecordStore::create(&meta_path, page)?,
        };
        Ok(Self {
            fwd: storage::adjstore::AdjStore::open(dir, "adjp_fwd", page)?,
            rev: storage::adjstore::AdjStore::open(dir, "adjp_rev", page)?,
            meta,
        })
    }

    fn dir(&self, forward: bool) -> &storage::adjstore::AdjStore {
        if forward { &self.fwd } else { &self.rev }
    }

    /// A node's stored edges in the shape the merge expects.
    ///
    /// Returns `None` both when the node has no edges and when the read fails. An
    /// unreadable base reads as absent rather than propagating an error into every
    /// graph query, which is the contract the mapped base already has.
    fn edges(&self, hash: u64, forward: bool) -> Option<Vec<storage::topology::MappedEdge>> {
        // `edges_as`, not `edges`: decoded straight into the type the graph
        // surface reads, so the lookup allocates once instead of four times.
        self.dir(forward)
            .edges_as(hash, |other_hash, edge_type_hash, meta_ref| {
                storage::topology::MappedEdge { other_hash, edge_type_hash, meta_ref }
            })
            .ok()
            .flatten()
    }

    /// The stored bytes of one edge's attributes.
    fn meta_bytes(&self, meta_ref: u64) -> Option<Vec<u8>> {
        self.meta.read(storage::recordstore::RecordId(meta_ref)).ok().flatten()
    }

    fn is_empty(&self) -> bool {
        self.fwd.owner_count() == 0 && self.rev.owner_count() == 0
    }

    /// Copy a store's existing CSR graph into pages, once.
    ///
    /// Turning paged adjacency on makes this the base a read falls through to,
    /// *instead of* the segments — consulting both would double every edge that had
    /// been folded in. So a store whose graph is still in `adj_fwd.bin` has to have
    /// it moved across, or the flag would appear to delete the entire graph.
    ///
    /// This is a migration and it is honestly O(edges): every edge is read once and
    /// written once. It runs at open, only when the paged store is empty and a
    /// segment has edges, so it happens once in a database's life rather than on
    /// every compaction — which is the difference this whole direction is about.
    ///
    /// It streams node by node. Collecting the graph first would cost RAM
    /// proportional to the store, which is what Law 1 forbids and what made
    /// compaction unaffordable in the first place.
    fn migrate_from(&mut self, base: &storage::topology::MappedTopology) -> io::Result<usize> {
        use storage::adjstore::{AdjEdge, NO_META};
        let mut moved = 0usize;
        for id in 0..base.node_count() as u64 {
            let Some(hash) = base.hash_of(id) else { continue };
            for (forward, edges) in [
                (true, base.fwd_by_hash(hash)),
                (false, base.rev_by_hash(hash)),
            ] {
                let Some(edges) = edges else { continue };
                let mut list = Vec::with_capacity(edges.len());
                for e in edges {
                    // Attributes move too, out of the side file and into a record
                    // whose id does not change when anything is rebuilt.
                    let meta_ref = match base.edge_meta_bytes(e.meta_ref as u32) {
                        Some(bytes) if e.meta_ref != u64::MAX => self.meta.insert(bytes)?.0,
                        _ => NO_META,
                    };
                    list.push(AdjEdge {
                        other: e.other_hash,
                        edge_type: e.edge_type_hash,
                        meta_ref,
                    });
                }
                if list.is_empty() { continue }
                if forward { moved += list.len() }
                let store = if forward { &mut self.fwd } else { &mut self.rev };
                store.add_many(hash, &list)?;
            }
        }
        self.sync()?;
        Ok(moved)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.fwd.sync()?;
        self.rev.sync()?;
        self.meta.sync()
    }
}

impl Segments {
    pub(crate) fn is_empty(&self) -> bool { self.segs.is_empty() }

    /// Newest first — a later segment shadows an earlier one for the same key.
    pub(crate) fn newest_first(
        &self,
    ) -> impl Iterator<Item = &std::sync::Arc<storage::topology::MappedTopology>> {
        self.segs.iter().rev()
    }

    /// Replace everything with a single segment. What compaction does today.
    pub(crate) fn replace_with(
        &mut self,
        seg: std::sync::Arc<storage::topology::MappedTopology>,
    ) {
        self.segs.clear();
        self.segs.push(seg);
    }

    pub(crate) fn clear(&mut self) { self.segs.clear(); }

    /// The segment holding `hash`, and its local id, searching newest first.
    ///
    /// `idx.bin` answers with a **slot**, which the slot table turns into a
    /// concrete location. With one segment the two coincide, which is why a store
    /// written before `slots.bin` existed resolves correctly with no table at all.
    pub(crate) fn resolve(
        &self,
        hash: u64,
    ) -> Option<(&storage::topology::MappedTopology, u64)> {
        for seg in self.newest_first() {
            let Some(slot) = seg.resolve(hash) else { continue };
            return self.locate(slot).or(Some((seg.as_ref(), slot)));
        }
        None
    }

    /// Turn a slot into the segment and local id currently holding it.
    #[inline]
    pub(crate) fn locate(
        &self,
        slot: u64,
    ) -> Option<(&storage::topology::MappedTopology, u64)> {
        let Some(map) = self.slots.as_ref() else {
            // No table: the identity mapping of a single-segment store.
            return self.segs.first().map(|s| (s.as_ref(), slot));
        };
        let (seg, local) = map.locate(slot)?;
        self.segs.get(seg as usize).map(|s| (s.as_ref(), local))
    }

    /// Attach the slot table read from disk. `None` keeps the identity mapping.
    pub(crate) fn set_slots(
        &mut self,
        slots: Option<std::sync::Arc<storage::slotmap::MappedSlots>>,
    ) {
        self.slots = slots;
    }

    /// First non-`None` answer, newest segment first.
    pub(crate) fn find<T>(
        &self,
        mut f: impl FnMut(&storage::topology::MappedTopology) -> Option<T>,
    ) -> Option<T> {
        self.newest_first().find_map(|s| f(s.as_ref()))
    }
}

pub(crate) enum FieldIndexRef<'a> {
    Heap(&'a std::collections::BTreeMap<FieldKey, Vec<u64>>),
    Mapped(&'a storage::fieldstore::MappedFieldStore),
}

impl<'a> FieldIndexRef<'a> {
    pub(crate) fn len(&self) -> usize {
        match *self {
            FieldIndexRef::Heap(m) => m.len(),
            FieldIndexRef::Mapped(s) => s.len(),
        }
    }

    /// Postings for an exact key. `Cow` avoids cloning the heap posting list.
    pub(crate) fn get_eq(&self, k: &FieldKey) -> Option<std::borrow::Cow<'a, [u64]>> {
        match *self {
            FieldIndexRef::Heap(m) => m.get(k).map(|v| std::borrow::Cow::Borrowed(v.as_slice())),
            FieldIndexRef::Mapped(s) => s.get_eq(k).map(std::borrow::Cow::Owned),
        }
    }

    /// Concatenated postings for all keys in the `(lo, hi)` window.
    pub(crate) fn range_postings(
        &self,
        lo: std::ops::Bound<&FieldKey>,
        hi: std::ops::Bound<&FieldKey>,
    ) -> Vec<u64> {
        match *self {
            FieldIndexRef::Heap(m) => m
                .range((lo, hi))
                .flat_map(|(_, ids)| ids.iter().copied())
                .collect(),
            FieldIndexRef::Mapped(s) => s.range_postings(lo, hi),
        }
    }

    /// All `(key, postings)` pairs in key order (`rev` = descending).
    pub(crate) fn iter_kv(&self, rev: bool) -> Vec<(FieldKey, Vec<u64>)> {
        match *self {
            FieldIndexRef::Heap(m) => {
                if rev {
                    m.iter().rev().map(|(k, v)| (k.clone(), v.clone())).collect()
                } else {
                    m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                }
            }
            FieldIndexRef::Mapped(s) => s.iter_kv(rev),
        }
    }
}

/// Hash a string with SeaHash (fast, non-cryptographic, deterministic).
pub(crate) fn sk_hash(s: &str) -> u64 {
    seahash::hash(s.as_bytes())
}

/// Lower-hex encode bytes (for embedding a field name in a fieldstore filename).
fn hex_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.as_bytes() {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    out
}

/// Inverse of [`hex_encode`]. Returns `None` on malformed input.
fn hex_decode(s: &str) -> Option<String> {
    if s.len() % 2 != 0 {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16)? as u8;
        let lo = (bytes[i + 1] as char).to_digit(16)? as u8;
        out.push((hi << 4) | lo);
        i += 2;
    }
    String::from_utf8(out).ok()
}

/// Bytes of owner tag on a paged payload record.
const OWNER_TAG: usize = 4;
/// "This read has no node hash to check against." Also what a payload written
/// without one is tagged with, so such records stay readable from every path.
const OWNER_UNKNOWN: u64 = 0;
/// The low 32 bits of a node hash, which is what a paged payload record carries.
/// `0` is reserved for "unknown", so a hash that would land there is nudged.
fn owner_tag(owner: u64) -> u32 {
    match owner as u32 { 0 => 1, t => t }
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
    /// Where the bytes actually live (RAM vs. disk vs. remote) — see [`PayloadInner`].
    inner: PayloadInner,
}

// ── Read-only mmap (shared between PayloadStore and VectorStore) ─────────────
#[cfg(unix)]
use storage::mmap::MmapView;

/// The three places a payload store can keep its bytes. `PayloadStore` presents
/// one `get(offset, len)` API over whichever of these it is.
enum PayloadInner {
    /// Ephemeral database: all record bytes held in one growing `Vec<u8>`.
    Memory { data: Vec<u8> },
    /// Persistent database, paged: bytes live in slotted pages with a free list,
    /// so deleting or updating a record returns its space immediately instead of
    /// leaving a hole for a later rewrite to squeeze out.
    ///
    /// The `offset` half of a `(offset, len)` pair is a
    /// [`RecordId`](storage::recordstore::RecordId) here, not a byte position —
    /// which is why [`PayloadStore::absolute_offsets`] exists: the read paths that
    /// do arithmetic on byte offsets, or coalesce neighbouring records into one
    /// read, have to take a different route.
    Paged { store: storage::recordstore::RecordStore },
    /// Persistent database: bytes in the `payloads.bin` file. Reads go through
    /// the `mmap` view when present (zero-copy), else fall back to `pread`.
    Disk {
        file: std::fs::File,
        total_len: u64,
        #[cfg(unix)]
        mmap: Option<std::sync::Arc<MmapView>>,
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
/// Carry a disk failure out through an error type that cannot name one.
///
/// The public write API returns `Result<_, serde_json::Error>`, chosen when the
/// only thing that could go wrong with a write was the JSON. That is why every
/// disk write on the path below it used to `.expect()`: there was nowhere to put
/// an `io::Error`, so the code took the only other exit and aborted the process.
///
/// A full disk is not a reason to kill the application. Until the signature can
/// carry an `io::Error` properly — a change that reaches every language binding —
/// the failure travels as a message, and the caller gets an `Err` it can act on
/// instead of a crash it cannot.
fn wal_write_failed(e: io::Error) -> serde_json::Error {
    use serde::ser::Error as _;
    serde_json::Error::custom(format!("sekejap: WAL write failed: {e}"))
}

/// Make the *names* in a directory durable, not just the bytes behind them.
///
/// Writing a file safely is two separate promises. `sync_all` on the file
/// promises its contents survive a power cut. `rename` promises the new contents
/// replace the old ones atomically — but that promise is a change to the
/// **directory**, and a directory is a file like any other: until it is synced,
/// the rename lives in the page cache and a crash may undo it.
///
/// Every durable file in a sekejap store is published by rename, so without this
/// a compaction had no crash-atomic commit point at all. Recovery could restore
/// the old `nodes.bin` while keeping the new `snapshot.json` that describes the
/// new one, and the pointers in the surviving snapshot would address a file that
/// no longer matched. Nothing would report an error; the reads would simply be
/// of the wrong bytes.
///
/// Not an error if the platform declines. Directory fsync is a POSIX
/// requirement; on Windows the equivalent guarantee comes from the filesystem
/// and opening a directory as a file is not permitted, so the call is skipped
/// rather than failed.
fn fsync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

fn payload_write_failed(e: io::Error) -> serde_json::Error {
    use serde::ser::Error as _;
    serde_json::Error::custom(format!("sekejap: payload write failed: {e}"))
}

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
        // Wrap in Arc so a read-only Snapshot can share this mmap (base payloads)
        // for lock-free reads — see docs/developer/notes/snapshot-reads-design.md.
        #[cfg(unix)]
        let mmap = MmapView::try_new(&file, total_len as usize).map(std::sync::Arc::new);
        Ok(Self { binary: false, field_table: storage::skbin::FieldTable::new(), inner: PayloadInner::Disk {
            file,
            total_len,
            #[cfg(unix)]
            mmap,
        } })
    }

    /// Whether the record bytes are on disk rather than in this process's memory.
    ///
    /// **Paged counts.** It was written when `Disk` was the only durable variant,
    /// and paged payloads were added beside it without revisiting the question —
    /// so a paged store answered "not on disk" about a file it had just written.
    ///
    /// What that decided was the shape of `snapshot.json`. A durable store gets a
    /// *manifest* snapshot: the nodes and edges live in the files, and the
    /// snapshot carries only schemas and index metadata. A store that answers
    /// "not on disk" gets every node embedded in the JSON **with its whole
    /// payload**, because for an in-memory database that is the only durable copy.
    ///
    /// So the default layout wrote its entire dataset twice — once into the paged
    /// files, once into `snapshot.json` — and the JSON copy was the bigger of the
    /// two: 39.8 MB against 36.4 MB of payloads at 200 000 rows, growing linearly.
    /// Every open then parsed all of it, which is 81% of the time an open took and
    /// the reason opening cost 8.5x more for 10x the rows. Both laws, on the path
    /// every process takes.
    fn is_disk(&self) -> bool {
        matches!(self.inner, PayloadInner::Disk { .. } | PayloadInner::Paged { .. })
    }

    /// Whether `(offset, len)` pairs are byte positions in one flat region.
    ///
    /// True for the memory and disk variants, where a record's bytes sit at a
    /// known offset and neighbouring records are adjacent. False when paged: an
    /// offset is an opaque record id, so callers that slice within a record or
    /// read several at once must go through `get_raw` per record instead.
    fn absolute_offsets(&self) -> bool {
        !matches!(self.inner, PayloadInner::Paged { .. })
    }

    /// Open (or create) a paged payload store at `path`.
    fn open_paged(path: &std::path::Path) -> io::Result<Self> {
        let store = match storage::recordstore::RecordStore::open(path)? {
            Some(s) => s,
            None => storage::recordstore::RecordStore::create(
                path, storage::pagestore::DEFAULT_PAGE_SIZE)?,
        };
        Ok(Self {
            binary: false,
            field_table: storage::skbin::FieldTable::default(),
            inner: PayloadInner::Paged { store },
        })
    }

    /// Release a record's space. The point of the paged variant: an updated or
    /// deleted row's bytes come back now, rather than waiting for a rewrite of the
    /// whole store to squeeze them out. A no-op for the append-only variants,
    /// which have nowhere to put reclaimed space.
    pub(crate) fn free(&mut self, offset: u64) -> bool {
        match &mut self.inner {
            PayloadInner::Paged { store } => store
                .delete(storage::recordstore::RecordId(offset))
                .unwrap_or(false),
            _ => false,
        }
    }

    pub(crate) fn sync_pages(&mut self) -> io::Result<()> {
        match &mut self.inner {
            PayloadInner::Paged { store } => store.sync(),
            _ => Ok(()),
        }
    }

    /// Pages held and pages free — paged variant only.
    pub(crate) fn page_stats(&self) -> Option<(u64, u64)> {
        match &self.inner {
            PayloadInner::Paged { store } => Some((store.page_count(), store.free_page_count())),
            _ => None,
        }
    }

    /// Append raw bytes; returns `(offset, len)`.
    /// Panics on disk write failure (disk-full etc.) — callers do not recover.
    fn append(&mut self, bytes: &[u8]) -> io::Result<(u64, u32)> {
        self.append_owned(OWNER_UNKNOWN, bytes)
    }

    /// Append raw bytes, recording which node they belong to.
    ///
    /// In the paged variant the record is prefixed with `owner`'s low 32 bits, so a
    /// read can tell whether it landed on the right record. A record store holds
    /// anonymous bytes at a slot: damage the slot directory and the read lands on a
    /// different row's payload, returned as if it were this row's. Fuzzing produced
    /// exactly that after the node and edge records had been protected — the node
    /// record was intact and its checksum passed, and the payload it pointed at
    /// belonged to somebody else.
    ///
    /// Four bytes a row, and one comparison on read. A truncated hash makes a false
    /// accept a one-in-four-billion accident, which is the right trade for a damage
    /// detector: it is not defending against a forger.
    ///
    /// The append-only variants are untouched — their offsets are byte positions,
    /// so a read cannot land on a neighbouring record by arithmetic.
    /// # Why this returns an error instead of panicking
    ///
    /// It used to `.expect()` on the write, on the reasoning that a caller cannot
    /// recover from a failed disk write without risking corruption. The
    /// conclusion does not follow. A full disk is an ordinary condition — the
    /// commonest one a long-running database meets — and aborting the process
    /// takes the whole application down with it, on a machine that may be a robot
    /// or a gateway with nobody to restart it. Worse, it aborts *mid-write*,
    /// which is precisely the crash the write-ahead log exists to survive: the
    /// recovery path was being triggered by the code meant to avoid needing it.
    ///
    /// Returning the error lets the write fail and the database keep running.
    fn append_owned(&mut self, owner: u64, bytes: &[u8]) -> io::Result<(u64, u32)> {
        match &mut self.inner {
            // The "offset" is a record id here, not a byte position.
            PayloadInner::Paged { store } => {
                let mut tagged = Vec::with_capacity(OWNER_TAG + bytes.len());
                tagged.extend_from_slice(&owner_tag(owner).to_le_bytes());
                tagged.extend_from_slice(bytes);
                let id = store.insert(&tagged)?;
                Ok((id.0, bytes.len() as u32))
            }
            PayloadInner::Memory { data } => {
                let offset = data.len() as u64;
                data.extend_from_slice(bytes);
                Ok((offset, bytes.len() as u32))
            }
            PayloadInner::Disk { file, total_len, .. } => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileExt;
                    file.write_all_at(bytes, *total_len)?;
                }
                #[cfg(not(unix))]
                {
                    use std::io::{Seek, SeekFrom, Write};
                    file.seek(SeekFrom::Start(*total_len))?;
                    file.write_all(bytes)?;
                }
                let offset = *total_len;
                *total_len += bytes.len() as u64;
                Ok((offset, bytes.len() as u32))
            }
        }
    }

    /// Append many records at once, each tagged with the node it belongs to.
    ///
    /// `owners` is parallel to `items`. Tagging these with "unknown" instead was a
    /// quiet way to break every bulk-loaded row: the write said "no owner" and the
    /// read asked for a specific one, so the record was refused and the row read as
    /// absent. The differential audit caught it as two collections' SEARCH results
    /// coming back short.
    fn append_batch(&mut self, items: &[&[u8]], owners: &[u64])
        -> io::Result<Vec<(u64, u32)>>
    {
        if items.is_empty() { return Ok(vec![]); }
        debug_assert_eq!(items.len(), owners.len(), "an owner per record, or none can be checked");
        match &mut self.inner {
            PayloadInner::Paged { store } => items.iter().enumerate().map(|(i, bytes)| {
                let owner = owners.get(i).copied().unwrap_or(OWNER_UNKNOWN);
                let mut tagged = Vec::with_capacity(OWNER_TAG + bytes.len());
                tagged.extend_from_slice(&owner_tag(owner).to_le_bytes());
                tagged.extend_from_slice(bytes);
                let id = store.insert(&tagged)?;
                Ok((id.0, bytes.len() as u32))
            }).collect(),
            PayloadInner::Memory { data } => {
                Ok(items.iter().map(|bytes| {
                    let offset = data.len() as u64;
                    data.extend_from_slice(bytes);
                    (offset, bytes.len() as u32)
                }).collect())
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
                    file.write_all_at(&buf, base)?;
                }
                #[cfg(not(unix))]
                {
                    use std::io::{Seek, SeekFrom, Write};
                    file.seek(SeekFrom::Start(base))?;
                    file.write_all(&buf)?;
                }
                // Only after the bytes are down. Advancing the cursor first would
                // leave the store believing it holds a record that was never
                // written, and the next append would place its neighbour past a
                // gap of nothing.
                *total_len = base + buf.len() as u64;
                Ok(results)
            }
        }
    }

    /// Decode the payload at the given position to a `Value` — the hot path for
    /// the query engine. SKBIN records decode DIRECTLY to `Value` (binary decode,
    /// no JSON round-trip — faster than parsing text); raw JSON records parse; a
    /// retired `0x01` zstd record yields `None`.
    /// The stored bytes of one record, however this store addresses them.
    ///
    /// Paged stores treat `offset` as a record id; the others as a byte position.
    /// Every read of a whole record goes through here so the two never diverge.
    fn stored_record(&self, offset: u64, len: u32) -> Option<Vec<u8>> {
        self.stored_record_of(OWNER_UNKNOWN, offset, len)
    }

    /// The stored record, refusing one that does not belong to `owner`.
    ///
    /// `OWNER_UNKNOWN` skips the check, for the paths that read a payload without a
    /// node hash in hand. Those are the minority and are no worse than before; the
    /// ones that do know are the whole-record reads, which is where a substitution
    /// would actually be served to a caller.
    fn stored_record_of(&self, owner: u64, offset: u64, len: u32) -> Option<Vec<u8>> {
        match &self.inner {
            PayloadInner::Paged { store } => {
                let raw = store.read(storage::recordstore::RecordId(offset)).ok().flatten()?;
                if raw.len() < OWNER_TAG { return None }
                let tag = u32::from_le_bytes(raw[..OWNER_TAG].try_into().ok()?);
                if owner != OWNER_UNKNOWN && tag != owner_tag(owner) { return None }
                Some(raw[OWNER_TAG..].to_vec())
            }
            _ => self.get_raw_at(offset, len as usize),
        }
    }

    /// Read a whole record known to belong to `owner`.
    fn get_of(&self, owner: u64, offset: u64, len: u32) -> Option<Value> {
        // The paged case decodes against the mapped page rather than taking a
        // copy of it. `stored_record_of` allocates twice — a page, then the
        // record cut out of it — and both copies exist only to be parsed and
        // dropped. This is the hottest read in the database; it should not
        // allocate to answer.
        if let PayloadInner::Paged { store } = &self.inner {
            return store.with_record(storage::recordstore::RecordId(offset), |raw| {
                if raw.len() < OWNER_TAG { return None }
                let tag = u32::from_le_bytes(raw[..OWNER_TAG].try_into().ok()?);
                if owner != OWNER_UNKNOWN && tag != owner_tag(owner) { return None }
                let stored = &raw[OWNER_TAG..];
                if storage::skbin::is_skbin(stored) {
                    return storage::skbin::decode(stored, &self.field_table);
                }
                decode_payload_record(stored.to_vec())
                    .and_then(|b| serde_json::from_slice(&b).ok())
            }).ok().flatten().flatten();
        }
        let stored = self.stored_record_of(owner, offset, len)?;
        if storage::skbin::is_skbin(&stored) {
            return storage::skbin::decode(&stored, &self.field_table);
        }
        decode_payload_record(stored).and_then(|b| serde_json::from_slice(&b).ok())
    }

    fn get(&self, offset: u64, len: u32) -> Option<Value> {
        let stored = self.stored_record(offset, len)?;
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
        self.get_raw_of(OWNER_UNKNOWN, offset, len)
    }

    /// The same, checked against the node the bytes are supposed to belong to.
    ///
    /// A paged store addresses records by `(page, slot)`. Damage the slot
    /// directory and the read lands on a different record — a real payload, well
    /// formed, belonging to another row. Every record carries four bytes of its
    /// owner's hash so the read can tell, and [`get_of`](Self::get_of) checks
    /// them.
    ///
    /// This did not, and neither did the batch read built on it. So the checked
    /// single-record path answered correctly while the batched path — taken as
    /// soon as a filter has enough candidates to be worth batching — handed back
    /// another row's payload under this row's hash. A query for `p/n51` returned
    /// `n58`. Found by the fuzzer at round 1687 of seed 20260818, which is the
    /// argument for running a campaign on seeds nothing has run before.
    pub(crate) fn get_raw_of(&self, owner: u64, offset: u64, len: u32) -> Option<Vec<u8>> {
        let stored = self.stored_record_of(owner, offset, len)?;
        if storage::skbin::is_skbin(&stored) {
            // SKBIN → reconstruct JSON bytes using the shared field-name table.
            let v = storage::skbin::decode(&stored, &self.field_table)?;
            return serde_json::to_vec(&v).ok();
        }
        decode_payload_record(stored)
    }

    /// Read `read_len` bytes starting at an arbitrary absolute byte offset.
    /// Uses mmap when available (zero syscalls), falls back to pread.
    pub(crate) fn get_raw_at(&self, abs_offset: u64, read_len: usize) -> Option<Vec<u8>> {
        if read_len == 0 {
            return Some(vec![]);
        }
        match &self.inner {
            // Deliberately unsupported: an offset is an opaque record id here, so
            // slicing at a byte position, or reading across neighbouring records,
            // has no meaning. Callers check `absolute_offsets()` and take the
            // per-record route instead — returning None rather than guessing keeps
            // a mistake visible instead of quietly returning the wrong bytes.
            PayloadInner::Paged { .. } => None,
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
        }
    }

    /// Borrow a slice of the payload store without copying (zero-alloc).
    /// Returns `None` if offset/len is out of range or no mmap is available.
    #[cfg(unix)]
    fn get_slice(&self, abs_offset: u64, read_len: usize) -> Option<&[u8]> {
        if read_len == 0 { return Some(&[]); }
        match &self.inner {
            // No borrowed view: a paged record may span pages and is assembled on
            // read, so there is nothing contiguous to lend out.
            PayloadInner::Paged { .. } => None,
            PayloadInner::Memory { data } => {
                let start = abs_offset as usize;
                let end = start.checked_add(read_len)?;
                data.get(start..end)
            }
            PayloadInner::Disk { mmap, .. } => {
                mmap.as_ref()?.slice(abs_offset as usize, read_len)
            }
        }
    }

    /// Reset the slab (in-memory only — used after in-memory compaction).
    fn reset(&mut self, new_data: Vec<u8>) {
        if let PayloadInner::Memory { data } = &mut self.inner {
            *data = new_data;
        }
    }

    /// A **read-only** copy of this store for a snapshot `CoreDB`.
    ///
    /// Nothing is duplicated that matters: the disk arm takes its own file
    /// descriptor (`try_clone` — a second fd onto the same `payloads.bin` inode, so
    /// the bytes stay readable even after the live store compacts and renames over
    /// it) and shares the same `Arc<MmapView>`. Only the tiny SKBIN field table is
    /// copied.
    ///
    /// The result must never be appended to — it points at the live file. Safety
    /// comes from the caller: [`CoreDB::snapshot_db`] hands out `Arc<CoreDB>`, and
    /// every mutating method takes `&mut self`, so a shared `Arc` cannot reach one.
    #[cfg(unix)]
    fn read_only_clone(&self) -> Option<PayloadStore> {
        let inner = match &self.inner {
            // A snapshot shares the base store's descriptor; the paged store owns a
            // writable handle that cannot be shared, so paged databases fall back
            // to the locked read path rather than snapshotting.
            PayloadInner::Paged { .. } => return None,
            PayloadInner::Memory { data } => PayloadInner::Memory { data: data.clone() },
            PayloadInner::Disk { file, total_len, mmap } => PayloadInner::Disk {
                file: file.try_clone().ok()?,
                total_len: *total_len,
                mmap: mmap.clone(),
            },
        };
        Some(PayloadStore { binary: self.binary, field_table: self.field_table.clone(), inner })
    }
}

/// The small in-RAM record for one node — everything the engine keeps resident
/// per node, which is deliberately **not** the payload.
///
/// This is the crux of disk-first: for every node we hold its identity and a
/// pointer to where its bytes live on disk (`payload_offset` + `payload_len`),
/// plus two cached values (`collection`, `spatial_meta`) that would otherwise
/// force a disk read + JSON parse for common lookups. The payload bytes stay on
/// disk and are fetched only when a query actually needs them. So per-node RAM is
/// a few dozen bytes regardless of how big the record is.
#[derive(Clone)]
pub struct NodeData {
    /// The node's human-readable slug, `"collection/key"` (e.g. `"users/alice"`).
    pub slug: String,
    /// Cached `_collection` field value (empty string if no collection).
    /// Avoids parsing JSON for collection-only lookups.
    pub collection: String,
    /// Cached spatial bounding-box, computed once in `put_raw()`.
    /// `rebuild_spatial_grid()` reads from here to avoid disk reads.
    pub spatial_meta: Option<Box<geo::SpatialMeta>>,
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

/// The database — owns every node, edge, index, and the on-disk stores.
///
/// Not thread-safe by itself: writes take `&mut self`, reads and query starters
/// take `&self`. Wrap in `Mutex<CoreDB>` (or use the optional `engine` wrapper)
/// for concurrent access. Create one with [`CoreDB::new`] (in-memory, ephemeral)
/// or [`CoreDB::open`] (WAL-backed, persistent).
///
/// The many fields group into a few roles:
///
/// - **Identity & topology** — `nodes` (id → the small in-RAM [`NodeData`]),
///   `slug_map` (`"collection/key"` → id), `edges`, `collections`, and
///   `topo_base` (the mmap'd base in paged mode; the maps above become a write
///   *overlay* on top of it).
/// - **Record storage** — `payload_store` lives inside the nodes' offsets; the
///   actual bytes are on disk (or in RAM for an ephemeral DB).
/// - **Indexes** — one map per family: `spatial_grid`, `text_indexes` (GiST),
///   `gin_indexes`, `bm25_indexes`, `search_indexes`, `vectors` + HNSW. Each
///   turns a query into a set of node ids.
/// - **Schema** — `schemas` (from `CREATE TABLE`) and `materialized_views`.
/// - **Durability & config** — `wal` (+ `wal_format`, `wal_sync`), `data_dir`,
///   the auto-compaction thresholds, and the change-feed bookkeeping.
/// Cheap event counters kept by a live database. All relaxed atomics: a read path
/// only ever does one increment (~a nanosecond against a query measured in
/// microseconds), so an embedded user pays nothing meaningful for them.
#[derive(Default)]
pub(crate) struct Counters {
    queries: std::sync::atomic::AtomicU64,
    writes: std::sync::atomic::AtomicU64,
    compactions: std::sync::atomic::AtomicU64,
    snapshots: std::sync::atomic::AtomicU64,
    compact_us_last: std::sync::atomic::AtomicU64,
    compact_us_max: std::sync::atomic::AtomicU64,
    snapshot_us_last: std::sync::atomic::AtomicU64,
    snapshot_us_max: std::sync::atomic::AtomicU64,
}

/// A point-in-time picture of what the database is holding and what it has done —
/// the answer to "how big is this, and why was that slow".
///
/// Gathered by [`CoreDB::stats`]. Sizes are measured when you ask; counters and
/// timings accumulate from the moment the database was opened.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Stats {
    // ── how big is it ───────────────────────────────────────────────────────
    /// Nodes visible now (mmap'd base + in-RAM overlay).
    pub nodes: usize,
    /// Edges visible now.
    pub edges: usize,
    /// Distinct collections.
    pub collections: usize,
    /// Nodes sitting in the RAM write-overlay — i.e. written since the last
    /// `compact()`. This is what a snapshot has to copy, so it is the number to
    /// watch if snapshot minting feels expensive.
    pub overlay_nodes: usize,
    /// Size of `payloads.bin` on disk, bytes.
    pub payload_bytes: u64,
    /// Size of `wal.log` on disk, bytes. Auto-compaction triggers off this.
    pub wal_bytes: u64,
    /// Whether this store is paged (mmap'd base) rather than fully resident.
    pub paged: bool,

    // ── indexes ─────────────────────────────────────────────────────────────
    /// Scalar btree/hash field indexes (heap overlay + mmap'd base).
    pub field_indexes: usize,
    /// Vector (HNSW) indexes.
    pub hnsw_indexes: usize,
    /// BM25 relevance indexes.
    pub bm25_indexes: usize,
    /// Positional search indexes.
    pub search_indexes: usize,
    /// Trigram (GIN + GiST) indexes for `ILIKE`.
    pub trigram_indexes: usize,
    /// Whether a spatial index is built.
    pub spatial_index: bool,

    // ── what has happened since open ────────────────────────────────────────
    /// Queries executed.
    pub queries: u64,
    /// Durable mutations written to the WAL.
    pub writes: u64,
    /// Compactions run.
    pub compactions: u64,
    /// Snapshots minted (`snapshot` + `snapshot_db`).
    pub snapshots: u64,

    // ── how long the slow things took ───────────────────────────────────────
    /// Most recent compaction, microseconds. Compaction holds the write lock, so
    /// this is how long writers were stalled.
    pub last_compact_us: u64,
    /// Slowest compaction seen, microseconds.
    pub max_compact_us: u64,
    /// Most recent snapshot mint, microseconds.
    pub last_snapshot_us: u64,
    /// Slowest snapshot mint seen, microseconds.
    pub max_snapshot_us: u64,
}

pub struct CoreDB {
    /// Event counters + timings for [`CoreDB::stats`].
    pub(crate) counters: Counters,
    /// Nodes deleted from the **mmap'd base** since the last `compact()`.
    ///
    /// The base is immutable, so a delete cannot be applied there. Instead the
    /// hash is recorded here and every base-aware lookup treats it as absent;
    /// `compact()` then drops those nodes for real and clears the set. Empty in
    /// resident mode, where deletes just remove from the maps.
    tombstones: HashSet<u64>,
    /// The primary map: node id (a `u64` hash of the slug) → its in-RAM record.
    nodes: HashMap<u64, NodeData>,
    /// Reverse lookup: human slug `"collection/key"` → node id.
    slug_map: HashMap<String, u64>,
    /// Graph edges (forward + reverse adjacency, edge type names, metadata).
    edges: storage::edgestore::EdgeStore,
    /// Auto-compaction mode + thresholds (copied from `Config` at open).
    auto_compact: AutoCompact,
    /// When auto-compaction fires — the write-count / WAL-size limits it checks.
    compact_thresholds: CompactThresholds,
    /// How hard each WAL write is flushed to disk (durability vs. speed trade-off).
    wal_sync: SyncMode,
    /// Changes accumulated since the last emission; flushed as one [`ChangeEvent`]
    /// at each committed-mutation boundary (see [`CoreDB::subscribe_changes`]).
    pending_change: ChangeEvent,
    /// Registered change listeners: `(id, callback)`.
    change_listeners: Vec<(u64, Box<dyn FnMut(&ChangeEvent) + Send + Sync>)>,
    /// Next subscription id.
    next_change_id: u64,
    /// >0 while replaying a COMMIT: batch arms accumulate but do not emit, so
    /// the whole transaction publishes exactly one change event at COMMIT.
    commit_depth: u32,
    /// If set, run a final `compact()` when the database is dropped, so the next
    /// open starts from a clean snapshot instead of replaying a long WAL.
    compact_on_close: bool,
    /// Amortises the WAL-size stat: thresholds are checked every N writes.
    writes_since_compact_check: u32,
    /// Reentrancy guard for the on-write hook.
    autocompacting: bool,
    /// Paged-topology base (mmap'd files written at compact). `None` = resident
    /// mode (default). When `Some`, the resident maps above act as the **write
    /// overlay** since open, and the topology accessors merge overlay-over-base.
    ///
    /// Wrapped in `Arc` so a read-only snapshot can share
    /// this immutable base for free (a refcount bump, no bytes copied) — the base
    /// never changes, only the overlay does. See
    /// `docs/developer/notes/snapshot-reads-design.md`.
    /// The compacted store, oldest first.
    ///
    /// One entry today. The list exists because compaction is meant to *append* a
    /// segment holding only what changed, rather than rewrite everything — the
    /// latter is O(store) for O(change) input and is what Laws 1 and 2 forbid.
    /// Reads consult the overlay, then segments newest-first.
    segments: Segments,
    /// Durable adjacency in slotted pages, when `Config::paged_adjacency` is on.
    ///
    /// Takes the place of a segment's `adj_fwd.bin` / `adj_rev.bin` as the base
    /// that reads fall through to. The difference is that this one can be written:
    /// `compact()` folds the overlay into it owner by owner instead of rebuilding
    /// the graph, so the cost follows the change rather than the store.
    paged_adj: Option<PagedAdjacency>,
    /// Topology is served from the mapped files rather than loaded into RAM.
    ///
    /// Remembered because compaction needs it. A compaction writes the topology
    /// files and then adopts them — maps them as the new base and empties the
    /// overlay they came from, which is what returns the RAM. It only did that
    /// when a base already existed, so the *first* compaction in a fresh process
    /// wrote the files and walked past them: the overlay stayed resident, the
    /// segments stayed empty, and neither changed again until the database was
    /// reopened.
    ///
    /// That is the service case exactly — `open_as_service` uses this layout, and
    /// a service that creates its database and never restarts got no overlay
    /// compaction at all.
    paged_topology: bool,
    /// Opened read-only: every mutation is refused rather than accepted and
    /// dropped.
    ///
    /// The flag lived on `Config` and stopped there. It decided whether to take
    /// the directory lock and whether to open a log writer, and nothing
    /// downstream ever learned about it — so a write to a read-only handle went
    /// through the ordinary path, changed the in-memory maps, found no log to
    /// append to, and returned `Ok`.
    ///
    /// The documented behaviour was "writes silently skip WAL persistence", which
    /// undersells it. `put` reported success. `DELETE` reported the number of rows
    /// it had removed. Reads in that session then answered from the mutated
    /// overlay, so the handle disagreed with the file it was supposedly reading,
    /// and everything vanished at close with nothing raised at any point.
    ///
    /// `EngineBuilder::read_only` documented the opposite — that writes return an
    /// error — for the same idea. That is now the behaviour of both.
    read_only: bool,
    /// The first disk failure that could not be reported to whoever caused it.
    ///
    /// [`wal_write`](CoreDB::wal_write) returns nothing — it is called from the
    /// middle of a mutation whose in-memory half has already happened — so a
    /// failed WAL append had no way out and used to abort the process. That is
    /// the wrong end of the trade: a full disk is an ordinary condition, and the
    /// abort lands mid-write, which is exactly the crash the log exists to
    /// survive.
    ///
    /// So the failure is remembered instead. Reads keep working — what is in
    /// memory is still true — and anything that would make the loss permanent
    /// refuses: `compact()` above all, because a compaction folds the overlay
    /// into the base and drops the log, and a log that is missing entries must
    /// not be dropped. Nothing that can be wrong about what exists may delete it.
    ///
    /// Cleared by nothing. A database that has failed to write stays failed until
    /// it is reopened, at which point WAL replay decides what is actually there.
    write_error: Option<String>,
    /// Durable nodes in slotted pages, when `Config::paged_nodes` is on.
    ///
    /// Takes the place of a segment's `nodes.bin` / `idx.bin` / `slugs.bin` /
    /// `collections.bin` as the store that reads fall through to. Every question
    /// the engine asks of it goes through the `base_*` accessors, which is the
    /// only place either backend is chosen.
    paged_nodes: Option<storage::nodestore::NodeStore>,
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
    /// Materialized views: view name → (body SQL, auto_index flag, root collection).
    /// The materialized docs are ordinary nodes in a collection named after the view.
    /// `root_collection` is the projection's root (for mirroring its source vectors).
    /// Refreshed explicitly via `REFRESH MATERIALIZED VIEW` (Postgres-faithful; embedded
    /// -native — no background auto-refresh, which wouldn't fit the single-threaded core).
    materialized_views: HashMap<String, (String, bool, String)>,
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
    /// Disk-first int8 stores: field -> quantized codes resident in RAM.
    quant_fields: HashMap<String, vector::QuantizedField>,
    compact_indexes: HashMap<String, vector::CompactDiskIndex>,
    /// Btree field indexes: (collection_hash, field_name) → ordered value → [node hashes].
    /// Built via `CREATE INDEX ON collection(field) USING btree`.
    /// Maintained incrementally on every put()/remove().
    field_indexes: HashMap<(u64, String), BTreeMap<FieldKey, Vec<u64>>>,
    /// Paged mode: mmap'd on-disk btree indexes (posting lists live in reclaimable
    /// page cache, not the heap). Consulted by `field_index_ref` when a
    /// `(collection, field)` is absent from the heap `field_indexes` overlay.
    field_base: HashMap<(u64, String), storage::fieldstore::MappedFieldStore>,
    /// Build params for each HNSW index: field → (m, ef_construction).
    /// Populated by build_hnsw_index(); used to auto-rebuild on version mismatch.
    hnsw_params: HashMap<String, (usize, usize)>,
    /// Distance metric each HNSW index was built with (Cosine if unset).
    hnsw_metric: HashMap<String, crate::query::VecMetric>,
    hnsw_ef_search: Option<usize>,
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
    /// Where each live record's payload moved to during the compaction currently
    /// in progress. Empty at all other times.
    ///
    /// The payload rewrite happens before the topology is written, and it used to
    /// record the new locations by mutating `self.nodes` — which only works if
    /// every node is in `self.nodes`, which is why the base was copied into RAM
    /// first. Publishing them here instead lets a base-resident record learn its
    /// new payload location without being hydrated.
    compact_payload_moves: HashMap<u64, (u64, u32)>,
    /// Nodes whose edges in the immutable base have been cascade-deleted.
    ///
    /// Separate from `tombstones` because it must outlive them: writing the key
    /// again retires the node tombstone, but the edges that were deleted along with
    /// the node must stay deleted. Sharing one set resurrected them.
    ///
    /// Cleared by compaction, which folds the base away entirely.
    edge_tombstones: HashSet<u64>,
    /// Collections whose membership in the durable store must be ignored.
    ///
    /// Renaming a table moves its rows to a new collection hash in the overlay, but
    /// the durable membership index still lists them under the old one — so both
    /// names answered, and `SELECT FROM the_old_name` kept returning every row. The
    /// base cannot be edited in place, so the old name is recorded here and
    /// subtracted where base membership is read, the same way a withdrawn edge is.
    ///
    /// Cleared by compaction, which writes membership that no longer has them.
    renamed_collections: HashSet<u64>,
    /// `(from, to, edge_type)` for single edges withdrawn by `unlink`.
    ///
    /// `EdgeStore::unlink` can only retain-out of the RAM adjacency maps, and in
    /// paged mode most edges are not there — they are in the immutable base, and a
    /// read merges the two. Without this set, unlinking a base edge did nothing at
    /// all: the call returned, the overlay had never held the edge, and the next
    /// read produced it again from the base. A delete that does not delete.
    ///
    /// So the withdrawal is recorded here and applied where the base is read.
    /// Cleared by compaction, which writes a base that no longer contains them.
    unlinked_edges: HashSet<(u64, u64, u64)>,
    /// The caller fsyncs the log itself, so this database appends without one.
    ///
    /// Set by `Engine` in service mode: several writers that commit at the same
    /// time then share a single fsync instead of each paying for their own. The
    /// record is still appended and flushed to the OS here — only the fsync moves
    /// out, and the engine does not report a write committed until it has run.
    /// Never set for an embedded `CoreDB`, which fsyncs inline as before.
    group_commit: bool,
    /// Bumped whenever the log file is replaced — which compaction does, renaming
    /// the old one away and creating a new one.
    ///
    /// A caller coordinating fsyncs must notice this: the descriptor it holds now
    /// points at a removed inode, and the record count has restarted from zero.
    /// Without it, syncs would silently stop covering anything.
    wal_generation: u64,
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

/// What changed in one committed mutation (or transaction). Delivered to
/// listeners registered with [`CoreDB::subscribe_changes`] — the foundation for
/// reactive/`watch`-style queries in the language wrappers. Granularity is
/// chosen so a watcher can decide precisely whether to refresh: a collection
/// query cares about `collections`, a single-record watcher about `keys`, a
/// graph traversal about `edge_types`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeEvent {
    /// Collections whose members were inserted, updated, or removed.
    pub collections: Vec<String>,
    /// Node slugs (`collection/key`) that were put, updated, or removed.
    pub keys: Vec<String>,
    /// Edge type labels that were linked or unlinked.
    pub edge_types: Vec<String>,
}

impl ChangeEvent {
    fn is_empty(&self) -> bool {
        self.collections.is_empty() && self.keys.is_empty() && self.edge_types.is_empty()
    }
    fn clear(&mut self) {
        self.collections.clear();
        self.keys.clear();
        self.edge_types.clear();
    }
    fn push_unique(v: &mut Vec<String>, s: &str) {
        if !v.iter().any(|x| x == s) {
            v.push(s.to_string());
        }
    }
}

/// WAL durability level. See [`Config::wal_sync`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMode {
    /// fsync every write. Safe against power loss; slow on mobile flash.
    Full,
    /// Append without per-write fsync; sync at checkpoint/compact. The standard
    /// mobile trade-off (SQLite `synchronous=NORMAL`): durable to the last
    /// checkpoint, never corrupting.
    Normal,
    /// Never fsync. Fastest, least durable.
    Off,
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
    /// Durability of individual writes. [`SyncMode::Full`] (default) fsyncs the
    /// WAL on every write — safe against power loss, but each fsync costs tens
    /// of milliseconds on mobile flash (Android FUSE/eMMC). [`SyncMode::Normal`]
    /// appends to the WAL without a per-write fsync and syncs only at
    /// checkpoint/compact — the standard mobile trade-off (SQLite
    /// `synchronous=NORMAL`): a crash can lose writes since the last checkpoint
    /// but never corrupts the database (WAL replay resumes from the last synced
    /// point). [`SyncMode::Off`] never syncs.
    pub wal_sync: SyncMode,
    /// Re-encode payloads as SKBIN (schema-aware binary) at compaction: field
    /// names → IDs, typed values, strings literal. ~1.6× smaller, faster field
    /// reads, 1-record corruption isolation (values never leave their record).
    /// Incremental writes stay raw JSON until the next `compact()`. Default on.
    pub payload_binary: bool,
    /// Keep payloads in slotted pages with a free list, so an updated or deleted
    /// row returns its bytes immediately rather than leaving a hole for
    /// compaction to squeeze out later.
    ///
    /// This is the write path SQLite, LMDB and DuckDB use, and the reason they have
    /// no compaction step: space comes back continuously as it is freed.
    ///
    /// **On by default.** What it gives up: two read optimisations that assume
    /// byte offsets — the batch read that coalesces neighbouring records, and
    /// head-and-tail extraction of large records — fall back to per-record reads.
    pub paged_payloads: bool,
    /// Keep adjacency in slotted pages keyed by slug hash, so a new edge is
    /// written where it belongs instead of waiting for a rebuild.
    ///
    /// The CSR layout in `adj_fwd.bin` / `adj_rev.bin` cannot absorb one edge: a
    /// node's block grows, every block after it shifts, every offset after it
    /// changes. So edges accumulate in RAM until `compact()` folds the whole graph
    /// back, at a cost set by the size of the graph rather than the size of the
    /// change — and adjacency is about two thirds of what a compaction rewrites.
    ///
    /// **On by default.** The sacrifice is disk — 2.3x CSR, because a B+tree entry
    /// per owner is 42 bytes and CSR's offset array is 8 — and traversal speed: a
    /// one-hop read is about 0.65x the mmap'd CSR it replaces, a tree descent and
    /// a record read where CSR had two array reads.
    pub paged_adjacency: bool,
    /// Keep nodes in slotted pages keyed by slug hash, so a new or changed node is
    /// written where it belongs instead of waiting for a rebuild.
    ///
    /// `nodes.bin`, `idx.bin`, `slugs.bin` and `collections.bin` are the whole of
    /// what a compaction still rewrites once adjacency and payloads are paged.
    /// None of them can absorb a write: a dense id is a *position*, so inserting
    /// one node renumbers the rest; a sorted array has nowhere to put an entry; an
    /// offsets array shifts when anything before it grows.
    ///
    /// **On by default.** The sacrifice is disk: about 1.8x what the four files
    /// spend, almost all of it the two B+trees, where the files are packed arrays.
    /// Point reads are faster than the layout it replaces.
    pub paged_nodes: bool,
    /// Serve topology (nodes + edges) from the mmap'd files written at
    /// `compact()` instead of loading it into RAM. The OS page cache keeps the
    /// hot working set resident and pages the rest — topology size is no longer
    /// bounded by RAM. Writes since open live in a RAM overlay merged with the
    /// mapped base on every read; `compact()` folds them together.
    ///
    /// **On by default.** This is Law 1 — disk-first — and the reason a store
    /// larger than memory opens at all.
    ///
    /// Unlike the other three flags this one is not a file format. It says where
    /// topology is *served from* in this process, so it can be turned on or off
    /// for any store, and `adopt_layout_from_disk` leaves it alone.
    ///
    /// The base and the overlay disagreeing was the source of most of the bugs
    /// this mode has had — a read that consulted only the overlay saw a fraction
    /// of the database and reported it as the whole. `tests/differential_audit.rs`
    /// exists for that: it answers every query in every mode, before and after a
    /// restart, and fails on any disagreement.
    pub paged_topology: bool,
}

impl Config {
    /// The layout used before paged became the default: topology loaded into RAM,
    /// payloads appended to a flat file, nodes and adjacency rebuilt by
    /// compaction.
    ///
    /// Kept because it is the thing the paged layout has to agree with. Every
    /// answer either mode gives is compared against the other in
    /// `tests/differential_audit.rs`, and a mode that nothing compares against is
    /// a mode nothing checks.
    ///
    /// It is also the right choice for a database that is written once and read
    /// many times — a shipped dataset, a build artefact — where compaction never
    /// runs twice and the packed files are smaller and traverse faster. For
    /// anything that keeps being written, the default is what you want.
    ///
    /// This decides a **new** store. Pointing it at a directory holding a paged
    /// store does not reinterpret those files; the layout on disk wins.
    pub fn resident() -> Self {
        Self {
            paged_payloads: false,
            paged_adjacency: false,
            paged_nodes: false,
            paged_topology: false,
            ..Self::default()
        }
    }
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
            wal_sync: SyncMode::Full,
            // SKBIN Level-1 is the official default payload format: schema-aware
            // binary (~1.2–2x smaller on structured data, faster field reads,
            // 1-record corruption isolation, zero user data in shared state).
            // Fuzzed decoder, integrated across DML/DDL + resident/paged. Set
            // false for legacy raw-JSON payloads.
            payload_binary: true,
            // Paged is the default layout, and the four flags come as a set: a
            // compaction that no longer rewrites payloads still rewrites nodes,
            // and one that rewrites neither still rewrites adjacency. Turning on
            // three of four leaves the rebuild in place and pays the disk for
            // nothing.
            //
            // What this buys is Law 2 — cost proportional to the change rather
            // than to the store. Compaction at 500 000 rows went from 4 816 ms to
            // 132 ms and stopped growing: 108-130 ms all the way to two million.
            //
            // What it costs, stated where the choice is made:
            //
            //  * **Disk.** About 2.3x CSR for adjacency and 1.8x for the node
            //    files, almost all of it B+tree entries where the old files were
            //    packed arrays.
            //  * **Traversal.** A one-hop read is roughly 0.65x the speed of the
            //    mmap'd CSR it replaces: a tree descent and a record read where
            //    CSR had two array reads. Point reads are faster.
            //  * **Snapshots.** `snapshot_db` returns `None` over paged nodes or
            //    adjacency, because those files are written in place and a
            //    snapshot sharing them would see a writer's later edits appear
            //    underneath it. Reads fall back to taking the lock, which is
            //    correct and slower. See `.workbench/snapshot-over-paged-design.md`.
            //
            // An existing database is unaffected: `adopt_layout_from_disk` reads
            // the files and opens it in the layout it was written in. This
            // default decides new stores only.
            paged_payloads: true,
            paged_adjacency: true,
            paged_nodes: true,
            paged_topology: true,
        }
    }
}

/// Make `config` agree with the store already on disk.
///
/// The paged flags read like options, and for a directory that holds nothing yet
/// they are: the first open decides where the data will live, and that is what
/// [`Config::default`] is for. For a directory that already holds a store they
/// are not options at all. They name the format of files that exist, and files
/// do not change format because the next caller preferred a different one.
///
/// So for an existing store the **files decide, in both directions**:
///
/// * `payloads.bin` starts with the page-store magic → `paged_payloads` on,
///   otherwise off
/// * `nodesp.rec` / `adjp_fwd.rec` hold bytes → `paged_nodes` /
///   `paged_adjacency` on, otherwise off
///
/// Turning a flag *off* matters as much as turning it on, and it started
/// mattering the day paged became the default. A database written by an older
/// version has flat files; opening it with the new default would have found
/// empty paged stores, migrated the graph into them, and left a store half in
/// each format. Nothing would have been lost — the migration is real — but the
/// number of shapes a store can be in would have doubled, silently, for every
/// existing database. Converting a store is a thing to ask for, not a thing that
/// happens because you opened it.
///
/// Turning a flag on is the older half and the one that prevents deletion: a
/// paged store opened without its flags finds its data in files nothing reads,
/// reports an empty database, truncates `payloads.bin`, and loses the lot at the
/// next compaction. The refusal that follows this call stays as a backstop.
///
/// `paged_topology` is not decided here. It says whether topology is *served*
/// from the mapping or loaded into RAM, which is a choice about this process, not
/// a fact about the files.
fn adopt_layout_from_disk(dir: &Path, config: &mut Config) {
    let nonempty = |file: &str| -> bool {
        std::fs::metadata(dir.join(file)).is_ok_and(|m| m.len() > 0)
    };
    // Whether there is a store here at all. Any one of these means a previous
    // open got far enough to write something, so the layout is already settled.
    // A directory that holds none of them is new, and the caller's config stands.
    let existing = ["snapshot.json", "wal.log", "nodes.bin", "payloads.bin"]
        .iter()
        .any(|f| nonempty(f));
    if !existing {
        return;
    }
    if nonempty("payloads.bin") {
        let paged = std::fs::File::open(dir.join("payloads.bin"))
            .and_then(|mut f| {
                use std::io::Read;
                let mut magic = [0u8; 8];
                f.read_exact(&mut magic).map(|_| magic)
            })
            .is_ok_and(|magic| &magic == b"SKPAGE\0\0");
        config.paged_payloads = paged;
    }
    config.paged_nodes     = nonempty("nodesp.rec");
    config.paged_adjacency = nonempty("adjp_fwd.rec");
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
            counters: Counters::default(),
            tombstones: HashSet::new(),
            nodes: HashMap::new(),
            slug_map: HashMap::new(),
            // An in-memory database has no directory to put pages in.
            paged_adj: None,
            paged_topology: false,
            read_only: false,
            write_error: None,
            paged_nodes: None,
            auto_compact: AutoCompact::Off, // memory DBs have nothing to compact
            compact_thresholds: CompactThresholds::default(),
            wal_sync: SyncMode::Full,
            pending_change: ChangeEvent::default(),
            change_listeners: Vec::new(),
            next_change_id: 0,
            commit_depth: 0,
            compact_on_close: false,
            writes_since_compact_check: 0,
            autocompacting: false,
            segments: Segments::default(),
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
            materialized_views: HashMap::new(),
            vectors: HashMap::new(),
            hnsw_indexes: HashMap::new(),
            quant_fields: HashMap::new(),
            compact_indexes: HashMap::new(),
            field_indexes: HashMap::new(),
            field_base: HashMap::new(),
            hnsw_params: HashMap::new(),
            hnsw_metric: HashMap::new(),
            hnsw_ef_search: None,
            payload_store: PayloadStore::new(),
            replaying: false,
            pending_txn: None,
            defer_wal_sync: false,
            edge_tombstones: HashSet::new(),
            unlinked_edges: HashSet::new(),
            renamed_collections: HashSet::new(),
            compact_payload_moves: HashMap::new(),
            group_commit: false,
            wal_generation: 0,
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
    /// Open (or create) a persistent, WAL-backed database in directory `dir`.
    ///
    /// This is the normal way to get a durable `CoreDB`. If the directory is
    /// empty it starts a fresh database there; otherwise it recovers the previous
    /// state by loading the snapshot and replaying the WAL (see
    /// `open_with_config`, which does the real work). `impl AsRef<Path>` just
    /// means "anything that can be viewed as a filesystem path" — a `&str`, a
    /// `String`, a `PathBuf`, etc. — so callers can pass whichever they have.
    pub fn open(dir: impl AsRef<Path>) -> io::Result<Self> {
        Self::open_with_config(dir, Config::default())
    }

    /// Open with paged topology and nothing else paged: nodes + edges are served
    /// from the mmap'd files written at the last `compact()` (the OS page cache
    /// holds the hot working set), while writes since open live in a RAM overlay.
    /// Falls back to a normal resident open when the topology files are absent.
    ///
    /// Narrower than [`CoreDB::open`], which now gives the full paged layout.
    /// This exists for the one shape it names, and for the tests that compare a
    /// single change against the resident layout rather than four at once.
    pub fn open_paged(dir: impl AsRef<Path>) -> io::Result<Self> {
        Self::open_with_config(dir, Config { paged_topology: true, ..Config::resident() })
    }

    /// Open a database in read-only mode (no lock, no log writer).
    ///
    /// Suitable for read replicas alongside a writer process.
    ///
    /// **Writes are refused**, not ignored. Anything that would change the store
    /// returns an error — `PermissionDenied` for the `io::Result` methods, and an
    /// `SqlError` for statements. The three that return nothing (`remove`,
    /// `link`, `unlink`) do nothing and record the refusal, readable through
    /// [`write_error`](Self::write_error).
    ///
    /// It used to say "write operations will silently skip WAL persistence",
    /// which undersold what happened: `put` returned `Ok`, `DELETE` returned the
    /// number of rows it claimed to have removed, reads in that session answered
    /// from the changed overlay, and the whole lot disappeared at close without
    /// anything being raised.
    pub fn open_read_only(dir: impl AsRef<Path>) -> io::Result<Self> {
        Self::open_with_config(
            dir,
            Config { read_only: true, ..Config::default() },
        )
    }


    /// Open (or create) a persistent database with explicit configuration — the
    /// full startup / crash-recovery routine.
    ///
    /// The startup story, in order:
    /// 1. **Lock** the directory (`db.lock`) so two processes can't corrupt it
    ///    by writing at once.
    /// 2. **Load the snapshot** — the manifest from the last `compact()`, which is
    ///    the bulk of the state. Its version is checked first: a store newer than
    ///    this build is refused, an older one is migrated (see the Ring-2
    ///    migration framework near the version constants).
    /// 3. **Replay the WAL** — re-apply every change logged *after* that snapshot,
    ///    catching up on writes that hadn't been folded in yet. This is what makes
    ///    a crash recoverable: whatever was logged is re-applied.
    /// 4. **Restore the indexes** — for each family, either load its on-disk
    ///    sidecar (fast) or rebuild it from the data (correct), depending on
    ///    whether the WAL carried new data and whether the sidecar's version
    ///    matches. This is the "never rebuild what you can map" rule in action.
    ///
    /// If the WAL contains a corrupted frame, recovery stops at that frame — all
    /// entries before it are intact — and a warning is printed to stderr, so a
    /// partially-written last record can never make the whole database unopenable.
    ///
    /// # Errors
    /// Returns an error if the directory cannot be created, the snapshot cannot be
    /// parsed, or the WAL file cannot be opened.
    pub fn open_with_config(dir: impl AsRef<Path>, config: Config) -> io::Result<Self> {
        let dir = dir.as_ref();
        let mut config = config;
        std::fs::create_dir_all(dir)?;

        // Take an exclusive lock on a `db.lock` file so a second process can't
        // open the same database for writing and corrupt it. This is an *advisory*
        // OS lock (like SQLite's): it only blocks other processes that also try to
        // lock — it doesn't physically prevent reads. Skipped in read-only mode,
        // where many replicas may share the directory safely.
        let lock_file = if config.read_only {
            None
        } else {
            let f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .open(dir.join("db.lock"))?;
            // Only a genuine `WouldBlock` means another process holds the lock.
            // On some filesystems (Android FUSE/sdcardfs, some network mounts)
            // `flock` is unsupported and returns an error instead — there we
            // proceed without advisory locking, the way SQLite degrades.
            match f.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "database is locked by another process",
                    ));
                }
                Err(std::fs::TryLockError::Error(_)) => {
                    // Locking not supported on this filesystem — continue unlocked.
                }
            }
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
            // Ring 2: an older store is upgraded in place by registered migrations
            // before it is read. Dormant while STORE_MIGRATIONS is empty (older
            // snapshots then fall through to backward-compatible parsing below); a
            // future format change activates this path. A rewriting migration
            // reopens the header, so re-probe when the chain completed.
            if fmt_version < SNAPSHOT_FORMAT_VERSION && !STORE_MIGRATIONS.is_empty() {
                drop(file);
                apply_store_migrations(dir, fmt_version, SNAPSHOT_FORMAT_VERSION, STORE_MIGRATIONS)?;
                file = std::fs::File::open(&snap_path)?;
                let n2 = file.read(&mut head).unwrap_or(0);
                let (_v2, body2) = snapshot_probe(&head[..n2]);
                file.seek(SeekFrom::Start(body2 as u64))?;
            } else {
                file.seek(SeekFrom::Start(body_offset as u64))?;
            }
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
        // ── refuse a config that does not match how the store was written ────
        //
        // The reverse direction was already guarded: a flat `payloads.bin` opened
        // with `paged_payloads` is refused, because creating a page store over it
        // would truncate it. This direction was not, and it is worse.
        //
        // A store written with the paged flags, opened without them, reports itself
        // healthy and serves an **empty database** — the paged stores hold the data
        // and nothing looks at them. `payloads.bin` is then truncated to zero by the
        // flat payload path on open, before any write is issued, and the first
        // compaction makes the loss permanent. Reachable from `open_as_service()`,
        // because `EngineBuilder` sets `paged_topology` alone and has no way to ask
        // for the others.
        //
        // So the store's own files decide. Each check is "these bytes exist and this
        // config would ignore them".
        //
        // Refusing is the floor, not the answer. A paged flag is not a preference
        // about behaviour — it names a file format, and the format was settled when
        // the store was written. Asking for flat payloads over a paged
        // `payloads.bin` is not a different taste; it is a false statement about
        // bytes that already exist. So before refusing, the open adopts what the
        // files say: flags the store needs are turned on, and `paged_payloads` is
        // turned off when the file on disk is flat. The caller's config still
        // decides the layout of a store that does not exist yet.
        adopt_layout_from_disk(dir, &mut config);
        {
            let mismatched = |file: &str, flag: bool| -> bool {
                !flag && std::fs::metadata(dir.join(file)).is_ok_and(|m| m.len() > 0)
            };
            let flat_payloads_but_paged_file = !config.paged_payloads
                && std::fs::read(dir.join("payloads.bin")).is_ok_and(|b| {
                    b.len() >= 8 && &b[0..8] == b"SKPAGE\0\0"
                });
            let offender = if flat_payloads_but_paged_file {
                Some(("payloads.bin", "paged_payloads"))
            } else if mismatched("nodesp.rec", config.paged_nodes) {
                Some(("nodesp.rec", "paged_nodes"))
            } else if mismatched("adjp_fwd.rec", config.paged_adjacency) {
                Some(("adjp_fwd.rec", "paged_adjacency"))
            } else {
                None
            };
            if let Some((file, flag)) = offender {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "sekejap: {} holds data written with `{flag}`, but this open \
                         did not ask for it and the layout could not be adopted from \
                         the files. Continuing would serve an empty database and then \
                         overwrite what is there. Open it with \
                         Config {{ {flag}: true, .. }} — through \
                         `Engine::builder(..).config(..)` if this is a service.",
                        dir.join(file).display(),
                    ),
                ));
            }
        }

        if config.paged_adjacency {
            // The durable graph, written where it belongs instead of rebuilt.
            db.paged_adj = Some(PagedAdjacency::open(dir)?);
        }
        if config.paged_nodes {
            db.paged_nodes = Some(storage::nodestore::NodeStore::open(
                dir, storage::pagestore::DEFAULT_PAGE_SIZE)?);
        }
        // dict.bin carries these when a compaction writes the edge list; paged
        // adjacency writes no edge list, so they come from their own file.
        for (hash, name) in edge_type_names::read(&dir.join(edge_type_names::FILE)) {
            db.edges.register_type_name(hash, name);
        }
        for (hash, name) in edge_type_names::read(&dir.join(edge_type_names::COLL_FILE)) {
            db.collection_names_map.entry(hash).or_insert(name);
        }
        if config.paged_payloads {
            // Slotted pages with a free list: space returns as records die, so
            // there is nothing for a rewrite to reclaim later.
            db.payload_store = PayloadStore::open_paged(&pay_path)?;
        } else if preserve && pay_path.exists() {
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
        db.wal_sync = config.wal_sync;
        db.compact_thresholds = config.compact_thresholds;
        db.compact_on_close = config.compact_on_close;

        if let Some(snap) = snap {
            if paged {
                // Attach the mmap'd base; the snapshot supplies everything else
                // (schemas, vectors, HNSW, btree indexes). Nodes + edges are NOT
                // loaded into RAM — the resident maps stay empty and act as the
                // write overlay. WAL replay below adds post-compact writes to it.
                let base = std::sync::Arc::new(storage::topology::MappedTopology::open(dir)?);
                // The base carries its own edge type names; the live store only
                // learns them from link(), which never runs on a reopen.
                for (h, name) in base.edge_type_table() {
                    db.edges.register_type_name(h, name);
                }
                db.segments.replace_with(base);
                let slots = storage::slotmap::MappedSlots::open(&dir.join("slots.bin"))
                    .ok()
                    .flatten()
                    .map(std::sync::Arc::new);
                db.segments.set_slots(slots);
                db.load_snapshot_parts(snap, /*load_topology=*/ false);
                // Serve btree indexes from the mmap'd sidecars, not the heap: mmap
                // them into field_base and drop any heap copies the snapshot loaded
                // (posting lists become reclaimable page cache instead of RAM).
                db.load_field_base(dir)?;
                if !db.field_base.is_empty() {
                    db.field_indexes.clear();
                }
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

        // A store whose graph is still in CSR has to have it moved onto pages
        // before anything reads it. Paged adjacency *replaces* the segments as the
        // base rather than adding to them — consulting both would double every edge
        // once folding starts — so without this, turning the flag on would look
        // exactly like the graph having been deleted.
        //
        // Guarded on the paged store being empty, so it is a migration and not a
        // step in the open path: it runs once in a database's life.
        if db.paged_adj.as_ref().is_some_and(|pa| pa.is_empty()) {
            if let Some(base) = db.segments.newest_first().next().cloned() {
                if let Some(pa) = db.paged_adj.as_mut() {
                    pa.migrate_from(&base)?;
                }
            }
        }
        // The same for nodes, and for the same reason: paged nodes *replace* the
        // segments as the store reads fall through to, so a database whose nodes
        // are still in nodes.bin would look empty rather than paged.
        if db.paged_nodes.as_ref().is_some_and(|ns| ns.len() == 0) {
            if let Some(base) = db.segments.newest_first().next().cloned() {
                if let Some(ns) = db.paged_nodes.as_mut() {
                    for id in 0..base.node_count() as u64 {
                        let (Some(hash), Some(rec)) = (base.hash_of(id), base.node_record(id))
                        else { continue };
                        ns.put(hash, &storage::nodestore::StoredNode {
                            collection: base.collection_name(rec.collection_id)
                                .unwrap_or("").to_string(),
                            payload_offset: rec.payload_offset,
                            payload_len: rec.payload_len,
                            spatial: base.spatial(id),
                            slug: base.slug_of(id).unwrap_or("").to_string(),
                        })?;
                    }
                    ns.sync()?;
                }
            }
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
            db.merge_index_deltas();
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
        db.read_only = config.read_only;
        db.paged_topology = config.paged_topology;
        if !config.read_only {
            let wal = WalWriter::open_with_format(&wal_path, config.wal_format)?;
            db.wal_format = wal.format();
            db.wal = Some(wal);
        }

        // 4. Spatial index: in paged mode, serve the cell index + meta from the
        //    mmap'd spatialgrid.bin (disk-first) if present; otherwise rebuild it
        //    resident. WAL-added geometry (wal_had_payload) needs a fresh grid.
        if !(paged && !wal_had_payload && db.attach_spatial_base(dir)) {
            db.rebuild_spatial_grid();
        }

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
            // Paged mode: drop the just-built resident search blobs and re-serve
            // them from the mmap'd search.bin (disk-first) — same as the load path.
            if !db.segments.is_empty() {
                db.search_indexes.clear();
                let _ = db.load_search_binary(&search_bin_path);
            }
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
            // so here vectors are unchanged — no rebuild needed). In paged mode, mmap the
            // compact vector indexes from vecidx.bin (disk-first) so vector queries use the
            // int8+CSR fast path without a resident graph rebuild.
            if !db.segments.is_empty() {
                let _ = db.load_vector_base(&dir.join("vecidx.bin"));
                // BM25: mmap dict/doc-arrays from bm25.bin (disk-first) — doc arrays off
                // the map, dict resident, postings pread. Also covers the clean-reopen
                // case where BM25 was previously not restored at all.
                let _ = db.load_bm25_base(&dir.join("bm25.bin"), dir);
            } else {
                // Resident mode: BM25 has no resident sidecar loader (bm25.bin serves
                // the paged mmap path, and a mmap-backed index would silently drop live
                // inserts). Rebuild into owned/mutable form. Without this, a clean
                // reopen with no writes since compact serves EMPTY BM25.
                db.rebuild_declared_bm25_indexes();
            }
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
                        // Migrating an in-memory store to disk: a failure here means
                        // the vector is not on disk, so the migration is abandoned
                        // rather than half-done.
                        disk_store.put(id, data.to_vec())?;
                    }
                    disk_store.remap();
                    db.vectors.insert(field, disk_store);
                }
            }
        }

        Ok(db)
    }

    // ── Raw internals (no WAL write — used during replay and open) ────────────
    //
    // The public `put` writes the WAL and then calls into here. On `open()`, WAL
    // replay ALSO calls in here directly — the change is already in the log, so
    // re-logging it would be wrong. That's the whole reason the "apply" logic
    // lives in a separate no-WAL function.

    /// Parse `payload_json` and apply the write. The no-WAL entry point used by
    /// the public `put` (which already logged) and by WAL replay.
    fn put_raw(&mut self, slug: &str, payload_json: &str) -> Result<u64, serde_json::Error> {
        let payload: Value = serde_json::from_str(payload_json)?;
        self.put_raw_inner(slug, payload_json.as_bytes(), payload)
    }

    /// Apply one node insert/update to the in-memory maps, the payload store, and
    /// the indexes. Takes the raw bytes AND the parsed `Value` so callers that
    /// already have both don't pay to re-parse or re-serialize.
    ///
    /// The steps: stamp timestamps, guard against hash collisions, append the
    /// bytes to the payload store, update the node metadata + collection
    /// membership, and refresh any affected indexes. Returns the node's id hash.
    fn put_raw_inner(&mut self, slug: &str, raw: &[u8], payload: Value) -> Result<u64, serde_json::Error> {
        if self.read_only {
            use serde::ser::Error as _;
            return Err(serde_json::Error::custom(self.refuse_write("write").to_string()));
        }
        self.note_key_change(slug); // remember this key changed (for the change feed)
        let hash = sk_hash(slug);   // the node's u64 identity
        // In a bulk batch every row shares one timestamp, so we take the clock
        // once (`batch_now`) instead of a `now()` syscall per row.
        let now = self.batch_now.unwrap_or_else(|| chrono::Utc::now().timestamp_millis());

        // A node is always a JSON object (it has named fields); reject anything else.
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

        // If this slug already exists, remember its old collection + payload
        // location so we can retract stale index entries and reclaim its space.
        let old_info: Option<(String, u64, u32)> = self
            .node_data(hash)
            .map(|n| (n.collection.clone(), n.payload_offset, n.payload_len));

        // Add `_updated_unix` by editing the JSON *bytes* directly rather than
        // re-serializing the whole parsed `Value` — much cheaper for a large
        // record. `splice_json_field` returns the edited buffer, or (on failure)
        // we keep the original via `unwrap_or(buf)`.
        let now_str = now.to_string();
        let mut buf = raw.to_vec(); // owned copy we can edit
        buf = query::splice_json_field(&buf, "_updated_unix", now_str.as_bytes())
            .unwrap_or(buf);

        // `_created_unix` must be set once and then never change. If the caller
        // didn't supply it, try to preserve the ORIGINAL creation time by reading
        // it out of the previous version's bytes; only fall back to `now` for a
        // genuinely new node. The `.and_then(...)?...` chain is Option plumbing:
        // each step yields the next value or short-circuits to `None`, and
        // `unwrap_or_else(now)` supplies the default when there's no old value.
        if payload.get("_created_unix").is_none() {
            let created_str = old_info.as_ref()
                .and_then(|(_, off, len)| {
                    let old_raw = self.payload_store.get_raw_of(hash, *off, *len)?;
                    let map = query::extract_fields_by_search(
                        &old_raw, &["_created_unix".to_string()],
                    );
                    map.get("_created_unix").and_then(|v| v.as_i64())
                })
                .map(|v| v.to_string())
                .unwrap_or_else(|| now_str.clone()); // new node → created == now
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

        // The previous version's bytes are now garbage. A paged store takes them
        // back immediately; the append-only ones have nowhere to put them, which is
        // the debt compaction later pays off.
        if let Some((_, old_off, _)) = old_info {
            self.payload_store.free(old_off);
        }

        // Remove old collection + field-index entries for this hash (if updating)
        if let Some((ref old_coll, old_off, old_len)) = old_info {
            if !old_coll.is_empty() {
                let coll_hash = sk_hash(old_coll);
                if let Some(members) = self.collections.get_mut(&coll_hash) {
                    members.retain(|&h| h != hash);
                }
                let has_fi = self.field_indexes.keys().any(|(c, _)| *c == coll_hash);
                if has_fi {
                    let old_payload = self.payload_store.get_of(hash, old_off, old_len)
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
            // A fresh insert's hash is brand new, so it can't already be a member —
            // skip the O(n) `contains` scan that made bulk-load into one collection
            // O(n²). On update the old-collection cleanup above already removed this
            // hash, so the guard is only a safety net for that rarer path.
            if old_info.is_none() || !members.contains(&hash) {
                members.push(hash);
            }
            self.collection_names_map.entry(coll_hash).or_insert_with(|| coll.to_string());
            // A btree index that lives only in the mmap base has to be brought into
            // the heap before it can be updated, or this write is silently dropped.
            let mapped_fields: Vec<String> = self.field_base.keys()
                .filter(|(c, _)| *c == coll_hash)
                .map(|(_, f)| f.clone())
                .collect();
            // Which of this collection's indexes were seeded from an immutable
            // base — see the note on the containment check below.
            let from_base: std::collections::HashSet<String> =
                mapped_fields.iter().cloned().collect();
            for f in mapped_fields {
                self.ensure_field_index_writable(coll_hash, &f);
            }
            for ((idx_coll, idx_field), btree) in &mut self.field_indexes {
                if *idx_coll == coll_hash {
                    if let Some(key) = FieldKey::from_json(
                        payload.get(idx_field.as_str()).unwrap_or(&Value::Null)
                    ) {
                        let ids = btree.entry(key).or_default();
                        // The fast path skips an O(n) scan on the reasoning that a
                        // *new* row's hash cannot already be in the posting list.
                        // That holds for an index this database built from nothing.
                        // It does not hold for one materialised out of the mmap
                        // base: the base is immutable, so it still lists rows that
                        // have since been deleted, and a slug written again gets
                        // the same hash. Delete `p/n5` and write it back and the
                        // hash was pushed on top of the copy already there — the
                        // row was then counted twice by every aggregate that reads
                        // the index. Row counts stayed right, so it showed up as
                        // `SUM` disagreeing with the rows it summed, and only after
                        // a restart.
                        //
                        // Where a base exists the scan is paid. Where one does not —
                        // a fresh database being loaded — the fast path stands.
                        if (old_info.is_none() && !from_base.contains(idx_field))
                            || !ids.contains(&hash)
                        {
                            ids.push(hash);
                        }
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

        // Store spliced bytes directly — no re-serialize. Tagged with the node it
        // belongs to, so a read that lands on a different record can tell.
        // A failed disk write is reported, not fatal. The error type here cannot
        // name an `io::Error`, so the failure is carried as a message rather than
        // being turned into a process abort — which is what it used to be.
        let (offset, len) = self.payload_store.append_owned(hash, &buf)
            .map_err(payload_write_failed)?;

        let collection_str = payload.get("_collection")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // Refused here rather than truncated at compaction. The name goes into a
        // dictionary whose entries carry a two-byte length, so a longer one
        // cannot be written down truthfully — and a store is better off rejecting
        // a name it cannot keep than accepting it and returning a different one.
        if collection_str.len() > storage::topology::MAX_NAME_BYTES {
            use serde::ser::Error as _;
            return Err(serde_json::Error::custom(format!(
                "sekejap: collection name is {} bytes; the limit is {}",
                collection_str.len(),
                storage::topology::MAX_NAME_BYTES,
            )));
        }

        self.slug_map.insert(slug.to_string(), hash);
        // Writing a key retires any tombstone left by an earlier delete. Without
        // this the row was written and logged but stayed invisible — get() and
        // contains() both said no — until the next compaction cleared the tombstone
        // and it silently reappeared.
        if !self.tombstones.is_empty() {
            self.tombstones.remove(&hash);
        }
        self.nodes.insert(hash, NodeData {
            slug: slug.to_string(),
            collection: collection_str,
            spatial_meta: spatial_meta.clone().map(Box::new),
            payload_offset: offset,
            payload_len: len,
        });

        if self.defer_index_rebuild {
            for field in bm25_fields {
                self.dirty_bm25.insert(field);
            }
        } else {
            for field in bm25_fields {
                // Index this one document rather than rebuilding the corpus.
                // build_bm25_index is O(rows): at 20 000 rows it made a single
                // INSERT cost 134 ms, and it grew with the table.
                let text = payload.get(field.as_str())
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                match (text, self.bm25_indexes.get_mut(&field)) {
                    (Some(text), Some(ix)) => ix.insert_doc(hash, &text),
                    _ => self.build_bm25_index(&field),
                }
            }
        }

        if let Some(grid) = &mut self.spatial_grid {
            grid.remove(hash);
            if let Some(meta) = spatial_meta {
                grid.insert(hash, meta);
            }
        }

        // Search index: the FST is immutable, so this row joins the delta segment
        // rather than forcing a rebuild (deferred in a batch).
        // Skipped during WAL replay — open() rebuilds search once at the end.
        if !self.replaying {
            let coll_for_search = self.nodes.get(&hash).map(|n| n.collection.clone());
            if let Some(coll) = coll_for_search {
                self.touch_search_index_row(&coll, hash);
            }
        }

        Ok(hash)
    }

    fn remove_raw(&mut self, slug: &str) {
        self.note_key_change(slug);
        let hash = sk_hash(slug);
        // Paged mode: a node that lives in the immutable base cannot be removed
        // in place, so record a tombstone that every base-aware lookup honours
        // until the next compact() folds it away for real.
        if !self.nodes.contains_key(&hash) && self.base_contains(hash)
        {
            // Read the row before tombstoning it: every base-aware accessor honours
            // tombstones, so afterwards its own payload is unreachable and the
            // index cleanup below would silently do nothing.
            let doomed = self.get_payload(hash);
            if let Some((off, _)) = self.payload_loc(hash) {
                self.payload_store.free(off);
            }
            self.tombstones.insert(hash);
            self.slug_map.remove(slug);
            // The base row cannot be edited in place, but everything derived from
            // it still can — and this used to return here, skipping the whole
            // cascade a normal delete performs. The row then stayed in the text
            // indexes, kept its geometry in the spatial grid, kept its vectors and
            // its place in the HNSW graph, and its edges stayed traversable.
            if !self.replaying {
                for ix in self.bm25_indexes.values_mut() { ix.delete(hash); }
                for ix in self.gin_indexes.values_mut() { ix.delete(hash); }
                for ix in self.search_indexes.values_mut() { ix.delete(hash); }
                for graph in self.hnsw_indexes.values_mut() { graph.remove(hash); }
            }
            // Btree indexes too, or the deleted row keeps matching `WHERE` — and
            // writing the key again stacks a second posting for the same row, which
            // is how `WHERE n = 7` started returning the same row twice.
            if let Some(payload) = doomed {
                if let Some(coll) = payload.get("_collection").and_then(|v| v.as_str()) {
                    let ch = sk_hash(coll);
                    let mapped: Vec<String> = self.field_base.keys()
                        .filter(|(c, _)| *c == ch).map(|(_, f)| f.clone()).collect();
                    for f in mapped { self.ensure_field_index_writable(ch, &f); }
                    for ((idx_coll, idx_field), btree) in &mut self.field_indexes {
                        if *idx_coll != ch { continue; }
                        if let Some(key) = FieldKey::from_json(
                            payload.get(idx_field.as_str()).unwrap_or(&Value::Null)
                        ) {
                            if let Some(ids) = btree.get_mut(&key) {
                                ids.retain(|&id| id != hash);
                                if ids.is_empty() { btree.remove(&key); }
                            }
                        }
                    }
                }
            }
            for field_vecs in self.vectors.values_mut() { field_vecs.remove(hash); }
            if let Some(grid) = &mut self.spatial_grid { grid.remove(hash); }
            // Resident edges can be dropped outright; edges held in the immutable
            // base cannot, so record that this node's base adjacency is gone and
            // filter it at read time. This is deliberately NOT the node tombstone:
            // writing the key again brings the row back, but must not bring back
            // the edges that were deleted with it.
            self.edges.remove_node(hash);
            self.edge_tombstones.insert(hash);
            return;
        }
        if let Some(node) = self.nodes.remove(&hash) {
            // The row's bytes are dead now; a paged store reclaims them at once.
            self.payload_store.free(node.payload_offset);
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

            // Search deletes incrementally: dropping the hash from id_to_slot is
            // enough, because slot_to_hash gates on it. GIN has no incremental
            // delete (trigram bitmaps) → rebuild, deferred inside a batch.
            // Skipped during replay: open() rebuilds everything once at the end.
            if !self.replaying {
                let coll = node.collection.clone();
                let skey = Self::search_index_key(&coll);
                match self.search_indexes.get_mut(&skey) {
                    Some(ix) => ix.delete(hash),
                    None => self.touch_search_index(&coll),
                }

                // Retiring a document is O(1) and order-independent, so it runs
                // even inside a batch — deferring it would only mean a full rebuild
                // at flush, which is what this replaces.
                if !self.gin_indexes.is_empty() {
                    for ix in self.gin_indexes.values_mut() {
                        ix.delete(hash);
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

        // Members and their slugs from the base-aware accessors, not from the
        // overlay maps. Reading `self.collections` and `self.slug_map` directly
        // meant this saw only rows written since the last compaction: on a paged
        // database it dropped nothing at all, and in *any* mode after a reopen it
        // removed the schema while leaving every row queryable — and a second
        // `DROP TABLE IF EXISTS` then reported nothing to do, because neither the
        // schema nor the overlay rows were there any more. The rows became
        // permanently unreachable by DROP.
        let member_hashes: Vec<u64> = self.collection_members(col_hash)
            .map(|m| m.into_owned())
            .unwrap_or_default();

        // Collect slugs (cannot hold borrow while mutating)
        let slugs: Vec<String> = member_hashes.iter()
            .filter_map(|h| self.node_data(*h).map(|n| n.slug.clone()))
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
                // Members and their payload locations from the base-aware
                // accessors. Built from `self.collections` and `self.nodes`, this
                // saw only rows written since the last compaction — so on a paged
                // database it visited none of them and the column changed in the
                // schema while every row kept its old shape.
                let node_meta: Vec<(u64, u64, u32)> = self
                    .collection_members(col_hash)
                    .map(|m| m.into_owned())
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|h| self.payload_loc(h).map(|(o, l)| (h, o, l)))
                    .collect();
                let mut count = 0usize;
                let mut node_updates: Vec<(u64, u64, u32)> = Vec::new();
                for (h, off, len) in node_meta {
                    if let Some(mut p) = self.payload_store.get_of(h, off, len) {
                        if p.as_object_mut().map(|o| o.remove(&name).is_some()).unwrap_or(false) {
                            let new_json = serde_json::to_string(&p)
                                .unwrap_or_else(|_| "{}".to_string());
                            let (new_off, new_len) =
                                self.payload_store.append_owned(h, new_json.as_bytes())
                                    .map_err(|e| sql::SqlError::InvalidValue(
                                        format!("sekejap: payload write failed: {e}")))?;
                            node_updates.push((h, new_off, new_len));
                            count += 1;
                        }
                    }
                }
                for (h, new_off, new_len) in node_updates {
                    self.set_payload_loc(h, new_off, new_len);
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
                // Members and their payload locations from the base-aware
                // accessors. Built from `self.collections` and `self.nodes`, this
                // saw only rows written since the last compaction — so on a paged
                // database it visited none of them and the column changed in the
                // schema while every row kept its old shape.
                let node_meta: Vec<(u64, u64, u32)> = self
                    .collection_members(col_hash)
                    .map(|m| m.into_owned())
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|h| self.payload_loc(h).map(|(o, l)| (h, o, l)))
                    .collect();
                let mut count = 0usize;
                let mut node_updates: Vec<(u64, u64, u32)> = Vec::new();
                for (h, off, len) in node_meta {
                    if let Some(mut p) = self.payload_store.get_of(h, off, len) {
                        if let Some(obj) = p.as_object_mut() {
                            if let Some(val) = obj.remove(&old_name) {
                                obj.insert(new_name.clone(), val);
                                let new_json = serde_json::to_string(&p)
                                    .unwrap_or_else(|_| "{}".to_string());
                                let (new_off, new_len) =
                                    self.payload_store.append_owned(h, new_json.as_bytes())
                                        .map_err(|e| sql::SqlError::InvalidValue(
                                            format!("sekejap: payload write failed: {e}")))?;
                                node_updates.push((h, new_off, new_len));
                                count += 1;
                            }
                        }
                    }
                }
                for (h, new_off, new_len) in node_updates {
                    self.set_payload_loc(h, new_off, new_len);
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
                // Every member, not just the ones in the overlay — otherwise a
                // renamed table takes only its recent rows with it and the rest
                // stay under the old name.
                let node_hashes: Vec<u64> = self.collection_members(old_hash)
                    .map(|m| m.into_owned())
                    .unwrap_or_default();
                self.collections.remove(&old_hash);
                self.renamed_collections.insert(old_hash);
                // Every member has to appear in the overlay under the new name, so
                // that the collection it belongs to is what the new hash reports.
                for &h in &node_hashes {
                    if !self.nodes.contains_key(&h) {
                        if let Some(n) = self.base_node(h) {
                            self.slug_map.insert(n.slug.clone(), h);
                            self.nodes.insert(h, n);
                        }
                    }
                    if let Some(n) = self.nodes.get_mut(&h) { n.collection = new_name.clone(); }
                }
                let count = node_hashes.len();
                self.collections.insert(new_hash, node_hashes.clone());
                // Update the O(1) name map
                self.collection_names_map.remove(&old_hash);
                self.collection_names_map.insert(new_hash, new_name.clone());

                // Update _collection field in every node payload + cached collection field
                let node_meta: Vec<(u64, u64, u32)> = node_hashes.iter()
                    .filter_map(|&h| self.payload_loc(h).map(|(o, l)| (h, o, l)))
                    .collect();
                let mut node_updates: Vec<(u64, u64, u32)> = Vec::new();
                for (h, off, len) in node_meta {
                    if let Some(mut p) = self.payload_store.get_of(h, off, len) {
                        if let Some(obj) = p.as_object_mut() {
                            obj.insert("_collection".to_string(), serde_json::json!(new_name));
                        }
                        let new_json = serde_json::to_string(&p)
                            .unwrap_or_else(|_| "{}".to_string());
                        let (new_off, new_len) =
                            self.payload_store.append_owned(h, new_json.as_bytes())
                                .map_err(|e| sql::SqlError::InvalidValue(
                                    format!("sekejap: payload write failed: {e}")))?;
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
                let payload =
                    self.payload_store.get_of(hash, node.payload_offset, node.payload_len)?;
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
                let payload =
                    self.payload_store.get_of(hash, node.payload_offset, node.payload_len)?;
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
                use crate::query::VecMetric;
                use vector::{CosineDistance, DotProduct, L1Distance, L2Distance};
                let graph = match self.hnsw_metric(field) {
                    VecMetric::Cosine => vector::HnswGraph::build::<CosineDistance, _>(&filtered, 16, 200),
                    VecMetric::L2     => vector::HnswGraph::build::<L2Distance, _>(&filtered, 16, 200),
                    VecMetric::Dot    => vector::HnswGraph::build::<DotProduct, _>(&filtered, 16, 200),
                    VecMetric::L1     => vector::HnswGraph::build::<L1Distance, _>(&filtered, 16, 200),
                };
                self.hnsw_indexes.insert(field.to_string(), graph);
            }
        } else {
            self.hnsw_indexes.remove(field);
        }
    }

    fn link_raw(&mut self, from: &str, to: &str, edge_type: &str) {
        self.note_edge_change(edge_type);
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
        self.note_edge_change(edge_type);
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
                // A `_key` attribute makes the edge keyed: identity = (from, type,
                // sk_hash(_key)). Keyed edges dedup on re-insert (idempotent);
                // unkeyed edges stay additive (parallel allowed), as before.
                let key_hash = m.get("_key").and_then(|v| v.as_str()).map(sk_hash);
                let (cols, json) =
                    Self::route_edge_attrs(m.into_iter().collect());
                match key_hash {
                    Some(kh) => {
                        // A failed spill of the attribute bytes is recorded, not
                        // fatal — see `write_error`. The edge itself is in memory
                        // either way; what is at risk is its attributes.
                        if let Err(e) = self.edges
                            .link_keyed(from_h, to_h, type_h, edge_type, kh, &cols, json)
                        {
                            self.note_write_error(format!("edge attribute write failed: {e}"));
                        }
                    }
                    None => {
                        if let Err(e) = self.edges
                            .link_with_attrs(from_h, to_h, type_h, edge_type, &cols, json)
                        {
                            self.note_write_error(format!("edge attribute write failed: {e}"));
                        }
                    }
                }
            }
            // Non-object meta (rare): keep whole in the JSON bag as before.
            other => {
                if let Err(e) = self.edges
                    .link_meta(from_h, to_h, type_h, edge_type, other)
                {
                    self.note_write_error(format!("edge attribute write failed: {e}"));
                }
            }
        }
        Ok(())
    }

    fn unlink_raw(&mut self, from: &str, to: &str, edge_type: &str) {
        self.note_edge_change(edge_type);
        let from_h = sk_hash(from);
        let to_h = sk_hash(to);
        let type_h = sk_hash(edge_type);
        self.edges.unlink(from_h, to_h, type_h);
        // The overlay is only half the graph. Anything already in the durable base
        // is untouched by the line above, so the withdrawal has to be recorded
        // where reads of the base can see it.
        self.unlinked_edges.insert((from_h, to_h, type_h));
    }

    /// Delete only edges from→to of `edge_type` whose attributes match the JSON
    /// `props` object (equality). Empty object = delete all. Returns count removed.
    /// Edges from→to of this type, living in the durable base rather than the
    /// overlay, whose attributes match `props`.
    ///
    /// `EdgeStore` only ever sees the overlay, so anything that filters edges by
    /// attribute — `unlink_where`, `update_edge` — silently did nothing to a base
    /// edge. `unlink` had the same hole and was fixed by recording the withdrawal
    /// where base reads could subtract it; these two never got the same treatment.
    fn matching_base_edges(&self, from_h: u64, to_h: u64, type_h: u64,
                           props: &[(String, Value)]) -> Vec<Edge> {
        let Some(edges) = self.fwd_edges(from_h) else { return Vec::new() };
        edges.iter()
            .filter(|e| e.other == to_h && e.edge_type == type_h)
            // Only the ones the overlay does not already hold: those it does are
            // handled by `EdgeStore` itself, and withdrawing them here as well
            // would double-count.
            .filter(|e| e.base_meta_ref().is_some() || self.edges.fwd_edges(from_h)
                .is_none_or(|o| !o.iter().any(|x| x.other == e.other
                                              && x.edge_type == e.edge_type)))
            .filter(|e| {
                if props.is_empty() { return true }
                let have = self.edge_all_attrs(e).unwrap_or(Value::Null);
                props.iter().all(|(k, v)| have.get(k) == Some(v))
            })
            .cloned()
            .collect()
    }

    fn unlink_where_raw(&mut self, from: &str, to: &str, edge_type: &str, props_json: &str) -> usize {
        let from_h = sk_hash(from);
        let to_h = sk_hash(to);
        let type_h = sk_hash(edge_type);
        let props: Vec<(String, Value)> = serde_json::from_str::<Value>(props_json)
            .ok()
            .and_then(|v| match v {
                Value::Object(m) => Some(m.into_iter().collect()),
                _ => None,
            })
            .unwrap_or_default();
        // The overlay's own copies, then the durable ones — which `EdgeStore`
        // cannot see and which used to be left in place, so an `unlink_where`
        // against a compacted graph reported a count and changed nothing.
        let mut n = self.edges.unlink_matching(from_h, to_h, type_h, &props);
        for _ in self.matching_base_edges(from_h, to_h, type_h, &props) {
            self.unlinked_edges.insert((from_h, to_h, type_h));
            n += 1;
        }
        n
    }

    // ── WAL helpers ───────────────────────────────────────────────────────────

    /// Record one change in the write-ahead log (WAL) — the durability step.
    ///
    /// A WAL is the classic crash-safety trick: before we change any in-memory
    /// state, we append a description of the change to the end of a log file. If
    /// the process crashes mid-write, the next `open()` replays the log and ends
    /// up exactly where it left off. This is why every public write calls
    /// `wal_write` *before* touching the maps.
    ///
    /// Two subtleties:
    /// - `if let Some(wal)` — an in-memory (ephemeral) database has no WAL
    ///   (`wal` is `None`), so this is a no-op there; nothing is persisted.
    /// - **fsync policy.** `append` writes to the file, but the OS may still hold
    ///   those bytes in its own buffer. `sync()` (fsync) forces them to the
    ///   physical disk. We only fsync per-write under `Full` durability; `Normal`/
    ///   `Off` defer the fsync to the next checkpoint/compact. That's the standard
    ///   mobile trade-off — a per-write fsync on phone flash costs tens of ms.
    /// Remember a disk failure that had nowhere to be returned to.
    ///
    /// Only the first is kept. What matters is that the database stopped being
    /// able to write, not how many times it noticed.
    fn note_write_error(&mut self, msg: String) {
        if self.write_error.is_none() {
            self.write_error = Some(format!("sekejap: {msg}"));
        }
    }

    /// Refuse a mutation on a handle opened read-only.
    ///
    /// Returned as an error rather than ignored. A write that is dropped in
    /// silence is worse than one that fails: the caller carries on believing the
    /// row is there, reads it back from the overlay in the same session, and only
    /// finds out at the next open — if anyone is looking.
    fn refuse_write(&self, what: &str) -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("sekejap: {what} on a database opened read-only"),
        )
    }

    /// Whether this handle was opened read-only, so writes are refused.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// The disk failure this database has hit, if any.
    ///
    /// `Some` means at least one write reached the in-memory maps but did not
    /// reach the log, so it is not durable and will not survive a restart. Reads
    /// still answer from memory and are still true for this process; compaction
    /// refuses, because folding an overlay into the base and dropping an
    /// incomplete log is how a failed write becomes a lost one.
    ///
    /// A database in this state should be closed and reopened. Replay will decide
    /// what is actually on disk.
    pub fn write_error(&self) -> Option<&str> {
        self.write_error.as_deref()
    }

    fn wal_write(&mut self, entry: WalEntry) {
        self.counters.writes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(wal) = &mut self.wal {
            // Recorded rather than fatal — see `write_error`. The write that
            // triggered this has already changed the in-memory maps, so it is
            // reported as durable when it is not; the flag is how that gets told,
            // and it stops the compaction that would make it permanent.
            let mut failed = wal.append(&entry).err();
            // Force-flush to physical disk only when durability is Full and we're
            // not inside a batch (`defer_wal_sync`), which fsyncs once at the end.
            if failed.is_none()
                && !self.defer_wal_sync && !self.group_commit
                && self.wal_sync == SyncMode::Full
            {
                failed = wal.sync().err();
            }
            if let Some(e) = failed {
                self.note_write_error(format!("WAL write failed: {e}"));
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
        // The other trigger: the RAM write-overlay has grown past its bound.
        //
        // `has_base`, not `!segments.is_empty()`. The guard is asking "would a
        // compaction actually move these nodes out of RAM" — pointless in a purely
        // resident database, where the maps *are* the database and folding them
        // changes nothing — and a mapped segment is only one of the two things
        // that make the answer yes. Paged nodes are the other, and they are the
        // default.
        //
        // `compact()` had this exact bug and it was fixed there (see
        // `overlay_becomes_durable`); the eligibility check that decides whether
        // to call it never got the same correction. So the threshold documented as
        // bounding RAM growth in paged mode never fired: 5 500 nodes against a
        // 1 000-node bound left `maybe_compact` returning false, and the only live
        // trigger was the 64 MB log.
        self.has_base()
            && self.nodes.len() >= self.compact_thresholds.overlay_entries
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
        if self.wal_sync != SyncMode::Full || self.group_commit { return; }
        if let Some(wal) = &mut self.wal {
            wal.sync()
                .map_err(|e| format!("WAL fsync failed: {e}"))
                .err()
                .map(|e| self.note_write_error(e));
        }
    }

    // ── Change notification ────────────────────────────────────────────────────

    /// Record that a node slug changed (put/update/remove). Derives the
    /// collection from the slug (`collection/key`). Accumulates into the pending
    /// event; flushed once at the mutation boundary by [`emit_changes`].
    fn note_key_change(&mut self, slug: &str) {
        if self.change_listeners.is_empty() { return; }
        ChangeEvent::push_unique(&mut self.pending_change.keys, slug);
        if let Some((coll, _)) = slug.split_once('/') {
            ChangeEvent::push_unique(&mut self.pending_change.collections, coll);
        }
    }

    /// Record that edges of a type changed (link/unlink).
    fn note_edge_change(&mut self, edge_type: &str) {
        if self.change_listeners.is_empty() { return; }
        ChangeEvent::push_unique(&mut self.pending_change.edge_types, edge_type);
    }

    /// Flush accumulated changes to listeners as one [`ChangeEvent`]. No-op
    /// inside a transaction (`defer_wal_sync` set, or `commit_depth` > 0 during
    /// COMMIT replay) — changes accumulate and emit once at COMMIT — or when
    /// nothing changed / no one is listening.
    fn emit_changes(&mut self) {
        if self.defer_wal_sync
            || self.commit_depth > 0
            || self.change_listeners.is_empty()
            || self.pending_change.is_empty()
        {
            return;
        }
        let event = std::mem::take(&mut self.pending_change);
        for (_, cb) in self.change_listeners.iter_mut() {
            cb(&event);
        }
    }

    /// Post-mutation boundary: emit the change event, then run auto-compaction.
    /// Called at the end of every public mutation.
    fn after_mutation(&mut self) {
        self.emit_changes();
        self.autocompact_after_write();
    }

    /// Subscribe to change events. The callback fires once per committed
    /// mutation (a transaction fires once, at COMMIT) with the set of
    /// collections, keys, and edge types that changed — the basis for reactive
    /// `watch`-style queries. Returns an id for [`unsubscribe_changes`]. The
    /// callback must not call back into this database.
    pub fn subscribe_changes(
        &mut self,
        f: impl FnMut(&ChangeEvent) + Send + Sync + 'static,
    ) -> u64 {
        let id = self.next_change_id;
        self.next_change_id += 1;
        self.change_listeners.push((id, Box::new(f)));
        id
    }

    /// Remove a change subscription registered with [`subscribe_changes`].
    pub fn unsubscribe_changes(&mut self, id: u64) {
        self.change_listeners.retain(|(i, _)| *i != id);
    }

    /// Fold every BM25 delta into its base.
    ///
    /// The on-disk BM25 format holds a single contiguous segment, so an index
    /// carrying an unmerged delta cannot be serialised — `write_binary` refuses,
    /// rather than silently writing an index that omits the newest documents.
    /// Anything that persists indexes calls this first.
    fn merge_index_deltas(&mut self) {
        self.merge_search_deltas();
        self.merge_gin_overlays();
        self.merge_bm25_deltas_inner();
    }

    /// Fold resident GIN writes back into a single segment before anything is
    /// persisted.
    ///
    /// `gin.bin` holds one flat segment and is left untouched while any index is
    /// served from it, so writes sitting on top of the mmap base have nowhere to go
    /// and would vanish on reopen. Rebuilding every disk-backed index — not only the
    /// ones with an overlay — is deliberate: the file is written as a whole, so a
    /// partial rebuild would persist the rebuilt indexes and drop the rest.
    fn merge_gin_overlays(&mut self) {
        if !self.gin_indexes.values().any(|g| g.has_pending_overlay()) {
            return;
        }
        let fields: Vec<String> = self.gin_indexes.iter()
            .filter(|(_, g)| g.is_disk_backed())
            .map(|(k, _)| k.clone())
            .collect();
        for field in fields {
            self.build_gin_index(&field);
        }
    }

    /// Fold every search delta into its base by rebuilding the collection's index.
    /// Unlike BM25 this needs the base documents' text, so it lives here rather
    /// than inside the index.
    fn merge_search_deltas(&mut self) {
        let stale: Vec<String> = self.search_indexes.iter()
            .filter(|(_, ix)| ix.delta_len() > 0)
            .map(|(k, _)| k.clone())
            .collect();
        for coll in stale {
            self.rebuild_search_for_collection(&coll);
        }
    }

    fn merge_bm25_deltas_inner(&mut self) {
        let dir = self.data_dir.clone();
        for (field, ix) in self.bm25_indexes.iter_mut() {
            if ix.delta_len() == 0 {
                continue;
            }
            ix.merge_delta();
            // Merging rebuilds the postings blob, so the dictionary offsets about
            // to be written no longer address the spilled file. Rewrite it in the
            // same breath — a dictionary that outlives its postings is exactly the
            // kind of split-brain that makes an index unreadable on reopen.
            #[cfg(unix)]
            if let Some(ref dir) = dir {
                let _ = ix.spill_to_disk(&dir.join(format!("bm25_{field}.postings")));
            }
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
    /// Index a single row into `collection`'s search index.
    ///
    /// The collection-wide rebuild this replaces was `O(rows)` — 144 ms per INSERT
    /// at 20 000 rows. Falls back to a rebuild when there is nothing to add to
    /// (no index yet) or when the delta has grown enough to be worth folding in.
    fn touch_search_index_row(&mut self, collection: &str, hash: u64) {
        if collection.is_empty() || !self.collection_has_search_index(collection) {
            return;
        }
        if self.defer_index_rebuild {
            self.dirty_search.insert(collection.to_string());
            return;
        }
        let key = Self::search_index_key(collection);
        let fields = match self.search_indexes.get(&key) {
            Some(ix) if ix.delta_len() < SEARCH_DELTA_MERGE_DOCS => ix.fields.clone(),
            // No index yet, or the delta has earned a merge: only the database can
            // do that, since folding in needs the base documents' text.
            _ => return self.rebuild_search_for_collection(collection),
        };
        let payload = match self.get_payload(hash) {
            Some(p) => p,
            None => return self.rebuild_search_for_collection(collection),
        };
        let field_values: Vec<String> = fields.iter()
            .map(|f| payload.get(f).and_then(|v| v.as_str()).unwrap_or("").to_string())
            .collect();
        if let Some(ix) = self.search_indexes.get_mut(&key) {
            ix.insert_doc(search::index::DocFields { hash, field_values });
        }
    }

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
                // Base-aware. `self.nodes` is only the write overlay in paged mode,
                // so looking there alone dropped every row that still lived in the
                // compacted base — the planner matched them, this discarded them,
                // and the UPDATE reported 0 rows and silently lost the write.
                let n = self.node_data(hash)?;
                let raw = self.payload_store.get_raw_of(hash, n.payload_offset, n.payload_len)?;
                Some((n.slug.clone(), hash, raw))
            })
            .collect();
        let count = hits.len();
        if count == 0 { return Ok(0); }
        if !self.change_listeners.is_empty() {
            let slugs: Vec<String> = hits.iter().map(|(s, _, _)| s.clone()).collect();
            for slug in &slugs { self.note_key_change(slug); }
        }

        // Schema validation (once for the batch)
        let coll_name = self.node_data(hits[0].1)
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
        //
        // `field_indexes` is the *writable* map. On a paged store the index was
        // loaded as an mmap'd sidecar into `field_base` instead, so this map is
        // empty and every field looked unindexed — neither the old key nor the new
        // one was maintained, and the index kept answering from before the update.
        // That survives a compaction and a reopen: `WHERE n = 5` returns the row
        // that is now 9999, `WHERE n = 9999` returns nothing, and `MAX(n)` reports
        // a value no row holds any more.
        //
        // `put_raw` gets this right by hydrating first. This path never did, which
        // is the difference between a write and an update on the same column.
        if let Some(ch) = coll_hash {
            for (field, _) in updates.iter() {
                self.ensure_field_index_writable(ch, field);
            }
        }
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

        // Which rows actually changed — needed for index maintenance below, and
        // captured here because the WAL phase consumes `batch`.
        let touched: Vec<u64> = batch.iter().map(|(_, h, _)| *h).collect();

        // ── Phase 2: batch payload write (one syscall) ───────────
        let offsets = {
            let refs: Vec<&[u8]> = batch.iter()
                .map(|(_, _, buf)| buf.as_slice()).collect();
            let owners: Vec<u64> = batch.iter().map(|(_, h, _)| *h).collect();
            self.payload_store.append_batch(&refs, &owners)
                .map_err(|e| SqlError::InvalidValue(
                    format!("sekejap: payload write failed: {e}")))?
        };

        // ── Phase 3: update node metadata ────────────────────────
        for (i, (_, hash, _)) in batch.iter().enumerate() {
            if let Some(node) = self.nodes.get_mut(hash) {
                node.payload_offset = offsets[i].0;
                node.payload_len = offsets[i].1;
                continue;
            }
            // Paged mode: the row still lives in the immutable base, which cannot be
            // edited in place. Promote it into the write overlay pointing at the new
            // payload — the overlay wins over the base for every base-aware lookup,
            // and compact() folds it down later.
            if let Some(mut nd) = self.node_data(*hash).map(|n| n.into_owned()) {
                nd.payload_offset = offsets[i].0;
                nd.payload_len = offsets[i].1;
                self.nodes.insert(*hash, nd);
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
                        .map_err(|e| SqlError::InvalidValue(
                            format!("sekejap: WAL write failed: {e}")))?;
                    if !self.defer_wal_sync && !self.group_commit && self.wal_sync == SyncMode::Full {
                        wal.sync().map_err(|e| SqlError::InvalidValue(
                            format!("sekejap: WAL fsync failed: {e}")))?;
                    }
                }
            }
        }

        // Index maintenance. Re-indexing only the rows that changed costs
        // O(rows touched); rebuilding costs O(table). Past a point the rebuild wins,
        // so wide updates still take it.
        let incremental = touched.len() <= UPDATE_INCREMENTAL_ROWS;
        for (field, _) in updates {
            let has_gin = self.gin_indexes.contains_key(field.as_str());
            let has_bm25 = self.bm25_indexes.contains_key(field.as_str());
            if !has_gin && !has_bm25 {
                continue;
            }
            if !incremental {
                if has_gin { self.build_gin_index(field); }
                if has_bm25 { self.build_bm25_index(field); }
                continue;
            }
            for &h in &touched {
                let text = self.get_payload(h)
                    .and_then(|p| p.get(field).and_then(|v| v.as_str()).map(|s| s.to_string()));
                let Some(text) = text else { continue };
                if has_gin {
                    if let Some(ix) = self.gin_indexes.get_mut(field.as_str()) {
                        ix.insert_doc(h, &text);
                    }
                }
                if has_bm25 {
                    if let Some(ix) = self.bm25_indexes.get_mut(field.as_str()) {
                        ix.insert_doc(h, &text);
                    }
                }
            }
        }
        if !coll_name.is_empty() {
            if incremental {
                for &h in &touched { self.touch_search_index_row(&coll_name, h); }
            } else {
                self.touch_search_index(&coll_name);
            }
        }

        self.emit_changes();
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
            WalEntry::UnlinkWhere {
                from,
                to,
                edge_type,
                props,
            } => {
                self.unlink_where_raw(&from, &to, &edge_type, &props);
            }
            WalEntry::UpdateEdge {
                from,
                to,
                edge_type,
                props,
                sets,
            } => {
                self.update_edge_raw(&from, &to, &edge_type, &props, &sets);
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
                if let Err(e) = self.vectors.entry(field).or_default().put(hash, data) {
                    self.note_write_error(format!("vector write failed: {e}"));
                }
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

    /// Insert or update one node from a JSON string.
    ///
    /// `slug` is the node's `"collection/key"` id; the `_collection` field inside
    /// the payload is what registers it in a named collection for
    /// `db.collection()` queries. Called again with the same slug, it overwrites.
    ///
    /// The order matters for crash-safety: the change is written to the WAL
    /// **first** (so a crash mid-write can be replayed on the next open), and only
    /// then applied to the in-memory maps, the on-disk payload store, and any
    /// affected indexes. Returns the node's id hash on success, or a parse error
    /// if `payload_json` isn't valid JSON.
    pub fn put(&mut self, slug: &str, payload_json: &str) -> Result<u64, serde_json::Error> {
        // Validate the JSON up front — a bad payload must fail before we touch the WAL.
        let payload: Value = serde_json::from_str(payload_json)?;

        // Durability first: append the intent to the WAL before mutating state.
        self.wal_write(WalEntry::Put {
            slug: slug.to_string(),
            payload: payload_json.to_string(),
        });

        let hash = self.put_raw_indexed(slug, payload_json, payload)?;
        self.after_mutation();
        Ok(hash)
    }

    /// Store a row and maintain every index that depends on it, without touching
    /// the WAL — the caller decides how the intent is logged.
    ///
    /// This is the body `put` used to carry inline, and carrying it inline is why a
    /// transaction's rows were stored but never indexed: `Transaction::commit`
    /// applies its operations with `put_raw`, which reaches `put_raw_inner` and so
    /// maintains bm25, spatial and search — but the GIN maintenance lived out here,
    /// in `put`, where a transaction never went. Ten committed rows containing
    /// "fox" left `ILIKE '%fox%'` answering eleven where the data said sixteen.
    fn put_raw_indexed(&mut self, slug: &str, payload_json: &str, payload: Value)
        -> Result<u64, serde_json::Error>
    {
        let node_hash = sk_hash(slug);
        // Whether this is a fresh insert or an overwrite decides index bookkeeping
        // (an update must first retract the node's old index entries).
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
        // Saved and restored, not set and cleared. Called inside a larger batch —
        // `begin_bulk`, a transaction, another bulk helper — a hard reset ends the
        // outer group early, and every write after it fsyncs one at a time while
        // the caller believes it is still batching. `link_meta_many` documents
        // this and handles it; its two siblings did not, and `put_many` was
        // already saving `batch_now` for the same reason two lines below.
        let prev_defer = self.defer_wal_sync;
        let prev_index = self.defer_index_rebuild;
        self.defer_wal_sync = true;
        self.defer_index_rebuild = true;
        // One timestamp for the whole batch — skip a per-row time syscall.
        let had_batch_now = self.batch_now.is_some();
        if !had_batch_now {
            self.batch_now = Some(chrono::Utc::now().timestamp_millis());
        }
        let result: Result<Vec<u64>, _> = items
            .into_iter()
            .map(|(slug, json)| self.put(slug, json))
            .collect();
        if !had_batch_now {
            self.batch_now = None;
        }
        self.defer_wal_sync = prev_defer;
        self.wal_flush();
        self.flush_deferred_indexes();
        self.defer_index_rebuild = prev_index;
        self.after_mutation();
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
        if self.read_only {
            use serde::ser::Error as _;
            return Err(serde_json::Error::custom(self.refuse_write("bulk write").to_string()));
        }
        if rows.is_empty() {
            return Ok(0);
        }
        let now = self.batch_now.unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let now_val: Value = now.into();

        let mut wal_entries: Vec<WalEntry> = Vec::with_capacity(rows.len());
        // (slug, hash, collection, spatial_meta, is_new)
        let mut metas: Vec<(String, u64, String, Option<geo::SpatialMeta>, bool)> =
            Vec::with_capacity(rows.len());
        // Which fields carry a btree index, per collection — read once, because the
        // payload is only in scope inside the loop below and `field_indexes` cannot
        // be borrowed mutably while it runs.
        let indexed_by_coll: HashMap<u64, Vec<String>> = {
            let mut m: HashMap<u64, Vec<String>> = HashMap::new();
            for (c, f) in self.field_indexes.keys().chain(self.field_base.keys()) {
                let v = m.entry(*c).or_default();
                if !v.contains(f) { v.push(f.clone()) }
            }
            m
        };
        let mut pending_keys: Vec<Vec<(String, FieldKey)>> = Vec::with_capacity(rows.len());

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
                    .and_then(|(o, l)| self.payload_store.get_of(hash, o, l))
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
            pending_keys.push(
                indexed_by_coll.get(&sk_hash(&coll)).map_or_else(Vec::new, |fields| {
                    fields.iter()
                        .filter_map(|f| FieldKey::from_json(val.get(f.as_str()).unwrap_or(&Value::Null))
                            .map(|k| (f.clone(), k)))
                        .collect()
                }));
            metas.push((slug, hash, coll, spatial_meta, is_new));
        }

        // WAL first (durable), then payloads (payloads.bin is rebuilt from the WAL
        // on open, so this ordering is crash-safe). One batch each, one fsync.
        if let Some(wal) = &mut self.wal {
            wal.append_batch(&wal_entries).map_err(wal_write_failed)?;
            if !self.defer_wal_sync && !self.group_commit && self.wal_sync == SyncMode::Full {
                wal.sync().map_err(wal_write_failed)?;
            }
        }
        // Payload bytes borrowed straight from the WAL entries — no extra copy.
        let refs: Vec<&[u8]> = wal_entries.iter().map(|e| match e {
            WalEntry::Put { payload, .. } => payload.as_bytes(),
            _ => &[][..],
        }).collect();
        let owners: Vec<u64> = metas.iter().map(|(_, h, _, _, _)| *h).collect();
        let offsets = self.payload_store.append_batch(&refs, &owners)
            .map_err(payload_write_failed)?;

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
                // Btree indexes, which this path never maintained. It refreshes
                // bm25, GIN and search and stops there, so a row written through it
                // was invisible to every indexed `WHERE` until the process
                // restarted and the index was rebuilt from disk — a full scan found
                // it, `WHERE t = 5` did not. This is the buffered prepared-insert
                // route, so it is the IoT write path specifically.
                //
                // Same two halves as the single-row path: bring an index that lives
                // only in the mmap base into the heap first, or the update lands
                // nowhere.
                let mapped: Vec<String> = self.field_base.keys()
                    .filter(|(c, _)| *c == coll_hash)
                    .map(|(_, f)| f.clone())
                    .collect();
                // Same rule as `put_raw_indexed`: an index materialised from the
                // immutable base still lists rows that were deleted, so "this row
                // is new" does not imply "its hash is not already here".
                let from_base: std::collections::HashSet<String> =
                    mapped.iter().cloned().collect();
                for f in mapped { self.ensure_field_index_writable(coll_hash, &f); }
                for (field, key) in pending_keys[i].drain(..) {
                    let fresh = is_new && !from_base.contains(&field);
                    let ids = self.field_indexes
                        .entry((coll_hash, field)).or_default()
                        .entry(key).or_default();
                    if fresh || !ids.contains(&hash) { ids.push(hash); }
                }
            }
            self.slug_map.insert(slug.clone(), hash);
            self.nodes.insert(hash, NodeData {
                slug,
                collection: coll,
                spatial_meta: spatial_meta.clone().map(Box::new),
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
            self.after_mutation();
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
            // Auto-compaction was suppressed for every statement in the group:
            // `autocompact_after_write` returns at the `defer_wal_sync` guard
            // *before* bumping its 64-write amortisation counter, so that counter
            // never advances during a batch and calling it here would not reach
            // the threshold check. Test the thresholds directly instead, once the
            // batch is durable. Changes were already emitted per statement, so
            // only compaction is re-checked here — not the full `after_mutation`.
            if self.auto_compact == AutoCompact::OnWrite
                && !self.autocompacting
                && self.compact_eligible()
            {
                self.autocompacting = true;
                let _ = self.compact();
                self.autocompacting = false;
                self.writes_since_compact_check = 0;
            }
        }
        result.map(|_| total)
    }

    pub fn begin_bulk(&mut self) { self.defer_wal_sync = true; }
    pub fn end_bulk(&mut self) {
        self.defer_wal_sync = false;
        self.wal_flush();
        self.after_mutation();
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
        // Nesting-safe, as `link_meta_many` already is — see the note there.
        let prev = self.defer_wal_sync;
        self.defer_wal_sync = true;
        for (from, to, edge_type) in edges {
            self.link(from, to, edge_type);
        }
        self.defer_wal_sync = prev;
        self.wal_flush();
        self.after_mutation();
    }

    /// Bulk edge insert with optional per-edge metadata — the attributed
    /// counterpart of [`link_many`](Self::link_many). Each item is
    /// `(from, to, edge_type, meta_json)`; `None` metadata takes the naked-edge
    /// fast lane ([`link`]), `Some(json)` rides [`link_meta`]. WAL fsync is
    /// deferred and flushed once for the whole batch — the primitive a graph
    /// import needs so attributed edges don't fsync one at a time.
    ///
    /// Nesting-safe: the previous defer state is saved and restored, so calling
    /// this inside a larger batch never prematurely flushes the outer group.
    /// Stops and returns the first metadata parse error encountered.
    pub fn link_meta_many<'a>(
        &mut self,
        edges: impl IntoIterator<Item = (&'a str, &'a str, &'a str, Option<&'a str>)>,
    ) -> Result<(), serde_json::Error> {
        let prev = self.defer_wal_sync;
        self.defer_wal_sync = true;
        let mut result = Ok(());
        for (from, to, edge_type, meta) in edges {
            match meta {
                Some(m) => {
                    if let Err(e) = self.link_meta(from, to, edge_type, m) {
                        result = Err(e);
                        break;
                    }
                }
                None => self.link(from, to, edge_type),
            }
        }
        self.defer_wal_sync = prev;
        if !prev {
            self.wal_flush();
        }
        self.after_mutation();
        result
    }

    /// Delete a node by slug, along with its collection membership and every
    /// edge that touches it.
    ///
    /// Note the shape shared by every public mutation: log the intent to the WAL
    /// first (durability), then apply it via the no-WAL `_raw` helper, then run
    /// `after_mutation` (fire the change feed, maybe auto-compact). `put`, `link`,
    /// and `remove` are all this same three-step dance.
    pub fn remove(&mut self, slug: &str) {
        if self.read_only {
            let e = self.refuse_write("remove").to_string();
            self.note_write_error(e);
            return;
        }
        self.wal_write(WalEntry::Remove { slug: slug.to_string() }); // 1. log
        self.remove_raw(slug);   // 2. apply to maps/indexes (no WAL)
        self.after_mutation();   // 3. notify + maybe compact
    }

    /// Create a directed graph edge: `from` → `to`, labelled `edge_type`.
    ///
    /// An edge is what makes this a *graph* database: a first-class connection
    /// between two nodes that queries can traverse (`.forward("follows")`, the
    /// `MATCH` patterns, shortest-path). This variant is a **naked** edge — just
    /// a typed arrow, no attributes; use [`link_attr`] / [`link_meta`] for a
    /// weighted or attributed edge. The endpoints don't have to exist yet: an
    /// edge is stored by the id hashes of its slugs, so you can wire up the graph
    /// before (or without) inserting the nodes themselves.
    pub fn link(&mut self, from: &str, to: &str, edge_type: &str) {
        if self.read_only {
            let e = self.refuse_write("link").to_string();
            self.note_write_error(e);
            return;
        }
        // Same WAL-first pattern as `put`/`remove` (see `remove` above).
        self.wal_write(WalEntry::Link {
            from: from.to_string(),
            to: to.to_string(),
            edge_type: edge_type.to_string(),
        });
        self.link_raw(from, to, edge_type);
        self.after_mutation();
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
        self.after_mutation();
        Ok(())
    }

    /// Remove all directed edges from → to with the given type.
    pub fn unlink(&mut self, from: &str, to: &str, edge_type: &str) {
        if self.read_only {
            let e = self.refuse_write("unlink").to_string();
            self.note_write_error(e);
            return;
        }
        self.wal_write(WalEntry::Unlink {
            from: from.to_string(),
            to: to.to_string(),
            edge_type: edge_type.to_string(),
        });
        self.unlink_raw(from, to, edge_type);
        self.after_mutation();
    }

    /// Set attributes (`sets_json` object) on edges from→to of `edge_type` matching
    /// the `props_json` predicate. Returns count updated.
    fn update_edge_raw(&mut self, from: &str, to: &str, edge_type: &str, props_json: &str, sets_json: &str) -> usize {
        self.note_edge_change(edge_type);
        let from_h = sk_hash(from);
        let to_h = sk_hash(to);
        let type_h = sk_hash(edge_type);
        let obj_to_vec = |s: &str| -> Vec<(String, Value)> {
            serde_json::from_str::<Value>(s).ok()
                .and_then(|v| match v { Value::Object(m) => Some(m.into_iter().collect()), _ => None })
                .unwrap_or_default()
        };
        let pred = obj_to_vec(props_json);
        let (set_cols, set_json_opt) = Self::route_edge_attrs(obj_to_vec(sets_json));
        let set_json = match set_json_opt {
            Some(Value::Object(m)) => m,
            _ => serde_json::Map::new(),
        };
        let mut n = match self.edges.update_matching(
            from_h, to_h, type_h, &pred, &set_cols, &set_json)
        {
            Ok(n) => n,
            Err(e) => {
                self.note_write_error(format!("edge attribute write failed: {e}"));
                0
            }
        };

        // A durable edge cannot be edited where it lies — the base is immutable and
        // the paged store is written only by a fold. So it is withdrawn and written
        // again into the overlay with the new attributes, which is what an update
        // of an immutable record is. Reads merge the two and see one edge with the
        // new values; the next compaction folds the overlay copy in and the
        // withdrawal removes the old one.
        let base_matches = self.matching_base_edges(from_h, to_h, type_h, &pred);
        for e in base_matches {
            let mut merged = match self.edge_all_attrs(&e) {
                Some(Value::Object(m)) => m,
                _ => serde_json::Map::new(),
            };
            // Only the JSON bag here; the fast-lane columns go to `link_with_attrs`
            // as columns, and a read merges them over the bag afterwards.
            for (k, v) in &set_json { merged.insert(k.clone(), v.clone()); }
            for (k, _) in &set_cols { merged.remove(k); }
            self.unlinked_edges.insert((from_h, to_h, type_h));
            if let Err(e) = self.edges.link_with_attrs(
                from_h, to_h, type_h, edge_type,
                &set_cols, Some(Value::Object(merged)))
            {
                self.note_write_error(format!("edge attribute write failed: {e}"));
            }
            n += 1;
        }
        n
    }

    /// Set attributes on edges from→to of `edge_type` matching `props_json`.
    /// Returns count updated. `sets_json` is a JSON object of field→value.
    pub fn update_edge(&mut self, from: &str, to: &str, edge_type: &str, props_json: &str, sets_json: &str) -> usize {
        self.wal_write(WalEntry::UpdateEdge {
            from: from.to_string(),
            to: to.to_string(),
            edge_type: edge_type.to_string(),
            props: props_json.to_string(),
            sets: sets_json.to_string(),
        });
        let n = self.update_edge_raw(from, to, edge_type, props_json, sets_json);
        self.after_mutation();
        n
    }

    /// Delete only edges from→to of `edge_type` matching the JSON `props` predicate
    /// (e.g. `{"_key":"u1"}`). Empty `props` deletes all. Returns count removed.
    pub fn unlink_where(&mut self, from: &str, to: &str, edge_type: &str, props_json: &str) -> usize {
        self.wal_write(WalEntry::UnlinkWhere {
            from: from.to_string(),
            to: to.to_string(),
            edge_type: edge_type.to_string(),
            props: props_json.to_string(),
        });
        let n = self.unlink_where_raw(from, to, edge_type, props_json);
        self.after_mutation();
        n
    }

    // ── Persistence ───────────────────────────────────────────────────────────

    /// Compact the database: write a full snapshot then truncate the WAL.
    ///
    /// Returns the current WAL encoding format.
    pub fn wal_format(&self) -> WalFormat {
        self.wal_format
    }

    /// Fold the write-ahead log into a fresh snapshot — the key to fast startup.
    ///
    /// Every write appends to the WAL, so the log only grows; without compaction,
    /// `open()` would replay an ever-longer history. Compaction rewrites the
    /// current state into two clean artifacts — a defragmented `payloads.bin`
    /// (only *live* nodes, dropped ones' space reclaimed) and a `snapshot.json`
    /// manifest — and then truncates the WAL to empty. The next `open()` just
    /// loads the snapshot and replays nothing, so startup stays fast no matter
    /// how many writes happened.
    ///
    /// Crash-safety throughout: new files are written to a `.tmp` path and then
    /// atomically `rename`d over the real one, so a crash mid-compaction leaves
    /// the old, still-valid files in place. Memory-only databases have no files,
    /// so this is a no-op for them.
    pub fn compact(&mut self) -> io::Result<()> {
        if self.read_only {
            return Err(self.refuse_write("compact"));
        }
        // A database that has already failed to write does not get to rewrite
        // itself. Compaction folds the overlay into the base and drops the log —
        // and if a write reached memory but not the log, the log is the only
        // record that it is missing. Folding here would write the in-memory state
        // out as the truth and throw away the evidence that it is not.
        //
        // Nothing that can be wrong about what exists may delete it. This is that
        // law at the one place in the codebase that deletes.
        if let Some(err) = &self.write_error {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("{err} — refusing to compact: reopen the database so the \
                         write-ahead log can be replayed"),
            ));
        }
        let t0 = std::time::Instant::now();
        // GUARD RAIL — data safety. Compaction rewrites the entire store; a bug here
        // silently destroys data (paged compaction did exactly that: it rebuilt from
        // the overlay alone and dropped every base-resident node). Count what must
        // survive first, and refuse to report success if it did not.
        let expected = self.node_count().saturating_sub(self.tombstones.len());
        // Edges are counted too. The first version of this guard checked only nodes,
        // and compaction went on to destroy every edge in the graph while reporting
        // success — the rail passed because the rows it watched were all still there.
        // Count what a full traversal can actually reach, not what one map happens
        // to hold, or a spilled adjacency reads as zero and the check is worthless.
        //
        // Through the same function the inner rail uses, so the two cannot drift
        // apart and both get the cheap count where one is available.
        let (_, expected_edges) = self.compaction_expectation();
        let r = self.compact_inner();
        if r.is_ok() {
            let actual = self.node_count();
            if actual < expected {
                // Do NOT return Ok: the caller must never believe a lossy compaction
                // succeeded. The WAL and the previous files are still on disk.
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "sekejap: compaction would lose data ({expected} nodes before, \
                         {actual} after) — aborted before it could be trusted. This is \
                         a bug; please report it with the database layout."
                    ),
                ));
            }
            let (_, actual_edges) = self.compaction_expectation();
            if actual_edges < expected_edges {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "sekejap: compaction would lose edges ({expected_edges} before, \
                         {actual_edges} after) — aborted before it could be trusted. \
                         This is a bug; please report it with the database layout."
                    ),
                ));
            }
        }
        let us = t0.elapsed().as_micros() as u64;
        self.counters.compactions.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.counters.compact_us_last.store(us, std::sync::atomic::Ordering::Relaxed);
        self.counters.compact_us_max.fetch_max(us, std::sync::atomic::Ordering::Relaxed);
        r
    }


    /// Temporary: report RSS at a labelled point when SK_COMPACT_RSS is set.
    /// Print how long a compaction phase took, when `SK_COMPACT_TIME` is set.
    ///
    /// Compaction is the operation the whole storage direction is judged on, and
    /// "it got slower" is not a finding — which phase got slower is. Reasoning from
    /// file sizes said adjacency was two thirds of the work; it is two thirds of the
    /// *bytes*, which turned out to be a different question.
    fn phase_probe(label: &str, since: &mut std::time::Instant) {
        if std::env::var_os("SK_COMPACT_TIME").is_none() { return; }
        eprintln!("    {:>8.1} ms  {label}", since.elapsed().as_secs_f64() * 1e3);
        *since = std::time::Instant::now();
    }

    fn rss_probe(label: &str) {
        if std::env::var_os("SK_COMPACT_RSS").is_none() { return; }
        if let Ok(o) = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()]).output() {
            if let Ok(t) = String::from_utf8(o.stdout) {
                if let Ok(kb) = t.trim().parse::<f64>() {
                    eprintln!("    RSS {:>7.0} MB  {label}", kb / 1024.0);
                }
            }
        }
    }

    fn compact_inner(&mut self) -> io::Result<()> {
        // No data directory ⇒ ephemeral in-memory DB ⇒ nothing on disk to compact.
        let dir = match self.data_dir.clone() {
            Some(d) => d,
            None => return Ok(()),
        };

        // Paged mode: the store is `immutable base + RAM overlay`. Everything below
        // must therefore read *both*, and a phase that consults `self.nodes` alone
        // sees only the writes since the last compaction — a fraction of the store,
        // reported as the whole of it. That mistake has been made here more than
        // once; `all_hashes()` is the enumeration that includes the base and drops
        // tombstoned nodes, which is where a delete against the immutable base
        // finally takes effect.
        Self::rss_probe("compact start");
        let mut phase = std::time::Instant::now();
        // The base is NOT copied into RAM. Every phase below reads it in place —
        // payload locations through payload_loc, records and slugs straight out of
        // the mmap, adjacency through fwd_edges — and the new base replaces it at
        // the end. Copying the store into memory in order to compact it is exactly
        // what Law 1 forbids, and it was the single largest allocation here.
        let had_base = !self.segments.is_empty();
        // Whether the overlay's contents are durable somewhere else by the end of
        // this compaction, and can therefore be dropped from RAM.
        //
        // This used to be `had_base` alone, which is the same question only while
        // a mapped segment is the sole durable store. With paged nodes it is not:
        // a fresh paged database has no segment at all, so `had_base` was false,
        // the overlay was never cleared, and every compaction folded every node
        // written since the process started — 456 ms to make 200 writes durable on
        // a 50 000-row store, growing without bound in both time and RAM.
        // …and paged topology, for the same reason: the files this compaction is
        // about to write become the base, so what the overlay held is durable in
        // them. Without it the first compaction of a fresh paged-topology database
        // left every node resident.
        let overlay_becomes_durable =
            had_base || self.paged_nodes.is_some() || self.paged_topology;

        // 1. Compact payload store: rebuild from live nodes only.
        // Must happen BEFORE build_snapshot() so the snapshot records the
        // new (post-compaction) offsets, not the pre-compaction ones.
        // Memory DB: rebuild Vec<u8> in-place.
        // Disk DB: streaming rewrite to payloads.bin.tmp then atomic rename.
        // Neither approach loads all payloads into RAM simultaneously.
        // Base-aware: every live record, not just the ones in the write overlay.
        // Only the payload rewrite needs this, and enumerating the whole store
        // costs time proportional to the store — 28 ms per compaction at 500 000
        // rows, spent building a list nothing then reads when payloads are paged.
        let node_keys: Vec<u64> = if self.payload_store.absolute_offsets() {
            self.all_hashes()
        } else {
            Vec::new()
        };
        self.compact_payload_moves.clear();

        // A paged payload store has nothing for this phase to do. Space belonging
        // to updated and deleted records was returned to its free list as they
        // died, so there are no holes to squeeze out — and record ids are not byte
        // positions, so rewriting the file would invalidate every one of them.
        //
        // Skipping it is the point rather than an exception: rewriting payloads is
        // the largest part of compaction, and continuous reclamation is what makes
        // it unnecessary.
        let paged_payloads = !self.payload_store.absolute_offsets();
        if paged_payloads {
            self.payload_store.sync_pages()?;
        } else if self.payload_store.is_disk() {
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
                        let (off, len) = match self.payload_loc(h) {
                            Some(loc) => loc,
                            None => continue,
                        };
                        if let Some(bytes) = self.payload_store.get_raw_of(h, off, len) {
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
            // Apply the new offsets now that tmp_file is closed. Overlay records
            // are updated in place; every record's new location is also published
            // so the topology writer can find it without the node being resident.
            for &(h, new_off, new_len) in &node_new_offsets {
                if let Some(node) = self.nodes.get_mut(&h) {
                    node.payload_offset = new_off;
                    node.payload_len    = new_len;
                }
                self.compact_payload_moves.insert(h, (new_off, new_len));
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
            // The crux of crash-safety: `rename` on the same filesystem is
            // ATOMIC — the OS switches the name over in one indivisible step. So
            // `payloads.bin` is either entirely the old file or entirely the new
            // one, never a half-written mix, no matter when a crash happens.
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
                let Some((old_off, old_len)) = self.payload_loc(h) else { continue };
                if let Some(bytes) = self.payload_store.get_raw_of(h, old_off, old_len) {
                    let new_off = new_slab.len() as u64;
                    new_slab.extend_from_slice(&bytes);
                    if let Some(n) = self.nodes.get_mut(&h) {
                        n.payload_offset = new_off;
                        n.payload_len    = old_len;
                    }
                    self.compact_payload_moves.insert(h, (new_off, old_len));
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
        Self::phase_probe("rewriting payloads + indexes", &mut phase);
        // What this compaction must not lose.
        //
        // This must count the same population `write_topology_files` writes: base
        // plus overlay, minus tombstones. It used to count `self.nodes`, which was
        // right only while compaction began by hydrating the base into the overlay.
        // Once hydration was removed — Law 1 forbids pulling the store into RAM to
        // compact it — `self.nodes` became the writes since the last compaction, a
        // fraction of the store, so the readback below was comparing the new
        // generation against a number it could not fail to beat. The outer rail in
        // `compact()` still caught real loss, but only after the log had been
        // truncated and the old generation dropped; this is the check that can still
        // put everything back.
        let (expect_nodes, expect_edges) = self.compaction_expectation();
        Self::phase_probe("counting what must survive", &mut phase);

        // Keep the outgoing generation reachable until the incoming one has been
        // read back and found complete.
        Self::rss_probe("after payloads + indexes");
        let staged = Self::stage_previous_generation(&dir);

        // Make every edge written since the last fold durable, in place. This is
        // what stands in for rebuilding adjacency, and it happens *before* the
        // topology is written so the readback below sees a settled graph.
        if let Err(e) = self.fold_edges_into_paged() {
            Self::restore_previous_generation(&staged);
            return Err(e);
        }
        Self::phase_probe("folding edges into pages", &mut phase);
        if let Err(e) = self.fold_nodes_into_paged() {
            Self::restore_previous_generation(&staged);
            return Err(e);
        }
        Self::phase_probe("folding nodes into pages", &mut phase);
        // The names the paged graph cannot carry. Written whenever it is on, before
        // the topology files, so a crash between the two leaves names for a
        // generation that still exists rather than for one that does not.
        if self.paged_adj.is_some() {
            let types = self.edges.type_table();
            if let Err(e) = edge_type_names::write(&dir.join(edge_type_names::FILE), &types) {
                Self::restore_previous_generation(&staged);
                return Err(e);
            }
        }
        if self.paged_nodes.is_some() {
            // Collections are keyed by hash and named only inside each node record,
            // so this is what stops `SHOW TABLES` from being a full scan.
            let names: Vec<(u64, &str)> = self.collection_names_map.iter()
                .map(|(&h, n)| (h, n.as_str()))
                .collect();
            if let Err(e) = edge_type_names::write(
                &dir.join(edge_type_names::COLL_FILE), &names)
            {
                Self::restore_previous_generation(&staged);
                return Err(e);
            }
        }

        self.merge_index_deltas();
        Self::phase_probe("merging index deltas", &mut phase);
        if let Err(e) = self.write_topology_files(&dir) {
            Self::restore_previous_generation(&staged);
            return Err(e);
        }
        Self::phase_probe("writing topology files", &mut phase);

        // Read the new files back, from disk, with a reader that shares nothing with
        // the writer. Only if they hold everything do we go on to drop the old
        // generation and truncate the log.
        //
        // The paged graph is not in those files, so it is counted here and passed
        // in. It is counted by walking it, not by trusting a header — the point of
        // the rail is to read back what was actually written.
        // Read off the store's running total, not by walking it. Walking made this
        // rail O(edges) and more than doubled a 200 000-node compaction — the exact
        // cost this direction removes, put back by the check that guards it.
        let paged_edges = self.paged_adj.as_ref().map(|pa| pa.fwd.edge_count() as usize);
        let paged_node_count = self.paged_nodes.as_ref().map(|ns| ns.len() as usize);
        match Self::count_generation_on_disk(&dir, paged_node_count, paged_edges) {
            Ok((n, e)) if n >= expect_nodes && e >= expect_edges => {}
            Ok((n, e)) => {
                Self::restore_previous_generation(&staged);
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "sekejap: compaction wrote an incomplete store \
                         ({n} nodes / {e} edges on disk, expected {expect_nodes} / \
                         {expect_edges}). The previous files have been put back and \
                         the write-ahead log is untouched, so nothing is lost. This \
                         is a bug; please report it with the database layout."
                    ),
                ));
            }
            Err(err) => {
                Self::restore_previous_generation(&staged);
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "sekejap: could not read back the store compaction just wrote \
                         ({err}). The previous files have been put back and the \
                         write-ahead log is untouched, so nothing is lost."
                    ),
                ));
            }
        }

        Self::phase_probe("reading the new generation back", &mut phase);

        // Persist btree field indexes as mmap'able sidecars so a reopened paged DB
        // serves indexed queries from page cache (not heap). One file per
        // (collection, field); the field name is hex-encoded so any identifier
        // round-trips through the filename.
        for ((coll_hash, field), btree) in &self.field_indexes {
            let fname = format!("fieldidx_{}_{}.bin", coll_hash, hex_encode(field));
            storage::fieldstore::write(&dir.join(fname), btree)?;
        }

        Self::phase_probe("field index sidecars", &mut phase);

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
        // The commit point of the whole compaction. Every base file has been
        // written and renamed by now, and the snapshot that names them has just
        // landed; this is the moment all of it becomes durable together.
        //
        // It has to happen *before* the log is rotated below. The log is the only
        // record of the writes that the new base is supposed to contain, so
        // throwing it away while the base is still in the page cache trades a
        // recoverable state for an unrecoverable one. The completeness rail that
        // runs above reads the new files back and finds them correct — from the
        // page cache, which is exactly what a power cut discards.
        fsync_dir(&dir)?;

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
        // New file, new inode, record count restarting at zero: anyone
        // coordinating fsyncs on our behalf has to be told.
        self.wal_generation += 1;
        // Both renames — the old log out of the way and the new one into place —
        // before the old log is destroyed. Otherwise a crash can leave no
        // `wal.log` at all: the rename that created it undone, and the file it
        // replaced already deleted.
        fsync_dir(&dir)?;
        if wal_old.exists() {
            std::fs::remove_file(&wal_old)?;
        }
        // The new generation has been read back and is complete, and the log has
        // been rotated. Only here does the old generation stop being needed.
        Self::discard_previous_generation(&staged);

        // Adopt the generation just written. Everything the overlay held is in it
        // now, so the overlay empties — which is what returns RAM to where it was
        // before the compaction rather than leaving the whole store resident.
        // Adopting the base and emptying the overlay are one decision: map without
        // clearing and every node is in both, counted twice by anything that reads
        // the merge.
        if overlay_becomes_durable && self.paged_topology {
            let nb = storage::topology::MappedTopology::open(&dir)?;
            for (h, name) in nb.edge_type_table() {
                self.edges.register_type_name(h, name);
            }
            self.segments.replace_with(std::sync::Arc::new(nb));
            let slots = storage::slotmap::MappedSlots::open(&dir.join("slots.bin"))
                .ok()
                .flatten()
                .map(std::sync::Arc::new);
            self.segments.set_slots(slots);
        }
        if overlay_becomes_durable {
            // Everything the overlay held is in the durable store now, so it goes —
            // which is what returns RAM to where it was before the compaction
            // rather than leaving the whole database resident.
            self.nodes.clear();
            self.collections.clear();
            // Kept when nodes are paged: this map is the in-RAM side of
            // coll_names.bin, and clearing it would send `SHOW TABLES` back to
            // reading every node record to learn the names again.
            if self.paged_nodes.is_none() { self.collection_names_map.clear(); }
            self.slug_map.clear();
            self.tombstones.clear();
            self.edge_tombstones.clear();
            self.edges.reset_adjacency();
        }
        // Unconditionally: the generation just written contains none of the edges
        // these record, whether or not there was a base before it. Leaving them
        // would subtract the same edges from a base that never had them.
        self.unlinked_edges.clear();
        self.renamed_collections.clear();
        self.compact_payload_moves.clear();
        Self::phase_probe("snapshot + adopting the generation", &mut phase);

        // Regenerate gin.bin so the next open loads GIN instantly.
        if let Some(ref gin_bin_path) = self.data_dir.as_ref().map(|d| d.join("gin.bin")) {
            let _ = self.save_gin_binary(gin_bin_path);
        }
        // Regenerate search.bin so the next open loads search indexes instantly.
        if let Some(ref search_bin_path) = self.data_dir.as_ref().map(|d| d.join("search.bin")) {
            let _ = self.save_search_binary(search_bin_path);
        }
        Self::phase_probe("gin.bin + search.bin", &mut phase);

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
    /// atomically. Read back by `open()` for snapshot-missing recovery and by the
    /// paged-topology mode — see `docs/developer/notes/topology-format.md`.
    /// The files that together are the compacted base — one generation of it.
    const BASE_FILES: [&'static str; 9] = [
        "nodes.bin", "idx.bin", "slugs.bin", "collections.bin",
        "adj_fwd.bin", "adj_rev.bin", "edgemeta.bin", "dict.bin", "spatial.bin",
    ];

    /// Park a copy of the current generation before it is overwritten.
    ///
    /// Hard links, so this costs one inode entry per file and no data copy — the
    /// old bytes stay reachable under `<name>.prev` even after the rename replaces
    /// `<name>`. Nothing is duplicated and nothing is read.
    ///
    /// This works because every base file is replaced by writing a new file and
    /// renaming it over the old name: the rename swings the directory entry to a
    /// new inode while this link still holds the old one. A base file written *in
    /// place* would destroy the parked copy along with the live one, so that must
    /// never happen — see `write_atomic`, which is the only sanctioned way to
    /// replace one.
    fn stage_previous_generation(dir: &Path) -> Vec<(std::path::PathBuf, std::path::PathBuf)> {
        let mut staged = Vec::new();
        for name in Self::BASE_FILES {
            let live = dir.join(name);
            if !live.exists() {
                continue;
            }
            let prev = dir.join(format!("{name}.prev"));
            let _ = std::fs::remove_file(&prev);
            if std::fs::hard_link(&live, &prev).is_ok() {
                staged.push((live, prev));
            }
        }
        staged
    }

    /// Put the previous generation back, undoing a compaction that did not verify.
    fn restore_previous_generation(staged: &[(std::path::PathBuf, std::path::PathBuf)]) {
        for (live, prev) in staged {
            let _ = std::fs::rename(prev, live);
        }
    }

    fn discard_previous_generation(staged: &[(std::path::PathBuf, std::path::PathBuf)]) {
        for (_, prev) in staged {
            let _ = std::fs::remove_file(prev);
        }
    }

    /// Read the generation just written back off disk and count what is in it.
    ///
    /// This is the whole point of the exercise, so it deliberately shares nothing
    /// with the code that produced the files: a fresh `MappedTopology` opened from
    /// the directory, walked directly. The previous guard rail counted through the
    /// same accessors that had written the data, so when those accessors were wrong
    /// it agreed with them and confirmed a compaction that had just deleted the
    /// entire graph. A check that can agree with the bug is not a check.
    fn count_generation_on_disk(
        dir: &Path,
        paged_nodes: Option<usize>,
        paged_edges: Option<usize>,
    ) -> io::Result<(usize, usize)> {
        let base = storage::topology::MappedTopology::open(dir)?;
        // Both counts come straight off the mapped headers and offset tables —
        // no edges are decoded and nothing is allocated per node, so this stays
        // cheap enough to run on every compaction.
        //
        // With paged adjacency the CSR files are deliberately empty, so their edge
        // count would be zero and the rail would report catastrophic loss on every
        // compaction. The caller counts the paged graph instead and passes it in.
        // Whichever half is paged is not in these files — deliberately, since not
        // rewriting it is the point — so its count comes from the store that does
        // hold it. Reading a zero out of a file nobody writes any more and calling
        // it data loss is how this rail cried wolf on every compaction.
        Ok((
            paged_nodes.unwrap_or_else(|| base.node_count()),
            paged_edges.unwrap_or_else(|| base.edge_count()),
        ))
    }

    fn write_topology_files(&self, dir: &Path) -> io::Result<()> {
        use storage::topology::{self, TopoEdge, TopoNode};

        // This reads the base AND the overlay, so it may run with a base still
        // mapped — that is now the point. It used to rewrite from the resident map
        // alone, which in paged mode is only the overlay, and callers had to copy
        // the entire base into RAM first so it would not be destroyed. Copying the
        // store into memory to compact it is what Law 1 forbids; the slugs and
        // collection names below are borrowed straight out of the mmap instead.
        let segments = self.segments.clone();

        // With paged nodes the store is already durable and was never taken apart,
        // so there is nothing to write. The files still exist and still hold the
        // generation they were written with — they are simply no longer where the
        // nodes are, which is what `base_node` and its siblings decide.
        let rebuild_nodes = self.paged_nodes.is_none();

        // Nodes — overlay first, then base entries the overlay does not shadow.
        let mut topo_nodes: Vec<TopoNode> = Vec::with_capacity(self.nodes.len());
        if rebuild_nodes {
        for (&h, n) in &self.nodes {
            if self.tombstones.contains(&h) {
                continue;
            }
            topo_nodes.push(TopoNode {
                hash: h,
                slug: n.slug.as_str(),
                collection: n.collection.as_str(),
                payload_offset: n.payload_offset,
                payload_len: n.payload_len,
                spatial: n.spatial_meta.as_ref().map(|m| Box::new([
                    m.centroid_lat, m.centroid_lon,
                    m.bbox_min_lat, m.bbox_min_lon,
                    m.bbox_max_lat, m.bbox_max_lon,
                ])),
            });
        }
        if let Some(b) = segments.newest_first().next() {
            for id in 0..b.node_count() as u64 {
                let Some(h) = b.hash_of(id) else { continue };
                if self.tombstones.contains(&h) || self.nodes.contains_key(&h) {
                    continue; // deleted, or the overlay holds a newer version
                }
                let Some(rec) = b.node_record(id) else { continue };
                // The payload rewrite has already moved this record; the mapped
                // base still holds its OLD location.
                let (off, len) = self.compact_payload_moves.get(&h).copied()
                    .unwrap_or((rec.payload_offset, rec.payload_len));
                topo_nodes.push(TopoNode {
                    hash: h,
                    slug: b.slug_of(id).unwrap_or(""),
                    collection: b.collection_name(rec.collection_id).unwrap_or(""),
                    payload_offset: off,
                    payload_len: len,
                    spatial: b.spatial(id).map(Box::new),
                });
            }
        }
        }
        topo_nodes.sort_by_key(|n| n.hash);

        // Edges — forward adjacency over the same merged node set; skip dangling.
        // `topo_nodes` is already sorted by hash, so it doubles as the liveness set
        // via binary search. A HashSet of every live hash cost tens of megabytes on
        // a large store for information already sitting in the vector.
        let live = |h: u64| topo_nodes.binary_search_by_key(&h, |n| n.hash).is_ok();
        let mut topo_edges: Vec<TopoEdge> = Vec::new();
        // With paged adjacency the graph is already durable and was never taken
        // apart, so there is nothing to write here. Writing it anyway would not be
        // wrong — reads prefer the paged store — but it is the exact work this
        // direction exists to stop doing: two thirds of a compaction's bytes.
        let rebuild_edges = self.paged_adj.is_none();
        for i in 0..topo_nodes.len() {
            if !rebuild_edges { break }
            let from_h = topo_nodes[i].hash;
            let Some(edge_list) = self.fwd_edges(from_h) else { continue };
            for e in edge_list.iter() {
                if !live(e.other) {
                    continue;
                }
                let edge_type = self
                    .edges
                    .type_name(e.edge_type)
                    // Borrowed when the type has a registered name, which is the
                    // normal case — cloning produced one String per edge, all
                    // duplicates of a handful of distinct names.
                    .map(std::borrow::Cow::Borrowed)
                    .unwrap_or_else(|| std::borrow::Cow::Owned(format!("{:016x}", e.edge_type)));
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

        Self::rss_probe("topo vecs built");
        let mut inner = std::time::Instant::now();
        // Each file is written and released as it is produced, so peak memory is
        // the largest single file rather than the sum of all nine.
        topology::build_into(&topo_nodes, &topo_edges, |name, bytes| {
            Self::write_atomic(dir, name, bytes)
        })?;
        // The slot table for the generation just written. One segment, so it is the
        // identity mapping — slot n lives at local id n of segment 0. It is written
        // now so that the moment a second segment exists there is a table to extend
        // rather than a meaning to change.
        let slots: Vec<u64> = (0..topo_nodes.len() as u64)
            .map(|id| storage::slotmap::pack(0, id))
            .collect();
        Self::write_atomic(dir, "slots.bin", &storage::slotmap::write(&slots))?;
        drop(slots);
        drop(topo_edges);
        drop(topo_nodes);
        Self::rss_probe("topology written");
        // Spatial grid (cell index + per-node meta) sidecar — lets a paged reopen
        // serve the grid straight from the mmap instead of rebuilding it resident.
        // Built fresh from all node metas (overlay + base) so it is complete even
        // when compacting in paged mode; ring caches are not persisted.
        Self::phase_probe("  topology files + slot table", &mut inner);
        if self.spatial_grid.is_some() {
            let grid = geo::SpatialGrid::build(self.all_spatial_items().into_iter());
            let mut buf = Vec::new();
            grid.write_binary(&mut buf)?;
            Self::write_atomic(dir, "spatialgrid.bin", &buf)?;
        }
        // Compact vector indexes (int8 + CSR) sidecar — lets a paged reopen mmap them
        // instead of rebuilding the HNSW graph resident.
        Self::phase_probe("  spatial grid", &mut inner);
        self.save_vector_binary(&dir.join("vecidx.bin"))?;
        // BM25 metadata sidecar (dict + doc arrays) for disk-first paged reopen.
        self.save_bm25_binary(&dir.join("bm25.bin"))?;
        Self::phase_probe("  vector + bm25 sidecars", &mut inner);
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
    /// Used by local open (snapshot + recovery).
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
                        .get_raw_of(hash, rec.payload_offset, rec.payload_len)
                        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                        .and_then(|p| geo::extract_spatial_meta(&p))
                });
            self.nodes.insert(hash, NodeData {
                slug: slug.to_string(),
                collection: coll.clone(),
                spatial_meta: spatial_meta.map(Box::new),
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
        // The rename is the commit, and until the *directory* is synced the
        // rename itself is not durable — only the bytes inside the temp file
        // are. A crash here could restore the old name over the new contents on
        // one file and not another, leaving files from two generations
        // pointing at each other. See `fsync_dir`.
        fsync_dir(dir)?;
        Ok(())
    }

    /// Save all current GIN indexes to a compact binary sidecar `gin.bin`.
    ///
    /// The file format uses RoaringBitmap's native binary serialization, which
    /// is ~10-50× smaller and faster to load than JSON integer arrays.
    /// Called automatically after GIN is rebuilt so future opens skip the rebuild.
    /// Persist the compact (int8 + CSR) vector indexes to a `SKVEC001` container so
    /// a paged reopen can mmap them instead of rebuilding the HNSW graph resident.
    /// Skipped when indexes are already mmap-backed (the file is authoritative).
    fn save_vector_binary(&self, path: &Path) -> io::Result<()> {
        use std::io::Write;
        if self.compact_indexes.is_empty() { return Ok(()); }
        if self.compact_indexes.values().any(|c| c.is_disk_backed()) { return Ok(()); }
        let tmp = path.with_extension("bin.tmp");
        let mut f = std::io::BufWriter::new(
            std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?
        );
        f.write_all(b"SKVEC001")?;
        f.write_all(&(self.compact_indexes.len() as u32).to_le_bytes())?;
        for (field, ci) in &self.compact_indexes {
            let fb = field.as_bytes();
            f.write_all(&(fb.len() as u16).to_le_bytes())?;
            f.write_all(fb)?;
            ci.write_binary(&mut f)?;
        }
        f.flush()?;
        f.get_ref().sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Disk-first (paged): mmap the vecidx.bin container and serve each field's
    /// compact vector index (int8 codes + CSR graph) from the map. f32 re-rank
    /// still reads the mmap'd f32 store. Returns false on any problem.
    fn load_vector_base(&mut self, path: &Path) -> bool {
        use std::sync::Arc;
        let file = match std::fs::File::open(path) { Ok(f) => f, Err(_) => return false };
        let len = match file.metadata() { Ok(m) => m.len() as usize, Err(_) => return false };
        if len < 12 { return false; }
        let view = match storage::mmap::MmapView::try_new(&file, len) {
            Some(v) => Arc::new(v),
            None => return false,
        };
        let hdr = match view.slice(0, 12) { Some(h) => h, None => return false };
        if &hdr[..8] != b"SKVEC001" { return false; }
        let count = u32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]) as usize;
        let mut pos = 12usize;
        let mut loaded = Vec::with_capacity(count);
        for _ in 0..count {
            let kl = match view.slice(pos, 2) { Some(b) => u16::from_le_bytes([b[0], b[1]]) as usize, None => return false };
            pos += 2;
            let field = match view.slice(pos, kl).and_then(|b| std::str::from_utf8(b).ok()) {
                Some(s) => s.to_string(),
                None => return false,
            };
            pos += kl;
            match vector::CompactDiskIndex::open_mapped(&view, pos) {
                Ok((ci, consumed)) => { pos += consumed; loaded.push((field, ci)); }
                Err(_) => return false,
            }
        }
        for (field, ci) in loaded { self.compact_indexes.insert(field, ci); }
        true
    }

    /// Persist BM25 metadata (dict + doc arrays) to bm25.bin so a paged reopen can
    /// mmap it instead of rebuilding. Postings stay in the `bm25_<field>.postings`
    /// files. Skipped when any index is already mmap-backed (the file is authoritative).
    fn save_bm25_binary(&self, path: &Path) -> io::Result<()> {
        use std::io::Write;
        if self.bm25_indexes.is_empty() { return Ok(()); }
        if self.bm25_indexes.values().any(|ix| ix.is_disk_backed()) { return Ok(()); }
        let tmp = path.with_extension("bin.tmp");
        let mut f = std::io::BufWriter::new(
            std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?
        );
        f.write_all(b"SKBM2501")?;
        f.write_all(&(self.bm25_indexes.len() as u32).to_le_bytes())?;
        for ix in self.bm25_indexes.values() {
            ix.write_binary(&mut f)?;
        }
        f.flush()?;
        f.get_ref().sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Disk-first (paged): mmap bm25.bin and serve each field's BM25 index — doc
    /// arrays off the map, dict loaded resident (the accelerator), postings `pread`
    /// from their spilled files. Returns false on any problem → caller rebuilds.
    fn load_bm25_base(&mut self, path: &Path, dir: &Path) -> bool {
        use std::sync::Arc;
        let file = match std::fs::File::open(path) { Ok(f) => f, Err(_) => return false };
        let len = match file.metadata() { Ok(m) => m.len() as usize, Err(_) => return false };
        if len < 12 { return false; }
        let view = match storage::mmap::MmapView::try_new(&file, len) {
            Some(v) => Arc::new(v),
            None => return false,
        };
        let hdr = match view.slice(0, 12) { Some(h) => h, None => return false };
        if &hdr[..8] != b"SKBM2501" { return false; }
        let count = u32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]) as usize;
        let mut pos = 12usize;
        let mut loaded = Vec::with_capacity(count);
        for _ in 0..count {
            match bm25::Bm25Index::open_mapped(&view, pos, dir) {
                Ok((ix, consumed)) => { pos += consumed; loaded.push(ix); }
                Err(_) => return false,
            }
        }
        for ix in loaded {
            self.bm25_indexes.insert(ix.field_name().to_string(), ix);
        }
        true
    }

    fn save_gin_binary(&self, path: &Path) -> io::Result<()> {
        use std::io::Write;
        // If any index is served from the mmap (paged, disk-first), the on-disk
        // gin.bin is already authoritative and self-contained — don't overwrite it
        // with the empty resident maps. Rewritten only when indexes are resident.
        if self.gin_indexes.values().any(|g| g.is_disk_backed()) {
            return Ok(());
        }
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
    /// Disk-first (paged): mmap the gin.bin container and serve each field's GIN
    /// postings + id map from the map (`MappedGin`), leaving nothing resident.
    /// Mirrors `load_search_base`. Returns false on any problem → caller rebuilds.
    fn load_gin_base(&mut self, path: &Path) -> bool {
        use std::sync::Arc;
        let file = match std::fs::File::open(path) { Ok(f) => f, Err(_) => return false };
        let len = match file.metadata() { Ok(m) => m.len() as usize, Err(_) => return false };
        if len < 12 { return false; }
        let view = match storage::mmap::MmapView::try_new(&file, len) {
            Some(v) => Arc::new(v),
            None => return false,
        };
        let hdr = match view.slice(0, 12) { Some(h) => h, None => return false };
        if &hdr[..8] != b"SKGIN001" { return false; }
        let count = u32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]) as usize;
        let mut pos = 12usize;
        let mut loaded = Vec::with_capacity(count);
        for _ in 0..count {
            match storage::ginstore::MappedGin::open_mapped(&view, pos, GIN_INDEX_VERSION) {
                Ok((mg, consumed)) => { pos += consumed; loaded.push(mg); }
                Err(_) => return false,
            }
        }
        for mg in loaded {
            let field = mg.field().to_string();
            self.gin_indexes.insert(field, GINIndex::from_mapped(mg));
        }
        true
    }

    fn load_gin_binary(&mut self, path: &Path) -> bool {
        // Paged (disk-first): mmap the container instead of reading it into heap.
        if !self.segments.is_empty() && self.load_gin_base(path) {
            return true;
        }
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
        // INVARIANT GUARD — see write_topology_files. snapshot.json records the node
        // set; building it from the overlay while a base is mapped would persist a
        // truncated store.
        debug_assert!(
            self.segments.is_empty(),
            "build_snapshot() called with a paged base still mapped — would persist \
             only the overlay and lose base nodes"
        );

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
                let to_slug = match self.node_data(e.other) {
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
                    metric: self.hnsw_metric(field),
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
                        spatial_meta:   n.spatial_meta.map(Box::new),
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
                    let field = sv.field.clone();
                    if let Err(e) = self.vectors.entry(sv.field).or_default()
                        .put(hash, sv.data)
                    {
                        self.note_write_error(format!("vector write failed on {field}: {e}"));
                    }
                }
            }
        }
        // Restore HNSW graphs — rebuild if the stored version doesn't match.
        if let Some(hnsw_list) = snap.hnsw_indexes {
            for sh in hnsw_list {
                if sh.version == HNSW_INDEX_VERSION {
                    self.hnsw_params.insert(sh.field.clone(), (sh.m, sh.ef_construction));
                    self.hnsw_metric.insert(sh.field.clone(), sh.metric);
                    self.hnsw_indexes.insert(sh.field, sh.graph);
                } else {
                    // Version mismatch — rebuild from stored vectors with same metric.
                    let _ = self.build_hnsw_index_metric(&sh.field, sh.m, sh.ef_construction, sh.metric);
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
    pub fn slug_of(&self, hash: u64) -> Option<&str> {
        self.nodes.get(&hash).map(|n| n.slug.as_str())
    }

    /// Fetch one node's raw JSON payload by slug, or `None` if it doesn't exist.
    ///
    /// This is the disk-read counterpart to [`put`](Self::put): the in-RAM
    /// `NodeData` only holds *where* the bytes are, so this looks up the
    /// offset/length ([`payload_loc`](Self::payload_loc)) and then reads those
    /// bytes from the payload store (a zero-copy mmap slice on disk, or the RAM
    /// buffer for an ephemeral DB).
    pub fn get(&self, slug: &str) -> Option<String> {
        let hash = sk_hash(slug);
        let (off, len) = self.payload_loc(hash)?; // metadata → disk location
        self.payload_store
            // Checked against the row it should be. This is the plainest read in
            // the database and it was reading anonymously: a damaged slot
            // directory in a paged store made it return a different row's bytes,
            // correctly formed, under the slug that was asked for.
            .get_raw_of(hash, off, len) // the only disk touch
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    /// Where a node's payload lives in the payload store — overlay first, then
    /// the mapped base. Lean (no string materialization): this sits on the
    /// payload-read hot path.
    /// Hand the log's fsync duty to the caller.
    ///
    /// Returns a second descriptor onto the log and the strength to sync it at,
    /// or `None` when there is no log (in-memory) or durability is not `Full`, in
    /// which case there is nothing to coordinate. From here on this database
    /// appends without fsyncing; the caller must fsync that descriptor before
    /// reporting any write committed.
    pub(crate) fn take_over_wal_sync(&mut self) -> Option<(std::fs::File, storage::wal::SyncLevel)> {
        if self.wal_sync != SyncMode::Full {
            return None;
        }
        let wal = self.wal.as_ref()?;
        let file = wal.try_clone_file().ok()?;
        let level = wal.sync_level();
        self.group_commit = true;
        Some((file, level))
    }

    /// Records appended to the log so far — the handle a caller waits on to learn
    /// that its own write has been made durable.
    /// Where the log stands: which generation of the file, and how many records
    /// have been appended to it.
    ///
    /// A caller waits on this pair. The generation must be compared first — a
    /// count is only meaningful within the generation that produced it.
    pub(crate) fn wal_mark(&self) -> (u64, u64) {
        (self.wal_generation, self.wal.as_ref().map_or(0, |w| w.seq()))
    }

    /// A fresh descriptor onto the current log file.
    pub(crate) fn wal_clone_file(&self) -> Option<std::fs::File> {
        self.wal.as_ref()?.try_clone_file().ok()
    }

    pub(crate) fn payload_loc(&self, hash: u64) -> Option<(u64, u32)> {
        // `is_empty` first: with no pending base deletes this is a length check,
        // not a hash lookup, so the hot read path pays essentially nothing.
        if !self.tombstones.is_empty() && self.tombstones.contains(&hash) {
            return None;
        }
        // Mid-compaction the payloads have already been rewritten, so the mapped
        // base and the overlay both hold pre-rewrite locations. The moves table is
        // authoritative until the new base is adopted. Empty at every other moment,
        // so the hot path pays a length check.
        if !self.compact_payload_moves.is_empty() {
            if let Some(&loc) = self.compact_payload_moves.get(&hash) {
                return Some(loc);
            }
        }
        if let Some(n) = self.nodes.get(&hash) {
            return Some((n.payload_offset, n.payload_len));
        }
        self.base_payload_loc(hash)
    }

    /// Take a cheap, point-in-time **snapshot** for lock-free reads — the
    /// "photograph" primitive behind reads-that-never-block-writes (see
    /// `docs/developer/notes/snapshot-reads-design.md`).
    ///
    /// # What you get
    ///
    /// A **read-only `CoreDB`** frozen at this instant — the snapshot that indexed
    /// SQL runs against.
    ///
    /// Reads that need no index would only require the base plus the frozen
    /// overlay; this gives back a whole `CoreDB` instead,
    /// so the *existing* query executor runs against it unchanged — `WHERE`, `MATCH`,
    /// `VECTOR_NEAR`, `BM25`, `ST_*` all work, and none of them touch the live
    /// database or its write lock.
    ///
    /// # What it costs
    ///
    /// - The immutable base is **shared** (`Arc` bump): the mmap'd topology, and the
    ///   mmap-backed field indexes. No bytes are copied.
    /// - The write overlay and the resident index structures are **cloned**. That is
    ///   the real cost, and it grows with how much has been written since the last
    ///   `compact()` — so compaction frequency is the tuning knob, exactly as for
    ///   the overlay.
    ///
    /// # Why it is safe to hand around
    ///
    /// It returns `Arc<CoreDB>`. Every mutating method on `CoreDB` takes `&mut self`,
    /// and a shared `Arc` never yields `&mut`, so the borrow checker — not a runtime
    /// flag — guarantees a snapshot can only be read. On top of that the copy is
    /// defused: no WAL writer, no file lock, no `data_dir`, auto-compaction off and
    /// `compact_on_close` false (its `Drop` must never compact the real database).
    ///
    /// Returns `None` unless this is a paged-mode store (unix only), same as
    /// [`snapshot`](Self::snapshot).
    /// # Not available with paged nodes or paged adjacency
    ///
    /// Returns `None` for those configurations, and the reason is structural
    /// rather than missing work. A snapshot is a photograph: it shares the durable
    /// base by `Arc` and freezes a copy of the overlay, and that is sound precisely
    /// because the base is *immutable* — a compaction writes a new generation and
    /// swaps it in, so an existing snapshot keeps reading the old one.
    ///
    /// Paged stores are mutated **in place**. A snapshot sharing them would see the
    /// writer's later edits appear underneath it, which is not a stale photograph
    /// but an inconsistent one — rows from two different moments in the same read.
    /// Giving them their own file handles would not help either: the same pages
    /// change on disk.
    ///
    /// The fix is page-level copy-on-write, so a write allocates a new page and
    /// leaves the old one for readers holding the previous root. That is a real
    /// piece of design, not an oversight, and until it exists this returns `None`
    /// so callers fall back to locked reads — correct and slower, rather than fast
    /// and wrong. `paged_mode_without_snapshots_says_so` pins it.
    #[cfg(unix)]
    pub fn snapshot_db(&self) -> Option<std::sync::Arc<CoreDB>> {
        let t0 = std::time::Instant::now();
        // Snapshots require the compacted store to be mapped: a resident database
        // has no immutable half to share, so there is nothing to snapshot. The
        // `?` here is load-bearing — dropping it makes snapshot_db succeed in
        // embedded mode, which two tests correctly object to.
        if self.segments.is_empty() {
            return None;
        }
        let segments = self.segments.clone(); // shared immutable segments
        let payload_store = self.payload_store.read_only_clone()?; // own fd, shared mmap

        let snap = std::sync::Arc::new(CoreDB {
            // ── frozen write overlay ────────────────────────────────────────
            counters: Counters::default(), // a snapshot starts its own tally
            tombstones: self.tombstones.clone(),
            nodes: self.nodes.clone(),
            // A snapshot cannot share the paged adjacency: its stores hold their
            // own file handles and a write cursor, and a snapshot must not be able
            // to write. Falling back to the segments is correct rather than
            // convenient, and a store using paged adjacency simply has no segment
            // edges to find - which is why snapshot_reads is refused for it in
            // `open_with_config` rather than quietly answering an empty graph.
            paged_adj: None,
            paged_topology: false,
            read_only: false,
            write_error: None,
            paged_nodes: None,
            slug_map: self.slug_map.clone(),
            collections: self.collections.clone(),
            collection_names_map: self.collection_names_map.clone(),
            edges: self.edges.clone(),

            // ── shared immutable base (no bytes copied) ─────────────────────
            segments,
            field_base: self.field_base.clone(), // MappedFieldStore shares its mmap
            payload_store,

            // ── index overlays the executor needs ───────────────────────────
            spatial_grid: self.spatial_grid.clone(),
            text_indexes: self.text_indexes.clone(),
            gin_indexes: self.gin_indexes.clone(),
            bm25_indexes: self.bm25_indexes.clone(),
            search_indexes: self.search_indexes.clone(),
            vectors: self.vectors.clone(),
            hnsw_indexes: self.hnsw_indexes.clone(),
            quant_fields: self.quant_fields.clone(),
            compact_indexes: self.compact_indexes.clone(),
            field_indexes: self.field_indexes.clone(),
            hnsw_params: self.hnsw_params.clone(),
            hnsw_metric: self.hnsw_metric.clone(),
            hnsw_ef_search: self.hnsw_ef_search,

            // ── query-visible metadata ──────────────────────────────────────
            schemas: self.schemas.clone(),
            materialized_views: self.materialized_views.clone(),

            // ── every write path: disarmed ──────────────────────────────────
            wal: None,                       // no WAL writer: cannot log a mutation
            _lock_file: None,                // never touches the parent's lock
            data_dir: None,                  // no path: nothing on disk can be written
            compact_on_close: false,         // Drop must NOT compact the real database
            auto_compact: AutoCompact::Off,  // and must not compact while alive
            pending_change: ChangeEvent::default(),
            change_listeners: Vec::new(),    // never re-emit the parent's events
            next_change_id: 0,
            commit_depth: 0,
            pending_txn: None,
            replaying: false,
            autocompacting: false,
            writes_since_compact_check: 0,
            defer_wal_sync: false,
            edge_tombstones: HashSet::new(),
            unlinked_edges: HashSet::new(),
            renamed_collections: HashSet::new(),
            compact_payload_moves: HashMap::new(),
            group_commit: false,
            wal_generation: 0,
            batch_now: None,
            defer_index_rebuild: false,
            dirty_bm25: HashSet::new(),
            dirty_gin: HashSet::new(),
            dirty_search: HashSet::new(),

            // ── inert configuration (kept so reads behave identically) ──────
            compact_thresholds: self.compact_thresholds.clone(),
            wal_sync: self.wal_sync,
            wal_format: self.wal_format,
            logical_wal: self.logical_wal,
            wal_sync_level: self.wal_sync_level,
        });
        let us = t0.elapsed().as_micros() as u64;
        self.counters.snapshots.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.counters.snapshot_us_last.store(us, std::sync::atomic::Ordering::Relaxed);
        self.counters.snapshot_us_max.fetch_max(us, std::sync::atomic::Ordering::Relaxed);
        Some(snap)
    }

    /// Parse and return the JSON payload for a node hash. Returns `None` if
    /// the node does not exist or the payload cannot be parsed.
    pub(crate) fn get_payload(&self, hash: u64) -> Option<Value> {
        let (off, len) = self.payload_loc(hash)?;
        // Checked against the node it should belong to. Everything above this has
        // already been verified — the node record carries a checksum — but the
        // payload it points at is a separate record in a separate store, and a
        // damaged slot directory there lands this read on another row's bytes.
        self.payload_store.get_of(hash, off, len)
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
            let full = self.payload_store.get_raw_of(hash, off, len as u32)?;
            return Some((full.clone(), full));
        }
        // Reading only the ends is an optimisation that needs byte positions. A
        // paged store addresses by record id, so it reads the record and slices —
        // the same answer, without the saving.
        if !self.payload_store.absolute_offsets() {
            let full = self.payload_store.get_raw_of(hash, off, p_len)?;
            let n = full.len();
            let h = full[..head_size.min(n)].to_vec();
            let t = full[n.saturating_sub(tail_size)..].to_vec();
            return Some((h, t));
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

        // Coalescing neighbouring records into one read needs them to be adjacent
        // at known byte positions. A paged store addresses by record id, where
        // "adjacent" means nothing, so it reads each record individually — the same
        // answers, without the syscall saving.
        if !self.payload_store.absolute_offsets() {
            for &(hash, off, len) in &sorted {
                // `get_raw_of`: the batch knows whose bytes it is asking for, so it
                // says so. Reading anonymously here is what let a damaged slot
                // directory substitute one row for another.
                if let Some(bytes) = self.payload_store.get_raw_of(hash, off, len) {
                    result.insert(hash, bytes);
                }
            }
            return result;
        }

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
                    if let Some(raw) = self.payload_store.get_raw_of(hash, off, len) {
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
        self.node_exists(sk_hash(slug))
    }

    /// Base-aware node existence by hash (resident overlay or mmap base).
    pub(crate) fn node_exists(&self, h: u64) -> bool {
        if !self.tombstones.is_empty() && self.tombstones.contains(&h) {
            return false;
        }
        self.nodes.contains_key(&h) || self.base_contains(h)
    }

    /// Total number of nodes.
    /// What this database is holding, and what it has done since it was opened.
    ///
    /// Sizes are measured now; counters and timings accumulate from open. Cheap —
    /// no index is walked, only lengths and two file sizes are read.
    ///
    /// ```rust,no_run
    /// let db = sekejap::open("./mydb")?;
    /// let s = db.stats();
    /// println!("{} nodes, {} in the write overlay", s.nodes, s.overlay_nodes);
    /// println!("last compaction took {} µs", s.last_compact_us);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn stats(&self) -> Stats {
        let load = |c: &std::sync::atomic::AtomicU64| c.load(std::sync::atomic::Ordering::Relaxed);
        let file_len = |name: &str| {
            self.data_dir
                .as_ref()
                .and_then(|d| std::fs::metadata(d.join(name)).ok())
                .map(|m| m.len())
                .unwrap_or(0)
        };
        Stats {
            nodes: self.node_count(),
            edges: self.edge_count(),
            collections: self.collection_names().len(),
            // In paged mode the resident map IS the overlay; resident mode holds
            // everything, so "overlay" is only meaningful when paged.
            overlay_nodes: if !self.segments.is_empty() { self.nodes.len() } else { 0 },
            payload_bytes: file_len("payloads.bin"),
            wal_bytes: file_len("wal.log"),
            paged: !self.segments.is_empty(),

            field_indexes: self.field_indexes.len() + self.field_base.len(),
            hnsw_indexes: self.hnsw_indexes.len(),
            bm25_indexes: self.bm25_indexes.len(),
            search_indexes: self.search_indexes.len(),
            trigram_indexes: self.gin_indexes.len() + self.text_indexes.len(),
            // `Some` alone is not meaningful — an empty grid gets built eagerly.
            spatial_index: self.spatial_grid.as_ref().is_some_and(|g| g.len() > 0),

            queries: load(&self.counters.queries),
            writes: load(&self.counters.writes),
            compactions: load(&self.counters.compactions),
            snapshots: load(&self.counters.snapshots),

            last_compact_us: load(&self.counters.compact_us_last),
            max_compact_us: load(&self.counters.compact_us_max),
            last_snapshot_us: load(&self.counters.snapshot_us_last),
            max_snapshot_us: load(&self.counters.snapshot_us_max),
        }
    }

    pub fn node_count(&self) -> usize {
        if !self.has_base() {
            return self.nodes.len(); // resident: the map IS the store
        }
        // Base + overlay-only nodes - tombstones. An overlay entry that also exists
        // in the durable store is an update, not an addition, so it must not be
        // double-counted.
        let overlay_new = self.nodes.keys().filter(|h| !self.base_contains(**h)).count();
        (self.base_node_count() + overlay_new).saturating_sub(self.tombstones.len())
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
        // Base-aware. EdgeStore::edge_count sums the resident map, which is empty
        // once adjacency spills to CSR and never holds the mmap'd base at all — it
        // reported 0 for a database with a full graph.
        self.all_hashes().iter()
            .filter_map(|&h| self.fwd_edges(h).map(|e| e.len()))
            .sum()
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
        // Paged: collections whose members all live in the mmap'd base are absent
        // from the overlay entirely, so they must be read from the base too.
        for name in self.base_collection_names() {
            if !name.is_empty() {
                names.insert(name);
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

    /// Serialize the entire database as portable SGQL text (a `.sql` dump).
    ///
    /// The dump is the version-independent migration path (Ring 1 of the
    /// stability contract — see `docs/developer/invariants.md`): it loads into
    /// *any* sekejap version via [`load_sql`], independent of the on-disk binary
    /// format. It is plain SGQL — `CREATE TABLE`, `CREATE INDEX`, `INSERT`
    /// rows, and edge `INSERT`s — so `sqlite`/`pg_dump`-style tooling and any
    /// editor treat it as SQL. One statement per line (so `load_sql` splits on
    /// newlines without a SQL parser). Auto columns (`_id`, `_collection`,
    /// timestamps) are regenerated on load and are not emitted.
    pub fn dump_sql(&self) -> String {
        let mut out = String::from("-- sekejap dump; format 1; SGQL\n");
        let mut colls = self.collection_names();
        colls.sort();

        // 1. Schema — CREATE TABLE + CREATE INDEX for each declared collection.
        for coll in &colls {
            if let Some(s) = self.schemas.get(coll) {
                // Emit only user columns; the engine re-adds _key + timestamps on
                // CREATE TABLE, so listing them would double them each reload.
                let parts: Vec<String> = s.fields.iter()
                    .filter(|f| !is_internal_field(&f.name))
                    .map(|f| format!("{} {}", f.name, field_type_sql(f.ty)))
                    .collect();
                out.push_str(&format!("CREATE TABLE {coll} ({});\n", parts.join(", ")));

                let ix = &s.indexes;
                let groups: [(&str, &Vec<String>); 5] = [
                    ("btree", &ix.range),
                    ("gin", &ix.fulltext),
                    ("bm25", &ix.bm25),
                    ("spatial", &ix.spatial),
                    ("hnsw", &ix.vector),
                ];
                for (method, fields) in groups {
                    for f in fields {
                        out.push_str(&format!("CREATE INDEX ON {coll} USING {method} ({f});\n"));
                    }
                }
                // Search indexes cover a list of fields each.
                for fields in &ix.search {
                    out.push_str(&format!(
                        "CREATE INDEX ON {coll} USING search ({});\n", fields.join(", ")
                    ));
                }
            }
        }

        // 2. Rows — one INSERT per node, field-type aware.
        for coll in &colls {
            for hit in self.collection(coll).collect() {
                if let Some(line) = self.dump_row_insert(coll, &hit) {
                    out.push_str(&line);
                    out.push('\n');
                }
            }
        }

        // 3. Edges — INSERT ('from')-[:TYPE {attrs}]->('to').
        for coll in &colls {
            for e in self.edges_from_collection(coll) {
                let (Some(from), Some(to)) = (&e.from_slug, &e.to_slug) else { continue };
                let etype = e.edge_type.clone().unwrap_or_default();
                let attrs = dump_edge_attrs(e.meta.as_ref());
                out.push_str(&format!(
                    "INSERT ('{}')-[:{}{}]->('{}');\n",
                    sql_str_escape(from), etype, attrs, sql_str_escape(to)
                ));
            }
        }
        out
    }

    /// Build one `INSERT INTO coll (...) VALUES (...)` for a node. Returns `None`
    /// if the node has no payload. Vector fields are read from the separate
    /// vector store; GEO fields are emitted via `ST_GeomFromGeoJSON`.
    fn dump_row_insert(&self, coll: &str, hit: &query::Hit) -> Option<String> {
        let payload = hit.payload.as_ref()?.as_object()?;
        let key = payload.get("_key").and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| hit.slug.split_once('/').map(|(_, k)| k.to_string()))?;

        let mut cols = vec!["_key".to_string()];
        let mut vals = vec![format!("'{}'", sql_str_escape(&key))];

        // Field type lookup (schema'd collections get exact types; schemaless
        // collections infer from the JSON value).
        let field_types: std::collections::HashMap<&str, sql::FieldType> = self
            .schemas.get(coll)
            .map(|s| s.fields.iter().map(|f| (f.name.as_str(), f.ty)).collect())
            .unwrap_or_default();

        // Vector fields (from schema) are pulled from the vector store, not the
        // payload — emit them even though they are absent from the JSON.
        if let Some(s) = self.schemas.get(coll) {
            for f in &s.fields {
                if f.ty == sql::FieldType::Vector {
                    if let Some(v) = self.get_vector(&hit.slug, &f.name) {
                        cols.push(f.name.clone());
                        let nums: Vec<String> = v.iter().map(|x| fmt_f32(*x)).collect();
                        vals.push(format!("[{}]", nums.join(", ")));
                    }
                }
            }
        }

        // Payload fields (skip auto/internal columns, and vector fields — those
        // are emitted from the vector store above, and also linger in the payload).
        for (name, val) in payload {
            if is_internal_field(name) { continue; }
            let ty = field_types.get(name.as_str()).copied();
            if ty == Some(sql::FieldType::Vector) { continue; }
            cols.push(name.clone());
            vals.push(sql_value_literal(val, ty));
        }

        Some(format!(
            "INSERT INTO {coll} ({}) VALUES ({});",
            cols.join(", "),
            vals.join(", ")
        ))
    }

    /// Load a `.sql` dump produced by [`dump_sql`] into this database. Each
    /// non-comment line is one SGQL statement, run through the same execution
    /// path as a normal query. Returns the number of statements applied.
    pub fn load_sql(&mut self, dump: &str) -> Result<usize, SqlError> {
        let mut applied = 0usize;
        for raw in dump.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with("--") { continue; }
            let stmt = line.strip_suffix(';').unwrap_or(line);
            // CREATE/INSERT/UPDATE/DELETE go through execute(); the dump emits
            // only mutations + DDL, never SELECT.
            self.execute(stmt)?;
            applied += 1;
        }
        Ok(applied)
    }

    /// Return the structured schema for a collection, if one was declared via
    /// `CREATE TABLE`.  Returns `None` for schemaless collections.
    pub fn table_schema(&self, collection: &str) -> Option<&TableSchema> {
        self.schemas.get(collection)
    }

    /// Get all outgoing edges from a node, resolved to slugs where available.
    pub fn edges_from(&self, slug: &str) -> Vec<EdgeHit> {
        let hash = sk_hash(slug);
        // `self.fwd_edges`, not `self.edges.fwd_edges`: the former merges the mmap
        // base with the overlay, the latter sees only resident edges and returned
        // nothing at all in paged mode — taking edge attributes with it.
        self.fwd_edges(hash)
            .map(|edges| {
                edges
                    .iter()
                    .map(|e| EdgeHit {
                        from_slug: Some(slug.to_string()),
                        // Through the base-aware accessor. The adjacency here is
                        // already merged with the durable store, but the slug was
                        // looked up in the overlay alone — so on a paged database
                        // every neighbour came back as `None`.
                        to_slug: self.node_data(e.other).map(|n| n.slug.clone()),
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
        // Base-aware — see edges_from.
        self.rev_edges(hash)
            .map(|edges| {
                edges
                    .iter()
                    .map(|e| EdgeHit {
                        from_slug: self.node_data(e.other).map(|n| n.slug.clone()),
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
        // Base-aware: collection_members merges the mmap base with the overlay, so
        // this no longer reports an empty graph for a database whose rows have been
        // compacted.
        let members: Vec<u64> = match self.collection_members(col_h) {
            Some(m) => m.into_owned(),
            None => return result,
        };
        for node_h in members {
            let Some(node) = self.node_data(node_h) else { continue };
            if node.collection.is_empty() || sk_hash(&node.collection) != col_h { continue; }
            if let Some(edges) = self.fwd_edges(node_h) {
                for e in edges.iter() {
                    result.push(EdgeHit {
                        from_slug: Some(node.slug.clone()),
                        to_slug: self.node_data(e.other).map(|n| n.slug.clone()),
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
                // Base-aware: slug_map and self.nodes are overlay-only, so this
                // discarded every edge whose target had been compacted away.
                e.to_slug.as_deref()
                    .and_then(|s| self.node_data(sk_hash(s)))
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
        if let Some(edges) = self.fwd_edges(hash) {
            for e in edges.iter() {
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
        // Base-aware — see edges_from_collection.
        let members: Vec<u64> = match self.collection_members(col_h) {
            Some(m) => m.into_owned(),
            None => return types,
        };
        for node_h in members {
            let Some(node) = self.node_data(node_h) else { continue };
            if node.collection.is_empty() || sk_hash(&node.collection) != col_h { continue; }
            if let Some(edges) = self.fwd_edges(node_h) {
                for e in edges.iter() {
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
        // Base-aware — see edges_from_collection.
        for from_h in self.all_hashes() {
            let Some(node) = self.node_data(from_h) else { continue };
            if node.collection.is_empty() { continue; }
            let from_col = node.collection.clone();
            if let Some(edges) = self.fwd_edges(from_h) {
                for e in edges.iter() {
                    let edge_label = match self.edges.type_name(e.edge_type) {
                        Some(l) => l.to_string(),
                        None => continue,
                    };
                    let to_col = match self.node_data(e.other) {
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
    //
    // Each starter returns a [`Set`] builder seeded with one starting [`Step`].
    // Nothing runs yet — you chain filters/hops/shapers onto it and then call
    // `.collect()` or `.count()` to execute (see `query.rs`).

    /// Start a query from a single node (`SELECT … WHERE _key = …`).
    pub fn one(&self, slug: &str) -> Set<'_> {
        Set::new(self, Step::One(sk_hash(slug)))
    }

    /// Start a query from a specific set of nodes given by slug.
    pub fn many<'a>(&self, slugs: impl IntoIterator<Item = &'a str>) -> Set<'_> {
        Set::new(self, Step::Many(slugs.into_iter().map(sk_hash).collect()))
    }

    /// Start a query over every node in the database (a full scan).
    pub fn all(&self) -> Set<'_> {
        Set::new(self, Step::All)
    }

    /// Start a query over all nodes in one named collection (`SELECT … FROM name`).
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
        self.counters.queries.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        // (from_hash, edge_type_hash) — edge metadata is fetched lazily during
        // reconstruction for the ~path-length edges only, not every visited edge.
        let mut parent: HashMap<u64, (u64, u64)> = HashMap::new();

        // Same-node degenerate case
        if start == end {
            // Base-aware: `self.nodes` is only the write overlay in paged mode, so
            // checking it alone reported every base-resident node as missing and
            // made SHORTEST return nothing at all.
            if let Some(node) = self.node_data(start) {
                let hit = query::Hit {
                    slug: node.slug.clone(),
                    slug_hash: start,
                    payload: self.payload_store
                        .get_of(start, node.payload_offset, node.payload_len),
                };
                return Some(BfsPath { nodes: vec![hit], edges: vec![], length: 0 });
            } else {
                return None; // start node doesn't exist
            }
        }

        // The start node must exist
        if !self.node_exists(start) {
            return None;
        }

        parent.insert(start, (start, 0)); // sentinel
        let mut queue: VecDeque<u64> = VecDeque::new();
        queue.push_back(start);

        while let Some(current) = queue.pop_front() {
            if let Some(edges) = self.fwd_edges(current) {
                for e in edges.iter() {
                    if parent.contains_key(&e.other) {
                        continue; // already visited
                    }
                    parent.insert(e.other, (current, e.edge_type));
                    if e.other == end {
                        // Reconstruct path: walk parent map from end → start, then reverse.
                        let mut node_hashes: Vec<u64> = Vec::new();
                        let mut cur = end;
                        loop {
                            node_hashes.push(cur);
                            let (prev, _) = parent[&cur];
                            if prev == cur {
                                break; // reached the sentinel (start node)
                            }
                            cur = prev;
                        }
                        node_hashes.reverse();

                        // Build Hit list from the ordered hashes. No payload read:
                        // callers use only the slug (path node_slugs); a.*/b.* payloads
                        // are bound separately, and predicates reload per node.
                        let nodes: Vec<query::Hit> = node_hashes
                            .iter()
                            .filter_map(|&h| {
                                // Base-aware — an intermediate node living in the
                                // base would otherwise drop out of the path.
                                self.node_data(h).map(|n| query::Hit {
                                    slug: n.slug.clone(),
                                    slug_hash: h,
                                    payload: None,
                                })
                            })
                            .collect();

                        // Build EdgeHit list for the path edges only. Edge metadata is
                        // fetched here (path-length lookups) instead of for every edge
                        // visited during the search.
                        let edges: Vec<EdgeHit> = node_hashes
                            .windows(2)
                            .map(|w| {
                                let (_, edge_type_hash) = parent[&w[1]];
                                let meta = self.fwd_edges(w[0]).and_then(|es| {
                                    es.iter()
                                        .find(|e| e.other == w[1] && e.edge_type == edge_type_hash)
                                        .and_then(|e| self.edges.edge_meta(e))
                                });
                                EdgeHit {
                                    from_slug: self.node_data(w[0]).map(|n| n.slug.clone()),
                                    to_slug: self.node_data(w[1]).map(|n| n.slug.clone()),
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

    /// Length-only shortest path: BFS returning just the hop count, with zero path
    /// materialization — no `Hit`s, slugs, payloads, or edge metadata, and level-set
    /// BFS instead of a per-node parent map. Powers the fast-path for simple
    /// `MATCH SHORTEST` queries that only need `length(path)` + endpoint keys.
    /// Same reachability semantics as `bfs_shortest_path`.
    pub(crate) fn bfs_shortest_len(&self, start: u64, end: u64) -> Option<usize> {
        use std::collections::HashSet;
        if start == end {
            return if self.node_exists(start) { Some(0) } else { None };
        }
        if !self.node_exists(start) {
            return None;
        }
        let mut visited: HashSet<u64> = HashSet::new();
        visited.insert(start);
        let mut frontier: Vec<u64> = vec![start];
        let mut depth = 0usize;
        while !frontier.is_empty() {
            depth += 1;
            let mut next: Vec<u64> = Vec::new();
            for &node in &frontier {
                if let Some(edges) = self.fwd_edges(node) {
                    for e in edges.iter() {
                        if e.other == end {
                            return Some(depth);
                        }
                        if visited.insert(e.other) {
                            next.push(e.other);
                        }
                    }
                }
            }
            frontier = next;
        }
        None
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
                // Base-aware: collection_names() and collection_members() merge the
                // mmap base with the overlay. Reading collection_names_map and
                // self.collections directly reported only what had been written
                // since the last compaction — one collection instead of two, with
                // zero rows.
                let names: Vec<(u64, String)> = self.collection_names()
                    .into_iter().map(|n| (sk_hash(&n), n)).collect();
                // With nodes on pages, one streaming pass over the store answers
                // this for every collection at once. The per-collection version
                // below asks `payload_loc` for each member, and in this mode that
                // is a B+tree descent per row — 400 000 of them on a 400 000-row
                // store, to report a size. The walk reads each record once from the
                // id the index already handed over.
                let mut done = false;
                if let Some(ns) = &self.paged_nodes {
                    let mut totals: std::collections::HashMap<String, (usize, u64)> =
                        std::collections::HashMap::new();
                    if ns.for_each_node(|hash, n| {
                        if !n.collection.is_empty() && !self.tombstones.contains(&hash) {
                            let e = totals.entry(n.collection).or_insert((0, 0));
                            e.0 += 1;
                            e.1 += n.payload_len as u64;
                        }
                        true
                    }).is_ok() {
                        // Overlay rows the durable store has not taken yet.
                        for (&h, node) in &self.nodes {
                            if node.collection.is_empty() || self.tombstones.contains(&h) { continue }
                            if ns.contains(h).unwrap_or(false) { continue }
                            let e = totals.entry(node.collection.clone()).or_insert((0, 0));
                            e.0 += 1;
                            e.1 += node.payload_len as u64;
                        }
                        for (name, v) in totals { stats.insert(name, v); }
                        for (_, name) in &names { stats.entry(name.clone()).or_insert((0, 0)); }
                        done = true;
                    }
                }
                if !done {
                    for (hash, name) in &names {
                        let (count, size) = self.collection_members(*hash)
                            .map(|members| {
                                let c = members.len();
                                let s: u64 = members.iter()
                                    .filter_map(|h| self.payload_loc(*h).map(|(_, len)| len as u64))
                                    .sum();
                                (c, s)
                            })
                            .unwrap_or((0, 0));
                        stats.insert(name.clone(), (count, size));
                    }
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
                        for from_h in self.all_hashes() {
                            let Some(node) = self.node_data(from_h) else { continue };
                            let from_col = if node.collection.is_empty() {
                                continue;
                            } else {
                                node.collection.clone()
                            };
                            if let Some(edges) = self.fwd_edges(from_h) {
                                for edge in edges.iter() {
                                    let label = match self.edges.type_name(edge.edge_type) {
                                        Some(l) => l.to_string(),
                                        None => continue,
                                    };
                                    let to_col = match self.node_data(edge.other)
                                        .map(|n| n.collection.clone())
                                    {
                                        Some(c) if !c.is_empty() => c,
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
                        for node_h in self.collection_members(col_h)
                            .map(|m| m.into_owned()).unwrap_or_default()
                        {
                            let Some(node) = self.node_data(node_h) else { continue };
                            if !node.collection.is_empty() && sk_hash(&node.collection) == col_h {
                                if let Some(edges) = self.fwd_edges(node_h) {
                                    for edge in edges.iter() {
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
                        for node_h in self.collection_members(from_h)
                            .map(|m| m.into_owned()).unwrap_or_default()
                        {
                            let Some(node) = self.node_data(node_h) else { continue };
                            if !node.collection.is_empty() && sk_hash(&node.collection) == from_h {
                                if let Some(edges) = self.fwd_edges(node_h) {
                                    for edge in edges.iter() {
                                        let in_to = self.node_data(edge.other)
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

                let members: Vec<u64> = self
                    .collection_members(col_h)
                    .map(|m| m.into_owned())
                    .unwrap_or_default();
                for h in members {
                    if let Some(node) = self.node_data(h) {
                        if let Some(payload) =
                            self.payload_store.get_of(h, node.payload_offset, node.payload_len)
                        {
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
        let m = sql::parse_mutation_params(sql, params.to_vec())?;
        self.execute_mutation(m)
    }

    /// Run a materialized view's body query and (re)populate its derived collection.
    /// Each projected row becomes a node keyed by its `id` (or `_key`) column.
    fn materialize_view(&mut self, name: &str, query_sql: &str, root: &str) -> Result<usize, SqlError> {
        let hits = self.query(query_sql)?.collect();
        // Vector fields of the root collection are mirrored from the source vector
        // store into the view (they don't ride in the projection payload). GEO fields
        // ride in the payload, so they materialize for free.
        let root_vec_fields: Vec<String> = self.schemas.get(root)
            .map(|s| s.fields.iter()
                .filter(|f| matches!(f.ty, sql::FieldType::Vector))
                .map(|f| f.name.clone()).collect())
            .unwrap_or_default();
        let mut count = 0;
        for h in hits {
            let mut doc = match h.payload {
                Some(Value::Object(m)) => m,
                _ => continue,
            };
            let key = doc.get("id").or_else(|| doc.get("_key"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| SqlError::InvalidValue(format!(
                    "materialized view '{name}' projection must include an 'id' (or '_key') column")))?
                .to_string();
            // Keep the projected `id` column (so `SELECT id FROM view` works) and also
            // set `_key`/`_collection` so the view is an ordinary, queryable collection.
            doc.insert("_key".to_string(), Value::String(key.clone()));
            doc.insert("_collection".to_string(), Value::String(name.to_string()));
            let json = serde_json::to_string(&Value::Object(doc))
                .map_err(|e| SqlError::InvalidValue(e.to_string()))?;
            self.put(&format!("{name}/{key}"), &json)
                .map_err(|e| SqlError::InvalidValue(e.to_string()))?;
            // Mirror each root vector (same field name) into the view doc.
            for vf in &root_vec_fields {
                if let Some(vec) = self.get_vector(&format!("{root}/{key}"), vf) {
                    let _ = self.put_vector(&format!("{name}/{key}"), vf, &vec);
                }
            }
            count += 1;
        }
        Ok(count)
    }

    /// Remove every node of a view's derived collection (before a refresh repopulates).
    fn clear_view_collection(&mut self, name: &str) {
        let members: Vec<u64> = self.collection_members(sk_hash(name))
            .map(|m| m.into_owned())
            .unwrap_or_default();
        let slugs: Vec<String> = members.iter()
            .filter_map(|&h| self.node_data(h).map(|n| n.slug.clone()))
            .collect();
        for s in slugs {
            self.remove(&s);
        }
    }

    /// SEARCH VIEW: auto-build a BM25 index on each string field of the view.
    /// (v1 — vector/geo auto-indexing is a follow-up.)
    fn auto_index_view(&mut self, name: &str, root: &str) {
        let sample = self.collection_members(sk_hash(name))
            .and_then(|m| m.first().copied())
            .and_then(|h| self.get_payload(h));
        let mut text_fields = Vec::new();
        let mut geo_fields = Vec::new();
        if let Some(Value::Object(map)) = sample {
            for (k, v) in &map {
                if k.starts_with('_') || k == "id" { continue; }
                match v {
                    Value::String(_) => text_fields.push(k.clone()),
                    // GeoJSON object → spatial.
                    Value::Object(o) if o.contains_key("type") && o.contains_key("coordinates")
                        => geo_fields.push(k.clone()),
                    _ => {}
                }
            }
        }
        // Vector fields mirror the root collection's vector fields (same names).
        let vec_fields: Vec<String> = self.schemas.get(root)
            .map(|s| s.fields.iter()
                .filter(|f| matches!(f.ty, sql::FieldType::Vector))
                .map(|f| f.name.clone()).collect())
            .unwrap_or_default();
        for f in text_fields { let _ = self.apply_index(name, &sql::IndexMethod::Bm25, &[f]); }
        for f in geo_fields  { let _ = self.apply_index(name, &sql::IndexMethod::Spatial, &[f]); }
        for f in vec_fields  { let _ = self.apply_index(name, &sql::IndexMethod::Hnsw, &[f]); }
    }

    /// Internal: execute an already-parsed mutation.
    fn execute_mutation(&mut self, mutation: sql::CompiledMutation) -> Result<usize, SqlError> {
        // Every SQL mutation funnels through here, so one guard covers INSERT,
        // UPDATE, DELETE, the DDL forms and COMPACT alike.
        if self.read_only {
            return Err(SqlError::InvalidValue(self.refuse_write("statement").to_string()));
        }
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
                self.commit_depth += 1;
                let mut txn_err = None;
                for op in buf {
                    match self.execute_mutation(op) {
                        Ok(n) => total += n,
                        Err(e) => { txn_err = Some(e); break; }
                    }
                }
                self.commit_depth -= 1;
                if let Some(e) = txn_err {
                    self.wal_write(WalEntry::TxnEnd);
                    self.defer_wal_sync = false;
                    self.wal_flush();
                    self.flush_deferred_indexes();
                    self.pending_change.clear();
                    return Err(e);
                }
                self.wal_write(WalEntry::TxnEnd);
                self.defer_wal_sync = false;
                self.wal_flush();
                self.flush_deferred_indexes();
                self.emit_changes();
                return Ok(total);
            }
            sql::CompiledMutation::Rollback => {
                // Discard any changes accumulated in the aborted transaction.
                self.pending_change.clear();
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
                        // Generated columns: computed from the record's other fields,
                        // OVERRIDING any user-supplied value. Runs after defaults so it
                        // can read default-filled fields too.
                        for field in &schema.fields {
                            if let Some(expr) = &field.generated {
                                let val = expr.eval(&map);
                                map.insert(field.name.clone(), Value::String(val));
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
                    if let Err(e) = self.vectors.get_mut(&field).unwrap().put(hash, data) {
                        self.note_write_error(format!("vector write failed: {e}"));
                    }
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
                self.emit_changes();
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
                self.emit_changes();
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
                self.emit_changes();
                Ok(count)
            }
            sql::CompiledMutation::DeleteEdge(edges) => {
                let count = edges.len();
                self.defer_wal_sync = true;
                for edge in edges {
                    match &edge.props {
                        Some(Value::Object(m)) if !m.is_empty() => {
                            let props_json = serde_json::to_string(&Value::Object(m.clone()))
                                .unwrap_or_else(|_| "{}".to_string());
                            self.unlink_where(&edge.from, &edge.to, &edge.edge_type, &props_json);
                        }
                        _ => self.unlink(&edge.from, &edge.to, &edge.edge_type),
                    }
                }
                self.defer_wal_sync = false;
                self.wal_flush();
                self.emit_changes();
                Ok(count)
            }
            sql::CompiledMutation::UpdateEdge { from, to, edge_type, predicate, sets } => {
                let props_json = predicate
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "{}".to_string());
                let sets_map: serde_json::Map<String, Value> = sets.into_iter().collect();
                let sets_json = serde_json::to_string(&Value::Object(sets_map))
                    .unwrap_or_else(|_| "{}".to_string());
                let n = self.update_edge(&from, &to, &edge_type, &props_json, &sets_json);
                Ok(n)
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
                self.emit_changes();
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
                // Generated columns must be recomputed from the updated record, which
                // the splice fast path can't do — route through the slow (full-parse)
                // path whenever the target may carry a generated column.
                let has_generated = self.schemas.values()
                    .any(|s| s.fields.iter().any(|f| f.generated.is_some()));

                if !has_vec && !has_geo && !has_generated {
                    let now_ms = chrono::Utc::now().timestamp_millis();
                    self.update_fast_path(steps, &updates, now_ms)
                } else {
                    // ── SLOW PATH: full parse (vector/geo field updates) ──────────
                    let hits: Vec<(String, Value)> = Set::from_steps(self, steps)
                        .collect()
                        .into_iter()
                        .filter_map(|h| {
                            let n = self.nodes.get(&h.slug_hash)?;
                            let payload = self.payload_store
                                .get_of(h.slug_hash, n.payload_offset, n.payload_len)?;
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
                        // Recompute generated columns from the now-updated record.
                        if let Value::Object(ref mut map) = payload {
                            let coll = map.get("_collection").and_then(|v| v.as_str()).map(str::to_string);
                            if let Some(coll) = coll {
                                let gens: Vec<(String, sql::GenExpr)> = self.schemas.get(&coll)
                                    .map(|s| s.fields.iter()
                                        .filter_map(|f| f.generated.clone().map(|g| (f.name.clone(), g)))
                                        .collect())
                                    .unwrap_or_default();
                                for (name, expr) in gens {
                                    let val = expr.eval(map);
                                    map.insert(name, Value::String(val));
                                }
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
                            if let Err(e) = self.vectors.get_mut(&field).unwrap().put(hash, data) {
                                self.note_write_error(format!("vector write failed: {e}"));
                            }
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
                    self.emit_changes();
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
            sql::CompiledMutation::CreateView { name, query_sql, auto_index } => {
                let root = sql::extract_root_collection(&query_sql).unwrap_or_default();
                self.materialized_views.insert(name.clone(), (query_sql.clone(), auto_index, root.clone()));
                let n = self.materialize_view(&name, &query_sql, &root)?;
                if auto_index {
                    self.auto_index_view(&name, &root);
                }
                Ok(n)
            }
            sql::CompiledMutation::RefreshView { name } => {
                let (query_sql, root) = self.materialized_views.get(&name)
                    .map(|(q, _, r)| (q.clone(), r.clone()))
                    .ok_or_else(|| SqlError::InvalidValue(format!("no materialized view named '{name}'")))?;
                self.clear_view_collection(&name);
                let n = self.materialize_view(&name, &query_sql, &root)?;
                Ok(n)
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
        if !self.tombstones.is_empty() && self.tombstones.contains(&hash) {
            return None;
        }
        // Overlay first: anything written since open (or everything, in resident
        // mode) lives in the resident map and wins over the mapped base.
        if let Some(n) = self.nodes.get(&hash) {
            return Some(std::borrow::Cow::Borrowed(n));
        }
        self.base_node(hash).map(std::borrow::Cow::Owned)
    }

    // ── the durable node store ────────────────────────────────────────────────
    //
    // Below this line is every question the engine asks of the nodes it has
    // already written down, as opposed to the ones in the RAM overlay. There is
    // one implementation of each, so a storage backend is chosen in one place
    // rather than in the ten call sites that used to reach into `segments`
    // themselves. That is not tidiness: "which of these consults the base?" is
    // precisely the question this codebase has answered wrongly a dozen times, and
    // a call site that cannot get it wrong is worth more than a careful one.
    //
    // Each returns what the durable store holds, ignoring the overlay and
    // ignoring tombstones — the callers layer those on, because which of them
    // apply differs by question.

    /// One node as the durable store holds it.
    fn base_node(&self, hash: u64) -> Option<NodeData> {
        if let Some(ns) = &self.paged_nodes {
            let n = ns.get(hash).ok().flatten()?;
            return Some(NodeData {
                slug: n.slug,
                collection: n.collection,
                spatial_meta: n.spatial.map(|v| Box::new(geo::SpatialMeta {
                    centroid_lat: v[0], centroid_lon: v[1],
                    bbox_min_lat: v[2], bbox_min_lon: v[3],
                    bbox_max_lat: v[4], bbox_max_lon: v[5],
                })),
                payload_offset: n.payload_offset,
                payload_len: n.payload_len,
            });
        }
        let base = self.segments.newest_first().next()?;
        let id = base.resolve(hash)?;
        let rec = base.node_record(id)?;
        Some(NodeData {
            slug: base.slug_of(id)?.to_string(),
            collection: base.collection_name(rec.collection_id).unwrap_or("").to_string(),
            spatial_meta: base.spatial(id).map(|v| Box::new(geo::SpatialMeta {
                centroid_lat: v[0], centroid_lon: v[1],
                bbox_min_lat: v[2], bbox_min_lon: v[3],
                bbox_max_lat: v[4], bbox_max_lon: v[5],
            })),
            payload_offset: rec.payload_offset,
            payload_len: rec.payload_len,
        })
    }

    /// Whether the durable store holds this node — without reading its record,
    /// where the backend can answer that more cheaply.
    fn base_contains(&self, hash: u64) -> bool {
        if let Some(ns) = &self.paged_nodes {
            return ns.contains(hash).unwrap_or(false);
        }
        self.segments.resolve(hash).is_some()
    }

    /// Where a node's payload is, according to the durable store.
    fn base_payload_loc(&self, hash: u64) -> Option<(u64, u32)> {
        if let Some(ns) = &self.paged_nodes {
            let n = ns.get(hash).ok().flatten()?;
            return Some((n.payload_offset, n.payload_len));
        }
        let base = self.segments.newest_first().next()?;
        let rec = base.node_record(base.resolve(hash)?)?;
        Some((rec.payload_offset, rec.payload_len))
    }

    /// How many nodes the durable store holds.
    fn base_node_count(&self) -> usize {
        if let Some(ns) = &self.paged_nodes {
            return ns.len() as usize;
        }
        self.segments.newest_first().next().map_or(0, |b| b.node_count())
    }

    /// Whether there is a durable store at all. `false` means the overlay is the
    /// whole database, which is what resident mode is.
    fn has_base(&self) -> bool {
        self.paged_nodes.is_some() || !self.segments.is_empty()
    }

    /// Every node hash the durable store holds.
    fn base_hashes(&self) -> Vec<u64> {
        if let Some(ns) = &self.paged_nodes {
            let mut out = Vec::with_capacity(ns.len() as usize);
            let _ = ns.for_each_hash(|h| { out.push(h); true });
            return out;
        }
        self.segments.newest_first().next().map_or_else(Vec::new, |b| b.all_hashes())
    }

    /// The members of one collection, as the durable store has them.
    fn base_members(&self, coll_hash: u64) -> Option<Vec<u64>> {
        // A renamed collection's durable membership is stale by definition: the
        // rows moved to a new hash in the overlay and the base still lists them
        // here. Ignoring it is what makes the old name stop answering.
        if self.renamed_collections.contains(&coll_hash) { return None }
        if let Some(ns) = &self.paged_nodes {
            let m = ns.members(coll_hash).ok()?;
            return if m.is_empty() { None } else { Some(m) };
        }
        self.segments.find(|b| b.members_by_coll_hash(coll_hash))
    }

    /// Every node in the durable store that has geometry, with its extent.
    ///
    /// Its own accessor rather than `base_hashes` + `base_node` because the two
    /// backends answer it very differently: a segment keeps geometry in a 48-byte
    /// side table it can scan without touching a node record, while the paged store
    /// keeps it inside the record and has to read them. Going through the generic
    /// pair would make the segment path read every slug and collection name in the
    /// database to find the handful of rows that are places.
    fn base_spatial_items(&self) -> Vec<(u64, geo::SpatialMeta)> {
        let mut items = Vec::new();
        if let Some(ns) = &self.paged_nodes {
            // Through the geometry index, not a walk of every node: on a store with
            // no geometry that reads nothing, where the walk read every record to
            // discover exactly that — 183 ms per compaction on 200 000 rows.
            for (h, v) in ns.spatial_items().unwrap_or_default() {
                items.push((h, geo::SpatialMeta {
                    centroid_lat: v[0], centroid_lon: v[1],
                    bbox_min_lat: v[2], bbox_min_lon: v[3],
                    bbox_max_lat: v[4], bbox_max_lon: v[5],
                }));
            }
            return items;
        }
        let Some(base) = self.segments.newest_first().next() else { return items };
        for id in 0..base.node_count() as u64 {
            let (Some(v), Some(h)) = (base.spatial(id), base.hash_of(id)) else { continue };
            items.push((h, geo::SpatialMeta {
                centroid_lat: v[0], centroid_lon: v[1],
                bbox_min_lat: v[2], bbox_min_lon: v[3],
                bbox_max_lat: v[4], bbox_max_lon: v[5],
            }));
        }
        items
    }

    /// Point a node at a payload it has just been rewritten into.
    ///
    /// A node already in the overlay is edited in place. A node in the durable
    /// store cannot be: it is copied into the overlay carrying the new location,
    /// which is what an update to an immutable record means and what the next
    /// compaction folds back down.
    ///
    /// Writing only to the overlay — which is what every DDL rewrite did — left
    /// base nodes pointing at their old payloads, so the bytes were rewritten and
    /// nothing ever read them.
    fn set_payload_loc(&mut self, hash: u64, off: u64, len: u32) {
        if let Some(node) = self.nodes.get_mut(&hash) {
            node.payload_offset = off;
            node.payload_len = len;
            return;
        }
        if let Some(n) = self.base_node(hash) {
            self.slug_map.insert(n.slug.clone(), hash);
            self.nodes.insert(hash, NodeData { payload_offset: off, payload_len: len, ..n });
        }
    }

    /// Every collection name the durable store knows.
    fn base_collection_names(&self) -> Vec<String> {
        if let Some(ns) = &self.paged_nodes {
            // From the name table, not by reading the store. This used to walk every
            // node and decode its record to collect the distinct names — O(store)
            // work behind `SHOW TABLES`, and more expensive still once records were
            // checksummed. The table is written at every compaction and loaded at
            // open; a collection whose name is somehow missing from it is still
            // found by the fallback below, so the cheap path cannot lose a table.
            let mut names: std::collections::BTreeSet<String> =
                self.collection_names_map.values().cloned().collect();
            if names.is_empty() {
                let _ = ns.for_each_hash(|h| {
                    if let Ok(Some(n)) = ns.get(h) {
                        if !n.collection.is_empty() { names.insert(n.collection); }
                    }
                    true
                });
            }
            return names.into_iter().collect();
        }
        self.segments.newest_first().next()
            .map_or_else(Vec::new, |b| b.collection_names().to_vec())
    }

    pub(crate) fn collection_name(&self, coll_hash: u64) -> Option<&str> {
        if let Some(s) = self.collection_names_map.get(&coll_hash) {
            return Some(s.as_str());
        }
        // Borrowed from whichever segment knows the name, newest first.
        self.segments.newest_first().find_map(|b| b.collection_name_by_hash(coll_hash))
    }

    /// Drop, from a base node's edge list, the single edges `unlink` withdrew.
    ///
    /// The base is immutable, so a withdrawal cannot be applied to it — it is
    /// recorded and subtracted here instead, the same way `tombstones` subtracts a
    /// deleted node. Without this an `unlink` against a base edge did nothing at
    /// all, since `EdgeStore::unlink` can only retain-out of the RAM overlay.
    ///
    /// `owner` is whichever end of the edge this list belongs to, so the recorded
    /// `(from, to, type)` is reassembled from the right side for each direction.
    fn without_unlinked(
        &self,
        owner: u64,
        edges: Vec<storage::topology::MappedEdge>,
        forward: bool,
    ) -> Vec<storage::topology::MappedEdge> {
        if self.unlinked_edges.is_empty() { return edges }
        edges.into_iter()
            .filter(|e| {
                let key = if forward {
                    (owner, e.other_hash, e.edge_type_hash)
                } else {
                    (e.other_hash, owner, e.edge_type_hash)
                };
                !self.unlinked_edges.contains(&key)
            })
            .collect()
    }

    /// Move every edge written since the last fold into the durable paged graph.
    ///
    /// This is what replaces rebuilding `adj_fwd.bin` / `adj_rev.bin`. The old
    /// phase read the whole graph and wrote the whole graph; this one touches only
    /// the nodes whose edges changed, which is what Law 2 asks for — cost
    /// proportional to the change, not to the store.
    ///
    /// The reverse direction is derived from the forward one rather than read from
    /// its own overlay. Every edge `a → b` is exactly the edge `b ← a`, so deriving
    /// it makes the two directions incapable of disagreeing; reading both would
    /// make that a thing to get right, and adjacency getting out of step with
    /// itself is not a bug a caller could ever diagnose.
    ///
    /// Deletions are applied first. An edge removed and re-added in the same window
    /// has to end up present, and a node deleted after its edges were written has
    /// to end up with none.
    /// Move every node written since the last fold into the durable paged store.
    ///
    /// The counterpart of `fold_edges_into_paged`, and the reason the four node
    /// files stop being rewritten: what changed is written, and what did not is
    /// left alone.
    fn fold_nodes_into_paged(&mut self) -> io::Result<()> {
        let Some(mut ns) = self.paged_nodes.take() else { return Ok(()) };
        let result = (|| -> io::Result<()> {
            // Deletions first: a node written and then deleted in the same window
            // has to end up absent, not present.
            for &h in &self.tombstones {
                ns.delete(h)?;
            }
            for (&hash, n) in &self.nodes {
                if self.tombstones.contains(&hash) { continue }
                // The payload rewrite may already have moved this record; the
                // overlay still holds where it used to be.
                let (off, len) = self.compact_payload_moves.get(&hash).copied()
                    .unwrap_or((n.payload_offset, n.payload_len));
                ns.put(hash, &storage::nodestore::StoredNode {
                    collection: n.collection.clone(),
                    payload_offset: off,
                    payload_len: len,
                    spatial: n.spatial_meta.as_ref().map(|m| [
                        m.centroid_lat, m.centroid_lon,
                        m.bbox_min_lat, m.bbox_min_lon,
                        m.bbox_max_lat, m.bbox_max_lon,
                    ]),
                    slug: n.slug.clone(),
                })?;
            }
            ns.sync()
        })();
        self.paged_nodes = Some(ns);
        result
    }

    fn fold_edges_into_paged(&mut self) -> io::Result<()> {
        let Some(mut pa) = self.paged_adj.take() else { return Ok(()) };
        let result = self.fold_edges_into(&mut pa);
        self.paged_adj = Some(pa);
        result
    }

    fn fold_edges_into(&mut self, pa: &mut PagedAdjacency) -> io::Result<()> {
        use storage::adjstore::{AdjEdge, NO_META};

        // 1. Nodes whose edges were dropped wholesale — a deleted node, or one
        //    whose edges were cleared. Both directions, and the edges pointing at
        //    them, which is the expensive half and the reason `remove_all_to`
        //    exists rather than leaving dangling edges to be filtered on read.
        let dropped: Vec<u64> = self.edge_tombstones.iter()
            .chain(self.tombstones.iter())
            .copied()
            .collect();
        for h in dropped {
            pa.fwd.remove_owner(h)?;
            pa.rev.remove_owner(h)?;
            pa.fwd.remove_all_to(h)?;
            pa.rev.remove_all_to(h)?;
        }

        // 2. Individual edges withdrawn by `unlink`. The base could not be edited
        //    when the call came in, so the withdrawal was recorded; this is where
        //    it is finally applied to the durable graph.
        let withdrawn: Vec<(u64, u64, u64)> = self.unlinked_edges.iter().copied().collect();
        for (from, to, ty) in withdrawn {
            pa.fwd.remove(from, to, Some(ty))?;
            pa.rev.remove(to, from, Some(ty))?;
        }

        // 3. Everything added. Grouped per owner so the forward side is one rewrite
        //    per node rather than one per edge — the difference between O(d) and
        //    O(d^2) on a node that gained several edges.
        let mut by_owner: Vec<(u64, Vec<AdjEdge>)> = Vec::new();
        let mut reverse: HashMap<u64, Vec<AdjEdge>> = HashMap::new();
        for (&owner, edges) in self.edges.iter_fwd() {
            if self.tombstones.contains(&owner) { continue }
            let mut list = Vec::with_capacity(edges.len());
            for e in edges {
                if self.tombstones.contains(&e.other) { continue }
                // Fast-lane columns are folded back into the JSON bag, the same
                // way the CSR path folded them into edgemeta.bin. On read the
                // routing re-splits them, so columns survive without the durable
                // format having to know about them.
                let meta_ref = match self.edge_all_attrs(e) {
                    Some(v) => pa.meta.insert(v.to_string().as_bytes())?.0,
                    None => NO_META,
                };
                list.push(AdjEdge { other: e.other, edge_type: e.edge_type, meta_ref });
                reverse.entry(e.other).or_default()
                    .push(AdjEdge { other: owner, edge_type: e.edge_type, meta_ref });
            }
            if !list.is_empty() { by_owner.push((owner, list)) }
        }
        for (owner, list) in by_owner {
            pa.fwd.add_many(owner, &list)?;
        }
        for (owner, list) in reverse {
            pa.rev.add_many(owner, &list)?;
        }

        // The overlay has been made durable, so it must go — leaving it would
        // double every edge on the next read, since reads merge the two.
        self.edges.reset_adjacency();
        self.edge_tombstones.clear();
        pa.sync()
    }

    /// How many nodes and edges a compaction of this store must produce.
    ///
    /// Named rather than inlined because it is the number the verify-before-commit
    /// rail compares against, and because getting it from the wrong place is a
    /// silent failure: too small a number and the rail passes whatever it is given.
    /// `all_hashes` is the enumeration that spans the immutable base as well as the
    /// RAM overlay, which is exactly the population `write_topology_files` writes.
    ///
    /// Exposed to tests so the arithmetic can be checked without having to make a
    /// compaction go wrong on purpose.
    #[doc(hidden)]
    pub fn compaction_expectation(&self) -> (usize, usize) {
        // Both counts without enumerating the store, when the store can answer
        // them. `all_hashes` builds a set of every node in the database, which is
        // 26 ms at 500 000 rows — paid by the rail on every compaction, to count
        // rows that a paged store has been counting all along.
        let pending = !self.unlinked_edges.is_empty()
            || !self.tombstones.is_empty()
            || !self.edge_tombstones.is_empty();
        if !pending {
            if let (Some(_), Some(pa)) = (&self.paged_nodes, &self.paged_adj) {
                return (self.node_count(), pa.fwd.edge_count() as usize);
            }
        }
        let live = self.all_hashes();
        // Walking every node's edges to count them dominated compaction: 893 ms of
        // a 1478 ms rebuild at 200 000 nodes, so the check cost more than twice the
        // work it was checking. It is done that way because a walk drops dangling
        // edges, and only a walk knows which those are.
        //
        // With paged adjacency the walk is not merely expensive but pointless: the
        // graph is not rewritten by a compaction, so the number before and the
        // number after are the same number, read off a total the store maintains as
        // it is written. There is nothing for a readback to disagree with because
        // nothing was rewritten. `the_edge_count_matches_a_walk` is what stands
        // behind that total, since a rail checking a made-up number is worse than
        // no rail at all.
        //
        // Only when nothing is pending, though. The stored total is what the graph
        // holds *now*; a compaction is about to fold in withdrawals and deletions
        // that will make it smaller, and a rail expecting the larger number fails
        // every compaction that follows an `unlink`. When anything is pending, the
        // walk is what tells the truth, and paying for it is the right answer to
        // "the cheap number would be wrong here".
        if let Some(pa) = &self.paged_adj {
            let pending = !self.unlinked_edges.is_empty()
                || !self.tombstones.is_empty()
                || !self.edge_tombstones.is_empty();
            if !pending {
                return (live.len(), pa.fwd.edge_count() as usize);
            }
        }
        let edges = live.iter()
            .filter_map(|&h| self.fwd_edges(h).map(|e| e.len()))
            .sum();
        (live.len(), edges)
    }

    pub(crate) fn all_hashes(&self) -> Vec<u64> {
        if !self.has_base() {
            return self.nodes.keys().copied().collect();
        }
        // Base ∪ overlay (the overlay may hold updates of base nodes — dedup keeps
        // each hash once).
        let mut set: HashSet<u64> = self.base_hashes().into_iter().collect();
        set.extend(self.nodes.keys().copied());
        // Deleted base nodes must not reappear in a full enumeration.
        for t in &self.tombstones {
            set.remove(t);
        }
        set.into_iter().collect()
    }

    pub(crate) fn fwd_edges(&self, hash: u64) -> Option<std::borrow::Cow<'_, [Edge]>> {
        let dropped = self.edge_tombstones.contains(&hash);
        let edges = Self::merged_edges(self.edges.fwd_edges(hash), || {
            if dropped { return None; }
            // Paged adjacency *replaces* the segments as the durable base rather
            // than adding to them. Consulting both would double every edge that
            // had been folded in, since folding does not erase the segment it
            // came from until the segment itself is dropped.
            let base = match &self.paged_adj {
                Some(pa) => pa.edges(hash, true),
                None => self.segments.find(|b| b.fwd_by_hash(hash)),
            };
            base.map(|v| self.without_unlinked(hash, v, true))
        })?;
        Some(self.drop_dangling(edges))
    }

    pub(crate) fn rev_edges(&self, hash: u64) -> Option<std::borrow::Cow<'_, [Edge]>> {
        let dropped = self.edge_tombstones.contains(&hash);
        let edges = Self::merged_edges(self.edges.rev_edges(hash), || {
            if dropped { return None; }
            // Paged adjacency *replaces* the segments as the durable base rather
            // than adding to them. Consulting both would double every edge that
            // had been folded in, since folding does not erase the segment it
            // came from until the segment itself is dropped.
            let base = match &self.paged_adj {
                Some(pa) => pa.edges(hash, false),
                None => self.segments.find(|b| b.rev_by_hash(hash)),
            };
            base.map(|v| self.without_unlinked(hash, v, false))
        })?;
        Some(self.drop_dangling(edges))
    }

    /// Hide edges whose far end has been deleted.
    ///
    /// An edge stored in the immutable base cannot be erased when one of its
    /// endpoints is tombstoned, so it is filtered here instead. Without this a
    /// traversal walked straight through deleted nodes — `SHORTEST` reported paths
    /// that no longer existed — and `edges_from_collection` returned edges into
    /// rows that were gone.
    ///
    /// The `is_empty` check keeps the common case free: with no pending deletes
    /// this is a length test and the borrowed slice is handed straight back.
    fn drop_dangling<'a>(
        &self,
        edges: std::borrow::Cow<'a, [Edge]>,
    ) -> std::borrow::Cow<'a, [Edge]> {
        let dead = |h: &u64| self.tombstones.contains(h) || self.edge_tombstones.contains(h);
        if self.tombstones.is_empty() && self.edge_tombstones.is_empty() {
            return edges;
        }
        if edges.iter().all(|e| !dead(&e.other)) {
            return edges;
        }
        std::borrow::Cow::Owned(
            edges.iter().filter(|e| !dead(&e.other)).cloned().collect(),
        )
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
        // Base edges carry a reference into whichever durable store produced them
        // (high bit set); overlay edges resolve through the resident meta store.
        if let Some(meta_ref) = edge.base_meta_ref() {
            // Paged adjacency owns the edges when it is on, so its references are
            // the only ones that can appear — a record id in its attribute store,
            // not an index into any segment's edgemeta.bin.
            if let Some(pa) = &self.paged_adj {
                let bytes = pa.meta_bytes(meta_ref)?;
                return serde_json::from_slice(&bytes).ok();
            }
            let bytes = self.segments
                .newest_first()
                .find_map(|b| b.edge_meta_bytes(meta_ref as u32))?;
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

    /// Read an edge's JSON meta by its exact attribute slot (recorded during
    /// traversal). Unlike `edge_locate`, this reads the SPECIFIC edge, so parallel
    /// edges resolve to their own attributes instead of the first match.
    pub(crate) fn edge_json_at(&self, slot: u32) -> Option<Value> {
        self.edges.json_at(slot)
    }

    /// Locate the stored forward edge `from → to` (type_hash `0` = any) and return
    /// its `(type_name, merged_attrs)` for graph-shaped output. `merged_attrs` is
    /// the fast-lane columns + JSON bag as one object (empty object if none).
    /// Used by [`crate::query::execute_match_graph`].
    pub(crate) fn edge_between(
        &self,
        from: u64,
        to: u64,
        edge_type_hash: u64,
    ) -> Option<(String, Value)> {
        let edges = self.fwd_edges(from)?;
        for e in edges.iter() {
            if e.other == to && (edge_type_hash == 0 || e.edge_type == edge_type_hash) {
                let ty = self
                    .edges
                    .type_name(e.edge_type)
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                let attrs = self
                    .edge_all_attrs(e)
                    .unwrap_or_else(|| Value::Object(Default::default()));
                return Some((ty, attrs));
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
        // Deleted base nodes are still listed in the base posting list, so a
        // tombstoned member has to be filtered out of every scan.
        if !self.tombstones.is_empty() {
            let mut merged: Vec<u64> = Vec::new();
            if let Some(b) = self.base_members(hash) {
                merged.extend(b);
            }
            if let Some(o) = self.collections.get(&hash) {
                let seen: HashSet<u64> = merged.iter().copied().collect();
                merged.extend(o.iter().copied().filter(|h| !seen.contains(h)));
            }
            if merged.is_empty() {
                return None;
            }
            merged.retain(|h| !self.tombstones.contains(h));
            return Some(std::borrow::Cow::Owned(merged));
        }
        let overlay = self.collections.get(&hash);
        let base = self.base_members(hash);
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

    /// Every member payload of `collection` (base + overlay), as JSON strings — the
    /// live-lock analogue of [`CoreDB::snapshot_db`]. Used by `Engine::scan` when
    /// snapshot reads are off. Unindexed: reads every member's payload.
    #[allow(dead_code)] // used by the `engine` feature (Engine::scan fallback)
    pub(crate) fn collection_payloads(&self, collection: &str) -> Vec<String> {
        self.collection_payloads_bounded(collection, None, None)
    }

    /// Bounded variant of [`CoreDB::collection_payloads`] — the live-lock analogue of
    /// [`CoreDB::snapshot_db`] (stops at `max_rows`/`max_bytes`).
    #[allow(dead_code)] // used by the `engine` feature (Engine::scan fallback)
    pub(crate) fn collection_payloads_bounded(
        &self,
        collection: &str,
        max_rows: Option<usize>,
        max_bytes: Option<usize>,
    ) -> Vec<String> {
        let members = match self.collection_members(sk_hash(collection)) {
            Some(m) => m,
            None => return Vec::new(),
        };
        let cap = max_rows.map_or(members.len(), |m| m.min(members.len()));
        let mut out = Vec::with_capacity(cap);
        let mut bytes = 0usize;
        for &h in members.iter() {
            if max_rows.is_some_and(|m| out.len() >= m) {
                break;
            }
            if let Some((off, len)) = self.payload_loc(h) {
                if max_bytes.is_some_and(|m| !out.is_empty() && bytes + len as usize > m) {
                    break;
                }
                if let Some(b) = self.payload_store.get_raw_of(h, off, len) {
                    bytes += b.len();
                    out.push(String::from_utf8_lossy(&b).into_owned());
                }
            }
        }
        out
    }

    /// Number of members of `collection` (base + overlay). Live-lock analogue of
    /// [`CoreDB::snapshot_db`].
    #[allow(dead_code)] // used by the `engine` feature (Engine::count fallback)
    pub(crate) fn collection_count(&self, collection: &str) -> usize {
        self.collection_members(sk_hash(collection)).map_or(0, |m| m.len())
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

    /// Base-aware btree index handle: the heap overlay first, then the mmap'd base
    /// (paged mode). All query paths should use this, not `field_index`, so a
    /// reopened paged DB serves indexed queries from the mmap.
    pub(crate) fn field_index_ref(&self, coll_hash: u64, field: &str) -> Option<FieldIndexRef<'_>> {
        if let Some(m) = self.field_indexes.get(&(coll_hash, field.to_string())) {
            return Some(FieldIndexRef::Heap(m));
        }
        self.field_base
            .get(&(coll_hash, field.to_string()))
            .map(FieldIndexRef::Mapped)
    }

    /// Pull a btree index out of the mmap base and into the heap so it can be
    /// written to.
    ///
    /// The two stores are consulted in preference order, not merged: `field_index_ref`
    /// returns the heap entry if there is one and the mapped base otherwise. On a
    /// paged reopen only the base exists, so a write updated nothing — the loop over
    /// `field_indexes` had no entry to touch — while reads kept answering from the
    /// stale mapped copy. `WHERE n > 15` then returned nothing for rows that were
    /// plainly there, and `MIN`/`MAX`/`SUM`/`ORDER BY` answered from the pre-write
    /// data, permanently: neither compaction nor reopening rebuilt it.
    ///
    /// Materialising on first write keeps the disk-first behaviour for databases that
    /// are only read, and makes the heap authoritative the moment one is written.
    fn ensure_field_index_writable(&mut self, coll_hash: u64, field: &str) {
        let key = (coll_hash, field.to_string());
        if self.field_indexes.contains_key(&key) {
            return;
        }
        let Some(base) = self.field_base.get(&key) else { return };
        let mut btree: std::collections::BTreeMap<FieldKey, Vec<u64>> =
            std::collections::BTreeMap::new();
        for (k, ids) in base.iter_kv(false) {
            btree.insert(k, ids);
        }
        self.field_indexes.insert(key, btree);
    }

    /// Whether a btree index exists for `(collection, field)` — heap or mmap base.
    pub(crate) fn has_field_index(&self, coll_hash: u64, field: &str) -> bool {
        let k = (coll_hash, field.to_string());
        self.field_indexes.contains_key(&k) || self.field_base.contains_key(&k)
    }

    /// Populate `field_base` by mmap'ing every `fieldidx_<coll>_<field>.bin` sidecar
    /// in `dir` (written by `compact`). Filenames carry the hex-encoded field name
    /// so the `(collection, field)` key round-trips. Used on paged open.
    fn load_field_base(&mut self, dir: &Path) -> io::Result<()> {
        let rd = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => return Ok(()),
        };
        for entry in rd.flatten() {
            let name = entry.file_name();
            let name = match name.to_str() {
                Some(n) => n,
                None => continue,
            };
            let stem = match name.strip_prefix("fieldidx_").and_then(|s| s.strip_suffix(".bin")) {
                Some(s) => s,
                None => continue,
            };
            // stem = "<coll_hash>_<hexfield>"
            let (coll_str, hex_field) = match stem.split_once('_') {
                Some(p) => p,
                None => continue,
            };
            let coll_hash: u64 = match coll_str.parse() {
                Ok(h) => h,
                Err(_) => continue,
            };
            let field = match hex_decode(hex_field) {
                Some(f) => f,
                None => continue,
            };
            if let Some(store) = storage::fieldstore::MappedFieldStore::open_disk(&entry.path())? {
                self.field_base.insert((coll_hash, field), store);
            }
        }
        Ok(())
    }

    /// Convert a `FieldKey` to a `serde_json::Value` for result projection.
    pub(crate) fn field_key_to_value(key: &FieldKey) -> Value {
        match key {
            FieldKey::Null        => Value::Null,
            FieldKey::Bool(b)     => Value::Bool(*b),
            // A whole number comes back whole. The index stores every number as
            // an `f64` because that is what makes them one ordered key space, but
            // rendering the key that way put the float back into the *answer*:
            // `GROUP BY n` returned `1.0` where the row holds `1`, and `SUM(n)`
            // over an indexed integer column came out `6.0`. The same query
            // without an index returned `1` and `6`, so whether a value looked
            // like an integer depended on whether somebody had run CREATE INDEX.
            //
            // Past 2^53 an `f64` can no longer tell consecutive integers apart,
            // so above that the float is kept rather than printing a whole number
            // that is not the value.
            FieldKey::Number(OrdF64(f)) => {
                if f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_992.0 {
                    Value::Number(serde_json::Number::from(*f as i64))
                } else {
                    serde_json::Number::from_f64(*f)
                        .map(Value::Number)
                        .unwrap_or(Value::Null)
                }
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

    /// All `(hash, SpatialMeta)` across the resident overlay and the mmap base
    /// (paged). Shared by grid rebuild and the compact-time grid serialization.
    fn all_spatial_items(&self) -> Vec<(u64, geo::SpatialMeta)> {
        let mut items: Vec<(u64, geo::SpatialMeta)> = self.nodes.iter()
            .filter_map(|(&hash, node)| node.spatial_meta.clone().map(|m| (hash, *m)))
            .collect();
        // Nodes already written down are not in `self.nodes` — that map is the
        // overlay. This used to reach into `segments` itself, which is how paged
        // nodes ended up with an empty spatial index while every other read of them
        // worked: the accessor exists so a backend is chosen once, and a call site
        // that goes around it is a backend nobody chose.
        for (h, m) in self.base_spatial_items() {
            // Skip deleted rows, or the grid keeps matching geometry for rows that
            // are gone, and rows the overlay holds a newer version of.
            if self.tombstones.contains(&h) || self.nodes.contains_key(&h) { continue }
            items.push((h, m));
        }
        items
    }

    fn rebuild_spatial_grid(&mut self) {
        let items = self.all_spatial_items();
        // Polygon-ring caching is mode-dependent:
        //  - Resident (heap) opens eagerly cache every polygon's rings, trading
        //    RAM for fast PIP / ST_DWithin refinement (RAM-rich servers).
        //  - Paged (edge/bounded) opens SKIP this — parsing all geometry costs
        //    O(total geometry) resident RAM (~360 MB for 7k complex polygons) and
        //    defeats bounded serving. Rings load on demand in the query path; open
        //    stays O(1) RAM regardless of geometry size.
        let eager = self.segments.is_empty();
        let polys: Vec<(u64, Vec<Vec<[f64; 2]>>)> = if eager {
            items.iter()
                .filter(|(_, m)| m.bbox_min_lat != m.bbox_max_lat || m.bbox_min_lon != m.bbox_max_lon)
                .filter_map(|(h, _)| {
                    let rings = geo::rings_from_payload(&self.get_payload(*h)?);
                    (!rings.is_empty()).then_some((*h, rings))
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut grid = geo::SpatialGrid::build(items.into_iter());
        for (h, rings) in polys { grid.cache_rings(h, rings); }
        self.spatial_grid = Some(grid);
    }

    /// Paged (disk-first) open: serve the spatial grid from the mmap'd
    /// `spatialgrid.bin` instead of rebuilding it resident. Post-compact overlay
    /// writes (WAL-replayed geometry nodes not in the base) are folded into the
    /// resident overlay so queries still see them. Returns false if unavailable.
    fn attach_spatial_base(&mut self, dir: &Path) -> bool {
        let path = dir.join("spatialgrid.bin");
        let base = match storage::spatialstore::MappedSpatialGrid::open_disk(&path) {
            Ok(Some(b)) => b,
            _ => return false,
        };
        let mut grid = geo::SpatialGrid::from_mapped(base);
        for (&h, node) in &self.nodes {
            if let Some(m) = &node.spatial_meta {
                if !grid.base_contains(h) {
                    grid.insert(h, (**m).clone());
                }
            }
        }
        self.spatial_grid = Some(grid);
        true
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

        // Base-aware — see build_bm25_index.
        for hash in self.all_hashes() {
            if let Some(payload) = self.get_payload(hash) {
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
        // Base-aware — see build_bm25_index.
        let owned: Vec<(u64, String)> = self
            .all_hashes()
            .into_iter()
            .filter_map(|hash| {
                let payload = self.get_payload(hash)?;
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
            // Exclude nodes deleted since the index was built. Base-aware: in paged
            // mode the live nodes are in the mmap base, not self.nodes.
            .filter(|h| self.node_exists(*h))
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
        // Base-aware: `self.nodes` is only the write overlay in paged mode, so
        // enumerating it would rebuild an index covering just the recent writes and
        // silently drop every base-resident document from text search.
        let owned: Vec<(u64, String)> = self
            .all_hashes()
            .into_iter()
            .filter_map(|hash| {
                let payload = self.get_payload(hash)?;
                payload.get(field)?.as_str().map(|s| (hash, s.to_string()))
            })
            .collect();
        let refs: Vec<(u64, &str)> = owned.iter().map(|(h, s)| (*h, s.as_str())).collect();
        let mut index = bm25::Bm25Index::build(field, refs.into_iter());
        #[cfg(unix)]
        if let Some(ref dir) = self.data_dir {
            let _ = index.spill_to_disk(&dir.join(format!("bm25_{field}.postings")));
        }
        self.bm25_indexes.insert(field.to_string(), index);
        self.record_index_version("bm25", field, BM25_INDEX_VERSION);
        // Persist metadata (dict + doc arrays) so a paged reopen can mmap it instead
        // of rebuilding. Postings are already spilled above.
        if let Some(dir) = self.data_dir.clone() {
            let _ = self.save_bm25_binary(&dir.join("bm25.bin"));
        }
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
                    // Belt-and-suspenders: exclude any doc not present in the live
                    // node set, covering the window between deletion and index update.
                    // Base-aware: in paged mode the live nodes are in the mmap base.
                    .filter(|hit| self.node_exists(hit.doc_id))
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
        if self.read_only {
            use serde::ser::Error as _;
            return Err(serde_json::Error::custom(self.refuse_write("vector write").to_string()));
        }
        self.wal_write(WalEntry::PutVector {
            slug: slug.to_string(),
            field: field.to_string(),
            data: data.to_vec(),
        });
        let hash = sk_hash(slug);
        self.ensure_vector_store(field);
        self.vectors.get_mut(field).unwrap().put(hash, data.to_vec())
            .map_err(|e| { use serde::ser::Error as _; serde_json::Error::custom(e.to_string()) })?;
        let hnsw_declared = self.schemas.values()
            .any(|s| s.indexes.vector.contains(&field.to_string()));
        if hnsw_declared {
            #[cfg(unix)]
            self.vectors.get_mut(field).unwrap().remap();
            use crate::query::VecMetric;
            use vector::{CosineDistance, DotProduct, L1Distance, L2Distance};
            let (m, ef) = self.hnsw_params.get(field).copied().unwrap_or((16, 200));
            let metric = self.hnsw_metric(field);
            let field_vecs = self.vectors.get(field).unwrap();
            let graph = self.hnsw_indexes
                .entry(field.to_string())
                .or_insert_with(|| vector::HnswGraph::empty(m));
            match metric {
                VecMetric::Cosine => graph.insert::<CosineDistance, _>(hash, field_vecs, ef),
                VecMetric::L2     => graph.insert::<L2Distance, _>(hash, field_vecs, ef),
                VecMetric::Dot    => graph.insert::<DotProduct, _>(hash, field_vecs, ef),
                VecMetric::L1     => graph.insert::<L1Distance, _>(hash, field_vecs, ef),
            }
        }
        Ok(hash)
    }

    /// Retrieve the stored vector for a node under a named field.
    ///
    /// Returns `None` if the node has no vector for that field.
    ///
    /// Zero-copy when the vector is inside the store's mmap window; falls back
    /// to a positional read for data appended after the last remap (a disk
    /// store only refreshes its mmap on open/compact/index-build, so a
    /// write-then-read must not depend on it).
    pub fn get_vector(&self, slug: &str, field: &str) -> Option<Vec<f32>> {
        let hash = sk_hash(slug);
        use crate::vector::VectorAccess;
        let store = self.vectors.get(field)?;
        if let Some(v) = store.get(hash) {
            return Some(v.to_vec());
        }
        store.get_owned(hash)
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
        // Base-aware: merge heap overlay + mmap topology base. Reading only
        // self.collections/self.nodes misses every base node on a reopened paged
        // DB, which silently builds an EMPTY index (all indexed queries return 0).
        let members: Vec<u64> = self
            .collection_members(coll_hash)
            .map(|m| m.into_owned())
            .unwrap_or_default();
        let mut btree: BTreeMap<FieldKey, Vec<u64>> = BTreeMap::new();
        for hash in members {
            if let Some(payload) = self.get_payload(hash) {
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

    /// Seed a `Collection` step by INTERSECTING btree range scans over two or more
    /// distinct indexed fields — the fast path for an axis-aligned box query such as
    /// `WHERE lon BETWEEN … AND lat BETWEEN …`.
    ///
    /// Single-field `btree_seed` only indexes one axis, then reads a payload per
    /// candidate to filter the other axis — O(strip) payload reads. Here every axis
    /// with a btree becomes a `range_postings` scan; we intersect the posting sets
    /// (smallest first) entirely in the index, doing ZERO payload reads. Returns the
    /// intersected candidates plus every consumed step index, or `None` when fewer
    /// than two indexed fields carry a range (nothing to intersect → let the normal
    /// single-field seed handle it).
    pub(crate) fn btree_multi_range_seed(
        &self,
        coll_hash: u64,
        remaining: &[Step],
    ) -> Option<(Vec<u64>, Vec<usize>)> {
        use std::ops::Bound;
        // Per-field accumulated (lower, upper) bound and the step indices that set them.
        struct R {
            field: String,
            lo: Bound<FieldKey>,
            hi: Bound<FieldKey>,
            steps: Vec<usize>,
        }
        let mut ranges: Vec<R> = Vec::new();
        let mut touch = |field: &str, lo: Option<Bound<FieldKey>>, hi: Option<Bound<FieldKey>>, j: usize| {
            let slot = match ranges.iter_mut().find(|r| r.field == field) {
                Some(r) => r,
                None => {
                    ranges.push(R { field: field.to_string(), lo: Bound::Unbounded, hi: Bound::Unbounded, steps: Vec::new() });
                    ranges.last_mut().unwrap()
                }
            };
            if let Some(b) = lo { slot.lo = b; }
            if let Some(b) = hi { slot.hi = b; }
            slot.steps.push(j);
        };
        for (j, step) in remaining.iter().enumerate() {
            match step {
                Step::WhereGte(f, v) => touch(f, Some(Bound::Included(FieldKey::from_f64(*v))), None, j),
                Step::WhereGt(f, v)  => touch(f, Some(Bound::Excluded(FieldKey::from_f64(*v))), None, j),
                Step::WhereLte(f, v) => touch(f, None, Some(Bound::Included(FieldKey::from_f64(*v))), j),
                Step::WhereLt(f, v)  => touch(f, None, Some(Bound::Excluded(FieldKey::from_f64(*v))), j),
                Step::WhereBetween(f, lo, hi) =>
                    touch(f, Some(Bound::Included(FieldKey::from_f64(*lo))), Some(Bound::Included(FieldKey::from_f64(*hi))), j),
                // Other row-preserving filters on the SAME collection are fine to scan
                // past (we just don't consume them) — the ranges we collect still all
                // apply to this collection's nodes with AND semantics.
                Step::WhereEq(..) | Step::WhereNeq(..) | Step::WhereIn(..) | Step::Like(..)
                | Step::WhereIsNull(..) | Step::ArrayContains(..) | Step::WhereNot(..)
                | Step::WhereOr(..) => {}
                // Anything else (graph traversal, GROUP BY, SORT, SELECT, set algebra,
                // spatial/vector/search steps…) changes or reshapes the node-set. Ranges
                // after such a boundary may apply to DIFFERENT nodes (e.g. the atomic API
                // `.where_gte(a).forward(e).where_lte(b)`), so stop here — never intersect
                // across it. This keeps the seed correct for every query shape.
                _ => break,
            }
        }
        // Keep only fields that actually have a btree index on this collection.
        ranges.retain(|r| self.has_field_index(coll_hash, &r.field));
        if ranges.len() < 2 {
            return None;
        }
        // Scan each field's range; intersect smallest posting set first.
        let mut postings: Vec<Vec<u64>> = ranges
            .iter()
            .filter_map(|r| {
                let idx = self.field_index_ref(coll_hash, &r.field)?;
                Some(idx.range_postings(r.lo.as_ref(), r.hi.as_ref()))
            })
            .collect();
        if postings.len() != ranges.len() {
            return None; // an index vanished under us — bail to a safe path
        }
        postings.sort_by_key(|p| p.len());
        // Build ONE hash set from the smallest range, then probe it while streaming
        // each larger range — avoids materializing a hash set for the big sides.
        let mut acc: std::collections::HashSet<u64> = postings[0].iter().copied().collect();
        for p in &postings[1..] {
            acc = p.iter().copied().filter(|h| acc.contains(h)).collect();
            if acc.is_empty() {
                break;
            }
        }
        let skips: Vec<usize> = ranges.iter().flat_map(|r| r.steps.iter().copied()).collect();
        Some((acc.into_iter().collect(), skips))
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
                // `x = NULL` and `x != NULL` are unknown for every row, so both
                // return nothing. Handled before the index is consulted, because
                // the index would happily answer `= NULL` with the rows whose
                // field is missing — a different question, asked by `IS NULL`.
                Step::WhereEq(_, value) | Step::WhereNeq(_, value) if value.is_null() => {
                    return Some((Vec::new(), j, None));
                }
                Step::WhereEq(field, value) => {
                    if let Some(idx) = self.field_index_ref(coll_hash, field) {
                        if let Some(fk) = FieldKey::from_json(value) {
                            let ids = idx.get_eq(&fk).map(|c| c.into_owned()).unwrap_or_default();
                            return Some((ids, j, None));
                        }
                    }
                }
                Step::WhereNeq(field, value) => {
                    if let Some(idx) = self.field_index_ref(coll_hash, field) {
                        if let Some(fk) = FieldKey::from_json(value) {
                            // Set-difference: all collection members minus those
                            // matching the value — and minus the ones that hold
                            // NULL, which includes every row where the field is
                            // absent, since that is how a missing field is
                            // indexed. `!=` is not true for those rows, it is
                            // unknown, and unknown does not pass a filter. The
                            // scan drops them too; an index that kept them would
                            // make the answer depend on whether a column happened
                            // to be indexed.
                            let mut excluded: std::collections::HashSet<u64> = idx
                                .get_eq(&fk)
                                .map(|ids| ids.iter().copied().collect())
                                .unwrap_or_default();
                            if let Some(nulls) = idx.get_eq(&FieldKey::Null) {
                                excluded.extend(nulls.iter().copied());
                            }
                            let all = self
                                .collection_members(coll_hash)
                                .map(|c| c.into_owned())
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
                    if let Some(idx) = self.field_index_ref(coll_hash, field) {
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
                        let lo_b = Bound::Excluded(fk_lo);
                        return if let Some((pair_j, upper_bound)) = upper {
                            Some((idx.range_postings(lo_b.as_ref(), upper_bound.as_ref()), j, Some(pair_j)))
                        } else {
                            Some((idx.range_postings(
                                lo_b.as_ref(),
                                Bound::Excluded(&FieldKey::numbers_end())), j, None))
                        };
                    }
                }
                Step::WhereLt(field, hi) => {
                    if let Some(idx) = self.field_index_ref(coll_hash, field) {
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
                        let hi_b = Bound::Excluded(fk_hi);
                        return if let Some((pair_j, lower_bound)) = lower {
                            Some((idx.range_postings(lower_bound.as_ref(), hi_b.as_ref()), j, Some(pair_j)))
                        } else {
                            Some((idx.range_postings(
                                Bound::Excluded(&FieldKey::numbers_start()),
                                hi_b.as_ref()), j, None))
                        };
                    }
                }
                Step::WhereGte(field, lo) => {
                    if let Some(idx) = self.field_index_ref(coll_hash, field) {
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
                        let lo_b = Bound::Included(fk_lo);
                        return if let Some((pair_j, upper_bound)) = upper {
                            Some((idx.range_postings(lo_b.as_ref(), upper_bound.as_ref()), j, Some(pair_j)))
                        } else {
                            Some((idx.range_postings(
                                lo_b.as_ref(),
                                Bound::Excluded(&FieldKey::numbers_end())), j, None))
                        };
                    }
                }
                Step::WhereLte(field, hi) => {
                    if let Some(idx) = self.field_index_ref(coll_hash, field) {
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
                        let hi_b = Bound::Included(fk_hi);
                        return if let Some((pair_j, lower_bound)) = lower {
                            Some((idx.range_postings(lower_bound.as_ref(), hi_b.as_ref()), j, Some(pair_j)))
                        } else {
                            Some((idx.range_postings(
                                Bound::Excluded(&FieldKey::numbers_start()),
                                hi_b.as_ref()), j, None))
                        };
                    }
                }
                Step::WhereBetween(field, lo, hi) => {
                    if let Some(idx) = self.field_index_ref(coll_hash, field) {
                        let fk_lo = FieldKey::from_f64(*lo);
                        let fk_hi = FieldKey::from_f64(*hi);
                        return Some((
                            idx.range_postings(Bound::Included(&fk_lo), Bound::Included(&fk_hi)),
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

        let idx = self.field_index_ref(coll_hash, field)?;

        // Look ahead for a Take limit — enables O(k) extraction instead of O(N).
        //
        // The bound is OFFSET + LIMIT, not LIMIT. This took the LIMIT alone, so a
        // query with an offset seeded exactly `limit` rows and the `Skip` that ran
        // afterwards drained from those: `ORDER BY v LIMIT 5 OFFSET 3` returned two
        // rows with an index on `v` and five without one. An answer that depends on
        // whether an index happens to exist is the worst shape a query bug takes,
        // because nothing in the query says which path it took.
        //
        // A `Distinct` between here and the Take rules the shortcut out entirely:
        // rows that dedup away were still produced, so no count taken before the
        // dedup can bound what comes after it.
        let tail = &remaining[sort_pos + 1..];
        if tail.iter().any(|s| matches!(s, Step::Distinct)) {
            return None;
        }
        let skip_n: usize = tail.iter()
            .filter_map(|s| if let Step::Skip(n) = s { Some(*n) } else { None })
            .sum();
        let take_n = tail
            .iter()
            .find_map(|s| if let Step::Take(n) = s { Some(n.saturating_add(skip_n)) } else { None });

        // `iter_kv_sql_order`, not `iter_kv`. This seed does not merely order the
        // rows — it tells the executor to **skip the Sort step**, so whatever
        // order comes out of here is the answer, with nothing downstream to
        // correct it. Walking the btree raw puts NULLs first on `ASC`, because
        // `FieldKey::Null` is its lowest key, and a missing field is stored as
        // NULL. `ORDER BY b ASC` therefore led with every row that has no `b`,
        // and the same query without an index led with the smallest value.
        let result: Vec<u64> = crate::query::iter_kv_sql_order(&idx, *asc)
            .into_iter()
            .flat_map(|(_, ids)| ids)
            .collect();

        let candidates = match take_n {
            Some(n) => result.into_iter().take(n).collect(),
            None => result,
        };
        Some((candidates, sort_pos))
    }

    /// Fast-path for `Collection … ORDER BY ST_Distance(field, POINT) ASC LIMIT k`:
    /// return the k nearest nodes of this collection via the spatial grid (avoiding the
    /// O(N) per-row distance scan + sort), plus the step indices to skip (Sort + Take).
    /// Applies only when there are no filters/traversals — a pure kNN.
    pub(crate) fn spatial_knn_seed(&self, coll_hash: u64, remaining: &[Step]) -> Option<(Vec<u64>, Vec<usize>)> {
        let (mut sort_i, mut take_i, mut k) = (None, None, 0usize);
        let (mut lat, mut lon) = (0.0f64, 0.0f64);
        for (j, s) in remaining.iter().enumerate() {
            match s {
                // Match either ascending value — kNN is nearest-first by construction.
                Step::SortByExpr { expr: crate::query::ScoreExpr::StDistance { lat: la, lon: lo, .. }, .. } => {
                    sort_i = Some(j); lat = *la; lon = *lo;
                }
                Step::Take(n) => { take_i = Some(j); k = *n; }
                Step::Select(_) | Step::ScoreProject(_) | Step::Skip(_) | Step::Distinct => {}
                _ => return None, // any filter/traversal → not a pure kNN
            }
        }
        let (si, ti) = (sort_i?, take_i?);
        if k == 0 { return None; }
        let grid = self.spatial_grid()?;
        let total = grid.len();
        let mut fetch = k;
        loop {
            let cand = grid.k_nearest(lat, lon, fetch);
            let filtered: Vec<u64> = cand.into_iter()
                .filter(|&h| self.node_data(h).map(|n| sk_hash(&n.collection)) == Some(coll_hash))
                .collect();
            if filtered.len() >= k || fetch >= total {
                return Some((filtered.into_iter().take(k).collect(), vec![si, ti]));
            }
            fetch = (fetch * 4).min(total.max(k));
        }
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
        self.build_hnsw_index_metric(field, m, ef_construction, crate::query::VecMetric::Cosine)
    }

    /// Build (or rebuild) an HNSW ANN index with an explicit distance `metric`.
    pub fn build_hnsw_index_metric(
        &mut self,
        field: &str,
        m: usize,
        ef_construction: usize,
        metric: crate::query::VecMetric,
    ) -> Result<(), String> {
        use crate::query::VecMetric;
        use vector::{CosineDistance, DotProduct, L1Distance, L2Distance};
        // Ensure mmap covers any recently-appended vectors.
        #[cfg(unix)]
        if let Some(store) = self.vectors.get_mut(field) {
            store.remap();
        }
        let field_vecs = self
            .vectors
            .get(field)
            .ok_or_else(|| format!("no vectors stored for field '{field}'"))?;

        // Contiguous snapshot + DENSE-id build → no HashMap probes / pointer-chases.
        let dense = vector::DenseVectors::snapshot(field_vecs);
        let (flat, dim, ids) = dense.parts();

        // Build entirely into a local — zero writes to self until this line.
        let graph = match metric {
            VecMetric::Cosine => vector::HnswGraph::build_dense_parallel::<CosineDistance>(flat, dim, ids, m, ef_construction),
            VecMetric::L2     => vector::HnswGraph::build_dense_parallel::<L2Distance>(flat, dim, ids, m, ef_construction),
            VecMetric::Dot    => vector::HnswGraph::build_dense_parallel::<DotProduct>(flat, dim, ids, m, ef_construction),
            VecMetric::L1     => vector::HnswGraph::build_dense_parallel::<L1Distance>(flat, dim, ids, m, ef_construction),
        };

        // Atomic replace: old index (if any) is dropped here.
        self.hnsw_indexes.insert(field.to_string(), graph);
        self.hnsw_params.insert(field.to_string(), (m, ef_construction));
        self.hnsw_metric.insert(field.to_string(), metric);
        Ok(())
    }

    pub fn build_hnsw_index_disk(
        &mut self,
        field: &str,
        m: usize,
        ef_construction: usize,
        metric: crate::query::VecMetric,
    ) -> Result<(), String> {
        use crate::query::VecMetric;
        use vector::L2Distance;
        if !matches!(metric, VecMetric::L2) {
            return Err("disk-first int8 index currently supports L2 only".into());
        }
        #[cfg(unix)]
        if let Some(store) = self.vectors.get_mut(field) { store.remap(); }
        let compact = {
            let field_vecs = self.vectors.get(field)
                .ok_or_else(|| format!("no vectors stored for field '{field}'"))?;
            if !field_vecs.is_disk() {
                return Err("disk-first index needs a disk-backed store (open a data directory)".into());
            }
            let dense = vector::DenseVectors::snapshot(field_vecs);
            let (flat, dim, ids) = dense.parts();
            let graph = vector::HnswGraph::build_dense_parallel::<L2Distance>(flat, dim, ids, m, ef_construction);
            let mut sample: Vec<f32> = Vec::with_capacity(200_000);
            let stride = (flat.len() / 200_000).max(1);
            let mut i = 0;
            while i < flat.len() { sample.push(flat[i]); i += stride; }
            let quantizer = vector::ScalarQuantizer::calibrate(&mut sample);
            let mut qf = vector::QuantizedField::with_capacity(quantizer, dim, ids.len());
            for (chunk_idx, &id) in ids.iter().enumerate() {
                let off = chunk_idx * dim;
                qf.insert(id, &flat[off..off + dim]);
            }
            vector::CompactDiskIndex::from_hnsw(&graph, &qf, dim)
        };
        self.compact_indexes.insert(field.to_string(), compact);
        self.hnsw_params.insert(field.to_string(), (m, ef_construction));
        self.hnsw_metric.insert(field.to_string(), metric);
        // Persist the compact index so a paged reopen can mmap it (disk-first)
        // rather than rebuilding the graph resident. Written here (not only at
        // compact) because building an index doesn't itself trigger a compaction.
        if let Some(dir) = self.data_dir.clone() {
            let _ = self.save_vector_binary(&dir.join("vecidx.bin"));
        }
        #[cfg(unix)]
        if let Some(store) = self.vectors.get_mut(field) { store.drop_mmap(); }
        #[cfg(target_os = "linux")]
        { extern "C" { fn malloc_trim(pad: usize) -> std::os::raw::c_int; } unsafe { malloc_trim(0); } }
        Ok(())
    }

    pub(crate) fn quant_field(&self, field: &str) -> Option<&vector::QuantizedField> {
        self.quant_fields.get(field)
    }

    #[cfg(unix)]
    pub fn spill_edges_to_disk(&mut self) -> std::io::Result<()> {
        if let Some(dir) = self.data_dir.clone() { self.edges.spill_to_disk(&dir)?; }
        Ok(())
    }

    pub(crate) fn compact_index(&self, field: &str) -> Option<&vector::CompactDiskIndex> {
        self.compact_indexes.get(field)
    }

    pub fn memory_report(&self) -> Vec<(&'static str, usize)> {
        use std::mem::size_of;
        let node_map = self.nodes.capacity() * (8 + size_of::<NodeData>());
        let node_str: usize = self.nodes.values().map(|n| n.slug.capacity() + n.collection.capacity()).sum();
        let colls = self.collections.capacity() * (8 + 24) + self.collections.values().map(|v| v.capacity() * 8).sum::<usize>();
        let vec_store: usize = self.vectors.values().map(|v| v.mem_bytes()).sum();
        let graph: usize = self.hnsw_indexes.values().map(|g| g.mem_bytes()).sum();
        let int8: usize = self.quant_fields.values().map(|q| q.mem_bytes()).sum();
        let compact: usize = self.compact_indexes.values().map(|c| c.mem_bytes()).sum();
        vec![
            ("nodes.map (NodeData inline)", node_map),
            ("nodes.strings (slug+collection heap)", node_str),
            ("collections", colls),
            ("vector_store (id index + mmap)", vec_store),
            ("hnsw_graph (fat)", graph),
            ("int8_codes (fat)", int8),
            ("compact_index (disk-first CSR)", compact),
            ("bm25_index (in-RAM postings)", self.bm25_indexes.values().map(|b| b.mem_bytes()).sum()),
            ("edge_adjacency", self.edges.adjacency_mem_bytes()),
            ("_sizeof NodeData", size_of::<NodeData>()),
        ]
    }

    /// The distance metric an HNSW index was built with (Cosine if unset).
    pub(crate) fn hnsw_metric(&self, field: &str) -> crate::query::VecMetric {
        self.hnsw_metric.get(field).copied().unwrap_or(crate::query::VecMetric::Cosine)
    }

    pub fn set_hnsw_ef_search(&mut self, ef: Option<usize>) { self.hnsw_ef_search = ef; }

    /// Change the WAL durability level at runtime (see [`SyncMode`]). Under
    /// `Normal`/`Off`, individual writes skip the per-write fsync — the standard
    /// mobile trade-off. Durability is re-established at the next
    /// `compact()`/checkpoint. Switching back to `Full` fsyncs the pending WAL
    /// immediately so no already-acknowledged writes are left unsynced.
    pub fn set_wal_sync(&mut self, mode: SyncMode) {
        if mode == SyncMode::Full {
            if let Some(wal) = &mut self.wal {
                let _ = wal.sync();
            }
        }
        self.wal_sync = mode;
    }

    /// Change the auto-compaction policy at runtime (see [`AutoCompact`]).
    /// Mobile apps set `Manual` so a mutation burst never triggers an inline
    /// full compaction, then call [`compact`](Self::compact) at an idle moment
    /// (or rely on `compact_on_close`).
    pub fn set_auto_compact(&mut self, policy: AutoCompact) {
        self.auto_compact = policy;
    }
    pub(crate) fn hnsw_ef_search(&self) -> Option<usize> { self.hnsw_ef_search }

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
        // Base-aware: collection_members merges the mmap'd base with the overlay
        // (and drops tombstones); self.collections alone is overlay-only.
        let members: Vec<u64> = match self.collection_members(coll_hash) {
            Some(m) => m.into_owned(),
            None => return,
        };

        let docs = members.iter().filter_map(|&hash| {
            let node = self.node_data(hash)?;
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

    /// Disk-first (paged mode): mmap the `search.bin` container and serve each
    /// per-collection index's FST + postings from the map (`open_mapped`), leaving
    /// only scalars/norms/bitmaps resident. Mirrors `load_field_base`. Returns
    /// false on any problem so the caller falls back to a resident rebuild.
    fn load_search_base(&mut self, path: &std::path::Path) -> bool {
        use std::sync::Arc;
        let file = match std::fs::File::open(path) { Ok(f) => f, Err(_) => return false };
        let len = match file.metadata() { Ok(m) => m.len() as usize, Err(_) => return false };
        if len < 12 { return false; }
        let view = match storage::mmap::MmapView::try_new(&file, len) {
            Some(v) => Arc::new(v),
            None => return false,
        };
        let hdr = match view.slice(0, 12) { Some(h) => h, None => return false };
        if &hdr[..8] != b"SKSRCH01" { return false; }
        let count = u32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]) as usize;
        let mut pos = 12usize;
        let mut loaded = Vec::with_capacity(count);
        for _ in 0..count {
            let kl = match view.slice(pos, 2) { Some(b) => u16::from_le_bytes([b[0], b[1]]) as usize, None => return false };
            pos += 2;
            let key = match view.slice(pos, kl).and_then(|b| std::str::from_utf8(b).ok()) {
                Some(k) => k.to_string(),
                None => return false,
            };
            pos += kl;
            match search::SearchIndex::open_mapped(&view, pos) {
                Ok((idx, consumed)) => { pos += consumed; loaded.push((key, idx)); }
                Err(_) => return false,
            }
        }
        for (key, idx) in loaded {
            self.search_indexes.insert(key, idx);
        }
        true
    }

    fn load_search_binary(&mut self, path: &std::path::Path) -> bool {
        // In paged (disk-first) mode, mmap the container instead of reading it
        // into RAM. Fall through to the resident path if mmap serving fails.
        if !self.segments.is_empty() && self.load_search_base(path) {
            return true;
        }
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
                TxnOp::Put(slug, json) => {
                    let payload: Value = serde_json::from_str(json)?;
                    self.db.put_raw_indexed(slug, json, payload)?;
                }
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
                    if let Err(e) = self.db.vectors.get_mut(field).unwrap().put(hash, data.clone()) {
                        self.db.note_write_error(format!("vector write failed: {e}"));
                    }
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
        // A committed transaction is a mutation like any other: it has to emit its
        // change events and count towards auto-compaction. Skipping this meant a
        // subscriber never heard about anything written in a transaction, and a
        // workload that only ever wrote in transactions never compacted.
        self.db.after_mutation();
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
        let payload = self.payload_store.get_of(hash, off, len)?;
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
    #[serde(default)] // Cosine for pre-metric snapshots
    metric: crate::query::VecMetric,
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
mod payload_paging_tests {
    use super::*;

    fn rec(i: usize) -> Vec<u8> {
        format!("{{\"_key\":\"n{i}\",\"name\":\"record {i} west java\",\"n\":{i}}}")
            .into_bytes()
    }

    /// The paged store must return exactly what the append-only one does. If the
    /// two ever disagree, the paged path is silently corrupting payloads.
    #[test]
    fn paged_payloads_read_back_the_same_as_flat_ones() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut flat = PayloadStore::open_file(&dir.path().join("flat.bin")).unwrap();
        let mut paged = PayloadStore::open_paged(&dir.path().join("paged.bin")).unwrap();

        let sizes: Vec<Vec<u8>> = (0..300).map(rec)
            .chain(std::iter::once(vec![b'x'; 200_000]))   // spans many pages
            .chain(std::iter::once(vec![b'y'; 4096]))      // straddles the boundary
            .chain(std::iter::once(Vec::new()))            // empty
            .collect();

        for bytes in &sizes {
            let (fo, fl) = flat.append(bytes).unwrap();
            let (po, pl) = paged.append(bytes).unwrap();
            assert_eq!(fl, pl, "lengths disagree");
            assert_eq!(
                flat.get_raw(fo, fl).as_deref(),
                paged.get_raw(po, pl).as_deref(),
                "paged and flat disagree on a {}-byte record", bytes.len(),
            );
        }
    }

    /// The whole point: freeing a record returns its space, so a workload that
    /// deletes as fast as it writes stops growing the file. The flat store cannot
    /// do this at all — that is what compaction exists to work around.
    #[test]
    fn freeing_a_paged_payload_returns_its_space() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut p = PayloadStore::open_paged(&dir.path().join("paged.bin")).unwrap();

        let mut live: Vec<(u64, u32)> = (0..400).map(|i| p.append(&rec(i)).unwrap()).collect();
        let settled = p.page_stats().unwrap().0;

        // Churn: free the oldest, write a new one, far more times than there are
        // records, so any failure to reclaim shows up as unbounded growth.
        for i in 0..4000 {
            let (off, _) = live.remove(0);
            assert!(p.free(off), "free reported nothing reclaimed");
            live.push(p.append(&rec(10_000 + i)).unwrap());
        }
        let after = p.page_stats().unwrap().0;
        assert!(after <= settled + 4,
                "file grew from {settled} to {after} pages over 4000 replacements — \
                 space is not coming back");

        // Everything still live must still read.
        for (off, len) in &live {
            assert!(p.get_raw(*off, *len).is_some(), "a live record stopped reading");
        }
    }

    #[test]
    fn a_freed_paged_payload_stops_reading() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut p = PayloadStore::open_paged(&dir.path().join("paged.bin")).unwrap();
        let (a, al) = p.append(b"alpha").unwrap();
        let (b, bl) = p.append(b"bravo").unwrap();
        assert!(p.free(a));
        assert_eq!(p.get_raw(a, al), None, "a freed payload still reads");
        assert_eq!(p.get_raw(b, bl).as_deref(), Some(&b"bravo"[..]), "neighbour disturbed");
    }

    /// Byte-offset arithmetic is meaningless once offsets are record ids, so those
    /// paths must decline rather than return whatever bytes are at that position.
    #[test]
    fn paged_stores_decline_absolute_offset_reads() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut p = PayloadStore::open_paged(&dir.path().join("paged.bin")).unwrap();
        let flat = PayloadStore::open_file(&dir.path().join("flat.bin")).unwrap();
        let (off, _) = p.append(b"hello world").unwrap();

        assert!(!p.absolute_offsets(), "paged stores must not claim byte offsets");
        assert!(flat.absolute_offsets(), "flat stores do have byte offsets");
        assert_eq!(p.get_raw_at(off, 4), None, "a paged store answered a byte-offset read");
    }
}

#[cfg(test)]
mod compaction_safety_tests {
    use super::*;
    use serde_json::json;

    /// The post-compaction check must count the store as it really is on disk.
    ///
    /// This is the property the whole safety mechanism rests on. The previous guard
    /// rail counted through the same accessors that had written the data, so when
    /// those were wrong it agreed with them and waved through a compaction that had
    /// just deleted every edge. This counts by re-opening the files, so it can only
    /// agree with what is actually stored.
    #[test]
    fn the_on_disk_count_matches_what_was_written() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
        for i in 0..50 {
            db.put(&format!("p/n{i}"),
                   &json!({"_collection":"p","_key":format!("n{i}")}).to_string()).unwrap();
        }
        for i in 0..49 {
            db.link(&format!("p/n{i}"), &format!("p/n{}", i + 1), "next");
        }
        db.compact().unwrap();

        let (nodes, edges) = CoreDB::count_generation_on_disk(dir.path(), None, None).unwrap();
        assert_eq!(nodes, 50, "the check under-counts nodes and would wave loss through");
        assert_eq!(edges, 49, "the check under-counts edges and would wave loss through");
    }

    /// Restoring the parked generation must bring the exact bytes back.
    #[test]
    fn a_parked_generation_can_be_put_back() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
        for i in 0..20 {
            db.put(&format!("p/n{i}"),
                   &json!({"_collection":"p","_key":format!("n{i}")}).to_string()).unwrap();
        }
        for i in 0..19 {
            db.link(&format!("p/n{i}"), &format!("p/n{}", i + 1), "next");
        }
        db.compact().unwrap();
        drop(db);

        let before = CoreDB::count_generation_on_disk(dir.path(), None, None).unwrap();
        assert_eq!(before, (20, 19));

        // Park it, then destroy the live files the way a bad compaction would.
        let staged = CoreDB::stage_previous_generation(dir.path());
        assert!(!staged.is_empty(), "nothing was parked");
        // Damage them the way compaction replaces files: write a new file and
        // rename it over the old name. That is what leaves the parked copy intact —
        // rename swings the directory entry to a NEW inode while the parked link
        // still points at the old one. Truncating in place would destroy both,
        // which is exactly why the base files must only ever be replaced this way.
        for name in CoreDB::BASE_FILES {
            let p = dir.path().join(name);
            if p.exists() {
                let t = dir.path().join(format!("{name}.damaged"));
                std::fs::write(&t, b"").unwrap();
                std::fs::rename(&t, &p).unwrap();
            }
        }
        assert_ne!(
            CoreDB::count_generation_on_disk(dir.path(), None, None).unwrap_or((0, 0)), before,
            "the damage was not even detectable, so the check proves nothing",
        );

        CoreDB::restore_previous_generation(&staged);
        assert_eq!(
            CoreDB::count_generation_on_disk(dir.path(), None, None).unwrap(), before,
            "restoring the parked generation did not bring the data back",
        );

        // And the database itself opens with everything intact.
        let db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
        assert_eq!(db.node_count(), 20);
        assert_eq!(
            db.query("SELECT _key FROM MATCH (a:p)-[:next]->(b:p)").unwrap().collect().len(), 19,
        );
    }
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
            let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
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
        assert!(!paged.segments.is_empty(), "paged open must attach a mapped segment");
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

    #[cfg(unix)]
    #[test]
    fn snapshot_is_isolated_from_later_writes() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
            db.put("tourist/chloe", r#"{"_collection":"tourist","_key":"chloe","v":1}"#).unwrap();
            db.put("place/uluwatu", r#"{"_collection":"place","_key":"uluwatu"}"#).unwrap();
            db.compact().unwrap();
        }

        let mut db = CoreDB::open_paged(dir.path()).unwrap();

        // Photograph BEFORE any write — overlay is empty, so this view is the base.
        let before = db.snapshot_db().expect("paged mode is snapshottable");

        // Now write: a brand-new node + an update to a base node (lands in overlay).
        db.put("place/ubud", r#"{"_collection":"place","_key":"ubud"}"#).unwrap();
        db.put("tourist/chloe", r#"{"_collection":"tourist","_key":"chloe","v":2}"#).unwrap();

        // The pre-write snapshot must NOT see either change (isolation).
        assert!(before.get("place/ubud").is_none(),
            "snapshot must not see a node created after it was taken");
        let chloe_before: Value =
            serde_json::from_str(&before.get("tourist/chloe").unwrap()).unwrap();
        assert_eq!(chloe_before["v"].as_f64().unwrap(), 1.0,
            "snapshot must see the base value, not the later overlay update");
        assert!(before.get("place/uluwatu").is_some(), "base nodes are visible in the snapshot");

        // The live DB sees the new state.
        let chloe_live: Value = serde_json::from_str(&db.get("tourist/chloe").unwrap()).unwrap();
        assert_eq!(chloe_live["v"].as_f64().unwrap(), 2.0);

        // A snapshot taken AFTER the writes sees them (freshness by re-photographing).
        let after = db.snapshot_db().unwrap();
        assert!(after.get("place/ubud").is_some(), "fresh snapshot sees the new node");
        let chloe_after: Value =
            serde_json::from_str(&after.get("tourist/chloe").unwrap()).unwrap();
        assert_eq!(chloe_after["v"].as_f64().unwrap(), 2.0, "fresh snapshot sees the update");

        // Resident/ephemeral mode has no immutable base → snapshots are unsupported.
        let resident_dir = tempfile::tempdir().unwrap();
        let resident = CoreDB::open_with_config(resident_dir.path(), Config::resident()).unwrap();
        assert!(resident.snapshot_db().is_none(), "resident mode must not offer snapshots");
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
        let q_grid = "SELECT _key FROM place WHERE ST_DWithin(geometry, POINT(115.08 -8.83), 5000.0) ORDER BY _key ASC";
        let q_match = "SELECT b._key AS k FROM MATCH (a:tourist)-[:visited]->(b:place) \
                       WHERE a._key='chloe' AND ST_DWithin(b.geometry, POINT(115.08 -8.83), 5000.0)";
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
        // The grid's cell index + meta are served from the mmap'd spatialgrid.bin,
        // not a resident HashMap (disk-first).
        assert!(paged.spatial_grid.as_ref().unwrap().is_disk_backed(),
            "paged spatial grid must be mmap-backed");
        let p_grid: Vec<_> = paged.query(q_grid).unwrap().collect()
            .iter().map(|h| h.payload.clone()).collect();
        let p_match: Vec<_> = paged.query(q_match).unwrap().collect()
            .iter().map(|h| h.payload.clone()).collect();
        assert_eq!(r_grid, p_grid, "grid-path spatial must match resident");
        assert_eq!(r_match, p_match, "MATCH-filter spatial must match resident");
    }

    #[test]
    fn paged_mode_serves_vector_from_mmap() {
        // The compact vector index (int8 codes + CSR graph) served from mmap'd
        // vecidx.bin must return identical top-k to resident (same bytes, same
        // deterministic search) and be asserted disk-backed.
        let dir = tempfile::tempdir().unwrap();
        let dim = 16usize;
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            for i in 0..2000u32 {
                db.put(&format!("v/n{i}"), &format!(r#"{{"_collection":"v","_key":"n{i}"}}"#)).unwrap();
                let vec: Vec<f32> = (0..dim).map(|d| ((i as usize * 31 + d * 7) % 97) as f32 * 0.01).collect();
                db.put_vector(&format!("v/n{i}"), "emb", &vec).unwrap();
            }
            db.compact().unwrap();               // migrate vectors to the disk store
            db.build_hnsw_index_disk("emb", 16, 200, crate::query::VecMetric::L2).unwrap();
        }
        let qvec = "[0.1,0.2,0.3,0.4,0.5,0.6,0.7,0.8,0.05,0.15,0.25,0.35,0.45,0.55,0.65,0.75]";
        let q = format!("SELECT _key FROM v ORDER BY VECTOR_L2(emb, {qvec}) ASC LIMIT 10");

        let resident: Vec<String> = {
            let db = CoreDB::open(dir.path()).unwrap();
            db.query(&q).unwrap().collect().iter().map(|h| h.slug.clone()).collect()
        };
        assert_eq!(resident.len(), 10, "resident vector search returns 10");

        let paged = CoreDB::open_paged(dir.path()).unwrap();
        assert!(paged.compact_index("emb").map_or(false, |c| c.is_disk_backed()),
            "paged vector index must be mmap-backed");
        let p: Vec<String> = paged.query(&q).unwrap().collect().iter().map(|h| h.slug.clone()).collect();
        assert_eq!(resident, p, "paged vector top-k must match resident exactly");
    }

    #[test]
    fn paged_mode_serves_bm25_from_mmap() {
        // BM25 served from mmap'd bm25.bin (doc arrays off the map, dict resident,
        // postings pread) must match resident and be asserted disk-backed.
        let dir = tempfile::tempdir().unwrap();
        {
            let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
            db.execute("CREATE TABLE docs (body TEXT)").unwrap();
            let bodies = ["rust systems programming", "python is easy", "rust async runtime fast",
                          "coffee and rust", "great coffee place", "melbourne coffee roasters",
                          "learning rust today", "fast systems language"];
            for (i, b) in bodies.iter().enumerate() {
                db.execute(&format!("INSERT INTO docs (_key, body) VALUES ('d{i}', '{b}')")).unwrap();
            }
            db.compact().unwrap();               // migrate payloads to disk
            db.build_bm25_index("body");         // build + spill + save bm25.bin
        }
        let queries = [
            "SELECT _key FROM docs WHERE BM25(body, 'rust') > 0.0 ORDER BY _key ASC",
            "SELECT _key FROM docs WHERE BM25(body, 'coffee') > 0.0 ORDER BY _key ASC",
            "SELECT _key FROM docs WHERE BM25(body, 'zzznope') > 0.0 ORDER BY _key ASC",
            // No secondary key: a tie-break after a scoring expression is refused
            // rather than dropped, which is what used to happen — silently, taking
            // the LIMIT with it.
            "SELECT _key FROM docs ORDER BY BM25(body, 'rust fast') DESC LIMIT 3",
        ];
        let resident: Vec<Vec<String>> = {
            let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
            db.build_bm25_index("body"); // heap reopen rebuilds; ensure present for the baseline
            queries.iter().map(|q| db.query(q).unwrap().collect()
                .iter().map(|h| h.slug.clone()).collect()).collect()
        };
        assert!(!resident[0].is_empty(), "resident BM25 must find 'rust'");

        let paged = CoreDB::open_paged(dir.path()).unwrap();
        assert!(paged.bm25_indexes.get("body").map_or(false, |ix| ix.is_disk_backed()),
            "paged BM25 must be mmap-backed");
        for (i, q) in queries.iter().enumerate() {
            let p: Vec<String> = paged.query(q).unwrap().collect()
                .iter().map(|h| h.slug.clone()).collect();
            assert_eq!(resident[i], p, "paged BM25 must match resident: {q}");
        }
    }

    #[test]
    fn paged_mode_serves_gin_from_mmap() {
        // GIN (LIKE/ILIKE trigram index) served from the mmap'd gin.bin in paged
        // mode must match resident results, and be asserted disk-backed.
        let dir = tempfile::tempdir().unwrap();
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            db.execute("CREATE TABLE docs (name TEXT)").unwrap();
            let names = ["Alpha Bakery", "Beta Cafe", "Gamma Coffee House", "Delta Bakehouse",
                         "Epsilon Bistro", "Zeta Bakery Corner", "coffee roasters", "the coffee lab"];
            for (i, n) in names.iter().enumerate() {
                db.execute(&format!("INSERT INTO docs (_key, name) VALUES ('d{i}', '{n}')")).unwrap();
            }
            db.execute("CREATE INDEX ON docs USING gin (name)").unwrap();
            db.compact().unwrap();
        }
        let queries = [
            "SELECT _key FROM docs WHERE name ILIKE '%bak%' ORDER BY _key ASC",
            "SELECT _key FROM docs WHERE name ILIKE '%coffee%' ORDER BY _key ASC",
            "SELECT _key FROM docs WHERE name LIKE '%Cafe%' ORDER BY _key ASC",
            "SELECT _key FROM docs WHERE name ILIKE '%zzzznope%' ORDER BY _key ASC",
        ];
        let resident: Vec<Vec<String>> = {
            let db = CoreDB::open(dir.path()).unwrap();
            queries.iter().map(|q| db.query(q).unwrap().collect()
                .iter().map(|h| h.slug.clone()).collect()).collect()
        };
        assert!(!resident[0].is_empty(), "resident ILIKE must find bakeries");
        let paged = CoreDB::open_paged(dir.path()).unwrap();
        assert!(paged.gin_indexes.get("name").unwrap().is_disk_backed(),
            "paged GIN must be mmap-backed");
        for (i, q) in queries.iter().enumerate() {
            let p: Vec<String> = paged.query(q).unwrap().collect()
                .iter().map(|h| h.slug.clone()).collect();
            assert_eq!(resident[i], p, "paged GIN must match resident: {q}");
        }
    }

    #[test]
    fn paged_spatial_grid_bbox_radius_knn_match_resident() {
        // Grid served from mmap must match resident for bbox, radius, and kNN over a
        // denser point set (exercises binary search on cells + meta at scale).
        let dir = tempfile::tempdir().unwrap();
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            for i in 0..2000u32 {
                // Jitter off the regular grid so distances are unique (no kNN ties).
                let lat = -8.0 + (i % 50) as f64 * 0.02 + i as f64 * 1e-7;
                let lon = 115.0 + (i / 50) as f64 * 0.02 + i as f64 * 3e-7;
                db.put(&format!("p/n{i}"), &format!(
                    r#"{{"_collection":"p","_key":"n{i}","geometry":{{"type":"Point","coordinates":[{lon},{lat}]}}}}"#
                )).unwrap();
            }
            db.build_spatial_index();
            db.compact().unwrap();
        }
        let queries = [
            "SELECT _key FROM p WHERE ST_DWithin(geometry, POINT(115.3 -7.5), 8000) ORDER BY _key ASC",
            "SELECT _key FROM p WHERE ST_DWithin(geometry, POINT(115.05 -7.95), 3000) ORDER BY _key ASC",
            "SELECT _key FROM p ORDER BY ST_Distance(geometry, POINT(115.25 -7.55)) ASC LIMIT 15",
        ];
        // Compare order-independently (sorted) — kNN order among near-ties is not
        // guaranteed stable across resident vs mmap iteration; the SET must match.
        let run = |db: &CoreDB, q: &str| -> Vec<String> {
            let mut v: Vec<String> = db.query(q).unwrap().collect()
                .iter().map(|h| h.slug.clone()).collect();
            v.sort();
            v
        };
        let resident: Vec<Vec<String>> = {
            let db = CoreDB::open(dir.path()).unwrap();
            queries.iter().map(|q| run(&db, q)).collect()
        };
        let paged = CoreDB::open_paged(dir.path()).unwrap();
        assert!(paged.spatial_grid.as_ref().unwrap().is_disk_backed(),
            "grid must be mmap-backed in paged mode");
        for (i, q) in queries.iter().enumerate() {
            let p = run(&paged, q);
            assert_eq!(resident[i], p, "paged spatial must match resident: {q}");
            assert!(!p.is_empty(), "query should return rows: {q}");
        }
    }

    #[test]
    fn paged_mode_serves_search_from_mmap() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            db.execute("CREATE TABLE articles (title TEXT, body TEXT)").unwrap();
            db.execute("INSERT INTO articles (_key, title, body) VALUES ('a1', 'Rust Programming', 'Rust is fast and safe')").unwrap();
            db.execute("INSERT INTO articles (_key, title, body) VALUES ('a2', 'Python Guide', 'Python is easy to learn')").unwrap();
            db.execute("INSERT INTO articles (_key, title, body) VALUES ('a3', 'Rust and Python', 'Both languages are great')").unwrap();
            db.execute("CREATE INDEX ON articles USING search (title, body)").unwrap();
            db.compact().unwrap();
        }

        // Exercise exact-term, multi-term AND, and fuzzy — all go through the FST
        // + postings, which are the mmap-served blobs in paged mode.
        let queries = [
            "SELECT _key FROM articles WHERE SEARCH('rust') ORDER BY _key ASC",
            "SELECT _key FROM articles WHERE SEARCH('rust fast') ORDER BY _key ASC",
            "SELECT _key FROM articles WHERE SEARCH('programing') ORDER BY _key ASC", // fuzzy
            // Ranking exercises field_post + position_post (proximity/field-order),
            // which are also mmap-served in paged mode.
            "SELECT _key FROM articles WHERE SEARCH('rust') ORDER BY SEARCH_SCORE('rust fast') DESC",
        ];

        let resident: Vec<Vec<Option<serde_json::Value>>> = {
            let db = CoreDB::open(dir.path()).unwrap();
            queries.iter().map(|q| db.query(q).unwrap().collect()
                .iter().map(|h| h.payload.clone()).collect()).collect()
        };
        assert!(!resident[0].is_empty(), "resident SEARCH must find 'rust'");

        let paged = CoreDB::open_paged(dir.path()).unwrap();
        assert!(paged.nodes.is_empty(), "paged: node map stays empty (disk-first)");
        // Proof the index blobs are served from the mmap, not the heap.
        let idx = paged.search_indexes.values().next().expect("search index present in paged mode");
        assert!(matches!(idx.fst_data, search::index::Bytes::Mapped { .. }),
                "FST must be mmap-backed in paged mode");
        assert!(matches!(idx.postings_data, search::index::Bytes::Mapped { .. }),
                "postings must be mmap-backed in paged mode");

        for (i, q) in queries.iter().enumerate() {
            let p: Vec<_> = paged.query(q).unwrap().collect()
                .iter().map(|h| h.payload.clone()).collect();
            assert_eq!(resident[i], p, "paged SEARCH must match resident for: {q}");
        }
    }

    #[test]
    fn manifest_snapshot_shrinks_and_reopens_complete() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
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
            let mut db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
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
            let db = CoreDB::open_with_config(dir.path(), Config::resident()).unwrap();
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
        let cfg = Config { payload_binary: true, ..Config::resident() };
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
        let cfg = Config { payload_binary: true, ..Config::resident() };
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

    /// Compile-time proof that every index family a snapshot must freeze is
    /// `Clone`-able. This is the prerequisite for running indexed SQL against a
    /// snapshot (`snapshot_db()`): the immutable base is shared by `Arc`, and each
    /// index overlay is cloned. If someone adds a non-`Clone` member to any of
    /// these, this test stops compiling instead of failing much later.
    #[test]
    fn every_index_family_is_clone() {
        fn assert_clone<T: Clone>() {}
        assert_clone::<crate::bm25::Bm25Index>();          // relevance ranking
        assert_clone::<crate::search::SearchIndex>();      // positional / phrase
        assert_clone::<crate::vector::QuantizedField>();   // int8 vector codes
        assert_clone::<crate::vector::HnswGraph>();        // vector graph
        assert_clone::<crate::geo::SpatialGrid>();         // spatial
        assert_clone::<crate::storage::edgestore::EdgeStore>();          // graph edges
        assert_clone::<crate::storage::fieldstore::MappedFieldStore>();  // btree field index
        assert_clone::<crate::storage::vecstore::VectorStore>();         // raw vectors
        assert_clone::<crate::text_index::gin::GINIndex>();   // trigram ILIKE
        assert_clone::<crate::text_index::gist::GiSTIndex>(); // trigram (lossy)
        assert_clone::<crate::storage::mmap::MmapView>();  // the shared primitive
    }

    #[test]
    fn auto_compact_fires_after_a_grouped_batch() {
        // Regression: every statement inside `execute_batch_grouped` runs with
        // `defer_wal_sync = true`, which short-circuits `autocompact_after_write`.
        // Without a check once the group is durable, buffered SQL (the path
        // `Engine::flush` uses) would never auto-compact.
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            auto_compact: AutoCompact::OnWrite,
            compact_thresholds: CompactThresholds { wal_bytes: 2048, overlay_entries: usize::MAX },
            ..Config::default()
        };
        let mut db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
        db.execute("CREATE TABLE t (_key TEXT PRIMARY KEY, pad TEXT)").unwrap();

        let stmts: Vec<String> = (0..150)
            .map(|i| format!(
                "INSERT INTO t (_key, pad) VALUES ('n{i}', 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx')"
            ))
            .collect();
        db.execute_batch_grouped(&stmts).unwrap();

        let wal_len = std::fs::metadata(dir.path().join("wal.log")).unwrap().len();
        assert!(
            wal_len < 2048 + 4096,
            "grouped batch must auto-compact once durable (wal len = {wal_len})"
        );
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

    // ── Ring-2 store-format migration framework ──
    fn mig_1to2(dir: &std::path::Path) -> std::io::Result<()> {
        std::fs::write(dir.join("m12"), b"")
    }
    fn mig_2to3(dir: &std::path::Path) -> std::io::Result<()> {
        std::fs::write(dir.join("m23"), b"")
    }

    #[test]
    fn store_migration_empty_registry_is_noop() {
        let d = tempfile::tempdir().unwrap();
        let reached = super::apply_store_migrations(d.path(), 1, 3, &[]).unwrap();
        assert_eq!(reached, 1, "no registered migration → stays at source version");
    }

    #[test]
    fn store_migration_full_chain_applies_in_order() {
        let d = tempfile::tempdir().unwrap();
        let migs = [
            super::StoreMigration { from: 1, describe: "1->2", run: mig_1to2 },
            super::StoreMigration { from: 2, describe: "2->3", run: mig_2to3 },
        ];
        let reached = super::apply_store_migrations(d.path(), 1, 3, &migs).unwrap();
        assert_eq!(reached, 3);
        assert!(d.path().join("m12").exists() && d.path().join("m23").exists());
    }

    #[test]
    fn store_migration_stops_at_gap() {
        let d = tempfile::tempdir().unwrap();
        // 1->2 registered, 2->3 missing: chain reaches 2, then backward-compat read.
        let migs = [super::StoreMigration { from: 1, describe: "1->2", run: mig_1to2 }];
        let reached = super::apply_store_migrations(d.path(), 1, 3, &migs).unwrap();
        assert_eq!(reached, 2);
        assert!(d.path().join("m12").exists());
        assert!(!d.path().join("m23").exists());
    }

    #[test]
    fn store_migration_same_version_runs_nothing() {
        let d = tempfile::tempdir().unwrap();
        let migs = [super::StoreMigration { from: 1, describe: "x", run: mig_1to2 }];
        let reached = super::apply_store_migrations(d.path(), 3, 3, &migs).unwrap();
        assert_eq!(reached, 3);
        assert!(!d.path().join("m12").exists());
    }
}

// ── SGQL dump helpers (used by CoreDB::dump_sql) ───────────────────────────────

/// Auto/internal payload columns that a dump must not emit (regenerated on load).
fn is_internal_field(name: &str) -> bool {
    matches!(
        name,
        "_id" | "_collection" | "_key" | "_created_unix" | "_updated_unix"
    )
}

/// Escape a string for a single-quoted SGQL literal (SQL-standard `''`).
fn sql_str_escape(s: &str) -> String {
    s.replace('\'', "''")
}

/// SGQL type keyword for a schema field type (mirrors `schema_ddl`).
fn field_type_sql(ty: sql::FieldType) -> &'static str {
    use sql::FieldType::*;
    match ty {
        Text => "TEXT",
        Integer => "INTEGER",
        Real => "REAL",
        Bool => "BOOLEAN",
        Timestamptz => "TIMESTAMPTZ",
        Geo => "GEO",
        Vector => "VECTOR",
        Json => "JSON",
    }
}

/// Shortest round-trippable float literal for a vector element (always has a `.`
/// or exponent so it lexes as a float, never a bare integer).
fn fmt_f32(x: f32) -> String {
    let s = format!("{x}");
    if s.contains('.') || s.contains('e') || s.contains('E') || s.contains("inf") || s.contains("NaN") {
        s
    } else {
        format!("{s}.0")
    }
}

/// Render a JSON value as an SGQL literal, honoring the column's declared type.
/// GEO → `ST_GeomFromGeoJSON('…')`; VECTOR arrays → `[…]`; objects/JSON arrays →
/// a quoted JSON string; scalars → their literal form.
fn sql_value_literal(v: &Value, ty: Option<sql::FieldType>) -> String {
    use sql::FieldType;
    if ty == Some(FieldType::Geo) {
        let json = serde_json::to_string(v).unwrap_or_else(|_| "null".into());
        return format!("ST_GeomFromGeoJSON('{}')", sql_str_escape(&json));
    }
    match v {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => if *b { "TRUE".into() } else { "FALSE".into() },
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("'{}'", sql_str_escape(s)),
        Value::Array(a) if ty == Some(FieldType::Vector) && a.iter().all(|x| x.is_number()) => {
            let nums: Vec<String> = a.iter().map(|x| x.to_string()).collect();
            format!("[{}]", nums.join(", "))
        }
        Value::Array(_) | Value::Object(_) => {
            format!("'{}'", sql_str_escape(&serde_json::to_string(v).unwrap_or_default()))
        }
    }
}

/// Format an edge's merged attributes as ` {k: v, …}` for an edge INSERT, or an
/// empty string if the edge is naked.
fn dump_edge_attrs(meta: Option<&Value>) -> String {
    let Some(Value::Object(map)) = meta else { return String::new() };
    let parts: Vec<String> = map
        .iter()
        .filter(|(k, _)| !is_internal_field(k))
        .map(|(k, v)| format!("{}: {}", k, sql_value_literal(v, None)))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!(" {{{}}}", parts.join(", "))
    }
}
