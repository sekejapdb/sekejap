//! The candidate drivers: the plan a driver walks (`DriverPlan`), the bounded
//! graph traversals, the carried-key candidate, every cursor's state, and the
//! `DriverCursor` keyspace helpers. See docs/QL_CONTRACT.md,
//! "Execution pipeline", and docs/GRAPH_CONTRACT.md section 4.
use super::*;

/// The key ONE driver's own walk is sorted by, resolved at prepare time so
/// the ranking never has to ask the plan again, per row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DriverKey {
    /// The entity id and nothing else. The primary cursor walks it, the text
    /// merge ascends by document, and a graph result is sorted before it
    /// leaves the traversal.
    Entity,
    /// `value || sequence` of one scalar index's postings.
    Scalar(IndexId),
    /// `cell || sequence` of one spatial index's postings.
    Cell(IndexId),
    /// `(level, cell, sequence)` of one geometry index's first admitted posting.
    GeomCell(IndexId),
    /// The external key bytes of one mapping entry.
    Key,
}

#[derive(Clone, Debug)]
pub(super) enum DriverPlan {
    Entities,
    Scalar {
        info: IndexInfo,
        predicate: EncodedScalarFilter,
        position: Option<usize>,
    },
    Graph {
        position: usize,
    },
    Spatial {
        info: IndexInfo,
        predicate: PointFilter,
        position: usize,
        ranges: Vec<(u64, u64)>,
        fallback_world: bool,
    },
    /// Outward-ring walk yielding postings in ascending geodesic distance.
    /// `radius_cap` is a same-index, same-centre radius filter this walk
    /// certifies; `certifies` is that filter's position.
    Nearest {
        info: IndexInfo,
        center: Point,
        radius_cap: Option<f64>,
        certifies: Option<usize>,
    },
    /// A geometry index walk. The posting `BoxF` admits a candidate; the
    /// predicate is not certified by this driver -- it is a live T1 atomic
    /// (`docs/QL_CONTRACT.md` §4.4) refined against the row by
    /// `filters_match`'s `CompiledFilter::Geometry` arm, which reads
    /// `CompiledFilter` directly rather than through this plan.
    Geometry {
        info: IndexInfo,
        position: usize,
        ranges: Vec<GeomRange>,
        query_bbox: BoxF,
        fallback_world: bool,
    },
    Text {
        prepared: PreparedText,
        position: Option<usize>,
    },
    ExactVector {
        info: IndexInfo,
    },
    QuantizedVector {
        info: IndexInfo,
    },
    /// Walk the external-key mapping keyspace. `predicate` is a range over
    /// the raw key bytes (`Empty`/`Range` only -- see `CompiledFilter::Key`);
    /// `position` is the filter position it certifies, or `None` when the
    /// request named no `QueryFilter::Key` at all and the whole collection's
    /// keys are walked.
    Keys {
        predicate: EncodedScalarFilter,
        position: Option<usize>,
    },
}

/// One Hilbert range of a geometry cover, at one ladder level. Walked in
/// on-disk key order: `LEVEL_WORLD` (0), then `LEVEL_COARSE` (8), then
/// `LEVEL_FINE` (12), because the posting key is `tag||index||level||cell||seq`.
#[derive(Clone, Debug)]
pub(super) struct GeomRange {
    pub(super) level: u8,
    pub(super) lo: u64,
    pub(super) hi: u64,
}

impl DriverPlan {
    pub(super) fn diagnostic(&self) -> QueryDriver {
        match self {
            Self::Entities => QueryDriver::Entities,
            Self::Scalar { info, .. } => QueryDriver::Scalar(info.id),
            Self::Graph { position } => QueryDriver::Graph { filter: *position },
            Self::Spatial {
                info,
                fallback_world,
                ..
            } => QueryDriver::Spatial {
                index: info.id,
                fallback_world: *fallback_world,
            },
            Self::Nearest { info, .. } => QueryDriver::Nearest { index: info.id },
            Self::Geometry {
                info,
                fallback_world,
                ..
            } => QueryDriver::Geometry {
                index: info.id,
                fallback_world: *fallback_world,
            },
            Self::Text { prepared, .. } => QueryDriver::Text(prepared.info.id),
            Self::ExactVector { info } => QueryDriver::ExactVector(info.id),
            Self::QuantizedVector { info } => QueryDriver::QuantizedVector(info.id),
            Self::Keys { .. } => QueryDriver::Keys,
        }
    }
}

/// The key one driver's own walk is ordered by.
///
/// Every driver the planner can choose for a driver-ordered query has one,
/// and it is a key the walk is already standing on: the primary cursor is in
/// id order, a scalar posting range is `value || sequence`, the text merge
/// ascends by document and refuses a posting that does not advance, a
/// traversal hands its ids over sorted, and a spatial walk ascends by
/// `(cell, sequence)` across a sorted, merged range list.
///
/// A vector driver has none. Its entries are in locator order and its answer
/// is in distance order, and driver order asked for neither -- an approximate
/// shortlist is a RANKING wearing a walk's clothes, and returning it as
/// though its walk were the answer's order would be a page that cannot be
/// resumed and an order nobody asked for. It is unreachable as the planner
/// stands (no filter drives a vector index, and `order_driver` hands driver
/// order the entity cursor), so this is the rule written down where the other
/// cases are rather than left for a future driver to rediscover.
pub(super) fn driver_key(driver: &DriverPlan) -> QueryResult<DriverKey> {
    match driver {
        DriverPlan::Entities | DriverPlan::Text { .. } | DriverPlan::Graph { .. } => {
            Ok(DriverKey::Entity)
        }
        DriverPlan::Scalar { info, .. } => Ok(DriverKey::Scalar(info.id)),
        DriverPlan::Spatial { info, .. } => Ok(DriverKey::Cell(info.id)),
        DriverPlan::Geometry { info, .. } => Ok(DriverKey::GeomCell(info.id)),
        DriverPlan::Keys { .. } => Ok(DriverKey::Key),
        DriverPlan::ExactVector { .. }
        | DriverPlan::QuantizedVector { .. }
        | DriverPlan::Nearest { .. } => Err(invalid_query(
            "driver order needs a driver that walks in an order of its own",
        )),
    }
}

