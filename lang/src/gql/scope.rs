//! The graph a `GRAPH_TABLE` names, and what its labels mean there
//! (`docs/core/EDGE_TABLES.md` §5).
//!
//! In a NAMED property graph a label is one the graph's definition gives an
//! element, and an unlabeled pattern covers the graph's own tables. In the
//! BASE graph -- and a graph context written through the API -- a label is a
//! table's own name, in any schema, or a label the base graph was given; one
//! name that reaches two tables is refused as ambiguous, and `"schema.table"`
//! picks one.
//!
//! The scope is set while one statement's body binds, the way `SET LOCAL
//! ef_search` reaches the planner, so the binding functions keep their
//! shape; a nested `GRAPH_TABLE` sets its own and the outer one comes back.

use std::cell::RefCell;
use std::rc::Rc;

use sekejap_core::collections::{CollectionId, Database, EdgeTypeId, GraphElement, BASE_GRAPH};

use crate::{SqlError, SqlResult2};

/// What a `GRAPH_TABLE`'s labels resolve against.
pub(crate) enum GraphScope {
    /// The base graph or a graph context: every table, with the base graph's
    /// own extra labels.
    Base { labels: Vec<GraphElement> },
    /// A named property graph's definition.
    Named { name: String, elements: Vec<GraphElement> },
}

thread_local! {
    static SCOPE: RefCell<Option<Rc<GraphScope>>> = const { RefCell::new(None) };
}

/// The scope of one statement body; the previous one comes back on drop.
pub(crate) struct ScopeGuard(Option<Rc<GraphScope>>);

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        let previous = self.0.take();
        SCOPE.with(|s| *s.borrow_mut() = previous);
    }
}

/// Resolve `graph` and make it the scope until the guard drops. A name that
/// is neither `base` nor a property graph is a graph context, whose labels
/// resolve as the base graph's.
pub(crate) fn enter(db: &Database, graph: &str) -> SqlResult2<ScopeGuard> {
    let scope = if graph.eq_ignore_ascii_case(BASE_GRAPH) {
        None
    } else {
        let elements = db.property_graph(graph).map_err(SqlError::from)?;
        (!elements.is_empty()).then(|| GraphScope::Named {
            name: graph.to_owned(),
            elements,
        })
    };
    let scope = match scope {
        Some(named) => named,
        None => GraphScope::Base {
            labels: db.property_graph(BASE_GRAPH).map_err(SqlError::from)?,
        },
    };
    let previous = SCOPE.with(|s| s.borrow_mut().replace(Rc::new(scope)));
    Ok(ScopeGuard(previous))
}

fn current() -> Option<Rc<GraphScope>> {
    SCOPE.with(|s| s.borrow().clone())
}

/// The name of the named graph in scope, if one is.
pub(crate) fn named_graph() -> Option<String> {
    match current().as_deref() {
        Some(GraphScope::Named { name, .. }) => Some(name.clone()),
        _ => None,
    }
}

/// The tables a vertex label names.
pub(crate) fn vertex_label(db: &Database, label: &str) -> SqlResult2<Vec<CollectionId>> {
    match current().as_deref() {
        Some(GraphScope::Named { name, elements }) => {
            let found: Vec<CollectionId> = elements
                .iter()
                .filter(|e| !e.edge && e.labels.iter().any(|l| l == label))
                .map(|e| e.table)
                .collect();
            if found.is_empty() {
                return Err(SqlError::unsupported(format!(
                    "graph `{name}` has no vertex label `{label}`"
                )));
            }
            Ok(dedupe(found))
        }
        Some(GraphScope::Base { labels }) => base_vertex_label(db, labels, label),
        None => base_vertex_label(db, &[], label),
    }
}

/// Every vertex table of the named graph in scope; `None` outside one.
pub(crate) fn named_vertices() -> Option<Box<[CollectionId]>> {
    match current().as_deref() {
        Some(GraphScope::Named { elements, .. }) => Some(dedupe(
            elements.iter().filter(|e| !e.edge).map(|e| e.table).collect(),
        )
        .into()),
        _ => None,
    }
}

/// The edge types an edge label names: `Ok(None)` for a name no edge type
/// holds yet (the hop matches nothing until one is written).
pub(crate) fn edge_label(db: &Database, label: &str) -> SqlResult2<Option<Vec<EdgeTypeId>>> {
    match current().as_deref() {
        Some(GraphScope::Named { name, elements }) => {
            let mut found = Vec::new();
            for element in elements.iter().filter(|e| e.edge && e.labels.iter().any(|l| l == label)) {
                found.push(bound_type(db, element.table)?);
            }
            if found.is_empty() {
                return Err(SqlError::unsupported(format!(
                    "graph `{name}` has no edge label `{label}`"
                )));
            }
            Ok(Some(dedupe(found)))
        }
        Some(GraphScope::Base { labels }) => base_edge_label(db, labels, label),
        None => base_edge_label(db, &[], label),
    }
}

/// Every edge type of the named graph in scope; `None` outside one.
pub(crate) fn named_edge_types(db: &Database) -> SqlResult2<Option<Vec<EdgeTypeId>>> {
    match current().as_deref() {
        Some(GraphScope::Named { elements, .. }) => {
            let mut out = Vec::new();
            for element in elements.iter().filter(|e| e.edge) {
                out.push(bound_type(db, element.table)?);
            }
            Ok(Some(dedupe(out)))
        }
        _ => Ok(None),
    }
}

