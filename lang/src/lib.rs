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
//! statement := select | gql_select | explain | insert | update | delete
//!            | create_table | create_index | drop | transaction | set_local
//!            | bulk
//!
//! select    := SELECT [DISTINCT] items FROM source [WHERE conj]
//!              [GROUP BY group] [HAVING having] [ORDER BY key] [LIMIT n]
//! explain   := EXPLAIN select | EXPLAIN gql_select | EXPLAIN drop_table
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
//! source    := name | ALL
//!                                        -- ALL is refused by name (§2)
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
//! -- The GQL body (docs/lang/GQL_PROFILE_DESIGN.md §5): the only body
//! -- `GRAPH_TABLE` takes (owner decision 1: the SQL/PGQ `GRAPH_TABLE (g
//! -- MATCH ... COLUMNS (...))` body has no compatibility alias, and a
//! -- `COLUMNS` written where this grammar stands is refused by name, naming
//! -- `RETURN`). `SELECT ... FROM gql_table ...` prepares and runs as ONE
//! -- GQL plan (`Plan::Gql`): the outer SELECT is the plan's last stage, its
//! -- select list and clauses GQL expressions over the relation's columns,
//! -- `alias.column` naming one; `DISTINCT`, `HAVING` and a join are refused
//! -- by name. Everything else of the profile is refused by name with its
//! -- milestone (`gql_refusals()`).
//! gql_select:= SELECT ('*' | gexpr [[AS] name] (',' ...)*) FROM gql_table
//!                [WHERE gexpr] [GROUP BY gexpr (',' gexpr)*]
//!                [ORDER BY gexpr [ASC|DESC] (',' ...)*]
//!                [LIMIT count] [OFFSET count]   -- either order; a whole
//!                                                  number key is a position
//! gql_table := GRAPH_TABLE '(' name stage ')' [[AS] name]
//! stage     := (MATCH pattern (',' pattern)* [WHERE gexpr])* RETURN
//!                gexpr [AS name] (',' gexpr [AS name])*
//! pattern   := node (edge node)*
//! node      := '(' [var] [glabel] [WHERE gexpr] ')'
//! edge      := '-[' filler ']->' | '<-[' filler ']-' | '-[' filler ']-'
//!            | '->' | '<-' | '-'
//! filler    := [var] [glabel] [WHERE gexpr]
//! glabel    := (':' | IS) name ('|' name)*
//! gexpr     := gexpr OR gexpr | gexpr AND gexpr | NOT gexpr
//!            | operand [cmp operand] | '(' gexpr ')'
//! operand   := var | var '.' name | number | string | TRUE | FALSE | NULL
//!            | '$' n
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
pub mod catalog;
mod compile;
mod explain;
mod functions;
mod gql;
mod lexer;
pub use lexer::{after_leading_comments, commits_on_its_own, highest_parameter, MAX_PARAMETER};
mod parser;
mod pgcrypto;
mod refuse;

use sekejap_core::collections::{
    CollectionId, Database, EntityId, Error, OrderValue, PreparedAggregate, PreparedQuery,
    ProjectedValue, QueryBudget, QueryError, QueryPage, QueryWork, PUBLIC_SCHEMA,
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
/// The page a `Database::sql` SELECT assembles its answer from.
const PAGE: usize = 8192;

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
    /// An error PostgreSQL raises for the same statement, with PostgreSQL's
    /// five-character SQLSTATE (`22012`, `42703`, ...): a value an evaluation cannot compute (a
    /// division by zero, an integer overflow, a text that does not spell
    /// its type), or a statement that names a variable or a function that
    /// does not exist or gives a value of the wrong kind. The message does
    /// not repeat the code; `Display` adds it.
    Coded {
        sqlstate: &'static str,
        message: String,
    },
}

