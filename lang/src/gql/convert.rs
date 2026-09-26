//! Values across the edge of the GQL profile (`docs/lang/GQL_PROFILE_DESIGN.md`
//! §2.4, §2.5, §6.3).
//!
//! Two crossings, each made once:
//!
//! * **In, from a caller:** [`from_param`], once per execution for every
//!   bound `$n`. (A stored property enters through the engine's
//!   `ElementReader`, which already reads an absent property and a stored
//!   null as one `Null`; the SQL surface keeps the difference --
//!   `crate::projected` maps an absent field to `SqlValue::Missing` -- and
//!   nothing here changes that.)
//! * **Out, to SQL:** [`to_sql`], for a final output column. A list becomes
//!   `SqlValue::Json` holding a JSON array, so `SqlValue` gets no new
//!   variant. A node, an edge or a path has no SQL representation; the
//!   output must name a property or an id instead, and the error says so,
//!   naming the column.

use super::types;
use crate::compile::bytea_text;
use crate::functions::{format_date, format_timestamp};
use crate::sqlstate::DATATYPE_MISMATCH;
use crate::{Param, SqlError, SqlResult2, SqlValue};
use sekejap_core::collections::gql::{BindingValue, ValueType};
use serde_json::{Number, Value};
use std::fmt;

/// A bound `$n`, as the GQL profile reads it.
pub(crate) fn from_param(param: &Param) -> BindingValue {
    match param {
        Param::Null => BindingValue::Null,
        Param::Bool(b) => BindingValue::Bool(*b),
        Param::Int(i) => BindingValue::Int(*i),
        Param::Float(f) => BindingValue::Float(*f),
        Param::Text(t) => BindingValue::Text(t.as_str().into()),
        Param::Vector(v) => BindingValue::Vector(v.as_slice().into()),
        Param::Json(v) => BindingValue::Json(v.clone().into()),
    }
}

/// A bound `$n` in a list position (`FOR x IN $n`): a JSON array, whose
/// elements become the list's values -- a string text, a whole number an
/// integer, any other number a float, `true`/`false` a boolean, `null`
/// NULL, and an object or array JSON. The list's element type is the one
/// kind its non-null elements share (integers among floats read as
/// floats); a list mixing other kinds is refused, as a list the binder
/// built from mixed values would be (design §2.2). `NULL` is a NULL list,
/// which unnests to no row. A vector is not a list (brief §7).
pub(crate) fn list_param(param: &Param, n: usize) -> SqlResult2<BindingValue> {
    let items = match param {
        Param::Null => return Ok(BindingValue::Null),
        Param::Json(Value::Array(items)) => items,
        other => {
            return Err(SqlError::Parameter(format!(
                "${n} is read as a list (`FOR ... IN ${n}`): bind a JSON array, not {}",
                match other {
                    Param::Vector(_) => "a vector",
                    Param::Json(_) => "a JSON value that is not an array",
                    _ => "a scalar",
                }
            )))
        }
    };
    let values = items
        .iter()
        .map(|item| match item {
            Value::Null => BindingValue::Null,
            Value::Bool(b) => BindingValue::Bool(*b),
            Value::Number(number) => match number.as_i64() {
                Some(i) => BindingValue::Int(i),
                None => BindingValue::Float(number.as_f64().unwrap_or(f64::NAN)),
            },
            Value::String(text) => BindingValue::Text(text.as_str().into()),
            other => BindingValue::Json(other.clone().into()),
        })
        .collect();
    types::list_of(values).map_err(|(a, b)| {
        SqlError::Parameter(format!(
            "${n} is a list mixing {} and {} elements: a list holds one kind",
            types::described(&a),
            types::described(&b)
        ))
    })
}

