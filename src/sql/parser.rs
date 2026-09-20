//! A hand-written recursive-descent parser for the Tier-1 grammar, and for
//! nothing else.
//!
//! The rule it follows throughout: a construct `docs/QL_CONTRACT.md` puts in
//! Tier 2 or Tier 3 is RECOGNISED and then REFUSED with that tier's reason
//! (`refuse.rs`), never skipped and never approximated. A construct the
//! contract does not mention at all is a syntax error naming the place.

use super::ast::*;
use super::lexer::{tokenize, Tok, Token};
use super::refuse;
use super::{SqlError, SqlResult2};
use crate::Kind;

/// A statement nests at most this deep: a scalar subquery inside a predicate
/// inside a statement, and an arithmetic ORDER BY of bounded depth. The cap is
/// here so pathological text cannot recurse the parser off its stack.
const MAX_DEPTH: usize = 32;

pub(super) struct Parser {
    tokens: Vec<Token>,
    at: usize,
    depth: usize,
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

pub(super) fn parse(text: &str) -> SqlResult2<Stmt> {
    let mut parser = Parser {
        tokens: tokenize(text)?,
        at: 0,
        depth: 0,
    };
    let statement = parser.statement()?;
    parser.eat(&Tok::Semicolon);
    if !matches!(parser.peek(), Tok::Eof) {
        return Err(SqlError::syntax(
            "one statement per call; the text continues after the first",
            parser.here(),
        ));
    }
    Ok(statement)
}

impl Parser {
    // ── the token stream ─────────────────────────────────────────────────

    fn peek(&self) -> &Tok {
        &self.tokens[self.at].tok
    }

    fn peek_at(&self, ahead: usize) -> &Tok {
        let index = (self.at + ahead).min(self.tokens.len() - 1);
        &self.tokens[index].tok
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
        if let Some(word) = self.word() {
            if let Some(error) = self.listed(&word) {
                return Err(error);
            }
        }
        Err(SqlError::syntax(
            format!(
                "expected `{want}`, found `{}`",
                self.peek().written()
            ),
            self.here(),
        ))
    }

    /// The refusal a word or operator carries, if the Tier-2/3 table lists
    /// it. Two-word constructs are tried first so `GROUP BY` is refused as
    /// itself rather than as a bare `GROUP`.
    fn listed(&self, word: &str) -> Option<SqlError> {
        for (first, second) in [
            ("GROUP", "BY"),
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
            Tok::LongArrow => "->>",
            Tok::Concat => "||",
            Tok::VecL1 => "<+>",
            Tok::Tilde => "~",
            _ => return Ok(()),
        };
        Err(refuse::refuse(spelling))
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

    // ── statements ───────────────────────────────────────────────────────

    fn statement(&mut self) -> SqlResult2<Stmt> {
        let Some(word) = self.word() else {
            return Err(SqlError::syntax(
                format!("expected a statement, found `{}`", self.peek().written()),
                self.here(),
            ));
        };
        match word.as_str() {
            "SELECT" => Ok(Stmt::Select(Box::new(self.select()?))),
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
                Ok(Stmt::Explain(Box::new(self.select()?)))
            }
            "INSERT" => self.insert(),
            "UPDATE" => self.update(),
            "DELETE" => self.delete(),
            "CREATE" => self.create(),
            "DROP" => self.drop(),
            "BEGIN" => {
                self.bump();
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
                let _ = self.eat_word("TRANSACTION") || self.eat_word("WORK");
                Ok(Stmt::Commit)
            }
            "ROLLBACK" | "ABORT" => {
                self.bump();
                let _ = self.eat_word("TRANSACTION") || self.eat_word("WORK");
                Ok(Stmt::Rollback)
            }
            "SET" => self.set_local(),
            other => match self.listed(other) {
                Some(error) => Err(error),
                None => Err(SqlError::syntax(
                    format!("`{other}` does not begin a statement"),
                    self.here(),
                )),
            },
        }
    }

    fn set_local(&mut self) -> SqlResult2<Stmt> {
        self.expect_word("SET")?;
        let _ = self.eat_word("LOCAL") || self.eat_word("SESSION");
        let mut name = self.name()?;
        // `diskann.query_search_list_size` arrives as two dotted names, and
        // `name()` keeps only the last segment; the knob is the whole thing.
        if name.eq_ignore_ascii_case("query_search_list_size")
            || name.eq_ignore_ascii_case("query_rescore")
        {
            name = format!("diskann.{name}");
        }
        if !self.eat(&Tok::Eq) {
            self.expect_word("TO")?;
        }
        let value = if let Some(word) = self.word() {
            match word.as_str() {
                "ON" | "OFF" | "DEFAULT" => {
                    self.bump();
                    Literal::Str(word.to_ascii_lowercase())
                }
                _ => self.literal()?,
            }
        } else {
            self.literal()?
        };
        Ok(Stmt::SetLocal { name, value })
    }

    fn create(&mut self) -> SqlResult2<Stmt> {
        self.expect_word("CREATE")?;
        match self.word().as_deref() {
            Some("SCHEMA") => Err(refuse::refuse("CREATE SCHEMA")),
            Some("VIEW") | Some("MATERIALIZED") => Err(refuse::refuse("CREATE VIEW")),
            Some("TRIGGER") => Err(refuse::refuse("CREATE TRIGGER")),
            Some("PROPERTY") => Err(refuse::refuse("CREATE PROPERTY GRAPH")),
            Some("TABLE") => self.create_table(),
            Some("INDEX") | Some("UNIQUE") => self.create_index(),
            _ => Err(SqlError::syntax(
                format!(
                    "expected TABLE or INDEX after CREATE, found `{}`",
                    self.peek().written()
                ),
                self.here(),
            )),
        }
    }

