//! `EXISTS { ... }` and `NOT EXISTS { ... }`: scope, placement and planning
//! of the body as the inner side of one engine `ExistsApply`
//! (`docs/lang/GQL_PROFILE_DESIGN_M5_M7.md` §2.3; owner answers Q18, Q19;
//! M5-C).
//!
//! **Scope.** A body is planned into the stage it stands in, as an
//! `OPTIONAL MATCH` is: its slots are slots of the stage's row, because the
//! inner side starts from that row. A name the body shares with the scope
//! around it IS that variable (implicit correlation): a pattern naming it
//! is seeded from it (`SeedSource::Bound`), and naming an outer node as an
//! edge, or binding an outer name again with a `LET`, is the same error it
//! is anywhere in a stage. The body's own variables are local: after the
//! `}` no name resolves to their slots (`BindingSchema::close_from`), so
//! they are never in `RETURN *` and a later statement may bind the name
//! afresh.
//!
//! **The two forms.** A top-level conjunct of a `FILTER` or of a `MATCH`'s
//! `WHERE` -- `EXISTS { .. }`, or the `NOT` of one -- is the FILTER form:
//! the conjunct comes out of the predicate and the apply itself keeps or
//! drops the row. Anywhere else (inside `OR`, `CASE`, a `LET`, a `FOR`
//! list, a `RETURN` item or `ORDER BY` key, an aggregate's argument) it is
//! the MARK form: the apply writes whether the body gave a row into a
//! hidden `BOOLEAN` slot, and the expression reads that slot (Q19). One
//! operator serves every position; there is no second evaluator.
//!
//! **Placement.** Every variable a statement reads is bound before it, so
//! a `LET`'s, a `FOR`'s, a `RETURN`'s and a mark-form `FILTER`'s tests run
//! right before the statement's own operator, and a `FILTER`'s filter-form
//! tests right after its `Filter`. A `MATCH` binds variables of its own:
//!
//! * a filter-form test runs right after the operator that binds the last
//!   outer variable its body names -- right after the seed when it names
//!   only the seed;
//! * a mark-form test runs after the pattern, just before the `MATCH`'s
//!   `WHERE`, which reads its slot (the slot is bound only then, so no
//!   predicate that reads it is placed earlier);
//! * an element's inline `WHERE` conjunct that holds an `EXISTS` is moved
//!   to the `MATCH`'s `WHERE` first: in a pattern with no selector, a test
//!   on an element outside every quantifier gives the same rows either way.
//!   Inside a quantified subpath, on a quantified edge, in a `COST` or in a
//!   selective pattern (`ANY`, `ANY SHORTEST`, `ANY CHEAPEST`) the
//!   predicate runs during the path search, once per search state, and an
//!   `EXISTS` there is refused, naming the rule.
//!
//! In an `OPTIONAL MATCH` the tests sit inside its `OptionalApply`, so they
//! decide whether a match exists, like the rest of its `WHERE`.
//!
//! **Cost (named, design §7).** The engine rebuilds the inner tree per
//! input row (one small allocation, no store read) and stops at its first
//! row; nothing here adds a store read.

use super::ast::{Expr, PathElement, PathPattern, Return, Stage, Statement};
use super::expr::Ex;
use super::plan::{FilterAt, Op, Planner};
use super::stage::Reader;
use super::schema::{Name, Provenance, SlotInfo};
use crate::{SqlError, SqlResult2};
use sekejap_core::collections::gql::{ExistsMode, SlotId, Target, ValueType};

/// What a statement's `EXISTS` tests leave to plan after its own operators.
#[derive(Default)]
pub(super) struct Deferred {
    /// The statement is a `MATCH`, whose tests wait for its pattern.
    in_match: bool,
    /// Filter-form tests: the body, and whether it is negated.
    filters: Vec<(Vec<Statement>, bool)>,
    /// A `MATCH`'s mark-form tests: the body, and its slot.
    marks: Vec<(Vec<Statement>, SlotId)>,
}

