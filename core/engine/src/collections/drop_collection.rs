//! Removing a collection: the DROPPING mark, and the bounded, resumable steps
//! that empty every keyspace the collection owns before its descriptor goes.
//!
//! The shape is `catalog.rs`'s `begin_drop_index` / `drop_index_step`, one
//! level up. An index is one keyspace and one descriptor; a collection is
//! seven keyspaces, a name, a sequence counter, a layout and every index of
//! its own, so the state machine has phases and the descriptor carries which
//! one it reached.
//!
//! ## Law 3 -- nothing is deleted before the mark is published
//!
//! [`Database::begin_drop_collection`] commits the DROPPING mark and the
//! feature bit together, BEFORE it returns, and removes nothing. So by the
//! time a single entry can be deleted the mark is durable, and every reader
//! and writer of that collection -- `get`, `scan`, `put`, `delete`,
//! `alter_collection`, `prepare_query`, `list_indexes`, and an edge written
//! onto one of its rows -- is already refusing with a named reason. A crash at
//! any point after that leaves a collection that is visibly mid-drop and
//! resumable, never one that has silently lost half its rows.
//!
//! The descriptor is removed LAST, in the step that has just proved by a range
//! probe that every keyspace of the collection is empty.
//!
//! ## The phases, in order, and why that order
//!
//! | phase | keyspace | why here |
//! | --- | --- | --- |
//! | `Indexes` | every index of the collection | first, so no later delete pays index maintenance, and so a half-dropped index is never left pointing at rows that are gone |
//! | `Sidecars` | the `0x60` prefix of the collection -- vector cells | before the rows whose layout describes them |
//! | `Rows` | the `0x40` prefix -- primary rows, plus the incident edges under CASCADE | |
//! | `Mappings` | the `0x20` prefix -- the external-key mapping | after the rows, so a surviving mapping never points at a row that is gone while a reader could still see it -- it cannot, the collection is DROPPING |
//! | `Descriptor` | the name, the catalog replicas, the sequence replicas, the layout replicas | last, after the probes |
//!
//! ## The cursor
//!
//! There is no byte cursor. Each phase deletes from the FRONT of its own
//! prefix, so what is left in the keyspace is itself the cursor: a step scans
//! from the prefix start, takes at most `budget` entries, deletes them, and
//! advances the phase when it finds fewer than `budget`. The committed cursor
//! is therefore the phase byte in the descriptor plus the surviving keys, and
//! a resume after a crash needs nothing that a reopen does not already read.
//! The `removed` counter in the descriptor is a total for the caller, not an
//! input to the walk.
//!
//! ## Format
//!
//! The catalog descriptor had no state field, so this adds one: bit 1 of the
//! frozen flags byte plus a ten-byte tail (see `CATALOG_DROPPING` in
//! `super`), admitted by the additive feature bit [`DROP_FEATURE`]. A database
//! that has never begun a drop is byte-identical to one written before this
//! code and opens in every older binary.
use super::*;
use crate::collections::catalog::{COLLECTION_INDEX, INDEX_NAME};
use crate::index::graph::{PRIMARY_EDGE, REVERSE_EDGE};

/// The collection-header bit that says one catalog record in this file
/// carries the DROPPING tail.
///
/// NOT monotone, and this is the one bit that is not. Every other logical
/// feature bit marks a representation the file may hold for the rest of its
/// life, and nothing can cheaply prove the last instance of it is gone. This
/// one marks a representation that exists only between
/// `begin_drop_collection` and the final step, and the final step PROVES no
/// record carries it -- it is the step that removes the last one, and it
/// removes it having range-probed every keyspace of the collection empty. So
/// the bit is cleared there, and a database goes back to opening in a binary
/// that predates this code once the drop it was set for has finished. Leaving
/// it set would cost that compatibility permanently to describe a state that
/// no longer exists.
pub(crate) const DROP_FEATURE: u64 = 0x200;

/// The widest batch one step may remove, matching `catalog::MAX_BATCH`: a
/// drop's transaction is the same page-WAL allowance an index drop's is.
pub const MAX_DROP_BATCH: usize = 256;

/// How many (entity, context) runs the RESTRICT probe seeks through before it
/// reports what it has. See `Database::collection_edge_contexts`.
const RESTRICT_PROBE_SEEKS: usize = 1024;

