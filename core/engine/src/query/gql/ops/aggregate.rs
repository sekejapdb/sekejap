//! `Aggregate`: grouping and the vertical accumulators
//! (`docs/lang/GQL_PROFILE_DESIGN.md` §2.2, §3.2, Q9). The whole input is
//! folded before the first row out.
//!
//! Groups are keyed by the grouping values under the grouping equality of
//! §2.2 -- identity for nodes, edges and paths, value for scalars, `Null`
//! with `Null` -- and come out in the order they were first seen. With no
//! grouping key there is exactly one group, made before the input is read,
//! so an empty input gives one row: `COUNT` 0 and every other accumulator
//! `Null` (`ARRAY_AGG` included, Q9). With a key an empty input gives none.
//!
//! Every argument is evaluated by the host; this file only folds values.
//!
//! Charges, held until the group is handed out and charged again to every
//! page before that:
//!
//! * `groups` 1 per group (the existing resource and its cap);
//! * `sort_bytes`: the key's bytes and a fixed accumulator size per group,
//!   each `COUNT(DISTINCT)` value kept, and the value a `MIN` / `MAX` holds;
//! * `list_bytes`: each `ARRAY_AGG` element;
//! * `binding_rows` 1 per row out.

use super::super::super::{invalid_query, QueryResult, WorkResource};
use super::super::host::{ExprId, GqlHost};
use super::super::plan::AggSpec;
use super::super::value::{BindingRow, BindingValue, ListRef};
use super::{ExecCx, Held, Op, Operator};
use std::collections::{HashMap, HashSet};
use std::mem::size_of;

pub(super) struct Aggregate<'q> {
    /// `None` once the input has been folded.
    input: Option<Op<'q>>,
    keys: &'q [ExprId],
    aggs: &'q [AggSpec],
    width: u16,
    /// The groups still to hand out, LAST first, so `pop` gives the next.
    out: Vec<(Box<[BindingValue]>, Group)>,
    held: Held,
}

/// One group's accumulators and what they are charged.
struct Group {
    accs: Vec<Acc>,
    sort_bytes: u64,
    list_bytes: u64,
}

enum Acc {
    Rows(u64),
    Count(u64),
    Distinct(HashSet<BindingValue>),
    /// `SUM` and `AVG`: integers exactly, floats apart, and how many.
    Sum {
        int: i128,
        float: f64,
        floats: bool,
        n: u64,
    },
    Min(Option<BindingValue>),
    Max(Option<BindingValue>),
    Array(Vec<BindingValue>),
}

impl Acc {
    fn new(spec: &AggSpec) -> Self {
        match spec {
            AggSpec::CountRows => Acc::Rows(0),
            AggSpec::Count { distinct: false, .. } => Acc::Count(0),
            AggSpec::Count { distinct: true, .. } => Acc::Distinct(HashSet::new()),
            AggSpec::Sum(_) | AggSpec::Avg(_) => Acc::Sum {
                int: 0,
                float: 0.0,
                floats: false,
                n: 0,
            },
            AggSpec::Min(_) => Acc::Min(None),
            AggSpec::Max(_) => Acc::Max(None),
            AggSpec::ArrayAgg { .. } => Acc::Array(Vec::new()),
        }
    }

    /// The aggregate's value; `host` names an integer `SUM` out of range.
    fn finish(self, spec: &AggSpec, host: &dyn GqlHost) -> QueryResult<BindingValue> {
        let count = |n: usize| BindingValue::Int(i64::try_from(n).unwrap_or(i64::MAX));
        Ok(match (self, spec) {
            (Acc::Rows(n) | Acc::Count(n), _) => count(n as usize),
            (Acc::Distinct(set), _) => count(set.len()),
            (Acc::Sum { n: 0, .. }, _) => BindingValue::Null,
            (Acc::Sum { int, float, n, .. }, AggSpec::Avg(_)) => {
                BindingValue::Float((int as f64 + float) / n as f64)
            }
            (Acc::Sum { int, float, floats: true, .. }, _) => {
                BindingValue::Float(int as f64 + float)
            }
            (Acc::Sum { int, .. }, _) => BindingValue::Int(
                i64::try_from(int).map_err(|_| host.out_of_range())?,
            ),
            (Acc::Min(value) | Acc::Max(value), _) => value.unwrap_or(BindingValue::Null),
            (Acc::Array(items), _) if items.is_empty() => BindingValue::Null,
            (Acc::Array(items), AggSpec::ArrayAgg { elem, .. }) => BindingValue::List(ListRef {
                items: items.into(),
                elem: elem.clone(),
            }),
            (Acc::Array(_), _) => unreachable!("an array accumulator is made for ARRAY_AGG"),
        })
    }
}

/// The argument an accumulator reads, if it reads one.
fn arg(spec: &AggSpec) -> Option<ExprId> {
    match spec {
        AggSpec::CountRows => None,
        AggSpec::Count { arg, .. } | AggSpec::ArrayAgg { arg, .. } => Some(*arg),
        AggSpec::Sum(arg) | AggSpec::Avg(arg) | AggSpec::Min(arg) | AggSpec::Max(arg) => {
            Some(*arg)
        }
    }
}

impl<'q> Aggregate<'q> {
    pub(super) fn new(
        input: Op<'q>,
        keys: &'q [ExprId],
        aggs: &'q [AggSpec],
        width: u16,
    ) -> Self {
        Self {
            input: Some(input),
            keys,
            aggs,
            width,
            out: Vec::new(),
            held: Held::default(),
        }
    }

