//! Parameters as typed SLOTS: reading one `$n` into the typed value a
//! compiled node holds, and refilling that node when the same compiled
//! statement is RE-BOUND with new parameters.
//!
//! Two halves live here.
//!
//! [`Binder`] is the compiler's value reader with the compiler taken away:
//! everything it needs is the bound parameters and the database a scalar
//! subquery reads, so the same code serves a PREPARE (a literal read once
//! while the plan is built) and a REBIND (the same literal read again
//! against a new parameter list, with nothing parsed and nothing compiled).
//!
//! The `*Fill` types are the slots. A compiled node keeps, beside the value
//! it folded, the literal the statement wrote in that position -- but ONLY
//! when that literal is a `$n`, because a written constant cannot change and
//! so needs no slot. `QL_CONTRACT` §2: the plan is a plan, and the values it
//! compares against are the caller's.
//!
//! What is NOT a slot is folded at prepare and named as such: `now()` and
//! `current_date` (one clock per compiled statement, so every row of one
//! answer sees the same instant), a §4.1 / §4.2 range rewrite whose
//! pre-image is computed from the value, a semi-join's membership set, an
//! INSERT/UPDATE document. A statement that folds a PARAMETER into any of
//! those is marked `rebind: false` with the reason, and `EXPLAIN` prints it.

use super::*;
use crate::Param;

/// The compiler's value reader, without the compiler.
///
/// Holds only what reading a written value needs: the parameters, and the
/// database a scalar subquery is a point-get against.
pub(crate) struct Binder<'a> {
    pub(crate) db: &'a Database,
    pub(crate) params: &'a [Param],
}

impl<'a> Binder<'a> {
    pub(crate) fn new(db: &'a Database, params: &'a [Param]) -> Self {
        Self { db, params }
    }

    pub(crate) fn param(&self, n: usize) -> SqlResult2<&Param> {
        self.params.get(n - 1).ok_or_else(|| {
            SqlError::Parameter(format!(
                "${n} is not bound; {} parameter(s) were given",
                self.params.len()
            ))
        })
    }

    /// A literal, as JSON. A subquery runs here, because by the time the
    /// outer statement runs it is a constant.
    pub(crate) fn value_of(&self, literal: &Literal) -> SqlResult2<Value> {
        Ok(match literal {
            Literal::Null => Value::Null,
            Literal::Bool(b) => Value::Bool(*b),
            Literal::Num(v, exact) => {
                if *exact && v.fract() == 0.0 && v.abs() < 9.0e18 {
                    Value::from(*v as i64)
                } else {
                    Value::from(*v)
                }
            }
            Literal::Str(s) => Value::String(s.clone()),
            Literal::Param(n) => match self.param(*n)? {
                Param::Null => Value::Null,
                Param::Bool(b) => Value::Bool(*b),
                Param::Int(i) => Value::from(*i),
                Param::Float(f) => Value::from(*f),
                Param::Text(t) => Value::String(t.clone()),
                Param::Vector(v) => Value::from(v.clone()),
                Param::Json(v) => v.clone(),
            },
            Literal::Geo(argument) => geom_to_json(&self.geom_of(argument)?),
            Literal::Call(call) => {
                let mut args = Vec::with_capacity(call.args.len());
                for arg in &call.args {
                    args.push(match self.value_of(arg)? {
                        Value::Null => SqlValue::Null,
                        Value::Bool(b) => SqlValue::Bool(b),
                        Value::Number(n) => match n.as_i64() {
                            Some(i) => SqlValue::Int(i),
                            None => SqlValue::Float(n.as_f64().unwrap_or(f64::NAN)),
                        },
                        Value::String(s) => SqlValue::Text(s),
                        other => SqlValue::Json(other),
                    });
                }
                match crate::pgcrypto::call(&call.name, &args)? {
                    SqlValue::Text(t) => Value::String(t),
                    SqlValue::Bool(b) => Value::Bool(b),
                    SqlValue::Int(i) => Value::from(i),
                    _ => Value::Null,
                }
            }
            Literal::Subquery(query) => {
                let collection = collection(self.db, &query.table)?;
                let key = self.text_of(&query.key)?;
                let entity = self
                    .db
                    .get(collection, &key)
                    .map_err(SqlError::from)?
                    .ok_or_else(|| {
                        SqlError::engine(format!(
                            "scalar subquery: `{}` has no row at key `{key}`",
                            query.table
                        ))
                    })?;
                if query.column == KEY_COLUMN {
                    Value::String(entity.key)
                } else {
                    entity
                        .document
                        .get(&query.column)
                        .cloned()
                        .unwrap_or(Value::Null)
                }
            }
        })
    }

