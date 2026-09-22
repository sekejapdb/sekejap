//! The `Rows` DRIVER: a bounded in-memory row source the compiler builds at
//! prepare, and the one thing this catalog surface adds to the plan.
//!
//! It is named and bounded here so the addition is stated rather than
//! inferred. A `SELECT` over a catalog relation is an ORDINARY `SELECT`: the
//! select list, `WHERE`, `ORDER BY`, `LIMIT` and `DISTINCT` are the same
//! clauses the parser already produced, and they are applied over a
//! `Vec<Vec<SqlValue>>` instead of over a driver's candidates.
//!
//! **The bound.** The `Vec` is built by `catalog::build`, whose length is a
//! function of the CATALOG -- collections, declared fields, indexes,
//! interned contexts, the graph-shape triples -- and never of the rows in
//! the collections. It is materialised ONCE, at prepare; running the
//! statement walks it and nothing else, so `QueryWork::candidates` is the
//! row count and every other counter is zero. There is no cursor, no page
//! and no resume: the whole relation fits in the answer by construction, and
//! anything that did not would not be a catalog.
//!
//! **What this is NOT.** It is not a second planner. A predicate here is
//! evaluated per row by [`RowFilter::matches`] because there is no index
//! over a list that was built a microsecond ago; a predicate SHAPE that this
//! evaluator does not have (a text search, a geometry, a traversal, a
//! semi-join) is REFUSED by name rather than approximated, exactly as it is
//! over a stored collection.

use super::*;
use crate::catalog::{self, CatalogRelation};

/// One compiled predicate over the virtual relation's columns.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RowFilter {
    /// `column <op> value`.
    Compare {
        at: usize,
        op: Cmp2,
        value: SqlValue,
    },
    /// `column IN (v1, v2, ...)`.
    In { at: usize, values: Vec<SqlValue> },
    /// `column BETWEEN lower AND upper`.
    Between {
        at: usize,
        lower: SqlValue,
        upper: SqlValue,
    },
    IsNull { at: usize },
    /// A predicate with no column: `1 <> 1`.
    Constant(bool),
    Not(Box<RowFilter>),
    All(Vec<RowFilter>),
    Any(Vec<RowFilter>),
}

/// The six comparisons, as this evaluator holds them. Named apart from the
/// engine's `Cmp` (an edge-property comparison) so the two cannot be mixed
/// up at a call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cmp2 {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Cmp2 {
    pub(crate) fn written(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Ne => "<>",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        }
    }
}

/// Compare two projected values the way SQL orders them.
///
/// `None` when the two are not comparable -- a text against a number, or
/// either side NULL or MISSING. A comparison that is not comparable is
/// FALSE, never an error and never a silent coercion: that is PostgreSQL's
/// three-valued logic reduced to the two values a filter has.
fn compare(left: &SqlValue, right: &SqlValue) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    match (left, right) {
        (SqlValue::Null | SqlValue::Missing, _) | (_, SqlValue::Null | SqlValue::Missing) => None,
        (SqlValue::Bool(a), SqlValue::Bool(b)) => Some(a.cmp(b)),
        (SqlValue::Text(a), SqlValue::Text(b)) => Some(a.as_str().cmp(b.as_str())),
        (SqlValue::Id(a), SqlValue::Id(b)) => Some(a.cmp(b)),
        (SqlValue::Int(a), SqlValue::Int(b)) => Some(a.cmp(b)),
        (SqlValue::Int(a), SqlValue::Float(b)) => (*a as f64).partial_cmp(b),
        (SqlValue::Float(a), SqlValue::Int(b)) => a.partial_cmp(&(*b as f64)),
        (SqlValue::Float(a), SqlValue::Float(b)) => a.partial_cmp(b),
        // A driver writes a catalog filter as a string even when the column
        // is a number (`WHERE attrelid = '16385'`), so a number against the
        // text of that number is the comparison it meant.
        (SqlValue::Int(a), SqlValue::Text(b)) => b.parse::<i64>().ok().map(|b| a.cmp(&b)),
        (SqlValue::Text(a), SqlValue::Int(b)) => a.parse::<i64>().ok().map(|a| a.cmp(b)),
        (SqlValue::Bool(a), SqlValue::Text(b)) => {
            parse_bool(b).map(|b| a.cmp(&b)).or(Some(Ordering::Less))
        }
        (SqlValue::Text(a), SqlValue::Bool(b)) => {
            parse_bool(a).map(|a| a.cmp(b)).or(Some(Ordering::Less))
        }
        _ => None,
    }
}

