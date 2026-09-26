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
//!
//! **`UNION`.** A part of the body may be a union of stages, planned as
//! ONE stage of the plan by `union.rs`: each branch binds and plans as a
//! stage does here, and the union's rows are what the next part reads.
//!
//! **The outer `SELECT`** over the relation (design §5.5) is ONE MORE
//! STAGE, planned after the body's last: its scope is the relation's
//! columns, its `WHERE` a `Filter` over them -- after the search, never
//! pushed into it (brief §8.3) -- and the rest a `RETURN` with PostgreSQL's
//! meaning: `*` is every column, a whole-number `GROUP BY` or `ORDER BY` key
//! is the select-list item at that position, and an aggregate without
//! `GROUP BY` folds the whole relation into one group rather than grouping
//! by the other items. The body's last stage then names each column once
//! and returns values only, since a relation's columns are what SQL reads.

use super::lineage;
use super::ast::{
    AggFunc, BodyPart, Count, Expr, Literal, Outer, Pipeline, Return, ReturnItem, Stage,
    Statement,
};
use super::bind::{bind_match, column_name, outer_column_name};
use super::convert;
use super::expr::{is_group, Ex, Lowering};
use super::horizontal::Horizontal;
use super::plan::{show, FilterAt, Op, Planner};
use super::schema::{BindingSchema, Name, Provenance, SlotInfo};
use super::subquery::{holds_exists, statement_holds_exists};
use super::types::{aggregate_type, described, spelling};
use crate::sqlstate::{DATATYPE_MISMATCH, INVALID_COLUMN_REFERENCE, UNDEFINED_COLUMN};
use crate::{SqlError, SqlResult2};
use sekejap_core::collections::gql::{
    AggSpec, CountExpr, ExprId, OpSpec, SlotId, SortKey, ValueType,
};

/// One stage as `EXPLAIN` shows it: its slots, where its operators start in
/// the plan's list, the names of the columns its `RETURN` projects (`None`
/// for a hidden sort column), and whether it is the outer `SELECT`.
#[derive(Clone, Debug)]
pub(crate) struct StageView {
    pub(crate) schema: BindingSchema,
    pub(crate) first_op: usize,
    pub(crate) columns: Vec<Option<Name>>,
    pub(crate) outer: bool,
}