    pub(crate) fn text_of(&self, literal: &Literal) -> SqlResult2<String> {
        match self.value_of(literal)? {
            Value::String(s) => Ok(s),
            other => Err(SqlError::Parameter(format!("expected text, found {other}"))),
        }
    }

    pub(crate) fn f64_of(&self, literal: &Literal) -> SqlResult2<f64> {
        match self.value_of(literal)? {
            Value::Number(n) => n
                .as_f64()
                .ok_or_else(|| SqlError::Parameter("number is not finite".into())),
            other => Err(SqlError::Parameter(format!(
                "expected a number, found {other}"
            ))),
        }
    }

    pub(crate) fn i64_of(&self, literal: &Literal) -> SqlResult2<i64> {
        match self.value_of(literal)? {
            Value::Number(n) => n
                .as_i64()
                .ok_or_else(|| SqlError::Parameter("expected a whole number".into())),
            other => Err(SqlError::Parameter(format!(
                "expected a whole number, found {other}"
            ))),
        }
    }

    /// pgvector's text form `[a,b,c]`, a JSON array, or a bound
    /// `Param::Vector`.
    pub(crate) fn vector_of(&self, literal: &Literal) -> SqlResult2<Vec<f32>> {
        if let Literal::Param(n) = literal {
            if let Param::Vector(v) = self.param(*n)? {
                return Ok(v.clone());
            }
        }
        match self.value_of(literal)? {
            Value::String(text) => parse_vector_literal(&text),
            Value::Array(items) => items
                .iter()
                .map(|item| {
                    item.as_f64()
                        .map(|v| v as f32)
                        .ok_or_else(|| SqlError::Parameter("vector holds a non-number".into()))
                })
                .collect(),
            other => Err(SqlError::Parameter(format!(
                "expected a vector literal, found {other}"
            ))),
        }
    }

    pub(crate) fn point_of(&self, point: &PointArg) -> SqlResult2<Point> {
        let lon = self.f64_of(&point.lon)?;
        let lat = self.f64_of(&point.lat)?;
        Point::new(lon, lat)
            .map_err(|e| SqlError::engine(format!("ST_MakePoint({lon}, {lat}): {e}")))
    }

    pub(crate) fn geom_of(&self, argument: &GeoArg) -> SqlResult2<Geom> {
        geom_with(argument, &|literal| self.value_of(literal))
    }

    /// The rectangle `col && <shape>` compares a POINT column against, as
    /// the point index's inclusive `Bounds`.
    ///
    /// PostGIS compares float4 boxes rounded outward, so a point overlaps
    /// the box exactly when its own rounded-down coordinate is not past the
    /// box's rounded-up edge. On doubles that is: the point is below the
    /// NEXT float4 after the rounded-up edge. The bounds here are that edge,
    /// less one double, so the point index answers what PostGIS answers
    /// (`sql_geometry_io.rs`).
    pub(crate) fn overlap_bounds_of(&self, argument: &GeoArg) -> SqlResult2<Bounds> {
        let geometry = self.geom_of(argument)?;
        let (w, e, s, n) = geometry
            .bbox()
            .ok_or_else(|| SqlError::Parameter("&&: the shape has no coordinates".into()))?;
        let low = |v: f64, floor: f64| float4_reach_below(v).max(floor);
        let high = |v: f64, ceiling: f64| float4_reach_above(v).min(ceiling);
        Bounds::new(low(w, -180.0), high(e, 180.0), low(s, -90.0), high(n, 90.0))
            .map_err(|err| SqlError::engine(format!("&&: {err}")))
    }

