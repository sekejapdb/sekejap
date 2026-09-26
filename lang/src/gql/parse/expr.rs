//! GQL expressions, by precedence climbing, with PostgreSQL's precedence.
//!
//! ```text
//! expr       := and (OR and)*
//! and        := not (AND not)*
//! not        := NOT not | is_test
//! is_test    := comparison (IS [NOT] NULL)*
//! comparison := membership [('=' | '<>' | '!=' | '<' | '<=' | '>' | '>=') membership]
//! membership := concat [[NOT] IN '(' expr (',' expr)* ')']
//! concat     := additive ('||' additive)*
//! additive   := term (('+' | '-') term)*
//! term       := power (('*' | '/' | '%') power)*
//! power      := unary ('^' unary)*
//! unary      := '-' unary | cast
//! cast       := primary ('::' type)*
//! primary    := literal | '$' n | var | var '.' property | '(' expr ')'
//!             | CASE [expr] (WHEN expr THEN expr)+ [ELSE expr] END
//!             | CAST '(' expr AS type ')' | COALESCE '(' expr, ... ')'
//!             | NULLIF '(' expr ',' expr ')' | function '(' expr, ... ')'
//!             | SUBSTRING '(' expr FROM expr [FOR expr] ')'
//!             | COUNT '(' '*' ')' | aggregate '(' [DISTINCT] expr ')'
//!             | graph_function '(' expr ')' | '[' [expr (',' expr)*] ']'
//! literal    := number | string | TRUE | FALSE | NULL
//! ```
//!
//! Binary operators associate to the left (`2 ^ 3 ^ 2` is `(2 ^ 3) ^ 2`, as
//! in PostgreSQL), a comparison does not chain (`a = b = c` is a syntax
//! error), and `::` binds tighter than a unary minus (`-x::int` is
//! `-(x::int)`). A minus written before a number is part of the literal.
//! A type is spelled as `CREATE TABLE` spells a column type
//! (`Parser::column_type`), so a cast and a declaration cannot disagree on a
//! name. This is NOT the SQL predicate grammar of `parser/expr.rs`, which is
//! shaped by what an index answers; a GQL expression is evaluated over a
//! binding row.

use super::super::ast::{AggFunc, ArithOp, CastType, Expr, Func, GraphFunc, Literal};
use crate::ast::CmpOp;
use crate::lexer::Tok;
use crate::parser::Parser;
use crate::refuse;
use crate::sqlstate::UNDEFINED_FUNCTION;
use crate::{SqlError, SqlResult2};
use sekejap_core::Kind;

/// SQL host functions that are not `ST_*`: the full-text ones. Inside a GQL
/// body they are refused as M6's, not reported as unknown.
const TEXT_FUNCTIONS: &[&str] = &["TO_TSVECTOR", "TO_TSQUERY", "BM25"];

impl Parser {
    pub(super) fn gql_expr(&mut self) -> SqlResult2<Expr> {
        self.deeper()?;
        let result = self.gql_or();
        self.shallower();
        result
    }

    fn gql_or(&mut self) -> SqlResult2<Expr> {
        let mut left = self.gql_and()?;
        while self.eat_word("OR") {
            left = Expr::Or(Box::new(left), Box::new(self.gql_and()?));
        }
        Ok(left)
    }

    fn gql_and(&mut self) -> SqlResult2<Expr> {
        let mut left = self.gql_not()?;
        while self.eat_word("AND") {
            left = Expr::And(Box::new(left), Box::new(self.gql_not()?));
        }
        Ok(left)
    }

    fn gql_not(&mut self) -> SqlResult2<Expr> {
        if self.eat_word("NOT") {
            self.deeper()?;
            let inner = self.gql_not();
            self.shallower();
            return Ok(Expr::Not(Box::new(inner?)));
        }
        self.gql_is_test()
    }

    fn gql_is_test(&mut self) -> SqlResult2<Expr> {
        let mut expr = self.gql_comparison()?;
        while self.word().as_deref() == Some("IS") {
            let negated = self.word_at(1).as_deref() == Some("NOT");
            let tested = if negated { 2 } else { 1 };
            match self.word_at(tested).as_deref() {
                Some("NULL") => {}
                Some("MISSING") => {
                    return Err(SqlError::unsupported(
                        "`IS MISSING` is SQL's: inside a GQL body a missing property reads as NULL, so `IS NULL` is true for it; PROPERTY_NAMES(x) tells a missing property from a stored null",
                    ))
                }
                _ => {
                    return Err(SqlError::syntax(
                        "`IS` after a value is `IS [NOT] NULL`",
                        self.here(),
                    ))
                }
            }
            for _ in 0..=tested {
                self.bump();
            }
            expr = Expr::IsNull {
                expr: Box::new(expr),
                negated,
            };
        }
        Ok(expr)
    }