impl Planner<'_> {
    /// Before `statement`, the statement `number`'s stage plans next: the
    /// statement with its filter-form tests taken out (`None` when nothing
    /// is left of a `FILTER`), and what [`Planner::exists_after`] plans
    /// after it. A `LET`'s, a `FOR`'s and a `FILTER`'s mark-form tests are
    /// planned here, before the statement's operator.
    pub(super) fn exists_before(
        &mut self,
        statement: &Statement,
        number: u16,
        patterns: &mut u16,
        notices: &mut Vec<String>,
    ) -> SqlResult2<(Option<Statement>, Deferred)> {
        let mut deferred = Deferred::default();
        let statement = match statement {
            Statement::Match {
                patterns: written,
                where_,
                optional,
            } => {
                deferred.in_match = true;
                let mut written = written.clone();
                let mut conjuncts = Vec::new();
                if let Some(predicate) = where_ {
                    split(predicate.clone(), &mut conjuncts);
                }
                for pattern in &mut written {
                    lift(pattern, &mut conjuncts)?;
                }
                let mut kept = Vec::new();
                for conjunct in conjuncts {
                    if let Some(test) = filter_form(&conjunct) {
                        deferred.filters.push(test);
                        continue;
                    }
                    for exists in unique(&conjunct) {
                        let Expr::Exists(body) = exists else {
                            unreachable!("unique finds EXISTS")
                        };
                        let slot = self.mark_slot(exists, number)?;
                        deferred.marks.push((body.clone(), slot));
                    }
                    kept.push(conjunct);
                }
                Some(Statement::Match {
                    patterns: written,
                    where_: and_all(kept),
                    optional: *optional,
                })
            }
            Statement::Filter(predicate) => {
                let mut conjuncts = Vec::new();
                split(predicate.clone(), &mut conjuncts);
                let mut kept = Vec::new();
                for conjunct in conjuncts {
                    match filter_form(&conjunct) {
                        Some(test) => deferred.filters.push(test),
                        None => {
                            self.marks_now(&[&conjunct], number, patterns, notices)?;
                            kept.push(conjunct);
                        }
                    }
                }
                and_all(kept).map(Statement::Filter)
            }
            Statement::Let(assignments) => {
                let exprs: Vec<&Expr> = assignments.iter().map(|(_, expr)| expr).collect();
                self.marks_now(&exprs, number, patterns, notices)?;
                Some(statement.clone())
            }
            Statement::For { list, .. } => {
                self.marks_now(&[list], number, patterns, notices)?;
                Some(statement.clone())
            }
            // A CALL body's EXISTS tests are its own statements' (`call`).
            Statement::Call { .. } => Some(statement.clone()),
        };
        Ok((statement, deferred))
    }

    /// After the statement's own operators, which start at `first_op`: its
    /// filter-form tests, and a `MATCH`'s mark-form ones (module doc).
    pub(super) fn exists_after(
        &mut self,
        deferred: Deferred,
        first_op: usize,
        number: u16,
        patterns: &mut u16,
        notices: &mut Vec<String>,
    ) -> SqlResult2<()> {
        if !deferred.in_match {
            for (body, negated) in deferred.filters {
                let inner = self.body(&body, number, patterns, notices)?;
                self.ops.push(Op::Exists {
                    inner,
                    mode: ExistsMode::Filter { negated },
                });
            }
            return Ok(());
        }
        // The MATCH's WHERE, if it is left, is its last operator; a mark is
        // written before it, since it reads the mark.
        let before_where = match self.ops.last() {
            Some(Op::Filter {
                at: FilterAt::AfterPattern,
                ..
            }) if self.ops.len() > first_op => self.ops.len() - 1,
            _ => self.ops.len(),
        };
        let mut placed: Vec<(usize, Op)> = Vec::new();
        let mut written = Vec::with_capacity(deferred.marks.len());
        for (body, slot) in deferred.marks {
            let inner = self.body(&body, number, patterns, notices)?;
            placed.push((
                before_where,
                Op::Exists {
                    inner,
                    mode: ExistsMode::Mark { slot },
                },
            ));
            written.push(slot);
        }
        for (body, negated) in deferred.filters {
            let at = self.after_binding(first_op, &body);
            let inner = self.body(&body, number, patterns, notices)?;
            placed.push((
                at,
                Op::Exists {
                    inner,
                    mode: ExistsMode::Filter { negated },
                },
            ));
        }
        // Insert from the last position back, so each position still
        // names the operator it was computed against; tests at one
        // position keep their written order.
        placed.sort_by_key(|(at, _)| *at);
        for (at, op) in placed.into_iter().rev() {
            self.ops.insert(at, op);
        }
        for slot in written {
            self.bind_slot(slot);
        }
        Ok(())
    }

    /// Every `EXISTS` of a stage's `RETURN` -- its items, `GROUP BY` and
    /// `ORDER BY` keys -- in the mark form, before the `RETURN`'s operators.
    /// One in a grouped `RETURN` item reads a group, where no mark is; it
    /// is refused as that item is lowered.
    pub(super) fn return_exists(
        &mut self,
        ret: &Return,
        number: u16,
        patterns: &mut u16,
        notices: &mut Vec<String>,
    ) -> SqlResult2<()> {
        let exprs: Vec<&Expr> = ret
            .items
            .iter()
            .map(|item| &item.expr)
            .chain(ret.group_by.iter().flatten())
            .chain(ret.order_by.iter().map(|key| &key.expr))
            .filter(|expr| holds_exists(expr))
            .collect();
        if exprs.is_empty() {
            return Ok(());
        }
        self.marks_now(&exprs, number, patterns, notices)
    }

    /// Plan every `EXISTS` of `exprs` in the mark form, here and now, each
    /// distinct one once.
    fn marks_now(
        &mut self,
        exprs: &[&Expr],
        number: u16,
        patterns: &mut u16,
        notices: &mut Vec<String>,
    ) -> SqlResult2<()> {
        let mut seen: Vec<&Expr> = Vec::new();
        for expr in exprs {
            for exists in unique(expr) {
                if seen.contains(&exists) {
                    continue;
                }
                seen.push(exists);
                let Expr::Exists(body) = exists else {
                    unreachable!("unique finds EXISTS")
                };
                let slot = self.mark_slot(exists, number)?;
                let inner = self.body(body, number, patterns, notices)?;
                self.ops.push(Op::Exists {
                    inner,
                    mode: ExistsMode::Mark { slot },
                });
                self.bind_slot(slot);
            }
        }
        Ok(())
    }

    /// The hidden `BOOLEAN` slot a mark-form `exists` writes, recorded so
    /// the expression holding it reads it.
    fn mark_slot(&mut self, exists: &Expr, number: u16) -> SqlResult2<SlotId> {
        let slot = self.schema.add(SlotInfo {
            name: None,
            ty: ValueType::Bool,
            provenance: Provenance::Exists { stage: number },
            nullable: false,
        })?;
        self.schema.mark(exists.clone(), slot);
        Ok(slot)
    }

    /// Plan `body` into the stage, and take its operators out as an inner
    /// side. Its variables go out of scope, and out of the bound set.
    fn body(
        &mut self,
        body: &[Statement],
        number: u16,
        patterns: &mut u16,
        notices: &mut Vec<String>,
    ) -> SqlResult2<Vec<Op>> {
        if body.iter().any(|statement| matches!(statement, Statement::Call { .. })) {
            return Err(SqlError::unsupported(
                "CALL inside an EXISTS body: an EXISTS asks only whether a row exists, so write the CALL's MATCH in the EXISTS body itself",
            ));
        }
        let (width, first_op, bound) = (self.schema.width(), self.ops.len(), self.bound.len());
        self.scoped(first_op, |planner| planner.statements(body, number, patterns, notices))?;
        let inner = self.ops.drain(first_op..).collect();
        self.schema.close_from(width);
        self.bound.truncate(bound);
        Ok(inner)
    }

    /// `CALL (imports) { body }` (M5-D, design §2.4): the body is one stage,
    /// planned into this stage's row like an `EXISTS` body, but seeing only
    /// `imports`; its `RETURN` projects to exactly its returned columns (a
    /// second `Project` drops a hidden sort key), and the engine's
    /// `CallApply` writes each row it gives into new slots named by those
    /// columns. A name that collides with a variable in scope is PostgreSQL's
    /// duplicate alias, `42712` (`BindingSchema::add`).
    pub(super) fn call(
        &mut self,
        imports: &[Name],
        body: &Stage,
        number: u16,
        patterns: &mut u16,
        notices: &mut Vec<String>,
    ) -> SqlResult2<()> {
        let mut imported = Vec::with_capacity(imports.len());
        for name in imports {
            let slot = self.schema.resolve(name).ok_or_else(|| {
                SqlError::coded(
                    crate::sqlstate::UNDEFINED_COLUMN,
                    format!("CALL imports `{name}`, which is not bound before the CALL"),
                )
            })?;
            imported.push(slot);
        }
        let (width, first_op, bound) = (self.schema.width(), self.ops.len(), self.bound.len());
        let hidden = self.schema.hide_except(&imported);
        let (next, project, columns, out) = self.scoped(first_op, |planner| {
            planner.statements(&body.statements, number, patterns, notices)?;
            planner.return_exists(&body.ret, number, patterns, notices)?;
            planner.ret(&body.ret, number, Reader::Call, false, None)
        })?;
        let visible = out.columns.len();
        if let Op::Project { width: at, .. } = &mut self.ops[project] {
            *at = next.row_width();
        }
        if next.width() != visible {
            let stage_schema = std::mem::replace(&mut self.schema, next.clone());
            let cols = (0..visible).map(|at| self.expr(Ex::Slot(SlotId(at as u16)))).collect();
            self.schema = stage_schema;
            self.ops.push(Op::Project {
                cols,
                width: visible as u16,
            });
        }
        let inner: Vec<Op> = self.ops.drain(first_op..).collect();
        self.schema.reopen(hidden);
        self.schema.close_from(width);
        self.bound.truncate(bound);
        let mut outputs = Vec::with_capacity(visible);
        for at in 0..visible {
            let info = next.slot(SlotId(at as u16));
            outputs.push(self.schema.add(SlotInfo {
                name: info.name.clone(),
                ty: info.ty.clone(),
                provenance: Provenance::Called { stage: number },
                nullable: info.nullable,
            })?);
        }
        for slot in &outputs {
            self.bind_slot(*slot);
        }
        self.ops.push(Op::Call {
            inner,
            outputs: outputs.into(),
            columns,
        });
        Ok(())
    }

    /// The first position, from `first_op` on, after every operator that
    /// binds a variable of the scope around `body` that `body` names, and
    /// after the filters that follow that operator: a cheaper test that
    /// drops a row first spares the body a run.
    fn after_binding(&self, first_op: usize, body: &[Statement]) -> usize {
        let mut names = Vec::new();
        statement_names(body, &mut names);
        let outer: Vec<SlotId> = names
            .iter()
            .filter_map(|name| self.schema.resolve(name))
            .collect();
        let mut at = first_op;
        for (position, op) in self.ops.iter().enumerate().skip(first_op) {
            if binds(op).iter().any(|slot| outer.contains(slot)) {
                at = position + 1;
            }
        }
        while at > first_op && matches!(self.ops.get(at), Some(Op::Filter { .. })) {
            at += 1;
        }
        at
    }
}