/// True when `QueryOrder::Distance` on `order_index` can itself be the
/// candidate driver: every spatial filter is a same-index, same-centre
/// radius (the common "nearest k within R" shape). A bbox, or a radius
/// around a different point, is not monotone in distance from `center`, so
/// stopping the ordered walk at `k` would drop in-filter rows that sit
/// farther than k out-of-filter neighbours.
pub(super) fn distance_can_drive(order_index: IndexId, center: Point, filters: &[CompiledFilter]) -> bool {
    filters.iter().all(|filter| match filter {
        CompiledFilter::Point { info, predicate } if info.id == order_index => {
            matches!(predicate, PointFilter::Radius { center: c, .. } if *c == center)
        }
        CompiledFilter::Point { .. } => false,
        _ => true,
    })
}

pub(super) fn nearest_plan(info: &IndexInfo, center: Point, filters: &[CompiledFilter]) -> DriverPlan {
    let mut radius_cap = None;
    let mut certifies = None;
    for (position, filter) in filters.iter().enumerate() {
        if let CompiledFilter::Point {
            info: filter_info,
            predicate: PointFilter::Radius {
                center: filter_center,
                radius_metres,
            },
        } = filter
        {
            if filter_info.id == info.id && *filter_center == center {
                radius_cap = Some(*radius_metres);
                certifies = Some(position);
                break;
            }
        }
    }
    DriverPlan::Nearest {
        info: info.clone(),
        center,
        radius_cap,
        certifies,
    }
}

/// Can every filter of a `QueryOrder::Distance` query be decided WITHOUT the
/// primary row once the nearest walk drives?
///
/// That is the whole condition for keeping the ordered walk in front. The
/// walk hands over `(id, point, distance)` and stops at `k`; what it must not
/// do is turn each of those into a point-get, because then the plan is paying
/// per candidate what the equality plan pays per match, over more candidates.
///
///   * a scalar EQUALITY that is not driving is answered from its own posting
///     (`scalar_eq_posting_matches`) -- one index key, no row. That is the
///     same authority `order_index_drives_better` already leans on;
///   * a POINT filter here is a same-index, same-centre radius and nothing
///     else, because `distance_can_drive` is checked first, and `nearest_plan`
///     certifies exactly that filter from the walk's own radius cap.
///
/// Every other filter -- a scalar RANGE (whose set may overflow back to row
/// reads), a JSON equality, a geometry refinement, a text score, a traversal
/// -- reads or may read the row, so the walk would pay for the rows it
/// rejects as well as the ones it keeps. Those keep the equality driver.
pub(super) fn nearest_tests_filters_index_side(filters: &[CompiledFilter]) -> bool {
    filters.iter().all(|filter| match filter {
        CompiledFilter::Scalar { predicate, .. } => {
            matches!(predicate, EncodedScalarFilter::Eq(_))
        }
        CompiledFilter::Point { .. } => true,
        _ => false,
    })
}

/// The ratio the crossover below is priced at: one primary row read against
/// one examined spatial posting. A row read is a random point-get into the
/// primary tree plus a dense-v3 walk to the point field; an examined posting
/// is a sequential step in a leaf the ring cover already positioned, plus one
/// geodesic distance. Four is the conservative end of what the 50K battery
/// shows (about 2.9 us against about 0.6 us) -- conservative because a lower
/// ratio moves the crossover UP, and a crossover that is too high only leaves
/// the equality driving a query it used to drive anyway.
const NEAREST_DRIVE_ROW_RATIO: u64 = 4;

/// Floor and ceiling on how many of the equality's postings the probe reads.
/// The floor keeps a tiny collection from deciding on three postings; the
/// ceiling keeps the probe itself from becoming the cost it is measuring.
const NEAREST_DRIVE_PROBE_FLOOR: u64 = 64;

const NEAREST_DRIVE_PROBE_CEILING: u64 = 1024;

/// Should an equality filter plus a DISTANCE order under a LIMIT be driven by
/// the nearest walk rather than by the equality's postings?
///
/// Driving from the equality hands candidates over in entity order, which is
/// not the order asked for, so the distance of every match has to be read out
/// of its primary row and the whole set sorted before the first row can be
/// returned: `m` row reads for a `k`-row answer, and `m` is the filter's
/// cardinality, not `k`. Driving from the spatial index inverts it -- the walk
/// is already in distance order, the LIMIT becomes a stop condition, the point
/// falls out of the posting the cursor is standing on, and the equality is a
/// membership probe against its own posting. The price is walking over the
/// rows the equality rejects: about `k / acceptance` postings for `k` answers.
///
/// So the two plans meet where `m * ratio == k * n / m`, with `n` the
/// collection's span and `ratio` the row-to-posting price above: `m` around
/// `sqrt(k * n / ratio)`, which is 353 postings for `k = 10` over 50,000 rows.
/// Below it the equality is narrow enough that reading its rows beats crossing
/// the space between them; above it the walk wins, and by a widening margin --
/// at one row in eight of a 50,000-row collection the equality plan reads
/// 6,250 rows to answer with ten.
///
/// The probe is that crossover, walked. It reads the equality's own postings
/// and stops the moment it has seen enough of them to decide, so a broad
/// filter costs a bounded prefix and a narrow one costs its whole (small)
/// posting list -- the same postings its driver is about to walk anyway.
/// Counting is exact in the direction that matters: "at least `crossover`
/// members" is the only claim made, never an estimate of how many more.
pub(super) fn nearest_drives_better(
    db: &Database,
    limit: usize,
    filter: &IndexInfo,
    expected: &[u8],
) -> QueryResult<bool> {
    let span = db
        .collection_span(filter.collection)
        .map_err(QueryError::from)?;
    let crossover = (limit as u64)
        .saturating_mul(span)
        .saturating_div(NEAREST_DRIVE_ROW_RATIO)
        .isqrt()
        .clamp(NEAREST_DRIVE_PROBE_FLOOR, NEAREST_DRIVE_PROBE_CEILING);
    let prefix = scalar_prefix(filter.id);
    let mut start = prefix.clone();
    start.extend_from_slice(expected);
    let Some(mut walk) = db.index_range(filter, &start).map_err(QueryError::from)? else {
        return Ok(false);
    };
    let mut seen = 0u64;
    while seen < crossover {
        {
            let Some((key, _)) = walk.peek_ref().map_err(Error::from).map_err(QueryError::from)?
            else {
                break;
            };
            if !key.starts_with(&prefix) {
                break;
            }
            let suffix = &key[prefix.len()..];
            let value_len = scalar_key::width(&filter.kind, suffix)?;
            let value = suffix
                .get(..value_len)
                .ok_or_else(|| corrupt_query("truncated scalar value key"))?;
            if value != expected {
                break;
            }
        }
        walk.step();
        seen += 1;
    }
    Ok(seen >= crossover)
}

