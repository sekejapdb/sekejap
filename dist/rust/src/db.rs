//! `Db`: the one handle an application opens. `docs/dist/RUST_API.md`.

use crate::catalog::{Collection, Field, Index};
use crate::error::{Error, Result};
use crate::rows::{expect_affected, expect_rows, params_of, Rows};
use crate::scan::Scan;
use crate::{Addr, Document, Mode, Storage};
use sekejap_core::collections::{
    CollectionId, CollectionOptions, Database, Direction, DropMode, EntityId, GraphContextId,
    NeighborRequest,
};
use sekejap_core::{Config, Kind, SyncMode};
use sekejap_dist::service::{ServiceDatabase, Snapshot, WriterGuard};
use crate::plans::{CacheStats, PlanCache, Statement};
use sekejap_lang::{prepare_sql, Param, PreparedSql, SqlDatabase, SqlResult};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// The external key, as a document spells it. `Db::get` injects it and
/// `Db::put` takes it off again, so a document read back is a document that
/// can be written back.
pub const KEY: &str = "_key";
/// The default rows a [`Scan`] holds at once. Law 1: no call holds a
/// collection-sized vector.
pub const SCAN_PAGE: usize = 256;
/// The rows [`Db::query`] assembles its answer from, one page at a time.
pub const QUERY_PAGE: usize = 4096;
/// The complete-or-error bound `Database::neighbor_ids` enforces.
pub const MAX_NEIGHBOURS: usize = 256;
/// The keys one drop step removes per call. `MAX_DROP_BATCH` is 256.
const DROP_BATCH: usize = 256;

enum Backing {
    /// One `Mutex<Database>`: readers and the writer share it, so a read
    /// sees the write before it with no publication step.
    Single(Mutex<Database>),
    /// `docs/dist/OPS_CONTRACT.md` §1: one writer, parallel readers on a
    /// published snapshot, a commit-time change feed.
    Service(ServiceDatabase),
}

/// An E4 database, opened once and shared. `Send + Sync` in both modes.
pub struct Db {
    backing: Backing,
    path: PathBuf,
    /// The bounded prepared-plan cache of `QL_CONTRACT` §2. Its three
    /// ceilings are fixed at open (`crate::plans`), and a HIT is a REBIND.
    plans: Mutex<PlanCache>,
    /// The catalog generation every cache key carries. Bumped by every call
    /// that changes the catalog, so a plan compiled against a layout that no
    /// longer exists can never be hit again.
    generation: std::sync::atomic::AtomicU64,
}

impl Db {
    // ── §1 opening ───────────────────────────────────────────────────────

