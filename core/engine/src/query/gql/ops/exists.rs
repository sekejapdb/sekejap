//! `ExistsApply`: `EXISTS { ... }` and `NOT EXISTS { ... }`
//! (`docs/lang/GQL_PROFILE_DESIGN_M5_M7.md` §2.3).
//!
//! A SEMI-JOIN (or an ANTI-JOIN), one input row at a time. For each input
//! row the inner side -- its leaf an `Argument` that hands it the row --
//! runs until its FIRST row, and is then dropped: whether a first row
//! exists is the whole answer. So the operator never multiplies a row: at
//! most one row out per row in (brief §7 rule 7).
//!
//! * The FILTER form keeps the row when the answer is what it asks for
//!   (`EXISTS`: a row; `NOT EXISTS`: none) and drops it otherwise.
//! * The MARK form keeps every row, with the answer written into its slot
//!   as a `Bool`, for an `EXISTS` inside `OR`, `CASE`, `LET` or `RETURN`.
//!
//! The inner side is stopped part way, so it may still hold a refill or a
//! search frontier: it is built afresh for each input row ([`Fresh`]), and
//! what it held is given back when it is dropped. Its hops walk their first
//! refill one posting at a time, so an existence test over a node with many
//! edges reads one posting past the seek (`expand.rs`).
//!
//! Charges: what the inner side charges up to its first row; nothing of its
//! own -- like `Filter`, it passes input rows on.

use super::super::super::QueryResult;
use super::super::plan::ExistsMode;
use super::super::value::{BindingRow, BindingValue};
use super::{ExecCx, Fresh, Op, Operator};

pub(super) struct ExistsApply<'q> {
    input: Op<'q>,
    inner: Fresh<'q>,
    mode: ExistsMode,
}

impl<'q> ExistsApply<'q> {
    pub(super) fn new(input: Op<'q>, inner: Fresh<'q>, mode: ExistsMode) -> Self {
        Self { input, inner, mode }
    }
}

impl<'q> Operator<'q> for ExistsApply<'q> {
    fn next(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<BindingRow>> {
        while let Some(mut row) = self.input.next(cx)? {
            self.inner.start(cx, &row)?;
            let found = self.inner.next(cx)?.is_some();
            self.inner.stop(cx);
            match self.mode {
                ExistsMode::Filter { negated } => {
                    if found != negated {
                        return Ok(Some(row));
                    }
                }
                ExistsMode::Mark { slot } => {
                    row.slots[usize::from(slot.0)] = BindingValue::Bool(found);
                    return Ok(Some(row));
                }
            }
        }
        Ok(None)
    }
}
