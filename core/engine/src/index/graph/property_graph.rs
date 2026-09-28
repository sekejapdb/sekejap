//! PROPERTY GRAPHS as views over the base graph (`docs/core/EDGE_TABLES.md`).
//!
//! Three layers. The base graph stores every edge once, in the adjacency
//! keyspaces. An edge table is the door its edges are written through; its
//! direction -- which `REFERENCES` column is the source, which the
//! destination -- is its BINDING, fixed once and kept on the edge table
//! (`edge_table.rs`). A property graph is only a stored definition over those
//! two: which tables it shows, under which element name (alias), with which
//! labels. Any number of graphs may show the same table; changing or dropping
//! one never touches an edge.
//!
//! ## Where a definition is stored
//!
//! On each member table's own catalog record, as a MEMBERSHIPS tail (catalog flag
//! bit `CATALOG_GRAPHS`): the graphs the table belongs to, with its alias,
//! whether it is an edge element, and its labels. A graph is the set of
//! tables that name it. The base graph's own extra labels
//! (`ALTER PROPERTY GRAPH base ALTER VERTEX TABLE t ADD LABEL l`) are
//! memberships of the graph named `base`. Keeping the definition in the
//! catalog record means recovery and verification carry it as they carry
//! every other tail, with no keyspace of its own.
//!
//! ## Older files
//!
//! Before 0.18.3 a graph was recorded only as a name on each edge table it
//! declared (`EdgeBinding::graph`). A graph with no memberships is read from
//! those names: each such edge table an edge element, each table at one of
//! their ends a vertex element, every element labelled with its own name --
//! the edge table with its edge type's name. The first definition write for
//! that graph replaces the names with memberships.

use super::*;
use crate::collections::{Catalog, PUBLIC_SCHEMA};

/// The logical feature bit that says a catalog record carries a MEMBERSHIPS
/// tail. Additive and monotone: set in the transaction of the first
/// definition write, never cleared; a binary that predates it refuses the
/// file as `Unsupported` at admission (Law 8). A file with no stored
/// definition does not carry it.
pub const PROPERTY_GRAPH_FEATURE: u64 = 0x400000;

/// The encoding of the MEMBERSHIPS tail.
const MEMBERSHIPS_VERSION: u8 = 1;

/// The name of the default graph.
pub const BASE_GRAPH: &str = "base";

/// One table's place in one graph, as its catalog record holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphMembership {
    pub graph: String,
    /// The element name: the table name without its schema unless `AS`
    /// gave another. Unique within the graph.
    pub alias: String,
    pub edge: bool,
    /// At least one.
    pub labels: Vec<String>,
}

/// One element of a property graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphElement {
    pub table: CollectionId,
    pub alias: String,
    pub edge: bool,
    pub labels: Vec<String>,
}

fn name_bytes(b: &mut Vec<u8>, what: &str, n: &str) -> Result<()> {
    if n.is_empty() || n.len() > 255 {
        return Err(invalid(format!("a {what} is 1..255 bytes")));
    }
    b.push(n.len() as u8);
    b.extend_from_slice(n.as_bytes());
    Ok(())
}

/// The MEMBERSHIPS tail, appended to `b`.
pub(crate) fn encode_memberships(b: &mut Vec<u8>, memberships: &[GraphMembership]) -> Result<()> {
    if memberships.len() > 255 {
        return Err(invalid("a table belongs to at most 255 property graphs"));
    }
    b.push(MEMBERSHIPS_VERSION);
    b.push(memberships.len() as u8);
    for m in memberships {
        name_bytes(b, "property graph name", &m.graph)?;
        name_bytes(b, "graph element name", &m.alias)?;
        b.push(u8::from(m.edge));
        if m.labels.is_empty() || m.labels.len() > 255 {
            return Err(invalid("a graph element has 1..255 labels"));
        }
        b.push(m.labels.len() as u8);
        for label in &m.labels {
            name_bytes(b, "label", label)?;
        }
    }
    Ok(())
}

pub(crate) fn decode_memberships(
    mut read: impl FnMut(usize) -> Result<Vec<u8>>,
) -> Result<Vec<GraphMembership>> {
    let version = read(1)?[0];
    if version != MEMBERSHIPS_VERSION {
        return Err(Error::Unsupported(format!("graph memberships tail version {version}")));
    }
    let name = |read: &mut dyn FnMut(usize) -> Result<Vec<u8>>| -> Result<String> {
        let n = read(1)?[0] as usize;
        if n == 0 {
            return Err(corrupt("empty name in the graph memberships tail"));
        }
        String::from_utf8(read(n)?).map_err(corrupt)
    };
    let count = read(1)?[0] as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let graph = name(&mut read)?;
        let alias = name(&mut read)?;
        let edge = match read(1)?[0] {
            0 => false,
            1 => true,
            _ => return Err(corrupt("graph membership kind")),
        };
        let labels = read(1)?[0] as usize;
        if labels == 0 {
            return Err(corrupt("a graph membership with no label"));
        }
        let mut names = Vec::with_capacity(labels);
        for _ in 0..labels {
            names.push(name(&mut read)?);
        }
        out.push(GraphMembership {
            graph,
            alias,
            edge,
            labels: names,
        });
    }
    if out.is_empty() {
        return Err(corrupt("an empty graph memberships tail"));
    }
    Ok(out)
}

