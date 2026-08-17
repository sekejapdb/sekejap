//! # The write-ahead log (WAL) — how a crash can't lose your data
//!
//! A WAL is the classic durability trick. Before the database changes anything in
//! memory, it *appends* a description of the change to the end of this log file
//! and flushes it to disk. If the process crashes, the next `open()` replays the
//! log and re-applies every change, arriving exactly where it left off. Because
//! writes only ever append (never seek and overwrite), they're fast and simple to
//! reason about, and a crash can at worst leave a half-written *last* record —
//! which the CRC check below catches and discards, never corrupting earlier ones.
//!
//! Each change is one **frame**: a CRC32 checksum (to detect a torn write), a
//! length, then the encoded payload. Two encodings are supported, told apart by
//! the file's 4-byte magic: `SKWJ` (JSON — human-readable, greppable) and `SKWB`
//! (binary — compact). `compact()` folds this log into a snapshot and truncates
//! it back to empty (see `CoreDB::compact`).
//!
//! # File layout
//! ```text
//! ┌──────────────────┬──────────────────────────────────────────┐
//! │ Header (8 bytes) │ Frames ...                              │
//! │ [magic 4B]       │ ┌──────┬──────┬──────────────────────┐  │
//! │ [version 4B]     │ │CRC32 │length│ encoded payload      │  │
//! │                  │ │4 B   │4 B   │ N bytes              │  │
//! │                  │ └──────┴──────┴──────────────────────┘  │
//! └──────────────────┴──────────────────────────────────────────┘
//! ```
//!
//! Magic bytes identify the encoding:
//! - `SKWJ` — JSON (human-readable, inspectable with any text tool)
//! - `SKWB` — Binary (compact type-tag + length-prefixed fields)
//!
//! Legacy WAL files (no header) are auto-detected and read as JSON.
//! CRC32 is computed over `[length_bytes || payload_bytes]`.

use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

// ── Format ────────────────────────────────────────────────────────────────────

const MAGIC_JSON: [u8; 4] = *b"SKWJ";
const MAGIC_BINARY: [u8; 4] = *b"SKWB";
const WAL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalFormat {
    Json,
    Binary,
}

impl Default for WalFormat {
    fn default() -> Self { WalFormat::Binary }
}

impl std::fmt::Display for WalFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalFormat::Json => write!(f, "json"),
            WalFormat::Binary => write!(f, "binary"),
        }
    }
}

// ── Sync level ────────────────────────────────────────────────────────────────

/// How hard `WalWriter::sync()` pushes bytes toward the platter.
///
/// On macOS the three levels differ enormously (measured on Apple SSD):
/// plain `fsync` ≈ 60µs, `F_BARRIERFSYNC` ≈ 400µs, `F_FULLFSYNC` ≈ 3ms.
/// SQLite's `synchronous=FULL` uses plain `fsync` unless `PRAGMA fullfsync=ON`
/// — so `Os` is the level that matches SQLite's actual default durability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncLevel {
    /// True power-loss durability: `F_FULLFSYNC` on macOS (via `sync_data`),
    /// `fdatasync` elsewhere. The strongest — and slowest — level. Default.
    Full,
    /// Write-ordering barrier: `F_BARRIERFSYNC` on macOS. Data reaches the
    /// drive in order but its cache is not forced. ~7x faster than Full.
    /// Falls back to `Full` on non-macOS.
    Barrier,
    /// OS-level flush only: plain `fsync`. Same durability as SQLite's
    /// default `synchronous=FULL` on macOS. ~50x faster than Full.
    Os,
}

#[cfg(target_os = "macos")]
extern "C" {
    fn fsync(fd: std::os::raw::c_int) -> std::os::raw::c_int;
    fn fcntl(fd: std::os::raw::c_int, cmd: std::os::raw::c_int, ...) -> std::os::raw::c_int;
}

#[cfg(target_os = "macos")]
const F_BARRIERFSYNC: std::os::raw::c_int = 85;

// ── WAL entry ─────────────────────────────────────────────────────────────────

