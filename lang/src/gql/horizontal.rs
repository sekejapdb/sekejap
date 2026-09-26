//! Horizontal aggregation (`docs/lang/GQL_PROFILE_DESIGN.md` §4.5; brief §6,
//! "Aggregation has two different axes").
//!
//! An aggregate is classified by WHERE it stands, never by its spelling:
//!
//! * in a `RETURN` item or the `RETURN`'s `ORDER BY` it is VERTICAL: it
//!   folds the ROWS of the working table (`stage.rs`, M3-B). Its argument
//!   may not name a group variable, and may not hold a horizontal aggregate:
//!   a per-path value is computed in a `LET` first;
//! * in a `LET`, a `FILTER` or a `MATCH`'s `WHERE` it is HORIZONTAL: it
//!   folds, per row, the LIST one list variable of its argument holds -- a
//!   group variable (the edges or nodes one quantifier bound along ONE
//!   path), or a list a `LET`, `NODES`/`EDGES` or an earlier stage's
//!   `ARRAY_AGG` made. Inside the argument, and only there, that variable
//!   is ONE element, so `SUM(e.cost)` is the sum of the path's edge costs
//!   and `ARRAY_AGG(ns._key)` the keys of its nodes;
//! * anywhere else -- an inline element predicate (which the search
//!   evaluates per iteration, where a group variable is one edge), a
//!   `COST`, a `FOR` list, a `GROUP BY` key, or inside another aggregate --
//!   it is refused.
//!
//! The fold follows SQL's aggregate rules over the list's elements: `NULL`
//! values are skipped, `COUNT` over an empty list is 0, `SUM`, `AVG`,
//! `MIN` and `MAX` over an empty list or a list of nulls are `NULL` (so
//! `COALESCE(SUM(e.cost), 0.0)` is the zero-length idiom), `ARRAY_AGG` keeps
//! nulls and over an empty list is an empty list (Q9), and `COUNT(DISTINCT
//! x)` compares elements by identity and values by value (§2.2). `SUM` of
//! integers is an integer, exactly as the vertical `SUM` computes it. A
//! `NULL` list folds to `NULL`, whatever the aggregate.
//!
//! **Cost, named:** a fold copies nothing: its argument reads the input row
//! with the element in the list's slot (`eval::View`), once per element,
//! and every aggregate but `ARRAY_AGG` and `COUNT(DISTINCT)` folds each
//! value as it comes. Nothing is charged here beyond what that argument
//! reads. A list is held by the operator that keeps it, as every list is
//! (`EvalCx`).

use super::ast::{AggFunc, Expr};
use super::expr::{Ex, Lowering};
use super::schema::Name;
use super::stage::names;
use crate::{SqlError, SqlResult2};
use super::eval::{kind, mismatch, Evaluated, View};
use super::scalar::bigint_out_of_range;
use super::types;
use sekejap_core::collections::gql::{BindingValue, ListRef, SlotId, ValueType};
use std::collections::HashSet;

/// Where the aggregate being lowered stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Horizontal {
    /// Not a place for a horizontal aggregate.
    Refused,
    /// A `LET`, a `FILTER` or a `MATCH`'s `WHERE`.
    Allowed,
    /// Inside the argument of a fold over this list slot, where the list
    /// variable is one element.
    Folding(SlotId),
}

/// A horizontal aggregate, lowered.
#[derive(Clone, Debug)]
pub(crate) struct Fold {
    pub(crate) func: AggFunc,
    pub(crate) distinct: bool,
    /// The list variable folded.
    pub(crate) list: SlotId,
    /// The argument, evaluated once per element with slot `list` holding
    /// that element.
    pub(crate) arg: Ex,
}

/// Classify and lower the aggregate `written` = `func([DISTINCT] arg)`
/// that `lowering` meets outside a grouped `RETURN`'s computed items.
pub(crate) fn aggregate(
    lowering: &mut Lowering<'_>,
    written: &Expr,
    func: AggFunc,
    distinct: bool,
    arg: Option<&Expr>,
) -> SqlResult2<Ex> {
    if lowering.horizontal != Horizontal::Allowed {
        return Err(SqlError::unsupported(format!(
            "{written}: an aggregate folds the rows of the working table in a RETURN item or the RETURN's ORDER BY (vertical), or, in a LET, a FILTER or a MATCH's WHERE, the list one list variable holds, per row (horizontal: `LET total = SUM(e.cost)`); it stands nowhere else -- not in an inline element predicate, a COST, a FOR list, a GROUP BY key or inside another aggregate"
        )));
    }
    let Some(arg) = arg else {
        return Err(SqlError::unsupported(format!(
            "{written} counts rows, a vertical aggregate that stands in a RETURN; a horizontal COUNT names the list it counts, `COUNT(e)`"
        )));
    };
    let mut lists: Vec<(Name, SlotId)> = Vec::new();
    for name in names(arg) {
        if let Some(slot) = lowering.schema.resolve(&name) {
            if matches!(lowering.schema.slot(slot).ty, ValueType::List(_))
                && !lists.iter().any(|(_, seen)| *seen == slot)
            {
                lists.push((name, slot));
            }
        }
    }
    let list = match &lists[..] {
        [(_, slot)] => *slot,
        [] => {
            return Err(SqlError::unsupported(format!(
                "{written} stands in a LET, a FILTER or a MATCH's WHERE, where an aggregate is horizontal: it folds the list one list variable of its argument holds, and `{arg}` names no list variable. A vertical aggregate over the working table's rows stands in a RETURN"
            )))
        }
        [(first, _), (second, _), ..] => {
            return Err(SqlError::unsupported(format!(
                "{written}: a horizontal aggregate folds one list, and `{arg}` names the lists `{first}` and `{second}`; fold each in its own aggregate"
            )))
        }
    };
    lowering.horizontal = Horizontal::Folding(list);
    lowering.admitted.push(list);
    let lowered = lowering.value(arg);
    lowering.admitted.pop();
    lowering.horizontal = Horizontal::Allowed;
    Ok(Ex::Fold(Box::new(Fold {
        func,
        distinct,
        list,
        arg: lowered?,
    })))
}

