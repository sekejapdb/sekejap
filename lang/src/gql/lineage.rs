//! Index lineage (M6-E of `docs/lang/GQL_PROFILE_DESIGN_M5_M7.md` §3.5):
//! which seed node, or which property of it, a slot's value IS, so that a
//! conjunct written later -- a `FILTER`, or the outer `SELECT`'s `WHERE` --
//! can be answered by the index that seeds that node.
//!
//! A lineage STARTS where a seed binds a node of one label collection (the
//! slot holds the node itself), is COPIED by `n.f` (the property of such a
//! node), by an alias (`RETURN x AS y`, `LET y = x`) and across `NEXT` by a
//! column that is exactly `x`, and ENDS everywhere else: arithmetic, an
//! aggregate, a `CASE`, a grouped `RETURN`. Each lineage names its seed by a
//! mark ([`Op::Seed`]'s), not by position, because operators are moved into
//! bodies after they are planned.
//!
//! A conjunct MOVES into the seed only when (1) every slot it reads has a
//! lineage to the same seed node, (2) a READY index of that node's collection
//! answers the conjunct exactly (the SQL side's rules, `Planner::seedable`
//! and `Planner::host_seed`), and (3) every operator between the seed and
//! the conjunct is a per-row streaming one that neither chooses among rows
//! nor counts them ([`streaming`]). Then the moved conjunct removes exactly
//! the rows it would have removed later. Anything else stays where it was
//! written: that is what keeps a `FILTER` no index answers a budgeted scan.
//! A conjunct inside an `EXISTS`, `CALL` or `UNION` body never moves out of
//! its body (`Planner::floor`, set by `Planner::scoped`): under
//! `NOT EXISTS` that would change the answer.

use super::eval::{IndexSeed, SeedFilter, SeedOrder};
use super::expr::Ex;
use super::host::HostEx;
use sekejap_core::collections::gql::BindingValue;
use super::plan::{ready, show, Access, FilterAt, Op, Planner};
use super::schema::BindingSchema;
use super::stage::TableOp;
use crate::ast::VecOp;
use crate::{SqlError, SqlResult2};
use sekejap_core::collections::gql::{ExistsMode, PathSearch, SeedSource, SlotId};
use sekejap_core::collections::{IndexFamily, VectorMetric};
use sekejap_core::Kind;

/// What a slot's value is, in terms of a seed node: `origin` is an
/// expression over the node's slot in the seeding stage's schema,
/// `Ex::Slot(node)` or `Ex::NodeProperty(node, field)`.
#[derive(Clone, Debug)]
pub(crate) struct Lineage {
    pub(crate) mark: u32,
    pub(crate) origin: Ex,
}

/// The lineage of the value `ex` gives, over `schema`.
pub(super) fn of(schema: &BindingSchema, ex: &Ex) -> Option<Lineage> {
    match ex {
        Ex::Slot(slot) => schema.lineage(*slot).cloned(),
        Ex::NodeProperty(slot, field) => node_of(schema, *slot).map(|(mark, node)| Lineage {
            mark,
            origin: Ex::NodeProperty(node, field.clone()),
        }),
        _ => None,
    }
}

/// The seed node the slot holds, if its lineage is a node.
fn node_of(schema: &BindingSchema, slot: SlotId) -> Option<(u32, SlotId)> {
    match schema.lineage(slot) {
        Some(Lineage {
            mark,
            origin: Ex::Slot(node),
        }) => Some((*mark, *node)),
        _ => None,
    }
}

