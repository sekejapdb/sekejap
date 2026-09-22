//! Bounded, resumable writes over a CANDIDATE SET, and the bulk write scope.
//!
//! Two atomics and a counter:
//!
//! * [`Database::delete_where`] -- every row an ordinary prepared query
//!   matches, deleted by key, with GRAPH_CONTRACT 6.1's RESTRICT preflight
//!   per row;
//! * [`Database::update_where`] -- the same candidates, read-modify-put
//!   through an [`UpdatePatch`];
//! * [`Database::begin_bulk`] / [`Database::end_bulk`] --
//!   `docs/dist/OPS_CONTRACT.md` §7's nesting-counted scope whose OUTERMOST
//!   close calls `commit`.
//!
//! ## Where the candidates come from
//!
//! An ordinary `prepare_query` with `Projection::Ids`, `QueryOrder::Driver`
//! and the caller's `CandidateDriver` (`Auto` unless it says otherwise).
//! Nothing here is a second planner: the same driver selection, the same
//! membership sets and the same filters answer a `DELETE ... WHERE` that
//! answer the `SELECT ... WHERE` with the same predicate, so the work is
//! proportional to the ROWS MATCHED and not to the collection, which is the
//! whole of `docs/lang/QL_CONTRACT.md` §2's promise for these two rows.
//!
//! `QueryOrder::Driver` and not `EntityId` on purpose. Driver order is the
//! one order in which EVERY driver walks in rank order
//! (`page.rs::driver_walks_in_rank_order`), so every page of this pass stops
//! when it is full and the page after it OPENS at the key the last one
//! stopped on. Under an id ranking a scalar RANGE driver walks by value while
//! ranking by id, which cannot resume -- and a write pass resumes between
//! every page by construction, because the page in between needs
//! `&mut Database` and a prepared query holds `&Database`.
//!
//! ## Law 3 -- the page is materialised before anything is written
//!
//! One page of candidates is walked to completion and turned into a `Vec` of
//! [`EntityId`] BEFORE the first write of that page. The walk is therefore
//! finished before the tree it walked is touched: a write cannot move the
//! cursor that found it, split the leaf it is standing on, or delete the
//! posting it is about to read. The `Vec` is bounded by
//! [`MAX_WRITE_BATCH`] rows, so it is the page's own bound and never the
//! collection's.
//!
//! ## What is committed, and by whom
//!
//! **Nothing here commits.** `delete_where` and `update_where` write into the
//! working tree exactly as `put` and `delete` do, and the caller's
//! transaction decides their fate: one `commit` makes the whole pass durable,
//! one `rollback` discards it. That is stated rather than implied because the
//! pass is bounded and resumable, and a reader could reasonably expect a
//! bounded step to commit the way `drop_collection_step` does. It does not.
//! A caller that wants a long pass to land in pieces commits between calls --
//! which is also what makes the committed prefix that
//! `WriteProgress::cursor` resumes from.
//!
//! ## The budget
//!
//! [`QueryBudget::rows_written`] bounds the pass. Every page is sized to what
//! is LEFT of it, so the pass writes at most the budget and stops with
//! `done == false` and a cursor, rather than writing past it and reporting
//! the overrun afterwards. Every other dimension of the budget bounds the
//! candidate WALK, charged by the pages the way any query's pages charge it.
use super::*;
use crate::index::graph::GraphContextId;
use crate::query::{
    CandidateDriver, Projection, QueryBudget, QueryDriver, QueryError, QueryFilter, QueryOrder,
    QueryRequest, QueryResult, QueryWork, WriteCursor,
};
use std::cell::RefCell;

/// The widest page of candidates one write pass materialises at once, in
/// rows. The same 256 `MAX_DROP_BATCH` uses: a bounded write step's
/// transaction is the same page-WAL allowance a drop step's is, and the id
/// vector this holds is 16 bytes a row.
pub const MAX_WRITE_BATCH: usize = 256;

/// How many columns one [`UpdatePatch`] may set. The same ceiling
/// `MAX_PROJECTION_FIELDS` puts on the fields one query names, for the same
/// reason: a row has a declared layout and a patch over more slots than that
/// is not a patch.
pub const MAX_PATCH_COLUMNS: usize = 64;

