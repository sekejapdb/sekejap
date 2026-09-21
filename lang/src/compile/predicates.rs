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
                    && matches!(self.value_of(value)?, Value::String(_))
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
                let scalar = self.scalar(&kind, value, column)?;
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
                OwnedFilter::Scalar { index, predicate }
            }
            Predicate::Between {
                column,
                lower,
                upper,
            } => {
                if self.time_column(c, column)?.is_some()
                    && matches!(self.value_of(lower)?, Value::String(_))
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
                OwnedFilter::Scalar {
                    index,
                    predicate: OwnedScalarFilter::Range {
                        lower: Bound::Included(self.scalar(&kind, lower, column)?),
                        upper: Bound::Included(self.scalar(&kind, upper, column)?),
                    },
                }
            }
            Predicate::IsNull { column, negated } => {
                let index = self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?;
                let leaf = OwnedFilter::Scalar {
                    index,
                    predicate: OwnedScalarFilter::IsNull,
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
                }
            }
            Predicate::KeyCompare { op, value } => {
                let key = self.text_of(value)?;
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
                        })))
                    }
                };
                OwnedFilter::Key { lower, upper }
            }
            Predicate::KeyBetween { lower, upper } => OwnedFilter::Key {
                lower: Bound::Included(self.text_of(lower)?),
                upper: Bound::Included(self.text_of(upper)?),
            },
            // `col IN (v1, v2, ...)`: one equality per value, unioned into
            // one membership set. `docs/lang/QL_CONTRACT.md` §3.
            Predicate::InList { column, values } => {
                let kind = self.kind_of(c, column)?;
                let index = self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?;
                let mut leaves = Vec::with_capacity(values.len());
                for value in values {
                    let scalar = self.scalar(&kind, value, column)?;
                    leaves.push(OwnedFilter::Scalar {
                        index,
                        predicate: OwnedScalarFilter::Eq(scalar),
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
                    let key = self.text_of(value)?;
                    leaves.push(OwnedFilter::Key {
                        lower: Bound::Included(key.clone()),
                        upper: Bound::Included(key),
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
                let stripped = TsQuery {
                    source: Literal::Str(
                        self.text_of(&query.source)?
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
                let (query, matching) = self.tsquery(query)?;
                OwnedFilter::Text {
                    index,
                    query,
                    matching,
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

    fn scalar(&self, kind: &Kind, literal: &Literal, column: &str) -> SqlResult2<Scalar> {
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
    pub(super) fn tsquery(&self, query: &TsQuery) -> SqlResult2<(String, TextMatch)> {
        let text = self.text_of(&query.source)?;
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
                        let center = self.point_of(point)?;
                        let radius_metres = self.f64_of(metres.ok_or_else(|| {
                            SqlError::syntax("ST_DWithin needs a distance", 0)
                        })?)?;
                        Ok(OwnedFilter::Point {
                            index,
                            predicate: PointFilter::Radius {
                                center,
                                radius_metres,
                            },
                        })
                    }
                    SpatialPredicate::Within => Ok(OwnedFilter::Point {
                        index,
                        predicate: PointFilter::Bbox(self.bounds_of(argument)?),
                    }),
                    other => Err(SqlError::unsupported(format!(
                        "{other:?} on a Point column: the point atomics are PointFilter::Bbox (ST_Within against an envelope) and PointFilter::Radius (ST_DWithin)"
                    ))),
                }
            }
            Kind::Geo => {
                let index =
                    self.index_for(c, column, IndexFamily::SpatialGeometry, "a geometry index")?;
                let geometry = self.geom_of(argument)?;
                let predicate = match predicate {
                    SpatialPredicate::DWithin => GeometryFilter::DWithin {
                        geometry,
                        metres: self.f64_of(metres.ok_or_else(|| {
                            SqlError::syntax("ST_DWithin needs a distance", 0)
                        })?)?,
                    },
                    SpatialPredicate::Intersects => GeometryFilter::Intersects(geometry),
                    SpatialPredicate::Within => GeometryFilter::Within(geometry),
                    SpatialPredicate::Contains => GeometryFilter::Contains(geometry),
                };
                Ok(OwnedFilter::Geometry { index, predicate })
            }
            other => Err(SqlError::unsupported(format!(
                "`{column}` is declared {other:?}; a spatial predicate needs a Point or a Geo column"
            ))),
        }
    }

}