/// A single mutation recorded in the WAL.
///
/// The `op` tag is used as a discriminant in JSON: `{"op":"put",...}`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WalEntry {
    Put {
        slug: String,
        payload: String,
    },
    Remove {
        slug: String,
    },
    Link {
        from: String,
        to: String,
        edge_type: String,
    },
    LinkMeta {
        from: String,
        to: String,
        edge_type: String,
        meta: String,
    },
    Unlink {
        from: String,
        to: String,
        edge_type: String,
    },
    /// Delete only edges matching an attribute predicate (`props` = JSON object).
    UnlinkWhere {
        from: String,
        to: String,
        edge_type: String,
        props: String,
    },
    /// Set attributes (`sets` = JSON object) on edges matching a predicate.
    UpdateEdge {
        from: String,
        to: String,
        edge_type: String,
        props: String,
        sets: String,
    },
    CreateTable {
        collection: String,
        schema_json: String,
    },
    PutVector {
        slug: String,
        field: String,
        data: Vec<f32>,
    },
    CreateIndex {
        collection: String,
        method: String,
        fields: Vec<String>,
    },
    DropTable {
        collection: String,
    },
    DropIndex {
        collection: String,
        method: String,
        field: String,
    },
    AlterTable {
        collection: String,
        /// JSON-serialised `AlterTableOp` — keeps the WAL self-contained.
        op_json: String,
    },
    /// Logical UPDATE (command logging): records the compiled statement
    /// instead of one `Put` per affected row. Replay re-executes the
    /// statement against the replayed-so-far state, which is identical to
    /// the state it saw at runtime (WAL replay is strictly sequential and
    /// stops at the first corrupted frame). `now_ms` is embedded so
    /// `_updated_unix` is reproduced exactly.
    ///
    /// Written only when `SET WAL_MODE = logical` is active.
    Update {
        /// JSON-serialised `Vec<Step>` — the filter pipeline.
        steps_json: String,
        /// JSON-serialised `Vec<(String, Value)>` — the SET assignments.
        updates_json: String,
        /// Timestamp (ms) captured at execution time.
        now_ms: i64,
    },
    /// Transaction boundary: marks the start of an atomic group.
    /// All entries between `TxnBegin` and `TxnEnd` are replayed
    /// together or discarded together on crash recovery.
    TxnBegin,
    /// Transaction boundary: marks the end of an atomic group.
    TxnEnd,
    /// Forward-compatibility catch-all: entries written by a newer binary
    /// with an unknown `op` value are silently skipped on replay.
    #[serde(other)]
    Unknown,
}

// ── CRC helper ────────────────────────────────────────────────────────────────

fn crc32(data: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(data);
    h.finalize()
}

// ── Codec ─────────────────────────────────────────────────────────────────────

fn encode_entry(entry: &WalEntry, format: WalFormat) -> Result<Vec<u8>, io::Error> {
    match format {
        WalFormat::Json => serde_json::to_vec(entry)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        WalFormat::Binary => Ok(binary_encode(entry)),
    }
}

fn decode_entry(data: &[u8], format: WalFormat) -> Result<WalEntry, io::Error> {
    match format {
        WalFormat::Json => serde_json::from_slice(data)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        WalFormat::Binary => binary_decode(data)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad binary WAL frame")),
    }
}

// ── Binary wire format ───────────────────────────────────────────────────────
//
// Tag (u8) identifies the variant. Strings are [len:u32 LE][utf8].
// f32 is 4 bytes LE. Vec<f32> is [count:u32 LE][f32 * count].
// Vec<String> is [count:u32 LE][String * count].

const TAG_PUT: u8 = 0;
const TAG_REMOVE: u8 = 1;
const TAG_LINK: u8 = 2;
const TAG_LINK_META: u8 = 3;
const TAG_UNLINK: u8 = 4;
const TAG_CREATE_TABLE: u8 = 5;
const TAG_PUT_VECTOR: u8 = 6;
const TAG_CREATE_INDEX: u8 = 7;
const TAG_DROP_TABLE: u8 = 8;
const TAG_DROP_INDEX: u8 = 9;
const TAG_ALTER_TABLE: u8 = 10;
const TAG_TXN_BEGIN: u8 = 11;
const TAG_TXN_END: u8 = 12;
const TAG_UPDATE: u8 = 13;
const TAG_UNLINK_WHERE: u8 = 14;
const TAG_UPDATE_EDGE: u8 = 15;

fn put_str(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
}

