//! The path, element and list functions (`docs/lang/GQL_PROFILE_DESIGN.md`
//! §4.4; brief §6; owner answers Q2, Q9, Q16): the kind each takes, the
//! type each gives, and their evaluation.
//!
//! | Function | Takes | Gives |
//! | --- | --- | --- |
//! | `PATH_LENGTH(p)` | a path | `BIGINT`: its edge count, O(1) |
//! | `PATH_FIRST(p)`, `PATH_LAST(p)` | a path | its first and last NODE (never an edge's property) |
//! | `NODES(p)`, `EDGES(p)` | a path | the list of its nodes (`len + 1`) or edges, in path order |
//! | `IS_ACYCLIC(p)`, `IS_TRAIL(p)` | a path | `BOOLEAN`: no node repeats; no edge repeats. O(length) |
//! | `ELEMENT_ID(x)` | a node or an edge | `TEXT`: opaque, unique within the query and promised no further (Q16) |
//! | `SOURCE_NODE_ID(e)`, `DESTINATION_NODE_ID(e)` | an edge | `TEXT`: the `ELEMENT_ID` of its STORED source or destination, whatever direction the pattern crossed it |
//! | `LABELS(x)` | a node or an edge | `TEXT[]`: its collection's name, or its edge type's |
//! | `PROPERTY_NAMES(x)` | a node or an edge | `TEXT[]`, sorted: the properties its row or bag HOLDS -- a stored null included, an absent property not (§2.5) |
//! | `ARRAY_LENGTH(list)` | a list | `BIGINT`: its element count, 0 for an empty list (Q9) |
//!
//! The kind is checked when the statement is compiled; `NULL` in gives
//! `NULL` out. `LABELS` and `ELEMENT_ID` read no row; `PROPERTY_NAMES` of a
//! node reads its row, one charged primary read, and of an edge its bag,
//! free when the hop carried it (`ElementReader`). A list is returned as a
//! JSON array on the way out (Q2).

use super::ast::GraphFunc;
use super::convert::Element;
use super::eval::{kind, mismatch, Evaluated};
use super::types::described;
use crate::sqlstate::DATATYPE_MISMATCH;
use crate::{SqlError, SqlResult2};
use sekejap_core::collections::gql::{BindingValue, EvalCx, ListRef, NodeRef, ValueType};
use std::collections::HashSet;

/// The kind of argument a function takes.
#[derive(Clone, Copy)]
enum Takes {
    Path,
    Element,
    Edge,
    List,
}

impl Takes {
    /// What `func` takes.
    fn of(func: GraphFunc) -> Self {
        match func {
            GraphFunc::PathLength
            | GraphFunc::PathFirst
            | GraphFunc::PathLast
            | GraphFunc::Nodes
            | GraphFunc::Edges
            | GraphFunc::IsAcyclic
            | GraphFunc::IsTrail => Self::Path,
            GraphFunc::ElementId | GraphFunc::Labels | GraphFunc::PropertyNames => Self::Element,
            GraphFunc::SourceNodeId | GraphFunc::DestinationNodeId => Self::Edge,
            GraphFunc::ArrayLength => Self::List,
        }
    }

    /// Does an argument of type `arg` fit? `Unknown` (the `NULL` literal)
    /// is any kind.
    fn fits(self, arg: &ValueType) -> bool {
        matches!(
            (self, arg),
            (_, ValueType::Unknown)
                | (Self::Path, ValueType::Path)
                | (Self::Element, ValueType::Node(_) | ValueType::Edge(_))
                | (Self::Edge, ValueType::Edge(_))
                | (Self::List, ValueType::List(_))
        )
    }

    /// As the errors name it.
    fn written(self) -> &'static str {
        match self {
            Self::Path => Element::Path.written(),
            Self::Element => "a node or an edge",
            Self::Edge => Element::Edge.written(),
            Self::List => "a list",
        }
    }
}

