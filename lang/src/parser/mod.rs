//! A hand-written recursive-descent parser for the Tier-1 grammar, and for
//! nothing else.
//!
//! The rule it follows throughout: a construct `docs/lang/QL_CONTRACT.md` puts in
//! Tier 2 or Tier 3 is RECOGNISED and then REFUSED with that tier's reason
//! (`refuse.rs`), never skipped and never approximated. A construct the
//! contract does not mention at all is a syntax error naming the place.

use super::ast::*;
use super::functions::{self, TimeUnit};
use super::lexer::{tokenize, Tok, Token};
use super::refuse;
use super::{SqlError, SqlResult2};
use sekejap_core::Kind;

/// The §4.1 / §4.2 function names this parser reads as ROW functions or, in a
/// `WHERE`, as range rewrites. Everything outside this set and outside
/// `refuse::TABLE` is a syntax error naming the place.
const ROW_FUNCTIONS: &[&str] = &[
    "EXTRACT",
    "DATE_TRUNC",
    "NOW",
    "CURRENT_DATE",
    "CURRENT_TIMESTAMP",
    "INTERVAL",
    "AGE",
    "TO_CHAR",
    "TO_TIMESTAMP",
    "TO_DATE",
    "LOWER",
    "UPPER",
    "LENGTH",
    "CHAR_LENGTH",
    "CONCAT",
    "SUBSTRING",
    "SUBSTR",
    "LEFT",
    "RIGHT",
    "TRIM",
    "BTRIM",
    "SPLIT_PART",
    "REPLACE",
    "POSITION",
    "STRPOS",
    "STARTS_WITH",
];

/// The reason a `LIKE` that is not a pure prefix carries. Named here because
/// two sites raise it and `docs/lang/QL_CONTRACT.md` §3 writes it once.
pub(super) const LIKE_NOT_A_PREFIX: &str = "QL_CONTRACT §3: `LIKE 'abc%'` is a text-key PREFIX range and is accepted; any other pattern (`'%abc%'`, `'a_c'`, an interior `%`) needs the trigram index family (pg_trgm-compatible) under a new feature bit, which is not built. Without that index the only way to answer it is a scan, and §6 does not allow one to be taken silently.";

/// A statement nests at most this deep: a scalar subquery inside a predicate
/// inside a statement, and an arithmetic ORDER BY of bounded depth. The cap is
/// here so pathological text cannot recurse the parser off its stack.
const MAX_DEPTH: usize = 32;

pub(super) struct Parser {
    tokens: Vec<Token>,
    at: usize,
    depth: usize,
    /// The geometry/geography cast the most recent `optional_cast` chain
    /// ended on, if any. PostGIS decides a distance's UNIT from the argument
    /// types -- degrees on `geometry`, metres on `geography` -- so the
    /// spatial forms read this to tell which of the two a statement wrote
    /// (`docs/core/SPATIAL_FUNCTIONS.md`, "The unit is the type").
    last_geo_cast: Option<GeoCast>,
    /// Whether the most recent spatial argument carried SRID 4326 itself
    /// (`ST_SetSRID(.., 4326)`, `ST_MakeEnvelope(.., 4326)`, GeoJSON). A bare
    /// `ST_MakePoint` has SRID 0, which PostGIS refuses against a 4326 column
    /// in a geometry predicate ("mixed SRID geometries").
    last_geo_srid: bool,
}

/// The two spatial types a `::` cast can name. Only the unit rule reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GeoCast {
    Geography,
    Geometry,
}

/// What an expression position parsed to, before it is classified into an
/// `OrderKey` or lowered into a `ScoreNode`.
#[derive(Clone, Debug)]
enum PExpr {
    Num(f64),
    Str(String),
    Param(usize),
    Column(String),
    Geo(GeoArg),
    Bm25 { column: String, query: TsQuery },
    SearchScore,
    StDistance { column: String, point: PointArg },
    /// `col <=> v`, `col <-> v`, `col <#> v`: a DISTANCE, whichever family
    /// the column turns out to belong to.
    Distance {
        column: String,
        right: Box<PExpr>,
        op: VecOp,
    },
    Add(Box<PExpr>, Box<PExpr>),
    Sub(Box<PExpr>, Box<PExpr>),
    Mul(Box<PExpr>, Box<PExpr>),
    Div(Box<PExpr>, Box<PExpr>),
    Neg(Box<PExpr>),
}

