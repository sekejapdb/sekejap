//! A value's type, decided in ONE place (`docs/lang/GQL_PROFILE_DESIGN.md`
//! §2.2, §6.1; owner answer Q8).
//!
//! Every type the profile states -- a slot's, an output column's declared
//! SQL spelling, a `$n`'s, a list's element type at compile and at run time
//! -- is a [`ValueType`] computed here, and a spelling is only ever printed
//! from one ([`spelling`]), never read back:
//!
//! * [`Planner::slot_type`] types an expression the binder lowered;
//! * [`unify`] is the one rule for what a list holds, and what the branches
//!   of `CASE`, `COALESCE` and `NULLIF` give: one kind, integers among
//!   floats being floats (PostgreSQL's `bigint` and `double precision`
//!   resolving to `double precision`), and nodes (edges) of any labels
//!   being nodes (edges) of every label either may have. The evaluator's
//!   list values and a `$n` bound as a list are typed by the same rule
//!   ([`shared`]);
//! * [`aggregate_type`] types an aggregate and refuses one over an element,
//!   for the vertical and the horizontal aggregates alike;
//! * the statement's ONE parameter table (design §7): each `$n` gets the
//!   type every use that decides one agrees on -- the outer SELECT's, every
//!   stage's and every inline predicate's alike -- and two uses that
//!   disagree are PostgreSQL's `42P08` at prepare, naming both
//!   ([`Planner::note_params`], [`Planner::param_types`]); and each value
//!   bound to a typed `$n` is checked against that type when an execution
//!   opens ([`typed_param`]).

use super::ast::{AggFunc, ArithOp, CastType, Func};
use super::convert;
use super::elements;
use super::expr::Ex;
use super::plan::{show, Planner};
use crate::functions::is_time_type;
use crate::sqlstate::{AMBIGUOUS_PARAMETER, DATATYPE_MISMATCH};
use crate::{Param, SqlError, SqlResult2};
use sekejap_core::collections::gql::{BindingValue, ListRef, SlotId, ValueType};
use sekejap_core::Kind;

