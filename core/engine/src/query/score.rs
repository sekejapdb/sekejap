//! The score expression: its compiled form and its evaluation.
//! See docs/lang/QL_CONTRACT.md,
//! "Contract and public shape".
use super::*;

#[derive(Clone, Debug)]
pub(super) enum CompiledScoreExpr {
    Lit(f64),
    Scalar {
        info: IndexInfo,
    },
    Bm25(PreparedText),
    /// `search_score()`: the same `PreparedText` a `TextMatch::Search`
    /// predicate prepares, scored by its own [0,1] formula rather than BM25.
    SearchScore(PreparedText),
    VectorSimilarity {
        info: IndexInfo,
        query: Vec<f32>,
        query_norm: f64,
        metric: VectorMetric,
    },
    Distance {
        info: IndexInfo,
        center: Point,
    },
    Add(Box<CompiledScoreExpr>, Box<CompiledScoreExpr>),
    Sub(Box<CompiledScoreExpr>, Box<CompiledScoreExpr>),
    Mul(Box<CompiledScoreExpr>, Box<CompiledScoreExpr>),
    Div(Box<CompiledScoreExpr>, Box<CompiledScoreExpr>),
    Neg(Box<CompiledScoreExpr>),
}

impl CompiledScoreExpr {
    pub(super) fn vector_similarity_leaf(&self) -> Option<&IndexInfo> {
        match self {
            Self::VectorSimilarity { info, .. } => Some(info),
            _ => None,
        }
    }

    pub(super) fn uses_scalar(&self, index: IndexId) -> bool {
        match self {
            Self::Scalar { info } => info.id == index,
            Self::Add(a, b) | Self::Sub(a, b) | Self::Mul(a, b) | Self::Div(a, b) => {
                a.uses_scalar(index) || b.uses_scalar(index)
            }
            Self::Neg(a) => a.uses_scalar(index),
            _ => false,
        }
    }

    pub(super) fn needs_row(&self, driver: &DriverPlan) -> bool {
        match self {
            Self::Lit(_) | Self::VectorSimilarity { .. } => false,
            Self::Bm25(prepared) => prepared.phrase.is_some(),
            // A search score is the automaton's own numbers over the
            // postings: no row, ever.
            Self::SearchScore(_) => false,
            Self::Scalar { info } => match driver {
                DriverPlan::Scalar {
                    info: driving, ..
                } if driving.id == info.id => false,
                _ => true,
            },
            Self::Distance { info, .. } => match driver {
                DriverPlan::Nearest {
                    info: driving, ..
                }
                | DriverPlan::Spatial {
                    info: driving, ..
                } if driving.id == info.id => false,
                _ => true,
            },
            Self::Add(a, b) | Self::Sub(a, b) | Self::Mul(a, b) | Self::Div(a, b) => {
                a.needs_row(driver) || b.needs_row(driver)
            }
            Self::Neg(a) => a.needs_row(driver),
        }
    }

    pub(super) fn mark_text_driven(&mut self, driving: &PreparedText, source: TextSource, carries: bool) {
        match self {
            Self::Bm25(prepared) | Self::SearchScore(prepared) => {
                prepared.driven = (carries && prepared.same_terms(driving)).then_some(source);
            }
            Self::Add(a, b) | Self::Sub(a, b) | Self::Mul(a, b) | Self::Div(a, b) => {
                a.mark_text_driven(driving, source, carries);
                b.mark_text_driven(driving, source, carries);
            }
            Self::Neg(a) => a.mark_text_driven(driving, source, carries),
            _ => {}
        }
    }
}

pub(super) fn compile_score_expr(
    db: &Database,
    collection: CollectionId,
    expr: &ScoreExpr<'_>,
    depth: usize,
    leaves: &mut usize,
) -> QueryResult<CompiledScoreExpr> {
    if depth > MAX_SCORE_DEPTH {
        return Err(invalid_query("score expression depth exceeds 32"));
    }
    let leaf = |leaves: &mut usize| -> QueryResult<()> {
        *leaves = leaves.saturating_add(1);
        if *leaves > MAX_SCORE_LEAVES {
            Err(invalid_query("score expression has more than 8 leaves"))
        } else {
            Ok(())
        }
    };
    match expr {
        ScoreExpr::Lit(value) => {
            leaf(leaves)?;
            Ok(CompiledScoreExpr::Lit(*value))
        }
        ScoreExpr::Scalar { index } => {
            leaf(leaves)?;
            let info = require_scalar_index(db, collection, *index)?;
            if !matches!(info.kind, Kind::Int | Kind::Real | Kind::Bool) {
                return Err(invalid_query(
                    "score scalar requires an Int, Real or Bool scalar index",
                ));
            }
            Ok(CompiledScoreExpr::Scalar { info })
        }
        ScoreExpr::Bm25 {
            index,
            query,
            matching,
        } => {
            leaf(leaves)?;
            Ok(CompiledScoreExpr::Bm25(prepare_text(
                db, collection, *index, query, *matching,
            )?))
        }
        ScoreExpr::SearchScore { index, query } => {
            leaf(leaves)?;
            Ok(CompiledScoreExpr::SearchScore(prepare_text(
                db,
                collection,
                *index,
                query,
                TextMatch::Search,
            )?))
        }
        ScoreExpr::VectorSimilarity {
            index,
            query,
            metric,
        } => {
            leaf(leaves)?;
            let (info, query, query_norm) =
                prepare_vector(db, collection, *index, query, *metric)?;
            Ok(CompiledScoreExpr::VectorSimilarity {
                info,
                query,
                query_norm,
                metric: *metric,
            })
        }
        ScoreExpr::Distance { index, center } => {
            leaf(leaves)?;
            let info = require_family_index(
                db,
                collection,
                *index,
                IndexFamily::SpatialPoint,
                "spatial-point",
            )?;
            crate::index::spatial::point::descriptor(&info)?;
            Ok(CompiledScoreExpr::Distance {
                info,
                center: *center,
            })
        }
        ScoreExpr::Add(left, right) => Ok(CompiledScoreExpr::Add(
            Box::new(compile_score_expr(db, collection, left, depth + 1, leaves)?),
            Box::new(compile_score_expr(
                db, collection, right, depth + 1, leaves,
            )?),
        )),
        ScoreExpr::Sub(left, right) => Ok(CompiledScoreExpr::Sub(
            Box::new(compile_score_expr(db, collection, left, depth + 1, leaves)?),
            Box::new(compile_score_expr(
                db, collection, right, depth + 1, leaves,
            )?),
        )),
        ScoreExpr::Mul(left, right) => Ok(CompiledScoreExpr::Mul(
            Box::new(compile_score_expr(db, collection, left, depth + 1, leaves)?),
            Box::new(compile_score_expr(
                db, collection, right, depth + 1, leaves,
            )?),
        )),
        ScoreExpr::Div(left, right) => Ok(CompiledScoreExpr::Div(
            Box::new(compile_score_expr(db, collection, left, depth + 1, leaves)?),
            Box::new(compile_score_expr(
                db, collection, right, depth + 1, leaves,
            )?),
        )),
        ScoreExpr::Neg(inner) => Ok(CompiledScoreExpr::Neg(Box::new(compile_score_expr(
            db, collection, inner, depth + 1, leaves,
        )?))),
    }
}