/// What a drop does about graph edges that reference the collection's rows.
/// GRAPH_CONTRACT 6.1: RESTRICT is the default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropMode {
    /// Refuse to begin while any edge in any context references a row of the
    /// collection, naming those contexts.
    Restrict,
    /// Remove each row's incident edges through the existing bounded cascade,
    /// inside the same bounded steps that remove the rows.
    Cascade,
}

impl DropMode {
    pub(super) fn byte(self) -> u8 {
        match self {
            Self::Restrict => 0,
            Self::Cascade => 1,
        }
    }
    pub(super) fn from_byte(b: u8) -> Result<Self> {
        match b {
            0 => Ok(Self::Restrict),
            1 => Ok(Self::Cascade),
            _ => Err(corrupt("catalog drop mode")),
        }
    }
}

/// Which keyspace the drop is working through. The order is the order of the
/// table in this module's documentation and the byte is the persisted form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DropPhase {
    Indexes,
    Sidecars,
    Rows,
    Mappings,
    Descriptor,
}

impl DropPhase {
    pub(super) fn byte(self) -> u8 {
        match self {
            Self::Indexes => 0,
            Self::Sidecars => 1,
            Self::Rows => 2,
            Self::Mappings => 3,
            Self::Descriptor => 4,
        }
    }
    pub(super) fn from_byte(b: u8) -> Result<Self> {
        Ok(match b {
            0 => Self::Indexes,
            1 => Self::Sidecars,
            2 => Self::Rows,
            3 => Self::Mappings,
            4 => Self::Descriptor,
            _ => return Err(corrupt("catalog drop phase")),
        })
    }
    /// The phase's name, as a refusal and a progress report spell it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Indexes => "indexes",
            Self::Sidecars => "vector sidecars",
            Self::Rows => "rows",
            Self::Mappings => "external-key mappings",
            Self::Descriptor => "descriptor",
        }
    }
    fn next(self) -> Self {
        match self {
            Self::Indexes => Self::Sidecars,
            Self::Sidecars => Self::Rows,
            Self::Rows => Self::Mappings,
            Self::Mappings => Self::Descriptor,
            Self::Descriptor => Self::Descriptor,
        }
    }
}

/// The committed cursor of a drop, as the descriptor carries it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DropState {
    pub phase: DropPhase,
    pub mode: DropMode,
    /// Entries removed since `begin_drop_collection`, across every phase.
    pub removed: u64,
}

/// What one [`Database::drop_collection_step`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DropProgress {
    /// True when the descriptor is gone and the collection no longer exists.
    pub done: bool,
    /// Entries this step removed.
    pub removed: u64,
    /// Entries every step of this drop has removed.
    pub total_removed: u64,
    /// The phase the drop is in AFTER this step. `Descriptor` with
    /// `done == false` means the next step is the final one.
    pub phase: DropPhase,
}

/// The one collection whose descriptor carries a DROPPING mark, or `None`.
///
/// Reads nothing at all unless the header declares [`DROP_FEATURE`], which it
/// does only while a drop is actually in flight. When it does, this is one
/// range over the copy-0 catalog replica keys -- proportional to the number of
/// collections, not to their rows -- and each candidate is read through the
/// three-replica agreement, so a damaged copy-0 record cannot hide a drop.
pub(super) fn scan_dropping(
    s: &crate::pagewal::PageWalStore,
    header: Option<IndexHeader>,
) -> Result<Option<CollectionId>> {
    if !header.is_some_and(|h| h.features & DROP_FEATURE != 0) {
        return Ok(None);
    }
    let prefix = [1u8, 0u8];
    for row in s.range(&prefix)? {
        let (k, _) = row?;
        if !has_prefix(&k, &prefix) {
            break;
        }
        let mut at = 2;
        let id = CollectionId(u32::try_from(read_ordered(&k, &mut at)?).map_err(corrupt)?);
        if at != k.len() || id.0 == 0 {
            return Err(corrupt("catalog replica key"));
        }
        let c = replicas(
            |k| s.get(k).map_err(Error::from),
            |copy| replica_key(1, id.0, copy),
            parse_catalog,
        )?;
        if c.drop.is_some() {
            return Ok(Some(id));
        }
    }
    Err(corrupt(
        "collection header declares a drop in flight and no collection is DROPPING",
    ))
}

