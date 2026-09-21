//! What a SELECT hands back, and how a `serde_json::Value` becomes a
//! statement parameter. `docs/dist/RUST_API.md` §3.

use crate::error::{Error, Result};
use sekejap_core::collections::EntityId;
use sekejap_lang::{Param, SqlResult, SqlRow, SqlValue};
use serde_json::{Map, Value};
use std::sync::Arc;

/// One answer: the column names the statement returns, and its rows.
#[derive(Clone, Debug)]
pub struct Rows {
    /// The columns in select-list order, shared with every row.
    pub columns: Arc<Vec<String>>,
    pub rows: Vec<Row>,
}

impl Rows {
    pub fn len(&self) -> usize {
        self.rows.len()
    }
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
    pub fn iter(&self) -> std::slice::Iter<'_, Row> {
        self.rows.iter()
    }
    /// The column names, for a caller that wants them without the `Arc`.
    pub fn column_names(&self) -> &[String] {
        &self.columns
    }
}

impl IntoIterator for Rows {
    type Item = Row;
    type IntoIter = std::vec::IntoIter<Row>;
    fn into_iter(self) -> Self::IntoIter {
        self.rows.into_iter()
    }
}

/// One row: the engine's identity for it, and one value per column.
///
/// The row carries its own column names so a row handed to
/// [`crate::Db::stream`] can be read by name without the answer around it.
#[derive(Clone, Debug)]
pub struct Row {
    pub columns: Arc<Vec<String>>,
    pub id: EntityId,
    pub values: Vec<SqlValue>,
}

impl Row {
    /// The value under `column`, or `None` if the statement has no such
    /// column. A column that is present but MISSING in this row answers
    /// `Some(&SqlValue::Missing)`: absent from the row is not absent from
    /// the statement.
    pub fn value(&self, column: &str) -> Option<&SqlValue> {
        let at = self.columns.iter().position(|c| c == column)?;
        self.values.get(at)
    }

    /// The value under `column` as JSON. `None` for an unknown column and
    /// for a MISSING one, which have different meanings and the same shape
    /// here on purpose: neither has a value to give.
    pub fn json(&self, column: &str) -> Option<Value> {
        match self.value(column)? {
            SqlValue::Missing => None,
            other => Some(value_to_json(other)),
        }
    }

    /// The row as a JSON object. A MISSING column is OMITTED, because
    /// missing is not null (`docs/lang/QL_CONTRACT.md` §5).
    pub fn to_object(&self) -> Map<String, Value> {
        let mut out = Map::new();
        for (name, value) in self.columns.iter().zip(&self.values) {
            if matches!(value, SqlValue::Missing) {
                continue;
            }
            out.insert(name.clone(), value_to_json(value));
        }
        out
    }
}

/// One `SqlValue` as JSON. `Missing` becomes `null` here; the callers above
/// omit it instead, which is the distinction this function cannot make on
/// its own.
pub fn value_to_json(value: &SqlValue) -> Value {
    match value {
        SqlValue::Missing | SqlValue::Null => Value::Null,
        SqlValue::Bool(b) => Value::Bool(*b),
        SqlValue::Int(i) => Value::from(*i),
        SqlValue::Float(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        SqlValue::Text(s) => Value::String(s.clone()),
        SqlValue::Json(v) => v.clone(),
        SqlValue::Id(id) => Value::String(format!("{}:{}", id.collection.0, id.sequence)),
    }
}

/// One JSON value as a statement parameter, by the rule
/// `docs/dist/RUST_API.md` §3 states: an integer number is `Int`, any other
/// number is `Float`, an array whose members are all numbers is `Vector`,
/// and anything else that is not a scalar is `Json`.
pub fn param_of(value: &Value) -> Param {
    match value {
        Value::Null => Param::Null,
        Value::Bool(b) => Param::Bool(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => Param::Int(i),
            None => Param::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        Value::String(s) => Param::Text(s.clone()),
        Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_number) => {
            Param::Vector(
                items
                    .iter()
                    .map(|v| v.as_f64().unwrap_or(0.0) as f32)
                    .collect(),
            )
        }
        other => Param::Json(other.clone()),
    }
}

/// Every parameter of a call, in order.
pub fn params_of(values: &[Value]) -> Vec<Param> {
    values.iter().map(param_of).collect()
}

/// Build the answer of a row-returning statement.
pub(crate) fn rows_of(columns: Vec<String>, rows: Vec<SqlRow>) -> Rows {
    let columns = Arc::new(columns);
    let rows = rows
        .into_iter()
        .map(|r| Row {
            columns: Arc::clone(&columns),
            id: r.id,
            values: r.values,
        })
        .collect();
    Rows { columns, rows }
}

/// The one place a `SqlResult` that should have been rows is refused by name.
pub(crate) fn expect_rows(result: SqlResult, statement: &str) -> Result<Rows> {
    match result {
        SqlResult::Rows { columns, rows } => Ok(rows_of(columns, rows)),
        SqlResult::Affected(_) => Err(Error::refused(
            format!("Db::query on `{}`", first_words(statement)),
            "the statement writes rows rather than returning them: use Db::execute",
        )),
        SqlResult::Explain(_) => Err(Error::refused(
            format!("Db::query on `{}`", first_words(statement)),
            "an EXPLAIN returns a plan and not rows: use Db::explain",
        )),
        SqlResult::Notice(notice) => Err(Error::refused(
            format!("Db::query on `{}`", first_words(statement)),
            format!("the statement returns no rows: {notice}"),
        )),
    }
}

/// The one place a `SqlResult` that should have been a count is refused.
pub(crate) fn expect_affected(result: SqlResult, statement: &str) -> Result<u64> {
    match result {
        SqlResult::Affected(n) => Ok(n),
        // A notice is a statement that ran and moved no row: `SET LOCAL`, a
        // `BEGIN` on a writer already in a transaction. Zero rows is the
        // truth, not a swallowed error.
        SqlResult::Notice(_) => Ok(0),
        SqlResult::Rows { .. } => Err(Error::refused(
            format!("Db::execute on `{}`", first_words(statement)),
            "the statement returns rows: use Db::query or Db::stream",
        )),
        SqlResult::Explain(_) => Err(Error::refused(
            format!("Db::execute on `{}`", first_words(statement)),
            "an EXPLAIN returns a plan: use Db::explain",
        )),
    }
}

/// The first three words of a statement, for a refusal that names it without
/// quoting a whole query back at the caller.
fn first_words(statement: &str) -> String {
    statement
        .split_whitespace()
        .take(3)
        .collect::<Vec<_>>()
        .join(" ")
}
