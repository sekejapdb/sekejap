use super::*;

impl Parser {
    // ── SELECT ───────────────────────────────────────────────────────────

    pub(super) fn select(&mut self) -> SqlResult2<SelectStmt> {
        self.expect_word("SELECT")?;
        // `SELECT DISTINCT col` is a group with no accumulators -- the same
        // aggregate atomic, said with one keyword (QL_CONTRACT §4.7).
        let distinct = self.eat_word("DISTINCT");
        if distinct && self.word().as_deref() == Some("ON") {
            return Err(SqlError::Refused {
                keyword: "DISTINCT ON".into(),
                tier: super::Tier::Three,
                reason: "QL_CONTRACT §4.7: DISTINCT is a group with no accumulators; DISTINCT ON picks a representative ROW per group, which needs a per-group ranking the aggregate atomic does not have.",
            });
        }
        let mut items = Vec::new();
        loop {
            let item = self.select_item()?;
            let alias = if self.eat_word("AS") {
                Some(self.name()?)
            } else if matches!(self.peek(), Tok::Word(_) | Tok::Quoted(_))
                && self.word().as_deref() != Some("FROM")
            {
                self.guard_word()?;
                Some(self.name()?)
            } else {
                None
            };
            items.push((item, alias));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect_word("FROM")?;
        let source = if self.word().as_deref() == Some("GRAPH_TABLE") {
            Source::Graph(Box::new(self.graph_table()?))
        } else {
            let table = self.name()?;
            if self.eat_word("AS") {
                let _ = self.name()?;
            }
            Source::Table(table)
        };
        if let Some(word) = self.word() {
            if matches!(
                word.as_str(),
                "JOIN" | "INNER" | "LEFT" | "RIGHT" | "FULL" | "CROSS" | "NATURAL"
            ) {
                return Err(refuse::refuse("JOIN"));
            }
            if word == "," {
                return Err(refuse::refuse("JOIN"));
            }
        }
        if matches!(self.peek(), Tok::Comma) {
            return Err(refuse::refuse("JOIN"));
        }
        let predicates = if self.eat_word("WHERE") {
            self.where_clause()?
        } else {
            Vec::new()
        };
        if self.word().as_deref() == Some("WINDOW") {
            return Err(refuse::refuse("OVER"));
        }
        let group = if self.eat_word("GROUP") {
            self.expect_word("BY")?;
            Some(self.group_expression()?)
        } else {
            None
        };
        let having = if self.eat_word("HAVING") {
            self.having()?
        } else {
            Vec::new()
        };
        let order = if self.eat_word("ORDER") {
            self.expect_word("BY")?;
            let key = self.order_key()?;
            if matches!(self.peek(), Tok::Comma) {
                return Err(SqlError::Refused {
                    keyword: "ORDER BY <two keys>".into(),
                    tier: super::Tier::Three,
                    reason: "QL_CONTRACT §5 deviation 3: ORDER BY takes ONE key; an expression is one key (the Score atomic). Two keys are a refusal because there is no second-key atomic: a page ranks by one RankKey.",
                });
            }
            Some(key)
        } else {
            None
        };
        let limit = if self.eat_word("LIMIT") {
            match self.bump() {
                Tok::Num(n, true) if n >= 0.0 => Some(n as usize),
                other => {
                    return Err(SqlError::syntax(
                        format!("LIMIT needs a whole number, found `{}`", other.written()),
                        self.here(),
                    ))
                }
            }
        } else {
            None
        };
        if self.word().as_deref() == Some("OFFSET") {
            return Err(refuse::refuse("OFFSET"));
        }
        if self.word().as_deref() == Some("UNION") {
            return Err(refuse::refuse("UNION"));
        }
        Ok(SelectStmt {
            items,
            source,
            predicates,
            distinct,
            group,
            having,
            order,
            limit,
        })
    }

    /// `GROUP BY col` or `GROUP BY col / n`. One key: a second one would be a
    /// composite group, which has no atomic here (the streaming shape is a
    /// statement about ONE index's own order).
    fn group_expression(&mut self) -> SqlResult2<GroupExpr> {
        self.guard_word()?;
        let column = self.name()?;
        let divisor = if self.eat(&Tok::Slash) {
            match self.bump() {
                Tok::Num(value, true) if value >= 1.0 => Some(value as i64),
                other => {
                    return Err(SqlError::unsupported(format!(
                        "GROUP BY {column} / n takes a positive whole divisor -- only then is the expression monotone in the index's own order, which is what lets it stream; found `{}`",
                        other.written()
                    )))
                }
            }
        } else {
            None
        };
        if matches!(self.peek(), Tok::Comma) {
            return Err(SqlError::Refused {
                keyword: "GROUP BY <two keys>".into(),
                tier: super::Tier::Three,
                reason: "QL_CONTRACT §4.7: GROUP BY takes ONE key. A composite key has no atomic: the streaming shape is the statement that ONE index's own order delivers the groups contiguously, and two columns are not one index's order.",
            });
        }
        if !matches!(self.peek(), Tok::Eof | Tok::Semicolon)
            && !matches!(
                self.word().as_deref(),
                Some("HAVING" | "ORDER" | "LIMIT" | "OFFSET" | "UNION")
            )
        {
            return Err(SqlError::unsupported(format!(
                "GROUP BY {column}: the only grouping expression this slice accepts is `col` or `col / n`"
            )));
        }
        Ok(GroupExpr { column, divisor })
    }

    /// `HAVING <agg>(<arg>) <cmp> <value>`, conjoined with AND.
    fn having(&mut self) -> SqlResult2<Vec<HavingPredicate>> {
        let mut out = Vec::new();
        loop {
            let at = self.here();
            let Some((function, argument)) = self.aggregate_call()? else {
                return Err(SqlError::unsupported(
                    "HAVING is a predicate on a group's ACCUMULATOR value: write `HAVING count(*) > 100`, not a row predicate (that is WHERE)",
                ));
            };
            let op = match self.bump() {
                Tok::Eq => CmpOp::Eq,
                Tok::Ne => CmpOp::Ne,
                Tok::Lt => CmpOp::Lt,
                Tok::Le => CmpOp::Le,
                Tok::Gt => CmpOp::Gt,
                Tok::Ge => CmpOp::Ge,
                other => {
                    return Err(SqlError::syntax(
                        format!("HAVING compares an accumulator, found `{}`", other.written()),
                        at,
                    ))
                }
            };
            let value = self.literal()?;
            out.push(HavingPredicate {
                function,
                argument,
                op,
                value,
            });
            if self.word().as_deref() == Some("OR") {
                return Err(SqlError::unsupported(
                    "OR inside HAVING: a HAVING predicate is applied to a finished group, and a union of group predicates has no accumulator atomic (QL_CONTRACT §4.7)",
                ));
            }
            if !self.eat_word("AND") {
                break;
            }
        }
        Ok(out)
    }

    /// `count(*)`, `count(col)`, `sum(col)`, `min(col)`, `max(col)` or
    /// `avg(col)` at the cursor. `None` when the cursor is on something else,
    /// with nothing consumed.
    fn aggregate_call(&mut self) -> SqlResult2<Option<(AggFunc, AggArg)>> {
        let Some(word) = self.word() else {
            return Ok(None);
        };
        let function = match word.as_str() {
            "COUNT" => AggFunc::Count,
            "SUM" => AggFunc::Sum,
            "MIN" => AggFunc::Min,
            "MAX" => AggFunc::Max,
            "AVG" => AggFunc::Avg,
            _ => return Ok(None),
        };
        if !matches!(self.peek_at(1), Tok::LParen) {
            return Ok(None);
        }
        self.bump();
        self.bump();
        if self.eat_word("DISTINCT") {
            return Err(SqlError::Refused {
                keyword: "COUNT DISTINCT".into(),
                tier: super::Tier::Two,
                reason: "QL_CONTRACT §4.7: an aggregate over DISTINCT values needs a per-group distinct set, which is a second unbounded structure inside each group; the bounded atomic here is one accumulator per group.",
            });
        }
        let argument = if self.eat(&Tok::Star) {
            if function != AggFunc::Count {
                return Err(SqlError::unsupported(format!(
                    "{}(*) is not a function: only count(*) counts rows without reading a column",
                    function.written()
                )));
            }
            AggArg::Star
        } else {
            self.guard_word()?;
            AggArg::Column(self.name()?)
        };
        self.expect(&Tok::RParen)?;
        Ok(Some((function, argument)))
    }

    fn select_item(&mut self) -> SqlResult2<SelectItem> {
        if self.eat(&Tok::Star) {
            return Ok(SelectItem::Star);
        }
        if let Some((function, argument)) = self.aggregate_call()? {
            return Ok(SelectItem::Aggregate { function, argument });
        }
        // A §4.1 / §4.2 ROW function, or a plain name that a cast or a `||`
        // turns into one. Cost is proportional to the rows RETURNED and
        // EXPLAIN says so (QL_CONTRACT §4.1, §4.2).
        if self.at_row_function() || self.row_expression_ahead() {
            let expr = self.row_expr()?;
            return Ok(SelectItem::Function(Box::new(expr)));
        }
        // A plain name, possibly qualified, is a column; anything else is an
        // expression, and the only expression a select list can hold here is
        // this statement's own ranking value.
        let is_plain_name = matches!(self.peek(), Tok::Word(_) | Tok::Quoted(_))
            && !matches!(self.peek_at(1), Tok::LParen)
            && !matches!(
                self.peek_at(1),
                Tok::VecCosine | Tok::VecL2 | Tok::VecDot | Tok::VecL1 | Tok::Cast
            )
            && !(matches!(self.peek_at(1), Tok::Dot) && matches!(self.peek_at(3), Tok::LParen));
        if is_plain_name {
            self.guard_word()?;
            let name = self.name()?;
            // `col / n`: the grouping expression, written in the select list
            // the way a statement reports what it grouped by.
            if matches!(self.peek(), Tok::Slash) && matches!(self.peek_at(1), Tok::Num(_, true)) {
                self.bump();
                let Tok::Num(value, _) = self.bump() else {
                    unreachable!("the divisor was just peeked");
                };
                if value >= 1.0 {
                    return Ok(SelectItem::Divided {
                        column: name,
                        divisor: value as i64,
                    });
                }
                return Err(SqlError::unsupported(format!(
                    "{name} / {value}: a divided group key takes a positive whole divisor"
                )));
            }
            return Ok(if name == super::ID_COLUMN {
                SelectItem::Id
            } else if super::is_key_column(&name) {
                SelectItem::Key
            } else {
                SelectItem::Column(name)
            });
        }
        let at = self.here();
        let _ = self.expression(0)?;
        Ok(SelectItem::OrderValue(format!("expression at byte {at}")))
    }

}