/// A final output column's value, as SQL reads it, under the column's
/// type `ty`. `column` is the column's name, which is what the error names.
///
/// The type decides how a value prints: a `TIMESTAMPTZ` or a `DATE`, stored
/// as integer microseconds, prints as ISO-8601 text exactly as a collection
/// `SELECT` prints it, and a list's items print as its element type does.
pub(crate) fn to_sql(value: &BindingValue, ty: &ValueType, column: &str) -> SqlResult2<SqlValue> {
    Ok(match (ty, value) {
        (_, BindingValue::Null) => SqlValue::Null,
        (ValueType::Timestamp, BindingValue::Int(micros)) => SqlValue::Text(format_timestamp(*micros)),
        (ValueType::Date, BindingValue::Int(micros)) => SqlValue::Text(format_date(*micros)),
        (_, BindingValue::Bool(b)) => SqlValue::Bool(*b),
        (_, BindingValue::Int(i)) => SqlValue::Int(*i),
        (_, BindingValue::Float(f)) => SqlValue::Float(*f),
        (_, BindingValue::Text(t)) => SqlValue::Text(t.to_string()),
        (_, BindingValue::Bytes(b)) => SqlValue::Text(bytea_text(b)),
        (
            _,
            BindingValue::Json(_)
            | BindingValue::Geo(_)
            | BindingValue::Vector(_)
            | BindingValue::List(_),
        ) => SqlValue::Json(to_json(value, ty, column)?),
        (_, BindingValue::Node(_) | BindingValue::Edge(_) | BindingValue::Path(_)) => {
            return Err(not_a_value(element_of(value), &column, "is"))
        }
    })
}

/// A value of type `ty` as JSON: what a vector, a geometry and a list
/// travel as, and what a list's items become, each printed as
/// [`to_sql`] prints its type. A geometry is its GeoJSON and a vector its
/// array of numbers, as the SQL surface projects those columns.
fn to_json(value: &BindingValue, ty: &ValueType, column: &str) -> SqlResult2<Value> {
    Ok(match (ty, value) {
        (_, BindingValue::Null) => Value::Null,
        (ValueType::Timestamp, BindingValue::Int(micros)) => Value::String(format_timestamp(*micros)),
        (ValueType::Date, BindingValue::Int(micros)) => Value::String(format_date(*micros)),
        (_, BindingValue::Bool(b)) => Value::Bool(*b),
        (_, BindingValue::Int(i)) => Value::from(*i),
        (_, BindingValue::Float(f)) => Value::Number(finite(*f, column)?),
        (_, BindingValue::Text(t)) => Value::String(t.to_string()),
        (_, BindingValue::Bytes(b)) => Value::String(bytea_text(b)),
        (_, BindingValue::Json(v) | BindingValue::Geo(v)) => (**v).clone(),
        (_, BindingValue::Vector(lanes)) => Value::Array(
            lanes
                .iter()
                .map(|lane| finite(f64::from(*lane), column).map(Value::Number))
                .collect::<SqlResult2<_>>()?,
        ),
        (_, BindingValue::List(list)) => {
            let element = match ty {
                ValueType::List(element) => element,
                _ => &list.elem,
            };
            Value::Array(
                list.items
                    .iter()
                    .map(|item| to_json(item, element, column))
                    .collect::<SqlResult2<_>>()?,
            )
        }
        // Only a list's items reach this arm: `to_sql` stops a bare element.
        (_, BindingValue::Node(_) | BindingValue::Edge(_) | BindingValue::Path(_)) => {
            return Err(not_a_value(element_of(value), &column, "holds"))
        }
    })
}

/// JSON has no NaN and no infinity, and writing `null` in their place
/// would be a wrong answer, so the column is refused instead.
fn finite(f: f64, column: &str) -> SqlResult2<Number> {
    Number::from_f64(f).ok_or_else(|| {
        SqlError::unsupported(format!(
            "column `{column}` holds {f} inside a list, which a JSON array cannot carry"
        ))
    })
}

/// A value SQL cannot represent: an element of the graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Element {
    Node,
    Edge,
    Path,
}

