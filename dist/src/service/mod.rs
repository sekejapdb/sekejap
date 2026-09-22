//! The embedded wrapper of `docs/dist/OPS_CONTRACT.md` §1-§5: one writer,
//! snapshot readers, a publish barrier with a stated staleness window, a
//! wall-clock statement timeout, a public cancel, and a change feed that
//! fires once per committed batch.
//!
//! `Database::open` is the embedded form that starts and stops with an app.
//! [`ServiceDatabase`] is the form a server, an agent or a gateway keeps
//! alive: it owns both halves of the single-writer rule -- the writer behind
//! a mutex, the published read view behind an `RwLock` -- so a slow write, a
//! bulk load or a checkpoint never stalls a query.
//!
//! ## What is here, section by section
//!
//! | § | Surface | Where |
//! |---|---|---|
//! | 1 | `open`, `writer`, `reader`, `close`, the single-writer refusal | this file |
//! | 2 | `publish_now`, `publish_interval`, the window | this file and [`snapshot`] |
//! | 3 | `set_statement_timeout`, the deadline on the budget | this file and `core/engine/src/query/mod.rs` |
//! | 4 | `interrupt_handle`, `cancel`, `clear_interrupt` | [`interrupt`] |
//! | 5 | `subscribe_changes`, `unsubscribe`, `ChangeEvent` | [`changes`] |
//!
//! ## The one change this needed in core
//!
//! Everything above except the deadline composes handles that already exist.
//! The deadline could not: `WorkMeter` (`core/engine/src/query/mod.rs`) is
//! where every driver's check point already is, and there was no way to give
//! it a clock. `QueryBudget` gained `deadline: Option<Instant>` and a
//! `with_deadline` builder, `WorkResource` gained a `Deadline` variant, and
//! `WorkMeter::check_cancelled` reads the clock once per meter and then once
//! per `DEADLINE_POLL_CHARGES` charges. Nothing else in core moved.
//!
//! ## SQL
//!
//! The service issues SQL through `sekejap_lang::SqlDatabase`, which is what
//! the layering says: `dist -> lang -> core`. A read that must be bounded in
//! wall clock or stoppable from another thread goes through
//! [`ServiceDatabase::scan`], which pages the compiled SELECT itself and
//! hands `next_page` both the deadline and the interrupt; `SqlDatabase::sql`
//! runs a statement to exhaustion under neither, and the guard's
//! [`WriterGuard::sql`] therefore carries them only as far as the statement's
//! COMPILE-time semi-join set, which is as far as `sql_with` reaches.

pub mod changes;
pub mod interrupt;
pub mod snapshot;

pub use changes::{
    ChangeEvent, ChangeKind, ChangedKey, Receiver, SubscriptionId, CHANGE_KEY_CAP,
    CHANGE_QUEUE_BOUND,
};
pub use interrupt::InterruptHandle;
pub use snapshot::{Snapshot, PUBLISH_INTERVAL_DEFAULT};

use changes::{PendingBatch, Subscribers};
use kernel::store::Config;
use sekejap_core::collections::{
    CollectionId, Database, EdgeKey, EdgeTypeId, EntityId, Error as CoreError, GraphContextId,
    QueryBudget, QueryError, QueryPage, QueryWork,
};
use sekejap_lang::{Param, SqlDatabase, SqlError, SqlResult};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