    fn gql_comparison(&mut self) -> SqlResult2<Expr> {
        let left = self.gql_membership()?;
        let Some(op) = self.gql_cmp_op() else {
            return Ok(left);
        };
        self.bump();
        let right = self.gql_membership()?;
        if self.gql_cmp_op().is_some() {
            return Err(SqlError::syntax(
                "comparisons do not chain: write `a = b AND b = c`",
                self.here(),
            ));
        }
        Ok(Expr::Compare {
            op,
            left: Box::new(left),
            right: Box::new(right),
        })
    }

    fn gql_cmp_op(&self) -> Option<CmpOp> {
        Some(match self.peek() {
            Tok::Eq => CmpOp::Eq,
            Tok::Ne => CmpOp::Ne,
            Tok::Lt => CmpOp::Lt,
            Tok::Le => CmpOp::Le,
            Tok::Gt => CmpOp::Gt,
            Tok::Ge => CmpOp::Ge,
            _ => return None,
        })
    }

    fn gql_membership(&mut self) -> SqlResult2<Expr> {
        let expr = self.gql_concat()?;
        let negated =
            self.word().as_deref() == Some("NOT") && self.word_at(1).as_deref() == Some("IN");
        if !negated && self.word().as_deref() != Some("IN") {
            return Ok(expr);
        }
        if negated {
            self.bump();
        }
        self.bump();
        if !matches!(self.peek(), Tok::LParen) {
            return Ok(Expr::Member {
                expr: Box::new(expr),
                list: Box::new(self.gql_concat()?),
                negated,
            });
        }
        self.expect(&Tok::LParen)?;
        Ok(Expr::In {
            expr: Box::new(expr),
            list: self.gql_arguments()?,
            negated,
        })
    }

    /// `expr, expr, ... )`, the cursor after the `(`.
    fn gql_arguments(&mut self) -> SqlResult2<Vec<Expr>> {
        let mut args = vec![self.gql_expr()?];
        while self.eat(&Tok::Comma) {
            args.push(self.gql_expr()?);
        }
        self.expect(&Tok::RParen)?;
        Ok(args)
    }

    fn gql_concat(&mut self) -> SqlResult2<Expr> {
        let mut left = self.gql_additive()?;
        while self.eat(&Tok::Concat) {
            left = Expr::Concat(Box::new(left), Box::new(self.gql_additive()?));
        }
        Ok(left)
    }

    /// One level of left-associative arithmetic operators over `next`.
    fn gql_arith(
        &mut self,
        ops: &[(Tok, ArithOp)],
        next: fn(&mut Self) -> SqlResult2<Expr>,
    ) -> SqlResult2<Expr> {
        let mut left = next(self)?;
        while let Some(op) = ops
            .iter()
            .find(|(tok, _)| tok == self.peek())
            .map(|(_, op)| *op)
        {
            self.bump();
            left = Expr::Arith {
                op,
                left: Box::new(left),
                right: Box::new(next(self)?),
            };
        }
        Ok(left)
    }

    fn gql_additive(&mut self) -> SqlResult2<Expr> {
        self.gql_arith(
            &[(Tok::Plus, ArithOp::Add), (Tok::Minus, ArithOp::Sub)],
            Self::gql_term,
        )
    }

    fn gql_term(&mut self) -> SqlResult2<Expr> {
        self.gql_arith(
            &[
                (Tok::Star, ArithOp::Mul),
                (Tok::Slash, ArithOp::Div),
                (Tok::Percent, ArithOp::Mod),
            ],
            Self::gql_power,
        )
    }

    fn gql_power(&mut self) -> SqlResult2<Expr> {
        self.gql_arith(&[(Tok::Caret, ArithOp::Pow)], Self::gql_unary)
    }

    fn gql_unary(&mut self) -> SqlResult2<Expr> {
        if !self.eat(&Tok::Minus) {
            return self.gql_cast();
        }
        self.deeper()?;
        let inner = self.gql_unary();
        self.shallower();
        Ok(match inner? {
            Expr::Literal(Literal::Num(value, exact)) => {
                Expr::Literal(Literal::Num(-value, exact))
            }
            inner => Expr::Neg(Box::new(inner)),
        })
    }

    fn gql_cast(&mut self) -> SqlResult2<Expr> {
        let mut expr = self.gql_primary()?;
        while self.eat(&Tok::Cast) {
            expr = Expr::Cast {
                expr: Box::new(expr),
                to: self.gql_cast_type()?,
            };
        }
        Ok(expr)
    }

