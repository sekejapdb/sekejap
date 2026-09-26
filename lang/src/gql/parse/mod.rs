//! The GQL body parser (`docs/lang/GQL_PROFILE_DESIGN.md` §5).
//!
//! It runs on the SQL parser's cursor, over the same tokens, with the
//! parser's `dialect` set to [`Dialect::Gql`] for exactly the length of the
//! body. Inside, a word is looked up in `refuse::GQL_TABLE` and never in the
//! SQL table, and a word is a keyword only where the GQL grammar puts one
//! (brief §5: keywords by position, not reserved globally). Outside, nothing
//! changes: the graph name before the body and the alias after it are read
//! as SQL reads them.
//!
//! * `stage.rs`: the statements and the `RETURN` of a stage; and the outer
//!   `SELECT` over the relation, which is read as one more `RETURN`
//!   (design §5.5). `NEXT` separates the parts of a body and `UNION` the
//!   stages of one part, both read here.
//! * `pattern.rs`: path patterns, element patterns and labels.
//! * `expr.rs`: expressions: the M2 subset and the M3-C scalar pack.
//!
//! What the profile builds later is refused by name with the milestone that
//! builds it; what it does not know at all is a syntax error naming the
//! place.

mod expr;
mod pattern;
mod stage;
#[cfg(test)]
mod tests;

use super::ast::{BodyPart, GqlGraphTable, Pipeline};
use super::schema::Name;
use crate::lexer::Tok;
use crate::parser::{Dialect, Parser};
use crate::refuse;
use crate::{SqlError, SqlResult2};

/// Words that end a GQL construct and follow it, and so are never read as a
/// name where one is optional (`(a IS t)`: `IS` is not the variable).
const STRUCTURAL: &[&str] = &["IS", "WHERE", "COST", "AS"];

/// Words that may follow `GRAPH_TABLE (...)` in a `FROM`, and so are never
/// read as the relation's alias.
const RELATION_FOLLOWERS: &[&str] = &[
    "WHERE", "GROUP", "HAVING", "ORDER", "LIMIT", "OFFSET", "UNION", "INTERSECT", "EXCEPT",
    "JOIN", "INNER", "LEFT", "RIGHT", "FULL", "CROSS", "NATURAL", "WINDOW",
];

impl Parser {
    /// `GRAPH_TABLE ( <graph> <body> ) [AS <alias>]`, the cursor on
    /// `GRAPH_TABLE`.
    pub(crate) fn gql_graph_table(&mut self) -> SqlResult2<GqlGraphTable> {
        self.expect_word("GRAPH_TABLE")?;
        self.expect(&Tok::LParen)?;
        let graph = self.name()?;
        let outer = std::mem::replace(&mut self.dialect, Dialect::Gql);
        // The closing `)` is read in the GQL dialect too: a word standing
        // where it should be (`LIMIT`, `NEXT`, `UNION`) is refused as the
        // GQL construct it is.
        let body = self.reject_columns_body().and_then(|()| self.gql_pipeline()).and_then(|stages| {
            self.expect(&Tok::RParen)?;
            Ok(stages)
        });
        self.dialect = outer;
        let body = body?;
        // `[AS] alias`, as SQL names a relation in `FROM`.
        let alias = if self.eat_word("AS") {
            Some(self.gql_name("the relation's alias")?)
        } else if matches!(self.peek(), Tok::Quoted(_))
            || self.word().is_some_and(|word| !RELATION_FOLLOWERS.contains(&word.as_str()))
        {
            Some(self.gql_name("the relation's alias")?)
        } else {
            None
        };
        Ok(GqlGraphTable {
            graph,
            body: Pipeline { parts: body },
            alias,
            outer: None,
        })
    }

    /// `part (NEXT part)*`.
    fn gql_pipeline(&mut self) -> SqlResult2<Vec<BodyPart>> {
        let mut parts = vec![self.gql_part()?];
        while self.eat_word("NEXT") {
            parts.push(self.gql_part()?);
        }
        Ok(parts)
    }

    /// `part := stage ((UNION [ALL | DISTINCT]) stage)*` (design §2.5):
    /// `UNION` binds tighter than `NEXT`, and a branch is one stage. One
    /// chain takes ONE conjunction -- `UNION` and `UNION DISTINCT` are the
    /// same one -- and mixing it with `UNION ALL` is refused by name
    /// (`mixed UNION`, P1, design Q24): ISO's parenthesised composite form,
    /// which would say which applies first, is not built.
    fn gql_part(&mut self) -> SqlResult2<BodyPart> {
        let mut branches = vec![self.gql_stage()?];
        let mut conjunction: Option<bool> = None;
        while self.eat_word("UNION") {
            let all = self.eat_word("ALL");
            if !all {
                self.eat_word("DISTINCT");
            }
            if conjunction.is_some_and(|chain| chain != all) {
                return Err(refuse::gql_refuse("mixed UNION"));
            }
            conjunction = Some(all);
            branches.push(self.gql_stage()?);
        }
        Ok(match conjunction {
            None => BodyPart::Stage(branches.pop().expect("a part has a stage")),
            Some(all) => BodyPart::Union { branches, all },
        })
    }

