//! `Sort`: `ORDER BY` (`docs/lang/GQL_PROFILE_DESIGN.md` §3.2, Q12). The
//! whole input is read before the first row out.
//!
//! Multi-key and STABLE: every row gets its input position, and rows equal
//! on every key come out in that order -- deterministic for a fixed
//! snapshot and plan. `Null` sorts as PostgreSQL sorts it by default: last
//! ascending, first descending. Other values compare under the internal
//! total order (§2.2); the binder admits only keys of one comparable type.
//!
//! Directly under a `Page` with a limit, the sort keeps only the first
//! `keep = offset + limit` rows (top-k): a max-heap of `keep` entries, whose
//! worst entry a better row replaces. Otherwise it buffers every row. When
//! the input is proved ordered by the first key (`monotone_first`), a full
//! heap stops pulling at the first row strictly worse on that key than its
//! worst entry: nothing after it can enter.
//!
//! Charges: each kept entry's row, key and bookkeeping bytes as
//! `sort_bytes`, held until the row is handed out and charged again to
//! every page before that. No spill: past the cap the page is refused.

use super::super::super::{QueryResult, WorkResource};
use super::super::plan::SortKey;
use super::super::value::{BindingRow, BindingValue};
use super::{ExecCx, Held, Op, Operator};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::mem::size_of;

pub(super) struct Sort<'q> {
    /// `None` once the input has been read.
    input: Option<Op<'q>>,
    keys: &'q [SortKey],
    keep: Option<u64>,
    /// The input arrives ordered by the first key, in its direction.
    monotone_first: bool,
    /// The rows still to hand out, LAST row first, so `pop` gives the next.
    sorted: Vec<Entry<'q>>,
    held: Held,
}

/// One kept row: its key values, its input position and what it is
/// charged.
struct Entry<'q> {
    keys: Box<[BindingValue]>,
    seq: u64,
    row: BindingRow,
    bytes: u64,
    spec: &'q [SortKey],
}

/// One key's comparison: `Null` after every value, then the direction.
fn key_order(key: &SortKey, a: &BindingValue, b: &BindingValue) -> Ordering {
    let order = match (a, b) {
        (BindingValue::Null, BindingValue::Null) => Ordering::Equal,
        (BindingValue::Null, _) => Ordering::Greater,
        (_, BindingValue::Null) => Ordering::Less,
        _ => a.cmp(b),
    };
    if key.descending {
        order.reverse()
    } else {
        order
    }
}

/// The sort order: the keys in turn, then the input position.
impl Ord for Entry<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.spec
            .iter()
            .zip(self.keys.iter().zip(other.keys.iter()))
            .map(|(key, (a, b))| key_order(key, a, b))
            .find(|order| *order != Ordering::Equal)
            .unwrap_or_else(|| self.seq.cmp(&other.seq))
    }
}

impl PartialOrd for Entry<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Entry<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Entry<'_> {}

impl<'q> Sort<'q> {
    pub(super) fn new(input: Op<'q>, keys: &'q [SortKey], keep: Option<u64>, monotone_first: bool) -> Self {
        Self {
            input: Some(input),
            keys,
            keep,
            monotone_first,
            sorted: Vec::new(),
            held: Held::default(),
        }
    }

    /// Read the whole input, keeping what the sort (or its top-k) needs.
    fn fill(&mut self, mut input: Op<'q>, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<()> {
        let mut all = Vec::new();
        let mut best = BinaryHeap::new();
        let mut seq = 0;
        while let Some(row) = input.next(cx)? {
            let keys = self
                .keys
                .iter()
                .map(|key| cx.eval(key.expr, &row))
                .collect::<QueryResult<Box<[BindingValue]>>>()?;
            let bytes = size_of::<Entry>() as u64
                + row.held_bytes()
                + keys.iter().map(BindingValue::held_bytes).sum::<u64>();
            let entry = Entry {
                keys,
                seq,
                row,
                bytes,
                spec: self.keys,
            };
            seq += 1;
            match self.keep {
                None => {
                    self.held.charge(cx, WorkResource::SortBytes, bytes)?;
                    all.push(entry);
                }
                Some(keep) if (best.len() as u64) < keep => {
                    self.held.charge(cx, WorkResource::SortBytes, bytes)?;
                    best.push(entry);
                }
                Some(_) => {
                    // Full, over an input ordered by the first key: a row
                    // strictly worse on it than the worst kept one ends the
                    // read, since every later row is at least as bad.
                    if self.monotone_first
                        && best.peek().is_some_and(|worst: &Entry<'_>| {
                            key_order(&self.keys[0], &entry.keys[0], &worst.keys[0]) == Ordering::Greater
                        })
                    {
                        break;
                    }
                    // Full: a row enters only if it beats the worst kept one.
                    if best.peek().is_some_and(|worst| entry < *worst) {
                        self.held.charge(cx, WorkResource::SortBytes, bytes)?;
                        best.push(entry);
                        if let Some(out) = best.pop() {
                            self.held.release(cx, WorkResource::SortBytes, out.bytes);
                        }
                    }
                }
            }
        }
        let mut sorted = match self.keep {
            None => {
                all.sort_unstable();
                all
            }
            Some(_) => best.into_sorted_vec(),
        };
        sorted.reverse();
        self.sorted = sorted;
        Ok(())
    }
}

impl<'q> Operator<'q> for Sort<'q> {
    fn next(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<BindingRow>> {
        self.held.recharge(cx)?;
        if let Some(input) = self.input.take() {
            self.fill(input, cx)?;
        }
        let Some(entry) = self.sorted.pop() else {
            return Ok(None);
        };
        self.held.release(cx, WorkResource::SortBytes, entry.bytes);
        Ok(Some(entry.row))
    }
}
