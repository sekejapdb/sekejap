//! `EXPLAIN` of a GQL plan: what [`GqlPlan::describe`] prints -- each
//! stage's slot schema, every operator with its seed access, its
//! predicates' placement and the resources that can stop it -- read from
//! the plan the planner built, never from the statement's text.

use super::super::expr::{bound_at, Ex};
use super::super::schema::{BindingSchema, Provenance};
use super::super::stage::StageView;
use super::super::types::spelling;
use super::{flat, Access, ContextRef, FilterAt, GqlPlan, Op};
use super::super::bind::TypeRef;
use crate::{SqlError, SqlResult2};
use sekejap_core::collections::gql::{
    BindingValue, ExprId, PathLink, PathSearch, SlotId, Target, ValueType,
};
use sekejap_core::collections::{CollectionId, Database, Direction};

impl GqlPlan {
    /// The plan as `EXPLAIN` prints it, minus what a run adds: the graph,
    /// the statement's normal form, each stage's slot schema, every operator
    /// with its seed access, its predicates' placement and the resources
    /// that can stop it, the typed columns, the budget and rebind status.
    pub(crate) fn describe(&self, db: &Database) -> SqlResult2<String> {
        let graph = if self.graph.eq_ignore_ascii_case("base") {
            "the base graph"
        } else if !self.reads_graph {
            "not read: no pattern has an edge"
        } else {
            match self.context {
                ContextRef::Id(_) => "a named context",
                ContextRef::Named(_) => "a named context no edge has used yet: looked up again each time the statement runs",
            }
        };
        let count = self.stages.len();
        let mut out = format!(
            "GQL plan over graph `{}` ({graph}), {count} stage{}\n",
            self.graph,
            if count == 1 { "" } else { "s" }
        );
        out.push_str(&format!("statement: {}\n", self.statement));
        let mut renamed = Vec::new();
        for (at, stage) in self.stages.iter().enumerate() {
            let schema = &stage.schema;
            let end = self
                .stages
                .get(at + 1)
                .map_or(self.ops.len(), |next| next.first_op);
            let ops = &self.ops[stage.first_op..end];
            if stage.outer {
                out.push_str(&format!(
                    "outer SELECT: reads the rows stage {at} returned, as the relation's columns, and nothing else of them\n"
                ));
                out.push_str(&format!("stage {} (the outer SELECT):\n  slots:\n", at + 1));
            } else {
                if at > 0 {
                    out.push_str(&format!(
                        "NEXT: stage {} reads the rows stage {at} returned, and nothing else of them\n",
                        at + 1
                    ));
                }
                out.push_str(&format!("stage {}:\n  slots:\n", at + 1));
            }
            for at in 0..schema.width() {
                let slot = SlotId(at as u16);
                let info = schema.slot(slot);
                let ty = match &info.ty {
                    ValueType::List(of) => match &**of {
                        ValueType::Node(ids) => format!("list of nodes of {}", collections(db, ids)?),
                        ValueType::Edge(_) => format!("list of edges of {}", edge_label(ops, slot)),
                        _ => spelling(&info.ty).to_owned(),
                    },
                    ValueType::Node(ids) => format!("node of {}", collections(db, ids)?),
                    ValueType::Edge(_) => format!("edge of {}", edge_label(ops, slot)),
                    ValueType::Path => "path".to_owned(),
                    other => spelling(other).to_owned(),
                };
                // A group variable is always a list, empty for zero
                // iterations; an element or a value is null only where the
                // binder found it may be (`SlotInfo::nullable`).
                let null = if info.nullable { "nullable" } else { "never null" };
                let origin = match info.provenance {
                    Provenance::Returned { stage } => format!("returned by stage {stage}"),
                    ref other => format!("bound at {}", bound_at(other)),
                };
                out.push_str(&format!(
                    "    {at} {}: {ty}, {null}, {origin}\n",
                    var(slot, schema)
                ));
            }
            out.push_str("  operators, first to last:\n");
            for (n, op) in ops.iter().enumerate() {
                let n = (stage.first_op + n + 1).to_string();
                self.describe_op(op, &n, at > 0, schema, stage, &mut renamed, &mut out);
            }
        }
        let columns: Vec<String> = self
            .columns
            .iter()
            .zip(&self.types)
            .map(|(name, ty)| format!("{name} {}", spelling(ty)))
            .collect();
        out.push_str(&format!("  columns: {}\n", columns.join(", ")));
        out.push_str(
            "budget: every operator stops at the deadline and at a cancel; the work it charges (the resources above, and binding_rows) is bounded by the caller's budget, and the memory resources (sort_bytes, list_bytes, queue_entries, predecessor_arcs) by their fixed caps, which no budget lifts\n",
        );
        let mut rebind = "rebind: yes -- a GQL plan folds no parameter value: every $n is read when an execution opens".to_owned();
        if matches!(self.context, ContextRef::Named(_)) || !renamed.is_empty() {
            rebind.push_str(", and the names no edge has used yet are looked up again then");
        }
        out.push_str(&rebind);
        out.push('\n');
        Ok(out)
    }

