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
//!   for the vertical and the horizontal aggregates alike.

use super::ast::{AggFunc, ArithOp, CastType, Func};
use super::convert;
use super::elements;
use super::expr::Ex;
use super::plan::{show, Planner};
use crate::functions::is_time_type;
use crate::sqlstate::DATATYPE_MISMATCH;
use crate::{SqlError, SqlResult2};
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
        if matches!(ex, Ex::Graph(..) | Ex::List(_) | Ex::Fold(_)) {
            self.slot_type(ex)?;
        }
        for child in ex.children() {
            self.typed(child)?;
        }
        Ok(())
    }

    /// Record what `ex` says about the type of each `$n` it reads: `$n`
    /// against a property whose type is declared alike in every label
    /// collection takes that type; any other use leaves it undecided, and
    /// so do two uses that disagree. `ex` is bound against `self.schema`.
    ///
    /// Every lowered expression passes here, so this is also where the
    /// kinds M4-D decides at compile are checked ([`Planner::typed`]).
    pub(super) fn note_params(&mut self, ex: &Ex) -> SqlResult2<()> {
        self.typed(ex)?;
        let mut uses = std::mem::take(&mut self.uses);
        uses.resize(self.params.max(uses.len()), ParamUse::Unused);
        let noted = self.param_uses(ex, &mut uses);
        self.uses = uses;
        noted
    }

    /// `$at+1` is an `OFFSET` or `LIMIT` count: a `BIGINT`.
    pub(super) fn note_count(&mut self, at: usize) {
        self.uses.resize(self.params.max(self.uses.len()), ParamUse::Unused);
        self.uses[at] = take_use(&mut self.uses[at]).and(ParamUse::Typed(ValueType::Int));
        if !self.counts.contains(&at) {
            self.counts.push(at);
        }
    }

    fn param_uses(&self, ex: &Ex, uses: &mut [ParamUse]) -> SqlResult2<()> {
        match ex {
            Ex::Param(at) => uses[*at] = take_use(&mut uses[*at]).and(ParamUse::Undecided),
            Ex::Compare(_, left, right) => match (&**left, &**right) {
                (Ex::Param(at), Ex::NodeProperty(slot, field))
                | (Ex::NodeProperty(slot, field), Ex::Param(at)) => {
                    let typed = match self.property_type(*slot, field)? {
                        Some(
                            ty @ (ValueType::Text | ValueType::Int | ValueType::Float | ValueType::Bool),
                        ) => ParamUse::Typed(ty),
                        _ => ParamUse::Undecided,
                    };
                    uses[*at] = take_use(&mut uses[*at]).and(typed);
                }
                _ => {
                    self.param_uses(left, uses)?;
                    self.param_uses(right, uses)?;
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

    /// The SQL type each `$n` is compared as: entry `i` is `$i+1`, `None`
    /// where no comparison decides it or two disagree.
    pub(super) fn param_types(&mut self) -> Vec<Option<&'static str>> {
        let mut uses = std::mem::take(&mut self.uses);
        uses.resize(self.params, ParamUse::Unused);
        uses.into_iter()
            .map(|u| match u {
                ParamUse::Typed(ty) => Some(spelling(&ty)),
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

/// What the comparisons of a statement say about one `$n`.
#[derive(Clone, PartialEq)]
pub(super) enum ParamUse {
    Unused,
    Typed(ValueType),
    Undecided,
}

/// A `$n`'s use so far, taken out to be combined with the next.
fn take_use(slot: &mut ParamUse) -> ParamUse {
    std::mem::replace(slot, ParamUse::Unused)
}

impl ParamUse {
    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unused, next) | (next, Self::Unused) => next,
            (Self::Typed(a), Self::Typed(b)) if a == b => Self::Typed(a),
            _ => Self::Undecided,
        }
    }
}