/// The value a client GUC reports. See `parser/catalog.rs`.
pub(crate) fn client_guc(name: &str) -> Option<&'static str> {
    catalog::guc(name)
}

pub(super) fn parse(text: &str) -> SqlResult2<Stmt> {
    let mut parser = Parser {
        tokens: tokenize(text)?,
        at: 0,
        depth: 0,
        last_geo_cast: None,
        last_geo_srid: false,
    };
    let statement = parser.statement()?;
    parser.eat(&Tok::Semicolon);
    if !matches!(parser.peek(), Tok::Eof) {
        parser.guard_here()?;
        return Err(SqlError::syntax(
            "one statement per call; the text continues after the first",
            parser.here(),
        ));
    }
    Ok(statement)
}

// Names the moved code reaches by `super::` path: they were one level up
// when this was one file, and are bound here so that they still are.
use super::{is_key_column, Tier, ID_COLUMN, KEY_COLUMN, MAX_GRAPH_DEPTH};

mod catalog;
mod ddl;
mod dml;
mod expr;
mod graph_table;
mod select;

impl Parser {
    // ── the token stream ─────────────────────────────────────────────────

    fn peek(&self) -> &Tok {
        &self.tokens[self.at].tok
    }

    fn peek_at(&self, ahead: usize) -> &Tok {
        let index = (self.at + ahead).min(self.tokens.len() - 1);
        &self.tokens[index].tok
    }

    /// The cursor, so a decision that turns out not to hold can be undone.
    /// Used by the two DML statements that have to LOOK at their `WHERE`
    /// before they know which atomic they are.
    fn mark(&self) -> usize {
        self.at
    }

    fn reset(&mut self, mark: usize) {
        self.at = mark;
    }

    fn here(&self) -> usize {
        self.tokens[self.at].at
    }

    fn bump(&mut self) -> Tok {
        let tok = self.tokens[self.at].tok.clone();
        if self.at + 1 < self.tokens.len() {
            self.at += 1;
        }
        tok
    }

