use super::*;

impl Compiler<'_> {
    // ── §4.1 / §4.2 range rewrites (QL_CONTRACT §4.1, §4.2) ──────────────

    /// The instant `now()` folds to for one statement.
    ///
    /// Read ONCE per compiled statement, so every row of one answer sees the
    /// same clock and a page that resumes does not drift. `docs/lang/QL_CONTRACT.md`
    /// §4.2: "constants folded at prepare".
    pub(super) fn clock_micros(&self) -> i64 {
        // The clock is FOLDED, not a slot: one instant per compiled
        // statement, so every row of one answer sees the same `now()` and a
        // page that resumes does not drift. A rebind must therefore take a
        // NEW clock, which is a new compile -- so a statement that reads the
        // clock is not rebindable, and says so.
        self.folds_reason(
            "now()/current_date is folded at prepare, and a rebind takes a NEW clock".to_owned(),
        );
        self.clock
    }

    /// A [`TimeValue`] folded to stored microseconds.
    fn time_value(&self, value: &TimeValue, column: &str) -> SqlResult2<i64> {
        Ok(match value {
            TimeValue::Lit(literal) => self.time_literal(literal, column)?,
            TimeValue::Clock { date_only, offset } => {
                let base = self.clock_micros();
                let base = if *date_only {
                    functions::date_trunc(TimeUnit::Day, base)?
                } else {
                    base
                };
                base.checked_add(*offset).ok_or_else(|| {
                    SqlError::unsupported("the folded clock arithmetic overflows i64 microseconds")
                })?
            }
        })
    }

    /// A written literal read as stored microseconds: a string is an ISO-8601
    /// or Postgres date/time literal, a whole number is already the stored
    /// integer.
    fn time_literal(&self, literal: &Literal, column: &str) -> SqlResult2<i64> {
        match self.value_of(literal)? {
            Value::String(text) => functions::parse_timestamp(&text),
            Value::Number(n) => n.as_i64().ok_or_else(|| {
                SqlError::Parameter(format!(
                    "`{column}` stores whole microseconds and {n} is not a whole number"
                ))
            }),
            other => Err(SqlError::Parameter(format!(
                "`{column}` is a declared timestamp and {other} is neither a date/time literal nor a whole number of microseconds"
            ))),
        }
    }

    /// A whole number written beside an `EXTRACT`.
    fn whole(&self, literal: &Literal, what: &str) -> SqlResult2<i64> {
        match self.value_of(literal)? {
            Value::Number(n) => n.as_i64().ok_or_else(|| {
                SqlError::Parameter(format!("{what} compares against a whole number, not {n}"))
            }),
            other => Err(SqlError::Parameter(format!(
                "{what} compares against a whole number, not {other}"
            ))),
        }
    }

    /// One half-open range `[lower, upper)` over the stored microseconds, as
    /// the scalar filter spells it.
    fn micro_range(lower: Option<i64>, upper: Option<i64>) -> OwnedScalarFilter {
        OwnedScalarFilter::Range {
            lower: lower.map_or(Bound::Unbounded, |v| Bound::Included(Scalar::I64(v))),
            upper: upper.map_or(Bound::Unbounded, |v| Bound::Excluded(Scalar::I64(v))),
        }
    }

    /// The empty range: a predicate whose pre-image holds no instant at all
    /// (`date_trunc('year', t) = '1950-06-01'`). It is still ONE range, so it
    /// is answered by the index with no candidates walked rather than
    /// refused: the statement is well formed and its answer is no rows.
    fn empty_range() -> OwnedScalarFilter {
        OwnedScalarFilter::Range {
            lower: Bound::Excluded(Scalar::I64(i64::MAX)),
            upper: Bound::Excluded(Scalar::I64(i64::MAX)),
        }
    }

    /// The bounds a comparison against one folded instant produces.
    fn compare_range(op: CmpOp, at: i64, what: &str) -> SqlResult2<OwnedScalarFilter> {
        Ok(match op {
            CmpOp::Eq => Self::micro_range(Some(at), at.checked_add(1)),
            CmpOp::Lt => Self::micro_range(None, Some(at)),
            CmpOp::Le => Self::micro_range(None, at.checked_add(1)),
            CmpOp::Gt => OwnedScalarFilter::Range {
                lower: Bound::Excluded(Scalar::I64(at)),
                upper: Bound::Unbounded,
            },
            CmpOp::Ge => Self::micro_range(Some(at), None),
            CmpOp::Ne => return Err(refuse::multi_range(what)),
        })
    }

    /// A `[start, end)` window compared against: `= window` is the window,
    /// `< window` is everything below its start, and so on. This is what
    /// makes `EXTRACT(YEAR FROM t) = 1950` and `date_trunc('year', t) = lit`
    /// ONE range each.
    fn window_range(op: CmpOp, start: i64, end: i64, what: &str) -> SqlResult2<OwnedScalarFilter> {
        Ok(match op {
            CmpOp::Eq => Self::micro_range(Some(start), Some(end)),
            CmpOp::Lt => Self::micro_range(None, Some(start)),
            CmpOp::Le => Self::micro_range(None, Some(end)),
            CmpOp::Gt => Self::micro_range(Some(end), None),
            CmpOp::Ge => Self::micro_range(Some(start), None),
            // The complement of a window is TWO ranges: below it and above
            // it. That is a union.
            CmpOp::Ne => return Err(refuse::multi_range(what)),
        })
    }

    /// The same comparison when the literal is NOT on the unit's boundary --
    /// `date_trunc('month', t) >= '1950-01-15'`, `t::date < '1950-01-15 12:00'`.
    ///
    /// The left-hand side only ever takes boundary values, so an interior
    /// literal moves every cut to the boundary ABOVE it, which is the window's
    /// own `end`: `>= lit` and `> lit` are both `t >= end` (January is
    /// excluded, because `1950-01-01 >= 1950-01-15` is false), `< lit` and
    /// `<= lit` are both `t < end` (January is kept, because
    /// `1950-01-01 < 1950-01-15` is true), and `= lit` holds for no instant.
    /// Postgres answers each of these the same way. `<>` stays refused with
    /// the window reason rather than becoming a second spelling of "every
    /// row": one shape, one refusal.
    fn offset_window_range(op: CmpOp, end: i64, what: &str) -> SqlResult2<OwnedScalarFilter> {
        Ok(match op {
            CmpOp::Eq => Self::empty_range(),
            CmpOp::Lt | CmpOp::Le => Self::micro_range(None, Some(end)),
            CmpOp::Gt | CmpOp::Ge => Self::micro_range(Some(end), None),
            CmpOp::Ne => return Err(refuse::multi_range(what)),
        })
    }

    /// 1 January of `year`, in stored microseconds, saturating at the ends of
    /// the representable range.
    ///
    /// `EXTRACT(YEAR FROM t) = 300000` is a legal literal a user can write,
    /// and `days_from_civil(n, 1, 1) * MICROS_PER_DAY` leaves i64 somewhere
    /// past year 294,000. The release profile sets no `overflow-checks`
    /// (`Cargo.toml`), so the unchecked form wraps to a garbage window in
    /// release and panics in a test build, on a literal.
    ///
    /// Saturating is not a clamp of the ANSWER. Every instant a column can
    /// hold lies inside `[i64::MIN, i64::MAX]` microseconds, so a year above
    /// the range makes `= n` and `>= n` empty and `< n` everything, and a
    /// year below it makes `<= n` empty and `> n` everything -- which is
    /// what those comparisons mean. The year is bounded first, because
    /// `days_from_civil` multiplies the year itself.
    fn year_start(year: i64) -> i64 {
        const BOUND: i64 = 400_000;
        if year > BOUND {
            return i64::MAX;
        }
        if year < -BOUND {
            return i64::MIN;
        }
        match functions::days_from_civil(year, 1, 1).checked_mul(functions::MICROS_PER_DAY) {
            Some(micros) => micros,
            None if year > 0 => i64::MAX,
            None => i64::MIN,
        }
    }

    /// The `[start, end)` window `EXTRACT(<unit> FROM t) = n` names.
    ///
    /// `YEAR` is the one unit whose equality is contiguous over the stored
    /// integer: every instant of year `n` lies between 1 January `n` and
    /// 1 January `n + 1`, and nothing else does. `MONTH`, `DAY`, `DOW`,
    /// `HOUR`, `MINUTE` and `SECOND` repeat, so their pre-image is one
    /// interval PER period in the corpus -- a set of ranges, which is the
    /// membership-set union `OR` compiles to.
    fn extract_window(unit: TimeUnit, n: i64, what: &str) -> SqlResult2<(i64, i64)> {
        match unit {
            TimeUnit::Year => Ok((
                Self::year_start(n),
                Self::year_start(n.saturating_add(1)),
            )),
            TimeUnit::Epoch => Ok((
                n.saturating_mul(functions::MICROS_PER_SECOND),
                n.saturating_add(1).saturating_mul(functions::MICROS_PER_SECOND),
            )),
            _ => Err(refuse::multi_range(what)),
        }
    }

    /// A `Predicate::Time` folded into ONE scalar range on `column`'s index.
    pub(super) fn time_filter(
        &mut self,
        c: CollectionId,
        column: &str,
        shape: &TimeShape,
    ) -> SqlResult2<OwnedFilter> {
        let what = shape.written(column);
        if self.time_column(c, column)?.is_none() {
            return Err(SqlError::unsupported(format!(
                "{what}: `{column}` is not a declared TIMESTAMPTZ or DATE. QL_CONTRACT §4.2 folds a date/time function over a column whose declared type says it holds UTC microseconds; over an untyped Int there is nothing to fold"
            )));
        }
        let index = self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?;
        let predicate = match shape {
            TimeShape::Extract { unit, op, value } => {
                let n = self.whole(value, &what)?;
                let (start, end) = Self::extract_window(*unit, n, &what)?;
                Self::window_range(*op, start, end, &what)?
            }
            TimeShape::ExtractBetween { unit, lower, upper } => {
                let low = self.whole(lower, &what)?;
                let high = self.whole(upper, &what)?;
                if high < low {
                    Self::empty_range()
                } else {
                    let (start, _) = Self::extract_window(*unit, low, &what)?;
                    let (_, end) = Self::extract_window(*unit, high, &what)?;
                    Self::micro_range(Some(start), Some(end))
                }
            }
            TimeShape::Trunc { unit, op, value } => {
                let at = self.time_literal(value, column)?;
                let start = functions::date_trunc(*unit, at)?;
                let end = functions::next_unit(*unit, start)?;
                // An off-boundary literal is its own comparison: `= v` holds
                // for no instant at all, and both inequalities cut at `end`
                // rather than at `start`. Postgres answers the same.
                if start == at {
                    Self::window_range(*op, start, end, &what)?
                } else {
                    Self::offset_window_range(*op, end, &what)?
                }
            }
            TimeShape::TruncBetween { unit, lower, upper } => {
                let low_at = self.time_literal(lower, column)?;
                let high_at = self.time_literal(upper, column)?;
                let low = functions::date_trunc(*unit, low_at)?;
                let high = functions::date_trunc(*unit, high_at)?;
                // BETWEEN is `>= lower AND <= upper`. The lower half carries
                // the off-boundary rule above: no truncated instant lies
                // between `low` and an interior `low_at`, so the window starts
                // at the next boundary. The upper half does not: `<= high_at`
                // and `<= high` admit the same truncated instants either way.
                let start = if low == low_at {
                    low
                } else {
                    functions::next_unit(*unit, low)?
                };
                let end = functions::next_unit(*unit, high)?;
                if end <= start {
                    Self::empty_range()
                } else {
                    Self::micro_range(Some(start), Some(end))
                }
            }
            TimeShape::CastDate { op, value } => {
                let at = self.time_literal(value, column)?;
                let start = functions::date_trunc(TimeUnit::Day, at)?;
                let end = functions::next_unit(TimeUnit::Day, start)?;
                // `t::date` is a truncation to the day, so it takes the same
                // off-boundary rule as `date_trunc('day', t)`.
                if start == at {
                    Self::window_range(*op, start, end, &what)?
                } else {
                    Self::offset_window_range(*op, end, &what)?
                }
            }
            TimeShape::Clock { op, value } => {
                let at = self.time_value(value, column)?;
                Self::compare_range(*op, at, &what)?
            }
            TimeShape::ClockBetween { lower, upper } => {
                let low = self.time_value(lower, column)?;
                let high = self.time_value(upper, column)?;
                if high < low {
                    Self::empty_range()
                } else {
                    Self::micro_range(Some(low), high.checked_add(1))
                }
            }
        };
        self.rewrites.push(format!(
            "{what} -> scalar range on `{column}` (index-side; the function is folded at prepare and never evaluated per candidate)"
        ));
        Ok(OwnedFilter::Scalar {
            index,
            predicate,
            // A §4.1 / §4.2 rewrite folds its pre-image into an index RANGE,
            // so the value is no longer in the plan to refill.
            fills: Vec::new(),
        })
    }

    /// A `Predicate::TextFn` folded into ONE text-key range.
    pub(super) fn text_filter(
        &mut self,
        c: CollectionId,
        column: &str,
        shape: &TextShape,
    ) -> SqlResult2<OwnedFilter> {
        let what = shape.written(column);
        // `col->>'m' = v` is the ONE shape whose column is not TEXT: it reads
        // a JSONB column and the index holds the member's text. It is
        // answered from the expression index over that same member and from
        // nothing else -- a different member, or no index, is a refusal that
        // names the index it would need, never a scan (QL_CONTRACT §6).
        if let TextShape::JsonEq { member, value } = shape {
            if !matches!(self.kind_of(c, column)?, Kind::Json) {
                return Err(SqlError::unsupported(format!(
                    "{what}: `->>` extracts from a JSONB column, and `{column}` is not one"
                )));
            }
            let index = self.index_for_expression(
                c,
                column,
                IndexFamily::Scalar,
                Some(IndexExpr::JsonText(member.clone())),
                &format!(
                    "an expression index `CREATE INDEX ... ON t (({column}->>'{member}'))`"
                ),
            )?;
            self.rewrites.push(format!(
                "{what} -> scalar equality on the expression index over `{column}->>'{member}'` (index-side)"
            ));
            return Ok(OwnedFilter::Scalar {
                index,
                predicate: OwnedScalarFilter::Eq(Scalar::Text(self.text_of(value)?)),
                fills: Vec::new(),
            });
        }
        if !matches!(self.kind_of(c, column)?, Kind::Text) {
            return Err(SqlError::unsupported(format!(
                "{what}: `{column}` is not a TEXT column, and a text-key range is over text keys"
            )));
        }
        let lowered = matches!(shape, TextShape::LowerEq { .. } | TextShape::LowerPrefix { .. });
        let index = if lowered {
            self.index_for_expression(
                c,
                column,
                IndexFamily::Scalar,
                Some(IndexExpr::Lower),
                "an expression index `CREATE INDEX ... ON t (lower(col))`",
            )?
        } else {
            self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?
        };
        // The bound is folded the way the INDEX stores it: an expression
        // index over lower(col) holds folded keys, so the literal is folded
        // to match. Without that the range would be over a different
        // alphabet than the keys it walks.
        let literal = |compiler: &Self, value: &Literal| -> SqlResult2<String> {
            let text = compiler.text_of(value)?;
            Ok(if lowered { text.to_lowercase() } else { text })
        };
        let predicate = match shape {
            // Answered above: it is the one shape whose column is JSONB.
            TextShape::JsonEq { .. } => unreachable!("JsonEq returns above"),
            TextShape::LowerEq { value } => OwnedScalarFilter::Eq(Scalar::Text(literal(self, value)?)),
            TextShape::LowerPrefix { value, .. } | TextShape::Prefix { value, .. } => {
                let raw = literal(self, value)?;
                let prefix = match shape {
                    TextShape::Prefix { written: "LIKE", .. }
                    | TextShape::LowerPrefix { written: "LIKE", .. } => {
                        functions::like_prefix(&raw)
                            .ok_or_else(|| SqlError::Refused {
                                keyword: "LIKE".into(),
                                tier: Tier::Two,
                                reason: super::parser::LIKE_NOT_A_PREFIX,
                            })?
                            .to_owned()
                    }
                    _ => raw,
                };
                if prefix.is_empty() {
                    return Err(SqlError::unsupported(format!(
                        "{what}: an empty prefix admits every row, which is a scan, and §6 does not allow one to be taken silently"
                    )));
                }
                if prefix.contains('\0') {
                    return Err(SqlError::unsupported(format!(
                        "{what}: a NUL inside a prefix has no successor in the escaped text key encoding (`src/store/scalar_key.rs`)"
                    )));
                }
                let upper = match functions::prefix_successor(&prefix) {
                    functions::PrefixSuccessor::Bound(next) => {
                        Bound::Excluded(Scalar::Text(next))
                    }
                    functions::PrefixSuccessor::Unbounded => Bound::Unbounded,
                    // The bound travels as text and this one is not text.
                    // Widening it to the replacement character would admit
                    // every value in between, and the residual filter uses
                    // the same bound, so nothing downstream would catch it.
                    functions::PrefixSuccessor::NotUtf8 => {
                        return Err(SqlError::unsupported(format!(
                            "{what}: this prefix's upper bound is a byte string that is not valid UTF-8 (incrementing the last byte of `{prefix}` leaves one), and a text range bound is text. QL_CONTRACT §3: there is no byte-valued text bound in this slice, so the prefix is refused rather than answered from a wider range than the one asked for"
                        )))
                    }
                };
                OwnedScalarFilter::Range {
                    lower: Bound::Included(Scalar::Text(prefix.clone())),
                    upper,
                }
            }
        };
        self.rewrites.push(format!(
            "{what} -> {} on `{column}`{} (index-side)",
            match predicate {
                OwnedScalarFilter::Eq(_) => "scalar equality",
                _ => "text-key prefix range",
            },
            if lowered {
                " through the expression index over lower(col)"
            } else {
                ""
            }
        ));
        Ok(OwnedFilter::Scalar {
            index,
            predicate,
            // A §4.1 / §4.2 rewrite folds its pre-image into an index RANGE,
            // so the value is no longer in the plan to refill.
            fills: Vec::new(),
        })
    }

}