/// Why the service refused, or what it was handed by the layer beneath.
#[derive(Debug)]
pub enum ServiceError {
    /// The engine or the store refused.
    Core(CoreError),
    /// A bounded walk refused: a work budget, the §3 deadline
    /// (`WorkResource::Deadline`, with the elapsed microseconds), or the §4
    /// cancel.
    Query(QueryError),
    /// The SQL layer refused to compile or to run a statement.
    Sql(SqlError),
    /// The service itself refused, with the reason named. Never a silent
    /// emulation and never a wait: `docs/dist/OPS_CONTRACT.md` §1 says a
    /// service that blocks to honour a bound has broken L6 to satisfy L1.
    Refused(String),
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Core(e) => write!(f, "{e}"),
            Self::Query(e) => write!(f, "{e}"),
            Self::Sql(e) => write!(f, "{e}"),
            Self::Refused(reason) => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for ServiceError {}

impl From<CoreError> for ServiceError {
    fn from(value: CoreError) -> Self {
        Self::Core(value)
    }
}

impl From<QueryError> for ServiceError {
    fn from(value: QueryError) -> Self {
        Self::Query(value)
    }
}

impl From<SqlError> for ServiceError {
    fn from(value: SqlError) -> Self {
        Self::Sql(value)
    }
}

pub type Result<T> = std::result::Result<T, ServiceError>;

/// The reason a second writer on one live service is refused.
pub const SECOND_WRITER_REFUSAL: &str =
    "the page WAL is single-writer: this service's writer is held by another caller";

/// The reason a transaction-control statement is refused through the guard.
pub const TRANSACTION_SQL_REFUSAL: &str =
    "BEGIN, COMMIT and ROLLBACK are the service's own barrier: use WriterGuard::commit \
     or WriterGuard::rollback so the change feed fires exactly once, after durability";

/// What the publish half knows between publications.
struct PublishState {
    /// When the published view was last swapped.
    last: Instant,
    /// Set by a commit, cleared by a publication. e3's `dirty`
    /// (`src/service.rs:136-151`): a publish that has nothing to publish
    /// costs nothing.
    dirty: bool,
    /// The ordinal the next published view will carry.
    next_serial: u64,
}

/// One writer, snapshot readers, and the four operator surfaces §2-§5 name.
///
/// `Send + Sync` by construction and meant to be shared behind an `Arc`
/// rather than wrapped in the caller's own lock.
pub struct ServiceDatabase {
    path: PathBuf,
    config: Config,
    /// §1: the single writer. Every write serialises here, and no read ever
    /// takes it.
    writer: Mutex<Database>,
    /// §1: the published read view. A reader clones the `Arc` under a
    /// momentary read lock and then walks without touching this again.
    published: RwLock<Arc<Snapshot>>,
    publish: Mutex<PublishState>,
    /// §2, in microseconds.
    interval_micros: AtomicU64,
    /// §3, in microseconds. Zero means no wall-clock bound.
    timeout_micros: AtomicU64,
    /// §4.
    interrupt: InterruptHandle,
    /// §5.
    subscribers: Subscribers,
    /// Mints that failed. A failed mint leaves readers on the view they
    /// already had -- L3's shape at the API, the old photo dropped only once
    /// the new one is in hand -- and is counted here rather than retried into
    /// a gap.
    publish_failures: AtomicU64,
}

impl ServiceDatabase {
    /// §1. Open the single writer and mint the first published view.
    ///
    /// A second `open` on a live directory is refused by the page WAL's own
    /// file lock, which is what says the store is single-writer; this call
    /// adds no second rule and emulates nothing.
    pub fn open(path: impl AsRef<Path>, config: Config) -> Result<Self> {
        let path = path.as_ref().to_owned();
        let writer = Database::open(&path, config)?;
        let (snapshot, _) = mint(&path, config, 1)?;
        Ok(Self {
            path,
            config,
            writer: Mutex::new(writer),
            published: RwLock::new(Arc::new(snapshot)),
            publish: Mutex::new(PublishState {
                last: Instant::now(),
                dirty: false,
                next_serial: 2,
            }),
            interval_micros: AtomicU64::new(micros(PUBLISH_INTERVAL_DEFAULT)),
            timeout_micros: AtomicU64::new(0),
            interrupt: InterruptHandle::new(),
            subscribers: Subscribers::new(),
            publish_failures: AtomicU64::new(0),
        })
    }

    /// The directory this service owns.
    pub fn path(&self) -> &Path {
        &self.path
    }

    // ── §1 the writer ────────────────────────────────────────────────────

