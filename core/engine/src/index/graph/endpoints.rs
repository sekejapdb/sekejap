//! ENDPOINT SETS: one key per DISTINCT entity that has at least one edge of a
//! given (context, edge type, direction).
//!
//! ## The question this keyspace answers
//!
//! `EXISTS (SELECT 1 FROM related r WHERE r.source = place._key)` asks which
//! entities have an outgoing `related` edge. The edge keyspace can answer it
//! -- [`Database::edge_endpoints`] walks `tag | collection` and, after the
//! first edge of an entity, SEEKS past the rest of that entity's run -- but
//! the walk is one descent per entity that has an edge, on EVERY statement:
//! 50,000 seeks over 150,000 edges on the 50K battery, 34.0 ms of a statement
//! whose PostgreSQL twin costs 19.2 ms.
//!
//! The set is a DERIVED representation of the same fact, filed the way the
//! question asks it: one posting per distinct entity, contiguous, in one
//! range. The read then walks `rows` keys and seeks once.
//!
//! ## The key
//!
//! ```text
//! 0x7E | context | edge type | direction | collection | sequence
//! ```
//!
//! with NO value. `context` and `edge type` are the width-tagged ordered
//! integer encoding `src/collections/mod.rs` uses everywhere; `direction` is
//! one byte, [`DIR_OUT`] for the source end and [`DIR_IN`] for the
//! destination end; `collection | sequence` is the entity, the same two
//! integers `append_entity` writes into an edge key. The entity's collection
//! is therefore a prefix INSIDE one (context, type, direction), which is what
//! makes the read one contiguous range: `edge_endpoints` is asked for one
//! collection's entities.
//!
//! THE TAG. `0x7E`, taken after auditing the registry
//! (`docs/core/FORMAT_V2.md` "Extension boundary" and
//! `core/engine/src/collections`): `0x00` header and layout replicas, `0x03`
//! `0x04` `0x05` catalog/registry/collection-index, `0x06` `0x07` graph
//! header and name descriptors, `0x10` `0x11` `0x12` collection names, index
//! names and graph name lookups, `0x20` external-key mappings, `0x40` entity
//! rows, `0x60` vector sidecars, `0x70` scalar entries, `0x71`/`0x72` the two
//! edge directions, `0x73` exact vector, `0x74` spatial point, `0x75`-`0x78`
//! text postings/norms/term stats/corpus stats, `0x79` quantized vector,
//! `0x7A`/`0x7B` text segments and norm blocks, `0x7C` geometry cells.
//! `0x7D` is the first free byte and is left free DELIBERATELY: a parallel
//! worker is adding a live row count under feature bit `0x2000`, one bit
//! below this module's, and taking the tag one above keeps the two from
//! colliding. `0x7E` is the tag this module takes.
//!
//! ## The feature bit
//!
//! [`ENDPOINT_FEATURE`] = `0x4000`, additive. A file that has never written an
//! endpoint key is byte-identical to one written before this module existed
//! and opens in every older binary. A file that HAS written one declares the
//! bit, and a binary whose mask predates it refuses the file whole at
//! admission as `Unsupported` -- not as damage (Law 8).
//!
//! ## When the bit is set, and why not before
//!
//! The bit rides the FIRST key, like every other additive bit here. But it
//! can only ride it when the set is KNOWN COMPLETE, and a set is complete
//! only if no edge predates it. So the write path asks one question before it
//! turns maintenance on: is the edge keyspace EMPTY? On a database that has
//! never held an edge the answer is yes, the first edge's two keys are the
//! whole set, and the bit rides them. On a database that already holds edges
//! -- one written by an older binary, or by this one before the bit existed
//! -- the answer is no, and the write path maintains NOTHING: half a set is
//! not a cheaper answer, it is a wrong one. That database keeps the walk it
//! has always taken until [`Database::backfill_endpoint_sets`] builds the set
//! once and sets the bit at the end of the pass.
//!
//! The probe is one range seek, and only while the bit is clear; the answer
//! is cached on the handle (`endpoint_state`) and discarded by a rollback,
//! which re-reads the durable header.
//!
//! ## What the write path costs
//!
//! * A NEW edge: two puts, one per end. An edge whose quadruple is already on
//!   disk writes none -- the forward/reverse probe `put_edge` already runs is
//!   what says which it is, so nothing is read that was not read before.
//!   `link_many` DEDUPLICATES within the batch, so a batch of `n` edges over
//!   `k` distinct ends pays `k` puts, not `2n`.
//! * A REMOVED edge (unlink, delete_edge, the cascade of an entity delete,
//!   and the CASCADE phase of a collection drop): one BOUNDED RANGE PROBE per
//!   end -- `range(edge tag | entity | context | type).next()`, one B-tree
//!   descent and one key comparison -- which answers "does another edge of
//!   this (entity, context, type, direction) remain?". If it does the key
//!   stays; if it does not the key is deleted. Two descents and at most two
//!   deletes per removed edge.
//!
//! ## Law 5
//!
//! `src/collections/verification.rs` compares this keyspace against an
//! independent walk of the edges -- a key with no edge behind it and an edge
//! with no key are each a named `Corrupt` finding -- and
//! `src/collections/rebuild.rs` recomputes it from the authoritative primary
//! edges rather than copying it.
use super::{
    edge_prefix, key_after_prefix, parse_edge_key, Direction, EdgeKey, EdgeTypeId, GraphContextId,
    GRAPH_FEATURE, PRIMARY_EDGE, REVERSE_EDGE,
};
use crate::collections::{
    corrupt, invalid, ordered_into, read_ordered, CollectionId, Database, EntityId, Error, Result,
};
use crate::store::pagewal::PageWalStore;
use std::collections::BTreeSet;

