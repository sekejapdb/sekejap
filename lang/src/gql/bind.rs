//! The binder of a `MATCH`: its patterns, resolved against the catalog and
//! the stage's scope (`docs/lang/GQL_PROFILE_DESIGN.md` §2.3, §7). The
//! stage's other statements and its `RETURN` are bound in `stage.rs`, in
//! statement order, so each statement sees what the statements before it
//! bound (rule 3 and 4).
//!
//! Two passes over one `MATCH`:
//!
//! 1. **Slots.** Every node occurrence and every named edge gets a slot of
//!    the stage's [`BindingSchema`]. A variable named again -- in the same
//!    pattern, in another comma pattern, in an earlier `MATCH` of the stage,
//!    or carried through `NEXT` -- is the SAME element: it reuses its slot,
//!    and the planner turns the second occurrence into an identity
//!    constraint (an `ExpandInto`, a bound seed, or an edge identity test).
//!    A name used as a node in one place and as an edge in another is
//!    refused, naming both places, and so is a value variable (a `LET`, a
//!    `FOR`, a returned scalar) written as an element. An anonymous node
//!    gets a hidden slot: the hop after it starts there.
//! 2. **Expressions.** Inline element predicates and the `MATCH`'s `WHERE`
//!    are lowered against the scope with the whole `MATCH` bound, so an
//!    inline predicate may name a variable bound further along the pattern
//!    (the planner then evaluates it once that variable is bound).
//!
//! Every pattern is laid out as a line by `automaton.rs`, which numbers its
//! slots and places its expressions. A chain of nodes and single edges is
//! then planned hop by hop (M2-C); a pattern with a quantifier, a
//! parenthesised subpath, a path mode, a selector or a path variable is the
//! engine's path automaton (M4-A), and a variable bound inside a
//! quantifier is a GROUP variable, list-valued outside it (§4.5).
//!
//! Labels resolve at bind: a node label is a collection, found exactly as a
//! table name is (`crate::collection`), and a missing one is an error naming
//! it; a label alternation is several collections, in the order written. An
//! edge label is an edge type, which a WRITE interns and which no DDL
//! announces, so a type no edge has used yet is kept by name, planned as
//! "matches nothing", looked up again when an execution opens, and noticed.

use super::ast::{EdgeDirection, EdgePattern, Expr, LabelExpr, NodePattern, PathPattern};
use super::automaton::{self, BoundPath};
use super::convert::Element;
use super::expr::{bound_at, conjuncts, is_group, Conjunct, Lowering};
use super::horizontal::Horizontal;
use super::schema::{BindingSchema, Name, PatternId, Provenance, SlotInfo};
use crate::sqlstate::{DATATYPE_MISMATCH, DUPLICATE_ALIAS};
use crate::{SqlError, SqlResult2};
use sekejap_core::collections::gql::{SlotId, ValueType};
use sekejap_core::collections::{CollectionId, Database, EdgeTypeId};

/// One `MATCH`: its comma patterns, and its `WHERE` split into conjuncts.
pub(crate) struct BoundMatch {
    pub(crate) patterns: Vec<BoundPattern>,
    pub(crate) where_: Vec<Conjunct>,
}

/// One comma pattern.
pub(crate) enum BoundPattern {
    /// Nodes and single edges only: planned hop by hop (M2-C).
    Chain(Chain),
    /// Anything else: one engine path search (M4-A).
    Path(BoundPath),
}

/// A chain: `edges[i]` joins `nodes[i]` and `nodes[i + 1]`.
pub(crate) struct Chain {
    pub(crate) nodes: Vec<NodeOcc>,
    pub(crate) edges: Vec<EdgeOcc>,
}

/// One node occurrence.
pub(crate) struct NodeOcc {
    pub(crate) slot: SlotId,
    /// The collections the label names, as written; `None`: unlabelled.
    pub(crate) labels: Option<Labels>,
    /// The inline `WHERE`, split into conjuncts.
    pub(crate) inline: Vec<Conjunct>,
}

/// A resolved node label and how it was written, for the messages.
pub(crate) struct Labels {
    pub(crate) ids: Box<[CollectionId]>,
    pub(crate) written: String,
}

/// One edge occurrence.
pub(crate) struct EdgeOcc {
    /// The variable's slot; `None` for an anonymous edge.
    pub(crate) var: Option<SlotId>,
    /// The types the label names; `None`: any type.
    pub(crate) types: Option<Vec<TypeRef>>,
    /// The label as written, alternatives joined by `|`, for `EXPLAIN`.
    pub(crate) label: Option<String>,
    pub(crate) direction: EdgeDirection,
    pub(crate) inline: Vec<Conjunct>,
    /// Where it is written, for a hidden slot the planner allocates.
    pub(crate) provenance: Provenance,
}

/// An edge type: interned, or a name no edge has used yet.
#[derive(Clone, Debug)]
pub(crate) enum TypeRef {
    Id(EdgeTypeId),
    Named(String),
}

