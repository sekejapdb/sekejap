//! `Distinct`: each whole row once (`docs/lang/GQL_PROFILE_DESIGN.md` §3.2).
//! It streams -- a row is handed out the first time it is seen -- but keeps
//! every row seen so far, in a hash set whose equality is the grouping
//! equality of §2.2: identity for nodes, edges and paths, value for
//! scalars, `Null` equal to `Null`.
//!
//! Charges: each kept row's [`held_bytes`](BindingRow::held_bytes) as
//! `sort_bytes`, held until the input ends and charged again to every page
//! before that.

use super::super::super::{QueryResult, WorkResource};
use super::super::value::{BindingRow, BindingValue};
use super::{ExecCx, Held, Op, Operator};
use std::collections::HashSet;

pub(super) struct Distinct<'q> {
    input: Op<'q>,
    seen: HashSet<Box<[BindingValue]>>,
    held: Held,
}

impl<'q> Distinct<'q> {
    pub(super) fn new(input: Op<'q>) -> Self {
        Self {
            input,
            seen: HashSet::new(),
            held: Held::default(),
        }
    }
}

impl<'q> Operator<'q> for Distinct<'q> {
    fn next(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<BindingRow>> {
        self.held.recharge(cx)?;
        while let Some(row) = self.input.next(cx)? {
            if self.seen.contains(&row.slots) {
                continue;
            }
            self.held
                .charge(cx, WorkResource::SortBytes, row.held_bytes())?;
            self.seen.insert(row.slots.clone());
            return Ok(Some(row));
        }
        self.seen = HashSet::new();
        self.held.release_all(cx);
        Ok(None)
    }
}