impl Database {
    /// Every table, with its catalog record, in catalog order.
    fn catalogs(&self) -> Result<Vec<(String, Catalog)>> {
        let mut out = Vec::new();
        for (schema, name) in self.list_qualified_collections()? {
            let Some(c) = self.collection_in(&schema, &name)? else {
                continue;
            };
            let catalog = self.catalog(c)?;
            let shown = if schema == PUBLIC_SCHEMA {
                name
            } else {
                format!("{schema}.{name}")
            };
            out.push((shown, catalog));
        }
        Ok(out)
    }

    /// The elements of property graph `graph`, in catalog order; empty when
    /// no such graph exists. `base` returns the base graph's own labels only
    /// -- every table is in the base graph whatever this says.
    pub fn property_graph(&self, graph: &str) -> Result<Vec<GraphElement>> {
        let catalogs = self.catalogs()?;
        let mut out = Vec::new();
        for (_, catalog) in &catalogs {
            for m in catalog.graphs.iter().filter(|m| m.graph == graph) {
                out.push(GraphElement {
                    table: catalog.id,
                    alias: m.alias.clone(),
                    edge: m.edge,
                    labels: m.labels.clone(),
                });
            }
        }
        if !out.is_empty() || graph == BASE_GRAPH {
            return Ok(out);
        }
        // A graph recorded the pre-0.18.3 way: a name on its edge tables.
        let mut vertices: Vec<CollectionId> = Vec::new();
        for (shown, catalog) in &catalogs {
            let Some(binding) = catalog.edge.as_ref().and_then(|e| e.binding.as_ref()) else {
                continue;
            };
            if binding.graph != graph {
                continue;
            }
            let _ = shown;
            let label = self.edge_type_name(binding.edge_type)?;
            out.push(GraphElement {
                table: catalog.id,
                alias: catalog.name.clone(),
                edge: true,
                labels: vec![label],
            });
            let edge = catalog.edge.as_ref().expect("checked above");
            for end in [&binding.source, &binding.destination] {
                if let Some((_, target)) = edge.references.iter().find(|(column, _)| column == end) {
                    if !vertices.contains(target) {
                        vertices.push(*target);
                    }
                }
            }
        }
        for target in vertices {
            let name = self.catalog(target)?.name;
            out.push(GraphElement {
                table: target,
                alias: name.clone(),
                edge: false,
                labels: vec![name],
            });
        }
        Ok(out)
    }

