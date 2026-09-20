//! Membership sets: the bounded set or bitmap a filter is turned into when a
//! posting walk is cheaper than a row read, and the budget that bounds it.
use super::*;

/// One non-driving filter's own index walk, done ONCE and kept for the rest
/// of the query's pages: every entity SEQUENCE that filter's postings prove,
/// so a candidate afterwards is a membership test instead of a primary read.
///
/// Two filter shapes are collected here.
///
/// A scalar RANGE. An equality filter already answers a non-driving candidate
/// from its own posting (`scalar_eq_posting_matches`): the key
/// `value || entity` is known, so testing it costs one point read. A range
/// predicate cannot do that -- the candidate's value is unknown until
/// something reads it -- so what this holds is the index-side answer Postgres
/// gets from a bitmap AND of two posting lists.
///
/// A POINT filter (bbox or radius). Its postings are filed by Hilbert cell
/// and each one CARRIES the stored coordinates, so the exact predicate is
/// decided from the posting alone -- the same test, on the same bytes, that
/// `SpatialCursor::next` makes when the same filter drives. That is why a set
/// collected here certifies the filter outright (T3's no-row rule): a point
/// filter used to be the one non-driving filter that went to the primary tree
/// for EVERY candidate, because its cover ranges were only ever walked when
/// it was the driver.
#[derive(Clone, Debug)]
pub(super) enum MembershipSet {
    /// This position is not a candidate for the optimization: neither a Range
    /// predicate nor a point filter, or the position driving the query (whose
    /// own walk already answers it for free).
    Ineligible,
    /// Eligible, but no page has walked it yet.
    Unbuilt,
    /// The filter's own collection span (every sequence it could ever name)
    /// is too wide for even a bitmap to fit the budget, or the walk found more
    /// entities than the plain-Vec cap allows while a bitmap was not a viable
    /// fallback either. Named sacrifice (Law 4): a predicate wide enough to
    /// fail this budget gets no faster than it already was -- every candidate
    /// still reads its row -- rather than holding an unbounded set in memory.
    Overflow,
    /// Every entity SEQUENCE the walk proved, ascending, so `binary_search`
    /// answers membership. Chosen over a bitmap when the predicate is narrow
    /// enough, in a large enough collection, that the Vec is the smaller of
    /// the two -- e.g. one day out of decades of `born` values, or a 1 km
    /// radius in a world-sized point index.
    Ids(Arc<Vec<u64>>),
    /// One bit per sequence in `1..=collection_span`, set for every entity
    /// SEQUENCE the walk proved. Chosen once the Vec representation would be
    /// bigger than this: unlike the Vec, setting a bit costs no sort and no
    /// allocation growth once the bitmap is sized, so a wide predicate (e.g. a
    /// whole decade of `born`) is one linear pass, no CPU cost from the
    /// postings count once past the initial allocation.
    Bitmap(Arc<Vec<u8>>),
}

/// How many entity ids one [`MembershipSet`] may hold as a plain `Vec`
/// before it is either converted to a [`MembershipSet::Bitmap`] (when one
/// would fit the budget) or abandoned as [`MembershipSet::Overflow`] (when
/// even a bitmap would not), in the same currency [`RUN_BYTES`] already
/// bounds a held run in: both are memory one page keeps beyond what it
/// returns this call.
///
/// This is also the ceiling used when a bitmap is not viable at all (the
/// collection's span alone would need more than `RUN_BYTES` of bits) -- the
/// same cap the Vec-only design used before bitmaps existed, so a collection
/// too large even for a bitmap degrades to exactly that prior behaviour
/// rather than something new.
const MEMBERSHIP_SET_CAP: usize = RUN_BYTES / std::mem::size_of::<u64>();

/// The one ceiling in BYTES that every membership set of one query is held
/// against, whatever representation it is in and however many of them are
/// live at the same time: the `RUN_BYTES` memory promise, stated once.
///
/// [`WorkResource::MembershipBytes`] is the resource that names it when a
/// walk passes it. It has no [`QueryBudget`] field, because it is not a
/// caller allowance -- it is what this page promises to hold.
pub(super) const MEMBERSHIP_BYTES_CAP: usize = RUN_BYTES;

/// A bitmap large enough to need more than this many bytes is not a viable
/// [`MembershipSet::Bitmap`]: `RUN_BYTES` is the same per-page memory
/// currency the Vec cap above is drawn from. One bit per sequence, so a
/// collection whose span exceeds `8 * RUN_BYTES` sequences (about 67
/// million entities) never gets a bitmap here, whatever the range's own
/// selectivity.
const MEMBERSHIP_BITMAP_CAP_BYTES: usize = MEMBERSHIP_BYTES_CAP;

/// The number of bytes a bitmap covering sequences `1..=span` would need.
fn membership_bitmap_bytes(span: u64) -> u64 {
    span.div_ceil(8)
}

/// Sets the bit for `sequence` (1-based, as every allocated entity sequence
/// is) in a bitmap sized by [`membership_bitmap_bytes`]. A sequence of zero
/// or one past the bitmap's span was never allocated when the bitmap was
/// sized, so a posting that names one is corrupt: the caller gets `Err`,
/// never an out-of-bounds write (Law 5).
fn membership_bitmap_set(bits: &mut [u8], sequence: u64) -> QueryResult<()> {
    let index = usize::try_from(sequence.checked_sub(1).ok_or_else(|| {
        corrupt_query("membership posting sequence is zero")
    })?)
    .map_err(|_| corrupt_query("membership posting sequence overflow"))?;
    match bits.get_mut(index / 8) {
        Some(byte) => {
            *byte |= 1 << (index % 8);
            Ok(())
        }
        None => Err(corrupt_query(
            "membership posting sequence past the collection span",
        )),
    }
}

/// Tests the bit for `sequence` (1-based). A sequence at or past the
/// bitmap's span was never allocated when the bitmap was built and so was
/// never set -- `false`, not a panic or an out-of-bounds read.
pub(super) fn membership_bitmap_contains(bits: &[u8], sequence: u64) -> bool {
    let index = (sequence - 1) as usize;
    bits.get(index / 8).is_some_and(|byte| byte & (1 << (index % 8)) != 0)
}

