//! The binding schema: what the binder knows about each slot of a working
//! table (`docs/lang/GQL_PROFILE_DESIGN.md` §2.3).
//!
//! A GQL stage produces rows whose slots are positions, `SlotId(0)`,
//! `SlotId(1)`, ... The engine carries the rows and knows only how wide
//! they are. Everything else about a slot -- the variable it holds, the type
//! the binder proved for it, where in the statement it was bound -- is a
//! compile-time fact, and it lives here, in the language layer.
//!
//! Variable names follow the SQL parser's identifier rules: an unquoted name
//! is folded to lower case, a double-quoted one is kept exactly, and two
//! names are the same variable when their folded forms are equal. That is
//! PostgreSQL's rule, and it keeps "same name" an equivalence: `Person`,
//! `PERSON` and `"person"` are one variable, `"Person"` is another.

use super::ast::Expr;
use crate::sqlstate::DUPLICATE_ALIAS;
use crate::{SqlError, SqlResult2};
use sekejap_core::collections::gql::{SlotId, ValueType};
use std::fmt;

/// A variable name, folded the way the SQL parser folds identifiers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Name(Box<str>);

impl Name {
    /// A name written without quotes: compared without case.
    pub(crate) fn unquoted(written: &str) -> Self {
        Self(written.to_ascii_lowercase().into())
    }

    /// A name written in double quotes: compared exactly.
    pub(crate) fn quoted(written: &str) -> Self {
        Self(written.into())
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One path pattern of a `MATCH`, by its position among the comma-separated
/// patterns (`MATCH (a)-[e]->(b), (b)-[f]->(c)` has patterns 0 and 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PatternId(pub(crate) u16);

/// Where a slot's value comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Provenance {
    /// Bound by a node or edge pattern: the pattern, and the element's
    /// position in it counting from 0 (`(a)-[e]->(b)`: `a` 0, `e` 1, `b` 2).
    Element { pattern: PatternId, position: u16 },
    /// A column the `RETURN` of stage `stage` (counting from 1) handed on
    /// through `NEXT`.
    Returned { stage: u16 },
    /// Assigned by a `LET` of stage `stage`.
    Let { stage: u16 },
    /// The element variable of a `FOR` of stage `stage`.
    Unnest { stage: u16 },
    /// A grouping key or an aggregate of stage `stage`'s `RETURN`: a slot of
    /// the row the grouping produces, before the `RETURN` projects it.
    Aggregate { stage: u16 },
    /// Whether an `EXISTS` of stage `stage` gave a row: the hidden `BOOLEAN`
    /// its mark form writes (M5-C).
    Exists { stage: u16 },
    /// A column the `RETURN` of a `CALL` body in stage `stage` added to the
    /// row (M5-D).
    Called { stage: u16 },
}

/// What the binder knows about one slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SlotInfo {
    /// The variable, or `None` for a slot the plan needs but the statement
    /// did not name (an anonymous node between two hops). A slot without a
    /// name cannot be resolved, so the user can never see it.
    pub(crate) name: Option<Name>,
    pub(crate) ty: ValueType,
    pub(crate) provenance: Provenance,
    /// True when a row may hold `Null` here. An element is null only where
    /// an `OPTIONAL MATCH` introduced it (M3-F) or a column carries such
    /// an element on through `LET`, grouping or `NEXT`; a value may be null
    /// unless it is a `COUNT` (or a copy of a value that is never null).
    pub(crate) nullable: bool,
}

impl SlotInfo {
    /// A slot the plan holds but no variable names: an anonymous node, an
    /// implied position, an anonymous edge with a predicate or a COST, a
    /// variable's second occurrence. Never null.
    pub(crate) fn hidden(ty: ValueType, provenance: Provenance) -> Self {
        Self {
            name: None,
            ty,
            provenance,
            nullable: false,
        }
    }

    /// The slot as an error names it: its variable, or "an anonymous
    /// element".
    pub(crate) fn described(&self) -> String {
        self.name
            .as_ref()
            .map_or_else(|| "an anonymous element".to_owned(), |name| format!("`{name}`"))
    }
}

/// The most slots one stage binds: a row's width is a `u16`.
pub(crate) const MAX_SLOTS: usize = u16::MAX as usize;

