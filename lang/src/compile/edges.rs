//! EDGE TABLES in SQL (`docs/core/EDGE_TABLES.md`): the PostgreSQL 19,
//! Oracle 23ai and Spanner statements for a property graph's edges, each
//! compiled to one engine call over native edges.
//!
//! | statement | engine call |
//! |---|---|
//! | `CREATE TABLE t (... REFERENCES v ..., PRIMARY KEY (...))` | `create_collection` + `declare_edge_table` |
//! | `CREATE/ALTER PROPERTY GRAPH g ... EDGE TABLES (t SOURCE KEY ... DESTINATION KEY ...)` | `bind_edge_table` per table |
//! | `DROP PROPERTY GRAPH g` | `drop_property_graph` |
//! | `INSERT INTO t ... [ON CONFLICT ...]` | `insert_edge_row` / `upsert_edge_row` |
//! | `UPDATE t SET ... WHERE <an end> = ...` | `update_edge_rows` |
//! | `DELETE FROM t WHERE <an end> = ...` | `delete_edge_rows` |
//! | `SELECT ... FROM t WHERE <an end> = ...` | `edge_rows`, read when the statement RUNS |
//!
//! Whatever has no such call is refused by name, never emulated.

use super::*;
use sekejap_core::collections::{EdgeTable, OnConflict};

/// The most edges one `SELECT` over an edge table reads: one node's edges
/// of one type. More is an error, never a silent cut.
const MAX_EDGE_ROWS: usize = 65_536;

/// `SELECT ... FROM <edge table> WHERE <an end> = ...`, compiled. The rows
/// are read when the statement runs, so a prepared statement never answers
/// from edges it saw at prepare.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EdgeRowsPlan {
    pub(crate) collection: CollectionId,
    pub(crate) table: String,
    /// `column = value` pairs, one of which names an end.
    pub(crate) filter: Value,
    /// Every column of the table in its order, and for each whether it is a
    /// declared DATE (`Some(true)`) or TIMESTAMPTZ (`Some(false)`).
    pub(crate) table_columns: Vec<(String, Option<bool>)>,
    /// The select list: positions into `table_columns`, and the names the
    /// answer reports.
    pub(crate) picks: Vec<usize>,
    pub(crate) columns: Vec<String>,
    pub(crate) order: Option<(usize, bool)>,
    pub(crate) limit: Option<usize>,
}

impl EdgeRowsPlan {
    pub(crate) fn answer(&self, db: &Database) -> SqlResult2<SqlResult> {
        let found = db
            .edge_rows(self.collection, &self.filter, MAX_EDGE_ROWS)
            .map_err(SqlError::from)?;
        let mut rows: Vec<Vec<SqlValue>> = found
            .iter()
            .map(|row| {
                self.table_columns
                    .iter()
                    .map(|(column, time)| match (row.values.get(column), time) {
                        (Some(Value::Number(n)), Some(date)) if n.as_i64().is_some() => {
                            let micros = n.as_i64().unwrap_or_default();
                            SqlValue::Text(if *date {
                                functions::format_date(micros)
                            } else {
                                functions::format_timestamp(micros)
                            })
                        }
                        (Some(_), _) => plan::field_value(&row.values, column),
                        (None, _) => SqlValue::Null,
                    })
                    .collect()
            })
            .collect();
        if let Some((at, descending)) = self.order {
            rows.sort_by(|a, b| {
                rows::compare(&a[at], &b[at])
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| rows::shown(&a[at]).cmp(&rows::shown(&b[at])))
            });
            if descending {
                rows.reverse();
            }
        }
        if let Some(limit) = self.limit {
            rows.truncate(limit);
        }
        Ok(SqlResult::Rows {
            columns: self.columns.clone(),
            rows: rows
                .into_iter()
                .map(|row| SqlRow {
                    id: EntityId::NO_OWNER,
                    values: self.picks.iter().map(|at| row[*at].clone()).collect(),
                })
                .collect(),
        })
    }

    pub(crate) fn render(&self) -> String {
        format!(
            "statement: SELECT from edge table {}\ndriver: edges of one end (docs/core/EDGE_TABLES.md §5.1), read when the statement runs\nfilter: {}\ncolumns: {}\n",
            self.table,
            self.filter,
            self.columns.join(", ")
        )
    }
}

