use super::*;

impl Compiler<'_> {
    // ── writes ───────────────────────────────────────────────────────────

    pub(super) fn insert(
        &mut self,
        table: &str,
        columns: &[String],
        values: &[Vec<Literal>],
    ) -> SqlResult2<WritePlan> {
        let c = collection(self.db, table)?;
        let key_at = match columns.iter().position(|name| is_key_column(name)) {
            Some(at) => at,
            None => {
                // With no `_key` column the FIRST column supplies the key,
                // which is where a `TEXT PRIMARY KEY` is written. It must be
                // TEXT, because an external key is a string.
                let first = columns
                    .first()
                    .ok_or_else(|| SqlError::syntax("INSERT names no columns", 0))?;
                if !matches!(self.kind_of(c, first)?, Kind::Text) {
                    return Err(SqlError::unsupported(format!(
                        "INSERT INTO {table} names no `{KEY_COLUMN}` and its first column `{first}` is not TEXT: `Database::put` takes a string key, so either name `{KEY_COLUMN}` or put the TEXT PRIMARY KEY first"
                    )));
                }
                self.notices.push(format!(
                    "INSERT INTO {table}: the external key comes from `{first}`, the first column; the value is ALSO stored as that declared field (battle50k deviation 13)"
                ));
                0
            }
        };
        let mut rows = Vec::with_capacity(values.len());
        for row in values {
            let key = self.text_of(&row[key_at])?;
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
        Ok(WritePlan::Insert {
            collection: c,
            rows,
        })
    }

    pub(super) fn update(
        &mut self,
        table: &str,
        assignments: &[(String, Literal)],
        key: &Literal,
    ) -> SqlResult2<WritePlan> {
        let c = collection(self.db, table)?;
        let mut patch = Map::new();
        for (column, literal) in assignments {
            if is_key_column(column) {
                return Err(SqlError::unsupported(
                    "UPDATE ... SET _key = ...: the external key is the row's identity; a new key is a new row (INSERT) and the old one is a DELETE",
                ));
            }
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
    fn time_document_value(
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

    fn document_value(&self, kind: Kind, literal: &Literal, column: &str) -> SqlResult2<Value> {
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
                let document = match value {
                    Value::String(text) => serde_json::from_str::<Value>(&text)
                        .map_err(|e| SqlError::Parameter(format!("`{column}`: GeoJSON: {e}")))?,
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
        let mut fields: Vec<String> = Vec::new();
        let mut sets = Vec::with_capacity(assignments.len());
        for (column, value) in assignments {
            if is_key_column(column) || column == ID_COLUMN {
                return Err(SqlError::unsupported(
                    "UPDATE ... SET _key = ...: the external key is the row's identity; a new key is a new row (INSERT) and the old one is a DELETE",
                ));
            }
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
