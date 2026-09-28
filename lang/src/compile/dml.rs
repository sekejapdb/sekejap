use super::*;

impl Compiler<'_> {
    // ── writes ───────────────────────────────────────────────────────────

    pub(super) fn insert(
        &mut self,
        table: &str,
        columns: &[String],
        values: &[Vec<Literal>],
        on_conflict: Option<ConflictClause>,
        returning: &[ReturningItem],
    ) -> SqlResult2<WritePlan> {
        let c = collection(self.db, table)?;
        let returning = self.returning(c, table, returning)?;
        // Where the key comes from, as the table declares it:
        // `_key` when the statement names it; else the named PRIMARY KEY
        // column; else the key's DEFAULT, minted per row when it runs; else
        // the INSERT has no key, which is PostgreSQL's 23502 -- never a guess
        // at the first column.
        let spec = self.db.collection_info(c).map_err(SqlError::from)?.key;
        let named = spec.as_ref().and_then(|k| k.column.clone());
        let key_at = match columns.iter().position(|name| is_key_column(name)) {
            Some(at) => Some(at),
            None => match named.as_ref().and_then(|col| columns.iter().position(|n| n == col)) {
                Some(at) => Some(at),
                None if spec.as_ref().is_some_and(|k| k.default.is_some()) => None,
                None => {
                    let column = named.clone().unwrap_or_else(|| KEY_COLUMN.to_owned());
                    return Err(SqlError::coded(
                        "23502",
                        format!(
                            "null value in column \"{column}\" of relation \"{table}\" violates not-null constraint: the INSERT names no key; name `{column}`, or declare the key with DEFAULT ulid() / uuid4()"
                        ),
                    ));
                }
            },
        };
        let mut rows = Vec::with_capacity(values.len());
        for row in values {
            let key = match key_at {
                Some(at) => Some(self.text_of(&row[at])?),
                None => None,
            };
            let mut document = Map::new();
            for (at, column) in columns.iter().enumerate() {
                if is_key_column(column) {
                    continue;
                }
                let kind = self.kind_of(c, column)?;
                // A declared TIMESTAMPTZ/DATE column accepts an ISO-8601 or
                // Postgres date/time LITERAL and stores the integer
                // (QL_CONTRACT §4.2); the same column still accepts the
                // integer itself.
                let value = match self.time_column(c, column)? {
                    Some(declared) => self.time_document_value(&row[at], column, &declared)?,
                    None => self.document_value(kind, &row[at], column)?,
                };
                document.insert(column.clone(), value);
            }
            rows.push((key, Value::Object(document)));
        }
        let key_column = columns
            .iter()
            .find(|n| is_key_column(n))
            .cloned()
            .or_else(|| named.clone())
            .unwrap_or_else(|| KEY_COLUMN.to_owned());
        // `ON CONFLICT (_key)`, or the column that supplies the key: the one
        // key a row has. A conflict on another column would need that
        // column's UNIQUE index to find the row, which is not built.
        let on_conflict = match on_conflict {
            None => None,
            Some(clause) => {
                let key_column = &key_column;
                let targets_key = clause.target.len() == 1
                    && (is_key_column(&clause.target[0]) || clause.target[0] == *key_column);
                if !targets_key {
                    return Err(SqlError::unsupported(format!(
                        "ON CONFLICT ({}) on `{table}`: the conflict target of a table of rows is its key, `{KEY_COLUMN}`",
                        clause.target.join(", ")
                    )));
                }
                if let Some(set) = &clause.update {
                    if let Some(bad) = set.iter().find(|c| is_key_column(c) || *c == key_column) {
                        return Err(SqlError::unsupported(format!(
                            "ON CONFLICT DO UPDATE SET {bad}: the key is the row's identity; a new key is a new row"
                        )));
                    }
                }
                Some(clause.update)
            }
        };
        Ok(WritePlan::Insert {
            collection: c,
            rows,
            key_column: named,
            on_conflict,
            returning,
        })
    }

    /// The columns an INSERT's `RETURNING` answers, each with how it is read
    /// back from the stored row: `_key` from the row's key, `*` as the
    /// columns `SELECT *` answers, a declared TIMESTAMPTZ/DATE as the ISO
    /// text a SELECT prints. An unknown column is PostgreSQL's 42703, raised
    /// while the statement compiles, so nothing is written.
    fn returning(
        &self,
        c: CollectionId,
        table: &str,
        items: &[ReturningItem],
    ) -> SqlResult2<Returning> {
        let mut out = Returning::default();
        let column = |name: &str, out: &mut Returning| -> SqlResult2<()> {
            if is_key_column(name) {
                out.push(name, None);
                return Ok(());
            }
            if self.kind_of(c, name).is_err() {
                return Err(SqlError::coded(
                    "42703",
                    format!("column \"{name}\" of relation \"{table}\" does not exist"),
                ));
            }
            let time = self.time_column(c, name)?.map(|declared| declared == "DATE");
            out.push(name, time);
            Ok(())
        };
        for item in items {
            match item {
                ReturningItem::Star => {
                    for name in self.declared_fields(c)? {
                        column(&name, &mut out)?;
                    }
                }
                ReturningItem::Column(name) => column(name, &mut out)?,
            }
        }
        Ok(out)
    }

    pub(super) fn update(
        &mut self,
        table: &str,
        assignments: &[(String, Literal)],
        key: &Literal,
    ) -> SqlResult2<WritePlan> {
        let c = collection(self.db, table)?;
        let declared_key = self.declared_key_column(c)?;
        let mut patch = Map::new();
        for (column, literal) in assignments {
            if is_key_column(column) {
                return Err(SqlError::unsupported(
                    "UPDATE ... SET _key = ...: the external key is the row's identity; a new key is a new row (INSERT) and the old one is a DELETE",
                ));
            }
            refuse_key_assignment(column, declared_key.as_deref())?;
            let kind = self.kind_of(c, column)?;
            let value = match self.time_column(c, column)? {
                Some(declared) => self.time_document_value(literal, column, &declared)?,
                None => self.document_value(kind, literal, column)?,
            };
            patch.insert(column.clone(), value);
        }
        Ok(WritePlan::Update {
            collection: c,
            key: self.text_of(key)?,
            patch: Value::Object(patch),
        })
    }

    /// One written value, checked against the column's declared `Kind`.
    /// A written value for a declared TIMESTAMPTZ/DATE column, stored as the
    /// integer microseconds `docs/lang/QL_CONTRACT.md` §5 deviation 8 pins.
    ///
    /// A `DATE` is midnight UTC of its day, so a literal that carries a time
    /// of day is REFUSED rather than silently truncated: a statement that
    /// wrote one meant a timestamp and the column is not one.
    pub(super) fn time_document_value(
        &self,
        literal: &Literal,
        column: &str,
        declared: &str,
    ) -> SqlResult2<Value> {
        let value = self.value_of(literal)?;
        if value.is_null() {
            return Ok(Value::Null);
        }
        let micros = match &value {
            Value::String(text) => functions::parse_timestamp(text)?,
            Value::Number(n) => n.as_i64().ok_or_else(|| {
                SqlError::Parameter(format!(
                    "`{column}` is declared {declared} and stores whole microseconds; {n} is not a whole number"
                ))
            })?,
            other => {
                return Err(SqlError::Parameter(format!(
                    "`{column}` is declared {declared} and {other} is neither a date/time literal nor a whole number of microseconds"
                )))
            }
        };
        if declared == "DATE" && micros != functions::date_trunc(TimeUnit::Day, micros)? {
            return Err(SqlError::Parameter(format!(
                "`{column}` is declared DATE, which is midnight UTC of its day; `{}` carries a time of day and would be truncated silently",
                value
            )));
        }
        Ok(Value::from(micros))
    }

    pub(super) fn document_value(&self, kind: Kind, literal: &Literal, column: &str) -> SqlResult2<Value> {
        let value = self.value_of(literal)?;
        if value.is_null() {
            return Ok(Value::Null);
        }
        Ok(match kind {
            Kind::Vector(dimensions) => {
                let vector = self.vector_of(literal)?;
                if vector.len() != dimensions {
                    return Err(SqlError::Parameter(format!(
                        "`{column}` is VECTOR({dimensions}) and the value has {} dimension(s)",
                        vector.len()
                    )));
                }
                Value::from(vector)
            }
            Kind::Point | Kind::Geo => {
                // Text is read the way PostgreSQL reads a `geometry` literal:
                // GeoJSON is stored as written, hex EWKB and (E)WKT as the
                // GeoJSON document of the shape they spell.
                let document = match value {
                    Value::String(text) if text.trim_start().starts_with('{') => {
                        serde_json::from_str::<Value>(&text).map_err(|e| {
                            SqlError::Parameter(format!("`{column}`: GeoJSON: {e}"))
                        })?
                    }
                    Value::String(text) => geom_to_json(
                        &geom_from_text(&text)
                            .map_err(|e| SqlError::Parameter(format!("`{column}`: {e}")))?,
                    ),
                    other => other,
                };
                // Parsed once here so a bad geometry is refused by the
                // statement rather than by the index maintenance behind it.
                let geom = geom_from_json(&document)?;
                if kind == Kind::Point && !matches!(geom, Geom::Point(_, _)) {
                    return Err(SqlError::Parameter(format!(
                        "`{column}` is GEOMETRY(Point,4326) and the value is not a Point"
                    )));
                }
                document
            }
            Kind::Int => match &value {
                Value::Number(n) if n.is_i64() => value,
                other => {
                    return Err(SqlError::Parameter(format!(
                        "`{column}` is an integer column and the value is {other}"
                    )))
                }
            },
            Kind::Real => match &value {
                Value::Number(_) => value,
                other => {
                    return Err(SqlError::Parameter(format!(
                        "`{column}` is REAL and the value is {other}"
                    )))
                }
            },
            Kind::Text => match &value {
                Value::String(_) => value,
                other => {
                    return Err(SqlError::Parameter(format!(
                        "`{column}` is TEXT and the value is {other}"
                    )))
                }
            },
            Kind::Bool => match &value {
                Value::Bool(_) => value,
                other => {
                    return Err(SqlError::Parameter(format!(
                        "`{column}` is BOOLEAN and the value is {other}"
                    )))
                }
            },
            Kind::Json => value,
        })
    }

}

