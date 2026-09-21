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
            assignments.push((column, self.literal()?));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        let key = self.key_equality("UPDATE")?;
        Ok(Stmt::Update {
            table,
            assignments,
            key,
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
        let table = self.name()?;
        let key = self.key_equality("DELETE")?;
        Ok(Stmt::Delete { table, key })
    }

    /// `WHERE <key column> = <literal>`, the only shape an UPDATE or a DELETE
    /// takes: both compile to a write AT ONE KEY.
    fn key_equality(&mut self, statement: &'static str) -> SqlResult2<Literal> {
        self.expect_word("WHERE")?;
        let column = self.name()?;
        if !super::is_key_column(&column) {
            return Err(SqlError::unsupported(format!(
                "{statement} ... WHERE {column} = ...: a write names ONE key, written `{}`; `put`/`delete` take a key, and a predicated write would be a scan plus a write per row",
                super::KEY_COLUMN
            )));
        }
        self.expect(&Tok::Eq)?;
        let key = self.literal()?;
        if self.word().as_deref() == Some("AND") {
            return Err(SqlError::unsupported(format!(
                "{statement} ... WHERE key = ... AND ...: a write names ONE key and nothing else"
            )));
        }
        Ok(key)
    }

}