fn put_i64(buf: &mut Vec<u8>, v: i64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn put_vec_f32(buf: &mut Vec<u8>, v: &[f32]) {
    buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
    for &f in v {
        buf.extend_from_slice(&f.to_le_bytes());
    }
}

fn put_vec_str(buf: &mut Vec<u8>, v: &[String]) {
    buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
    for s in v {
        put_str(buf, s);
    }
}

struct BinaryReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BinaryReader<'a> {
    fn new(data: &'a [u8]) -> Self { Self { data, pos: 0 } }

    fn remaining(&self) -> usize { self.data.len() - self.pos }

    fn read_u8(&mut self) -> Option<u8> {
        if self.remaining() < 1 { return None; }
        let v = self.data[self.pos];
        self.pos += 1;
        Some(v)
    }

    fn read_u32(&mut self) -> Option<u32> {
        if self.remaining() < 4 { return None; }
        let v = u32::from_le_bytes(self.data[self.pos..self.pos + 4].try_into().unwrap());
        self.pos += 4;
        Some(v)
    }

    fn read_f32(&mut self) -> Option<f32> {
        if self.remaining() < 4 { return None; }
        let v = f32::from_le_bytes(self.data[self.pos..self.pos + 4].try_into().unwrap());
        self.pos += 4;
        Some(v)
    }

    fn read_i64(&mut self) -> Option<i64> {
        if self.remaining() < 8 { return None; }
        let v = i64::from_le_bytes(self.data[self.pos..self.pos + 8].try_into().unwrap());
        self.pos += 8;
        Some(v)
    }

    fn read_str(&mut self) -> Option<String> {
        let len = self.read_u32()? as usize;
        if self.remaining() < len { return None; }
        let s = std::str::from_utf8(&self.data[self.pos..self.pos + len]).ok()?.to_string();
        self.pos += len;
        Some(s)
    }

    fn read_vec_f32(&mut self) -> Option<Vec<f32>> {
        let count = self.read_u32()? as usize;
        // Reserved against what the frame can actually contain, not against the
        // number it claims. The count is four bytes of file, so a thirty-byte frame
        // declaring 0xFFFF_FFFF elements asked for 16 GB before reading the first
        // one and failing — a memory-exhaustion denial of service out of a tiny
        // crafted file, and the CRC is no obstacle because a CRC is not a signature.
        let mut v = Vec::with_capacity(count.min(self.remaining() / 4));
        for _ in 0..count {
            v.push(self.read_f32()?);
        }
        Some(v)
    }

    fn read_vec_str(&mut self) -> Option<Vec<String>> {
        let count = self.read_u32()? as usize;
        // As above. A `String` is 24 bytes of header, and the shortest one a frame
        // can encode is its 4-byte length — so the claim is capped by what is left.
        let mut v = Vec::with_capacity(count.min(self.remaining() / 4));
        for _ in 0..count {
            v.push(self.read_str()?);
        }
        Some(v)
    }
}

pub fn binary_encode(entry: &WalEntry) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    match entry {
        WalEntry::Put { slug, payload } => {
            buf.push(TAG_PUT);
            put_str(&mut buf, slug);
            put_str(&mut buf, payload);
        }
        WalEntry::Remove { slug } => {
            buf.push(TAG_REMOVE);
            put_str(&mut buf, slug);
        }
        WalEntry::Link { from, to, edge_type } => {
            buf.push(TAG_LINK);
            put_str(&mut buf, from);
            put_str(&mut buf, to);
            put_str(&mut buf, edge_type);
        }
        WalEntry::LinkMeta { from, to, edge_type, meta } => {
            buf.push(TAG_LINK_META);
            put_str(&mut buf, from);
            put_str(&mut buf, to);
            put_str(&mut buf, edge_type);
            put_str(&mut buf, meta);
        }
        WalEntry::Unlink { from, to, edge_type } => {
            buf.push(TAG_UNLINK);
            put_str(&mut buf, from);
            put_str(&mut buf, to);
            put_str(&mut buf, edge_type);
        }
        WalEntry::UnlinkWhere { from, to, edge_type, props } => {
            buf.push(TAG_UNLINK_WHERE);
            put_str(&mut buf, from);
            put_str(&mut buf, to);
            put_str(&mut buf, edge_type);
            put_str(&mut buf, props);
        }
        WalEntry::UpdateEdge { from, to, edge_type, props, sets } => {
            buf.push(TAG_UPDATE_EDGE);
            put_str(&mut buf, from);
            put_str(&mut buf, to);
            put_str(&mut buf, edge_type);
            put_str(&mut buf, props);
            put_str(&mut buf, sets);
        }
        WalEntry::CreateTable { collection, schema_json } => {
            buf.push(TAG_CREATE_TABLE);
            put_str(&mut buf, collection);
            put_str(&mut buf, schema_json);
        }
        WalEntry::PutVector { slug, field, data } => {
            buf.push(TAG_PUT_VECTOR);
            put_str(&mut buf, slug);
            put_str(&mut buf, field);
            put_vec_f32(&mut buf, data);
        }
        WalEntry::CreateIndex { collection, method, fields } => {
            buf.push(TAG_CREATE_INDEX);
            put_str(&mut buf, collection);
            put_str(&mut buf, method);
            put_vec_str(&mut buf, fields);
        }
        WalEntry::DropTable { collection } => {
            buf.push(TAG_DROP_TABLE);
            put_str(&mut buf, collection);
        }
        WalEntry::DropIndex { collection, method, field } => {
            buf.push(TAG_DROP_INDEX);
            put_str(&mut buf, collection);
            put_str(&mut buf, method);
            put_str(&mut buf, field);
        }
        WalEntry::AlterTable { collection, op_json } => {
            buf.push(TAG_ALTER_TABLE);
            put_str(&mut buf, collection);
            put_str(&mut buf, op_json);
        }
        WalEntry::Update { steps_json, updates_json, now_ms } => {
            buf.push(TAG_UPDATE);
            put_str(&mut buf, steps_json);
            put_str(&mut buf, updates_json);
            put_i64(&mut buf, *now_ms);
        }
        WalEntry::TxnBegin => { buf.push(TAG_TXN_BEGIN); }
        WalEntry::TxnEnd => { buf.push(TAG_TXN_END); }
        WalEntry::Unknown => { buf.push(255); }
    }
    buf
}