    fn eat(&mut self, want: &Tok) -> bool {
        if self.peek() == want {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, want: &Tok) -> SqlResult2<()> {
        if self.eat(want) {
            return Ok(());
        }
        // The token the statement actually wrote may be a LISTED construct
        // standing where this one was expected -- `MATCH TRAIL (` is the
        // shape. The table is the authority there, so it is asked before a
        // place is named: a construct with a tier and a reason is refused by
        // name, and only text the table does not know is a syntax error.
        self.guard_here()?;
        Err(SqlError::syntax(
            format!(
                "expected `{}`, found `{}`",
                want.written(),
                self.peek().written()
            ),
            self.here(),
        ))
    }

    /// The upper-cased word at the cursor, if the cursor is on a word.
    fn word(&self) -> Option<String> {
        self.peek().keyword()
    }

    fn word_at(&self, ahead: usize) -> Option<String> {
        self.peek_at(ahead).keyword()
    }

    fn eat_word(&mut self, want: &str) -> bool {
        if self.word().as_deref() == Some(want) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect_word(&mut self, want: &str) -> SqlResult2<()> {
        if self.eat_word(want) {
            return Ok(());
        }
        self.guard_here()?;
        Err(SqlError::syntax(
            format!(
                "expected `{want}`, found `{}`",
                self.peek().written()
            ),
            self.here(),
        ))
    }

    /// The refusal a word or operator carries, if the Tier-2/3 table lists
    /// it. Two-word constructs are tried first so `ANY SHORTEST` is refused
    /// as itself rather than as a bare `ANY`. `GROUP BY` left this list when
    /// it became Tier 1 (`src/query/aggregate.rs`).
    fn listed(&self, word: &str) -> Option<SqlError> {
        for (first, second) in [
            ("ANY", "SHORTEST"),
            ("ALL", "SHORTEST"),
            ("SIMILAR", "TO"),
            ("GROUPING", "SETS"),
        ] {
            if word == first && self.word_at(1).as_deref() == Some(second) {
                return Some(refuse::refuse(&format!("{first} {second}")));
            }
        }
        if word == "AT"
            && self.word_at(1).as_deref() == Some("TIME")
            && self.word_at(2).as_deref() == Some("ZONE")
        {
            return Some(refuse::refuse("AT TIME ZONE"));
        }
        refuse::lookup(word).map(|(tier, reason)| SqlError::Refused {
            keyword: word.to_owned(),
            tier,
            reason,
        })
    }

    /// Refuse the word at the cursor if the table lists it.
    fn guard_word(&self) -> SqlResult2<()> {
        if let Some(word) = self.word() {
            if let Some(error) = self.listed(&word) {
                return Err(error);
            }
        }
        Ok(())
    }

    /// Refuse the OPERATOR at the cursor if the table lists it. The table is
    /// keyed by the operator's own spelling, so `&&` is refused as `&&`.
    fn guard_operator(&self) -> SqlResult2<()> {
        let spelling = match self.peek() {
            Tok::Overlaps => "&&",
            Tok::ContainsOp => "@>",
            Tok::Arrow => "->",
            Tok::LongArrow => "->>",
            Tok::HashArrow => "#>",
            Tok::HashLongArrow => "#>>",
            Tok::VecL1 => "<+>",
            Tok::Tilde => "~",
            _ => return Ok(()),
        };
        Err(refuse::refuse(spelling))
    }

    /// Refuse whatever stands at the cursor -- word or operator -- if the
    /// Tier-2/3 table lists it.
    ///
    /// This is the ONE sweep the contract's "refused by name" rule needs: a
    /// listed construct is refused wherever a statement can write it, rather
    /// than wherever someone remembered to test for it by hand. Every place
    /// that would otherwise name a position -- [`Parser::expect`],
    /// [`Parser::expect_word`], a SELECT's tail, the end of a statement --
    /// asks this first.
    fn guard_here(&self) -> SqlResult2<()> {
        self.guard_word()?;
        self.guard_operator()
    }

    fn deeper(&mut self) -> SqlResult2<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(SqlError::syntax(
                format!("statement nests deeper than {MAX_DEPTH}"),
                self.here(),
            ));
        }
        Ok(())
    }