    /// §1. Take the writer, waiting for it.
    ///
    /// Waiting is correct here and refusing is correct in
    /// [`ServiceDatabase::try_writer`]: a second writer in THIS process is a
    /// queue, not a contradiction, because there is still exactly one writer
    /// at a time. A second writer PROCESS on one directory is the contract's
    /// T3 and is refused by the file lock at `open`, not here.
    pub fn writer(&self) -> WriterGuard<'_> {
        WriterGuard {
            service: self,
            db: self.writer.lock().unwrap_or_else(|e| e.into_inner()),
            batch: PendingBatch::default(),
        }
    }

    /// §1. Take the writer, or refuse naming the reason.
    ///
    /// The refusal a caller that must not wait asks for.
    pub fn try_writer(&self) -> Result<WriterGuard<'_>> {
        match self.writer.try_lock() {
            Ok(db) => Ok(WriterGuard {
                service: self,
                db,
                batch: PendingBatch::default(),
            }),
            Err(std::sync::TryLockError::Poisoned(e)) => Ok(WriterGuard {
                service: self,
                db: e.into_inner(),
                batch: PendingBatch::default(),
            }),
            Err(std::sync::TryLockError::WouldBlock) => {
                Err(ServiceError::Refused(SECOND_WRITER_REFUSAL.to_owned()))
            }
        }
    }

    // ── §1/§2 the readers ────────────────────────────────────────────────

    /// §1/§2. The published read view.
    ///
    /// Cloning an `Arc` under a momentary read lock, plus -- when the view is
    /// dirty and older than the publish interval -- one republish, which is
    /// the second half of the stated window. A background republisher would
    /// let the writer pay that mint every time; this build has no background
    /// thread, so the reader that finds a stale-past-the-interval view pays
    /// it instead. Whenever another commit follows within the interval the
    /// writer still pays it, in [`WriterGuard::commit`].
    pub fn reader(&self) -> Arc<Snapshot> {
        self.publish_if_due();
        Arc::clone(&self.published.read().unwrap_or_else(|e| e.into_inner()))
    }

    /// §1. A PRIVATE snapshot, not the published one: a fresh handle for one
    /// thread that wants to walk beside another.
    ///
    /// It spends one more of the persisted `readers` slots and is refused,
    /// never blocked, past that bound.
    pub fn open_reader(&self) -> Result<Snapshot> {
        let serial = {
            let mut state = self.publish.lock().unwrap_or_else(|e| e.into_inner());
            let serial = state.next_serial;
            state.next_serial += 1;
            serial
        };
        Ok(mint(&self.path, self.config, serial)?.0)
    }

    // ── §2 publish ───────────────────────────────────────────────────────

    /// §2. Mint a fresh snapshot and swap it in, now.
    ///
    /// The read-your-own-writes call. A failed mint leaves the previous view
    /// in place and is REPORTED, never silently retried into a gap.
    pub fn publish_now(&self) -> Result<()> {
        let mut state = self.publish.lock().unwrap_or_else(|e| e.into_inner());
        self.publish_locked(&mut state)
    }

    /// §2. The republish interval. A read is stale by at most this plus one
    /// snapshot open.
    pub fn publish_interval(&self) -> Duration {
        Duration::from_micros(self.interval_micros.load(Ordering::Relaxed))
    }

    /// §2. Set the republish interval. `Duration::ZERO` republishes after
    /// every commit, which is read-your-own-writes at the cost of one mint
    /// per commit.
    pub fn set_publish_interval(&self, interval: Duration) {
        self.interval_micros
            .store(micros(interval), Ordering::Relaxed);
    }

    /// Mints that failed. Counted rather than hidden: a service whose
    /// publication is failing serves an ever-older view, and this is how a
    /// caller sees that.
    pub fn publish_failures(&self) -> u64 {
        self.publish_failures.load(Ordering::Relaxed)
    }

    fn publish_if_due(&self) {
        let mut state = self.publish.lock().unwrap_or_else(|e| e.into_inner());
        if !state.dirty || state.last.elapsed() < self.publish_interval() {
            return;
        }
        let _ = self.publish_locked(&mut state);
    }

    fn publish_locked(&self, state: &mut PublishState) -> Result<()> {
        let serial = state.next_serial;
        match mint(&self.path, self.config, serial) {
            Ok((snapshot, _)) => {
                // The new photo is in hand before the old one is dropped.
                *self.published.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(snapshot);
                state.next_serial += 1;
                state.last = Instant::now();
                state.dirty = false;
                Ok(())
            }
            Err(error) => {
                self.publish_failures.fetch_add(1, Ordering::Relaxed);
                Err(ServiceError::Core(error))
            }
        }
    }

    // ── §3 statement timeout ─────────────────────────────────────────────

    /// §3. Bound every statement this service issues from now on in wall
    /// clock.
    ///
    /// In e3 this had to be applied to every snapshot the service had minted
    /// and republished so it took effect (`src/service.rs:158-166`). It does
    /// not here: the deadline rides the `QueryBudget` the service hands each
    /// statement, so it is read at statement entry and no handle holds it.
    /// That is one fewer republish on the path, and it is why a timeout can
    /// be changed between two statements on the same snapshot.
    ///
    /// The bound is IN ADDITION to the work budget, never instead of it: the
    /// work bound is what makes a cost reproducible.
    pub fn set_statement_timeout(&self, timeout: Duration) {
        self.timeout_micros.store(micros(timeout), Ordering::Relaxed);
    }

    /// §3. Remove the wall-clock bound.
    pub fn clear_statement_timeout(&self) {
        self.timeout_micros.store(0, Ordering::Relaxed);
    }

    /// §3. The standing wall-clock bound, if any.
    pub fn statement_timeout(&self) -> Option<Duration> {
        match self.timeout_micros.load(Ordering::Relaxed) {
            0 => None,
            micros => Some(Duration::from_micros(micros)),
        }
    }

    /// §3. `QueryBudget::unlimited()` carrying this service's deadline, if
    /// one is set: the budget every statement the service issues runs under
    /// unless the caller supplies its own.
    pub fn budget(&self) -> QueryBudget {
        self.bound(QueryBudget::unlimited())
    }

    /// §3. The caller's own work budget with this service's deadline on top
    /// of it. The work bounds are untouched.
    pub fn bound(&self, budget: QueryBudget) -> QueryBudget {
        match self.statement_timeout() {
            Some(timeout) => budget.with_deadline(Instant::now() + timeout),
            None => budget,
        }
    }

    // ── §4 interrupt ─────────────────────────────────────────────────────

    /// §4. A handle another thread holds while this one runs a statement.
    pub fn interrupt_handle(&self) -> InterruptHandle {
        self.interrupt.clone()
    }

    /// §4. Stop every statement this service has in flight, at its next check
    /// point. Sticky until [`ServiceDatabase::clear_interrupt`].
    pub fn cancel(&self) {
        self.interrupt.cancel();
    }

    /// §4. Clear a standing cancel. Returns whether one was standing.
    pub fn clear_interrupt(&self) -> bool {
        self.interrupt.clear()
    }

    // ── §5 change feed ───────────────────────────────────────────────────

    /// §5. Subscribe to the change feed.
    ///
    /// Each subscriber gets its own queue, bounded at
    /// [`CHANGE_QUEUE_BOUND`] events. A subscriber that stops draining loses
    /// events and is told how many in [`Receiver::lagged`]; the writer never
    /// waits for it.
    pub fn subscribe_changes(&self) -> Receiver {
        self.subscribers.subscribe()
    }

    /// §5. Drop a subscription. Returns whether it was live.
    pub fn unsubscribe(&self, id: SubscriptionId) -> bool {
        self.subscribers.unsubscribe(id)
    }

    /// §5. Events delivered so far: the ordinal the last DELIVERED batch
    /// carried. A commit made with no subscriber is not among them.
    pub fn events_delivered(&self) -> u64 {
        self.subscribers.delivered()
    }

    // ── reading ──────────────────────────────────────────────────────────

    /// §3 + §4 on one read: compile `text` against `snapshot` and page it,
    /// handing `next_page` this service's deadline and interrupt on every
    /// page.
    ///
    /// `work` accumulates what each COMPLETED page charged, so a refusal
    /// leaves behind the counters that say where the walk stopped. The
    /// refusing page's own partial charges are not in it: `next_page` refuses
    /// by returning an error instead of a page, which is the same rule that
    /// makes a refused page leave no state behind.
    ///
    /// Returns the rows handed to `body`, or the refusal.
    pub fn scan(
        &self,
        snapshot: &Snapshot,
        text: &str,
        params: &[Param],
        page_rows: usize,
        work: &mut QueryWork,
        body: &mut dyn FnMut(&QueryPage) -> Result<()>,
    ) -> Result<u64> {
        let budget = self.budget();
        let interrupt = self.interrupt.clone();
        let guard = snapshot.lock();
        let db: &Database = &guard;
        let prepared =
            sekejap_lang::prepare_sql_with(db, text, params, budget, &mut || {
                interrupt.is_cancelled()
            })?;
        let mut rows = 0u64;
        let mut stopped: Option<ServiceError> = None;
        // `with_query` is a callback because the request borrows what the
        // plan owns; its body may only return the SQL layer's error, so a
        // structured `QueryError` -- which is what names `Deadline` and its
        // elapsed microseconds -- is carried out past it rather than
        // flattened into prose.
        prepared.with_query(db, &mut |query| {
            loop {
                match query.next_page(page_rows, budget, || interrupt.is_cancelled()) {
                    Ok(page) => {
                        accumulate(work, &page.work);
                        rows += page.rows.len() as u64;
                        if let Err(error) = body(&page) {
                            stopped = Some(error);
                            break;
                        }
                        if page.done || page.rows.is_empty() {
                            break;
                        }
                    }
                    Err(error) => {
                        stopped = Some(ServiceError::Query(error));
                        break;
                    }
                }
            }
            Ok(())
        })?;
        drop(guard);
        match stopped {
            Some(error) => Err(error),
            None => Ok(rows),
        }
    }

    /// One whole statement against `snapshot`, through
    /// `sekejap_lang::SqlDatabase`.
    ///
    /// The snapshot handle is read-only, so a write refuses with
    /// `Error::ReadOnly` from the engine itself. The deadline and the
    /// interrupt reach the statement's COMPILE-time semi-join set and no
    /// further, because `sql_with` bounds compilation and runs the walk under
    /// `QueryBudget::unlimited()`. A read that must be bounded or stoppable
    /// while it WALKS goes through [`ServiceDatabase::scan`].
    pub fn query(&self, snapshot: &Snapshot, text: &str, params: &[Param]) -> Result<SqlResult> {
        let budget = self.budget();
        let interrupt = self.interrupt.clone();
        let mut guard = snapshot.lock();
        Ok(guard.sql_with(text, params, budget, &mut || interrupt.is_cancelled())?)
    }

    // ── §1 close ─────────────────────────────────────────────────────────

    /// §1. Close the service.
    ///
    /// The published view goes first, because it is what holds the reader
    /// slot and a slot still out defers the writer's checkpoint. Uncommitted
    /// work is discarded, never silently committed: a close is not a commit.
    pub fn close(self) -> Result<()> {
        {
            let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
            let _ = writer.rollback();
        }
        let Self {
            writer, published, ..
        } = self;
        drop(published);
        drop(writer);
        Ok(())
    }
}