// ── predicated writes (QL_CONTRACT §2, `Database::write_where`) ───────────

/// The reason `FROM ALL` carries. Written once because three statements raise
/// it: `SELECT ... FROM ALL`, `DELETE FROM ALL`, and the EXPLAIN of either.
pub(super) const FROM_ALL: &str = "QL_CONTRACT §2: `FROM ALL` is the `CandidateDriver::Collections` concatenation driver -- every collection of the catalog in id order, each an ordinary bounded driver walk, with the resume key `(collection id, inner rank key)` and the collection id on every returned row. The driver is NOT built in this slice. What it needs and this slice does not have: a prepared query compiles its predicates against ONE collection (a `QueryFilter::Scalar` names an `IndexId`, and an `IndexId` belongs to a collection), so `FROM ALL` is one compiled plan per collection plus a refusal naming the collection whose index is missing -- not one plan over a wider driver. Refused rather than emulated as a loop of per-collection statements: that is N statements with N budgets and N cursors, which is neither one bounded walk nor one resumable one.";

impl Compiler<'_> {
    pub(super) fn update_where(
        &mut self,
        table: &str,
        assignments: &[(String, SetValue)],
        predicates: &[Expr],
    ) -> SqlResult2<WritePlan> {
        let c = collection(self.db, table)?;
        let declared_key = self.declared_key_column(c)?;
        let mut fields: Vec<String> = Vec::new();
        let mut sets = Vec::with_capacity(assignments.len());
        for (column, value) in assignments {
            if is_key_column(column) || column == ID_COLUMN {
                return Err(SqlError::unsupported(
                    "UPDATE ... SET _key = ...: the external key is the row's identity; a new key is a new row (INSERT) and the old one is a DELETE",
                ));
            }
            refuse_key_assignment(column, declared_key.as_deref())?;
            let kind = self.kind_of(c, column)?;
            let compiled = match value {
                SetValue::Lit(literal) => CompiledSet::Lit(match self.time_column(c, column)? {
                    Some(declared) => self.time_document_value(literal, column, &declared)?,
                    None => self.document_value(kind, literal, column)?,
                }),
                SetValue::Row(expr) => CompiledSet::Row {
                    expr: self.row_function(c, expr, &mut fields)?,
                    kind,
                },
            };
            sets.push((column.clone(), compiled));
        }
        let mut filters = Vec::with_capacity(predicates.len());
        for expr in predicates {
            filters.push(self.where_filter(c, expr)?);
        }
        Ok(WritePlan::UpdateWhere {
            collection: c,
            filters,
            sets,
            fields,
            driver: CandidateDriver::Auto,
        })
    }

    pub(super) fn delete_where(
        &mut self,
        table: Option<&str>,
        predicates: &[Expr],
        cascade: bool,
    ) -> SqlResult2<WritePlan> {
        let Some(table) = table else {
            return Err(SqlError::Refused {
                keyword: "DELETE FROM ALL".into(),
                tier: Tier::Two,
                reason: FROM_ALL,
            });
        };
        let c = collection(self.db, table)?;
        let mut filters = Vec::with_capacity(predicates.len());
        for expr in predicates {
            filters.push(self.where_filter(c, expr)?);
        }
        if filters.is_empty() {
            self.notices.push(format!(
                "DELETE FROM {table} names no predicate, so every row of the collection matches and the candidate walk is the entity cursor -- a SCAN by definition (QL_CONTRACT §6). `DROP TABLE` removes the collection itself in bounded, resumable steps."
            ));
        }
        Ok(WritePlan::DeleteWhere {
            collection: c,
            filters,
            cascade,
            driver: CandidateDriver::Auto,
        })
    }

    /// `EXPLAIN UPDATE ...` / `EXPLAIN DELETE ...`.
    ///
    /// The one EXPLAIN family besides `DROP TABLE` that does NOT run its
    /// statement, for the same reason: printing the plan of a destructive
    /// statement by executing it is not an explanation. So the candidate
    /// query is PREPARED and described, and no page is walked -- which is
    /// also why the membership sets read "not walked yet".
    pub(super) fn explain_write(&mut self, write: Stmt) -> SqlResult2<String> {
        let (header, plan, extra) = match write {
            Stmt::UpdateWhere {
                table,
                assignments,
                predicates,
            } => {
                let plan = self.update_where(&table, &assignments, &predicates)?;
                let WritePlan::UpdateWhere { sets, .. } = &plan else {
                    unreachable!("update_where compiles to UpdateWhere")
                };
                let mut lines = String::from("set:\n");
                for (column, set) in sets {
                    lines.push_str(&match set {
                        CompiledSet::Lit(value) => {
                            format!("  {column} = {value} (constant, checked at compile)\n")
                        }
                        CompiledSet::Row { .. } => format!(
                            "  {column} = <row function over the same row> (QL_CONTRACT §4.1/§4.2; one evaluation per row WRITTEN)\n"
                        ),
                    });
                }
                (format!("UPDATE {table}"), plan, lines)
            }
            Stmt::DeleteWhere {
                table,
                predicates,
                cascade,
            } => {
                let name = table.clone().unwrap_or_else(|| "ALL".to_owned());
                let plan = self.delete_where(table.as_deref(), &predicates, cascade)?;
                let mode = if cascade {
                    "  mode:  CASCADE -- each row's incident edges are removed in every context (GRAPH_CONTRACT 6.1)\n"
                } else {
                    "  mode:  RESTRICT (the default) -- a row with edges in any context refuses the statement and names the contexts (GRAPH_CONTRACT 6.1)\n"
                };
                (format!("DELETE FROM {name}"), plan, mode.to_owned())
            }
            _ => {
                return Err(SqlError::unsupported(
                    "EXPLAIN takes a SELECT, a DROP TABLE or a predicated UPDATE/DELETE",
                ))
            }
        };
        let (c, filters, driver) = match &plan {
            WritePlan::UpdateWhere {
                collection,
                filters,
                driver,
                ..
            }
            | WritePlan::DeleteWhere {
                collection,
                filters,
                driver,
                ..
            } => (*collection, filters, *driver),
            _ => unreachable!("only the two predicated writes reach here"),
        };
        let description = with_write_filters(filters, &mut |borrowed| {
            let prepared = self.db.prepare_query(QueryRequest {
                collection: c,
                filters: borrowed,
                // The order a write pass asks for: every driver walks in rank
                // order under it, so every page resumes.
                order: QueryOrder::Driver,
                projection: Projection::Ids,
                total_limit: None,
                driver,
            })?;
            Ok(prepared.describe())
        })?;
        let mut out = format!("{header}\n");
        out.push_str(&super::super::explain::format_write_plan(&description));
        out.push_str(&extra);
        out.push_str("rows written: one per candidate, bounded by the caller's `QueryBudget::rows_written`; over it the statement is REFUSED with the count it reached, never truncated\n");
        out.push_str("commits: nothing -- the pass writes into the caller's transaction, which COMMIT or ROLLBACK decides\n");
        out.push_str("note:  this plan was PREPARED, not run: an EXPLAIN that executed a destructive statement would be the statement\n");
        Ok(out)
    }
}

impl Compiler<'_> {
    /// The column a `PRIMARY KEY` declaration made the row's key, if one did.
    fn declared_key_column(&self, c: CollectionId) -> SqlResult2<Option<String>> {
        Ok(self
            .db
            .collection_info(c)
            .map_err(SqlError::from)?
            .key
            .and_then(|k| k.column))
    }
}

/// A declared PRIMARY KEY column supplies the row's key: assigning it would
/// leave the row under its old key with a different or NULL declared one
/// (finding vuln-a08). Refused as `_key` is.
fn refuse_key_assignment(column: &str, declared_key: Option<&str>) -> SqlResult2<()> {
    if declared_key == Some(column) {
        return Err(SqlError::unsupported(format!(
            "UPDATE ... SET {column} = ...: `{column}` is this table's PRIMARY KEY, the row's identity; a new key is a new row (INSERT) and the old one is a DELETE"
        )));
    }
    Ok(())
}