impl Planner<'_> {
    /// The type of the value `ex` gives, against the schema being planned.
    pub(super) fn slot_type(&self, ex: &Ex) -> SqlResult2<ValueType> {
        Ok(match ex {
            Ex::Const(value) => value_type(value),
            // A parameter's type is not decided by where it lands (Q8): it
            // is text, as the SQL surface reads an untyped `$n`.
            Ex::Param(_) => ValueType::Text,
            Ex::Slot(slot) => self.schema.slot(*slot).ty.clone(),
            // A fold's list variable is one element in its argument.
            Ex::Item(slot) => match &self.schema.slot(*slot).ty {
                ValueType::List(element) => (**element).clone(),
                other => other.clone(),
            },
            Ex::NodeProperty(slot, field) => {
                self.property_type(*slot, field)?.unwrap_or(ValueType::Text)
            }
            // An edge's bag declares nothing.
            Ex::EdgeProperty(..) | Ex::Concat(..) => ValueType::Text,
            Ex::Compare(..) | Ex::Not(_) | Ex::And(..) | Ex::Or(..) | Ex::IsNull(_) | Ex::In(..) => {
                ValueType::Bool
            }
            // A `$n` list is converted when an execution opens; any other
            // list operand is a list value.
            Ex::Member(_, list) => {
                if !matches!(**list, Ex::Param(_)) {
                    let ty = self.slot_type(list)?;
                    if !matches!(ty, ValueType::List(_) | ValueType::Unknown) {
                        return Err(SqlError::coded(
                            DATATYPE_MISMATCH,
                            format!(
                                "{}: `{}` is {}, not a list; a list of values is written `IN (a, b)`",
                                show(ex, &self.schema),
                                show(list, &self.schema),
                                described(&ty)
                            ),
                        ));
                    }
                }
                ValueType::Bool
            }
            Ex::Cast(_, to) => cast_type(*to),
            // PostgreSQL: integer arithmetic stays integer, anything with a
            // float or an undecided operand is double precision, and `^` is
            // always double precision. Double is also the safe type for an
            // operand not decided here: an integer value encodes exactly as
            // a float8, never the other way round.
            Ex::Neg(inner) => self.numeric(&[inner])?,
            Ex::Arith(ArithOp::Pow, ..) => ValueType::Float,
            Ex::Arith(_, left, right) => self.numeric(&[left, right])?,
            // PostgreSQL resolves the two arguments to one type, which is
            // the result's: an integer against a float is a float.
            Ex::Nullif(first, second) => {
                let first = self.slot_type(first)?;
                unify(first.clone(), self.slot_type(second)?).unwrap_or(first)
            }
            Ex::Coalesce(items) => self.agreed(items.iter())?,
            Ex::Case {
                branches,
                otherwise,
                ..
            } => self.agreed(branches.iter().map(|(_, then)| then).chain(otherwise.as_deref()))?,
            Ex::Call(func, args) => match func {
                Func::Abs => self.numeric(&[&args[0]])?,
                Func::Sqrt | Func::Power | Func::Exp | Func::Ln => ValueType::Float,
                Func::Length => ValueType::Int,
                Func::Lower | Func::Upper | Func::Trim | Func::Substring | Func::Concat => {
                    ValueType::Text
                }
            },
            Ex::Graph(func, arg) => elements::result_type(*func, &self.slot_type(arg)?)?,
            Ex::List(items) => {
                let mut elem = ValueType::Unknown;
                for item in items {
                    // A `$n` item takes the list's type.
                    if matches!(item, Ex::Param(_)) {
                        continue;
                    }
                    elem = unify(elem, self.slot_type(item)?).map_err(|(a, b)| {
                        SqlError::coded(
                            DATATYPE_MISMATCH,
                            format!(
                                "list literal {} mixes {} and {}: a list holds values of one kind (integers among floats are floats); cast one side",
                                show(ex, &self.schema),
                                spelling(&a),
                                spelling(&b)
                            ),
                        )
                    })?;
                }
                ValueType::List(Box::new(elem))
            }
            Ex::Fold(fold) => aggregate_type(
                fold.func,
                &self.slot_type(&fold.arg)?,
                &show(&fold.arg, &self.schema),
            )?,
        })
    }

    /// `BIGINT` when every operand is, otherwise `DOUBLE PRECISION`.
    fn numeric(&self, operands: &[&Ex]) -> SqlResult2<ValueType> {
        for operand in operands {
            if self.slot_type(operand)? != ValueType::Int {
                return Ok(ValueType::Float);
            }
        }
        Ok(ValueType::Int)
    }

    /// The one type the result branches of a `CASE` or a `COALESCE` share
    /// ([`unify`], so integers among floats are floats), `TEXT` when two
    /// share none (Q8).
    fn agreed<'e>(&self, items: impl Iterator<Item = &'e Ex>) -> SqlResult2<ValueType> {
        let mut agreed = ValueType::Unknown;
        for item in items {
            agreed = match unify(agreed, self.slot_type(item)?) {
                Ok(ty) => ty,
                Err(_) => return Ok(ValueType::Text),
            };
        }
        Ok(agreed)
    }

    /// The declared type of `field` on the node in `slot`: `_key` is text;
    /// otherwise the type every label collection declares for it, or `None`
    /// when one does not declare it, two differ, or the node is unlabelled.
    pub(super) fn property_type(&self, slot: SlotId, field: &str) -> SqlResult2<Option<ValueType>> {
        if field == crate::KEY_COLUMN {
            return Ok(Some(ValueType::Text));
        }
        // A list of nodes is read one node at a time, inside a fold.
        let collections = match &self.schema.slot(slot).ty {
            ValueType::Node(collections) => collections,
            ValueType::List(element) => match &**element {
                ValueType::Node(collections) => collections,
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };
        let mut agreed = None;
        for collection in collections.iter() {
            let info = self.db.collection_info(*collection).map_err(SqlError::from)?;
            let Some((_, kind)) = info.layout.fields.iter().find(|(name, _)| name == field)
            else {
                return Ok(None);
            };
            let time = info
                .declared
                .iter()
                .find(|(name, declared)| name == field && is_time_type(declared))
                .map(|(_, declared)| declared.as_str());
            let ty = declared_type(kind, time);
            if agreed.as_ref().is_some_and(|seen| *seen != ty) {
                return Ok(None);
            }
            agreed = Some(ty);
        }
        Ok(agreed)
    }

    /// Type every list literal, path or element function and horizontal
    /// aggregate `ex` holds, refusing a mixed-kind list, an argument of the
    /// wrong kind and a fold of elements when the statement is compiled.
    pub(super) fn typed(&self, ex: &Ex) -> SqlResult2<()> {
        if matches!(ex, Ex::Graph(..) | Ex::List(_) | Ex::Fold(_) | Ex::Member(..)) {
            self.slot_type(ex)?;
        }
        for child in ex.children() {
            self.typed(child)?;
        }
        Ok(())
    }

    /// Record in the parameter table what `ex` says about each `$n` it
    /// reads. `$n` compared with a value whose type is decided -- a
    /// property its label collections all declare alike, or a column or
    /// variable of a decided type -- takes that type; `x IN $n` makes `$n` a
    /// list of `x`'s type; any other use accepts the type the others
    /// decide. `ex` is bound against `self.schema`.
    ///
    /// Every lowered expression passes here, so this is also where the
    /// kinds M4-D decides at compile are checked ([`Planner::typed`]).
    pub(super) fn note_params(&mut self, ex: &Ex) -> SqlResult2<()> {
        self.typed(ex)?;
        let mut uses = Vec::new();
        self.param_uses(ex, &mut uses)?;
        for (at, used) in uses {
            self.deduce(at, used)?;
        }
        Ok(())
    }

    /// `$at+1` is the count of `clause` (`OFFSET` or `LIMIT`): a `BIGINT`,
    /// checked from 0 to `i64::MAX` when an execution opens (Q13).
    pub(super) fn note_count(&mut self, at: usize, clause: &str) -> SqlResult2<()> {
        self.deduce(
            at,
            ParamUse::Typed {
                ty: ValueType::Int,
                at: format!("{clause} ${}", at + 1),
            },
        )?;
        if !self.counts.contains(&at) {
            self.counts.push(at);
        }
        Ok(())
    }

    /// `$at+1` is read as a list of `elem` where `written` says.
    pub(super) fn note_list(&mut self, at: usize, elem: ValueType, written: String) -> SqlResult2<()> {
        self.deduce(
            at,
            ParamUse::Typed {
                ty: ValueType::List(Box::new(elem)),
                at: written,
            },
        )
    }

    /// Combine a use of `$at+1` with the uses before it.
    fn deduce(&mut self, at: usize, used: ParamUse) -> SqlResult2<()> {
        if self.uses.len() <= at {
            self.uses.resize(at + 1, ParamUse::Unused);
        }
        let before = std::mem::replace(&mut self.uses[at], ParamUse::Unused);
        self.uses[at] = before.and(used, at + 1)?;
        Ok(())
    }

    fn param_uses(&self, ex: &Ex, uses: &mut Vec<(usize, ParamUse)>) -> SqlResult2<()> {
        let typed = |ty: Option<ValueType>| match ty {
            Some(ty) => ParamUse::Typed {
                ty,
                at: show(ex, &self.schema),
            },
            None => ParamUse::Undecided,
        };
        match ex {
            Ex::Param(at) => uses.push((*at, ParamUse::Undecided)),
            Ex::Compare(_, left, right) => match (&**left, &**right) {
                (Ex::Param(at), other) | (other, Ex::Param(at)) if !matches!(other, Ex::Param(_)) => {
                    uses.push((*at, typed(self.decided(other)?)));
                    self.param_uses(other, uses)?;
                }
                _ => {
                    self.param_uses(left, uses)?;
                    self.param_uses(right, uses)?;
                }
            },
            Ex::Member(value, list) => match &**list {
                Ex::Param(at) => {
                    let elem = self.decided(value)?.unwrap_or(ValueType::Unknown);
                    uses.push((*at, typed(Some(ValueType::List(Box::new(elem))))));
                    self.param_uses(value, uses)?;
                }
                _ => {
                    self.param_uses(value, uses)?;
                    self.param_uses(list, uses)?;
                }
            },
            // Every other expression decides nothing about a `$n` itself;
            // a parameter inside it is typed by the comparisons around it.
            other => {
                for child in other.children() {
                    self.param_uses(child, uses)?;
                }
            }
        }
        Ok(())
    }

    /// The type a value compared with a `$n` decides for it: a node
    /// property's declared type, or a column's or variable's type -- but
    /// not `TEXT`, which may stand for an undeclared property (Q8) whose
    /// stored values are of any kind -- when it is a scalar a parameter
    /// binds (`TEXT`, `BIGINT`, `DOUBLE PRECISION`, `BOOLEAN`).
    fn decided(&self, ex: &Ex) -> SqlResult2<Option<ValueType>> {
        let ty = match ex {
            Ex::NodeProperty(slot, field) => self.property_type(*slot, field)?,
            Ex::Slot(slot) => Some(self.schema.slot(*slot).ty.clone()).filter(|ty| *ty != ValueType::Text),
            _ => None,
        };
        Ok(ty.filter(|ty| {
            matches!(ty, ValueType::Text | ValueType::Int | ValueType::Float | ValueType::Bool)
        }))
    }

    /// The type the statement gives each `$n`: entry `i` is `$i+1`, `None`
    /// where no use decides it.
    pub(super) fn param_types(&mut self) -> Vec<Option<ValueType>> {
        let mut uses = std::mem::take(&mut self.uses);
        uses.resize(self.params, ParamUse::Unused);
        uses.into_iter()
            .map(|u| match u {
                ParamUse::Typed { ty, .. } => Some(ty),
                _ => None,
            })
            .collect()
    }
}

