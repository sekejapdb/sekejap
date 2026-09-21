//! The SQL thin slice: a parser and a compiler for the Tier-1 statements of
//! `docs/lang/QL_CONTRACT.md`, an `EXPLAIN` that prints the plan the engine
//! actually built, and nothing else.
//!
//! What this module is NOT: a second execution engine. Every statement here
//! compiles to a call the crate already has -- `Database::prepare_query`,
//! `put`, `delete`, `create_collection`, `create_*_index`,
//! `begin_drop_collection` / `drop_collection_step` -- so text queries
//! and code queries run through one engine and cost the same, minus the parse.
//!
//! What it refuses: every construct `docs/lang/QL_CONTRACT.md` places in Tier 2 or
//! Tier 3. A refusal names the keyword, the tier and the atomic that is not
//! built (`refuse.rs`). Nothing is emulated: the eighth law of
//! `docs/core/FOUNDATION_TEST_STANDARD.md` is that a construct with no atomic is
//! REFUSED with a named reason, and a parser is exactly where that is decided.
//!
//! ## The grammar actually accepted
//!
//! ```text
//! statement := select | explain | insert | update | delete
//!            | create_table | create_index | drop | transaction | set_local
//!            | bulk
//!
//! select    := SELECT [DISTINCT] items FROM source [WHERE conj]
//!              [GROUP BY group] [HAVING having] [ORDER BY key] [LIMIT n]
//! explain   := EXPLAIN select | EXPLAIN drop_table
//!            | EXPLAIN update | EXPLAIN delete   -- predicated only; the
//!                                                   plan is PREPARED, never
//!                                                   run
//! items     := '*' | item (',' item)*
//! item      := '_id' | '_key' | name | name '/' n | aggregate
//!            | order_expression [AS alias]
//! aggregate := ('count' '(' ('*' | name) ')')
//!            | ('sum'|'min'|'max'|'avg') '(' name ')'
//! group     := name ['/' n]        -- one key; `/ n` needs an Int index
//! having    := aggregate cmp value (AND aggregate cmp value)*
//! source    := name | graph_table | ALL   -- ALL is refused by name (§2)
//! conj      := predicate (AND predicate)*
//!
//! predicate := name cmp value
//!            | name BETWEEN value AND value
//!            | name IS [NOT] NULL | name IS MISSING
//!            | '_key' cmp value | '_key' BETWEEN value AND value
//!            | to_tsvector('simple', name) '@@' to_tsquery('simple', value)
//!            | ST_DWithin(name, geo, value [, true])
//!            | ST_Intersects(name, geo) | ST_Within(name, geo)
//!            | ST_Contains(name, geo)
//! cmp       := '=' | '<>' | '<' | '<=' | '>' | '>='
//! geo       := ST_MakePoint(v, v) | ST_SetSRID(geo, 4326)
//!            | ST_MakeEnvelope(v, v, v, v [, 4326])
//!            | ST_GeomFromGeoJSON(value) | value        [ '::' geography|geometry ]
//! value     := number | string | TRUE | FALSE | NULL | '$' n
//!            | '(' SELECT name FROM name WHERE '_key' '=' value ')'
//!
//! key       := name [ASC|DESC]
//!            | name '<->' geo [ASC|DESC]
//!            | name ('<=>'|'<->'|'<#>') value [ASC|DESC]
//!            | ts_rank_cd(to_tsvector('simple', name),
//!                         to_tsquery('simple', value)) [ASC|DESC]
//!            | bm25(name, value) [ASC|DESC]
//!            | expression [ASC|DESC]
//! expression := arithmetic over bm25(), 1 - (name '<=>' value),
//!               ST_Distance(name, geo), scalar names and numbers
//!
//! graph_table := GRAPH_TABLE '(' name MATCH
//!                  '(' name label WHERE name '.' '_key' '=' value ')'
//!                  hop
//!                  '(' name label ')'
//!                COLUMNS '(' (name '.' name [AS alias]) (',' ...)* ')' ')'
//! hop       := '-' ['[' [name] label ']'] ('->' | '-') [quantifier]
//!            | '<-' ['[' [name] label ']'] '-' [quantifier]
//! quantifier:= '{' n [',' [n]] '}' | '+' | '?'
//! label     := ':' name | IS name
//!
//! insert    := INSERT INTO name '(' names ')' VALUES '(' values ')'
//!                                                  (',' '(' values ')')*
//! update    := UPDATE name SET (name '=' set_value)+ WHERE where_tail
//! delete    := DELETE FROM (name | ALL) [WHERE conj] [RESTRICT | CASCADE]
//! set_value := value | expression        -- a row function over the SAME row
//! where_tail:= '_key' '=' value          -- one point write
//!            | conj                      -- Database::update_where /
//!                                           delete_where, a bounded
//!                                           resumable pass over the
//!                                           candidates the predicate admits
//! bulk      := BEGIN BULK | END BULK     -- OPS_CONTRACT §7
//! create_table := CREATE TABLE name '(' (name type [PRIMARY KEY])+ ')'
//! type      := TEXT | INT | BIGINT | REAL | DOUBLE PRECISION | BOOLEAN
//!            | JSONB | TIMESTAMPTZ | DATE | VECTOR '(' n ')'
//!            | GEOMETRY ['(' Point|Polygon|... [',' 4326] ')']
//! create_index := CREATE [UNIQUE] INDEX name ON name USING method '(' ... ')'
//! method    := btree(name) | gin(to_tsvector('simple', name)) | gist(name)
//!            | exact(name) | quantized(name [vector_cosine_ops])
//!            | hnsw|diskann|ivfflat(name [vector_cosine_ops])
//! drop      := drop_table | DROP INDEX [IF EXISTS] name
//! drop_table:= DROP TABLE [IF EXISTS] name [CASCADE | RESTRICT]
//! transaction := BEGIN [READ ONLY] | COMMIT | ROLLBACK
//! set_local := SET [LOCAL] name '=' value
//! ```

