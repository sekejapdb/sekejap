//! Query planning: the compiled forms of a request, the `prepare_query`
//! entry point, the predicate encodings the index keyspace understands, and
//! the driver choice. See docs/QL_CONTRACT.md,
//! "Execution pipeline" and "Family hooks".
use super::*;

#[derive(Clone, Debug)]
pub(super) enum EncodedScalarFilter {
    Empty,
    Eq(Vec<u8>),
    Range {
        lower: EncodedBound,
        upper: EncodedBound,
    },
    IsNull,
    IsMissing,
}

#[derive(Clone, Debug)]
pub(super) enum EncodedBound {
    Included(Vec<u8>),
    Excluded(Vec<u8>),
    Unbounded,
}

#[derive(Clone, Debug)]
pub(super) enum CompiledFilter {
    Scalar {
        info: IndexInfo,
        predicate: EncodedScalarFilter,
        posting_membership: bool,
    },
    /// A scalar filter whose predicate was folded into `into` -- another
    /// position naming the SAME index -- by [`fold_same_index_scalars`].
    ///
    /// The position survives because positions are user-visible: a caller may
    /// name any of them as `CandidateDriver::Filter`, and a candidate's
    /// `satisfied_filter` is a position. What does not survive is the WORK:
    /// the surviving predicate is the conjunction, so this slot is already
    /// answered for every candidate that reaches it and it reads nothing.
    Folded {
        into: usize,
    },
    JsonEq {
        field: String,
        value: Value,
    },
    Graph {
        request: BfsRequest,
        position: usize,
    },
    Point {
        info: IndexInfo,
        predicate: PointFilter,
    },
    Geometry {
        info: IndexInfo,
        predicate: GeometryFilter,
    },
    Text(PreparedText),
    /// A range over the external-key mapping keyspace. Reuses
    /// `EncodedScalarFilter`'s `Range`/`Empty` shape -- `scalar_key_position`
    /// and `range_or_empty` are pure byte-range logic with nothing
    /// scalar-index-specific in them, and a key has no `Eq`/`IsNull`/
    /// `IsMissing` counterpart (every mapping entry is a present real key).
    /// Meaningful only at the position `DriverPlan::Keys` certifies;
    /// `prepare_query` refuses any other placement.
    Key {
        predicate: EncodedScalarFilter,
    },
}

#[derive(Clone, Debug)]
pub(super) struct PreparedText {
    pub(super) info: IndexInfo,
    pub(super) terms: Vec<String>,
    pub(super) phrase: Option<Vec<String>>,
    /// The phrase's KMP failure function. It depends on the phrase alone, and
    /// the document scanner used to rebuild it per document.
    pub(super) phrase_prefix: Vec<usize>,
    pub(super) matching: TextMatch,
    pub(super) dfs: Vec<u64>,
    /// The corpus half of every BM25 score this query can produce, and one
    /// inverse document frequency per term. Both are settled the moment the
    /// query is prepared; both used to be recomputed inside the per-document
    /// loop, the idf with a natural logarithm.
    pub(super) weights: crate::index::text::Bm25Weights,
    pub(super) idfs: Vec<f64>,
    /// Which candidate driver, if any, hands this prepared query the term
    /// frequencies it needs -- see `TextSource`. Filled in once, after the
    /// driver is chosen, by comparing term lists; `None` means this scorer
    /// must read its own frequencies.
    pub(super) driven: Option<TextSource>,
}

impl PreparedText {
    /// Would the driver's per-term frequencies answer THIS query's terms?
    ///
    /// Same index, same match mode, same terms in the same order. Nothing is
    /// hashed or assumed: a query may search one term list and rank by
    /// another (`WHERE SEARCH(body,'comet') ORDER BY BM25(body,'harbour
    /// comet')`), and taking the driver's frequencies for the wrong term list
    /// would silently score the wrong documents.
    pub(super) fn same_terms(&self, other: &Self) -> bool {
        self.info.id == other.info.id && self.matching == other.matching && self.terms == other.terms
    }
}

/// Which compiled text site produced a candidate, so a scorer can tell whether
/// the frequencies riding on that candidate are the ones IT asked for.
///
/// A text query is compiled at most twice in one request -- once per `WHERE`
/// text filter, once for a BM25 order -- and the driver is exactly one of
/// those sites. The candidate carries the site, the scorer knows its own, and
/// `PreparedText::driven` records (once, at prepare time) which site's
/// frequencies it may consume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TextSource {
    Filter(usize),
    Order,
}

/// How many query terms' frequencies ride on a candidate without allocating.
///
/// A text query may have up to 64 distinct terms; carrying 64 slots on every
/// candidate would cost more than the reads they save. Past this many the
/// driver stamps nothing and the scorer reads its own frequencies, which is
/// the behaviour every driver had before.
pub(super) const INLINE_TEXT_TERMS: usize = 8;

/// The most distinct terms one text query may have, enforced by
/// `prepare_text`. Scoring sizes its per-document buffers from this so it can
/// keep them on the stack.
pub(super) const MAX_TEXT_TERMS: usize = 64;

/// The two buffers the text scorer fills per document: one frequency and one
/// inverse document frequency per term that actually matched.
///
/// They were stack arrays sized to the 64-term bound -- 256 bytes plus 512
/// bytes, zeroed on every scored document, for a query that usually has one
/// term. A page builds this once and every document reuses it: `clear` frees
/// nothing and `push` allocates only until the first document has settled the
/// capacity.
#[derive(Default)]
pub(super) struct TextRowScratch {
    pub(super) frequencies: Vec<u32>,
    pub(super) idfs: Vec<f64>,
}

/// Everything one page reuses across its candidates, in one place.
///
/// Each of these is a buffer whose contents belong to the row being looked at
/// and whose CAPACITY belongs to the page. Passing them as one value is what
/// keeps `filters_match` and `rank_candidate` from growing a parameter every
/// time another per-row allocation is found.
#[derive(Default)]
pub(super) struct RowScratch {
    /// One decoded norm block, reused by every candidate that lands in it.
    pub(super) norms: crate::index::text::TextScratch,
    /// The per-term frequencies and idfs of the document being scored.
    pub(super) text: TextRowScratch,
    /// The encoded scalar key of ONE row value.
    ///
    /// A non-driving scalar predicate encodes the row's value to compare it
    /// against its bound. Through `scalar_key::encode` that was two
    /// allocations per candidate for nine bytes that are read once and
    /// dropped.
    pub(super) scalar: Vec<u8>,
    /// The phrase scanner's current token.
    pub(super) token: String,
    /// How often each of the query's terms was seen in the document the phrase
    /// scanner is on. Positionally by prepared term.
    pub(super) seen: Vec<u32>,
}