/// How many postings of the ORDER index the planner walks before deciding.
const ORDER_DRIVE_PROBE: u64 = 64;

/// The density the probe must find: one accepted row per this many walked.
/// It is the crossover of the two plans -- see [`order_index_drives_better`].
const ORDER_DRIVE_MIN_DENSITY: u64 = 16;

/// One accepted row per this many of the collection's sequences: the density
/// at which the page-order compact scan beats the filter-driven plan under an
/// APPROXIMATE VECTOR order. See [`approximate_scan_drives`].
const APPROX_SCAN_MIN_DENSITY: u64 = 16;

/// The most postings [`approximate_scan_drives`] will walk before it gives up
/// on deciding. A collection wide enough that even one row in
/// [`APPROX_SCAN_MIN_DENSITY`] of it is more postings than this keeps the plan
/// it had; the probe never becomes a pass over a large index to decide how to
/// read it.
const APPROX_SCAN_PROBE_CAP: u64 = 4_096;

/// Should a FILTERED approximate vector order be driven from the quantized
/// index -- one page-order compact scan with the filters tested index-side --
/// rather than from the filter?
///
/// The two plans differ in what they pay per row. Driving from the FILTER
/// hands over the rows it accepts and point-gets each one's compact entry: one
/// root-to-leaf descent into the `0x79` table per MATCH. Driving from the
/// quantized index reads every compact entry of the collection in page order,
/// one sequential leaf step each, and decodes the int8 codes only for the
/// entries the filters' membership sets admit. So the filter plan costs
/// matches x random-read and the scan costs span x sequential-step plus
/// matches x score, and the two meet around one accepted row in
/// [`APPROX_SCAN_MIN_DENSITY`] of the collection -- the same crossover, and
/// for the same reason, that [`ORDER_DRIVE_MIN_DENSITY`] names.
///
/// Every filter has to be one the scan can answer from an index, because a
/// filter that needs the row would put a primary read back in front of every
/// admitted entry: a scalar equality or range (its postings become a
/// `MembershipSet`), a point filter with a bounded cover (so does its), or a
/// folded position, which is already part of another one's predicate.
///
/// The density is measured, not guessed, and the measurement is bounded: the
/// first scalar filter's postings are walked until they reach the count the
/// crossover needs, and no further. Where that count is more postings than
/// [`APPROX_SCAN_PROBE_CAP`], nothing is walked at all and the answer is no,
/// so the plan for a collection too large to decide cheaply is exactly the
/// plan it had before this existed.
pub(super) fn approximate_scan_drives(
    db: &Database,
    collection: CollectionId,
    filters: &[CompiledFilter],
) -> QueryResult<bool> {
    if filters.is_empty() {
        return Ok(false);
    }
    let mut scalar = None;
    for filter in filters {
        match filter {
            CompiledFilter::Folded { .. } => {}
            CompiledFilter::Scalar { info, predicate, .. } => {
                if !matches!(predicate, EncodedScalarFilter::Range { .. })
                    && !matches!(predicate, EncodedScalarFilter::Eq(value) if value.as_slice() != NULLISH_SCALAR_KEY)
                {
                    return Ok(false);
                }
                if scalar.is_none() {
                    scalar = Some((info, predicate));
                }
            }
            // A world-wide cover builds no set (see `MembershipSet`), so the
            // scan could not answer it index-side.
            CompiledFilter::Point { predicate, .. } => {
                if !point_ranges(*predicate).is_ok_and(|(_, world)| !world) {
                    return Ok(false);
                }
            }
            _ => return Ok(false),
        }
    }
    // A point filter alone says nothing about density here, and its cover walk
    // is not a count. Only a scalar filter is probed, so a query filtered
    // ONLY by a point keeps the plan it had.
    let Some((info, predicate)) = scalar else {
        return Ok(false);
    };
    let span = db.collection_span(collection).map_err(QueryError::from)?;
    let needed = span / APPROX_SCAN_MIN_DENSITY + 1;
    if needed > APPROX_SCAN_PROBE_CAP {
        return Ok(false);
    }
    scalar_postings_reach(db, info, predicate, needed)
}

/// Does this predicate's posting walk reach `needed` proved sequences?
///
/// The same forward walk `build_scalar_range_set` makes -- open at the
/// predicate's lower bound, stop at the first key past its upper one, skip the
/// nullish key a NULL and a MISSING share -- stopped as soon as the answer is
/// known instead of collecting anything. Nothing is held: this counts.
///
/// The step budget is the second bound. A predicate whose lower bound is
/// EXCLUDED opens on postings it will not count, and they are postings of one
/// value rather than of the range, so the walk is allowed that many steps
/// beyond `needed` and then stops whatever it has seen.
fn scalar_postings_reach(
    db: &Database,
    info: &IndexInfo,
    predicate: &EncodedScalarFilter,
    needed: u64,
) -> QueryResult<bool> {
    let prefix = scalar_prefix(info.id);
    let mut start = prefix.clone();
    if let Some(lower) = scalar_lower(predicate) {
        start.extend_from_slice(lower);
    }
    let Some(mut walk) = db.index_range(info, &start).map_err(QueryError::from)? else {
        return Ok(false);
    };
    let mut seen = 0u64;
    let mut steps = 0u64;
    let ceiling = needed.saturating_add(APPROX_SCAN_PROBE_CAP);
    while seen < needed && steps < ceiling {
        let counted = {
            let Some((key, _)) = walk.peek_ref().map_err(Error::from).map_err(QueryError::from)?
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
                Ordering::Less => false,
                Ordering::Equal => encoded != NULLISH_SCALAR_KEY,
            }
        };
        walk.step();
        steps += 1;
        if counted {
            seen += 1;
        }
    }
    Ok(seen >= needed)
}

