use super::*;

// ── the compiled aggregate ────────────────────────────────────────────────

/// Where one column of an aggregate's answer comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AggOutput {
    /// The group key itself.
    Key,
    /// One accumulator, by its position in the request.
    Value(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum OwnedAggInput {
    Index(IndexId),
    Field(String),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OwnedAccumulator {
    pub(crate) function: AggregateFn,
    pub(crate) input: Option<OwnedAggInput>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum OwnedGroupKey {
    Index(IndexId),
    Field(String),
    IndexDiv { index: IndexId, divisor: i64 },
}

/// A compiled `GROUP BY` / `DISTINCT` / aggregate statement. It owns what the
/// borrowed [`AggregateRequest`] points at, exactly as [`SelectPlan`] owns
/// what a `QueryRequest` points at.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AggregatePlan {
    pub(crate) collection: CollectionId,
    pub(crate) columns: Vec<String>,
    pub(crate) outputs: Vec<AggOutput>,
    pub(crate) filters: Vec<OwnedFilter>,
    pub(crate) group: Option<OwnedGroupKey>,
    pub(crate) accumulators: Vec<OwnedAccumulator>,
    pub(crate) having: Vec<GroupPredicate>,
    pub(crate) order: GroupOrder,
    pub(crate) driver: CandidateDriver,
    pub(crate) limit: Option<usize>,
    /// `Some(date_only)` when the group key is a declared TIMESTAMPTZ/DATE
    /// column. See [`group_key_value`].
    pub(crate) key_iso: Option<bool>,
    pub(crate) text: String,
}

/// An aggregate row is not a row of the collection: no entity produced it, so
/// there is no id to report. `SqlRow` carries one, so a group reports
/// sequence 0, which `Database::put` never issues (sequences are one-based).
fn group_identity(collection: CollectionId) -> EntityId {
    EntityId {
        collection,
        sequence: 0,
    }
}

fn agg_value(value: &AggValue) -> SqlValue {
    match value {
        AggValue::Count(n) => SqlValue::Int(*n as i64),
        AggValue::I64(v) => SqlValue::Int(*v),
        AggValue::F64(v) => SqlValue::Float(*v),
        AggValue::Text(v) => SqlValue::Text(v.clone()),
        AggValue::Bool(v) => SqlValue::Bool(*v),
        AggValue::Null => SqlValue::Null,
    }
}

/// The group key as the answer reports it.
///
/// `iso` is `Some(date_only)` when the key is a declared TIMESTAMPTZ or DATE
/// column: the declared type belongs to the column, so `GROUP BY born_ts`
/// prints the same ISO text `SELECT born_ts` does rather than the decimal of
/// its microseconds. A DIVIDED key (`GROUP BY col / n`) is a bucket number
/// and not an instant, so it stays an integer and never reaches here as
/// `Some`.
fn group_key_value(value: Option<&OwnedScalarValue>, iso: Option<bool>) -> SqlValue {
    match value {
        None | Some(OwnedScalarValue::Nullish) => SqlValue::Null,
        Some(OwnedScalarValue::Bool(v)) => SqlValue::Bool(*v),
        Some(OwnedScalarValue::I64(v)) => match iso {
            Some(true) => SqlValue::Text(functions::format_date(*v)),
            Some(false) => SqlValue::Text(functions::format_timestamp(*v)),
            None => SqlValue::Int(*v),
        },
        Some(OwnedScalarValue::F64(v)) => SqlValue::Float(*v),
        Some(OwnedScalarValue::Text(v)) => SqlValue::Text(v.clone()),
    }
}

impl AggregatePlan {
    pub(crate) fn row(&self, group: &GroupRow) -> SqlRow {
        let values = self
            .outputs
            .iter()
            .map(|output| match output {
                AggOutput::Key => group_key_value(group.key.as_ref(), self.key_iso),
                AggOutput::Value(at) => group
                    .values
                    .get(*at)
                    .map_or(SqlValue::Missing, agg_value),
            })
            .collect();
        SqlRow {
            id: group_identity(self.collection),
            values,
        }
    }

    /// Prepare the aggregate this plan compiled to and hand it to `body`.
    pub(crate) fn with_aggregate<T>(
        &self,
        db: &Database,
        body: &mut dyn FnMut(&mut sekejap_core::collections::PreparedAggregate<'_>) -> SqlResult2<T>,
    ) -> SqlResult2<T> {
        let accumulators: Vec<Accumulator<'_>> = self
            .accumulators
            .iter()
            .map(|accumulator| Accumulator {
                function: accumulator.function,
                input: accumulator.input.as_ref().map(|input| match input {
                    OwnedAggInput::Index(index) => AggregateInput::Index(*index),
                    OwnedAggInput::Field(field) => AggregateInput::Field(field.as_str()),
                }),
            })
            .collect();
        let group = self.group.as_ref().map(|group| match group {
            OwnedGroupKey::Index(index) => GroupKey::Index(*index),
            OwnedGroupKey::Field(field) => GroupKey::Field(field.as_str()),
            OwnedGroupKey::IndexDiv { index, divisor } => GroupKey::IndexDiv {
                index: *index,
                divisor: *divisor,
            },
        });
        with_borrowed_filters(&self.filters, &[], &[], &mut |filters| {
            let mut prepared = db.prepare_aggregate(AggregateRequest {
                collection: self.collection,
                filters,
                group,
                accumulators: &accumulators,
                having: &self.having,
                order: self.order,
                driver: self.driver,
                total_limit: self.limit,
            })?;
            body(&mut prepared)
        })
    }
}

impl Compiler<'_> {
    // ── GROUP BY / DISTINCT / the aggregate functions (QL_CONTRACT §4.7) ──

    /// `Some` when this statement is an aggregate: it names an aggregate
    /// function, a `GROUP BY`, a `HAVING` or a `DISTINCT`. `None` leaves it
    /// to [`Compiler::select`], which is the ordinary row path.
    pub(super) fn aggregate(&mut self, statement: &SelectStmt) -> SqlResult2<Option<AggregatePlan>> {
        let has_function = statement
            .items
            .iter()
            .any(|(item, _)| matches!(item, SelectItem::Aggregate { .. }));
        if !has_function
            && !statement.distinct
            && statement.group.is_none()
            && statement.having.is_empty()
        {
            return Ok(None);
        }
        let Source::Table(table) = &statement.source else {
            return Err(SqlError::unsupported(
                "an aggregate over GRAPH_TABLE: the aggregate atomic folds the candidates of ONE collection's plan; a traversal's COLUMNS are rows, and folding them is the path-accumulator item (QL_CONTRACT §4.3)",
            ));
        };
        let c = collection(self.db, table)?;

        let mut filters: Vec<OwnedFilter> = Vec::new();
        for expr in &statement.predicates {
            filters.push(self.where_filter(c, expr)?);
        }

        // The group key. `DISTINCT col` IS `GROUP BY col` with no
        // accumulators, so it lands on the same field.
        let group_expr = match (&statement.group, statement.distinct) {
            (Some(_), true) => {
                return Err(SqlError::unsupported(
                    "SELECT DISTINCT with GROUP BY: DISTINCT is a group with no accumulators, so writing both names the group twice",
                ))
            }
            (Some(group), false) => Some(group.clone()),
            (None, true) => {
                let mut named = None;
                for (item, _) in &statement.items {
                    match item {
                        SelectItem::Column(name) if named.is_none() => {
                            named = Some(name.clone());
                        }
                        SelectItem::Column(_) => {
                            return Err(SqlError::Refused {
                                keyword: "DISTINCT <two columns>".into(),
                                tier: Tier::Three,
                                reason: "QL_CONTRACT §4.7: DISTINCT is a group with no accumulators, and GROUP BY takes ONE key; a composite key has no atomic.",
                            })
                        }
                        _ => {
                            return Err(SqlError::unsupported(
                                "SELECT DISTINCT takes one column: it is a group with no accumulators",
                            ))
                        }
                    }
                }
                Some(GroupExpr {
                    column: named.ok_or_else(|| {
                        SqlError::unsupported("SELECT DISTINCT names no column")
                    })?,
                    divisor: None,
                })
            }
            (None, false) => None,
        };

        let group = match &group_expr {
            None => None,
            Some(GroupExpr { column, divisor }) => {
                let kind = self.kind_of(c, column)?;
                let index = self.scalar_index_opt(c, column)?;
                Some(match (divisor, index) {
                    (Some(divisor), Some(index)) if kind == Kind::Int => {
                        OwnedGroupKey::IndexDiv {
                            index,
                            divisor: *divisor,
                        }
                    }
                    (Some(_), _) => {
                        // The brief's fallback, said out loud: the expression
                        // group key rides the posting, so without an Int
                        // scalar index there is nothing to compute it from
                        // index-side and it is REFUSED rather than emulated
                        // over rows.
                        return Err(SqlError::Refused {
                            keyword: "GROUP BY <expression>".into(),
                            tier: Tier::Two,
                            reason: "QL_CONTRACT §4.7: `GROUP BY col / n` is accepted only when it can be computed INDEX-SIDE from the posting -- an Int scalar index on the column, whose own order the truncating division is monotone in. Without one, write `GROUP BY col` with a range filter instead; an expression folded over rows would be a scan wearing a group key's clothes.",
                        });
                    }
                    (None, Some(index)) => OwnedGroupKey::Index(index),
                    (None, None) => OwnedGroupKey::Field(column.clone()),
                })
            }
        };

        // A declared TIMESTAMPTZ/DATE group key prints as ISO text, the same
        // way the same column does in a row answer. A DIVIDED key is a bucket
        // number rather than an instant, so it stays an integer.
        let key_iso = match &group_expr {
            Some(GroupExpr {
                column,
                divisor: None,
            }) => self
                .time_column(c, column)?
                .map(|declared| declared == "DATE"),
            _ => None,
        };

        // The select list: the group key, and the aggregate functions.
        let mut columns: Vec<String> = Vec::new();
        let mut outputs: Vec<AggOutput> = Vec::new();
        let mut accumulators: Vec<OwnedAccumulator> = Vec::new();
        // The written form of each accumulator, so HAVING and ORDER BY can
        // find the one they name.
        let mut written: Vec<(AggFunc, AggArg, Option<String>)> = Vec::new();
        for (item, alias) in &statement.items {
            match item {
                SelectItem::Function(expr) => {
                    return Err(SqlError::unsupported(format!(
                        "`{}` in a folded answer: QL_CONTRACT §4.1 and §4.2 make a function over projected values a ROW function, and a folded answer returns groups, not rows. Group by the value the function computes (`GROUP BY col`), or select the function without folding",
                        expr.written()
                    )))
                }
                SelectItem::Aggregate { function, argument } => {
                    let accumulator = self.accumulator(c, *function, argument)?;
                    let at = accumulators.len();
                    accumulators.push(accumulator);
                    written.push((*function, argument.clone(), alias.clone()));
                    columns.push(alias.clone().unwrap_or_else(|| function.written().to_owned()));
                    outputs.push(AggOutput::Value(at));
                }
                SelectItem::Column(name) => {
                    let Some(GroupExpr { column, .. }) = &group_expr else {
                        return Err(SqlError::unsupported(format!(
                            "`{name}` is neither an aggregate nor a GROUP BY key: a statement that folds rows can only report what is the same for every row of a group"
                        )));
                    };
                    if name != column {
                        return Err(SqlError::unsupported(format!(
                            "`{name}` is not the GROUP BY key `{column}`: a statement that folds rows can only report the key it grouped by"
                        )));
                    }
                    columns.push(alias.clone().unwrap_or_else(|| name.clone()));
                    outputs.push(AggOutput::Key);
                }
                SelectItem::Divided { column, divisor } => {
                    let Some(group) = &group_expr else {
                        return Err(SqlError::unsupported(format!(
                            "`{column} / {divisor}` is neither an aggregate nor a GROUP BY key"
                        )));
                    };
                    if group.column != *column || group.divisor != Some(*divisor) {
                        return Err(SqlError::unsupported(format!(
                            "`{column} / {divisor}` is not the GROUP BY key: a statement that folds rows can only report the key it grouped by"
                        )));
                    }
                    columns.push(alias.clone().unwrap_or_else(|| column.clone()));
                    outputs.push(AggOutput::Key);
                }
                SelectItem::Star => {
                    return Err(SqlError::unsupported(
                        "SELECT * with an aggregate: a folded answer has no row to expand",
                    ))
                }
                SelectItem::Id | SelectItem::Key => {
                    return Err(SqlError::unsupported(
                        "an entity id or external key with an aggregate: a group is not a row and has neither",
                    ))
                }
                SelectItem::OrderValue(what) => {
                    return Err(SqlError::unsupported(format!(
                        "{what} with an aggregate: the only expressions a folded answer reports are its group key and its accumulators"
                    )))
                }
            }
        }
        if group_expr.is_some() && !outputs.contains(&AggOutput::Key) && accumulators.is_empty() {
            // `SELECT DISTINCT col` always names the key; a bare
            // `GROUP BY col` with nothing selected has nothing to report.
            return Err(SqlError::unsupported(
                "GROUP BY with an empty select list reports nothing",
            ));
        }

        // HAVING. A predicate may name an aggregate the select list does not
        // report, exactly as Postgres allows; that one becomes a HIDDEN
        // accumulator -- it is folded, it is not a column.
        let mut having = Vec::new();
        for predicate in &statement.having {
            let at = match written.iter().position(|(function, argument, _)| {
                *function == predicate.function && *argument == predicate.argument
            }) {
                Some(at) => at,
                None => {
                    let accumulator =
                        self.accumulator(c, predicate.function, &predicate.argument)?;
                    accumulators.push(accumulator);
                    written.push((predicate.function, predicate.argument.clone(), None));
                    accumulators.len() - 1
                }
            };
            if matches!(predicate.function, AggFunc::Min | AggFunc::Max) {
                let column = match &predicate.argument {
                    AggArg::Column(name) => name.clone(),
                    AggArg::Star => String::new(),
                };
                if !column.is_empty() && self.kind_of(c, &column)? == Kind::Text {
                    return Err(SqlError::unsupported(format!(
                        "HAVING {}({column}) compares numbers, and this accumulator's value is text",
                        predicate.function.written()
                    )));
                }
            }
            having.push(GroupPredicate {
                accumulator: at,
                op: match predicate.op {
                    CmpOp::Eq => GroupCmp::Eq,
                    CmpOp::Ne => GroupCmp::Ne,
                    CmpOp::Lt => GroupCmp::Lt,
                    CmpOp::Le => GroupCmp::Le,
                    CmpOp::Gt => GroupCmp::Gt,
                    CmpOp::Ge => GroupCmp::Ge,
                },
                value: self.f64_of(&predicate.value)?,
            });
        }

        // ORDER BY: the group key, or one accumulator by its alias.
        let order = match &statement.order {
            None => GroupOrder::Key,
            Some(OrderKey::Column { column, descending }) => {
                let is_key = group_expr
                    .as_ref()
                    .is_some_and(|group| group.column == *column)
                    || outputs.iter().zip(&columns).any(|(output, name)| {
                        *output == AggOutput::Key && name == column
                    });
                if is_key {
                    if *descending {
                        return Err(SqlError::unsupported(
                            "ORDER BY <group key> DESC: the groups arrive in the driving index's own ASCENDING order, and reversing them would mean holding every group to turn it round -- which is the hashed shape's sort, and it sorts by an accumulator, not by the key",
                        ));
                    }
                    GroupOrder::Key
                } else {
                    let at = written
                        .iter()
                        .position(|(function, _, alias)| {
                            alias.as_deref() == Some(column.as_str())
                                || (alias.is_none() && function.written() == column)
                        })
                        .ok_or_else(|| {
                            SqlError::unsupported(format!(
                                "ORDER BY `{column}`: a folded answer is ordered by its group key or by one of its own aggregate aliases"
                            ))
                        })?;
                    GroupOrder::Accumulator {
                        at,
                        direction: if *descending {
                            SortDirection::Descending
                        } else {
                            SortDirection::Ascending
                        },
                    }
                }
            }
            Some(_) => {
                return Err(SqlError::unsupported(
                    "ORDER BY on a folded answer takes the group key or an aggregate alias; a distance, a vector or a BM25 ranking ranks ROWS",
                ))
            }
        };

        let driver = if filters
            .iter()
            .any(|filter| matches!(filter, OwnedFilter::Key { .. }))
        {
            CandidateDriver::Keys
        } else {
            CandidateDriver::Auto
        };

        Ok(Some(AggregatePlan {
            collection: c,
            columns,
            outputs,
            filters,
            group,
            accumulators,
            having,
            order,
            driver,
            limit: statement.limit,
            key_iso,
            text: String::new(),
        }))
    }

    fn accumulator(
        &mut self,
        c: CollectionId,
        function: AggFunc,
        argument: &AggArg,
    ) -> SqlResult2<OwnedAccumulator> {
        Ok(match (function, argument) {
            (AggFunc::Count, AggArg::Star) => OwnedAccumulator {
                function: AggregateFn::CountStar,
                input: None,
            },
            (_, AggArg::Star) => {
                return Err(SqlError::unsupported(format!(
                    "{}(*) is not a function",
                    function.written()
                )))
            }
            (function, AggArg::Column(column)) => {
                let kind = self.kind_of(c, column)?;
                if matches!(function, AggFunc::Sum | AggFunc::Avg)
                    && !matches!(kind, Kind::Int | Kind::Real)
                {
                    return Err(SqlError::unsupported(format!(
                        "{}({column}): sum and avg take a numeric column",
                        function.written()
                    )));
                }
                let input = match self.scalar_index_opt(c, column)? {
                    Some(index) => OwnedAggInput::Index(index),
                    None => OwnedAggInput::Field(column.clone()),
                };
                OwnedAccumulator {
                    function: match function {
                        AggFunc::Count => AggregateFn::Count,
                        AggFunc::Sum => AggregateFn::Sum,
                        AggFunc::Min => AggregateFn::Min,
                        AggFunc::Max => AggregateFn::Max,
                        AggFunc::Avg => AggregateFn::Avg,
                    },
                    input: Some(input),
                }
            }
        })
    }

}