/// A stored field's type: its `Kind`, refined by a declared `TIMESTAMPTZ`,
/// `TIMESTAMP` or `DATE` (all three are stored as `Kind::Int`).
fn declared_type(kind: &Kind, time: Option<&str>) -> ValueType {
    match (time, kind) {
        (Some("DATE"), _) => ValueType::Date,
        (Some(_), _) => ValueType::Timestamp,
        (None, Kind::Text) => ValueType::Text,
        (None, Kind::Int) => ValueType::Int,
        (None, Kind::Real) => ValueType::Float,
        (None, Kind::Bool) => ValueType::Bool,
        (None, Kind::Json) => ValueType::Json,
        (None, Kind::Geo | Kind::Point) => ValueType::Geo,
        (None, Kind::Vector(_)) => ValueType::Vector(None),
    }
}

/// The type a cast gives.
pub(super) fn cast_type(to: CastType) -> ValueType {
    match to {
        CastType::Text => ValueType::Text,
        CastType::Int => ValueType::Int,
        CastType::Float => ValueType::Float,
        CastType::Bool => ValueType::Bool,
        CastType::Json => ValueType::Json,
        CastType::Date => ValueType::Date,
        CastType::Timestamp => ValueType::Timestamp,
    }
}

/// The type of one evaluated value, `Unknown` for `NULL`. A vector's
/// dimension, an element's labels and a list's element type are not read:
/// two values of one kind share it.
pub(super) fn value_type(value: &BindingValue) -> ValueType {
    match value {
        BindingValue::Null => ValueType::Unknown,
        BindingValue::Bool(_) => ValueType::Bool,
        BindingValue::Int(_) => ValueType::Int,
        BindingValue::Float(_) => ValueType::Float,
        BindingValue::Text(_) => ValueType::Text,
        BindingValue::Json(_) => ValueType::Json,
        BindingValue::Vector(_) => ValueType::Vector(None),
        BindingValue::Geo(_) => ValueType::Geo,
        BindingValue::Bytes(_) => ValueType::Bytes,
        BindingValue::Node(_) => ValueType::Node(Box::new([])),
        BindingValue::Edge(_) => ValueType::Edge(Box::new([])),
        BindingValue::Path(_) => ValueType::Path,
        BindingValue::List(_) => ValueType::List(Box::new(ValueType::Unknown)),
    }
}

