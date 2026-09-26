//! The host forms inside a GQL body (M6-A, M6-B of
//! `docs/lang/GQL_PROFILE_DESIGN_M5_M7.md` §3.1-§3.4): full-text match and
//! `bm25`, the `ST_*` predicates and `ST_Distance`, and the vector distance
//! operators `<=>`, `<->`, `<#>`.
//!
//! Each form keeps ONE spelling with SQL: the argument shapes are read by the
//! SQL sub-parsers (`Parser::tsquery`, `Parser::spatial_rest`,
//! `Parser::distance_rest`), which also hold the unit and CRS rules of
//! `QL_CONTRACT` §4.4, and a literal inside a shape or a tsquery is read
//! through the same functions the SQL compiler uses (`compile::geom_with`,
//! `compile::tsquery_of`), with the execution's `$n` values.
//!
//! Evaluation per row (§3.4): a spatial predicate or distance, and a vector
//! distance, are PURE over two values the reader handed over, computed by
//! core's own functions (`spatial_geometry`, `vector_distance`); a text
//! match or `bm25` reads the node's text index through
//! `ElementReader::{text_matches, text_score}` (M6-C). A node whose label has
//! no text index on the field is refused when the statement is bound (Q27):
//! there is no analyzer outside an index, so a per-row emulation would be a
//! second tokenizer whose answers could drift from the index's.

use super::ast::Expr;
use super::eval::{mismatch, raise, Evaluated, Fault};
use super::expr::Ex;
use crate::ast::{GeoArg, GeoFormat, Literal as SqlLiteral, SpatialPredicate, TsQuery, VecOp};
use crate::compile::{geom_from_json, geom_with, parse_vector_literal, tsquery_of};
use crate::sqlstate::DATA_EXCEPTION;
use sekejap_core::collections::gql::{BindingValue, SlotId, ValueType};
use sekejap_core::collections::{
    vector_distance, CollectionId, GeometryFilter, IndexId, PointFilter, VectorMetric,
};
use sekejap_core::spatial_math::{Bounds, Point};
use sekejap_core::spatial_geometry;
use serde_json::Value;
use std::fmt;

/// A host form as written.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Host {
    /// `to_tsvector('simple', v.p) @@ to_tsquery('simple', q)`, or, `score`,
    /// `bm25(v.p, q)`. `target` is the property.
    Text {
        target: Box<Expr>,
        query: TsQuery,
        score: bool,
    },
    /// `ST_DWithin` / `ST_Intersects` / `ST_Within` / `ST_Contains` of the
    /// property `target` and a written shape; `metres` for `ST_DWithin`.
    Spatial {
        predicate: SpatialPredicate,
        target: Box<Expr>,
        shape: GeoArg,
        metres: Option<SqlLiteral>,
    },
    /// `ST_Distance(target, shape)`, in metres.
    Distance { target: Box<Expr>, shape: GeoArg },
    /// `left <=> right`, `<->`, `<#>`: pgvector's distances.
    Vector {
        op: VecOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
}

impl Host {
    /// The expressions this one is built from.
    pub(crate) fn children(&self) -> Vec<&Expr> {
        match self {
            Self::Text { target, .. } | Self::Spatial { target, .. } | Self::Distance { target, .. } => {
                vec![&**target]
            }
            Self::Vector { left, right, .. } => vec![&**left, &**right],
        }
    }
}

/// A host form, lowered.
#[derive(Clone, Debug)]
pub(crate) enum HostEx {
    /// The node in `node`, tested or scored through the text index of its
    /// collection, one per label collection.
    Text {
        node: SlotId,
        field: Box<str>,
        indexes: Box<[(CollectionId, IndexId)]>,
        query: TsQuery,
        score: bool,
    },
    Spatial {
        predicate: SpatialPredicate,
        left: Ex,
        shape: GeoArg,
        metres: Option<SqlLiteral>,
    },
    Distance {
        left: Ex,
        shape: GeoArg,
    },
    Vector {
        op: VecOp,
        left: Ex,
        right: Ex,
    },
}

impl HostEx {
    /// The expressions this one is built from.
    pub(crate) fn children(&self) -> Vec<&Ex> {
        match self {
            Self::Text { .. } => Vec::new(),
            Self::Spatial { left, .. } | Self::Distance { left, .. } => vec![left],
            Self::Vector { left, right, .. } => vec![left, right],
        }
    }

