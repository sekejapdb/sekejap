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
use super::{
    collection, is_key_column, order_value, projected, refuse, SqlError, SqlResult, SqlResult2,
    SqlRow, SqlValue, Tier, GRAPH_EDGES, GRAPH_RESULTS, GRAPH_VISITED, ID_COLUMN, KEY_COLUMN,
    MAX_GRAPH_DEPTH,
};
use crate::collections::{
    BfsRequest, CandidateDriver, CollectionId, CollectionOptions, Database, Direction, Geom,
    GeometryFilter, GraphContextId, IndexFamily, IndexId, IndexInfo, IndexState, PointFilter,
    Projection,
    QueryFilter, QueryOrder, QueryRequest, QueryRow, ScalarFilter, ScalarValue, ScoreExpr,
    SortDirection, TextMatch, VectorMetric,
};
use crate::spatial_math::{Bounds, Point};
use crate::Kind;
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

// ── the compiled plan ─────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Scalar {
    Bool(bool),
    I64(i64),
    F64(f64),
    Text(String),
}

impl Scalar {
    fn borrowed(&self) -> ScalarValue<'_> {
        match self {
            Self::Bool(b) => ScalarValue::Bool(*b),
            Self::I64(i) => ScalarValue::I64(*i),
            Self::F64(f) => ScalarValue::F64(*f),
            Self::Text(t) => ScalarValue::Text(t),
        }
    }
}

