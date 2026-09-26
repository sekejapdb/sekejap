//! `Seed`: bind a node slot to each start node, once per input row
//! (`docs/lang/GQL_PROFILE_DESIGN.md` §3.2, §7).
//!
//! | source | how | charges |
//! | --- | --- | --- |
//! | key | one external-key mapping lookup per collection of the label | `key_postings` 1 per lookup |
//! | index | the host's prepared query, paged 256 ids at a time | whatever that walk charges |
//! | bound | the node already in a slot, after its label test | nothing |
//! | scan | the query engine's entity walk over each collection | what that walk charges (a primary read per row) |
//!
//! And `binding_rows` 1 per row out, for every source.
//!
//! Seeds are found again per execution, under its snapshot: a plan holds no
//! entity id. A key that is not there -- deleted, never written, of another
//! collection, or `NULL` -- is an empty stream, never an error.

use super::super::super::{
    corrupt_query, invalid_query, CandidateDriver, PreparedQuery, Projection, QueryError,
    QueryOrder, QueryPage, QueryRequest, QueryResult, QueryRow, WorkResource,
};
use super::super::host::ExecMeter;
use super::super::plan::SeedSource;
use super::super::value::{BindingRow, BindingValue, NodeRef, SlotId};
use super::{ExecCx, Op, Operator};
use crate::collections::{mapping_key, read_ordered, CollectionId, EntityId};
use std::sync::Arc;

/// Ids one engine page of an index or scan seed hands over: the refill
/// bound the design states (§3.2).
const SEED_PAGE_ROWS: usize = 256;

pub(super) struct Seed<'q> {
    input: Op<'q>,
    out: usize,
    source: &'q SeedSource,
    /// The input row being seeded, and what is left of its seeds.
    current: Option<(BindingRow, Starts<'q>)>,
}

/// The start nodes of one input row still to hand out.
enum Starts<'q> {
    /// Nothing (left).
    Empty,
    /// A bound node, not yet handed out.
    One(NodeRef),
    /// A key, the collections to look it up in, and the next one's index.
    Key {
        key: Arc<str>,
        labels: &'q [CollectionId],
        at: usize,
    },
    /// An index seed's query.
    Index(Paged<'q>),
    /// The collections to scan, the next one's index, and the walk of the
    /// one being scanned.
    Scan {
        labels: &'q [CollectionId],
        at: usize,
        walk: Option<Paged<'q>>,
    },
}

impl<'q> Seed<'q> {
    pub(super) fn new(input: Op<'q>, out: SlotId, source: &'q SeedSource) -> Self {
        Self {
            input,
            out: usize::from(out.0),
            source,
            current: None,
        }
    }

    /// The start nodes for input row `row`.
    fn starts(&self, row: &BindingRow, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Starts<'q>> {
        Ok(match self.source {
            SeedSource::Key { key, labels } => match cx.eval(*key, row)? {
                BindingValue::Text(key) => Starts::Key { key, labels, at: 0 },
                BindingValue::Null => Starts::Empty,
                other => {
                    return Err(invalid_query(format!(
                        "a key seed needs a text key, not {other:?}"
                    )))
                }
            },
            SeedSource::Index { seed } => match cx.open_seed(*seed, row)? {
                Some(query) => Starts::Index(Paged::new(query)),
                None => Starts::Empty,
            },
            SeedSource::Bound { slot, labels } => match row.get(*slot) {
                BindingValue::Node(node)
                    if labels
                        .as_deref()
                        .is_none_or(|labels| labels.contains(&node.0.collection)) =>
                {
                    Starts::One(*node)
                }
                BindingValue::Node(_) | BindingValue::Null => Starts::Empty,
                other => {
                    return Err(invalid_query(format!(
                        "a bound seed needs a node, not {other:?}"
                    )))
                }
            },
            SeedSource::Scan { labels } => Starts::Scan {
                labels,
                at: 0,
                walk: None,
            },
        })
    }

