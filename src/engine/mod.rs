//! Engine — concurrent, multi-model wrapper for [`CoreDB`].
//!
//! The engine layer sits on top of the raw [`CoreDB`] and adds:
//!
//! - **RwLock-based concurrency** — multiple readers proceed in parallel;
//!   writes take an exclusive lock but hold it only for the duration of the
//!   mutation (microseconds with incremental HNSW).
//! - **Write buffering** — accumulate SQL statements and apply them in one
//!   short lock acquisition via [`Engine::flush()`].
//!
//! The engine exposes the **full** Sekejap SQL surface — graph traversals,
//! spatial queries, vector search, full-text search, and standard CRUD — all
//! through [`Engine::query()`] and [`Engine::execute()`].
//!
//! # Quick start
//!
//! ```rust,no_run
//! use sekejap::engine::Engine;
//!
//! // Open (or create) a persistent database
//! let engine = Engine::builder("/tmp/mydb")
//!     .buffer_size(100)
//!     .build()
//!     .unwrap();
//!
//! // Reads — shared lock, concurrent
//! let hits = engine.query("SELECT name FROM users").unwrap();
//!
//! // Writes — exclusive lock (or buffered)
//! engine.execute("INSERT INTO users (_key, name) VALUES ('alice', 'Alice')").unwrap();
//!
//! // Graph
//! engine.execute("LINK alice -> bob AS follows WEIGHT 1.0").unwrap();
//!
//! // Vector
//! engine.execute("INSERT INTO docs (_key, emb) VALUES ('d1', [1.0, 0.0, 0.0])").unwrap();
//! let similar = engine.query(
//!     "SELECT _key FROM docs WHERE VECTOR_NEAR(emb, [1.0, 0.1, 0.0], 10)"
//! ).unwrap();
//!
//! // Flush buffered writes + maybe compact WAL
//! engine.flush().unwrap();
//! ```

pub mod buffer;
pub mod guard;

use buffer::{RowBuffer, WriteBuffer};
use guard::ReadWriteGuard;

use crate::query::Hit;
use crate::CoreDB;
use serde_json::Value;

/// Concurrent database engine wrapping [`CoreDB`].
///
/// Provides thread-safe read/write access to the full Sekejap multi-model
/// database: graph, spatial, vector, full-text, and relational — all via SQL.
///
/// Use [`Engine::builder()`] for persistent databases or [`Engine::memory()`]
/// for ephemeral in-memory instances.
pub struct Engine {
    guard: ReadWriteGuard,
    buffer: Option<WriteBuffer>,
    /// Prepared-row buffer (pre-built (slug, json)); group-committed via put_many.
    rows: Option<RowBuffer>,
    read_only: bool,
    /// Safety-valve caps for [`Engine::scan`] — max rows and max payload bytes a
    /// single scan will collect (`None` = unbounded). Guard against a runaway
    /// full-collection read on the shared engine.
    scan_max_rows: Option<usize>,
    scan_max_bytes: Option<usize>,
    /// Opt-in lock-free read path (snapshot reads). `Some` only when the engine was
    /// built with `.snapshot_reads(true)` on a writable paged-mode database (unix).
    /// See [`Engine::get`] and `docs/developer/notes/snapshot-reads-design.md`.
    #[cfg(unix)]
    published: Option<Published>,
}

/// The engine's published snapshot: a shared, periodically-refreshed "photograph"
/// of the store that readers read **lock-free** (a read-only [`CoreDB`]).
///
/// The writer re-mints it after a committed write, but **at most once per
/// `interval`** (write-debounced): minting copies the write overlay, so this keeps
/// that cost off the per-write path and off the reader entirely. Readers only ever
/// bump an `Arc` out of `slot` and read from their own copy — the writer swapping in
/// a fresh photo never blocks them. Staleness is bounded by `interval`.
#[cfg(unix)]
struct Published {
    /// The latest published photo. A reader takes it with a tiny slot-lock + `Arc`
    /// bump (nothing to do with the DB lock); the writer swaps a fresh one in.
    slot: std::sync::RwLock<std::sync::Arc<CoreDB>>,
    /// When the slot was last re-minted — the debounce clock.
    last: std::sync::Mutex<std::time::Instant>,
    /// Minimum time between re-mints (staleness bound).
    interval: std::time::Duration,
}

#[cfg(unix)]
impl Published {
    /// The current published photo (lock-free read path for callers).
    fn current(&self) -> std::sync::Arc<CoreDB> {
        self.slot.read().expect("snapshot slot poisoned").clone()
    }
}

/// A compiled INSERT template (collection + columns), prepared once and reused.
/// Carries no SQL — `Engine::insert_prepared` binds values and writes directly,
/// so per-row parsing/compilation is eliminated. Cheap to clone and share.
#[derive(Clone, Debug)]
pub struct PreparedInsert {
    collection: String,
    columns: Vec<String>,
    key_idx: usize,
}