/// The SQLSTATEs [`SqlError::Coded`] carries, by PostgreSQL's names
/// (PostgreSQL's `errcodes.txt`).
pub(crate) mod sqlstate {
    /// `22003 numeric_value_out_of_range`.
    pub const NUMERIC_VALUE_OUT_OF_RANGE: &str = "22003";
    /// `22000 data_exception`: as pgvector, a distance between vectors of
    /// two widths.
    pub const DATA_EXCEPTION: &str = "22000";
    /// `22007 invalid_datetime_format`.
    pub const INVALID_DATETIME_FORMAT: &str = "22007";
    /// `22008 datetime_field_overflow`.
    pub const DATETIME_FIELD_OVERFLOW: &str = "22008";
    /// `22011 substring_error`.
    pub const SUBSTRING_ERROR: &str = "22011";
    /// `22012 division_by_zero`.
    pub const DIVISION_BY_ZERO: &str = "22012";
    /// `2201E invalid_argument_for_logarithm`.
    pub const INVALID_ARGUMENT_FOR_LOGARITHM: &str = "2201E";
    /// `2201F invalid_argument_for_power_function`.
    pub const INVALID_ARGUMENT_FOR_POWER_FUNCTION: &str = "2201F";
    /// `22P02 invalid_text_representation`.
    pub const INVALID_TEXT_REPRESENTATION: &str = "22P02";
    /// `42601 syntax_error`: as PostgreSQL, the branches of a `UNION` that
    /// return different numbers of columns.
    pub const SYNTAX_ERROR: &str = "42601";
    /// `42P08 ambiguous_parameter`: two uses of one `$n` deduce two types.
    pub const AMBIGUOUS_PARAMETER: &str = "42P08";
    /// `42P10 invalid_column_reference`: a `GROUP BY` or `ORDER BY`
    /// position past the select list.
    pub const INVALID_COLUMN_REFERENCE: &str = "42P10";
    /// `42703 undefined_column`: a variable no statement binds.
    pub const UNDEFINED_COLUMN: &str = "42703";
    /// `42712 duplicate_alias`: a variable bound twice in one stage.
    pub const DUPLICATE_ALIAS: &str = "42712";
    /// `42803 grouping_error`: a grouped `RETURN` reads what it did not group.
    pub const GROUPING_ERROR: &str = "42803";
    /// `42804 datatype_mismatch`: a value of a kind the operation does not
    /// take.
    pub const DATATYPE_MISMATCH: &str = "42804";
    /// `42846 cannot_coerce`: a cast with no rule between the two types.
    pub const CANNOT_COERCE: &str = "42846";
    /// `42883 undefined_function`: an unknown function, or a known one
    /// given the wrong number of arguments.
    pub const UNDEFINED_FUNCTION: &str = "42883";
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

    pub(crate) fn coded(sqlstate: &'static str, message: impl fmt::Display) -> Self {
        Self::Coded {
            sqlstate,
            message: message.to_string(),
        }
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
            Self::Coded { sqlstate, message } => write!(f, "{message} (SQLSTATE {sqlstate})"),
        }
    }
}

impl std::error::Error for SqlError {}

impl From<Error> for SqlError {
    fn from(value: Error) -> Self {
        match value {
            // The data refused the write with PostgreSQL's own SQLSTATE
            // (23505, 23503, 23502), which a client reads as PostgreSQL's.
            Error::Constraint { sqlstate, message } => Self::Coded { sqlstate, message },
            other => Self::Engine(other.to_string()),
        }
    }
}