    /// The slot a text form reads its node from, which no child names.
    pub(crate) fn node_slot(&self) -> Option<SlotId> {
        match self {
            Self::Text { node, .. } => Some(*node),
            _ => None,
        }
    }

    /// The value type of the form.
    pub(crate) fn value_type(&self) -> ValueType {
        match self {
            Self::Text { score: false, .. } | Self::Spatial { .. } => ValueType::Bool,
            Self::Text { score: true, .. } | Self::Distance { .. } | Self::Vector { .. } => ValueType::Float,
        }
    }

    /// Each `$n` a literal of the form reads, numbered from 1, with the type
    /// its place gives it: a tsquery or a GeoJSON shape is text, a
    /// coordinate, an envelope edge or a radius double precision.
    pub(crate) fn param_types(&self) -> Vec<(usize, ValueType)> {
        let mut out = Vec::new();
        let mut typed = |literal: &SqlLiteral, ty: ValueType| {
            if let SqlLiteral::Param(n) = literal {
                out.push((*n, ty));
            }
        };
        let shape = |shape: &GeoArg, typed: &mut dyn FnMut(&SqlLiteral, ValueType)| match shape {
            GeoArg::Point(point) => {
                typed(&point.lon, ValueType::Float);
                typed(&point.lat, ValueType::Float);
            }
            GeoArg::Envelope {
                minlon,
                minlat,
                maxlon,
                maxlat,
            } => [minlon, minlat, maxlon, maxlat].into_iter().for_each(|edge| typed(edge, ValueType::Float)),
            GeoArg::GeoJson(literal) | GeoArg::Encoded { source: literal, .. } => typed(literal, ValueType::Text),
        };
        match self {
            Self::Text { query, .. } => typed(&query.source, ValueType::Text),
            Self::Spatial { shape: s, metres, .. } => {
                shape(s, &mut typed);
                if let Some(metres) = metres {
                    typed(metres, ValueType::Float);
                }
            }
            Self::Distance { shape: s, .. } => shape(s, &mut typed),
            Self::Vector { .. } => {}
        }
        out
    }

    /// Every `$n` a literal of the form reads, numbered from 1 as SQL's
    /// `Literal::Param` is (a lowered `Ex::Param` is numbered from 0).
    pub(crate) fn params(&self) -> Vec<usize> {
        let mut out = Vec::new();
        let mut literal = |literal: &SqlLiteral| {
            if let SqlLiteral::Param(n) = literal {
                out.push(*n);
            }
        };
        match self {
            Self::Text { query, .. } => literal(&query.source),
            Self::Spatial { shape, metres, .. } => {
                shape_literals(shape).into_iter().for_each(&mut literal);
                metres.iter().for_each(literal);
            }
            Self::Distance { shape, .. } => shape_literals(shape).into_iter().for_each(literal),
            Self::Vector { .. } => {}
        }
        out
    }

    /// The form as `EXPLAIN` prints it, `show` printing a sub-expression.
    pub(crate) fn show(&self, show: &dyn Fn(&Ex) -> String) -> String {
        match self {
            Self::Text {
                field,
                query,
                score: false,
                ..
            } => format!("to_tsvector('simple', <node>.{field}) @@ to_tsquery('simple', {})", lit(&query.source)),
            Self::Text { field, query, .. } => format!("bm25(<node>.{field}, {})", lit(&query.source)),
            Self::Spatial {
                predicate,
                left,
                shape,
                metres,
            } => match metres {
                Some(metres) => format!("{}({}, {}, {})", spatial(*predicate), show(left), shape_text(shape), lit(metres)),
                None => format!("{}({}, {})", spatial(*predicate), show(left), shape_text(shape)),
            },
            Self::Distance { left, shape } => format!("ST_DISTANCE({}, {})", show(left), shape_text(shape)),
            Self::Vector { op, left, right } => format!("({} {} {})", show(left), vec_op(*op), show(right)),
        }
    }
}

/// Every literal a written shape holds.
fn shape_literals(shape: &GeoArg) -> Vec<&SqlLiteral> {
    match shape {
        GeoArg::Point(point) => vec![&point.lon, &point.lat],
        GeoArg::Envelope {
            minlon,
            minlat,
            maxlon,
            maxlat,
        } => vec![minlon, minlat, maxlon, maxlat],
        GeoArg::GeoJson(literal) => vec![literal],
        GeoArg::Encoded { source, .. } => vec![source],
    }
}