/// The per-term frequencies the text merge cursor decoded on its way past this
/// document.
///
/// The merge already stands on the `(document, frequency)` entry it is
/// emitting. Keeping it costs one `u32` per query term; throwing it away costs
/// a re-seek and a whole-segment re-decode per term per document, which is the
/// O(df^2) that this type removes.
#[derive(Clone, Copy, Debug)]
pub(super) struct TextFrequencies {
    pub(super) source: TextSource,
    pub(super) len: u8,
    /// Frequency per query term, positionally. `0` means the term is absent
    /// from this document, which the posting format never stores.
    pub(super) slots: [u32; INLINE_TEXT_TERMS],
}

#[derive(Clone, Debug)]
pub(super) enum CompiledOrder {
    EntityId,
    Scalar {
        info: IndexInfo,
        direction: SortDirection,
    },
    ExactVector {
        info: IndexInfo,
        query: Vec<f32>,
        query_norm: f64,
        metric: VectorMetric,
    },
    ApproximateVector {
        info: IndexInfo,
        query: Vec<f32>,
        query_norm: f64,
        metric: VectorMetric,
        ef: usize,
    },
    Bm25(PreparedText),
    /// Rank by the driver's own walk key. Which key that is cannot be known
    /// while the order is being compiled -- the driver is chosen after -- so
    /// `prepare_query` fills it in once the plan is settled.
    Driver(DriverKey),
    Distance {
        info: IndexInfo,
        center: Point,
    },
    Score {
        expr: CompiledScoreExpr,
        direction: SortDirection,
    },
}

pub(super) fn require_scalar_index(
    db: &Database,
    collection: CollectionId,
    id: IndexId,
) -> QueryResult<IndexInfo> {
    let info = db.index_info_cached(id)?;
    if info.collection != collection {
        return Err(invalid_query("query index belongs to another collection"));
    }
    if info.family != IndexFamily::Scalar {
        return Err(invalid_query("query requires a scalar index"));
    }
    if info.state != IndexState::Ready {
        return Err(invalid_query("query index is not ready"));
    }
    Ok(info)
}

pub(super) fn require_family_index(
    db: &Database,
    collection: CollectionId,
    id: IndexId,
    family: IndexFamily,
    label: &str,
) -> QueryResult<IndexInfo> {
    let info = db.index_info_cached(id)?;
    if info.collection != collection {
        return Err(invalid_query("query index belongs to another collection"));
    }
    if info.family != family {
        return Err(invalid_query(format!("query requires a {label} index")));
    }
    if info.state != IndexState::Ready {
        return Err(invalid_query("query index is not ready"));
    }
    Ok(info)
}

pub(super) fn prepare_text(
    db: &Database,
    collection: CollectionId,
    id: IndexId,
    query: &str,
    matching: TextMatch,
) -> QueryResult<PreparedText> {
    let info = require_family_index(db, collection, id, IndexFamily::Text, "text")?;
    crate::index::text::descriptor(&info)?;
    let (analysis, phrase) = if matching == TextMatch::Phrase {
        let phrase = crate::text_analyzer::analyze_phrase_query(query).map_err(invalid_query)?;
        (phrase.analysis, Some(phrase.sequence))
    } else {
        (
            crate::text_analyzer::analyze(query).map_err(invalid_query)?,
            None,
        )
    };
    if analysis.terms.len() > 64 {
        return Err(invalid_query("text query exceeds 64 distinct terms"));
    }
    let terms: Vec<_> = analysis.terms.into_keys().collect();
    let corpus = crate::index::text::read_corpus(db, id)?;
    let mut dfs = Vec::with_capacity(terms.len());
    for term in &terms {
        let df = crate::index::text::read_df(db, id, term)?.unwrap_or(0);
        if df > corpus.documents {
            return Err(corrupt_query(
                "text document frequency exceeds corpus document count",
            ));
        }
        dfs.push(df);
    }
    let weights = crate::index::text::bm25_weights(corpus)?;
    let mut idfs = Vec::with_capacity(terms.len());
    for &df in &dfs {
        idfs.push(if df == 0 {
            // No posting carries this term, so nothing will ever be scored
            // against it; a document that claims one is refused by
            // `bm25_scored`'s own bounds and never reaches this weight.
            0.0
        } else {
            crate::index::text::bm25_idf(corpus, df)?
        });
    }
    Ok(PreparedText {
        info,
        terms,
        phrase_prefix: phrase
            .as_deref()
            .map(crate::text_analyzer::phrase_prefix)
            .unwrap_or_default(),
        phrase,
        matching,
        dfs,
        weights,
        idfs,
        driven: None,
    })
}

pub(super) fn prepare_vector(
    db: &Database,
    collection: CollectionId,
    id: IndexId,
    query: &[f32],
    metric: VectorMetric,
) -> QueryResult<(IndexInfo, Vec<f32>, f64)> {
    let info = require_family_index(db, collection, id, IndexFamily::ExactVector, "exact-vector")?;
    let dimension = crate::index::vector::exact::dimension(&info)?;
    if query.len() != dimension || query.iter().any(|lane| !lane.is_finite()) {
        return Err(invalid_query(
            "query vector has wrong dimension or non-finite lane",
        ));
    }
    let norm = query
        .iter()
        .fold(0.0, |sum, lane| sum + f64::from(*lane) * f64::from(*lane));
    if metric == VectorMetric::Cosine && norm == 0.0 {
        return Err(invalid_query("cosine query vector must have nonzero norm"));
    }
    Ok((info, query.to_vec(), norm))
}

fn prepare_approximate_vector(
    db: &Database,
    collection: CollectionId,
    id: IndexId,
    query: &[f32],
    metric: VectorMetric,
    ef: usize,
) -> QueryResult<(IndexInfo, Vec<f32>, f64)> {
    if !(1..=crate::collections::catalog::MAX_RESULTS).contains(&ef) {
        return Err(invalid_query("approximate vector ef requires 1..=65536"));
    }
    let info = require_family_index(
        db,
        collection,
        id,
        IndexFamily::QuantizedVector,
        "quantized-vector",
    )?;
    let dimension = crate::index::vector::quantized::dimension(&info)?;
    if query.len() != dimension || query.iter().any(|lane| !lane.is_finite()) {
        return Err(invalid_query(
            "query vector has wrong dimension or non-finite lane",
        ));
    }
    let norm = query
        .iter()
        .fold(0.0, |sum, lane| sum + f64::from(*lane) * f64::from(*lane));
    if metric == VectorMetric::Cosine && norm == 0.0 {
        return Err(invalid_query("cosine query vector must have nonzero norm"));
    }
    Ok((info, query.to_vec(), norm))
}

