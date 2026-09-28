//! EDGE TABLES: SQL's view of one edge type (`docs/core/EDGE_TABLES.md`).
//!
//! An edge table is a collection whose catalog record carries an edge tail
//! and which never holds a row. The tail names the table's `REFERENCES`
//! columns, its primary key and -- once a property graph has declared it --
//! which column is the source, which the destination, and the edge type its
//! edges are written under. The collection's LAYOUT types the edges'
//! properties; the edges themselves are ordinary edges in the base context,
//! stored once, in the adjacency keyspaces (`GRAPH_CONTRACT.md` §2.2).
//!
//! Every write goes through a key check that reads edges stored together: a
//! key names the source, the destination or both, so the edges that could
//! collide are one adjacency range (§3 of the design).

use super::adjacency::{primary_posting, AdjacencyCursor};
use super::*;
use crate::collections::KEY_FIELD;
use crate::encode_dense_v3;
use crate::Kind;

/// The logical feature bit that says this file's catalog holds at least one
/// edge table. Set in the transaction of the first `declare_edge_table`,
/// never cleared; a binary that predates it refuses the file whole, as
/// `Unsupported` (Law 8). A file that never declares one does not carry it.
pub const EDGE_TABLE_FEATURE: u64 = 0x80000;
/// The encoding of the catalog's edge tail. A record of another version is
/// refused as `Unsupported` rather than read.
const EDGE_TAIL_VERSION: u8 = 1;
/// The SQLSTATEs an edge write raises, as PostgreSQL names them.
pub const UNIQUE_VIOLATION: &str = "23505";
pub const FOREIGN_KEY_VIOLATION: &str = "23503";
pub const NOT_NULL_VIOLATION: &str = "23502";

/// Which column is which end, and the edge type the edges carry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeBinding {
    pub source: String,
    pub destination: String,
    pub edge_type: EdgeTypeId,
    /// The property graph that declared the table; empty once that graph is
    /// dropped. The binding itself outlives the graph (§2.3).
    pub graph: String,
}

/// An edge table's declaration, as its catalog record holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeTable {
    /// Each `REFERENCES` column and the collection it references, in
    /// declaration order.
    pub references: Vec<(String, CollectionId)>,
    /// The primary key's columns; empty for a table without one, where every
    /// insert is a new edge.
    pub key: Vec<String>,
    /// `None` until a property graph declares the table.
    pub binding: Option<EdgeBinding>,
}

pub(crate) type EdgeTableRecord = EdgeTable;

/// One edge read through its table: its identity and its columns -- the two
/// ends as the external keys of their rows, then the properties the bag
/// holds.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeTableRow {
    pub edge: EdgeId,
    pub values: Value,
}

/// What an upsert does when the key is taken: `ON CONFLICT DO NOTHING`, or
/// `ON CONFLICT DO UPDATE` rewriting the named property columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OnConflict {
    Nothing,
    Update(Vec<String>),
}

fn constraint(sqlstate: &'static str, message: impl Into<String>) -> Error {
    Error::Constraint {
        sqlstate,
        message: message.into(),
    }
}

impl EdgeTable {
    pub(crate) fn encode(&self, b: &mut Vec<u8>) -> Result<()> {
        let name = |b: &mut Vec<u8>, n: &str| -> Result<()> {
            if n.is_empty() || n.len() > 255 {
                return Err(invalid("an edge table column name is 1..255 bytes"));
            }
            b.push(n.len() as u8);
            b.extend_from_slice(n.as_bytes());
            Ok(())
        };
        if self.references.len() > 255 || self.key.len() > 255 {
            return Err(invalid("an edge table names at most 255 references and key columns"));
        }
        b.push(EDGE_TAIL_VERSION);
        b.push(self.references.len() as u8);
        for (column, collection) in &self.references {
            name(b, column)?;
            b.extend_from_slice(&collection.0.to_be_bytes());
        }
        b.push(self.key.len() as u8);
        for column in &self.key {
            name(b, column)?;
        }
        match &self.binding {
            None => b.push(0),
            Some(binding) => {
                b.push(1);
                name(b, &binding.source)?;
                name(b, &binding.destination)?;
                b.extend_from_slice(&binding.edge_type.0.to_be_bytes());
                if binding.graph.len() > 255 {
                    return Err(invalid("a property graph name is at most 255 bytes"));
                }
                b.push(binding.graph.len() as u8);
                b.extend_from_slice(binding.graph.as_bytes());
            }
        }
        Ok(())
    }

