//! A stage: its statements, then its `RETURN`.
//!
//! ```text
//! stage     := statement* return
//! statement := MATCH pattern (',' pattern)* [WHERE expr]
//!            | OPTIONAL MATCH pattern (',' pattern)* [WHERE expr]
//!            | LET name '=' expr (',' name '=' expr)*
//!            | FILTER expr
//!            | FOR name IN expr
//! return    := RETURN [DISTINCT] ('*' | item (',' item)*)
//!              [GROUP BY expr (',' expr)*]
//!              [ORDER BY expr [ASC | DESC] (',' expr [ASC | DESC])*]
//!              [OFFSET count] [LIMIT count]
//! item      := expr [AS name]
//! count     := integer literal | '$' n
//!
//! exists_body := pattern (',' pattern)* [WHERE expr]  -- the short form
//!              | statement+ [return]                  -- the full form
//! ```
//!
//! An `EXISTS` body (M5-C, design §2.3) is read as statements: the short
//! form is its `MATCH`. A `RETURN` in the full form is accepted and its
//! items ignored, as in Google's documented form; an aggregate, `GROUP BY`,
//! `ORDER BY`, `OFFSET` or `LIMIT` there is refused by name (Q18), because
//! each changes whether a row exists only in ways a `FILTER` in the body
//! says plainly.
//!
//! The words are keywords only here, by position: `LET`, `FILTER`, `FOR`,
//! `MATCH` and `OPTIONAL MATCH` where a statement starts, the `RETURN` clauses after the
//! items. `NEXT` between stages is read by the body (`mod.rs`).
//!
//! `OPTIONAL MATCH p1, p2 [WHERE ...]` matches every pattern optional
//! TOGETHER (M5-A, design Q3 -> Q17): one `WHERE`, and no match for one
//! pattern empties the whole combined match, exactly like a plain `MATCH`'s
//! comma patterns (`Planner::matched`, `lang/src/gql/bind.rs::bind_match`).
//! ISO's block form, `OPTIONAL { MATCH ...; MATCH ... }`, is a different
//! construct and is not this: `OPTIONAL` not followed by `MATCH` is refused
//! by name (`OPTIONAL block`, P1, `gql_listed`).
//!
//! The outer `SELECT` over a GQL relation (design §5.5) is read here too,
//! in the GQL dialect, as the `RETURN` of one more stage whose `WHERE` is
//! its `FILTER`:
//!
//! ```text
//! select := SELECT [DISTINCT] ('*' | item (',' item)*) FROM GRAPH_TABLE (...) [[AS] alias]
//!           [WHERE expr] [GROUP BY expr (',' expr)*] [HAVING expr]
//!           [ORDER BY expr [ASC | DESC] (',' expr [ASC | DESC])*]
//!           [LIMIT count] [OFFSET count]         -- in either order
//! item   := expr [[AS] name]
//! ```
//!
//! `alias.column` names a column of the relation. `DISTINCT` is the
//! existing `Distinct` operator, over the projected row (M3-D2, brief
//! gap 1); `SELECT DISTINCT ON (...)` stays refused by name, as it is on
//! the plain SQL side, since picking a representative row per group needs
//! a per-group ranking the aggregate atomic does not have. `HAVING` is a
//! `Filter` right after the outer `Aggregate` (brief gap 2), reading only
//! the group's keys and aggregates, PostgreSQL's `42803` otherwise; it may
//! write HAVING with no `GROUP BY`, which then folds the whole relation
//! into one group, as PostgreSQL does.

use super::super::ast::{Count, GqlGraphTable, OrderItem, Outer, Return, ReturnItem, Stage, Statement};
use super::super::ast::Expr;
use crate::lexer::Tok;
use crate::parser::{Dialect, Parser};
use crate::refuse;
use crate::{SqlError, SqlResult2};

/// 2^53, one past the largest count a literal states exactly: the lexer
/// reads a number as a double, and from 2^53 on two integer literals can
/// read as one double (`9007199254740993` reads as 2^53). A larger count is
/// bound as `$n`, which reaches `i64::MAX`.
const INEXACT_FROM: f64 = 9_007_199_254_740_992.0;

impl Parser {
    pub(super) fn gql_stage(&mut self) -> SqlResult2<Stage> {
        let statements = self.gql_statements()?;
        if !self.eat_word("RETURN") {
            return Err(self.gql_expected("`MATCH`, `OPTIONAL MATCH`, `LET`, `FILTER`, `FOR` or `RETURN`"));
        }
        Ok(Stage {
            statements,
            ret: self.gql_return()?,
        })
    }