/// §1. The writer, held.
///
/// Every mutation made through this guard is recorded for §5's event; a
/// mutation made through [`WriterGuard::database`] is not, and says so.
/// Dropping the guard without committing ROLLS BACK: an uncommitted batch
/// must not be inherited by whoever takes the writer next, and a rollback
/// fires no event.
pub struct WriterGuard<'a> {
    service: &'a ServiceDatabase,
    db: MutexGuard<'a, Database>,
    batch: PendingBatch,
}

impl WriterGuard<'_> {
    /// The writer handle, unrecorded.
    ///
    /// Anything written through this is durable like any other write but is
    /// INVISIBLE to §5's feed unless the caller names what it touched with
    /// [`WriterGuard::note_collection`]. It is here because the catalog calls
    /// -- `create_collection`, `create_index`, `drop_collection` -- are not
    /// key mutations and the feed does not model them.
    pub fn database(&mut self) -> &mut Database {
        &mut self.db
    }

    /// Record that `collection` moved, for a caller that wrote through
    /// [`WriterGuard::database`].
    pub fn note_collection(&mut self, collection: CollectionId) {
        if self.service.subscribers.listening() {
            self.batch.note_collection(collection);
        }
    }

    /// Record that an EDGE of `edge_type` was linked or unlinked, for a
    /// caller that wrote through [`WriterGuard::database`]. The companion of
    /// [`WriterGuard::note_collection`], and there for the same reason: the
    /// graph atomics take an edge type NAME and intern it, so a caller that
    /// links through the engine handle has the identity only after the call.
    pub fn note_edge_type(&mut self, edge_type: EdgeTypeId) {
        if self.service.subscribers.listening() {
            self.batch.note_edge_type(edge_type);
        }
    }

    /// Record that a statement run through [`WriterGuard::database`] moved
    /// `rows`, for a caller that compiles its own plan and therefore cannot
    /// go through [`WriterGuard::sql`]. Lands in
    /// [`ChangeEvent::unnamed_writes`](super::ChangeEvent::unnamed_writes),
    /// which is exactly the count of writes this feed could not attribute to
    /// a collection.
    pub fn note_unnamed_write(&mut self, rows: u64) {
        if self.service.subscribers.listening() {
            self.batch.note_unnamed_write(rows);
        }
    }

    /// Put one row, recorded.
    pub fn put(&mut self, collection: CollectionId, key: &str, doc: &Value) -> Result<EntityId> {
        let id = self.db.put(collection, key, doc)?;
        if self.service.subscribers.listening() {
            self.batch.note_key(collection, key, ChangeKind::Put);
        }
        Ok(id)
    }

    /// Delete one row, recorded.
    pub fn delete(&mut self, collection: CollectionId, key: &str) -> Result<bool> {
        let removed = self.db.delete(collection, key)?;
        if removed && self.service.subscribers.listening() {
            self.batch.note_key(collection, key, ChangeKind::Delete);
        }
        Ok(removed)
    }

    /// Link one edge, recorded by its edge type.
    pub fn put_edge(
        &mut self,
        context: GraphContextId,
        source: EntityId,
        edge_type: EdgeTypeId,
        destination: EntityId,
        properties: &Value,
    ) -> Result<EdgeKey> {
        let key = self
            .db
            .put_edge(context, source, edge_type, destination, properties)?;
        if self.service.subscribers.listening() {
            self.batch.note_edge_type(edge_type);
        }
        Ok(key)
    }

    /// Unlink one edge, recorded by its edge type.
    pub fn delete_edge(&mut self, key: EdgeKey) -> Result<bool> {
        let removed = self.db.delete_edge(key)?;
        if removed && self.service.subscribers.listening() {
            self.batch.note_edge_type(key.edge_type);
        }
        Ok(removed)
    }

    /// One SQL statement on the writer.
    ///
    /// `BEGIN`, `COMMIT` and `ROLLBACK` are REFUSED with
    /// [`TRANSACTION_SQL_REFUSAL`]: lang compiles `COMMIT` straight to
    /// `Database::commit`, which would cross the durability barrier without
    /// the feed noticing, and §5's whole claim is that the event and the
    /// barrier are the same point. The service owns that point.
    pub fn sql(&mut self, text: &str, params: &[Param]) -> Result<SqlResult> {
        if let Some(keyword) = transaction_keyword(text) {
            return Err(ServiceError::Refused(format!(
                "{keyword}: {TRANSACTION_SQL_REFUSAL}"
            )));
        }
        let budget = self.service.budget();
        let interrupt = self.service.interrupt.clone();
        let result = self
            .db
            .sql_with(text, params, budget, &mut || interrupt.is_cancelled())?;
        if self.service.subscribers.listening() {
            if let SqlResult::Affected(rows) = &result {
                self.batch.note_unnamed_write(*rows);
            }
        }
        Ok(result)
    }

    /// §5's emission point, and §2's republish point.
    ///
    /// In order: the durability barrier, then the event, then the republish.
    /// The event is handed over only once `Database::commit` has returned
    /// `Ok`, so a subscriber that reacts by opening a snapshot always finds
    /// the batch already there; a commit that fails emits nothing and leaves
    /// the batch pending, which the guard's drop then rolls back.
    pub fn commit(&mut self) -> Result<()> {
        self.db.commit()?;
        let event = if self.batch.touched() {
            let event = self.batch.take();
            Some(event)
        } else {
            self.batch.clear();
            None
        };
        if let Some(event) = event {
            // Delivered inside the commit call, after the barrier, while the
            // writer lock is still held: one event, once, in commit order.
            self.service.subscribers.deliver(event);
        }
        self.service.mark_dirty_and_maybe_publish();
        Ok(())
    }

    /// Drop everything written since the last commit. Fires no event and
    /// remembers nothing: §5 says a rolled-back batch fires **not at all**.
    pub fn rollback(&mut self) -> Result<()> {
        self.batch.clear();
        self.db.rollback()?;
        Ok(())
    }
}

