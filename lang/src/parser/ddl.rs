use super::*;
use sekejap_core::collections::{ColumnRule, DefaultValue};

impl Parser {
    pub(super) fn create(&mut self) -> SqlResult2<Stmt> {
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
        // The catalog probe this needs is `Database::collection(name)`, which
        // answers `Option<CollectionId>` without reading a row: the same
        // probe `DROP TABLE IF EXISTS` uses, run the other way round.
        let if_not_exists = if self.eat_word("IF") {
            self.expect_word("NOT")?;
            self.expect_word("EXISTS")?;
            true
        } else {
            false
        };
        let table = self.name()?;
        self.expect(&Tok::LParen)?;
        let mut columns = Vec::new();
        loop {
            columns.push(self.column_def()?);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen)?;
        Ok(Stmt::CreateTable {
            table,
            columns,
            if_not_exists,
        })
    }

    /// One column: a name, a type, and the clauses after it. `DEFAULT` and
    /// `NOT NULL` become the COLUMN RULE the descriptor records
    /// (QL_CONTRACT §2); the rest of the clause set has no atomic and is
    /// refused by name.
    fn column_def(&mut self) -> SqlResult2<ColumnDef> {
        let name = self.name()?;
        let (kind, declared) = self.column_type()?;
        let mut primary_key = false;
        let mut rule = ColumnRule::default();
        loop {
            match self.word().as_deref() {
                Some("PRIMARY") => {
                    self.bump();
                    self.expect_word("KEY")?;
                    primary_key = true;
                }
                Some("NOT") if self.word_at(1).as_deref() == Some("NULL") => {
                    self.bump();
                    self.bump();
                    rule.not_null = true;
                }
                // `NULL` written out is Postgres's way of saying "the
                // default nullability", and it decides nothing here either.
                Some("NULL") => {
                    self.bump();
                }
                Some("DEFAULT") => {
                    self.bump();
                    rule.default = Some(self.default_generator()?);
                }
                Some("UNIQUE") | Some("REFERENCES") | Some("CHECK") => {
                    return Err(SqlError::unsupported(format!(
                        "column constraint `{}`: the descriptor's per-field slot holds a DEFAULT generator and a NOT NULL flag, and nothing else",
                        self.peek().written()
                    )));
                }
                Some("GENERATED") => {
                    return Err(SqlError::Refused {
                        keyword: "GENERATED ALWAYS".to_owned(),
                        tier: super::Tier::Two,
                        reason: "QL_CONTRACT §2: a generated column is a compiled row expression in the same descriptor slot, evaluated before the index-maintenance hook; the slot holds the closed generator set today.",
                    });
                }
                _ => break,
            }
        }
        Ok(ColumnDef {
            name,
            kind,
            declared,
            primary_key,
            rule: (rule != ColumnRule::default()).then_some(rule),
        })
    }

    /// The generator a `DEFAULT` names. The set is CLOSED (QL_CONTRACT §2):
    /// `now()`, `uuid4()`, `uuid5(namespace, name)` and the Postgres
    /// spellings of those three. Anything else -- a literal, an arithmetic
    /// expression, a call this engine does not have -- is refused by name,
    /// because a default that is an expression is the generated-column row
    /// of the contract and has no write-path atomic.
    fn default_generator(&mut self) -> SqlResult2<DefaultValue> {
        let at = self.here();
        let Some(word) = self.word() else {
            return Err(SqlError::unsupported(format!(
                "DEFAULT {}: the generator set is closed -- now(), uuid4(), uuid5(namespace, name) -- and an arbitrary expression is the GENERATED ALWAYS row of QL_CONTRACT §2",
                self.peek().written()
            )));
        };
        self.bump();
        let no_args = |p: &mut Self| -> SqlResult2<()> {
            if p.eat(&Tok::LParen) {
                p.expect(&Tok::RParen)?;
            }
            Ok(())
        };
        match word.as_str() {
            "NOW" | "CURRENT_TIMESTAMP" | "TRANSACTION_TIMESTAMP" | "STATEMENT_TIMESTAMP" => {
                no_args(self)?;
                Ok(DefaultValue::Now)
            }
            "UUID4" | "GEN_RANDOM_UUID" | "UUID_GENERATE_V4" => {
                no_args(self)?;
                Ok(DefaultValue::Uuid4)
            }
            "UUID5" | "UUID_GENERATE_V5" => {
                self.expect(&Tok::LParen)?;
                let namespace = match self.bump() {
                    Tok::Str(text) => text,
                    other => {
                        return Err(SqlError::syntax(
                            format!(
                                "uuid5 takes a namespace UUID written as a string, found `{}`",
                                other.written()
                            ),
                            at,
                        ))
                    }
                };
                self.expect(&Tok::Comma)?;
                let name = match self.bump() {
                    Tok::Str(text) => text,
                    other => {
                        return Err(SqlError::syntax(
                            format!(
                                "uuid5 takes a name written as a string, found `{}`",
                                other.written()
                            ),
                            at,
                        ))
                    }
                };
                self.expect(&Tok::RParen)?;
                Ok(DefaultValue::Uuid5 {
                    namespace: sekejap_core::internal::parse_uuid(&namespace)
                        .map_err(|e| SqlError::unsupported(format!("uuid5 namespace: {e}")))?,
                    name,
                })
            }
            other => Err(SqlError::unsupported(format!(
                "DEFAULT {other}: the generator set is closed -- now(), uuid4(), uuid5(namespace, name) -- and each member is O(1) per row; an arbitrary expression is the GENERATED ALWAYS row of QL_CONTRACT §2"
            ))),
        }
    }

