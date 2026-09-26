//! `Page`: `OFFSET` / `LIMIT` (`docs/lang/GQL_PROFILE_DESIGN.md` §3.2, Q13).
//! The counts were read and range-checked when the execution opened.
//!
//! It skips `offset` input rows, then hands out at most `limit`. `LIMIT 0`
//! pulls nothing. Once the limit is reached, or the input ends, the input is
//! DROPPED: an unsorted pipeline does no work past the limit, and whatever a
//! blocking operator below still held is freed rather than kept for pages
//! that will never pull it. (The page in which that happens still reports
//! it as held; the pages after it do not.)
//!
//! Charges nothing itself: skipped rows were charged by the operators that
//! produced them, so the cost of an `OFFSET` is the rows it skips.

use super::super::super::QueryResult;
use super::super::value::BindingRow;
use super::{ExecCx, Op, Operator};

pub(super) struct Page<'q> {
    /// `None` once the input has ended or the limit is reached.
    input: Option<Op<'q>>,
    skip: u64,
    left: Option<u64>,
}

impl<'q> Page<'q> {
    pub(super) fn new(input: Op<'q>, offset: u64, limit: Option<u64>) -> Self {
        Self {
            input: Some(input),
            skip: offset,
            left: limit,
        }
    }
}

impl<'q> Operator<'q> for Page<'q> {
    fn next(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<BindingRow>> {
        if self.left == Some(0) {
            self.input = None;
        }
        let Some(input) = &mut self.input else {
            return Ok(None);
        };
        while self.skip > 0 {
            if input.next(cx)?.is_none() {
                self.input = None;
                return Ok(None);
            }
            self.skip -= 1;
        }
        let Some(row) = input.next(cx)? else {
            self.input = None;
            return Ok(None);
        };
        if let Some(left) = &mut self.left {
            *left -= 1;
        }
        Ok(Some(row))
    }
}