/// The keyspace tag. See the module documentation for the audit that freed it.
pub(crate) const ENDPOINT_ENTRY: u8 = 0x7E;

/// The additive logical feature bit that says this file carries endpoint sets.
///
/// `pub` because the compatibility fixtures and the older-binary refusal test
/// quote it beside `SUPPORTED_LOGICAL_FEATURES`.
pub const ENDPOINT_FEATURE: u64 = 0x4000;

/// The source end of an edge: the entity `r.source` names.
pub(crate) const DIR_OUT: u8 = 0;
/// The destination end: the entity `r.destination` names.
pub(crate) const DIR_IN: u8 = 1;

/// Room for a tag, two identities and a direction byte at their widest.
const ENDPOINT_KEY_BYTES: usize = 1 + 9 + 9 + 1 + 9 + 9;

/// How many distinct (context, type, direction) runs the drop's emptiness
/// probe seeks through before it gives up and says so. Matches
/// `drop_collection::RESTRICT_PROBE_SEEKS` in shape and in reason.
pub(crate) const DROP_PROBE_SEEKS: usize = 4096;

fn direction_byte(direction: Direction) -> Result<u8> {
    match direction {
        Direction::Outgoing => Ok(DIR_OUT),
        Direction::Incoming => Ok(DIR_IN),
        Direction::Both => Err(invalid(
            "an edge-endpoint set names one direction: a column of an edge table is either its source or its destination",
        )),
    }
}

/// `0x7E | context | type | direction`, the run one (context, type, direction)
/// owns.
pub(crate) fn endpoint_run(context: GraphContextId, edge_type: EdgeTypeId, dir: u8) -> Vec<u8> {
    let mut key = Vec::with_capacity(ENDPOINT_KEY_BYTES);
    key.push(ENDPOINT_ENTRY);
    ordered_into(&mut key, context.0);
    ordered_into(&mut key, edge_type.0);
    key.push(dir);
    key
}

