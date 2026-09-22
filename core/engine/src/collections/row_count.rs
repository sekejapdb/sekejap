//! The LIVE ROW COUNT: one record per collection, exact and crash-consistent,
//! so `count(*)` with no filter and no group is a single `get` instead of a
//! walk of the whole collection.
//!
//! ## What is on disk
//!
//! One record per collection, in its own small keyspace:
//!
//! ```text
//! key   = 0x08 || ordered(collection id)        (3..5 bytes)
//! value = rows: u64 BE || generation: u64 BE    (16 bytes, fixed)
//! ```
//!
//! `0x08` is free in the tag registry audited in `docs/core/FORMAT_V2.md`
//! "Extension boundary" and in this crate: `0` (header and layout replicas),
//! `1` and `2` (collection descriptor and sequence replicas, `replica_key`),
//! `3` `4` `5` (index descriptor, registry, per-collection index list),
//! `6` `7` (graph header, graph name descriptor), `0x10` (collection names),
//! `0x11` (index names), `0x12` (graph name lookup), `0x20` (external-key
//! mappings), `0x40` (primary rows), `0x60` (vector sidecars) and `0x70`
//! through `0x7c` (the index families). `0x08` sits with the other
//! per-collection metadata and collides with none of them.
//!
//! `generation` is the number of commits that have written this record. It
//! decides nothing -- the count is `rows` -- and it is there so an operator
//! reading two copies of a file can tell which one is newer.
//!
//! ## How it stays exact across a crash
//!
//! The delta is accumulated IN MEMORY, per transaction, per collection, and
//! written ONCE per touched collection at commit -- one tree put per
//! collection per commit, not one per row -- inside the same transaction as
//! the rows themselves. So the record and the rows it counts become durable
//! together or not at all, and a crash cannot leave the count off by one. A
//! rollback discards the delta with the rows, because it discards the map.
//!
//! ## The rule that keeps a stale record impossible
//!
//! A collection's count is maintained if and only if the collection HAS a
//! record. A database written before this feature has none, so nothing is
//! maintained, nothing extra is written, and its files stay byte for byte
//! what they were; `count(*)` takes the walk it always took. The file-level
//! gate is [`ROW_COUNT_FEATURE`]: when the header does not declare it, no
//! record exists anywhere in the file and the write path does not even probe.
//!
//! [`Database::backfill_row_counts`] is the bounded, resumable atomic that
//! builds the records once for a database that has none.

use super::{
    corrupt, has_prefix, invalid, prefix, row_id_after_prefix, CollectionId, Database, EntityId,
    Result,
};
use std::collections::BTreeMap;

/// The key tag of the live-row-count keyspace. See the module comment for the
/// audit that says it is free.
pub(crate) const ROW_COUNT: u8 = 0x08;

/// The fixed record: `rows` then `generation`, both big-endian.
const RECORD_BYTES: usize = 16;

/// The collection-header bit that says this database carries live row-count
/// records.
///
/// Additive and monotone (Law 8): set in the same transaction that writes the
/// FIRST counter record, never by opening and never by an ordinary write, and
/// never cleared. It exists for the refusal CLASS. A binary that predates the
/// keyspace must refuse the file whole at admission, as `Unsupported`, before
/// a record is read -- not read the file and answer `count(*)` from a walk
/// while a newer binary answers it from a record the older one would never
/// maintain. `admit_features` is the one decision, and
/// `collections::tests::a_row_count_file_is_unsupported_to_a_binary_that_predates_the_bit`
/// puts this build's mask minus this bit through it.
pub const ROW_COUNT_FEATURE: u64 = 0x2000;

/// One collection's record as it is read and written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowCountRecord {
    pub rows: u64,
    pub generation: u64,
}

pub(crate) fn row_count_key(c: CollectionId) -> Vec<u8> {
    prefix(ROW_COUNT, c)
}

