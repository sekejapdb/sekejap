//! The operators that make a row rather than find one
//! (`docs/lang/GQL_PROFILE_DESIGN.md` §3.2, §3.3):
//!
//! * `Unit` -- the one row of `Null`s a first stage starts from;
//! * `Let` -- values evaluated against the input row, then stored in it;
//! * `Project` -- a new row of the next stage's width: `RETURN`, and the
//!   `NEXT` boundary, which nothing else of the input row crosses.
//!
//! None of them reads anything itself; they charge whatever the
//! evaluations read.

use super::super::super::QueryResult;
use super::super::host::ExprId;
use super::super::value::{BindingRow, BindingValue, SlotId};
use super::{ExecCx, Op, Operator};

fn nulls(width: u16) -> BindingRow {
    BindingRow {
        slots: vec![BindingValue::Null; usize::from(width)].into_boxed_slice(),
    }
}

pub(super) struct Unit {
    width: u16,
    given: bool,
}

impl Unit {
    pub(super) fn new(width: u16) -> Self {
        Self {
            width,
            given: false,
        }
    }
}

impl<'q> Operator<'q> for Unit {
    fn next(&mut self, _: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<BindingRow>> {
        if self.given {
            return Ok(None);
        }
        self.given = true;
        Ok(Some(nulls(self.width)))
    }
}

pub(super) struct Let<'q> {
    input: Op<'q>,
    assign: &'q [(SlotId, ExprId)],
}

impl<'q> Let<'q> {
    pub(super) fn new(input: Op<'q>, assign: &'q [(SlotId, ExprId)]) -> Self {
        Self { input, assign }
    }
}

impl<'q> Operator<'q> for Let<'q> {
    fn next(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<BindingRow>> {
        let Some(mut row) = self.input.next(cx)? else {
            return Ok(None);
        };
        // Every value against the row as it came in, so no assignment sees
        // another; then all of them stored.
        let values = self
            .assign
            .iter()
            .map(|(_, expr)| cx.eval(*expr, &row))
            .collect::<QueryResult<Vec<_>>>()?;
        for ((slot, _), value) in self.assign.iter().zip(values) {
            row.slots[usize::from(slot.0)] = value;
        }
        Ok(Some(row))
    }
}

pub(super) struct Project<'q> {
    input: Op<'q>,
    cols: &'q [ExprId],
    width: u16,
}

impl<'q> Project<'q> {
    pub(super) fn new(input: Op<'q>, cols: &'q [ExprId], width: u16) -> Self {
        Self { input, cols, width }
    }
}

impl<'q> Operator<'q> for Project<'q> {
    fn next(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<BindingRow>> {
        let Some(row) = self.input.next(cx)? else {
            return Ok(None);
        };
        let mut out = nulls(self.width);
        for (slot, expr) in out.slots.iter_mut().zip(self.cols) {
            *slot = cx.eval(*expr, &row)?;
        }
        Ok(Some(out))
    }
}
