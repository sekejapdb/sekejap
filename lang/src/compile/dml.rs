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