impl Drop for WriterGuard<'_> {
    fn drop(&mut self) {
        if self.batch.touched() {
            self.batch.clear();
            let _ = self.db.rollback();
        }
    }
}

impl ServiceDatabase {
    fn mark_dirty_and_maybe_publish(&self) {
        let mut state = self.publish.lock().unwrap_or_else(|e| e.into_inner());
        state.dirty = true;
        if state.last.elapsed() >= self.publish_interval() {
            // The writer pays the mint, so a reader does not.
            let _ = self.publish_locked(&mut state);
        }
    }
}

/// Open one read-only handle and time it. The second term of §2's window.
fn mint(
    path: &Path,
    config: Config,
    serial: u64,
) -> std::result::Result<(Snapshot, Duration), CoreError> {
    let started = Instant::now();
    let db = Database::open_snapshot(path, config)?;
    let cost = started.elapsed();
    Ok((Snapshot::new(db, serial, Instant::now(), cost), cost))
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// The leading keyword when it is transaction control, `None` otherwise.
fn transaction_keyword(text: &str) -> Option<&'static str> {
    let first: String = text
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    match first.as_str() {
        "BEGIN" => Some("BEGIN"),
        "COMMIT" => Some("COMMIT"),
        "ROLLBACK" => Some("ROLLBACK"),
        "START" => Some("START"),
        "END" => Some("END"),
        _ => None,
    }
}