    pub(crate) fn bounds_of(&self, argument: &GeoArg) -> SqlResult2<Bounds> {
        match argument {
            GeoArg::Envelope {
                minlon,
                minlat,
                maxlon,
                maxlat,
            } => {
                let (w, s, e, n) = (
                    self.f64_of(minlon)?,
                    self.f64_of(minlat)?,
                    self.f64_of(maxlon)?,
                    self.f64_of(maxlat)?,
                );
                Bounds::new(w, e, s, n)
                    .map_err(|err| SqlError::engine(format!("ST_MakeEnvelope: {err}")))
            }
            _ => Err(SqlError::unsupported(
                "a rectangle over a Point column is ST_Within(col, ST_MakeEnvelope(...)): PointFilter::Bbox is a lon/lat rectangle and has no other shape",
            )),
        }
    }

    /// One value inside the index's DECLARED domain. A scalar predicate does
    /// not coerce: `ScalarFilter` compares inside one `Kind`.
    pub(crate) fn scalar(
        &self,
        kind: &Kind,
        literal: &Literal,
        column: &str,
    ) -> SqlResult2<Scalar> {
        let value = self.value_of(literal)?;
        Ok(match (kind, &value) {
            (Kind::Int, Value::Number(n)) => Scalar::I64(n.as_i64().ok_or_else(|| {
                SqlError::Parameter(format!("`{column}` is INT and {n} is not a whole number"))
            })?),
            (Kind::Real, Value::Number(n)) => Scalar::F64(n.as_f64().ok_or_else(|| {
                SqlError::Parameter(format!("`{column}` is REAL and {n} is not a number"))
            })?),
            (Kind::Text, Value::String(s)) => Scalar::Text(s.clone()),
            (Kind::Bool, Value::Bool(b)) => Scalar::Bool(*b),
            _ => {
                return Err(SqlError::Parameter(format!(
                    "`{column}` is declared {kind:?} and the value is {value}; a scalar predicate stays inside the index's declared domain rather than coercing (`ScalarFilter`, src/query/mod.rs)"
                )))
            }
        })
    }

    /// The tsquery text, split into E4's `TextMatch`. A tsquery that mixes
    /// `&` and `|` is an AND/OR tree, which is Tier 2.
    pub(crate) fn tsquery(&self, query: &TsQuery) -> SqlResult2<(String, TextMatch)> {
        tsquery_of(self.text_of(&query.source)?, query)
    }
}

/// The float4 edge `v` rounds DOWN to (as PostGIS rounds a box's minimum),
/// and then the lowest double whose own float4 rounding-up still reaches it.
fn float4_reach_below(v: f64) -> f64 {
    let mut edge = v as f32;
    if f64::from(edge) > v {
        edge = f32::from_bits(if edge > 0.0 { edge.to_bits() - 1 } else { edge.to_bits() + 1 });
    }
    // A point's box maximum rounds UP; it meets `edge` when it is above the
    // float4 just below `edge`.
    let below = if edge > 0.0 {
        f32::from_bits(edge.to_bits() - 1)
    } else if edge == 0.0 {
        -f32::from_bits(1)
    } else {
        f32::from_bits(edge.to_bits() + 1)
    };
    let below = f64::from(below);
    f64::from_bits(if below >= 0.0 { below.to_bits() + 1 } else { below.to_bits() - 1 })
}

/// The mirror of [`float4_reach_below`] for a box's maximum.
fn float4_reach_above(v: f64) -> f64 {
    -float4_reach_below(-v)
}

/// True when a written value can change between binds -- that is, when it is
/// a `$n` or holds one. Everything else is a constant of the text and needs
/// no slot.
pub(crate) fn literal_is_bound(literal: &Literal) -> bool {
    match literal {
        Literal::Param(_) => true,
        // A scalar subquery reads a ROW, so its value is the database's and
        // not the text's: it is always a slot, and a bind runs the same
        // point-get again rather than serving what a prepare once read.
        Literal::Subquery(_) => true,
        Literal::Geo(argument) => geo_is_bound(argument),
        // A pgcrypto call is computed again at every bind: `gen_salt` and
        // `gen_random_bytes` must never serve one execution's randomness to
        // the next.
        Literal::Call(_) => true,
        _ => false,
    }
}

