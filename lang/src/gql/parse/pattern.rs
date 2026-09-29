//! Path patterns, element patterns, quantifiers and label expressions.
//!
//! ```text
//! pattern    := [var '='] [prefix] factor+
//! prefix     := WALK | TRAIL | ACYCLIC | ANY [SHORTEST | CHEAPEST]
//! factor     := node | edge [quantifier] | '(' node (edge node)* ')' quantifier
//! quantifier := '{' int '}' | '{' int ',' [int] '}' | '?' | '*' | '+'
//! node       := '(' [var] [(IS | ':') label] [WHERE expr] ')'
//! edge       := '-' '[' filler ']' '->' | '<-' '[' filler ']' '-' | '-' '[' filler ']' '-'
//!             | '->' | '<-' | '-'                -- abbreviated: no variable, no label
//! filler     := [var] [(IS | ':') label] [WHERE expr] [COST expr]
//! label      := name ('|' name)*
//! ```
//!
//! An edge stands between two node positions -- a node, or the end of a
//! parenthesised subpath -- and two node patterns touch only when one of
//! them is a subpath (the concatenation of §4.1). A quantifier's bounds are
//! integer literals (brief §7). The path variable is written before the
//! prefix, and the prefix is ONE path mode or ONE selector (§4.3): the two
//! together are refused by name, as is a quantifier inside a quantified
//! subpath (Q10).

use super::super::ast::{
    EdgeDirection, EdgePattern, LabelExpr, NodePattern, PathElement, PathPattern, PathPrefix,
    Quantifier,
};
use crate::lexer::Tok;
use crate::parser::Parser;
use crate::refuse;
use crate::{SqlError, SqlResult2};

impl Parser {
    pub(super) fn gql_path_pattern(&mut self) -> SqlResult2<PathPattern> {
        let name = if self.at_path_variable() {
            let name = self.gql_name("a path variable")?;
            self.expect(&Tok::Eq)?;
            Some(name)
        } else {
            None
        };
        let prefix = self.gql_path_prefix();
        if let Some(first) = prefix {
            if self.at_path_variable() {
                return Err(SqlError::syntax(
                    format!(
                        "the path variable goes before the selector or path mode: write `p = {} (...)`",
                        first.written()
                    ),
                    self.here(),
                ));
            }
            if let Some(second) = self.gql_path_prefix() {
                if first.is_selector() != second.is_selector() {
                    return Err(refuse::gql_refuse("selector with a path mode"));
                }
                return Err(SqlError::syntax(
                    format!(
                        "`{}` after `{}`: a path pattern takes one path mode or one selector",
                        second.written(),
                        first.written()
                    ),
                    self.here(),
                ));
            }
        }
        let elements = self.gql_path_elements()?;
        Ok(PathPattern {
            name,
            prefix,
            elements,
        })
    }

    /// `p =` at the cursor.
    fn at_path_variable(&self) -> bool {
        matches!(self.peek(), Tok::Word(_) | Tok::Quoted(_)) && matches!(self.peek_at(1), Tok::Eq)
    }

    /// A path mode or a selector, if one is written: words recognised here
    /// only, so each stays a legal name everywhere else.
    fn gql_path_prefix(&mut self) -> Option<PathPrefix> {
        let prefix = match self.word().as_deref() {
            Some("WALK") => PathPrefix::Walk,
            Some("TRAIL") => PathPrefix::Trail,
            Some("ACYCLIC") => PathPrefix::Acyclic,
            Some("ANY") => match self.word_at(1).as_deref() {
                Some("SHORTEST") => PathPrefix::AnyShortest,
                Some("CHEAPEST") => PathPrefix::AnyCheapest,
                _ => PathPrefix::Any,
            },
            _ => return None,
        };
        self.bump();
        if matches!(prefix, PathPrefix::AnyShortest | PathPrefix::AnyCheapest) {
            self.bump();
        }
        Some(prefix)
    }

