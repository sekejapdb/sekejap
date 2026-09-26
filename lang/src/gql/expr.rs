//! The expression IR of the GQL profile: the M2 subset and the M3-C scalar
//! pack (`docs/lang/GQL_PROFILE_DESIGN.md` §1.2, §2.3 rules 5 and 6, §2.5).
//!
//! An [`Ex`] is an AST expression with every variable resolved to its slot
//! and every property access resolved to the reader call it needs: a node
//! row or an edge bag. It is evaluated by `eval.rs` over a binding row the
//! engine hands in, through the engine's charged reader; nothing here reads
//! storage.
//!
//! Lowering enforces the typing rules the M2 subset can already decide:
//!
//! * a node or an edge is compared by IDENTITY only, with `=` or `<>`, and
//!   only with an element of its own kind (rule 5);
//! * an element is not a condition, not a column and not an operand: a
//!   predicate, an output column, or the argument of an operator or a
//!   function that is a bare node or edge is refused, naming the variable
//!   and what to write instead (rule 6). `IS [NOT] NULL` takes any value,
//!   and `IN` and a simple `CASE` compare by identity as `=` does;
//! * a variable the stage does not bind is a scope error naming it;
//! * a GROUP variable -- bound inside a quantifier -- is one element only
//!   in the inline predicates and the `COST` written inside that
//!   quantifier ([`Lowering::admitted`]); anywhere else it is a list, which
//!   only a horizontal aggregate's argument may name (`horizontal.rs`);
//! * a property is read from a node or an edge: a list's elements' property
//!   is read only inside a horizontal aggregate (`ARRAY_AGG(ns._key)`), and
//!   a path has none.
//!
//! The path and element functions (§4.4) take their element, path or list
//! argument as a value; their argument kinds and result types are decided
//! by the planner's typing (`Planner::slot_type`), with the list literals'.
//!
//! What the M3-D parameter table decides -- one type per `$n` across every
//! use -- is not decided here: a parameter's value is checked where it is
//! used, per execution.

use super::ast::{ArithOp, CastType, Expr, Func, GraphFunc, Literal};
use super::convert::{self, Element};
use super::horizontal::{self, Fold, Horizontal};
use super::schema::{BindingSchema, Name, Provenance, SlotInfo};
use crate::ast::CmpOp;
use crate::sqlstate::{DATATYPE_MISMATCH, GROUPING_ERROR, UNDEFINED_COLUMN};
use crate::{SqlError, SqlResult2};
use sekejap_core::collections::gql::{BindingValue, SlotId, ValueType};

/// A resolved expression.
#[derive(Clone, Debug)]
pub(crate) enum Ex {
    Const(BindingValue),
    /// `$n`, zero-based: `$1` is `Param(0)`.
    Param(usize),
    /// A variable's whole value: an element, compared by identity.
    Slot(SlotId),
    /// A property of the node in the slot: one charged row read.
    NodeProperty(SlotId, Box<str>),
    /// A property of the edge in the slot: its bag, free when the hop
    /// carried it, one charged posting read when it did not.
    EdgeProperty(SlotId, Box<str>),
    Compare(CmpOp, Box<Ex>, Box<Ex>),
    Not(Box<Ex>),
    And(Box<Ex>, Box<Ex>),
    Or(Box<Ex>, Box<Ex>),
    Neg(Box<Ex>),
    Arith(ArithOp, Box<Ex>, Box<Ex>),
    Concat(Box<Ex>, Box<Ex>),
    /// `IS NULL`; `IS NOT NULL` is its `Not`, which is exact because this
    /// test is never unknown.
    IsNull(Box<Ex>),
    /// `x IN (list)`; `NOT IN` is its `Not`, as in SQL.
    In(Box<Ex>, Vec<Ex>),
    /// `operand` is evaluated ONCE and compared with each `WHEN` value in
    /// the simple form; each `WHEN` is a condition in the searched form.
    Case {
        operand: Option<Box<Ex>>,
        branches: Vec<(Ex, Ex)>,
        otherwise: Option<Box<Ex>>,
    },
    Cast(Box<Ex>, CastType),
    Coalesce(Vec<Ex>),
    Nullif(Box<Ex>, Box<Ex>),
    Call(Func, Vec<Ex>),
    /// A path or element function (§4.4).
    Graph(GraphFunc, Box<Ex>),
    /// A list literal, its items in order.
    List(Vec<Ex>),
    /// A horizontal aggregate (§4.5).
    Fold(Box<Fold>),
    /// Inside a fold's argument: the element of the list in the slot, which
    /// the fold puts in that slot for each evaluation.
    Item(SlotId),
}