    /// A type name, the cursor on it.
    fn gql_cast_type(&mut self) -> SqlResult2<CastType> {
        // `::vector` needs no dimension in a cast, which `column_type` asks
        // for; every spatial and vector type is M6's either way.
        if matches!(
            self.word().as_deref(),
            Some("VECTOR" | "GEOMETRY" | "GEOGRAPHY")
        ) {
            return Err(refuse::gql_refuse("host function"));
        }
        let (kind, declared) = self.column_type()?;
        Ok(match kind {
            Kind::Text => CastType::Text,
            Kind::Int if declared == "DATE" => CastType::Date,
            Kind::Int if declared == "TIMESTAMPTZ" => CastType::Timestamp,
            Kind::Int => CastType::Int,
            Kind::Real => CastType::Float,
            Kind::Bool => CastType::Bool,
            Kind::Json => CastType::Json,
            // A geometry or a vector.
            _ => return Err(refuse::gql_refuse("host function")),
        })
    }

    fn gql_primary(&mut self) -> SqlResult2<Expr> {
        let at = self.here();
        match self.peek().clone() {
            Tok::Num(value, exact) => {
                self.bump();
                Ok(Expr::Literal(Literal::Num(value, exact)))
            }
            Tok::Str(text) => {
                self.bump();
                Ok(Expr::Literal(Literal::Str(text)))
            }
            Tok::Param(n) => {
                self.bump();
                Ok(Expr::Param(n))
            }
            Tok::LParen => {
                self.bump();
                let inner = self.gql_expr()?;
                self.expect(&Tok::RParen)?;
                Ok(inner)
            }
            Tok::LBracket => {
                self.bump();
                let mut items = Vec::new();
                if !self.eat(&Tok::RBracket) {
                    items.push(self.gql_expr()?);
                    while self.eat(&Tok::Comma) {
                        items.push(self.gql_expr()?);
                    }
                    self.expect(&Tok::RBracket)?;
                }
                Ok(Expr::List(items))
            }
            Tok::Word(word) => {
                let upper = word.to_ascii_uppercase();
                let literal = match upper.as_str() {
                    "TRUE" => Literal::Bool(true),
                    "FALSE" => Literal::Bool(false),
                    "NULL" => Literal::Null,
                    "CASE" => {
                        self.bump();
                        return self.gql_case();
                    }
                    "EXISTS" => return Err(refuse::gql_refuse("EXISTS")),
                    // `name(`: a function of the pack, or a construct the GQL
                    // table names (`count(`, `nodes(`). A word WITHOUT a `(`
                    // is an ordinary variable name.
                    _ if matches!(self.peek_at(1), Tok::LParen) => {
                        return self.gql_call(&word, &upper)
                    }
                    _ => return self.gql_reference(),
                };
                self.bump();
                Ok(Expr::Literal(literal))
            }
            Tok::Quoted(_) => self.gql_reference(),
            other => {
                self.guard_here()?;
                Err(SqlError::syntax(
                    format!("expected an expression, found `{}`", other.written()),
                    at,
                ))
            }
        }
    }