impl From<QueryError> for SqlError {
    fn from(value: QueryError) -> Self {
        match value {
            QueryError::Database(Error::Constraint { sqlstate, message }) => {
                Self::Coded { sqlstate, message }
            }
            other => Self::Engine(other.to_string()),
        }
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
    /// The stored row this answer row came from. A row of a derived
    /// relation has no single owner and carries [`EntityId::NO_OWNER`];
    /// read [`SqlRow::owner`] rather than this field to tell the two apart.
    pub id: EntityId,
    pub values: Vec<SqlValue>,
}

impl SqlRow {
    /// The stored row this answer row came from, or `None` for a row of a
    /// derived relation, which carries [`EntityId::NO_OWNER`].
    pub fn owner(&self) -> Option<EntityId> {
        (self.id != EntityId::NO_OWNER).then_some(self.id)
    }
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

/// A statement, parsed and compiled against one database's catalog, and
/// REUSABLE: the same compiled form answers again under new parameters.
///
/// Compilation is the expensive half and it is separable. A caller that runs
/// the same shape many times prepares ONCE and then [`PreparedSql::bind`]s
/// each new parameter list -- which parses nothing and, for a statement
/// whose every `$n` landed in a typed slot, compiles nothing either. That is
/// what the `e4-sql` arm of `battle50k` does under `--prepared`, and what
/// the bounded plan cache of `QL_CONTRACT` §2 serves.
///
/// ## What a prepare FOLDS, and what a bind fills
///
/// A prepare folds everything that is a SHAPE rather than a value: the
/// driver, the indexes each predicate is answered from, a §4.1 / §4.2 range
/// rewrite's pre-image, a semi-join's membership set, an INSERT or UPDATE
/// document -- and the CLOCK. `now()` and `current_date` are read once per
/// compiled statement (`compile/functions.rs`), so every row of one answer
/// sees the same instant and a page that resumes does not drift.
///
/// A bind fills the typed slots: a scalar value inside its declared kind, an
/// external key, a tsquery (terms AND `TextMatch`, because `a & b` and
/// `a | b` arrive through one slot), a point centre and radius, a rectangle,
/// a geometry, a query vector, a `GRAPH_TABLE` seed key -- the seed is
/// resolved to its entity id at BIND, one point-get, not at prepare.
///
/// ## When a statement is not rebindable
///
/// A statement whose plan depends on a parameter VALUE is marked
/// `rebind: false` and says why: a semi-join set built while it compiled, a
/// `$n` folded into an index range by a §4.1 / §4.2 rewrite, a document, a
/// session knob, or the folded clock -- which a rebind must take anew, so a
/// statement that reads it is compiled again per execution and therefore
/// gets a NEW clock per execution. [`PreparedSql::bind`] still works for
/// such a statement: it compiles again from the PARSED statement this holds,
/// so the parse is still paid once. `EXPLAIN` prints which of the two a
/// statement is.
pub struct PreparedSql {
    plan: compile::Plan,
    notices: Vec<String>,
    /// The statement as parsed. Kept so a compiled form that cannot be
    /// refilled is COMPILED again without being PARSED again.
    statement: Option<Box<ast::Stmt>>,
    /// Empty when every `$n` of this statement landed in a typed slot.
    rebind: compile::Rebind,
}

impl PreparedSql {
    /// True when new parameters can be written into this compiled form
    /// without compiling it again.
    pub fn rebindable(&self) -> bool {
        self.rebind.ok()
    }

    /// Why a rebind would have to compile again, or `None` when it would
    /// not. One line per construct that folded a value at prepare.
    pub fn rebind_refusal(&self) -> Option<String> {
        self.rebind.reason()
    }

    /// Bind new parameters to this compiled statement.
    ///
    /// For a rebindable statement this writes the new values into the slots
    /// the prepare marked, and nothing is parsed, compiled, or read from the
    /// catalog. For one that is not, it compiles again from the parsed
    /// statement -- still no parse, and a NEW clock, which is what a folded
    /// `now()` requires.
    pub fn bind(&mut self, db: &Database, params: &[Param]) -> Result<()> {
        if self.rebind.ok() {
            let binder = compile::Binder::new(db, params);
            return match &mut self.plan {
                compile::Plan::Select(select) | compile::Plan::Explain(select) => {
                    select.rebind(&binder)
                }
                compile::Plan::Aggregate(aggregate)
                | compile::Plan::ExplainAggregate(aggregate) => aggregate.rebind(&binder),
                // A GQL plan folds no value: the next execution reads the
                // new parameters when it opens (design §7).
                compile::Plan::Gql(gql) | compile::Plan::ExplainGql(gql) => {
                    gql.params = params.to_vec();
                    Ok(())
                }
                // A write folds its document and a notice holds no value, so
                // neither carries a slot; a rebindable one is one with no
                // parameter at all. A `Rows` plan is the same case: its
                // filter ran while it compiled, so a statement with a `$n`
                // in it is marked not rebindable and never reaches here.
                compile::Plan::Write(_)
                | compile::Plan::ExplainText(_)
                | compile::Plan::Rows(_)
                | compile::Plan::EdgeRows(_) => Ok(()),
            };
        }
        let statement = self.statement.clone().ok_or_else(|| {
            SqlError::unsupported(
                "this prepared statement cannot be rebound and did not keep its parsed form",
            )
        })?;
        let mut notices = Vec::new();
        let (plan, rebind) = compile::compile(
            db,
            *statement,
            params,
            &mut notices,
            QueryBudget::unlimited(),
            &mut || false,
        )?;
        self.plan = plan;
        self.notices = notices;
        self.rebind = rebind;
        Ok(())
    }