pub fn binary_decode(data: &[u8]) -> Option<WalEntry> {
    let mut r = BinaryReader::new(data);
    let tag = r.read_u8()?;
    match tag {
        TAG_PUT => Some(WalEntry::Put {
            slug: r.read_str()?,
            payload: r.read_str()?,
        }),
        TAG_REMOVE => Some(WalEntry::Remove {
            slug: r.read_str()?,
        }),
        TAG_LINK => Some(WalEntry::Link {
            from: r.read_str()?,
            to: r.read_str()?,
            edge_type: r.read_str()?,
        }),
        TAG_LINK_META => Some(WalEntry::LinkMeta {
            from: r.read_str()?,
            to: r.read_str()?,
            edge_type: r.read_str()?,
            meta: r.read_str()?,
        }),
        TAG_UNLINK => Some(WalEntry::Unlink {
            from: r.read_str()?,
            to: r.read_str()?,
            edge_type: r.read_str()?,
        }),
        TAG_UNLINK_WHERE => Some(WalEntry::UnlinkWhere {
            from: r.read_str()?,
            to: r.read_str()?,
            edge_type: r.read_str()?,
            props: r.read_str()?,
        }),
        TAG_UPDATE_EDGE => Some(WalEntry::UpdateEdge {
            from: r.read_str()?,
            to: r.read_str()?,
            edge_type: r.read_str()?,
            props: r.read_str()?,
            sets: r.read_str()?,
        }),
        TAG_CREATE_TABLE => Some(WalEntry::CreateTable {
            collection: r.read_str()?,
            schema_json: r.read_str()?,
        }),
        TAG_PUT_VECTOR => Some(WalEntry::PutVector {
            slug: r.read_str()?,
            field: r.read_str()?,
            data: r.read_vec_f32()?,
        }),
        TAG_CREATE_INDEX => Some(WalEntry::CreateIndex {
            collection: r.read_str()?,
            method: r.read_str()?,
            fields: r.read_vec_str()?,
        }),
        TAG_DROP_TABLE => Some(WalEntry::DropTable {
            collection: r.read_str()?,
        }),
        TAG_DROP_INDEX => Some(WalEntry::DropIndex {
            collection: r.read_str()?,
            method: r.read_str()?,
            field: r.read_str()?,
        }),
        TAG_ALTER_TABLE => Some(WalEntry::AlterTable {
            collection: r.read_str()?,
            op_json: r.read_str()?,
        }),
        TAG_UPDATE => Some(WalEntry::Update {
            steps_json: r.read_str()?,
            updates_json: r.read_str()?,
            now_ms: r.read_i64()?,
        }),
        TAG_TXN_BEGIN => Some(WalEntry::TxnBegin),
        TAG_TXN_END => Some(WalEntry::TxnEnd),
        _ => Some(WalEntry::Unknown),
    }
}

// ── Writer ────────────────────────────────────────────────────────────────────

pub(crate) struct WalWriter {
    inner: BufWriter<File>,
    format: WalFormat,
    sync_level: SyncLevel,
    /// Records appended since open. See [`seq`](WalWriter::seq).
    seq: u64,
}

impl WalWriter {
    /// Open (or create) a WAL file in append mode using JSON format.
    ///
    /// Test-only convenience; production opens via `open_with_format` so the
    /// WAL format is chosen explicitly from config.
    #[cfg(test)]
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::open_with_format(path, WalFormat::Json)
    }

    /// Open (or create) a WAL file in append mode with the given format.
    ///
    /// If the file already exists and is non-empty, the existing format
    /// (detected from the header) is used regardless of `format` — you
    /// cannot switch formats on an existing WAL. To switch, compact first
    /// (which truncates the WAL) then reopen.
    pub fn open_with_format(path: &Path, format: WalFormat) -> io::Result<Self> {
        let exists = path.exists();
        let existing_len = if exists { std::fs::metadata(path)?.len() } else { 0 };

        if exists && existing_len >= 8 {
            let detected = detect_format(path)?;
            let file = OpenOptions::new().append(true).open(path)?;
            return Ok(Self {
                inner: BufWriter::new(file),
                format: detected,
                sync_level: SyncLevel::Full,
                seq: 0,
            });
        }

        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let mut writer = BufWriter::new(file);
        let magic = match format {
            WalFormat::Json => MAGIC_JSON,
            WalFormat::Binary => MAGIC_BINARY,
        };
        writer.write_all(&magic)?;
        writer.write_all(&WAL_VERSION.to_le_bytes())?;
        writer.flush()?;
        Ok(Self { inner: writer, format, sync_level: SyncLevel::Full, seq: 0 })
    }

    /// The encoding format this writer uses.
    pub fn format(&self) -> WalFormat { self.format }

    /// Change how hard `sync()` pushes toward the platter.
    pub fn set_sync_level(&mut self, level: SyncLevel) {
        self.sync_level = level;
    }

    /// Append one entry. Flushes to OS after every write.
    /// Call `sync()` if you need fsync-level durability.
    pub fn append(&mut self, entry: &WalEntry) -> io::Result<()> {
        let encoded = encode_entry(entry, self.format)?;
        let len_bytes = (encoded.len() as u32).to_le_bytes();

        let mut crc_input = Vec::with_capacity(4 + encoded.len());
        crc_input.extend_from_slice(&len_bytes);
        crc_input.extend_from_slice(&encoded);
        let checksum = crc32(&crc_input).to_le_bytes();

        self.inner.write_all(&checksum)?;
        self.inner.write_all(&len_bytes)?;
        self.inner.write_all(&encoded)?;
        self.seq += 1;
        self.inner.flush()
    }

    pub fn append_batch(&mut self, entries: &[WalEntry]) -> io::Result<()> {
        for entry in entries {
            let encoded = encode_entry(entry, self.format)?;
            let len_bytes = (encoded.len() as u32).to_le_bytes();

            let mut crc_input = Vec::with_capacity(4 + encoded.len());
            crc_input.extend_from_slice(&len_bytes);
            crc_input.extend_from_slice(&encoded);
            let checksum = crc32(&crc_input).to_le_bytes();

            self.inner.write_all(&checksum)?;
            self.inner.write_all(&len_bytes)?;
            self.inner.write_all(&encoded)?;
            self.seq += 1;
        }
        self.inner.flush()
    }

    /// fsync — call after a batch of writes when you need
    /// guaranteed on-disk durability. Strength depends on `sync_level`.
    pub fn sync(&mut self) -> io::Result<()> {
        self.inner.flush()?;
        sync_file(self.inner.get_ref(), self.sync_level)
    }

    /// Records appended so far, counted since this writer was opened.
    ///
    /// The log is append-only and written in order, so one `fsync` makes every
    /// record up to the current count durable. That is what lets several writers
    /// share a single `fsync`: each remembers the count it reached, and waits
    /// until a sync covering at least that count has completed.
    pub fn seq(&self) -> u64 { self.seq }

    /// The strength this writer syncs at, so a shared descriptor matches it.
    pub fn sync_level(&self) -> SyncLevel { self.sync_level }

    /// A second descriptor onto the same log file.
    ///
    /// `fsync` on it flushes the same file, which is what allows the sync to
    /// happen *outside* the write lock while another thread is already appending
    /// the next record.
    pub fn try_clone_file(&self) -> io::Result<File> {
        self.inner.get_ref().try_clone()
    }
}