/// The one type two values share: equal types; integers and floats as
/// floats; nodes (or edges) of any labels as nodes (edges) of every label
/// either may have. `Unknown` is any type. The two types that share none
/// otherwise.
pub(super) fn unify(a: ValueType, b: ValueType) -> Result<ValueType, (ValueType, ValueType)> {
    Ok(match (a, b) {
        (ValueType::Unknown, other) | (other, ValueType::Unknown) => other,
        (a, b) if a == b => a,
        (ValueType::Int, ValueType::Float) | (ValueType::Float, ValueType::Int) => ValueType::Float,
        (ValueType::Node(a), ValueType::Node(b)) => ValueType::Node(union(&a, &b)),
        (ValueType::Edge(a), ValueType::Edge(b)) => ValueType::Edge(union(&a, &b)),
        (a, b) => return Err((a, b)),
    })
}

/// [`unify`] over every type of `types`: the one type a list of them holds.
pub(super) fn shared(types: impl IntoIterator<Item = ValueType>) -> Result<ValueType, (ValueType, ValueType)> {
    types.into_iter().try_fold(ValueType::Unknown, unify)
}

/// A list value of `items`, typed [`shared`]: an integer among floats is
/// made a float. The two types that share none otherwise.
pub(super) fn list_of(mut items: Vec<BindingValue>) -> Result<BindingValue, (ValueType, ValueType)> {
    let elem = shared(items.iter().map(value_type))?;
    if elem == ValueType::Float {
        for item in &mut items {
            if let BindingValue::Int(i) = item {
                *item = BindingValue::Float(*i as f64);
            }
        }
    }
    Ok(BindingValue::List(ListRef {
        items: items.into(),
        elem,
    }))
}

