//! A stage: its statements, then its `RETURN`.
//!
//! ```text
//! stage     := statement* return
//! statement := MATCH pattern (',' pattern)* [WHERE expr]
//!            | OPTIONAL MATCH pattern [WHERE expr]
//!            | LET name '=' expr (',' name '=' expr)*
//!            | FILTER expr
//!            | FOR name IN expr
//! return    := RETURN [DISTINCT] ('*' | item (',' item)*)
//!              [GROUP BY expr (',' expr)*]
//!              [ORDER BY expr [ASC | DESC] (',' expr [ASC | DESC])*]
//!              [OFFSET count] [LIMIT count]
//! item      := expr [AS name]
//! count     := integer literal | '$' n
//! ```
//!
//! The words are keywords only here, by position: `LET`, `FILTER`, `FOR`,
//! `MATCH` and `OPTIONAL MATCH` where a statement starts, the `RETURN` clauses after the
//! items. `NEXT` between stages is read by the body (`mod.rs`).

use super::super::ast::{Count, OrderItem, Return, ReturnItem, Stage, Statement};
use crate::lexer::Tok;
use crate::parser::Parser;
use crate::refuse;
use crate::{SqlError, SqlResult2};

/// 2^53, one past the largest count a literal states exactly: the lexer
/// reads a number as a double, and from 2^53 on two integer literals can
/// read as one double (`9007199254740993` reads as 2^53). A larger count is
/// bound as `$n`, which reaches `i64::MAX`.
const INEXACT_FROM: f64 = 9_007_199_254_740_992.0;

impl Parser {
    pub(super) fn gql_stage(&mut self) -> SqlResult2<Stage> {
        let mut statements = Vec::new();
        loop {
            if self.eat_word("MATCH") {
                statements.push(self.gql_match(false)?);
            } else if self.gql_eat_pair("OPTIONAL", "MATCH") {
                statements.push(self.gql_match(true)?);
            } else if self.eat_word("LET") {
                statements.push(self.gql_let()?);
            } else if self.eat_word("FILTER") {
                statements.push(Statement::Filter(self.gql_expr()?));
            } else if self.eat_word("FOR") {
                let var = self.gql_name("the FOR variable")?;
                self.expect_word("IN")?;
                statements.push(Statement::For {
                    var,
                    list: self.gql_expr()?,
                });
            } else {
                break;
            }
        }
        if !self.eat_word("RETURN") {
            return Err(self.gql_expected("`MATCH`, `OPTIONAL MATCH`, `LET`, `FILTER`, `FOR` or `RETURN`"));
        }
        Ok(Stage {
            statements,
            ret: self.gql_return()?,
        })
    }

    /// `MATCH`, or `OPTIONAL MATCH` when `optional`, has been read.
    fn gql_match(&mut self, optional: bool) -> SqlResult2<Statement> {
        let mut patterns = vec![self.gql_path_pattern()?];
        while self.eat(&Tok::Comma) {
            if optional {
                return Err(refuse::gql_refuse("OPTIONAL MATCH with comma patterns"));
            }
            patterns.push(self.gql_path_pattern()?);
        }
        let where_ = if self.eat_word("WHERE") {
            Some(self.gql_expr()?)
        } else {
            None
        };
        Ok(Statement::Match {
            patterns,
            where_,
            optional,
        })
    }

    /// `LET` has been read.
    fn gql_let(&mut self) -> SqlResult2<Statement> {
        let mut assignments = Vec::new();
        loop {
            let name = self.gql_name("a LET variable")?;
            self.expect(&Tok::Eq)?;
            assignments.push((name, self.gql_expr()?));
            if !self.eat(&Tok::Comma) {
                return Ok(Statement::Let(assignments));
            }
        }
    }

    /// `RETURN` has been read.
    fn gql_return(&mut self) -> SqlResult2<Return> {
        let distinct = self.eat_word("DISTINCT");
        let star = self.eat(&Tok::Star);
        let mut items = Vec::new();
        while !star {
            let expr = self.gql_expr()?;
            let alias = if self.eat_word("AS") {
                Some(self.gql_name("an output column name")?)
            } else {
                None
            };
            items.push(ReturnItem { expr, alias });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        let group_by = if self.gql_eat_pair("GROUP", "BY") {
            let mut keys = vec![self.gql_expr()?];
            while self.eat(&Tok::Comma) {
                keys.push(self.gql_expr()?);
            }
            Some(keys)
        } else {
            None
        };
        let mut order_by = Vec::new();
        if self.gql_eat_pair("ORDER", "BY") {
            loop {
                let expr = self.gql_expr()?;
                let descending = if self.eat_word("DESC") {
                    true
                } else {
                    self.eat_word("ASC");
                    false
                };
                order_by.push(OrderItem { expr, descending });
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let offset = if self.eat_word("OFFSET") {
            Some(self.gql_count("OFFSET")?)
        } else {
            None
        };
        let limit = if self.eat_word("LIMIT") {
            Some(self.gql_count("LIMIT")?)
        } else {
            None
        };
        Ok(Return {
            distinct,
            star,
            items,
            group_by,
            order_by,
            offset,
            limit,
        })
    }

    /// Two words in a row, consumed only together.
    fn gql_eat_pair(&mut self, first: &str, second: &str) -> bool {
        if self.word().as_deref() == Some(first) && self.word_at(1).as_deref() == Some(second) {
            self.bump();
            self.bump();
            return true;
        }
        false
    }

    /// An `OFFSET` or `LIMIT` count (design Q13): a whole-number literal or a
    /// `$n`, whose value an execution checks when it opens.
    fn gql_count(&mut self, clause: &str) -> SqlResult2<Count> {
        let at = self.here();
        match self.bump() {
            Tok::Num(n, true) if (0.0..INEXACT_FROM).contains(&n) => Ok(Count::Lit(n as u64)),
            Tok::Num(n, true) if n >= INEXACT_FROM => Err(SqlError::syntax(
                format!("{clause} {n} is not below 2^53, where a literal count is read exactly; bind it as a parameter (`{clause} $1`), which reaches 9223372036854775807"),
                at,
            )),
            Tok::Param(n) => Ok(Count::Param(n)),
            other => Err(SqlError::syntax(
                format!(
                    "{clause} is a whole number from 0 or a parameter `$n`, found `{}`",
                    other.written()
                ),
                at,
            )),
        }
    }
}