/// Sum one page's work into a running total.
///
/// An exhaustive literal on purpose: a new `QueryWork` counter must be
/// decided about here rather than silently dropped from what a service
/// reports. `groups` and `membership_bytes` are high-water marks, not totals,
/// so they take a maximum.
fn accumulate(total: &mut QueryWork, page: &QueryWork) {
    let QueryWork {
        candidates,
        primary_reads,
        row_decodes,
        scalar_postings,
        graph_edges,
        graph_visited,
        spatial_postings,
        text_postings,
        text_tokens,
        vector_locators,
        vector_sidecars,
        vector_lanes,
        key_postings,
        rows_written,
        groups,
        membership_bytes,
        output_bytes,
    } = *page;
    total.candidates += candidates;
    total.primary_reads += primary_reads;
    total.row_decodes += row_decodes;
    total.scalar_postings += scalar_postings;
    total.graph_edges += graph_edges;
    total.graph_visited += graph_visited;
    total.spatial_postings += spatial_postings;
    total.text_postings += text_postings;
    total.text_tokens += text_tokens;
    total.vector_locators += vector_locators;
    total.vector_sidecars += vector_sidecars;
    total.vector_lanes += vector_lanes;
    total.key_postings += key_postings;
    total.rows_written += rows_written;
    total.groups = total.groups.max(groups);
    total.membership_bytes = total.membership_bytes.max(membership_bytes);
    total.output_bytes += output_bytes;
}