    /// The factors of a top-level pattern.
    fn gql_path_elements(&mut self) -> SqlResult2<Vec<PathElement>> {
        // A path mode or a selector word the profile does not build stands
        // here (`MATCH SIMPLE (`); the GQL table names it before a `(` is
        // demanded.
        if !matches!(self.peek(), Tok::LParen) {
            return Err(self.gql_expected("`(` to begin a path pattern"));
        }
        let mut elements: Vec<PathElement> = Vec::new();
        loop {
            match self.peek() {
                Tok::LParen => {
                    let group = matches!(self.peek_at(1), Tok::LParen);
                    if !group && matches!(elements.last(), Some(PathElement::Node(_))) {
                        return Err(self.gql_expected("an edge pattern between two node patterns"));
                    }
                    elements.push(if group {
                        self.gql_group()?
                    } else {
                        PathElement::Node(self.gql_node()?)
                    });
                }
                Tok::Arrow | Tok::BackArrow | Tok::Minus => {
                    let mut edge = self.gql_edge()?;
                    edge.quantifier = self.gql_quantifier()?;
                    elements.push(PathElement::Edge(edge));
                    if !matches!(self.peek(), Tok::LParen) {
                        return Err(
                            self.gql_expected("a node pattern `(...)` after an edge pattern")
                        );
                    }
                }
                _ => return Ok(elements),
            }
        }
    }

    /// `( node (edge node)* ) quantifier`, the cursor on the outer `(`.
    fn gql_group(&mut self) -> SqlResult2<PathElement> {
        self.expect(&Tok::LParen)?;
        if matches!(self.peek_at(1), Tok::LParen) {
            // A subpath inside a subpath: its quantifier would nest.
            return Err(refuse::gql_refuse("nested quantifier"));
        }
        let mut elements = vec![PathElement::Node(self.gql_node()?)];
        while matches!(self.peek(), Tok::Arrow | Tok::BackArrow | Tok::Minus) {
            let edge = self.gql_edge()?;
            if self.gql_quantifier()?.is_some() {
                return Err(refuse::gql_refuse("nested quantifier"));
            }
            elements.push(PathElement::Edge(edge));
            if !matches!(self.peek(), Tok::LParen) {
                return Err(self.gql_expected("a node pattern `(...)` after an edge pattern"));
            }
            if matches!(self.peek_at(1), Tok::LParen) {
                return Err(refuse::gql_refuse("nested quantifier"));
            }
            elements.push(PathElement::Node(self.gql_node()?));
        }
        self.expect(&Tok::RParen)?;
        let Some(quantifier) = self.gql_quantifier()? else {
            return Err(self.gql_expected(
                "a quantifier (`{m,n}`, `{n}`, `{m,}`, `?`, `*`, `+`) after a parenthesized path pattern",
            ));
        };
        Ok(PathElement::Group {
            elements,
            quantifier,
        })
    }

    /// A quantifier, if one is written at the cursor.
    fn gql_quantifier(&mut self) -> SqlResult2<Option<Quantifier>> {
        let quantifier = match self.peek() {
            Tok::Star => Quantifier { lo: 0, hi: None },
            Tok::Plus => Quantifier { lo: 1, hi: None },
            Tok::Question => Quantifier { lo: 0, hi: Some(1) },
            Tok::LBrace => {
                self.bump();
                let lo = self.gql_bound()?;
                let hi = if self.eat(&Tok::Comma) {
                    if matches!(self.peek(), Tok::RBrace) {
                        None
                    } else {
                        Some(self.gql_bound()?)
                    }
                } else {
                    Some(lo)
                };
                if let Some(hi) = hi {
                    if hi < lo {
                        return Err(SqlError::syntax(
                            format!(
                                "the quantifier's upper bound {hi} is below its lower bound {lo}"
                            ),
                            self.here(),
                        ));
                    }
                }
                self.expect(&Tok::RBrace)?;
                return Ok(Some(Quantifier { lo, hi }));
            }
            _ => return Ok(None),
        };
        self.bump();
        Ok(Some(quantifier))
    }

    /// One quantifier bound: an integer literal (brief §7).
    fn gql_bound(&mut self) -> SqlResult2<u32> {
        match *self.peek() {
            Tok::Num(value, Some(_)) if value >= 0.0 && value <= f64::from(u32::MAX) => {
                self.bump();
                Ok(value as u32)
            }
            Tok::Param(n) => Err(SqlError::syntax(
                format!(
                    "a quantifier bound is an integer literal, and `${n}` is a parameter: the path automaton is fixed when the statement compiles (GQL profile, brief §7)"
                ),
                self.here(),
            )),
            _ => Err(SqlError::syntax(
                format!(
                    "a quantifier bound is an integer literal from 0 to {}, found `{}`",
                    u32::MAX,
                    self.peek().written()
                ),
                self.here(),
            )),
        }
    }