    /// The property graphs table `c` is an element of, by name, `base` not
    /// included -- a pre-0.18.3 graph's name on an edge table counted.
    pub fn graphs_of(&self, c: CollectionId) -> Result<Vec<String>> {
        let catalog = self.catalog(c)?;
        let mut out: Vec<String> = catalog
            .graphs
            .iter()
            .filter(|m| m.graph != BASE_GRAPH)
            .map(|m| m.graph.clone())
            .collect();
        if let Some(binding) = catalog.edge.as_ref().and_then(|e| e.binding.as_ref()) {
            if !binding.graph.is_empty() && !out.contains(&binding.graph) {
                out.push(binding.graph.clone());
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// Every property graph by name, `base` not included.
    pub fn list_property_graphs(&self) -> Result<Vec<String>> {
        let mut out: Vec<String> = Vec::new();
        for (_, catalog) in self.catalogs()? {
            for m in &catalog.graphs {
                if m.graph != BASE_GRAPH && !out.contains(&m.graph) {
                    out.push(m.graph.clone());
                }
            }
            if let Some(binding) = catalog.edge.as_ref().and_then(|e| e.binding.as_ref()) {
                if !binding.graph.is_empty() && !out.contains(&binding.graph) {
                    out.push(binding.graph.clone());
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Replace property graph `graph`'s whole definition with `elements`: an
    /// empty list removes the graph. One transaction; no edge and no binding
    /// changes. The caller has checked the definition against the statement
    /// that asked for it; this checks what the catalog alone can know --
    /// each table exists, an edge element is a bound edge table and a vertex
    /// element is not an edge table, each element has a label, and element
    /// names are unique.
    pub fn set_property_graph(&mut self, graph: &str, elements: &[GraphElement]) -> Result<()> {
        self.user_write()?;
        if graph.is_empty() || graph.len() > 255 {
            return Err(invalid("a property graph name is 1..255 bytes"));
        }
        for (at, element) in elements.iter().enumerate() {
            if elements[..at].iter().any(|other| other.alias == element.alias) {
                return Err(invalid(format!(
                    "graph `{graph}` names two elements `{}`: an element name is unique in its graph",
                    element.alias
                )));
            }
            if element.labels.is_empty() {
                return Err(invalid(format!("element `{}` has no label", element.alias)));
            }
            let catalog = self.catalog(element.table)?;
            let bound = catalog.edge.as_ref().is_some_and(|e| e.binding.is_some());
            match (element.edge, catalog.edge.is_some()) {
                (true, true) if bound => {}
                (true, _) => {
                    return Err(invalid(format!(
                        "`{}` is not an edge table with a direction, so it cannot be an edge element",
                        catalog.name
                    )))
                }
                (false, true) => {
                    return Err(invalid(format!(
                        "`{}` is an edge table: it is an edge element, not a vertex element",
                        catalog.name
                    )))
                }
                (false, false) => {}
            }
        }
        let catalogs = self.catalogs()?;
        let result = (|| {
            if !elements.is_empty() {
                self.enable_logical_feature(PROPERTY_GRAPH_FEATURE)?;
            }
            for (_, mut catalog) in catalogs {
                let before = catalog.clone();
                catalog.graphs.retain(|m| m.graph != graph);
                // The pre-0.18.3 record of the same graph is replaced too.
                if let Some(binding) = catalog.edge.as_mut().and_then(|e| e.binding.as_mut()) {
                    if binding.graph == graph {
                        binding.graph.clear();
                    }
                }
                for element in elements.iter().filter(|e| e.table == catalog.id) {
                    catalog.graphs.push(GraphMembership {
                        graph: graph.to_owned(),
                        alias: element.alias.clone(),
                        edge: element.edge,
                        labels: element.labels.clone(),
                    });
                }
                if catalog != before {
                    self.save_catalog(&catalog)?;
                }
            }
            Ok(())
        })();
        self.finish(result)
    }

    /// Whether edge table `c`'s edges are all gone: true when its edge type
    /// holds no edge. Every edge of a bound type starts at the table's source
    /// collection, so one bounded read there answers it.
    pub fn edge_table_is_empty(&self, c: CollectionId) -> Result<bool> {
        let catalog = self.catalog(c)?;
        let Some(edge) = catalog.edge.as_ref() else {
            return Err(invalid(format!("`{}` is not an edge table", catalog.name)));
        };
        let Some(binding) = edge.binding.as_ref() else {
            return Ok(true);
        };
        let Some((_, source)) = edge.references.iter().find(|(column, _)| *column == binding.source) else {
            return Err(corrupt("an edge binding names a column the table does not reference"));
        };
        match self.edge_endpoints(
            *source,
            GraphContextId::BASE,
            binding.edge_type,
            Direction::Outgoing,
            usize::MAX,
            1,
            crate::query::QueryBudget::unlimited(),
            || false,
        ) {
            Ok(found) => Ok(found.is_empty()),
            // More than the one asked for is "not empty".
            Err(Error::BudgetExceeded { .. }) => Ok(false),
            Err(Error::Kernel(kernel::Error::ResourceLimit(_))) => Ok(false),
            Err(other) => Err(other),
        }
    }

    /// Take edge table `c`'s direction back, so it takes no write until a
    /// graph fixes one again. Refused while it has an edge: those edges would
    /// be left with no table to read or delete them through. Its edge type
    /// stays registered.
    pub fn unbind_edge_table(&mut self, c: CollectionId) -> Result<()> {
        self.user_write()?;
        let mut catalog = self.catalog(c)?;
        let name = catalog.name.clone();
        let Some(edge) = catalog.edge.as_mut() else {
            return Err(invalid(format!("`{name}` is not an edge table")));
        };
        if edge.binding.is_none() {
            return Err(invalid(format!("`{name}` has no direction to take back")));
        }
        if catalog.graphs.iter().any(|m| m.edge && m.graph != BASE_GRAPH) {
            return Err(invalid(format!(
                "`{name}` is an edge element of a property graph: remove it from that graph first"
            )));
        }
        if !self.edge_table_is_empty(c)? {
            return Err(invalid(format!(
                "`{name}` still has edges: DELETE FROM {name} first, so no edge is left without its table"
            )));
        }
        let result = (|| {
            if let Some(edge) = catalog.edge.as_mut() {
                edge.binding = None;
            }
            catalog.graphs.retain(|m| !m.edge);
            self.save_catalog(&catalog)
        })();
        *self.bound_edge_types.borrow_mut() = None;
        self.finish(result)
    }
}