    /// A new group for `key`, charged.
    fn open_group(
        &mut self,
        cx: &mut ExecCx<'q, '_, '_>,
        key: &[BindingValue],
    ) -> QueryResult<Group> {
        let bytes = key.iter().map(BindingValue::held_bytes).sum::<u64>()
            + (self.aggs.len() * size_of::<Acc>()) as u64;
        self.held.charge(cx, WorkResource::Groups, 1)?;
        self.held.charge(cx, WorkResource::SortBytes, bytes)?;
        Ok(Group {
            accs: self.aggs.iter().map(Acc::new).collect(),
            sort_bytes: bytes,
            list_bytes: 0,
        })
    }

    /// Fold `row` into `group`.
    fn add(
        &mut self,
        cx: &mut ExecCx<'q, '_, '_>,
        group: &mut Group,
        row: &BindingRow,
    ) -> QueryResult<()> {
        for (acc, spec) in group.accs.iter_mut().zip(self.aggs) {
            let value = match arg(spec) {
                Some(expr) => cx.eval(expr, row)?,
                None => BindingValue::Null,
            };
            let null = matches!(value, BindingValue::Null);
            match acc {
                Acc::Rows(n) => *n += 1,
                Acc::Count(n) => *n += u64::from(!null),
                Acc::Distinct(set) => {
                    if !null && !set.contains(&value) {
                        let bytes = value.held_bytes();
                        self.held.charge(cx, WorkResource::SortBytes, bytes)?;
                        group.sort_bytes += bytes;
                        set.insert(value);
                    }
                }
                Acc::Sum {
                    int,
                    float,
                    floats,
                    n,
                } => match value {
                    BindingValue::Null => {}
                    BindingValue::Int(i) => {
                        *int += i128::from(i);
                        *n += 1;
                    }
                    BindingValue::Float(f) => {
                        *float += f;
                        *floats = true;
                        *n += 1;
                    }
                    _ => return Err(invalid_query("SUM and AVG take numbers")),
                },
                Acc::Min(kept) | Acc::Max(kept) => {
                    let min = matches!(spec, AggSpec::Min(_));
                    let better = match kept {
                        _ if null => false,
                        None => true,
                        Some(old) => (value < *old) == min && value != *old,
                    };
                    if better {
                        let bytes = value.held_bytes();
                        self.held.charge(cx, WorkResource::SortBytes, bytes)?;
                        group.sort_bytes += bytes;
                        if let Some(old) = kept.replace(value) {
                            let old = old.held_bytes();
                            self.held.release(cx, WorkResource::SortBytes, old);
                            group.sort_bytes -= old;
                        }
                    }
                }
                Acc::Array(items) => {
                    let bytes = value.held_bytes();
                    self.held.charge(cx, WorkResource::ListBytes, bytes)?;
                    group.list_bytes += bytes;
                    items.push(value);
                }
            }
        }
        Ok(())
    }

    /// Read the whole input into groups, in first-seen order.
    fn fold(&mut self, mut input: Op<'q>, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<()> {
        let mut index: HashMap<Box<[BindingValue]>, usize> = HashMap::new();
        let mut groups: Vec<Group> = Vec::new();
        if self.keys.is_empty() {
            groups.push(self.open_group(cx, &[])?);
            index.insert(Box::new([]), 0);
        }
        while let Some(row) = input.next(cx)? {
            let key = self
                .keys
                .iter()
                .map(|expr| cx.eval(*expr, &row))
                .collect::<QueryResult<Box<[BindingValue]>>>()?;
            let at = match index.get(&key) {
                Some(at) => *at,
                None => {
                    groups.push(self.open_group(cx, &key)?);
                    index.insert(key, groups.len() - 1);
                    groups.len() - 1
                }
            };
            self.add(cx, &mut groups[at], &row)?;
        }
        let mut keys: Vec<(usize, Box<[BindingValue]>)> =
            index.into_iter().map(|(key, at)| (at, key)).collect();
        keys.sort_unstable_by_key(|(at, _)| *at);
        self.out = keys
            .into_iter()
            .zip(groups)
            .map(|((_, key), group)| (key, group))
            .rev()
            .collect();
        Ok(())
    }
}

impl<'q> Operator<'q> for Aggregate<'q> {
    fn next(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<BindingRow>> {
        self.held.recharge(cx)?;
        if let Some(input) = self.input.take() {
            self.fold(input, cx)?;
        }
        let Some((key, group)) = self.out.pop() else {
            return Ok(None);
        };
        cx.meter.charge(WorkResource::BindingRows, 1)?;
        self.held.release(cx, WorkResource::Groups, 1);
        self.held
            .release(cx, WorkResource::SortBytes, group.sort_bytes);
        self.held
            .release(cx, WorkResource::ListBytes, group.list_bytes);
        let mut slots = vec![BindingValue::Null; usize::from(self.width)];
        let values = key.into_vec().into_iter().chain(
            group
                .accs
                .into_iter()
                .zip(self.aggs)
                .map(|(acc, spec)| acc.finish(spec, cx.host))
                .collect::<QueryResult<Vec<_>>>()?,
        );
        for (slot, value) in slots.iter_mut().zip(values) {
            *slot = value;
        }
        Ok(Some(BindingRow {
            slots: slots.into_boxed_slice(),
        }))
    }
}
