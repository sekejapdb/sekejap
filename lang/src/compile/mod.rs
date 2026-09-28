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
    SqlRow, SqlValue, Tier, ID_COLUMN, KEY_COLUMN,
};
use sekejap_core::collections::{
    Accumulator, AggValue, AggregateFn, AggregateInput, AggregateRequest,
    CandidateDriver, CollectionId, CollectionOptions, Database, Direction, DropMode,
    DropPhase, EntityId, Geom, GeometryFilter, GraphContextId,
    Cmp, ColumnRule, DefaultValue, GroupCmp, GroupKey, GroupOrder, GroupPredicate, GroupRow, IndexExpr,
    IndexFamily, IndexId,
    IndexInfo, IndexState, OwnedScalarValue, PointFilter, ProjectedValue, Projection, QueryBudget,
    QueryFilter, QueryOrder, QueryRequest, QueryRow, ScalarFilter, ScalarValue, ScoreExpr,
    SortDirection, SortKey, SortValue, TextAnalyzer, TextMatch, UpdatePatch, VectorMetric, WriteAction,
    WriteCursor, WriteRequest,
};
use sekejap_core::spatial_io;
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

/// Set (or, `None`, clear) the `ef_search` knob for the rest of this
/// thread's transaction: what `SET LOCAL ef_search` does when it RUNS, and
/// what [`end_transaction`] undoes.
pub(crate) fn set_ef_search(ef: Option<usize>) {
    EF_SEARCH.with(|cell| cell.set(ef));
}

/// The end of a transaction on this thread, however it ends: `COMMIT` or
/// `ROLLBACK` as SQL, or a caller's own commit, rollback or drop of a
/// transaction handle. `SET LOCAL` lasts until here, as in PostgreSQL.
pub fn end_transaction() {
    set_ef_search(None);
}

/// The `ef_search` knob as this thread holds it now. A GQL execution reads
/// it when it OPENS, so a cached GQL plan follows the transaction it runs in
/// (GQL profile Q29).
pub(crate) fn ef_search() -> Option<usize> {
    EF_SEARCH.with(Cell::get)
}

// `super::parser` is reached by path from `predicates.rs`; the name has
// to be bound here for `super::` to find it one level down.
use super::parser;

mod aggregate;
mod bind;
// Shared with a GQL body's host forms, which read their literals when an
// execution opens (`gql/host.rs`).
pub(crate) use bind::{geom_with, tsquery_of};
mod boolean;
mod ddl;
mod dml;
pub(crate) mod edges;
mod plan;
mod predicates;
mod row;
mod rows;
mod select;
// `functions.rs` carries the §4.1 / §4.2 range rewrites. The module is
// named `range_rewrites` because the name `functions` is already taken
// here by the `super::functions` import above.
#[path = "functions.rs"]
mod range_rewrites;

use bind::*;
use plan::*;
use row::*;

pub(crate) use aggregate::AggregatePlan;
pub(crate) use bind::{Binder, Rebind};
pub(crate) use plan::{GqlSqlPlan, Plan, Returning, SelectPlan, WritePlan};
pub(crate) use row::bytea_text;
pub(crate) use rows::RowsPlan;

// ── the compiler ──────────────────────────────────────────────────────────

/// What the `WHERE` gave `search_score()` to score.
#[derive(Clone, Debug)]
enum SearchLeaf {
    One { index: IndexId, query: String },
    /// Two or more `search()` predicates in one statement. There is one
    /// `search_score()` spelling and no way to say WHICH predicate it means,
    /// so it is refused rather than bound to whichever compiled last.
    Several,
}

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
    /// This statement's `search(col, 'query')` leaf, recorded while the
    /// `WHERE` compiles so that `search_score()` -- which carries neither a
    /// column nor a query -- can resolve against it afterwards. `None` means
    /// the statement has no `search()`, and then `search_score()` is REFUSED
    /// by name instead of returning a number that means nothing.
    search_leaf: Option<SearchLeaf>,
    /// Why this statement cannot be REFILLED with new parameters, collected
    /// while it compiles. Empty means every `$n` landed in a typed slot.
    /// `RefCell` because the value readers that record a fold take `&self`.
    rebind: std::cell::RefCell<bind::Rebind>,
}