    /// The next start node of the current input row.
    fn next_start(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<NodeRef>> {
        let Some((_, starts)) = &mut self.current else {
            return Ok(None);
        };
        match starts {
            Starts::Empty => Ok(None),
            Starts::One(node) => {
                let node = *node;
                *starts = Starts::Empty;
                Ok(Some(node))
            }
            Starts::Key { key, labels, at } => {
                while let Some(&collection) = labels.get(*at) {
                    *at += 1;
                    cx.meter.charge(WorkResource::KeyPostings, 1)?;
                    if let Some(id) = lookup(cx, collection, key)? {
                        return Ok(Some(NodeRef(id)));
                    }
                }
                Ok(None)
            }
            Starts::Index(walk) => Ok(walk.next(cx.meter)?.map(NodeRef)),
            Starts::Scan { labels, at, walk } => loop {
                if let Some(open) = walk {
                    if let Some(id) = open.next(cx.meter)? {
                        return Ok(Some(NodeRef(id)));
                    }
                }
                let Some(&collection) = labels.get(*at) else {
                    return Ok(None);
                };
                *at += 1;
                *walk = Some(Paged::new(cx.db.prepare_query(QueryRequest {
                    collection,
                    filters: &[],
                    order: QueryOrder::Driver,
                    projection: Projection::Ids,
                    total_limit: None,
                    driver: CandidateDriver::Entities,
                })?));
            },
        }
    }
}

impl<'q> Operator<'q> for Seed<'q> {
    fn next(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<BindingRow>> {
        loop {
            if let Some(node) = self.next_start(cx)? {
                let (row, _) = self
                    .current
                    .as_ref()
                    .expect("a start came from the current row");
                let mut row = row.clone();
                row.slots[self.out] = BindingValue::Node(node);
                cx.meter.charge(WorkResource::BindingRows, 1)?;
                return Ok(Some(row));
            }
            let Some(row) = self.input.next(cx)? else {
                self.current = None;
                return Ok(None);
            };
            let starts = self.starts(&row, cx)?;
            self.current = Some((row, starts));
        }
    }
}

/// The row `key` names in `collection` under this snapshot, if any: one
/// point read of the external-key mapping.
fn lookup(
    cx: &ExecCx<'_, '_, '_>,
    collection: CollectionId,
    key: &str,
) -> QueryResult<Option<EntityId>> {
    let Some(bytes) = cx.db.store()?.get(&mapping_key(collection, key))? else {
        return Ok(None);
    };
    let mut at = 0;
    let sequence = read_ordered(&bytes, &mut at)?;
    if at != bytes.len() || sequence == 0 {
        return Err(corrupt_query("external-key mapping"));
    }
    Ok(Some(EntityId {
        collection,
        sequence,
    }))
}

/// A prepared engine query handed over in pages of ids.
struct Paged<'q> {
    query: PreparedQuery<'q>,
    rows: std::vec::IntoIter<QueryRow>,
    done: bool,
}

impl<'q> Paged<'q> {
    fn new(query: PreparedQuery<'q>) -> Self {
        Self {
            query,
            rows: Vec::new().into_iter(),
            done: false,
        }
    }

    fn next(&mut self, meter: &mut ExecMeter<'_>) -> QueryResult<Option<EntityId>> {
        loop {
            if let Some(row) = self.rows.next() {
                return Ok(Some(row.id));
            }
            if self.done {
                return Ok(None);
            }
            let page = engine_page(&mut self.query, meter)?;
            self.done = page.done;
            self.rows = page.rows.into_iter();
        }
    }
}

/// One engine page of `query`, run under what is LEFT of the GQL page's
/// budget, with the GQL page's cancellation, and charged to its meter.
///
/// A refusal inside the engine page names the resource against the budget
/// it was handed -- the remainder. It is restated against the whole GQL
/// page: the limit that page was given, and the total the charge would have
/// reached. Which resources [`QueryBudget::left_after`] subtracts is read
/// off the refusal itself, so no list of them is kept here to drift from
/// it: a resource it subtracted was handed exactly the page's limit less
/// what the page had spent; one it passes through was handed the whole
/// limit and is restated as it is.
///
/// [`QueryBudget::left_after`]: super::super::super::QueryBudget
fn engine_page(query: &mut PreparedQuery<'_>, meter: &mut ExecMeter<'_>) -> QueryResult<QueryPage> {
    let base = meter.base();
    let budget = base.limit.left_after(&base.used);
    match query.next_page(SEED_PAGE_ROWS, budget, &mut *base.cancelled) {
        Ok(page) => {
            base.used.add_page(&page.work);
            Ok(page)
        }
        Err(QueryError::BudgetExceeded {
            resource,
            limit,
            attempted,
        }) => {
            let (used, total) = base.slot(resource);
            let spent = *used;
            if spent > 0 && total.checked_sub(spent) == Some(limit) {
                return Err(QueryError::BudgetExceeded {
                    resource,
                    limit: total,
                    attempted: attempted.saturating_add(spent),
                });
            }
            Err(QueryError::BudgetExceeded {
                resource,
                limit,
                attempted,
            })
        }
        Err(error) => Err(error),
    }
}