mod ast;
mod compile;
mod explain;
mod functions;
mod lexer;
mod parser;
mod refuse;

use sekejap_core::collections::{
    CollectionId, Database, EntityId, Error, OrderValue, PreparedAggregate, PreparedQuery,
    ProjectedValue, QueryBudget, QueryError, QueryPage, QueryWork,
};
use serde_json::Value;
use std::fmt;

pub(crate) use compile::{AggregatePlan, SelectPlan};

/// The E4 row identity, as a SELECT list writes it. `Database::put` maps an
/// external key onto it, and a `QueryRow` carries it without reading a row.
pub const ID_COLUMN: &str = "_id";
/// The external key, as a statement writes it. It is a declared field of
/// every layout (`collections::KEY_FIELD`), so projecting it reads the row;
/// naming it in a WHERE is the key-order driver's own range.
pub const KEY_COLUMN: &str = "_key";
/// The deepest a `{n,}` or `+` quantifier walks when the statement writes no
/// upper bound. The traversal is bounded by contract; this is the bound.
pub const MAX_GRAPH_DEPTH: usize = 16;
/// The page a `Database::sql` SELECT assembles its answer from.
const PAGE: usize = 8192;
/// Bounds every traversal a `GRAPH_TABLE` compiles to, so a pattern cannot
/// ask for unbounded work. `GRAPH_CONTRACT` section 4 makes the bound the
/// caller's; a statement that writes none gets these.
const GRAPH_VISITED: usize = 1 << 16;
const GRAPH_EDGES: usize = 1 << 18;
const GRAPH_RESULTS: usize = 1 << 16;

pub(crate) fn is_key_column(name: &str) -> bool {
    name == KEY_COLUMN
}

/// The tier a refused construct sits in, per `docs/lang/QL_CONTRACT.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// A Phase-3 item whose atomic is named and in build order.
    Two,
    /// Refused for good: no atomic, and none accepted.
    Three,
}

impl Tier {
    pub fn number(self) -> u8 {
        match self {
            Self::Two => 2,
            Self::Three => 3,
        }
    }
}