    /// One operator of `EXPLAIN`, numbered `n`, and an `OptionalApply`'s
    /// inner steps after it, numbered `n.1`, `n.2`, ... and indented.
    /// `per_row`: the operator runs again for each input row (a later stage,
    /// or an inner side).
    #[allow(clippy::too_many_arguments)]
    fn describe_op(
        &self,
        op: &Op,
        n: &str,
        per_row: bool,
        schema: &BindingSchema,
        stage: &StageView,
        renamed: &mut Vec<String>,
        out: &mut String,
    ) {
        let text = |id: &ExprId| &self.texts[id.0 as usize];
        let indent = " ".repeat(4 + 2 * n.matches('.').count());
        match op {
            Op::Optional { inner, introduced } => {
                let named: Vec<String> = introduced
                    .iter()
                    .filter(|slot| schema.slot(**slot).name.is_some())
                    .map(|slot| var(*slot, schema))
                    .collect();
                let nulls = if named.is_empty() {
                    String::new()
                } else {
                    format!(" with {} NULL", named.join(", "))
                };
                let steps = match inner.len() {
                    1 => format!("step {n}.1 gives"),
                    k => format!("steps {n}.1-{n}.{k} give"),
                };
                out.push_str(&format!(
                    "{indent}{n}. OptionalApply: per input row, every row {steps} from it, or that row once{nulls} when they give none -- charges binding_rows\n"
                ));
                for (at, op) in inner.iter().enumerate() {
                    let n = format!("{n}.{}", at + 1);
                    self.describe_op(op, &n, true, schema, stage, renamed, out);
                }
            }
            Op::Seed { out: slot, access, label, .. } => {
                let v = var(*slot, schema);
                let of = label.as_deref().unwrap_or("every collection");
                let (mut how, charges) = match access {
                    Access::Key(key) => (
                        format!("key lookup of {} in {of}", text(key)),
                        "key_postings, binding_rows",
                    ),
                    Access::Index { name, field, op, value } => (
                        format!("index `{name}` on {of} ({field} {} {value})", op.written()),
                        "scalar_postings, candidates, binding_rows",
                    ),
                    Access::Bound => {
                        (format!("the node already bound in {v}"), "binding_rows")
                    }
                    Access::Scan => (format!("SCAN of {of}"), "candidates, binding_rows"),
                };
                if per_row && !matches!(access, Access::Bound) {
                    how.push_str(", re-evaluated per input row");
                }
                out.push_str(&format!("{indent}{n}. Seed {v}: {how} -- charges {charges}\n"));
            }
            Op::Expand { from, edge, to, hop } => {
                let named = edge.filter(|slot| schema.slot(*slot).name.is_some());
                let inside = format!(
                    "{}{}",
                    named.map_or_else(String::new, |slot| var(slot, schema)),
                    hop.label.as_ref().map_or_else(String::new, |l| format!(":{l}"))
                );
                let arrow = match hop.direction {
                    Direction::Outgoing => format!("-[{inside}]->"),
                    Direction::Incoming => format!("<-[{inside}]-"),
                    Direction::Both => format!("-[{inside}]-"),
                };
                let walks = walks(hop.direction);
                let (far, target) = match to {
                    Target::New(slot) => {
                        let v = var(*slot, schema);
                        let of = hop.far_label.as_deref().unwrap_or("any collection");
                        (v.clone(), format!("{v} a new node of {of}"))
                    }
                    Target::Bound(slot) => {
                        let v = var(*slot, schema);
                        (v.clone(), format!("into {v}, already bound (ExpandInto)"))
                    }
                };
                out.push_str(&format!(
                    "{indent}{n}. Expand {} {arrow} {far}: {walks}, {target} -- charges graph_edges, binding_rows\n",
                    var(*from, schema)
                ));
                for t in hop.types.iter().flatten() {
                    if let TypeRef::Named(name) = t {
                        renamed.push(name.clone());
                        out.push_str(&format!(
                            "{indent}     edge type `{name}`: no edge has used it yet; the hop matches nothing until one is written, and the name is looked up again each time the statement runs\n"
                        ));
                    }
                }
                if let Some(filter) = &hop.edge_filter {
                    out.push_str(&format!(
                        "{indent}     edge filter, per edge: {} -- an outgoing edge carries its bag; an incoming one charges graph_edges\n",
                        text(filter)
                    ));
                }
                if let Some(filter) = &hop.far_filter {
                    out.push_str(&format!(
                        "{indent}     far filter, per far node: {} -- charges primary_reads\n",
                        text(filter)
                    ));
                }
            }
            Op::Filter { predicate, at } => {
                let place = match at {
                    FilterAt::Seed => "right after the seed",
                    FilterAt::AfterPattern => "after the pattern",
                    FilterAt::Statement => "as the FILTER statement",
                    FilterAt::Outer => {
                        "as the outer WHERE, over the relation's rows (not pushed into the search)"
                    }
                    FilterAt::Having => "as the outer HAVING, over the finished group",
                };
                out.push_str(&format!(
                    "{indent}{n}. Filter {place}: {} -- charges primary_reads, graph_edges\n",
                    text(predicate)
                ));
            }
            Op::Project { cols, .. } => {
                let items: Vec<String> = stage
                    .columns
                    .iter()
                    .zip(cols.iter())
                    .enumerate()
                    .map(|(at, (name, col))| match name {
                        Some(name) => format!("{name} := {}", text(col)),
                        None => format!("#{at} := {} (a sort key, not returned)", text(col)),
                    })
                    .collect();
                out.push_str(&format!(
                    "{indent}{n}. Project {} -- charges primary_reads, graph_edges\n",
                    items.join(", ")
                ));
            }
            Op::PathSearch {
                from,
                automaton,
                search,
                reach,
                ..
            } => {
                let selector = match search {
                    PathSearch::Enumerate => "every path",
                    PathSearch::Any => "ANY path",
                    PathSearch::Shortest => "ANY SHORTEST path",
                    PathSearch::Cheapest { .. } => "ANY CHEAPEST path",
                };
                out.push_str(&format!(
                    "{indent}{n}. PathSearch from {}: {selector}, mode {:?}, {} node positions -- charges graph_edges, path_states, queue_entries, predecessor_arcs, binding_rows\n",
                    var(*from, schema),
                    automaton.mode,
                    automaton.states.len()
                ));
                for (position, state) in automaton.states.iter().enumerate() {
                    if let Some(filter) = state.filter {
                        out.push_str(&format!(
                            "{indent}     node filter, per node at position {position}: {} -- charges primary_reads\n",
                            text(&filter)
                        ));
                    }
                }
                for (step, link) in automaton.links.iter().enumerate() {
                    if let PathLink::Edge(edge) = link {
                        if let Some(filter) = edge.filter {
                            out.push_str(&format!(
                                "{indent}     edge filter, per edge: {} (step {step}) -- an outgoing edge carries its bag; an incoming one charges graph_edges\n",
                                text(&filter)
                            ));
                        }
                    }
                }
                if let PathSearch::Cheapest { cost } = search {
                    out.push_str(&format!("{indent}     cost, per edge: {}\n", text(cost)));
                }
                if let Some(refusal) = reach {
                    out.push_str(&format!("{indent}     {}\n", refusal.describe()));
                }
            }
            Op::Reach(reach) => out.push_str(&reach.describe(n, &indent, schema, &|id| text(id).clone())),
            Op::Table(op) => {
                let line = op.describe(
                    &|id| self.texts[id.0 as usize].clone(),
                    &|slot| var(slot, schema),
                );
                out.push_str(&format!("{indent}{n}. {line}\n"));
            }
        }
    }
}