    /// A `COLUMNS (...)` body: the removed SQL/PGQ form (owner decision 1,
    /// `docs/lang/GQL_PROFILE_DESIGN.md`). Caught here, at the top level of
    /// the parentheses, before the GQL grammar meets the word and calls it
    /// something else (a variable, say) -- so a body that wrote the old
    /// projection is refused by name, naming `RETURN` as the replacement,
    /// rather than ending as a syntax error at whatever byte the GQL grammar
    /// happened to choke on. Mirrors the bounded-depth scan M2-A used to
    /// choose between the two bodies, now used only to reject one of them.
    /// Only the projection shape counts -- `COLUMNS (` at the top level, not
    /// after a `.` -- so a variable or property called `columns` stays
    /// ordinary GQL.
    fn reject_columns_body(&self) -> SqlResult2<()> {
        let mut depth = 0usize;
        let mut after_dot = false;
        for ahead in 0.. {
            let token = self.peek_at(ahead);
            match token {
                Tok::LParen | Tok::LBracket | Tok::LBrace => depth += 1,
                Tok::RParen | Tok::RBracket | Tok::RBrace if depth == 0 => break,
                Tok::RParen | Tok::RBracket | Tok::RBrace => depth -= 1,
                Tok::Eof => break,
                _ if depth == 0
                    && !after_dot
                    && token.keyword().as_deref() == Some("COLUMNS")
                    && matches!(self.peek_at(ahead + 1), Tok::LParen) =>
                {
                    return Err(refuse::gql_refuse("COLUMNS"));
                }
                _ => {}
            }
            after_dot = matches!(token, Tok::Dot);
        }
        Ok(())
    }

    /// The GQL refusal the word at the cursor carries, two-word forms
    /// first so `ALL SHORTEST` is refused as itself.
    ///
    /// `OPTIONAL` not followed by `MATCH` is ISO's block form, `OPTIONAL {
    /// MATCH ...; MATCH ... }` (design Q17): the comma form already matches
    /// every pattern optional together (M5-A), so the block form is a named
    /// P1 row rather than a bare syntax error at the `{`.
    pub(crate) fn gql_listed(&self, word: &str) -> Option<SqlError> {
        for (first, second) in [("ALL", "SHORTEST")] {
            if word == first && self.word_at(1).as_deref() == Some(second) {
                return Some(refuse::gql_refuse(&format!("{first} {second}")));
            }
        }
        if word == "OPTIONAL" && matches!(self.peek_at(1), Tok::LBrace) {
            return Some(refuse::gql_refuse("OPTIONAL block"));
        }
        if word == "OPTIONAL" && self.word_at(1).as_deref() == Some("CALL") {
            return Some(refuse::gql_refuse("OPTIONAL CALL"));
        }
        refuse::gql_lookup(word)
    }

    /// A variable or alias name: a bare word, folded, or a quoted one, kept.
    fn gql_name(&mut self, what: &str) -> SqlResult2<Name> {
        let name = match self.peek() {
            Tok::Word(word) => Name::unquoted(word),
            Tok::Quoted(word) => Name::quoted(word),
            _ => return Err(self.gql_expected(what)),
        };
        self.bump();
        Ok(name)
    }

    /// An optional element variable: a word that is not one of the words
    /// that follow it.
    fn gql_optional_var(&mut self) -> SqlResult2<Option<Name>> {
        let is_var = match self.peek() {
            Tok::Quoted(_) => true,
            Tok::Word(_) => !STRUCTURAL.contains(&self.word().unwrap_or_default().as_str()),
            _ => false,
        };
        if is_var {
            return self.gql_name("a variable").map(Some);
        }
        Ok(None)
    }

    /// A label or a property: spelled as written, as SQL keeps a name.
    fn gql_spelled(&mut self, what: &str) -> SqlResult2<String> {
        match self.peek().clone() {
            Tok::Word(word) | Tok::Quoted(word) => {
                self.bump();
                Ok(word)
            }
            _ => Err(self.gql_expected(what)),
        }
    }

    /// A syntax error at the cursor, after the GQL table has had its say.
    fn gql_expected(&self, what: &str) -> SqlError {
        if let Err(refusal) = self.guard_here() {
            return refusal;
        }
        SqlError::syntax(
            format!("expected {what}, found `{}`", self.peek().written()),
            self.here(),
        )
    }
}
