//! `Filter`: the input rows whose predicate is `True`
//! (`docs/lang/GQL_PROFILE_DESIGN.md` §2.5, §3.2). `Unknown` -- a comparison
//! with `Null` -- drops the row, as `False` does. It charges whatever the
//! evaluation reads.

use super::super::super::QueryResult;
use super::super::host::ExprId;
use super::super::value::BindingRow;
use super::{ExecCx, Op, Operator};

pub(super) struct Filter<'q> {
    input: Op<'q>,
    predicate: ExprId,
}

impl<'q> Filter<'q> {
    pub(super) fn new(input: Op<'q>, predicate: ExprId) -> Self {
        Self { input, predicate }
    }
}

impl<'q> Operator<'q> for Filter<'q> {
    fn next(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<BindingRow>> {
        while let Some(row) = self.input.next(cx)? {
            if cx.holds(self.predicate, &row)? {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }
}