/// `fsync` a descriptor at the requested strength.
///
/// Split out of `WalWriter::sync` so the group-commit coordinator can sync a
/// cloned descriptor without holding the writer.
pub(crate) fn sync_file(file: &File, level: SyncLevel) -> io::Result<()> {
    {
        match level {
            SyncLevel::Full => file.sync_data(),
            #[cfg(target_os = "macos")]
            SyncLevel::Barrier => {
                use std::os::unix::io::AsRawFd;
                let rc = unsafe { fcntl(file.as_raw_fd(), F_BARRIERFSYNC) };
                if rc == -1 {
                    // Barrier unsupported on this filesystem — fall back to full.
                    return file.sync_data();
                }
                Ok(())
            }
            #[cfg(target_os = "macos")]
            SyncLevel::Os => {
                use std::os::unix::io::AsRawFd;
                let rc = unsafe { fsync(file.as_raw_fd()) };
                if rc == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            }
            // On non-macOS, sync_data (fdatasync) is already the sane level
            // for all three settings.
            #[cfg(not(target_os = "macos"))]
            SyncLevel::Barrier | SyncLevel::Os => file.sync_data(),
        }
    }
}

// ── Reader ────────────────────────────────────────────────────────────────────

/// Detect the WAL format from the file header.
///
/// Returns `WalFormat::Json` for legacy headerless files.
fn detect_format(path: &Path) -> io::Result<WalFormat> {
    let mut f = File::open(path)?;
    let mut magic = [0u8; 4];
    match f.read_exact(&mut magic) {
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(WalFormat::Json),
        Err(e) => return Err(e),
        Ok(_) => {}
    }
    if magic == MAGIC_JSON { return Ok(WalFormat::Json); }
    if magic == MAGIC_BINARY { return Ok(WalFormat::Binary); }
    Ok(WalFormat::Json) // legacy: no header
}

/// Returns `true` if the first 4 bytes match a known magic header.
fn has_magic_header(path: &Path) -> io::Result<bool> {
    let mut f = File::open(path)?;
    let mut magic = [0u8; 4];
    match f.read_exact(&mut magic) {
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
        Err(e) => return Err(e),
        Ok(_) => {}
    }
    Ok(magic == MAGIC_JSON || magic == MAGIC_BINARY)
}

pub(crate) struct WalReader {
    inner: BufReader<File>,
    format: WalFormat,
}

impl WalReader {
    pub fn open(path: &Path) -> io::Result<Self> {
        let format = detect_format(path)?;
        let mut file = File::open(path)?;

        if has_magic_header(path)? {
            file.seek(SeekFrom::Start(8))?;
        }

        Ok(Self {
            inner: BufReader::new(file),
            format,
        })
    }

    /// The encoding format detected in this WAL file.
    #[cfg(test)]
    pub fn format(&self) -> WalFormat { self.format }

