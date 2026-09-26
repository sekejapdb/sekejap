//! `Union`, `Buffered` and its `Replay` leaf: `UNION [ALL | DISTINCT]`
//! (`docs/lang/GQL_PROFILE_DESIGN_M5_M7.md` §2.5, Q25).
//!
//! * `Union` gives the rows of each branch in turn, branch order then row
//!   order, and drops a branch once it has ended. `UNION` without `ALL` is
//!   the existing `Distinct` over it. Charges nothing itself.
//! * In a FIRST part each branch starts from its own `Unit`, so nothing is
//!   buffered. After `NEXT` every branch must see the WHOLE incoming table
//!   -- a branch may aggregate it, so running the branches per input row
//!   would change their answers. `Buffered` reads its input once into a
//!   table, then pulls the union, lending it the table through
//!   [`ExecCx::replay`] for the length of each pull; each branch's `Replay`
//!   leaf hands the table's rows out from the start.
//!
//! Charges: `Buffered` holds each buffered row's
//! [`held_bytes`](BindingRow::held_bytes) as `sort_bytes`, charged again to
//! every page until the last branch has replayed it, then released. No
//! spill: past the cap the page is refused.

use super::super::super::{invalid_query, QueryResult, WorkResource};
use super::super::value::BindingRow;
use super::{ExecCx, Held, Op, Operator};
use std::collections::VecDeque;
use std::mem;

pub(super) struct Union<'q> {
    /// The branches not yet ended, the running one first.
    branches: VecDeque<Op<'q>>,
}

impl<'q> Union<'q> {
    pub(super) fn new(branches: Vec<Op<'q>>) -> Self {
        Self {
            branches: branches.into(),
        }
    }
}

impl<'q> Operator<'q> for Union<'q> {
    fn next(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<BindingRow>> {
        while let Some(branch) = self.branches.front_mut() {
            if let Some(row) = branch.next(cx)? {
                return Ok(Some(row));
            }
            self.branches.pop_front();
        }
        Ok(None)
    }
}

pub(super) struct Buffered<'q> {
    /// `None` once the incoming table has been read.
    input: Option<Op<'q>>,
    union: Union<'q>,
    /// The incoming table, between two pulls of the union.
    table: Vec<BindingRow>,
    held: Held,
}

impl<'q> Buffered<'q> {
    pub(super) fn new(input: Op<'q>, union: Union<'q>) -> Self {
        Self {
            input: Some(input),
            union,
            table: Vec::new(),
            held: Held::default(),
        }
    }
}

impl<'q> Operator<'q> for Buffered<'q> {
    fn next(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<BindingRow>> {
        self.held.recharge(cx)?;
        if let Some(mut input) = self.input.take() {
            while let Some(row) = input.next(cx)? {
                self.held
                    .charge(cx, WorkResource::SortBytes, row.held_bytes())?;
                self.table.push(row);
            }
        }
        cx.replay = Some(mem::take(&mut self.table));
        let row = self.union.next(cx);
        self.table = cx.replay.take().unwrap_or_default();
        let row = row?;
        if row.is_none() {
            self.table = Vec::new();
            self.held.release_all(cx);
        }
        Ok(row)
    }
}

/// A buffered branch's leaf: the incoming table's rows, from the start.
pub(super) struct Replay {
    at: usize,
}

impl Replay {
    pub(super) fn new() -> Self {
        Self { at: 0 }
    }
}

impl<'q> Operator<'q> for Replay {
    fn next(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<BindingRow>> {
        let Some(table) = &cx.replay else {
            return Err(invalid_query(
                "a Replay ran with no incoming table lent to it",
            ));
        };
        let row = table.get(self.at).cloned();
        if row.is_some() {
            self.at += 1;
        }
        Ok(row)
    }
}