    /// Open the database in `path`, creating it if the directory holds none.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path, Self::config())
    }

    /// The store configuration this crate opens with when the caller names
    /// none: `kernel::store::Config` with [`SyncMode::Normal`], so every
    /// commit is published with a data barrier -- `fdatasync` on Linux, plain
    /// `fsync` on macOS. That is the barrier SQLite issues with `fullfsync`
    /// off (its default everywhere) and the one PostgreSQL issues with a
    /// plain `fsync`, so a sekejap database and one of those two, opened
    /// with nothing named, promise the same thing and cost comparable time.
    ///
    /// WHAT `Normal` PROMISES: every acknowledged commit survives a process
    /// crash, a kill, and an operating-system panic or reboot, because the
    /// bytes are out of the page cache and in the drive's hands before the
    /// commit returns. WHAT IT DOES NOT: it does not flush the drive's own
    /// write cache, so a sudden loss of power to a drive that acknowledged a
    /// write it had not yet persisted can lose recently acknowledged
    /// commits. Nothing is left half-written either way -- a torn or missing
    /// tail is truncated at the last complete commit frame on the next open
    /// -- but that tail can be shorter than what was acknowledged.
    ///
    /// ASKING FOR THE STRONGER BARRIER: name it.
    ///
    /// ```no_run
    /// use sekejap::{Db, Config, SyncMode};
    /// let db = Db::open_with("/tmp/example", Config { sync: SyncMode::Full, ..Db::config() })?;
    /// # Ok::<(), sekejap::Error>(())
    /// ```
    ///
    /// [`SyncMode::Full`] issues the strongest barrier the platform has --
    /// on macOS `fcntl(F_FULLFSYNC)`, which flushes the drive's own write
    /// cache, and elsewhere `fsync` -- so an acknowledged commit survives a
    /// power cut as well. It costs 11.9 ms against 1.45 ms for `Normal` on
    /// the volume the write-path measurements were taken on, which at bulk
    /// sizes is over 90% of a write's wall time. [`SyncMode::Off`] issues no
    /// barrier and is for throwaway and rebuildable data only.
    ///
    /// All three are honoured end to end, at the kernel store
    /// (`core/kernel/src/store.rs`) and at every publication point of the
    /// page-WAL collection backend (`core/engine/src/store/pagewal/mod.rs`).
    /// Which primitive actually ran is readable per handle:
    /// `Database::io_counters()` reports `sync_full_calls` and
    /// `sync_data_calls` separately.
    pub fn config() -> Config {
        Config {
            sync: SyncMode::Normal,
            ..Config::default()
        }
    }

    /// [`Db::open`] under the caller's store configuration.
    pub fn open_with(path: impl AsRef<Path>, config: Config) -> Result<Self> {
        let path = path.as_ref().to_owned();
        let db = open_or_create(&path, config)?;
        Ok(Self {
            backing: Backing::Single(Mutex::new(db)),
            path,
            plans: Mutex::new(PlanCache::new()),
            generation: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Open in SERVICE mode: one writer, parallel readers on a published
    /// snapshot, the change feed, the statement timeout and the cancel of
    /// `docs/dist/OPS_CONTRACT.md` §1-§5.
    ///
    /// The publish interval starts at zero, so a commit is visible to the
    /// next reader: that costs one snapshot mint per commit, and
    /// [`ServiceDatabase::set_publish_interval`] through [`Db::service`]
    /// trades it back for staleness.
    pub fn open_service(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_service_with(path, Self::config())
    }

    /// [`Db::open_service`] under the caller's store configuration.
    pub fn open_service_with(path: impl AsRef<Path>, config: Config) -> Result<Self> {
        let path = path.as_ref().to_owned();
        // `ServiceDatabase::open` opens; it does not create. A first open of
        // an empty directory is a create, then a close, then the service.
        drop(open_or_create(&path, config)?);
        let service = ServiceDatabase::open(&path, config)?;
        service.set_publish_interval(std::time::Duration::ZERO);
        Ok(Self {
            backing: Backing::Service(service),
            path,
            plans: Mutex::new(PlanCache::new()),
            generation: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Which of the two modes this handle is.
    pub fn mode(&self) -> Mode {
        match &self.backing {
            Backing::Single(_) => Mode::Single,
            Backing::Service(_) => Mode::Service,
        }
    }

    /// The directory this handle owns.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The service handle, for the change feed, the interrupt and the
    /// statement timeout. `None` in single mode, which has none of them.
    pub fn service(&self) -> Option<&ServiceDatabase> {
        match &self.backing {
            Backing::Service(s) => Some(s),
            Backing::Single(_) => None,
        }
    }

    /// Close the handle. Uncommitted work is discarded, never committed: a
    /// close is not a commit.
    pub fn close(self) -> Result<()> {
        match self.backing {
            Backing::Single(m) => {
                let mut db = m.into_inner().unwrap_or_else(|e| e.into_inner());
                let _ = db.rollback();
                Ok(())
            }
            Backing::Service(s) => Ok(s.close()?),
        }
    }

    // ── the two access paths every call below goes through ───────────────

    /// One read, on a handle that cannot write.
    pub(crate) fn read<T>(&self, body: impl FnOnce(&Database) -> Result<T>) -> Result<T> {
        match &self.backing {
            Backing::Single(m) => {
                let guard = m.lock().unwrap_or_else(|e| e.into_inner());
                body(&guard)
            }
            Backing::Service(s) => {
                let snapshot: std::sync::Arc<Snapshot> = s.reader();
                snapshot.with(|db| body(db))
            }
        }
    }

    /// One read that needs `&mut Database` because `SqlDatabase::sql` does.
    /// In service mode the handle is a read-only snapshot, so a write
    /// through it refuses with `Error::ReadOnly` from the engine itself.
    fn read_mut<T>(&self, body: impl FnOnce(&mut Database) -> Result<T>) -> Result<T> {
        match &self.backing {
            Backing::Single(m) => {
                let mut guard = m.lock().unwrap_or_else(|e| e.into_inner());
                let out = body(&mut guard);
                if out.is_err() {
                    let _ = guard.rollback();
                }
                out
            }
            Backing::Service(s) => {
                let snapshot: std::sync::Arc<Snapshot> = s.reader();
                snapshot.with(|db| body(db))
            }
        }
    }

    /// One write, committed before it returns: durability per call
    /// (`docs/dist/RUST_API.md` §6). A failure rolls the call back whole.
    ///
    /// The body holds the ENGINE handle, which in service mode is the
    /// unrecorded one: what it writes is durable but invisible to the §5
    /// change feed. It is the path for the CATALOG calls, which the feed
    /// does not model. Every call that moves ROWS or EDGES goes through
    /// [`Db::in_transaction`] instead.
    pub(crate) fn write<T>(&self, body: impl FnOnce(&mut Database) -> Result<T>) -> Result<T> {
        self.in_transaction(|tx| body(tx.database()))
    }

    /// One transaction, committed before it returns and rolled back whole on
    /// a failure. The body holds the [`Tx`], whose writes are RECORDED: in
    /// service mode they reach the change feed, the statement timeout and
    /// the cancel of `docs/dist/OPS_CONTRACT.md` §3-§5.
    pub(crate) fn in_transaction<T>(&self, body: impl FnOnce(&mut Tx<'_>) -> Result<T>) -> Result<T> {
        let mut tx = self.transaction()?;
        match body(&mut tx) {
            Ok(value) => {
                tx.commit()?;
                Ok(value)
            }
            Err(e) => {
                let _ = tx.rollback();
                Err(e)
            }
        }
    }

    // ── §2 documents ─────────────────────────────────────────────────────

    /// Declare a collection. `false` if it was already there.
    ///
    /// The fields are the DECLARED columns: their `Kind` is what the row
    /// codec and every index key encode. A field a document carries but the
    /// declaration does not name is stored in the row's extras map, so a
    /// declaration is a floor and not a fence.
    pub fn create_collection(&self, name: &str, fields: &[(&str, Kind)]) -> Result<bool> {
        let fields: Vec<(String, Kind)> = fields
            .iter()
            .map(|(n, k)| ((*n).to_owned(), k.clone()))
            .collect();
        let out = self.write(|db| {
            if db.collection(name)?.is_some() {
                return Ok(false);
            }
            db.create_collection(name, fields, CollectionOptions::default())?;
            Ok(true)
        });
        self.catalog_changed();
        out
    }

    /// Remove a collection, its rows, its indexes and its descriptor.
    /// `false` if there was no such collection.
    pub fn drop_collection(&self, name: &str) -> Result<bool> {
        let out = self.write(|db| {
            let Some(id) = db.collection(name)? else {
                return Ok(false);
            };
            db.begin_drop_collection_mode(id, DropMode::Cascade)?;
            while !db.drop_collection_step(id, DROP_BATCH)?.done {}
            Ok(true)
        });
        self.catalog_changed();
        out
    }

    /// Write one document, committed.
    pub fn put<'a>(&self, addr: impl Into<Addr<'a>>, document: &Value) -> Result<EntityId> {
        let addr = addr.into();
        let doc = strip_key(document, addr.key)?;
        self.in_transaction(|tx| tx.put(addr, &doc))
    }

    /// Write many documents into one collection under ONE commit.
    pub fn put_many(
        &self,
        collection: &str,
        rows: impl IntoIterator<Item = (String, Value)>,
    ) -> Result<usize> {
        let rows: Vec<(String, Value)> = rows.into_iter().collect();
        self.in_transaction(|tx| tx.put_many(collection, rows))
    }

    /// Read one document, with `_key` set. `None` is a miss, never an error.
    pub fn get<'a>(&self, addr: impl Into<Addr<'a>>) -> Result<Option<Value>> {
        let addr = addr.into();
        self.read(|db| {
            let Some(id) = db.collection(addr.collection)? else {
                return Ok(None);
            };
            Ok(db
                .get(id, addr.key)?
                .map(|e| with_key(e.document, &e.key)))
        })
    }

    /// Whether the row is there.
    pub fn exists<'a>(&self, addr: impl Into<Addr<'a>>) -> Result<bool> {
        let addr = addr.into();
        self.read(|db| {
            let Some(id) = db.collection(addr.collection)? else {
                return Ok(false);
            };
            Ok(db.get(id, addr.key)?.is_some())
        })
    }

    /// Delete one row and every edge that touches it. `false` if it was not
    /// there.
    pub fn delete<'a>(&self, addr: impl Into<Addr<'a>>) -> Result<bool> {
        let addr = addr.into();
        self.in_transaction(|tx| tx.delete(addr))
    }

    /// Walk a collection in stable id order, one page of rows at a time.
    pub fn scan<'a>(&'a self, collection: &str) -> Result<Scan<'a>> {
        let id = self.read(|db| collection_id(db, collection))?;
        Ok(Scan::new(self, collection.to_owned(), id, SCAN_PAGE))
    }

    // ── §3 SQL ───────────────────────────────────────────────────────────

    /// The catalog generation every plan-cache key carries.
    fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Acquire)
    }

    /// True when a statement can change the CATALOG, which is what a
    /// cached plan is compiled against: only `CREATE`, `DROP` and `ALTER`
    /// can, in this grammar. An INSERT, UPDATE or DELETE changes rows, and
    /// a plan does not hold rows -- the one compiled form that holds a set
    /// of them, a semi-join, is never rebound and is compiled again instead.
    fn changes_the_catalog(sql: &str) -> bool {
        let first = sql
            .trim_start()
            .split(|c: char| c.is_whitespace())
            .next()
            .unwrap_or_default();
        first.eq_ignore_ascii_case("create")
            || first.eq_ignore_ascii_case("drop")
            || first.eq_ignore_ascii_case("alter")
    }

    /// Drop every cached plan, for a caller that changed the catalog through
    /// a handle this crate does not see -- `Tx::database`, or the layers
    /// re-exported at the crate root.
    pub fn invalidate_plans(&self) {
        self.catalog_changed();
    }

    /// Invalidate every cached plan by moving the generation on. Called by
    /// each catalog change, because a plan names index ids and a layout.
    pub(crate) fn catalog_changed(&self) {
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.plans
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// What the bounded prepared-plan cache has done, and the ceilings it
    /// was opened with. `docs/lang/QL_CONTRACT.md` §2.
    pub fn cache_stats(&self) -> CacheStats {
        self.plans
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stats()
    }

    /// Prepare one statement by hand: parsed here, compiled on its first
    /// bind, rebound after. See [`Statement`].
    pub fn prepare(&self, sql: &str) -> Result<Statement<'_>> {
        Statement::new(self, sql)
    }

    /// Run `body` against the compiled plan for `sql`, taking it from the
    /// plan cache when it is there and putting it back when it succeeds.
    ///
    /// A cache HIT is a rebind: the plan's typed slots are refilled from
    /// `params` and nothing is parsed or compiled. A MISS compiles once. A
    /// plan whose execution FAILED is not returned to the cache -- a rebind
    /// that refused halfway has written some slots and not others, and a
    /// half-bound plan is not a plan.
    fn with_cached_plan<T>(
        &self,
        sql: &str,
        params: &[Param],
        body: impl FnOnce(&Database, &PreparedSql) -> Result<T>,
    ) -> Result<T> {
        let generation = self.generation();
        let taken = {
            let mut cache = self.plans.lock().unwrap_or_else(|e| e.into_inner());
            match cache.take(sql, generation) {
                Some(prepared) => Some(prepared),
                None => {
                    cache.missed(sql);
                    None
                }
            }
        };
        let (out, keep) = self.read(|db| {
            let prepared = match taken {
                Some(mut prepared) => {
                    prepared.bind(db, params)?;
                    prepared
                }
                None => prepare_sql(db, sql, params)?,
            };
            // Only a reading statement is cached: a write compiles and runs
            // under the writer's borrow, and this path holds a read borrow.
            let cacheable = prepared.is_select() || prepared.is_aggregate();
            let out = body(db, &prepared)?;
            Ok((out, if cacheable { Some(prepared) } else { None }))
        })?;
        if let Some(prepared) = keep {
            self.plans
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .give(sql, generation, prepared);
        }
        Ok(out)
    }

    /// Run one writing statement and commit. Returns the rows it moved; a
    /// statement that only raises a notice returns zero.
    pub fn execute(&self, sql: &str, params: &[Value]) -> Result<u64> {
        self.in_transaction(|tx| tx.execute(sql, params))
    }

    /// Run one row-returning statement and assemble its answer.
    ///
    /// This is the plan cache's own door: the statement text is looked up
    /// there first, and a hit REBINDS the compiled plan rather than parsing
    /// and compiling it again. `Db::cache_stats` reports what that is doing.
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<Rows> {
        let params = params_of(params);
        match self.with_cached_plan(sql, &params, |db, prepared| {
            if !(prepared.is_select() || prepared.is_aggregate()) {
                return Ok(None);
            }
            Ok(Some(expect_rows(prepared.run(db)?, sql)?))
        })? {
            Some(rows) => Ok(rows),
            // Not a row-returning statement: the old path runs it and
            // refuses with the message `docs/dist/RUST_API.md` §3 states.
            None => self.read_mut(|db| {
                let result = db.sql(sql, &params)?;
                expect_rows(result, sql)
            }),
        }
    }

    /// Page a row-returning statement and hand each row to `body`, holding
    /// one page at a time. Returns the rows handed over.
    ///
    /// A callback and not an iterator because a compiled SELECT owns what
    /// its request borrows, so the cursor cannot outlive the call.
    pub fn stream(
        &self,
        sql: &str,
        params: &[Value],
        page_rows: usize,
        body: &mut dyn FnMut(&crate::rows::Row) -> Result<()>,
    ) -> Result<u64> {
        let params = params_of(params);
        let page_rows = page_rows.max(1);
        self.with_cached_plan(sql, &params, |db, prepared| {
            let columns = std::sync::Arc::new(prepared.columns().to_vec());
            let mut seen = 0u64;
            let mut stopped: Option<Error> = None;
            prepared.for_each_row(db, page_rows, &mut |row| {
                if stopped.is_some() {
                    return Ok(());
                }
                let row = crate::rows::Row {
                    columns: std::sync::Arc::clone(&columns),
                    id: row.id,
                    values: row.values.clone(),
                };
                match body(&row) {
                    Ok(()) => {
                        seen += 1;
                        Ok(())
                    }
                    Err(e) => {
                        stopped = Some(e);
                        Ok(())
                    }
                }
            })?;
            match stopped {
                Some(e) => Err(e),
                None => Ok(seen),
            }
        })
    }

    /// The plan the engine would build for one statement.
    pub fn explain(&self, sql: &str, params: &[Value]) -> Result<String> {
        let params = params_of(params);
        self.read(|db| Ok(sekejap_lang::explain_sql(db, sql, &params)?))
    }

    // ── §4 edges ─────────────────────────────────────────────────────────

    /// Link two rows with a typed edge in the base graph context.
    pub fn link<'a, 'b>(
        &self,
        from: impl Into<Addr<'a>>,
        edge_type: &str,
        to: impl Into<Addr<'b>>,
    ) -> Result<()> {
        self.link_with(from, edge_type, to, &Value::Object(Map::new()))
    }

    /// [`Db::link`] carrying a properties object.
    pub fn link_with<'a, 'b>(
        &self,
        from: impl Into<Addr<'a>>,
        edge_type: &str,
        to: impl Into<Addr<'b>>,
        properties: &Value,
    ) -> Result<()> {
        let (from, to) = (from.into(), to.into());
        self.in_transaction(|tx| tx.link_with(from, edge_type, to, properties))
    }

    /// Remove one edge. `false` if there was no such edge.
    pub fn unlink<'a, 'b>(
        &self,
        from: impl Into<Addr<'a>>,
        edge_type: &str,
        to: impl Into<Addr<'b>>,
    ) -> Result<bool> {
        let (from, to) = (from.into(), to.into());
        self.in_transaction(|tx| tx.unlink(from, edge_type, to))
    }

    /// The rows one hop away, in one direction, under a complete-or-error
    /// bound of at most [`MAX_NEIGHBOURS`] edges.
    pub fn neighbours<'a>(
        &self,
        of: impl Into<Addr<'a>>,
        edge_type: Option<&str>,
        direction: Direction,
        limit: usize,
    ) -> Result<Vec<Document>> {
        let of = of.into();
        if limit > MAX_NEIGHBOURS {
            return Err(Error::refused(
                format!("Db::neighbours with limit {limit}"),
                format!(
                    "a neighbour answer is complete or refused, and the bound is {MAX_NEIGHBOURS} edges \
                     (docs/core/GRAPH_CONTRACT.md §4.1); a wider walk is GRAPH_TABLE"
                ),
            ));
        }
        self.read(|db| {
            let entity = entity_id(db, of)?;
            // No edge in the store is no neighbour, and a database that has
            // never linked carries no graph header to ask.
            if db.scan_count_edges()? == 0 {
                return Ok(Vec::new());
            }
            let edge_type = match edge_type {
                Some(name) => match db.edge_type(name)? {
                    Some(id) => Some(id),
                    // A type nothing was ever linked with has no neighbours.
                    None => return Ok(Vec::new()),
                },
                None => None,
            };
            let ids = db.neighbor_ids(NeighborRequest {
                entity,
                direction,
                context: GraphContextId::BASE,
                edge_type,
                limit,
            })?;
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                let Some(entity) = db.get_by_id(id)? else {
                    continue;
                };
                let name = db.collection_info(id.collection)?.name;
                out.push(Document {
                    collection: name,
                    key: entity.key.clone(),
                    id,
                    fields: with_key(entity.document, &entity.key),
                });
            }
            Ok(out)
        })
    }

    // ── §5 catalog ───────────────────────────────────────────────────────

    /// Every collection name in the catalog, in key order.
    pub fn collections(&self) -> Result<Vec<String>> {
        self.read(|db| Ok(db.list_collections()?))
    }

    /// The declared shape of one collection. `None` if there is no such
    /// collection.
    pub fn describe(&self, collection: &str) -> Result<Option<Collection>> {
        self.read(|db| {
            let Some(id) = db.collection(collection)? else {
                return Ok(None);
            };
            let info = db.collection_info(id)?;
            let mut fields = vec![Field {
                name: KEY.to_owned(),
                kind: Kind::Text,
                declared: Some("TEXT".to_owned()),
                primary_key: true,
            }];
            for (name, kind) in &info.layout.fields {
                fields.push(Field {
                    name: name.clone(),
                    kind: kind.clone(),
                    declared: info
                        .declared
                        .iter()
                        .find(|(f, _)| f == name)
                        .map(|(_, d)| d.clone()),
                    primary_key: false,
                });
            }
            let indexes = db
                .list_indexes(id)?
                .into_iter()
                .map(|i| Index {
                    name: i.name,
                    field: i.field,
                    family: i.family,
                    unique: i.unique,
                    ready: matches!(
                        i.state,
                        sekejap_core::collections::IndexState::Ready
                    ),
                })
                .collect();
            Ok(Some(Collection {
                name: info.name,
                fields,
                indexes,
                timestamps: info.timestamps,
                rows: db.row_count(id)?,
            }))
        })
    }

    /// The rows of one collection, from the LIVE ROW COUNT record when the
    /// database keeps one and from the walk when it does not.
    ///
    /// The record is maintained by the write path inside the same transaction
    /// as the rows, so reading it is one `get` and the answer is exact. A
    /// database written before the record existed, or a collection a
    /// `Database::backfill_row_counts` has not reached, has none: then this
    /// is [`Db::scan_count_rows`], with the walk that name promises.
    /// [`Db::describe`]'s `rows` field is the one that tells the two apart
    /// without paying for either.
    pub fn count_rows(&self, collection: &str) -> Result<u64> {
        self.read(|db| {
            let Some(id) = db.collection(collection)? else {
                return Err(Error::UnknownCollection(collection.to_owned()));
            };
            match db.row_count(id)? {
                Some(rows) => Ok(rows),
                None => count_rows(db, id),
            }
        })
    }

    /// Count the rows of one collection BY WALKING them, whether or not the
    /// database keeps a live record. The explicit walk; [`Db::count_rows`] is
    /// the one that reads the record when there is one.
    pub fn scan_count_rows(&self, collection: &str) -> Result<u64> {
        self.read(|db| {
            let Some(id) = db.collection(collection)? else {
                return Err(Error::UnknownCollection(collection.to_owned()));
            };
            count_rows(db, id)
        })
    }

    /// Count every row of every collection BY WALKING them all.
    pub fn scan_count_all_rows(&self) -> Result<u64> {
        self.read(|db| {
            let mut total = 0u64;
            for name in db.list_collections()? {
                if let Some(id) = db.collection(&name)? {
                    total += count_rows(db, id)?;
                }
            }
            Ok(total)
        })
    }

    /// Count every edge BY WALKING the primary edge keyspace.
    pub fn scan_count_edges(&self) -> Result<u64> {
        self.read(|db| Ok(db.scan_count_edges()?))
    }

    // ── §6 transactions ──────────────────────────────────────────────────

    /// Take the writer for many writes under one barrier. A `Tx` dropped
    /// without [`Tx::commit`] ROLLS BACK.
    pub fn transaction(&self) -> Result<Tx<'_>> {
        Ok(match &self.backing {
            Backing::Single(m) => Tx {
                inner: TxInner::Single(m.lock().unwrap_or_else(|e| e.into_inner())),
                done: false,
                db: self,
            },
            Backing::Service(s) => Tx {
                inner: TxInner::Service(s.writer()),
                done: false,
                db: self,
            },
        })
    }

    // ── §7 maintenance ───────────────────────────────────────────────────

    /// Fold the committed WAL into the data file. `Ok(false)` means a live
    /// reader holds a slot and the fold is DEFERRED, not failed -- which in
    /// service mode is every call, because the published read view holds one
    /// for its whole life (`docs/dist/OPS_CONTRACT.md` §1).
    pub fn checkpoint(&self) -> Result<bool> {
        match &self.backing {
            Backing::Single(m) => {
                let mut db = m.lock().unwrap_or_else(|e| e.into_inner());
                db.commit()?;
                Ok(db.checkpoint()?)
            }
            Backing::Service(_) => Ok(false),
        }
    }

    /// Make the newest commit visible to readers now. In single mode there
    /// is no published view to swap and every commit is already visible to
    /// this handle, so there is nothing to do.
    pub fn publish(&self) -> Result<()> {
        match &self.backing {
            Backing::Single(_) => Ok(()),
            Backing::Service(s) => Ok(s.publish_now()?),
        }
    }

    /// The bytes on disk: the data file and the write-ahead log.
    pub fn storage(&self) -> Result<Storage> {
        self.read(|db| {
            let (data_bytes, wal_bytes) = db.storage_bytes()?;
            Ok(Storage {
                data_bytes,
                wal_bytes,
            })
        })
    }
}