    fn create_table(&mut self) -> SqlResult2<Stmt> {
        self.expect_word("TABLE")?;
        if self.eat_word("IF") {
            self.expect_word("NOT")?;
            self.expect_word("EXISTS")?;
            return Err(SqlError::unsupported(
                "CREATE TABLE IF NOT EXISTS: `Database::create_collection` refuses a duplicate name and there is no catalog probe that makes the refusal conditional",
            ));
        }
        let table = self.name()?;
        self.expect(&Tok::LParen)?;
        let mut columns = Vec::new();
        loop {
            let name = self.name()?;
            let (kind, declared) = self.column_type()?;
            let mut primary_key = false;
            loop {
                match self.word().as_deref() {
                    Some("PRIMARY") => {
                        self.bump();
                        self.expect_word("KEY")?;
                        primary_key = true;
                    }
                    Some("NOT") if self.word_at(1).as_deref() == Some("NULL") => {
                        return Err(SqlError::unsupported(
                            "NOT NULL: a declared field is present or absent per row and the codec has no column constraint to enforce",
                        ));
                    }
                    Some("DEFAULT") | Some("UNIQUE") | Some("REFERENCES") | Some("CHECK") => {
                        return Err(SqlError::unsupported(format!(
                            "column constraint `{}`: the catalog holds a name and a Kind per field and nothing else",
                            self.peek().written()
                        )));
                    }
                    _ => break,
                }
            }
            columns.push(ColumnDef {
                name,
                kind,
                declared,
                primary_key,
            });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen)?;
        Ok(Stmt::CreateTable { table, columns })
    }

    /// The declared SQL type, and the `Kind` it is stored as.
    fn column_type(&mut self) -> SqlResult2<(Kind, String)> {
        let at = self.here();
        let Some(word) = self.word() else {
            return Err(SqlError::syntax(
                format!("expected a column type, found `{}`", self.peek().written()),
                at,
            ));
        };
        self.bump();
        let pair = match word.as_str() {
            "TEXT" | "VARCHAR" => (Kind::Text, "TEXT"),
            "INT" | "INTEGER" | "INT4" | "SMALLINT" => (Kind::Int, "INT"),
            "BIGINT" | "INT8" => (Kind::Int, "BIGINT"),
            "REAL" | "FLOAT4" => (Kind::Real, "REAL"),
            "DOUBLE" => {
                self.expect_word("PRECISION")?;
                (Kind::Real, "DOUBLE PRECISION")
            }
            "BOOLEAN" | "BOOL" => (Kind::Bool, "BOOLEAN"),
            "JSONB" | "JSON" => (Kind::Json, "JSONB"),
            // Stored as Int microseconds; the declaration is what says so
            // (QL_CONTRACT §5 deviation 8).
            "TIMESTAMPTZ" => (Kind::Int, "TIMESTAMPTZ"),
            "TIMESTAMP" => {
                if self.eat_word("WITH") {
                    self.expect_word("TIME")?;
                    self.expect_word("ZONE")?;
                }
                (Kind::Int, "TIMESTAMPTZ")
            }
            "DATE" => (Kind::Int, "DATE"),
            "GEOMETRY" | "GEOGRAPHY" => {
                let declared = if self.eat(&Tok::LParen) {
                    let shape = self
                        .word()
                        .ok_or_else(|| SqlError::syntax("expected a geometry type", at))?;
                    self.bump();
                    if self.eat(&Tok::Comma) {
                        match self.bump() {
                            Tok::Num(n, _) if n == 4326.0 => {}
                            other => {
                                return Err(SqlError::unsupported(format!(
                                    "SRID `{}`: storage is WGS84 and ST_Transform is Tier 2",
                                    other.written()
                                )))
                            }
                        }
                    }
                    self.expect(&Tok::RParen)?;
                    shape
                } else {
                    "GEOMETRY".to_owned()
                };
                return Ok(match declared.to_ascii_uppercase().as_str() {
                    "POINT" => (Kind::Point, "GEOMETRY(Point,4326)".to_owned()),
                    "POLYGON" | "MULTIPOLYGON" | "LINESTRING" | "GEOMETRY" => {
                        (Kind::Geo, format!("GEOMETRY({declared},4326)"))
                    }
                    other => {
                        return Err(SqlError::unsupported(format!(
                            "GEOMETRY({other}): the stored kinds are Point and Geo"
                        )))
                    }
                });
            }
            "VECTOR" => {
                self.expect(&Tok::LParen)?;
                let dimensions = match self.bump() {
                    Tok::Num(n, true) if n > 0.0 => n as usize,
                    other => {
                        return Err(SqlError::syntax(
                            format!("VECTOR needs a positive dimension, found `{}`", other.written()),
                            at,
                        ))
                    }
                };
                self.expect(&Tok::RParen)?;
                return Ok((Kind::Vector(dimensions), format!("VECTOR({dimensions})")));
            }
            "HALFVEC" | "SPARSEVEC" => {
                return Err(SqlError::Refused {
                    keyword: word,
                    tier: super::Tier::Three,
                    reason: "QL_CONTRACT §4.5: halfvec, sparsevec and binary quantization have no atomic.",
                })
            }
            other => {
                return Err(SqlError::unsupported(format!(
                    "column type `{other}` has no Kind in docs/FORMAT_V1.md"
                )))
            }
        };
        Ok((pair.0, pair.1.to_owned()))
    }