    /// Notices the compiler raised: a `SET LOCAL` this engine does not have,
    /// an index method accepted as an alias, a post-filter where the contract
    /// promises a per-hop prune.
    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    /// The columns a SELECT -- or an INSERT's `RETURNING` -- returns, in
    /// order. Empty for anything else.
    pub fn columns(&self) -> &[String] {
        match &self.plan {
            compile::Plan::Select(select) | compile::Plan::Explain(select) => &select.columns,
            compile::Plan::Aggregate(aggregate) | compile::Plan::ExplainAggregate(aggregate) => {
                &aggregate.columns
            }
            compile::Plan::Rows(rows) => &rows.columns,
            compile::Plan::EdgeRows(edges) => &edges.columns,
            compile::Plan::Gql(gql) | compile::Plan::ExplainGql(gql) => gql.plan.columns(),
            compile::Plan::Write(compile::WritePlan::Insert { returning, .. }) => &returning.columns,
            _ => &[],
        }
    }

    /// The SQL type of column `at` when a row function decides it -- `BYTEA`
    /// for `ST_AsBinary`, `FLOAT8` for `ST_X` -- and `None` when the column
    /// is a declared field (whose type the catalog holds) or text. A wire
    /// describes a result with this before any row exists.
    ///
    /// A GQL relation types EVERY column here, from its binding schema
    /// (`docs/lang/GQL_PROFILE_DESIGN.md` §6.1): a node property's declared
    /// type, `TEXT` for a property declared differently or not at all, `T[]`
    /// for a list.
    pub fn column_type(&self, at: usize) -> Option<&'static str> {
        match &self.plan {
            compile::Plan::Select(select) | compile::Plan::Explain(select) => {
                select.column_type(at)
            }
            compile::Plan::Gql(gql) => gql.plan.column_type(at),
            _ => None,
        }
    }

    /// The SQL type this statement itself gives each `$n`, in the spellings
    /// [`PreparedSql::column_type`] uses (`TEXT`, `BIGINT`, `TEXT[]`, ...):
    /// entry `i` is `$i+1`, and `None` or a position past the end means the
    /// statement does not decide it. A wire answers `ParameterDescription`
    /// with this for a position its `Parse` left undeclared.
    ///
    /// A collection statement types its `$n` by where each is USED, at
    /// bind, so this is empty for it. A GQL plan has ONE parameter table
    /// for its body and its outer SELECT (`docs/lang/GQL_PROFILE_DESIGN.md`
    /// §7): a `$n` compared with a declared property or a typed column
    /// takes that type, a count is `BIGINT`, a list position `T[]`, and two
    /// uses that disagree are refused at prepare (`42P08`).
    pub fn param_types(&self) -> Vec<Option<&'static str>> {
        match &self.plan {
            compile::Plan::Gql(gql) | compile::Plan::ExplainGql(gql) => gql.plan.param_types(),
            _ => Vec::new(),
        }
    }

    pub fn is_select(&self) -> bool {
        match &self.plan {
            compile::Plan::Select(_)
            | compile::Plan::Aggregate(_)
            | compile::Plan::Gql(_)
            | compile::Plan::EdgeRows(_) => true,
            // A catalog view, a `SHOW` and a `SELECT` with no `FROM` all
            // answer with rows; an `EXPLAIN` of one answers with its plan.
            compile::Plan::Rows(rows) => !rows.explain,
            _ => false,
        }
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
        self.not_gql("prepared query")?;
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
        self.not_gql("prepared aggregate")?;
        let aggregate = self.aggregate_plan().ok_or_else(|| {
            SqlError::unsupported(
                "this statement does not fold rows and has no prepared aggregate",
            )
        })?;
        aggregate.with_aggregate(db, body)
    }