/// How many (entity, context) runs the per-row RESTRICT probe seeks through
/// before it reports what it has. GRAPH_CONTRACT 6.1 at ROW granularity;
/// smaller than the collection probe's 1,024 because it is paid once per
/// candidate rather than once per drop.
pub const RESTRICT_ROW_PROBE_SEEKS: usize = 256;

/// What a predicated DELETE does about the graph edges incident on a row it
/// is about to remove. GRAPH_CONTRACT 6.1: RESTRICT is the default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteMode {
    /// Refuse the whole pass while any edge in any context references the
    /// candidate row, naming those contexts. Nothing of the pass is written:
    /// the preflight runs before the row's first mutation, and the rows
    /// written before it are the caller's uncommitted transaction to keep or
    /// to roll back.
    Restrict,
    /// Remove each row's incident edges in every context through the bounded
    /// cascade `Database::delete` already runs. The explicit opt-in Law 3
    /// asks for: a delete that silently removed edges from another graph
    /// would be a fallible delete.
    Cascade,
}

/// One column's new value in an [`UpdatePatch`].
pub enum PatchValue<'a> {
    /// The same value for every row: a SQL literal or a bound parameter,
    /// already folded.
    Literal(Value),
    /// A ROW FUNCTION: one row's document in, that column's new value out.
    ///
    /// The closure boundary `docs/lang/QL_CONTRACT.md` §4.1 / §4.2 describes.
    /// The language layer compiles `SET n = n + 1`, `SET s = lower(s)` or
    /// `SET d = date_trunc('day', d)` into it; this crate applies it and
    /// knows nothing about the expression. It reads ONE row -- the row being
    /// written -- so the cost stays proportional to the rows matched, and an
    /// expression that would read another row has no closure to be.
    ///
    /// `RefCell` because the brief's signature takes `&UpdatePatch` while a
    /// compiled expression is a `&mut dyn FnMut`: the patch is shared across
    /// every row of the pass and the closure is called once per row, one at a
    /// time.
    Row(RefCell<&'a mut dyn FnMut(&Value) -> Result<Value>>),
}

/// `SET column = ...`, one entry per column, applied to a row that already
/// exists.
///
/// Every expression reads the row as it was BEFORE this patch: the new values
/// are all computed from the original document and only then written into it,
/// so `SET a = b, b = a` swaps rather than copying, exactly as SQL says.
pub struct UpdatePatch<'a> {
    columns: Vec<(String, PatchValue<'a>)>,
}

impl Default for UpdatePatch<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> UpdatePatch<'a> {
    pub fn new() -> Self {
        Self {
            columns: Vec::new(),
        }
    }

    /// `SET column = <value>`.
    pub fn set(&mut self, column: &str, value: Value) -> Result<()> {
        self.push(column, PatchValue::Literal(value))
    }

    /// `SET column = <row expression over the same row>`.
    pub fn set_row(
        &mut self,
        column: &str,
        f: &'a mut dyn FnMut(&Value) -> Result<Value>,
    ) -> Result<()> {
        self.push(column, PatchValue::Row(RefCell::new(f)))
    }

    fn push(&mut self, column: &str, value: PatchValue<'a>) -> Result<()> {
        if column.is_empty() || column.len() > 1024 {
            return Err(invalid("patched column must contain 1..1024 UTF-8 bytes"));
        }
        // The same names `validate_document` keeps a caller out of: the
        // external key is the row's identity and the managed timestamps are
        // the engine's.
        if reserved(column) || column == KEY_FIELD || matches!(column, CREATED | UPDATED) {
            return Err(invalid(format!(
                "`{column}` is a managed/reserved field and is not patchable"
            )));
        }
        if self.columns.iter().any(|(existing, _)| existing == column) {
            return Err(invalid(format!("`{column}` is set twice by one patch")));
        }
        if self.columns.len() >= MAX_PATCH_COLUMNS {
            return Err(invalid(format!(
                "a patch sets at most {MAX_PATCH_COLUMNS} columns"
            )));
        }
        self.columns.push((column.to_owned(), value));
        Ok(())
    }

    /// The columns this patch writes, in the order they were set. The UPDATE
    /// preflight reads it to decide whether a write would move the walk that
    /// found the row, and `EXPLAIN` prints it.
    pub fn columns(&self) -> Vec<&str> {
        self.columns.iter().map(|(name, _)| name.as_str()).collect()
    }

    /// True when this patch writes `field`.
    pub fn writes(&self, field: &str) -> bool {
        self.columns.iter().any(|(name, _)| name == field)
    }

    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// The row this patch makes of `old`. Every expression sees `old`.
    fn apply(&self, old: &Value) -> Result<Value> {
        let mut new = old.clone();
        let object = new
            .as_object_mut()
            .ok_or_else(|| corrupt("stored row is not an object"))?;
        for (column, value) in &self.columns {
            let next = match value {
                PatchValue::Literal(value) => value.clone(),
                PatchValue::Row(f) => (f
                    .try_borrow_mut()
                    .map_err(|_| invalid("a patch expression re-entered its own patch"))?)(
                    old
                )?,
            };
            object.insert(column.clone(), next);
        }
        Ok(new)
    }
}

