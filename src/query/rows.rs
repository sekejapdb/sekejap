//! Reading primary rows: the lockstep row reader, the batched row reads, and
//! the projection that turns a row into the values a page returns.
use super::*;

pub(super) struct RowData {
    pub(super) layout: Arc<Layout>,
    pub(super) bytes: Vec<u8>,
}

pub(super) fn decode_row(db: &Database, bytes: Vec<u8>) -> QueryResult<RowData> {
    let id = layout_id(&bytes)?;
    Ok(RowData {
        layout: db.layout(id)?,
        bytes,
    })
}

/// How far ahead the lockstep cursor will walk before it gives up on itself.
///
/// `peek_at_or_after` reaches a key past its pinned leaf by binary-searching
/// the leaf it is on and then stepping to the next leaf, one at a time --
/// cheap for a row a few slots away, ruinous for a row a hundred leaves away.
/// A page whose winners are SPARSE in the primary tree (a nine-row spatial
/// answer, a one-row graph hop) would walk the whole collection's leaves to
/// collect them, and it did: `win/spatial_tiny` went 30 -> 109 us and
/// `graph/hop1_project` 11 -> 47 us before this bound.
///
/// The bound is a number of ROWS but the thing it is protecting against is
/// LEAVES. At 32 it was a fraction of one leaf of a narrow collection, so a
/// walk that is dense in runs and sparse between them -- `price > 400` matches
/// nine rows out of every forty-nine, so it steps 1,1,...,1,40 -- gave the
/// whole page back to the point-get at its FIRST gap, and then paid a full
/// root-to-leaf descent per winner: measured at 4.0 pager accesses and 6
/// allocations per row on `filter/range_open`.
///
/// How many leaves a row-gap spans is a property of the DATA, not of the plan:
/// the same gap of a hundred sequences is a fraction of a leaf in a collection
/// of forty-byte rows and a dozen leaves in one of five-hundred-byte rows. So
/// this stays a cheap pre-filter -- a gap wider than this cannot be worth
/// stepping under any row width, and the first reach is what it bounds -- and
/// what the page actually decides on is [`LOCKSTEP_LEAVES`], which the cursor
/// measures.
const LOCKSTEP_REACH: u64 = 256;

/// How many LEAVES one reach may cross before the page gives the rest of
/// itself back to the point-get.
///
/// `RangeIter::advance` climbs the parent path and re-descends the leftmost
/// spine for every leaf it steps, so a reach across a dozen leaves costs
/// several times the root-to-leaf descent it was replacing. A reach that stays
/// inside the pinned leaf costs nothing at all, and one that crosses a leaf or
/// two is still cheaper than a descent; past that the cursor is not paying for
/// itself and the reader stops pretending it is.
///
/// The cost is only knowable AFTER the reach, so the page pays one expensive
/// one and then stops -- the same shape as the row pre-filter above, which is
/// what keeps that pre-filter necessary: it is the bound on how bad that one
/// reach can be.
const LOCKSTEP_LEAVES: u32 = 2;

/// How a page reads primary rows.
///
/// When rows are asked for in ascending entity id -- an equality posting is
/// ordered by sequence, a graph result is sorted, the entity walk is the
/// primary tree, and an id ranking returns winners in that order too -- the
/// keys are ascending primary keys. One forward cursor can then step through
/// them, paying one pinned leaf for every row that lives on it, instead of
/// descending the tree from the root for each. Any other order keeps the
/// point-get it always did, and the cursor is opened only when something
/// actually reads a row.
pub(super) struct PrimaryRows<'a> {
    db: &'a Database,
    ascending: bool,
    cursor: Option<RangeIter<'a>>,
    last: u64,
    /// The seek key, rebuilt in place. One `Vec` for the page rather than one
    /// per row read.
    key: Vec<u8>,
}

/// Which way one row is going to be reached.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// Forward from where the cursor is parked.
    Lockstep,
    /// A fresh descent from the root.
    PointGet,
}

impl<'a> PrimaryRows<'a> {
    pub(super) fn new(db: &'a Database, ascending: bool) -> Self {
        Self {
            db,
            ascending,
            cursor: None,
            last: 0,
            key: Vec::new(),
        }
    }