    /// A GQL plan is a pipeline of operators, not one engine query: the
    /// callers that want the engine's own request are refused by name.
    fn not_gql(&self, what: &str) -> Result<()> {
        match &self.plan {
            compile::Plan::Gql(_) | compile::Plan::ExplainGql(_) => Err(SqlError::unsupported(format!(
                "a GQL plan has no single {what}: it is a pipeline of operators; page its rows with `for_each_row_with`"
            ))),
            _ => Ok(()),
        }
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
        self.for_each_row_with(db, page_rows, QueryBudget::unlimited(), &mut || false, body)
    }

    /// [`PreparedSql::for_each_row`] under the caller's own budget and
    /// cancellation, handed to EVERY page rather than only to the compile.
    ///
    /// [`PreparedSql::for_each_row`] is this with `QueryBudget::unlimited()`
    /// and a cancel that never fires, which is all a caller could ask for
    /// until a server had to put a statement of the caller's ON A WIRE: the
    /// PostgreSQL surface (`dist/src/pg/`) owes `statement_timeout`
    /// (`docs/dist/OPS_CONTRACT.md` §3) and `CancelRequest` (§4) on a walk
    /// that produces the STATEMENT's columns, and `ServiceDatabase::scan`
    /// produces the ENGINE's projected fields instead.
    pub fn for_each_row_with(
        &self,
        db: &Database,
        page_rows: usize,
        budget: QueryBudget,
        cancelled: &mut dyn FnMut() -> bool,
        body: &mut dyn FnMut(&SqlRow) -> Result<()>,
    ) -> Result<()> {
        // A catalog relation, `SHOW`, or the session rows: a bounded list
        // built at prepare (`docs/dist/PG_SURFACE.md`), handed out row by row.
        // There is no walk to charge and nothing to cancel between two rows.
        if let compile::Plan::EdgeRows(edges) = &self.plan {
            if let SqlResult::Rows { rows, .. } = edges.answer(db)? {
                for row in &rows {
                    body(row)?;
                }
            }
            return Ok(());
        }
        if let compile::Plan::Rows(rows) = &self.plan {
            if rows.explain {
                return Err(SqlError::unsupported(
                    "an EXPLAIN pages no rows: it is one text answer",
                ));
            }
            if let SqlResult::Rows { rows, .. } = rows.answer() {
                for row in &rows {
                    body(row)?;
                }
            }
            return Ok(());
        }
        // A GQL relation: its rows carry no owner (design Q1), and every
        // page runs under the caller's budget and cancel.
        if let compile::Plan::Gql(gql) = &self.plan {
            return gql.for_each_row(db, page_rows, budget, cancelled, body);
        }
        let select = self.select_plan().ok_or_else(|| {
            SqlError::unsupported("this statement is not a row SELECT and pages no rows")
        })?;
        select.with_query(db, &mut |prepared| {
            loop {
                let page = prepared.next_page(page_rows, budget, &mut *cancelled)?;
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

    /// Page a compiled AGGREGATE under the caller's own budget and
    /// cancellation, handing each folded group to `body` as this statement's
    /// columns.
    ///
    /// The aggregate twin of [`PreparedSql::for_each_row_with`], and here
    /// for the same reason: `PreparedSql::run` folds an aggregate under
    /// `QueryBudget::unlimited()`, so without this a `statement_timeout`
    /// (`docs/dist/OPS_CONTRACT.md` §3) would reach a `COUNT(*)`'s COMPILE
    /// and not its WALK.
    pub fn for_each_group_with(
        &self,
        db: &Database,
        page_rows: usize,
        budget: QueryBudget,
        cancelled: &mut dyn FnMut() -> bool,
        body: &mut dyn FnMut(&SqlRow) -> Result<()>,
    ) -> Result<()> {
        let aggregate = self.aggregate_plan().ok_or_else(|| {
            SqlError::unsupported("this statement does not fold rows and pages no groups")
        })?;
        aggregate.with_aggregate(db, &mut |prepared| {
            loop {
                let page = prepared.next_page(page_rows, budget, &mut *cancelled)?;
                for group in &page.groups {
                    body(&aggregate.row(group))?;
                }
                if page.done || page.groups.is_empty() {
                    break;
                }
            }
            Ok(())
        })
    }

    /// The collection a row SELECT reads, for a caller that must describe the
    /// statement's columns before it runs: the PostgreSQL wire surface types
    /// a `RowDescription` from the collection's DECLARED column types
    /// (`CollectionInfo::declared`), which is the only place a
    /// `TIMESTAMPTZ`, a `GEOMETRY` or a `VECTOR` is distinguishable from the
    /// `Kind` it is stored as.
    ///
    /// `None` for every statement that is not a row SELECT.
    pub fn source_collection(&self) -> Option<CollectionId> {
        match &self.plan {
            compile::Plan::Write(compile::WritePlan::Insert { collection, .. }) => Some(*collection),
            _ => self.select_plan().map(|select| select.collection),
        }
    }

    /// True for an INSERT whose `RETURNING` answers rows: a write that a
    /// wire describes, and a caller reads, like a query.
    pub fn returns_rows_from_a_write(&self) -> bool {
        matches!(
            &self.plan,
            compile::Plan::Write(compile::WritePlan::Insert { returning, .. }) if !returning.columns.is_empty()
        )
    }

    /// Run this compiled statement to exhaustion and assemble its answer.
    ///
    /// The reading half of [`SqlDatabase::sql`], for a statement prepared
    /// ONCE and [`PreparedSql::bind`]-ed many times: it takes `&Database`,
    /// so it serves rows and folded answers and refuses a write, which needs
    /// the mutable borrow.
    pub fn run(&self, db: &Database) -> Result<SqlResult> {
        match &self.plan {
            compile::Plan::Select(_) | compile::Plan::Aggregate(_) => self.rows(db),
            compile::Plan::Rows(rows) => Ok(if rows.explain {
                SqlResult::Explain(rows.render())
            } else {
                rows.answer()
            }),
            compile::Plan::Explain(select) => Ok(SqlResult::Explain(explain::render(
                db,
                select,
                &self.notices,
                &self.rebind,
            )?)),
            compile::Plan::ExplainAggregate(aggregate) => Ok(SqlResult::Explain(
                explain::render_aggregate(db, aggregate, &self.notices, &self.rebind)?,
            )),
            compile::Plan::ExplainText(text) => Ok(SqlResult::Explain(text.clone())),
            compile::Plan::Gql(gql) => gql.answer(db),
            compile::Plan::EdgeRows(edges) => edges.answer(db),
            compile::Plan::ExplainGql(gql) => Ok(SqlResult::Explain(explain::render_gql(
                db,
                gql,
                &self.notices,
            )?)),
            compile::Plan::Write(_) => Err(SqlError::unsupported(
                "a writing statement runs through `SqlDatabase::sql`, which takes the mutable borrow one writer needs",
            )),
        }
    }

    /// Run this compiled statement where it needs the WRITER's borrow.
    ///
    /// The reading families go straight to [`PreparedSql::run`]; a write is
    /// CLONED before it runs, because performing one consumes the plan (a
    /// bounded write pass owns the request it resumes) and this statement is
    /// reusable -- the next bind has to find the plan still here.
    pub fn run_mut(&self, db: &mut Database) -> Result<SqlResult> {
        match &self.plan {
            compile::Plan::Write(write) => {
                write
                    .clone()
                    .run(db, self.notices.clone(), QueryBudget::unlimited())
            }
            _ => self.run(db),
        }
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

/// The refusal table INSIDE a GQL body (`docs/lang/GQL_PROFILE_DESIGN.md`
/// §5.4): one row per construct the profile builds later, with the tier and
/// a reason that names the milestone building it, or says it is not
/// adopted. [`refusals`] is never consulted inside a GQL body, and this
/// table never outside one.
pub fn gql_refusals() -> &'static [(&'static str, Tier, &'static str)] {
    refuse::GQL_TABLE
}

/// The reason a §4.2 rewrite whose pre-image is a SET of scalar ranges
/// carries. Public so a caller -- and `lang/tests/sql_functions.rs` -- can name it
/// instead of matching on the text.
pub use refuse::MULTI_RANGE as MULTI_RANGE_REASON;

/// End this thread's transaction for the settings `SET LOCAL` made
/// (`ef_search`): a caller that commits, rolls back or drops a transaction
/// through its own API, not through SQL `COMMIT`/`ROLLBACK`, calls this so a
/// `SET LOCAL` never outlives its transaction, as in PostgreSQL.
pub use compile::end_transaction;

/// Parse `text` without compiling it: the half of a prepare that needs
/// neither a database nor the parameters.
///
/// [`prepare_sql`] needs both, because this compiler FOLDS at prepare -- a
/// §4.1 / §4.2 rewrite computes its index range from the value, a semi-join
/// builds its set, a `GRAPH_TABLE` seed resolves its key. A caller that
/// wants a statement's syntax checked before it holds any parameters uses
/// this; `Db::prepare` in the published crate is exactly that.
pub fn parse_sql(text: &str) -> Result<()> {
    parser::parse(text).map(|_| ())
}

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
    let kept = statement.clone();
    let mut notices = Vec::new();
    let (plan, rebind) = compile::compile(db, statement, params, &mut notices, budget, cancelled)?;
    Ok(PreparedSql {
        plan,
        notices,
        statement: Some(Box::new(kept)),
        rebind,
    })
}

/// `EXPLAIN <select>` without the `EXPLAIN` keyword: prepare, run, and print
/// the plan together with the work the run charged.
///
/// This entry point runs the statement to explain it, so it takes the
/// families that can be run for an answer -- a SELECT, an aggregate and a
/// GQL relation -- and nothing else. The EXPLAIN families that must NOT be run to be explained
/// (`DROP TABLE` and the predicated `UPDATE`/`DELETE`) go through
/// `SqlDatabase::sql` with the `EXPLAIN` keyword, which prepares and
/// describes without executing; the refusal below names that route rather
/// than leaving a caller with "EXPLAIN takes a SELECT" in front of a
/// statement that does have an EXPLAIN.
pub fn explain_sql(db: &Database, text: &str, params: &[Param]) -> Result<String> {
    let prepared = prepare_sql(db, text, params)?;
    if let compile::Plan::Gql(gql) = &prepared.plan {
        return explain::render_gql(db, gql, prepared.notices());
    }
    if let Some(aggregate) = prepared.aggregate_plan() {
        return explain::render_aggregate(db, aggregate, prepared.notices(), &prepared.rebind);
    }
    let select = prepared.select_plan().ok_or_else(|| {
        SqlError::unsupported(
            "explain_sql/sql_explain RUNS the statement to explain it, so it takes a SELECT or an aggregate. A DROP TABLE or a predicated UPDATE/DELETE is explained WITHOUT being run: `db.sql(\"EXPLAIN <statement>\")`, which prepares and describes it.",
        )
    })?;
    explain::render(db, select, prepared.notices(), &prepared.rebind)
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
        let (plan, rebind) =
            compile::compile(self, statement, params, &mut notices, budget, cancelled)?;
        match plan {
            compile::Plan::Select(select) => {
                let prepared = PreparedSql {
                    plan: compile::Plan::Select(select),
                    notices,
                    statement: None,
                    rebind,
                };
                prepared.rows(self)
            }
            compile::Plan::Aggregate(aggregate) => {
                let prepared = PreparedSql {
                    plan: compile::Plan::Aggregate(aggregate),
                    notices,
                    statement: None,
                    rebind,
                };
                prepared.groups(self)
            }
            compile::Plan::Explain(select) => {
                let text = explain::render(self, &select, &notices, &rebind)?;
                Ok(SqlResult::Explain(text))
            }
            compile::Plan::ExplainText(text) => Ok(SqlResult::Explain(text)),
            compile::Plan::Rows(rows) => Ok(if rows.explain {
                SqlResult::Explain(rows.render())
            } else {
                rows.answer()
            }),
            compile::Plan::ExplainAggregate(aggregate) => {
                let text = explain::render_aggregate(self, &aggregate, &notices, &rebind)?;
                Ok(SqlResult::Explain(text))
            }
            compile::Plan::Gql(gql) => gql.answer(self),
            compile::Plan::EdgeRows(edges) => edges.answer(self),
            compile::Plan::ExplainGql(gql) => {
                Ok(SqlResult::Explain(explain::render_gql(self, &gql, &notices)?))
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
        OrderValue::EntityId | OrderValue::Driver | OrderValue::Missing => SqlValue::Null,
        // A several-key order has no single ranking value; the compiler
        // refuses to project one (`Output::OrderValue`).
        OrderValue::Keys => SqlValue::Null,
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
    /// One aggregate page's work, added as `QueryWork::add_page` adds it:
    /// `groups` is a HIGH-WATER mark of live accumulator sets, not a running
    /// total, so it is maxed rather than summed.
    pub(crate) fn add_groups(&mut self, page: &sekejap_core::collections::GroupPage) {
        self.work.add_page(&page.work);
        self.rows += page.groups.len() as u64;
        self.pages += 1;
    }

    /// One row page's work.
    pub(crate) fn add(&mut self, page: &QueryPage) {
        self.work.add_page(&page.work);
        self.rows += page.rows.len() as u64;
        self.pages += 1;
    }
}

/// The collection a name refers to, or an error that names it.
pub(crate) fn collection(db: &Database, name: &str) -> SqlResult2<CollectionId> {
    find(db, name)?.ok_or_else(|| {
        SqlError::engine(format!("no collection named `{}`", shown_table(name)))
    })
}

/// A table name as the parser records it: bare for a table in `public`, and
/// `schema` NUL `table` for one in a named schema. NUL cannot occur in a
/// PostgreSQL identifier, so no written name can be mistaken for a
/// qualified one -- a quoted `"a.b"` is the table `a.b` in `public`.
pub(crate) const SCHEMA_SEPARATOR: char = '\u{0}';

/// `schema.table`, as the parser records it.
pub(crate) fn qualified(schema: &str, table: String) -> String {
    if schema.eq_ignore_ascii_case(PUBLIC_SCHEMA) {
        table
    } else {
        format!("{schema}{SCHEMA_SEPARATOR}{table}")
    }
}

/// `(schema, table)` of a recorded name.
pub(crate) fn split_table(name: &str) -> (&str, &str) {
    name.split_once(SCHEMA_SEPARATOR)
        .unwrap_or((PUBLIC_SCHEMA, name))
}

/// A recorded name as a statement would write it: `table`, or
/// `schema.table`.
pub(crate) fn shown_table(name: &str) -> String {
    match name.split_once(SCHEMA_SEPARATOR) {
        Some((schema, table)) => format!("{schema}.{table}"),
        None => name.to_owned(),
    }
}

/// The stem a generated index name starts with. Index names are one
/// namespace across the whole database here, so a table in a named schema
/// carries its schema into the name: `sales_orders_total_btree` beside
/// `orders_total_btree`.
pub(crate) fn index_stem(name: &str) -> String {
    match name.split_once(SCHEMA_SEPARATOR) {
        Some((schema, table)) => format!("{schema}_{table}"),
        None => name.to_owned(),
    }
}

/// The collection a recorded name refers to, if there is one.
/// A table's name as a statement writes it: bare in `public`, `schema.table`
/// elsewhere.
pub(crate) fn table_name_of(db: &Database, c: CollectionId) -> SqlResult2<String> {
    let info = db.collection_info(c).map_err(SqlError::from)?;
    Ok(if info.schema == sekejap_core::collections::PUBLIC_SCHEMA {
        info.name
    } else {
        format!("{}.{}", info.schema, info.name)
    })
}

pub(crate) fn find(db: &Database, name: &str) -> SqlResult2<Option<CollectionId>> {
    let (schema, table) = split_table(name);
    db.collection_in(schema, table).map_err(SqlError::from)
}