    /// `name ( ... )`, the cursor on the name and `(` after it.
    fn gql_call(&mut self, word: &str, upper: &str) -> SqlResult2<Expr> {
        match upper {
            "CAST" => {
                self.bump();
                self.bump();
                let expr = self.gql_expr()?;
                self.expect_word("AS")?;
                let to = self.gql_cast_type()?;
                self.expect(&Tok::RParen)?;
                return Ok(Expr::Cast {
                    expr: Box::new(expr),
                    to,
                });
            }
            "COALESCE" => {
                self.bump();
                self.bump();
                return Ok(Expr::Coalesce(self.gql_arguments()?));
            }
            "NULLIF" => {
                self.bump();
                self.bump();
                let left = self.gql_expr()?;
                self.expect(&Tok::Comma)?;
                let right = self.gql_expr()?;
                self.expect(&Tok::RParen)?;
                return Ok(Expr::Nullif(Box::new(left), Box::new(right)));
            }
            _ => {}
        }
        if let Some(func) = AggFunc::named(upper) {
            return self.gql_aggregate(func);
        }
        if let Some(func) = GraphFunc::named(upper) {
            self.bump();
            self.bump();
            let arg = self.gql_expr()?;
            if self.eat(&Tok::Comma) {
                return Err(SqlError::coded(UNDEFINED_FUNCTION, format!(
                    "{} takes one argument",
                    func.written()
                )));
            }
            self.expect(&Tok::RParen)?;
            return Ok(Expr::Graph {
                func,
                arg: Box::new(arg),
            });
        }
        let Some(func) = Func::named(upper) else {
            if let Some(refusal) = self.gql_listed(upper) {
                return Err(refusal);
            }
            if upper.starts_with("ST_") || TEXT_FUNCTIONS.contains(&upper) {
                return Err(refuse::gql_refuse("host function"));
            }
            let scalars: Vec<&str> = Func::ALL.iter().map(|f| f.written()).collect();
            return Err(SqlError::coded(UNDEFINED_FUNCTION, format!(
                "function `{word}` is not in the GQL expression pack, which holds {}, COALESCE, NULLIF, CAST, the aggregates {}, and the graph functions {}",
                scalars.join(", "),
                listed(AggFunc::ALL.iter().map(|f| f.written())),
                listed(GraphFunc::ALL.iter().map(|f| f.written())),
            )));
        };
        self.bump();
        self.bump();
        let mut args = vec![self.gql_expr()?];
        if func == Func::Substring && self.eat_word("FROM") {
            // `substring(s FROM start [FOR count])` is the same call as
            // `substring(s, start [, count])`, as in SQL.
            args.push(self.gql_expr()?);
            if self.eat_word("FOR") {
                args.push(self.gql_expr()?);
            }
            self.expect(&Tok::RParen)?;
        } else if self.eat(&Tok::Comma) {
            args.extend(self.gql_arguments()?);
        } else {
            self.expect(&Tok::RParen)?;
        }
        let (fewest, most) = func.arity();
        if args.len() < fewest || args.len() > most {
            let takes = match (fewest, most) {
                (fewest, most) if fewest == most => fewest.to_string(),
                (fewest, usize::MAX) => format!("{fewest} or more"),
                (fewest, most) => format!("{fewest} to {most}"),
            };
            return Err(SqlError::coded(UNDEFINED_FUNCTION, format!(
                "{} takes {takes} arguments, not {}",
                func.written(),
                args.len()
            )));
        }
        Ok(Expr::Call { func, args })
    }

    /// `COUNT(*)`, `COUNT([DISTINCT] x)` and the other vertical
    /// aggregates, the cursor on the name. Only `COUNT` counts rows (`*`),
    /// and only `COUNT` takes `DISTINCT` (the P0 pack).
    fn gql_aggregate(&mut self, func: AggFunc) -> SqlResult2<Expr> {
        self.bump();
        self.bump();
        if func == AggFunc::Count && self.eat(&Tok::Star) {
            self.expect(&Tok::RParen)?;
            return Ok(Expr::Aggregate {
                func,
                distinct: false,
                arg: None,
            });
        }
        let distinct = self.word().as_deref() == Some("DISTINCT");
        if distinct {
            if func != AggFunc::Count {
                return Err(SqlError::unsupported(format!(
                    "{}(DISTINCT ...) is not in the aggregate pack; COUNT(DISTINCT x) is",
                    func.written()
                )));
            }
            self.bump();
        }
        let arg = self.gql_expr()?;
        self.expect(&Tok::RParen)?;
        Ok(Expr::Aggregate {
            func,
            distinct,
            arg: Some(Box::new(arg)),
        })
    }

    /// `CASE ... END`, the cursor after `CASE`.
    fn gql_case(&mut self) -> SqlResult2<Expr> {
        let operand = if self.word().as_deref() == Some("WHEN") {
            None
        } else {
            Some(Box::new(self.gql_expr()?))
        };
        let mut branches = Vec::new();
        while self.eat_word("WHEN") {
            let when = self.gql_expr()?;
            self.expect_word("THEN")?;
            branches.push((when, self.gql_expr()?));
        }
        if branches.is_empty() {
            return Err(self.gql_expected("`WHEN`"));
        }
        let otherwise = if self.eat_word("ELSE") {
            Some(Box::new(self.gql_expr()?))
        } else {
            None
        };
        self.expect_word("END")?;
        Ok(Expr::Case {
            operand,
            branches,
            otherwise,
        })
    }

    /// `var` or `var.property`, the cursor on the variable. In an outer
    /// `SELECT`, `alias.column` is the relation's column.
    fn gql_reference(&mut self) -> SqlResult2<Expr> {
        let var = self.gql_name("a variable")?;
        if !self.eat(&Tok::Dot) {
            return Ok(Expr::Var(var));
        }
        if self.relation.as_ref() == Some(&var) {
            return Ok(Expr::Var(self.gql_name("a column of the relation")?));
        }
        let property = self.gql_spelled("a property name")?;
        if matches!(self.peek(), Tok::Dot) {
            return Err(SqlError::syntax(
                "a property reference is `variable.property`, one level",
                self.here(),
            ));
        }
        Ok(Expr::Property { var, property })
    }
}

/// `A, B and C`.
fn listed<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let names: Vec<&str> = names.collect();
    let (last, rest) = names.split_last().expect("a table lists at least two");
    format!("{} and {last}", rest.join(", "))
}