pub(crate) fn encode(record: RowCountRecord) -> [u8; RECORD_BYTES] {
    let mut b = [0u8; RECORD_BYTES];
    b[..8].copy_from_slice(&record.rows.to_be_bytes());
    b[8..].copy_from_slice(&record.generation.to_be_bytes());
    b
}

pub(crate) fn decode(b: &[u8]) -> Result<RowCountRecord> {
    if b.len() != RECORD_BYTES {
        return Err(corrupt("live row count record length"));
    }
    Ok(RowCountRecord {
        rows: u64::from_be_bytes(b[..8].try_into().unwrap()),
        generation: u64::from_be_bytes(b[8..].try_into().unwrap()),
    })
}

/// One collection's uncommitted arithmetic: what the record said when this
/// transaction first touched the collection, and what this transaction has
/// done to it since.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Pending {
    base: u64,
    generation: u64,
    delta: i64,
}

impl Pending {
    fn applied(&self) -> Result<u64> {
        let out = if self.delta >= 0 {
            self.base.checked_add(self.delta as u64)
        } else {
            self.base.checked_sub(self.delta.unsigned_abs())
        };
        out.ok_or_else(|| corrupt("live row count left its domain"))
    }
}

/// Where a bounded backfill has got to.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Backfill {
    collection: CollectionId,
    /// The last row this walk counted, or `None` before its first.
    after: Option<EntityId>,
    counted: u64,
}

/// What one [`Database::backfill_row_counts`] call did, and whether there is
/// more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowCountProgress {
    /// The collection the NEXT call resumes on, or `None` when there is none
    /// left.
    pub collection: Option<CollectionId>,
    /// Rows walked by this call.
    pub rows_walked: u64,
    /// Records this call wrote, each at the end of one collection's walk.
    pub records_written: u64,
    /// True when every live collection has a record.
    pub done: bool,
}

impl Database {
    /// True when this file declares the keyspace, so a record may exist.
    /// False is the whole gate for a database written before this feature:
    /// no probe, no read, no write.
    pub(crate) fn row_counts_declared(&self) -> bool {
        self.index_header
            .is_some_and(|h| h.features & ROW_COUNT_FEATURE != 0)
    }

    /// One collection's live count as the committed file plus this handle's
    /// own uncommitted transaction say it is, or `None` when the collection
    /// has no record and the answer is a walk.
    pub fn row_count(&self, c: CollectionId) -> Result<Option<u64>> {
        if let Some(pending) = self.row_counts.get(&c) {
            return pending.applied().map(Some);
        }
        if !self.row_counts_declared() {
            return Ok(None);
        }
        match self.store()?.get(&row_count_key(c))? {
            Some(b) => Ok(Some(decode(&b)?.rows)),
            None => Ok(None),
        }
    }

    /// The record itself, for verification and for the rebuild.
    pub(crate) fn row_count_record(&self, c: CollectionId) -> Result<Option<RowCountRecord>> {
        if !self.row_counts_declared() {
            return Ok(None);
        }
        self.store()?
            .get(&row_count_key(c))?
            .as_deref()
            .map(decode)
            .transpose()
    }

    /// Open this collection's arithmetic for the current transaction, reading
    /// the record once. `false` when the collection has no record, which is
    /// the one state in which nothing is maintained.
    fn touch_row_count(&mut self, c: CollectionId) -> Result<bool> {
        if self.row_counts.contains_key(&c) {
            return Ok(true);
        }
        // A DROPPING collection's record is deleted by the drop's last step,
        // so counting its rows away is arithmetic on a record that is on its
        // way out. Every reader and writer of that collection is already
        // refusing (`drop_collection.rs`), so nothing can observe the gap.
        if !self.row_counts_declared() || self.dropping == Some(c) {
            return Ok(false);
        }
        let Some(b) = self.store()?.get(&row_count_key(c))? else {
            return Ok(false);
        };
        let record = decode(&b)?;
        self.row_counts.insert(
            c,
            Pending {
                base: record.rows,
                generation: record.generation,
                delta: 0,
            },
        );
        Ok(true)
    }