impl Engine {
    /// Start building an Engine from a database directory path.
    ///
    /// The directory will be created if it does not exist. Pass the same path
    /// to reopen an existing database.
    pub fn builder(path: &str) -> EngineBuilder {
        EngineBuilder {
            path: path.to_string(),
            buffer_size: None,
            read_only: false,
            snapshot_reads: false,
            publish_interval: std::time::Duration::from_millis(5),
            scan_max_rows: None,
            scan_max_bytes: None,
        }
    }

    /// Open a database that runs **as a service** — something long-lived that keeps
    /// running and looks after its own data: a small server, a robot, an IoT
    /// gateway. Even when it serves only one person.
    ///
    /// Use [`CoreDB::open`] instead when the database is simply part of your app and
    /// starts and stops with it (a mobile app, a game, an analysis script).
    ///
    /// What you get here that a plain open doesn't give you:
    ///
    /// - **Reads don't wait for writes.** Readers work from a shared point-in-time
    ///   snapshot the writer refreshes after commits, so a long write never freezes
    ///   them. (Today this covers [`get`](Engine::get), [`scan`](Engine::scan) and
    ///   [`count`](Engine::count); indexed `query()` still takes a shared lock.)
    /// - **Memory stays bounded** over weeks of running, because the store is opened
    ///   in paged mode: the bulk lives in memory-mapped files, not the heap.
    /// - **It compacts itself** as the write-ahead log grows.
    /// - **Many threads can share it** — reads run in parallel, writes take turns.
    ///
    /// There is still no separate server process: these are functions your app calls,
    /// in your app's own process.
    ///
    /// ```rust,no_run
    /// let db = sekejap::open_as_service("/var/lib/app/db")?;
    /// let row = db.get("venues/v1");          // lock-free, even while writing
    /// db.execute("INSERT INTO venues (_key) VALUES ('v2')")?;
    /// # Ok::<(), String>(())
    /// ```
    ///
    /// For finer control (read-only, buffer size, staleness bound, scan caps) use
    /// [`Engine::builder`].
    pub fn open_as_service(path: &str) -> Result<Engine, String> {
        Engine::builder(path).snapshot_reads(true).build()
    }

    /// Create an in-memory Engine (no persistence).
    pub fn memory() -> Self {
        Self {
            guard: ReadWriteGuard::new(CoreDB::new()),
            buffer: None,
            rows: None,
            read_only: false,
            scan_max_rows: None,
            scan_max_bytes: None,
            #[cfg(unix)]
            published: None,
        }
    }

    /// Whether this engine is in read-only mode.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    // ── Reads ────────────────────────────────────────────────────────────────

    /// Execute a read-only SQL query. Takes a shared read lock.
    ///
    /// Multiple `query()` calls from different threads proceed concurrently.
    /// Supports the full Sekejap SQL surface: `SELECT`, `MATCH`, `VECTOR_NEAR`,
    /// `GEO_NEAR`, `BM25_SEARCH`, etc.
    pub fn query(&self, sql: &str) -> Result<Vec<Hit>, String> {
        #[cfg(unix)]
        if let Some(p) = &self.published {
            // Lock-free: the snapshot is a read-only CoreDB, so the normal executor
            // runs against it without touching the live database or its write lock.
            return p.current().query(sql).map(|set| set.collect()).map_err(|e| e.to_string());
        }
        let db = self.guard.read();
        db.query(sql)
            .map(|set| set.collect())
            .map_err(|e| e.to_string())
    }

    /// Execute a read-only SQL query with parameter bindings.
    ///
    /// Parameters are referenced as `$1`, `$2`, ... in the SQL string.
    /// Takes a shared read lock (concurrent with other readers).
    pub fn query_params(&self, sql: &str, params: &[Value]) -> Result<Vec<Hit>, String> {
        #[cfg(unix)]
        if let Some(p) = &self.published {
            return p.current().query_params(sql, params)
                .map(|set| set.collect())
                .map_err(|e| e.to_string());
        }
        let db = self.guard.read();
        db.query_params(sql, params)
            .map(|set| set.collect())
            .map_err(|e| e.to_string())
    }

    // ── Snapshot reads (lock-free point reads) ────────────────────────────────

    /// Point read: the JSON payload for `slug`, or `None` if absent.
    ///
    /// When the engine was built with [`snapshot_reads`](EngineBuilder::snapshot_reads),
    /// this reads a shared point-in-time snapshot **lock-free** — it never queues
    /// behind a writer, even a long one (bulk import, index rebuild). Otherwise it
    /// falls back to a normal shared read lock (still concurrent with other readers,
    /// but blocked while a writer holds the exclusive lock).
    ///
    /// The snapshot is "as of" the last publish (bounded by the configured interval),
    /// so a `get` right after your own `execute` may not yet reflect it — call
    /// [`refresh_snapshot`](Self::refresh_snapshot) first, or use [`query`](Self::query)
    /// (locked) for read-your-own-write.
    pub fn get(&self, slug: &str) -> Option<String> {
        #[cfg(unix)]
        if let Some(p) = &self.published {
            return p.current().get(slug);
        }
        self.guard.read().get(slug)
    }

