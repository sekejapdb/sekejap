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
