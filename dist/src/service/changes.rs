//! §5 of `docs/dist/OPS_CONTRACT.md`: change notifications, one event per
//! committed batch.
//!
//! The delivery rule is the whole value of the primitive and it is exact:
//!
//! * **One event per committed batch.** A batch is everything written between
//!   two [`WriterGuard::commit`](super::WriterGuard::commit) calls, so a
//!   single write followed by a commit fires once and a hundred writes
//!   followed by one commit fire once, with the union of what they touched.
//! * **Never on rollback.** The accumulator is dropped whole.
//! * **After the durability barrier, never before it.** The event is
//!   assembled while the writer lock is still held, but it is handed to the
//!   subscribers only once `Database::commit` has returned `Ok`. A subscriber
//!   that reacts by opening a snapshot therefore always finds the batch
//!   already published; there is no window in which the signal runs ahead of
//!   the durable state. That ordering is the L3 shape at the API.
//! * **A slow subscriber never blocks the writer.** The channel is bounded at
//!   [`CHANGE_QUEUE_BOUND`] events; a send into a full queue is dropped and
//!   counted in [`Receiver::lagged`], it is not waited on.
//!
//! Recording is free when nobody is listening: every record call returns on a
//! relaxed load of the listener count before it allocates anything.
//!
//! **L1, and where e4 does not copy e3.** e3's `ChangeEvent.keys` grows with
//! the batch, so one bulk commit of ten million rows holds ten million key
//! strings in RAM before a listener sees any of them. Here the key list stops
//! at [`CHANGE_KEY_CAP`] and the event reports [`ChangeEvent::keys_truncated`]
//! with [`ChangeEvent::keys_total`]; the collection and edge-type lists are
//! carried in full because both are bounded by the catalog, which is bounded
//! by the schema. A listener past the cap does what a listener is supposed to
//! do anyway: re-run its query against the collections it was told about.

use sekejap_core::collections::{CollectionId, EdgeTypeId};
use std::sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    mpsc::{sync_channel, Receiver as MpscReceiver, SyncSender, TryRecvError, TrySendError},
    Arc, Mutex,
};
use std::time::Duration;

/// Events one subscriber's queue holds before the next is dropped and
/// counted. A stated bound, in the same family as §5's key cap: the memory a
/// subscriber can cost the writer is `CHANGE_QUEUE_BOUND` events, and the
/// writer's cost of a subscriber that has stopped reading is one failed
/// `try_send` per commit.
pub const CHANGE_QUEUE_BOUND: usize = 256;

/// Keys one event carries before the list is dropped whole and
/// [`ChangeEvent::keys_truncated`] is set. Fixed at open, in the same family
/// as the plan cache's three ceilings.
pub const CHANGE_KEY_CAP: usize = 1_024;

/// What happened to one key in a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Put,
    Delete,
}

/// One changed key, with the collection it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedKey {
    pub collection: CollectionId,
    pub key: String,
    pub kind: ChangeKind,
}

/// What one committed batch moved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeEvent {
    /// This event's ordinal in the service's DELIVERED stream, starting at
    /// 1. A subscriber that sees `sequence` jump knows exactly how many
    /// events its queue dropped, which is the same number
    /// [`Receiver::lagged`] reports.
    ///
    /// It is the commit ordinal only while somebody is listening: a commit
    /// made with no subscriber records nothing and is not numbered, which is
    /// what "recording is free when nobody is listening" costs.
    pub sequence: u64,
    /// Every collection whose members moved, deduplicated, in first-touch
    /// order. Carried in full: bounded by the catalog.
    pub collections: Vec<CollectionId>,
    /// Every edge type linked or unlinked, deduplicated. Carried in full for
    /// the same reason.
    pub edge_types: Vec<EdgeTypeId>,
    /// The keys this batch put or deleted, up to [`CHANGE_KEY_CAP`]. Empty
    /// when `keys_truncated` is set: the list is dropped whole rather than
    /// handed over half-true.
    pub keys: Vec<ChangedKey>,
    /// How many key mutations the batch made, whether or not they are listed.
    pub keys_total: u64,
    /// Set when `keys_total` passed [`CHANGE_KEY_CAP`] and `keys` was
    /// dropped. `collections` is still exact, which is what a listener past
    /// the cap re-runs its query against.
    pub keys_truncated: bool,
    /// Writes this feed could not attribute to a collection: SQL DML and DDL
    /// run through [`WriterGuard::sql`](super::WriterGuard::sql).
    ///
    /// The reason is a layer boundary, not a design choice: `lang`'s compiled
    /// `WritePlan` -- which is the only thing that knows a statement's target
    /// collection -- is `pub(crate)` to `sekejap-lang`, and widening it is a
    /// lang change this surface does not make. A listener that sees this
    /// above zero re-runs its query, exactly as one past the key cap does.
    pub unnamed_writes: u64,
    /// Rows the batch's SQL statements reported affected.
    pub rows_affected: u64,
}

/// A subscriber's end of the feed.
///
/// Not `Sync`: one thread owns it and reads it. Cloning a subscription is
/// subscribing again, which is what gives each listener its own bound.
pub struct Receiver {
    id: SubscriptionId,
    rx: MpscReceiver<ChangeEvent>,
    lagged: Arc<AtomicU64>,
}

/// The token [`ServiceDatabase::unsubscribe`](super::ServiceDatabase::unsubscribe)
/// takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SubscriptionId(pub u64);

impl Receiver {
    pub fn id(&self) -> SubscriptionId {
        self.id
    }

    /// The next event, or `None` when none is queued.
    pub fn try_recv(&self) -> Option<ChangeEvent> {
        match self.rx.try_recv() {
            Ok(event) => Some(event),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }

    /// The next event, waiting up to `timeout`.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<ChangeEvent> {
        self.rx.recv_timeout(timeout).ok()
    }