#[derive(Clone, Debug)]
pub enum SqlError {
    /// A Tier-2 or Tier-3 construct, named, with the contract's reason.
    Refused {
        keyword: String,
        tier: Tier,
        reason: &'static str,
    },
    /// The text does not spell a Tier-1 statement.
    Syntax { message: String, at: usize },
    /// A Tier-1 shape this engine cannot serve, with the reason.
    Unsupported(String),
    /// A parameter is missing, or is not the type its position needs.
    Parameter(String),
    /// The statement is well formed but the database refuses it -- a missing
    /// collection, a missing index, a budget.
    Engine(String),
}

impl SqlError {
    pub(crate) fn syntax(message: impl fmt::Display, at: usize) -> Self {
        Self::Syntax {
            message: message.to_string(),
            at,
        }
    }

    pub(crate) fn unsupported(message: impl fmt::Display) -> Self {
        Self::Unsupported(message.to_string())
    }

    pub(crate) fn engine(message: impl fmt::Display) -> Self {
        Self::Engine(message.to_string())
    }

    /// The tier of a refusal, for a caller that wants to count them.
    pub fn tier(&self) -> Option<Tier> {
        match self {
            Self::Refused { tier, .. } => Some(*tier),
            _ => None,
        }
    }

    /// The reason text a refusal carries. Never empty for a refusal.
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Self::Refused { reason, .. } => Some(reason),
            _ => None,
        }
    }
}

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused {
                keyword,
                tier,
                reason,
            } => write!(
                f,
                "refused: `{keyword}` is Tier {} -- {reason}",
                tier.number()
            ),
            Self::Syntax { message, at } => write!(f, "syntax error at byte {at}: {message}"),
            Self::Unsupported(message) => write!(f, "unsupported: {message}"),
            Self::Parameter(message) => write!(f, "parameter: {message}"),
            Self::Engine(message) => write!(f, "engine: {message}"),
        }
    }
}

impl std::error::Error for SqlError {}

impl From<Error> for SqlError {
    fn from(value: Error) -> Self {
        Self::Engine(value.to_string())
    }
}

impl From<QueryError> for SqlError {
    fn from(value: QueryError) -> Self {
        Self::Engine(value.to_string())
    }
}

pub(crate) type SqlResult2<T> = std::result::Result<T, SqlError>;
pub type Result<T> = std::result::Result<T, SqlError>;

/// A bound `$n` value. Its SQL type comes from where it is used, not from
/// how it was built: a `Text` in a vector position is read as a pgvector
/// literal, a `Text` in a geometry position as GeoJSON.
#[derive(Clone, Debug, PartialEq)]
pub enum Param {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Vector(Vec<f32>),
    Json(Value),
}

/// One projected value.
#[derive(Clone, Debug, PartialEq)]
pub enum SqlValue {
    /// The field is not present in this row at all.
    Missing,
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Json(Value),
    /// The E4 row identity, for `_id`.
    Id(EntityId),
}

#[derive(Clone, Debug, PartialEq)]
pub struct SqlRow {
    pub id: EntityId,
    pub values: Vec<SqlValue>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SqlResult {
    Rows {
        columns: Vec<String>,
        rows: Vec<SqlRow>,
    },
    Affected(u64),
    Explain(String),
    Notice(String),
}

/// A statement, parsed and compiled against one database's catalog.
///
/// Compilation is the expensive half and it is separable: a caller that runs
/// the same shape many times prepares once and pages many times, which is
/// what the `e4-sql` arm of `battle50k` does.
pub struct PreparedSql {
    plan: compile::Plan,
    notices: Vec<String>,
}

impl PreparedSql {
    /// Notices the compiler raised: a `SET LOCAL` this engine does not have,
    /// an index method accepted as an alias, a post-filter where the contract
    /// promises a per-hop prune.
    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    /// The columns a SELECT returns, in order. Empty for anything else.
    pub fn columns(&self) -> &[String] {
        match &self.plan {
            compile::Plan::Select(select) | compile::Plan::Explain(select) => &select.columns,
            compile::Plan::Aggregate(aggregate) | compile::Plan::ExplainAggregate(aggregate) => {
                &aggregate.columns
            }
            _ => &[],
        }
    }