/// The writer, held across many writes. `docs/dist/RUST_API.md` §6.
pub struct Tx<'a> {
    inner: TxInner<'a>,
    done: bool,
    /// The handle this transaction was opened on, so a DDL statement run
    /// through [`Tx::execute`] invalidates the plan cache the same way
    /// `Db::execute` does.
    db: &'a Db,
}

enum TxInner<'a> {
    Single(MutexGuard<'a, Database>),
    Service(WriterGuard<'a>),
}

impl Tx<'_> {
    /// The engine handle, for a call this crate does not wrap.
    pub fn database(&mut self) -> &mut Database {
        match &mut self.inner {
            TxInner::Single(g) => g,
            TxInner::Service(g) => g.database(),
        }
    }

    /// Write one document. Not durable until [`Tx::commit`].
    pub fn put<'a>(&mut self, addr: impl Into<Addr<'a>>, document: &Value) -> Result<EntityId> {
        let addr = addr.into();
        let doc = strip_key(document, addr.key)?;
        let id = collection_id(self.database(), addr.collection)?;
        match &mut self.inner {
            TxInner::Single(g) => Ok(g.put(id, addr.key, &doc)?),
            // The RECORDED write path: in service mode a put is one entry in
            // the §5 change feed, delivered by the commit that made it
            // durable.
            TxInner::Service(g) => Ok(g.put(id, addr.key, &doc)?),
        }
    }

    /// Write many documents into one collection.
    pub fn put_many(
        &mut self,
        collection: &str,
        rows: impl IntoIterator<Item = (String, Value)>,
    ) -> Result<usize> {
        let id = collection_id(self.database(), collection)?;
        let mut written = 0;
        for (key, doc) in rows {
            let doc = strip_key(&doc, &key)?;
            match &mut self.inner {
                TxInner::Single(g) => {
                    g.put(id, &key, &doc)?;
                }
                TxInner::Service(g) => {
                    g.put(id, &key, &doc)?;
                }
            }
            written += 1;
        }
        Ok(written)
    }

    /// Delete one row.
    pub fn delete<'a>(&mut self, addr: impl Into<Addr<'a>>) -> Result<bool> {
        let addr = addr.into();
        let Some(id) = self.database().collection(addr.collection)? else {
            return Ok(false);
        };
        match &mut self.inner {
            TxInner::Single(g) => Ok(g.delete(id, addr.key)?),
            TxInner::Service(g) => Ok(g.delete(id, addr.key)?),
        }
    }

    /// Link two rows with a typed edge in the base graph context.
    pub fn link<'a, 'b>(
        &mut self,
        from: impl Into<Addr<'a>>,
        edge_type: &str,
        to: impl Into<Addr<'b>>,
    ) -> Result<()> {
        self.link_with(from, edge_type, to, &Value::Object(Map::new()))
    }

    /// [`Tx::link`] carrying a properties object.
    pub fn link_with<'a, 'b>(
        &mut self,
        from: impl Into<Addr<'a>>,
        edge_type: &str,
        to: impl Into<Addr<'b>>,
        properties: &Value,
    ) -> Result<()> {
        let (from, to) = (from.into(), to.into());
        let db = self.database();
        let source = entity_id(db, from)?;
        let destination = entity_id(db, to)?;
        // The graph keyspace is an additive feature bit, set on the first
        // edge and idempotent afterwards: a database that never links carries
        // no graph header.
        db.enable_graph()?;
        // The atomic takes the edge type by NAME and interns it, so the
        // identity the feed records exists only once the link has been made.
        let key = db.link(source, edge_type, destination, "", properties)?;
        if let TxInner::Service(g) = &mut self.inner {
            g.note_edge_type(key.edge_type);
        }
        Ok(())
    }

    /// Remove one edge.
    pub fn unlink<'a, 'b>(
        &mut self,
        from: impl Into<Addr<'a>>,
        edge_type: &str,
        to: impl Into<Addr<'b>>,
    ) -> Result<bool> {
        let (from, to) = (from.into(), to.into());
        let db = self.database();
        if db.scan_count_edges()? == 0 {
            return Ok(false);
        }
        let source = entity_id(db, from)?;
        let destination = entity_id(db, to)?;
        let type_id = db.edge_type(edge_type)?;
        let removed = db.unlink(source, edge_type, destination, "")?;
        if removed {
            if let (TxInner::Service(g), Some(type_id)) = (&mut self.inner, type_id) {
                g.note_edge_type(type_id);
            }
        }
        Ok(removed)
    }

    /// Run one writing statement. Not durable until [`Tx::commit`].
    ///
    /// In service mode the statement goes through the service's own writer,
    /// which is what puts it under the §3 statement timeout and the §4
    /// cancel and what counts it in the feed's `unnamed_writes`. That is
    /// also where `BEGIN`, `COMMIT` and `ROLLBACK` as SQL are refused
    /// (`docs/dist/RUST_API.md` §6): the service owns the barrier.
    pub fn execute(&mut self, sql: &str, params: &[Value]) -> Result<u64> {
        let params = params_of(params);
        let result: SqlResult = match &mut self.inner {
            TxInner::Single(g) => g.sql(sql, &params)?,
            TxInner::Service(g) => g.sql(sql, &params)?,
        };
        if Db::changes_the_catalog(sql) {
            self.db.catalog_changed();
        }
        expect_affected(result, sql)
    }

    /// Record that a statement run through [`Tx::database`] moved `rows`,
    /// for the change feed. Nothing to do in single mode, which has no feed.
    pub fn note_unnamed_write(&mut self, rows: u64) {
        if let TxInner::Service(g) = &mut self.inner {
            g.note_unnamed_write(rows);
        }
    }

    /// Make everything written through this transaction durable.
    pub fn commit(mut self) -> Result<()> {
        self.done = true;
        match &mut self.inner {
            TxInner::Single(g) => Ok(g.commit()?),
            TxInner::Service(g) => Ok(g.commit()?),
        }
    }

    /// Discard everything written through this transaction.
    pub fn rollback(mut self) -> Result<()> {
        self.done = true;
        match &mut self.inner {
            TxInner::Single(g) => Ok(g.rollback()?),
            TxInner::Service(g) => Ok(g.rollback()?),
        }
    }
}