/// Should an equality filter plus a scalar order on a DIFFERENT index, under a
/// LIMIT, be driven from the ORDER index rather than from the filter?
///
/// Driving from the equality posting hands candidates over in entity-id order,
/// which is not the order asked for. Every match then has to give up its order
/// value, and that value is not in the posting being walked -- it is in the
/// primary row, one point-get and one dense-v3 walk per match -- and the whole
/// set has to be sorted before the first row can be returned.
///
/// Driving from the ORDER index inverts that. The walk is already in rank
/// order, so the LIMIT becomes a stop condition, the order key falls out of the
/// posting the cursor is standing on, and the equality filter is answered from
/// ITS posting as a membership probe -- no row at all, which is the same
/// authority `scalar_eq_posting_matches` already carries.
///
/// The price is walking over the rows the filter rejects, `1/density` of them
/// per accepted row, each costing a sequential posting step plus a membership
/// probe. Against the other plan's random primary point-get plus row walk per
/// match, the two meet around one accepted row in sixteen. A LIMIT is required:
/// without one the order walk crosses the whole index and pays a membership
/// probe on every row of it, which is strictly more work than the equality plan
/// ever does.
///
/// WHICH density, though, is the whole question. An earlier version of this
/// sampled the equality posting's own sequence spread -- how thinly its matches
/// are scattered through the collection -- and it was WRONG, measurably: with
/// `cat` and `rating` correlated, the highest-rated 3,500 rows of the
/// benchmark's collection contain no `cafe` at all, the equality filter looks
/// perfectly broad in sequence space, and the order walk still crosses
/// thousands of rejected rows before its first hit. The query got 1.5x SLOWER.
///
/// So the probe walks the ORDER index itself, in the direction the real walk
/// would take, for a bounded prefix, and counts how many of those postings the
/// equality filter accepts. That is the statistic the plan actually depends on,
/// and correlation cannot hide from it. It costs [`ORDER_DRIVE_PROBE`] posting
/// steps and the same number of membership probes, once per prepared query.
pub(super) fn order_index_drives_better(
    db: &Database,
    order: &IndexInfo,
    descending: bool,
    filter: &IndexInfo,
    expected: &[u8],
) -> QueryResult<bool> {
    let prefix = scalar_prefix(order.id);
    let mut walk = if descending {
        match db
            .index_range_reverse(order, &prefix_successor(&prefix))
            .map_err(QueryError::from)?
        {
            Some(iter) => ScalarWalk::Reverse(iter),
            None => return Ok(false),
        }
    } else {
        match db.index_range(order, &prefix).map_err(QueryError::from)? {
            Some(iter) => ScalarWalk::Forward(iter),
            None => return Ok(false),
        }
    };
    let mut probed = 0u64;
    let mut accepted = 0u64;
    while probed < ORDER_DRIVE_PROBE {
        let sequence = {
            let Some((key, _)) = walk.peek().map_err(Error::from).map_err(QueryError::from)? else {
                break;
            };
            if !key.starts_with(&prefix) {
                break;
            }
            let suffix = &key[prefix.len()..];
            let value_len = scalar_key::width(&order.kind, suffix)?;
            let mut at = prefix.len() + value_len;
            read_ordered(key, &mut at)?
        };
        walk.step();
        probed += 1;
        if db
            .index_get(filter, &crate::collections::catalog::skey(filter, expected, sequence))
            .map_err(QueryError::from)?
            .is_some()
        {
            accepted += 1;
        }
    }
    // A short index is walked end to end either way; only a real prefix says
    // anything about the rest.
    if probed < ORDER_DRIVE_PROBE {
        return Ok(false);
    }
    Ok(accepted.saturating_mul(ORDER_DRIVE_MIN_DENSITY) >= probed)
}

/// Checks the request against the graph's identities and hands the header
/// back, because the walk needs the same header this read.
///
/// The header used to be read from the store TWICE per traversal -- once
/// here and once at the top of `execute_graph` -- through
/// `read_graph_header`, which reads three replica keys and verifies them,
/// while the graph code keeps a decoded copy in `graph_header_cache` for
/// exactly this question.
fn validate_graph_request(
    db: &Database,
    request: BfsRequest,
) -> QueryResult<crate::index::graph::GraphHeader> {
    if request.min_depth > request.max_depth
        || request.max_depth > 64
        || request.max_visited == 0
        || request.max_visited > 65_536
        || request.max_edges == 0
        || request.max_edges > 1_000_000
        || request.result_limit > 65_536
    {
        return Err(invalid_query("invalid BFS depth/work/result bounds"));
    }
    let header = db.graph_header()?;
    if request.context.0 >= header.next_context
        || request
            .edge_type
            .is_some_and(|edge_type| edge_type.0 == 0 || edge_type.0 >= header.next_type)
    {
        return Err(invalid_query("unknown graph context or edge type"));
    }
    Ok(header)
}