    /// `EXISTS` has been read, the cursor on `{`: the body, as statements.
    pub(super) fn gql_exists(&mut self) -> SqlResult2<Expr> {
        self.expect(&Tok::LBrace)?;
        let starts_a_statement = matches!(
            self.word().as_deref(),
            Some("MATCH" | "OPTIONAL" | "LET" | "FILTER" | "FOR" | "CALL")
        );
        let statements = if starts_a_statement {
            let statements = self.gql_statements()?;
            if self.eat_word("RETURN") {
                exists_return(&self.gql_return()?)?;
            }
            statements
        } else {
            vec![self.gql_match(false)?]
        };
        if !self.eat(&Tok::RBrace) {
            return Err(self.gql_expected(
                "`MATCH`, `OPTIONAL MATCH`, `LET`, `FILTER`, `FOR`, `RETURN` or `}` in the EXISTS body",
            ));
        }
        Ok(Expr::Exists(statements))
    }

    /// Zero or more statements, up to the first word that starts none.
    fn gql_statements(&mut self) -> SqlResult2<Vec<Statement>> {
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
            } else if self.eat_word("CALL") {
                statements.push(self.gql_call_statement()?);
            } else {
                return Ok(statements);
            }
        }
    }

    /// `CALL` has been read: `(imports) { stage }` (design §2.4). The import
    /// list is required -- `()` imports nothing -- so a bare `CALL { }` is an
    /// error naming it (Q20); `NEXT` inside the body is refused by name (P1,
    /// Q22), and so is `OPTIONAL CALL` (P1, Q21, `gql_listed`).
    fn gql_call_statement(&mut self) -> SqlResult2<Statement> {
        if !self.eat(&Tok::LParen) {
            return Err(self.gql_expected(
                "the import list after CALL: `CALL (a, b) { ... }` names the variables the body sees, `CALL () { ... }` none",
            ));
        }
        let mut imports = Vec::new();
        if !self.eat(&Tok::RParen) {
            loop {
                imports.push(self.gql_name("an imported variable")?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(&Tok::RParen)?;
        }
        self.expect(&Tok::LBrace)?;
        let body = self.gql_stage()?;
        if self.word().as_deref() == Some("NEXT") {
            return Err(refuse::gql_refuse("NEXT inside CALL"));
        }
        if !self.eat(&Tok::RBrace) {
            return Err(self.gql_expected("`}` to close the CALL body"));
        }
        Ok(Statement::Call {
            imports,
            body: Box::new(body),
        })
    }

    /// `MATCH`, or `OPTIONAL MATCH` when `optional`, has been read. Every
    /// comma pattern is optional TOGETHER when `optional` (design Q3 ->
    /// Q17, M5-A): `Planner::matched` plans them exactly as it does a plain
    /// `MATCH`'s comma patterns, and the caller wraps the lot in one
    /// `OptionalApply`.
    fn gql_match(&mut self, optional: bool) -> SqlResult2<Statement> {
        let mut patterns = vec![self.gql_path_pattern()?];
        while self.eat(&Tok::Comma) {
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
        let (star, items) = self.gql_items(None)?;
        let group_by = self.gql_group_by()?;
        let order_by = self.gql_order_by()?;
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

    /// `SELECT` over a GQL relation, the cursor on `SELECT` and its `FROM`
    /// `from` tokens ahead (`Parser::gql_relation_ahead`). The relation is
    /// read first, so the select list, read next, knows its alias.
    pub(crate) fn gql_select(&mut self, from: usize) -> SqlResult2<GqlGraphTable> {
        self.expect_word("SELECT")?;
        let list = self.mark();
        self.reset(list + from);
        let mut relation = self.gql_graph_table()?;
        let tail = self.mark();
        self.reset(list);
        let dialect = std::mem::replace(&mut self.dialect, Dialect::Gql);
        let alias = std::mem::replace(&mut self.relation, relation.alias.clone());
        let outer = self.gql_outer(list + from - 1, tail);
        self.dialect = dialect;
        self.relation = alias;
        relation.outer = outer?;
        Ok(relation)
    }

    /// The select list, up to the `FROM` at `from`, then the clauses from
    /// `tail`, after the relation. `None` for `SELECT *` alone.
    fn gql_outer(&mut self, from: usize, tail: usize) -> SqlResult2<Option<Outer>> {
        let distinct = self.eat_word("DISTINCT");
        if distinct && self.word().as_deref() == Some("ON") {
            return Err(SqlError::Refused {
                keyword: "DISTINCT ON".into(),
                tier: crate::Tier::Three,
                reason: "QL_CONTRACT §4.7: DISTINCT is a group with no accumulators; DISTINCT ON picks a representative ROW per group, which needs a per-group ranking the aggregate atomic does not have.",
            });
        }
        let (star, items) = self.gql_items(Some(from))?;
        if self.mark() != from {
            return Err(self.gql_expected("`,` or `FROM`"));
        }
        self.reset(tail);
        if matches!(
            self.word().as_deref(),
            Some("JOIN" | "INNER" | "LEFT" | "RIGHT" | "FULL" | "CROSS" | "NATURAL")
        ) || matches!(self.peek(), Tok::Comma)
        {
            return Err(refuse::refuse("JOIN"));
        }
        let where_ = if self.eat_word("WHERE") {
            Some(self.gql_expr()?)
        } else {
            None
        };
        let group_by = self.gql_group_by()?;
        let having = if self.eat_word("HAVING") {
            Some(self.gql_expr()?)
        } else {
            None
        };
        let order_by = self.gql_order_by()?;
        let (mut offset, mut limit) = (None, None);
        loop {
            if offset.is_none() && self.eat_word("OFFSET") {
                offset = Some(self.gql_count("OFFSET")?);
            } else if limit.is_none() && self.eat_word("LIMIT") {
                limit = Some(self.gql_count("LIMIT")?);
            } else {
                break;
            }
        }
        let bare = star
            && !distinct
            && where_.is_none()
            && group_by.is_none()
            && having.is_none()
            && order_by.is_empty()
            && offset.is_none()
            && limit.is_none();
        Ok((!bare).then_some(Outer {
            where_,
            select: Return {
                distinct,
                star,
                items,
                group_by,
                order_by,
                offset,
                limit,
            },
            having,
        }))
    }

    /// `* | item, ...` of a `RETURN`, or of an outer `SELECT` whose `FROM`
    /// stands at `from`, where an alias may be written without `AS`.
    fn gql_items(&mut self, from: Option<usize>) -> SqlResult2<(bool, Vec<ReturnItem>)> {
        if self.eat(&Tok::Star) {
            return Ok((true, Vec::new()));
        }
        let mut items = Vec::new();
        loop {
            let expr = self.gql_expr()?;
            let bare_alias = from.is_some_and(|from| self.mark() != from)
                && matches!(self.peek(), Tok::Word(_) | Tok::Quoted(_));
            let alias = if self.eat_word("AS") || bare_alias {
                Some(self.gql_name("an output column name")?)
            } else {
                None
            };
            items.push(ReturnItem { expr, alias });
            if !self.eat(&Tok::Comma) {
                return Ok((false, items));
            }
        }
    }

    /// `[GROUP BY expr, ...]`.
    fn gql_group_by(&mut self) -> SqlResult2<Option<Vec<Expr>>> {
        if !self.gql_eat_pair("GROUP", "BY") {
            return Ok(None);
        }
        let mut keys = vec![self.gql_expr()?];
        while self.eat(&Tok::Comma) {
            keys.push(self.gql_expr()?);
        }
        Ok(Some(keys))
    }

    /// `[ORDER BY expr [ASC | DESC], ...]`.
    fn gql_order_by(&mut self) -> SqlResult2<Vec<OrderItem>> {
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
        Ok(order_by)
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
            Tok::Num(n, Some(_)) if (0.0..INEXACT_FROM).contains(&n) => Ok(Count::Lit(n as u64)),
            Tok::Num(n, Some(_)) if n >= INEXACT_FROM => Err(SqlError::syntax(
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

/// Q18: the `RETURN` of an `EXISTS` body, whose items are ignored, holds no
/// clause that would change whether a row exists.
fn exists_return(ret: &Return) -> SqlResult2<()> {
    let clause = if ret.items.iter().any(|item| item.expr.has_aggregate()) {
        "an aggregate"
    } else if ret.group_by.is_some() {
        "GROUP BY"
    } else if !ret.order_by.is_empty() {
        "ORDER BY"
    } else if ret.offset.is_some() {
        "OFFSET"
    } else if ret.limit.is_some() {
        "LIMIT"
    } else {
        return Ok(());
    };
    Err(SqlError::unsupported(format!(
        "{clause} in the RETURN of an EXISTS {{ ... }} body: EXISTS asks only whether the body gives a row, and {clause} changes that only in ways a FILTER in the body says plainly (GQL profile Q18); the RETURN's items are ignored, so write the condition as a FILTER"
    )))
}