impl Database {
    /// Publish the DROPPING mark, RESTRICT (GRAPH_CONTRACT 6.1).
    ///
    /// Refuses while any edge in any context references a row of the
    /// collection, naming those contexts. Nothing is removed here and nothing
    /// is removed until this commit is durable.
    pub fn begin_drop_collection(&mut self, id: CollectionId) -> Result<()> {
        self.begin_drop_collection_mode(id, DropMode::Restrict)
    }

    /// Publish the DROPPING mark in the mode the caller asked for.
    ///
    /// `DropMode::Cascade` is the explicit opt-in of GRAPH_CONTRACT L3: the
    /// rows' incident edges are removed by the drop, in every context, through
    /// `cascade_graph_delete`, inside the bounded row steps.
    pub fn begin_drop_collection_mode(&mut self, id: CollectionId, mode: DropMode) -> Result<()> {
        self.ready_write()?;
        // A drop is engine work, like an index build: it may commit its own
        // steps, and committing a caller's half-finished rows on their behalf
        // is not the engine's decision to make. Same guard, same remedy.
        if self.user_writes_pending {
            return Err(invalid("commit pending writes before dropping a collection"));
        }
        // Refuses a collection that is already DROPPING, which is what makes
        // "at most one drop in flight" true rather than hoped for.
        let mut c = self.catalog(id)?;
        if let Some(other) = self.dropping {
            return Err(invalid(format!(
                "collection {} is already DROPPING; finish it with drop_collection_step before starting another",
                other.0
            )));
        }
        if mode == DropMode::Restrict {
            let (contexts, truncated) = self.collection_edge_contexts(id, RESTRICT_PROBE_SEEKS)?;
            if !contexts.is_empty() {
                let mut named = Vec::with_capacity(contexts.len());
                for context in &contexts {
                    named.push(format!("`{}`", self.graph_context_name(*context)?));
                }
                return Err(invalid(format!(
                    "RESTRICT: `{}` cannot be dropped while graph edges reference its rows; {} context(s) hold such edges: {}{}. Drop the edges, or ask for CASCADE.",
                    c.name,
                    contexts.len(),
                    named.join(", "),
                    if truncated {
                        format!(
                            " (the probe stopped after {RESTRICT_PROBE_SEEKS} seeks per direction; there may be more)"
                        )
                    } else {
                        String::new()
                    }
                )));
            }
        }
        // Everything the handle already holds about this collection is about
        // to stop being true; a sequence buffered here would be rewritten by
        // the next commit into a counter this drop is removing.
        if self.store.is_dirty() || self.sequence.is_some() {
            self.commit()?;
        }
        c.drop = Some(DropState {
            phase: DropPhase::Indexes,
            mode,
            removed: 0,
        });
        let result = (|| {
            // The feature bit rides the same transaction as the mark: a file
            // whose catalog carries the DROPPING tail declares it, so a binary
            // without this code refuses the file whole at admission rather
            // than reading the collection as live (Law 8).
            let mut header = self.index_header.unwrap_or(IndexHeader {
                features: 1,
                next: 1,
                count: 0,
            });
            header.features |= DROP_FEATURE;
            let (next_collection, next_layout) = self.header()?;
            self.index_header = Some(header);
            self.write_header(next_collection, next_layout)?;
            self.persist_catalog(&c)?;
            self.dropping = Some(id);
            // Law 3: published before the first entry can be removed.
            self.commit()
        })();
        self.finish(result)
    }

    /// The committed cursor of a drop in flight on `id`, or `None` when the
    /// collection is live.
    pub fn drop_collection_state(&self, id: CollectionId) -> Result<Option<DropState>> {
        Ok(self.catalog_any(id)?.drop)
    }

    /// The collection this database is in the middle of dropping, if any.
    /// A reopen after a crash finds the resume point with this.
    pub fn dropping_collection(&self) -> Option<CollectionId> {
        self.dropping
    }