fn visit_graph_direction<C: FnMut() -> bool>(
    db: &Database,
    header: crate::index::graph::GraphHeader,
    entity: EntityId,
    direction: Direction,
    request: &BfsRequest,
    seen: &[EntityId],
    next: &mut crate::index::graph::Frontier,
    scanned: &mut usize,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<()> {
    let incoming = direction == Direction::Incoming;
    let tag = if incoming {
        crate::index::graph::REVERSE_EDGE
    } else {
        crate::index::graph::PRIMARY_EDGE
    };
    // The prefix is built into a stack buffer. It used to be a `Vec` per
    // direction per entity, which on a wide frontier is one heap allocation
    // per step of the walk for bytes that never leave this function.
    let mut buffer = [0u8; crate::index::graph::MAX_EDGE_PREFIX];
    let at0 = crate::index::graph::edge_prefix_into(
        &mut buffer,
        tag,
        entity,
        Some(request.context),
        request.edge_type,
    );
    let prefix = &buffer[..at0];
    // The near entity, the context and (when the caller named one) the type
    // are the prefix itself: `starts_with` proves the row carries exactly the
    // bytes we built, so only the far endpoint has to be read back out.
    //
    // `for_each_ref` hands the callback borrows into the pinned leaf. The
    // allocating cursor built a key `Vec` and a value `Vec` for every edge
    // walked, for a parser that only reads them. The work meter is charged on
    // exactly the old schedule: one unit per turn of the loop, including the
    // turn that found no further row.
    let (pinned, context, max_edges) = (request.edge_type, request.context, request.max_edges);
    let mut failure: Option<QueryError> = None;
    let mut stopped = false;
    {
        let mut step = |key: &[u8], value: &[u8]| -> QueryResult<bool> {
            meter.charge(WorkResource::GraphEdges, 1)?;
            if !key.starts_with(prefix) {
                return Ok(false);
            }
            *scanned = scanned
                .checked_add(1)
                .ok_or_else(|| invalid_query("BFS edge work overflow"))?;
            if *scanned > max_edges {
                return Err(invalid_query("BFS edge work limit exceeded"));
            }
            // The SQL traversal returns entities, so it decodes no properties
            // and reads nothing across the pair: both directions are written in
            // one transaction, so a committed snapshot cannot hold half a pair,
            // and `verify_indexed_source` is the tool that checks pair
            // consistency.
            if incoming && !value.is_empty() {
                return Err(corrupt_query("nonempty reverse edge marker"));
            }
            let (_, adjacent) = crate::index::graph::adjacent_from_tail(
                key, at0, pinned, context, header,
            )?;
            // The visited set is a SORTED VECTOR and the level being built is
            // a `Frontier`, the same two structures `traverse_bfs` uses. The
            // three `BTreeSet`s this walk used to keep -- visited, level and
            // answer -- allocated a heap cell every few entities to answer a
            // question a binary search answers for nothing.
            if seen.binary_search(&adjacent).is_err() {
                next.offer(adjacent)
                    .map_err(|_| invalid_query("BFS visited limit exceeded"))?;
            }
            Ok(true)
        };
        db.store()?
            .range(prefix)
            .map_err(Error::from)?
            .for_each_ref(|key, value| match step(key, value) {
                Ok(true) => true,
                Ok(false) => {
                    stopped = true;
                    false
                }
                Err(error) => {
                    failure = Some(error);
                    stopped = true;
                    false
                }
            })
            .map_err(Error::from)?;
    }
    if let Some(error) = failure {
        return Err(error);
    }
    if !stopped {
        meter.charge(WorkResource::GraphEdges, 1)?;
    }
    Ok(())
}

/// The breadth-first walk behind `QueryFilter::Graph`, answering with the
/// distinct entities it reached in ascending entity order.
///
/// It is the walk `Database::traverse_bfs` runs, built out of the same
/// pieces: a sorted visited vector merged one level at a time, a `Frontier`
/// for the level being discovered, prefixes built on the stack, and the graph
/// header the graph code already has in hand. What it adds is the query
/// engine's work meter, charged on the same schedule as before -- one unit
/// per turn of the edge loop, and one per DISTINCT entity a level discovers,
/// which is what the old per-edge charge added up to.
///
/// PENDING (`docs/GRAPH_CONTRACT.md` §4.3, order-of-work item 1): this walk
/// prunes by type, direction and depth only, through `visit_graph_direction`.
/// A predicate on an edge property or a covered node field is not yet
/// evaluated as the frontier expands, so §4.3's "a traversal never reads a
/// row for a predicate on a covered field" is not the code's behaviour today
/// -- a per-hop predicate is still a post-filter over the entities this
/// function returns.
fn execute_graph<C: FnMut() -> bool>(
    db: &Database,
    request: BfsRequest,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<Vec<EntityId>> {
    let header = validate_graph_request(db, request)?;
    meter.charge(WorkResource::GraphVisited, 1)?;
    let mut seen = vec![request.seed];
    let mut merged: Vec<EntityId> = Vec::new();
    let mut results: Vec<EntityId> = Vec::new();
    if request.include_seed && request.min_depth == 0 {
        if request.result_limit == 0 {
            return Err(invalid_query("BFS result limit exceeded"));
        }
        results.push(request.seed);
    }
    let mut frontier = vec![request.seed];
    let mut scanned = 0usize;
    for depth in 1..=request.max_depth {
        meter.check_cancelled()?;
        let mut next = crate::index::graph::Frontier::new(request.max_visited - seen.len());
        for entity in frontier {
            meter.check_cancelled()?;
            if matches!(request.direction, Direction::Outgoing | Direction::Both) {
                visit_graph_direction(
                    db,
                    header,
                    entity,
                    Direction::Outgoing,
                    &request,
                    &seen,
                    &mut next,
                    &mut scanned,
                    meter,
                )?;
            }
            if matches!(request.direction, Direction::Incoming | Direction::Both) {
                visit_graph_direction(
                    db,
                    header,
                    entity,
                    Direction::Incoming,
                    &request,
                    &seen,
                    &mut next,
                    &mut scanned,
                    meter,
                )?;
            }
        }
        let next = next.into_sorted();
        meter.charge(WorkResource::GraphVisited, next.len() as u64)?;
        if seen.len() + next.len() > request.max_visited {
            return Err(invalid_query("BFS visited limit exceeded"));
        }
        crate::index::graph::merge_sorted_disjoint(&mut seen, &next, &mut merged);
        if depth >= request.min_depth {
            if results.len() + next.len() > request.result_limit {
                return Err(invalid_query("BFS result limit exceeded"));
            }
            results.extend(next.iter().copied());
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    // The seed-existence refusal, paid only when it can still be the answer.
    // An edge cannot outlive its endpoints -- a write validates both and a
    // delete cascades -- so an edge already walked is itself the proof that
    // the seed's row is there. This read used to be paid by every traversal,
    // before the walk, whether or not anything was going to need it; it is
    // the same rule `traverse_bfs` and `neighbors` already apply.
    if scanned == 0 {
        meter.charge(WorkResource::PrimaryReads, 1)?;
        if db.store()?.get(&row_key(request.seed))?.is_none() {
            return Err(QueryError::Database(Error::NotFound("graph endpoint")));
        }
    }
    // Each level is sorted, but the levels were appended in discovery order.
    // Membership is asked once per candidate and the driver promises
    // ascending ids, so the answer is sorted once here.
    results.sort_unstable();
    Ok(results)
}

pub(super) fn execute_graph_filters<C: FnMut() -> bool>(
    db: &Database,
    filters: &[CompiledFilter],
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<Vec<Option<Vec<EntityId>>>> {
    let mut results = vec![None; filters.len()];
    for filter in filters {
        if let CompiledFilter::Graph { request, position } = filter {
            results[*position] = Some(execute_graph(db, *request, meter)?);
        }
    }
    Ok(results)
}

/// An index key a driver carried over from the entry it was standing on.
///
/// Exactly one driver kind ever fills this in, and each consumer proves the
/// key is the one IT asked for by matching the index id as well as the
/// variant. As three separate `Option<(IndexId, Vec<u8>)>` fields this cost 96
/// bytes on EVERY candidate -- including the 20,000 of a key-only scan, which
/// carry none of them -- and a candidate is built, moved and dropped once per
/// row.
pub(super) enum CarriedKey {
    /// A scalar posting's value key, read only by a ranking over that index.
    Scalar(IndexId, Vec<u8>),
    /// An exact-vector locator.
    Vector(IndexId, Vec<u8>),
    /// A quantized-vector entry.
    Quantized(IndexId, Vec<u8>),
    /// The Hilbert cell a spatial posting is filed under, read only by a
    /// ranking in that index's own walk order. The point is the posting's
    /// own coordinates, so a Distance ranking over a spatial driver does
    /// not open the row.
    Cell(IndexId, u32, Point),
    /// The `(level, cell)` of the first geometry posting that admitted this
    /// entity, read only by a ranking in that index's own walk order.
    GeomCell(IndexId, u8, u32),
    /// A mapping entry's external-key bytes, read only by a ranking in the
    /// keys driver's own walk order.
    Key(Vec<u8>),
    /// A nearest-walk posting: coordinates and the geodesic distance already
    /// computed from the centre the walk was opened at.
    Distance(IndexId, Point, f64),
}

pub(super) struct Candidate {
    pub(super) id: EntityId,
    pub(super) row: Option<Vec<u8>>,
    pub(super) carried: Option<CarriedKey>,
    pub(super) satisfied_filter: Option<usize>,
    /// What the batched row pass already decided about this candidate's
    /// FILTERS. `Some(true)` means every filter passed and there is nothing
    /// left to evaluate; `Some(false)` means one refused it. `None` means the
    /// pass did not settle it -- no batch, or a row it could not reach -- and
    /// the walk decides as it always did.
    pub(super) row_filtered: Option<bool>,
    /// What the text merge cursor already knew about this document. Only the
    /// text driver fills it in; every other driver leaves it `None` and every
    /// scorer that cannot prove the frequencies are its own ignores it.
    pub(super) text: Option<TextFrequencies>,
}

impl Candidate {
    /// One candidate that carries nothing but its identity.
    pub(super) fn bare(id: EntityId) -> Self {
        Self {
            id,
            row: None,
            carried: None,
            satisfied_filter: None,
            row_filtered: None,
            text: None,
        }
    }

    /// The scalar value key this candidate came in on, if it came in on
    /// `index`'s postings.
    pub(super) fn scalar(&self, index: IndexId) -> Option<&[u8]> {
        match &self.carried {
            Some(CarriedKey::Scalar(id, key)) if *id == index => Some(key),
            _ => None,
        }
    }

    pub(super) fn vector(&self, index: IndexId) -> Option<&[u8]> {
        match &self.carried {
            Some(CarriedKey::Vector(id, key)) if *id == index => Some(key),
            _ => None,
        }
    }

    pub(super) fn quantized(&self, index: IndexId) -> Option<&[u8]> {
        match &self.carried {
            Some(CarriedKey::Quantized(id, key)) if *id == index => Some(key),
            _ => None,
        }
    }

    /// The spatial cell this candidate came in on, if it came in on
    /// `index`'s postings.
    pub(super) fn cell(&self, index: IndexId) -> Option<u32> {
        match &self.carried {
            Some(CarriedKey::Cell(id, cell, _)) if *id == index => Some(*cell),
            _ => None,
        }
    }

    pub(super) fn point(&self, index: IndexId) -> Option<Point> {
        match &self.carried {
            Some(CarriedKey::Cell(id, _, point) | CarriedKey::Distance(id, point, _))
                if *id == index =>
            {
                Some(*point)
            }
            _ => None,
        }
    }

    pub(super) fn distance_metres(&self, index: IndexId) -> Option<f64> {
        match &self.carried {
            Some(CarriedKey::Distance(id, _, distance)) if *id == index => Some(*distance),
            _ => None,
        }
    }

    pub(super) fn geom_cell(&self, index: IndexId) -> Option<(u8, u32)> {
        match &self.carried {
            Some(CarriedKey::GeomCell(id, level, cell)) if *id == index => Some((*level, *cell)),
            _ => None,
        }
    }

    /// The external-key bytes this candidate came in on, if it came from the
    /// keys driver's mapping-entry walk.
    pub(super) fn key(&self) -> Option<&[u8]> {
        match &self.carried {
            Some(CarriedKey::Key(key)) => Some(key),
            _ => None,
        }
    }
}

pub(super) struct EntityCursor<'a> {
    pub(super) inner: RangeIter<'a>,
    pub(super) prefix: Vec<u8>,
    /// The collection the prefix was built from. A key that matched the
    /// prefix has already proved its collection; carrying it here is what
    /// lets the id decode read only the sequence.
    pub(super) collection: CollectionId,
    /// Copy the primary row out of the leaf, or read its key only. The bytes
    /// are worth an allocation only when a filter or the ranking will decode
    /// them; a key-only scan used to allocate one per row and drop it.
    pub(super) wants_row: bool,
    pub(super) done: bool,
}

/// One posting range, walked in one direction. The two cursors are the same
/// tree read the two ways round: ascending from the predicate's lower bound,
/// descending from one key past its upper bound.
pub(super) enum ScalarWalk<'a> {
    /// Nothing can match -- the `Empty` predicate, or an index whose tree has
    /// never been written.
    Nothing,
    Forward(RangeIter<'a>),
    Reverse(ReverseRangeIter<'a>),
}

impl ScalarWalk<'_> {
    pub(super) fn peek(&mut self) -> kernel::Result<Option<(&[u8], &[u8])>> {
        match self {
            Self::Nothing => Ok(None),
            Self::Forward(iter) => iter.peek_ref(),
            Self::Reverse(iter) => iter.peek_ref(),
        }
    }

    pub(super) fn step(&mut self) {
        match self {
            Self::Nothing => {}
            Self::Forward(iter) => iter.step(),
            Self::Reverse(iter) => iter.step(),
        }
    }

    /// True when the walk runs from high keys to low ones, which inverts what
    /// "before the predicate" and "past the predicate" mean.
    pub(super) fn descending(&self) -> bool {
        matches!(self, Self::Reverse(_))
    }
}

pub(super) struct ScalarCursor<'a> {
    pub(super) walk: ScalarWalk<'a>,
    pub(super) prefix: Vec<u8>,
    pub(super) info: IndexInfo,
    pub(super) predicate: EncodedScalarFilter,
    /// The filter position this cursor's own walk already proves, if any --
    /// see `DriverCursor::new`.
    pub(super) certifies: Option<usize>,
    /// Copy the posting's value key out of the leaf, or read the sequence
    /// only. Only a scalar ranking over this same index reads it.
    pub(super) wants_scalar: bool,
    pub(super) done: bool,
}

/// What one page needs from each candidate, decided once per page instead of
/// per row. Each field is something a driver can hand over for free when it is
/// wanted and must allocate for when it is not.
#[derive(Clone, Copy)]
pub(super) struct CursorNeeds {
    pub(super) row: bool,
    pub(super) scalar_key: bool,
    pub(super) key: bool,
}

/// One mapping-keyspace range, walked ascending. Unlike `ScalarWalk` there is
/// no reverse variant: `QueryOrder::Driver` has no direction of its own (see
/// `DriverKey`'s doc), so nothing ever asks this cursor to open backwards --
/// stated, not implemented, per item KD's scope.
pub(super) struct KeysCursor<'a> {
    pub(super) inner: RangeIter<'a>,
    pub(super) prefix: Vec<u8>,
    pub(super) collection: CollectionId,
    pub(super) predicate: EncodedScalarFilter,
    /// The filter position this cursor's own walk already proves, if any.
    /// Unlike a scalar posting there is no nullish sentinel to leave
    /// uncertified: every mapping entry this cursor yields is a real,
    /// present key inside the predicate, full stop.
    pub(super) certifies: Option<usize>,
    pub(super) wants_key: bool,
    pub(super) done: bool,
}

pub(super) struct VectorCursor<'a> {
    pub(super) inner: RangeIter<'a>,
    pub(super) prefix: Vec<u8>,
    pub(super) info: IndexInfo,
    pub(super) done: bool,
}

pub(super) struct QuantizedVectorCursor<'a> {
    pub(super) inner: RangeIter<'a>,
    pub(super) prefix: Vec<u8>,
    pub(super) info: IndexInfo,
    pub(super) done: bool,
}