/// The edge type interned under `name`, if any: none on a database whose
/// graph was never switched on.
pub(crate) fn edge_type_named(db: &Database, name: &str) -> SqlResult2<Option<EdgeTypeId>> {
    let (types, _) = db.graph_names().map_err(SqlError::from)?;
    Ok(types.into_iter().find(|(_, n)| n == name).map(|(id, _)| id))
}

/// Whether `name` is a graph context written through the API.
pub(crate) fn context_named(db: &Database, name: &str) -> SqlResult2<bool> {
    let (_, contexts) = db.graph_names().map_err(SqlError::from)?;
    Ok(contexts.iter().any(|(_, n)| n == name))
}

fn dedupe<T: PartialEq>(items: Vec<T>) -> Vec<T> {
    let mut out: Vec<T> = Vec::with_capacity(items.len());
    for item in items {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

fn bound_type(db: &Database, table: CollectionId) -> SqlResult2<EdgeTypeId> {
    db.edge_table(table)
        .map_err(SqlError::from)?
        .and_then(|t| t.binding)
        .map(|b| b.edge_type)
        .ok_or_else(|| SqlError::engine("an edge element has no direction"))
}

/// `"schema.table"`: the one table it names, quoted in a base-graph label.
fn qualified(db: &Database, label: &str) -> SqlResult2<Option<CollectionId>> {
    let Some((schema, table)) = label.split_once('.') else {
        return Ok(None);
    };
    db.collection_in(schema, table).map_err(SqlError::from)
}

/// Every table named `name`, in any schema, as `(shown name, id)`.
fn named_tables(db: &Database, name: &str) -> SqlResult2<Vec<(String, CollectionId)>> {
    let mut out = Vec::new();
    for (schema, table) in db.list_qualified_collections().map_err(SqlError::from)? {
        if table != name {
            continue;
        }
        if let Some(id) = db.collection_in(&schema, &table).map_err(SqlError::from)? {
            out.push((crate::table_name_of(db, id)?, id));
        }
    }
    Ok(out)
}

fn ambiguous(label: &str, shown: &[String]) -> SqlError {
    let quoted: Vec<String> = shown.iter().map(|s| format!("\"{s}\"")).collect();
    SqlError::unsupported(format!(
        "label `{label}` names {} in the base graph: write {} to pick one, or give one a label with ALTER PROPERTY GRAPH base ALTER VERTEX TABLE ... ADD LABEL",
        shown.join(" and "),
        quoted.join(" or ")
    ))
}

fn base_vertex_label(db: &Database, labels: &[GraphElement], label: &str) -> SqlResult2<Vec<CollectionId>> {
    if let Some(id) = qualified(db, label)? {
        return Ok(vec![id]);
    }
    let mut found: Vec<(String, CollectionId)> = named_tables(db, label)?
        .into_iter()
        .filter(|(_, id)| db.edge_table(*id).ok().flatten().is_none())
        .collect();
    for element in labels.iter().filter(|e| !e.edge && e.labels.iter().any(|l| l == label)) {
        if !found.iter().any(|(_, id)| *id == element.table) {
            found.push((crate::table_name_of(db, element.table)?, element.table));
        }
    }
    match found.len() {
        0 => Err(SqlError::engine(format!("no collection named `{label}`"))),
        1 => Ok(vec![found[0].1]),
        _ => Err(ambiguous(label, &found.into_iter().map(|(s, _)| s).collect::<Vec<_>>())),
    }
}

fn base_edge_label(
    db: &Database,
    labels: &[GraphElement],
    label: &str,
) -> SqlResult2<Option<Vec<EdgeTypeId>>> {
    if let Some(id) = qualified(db, label)? {
        if db.edge_table(id).map_err(SqlError::from)?.is_some() {
            return Ok(Some(vec![bound_type(db, id)?]));
        }
    }
    // (what reaches it, the edge type)
    let mut found: Vec<(String, EdgeTypeId)> = Vec::new();
    if let Some(t) = edge_type_named(db, label)? {
        let shown = match db.edge_table_of_type(t).map_err(SqlError::from)? {
            Some(table) => crate::table_name_of(db, table)?,
            None => format!("edge type {label}"),
        };
        found.push((shown, t));
    }
    for (shown, id) in named_tables(db, label)? {
        if let Some(binding) = db.edge_table(id).map_err(SqlError::from)?.and_then(|t| t.binding) {
            if !found.iter().any(|(_, t)| *t == binding.edge_type) {
                found.push((shown, binding.edge_type));
            }
        }
    }
    for element in labels.iter().filter(|e| e.edge && e.labels.iter().any(|l| l == label)) {
        let t = bound_type(db, element.table)?;
        if !found.iter().any(|(_, have)| *have == t) {
            found.push((crate::table_name_of(db, element.table)?, t));
        }
    }
    match found.len() {
        0 => Ok(None),
        1 => Ok(Some(vec![found[0].1])),
        _ => Err(ambiguous(label, &found.into_iter().map(|(s, _)| s).collect::<Vec<_>>())),
    }
}