/// Compile one statement, and say whether the compiled form can be REFILLED
/// with new parameters without being compiled again.
pub(crate) fn compile(
    db: &Database,
    statement: Stmt,
    params: &[super::Param],
    notices: &mut Vec<String>,
    budget: QueryBudget,
    cancelled: &mut dyn FnMut() -> bool,
) -> SqlResult2<(Plan, bind::Rebind)> {
    let mut compiler = Compiler {
        index_lists: std::cell::RefCell::new(Vec::new()),
        rewrites: Vec::new(),
        row_functions: Vec::new(),
        search_leaf: None,
        clock: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX)),
        db,
        params,
        notices,
        budget,
        cancelled,
        rebind: std::cell::RefCell::new(bind::Rebind::default()),
    };
    let plan = compiler.statement(statement)?;
    Ok((plan, compiler.rebind.into_inner()))
}

impl Compiler<'_> {
    /// Bind and plan a GQL relation and the outer SELECT over it. The
    /// statement's parameters are kept, not folded: an execution reads them
    /// when it opens.
    fn gql(&mut self, graph: &crate::gql::ast::GqlGraphTable) -> SqlResult2<GqlSqlPlan> {
        let plan = crate::gql::plan::GqlPlan::compile(self.db, graph, self.notices)?;
        Ok(GqlSqlPlan {
            plan,
            params: self.params.to_vec(),
        })
    }

    fn statement(&mut self, statement: Stmt) -> SqlResult2<Plan> {
        Ok(match statement {
            // A GQL relation, with the outer SELECT over it, is ONE GQL plan
            // (design §5.5). A `FROM` that names a CATALOG relation takes
            // the `Rows` driver: the statement is an ordinary SELECT whose
            // candidates are a bounded list the compiler builds here
            // (`rows.rs`).
            Stmt::Gql(graph) => Plan::Gql(self.gql(&graph)?),
            Stmt::ExplainGql(graph) => Plan::ExplainGql(self.gql(&graph)?),
            Stmt::Select(select) if self.edge_source(&select)?.is_some() => {
                let (c, table, edge) = self.edge_source(&select)?.expect("checked by the guard");
                Plan::EdgeRows(self.edge_select(c, &table, &edge, &select)?)
            }
            Stmt::Explain(select) if self.edge_source(&select)?.is_some() => {
                let (c, table, edge) = self.edge_source(&select)?.expect("checked by the guard");
                Plan::ExplainText(self.edge_select(c, &table, &edge, &select)?.render())
            }
            Stmt::Select(select) => match Self::catalog_source(&select) {
                Some(relation) => Plan::Rows(self.catalog_select(relation, *select, false)?),
                None => match self.aggregate(&select)? {
                    Some(plan) => Plan::Aggregate(plan),
                    None => Plan::Select(self.select(*select)?),
                },
            },
            Stmt::Explain(select) => match Self::catalog_source(&select) {
                Some(relation) => Plan::Rows(self.catalog_select(relation, *select, true)?),
                None => match self.aggregate(&select)? {
                    Some(plan) => Plan::ExplainAggregate(plan),
                    None => Plan::Explain(self.select(*select)?),
                },
            },
            Stmt::SessionRows(items) => Plan::Rows(self.session_rows(&items)?),
            Stmt::Show(show) => Plan::Rows(self.show(&show)?),
            Stmt::SetGuc { name, value } => Plan::Write(self.set_guc(&name, &value)),
            Stmt::Insert {
                table,
                columns,
                rows,
                on_conflict,
                returning,
            } => match self.edge_table_of(&table)? {
                Some((c, edge)) => {
                    if !returning.is_empty() {
                        return Err(SqlError::unsupported(format!(
                            "INSERT INTO {table} ... RETURNING: `{table}` is an edge table, whose rows are edges with no key to return; RETURNING is answered for a table of rows"
                        )));
                    }
                    Plan::Write(self.insert_edges(c, &table, &edge, columns, &rows, on_conflict)?)
                }
                None => {
                    if columns.is_empty() {
                        return Err(SqlError::unsupported(format!(
                            "INSERT INTO {table} VALUES with no column list: name the columns, `_key` among them"
                        )));
                    }
                    Plan::Write(self.insert(&table, &columns, &rows, on_conflict, &returning)?)
                }
            },
            Stmt::Update { table, .. } | Stmt::Delete { table, .. }
                if self.edge_table_of(&table)?.is_some() =>
            {
                return Err(SqlError::unsupported(format!(
                    "`{table}` is an edge table and has no `_key`: name an end in the WHERE (docs/core/EDGE_TABLES.md §4)"
                )));
            }
            Stmt::UpdateWhere {
                table,
                assignments,
                predicates,
            } if self.edge_table_of(&table)?.is_some() => {
                let (c, edge) = self.edge_table_of(&table)?.expect("checked by the guard");
                Plan::Write(self.update_edges(c, &table, &edge, &assignments, &predicates)?)
            }
            Stmt::DeleteWhere {
                table: Some(table),
                predicates,
                cascade,
            } if self.edge_table_of(&table)?.is_some() => {
                let (c, edge) = self.edge_table_of(&table)?.expect("checked by the guard");
                Plan::Write(self.delete_edges(c, &table, &edge, &predicates, cascade)?)
            }
            Stmt::PropertyGraph {
                name,
                mode,
                vertex_tables,
                edge_tables,
                alters,
            } => Plan::Write(self.property_graph(name, mode, vertex_tables, edge_tables, alters)?),
            Stmt::DropPropertyGraph { name, if_exists } => {
                Plan::Write(WritePlan::DropPropertyGraph { name, if_exists })
            }
            Stmt::AddUnique {
                table,
                name,
                columns,
            } => Plan::Write(self.add_unique(&table, name, &columns)?),
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
                primary_key,
                unique,
                if_not_exists,
                indexes,
                ..
            } if !primary_key.is_empty() || columns.iter().any(|c| c.references.is_some()) => {
                if !unique.is_empty() || columns.iter().any(|c| c.unique) {
                    return Err(SqlError::unsupported(format!(
                        "UNIQUE on edge table `{table}`: an edge table's uniqueness is its PRIMARY KEY (docs/core/EDGE_TABLES.md §3)"
                    )));
                }
                Plan::Write(self.create_edge_table(table, columns, primary_key, if_not_exists, &indexes)?)
            }
            Stmt::CreateTable {
                table,
                columns,
                unique,
                if_not_exists,
                indexes,
                automatic,
                ..
            } => Plan::Write(self.create_table(
                table,
                columns,
                unique,
                if_not_exists,
                &indexes,
                &automatic,
            )?),
            Stmt::CreateIndex {
                name,
                table,
                method,
                unique,
            } => Plan::Write(self.create_index(name, &table, method, unique)?),
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
            Stmt::Reindex {
                target,
                concurrently,
            } => {
                let mut collections = Vec::new();
                match &target {
                    ReindexTarget::System => {
                        return Err(SqlError::coded(
                            "0A000",
                            "REINDEX SYSTEM: there are no system catalogs to rebuild",
                        ))
                    }
                    ReindexTarget::Index(name) => {
                        let Some(index) = self.index_named(name)? else {
                            return Err(SqlError::coded(
                                "42704",
                                format!(r#"index "{name}" does not exist"#),
                            ));
                        };
                        collections.push(self.db.index_info(index).map_err(SqlError::from)?.collection);
                    }
                    ReindexTarget::Table(table) => {
                        let c = crate::collection(self.db, table).map_err(|_| {
                            SqlError::coded("42P01", format!(r#"relation "{table}" does not exist"#))
                        })?;
                        collections.push(c);
                    }
                    ReindexTarget::Schema(schema) => {
                        if !self.db.schema_exists(schema).map_err(SqlError::from)? {
                            return Err(SqlError::coded(
                                "3F000",
                                format!(r#"schema "{schema}" does not exist"#),
                            ));
                        }
                        for (s, table) in self.db.list_qualified_collections().map_err(SqlError::from)? {
                            if s == *schema {
                                if let Some(c) = self.db.collection_in(&s, &table).map_err(SqlError::from)? {
                                    collections.push(c);
                                }
                            }
                        }
                    }
                    ReindexTarget::Database => {
                        for (s, table) in self.db.list_qualified_collections().map_err(SqlError::from)? {
                            if let Some(c) = self.db.collection_in(&s, &table).map_err(SqlError::from)? {
                                collections.push(c);
                            }
                        }
                    }
                }
                let mut indexes = Vec::new();
                for &c in &collections {
                    for info in self.db.list_indexes(c).map_err(SqlError::from)? {
                        let named = match &target {
                            ReindexTarget::Index(name) => info.name == *name,
                            _ => true,
                        };
                        // A crashed run's leftovers are finished, not rebuilt.
                        if named && info.state == IndexState::Ready && !info.name.starts_with("__reindex_") {
                            indexes.push(info.id);
                        }
                    }
                }
                Plan::Write(WritePlan::Reindex {
                    collections,
                    indexes,
                    notice: concurrently.then(|| {
                        "REINDEX CONCURRENTLY: every rebuild already runs beside the old index, so the word changes nothing".to_owned()
                    }),
                })
            }
            // pg_trgm's index is built in (0.19 A1); its similarity
            // functions and operators are not, and are refused where they
            // are written. Any other extension is not available.
            Stmt::CreateExtension { name } => {
                if name.eq_ignore_ascii_case("pg_trgm") {
                    return Ok(Plan::Write(WritePlan::Notice(
                        "CREATE EXTENSION pg_trgm: the trigram index is built in, so nothing was installed -- `CREATE INDEX ... USING gin (col gin_trgm_ops)` narrows LIKE and ILIKE; similarity() and the % and <-> operators are not built".into(),
                    )));
                }
                return Err(SqlError::coded(
                    "0A000",
                    format!(r#"extension "{name}" is not available"#),
                ));
            }
            Stmt::CreateSchema {
                name,
                if_not_exists,
            } => {
                if self.db.schema_exists(&name).map_err(SqlError::from)? {
                    if if_not_exists {
                        return Ok(Plan::Write(WritePlan::Notice(format!(
                            "CREATE SCHEMA IF NOT EXISTS {name}: the schema is already there, so nothing was created"
                        ))));
                    }
                    return Err(SqlError::engine(format!("schema `{name}` already exists")));
                }
                Plan::Write(WritePlan::CreateSchema { name })
            }
            Stmt::DropSchema { name, if_exists } => {
                if !self.db.schema_exists(&name).map_err(SqlError::from)? {
                    if if_exists {
                        return Ok(Plan::Write(WritePlan::Notice(format!(
                            "DROP SCHEMA IF EXISTS {name}: no such schema"
                        ))));
                    }
                    return Err(SqlError::engine(format!("no schema named `{name}`")));
                }
                Plan::Write(WritePlan::DropSchema { name })
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
    /// A client GUC a driver sent on connect.
    ///
    /// Accepted as a NOTICE that names the knob and the value. There is
    /// nothing to set: a connection is a process here and the session holds
    /// no settings object, so a silent `SET` would read as one that took
    /// effect. `SHOW <name>` answers the same knob from the constant in
    /// `parser/catalog.rs`, which is the value this engine actually has --
    /// `client_encoding` is `UTF8` because text is stored as UTF-8 and there
    /// is no other encoding, not because a `SET` said so.
    /// The edge table a `SELECT`'s `FROM` names, if it names one.
    fn edge_source(
        &self,
        select: &SelectStmt,
    ) -> SqlResult2<Option<(CollectionId, String, sekejap_core::collections::EdgeTable)>> {
        let Source::Table(table) = &select.source else {
            return Ok(None);
        };
        if Self::catalog_source(select).is_some() {
            return Ok(None);
        }
        Ok(self.edge_table_of(table)?.map(|(c, edge)| (c, table.clone(), edge)))
    }

    fn set_guc(&mut self, name: &str, value: &str) -> WritePlan {
        match crate::parser::client_guc(name) {
            Some(have) => WritePlan::Notice(format!(
                "SET {name} = {value}: accepted and not stored. A connection is a process here and there is no session settings table, so this engine's {name} is and stays `{have}`; `SHOW {name}` reports it"
            )),
            None => WritePlan::Notice(format!(
                "SET {name} = {value}: accepted and not stored. `{name}` is not a knob this engine has, and nothing was changed"
            )),
        }
    }

    fn set_local(&mut self, name: &str, value: &Literal) -> SqlResult2<WritePlan> {
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            "ef_search" | "hnsw.ef_search" | "diskann.query_search_list_size" => {
                // `= DEFAULT` clears the knob, which is what RESET means and
                // what COMMIT does at the end of a transaction.
                if matches!(value, Literal::Str(text) if text.eq_ignore_ascii_case("default")) {
                    return Ok(WritePlan::SetEf(None, format!(
                        "SET LOCAL {name} = DEFAULT: the approximate shortlist bound is cleared, so a vector order takes the exact family when the column has one"
                    )));
                }
                let ef = self.i64_of(value)?;
                if ef <= 0 {
                    return Err(SqlError::unsupported(format!(
                        "SET LOCAL {name} = {ef}: an approximate shortlist has a positive width"
                    )));
                }
                Ok(WritePlan::SetEf(Some(ef as usize), format!(
                    "SET LOCAL {name} = {ef}: read as `ef`, the approximate shortlist bound of QueryOrder::ApproximateVector (QL_CONTRACT §4.5; the two knobs are not the same algorithm -- battle50k deviation 12)"
                )))
            }
            _ => Ok(WritePlan::Notice(format!(
                "SET LOCAL {name}: no such knob here, and nothing was changed. The planner has no enable_indexscan / enable_bitmapscan: a query's driver is chosen from the indexes the statement's own predicates name (`src/query/drivers.rs`), and exactness is which vector index the column has, not a planner setting"
            ))),
        }
    }

    // ── values ───────────────────────────────────────────────────────────
    //
    // Two doors. The readers below FOLD: they turn a written value into the
    // plan's own typed form and, when that value was a `$n`, record the fold
    // so the statement is not rebindable -- a folded parameter cannot be
    // refilled, because what it produced is no longer a value but a shape.
    // The slot-producing helpers in `bind.rs` go through `Binder` directly
    // and record a SLOT instead. The default is therefore the safe one: a
    // construct nobody taught to rebind refuses.

    /// The value reader with no compiler state: the same code a REBIND runs.
    pub(super) fn binder(&self) -> bind::Binder<'_> {
        bind::Binder::new(self.db, self.params)
    }

    /// Record that a written `$n` was folded into the plan's shape, with the
    /// construct that folded it.
    pub(super) fn folds(&self, literal: &Literal, what: &str) {
        if bind::literal_is_bound(literal) {
            let slot = match literal {
                Literal::Param(n) => format!("${n}"),
                _ => "a scalar subquery's key".to_owned(),
            };
            self.folds_reason(format!("{slot} is folded at prepare by {what}"));
        }
    }

    /// Record a refusal that is not about one written value.
    pub(super) fn folds_reason(&self, reason: String) {
        let mut rebind = self.rebind.borrow_mut();
        if !rebind.refusals.contains(&reason) {
            rebind.refusals.push(reason);
        }
    }

    /// A literal, as JSON. A subquery runs here, at compile time, because by
    /// the time the outer statement runs it is a constant.
    fn value_of(&self, literal: &Literal) -> SqlResult2<Value> {
        self.folds(literal, "a value read into the plan");
        self.binder().value_of(literal)
    }

    /// [`Compiler::value_of`] without the fold record: a TYPE TEST that does
    /// not decide the plan on its own.
    fn peek_value(&self, literal: &Literal) -> SqlResult2<Value> {
        self.binder().value_of(literal)
    }

    fn text_of(&self, literal: &Literal) -> SqlResult2<String> {
        self.folds(literal, "text read into the plan");
        self.binder().text_of(literal)
    }

    fn f64_of(&self, literal: &Literal) -> SqlResult2<f64> {
        self.folds(literal, "a number read into the plan");
        self.binder().f64_of(literal)
    }

    fn i64_of(&self, literal: &Literal) -> SqlResult2<i64> {
        self.folds(literal, "a whole number read into the plan");
        self.binder().i64_of(literal)
    }

    /// pgvector's text form `[a,b,c]`, a JSON array, or a bound
    /// `Param::Vector`.
    fn vector_of(&self, literal: &Literal) -> SqlResult2<Vec<f32>> {
        self.folds(literal, "a vector read into the plan");
        self.binder().vector_of(literal)
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
            // A trigram index is a text-family index whose terms are pieces:
            // a word query, a phrase or BM25 is never answered from it.
            if info.analyzer == Some(TextAnalyzer::Trigram) {
                continue;
            }
            if info.field == field && info.family == family && info.expression == expression {
                match info.state {
                    IndexState::Ready => return Ok(info.id),
                    IndexState::Building { .. } => building = true,
                    IndexState::Dropping => {}
                }
            }
        }
        if building {
            return Err(SqlError::engine(format!(
                "{what} on `{field}` is still building; run it to READY before a query can use it"
            )));
        }
        // NOT `SqlError::Engine`. "there is no index that can answer this
        // predicate" is a NAMED refusal -- QL_CONTRACT §6 wrote the sentence
        // and §7 item 9 wrote the code -- and an `Engine` error reaches a
        // PostgreSQL client as `XX000 internal_error`, which is the one code
        // a client RETRIES. `Unsupported` carries the same sentence and is
        // `0A000 feature_not_supported`, the code the contract's own §8 block
        // claims for it. Raised here, at the one chokepoint every index
        // family passes through, so scalar, expression, text, point,
        // geometry and vector all say it the same way.
        Err(SqlError::unsupported(format!(
            "{what} on `{field}` does not exist. QL_CONTRACT §6: every Tier-1 predicate on an indexed field is answered index-side, so the predicate compiles to a filter that NAMES an index; without one there is nothing to name"
        )))
    }

    /// Resolve a bare index name over the whole catalog.
    ///
    /// This used to probe index ids 1..=64 and stop, which silently failed to
    /// find an index in any database that had ever allocated more than
    /// sixty-four of them: an id is not reused when an index is dropped, so a
    /// long-lived database walks past the ceiling and `DROP INDEX <name>`
    /// then reported that a present index did not exist. It now walks the
    /// catalog itself, which is the only thing that knows what exists.
    fn index_named(&self, name: &str) -> SqlResult2<Option<IndexId>> {
        for (schema, collection) in self.db.list_qualified_collections().map_err(SqlError::from)? {
            let Some(c) = self.db.collection_in(&schema, &collection).map_err(SqlError::from)? else {
                continue;
            };
            for info in self.db.list_indexes(c).map_err(SqlError::from)? {
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

    /// The READY trigram index over `field` (`gin_trgm_ops`), if there is
    /// one: a `LIKE` without one checks rows, which is a stated cost.
    fn trigram_index_opt(&self, c: CollectionId, field: &str) -> SqlResult2<Option<IndexId>> {
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
                    && info.family == IndexFamily::Text
                    && info.analyzer == Some(TextAnalyzer::Trigram)
                    && info.state == IndexState::Ready
            })
            .map(|info| info.id))
    }
}

/// pgvector's text form: `[a, b, c]`.
pub(crate) fn parse_vector_literal(text: &str) -> SqlResult2<Vec<f32>> {
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

/// A geometry written as TEXT, read the way PostgreSQL reads a `geometry`
/// literal: a GeoJSON document, the hex EWKB a geometry column prints (with
/// or without `\x`), or WKT / EWKT. An SRID the text names must be 4326.
pub(crate) fn geom_from_text(text: &str) -> SqlResult2<Geom> {
    let trimmed = text.trim();
    if trimmed.starts_with('{') {
        let document = serde_json::from_str::<Value>(trimmed)
            .map_err(|e| SqlError::Parameter(format!("GeoJSON: {e}")))?;
        return geom_from_json(&document);
    }
    let hex = trimmed.strip_prefix("\\x").unwrap_or(trimmed);
    if !hex.is_empty() && hex.bytes().all(|c| c.is_ascii_hexdigit()) {
        return geom_from_wkb_hex(trimmed);
    }
    geom_from_wkt(trimmed)
}

/// Hex WKB or EWKB, as `ST_GeomFromWKB` / `ST_GeomFromEWKB` read it.
pub(crate) fn geom_from_wkb_hex(text: &str) -> SqlResult2<Geom> {
    let bytes = spatial_io::from_hex(text.trim())
        .map_err(|e| SqlError::Parameter(format!("WKB: {e}")))?;
    let decoded =
        spatial_io::from_wkb(&bytes).map_err(|e| SqlError::Parameter(format!("WKB: {e}")))?;
    srid_4326(decoded.srid)?;
    Ok(decoded.geometry)
}

/// WKT or EWKT, as `ST_GeomFromText` / `ST_GeomFromEWKT` read it.
pub(crate) fn geom_from_wkt(text: &str) -> SqlResult2<Geom> {
    let decoded =
        spatial_io::from_wkt(text).map_err(|e| SqlError::Parameter(format!("WKT: {e}")))?;
    srid_4326(decoded.srid)?;
    Ok(decoded.geometry)
}

/// Storage is WGS84: an SRID a value names must be 4326, and a value that
/// names none is read as the column's.
fn srid_4326(srid: Option<i32>) -> SqlResult2<()> {
    match srid {
        None | Some(4326) => Ok(()),
        Some(other) => Err(SqlError::unsupported(format!(
            "a geometry of SRID {other}: storage is WGS84 (SRID 4326); ST_Transform is Tier 2"
        ))),
    }
}

/// A shape as the GeoJSON document a geometry column stores. Built from the
/// doubles themselves, not from printed text, so every coordinate -- a
/// negative zero included -- is stored exactly as it was read.
pub(crate) fn geom_to_json(geom: &Geom) -> Value {
    let (kind, coordinates) = match geom {
        Geom::Point(x, y) => ("Point", serde_json::json!([x, y])),
        Geom::LineString(c) => ("LineString", serde_json::json!(c)),
        Geom::Polygon(c) => ("Polygon", serde_json::json!(c)),
        Geom::MultiPoint(c) => ("MultiPoint", serde_json::json!(c)),
        Geom::MultiLineString(c) => ("MultiLineString", serde_json::json!(c)),
        Geom::MultiPolygon(c) => ("MultiPolygon", serde_json::json!(c)),
    };
    serde_json::json!({ "type": kind, "coordinates": coordinates })
}

/// A GeoJSON geometry, as `kernel::spatial::Geom`.
pub(crate) fn geom_from_json(value: &Value) -> SqlResult2<Geom> {
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