/// The run above, narrowed to one collection: the prefix the read walks.
pub(crate) fn endpoint_collection_prefix(
    context: GraphContextId,
    edge_type: EdgeTypeId,
    dir: u8,
    collection: CollectionId,
) -> Vec<u8> {
    let mut key = endpoint_run(context, edge_type, dir);
    ordered_into(&mut key, u64::from(collection.0));
    key
}

/// One entity's key.
pub(crate) fn endpoint_key(
    context: GraphContextId,
    edge_type: EdgeTypeId,
    dir: u8,
    entity: EntityId,
) -> Vec<u8> {
    let mut key = endpoint_collection_prefix(context, edge_type, dir, entity.collection);
    ordered_into(&mut key, entity.sequence);
    key
}

/// The four fields of a stored key, read back.
pub(crate) fn parse_endpoint_key(key: &[u8]) -> Result<(GraphContextId, EdgeTypeId, u8, EntityId)> {
    if key.first() != Some(&ENDPOINT_ENTRY) {
        return Err(corrupt("endpoint key tag"));
    }
    let mut at = 1;
    let context = GraphContextId(read_ordered(key, &mut at)?);
    let edge_type = EdgeTypeId(read_ordered(key, &mut at)?);
    let dir = *key.get(at).ok_or_else(|| corrupt("endpoint key direction"))?;
    at += 1;
    if dir > DIR_IN {
        return Err(corrupt("endpoint key direction"));
    }
    let collection = u32::try_from(read_ordered(key, &mut at)?).map_err(corrupt)?;
    let sequence = read_ordered(key, &mut at)?;
    if at != key.len() || edge_type.0 == 0 || collection == 0 || sequence == 0 {
        return Err(corrupt("endpoint key fields"));
    }
    Ok((
        context,
        edge_type,
        dir,
        EntityId {
            collection: CollectionId(collection),
            sequence,
        },
    ))
}

/// The two (direction, near entity) pairs one edge files.
pub(crate) fn ends(key: EdgeKey) -> [(u8, EntityId); 2] {
    [(DIR_OUT, key.source), (DIR_IN, key.destination)]
}

/// One bounded pass of [`Database::backfill_endpoint_sets`].
#[derive(Clone, Copy, Debug, Default)]
pub struct EndpointProgress {
    /// True once the whole edge keyspace has been walked and the bit is set.
    pub done: bool,
    /// Edges this call looked at. Never more than the budget it was given.
    pub edges_seen: u64,
    /// Keys this call wrote. A key already present is written again rather
    /// than probed for: a probe and a put are the same descent.
    pub keys_written: u64,
    /// The totals since the pass began on this handle.
    pub total_edges_seen: u64,
    pub total_keys_written: u64,
}

/// The open-time probe: a `0x7E` key cannot exist in a file that does not
/// declare the bit. One range seek, the shape `validate_graph` uses for the
/// five graph tags.
pub(crate) fn validate_endpoint_sets(s: &PageWalStore, enabled: bool) -> Result<()> {
    if enabled {
        return Ok(());
    }
    if let Some(row) = s.range(&[ENDPOINT_ENTRY])?.next() {
        let (key, _) = row?;
        if key.first() == Some(&ENDPOINT_ENTRY) {
            return Err(corrupt(
                "graph endpoint sets exist without the endpoint feature",
            ));
        }
    }
    Ok(())
}

impl Database {
    /// Whether this file carries endpoint sets, so a read may answer from
    /// them and a write must maintain them.
    pub fn endpoint_sets_present(&self) -> bool {
        self.index_header
            .is_some_and(|h| h.features & ENDPOINT_FEATURE != 0)
    }