/// One conjunct of a predicate, with the slots it reads: the planner places
/// each conjunct at the first operator after which all of them are bound.
#[derive(Clone, Debug)]
pub(crate) struct Conjunct {
    pub(crate) ex: Ex,
    pub(crate) refs: Vec<SlotId>,
}

impl Ex {
    /// The slots this expression reads, each once.
    pub(crate) fn refs(&self) -> Vec<SlotId> {
        let mut out = Vec::new();
        self.collect_refs(&mut out);
        out
    }

    fn collect_refs(&self, out: &mut Vec<SlotId>) {
        match self {
            Self::Slot(slot)
            | Self::Item(slot)
            | Self::NodeProperty(slot, _)
            | Self::EdgeProperty(slot, _) => {
                if !out.contains(slot) {
                    out.push(*slot);
                }
            }
            other => {
                for child in other.children() {
                    child.collect_refs(out);
                }
            }
        }
    }

    /// The expressions this one is built from, in written order.
    pub(crate) fn children(&self) -> Vec<&Ex> {
        match self {
            Self::Const(_)
            | Self::Param(_)
            | Self::Slot(_)
            | Self::Item(_)
            | Self::NodeProperty(..)
            | Self::EdgeProperty(..) => Vec::new(),
            Self::Compare(_, left, right)
            | Self::And(left, right)
            | Self::Or(left, right)
            | Self::Arith(_, left, right)
            | Self::Concat(left, right)
            | Self::Nullif(left, right) => vec![&**left, &**right],
            Self::Not(inner)
            | Self::Neg(inner)
            | Self::IsNull(inner)
            | Self::Cast(inner, _)
            | Self::Graph(_, inner) => vec![&**inner],
            Self::List(items) => items.iter().collect(),
            // The fold's own list slot is read by the fold, as its argument
            // reads it.
            Self::Fold(fold) => vec![&fold.arg],
            Self::In(expr, list) => std::iter::once(&**expr).chain(list).collect(),
            Self::Coalesce(args) | Self::Call(_, args) => args.iter().collect(),
            Self::Case {
                operand,
                branches,
                otherwise,
            } => operand
                .as_deref()
                .into_iter()
                .chain(branches.iter().flat_map(|(when, then)| [when, then]))
                .chain(otherwise.as_deref())
                .collect(),
        }
    }

    /// `self AND next`, or `next` alone.
    pub(crate) fn and(this: Option<Self>, next: Self) -> Self {
        match this {
            Some(this) => Self::And(Box::new(this), Box::new(next)),
            None => next,
        }
    }
}

/// Split a predicate at its top-level `AND`s.
pub(crate) fn conjuncts(ex: Ex, out: &mut Vec<Conjunct>) {
    match ex {
        Ex::And(left, right) => {
            conjuncts(*left, out);
            conjuncts(*right, out);
        }
        ex => out.push(Conjunct {
            refs: ex.refs(),
            ex,
        }),
    }
}

/// Lowers AST expressions against one stage's schema, and records the
/// highest `$n` it meets.
pub(crate) struct Lowering<'s> {
    pub(crate) schema: &'s BindingSchema,
    /// How many parameters the statement reads: the highest `n` of `$n`,
    /// raised in place.
    pub(crate) params: &'s mut usize,
    /// The group variables the expression being lowered sees as ONE
    /// element: those of the quantifier it is written inside.
    pub(crate) admitted: Vec<SlotId>,
    /// In a grouped `RETURN` (M3-B): the expressions the grouping already
    /// computed -- the keys and the aggregates, as written -- each with the
    /// slot of the grouped row that holds it. `schema` is then that row's
    /// schema, whose only named slots are the keys that are variables.
    pub(crate) grouped: Option<&'s [(Expr, SlotId)]>,
    /// Whether an aggregate met here is a horizontal one (`horizontal.rs`).
    pub(crate) horizontal: Horizontal,
}