/// What a bounded write pass does to each candidate row.
pub enum WriteAction<'a, 'p> {
    Delete(DeleteMode),
    Update(&'a UpdatePatch<'p>),
}

impl WriteAction<'_, '_> {
    /// `delete` or `update`, for a refusal and an `EXPLAIN` line.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Delete(_) => "delete",
            Self::Update(_) => "update",
        }
    }
}

/// One bounded write pass, as a caller names it.
pub struct WriteRequest<'a, 'p> {
    pub collection: CollectionId,
    pub filters: &'a [QueryFilter<'a>],
    pub action: WriteAction<'a, 'p>,
    /// Where the candidates come from. `Auto` is what the two named entry
    /// points ask for; an explicit driver is how a caller takes
    /// `CandidateDriver::Entities` when the planner refuses to ride an index
    /// the patch would move (see [`Database::update_where`]).
    pub driver: CandidateDriver,
    /// Where to continue. [`WriteCursor::start`] for a fresh pass.
    pub after: WriteCursor,
}

/// What one bounded write pass did, and where the next one continues.
#[derive(Clone, Debug, PartialEq)]
pub struct WriteProgress {
    /// True when the candidate stream is exhausted: every matching row has
    /// been written and `cursor` resumes nothing.
    pub done: bool,
    /// Rows this pass wrote -- one per row deleted or put back.
    pub rows_written: u64,
    /// The rank key of the LAST row written, so the next call continues
    /// instead of starting over. Valid against the same collection, the same
    /// filters and the same catalog.
    pub cursor: WriteCursor,
    /// The driver the candidate walk used, as every page reports it.
    pub driver: QueryDriver,
    /// The work every page of this pass charged, summed, with
    /// `rows_written` the count above.
    pub work: QueryWork,
}

/// The budget one page of the candidate walk may still spend.
fn remaining(budget: &QueryBudget, used: &QueryWork) -> QueryBudget {
    let left = |limit: u64, spent: u64| limit.saturating_sub(spent);
    QueryBudget {
        candidates: left(budget.candidates, used.candidates),
        primary_reads: left(budget.primary_reads, used.primary_reads),
        scalar_postings: left(budget.scalar_postings, used.scalar_postings),
        graph_edges: left(budget.graph_edges, used.graph_edges),
        graph_visited: left(budget.graph_visited, used.graph_visited),
        spatial_postings: left(budget.spatial_postings, used.spatial_postings),
        text_postings: left(budget.text_postings, used.text_postings),
        text_tokens: left(budget.text_tokens, used.text_tokens),
        vector_locators: left(budget.vector_locators, used.vector_locators),
        vector_sidecars: left(budget.vector_sidecars, used.vector_sidecars),
        vector_lanes: left(budget.vector_lanes, used.vector_lanes),
        key_postings: left(budget.key_postings, used.key_postings),
        // The pages do not write; the pass does, and it charges this itself.
        rows_written: budget.rows_written,
        groups: budget.groups,
        output_bytes: left(budget.output_bytes, used.output_bytes),
        // A deadline is an instant, not an amount: every page keeps the
        // statement's own.
        deadline: budget.deadline,
    }
}