/// PostgreSQL's boolean input spellings, which is what a catalog filter
/// written as a string carries (`WHERE attnotnull = 't'`).
fn parse_bool(text: &str) -> Option<bool> {
    match text.to_ascii_lowercase().as_str() {
        "t" | "true" | "yes" | "on" | "1" => Some(true),
        "f" | "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

impl RowFilter {
    fn matches(&self, row: &[SqlValue]) -> bool {
        match self {
            Self::Constant(value) => *value,
            Self::Compare { at, op, value } => {
                let Some(ordering) = compare(&row[*at], value) else {
                    return false;
                };
                match op {
                    Cmp2::Eq => ordering.is_eq(),
                    Cmp2::Ne => ordering.is_ne(),
                    Cmp2::Lt => ordering.is_lt(),
                    Cmp2::Le => ordering.is_le(),
                    Cmp2::Gt => ordering.is_gt(),
                    Cmp2::Ge => ordering.is_ge(),
                }
            }
            Self::In { at, values } => values
                .iter()
                .any(|value| compare(&row[*at], value).is_some_and(|o| o.is_eq())),
            Self::Between { at, lower, upper } => {
                compare(&row[*at], lower).is_some_and(|o| o.is_ge())
                    && compare(&row[*at], upper).is_some_and(|o| o.is_le())
            }
            Self::IsNull { at } => matches!(row[*at], SqlValue::Null | SqlValue::Missing),
            Self::Not(inner) => !inner.matches(row),
            Self::All(parts) => parts.iter().all(|part| part.matches(row)),
            Self::Any(parts) => parts.iter().any(|part| part.matches(row)),
        }
    }

    /// The predicate as a statement would have written it, for EXPLAIN.
    fn written(&self, relation: &CatalogRelation) -> String {
        let column = |at: &usize| relation.columns[*at].name;
        match self {
            Self::Constant(value) => format!("{value}"),
            Self::Compare { at, op, value } => {
                format!("{} {} {}", column(at), op.written(), shown(value))
            }
            Self::In { at, values } => format!(
                "{} IN ({})",
                column(at),
                values.iter().map(shown).collect::<Vec<_>>().join(", ")
            ),
            Self::Between { at, lower, upper } => format!(
                "{} BETWEEN {} AND {}",
                column(at),
                shown(lower),
                shown(upper)
            ),
            Self::IsNull { at } => format!("{} IS NULL", column(at)),
            Self::Not(inner) => format!("NOT ({})", inner.written(relation)),
            Self::All(parts) => parts
                .iter()
                .map(|part| part.written(relation))
                .collect::<Vec<_>>()
                .join(" AND "),
            Self::Any(parts) => format!(
                "({})",
                parts
                    .iter()
                    .map(|part| part.written(relation))
                    .collect::<Vec<_>>()
                    .join(" OR ")
            ),
        }
    }
}

/// One item of a catalog SELECT's list: a column of the relation, or a §4.1
/// row function over its columns.
///
/// A `CompiledRow`'s `Field { at }` is a position in the values it is handed,
/// and what it is handed here is the WHOLE relation row -- so `at` is the
/// relation's own column index and a function can read a column the select
/// list does not return.
#[derive(Clone, Debug, PartialEq)]
enum CatalogOutput {
    Column(usize),
    Row(CompiledRow),
}

/// A §4.1 row expression over a catalog relation's columns.
///
/// The set accepted is the STRING functions and literals: those read one
/// value and return one value, which is all a row of a catalog view has.
/// A §4.2 date/time function, a cast and arithmetic are refused by name --
/// a catalog column is text, a name or a small integer, and there is no
/// declared TIMESTAMPTZ among them for a time function to mean anything on.
fn catalog_row_function(
    relation: &CatalogRelation,
    expr: &RowExpr,
) -> SqlResult2<CompiledRow> {
    Ok(match expr {
        RowExpr::Column(name) => CompiledRow::Field {
            at: relation.column_at(name).ok_or_else(|| {
                SqlError::engine(format!(
                    "`{}` has no column `{name}`",
                    relation.written()
                ))
            })?,
            time: false,
        },
        RowExpr::Lit(Literal::Str(text)) => CompiledRow::Lit(SqlValue::Text(text.clone())),
        RowExpr::Lit(Literal::Num(value, exact)) => CompiledRow::Lit(if *exact {
            SqlValue::Int(*value as i64)
        } else {
            SqlValue::Float(*value)
        }),
        RowExpr::Lit(Literal::Bool(b)) => CompiledRow::Lit(SqlValue::Bool(*b)),
        RowExpr::Lit(Literal::Null) => CompiledRow::Lit(SqlValue::Null),
        RowExpr::Str { func, args } => CompiledRow::Str {
            func: *func,
            args: args
                .iter()
                .map(|arg| catalog_row_function(relation, arg))
                .collect::<SqlResult2<Vec<_>>>()?,
        },
        RowExpr::Concat(a, b) => CompiledRow::Concat(
            Box::new(catalog_row_function(relation, a)?),
            Box::new(catalog_row_function(relation, b)?),
        ),
        RowExpr::CastText(arg) => {
            CompiledRow::CastText(Box::new(catalog_row_function(relation, arg)?))
        }
        other => {
            return Err(SqlError::unsupported(format!(
                "`{}` over a catalog view: its columns are text, names and small integers, so the row functions over them are the §4.1 STRING set, `||` and `::text`. A §4.2 date/time function, a cast to date and arithmetic have nothing here to act on",
                other.written()
            )))
        }
    })
}

/// One value, as an EXPLAIN line and a `SHOW CREATE TABLE` write it.
fn shown(value: &SqlValue) -> String {
    match value {
        SqlValue::Missing => "MISSING".into(),
        SqlValue::Null => "NULL".into(),
        SqlValue::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        SqlValue::Int(i) => i.to_string(),
        SqlValue::Float(f) => f.to_string(),
        SqlValue::Text(t) => format!("'{t}'"),
        SqlValue::Json(j) => j.to_string(),
        SqlValue::Id(id) => format!("{}:{}", id.collection.0, id.sequence),
    }
}

/// A compiled statement whose driver is a bounded in-memory row list.
///
/// Everything is decided at PREPARE: the rows are built, filtered, ordered,
/// deduplicated and truncated there, so running the statement hands back
/// what is already here. That is also why a `$n` inside such a statement
/// marks it not rebindable -- there is no slot left to refill, the value
/// having chosen which rows exist.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RowsPlan {
    pub(crate) columns: Vec<String>,
    pub(crate) rows: Vec<Vec<SqlValue>>,
    /// What EXPLAIN calls the driver: the relation and how many rows the
    /// catalog produced before the filters.
    pub(crate) source: String,
    /// The statement as written, for the EXPLAIN header.
    pub(crate) text: String,
    /// One line per applied filter, for EXPLAIN.
    pub(crate) filters: Vec<String>,
    pub(crate) order: Option<String>,
    pub(crate) limit: Option<usize>,
    pub(crate) distinct: bool,
    /// True when this plan is an `EXPLAIN` of itself rather than the answer.
    pub(crate) explain: bool,
}

impl RowsPlan {
    /// The answer, as rows with a synthetic identity.
    ///
    /// A virtual row has no `EntityId`: it is not in any collection and
    /// there is nothing to read it back by. The id reported is
    /// `(collection 0, ordinal)`, which is the position in the answer -- a
    /// caller that treats it as a row identity and asks for it back gets
    /// nothing, which is the truthful outcome for a row that is not stored.
    pub(crate) fn answer(&self) -> SqlResult {
        SqlResult::Rows {
            columns: self.columns.clone(),
            rows: self
                .rows
                .iter()
                .enumerate()
                .map(|(at, values)| SqlRow {
                    id: EntityId {
                        collection: CollectionId(0),
                        sequence: at as u64,
                    },
                    values: values.clone(),
                })
                .collect(),
        }
    }

    /// The plan, printed. Same sections an ordinary SELECT's EXPLAIN has,
    /// with the driver named as what it is.
    pub(crate) fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("statement: {}\n", self.text));
        out.push_str(&format!("driver: {}\n", self.source));
        out.push_str(&format!(
            "columns: {}\n",
            if self.columns.is_empty() {
                "(none)".to_owned()
            } else {
                self.columns.join(", ")
            }
        ));
        if self.filters.is_empty() {
            out.push_str("filters: (none)\n");
        } else {
            out.push_str("filters:\n");
            for filter in &self.filters {
                out.push_str(&format!("  {filter}\n"));
            }
        }
        out.push_str(&format!(
            "order: {}\n",
            self.order.clone().unwrap_or_else(|| "catalog order".into())
        ));
        out.push_str(&format!("distinct: {}\n", self.distinct));
        out.push_str(&format!(
            "limit: {}\n",
            self.limit.map_or_else(|| "none".to_owned(), |n| n.to_string())
        ));
        out.push_str(&format!("rows: {}\n", self.rows.len()));
        out.push_str(
            "work: candidates = the rows above; no index is opened, no row is read, and the list is built once at prepare from the catalog readers (QL_CONTRACT §1, row \"catalog\")\n",
        );
        out
    }
}

