//! The AST, turned into calls the crate already has.
//!
//! Every SELECT becomes one `QueryRequest`; every write becomes `put`,
//! `update` or `delete`; every DDL statement becomes a `create_*` or a
//! catalog drop. Nothing here executes anything itself, and nothing here
//! invents a predicate the engine does not already have -- a shape with no
//! atomic is refused with the atomic named.
//!
//! The plan OWNS what the request borrows. A `QueryRequest` is a borrowed
//! view (`&str` terms, `&[f32]` query vectors, a `ScoreExpr` tree of
//! references), so the compiled form holds owned values and hands out the
//! borrowed view for the length of one page loop.

use super::ast::*;
use super::functions::{self, TimeUnit};
use super::{
    collection, is_key_column, order_value, projected, refuse, SqlError, SqlResult, SqlResult2,
    SqlRow, SqlValue, Tier, GRAPH_EDGES, GRAPH_RESULTS, GRAPH_VISITED, ID_COLUMN, KEY_COLUMN,
    MAX_GRAPH_DEPTH,
};
use sekejap_core::collections::{
    Accumulator, AggValue, AggregateFn, AggregateInput, AggregateRequest, BfsRequest,
    CandidateDriver, Cmp, CollectionId, CollectionOptions, Database, Direction, DropMode,
    DropPhase, EdgePredicate, EdgeTypeId, EntityId, Geom, GeometryFilter, GraphContextId,
    ColumnRule, DefaultValue, GroupCmp, GroupKey, GroupOrder, GroupPredicate, GroupRow, IndexExpr,
    IndexFamily, IndexId,
    IndexInfo, IndexState, OwnedScalarValue, PointFilter, ProjectedValue, Projection, QueryBudget,
    QueryFilter, QueryOrder, QueryRequest, QueryRow, ScalarFilter, ScalarValue, ScoreExpr,
    SortDirection, TextMatch, UpdatePatch, VectorMetric, WriteAction, WriteCursor, WriteRequest,
};
use sekejap_core::internal::EDGE_FIELD_PREFIX;
use sekejap_core::spatial_math::{Bounds, Point};
use sekejap_core::Kind;
use serde_json::{Map, Value};
use std::{cell::Cell, ops::Bound};

/// The approximate shortlist width a vector order gets when the only index on
/// the column is quantized and no `SET LOCAL` named one. pgvector's own
/// default is 40; this is battle50k's `EF`, which is the number this tree's
/// approximate measurements are written against.
const DEFAULT_EF: usize = 100;

thread_local! {
    /// `SET LOCAL ef_search` / `diskann.query_search_list_size`, remembered
    /// between statements.
    ///
    /// `Database` has no session object -- a connection is a process here --
    /// so the session knob lives in the thread that ran the statement, and
    /// COMMIT or ROLLBACK clears it, which is what LOCAL means.
    static EF_SEARCH: Cell<Option<usize>> = const { Cell::new(None) };
}

// `super::parser` is reached by path from `predicates.rs`; the name has
// to be bound here for `super::` to find it one level down.
use super::parser;

mod aggregate;
mod boolean;
mod ddl;
mod dml;
mod graph_table;
mod plan;
mod predicates;
mod row;
mod select;
// `functions.rs` carries the §4.1 / §4.2 range rewrites. The module is
// named `range_rewrites` because the name `functions` is already taken
// here by the `super::functions` import above.
#[path = "functions.rs"]
mod range_rewrites;

use plan::*;
use row::*;

pub(crate) use aggregate::AggregatePlan;
pub(crate) use plan::{Plan, SelectPlan};

// ── the compiler ──────────────────────────────────────────────────────────