    fn create_index(&mut self) -> SqlResult2<Stmt> {
        let unique = self.eat_word("UNIQUE");
        self.expect_word("INDEX")?;
        if self.eat_word("CONCURRENTLY") {
            return Err(SqlError::unsupported(
                "CREATE INDEX CONCURRENTLY: an E4 build is already incremental (`build_index_step`) and a writer is single",
            ));
        }
        let name = self.name()?;
        self.expect_word("ON")?;
        let table = self.name()?;
        self.expect_word("USING")?;
        let method_at = self.here();
        let Some(method) = self.word() else {
            return Err(SqlError::syntax("expected an index method", method_at));
        };
        self.bump();
        self.expect(&Tok::LParen)?;
        let method = match method.as_str() {
            "BTREE" => IndexMethod::Btree(self.name()?),
            "GIN" => {
                // `gin(to_tsvector('simple', col))` -- the expression index
                // Postgres needs, spelled the same way the query spells it.
                self.expect_word("TO_TSVECTOR")?;
                self.expect(&Tok::LParen)?;
                self.simple_config()?;
                self.expect(&Tok::Comma)?;
                let column = self.name()?;
                self.expect(&Tok::RParen)?;
                IndexMethod::Gin(column)
            }
            "GIST" | "SPGIST" => IndexMethod::Gist(self.name()?),
            "EXACT" => IndexMethod::Exact(self.name()?),
            "QUANTIZED" => {
                let column = self.name()?;
                self.vector_opclass()?;
                IndexMethod::Quantized {
                    column,
                    alias: None,
                }
            }
            alias @ ("HNSW" | "DISKANN" | "IVFFLAT") => {
                let column = self.name()?;
                self.vector_opclass()?;
                IndexMethod::Quantized {
                    column,
                    alias: Some(alias.to_owned()),
                }
            }
            "BRIN" | "HASH" => {
                return Err(SqlError::unsupported(format!(
                    "USING {method}: the index families are btree, gin, gist, exact and quantized (docs/SOURCE_LAYOUT.md, `src/index/`)"
                )))
            }
            other => {
                return Err(SqlError::unsupported(format!(
                    "USING {other}: no such index family"
                )))
            }
        };
        self.expect(&Tok::RParen)?;
        if unique && !matches!(method, IndexMethod::Btree(_)) {
            return Err(SqlError::unsupported(
                "UNIQUE belongs to a btree index only: `create_scalar_index` is the one family that takes it",
            ));
        }
        if self.eat_word("WITH") {
            return Err(SqlError::unsupported(
                "CREATE INDEX ... WITH (...): build parameters are the family's own and are not settable from SQL",
            ));
        }
        Ok(Stmt::CreateIndex {
            name,
            table,
            method,
        })
    }