/// One `MATCH`, bound against `schema`: the variables the statements before
/// it bound (a `NEXT`'s input columns first), which it extends with its own
/// elements. `pattern_no` numbers the stage's patterns across its `MATCH`es
/// and `params` records the highest `$n` read.
pub(crate) fn bind_match(
    db: &Database,
    schema: &mut BindingSchema,
    patterns: &[PathPattern],
    where_: &Option<Expr>,
    pattern_no: &mut u16,
    params: &mut usize,
    notices: &mut Vec<String>,
) -> SqlResult2<BoundMatch> {
    // Pass 1: slots and labels, every pattern laid out as a line.
    let mut layouts = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        let id = PatternId(*pattern_no);
        *pattern_no += 1;
        layouts.push(automaton::layout(db, schema, pattern, id, notices)?);
    }
    // Pass 2: expressions, against the scope with this MATCH's elements, so
    // an inline predicate may name a variable bound further along.
    let mut lowering = Lowering {
        db,
        schema: &*schema,
        params,
        admitted: Vec::new(),
        grouped: None,
        horizontal: Horizontal::Refused,
    };
    let mut bound = Vec::with_capacity(layouts.len());
    for layout in layouts {
        bound.push(layout.lower(&mut lowering)?);
    }
    let mut split = Vec::new();
    if let Some(predicate) = where_ {
        lowering.horizontal = Horizontal::Allowed;
        conjuncts(lowering.predicate(predicate)?, &mut split);
    }
    Ok(BoundMatch {
        patterns: bound,
        where_: split,
    })
}

/// A column's name when `RETURN` gives it none: a property is named by the
/// property, a variable by itself, anything else `?column?`. This is the
/// body's own rule; the design never asked a body `RETURN` to follow
/// PostgreSQL's naming (`outer_column_name` below is the outer `SELECT`'s).
pub(crate) fn column_name(expr: &Expr) -> Name {
    match expr {
        Expr::Property { property, .. } => Name::quoted(property),
        Expr::Var(name) => name.clone(),
        _ => Name::quoted("?column?"),
    }
}

/// The outer `SELECT`'s column name when it gives one none: PostgreSQL's
/// own rule for a plain SQL select list, applied to the relation's columns
/// (brief M3-D2 gap 3). An aggregate is named by its function --
/// `count`, `sum`, `avg`, `min`, `max`, `array_agg` -- a scalar function
/// call or a graph function by its function name, a cast by PostgreSQL's
/// internal type name (`int8`, `float8`, ...), and `CASE`, `COALESCE` and
/// `NULLIF` by their keyword. Everything else (an operator, a plain column
/// or variable) falls to [`column_name`]'s rule, which is `?column?` for
/// anything that is not a property or a variable.
pub(crate) fn outer_column_name(expr: &Expr) -> Name {
    match expr {
        Expr::Aggregate { func, .. } => Name::quoted(&func.written().to_ascii_lowercase()),
        Expr::Call { func, .. } => Name::quoted(&func.written().to_ascii_lowercase()),
        Expr::Graph { func, .. } => Name::quoted(&func.written().to_ascii_lowercase()),
        Expr::Cast { to, .. } => Name::quoted(to.pg_name()),
        Expr::Case { .. } => Name::quoted("case"),
        Expr::Coalesce(_) => Name::quoted("coalesce"),
        Expr::Nullif(..) => Name::quoted("nullif"),
        _ => column_name(expr),
    }
}

/// One node occurrence; `group` when it stands inside a quantifier, where
/// its variable is a group variable, a list of nodes outside it (§4.5).
pub(super) fn node_occurrence(
    db: &Database,
    schema: &mut BindingSchema,
    node: &NodePattern,
    provenance: Provenance,
    group: bool,
) -> SqlResult2<NodeOcc> {
    let labels = match &node.label {
        None => None,
        Some(label) => Some(node_labels(db, label)?),
    };
    let ty = grouped(
        ValueType::Node(labels.as_ref().map_or_else(|| Box::new([]) as Box<[_]>, |l| l.ids.clone())),
        group,
    );
    let slot = match &node.var {
        Some(name) => element_slot(schema, name, ty, provenance, true)?,
        None => schema.add(SlotInfo::hidden(ty, provenance))?,
    };
    Ok(NodeOcc {
        slot,
        labels,
        inline: Vec::new(),
    })
}

/// A node position no pattern wrote: the concatenation point a path
/// automaton needs around a quantifier (`automaton.rs`). It has a hidden
/// slot, tests nothing and binds nothing unless it starts the search.
pub(super) fn implied_node(schema: &mut BindingSchema, provenance: Provenance) -> SqlResult2<NodeOcc> {
    let slot = schema.add(SlotInfo::hidden(ValueType::Node(Box::new([])), provenance))?;
    Ok(NodeOcc {
        slot,
        labels: None,
        inline: Vec::new(),
    })
}