    /// Point the reusable key at `id` and say how it will be reached, moving
    /// the cursor's own state along with the decision.
    fn plan(&mut self, id: EntityId) -> Reach {
        // `ordered` RETURNS a `Vec`, so building the key out of it allocated
        // twice -- nine bytes each -- for every row any page read, on every
        // path: the counting allocator put two of `filter/two_ranges`' six
        // allocations per candidate right here, for a key whose buffer is
        // already owned and already the right size. `ordered_into` is the same
        // frozen encoding appended to a buffer the caller keeps.
        self.key.clear();
        self.key.push(0x40);
        ordered_into(&mut self.key, u64::from(id.collection.0));
        ordered_into(&mut self.key, id.sequence);
        if !self.ascending {
            return Reach::PointGet;
        }
        if self.cursor.is_some() && id.sequence <= self.last {
            // The cursor only moves forward. A target behind it would make
            // `peek_at_or_after` return the record the cursor is parked on,
            // whose key does not match, and a present row would be reported
            // missing. Callers ascend by construction; this is the guard that
            // makes that a property of the reader rather than of every caller.
            return Reach::PointGet;
        }
        if self.cursor.is_some() && id.sequence.saturating_sub(self.last) > LOCKSTEP_REACH {
            // The rows this page wants are SPARSE in the primary tree, so the
            // cursor is not paying for itself: reaching each one costs a
            // re-descent anyway, and a cursor re-descent builds cursor state a
            // point-get does not. Give the rest of the page back to the
            // point-get. One page decides this once, from the first gap it
            // sees, and the next page decides again.
            self.ascending = false;
            self.cursor = None;
            return Reach::PointGet;
        }
        Reach::Lockstep
    }

    /// Point the reader at a fresh ascending run.
    ///
    /// The sparse bail-out in `plan` is a decision the reader makes ONCE and
    /// then keeps: it drops the cursor and never opens another. That is right
    /// for a page that reads its rows in one pass, and wrong for one that
    /// reads them in BATCHES -- each batch ascends from its own smallest id,
    /// so a batch that was sparse says nothing about the next one.
    fn restart(&mut self, ascending: bool) {
        self.ascending = ascending;
        self.cursor = None;
        self.last = 0;
    }

    /// What the reach the cursor just made actually cost, in leaves.
    ///
    /// A page that crossed more than [`LOCKSTEP_LEAVES`] to reach one row is
    /// sparse in the primary tree however close together its SEQUENCES looked,
    /// so it gives the rest of itself back to the point-get exactly as
    /// `plan`'s row pre-filter does.
    fn charge(&mut self, stepped: u32) {
        if stepped > LOCKSTEP_LEAVES {
            self.ascending = false;
            self.cursor = None;
        }
    }

    /// Park the cursor on the first record at or after the planned key.
    fn seek(&mut self, id: EntityId) -> QueryResult<()> {
        if self.cursor.is_none() {
            self.cursor = Some(
                self.db
                    .store()?
                    .range(&self.key)
                    .map_err(Error::from)
                    .map_err(QueryError::from)?,
            );
        }
        self.last = id.sequence;
        Ok(())
    }

    pub(super) fn read(&mut self, id: EntityId) -> QueryResult<Option<Vec<u8>>> {
        if self.plan(id) == Reach::PointGet {
            return self
                .db
                .store()?
                .get(&self.key)
                .map_err(Error::from)
                .map_err(QueryError::from);
        }
        self.seek(id)?;
        let key = &self.key;
        let cursor = self.cursor.as_mut().expect("the cursor was just opened");
        let before = cursor.leaves_stepped();
        let found = match cursor
            .peek_at_or_after(key)
            .map_err(Error::from)
            .map_err(QueryError::from)?
        {
            Some((found, value)) if found == key.as_slice() => Some(value.to_vec()),
            _ => None,
        };
        let stepped = cursor.leaves_stepped() - before;
        self.charge(stepped);
        Ok(found)
    }

    /// Look at one row WITHOUT copying it out of the leaf.
    ///
    /// A page that reads a row only to answer a predicate -- `price > 100 AND
    /// rating < 3.0` reads 15,912 rows to return 7,956 -- copied every one of
    /// them into a `Vec` that the filter read one field out of and dropped.
    /// The lockstep cursor is standing on the record, so the bytes can be
    /// borrowed straight out of the pinned leaf; the point-get path has no
    /// such borrow to offer and materialises as it always did.
    pub(super) fn with_row<R>(
        &mut self,
        id: EntityId,
        f: impl FnOnce(Option<&[u8]>) -> QueryResult<R>,
    ) -> QueryResult<R> {
        if self.plan(id) == Reach::PointGet {
            let bytes = self
                .db
                .store()?
                .get(&self.key)
                .map_err(Error::from)
                .map_err(QueryError::from)?;
            return f(bytes.as_deref());
        }
        self.seek(id)?;
        let key = &self.key;
        let cursor = self.cursor.as_mut().expect("the cursor was just opened");
        let before = cursor.leaves_stepped();
        let value = match cursor
            .peek_at_or_after(key)
            .map_err(Error::from)
            .map_err(QueryError::from)?
        {
            Some((found, value)) if found == key.as_slice() => f(Some(value)),
            _ => f(None),
        };
        let stepped = cursor.leaves_stepped() - before;
        self.charge(stepped);
        value
    }