/// The type `func` gives over an argument of type `arg`, or the refusal
/// naming the function and what it takes.
pub(super) fn result_type(func: GraphFunc, arg: &ValueType) -> SqlResult2<ValueType> {
    let takes = Takes::of(func);
    if !takes.fits(arg) {
        return Err(SqlError::coded(DATATYPE_MISMATCH, format!(
            "{} takes {}, not {}",
            func.written(),
            takes.written(),
            described(arg)
        )));
    }
    let texts = || ValueType::List(Box::new(ValueType::Text));
    Ok(match func {
        GraphFunc::PathLength | GraphFunc::ArrayLength => ValueType::Int,
        GraphFunc::PathFirst | GraphFunc::PathLast => ValueType::Node(Box::new([])),
        GraphFunc::Nodes => ValueType::List(Box::new(ValueType::Node(Box::new([])))),
        GraphFunc::Edges => ValueType::List(Box::new(ValueType::Edge(Box::new([])))),
        GraphFunc::IsAcyclic | GraphFunc::IsTrail => ValueType::Bool,
        GraphFunc::ElementId | GraphFunc::SourceNodeId | GraphFunc::DestinationNodeId => {
            ValueType::Text
        }
        GraphFunc::Labels | GraphFunc::PropertyNames => texts(),
    })
}

/// `func` over `value`, which is not `NULL`.
pub(super) fn call(
    func: GraphFunc,
    value: &BindingValue,
    cx: &mut EvalCx<'_, '_>,
) -> Evaluated<BindingValue> {
    use BindingValue as V;
    Ok(match (func, value) {
        (GraphFunc::PathLength, V::Path(path)) => V::Int(i64::from(path.len())),
        (GraphFunc::PathFirst, V::Path(path)) => V::Node(path.start()),
        (GraphFunc::PathLast, V::Path(path)) => V::Node(path.end()),
        (GraphFunc::Nodes, V::Path(path)) => V::List(ListRef {
            items: path.nodes().into_iter().map(V::Node).collect(),
            elem: ValueType::Node(Box::new([])),
        }),
        (GraphFunc::Edges, V::Path(path)) => V::List(ListRef {
            items: path.edges().into_iter().map(V::Edge).collect(),
            elem: ValueType::Edge(Box::new([])),
        }),
        (GraphFunc::IsAcyclic, V::Path(path)) => {
            let nodes = path.nodes();
            V::Bool(nodes.iter().collect::<HashSet<_>>().len() == nodes.len())
        }
        (GraphFunc::IsTrail, V::Path(path)) => {
            let edges = path.edges();
            V::Bool(edges.iter().collect::<HashSet<_>>().len() == edges.len())
        }
        (GraphFunc::ElementId, V::Node(node)) => V::Text(node.element_id().into()),
        (GraphFunc::ElementId, V::Edge(edge)) => V::Text(edge.element_id().into()),
        (GraphFunc::SourceNodeId, V::Edge(edge)) => {
            V::Text(NodeRef(edge.key.source).element_id().into())
        }
        (GraphFunc::DestinationNodeId, V::Edge(edge)) => {
            V::Text(NodeRef(edge.key.destination).element_id().into())
        }
        (GraphFunc::Labels, V::Node(node)) => texts(vec![cx.reader.node_label(*node)?]),
        (GraphFunc::Labels, V::Edge(edge)) => texts(vec![cx.reader.edge_label(edge)?]),
        (GraphFunc::PropertyNames, V::Node(node)) => {
            texts(cx.reader.node_property_names(*node, cx.meter)?)
        }
        (GraphFunc::PropertyNames, V::Edge(edge)) => {
            texts(cx.reader.edge_property_names(edge, cx.meter)?)
        }
        (GraphFunc::ArrayLength, V::List(list)) => {
            V::Int(i64::try_from(list.items.len()).unwrap_or(i64::MAX))
        }
        (func, other) => {
            return Err(mismatch(format!(
                "{} takes {}, not {}",
                func.written(),
                Takes::of(func).written(),
                kind(other)
            )))
        }
    })
}

/// A list of texts.
fn texts(items: Vec<String>) -> BindingValue {
    BindingValue::List(ListRef {
        items: items
            .into_iter()
            .map(|item| BindingValue::Text(item.into()))
            .collect(),
        elem: ValueType::Text,
    })
}
