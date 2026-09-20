//! Per-candidate filter tests: the row-side predicates a candidate must pass
//! once the driver has offered it, and the JSON/point/geometry comparisons
//! they are built from.
use super::*;

fn geometry_predicate_matches(predicate: &GeometryFilter, row: &Geom) -> bool {
    match predicate {
        GeometryFilter::Intersects(query) => spatial_geometry::intersects(row, query),
        GeometryFilter::Within(query) => spatial_geometry::within(row, query),
        GeometryFilter::Contains(query) => spatial_geometry::contains(row, query),
        GeometryFilter::DWithin {
            geometry: query,
            metres,
        } => spatial_geometry::dwithin_m(row, query, *metres),
    }
}

/// Every filter of a BATCHED page, against one borrowed row.
///
/// `batches_row_reads` has already established that each of these is a pure
/// function of the row -- no graph set, no posting probe, no text merge -- so
/// evaluating them here rather than in driver order changes nothing but the
/// order the rows are read in. `None` means one of them was not of that kind
/// after all and the walk must decide; the gate makes that unreachable, and it
/// is a fallback rather than an assertion because being merely slow is the
/// right failure for a plan predicate that drifts.
pub(super) fn batch_filters_match<C: FnMut() -> bool>(
    db: &Database,
    filters: &[CompiledFilter],
    ranges: &[MembershipSet],
    satisfied: Option<usize>,
    id: EntityId,
    bytes: &[u8],
    scratch: &mut RowScratch,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<Option<bool>> {
    let layout = db.layout(layout_id(bytes)?)?;
    for (position, filter) in filters.iter().enumerate() {
        meter.check_cancelled()?;
        if satisfied == Some(position) {
            continue;
        }
        let matches = match filter {
            CompiledFilter::Scalar {
                info,
                predicate,
                posting_membership,
            } => match &ranges[position] {
                MembershipSet::Ids(ids) => ids.binary_search(&id.sequence).is_ok(),
                MembershipSet::Bitmap(bits) => membership_bitmap_contains(bits, id.sequence),
                _ => {
                    if *posting_membership && matches!(predicate, EncodedScalarFilter::Eq(_)) {
                        return Ok(None);
                    }
                    meter.note_row_decode();
                    scalar_filter_matches(
                        info,
                        predicate,
                        selected_field_in(&layout, bytes, &info.field)?,
                        &mut scratch.scalar,
                    )?
                }
            },
            CompiledFilter::JsonEq { field, value } => {
                meter.note_row_decode();
                json_filter_matches(selected_field_in(&layout, bytes, field)?, value)?
            }
            CompiledFilter::Point { info, predicate } => match &ranges[position] {
                MembershipSet::Ids(ids) => ids.binary_search(&id.sequence).is_ok(),
                MembershipSet::Bitmap(bits) => membership_bitmap_contains(bits, id.sequence),
                _ => {
                    // The membership set overflowed (see membership.rs), so
                    // there is no posting left to read: this bills
                    // `SpatialPostings` as a stand-in for the per-candidate
                    // row-field extract below, not for a posting probe.
                    meter.charge(WorkResource::SpatialPostings, 1)?;
                    meter.note_row_decode();
                    match point_from_field(selected_field_in(&layout, bytes, &info.field)?)? {
                        Some(point) => match predicate {
                            PointFilter::Bbox(bounds) => bounds.contains(point),
                            PointFilter::Radius {
                                center,
                                radius_metres,
                            } => {
                                within_radius(*center, point, *radius_metres)
                                    .map_err(corrupt_query)?
                            }
                        },
                        None => false,
                    }
                }
            },
            CompiledFilter::Geometry { info, predicate } => {
                meter.note_row_decode();
                match geom_from_field(selected_field_in(&layout, bytes, &info.field)?)? {
                    Some(geom) => geometry_predicate_matches(predicate, &geom),
                    None => false,
                }
            }
            // Already answered by the position it folded into.
            CompiledFilter::Folded { .. } => true,
            CompiledFilter::Graph { .. } | CompiledFilter::Text(_) | CompiledFilter::Key { .. } => {
                return Ok(None)
            }
        };
        if !matches {
            return Ok(Some(false));
        }
    }
    Ok(Some(true))
}

/// [`ensure_row`] through the page's own primary reader.
pub(super) fn ensure_row_seq<'a, C: FnMut() -> bool>(
    db: &'a Database,
    rows: &mut PrimaryRows<'a>,
    id: EntityId,
    row: &mut Option<RowData>,
    encoded: &mut Option<Vec<u8>>,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<()> {
    if row.is_some() {
        return Ok(());
    }
    let bytes = if let Some(bytes) = encoded.take() {
        bytes
    } else {
        meter.charge(WorkResource::PrimaryReads, 1)?;
        rows.read(id)?
            .ok_or_else(|| corrupt_query("query candidate points to a missing entity"))?
    };
    *row = Some(decode_row(db, bytes)?);
    Ok(())
}

/// One field of one candidate's row.
///
/// The layout is not re-validated here. A `RowData` gets its layout from
/// `Database::layout`, which produces one only through
/// `Layout::from_descriptor` or from a layout this handle itself wrote, and
/// both of those validate -- see [`dense_v3::read_field_in`]. This is the
/// per-candidate path of every non-driving field predicate, so the check was
/// being repaid once per candidate for an answer fixed when the collection was
/// created.
pub(super) fn selected_field(row: &RowData, field: &str) -> QueryResult<dense_v3::FieldValue> {
    selected_field_in(&row.layout, &row.bytes, field)
}

/// [`selected_field`] for a row whose bytes are BORROWED.
fn selected_field_in(
    layout: &Layout,
    bytes: &[u8],
    field: &str,
) -> QueryResult<dense_v3::FieldValue> {
    // A predicate reads committed, checksum-valid pages: the trusted reader
    // stops at the field it wants and steps over the rest by length. The
    // sacrifice is named on `read_field_in_trusted`.
    dense_v3::read_field_in_trusted(layout, bytes, field)
        .map_err(|error| corrupt_query(format!("dense-v3 row: {error}")))
}

pub(super) fn persisted_scalar_key(
    info: &IndexInfo,
    value: dense_v3::FieldValue,
) -> QueryResult<Option<Vec<u8>>> {
    match value {
        dense_v3::FieldValue::Missing | dense_v3::FieldValue::Null => Ok(Some(vec![0])),
        dense_v3::FieldValue::Inline(value) => scalar_key::encode(&info.kind, Some(&value))
            .map(Some)
            .map_err(|error| corrupt_query(format!("indexed scalar row value: {error}"))),
        dense_v3::FieldValue::Vector { .. } => {
            Err(corrupt_query("scalar index field is a historical vector"))
        }
    }
}

fn scalar_filter_matches(
    info: &IndexInfo,
    predicate: &EncodedScalarFilter,
    value: dense_v3::FieldValue,
    out: &mut Vec<u8>,
) -> QueryResult<bool> {
    match predicate {
        EncodedScalarFilter::Empty => Ok(false),
        EncodedScalarFilter::IsNull => Ok(matches!(value, dense_v3::FieldValue::Null)),
        EncodedScalarFilter::IsMissing => Ok(matches!(value, dense_v3::FieldValue::Missing)),
        EncodedScalarFilter::Eq(expected) => match value {
            dense_v3::FieldValue::Inline(value) => {
                scalar_key::encode_into(&info.kind, Some(&value), out)
                    .map_err(|error| corrupt_query(format!("indexed scalar row value: {error}")))?;
                Ok(out.as_slice() == expected.as_slice())
            }
            dense_v3::FieldValue::Missing | dense_v3::FieldValue::Null => Ok(false),
            dense_v3::FieldValue::Vector { .. } => {
                Err(corrupt_query("scalar index field is a historical vector"))
            }
        },
        EncodedScalarFilter::Range { .. } => match value {
            dense_v3::FieldValue::Inline(value) => {
                scalar_key::encode_into(&info.kind, Some(&value), out)
                    .map_err(|error| corrupt_query(format!("indexed scalar row value: {error}")))?;
                Ok(scalar_key_position(predicate, out) == Ordering::Equal)
            }
            dense_v3::FieldValue::Missing | dense_v3::FieldValue::Null => Ok(false),
            dense_v3::FieldValue::Vector { .. } => {
                Err(corrupt_query("scalar index field is a historical vector"))
            }
        },
    }
}

fn number_parts(number: &serde_json::Number) -> std::result::Result<i128, f64> {
    if number.is_f64() {
        return Err(number.as_f64().unwrap_or(0.0));
    }
    if let Some(value) = number.as_i64() {
        return Ok(i128::from(value));
    }
    if let Some(value) = number.as_u64() {
        return Ok(i128::from(value));
    }
    Err(number.as_f64().unwrap_or(0.0))
}

/// Compare an exact JSON integer to binary64 without first rounding the
/// integer. JSON's integer domain fits in i64/u64.
fn compare_integer_float(integer: i128, float: f64) -> Ordering {
    if float.is_nan() {
        return Ordering::Less;
    }
    if float >= 18_446_744_073_709_551_616.0 {
        return Ordering::Less;
    }
    if float < -9_223_372_036_854_775_808.0 {
        return Ordering::Greater;
    }
    let whole = float as i128;
    match integer.cmp(&whole) {
        Ordering::Equal if float.fract() > 0.0 => Ordering::Less,
        Ordering::Equal if float.fract() < 0.0 => Ordering::Greater,
        order => order,
    }
}

fn json_numbers_equal(left: &serde_json::Number, right: &serde_json::Number) -> bool {
    match (number_parts(left), number_parts(right)) {
        (Ok(left), Ok(right)) => left == right,
        (Ok(left), Err(right)) => compare_integer_float(left, right).is_eq(),
        (Err(left), Ok(right)) => compare_integer_float(right, left).is_eq(),
        (Err(left), Err(right)) => (left == 0.0 && right == 0.0) || left.total_cmp(&right).is_eq(),
    }
}

fn json_structural_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(left), Value::Bool(right)) => left == right,
        (Value::Number(left), Value::Number(right)) => json_numbers_equal(left, right),
        (Value::String(left), Value::String(right)) => left == right,
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| json_structural_equal(left, right))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, left)| {
                    right
                        .get(key)
                        .is_some_and(|right| json_structural_equal(left, right))
                })
        }
        _ => false,
    }
}