impl Drop for Tx<'_> {
    /// A transaction that is dropped without a decision is ROLLED BACK. The
    /// alternative -- committing on drop -- would make an early `?` durable.
    fn drop(&mut self) {
        if self.done {
            return;
        }
        match &mut self.inner {
            TxInner::Single(g) => {
                let _ = g.rollback();
            }
            TxInner::Service(g) => {
                let _ = g.rollback();
            }
        }
    }
}

// ── the shared pieces ────────────────────────────────────────────────────

/// Open the database in `path`, or create it there.
///
/// `data` is what says a database is there: the page WAL writes it first and
/// refuses to initialize over a byte of it. A directory the caller made and
/// left empty is created into; a directory with a `data` file is opened.
fn open_or_create(path: &Path, config: Config) -> Result<Database> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
    }
    if path.join("data").exists() {
        Ok(Database::open(path, config)?)
    } else {
        Ok(Database::create(path, config)?)
    }
}

pub(crate) fn collection_id(db: &Database, name: &str) -> Result<CollectionId> {
    db.collection(name)?
        .ok_or_else(|| Error::UnknownCollection(name.to_owned()))
}

fn entity_id(db: &Database, addr: Addr<'_>) -> Result<EntityId> {
    let id = collection_id(db, addr.collection)?;
    db.get(id, addr.key)?
        .map(|e| e.id)
        .ok_or_else(|| Error::UnknownRow {
            collection: addr.collection.to_owned(),
            key: addr.key.to_owned(),
        })
}

