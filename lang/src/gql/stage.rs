//! The stage grammar, bound and planned: `LET`, `FILTER`, `FOR`, the
//! `RETURN` with its grouping, `DISTINCT`, `ORDER BY`, `OFFSET` and
//! `LIMIT`, and `NEXT` (`docs/lang/GQL_PROFILE_DESIGN.md` §2.3, §3.3, §7;
//! owner answers Q9, Q12, Q13).
//!
//! **Statements, in order.** A stage's statements are bound and planned one
//! after another, each against the scope the statements before it built: a
//! `MATCH` extends it with its elements (`bind.rs`), a `LET` with its
//! assignments, a `FOR` with its element variable. A `LET`'s expressions
//! all read the scope BEFORE the `LET`, so `LET a = .., b = a` is refused
//! and `LET a = .. LET b = a` is not (rule 3).
//!
//! **The `RETURN`.** It is one of two shapes:
//!
//! ```text
//! plain:    Project -> [Distinct] -> [Sort] -> [Page]
//! grouped:  Aggregate -> Project -> [Distinct] -> [Sort] -> [Page]
//! ```
//!
//! A `RETURN` groups when it writes `GROUP BY` or an aggregate stands in an
//! item or in its `ORDER BY`. The keys are the `GROUP BY` expressions, or
//! else the items that hold no aggregate. The `Aggregate` computes the keys
//! and every aggregate once per group; the items are then bound against
//! the grouped row, where an expression equal to a key or an aggregate IS
//! that slot, a key that is a variable keeps its name (so `person._key`
//! reads a grouped `person`), and anything else is an error naming the
//! variable.
//!
//! An `ORDER BY` key that names only output columns sorts the projected
//! row. Any other key is projected as a HIDDEN column after the returned
//! ones -- unless `DISTINCT`, which would compare it (PostgreSQL's rule) --
//! and a node, an edge, a path or a list is refused as a key (Q12).
//!
//! **`NEXT`.** The `Project` is the boundary: the next stage's rows are the
//! projected rows, so its scope is exactly the returned columns, typed as
//! returned (a node stays a node, and seeds a later `MATCH` as the node
//! already bound). A variable the `RETURN` did not carry is out of scope,
//! and naming it is an error that names the stage that dropped it
//! (rule 4). A later stage's aggregate folds the whole incoming table.

use super::ast::{AggFunc, Count, Expr, Pipeline, Return, ReturnItem, Stage, Statement};
use super::bind::{bind_match, column_name};
use super::convert;
use super::expr::{is_group, Ex, Lowering};
use super::horizontal::Horizontal;
use super::plan::{FilterAt, Op, Planner};
use super::schema::{BindingSchema, Name, Provenance, SlotInfo};
use super::types::{aggregate_type, described, spelling};
use crate::sqlstate::{DATATYPE_MISMATCH, UNDEFINED_COLUMN};
use crate::{SqlError, SqlResult2};
use sekejap_core::collections::gql::{
    AggSpec, CountExpr, ExprId, OpSpec, SlotId, SortKey, ValueType,
};

/// One stage as `EXPLAIN` shows it: its slots, where its operators start in
/// the plan's list, and the names of the columns its `RETURN` projects
/// (`None` for a hidden sort column).
#[derive(Clone, Debug)]
pub(crate) struct StageView {
    pub(crate) schema: BindingSchema,
    pub(crate) first_op: usize,
    pub(crate) columns: Vec<Option<Name>>,
}

/// An operator of the working-table grammar, as the planner holds it; each
/// is one engine operator ([`TableOp::spec`]).
#[derive(Clone, Debug)]
pub(crate) enum TableOp {
    /// `LET`: each slot assigned from the row before the statement.
    Let { assign: Box<[(SlotId, ExprId)]> },
    /// `FOR out IN list`.
    Unnest { list: ExprId, out: SlotId },
    /// A grouped `RETURN`'s fold. `shown` is how `EXPLAIN` names the keys
    /// and each aggregate.
    Aggregate {
        keys: Box<[ExprId]>,
        aggs: Box<[AggSpec]>,
        width: u16,
        shown: Shown,
    },
    Distinct,
    Sort { keys: Box<[SortKey]>, shown: Vec<String> },
    Page {
        offset: Option<CountExpr>,
        limit: Option<CountExpr>,
    },
}