/// The written label of the edge that a hop or a path search of `ops` (one
/// stage's) binds into `slot`, or "any type" when it has none.
fn edge_label(ops: &[Op], slot: SlotId) -> &str {
    flat(ops)
        .into_iter()
        .find_map(|op| match op {
            Op::Expand { edge: Some(e), hop, .. } if *e == slot => Some(hop.label.as_deref()),
            Op::PathSearch {
                automaton, labels, ..
            } => automaton.links.iter().zip(labels).find_map(|(link, label)| match link {
                PathLink::Edge(step) if step.bind == Some(slot) => Some(label.as_deref()),
                _ => None,
            }),
            _ => None,
        })
        .flatten()
        .unwrap_or("any type")
}

/// The edges a walk in `direction` reads, as `EXPLAIN` names them.
pub(super) fn walks(direction: Direction) -> &'static str {
    match direction {
        Direction::Outgoing => "outgoing edges",
        Direction::Incoming => "incoming edges",
        Direction::Both => "edges in either direction",
    }
}

/// A slot as `EXPLAIN` names it: its variable, or `#n` for a slot the
/// statement did not name.
pub(super) fn var(slot: SlotId, schema: &BindingSchema) -> String {
    schema
        .slot(slot)
        .name
        .as_ref()
        .map_or_else(|| format!("#{}", slot.0), ToString::to_string)
}