    /// How many events this subscriber's queue dropped because it was full
    /// when the writer committed. Counted, never silent: a feed that loses an
    /// event and does not say so is worse than no feed.
    pub fn lagged(&self) -> u64 {
        self.lagged.load(Ordering::Relaxed)
    }
}

struct Subscriber {
    id: SubscriptionId,
    tx: SyncSender<ChangeEvent>,
    lagged: Arc<AtomicU64>,
}

/// The registry the writer delivers through.
pub(super) struct Subscribers {
    live: Mutex<Vec<Subscriber>>,
    /// Read relaxed before any recording work happens, so a database with no
    /// listener pays one atomic load per mutation and nothing else.
    count: AtomicUsize,
    next_id: AtomicU64,
    sequence: AtomicU64,
}

impl Subscribers {
    pub(super) fn new() -> Self {
        Self {
            live: Mutex::new(Vec::new()),
            count: AtomicUsize::new(0),
            next_id: AtomicU64::new(1),
            sequence: AtomicU64::new(0),
        }
    }

    pub(super) fn listening(&self) -> bool {
        self.count.load(Ordering::Relaxed) > 0
    }

    pub(super) fn subscribe(&self) -> Receiver {
        let (tx, rx) = sync_channel(CHANGE_QUEUE_BOUND);
        let id = SubscriptionId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let lagged = Arc::new(AtomicU64::new(0));
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        live.push(Subscriber {
            id,
            tx,
            lagged: Arc::clone(&lagged),
        });
        self.count.store(live.len(), Ordering::Relaxed);
        Receiver { id, rx, lagged }
    }

    pub(super) fn unsubscribe(&self, id: SubscriptionId) -> bool {
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let before = live.len();
        live.retain(|s| s.id != id);
        self.count.store(live.len(), Ordering::Relaxed);
        before != live.len()
    }

    /// Stamp the batch with its commit ordinal and hand it to every live
    /// subscriber. Called by the writer AFTER `Database::commit` returned
    /// `Ok`, and by nothing else.
    pub(super) fn deliver(&self, mut event: ChangeEvent) {
        event.sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let mut dead = Vec::new();
        for subscriber in live.iter() {
            match subscriber.tx.try_send(event.clone()) {
                Ok(()) => {}
                // The bound is a bound: a subscriber that is not draining
                // loses the event and is told how many it lost. The writer
                // does not wait.
                Err(TrySendError::Full(_)) => {
                    subscriber.lagged.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => dead.push(subscriber.id),
            }
        }
        if !dead.is_empty() {
            live.retain(|s| !dead.contains(&s.id));
            self.count.store(live.len(), Ordering::Relaxed);
        }
    }

    /// Commits delivered so far. The ordinal the next event will carry is
    /// this plus one.
    pub(super) fn delivered(&self) -> u64 {
        self.sequence.load(Ordering::Relaxed)
    }
}

/// What a batch has touched so far. Lives on the writer guard, is drained by
/// a commit and dropped by a rollback.
#[derive(Default)]
pub(super) struct PendingBatch {
    collections: Vec<CollectionId>,
    edge_types: Vec<EdgeTypeId>,
    keys: Vec<ChangedKey>,
    keys_total: u64,
    keys_truncated: bool,
    unnamed_writes: u64,
    rows_affected: u64,
    /// Whether anything at all was recorded. A commit with nothing recorded
    /// still commits; whether it emits is the caller's `emit_empty` choice in
    /// `WriterGuard::commit`.
    touched: bool,
}

impl PendingBatch {
    pub(super) fn note_key(&mut self, collection: CollectionId, key: &str, kind: ChangeKind) {
        self.touched = true;
        self.note_collection(collection);
        self.keys_total = self.keys_total.saturating_add(1);
        if self.keys_truncated {
            return;
        }
        if self.keys.len() == CHANGE_KEY_CAP {
            // Dropped whole, not trimmed: a half list read as a full one is
            // the failure mode this cap exists to prevent.
            self.keys = Vec::new();
            self.keys_truncated = true;
            return;
        }
        self.keys.push(ChangedKey {
            collection,
            key: key.to_owned(),
            kind,
        });
    }

    pub(super) fn note_collection(&mut self, collection: CollectionId) {
        self.touched = true;
        if !self.collections.contains(&collection) {
            self.collections.push(collection);
        }
    }

    pub(super) fn note_edge_type(&mut self, edge_type: EdgeTypeId) {
        self.touched = true;
        if !self.edge_types.contains(&edge_type) {
            self.edge_types.push(edge_type);
        }
    }

    pub(super) fn note_unnamed_write(&mut self, rows: u64) {
        self.touched = true;
        self.unnamed_writes = self.unnamed_writes.saturating_add(1);
        self.rows_affected = self.rows_affected.saturating_add(rows);
    }

    pub(super) fn touched(&self) -> bool {
        self.touched
    }

    /// Take the batch, leaving an empty one behind. `sequence` is stamped by
    /// [`Subscribers::deliver`], which is the only thing that may number a
    /// commit.
    pub(super) fn take(&mut self) -> ChangeEvent {
        let taken = std::mem::take(self);
        ChangeEvent {
            sequence: 0,
            collections: taken.collections,
            edge_types: taken.edge_types,
            keys: taken.keys,
            keys_total: taken.keys_total,
            keys_truncated: taken.keys_truncated,
            unnamed_writes: taken.unnamed_writes,
            rows_affected: taken.rows_affected,
        }
    }

    /// Drop everything recorded. A rollback emits nothing and remembers
    /// nothing.
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }
}