    /// `ALTER TABLE t <action>`: the four descriptor rewrites of
    /// QL_CONTRACT §2, plus `ALTER COLUMN ... TYPE` within one `Kind`.
    pub(super) fn alter(&mut self) -> SqlResult2<Stmt> {
        self.expect_word("ALTER")?;
        match self.word().as_deref() {
            Some("TABLE") => {}
            Some(other) => {
                return Err(SqlError::unsupported(format!(
                    "ALTER {other}: ALTER TABLE is the catalog's own; there is no other alterable object"
                )))
            }
            None => {
                return Err(SqlError::syntax(
                    format!("expected TABLE after ALTER, found `{}`", self.peek().written()),
                    self.here(),
                ))
            }
        }
        self.bump();
        if self.eat_word("IF") {
            self.expect_word("EXISTS")?;
            return Err(SqlError::unsupported(
                "ALTER TABLE IF EXISTS: `Database::alter_collection` takes a CollectionId and there is no catalog probe that makes the rewrite conditional",
            ));
        }
        if self.eat_word("ONLY") {
            return Err(SqlError::unsupported(
                "ALTER TABLE ONLY: there is no inheritance here, so ONLY names a distinction the catalog has not got",
            ));
        }
        let table = self.name()?;
        let action = self.alter_action()?;
        if self.eat(&Tok::Comma) {
            return Err(SqlError::unsupported(
                "two actions in one ALTER TABLE: each action is its own `alter_collection` commit, so a comma would hide a partial rewrite; write them as separate statements",
            ));
        }
        Ok(Stmt::AlterTable { table, action })
    }