    /// Is the row still there? The same reach, without the copy.
    ///
    /// A key-only page asks the primary tree for a winner's row only to refuse
    /// an orphan -- a posting can outlive the record it names. It then decodes
    /// nothing and drops the bytes, so copying a whole row out of the pinned
    /// leaf to answer a yes/no was one allocation and one row-sized memcpy per
    /// returned row, on every page that projects no field.
    pub(super) fn exists(&mut self, id: EntityId) -> QueryResult<bool> {
        if self.plan(id) == Reach::PointGet {
            return Ok(self
                .db
                .store()?
                .get(&self.key)
                .map_err(Error::from)
                .map_err(QueryError::from)?
                .is_some());
        }
        self.seek(id)?;
        let key = &self.key;
        let cursor = self.cursor.as_mut().expect("the cursor was just opened");
        let before = cursor.leaves_stepped();
        let present = matches!(
            cursor
                .peek_at_or_after(key)
                .map_err(Error::from)
                .map_err(QueryError::from)?,
            Some((found, _)) if found == key.as_slice()
        );
        let stepped = cursor.leaves_stepped() - before;
        self.charge(stepped);
        Ok(present)
    }
}

/// How many candidates one batch of row reads holds.
///
/// The batch exists to turn a random point-get per candidate into one forward
/// pass of the primary tree, so it wants to be large; it holds a row per entry
/// while it does, so it cannot be unbounded. A page never gathers more than it
/// could return, and never more than this.
/// How many ranked rows one page may HOLD BACK for the pages after it.
///
/// A page whose driver walks in an order unrelated to the ranking reads the
/// whole candidate stream whatever it does (see [`PreparedQuery::run`]), so
/// the rows past this page are rows it has already ranked. Keeping them turns
/// the answer's cost from one pass PER PAGE into one pass per this many rows:
/// an answer of R rows costs `R / RUN_ROWS` passes instead of `R / page_size`.
/// At 48M rows `popsim`'s `born_decade` returned 5.58M rows in pages of 8,192
/// -- 681 passes over a 5.58M-posting range, which is what made it take
/// 1,011 s.
///
/// What is LEFT here is a walk whose order the RANKING does not share: a
/// spatial driver under an id or scalar ranking, whose cells arrive in no
/// order that ranking knows. Each of the big cases has since been asked in
/// the order its own driver produces instead -- `born_decade` in its index's
/// ascending order (`popsim` deviation 8), the radius and bbox cases in cell
/// order (deviation 9, `QueryOrder::Driver`) -- and a text-driven page never
/// needed the run, because its merge ascends by document. A query that still
/// asks for a re-ranking it cannot walk in keeps the run, and pays one pass
/// per this many rows rather than one per page.
///
/// The bound is in BYTES because what is held is a rank key each and the
/// promise has to mean the same thing whatever a rank key weighs. It is the
/// same order as the default buffer pool, it is transient -- it lives on the
/// prepared query and goes when the query does -- and it is only ever reached
/// by an answer large enough to have paid far more than this in re-walking.
pub(super) const RUN_BYTES: usize = 8 << 20;

pub(super) const RUN_ROWS: usize = {
    let rows = RUN_BYTES / std::mem::size_of::<HeapEntry>();
    if rows < MAX_PAGE_SIZE {
        MAX_PAGE_SIZE
    } else {
        rows
    }
};

pub(super) const ROW_BATCH: usize = 4_096;

/// ... and how many bytes of row those entries may hold.
///
/// Rows are whatever the caller stored. The count bound alone would let a
/// batch of 4,096 megabyte blobs hold four gigabytes, so the read stops
/// filling at this many bytes and the candidates past it are read one at a
/// time exactly as before -- slower, and bounded.
const ROW_BATCH_BYTES: usize = 4 << 20;