    /// Hand out the engine's current published snapshot for several consistent
    /// lock-free reads (all against the same instant). `None` if snapshot reads are
    /// not enabled. Keep it short-lived — a live snapshot pins the memory it references.
    #[cfg(unix)]
    pub fn snapshot(&self) -> Option<std::sync::Arc<CoreDB>> {
        self.published.as_ref().map(|p| p.current())
    }

    /// Scan a whole collection: every member node's JSON payload. Like [`get`](Self::get),
    /// this is **lock-free** when snapshot reads are enabled (never queues behind a
    /// writer), else it falls back to a shared read lock. Unindexed — it reads every
    /// member's payload, so it's for list endpoints on bounded collections, not a
    /// substitute for an indexed `query("SELECT ... WHERE ...")`.
    pub fn scan(&self, collection: &str) -> Vec<String> {
        let (rows, bytes) = (self.scan_max_rows, self.scan_max_bytes);
        #[cfg(unix)]
        if let Some(p) = &self.published {
            return p.current().collection_payloads_bounded(collection, rows, bytes);
        }
        self.guard.read().collection_payloads_bounded(collection, rows, bytes)
    }

    /// Count the members of `collection` — lock-free when snapshot reads are on,
    /// else via a shared read lock. The `COUNT(*)` companion to [`scan`](Self::scan).
    pub fn count(&self, collection: &str) -> usize {
        #[cfg(unix)]
        if let Some(p) = &self.published {
            return p.current().collection_count(collection);
        }
        self.guard.read().collection_count(collection)
    }

    /// Force an immediate re-mint of the published snapshot (bypasses the debounce),
    /// so subsequent [`get`](Self::get) calls see everything committed up to now.
    /// No-op if snapshot reads are not enabled. Use for read-your-own-write.
    #[cfg(unix)]
    pub fn refresh_snapshot(&self) {
        self.republish_now();
    }

    /// Re-mint the published snapshot unconditionally (if enabled). Takes a brief
    /// shared read lock to freeze the current view; readers keep using the old photo
    /// until the new one is swapped in.
    #[cfg(unix)]
    fn republish_now(&self) {
        if let Some(p) = &self.published {
            if let Some(snap) = self.guard.read().snapshot_db() {
                *p.slot.write().expect("snapshot slot poisoned") = snap;
                *p.last.lock().expect("snapshot clock poisoned") = std::time::Instant::now();
            }
        }
    }

    /// Re-mint the published snapshot only if `interval` has elapsed since the last
    /// mint (write-debounced). Called after each committed write, so republish cost
    /// is bounded to once per interval regardless of write rate.
    #[cfg(unix)]
    fn maybe_republish(&self) {
        if let Some(p) = &self.published {
            let due = {
                let last = p.last.lock().expect("snapshot clock poisoned");
                last.elapsed() >= p.interval
            };
            if due {
                self.republish_now();
            }
        }
    }

    /// No-op stand-in on non-unix (snapshot reads are unix-only).
    #[cfg(not(unix))]
    #[inline]
    fn maybe_republish(&self) {}

    // ── Writes ───────────────────────────────────────────────────────────────

    /// Execute a write SQL statement.
    ///
    /// If a write buffer is configured, the statement is buffered and only
    /// applied when the threshold is reached or [`flush()`](Self::flush) is called.
    /// Without a buffer, the write is applied immediately.
    ///
    /// Returns the number of affected rows.
    /// Returns an error if the engine is in read-only mode.
    pub fn execute(&self, sql: &str) -> Result<usize, String> {
        if self.read_only {
            return Err("database is read-only".to_string());
        }
        if let Some(ref buf) = self.buffer {
            let should_flush = buf.push(sql.to_string());
            if should_flush {
                return self.flush();
            }
            return Ok(0);
        }
        // No buffer — apply immediately, then refresh the published snapshot
        // (debounced) so lock-free readers pick up the change.
        let n = {
            let mut db = self.guard.write();
            db.execute(sql).map_err(|e| e.to_string())?
        };
        self.maybe_republish();
        Ok(n)
    }

