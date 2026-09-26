//! `CallApply`: `CALL (imports) { ... }`
//! (`docs/lang/GQL_PROFILE_DESIGN_M5_M7.md` §2.4).
//!
//! A LATERAL INNER JOIN, one input row at a time. For each input row a
//! fresh instance of the body -- its leaf an `Argument` that hands it the
//! row -- runs to its end, and each row it returns extends the input row:
//! its columns are written into the output slots of a copy of the input
//! row. An input row whose body returns nothing is DROPPED; a body that
//! aggregates with no grouping key returns one row over empty input, so it
//! keeps the row (with `COUNT` 0). The left-outer form, `OPTIONAL CALL`, is
//! not built (Q21).
//!
//! The body may hold blocking operators -- per-input grouping and top-k are
//! the point of `CALL` -- whose state cannot restart in place, so it is
//! built afresh for each input row ([`Fresh`]), and what it held is given
//! back when that row's tree is dropped. The body's rows may span pages:
//! its blocking operators charge what they hold again to each page, as
//! everywhere else.
//!
//! Charges: whatever the body charges, and `binding_rows` 1 per row out.

use super::super::super::{QueryResult, WorkResource};
use super::super::value::{BindingRow, SlotId};
use super::{ExecCx, Fresh, Op, Operator};

pub(super) struct CallApply<'q> {
    input: Op<'q>,
    inner: Fresh<'q>,
    outputs: &'q [SlotId],
    /// The input row whose body is running.
    current: Option<BindingRow>,
}

impl<'q> CallApply<'q> {
    pub(super) fn new(input: Op<'q>, inner: Fresh<'q>, outputs: &'q [SlotId]) -> Self {
        Self {
            input,
            inner,
            outputs,
            current: None,
        }
    }
}

impl<'q> Operator<'q> for CallApply<'q> {
    fn next(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<BindingRow>> {
        loop {
            if let Some(row) = &self.current {
                if let Some(returned) = self.inner.next(cx)? {
                    cx.meter.charge(WorkResource::BindingRows, 1)?;
                    let mut out = row.clone();
                    for (slot, value) in self.outputs.iter().zip(returned.slots.into_vec()) {
                        out.slots[usize::from(slot.0)] = value;
                    }
                    return Ok(Some(out));
                }
                self.inner.stop(cx);
                self.current = None;
            }
            let Some(row) = self.input.next(cx)? else {
                return Ok(None);
            };
            self.inner.start(cx, &row)?;
            self.current = Some(row);
        }
    }
}