    /// Whether the write path about to run must maintain the sets, TURNING
    /// THE FEATURE ON if this file can start one complete.
    ///
    /// See the module documentation: a set is only safe to believe if no edge
    /// predates it, so the bit is set on the first edge of a file that holds
    /// none, and never on a file that already holds edges. The emptiness
    /// probe is one range seek and runs only while the bit is clear; its
    /// answer is cached on the handle.
    pub(crate) fn endpoint_maintenance(&mut self) -> Result<bool> {
        if self.endpoint_sets_present() {
            return Ok(true);
        }
        if let Some(known) = self.endpoint_state.get() {
            return Ok(known);
        }
        let empty = {
            let store = self.store()?;
            let mut empty = true;
            for tag in [PRIMARY_EDGE, REVERSE_EDGE] {
                if let Some(row) = store.range(&[tag])?.next() {
                    let (key, _) = row?;
                    if key.first() == Some(&tag) {
                        empty = false;
                        break;
                    }
                }
            }
            empty
        };
        if !empty {
            self.endpoint_state.set(Some(false));
            return Ok(false);
        }
        self.enable_logical_feature(ENDPOINT_FEATURE)?;
        self.endpoint_state.set(Some(true));
        Ok(true)
    }

    /// The keys one NEW edge adds, written into the open transaction.
    ///
    /// A key this transaction has already put is skipped: the put would
    /// change nothing, and the descent it costs is the one this module is
    /// measured on. See `Database::endpoint_written`.
    pub(crate) fn insert_endpoint_keys(&mut self, key: EdgeKey) -> Result<()> {
        for (dir, entity) in ends(key) {
            let k = endpoint_key(key.context, key.edge_type, dir, entity);
            if self.endpoint_written.contains(&k) {
                continue;
            }
            self.writer()?.put(&k, &[])?;
            if self.endpoint_written.len() < crate::collections::ENDPOINT_MEMO {
                self.endpoint_written.insert(k);
            }
        }
        Ok(())
    }

    /// Both ends of a REMOVED edge, each key taken only when that entity's
    /// LAST edge of its (context, type, direction) has gone.
    ///
    /// One bounded range probe per end: `range(tag | entity | context | type)`
    /// and one `next()`. That is one B-tree descent and one key comparison --
    /// it does not walk the entity's edges, it asks whether there is a first
    /// one. The edge itself must already be deleted from the transaction when
    /// this runs, or the probe finds the edge it is retiring.
    pub(crate) fn remove_endpoint_keys_if_last(&mut self, key: EdgeKey) -> Result<()> {
        self.remove_endpoint_keys_for(&[key])
    }

    /// The same for a batch of removed edges, with the repeats taken out: a
    /// cascade removes every edge of one entity, and probing that entity once
    /// per edge would be the walk this module exists to avoid.
    pub(crate) fn remove_endpoint_keys_for(&mut self, edges: &[EdgeKey]) -> Result<()> {
        if !self.endpoint_sets_present() {
            return Ok(());
        }
        let mut seen: BTreeSet<(u64, u64, u8, u32, u64)> = BTreeSet::new();
        for edge in edges {
            for (dir, entity) in ends(*edge) {
                if !seen.insert((
                    edge.context.0,
                    edge.edge_type.0,
                    dir,
                    entity.collection.0,
                    entity.sequence,
                )) {
                    continue;
                }
                let tag = if dir == DIR_OUT {
                    PRIMARY_EDGE
                } else {
                    REVERSE_EDGE
                };
                let probe = edge_prefix(tag, entity, Some(edge.context), Some(edge.edge_type));
                let mut remains = false;
                if let Some(row) = self.store()?.range(&probe)?.next() {
                    let (found, _) = row?;
                    remains = found.starts_with(&probe);
                }
                if !remains {
                    let k = endpoint_key(edge.context, edge.edge_type, dir, entity);
                    self.writer()?.delete(&k)?;
                    // It is no longer there, so a later insert in this same
                    // transaction must not skip its put.
                    self.endpoint_written.remove(&k);
                }
            }
        }
        Ok(())
    }