fn json_filter_matches(value: dense_v3::FieldValue, expected: &Value) -> QueryResult<bool> {
    match value {
        dense_v3::FieldValue::Missing => Ok(false),
        dense_v3::FieldValue::Null => Ok(expected.is_null()),
        dense_v3::FieldValue::Inline(actual) => Ok(json_structural_equal(&actual, expected)),
        dense_v3::FieldValue::Vector { .. } => {
            Err(corrupt_query("JSON equality field is a historical vector"))
        }
    }
}

pub(super) fn point_from_field(value: dense_v3::FieldValue) -> QueryResult<Option<Point>> {
    let value = match value {
        dense_v3::FieldValue::Missing | dense_v3::FieldValue::Null => return Ok(None),
        dense_v3::FieldValue::Inline(value) => value,
        dense_v3::FieldValue::Vector { .. } => {
            return Err(corrupt_query("spatial point field is a historical vector"));
        }
    };
    let object = value
        .as_object()
        .ok_or_else(|| corrupt_query("indexed point is not an object"))?;
    if object.len() != 2 || object.get("type").and_then(Value::as_str) != Some("Point") {
        return Err(corrupt_query("indexed point is not a GeoJSON Point"));
    }
    let coordinates = object
        .get("coordinates")
        .and_then(Value::as_array)
        .filter(|coordinates| coordinates.len() == 2)
        .ok_or_else(|| corrupt_query("indexed point coordinate count"))?;
    let longitude = coordinates[0]
        .as_f64()
        .ok_or_else(|| corrupt_query("indexed point longitude"))?;
    let latitude = coordinates[1]
        .as_f64()
        .ok_or_else(|| corrupt_query("indexed point latitude"))?;
    Point::new(longitude, latitude)
        .map(Some)
        .map_err(corrupt_query)
}