/// True when an `EXISTS` stands anywhere in `expr`, outside other bodies.
pub(super) fn holds_exists(expr: &Expr) -> bool {
    matches!(expr, Expr::Exists(_)) || expr.children().into_iter().any(holds_exists)
}

/// True when `statement` itself holds an `EXISTS` (a `CALL` body's are
/// planned by the call): the planner skips the EXISTS pass, and its copy of
/// the statement, for every statement that holds none.
pub(super) fn statement_holds_exists(statement: &Statement) -> bool {
    fn any_in(elements: &[PathElement]) -> bool {
        elements.iter().any(|element| match element {
            PathElement::Node(node) => node.where_.as_ref().is_some_and(holds_exists),
            PathElement::Edge(edge) => {
                edge.where_.as_ref().is_some_and(holds_exists) || edge.cost.as_ref().is_some_and(holds_exists)
            }
            PathElement::Group { elements: inner, .. } => any_in(inner),
        })
    }
    match statement {
        Statement::Match {
            patterns, where_, ..
        } => where_.as_ref().is_some_and(holds_exists) || patterns.iter().any(|p| any_in(&p.elements)),
        Statement::Let(assignments) => assignments.iter().any(|(_, expr)| holds_exists(expr)),
        Statement::Filter(predicate) => holds_exists(predicate),
        Statement::For { list, .. } => holds_exists(list),
        Statement::Call { .. } => false,
    }
}