pub(crate) fn point_is_bound(point: &PointArg) -> bool {
    literal_is_bound(&point.lon) || literal_is_bound(&point.lat)
}

pub(crate) fn geo_is_bound(argument: &GeoArg) -> bool {
    match argument {
        GeoArg::Point(point) => point_is_bound(point),
        GeoArg::Envelope {
            minlon,
            minlat,
            maxlon,
            maxlat,
        } => [minlon, minlat, maxlon, maxlat]
            .iter()
            .any(|literal| literal_is_bound(literal)),
        GeoArg::GeoJson(literal) => literal_is_bound(literal),
        GeoArg::Encoded { source, .. } => literal_is_bound(source),
    }
}

// ── the slots ─────────────────────────────────────────────────────────────

/// Which half of a compiled scalar predicate a slot fills.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScalarAt {
    Eq,
    Lower,
    Upper,
}

/// One `$n` in a scalar predicate, with the declared kind it is read as.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ScalarFill {
    pub(crate) at: ScalarAt,
    pub(crate) literal: Literal,
    pub(crate) kind: Kind,
    pub(crate) column: String,
}

/// Which half of a compiled key range a slot fills.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyAt {
    Lower,
    Upper,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct KeyFill {
    pub(crate) at: KeyAt,
    pub(crate) literal: Literal,
}

/// A point predicate's slots. The whole predicate is rebuilt from the
/// written argument, because a radius is a centre AND a distance and only
/// one of the two may be a `$n`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PointFill {
    Radius { center: PointArg, metres: Literal },
    Bbox(GeoArg),
    /// `col && <shape>` over a point column.
    Overlaps(GeoArg),
}

/// A geometry predicate's slots.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GeomFill {
    pub(crate) predicate: SpatialPredicate,
    pub(crate) argument: GeoArg,
    pub(crate) metres: Option<Literal>,
}

// ── rebindability ─────────────────────────────────────────────────────────

/// Whether a compiled statement can be REFILLED with new parameters, and why
/// not when it cannot.
///
/// A refusal is recorded by the compile itself: every value reader that
/// FOLDS a parameter into the plan -- a range rewrite, a document, a
/// membership set, a session knob -- names the `$n` it folded, and a
/// statement with any such fold is not rebindable. The rule is
/// safe-by-default: a reader that does not produce a slot records a fold, so
/// a construct nobody taught to rebind refuses rather than serving a stale
/// value.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Rebind {
    pub(crate) refusals: Vec<String>,
}

impl Rebind {
    pub(crate) fn ok(&self) -> bool {
        self.refusals.is_empty()
    }

    /// The reason a rebind is refused, or `None` when it is not.
    pub(crate) fn reason(&self) -> Option<String> {
        if self.refusals.is_empty() {
            None
        } else {
            Some(self.refusals.join("; "))
        }
    }
}