    fn shallower(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// A bare or double-quoted name. `tbl.col` keeps the last segment, which
    /// is what a single-table statement means by it; a schema-qualified name
    /// (`schema.tbl.col`) is Tier 2.
    fn name(&mut self) -> SqlResult2<String> {
        let at = self.here();
        let mut parts = Vec::new();
        loop {
            let part = match self.peek().clone() {
                Tok::Word(word) => {
                    if let Some(error) = self.listed(&word.to_ascii_uppercase()) {
                        return Err(error);
                    }
                    self.bump();
                    word
                }
                Tok::Quoted(word) => {
                    self.bump();
                    word
                }
                other => {
                    return Err(SqlError::syntax(
                        format!("expected a name, found `{}`", other.written()),
                        at,
                    ))
                }
            };
            parts.push(part);
            if !self.eat(&Tok::Dot) {
                break;
            }
        }
        if parts.len() > 2 {
            return Err(refuse::refuse("CREATE SCHEMA"));
        }
        Ok(parts.pop().expect("at least one name part"))
    }

    /// A relation name in a `FROM`, keeping the schema qualifier when the
    /// qualifier names one of the two catalog schemas.
    ///
    /// `docs/lang/QL_CONTRACT.md` §2 puts `CREATE SCHEMA` and a real schema
    /// segment in Tier 2, so there is exactly one user schema and
    /// `public.t` MEANS `t` -- the qualifier is read and dropped, which is
    /// what [`Parser::name`] already did everywhere. The two exceptions are
    /// `pg_catalog` and `information_schema`: those qualifiers SELECT a
    /// relation rather than decorate one, and `information_schema.tables` is
    /// not the collection `tables`, so the qualified spelling is kept and
    /// `catalog::relation` resolves it.
    fn source_name(&mut self) -> SqlResult2<String> {
        let at = self.here();
        let first = self.name_part(at)?;
        if !self.eat(&Tok::Dot) {
            return Ok(first);
        }
        let second = self.name_part(at)?;
        if self.eat(&Tok::Dot) {
            return Err(refuse::refuse("CREATE SCHEMA"));
        }
        let qualifier = first.to_ascii_lowercase();
        if qualifier == "pg_catalog" || qualifier == "information_schema" {
            return Ok(format!("{qualifier}.{second}"));
        }
        Ok(second)
    }

    /// One segment of a dotted name, with the Tier-2/3 table consulted for a
    /// bare word exactly as [`Parser::name`] consults it.
    fn name_part(&mut self, at: usize) -> SqlResult2<String> {
        match self.peek().clone() {
            Tok::Word(word) => {
                if let Some(error) = self.listed(&word.to_ascii_uppercase()) {
                    return Err(error);
                }
                self.bump();
                Ok(word)
            }
            Tok::Quoted(word) => {
                self.bump();
                Ok(word)
            }
            other => Err(SqlError::syntax(
                format!("expected a name, found `{}`", other.written()),
                at,
            )),
        }
    }

    // ── statements ───────────────────────────────────────────────────────

    fn statement(&mut self) -> SqlResult2<Stmt> {
        let Some(word) = self.word() else {
            return Err(SqlError::syntax(
                format!("expected a statement, found `{}`", self.peek().written()),
                self.here(),
            ));
        };
        match word.as_str() {
            "SELECT" => {
                if let Some(items) = self.session_select()? {
                    return Ok(Stmt::SessionRows(items));
                }
                Ok(Stmt::Select(Box::new(self.select()?)))
            }
            "SHOW" => self.show(),
            "EXPLAIN" => {
                self.bump();
                // `EXPLAIN ANALYZE` and `EXPLAIN (FORMAT ...)` name options
                // this EXPLAIN does not have; it always executes and always
                // prints the same shape.
                if self.word().as_deref() == Some("ANALYZE") || matches!(self.peek(), Tok::LParen) {
                    return Err(SqlError::unsupported(
                        "EXPLAIN takes no options here: it always runs the statement and always prints the plan, the filters, the order and the QueryWork counters",
                    ));
                }
                if self.word().as_deref() == Some("DROP") {
                    return match self.drop()? {
                        Stmt::DropTable {
                            table,
                            if_exists,
                            cascade,
                        } => Ok(Stmt::ExplainDropTable {
                            table,
                            if_exists,
                            cascade,
                        }),
                        _ => Err(SqlError::unsupported(
                            "EXPLAIN DROP is written for DROP TABLE; DROP INDEX has one bounded phase and nothing to print",
                        )),
                    };
                }
                if self.word().as_deref() == Some("ALTER") {
                    let Stmt::AlterTable { table, action } = self.alter()? else {
                        unreachable!("alter builds only an AlterTable statement")
                    };
                    return Ok(Stmt::ExplainAlterTable { table, action });
                }
                if matches!(self.word().as_deref(), Some("UPDATE") | Some("DELETE")) {
                    let write = self.statement()?;
                    return Ok(match write {
                        // A write AT ONE KEY has no candidate walk to print;
                        // the predicated forms do.
                        Stmt::UpdateWhere { .. } | Stmt::DeleteWhere { .. } => {
                            Stmt::ExplainWrite(Box::new(write))
                        }
                        _ => {
                            return Err(SqlError::unsupported(
                                "EXPLAIN is written for the PREDICATED write: `UPDATE t SET ... WHERE <predicate>` and `DELETE FROM t WHERE <predicate>` prepare a candidate query and have a plan; a write at one key is one point-get and one put",
                            ))
                        }
                    });
                }
                Ok(Stmt::Explain(Box::new(self.select()?)))
            }
            "INSERT" => self.insert(),
            "UPDATE" => self.update(),
            "DELETE" => self.delete(),
            "CREATE" => self.create(),
            "ALTER" => self.alter(),
            "DROP" => self.drop(),
            "BEGIN" => {
                self.bump();
                // `docs/dist/OPS_CONTRACT.md` §7: e4 spells the bulk scope
                // `BEGIN BULK` / `END BULK`, because the scope is not a
                // transaction -- the writer is already inside one -- it is a
                // deferral of the durability point to the matching close.
                if self.eat_word("BULK") {
                    return Ok(Stmt::BeginBulk);
                }
                let _ = self.eat_word("TRANSACTION") || self.eat_word("WORK");
                if self.eat_word("READ") {
                    self.expect_word("ONLY")?;
                }
                Ok(Stmt::Begin)
            }
            "START" => {
                self.bump();
                self.expect_word("TRANSACTION")?;
                Ok(Stmt::Begin)
            }
            "COMMIT" | "END" => {
                self.bump();
                if self.eat_word("BULK") {
                    return Ok(Stmt::EndBulk);
                }
                let _ = self.eat_word("TRANSACTION") || self.eat_word("WORK");
                Ok(Stmt::Commit)
            }
            "ROLLBACK" | "ABORT" => {
                self.bump();
                let _ = self.eat_word("TRANSACTION") || self.eat_word("WORK");
                Ok(Stmt::Rollback)
            }
            "SET" | "RESET" => self.set_local(),
            other => match self.listed(other) {
                Some(error) => Err(error),
                None => Err(SqlError::syntax(
                    format!("`{other}` does not begin a statement"),
                    self.here(),
                )),
            },
        }
    }


}

/// Flatten every `And` of a WHERE tree into one conjunct list, recursively.
///
/// `AND` is associative, so a nested `And` carries no meaning the flat list
/// does not -- it only reaches the compiler as a boolean sub-tree whose leaves
/// then have to be membership sets. See the note above `where_clause`.
fn flatten_and(expr: Expr, out: &mut Vec<Expr>) {
    match expr {
        Expr::And(parts) => {
            for part in parts {
                flatten_and(part, out);
            }
        }
        other => out.push(other),
    }
}

/// An arithmetic expression, lowered to the `ScoreExpr` shape. Anything that
/// is not a number, an index leaf or arithmetic over them has no Score leaf
/// and is refused here rather than at execution.
fn lower(expr: PExpr) -> SqlResult2<ScoreNode> {
    Ok(match expr {
        PExpr::Num(value) => ScoreNode::Lit(value),
        PExpr::Column(column) => ScoreNode::Column(column),
        PExpr::Bm25 { column, query } => ScoreNode::Bm25 { column, query },
        PExpr::SearchScore => ScoreNode::SearchScore,
        PExpr::StDistance { column, point } => ScoreNode::Distance { column, point },
        PExpr::Distance { column, right, op } => match *right {
            PExpr::Param(n) => ScoreNode::VecDistance {
                column,
                query: Literal::Param(n),
                op,
            },
            PExpr::Str(text) => ScoreNode::VecDistance {
                column,
                query: Literal::Str(text),
                op,
            },
            _ => {
                return Err(SqlError::unsupported(
                    "inside an arithmetic ranking a distance operator takes a vector literal or a parameter",
                ))
            }
        },
        PExpr::Add(a, b) => ScoreNode::Add(Box::new(lower(*a)?), Box::new(lower(*b)?)),
        PExpr::Sub(a, b) => ScoreNode::Sub(Box::new(lower(*a)?), Box::new(lower(*b)?)),
        PExpr::Mul(a, b) => ScoreNode::Mul(Box::new(lower(*a)?), Box::new(lower(*b)?)),
        PExpr::Div(a, b) => ScoreNode::Div(Box::new(lower(*a)?), Box::new(lower(*b)?)),
        PExpr::Neg(a) => ScoreNode::Neg(Box::new(lower(*a)?)),
        PExpr::Str(_) | PExpr::Param(_) => {
            return Err(SqlError::unsupported(
                "a ranking expression is arithmetic over index leaves and numbers: a string or a bare parameter is not a leaf",
            ))
        }
        PExpr::Geo(_) => {
            return Err(SqlError::unsupported(
                "a geometry is not a ranking leaf: ST_Distance(col, point) is",
            ))
        }
    })
}