/// A bound `$n` the statement gives type `ty`, checked against it: `NULL`
/// is a value of every type; an integer is also a `DOUBLE PRECISION`; a
/// list is read by [`list_param`]; any other kind is refused naming `$n`.
pub(super) fn typed_param(param: &Param, ty: &ValueType, n: usize) -> SqlResult2<BindingValue> {
    let fits = match (ty, param) {
        (ValueType::List(_), _) => return convert::list_param(param, n),
        (_, Param::Null)
        | (ValueType::Text, Param::Text(_))
        | (ValueType::Int, Param::Int(_))
        | (ValueType::Float, Param::Int(_) | Param::Float(_))
        | (ValueType::Bool, Param::Bool(_)) => true,
        _ => false,
    };
    if !fits {
        return Err(SqlError::Parameter(format!(
            "${n} is {}, from where the statement uses it, not {param:?}",
            spelling(ty)
        )));
    }
    Ok(convert::from_param(param))
}

/// Two label (or edge type) sets as one; empty, meaning any, absorbs.
fn union<T: Copy + PartialEq>(a: &[T], b: &[T]) -> Box<[T]> {
    if a.is_empty() || b.is_empty() {
        return Box::new([]);
    }
    let mut out = a.to_vec();
    out.extend(b.iter().filter(|x| !a.contains(x)));
    out.into()
}

/// The type an aggregate gives over an argument of type `arg`, written
/// `written`: the vertical `RETURN` aggregate's and the horizontal fold's.
/// Only `COUNT` and `ARRAY_AGG` take an element.
pub(super) fn aggregate_type(func: AggFunc, arg: &ValueType, written: &str) -> SqlResult2<ValueType> {
    if !matches!(func, AggFunc::Count | AggFunc::ArrayAgg) && convert::element_type(arg).is_some() {
        return Err(SqlError::coded(
            DATATYPE_MISMATCH,
            format!(
                "{}({written}): `{written}` is {}, not a value; aggregate one of its properties",
                func.written(),
                described(arg)
            ),
        ));
    }
    Ok(match func {
        AggFunc::Count => ValueType::Int,
        AggFunc::Sum if *arg == ValueType::Int => ValueType::Int,
        AggFunc::Sum | AggFunc::Avg => ValueType::Float,
        AggFunc::Min | AggFunc::Max => arg.clone(),
        AggFunc::ArrayAgg => ValueType::List(Box::new(arg.clone())),
    })
}

