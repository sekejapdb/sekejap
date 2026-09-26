//! `OptionalApply` and its `Argument` leaf: `OPTIONAL MATCH`
//! (`docs/lang/GQL_PROFILE_DESIGN.md` §3.2, owner answer Q3).
//!
//! A LEFT OUTER JOIN, one input row at a time. For each input row the inner
//! side -- the optional pattern's seeds, hops, path searches and filters,
//! its `WHERE` included -- runs from that row, and every row it gives is an
//! output row. When it gives none, whether it found nothing or its
//! predicates rejected all it found, the input row comes out ONCE, with the
//! slots the pattern introduces set to `Null`. So an input row is never
//! dropped, and the optional `WHERE` decides whether a match exists: it
//! never filters the `Null` row (brief §7 rule 6).
//!
//! The inner side reads its input row through an [`Argument`] leaf: the
//! operator hands the row over in [`ExecCx::argument`] and pulls the inner
//! side, whose first pull takes it. The inner side is built from STREAMING
//! operators only (`build` refuses the others inside it), and each of them
//! pulls its input again only once it has given out everything the last
//! input row led to -- so once the inner side has answered `None` it holds
//! nothing of the row before, and the next row starts it afresh.
//!
//! Charges: whatever the inner side charges, and `binding_rows` 1 per
//! `Null` row.

use super::super::super::{QueryResult, WorkResource};
use super::super::value::{BindingRow, BindingValue, SlotId};
use super::{ExecCx, Op, Operator};

pub(super) struct OptionalApply<'q> {
    input: Op<'q>,
    inner: Op<'q>,
    introduced: &'q [SlotId],
    /// The input row whose inner side is running, and whether it has
    /// given a row yet.
    current: Option<(BindingRow, bool)>,
}

impl<'q> OptionalApply<'q> {
    pub(super) fn new(input: Op<'q>, inner: Op<'q>, introduced: &'q [SlotId]) -> Self {
        Self {
            input,
            inner,
            introduced,
            current: None,
        }
    }
}

impl<'q> Operator<'q> for OptionalApply<'q> {
    fn next(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<BindingRow>> {
        loop {
            if let Some((_, matched)) = &mut self.current {
                if let Some(row) = self.inner.next(cx)? {
                    *matched = true;
                    return Ok(Some(row));
                }
                let (mut row, matched) = self.current.take().expect("checked above");
                if !matched {
                    cx.meter.charge(WorkResource::BindingRows, 1)?;
                    for slot in self.introduced {
                        row.slots[usize::from(slot.0)] = BindingValue::Null;
                    }
                    return Ok(Some(row));
                }
            }
            let Some(row) = self.input.next(cx)? else {
                return Ok(None);
            };
            cx.argument = Some(row.clone());
            self.current = Some((row, false));
        }
    }
}

/// The inner side's leaf: the input row its [`OptionalApply`] handed over,
/// once.
pub(super) struct Argument;

impl<'q> Operator<'q> for Argument {
    fn next(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<BindingRow>> {
        Ok(cx.argument.take())
    }
}