fn geom_from_field(value: dense_v3::FieldValue) -> QueryResult<Option<Geom>> {
    let value = match value {
        dense_v3::FieldValue::Missing | dense_v3::FieldValue::Null => return Ok(None),
        dense_v3::FieldValue::Inline(value) => value,
        dense_v3::FieldValue::Vector { .. } => {
            return Err(corrupt_query("indexed geometry field is a historical vector"));
        }
    };
    crate::index::spatial::geometry_index::geom_from_value(&value)
        .map(Some)
        .map_err(QueryError::from)
}

pub(super) fn filters_match<'a, C: FnMut() -> bool>(
    db: &'a Database,
    rows: &mut PrimaryRows<'a>,
    filters: &[CompiledFilter],
    ranges: &[MembershipSet],
    candidate: &Candidate,
    row: &mut Option<RowData>,
    encoded: &mut Option<Vec<u8>>,
    graph: &[Option<Vec<EntityId>>],
    scratch: &mut RowScratch,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<bool> {
    for (position, filter) in filters.iter().enumerate() {
        meter.check_cancelled()?;
        if candidate.satisfied_filter == Some(position) {
            continue;
        }
        let id = candidate.id;
        let matches = match filter {
            CompiledFilter::Scalar {
                info,
                predicate,
                posting_membership,
            } => match &ranges[position] {
                MembershipSet::Ids(ids) => ids.binary_search(&id.sequence).is_ok(),
                MembershipSet::Bitmap(bits) => membership_bitmap_contains(bits, id.sequence),
                _ => match (
                    *posting_membership,
                    predicate,
                    row.is_none() && encoded.is_none(),
                ) {
                    (true, EncodedScalarFilter::Eq(expected), true) => {
                        scalar_eq_posting_matches(db, info, expected, id, meter)?
                    }
                    _ => {
                        ensure_row_seq(db, rows, id, row, encoded, meter)?;
                        let row = row.as_ref().unwrap();
                        meter.note_row_decode();
                        scalar_filter_matches(
                            info,
                            predicate,
                            selected_field(row, &info.field)?,
                            &mut scratch.scalar,
                        )?
                    }
                },
            },
            CompiledFilter::JsonEq { field, value } => {
                ensure_row_seq(db, rows, id, row, encoded, meter)?;
                let row = row.as_ref().unwrap();
                meter.note_row_decode();
                json_filter_matches(selected_field(row, field)?, value)?
            }
            CompiledFilter::Graph { position, .. } => graph
                .get(*position)
                .and_then(Option::as_ref)
                // Sorted, so membership is a binary search rather than a walk
                // down a tree whose nodes were allocated to answer this.
                .is_some_and(|ids| ids.binary_search(&id).is_ok()),
            CompiledFilter::Point { info, predicate } => match &ranges[position] {
                // The cover walk decided this predicate from the postings'
                // own coordinates; the row has nothing to add.
                MembershipSet::Ids(ids) => ids.binary_search(&id.sequence).is_ok(),
                MembershipSet::Bitmap(bits) => membership_bitmap_contains(bits, id.sequence),
                _ => {
                    let point = if let Some(point) = candidate.point(info.id) {
                        point
                    } else {
                        ensure_row_seq(db, rows, id, row, encoded, meter)?;
                        let row = row.as_ref().unwrap();
                        // Same stand-in charge as the non-driving arm above:
                        // `SpatialPostings` for a row-field extract, because
                        // the cover walk already gave up on this filter and
                        // there is no posting left to bill.
                        meter.charge(WorkResource::SpatialPostings, 1)?;
                        meter.note_row_decode();
                        let Some(point) = point_from_field(selected_field(row, &info.field)?)?
                        else {
                            return Ok(false);
                        };
                        point
                    };
                    match predicate {
                        PointFilter::Bbox(bounds) => bounds.contains(point),
                        PointFilter::Radius {
                            center,
                            radius_metres,
                        } => within_radius(*center, point, *radius_metres).map_err(corrupt_query)?,
                    }
                }
            },
            CompiledFilter::Geometry { info, predicate } => {
                ensure_row_seq(db, rows, id, row, encoded, meter)?;
                let row = row.as_ref().unwrap();
                meter.note_row_decode();
                let Some(geom) = geom_from_field(selected_field(row, &info.field)?)? else {
                    return Ok(false);
                };
                geometry_predicate_matches(predicate, &geom)
            }
            // Already answered by the position it folded into.
            CompiledFilter::Folded { .. } => true,
            CompiledFilter::Text(prepared) => text_score(
                db,
                rows,
                prepared,
                id,
                candidate.text.as_ref(),
                row,
                encoded,
                scratch,
                meter,
            )?
            .is_some(),
            // Reached only if a candidate arrived here uncertified, which
            // `prepare_query` refuses to compile: a key filter exists only at
            // the position `CandidateDriver::Keys` certifies, and `KeysCursor`
            // never yields an entry outside its own predicate.
            CompiledFilter::Key { .. } => {
                unreachable!("a key filter is always certified by CandidateDriver::Keys")
            }
        };
        if !matches {
            return Ok(false);
        }
    }
    Ok(true)
}