    /// Replay every valid frame from the WAL, calling `cb` for each entry.
    ///
    /// Processes one frame at a time — no buffering of all entries into RAM.
    /// Stops at the first bad CRC, truncated frame, or oversized payload.
    /// Returns `true` if corruption was detected.
    pub fn replay_all<F: FnMut(WalEntry)>(mut self, mut cb: F) -> bool {
        let format = self.format;
        let mut corrupted = false;
        loop {
            let mut header = [0u8; 8];
            match self.inner.read_exact(&mut header) {
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(_) => { corrupted = true; break; }
                Ok(_) => {}
            }

            let stored_crc = u32::from_le_bytes(header[..4].try_into().unwrap());
            let len = u32::from_le_bytes(header[4..].try_into().unwrap()) as usize;

            if len > 64 << 20 { corrupted = true; break; }

            let mut payload = vec![0u8; len];
            match self.inner.read_exact(&mut payload) {
                Err(_) => { corrupted = true; break; }
                Ok(_) => {}
            }

            let mut crc_input = Vec::with_capacity(4 + len);
            crc_input.extend_from_slice(&(len as u32).to_le_bytes());
            crc_input.extend_from_slice(&payload);
            if crc32(&crc_input) != stored_crc { corrupted = true; break; }

            match decode_entry(&payload, format) {
                Ok(WalEntry::Unknown) => { /* forward-compat: skip silently */ }
                Ok(entry)             => cb(entry),
                Err(_)                => { corrupted = true; break; }
            }
        }
        corrupted
    }

    /// Read every valid frame from the WAL into a Vec.
    ///
    /// Test-only; production replays one frame at a time via `replay_all` to
    /// avoid buffering all entries in RAM.
    /// Returns `(entries, had_corruption)`.
    #[cfg(test)]
    pub fn read_all(self) -> (Vec<WalEntry>, bool) {
        let mut entries = Vec::new();
        let corrupted = self.replay_all(|e| entries.push(e));
        (entries, corrupted)
    }
}

// ── Migration ────────────────────────────────────────────────────────────────