    fn alter_action(&mut self) -> SqlResult2<AlterAction> {
        match self.word().as_deref() {
            Some("ADD") => {
                self.bump();
                if let Some(what @ ("CONSTRAINT" | "PRIMARY" | "UNIQUE" | "FOREIGN" | "CHECK"
                | "EXCLUDE")) = self.word().as_deref()
                {
                    return Err(SqlError::unsupported(format!(
                        "ALTER TABLE ... ADD {what}: the descriptor's per-field slot holds a DEFAULT generator and a NOT NULL flag; the forms are ADD COLUMN, DROP COLUMN, RENAME COLUMN, RENAME TO and ALTER COLUMN ... TYPE (QL_CONTRACT §2)"
                    )));
                }
                let _ = self.eat_word("COLUMN");
                if self.eat_word("IF") {
                    self.expect_word("NOT")?;
                    self.expect_word("EXISTS")?;
                    return Err(SqlError::unsupported(
                        "ADD COLUMN IF NOT EXISTS: the rewrite is refused for a duplicate name and there is no probe that makes it conditional",
                    ));
                }
                Ok(AlterAction::AddColumn(Box::new(self.column_def()?)))
            }
            Some("DROP") => {
                self.bump();
                let _ = self.eat_word("COLUMN");
                let if_exists = if self.eat_word("IF") {
                    self.expect_word("EXISTS")?;
                    true
                } else {
                    false
                };
                let column = self.name()?;
                if self.eat_word("CASCADE") {
                    return Err(SqlError::unsupported(
                        "DROP COLUMN ... CASCADE: a dependent object here is an INDEX over the column, and dropping it silently is what CASCADE would do; DROP INDEX first",
                    ));
                }
                let _ = self.eat_word("RESTRICT");
                Ok(AlterAction::DropColumn { column, if_exists })
            }
            Some("RENAME") => {
                self.bump();
                if self.eat_word("TO") {
                    return Ok(AlterAction::RenameTable { to: self.name()? });
                }
                let _ = self.eat_word("COLUMN");
                let from = self.name()?;
                self.expect_word("TO")?;
                Ok(AlterAction::RenameColumn {
                    from,
                    to: self.name()?,
                })
            }
            Some("ALTER") => {
                self.bump();
                let _ = self.eat_word("COLUMN");
                let column = self.name()?;
                if self.eat_word("SET") {
                    if self.eat_word("DATA") {
                        self.expect_word("TYPE")?;
                    } else {
                        return Err(SqlError::unsupported(format!(
                            "ALTER COLUMN {column} SET ...: the descriptor slot is written whole by ADD COLUMN and ALTER COLUMN ... TYPE; there is no per-clause SET"
                        )));
                    }
                } else {
                    self.expect_word("TYPE")?;
                }
                let (kind, declared) = self.column_type()?;
                if self.eat_word("USING") {
                    return Err(SqlError::unsupported(format!(
                        "ALTER COLUMN {column} TYPE ... USING <expr>: a USING clause rewrites every row through an expression, and no bounded resumable rewrite exists (QL_CONTRACT §2)"
                    )));
                }
                Ok(AlterAction::ColumnType {
                    column,
                    kind,
                    declared,
                })
            }
            _ => Err(SqlError::unsupported(format!(
                "ALTER TABLE ... {}: the forms are ADD COLUMN, DROP COLUMN, RENAME COLUMN, RENAME TO and ALTER COLUMN ... TYPE (QL_CONTRACT §2)",
                self.peek().written()
            ))),
        }
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
                    "column type `{other}` has no Kind in docs/core/FORMAT_V1.md"
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
        // `CREATE INDEX i ON t (lower(col))` -- an EXPRESSION index, written
        // the way Postgres writes one. `USING btree (lower(col))` is the same
        // index with the method spelled out (QL_CONTRACT §4.1).
        if !self.eat_word("USING") {
            self.expect(&Tok::LParen)?;
            let method = self.index_expression()?;
            self.expect(&Tok::RParen)?;
            return Ok(Stmt::CreateIndex {
                name,
                table,
                method: if unique {
                    return Err(SqlError::unsupported(
                        "UNIQUE on an expression index: uniqueness over a folded value would refuse two rows that differ, which is not what the statement says",
                    ));
                } else {
                    method
                },
            });
        }
        let method_at = self.here();
        let Some(method) = self.word() else {
            return Err(SqlError::syntax("expected an index method", method_at));
        };
        self.bump();
        self.expect(&Tok::LParen)?;
        let method = match method.as_str() {
            "BTREE" => self.index_expression()?,
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
                    "USING {method}: the index families are btree, gin, gist, exact and quantized (docs/core/SOURCE_LAYOUT.md, `src/index/`)"
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
    /// A btree index's target: a column, or `lower(column)`.
    fn index_expression(&mut self) -> SqlResult2<IndexMethod> {
        if self.word().as_deref().map(str::to_ascii_uppercase).as_deref() == Some("LOWER")
            && matches!(self.peek_at(1), Tok::LParen)
        {
            self.bump();
            self.expect(&Tok::LParen)?;
            let column = self.name()?;
            self.expect(&Tok::RParen)?;
            return Ok(IndexMethod::LowerBtree(column));
        }
        Ok(IndexMethod::Btree(self.name()?))
    }

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

    pub(super) fn simple_config(&mut self) -> SqlResult2<()> {
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

    pub(super) fn drop(&mut self) -> SqlResult2<Stmt> {
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

}
