//! `UNION [ALL | DISTINCT]` over the stages of one part of a body, bound
//! and planned (M5-E; `docs/lang/GQL_PROFILE_DESIGN_M5_M7.md` §2.5, owner
//! answers Q23-Q25).
//!
//! **Branches.** Each branch is one stage, bound and planned exactly as
//! `stage.rs` plans a stage -- its statements, then its `RETURN` with its
//! own grouping, `ORDER BY`, `OFFSET` and `LIMIT` -- from the same input:
//! nothing in a first part, the incoming table after `NEXT`. Its operators
//! are kept apart, one list per branch, and the union is ONE stage of the
//! plan: the next part reads its rows.
//!
//! **Columns** (Q23, PostgreSQL's rules where they are not graph-specific).
//! Every branch returns the same number of columns, with the same NAMES in
//! the same ORDER, which satisfies both GQL's by-name reading and SQL's
//! by-position one. Types unify per column ([`unify`]): `BIGINT` with
//! `DOUBLE PRECISION` is `DOUBLE PRECISION` -- an integer value in it
//! encodes exactly as a float8 and compares equal to the same float, as a
//! `CASE` over both already does -- and a node of one label with a node of
//! another is a node of both, which only a later stage may read. Anything
//! else -- a name or an order that differs, types that do not unify -- is
//! `42804`, naming the column and the branch. A different column COUNT is
//! PostgreSQL's own `42601` ("each UNION query must have the same number
//! of columns").
//!
//! A branch that sorts by a key it does not return carries that key as a
//! hidden column (`stage.rs`); a second `Project` after its page drops it,
//! so the union's rows are the returned columns only and `UNION` never
//! tells two equal rows apart by a sort key.
//!
//! **The engine's operators** (M5-B). In a first part each branch starts
//! from its own `Unit` and the union is [`OpSpec::Union`]. After `NEXT`
//! every branch must see the WHOLE incoming table -- a branch may aggregate
//! it -- so the table is read once and held under `sort_bytes`
//! ([`OpSpec::Buffered`], Q25), and each branch starts from a `Replay` of
//! it. `UNION` and `UNION DISTINCT` are the existing `Distinct` after the
//! union, planned as the stage's next operator; `UNION ALL` has none.
//! Either way the rows come branch by branch, each branch's in its order.

use super::ast::Stage;
use super::expr::Ex;
use super::plan::{chain, Op, Planner};
use super::schema::{BindingSchema, Name, Provenance, SlotInfo};
use super::stage::{Output, Reader, StageView, TableOp};
use super::types::{described, unify};
use crate::sqlstate::{DATATYPE_MISMATCH, SYNTAX_ERROR};
use crate::{SqlError, SqlResult2};
use sekejap_core::collections::gql::{OpSpec, SlotId};
use sekejap_core::collections::{Database, GraphContextId};

/// A planned union: its branches, and how the engine runs them.
#[derive(Clone, Debug)]
pub(super) struct Union {
    pub(super) branches: Vec<Branch>,
    /// `UNION ALL`: no `Distinct` follows.
    all: bool,
    /// After `NEXT`: the incoming table is buffered once for the branches.
    pub(super) buffered: bool,
    /// The union's stage number.
    number: u16,
    /// How many columns every branch returns.
    pub(super) columns: u16,
    /// The width of the rows the union gives, fixed by the part that reads
    /// them ([`Union::set_width`]).
    pub(super) width: u16,
}

/// One branch of a union: its operators, first to last, and its slots.
#[derive(Clone, Debug)]
pub(super) struct Branch {
    pub(super) ops: Vec<Op>,
    /// The branch's slots and projected columns, for `EXPLAIN` and the
    /// node-BFS rules (`reach.rs`); `first_op` is 0, into `ops`.
    pub(super) view: StageView,
    /// The branch's last `Project`, in `ops`: its width is the union's.
    project: usize,
    /// The branch's own row width, its `Unit`'s in a first part.
    width: u16,
}