    /// Execute a write SQL statement with parameter bindings.
    ///
    /// Parameters are referenced as `$1`, `$2`, ... in the SQL string.
    /// Always applied immediately (bypasses the write buffer).
    /// Takes an exclusive write lock.
    ///
    /// Returns an error if the engine is in read-only mode.
    pub fn execute_params(&self, sql: &str, params: &[Value]) -> Result<usize, String> {
        if self.read_only {
            return Err("database is read-only".to_string());
        }
        let n = {
            let mut db = self.guard.write();
            db.execute_params(sql, params).map_err(|e| e.to_string())?
        };
        self.maybe_republish();
        Ok(n)
    }

    // ── Prepared inserts (no SQL parsing on the hot path) ──────────────────────

    /// Prepare a typed INSERT template ONCE — no SQL, nothing parsed per row.
    /// `columns` must include `"_key"` (used to derive the node slug). Reuse the
    /// returned handle across many `insert_prepared` calls: the IoT write path.
    pub fn prepare_insert(&self, collection: &str, columns: &[&str]) -> Result<PreparedInsert, String> {
        let key_idx = columns.iter().position(|c| *c == "_key")
            .ok_or_else(|| "prepare_insert requires a '_key' column".to_string())?;
        Ok(PreparedInsert {
            collection: collection.to_string(),
            columns: columns.iter().map(|s| s.to_string()).collect(),
            key_idx,
        })
    }

    /// Execute a prepared insert with bound `params` (one per column). Skips ALL
    /// SQL parsing — builds the payload directly. Buffered + group-committed when
    /// a buffer is configured (call [`flush`](Self::flush) to drain), else applied
    /// immediately. `params` length must equal the prepared column count.
    pub fn insert_prepared(&self, p: &PreparedInsert, params: &[Value]) -> Result<(), String> {
        if self.read_only {
            return Err("database is read-only".to_string());
        }
        if params.len() != p.columns.len() {
            return Err(format!("expected {} params, got {}", p.columns.len(), params.len()));
        }
        let key = match &params[p.key_idx] {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let slug = format!("{}/{}", p.collection, key);
        let mut obj = serde_json::Map::with_capacity(p.columns.len() + 1);
        obj.insert("_collection".to_string(), Value::String(p.collection.clone()));
        for (c, v) in p.columns.iter().zip(params) {
            obj.insert(c.clone(), v.clone());
        }
        let val = Value::Object(obj);
        if let Some(ref rb) = self.rows {
            if rb.push(slug, val) {
                self.flush()?; // group-commit at threshold
            }
            Ok(())
        } else {
            {
                let mut db = self.guard.write();
                db.put_value(&slug, val).map_err(|e| e.to_string())?;
            }
            self.maybe_republish();
            Ok(())
        }
    }

    // ── Flush & Maintenance ──────────────────────────────────────────────────

    /// Drain the write buffer, apply all pending statements, and optionally
    /// compact the WAL if the policy threshold is exceeded.
    ///
    /// Returns the total number of rows affected across all flushed statements.
    /// Returns `Ok(0)` if no buffer is configured or the buffer is empty.
    pub fn flush(&self) -> Result<usize, String> {
        let statements = self.buffer.as_ref().map(|b| b.drain()).unwrap_or_default();
        let rows = self.rows.as_ref().map(|r| r.drain()).unwrap_or_default();
        if statements.is_empty() && rows.is_empty() {
            return Ok(0);
        }

        let mut db = self.guard.write();
        let mut total = 0usize;
        // Prepared rows: pre-built (slug, Value) → put_value_bulk (group-commit, one
        // shared timestamp, zero parsing). The fast IoT write path.
        if !rows.is_empty() {
            total += db.put_value_bulk(rows).map_err(|e| e.to_string())?;
        }
        // Buffered SQL: apply the whole batch under one WAL fsync (group-commit).
        if !statements.is_empty() {
            total += db.execute_batch_grouped(&statements).map_err(|e| e.to_string())?;
        }

        // Auto-compaction is core's job (`CoreDB::set_auto_compact` +
        // `CompactThresholds`), applied inside the bulk write paths above.
        drop(db); // release the write lock before re-minting the read snapshot
        self.maybe_republish();

        Ok(total)
    }

    /// What the database is holding and what it has done — see [`CoreDB::stats`].
    ///
    /// Always read from the **live** database, not the published snapshot, so the
    /// numbers describe the real store (overlay size, WAL size, compaction timings)
    /// rather than a frozen copy of it.
    pub fn stats(&self) -> crate::Stats {
        self.guard.read().stats()
    }

    /// Reclaim excess in-RAM capacity on demand (see [`CoreDB::trim_memory`]).
    /// Cheap and safe — never drops data or indexes; query results are unchanged.
    /// Briefly takes the exclusive write lock. For deeper reclaim use [`compact`](Self::compact).
    pub fn trim_memory(&self) {
        self.guard.write().trim_memory();
    }

    /// Force WAL compaction regardless of policy.
    ///
    /// Rewrites the snapshot + payloads.bin and truncates the WAL log to zero.
    /// Takes an exclusive write lock for the duration of the compaction.
    ///
    /// When an S3 remote is configured (feature `s3`), uploads the compacted
    /// segments after successful local compaction.
    ///
    /// Returns an error if the engine is in read-only mode.
    pub fn compact(&self) -> Result<(), String> {
        if self.read_only {
            return Err("database is read-only".to_string());
        }
        {
            let mut db = self.guard.write();
            db.compact().map_err(|e| e.to_string())?;
        }
        // Compaction swaps in a fresh (empty-overlay) base — re-mint now, bypassing
        // the debounce, so lock-free readers move onto the compacted base promptly.
        #[cfg(unix)]
        self.republish_now();


        Ok(())
    }

    /// Manually sync local segments to S3.
    ///
    /// Uploads all segment files and writes a new manifest.
    /// Only available when the `s3` feature is enabled and a remote is configured.

    /// Check S3 for a newer manifest and hot-swap the local database.
    ///
    /// Returns `true` if the database was refreshed (newer generation found),
    /// `false` if already up-to-date.
    ///
    /// Only available in read-only mode with an S3 remote configured.
    /// During the swap, queries briefly block (microseconds) while the inner
    /// `CoreDB` is replaced — in-flight reads continue on the old data.

    /// The current manifest generation this engine was opened from.
    ///
    /// Increments after each successful [`compact()`](Self::compact) upload
    /// or [`refresh()`](Self::refresh) download.

    /// Consume the engine and return the inner `CoreDB`.
    pub fn into_inner(self) -> CoreDB {
        self.guard.into_inner()
    }
}

/// Builder for configuring an [`Engine`] instance.
///
/// Obtained via [`Engine::builder()`]. All settings have sensible defaults
/// — you only need to call [`build()`](Self::build) to get a working engine.
///
/// # Example
///
/// ```rust,no_run
/// use sekejap::engine::Engine;
///
/// let engine = Engine::builder("/tmp/mydb")
///     .buffer_size(100)                       // buffer 100 writes before flush
///     .build()
///     .unwrap();
/// ```
pub struct EngineBuilder {
    path: String,
    buffer_size: Option<usize>,
    read_only: bool,
    /// Opt-in lock-free snapshot reads (implies paged-mode open). unix-only.
    snapshot_reads: bool,
    /// Staleness bound / debounce for the published snapshot (default 5 ms).
    publish_interval: std::time::Duration,
    /// Safety-valve caps for `Engine::scan` (`None` = unbounded).
    scan_max_rows: Option<usize>,
    scan_max_bytes: Option<usize>,
}

impl EngineBuilder {
    /// Set the write buffer threshold (number of statements before auto-flush).
    /// Pass `0` or omit to disable buffering.
    pub fn buffer_size(mut self, size: usize) -> Self {
        self.buffer_size = if size > 0 { Some(size) } else { None };
        self
    }