    pub(crate) fn decode(mut read: impl FnMut(usize) -> Result<Vec<u8>>) -> Result<Self> {
        let version = read(1)?[0];
        if version != EDGE_TAIL_VERSION {
            return Err(Error::Unsupported(format!("edge table tail version {version}")));
        }
        let name = |read: &mut dyn FnMut(usize) -> Result<Vec<u8>>| -> Result<String> {
            let n = read(1)?[0] as usize;
            if n == 0 {
                return Err(corrupt("empty edge table column name"));
            }
            String::from_utf8(read(n)?).map_err(corrupt)
        };
        let count = read(1)?[0];
        let mut references = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let column = name(&mut read)?;
            let collection = u32::from_be_bytes(read(4)?.try_into().unwrap());
            if collection == 0 {
                return Err(corrupt("edge table references collection 0"));
            }
            references.push((column, CollectionId(collection)));
        }
        let count = read(1)?[0];
        let mut key = Vec::with_capacity(count as usize);
        for _ in 0..count {
            key.push(name(&mut read)?);
        }
        let binding = match read(1)?[0] {
            0 => None,
            1 => {
                let source = name(&mut read)?;
                let destination = name(&mut read)?;
                let edge_type = u64::from_be_bytes(read(8)?.try_into().unwrap());
                if edge_type == 0 {
                    return Err(corrupt("edge table bound to edge type 0"));
                }
                let n = read(1)?[0] as usize;
                let graph = String::from_utf8(read(n)?).map_err(corrupt)?;
                Some(EdgeBinding {
                    source,
                    destination,
                    edge_type: EdgeTypeId(edge_type),
                    graph,
                })
            }
            _ => return Err(corrupt("edge table binding flag")),
        };
        Ok(Self {
            references,
            key,
            binding,
        })
    }
}

/// A bound edge table, with what every write needs resolved once.
struct Bound {
    collection: CollectionId,
    name: String,
    table: EdgeTable,
    binding: EdgeBinding,
    source_collection: CollectionId,
    destination_collection: CollectionId,
    fields: Vec<(String, Kind)>,
}

impl Bound {
    fn is_end(&self, column: &str) -> bool {
        column == self.binding.source || column == self.binding.destination
    }
}

/// One stored edge a key check or a filter read found.
struct Found {
    edge: EdgeId,
    bag: Value,
}

impl Database {
    /// The edge table declaration of collection `c`, or `None` for an
    /// ordinary collection.
    pub fn edge_table(&self, c: CollectionId) -> Result<Option<EdgeTable>> {
        Ok(self.catalog(c)?.edge)
    }

    /// The edge table that owns edge type `t`, if one does.
    pub fn edge_table_of_type(&self, t: EdgeTypeId) -> Result<Option<CollectionId>> {
        Ok(self.bound_types()?.get(&t).copied())
    }

    /// Declare collection `c` an edge table: its `REFERENCES` columns and the
    /// collection each names, and its primary key (`EDGE_TABLES.md` §2.1).
    /// The collection must hold no row; the key's columns must be columns of
    /// it, and a non-empty key must name at least one `REFERENCES` column.
    pub fn declare_edge_table(
        &mut self,
        c: CollectionId,
        references: Vec<(String, CollectionId)>,
        key: Vec<String>,
    ) -> Result<()> {
        self.user_write()?;
        let mut catalog = self.catalog(c)?;
        if catalog.edge.is_some() {
            return Err(invalid(format!("`{}` is already an edge table", catalog.name)));
        }
        let layout = self.layout(catalog.layout)?;
        if references.is_empty() {
            return Err(invalid("an edge table has at least one REFERENCES column"));
        }
        for (at, (column, target)) in references.iter().enumerate() {
            match layout.fields.iter().find(|(n, _)| n == column) {
                Some((_, Kind::Text)) => {}
                Some((_, other)) => {
                    return Err(invalid(format!(
                        "REFERENCES column `{column}` is {other:?}: an end is named by its row's key, which is TEXT"
                    )))
                }
                None => return Err(invalid(format!("REFERENCES names `{column}`, a column the table has not got"))),
            }
            if references[..at].iter().any(|(n, _)| n == column) {
                return Err(invalid(format!("`{column}` carries REFERENCES twice")));
            }
            let referenced = self.catalog(*target)?;
            if referenced.edge.is_some() {
                return Err(invalid(format!(
                    "`{column}` references `{}`, an edge table: an edge joins two rows",
                    referenced.name
                )));
            }
        }
        for (at, column) in key.iter().enumerate() {
            if !layout.fields.iter().any(|(n, _)| n == column) {
                return Err(invalid(format!("PRIMARY KEY names `{column}`, a column the table has not got")));
            }
            if key[..at].contains(column) {
                return Err(invalid(format!("PRIMARY KEY names `{column}` twice")));
            }
        }
        if !key.is_empty() && !key.iter().any(|k| references.iter().any(|(r, _)| r == k)) {
            return Err(invalid(
                "an edge table's PRIMARY KEY must name an end: a key over properties alone needs an index over edge properties, which does not exist (docs/core/EDGE_TABLES.md §3)",
            ));
        }
        if self.scan(c, None)?.next().is_some() {
            return Err(invalid(format!(
                "`{}` holds rows: a table that is declared an edge table must be empty, because its rows are not edges",
                catalog.name
            )));
        }
        catalog.edge = Some(EdgeTable {
            references,
            key,
            binding: None,
        });
        let result = (|| {
            self.enable_logical_feature(EDGE_TABLE_FEATURE)?;
            self.save_catalog(&catalog)
        })();
        self.finish(result)
    }