/// `ex` rewritten over its seed node, with the seed's mark: only the
/// shapes a seed can answer, and only when every slot it reads has a
/// lineage to the SAME seed.
fn rebase(schema: &BindingSchema, ex: &Ex) -> Option<(u32, Ex)> {
    let mut mark = None;
    let rebased = match ex {
        Ex::Compare(op, left, right) => Ex::Compare(
            *op,
            Box::new(side(schema, left, &mut mark)?),
            Box::new(side(schema, right, &mut mark)?),
        ),
        Ex::Host(host) => Ex::Host(Box::new(match &**host {
            HostEx::Text {
                node,
                field,
                indexes,
                query,
                score: false,
            } => {
                let (at, seed) = node_of(schema, *node)?;
                agree(&mut mark, at)?;
                HostEx::Text {
                    node: seed,
                    field: field.clone(),
                    indexes: indexes.clone(),
                    query: query.clone(),
                    score: false,
                }
            }
            HostEx::Spatial {
                predicate,
                left,
                shape,
                metres,
            } => HostEx::Spatial {
                predicate: *predicate,
                left: side(schema, left, &mut mark)?,
                shape: shape.clone(),
                metres: metres.clone(),
            },
            _ => return None,
        })),
        _ => return None,
    };
    Some((mark?, rebased))
}

/// One operand of a movable conjunct: a slot or property with lineage, or
/// a value that reads no slot.
fn side(schema: &BindingSchema, ex: &Ex, mark: &mut Option<u32>) -> Option<Ex> {
    match ex {
        Ex::Slot(_) | Ex::NodeProperty(..) => {
            let lineage = of(schema, ex)?;
            agree(mark, lineage.mark)?;
            Some(lineage.origin)
        }
        other if other.refs().is_empty() => Some(other.clone()),
        _ => None,
    }
}

fn agree(mark: &mut Option<u32>, at: u32) -> Option<()> {
    match mark {
        Some(seen) if *seen != at => None,
        _ => {
            *mark = Some(at);
            Some(())
        }
    }
}

/// True for an operator that emits the rows of one input row before it
/// pulls the next, and neither chooses among rows nor counts them: a
/// conjunct over the seed node commutes with it (design §3.5 use 1).
fn streaming(op: &Op) -> bool {
    match op {
        Op::Seed { .. } | Op::Expand { .. } | Op::Filter { .. } | Op::Project { .. } => true,
        Op::PathSearch { search, .. } => matches!(search, PathSearch::Enumerate),
        Op::Table(TableOp::Let { .. } | TableOp::Unnest { .. }) => true,
        // A left-outer apply keeps every input row, and the slots it adds
        // are not the seed node's.
        Op::Optional { .. } => true,
        // A mark adds a column; a filter drops rows by its own test, which
        // commutes with another filter.
        Op::Exists {
            mode: ExistsMode::Filter { .. } | ExistsMode::Mark { .. },
            ..
        } => true,
        Op::Reach(_) | Op::Call { .. } | Op::Union(_) => false,
        Op::Table(_) => false,
    }
}

/// The conjuncts of `ex`, its top-level `AND`s split.
fn conjuncts(ex: Ex, out: &mut Vec<Ex>) {
    match ex {
        Ex::And(left, right) => {
            conjuncts(*left, out);
            conjuncts(*right, out);
        }
        other => out.push(other),
    }
}