impl Union {
    /// Every branch gives rows `width` slots wide.
    pub(super) fn set_width(&mut self, width: u16) {
        self.width = width;
        for branch in &mut self.branches {
            if let Op::Project { width: at, .. } = &mut branch.ops[branch.project] {
                *at = width;
            }
        }
    }

    /// The engine operator over `input`, whose rows are `incoming` slots
    /// wide: a [`OpSpec::Buffered`] after `NEXT`; in a first part the
    /// [`OpSpec::Union`] alone, whose branches start from their own `Unit`,
    /// so `input` (the plan's `Unit`) is not read.
    pub(super) fn spec(
        &self,
        db: &Database,
        context: Option<GraphContextId>,
        input: Box<OpSpec>,
        incoming: u16,
    ) -> SqlResult2<OpSpec> {
        let branches = self
            .branches
            .iter()
            .map(|branch| {
                let (leaf, width) = if self.buffered {
                    (OpSpec::Replay { width: incoming }, incoming)
                } else {
                    (OpSpec::Unit { width: branch.width }, branch.width)
                };
                chain(db, context, leaf, width, &branch.ops)
            })
            .collect::<SqlResult2<Box<[OpSpec]>>>()?;
        let union = OpSpec::Union {
            branches,
            width: self.width,
        };
        Ok(if self.buffered {
            OpSpec::Buffered {
                input,
                union: Box::new(union),
            }
        } else {
            union
        })
    }

    /// The operator's `EXPLAIN` line, numbered `n`: its branches' steps are
    /// numbered `n.1`, `n.2`, ... across the branches, in order.
    pub(super) fn describe(&self, n: &str) -> String {
        let conjunction = if self.all { "ALL" } else { "DISTINCT" };
        let count = self.branches.len();
        let then = if self.all { "" } else { ", then Distinct" };
        let mut first = 1;
        let ranges: Vec<String> = self
            .branches
            .iter()
            .enumerate()
            .map(|(at, branch)| {
                let last = first + branch.ops.len() - 1;
                let steps = if last == first {
                    format!("step {n}.{first}")
                } else {
                    format!("steps {n}.{first}-{n}.{last}")
                };
                first = last + 1;
                format!("branch {} is {steps}", at + 1)
            })
            .collect();
        let ranges = ranges.join(", ");
        if self.buffered {
            format!(
                "Buffered Union {conjunction} of {count} branches{then}: the rows stage {} returned, held once and read by each branch from its start; branch order, then row order -- {ranges}; holds sort_bytes",
                self.number - 1
            )
        } else {
            format!(
                "Union {conjunction} of {count} branches{then}: branch order, then row order -- {ranges}; charges nothing of its own"
            )
        }
    }
}