/// Migrate a WAL file from its current format to `target`.
///
/// Reads all valid entries from `src`, writes them to `dst` in the target
/// format, then atomically renames `dst` over `src`. Currently exercised only
/// by the format-migration tests.
#[cfg(test)]
pub(crate) fn migrate_wal(
    src: &Path,
    target: WalFormat,
) -> io::Result<usize> {
    let reader = WalReader::open(src)?;
    if reader.format() == target {
        return Ok(0);
    }

    let dst = src.with_extension("wal.mig");
    let mut writer = WalWriter::open_with_format(&dst, target)?;
    let mut count = 0usize;
    let corrupted = WalReader::open(src)?.replay_all(|entry| {
        writer.append(&entry).expect("migration write failed");
        count += 1;
    });
    writer.sync()?;

    if corrupted {
        eprintln!(
            "sekejap: WAL migration stopped at corrupted frame — \
             migrated {} valid entries.",
            count
        );
    }

    std::fs::rename(&dst, src)?;
    Ok(count)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn roundtrip(entries: Vec<WalEntry>) -> (Vec<WalEntry>, bool) {
        roundtrip_format(entries, WalFormat::Json)
    }

    fn roundtrip_format(entries: Vec<WalEntry>, format: WalFormat) -> (Vec<WalEntry>, bool) {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp);

        let mut w = WalWriter::open_with_format(&path, format).unwrap();
        for e in &entries {
            w.append(e).unwrap();
        }
        drop(w);

        WalReader::open(&path).unwrap().read_all()
    }

    fn all_entry_variants() -> Vec<WalEntry> {
        vec![
            WalEntry::Put { slug: "users/alice".into(), payload: r#"{"name":"Alice","age":30}"#.into() },
            WalEntry::Remove { slug: "users/bob".into() },
            WalEntry::Link { from: "users/alice".into(), to: "users/bob".into(), edge_type: "follows".into() },
            WalEntry::LinkMeta { from: "a".into(), to: "b".into(), edge_type: "rated".into(), meta: r#"{"score":5}"#.into() },
            WalEntry::Unlink { from: "a".into(), to: "b".into(), edge_type: "follows".into() },
            WalEntry::CreateTable { collection: "users".into(), schema_json: r#"{"fields":[{"name":"name","type":"text"}]}"#.into() },
            WalEntry::PutVector { slug: "docs/d1".into(), field: "embedding".into(), data: vec![0.1, 0.2, 0.3, 0.4] },
            WalEntry::CreateIndex { collection: "docs".into(), method: "bm25".into(), fields: vec!["title".into(), "body".into()] },
            WalEntry::DropTable { collection: "old_table".into() },
            WalEntry::DropIndex { collection: "docs".into(), method: "gin".into(), field: "title".into() },
            WalEntry::AlterTable { collection: "users".into(), op_json: r#"{"AddColumn":{"name":"email","type":"text"}}"#.into() },
            WalEntry::TxnBegin,
            WalEntry::TxnEnd,
        ]
    }

    // ── JSON format tests ────────────────────────────────────────────────────

    #[test]
    fn write_and_read_put() {
        let (entries, corrupted) = roundtrip(vec![WalEntry::Put {
            slug: "alice".into(),
            payload: r#"{"name":"Alice"}"#.into(),
        }]);
        assert!(!corrupted);
        assert_eq!(entries.len(), 1);
        assert!(matches!(&entries[0], WalEntry::Put { slug, .. } if slug == "alice"));
    }

    #[test]
    fn write_multiple_ops() {
        let (entries, corrupted) = roundtrip(vec![
            WalEntry::Put { slug: "alice".into(), payload: "{}".into() },
            WalEntry::Link { from: "alice".into(), to: "bob".into(), edge_type: "follows".into() },
            WalEntry::Remove { slug: "alice".into() },
        ]);
        assert!(!corrupted);
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn bad_crc_stops_replay() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp);

        let mut w = WalWriter::open(&path).unwrap();
        w.append(&WalEntry::Put { slug: "a".into(), payload: "{}".into() }).unwrap();
        drop(w);

        let mut data = std::fs::read(&path).unwrap();
        let mid = data.len() / 2;
        data[mid] ^= 0xff;
        std::fs::write(&path, &data).unwrap();

        let (entries, corrupted) = WalReader::open(&path).unwrap().read_all();
        assert!(corrupted);
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn partial_frame_detected() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp);

        let mut w = WalWriter::open(&path).unwrap();
        w.append(&WalEntry::Put { slug: "x".into(), payload: "{}".into() }).unwrap();
        drop(w);

        let data = std::fs::read(&path).unwrap();
        std::fs::write(&path, &data[..data.len() / 2]).unwrap();

        let (entries, corrupted) = WalReader::open(&path).unwrap().read_all();
        assert!(corrupted);
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn unknown_wal_entry_is_skipped() {
        let raw = br#"{"op":"future_feature","x":1}"#;
        let entry: WalEntry = serde_json::from_slice(raw).expect("should deserialise");
        assert!(matches!(entry, WalEntry::Unknown), "unknown op must yield WalEntry::Unknown");
    }

    #[test]
    fn good_entries_before_corruption_are_preserved() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp);

        let mut w = WalWriter::open(&path).unwrap();
        w.append(&WalEntry::Put { slug: "a".into(), payload: "{}".into() }).unwrap();
        w.append(&WalEntry::Put { slug: "b".into(), payload: "{}".into() }).unwrap();
        drop(w);

        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&[0xde, 0xad, 0xbe, 0xef, 0x05, 0x00, 0x00, 0x00, b'b', b'a', b'd', 0x00, 0x00]).unwrap();
        drop(f);

        let (entries, corrupted) = WalReader::open(&path).unwrap().read_all();
        assert!(corrupted);
        assert_eq!(entries.len(), 2);
    }

    // ── Binary format tests ──────────────────────────────────────────────────

    #[test]
    fn binary_roundtrip_all_variants() {
        let (entries, corrupted) = roundtrip_format(all_entry_variants(), WalFormat::Binary);
        assert!(!corrupted);
        assert_eq!(entries.len(), 13);
        assert!(matches!(&entries[0], WalEntry::Put { slug, payload }
            if slug == "users/alice" && payload.contains("Alice")));
        assert!(matches!(&entries[1], WalEntry::Remove { slug } if slug == "users/bob"));
        assert!(matches!(&entries[2], WalEntry::Link { from, edge_type, .. } if from == "users/alice" && edge_type == "follows"));
        assert!(matches!(&entries[6], WalEntry::PutVector { data, .. } if data.len() == 4));
        assert!(matches!(&entries[7], WalEntry::CreateIndex { fields, .. } if fields.len() == 2));
        assert!(matches!(&entries[11], WalEntry::TxnBegin));
        assert!(matches!(&entries[12], WalEntry::TxnEnd));
    }

    #[test]
    fn json_roundtrip_all_variants() {
        let (entries, corrupted) = roundtrip_format(all_entry_variants(), WalFormat::Json);
        assert!(!corrupted);
        assert_eq!(entries.len(), 13);
    }

    #[test]
    fn binary_bad_crc_stops_replay() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp);

        let mut w = WalWriter::open_with_format(&path, WalFormat::Binary).unwrap();
        w.append(&WalEntry::Put { slug: "a".into(), payload: "{}".into() }).unwrap();
        drop(w);

        let mut data = std::fs::read(&path).unwrap();
        let mid = data.len() / 2;
        data[mid] ^= 0xff;
        std::fs::write(&path, &data).unwrap();

        let (entries, corrupted) = WalReader::open(&path).unwrap().read_all();
        assert!(corrupted);
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn format_auto_detection() {
        let tmp_j = NamedTempFile::new().unwrap();
        let path_j = tmp_j.path().to_path_buf();
        drop(tmp_j);
        let tmp_b = NamedTempFile::new().unwrap();
        let path_b = tmp_b.path().to_path_buf();
        drop(tmp_b);

        let mut wj = WalWriter::open_with_format(&path_j, WalFormat::Json).unwrap();
        wj.append(&WalEntry::TxnBegin).unwrap();
        drop(wj);

        let mut wb = WalWriter::open_with_format(&path_b, WalFormat::Binary).unwrap();
        wb.append(&WalEntry::TxnBegin).unwrap();
        drop(wb);

        assert_eq!(WalReader::open(&path_j).unwrap().format(), WalFormat::Json);
        assert_eq!(WalReader::open(&path_b).unwrap().format(), WalFormat::Binary);
    }

    #[test]
    fn reopen_appends_same_format() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp);

        let mut w = WalWriter::open_with_format(&path, WalFormat::Binary).unwrap();
        w.append(&WalEntry::Put { slug: "a".into(), payload: "{}".into() }).unwrap();
        drop(w);

        // Reopen — should detect binary, not switch to json even if we say json
        let mut w2 = WalWriter::open_with_format(&path, WalFormat::Json).unwrap();
        assert_eq!(w2.format(), WalFormat::Binary);
        w2.append(&WalEntry::Put { slug: "b".into(), payload: "{}".into() }).unwrap();
        drop(w2);

        let (entries, corrupted) = WalReader::open(&path).unwrap().read_all();
        assert!(!corrupted);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn migrate_json_to_binary() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp);

        let orig = all_entry_variants();
        let mut w = WalWriter::open_with_format(&path, WalFormat::Json).unwrap();
        for e in &orig {
            w.append(e).unwrap();
        }
        drop(w);

        let count = migrate_wal(&path, WalFormat::Binary).unwrap();
        assert_eq!(count, 13);

        let reader = WalReader::open(&path).unwrap();
        assert_eq!(reader.format(), WalFormat::Binary);
        let (entries, corrupted) = reader.read_all();
        assert!(!corrupted);
        assert_eq!(entries.len(), 13);
    }

    #[test]
    fn migrate_same_format_is_noop() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp);

        let mut w = WalWriter::open_with_format(&path, WalFormat::Binary).unwrap();
        w.append(&WalEntry::TxnBegin).unwrap();
        drop(w);

        let count = migrate_wal(&path, WalFormat::Binary).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn binary_encode_is_smaller_than_json() {
        let entry = WalEntry::Put {
            slug: "users/alice".into(),
            payload: r#"{"name":"Alice","age":30}"#.into(),
        };
        let json_bytes = serde_json::to_vec(&entry).unwrap();
        let binary_bytes = binary_encode(&entry);
        assert!(binary_bytes.len() < json_bytes.len(),
            "binary {} bytes should be smaller than json {} bytes",
            binary_bytes.len(), json_bytes.len());
    }

    #[test]
    fn binary_unknown_tag_yields_unknown() {
        let data = vec![254]; // unknown tag
        let entry = binary_decode(&data);
        assert!(matches!(entry, Some(WalEntry::Unknown)));
    }

    /// **A frame must not be able to ask for memory it cannot contain.**
    ///
    /// The element count in a vector frame is four bytes of file, and it was passed
    /// straight to `Vec::with_capacity` before a single element was read. A
    /// thirty-byte frame declaring `0xFFFF_FFFF` elements therefore asked for
    /// roughly 16 GB and then failed as "corrupted" — a memory-exhaustion denial of
    /// service out of a tiny crafted file. A CRC does not stop this: it proves the
    /// bytes are intact, not that they are friendly.
    #[test]
    fn a_lying_element_count_does_not_reserve_the_world() {
        // A PutVector frame: tag, slug, field, then a count with no body behind it.
        let mut frame = Vec::new();
        frame.push(6u8); // TAG_PUT_VECTOR
        for s in ["p/n1", "emb"] {
            frame.extend_from_slice(&(s.len() as u32).to_le_bytes());
            frame.extend_from_slice(s.as_bytes());
        }
        frame.extend_from_slice(&u32::MAX.to_le_bytes()); // "four billion floats follow"

        // Decoding must decline rather than allocate against the claim. Reaching
        // this line at all is the assertion: a reservation of that size either
        // aborts the process or leaves it swapping.
        let got = binary_decode(&frame);
        assert!(got.is_none(), "a frame claiming {} elements with no body decoded", u32::MAX);

        // And an honest frame still round-trips, so the cap did not break decoding.
        let mut good = Vec::new();
        good.push(6u8);
        for s in ["p/n1", "emb"] {
            good.extend_from_slice(&(s.len() as u32).to_le_bytes());
            good.extend_from_slice(s.as_bytes());
        }
        good.extend_from_slice(&3u32.to_le_bytes());
        for v in [1.0f32, 2.0, 3.0] { good.extend_from_slice(&v.to_le_bytes()); }
        match binary_decode(&good) {
            Some(WalEntry::PutVector { slug, field, data }) => {
                assert_eq!(slug, "p/n1");
                assert_eq!(field, "emb");
                assert_eq!(data, vec![1.0, 2.0, 3.0]);
            }
            other => panic!("an honest vector frame no longer decodes: {other:?}"),
        }
    }
}
