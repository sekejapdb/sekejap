//! Reading what a bound element holds (`docs/lang/GQL_PROFILE_DESIGN.md`
//! §2.1, §2.4, §2.5, §3.2).
//!
//! A [`NodeRef`] or an [`EdgeRef`] is an identity; its properties live in the
//! store. [`ElementReader`] is the one place a GQL execution turns the one
//! into the other, and every read it makes is charged to the execution's
//! [`GqlMeter`] -- a seeded traversal may read rows, but never for free.
//!
//! | Read | Cost |
//! | --- | --- |
//! | a node property, or a node's property names | one primary read (`primary_reads`), plus a vector sidecar when the property is a vector |
//! | an edge's bag when the edge carries it (an outgoing hop) | nothing |
//! | an edge's bag when it does not (an incoming hop) | one point read of the primary posting (`graph_edges`, as a traversal charges it) |
//! | a label | nothing charged: the collection and the edge type ride in the identity, and the name is catalog metadata |
//! | `ELEMENT_ID` | nothing: a function of the identity |
//!
//! Inside the profile a property absent from its row and a stored null are
//! one `Null` (§2.5). The difference survives only in the property NAMES: a
//! stored null is a present property, an absent one is not there.

use super::super::rows::{decode_row, project_value_or_sidecar, RowData};
use super::super::{corrupt_query, QueryResult, WorkResource};
use super::budget::GqlMeter;
use super::value::{BindingValue, EdgeRef, NodeRef};
use crate::collections::{row_key, Database, Error, ProjectedValue, KEY_FIELD};
use crate::index::graph::adjacency::primary_posting;
use crate::index::graph::{decode_properties, read_name};
use crate::{dense_v3, Kind};
use serde_json::Value;
use std::sync::Arc;

/// The name a node's external key is read under. The row stores it in a
/// reserved field, which no user field can be called.
const KEY_PROPERTY: &str = "_key";

/// Charged property reads over one database or snapshot, for one execution.
pub struct ElementReader<'db> {
    db: &'db Database,
}

impl<'db> ElementReader<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Property `name` of `node`: `Null` when the row does not hold it or
    /// holds a null. A declared field reads as its declared kind (a
    /// geometry as `Geo`, a vector as `Vector`, a JSON column as `Json`);
    /// an undeclared one, from the row's extras, by its JSON kind. `_key` is
    /// the external key.
    pub fn node_property<C: FnMut() -> bool>(
        &self,
        node: NodeRef,
        name: &str,
        meter: &mut GqlMeter<'_, C>,
    ) -> QueryResult<BindingValue> {
        let row = self.row(node, meter)?;
        let field = if name == KEY_PROPERTY {
            KEY_FIELD
        } else {
            name
        };
        let value = dense_v3::read_field_in(&row.layout, &row.bytes, field)
            .map_err(|error| corrupt_query(format!("dense-v3 row: {error}")))?;
        let kind = row
            .layout
            .fields
            .iter()
            .find(|(declared, _)| declared == field)
            .map(|(_, kind)| kind);
        let value = project_value_or_sidecar(self.db, node.0, value, meter.base())?;
        declared_value(kind, value)
    }

    /// The properties `node`'s row holds, sorted: every field stored with a
    /// value or a null, and `_key`. A field the row does not hold is not
    /// listed.
    pub fn node_property_names<C: FnMut() -> bool>(
        &self,
        node: NodeRef,
        meter: &mut GqlMeter<'_, C>,
    ) -> QueryResult<Vec<String>> {
        let row = self.row(node, meter)?;
        // A vector's presence is its state in the row; its lanes stay in the
        // sidecar, unread.
        let decoded = dense_v3::decode_with_vector_values(&row.layout, &row.bytes, |_, _| {
            Ok(Some(Value::Null))
        })
        .map_err(|error| corrupt_query(format!("dense-v3 row: {error}")))?;
        let Value::Object(fields) = decoded else {
            return Err(corrupt_query("dense-v3 row is not an object"));
        };
        let mut names: Vec<String> = fields
            .into_iter()
            .map(|(name, _)| {
                if name == KEY_FIELD {
                    KEY_PROPERTY.to_owned()
                } else {
                    name
                }
            })
            .collect();
        names.sort_unstable();
        Ok(names)
    }

    /// `edge`'s property bag: the cached one when the edge carries it,
    /// otherwise its primary posting, read once and charged.
    fn edge_bag<C: FnMut() -> bool>(
        &self,
        edge: &EdgeRef,
        meter: &mut GqlMeter<'_, C>,
    ) -> QueryResult<Arc<Value>> {
        if let Some(bag) = &edge.bag {
            return Ok(Arc::clone(bag));
        }
        meter.charge(WorkResource::GraphEdges, 1)?;
        let bytes = primary_posting(self.db, edge.key, edge.id)?;
        Ok(Arc::new(decode_properties(&bytes)?))
    }

    /// Property `name` of `edge`, by its JSON kind: `Null` when the bag does
    /// not hold it or holds a null.
    pub fn edge_property<C: FnMut() -> bool>(
        &self,
        edge: &EdgeRef,
        name: &str,
        meter: &mut GqlMeter<'_, C>,
    ) -> QueryResult<BindingValue> {
        let bag = self.edge_bag(edge, meter)?;
        Ok(bag.get(name).map_or(BindingValue::Null, json_value))
    }

    /// The properties `edge`'s bag holds, sorted, a stored null included.
    pub fn edge_property_names<C: FnMut() -> bool>(
        &self,
        edge: &EdgeRef,
        meter: &mut GqlMeter<'_, C>,
    ) -> QueryResult<Vec<String>> {
        let bag = self.edge_bag(edge, meter)?;
        let mut names: Vec<String> = bag
            .as_object()
            .into_iter()
            .flat_map(|fields| fields.keys().cloned())
            .collect();
        names.sort_unstable();
        Ok(names)
    }

    /// A node's label: the name of its collection.
    pub fn node_label(&self, node: NodeRef) -> QueryResult<String> {
        Ok(self.db.catalog(node.0.collection)?.name)
    }

    /// An edge's label: the name of its type.
    pub fn edge_label(&self, edge: &EdgeRef) -> QueryResult<String> {
        let store = self.db.store()?;
        let name = read_name(
            |key| store.get(key).map_err(Error::from),
            0,
            edge.key.edge_type.0,
        )?;
        Ok(name.name)
    }

    /// `node`'s row, one charged primary read. A node reference is made
    /// only from this snapshot, so a missing row is corruption.
    fn row<C: FnMut() -> bool>(
        &self,
        node: NodeRef,
        meter: &mut GqlMeter<'_, C>,
    ) -> QueryResult<RowData> {
        meter.charge(WorkResource::PrimaryReads, 1)?;
        let bytes = self
            .db
            .store()?
            .get(&row_key(node.0))?
            .ok_or_else(|| corrupt_query("a graph node's row is missing"))?;
        meter.base().note_row_decode();
        decode_row(self.db, bytes)
    }
}