/// One edge occurrence; `group` as for [`node_occurrence`].
pub(super) fn edge_occurrence(
    db: &Database,
    schema: &mut BindingSchema,
    edge: &EdgePattern,
    provenance: Provenance,
    group: bool,
    notices: &mut Vec<String>,
) -> SqlResult2<EdgeOcc> {
    let (types, label) = match &edge.label {
        None => (None, None),
        Some(label) => {
            let (types, written) = edge_types(db, label, notices)?;
            (Some(types), Some(written))
        }
    };
    let var = match &edge.var {
        None => None,
        Some(name) => Some(element_slot(
            schema,
            name,
            grouped(ValueType::Edge(resolved(&types)), group),
            provenance.clone(),
            false,
        )?),
    };
    Ok(EdgeOcc {
        var,
        types,
        label,
        direction: edge.direction,
        inline: Vec::new(),
        provenance,
    })
}

/// A group variable's type: the list of its elements.
fn grouped(element: ValueType, group: bool) -> ValueType {
    if group {
        ValueType::List(Box::new(element))
    } else {
        element
    }
}

/// The slot of element variable `name`: its existing one when it is bound
/// already -- as the same kind of element, and neither time inside a
/// quantifier -- or a new one.
fn element_slot(
    schema: &mut BindingSchema,
    name: &Name,
    ty: ValueType,
    provenance: Provenance,
    node: bool,
) -> SqlResult2<SlotId> {
    if let Some(slot) = schema.resolve(name) {
        let first = schema.slot(slot);
        if matches!(first.ty, ValueType::Path) {
            return Err(SqlError::coded(DATATYPE_MISMATCH, format!(
                "variable `{name}` names the path at {} and an element at {}: one variable is one value",
                bound_at(&first.provenance),
                bound_at(&provenance),
            )));
        }
        if is_group(&first.ty) || is_group(&ty) {
            return Err(SqlError::coded(DUPLICATE_ALIAS, format!(
                "variable `{name}` is bound at {} and again at {}, and one of them is inside a quantifier: a group variable is bound at exactly one position (it is the list of that position's elements)",
                bound_at(&first.provenance),
                bound_at(&provenance),
            )));
        }
        if !matches!(first.ty, ValueType::Node(_) | ValueType::Edge(_)) {
            return Err(SqlError::coded(DATATYPE_MISMATCH, format!(
                "variable `{name}` is a value bound by {}, not an element: a pattern at {} binds a node or an edge",
                bound_at(&first.provenance),
                bound_at(&provenance),
            )));
        }
        if matches!(first.ty, ValueType::Node(_)) != node {
            let (was, is) = if node {
                (Element::Edge, Element::Node)
            } else {
                (Element::Node, Element::Edge)
            };
            return Err(SqlError::coded(DATATYPE_MISMATCH, format!(
                "variable `{name}` is {} at {} and {} at {}: one variable is one element",
                was.written(),
                bound_at(&first.provenance),
                is.written(),
                bound_at(&provenance),
            )));
        }
        return Ok(slot);
    }
    schema.add(SlotInfo {
        name: Some(name.clone()),
        ty,
        provenance,
        nullable: false,
    })
}

fn label_names(label: &LabelExpr, out: &mut Vec<String>) {
    match label {
        LabelExpr::Name(name) => {
            if !out.contains(name) {
                out.push(name.clone());
            }
        }
        LabelExpr::Or(options) => {
            for option in options {
                label_names(option, out);
            }
        }
    }
}

fn node_labels(db: &Database, label: &LabelExpr) -> SqlResult2<Labels> {
    let mut names = Vec::new();
    label_names(label, &mut names);
    let ids = names
        .iter()
        .map(|name| crate::collection(db, name))
        .collect::<SqlResult2<Vec<_>>>()?;
    Ok(Labels {
        ids: ids.into(),
        written: names.join("|"),
    })
}

/// The edge types a label names, and the label as written.
fn edge_types(
    db: &Database,
    label: &LabelExpr,
    notices: &mut Vec<String>,
) -> SqlResult2<(Vec<TypeRef>, String)> {
    let mut names = Vec::new();
    label_names(label, &mut names);
    let written = names.join("|");
    let mut types = Vec::new();
    for name in names {
        types.push(match db.edge_type(&name).map_err(SqlError::from)? {
            Some(id) => TypeRef::Id(id),
            None => {
                notices.push(format!(
                    "edge type `{name}` has no edge yet: the hop matches nothing until one is written, and the name is looked up again each time the statement runs"
                ));
                TypeRef::Named(name)
            }
        });
    }
    Ok((types, written))
}

/// The edge types a slot can hold: the interned ones of the label (a type
/// not interned yet holds no edge today); empty means any type.
fn resolved(types: &Option<Vec<TypeRef>>) -> Box<[EdgeTypeId]> {
    types
        .iter()
        .flatten()
        .filter_map(|t| match t {
            TypeRef::Id(id) => Some(*id),
            TypeRef::Named(_) => None,
        })
        .collect()
}