/// Is `ty` a group variable's type: a list of nodes or of edges?
pub(crate) fn is_group(ty: &ValueType) -> bool {
    matches!(ty, ValueType::List(element) if matches!(**element, ValueType::Node(_) | ValueType::Edge(_)))
}

/// Is `info` a group variable: a list of elements a pattern bound? A list
/// of nodes a `LET` holds (`LET ns = NODES(p)`) is an ordinary list value.
fn is_group_variable(info: &SlotInfo) -> bool {
    is_group(&info.ty) && matches!(info.provenance, Provenance::Element { .. })
}

impl Lowering<'_> {
    /// A predicate: a `WHERE` or an inline element `WHERE`.
    pub(crate) fn predicate(&mut self, expr: &Expr) -> SqlResult2<Ex> {
        let ex = self.lower(expr)?;
        self.not_an_element(&ex, "a condition")?;
        Ok(ex)
    }

    /// An output column of the final stage, which SQL must be able to
    /// represent.
    pub(crate) fn column(&mut self, expr: &Expr, name: &Name) -> SqlResult2<Ex> {
        let ex = self.lower(expr)?;
        if let Ex::Slot(slot) = ex {
            if let Some(element) = convert::element_type(&self.schema.slot(slot).ty) {
                return Err(convert::not_a_value(element, name, "is"));
            }
        }
        Ok(ex)
    }

    /// An operand of an operator or a function, and an edge's `COST`: a
    /// value, never an element.
    pub(crate) fn operand(&mut self, expr: &Expr) -> SqlResult2<Ex> {
        let ex = self.lower(expr)?;
        self.not_an_element(&ex, "a value")?;
        Ok(ex)
    }

    /// Any value, an element included: what a `LET`, a `FOR` list, an
    /// intermediate stage's column or a `COUNT` argument may hold.
    pub(crate) fn value(&mut self, expr: &Expr) -> SqlResult2<Ex> {
        self.lower(expr)
    }

    fn operands(&mut self, exprs: &[Expr]) -> SqlResult2<Vec<Ex>> {
        exprs.iter().map(|expr| self.operand(expr)).collect()
    }

    fn lower(&mut self, expr: &Expr) -> SqlResult2<Ex> {
        if let Some((_, slot)) = self
            .grouped
            .and_then(|computed| computed.iter().find(|(written, _)| written == expr))
        {
            return Ok(Ex::Slot(*slot));
        }
        Ok(match expr {
            Expr::Literal(literal) => Ex::Const(constant(literal)),
            Expr::Param(n) => {
                // The lexer reads `$0` as a parameter too; SQL numbers from 1.
                let at = n.checked_sub(1).ok_or_else(|| {
                    SqlError::Parameter("parameters are numbered from $1".into())
                })?;
                *self.params = (*self.params).max(*n);
                Ex::Param(at)
            }
            Expr::Var(name) => {
                let slot = self.resolve(name)?;
                if self.horizontal == Horizontal::Folding(slot) {
                    Ex::Item(slot)
                } else {
                    Ex::Slot(slot)
                }
            }
            Expr::Property { var, property } => {
                let slot = self.resolve(var)?;
                let property = property.as_str().into();
                match self.ty(slot) {
                    ValueType::Edge(_) => Ex::EdgeProperty(slot, property),
                    ValueType::List(_) => {
                        return Err(SqlError::coded(DATATYPE_MISMATCH, format!(
                            "`{var}` is a list: `{var}.{property}` reads each element's property only inside a horizontal aggregate over `{var}` in a LET, a FILTER or a MATCH's WHERE, such as `ARRAY_AGG({var}.{property})`"
                        )))
                    }
                    ValueType::Path => {
                        return Err(SqlError::coded(DATATYPE_MISMATCH, format!(
                            "`{var}` is a path, which has no properties; read them from `NODES({var})` or `EDGES({var})`"
                        )))
                    }
                    _ => Ex::NodeProperty(slot, property),
                }
            }
            Expr::Compare { op, left, right } => {
                let (left, right) = (self.lower(left)?, self.lower(right)?);
                self.comparable(*op, &left, &right)?;
                Ex::Compare(*op, Box::new(left), Box::new(right))
            }
            Expr::Not(inner) => Ex::Not(Box::new(self.predicate(inner)?)),
            Expr::And(left, right) => Ex::And(
                Box::new(self.predicate(left)?),
                Box::new(self.predicate(right)?),
            ),
            Expr::Or(left, right) => Ex::Or(
                Box::new(self.predicate(left)?),
                Box::new(self.predicate(right)?),
            ),
            Expr::Neg(inner) => Ex::Neg(Box::new(self.operand(inner)?)),
            Expr::Arith { op, left, right } => Ex::Arith(
                *op,
                Box::new(self.operand(left)?),
                Box::new(self.operand(right)?),
            ),
            Expr::Concat(left, right) => Ex::Concat(
                Box::new(self.operand(left)?),
                Box::new(self.operand(right)?),
            ),
            Expr::IsNull { expr, negated } => {
                let test = Ex::IsNull(Box::new(self.lower(expr)?));
                if *negated {
                    Ex::Not(Box::new(test))
                } else {
                    test
                }
            }
            Expr::In {
                expr,
                list,
                negated,
            } => {
                let value = self.lower(expr)?;
                let mut members = Vec::with_capacity(list.len());
                for member in list {
                    let member = self.lower(member)?;
                    self.comparable(CmpOp::Eq, &value, &member)?;
                    members.push(member);
                }
                let test = Ex::In(Box::new(value), members);
                if *negated {
                    Ex::Not(Box::new(test))
                } else {
                    test
                }
            }
            Expr::Case {
                operand,
                branches,
                otherwise,
            } => {
                let operand = operand.as_deref().map(|operand| self.lower(operand)).transpose()?;
                let mut lowered = Vec::with_capacity(branches.len());
                for (when, then) in branches {
                    let when = match &operand {
                        Some(operand) => {
                            let value = self.lower(when)?;
                            self.comparable(CmpOp::Eq, operand, &value)?;
                            value
                        }
                        None => self.predicate(when)?,
                    };
                    lowered.push((when, self.operand(then)?));
                }
                Ex::Case {
                    operand: operand.map(Box::new),
                    branches: lowered,
                    otherwise: otherwise
                        .as_deref()
                        .map(|otherwise| self.operand(otherwise))
                        .transpose()?
                        .map(Box::new),
                }
            }
            Expr::Cast { expr, to } => Ex::Cast(Box::new(self.operand(expr)?), *to),
            Expr::Coalesce(args) => Ex::Coalesce(self.operands(args)?),
            Expr::Nullif(left, right) => Ex::Nullif(
                Box::new(self.operand(left)?),
                Box::new(self.operand(right)?),
            ),
            Expr::Call { func, args } => Ex::Call(*func, self.operands(args)?),
            // A grouped `RETURN` found each of its vertical aggregates above.
            Expr::Aggregate {
                func,
                distinct,
                arg,
            } => return horizontal::aggregate(self, expr, *func, *distinct, arg.as_deref()),
            Expr::Graph { func, arg } => Ex::Graph(*func, Box::new(self.value(arg)?)),
            Expr::List(items) => Ex::List(
                items
                    .iter()
                    .map(|item| self.value(item))
                    .collect::<SqlResult2<_>>()?,
            ),
        })
    }

    fn resolve(&self, name: &Name) -> SqlResult2<SlotId> {
        let slot = self.schema.resolve(name).ok_or_else(|| {
            if let Some(stage) = self.schema.dropped_by(name) {
                return SqlError::coded(UNDEFINED_COLUMN, format!(
                    "variable `{name}` is out of scope: the RETURN of stage {stage} did not carry it through NEXT; return it there to use it here"
                ));
            }
            if self.grouped.is_some() {
                return SqlError::coded(GROUPING_ERROR, format!(
                    "variable `{name}` must appear in GROUP BY or be used inside an aggregate: a grouped RETURN reads only its keys and its aggregates"
                ));
            }
            SqlError::coded(UNDEFINED_COLUMN, format!(
                "variable `{name}` is not bound in this stage: a GQL expression names a variable a MATCH, LET or FOR before it binds"
            ))
        })?;
        let info = self.schema.slot(slot);
        if is_group_variable(info) && !self.admitted.contains(&slot) {
            return Err(SqlError::coded(DATATYPE_MISMATCH, format!(
                "variable `{name}` is a group variable: bound inside the quantifier at {}, it is one element only in the inline predicates and the COST written inside that quantifier, and the list of every iteration's element everywhere else, never its last element. Fold the list per path with a horizontal aggregate in a LET, a FILTER or the MATCH's WHERE (`LET total = SUM({name}.cost)`); a RETURN aggregate folds rows, so compute the per-path value in a LET first",
                bound_at(&info.provenance)
            )));
        }
        Ok(slot)
    }

    /// The type `slot` has in the expression being lowered: a group
    /// variable admitted here is one element.
    fn ty(&self, slot: SlotId) -> &ValueType {
        match &self.schema.slot(slot).ty {
            ValueType::List(element) if self.admitted.contains(&slot) => element,
            ty => ty,
        }
    }

    /// The element variable `ex` is, if it is one: its name and whether it
    /// is a node or an edge.
    fn element(&self, ex: &Ex) -> Option<(String, Element)> {
        let slot = match ex {
            Ex::Slot(slot) | Ex::Item(slot) => slot,
            Ex::Graph(func @ (GraphFunc::PathFirst | GraphFunc::PathLast), path) => {
                return Some((format!("{}({})", func.written(), self.written(path)), Element::Node))
            }
            _ => return None,
        };
        let element = match self.ty(*slot) {
            ValueType::Node(_) => Element::Node,
            ValueType::Edge(_) => Element::Edge,
            _ => return None,
        };
        Some((self.schema.slot(*slot).described(), element))
    }

    /// A path argument as an error names it: its variable when it is one.
    fn written(&self, ex: &Ex) -> String {
        match ex {
            Ex::Slot(slot) => self
                .schema
                .slot(*slot)
                .name
                .as_ref()
                .map_or_else(|| "a path".to_owned(), ToString::to_string),
            _ => "a path".to_owned(),
        }
    }

    fn not_an_element(&self, ex: &Ex, what: &str) -> SqlResult2<()> {
        match self.element(ex) {
            Some((name, element)) => Err(SqlError::coded(DATATYPE_MISMATCH, format!(
                "{name} is {}, not {what}; compare it by identity (`=`, `<>`) or use one of its properties",
                element.written()
            ))),
            None => Ok(()),
        }
    }

    /// Rule 5: identity comparison is its own kind of comparison.
    fn comparable(&self, op: CmpOp, left: &Ex, right: &Ex) -> SqlResult2<()> {
        match (self.element(left), self.element(right)) {
            (None, None) => Ok(()),
            (Some((a, a_kind)), Some((b, b_kind))) => {
                if a_kind != b_kind {
                    return Err(SqlError::coded(DATATYPE_MISMATCH, format!(
                        "{a} and {b} are a node and an edge: identity compares two elements of one kind"
                    )));
                }
                if !matches!(op, CmpOp::Eq | CmpOp::Ne) {
                    return Err(SqlError::coded(DATATYPE_MISMATCH, format!(
                        "{a} {} {b}: elements are compared by identity, with `=` or `<>`; order them by a property",
                        op.written()
                    )));
                }
                Ok(())
            }
            (Some((name, element)), None) | (None, Some((name, element))) => {
                Err(SqlError::coded(DATATYPE_MISMATCH, format!(
                    "{name} is {} and is compared by identity with another {0} only; compare one of its properties, such as its `_key`, with a value",
                    element.written()
                )))
            }
        }
    }
}

/// A literal as a value: an exact number that fits `i64` is an integer,
/// any other a float.
fn constant(literal: &Literal) -> BindingValue {
    match literal {
        Literal::Null => BindingValue::Null,
        Literal::Bool(b) => BindingValue::Bool(*b),
        Literal::Num(value, exact) => {
            if *exact && value.fract() == 0.0 && value.abs() < 9.2e18 {
                BindingValue::Int(*value as i64)
            } else {
                BindingValue::Float(*value)
            }
        }
        Literal::Str(text) => BindingValue::Text(text.as_str().into()),
    }
}

/// Where a variable was bound, for an error that names two uses of it.
pub(crate) fn bound_at(provenance: &Provenance) -> String {
    match provenance {
        Provenance::Element { pattern, position } => {
            format!("pattern {} position {position}", pattern.0 + 1)
        }
        Provenance::Returned { stage } => format!("the RETURN of stage {stage}"),
        Provenance::Let { stage } => format!("a LET of stage {stage}"),
        Provenance::Unnest { stage } => format!("a FOR of stage {stage}"),
        Provenance::Aggregate { stage } => format!("the grouping of stage {stage}"),
    }
}