pub(super) fn eval_score_expr<'a, C: FnMut() -> bool>(
    db: &'a Database,
    rows: &mut PrimaryRows<'a>,
    expr: &CompiledScoreExpr,
    candidate: &Candidate,
    row: &mut Option<RowData>,
    encoded: &mut Option<Vec<u8>>,
    scratch: &mut RowScratch,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<f64> {
    match expr {
        CompiledScoreExpr::Lit(value) => Ok(*value),
        CompiledScoreExpr::Scalar { info } => {
            let key = if let Some(key) = candidate.scalar(info.id) {
                Some(key.to_vec())
            } else {
                let fresh = row.is_none();
                ensure_row_seq(db, rows, candidate.id, row, encoded, meter)?;
                if fresh {
                    meter.note_row_decode();
                }
                persisted_scalar_key(info, selected_field(row.as_ref().unwrap(), &info.field)?)?
            };
            match key {
                Some(key) => scalar_key_to_score(info, &key),
                None => Ok(0.0),
            }
        }
        CompiledScoreExpr::Bm25(prepared) => Ok(text_score(
            db,
            rows,
            prepared,
            candidate.id,
            candidate.text.as_ref(),
            row,
            encoded,
            scratch,
            meter,
        )?
        .unwrap_or(0.0)),
        CompiledScoreExpr::SearchScore(prepared) => Ok(search_quality(
            db,
            prepared,
            candidate.id,
            candidate.text.as_ref(),
            scratch,
            meter,
        )?
        .unwrap_or(0.0)),
        CompiledScoreExpr::VectorSimilarity {
            info,
            query,
            query_norm,
            metric,
        } => {
            let Some(distance) =
                vector_score(db, candidate, info, query, *query_norm, *metric, meter)?
            else {
                // No stored vector: the worst possible similarity under both
                // directions, so a plan that enumerates vector-less rows
                // (Entities) ranks them where ExactVector's driver omits them.
                return Ok(f64::NEG_INFINITY);
            };
            Ok(-distance)
        }
        CompiledScoreExpr::Distance { info, center } => {
            let distance = if let Some(distance) = candidate.distance_metres(info.id) {
                Some(distance)
            } else if let Some(point) = candidate.point(info.id) {
                Some(wgs84_distance_metres(*center, point))
            } else {
                let fresh = row.is_none();
                ensure_row_seq(db, rows, candidate.id, row, encoded, meter)?;
                if fresh {
                    meter.note_row_decode();
                }
                match point_from_field(selected_field(row.as_ref().unwrap(), &info.field)?)? {
                    Some(point) => Some(wgs84_distance_metres(*center, point)),
                    None => None,
                }
            };
            Ok(distance.unwrap_or(f64::INFINITY))
        }
        CompiledScoreExpr::Add(left, right) => Ok(eval_score_expr(
            db, rows, left, candidate, row, encoded, scratch, meter,
        )? + eval_score_expr(
            db, rows, right, candidate, row, encoded, scratch, meter,
        )?),
        CompiledScoreExpr::Sub(left, right) => Ok(eval_score_expr(
            db, rows, left, candidate, row, encoded, scratch, meter,
        )? - eval_score_expr(
            db, rows, right, candidate, row, encoded, scratch, meter,
        )?),
        CompiledScoreExpr::Mul(left, right) => Ok(eval_score_expr(
            db, rows, left, candidate, row, encoded, scratch, meter,
        )? * eval_score_expr(
            db, rows, right, candidate, row, encoded, scratch, meter,
        )?),
        CompiledScoreExpr::Div(left, right) => {
            let numer = eval_score_expr(
                db, rows, left, candidate, row, encoded, scratch, meter,
            )?;
            let denom = eval_score_expr(
                db, rows, right, candidate, row, encoded, scratch, meter,
            )?;
            if denom == 0.0 {
                Ok(f64::NAN)
            } else {
                Ok(numer / denom)
            }
        }
        CompiledScoreExpr::Neg(inner) => Ok(-eval_score_expr(
            db, rows, inner, candidate, row, encoded, scratch, meter,
        )?),
    }
}
