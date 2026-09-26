//! `Unnest`: `FOR x IN list` (`docs/lang/GQL_PROFILE_DESIGN.md` §3.2). One
//! row per element, in list order: the input row with the element in the
//! `out` slot. A `Null` list and an empty list give no row; a `Null`
//! element is a row. Anything else is an error (the binder refuses it
//! first).
//!
//! Charges: `binding_rows` 1 per row out, and the list's
//! [`held_bytes`](BindingValue::held_bytes) as `list_bytes` for as long as
//! its elements are being handed out -- across pages, so a list that is
//! still being walked is charged again to each page.

use super::super::super::{invalid_query, QueryResult, WorkResource};
use super::super::host::ExprId;
use super::super::value::{BindingRow, BindingValue, SlotId};
use super::{ExecCx, Held, Op, Operator};
use std::sync::Arc;

pub(super) struct Unnest<'q> {
    input: Op<'q>,
    list: ExprId,
    out: SlotId,
    /// The input row being expanded, its list's elements, and the next one.
    current: Option<(BindingRow, Arc<[BindingValue]>, usize)>,
    held: Held,
}

impl<'q> Unnest<'q> {
    pub(super) fn new(input: Op<'q>, list: ExprId, out: SlotId) -> Self {
        Self {
            input,
            list,
            out,
            current: None,
            held: Held::default(),
        }
    }
}

impl<'q> Operator<'q> for Unnest<'q> {
    fn next(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<BindingRow>> {
        self.held.recharge(cx)?;
        loop {
            if let Some((row, items, at)) = &mut self.current {
                if let Some(item) = items.get(*at) {
                    cx.meter.charge(WorkResource::BindingRows, 1)?;
                    let mut out = row.clone();
                    out.slots[usize::from(self.out.0)] = item.clone();
                    *at += 1;
                    return Ok(Some(out));
                }
                self.current = None;
                self.held.release_all(cx);
            }
            let Some(row) = self.input.next(cx)? else {
                return Ok(None);
            };
            let value = cx.eval(self.list, &row)?;
            match &value {
                BindingValue::Null => {}
                BindingValue::List(list) if list.items.is_empty() => {}
                BindingValue::List(list) => {
                    self.held
                        .charge(cx, WorkResource::ListBytes, value.held_bytes())?;
                    self.current = Some((row, Arc::clone(&list.items), 0));
                }
                _ => return Err(invalid_query("FOR takes a list")),
            }
        }
    }
}
