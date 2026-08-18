//! # Batching writes to amortize the lock — the write buffer
//!
//! Part of the optional `engine` wrapper. Taking the exclusive write lock (see
//! `guard.rs`) has a fixed cost, and paying it once per statement is wasteful
//! when many writes arrive together. [`WriteBuffer`] collects incoming SQL
//! statements in a thread-safe list and signals when a threshold is reached; the
//! engine then takes the write lock ONCE and applies the whole batch, spreading
//! that lock cost over many statements.
//!
//! The pending list is guarded by a `Mutex` (mutual exclusion — one holder at a
//! time) because several threads may push into it at once.

use std::sync::Mutex;

/// Buffered write accumulator for batching SQL mutations.
///
/// Collects SQL statements in a thread-safe buffer and signals when the
/// configured threshold is reached. The caller (typically [`Engine`](super::Engine))
/// then drains the buffer and applies all statements in a single lock
/// acquisition, amortizing the cost of exclusive access.
///
/// # Example
///
/// ```rust,ignore
/// let buf = WriteBuffer::new(50);
///
/// for sql in statements {
///     if buf.push(sql) {
///         // Threshold reached — drain and apply
///         let batch = buf.drain();
///         apply_all(&batch);
///     }
/// }
/// // Drain any remaining
/// let remainder = buf.drain();
/// ```
pub struct WriteBuffer {
    pending: Mutex<Vec<String>>,
    threshold: usize,
}

impl WriteBuffer {
    /// Create a buffer that signals flush at `threshold` pending statements.
    pub fn new(threshold: usize) -> Self {
        Self {
            pending: Mutex::new(Vec::with_capacity(threshold)),
            threshold,
        }
    }

    /// Push a SQL statement into the buffer.
    ///
    /// Returns `true` if the buffer has reached the flush threshold,
    /// signaling the caller to drain and apply.
    pub fn push(&self, sql: String) -> bool {
        let mut buf = self.pending.lock().expect("WriteBuffer poisoned");
        buf.push(sql);
        buf.len() >= self.threshold
    }

    /// Drain all buffered statements, returning them in insertion order.
    ///
    /// The buffer is empty after this call.
    pub fn drain(&self) -> Vec<String> {
        let mut buf = self.pending.lock().expect("WriteBuffer poisoned");
        std::mem::take(&mut *buf)
    }

    /// Put statements back at the **front** of the buffer.
    ///
    /// A flush drains the buffer before it applies anything, so a batch that
    /// fails part-way has statements that exist nowhere else. They used to be
    /// dropped — writes the engine had already answered `Ok` to, gone, with a
    /// retry finding nothing to retry.
    ///
    /// At the front, not the end: other threads may have buffered statements
    /// during the flush, and appending would run the returned statements *after*
    /// writes that were issued later. An `UPDATE` that was queued before an
    /// `INSERT` has to stay before it.
    pub fn restore(&self, stmts: Vec<String>) {
        if stmts.is_empty() {
            return;
        }
        let mut buf = self.pending.lock().expect("WriteBuffer poisoned");
        let queued = std::mem::take(&mut *buf);
        buf.extend(stmts);
        buf.extend(queued);
    }

    /// Number of pending (unflushed) statements.
    pub fn len(&self) -> usize {
        self.pending.lock().expect("WriteBuffer poisoned").len()
    }

    /// Returns `true` if no statements are buffered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Buffered accumulator for PREPARED row writes — pre-built `(slug, Value)` pairs
/// that skip SQL parsing AND JSON re-parsing entirely. Drained via a group-commit
/// batch put (one fsync, one shared timestamp, zero parses). Thread-safe.
pub struct RowBuffer {
    pending: Mutex<Vec<(String, serde_json::Value)>>,
    threshold: usize,
}

impl RowBuffer {
    pub fn new(threshold: usize) -> Self {
        Self { pending: Mutex::new(Vec::with_capacity(threshold)), threshold }
    }
    /// Push a pre-built `(slug, payload Value)`; returns true at the flush threshold.
    pub fn push(&self, slug: String, val: serde_json::Value) -> bool {
        let mut b = self.pending.lock().expect("RowBuffer poisoned");
        b.push((slug, val));
        b.len() >= self.threshold
    }
    pub fn drain(&self) -> Vec<(String, serde_json::Value)> {
        let mut b = self.pending.lock().expect("RowBuffer poisoned");
        std::mem::take(&mut *b)
    }
}