pub(super) fn scalar_eq_posting_matches<C: FnMut() -> bool>(
    db: &Database,
    info: &IndexInfo,
    expected: &[u8],
    id: EntityId,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<bool> {
    meter.charge(WorkResource::ScalarPostings, 1)?;
    let value = db
        .index_get(info, &crate::collections::catalog::skey(info, expected, id.sequence))
        .map_err(QueryError::from)?;
    match value {
        None => Ok(false),
        Some(value) if value.is_empty() => Ok(true),
        Some(_) => Err(corrupt_query("scalar index entry")),
    }
}

/// Walk one non-driving scalar RANGE's postings once and collect every
/// entity SEQUENCE it proves, ascending.
///
/// Same shape as `ScalarCursor::next`'s own forward walk -- open at the
/// predicate's lower bound, stop the first key `scalar_key_position` puts
/// past the upper one -- but with no candidate to build and no direction to
/// choose: this collects the whole range instead of stopping at a page size.
/// The nullish entry a NULL and a MISSING field share is excluded exactly as
/// `proves_predicate` excludes it there, so a set this returns answers a
/// range predicate exactly as `scalar_filter_matches` answers it from the row
/// -- neither ever matches null or missing.
///
/// `Overflow` abandons whatever it collected rather than handing back a
/// partial set: a binary search over less than the whole range would answer
/// "not found" for members the row-read path would have kept.
fn build_scalar_range_set<C: FnMut() -> bool>(
    db: &Database,
    info: &IndexInfo,
    predicate: &EncodedScalarFilter,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<MembershipSet> {
    let prefix = scalar_prefix(info.id);
    let mut start = prefix.clone();
    if let Some(lower) = scalar_lower(predicate) {
        start.extend_from_slice(lower);
    }
    let mut walk = match db.index_range(info, &start).map_err(QueryError::from)? {
        Some(iter) => iter,
        None => return Ok(MembershipSet::Ids(Arc::new(Vec::new()))),
    };
    // The collection's span bounds a bitmap without a scan: every sequence a
    // posting in this walk can name is below it. A Vec is kept only while it
    // would stay smaller than that bitmap; once it would not, converting
    // loses nothing (every id collected so far still fits the bitmap by
    // construction) and every posting after that is one bit, not a growing
    // allocation. When the bitmap itself would not fit the budget, `vec_cap`
    // falls back to the plain per-element cap the Vec-only design used, and
    // the walk is abandoned exactly as it was before bitmaps existed.
    let budget = MembershipBudget::new(db, info.collection)?;
    let (bitmap_bytes, bitmap_viable, vec_cap) =
        (budget.bitmap_bytes, budget.bitmap_viable, budget.vec_cap);
    let mut ids: Vec<u64> = Vec::new();
    let mut bits: Option<Vec<u8>> = None;
    loop {
        meter.check_cancelled()?;
        meter.charge(WorkResource::ScalarPostings, 1)?;
        let sequence = {
            let Some((key, value)) = walk.peek_ref().map_err(Error::from).map_err(QueryError::from)?
            else {
                break;
            };
            if !key.starts_with(&prefix) {
                break;
            }
            let suffix = &key[prefix.len()..];
            let value_len = scalar_key::width(&info.kind, suffix)?;
            let encoded = suffix
                .get(..value_len)
                .ok_or_else(|| corrupt_query("truncated scalar value key"))?;
            match scalar_key_position(predicate, encoded) {
                Ordering::Greater => break,
                Ordering::Less => None,
                Ordering::Equal if encoded == NULLISH_SCALAR_KEY => None,
                Ordering::Equal => {
                    let mut at = prefix.len() + value_len;
                    let sequence = read_ordered(key, &mut at)?;
                    if at != key.len() || sequence == 0 || !value.is_empty() {
                        return Err(corrupt_query("scalar index entry"));
                    }
                    Some(sequence)
                }
            }
        };
        walk.step();
        if let Some(sequence) = sequence {
            match bits.as_mut() {
                Some(bits) => membership_bitmap_set(bits, sequence)?,
                None if ids.len() == vec_cap => {
                    if !bitmap_viable {
                        return Ok(MembershipSet::Overflow);
                    }
                    let mut fresh = vec![0u8; bitmap_bytes as usize];
                    for &s in &ids {
                        membership_bitmap_set(&mut fresh, s)?;
                    }
                    membership_bitmap_set(&mut fresh, sequence)?;
                    ids = Vec::new();
                    bits = Some(fresh);
                }
                None => ids.push(sequence),
            }
        }
    }
    if let Some(bits) = bits {
        return Ok(MembershipSet::Bitmap(Arc::new(bits)));
    }
    ids.sort_unstable();
    Ok(MembershipSet::Ids(Arc::new(ids)))
}

/// The Vec/bitmap/Overflow budget every [`MembershipSet`] walk shares.
///
/// Stated once so a scalar range and a point cover are bounded by the SAME
/// rule: a bitmap is viable when one bit per sequence in the collection's
/// span fits [`MEMBERSHIP_BITMAP_CAP_BYTES`]; while it is, the plain Vec is
/// kept only while it would stay smaller than that bitmap; when it is not,
/// the Vec's own [`MEMBERSHIP_SET_CAP`] is the ceiling and passing it is
/// `Overflow`.
struct MembershipBudget {
    /// The collection's span: the highest sequence it has ever issued, so
    /// nothing above it can name a member of it.
    span: u64,
    bitmap_bytes: u64,
    bitmap_viable: bool,
    vec_cap: usize,
}

impl MembershipBudget {
    pub(super) fn new(db: &Database, collection: CollectionId) -> QueryResult<Self> {
        let span = db.collection_span(collection).map_err(QueryError::from)?;
        let bitmap_bytes = membership_bitmap_bytes(span);
        let bitmap_viable = bitmap_bytes <= MEMBERSHIP_BITMAP_CAP_BYTES as u64;
        let vec_cap = if bitmap_viable {
            (bitmap_bytes / std::mem::size_of::<u64>() as u64) as usize
        } else {
            MEMBERSHIP_SET_CAP
        };
        Ok(Self {
            span,
            bitmap_bytes,
            bitmap_viable,
            vec_cap,
        })
    }

    /// Is this sequence one the collection could ever have issued?
    ///
    /// A CALLER's set is the only place an out-of-span sequence can come
    /// from -- an index posting past the span is corruption and is still
    /// reported as such -- and a caller naming one has named a row that
    /// cannot exist, not a corrupt database.
    fn in_span(&self, sequence: u64) -> bool {
        sequence >= 1 && sequence <= self.span
    }
}

/// The ONE byte budget every intermediate of a boolean walk is held against.
///
/// `MAX_BOOLEAN_DEPTH` bounds how deep a tree nests; it bounds nothing about
/// how much memory the frames hold AT ONCE. Each `Any`/`All` frame keeps its
/// accumulator alive while the next child is built, so a depth-8 tree used to
/// be able to hold a dozen bitmaps of the collection's span -- about 88 MiB
/// against an 8 MiB stated promise -- and the union cloned BOTH operands
/// before producing a third.
///
/// Two changes make the promise true. The folds are now in place (the
/// accumulator's own allocation is taken over and the other side is OR'd or
/// AND'd into it, so no third bitmap exists), and every live set is counted
/// here: charged when it is created, released when it is folded away, and
/// with the fold's own worst case reserved while both operands are still
/// charged. Past [`MEMBERSHIP_BYTES_CAP`] the boolean is REFUSED with
/// [`WorkResource::MembershipBytes`] rather than held.
struct LiveBytes {
    live: usize,
    peak: usize,
}

impl LiveBytes {
    fn new() -> Self {
        Self { live: 0, peak: 0 }
    }

    /// Take `bytes` of the shared budget, or refuse naming the real cap.
    fn reserve(&mut self, bytes: usize) -> QueryResult<()> {
        let want = self.live.saturating_add(bytes);
        if want > MEMBERSHIP_BYTES_CAP {
            return Err(membership_bytes_exceeded(want));
        }
        self.live = want;
        self.peak = self.peak.max(want);
        Ok(())
    }

    /// Hold one built set against the budget.
    fn hold(&mut self, set: &MembershipSet) -> QueryResult<()> {
        self.reserve(set.bytes())
    }

    /// Give back bytes that are no longer live.
    fn release(&mut self, bytes: usize) {
        self.live = self.live.saturating_sub(bytes);
    }
}

/// The refusal a membership walk raises when it would hold more than the
/// stated memory: the resource is bytes, the limit is the real cap in bytes,
/// and the attempt is the byte total that passed it. Before this it named
/// `Candidates` and the ENTRY cap of a representation that may not even have
/// been the one that overflowed.
fn membership_bytes_exceeded(attempted: usize) -> QueryError {
    QueryError::BudgetExceeded {
        resource: WorkResource::MembershipBytes,
        limit: MEMBERSHIP_BYTES_CAP as u64,
        attempted: attempted as u64,
    }
}

/// Walk one non-driving POINT filter's cover ranges once and collect every
/// entity SEQUENCE the predicate admits, ascending.
///
/// Same walk `SpatialCursor::next` makes when this filter drives -- the same
/// cover ranges from `point_ranges`, the same per-posting exact test against
/// the coordinates the posting itself carries -- with no candidate to build
/// and no page size to stop at. So a set this returns answers the filter
/// exactly as the driving walk answers it, and exactly as a row read answers
/// it: a stored point that is missing or unindexable has no posting, and
/// neither path ever admits one.
///
/// `Overflow` abandons whatever it collected rather than handing back a
/// partial set: a membership test over less than the whole cover would answer
/// "not found" for members the row-read path would have kept.
fn build_point_set<C: FnMut() -> bool>(
    db: &Database,
    info: &IndexInfo,
    predicate: PointFilter,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<MembershipSet> {
    let (ranges, _) = point_ranges(predicate)?;
    let prefix = crate::index::spatial::point::posting_prefix(info.id);
    let budget = MembershipBudget::new(db, info.collection)?;
    let mut ids: Vec<u64> = Vec::new();
    let mut bits: Option<Vec<u8>> = None;
    for (lo, hi) in ranges {
        let mut start = prefix.clone();
        start.extend((lo as u32).to_be_bytes());
        // An index whose tree is still empty has no postings in any range.
        let Some(mut walk) = db.index_range(info, &start).map_err(QueryError::from)? else {
            break;
        };
        loop {
            meter.check_cancelled()?;
            meter.charge(WorkResource::SpatialPostings, 1)?;
            let sequence = {
                let Some((key, value)) =
                    walk.peek_ref().map_err(Error::from).map_err(QueryError::from)?
                else {
                    break;
                };
                if !key.starts_with(&prefix) {
                    break;
                }
                let cell = key
                    .get(prefix.len()..prefix.len() + 4)
                    .ok_or_else(|| corrupt_query("spatial point posting key"))?;
                if u64::from(u32::from_be_bytes(cell.try_into().unwrap())) > hi {
                    break;
                }
                let (_, sequence, point) =
                    crate::index::spatial::point::decode_posting(&prefix, key, value)?;
                let matches = match predicate {
                    PointFilter::Bbox(bounds) => bounds.contains(point),
                    PointFilter::Radius {
                        center,
                        radius_metres,
                    } => within_radius(center, point, radius_metres).map_err(corrupt_query)?,
                };
                matches.then_some(sequence)
            };
            walk.step();
            if let Some(sequence) = sequence {
                match bits.as_mut() {
                    Some(bits) => membership_bitmap_set(bits, sequence)?,
                    None if ids.len() == budget.vec_cap => {
                        if !budget.bitmap_viable {
                            return Ok(MembershipSet::Overflow);
                        }
                        let mut fresh = vec![0u8; budget.bitmap_bytes as usize];
                        for &s in &ids {
                            membership_bitmap_set(&mut fresh, s)?;
                        }
                        membership_bitmap_set(&mut fresh, sequence)?;
                        ids = Vec::new();
                        bits = Some(fresh);
                    }
                    None => ids.push(sequence),
                }
            }
        }
    }
    if let Some(bits) = bits {
        return Ok(MembershipSet::Bitmap(Arc::new(bits)));
    }
    ids.sort_unstable();
    Ok(MembershipSet::Ids(Arc::new(ids)))
}

impl PreparedQuery<'_> {
    /// Walk every not-yet-built [`MembershipSet`] once, so this call's
    /// `work.scalar_postings` pays for it and every later page -- including a
    /// resumed one -- finds it already there.
    ///
    /// Idempotent: a position the loop has already resolved, in this call or
    /// an earlier one, is `Ineligible`, `Overflow`, or `Ids` and is skipped.
    pub(super) fn ensure_membership_sets<C: FnMut() -> bool>(
        &mut self,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<()> {
        // A traversal's node predicates (GRAPH_CONTRACT 4.3) are membership
        // sets like any other, built once for the prepared query: every page,
        // resumed or not, finds them already walked. The gate is lifted out
        // of the filter because building needs `&mut` while the walk below
        // holds `&self.filters`.
        for position in 0..self.filters.len() {
            let CompiledFilter::Graph { gate, .. } = &mut self.filters[position] else {
                continue;
            };
            if gate.is_built() {
                continue;
            }
            let mut lifted = std::mem::replace(gate, NodeGate::empty());
            let outcome = lifted.build(self.db, meter);
            if let CompiledFilter::Graph { gate, .. } = &mut self.filters[position] {
                *gate = lifted;
            }
            outcome?;
        }
        // A BOOLEAN filter first, and on its own terms: it has no row path
        // to fall back to, so an overflowed set is a REFUSAL with its
        // resource named, not a plan. `docs/QL_CONTRACT.md` §3 says so
        // outright -- a row-path OR reads every candidate's record, which is
        // the work the union exists to avoid.
        // One byte budget for every boolean filter of this query, not one
        // each: what `MEMBERSHIP_BYTES_CAP` promises is what the PAGE holds.
        let mut live = LiveBytes::new();
        for position in 0..self.filters.len() {
            if !matches!(self.membership[position], MembershipSet::Unbuilt) {
                continue;
            }
            let CompiledFilter::Boolean { expr, .. } = &self.filters[position] else {
                continue;
            };
            let expr = expr.clone();
            let set = build_set_expr(self.db, self.collection, &expr, &mut live, meter)?;
            if matches!(set, MembershipSet::Overflow) {
                // The resource is the memory this walk could not hold, in
                // the currency the cap is actually stated in. It used to
                // name `Candidates` and the ENTRY cap of the plain-Vec
                // representation, which is not the cap a span-sized bitmap
                // fails against; the walk stops AT the cap, so the attempt
                // is the byte that passed it.
                return Err(membership_bytes_exceeded(MEMBERSHIP_BYTES_CAP + 1));
            }
            // The finished set is already held by the walk that built it and
            // stays held: the page keeps it for the rest of the query, so the
            // next boolean filter's walk starts with it already counted.
            self.membership[position] = set;
        }
        for position in 0..self.filters.len() {
            if !matches!(self.membership[position], MembershipSet::Unbuilt) {
                continue;
            }
            let built = match &self.filters[position] {
                CompiledFilter::Scalar { info, predicate, .. } => {
                    let (info, predicate) = (info.clone(), predicate.clone());
                    build_scalar_range_set(self.db, &info, &predicate, meter)
                }
                CompiledFilter::Point { info, predicate } => {
                    let (info, predicate) = (info.clone(), *predicate);
                    build_point_set(self.db, &info, predicate, meter)
                }
                _ => unreachable!("only a scalar range or a point filter is marked Unbuilt"),
            };
            self.membership[position] = match built {
                Ok(set) => set,
                // The row-read path these filters used before this walk
                // existed never spent `ScalarPostings` -- and a non-driving
                // point filter's row-read path (`filters.rs`, `Overflow`
                // arm) charges `SpatialPostings` once per candidate for a
                // row-field extract, not once per posting; that charge is a
                // stand-in against the same counter, not a spatial posting
                // read. Either way, a caller whose budget cannot afford even
                // one posting of this walk must get exactly that path back,
                // not a new way for the same query to fail. Postings already
                // charged before this one stay charged; nothing here refunds
                // real reads.
                Err(QueryError::BudgetExceeded {
                    resource: WorkResource::ScalarPostings | WorkResource::SpatialPostings,
                    ..
                }) => MembershipSet::Overflow,
                Err(other) => return Err(other),
            };
        }
        Ok(())
    }
}


// ── boolean set algebra (docs/QL_CONTRACT.md §3) ──────────────────────────

impl MembershipSet {
    /// Is this sequence a member?
    ///
    /// `Ineligible`, `Unbuilt` and `Overflow` are not answers, so they are
    /// errors here rather than a silent `false`: a boolean filter that is
    /// tested before its set exists would refuse every candidate, and the
    /// query would return an empty page with no reason given.
    pub(super) fn contains(&self, sequence: u64) -> QueryResult<bool> {
        match self {
            Self::Ids(ids) => Ok(ids.binary_search(&sequence).is_ok()),
            Self::Bitmap(bits) => Ok(membership_bitmap_contains(bits, sequence)),
            Self::Ineligible | Self::Unbuilt | Self::Overflow => Err(corrupt_query(
                "a boolean filter was tested before its membership set was built",
            )),
        }
    }

    /// What this set costs in memory, which is the currency
    /// [`MEMBERSHIP_BYTES_CAP`] is stated in. `Overflow` costs nothing: it
    /// holds no members, which is the whole of what it means.
    fn bytes(&self) -> usize {
        match self {
            Self::Ids(ids) => ids.len().saturating_mul(std::mem::size_of::<u64>()),
            Self::Bitmap(bits) => bits.len(),
            Self::Ineligible | Self::Unbuilt | Self::Overflow => 0,
        }
    }

    /// The set as `EXPLAIN` prints it: the representation and its size.
    pub(super) fn describe(&self) -> String {
        match self {
            Self::Ids(ids) => format!("{} ids", ids.len()),
            Self::Bitmap(bits) => format!("bitmap, {} bytes", bits.len()),
            Self::Ineligible => "ineligible".to_owned(),
            Self::Unbuilt => "unbuilt".to_owned(),
            Self::Overflow => "overflow".to_owned(),
        }
    }
}

/// One node of a compiled boolean filter, as SET ALGEBRA rather than as a
/// predicate tree: every leaf is an index walk that yields entity sequences,
/// `Any` is their union, `All` their intersection and `Complement` the
/// difference between one leaf's own universe and that leaf.
///
/// There is no row path in here and there is no place to add one. That is
/// the whole point of the form: an OR evaluated per candidate reads every
/// candidate's record, so a leaf an index cannot answer is refused while the
/// filter is compiled (`compile_set_expr`), not discovered per row.
#[derive(Clone, Debug)]
pub(super) enum SetExpr {
    /// A scalar `Eq` or `Range` over one index's postings. The nullish key is
    /// excluded by `build_scalar_range_set`, exactly as the row path excludes
    /// it.
    Scalar {
        info: IndexInfo,
        predicate: EncodedScalarFilter,
    },
    /// A point `Bbox` or `Radius`: the cover walk decides the predicate from
    /// the coordinates the postings carry.
    Point {
        info: IndexInfo,
        predicate: PointFilter,
    },
    /// A range over the external-key mapping keyspace.
    Key {
        collection: CollectionId,
        predicate: EncodedScalarFilter,
    },
    /// A text match through the index's posting ids. `Phrase` is refused
    /// while compiling: an ordered adjacency is settled against the
    /// authoritative primary text, which is a row read.
    Text(PreparedText),
    /// An explicit set the caller handed over (`QueryFilter::Ids`).
    Ids(Arc<Vec<u64>>),
    Any(Vec<SetExpr>),
    All(Vec<SetExpr>),
    /// The complement of ONE leaf over that leaf's own universe. De Morgan is
    /// applied while the tree is compiled, so a `Complement` never stands
    /// over a union or an intersection and there is never a universe to guess
    /// at.
    Complement(Box<SetExpr>),
}

impl SetExpr {
    /// The shape `EXPLAIN` prints for this node, with no set sizes in it --
    /// those belong to the built set, which is printed beside this.
    pub(super) fn describe(&self) -> String {
        match self {
            Self::Scalar { info, predicate } => {
                format!("{}.{} {}", info.name, info.field, describe_encoded(predicate))
            }
            Self::Point { info, predicate } => match predicate {
                PointFilter::Bbox(_) => format!("{}.{} bbox", info.name, info.field),
                PointFilter::Radius { radius_metres, .. } => {
                    format!("{}.{} within {radius_metres} m", info.name, info.field)
                }
            },
            Self::Key { predicate, .. } => format!("_key {}", describe_encoded(predicate)),
            Self::Text(prepared) => format!(
                "{} text {:?} {:?}",
                prepared.info.name, prepared.matching, prepared.terms
            ),
            Self::Ids(ids) => format!("semi-join set, {} ids", ids.len()),
            Self::Any(children) => {
                let parts: Vec<String> = children.iter().map(Self::describe).collect();
                format!("union({})", parts.join(", "))
            }
            Self::All(children) => {
                let parts: Vec<String> = children.iter().map(Self::describe).collect();
                format!("intersect({})", parts.join(", "))
            }
            Self::Complement(child) => format!("complement({})", child.describe()),
        }
    }

    /// Does every member of this set provably name a row that is THERE?
    ///
    /// An index-side leaf does: a posting is written and retired in the same
    /// transaction as its row, which is the membership record this engine
    /// already trusts for a driving scalar range. A `Complement` does too --
    /// its members come from the universe it was taken against, which is
    /// either an index's postings or the live primary keyspace.
    ///
    /// An explicit `Ids` set does NOT: the caller chose those ids and may
    /// name one whose row is gone, so a page driven by such a set keeps the
    /// existence probe its winners owe. A union inherits the weakest of its
    /// children; an intersection needs only one child that proves it.
    pub(super) fn proves_live(&self) -> bool {
        match self {
            Self::Ids(_) => false,
            Self::Any(children) => children.iter().all(Self::proves_live),
            Self::All(children) => children.iter().any(Self::proves_live),
            Self::Complement(_) => true,
            _ => true,
        }
    }

    /// How many LEAVES this tree has, which is how many index walks building
    /// it costs. `prepare_query` bounds it by `MAX_BOOLEAN_LEAVES`.
    pub(super) fn leaves(&self) -> usize {
        match self {
            Self::Any(children) | Self::All(children) => {
                children.iter().map(Self::leaves).sum()
            }
            Self::Complement(child) => child.leaves(),
            _ => 1,
        }
    }
}

fn describe_encoded(predicate: &EncodedScalarFilter) -> String {
    match predicate {
        EncodedScalarFilter::Empty => "empty range".to_owned(),
        EncodedScalarFilter::Eq(_) => "= <value>".to_owned(),
        EncodedScalarFilter::Range { lower, upper } => {
            format!("range {}..{}", describe_bound(lower), describe_bound(upper))
        }
        EncodedScalarFilter::IsNull => "IS NULL".to_owned(),
        EncodedScalarFilter::IsMissing => "IS MISSING".to_owned(),
    }
}

fn describe_bound(bound: &EncodedBound) -> &'static str {
    match bound {
        EncodedBound::Included(_) => "[value]",
        EncodedBound::Excluded(_) => "(value)",
        EncodedBound::Unbounded => "*",
    }
}

/// A set of ids in whichever representation the budget allows: the plain Vec
/// while it stays smaller than the bitmap, the bitmap once it would not, and
/// `Overflow` when neither fits.
///
/// A sequence PAST the collection's span is dropped here rather than carried.
/// The only way one arrives is a caller's explicit `QueryFilter::Ids`, and a
/// sequence the collection has never issued can never be a member of
/// anything: dropping it answers "no row" -- which is the truth -- where the
/// bitmap path used to report the database CORRUPT for a caller's typo.
fn fit_ids(mut ids: Vec<u64>, budget: &MembershipBudget) -> QueryResult<MembershipSet> {
    ids.retain(|sequence| budget.in_span(*sequence));
    ids.sort_unstable();
    ids.dedup();
    if ids.len() <= budget.vec_cap {
        return Ok(MembershipSet::Ids(Arc::new(ids)));
    }
    if !budget.bitmap_viable {
        return Ok(MembershipSet::Overflow);
    }
    let mut bits = vec![0u8; budget.bitmap_bytes as usize];
    for sequence in ids {
        membership_bitmap_set(&mut bits, sequence)?;
    }
    Ok(MembershipSet::Bitmap(Arc::new(bits)))
}

/// One set as a bitmap of the collection's span, CONSUMING it: a set that
/// already is a bitmap and holds the only reference to it hands that
/// allocation over instead of being copied, which is what makes a fold in
/// place possible at all. `None` when a bitmap of that span does not fit the
/// budget.
fn into_bitmap(set: MembershipSet, budget: &MembershipBudget) -> QueryResult<Option<Vec<u8>>> {
    if !budget.bitmap_viable {
        return Ok(None);
    }
    Ok(Some(match set {
        MembershipSet::Bitmap(bits) => match Arc::try_unwrap(bits) {
            Ok(bits) => bits,
            Err(shared) => shared.as_ref().clone(),
        },
        MembershipSet::Ids(ids) => {
            let mut bits = vec![0u8; budget.bitmap_bytes as usize];
            for sequence in ids.iter() {
                // Out of span is dropped, not corrupt: see `fit_ids`.
                if budget.in_span(*sequence) {
                    membership_bitmap_set(&mut bits, *sequence)?;
                }
            }
            bits
        }
        _ => return Ok(None),
    }))
}

/// Set one bit of a bitmap that is already sized for the span, ignoring a
/// sequence outside it. The checked form is [`membership_bitmap_set`], which
/// an INDEX walk uses because a posting past the span really is corruption.
fn set_bit_in_span(bits: &mut [u8], budget: &MembershipBudget, sequence: u64) {
    if !budget.in_span(sequence) {
        return;
    }
    let index = (sequence - 1) as usize;
    if let Some(byte) = bits.get_mut(index / 8) {
        *byte |= 1 << (index % 8);
    }
}

/// The UNION of two sets, folded IN PLACE into whichever side already owns a
/// bitmap, and held against the shared byte budget while it happens.
///
/// An `Overflow` on either side is an overflow of the union: the members the
/// overflowed side would have contributed are unknown, and a union missing
/// them would answer "not a member" for rows that are one. That is why an
/// overflowed leaf refuses the whole disjunction rather than sending it to
/// the row path -- see `QueryFilter::Any`.
fn union_sets(
    left: MembershipSet,
    right: MembershipSet,
    budget: &MembershipBudget,
    live: &mut LiveBytes,
) -> QueryResult<MembershipSet> {
    let held = left.bytes() + right.bytes();
    let scratch = union_scratch(&left, &right, budget);
    live.reserve(scratch)?;
    let out = match (left, right) {
        (MembershipSet::Overflow, _) | (_, MembershipSet::Overflow) => MembershipSet::Overflow,
        (MembershipSet::Ids(left), MembershipSet::Ids(right)) => {
            let mut merged = Vec::with_capacity(left.len() + right.len());
            merged.extend_from_slice(&left);
            merged.extend_from_slice(&right);
            fit_ids(merged, budget)?
        }
        (left, right) => {
            // Whichever side is already a bitmap becomes the accumulator; the
            // other is OR'd into it without being materialised as a second
            // bitmap of its own. Nothing is cloned and no third span exists.
            let (accumulator, other) = match (&left, &right) {
                (MembershipSet::Bitmap(_), _) => (left, right),
                _ => (right, left),
            };
            let Some(mut bits) = into_bitmap(accumulator, budget)? else {
                live.release(scratch);
                return Ok(MembershipSet::Overflow);
            };
            match other {
                MembershipSet::Ids(ids) => {
                    for sequence in ids.iter() {
                        set_bit_in_span(&mut bits, budget, *sequence);
                    }
                }
                MembershipSet::Bitmap(other) => {
                    for (byte, mask) in bits.iter_mut().zip(other.iter()) {
                        *byte |= *mask;
                    }
                }
                _ => {
                    live.release(scratch);
                    return Ok(MembershipSet::Overflow);
                }
            }
            MembershipSet::Bitmap(Arc::new(bits))
        }
    };
    live.release(scratch + held);
    live.hold(&out)?;
    Ok(out)
}

/// The worst case a union can hold beyond its two operands: the merged Vec
/// and, when that Vec would not fit, the bitmap it is poured into; or one
/// bitmap of the span when either side already is one (which covers the copy
/// `into_bitmap` makes if the accumulator's allocation turns out to be
/// shared).
fn union_scratch(
    left: &MembershipSet,
    right: &MembershipSet,
    budget: &MembershipBudget,
) -> usize {
    match (left, right) {
        (MembershipSet::Ids(left), MembershipSet::Ids(right)) => {
            let entries = left.len() + right.len();
            let merged = entries.saturating_mul(std::mem::size_of::<u64>());
            if entries <= budget.vec_cap {
                merged
            } else {
                merged.saturating_add(budget.bitmap_bytes as usize)
            }
        }
        _ => budget.bitmap_bytes as usize,
    }
}

/// The INTERSECTION of two sets. Never larger than the smaller side, so it
/// needs no new budget headroom -- but an `Overflow` operand is still an
/// overflow: an unknown side cannot narrow anything.
fn intersect_sets(
    left: MembershipSet,
    right: MembershipSet,
    budget: &MembershipBudget,
    live: &mut LiveBytes,
) -> QueryResult<MembershipSet> {
    let held = left.bytes() + right.bytes();
    let scratch = intersect_scratch(&left, &right, budget);
    live.reserve(scratch)?;
    let out = match (left, right) {
        (MembershipSet::Overflow, _) | (_, MembershipSet::Overflow) => MembershipSet::Overflow,
        (MembershipSet::Ids(left), MembershipSet::Ids(right)) => {
            let mut kept = Vec::new();
            let (mut a, mut b) = (0usize, 0usize);
            while a < left.len() && b < right.len() {
                match left[a].cmp(&right[b]) {
                    Ordering::Less => a += 1,
                    Ordering::Greater => b += 1,
                    Ordering::Equal => {
                        kept.push(left[a]);
                        a += 1;
                        b += 1;
                    }
                }
            }
            MembershipSet::Ids(Arc::new(kept))
        }
        (MembershipSet::Ids(ids), other) | (other, MembershipSet::Ids(ids)) => {
            let mut kept = Vec::new();
            for sequence in ids.iter() {
                if other.contains(*sequence)? {
                    kept.push(*sequence);
                }
            }
            MembershipSet::Ids(Arc::new(kept))
        }
        (left, right) => {
            let Some(mut bits) = into_bitmap(left, budget)? else {
                live.release(scratch);
                return Ok(MembershipSet::Overflow);
            };
            let MembershipSet::Bitmap(other) = right else {
                live.release(scratch);
                return Ok(MembershipSet::Overflow);
            };
            for (byte, mask) in bits.iter_mut().zip(other.iter()) {
                *byte &= *mask;
            }
            MembershipSet::Bitmap(Arc::new(bits))
        }
    };
    live.release(scratch + held);
    live.hold(&out)?;
    Ok(out)
}

/// The worst case an intersection holds beyond its operands: the kept Vec,
/// which cannot outgrow the smaller side, or one bitmap of the span.
fn intersect_scratch(
    left: &MembershipSet,
    right: &MembershipSet,
    budget: &MembershipBudget,
) -> usize {
    match (left, right) {
        (MembershipSet::Ids(left), MembershipSet::Ids(right)) => {
            left.len().min(right.len()).saturating_mul(std::mem::size_of::<u64>())
        }
        (MembershipSet::Ids(ids), _) | (_, MembershipSet::Ids(ids)) => {
            ids.len().saturating_mul(std::mem::size_of::<u64>())
        }
        _ => budget.bitmap_bytes as usize,
    }
}

/// `universe` minus `inner`, as a bitmap of the collection's span.
///
/// The complement is ALWAYS a bitmap: it names the rows a predicate refuses,
/// which in a collection of any size is most of them, and a Vec of most of a
/// collection is the representation the bitmap exists to replace. So a span
/// whose bitmap does not fit `MEMBERSHIP_BITMAP_CAP_BYTES` has no complement
/// and the filter is refused -- named, not degraded.
///
/// The universe's own allocation is taken over rather than copied, so the
/// difference costs no bitmap of its own.
fn complement_set(
    universe: MembershipSet,
    inner: MembershipSet,
    budget: &MembershipBudget,
    live: &mut LiveBytes,
) -> QueryResult<MembershipSet> {
    let held = universe.bytes() + inner.bytes();
    let scratch = budget.bitmap_bytes as usize;
    live.reserve(scratch)?;
    let out = 'fold: {
        if matches!(universe, MembershipSet::Overflow) || matches!(inner, MembershipSet::Overflow) {
            break 'fold MembershipSet::Overflow;
        }
        let Some(mut bits) = into_bitmap(universe, budget)? else {
            break 'fold MembershipSet::Overflow;
        };
        match inner {
            MembershipSet::Ids(ids) => {
                for sequence in ids.iter() {
                    let index = (*sequence - 1) as usize;
                    if let Some(byte) = bits.get_mut(index / 8) {
                        *byte &= !(1 << (index % 8));
                    }
                }
            }
            MembershipSet::Bitmap(other) => {
                for (byte, mask) in bits.iter_mut().zip(other.iter()) {
                    *byte &= !mask;
                }
            }
            _ => break 'fold MembershipSet::Overflow,
        }
        MembershipSet::Bitmap(Arc::new(bits))
    };
    live.release(scratch + held);
    live.hold(&out)?;
    Ok(out)
}

/// Every entity SEQUENCE one point index holds a posting for.
///
/// This is the universe a `NOT` over a point predicate is taken against, and
/// it is the index's own postings rather than the collection's rows on
/// purpose: a row whose point is missing or unindexable has no posting, and
/// `NOT ST_DWithin(NULL, ...)` is unknown in SQL and returns no row either.
fn build_point_universe<C: FnMut() -> bool>(
    db: &Database,
    info: &IndexInfo,
    budget: &MembershipBudget,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<MembershipSet> {
    if !budget.bitmap_viable {
        return Ok(MembershipSet::Overflow);
    }
    let prefix = crate::index::spatial::point::posting_prefix(info.id);
    let mut bits = vec![0u8; budget.bitmap_bytes as usize];
    let Some(mut walk) = db.index_range(info, &prefix).map_err(QueryError::from)? else {
        return Ok(MembershipSet::Bitmap(Arc::new(bits)));
    };
    loop {
        meter.check_cancelled()?;
        meter.charge(WorkResource::SpatialPostings, 1)?;
        let sequence = {
            let Some((key, value)) = walk.peek_ref().map_err(Error::from).map_err(QueryError::from)?
            else {
                break;
            };
            if !key.starts_with(&prefix) {
                break;
            }
            let (_, sequence, _) = crate::index::spatial::point::decode_posting(&prefix, key, value)?;
            sequence
        };
        walk.step();
        membership_bitmap_set(&mut bits, sequence)?;
    }
    Ok(MembershipSet::Bitmap(Arc::new(bits)))
}

/// Every document SEQUENCE one TEXT index holds a norm for: the index's own
/// document universe.
///
/// This is the universe a `NOT` over a text match is taken against, and it is
/// the index's own record of which documents it covers rather than the
/// collection's live rows, for the same reason `build_point_universe` uses
/// the point postings. A row whose text field is NULL or MISSING contributes
/// no norm and no posting, so it is in neither the leaf nor its complement --
/// which is SQL's answer: `NOT (name @@ 'x')` over a NULL name is UNKNOWN,
/// and an UNKNOWN row is not returned. Taken over the live keyspace instead,
/// the complement returned it, and a tree that also held a scalar leaf (which
/// gets Kleene semantics from the nullish key being in no range) then
/// disagreed with itself.
///
/// Two tiers, in the order the index resolves them: the packed `0x7B` blocks
/// carry a presence bitmap for 256 documents each, and a `0x76` head row
/// OVERRIDES its block -- present with a length, absent when the value is
/// empty (the norm counterpart of the `tf = 0` posting tombstone). So the
/// blocks are laid down first and the heads are applied over them.
fn build_text_universe<C: FnMut() -> bool>(
    db: &Database,
    info: &IndexInfo,
    budget: &MembershipBudget,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<MembershipSet> {
    if !budget.bitmap_viable {
        return Ok(MembershipSet::Overflow);
    }
    use crate::index::text::{decode_norm, index_prefix, segments, segments_enabled, NORM};
    let segments_on = segments_enabled(db);
    let mut bits = vec![0u8; budget.bitmap_bytes as usize];

    // Tier one: the packed blocks.
    let block_prefix = index_prefix(segments::NORM_BLOCK, info.id);
    let mut entries: Vec<(usize, u32)> = Vec::new();
    let mut walk = db
        .store()?
        .range(&block_prefix)
        .map_err(Error::from)
        .map_err(QueryError::from)?;
    loop {
        meter.check_cancelled()?;
        meter.charge(WorkResource::TextPostings, 1)?;
        let block = {
            let Some((key, value)) = walk.peek_ref().map_err(Error::from).map_err(QueryError::from)?
            else {
                break;
            };
            if !crate::collections::has_prefix(key, &block_prefix) {
                break;
            }
            let mut at = block_prefix.len();
            let block = read_ordered(key, &mut at)?;
            if at != key.len() {
                return Err(corrupt_query("text norm block key"));
            }
            segments::decode_norm_block(value, &mut entries).map_err(QueryError::from)?;
            block
        };
        walk.step();
        for (slot, _) in &entries {
            let sequence = block
                .saturating_mul(segments::NORM_BLOCK_SPAN)
                .saturating_add(*slot as u64);
            set_bit_in_span(&mut bits, budget, sequence);
        }
    }

    // Tier two: the head rows, which override whatever the block said.
    let head_prefix = index_prefix(NORM, info.id);
    let mut walk = db
        .store()?
        .range(&head_prefix)
        .map_err(Error::from)
        .map_err(QueryError::from)?;
    loop {
        meter.check_cancelled()?;
        meter.charge(WorkResource::TextPostings, 1)?;
        let Some((key, value)) = walk.peek_ref().map_err(Error::from).map_err(QueryError::from)?
        else {
            break;
        };
        if !crate::collections::has_prefix(key, &head_prefix) {
            break;
        }
        let mut at = head_prefix.len();
        let sequence = read_ordered(key, &mut at)?;
        if at != key.len() {
            return Err(corrupt_query("text norm head key"));
        }
        let present = decode_norm(value, segments_on)
            .map_err(QueryError::from)?
            .is_some();
        walk.step();
        if present {
            set_bit_in_span(&mut bits, budget, sequence);
        } else if budget.in_span(sequence) {
            let index = (sequence - 1) as usize;
            if let Some(byte) = bits.get_mut(index / 8) {
                *byte &= !(1 << (index % 8));
            }
        }
    }
    Ok(MembershipSet::Bitmap(Arc::new(bits)))
}

/// Every entity SEQUENCE the collection currently holds a row for.
///
/// The universe a `NOT` is taken against when the leaf under it has no index
/// of its own to name one: an explicit semi-join set, or a text match. It is
/// a key-only walk of the primary keyspace -- no row is decoded -- and it is
/// charged as primary reads, which is the resource that bounds it.
///
/// A DELETED row has no primary record, so it is not in this universe and
/// therefore never in a complement: that is the whole reason the universe is
/// walked instead of assumed to be `1..=span`.
fn build_live_universe<C: FnMut() -> bool>(
    db: &Database,
    collection: CollectionId,
    budget: &MembershipBudget,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<MembershipSet> {
    if !budget.bitmap_viable {
        return Ok(MembershipSet::Overflow);
    }
    let prefix = prefix(0x40, collection);
    let mut bits = vec![0u8; budget.bitmap_bytes as usize];
    let mut walk = db
        .store()?
        .range(&prefix)
        .map_err(Error::from)
        .map_err(QueryError::from)?;
    loop {
        meter.check_cancelled()?;
        meter.charge(WorkResource::PrimaryReads, 1)?;
        let sequence = {
            let Some((key, _)) = walk.peek_ref().map_err(Error::from).map_err(QueryError::from)?
            else {
                break;
            };
            if !crate::collections::has_prefix(key, &prefix) {
                break;
            }
            crate::collections::row_id_after_prefix(key, prefix.len(), collection)?.sequence
        };
        walk.step();
        membership_bitmap_set(&mut bits, sequence)?;
    }
    Ok(MembershipSet::Bitmap(Arc::new(bits)))
}

/// Every entity SEQUENCE whose external key falls inside `predicate`.
///
/// The same walk `KeysCursor` makes when a key range drives, with no
/// candidate to build: a mapping entry is a present, real key, so an entry
/// this collects proves the predicate outright.
fn build_key_set<C: FnMut() -> bool>(
    db: &Database,
    collection: CollectionId,
    predicate: &EncodedScalarFilter,
    budget: &MembershipBudget,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<MembershipSet> {
    if matches!(predicate, EncodedScalarFilter::Empty) {
        return Ok(MembershipSet::Ids(Arc::new(Vec::new())));
    }
    let prefix = prefix(0x20, collection);
    let mut start = prefix.clone();
    if let Some(lower) = scalar_lower(predicate) {
        start.extend_from_slice(lower);
    }
    let mut ids: Vec<u64> = Vec::new();
    let mut bits: Option<Vec<u8>> = None;
    let mut walk = db
        .store()?
        .range(&start)
        .map_err(Error::from)
        .map_err(QueryError::from)?;
    loop {
        meter.check_cancelled()?;
        meter.charge(WorkResource::KeyPostings, 1)?;
        let sequence = {
            let Some((key, value)) = walk.peek_ref().map_err(Error::from).map_err(QueryError::from)?
            else {
                break;
            };
            if !crate::collections::has_prefix(key, &prefix) {
                break;
            }
            let suffix = &key[prefix.len()..];
            match scalar_key_position(predicate, suffix) {
                Ordering::Greater => break,
                Ordering::Less => None,
                Ordering::Equal => {
                    let mut at = 0;
                    let sequence = read_ordered(value, &mut at)?;
                    if at != value.len() || sequence == 0 {
                        return Err(corrupt_query("external-key mapping entry"));
                    }
                    Some(sequence)
                }
            }
        };
        walk.step();
        if let Some(sequence) = sequence {
            match bits.as_mut() {
                Some(bits) => membership_bitmap_set(bits, sequence)?,
                None if ids.len() == budget.vec_cap => {
                    if !budget.bitmap_viable {
                        return Ok(MembershipSet::Overflow);
                    }
                    let mut fresh = vec![0u8; budget.bitmap_bytes as usize];
                    for s in &ids {
                        membership_bitmap_set(&mut fresh, *s)?;
                    }
                    membership_bitmap_set(&mut fresh, sequence)?;
                    ids = Vec::new();
                    bits = Some(fresh);
                }
                None => ids.push(sequence),
            }
        }
    }
    if let Some(bits) = bits {
        return Ok(MembershipSet::Bitmap(Arc::new(bits)));
    }
    ids.sort_unstable();
    Ok(MembershipSet::Ids(Arc::new(ids)))
}

/// Every document SEQUENCE one text match admits, through the same term
/// merge a text driver walks.
///
/// The merge is the authority for `Any` and `All`: a document it emits holds
/// the terms the match mode requires, and nothing else is read. A `Phrase` is
/// refused while the filter is compiled, so it never reaches here.
fn build_text_set<C: FnMut() -> bool>(
    db: &Database,
    collection: CollectionId,
    prepared: &PreparedText,
    budget: &MembershipBudget,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<MembershipSet> {
    let plan = DriverPlan::Text {
        prepared: prepared.clone(),
        position: None,
    };
    let needs = CursorNeeds {
        row: false,
        scalar_key: false,
        key: false,
        edge: false,
    };
    let mut cursor = DriverCursor::new(
        db,
        collection,
        &plan,
        &[],
        needs,
        None,
        false,
        None,
        HashSet::new(),
        &[],
    )?;
    let mut ids: Vec<u64> = Vec::new();
    let mut bits: Option<Vec<u8>> = None;
    while let Some(candidate) = cursor.next(meter)? {
        let sequence = candidate.id.sequence;
        match bits.as_mut() {
            Some(bits) => membership_bitmap_set(bits, sequence)?,
            None if ids.len() == budget.vec_cap => {
                if !budget.bitmap_viable {
                    return Ok(MembershipSet::Overflow);
                }
                let mut fresh = vec![0u8; budget.bitmap_bytes as usize];
                for s in &ids {
                    membership_bitmap_set(&mut fresh, *s)?;
                }
                membership_bitmap_set(&mut fresh, sequence)?;
                ids = Vec::new();
                bits = Some(fresh);
            }
            None => ids.push(sequence),
        }
    }
    if let Some(bits) = bits {
        return Ok(MembershipSet::Bitmap(Arc::new(bits)));
    }
    ids.sort_unstable();
    Ok(MembershipSet::Ids(Arc::new(ids)))
}

/// Walk one compiled boolean filter into ONE membership set.
///
/// Every leaf is walked once and folded into the set above it, so a
/// disjunction of five ranges is five posting walks and one union, not five
/// predicates re-asked per candidate.
///
/// `live` is the byte budget the walk's SIMULTANEOUSLY live intermediates
/// share -- see [`LiveBytes`]. It is the caller's, not this walk's, so two
/// boolean filters of one query are bounded together rather than each getting
/// the whole promise to itself.
fn build_set_expr<C: FnMut() -> bool>(
    db: &Database,
    collection: CollectionId,
    expr: &SetExpr,
    live: &mut LiveBytes,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<MembershipSet> {
    let budget = MembershipBudget::new(db, collection)?;
    let set = build_set_expr_in(db, collection, expr, &budget, live, meter)?;
    meter.note_membership_bytes(live.peak as u64);
    Ok(set)
}

/// One leaf's set, held against the shared byte budget the moment it exists.
fn built_leaf(set: QueryResult<MembershipSet>, live: &mut LiveBytes) -> QueryResult<MembershipSet> {
    let set = set?;
    live.hold(&set)?;
    Ok(set)
}

fn build_set_expr_in<C: FnMut() -> bool>(
    db: &Database,
    collection: CollectionId,
    expr: &SetExpr,
    budget: &MembershipBudget,
    live: &mut LiveBytes,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<MembershipSet> {
    meter.check_cancelled()?;
    match expr {
        SetExpr::Scalar { info, predicate } => {
            built_leaf(build_scalar_range_set(db, info, predicate, meter), live)
        }
        SetExpr::Point { info, predicate } => {
            built_leaf(build_point_set(db, info, *predicate, meter), live)
        }
        SetExpr::Key {
            collection,
            predicate,
        } => built_leaf(build_key_set(db, *collection, predicate, budget, meter), live),
        SetExpr::Text(prepared) => built_leaf(
            build_text_set(db, collection, prepared, budget, meter),
            live,
        ),
        // The caller's own ids, already checked and span-filtered by
        // `checked_id_set` while the filter was compiled, so this is an
        // `Arc` clone and not a copy of the set.
        SetExpr::Ids(ids) => built_leaf(Ok(MembershipSet::Ids(ids.clone())), live),
        SetExpr::Any(children) => {
            let mut set = MembershipSet::Ids(Arc::new(Vec::new()));
            for child in children {
                let next = build_set_expr_in(db, collection, child, budget, live, meter)?;
                set = union_sets(set, next, budget, live)?;
                if matches!(set, MembershipSet::Overflow) {
                    return Ok(set);
                }
            }
            Ok(set)
        }
        SetExpr::All(children) => {
            let mut set: Option<MembershipSet> = None;
            for child in children {
                let next = build_set_expr_in(db, collection, child, budget, live, meter)?;
                set = Some(match set {
                    None => next,
                    Some(set) => intersect_sets(set, next, budget, live)?,
                });
                if matches!(set, Some(MembershipSet::Overflow)) {
                    return Ok(MembershipSet::Overflow);
                }
            }
            Ok(set.unwrap_or(MembershipSet::Ids(Arc::new(Vec::new()))))
        }
        SetExpr::Complement(child) => {
            let universe = match child.as_ref() {
                SetExpr::Point { info, .. } => {
                    built_leaf(build_point_universe(db, info, budget, meter), live)?
                }
                // A TEXT complement is taken over the text index's OWN
                // document universe -- the documents it holds a norm for --
                // so a row whose text field is null or missing lands in
                // neither the leaf nor its complement, exactly as the scalar
                // and point rules already had it.
                SetExpr::Text(prepared) => built_leaf(
                    build_text_universe(db, &prepared.info, budget, meter),
                    live,
                )?,
                // An explicit `Ids` set has NO index of its own, so there is
                // no posting universe to take: the live primary keyspace is
                // the only universe that exists for it, and `NOT IN (a set)`
                // over it means every row the collection currently holds and
                // the set does not name.
                SetExpr::Ids(_) => {
                    built_leaf(build_live_universe(db, collection, budget, meter), live)?
                }
                // A scalar or key complement is a union of ranges over the
                // same keyspace and is lowered to one while the filter is
                // compiled; a complement over a union or an intersection is
                // De Morgan's law, applied there too.
                _ => {
                    return Err(corrupt_query(
                        "a boolean complement reached the walk with no leaf universe",
                    ))
                }
            };
            let inner = build_set_expr_in(db, collection, child, budget, live, meter)?;
            complement_set(universe, inner, budget, live)
        }
    }
}

// ── per-hop node predicates (docs/GRAPH_CONTRACT.md §4.3) ─────────────────

/// How one node predicate of a traversal is answered for one visited node.
///
/// Every arm is index-side. §4.3's rule is absolute: "a traversal never reads
/// a row for a predicate on a covered field", so there is no row arm here and
/// no fallback that would quietly become one.
#[derive(Clone, Debug)]
enum NodeProbe {
    /// A scalar EQUALITY. The key `value || sequence` is known before the
    /// walk starts, so one point read of the index answers one node --
    /// exactly `scalar_eq_posting_matches`, which is how a non-driving
    /// equality filter already answers a candidate. No set is built and none
    /// can overflow.
    ScalarEq { info: IndexInfo, expected: Vec<u8> },
    /// A scalar RANGE or a POINT predicate: one [`MembershipSet`] walked
    /// once, then one binary search or one bit per visited node.
    Set {
        info: IndexInfo,
        predicate: NodeSetPredicate,
        set: MembershipSet,
    },
}

#[derive(Clone, Debug)]
enum NodeSetPredicate {
    Scalar(EncodedScalarFilter),
    Point(PointFilter),
}

/// The compiled `node_where` of one traversal: what each predicate is, and
/// the sets they are answered from once those are built.
#[derive(Clone, Debug)]
pub(super) struct NodeGate {
    probes: Vec<NodeProbe>,
}

impl NodeGate {
    /// Compile one traversal's `node_where`, refusing every filter kind that
    /// an index cannot answer without opening the row.
    ///
    /// Accepted: `Scalar` `Eq` and `Range`, `Point` `Bbox` and `Radius`.
    ///
    /// Refused, each with its own reason: `Scalar` `IsNull` and `IsMissing`,
    /// because NULL and MISSING share one nullish index key and only the row
    /// tells them apart; `Geometry`, because a geometry posting's box is a
    /// candidate test and the refine reads the row; `Text`, because a term
    /// merge is a stream, not a set; `JsonEq`, which has no index at all;
    /// `Key`, which is the mapping keyspace and not a predicate on a node;
    /// and `Graph`, because a traversal inside a traversal is not an atomic
    /// this engine has.
    pub(super) fn compile(db: &Database, filters: &[QueryFilter<'_>]) -> QueryResult<Self> {
        if filters.len() > MAX_FILTERS {
            return Err(invalid_query(
                "a traversal's node predicates exceed the filter limit",
            ));
        }
        let mut probes = Vec::with_capacity(filters.len());
        for filter in filters {
            probes.push(match filter {
                QueryFilter::Scalar { index, predicate } => {
                    let info = db.index_info_cached(*index)?;
                    if info.family != IndexFamily::Scalar {
                        return Err(invalid_query(
                            "a traversal node predicate requires a scalar index",
                        ));
                    }
                    if info.state != IndexState::Ready {
                        return Err(invalid_query("query index is not ready"));
                    }
                    match predicate {
                        ScalarFilter::Eq(value) => NodeProbe::ScalarEq {
                            expected: encode_scalar_value(&info.kind, *value)?,
                            info,
                        },
                        ScalarFilter::Range { .. } => {
                            let predicate = NodeSetPredicate::Scalar(compile_scalar_filter(
                                &info.kind,
                                predicate,
                            )?);
                            NodeProbe::Set {
                                info,
                                predicate,
                                set: MembershipSet::Unbuilt,
                            }
                        }
                        ScalarFilter::IsNull | ScalarFilter::IsMissing => {
                            return Err(invalid_query(
                                "a traversal node predicate cannot be IS NULL or IS MISSING: NULL and MISSING share one nullish index key and only the row tells them apart, and GRAPH_CONTRACT 4.3 forbids a row read for a per-hop predicate",
                            ))
                        }
                    }
                }
                QueryFilter::Point { index, predicate } => {
                    let info = db.index_info_cached(*index)?;
                    if info.family != IndexFamily::SpatialPoint {
                        return Err(invalid_query(
                            "a traversal node predicate requires a spatial-point index",
                        ));
                    }
                    if info.state != IndexState::Ready {
                        return Err(invalid_query("query index is not ready"));
                    }
                    crate::index::spatial::point::descriptor(&info)?;
                    if let PointFilter::Radius { radius_metres, .. } = predicate {
                        if !radius_metres.is_finite() || *radius_metres < 0.0 {
                            return Err(invalid_query(
                                "point radius must be finite and non-negative",
                            ));
                        }
                    }
                    NodeProbe::Set {
                        info,
                        predicate: NodeSetPredicate::Point(*predicate),
                        set: MembershipSet::Unbuilt,
                    }
                }
                QueryFilter::Geometry { .. } => {
                    return Err(invalid_query(
                        "a traversal node predicate cannot be a geometry predicate: a geometry posting's box is a candidate test and the refine reads the row, which GRAPH_CONTRACT 4.3 forbids per hop",
                    ))
                }
                QueryFilter::Text { .. } => {
                    return Err(invalid_query(
                        "a traversal node predicate cannot be a text search: a term merge is a document stream, not a membership set",
                    ))
                }
                QueryFilter::JsonEq { .. } => {
                    return Err(invalid_query(
                        "a traversal node predicate cannot be a JSON equality: it has no index and is answered from the row",
                    ))
                }
                QueryFilter::Key { .. } => {
                    return Err(invalid_query(
                        "a traversal node predicate cannot be a key range: the external-key mapping is a driver's keyspace, not a predicate on a node",
                    ))
                }
                QueryFilter::Graph(_) => {
                    return Err(invalid_query(
                        "a traversal node predicate cannot be another traversal: a nested traversal is not an atomic this engine has",
                    ))
                }
                // A boolean is a membership set built over the WHOLE
                // collection; a per-hop predicate is asked of one visited
                // node at a time, and GRAPH_CONTRACT 4.3 bounds a traversal
                // by its own frontier rather than by a scan of the rows it
                // never reaches.
                QueryFilter::Any(_)
                | QueryFilter::All(_)
                | QueryFilter::Not(_)
                | QueryFilter::Ids(_) => {
                    return Err(invalid_query(
                        "a traversal node predicate cannot be a boolean set: a union, a complement or a semi-join set is built over the whole collection, which is work a bounded traversal must not do per hop",
                    ))
                }
            });
        }
        Ok(Self { probes })
    }

    pub(super) fn is_empty(&self) -> bool {
        self.probes.is_empty()
    }

    /// The gate a filter holds while its own is lifted out to be built.
    pub(super) fn empty() -> Self {
        Self { probes: Vec::new() }
    }

    /// True once every set this gate needs has been walked.
    pub(super) fn is_built(&self) -> bool {
        !self
            .probes
            .iter()
            .any(|probe| matches!(probe, NodeProbe::Set { set: MembershipSet::Unbuilt, .. }))
    }

    /// Walk every not-yet-built set once. Idempotent, so a prepared query
    /// pays for it on its first page and every later page finds it there.
    ///
    /// A set that overflows its budget is an ERROR here, not a fallback: the
    /// row path a non-driving filter falls back to is exactly what §4.3
    /// forbids inside a traversal, and emulating the predicate by reading
    /// rows would be the eighth law's "emulated" rather than "refused".
    pub(super) fn build<C: FnMut() -> bool>(
        &mut self,
        db: &Database,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<()> {
        for probe in &mut self.probes {
            let NodeProbe::Set {
                info,
                predicate,
                set,
            } = probe
            else {
                continue;
            };
            if !matches!(set, MembershipSet::Unbuilt) {
                continue;
            }
            *set = match predicate {
                NodeSetPredicate::Scalar(predicate) => {
                    build_scalar_range_set(db, info, predicate, meter)?
                }
                NodeSetPredicate::Point(predicate) => build_point_set(db, info, *predicate, meter)?,
            };
            if matches!(set, MembershipSet::Overflow) {
                // A NAMED budget, not prose: the resource is the postings
                // counter the walk that overflowed was charging, so a caller
                // can match on it the way it matches on every other
                // `BudgetExceeded`. The limit is the entry cap the walk
                // stopped at (`MEMBERSHIP_SET_CAP`, the ceiling that applies
                // whenever a bitmap is not viable, which is the only way a
                // set reaches `Overflow`), and the attempt is the entry that
                // passed it. GRAPH_CONTRACT 4.3 forbids falling back to a
                // row read per hop, so this is a refusal and not a plan.
                return Err(QueryError::BudgetExceeded {
                    resource: match predicate {
                        NodeSetPredicate::Scalar(_) => WorkResource::ScalarPostings,
                        NodeSetPredicate::Point(_) => WorkResource::SpatialPostings,
                    },
                    limit: MEMBERSHIP_SET_CAP as u64,
                    attempted: MEMBERSHIP_SET_CAP as u64 + 1,
                });
            }
        }
        Ok(())
    }

    /// Does one visited node satisfy every predicate?
    ///
    /// A node in another collection than a predicate's index REFUSES that
    /// predicate: a traversal is inter-collection (§2.1) and a row of another
    /// collection does not have the field, which is the same answer a missing
    /// field already gets.
    pub(super) fn admits<C: FnMut() -> bool>(
        &self,
        db: &Database,
        id: EntityId,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<bool> {
        for probe in &self.probes {
            let passes = match probe {
                NodeProbe::ScalarEq { info, expected } => {
                    info.collection == id.collection
                        && scalar_eq_posting_matches(db, info, expected, id, meter)?
                }
                NodeProbe::Set { info, set, .. } => {
                    info.collection == id.collection
                        && match set {
                            MembershipSet::Ids(ids) => ids.binary_search(&id.sequence).is_ok(),
                            MembershipSet::Bitmap(bits) => {
                                membership_bitmap_contains(bits, id.sequence)
                            }
                            _ => {
                                return Err(corrupt_query(
                                    "a traversal node predicate was tested before its set was built",
                                ))
                            }
                        }
                }
            };
            if !passes {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// One line per node predicate, for `EXPLAIN`.
    pub(super) fn describe(&self) -> Vec<String> {
        self.probes
            .iter()
            .map(|probe| match probe {
                NodeProbe::ScalarEq { info, .. } => {
                    format!("{}.{} = <value> via index posting", info.name, info.field)
                }
                NodeProbe::Set { info, predicate, set } => {
                    let shape = match predicate {
                        NodeSetPredicate::Scalar(_) => "range",
                        NodeSetPredicate::Point(PointFilter::Bbox(_)) => "bbox",
                        NodeSetPredicate::Point(PointFilter::Radius { .. }) => "radius",
                    };
                    let built = match set {
                        MembershipSet::Ids(ids) => format!("membership set, {} ids", ids.len()),
                        MembershipSet::Bitmap(bits) => {
                            format!("membership bitmap, {} bytes", bits.len())
                        }
                        _ => "membership set, unbuilt".to_owned(),
                    };
                    format!("{}.{} {shape} via {built}", info.name, info.field)
                }
            })
            .collect()
    }
}

/// [`NodeGate`] for a caller that is not a query: `Database::traverse_bfs`,
/// the traversal atomic itself, which has no `QueryBudget` and no work meter.
///
/// The sets are still bounded -- [`MembershipBudget`] is the same memory rule
/// whoever walks them -- and the posting reads are still counted, into a
/// meter that is thrown away. What is absent is a CEILING on those counts,
/// because the atomic's bounds are its own (`max_visited`, `max_edges`,
/// `result_limit`), not a page's.
pub(crate) struct StandaloneNodeGate {
    gate: NodeGate,
}

/// The atomic's error type, with nothing lost on the way: a budget refusal
/// stays a budget refusal with its resource named (`Error::BudgetExceeded`),
/// so a node membership set that outgrows its memory budget is as
/// machine-readable through `Database::traverse_bfs` as it is through a
/// prepared query. `From<Error> for QueryError` carries it back unchanged.
fn as_database_error(error: QueryError) -> Error {
    match error {
        QueryError::Database(error) => error,
        QueryError::Cancelled => invalid("graph query cancelled"),
        QueryError::BudgetExceeded {
            resource,
            limit,
            attempted,
        } => Error::BudgetExceeded {
            resource,
            limit,
            attempted,
        },
    }
}

impl StandaloneNodeGate {
    pub(crate) fn new(db: &Database, filters: &[QueryFilter<'_>]) -> crate::collections::Result<Self> {
        let mut never = || false;
        let mut meter = WorkMeter::new(QueryBudget::unlimited(), &mut never);
        let mut gate = NodeGate::compile(db, filters).map_err(as_database_error)?;
        gate.build(db, &mut meter).map_err(as_database_error)?;
        Ok(Self { gate })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.gate.is_empty()
    }

    pub(crate) fn admits(&self, db: &Database, id: EntityId) -> crate::collections::Result<bool> {
        let mut never = || false;
        let mut meter = WorkMeter::new(QueryBudget::unlimited(), &mut never);
        self.gate
            .admits(db, id, &mut meter)
            .map_err(as_database_error)
    }
}