    /// A row that did not exist now does. Called from the one place a NEW row
    /// key is written (`write_entity` with no old entity); a REPLACE writes
    /// the same key and changes no count, so it does not call this.
    pub(crate) fn note_row_inserted(&mut self, c: CollectionId) -> Result<()> {
        if !self.touch_row_count(c)? {
            return Ok(());
        }
        let pending = self.row_counts.get_mut(&c).expect("just touched");
        pending.delta = pending
            .delta
            .checked_add(1)
            .ok_or_else(|| invalid("live row count delta overflow"))?;
        Ok(())
    }

    /// A row that existed is gone. Called from `delete`, which is the one
    /// row-delete site outside the collection drop.
    pub(crate) fn note_row_removed(&mut self, id: EntityId) -> Result<()> {
        // A backfill walking this collection right now has already counted
        // every row at or below its cursor, so removing one of those makes
        // its running total one too high. A row ABOVE the cursor has not been
        // counted and the resumed walk will not find it. An INSERT needs no
        // such adjustment: sequences are monotone, so a new row always lands
        // above the cursor and the resumed walk counts it exactly once.
        if let Some(state) = self.row_count_backfill.as_mut() {
            if state.collection == id.collection
                && state
                    .after
                    .is_some_and(|after| id.sequence <= after.sequence)
            {
                state.counted = state.counted.saturating_sub(1);
            }
        }
        if !self.touch_row_count(id.collection)? {
            return Ok(());
        }
        let pending = self.row_counts.get_mut(&id.collection).expect("just touched");
        pending.delta = pending
            .delta
            .checked_sub(1)
            .ok_or_else(|| invalid("live row count delta overflow"))?;
        Ok(())
    }