struct Compiler<'a> {
    db: &'a Database,
    params: &'a [super::Param],
    notices: &'a mut Vec<String>,
    /// The index registry of each collection this statement names, read
    /// ONCE per statement. `list_indexes` is a catalog walk (one range plus
    /// several point reads per index); a statement with five predicates on
    /// one collection used to pay it five times.
    index_lists: std::cell::RefCell<Vec<(CollectionId, Vec<IndexInfo>)>>,
    /// The budget and the cancellation this statement's caller handed in.
    ///
    /// COMPILING is not free here: a semi-join's set is built while the
    /// statement is compiled, and it is an index walk or a whole inner query.
    /// Both used to run under `QueryBudget::unlimited()` and a cancel closure
    /// that always said no, so `Ctrl-C` was inert and no resource was charged
    /// for work the caller had asked to bound.
    budget: QueryBudget,
    cancelled: &'a mut dyn FnMut() -> bool,
    /// One line per WHERE function folded into an index range, and one per
    /// projected row function. They are the two EXPLAIN sections
    /// `docs/lang/QL_CONTRACT.md` §4.1 and §4.2 ask for: a rewrite is index-side
    /// and costs candidates, a row function is per RETURNED row.
    rewrites: Vec<String>,
    row_functions: Vec<String>,
    /// `now()` and `current_date`, folded ONCE for the whole statement.
    clock: i64,
}

pub(crate) fn compile(
    db: &Database,
    statement: Stmt,
    params: &[super::Param],
    notices: &mut Vec<String>,
    budget: QueryBudget,
    cancelled: &mut dyn FnMut() -> bool,
) -> SqlResult2<Plan> {
    let mut compiler = Compiler {
        index_lists: std::cell::RefCell::new(Vec::new()),
        rewrites: Vec::new(),
        row_functions: Vec::new(),
        clock: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX)),
        db,
        params,
        notices,
        budget,
        cancelled,
    };
    compiler.statement(statement)
}

impl Compiler<'_> {
    fn statement(&mut self, statement: Stmt) -> SqlResult2<Plan> {
        Ok(match statement {
            Stmt::Select(select) => match self.aggregate(&select)? {
                Some(plan) => Plan::Aggregate(plan),
                None => Plan::Select(self.select(*select)?),
            },
            Stmt::Explain(select) => match self.aggregate(&select)? {
                Some(plan) => Plan::ExplainAggregate(plan),
                None => Plan::Explain(self.select(*select)?),
            },
            Stmt::Insert {
                table,
                columns,
                rows,
            } => Plan::Write(self.insert(&table, &columns, &rows)?),
            Stmt::Update {
                table,
                assignments,
                key,
            } => Plan::Write(self.update(&table, &assignments, &key)?),
            Stmt::Delete { table, key } => {
                let collection = collection(self.db, &table)?;
                let key = self.text_of(&key)?;
                Plan::Write(WritePlan::Delete { collection, key })
            }
            Stmt::UpdateWhere {
                table,
                assignments,
                predicates,
            } => Plan::Write(self.update_where(&table, &assignments, &predicates)?),
            Stmt::DeleteWhere {
                table,
                predicates,
                cascade,
            } => Plan::Write(self.delete_where(table.as_deref(), &predicates, cascade)?),
            Stmt::ExplainWrite(write) => Plan::ExplainText(self.explain_write(*write)?),
            Stmt::BeginBulk => Plan::Write(WritePlan::BeginBulk),
            Stmt::EndBulk => Plan::Write(WritePlan::EndBulk),
            Stmt::CreateTable {
                table,
                columns,
                if_not_exists,
            } => Plan::Write(self.create_table(table, columns, if_not_exists)?),
            Stmt::CreateIndex {
                name,
                table,
                method,
            } => Plan::Write(self.create_index(name, &table, method)?),
            Stmt::DropTable {
                table,
                if_exists,
                cascade,
            } => match self.drop_table(&table, if_exists, cascade)? {
                Some(plan) => Plan::Write(plan),
                None => Plan::Write(WritePlan::Notice(format!(
                    "DROP TABLE IF EXISTS {table}: no such collection"
                ))),
            },
            Stmt::ExplainDropTable {
                table,
                if_exists,
                cascade,
            } => Plan::ExplainText(self.explain_drop_table(&table, if_exists, cascade)?),
            Stmt::DropIndex { name, if_exists } => {
                let Some(index) = self.index_named(&name)? else {
                    if if_exists {
                        return Ok(Plan::Write(WritePlan::Notice(format!(
                            "DROP INDEX IF EXISTS {name}: no such index"
                        ))));
                    }
                    return Err(SqlError::engine(format!("no index named `{name}`")));
                };
                Plan::Write(WritePlan::DropIndex { index, name })
            }
            Stmt::AlterTable { table, action } => {
                Plan::Write(self.alter_table(&table, &action)?)
            }
            Stmt::ExplainAlterTable { table, action } => {
                Plan::ExplainText(self.explain_alter_table(&table, &action)?)
            }
            Stmt::Begin => Plan::Write(WritePlan::Begin),
            Stmt::Commit => Plan::Write(WritePlan::Commit),
            Stmt::Rollback => Plan::Write(WritePlan::Rollback),
            Stmt::SetLocal { name, value } => Plan::Write(self.set_local(&name, &value)?),
        })
    }

}