    /// Remove at most `budget` entries of a DROPPING collection and return
    /// what was removed.
    ///
    /// Law 1: the step reads and writes at most `budget` entries of one
    /// keyspace plus, under CASCADE, at most 256 incident edges per row (the
    /// bound `preflight_incident_edges` already enforces). It holds `budget`
    /// keys and nothing else, so its memory is the caller's bound and not the
    /// collection's size.
    ///
    /// The caller commits. Each step's deletions and the descriptor's phase
    /// are one transaction, so a commit lands a consistent cursor and a crash
    /// loses at most the uncommitted step.
    pub fn drop_collection_step(&mut self, id: CollectionId, budget: usize) -> Result<DropProgress> {
        self.ready_write()?;
        if !(1..=MAX_DROP_BATCH).contains(&budget) {
            return Err(invalid("drop batch must be 1..256"));
        }
        let c = self.catalog_any(id)?;
        let Some(state) = c.drop else {
            return Err(invalid("call begin_drop_collection first"));
        };
        let result = (|| match state.phase {
            DropPhase::Indexes => self.drop_step_indexes(&c, state, budget),
            DropPhase::Sidecars => self.drop_step_prefix(&c, state, budget, prefix(0x60, id)),
            DropPhase::Rows => self.drop_step_rows(&c, state, budget),
            DropPhase::Mappings => self.drop_step_prefix(&c, state, budget, prefix(0x20, id)),
            DropPhase::Descriptor => self.drop_step_descriptor(&c, state),
        })();
        self.finish(result)
    }

    /// Run a drop already begun to its end, committing every step. The SQL
    /// `DROP TABLE` path and a resume after a crash both use this.
    pub fn drop_collection_to_end(&mut self, id: CollectionId, budget: usize) -> Result<u64> {
        loop {
            let progress = self.drop_collection_step(id, budget)?;
            self.commit()?;
            if progress.done {
                return Ok(progress.total_removed);
            }
        }
    }

    /// Begin and finish in one call, RESTRICT. The bounded steps are still
    /// bounded; this only spares a caller with nothing else to do the loop.
    pub fn drop_collection(&mut self, id: CollectionId) -> Result<u64> {
        self.begin_drop_collection(id)?;
        self.drop_collection_to_end(id, MAX_DROP_BATCH)
    }

    /// One step of the index phase: advance the drop of the first index the
    /// collection still has, cancelling a BUILDING one the way
    /// `begin_drop_index` already does.
    fn drop_step_indexes(
        &mut self,
        c: &Catalog,
        state: DropState,
        budget: usize,
    ) -> Result<DropProgress> {
        let Some(index) = self.list_indexes_any(c.id)?.into_iter().next() else {
            return self.advance(c, state, 0);
        };
        if index.state != catalog::IndexState::Dropping {
            self.begin_drop_index(index.id)?;
        }
        let before = self.index_header.map_or(0, |h| h.count);
        let end = self.drop_index_step(index.id, budget)?;
        // An index's own step does not count entries, so the drop counts what
        // it can account for: one per index removed. The bound the step
        // honoured is `budget`; the number is a report, not a cursor.
        let removed = u64::from(end && self.index_header.map_or(0, |h| h.count) < before);
        self.save_drop_state(c, DropState { removed: state.removed + removed, ..state })?;
        Ok(DropProgress {
            done: false,
            removed,
            total_removed: state.removed + removed,
            phase: DropPhase::Indexes,
        })
    }

    /// One step of a phase that is a single prefix: take the first `budget`
    /// keys under it and delete them.
    fn drop_step_prefix(
        &mut self,
        c: &Catalog,
        state: DropState,
        budget: usize,
        prefix: Vec<u8>,
    ) -> Result<DropProgress> {
        let keys = self.first_keys(&prefix, budget)?;
        let end = keys.len() < budget;
        let removed = keys.len() as u64;
        for key in keys {
            self.writer()?.delete(&key)?;
        }
        if end {
            return self.advance(c, state, removed);
        }
        let total = state.removed + removed;
        self.save_drop_state(c, DropState { removed: total, ..state })?;
        Ok(DropProgress {
            done: false,
            removed,
            total_removed: total,
            phase: state.phase,
        })
    }

    /// The row phase. Identical to `drop_step_prefix` but for CASCADE, which
    /// removes each row's incident edges -- in every context, forward and
    /// reverse -- through the audited cascade before the row itself goes.
    fn drop_step_rows(
        &mut self,
        c: &Catalog,
        state: DropState,
        budget: usize,
    ) -> Result<DropProgress> {
        let prefix = prefix(0x40, c.id);
        let keys = self.first_keys(&prefix, budget)?;
        let end = keys.len() < budget;
        let removed = keys.len() as u64;
        for key in keys {
            let id = row_id(&key)?;
            if state.mode == DropMode::Cascade {
                self.cascade_graph_delete(id)?;
            }
            self.writer()?.delete(&key)?;
            self.note_deleted(id);
        }
        if end {
            return self.advance(c, state, removed);
        }
        let total = state.removed + removed;
        self.save_drop_state(c, DropState { removed: total, ..state })?;
        Ok(DropProgress {
            done: false,
            removed,
            total_removed: total,
            phase: DropPhase::Rows,
        })
    }