impl Element {
    /// The element as every message names it.
    pub(crate) fn written(self) -> &'static str {
        match self {
            Self::Node => "a node",
            Self::Edge => "an edge",
            Self::Path => "a path",
        }
    }
}

/// The element a value that reached `not_a_value` is.
fn element_of(value: &BindingValue) -> Element {
    match value {
        BindingValue::Node(_) => Element::Node,
        BindingValue::Edge(_) => Element::Edge,
        _ => Element::Path,
    }
}

/// The element a slot of type `ty` holds, if it holds one: the binder
/// refuses such an output column before any row exists.
pub(crate) fn element_type(ty: &ValueType) -> Option<Element> {
    match ty {
        ValueType::Node(_) => Some(Element::Node),
        ValueType::Edge(_) => Some(Element::Edge),
        ValueType::Path => Some(Element::Path),
        _ => None,
    }
}

/// The error for a node, an edge or a path reaching SQL: what the column
/// `is` (or, for a list, `holds`), and what to return instead.
pub(crate) fn not_a_value(element: Element, column: &dyn fmt::Display, verb: &str) -> SqlError {
    let instead = match element {
        Element::Node => format!("`{column}._key` or `ELEMENT_ID({column})`"),
        Element::Edge => format!("a property of it or `ELEMENT_ID({column})`"),
        Element::Path => "values computed from it".to_owned(),
    };
    SqlError::coded(DATATYPE_MISMATCH, format!(
        "`{column}` {verb} {}, which SQL cannot represent; return {instead}",
        element.written()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sekejap_core::collections::gql::{EdgeRef, ListRef, NodeRef, PathRef, ValueType};
    use sekejap_core::collections::{CollectionId, EdgeKey, EdgeTypeId, EntityId, GraphContextId};
    use serde_json::json;
    use std::sync::Arc;

    fn entity(sequence: u64) -> EntityId {
        EntityId {
            collection: CollectionId(1),
            sequence,
        }
    }

    fn edge() -> EdgeRef {
        EdgeRef {
            key: EdgeKey {
                source: entity(1),
                context: GraphContextId::BASE,
                edge_type: EdgeTypeId(1),
                destination: entity(2),
            },
            id: 7,
            bag: None,
        }
    }

    fn list(elem: ValueType, items: Vec<BindingValue>) -> BindingValue {
        BindingValue::List(ListRef {
            items: items.into(),
            elem,
        })
    }

    #[test]
    fn scalars_convert_to_the_sql_value_of_their_kind() {
        let out = |v: BindingValue| to_sql(&v, &ValueType::Unknown, "c").unwrap();
        assert_eq!(out(BindingValue::Null), SqlValue::Null);
        assert_eq!(out(BindingValue::Bool(false)), SqlValue::Bool(false));
        assert_eq!(out(BindingValue::Int(42)), SqlValue::Int(42));
        assert_eq!(out(BindingValue::Float(0.5)), SqlValue::Float(0.5));
        assert!(matches!(out(BindingValue::Float(f64::NAN)), SqlValue::Float(f) if f.is_nan()));
        assert_eq!(
            out(BindingValue::Text("b1".into())),
            SqlValue::Text("b1".into())
        );
        assert_eq!(
            out(BindingValue::Json(Arc::new(json!({"k": [1]})))),
            SqlValue::Json(json!({"k": [1]}))
        );
        let point = json!({"type": "Point", "coordinates": [1.0, 2.0]});
        assert_eq!(
            out(BindingValue::Geo(Arc::new(point.clone()))),
            SqlValue::Json(point)
        );
        assert_eq!(
            out(BindingValue::Vector(Arc::from([0.5f32, -1.0]))),
            SqlValue::Json(json!([0.5, -1.0]))
        );
        // `bytea` prints as PostgreSQL's hex output, as `ST_AsBinary` does.
        assert_eq!(
            out(BindingValue::Bytes(Arc::from([0x01u8, 0xab]))),
            SqlValue::Text("\\x01ab".into())
        );
    }

    #[test]
    fn a_list_converts_to_a_json_array() {
        let out = |v: BindingValue| to_sql(&v, &ValueType::Unknown, "c").unwrap();
        let ints = list(
            ValueType::Int,
            vec![
                BindingValue::Int(1),
                BindingValue::Null,
                BindingValue::Int(3),
            ],
        );
        assert_eq!(out(ints), SqlValue::Json(json!([1, null, 3])));
        let texts = list(
            ValueType::Text,
            vec![
                BindingValue::Text("t1".into()),
                BindingValue::Text("a \"b\"".into()),
            ],
        );
        assert_eq!(out(texts), SqlValue::Json(json!(["t1", "a \"b\""])));
        assert_eq!(
            out(list(ValueType::Float, vec![])),
            SqlValue::Json(json!([]))
        );
        let floats = list(ValueType::Float, vec![BindingValue::Float(1.5)]);
        assert_eq!(out(floats), SqlValue::Json(json!([1.5])));
    }

    #[test]
    fn a_non_finite_float_in_a_list_is_refused_naming_the_column() {
        // JSON has no NaN or infinity, and writing null instead would be a
        // wrong answer.
        let v = list(ValueType::Float, vec![BindingValue::Float(f64::INFINITY)]);
        let err = to_sql(&v, &ValueType::Unknown, "costs").unwrap_err();
        assert!(err.to_string().contains("`costs`"), "{err}");
    }

    #[test]
    fn nodes_edges_and_paths_are_not_sql_values_and_the_error_names_the_variable() {
        let person = BindingValue::Node(NodeRef(entity(1)));
        let err = to_sql(&person, &ValueType::Unknown, "person").unwrap_err();
        let text = err.to_string();
        assert!(matches!(err, SqlError::Coded { sqlstate: "42804", .. }), "{text}");
        assert!(text.contains("`person`"), "{text}");
        assert!(text.contains("person._key"), "{text}");
        assert!(text.contains("ELEMENT_ID(person)"), "{text}");

        let err = to_sql(&BindingValue::Edge(edge()), &ValueType::Unknown, "r").unwrap_err();
        assert!(err.to_string().contains("`r` is an edge"), "{err}");
        assert!(err.to_string().contains("ELEMENT_ID(r)"), "{err}");

        let path = PathRef::new(NodeRef(entity(1))).extend(edge(), true, NodeRef(entity(2)));
        let err = to_sql(&BindingValue::Path(path), &ValueType::Unknown, "p").unwrap_err();
        assert!(err.to_string().contains("`p` is a path"), "{err}");

        // Inside a list too: a list of nodes is not a list of values.
        let nodes = list(
            ValueType::Node(Box::new([CollectionId(1)])),
            vec![BindingValue::Node(NodeRef(entity(1)))],
        );
        let err = to_sql(&nodes, &ValueType::Unknown, "members").unwrap_err();
        assert!(err.to_string().contains("`members` holds a node"), "{err}");
    }

    #[test]
    fn every_param_kind_converts() {
        assert!(matches!(from_param(&Param::Null), BindingValue::Null));
        assert!(matches!(
            from_param(&Param::Bool(true)),
            BindingValue::Bool(true)
        ));
        assert!(matches!(from_param(&Param::Int(-9)), BindingValue::Int(-9)));
        assert!(matches!(from_param(&Param::Float(0.25)), BindingValue::Float(f) if f == 0.25));
        assert!(
            matches!(from_param(&Param::Text("p1".into())), BindingValue::Text(t) if &*t == "p1")
        );
        assert!(matches!(
            from_param(&Param::Vector(vec![1.0, 2.0])),
            BindingValue::Vector(v) if *v == [1.0, 2.0]
        ));
        assert!(matches!(
            from_param(&Param::Json(json!(["t1", "t2"]))),
            BindingValue::Json(v) if *v == json!(["t1", "t2"])
        ));
    }
}