pub(super) struct TextPostingCursor<'a> {
    pub(super) inner: crate::index::text::TermPostings<'a>,
    /// The posting the merge is standing on: document AND term frequency.
    /// `TermPostings::next` decodes both; the frequency used to be dropped
    /// here and re-read, per document, by the scorer.
    pub(super) head: Option<(u64, u32)>,
    pub(super) done: bool,
}

pub(super) struct TextCursor<'a> {
    pub(super) streams: Vec<TextPostingCursor<'a>>,
    pub(super) collection: CollectionId,
    pub(super) matching: TextMatch,
    pub(super) position: Option<usize>,
    /// Which compiled text site this cursor is, stamped onto every candidate
    /// it emits so only the scorer that asked for these terms reads them.
    pub(super) source: TextSource,
    /// Are there few enough query terms to carry their frequencies inline?
    pub(super) carries: bool,
    /// The last document emitted, to prove the merge ascends.
    pub(super) previous: Option<u64>,
    pub(super) initialized: bool,
    pub(super) done: bool,
}

pub(super) struct SpatialCursor<'a> {
    pub(super) db: &'a Database,
    pub(super) info: IndexInfo,
    pub(super) predicate: PointFilter,
    pub(super) position: usize,
    pub(super) prefix: Vec<u8>,
    pub(super) ranges: Vec<(u64, u64)>,
    pub(super) range: usize,
    /// Where the FIRST range this cursor opens starts, when the page is
    /// resuming: the exact posting the previous page stopped on. `None` opens
    /// at the range's own low cell, which is what an unresumed walk does and
    /// what every page used to do.
    pub(super) start: Option<Vec<u8>>,
    pub(super) inner: Option<RangeIter<'a>>,
    pub(super) done: bool,
}