    /// `vector_cosine_ops` and its siblings, which name the metric.
    fn vector_opclass(&mut self) -> SqlResult2<()> {
        if let Some(word) = self.word() {
            match word.as_str() {
                "VECTOR_COSINE_OPS" => {
                    self.bump();
                }
                other if other.starts_with("VECTOR_") => {
                    return Err(SqlError::unsupported(format!(
                        "operator class `{other}`: the quantized family is built for cosine (`vector_cosine_ops`); the metric of a query is the operator it writes"
                    )))
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn simple_config(&mut self) -> SqlResult2<()> {
        match self.bump() {
            Tok::Str(name) if name.eq_ignore_ascii_case("simple") => Ok(()),
            Tok::Str(other) => Err(SqlError::Refused {
                keyword: format!("to_tsvector('{other}', ...)"),
                tier: super::Tier::Three,
                reason: "QL_CONTRACT §4.6: analyzer v1 is language-neutral; stemming beyond 'simple' has no atomic.",
            }),
            other => Err(SqlError::syntax(
                format!("expected the text search configuration 'simple', found `{}`", other.written()),
                self.here(),
            )),
        }
    }

    fn drop(&mut self) -> SqlResult2<Stmt> {
        self.expect_word("DROP")?;
        let what = self.word().unwrap_or_default();
        match what.as_str() {
            "TABLE" => {
                self.bump();
                let if_exists = self.if_exists()?;
                let table = self.name()?;
                // Postgres spells the two behaviours this way and RESTRICT is
                // its default too; here the restriction is graph edges rather
                // than foreign keys (GRAPH_CONTRACT 6.1).
                let cascade = if self.eat_word("CASCADE") {
                    true
                } else {
                    let _ = self.eat_word("RESTRICT");
                    false
                };
                Ok(Stmt::DropTable {
                    table,
                    if_exists,
                    cascade,
                })
            }
            "INDEX" => {
                self.bump();
                let if_exists = self.if_exists()?;
                let name = self.name()?;
                Ok(Stmt::DropIndex { name, if_exists })
            }
            other => Err(SqlError::unsupported(format!(
                "DROP {other}: DROP TABLE and DROP INDEX are the catalog's own"
            ))),
        }
    }

    fn if_exists(&mut self) -> SqlResult2<bool> {
        if self.eat_word("IF") {
            self.expect_word("EXISTS")?;
            return Ok(true);
        }
        Ok(false)
    }

    fn insert(&mut self) -> SqlResult2<Stmt> {
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

    fn update(&mut self) -> SqlResult2<Stmt> {
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

    fn delete(&mut self) -> SqlResult2<Stmt> {
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

    // ── SELECT ───────────────────────────────────────────────────────────

    fn select(&mut self) -> SqlResult2<SelectStmt> {
        self.expect_word("SELECT")?;
        if self.word().as_deref() == Some("DISTINCT") {
            return Err(refuse::refuse("DISTINCT"));
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
            self.conjunction()?
        } else {
            Vec::new()
        };
        if let Some(word) = self.word() {
            if word == "GROUP" || word == "HAVING" || word == "WINDOW" {
                return Err(self
                    .listed(&word)
                    .unwrap_or_else(|| refuse::refuse("GROUP BY")));
            }
        }
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
            order,
            limit,
        })
    }

    fn select_item(&mut self) -> SqlResult2<SelectItem> {
        if self.eat(&Tok::Star) {
            return Ok(SelectItem::Star);
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

    // ── WHERE ────────────────────────────────────────────────────────────

    fn conjunction(&mut self) -> SqlResult2<Vec<Predicate>> {
        let mut out = Vec::new();
        loop {
            out.push(self.predicate()?);
            if self.word().as_deref() == Some("OR") {
                return Err(refuse::refuse("OR"));
            }
            if !self.eat_word("AND") {
                break;
            }
        }
        Ok(out)
    }

    fn predicate(&mut self) -> SqlResult2<Predicate> {
        self.deeper()?;
        let result = self.predicate_inner();
        self.shallower();
        result
    }

    fn predicate_inner(&mut self) -> SqlResult2<Predicate> {
        if matches!(self.peek(), Tok::LParen) {
            return Err(SqlError::unsupported(
                "a parenthesised WHERE group: the conjunction is flat because a filter list is flat; OR and NOT, which are what parentheses are for, are Tier 2",
            ));
        }
        self.guard_word()?;
        if let Some(word) = self.word() {
            match word.as_str() {
                "ST_DWITHIN" | "ST_INTERSECTS" | "ST_WITHIN" | "ST_CONTAINS" | "ST_COVERS"
                | "ST_CROSSES" => return self.spatial_predicate(),
                "TO_TSVECTOR" => return self.text_predicate(),
                _ => {}
            }
        }
        let at = self.here();
        let column = self.name()?;
        // A cast on the left of a predicate (`plot::geometry`) is PostGIS's
        // way of choosing the planar overload; the unit semantics here come
        // from the predicate itself, so the cast is read and dropped.
        self.optional_cast()?;
        self.guard_operator()?;
        if let Some(word) = self.word() {
            match word.as_str() {
                "BETWEEN" => {
                    self.bump();
                    let lower = self.literal()?;
                    self.expect_word("AND")?;
                    let upper = self.literal()?;
                    return Ok(if super::is_key_column(&column) {
                        Predicate::KeyBetween { lower, upper }
                    } else {
                        Predicate::Between {
                            column,
                            lower,
                            upper,
                        }
                    });
                }
                "IS" => {
                    self.bump();
                    let negated = self.eat_word("NOT");
                    if self.eat_word("NULL") {
                        return Ok(Predicate::IsNull { column, negated });
                    }
                    if self.eat_word("MISSING") {
                        if negated {
                            return Err(SqlError::unsupported(
                                "IS NOT MISSING: the scalar predicates are Eq, Range, IsNull and IsMissing; a complement is Tier 2 (NOT)",
                            ));
                        }
                        return Ok(Predicate::IsMissing { column });
                    }
                    return Err(SqlError::syntax(
                        "expected NULL or MISSING after IS",
                        self.here(),
                    ));
                }
                other => {
                    if let Some(error) = self.listed(other) {
                        return Err(error);
                    }
                }
            }
        }
        let op = match self.peek() {
            Tok::Eq => CmpOp::Eq,
            Tok::Ne => CmpOp::Ne,
            Tok::Lt => CmpOp::Lt,
            Tok::Le => CmpOp::Le,
            Tok::Gt => CmpOp::Gt,
            Tok::Ge => CmpOp::Ge,
            Tok::VecCosine | Tok::VecL2 | Tok::VecDot => {
                return Err(SqlError::Refused {
                    keyword: self.peek().written(),
                    tier: super::Tier::Two,
                    reason: "QL_CONTRACT §4.5: a distance as a FILTER (`emb <=> $v < 0.3`) is an ef-bounded approximate membership set. As an ORDER BY the same operator is Tier 1.",
                })
            }
            other => {
                return Err(SqlError::syntax(
                    format!("expected a comparison after `{column}`, found `{}`", other.written()),
                    at,
                ))
            }
        };
        self.bump();
        let value = self.literal()?;
        Ok(if super::is_key_column(&column) {
            Predicate::KeyCompare { op, value }
        } else {
            Predicate::Compare { column, op, value }
        })
    }

    fn text_predicate(&mut self) -> SqlResult2<Predicate> {
        let column = self.tsvector()?;
        if !self.eat(&Tok::Matches) {
            return Err(SqlError::syntax(
                format!(
                    "expected `@@` after to_tsvector, found `{}`",
                    self.peek().written()
                ),
                self.here(),
            ));
        }
        let query = self.tsquery()?;
        Ok(Predicate::Text { column, query })
    }

    /// `to_tsvector('simple', col)` -- one declared field, because a text
    /// index spans one (battle50k deviation 1).
    fn tsvector(&mut self) -> SqlResult2<String> {
        self.expect_word("TO_TSVECTOR")?;
        self.expect(&Tok::LParen)?;
        self.simple_config()?;
        self.expect(&Tok::Comma)?;
        let column = self.name()?;
        if matches!(self.peek(), Tok::Concat) {
            return Err(refuse::refuse("||"));
        }
        self.expect(&Tok::RParen)?;
        Ok(column)
    }

    fn tsquery(&mut self) -> SqlResult2<TsQuery> {
        self.expect_word("TO_TSQUERY")?;
        self.expect(&Tok::LParen)?;
        self.simple_config()?;
        self.expect(&Tok::Comma)?;
        let source = self.literal()?;
        self.expect(&Tok::RParen)?;
        Ok(TsQuery {
            source,
            tsquery_syntax: true,
        })
    }

    fn spatial_predicate(&mut self) -> SqlResult2<Predicate> {
        let name = self.word().expect("caller checked the word");
        let at = self.here();
        self.bump();
        let predicate = match name.as_str() {
            "ST_DWITHIN" => SpatialPredicate::DWithin,
            "ST_INTERSECTS" => SpatialPredicate::Intersects,
            "ST_WITHIN" => SpatialPredicate::Within,
            "ST_CONTAINS" => SpatialPredicate::Contains,
            other => {
                return Err(SqlError::Refused {
                    keyword: other.to_owned(),
                    tier: super::Tier::Two,
                    reason: "QL_CONTRACT §4.4: ST_Covers and ST_Crosses are Tier 1 as ROW functions; as index-side predicates the filter atomics are Intersects, Within, Contains and DWithin.",
                })
            }
        };
        self.expect(&Tok::LParen)?;
        let column = self.name()?;
        self.optional_cast()?;
        self.expect(&Tok::Comma)?;
        let argument = self.geo_argument()?;
        let metres = if predicate == SpatialPredicate::DWithin {
            self.expect(&Tok::Comma)?;
            let metres = self.literal()?;
            // PostGIS's fourth argument chooses the spheroid; E4's radius is
            // spheroidal and has no planar twin.
            if self.eat(&Tok::Comma) {
                match self.word().as_deref() {
                    Some("TRUE") => {
                        self.bump();
                    }
                    Some("FALSE") => {
                        return Err(SqlError::unsupported(
                            "ST_DWithin(..., false): use_spheroid = false is a planar distance; PointFilter::Radius and GeometryFilter::DWithin are spheroidal (docs/SPATIAL_FUNCTIONS.md) and there is no planar-distance atomic",
                        ))
                    }
                    _ => {
                        return Err(SqlError::syntax(
                            "ST_DWithin's fourth argument is use_spheroid",
                            self.here(),
                        ))
                    }
                }
            }
            Some(metres)
        } else {
            None
        };
        self.expect(&Tok::RParen)?;
        let _ = at;
        Ok(Predicate::Spatial {
            predicate,
            column,
            argument,
            metres,
        })
    }

    /// `::geography` / `::geometry` / `::vector`, read and dropped: the unit
    /// semantics of a predicate here come from the predicate, not the cast
    /// (`GeometryFilter`'s own documentation, `src/query/mod.rs`).
    fn optional_cast(&mut self) -> SqlResult2<()> {
        while self.eat(&Tok::Cast) {
            let at = self.here();
            let Some(word) = self.word() else {
                return Err(SqlError::syntax("expected a type after `::`", at));
            };
            self.bump();
            match word.as_str() {
                "GEOGRAPHY" | "GEOMETRY" | "VECTOR" | "TEXT" | "FLOAT8" | "DOUBLE" | "INT"
                | "INTEGER" | "BIGINT" | "REAL" => {}
                other => {
                    return Err(SqlError::unsupported(format!(
                        "cast `::{other}` has no Tier-1 meaning here"
                    )))
                }
            }
            if word == "DOUBLE" {
                self.expect_word("PRECISION")?;
            }
        }
        Ok(())
    }

    fn geo_argument(&mut self) -> SqlResult2<GeoArg> {
        self.deeper()?;
        let result = self.geo_argument_inner();
        self.shallower();
        result
    }

    fn geo_argument_inner(&mut self) -> SqlResult2<GeoArg> {
        self.guard_word()?;
        let argument = match self.word().as_deref() {
            Some("ST_SETSRID") => {
                self.bump();
                self.expect(&Tok::LParen)?;
                let inner = self.geo_argument()?;
                self.expect(&Tok::Comma)?;
                match self.bump() {
                    Tok::Num(n, _) if n == 4326.0 => {}
                    other => {
                        return Err(SqlError::unsupported(format!(
                            "ST_SetSRID(..., {}): storage is WGS84; ST_Transform is Tier 2",
                            other.written()
                        )))
                    }
                }
                self.expect(&Tok::RParen)?;
                inner
            }
            Some("ST_MAKEPOINT") => {
                self.bump();
                self.expect(&Tok::LParen)?;
                let lon = self.literal()?;
                self.expect(&Tok::Comma)?;
                let lat = self.literal()?;
                self.expect(&Tok::RParen)?;
                GeoArg::Point(PointArg { lon, lat })
            }
            Some("ST_POINT") => {
                self.bump();
                self.expect(&Tok::LParen)?;
                let lon = self.literal()?;
                self.expect(&Tok::Comma)?;
                let lat = self.literal()?;
                self.expect(&Tok::RParen)?;
                GeoArg::Point(PointArg { lon, lat })
            }
            Some("ST_MAKEENVELOPE") => {
                self.bump();
                self.expect(&Tok::LParen)?;
                let minlon = self.literal()?;
                self.expect(&Tok::Comma)?;
                let minlat = self.literal()?;
                self.expect(&Tok::Comma)?;
                let maxlon = self.literal()?;
                self.expect(&Tok::Comma)?;
                let maxlat = self.literal()?;
                if self.eat(&Tok::Comma) {
                    match self.bump() {
                        Tok::Num(n, _) if n == 4326.0 => {}
                        other => {
                            return Err(SqlError::unsupported(format!(
                                "ST_MakeEnvelope(..., {}): storage is WGS84",
                                other.written()
                            )))
                        }
                    }
                }
                self.expect(&Tok::RParen)?;
                GeoArg::Envelope {
                    minlon,
                    minlat,
                    maxlon,
                    maxlat,
                }
            }
            Some("ST_GEOMFROMGEOJSON") => {
                self.bump();
                self.expect(&Tok::LParen)?;
                let json = self.literal()?;
                self.expect(&Tok::RParen)?;
                GeoArg::GeoJson(json)
            }
            _ => GeoArg::GeoJson(self.literal()?),
        };
        self.optional_cast()?;
        Ok(argument)
    }

    // ── ORDER BY ─────────────────────────────────────────────────────────

    fn order_key(&mut self) -> SqlResult2<OrderKey> {
        let expression = self.expression(0)?;
        let descending = if self.eat_word("DESC") {
            true
        } else {
            let _ = self.eat_word("ASC");
            false
        };
        if self.word().as_deref() == Some("NULLS") {
            return Err(SqlError::unsupported(
                "NULLS FIRST / NULLS LAST: `compare_rank` fixes where a nullish key sorts, and it is not a per-query choice",
            ));
        }
        Ok(match expression {
            PExpr::Column(column) => OrderKey::Column { column, descending },
            PExpr::Bm25 { column, query } => OrderKey::Bm25 {
                column,
                query,
                descending,
            },
            PExpr::Distance { column, right, op } => match *right {
                PExpr::Geo(GeoArg::Point(point)) => {
                    if op != VecOp::L2 {
                        return Err(SqlError::unsupported(
                            "the PostGIS KNN operator is `<->`; `<=>` and `<#>` are pgvector's and take a vector",
                        ));
                    }
                    OrderKey::Distance {
                        column,
                        point,
                        descending,
                    }
                }
                PExpr::Geo(_) => {
                    return Err(SqlError::unsupported(
                        "ORDER BY <-> takes a point: the nearest walk starts at one centre",
                    ))
                }
                PExpr::Param(n) => OrderKey::Vector {
                    column,
                    query: Literal::Param(n),
                    op,
                    descending,
                },
                PExpr::Str(text) => OrderKey::Vector {
                    column,
                    query: Literal::Str(text),
                    op,
                    descending,
                },
                other => {
                    return Err(SqlError::unsupported(format!(
                        "the right side of a distance operator is a vector literal, a parameter or a point; found {other:?}"
                    )))
                }
            },
            other => OrderKey::Score {
                expr: lower(other)?,
                descending,
            },
        })
    }

    /// A precedence-climbing expression parser. Level 0 is `+`/`-`, level 1
    /// `*`/`/`, level 2 the distance operators, level 3 a primary.
    fn expression(&mut self, level: usize) -> SqlResult2<PExpr> {
        self.deeper()?;
        let result = self.expression_inner(level);
        self.shallower();
        result
    }

    fn expression_inner(&mut self, level: usize) -> SqlResult2<PExpr> {
        if level >= 3 {
            return self.primary();
        }
        let mut left = self.expression(level + 1)?;
        loop {
            self.guard_operator()?;
            let node = match (level, self.peek()) {
                (0, Tok::Plus) => {
                    self.bump();
                    PExpr::Add(Box::new(left), Box::new(self.expression(1)?))
                }
                (0, Tok::Minus) => {
                    self.bump();
                    PExpr::Sub(Box::new(left), Box::new(self.expression(1)?))
                }
                (1, Tok::Star) => {
                    self.bump();
                    PExpr::Mul(Box::new(left), Box::new(self.expression(2)?))
                }
                (1, Tok::Slash) => {
                    self.bump();
                    PExpr::Div(Box::new(left), Box::new(self.expression(2)?))
                }
                (2, Tok::VecCosine | Tok::VecL2 | Tok::VecDot) => {
                    let op = match self.peek() {
                        Tok::VecCosine => VecOp::Cosine,
                        Tok::VecL2 => VecOp::L2,
                        _ => VecOp::NegativeDot,
                    };
                    self.bump();
                    let PExpr::Column(column) = left else {
                        return Err(SqlError::unsupported(
                            "a distance operator takes an indexed column on its left: the index is what the walk reads",
                        ));
                    };
                    let right = if matches!(
                        self.word().as_deref(),
                        Some("ST_SETSRID") | Some("ST_MAKEPOINT") | Some("ST_POINT")
                    ) {
                        PExpr::Geo(self.geo_argument()?)
                    } else {
                        self.expression(3)?
                    };
                    PExpr::Distance {
                        column,
                        right: Box::new(right),
                        op,
                    }
                }
                _ => return Ok(left),
            };
            left = node;
        }
    }

    fn primary(&mut self) -> SqlResult2<PExpr> {
        self.guard_operator()?;
        if self.eat(&Tok::Minus) {
            return Ok(PExpr::Neg(Box::new(self.expression(2)?)));
        }
        if self.eat(&Tok::Plus) {
            return self.expression(2);
        }
        if self.eat(&Tok::LParen) {
            let inner = self.expression(0)?;
            self.expect(&Tok::RParen)?;
            self.optional_cast()?;
            return Ok(inner);
        }
        let at = self.here();
        match self.peek().clone() {
            Tok::Num(value, _) => {
                self.bump();
                Ok(PExpr::Num(value))
            }
            Tok::Str(text) => {
                self.bump();
                self.optional_cast()?;
                Ok(PExpr::Str(text))
            }
            Tok::Param(n) => {
                self.bump();
                self.optional_cast()?;
                Ok(PExpr::Param(n))
            }
            Tok::Word(_) | Tok::Quoted(_) => {
                self.guard_word()?;
                match self.word().as_deref() {
                    Some("TS_RANK_CD") | Some("TS_RANK") => {
                        self.bump();
                        self.expect(&Tok::LParen)?;
                        let column = self.tsvector()?;
                        self.expect(&Tok::Comma)?;
                        let query = self.tsquery()?;
                        // ts_rank_cd's optional normalisation argument picks
                        // a document-length correction Postgres applies to
                        // its own formula; BM25 has its own (QL_CONTRACT §5
                        // deviation 5) and cannot be steered by it.
                        if self.eat(&Tok::Comma) {
                            return Err(SqlError::unsupported(
                                "ts_rank_cd's normalisation argument: the ranking here is BM25 (QL_CONTRACT §5 deviation 5) and has no ts_rank normalisation knob",
                            ));
                        }
                        self.expect(&Tok::RParen)?;
                        Ok(PExpr::Bm25 { column, query })
                    }
                    Some("BM25") => {
                        self.bump();
                        self.expect(&Tok::LParen)?;
                        let column = self.name()?;
                        self.expect(&Tok::Comma)?;
                        let source = self.literal()?;
                        self.expect(&Tok::RParen)?;
                        Ok(PExpr::Bm25 {
                            column,
                            query: TsQuery {
                                source,
                                tsquery_syntax: false,
                            },
                        })
                    }
                    Some("ST_DISTANCE") => {
                        self.bump();
                        self.expect(&Tok::LParen)?;
                        let column = self.name()?;
                        self.optional_cast()?;
                        self.expect(&Tok::Comma)?;
                        let point = match self.geo_argument()? {
                            GeoArg::Point(point) => point,
                            _ => {
                                return Err(SqlError::unsupported(
                                    "ST_Distance in a ranking takes a point: `ScoreExpr::Distance` is geodesic metres from one centre",
                                ))
                            }
                        };
                        self.expect(&Tok::RParen)?;
                        Ok(PExpr::StDistance { column, point })
                    }
                    Some("ST_SETSRID") | Some("ST_MAKEPOINT") | Some("ST_POINT")
                    | Some("ST_MAKEENVELOPE") | Some("ST_GEOMFROMGEOJSON") => {
                        Ok(PExpr::Geo(self.geo_argument()?))
                    }
                    Some("TRUE") => {
                        self.bump();
                        Ok(PExpr::Num(1.0))
                    }
                    Some("FALSE") => {
                        self.bump();
                        Ok(PExpr::Num(0.0))
                    }
                    _ => {
                        if matches!(self.peek_at(1), Tok::LParen) {
                            let name = self.word().unwrap_or_default();
                            return Err(match self.listed(&name) {
                                Some(error) => error,
                                None => SqlError::unsupported(format!(
                                    "function `{name}` is not in QL_CONTRACT §4"
                                )),
                            });
                        }
                        let name = self.name()?;
                        self.optional_cast()?;
                        Ok(PExpr::Column(name))
                    }
                }
            }
            other => Err(SqlError::syntax(
                format!("expected a value, found `{}`", other.written()),
                at,
            )),
        }
    }

    // ── GRAPH_TABLE ──────────────────────────────────────────────────────

    fn graph_table(&mut self) -> SqlResult2<GraphTable> {
        self.expect_word("GRAPH_TABLE")?;
        self.expect(&Tok::LParen)?;
        let context = self.name()?;
        self.expect_word("MATCH")?;
        if let Some(word) = self.word() {
            if word == "ANY" || word == "ALL" || word == "SHORTEST" {
                return Err(refuse::refuse("ANY SHORTEST"));
            }
        }
        // (a:coll WHERE a.key = $1)
        self.expect(&Tok::LParen)?;
        let _seed_variable = self.name()?;
        let seed_collection = self.element_label()?;
        self.expect_word("WHERE")?;
        // `name()` reads `a._key` and keeps the last segment, so the element
        // variable in front of the field is already accounted for.
        let field = self.name()?;
        if !super::is_key_column(&field) {
            return Err(SqlError::Refused {
                keyword: "inline element WHERE".into(),
                tier: super::Tier::Two,
                reason: "QL_CONTRACT §4.3 and §5 deviation 2: an inline element WHERE prunes per hop (GRAPH_CONTRACT 4.3). Only a key equality seeds a traversal in this slice; any other inline predicate is the per-hop prune, which is Tier 2.",
            });
        }
        self.expect(&Tok::Eq)?;
        let seed_key = self.literal()?;
        if self.word().as_deref() == Some("AND") {
            return Err(SqlError::Refused {
                keyword: "inline element WHERE".into(),
                tier: super::Tier::Two,
                reason: "QL_CONTRACT §4.3 and §5 deviation 2: an inline element WHERE prunes per hop (GRAPH_CONTRACT 4.3). Only a key equality seeds in this slice.",
            });
        }
        self.expect(&Tok::RParen)?;
        let hop = self.graph_hop()?;
        self.expect(&Tok::LParen)?;
        let _target_variable = self.name()?;
        let target_collection = self.element_label()?;
        if self.word().as_deref() == Some("WHERE") {
            return Err(SqlError::Refused {
                keyword: "inline element WHERE".into(),
                tier: super::Tier::Two,
                reason: "QL_CONTRACT §4.3: an inline WHERE on the far element is the per-hop prune (GRAPH_CONTRACT 4.3). Write it after COLUMNS, where it is the Tier-1 post-pattern filter on completed matches.",
            });
        }
        self.expect(&Tok::RParen)?;
        self.expect_word("COLUMNS")?;
        self.expect(&Tok::LParen)?;
        let mut columns = Vec::new();
        loop {
            // `name()` reads `b._key` and keeps the last segment, which is
            // the field; the variable in front of it is the element the
            // pattern already bound.
            let field = self.name()?;
            let item = if field == super::ID_COLUMN {
                SelectItem::Id
            } else if super::is_key_column(&field) {
                SelectItem::Key
            } else {
                SelectItem::Column(field.clone())
            };
            let alias = if self.eat_word("AS") {
                self.name()?
            } else {
                field
            };
            columns.push((item, alias));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen)?;
        self.expect(&Tok::RParen)?;
        Ok(GraphTable {
            context,
            seed_collection,
            seed_key,
            hop,
            target_collection,
            columns,
        })
    }

    /// `:label` or `IS label`, both spellings SQL/PGQ allows.
    fn element_label(&mut self) -> SqlResult2<String> {
        if self.eat(&Tok::Colon) || self.eat_word("IS") {
            let label = self.name()?;
            if matches!(self.peek(), Tok::Pipe) {
                return Err(SqlError::Refused {
                    keyword: "label alternation".into(),
                    tier: super::Tier::Two,
                    reason: "QL_CONTRACT §4.3: label alternation `a|b` is a multi-type hop (two ranges per hop); not built in this slice.",
                });
            }
            return Ok(label);
        }
        Err(SqlError::unsupported(
            "an element pattern names its collection (`(a:place)` or `(a IS place)`): a traversal walks one collection's rows",
        ))
    }

    fn graph_hop(&mut self) -> SqlResult2<GraphHop> {
        let incoming = self.eat(&Tok::BackArrow);
        if !incoming {
            self.expect(&Tok::Minus)?;
        }
        let mut edge_type = None;
        if self.eat(&Tok::LBracket) {
            if !matches!(self.peek(), Tok::Colon) {
                let _variable = self.name()?;
            }
            if self.eat(&Tok::Colon) || self.eat_word("IS") {
                let name = self.name()?;
                if matches!(self.peek(), Tok::Pipe) {
                    return Err(SqlError::Refused {
                        keyword: "label alternation".into(),
                        tier: super::Tier::Two,
                        reason: "QL_CONTRACT §4.3: label alternation `a|b` is a multi-type hop (two ranges per hop); not built in this slice.",
                    });
                }
                edge_type = Some(name);
            }
            if self.word().as_deref() == Some("WHERE") {
                return Err(SqlError::Refused {
                    keyword: "edge inline WHERE".into(),
                    tier: super::Tier::Two,
                    reason: "QL_CONTRACT §4.3: an inline WHERE on an edge element is the per-hop prune over inline edge properties (GRAPH_CONTRACT 4.3); not built in this slice.",
                });
            }
            self.expect(&Tok::RBracket)?;
        }
        // The arrow head decides the direction. After the bracket the text
        // reads `->` (one token) for an outgoing hop and `-` for an
        // undirected one; `<-[..]-` is the incoming form.
        let outgoing = if incoming {
            if !self.eat(&Tok::Minus) {
                self.expect(&Tok::Arrow)?;
            }
            false
        } else if self.eat(&Tok::Arrow) {
            true
        } else {
            self.expect(&Tok::Minus)?;
            self.eat(&Tok::Gt)
        };
        let direction = match (incoming, outgoing) {
            (true, _) => GraphDirection::Incoming,
            (false, true) => GraphDirection::Outgoing,
            (false, false) => GraphDirection::Both,
        };
        let (min_depth, max_depth) = self.quantifier()?;
        Ok(GraphHop {
            edge_type,
            direction,
            min_depth,
            max_depth,
        })
    }

    fn quantifier(&mut self) -> SqlResult2<(usize, usize)> {
        if self.eat(&Tok::LBrace) {
            let min = match self.bump() {
                Tok::Num(n, true) => n as usize,
                other => {
                    return Err(SqlError::syntax(
                        format!("expected a depth, found `{}`", other.written()),
                        self.here(),
                    ))
                }
            };
            let max = if self.eat(&Tok::Comma) {
                if matches!(self.peek(), Tok::RBrace) {
                    super::MAX_GRAPH_DEPTH
                } else {
                    match self.bump() {
                        Tok::Num(n, true) => n as usize,
                        other => {
                            return Err(SqlError::syntax(
                                format!("expected a depth, found `{}`", other.written()),
                                self.here(),
                            ))
                        }
                    }
                }
            } else {
                min
            };
            self.expect(&Tok::RBrace)?;
            return Ok((min.max(1), max));
        }
        if self.eat(&Tok::Plus) {
            return Ok((1, super::MAX_GRAPH_DEPTH));
        }
        if self.eat(&Tok::Question) {
            return Ok((1, 1));
        }
        Ok((1, 1))
    }

    fn literal(&mut self) -> SqlResult2<Literal> {
        self.deeper()?;
        let result = self.literal_inner();
        self.shallower();
        result
    }

    fn literal_inner(&mut self) -> SqlResult2<Literal> {
        if self.eat(&Tok::Minus) {
            return match self.literal()? {
                Literal::Num(value, exact) => Ok(Literal::Num(-value, exact)),
                other => Err(SqlError::unsupported(format!(
                    "unary minus applies to a number, not to {other:?}"
                ))),
            };
        }
        if matches!(self.peek(), Tok::LParen) && self.word_at(1).as_deref() == Some("SELECT") {
            self.bump();
            self.bump();
            let column = self.name()?;
            self.expect_word("FROM")?;
            let table = self.name()?;
            self.expect_word("WHERE")?;
            let key_column = self.name()?;
            if !super::is_key_column(&key_column) {
                return Err(SqlError::unsupported(format!(
                    "a scalar subquery reads ONE row by key: write `WHERE {} = ...`",
                    super::KEY_COLUMN
                )));
            }
            self.expect(&Tok::Eq)?;
            let key = self.literal()?;
            self.expect(&Tok::RParen)?;
            return Ok(Literal::Subquery(Box::new(ScalarSubquery {
                column,
                table,
                key,
            })));
        }
        let at = self.here();
        let literal = match self.peek().clone() {
            Tok::Num(value, exact) => {
                self.bump();
                Literal::Num(value, exact)
            }
            Tok::Str(text) => {
                self.bump();
                Literal::Str(text)
            }
            Tok::Param(n) => {
                self.bump();
                Literal::Param(n)
            }
            Tok::Word(word) => match word.to_ascii_uppercase().as_str() {
                "NULL" => {
                    self.bump();
                    Literal::Null
                }
                "TRUE" => {
                    self.bump();
                    Literal::Bool(true)
                }
                "FALSE" => {
                    self.bump();
                    Literal::Bool(false)
                }
                other => {
                    return Err(match self.listed(other) {
                        Some(error) => error,
                        None => SqlError::syntax(
                            format!("expected a literal or a parameter, found `{word}`"),
                            at,
                        ),
                    })
                }
            },
            other => {
                return Err(SqlError::syntax(
                    format!("expected a literal or a parameter, found `{}`", other.written()),
                    at,
                ))
            }
        };
        self.optional_cast()?;
        Ok(literal)
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