impl Compiler<'_> {
    fn set_local(&mut self, name: &str, value: &Literal) -> SqlResult2<WritePlan> {
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            "ef_search" | "hnsw.ef_search" | "diskann.query_search_list_size" => {
                // `= DEFAULT` clears the knob, which is what RESET means and
                // what COMMIT does at the end of a transaction.
                if matches!(value, Literal::Str(text) if text.eq_ignore_ascii_case("default")) {
                    EF_SEARCH.with(|cell| cell.set(None));
                    return Ok(WritePlan::Notice(format!(
                        "SET LOCAL {name} = DEFAULT: the approximate shortlist bound is cleared, so a vector order takes the exact family when the column has one"
                    )));
                }
                let ef = self.i64_of(value)?;
                if ef <= 0 {
                    return Err(SqlError::unsupported(format!(
                        "SET LOCAL {name} = {ef}: an approximate shortlist has a positive width"
                    )));
                }
                EF_SEARCH.with(|cell| cell.set(Some(ef as usize)));
                Ok(WritePlan::Notice(format!(
                    "SET LOCAL {name} = {ef}: read as `ef`, the approximate shortlist bound of QueryOrder::ApproximateVector (QL_CONTRACT §4.5; the two knobs are not the same algorithm -- battle50k deviation 12)"
                )))
            }
            _ => Ok(WritePlan::Notice(format!(
                "SET LOCAL {name}: no such knob here, and nothing was changed. The planner has no enable_indexscan / enable_bitmapscan: a query's driver is chosen from the indexes the statement's own predicates name (`src/query/drivers.rs`), and exactness is which vector index the column has, not a planner setting"
            ))),
        }
    }

    // ── values ───────────────────────────────────────────────────────────

    fn param(&self, n: usize) -> SqlResult2<&super::Param> {
        self.params.get(n - 1).ok_or_else(|| {
            SqlError::Parameter(format!(
                "${n} is not bound; {} parameter(s) were given",
                self.params.len()
            ))
        })
    }

    /// A literal, as JSON. A subquery runs here, at compile time, because by
    /// the time the outer statement runs it is a constant.
    fn value_of(&self, literal: &Literal) -> SqlResult2<Value> {
        Ok(match literal {
            Literal::Null => Value::Null,
            Literal::Bool(b) => Value::Bool(*b),
            Literal::Num(v, exact) => {
                if *exact && v.fract() == 0.0 && v.abs() < 9.0e18 {
                    Value::from(*v as i64)
                } else {
                    Value::from(*v)
                }
            }
            Literal::Str(s) => Value::String(s.clone()),
            Literal::Param(n) => match self.param(*n)? {
                super::Param::Null => Value::Null,
                super::Param::Bool(b) => Value::Bool(*b),
                super::Param::Int(i) => Value::from(*i),
                super::Param::Float(f) => Value::from(*f),
                super::Param::Text(t) => Value::String(t.clone()),
                super::Param::Vector(v) => Value::from(v.clone()),
                super::Param::Json(v) => v.clone(),
            },
            Literal::Subquery(query) => {
                let collection = collection(self.db, &query.table)?;
                let key = self.text_of(&query.key)?;
                let entity = self
                    .db
                    .get(collection, &key)
                    .map_err(SqlError::from)?
                    .ok_or_else(|| {
                        SqlError::engine(format!(
                            "scalar subquery: `{}` has no row at key `{key}`",
                            query.table
                        ))
                    })?;
                if query.column == KEY_COLUMN {
                    Value::String(entity.key)
                } else {
                    entity
                        .document
                        .get(&query.column)
                        .cloned()
                        .unwrap_or(Value::Null)
                }
            }
        })
    }

    fn text_of(&self, literal: &Literal) -> SqlResult2<String> {
        match self.value_of(literal)? {
            Value::String(s) => Ok(s),
            other => Err(SqlError::Parameter(format!(
                "expected text, found {other}"
            ))),
        }
    }

    fn f64_of(&self, literal: &Literal) -> SqlResult2<f64> {
        match self.value_of(literal)? {
            Value::Number(n) => n
                .as_f64()
                .ok_or_else(|| SqlError::Parameter("number is not finite".into())),
            other => Err(SqlError::Parameter(format!(
                "expected a number, found {other}"
            ))),
        }
    }

    fn i64_of(&self, literal: &Literal) -> SqlResult2<i64> {
        match self.value_of(literal)? {
            Value::Number(n) => n
                .as_i64()
                .ok_or_else(|| SqlError::Parameter("expected a whole number".into())),
            other => Err(SqlError::Parameter(format!(
                "expected a whole number, found {other}"
            ))),
        }
    }

    /// pgvector's text form `[a,b,c]`, a JSON array, or a bound
    /// `Param::Vector`.
    fn vector_of(&self, literal: &Literal) -> SqlResult2<Vec<f32>> {
        if let Literal::Param(n) = literal {
            if let super::Param::Vector(v) = self.param(*n)? {
                return Ok(v.clone());
            }
        }
        match self.value_of(literal)? {
            Value::String(text) => parse_vector_literal(&text),
            Value::Array(items) => items
                .iter()
                .map(|item| {
                    item.as_f64()
                        .map(|v| v as f32)
                        .ok_or_else(|| SqlError::Parameter("vector holds a non-number".into()))
                })
                .collect(),
            other => Err(SqlError::Parameter(format!(
                "expected a vector literal, found {other}"
            ))),
        }
    }

    fn point_of(&self, point: &PointArg) -> SqlResult2<Point> {
        let lon = self.f64_of(&point.lon)?;
        let lat = self.f64_of(&point.lat)?;
        Point::new(lon, lat).map_err(|e| SqlError::engine(format!("ST_MakePoint({lon}, {lat}): {e}")))
    }

    fn geom_of(&self, argument: &GeoArg) -> SqlResult2<Geom> {
        Ok(match argument {
            GeoArg::Point(point) => {
                let p = self.point_of(point)?;
                Geom::Point(p.longitude(), p.latitude())
            }
            GeoArg::Envelope {
                minlon,
                minlat,
                maxlon,
                maxlat,
            } => {
                let (w, s, e, n) = (
                    self.f64_of(minlon)?,
                    self.f64_of(minlat)?,
                    self.f64_of(maxlon)?,
                    self.f64_of(maxlat)?,
                );
                Geom::Polygon(vec![vec![[w, s], [e, s], [e, n], [w, n], [w, s]]])
            }
            GeoArg::GeoJson(literal) => {
                let value = self.value_of(literal)?;
                let document = match value {
                    Value::String(text) => serde_json::from_str::<Value>(&text)
                        .map_err(|e| SqlError::Parameter(format!("GeoJSON: {e}")))?,
                    other => other,
                };
                geom_from_json(&document)?
            }
        })
    }

    fn bounds_of(&self, argument: &GeoArg) -> SqlResult2<Bounds> {
        match argument {
            GeoArg::Envelope {
                minlon,
                minlat,
                maxlon,
                maxlat,
            } => {
                let (w, s, e, n) = (
                    self.f64_of(minlon)?,
                    self.f64_of(minlat)?,
                    self.f64_of(maxlon)?,
                    self.f64_of(maxlat)?,
                );
                Bounds::new(w, e, s, n)
                    .map_err(|err| SqlError::engine(format!("ST_MakeEnvelope: {err}")))
            }
            _ => Err(SqlError::unsupported(
                "a rectangle over a Point column is ST_Within(col, ST_MakeEnvelope(...)): PointFilter::Bbox is a lon/lat rectangle and has no other shape",
            )),
        }
    }

    // ── the catalog ──────────────────────────────────────────────────────

    fn kind_of(&self, c: CollectionId, field: &str) -> SqlResult2<Kind> {
        let info = self.db.collection_info(c).map_err(SqlError::from)?;
        info.layout
            .fields
            .iter()
            .find(|(name, _)| name == field)
            .map(|(_, kind)| kind.clone())
            .ok_or_else(|| SqlError::engine(format!("no column `{field}` in this collection")))
    }

    /// The DECLARED SQL spelling of `field`, when the catalog records one.
    ///
    /// `TIMESTAMPTZ` and `DATE` are both `Kind::Int` (UTC microseconds,
    /// §5 deviation 8), so this is what tells a date/time rewrite from an
    /// ordinary integer comparison, and what makes a projected column print
    /// back as an ISO string.
    fn declared_of(&self, c: CollectionId, field: &str) -> SqlResult2<Option<String>> {
        let info = self.db.collection_info(c).map_err(SqlError::from)?;
        Ok(info
            .declared
            .iter()
            .find(|(name, _)| name == field)
            .map(|(_, declared)| declared.clone()))
    }

    /// `Some(declared)` when `field` is a declared TIMESTAMPTZ/DATE stored as
    /// `Kind::Int`.
    fn time_column(&self, c: CollectionId, field: &str) -> SqlResult2<Option<String>> {
        if !matches!(self.kind_of(c, field)?, Kind::Int) {
            return Ok(None);
        }
        Ok(self
            .declared_of(c, field)?
            .filter(|declared| functions::is_time_type(declared)))
    }

    fn declared_fields(&self, c: CollectionId) -> SqlResult2<Vec<String>> {
        let info = self.db.collection_info(c).map_err(SqlError::from)?;
        Ok(info
            .layout
            .fields
            .iter()
            .map(|(name, _)| name.clone())
            .filter(|name| !name.starts_with("__e4"))
            .collect())
    }

    /// The READY index of `family` over `field`, or an error that names what
    /// is missing. An index that exists but is still BUILDING is named too:
    /// the fix is different.
    fn index_for(
        &self,
        c: CollectionId,
        field: &str,
        family: IndexFamily,
        what: &str,
    ) -> SqlResult2<IndexId> {
        self.index_for_expression(c, field, family, None, what)
    }

    /// The same lookup, matching the index's EXPRESSION as well as its field.
    ///
    /// An expression index records the SOURCE field, so `kind = 'Home'` and
    /// `lower(kind) = 'home'` both name `kind` and must not pick each other's
    /// index: the first would be answered from keys that hold `'home'`, the
    /// second from keys that hold `'Home'`. The expression is part of the
    /// identity of the index a predicate names.
    fn index_for_expression(
        &self,
        c: CollectionId,
        field: &str,
        family: IndexFamily,
        expression: Option<IndexExpr>,
        what: &str,
    ) -> SqlResult2<IndexId> {
        let mut lists = self.index_lists.borrow_mut();
        let position = match lists.iter().position(|(id, _)| *id == c) {
            Some(position) => position,
            None => {
                lists.push((c, self.db.list_indexes(c).map_err(SqlError::from)?));
                lists.len() - 1
            }
        };
        let indexes = &lists[position].1;
        let mut building = false;
        for info in indexes {
            if info.field == field && info.family == family && info.expression == expression {
                match info.state {
                    IndexState::Ready => return Ok(info.id),
                    IndexState::Building { .. } => building = true,
                    IndexState::Dropping => {}
                }
            }
        }
        Err(SqlError::engine(if building {
            format!("{what} on `{field}` is still building; run it to READY before a query can use it")
        } else {
            format!(
                "{what} on `{field}` does not exist. QL_CONTRACT §6: every Tier-1 predicate on an indexed field is answered index-side, so the predicate compiles to a filter that NAMES an index; without one there is nothing to name"
            )
        }))
    }

    fn index_named(&self, name: &str) -> SqlResult2<Option<IndexId>> {
        for n in 1..=64u64 {
            if let Ok(info) = self.db.index_info(IndexId(n)) {
                if info.name == name {
                    return Ok(Some(info.id));
                }
            }
        }
        Ok(None)
    }

}