/// Each `EXISTS` in `expr`, outermost first, each distinct one once; one
/// inside another's body is that body's own.
fn unique(expr: &Expr) -> Vec<&Expr> {
    fn walk<'e>(expr: &'e Expr, out: &mut Vec<&'e Expr>) {
        if matches!(expr, Expr::Exists(_)) {
            if !out.contains(&expr) {
                out.push(expr);
            }
            return;
        }
        for child in expr.children() {
            walk(child, out);
        }
    }
    let mut out = Vec::new();
    walk(expr, &mut out);
    out
}

/// A top-level conjunct that is the filter form: its body, and whether it
/// is `NOT EXISTS`.
fn filter_form(conjunct: &Expr) -> Option<(Vec<Statement>, bool)> {
    match conjunct {
        Expr::Exists(body) => Some((body.clone(), false)),
        Expr::Not(inner) => match &**inner {
            Expr::Exists(body) => Some((body.clone(), true)),
            _ => None,
        },
        _ => None,
    }
}

/// Split a predicate at its top-level `AND`s, in written order.
fn split(expr: Expr, out: &mut Vec<Expr>) {
    match expr {
        Expr::And(left, right) => {
            split(*left, out);
            split(*right, out);
        }
        other => out.push(other),
    }
}

/// The conjunction of `conjuncts`, left to right; `None` for none.
fn and_all(conjuncts: Vec<Expr>) -> Option<Expr> {
    conjuncts
        .into_iter()
        .reduce(|left, right| Expr::And(Box::new(left), Box::new(right)))
}