/// A refusal raised once the pass has ALREADY written rows of its own.
///
/// e4 has one transaction per handle and no savepoint, so a pass cannot undo
/// only its own rows: `rollback` discards everything uncommitted on the
/// handle, the caller's earlier statements included, which is a destructive
/// act this pass has no right to take on the caller's behalf (Law 3). What it
/// owes instead is the TRUTH -- a refusal that arrives after the pass has
/// written rows must SAY those rows are there, or the caller reads "refused",
/// commits the next statement, and publishes them by accident.
///
/// `docs/lang/QL_CONTRACT.md` §2 states the rule; this is where it is
/// enforced, because `used.rows_written` is known here and nowhere above.
///
/// A named budget and a cancellation keep their machine-readable shape
/// (`WorkResource`, the limit, the attempt) rather than being flattened into
/// prose: a caller that set a budget already knows a bounded pass stops where
/// its budget stops, and the `rows_written` dimension -- the one refusal that
/// is ABOUT the rows written -- is raised by the caller, which holds the
/// count in `WriteProgress::rows_written`.
fn refusal_names_the_rows_already_written(error: QueryError, rows_written: u64) -> QueryError {
    if rows_written == 0 {
        return error;
    }
    let pending = format!(
        " [{rows_written} row(s) of this pass are ALREADY WRITTEN and UNCOMMITTED: e4 has one transaction per handle and no savepoint, so the pass cannot undo only its own rows -- `rollback` discards them together with everything else uncommitted on this handle, and `commit` publishes them.]"
    );
    let with = |message: String| format!("{message}{pending}");
    match error {
        QueryError::Database(Error::InvalidInput(message)) => {
            QueryError::Database(Error::InvalidInput(with(message)))
        }
        QueryError::Database(Error::Unsupported(message)) => {
            QueryError::Database(Error::Unsupported(with(message)))
        }
        QueryError::Database(Error::Corrupt(message)) => {
            QueryError::Database(Error::Corrupt(with(message)))
        }
        other => other,
    }
}

/// Add one page's charges to the pass's running total. `groups` and
/// `membership_bytes` are high-water marks, not running totals, so they are
/// maxed for the same reason their budgets bound simultaneous memory.
fn add(used: &mut QueryWork, page: &QueryWork) {
    used.candidates += page.candidates;
    used.primary_reads += page.primary_reads;
    used.row_decodes += page.row_decodes;
    used.scalar_postings += page.scalar_postings;
    used.graph_edges += page.graph_edges;
    used.graph_visited += page.graph_visited;
    used.spatial_postings += page.spatial_postings;
    used.text_postings += page.text_postings;
    used.text_tokens += page.text_tokens;
    used.vector_locators += page.vector_locators;
    used.vector_sidecars += page.vector_sidecars;
    used.vector_lanes += page.vector_lanes;
    used.key_postings += page.key_postings;
    used.groups = used.groups.max(page.groups);
    used.membership_bytes = used.membership_bytes.max(page.membership_bytes);
    used.output_bytes += page.output_bytes;
}