    /// Whether any `0x7E` key names a row of `c`, and whether the probe gave
    /// up before it could say. For the emptiness proof the collection drop
    /// runs before it removes the descriptor.
    ///
    /// The collection is not a prefix of the WHOLE keyspace -- a key is filed
    /// by (context, type, direction) first -- so this seeks per RUN: it reads
    /// one key, takes the run it belongs to, probes that run's `| collection`
    /// prefix, then seeks to the first key of the next run. One descent per
    /// distinct (context, type, direction) present, which is bounded by the
    /// graph's name dictionary and not by its edges.
    pub(crate) fn collection_has_endpoint_key(&self, c: CollectionId) -> Result<(bool, bool)> {
        if !self.endpoint_sets_present() {
            return Ok((false, false));
        }
        let store = self.store()?;
        let mut from = vec![ENDPOINT_ENTRY];
        let mut seeks = 0usize;
        loop {
            if seeks >= DROP_PROBE_SEEKS {
                return Ok((false, true));
            }
            seeks += 1;
            let Some(row) = store.range(&from)?.next() else {
                return Ok((false, false));
            };
            let (key, _) = row?;
            if key.first() != Some(&ENDPOINT_ENTRY) {
                return Ok((false, false));
            }
            let (context, edge_type, dir, _) = parse_endpoint_key(&key)?;
            let narrowed = endpoint_collection_prefix(context, edge_type, dir, c);
            if let Some(row) = store.range(&narrowed)?.next() {
                let (found, _) = row?;
                if found.starts_with(&narrowed) {
                    return Ok((true, false));
                }
            }
            let run = endpoint_run(context, edge_type, dir);
            let Some(next) = key_after_prefix(&run) else {
                return Ok((false, false));
            };
            from = next;
        }
    }

    /// Build the endpoint sets of a database whose edges predate them, in
    /// bounded steps, and set the feature bit when the last edge is in.
    ///
    /// `budget` is edges per call. Each call returns what it did; the pass is
    /// over when `done` is true. Nothing here commits -- the caller decides
    /// when, exactly as it does for `build_index_to_ready` -- and the bit is
    /// written in the SAME transaction as the step that completes the pass,
    /// so a crash before that commit leaves a file whose bit is clear and
    /// whose partial keys are invisible to every reader: the next open takes
    /// the walk, and the next backfill writes them again. Each write is a put
    /// of a key that may already be there, which is idempotent.
    ///
    /// L1: the pass is proportional to the EDGES, once, instead of to the
    /// statements that would each otherwise walk them.
    pub fn backfill_endpoint_sets(&mut self, budget: usize) -> Result<EndpointProgress> {
        self.ready_write()?;
        if self.endpoint_sets_present() {
            return Ok(EndpointProgress {
                done: true,
                ..Default::default()
            });
        }
        if !self
            .index_header
            .is_some_and(|h| h.features & GRAPH_FEATURE != 0)
        {
            // No graph, no edges, and therefore a complete (empty) set. The
            // bit is NOT set: there is no key to ride it, and a file that
            // holds no endpoint key must stay readable by an older binary.
            return Ok(EndpointProgress {
                done: true,
                ..Default::default()
            });
        }
        let budget = budget.max(1);
        let h = self.graph_header()?;
        let mut progress = EndpointProgress::default();
        let result = (|| {
            let start = self
                .endpoint_backfill_at
                .clone()
                .unwrap_or_else(|| vec![PRIMARY_EDGE]);
            let mut writes: Vec<Vec<u8>> = Vec::new();
            let mut next_from: Option<Vec<u8>> = None;
            let mut seen = 0u64;
            {
                let store = self.store()?;
                for row in store.range(&start)? {
                    let (key, _) = row?;
                    if key.first() != Some(&PRIMARY_EDGE) {
                        break;
                    }
                    let edge = parse_edge_key(&key, PRIMARY_EDGE)?;
                    if edge.edge_type.0 == 0
                        || edge.edge_type.0 >= h.next_type
                        || edge.context.0 >= h.next_context
                    {
                        return Err(corrupt("stored edge has unknown type/context identity"));
                    }
                    for (dir, entity) in ends(edge) {
                        writes.push(endpoint_key(edge.context, edge.edge_type, dir, entity));
                    }
                    seen += 1;
                    if seen as usize >= budget {
                        // The first key strictly after the one just read.
                        let mut after = key.clone();
                        after.push(0);
                        next_from = Some(after);
                        break;
                    }
                }
            }
            // Ascending, each distinct key once: a run of edges out of one
            // entity writes that entity's key once, and the puts walk the
            // keyspace forwards instead of jumping about in it.
            writes.sort_unstable();
            writes.dedup();
            let written = writes.len() as u64;
            for key in &writes {
                self.writer()?.put(key, &[])?;
            }
            progress.edges_seen = seen;
            progress.keys_written = written;
            match next_from {
                Some(from) => {
                    self.endpoint_backfill_at = Some(from);
                    progress.done = false;
                }
                None => {
                    // The last edge is in. The bit rides this transaction, so
                    // the set and the declaration are published together.
                    self.enable_logical_feature(ENDPOINT_FEATURE)?;
                    self.endpoint_state.set(Some(true));
                    self.endpoint_backfill_at = None;
                    progress.done = true;
                }
            }
            Ok(())
        })();
        self.finish(result)?;
        self.endpoint_backfill_seen += progress.edges_seen;
        self.endpoint_backfill_written += progress.keys_written;
        progress.total_edges_seen = self.endpoint_backfill_seen;
        progress.total_keys_written = self.endpoint_backfill_written;
        if progress.done {
            self.endpoint_backfill_seen = 0;
            self.endpoint_backfill_written = 0;
        }
        Ok(progress)
    }