    /// Bind edge table `c`: `source` and `destination` are two of its
    /// `REFERENCES` columns, and its edges are written under a NEW edge type
    /// named `label` (`EDGE_TABLES.md` §2.2). A binding is permanent.
    pub fn bind_edge_table(
        &mut self,
        c: CollectionId,
        source: &str,
        destination: &str,
        label: &str,
        graph: &str,
    ) -> Result<EdgeTypeId> {
        self.user_write()?;
        let mut catalog = self.catalog(c)?;
        let Some(mut table) = catalog.edge.clone() else {
            return Err(invalid(format!(
                "`{}` is not an edge table: an edge table is a table with REFERENCES columns",
                catalog.name
            )));
        };
        if table.binding.is_some() {
            return Err(invalid(format!("`{}` is already declared by a property graph", catalog.name)));
        }
        if source == destination {
            return Err(invalid("SOURCE KEY and DESTINATION KEY name one column: an edge has two ends"));
        }
        // An empty `graph` is a direction that belongs to no graph: the base
        // graph's, which is how 0.18.3 fixes one. A name is the pre-0.18.3
        // record of the graph that declared it (`property_graph.rs`).
        if graph.len() > 255 || graph.eq_ignore_ascii_case("base") {
            return Err(invalid(format!(
                "`{graph}` cannot name a property graph: a name is at most 255 bytes and `base` is the default graph's"
            )));
        }
        for end in [source, destination] {
            if !table.references.iter().any(|(n, _)| n == end) {
                return Err(invalid(format!(
                    "`{end}` is not a REFERENCES column of `{}`: an end names a row",
                    catalog.name
                )));
            }
        }
        if !table.key.is_empty() && !table.key.iter().any(|k| k == source || k == destination) {
            return Err(invalid(
                "the PRIMARY KEY names neither end: a key over properties alone needs an index over edge properties (docs/core/EDGE_TABLES.md §3)",
            ));
        }
        // The graph is switched on by the first edge type, as `link` does it.
        self.enable_graph()?;
        if self.edge_type(label)?.is_some() {
            return Err(invalid(format!(
                "edge type `{label}` already exists: an edge table's label must be new, because edges written without its types and key cannot be adopted"
            )));
        }
        let result = (|| {
            let edge_type = self.create_edge_type(label)?;
            table.binding = Some(EdgeBinding {
                source: source.to_owned(),
                destination: destination.to_owned(),
                edge_type,
                graph: graph.to_owned(),
            });
            catalog.edge = Some(table);
            self.save_catalog(&catalog)?;
            Ok(edge_type)
        })();
        *self.bound_edge_types.borrow_mut() = None;
        self.finish(result)
    }