impl Compiler<'_> {
    /// The READY scalar index over `field`, if there is one. Unlike
    /// [`Compiler::index_for`] a missing index is not an error here: an
    /// aggregate over an unindexed column reads the ROW, which is a stated
    /// cost, not a refusal.
    fn scalar_index_opt(&self, c: CollectionId, field: &str) -> SqlResult2<Option<IndexId>> {
        let mut lists = self.index_lists.borrow_mut();
        let position = match lists.iter().position(|(id, _)| *id == c) {
            Some(position) => position,
            None => {
                lists.push((c, self.db.list_indexes(c).map_err(SqlError::from)?));
                lists.len() - 1
            }
        };
        Ok(lists[position]
            .1
            .iter()
            .find(|info| {
                info.field == field
                    && info.family == IndexFamily::Scalar
                    && info.state == IndexState::Ready
            })
            .map(|info| info.id))
    }

}

/// pgvector's text form: `[a, b, c]`.
fn parse_vector_literal(text: &str) -> SqlResult2<Vec<f32>> {
    let trimmed = text.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .ok_or_else(|| {
            SqlError::Parameter("a vector literal is written `[a,b,c]`, as pgvector writes it".into())
        })?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|part| {
            part.trim()
                .parse::<f32>()
                .map_err(|e| SqlError::Parameter(format!("vector literal: {e}")))
        })
        .collect()
}