/// Move each inline conjunct of `pattern` that holds an `EXISTS` into
/// `conjuncts`, the `MATCH`'s `WHERE`; refuse one the path search would
/// evaluate per search state.
fn lift(pattern: &mut PathPattern, conjuncts: &mut Vec<Expr>) -> SqlResult2<()> {
    let selective = pattern.prefix.is_some_and(|prefix| prefix.is_selector());
    for element in &mut pattern.elements {
        match element {
            PathElement::Node(node) => lift_where(&mut node.where_, selective, conjuncts)?,
            PathElement::Edge(edge) => {
                if edge.cost.as_ref().is_some_and(holds_exists) {
                    return Err(during_search());
                }
                lift_where(
                    &mut edge.where_,
                    selective || edge.quantifier.is_some(),
                    conjuncts,
                )?;
            }
            PathElement::Group { elements, .. } => {
                if in_group(elements) {
                    return Err(during_search());
                }
            }
        }
    }
    Ok(())
}

/// One element's inline `WHERE`: its conjuncts that hold an `EXISTS` move
/// to `conjuncts`, unless the search evaluates it (`during`).
fn lift_where(
    where_: &mut Option<Expr>,
    during: bool,
    conjuncts: &mut Vec<Expr>,
) -> SqlResult2<()> {
    let Some(predicate) = where_.take() else {
        return Ok(());
    };
    if !holds_exists(&predicate) {
        *where_ = Some(predicate);
        return Ok(());
    }
    if during {
        return Err(during_search());
    }
    let mut own = Vec::new();
    split(predicate, &mut own);
    let (moved, kept): (Vec<Expr>, Vec<Expr>) = own.into_iter().partition(holds_exists);
    conjuncts.extend(moved);
    *where_ = and_all(kept);
    Ok(())
}