/// The tsquery `text` of `query`, split into E4's `TextMatch`: the part of
/// [`Binder::tsquery`] after the text is known, shared with the GQL body,
/// which reads the text when an execution opens. A tsquery that mixes `&`
/// and `|` is an AND/OR tree, which is Tier 2.
pub(crate) fn tsquery_of(text: String, query: &TsQuery) -> SqlResult2<(String, TextMatch)> {
    // `search()` names its own match mode: no tsquery operator chooses
    // it, and no value can turn an ordinary text filter into one.
    if query.fuzzy {
        return Ok((text, TextMatch::Search));
    }
    if !query.tsquery_syntax {
        return Ok((text, TextMatch::Any));
    }
    let trimmed = text.trim();
    if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
        return Ok((
            trimmed[1..trimmed.len() - 1].trim().to_owned(),
            TextMatch::Phrase,
        ));
    }
    if trimmed.contains('\'') {
        let inner = trimmed.trim_matches('\'').trim();
        if inner.split_whitespace().count() > 1 {
            return Ok((inner.to_owned(), TextMatch::Phrase));
        }
    }
    let has_or = trimmed.contains('|');
    let has_and = trimmed.contains('&');
    if has_or && has_and {
        return Err(SqlError::Refused {
            keyword: "tsquery & |".into(),
            tier: Tier::Two,
            reason: "QL_CONTRACT §4.6: a tsquery that mixes `&` and `|` is a boolean TREE inside one index's postings; the Tier-1 tsquery is one operator, and a union ACROSS predicates is written with SQL's own OR.",
        });
    }
    if trimmed.contains('!') {
        return Err(SqlError::Refused {
            keyword: "tsquery !".into(),
            tier: Tier::Two,
            reason: "QL_CONTRACT §4.6: a tsquery `!` inside a larger tsquery is a boolean TREE over one index's postings; the Tier-1 spelling is `!term` alone, which is NOT over the text set.",
        });
    }
    if trimmed.contains("<->") {
        return Err(SqlError::Refused {
            keyword: "tsquery <->".into(),
            tier: Tier::Two,
            reason: "QL_CONTRACT §4.6: a tsquery distance operator is a positional constraint; the Tier-1 phrase atomic is a quoted phrase (TextMatch::Phrase).",
        });
    }
    let separator = if has_or { '|' } else { '&' };
    let terms: Vec<&str> = trimmed
        .split(separator)
        .map(str::trim)
        .filter(|term| !term.is_empty())
        .collect();
    if terms.iter().any(|term| term.contains(':')) {
        return Err(SqlError::Refused {
            keyword: "tsquery weight".into(),
            tier: Tier::Three,
            reason: "QL_CONTRACT §4.6: tsvector weights have no atomic; analyzer v1 stores one weight per token.",
        });
    }
    let matching = if has_or || terms.len() == 1 {
        TextMatch::Any
    } else {
        TextMatch::All
    };
    Ok((terms.join(" "), matching))
}

/// The geometry `argument` constructs, each literal read through `value_of`:
/// the part of [`Binder::geom_of`] shared with the GQL body, which reads its
/// parameters when an execution opens.
pub(crate) fn geom_with(argument: &GeoArg, value_of: &dyn Fn(&Literal) -> SqlResult2<Value>) -> SqlResult2<Geom> {
    let f64_of = |literal: &Literal| match value_of(literal)? {
        Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| SqlError::Parameter("number is not finite".into())),
        other => Err(SqlError::Parameter(format!("expected a number, found {other}"))),
    };
    let text_of = |literal: &Literal| match value_of(literal)? {
        Value::String(s) => Ok(s),
        other => Err(SqlError::Parameter(format!("expected text, found {other}"))),
    };
    Ok(match argument {
        GeoArg::Point(point) => {
            let (lon, lat) = (f64_of(&point.lon)?, f64_of(&point.lat)?);
            let p = Point::new(lon, lat)
                .map_err(|e| SqlError::engine(format!("ST_MakePoint({lon}, {lat}): {e}")))?;
            Geom::Point(p.longitude(), p.latitude())
        }
        GeoArg::Envelope {
            minlon,
            minlat,
            maxlon,
            maxlat,
        } => {
            let (w, s, e, n) = (f64_of(minlon)?, f64_of(minlat)?, f64_of(maxlon)?, f64_of(maxlat)?);
            Geom::Polygon(vec![vec![[w, s], [e, s], [e, n], [w, n], [w, s]]])
        }
        GeoArg::GeoJson(literal) => match value_of(literal)? {
            Value::String(text) => geom_from_text(&text)?,
            document => geom_from_json(&document)?,
        },
        GeoArg::Encoded { source, format } => {
            let text = text_of(source)?;
            match format {
                GeoFormat::Wkb => geom_from_wkb_hex(&text)?,
                GeoFormat::Wkt => geom_from_wkt(&text)?,
            }
        }
    })
}