/// A GeoJSON geometry, as `kernel::spatial::Geom`.
fn geom_from_json(value: &Value) -> SqlResult2<Geom> {
    let ty = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| SqlError::Parameter("GeoJSON has no `type`".into()))?;
    let coordinates = value
        .get("coordinates")
        .cloned()
        .ok_or_else(|| SqlError::Parameter("GeoJSON has no `coordinates`".into()))?;
    let bad = |e: serde_json::Error| SqlError::Parameter(format!("GeoJSON coordinates: {e}"));
    Ok(match ty {
        "Point" => {
            let p: [f64; 2] = serde_json::from_value(coordinates).map_err(bad)?;
            Geom::Point(p[0], p[1])
        }
        "LineString" => Geom::LineString(serde_json::from_value(coordinates).map_err(bad)?),
        "Polygon" => Geom::Polygon(serde_json::from_value(coordinates).map_err(bad)?),
        "MultiPoint" => Geom::MultiPoint(serde_json::from_value(coordinates).map_err(bad)?),
        "MultiLineString" => {
            Geom::MultiLineString(serde_json::from_value(coordinates).map_err(bad)?)
        }
        "MultiPolygon" => Geom::MultiPolygon(serde_json::from_value(coordinates).map_err(bad)?),
        other => {
            return Err(SqlError::unsupported(format!(
                "GeoJSON type `{other}`: the stored geometries are Point, LineString, Polygon and their Multi forms (docs/core/SPATIAL_FUNCTIONS.md)"
            )))
        }
    })
}