impl Fold {
    /// Fold the list in `row`'s slot `list`, evaluating the argument per
    /// element through `value`, with that element in the list's slot.
    pub(crate) fn eval(
        &self,
        row: View<'_>,
        value: &mut dyn FnMut(&Ex, View<'_>) -> Evaluated<BindingValue>,
    ) -> Evaluated<BindingValue> {
        let items = match row.get(self.list) {
            BindingValue::List(list) => &list.items,
            BindingValue::Null => return Ok(BindingValue::Null),
            other => {
                return Err(mismatch(format!(
                    "{} folds a list, not {}",
                    self.func.written(),
                    kind(other)
                )))
            }
        };
        let mut acc = Acc::new(self);
        for item in items.iter() {
            acc.add(self.func, value(&self.arg, row.with(self.list, item))?)?;
        }
        acc.finish(self.func)
    }
}

/// A fold's running state: every aggregate folds each value as it comes
/// but `ARRAY_AGG`, which keeps them all, and `COUNT(DISTINCT)`, which
/// keeps each once.
enum Acc {
    Count(usize),
    Distinct(HashSet<BindingValue>),
    /// `SUM` and `AVG`: integers exactly, floats apart, and how many.
    Sum { int: i128, float: f64, floats: bool, n: u64 },
    /// `MIN` and `MAX`: the value kept so far.
    Kept(Option<BindingValue>),
    Array(Vec<BindingValue>),
}

impl Acc {
    fn new(fold: &Fold) -> Self {
        match fold.func {
            AggFunc::Count if fold.distinct => Self::Distinct(HashSet::new()),
            AggFunc::Count => Self::Count(0),
            AggFunc::Sum | AggFunc::Avg => Self::Sum {
                int: 0,
                float: 0.0,
                floats: false,
                n: 0,
            },
            AggFunc::Min | AggFunc::Max => Self::Kept(None),
            AggFunc::ArrayAgg => Self::Array(Vec::new()),
        }
    }

    /// Fold one element's value in. `NULL` is skipped, but by `ARRAY_AGG`.
    fn add(&mut self, func: AggFunc, value: BindingValue) -> Evaluated<()> {
        match self {
            Self::Array(values) => values.push(value),
            _ if matches!(value, BindingValue::Null) => {}
            Self::Count(n) => *n += 1,
            Self::Distinct(seen) => {
                seen.insert(value);
            }
            Self::Sum { int, float, floats, n } => {
                match value {
                    BindingValue::Int(i) => *int += i128::from(i),
                    BindingValue::Float(f) => {
                        *float += f;
                        *floats = true;
                    }
                    other => {
                        return Err(mismatch(format!(
                            "{} takes numbers, not {}",
                            func.written(),
                            kind(&other)
                        )))
                    }
                }
                *n += 1;
            }
            // The first of equal minima, and the last of equal maxima, as
            // `Iterator::min` and `max` keep them.
            Self::Kept(kept) => {
                let better = kept.as_ref().is_none_or(|old| match func {
                    AggFunc::Min => value < *old,
                    _ => value >= *old,
                });
                if better {
                    *kept = Some(value);
                }
            }
        }
        Ok(())
    }

    fn finish(self, func: AggFunc) -> Evaluated<BindingValue> {
        let count = |n: usize| BindingValue::Int(i64::try_from(n).unwrap_or(i64::MAX));
        Ok(match self {
            Self::Count(n) => count(n),
            Self::Distinct(seen) => count(seen.len()),
            Self::Sum { n: 0, .. } => BindingValue::Null,
            Self::Sum { int, float, n, .. } if func == AggFunc::Avg => {
                BindingValue::Float((int as f64 + float) / n as f64)
            }
            Self::Sum { int, float, floats: true, .. } => BindingValue::Float(int as f64 + float),
            Self::Sum { int, .. } => {
                BindingValue::Int(i64::try_from(int).map_err(|_| bigint_out_of_range())?)
            }
            Self::Kept(kept) => kept.unwrap_or(BindingValue::Null),
            Self::Array(values) => {
                let elem = types::shared(values.iter().map(types::value_type))
                    .unwrap_or(ValueType::Unknown);
                BindingValue::List(ListRef {
                    items: values.into(),
                    elem,
                })
            }
        })
    }
}