impl Compiler<'_> {
    /// The catalog relation a `SELECT`'s `FROM` names, if it names one.
    pub(super) fn catalog_source(select: &SelectStmt) -> Option<&'static CatalogRelation> {
        match &select.source {
            Source::Table(name) => catalog::relation(name),
            _ => None,
        }
    }

    /// Compile a `SELECT` over a catalog relation.
    pub(super) fn catalog_select(
        &mut self,
        relation: &'static CatalogRelation,
        select: SelectStmt,
        explain: bool,
    ) -> SqlResult2<RowsPlan> {
        if select.group.is_some() || !select.having.is_empty() {
            return Err(SqlError::unsupported(format!(
                "GROUP BY / HAVING over `{}`: the catalog views are a bounded in-memory row list with no aggregate driver, so a folded answer over them has no atomic. Read the rows and fold them in the caller",
                relation.written()
            )));
        }
        if select
            .items
            .iter()
            .any(|(item, _)| matches!(item, SelectItem::Aggregate { .. }))
        {
            return Err(SqlError::unsupported(format!(
                "an aggregate over `{}`: the catalog views are a bounded in-memory row list with no aggregate driver. Read the rows and fold them in the caller",
                relation.written()
            )));
        }
        let (built, notice) = catalog::build(self.db, relation)?;
        if let Some(notice) = notice {
            self.notices.push(notice);
        }
        let total = built.len();

        // ── the select list ──────────────────────────────────────────────
        let mut columns = Vec::new();
        let mut picks: Vec<CatalogOutput> = Vec::new();
        for (item, alias) in &select.items {
            match item {
                SelectItem::Star => {
                    for (at, column) in relation.columns.iter().enumerate() {
                        picks.push(CatalogOutput::Column(at));
                        columns.push(column.name.to_owned());
                    }
                }
                SelectItem::Column(name) => {
                    let at = self.catalog_column(relation, name)?;
                    picks.push(CatalogOutput::Column(at));
                    columns.push(alias.clone().unwrap_or_else(|| name.clone()));
                }
                // A §4.1 STRING function over a catalog column is the row
                // function it is anywhere else -- one row in, one value out,
                // no read of any other row -- so it is compiled to the same
                // `CompiledRow` a stored collection's select list uses. QGIS
                // writes `upper(type)` over `geometry_columns`.
                SelectItem::Function(expr) => {
                    picks.push(CatalogOutput::Row(catalog_row_function(relation, expr)?));
                    columns.push(alias.clone().unwrap_or_else(|| expr.written()));
                }
                other => {
                    return Err(SqlError::unsupported(format!(
                        "`{}` answers columns of the catalog, so its select list is `*`, its own column names, or a §4.1 string function over one of them; {other:?} is none of those",
                        relation.written()
                    )))
                }
            }
        }

        // ── WHERE ────────────────────────────────────────────────────────
        let mut filters = Vec::new();
        for predicate in &select.predicates {
            filters.push(self.catalog_filter(relation, predicate)?);
        }
        let written: Vec<String> = filters.iter().map(|f| f.written(relation)).collect();
        let mut rows: Vec<Vec<SqlValue>> = built
            .into_iter()
            .filter(|row| filters.iter().all(|filter| filter.matches(row)))
            .collect();

        // ── ORDER BY ─────────────────────────────────────────────────────
        let mut order = None;
        if let Some(key) = &select.order {
            let OrderKey::Column { column, descending } = key else {
                return Err(SqlError::unsupported(format!(
                    "`{}` is ordered by one of its own columns: a distance, a vector or a ranking key names an index, and a catalog view has none",
                    relation.written()
                )));
            };
            let at = self.catalog_column(relation, column)?;
            rows.sort_by(|a, b| {
                compare(&a[at], &b[at])
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| shown(&a[at]).cmp(&shown(&b[at])))
            });
            if *descending {
                rows.reverse();
            }
            order = Some(format!(
                "{column} {}",
                if *descending { "DESC" } else { "ASC" }
            ));
        }

        // ── the projection, then DISTINCT, then LIMIT ────────────────────
        //
        // In that order because that is the order SQL evaluates them:
        // DISTINCT folds the PROJECTED rows, and LIMIT counts what is left.
        let mut projected: Vec<Vec<SqlValue>> = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut out = Vec::with_capacity(picks.len());
            for pick in &picks {
                out.push(match pick {
                    CatalogOutput::Column(at) => row[*at].clone(),
                    CatalogOutput::Row(expr) => expr.eval(row)?,
                });
            }
            projected.push(out);
        }
        if select.distinct {
            let mut seen: Vec<String> = Vec::new();
            projected.retain(|row| {
                let key = row.iter().map(shown).collect::<Vec<_>>().join("\u{1}");
                if seen.contains(&key) {
                    false
                } else {
                    seen.push(key);
                    true
                }
            });
        }
        if let Some(limit) = select.limit {
            projected.truncate(limit);
        }

        Ok(RowsPlan {
            columns,
            rows: projected,
            source: format!(
                "rows({}): {total} row(s) built from the catalog at prepare",
                relation.written()
            ),
            text: statement_text(relation, &select),
            filters: written,
            order,
            limit: select.limit,
            distinct: select.distinct,
            explain,
        })
    }

    fn catalog_column(&self, relation: &CatalogRelation, name: &str) -> SqlResult2<usize> {
        relation.column_at(name).ok_or_else(|| {
            SqlError::engine(format!(
                "`{}` has no column `{name}`; it has {}",
                relation.written(),
                relation
                    .columns
                    .iter()
                    .map(|c| c.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
    }

    fn catalog_filter(
        &self,
        relation: &CatalogRelation,
        expr: &Expr,
    ) -> SqlResult2<RowFilter> {
        Ok(match expr {
            Expr::Not(inner) => RowFilter::Not(Box::new(self.catalog_filter(relation, inner)?)),
            Expr::And(parts) => RowFilter::All(
                parts
                    .iter()
                    .map(|part| self.catalog_filter(relation, part))
                    .collect::<SqlResult2<Vec<_>>>()?,
            ),
            Expr::Or(parts) => RowFilter::Any(
                parts
                    .iter()
                    .map(|part| self.catalog_filter(relation, part))
                    .collect::<SqlResult2<Vec<_>>>()?,
            ),
            Expr::Leaf(Predicate::Constant(value)) => RowFilter::Constant(*value),
            Expr::Leaf(Predicate::Compare { column, op, value }) => RowFilter::Compare {
                at: self.catalog_column(relation, column)?,
                op: match op {
                    CmpOp::Eq => Cmp2::Eq,
                    CmpOp::Ne => Cmp2::Ne,
                    CmpOp::Lt => Cmp2::Lt,
                    CmpOp::Le => Cmp2::Le,
                    CmpOp::Gt => Cmp2::Gt,
                    CmpOp::Ge => Cmp2::Ge,
                },
                value: self.catalog_value(value)?,
            },
            Expr::Leaf(Predicate::InList { column, values }) => RowFilter::In {
                at: self.catalog_column(relation, column)?,
                values: values
                    .iter()
                    .map(|value| self.catalog_value(value))
                    .collect::<SqlResult2<Vec<_>>>()?,
            },
            Expr::Leaf(Predicate::Between {
                column,
                lower,
                upper,
            }) => RowFilter::Between {
                at: self.catalog_column(relation, column)?,
                lower: self.catalog_value(lower)?,
                upper: self.catalog_value(upper)?,
            },
            Expr::Leaf(Predicate::IsNull { column, negated }) => {
                let filter = RowFilter::IsNull {
                    at: self.catalog_column(relation, column)?,
                };
                if *negated {
                    RowFilter::Not(Box::new(filter))
                } else {
                    filter
                }
            }
            other => {
                return Err(SqlError::unsupported(format!(
                    "a predicate over `{}` is a comparison, IN, BETWEEN or IS NULL on one of its own columns: the rows are a bounded list built at prepare and there is no index over them, so a text search, a geometry, a traversal or a semi-join has nothing here to answer it. Refused rather than answered by a scan with a different meaning: {other:?}",
                    relation.written()
                )))
            }
        })
    }

    /// One written value, as the row list holds values.
    fn catalog_value(&self, literal: &Literal) -> SqlResult2<SqlValue> {
        // A `$n` here chooses which rows the plan HAS, because the filter is
        // applied while the statement compiles; that is a fold, and a fold
        // is what makes a statement not rebindable.
        self.folds(literal, "a filter over a catalog view, which is applied at prepare");
        Ok(match self.value_of(literal)? {
            Value::Null => SqlValue::Null,
            Value::Bool(b) => SqlValue::Bool(b),
            Value::Number(n) => match n.as_i64() {
                Some(i) => SqlValue::Int(i),
                None => SqlValue::Float(n.as_f64().unwrap_or(f64::NAN)),
            },
            Value::String(s) => SqlValue::Text(s),
            other => SqlValue::Json(other),
        })
    }
}

/// The statement, as the EXPLAIN header prints it. Built from the parsed
/// form rather than from the source text, so the two cannot disagree.
fn statement_text(relation: &CatalogRelation, select: &SelectStmt) -> String {
    let mut out = String::from("SELECT ");
    if select.distinct {
        out.push_str("DISTINCT ");
    }
    let items: Vec<String> = select
        .items
        .iter()
        .map(|(item, alias)| match (item, alias) {
            (SelectItem::Star, _) => "*".to_owned(),
            (SelectItem::Column(name), None) => name.clone(),
            (SelectItem::Column(name), Some(alias)) => format!("{name} AS {alias}"),
            _ => "?".to_owned(),
        })
        .collect();
    out.push_str(&items.join(", "));
    out.push_str(&format!(" FROM {}", relation.written()));
    if !select.predicates.is_empty() {
        out.push_str(" WHERE ...");
    }
    if let Some(limit) = select.limit {
        out.push_str(&format!(" LIMIT {limit}"));
    }
    out
}

// ── the FROM-less SELECT, and the SHOW family ────────────────────────────

impl Compiler<'_> {
    /// `SELECT version()`, `SELECT 1`, `SELECT current_schema(), current_user`.
    ///
    /// Every item is a constant, so the plan is one row and the same `Rows`
    /// driver with nothing to walk. A driver asks these before it has a
    /// catalog to ask anything else of.
    pub(super) fn session_rows(
        &mut self,
        items: &[(SessionItem, Option<String>)],
    ) -> SqlResult2<RowsPlan> {
        let mut columns = Vec::new();
        let mut row = Vec::new();
        for (item, alias) in items {
            columns.push(alias.clone().unwrap_or_else(|| item.column()));
            row.push(match item {
                SessionItem::Version => SqlValue::Text(catalog::VERSION.into()),
                SessionItem::DbVersion => SqlValue::Text(catalog::DB_VERSION.into()),
                SessionItem::CurrentSchema => SqlValue::Text(catalog::PUBLIC.into()),
                SessionItem::CurrentDatabase => SqlValue::Text(catalog::DATABASE.into()),
                SessionItem::CurrentUser => SqlValue::Text(catalog::USER.into()),
                // A connection is a process here, so the backend pid IS this
                // process's id -- the same number the operating system would
                // report, not a fabricated one.
                SessionItem::BackendPid => SqlValue::Int(i64::from(std::process::id())),
                SessionItem::Setting(name) => match crate::parser::client_guc(name) {
                    Some(value) => SqlValue::Text(value.into()),
                    None => SqlValue::Null,
                },
                SessionItem::Lit(literal) => self.catalog_value(literal)?,
            });
        }
        let text = format!("SELECT {}", columns.join(", "));
        Ok(RowsPlan {
            source: "rows(session): one row of constants; no catalog is read".into(),
            columns,
            rows: vec![row],
            text,
            filters: Vec::new(),
            order: None,
            limit: None,
            distinct: false,
            explain: false,
        })
    }

    /// The `SHOW` family of `docs/lang/QL_CONTRACT.md` §2: fixed sugar over
    /// the `db_*` rows, one relation each, plus `SHOW <guc>`.
    pub(super) fn show(&mut self, show: &Show) -> SqlResult2<RowsPlan> {
        match show {
            Show::Tables => self.show_relation("db_tables", "SHOW TABLES", None, &[]),
            Show::Edges => self.show_relation("db_edges", "SHOW EDGES", None, &[]),
            Show::Indexes(table) => self.show_relation(
                "db_indexes",
                &match table {
                    Some(table) => format!("SHOW INDEXES ON {table}"),
                    None => "SHOW INDEXES".to_owned(),
                },
                table.as_deref().map(|table| ("table", table.to_owned())),
                // `table` is the filter, so it is not repeated in every row.
                &["name", "family", "field", "expression", "state"],
            ),
            Show::CreateTable(table) => self.show_create_table(table),
            Show::Name(name) => {
                if self.db.collection(name).map_err(SqlError::from)?.is_some() {
                    return self.show_relation(
                        "db_columns",
                        &format!("SHOW {name}"),
                        Some(("table", name.clone())),
                        &["name", "kind", "declared_type", "position", "not_null", "has_default"],
                    );
                }
                let Some(value) = crate::parser::client_guc(name) else {
                    // NOT `SqlError::Engine`: this is a named refusal -- the
                    // word names neither a collection nor a client setting,
                    // and the sentence says which two things it could have
                    // been. `Engine` reaches a client as `XX000
                    // internal_error`, the one code a client retries;
                    // `Unsupported` is `0A000 feature_not_supported`, which
                    // is what QL_CONTRACT §2 and §7 item 9 claim for it.
                    return Err(SqlError::unsupported(format!(
                        "SHOW {name}: there is no collection of that name and no client setting of that name. `SHOW TABLES` lists the collections"
                    )));
                };
                Ok(RowsPlan {
                    columns: vec![name.to_ascii_lowercase()],
                    rows: vec![vec![SqlValue::Text(value.into())]],
                    source: "rows(session): one client setting, answered from a constant".into(),
                    text: format!("SHOW {name}"),
                    filters: Vec::new(),
                    order: None,
                    limit: None,
                    distinct: false,
                    explain: false,
                })
            }
        }
    }

    /// One `SHOW` form: the named `db_*` relation, optionally filtered to one
    /// table, projected to the columns the form prints.
    fn show_relation(
        &mut self,
        relation: &str,
        text: &str,
        filter: Option<(&str, String)>,
        project: &[&str],
    ) -> SqlResult2<RowsPlan> {
        let relation = catalog::relation(relation).expect("a db_* relation this file names");
        let (built, notice) = catalog::build(self.db, relation)?;
        if let Some(notice) = notice {
            self.notices.push(notice);
        }
        let total = built.len();
        let mut written = Vec::new();
        let rows: Vec<Vec<SqlValue>> = match &filter {
            Some((column, value)) => {
                let at = self.catalog_column(relation, column)?;
                written.push(format!("{column} = '{value}'"));
                built
                    .into_iter()
                    .filter(|row| row[at] == SqlValue::Text(value.clone()))
                    .collect()
            }
            None => built,
        };
        let picks: Vec<usize> = if project.is_empty() {  // SHOW projects columns only
            (0..relation.columns.len()).collect()
        } else {
            project
                .iter()
                .map(|name| self.catalog_column(relation, name))
                .collect::<SqlResult2<Vec<_>>>()?
        };
        Ok(RowsPlan {
            columns: picks
                .iter()
                .map(|at| relation.columns[*at].name.to_owned())
                .collect(),
            rows: rows
                .iter()
                .map(|row| picks.iter().map(|at| row[*at].clone()).collect())
                .collect(),
            source: format!(
                "rows({}): {total} row(s) built from the catalog at prepare",
                relation.written()
            ),
            text: text.to_owned(),
            filters: written,
            order: None,
            limit: None,
            distinct: false,
            explain: false,
        })
    }

    /// `SHOW CREATE TABLE t`: the `CREATE TABLE` and the `CREATE INDEX`
    /// statements that would build `t` again.
    ///
    /// Built from the descriptor, never from remembered text, so it cannot
    /// disagree with what is there: the declared spelling of a column comes
    /// from the catalog's own record of it (`CollectionInfo::declared`), and
    /// a column whose declaration was never recorded prints the `Kind`'s own
    /// SQL spelling.
    fn show_create_table(&mut self, table: &str) -> SqlResult2<RowsPlan> {
        let id = crate::collection(self.db, table)?;
        let info = self.db.collection_info(id).map_err(SqlError::from)?;
        // The external key is column one of every collection and is the
        // PRIMARY KEY every constraint view names, so the DDL declares it
        // first -- a statement that replayed this without it would build a
        // collection whose rows had no key.
        let mut lines = vec![format!("  {} TEXT PRIMARY KEY", crate::KEY_COLUMN)];
        for (field, kind) in &info.layout.fields {
            let declared = info
                .declared
                .iter()
                .find(|(name, _)| name == field)
                .map(|(_, declared)| declared.clone())
                .unwrap_or_else(|| catalog::declared_of(kind));
            let mut line = format!("  {field} {declared}");
            if let Some((_, rule)) = info.rules.iter().find(|(name, _)| name == field) {
                if let Some(default) = &rule.default {
                    line.push_str(&format!(" DEFAULT {}", default_spelling(default)));
                }
                if rule.not_null {
                    line.push_str(" NOT NULL");
                }
            }
            lines.push(line);
        }
        let mut ddl = format!("CREATE TABLE {table} (\n{}\n);", lines.join(",\n"));
        for index in self.db.list_indexes(id).map_err(SqlError::from)? {
            ddl.push('\n');
            ddl.push_str(&index_statement(table, &index));
            ddl.push(';');
        }
        Ok(RowsPlan {
            columns: vec!["create_table".into()],
            rows: vec![vec![SqlValue::Text(ddl)]],
            source: format!(
                "rows(db_columns + db_indexes): one collection's descriptor, {} column(s)",
                info.layout.fields.len() + 1
            ),
            text: format!("SHOW CREATE TABLE {table}"),
            filters: Vec::new(),
            order: None,
            limit: None,
            distinct: false,
            explain: false,
        })
    }
}

fn default_spelling(value: &DefaultValue) -> String {
    match value {
        DefaultValue::Now => "now()".into(),
        DefaultValue::Uuid4 => "uuid4()".into(),
        DefaultValue::Uuid5 { .. } => "uuid5(namespace, name)".into(),
    }
}

/// The `CREATE INDEX` that would build one index again.
fn index_statement(table: &str, index: &IndexInfo) -> String {
    let unique = if index.unique { "UNIQUE " } else { "" };
    let target = match &index.expression {
        Some(expression) => format!("{}({})", expression.written(), index.field),
        None => index.field.clone(),
    };
    let method = match index.family {
        IndexFamily::Scalar => format!("btree ({target})"),
        IndexFamily::Text => format!("gin (to_tsvector('simple', {target}))"),
        IndexFamily::SpatialPoint | IndexFamily::SpatialGeometry => format!("gist ({target})"),
        IndexFamily::ExactVector => format!("exact ({target})"),
        IndexFamily::QuantizedVector => format!("quantized ({target} vector_cosine_ops)"),
    };
    format!(
        "CREATE {unique}INDEX {} ON {table} USING {method}",
        index.name
    )
}