/// True when an element of a quantified subpath holds an `EXISTS` in its
/// `WHERE` or its `COST`.
fn in_group(elements: &[PathElement]) -> bool {
    elements.iter().any(|element| match element {
        PathElement::Node(node) => node.where_.as_ref().is_some_and(holds_exists),
        PathElement::Edge(edge) => {
            edge.where_.as_ref().is_some_and(holds_exists)
                || edge.cost.as_ref().is_some_and(holds_exists)
        }
        PathElement::Group { elements, .. } => in_group(elements),
    })
}

fn during_search() -> SqlError {
    SqlError::unsupported(
        "EXISTS { ... } in the WHERE of an element inside a quantifier, of a quantified edge, or of a selective (ANY, ANY SHORTEST, ANY CHEAPEST) path pattern, or in a COST: that predicate runs during the path search, once per search state, where a subquery per state is bounded by no budget line; write the test in the MATCH's WHERE or in a FILTER after the MATCH",
    )
}

/// Every variable name `statements` write, nested bodies included.
fn statement_names(statements: &[Statement], out: &mut Vec<Name>) {
    for statement in statements {
        match statement {
            Statement::Match {
                patterns, where_, ..
            } => {
                for pattern in patterns {
                    out.extend(pattern.name.clone());
                    element_names(&pattern.elements, out);
                }
                if let Some(predicate) = where_ {
                    expr_names(predicate, out);
                }
            }
            Statement::Let(assignments) => {
                for (_, expr) in assignments {
                    expr_names(expr, out);
                }
            }
            Statement::Filter(predicate) => expr_names(predicate, out),
            Statement::For { list, .. } => expr_names(list, out),
            Statement::Call { imports, .. } => out.extend(imports.iter().cloned()),
        }
    }
}

fn element_names(elements: &[PathElement], out: &mut Vec<Name>) {
    for element in elements {
        match element {
            PathElement::Node(node) => {
                out.extend(node.var.clone());
                if let Some(predicate) = &node.where_ {
                    expr_names(predicate, out);
                }
            }
            PathElement::Edge(edge) => {
                out.extend(edge.var.clone());
                for expr in edge.where_.iter().chain(&edge.cost) {
                    expr_names(expr, out);
                }
            }
            PathElement::Group { elements, .. } => element_names(elements, out),
        }
    }
}

fn expr_names(expr: &Expr, out: &mut Vec<Name>) {
    match expr {
        Expr::Var(name) | Expr::Property { var: name, .. } => out.push(name.clone()),
        Expr::Exists(body) => statement_names(body, out),
        other => {
            for child in other.children() {
                expr_names(child, out);
            }
        }
    }
}

/// The slots a pattern operator binds.
fn binds(op: &Op) -> Vec<SlotId> {
    match op {
        Op::Seed { out, .. } => vec![*out],
        Op::Expand { edge, to, .. } => {
            let mut out: Vec<SlotId> = edge.iter().copied().collect();
            if let Target::New(slot) = to {
                out.push(*slot);
            }
            out
        }
        Op::PathSearch { automaton, .. } => super::automaton::binds(automaton),
        _ => Vec::new(),
    }
}