/// The slots of one stage's working table, in `SlotId` order.
#[derive(Clone, Debug, Default)]
pub(crate) struct BindingSchema {
    slots: Vec<SlotInfo>,
    /// Variables an earlier stage bound and its `RETURN` did not carry
    /// through `NEXT`, each with the stage (from 1) that dropped it: out of
    /// scope here, and named so in the error (§2.3 rule 4).
    dropped: Vec<(Name, u16)>,
    /// Slots an `EXISTS` body bound (M5-C): the rows of the body live in
    /// them while it runs, and after its `}` no name resolves to them.
    local: Vec<SlotId>,
    /// Each `EXISTS` planned in its mark form, as written, with the slot
    /// its answer is written into; an expression reads that slot.
    marks: Vec<(Expr, SlotId)>,
}

impl BindingSchema {
    /// Allocate the next slot. A name already bound in this schema is
    /// refused: a repeated variable is an identity constraint on the slot
    /// it already has, which the binder finds with [`Self::resolve`] before
    /// it allocates. A stage is at most [`MAX_SLOTS`] wide, so its
    /// [`Self::width`] always fits a row width (`u16`).
    pub(crate) fn add(&mut self, slot: SlotInfo) -> SqlResult2<SlotId> {
        if let Some(name) = &slot.name {
            if self.resolve(name).is_some() {
                return Err(SqlError::coded(DUPLICATE_ALIAS, format!(
                    "variable `{name}` is already bound in this stage"
                )));
            }
        }
        if self.slots.len() == MAX_SLOTS {
            return Err(SqlError::unsupported(format!(
                "a GQL stage binds at most {MAX_SLOTS} slots"
            )));
        }
        self.slots.push(slot);
        Ok(SlotId(self.slots.len() as u16 - 1))
    }

    /// The slot a variable is bound to, if it is bound in this stage.
    pub(crate) fn resolve(&self, name: &Name) -> Option<SlotId> {
        // `add` never allocates past `u16::MAX`.
        (0..self.slots.len())
            .map(|at| SlotId(at as u16))
            .find(|slot| self.slot(*slot).name.as_ref() == Some(name) && !self.is_local(*slot))
    }

    /// Put every slot from `from` on out of scope: an `EXISTS` body's own
    /// variables, once its `}` is read.
    pub(crate) fn close_from(&mut self, from: usize) {
        for at in from..self.slots.len() {
            let slot = SlotId(at as u16);
            if !self.is_local(slot) {
                self.local.push(slot);
            }
        }
    }

    /// Put every named slot but `keep` out of scope while a `CALL` body
    /// binds (M5-D): the body sees only its imports. Answers the mark
    /// [`Self::reopen`] restores.
    pub(crate) fn hide_except(&mut self, keep: &[SlotId]) -> usize {
        let mark = self.local.len();
        for at in 0..self.slots.len() {
            let slot = SlotId(at as u16);
            if self.slots[at].name.is_some() && !keep.contains(&slot) && !self.is_local(slot) {
                self.local.push(slot);
            }
        }
        mark
    }

    /// Undo [`Self::hide_except`], together with every slot put out of
    /// scope since (the body's own `EXISTS` bodies).
    pub(crate) fn reopen(&mut self, mark: usize) {
        self.local.truncate(mark);
    }

    /// True when `slot` belongs to an `EXISTS` body that has ended.
    pub(crate) fn is_local(&self, slot: SlotId) -> bool {
        self.local.contains(&slot)
    }

    /// Record that the `EXISTS` written as `exists` answers in `slot`.
    pub(crate) fn mark(&mut self, exists: Expr, slot: SlotId) {
        self.marks.push((exists, slot));
    }

    /// The slot the latest `EXISTS` written as `exists` answers in, if one
    /// was planned in this schema.
    pub(crate) fn marked(&self, exists: &Expr) -> Option<SlotId> {
        self.marks
            .iter()
            .rev()
            .find(|(written, _)| written == exists)
            .map(|(_, slot)| *slot)
    }

    /// What the binder knows about `slot`. Panics on a slot this schema did
    /// not allocate, which is a binder bug, never user input.
    pub(crate) fn slot(&self, slot: SlotId) -> &SlotInfo {
        &self.slots[usize::from(slot.0)]
    }

    /// Mark every slot from `from` on nullable -- the slots an `OPTIONAL
    /// MATCH` allocated -- and answer them.
    pub(crate) fn nullable_from(&mut self, from: usize) -> Vec<SlotId> {
        for slot in &mut self.slots[from..] {
            slot.nullable = true;
        }
        // `add` never allocates past `u16::MAX`.
        (from..self.slots.len()).map(|at| SlotId(at as u16)).collect()
    }