impl Planner<'_> {
    /// A `Filter` for `ex` at `at`, each of whose conjuncts moves into the
    /// seed it has lineage to when it may; `from` names where it was written
    /// for `EXPLAIN`.
    pub(super) fn filter_moving(&mut self, ex: Ex, at: FilterAt, from: &str) -> SqlResult2<()> {
        let mut all = Vec::new();
        conjuncts(ex, &mut all);
        let mut kept = Vec::with_capacity(all.len());
        for conjunct in all {
            if !self.move_to_seed(&conjunct, from)? {
                kept.push(conjunct);
            }
        }
        self.filter(kept, at);
        Ok(())
    }

    /// Move `ex` into the seed it has lineage to, if the rules allow.
    fn move_to_seed(&mut self, ex: &Ex, from: &str) -> SqlResult2<bool> {
        let Some((mark, rebased)) = rebase(&self.schema, ex) else {
            return Ok(false);
        };
        let Some(at) = self.ops[self.floor..]
            .iter()
            .position(|op| matches!(op, Op::Seed { mark: m, .. } if *m == mark))
            .map(|at| at + self.floor)
        else {
            return Ok(false);
        };
        if !self.ops[at + 1..].iter().all(streaming) {
            return Ok(false);
        }
        let Op::Seed { out: node, source, .. } = &self.ops[at] else {
            unreachable!("found as a seed")
        };
        let node = *node;
        let collection = match source {
            SeedSource::Scan { labels } if labels.len() == 1 => labels[0],
            SeedSource::Index { seed } => self.program().seed_collection(*seed),
            _ => return Ok(false),
        };
        let indexes = self.db.list_indexes(collection).map_err(SqlError::from)?;
        let fields = self.db.collection_info(collection).map_err(SqlError::from)?.layout.fields;
        let found = match self.seedable(node, &rebased) {
            Some((_, field, _)) if field == crate::KEY_COLUMN => None,
            Some((op, field, value)) => ready(&indexes, field, IndexFamily::Scalar).map(|info| {
                (
                    SeedFilter::Scalar {
                        index: info.id,
                        kind: info.kind.clone(),
                        field: field.to_owned(),
                        op,
                        value: value.clone(),
                    },
                    info.name.clone(),
                    "scalar_postings",
                )
            }),
            None => self.host_seed(node, collection, &indexes, &fields, &rebased),
        };
        let Some((filter, name, charge)) = found else {
            return Ok(false);
        };
        let part = (name, format!("{} -- moved from {from} by lineage", show(ex, &self.schema)));
        let scanned = matches!(&self.ops[at], Op::Seed { source: SeedSource::Scan { .. }, .. });
        let created = scanned.then(|| {
            self.program_mut().seed(IndexSeed {
                collection,
                filters: Vec::new(),
                order: None,
            })
        });
        let Op::Seed { source, access, .. } = &mut self.ops[at] else {
            unreachable!("found as a seed")
        };
        if let Some(seed) = created {
            *source = SeedSource::Index { seed };
            *access = Access::Index {
                parts: Vec::new(),
                charges: String::new(),
            };
        }
        let SeedSource::Index { seed } = *source else {
            unreachable!("an index seed now")
        };
        if let Access::Index { parts, charges } = access {
            parts.push(part);
            if !charges.contains(charge) {
                *charges = if charges.is_empty() {
                    charge.to_owned()
                } else {
                    format!("{charges}, {charge}")
                };
            }
        }
        self.program_mut().seed_mut(seed).filters.push(filter);
        Ok(true)
    }

    /// Read the seed of `key`'s node in the order of `key`, for a limited
    /// sort whose first key it is (design §3.5 use 2), when the index order
    /// IS the key's order row for row: an exact vector index under `<->`,
    /// `<=>` or `<#>` (the engine ranks with the arithmetic the expression
    /// evaluates, `vector_distance`, and puts an all-zero vector's NaN after
    /// every number and a row with no vector after that, where this sort puts
    /// NaN and NULL), from a seed that is the plan's first operator (it opens
    /// once) with only streaming operators after it. Ascending only: that is
    /// the direction of nearness. Anything else keeps the scan and the full
    /// sort, which are exact too.
    pub(super) fn order_seed(&mut self, key: &Ex, descending: bool) -> SqlResult2<Option<Ex>> {
        let Ex::Host(host) = key else { return Ok(None) };
        let HostEx::Vector { op, left, right } = &**host else {
            return Ok(None);
        };
        let metric = match op {
            VecOp::L2 => VectorMetric::SquaredL2,
            VecOp::NegativeDot => VectorMetric::NegativeDot,
            VecOp::Cosine => VectorMetric::Cosine,
        };
        if descending || self.floor != 0 {
            return Ok(None);
        }
        let (property, query) = if right.refs().is_empty() { (left, right) } else { (right, left) };
        if !query.refs().is_empty() {
            return Ok(None);
        }
        let Some(Lineage {
            mark,
            origin: Ex::NodeProperty(_, field),
        }) = of(&self.schema, property)
        else {
            return Ok(None);
        };
        match self.ops.first() {
            Some(Op::Seed { mark: m, .. }) if *m == mark => {}
            _ => return Ok(None),
        }
        if !self.ops[1..].iter().all(streaming) {
            return Ok(None);
        }
        let collection = match &self.ops[0] {
            Op::Seed {
                source: SeedSource::Scan { labels },
                ..
            } if labels.len() == 1 => labels[0],
            Op::Seed {
                source: SeedSource::Index { seed },
                ..
            } if self.program().seed_at(*seed).order.is_none() => self.program().seed_collection(*seed),
            _ => return Ok(None),
        };
        let info = self.db.collection_info(collection).map_err(SqlError::from)?;
        let Some(dimension) = info.layout.fields.iter().find_map(|(name, kind)| match kind {
            Kind::Vector(dimension) if **name == *field => Some(*dimension),
            _ => None,
        }) else {
            return Ok(None);
        };
        let indexes = self.db.list_indexes(collection).map_err(SqlError::from)?;
        let exact = ready(&indexes, &field, IndexFamily::ExactVector);
        let approximate = ready(&indexes, &field, IndexFamily::VamanaGraph)
            .or_else(|| ready(&indexes, &field, IndexFamily::QuantizedVector));
        // The index EXPLAIN names, and the condition the sort stops early
        // under: always, over an exact index; only under `ef_search`, over an
        // approximate one alone -- without the knob that seed is read
        // unordered and the sort orders every row, exactly.
        let (named, condition) = match (exact, approximate) {
            (Some(exact), _) => (exact, Ex::Const(BindingValue::Bool(true))),
            (None, Some(approximate)) => (approximate, Ex::Host(Box::new(HostEx::EfSearchSet))),
            (None, None) => return Ok(None),
        };
        // What the order is if the execution opened now: EXPLAIN reads the
        // knob when EXPLAIN runs, as the execution does when it opens.
        let exactness = match (crate::compile::ef_search(), exact, approximate) {
            (Some(ef), _, Some(info)) => format!(
                "APPROXIMATE (ef={ef}) through `{}` under SET LOCAL ef_search: the shortlist bounds the whole answer",
                info.name
            ),
            (None, Some(_), _) => "exact".to_owned(),
            (Some(_), Some(_), None) => {
                "exact: SET LOCAL ef_search is unused, the column has no approximate index".to_owned()
            }
            (None, None, Some(info)) => format!(
                "exact: the column has no exact index, so the rows are read unordered and sorted whole; SET LOCAL ef_search reads `{}` in order, APPROXIMATELY, and CREATE INDEX ... USING exact reads it in order exactly",
                info.name
            ),
            (_, None, None) => unreachable!("an index was found above"),
        };
        let order = SeedOrder::Vector {
            index: exact.map(|info| info.id),
            approximate: approximate.map(|info| info.id),
            metric,
            query: query.clone(),
            dimension,
        };
        let part = (
            named.name.clone(),
            format!("ordered by {}, {exactness}", show(key, &self.schema)),
        );
        let created = matches!(&self.ops[0], Op::Seed { source: SeedSource::Scan { .. }, .. }).then(|| {
            self.program_mut().seed(IndexSeed {
                collection,
                filters: Vec::new(),
                order: None,
            })
        });
        let Op::Seed { source, access, .. } = &mut self.ops[0] else {
            unreachable!("checked above")
        };
        if let Some(seed) = created {
            *source = SeedSource::Index { seed };
            *access = Access::Index {
                parts: Vec::new(),
                charges: String::new(),
            };
        }
        let SeedSource::Index { seed } = *source else {
            unreachable!("an index seed now")
        };
        if let Access::Index { parts, charges } = access {
            parts.push(part);
            *charges = if charges.is_empty() {
                "vector_sidecars, vector_lanes".to_owned()
            } else {
                format!("{charges}, vector_sidecars, vector_lanes")
            };
        }
        self.program_mut().seed_mut(seed).order = Some(order);
        Ok(Some(condition))
    }
}