// ── evaluation (M6-B) ─────────────────────────────────────────────────────

/// The JSON value a literal of a written shape or tsquery stands for, `$n`
/// read from `params`.
pub(crate) fn literal_value(literal: &SqlLiteral, params: &[BindingValue]) -> Evaluated<Value> {
    Ok(match literal {
        SqlLiteral::Null => Value::Null,
        SqlLiteral::Bool(b) => Value::Bool(*b),
        SqlLiteral::Num(v, exact) if *exact && v.fract() == 0.0 && v.abs() < 9.0e18 => Value::from(*v as i64),
        SqlLiteral::Num(v, _) => Value::from(*v),
        SqlLiteral::Str(s) => Value::String(s.clone()),
        // Numbered from 1, and checked against the count at bind.
        SqlLiteral::Param(n) => match &params[*n - 1] {
            BindingValue::Null => Value::Null,
            BindingValue::Bool(b) => Value::Bool(*b),
            BindingValue::Int(i) => Value::from(*i),
            BindingValue::Float(f) => Value::from(*f),
            BindingValue::Text(t) => Value::String(t.to_string()),
            BindingValue::Json(j) | BindingValue::Geo(j) => (**j).clone(),
            other => return Err(mismatch(format!("${n} is {other:?}, not a value a shape or a query takes"))),
        },
        other => return Err(mismatch(format!("`{other:?}` is not a literal a GQL host form takes"))),
    })
}

/// A text form over the node `node`: whether its field matches, or its BM25
/// score, through the text index of the node's collection.
pub(crate) fn text_query(query: &TsQuery, params: &[BindingValue]) -> Evaluated<(String, sekejap_core::collections::TextMatch)> {
    let text = match literal_value(&query.source, params)? {
        Value::String(text) => text,
        Value::Null => return Err(mismatch("a text query is NULL")),
        other => return Err(mismatch(format!("a text query is text, not {other}"))),
    };
    tsquery_of(text, query).map_err(Fault::Sql)
}

/// The index a text form reads for `collection`, from the ones bound.
pub(crate) fn text_index(indexes: &[(CollectionId, IndexId)], collection: CollectionId) -> Option<IndexId> {
    indexes.iter().find(|(c, _)| *c == collection).map(|(_, index)| *index)
}

/// A spatial seed filter in the engine's form.
pub(crate) enum SpatialSeed {
    Point(PointFilter),
    Geometry(GeometryFilter),
}

/// The engine filter a spatial seed conjunct stands for, its literals read
/// with this execution's `$n`, as SQL builds it
/// (`compile/predicates.rs::spatial`). `None`: no row passes (a `NULL`
/// radius). A point index answers ST_DWithin to a point and ST_Within an
/// envelope, which the planner checked; a geometry index every predicate.
pub(crate) fn spatial_seed(
    point: bool,
    predicate: SpatialPredicate,
    shape: &GeoArg,
    metres: Option<&SqlLiteral>,
    params: &[BindingValue],
) -> Evaluated<Option<SpatialSeed>> {
    let number = |literal: &SqlLiteral, what: &str| -> Evaluated<Option<f64>> {
        match literal_value(literal, params)? {
            Value::Number(n) => Ok(n.as_f64()),
            Value::Null => Ok(None),
            other => Err(mismatch(format!("{what} is a number, not {other}"))),
        }
    };
    let radius = match (predicate, metres) {
        (SpatialPredicate::DWithin, Some(metres)) => match number(metres, "ST_DWithin's radius")? {
            Some(metres) => Some(metres),
            None => return Ok(None),
        },
        _ => None,
    };
    if !point {
        let geometry = geom_with(shape, &|literal| literal_value(literal, params).map_err(fault_sql)).map_err(Fault::Sql)?;
        return Ok(Some(SpatialSeed::Geometry(match predicate {
            SpatialPredicate::DWithin => GeometryFilter::DWithin {
                geometry,
                metres: radius.unwrap_or(f64::NAN),
            },
            SpatialPredicate::Intersects => GeometryFilter::Intersects(geometry),
            SpatialPredicate::Within => GeometryFilter::Within(geometry),
            SpatialPredicate::Contains => GeometryFilter::Contains(geometry),
            SpatialPredicate::Overlaps => GeometryFilter::Overlaps(geometry),
        })));
    }
    let engine = |e: String| raise(DATA_EXCEPTION, e);
    Ok(Some(SpatialSeed::Point(match (predicate, shape) {
        (SpatialPredicate::DWithin, GeoArg::Point(centre)) => {
            let (Some(lon), Some(lat)) = (number(&centre.lon, "a longitude")?, number(&centre.lat, "a latitude")?) else {
                return Ok(None);
            };
            PointFilter::Radius {
                center: Point::new(lon, lat).map_err(|e| engine(format!("ST_MakePoint({lon}, {lat}): {e}")))?,
                radius_metres: radius.unwrap_or(f64::NAN),
            }
        }
        (
            SpatialPredicate::Within,
            GeoArg::Envelope {
                minlon,
                minlat,
                maxlon,
                maxlat,
            },
        ) => {
            let edge = |literal| number(literal, "an envelope edge");
            let [Some(w), Some(s), Some(e), Some(n)] = [edge(minlon)?, edge(minlat)?, edge(maxlon)?, edge(maxlat)?] else {
                return Ok(None);
            };
            PointFilter::Bbox(Bounds::new(w, e, s, n).map_err(|err| engine(format!("ST_MakeEnvelope: {err}")))?)
        }
        _ => unreachable!("the planner seeds a point index from ST_DWithin to a point or ST_Within an envelope"),
    })))
}