    /// The read: every entity of `collection` with at least one edge of
    /// `(context, edge_type, direction)`, taken from the endpoint keyspace.
    ///
    /// One contiguous range, one posting per entity, no seek over an edge.
    /// `GraphVisited` is charged per KEY, which is per returned id -- the
    /// resource the walk actually spends.
    pub(crate) fn endpoints_from_set<C: FnMut() -> bool>(
        &self,
        collection: CollectionId,
        context: GraphContextId,
        edge_type: EdgeTypeId,
        direction: Direction,
        max_ids: usize,
        budget: crate::query::QueryBudget,
        cancelled: &mut C,
    ) -> Result<Vec<EntityId>> {
        let meter = &mut crate::query::WorkMeter::new(budget, cancelled);
        let dir = direction_byte(direction)?;
        let prefix = endpoint_collection_prefix(context, edge_type, dir, collection);
        let mut out: Vec<EntityId> = Vec::new();
        for row in self.store()?.range(&prefix)? {
            meter.check_cancelled()?;
            let (key, value) = row?;
            if !key.starts_with(&prefix) {
                break;
            }
            if !value.is_empty() {
                return Err(corrupt("endpoint key carries a value"));
            }
            let mut at = prefix.len();
            let sequence = read_ordered(&key, &mut at)?;
            if at != key.len() || sequence == 0 {
                return Err(corrupt("endpoint key fields"));
            }
            meter.charge(crate::query::WorkResource::GraphVisited, 1)?;
            out.push(EntityId {
                collection,
                sequence,
            });
            if out.len() > max_ids {
                return Err(Error::BudgetExceeded {
                    resource: crate::query::WorkResource::GraphVisited,
                    limit: max_ids as u64,
                    attempted: out.len() as u64,
                });
            }
        }
        // The keyspace hands them over in ascending sequence inside one
        // collection, which is the order the caller's `QueryFilter::Ids`
        // wants.
        debug_assert!(out.windows(2).all(|w| w[0].sequence < w[1].sequence));
        Ok(out)
    }
}