/// A stage of the plan: one of the body, a union of stages (`union.rs`),
/// or the outer `SELECT`.
enum Part<'p> {
    Stage(&'p Stage),
    Union { branches: &'p [Stage], all: bool },
    Outer(&'p Outer),
}

/// What reads a stage's `RETURN`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Reader {
    /// The next stage of the body, through `NEXT`.
    Next,
    /// The outer `SELECT`, as the relation's columns.
    Outer,
    /// The caller: the statement's answer.
    Caller,
    /// The statements after a `CALL`, whose body's `RETURN` adds columns to
    /// the row (M5-D).
    Call,
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
    /// `monotone`: the condition under which an index-ordered seed feeds
    /// the first key (`lineage.rs`), so a limited sort stops early, and how
    /// `EXPLAIN` words it (`None` when it always holds).
    Sort { keys: Box<[SortKey]>, shown: Vec<String>, monotone: Option<(ExprId, Option<String>)> },
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
            Self::Sort { keys, monotone, .. } => OpSpec::Sort {
                input,
                keys: keys.clone(),
                monotone_first: monotone.as_ref().map(|(condition, _)| *condition),
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
            Self::Sort { shown, monotone, .. } => format!(
                "Sort by {} -- stable; holds sort_bytes (only offset + limit rows under a LIMIT){}",
                shown.join(", "),
                match monotone {
                    Some((_, None)) => {
                        "; fed in the seed's index order, it stops at the first row past the worst it keeps".to_owned()
                    }
                    Some((_, Some(when))) => format!(
                        "; when {when}, fed in the seed's index order, it stops at the first row past the worst it keeps"
                    ),
                    None => String::new(),
                }
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
    /// Bind and plan every stage of `pipeline`, in order, then the outer
    /// `SELECT` over it, if one is written.
    pub(crate) fn pipeline(
        &mut self,
        pipeline: &Pipeline,
        outer: Option<&Outer>,
        notices: &mut Vec<String>,
    ) -> SqlResult2<Output> {
        let parts: Vec<Part<'_>> = pipeline
            .parts
            .iter()
            .map(|part| match part {
                BodyPart::Stage(stage) => Part::Stage(stage),
                BodyPart::Union { branches, all } => Part::Union {
                    branches,
                    all: *all,
                },
            })
            .chain(outer.map(Part::Outer))
            .collect();
        let mut input = BindingSchema::default();
        // The `Project` whose width is the NEXT stage's, fixed once that
        // stage has bound all its statements.
        let mut boundary: Option<usize> = None;
        let mut output = None;
        for (at, part) in parts.iter().enumerate() {
            let number = u16::try_from(at + 1)
                .map_err(|_| SqlError::unsupported("a GQL body of more than 65,535 stages"))?;
            let reader = match parts.get(at + 1) {
                None => Reader::Caller,
                Some(Part::Outer(_)) => Reader::Outer,
                Some(Part::Stage(_) | Part::Union { .. }) => Reader::Next,
            };
            let first_op = self.ops.len();
            if let Part::Union { branches, all } = part {
                let (next, columns, out) =
                    self.union(branches, *all, number, reader, &input, boundary.take(), notices)?;
                self.stages.push(StageView {
                    schema: next.clone(),
                    first_op,
                    columns,
                    outer: false,
                });
                boundary = Some(first_op);
                input = next;
                output = Some(out);
                continue;
            }
            self.schema = input;
            self.bound = (0..self.schema.width()).map(|i| SlotId(i as u16)).collect();
            let select;
            let mut having: Option<&Expr> = None;
            let ret = match part {
                Part::Stage(stage) => {
                    let mut patterns = 0u16;
                    self.statements(&stage.statements, number, &mut patterns, notices)?;
                    self.return_exists(&stage.ret, number, &mut patterns, notices)?;
                    &stage.ret
                }
                Part::Outer(outer) => {
                    if let Some(predicate) = &outer.where_ {
                        let ex = self.lower(predicate, Horizontal::Refused, None, Role::Predicate)?;
                        self.filter_moving(ex, FilterAt::Outer, "the outer WHERE")?;
                    }
                    having = outer.having.as_ref();
                    select = self.outer_select(&outer.select, having)?;
                    &select
                }
                Part::Union { .. } => unreachable!("a union is planned above"),
            };
            if let Some(project) = boundary.take() {
                self.set_width(project, self.schema.row_width());
            }
            let is_outer = matches!(part, Part::Outer(_));
            let (next, project, columns, out) = self.ret(ret, number, reader, is_outer, having)?;
            self.stages.push(StageView {
                schema: self.schema.clone(),
                first_op,
                columns,
                outer: matches!(part, Part::Outer(_)),
            });
            boundary = Some(project);
            input = next;
            output = Some(out);
        }
        let project = boundary.expect("a body has a stage");
        self.set_width(project, input.row_width());
        Ok(output.expect("a body has a stage"))
    }

    /// Fix the row width of the `Project` at `project` -- or of every
    /// branch of the union there -- to `width`, the reading stage's.
    pub(super) fn set_width(&mut self, project: usize, width: u16) {
        match &mut self.ops[project] {
            Op::Project { width: at, .. } => *at = width,
            Op::Union(union) => union.set_width(width),
            _ => {}
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
            db: self.db,
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

    /// Plan `statements` in order, into stage `number`, whose patterns
    /// `patterns` numbers; an `EXISTS` body's statements are planned here
    /// too (`subquery.rs`), into the same stage.
    pub(super) fn statements(
        &mut self,
        statements: &[Statement],
        number: u16,
        patterns: &mut u16,
        notices: &mut Vec<String>,
    ) -> SqlResult2<()> {
        for statement in statements {
            let (width, first_op) = (self.schema.width(), self.ops.len());
            // Its `EXISTS` tests: those it holds as a top-level conjunct come
            // out of it, to run after it; the rest read a slot. A statement
            // that holds none is planned as written, uncopied.
            let rewritten;
            let (statement, exists) = if statement_holds_exists(statement) {
                let (rewrite, exists) = self.exists_before(statement, number, patterns, notices)?;
                rewritten = rewrite;
                (rewritten.as_ref(), Some(exists))
            } else {
                (Some(statement), None)
            };
            match statement {
                Some(Statement::Match {
                    patterns: written,
                    where_,
                    ..
                }) => {
                    let matched = bind_match(
                        self.db,
                        &mut self.schema,
                        written,
                        where_,
                        patterns,
                        &mut self.params,
                        notices,
                    )?;
                    self.matched(matched)?;
                }
                Some(Statement::Let(assignments)) => self.let_(assignments, number)?,
                Some(Statement::Filter(predicate)) => {
                    let ex = self.lower(predicate, Horizontal::Allowed, None, Role::Predicate)?;
                    self.filter_moving(ex, FilterAt::Statement, "FILTER")?;
                }
                Some(Statement::For { var, list }) => self.for_(var, list, number)?,
                Some(Statement::Call { imports, body }) => self.call(imports, body, number, patterns, notices)?,
                None => {}
            }
            if let Some(exists) = exists {
                self.exists_after(exists, first_op, number, patterns, notices)?;
            }
            if let Some(Statement::Match { optional: true, .. }) = statement {
                self.optional(width, first_op);
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
            let lineage = lineage::of(&self.schema, &ex);
            let id = self.expr(ex);
            let slot = self.schema.add(SlotInfo {
                name: Some(name.clone()),
                ty,
                provenance: Provenance::Let { stage: number },
                nullable,
            })?;
            if let Some(lineage) = lineage {
                self.schema.set_lineage(slot, lineage);
            }
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
                self.note_list(*at, ValueType::Unknown, format!("FOR {var} IN {list}"))?;
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

    /// The outer `SELECT` as its stage's `RETURN`, with PostgreSQL's
    /// meaning: `*` is every column of the relation; a whole-number key of
    /// `GROUP BY` or `ORDER BY` is the select-list item at that position
    /// (`42P10` past the list); and an aggregate without `GROUP BY` --
    /// anywhere in the items, the `ORDER BY` or `having` -- folds the whole
    /// relation into one group, so a column beside it must be grouped
    /// (`42803`), never grouped by implicitly. A `having` with no aggregate
    /// of its own still forces that one group: PostgreSQL's `HAVING` alone
    /// makes a query grouped.
    fn outer_select(&self, select: &Return, having: Option<&Expr>) -> SqlResult2<Return> {
        let mut select = select.clone();
        if select.star {
            select.star = false;
            select.items = self
                .schema
                .names()
                .map(|name| ReturnItem {
                    expr: Expr::Var(name.clone()),
                    alias: None,
                })
                .collect();
        }
        let items = select.items.clone();
        let positional = |expr: &mut Expr, clause: &str| -> SqlResult2<()> {
            let Expr::Literal(Literal::Num(n, true)) = expr else {
                return Ok(());
            };
            let item = (*n >= 1.0)
                .then(|| items.get(*n as usize - 1))
                .flatten()
                .ok_or_else(|| {
                    SqlError::coded(
                        INVALID_COLUMN_REFERENCE,
                        format!("{clause} position {n} is not in select list"),
                    )
                })?;
            *expr = item.expr.clone();
            Ok(())
        };
        for key in select.group_by.iter_mut().flatten() {
            positional(key, "GROUP BY")?;
        }
        for key in &mut select.order_by {
            positional(&mut key.expr, "ORDER BY")?;
        }
        let aggregates = select.items.iter().map(|item| &item.expr).chain(select.order_by.iter().map(|key| &key.expr));
        let forces_one_group = having.is_some() || aggregates.into_iter().any(Expr::has_aggregate);
        if select.group_by.is_none() && forces_one_group {
            select.group_by = Some(Vec::new());
        }
        Ok(select)
    }

    /// Plan a `RETURN` that `reader` reads. Answers the next stage's input
    /// schema, the index of the `Project` (whose width the next stage
    /// fixes), the projected columns' names, and the columns of the answer
    /// if the caller reads it. `having` is the outer `SELECT`'s `HAVING`
    /// (`None` for a body stage, which has none); `outer` is whether THIS
    /// `RETURN` is the outer `SELECT`'s own (brief M3-D2 gap 3 -- its
    /// naming rule is PostgreSQL's own, unlike a body stage's, and it is a
    /// property of which `RETURN` this is, not of who reads it: the body's
    /// LAST stage is read BY the outer SELECT, `reader == Reader::Outer`,
    /// but keeps its own body naming).
    pub(super) fn ret(
        &mut self,
        ret: &Return,
        number: u16,
        reader: Reader,
        outer: bool,
        having: Option<&Expr>,
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
        let next = match reader {
            Reader::Next => "the next stage",
            Reader::Call => "the statements after the CALL",
            Reader::Outer => "the outer SELECT",
            Reader::Caller => "",
        };
        for item in written {
            let name = item.alias.clone().unwrap_or_else(|| {
                if outer {
                    // PostgreSQL's own rule for a plain SELECT list, brief
                    // M3-D2 gap 3 -- the body's own RETURN keeps
                    // `column_name`'s rule, unchanged.
                    outer_column_name(&item.expr)
                } else {
                    column_name(&item.expr)
                }
            });
            if reader != Reader::Caller {
                if item.alias.is_none() && !matches!(item.expr, Expr::Var(_) | Expr::Property { .. }) {
                    return Err(SqlError::unsupported(format!(
                        "stage {number} returns `{}` without a name: a column {next} reads is named, `{} AS name`",
                        item.expr, item.expr
                    )));
                }
                if items.iter().any(|seen| seen.name.as_ref() == Some(&name)) {
                    return Err(SqlError::unsupported(format!(
                        "stage {number} returns two columns named `{name}`: {next} names each column once; alias one (`AS other_name`)"
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
            // An `EXISTS` reads the stage's row: it is a hidden column.
            let over_output = !key.expr.has_aggregate()
                && !holds_exists(&key.expr)
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
            let (post, computed, display) = self.aggregate(ret, &items[..visible], &items, having, number)?;
            let stage_schema = std::mem::replace(&mut self.schema, post);
            // `HAVING`: a `Filter` right after the `Aggregate`, over the
            // finished group -- reading only its keys and its aggregates,
            // via the same `computed` map the items below read (brief
            // M3-D2 gap 2). Anything else is PostgreSQL's `42803`
            // (`Lowering::resolve`, `expr.rs`).
            if let Some(having) = having {
                let ex = self.lower(having, Horizontal::Refused, Some(computed.as_slice()), Role::Predicate)?;
                self.filter(vec![ex], FilterAt::Having);
            }
            (Some(stage_schema), Some(computed), Some(display))
        } else {
            (None, None, None)
        };
        let mut lowered = Vec::with_capacity(items.len());
        for (at, item) in items.iter().enumerate() {
            let ex = match &item.name {
                Some(name) if !matches!(reader, Reader::Next | Reader::Call) && at < visible => {
                    let ex = self.lower(item.expr, Horizontal::Refused, computed.as_deref(), Role::Column(name))?;
                    self.final_column(&ex, name)?;
                    ex
                }
                _ => self.lower(item.expr, Horizontal::Refused, computed.as_deref(), Role::Value)?,
            };
            lowered.push(ex);
        }
        // A limited sort whose first key is a distance an index orders (use
        // 2 of lineage) reads its seed in that order and stops early.
        let first_key = match (order.first(), ret.order_by.first()) {
            (Some(Some(hidden)), _) => lowered.get(*hidden),
            (Some(None), Some(key)) => match &key.expr {
                Expr::Var(name) => items[..visible]
                    .iter()
                    .position(|item| item.name.as_ref() == Some(name))
                    .and_then(|at| lowered.get(at)),
                _ => None,
            },
            _ => None,
        };
        let monotone = match first_key {
            Some(key) if ret.limit.is_some() && !grouped && !ret.distinct => {
                let key = key.clone();
                self.order_seed(&key, ret.order_by[0].descending)?
            }
            _ => None,
        };
        let mut next = BindingSchema::default();
        // A column that is exactly a node or a property with lineage keeps
        // it across the projection; a grouped row has none.
        let lineages: Vec<_> = if grouped {
            Vec::new()
        } else {
            lowered.iter().map(|ex| lineage::of(&self.schema, ex)).collect()
        };
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
        for (at, lineage) in lineages.into_iter().enumerate() {
            if let Some(lineage) = lineage {
                next.set_lineage(SlotId(at as u16), lineage);
            }
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
            self.sort(ret, &order, &next, monotone)?;
        }
        if ret.offset.is_some() || ret.limit.is_some() {
            let offset = ret.offset.map(|c| self.count(c, "OFFSET")).transpose()?;
            let limit = ret.limit.map(|c| self.count(c, "LIMIT")).transpose()?;
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
    /// each slot named by what it holds, for `EXPLAIN`. `having` (the outer
    /// `SELECT`'s only, `None` for a body stage) is scanned for its own
    /// aggregates too, so a `HAVING` that names one the select list did not
    /// return still gets a slot of the grouped row.
    fn aggregate(
        &mut self,
        ret: &Return,
        returned: &[Item<'_>],
        items: &[Item<'_>],
        having: Option<&Expr>,
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
        if let Some(having) = having {
            collect_aggregates(having, &mut aggregates);
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
        monotone: Option<Ex>,
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
            let monotone = monotone.map(|condition| {
                let when = match &condition {
                    Ex::Const(_) => None,
                    other => Some(show(other, &self.schema)),
                };
                (self.expr(condition), when)
            });
            self.ops.push(Op::Table(TableOp::Sort {
                keys: keys.into(),
                shown,
                monotone,
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
    fn count(&mut self, count: Count, clause: &str) -> SqlResult2<CountExpr> {
        Ok(match count {
            Count::Lit(n) => CountExpr::Lit(n),
            Count::Param(n) => {
                let at = n.checked_sub(1).ok_or_else(|| {
                    SqlError::Parameter("parameters are numbered from $1".into())
                })?;
                self.params = self.params.max(n);
                self.note_count(at, clause)?;
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