    /// The last step. Every keyspace the collection owns is probed empty
    /// first; only then does the descriptor go, and with it the name, the
    /// sequence counter and the layout.
    fn drop_step_descriptor(&mut self, c: &Catalog, state: DropState) -> Result<DropProgress> {
        for (what, probe) in [
            ("index registry", prefix(COLLECTION_INDEX, c.id)),
            ("index names", prefix(INDEX_NAME, c.id)),
            ("vector sidecars", prefix(0x60, c.id)),
            ("rows", prefix(0x40, c.id)),
            ("external-key mappings", prefix(0x20, c.id)),
            ("outgoing graph edges", prefix(PRIMARY_EDGE, c.id)),
            ("incoming graph edges", prefix(REVERSE_EDGE, c.id)),
        ] {
            if let Some(row) = self.store()?.range(&probe)?.next() {
                let (key, _) = row?;
                if has_prefix(&key, &probe) {
                    return Err(corrupt(format!(
                        "drop of `{}` reached its last step with its {what} keyspace not empty",
                        c.name
                    )));
                }
            }
        }
        let removed = 3 + 3 + 3 + 1;
        let total = state.removed + removed;
        // A buffered sequence for this collection would be flushed by the
        // next commit into the counter this step is deleting.
        if self.sequence.as_ref().is_some_and(|s| s.collection == c.id) {
            self.sequence = None;
        }
        self.writer()?.delete(&name_key(&c.name))?;
        for copy in 0..3 {
            self.writer()?.delete(&replica_key(1, c.id.0, copy))?;
            self.writer()?.delete(&replica_key(2, c.id.0, copy))?;
            // SACRIFICE (Law 4): the layout the descriptor names, and only
            // that one. `alter_collection` allocates a NEW layout id and
            // leaves the superseded descriptor in the file already -- that is
            // HEAD's behaviour, independent of any drop -- and a layout is
            // keyed by its own id, so there is no per-collection layout
            // keyspace to enumerate. What is left behind is what an alter
            // already left behind: 3 x 2,081 bytes per superseded version,
            // reachable from no catalog record, and unable to alias anything
            // because layout ids come from a monotone counter and are never
            // reused.
            self.writer()?.delete(&layout_key(c.layout, copy))?;
        }
        // The DROPPING representation is gone from the file, proved by the
        // probes above; see `DROP_FEATURE`.
        if let Some(mut header) = self.index_header {
            if header.features & DROP_FEATURE != 0 {
                header.features &= !DROP_FEATURE;
                let (next_collection, next_layout) = self.header()?;
                self.index_header = Some(header);
                self.write_header(next_collection, next_layout)?;
            }
        }
        *self.catalog_cache.borrow_mut() = None;
        self.index_descriptors_changed();
        *self.layout_cache.borrow_mut() = None;
        self.allocated.remove(&c.id);
        self.dropping = None;
        Ok(DropProgress {
            done: true,
            removed,
            total_removed: total,
            phase: DropPhase::Descriptor,
        })
    }

    /// Move the cursor on to the next phase, committing the phase byte in the
    /// same transaction as the last deletions of the phase that ended.
    fn advance(&mut self, c: &Catalog, state: DropState, removed: u64) -> Result<DropProgress> {
        let total = state.removed + removed;
        let phase = state.phase.next();
        self.save_drop_state(
            c,
            DropState {
                phase,
                removed: total,
                ..state
            },
        )?;
        Ok(DropProgress {
            done: false,
            removed,
            total_removed: total,
            phase,
        })
    }

    fn save_drop_state(&mut self, c: &Catalog, state: DropState) -> Result<()> {
        let mut next = c.clone();
        next.drop = Some(state);
        self.persist_catalog(&next)
    }

    /// The first `budget` keys under `prefix`, in key order. One descent: the
    /// phase deletes from the front, so the front of the range is always where
    /// the next batch is.
    fn first_keys(&self, prefix: &[u8], budget: usize) -> Result<Vec<Vec<u8>>> {
        let mut keys = Vec::with_capacity(budget);
        for row in self.store()?.range(prefix)? {
            let (key, _) = row?;
            if !has_prefix(&key, prefix) {
                break;
            }
            keys.push(key);
            if keys.len() == budget {
                break;
            }
        }
        Ok(keys)
    }
}