/// Walks a geometry index's cover ranges, admitting a posting when its `BoxF`
/// overlaps the query bbox. Dedup is a `HashSet` of sequences: an entity
/// posts at most 8 cells at one level, and other entities interleave, so a
/// consecutive-run exploit is not sound. Under Driver order the set is
/// cloned onto the next page so a later posting of an already-emitted
/// entity is not re-yielded after a resume.
///
/// The box is only a candidate test. This cursor does NOT certify the
/// filter (`satisfied_filter` stays `None`): the predicate is a live T1
/// atomic (`docs/QL_CONTRACT.md` §4.4), not a T3 refusal, and the row's
/// geometry is refined through `src/index/spatial/geometry.rs`.
pub(super) struct GeometryCursor<'a> {
    pub(super) db: &'a Database,
    pub(super) info: IndexInfo,
    pub(super) prefix: Vec<u8>,
    pub(super) ranges: Vec<GeomRange>,
    pub(super) range: usize,
    pub(super) start: Option<Vec<u8>>,
    pub(super) inner: Option<RangeIter<'a>>,
    pub(super) query_bbox: BoxF,
    pub(super) seen: HashSet<u64>,
    pub(super) done: bool,
}

pub(super) enum DriverCursor<'a> {
    Entities(EntityCursor<'a>),
    Scalar(ScalarCursor<'a>),
    Spatial(SpatialCursor<'a>),
    Nearest(NearestCursor<'a>),
    Geometry(GeometryCursor<'a>),
    Text(TextCursor<'a>),
    Vector(VectorCursor<'a>),
    QuantizedVector(QuantizedVectorCursor<'a>),
    Keys(KeysCursor<'a>),
    Ids(std::vec::IntoIter<EntityId>),
}

pub(super) fn scalar_prefix(id: IndexId) -> Vec<u8> {
    let mut out = vec![crate::collections::catalog::SCALAR];
    out.extend(ordered(id.0));
    out
}

pub(super) fn scalar_lower(predicate: &EncodedScalarFilter) -> Option<&[u8]> {
    match predicate {
        EncodedScalarFilter::Empty => None,
        EncodedScalarFilter::Eq(value) => Some(value),
        EncodedScalarFilter::Range { lower, .. } => bound_bytes(lower),
        EncodedScalarFilter::IsNull | EncodedScalarFilter::IsMissing => Some(&[0]),
    }
}

/// The successor of a prefix: the shortest key above every key that starts
/// with it. A scalar prefix begins with the family tag `0x70`, so the loop
/// always finds a byte to raise.
fn prefix_successor(prefix: &[u8]) -> Vec<u8> {
    let mut key = prefix.to_vec();
    while let Some(last) = key.last_mut() {
        if *last == u8::MAX {
            key.pop();
        } else {
            *last += 1;
            return key;
        }
    }
    vec![u8::MAX; 64]
}

/// One key past every posting of `value`. Postings are
/// `prefix || value || ordered(sequence)`, so the widest of them is the one
/// with the maximum sequence, and one byte beyond that sits above the whole
/// group and below the next value.
fn past_scalar_value(info: &IndexInfo, value: &[u8]) -> Vec<u8> {
    let mut key = crate::collections::catalog::skey(info, value, u64::MAX);
    key.push(0);
    key
}

/// Where a DESCENDING walk of this predicate opens. `range_reverse` yields
/// keys strictly below its argument, so this is the exclusive upper edge of
/// the predicate's range rather than its last member.
pub(super) fn scalar_reverse_start(
    info: &IndexInfo,
    prefix: &[u8],
    predicate: &EncodedScalarFilter,
) -> Option<Vec<u8>> {
    match predicate {
        EncodedScalarFilter::Empty => None,
        EncodedScalarFilter::Eq(value) => Some(past_scalar_value(info, value)),
        EncodedScalarFilter::IsNull | EncodedScalarFilter::IsMissing => {
            Some(past_scalar_value(info, NULLISH_SCALAR_KEY))
        }
        EncodedScalarFilter::Range { upper, .. } => Some(match upper {
            EncodedBound::Included(value) => past_scalar_value(info, value),
            // Every posting of `value` sorts after the bare `prefix || value`,
            // so opening there excludes the whole group, which is what an
            // exclusive upper bound means.
            EncodedBound::Excluded(value) => {
                let mut key = prefix.to_vec();
                key.extend_from_slice(value);
                key
            }
            EncodedBound::Unbounded => prefix_successor(prefix),
        }),
    }
}

/// Where a resumed DESCENDING walk opens.
///
/// A descending query ranks by (value descending, entity id ASCENDING) -- the
/// id half never flips -- while the reverse walk hands a tie group over id
/// DESCENDING. So the previous page's last key is NOT a point the walk can
/// resume from: the rows still owed inside that tie group lie *earlier* in
/// the walk, not later. Resuming at the top of the boundary value's group
/// re-walks that one group and lets `next_page`'s `after` comparison drop the
/// members it already emitted. The re-walk is bounded by one value's tie
/// group per page; opening at the start of the index is bounded by everything
/// emitted so far.
pub(super) fn resume_scalar_reverse_key(info: &IndexInfo, after: &RankKey) -> Option<Vec<u8>> {
    match &after.value {
        RankValue::Scalar(value) => Some(past_scalar_value(info, value)),
        _ => None,
    }
}

/// The posting key a resumed scalar walk should open at: the exact entry the
/// previous page stopped on. Both shapes of rank key that a scalar driver can
/// produce in its own walk order are covered -- a scalar ranking carries the
/// value key, and an id ranking over an equality driver has one value for the
/// whole range. Anything else returns `None` and the cursor opens where it
/// always did.
pub(super) fn resume_scalar_key(
    info: &IndexInfo,
    predicate: &EncodedScalarFilter,
    after: &RankKey,
) -> Option<Vec<u8>> {
    let value = match (&after.value, predicate) {
        (RankValue::Scalar(key), _) => key.as_slice(),
        (RankValue::Entity, EncodedScalarFilter::Eq(value)) => value.as_slice(),
        _ => return None,
    };
    Some(crate::collections::catalog::skey(info, value, after.id.sequence))
}

/// The mapping key a resumed keys walk should open at: the previous page's
/// last key, re-yielded and dropped by `next_page`'s own `after` comparison --
/// the same shape `EntityCursor`'s resume uses. One function serves both
/// directions here (unlike `resume_scalar_key`/`resume_scalar_reverse_key`)
/// because a mapping entry is unique per key: there is no tie group whose
/// still-owed members lie on the far side of it.
pub(super) fn resume_key_walk(prefix: &[u8], after: &RankKey) -> Option<Vec<u8>> {
    match &after.value {
        RankValue::Key(key) => {
            let mut start = prefix.to_vec();
            start.extend_from_slice(key);
            Some(start)
        }
        _ => None,
    }
}

/// The nullish scalar key. A NULL field and a MISSING one share it, so a
/// posting carrying it proves neither.
pub(super) const NULLISH_SCALAR_KEY: &[u8] = &[0];

pub(super) fn scalar_key_position(predicate: &EncodedScalarFilter, key: &[u8]) -> Ordering {
    match predicate {
        EncodedScalarFilter::Empty => Ordering::Greater,
        EncodedScalarFilter::Eq(value) => key.cmp(value),
        EncodedScalarFilter::Range { lower, upper } => {
            let below = match lower {
                EncodedBound::Included(value) => key < value.as_slice(),
                EncodedBound::Excluded(value) => key <= value.as_slice(),
                EncodedBound::Unbounded => false,
            };
            if below {
                return Ordering::Less;
            }
            let above = match upper {
                EncodedBound::Included(value) => key > value.as_slice(),
                EncodedBound::Excluded(value) => key >= value.as_slice(),
                EncodedBound::Unbounded => false,
            };
            if above {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }
        EncodedScalarFilter::IsNull | EncodedScalarFilter::IsMissing => key.cmp(&[0]),
    }
}