/// Read one batch of candidates' rows in ASCENDING ENTITY ORDER.
///
/// The candidates arrive in the driver's order, which for a range posting is
/// `(value, sequence)`: the ids do not ascend, so the page's lockstep reader
/// gives up on its first gap and every candidate pays a fresh root-to-leaf
/// descent -- measured at 4.0 pager accesses and 6.0 allocations per candidate
/// on `filter/two_ranges`. Filters are pure functions of the row, so the ORDER
/// they are evaluated in cannot change the answer: the batch is sorted by id
/// here, read through one forward cursor, and handed back to the walk in its
/// original order with the rows already in hand.
///
/// A candidate whose row is absent is left alone rather than refused. A
/// posting can outlive the record it names, and whether that is an error is a
/// question for the filter that asked for the row -- not for a reader that is
/// only moving the read earlier.
pub(super) fn read_batch_rows<'a, C: FnMut() -> bool>(
    db: &'a Database,
    rows: &mut PrimaryRows<'a>,
    filters: &[CompiledFilter],
    ranges: &[MembershipSet],
    keep_rows: bool,
    batch: &mut [Candidate],
    order: &mut Vec<(u64, u32)>,
    scratch: &mut RowScratch,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<()> {
    order.clear();
    // The SEQUENCE travels with the index rather than being looked up through
    // it. Sorting indices alone makes every one of a 4,096-entry sort's ~49,000
    // comparisons an indirect load into a 64-byte-per-entry array; the
    // collection is the same for every candidate here (the walk refuses one
    // that crosses), so what orders them is one `u64` each.
    for (at, candidate) in batch.iter().enumerate() {
        if candidate.row.is_none() {
            order.push((
                candidate.id.sequence,
                u32::try_from(at).map_err(|_| invalid_query("query batch overflow"))?,
            ));
        }
    }
    order.sort_unstable();
    rows.restart(true);
    let mut held = 0usize;
    for (_, at) in order.iter() {
        meter.check_cancelled()?;
        let at = *at as usize;
        meter.charge(WorkResource::PrimaryReads, 1)?;
        let id = batch[at].id;
        let satisfied = batch[at].satisfied_filter;
        // Whether this row has to survive the pass as BYTES. A page that
        // projects fields, or ranks by a key only the row holds, keeps it; a
        // key-only page reads it to answer a predicate and then wants nothing
        // from it, and copying it out of the leaf for that was one allocation
        // and a row-sized memcpy per candidate.
        let (verdict, kept) = rows.with_row(id, |bytes| {
            let Some(bytes) = bytes else {
                return Ok((None, None));
            };
            let verdict = batch_filters_match(db, filters, ranges, satisfied, id, bytes, scratch, meter)?;
            let kept = if keep_rows && verdict != Some(false) && held < ROW_BATCH_BYTES {
                Some(bytes.to_vec())
            } else {
                None
            };
            Ok((verdict, kept))
        })?;
        if let Some(bytes) = kept {
            held = held.saturating_add(bytes.len());
            batch[at].row = Some(bytes);
        }
        batch[at].row_filtered = verdict;
    }
    Ok(())
}

fn project_value(value: dense_v3::FieldValue) -> QueryResult<ProjectedValue> {
    match value {
        dense_v3::FieldValue::Missing => Ok(ProjectedValue::Missing),
        dense_v3::FieldValue::Null => Ok(ProjectedValue::Null),
        dense_v3::FieldValue::Inline(value) => Ok(ProjectedValue::Value(value)),
        dense_v3::FieldValue::Vector { .. } => unreachable!("vector projection needs sidecar"),
    }
}

