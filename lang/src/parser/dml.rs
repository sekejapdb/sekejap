use super::*;

impl Parser {
    pub(super) fn insert(&mut self) -> SqlResult2<Stmt> {
        self.expect_word("INSERT")?;
        self.expect_word("INTO")?;
        if self.word().as_deref() == Some("GRAPH") {
            return Err(SqlError::Refused {
                keyword: "INSERT INTO GRAPH".into(),
                tier: super::Tier::Two,
                reason: "QL_CONTRACT §2: `INSERT INTO GRAPH g EDGE type (...) VALUES` compiles to put_edge; the spelling is still open and nothing is built in this slice.",
            });
        }
        let table = self.name()?;
        self.expect(&Tok::LParen)?;
        let mut columns = Vec::new();
        loop {
            columns.push(self.name()?);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen)?;
        self.expect_word("VALUES")?;
        let mut rows = Vec::new();
        loop {
            self.expect(&Tok::LParen)?;
            let mut values = Vec::new();
            loop {
                values.push(self.literal()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(&Tok::RParen)?;
            if values.len() != columns.len() {
                return Err(SqlError::syntax(
                    format!(
                        "row {} has {} value(s) for {} column(s)",
                        rows.len() + 1,
                        values.len(),
                        columns.len()
                    ),
                    self.here(),
                ));
            }
            rows.push(values);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        if self.word().as_deref() == Some("ON") {
            return Err(SqlError::unsupported(
                "INSERT ... ON CONFLICT: `Database::put` replaces the row at a key, and a conditional write has no atomic",
            ));
        }
        if self.word().as_deref() == Some("RETURNING") {
            return Err(SqlError::unsupported(
                "INSERT ... RETURNING: a write reports the rows it affected, not their contents",
            ));
        }
        Ok(Stmt::Insert {
            table,
            columns,
            rows,
        })
    }

    pub(super) fn update(&mut self) -> SqlResult2<Stmt> {
        self.expect_word("UPDATE")?;
        if self.word().as_deref() == Some("GRAPH") {
            return Err(SqlError::Refused {
                keyword: "UPDATE GRAPH".into(),
                tier: super::Tier::Two,
                reason: "QL_CONTRACT §2: `UPDATE GRAPH g EDGE type SET ...` is an edge posting rewrite; not built in this slice.",
            });
        }
        let table = self.name()?;
        self.expect_word("SET")?;
        let mut assignments = Vec::new();
        loop {
            let column = self.name()?;
            self.expect(&Tok::Eq)?;
            assignments.push((column, self.set_value()?));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect_word("WHERE")?;
        // The key-equality form keeps its own statement: one point-get and
        // one put, with no candidate walk to prepare. It is taken only when
        // every assignment is a constant, because a row expression needs the
        // OLD row and `Database::update` merges a patch without reading one.
        let literals: Option<Vec<(String, Literal)>> = assignments
            .iter()
            .map(|(column, value)| match value {
                SetValue::Lit(literal) => Some((column.clone(), literal.clone())),
                SetValue::Row(_) => None,
            })
            .collect();
        if let Some(literals) = literals {
            if let Some(key) = self.key_equality_ahead()? {
                self.no_returning("UPDATE")?;
                return Ok(Stmt::Update {
                    table,
                    assignments: literals,
                    key,
                });
            }
        }
        let predicates = self.where_clause()?;
        self.no_returning("UPDATE")?;
        Ok(Stmt::UpdateWhere {
            table,
            assignments,
            predicates,
        })
    }

    pub(super) fn delete(&mut self) -> SqlResult2<Stmt> {
        self.expect_word("DELETE")?;
        self.expect_word("FROM")?;
        if self.word().as_deref() == Some("GRAPH") {
            return Err(SqlError::Refused {
                keyword: "DELETE FROM GRAPH".into(),
                tier: super::Tier::Two,
                reason: "QL_CONTRACT §2: `DELETE FROM GRAPH g EDGE type WHERE ...` compiles to delete_edge; not built in this slice.",
            });
        }
        // `DELETE FROM ALL`: every collection at once, riding the `FROM ALL`
        // concatenation driver.
        if self.word().as_deref() == Some("ALL") && !matches!(self.peek_at(1), Tok::Dot) {
            self.bump();
            let predicates = if self.eat_word("WHERE") {
                self.where_clause()?
            } else {
                Vec::new()
            };
            let cascade = self.delete_mode()?;
            self.no_returning("DELETE")?;
            return Ok(Stmt::DeleteWhere {
                table: None,
                predicates,
                cascade,
            });
        }
        let table = self.name()?;
        // `DELETE FROM t WHERE _key = ...` is the single-key atomic:
        // `Database::delete` by key, with no candidate walk to prepare.
        if self.word().as_deref() == Some("WHERE") {
            let mark = self.mark();
            self.bump();
            if let Some(key) = self.key_equality_ahead()? {
                self.no_returning("DELETE")?;
                return Ok(Stmt::Delete { table, key });
            }
            self.reset(mark);
        }
        let predicates = if self.eat_word("WHERE") {
            self.where_clause()?
        } else {
            Vec::new()
        };
        let cascade = self.delete_mode()?;
        self.no_returning("DELETE")?;
        Ok(Stmt::DeleteWhere {
            table: Some(table),
            predicates,
            cascade,
        })
    }

    /// `RESTRICT` (the default) or `CASCADE`, GRAPH_CONTRACT 6.1.
    fn delete_mode(&mut self) -> SqlResult2<bool> {
        if self.eat_word("CASCADE") {
            return Ok(true);
        }
        let _ = self.eat_word("RESTRICT");
        Ok(false)
    }

    fn no_returning(&mut self, statement: &'static str) -> SqlResult2<()> {
        if self.word().as_deref() == Some("RETURNING") {
            return Err(SqlError::unsupported(format!(
                "{statement} ... RETURNING: a write reports the rows it affected, not their contents"
            )));
        }
        Ok(())
    }

    /// One `SET column = <value>`.
    ///
    /// A bare constant stays a `Literal` -- the same value an INSERT writes,
    /// checked against the column's declared `Kind` while the statement
    /// compiles. Anything else is a §4.1 / §4.2 ROW EXPRESSION over the same
    /// row, which reaches the engine as the `UpdatePatch` closure boundary.
    fn set_value(&mut self) -> SqlResult2<SetValue> {
        if self.plain_literal_ahead() {
            return Ok(SetValue::Lit(self.literal()?));
        }
        Ok(SetValue::Row(self.row_expr()?))
    }

    /// True when the value at the cursor is ONE literal token and the token
    /// after it ends the assignment. Everything else -- a column name,
    /// arithmetic, `||`, a cast, a function call -- is a row expression.
    fn plain_literal_ahead(&self) -> bool {
        // A scalar subquery is a constant by construction: it reads ONE row
        // by key while the statement compiles.
        if matches!(self.peek(), Tok::LParen) && self.word_at(1).as_deref() == Some("SELECT") {
            return true;
        }
        let at = usize::from(matches!(self.peek(), Tok::Minus));
        let is_value = match self.peek_at(at) {
            Tok::Num(_, _) | Tok::Str(_) | Tok::Param(_) => true,
            Tok::Word(word) => matches!(
                word.to_ascii_uppercase().as_str(),
                "TRUE" | "FALSE" | "NULL"
            ),
            _ => false,
        };
        if !is_value {
            return false;
        }
        match self.peek_at(at + 1) {
            Tok::Comma | Tok::Eof | Tok::Semicolon => true,
            Tok::Word(word) => word.eq_ignore_ascii_case("WHERE"),
            _ => false,
        }
    }

    /// `<key column> = <literal>` and nothing after it, consumed; `None` with
    /// the cursor left where it was when the predicate is anything else.
    ///
    /// The `WHERE` keyword is already consumed by the caller.
    fn key_equality_ahead(&mut self) -> SqlResult2<Option<Literal>> {
        let mark = self.mark();
        if !matches!(self.peek(), Tok::Word(_) | Tok::Quoted(_)) {
            return Ok(None);
        }
        let Ok(column) = self.name() else {
            self.reset(mark);
            return Ok(None);
        };
        if !super::is_key_column(&column) || !self.eat(&Tok::Eq) {
            self.reset(mark);
            return Ok(None);
        }
        let Ok(key) = self.literal() else {
            self.reset(mark);
            return Ok(None);
        };
        // `_key = 'a' AND ...` is a predicated write, not a point write; so
        // is `_key = 'a' CASCADE`, which asks for the per-row edge mode the
        // point delete does not take.
        if matches!(self.peek(), Tok::Eof | Tok::Semicolon)
            || self.word().as_deref() == Some("RETURNING")
        {
            return Ok(Some(key));
        }
        self.reset(mark);
        Ok(None)
    }
}