/// An expression as `EXPLAIN` prints it: every binary operator in
/// parentheses, as the statement's normal form writes it.
pub(crate) fn show(ex: &Ex, schema: &BindingSchema) -> String {
    match ex {
        Ex::Const(value) => match value {
            BindingValue::Null => "NULL".to_owned(),
            BindingValue::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_owned(),
            BindingValue::Int(i) => i.to_string(),
            BindingValue::Float(f) => format!("{f:?}"),
            BindingValue::Text(t) => format!("'{}'", t.replace('\'', "''")),
            other => format!("{other:?}"),
        },
        Ex::Param(at) => format!("${}", at + 1),
        Ex::Slot(slot) => var(*slot, schema),
        Ex::NodeProperty(slot, field) | Ex::EdgeProperty(slot, field) => {
            format!("{}.{field}", var(*slot, schema))
        }
        Ex::Compare(op, left, right) => format!(
            "({} {} {})",
            show(left, schema),
            op.written(),
            show(right, schema)
        ),
        Ex::Not(inner) => format!("(NOT {})", show(inner, schema)),
        Ex::And(left, right) => format!("({} AND {})", show(left, schema), show(right, schema)),
        Ex::Or(left, right) => format!("({} OR {})", show(left, schema), show(right, schema)),
        Ex::Neg(inner) => format!("(-{})", show(inner, schema)),
        Ex::Arith(op, left, right) => format!(
            "({} {} {})",
            show(left, schema),
            op.written(),
            show(right, schema)
        ),
        Ex::Concat(left, right) => format!("({} || {})", show(left, schema), show(right, schema)),
        Ex::IsNull(inner) => format!("({} IS NULL)", show(inner, schema)),
        Ex::Member(needle, list) => format!("({} IN {})", show(needle, schema), show(list, schema)),
        Ex::In(needle, list) => format!(
            "({} IN ({}))",
            show(needle, schema),
            list.iter().map(|item| show(item, schema)).collect::<Vec<_>>().join(", ")
        ),
        Ex::Case {
            operand,
            branches,
            otherwise,
        } => {
            let mut out = String::from("CASE");
            if let Some(operand) = operand {
                out.push_str(&format!(" {}", show(operand, schema)));
            }
            for (when, then) in branches {
                out.push_str(&format!(" WHEN {} THEN {}", show(when, schema), show(then, schema)));
            }
            if let Some(otherwise) = otherwise {
                out.push_str(&format!(" ELSE {}", show(otherwise, schema)));
            }
            out.push_str(" END");
            out
        }
        Ex::Cast(inner, to) => format!("CAST({} AS {})", show(inner, schema), to.written()),
        Ex::Coalesce(items) => format!(
            "COALESCE({})",
            items.iter().map(|item| show(item, schema)).collect::<Vec<_>>().join(", ")
        ),
        Ex::Nullif(first, second) => {
            format!("NULLIF({}, {})", show(first, schema), show(second, schema))
        }
        Ex::Call(func, args) => format!(
            "{}({})",
            func.written(),
            args.iter().map(|arg| show(arg, schema)).collect::<Vec<_>>().join(", ")
        ),
        Ex::Item(slot) => var(*slot, schema),
        Ex::Graph(func, arg) => format!("{}({})", func.written(), show(arg, schema)),
        Ex::List(items) => format!(
            "[{}]",
            items.iter().map(|item| show(item, schema)).collect::<Vec<_>>().join(", ")
        ),
        Ex::Fold(fold) => format!(
            "{}({}{})",
            fold.func.written(),
            if fold.distinct { "DISTINCT " } else { "" },
            show(&fold.arg, schema)
        ),
    }
}

/// Collections by name, as a label alternation writes them.
fn collections(db: &Database, ids: &[CollectionId]) -> SqlResult2<String> {
    if ids.is_empty() {
        return Ok("any collection".to_owned());
    }
    let names = ids
        .iter()
        .map(|id| db.collection_info(*id).map(|info| info.name))
        .collect::<Result<Vec<_>, _>>()
        .map_err(SqlError::from)?;
    Ok(names.join("|"))
}