pub(super) fn point_ranges(predicate: PointFilter) -> QueryResult<(Vec<(u64, u64)>, bool)> {
    let bounds = match predicate {
        PointFilter::Bbox(bounds) => bounds,
        PointFilter::Radius {
            center,
            radius_metres,
        } => radius_candidate_bounds(center, radius_metres).map_err(invalid_query)?,
    };
    let ranges = bounds_hilbert_ranges(bounds);
    let fallback_world = ranges.as_slice() == [(0, MAX_HILBERT_VALUE)];
    Ok((ranges, fallback_world))
}

/// Degree margin matching `radius_candidate_bounds`'s private `OUTWARD_DEGREES`.
const GEOM_OUTWARD_DEGREES: f64 = 1e-10;

fn geometry_query_geom(predicate: &GeometryFilter) -> &Geom {
    match predicate {
        GeometryFilter::Intersects(g)
        | GeometryFilter::Within(g)
        | GeometryFilter::Contains(g) => g,
        GeometryFilter::DWithin { geometry, .. } => geometry,
    }
}

fn geometry_query_bbox(predicate: &GeometryFilter) -> QueryResult<(f64, f64, f64, f64)> {
    let geom = geometry_query_geom(predicate);
    // The same rule the index files geometries under: a query geometry that
    // crosses the antimeridian asks about the whole longitude range.
    let bbox = crate::index::spatial::geometry_index::indexed_bbox(geom)
        .ok_or_else(|| invalid_query("geometry filter requires a non-empty geometry"))?;
    match predicate {
        GeometryFilter::DWithin { metres, .. } => {
            if !metres.is_finite() || *metres < 0.0 {
                return Err(invalid_query(
                    "geometry dwithin metres must be finite and non-negative",
                ));
            }
            Ok(expand_bbox_by_metres(bbox, *metres))
        }
        _ => Ok(bbox),
    }
}

/// Expand a query bbox by a geodesic radius so every stored geometry within
/// `metres` of any point in the original box has a bbox that overlaps the
/// expansion. Same conservative envelope as [`radius_candidate_bounds`]:
///
/// - `angular = metres / WGS84_MIN_CURVATURE_RADIUS_METRES` (the smallest
///   WGS84 curvature radius, so the angle is an over-estimate);
/// - latitude expands by `angular.to_degrees() + 1e-10` on both sides;
/// - longitude expands by `asin(sin(angular) / cos(φ)) + 1e-10` at the
///   **poleward** latitude of the expanded band (smallest `cos(φ)` → most
///   degrees). If that band reaches a pole, longitude is the whole world.
///
/// Never misses; may over-cover. A dateline wrap becomes a world longitude.
fn expand_bbox_by_metres(
    (xmin, xmax, ymin, ymax): (f64, f64, f64, f64),
    metres: f64,
) -> (f64, f64, f64, f64) {
    let angular = metres / WGS84_MIN_CURVATURE_RADIUS_METRES;
    if angular >= std::f64::consts::PI {
        return (-180.0, 180.0, -90.0, 90.0);
    }
    let lat_delta = angular.to_degrees() + GEOM_OUTWARD_DEGREES;
    let south = (ymin - lat_delta).max(-90.0);
    let north = (ymax + lat_delta).min(90.0);
    let poleward = south.abs().max(north.abs()).to_radians();
    if poleward + angular >= std::f64::consts::FRAC_PI_2 {
        return (-180.0, 180.0, south, north);
    }
    let ratio = (angular.sin() / poleward.cos()).clamp(-1.0, 1.0);
    let lon_delta = ratio.asin().to_degrees() + GEOM_OUTWARD_DEGREES;
    let west = xmin - lon_delta;
    let east = xmax + lon_delta;
    if west < -180.0 || east > 180.0 || west > east {
        (-180.0, 180.0, south, north)
    } else {
        (west, east, south, north)
    }
}