/// A type as an error names it.
pub(super) fn described(ty: &ValueType) -> String {
    match (convert::element_type(ty), ty) {
        (Some(element), _) => element.written().to_owned(),
        (None, ValueType::List(_)) => "a list".to_owned(),
        (None, other) => format!("a value of type {}", spelling(other)),
    }
}

/// The declared SQL spelling of a value of type `ty` (design §6.1); a list
/// is `T[]` of its element's spelling. A node, an edge or a path is never
/// an output column (the binder refuses one), and describes as text.
pub(super) fn spelling(ty: &ValueType) -> &'static str {
    match ty {
        ValueType::Bool => "BOOLEAN",
        ValueType::Int => "BIGINT",
        ValueType::Float => "DOUBLE PRECISION",
        ValueType::Unknown | ValueType::Text => "TEXT",
        ValueType::Json => "JSONB",
        ValueType::Vector(_) => "VECTOR",
        ValueType::Geo => "GEOMETRY",
        ValueType::Bytes => "BYTEA",
        ValueType::Timestamp => "TIMESTAMPTZ",
        ValueType::Date => "DATE",
        ValueType::Node(_) | ValueType::Edge(_) | ValueType::Path => "TEXT",
        ValueType::List(element) => match **element {
            ValueType::Bool => "BOOLEAN[]",
            ValueType::Int => "BIGINT[]",
            ValueType::Float => "DOUBLE PRECISION[]",
            ValueType::Unknown | ValueType::Text => "TEXT[]",
            ValueType::Timestamp => "TIMESTAMPTZ[]",
            ValueType::Date => "DATE[]",
            _ => "JSONB[]",
        },
    }
}

/// What the uses of a statement so far say about one `$n`.
#[derive(Clone, PartialEq)]
pub(super) enum ParamUse {
    Unused,
    /// Read only where any type is taken.
    Undecided,
    /// Of type `ty`, as the use written `at` first decided.
    Typed { ty: ValueType, at: String },
}

impl ParamUse {
    /// This use and `other`, of `$n`: one type, or PostgreSQL's `42P08`
    /// naming the two uses that disagree.
    fn and(self, other: Self, n: usize) -> SqlResult2<Self> {
        Ok(match (self, other) {
            (Self::Unused, next) | (next, Self::Unused) => next,
            (Self::Undecided, next) | (next, Self::Undecided) => next,
            (Self::Typed { ty: a, at: first }, Self::Typed { ty: b, at: second }) => {
                match agree(a, b) {
                    Ok(ty) => Self::Typed { ty, at: first },
                    Err((a, b)) => {
                        return Err(SqlError::coded(
                            AMBIGUOUS_PARAMETER,
                            format!(
                                "inconsistent types deduced for parameter ${n}: {} in `{first}` versus {} in `{second}`; one parameter has one type in the whole statement, the outer SELECT and every stage alike",
                                spelling(&a),
                                spelling(&b)
                            ),
                        ))
                    }
                }
            }
        })
    }
}

/// The one type two uses of a parameter agree on: [`unify`]'s, and for two
/// lists a list of their elements' one type.
fn agree(a: ValueType, b: ValueType) -> Result<ValueType, (ValueType, ValueType)> {
    match (a, b) {
        (ValueType::List(x), ValueType::List(y)) => unify(*x, *y)
            .map(|elem| ValueType::List(Box::new(elem)))
            .map_err(|(x, y)| (ValueType::List(Box::new(x)), ValueType::List(Box::new(y)))),
        (a, b) => unify(a, b),
    }
}