/// A document as the engine takes it: an object, without `_key`, which the
/// address already carries. A `_key` that disagrees with the address is
/// refused rather than silently preferred one way or the other.
fn strip_key(document: &Value, key: &str) -> Result<Value> {
    let Some(object) = document.as_object() else {
        return Err(Error::refused(
            "a document that is not a JSON object",
            "a row is a set of named fields; an array or a scalar names none",
        ));
    };
    match object.get(KEY) {
        None => Ok(document.clone()),
        Some(Value::String(written)) if written == key => {
            let mut out = object.clone();
            out.remove(KEY);
            Ok(Value::Object(out))
        }
        Some(other) => Err(Error::refused(
            format!("a document whose `{KEY}` is {other}"),
            format!("the address says `{key}`, and a row has one external key"),
        )),
    }
}

/// A document as a caller reads it: the row's fields, with `_key` set.
pub(crate) fn with_key(document: Value, key: &str) -> Value {
    match document {
        Value::Object(mut map) => {
            map.insert(KEY.to_owned(), Value::String(key.to_owned()));
            Value::Object(map)
        }
        other => other,
    }
}

fn count_rows(db: &Database, id: CollectionId) -> Result<u64> {
    let mut seen = 0u64;
    let mut after: Option<EntityId> = None;
    loop {
        let mut last = None;
        let mut page = 0u64;
        for row in db.scan(id, after)?.take(SCAN_PAGE) {
            let row = row?;
            last = Some(row.id);
            page += 1;
        }
        seen += page;
        if page == 0 {
            return Ok(seen);
        }
        after = last;
    }
}