    /// `INSERT` one edge through its table (`EDGE_TABLES.md` §4.1). `row`
    /// names each end by its row's external key and carries the properties.
    pub fn insert_edge_row(&mut self, c: CollectionId, row: &Value) -> Result<EdgeId> {
        self.user_write()?;
        let bound = self.bound(c)?;
        let (key, bag) = self.edge_of_row(&bound, row)?;
        if self.conflict(&bound, key, &bag)?.is_some() {
            // PostgreSQL's sentence and its DETAIL, on one line.
            let values: Vec<String> = bound
                .table
                .key
                .iter()
                .map(|k| match row.get(k) {
                    Some(Value::String(text)) => text.clone(),
                    Some(other) => other.to_string(),
                    None => "null".to_owned(),
                })
                .collect();
            return Err(constraint(
                UNIQUE_VIOLATION,
                format!(
                    "duplicate key value violates the primary key of `{}`: Key ({})=({}) already exists",
                    bound.name,
                    bound.table.key.join(", "),
                    values.join(", ")
                ),
            ));
        }
        self.write_new_edge(&bound, key, &bag)
    }

    /// `INSERT ... ON CONFLICT`: insert, or on a taken key do nothing or
    /// rewrite the named properties of the edge that holds it. Returns the
    /// edge written, or `None` when nothing was.
    pub fn upsert_edge_row(
        &mut self,
        c: CollectionId,
        row: &Value,
        on_conflict: &OnConflict,
    ) -> Result<Option<EdgeId>> {
        self.user_write()?;
        let bound = self.bound(c)?;
        let (key, bag) = self.edge_of_row(&bound, row)?;
        let Some(taken) = self.conflict(&bound, key, &bag)? else {
            return self.write_new_edge(&bound, key, &bag).map(Some);
        };
        match on_conflict {
            OnConflict::Nothing => Ok(None),
            OnConflict::Update(columns) => {
                let mut merged = taken.bag.clone();
                for column in columns {
                    if bound.is_end(column) || bound.table.key.contains(column) {
                        return Err(invalid(format!(
                            "ON CONFLICT DO UPDATE SET {column}: `{column}` is part of the edge's identity"
                        )));
                    }
                    match bag.get(column) {
                        Some(v) => merged[column.as_str()] = v.clone(),
                        None => {
                            merged.as_object_mut().unwrap().remove(column);
                        }
                    }
                }
                self.check_properties(&bound, &merged)?;
                let result = (|| {
                    self.write_edge_bag(taken.edge, &merged)?;
                    Ok(Some(taken.edge))
                })();
                self.finish(result)
            }
        }
    }

    /// `UPDATE t SET ... WHERE ...`: rewrite the properties of every edge
    /// `filter` matches (§4.3). Returns how many.
    pub fn update_edge_rows(&mut self, c: CollectionId, filter: &Value, patch: &Value) -> Result<usize> {
        self.user_write()?;
        let bound = self.bound(c)?;
        let patch = patch
            .as_object()
            .ok_or_else(|| invalid("an edge patch is an object"))?;
        for column in patch.keys() {
            if bound.is_end(column) || bound.table.key.contains(column) {
                return Err(invalid(format!(
                    "UPDATE {} SET {column}: `{column}` is the edge's identity; a new identity is a DELETE and an INSERT",
                    bound.name
                )));
            }
            if !bound.fields.iter().any(|(n, _)| n == column) {
                return Err(invalid(format!("`{}` has no column `{column}`", bound.name)));
            }
        }
        let found = self.matching(&bound, filter, MAX_TUPLE_EDGES)?;
        let mut rewritten = Vec::with_capacity(found.len());
        for f in found {
            let mut bag = f.bag;
            for (column, value) in patch {
                if value.is_null() {
                    bag.as_object_mut().unwrap().remove(column);
                } else {
                    bag[column.as_str()] = value.clone();
                }
            }
            self.check_properties(&bound, &bag)?;
            rewritten.push((f.edge, bag));
        }
        let result = (|| {
            for (edge, bag) in &rewritten {
                self.write_edge_bag(*edge, bag)?;
            }
            Ok(rewritten.len())
        })();
        self.finish(result)
    }