/// What `EXPLAIN` prints for an `Aggregate`: its keys, and each aggregate
/// as `name := COUNT(x)`.
#[derive(Clone, Debug)]
pub(crate) struct Shown {
    keys: Vec<String>,
    aggs: Vec<String>,
}

impl TableOp {
    /// The engine operator, over `input`.
    pub(crate) fn spec(&self, input: Box<OpSpec>) -> OpSpec {
        match self {
            Self::Let { assign } => OpSpec::Let {
                input,
                assign: assign.clone(),
            },
            Self::Unnest { list, out } => OpSpec::Unnest {
                input,
                list: *list,
                out: *out,
            },
            Self::Aggregate {
                keys, aggs, width, ..
            } => OpSpec::Aggregate {
                input,
                keys: keys.clone(),
                aggs: aggs.clone(),
                width: *width,
            },
            Self::Distinct => OpSpec::Distinct { input },
            Self::Sort { keys, .. } => OpSpec::Sort {
                input,
                keys: keys.clone(),
            },
            Self::Page { offset, limit } => OpSpec::Page {
                input,
                offset: *offset,
                limit: *limit,
            },
        }
    }

    /// The operator's `EXPLAIN` line, without its number. `text` prints an
    /// expression and `slot` a slot of the stage.
    pub(crate) fn describe(&self, text: &dyn Fn(ExprId) -> String, slot: &dyn Fn(SlotId) -> String) -> String {
        match self {
            Self::Let { assign } => format!(
                "Let {} -- charges primary_reads, graph_edges",
                assign
                    .iter()
                    .map(|(out, ex)| format!("{} := {}", slot(*out), text(*ex)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Unnest { list, out } => format!(
                "Unnest {} := each element of {}, in list order -- charges binding_rows, holds list_bytes",
                slot(*out),
                text(*list)
            ),
            Self::Aggregate { shown, .. } => {
                let by = if shown.keys.is_empty() {
                    "over the whole input".to_owned()
                } else {
                    format!("by {}", shown.keys.join(", "))
                };
                let aggs = if shown.aggs.is_empty() {
                    String::new()
                } else {
                    format!(": {}", shown.aggs.join(", "))
                };
                format!(
                    "Aggregate {by}{aggs} -- folds the whole input first; holds groups, sort_bytes, list_bytes; charges binding_rows"
                )
            }
            Self::Distinct => "Distinct -- holds sort_bytes".to_owned(),
            Self::Sort { shown, .. } => format!(
                "Sort by {} -- stable; holds sort_bytes (only offset + limit rows under a LIMIT)",
                shown.join(", ")
            ),
            Self::Page { offset, limit } => {
                let count = |c: &CountExpr| match c {
                    CountExpr::Lit(n) => n.to_string(),
                    CountExpr::Param(at) => format!("${}", at + 1),
                };
                let mut out = "Page".to_owned();
                if let Some(offset) = offset {
                    out.push_str(&format!(" OFFSET {}", count(offset)));
                }
                if let Some(limit) = limit {
                    out.push_str(&format!(" LIMIT {}", count(limit)));
                }
                out.push_str(" -- a skipped row is charged where it was produced; a reached LIMIT stops the input");
                out
            }
        }
    }
}

/// The last stage's columns: their names and types.
pub(crate) struct Output {
    pub(crate) columns: Vec<Name>,
    pub(crate) types: Vec<ValueType>,
}

/// How a lowered expression is used ([`Planner::lower`]).
enum Role<'n> {
    /// A condition.
    Predicate,
    /// Any value, an element included.
    Value,
    /// An output column of the final stage, named.
    Column(&'n Name),
}

/// One `RETURN` item to project: returned (named) or hidden (a sort key).
struct Item<'r> {
    expr: &'r Expr,
    name: Option<Name>,
}

impl Planner<'_> {
    /// Bind and plan every stage of `pipeline`, in order.
    pub(crate) fn pipeline(
        &mut self,
        pipeline: &Pipeline,
        notices: &mut Vec<String>,
    ) -> SqlResult2<Output> {
        let last = pipeline.stages.len();
        let mut input = BindingSchema::default();
        // The `Project` whose width is the NEXT stage's, fixed once that
        // stage has bound all its statements.
        let mut boundary: Option<usize> = None;
        let mut output = None;
        for (at, stage) in pipeline.stages.iter().enumerate() {
            let number = u16::try_from(at + 1)
                .map_err(|_| SqlError::unsupported("a GQL body of more than 65,535 stages"))?;
            self.schema = input;
            self.bound = (0..self.schema.width()).map(|i| SlotId(i as u16)).collect();
            let first_op = self.ops.len();
            self.statements(stage, number, notices)?;
            if let Some(project) = boundary.take() {
                self.set_width(project, self.schema.row_width());
            }
            let (next, project, columns, out) = self.ret(&stage.ret, number, at + 1 == last)?;
            self.stages.push(StageView {
                schema: self.schema.clone(),
                first_op,
                columns,
            });
            boundary = Some(project);
            input = next;
            output = Some(out);
        }
        let project = boundary.expect("a body has a stage");
        self.set_width(project, input.row_width());
        Ok(output.expect("a body has a stage"))
    }

    fn set_width(&mut self, project: usize, width: u16) {
        if let Op::Project { width: at, .. } = &mut self.ops[project] {
            *at = width;
        }
    }

    /// Lower `expr` against the stage's schema as `role`, where an
    /// aggregate is `how` (`horizontal.rs`) and, in a grouped `RETURN`,
    /// `grouped` holds what the grouping computed; then count its `$n` and
    /// check the kinds it holds ([`Planner::note_params`]).
    fn lower(
        &mut self,
        expr: &Expr,
        how: Horizontal,
        grouped: Option<&[(Expr, SlotId)]>,
        role: Role<'_>,
    ) -> SqlResult2<Ex> {
        let mut lowering = Lowering {
            schema: &self.schema,
            params: &mut self.params,
            admitted: Vec::new(),
            grouped,
            horizontal: how,
        };
        let ex = match role {
            Role::Predicate => lowering.predicate(expr),
            Role::Value => lowering.value(expr),
            Role::Column(name) => lowering.column(expr, name),
        }?;
        self.note_params(&ex)?;
        Ok(ex)
    }

    fn statements(&mut self, stage: &Stage, number: u16, notices: &mut Vec<String>) -> SqlResult2<()> {
        let mut patterns = 0u16;
        for statement in &stage.statements {
            match statement {
                Statement::Match {
                    patterns: written,
                    where_,
                    optional,
                } => {
                    let (width, first_op) = (self.schema.width(), self.ops.len());
                    let matched = bind_match(
                        self.db,
                        &mut self.schema,
                        written,
                        where_,
                        &mut patterns,
                        &mut self.params,
                        notices,
                    )?;
                    self.matched(matched)?;
                    if *optional {
                        self.optional(width, first_op);
                    }
                }
                Statement::Let(assignments) => self.let_(assignments, number)?,
                Statement::Filter(predicate) => {
                    let ex = self.lower(predicate, Horizontal::Allowed, None, Role::Predicate)?;
                    self.filter(vec![ex], FilterAt::Statement);
                }
                Statement::For { var, list } => self.for_(var, list, number)?,
            }
        }
        Ok(())
    }

    /// `OPTIONAL MATCH`: the operators its pattern and `WHERE` planned, from
    /// `first_op` on, become the inner side of ONE `OptionalApply`, and the
    /// slots it allocated, from `width` on, are nullable -- `Null` in the
    /// row it gives when nothing matched.
    fn optional(&mut self, width: usize, first_op: usize) {
        let inner: Vec<Op> = self.ops.drain(first_op..).collect();
        let introduced = self.schema.nullable_from(width);
        self.ops.push(Op::Optional {
            inner,
            introduced: introduced.into(),
        });
    }

    /// True when a slot holding `ex`, of type `ty`, may be `Null`. An
    /// element (or a list of elements) is, when `ex` reads a nullable slot:
    /// how an `OPTIONAL MATCH` variable stays nullable through a `LET`, a
    /// grouping key and `NEXT`. A value is, unless it copies a value that
    /// is never null or is a `COUNT` (or an `ARRAY_AGG`, which is a list)
    /// folding a list that is never null.
    fn may_be_null(&self, ex: &Ex, ty: &ValueType) -> bool {
        let nullable = |slot: SlotId| self.schema.slot(slot).nullable;
        if convert::element_type(ty).is_some() || is_group(ty) {
            return ex.refs().into_iter().any(nullable);
        }
        match ex {
            Ex::Slot(slot) => nullable(*slot),
            Ex::Fold(fold) if matches!(fold.func, AggFunc::Count | AggFunc::ArrayAgg) => {
                nullable(fold.list)
            }
            _ => true,
        }
    }

    /// `LET a = .., b = ..`: every expression against the scope before the
    /// statement, then the slots.
    fn let_(&mut self, assignments: &[(Name, Expr)], number: u16) -> SqlResult2<()> {
        let mut lowered = Vec::with_capacity(assignments.len());
        for (name, expr) in assignments {
            if let Some(sibling) = assignments
                .iter()
                .map(|(other, _)| other)
                .filter(|other| *other != name)
                .find(|other| self.schema.resolve(other).is_none() && names(expr).contains(other))
            {
                return Err(SqlError::coded(UNDEFINED_COLUMN, format!(
                    "`{name}` reads `{sibling}`, which the same LET assigns: every expression of one LET reads the scope before it; write a second LET (`LET {sibling} = ... LET {name} = ...`)"
                )));
            }
            let ex = self.lower(expr, Horizontal::Allowed, None, Role::Value)?;
            let ty = self.slot_type(&ex)?;
            let nullable = self.may_be_null(&ex, &ty);
            lowered.push((name, ex, ty, nullable));
        }
        let mut assign = Vec::with_capacity(lowered.len());
        for (name, ex, ty, nullable) in lowered {
            let id = self.expr(ex);
            let slot = self.schema.add(SlotInfo {
                name: Some(name.clone()),
                ty,
                provenance: Provenance::Let { stage: number },
                nullable,
            })?;
            self.bind_slot(slot);
            assign.push((slot, id));
        }
        self.ops.push(Op::Table(TableOp::Let {
            assign: assign.into(),
        }));
        Ok(())
    }

    /// `FOR var IN list`: a list value, or a `$n` bound to a JSON array.
    fn for_(&mut self, var: &Name, list: &Expr, number: u16) -> SqlResult2<()> {
        let ex = self.lower(list, Horizontal::Refused, None, Role::Value)?;
        let elem = match &ex {
            Ex::Param(at) => {
                self.lists.push(*at);
                ValueType::Unknown
            }
            _ => match self.slot_type(&ex)? {
                ValueType::List(elem) => *elem,
                ValueType::Unknown => ValueType::Unknown,
                other => {
                    return Err(SqlError::coded(DATATYPE_MISMATCH, format!(
                        "FOR {var} IN {list}: {list} is {}, not a list",
                        spelling(&other)
                    )))
                }
            },
        };
        let id = self.expr(ex);
        // A list of elements holds no null; a list of values may.
        let nullable = convert::element_type(&elem).is_none();
        let out = self.schema.add(SlotInfo {
            name: Some(var.clone()),
            ty: elem,
            provenance: Provenance::Unnest { stage: number },
            nullable,
        })?;
        self.bind_slot(out);
        self.ops.push(Op::Table(TableOp::Unnest { list: id, out }));
        Ok(())
    }

    /// Plan a `RETURN`. Answers the next stage's input schema, the index of
    /// the `Project` (whose width the next stage fixes), the projected
    /// columns' names, and the columns of the answer if this stage is last.
    fn ret(
        &mut self,
        ret: &Return,
        number: u16,
        last: bool,
    ) -> SqlResult2<(BindingSchema, usize, Vec<Option<Name>>, Output)> {
        // `RETURN *`: every variable in scope, in slot order.
        let starred: Vec<ReturnItem> = if ret.star {
            self.schema
                .names()
                .map(|name| ReturnItem {
                    expr: Expr::Var(name.clone()),
                    alias: None,
                })
                .collect()
        } else {
            Vec::new()
        };
        let written = if ret.star { &starred } else { &ret.items };
        let mut items: Vec<Item<'_>> = Vec::new();
        for item in written {
            let name = item.alias.clone().unwrap_or_else(|| column_name(&item.expr));
            if !last {
                if item.alias.is_none() && !matches!(item.expr, Expr::Var(_) | Expr::Property { .. }) {
                    return Err(SqlError::unsupported(format!(
                        "stage {number} returns `{}` without a name: a column the next stage reads is named, `{} AS name`",
                        item.expr, item.expr
                    )));
                }
                if items.iter().any(|seen| seen.name.as_ref() == Some(&name)) {
                    return Err(SqlError::unsupported(format!(
                        "stage {number} returns two columns named `{name}`: the next stage names each column once; alias one (`AS other_name`)"
                    )));
                }
            }
            items.push(Item {
                expr: &item.expr,
                name: Some(name),
            });
        }
        let visible = items.len();
        // Each ORDER BY key: over the output columns, or a hidden column.
        let output: Vec<Name> = items.iter().filter_map(|item| item.name.clone()).collect();
        let mut order = Vec::with_capacity(ret.order_by.len());
        for key in &ret.order_by {
            let over_output = !key.expr.has_aggregate()
                && names(&key.expr).iter().all(|name| output.contains(name));
            if over_output {
                order.push(None);
                continue;
            }
            if ret.distinct {
                return Err(SqlError::unsupported(format!(
                    "ORDER BY {}: under RETURN DISTINCT a sort key is a returned column, since DISTINCT compares whole returned rows (as SQL's SELECT DISTINCT)",
                    key.expr
                )));
            }
            order.push(Some(items.len()));
            items.push(Item {
                expr: &key.expr,
                name: None,
            });
        }
        let grouped = ret.group_by.is_some() || items.iter().any(|item| item.expr.has_aggregate());
        // The projection, from the stage's row or from the grouped row.
        let (stage_schema, computed, display) = if grouped {
            let (post, computed, display) = self.aggregate(ret, &items[..visible], &items, number)?;
            (Some(std::mem::replace(&mut self.schema, post)), Some(computed), Some(display))
        } else {
            (None, None, None)
        };
        let mut lowered = Vec::with_capacity(items.len());
        for (at, item) in items.iter().enumerate() {
            let ex = match &item.name {
                Some(name) if last && at < visible => {
                    let ex = self.lower(item.expr, Horizontal::Refused, computed.as_deref(), Role::Column(name))?;
                    self.final_column(&ex, name)?;
                    ex
                }
                _ => self.lower(item.expr, Horizontal::Refused, computed.as_deref(), Role::Value)?,
            };
            lowered.push(ex);
        }
        let mut next = BindingSchema::default();
        let mut types = Vec::with_capacity(visible);
        for (item, ex) in items.iter().zip(&lowered) {
            let ty = self.slot_type(ex)?;
            if types.len() < visible {
                types.push(ty.clone());
            }
            // A name the last stage returns twice is kept once for its
            // ORDER BY: the first column answers to it.
            let name = item.name.clone().filter(|name| next.resolve(name).is_none());
            let nullable = self.may_be_null(ex, &ty);
            next.add(SlotInfo {
                name,
                ty,
                provenance: Provenance::Returned { stage: number },
                nullable,
            })?;
        }
        // `EXPLAIN` prints a grouped row's slot as the key or aggregate it
        // holds.
        if let Some(display) = display {
            self.schema = display;
        }
        let cols: Box<[ExprId]> = lowered.into_iter().map(|ex| self.expr(ex)).collect();
        if let Some(stage_schema) = stage_schema {
            self.schema = stage_schema;
        }
        let project = self.ops.len();
        self.ops.push(Op::Project { cols, width: 0 });
        if ret.distinct {
            self.ops.push(Op::Table(TableOp::Distinct));
        }
        if !ret.order_by.is_empty() {
            self.sort(ret, &order, &next)?;
        }
        if ret.offset.is_some() || ret.limit.is_some() {
            let offset = ret.offset.map(|c| self.count(c)).transpose()?;
            let limit = ret.limit.map(|c| self.count(c)).transpose()?;
            self.ops.push(Op::Table(TableOp::Page { offset, limit }));
        }
        // What the next stage cannot see any more.
        for name in self.schema.names() {
            next.drop_name(name.clone(), number);
        }
        for (name, stage) in self.schema.dropped() {
            next.drop_name(name.clone(), *stage);
        }
        let columns = items.iter().map(|item| item.name.clone()).collect();
        let out = Output {
            columns: items[..visible]
                .iter()
                .map(|item| item.name.clone().expect("a returned column is named"))
                .collect(),
            types,
        };
        Ok((next, project, columns, out))
    }

    /// A grouped `RETURN`: the `Aggregate` operator, the schema of the row
    /// it produces, the keys and aggregates as written, each with its slot
    /// of that row, for the items to be bound against, and that schema with
    /// each slot named by what it holds, for `EXPLAIN`.
    fn aggregate(
        &mut self,
        ret: &Return,
        returned: &[Item<'_>],
        items: &[Item<'_>],
        number: u16,
    ) -> SqlResult2<(BindingSchema, Vec<(Expr, SlotId)>, BindingSchema)> {
        let mut keys: Vec<&Expr> = Vec::new();
        let written: Vec<&Expr> = match &ret.group_by {
            Some(keys) => keys.iter().collect(),
            None => returned
                .iter()
                .map(|item| item.expr)
                .filter(|expr| !expr.has_aggregate())
                .collect(),
        };
        for key in written {
            if key.has_aggregate() {
                return Err(SqlError::unsupported(format!(
                    "GROUP BY {key}: a grouping key holds no aggregate"
                )));
            }
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        let mut aggregates: Vec<&Expr> = Vec::new();
        for item in items {
            collect_aggregates(item.expr, &mut aggregates);
        }
        let mut post = BindingSchema::default();
        let mut display = BindingSchema::default();
        let mut computed = Vec::with_capacity(keys.len() + aggregates.len());
        let mut key_ids = Vec::with_capacity(keys.len());
        let mut shown = Shown {
            keys: Vec::new(),
            aggs: Vec::new(),
        };
        for key in &keys {
            let ex = self.lower(key, Horizontal::Refused, None, Role::Value)?;
            let ty = self.slot_type(&ex)?;
            let nullable = self.may_be_null(&ex, &ty);
            let id = self.expr(ex);
            shown.keys.push(self.texts[id.0 as usize].clone());
            key_ids.push(id);
            let name = match key {
                Expr::Var(name) => Some(name.clone()),
                _ => None,
            };
            let shown_as = self.texts[id.0 as usize].clone();
            display.add(named(&display, &shown_as, ty.clone(), number))?;
            let slot = post.add(SlotInfo {
                name,
                ty,
                provenance: Provenance::Aggregate { stage: number },
                nullable,
            })?;
            computed.push(((*key).clone(), slot));
        }
        let mut aggs = Vec::with_capacity(aggregates.len());
        for aggregate in aggregates {
            let Expr::Aggregate { func, distinct, arg } = aggregate else {
                unreachable!("collect_aggregates collects aggregates")
            };
            let (spec, ty, text) = match arg {
                None => (AggSpec::CountRows, ValueType::Int, "COUNT(*)".to_owned()),
                Some(arg) => {
                    let ex = self.lower(arg, Horizontal::Refused, None, Role::Value)?;
                    let arg_ty = self.slot_type(&ex)?;
                    if matches!(func, AggFunc::Sum | AggFunc::Avg)
                        && matches!(arg_ty, ValueType::List(_))
                    {
                        return Err(SqlError::coded(DATATYPE_MISMATCH, format!(
                            "{}({arg}): `{arg}` is a list, and an aggregate in a RETURN folds rows, never a list's elements; fold the list per row in a LET first (`LET total = {}({arg})`)",
                            func.written(),
                            func.written()
                        )));
                    }
                    let ty = aggregate_type(*func, &arg_ty, &arg.to_string())?;
                    let id = self.expr(ex);
                    let text = format!(
                        "{}({}{})",
                        func.written(),
                        if *distinct { "DISTINCT " } else { "" },
                        self.texts[id.0 as usize]
                    );
                    let spec = match func {
                        AggFunc::Count => AggSpec::Count {
                            arg: id,
                            distinct: *distinct,
                        },
                        AggFunc::Sum => AggSpec::Sum(id),
                        AggFunc::Avg => AggSpec::Avg(id),
                        AggFunc::Min => AggSpec::Min(id),
                        AggFunc::Max => AggSpec::Max(id),
                        AggFunc::ArrayAgg => AggSpec::ArrayAgg {
                            arg: id,
                            elem: arg_ty,
                        },
                    };
                    (spec, ty, text)
                }
            };
            display.add(named(&display, &text, ty.clone(), number))?;
            // A COUNT is never null; an ARRAY_AGG is only over no row at
            // all, which only an ungrouped RETURN folds (Q9).
            let nullable = match spec {
                AggSpec::CountRows | AggSpec::Count { .. } => false,
                AggSpec::ArrayAgg { .. } => keys.is_empty(),
                _ => true,
            };
            let slot = post.add(SlotInfo {
                name: None,
                ty,
                provenance: Provenance::Aggregate { stage: number },
                nullable,
            })?;
            // The aggregate as `EXPLAIN` names it: by the returned column
            // that is exactly it, else by its slot.
            let label = returned
                .iter()
                .find(|item| item.expr == aggregate)
                .and_then(|item| item.name.as_ref().map(ToString::to_string))
                .unwrap_or_else(|| format!("#{}", slot.0));
            shown.aggs.push(format!("{label} := {text}"));
            computed.push((aggregate.clone(), slot));
            aggs.push(spec);
        }
        let width = post.row_width();
        self.ops.push(Op::Table(TableOp::Aggregate {
            keys: key_ids.into(),
            aggs: aggs.into(),
            width,
            shown,
        }));
        Ok((post, computed, display))
    }

    /// The `Sort` over the projected row: a key over the output columns is
    /// bound against them, any other is its hidden column.
    fn sort(
        &mut self,
        ret: &Return,
        order: &[Option<usize>],
        output: &BindingSchema,
    ) -> SqlResult2<()> {
        let stage_schema = std::mem::replace(&mut self.schema, output.clone());
        let result: SqlResult2<()> = (|| {
            let mut keys = Vec::with_capacity(order.len());
            let mut shown = Vec::with_capacity(order.len());
            for (key, hidden) in ret.order_by.iter().zip(order) {
                let ex = match hidden {
                    Some(column) => Ex::Slot(SlotId(*column as u16)),
                    None => self.lower(&key.expr, Horizontal::Refused, None, Role::Value)?,
                };
                self.sortable(&ex, &key.expr)?;
                let id = self.expr(ex);
                let text = match hidden {
                    Some(_) => key.expr.to_string(),
                    None => self.texts[id.0 as usize].clone(),
                };
                shown.push(if key.descending { format!("{text} DESC") } else { text });
                keys.push(SortKey {
                    expr: id,
                    descending: key.descending,
                });
            }
            self.ops.push(Op::Table(TableOp::Sort {
                keys: keys.into(),
                shown,
            }));
            Ok(())
        })();
        self.schema = stage_schema;
        result
    }

    /// Q12: a sort key is a scalar. A node, an edge or a path orders by its
    /// identity, which means nothing to a reader, and a list has no order.
    fn sortable(&self, ex: &Ex, written: &Expr) -> SqlResult2<()> {
        let ty = &self.slot_type(ex)?;
        if convert::element_type(ty).is_some() {
            return Err(SqlError::unsupported(format!(
                "ORDER BY {written}: `{written}` is {}, which has no meaningful order; order by one of its properties, such as `{written}._key`, or by `ELEMENT_ID({written})`",
                described(ty)
            )));
        }
        if matches!(ty, ValueType::List(_)) {
            return Err(SqlError::unsupported(format!(
                "ORDER BY {written}: `{written}` is a list, which has no order; order by a scalar"
            )));
        }
        Ok(())
    }

    /// A final column holds something SQL can carry: not a list of
    /// elements, and not a list of lists (a PostgreSQL array is
    /// rectangular, design §6.3).
    fn final_column(&self, ex: &Ex, name: &Name) -> SqlResult2<()> {
        let ty = self.slot_type(ex)?;
        // A node a path function gives (`PATH_FIRST(p)`); a variable was
        // refused as it was lowered.
        if let Some(element) = convert::element_type(&ty) {
            return Err(convert::not_a_value(element, name, "is"));
        }
        if let ValueType::List(elem) = ty {
            if let Some(element) = convert::element_type(&elem) {
                return Err(convert::not_a_value(element, name, "holds"));
            }
            if matches!(*elem, ValueType::List(_)) {
                return Err(SqlError::unsupported(format!(
                    "`{name}` is a list of lists, which a PostgreSQL array (rectangular) does not carry; unnest it with FOR in a stage before"
                )));
            }
        }
        Ok(())
    }

    /// An `OFFSET` or `LIMIT` count, its `$n` noted as a count.
    fn count(&mut self, count: Count) -> SqlResult2<CountExpr> {
        Ok(match count {
            Count::Lit(n) => CountExpr::Lit(n),
            Count::Param(n) => {
                let at = n.checked_sub(1).ok_or_else(|| {
                    SqlError::Parameter("parameters are numbered from $1".into())
                })?;
                self.params = self.params.max(n);
                self.note_count(at);
                CountExpr::Param(at)
            }
        })
    }
}

/// A slot of a grouped row as `EXPLAIN` shows it: named by the key or the
/// aggregate it holds (unnamed if that text already names one).
fn named(display: &BindingSchema, text: &str, ty: ValueType, number: u16) -> SlotInfo {
    let name = Name::quoted(text);
    SlotInfo {
        name: display.resolve(&name).is_none().then_some(name),
        ty,
        provenance: Provenance::Aggregate { stage: number },
        nullable: false,
    }
}

/// The variable names an expression reads, as written.
pub(super) fn names(expr: &Expr) -> Vec<Name> {
    let mut out = Vec::new();
    fn walk(expr: &Expr, out: &mut Vec<Name>) {
        match expr {
            Expr::Var(name) | Expr::Property { var: name, .. } => {
                if !out.contains(name) {
                    out.push(name.clone());
                }
            }
            other => {
                for child in other.children() {
                    walk(child, out);
                }
            }
        }
    }
    walk(expr, &mut out);
    out
}

/// Every aggregate `expr` holds, outermost first, each once; an aggregate's
/// argument is not searched (an aggregate inside one is refused when the
/// argument is bound).
fn collect_aggregates<'e>(expr: &'e Expr, out: &mut Vec<&'e Expr>) {
    if let Expr::Aggregate { .. } = expr {
        if !out.contains(&expr) {
            out.push(expr);
        }
        return;
    }
    for child in expr.children() {
        collect_aggregates(child, out);
    }
}