fn borrow_bound(bound: &Bound<Scalar>) -> Bound<ScalarValue<'_>> {
    match bound {
        Bound::Included(v) => Bound::Included(v.borrowed()),
        Bound::Excluded(v) => Bound::Excluded(v.borrowed()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

fn borrow_key_bound(bound: &Bound<String>) -> Bound<&str> {
    match bound {
        Bound::Included(v) => Bound::Included(v.as_str()),
        Bound::Excluded(v) => Bound::Excluded(v.as_str()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum OwnedScalarFilter {
    Eq(Scalar),
    Range {
        lower: Bound<Scalar>,
        upper: Bound<Scalar>,
    },
    IsNull,
    IsMissing,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum OwnedFilter {
    Scalar {
        index: IndexId,
        predicate: OwnedScalarFilter,
    },
    Point {
        index: IndexId,
        predicate: PointFilter,
    },
    Geometry {
        index: IndexId,
        predicate: GeometryFilter,
    },
    Text {
        index: IndexId,
        query: String,
        matching: TextMatch,
    },
    Graph(BfsRequest),
    Key {
        lower: Bound<String>,
        upper: Bound<String>,
    },
}

impl OwnedFilter {
    fn borrowed(&self) -> QueryFilter<'_> {
        match self {
            Self::Scalar { index, predicate } => QueryFilter::Scalar {
                index: *index,
                predicate: match predicate {
                    OwnedScalarFilter::Eq(v) => ScalarFilter::Eq(v.borrowed()),
                    OwnedScalarFilter::Range { lower, upper } => ScalarFilter::Range {
                        lower: borrow_bound(lower),
                        upper: borrow_bound(upper),
                    },
                    OwnedScalarFilter::IsNull => ScalarFilter::IsNull,
                    OwnedScalarFilter::IsMissing => ScalarFilter::IsMissing,
                },
            },
            Self::Point { index, predicate } => QueryFilter::Point {
                index: *index,
                predicate: *predicate,
            },
            Self::Geometry { index, predicate } => QueryFilter::Geometry {
                index: *index,
                predicate: predicate.clone(),
            },
            Self::Text {
                index,
                query,
                matching,
            } => QueryFilter::Text {
                index: *index,
                query,
                matching: *matching,
            },
            Self::Graph(request) => QueryFilter::Graph(*request),
            Self::Key { lower, upper } => QueryFilter::Key {
                lower: borrow_key_bound(lower),
                upper: borrow_key_bound(upper),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum OwnedScore {
    Lit(f64),
    Scalar {
        index: IndexId,
    },
    Bm25 {
        index: IndexId,
        query: String,
        matching: TextMatch,
    },
    /// `col <=> v` in an arithmetic ranking is a DISTANCE, and the engine's
    /// leaf is a SIMILARITY (`-distance`). The lowering below writes the
    /// negation, so `1 - (col <=> v)` is the cosine itself, which is the same
    /// quantity `0.5 * (1 + VectorSimilarity)` names in `battle50k`.
    VectorDistance {
        index: IndexId,
        query: Vec<f32>,
        metric: VectorMetric,
    },
    Distance {
        index: IndexId,
        center: Point,
    },
    Add(Box<OwnedScore>, Box<OwnedScore>),
    Sub(Box<OwnedScore>, Box<OwnedScore>),
    Mul(Box<OwnedScore>, Box<OwnedScore>),
    Div(Box<OwnedScore>, Box<OwnedScore>),
    Neg(Box<OwnedScore>),
}

/// Build the borrowed `ScoreExpr` tree on the stack and call `k` with it.
///
/// The tree is references all the way down, so it cannot be returned: every
/// node lives in a frame that encloses the frame of the node above it, and
/// `k` runs in the innermost one. Depth is bounded by `MAX_SCORE_DEPTH`
/// inside `compile_score_expr`, which runs after this and refuses a tree
/// that is too deep or too wide.
fn with_score<R>(node: &OwnedScore, k: &mut dyn FnMut(&ScoreExpr<'_>) -> R) -> R {
    match node {
        OwnedScore::Lit(value) => k(&ScoreExpr::Lit(*value)),
        OwnedScore::Scalar { index } => k(&ScoreExpr::Scalar { index: *index }),
        OwnedScore::Bm25 {
            index,
            query,
            matching,
        } => k(&ScoreExpr::Bm25 {
            index: *index,
            query,
            matching: *matching,
        }),
        OwnedScore::VectorDistance {
            index,
            query,
            metric,
        } => {
            let similarity = ScoreExpr::VectorSimilarity {
                index: *index,
                query,
                metric: *metric,
            };
            k(&ScoreExpr::Neg(&similarity))
        }
        OwnedScore::Distance { index, center } => k(&ScoreExpr::Distance {
            index: *index,
            center: *center,
        }),
        OwnedScore::Neg(inner) => with_score(inner, &mut |e| k(&ScoreExpr::Neg(e))),
        OwnedScore::Add(a, b) => {
            with_score(a, &mut |left| {
                with_score(b, &mut |right| k(&ScoreExpr::Add(left, right)))
            })
        }
        OwnedScore::Sub(a, b) => {
            with_score(a, &mut |left| {
                with_score(b, &mut |right| k(&ScoreExpr::Sub(left, right)))
            })
        }
        OwnedScore::Mul(a, b) => {
            with_score(a, &mut |left| {
                with_score(b, &mut |right| k(&ScoreExpr::Mul(left, right)))
            })
        }
        OwnedScore::Div(a, b) => {
            with_score(a, &mut |left| {
                with_score(b, &mut |right| k(&ScoreExpr::Div(left, right)))
            })
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum OwnedOrder {
    Driver,
    EntityId,
    Scalar {
        index: IndexId,
        direction: SortDirection,
    },
    Distance {
        index: IndexId,
        center: Point,
    },
    Bm25 {
        index: IndexId,
        query: String,
        matching: TextMatch,
    },
    ExactVector {
        index: IndexId,
        query: Vec<f32>,
        metric: VectorMetric,
    },
    ApproximateVector {
        index: IndexId,
        query: Vec<f32>,
        metric: VectorMetric,
        ef: usize,
    },
    Score {
        expr: OwnedScore,
        direction: SortDirection,
    },
}

/// How one output column is filled from a returned row.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Output {
    /// The E4 row identity, which every row carries.
    Id,
    /// The n-th projected field.
    Field(usize),
    /// The external key.
    ///
    /// `Projection` refuses the reserved field the key lives in
    /// (`collections::reserved`, `src/collections/mod.rs`), so there is no
    /// way to ask a page for it; the only atomic that hands a key back is
    /// `Database::get_by_id`, which is one point-get per RETURNED row. That
    /// read is named here rather than hidden: `_id` costs nothing and is
    /// what a caller that already holds a key table should select.
    Key,
    /// This statement's own ranking value.
    OrderValue,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SelectPlan {
    pub(crate) collection: CollectionId,
    pub(crate) columns: Vec<String>,
    pub(crate) outputs: Vec<Output>,
    /// The fields `Projection::Fields` asks for, in order.
    pub(crate) fields: Vec<String>,
    pub(crate) filters: Vec<OwnedFilter>,
    pub(crate) order: OwnedOrder,
    pub(crate) limit: Option<usize>,
    pub(crate) driver: CandidateDriver,
    /// The statement as written, for the EXPLAIN header.
    pub(crate) text: String,
}

impl SelectPlan {
    /// True when a column of this statement is the external key, which is
    /// one point-get per returned row (see [`Output::Key`]).
    pub(crate) fn reads_keys(&self) -> bool {
        self.outputs.iter().any(|output| *output == Output::Key)
    }

    /// One returned row, as this statement's columns.
    pub(crate) fn row(&self, db: &Database, row: &QueryRow) -> SqlResult2<SqlRow> {
        let mut key = None;
        if self.reads_keys() {
            key = db
                .get_by_id(row.id)
                .map_err(SqlError::from)?
                .map(|entity| entity.key);
        }
        let values = self
            .outputs
            .iter()
            .map(|output| match output {
                Output::Id => SqlValue::Id(row.id),
                Output::Field(at) => row
                    .projected
                    .get(*at)
                    .map_or(SqlValue::Missing, |(_, value)| projected(value)),
                Output::Key => key
                    .clone()
                    .map_or(SqlValue::Missing, SqlValue::Text),
                Output::OrderValue => order_value(&row.order),
            })
            .collect();
        Ok(SqlRow { id: row.id, values })
    }

    /// Prepare the query this plan compiled to and hand it to `body`.
    pub(crate) fn with_query<T>(
        &self,
        db: &Database,
        body: &mut dyn FnMut(&mut crate::collections::PreparedQuery<'_>) -> SqlResult2<T>,
    ) -> SqlResult2<T> {
        let filters: Vec<QueryFilter<'_>> = self.filters.iter().map(OwnedFilter::borrowed).collect();
        let fields: Vec<&str> = self.fields.iter().map(String::as_str).collect();
        let projection = if fields.is_empty() {
            Projection::Ids
        } else {
            Projection::Fields(&fields)
        };
        let mut run = |order: QueryOrder<'_>| -> SqlResult2<T> {
            let mut prepared = db.prepare_query(QueryRequest {
                collection: self.collection,
                filters: &filters,
                order,
                projection,
                total_limit: self.limit,
                driver: self.driver,
            })?;
            body(&mut prepared)
        };
        match &self.order {
            OwnedOrder::Driver => run(QueryOrder::Driver),
            OwnedOrder::EntityId => run(QueryOrder::EntityId),
            OwnedOrder::Scalar { index, direction } => run(QueryOrder::Scalar {
                index: *index,
                direction: *direction,
            }),
            OwnedOrder::Distance { index, center } => run(QueryOrder::Distance {
                index: *index,
                center: *center,
                direction: SortDirection::Ascending,
            }),
            OwnedOrder::Bm25 {
                index,
                query,
                matching,
            } => run(QueryOrder::Bm25 {
                index: *index,
                query,
                matching: *matching,
            }),
            OwnedOrder::ExactVector {
                index,
                query,
                metric,
            } => run(QueryOrder::ExactVector {
                index: *index,
                query,
                metric: *metric,
            }),
            OwnedOrder::ApproximateVector {
                index,
                query,
                metric,
                ef,
            } => run(QueryOrder::ApproximateVector {
                index: *index,
                query,
                metric: *metric,
                ef: *ef,
            }),
            OwnedOrder::Score { expr, direction } => with_score(expr, &mut |compiled| {
                run(QueryOrder::Score {
                    expr: compiled,
                    direction: *direction,
                })
            }),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CompiledIndex {
    Scalar { field: String, unique: bool },
    Text { field: String },
    Point { field: String },
    Geometry { field: String },
    ExactVector { field: String },
    QuantizedVector { field: String },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum WritePlan {
    Insert {
        collection: CollectionId,
        rows: Vec<(String, Value)>,
    },
    Update {
        collection: CollectionId,
        key: String,
        patch: Value,
    },
    Delete {
        collection: CollectionId,
        key: String,
    },
    CreateTable {
        name: String,
        fields: Vec<(String, Kind)>,
    },
    CreateIndex {
        collection: CollectionId,
        name: String,
        method: CompiledIndex,
    },
    DropIndex {
        index: IndexId,
        name: String,
    },
    Begin,
    Commit,
    Rollback,
    /// A statement that changed nothing and said so.
    Notice(String),
}

impl WritePlan {
    pub(crate) fn run(self, db: &mut Database, notices: Vec<String>) -> SqlResult2<SqlResult> {
        let notice = |extra: String| -> SqlResult<> {
            let mut all = notices.clone();
            all.push(extra);
            SqlResult::Notice(all.join("; "))
        };
        Ok(match self {
            Self::Insert { collection, rows } => {
                let mut affected = 0u64;
                for (key, document) in rows {
                    db.put(collection, &key, &document)?;
                    affected += 1;
                }
                SqlResult::Affected(affected)
            }
            Self::Update {
                collection,
                key,
                patch,
            } => {
                if db.get(collection, &key)?.is_none() {
                    return Ok(SqlResult::Affected(0));
                }
                db.update(collection, &key, &patch)?;
                SqlResult::Affected(1)
            }
            Self::Delete { collection, key } => {
                let gone = db.delete(collection, &key)?;
                SqlResult::Affected(u64::from(gone))
            }
            Self::CreateTable { name, fields } => {
                db.create_collection(&name, fields, CollectionOptions::default())?;
                db.commit()?;
                SqlResult::Affected(0)
            }
            Self::CreateIndex {
                collection,
                name,
                method,
            } => {
                db.commit()?;
                let id = match &method {
                    CompiledIndex::Scalar { field, unique } => {
                        db.create_scalar_index(collection, &name, field, *unique)?
                    }
                    CompiledIndex::Text { field } => db.create_text_index(collection, &name, field)?,
                    CompiledIndex::Point { field } => {
                        db.create_point_index(collection, &name, field)?
                    }
                    CompiledIndex::Geometry { field } => {
                        db.create_geometry_index(collection, &name, field)?
                    }
                    CompiledIndex::ExactVector { field } => {
                        db.create_exact_vector_index(collection, &name, field)?
                    }
                    CompiledIndex::QuantizedVector { field } => {
                        db.create_quantized_vector_index(collection, &name, field)?
                    }
                };
                // Postgres hands back a usable index; so does this. The build
                // is incremental underneath (`build_index_step`), and it is
                // run to READY here rather than left half-built.
                db.commit()?;
                db.build_index_to_ready(id, 256)?;
                db.commit()?;
                SqlResult::Affected(0)
            }
            Self::DropIndex { index, name } => {
                db.commit()?;
                db.begin_drop_index(index)?;
                while !db.drop_index_step(index, 256)? {}
                db.commit()?;
                let _ = name;
                SqlResult::Affected(0)
            }
            Self::Begin => notice(
                "BEGIN: the writer is single and already inside a transaction; COMMIT ends it"
                    .to_owned(),
            ),
            Self::Commit => {
                db.commit()?;
                EF_SEARCH.with(|ef| ef.set(None));
                SqlResult::Affected(0)
            }
            Self::Rollback => {
                db.rollback()?;
                EF_SEARCH.with(|ef| ef.set(None));
                SqlResult::Affected(0)
            }
            Self::Notice(text) => notice(text),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Plan {
    Select(SelectPlan),
    Explain(SelectPlan),
    Write(WritePlan),
}

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
}

pub(crate) fn compile(
    db: &Database,
    statement: Stmt,
    params: &[super::Param],
    notices: &mut Vec<String>,
) -> SqlResult2<Plan> {
    let mut compiler = Compiler {
        index_lists: std::cell::RefCell::new(Vec::new()),
        db,
        params,
        notices,
    };
    compiler.statement(statement)
}

impl Compiler<'_> {
    fn statement(&mut self, statement: Stmt) -> SqlResult2<Plan> {
        Ok(match statement {
            Stmt::Select(select) => Plan::Select(self.select(*select)?),
            Stmt::Explain(select) => Plan::Explain(self.select(*select)?),
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
            Stmt::CreateTable { table, columns } => Plan::Write(self.create_table(table, columns)?),
            Stmt::CreateIndex {
                name,
                table,
                method,
            } => Plan::Write(self.create_index(name, &table, method)?),
            Stmt::DropTable { table, .. } => {
                return Err(SqlError::unsupported(format!(
                    "DROP TABLE {table}: there is no drop_collection at this HEAD -- `src/collections/mod.rs` creates and alters a collection but has no removal path for its catalog record, its rows, its indexes or its layout, so nothing here can be compiled to one"
                )))
            }
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
            Stmt::Begin => Plan::Write(WritePlan::Begin),
            Stmt::Commit => Plan::Write(WritePlan::Commit),
            Stmt::Rollback => Plan::Write(WritePlan::Rollback),
            Stmt::SetLocal { name, value } => Plan::Write(self.set_local(&name, &value)?),
        })
    }

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
            if info.field == field && info.family == family {
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

    // ── SELECT ───────────────────────────────────────────────────────────

    fn select(&mut self, statement: SelectStmt) -> SqlResult2<SelectPlan> {
        let (c, graph_filter, graph_columns) = match &statement.source {
            Source::Table(name) => (collection(self.db, name)?, None, None),
            Source::Graph(graph) => {
                let (target, filter) = self.graph_table(graph)?;
                (target, Some(filter), Some(graph.columns.clone()))
            }
        };

        let mut filters: Vec<OwnedFilter> = Vec::new();
        if let Some(filter) = graph_filter {
            filters.push(filter);
        }
        for predicate in &statement.predicates {
            filters.push(self.filter(c, predicate)?);
        }

        let order = match &statement.order {
            None => OwnedOrder::Driver,
            Some(key) => self.order(c, key)?,
        };

        // The select list. `_id` is free (a row carries its id); a named
        // column is a projected field; an expression is this statement's own
        // ranking value.
        let items: Vec<(SelectItem, Option<String>)> = match graph_columns {
            Some(columns) => columns
                .into_iter()
                .map(|(item, alias)| (item, Some(alias)))
                .collect(),
            None => statement.items.clone(),
        };
        let mut columns = Vec::new();
        let mut outputs = Vec::new();
        let mut fields: Vec<String> = Vec::new();
        let push_field = |field: String, fields: &mut Vec<String>| -> Output {
            match fields.iter().position(|existing| *existing == field) {
                Some(at) => Output::Field(at),
                None => {
                    fields.push(field);
                    Output::Field(fields.len() - 1)
                }
            }
        };
        for (item, alias) in items {
            match item {
                SelectItem::Star => {
                    for field in self.declared_fields(c)? {
                        columns.push(field.clone());
                        outputs.push(push_field(field, &mut fields));
                    }
                }
                SelectItem::Id => {
                    columns.push(alias.unwrap_or_else(|| ID_COLUMN.to_owned()));
                    outputs.push(Output::Id);
                }
                SelectItem::Key => {
                    columns.push(alias.unwrap_or_else(|| KEY_COLUMN.to_owned()));
                    outputs.push(Output::Key);
                }
                SelectItem::Column(name) => {
                    self.kind_of(c, &name)?;
                    columns.push(alias.unwrap_or_else(|| name.clone()));
                    outputs.push(push_field(name, &mut fields));
                }
                SelectItem::OrderValue(what) => {
                    if statement.order.is_none() {
                        return Err(SqlError::unsupported(format!(
                            "{what} in a select list: the only expression a row can report here is this statement's own ranking value, and this statement has no ORDER BY"
                        )));
                    }
                    columns.push(alias.unwrap_or_else(|| "score".to_owned()));
                    outputs.push(Output::OrderValue);
                }
            }
        }

        if outputs.iter().any(|output| *output == Output::Key) {
            self.notices.push(format!(
                "selecting `{KEY_COLUMN}` costs one `get_by_id` per returned row: a page cannot project the reserved field the external key lives in, so the key is fetched after the walk. `{ID_COLUMN}` is free"
            ));
        }
        let driver = if filters
            .iter()
            .any(|filter| matches!(filter, OwnedFilter::Key { .. }))
        {
            // A key filter is meaningful only under the driver that
            // certifies it from the mapping entry itself.
            CandidateDriver::Keys
        } else {
            CandidateDriver::Auto
        };

        Ok(SelectPlan {
            collection: c,
            columns,
            outputs,
            fields,
            filters,
            order,
            limit: statement.limit,
            driver,
            text: String::new(),
        })
    }

    fn filter(&mut self, c: CollectionId, predicate: &Predicate) -> SqlResult2<OwnedFilter> {
        Ok(match predicate {
            Predicate::Compare { column, op, value } => {
                let kind = self.kind_of(c, column)?;
                let index = self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?;
                let scalar = self.scalar(&kind, value, column)?;
                let predicate = match op {
                    CmpOp::Eq => OwnedScalarFilter::Eq(scalar),
                    CmpOp::Ne => {
                        return Err(SqlError::Refused {
                            keyword: op.written().into(),
                            tier: Tier::Two,
                            reason: "QL_CONTRACT §3: `<>` is the complement of an equality, which is NOT over a membership set; the scalar atomics are Eq, Range, IsNull and IsMissing.",
                        })
                    }
                    CmpOp::Lt => OwnedScalarFilter::Range {
                        lower: Bound::Unbounded,
                        upper: Bound::Excluded(scalar),
                    },
                    CmpOp::Le => OwnedScalarFilter::Range {
                        lower: Bound::Unbounded,
                        upper: Bound::Included(scalar),
                    },
                    CmpOp::Gt => OwnedScalarFilter::Range {
                        lower: Bound::Excluded(scalar),
                        upper: Bound::Unbounded,
                    },
                    CmpOp::Ge => OwnedScalarFilter::Range {
                        lower: Bound::Included(scalar),
                        upper: Bound::Unbounded,
                    },
                };
                OwnedFilter::Scalar { index, predicate }
            }
            Predicate::Between {
                column,
                lower,
                upper,
            } => {
                let kind = self.kind_of(c, column)?;
                let index = self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?;
                OwnedFilter::Scalar {
                    index,
                    predicate: OwnedScalarFilter::Range {
                        lower: Bound::Included(self.scalar(&kind, lower, column)?),
                        upper: Bound::Included(self.scalar(&kind, upper, column)?),
                    },
                }
            }
            Predicate::IsNull { column, negated } => {
                if *negated {
                    return Err(SqlError::Refused {
                        keyword: "IS NOT NULL".into(),
                        tier: Tier::Two,
                        reason: "QL_CONTRACT §3: IS NOT NULL is the complement of IsNull, which is NOT over a membership set. IS NULL and IS MISSING are the Tier-1 nullish atomics.",
                    });
                }
                let index = self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?;
                OwnedFilter::Scalar {
                    index,
                    predicate: OwnedScalarFilter::IsNull,
                }
            }
            Predicate::IsMissing { column } => {
                let index = self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?;
                OwnedFilter::Scalar {
                    index,
                    predicate: OwnedScalarFilter::IsMissing,
                }
            }
            Predicate::KeyCompare { op, value } => {
                let key = self.text_of(value)?;
                let (lower, upper) = match op {
                    CmpOp::Eq => (Bound::Included(key.clone()), Bound::Included(key)),
                    CmpOp::Lt => (Bound::Unbounded, Bound::Excluded(key)),
                    CmpOp::Le => (Bound::Unbounded, Bound::Included(key)),
                    CmpOp::Gt => (Bound::Excluded(key), Bound::Unbounded),
                    CmpOp::Ge => (Bound::Included(key), Bound::Unbounded),
                    CmpOp::Ne => {
                        return Err(SqlError::Refused {
                            keyword: op.written().into(),
                            tier: Tier::Two,
                            reason: "QL_CONTRACT §3: a key `<>` is the complement of a key range, which is NOT over a membership set.",
                        })
                    }
                };
                OwnedFilter::Key { lower, upper }
            }
            Predicate::KeyBetween { lower, upper } => OwnedFilter::Key {
                lower: Bound::Included(self.text_of(lower)?),
                upper: Bound::Included(self.text_of(upper)?),
            },
            Predicate::Text { column, query } => {
                let index = self.index_for(c, column, IndexFamily::Text, "a text index")?;
                let (query, matching) = self.tsquery(query)?;
                OwnedFilter::Text {
                    index,
                    query,
                    matching,
                }
            }
            Predicate::Spatial {
                predicate,
                column,
                argument,
                metres,
            } => self.spatial(c, *predicate, column, argument, metres.as_ref())?,
        })
    }

    fn scalar(&self, kind: &Kind, literal: &Literal, column: &str) -> SqlResult2<Scalar> {
        let value = self.value_of(literal)?;
        Ok(match (kind, &value) {
            (Kind::Int, Value::Number(n)) => Scalar::I64(n.as_i64().ok_or_else(|| {
                SqlError::Parameter(format!("`{column}` is INT and {n} is not a whole number"))
            })?),
            (Kind::Real, Value::Number(n)) => Scalar::F64(n.as_f64().ok_or_else(|| {
                SqlError::Parameter(format!("`{column}` is REAL and {n} is not a number"))
            })?),
            (Kind::Text, Value::String(s)) => Scalar::Text(s.clone()),
            (Kind::Bool, Value::Bool(b)) => Scalar::Bool(*b),
            _ => {
                return Err(SqlError::Parameter(format!(
                    "`{column}` is declared {kind:?} and the value is {value}; a scalar predicate stays inside the index's declared domain rather than coercing (`ScalarFilter`, src/query/mod.rs)"
                )))
            }
        })
    }

    /// The tsquery text, split into E4's `TextMatch`. A tsquery that mixes
    /// `&` and `|` is an AND/OR tree, which is Tier 2.
    fn tsquery(&self, query: &TsQuery) -> SqlResult2<(String, TextMatch)> {
        let text = self.text_of(&query.source)?;
        if !query.tsquery_syntax {
            return Ok((text, TextMatch::Any));
        }
        let trimmed = text.trim();
        if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
            return Ok((
                trimmed[1..trimmed.len() - 1].trim().to_owned(),
                TextMatch::Phrase,
            ));
        }
        if trimmed.contains('\'') {
            let inner = trimmed.trim_matches('\'').trim();
            if inner.split_whitespace().count() > 1 {
                return Ok((inner.to_owned(), TextMatch::Phrase));
            }
        }
        let has_or = trimmed.contains('|');
        let has_and = trimmed.contains('&');
        if has_or && has_and {
            return Err(refuse::refuse("OR"));
        }
        if trimmed.contains('!') {
            return Err(refuse::refuse("NOT"));
        }
        if trimmed.contains("<->") {
            return Err(SqlError::Refused {
                keyword: "tsquery <->".into(),
                tier: Tier::Two,
                reason: "QL_CONTRACT §4.6: a tsquery distance operator is a positional constraint; the Tier-1 phrase atomic is a quoted phrase (TextMatch::Phrase).",
            });
        }
        let separator = if has_or { '|' } else { '&' };
        let terms: Vec<&str> = trimmed
            .split(separator)
            .map(str::trim)
            .filter(|term| !term.is_empty())
            .collect();
        if terms.iter().any(|term| term.contains(':')) {
            return Err(SqlError::Refused {
                keyword: "tsquery weight".into(),
                tier: Tier::Three,
                reason: "QL_CONTRACT §4.6: tsvector weights have no atomic; analyzer v1 stores one weight per token.",
            });
        }
        let matching = if has_or || terms.len() == 1 {
            TextMatch::Any
        } else {
            TextMatch::All
        };
        Ok((terms.join(" "), matching))
    }

    fn spatial(
        &mut self,
        c: CollectionId,
        predicate: SpatialPredicate,
        column: &str,
        argument: &GeoArg,
        metres: Option<&Literal>,
    ) -> SqlResult2<OwnedFilter> {
        let kind = self.kind_of(c, column)?;
        match kind {
            Kind::Point => {
                let index = self.index_for(c, column, IndexFamily::SpatialPoint, "a point index")?;
                match predicate {
                    SpatialPredicate::DWithin => {
                        let GeoArg::Point(point) = argument else {
                            return Err(SqlError::unsupported(
                                "ST_DWithin on a Point column takes a point: PointFilter::Radius is a centre and a radius",
                            ));
                        };
                        let center = self.point_of(point)?;
                        let radius_metres = self.f64_of(metres.ok_or_else(|| {
                            SqlError::syntax("ST_DWithin needs a distance", 0)
                        })?)?;
                        Ok(OwnedFilter::Point {
                            index,
                            predicate: PointFilter::Radius {
                                center,
                                radius_metres,
                            },
                        })
                    }
                    SpatialPredicate::Within => Ok(OwnedFilter::Point {
                        index,
                        predicate: PointFilter::Bbox(self.bounds_of(argument)?),
                    }),
                    other => Err(SqlError::unsupported(format!(
                        "{other:?} on a Point column: the point atomics are PointFilter::Bbox (ST_Within against an envelope) and PointFilter::Radius (ST_DWithin)"
                    ))),
                }
            }
            Kind::Geo => {
                let index =
                    self.index_for(c, column, IndexFamily::SpatialGeometry, "a geometry index")?;
                let geometry = self.geom_of(argument)?;
                let predicate = match predicate {
                    SpatialPredicate::DWithin => GeometryFilter::DWithin {
                        geometry,
                        metres: self.f64_of(metres.ok_or_else(|| {
                            SqlError::syntax("ST_DWithin needs a distance", 0)
                        })?)?,
                    },
                    SpatialPredicate::Intersects => GeometryFilter::Intersects(geometry),
                    SpatialPredicate::Within => GeometryFilter::Within(geometry),
                    SpatialPredicate::Contains => GeometryFilter::Contains(geometry),
                };
                Ok(OwnedFilter::Geometry { index, predicate })
            }
            other => Err(SqlError::unsupported(format!(
                "`{column}` is declared {other:?}; a spatial predicate needs a Point or a Geo column"
            ))),
        }
    }

    // ── ORDER BY ─────────────────────────────────────────────────────────

    fn order(&mut self, c: CollectionId, key: &OrderKey) -> SqlResult2<OwnedOrder> {
        Ok(match key {
            OrderKey::Column { column, descending } => {
                if column == ID_COLUMN {
                    if *descending {
                        return Err(SqlError::unsupported(
                            "ORDER BY _id DESC: QueryOrder::EntityId ascends; the primary tree is walked forwards",
                        ));
                    }
                    return Ok(OwnedOrder::EntityId);
                }
                let index = self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?;
                OwnedOrder::Scalar {
                    index,
                    direction: if *descending {
                        SortDirection::Descending
                    } else {
                        SortDirection::Ascending
                    },
                }
            }
            OrderKey::Distance {
                column,
                point,
                descending,
            } => {
                if *descending {
                    return Err(SqlError::unsupported(
                        "ORDER BY <-> DESC: QueryOrder::Distance refuses Descending at prepare -- there is no reverse KNN walk, and treating DESC as ASC would answer a different question",
                    ));
                }
                let index = self.index_for(c, column, IndexFamily::SpatialPoint, "a point index")?;
                OwnedOrder::Distance {
                    index,
                    center: self.point_of(point)?,
                }
            }
            OrderKey::Vector {
                column,
                query,
                op,
                descending,
            } => {
                if *descending {
                    return Err(SqlError::unsupported(
                        "ORDER BY <=> DESC: a vector order ascends by distance; the nearest is first",
                    ));
                }
                let metric = match op {
                    VecOp::Cosine => VectorMetric::Cosine,
                    VecOp::L2 => VectorMetric::SquaredL2,
                    VecOp::NegativeDot => VectorMetric::NegativeDot,
                };
                let vector = self.vector_of(query)?;
                self.vector_order(c, column, vector, metric)?
            }
            OrderKey::Bm25 {
                column,
                query,
                descending,
            } => {
                if !*descending {
                    self.notices.push(
                        "ORDER BY a text rank ASC: BM25 ranks best-first, so this page returns the WORST matches first"
                            .to_owned(),
                    );
                }
                let index = self.index_for(c, column, IndexFamily::Text, "a text index")?;
                let (query, matching) = self.tsquery(query)?;
                if !*descending {
                    return Err(SqlError::unsupported(
                        "ORDER BY ts_rank_cd(...) ASC: QueryOrder::Bm25 ranks best-first and has no ascending form; write DESC",
                    ));
                }
                OwnedOrder::Bm25 {
                    index,
                    query,
                    matching,
                }
            }
            OrderKey::Score { expr, descending } => OwnedOrder::Score {
                expr: self.score(c, expr)?,
                direction: if *descending {
                    SortDirection::Descending
                } else {
                    SortDirection::Ascending
                },
            },
        })
    }

    /// Exact or approximate, decided by the indexes the column has and by a
    /// `SET LOCAL` the session saw -- never by a planner knob.
    fn vector_order(
        &mut self,
        c: CollectionId,
        column: &str,
        query: Vec<f32>,
        metric: VectorMetric,
    ) -> SqlResult2<OwnedOrder> {
        let indexes = self.db.list_indexes(c).map_err(SqlError::from)?;
        let ready = |family: IndexFamily| -> Option<IndexId> {
            indexes
                .iter()
                .find(|info| {
                    info.field == column && info.family == family && info.state == IndexState::Ready
                })
                .map(|info| info.id)
        };
        let exact = ready(IndexFamily::ExactVector);
        let quantized = ready(IndexFamily::QuantizedVector);
        let ef = EF_SEARCH.with(Cell::get);
        match (exact, quantized, ef) {
            (Some(index), _, None) => Ok(OwnedOrder::ExactVector {
                index,
                query,
                metric,
            }),
            (_, Some(index), ef) => {
                let ef = ef.unwrap_or(DEFAULT_EF);
                self.notices.push(format!(
                    "ORDER BY a vector distance on `{column}` is APPROXIMATE (ef={ef}): the quantized index answers it, and the shortlist bounds the whole result set of this prepared query"
                ));
                Ok(OwnedOrder::ApproximateVector {
                    index,
                    query,
                    metric,
                    ef,
                })
            }
            (Some(index), None, Some(ef)) => {
                self.notices.push(format!(
                    "SET LOCAL ef_search = {ef} was seen but `{column}` has only an exact vector index; the answer is exact and the shortlist bound is unused"
                ));
                Ok(OwnedOrder::ExactVector {
                    index,
                    query,
                    metric,
                })
            }
            (None, None, _) => Err(SqlError::engine(format!(
                "no vector index on `{column}`: a vector order names either the exact family (page-order sidecar scan) or the quantized one (compact scan then f32 rerank)"
            ))),
        }
    }

    fn score(&mut self, c: CollectionId, node: &ScoreNode) -> SqlResult2<OwnedScore> {
        Ok(match node {
            ScoreNode::Lit(value) => OwnedScore::Lit(*value),
            ScoreNode::Column(column) => {
                let kind = self.kind_of(c, column)?;
                if matches!(kind, Kind::Text) {
                    return Err(SqlError::unsupported(format!(
                        "`{column}` is TEXT: a Score leaf is numeric, and a text scalar is refused at prepare (`ScoreExpr::Scalar`, src/query/mod.rs)"
                    )));
                }
                OwnedScore::Scalar {
                    index: self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?,
                }
            }
            ScoreNode::Bm25 { column, query } => {
                let index = self.index_for(c, column, IndexFamily::Text, "a text index")?;
                let (query, matching) = self.tsquery(query)?;
                OwnedScore::Bm25 {
                    index,
                    query,
                    matching,
                }
            }
            ScoreNode::VecDistance { column, query, op } => {
                let index =
                    self.index_for(c, column, IndexFamily::ExactVector, "an exact vector index")?;
                OwnedScore::VectorDistance {
                    index,
                    query: self.vector_of(query)?,
                    metric: match op {
                        VecOp::Cosine => VectorMetric::Cosine,
                        VecOp::L2 => VectorMetric::SquaredL2,
                        VecOp::NegativeDot => VectorMetric::NegativeDot,
                    },
                }
            }
            ScoreNode::Distance { column, point } => OwnedScore::Distance {
                index: self.index_for(c, column, IndexFamily::SpatialPoint, "a point index")?,
                center: self.point_of(point)?,
            },
            ScoreNode::Add(a, b) => OwnedScore::Add(
                Box::new(self.score(c, a)?),
                Box::new(self.score(c, b)?),
            ),
            ScoreNode::Sub(a, b) => OwnedScore::Sub(
                Box::new(self.score(c, a)?),
                Box::new(self.score(c, b)?),
            ),
            ScoreNode::Mul(a, b) => OwnedScore::Mul(
                Box::new(self.score(c, a)?),
                Box::new(self.score(c, b)?),
            ),
            ScoreNode::Div(a, b) => OwnedScore::Div(
                Box::new(self.score(c, a)?),
                Box::new(self.score(c, b)?),
            ),
            ScoreNode::Neg(a) => OwnedScore::Neg(Box::new(self.score(c, a)?)),
        })
    }

    // ── GRAPH_TABLE ──────────────────────────────────────────────────────

    /// A pattern compiles to ONE bounded traversal filter plus the collection
    /// the far element names. The seed is a key equality, which is a point
    /// lookup, not a scan.
    fn graph_table(&mut self, graph: &GraphTable) -> SqlResult2<(CollectionId, OwnedFilter)> {
        let seed_collection = collection(self.db, &graph.seed_collection)?;
        let target = collection(self.db, &graph.target_collection)?;
        let key = self.text_of(&graph.seed_key)?;
        let seed = self
            .db
            .get(seed_collection, &key)
            .map_err(SqlError::from)?
            .ok_or_else(|| {
                SqlError::engine(format!(
                    "GRAPH_TABLE seed: `{}` has no row at key `{key}`",
                    graph.seed_collection
                ))
            })?
            .id;
        let context = self
            .db
            .graph_context(&graph.context)
            .map_err(SqlError::from)?
            .ok_or_else(|| {
                SqlError::engine(format!("no graph context named `{}`", graph.context))
            })?;
        let edge_type = match &graph.hop.edge_type {
            None => None,
            Some(name) => Some(
                self.db
                    .edge_type(name)
                    .map_err(SqlError::from)?
                    .ok_or_else(|| SqlError::engine(format!("no edge type named `{name}`")))?,
            ),
        };
        if graph.hop.max_depth > MAX_GRAPH_DEPTH {
            return Err(SqlError::unsupported(format!(
                "a quantifier of {} hops: a traversal is bounded by contract and this slice's bound is {MAX_GRAPH_DEPTH}",
                graph.hop.max_depth
            )));
        }
        if target != seed_collection {
            self.notices.push(format!(
                "GRAPH_TABLE walks from `{}` into `{}`: the traversal itself is untyped by collection, and the far element's label is checked by the collection the outer statement selects from",
                graph.seed_collection, graph.target_collection
            ));
        }
        self.notices.push(
            "GRAPH_TABLE: a WHERE written after the pattern is a POST-FILTER on completed matches (QL_CONTRACT §4.3, Tier 1). Per-hop pruning -- an inline element WHERE -- is Tier 2 and is refused rather than emulated by this post-filter"
                .to_owned(),
        );
        let request = BfsRequest {
            seed,
            direction: match graph.hop.direction {
                GraphDirection::Outgoing => Direction::Outgoing,
                GraphDirection::Incoming => Direction::Incoming,
                GraphDirection::Both => Direction::Both,
            },
            context: if graph.context.eq_ignore_ascii_case("base") {
                GraphContextId::BASE
            } else {
                context
            },
            edge_type,
            min_depth: graph.hop.min_depth,
            max_depth: graph.hop.max_depth,
            include_seed: false,
            max_visited: GRAPH_VISITED,
            max_edges: GRAPH_EDGES,
            result_limit: GRAPH_RESULTS,
        };
        Ok((target, OwnedFilter::Graph(request)))
    }

    // ── writes ───────────────────────────────────────────────────────────

    fn insert(
        &mut self,
        table: &str,
        columns: &[String],
        values: &[Vec<Literal>],
    ) -> SqlResult2<WritePlan> {
        let c = collection(self.db, table)?;
        let key_at = match columns.iter().position(|name| is_key_column(name)) {
            Some(at) => at,
            None => {
                // With no `_key` column the FIRST column supplies the key,
                // which is where a `TEXT PRIMARY KEY` is written. It must be
                // TEXT, because an external key is a string.
                let first = columns
                    .first()
                    .ok_or_else(|| SqlError::syntax("INSERT names no columns", 0))?;
                if !matches!(self.kind_of(c, first)?, Kind::Text) {
                    return Err(SqlError::unsupported(format!(
                        "INSERT INTO {table} names no `{KEY_COLUMN}` and its first column `{first}` is not TEXT: `Database::put` takes a string key, so either name `{KEY_COLUMN}` or put the TEXT PRIMARY KEY first"
                    )));
                }
                self.notices.push(format!(
                    "INSERT INTO {table}: the external key comes from `{first}`, the first column; the value is ALSO stored as that declared field (battle50k deviation 13)"
                ));
                0
            }
        };
        let mut rows = Vec::with_capacity(values.len());
        for row in values {
            let key = self.text_of(&row[key_at])?;
            let mut document = Map::new();
            for (at, column) in columns.iter().enumerate() {
                if is_key_column(column) {
                    continue;
                }
                let kind = self.kind_of(c, column)?;
                document.insert(column.clone(), self.document_value(kind, &row[at], column)?);
            }
            rows.push((key, Value::Object(document)));
        }
        Ok(WritePlan::Insert {
            collection: c,
            rows,
        })
    }

    fn update(
        &mut self,
        table: &str,
        assignments: &[(String, Literal)],
        key: &Literal,
    ) -> SqlResult2<WritePlan> {
        let c = collection(self.db, table)?;
        let mut patch = Map::new();
        for (column, literal) in assignments {
            if is_key_column(column) {
                return Err(SqlError::unsupported(
                    "UPDATE ... SET _key = ...: the external key is the row's identity; a new key is a new row (INSERT) and the old one is a DELETE",
                ));
            }
            let kind = self.kind_of(c, column)?;
            patch.insert(column.clone(), self.document_value(kind, literal, column)?);
        }
        Ok(WritePlan::Update {
            collection: c,
            key: self.text_of(key)?,
            patch: Value::Object(patch),
        })
    }

    /// One written value, checked against the column's declared `Kind`.
    fn document_value(&self, kind: Kind, literal: &Literal, column: &str) -> SqlResult2<Value> {
        let value = self.value_of(literal)?;
        if value.is_null() {
            return Ok(Value::Null);
        }
        Ok(match kind {
            Kind::Vector(dimensions) => {
                let vector = self.vector_of(literal)?;
                if vector.len() != dimensions {
                    return Err(SqlError::Parameter(format!(
                        "`{column}` is VECTOR({dimensions}) and the value has {} dimension(s)",
                        vector.len()
                    )));
                }
                Value::from(vector)
            }
            Kind::Point | Kind::Geo => {
                let document = match value {
                    Value::String(text) => serde_json::from_str::<Value>(&text)
                        .map_err(|e| SqlError::Parameter(format!("`{column}`: GeoJSON: {e}")))?,
                    other => other,
                };
                // Parsed once here so a bad geometry is refused by the
                // statement rather than by the index maintenance behind it.
                let geom = geom_from_json(&document)?;
                if kind == Kind::Point && !matches!(geom, Geom::Point(_, _)) {
                    return Err(SqlError::Parameter(format!(
                        "`{column}` is GEOMETRY(Point,4326) and the value is not a Point"
                    )));
                }
                document
            }
            Kind::Int => match &value {
                Value::Number(n) if n.is_i64() => value,
                other => {
                    return Err(SqlError::Parameter(format!(
                        "`{column}` is an integer column and the value is {other}"
                    )))
                }
            },
            Kind::Real => match &value {
                Value::Number(_) => value,
                other => {
                    return Err(SqlError::Parameter(format!(
                        "`{column}` is REAL and the value is {other}"
                    )))
                }
            },
            Kind::Text => match &value {
                Value::String(_) => value,
                other => {
                    return Err(SqlError::Parameter(format!(
                        "`{column}` is TEXT and the value is {other}"
                    )))
                }
            },
            Kind::Bool => match &value {
                Value::Bool(_) => value,
                other => {
                    return Err(SqlError::Parameter(format!(
                        "`{column}` is BOOLEAN and the value is {other}"
                    )))
                }
            },
            Kind::Json => value,
        })
    }

    fn create_table(&mut self, table: String, columns: Vec<ColumnDef>) -> SqlResult2<WritePlan> {
        let mut fields = Vec::with_capacity(columns.len());
        let mut keys = 0usize;
        for column in &columns {
            if column.name.starts_with('_') {
                return Err(SqlError::unsupported(format!(
                    "column `{}`: names beginning with `_` are reserved (`_id`, `_key`, `_collection`)",
                    column.name
                )));
            }
            if column.primary_key {
                keys += 1;
                if !matches!(column.kind, Kind::Text) {
                    return Err(SqlError::unsupported(format!(
                        "PRIMARY KEY on `{}`: an external key is a string, so the primary-key column is TEXT",
                        column.name
                    )));
                }
            }
            if column.declared == "TIMESTAMPTZ" || column.declared == "DATE" {
                self.notices.push(format!(
                    "`{}` is declared {} and stored as Kind::Int: UTC microseconds, no time-zone storage (QL_CONTRACT §5 deviation 8)",
                    column.name, column.declared
                ));
            }
            fields.push((column.name.clone(), column.kind.clone()));
        }
        if keys > 1 {
            return Err(SqlError::unsupported(
                "two PRIMARY KEY columns: a row has one external key",
            ));
        }
        if keys == 1 {
            self.notices.push(format!(
                "PRIMARY KEY on `{table}`: the column is stored as a declared field AND supplies the external key `Database::put` maps, so the key is held twice (battle50k deviation 13)"
            ));
        }
        Ok(WritePlan::CreateTable {
            name: table,
            fields,
        })
    }

    fn create_index(
        &mut self,
        name: String,
        table: &str,
        method: IndexMethod,
    ) -> SqlResult2<WritePlan> {
        let c = collection(self.db, table)?;
        let method = match method {
            IndexMethod::Btree(field) => {
                self.kind_of(c, &field)?;
                CompiledIndex::Scalar {
                    field,
                    unique: false,
                }
            }
            IndexMethod::Gin(field) => {
                if !matches!(self.kind_of(c, &field)?, Kind::Text) {
                    return Err(SqlError::unsupported(format!(
                        "gin(to_tsvector('simple', {field})): a text index spans one declared TEXT field (battle50k deviation 1)"
                    )));
                }
                CompiledIndex::Text { field }
            }
            IndexMethod::Gist(field) => match self.kind_of(c, &field)? {
                Kind::Point => CompiledIndex::Point { field },
                Kind::Geo => CompiledIndex::Geometry { field },
                other => {
                    return Err(SqlError::unsupported(format!(
                        "gist({field}): `{field}` is {other:?}; the spatial families index a Point or a Geo column"
                    )))
                }
            },
            IndexMethod::Exact(field) => {
                if !matches!(self.kind_of(c, &field)?, Kind::Vector(_)) {
                    return Err(SqlError::unsupported(format!(
                        "exact({field}): the exact family indexes a VECTOR column"
                    )));
                }
                CompiledIndex::ExactVector { field }
            }
            IndexMethod::Quantized { column, alias } => {
                if !matches!(self.kind_of(c, &column)?, Kind::Vector(_)) {
                    return Err(SqlError::unsupported(format!(
                        "quantized({column}): the quantized family indexes a VECTOR column"
                    )));
                }
                if let Some(alias) = alias {
                    self.notices.push(format!(
                        "USING {} is an alias of `quantized` and builds no new family (QL_CONTRACT §5 deviation 6): a symmetric int8 companion index with an f32 rerank, not a graph",
                        alias.to_ascii_lowercase()
                    ));
                }
                CompiledIndex::QuantizedVector { field: column }
            }
        };
        Ok(WritePlan::CreateIndex {
            collection: c,
            name,
            method,
        })
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
                "GeoJSON type `{other}`: the stored geometries are Point, LineString, Polygon and their Multi forms (docs/SPATIAL_FUNCTIONS.md)"
            )))
        }
    })
}