    /// Open the database in read-only mode.
    ///
    /// Read-only engines skip the exclusive file lock and WAL writer.
    /// Write operations ([`execute`](Engine::execute), [`compact`](Engine::compact))
    /// return an error. Use [`refresh()`](Engine::refresh) (with S3) to pull
    /// newer data published by a writer.
    pub fn read_only(mut self, ro: bool) -> Self {
        self.read_only = ro;
        self
    }

    /// Enable lock-free **snapshot reads** for [`Engine::get`] (unix-only).
    ///
    /// This opens the database in **paged mode** (the immutable-base layout that
    /// makes a store snapshottable) and maintains a shared point-in-time snapshot
    /// that the writer re-mints after commits. Point reads (`get`) then never queue
    /// behind a writer — the payoff for read-heavy, concurrent (server) workloads.
    ///
    /// Trade-offs: reads are "as of" the last publish (see
    /// [`publish_interval`](Self::publish_interval)); it needs paged mode; and it is
    /// a no-op in read-only mode and on non-unix. Ignored for single-threaded /
    /// embedded use, which should leave it off (the default) and pay nothing.
    pub fn snapshot_reads(mut self, enabled: bool) -> Self {
        self.snapshot_reads = enabled;
        self
    }

    /// Staleness bound for [`snapshot_reads`](Self::snapshot_reads): the minimum time
    /// between snapshot re-mints (default 5 ms). Smaller = fresher reads but more
    /// overlay copies; larger = cheaper but staler. Compaction always re-mints
    /// immediately regardless of this.
    pub fn publish_interval(mut self, interval: std::time::Duration) -> Self {
        self.publish_interval = interval;
        self
    }

    /// Cap the number of rows a single [`Engine::scan`] collects (`0` / unset =
    /// unbounded). A safety valve so one `scan` of an unexpectedly huge collection
    /// can't allocate without bound on the shared engine. `scan` returns *up to*
    /// this many rows (like an implicit `LIMIT`).
    pub fn max_scan_rows(mut self, rows: usize) -> Self {
        self.scan_max_rows = if rows > 0 { Some(rows) } else { None };
        self
    }