/// Cover ranges at one ladder level, budgeted like the point driver
/// (`MAX_HILBERT_RANGES` = 64). A cover that will not fit becomes the whole
/// level (never a miss).
fn geometry_level_ranges(
    xmin: f64,
    xmax: f64,
    ymin: f64,
    ymax: f64,
    bits: u8,
) -> (Vec<(u64, u64)>, bool) {
    let max_h = if bits == 0 {
        0
    } else {
        (1u64 << (2 * bits)) - 1
    };
    if bits == 0 {
        return (vec![(0, 0)], false);
    }
    let mut ranges = cover_ranges(xmin, xmax, ymin, ymax, bits, MAX_HILBERT_RANGES);
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (lo, hi) in ranges {
        if lo > hi || hi > max_h {
            return (vec![(0, max_h)], true);
        }
        match merged.last_mut() {
            Some(last) if lo <= last.1.saturating_add(1) => last.1 = last.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    if merged.is_empty() || merged.len() > MAX_HILBERT_RANGES {
        (vec![(0, max_h)], true)
    } else {
        (merged, false)
    }
}

fn geometry_ranges(predicate: &GeometryFilter) -> QueryResult<(Vec<GeomRange>, BoxF, bool)> {
    let (xmin, xmax, ymin, ymax) = geometry_query_bbox(predicate)?;
    let query_bbox = BoxF::from_f64(xmin, xmax, ymin, ymax);
    let mut out = Vec::new();
    let mut fallback_world = false;
    // On-disk key order: level byte first, so WORLD (0), COARSE (8), FINE (12).
    for level in [spatial::LEVEL_WORLD, spatial::LEVEL_COARSE, spatial::LEVEL_FINE] {
        let (ranges, world) = geometry_level_ranges(xmin, xmax, ymin, ymax, level);
        fallback_world |= world;
        for (lo, hi) in ranges {
            out.push(GeomRange {
                level,
                lo,
                hi,
            });
        }
    }
    Ok((out, query_bbox, fallback_world))
}

fn encode_scalar_value(kind: &Kind, value: ScalarValue<'_>) -> QueryResult<Vec<u8>> {
    if !matches!(
        (kind, value),
        (Kind::Bool, ScalarValue::Bool(_))
            | (Kind::Int, ScalarValue::I64(_))
            | (Kind::Real, ScalarValue::F64(_))
            | (Kind::Text, ScalarValue::Text(_))
    ) {
        return Err(invalid_query(
            "scalar predicate value does not match the index kind",
        ));
    }
    if matches!(value, ScalarValue::F64(number) if !number.is_finite()) {
        return Err(invalid_query("finite scalar Real value required"));
    }
    scalar_key::encode(kind, Some(&value.json())).map_err(QueryError::from)
}

fn encode_bound(kind: &Kind, bound: &Bound<ScalarValue<'_>>) -> QueryResult<EncodedBound> {
    match bound {
        Bound::Included(value) => Ok(EncodedBound::Included(encode_scalar_value(kind, *value)?)),
        Bound::Excluded(value) => Ok(EncodedBound::Excluded(encode_scalar_value(kind, *value)?)),
        Bound::Unbounded => Ok(EncodedBound::Unbounded),
    }
}

/// A key bound as its raw UTF-8 bytes -- `mapping_key` appends a key's bytes
/// directly with no order-preserving transform (`collections.rs:382`), so
/// byte order already is the bound's order and there is no `Kind` to encode
/// against, unlike a scalar value.
fn encode_key_bound(bound: &Bound<&str>) -> EncodedBound {
    match bound {
        Bound::Included(value) => EncodedBound::Included(value.as_bytes().to_vec()),
        Bound::Excluded(value) => EncodedBound::Excluded(value.as_bytes().to_vec()),
        Bound::Unbounded => EncodedBound::Unbounded,
    }
}

pub(super) fn bound_bytes(bound: &EncodedBound) -> Option<&[u8]> {
    match bound {
        EncodedBound::Included(value) | EncodedBound::Excluded(value) => Some(value),
        EncodedBound::Unbounded => None,
    }
}

fn compile_scalar_filter(
    kind: &Kind,
    predicate: &ScalarFilter<'_>,
) -> QueryResult<EncodedScalarFilter> {
    match predicate {
        ScalarFilter::Eq(value) => Ok(EncodedScalarFilter::Eq(encode_scalar_value(kind, *value)?)),
        ScalarFilter::Range { lower, upper } => Ok(range_or_empty(
            encode_bound(kind, lower)?,
            encode_bound(kind, upper)?,
        )),
        ScalarFilter::IsNull => Ok(EncodedScalarFilter::IsNull),
        ScalarFilter::IsMissing => Ok(EncodedScalarFilter::IsMissing),
    }
}

/// A pair of bounds as a predicate: `Range` when a value can sit between them,
/// `Empty` when nothing can. Crossed bounds, and bounds that meet on a value
/// at least one side excludes, admit nothing.
fn range_or_empty(lower: EncodedBound, upper: EncodedBound) -> EncodedScalarFilter {
    let empty = match (bound_bytes(&lower), bound_bytes(&upper)) {
        (Some(lower_value), Some(upper_value)) if lower_value > upper_value => true,
        (Some(lower_value), Some(upper_value)) if lower_value == upper_value => {
            matches!(lower, EncodedBound::Excluded(_)) || matches!(upper, EncodedBound::Excluded(_))
        }
        _ => false,
    };
    if empty {
        EncodedScalarFilter::Empty
    } else {
        EncodedScalarFilter::Range { lower, upper }
    }
}

/// The tighter of two LOWER bounds. Unbounded is the loosest; on the same
/// value the excluding bound is the tighter one.
fn tighter_lower(left: EncodedBound, right: EncodedBound) -> EncodedBound {
    match (bound_bytes(&left), bound_bytes(&right)) {
        (None, _) => right,
        (_, None) => left,
        (Some(a), Some(b)) => match a.cmp(b) {
            Ordering::Greater => left,
            Ordering::Less => right,
            Ordering::Equal => {
                if matches!(left, EncodedBound::Excluded(_)) {
                    left
                } else {
                    right
                }
            }
        },
    }
}

/// The tighter of two UPPER bounds, by the same rule mirrored.
fn tighter_upper(left: EncodedBound, right: EncodedBound) -> EncodedBound {
    match (bound_bytes(&left), bound_bytes(&right)) {
        (None, _) => right,
        (_, None) => left,
        (Some(a), Some(b)) => match a.cmp(b) {
            Ordering::Less => left,
            Ordering::Greater => right,
            Ordering::Equal => {
                if matches!(left, EncodedBound::Excluded(_)) {
                    left
                } else {
                    right
                }
            }
        },
    }
}

/// Two predicates on ONE scalar index, as the single predicate they mean.
///
/// The whole point is that an index answers a conjunction over itself without
/// help: a posting key that survives the folded predicate has satisfied both,
/// so nothing has to open the row to ask the second question. The rules, in
/// full, with `Empty` absorbing everything:
///
/// * `Range ∩ Range` -- the tighter bound on each side. Bounds that cross, or
///   meet on a value one side excludes, give `Empty`: the answer is empty and
///   the tree is never touched.
/// * `Eq ∩ Range` -- `Eq` when the value lies inside the range, otherwise
///   `Empty`. `Eq` is a subset of any range that contains it.
/// * `Eq ∩ Eq` -- the same `Eq` when the encodings agree, otherwise `Empty`.
/// * `IsNull ∩ IsNull`, `IsMissing ∩ IsMissing` -- unchanged. Mixed, `Empty`:
///   a field is not both absent and present-and-null.
/// * `IsNull` or `IsMissing` against `Eq` or `Range` -- `Empty`. Null and
///   missing share the nullish posting key, which sorts below every real
///   value, and `scalar_filter_matches` refuses both for `Eq` and for `Range`.
///   So no row is both nullish and inside a value predicate, and folding to
///   `Empty` says exactly what the per-row evaluation already said.
fn fold_scalar_predicates(
    left: &EncodedScalarFilter,
    right: &EncodedScalarFilter,
) -> EncodedScalarFilter {
    use EncodedScalarFilter as F;
    match (left, right) {
        (F::Empty, _) | (_, F::Empty) => F::Empty,
        (F::IsNull, F::IsNull) => F::IsNull,
        (F::IsMissing, F::IsMissing) => F::IsMissing,
        (F::IsNull | F::IsMissing, _) | (_, F::IsNull | F::IsMissing) => F::Empty,
        (F::Eq(a), F::Eq(b)) => {
            if a == b {
                F::Eq(a.clone())
            } else {
                F::Empty
            }
        }
        (F::Eq(value), range @ F::Range { .. }) | (range @ F::Range { .. }, F::Eq(value)) => {
            if scalar_key_position(range, value) == Ordering::Equal {
                F::Eq(value.clone())
            } else {
                F::Empty
            }
        }
        (
            F::Range {
                lower: left_lower,
                upper: left_upper,
            },
            F::Range {
                lower: right_lower,
                upper: right_upper,
            },
        ) => range_or_empty(
            tighter_lower(left_lower.clone(), right_lower.clone()),
            tighter_upper(left_upper.clone(), right_upper.clone()),
        ),
    }
}

/// Fold every group of scalar filters that name the SAME index into one
/// predicate, held by one surviving position.
///
/// Which position survives is user-visible, so the rule is fixed and narrow:
///
/// * if the request names one of the group as its candidate driver
///   (`CandidateDriver::Filter(position)`), THAT position keeps the folded
///   predicate, so an explicitly chosen driver still drives;
/// * otherwise the LOWEST position in the group keeps it.
///
/// Every other position in the group becomes [`CompiledFilter::Folded`],
/// which matches every candidate and reads nothing. The positions themselves
/// never move: naming a folded position as the driver drives the survivor,
/// and a candidate the survivor certifies still reports the survivor's
/// position in `satisfied_filter`.
fn fold_same_index_scalars(filters: &mut [CompiledFilter], driver: CandidateDriver) {
    let named = match driver {
        CandidateDriver::Filter(position) => Some(position),
        _ => None,
    };
    let mut groups: Vec<(IndexId, Vec<usize>)> = Vec::new();
    for (position, filter) in filters.iter().enumerate() {
        if let CompiledFilter::Scalar { info, .. } = filter {
            match groups.iter_mut().find(|(id, _)| *id == info.id) {
                Some((_, members)) => members.push(position),
                None => groups.push((info.id, vec![position])),
            }
        }
    }
    for (_, members) in groups {
        if members.len() < 2 {
            continue;
        }
        let keeper = named
            .filter(|position| members.contains(position))
            .unwrap_or(members[0]);
        let mut folded = match &filters[members[0]] {
            CompiledFilter::Scalar { predicate, .. } => predicate.clone(),
            _ => continue,
        };
        for position in members.iter().skip(1) {
            if let CompiledFilter::Scalar { predicate, .. } = &filters[*position] {
                folded = fold_scalar_predicates(&folded, predicate);
            }
        }
        for position in &members {
            if *position == keeper {
                if let CompiledFilter::Scalar { predicate, .. } = &mut filters[*position] {
                    *predicate = folded.clone();
                }
            } else {
                filters[*position] = CompiledFilter::Folded { into: keeper };
            }
        }
    }
}

fn scalar_driver_score(predicate: &EncodedScalarFilter) -> u8 {
    match predicate {
        EncodedScalarFilter::Empty | EncodedScalarFilter::Eq(_) => 0,
        EncodedScalarFilter::Range { .. } => 1,
        EncodedScalarFilter::IsNull | EncodedScalarFilter::IsMissing => 2,
    }
}

impl Database {
    pub fn prepare_query<'db>(
        &'db self,
        request: QueryRequest<'_>,
    ) -> QueryResult<PreparedQuery<'db>> {
        self.catalog(request.collection)?;
        if request.filters.len() > MAX_FILTERS {
            return Err(invalid_query("query has more than 64 filters"));
        }
        let collection = self.collection_info(request.collection)?;
        let mut filters = Vec::with_capacity(request.filters.len());
        for (position, filter) in request.filters.iter().enumerate() {
            filters.push(match filter {
                QueryFilter::Scalar { index, predicate } => {
                    let info = require_scalar_index(self, request.collection, *index)?;
                    let predicate = compile_scalar_filter(&info.kind, predicate)?;
                    CompiledFilter::Scalar {
                        info,
                        predicate,
                        posting_membership: false,
                    }
                }
                QueryFilter::JsonEq { field, value } => {
                    if field.is_empty() || field.len() > MAX_FIELD_BYTES || reserved(field) {
                        return Err(invalid_query("invalid JSON equality field"));
                    }
                    let encoded = crate::binary_json(value).map_err(invalid_query)?;
                    if encoded.len() > MAX_JSON_PREDICATE_BYTES {
                        return Err(invalid_query("JSON equality value exceeds 1 MiB"));
                    }
                    if collection
                        .layout
                        .fields
                        .iter()
                        .find(|(name, _)| name == field)
                        .is_some_and(|(_, kind)| kind != &Kind::Json)
                    {
                        return Err(invalid_query(
                            "JSON equality requires a Json field or an undeclared extra",
                        ));
                    }
                    CompiledFilter::JsonEq {
                        field: (*field).to_owned(),
                        value: (*value).clone(),
                    }
                }
                QueryFilter::Graph(request) => CompiledFilter::Graph {
                    request: *request,
                    position,
                },
                QueryFilter::Point { index, predicate } => {
                    let info = require_family_index(
                        self,
                        request.collection,
                        *index,
                        IndexFamily::SpatialPoint,
                        "spatial-point",
                    )?;
                    crate::index::spatial::point::descriptor(&info)?;
                    if let PointFilter::Radius { radius_metres, .. } = predicate {
                        if !radius_metres.is_finite() || *radius_metres < 0.0 {
                            return Err(invalid_query(
                                "point radius must be finite and non-negative",
                            ));
                        }
                    }
                    CompiledFilter::Point {
                        info,
                        predicate: *predicate,
                    }
                }
                QueryFilter::Geometry { index, predicate } => {
                    let info = self.index_info_cached(*index)?;
                    if info.collection != request.collection {
                        return Err(invalid_query("query index belongs to another collection"));
                    }
                    if info.family == IndexFamily::SpatialPoint {
                        return Err(invalid_query(
                            "a Geometry filter requires a spatial geometry index, not a point index",
                        ));
                    }
                    if info.family != IndexFamily::SpatialGeometry {
                        return Err(invalid_query(
                            "a Geometry filter requires a spatial geometry index",
                        ));
                    }
                    if info.state != IndexState::Ready {
                        return Err(invalid_query("query index is not ready"));
                    }
                    crate::index::spatial::geometry_index::descriptor(&info)?;
                    // Touch the bbox now so a DWithin with a bad radius, or
                    // an empty query geometry, is refused at prepare.
                    let _ = geometry_query_bbox(predicate)?;
                    CompiledFilter::Geometry {
                        info,
                        predicate: predicate.clone(),
                    }
                }
                QueryFilter::Text {
                    index,
                    query,
                    matching,
                } => CompiledFilter::Text(prepare_text(
                    self,
                    request.collection,
                    *index,
                    query,
                    *matching,
                )?),
                QueryFilter::Key { lower, upper } => CompiledFilter::Key {
                    predicate: range_or_empty(encode_key_bound(lower), encode_key_bound(upper)),
                },
            });
        }
        if filters
            .iter()
            .filter(|filter| matches!(filter, CompiledFilter::Key { .. }))
            .count()
            > 1
        {
            return Err(invalid_query("query has more than one key filter"));
        }

        // Two predicates on ONE scalar index are one predicate. Folding them
        // here, once, is the difference between an index that answers the
        // conjunction from its own postings and one that answers half of it
        // and opens the row for the rest -- a primary read per candidate for
        // a question the posting key had already settled.
        fold_same_index_scalars(&mut filters, request.driver);

        let mut order = match request.order {
            QueryOrder::EntityId => CompiledOrder::EntityId,
            QueryOrder::Scalar { index, direction } => CompiledOrder::Scalar {
                info: require_scalar_index(self, request.collection, index)?,
                direction,
            },
            QueryOrder::ExactVector {
                index,
                query,
                metric,
            } => {
                let (info, query, query_norm) =
                    prepare_vector(self, request.collection, index, query, metric)?;
                CompiledOrder::ExactVector {
                    info,
                    query,
                    query_norm,
                    metric,
                }
            }
            QueryOrder::ApproximateVector {
                index,
                query,
                metric,
                ef,
            } => {
                let (info, query, query_norm) =
                    prepare_approximate_vector(self, request.collection, index, query, metric, ef)?;
                CompiledOrder::ApproximateVector {
                    info,
                    query,
                    query_norm,
                    metric,
                    ef,
                }
            }
            QueryOrder::Bm25 {
                index,
                query,
                matching,
            } => CompiledOrder::Bm25(prepare_text(
                self,
                request.collection,
                index,
                query,
                matching,
            )?),
            // The key is the DRIVER's, and the driver is chosen below. This
            // stands in until it is; `driver_key` replaces it.
            QueryOrder::Driver => CompiledOrder::Driver(DriverKey::Entity),
            QueryOrder::Distance {
                index,
                center,
                direction,
            } => {
                if matches!(direction, SortDirection::Descending) {
                    return Err(invalid_query("distance order is ascending only"));
                }
                let info = require_family_index(
                    self,
                    request.collection,
                    index,
                    IndexFamily::SpatialPoint,
                    "spatial-point",
                )?;
                crate::index::spatial::point::descriptor(&info)?;
                CompiledOrder::Distance {
                    info,
                    center,
                }
            }
            QueryOrder::Score { expr, direction } => {
                let mut leaves = 0usize;
                CompiledOrder::Score {
                    expr: compile_score_expr(self, request.collection, expr, 1, &mut leaves)?,
                    direction,
                }
            }
        };

        let fields = match request.projection {
            Projection::Ids => Vec::new(),
            Projection::Fields(fields) => {
                if fields.len() > MAX_PROJECTION_FIELDS {
                    return Err(invalid_query("query projects more than 64 fields"));
                }
                let mut out = Vec::with_capacity(fields.len());
                for field in fields {
                    if field.is_empty()
                        || field.len() > MAX_FIELD_BYTES
                        || reserved(field)
                        || out.iter().any(|old| old == field)
                    {
                        return Err(invalid_query("invalid or duplicate projection field"));
                    }
                    out.push((*field).to_owned());
                }
                out
            }
        };

        let filter_driver = |position: usize| -> QueryResult<DriverPlan> {
            // A folded position names the survivor that holds its predicate,
            // so naming either one as the driver drives the same walk.
            let position = match filters.get(position) {
                Some(CompiledFilter::Folded { into }) => *into,
                _ => position,
            };
            match filters.get(position) {
                Some(CompiledFilter::Scalar {
                    info, predicate, ..
                }) => Ok(DriverPlan::Scalar {
                    info: info.clone(),
                    predicate: predicate.clone(),
                    position: Some(position),
                }),
                Some(CompiledFilter::Graph { position, .. }) => Ok(DriverPlan::Graph {
                    position: *position,
                }),
                Some(CompiledFilter::Point { info, predicate }) => {
                    let (ranges, fallback_world) = point_ranges(*predicate)?;
                    Ok(DriverPlan::Spatial {
                        info: info.clone(),
                        predicate: *predicate,
                        position,
                        ranges,
                        fallback_world,
                    })
                }
                Some(CompiledFilter::Geometry { info, predicate }) => {
                    let (ranges, query_bbox, fallback_world) = geometry_ranges(predicate)?;
                    Ok(DriverPlan::Geometry {
                        info: info.clone(),
                        position,
                        ranges,
                        query_bbox,
                        fallback_world,
                    })
                }
                Some(CompiledFilter::Text(prepared)) => Ok(DriverPlan::Text {
                    prepared: prepared.clone(),
                    position: Some(position),
                }),
                _ => Err(invalid_query("selected filter cannot drive candidates")),
            }
        };
        let order_driver = || -> QueryResult<DriverPlan> {
            match &order {
                CompiledOrder::Scalar { info, .. } => Ok(DriverPlan::Scalar {
                    info: info.clone(),
                    predicate: EncodedScalarFilter::Range {
                        lower: EncodedBound::Unbounded,
                        upper: EncodedBound::Unbounded,
                    },
                    position: None,
                }),
                CompiledOrder::EntityId => Ok(DriverPlan::Entities),
                CompiledOrder::ExactVector { info, .. } => {
                    Ok(DriverPlan::ExactVector { info: info.clone() })
                }
                CompiledOrder::ApproximateVector { info, .. } => {
                    Ok(DriverPlan::QuantizedVector { info: info.clone() })
                }
                CompiledOrder::Bm25(prepared) => Ok(DriverPlan::Text {
                    prepared: prepared.clone(),
                    position: None,
                }),
                // Driver order names no index, so there is no order index to
                // drive: the entity cursor is the candidate stream, and its
                // own order is the answer's. This is also what a driver-
                // ordered query with no filter at all lands on.
                CompiledOrder::Driver(_) => Ok(DriverPlan::Entities),
                CompiledOrder::Distance { info, center } => {
                    Ok(nearest_plan(info, *center, &filters))
                }
                // Score never drives, except a lone VectorSimilarity leaf with
                // no filters is the same query as ExactVector, so it reuses
                // that driver. With filters, Auto still picks from the
                // filters; CandidateDriver::Order on any other expression is
                // a full entity scan ranked by the score.
                CompiledOrder::Score { expr, .. } => {
                    if let (Some(info), true) = (expr.vector_similarity_leaf(), filters.is_empty()) {
                        Ok(DriverPlan::ExactVector { info: info.clone() })
                    } else {
                        Ok(DriverPlan::Entities)
                    }
                }
            }
        };
        // `CandidateDriver::Keys` is not `CandidateDriver::Filter(position)`
        // because its filter is OPTIONAL: with none, the whole collection's
        // keys are walked (what `count_all` needs); with one, that position
        // is the predicate and is certified the same way a driving scalar
        // range is.
        let keys_driver = |filters: &[CompiledFilter]| -> QueryResult<DriverPlan> {
            let found = filters.iter().enumerate().find_map(|(position, filter)| {
                match filter {
                    CompiledFilter::Key { predicate } => Some((position, predicate.clone())),
                    _ => None,
                }
            });
            Ok(match found {
                Some((position, predicate)) => DriverPlan::Keys {
                    predicate,
                    position: Some(position),
                },
                None => DriverPlan::Keys {
                    predicate: EncodedScalarFilter::Range {
                        lower: EncodedBound::Unbounded,
                        upper: EncodedBound::Unbounded,
                    },
                    position: None,
                },
            })
        };
        let driver = match request.driver {
            CandidateDriver::Entities => DriverPlan::Entities,
            CandidateDriver::Filter(position) => filter_driver(position)?,
            CandidateDriver::Order => order_driver()?,
            CandidateDriver::Keys => keys_driver(&filters)?,
            CandidateDriver::Auto => {
                // A filtered APPROXIMATE VECTOR order whose filters the
                // compact scan can answer index-side, over a filter broad
                // enough that scanning beats point-getting the matches: the
                // quantized index drives and the page is one page-order scan
                // (`filtered_vector_scan`). Ahead of every filter rule below,
                // because its own guard already refuses every filter shape
                // those rules exist for.
                if matches!(order, CompiledOrder::ApproximateVector { .. })
                    && approximate_scan_drives(self, request.collection, &filters)?
                {
                    order_driver()?
                } else if let Some(position) = filters
                    .iter()
                    .position(|filter| matches!(filter, CompiledFilter::Graph { .. }))
                {
                    filter_driver(position)?
                } else if let Some(position) = filters.iter().position(|filter| {
                    matches!(
                        filter,
                        CompiledFilter::Scalar {
                            predicate: EncodedScalarFilter::Empty | EncodedScalarFilter::Eq(_),
                            ..
                        }
                    )
                }) {
                    // An equality filter is the default driver -- unless the
                    // query also names an ORDER on another ready index and the
                    // filter is broad enough that walking that index in rank
                    // order is the cheaper plan: `order_index_drives_better`
                    // for a scalar order, `nearest_drives_better` for a
                    // distance one.
                    let ordered_elsewhere = match (&order, filters.get(position)) {
                        (
                            CompiledOrder::Scalar {
                                info: ranked,
                                direction,
                            },
                            Some(CompiledFilter::Scalar {
                                info,
                                predicate: EncodedScalarFilter::Eq(value),
                                ..
                            }),
                        ) if ranked.id != info.id && request.total_limit.is_some() => {
                            order_index_drives_better(
                                self,
                                ranked,
                                matches!(direction, SortDirection::Descending),
                                info,
                                value,
                            )?
                        }
                        (
                            CompiledOrder::Distance {
                                info: ranked,
                                center,
                            },
                            Some(CompiledFilter::Scalar {
                                info,
                                predicate: EncodedScalarFilter::Eq(value),
                                ..
                            }),
                        ) if request.total_limit.is_some_and(|limit| limit > 0)
                            && distance_can_drive(ranked.id, *center, &filters)
                            && nearest_tests_filters_index_side(&filters) =>
                        {
                            nearest_drives_better(
                                self,
                                request.total_limit.unwrap_or(0),
                                info,
                                value,
                            )?
                        }
                        _ => false,
                    };
                    if ordered_elsewhere {
                        order_driver()?
                    } else {
                        filter_driver(position)?
                    }
                } else if let Some(position) = filters
                    .iter()
                    .position(|filter| matches!(filter, CompiledFilter::Text(_)))
                {
                    filter_driver(position)?
                } else if let CompiledOrder::Distance { info, center } = &order {
                    if distance_can_drive(info.id, *center, &filters) {
                        nearest_plan(info, *center, &filters)
                    } else if let Some(position) = filters.iter().position(|filter| {
                        matches!(filter, CompiledFilter::Point { predicate, .. } if point_ranges(*predicate).is_ok_and(|(_, world)| !world))
                    }) {
                        filter_driver(position)?
                    } else if let Some(position) = filters
                        .iter()
                        .position(|filter| matches!(filter, CompiledFilter::Point { .. }))
                    {
                        filter_driver(position)?
                    } else {
                        nearest_plan(info, *center, &filters)
                    }
                } else if let Some(position) = filters.iter().position(|filter| {
                    matches!(filter, CompiledFilter::Point { predicate, .. } if point_ranges(*predicate).is_ok_and(|(_, world)| !world))
                }) {
                    filter_driver(position)?
                } else if let Some(position) = filters.iter().position(|filter| {
                    matches!(filter, CompiledFilter::Geometry { predicate, .. } if geometry_ranges(predicate).is_ok_and(|(_, _, world)| !world))
                }) {
                    filter_driver(position)?
                } else if let Some((_, position)) = filters
                    .iter()
                    .enumerate()
                    .filter_map(|(position, filter)| match filter {
                        CompiledFilter::Scalar { predicate, .. } => {
                            Some((scalar_driver_score(predicate), position))
                        }
                        _ => None,
                    })
                    .min()
                {
                    filter_driver(position)?
                } else if let Some(position) = filters
                    .iter()
                    .position(|filter| matches!(filter, CompiledFilter::Point { .. }))
                {
                    filter_driver(position)?
                } else if let Some(position) = filters
                    .iter()
                    .position(|filter| matches!(filter, CompiledFilter::Geometry { .. }))
                {
                    filter_driver(position)?
                } else {
                    order_driver()?
                }
            }
        };

        // A `QueryFilter::Key` is certified by the mapping-entry walk that
        // produced the candidate (`CompiledFilter::Key`'s doc); it has no row-
        // level fallback the way a scalar predicate does. So it is only
        // meaningful at the position `CandidateDriver::Keys` actually
        // certifies -- named by a DIFFERENT driver, or left uncertified
        // because the request chose `CandidateDriver::Keys` but a second key
        // filter lost the (at most one) slot, it would reach `filters_match`
        // with no way to answer itself.
        if let Some(key_position) = filters
            .iter()
            .position(|filter| matches!(filter, CompiledFilter::Key { .. }))
        {
            let certified = matches!(
                &driver,
                DriverPlan::Keys { position: Some(position), .. } if *position == key_position
            );
            if !certified {
                return Err(invalid_query(
                    "a key filter requires CandidateDriver::Keys to drive it",
                ));
            }
        }

        // Driver order ranks by the key the chosen driver's own walk is
        // sorted by, which is knowable only now.
        if matches!(order, CompiledOrder::Driver(_)) {
            order = CompiledOrder::Driver(driver_key(&driver)?);
        }

        let scalar_driver_position = match &driver {
            DriverPlan::Scalar { position, .. } => *position,
            _ => None,
        };
        for (position, filter) in filters.iter_mut().enumerate() {
            if let CompiledFilter::Scalar {
                predicate,
                posting_membership,
                ..
            } = filter
            {
                *posting_membership = matches!(predicate, EncodedScalarFilter::Eq(_))
                    && scalar_driver_position != Some(position);
            }
        }

        // A non-driving RANGE filter's candidate's value is unknown until
        // something reads it -- unlike an equality filter, its posting alone
        // cannot answer one candidate at a time. A non-driving POINT filter is
        // worse: nothing but the row carries the coordinates unless its own
        // cover ranges are walked, so every candidate was a primary read.
        // `ensure_membership_sets` walks each of them once instead, on first
        // use, and every position marked here is what that walk fills in.
        let driving_position = match &driver {
            DriverPlan::Scalar { position, .. }
            | DriverPlan::Text { position, .. }
            | DriverPlan::Keys { position, .. } => *position,
            DriverPlan::Spatial { position, .. }
            | DriverPlan::Geometry { position, .. }
            | DriverPlan::Graph { position } => Some(*position),
            DriverPlan::Nearest { certifies, .. } => *certifies,
            DriverPlan::Entities
            | DriverPlan::ExactVector { .. }
            | DriverPlan::QuantizedVector { .. } => None,
        };
        let mut membership = filters.iter().map(|_| MembershipSet::Ineligible).collect::<Vec<_>>();
        for (position, filter) in filters.iter().enumerate() {
            let eligible = match filter {
                CompiledFilter::Scalar { predicate, .. } => {
                    (matches!(predicate, EncodedScalarFilter::Range { .. })
                        // A non-driving EQUALITY under a QUANTIZED VECTOR
                        // order. Everywhere else an equality already answers
                        // one candidate at a time from its own posting
                        // (`scalar_eq_posting_matches`), and a set would buy
                        // nothing; here the candidates are the compact entries
                        // of the vector index in page order, and a per-
                        // candidate posting probe is a root-to-leaf descent
                        // per entry of the whole collection. An equality is a
                        // one-key range, so the same walk collects it, and the
                        // set is what lets the page-order scan refuse an entry
                        // without decoding its int8 codes.
                        //
                        // The nullish key is excluded: `build_scalar_range_set`
                        // drops it exactly as `scalar_filter_matches` does, so
                        // an equality that somehow encoded to it would get a
                        // set that disagrees with the row-read path.
                        || matches!(
                            (predicate, &driver),
                            (
                                EncodedScalarFilter::Eq(value),
                                DriverPlan::QuantizedVector { .. },
                            ) if value.as_slice() != NULLISH_SCALAR_KEY
                        ))
                        && scalar_driver_position != Some(position)
                }
                // A world-wide cover is every posting in the index, which is
                // the whole collection: walking it to build a set that admits
                // everything buys nothing, so that one is left on the row-read
                // path it already had.
                CompiledFilter::Point { predicate, .. } => {
                    driving_position != Some(position)
                        && point_ranges(*predicate).is_ok_and(|(_, world)| !world)
                }
                _ => false,
            };
            if eligible {
                membership[position] = MembershipSet::Unbuilt;
            }
        }

        // Which scorers may read the frequencies the text driver decodes.
        //
        // Decided once, here, by comparing term lists -- not per candidate,
        // and never by a hash. A text driver produces the frequencies of ITS
        // terms; a scorer over a different term list on the same index must go
        // on reading its own, or it would score the wrong words.
        if let DriverPlan::Text {
            prepared: driving,
            position,
        } = &driver
        {
            let source = match position {
                Some(position) => TextSource::Filter(*position),
                None => TextSource::Order,
            };
            let carries = driving.terms.len() <= INLINE_TEXT_TERMS;
            for filter in filters.iter_mut() {
                if let CompiledFilter::Text(prepared) = filter {
                    prepared.driven = (carries && prepared.same_terms(driving)).then_some(source);
                }
            }
            if let CompiledOrder::Bm25(prepared) = &mut order {
                prepared.driven = (carries && prepared.same_terms(driving)).then_some(source);
            }
            if let CompiledOrder::Score { expr, .. } = &mut order {
                expr.mark_text_driven(driving, source, carries);
            }
        }

        let nearest = match &driver {
            DriverPlan::Nearest {
                info,
                center,
                radius_cap,
                ..
            } => Some(crate::index::spatial::point::NearestWalk::new(
                info.clone(),
                *center,
                request.total_limit.unwrap_or(0),
                *radius_cap,
                usize::MAX,
            )),
            _ => None,
        };

        Ok(PreparedQuery {
            db: self,
            collection: request.collection,
            filters,
            order,
            projection: fields,
            driver,
            total_limit: request.total_limit,
            emitted: 0,
            after: None,
            run: Vec::new(),
            run_bounded: false,
            membership,
            nearest,
            geometry_seen: HashSet::new(),
        })
    }
}