impl Planner<'_> {
    /// Bind and plan `branches`, a union that is stage `number`, read by
    /// `reader`, each branch over `input` (the incoming table, or nothing
    /// in a first part). `incoming` is the `Project` of the part before
    /// (`None` in a first part), whose width the widest branch fixes.
    /// Answers the union's schema -- the next stage's input -- its column
    /// names, and its answer's columns.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn union(
        &mut self,
        branches: &[Stage],
        all: bool,
        number: u16,
        reader: Reader,
        input: &BindingSchema,
        incoming: Option<usize>,
        notices: &mut Vec<String>,
    ) -> SqlResult2<(BindingSchema, Vec<Option<Name>>, Output)> {
        let mut planned = Vec::with_capacity(branches.len());
        let mut returned = Vec::with_capacity(branches.len());
        for stage in branches {
            self.schema = input.clone();
            self.bound = (0..self.schema.width()).map(|i| SlotId(i as u16)).collect();
            let first = self.ops.len();
            let mut patterns = 0u16;
            self.statements(&stage.statements, number, &mut patterns, notices)?;
            self.return_exists(&stage.ret, number, &mut patterns, notices)?;
            let width = self.schema.row_width();
            let (next, project, columns, out) = self.ret(&stage.ret, number, reader, false, None)?;
            planned.push(Branch {
                ops: self.ops.drain(first..).collect(),
                view: StageView {
                    schema: self.schema.clone(),
                    first_op: 0,
                    columns,
                    outer: false,
                },
                project: project - first,
                width,
            });
            returned.push((next, out));
        }
        let (schema, output) = columns(&returned, number)?;
        let visible = output.columns.len();
        // A hidden sort column stops at its branch: a second `Project`
        // after the branch's page keeps the returned columns only.
        for (branch, (next, _)) in planned.iter_mut().zip(&returned) {
            if next.width() == visible {
                continue;
            }
            if let Op::Project { width, .. } = &mut branch.ops[branch.project] {
                *width = next.row_width();
            }
            let stage_schema = std::mem::replace(&mut self.schema, next.clone());
            let cols = (0..visible)
                .map(|at| self.expr(Ex::Slot(SlotId(at as u16))))
                .collect();
            self.schema = stage_schema;
            branch.project = branch.ops.len();
            branch.ops.push(Op::Project { cols, width: 0 });
        }
        if let Some(project) = incoming {
            let widest = planned.iter().map(|branch| branch.width).max().unwrap_or(0);
            self.set_width(project, widest);
        }
        self.ops.push(Op::Union(Union {
            branches: planned,
            all,
            buffered: incoming.is_some(),
            number,
            columns: visible as u16,
            width: 0,
        }));
        if !all {
            self.ops.push(Op::Table(TableOp::Distinct));
        }
        self.schema = schema.clone();
        let names = output.columns.iter().cloned().map(Some).collect();
        Ok((schema, names, output))
    }
}

/// The union's columns from what each branch `returned` (its next-stage
/// schema and its answer's columns): the first branch's names, each type
/// unified over the branches, nullable when any branch's is, and every
/// variable a branch dropped still named as dropped.
fn columns(returned: &[(BindingSchema, Output)], number: u16) -> SqlResult2<(BindingSchema, Output)> {
    let (first, first_out) = &returned[0];
    let mut types = first_out.types.clone();
    let mut nullable: Vec<bool> = (0..types.len())
        .map(|at| first.slot(SlotId(at as u16)).nullable)
        .collect();
    for (at, (next, out)) in returned.iter().enumerate().skip(1) {
        let branch = at + 1;
        if out.columns.len() != first_out.columns.len() {
            return Err(SqlError::coded(SYNTAX_ERROR, format!(
                "each UNION query must have the same number of columns: UNION branch {branch} returns {} columns and branch 1 returns {}: every branch of a UNION returns the same columns, by name and in order",
                out.columns.len(),
                first_out.columns.len()
            )));
        }
        for (column, (name, expected)) in out.columns.iter().zip(&first_out.columns).enumerate() {
            if name != expected {
                return Err(SqlError::coded(DATATYPE_MISMATCH, format!(
                    "UNION column {} is `{expected}` in branch 1 and `{name}` in branch {branch}: every branch of a UNION returns the same columns, by name and in order",
                    column + 1
                )));
            }
        }
        for (column, ty) in out.types.iter().enumerate() {
            types[column] = unify(types[column].clone(), ty.clone()).map_err(|(before, here)| {
                SqlError::coded(DATATYPE_MISMATCH, format!(
                    "UNION column `{}` is {} in branch {branch} and {} in the branches before it, which do not unify: only BIGINT and DOUBLE PRECISION meet (as DOUBLE PRECISION); cast one side",
                    first_out.columns[column],
                    described(&here),
                    described(&before)
                ))
            })?;
            nullable[column] |= next.slot(SlotId(column as u16)).nullable;
        }
    }
    let mut schema = BindingSchema::default();
    for (at, ty) in types.iter().enumerate() {
        schema.add(SlotInfo {
            name: first.slot(SlotId(at as u16)).name.clone(),
            ty: ty.clone(),
            provenance: Provenance::Returned { stage: number },
            nullable: nullable[at],
        })?;
    }
    for (next, _) in returned {
        for (name, stage) in next.dropped() {
            schema.drop_name(name.clone(), *stage);
        }
    }
    let output = Output {
        columns: first_out.columns.clone(),
        types,
    };
    Ok((schema, output))
}
