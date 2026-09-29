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
//! | does a node's text match a query; its BM25 score | what a one-id engine query over the text index charges: one candidate, the row's norm and one point read per query term (`text_postings`), and for a phrase the row's tokens (`text_tokens`) -- never a term's whole posting list |
//!
//! Inside the profile a property absent from its row and a stored null are
//! one `Null` (§2.5). The difference survives only in the property NAMES: a
//! stored null is a present property, an absent one is not there.

use super::super::rows::{decode_row, project_value_or_sidecar, RowData};
use super::super::{
    corrupt_query, invalid_query, CandidateDriver, OrderValue, Projection, QueryFilter, QueryOrder,
    QueryRequest, QueryResult, QueryRow, WorkResource,
};
use super::budget::GqlMeter;
use super::value::{BindingValue, EdgeRef, NodeRef};
use crate::collections::{row_key, Database, IndexId, ProjectedValue, KEY_FIELD};
use crate::index::graph::adjacency::primary_posting;
use crate::index::graph::decode_properties;
use crate::index::text::TextMatch;
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
        let map = self.db.bag_map(edge.key.edge_type)?;
        if let Some(bag) = &edge.bag {
            return Ok(match map {
                Some(m) => Arc::new(m.names(bag)),
                None => Arc::clone(bag),
            });
        }
        meter.charge(WorkResource::GraphEdges, 1)?;
        let bytes = primary_posting(self.db, edge.key, edge.id)?;
        let bag = decode_properties(&bytes)?;
        Ok(Arc::new(match map {
            Some(m) => m.names(&bag),
            None => bag,
        }))
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
        Ok(self.db.graph_name_of(0, edge.key.edge_type.0)?)
    }

    /// Does `node`'s text, as text index `index` holds it, match `query`
    /// under `matching` -- the answer the collection-level text query gives
    /// for that one row? A node of another collection than the index's, an
    /// index that is not a READY text index, or a budget refusal is an
    /// error, never `false`.
    pub fn text_matches<C: FnMut() -> bool>(
        &self,
        node: NodeRef,
        index: IndexId,
        query: &str,
        matching: TextMatch,
        meter: &mut GqlMeter<'_, C>,
    ) -> QueryResult<bool> {
        per_node(matching)?;
        let text = QueryFilter::Text {
            index,
            query,
            matching,
        };
        Ok(self
            .one_id(node, Some(text), QueryOrder::EntityId, meter)?
            .is_some())
    }

    /// `node`'s BM25 score for `query` under `matching` over text index
    /// `index`: the score the collection-level BM25 order gives that row,
    /// and `0.0` when the row does not match -- where the SQL `bm25()` leaf
    /// puts a non-matching row (`ScoreExpr::Bm25`). Errors as
    /// [`ElementReader::text_matches`].
    pub fn text_score<C: FnMut() -> bool>(
        &self,
        node: NodeRef,
        index: IndexId,
        query: &str,
        matching: TextMatch,
        meter: &mut GqlMeter<'_, C>,
    ) -> QueryResult<f64> {
        per_node(matching)?;
        // No text filter: the order admits only the rows that match.
        let order = QueryOrder::Bm25 {
            index,
            query,
            matching,
        };
        match self.one_id(node, None, order, meter)? {
            None => Ok(0.0),
            Some(QueryRow {
                order: OrderValue::Bm25(score),
                ..
            }) => Ok(score),
            Some(row) => Err(corrupt_query(format!(
                "a BM25 order reported {:?}",
                row.order
            ))),
        }
    }

    /// The row of `node` a one-id engine query answers -- `node`'s id as
    /// the candidate driver, `filter` if any, and `order` -- or `None` when
    /// the query drops it. The query is paged under what is left of
    /// `meter`'s budget and charged to it.
    fn one_id<C: FnMut() -> bool>(
        &self,
        node: NodeRef,
        filter: Option<QueryFilter<'_>>,
        order: QueryOrder<'_>,
        meter: &mut GqlMeter<'_, C>,
    ) -> QueryResult<Option<QueryRow>> {
        let ids = [node.0];
        let filters: Vec<QueryFilter<'_>> = std::iter::once(QueryFilter::Ids(&ids))
            .chain(filter)
            .collect();
        let mut query = self.db.prepare_query(QueryRequest {
            collection: node.0.collection,
            filters: &filters,
            order,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Filter(0),
        })?;
        loop {
            let page = meter.engine_page(&mut query, 1)?;
            if let Some(row) = page.rows.into_iter().next() {
                return Ok(Some(row));
            }
            if page.done {
                return Ok(None);
            }
        }
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

/// Refuse the typo-tolerant [`TextMatch::Search`] per node (design Q30):
/// its dictionary walk may stop at a bound and say so in a notice
/// (`QL_CONTRACT` §4.6), and a per-node answer has nowhere to carry it.
fn per_node(matching: TextMatch) -> QueryResult<()> {
    if matches!(matching, TextMatch::Search | TextMatch::Prefix) {
        return Err(invalid_query(
            "the typo-tolerant search() is not read per node: its dictionary walk can stop at a bound with a notice a per-node answer cannot carry",
        ));
    }
    Ok(())
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
