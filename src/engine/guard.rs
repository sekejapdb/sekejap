//! # Sharing one database across threads — the read/write guard
//!
//! Part of the optional `engine` wrapper. A bare [`CoreDB`] is not thread-safe,
//! so to use it from many threads at once we wrap it in a lock. This file is
//! [`ReadWriteGuard`], a thin wrapper around Rust's [`RwLock`].
//!
//! An `RwLock` (reader-writer lock) is the whole idea: it permits EITHER any
//! number of readers simultaneously, OR one writer alone — never both. That fits
//! a database exactly — queries ([`read`](ReadWriteGuard::read)) run concurrently
//! and don't block each other; only a mutation ([`write`](ReadWriteGuard::write))
//! needs exclusive access. A plain `Mutex` would allow just one holder at a time
//! (even two readers would queue), so `RwLock` removes that read-vs-read waiting.

use std::sync::RwLock;

use crate::CoreDB;

/// Read-write guard wrapping [`CoreDB`] in an [`RwLock`].
///
/// Replaces `Mutex<CoreDB>` to allow concurrent read access. Multiple threads
/// can call [`read()`](Self::read) simultaneously; only [`write()`](Self::write)
/// requires exclusive access.
///
/// This eliminates read starvation: queries no longer queue behind writes.
pub struct ReadWriteGuard {
    inner: RwLock<CoreDB>,
}

impl ReadWriteGuard {
    /// Wrap a [`CoreDB`] instance in a new read-write guard.
    pub fn new(db: CoreDB) -> Self {
        Self {
            inner: RwLock::new(db),
        }
    }

    /// Acquire a shared read lock. Multiple readers proceed concurrently.
    ///
    /// # Panics
    ///
    /// Panics if the `RwLock` is poisoned (a writer panicked while holding it).
    pub fn read(&self) -> std::sync::RwLockReadGuard<'_, CoreDB> {
        self.inner.read().expect("RwLock poisoned")
    }

    /// Acquire an exclusive write lock. Blocks all other readers and writers
    /// until the returned guard is dropped.
    ///
    /// # Panics
    ///
    /// Panics if the `RwLock` is poisoned.
    pub fn write(&self) -> std::sync::RwLockWriteGuard<'_, CoreDB> {
        self.inner.write().expect("RwLock poisoned")
    }

    /// Try to acquire an exclusive write lock without blocking.
    ///
    /// Returns `None` if another thread currently holds a read or write lock.
    pub fn try_write(&self) -> Option<std::sync::RwLockWriteGuard<'_, CoreDB>> {
        self.inner.try_write().ok()
    }

    /// Replace the inner [`CoreDB`] with a new one (hot-swap).
    ///
    /// Takes an exclusive write lock, swaps the database, and returns
    /// the old instance. In-flight reads (holding a read guard) continue
    /// on the old data; new reads see the replacement.
    pub fn replace(&self, new_db: CoreDB) -> CoreDB {
        let mut guard = self.write();
        std::mem::replace(&mut *guard, new_db)
    }

    /// Consume the guard and return the inner [`CoreDB`].
    ///
    /// # Panics
    ///
    /// Panics if the `RwLock` is poisoned.
    pub fn into_inner(self) -> CoreDB {
        self.inner.into_inner().expect("RwLock poisoned")
    }
}