/// Materialise every projected field of one winner in ONE dense-v3 walk.
///
/// A row is a single record; the number of columns asked for changes what
/// comes out of the walk, not how many walks there are.
pub(super) fn project_fields<C: FnMut() -> bool>(
    db: &Database,
    id: EntityId,
    row: &RowData,
    projection: &[String],
    scratch: &mut ProjectionScratch,
    out: &mut Vec<(String, ProjectedValue)>,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<()> {
    if projection.is_empty() {
        return Ok(());
    }
    meter.note_row_decode();
    scratch
        .ensure_plan(&row.layout, projection)
        .map_err(|error| corrupt_query(format!("dense-v3 row: {error}")))?;
    let ProjectionScratch { plan, values } = scratch;
    let plan = &plan.as_ref().expect("the plan was just built").1;
    dense_v3::read_fields(&row.layout, &row.bytes, projection, plan, values)
        .map_err(|error| corrupt_query(format!("dense-v3 row: {error}")))?;
    for (field, value) in projection.iter().zip(values.drain(..)) {
        meter.check_cancelled()?;
        out.push((field.clone(), project_value_or_sidecar(db, id, value, meter)?));
    }
    Ok(())
}

/// The two buffers a projected page reuses across its rows: the ordinal map
/// for the layout it is on, and the vector the decoder fills. Rows of one
/// collection share a layout in the ordinary case, and the map is rebuilt only
/// where they do not.
#[derive(Default)]
pub(super) struct ProjectionScratch {
    plan: Option<(u64, dense_v3::FieldPlan)>,
    values: Vec<dense_v3::FieldValue>,
}

impl ProjectionScratch {
    fn ensure_plan(
        &mut self,
        layout: &Layout,
        projection: &[String],
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        if !self.plan.as_ref().is_some_and(|(id, _)| *id == layout.id) {
            self.plan = Some((layout.id, dense_v3::FieldPlan::new(layout, projection)?));
        }
        Ok(())
    }
}

/// A projected value, fetching the authoritative vector sidecar when the field
/// is a historical vector.
fn project_value_or_sidecar<C: FnMut() -> bool>(
    db: &Database,
    id: EntityId,
    value: dense_v3::FieldValue,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<ProjectedValue> {
    match value {
        dense_v3::FieldValue::Vector { ordinal, dimension } => {
            meter.charge(WorkResource::VectorSidecars, 1)?;
            meter.charge(
                WorkResource::VectorLanes,
                u64::try_from(dimension).map_err(invalid_query)?,
            )?;
            let bytes = db
                .store()?
                .get(&vector_key(id, ordinal))?
                .ok_or_else(|| corrupt_query("projected vector sidecar is missing"))?;
            crate::index::vector::exact::validate_vector(&bytes, dimension)?;
            Ok(ProjectedValue::Value(
                crate::vector_json(&bytes, dimension).map_err(corrupt_query)?,
            ))
        }
        value => project_value(value),
    }
}

/// A `Write` that keeps the length and throws the bytes away.
struct ByteCount(u64);

impl std::io::Write for ByteCount {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(buf.len() as u64);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// How many bytes one projected value would serialise to.
///
/// This is charged against the output budget for every projected value of
/// every returned row, and it used to be measured by serialising the value
/// into a fresh `Vec` and reading `len()` -- a heap allocation and a full
/// serialisation per value, whose only product was an integer. A five-column
/// page of 8,192 rows did that 40,960 times and threw all of it away.
fn json_size(value: &Value) -> QueryResult<u64> {
    let mut count = ByteCount(0);
    serde_json::to_writer(&mut count, value).map_err(invalid_query)?;
    Ok(count.0)
}

#[inline]
pub(super) fn checked_output_size(row: &QueryRow) -> QueryResult<u64> {
    // The commonest row the engine emits: an id-ordered or driver-ordered row
    // with nothing projected. Its size is the 12 bytes of identity plus the
    // one byte every row is charged, and there is no arithmetic that can
    // overflow on the way to saying so.
    if row.projected.is_empty() && matches!(row.order, OrderValue::EntityId | OrderValue::Driver) {
        return Ok(13);
    }
    let mut size = 12u64; // collection u32 + sequence u64
    size = size.checked_add(1).ok_or_else(|| {
        QueryError::Database(Error::InvalidInput("query output size overflow".into()))
    })?;
    let order_bytes = match &row.order {
        OrderValue::Scalar(value) => match value {
            OwnedScalarValue::Nullish => 1,
            OwnedScalarValue::Bool(_) => 2,
            OwnedScalarValue::I64(_) | OwnedScalarValue::F64(_) => 9,
            OwnedScalarValue::Text(value) => 1u64
                .checked_add(u64::try_from(value.len()).map_err(invalid_query)?)
                .ok_or_else(|| invalid_query("query output size overflow"))?,
        },
        OrderValue::Distance(_) | OrderValue::Bm25(_) | OrderValue::Score(_) => 9,
        // Neither carries a value the caller is charged for: an id-ordered
        // row's key is its id, and a driver-ordered row's place is the
        // candidate stream's, not a value in the row.
        OrderValue::EntityId | OrderValue::Driver => 0,
    };
    if order_bytes != 0 {
        size = size
            .checked_add(order_bytes)
            .ok_or_else(|| invalid_query("query output size overflow"))?;
    }
    for (field, value) in &row.projected {
        size = size
            .checked_add(u64::try_from(field.len()).map_err(invalid_query)?)
            .ok_or_else(|| invalid_query("query output size overflow"))?;
        size = size
            .checked_add(1)
            .ok_or_else(|| invalid_query("query output size overflow"))?;
        let bytes = match value {
            ProjectedValue::Missing | ProjectedValue::Null => 0,
            ProjectedValue::Value(value) => json_size(value)?,
        };
        size = size
            .checked_add(bytes)
            .ok_or_else(|| invalid_query("query output size overflow"))?;
    }
    Ok(size)
}
