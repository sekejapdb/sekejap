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
    Ids(Vec<u64>),
    /// One bit per sequence in `1..=collection_span`, set for every entity
    /// SEQUENCE the walk proved. Chosen once the Vec representation would be
    /// bigger than this: unlike the Vec, setting a bit costs no sort and no
    /// allocation growth once the bitmap is sized, so a wide predicate (e.g. a
    /// whole decade of `born`) is one linear pass, no CPU cost from the
    /// postings count once past the initial allocation.
    Bitmap(Vec<u8>),
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

/// A bitmap large enough to need more than this many bytes is not a viable
/// [`MembershipSet::Bitmap`]: `RUN_BYTES` is the same per-page memory
/// currency the Vec cap above is drawn from. One bit per sequence, so a
/// collection whose span exceeds `8 * RUN_BYTES` sequences (about 67
/// million entities) never gets a bitmap here, whatever the range's own
/// selectivity.
const MEMBERSHIP_BITMAP_CAP_BYTES: usize = RUN_BYTES;

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
        None => return Ok(MembershipSet::Ids(Vec::new())),
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
        return Ok(MembershipSet::Bitmap(bits));
    }
    ids.sort_unstable();
    Ok(MembershipSet::Ids(ids))
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
            bitmap_bytes,
            bitmap_viable,
            vec_cap,
        })
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
        return Ok(MembershipSet::Bitmap(bits));
    }
    ids.sort_unstable();
    Ok(MembershipSet::Ids(ids))
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