    /// Cap the total payload bytes a single [`Engine::scan`] collects (`0` / unset =
    /// unbounded). Stops once the next payload would exceed the cap, but always
    /// returns at least one row so a single oversized payload isn't silently dropped.
    pub fn max_scan_bytes(mut self, bytes: usize) -> Self {
        self.scan_max_bytes = if bytes > 0 { Some(bytes) } else { None };
        self
    }

    /// Configure S3 remote storage for segment sync.
    ///
    /// The URL should be `s3://bucket-name/optional/prefix`.
    /// Credentials must be set via [`.credentials()`](Self::credentials).
    ///
    /// On `build()`, any remote segments missing locally will be downloaded.
    /// On `compact()`, compacted segments will be uploaded.

    /// Set S3 credentials. Required when using `.remote()`.

    /// Enable remote-only mode: payloads stay on S3, fetched on demand.
    ///
    /// Only the snapshot (node index) is downloaded. Payload reads go through
    /// a bounded LRU block cache backed by S3 `GET_RANGE`. This allows
    /// querying datasets much larger than local disk.
    ///
    /// Implies `read_only(true)`. Requires `.remote(url)`.

    /// Set the local block cache budget for remote-only mode.
    /// Default: 10 GB.

    /// Set a local disk cache directory for remote-only mode.
    ///
    /// Blocks evicted from the in-memory tier are written here (64 KB files).
    /// Survives process restarts — blocks are re-discovered on next open.

    /// Build the Engine, opening (or creating) the database at the configured path.
    pub fn build(self) -> Result<Engine, String> {
        // Snapshot reads need paged mode (the immutable base) and a writable engine;
        // unix-only. When on, open paged so the store is snapshottable.
        let want_paged = cfg!(unix) && self.snapshot_reads && !self.read_only;

        let db = if self.read_only {
            CoreDB::open_read_only(&self.path).map_err(|e| e.to_string())?
        } else if want_paged {
            CoreDB::open_paged(&self.path).map_err(|e| e.to_string())?
        } else {
            CoreDB::open(&self.path).map_err(|e| e.to_string())?
        };

        // Seed the initial published snapshot (unix + paged). If the store isn't
        // actually snapshottable, this stays None and `get` uses the read lock.
        #[cfg(unix)]
        let published = if want_paged {
            db.snapshot_db().map(|snap| Published {
                slot: std::sync::RwLock::new(snap),
                last: std::sync::Mutex::new(std::time::Instant::now()),
                interval: self.publish_interval,
            })
        } else {
            None
        };

        Ok(Engine {
            guard: ReadWriteGuard::new(db),
            buffer: if self.read_only {
                None
            } else {
                self.buffer_size.map(WriteBuffer::new)
            },
            rows: if self.read_only {
                None
            } else {
                self.buffer_size.map(RowBuffer::new)
            },
            read_only: self.read_only,
            scan_max_rows: self.scan_max_rows,
            scan_max_bytes: self.scan_max_bytes,
            #[cfg(unix)]
            published,
        })
    }
}

#[cfg(all(test, unix))]
mod snapshot_read_tests {
    use super::*;
    use serde_json::json;

    /// A paged engine with `snapshot_reads` serves `get` from a lock-free published
    /// snapshot, and a forced refresh makes a just-committed write visible.
    #[test]
    fn engine_snapshot_reads_served_and_refreshed() {
        let dir = tempfile::tempdir().unwrap();
        // Pre-create + compact so the paged open has an immutable base.
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            db.put(
                "venues/v1",
                &json!({"_collection":"venues","_key":"v1","n":1}).to_string(),
            )
            .unwrap();
            db.compact().unwrap();
        }

        let engine = Engine::builder(dir.path().to_str().unwrap())
            .snapshot_reads(true)
            .build()
            .unwrap();

        // Enabled → there is a published snapshot, and get() reads the base through it.
        assert!(engine.snapshot().is_some(), "paged engine should have snapshot reads on");
        assert!(engine.get("venues/v1").unwrap().contains("\"n\":1"));
        assert!(engine.get("venues/nope").is_none());

        // Write, then force a refresh → the lock-free get sees it.
        let ins = engine.prepare_insert("venues", &["_key", "n"]).unwrap();
        engine.insert_prepared(&ins, &[json!("v2"), json!(2)]).unwrap();
        engine.refresh_snapshot();
        assert!(engine.get("venues/v2").is_some(), "get sees committed write after refresh");
    }