    fn gql_node(&mut self) -> SqlResult2<NodePattern> {
        self.expect(&Tok::LParen)?;
        let var = self.gql_optional_var()?;
        let label = self.gql_label()?;
        let where_ = if self.eat_word("WHERE") {
            Some(self.gql_expr()?)
        } else {
            None
        };
        self.expect(&Tok::RParen)?;
        if matches!(
            self.peek(),
            Tok::LBrace | Tok::Star | Tok::Plus | Tok::Question
        ) {
            return Err(SqlError::syntax(
                "a quantifier follows an edge pattern or a parenthesized path pattern, not a node pattern",
                self.here(),
            ));
        }
        Ok(NodePattern { var, label, where_ })
    }

    /// An edge pattern, the cursor on `-`, `->` or `<-`.
    fn gql_edge(&mut self) -> SqlResult2<EdgePattern> {
        let abbreviated = |direction| EdgePattern {
            var: None,
            label: None,
            where_: None,
            cost: None,
            direction,
            quantifier: None,
        };
        match self.bump() {
            Tok::Arrow => Ok(abbreviated(EdgeDirection::Right)),
            Tok::BackArrow => {
                if !self.eat(&Tok::LBracket) {
                    return Ok(abbreviated(EdgeDirection::Left));
                }
                let edge = self.gql_edge_filler(EdgeDirection::Left)?;
                self.expect(&Tok::Minus)?;
                Ok(edge)
            }
            // `-`: the undirected abbreviation, or the opening of `-[..]->`
            // or `-[..]-`.
            _ => {
                if !self.eat(&Tok::LBracket) {
                    return Ok(abbreviated(EdgeDirection::Any));
                }
                let mut edge = self.gql_edge_filler(EdgeDirection::Any)?;
                if self.eat(&Tok::Arrow) {
                    edge.direction = EdgeDirection::Right;
                } else {
                    self.expect(&Tok::Minus)?;
                }
                Ok(edge)
            }
        }
    }

    /// `[ [var] [IS|: label] [WHERE expr] [COST expr] ]`, the `[` read,
    /// through the `]`.
    fn gql_edge_filler(&mut self, direction: EdgeDirection) -> SqlResult2<EdgePattern> {
        let var = self.gql_optional_var()?;
        let label = self.gql_label()?;
        let where_ = if self.eat_word("WHERE") {
            Some(self.gql_expr()?)
        } else {
            None
        };
        let cost = if self.eat_word("COST") {
            Some(self.gql_expr()?)
        } else {
            None
        };
        self.expect(&Tok::RBracket)?;
        Ok(EdgePattern {
            var,
            label,
            where_,
            cost,
            direction,
            quantifier: None,
        })
    }

    /// `IS <label>` or `:<label>`, if written. `A|B|C` is an alternation.
    fn gql_label(&mut self) -> SqlResult2<Option<LabelExpr>> {
        if !(self.eat(&Tok::Colon) || self.eat_word("IS")) {
            return Ok(None);
        }
        let mut options = vec![self.gql_label_name()?];
        while self.eat(&Tok::Pipe) {
            options.push(self.gql_label_name()?);
        }
        if matches!(self.peek(), Tok::Amp | Tok::Overlaps) {
            return Err(refuse::gql_refuse("label conjunction"));
        }
        Ok(Some(if options.len() == 1 {
            options.pop().expect("one label")
        } else {
            LabelExpr::Or(options)
        }))
    }

    fn gql_label_name(&mut self) -> SqlResult2<LabelExpr> {
        match self.peek() {
            Tok::Bang => Err(refuse::gql_refuse("label negation")),
            Tok::Percent => Err(refuse::gql_refuse("label wildcard")),
            _ => {
                let name = self.gql_spelled("a label")?;
                if matches!(self.peek(), Tok::Dot) {
                    return Err(SqlError::unsupported(format!(
                        "`{name}.`: a label is not schema-qualified; the schema belongs in the graph's definition, which gives the table its label (in the base graph, quote the table: \"{name}.table\")"
                    )));
                }
                Ok(LabelExpr::Name(name))
            }
        }
    }
}