/// A spatial predicate or distance over the geometry `left` and the written
/// `shape`: `Null` when `left` is.
pub(crate) fn spatial_value(
    predicate: Option<SpatialPredicate>,
    left: &BindingValue,
    shape: &GeoArg,
    metres: Option<&SqlLiteral>,
    params: &[BindingValue],
) -> Evaluated<BindingValue> {
    let left = match left {
        BindingValue::Null => return Ok(BindingValue::Null),
        BindingValue::Geo(value) | BindingValue::Json(value) => geom_from_json(value).map_err(Fault::Sql)?,
        other => return Err(mismatch(format!("a spatial function takes a geometry, not {other:?}"))),
    };
    let right = geom_with(shape, &|literal| literal_value(literal, params).map_err(fault_sql)).map_err(Fault::Sql)?;
    Ok(match predicate {
        None => BindingValue::Float(spatial_geometry::distance_m(&left, &right)),
        Some(SpatialPredicate::DWithin) => {
            let metres = match metres.map(|m| literal_value(m, params)).transpose()? {
                Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
                Some(Value::Null) | None => return Ok(BindingValue::Null),
                Some(other) => return Err(mismatch(format!("ST_DWithin's radius is a number, not {other}"))),
            };
            BindingValue::Bool(spatial_geometry::dwithin_m(&left, &right, metres))
        }
        Some(SpatialPredicate::Intersects) => BindingValue::Bool(spatial_geometry::intersects(&left, &right)),
        Some(SpatialPredicate::Within) => BindingValue::Bool(spatial_geometry::within(&left, &right)),
        Some(SpatialPredicate::Contains) => BindingValue::Bool(spatial_geometry::contains(&left, &right)),
        Some(SpatialPredicate::Overlaps) => BindingValue::Bool(spatial_geometry::bbox_overlaps(&left, &right)),
    })
}

/// A `geom_with` literal reader cannot carry a `Fault`: an engine fault
/// cannot arise reading a literal, so only the SQL error is kept.
fn fault_sql(fault: Fault) -> crate::SqlError {
    match fault {
        Fault::Sql(error) => error,
        Fault::Engine(error) => crate::SqlError::engine(error.to_string()),
    }
}

/// pgvector's distance of `op` between two vectors: `Null` when either is
/// `Null` or a cosine distance meets an all-zero vector; two widths is a
/// data error (`22000`).
pub(crate) fn vector_value(op: VecOp, left: &BindingValue, right: &BindingValue) -> Evaluated<(BindingValue, usize)> {
    let (Some(left), Some(right)) = (lanes(left)?, lanes(right)?) else {
        return Ok((BindingValue::Null, 0));
    };
    if left.len() != right.len() {
        return Err(raise(
            DATA_EXCEPTION,
            format!("different vector dimensions {} and {}", left.len(), right.len()),
        ));
    }
    let metric = match op {
        VecOp::Cosine => VectorMetric::Cosine,
        VecOp::L2 => VectorMetric::SquaredL2,
        VecOp::NegativeDot => VectorMetric::NegativeDot,
    };
    let value = match vector_distance(&left, &right, metric) {
        None => BindingValue::Null,
        // `<->` is the Euclidean distance; the engine keeps its square.
        Some(squared) if op == VecOp::L2 => BindingValue::Float(squared.sqrt()),
        Some(distance) => BindingValue::Float(distance),
    };
    Ok((value, left.len()))
}