    /// Minting must be safe while writes are in flight: the publisher takes the
    /// shared read lock, so a snapshot can only be built between mutations, never
    /// mid-write. Readers must keep seeing consistent, non-empty results throughout.
    #[test]
    fn snapshots_stay_consistent_under_concurrent_writes() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            db.execute("CREATE TABLE v (_key TEXT PRIMARY KEY, cat TEXT, n INTEGER)").unwrap();
            for i in 0..200 {
                db.execute(&format!("INSERT INTO v (_key,cat,n) VALUES ('b{i}','cafe',{i})")).unwrap();
            }
            db.execute("CREATE INDEX ON v USING btree (cat)").unwrap();
            db.compact().unwrap();
        }
        let engine = Arc::new(Engine::open_as_service(dir.path().to_str().unwrap()).unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let bad = Arc::new(AtomicUsize::new(0));

        // Readers: query continuously while the writer runs. Row counts may grow as
        // snapshots are republished, but must never drop below the committed base
        // and must never error.
        let readers: Vec<_> = (0..4).map(|_| {
            let e = Arc::clone(&engine);
            let stop = Arc::clone(&stop);
            let bad = Arc::clone(&bad);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match e.query("SELECT _key FROM v WHERE cat = 'cafe'") {
                        Ok(rows) if rows.len() >= 200 => {}
                        Ok(rows) => { bad.fetch_add(1, Ordering::Relaxed);
                                      eprintln!("short read: {} rows", rows.len()); }
                        Err(err) => { bad.fetch_add(1, Ordering::Relaxed);
                                      eprintln!("query error: {err}"); }
                    }
                }
            })
        }).collect();

        // Writer: mutate steadily, forcing republishes along the way.
        for i in 0..300 {
            engine.execute(&format!("INSERT INTO v (_key,cat,n) VALUES ('w{i}','cafe',{i})")).unwrap();
            if i % 50 == 0 { engine.refresh_snapshot(); }
        }
        stop.store(true, Ordering::Relaxed);
        for r in readers { r.join().unwrap(); }

        assert_eq!(bad.load(Ordering::Relaxed), 0, "no reader saw a short or failed read");
        engine.refresh_snapshot();
        assert_eq!(engine.query("SELECT _key FROM v").unwrap().len(), 500,
            "every committed write is visible after a final refresh");
    }

    /// The point of the whole exercise: on a service engine, `query()` — indexed
    /// SQL, not just point reads — runs against the published snapshot rather than
    /// taking the shared read lock.
    #[test]
    fn service_query_runs_on_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            db.execute("CREATE TABLE venues (_key TEXT PRIMARY KEY, cat TEXT, n INTEGER)").unwrap();
            for i in 0..30 {
                let cat = ["cafe", "bar"][i % 2];
                db.execute(&format!(
                    "INSERT INTO venues (_key, cat, n) VALUES ('v{i}', '{cat}', {i})"
                )).unwrap();
            }
            db.execute("CREATE INDEX ON venues USING btree (cat)").unwrap();
            db.compact().unwrap();
        }
        let db = Engine::open_as_service(dir.path().to_str().unwrap()).unwrap();

        // Indexed SQL works through the engine.
        let cafes = db.query("SELECT _key FROM venues WHERE cat = 'cafe'").unwrap();
        assert!(!cafes.is_empty(), "indexed WHERE must work on a service engine");
        assert_eq!(db.query("SELECT _key FROM venues").unwrap().len(), 30);

        // A write is not visible until the snapshot is republished — that is the
        // staleness contract, and it proves the read came from the snapshot rather
        // than the live database.
        db.execute("INSERT INTO venues (_key, cat, n) VALUES ('v999', 'cafe', 999)").unwrap();
        let before = db.query("SELECT _key FROM venues").unwrap().len();
        db.refresh_snapshot();
        let after = db.query("SELECT _key FROM venues").unwrap().len();
        assert_eq!(after, 31, "after an explicit refresh the write is visible");
        assert!(before <= after, "reads are served from a point-in-time snapshot");
    }

    /// `open_as_service` must actually deliver the service behaviour: a working
    /// database whose reads are served lock-free from a published snapshot.
    #[test]
    fn open_as_service_gives_lock_free_reads() {
        let dir = tempfile::tempdir().unwrap();
        // Pre-create + compact so the paged open has an immutable base.
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            db.put("venues/v1", &json!({"_collection":"venues","_key":"v1","n":1}).to_string())
                .unwrap();
            db.compact().unwrap();
        }

        let db = Engine::open_as_service(dir.path().to_str().unwrap()).unwrap();

        // The promise: reads come from a snapshot, not the write lock.
        assert!(db.snapshot().is_some(), "service mode must publish a snapshot");
        assert!(db.get("venues/v1").unwrap().contains("\"n\":1"));
        assert_eq!(db.count("venues"), 1);

        // And it is still a normal, writable database.
        let ins = db.prepare_insert("venues", &["_key", "n"]).unwrap();
        db.insert_prepared(&ins, &[json!("v2"), json!(2)]).unwrap();
        db.refresh_snapshot();
        assert_eq!(db.count("venues"), 2);
        assert!(!db.is_read_only());
    }

    /// The plain `open` path stays embedded: no snapshot machinery is created, so a
    /// single-threaded user pays nothing for the service features.
    #[test]
    fn plain_open_stays_embedded() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = crate::open(dir.path()).unwrap();
        db.put("t/a", &json!({"_collection":"t","_key":"a"}).to_string()).unwrap();
        assert!(db.snapshot_db().is_none(), "resident mode is not snapshottable");
        assert!(db.get("t/a").is_some());
    }

    /// Engine scan/count over a collection: lock-free via the published snapshot,
    /// and a forced refresh reflects a committed write.
    #[test]
    fn engine_scan_and_count() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            db.put("venues/v1", &json!({"_collection":"venues","_key":"v1","n":1}).to_string()).unwrap();
            db.compact().unwrap();
        }
        let engine = Engine::builder(dir.path().to_str().unwrap())
            .snapshot_reads(true)
            .build()
            .unwrap();

        assert_eq!(engine.count("venues"), 1);
        assert_eq!(engine.scan("venues").len(), 1);

        let ins = engine.prepare_insert("venues", &["_key", "n"]).unwrap();
        engine.insert_prepared(&ins, &[json!("v2"), json!(2)]).unwrap();
        engine.refresh_snapshot();
        assert_eq!(engine.count("venues"), 2, "scan/count reflect the write after refresh");
        assert_eq!(engine.scan("venues").len(), 2);
    }

    /// `max_scan_rows` caps a scan (safety valve) — via the snapshot and via the
    /// read-lock fallback alike.
    #[test]
    fn engine_scan_respects_row_cap() {
        fn seed(dir: &std::path::Path) {
            let mut db = CoreDB::open(dir).unwrap();
            for i in 0..10 {
                db.put(
                    &format!("t/v{i}"),
                    &json!({"_collection":"t","_key":format!("v{i}")}).to_string(),
                )
                .unwrap();
            }
            db.compact().unwrap();
        }

        // Snapshot path (its own dir — one writer per dir holds the file lock).
        let d1 = tempfile::tempdir().unwrap();
        seed(d1.path());
        let e1 = Engine::builder(d1.path().to_str().unwrap())
            .snapshot_reads(true)
            .max_scan_rows(3)
            .build()
            .unwrap();
        assert_eq!(e1.scan("t").len(), 3, "snapshot scan capped at 3");
        assert_eq!(e1.count("t"), 10, "count is not capped");

        // Read-lock fallback path (no snapshot reads), separate dir.
        let d2 = tempfile::tempdir().unwrap();
        seed(d2.path());
        let e2 = Engine::builder(d2.path().to_str().unwrap())
            .max_scan_rows(4)
            .build()
            .unwrap();
        assert_eq!(e2.scan("t").len(), 4, "fallback scan capped at 4");
    }

    /// Scan/count also work with snapshot reads off (via the read lock).
    #[test]
    fn engine_scan_falls_back_without_snapshot_reads() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            db.put("t/a", &json!({"_collection":"t","_key":"a"}).to_string()).unwrap();
            db.put("t/b", &json!({"_collection":"t","_key":"b"}).to_string()).unwrap();
        }
        let engine = Engine::builder(dir.path().to_str().unwrap()).build().unwrap();
        assert!(engine.snapshot().is_none());
        assert_eq!(engine.count("t"), 2, "count via read-lock fallback");
        assert_eq!(engine.scan("t").len(), 2, "scan via read-lock fallback");
    }

    /// Without `snapshot_reads`, there's no published snapshot and `get` falls back
    /// to the shared read lock (still correct, just not lock-free).
    #[test]
    fn engine_without_snapshot_reads_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            db.put(
                "t/a",
                &json!({"_collection":"t","_key":"a","n":1}).to_string(),
            )
            .unwrap();
        }
        let engine = Engine::builder(dir.path().to_str().unwrap()).build().unwrap();
        assert!(engine.snapshot().is_none(), "not enabled → no published snapshot");
        assert!(engine.get("t/a").is_some(), "get falls back to the read lock");
    }

    /// Snapshot reads only apply to a writable engine — read-only keeps it off.
    #[test]
    fn read_only_has_no_snapshot_reads() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            db.put("t/a", &json!({"_collection":"t","_key":"a"}).to_string()).unwrap();
            db.compact().unwrap();
        }
        let engine = Engine::builder(dir.path().to_str().unwrap())
            .read_only(true)
            .snapshot_reads(true) // requested, but read-only wins
            .build()
            .unwrap();
        assert!(engine.snapshot().is_none(), "read-only engine is not snapshottable");
        assert!(engine.get("t/a").is_some(), "read-only get still works via the lock");
    }
}