    /// How many slots a row of this stage has.
    pub(crate) fn width(&self) -> usize {
        self.slots.len()
    }

    /// [`Self::width`] as a row width: [`Self::add`] stops at
    /// [`MAX_SLOTS`], which fits.
    pub(crate) fn row_width(&self) -> u16 {
        self.slots.len() as u16
    }

    /// The named variables bound here, in slot order.
    pub(crate) fn names(&self) -> impl Iterator<Item = &Name> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(at, _)| !self.is_local(SlotId(*at as u16)))
            .filter_map(|(_, slot)| slot.name.as_ref())
    }

    /// Record that `name` went out of scope at the `NEXT` after `stage`.
    pub(crate) fn drop_name(&mut self, name: Name, stage: u16) {
        if self.resolve(&name).is_none() && self.dropped_by(&name).is_none() {
            self.dropped.push((name, stage));
        }
    }

    /// Every dropped variable, with the stage that dropped it.
    pub(crate) fn dropped(&self) -> impl Iterator<Item = &(Name, u16)> {
        self.dropped.iter()
    }

    /// The stage whose `RETURN` dropped `name`, if one did.
    pub(crate) fn dropped_by(&self, name: &Name) -> Option<u16> {
        self.dropped
            .iter()
            .find(|(dropped, _)| dropped == name)
            .map(|(_, stage)| *stage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sekejap_core::collections::CollectionId;

    fn node(name: Option<Name>, position: u16) -> SlotInfo {
        SlotInfo {
            name,
            ty: ValueType::Node(Box::new([CollectionId(1)])),
            provenance: Provenance::Element {
                pattern: PatternId(0),
                position,
            },
            nullable: false,
        }
    }

    #[test]
    fn slots_are_allocated_in_order_and_resolve_by_name() {
        let mut schema = BindingSchema::default();
        let a = schema.add(node(Some(Name::unquoted("a")), 0)).unwrap();
        let hidden = schema.add(node(None, 2)).unwrap();
        let b = schema.add(node(Some(Name::unquoted("b")), 4)).unwrap();
        assert_eq!((a, hidden, b), (SlotId(0), SlotId(1), SlotId(2)));
        assert_eq!(schema.width(), 3);
        assert_eq!(schema.resolve(&Name::unquoted("b")), Some(b));
        assert_eq!(schema.resolve(&Name::unquoted("c")), None);
        assert_eq!(
            schema.slot(b).provenance,
            Provenance::Element {
                pattern: PatternId(0),
                position: 4
            }
        );
        assert_eq!(schema.slot(hidden).name, None);
    }

    #[test]
    fn unquoted_names_fold_case_and_quoted_names_are_exact() {
        let mut schema = BindingSchema::default();
        let person = schema.add(node(Some(Name::unquoted("Person")), 0)).unwrap();
        let exact = schema.add(node(Some(Name::quoted("Person")), 2)).unwrap();
        assert_eq!(schema.resolve(&Name::unquoted("PERSON")), Some(person));
        assert_eq!(schema.resolve(&Name::quoted("person")), Some(person));
        assert_eq!(schema.resolve(&Name::quoted("Person")), Some(exact));
        assert_eq!(schema.resolve(&Name::quoted("PERSON")), None);
        assert_eq!(Name::unquoted("Person").to_string(), "person");
        assert_eq!(Name::quoted("Person").to_string(), "Person");
    }

    #[test]
    fn a_name_bound_twice_is_refused_by_name() {
        let mut schema = BindingSchema::default();
        schema.add(node(Some(Name::unquoted("a")), 0)).unwrap();
        let err = schema.add(node(Some(Name::unquoted("A")), 2)).unwrap_err();
        assert!(err.to_string().contains("`a`"), "{err}");
        assert_eq!(schema.width(), 1, "a refused slot is not allocated");
    }

    #[test]
    fn a_stage_wider_than_a_row_width_is_refused() {
        // A row's width is a `u16`, so a stage holds at most 65,535 slots
        // and every later width check is this one.
        let mut schema = BindingSchema::default();
        for position in 0..u16::MAX {
            schema.add(node(None, position)).unwrap();
        }
        assert_eq!(schema.width(), usize::from(u16::MAX));
        let err = schema.add(node(None, 0)).unwrap_err();
        assert!(err.to_string().contains("65535 slots"), "{err}");
        assert_eq!(schema.width(), usize::from(u16::MAX), "a refused slot is not allocated");
    }
}