/// A vector value's lanes: a stored vector, a `'[...]'` literal, or a JSON
/// array; `None` for `Null`.
pub(crate) fn lanes(value: &BindingValue) -> Evaluated<Option<Vec<f32>>> {
    Ok(Some(match value {
        BindingValue::Null => return Ok(None),
        BindingValue::Vector(lanes) => lanes.to_vec(),
        BindingValue::Text(text) => parse_vector_literal(text).map_err(Fault::Sql)?,
        BindingValue::Json(json) => match &**json {
            Value::Array(items) => items
                .iter()
                .map(|item| item.as_f64().map(|v| v as f32))
                .collect::<Option<Vec<f32>>>()
                .ok_or_else(|| mismatch("a vector holds a non-number"))?,
            other => return Err(mismatch(format!("a vector distance takes a vector, not {other}"))),
        },
        other => return Err(mismatch(format!("a vector distance takes a vector, not {other:?}"))),
    }))
}

// ── printing ──────────────────────────────────────────────────────────────

fn spatial(predicate: SpatialPredicate) -> &'static str {
    match predicate {
        SpatialPredicate::DWithin => "ST_DWITHIN",
        SpatialPredicate::Intersects => "ST_INTERSECTS",
        SpatialPredicate::Within => "ST_WITHIN",
        SpatialPredicate::Contains => "ST_CONTAINS",
        SpatialPredicate::Overlaps => "&&",
    }
}

fn vec_op(op: VecOp) -> &'static str {
    match op {
        VecOp::Cosine => "<=>",
        VecOp::L2 => "<->",
        VecOp::NegativeDot => "<#>",
    }
}

fn lit(literal: &SqlLiteral) -> String {
    match literal {
        SqlLiteral::Null => "NULL".to_owned(),
        SqlLiteral::Bool(b) => b.to_string().to_uppercase(),
        SqlLiteral::Num(v, _) => v.to_string(),
        SqlLiteral::Str(s) => format!("'{}'", s.replace('\'', "''")),
        SqlLiteral::Param(n) => format!("${}", n + 1),
        other => format!("{other:?}"),
    }
}

fn shape_text(shape: &GeoArg) -> String {
    match shape {
        GeoArg::Point(point) => format!("ST_MAKEPOINT({}, {})", lit(&point.lon), lit(&point.lat)),
        GeoArg::Envelope {
            minlon,
            minlat,
            maxlon,
            maxlat,
        } => format!("ST_MAKEENVELOPE({}, {}, {}, {}, 4326)", lit(minlon), lit(minlat), lit(maxlon), lit(maxlat)),
        GeoArg::GeoJson(literal) => format!("ST_GEOMFROMGEOJSON({})", lit(literal)),
        GeoArg::Encoded {
            source,
            format: GeoFormat::Wkb,
        } => format!("ST_GEOMFROMWKB({})", lit(source)),
        GeoArg::Encoded {
            source,
            format: GeoFormat::Wkt,
        } => format!("ST_GEOMFROMTEXT({})", lit(source)),
    }
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text {
                target,
                query,
                score: false,
            } => write!(f, "to_tsvector('simple', {target}) @@ to_tsquery('simple', {})", lit(&query.source)),
            Self::Text { target, query, .. } => write!(f, "bm25({target}, {})", lit(&query.source)),
            Self::Spatial {
                predicate,
                target,
                shape,
                metres: Some(metres),
            } => write!(f, "{}({target}, {}, {})", spatial(*predicate), shape_text(shape), lit(metres)),
            Self::Spatial {
                predicate,
                target,
                shape,
                ..
            } => write!(f, "{}({target}, {})", spatial(*predicate), shape_text(shape)),
            Self::Distance { target, shape } => write!(f, "ST_DISTANCE({target}, {})", shape_text(shape)),
            Self::Vector { op, left, right } => write!(f, "({left} {} {right})", vec_op(*op)),
        }
    }
}