    pub fn is_select(&self) -> bool {
        matches!(
            &self.plan,
            compile::Plan::Select(_) | compile::Plan::Aggregate(_)
        )
    }

    /// True when this statement folds rows into groups rather than returning
    /// them: an aggregate function, `GROUP BY`, `HAVING` or `DISTINCT`.
    pub fn is_aggregate(&self) -> bool {
        matches!(
            &self.plan,
            compile::Plan::Aggregate(_) | compile::Plan::ExplainAggregate(_)
        )
    }

    pub(crate) fn select_plan(&self) -> Option<&SelectPlan> {
        match &self.plan {
            compile::Plan::Select(select) | compile::Plan::Explain(select) => Some(select),
            _ => None,
        }
    }

    pub(crate) fn aggregate_plan(&self) -> Option<&AggregatePlan> {
        match &self.plan {
            compile::Plan::Aggregate(aggregate) | compile::Plan::ExplainAggregate(aggregate) => {
                Some(aggregate)
            }
            _ => None,
        }
    }

    /// Hand the compiled SELECT to `body` as a live `PreparedQuery`, so the
    /// caller pages it itself.
    ///
    /// The plan owns every value the request borrows -- the term strings, the
    /// query vector, the geometries -- and a `QueryOrder::Score` tree is a
    /// tree of REFERENCES, so it is built on the stack inside this call and
    /// cannot outlive it. That is why this is a callback rather than a
    /// returned cursor.
    pub fn with_query<T>(
        &self,
        db: &Database,
        body: &mut dyn FnMut(&mut PreparedQuery<'_>) -> Result<T>,
    ) -> Result<T> {
        let select = self.select_plan().ok_or_else(|| {
            SqlError::unsupported("this statement is not a SELECT and has no prepared query")
        })?;
        select.with_query(db, body)
    }

    /// Hand the compiled aggregate to `body` as a live [`PreparedAggregate`],
    /// so the caller pages the groups itself. A callback for the same reason
    /// [`PreparedSql::with_query`] is one: the request borrows what the plan
    /// owns.
    pub fn with_aggregate<T>(
        &self,
        db: &Database,
        body: &mut dyn FnMut(&mut PreparedAggregate<'_>) -> Result<T>,
    ) -> Result<T> {
        let aggregate = self.aggregate_plan().ok_or_else(|| {
            SqlError::unsupported(
                "this statement does not fold rows and has no prepared aggregate",
            )
        })?;
        aggregate.with_aggregate(db, body)
    }

    /// Page a compiled SELECT and hand each ASSEMBLED row to `body`.
    ///
    /// [`PreparedSql::with_query`] hands out the engine's own `QueryRow`,
    /// which carries projected FIELDS; a caller that wants the statement's
    /// own columns -- a `_key`, this statement's ranking value, a §4.1 / §4.2
    /// ROW FUNCTION over the projected values -- needs the row the select
    /// list describes, and that is what this produces. It is the paging half
    /// of [`Database::sql`] without the `Vec` of every row at the end, so a
    /// caller can stream an answer larger than it wants to hold.
    pub fn for_each_row(
        &self,
        db: &Database,
        page_rows: usize,
        body: &mut dyn FnMut(&SqlRow) -> Result<()>,
    ) -> Result<()> {
        let select = self.select_plan().ok_or_else(|| {
            SqlError::unsupported("this statement is not a row SELECT and pages no rows")
        })?;
        select.with_query(db, &mut |prepared| {
            loop {
                let page = prepared.next_page(page_rows, QueryBudget::unlimited(), || false)?;
                for row in &page.rows {
                    body(&select.row(db, row)?)?;
                }
                if page.done || page.rows.is_empty() {
                    break;
                }
            }
            Ok(())
        })
    }

    /// Run a compiled aggregate to exhaustion, in pages of groups.
    fn groups(&self, db: &Database) -> Result<SqlResult> {
        let aggregate = self
            .aggregate_plan()
            .ok_or_else(|| SqlError::unsupported("not an aggregate"))?;
        let columns = aggregate.columns.clone();
        let rows = aggregate.with_aggregate(db, &mut |prepared| {
            let mut out = Vec::new();
            loop {
                let page = prepared.next_page(PAGE, QueryBudget::unlimited(), || false)?;
                for group in &page.groups {
                    out.push(aggregate.row(group));
                }
                if page.done || page.groups.is_empty() {
                    break;
                }
            }
            Ok(out)
        })?;
        Ok(SqlResult::Rows { columns, rows })
    }

    /// Run a compiled SELECT to exhaustion, in pages.
    fn rows(&self, db: &Database) -> Result<SqlResult> {
        if self.is_aggregate() {
            return self.groups(db);
        }
        let select = self
            .select_plan()
            .ok_or_else(|| SqlError::unsupported("not a SELECT"))?;
        let columns = select.columns.clone();
        let rows = select.with_query(db, &mut |prepared| {
            let mut out = Vec::new();
            loop {
                let page = prepared.next_page(PAGE, QueryBudget::unlimited(), || false)?;
                for row in &page.rows {
                    out.push(select.row(db, row)?);
                }
                if page.done || page.rows.is_empty() {
                    break;
                }
            }
            Ok(out)
        })?;
        Ok(SqlResult::Rows { columns, rows })
    }
}

/// The Tier-2/Tier-3 table of `docs/lang/QL_CONTRACT.md` as this parser holds it:
/// one row per construct, with the tier and the reason a refusal carries.
///
/// Public so a caller -- and `lang/tests/sql_refusals.rs` -- can enumerate what is
/// refused without writing a statement for each, and so the table can be
/// printed next to the contract it is copied from.
pub fn refusals() -> &'static [(&'static str, Tier, &'static str)] {
    refuse::TABLE
}

/// The reason a §4.2 rewrite whose pre-image is a SET of scalar ranges
/// carries. Public so a caller -- and `lang/tests/sql_functions.rs` -- can name it
/// instead of matching on the text.
pub use refuse::MULTI_RANGE as MULTI_RANGE_REASON;

/// Parse `text` and compile it against `db`'s catalog.
pub fn prepare_sql(db: &Database, text: &str, params: &[Param]) -> Result<PreparedSql> {
    prepare_sql_with(db, text, params, QueryBudget::unlimited(), &mut || false)
}

/// [`prepare_sql`] under the caller's own budget and cancellation.
///
/// Compiling is not always free: `EXISTS (...)` and `key IN (SELECT ...)`
/// build their membership set while the statement is compiled, and that set
/// is an edge-keyspace walk or a whole inner query. A caller that wants those
/// bounded, or wants `Ctrl-C` to reach them, hands the budget and the cancel
/// in here; [`prepare_sql`] is this with `QueryBudget::unlimited()` and a
/// cancel that never fires, which is what the SQL layer used to do with no
/// way to say otherwise.
pub fn prepare_sql_with(
    db: &Database,
    text: &str,
    params: &[Param],
    budget: QueryBudget,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<PreparedSql> {
    let statement = parser::parse(text)?;
    let mut notices = Vec::new();
    let plan = compile::compile(db, statement, params, &mut notices, budget, cancelled)?;
    Ok(PreparedSql { plan, notices })
}

/// `EXPLAIN <select>` without the `EXPLAIN` keyword: prepare, run, and print
/// the plan together with the work the run charged.
///
/// This entry point runs the statement to explain it, so it takes the two
/// families that can be run for an answer -- a SELECT and an aggregate -- and
/// nothing else. The EXPLAIN families that must NOT be run to be explained
/// (`DROP TABLE` and the predicated `UPDATE`/`DELETE`) go through
/// `SqlDatabase::sql` with the `EXPLAIN` keyword, which prepares and
/// describes without executing; the refusal below names that route rather
/// than leaving a caller with "EXPLAIN takes a SELECT" in front of a
/// statement that does have an EXPLAIN.
pub fn explain_sql(db: &Database, text: &str, params: &[Param]) -> Result<String> {
    let prepared = prepare_sql(db, text, params)?;
    if let Some(aggregate) = prepared.aggregate_plan() {
        return explain::render_aggregate(db, aggregate, prepared.notices());
    }
    let select = prepared.select_plan().ok_or_else(|| {
        SqlError::unsupported(
            "explain_sql/sql_explain RUNS the statement to explain it, so it takes a SELECT or an aggregate. A DROP TABLE or a predicated UPDATE/DELETE is explained WITHOUT being run: `db.sql(\"EXPLAIN <statement>\")`, which prepares and describes it.",
        )
    })?;
    explain::render(db, select, prepared.notices())
}

/// The `Database::sql` family.
///
/// These were inherent methods of `Database` while the SQL slice lived in the
/// same crate as the engine. `Database` now belongs to `sekejap-core` and the
/// orphan rule forbids an inherent `impl` on it from here, so the same four
/// methods are an extension trait instead. Names, signatures and behaviour
/// are unchanged; a caller adds `use sekejap_lang::SqlDatabase;` and nothing
/// else.
pub trait SqlDatabase {
    /// Run one SQL statement.
    ///
    /// Deviation from the brief's signature: this takes `&mut self`. A
    /// Tier-1 statement list includes INSERT, UPDATE, DELETE and DDL, and
    /// every one of those goes through `Database::put` / `delete` /
    /// `create_*`, which take `&mut Database` because there is one writer.
    /// A read-only caller that wants a shared borrow uses [`prepare_sql`]
    /// and [`PreparedSql::with_query`], which take `&Database`.
    fn sql(&mut self, text: &str, params: &[Param]) -> Result<SqlResult>;

    /// [`SqlDatabase::sql`] under the caller's own budget and cancellation,
    /// which reach the semi-join set a statement builds while it COMPILES.
    /// See [`prepare_sql_with`].
    fn sql_with(
        &mut self,
        text: &str,
        params: &[Param],
        budget: QueryBudget,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Result<SqlResult>;

    /// Parse and compile without running. See [`prepare_sql`].
    fn sql_prepare(&self, text: &str, params: &[Param]) -> Result<PreparedSql>;

    /// The plan and the work counters for one SELECT. See [`explain_sql`].
    fn sql_explain(&self, text: &str, params: &[Param]) -> Result<String>;
}

impl SqlDatabase for Database {
    fn sql(&mut self, text: &str, params: &[Param]) -> Result<SqlResult> {
        self.sql_with(text, params, QueryBudget::unlimited(), &mut || false)
    }

    fn sql_with(
        &mut self,
        text: &str,
        params: &[Param],
        budget: QueryBudget,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Result<SqlResult> {
        let statement = parser::parse(text)?;
        let mut notices = Vec::new();
        // A write compiles and runs in one step: its plan borrows the
        // database immutably while it resolves names, and the write needs the
        // mutable borrow afterwards.
        match compile::compile(self, statement, params, &mut notices, budget, cancelled)? {
            compile::Plan::Select(select) => {
                let prepared = PreparedSql {
                    plan: compile::Plan::Select(select),
                    notices,
                };
                prepared.rows(self)
            }
            compile::Plan::Aggregate(aggregate) => {
                let prepared = PreparedSql {
                    plan: compile::Plan::Aggregate(aggregate),
                    notices,
                };
                prepared.groups(self)
            }
            compile::Plan::Explain(select) => {
                let text = explain::render(self, &select, &notices)?;
                Ok(SqlResult::Explain(text))
            }
            compile::Plan::ExplainText(text) => Ok(SqlResult::Explain(text)),
            compile::Plan::ExplainAggregate(aggregate) => {
                let text = explain::render_aggregate(self, &aggregate, &notices)?;
                Ok(SqlResult::Explain(text))
            }
            compile::Plan::Write(write) => write.run(self, notices, budget),
        }
    }

    fn sql_prepare(&self, text: &str, params: &[Param]) -> Result<PreparedSql> {
        prepare_sql(self, text, params)
    }

    fn sql_explain(&self, text: &str, params: &[Param]) -> Result<String> {
        explain_sql(self, text, params)
    }
}

/// A projected value, as SQL reads it.
pub(crate) fn projected(value: &ProjectedValue) -> SqlValue {
    match value {
        ProjectedValue::Missing => SqlValue::Missing,
        ProjectedValue::Null => SqlValue::Null,
        ProjectedValue::Value(value) => match value {
            Value::Null => SqlValue::Null,
            Value::Bool(b) => SqlValue::Bool(*b),
            Value::Number(n) => match n.as_i64() {
                Some(i) => SqlValue::Int(i),
                None => SqlValue::Float(n.as_f64().unwrap_or(f64::NAN)),
            },
            Value::String(s) => SqlValue::Text(s.clone()),
            other => SqlValue::Json(other.clone()),
        },
    }
}

/// The ranking value of a row, as SQL reads it.
pub(crate) fn order_value(value: &OrderValue) -> SqlValue {
    match value {
        OrderValue::EntityId | OrderValue::Driver => SqlValue::Null,
        OrderValue::Scalar(scalar) => match scalar {
            sekejap_core::collections::OwnedScalarValue::Nullish => SqlValue::Null,
            sekejap_core::collections::OwnedScalarValue::Bool(b) => SqlValue::Bool(*b),
            sekejap_core::collections::OwnedScalarValue::I64(i) => SqlValue::Int(*i),
            sekejap_core::collections::OwnedScalarValue::F64(f) => SqlValue::Float(*f),
            sekejap_core::collections::OwnedScalarValue::Text(t) => SqlValue::Text(t.clone()),
        },
        OrderValue::Distance(d) | OrderValue::Bm25(d) | OrderValue::Score(d) => SqlValue::Float(*d),
        // A reaching-edge ranking whose property the bag does not carry has
        // no number to report, which is SQL's NULL.
        OrderValue::Edge(value) => value.map_or(SqlValue::Null, SqlValue::Float),
    }
}

/// Everything a page charged, summed across the pages one statement ran.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RunWork {
    pub(crate) work: QueryWork,
    pub(crate) rows: u64,
    pub(crate) pages: u64,
}

impl RunWork {
    /// One aggregate page's work. `groups` is a HIGH-WATER mark of live
    /// accumulator sets, not a running total, so it is maxed rather than
    /// summed -- the same reason the budget is a memory bound.
    pub(crate) fn add_groups(&mut self, page: &sekejap_core::collections::GroupPage) {
        let w = &page.work;
        let total = &mut self.work;
        total.candidates += w.candidates;
        total.primary_reads += w.primary_reads;
        total.row_decodes += w.row_decodes;
        total.scalar_postings += w.scalar_postings;
        total.graph_edges += w.graph_edges;
        total.graph_visited += w.graph_visited;
        total.spatial_postings += w.spatial_postings;
        total.text_postings += w.text_postings;
        total.text_tokens += w.text_tokens;
        total.vector_locators += w.vector_locators;
        total.vector_sidecars += w.vector_sidecars;
        total.vector_lanes += w.vector_lanes;
        total.key_postings += w.key_postings;
        total.groups = total.groups.max(w.groups);
        total.output_bytes += w.output_bytes;
        self.rows += page.groups.len() as u64;
        self.pages += 1;
    }

    pub(crate) fn add(&mut self, page: &QueryPage) {
        let w = &page.work;
        let total = &mut self.work;
        total.candidates += w.candidates;
        total.primary_reads += w.primary_reads;
        total.row_decodes += w.row_decodes;
        total.scalar_postings += w.scalar_postings;
        total.graph_edges += w.graph_edges;
        total.graph_visited += w.graph_visited;
        total.spatial_postings += w.spatial_postings;
        total.text_postings += w.text_postings;
        total.text_tokens += w.text_tokens;
        total.vector_locators += w.vector_locators;
        total.vector_sidecars += w.vector_sidecars;
        total.vector_lanes += w.vector_lanes;
        total.key_postings += w.key_postings;
        total.groups = total.groups.max(w.groups);
        total.output_bytes += w.output_bytes;
        self.rows += page.rows.len() as u64;
        self.pages += 1;
    }
}

/// The collection a name refers to, or an error that names it.
pub(crate) fn collection(db: &Database, name: &str) -> SqlResult2<CollectionId> {
    db.collection(name)
        .map_err(SqlError::from)?
        .ok_or_else(|| SqlError::engine(format!("no collection named `{name}`")))
}