#[cfg(test)]
mod default_barrier_tests {
    use super::*;

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// The default `Db::open` gives is `SyncMode::Normal` -- the barrier
    /// SQLite issues with `fullfsync` off and PostgreSQL with a plain
    /// `fsync`, so an unconfigured sekejap database promises what an
    /// unconfigured one of those two promises. It was `Full` before, which
    /// made every default-configured comparison against them a comparison
    /// between different durability guarantees.
    ///
    /// Asserted on the barrier the store ACTUALLY issued, not only on the
    /// field: `Db::config()` naming `Normal` proves nothing if the page-WAL
    /// goes on hard-coding `sync_full`, which is what it did.
    #[test]
    fn the_default_a_caller_gets_from_db_open_is_normal() {
        assert_eq!(Db::config().sync, SyncMode::Normal);
        let d = temp();
        let db = Db::open(d.path().join("db")).unwrap();
        db.create_collection("c", &[("name", crate::FieldKind::Text)]).unwrap();
        db.put(("c", "k0"), &serde_json::json!({"name": "a"})).unwrap();
        let c = db.write(|d| Ok(d.io_counters()?)).unwrap();
        assert_eq!(c.sync_full_calls, 0, "the default must not place the drive-cache barrier");
        assert!(c.sync_data_calls > 0, "the default must place a data barrier at every commit");
    }