impl Compiler<'_> {
    /// The edge table `table` names, if it is one.
    pub(super) fn edge_table_of(&self, table: &str) -> SqlResult2<Option<(CollectionId, EdgeTable)>> {
        let Some(c) = crate::find(self.db, table)? else {
            return Ok(None);
        };
        Ok(self
            .db
            .edge_table(c)
            .map_err(SqlError::from)?
            .map(|edge| (c, edge)))
    }

    /// `CREATE TABLE` with a `REFERENCES` column or a table-level key: an
    /// EDGE TABLE (`EDGE_TABLES.md` §2.1).
    pub(super) fn create_edge_table(
        &mut self,
        table: String,
        columns: Vec<ColumnDef>,
        mut primary_key: Vec<String>,
        if_not_exists: bool,
        with: &[WithIndex],
    ) -> SqlResult2<WritePlan> {
        if if_not_exists && crate::find(self.db, &table)?.is_some() {
            return Ok(WritePlan::Notice(format!(
                "CREATE TABLE IF NOT EXISTS {table}: the table is already in the catalog, so nothing was created"
            )));
        }
        let references: Vec<(String, String)> = columns
            .iter()
            .filter_map(|c| c.references.as_ref().map(|(t, _)| (c.name.clone(), t.clone())))
            .collect();
        if references.is_empty() {
            return Err(SqlError::unsupported(format!(
                "PRIMARY KEY ({}) on `{table}`: a row has one external key, `_key`; a key of several columns is an edge table's -- a table whose REFERENCES columns name the rows its edges join (docs/core/EDGE_TABLES.md)",
                primary_key.join(", ")
            )));
        }
        if !with.is_empty() {
            return Err(SqlError::unsupported(format!(
                "CREATE TABLE {table} ... WITH (...): `{table}` is an edge table, and there is no index over edge properties"
            )));
        }
        for column in &columns {
            if let Some((target, Some(key))) = &column.references {
                if key != KEY_COLUMN {
                    return Err(SqlError::unsupported(format!(
                        "`{}` REFERENCES {target} ({key}): an edge's end names a row by its key, `{KEY_COLUMN}`",
                        column.name
                    )));
                }
            }
            if column.name.starts_with('_') {
                return Err(SqlError::unsupported(format!(
                    "column `{}` of edge table `{table}`: an edge table has no `_key` -- its ends name the rows it joins, and names beginning with `_` are reserved",
                    column.name
                )));
            }
            if column.references.is_some() && !matches!(column.kind, Kind::Text) {
                return Err(SqlError::unsupported(format!(
                    "`{}` REFERENCES a table: an end names a row by its key, which is TEXT",
                    column.name
                )));
            }
            if column.primary_key {
                if !primary_key.is_empty() {
                    return Err(SqlError::unsupported(format!(
                        "`{table}` names its PRIMARY KEY twice: a table has one key"
                    )));
                }
                primary_key = vec![column.name.clone()];
            }
        }
        if !primary_key.is_empty() && !primary_key.iter().any(|k| references.iter().any(|(r, _)| r == k)) {
            return Err(SqlError::unsupported(format!(
                "PRIMARY KEY ({}) of edge table `{table}` must name an end: a key over properties alone needs an index over edge properties, which does not exist (docs/core/EDGE_TABLES.md §3)",
                primary_key.join(", ")
            )));
        }
        let mut fields = Vec::with_capacity(columns.len());
        let mut declared = Vec::new();
        let mut rules = Vec::new();
        for column in &columns {
            declared.push((column.name.clone(), column.declared.clone()));
            if let Some(rule) = &column.rule {
                rules.push((column.name.clone(), rule.clone()));
            }
            fields.push((column.name.clone(), column.kind.clone()));
        }
        Ok(WritePlan::CreateEdgeTable {
            name: table,
            fields,
            declared,
            rules,
            references,
            key: primary_key,
        })
    }

    /// `CREATE PROPERTY GRAPH` and `ALTER PROPERTY GRAPH ... ADD` (§2.2).
    pub(super) fn property_graph(
        &mut self,
        name: String,
        alter: bool,
        vertex_tables: Vec<String>,
        edge_tables: Vec<EdgeTableDecl>,
    ) -> SqlResult2<WritePlan> {
        let exists = !self
            .db
            .property_graph_tables(&name)
            .map_err(SqlError::from)?
            .is_empty();
        if alter && !exists {
            return Err(SqlError::coded(
                "42704",
                format!("property graph `{name}` does not exist"),
            ));
        }
        if !alter && exists {
            return Err(SqlError::coded(
                "42P07",
                format!("property graph `{name}` already exists"),
            ));
        }
        if !alter && edge_tables.is_empty() {
            return Err(SqlError::unsupported(format!(
                "CREATE PROPERTY GRAPH {name} with no EDGE TABLES: a property graph here is recorded by the edge tables it declares, over the base graph, so one with none has nothing to hold it"
            )));
        }
        for vertex in &vertex_tables {
            let Some(c) = crate::find(self.db, vertex)? else {
                return Err(SqlError::engine(format!("no table named `{vertex}`")));
            };
            if self.db.edge_table(c).map_err(SqlError::from)?.is_some() {
                return Err(SqlError::unsupported(format!(
                    "`{vertex}` is an edge table: list it under EDGE TABLES"
                )));
            }
        }
        let mut binds = Vec::with_capacity(edge_tables.len());
        for edge in edge_tables {
            let Some((c, table)) = self.edge_table_of(&edge.table)? else {
                return Err(SqlError::unsupported(format!(
                    "`{}` is not an edge table: an edge table is a table whose REFERENCES columns name the rows its edges join",
                    edge.table
                )));
            };
            if table.binding.is_some() {
                return Err(SqlError::unsupported(format!(
                    "`{}` is already declared by a property graph: an edge table has one label, for good (docs/core/EDGE_TABLES.md §2.3)",
                    edge.table
                )));
            }
            for (column, target, word) in [
                (&edge.source, &edge.source_table, "SOURCE"),
                (&edge.destination, &edge.destination_table, "DESTINATION"),
            ] {
                let named = crate::find(self.db, target)?
                    .ok_or_else(|| SqlError::engine(format!("no table named `{target}`")))?;
                match table.references.iter().find(|(r, _)| r == column) {
                    Some((_, c)) if *c == named => {}
                    Some(_) => {
                        return Err(SqlError::unsupported(format!(
                            "{word} KEY ({column}) REFERENCES {target}: `{}`.`{column}` references another table",
                            edge.table
                        )))
                    }
                    None => {
                        return Err(SqlError::unsupported(format!(
                            "{word} KEY ({column}): `{}` declares no REFERENCES on `{column}`, so it names no row",
                            edge.table
                        )))
                    }
                }
                if !alter && !vertex_tables.iter().any(|v| v == target) {
                    return Err(SqlError::unsupported(format!(
                        "{word} KEY ... REFERENCES {target}: `{target}` is not among the graph's VERTEX TABLES"
                    )));
                }
            }
            let label = edge
                .label
                .clone()
                .unwrap_or_else(|| crate::split_table(&edge.table).1.to_owned());
            binds.push((c, edge.source, edge.destination, label));
        }
        self.notices.push(format!(
            "property graph `{name}` is a definition over the base graph: GRAPH_TABLE ({name} ...) walks the base graph, and a label outside the graph's tables is not refused (docs/core/EDGE_TABLES.md §5.2)"
        ));
        Ok(WritePlan::BindEdgeTables { graph: name, binds })
    }

    /// `INSERT INTO <edge table>` (§4.1, §4.2).
    pub(super) fn insert_edges(
        &mut self,
        c: CollectionId,
        table: &str,
        edge: &EdgeTable,
        mut columns: Vec<String>,
        values: &[Vec<Literal>],
        on_conflict: Option<ConflictClause>,
    ) -> SqlResult2<WritePlan> {
        let info = self.db.collection_info(c).map_err(SqlError::from)?;
        if columns.is_empty() {
            columns = info.layout.fields.iter().map(|(n, _)| n.clone()).collect();
        }
        let mut rows = Vec::with_capacity(values.len());
        for row in values {
            if row.len() != columns.len() {
                return Err(SqlError::syntax(
                    format!(
                        "row {} has {} value(s) for {} column(s) of `{table}`",
                        rows.len() + 1,
                        row.len(),
                        columns.len()
                    ),
                    0,
                ));
            }
            let mut document = Map::new();
            for (at, column) in columns.iter().enumerate() {
                document.insert(column.clone(), self.edge_value(c, edge, column, &row[at])?);
            }
            rows.push(Value::Object(document));
        }
        let on_conflict = match on_conflict {
            None => None,
            Some(clause) => {
                let mut target = clause.target.clone();
                let mut key = edge.key.clone();
                target.sort();
                key.sort();
                if edge.key.is_empty() || target != key {
                    return Err(SqlError::unsupported(format!(
                        "ON CONFLICT ({}) on `{table}`: the conflict target is the table's PRIMARY KEY ({})",
                        clause.target.join(", "),
                        edge.key.join(", ")
                    )));
                }
                Some(match clause.update {
                    None => OnConflict::Nothing,
                    Some(set) => OnConflict::Update(set),
                })
            }
        };
        Ok(WritePlan::InsertEdges {
            collection: c,
            rows,
            on_conflict,
        })
    }

    /// One value of an edge row: an end's key as TEXT, a property as its
    /// column's kind (a DATE or TIMESTAMPTZ literal as its microseconds, as
    /// a row stores it).
    fn edge_value(&self, c: CollectionId, edge: &EdgeTable, column: &str, literal: &Literal) -> SqlResult2<Value> {
        if edge.references.iter().any(|(r, _)| r == column) {
            let value = self.value_of(literal)?;
            return match value {
                Value::String(_) | Value::Null => Ok(value),
                other => Err(SqlError::Parameter(format!(
                    "`{column}` names a row by its TEXT key, not {other}"
                ))),
            };
        }
        let kind = self.kind_of(c, column)?;
        match self.time_column(c, column)? {
            Some(declared) => self.time_document_value(literal, column, &declared),
            None => self.document_value(kind, literal, column),
        }
    }

    /// A `WHERE` over an edge table: `column = value` joined by AND, which
    /// the engine requires to name an end (§4.5, §5.1).
    pub(super) fn edge_filter(&self, c: CollectionId, table: &str, edge: &EdgeTable, predicates: &[Expr]) -> SqlResult2<Value> {
        fn flatten<'e>(expr: &'e Expr, out: &mut Vec<&'e Expr>) {
            match expr {
                Expr::And(all) => all.iter().for_each(|e| flatten(e, out)),
                other => out.push(other),
            }
        }
        let mut leaves = Vec::new();
        predicates.iter().for_each(|p| flatten(p, &mut leaves));
        let mut filter = Map::new();
        for leaf in leaves {
            match leaf {
                Expr::Leaf(Predicate::Compare {
                    column,
                    op: CmpOp::Eq,
                    value,
                }) => {
                    filter.insert(column.clone(), self.edge_value(c, edge, column, value)?);
                }
                _ => {
                    return Err(SqlError::unsupported(format!(
                        "a WHERE on edge table `{table}` is `column = value` joined by AND, naming an end: the read is that end's edges (docs/core/EDGE_TABLES.md §5.1)"
                    )))
                }
            }
        }
        Ok(Value::Object(filter))
    }

    /// `UPDATE <edge table> SET c = v, ... WHERE ...` (§4.3).
    pub(super) fn update_edges(
        &mut self,
        c: CollectionId,
        table: &str,
        edge: &EdgeTable,
        assignments: &[(String, SetValue)],
        predicates: &[Expr],
    ) -> SqlResult2<WritePlan> {
        let mut patch = Map::new();
        for (column, set) in assignments {
            let SetValue::Lit(literal) = set else {
                return Err(SqlError::unsupported(format!(
                    "UPDATE {table} SET {column} = <expression>: an edge's property takes a value; a row expression over the edge has no atomic"
                )));
            };
            patch.insert(column.clone(), self.edge_value(c, edge, column, literal)?);
        }
        Ok(WritePlan::UpdateEdges {
            collection: c,
            filter: self.edge_filter(c, table, edge, predicates)?,
            patch: Value::Object(patch),
        })
    }

    /// `DELETE FROM <edge table> WHERE ...` (§4.4).
    pub(super) fn delete_edges(
        &mut self,
        c: CollectionId,
        table: &str,
        edge: &EdgeTable,
        predicates: &[Expr],
        cascade: bool,
    ) -> SqlResult2<WritePlan> {
        if cascade {
            return Err(SqlError::unsupported(format!(
                "DELETE FROM {table} ... CASCADE: deleting an edge removes nothing else"
            )));
        }
        Ok(WritePlan::DeleteEdges {
            collection: c,
            filter: self.edge_filter(c, table, edge, predicates)?,
        })
    }

    /// `SELECT ... FROM <edge table> WHERE ... [ORDER BY c] [LIMIT n]` (§5.1).
    pub(super) fn edge_select(
        &mut self,
        c: CollectionId,
        table: &str,
        edge: &EdgeTable,
        select: &SelectStmt,
    ) -> SqlResult2<EdgeRowsPlan> {
        if select.distinct || select.group.is_some() || !select.having.is_empty() {
            return Err(SqlError::unsupported(format!(
                "SELECT DISTINCT, GROUP BY and HAVING over edge table `{table}`: GRAPH_TABLE ... RETURN groups edges; the table form reads one end's edges"
            )));
        }
        let info = self.db.collection_info(c).map_err(SqlError::from)?;
        let mut table_columns = Vec::new();
        for (name, _) in &info.layout.fields {
            let time = match self.declared_of(c, name)?.as_deref() {
                Some("DATE") => Some(true),
                Some(d) if functions::is_time_type(d) => Some(false),
                _ => None,
            };
            table_columns.push((name.clone(), time));
        }
        let at = |name: &str| {
            table_columns
                .iter()
                .position(|(n, _)| n == name)
                .ok_or_else(|| SqlError::coded("42703", format!("column `{name}` does not exist in `{table}`")))
        };
        let mut picks = Vec::new();
        let mut columns = Vec::new();
        for (item, alias) in &select.items {
            match item {
                SelectItem::Star => {
                    for (i, (name, _)) in table_columns.iter().enumerate() {
                        picks.push(i);
                        columns.push(name.clone());
                    }
                }
                SelectItem::Column(name) => {
                    picks.push(at(name)?);
                    columns.push(alias.clone().unwrap_or_else(|| name.clone()));
                }
                other => {
                    return Err(SqlError::unsupported(format!(
                        "the select list of edge table `{table}` is `*` or its columns; {other:?} is neither"
                    )))
                }
            }
        }
        let order = match &select.order {
            None => None,
            Some(OrderKey::Column { column, descending }) => Some((at(column)?, *descending)),
            Some(_) => {
                return Err(SqlError::unsupported(format!(
                    "edge table `{table}` is ordered by one of its columns"
                )))
            }
        };
        Ok(EdgeRowsPlan {
            collection: c,
            table: table.to_owned(),
            filter: self.edge_filter(c, table, edge, &select.predicates)?,
            table_columns,
            picks,
            columns,
            order,
            limit: select.limit,
        })
    }
}
