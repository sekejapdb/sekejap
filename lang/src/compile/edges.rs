//! EDGE TABLES in SQL (`docs/core/EDGE_TABLES.md`): the ISO SQL/PGQ,
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
use sekejap_core::collections::{EdgeTable, GraphElement, OnConflict, BASE_GRAPH};

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

    /// `CREATE [OR REPLACE] PROPERTY GRAPH` and `ALTER PROPERTY GRAPH`
    /// (`docs/core/EDGE_TABLES.md` §2). The statement is applied to the
    /// graph's current definition in memory and checked whole; what runs is
    /// the edge tables whose direction is fixed or taken back, and the one
    /// definition write.
    pub(super) fn property_graph(
        &mut self,
        name: String,
        mode: GraphMode,
        vertex_tables: Vec<ElementDecl>,
        edge_tables: Vec<ElementDecl>,
        alters: Vec<GraphAlter>,
    ) -> SqlResult2<WritePlan> {
        let base = name.eq_ignore_ascii_case(BASE_GRAPH);
        if base && mode != GraphMode::Alter {
            return Err(SqlError::unsupported(
                "`base` is the default graph: it always exists and holds every table and every edge, so it is not created or replaced; change it with ALTER PROPERTY GRAPH base",
            ));
        }
        let name = if base { BASE_GRAPH.to_owned() } else { name };
        let current = self.db.property_graph(&name).map_err(SqlError::from)?;
        let exists = base || !current.is_empty();
        match mode {
            GraphMode::Alter if !exists => {
                return Err(SqlError::coded("42704", format!("property graph `{name}` does not exist")))
            }
            GraphMode::Create if exists => {
                return Err(SqlError::coded("42P07", format!("property graph `{name}` already exists")))
            }
            _ => {}
        }
        if !exists && crate::gql::scope::context_named(self.db, &name)? {
            return Err(SqlError::unsupported(format!(
                "`{name}` already names a graph context, the partition edges are written into through the API; a property graph needs another name"
            )));
        }
        let mut elements = if mode == GraphMode::CreateOrReplace { Vec::new() } else { current };
        let mut binds: Vec<(CollectionId, String, String, String)> = Vec::new();
        let mut unbinds: Vec<CollectionId> = Vec::new();
        let mut notices: Vec<String> = Vec::new();
        for (decl, edge) in vertex_tables
            .into_iter()
            .map(|d| (d, false))
            .chain(edge_tables.into_iter().map(|d| (d, true)))
        {
            let (element, bind) = self.graph_element(&decl, edge, base)?;
            if let Some(bind) = bind {
                if binds.iter().any(|(c, ..)| *c == element.table) {
                    return Err(SqlError::unsupported(format!(
                        "`{}` is named twice in one statement",
                        decl.table
                    )));
                }
                binds.push(bind);
            }
            if base {
                // Every table is in `base` already: what a statement adds
                // there is a direction or a label.
                let named: Vec<String> = decl
                    .labels
                    .iter()
                    .filter_map(|l| match l {
                        LabelDecl::Named(l) => Some(l.clone()),
                        LabelDecl::Default => None,
                    })
                    .collect();
                if named.is_empty() {
                    if !edge {
                        notices.push(format!(
                            "ALTER PROPERTY GRAPH base ADD VERTEX TABLES ({}): every table is in the base graph already",
                            crate::shown_table(&decl.table)
                        ));
                    }
                    continue;
                }
                base_labels(&mut elements, element.table, &crate::shown_table(&decl.table), edge, &named, true);
                continue;
            }
            elements.push(element);
        }
        for alter in alters {
            match alter {
                GraphAlter::Drop { edge, elements: names } => {
                    for written in names {
                        if base {
                            if !edge {
                                return Err(SqlError::unsupported(
                                    "a table cannot leave the base graph: `base` holds every table",
                                ));
                            }
                            let Some((c, _)) = self.edge_table_of(&written)? else {
                                return Err(SqlError::engine(format!(
                                    "`{}` is not an edge table",
                                    crate::shown_table(&written)
                                )));
                            };
                            unbinds.push(c);
                            elements.retain(|e| e.table != c);
                            continue;
                        }
                        let at = self.element_at(&elements, &written, edge, &name)?;
                        elements.remove(at);
                    }
                }
                GraphAlter::Label {
                    edge,
                    element,
                    label,
                    add,
                } => {
                    if base {
                        let c = crate::find(self.db, &element)?.ok_or_else(|| {
                            SqlError::engine(format!("no table named `{}`", crate::shown_table(&element)))
                        })?;
                        if !base_labels(&mut elements, c, &crate::shown_table(&element), edge, &[label.clone()], add)
                            && !add
                        {
                            return Err(SqlError::unsupported(format!(
                                "`{}` has no label `{label}` in the base graph",
                                crate::shown_table(&element)
                            )));
                        }
                        continue;
                    }
                    let at = self.element_at(&elements, &element, edge, &name)?;
                    let labels = &mut elements[at].labels;
                    if add {
                        if !labels.contains(&label) {
                            labels.push(label);
                        }
                    } else {
                        let before = labels.len();
                        labels.retain(|l| *l != label);
                        if labels.len() == before {
                            return Err(SqlError::unsupported(format!(
                                "element `{element}` of graph `{name}` has no label `{label}`"
                            )));
                        }
                        if labels.is_empty() {
                            return Err(SqlError::unsupported(format!(
                                "DROP LABEL {label}: element `{element}` keeps at least one label; add another first"
                            )));
                        }
                    }
                }
            }
        }
        if !base {
            if elements.is_empty() {
                return Err(SqlError::unsupported(format!(
                    "graph `{name}` would have no element left: remove the graph with DROP PROPERTY GRAPH {name}"
                )));
            }
            self.check_graph(&name, &elements, &binds)?;
        }
        for notice in notices {
            self.notices.push(notice);
        }
        Ok(WritePlan::PropertyGraph {
            graph: name,
            binds,
            unbinds,
            elements,
        })
    }

    /// One element a statement names: its table, its element name, its
    /// labels -- and, for an edge table whose direction is not fixed yet, the
    /// binding to fix. `base` takes an edge table's ends and labels only.
    fn graph_element(
        &self,
        decl: &ElementDecl,
        edge: bool,
        base: bool,
    ) -> SqlResult2<(GraphElement, Option<(CollectionId, String, String, String)>)> {
        let shown = crate::shown_table(&decl.table);
        let Some(c) = crate::find(self.db, &decl.table)? else {
            return Err(SqlError::engine(format!("no table named `{shown}`")));
        };
        let edge_table = self.db.edge_table(c).map_err(SqlError::from)?;
        match (edge, &edge_table) {
            (false, Some(_)) => {
                return Err(SqlError::unsupported(format!(
                    "`{shown}` is an edge table: list it under EDGE TABLES"
                )))
            }
            (true, None) => {
                return Err(SqlError::unsupported(format!(
                    "`{shown}` is not an edge table: an edge table is a table whose REFERENCES columns name the rows its edges join"
                )))
            }
            _ => {}
        }
        if base && decl.alias.is_some() {
            return Err(SqlError::unsupported(
                "AS in the base graph: a table's name there is its own name; a named graph gives it another",
            ));
        }
        let alias = decl
            .alias
            .clone()
            .unwrap_or_else(|| crate::split_table(&decl.table).1.to_owned());
        let mut labels = Vec::new();
        for label in &decl.labels {
            let label = match label {
                LabelDecl::Default => alias.clone(),
                LabelDecl::Named(l) => l.clone(),
            };
            if !labels.contains(&label) {
                labels.push(label);
            }
        }
        if labels.is_empty() {
            labels.push(alias.clone());
        }
        let mut bind = None;
        if let Some(table) = &edge_table {
            let reference = |column: &str| table.references.iter().find(|(r, _)| r == column).map(|(_, c)| *c);
            match (&table.binding, &decl.ends) {
                (Some(binding), Some(ends)) => {
                    let matches = binding.source == ends.source
                        && binding.destination == ends.destination
                        && reference(&ends.source) == crate::find(self.db, &ends.source_table)?
                        && reference(&ends.destination) == crate::find(self.db, &ends.destination_table)?;
                    if !matches {
                        let name_of = |column: &str| -> SqlResult2<String> {
                            Ok(match reference(column) {
                                Some(t) => crate::table_name_of(self.db, t)?,
                                None => "?".to_owned(),
                            })
                        };
                        return Err(SqlError::unsupported(format!(
                            "`{shown}`'s direction is fixed: source `{}` ({}), destination `{}` ({}); a graph names it as it is, or without SOURCE KEY and DESTINATION KEY",
                            binding.source,
                            name_of(&binding.source)?,
                            binding.destination,
                            name_of(&binding.destination)?
                        )));
                    }
                }
                (Some(_), None) => {}
                (None, None) => {
                    return Err(SqlError::unsupported(format!(
                        "`{shown}` has no direction yet: name its SOURCE KEY and DESTINATION KEY the first time a graph declares it"
                    )))
                }
                (None, Some(ends)) => {
                    for (column, target, word) in [
                        (&ends.source, &ends.source_table, "SOURCE"),
                        (&ends.destination, &ends.destination_table, "DESTINATION"),
                    ] {
                        let named = crate::find(self.db, target)?.ok_or_else(|| {
                            SqlError::engine(format!("no table named `{}`", crate::shown_table(target)))
                        })?;
                        match reference(column) {
                            Some(r) if r == named => {}
                            Some(_) => {
                                return Err(SqlError::unsupported(format!(
                                    "{word} KEY ({column}) REFERENCES {}: `{shown}`.`{column}` references another table",
                                    crate::shown_table(target)
                                )))
                            }
                            None => {
                                return Err(SqlError::unsupported(format!(
                                    "{word} KEY ({column}): `{shown}` declares no REFERENCES on `{column}`, so it names no row"
                                )))
                            }
                        }
                    }
                    // The stored edge type: the first label, else the table's
                    // name, with its schema when it has one, so two schemas'
                    // same-named edge tables never share a type.
                    let first = decl.labels.iter().find_map(|l| match l {
                        LabelDecl::Named(l) => Some(l.clone()),
                        LabelDecl::Default => None,
                    });
                    let bare = first.unwrap_or_else(|| crate::split_table(&decl.table).1.to_owned());
                    let (schema, _) = crate::split_table(&decl.table);
                    let edge_type = if schema == sekejap_core::collections::PUBLIC_SCHEMA {
                        bare
                    } else {
                        format!("{schema}.{bare}")
                    };
                    bind = Some((c, ends.source.clone(), ends.destination.clone(), edge_type));
                }
            }
        }
        Ok((
            GraphElement {
                table: c,
                alias,
                edge,
                labels,
            },
            bind,
        ))
    }

    /// The element an ALTER names: by its element name, or by the table it
    /// shows when that is unambiguous.
    fn element_at(&self, elements: &[GraphElement], written: &str, edge: bool, graph: &str) -> SqlResult2<usize> {
        let kind = if edge { "edge" } else { "vertex" };
        if let Some(at) = elements.iter().position(|e| e.edge == edge && e.alias == written) {
            return Ok(at);
        }
        if let Some(c) = crate::find(self.db, written)? {
            let found: Vec<usize> = elements
                .iter()
                .enumerate()
                .filter(|(_, e)| e.edge == edge && e.table == c)
                .map(|(at, _)| at)
                .collect();
            if found.len() == 1 {
                return Ok(found[0]);
            }
        }
        Err(SqlError::unsupported(format!(
            "graph `{graph}` has no {kind} table `{}`",
            crate::shown_table(written)
        )))
    }

    /// A named graph's definition, checked whole: element names unique, an
    /// edge's ends among the graph's vertex tables, and a label shared by
    /// several tables showing the same properties on each.
    fn check_graph(
        &self,
        graph: &str,
        elements: &[GraphElement],
        binds: &[(CollectionId, String, String, String)],
    ) -> SqlResult2<()> {
        let shown = |c: CollectionId| crate::table_name_of(self.db, c);
        for (at, element) in elements.iter().enumerate() {
            if let Some(other) = elements[..at].iter().find(|o| o.alias == element.alias) {
                return Err(SqlError::unsupported(format!(
                    "graph `{graph}` has two elements named `{}` ({} and {}): give one of them another name with AS",
                    element.alias,
                    shown(other.table)?,
                    shown(element.table)?
                )));
            }
        }
        // An edge's ends must be vertex tables of the same graph.
        for element in elements.iter().filter(|e| e.edge) {
            let table = self.db.edge_table(element.table).map_err(SqlError::from)?.expect("an edge element");
            let (source, destination) = match (&table.binding, binds.iter().find(|b| b.0 == element.table)) {
                (Some(binding), _) => (binding.source.clone(), binding.destination.clone()),
                (None, Some((_, s, d, _))) => (s.clone(), d.clone()),
                (None, None) => continue,
            };
            for end in [source, destination] {
                let Some((_, target)) = table.references.iter().find(|(r, _)| *r == end) else {
                    continue;
                };
                if !elements.iter().any(|e| !e.edge && e.table == *target) {
                    return Err(SqlError::unsupported(format!(
                        "edge table `{}` reaches `{}`, which is not among graph `{graph}`'s VERTEX TABLES: list it, or remove `{}` from the graph",
                        element.alias,
                        shown(*target)?,
                        element.alias
                    )));
                }
            }
        }
        // A shared label shows the same properties on every table.
        let properties = |element: &GraphElement| -> SqlResult2<Vec<String>> {
            let info = self.db.collection_info(element.table).map_err(SqlError::from)?;
            let mut out: Vec<String> = info.layout.fields.iter().map(|(n, _)| n.clone()).collect();
            if element.edge {
                if let Some(table) = self.db.edge_table(element.table).map_err(SqlError::from)? {
                    out.retain(|n| !table.references.iter().any(|(r, _)| r == n));
                }
            }
            out.sort();
            Ok(out)
        };
        for (at, element) in elements.iter().enumerate() {
            for label in &element.labels {
                for other in elements[..at].iter().filter(|o| o.edge == element.edge && o.labels.contains(label)) {
                    let (mine, theirs) = (properties(element)?, properties(other)?);
                    if mine != theirs {
                        let odd = mine
                            .iter()
                            .find(|p| !theirs.contains(p))
                            .map(|p| (p.clone(), element.alias.clone(), other.alias.clone()))
                            .or_else(|| {
                                theirs
                                    .iter()
                                    .find(|p| !mine.contains(p))
                                    .map(|p| (p.clone(), other.alias.clone(), element.alias.clone()))
                            })
                            .unwrap_or_default();
                        return Err(SqlError::unsupported(format!(
                            "label `{label}` would show `{}` on `{}` and not on `{}`: a label shared by several tables shows the same properties on each",
                            odd.0, odd.1, odd.2
                        )));
                    }
                }
            }
        }
        Ok(())
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
        if !select.then_by.is_empty() {
            return Err(SqlError::unsupported(
                "ORDER BY with several keys over an edge table: one end's edges are ordered by one column",
            ));
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

/// Add or remove base-graph labels on table `c`; its element there holds
/// only the labels a statement gave it (its own name is always a label).
/// Returns whether anything changed.
fn base_labels(
    elements: &mut Vec<GraphElement>,
    c: CollectionId,
    shown: &str,
    edge: bool,
    labels: &[String],
    add: bool,
) -> bool {
    let at = match elements.iter().position(|e| e.table == c) {
        Some(at) => at,
        None if add => {
            elements.push(GraphElement {
                table: c,
                alias: shown.to_owned(),
                edge,
                labels: Vec::new(),
            });
            elements.len() - 1
        }
        None => return false,
    };
    let held = &mut elements[at].labels;
    let before = held.clone();
    for label in labels {
        if add {
            if !held.contains(label) {
                held.push(label.clone());
            }
        } else {
            held.retain(|l| l != label);
        }
    }
    let changed = *held != before;
    if elements[at].labels.is_empty() {
        elements.remove(at);
    }
    changed
}