    /// `Full` stays available and stays honoured: the whole point of moving
    /// the default is that the stronger barrier is a choice a caller can
    /// still make in one line.
    #[test]
    fn open_with_full_is_honoured() {
        let d = temp();
        let db = Db::open_with(
            d.path().join("db"),
            Config { sync: SyncMode::Full, ..Db::config() },
        )
        .unwrap();
        db.create_collection("c", &[("name", crate::FieldKind::Text)]).unwrap();
        db.put(("c", "k0"), &serde_json::json!({"name": "a"})).unwrap();
        let c = db.write(|d| Ok(d.io_counters()?)).unwrap();
        assert!(c.sync_full_calls > 0, "open_with(Full) must place drive-cache barriers");
        assert_eq!(c.sync_data_calls, 0, "Full must never fall back to the weaker barrier");
    }

    /// `Off` is reachable too, and is observably neither of the other two.
    #[test]
    fn open_with_off_places_no_barrier_at_all() {
        let d = temp();
        let db = Db::open_with(
            d.path().join("db"),
            Config { sync: SyncMode::Off, ..Db::config() },
        )
        .unwrap();
        db.create_collection("c", &[("name", crate::FieldKind::Text)]).unwrap();
        db.put(("c", "k0"), &serde_json::json!({"name": "a"})).unwrap();
        let c = db.write(|d| Ok(d.io_counters()?)).unwrap();
        assert_eq!((c.sync_full_calls, c.sync_data_calls), (0, 0));
        assert_eq!(db.get(("c", "k0")).unwrap().unwrap()["name"], serde_json::json!("a"));
    }
}
