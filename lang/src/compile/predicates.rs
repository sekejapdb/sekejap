use super::*;

impl Compiler<'_> {
    pub(super) fn filter(&mut self, c: CollectionId, predicate: &Predicate) -> SqlResult2<OwnedFilter> {
        Ok(match predicate {
            Predicate::Compare { column, op, value } => {
                // `t >= '1950-01-01'` over a declared TIMESTAMPTZ/DATE is a
                // §4.2 rewrite: the literal is read to stored microseconds
                // and the predicate is the ordinary scalar Range. Without the
                // declared type there is no literal grammar to read it with.
                if self.time_column(c, column)?.is_some()
                    && matches!(self.peek_value(value)?, Value::String(_))
                {
                    return self.time_filter(
                        c,
                        column,
                        &TimeShape::Clock {
                            op: *op,
                            value: TimeValue::Lit(value.clone()),
                        },
                    );
                }
                let kind = self.kind_of(c, column)?;
                let index = self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?;
                // Which half of the compiled predicate the written value
                // fills, so a rebind writes the new value into the same
                // position without re-deciding the operator.
                let at = match op {
                    CmpOp::Eq | CmpOp::Ne => ScalarAt::Eq,
                    CmpOp::Lt | CmpOp::Le => ScalarAt::Upper,
                    CmpOp::Gt | CmpOp::Ge => ScalarAt::Lower,
                };
                let (scalar, fill) = self.scalar_slot(&kind, value, column, at)?;
                let fills: Vec<ScalarFill> = fill.into_iter().collect();
                let predicate = match op {
                    CmpOp::Eq => OwnedScalarFilter::Eq(scalar),
                    // `<>` is the complement of an equality, which the
                    // engine takes inside the index itself: a union of the
                    // postings below the value and the postings above it,
                    // the nullish key in neither. See `QueryFilter::Not`.
                    CmpOp::Ne => {
                        return Ok(OwnedFilter::Not(Box::new(OwnedFilter::Scalar {
                            index,
                            predicate: OwnedScalarFilter::Eq(scalar),
                            fills,
                        })))
                    }
                    CmpOp::Lt => OwnedScalarFilter::Range {
                        lower: Bound::Unbounded,
                        upper: Bound::Excluded(scalar),
                    },
                    CmpOp::Le => OwnedScalarFilter::Range {
                        lower: Bound::Unbounded,
                        upper: Bound::Included(scalar),
                    },
                    CmpOp::Gt => OwnedScalarFilter::Range {
                        lower: Bound::Excluded(scalar),
                        upper: Bound::Unbounded,
                    },
                    CmpOp::Ge => OwnedScalarFilter::Range {
                        lower: Bound::Included(scalar),
                        upper: Bound::Unbounded,
                    },
                };
                OwnedFilter::Scalar {
                    index,
                    predicate,
                    fills,
                }
            }
            Predicate::Between {
                column,
                lower,
                upper,
            } => {
                if self.time_column(c, column)?.is_some()
                    && matches!(self.peek_value(lower)?, Value::String(_))
                {
                    return self.time_filter(
                        c,
                        column,
                        &TimeShape::ClockBetween {
                            lower: TimeValue::Lit(lower.clone()),
                            upper: TimeValue::Lit(upper.clone()),
                        },
                    );
                }
                let kind = self.kind_of(c, column)?;
                let index = self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?;
                let (low, low_fill) = self.scalar_slot(&kind, lower, column, ScalarAt::Lower)?;
                let (high, high_fill) = self.scalar_slot(&kind, upper, column, ScalarAt::Upper)?;
                OwnedFilter::Scalar {
                    index,
                    predicate: OwnedScalarFilter::Range {
                        lower: Bound::Included(low),
                        upper: Bound::Included(high),
                    },
                    fills: low_fill.into_iter().chain(high_fill).collect(),
                }
            }
            Predicate::IsNull { column, negated } => {
                let index = self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?;
                let leaf = OwnedFilter::Scalar {
                    index,
                    predicate: OwnedScalarFilter::IsNull,
                    fills: Vec::new(),
                };
                // `IS NOT NULL` is the complement of the nullish key inside
                // the index, which is every other posting: one range, no
                // bitmap and no universe walk. See `QueryFilter::Not`.
                if *negated {
                    OwnedFilter::Not(Box::new(leaf))
                } else {
                    leaf
                }
            }
            Predicate::IsMissing { column } => {
                let index = self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?;
                OwnedFilter::Scalar {
                    index,
                    predicate: OwnedScalarFilter::IsMissing,
                    fills: Vec::new(),
                }
            }
            Predicate::KeyCompare { op, value } => {
                let (key, at) = match op {
                    CmpOp::Lt | CmpOp::Le => (self.key_slot(value, KeyAt::Upper)?, KeyAt::Upper),
                    CmpOp::Gt | CmpOp::Ge => (self.key_slot(value, KeyAt::Lower)?, KeyAt::Lower),
                    CmpOp::Eq | CmpOp::Ne => (self.key_slot(value, KeyAt::Lower)?, KeyAt::Lower),
                };
                // A one-key range fills BOTH ends from the same slot.
                let fills: Vec<KeyFill> = match (&key.1, at) {
                    (None, _) => Vec::new(),
                    (Some(fill), KeyAt::Lower) if matches!(op, CmpOp::Eq | CmpOp::Ne) => vec![
                        fill.clone(),
                        KeyFill {
                            at: KeyAt::Upper,
                            literal: fill.literal.clone(),
                        },
                    ],
                    (Some(fill), _) => vec![fill.clone()],
                };
                let key = key.0;
                let (lower, upper) = match op {
                    CmpOp::Eq => (Bound::Included(key.clone()), Bound::Included(key)),
                    CmpOp::Lt => (Bound::Unbounded, Bound::Excluded(key)),
                    CmpOp::Le => (Bound::Unbounded, Bound::Included(key)),
                    CmpOp::Gt => (Bound::Excluded(key), Bound::Unbounded),
                    CmpOp::Ge => (Bound::Included(key), Bound::Unbounded),
                    // The complement of a one-key range, which the engine
                    // takes as the two mapping ranges either side of it.
                    CmpOp::Ne => {
                        return Ok(OwnedFilter::Not(Box::new(OwnedFilter::Key {
                            lower: Bound::Included(key.clone()),
                            upper: Bound::Included(key),
                            fills,
                        })))
                    }
                };
                OwnedFilter::Key {
                    lower,
                    upper,
                    fills,
                }
            }
            Predicate::KeyBetween { lower, upper } => {
                let (low, low_fill) = self.key_slot(lower, KeyAt::Lower)?;
                let (high, high_fill) = self.key_slot(upper, KeyAt::Upper)?;
                OwnedFilter::Key {
                    lower: Bound::Included(low),
                    upper: Bound::Included(high),
                    fills: low_fill.into_iter().chain(high_fill).collect(),
                }
            }
            // `col IN (v1, v2, ...)`: one equality per value, unioned into
            // one membership set. `docs/lang/QL_CONTRACT.md` §3.
            Predicate::InList { column, values } => {
                let kind = self.kind_of(c, column)?;
                let index = self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?;
                let mut leaves = Vec::with_capacity(values.len());
                for value in values {
                    let (scalar, fill) = self.scalar_slot(&kind, value, column, ScalarAt::Eq)?;
                    leaves.push(OwnedFilter::Scalar {
                        index,
                        predicate: OwnedScalarFilter::Eq(scalar),
                        fills: fill.into_iter().collect(),
                    });
                }
                if leaves.len() == 1 {
                    leaves.pop().ok_or_else(|| {
                        SqlError::syntax("IN needs at least one value", 0)
                    })?
                } else {
                    OwnedFilter::Any(leaves)
                }
            }
            Predicate::KeyInList { values } => {
                let mut leaves = Vec::with_capacity(values.len());
                for value in values {
                    let (key, fill) = self.key_slot(value, KeyAt::Lower)?;
                    let fills: Vec<KeyFill> = match fill {
                        None => Vec::new(),
                        Some(fill) => vec![
                            KeyFill {
                                at: KeyAt::Upper,
                                literal: fill.literal.clone(),
                            },
                            fill,
                        ],
                    };
                    leaves.push(OwnedFilter::Key {
                        lower: Bound::Included(key.clone()),
                        upper: Bound::Included(key),
                        fills,
                    });
                }
                if leaves.len() == 1 {
                    leaves.pop().ok_or_else(|| {
                        SqlError::syntax("IN needs at least one value", 0)
                    })?
                } else {
                    OwnedFilter::Any(leaves)
                }
            }
            Predicate::Semi { table, column } => OwnedFilter::Ids(self.semi_join(c, table, column)?),
            // `to_tsquery('simple','!comet')`: the complement of the text
            // set. Written alone, because a `!` inside a larger tsquery is a
            // boolean tree over one index's postings rather than one leaf.
            Predicate::Text { column, query } if self.negated_tsquery(query)? => {
                // Whether this predicate is a COMPLEMENT is decided by the
                // value, so a `$n` here decides the plan's shape and cannot
                // be a slot.
                self.folds(
                    &query.source,
                    "a `!term` tsquery, whose complement shape is decided by the value",
                );
                let stripped = TsQuery {
                    source: Literal::Str(
                        self.binder()
                            .text_of(&query.source)?
                            .trim()
                            .trim_start_matches('!')
                            .trim()
                            .to_owned(),
                    ),
                    tsquery_syntax: query.tsquery_syntax,
                };
                OwnedFilter::Not(Box::new(self.filter(
                    c,
                    &Predicate::Text {
                        column: column.clone(),
                        query: stripped,
                    },
                )?))
            }
            Predicate::Text { column, query } => {
                let index = self.index_for(c, column, IndexFamily::Text, "a text index")?;
                let (text, matching, fill) = self.tsquery_slot(query)?;
                OwnedFilter::Text {
                    index,
                    query: text,
                    matching,
                    fill,
                }
            }
            Predicate::Spatial {
                predicate,
                column,
                argument,
                metres,
            } => self.spatial(c, *predicate, column, argument, metres.as_ref())?,
            Predicate::Time { column, shape } => self.time_filter(c, column, shape)?,
            Predicate::TextFn { column, shape } => self.text_filter(c, column, shape)?,
        })
    }

    /// One scalar value AND the slot it came from. The slot is recorded only
    /// when the written literal is a `$n`: a constant of the text cannot
    /// change, so it is not a slot and costs nothing to rebind.
    pub(super) fn scalar_slot(
        &self,
        kind: &Kind,
        literal: &Literal,
        column: &str,
        at: ScalarAt,
    ) -> SqlResult2<(Scalar, Option<ScalarFill>)> {
        let value = self.binder().scalar(kind, literal, column)?;
        let fill = literal_is_bound(literal).then(|| ScalarFill {
            at,
            literal: literal.clone(),
            kind: kind.clone(),
            column: column.to_owned(),
        });
        Ok((value, fill))
    }

    /// One external key AND its slot.
    pub(super) fn key_slot(&self, literal: &Literal, at: KeyAt) -> SqlResult2<(String, Option<KeyFill>)> {
        let key = self.binder().text_of(literal)?;
        let fill = literal_is_bound(literal).then(|| KeyFill {
            at,
            literal: literal.clone(),
        });
        Ok((key, fill))
    }

    /// One tsquery AND its slot. Both halves of the answer -- the terms and
    /// the `TextMatch` -- are re-derived on a rebind, because `a & b` and
    /// `a | b` arrive through the same slot.
    pub(super) fn tsquery_slot(
        &self,
        query: &TsQuery,
    ) -> SqlResult2<(String, TextMatch, Option<TsQuery>)> {
        let (text, matching) = self.binder().tsquery(query)?;
        let fill = literal_is_bound(&query.source).then(|| query.clone());
        Ok((text, matching, fill))
    }

    fn spatial(
        &mut self,
        c: CollectionId,
        predicate: SpatialPredicate,
        column: &str,
        argument: &GeoArg,
        metres: Option<&Literal>,
    ) -> SqlResult2<OwnedFilter> {
        let kind = self.kind_of(c, column)?;
        match kind {
            Kind::Point => {
                let index = self.index_for(c, column, IndexFamily::SpatialPoint, "a point index")?;
                match predicate {
                    SpatialPredicate::DWithin => {
                        let GeoArg::Point(point) = argument else {
                            return Err(SqlError::unsupported(
                                "ST_DWithin on a Point column takes a point: PointFilter::Radius is a centre and a radius",
                            ));
                        };
                        let metres = metres.ok_or_else(|| {
                            SqlError::syntax("ST_DWithin needs a distance", 0)
                        })?;
                        let center = self.binder().point_of(point)?;
                        let radius_metres = self.binder().f64_of(metres)?;
                        let fill = (point_is_bound(point) || literal_is_bound(metres)).then(|| {
                            PointFill::Radius {
                                center: point.clone(),
                                metres: metres.clone(),
                            }
                        });
                        Ok(OwnedFilter::Point {
                            index,
                            predicate: PointFilter::Radius {
                                center,
                                radius_metres,
                            },
                            fill,
                        })
                    }
                    SpatialPredicate::Within => Ok(OwnedFilter::Point {
                        index,
                        predicate: PointFilter::Bbox(self.binder().bounds_of(argument)?),
                        fill: geo_is_bound(argument)
                            .then(|| PointFill::Bbox(argument.clone())),
                    }),
                    other => Err(SqlError::unsupported(format!(
                        "{other:?} on a Point column: the point atomics are PointFilter::Bbox (ST_Within against an envelope) and PointFilter::Radius (ST_DWithin)"
                    ))),
                }
            }
            Kind::Geo => {
                let index =
                    self.index_for(c, column, IndexFamily::SpatialGeometry, "a geometry index")?;
                let geometry = self.binder().geom_of(argument)?;
                let mut bound = geo_is_bound(argument);
                let compiled = match predicate {
                    SpatialPredicate::DWithin => {
                        let metres = metres.ok_or_else(|| {
                            SqlError::syntax("ST_DWithin needs a distance", 0)
                        })?;
                        bound = bound || literal_is_bound(metres);
                        GeometryFilter::DWithin {
                            geometry,
                            metres: self.binder().f64_of(metres)?,
                        }
                    }
                    SpatialPredicate::Intersects => GeometryFilter::Intersects(geometry),
                    SpatialPredicate::Within => GeometryFilter::Within(geometry),
                    SpatialPredicate::Contains => GeometryFilter::Contains(geometry),
                };
                let fill = bound.then(|| GeomFill {
                    predicate,
                    argument: argument.clone(),
                    metres: metres.cloned(),
                });
                Ok(OwnedFilter::Geometry {
                    index,
                    predicate: compiled,
                    fill,
                })
            }
            other => Err(SqlError::unsupported(format!(
                "`{column}` is declared {other:?}; a spatial predicate needs a Point or a Geo column"
            ))),
        }
    }

}