/// `ELEMENT_ID` (design Q16): an opaque text, unique among the elements of
/// one query and promised no further. The leading `n1:` / `e1:` names the
/// encoding's version, so it can change without an old id being mistaken
/// for a new one.
impl NodeRef {
    /// `n1:` then the collection and the sequence: two collections holding
    /// the same external key hold two nodes with two ids.
    pub fn element_id(self) -> String {
        format!("n1:{}.{}", self.0.collection.0, self.0.sequence)
    }
}

impl EdgeRef {
    /// `e1:` then the stored tuple and the edge's id: parallel edges of one
    /// tuple have two ids.
    pub fn element_id(&self) -> String {
        let EdgeRef { key, id, .. } = self;
        format!(
            "e1:{}.{}:{}:{}:{}.{}:{id}",
            key.source.collection.0,
            key.source.sequence,
            key.context.0,
            key.edge_type.0,
            key.destination.collection.0,
            key.destination.sequence,
        )
    }
}

/// A projected row field as a binding value, typed by its declared kind
/// (`None`: undeclared, typed by its JSON kind).
fn declared_value(kind: Option<&Kind>, value: ProjectedValue) -> QueryResult<BindingValue> {
    let value = match value {
        ProjectedValue::Missing | ProjectedValue::Null | ProjectedValue::Value(Value::Null) => {
            return Ok(BindingValue::Null)
        }
        ProjectedValue::Value(value) => value,
    };
    Ok(match kind {
        Some(Kind::Json) => BindingValue::Json(Arc::new(value)),
        Some(Kind::Geo | Kind::Point) => BindingValue::Geo(Arc::new(value)),
        Some(Kind::Vector(_)) => {
            let lanes = value.as_array().and_then(|lanes| {
                lanes
                    .iter()
                    .map(|lane| lane.as_f64().map(|lane| lane as f32))
                    .collect::<Option<Arc<[f32]>>>()
            });
            BindingValue::Vector(lanes.ok_or_else(|| corrupt_query("projected vector"))?)
        }
        Some(Kind::Text | Kind::Int | Kind::Real | Kind::Bool) | None => json_value(&value),
    })
}

/// A JSON value by its kind: a number that fits `i64` is an integer and
/// any other a float (the SQL surface's rule); an array or an object stays
/// JSON -- a GQL list is only what the profile itself builds.
fn json_value(value: &Value) -> BindingValue {
    match value {
        Value::Null => BindingValue::Null,
        Value::Bool(b) => BindingValue::Bool(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => BindingValue::Int(i),
            None => BindingValue::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        Value::String(s) => BindingValue::Text(s.as_str().into()),
        other => BindingValue::Json(Arc::new(other.clone())),
    }
}