impl Database {
    /// Delete every row of `c` that `filters` match, bounded by
    /// `budget.rows_written` and resumable from
    /// [`WriteProgress::cursor`].
    ///
    /// GRAPH_CONTRACT 6.1: [`DeleteMode::Restrict`], so a row with edges in
    /// any context refuses the pass and names the contexts. CASCADE is
    /// [`Database::write_where`] with [`DeleteMode::Cascade`].
    ///
    /// Commits nothing. See the module documentation.
    pub fn delete_where(
        &mut self,
        c: CollectionId,
        filters: &[QueryFilter<'_>],
        budget: QueryBudget,
    ) -> QueryResult<WriteProgress> {
        self.write_where(
            WriteRequest {
                collection: c,
                filters,
                action: WriteAction::Delete(DeleteMode::Restrict),
                driver: CandidateDriver::Auto,
                after: WriteCursor::start(),
            },
            budget,
        )
    }

    /// [`Database::delete_where`], continued where a previous pass stopped.
    pub fn delete_where_after(
        &mut self,
        c: CollectionId,
        filters: &[QueryFilter<'_>],
        budget: QueryBudget,
        after: &WriteCursor,
    ) -> QueryResult<WriteProgress> {
        self.write_where(
            WriteRequest {
                collection: c,
                filters,
                action: WriteAction::Delete(DeleteMode::Restrict),
                driver: CandidateDriver::Auto,
                after: after.clone(),
            },
            budget,
        )
    }

    /// Read-modify-put every row of `c` that `filters` match, bounded by
    /// `budget.rows_written` and resumable from [`WriteProgress::cursor`].
    ///
    /// Refused when the driver the planner chose walks a key the patch would
    /// MOVE -- a scalar, point or geometry index over a column the patch sets.
    /// Writing such a row re-files its posting, and a posting that moves
    /// forward is a candidate the pass would meet again and update twice. The
    /// refusal names the index and the column, and the remedy is
    /// [`Database::write_where`] with an explicit
    /// [`CandidateDriver::Entities`], which walks the primary tree in entity
    /// id order -- an id a write does not move. That is a SCAN, and
    /// `docs/lang/QL_CONTRACT.md` §6 does not allow one to be taken silently,
    /// which is exactly why it is the caller's word and not a fallback.
    ///
    /// Commits nothing. See the module documentation.
    pub fn update_where(
        &mut self,
        c: CollectionId,
        filters: &[QueryFilter<'_>],
        patch: &UpdatePatch<'_>,
        budget: QueryBudget,
    ) -> QueryResult<WriteProgress> {
        self.write_where(
            WriteRequest {
                collection: c,
                filters,
                action: WriteAction::Update(patch),
                driver: CandidateDriver::Auto,
                after: WriteCursor::start(),
            },
            budget,
        )
    }

    /// [`Database::update_where`], continued where a previous pass stopped.
    pub fn update_where_after(
        &mut self,
        c: CollectionId,
        filters: &[QueryFilter<'_>],
        patch: &UpdatePatch<'_>,
        budget: QueryBudget,
        after: &WriteCursor,
    ) -> QueryResult<WriteProgress> {
        self.write_where(
            WriteRequest {
                collection: c,
                filters,
                action: WriteAction::Update(patch),
                driver: CandidateDriver::Auto,
                after: after.clone(),
            },
            budget,
        )
    }

    /// The atomic behind both: page the candidates, materialise each page as
    /// ids, then write that page.
    pub fn write_where(
        &mut self,
        request: WriteRequest<'_, '_>,
        budget: QueryBudget,
    ) -> QueryResult<WriteProgress> {
        let WriteRequest {
            collection,
            filters,
            action,
            driver,
            after,
        } = request;
        self.ready_write().map_err(QueryError::Database)?;
        if let WriteAction::Update(patch) = &action {
            if patch.is_empty() {
                return Err(QueryError::Database(invalid(
                    "an UPDATE sets at least one column",
                )));
            }
        }
        let mut used = QueryWork::default();
        let mut cursor = after;
        let mut reported = None;
        // The pages run behind this call so that a refusal raised once rows
        // are already written can NAME them: `used.rows_written` is known
        // here and nowhere above, and a caller told only "refused" would
        // commit those rows on its next statement.
        let done = match self.write_pages(
            collection,
            filters,
            &action,
            driver,
            &budget,
            &mut used,
            &mut cursor,
            &mut reported,
        ) {
            Ok(done) => done,
            Err(error) => {
                return Err(refusal_names_the_rows_already_written(
                    error,
                    used.rows_written,
                ))
            }
        };
        Ok(WriteProgress {
            done,
            rows_written: used.rows_written,
            cursor,
            // A pass that wrote nothing because its budget was zero never
            // prepared a query, and the entity cursor is what an empty walk
            // would have used.
            driver: reported.unwrap_or(QueryDriver::Entities),
            work: used,
        })
    }

    /// The page loop of one write pass: walk a page of candidates, materialise
    /// it as ids, write it, and repeat until the stream or the budget ends.
    ///
    /// Split out of [`Database::write_where`] so that `used` -- and with it
    /// the count of rows this pass has already written -- survives a refusal
    /// raised part way through.
    #[allow(clippy::too_many_arguments)]
    fn write_pages(
        &mut self,
        collection: CollectionId,
        filters: &[QueryFilter<'_>],
        action: &WriteAction<'_, '_>,
        driver: CandidateDriver,
        budget: &QueryBudget,
        used: &mut QueryWork,
        cursor: &mut WriteCursor,
        reported: &mut Option<QueryDriver>,
    ) -> QueryResult<bool> {
        loop {
            let allowance = budget.rows_written.saturating_sub(used.rows_written);
            if allowance == 0 {
                return Ok(false);
            }
            let page_size = allowance.min(MAX_WRITE_BATCH as u64) as usize;
            // ── Law 3: the whole page is walked and materialised as ids
            // before a single byte of it is written. The prepared query holds
            // `&Database`; it is dropped at the end of this block, which is
            // also what lets the writes below take `&mut Database`.
            let (ids, page_done, next_cursor, page_driver, page_work) = {
                let mut prepared = self.prepare_query(QueryRequest {
                    collection,
                    filters,
                    order: QueryOrder::Driver,
                    projection: Projection::Ids,
                    total_limit: None,
                    driver,
                })?;
                prepared.resume_from(cursor);
                let plan = prepared.describe();
                if let WriteAction::Update(patch) = action {
                    self.refuse_a_patch_that_moves_its_driver(plan.driver, patch)?;
                }
                let page = prepared.next_page(page_size, remaining(budget, used), || false)?;
                let ids: Vec<EntityId> = page.rows.iter().map(|row| row.id).collect();
                (
                    ids,
                    page.done,
                    prepared.write_cursor(),
                    page.driver,
                    page.work,
                )
            };
            add(used, &page_work);
            *reported = Some(page_driver);
            if ids.is_empty() {
                return Ok(true);
            }
            for id in &ids {
                let wrote = match action {
                    WriteAction::Delete(mode) => self
                        .delete_candidate(collection, *id, *mode)
                        .map_err(QueryError::Database)?,
                    WriteAction::Update(patch) => self
                        .update_candidate(collection, *id, patch)
                        .map_err(QueryError::Database)?,
                };
                // A candidate whose row is already gone is not an error and
                // is not a written row: the driver may hold a posting for a
                // row this same uncommitted transaction removed.
                if wrote {
                    used.rows_written += 1;
                }
            }
            *cursor = next_cursor;
            if page_done {
                return Ok(true);
            }
        }
    }

    /// Refuse an UPDATE whose driving index is over a column the patch sets.
    fn refuse_a_patch_that_moves_its_driver(
        &self,
        driver: QueryDriver,
        patch: &UpdatePatch<'_>,
    ) -> QueryResult<()> {
        // Only the drivers whose walk key carries a VALUE can be moved by a
        // write. `Entities`, `Keys`, `Text`, `Graph` and `Membership` all
        // walk a key derived from the entity id or the external key, and a
        // read-modify-put changes neither (`drivers::driver_key`).
        let index = match driver {
            QueryDriver::Scalar(index)
            | QueryDriver::Spatial { index, .. }
            | QueryDriver::Geometry { index, .. }
            | QueryDriver::Nearest { index } => index,
            QueryDriver::Entities
            | QueryDriver::Keys
            | QueryDriver::Text(_)
            | QueryDriver::ExactVector(_)
            | QueryDriver::QuantizedVector(_)
            // A vamana node's key is its entity sequence, so a write cannot
            // re-file it. Linking DOES rewrite the neighbours' records, but
            // it never moves one to another key, so the walk still meets
            // every sequence exactly once.
            | QueryDriver::VamanaVector(_)
            | QueryDriver::Graph { .. }
            | QueryDriver::Membership { .. } => return Ok(()),
        };
        let info = self.index_info_cached(index).map_err(QueryError::Database)?;
        if !patch.writes(&info.field) {
            return Ok(());
        }
        Err(QueryError::Database(invalid(format!(
            "UPDATE ... SET `{}`: the candidates ride index `{}`, which is over `{}`, so writing it would re-file the posting the walk is standing on and the pass could meet the same row twice. Drive the pass explicitly with `CandidateDriver::Entities` (a SCAN of the collection in entity-id order, which a write does not move), or narrow the WHERE with a predicate on a column the patch leaves alone.",
            info.field, info.name, info.field
        ))))
    }

    /// One candidate deleted by id, with the per-row RESTRICT preflight.
    ///
    /// `Database::delete` by key would re-read the mapping entry the driver
    /// has already proved; this is the same body starting from the id the
    /// page materialised.
    fn delete_candidate(
        &mut self,
        c: CollectionId,
        id: EntityId,
        mode: DeleteMode,
    ) -> Result<bool> {
        self.user_write()?;
        let Some(e) = self.load_candidate(c, id, false)? else {
            return Ok(false);
        };
        if mode == DeleteMode::Restrict {
            let (contexts, truncated) = self.entity_edge_contexts(id, RESTRICT_ROW_PROBE_SEEKS)?;
            if !contexts.is_empty() {
                return Err(self.restrict_refusal(&e.entity.key, &contexts, truncated));
            }
        }
        let result = (|| {
            self.cascade_graph_delete(id)?;
            self.maintain_indexes(id, Some(&e.entity.document), None, None)?;
            for (field, _) in e.vectors {
                self.writer()?.delete(&vector_key(id, field))?;
            }
            self.writer()?.delete(&row_key(id))?;
            self.writer()?.delete(&mapping_key(c, &e.entity.key))?;
            self.note_deleted(id);
            self.note_row_removed(id)?;
            Ok(true)
        })();
        self.finish(result)
    }

    /// The GRAPH_CONTRACT 6.1 refusal, with every context named.
    fn restrict_refusal(&self, key: &str, contexts: &[GraphContextId], truncated: bool) -> Error {
        let mut named = Vec::with_capacity(contexts.len());
        for context in contexts {
            match self.graph_context_name(*context) {
                Ok(name) => named.push(format!("`{name}`")),
                Err(e) => return e,
            }
        }
        invalid(format!(
            "RESTRICT: row `{key}` cannot be deleted while graph edges reference it; {} context(s) hold such edges: {}{}. Delete the edges, or ask for CASCADE.",
            contexts.len(),
            named.join(", "),
            if truncated {
                format!(" (the probe stopped after {RESTRICT_ROW_PROBE_SEEKS} seeks per direction; there may be more)")
            } else {
                String::new()
            }
        ))
    }

    /// One candidate read, patched and put back.
    fn update_candidate(
        &mut self,
        c: CollectionId,
        id: EntityId,
        patch: &UpdatePatch<'_>,
    ) -> Result<bool> {
        self.user_write()?;
        let catalog = self.catalog(c)?;
        let Some(old) = self.load_candidate(c, id, true)? else {
            return Ok(false);
        };
        let doc = patch.apply(&old.entity.document)?;
        let key = old.entity.key.clone();
        self.write_entity(&catalog, &key, doc, Some(old))?;
        Ok(true)
    }

    /// One row by id, with its vector sidecars, for a write that already
    /// knows the id. The mirror of `load_entity`, which starts from a key.
    fn load_candidate(
        &self,
        c: CollectionId,
        id: EntityId,
        render_vectors: bool,
    ) -> Result<Option<LoadedEntity>> {
        if id.collection != c {
            return Err(invalid("write candidate belongs to another collection"));
        }
        let Some(row) = self.store()?.get(&row_key(id))? else {
            return Ok(None);
        };
        let mut vectors = Vec::new();
        let entity = self.decode_with_vectors(id, &row, Some(&mut vectors), render_vectors)?;
        Ok(Some(LoadedEntity { entity, vectors }))
    }

    // ── the bulk scope (docs/dist/OPS_CONTRACT.md §7) ─────────────────────

    /// Open a write scope: the durability point moves to the matching
    /// [`Database::end_bulk`].
    ///
    /// No new atomic. A put inside a scope is an ordinary put -- e4's
    /// durability point is `commit` and `put`/`delete`/`put_edge` already
    /// write into the working tree without one, so a bulk load in e4 is
    /// already "many puts, then one commit". What this adds is the NAME:
    /// scopes nest, the counter says how deep, and only the outermost close
    /// commits, so a library that opens a scope inside a caller's scope
    /// cannot cut the caller's batch short.
    ///
    /// The durability rule (§7): the batch commits with **the same durability
    /// as any other commit** -- the same FULL barrier, the same publication.
    /// There is no weakened barrier to ask for.
    pub fn begin_bulk(&mut self) -> Result<()> {
        self.ready_write()?;
        self.bulk_depth = self
            .bulk_depth
            .checked_add(1)
            .ok_or_else(|| invalid("bulk scopes nested deeper than 4,294,967,295"))?;
        Ok(())
    }

    /// Close a write scope. The OUTERMOST close calls `commit`; an inner one
    /// only decrements.
    ///
    /// Returns whether this close committed. Unlike e3, an unbalanced close
    /// is an ERROR rather than absorbed: e3 absorbed it because the call
    /// arrives from FFI and a panic across that boundary is worse than a
    /// silent one, and this is not that boundary. A caller that has lost
    /// count has a bug, and `end_bulk` outside a scope would otherwise commit
    /// somebody else's uncommitted rows.
    pub fn end_bulk(&mut self) -> Result<bool> {
        if self.bulk_depth == 0 {
            return Err(invalid("end_bulk without a matching begin_bulk"));
        }
        self.bulk_depth -= 1;
        if self.bulk_depth > 0 {
            return Ok(false);
        }
        self.commit()?;
        Ok(true)
    }

    /// How many bulk scopes are open on this handle. Zero is the ordinary
    /// state.
    pub fn bulk_depth(&self) -> u32 {
        self.bulk_depth
    }
}