    /// `DELETE FROM t WHERE ...`: remove every edge `filter` matches (§4.4).
    pub fn delete_edge_rows(&mut self, c: CollectionId, filter: &Value) -> Result<usize> {
        self.user_write()?;
        let bound = self.bound(c)?;
        let found = self.matching(&bound, filter, MAX_TUPLE_EDGES)?;
        let mut removed = 0;
        for f in found {
            if self.delete_edge_by_id(f.edge)? {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// `SELECT ... FROM t WHERE ...`: the edges `filter` matches, at most
    /// `limit` of them -- more is an error, never a silent cut (§5.1).
    pub fn edge_rows(&self, c: CollectionId, filter: &Value, limit: usize) -> Result<Vec<EdgeTableRow>> {
        let bound = self.bound(c)?;
        let found = self.matching(&bound, filter, limit)?;
        let mut out = Vec::with_capacity(found.len());
        for f in found {
            let source = self
                .get_by_id(f.edge.key.source)?
                .ok_or_else(|| corrupt("an edge's source row is missing"))?;
            let destination = self
                .get_by_id(f.edge.key.destination)?
                .ok_or_else(|| corrupt("an edge's destination row is missing"))?;
            let mut values = serde_json::Map::new();
            values.insert(bound.binding.source.clone(), Value::from(source.key));
            values.insert(bound.binding.destination.clone(), Value::from(destination.key));
            if let Value::Object(bag) = f.bag {
                values.extend(bag);
            }
            out.push(EdgeTableRow {
                edge: f.edge,
                values: Value::Object(values),
            });
        }
        Ok(out)
    }

    // ── internals ──────────────────────────────────────────────────────

    /// Refuse an untyped write under an edge type an edge table owns
    /// (§2.4). Free for a file that declares no edge table.
    pub(crate) fn refuse_bound_edge_type(&self, t: EdgeTypeId) -> Result<()> {
        if !self
            .index_header
            .is_some_and(|h| h.features & EDGE_TABLE_FEATURE != 0)
        {
            return Ok(());
        }
        if let Some(c) = self.bound_types()?.get(&t) {
            let name = self.catalog(*c)?.name;
            return Err(invalid(format!(
                "edge type {} belongs to edge table `{name}`: its edges carry typed properties and a key, so they are written with INSERT INTO {name}",
                t.0
            )));
        }
        Ok(())
    }

    /// The name of an edge table that references collection `c`, if any.
    pub(crate) fn edge_table_referencing(&self, c: CollectionId) -> Result<Option<String>> {
        if !self
            .index_header
            .is_some_and(|h| h.features & EDGE_TABLE_FEATURE != 0)
        {
            return Ok(None);
        }
        for (schema, name) in self.list_qualified_collections()? {
            let Some(other) = self.collection_in(&schema, &name)? else {
                continue;
            };
            if other == c {
                continue;
            }
            if let Some(edge) = self.catalog(other)?.edge {
                if edge.references.iter().any(|(_, r)| *r == c) {
                    return Ok(Some(name));
                }
            }
        }
        Ok(None)
    }

    fn bound_types(&self) -> Result<BTreeMap<EdgeTypeId, CollectionId>> {
        if let Some(map) = self.bound_edge_types.borrow().as_ref() {
            return Ok(map.clone());
        }
        let mut map = BTreeMap::new();
        if self
            .index_header
            .is_some_and(|h| h.features & EDGE_TABLE_FEATURE != 0)
        {
            for (schema, name) in self.list_qualified_collections()? {
                let Some(c) = self.collection_in(&schema, &name)? else {
                    continue;
                };
                if let Some(binding) = self.catalog(c)?.edge.and_then(|e| e.binding) {
                    map.insert(binding.edge_type, c);
                }
            }
        }
        *self.bound_edge_types.borrow_mut() = Some(map.clone());
        Ok(map)
    }

    fn bound(&self, c: CollectionId) -> Result<Bound> {
        let catalog = self.catalog(c)?;
        let Some(table) = catalog.edge.clone() else {
            return Err(invalid(format!("`{}` is not an edge table", catalog.name)));
        };
        let Some(binding) = table.binding.clone() else {
            return Err(invalid(format!(
                "`{}` is an edge table no property graph has declared yet, so neither end is the source: declare it with CREATE PROPERTY GRAPH ... EDGE TABLES ({} SOURCE KEY (...) ... DESTINATION KEY (...) ...)",
                catalog.name, catalog.name
            )));
        };
        let find = |end: &str| {
            table
                .references
                .iter()
                .find(|(n, _)| n == end)
                .map(|(_, c)| *c)
                .ok_or_else(|| corrupt("edge table binding names a column it does not reference"))
        };
        let source_collection = find(&binding.source)?;
        let destination_collection = find(&binding.destination)?;
        let fields = self.layout(catalog.layout)?.fields.clone();
        Ok(Bound {
            collection: c,
            name: catalog.name,
            table,
            binding,
            source_collection,
            destination_collection,
            fields,
        })
    }

    /// The edge a row describes: its tuple, and its property bag checked
    /// against the table's layout and column rules.
    fn edge_of_row(&self, bound: &Bound, row: &Value) -> Result<(EdgeKey, Value)> {
        let row = row
            .as_object()
            .ok_or_else(|| invalid("an edge row is an object"))?;
        for column in row.keys() {
            if !bound.fields.iter().any(|(n, _)| n == column) {
                return Err(invalid(format!("`{}` has no column `{column}`", bound.name)));
            }
        }
        let end = |column: &str, collection: CollectionId| -> Result<EntityId> {
            let key = match row.get(column) {
                Some(Value::String(key)) => key,
                None | Some(Value::Null) => {
                    return Err(constraint(
                        NOT_NULL_VIOLATION,
                        format!("null value in column `{column}` of `{}`: an edge has two ends", bound.name),
                    ))
                }
                Some(other) => return Err(invalid(format!("`{column}` names a row by its TEXT key, not {other}"))),
            };
            match self.get(collection, key)? {
                Some(entity) => Ok(entity.id),
                None => Err(constraint(
                    FOREIGN_KEY_VIOLATION,
                    format!(
                        "insert into `{}` violates its reference `{column}`: key ({key}) is not present in `{}`",
                        bound.name,
                        self.catalog(collection)?.name
                    ),
                )),
            }
        };
        let source = end(&bound.binding.source, bound.source_collection)?;
        let destination = end(&bound.binding.destination, bound.destination_collection)?;
        let mut bag = serde_json::Map::new();
        for (column, value) in row {
            if !bound.is_end(column) && !value.is_null() {
                bag.insert(column.clone(), value.clone());
            }
        }
        let mut bag = Value::Object(bag);
        let catalog = self.catalog(bound.collection)?;
        if !catalog.rules.is_empty() {
            self.apply_column_rules(&catalog, &mut bag).map_err(|e| match e {
                Error::InvalidInput(m) if m.contains("NOT NULL") => constraint(NOT_NULL_VIOLATION, m),
                other => other,
            })?;
        }
        for column in &bound.table.key {
            if !bound.is_end(column) && bag.get(column).is_none_or(Value::is_null) {
                return Err(constraint(
                    NOT_NULL_VIOLATION,
                    format!("null value in key column `{column}` of `{}`", bound.name),
                ));
            }
        }
        self.check_properties(bound, &bag)?;
        Ok((
            EdgeKey {
                source,
                context: GraphContextId::BASE,
                edge_type: bound.binding.edge_type,
                destination,
            },
            bag,
        ))
    }

    /// The bag's values are the kinds the layout declares, checked with the
    /// row encoder itself so an edge property and a row column cannot
    /// disagree about what a kind admits.
    fn check_properties(&self, bound: &Bound, bag: &Value) -> Result<()> {
        let mut doc = bag.clone();
        let object = doc.as_object_mut().unwrap();
        object.insert(bound.binding.source.clone(), Value::from("s"));
        object.insert(bound.binding.destination.clone(), Value::from("d"));
        object.insert(KEY_FIELD.to_owned(), Value::from("k"));
        let layout = crate::Layout {
            id: 1,
            fields: bound.fields.clone(),
        };
        encode_dense_v3(&layout, &doc).map_err(|e| {
            invalid(format!("an edge of `{}` does not fit its columns: {e}", bound.name))
        })?;
        Ok(())
    }

    /// The edge already holding the key `key` + `bag` would take, if any.
    fn conflict(&self, bound: &Bound, key: EdgeKey, bag: &Value) -> Result<Option<Found>> {
        if bound.table.key.is_empty() {
            return Ok(None);
        }
        let has_source = bound.table.key.contains(&bound.binding.source);
        let has_destination = bound.table.key.contains(&bound.binding.destination);
        let (near, direction) = if has_source {
            (key.source, Direction::Outgoing)
        } else {
            (key.destination, Direction::Incoming)
        };
        for f in self.walk(bound, near, direction, MAX_TUPLE_EDGES)? {
            let same = (!has_source || f.edge.key.source == key.source)
                && (!has_destination || f.edge.key.destination == key.destination)
                && bound
                    .table
                    .key
                    .iter()
                    .filter(|k| !bound.is_end(k))
                    .all(|k| f.bag.get(k) == bag.get(k));
            if same {
                return Ok(Some(f));
            }
        }
        Ok(None)
    }

    /// Every edge of the table's type at `near` in `direction`, with its bag.
    fn walk(&self, bound: &Bound, near: EntityId, direction: Direction, limit: usize) -> Result<Vec<Found>> {
        let mut postings = Vec::new();
        {
            let mut cursor = AdjacencyCursor::open(
                self,
                near,
                direction,
                GraphContextId::BASE,
                Some(bound.binding.edge_type),
            )?;
            while let Some(posting) = cursor.next_posting()? {
                let edge = posting.edge()?;
                if postings.len() == limit {
                    return Err(invalid(format!(
                        "more than {limit} edges of `{}` at one end: the read is complete or it is an error",
                        bound.name
                    )));
                }
                postings.push((edge.key, edge.id, edge.bag()?));
            }
        }
        let mut out = Vec::with_capacity(postings.len());
        for (key, id, bag) in postings {
            let bag = match bag {
                Some(bag) => bag,
                None => decode_properties(&primary_posting(self, key, id)?)?,
            };
            out.push(Found {
                edge: EdgeId { key, id },
                bag,
            });
        }
        Ok(out)
    }

    /// The edges `filter` (column = value, conjoined) matches. It must name
    /// an end, so the read is one node's edges (§4.5, §5.1).
    fn matching(&self, bound: &Bound, filter: &Value, limit: usize) -> Result<Vec<Found>> {
        let filter = filter
            .as_object()
            .ok_or_else(|| invalid("an edge filter is an object"))?;
        for column in filter.keys() {
            if !bound.fields.iter().any(|(n, _)| n == column) {
                return Err(invalid(format!("`{}` has no column `{column}`", bound.name)));
            }
        }
        let named = |column: &str, collection: CollectionId| -> Result<Option<Option<EntityId>>> {
            match filter.get(column) {
                None => Ok(None),
                Some(Value::String(key)) => Ok(Some(self.get(collection, key)?.map(|e| e.id))),
                Some(other) => Err(invalid(format!("`{column}` names a row by its TEXT key, not {other}"))),
            }
        };
        let source = named(&bound.binding.source, bound.source_collection)?;
        let destination = named(&bound.binding.destination, bound.destination_collection)?;
        let (near, direction) = match (source, destination) {
            // An end that names no row matches no edge -- checked FIRST: an
            // existing source beside a missing destination used to take the
            // walk below, where the missing end then restricted nothing, so a
            // DELETE removed edges it never named (finding vuln-a01).
            (Some(None), _) | (_, Some(None)) => return Ok(Vec::new()),
            (Some(Some(s)), _) => (s, Direction::Outgoing),
            (None, Some(Some(d))) => (d, Direction::Incoming),
            (None, None) => {
                return Err(invalid(format!(
                    "a WHERE on `{}` must name `{}` or `{}`: without an end the read is every edge of the type, and there is no index over edge properties (docs/core/EDGE_TABLES.md §4.5)",
                    bound.name, bound.binding.source, bound.binding.destination
                )))
            }
        };
        let mut out = Vec::new();
        for f in self.walk(bound, near, direction, MAX_TUPLE_EDGES)? {
            let keep = destination
                .flatten()
                .is_none_or(|d| f.edge.key.destination == d)
                && source.flatten().is_none_or(|s| f.edge.key.source == s)
                && filter
                    .iter()
                    .filter(|(k, _)| !bound.is_end(k))
                    .all(|(k, v)| f.bag.get(k) == Some(v));
            if keep {
                if out.len() == limit {
                    return Err(invalid(format!(
                        "more than {limit} edges of `{}` match: the read is complete or it is an error",
                        bound.name
                    )));
                }
                out.push(f);
            }
        }
        Ok(out)
    }

    fn write_new_edge(&mut self, bound: &Bound, key: EdgeKey, bag: &Value) -> Result<EdgeId> {
        let pair_key = bound.table.key.len() == 2
            && bound.table.key.contains(&bound.binding.source)
            && bound.table.key.contains(&bound.binding.destination);
        if pair_key {
            // One edge per pair: the tuple's own edge, id 0.
            let key = self.put_edge_typed(key, bag)?;
            Ok(EdgeId { key, id: 0 })
        } else {
            self.create_edge_typed(key, bag)
        }
    }
}