    /// Write one tree put per touched collection, into the transaction the
    /// rows are already in. Called by `commit` before the barrier.
    ///
    /// A collection whose delta is zero -- every write was a REPLACE -- is
    /// not written at all, so a transaction that changed no count changes no
    /// byte here either.
    pub(crate) fn flush_row_counts(&mut self) -> Result<()> {
        if self.row_counts.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.row_counts);
        for (c, entry) in pending {
            if entry.delta == 0 {
                continue;
            }
            let record = RowCountRecord {
                rows: entry.applied()?,
                generation: entry.generation.saturating_add(1),
            };
            self.writer()?.put(&row_count_key(c), &encode(record))?;
        }
        Ok(())
    }

    /// Forget every uncommitted delta. Called by `rollback`, which discards
    /// the rows those deltas were counting.
    pub(crate) fn discard_row_counts(&mut self) {
        self.row_counts.clear();
        self.row_count_backfill = None;
    }

    /// Delete one collection's record. The drop's last step, after it has
    /// proved every keyspace of the collection empty.
    pub(crate) fn remove_row_count(&mut self, c: CollectionId) -> Result<()> {
        self.row_counts.remove(&c);
        if self
            .row_count_backfill
            .is_some_and(|state| state.collection == c)
        {
            self.row_count_backfill = None;
        }
        if !self.row_counts_declared() {
            return Ok(());
        }
        self.writer()?.delete(&row_count_key(c))?;
        Ok(())
    }

    /// Give a collection the record it does not have, from a count this
    /// caller computed. Used by `create_collection` (a fresh collection holds
    /// no rows, so the count is 0 and no walk is needed) and by the backfill
    /// at the end of one collection's walk.
    ///
    /// The feature bit rides the same transaction, which is what makes the
    /// bit and the first record one durable fact.
    pub(crate) fn seed_row_count(&mut self, c: CollectionId, rows: u64) -> Result<()> {
        self.enable_logical_feature(ROW_COUNT_FEATURE)?;
        self.writer()?.put(
            &row_count_key(c),
            &encode(RowCountRecord {
                rows,
                generation: 1,
            }),
        )?;
        self.row_counts.remove(&c);
        Ok(())
    }

    /// Build the live-row-count records for a database that has none, in
    /// bounded, resumable steps. ONE walk per collection, under the caller's
    /// budget; the record is written at the END of that collection's walk, so
    /// a record on disk is always a finished count and never a partial one.
    /// The feature bit rides the first record.
    ///
    /// Law 1: the call reads at most `budget` row keys and holds one cursor,
    /// so its memory is the caller's bound and not the collection's size. The
    /// CALLER commits, exactly as `delete_where` and `drop_collection_step`
    /// leave the commit to their caller; nothing is durable until it does,
    /// and a crash before that leaves the database as it was.
    ///
    /// A collection that already has a record is skipped -- the record is
    /// maintained by the write path from then on, so recomputing it would be
    /// work with nothing to show for it.
    pub fn backfill_row_counts(&mut self, budget: usize) -> Result<RowCountProgress> {
        self.ready_write()?;
        if budget == 0 {
            return Err(invalid("a row-count backfill budget counts 1.. rows"));
        }
        let result = (|| {
            let mut rows_walked = 0u64;
            let mut records_written = 0u64;
            loop {
                let state = match self.row_count_backfill {
                    Some(state) => state,
                    None => match self.next_uncounted_collection()? {
                        Some(c) => Backfill {
                            collection: c,
                            after: None,
                            counted: 0,
                        },
                        None => {
                            return Ok(RowCountProgress {
                                collection: None,
                                rows_walked,
                                records_written,
                                done: true,
                            })
                        }
                    },
                };
                let remaining = budget as u64 - rows_walked;
                let (counted, last, complete) =
                    self.walk_rows(state.collection, state.after, remaining as usize)?;
                rows_walked += counted;
                let state = Backfill {
                    collection: state.collection,
                    after: last.or(state.after),
                    counted: state.counted + counted,
                };
                if complete {
                    self.seed_row_count(state.collection, state.counted)?;
                    records_written += 1;
                    self.row_count_backfill = None;
                } else {
                    self.row_count_backfill = Some(state);
                }
                if rows_walked >= budget as u64 {
                    return Ok(RowCountProgress {
                        collection: self.row_count_backfill.map(|s| s.collection),
                        rows_walked,
                        records_written,
                        done: false,
                    });
                }
            }
        })();
        self.finish(result)
    }

    /// The lowest-numbered live collection with no record, or `None`.
    fn next_uncounted_collection(&self) -> Result<Option<CollectionId>> {
        for id in 1..self.header()?.0 {
            let c = CollectionId(id);
            let Ok(catalog) = self.catalog_any(c) else {
                continue;
            };
            if catalog.drop.is_some() {
                continue;
            }
            if self.row_count_record(c)?.is_none() {
                return Ok(Some(c));
            }
        }
        Ok(None)
    }

    /// At most `budget` row keys of one collection, from `after` exclusive.
    /// Returns what was counted, the last identity seen, and whether the
    /// collection's keyspace ended inside this budget.
    fn walk_rows(
        &self,
        c: CollectionId,
        after: Option<EntityId>,
        budget: usize,
    ) -> Result<(u64, Option<EntityId>, bool)> {
        let p = prefix(0x40, c);
        let start = match after {
            Some(id) => {
                let mut key = super::row_key(id);
                key.push(0);
                key
            }
            None => p.clone(),
        };
        let mut counted = 0u64;
        let mut last = None;
        for row in self.store()?.range(&start)? {
            let (key, _) = row?;
            if !has_prefix(&key, &p) {
                return Ok((counted, last, true));
            }
            last = Some(row_id_after_prefix(&key, p.len(), c)?);
            counted += 1;
            if counted as usize == budget {
                return Ok((counted, last, false));
            }
        }
        Ok((counted, last, true))
    }
}

/// The uncommitted map a `Database` carries. Declared here so the field on
/// `Database` names one type.
pub(crate) type PendingCounts = BTreeMap<CollectionId, Pending>;
