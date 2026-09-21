//! §1 and §2 of `docs/dist/OPS_CONTRACT.md`: the published read view, and the
//! staleness window around it.
//!
//! **The deviation from the contract's wording, and why.** §1 names the
//! published view `RwLock<Arc<Database>>`. `Arc<Database>` cannot cross a
//! thread boundary in e4: `Database` owns four interior-mutability caches --
//! `RefCell<Option<Catalog>>`, `RefCell<Option<Arc<Layout>>>`,
//! `RefCell<Vec<(IndexId, IndexInfo)>>`, `Cell<Option<GraphHeader>>` -- and
//! the page-WAL store beneath it owns two more `Cell`s, so `Database` is
//! `Send` but not `Sync`, and `Arc<T>` is `Send` only when `T: Send + Sync`.
//! The published view is therefore `RwLock<Arc<Snapshot>>` with [`Snapshot`]
//! owning the handle behind its own `Mutex`. The publication mechanism is
//! unchanged -- a reader clones the `Arc` under a momentary read lock and the
//! writer swaps it under a momentary write lock -- and so is the Law the
//! section exists for: no read takes the WRITER's lock, and no reader blocks
//! the writer.
//!
//! **The cost that buys** (L4): readers sharing one published snapshot
//! serialise against each other on that snapshot's own mutex, because the
//! caches they would race on are per handle.
//! [`ServiceDatabase::reader`](super::ServiceDatabase::reader) hands every
//! caller the same published handle, so two threads reading through it take
//! turns. A caller that wants two walks running at once calls
//! [`ServiceDatabase::open_reader`](super::ServiceDatabase::open_reader),
//! which mints a private handle for that thread and spends one more of the
//! persisted `readers` slots -- the bound §1 says a service must refuse
//! past, never block on.

use sekejap_core::collections::Database;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The default republish interval, matching e3 (`src/service.rs:186`) so a
/// migrating caller's timing does not change under it. A read is stale by at
/// most this plus one snapshot open.
pub const PUBLISH_INTERVAL_DEFAULT: Duration = Duration::from_millis(100);

/// One published read view: a read-only handle on the newest published
/// transaction as of the moment it was minted, byte-stable for its life.
///
/// It holds one reader slot for as long as any `Arc` to it lives, which
/// DEFERS the writer's checkpoint and blocks nothing.
pub struct Snapshot {
    db: Mutex<Database>,
    serial: u64,
    published_at: Instant,
    open_cost: Duration,
}

impl Snapshot {
    pub(super) fn new(db: Database, serial: u64, published_at: Instant, open_cost: Duration) -> Self {
        Self {
            db: Mutex::new(db),
            serial,
            published_at,
            open_cost,
        }
    }

    /// This view's publication ordinal, starting at 1 for the one `open`
    /// minted. Two readers holding the same serial hold the same view.
    pub fn serial(&self) -> u64 {
        self.serial
    }

    /// When this view was swapped in.
    pub fn published_at(&self) -> Instant {
        self.published_at
    }

    /// What minting this view cost. §2 says the window is the publish
    /// interval plus one snapshot open; this is that second term, measured
    /// rather than assumed.
    pub fn open_cost(&self) -> Duration {
        self.open_cost
    }

    /// How long this view has been the published one.
    pub fn age(&self) -> Duration {
        self.published_at.elapsed()
    }

    /// Run `body` against the read-only handle.
    ///
    /// `&mut Database` rather than `&Database` because the SQL surface
    /// (`sekejap_lang::SqlDatabase::sql`) takes `&mut self` for the writes it
    /// also serves; this handle refuses every one of them with
    /// `Error::ReadOnly`, which is the engine's own single-writer rule doing
    /// the work rather than a second rule here.
    pub fn with<T>(&self, body: impl FnOnce(&mut Database) -> T) -> T {
        let mut guard = self.lock();
        body(&mut guard)
    }

    pub(super) fn lock(&self) -> MutexGuard<'_, Database> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }
}
