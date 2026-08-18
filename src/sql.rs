//! # SGQL — turning query text into a runnable plan
//!
//! This file is sekejap's query language front-end. It reads a query written as
//! text (`SELECT … WHERE …`, `INSERT …`, and graph patterns inside `MATCH`) and
//! turns it into the plan the engine runs. The language is called **SGQL**:
//! familiar SQL for the relational parts, plus standard graph patterns for the
//! `MATCH` parts — one statement can mix both.
//!
//! Crucially, the parser does **not** invent a second execution engine. A
//! `SELECT` compiles down to the very same `Vec<Step>` the chainable API builds
//! (see `query.rs`), so text queries and code queries run through one executor
//! with no extra overhead. Writes (`INSERT`/`UPDATE`/`DELETE`) compile to a
//! small mutation type instead.
//!
//! ## How a query becomes a plan
//!
//! 1. **Tokenize** — split the raw text into tokens (keywords, identifiers,
//!    strings, numbers, operators). This is where, for example, a doubled quote
//!    inside a string (`'O''Brien'`) collapses to one literal quote.
//! 2. **Parse** — a hand-written recursive-descent [`Parser`] walks the tokens
//!    into a typed statement (a `SELECT`, an aggregate `MATCH`, a shortest-path
//!    query, a mutation, DDL like `CREATE TABLE`, …).
//! 3. **Lower / compile** — the statement is translated into `Vec<Step>` (for
//!    reads) or a mutation value (for writes), ready for the executor.
//!
//! The parser is hand-rolled (no parser-generator dependency) and guards against
//! pathological input with explicit caps (nesting depth, `IN`-list size, etc.),
//! while typed `$N` parameters keep user values out of the SQL text entirely — so
//! there is no string interpolation to inject through.
//!
//! ## Core components in this file
//!
//! - `Tok` / the tokenizer — the raw text split into tokens.
//! - [`Parser`] — the recursive-descent parser; one method per grammar rule.
//! - [`FieldType`] and the schema types — what `CREATE TABLE` declares.
//! - The lowering functions — statement → `Vec<Step>` / mutation.
//!
//! The full accepted grammar is below.
//!
//! # Grammar
//! ```text
//! SELECT [* | field, ...]
//! FROM   collection_name | ALL
//! [WHERE field OP value [AND field OP value]*]
//! [ORDER BY field [ASC|DESC]]
//! [LIMIT n]
//! [OFFSET n]
//!
//! -- SELECT … FROM MATCH (unified graph aggregate form)
//! SELECT expr AS alias [, ...]
//! FROM MATCH (start)-[edge]->(node) [...]
//! [WHERE start._key = 'val']
//! [GROUP BY var.field]
//! [ORDER BY alias [ASC|DESC]]
//! [LIMIT n]
//!
//! INSERT INTO collection (_key, field, ...) VALUES ('key', val, ...)
//! INSERT ('from')-[:KIND {strength: n, key: val}]->('to')
//! UPDATE collection SET field = val [, ...] [WHERE ...]
//! DELETE FROM collection | ALL [WHERE ...]
//! DELETE ('from')-[:KIND]->('to')
//! SELECT expr AS alias [, ...] FROM MATCH (a)-[r]->(b) [WHERE ...] [GROUP BY] [ORDER BY] [LIMIT]
//! SELECT expr AS alias [, ...] FROM MATCH SHORTEST (a)-[r*]->(b) WHERE a._key='x' AND b._key='y'
//! SELECT expr FROM MATCH (a)-[:e]->(b), collection AS alias   -- multi-FROM cross-join
//!
//! CREATE TABLE collection (field type, ...)
//!     [_key TEXT PRIMARY KEY, ...]
//!     [field TIMESTAMPTZ DEFAULT NOW(), ...]
//! WITH (hash: ['_key'], range: ['age'], fulltext: ['name'], bm25: ['bio'], spatial: ['location'])
//! DROP TABLE [IF EXISTS] collection
//! DROP INDEX [IF EXISTS] ON collection USING method (field)
//!
//! ALTER TABLE collection ADD [COLUMN] name type [PRIMARY KEY] [NOT NULL]
//! ALTER TABLE collection DROP [COLUMN] [IF EXISTS] name
//! ALTER TABLE collection RENAME COLUMN old TO new
//! ALTER TABLE collection RENAME TO new_name
//! ALTER TABLE collection ALTER [COLUMN] name TYPE new_type
//!
//! SHOW TABLES
//! SHOW EDGES [FROM collection] [TO collection]
//! SHOW CREATE TABLE collection
//! SHOW collection
//!
//! -- SELECT list expressions (SELECT … FROM MATCH)
//! expr ::= var.field
//!        | COUNT(*) | SUM(math) | AVG(math) | MIN(math) | MAX(math)
//!        | PATH_AVG(var.field) | PATH_SUM(var.field) | PATH_MIN(var.field)
//!        | PATH_MAX(var.field) | PATH_PRODUCT(var.field)
//!        | PATH_FIRST(var.field) | PATH_LAST(var.field)
//!        | CASE WHEN var.field op literal THEN literal
//!               [WHEN ...] [ELSE literal] END
//!        | AGE_DAYS(var.field) | AGE_HOURS(var.field)
//!        | NOW()
//!        | JSON_ARRAY_LENGTH(var.field)
//!
//! OP    ::= = | != | <> | > | < | >= | <= | <=> | <-> | <#> | <+>
//!         | BETWEEN n AND n
//!         | IN (val, ...)
//!         | LIKE 'pat' | ILIKE 'pat'
//! value ::= 'string' | number | true | false | null
//!         | '{"type":"Point",...}'  (auto-parsed JSON)
//! ```

use crate::query::{ScoreExpr, Step};
use crate::sk_hash;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

/// A numeric literal as the narrowest JSON number that represents it exactly.
///
/// The lexer hands back `f64` for every number it sees, so without this every
/// literal in every statement became a float — and a column of integers acquired
/// floats the moment anyone wrote to it through SQL.
fn int_or_float(n: f64) -> Option<serde_json::Number> {
    if n.fract() == 0.0 && n >= i64::MIN as f64 && n <= i64::MAX as f64 {
        return Some(serde_json::Number::from(n as i64));
    }
    serde_json::Number::from_f64(n)
}


// ── Error ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum SqlError {
    UnexpectedToken { expected: &'static str, got: String },
    UnexpectedEnd { expected: &'static str },
    InvalidNumber(String),
    MissingField { field: &'static str },
    FieldValueCountMismatch { fields: usize, values: usize },
    InvalidValue(String),
    /// A step needs an index that is not there, and cannot answer without it.
    ///
    /// Returned by `query()` before anything executes, for filters that are served
    /// entirely from an index — `SEARCH` and `BM25`. Without this they returned no
    /// rows, which reads as "nothing matches" rather than "nothing was consulted".
    ///
    /// Filters that can still answer by scanning (`ILIKE`, and any `WHERE` on an
    /// unindexed field) do not raise this: there the index is a matter of speed,
    /// not of the answer.
    ///
    /// `declared` separates the two ways an index can be absent, because the fix
    /// differs: `false` means no `CREATE INDEX` was ever issued, `true` means one
    /// was and the index is not currently built.
    IndexNotBuilt {
        collection: String,
        method: String,
        field: String,
        declared: bool,
    },
    /// An explicit index build failed (e.g. HNSW with no stored vectors).
    IndexBuildFailed {
        collection: String,
        method: String,
        field: String,
        reason: String,
    },
    /// A SELECT column is neither in the GROUP BY clause nor wrapped in an aggregate function.
    /// PostgreSQL raises the same error; MySQL silently picks a random row (we do not).
    GroupByViolation(String),
    /// Parameter $N is out of range (only M parameter(s) provided).
    ParamOutOfRange { index: usize, count: usize },
    /// Parameter $N has the wrong type.
    ParamTypeMismatch { index: usize, expected: &'static str },
    /// Transaction protocol error (nested BEGIN, COMMIT/ROLLBACK without active transaction).
    TransactionError(String),
    /// A schema qualifier other than `public` was used on a table name. sekejap
    /// has a single implicit namespace exposed as `public` (Postgres' default).
    UndefinedSchema(String),
    /// `SELECT … FROM <name>` where no such collection exists — no declared
    /// schema and no stored row belongs to it.
    ///
    /// It used to return no rows, so a mistyped table name was indistinguishable
    /// from an empty one. PostgreSQL raises `relation "x" does not exist`.
    UndefinedTable(String),
}

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SqlError::UnexpectedToken { expected, got } => {
                write!(f, "expected {expected}, got `{got}`")
            }
            SqlError::UnexpectedEnd { expected } => {
                write!(f, "unexpected end of input, expected {expected}")
            }
            SqlError::InvalidNumber(s) => write!(f, "invalid number: `{s}`"),
            SqlError::MissingField { field } => write!(f, "INSERT requires a `{field}` field"),
            SqlError::FieldValueCountMismatch { fields, values } => write!(
                f,
                "field count ({fields}) does not match value count ({values})"
            ),
            SqlError::InvalidValue(s) => write!(f, "invalid value: {s}"),
            SqlError::UndefinedSchema(s) => write!(f, "schema \"{s}\" does not exist"),
            SqlError::UndefinedTable(t) => write!(
                f,
                "relation \"{t}\" does not exist.\n  Hint: SHOW TABLES lists the \
                 collections this database holds"
            ),
            SqlError::IndexNotBuilt { collection, method, field, declared } => {
                // A search index covers the collection rather than one field, so
                // naming a field it does not have would send the reader looking
                // for the wrong thing.
                let target = match (collection.is_empty(), field.is_empty()) {
                    // Neither is known: `SEARCH_SCORE` resolves against whichever
                    // index holds the document, so the requirement is that one
                    // exists at all rather than that a named table has one.
                    (true, true) => "this database".to_string(),
                    (true, false) => format!("field {field}"),
                    (false, true) => collection.clone(),
                    (false, false) => format!("{collection}.{field}"),
                };
                let collection = if collection.is_empty() { "<table>" } else { collection };
                let args = if field.is_empty() { "…".to_string() } else { field.clone() };
                if *declared {
                    write!(
                        f,
                        "{method} index on {target} is declared but not built, and \
                         {method} cannot answer without it.\n  \
                         Hint: REINDEX ON {collection} USING {method} ({args})"
                    )
                } else {
                    write!(
                        f,
                        "there is no {method} index on {target}, and {method} cannot \
                         answer without one — it reads the index, not the rows.\n  \
                         Hint: CREATE INDEX ON {collection} USING {method} ({args})"
                    )
                }
            }
            SqlError::IndexBuildFailed { collection, method, field, reason } => write!(
                f,
                "cannot build {method} index on {collection}.{field}: {reason}.\n  Hint: once data is ready, run: REINDEX ON {collection} USING {method} ({field})"
            ),
            SqlError::GroupByViolation(col) => write!(
                f,
                "column \"{col}\" must appear in the GROUP BY clause or be used in an aggregate function"
            ),
            SqlError::ParamOutOfRange { index, count } => write!(
                f,
                "parameter ${index} out of range (only {count} parameter(s) provided)"
            ),
            SqlError::ParamTypeMismatch { index, expected } => write!(
                f,
                "parameter ${index}: expected {expected}"
            ),
            SqlError::TransactionError(msg) => write!(f, "transaction error: {msg}"),
        }
    }
}

impl std::error::Error for SqlError {}

// ── Tokens ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Kw(Kw),
    Ident(String),
    Str(String),
    Num(f64),
    Star,
    Comma,
    Eq,  // =
    Neq, // != or <>
    Gt,  // >
    Lt,  // <
    Gte, // >=
    Lte, // <=
    LParen,
    RParen,
    LBrace,    // {
    RBrace,    // }
    Arrow,     // ->
    LongArrow, // ->>
    BackArrow, // <-
    Colon,     // :
    LBracket,  // [
    RBracket,  // ]
    Dot,       // .
    DotDot,    // ..
    Dash,      // -
    Plus,      // +
    Slash,     // /
    Concat,    // ||  (string concatenation)
    VecCosineOp, // <=>  cosine distance
    VecL2Op,     // <->  Euclidean (L2) distance
    VecDotOp,    // <#>  inner product
    VecL1Op,     // <+>  Manhattan (L1) distance
    ArrayContains, // @>  PostgreSQL array containment
    Param(usize), // $1, $2, ... (1-indexed, like PostgreSQL)
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
enum Kw {
    Select,
    From,
    Where,
    And,
    Order,
    By,
    Asc,
    Desc,
    Limit,
    Offset,
    All,
    Between,
    In,
    Like,
    ILike,
    Null,
    True,
    False,
    Insert,
    Into,
    Values,
    Delete,
    Update,
    Set,
    Match,
    Return,
    Union,
    Create,
    Table,
    Index,
    On,
    Using,
    Primary,
    Key,
    With,
    VectorNear,
    // Logical / null check
    Not,
    Or,
    Is,
    // Projection alias
    As,
    // Aggregate functions
    Count,
    Sum,
    Avg,
    Min,
    Max,
    // Grouping / dedup
    Distinct,
    Group,
    Having,
    // Schema introspection
    Show,
    // DDL lifecycle
    Drop,
    If,
    Exists,
    // ALTER TABLE
    Alter,
    Column,
    Rename,
    To,
    Add,
    Default,
    // Index rebuild
    Reindex,
    // CASE expression
    Case,
    When,
    Then,
    Else,
    End,
    // EXPLAIN
    Explain,
    Analyze,
    // Path predicate quantifiers (ANY/ALL already: All, add Any/None_/Single)
    Any,
    None_,
    Single,
    // Transaction control
    Begin,
    Commit,
    Rollback,
    // Storage management
    Compact,
}

fn kw_to_str(kw: &Kw) -> &'static str {
    match kw {
        Kw::Select => "select",
        Kw::From => "from",
        Kw::Where => "where",
        Kw::And => "and",
        Kw::Order => "order",
        Kw::By => "by",
        Kw::Asc => "asc",
        Kw::Desc => "desc",
        Kw::Limit => "limit",
        Kw::Offset => "offset",
        Kw::All => "all",
        Kw::Between => "between",
        Kw::In => "in",
        Kw::Like => "like",
        Kw::ILike => "ilike",
        Kw::Null => "null",
        Kw::True => "true",
        Kw::False => "false",
        Kw::Insert => "insert",
        Kw::Into => "into",
        Kw::Values => "values",
        Kw::Delete => "delete",
        Kw::Update => "update",
        Kw::Set => "set",
        Kw::Match => "match",
        Kw::Return => "return",
        Kw::Union => "union",
        Kw::Create => "create",
        Kw::Table => "table",
        Kw::Index => "index",
        Kw::On => "on",
        Kw::Using => "using",
        Kw::Primary => "primary",
        Kw::Key => "key",
        Kw::With => "with",
        Kw::VectorNear => "vector_near",
        Kw::Not => "not",
        Kw::Or => "or",
        Kw::Is => "is",
        Kw::As => "as",
        Kw::Count => "count",
        Kw::Sum => "sum",
        Kw::Avg => "avg",
        Kw::Min => "min",
        Kw::Max => "max",
        Kw::Distinct => "distinct",
        Kw::Group => "group",
        Kw::Having => "having",
        Kw::Show => "show",
        Kw::Drop => "drop",
        Kw::If => "if",
        Kw::Exists => "exists",
        Kw::Alter => "alter",
        Kw::Column => "column",
        Kw::Rename => "rename",
        Kw::To => "to",
        Kw::Add => "add",
        Kw::Default => "default",
        Kw::Reindex => "reindex",
        Kw::Case => "case",
        Kw::When => "when",
        Kw::Then => "then",
        Kw::Else => "else",
        Kw::End => "end",
        Kw::Explain => "explain",
        Kw::Analyze => "analyze",
        Kw::Any => "any",
        Kw::None_ => "none",
        Kw::Single => "single",
        Kw::Begin => "begin",
        Kw::Commit => "commit",
        Kw::Rollback => "rollback",
        Kw::Compact => "compact",
    }
}

fn keyword(s: &str) -> Option<Kw> {
    match s.to_ascii_uppercase().as_str() {
        "SELECT" => Some(Kw::Select),
        "FROM" => Some(Kw::From),
        "WHERE" => Some(Kw::Where),
        "AND" => Some(Kw::And),
        "ORDER" => Some(Kw::Order),
        "BY" => Some(Kw::By),
        "ASC" => Some(Kw::Asc),
        "DESC" => Some(Kw::Desc),
        "LIMIT" => Some(Kw::Limit),
        "OFFSET" => Some(Kw::Offset),
        "ALL" => Some(Kw::All),
        "BETWEEN" => Some(Kw::Between),
        "IN" => Some(Kw::In),
        "LIKE" => Some(Kw::Like),
        "ILIKE" => Some(Kw::ILike),
        "NULL" => Some(Kw::Null),
        "TRUE" => Some(Kw::True),
        "FALSE" => Some(Kw::False),
        "INSERT" => Some(Kw::Insert),
        "INTO" => Some(Kw::Into),
        "VALUES" => Some(Kw::Values),
        "DELETE" => Some(Kw::Delete),
        "UPDATE" => Some(Kw::Update),
        "SET" => Some(Kw::Set),
        "MATCH" => Some(Kw::Match),
        "RETURN" => Some(Kw::Return),
        "UNION" => Some(Kw::Union),
        "CREATE" => Some(Kw::Create),
        "TABLE" => Some(Kw::Table),
        "INDEX" => Some(Kw::Index),
        "ON" => Some(Kw::On),
        "USING" => Some(Kw::Using),
        "PRIMARY" => Some(Kw::Primary),
        "KEY" => Some(Kw::Key),
        "WITH" => Some(Kw::With),
        "VECTOR_NEAR" => Some(Kw::VectorNear),
        "NOT" => Some(Kw::Not),
        "OR" => Some(Kw::Or),
        "IS" => Some(Kw::Is),
        "AS" => Some(Kw::As),
        "COUNT" => Some(Kw::Count),
        "SUM" => Some(Kw::Sum),
        "AVG" => Some(Kw::Avg),
        "MIN" => Some(Kw::Min),
        "MAX" => Some(Kw::Max),
        "DISTINCT" => Some(Kw::Distinct),
        "GROUP" => Some(Kw::Group),
        "HAVING" => Some(Kw::Having),
        "SHOW" => Some(Kw::Show),
        "DROP" => Some(Kw::Drop),
        "IF" => Some(Kw::If),
        "EXISTS" => Some(Kw::Exists),
        "ALTER" => Some(Kw::Alter),
        "COLUMN" => Some(Kw::Column),
        "RENAME" => Some(Kw::Rename),
        "TO" => Some(Kw::To),
        "ADD" => Some(Kw::Add),
        "DEFAULT" => Some(Kw::Default),
        "REINDEX" => Some(Kw::Reindex),
        "CASE" => Some(Kw::Case),
        "WHEN" => Some(Kw::When),
        "THEN" => Some(Kw::Then),
        "ELSE" => Some(Kw::Else),
        "END" => Some(Kw::End),
        "EXPLAIN" => Some(Kw::Explain),
        "ANALYZE" => Some(Kw::Analyze),
        "ANY" => Some(Kw::Any),
        "NONE" => Some(Kw::None_),
        "SINGLE" => Some(Kw::Single),
        "BEGIN" => Some(Kw::Begin),
        "COMMIT" => Some(Kw::Commit),
        "ROLLBACK" => Some(Kw::Rollback),
        "COMPACT" => Some(Kw::Compact),
        _ => None,
    }
}

// ── Tokenizer ─────────────────────────────────────────────────────────────────

/// Measurement helper: tokenize and return the token count (used by the
/// `bench_parse` example to isolate tokenization cost). Hidden from docs.
#[doc(hidden)]
pub fn bench_tokenize(sql: &str) -> Result<usize, SqlError> {
    tokenize(sql).map(|t| t.len())
}

/// Turn raw SQL text into a flat list of tokens — step 1 of parsing.
///
/// "Tokenizing" (or lexing) means splitting a string like `SELECT age FROM users`
/// into meaningful chunks: a keyword `SELECT`, an identifier `age`, a keyword
/// `FROM`, an identifier `users`. It's easier for the parser to work over these
/// typed `Tok`s than over raw characters. This is a classic character scanner: a
/// cursor `i` walks the input, and at each position it decides what kind of token
/// starts there (whitespace to skip, a comment, a quoted string, a number, an
/// operator, or a word) and advances past it.
///
/// Errors are returned for malformed input (e.g. an unterminated string), so a
/// bad query fails cleanly here rather than confusing the parser later.
fn tokenize(sql: &str) -> Result<Vec<Tok>, SqlError> {
    // Collect into `Vec<char>` so indexing counts CHARACTERS, not bytes — a `&str`
    // is UTF-8 and one character can span several bytes, which would break `chars[i]`.
    let chars: Vec<char> = sql.chars().collect();
    let len = chars.len();
    let mut i = 0;                 // cursor: index of the character we're looking at
    let mut tokens = Vec::new();   // the token stream we're building

    while i < len {
        let c = chars[i];

        // Whitespace between tokens is meaningless — skip it.
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }

        // SQL line comment: `-- …` to end of line (a single `-` falls through below).
        // Exception: `-->` is the anonymous forward-edge token, NOT a comment — so
        // only treat `--` as a comment when it is not immediately followed by `>`.
        if c == '-' && i + 1 < len && chars[i + 1] == '-'
            && !(i + 2 < len && chars[i + 2] == '>')
        {
            i += 2;
            while i < len && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        // SQL block comment: `/* … */`.
        if c == '/' && i + 1 < len && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < len && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(len); // skip the closing `*/`
            continue;
        }

        match c {
            '*' => {
                tokens.push(Tok::Star);
                i += 1;
            }
            ',' => {
                tokens.push(Tok::Comma);
                i += 1;
            }
            '(' => {
                tokens.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Tok::RParen);
                i += 1;
            }
            '{' => {
                tokens.push(Tok::LBrace);
                i += 1;
            }
            '}' => {
                tokens.push(Tok::RBrace);
                i += 1;
            }
            '[' => {
                tokens.push(Tok::LBracket);
                i += 1;
            }
            ']' => {
                tokens.push(Tok::RBracket);
                i += 1;
            }
            ':' => {
                tokens.push(Tok::Colon);
                i += 1;
            }
            '=' => {
                tokens.push(Tok::Eq);
                i += 1;
            }
            '.' => {
                if i + 1 < len && chars[i + 1] == '.' {
                    tokens.push(Tok::DotDot);
                    i += 2;
                } else {
                    tokens.push(Tok::Dot);
                    i += 1;
                }
            }
            '>' => {
                if i + 1 < len && chars[i + 1] == '=' {
                    tokens.push(Tok::Gte);
                    i += 2;
                } else {
                    tokens.push(Tok::Gt);
                    i += 1;
                }
            }
            '-' if i + 2 < len && chars[i + 1] == '>' && chars[i + 2] == '>' => {
                tokens.push(Tok::LongArrow);
                i += 3;
            }
            '-' if i + 1 < len && chars[i + 1] == '>' => {
                tokens.push(Tok::Arrow);
                i += 2;
            }
            '-' if i + 1 < len && chars[i + 1].is_ascii_digit() => {
                let start = i;
                i += 1; // skip '-'
                while i < len
                    && (chars[i].is_ascii_digit()
                        || (chars[i] == '.' && !(i + 1 < len && chars[i + 1] == '.')))
                {
                    i += 1;
                }
                let s: String = chars[start..i].iter().collect();
                let n = s.parse::<f64>().map_err(|_| SqlError::InvalidNumber(s))?;
                tokens.push(Tok::Num(n));
            }
            '-' => {
                tokens.push(Tok::Dash);
                i += 1;
            }
            '+' => {
                tokens.push(Tok::Plus);
                i += 1;
            }
            '/' => {
                tokens.push(Tok::Slash);
                i += 1;
            }
            '|' if i + 1 < len && chars[i + 1] == '|' => {
                tokens.push(Tok::Concat); // ||  string concatenation
                i += 2;
            }
            '<' => {
                // 3-char vector operators — must be checked before 2-char operators.
                if i + 2 < len && chars[i + 1] == '=' && chars[i + 2] == '>' {
                    tokens.push(Tok::VecCosineOp); // <=>
                    i += 3;
                } else if i + 2 < len && chars[i + 1] == '-' && chars[i + 2] == '>' {
                    tokens.push(Tok::VecL2Op); // <->
                    i += 3;
                } else if i + 2 < len && chars[i + 1] == '#' && chars[i + 2] == '>' {
                    tokens.push(Tok::VecDotOp); // <#>
                    i += 3;
                } else if i + 2 < len && chars[i + 1] == '+' && chars[i + 2] == '>' {
                    tokens.push(Tok::VecL1Op); // <+>
                    i += 3;
                } else if i + 1 < len && chars[i + 1] == '=' {
                    tokens.push(Tok::Lte); // <=
                    i += 2;
                } else if i + 1 < len && chars[i + 1] == '>' {
                    tokens.push(Tok::Neq); // <>
                    i += 2;
                } else if i + 1 < len && chars[i + 1] == '-' {
                    tokens.push(Tok::BackArrow); // <-
                    i += 2;
                } else {
                    tokens.push(Tok::Lt); // <
                    i += 1;
                }
            }
            '!' => {
                if i + 1 < len && chars[i + 1] == '=' {
                    tokens.push(Tok::Neq);
                    i += 2;
                } else {
                    return Err(SqlError::UnexpectedToken {
                        expected: "=",
                        got: if i + 1 < len {
                            chars[i + 1].to_string()
                        } else {
                            "end".into()
                        },
                    });
                }
            }
            '\'' | '"' => {
                let quote = c;
                i += 1;
                // Build char-by-char so a doubled quote inside the string
                // (SQL-standard escape: 'O''Brien' → O'Brien) collapses to one
                // literal quote instead of terminating the string.
                let mut s = String::new();
                while i < len {
                    if chars[i] == quote {
                        if i + 1 < len && chars[i + 1] == quote {
                            s.push(quote);
                            i += 2;
                            continue;
                        }
                        break; // closing quote
                    }
                    s.push(chars[i]);
                    i += 1;
                }
                if i < len {
                    i += 1;
                } // consume closing quote
                tokens.push(Tok::Str(s));
            }
            c if c.is_ascii_digit() => {
                let start = i;
                while i < len
                    && (chars[i].is_ascii_digit()
                        || (chars[i] == '.' && !(i + 1 < len && chars[i + 1] == '.')))
                {
                    i += 1;
                }
                let s: String = chars[start..i].iter().collect();
                let n = s.parse::<f64>().map_err(|_| SqlError::InvalidNumber(s))?;
                tokens.push(Tok::Num(n));
            }
            c if c.is_alphabetic() || c == '_' || c == '/' => {
                let start = i;
                while i < len && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '/')
                {
                    i += 1;
                }
                let s: String = chars[start..i].iter().collect();
                match keyword(&s) {
                    Some(kw) => tokens.push(Tok::Kw(kw)),
                    None => tokens.push(Tok::Ident(s)),
                }
            }
            '@' => {
                if i + 1 < len && chars[i + 1] == '>' {
                    tokens.push(Tok::ArrayContains);
                    i += 2;
                } else {
                    return Err(SqlError::UnexpectedToken {
                        expected: "> after @",
                        got: if i + 1 < len { chars[i + 1].to_string() } else { "end".into() },
                    });
                }
            }
            '$' => {
                i += 1;
                let start = i;
                while i < len && chars[i].is_ascii_digit() {
                    i += 1;
                }
                if i == start {
                    return Err(SqlError::UnexpectedToken {
                        expected: "parameter index after $",
                        got: "$".to_string(),
                    });
                }
                let s: String = chars[start..i].iter().collect();
                let idx: usize = s.parse().map_err(|_| SqlError::InvalidNumber(s))?;
                if idx == 0 {
                    return Err(SqlError::InvalidValue(
                        "parameter index starts at $1, not $0".into(),
                    ));
                }
                tokens.push(Tok::Param(idx));
            }
            ';' => {
                i += 1;
            } // trailing semicolons are ignored
            other => {
                return Err(SqlError::UnexpectedToken {
                    expected: "valid SQL token",
                    got: other.to_string(),
                });
            }
        }
    }

    tokens.push(Tok::Eof);
    Ok(tokens)
}

// ── AST ───────────────────────────────────────────────────────────────────────

enum Source {
    Collection(String),
    All,
}

#[derive(Clone, Copy)]
enum CompareOp {
    Eq,
    Neq,
    Gt,
    Lt,
    Gte,
    Lte,
}

enum CondExpr {
    Compare {
        field: String,
        op: CompareOp,
        value: Value,
    },
    Between {
        field: String,
        lo: f64,
        hi: f64,
    },
    In {
        field: String,
        values: Vec<Value>,
    },
    Like {
        field: String,
        pattern: String,
        case_insensitive: bool,
    },
    StDWithin {
        lat: f64,
        lon: f64,
        distance_m: f64,
    },
    StContainsPoint {
        lat: f64,
        lon: f64,
    },
    StWithin {
        ring: Vec<[f64; 2]>,
    },
    StContains {
        ring: Vec<[f64; 2]>,
    },
    StIntersects {
        ring: Vec<[f64; 2]>,
    },
    StDistance {
        field: String,
        lat: f64,
        lon: f64,
        max_m: f64,
    },
    StLength {
        field: String,
        min_m: f64,
    },
    StPerimeter {
        field: String,
        min_m: f64,
    },
    StArea {
        field: String,
        min_m2: f64,
    },
    Bm25Func {
        field: String,
        query: String,
    },
    Bm25 {
        field: String,
        query: String,
        min_score: f64,
    },
    VectorNear {
        field: String,
        query: Vec<f32>,
        k: usize,
    },
    /// `field IS NULL` or `field IS NOT NULL`.
    IsNull {
        field: String,
        negated: bool, // true = IS NOT NULL
    },
    /// `field @> ['a', 'b']` — JSON array field contains all specified values.
    ArrayContains {
        field: String,
        values: Vec<Value>,
    },
    /// `NOT <inner_cond>`
    Not(Box<CondExpr>),
    /// `cond_group OR cond_group [OR …]`
    /// Each inner Vec is one AND-group.
    Or(Vec<Vec<CondExpr>>),
    /// `SEARCH('query text')` — positional search index filter.
    Search { query: String, typo: Option<u32> },
}

enum OrderKey {
    /// One or more `field [ASC|DESC]` columns, evaluated left-to-right.
    Fields(Vec<(String, bool)>),
    /// `field <op> [f32, ...]` — sort by vector distance (nearest first, Dot negated).
    Vector { field: String, query: Vec<f32>, metric: crate::query::VecMetric },
    /// Arithmetic score expression, e.g. `BM25(title,'q') * 0.7 + popularity * 0.3`.
    Expr(ScoreExpr, bool),
}

struct SelectStmt {
    fields: Vec<String>,
    source: Source,
    conditions: Vec<CondExpr>,
    group_by: Vec<String>,
    having: Vec<CondExpr>,
    distinct: bool,
    order_by: Option<OrderKey>,
    limit: Option<usize>,
    offset: Option<usize>,
    score_projections: Vec<(ScoreExpr, String)>,
}

struct InsertStmt {
    collection: String,
    fields: Vec<String>,
    rows: Vec<Vec<Value>>,
}

struct DeleteStmt {
    source: Source,
    conditions: Vec<CondExpr>,
}

struct UpdateStmt {
    collection: String,
    updates: Vec<(String, Value)>, // SET field = value pairs
    conditions: Vec<CondExpr>,
}

/// A single edge parsed from INSERT ('a')-[:KIND {props}]->('b').
#[derive(Debug, Clone)]
pub struct EdgeInsert {
    pub from: String,
    pub to: String,
    pub edge_type: String,
    pub props_json: Option<String>,
}

/// A single edge parsed from DELETE ('a')-[:KIND {k:v}]->('b').
#[derive(Debug, Clone)]
pub struct EdgeDelete {
    pub from: String,
    pub to: String,
    pub edge_type: String,
    /// Optional inline attribute predicate — delete only edges matching these.
    /// `None` (no `{…}`) deletes ALL edges of this type between the endpoints.
    pub props: Option<Value>,
}

/// The result of compiling a mutation SQL statement.
#[derive(Debug)]
pub enum CompiledMutation {
    /// A single node to insert, plus any vector fields.
    Insert {
        /// The collection this insert targets (for schema lookup).
        collection: String,
        slug: String,
        payload_json: String,
        /// Vector fields separated from the JSON payload: (field_name, f32 data).
        vectors: Vec<(String, Vec<f32>)>,
    },
    /// Steps that select the set of nodes to delete.
    Delete(Vec<Step>),
    /// Steps that select nodes to update, plus the field→value patches.
    Update {
        steps: Vec<Step>,
        updates: Vec<(String, Value)>,
    },
    /// Multiple nodes to insert in one statement (multi-row VALUES).
    /// HNSW indexes are rebuilt once per affected vector field, not per row.
    InsertBatch {
        collection: String,
        items: Vec<(String, String, Vec<(String, Vec<f32>)>)>, // (slug, payload_json, vectors)
    },
    /// Create one or more directed edges via Cypher pattern.
    InsertEdge(Vec<EdgeInsert>),
    /// Remove one or more directed edges via Cypher pattern.
    DeleteEdge(Vec<EdgeDelete>),
    /// `UPDATE ('a')-[:T {pred}]->('b') SET k=v [WHERE …]` — set attributes on the
    /// edges from→to of `edge_type` matching `predicate` (None = all of that type).
    UpdateEdge {
        from: String,
        to: String,
        edge_type: String,
        predicate: Option<Value>,
        sets: Vec<(String, Value)>,
    },
    /// MATCH ... INSERT: select nodes via MATCH, then insert edges.
    MatchInsert {
        match_steps: Vec<Step>,
        target: String,
        edge_type: String,
        props: Option<String>,
    },
    /// CREATE TABLE: define schema for a collection.
    CreateTable {
        collection: String,
        schema: TableSchema,
    },
    /// CREATE INDEX: build an index on a collection field (PostgreSQL style).
    CreateIndex {
        /// Optional index name (ignored at runtime, stored for introspection).
        name: Option<String>,
        collection: String,
        method: IndexMethod,
        fields: Vec<String>,
    },
    /// `CREATE [MATERIALIZED|SEARCH] VIEW name AS <body>` — a materialized view: the
    /// body query is stored and (re)run to populate a derived collection named `name`.
    /// `auto_index` (SEARCH VIEW) auto-builds indexes on the view's search-type fields.
    CreateView {
        name: String,
        /// The raw SQL of the projection body (after `AS`), re-run on refresh.
        query_sql: String,
        /// SEARCH VIEW → true: auto-index search-type fields (TEXT→bm25, …).
        auto_index: bool,
    },
    /// `REFRESH MATERIALIZED VIEW name` — re-run the stored body and repopulate.
    RefreshView { name: String },
    /// DROP TABLE [IF EXISTS]: delete schema + all nodes + cascade edges.
    DropTable {
        collection: String,
        /// If true, silently succeed when the collection does not exist.
        if_exists: bool,
    },
    /// DROP INDEX [IF EXISTS] ON collection USING method (field): remove a specific index.
    DropIndex {
        collection: String,
        method: IndexMethod,
        field: String,
        if_exists: bool,
    },
    /// ALTER TABLE: modify an existing schema (add/drop/rename columns, rename table).
    AlterTable {
        collection: String,
        op: AlterTableOp,
    },
    /// REINDEX: rebuild an existing index on a collection field.
    /// Does not write a WAL entry — it is a rebuild, not a schema mutation.
    Reindex {
        collection: String,
        method: IndexMethod,
        fields: Vec<String>,
    },
    /// BEGIN: start a SQL transaction.
    Begin,
    /// COMMIT: apply all buffered mutations atomically.
    Commit,
    /// ROLLBACK: discard all buffered mutations.
    Rollback,
    /// COMPACT: flush snapshot and truncate WAL.
    Compact,
    /// SET WAL_FORMAT = json|binary: change WAL encoding and compact.
    SetWalFormat(crate::WalFormat),
    /// SET WAL_MODE = physical|logical: choose UPDATE logging strategy.
    /// `true` = logical (one command entry per statement),
    /// `false` = physical (one Put entry per affected row — the default).
    SetWalMode(bool),
    /// SET WAL_SYNC = full|barrier|os: choose fsync strength.
    /// `full` = F_FULLFSYNC-class (default, power-loss durable),
    /// `barrier` = write-ordering barrier, `os` = plain fsync
    /// (matches SQLite's actual synchronous=FULL durability on macOS).
    SetWalSync(crate::storage::wal::SyncLevel),
}

/// The specific alteration to apply in an `ALTER TABLE` statement.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AlterTableOp {
    /// `ALTER TABLE t ADD COLUMN name type`
    AddColumn { def: FieldDef },
    /// `ALTER TABLE t DROP COLUMN [IF EXISTS] name`
    DropColumn { name: String, if_exists: bool },
    /// `ALTER TABLE t RENAME COLUMN old TO new`
    RenameColumn { old_name: String, new_name: String },
    /// `ALTER TABLE t RENAME TO new_name`
    RenameTable { new_name: String },
    /// `ALTER TABLE t ALTER COLUMN name TYPE new_type` (schema-only; no data coercion)
    AlterColumnType { name: String, ty: FieldType },
}

// ── MATCH AST ─────────────────────────────────────────────────────────────────

struct MatchNode {
    var: Option<String>,         // (a:artist) → var="a"
    label: Option<String>,       // (a:artist) → label="artist"
    props: Vec<(String, Value)>, // (:genre {_key: 'rock'}) → inline WHERE
}

enum MatchCond {
    NodeField {
        field: String,
        op: CompareOp,
        value: Value,
    },
}

// ── CREATE INDEX ──────────────────────────────────────────────────────────────

/// Index method — mirrors PostgreSQL's `USING <method>` clause.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexMethod {
    /// B-tree: range and equality on ordered fields (price, age, …).
    Btree,
    /// Hash: fast exact-match lookups.
    Hash,
    /// GIN trigram: ILIKE / full-text search (exact, no false positives).
    Gin,
    /// GiST trigram: ILIKE / full-text search (lossy, lower memory).
    Gist,
    /// BM25: relevance-ranked full-text search.
    Bm25,
    /// Spatial grid: ST_DWITHIN, ST_WITHIN, ST_CONTAINS, …
    Spatial,
    /// HNSW: approximate nearest-neighbour vector search (Phase 2).
    Hnsw,
    /// Search: positional inverted index with RoaringBitmaps.
    Search,
}

impl std::fmt::Display for IndexMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Btree   => "btree",
            Self::Hash    => "hash",
            Self::Gin     => "gin",
            Self::Gist    => "gist",
            Self::Bm25    => "bm25",
            Self::Spatial => "spatial",
            Self::Hnsw    => "hnsw",
            Self::Search  => "search",
        };
        f.write_str(s)
    }
}

// ── CREATE TABLE AST ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FieldType {
    Text,
    Integer,
    Real,
    Bool,
    Timestamptz,
    Geo,
    Vector,
    Json,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldDef {
    pub name: String,
    pub ty: FieldType,
    pub is_primary_key: bool,
    pub is_timestamptz: bool,
    pub default_now: bool,
    /// If true, auto-fill this field with a random UUIDv4 when absent from INSERT.
    #[serde(default)]
    pub default_uuid4: bool,
    /// If set, auto-fill this field with UUIDV5(namespace, name) when absent from INSERT.
    #[serde(default)]
    pub default_uuid5: Option<(String, String)>,
    /// `GENERATED ALWAYS AS (<expr>) STORED` — a computed column. When set, the field's
    /// value is derived from this record's OTHER fields at INSERT/UPDATE (any value the
    /// user supplies is ignored). See [`GenExpr`].
    #[serde(default)]
    pub generated: Option<GenExpr>,
}

/// A generated-column expression, evaluated against a record's own fields. Kept small
/// on purpose: string building for search blobs / composite keys, not a full language.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GenExpr {
    /// Reference to another field in the same record.
    Field(String),
    /// A string literal.
    Lit(String),
    /// `a || b || …` — concatenation (NULL/missing treated as empty string).
    Concat(Vec<GenExpr>),
    /// `concat_ws(sep, a, b, …)` — join non-null parts with a separator.
    ConcatWs(String, Vec<GenExpr>),
    /// `lower(x)` / `upper(x)`.
    Lower(Box<GenExpr>),
    Upper(Box<GenExpr>),
    /// `coalesce(a, b, …)` — first non-null/non-empty part.
    Coalesce(Vec<GenExpr>),
}

impl GenExpr {
    /// Evaluate against a record's fields → the computed string value.
    pub fn eval(&self, fields: &serde_json::Map<String, serde_json::Value>) -> String {
        // Render a JSON value as text (strings unquoted, others via to_string; null → "").
        fn text(v: &serde_json::Value) -> String {
            match v {
                serde_json::Value::Null => String::new(),
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            }
        }
        match self {
            GenExpr::Field(name) => fields.get(name).map(text).unwrap_or_default(),
            GenExpr::Lit(s) => s.clone(),
            GenExpr::Concat(parts) => parts.iter().map(|p| p.eval(fields)).collect(),
            GenExpr::ConcatWs(sep, parts) => parts
                .iter()
                .map(|p| p.eval(fields))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(sep),
            GenExpr::Lower(e) => e.eval(fields).to_lowercase(),
            GenExpr::Upper(e) => e.eval(fields).to_uppercase(),
            GenExpr::Coalesce(parts) => parts
                .iter()
                .map(|p| p.eval(fields))
                .find(|s| !s.is_empty())
                .unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexHint {
    pub hash: Vec<String>,
    pub range: Vec<String>,
    pub fulltext: Vec<String>,
    pub bm25: Vec<String>,
    pub spatial: Vec<String>,
    /// Vector fields — populated automatically from VECTOR-typed columns,
    /// or explicitly via WITH (vector: ['field']). Reserved for Phase 2 HNSW.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vector: Vec<String>,
    /// Positional search indexes — each entry is a list of fields covered by one index.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub search: Vec<Vec<String>>,
    /// Version at which each index was last built.
    /// Key: `"method:field"` — e.g. `"gin:name"`, `"btree:price"`.
    /// Absent key (or stored 0) means built before versioning was introduced → rebuild.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub build_versions: std::collections::HashMap<String, u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableSchema {
    pub collection: String,
    pub fields: Vec<FieldDef>,
    pub indexes: IndexHint,
}

impl Default for IndexHint {
    fn default() -> Self {
        Self {
            hash: vec!["_key".to_string()],
            range: Vec::new(),
            fulltext: Vec::new(),
            bm25: Vec::new(),
            spatial: Vec::new(),
            vector: Vec::new(),
            search: Vec::new(),
            build_versions: std::collections::HashMap::new(),
        }
    }
}

// ── Parser ────────────────────────────────────────────────────────────────────

enum FieldOrBm25 {
    Field(String),
    Bm25 { field: String, query: String },
    ScoreProjection { expr: ScoreExpr, alias: String },
}

// ── Safety caps on user-controlled query resources ─────────────────────────────
// Reject-with-error (never silent). Generous enough that no legitimate query is
// affected; they only stop pathological/adversarial input from exhausting
// CPU / RAM / the parser stack. Injection is already prevented by typed `$N`
// params (no string interpolation); these guard the DoS surface.
const MAX_HOP_DEPTH: u32 = 100_000;           // graph traversal `*min..max`
const MAX_IN_LIST: usize = 1_000_000;         // `IN (...)` list size
const MAX_INSERT_ROWS: usize = 1_000_000;     // multi-row INSERT tuples
const MAX_VECTOR_K: usize = 1_000_000;        // VECTOR_NEAR k
const MAX_SUBSTRING_LEN: usize = 100_000_000; // SUBSTRING length
const MAX_NEST_DEPTH: usize = 256;            // paren/bracket nesting → parser recursion

/// Max parenthesis/bracket nesting in the token stream. Bounding this before
/// parsing prevents a deeply-nested query from overflowing the recursive-descent
/// parser's stack (the recursion consumes an opening token per level).
fn max_nesting(tokens: &[Tok]) -> usize {
    let (mut depth, mut max) = (0usize, 0usize);
    for t in tokens {
        match t {
            Tok::LParen | Tok::LBracket => { depth += 1; if depth > max { max = depth; } }
            Tok::RParen | Tok::RBracket => { depth = depth.saturating_sub(1); }
            _ => {}
        }
    }
    max
}

/// What a parsed statement needs from the indexes before it can be answered.
///
/// Collected while parsing rather than by walking the finished plan: the plan has
/// five shapes and these calls nest differently in each, so a walk that missed one
/// would restore the silent wrong answer for exactly the shape it missed.
pub(crate) struct PlanNeeds {
    /// Collections a plain `SELECT` reads, which must exist.
    pub(crate) from_tables: Vec<String>,
    /// `SEARCH_SCORE(...)` appears, so some search index must exist.
    pub(crate) search_index: bool,
    /// Fields each `BM25(field, …)` scores against.
    pub(crate) bm25_fields: Vec<String>,
}

struct Parser {
    tokens: Vec<Tok>,
    pos: usize,
    params: Vec<Value>,
    /// How many recursive-descent frames are currently open.
    ///
    /// `max_nesting` bounds parentheses before parsing, and its comment states
    /// the assumption that broke: "the recursion consumes an opening token per
    /// level". So do `NOT` and unary `-`, and neither is a parenthesis —
    /// `WHERE NOT NOT NOT … a = 1` with 200 000 `NOT`s walked straight past the
    /// guard and overflowed the stack. A Rust stack overflow is `SIGABRT`: not an
    /// error, not catchable, the process is gone. One query string, from anyone
    /// able to send SQL.
    ///
    /// Counting frames instead of tokens bounds every recursive path, including
    /// ones added later that no pre-scan would know to look for.
    depth: usize,
    /// Collections a plain `SELECT ... FROM <name>` reads.
    ///
    /// A name that no collection has silently produced no rows, so a typo in a
    /// table name was reported as an empty table. PostgreSQL raises `relation
    /// "x" does not exist`, and this is not a graph question — see the rule that
    /// non-graph query semantics follow PostgreSQL.
    from_tables: Vec<String>,
    /// Set when the statement contains `SEARCH_SCORE(...)`, which reads the
    /// positional search index and scores every row 0.0 without one — an
    /// `ORDER BY` over it then returns the rows in whatever order they were
    /// already in, with nothing to say the ranking never happened.
    ///
    /// A flag rather than a collection name because the score is resolved against
    /// whichever search index holds the document, not the one on the `FROM`
    /// collection, so "any index at all" is the honest requirement.
    needs_search_index: bool,
    /// Fields a `BM25(...)` in this statement needs an index for, recorded as the
    /// call is parsed.
    ///
    /// The alternative was to walk the finished plan looking for BM25 uses, and
    /// the plan has five shapes with the calls nested differently in each — a walk
    /// that missed one would restore the silent wrong answer for exactly the shape
    /// it missed, and nothing would report that it had. Recording the requirement
    /// where the call is *parsed* is the one place every shape passes through.
    bm25_needs: Vec<String>,
}

impl Parser {
    fn new(tokens: Vec<Tok>) -> Self {
        Self { tokens, pos: 0, params: vec![], depth: 0, from_tables: Vec::new(), needs_search_index: false, bm25_needs: Vec::new() }
    }

    fn with_params(tokens: Vec<Tok>, params: Vec<Value>) -> Self {
        Self { tokens, pos: 0, params, depth: 0, from_tables: Vec::new(), needs_search_index: false, bm25_needs: Vec::new() }
    }

    /// Run `f` one level deeper, refusing past [`MAX_NEST_DEPTH`].
    ///
    /// The depth is restored however `f` returns, so an error deep in an
    /// expression does not leave the counter raised for the rest of the parse.
    fn nested<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, SqlError>,
    ) -> Result<T, SqlError> {
        if self.depth >= MAX_NEST_DEPTH {
            return Err(SqlError::InvalidValue(format!(
                "expression nested too deeply (> {MAX_NEST_DEPTH} levels)"
            )));
        }
        self.depth += 1;
        let out = f(self);
        self.depth -= 1;
        out
    }

    /// The index requirements gathered while parsing.
    fn needs(self) -> PlanNeeds {
        PlanNeeds {
            from_tables: self.from_tables,
            search_index: self.needs_search_index,
            bm25_fields: self.bm25_needs,
        }
    }

    fn peek(&self) -> &Tok {
        &self.tokens[self.pos]
    }

    fn advance(&mut self) -> Tok {
        let t = self.tokens[self.pos].clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        t
    }

    fn expect_kw(&mut self, expected: Kw, name: &'static str) -> Result<(), SqlError> {
        match self.peek() {
            Tok::Kw(k) if *k == expected => {
                self.advance();
                Ok(())
            }
            Tok::Eof => Err(SqlError::UnexpectedEnd { expected: name }),
            other => Err(SqlError::UnexpectedToken {
                expected: name,
                got: format!("{other:?}"),
            }),
        }
    }

    fn expect_str(&mut self) -> Result<String, SqlError> {
        match self.advance() {
            Tok::Str(s) => Ok(s),
            Tok::Param(idx) => {
                match self.params.get(idx - 1) {
                    Some(Value::String(s)) => Ok(s.clone()),
                    Some(_) => Err(SqlError::ParamTypeMismatch { index: idx, expected: "string" }),
                    None => Err(SqlError::ParamOutOfRange { index: idx, count: self.params.len() }),
                }
            }
            Tok::Eof => Err(SqlError::UnexpectedEnd {
                expected: "string literal",
            }),
            other => Err(SqlError::UnexpectedToken {
                expected: "string literal",
                got: format!("{other:?}"),
            }),
        }
    }

    fn expect_usize(&mut self) -> Result<usize, SqlError> {
        let n = self.expect_num()?;
        if n < 0.0 || !n.is_finite() {
            return Err(SqlError::InvalidValue(format!("expected non-negative integer, got {n}")));
        }
        Ok(n as usize)
    }

    fn expect_num(&mut self) -> Result<f64, SqlError> {
        match self.advance() {
            Tok::Num(n) => Ok(n),
            Tok::Param(idx) => {
                match self.params.get(idx - 1) {
                    Some(Value::Number(n)) => n.as_f64().ok_or(SqlError::ParamTypeMismatch { index: idx, expected: "number" }),
                    Some(_) => Err(SqlError::ParamTypeMismatch { index: idx, expected: "number" }),
                    None => Err(SqlError::ParamOutOfRange { index: idx, count: self.params.len() }),
                }
            }
            Tok::Eof => Err(SqlError::UnexpectedEnd { expected: "number" }),
            other => Err(SqlError::UnexpectedToken {
                expected: "number",
                got: format!("{other:?}"),
            }),
        }
    }

    fn expect_comma(&mut self) -> Result<(), SqlError> {
        match self.peek() {
            Tok::Comma => {
                self.advance();
                Ok(())
            }
            Tok::Eof => Err(SqlError::UnexpectedEnd { expected: "," }),
            other => Err(SqlError::UnexpectedToken {
                expected: ",",
                got: format!("{other:?}"),
            }),
        }
    }

    fn expect_lparen(&mut self) -> Result<(), SqlError> {
        match self.peek() {
            Tok::LParen => {
                self.advance();
                Ok(())
            }
            Tok::Eof => Err(SqlError::UnexpectedEnd { expected: "(" }),
            other => Err(SqlError::UnexpectedToken {
                expected: "(",
                got: format!("{other:?}"),
            }),
        }
    }

    fn expect_rparen(&mut self) -> Result<(), SqlError> {
        match self.peek() {
            Tok::RParen => {
                self.advance();
                Ok(())
            }
            Tok::Eof => Err(SqlError::UnexpectedEnd { expected: ")" }),
            other => Err(SqlError::UnexpectedToken {
                expected: ")",
                got: format!("{other:?}"),
            }),
        }
    }

    fn expect_lbracket(&mut self) -> Result<(), SqlError> {
        match self.peek() {
            Tok::LBracket => {
                self.advance();
                Ok(())
            }
            Tok::Eof => Err(SqlError::UnexpectedEnd { expected: "[" }),
            other => Err(SqlError::UnexpectedToken {
                expected: "[",
                got: format!("{other:?}"),
            }),
        }
    }

    fn expect_rbracket(&mut self) -> Result<(), SqlError> {
        match self.peek() {
            Tok::RBracket => {
                self.advance();
                Ok(())
            }
            Tok::Eof => Err(SqlError::UnexpectedEnd { expected: "]" }),
            other => Err(SqlError::UnexpectedToken {
                expected: "]",
                got: format!("{other:?}"),
            }),
        }
    }

    fn expect_dot(&mut self) -> Result<(), SqlError> {
        match self.peek() {
            Tok::Dot => {
                self.advance();
                Ok(())
            }
            Tok::Eof => Err(SqlError::UnexpectedEnd { expected: "." }),
            other => Err(SqlError::UnexpectedToken {
                expected: ".",
                got: format!("{other:?}"),
            }),
        }
    }

    fn expect_colon(&mut self) -> Result<(), SqlError> {
        match self.peek() {
            Tok::Colon => {
                self.advance();
                Ok(())
            }
            Tok::Eof => Err(SqlError::UnexpectedEnd { expected: ":" }),
            other => Err(SqlError::UnexpectedToken {
                expected: ":",
                got: format!("{other:?}"),
            }),
        }
    }

    /// True if the next token starts a MATCH `WHERE` filter function.
    fn peek_is_match_filter_fn(&self) -> bool {
        matches!(self.peek(), Tok::Ident(n)
            if matches!(n.to_ascii_uppercase().as_str(), "ST_DWITHIN" | "BM25"))
    }

    /// Parse a MATCH `WHERE` filter function on a hop variable:
    /// `ST_DWithin(var.geo, POINT(lon lat), metres)` or `BM25(var.field,'q') op n`.
    fn parse_match_func_filter(&mut self) -> Result<crate::query::MatchFuncFilter, SqlError> {
        use crate::query::MatchFuncFilter;
        let name = self.expect_ident()?.to_ascii_uppercase();
        match name.as_str() {
            "ST_DWITHIN" => {
                self.expect_lparen()?;
                let _field = self.expect_score_field()?; // var.geo → geo (spatial is field-agnostic)
                self.expect_comma()?;
                let (lon, lat) = self.parse_point_literal()?;
                self.expect_comma()?;
                let m = self.expect_num()?;
                self.expect_rparen()?;
                Ok(MatchFuncFilter::StDWithin { lat, lon, m })
            }
            "BM25" => {
                self.expect_lparen()?;
                let field = self.expect_score_field()?;
                self.expect_comma()?;
                let query = self.expect_str()?;
                self.expect_rparen()?;
                let op = self.parse_cmp_op()?;
                let threshold = self.expect_num()?;
                self.bm25_needs.push(field.clone());
                Ok(MatchFuncFilter::Bm25 { field, query, op, threshold })
            }
            _ => unreachable!("peek_is_match_filter_fn gates this"),
        }
    }

    /// True if the next token starts a scoring function call.
    fn peek_is_score_fn(&self) -> bool {
        matches!(self.peek(), Tok::Ident(name) if matches!(name.to_ascii_uppercase().as_str(),
            "BM25" | "BM25_NORM" | "VECTOR_COSINE" | "VECTOR_L2" | "VECTOR_DOT"
            | "VECTOR_L1" | "ST_DISTANCE" | "SEARCH_SCORE"))
    }

    /// The one-argument scalar functions, for contexts that are not a projection.
    ///
    /// `SELECT LENGTH(name) … ORDER BY LENGTH(name)` parsed the projection but
    /// not the sort: the sort parser read `LENGTH` as a *column name*, stopped at
    /// the `(`, and the rest of the clause was dropped. No column is called
    /// `LENGTH`, so every row tied and the query came back in whatever order the
    /// scan produced — sorted by nothing, with no error.
    ///
    /// Encodes to the same `__FUNC__NAME__arg` sentinel the projection uses, which
    /// `resolve_field` already knows how to evaluate, so the sort works on the
    /// value rather than on the name.
    ///
    /// `None` when the identifier is not a function call, leaving the parser where
    /// it was. Multi-argument forms (`SUBSTRING`, `REPLACE`, `CONCAT`,
    /// `DATE_TRUNC`) are deliberately not here — they are reported as unsupported
    /// in a sort by the end-of-statement check rather than being silently ignored.
    fn parse_scalar_func_call(&mut self, ident: &str) -> Result<Option<String>, SqlError> {
        if !matches!(self.peek(), Tok::LParen) {
            return Ok(None);
        }
        let func = ident.to_uppercase();
        const ONE_ARG: &[&str] = &[
            "LENGTH", "LEN", "LOWER", "UPPER", "TRIM", "LTRIM", "RTRIM",
            "YEAR", "MONTH", "DAY", "HOUR", "MINUTE", "SECOND", "DOW", "QUARTER",
            "AGE_DAYS", "AGE_HOURS", "JSON_ARRAY_LENGTH",
        ];
        if !ONE_ARG.contains(&func.as_str()) {
            return Ok(None);
        }
        self.advance(); // consume (
        let arg = self.expect_ident()?;
        self.expect_rparen()?;
        Ok(Some(format!("__FUNC__{func}__{arg}")))
    }

    /// Consume an optional `ASC`/`DESC`; returns `true` for ascending (default).
    fn parse_sort_dir(&mut self) -> bool {
        if matches!(self.peek(), Tok::Kw(Kw::Desc)) { self.advance(); false }
        else { if matches!(self.peek(), Tok::Kw(Kw::Asc)) { self.advance(); } true }
    }

    /// Parse a MATCH `ORDER BY` clause → `(field_order, score_order)`:
    /// a bare/`var.field` column sort, or a scoring-expression rank.
    fn parse_match_order(&mut self)
        -> Result<(Option<(String, bool)>, Option<(crate::query::ScoreExpr, bool)>), SqlError>
    {
        if !matches!(self.peek(), Tok::Kw(Kw::Order)) {
            return Ok((None, None));
        }
        self.advance();
        self.expect_kw(Kw::By, "BY")?;
        if self.peek_is_score_fn() {
            let expr = self.parse_score_expr()?;
            let asc = self.parse_sort_dir();
            Ok((None, Some((expr, asc))))
        } else {
            let mut alias = self.expect_ident()?;
            if matches!(self.peek(), Tok::Dot) {
                self.advance();
                let field = self.expect_ident()?;
                alias = format!("{alias}.{field}");
            }
            let asc = self.parse_sort_dir();
            Ok((Some((alias, asc)), None))
        }
    }

    /// Parse a field name that may be variable-qualified: `var.field` → `field`.
    /// Lets score functions work both in plain SELECT (`BM25(body,'q')`) and in
    /// MATCH (`BM25(b.body,'q')`) — the `var` qualifier selects the hop node and
    /// is resolved by the executor; only the bare field name is kept here.
    fn expect_score_field(&mut self) -> Result<String, SqlError> {
        let first = self.expect_ident()?;
        if matches!(self.peek(), Tok::Dot) {
            self.advance();
            self.expect_ident() // keep the field, drop the var qualifier
        } else {
            Ok(first)
        }
    }

    fn expect_ident(&mut self) -> Result<String, SqlError> {
        match self.advance() {
            Tok::Ident(s) => Ok(s),
            Tok::Str(s) => Ok(s), // allow quoted identifiers
            // Allow keywords as identifiers (e.g. r.strength, collection named "set")
            Tok::Kw(kw) => Ok(kw_to_str(&kw).to_string()),
            Tok::Eof => Err(SqlError::UnexpectedEnd {
                expected: "identifier",
            }),
            other => Err(SqlError::UnexpectedToken {
                expected: "identifier",
                got: format!("{other:?}"),
            }),
        }
    }

    /// Parse a table reference with an optional schema qualifier: `[schema .] table`.
    /// sekejap has one implicit namespace surfaced as `public` (Postgres' default),
    /// so `public.artist` and `artist` both resolve to the `artist` collection.
    /// Any other schema is rejected — matching Postgres, where only existing
    /// schemas resolve. Lets standard clients (DBeaver, etc.) qualify table names.
    fn parse_table_ref(&mut self) -> Result<String, SqlError> {
        let first = self.expect_ident()?;
        if matches!(self.peek(), Tok::Dot) {
            self.advance(); // consume '.'
            let table = self.expect_ident()?;
            if !first.eq_ignore_ascii_case("public") {
                return Err(SqlError::UndefinedSchema(first));
            }
            return Ok(table);
        }
        Ok(first)
    }

    fn parse_value(&mut self) -> Result<Value, SqlError> {
        // NOW() in WHERE: returns current time as Unix milliseconds (i64),
        // suitable for comparison with _created_unix / _updated_unix fields.
        if let Tok::Ident(ref name) = self.peek().clone() {
            if name.to_uppercase() == "NOW" {
                self.advance(); // consume NOW
                self.expect_lparen()?;
                self.expect_rparen()?;
                let ts = chrono::Utc::now().timestamp_millis();
                return Ok(serde_json::json!(ts));
            }
        }
        // ST_GeomFromGeoJSON('json') — parse a GeoJSON text literal and return
        // it as a JSON object value.  Validates that the string is a GeoJSON
        // geometry (has a "type" field and "coordinates" or "geometries").
        // Used in INSERT INTO … VALUES to insert geometry from a string literal:
        //   INSERT INTO places (_key, geometry) VALUES ('p1', ST_GeomFromGeoJSON('…'))
        if let Tok::Ident(ref name) = self.peek().clone() {
            if name.to_uppercase() == "ST_GEOMFROMGEOJSON" {
                self.advance(); // consume identifier
                self.expect_lparen()?;
                let json_str = self.expect_str()?;
                self.expect_rparen()?;
                let val: Value = serde_json::from_str(&json_str).map_err(|e| {
                    SqlError::InvalidValue(format!("ST_GeomFromGeoJSON: invalid JSON: {e}"))
                })?;
                let valid = val.get("type").is_some()
                    && (val.get("coordinates").is_some()
                        || val.get("geometries").is_some());
                if valid {
                    return Ok(val);
                }
                return Err(SqlError::InvalidValue(
                    "ST_GeomFromGeoJSON: not a valid GeoJSON geometry \
                     (must have \"type\" and \"coordinates\" or \"geometries\")"
                        .into(),
                ));
            }
        }
        // Vector literal: [f32, f32, ...] — used in INSERT/UPDATE for vector fields.
        if matches!(self.peek(), Tok::LBracket) {
            let floats = self.parse_f32_array()?;
            let arr = floats
                .into_iter()
                .map(|f| {
                    serde_json::Number::from_f64(f as f64)
                        .map(Value::Number)
                        .unwrap_or(Value::Null)
                })
                .collect();
            return Ok(Value::Array(arr));
        }
        match self.advance() {
            Tok::Str(s) => Ok(Value::String(s)),
            Tok::Num(n) => {
                // A whole number stays whole. The lexer produces `f64` for every
                // numeric literal, and routing all of them through `from_f64` made
                // `UPDATE p SET n = 5000` store `5000.0` in a column that had held
                // integers — after which every reader asking for an integer skipped
                // the row silently, because a JSON float is not an integer however
                // round it looks. `INSERT ... VALUES (5)` had the same effect.
                //
                // Only values that are exactly representable convert: anything with
                // a fractional part, or beyond what an i64 holds, stays a float.
                Ok(Value::Number(int_or_float(n)
                    .ok_or_else(|| SqlError::InvalidNumber(n.to_string()))?))
            }
            Tok::Kw(Kw::True) => Ok(Value::Bool(true)),
            Tok::Kw(Kw::False) => Ok(Value::Bool(false)),
            Tok::Kw(Kw::Null) => Ok(Value::Null),
            Tok::Param(idx) => {
                self.params.get(idx - 1).cloned().ok_or(SqlError::ParamOutOfRange {
                    index: idx,
                    count: self.params.len(),
                })
            }
            Tok::Eof => Err(SqlError::UnexpectedEnd { expected: "value" }),
            other => Err(SqlError::UnexpectedToken {
                expected: "value",
                got: format!("{other:?}"),
            }),
        }
    }

    // ── JSON path tail ────────────────────────────────────────────────────────

    /// After reading a root identifier, consume any `->` / `->>` JSON path
    /// operators and return an encoded field key:
    ///
    /// - `col->>'key'`             → `__JP_TEXT__col__key`
    /// - `col->'nested'->>'key'`   → `__JP_TEXT__col__nested__key`
    /// - `col->'nested'`           → `__JP_OBJ__col__nested`
    ///
    /// If no path operator follows, the original `root` is returned unchanged.
    ///
    /// **Note**: key segments are joined with `__`. Keys that themselves contain
    /// `__` will be mis-parsed; this mirrors the existing `__FUNC__` convention.
    fn parse_json_path_tail(&mut self, root: String) -> String {
        // Quick check: is there a path operator next?
        if !matches!(self.peek(), Tok::Arrow | Tok::LongArrow) {
            return root;
        }

        let mut segments = vec![root];
        let mut is_text = false;

        loop {
            match self.peek() {
                Tok::LongArrow => {
                    self.advance();
                    let key = self.read_path_key();
                    segments.push(key);
                    is_text = true;
                    break; // ->> is always the terminal operator
                }
                Tok::Arrow => {
                    self.advance();
                    let key = self.read_path_key();
                    segments.push(key);
                    is_text = false;
                    // continue — may chain further ->/->>'s
                }
                _ => break,
            }
        }

        if segments.len() == 1 {
            return segments.into_iter().next().unwrap();
        }

        let prefix = if is_text { "__JP_TEXT__" } else { "__JP_OBJ__" };
        format!("{}{}", prefix, segments.join("__"))
    }

    /// Read the next token as a path segment key (string literal, identifier, or keyword).
    fn read_path_key(&mut self) -> String {
        match self.peek().clone() {
            Tok::Str(s) => { self.advance(); s }
            Tok::Ident(s) => { self.advance(); s }
            Tok::Kw(kw) => { self.advance(); kw_to_str(&kw).to_string() }
            _ => String::new(), // malformed — return empty, parser will error on next token
        }
    }

    // ── Top-level parse ───────────────────────────────────────────────────────

    fn parse(&mut self) -> Result<SelectStmt, SqlError> {
        self.expect_kw(Kw::Select, "SELECT")?;

        let distinct = if matches!(self.peek(), Tok::Kw(Kw::Distinct)) {
            self.advance();
            true
        } else {
            false
        };

        let (fields, score_projections) = self.parse_field_list()?;

        self.expect_kw(Kw::From, "FROM")?;

        let source = self.parse_source()?;

        let mut conditions = Vec::new();
        if matches!(self.peek(), Tok::Kw(Kw::Where)) {
            self.advance();
            conditions = self.parse_conditions()?;
        }

        let mut group_by: Vec<String> = Vec::new();
        if matches!(self.peek(), Tok::Kw(Kw::Group)) {
            self.advance();
            self.expect_kw(Kw::By, "BY")?;
            group_by.push(self.expect_ident()?);
            while matches!(self.peek(), Tok::Comma) {
                self.advance();
                group_by.push(self.expect_ident()?);
            }
        }

        // PG standard: every non-aggregate, non-function SELECT field must appear in
        // the GROUP BY list.  Skipped for SELECT * (fields is empty).
        if !group_by.is_empty() && !fields.is_empty() {
            for f in &fields {
                // Strip __AS__alias\x01 wrapper to expose the inner expression.
                let inner = if let Some(rest) = f.strip_prefix("__AS__") {
                    if let Some(idx) = rest.find('\x01') { &rest[idx + 1..] } else { f.as_str() }
                } else {
                    f.as_str()
                };
                // Aggregates and function sentinels (anything starting with __) are exempt.
                if inner.starts_with("__") { continue; }
                if !group_by.iter().any(|g| g == inner) {
                    return Err(SqlError::GroupByViolation(inner.to_string()));
                }
            }
        }

        let mut having: Vec<CondExpr> = Vec::new();
        if matches!(self.peek(), Tok::Kw(Kw::Having)) {
            self.advance();
            having = self.parse_conditions()?;
        }

        let mut order_by = None;
        if matches!(self.peek(), Tok::Kw(Kw::Order)) {
            self.advance();
            self.expect_kw(Kw::By, "BY")?;

            // Special case: `field <op> [vec]` — vector distance sort (all 4 metrics).
            // Peek ahead: if first token is a plain ident and the next is a vec operator,
            // use the fast vector path.
            let vec_op = if matches!(self.peek(), Tok::Ident(_)) {
                let saved = self.pos;
                self.advance(); // consume ident temporarily
                let op = match self.peek() {
                    Tok::VecCosineOp => Some(crate::query::VecMetric::Cosine),
                    Tok::VecL2Op     => Some(crate::query::VecMetric::L2),
                    Tok::VecDotOp    => Some(crate::query::VecMetric::Dot),
                    Tok::VecL1Op     => Some(crate::query::VecMetric::L1),
                    _ => None,
                };
                self.pos = saved; // roll back
                op
            } else {
                None
            };

            if let Some(metric) = vec_op {
                let field = self.expect_ident()?;
                self.advance(); // consume operator
                let query = self.parse_f32_array_or_param()?;
                order_by = Some(OrderKey::Vector { field, query, metric });
            } else if let Some(sentinel) = {
                // `ORDER BY LENGTH(name)` and friends: parsed here because the
                // score-expression parser below reads `LENGTH` as a column name
                // and stops at the `(`.
                match self.peek().clone() {
                    Tok::Ident(name) if matches!(self.tokens.get(self.pos + 1), Some(Tok::LParen)) => {
                        self.advance();
                        match self.parse_scalar_func_call(&name)? {
                            Some(sentinel) => Some(sentinel),
                            // Not a function we can sort by. Put the identifier
                            // back so the normal path reports it.
                            None => { self.pos -= 1; None }
                        }
                    }
                    _ => None,
                }
            } {
                let ascending = self.parse_sort_dir();
                order_by = Some(OrderKey::Fields(vec![(sentinel, ascending)]));
            } else {
                // Arithmetic score expression (handles plain fields, BM25, VECTOR_SIM,
                // and any combination with +, -, *, /, parentheses).
                let expr = self.parse_score_expr()?;

                // Read optional direction. Score expressions default to DESC
                // (highest score first); plain field sorts default to ASC.
                let is_plain_field = matches!(&expr, ScoreExpr::Field(_));
                let ascending = match self.peek() {
                    Tok::Kw(Kw::Desc) => { self.advance(); false }
                    Tok::Kw(Kw::Asc)  => { self.advance(); true  }
                    _ => is_plain_field, // field → true (ASC), score → false (DESC)
                };

                // Classify the result:
                match expr {
                    // Plain field(s) — use the fast multi-column sort path.
                    ScoreExpr::Field(name) if matches!(self.peek(), Tok::Comma) => {
                        // Multi-column: `field1 [ASC|DESC], field2 [ASC|DESC], ...`
                        let mut cols = vec![(name, ascending)];
                        while matches!(self.peek(), Tok::Comma) {
                            self.advance(); // consume comma
                            let next_ident = self.expect_ident()?;
                            let next_field = self.parse_json_path_tail(next_ident);
                            let next_asc = match self.peek() {
                                Tok::Kw(Kw::Desc) => { self.advance(); false }
                                Tok::Kw(Kw::Asc)  => { self.advance(); true  }
                                _ => true,
                            };
                            cols.push((next_field, next_asc));
                        }
                        order_by = Some(OrderKey::Fields(cols));
                    }
                    ScoreExpr::Field(name) => {
                        // Single-column plain field sort.
                        order_by = Some(OrderKey::Fields(vec![(name, ascending)]));
                    }
                    // Everything else — arithmetic score expression.
                    other => {
                        // A secondary key after a scoring expression has nowhere to
                        // go: `OrderKey::Expr` holds one expression and one
                        // direction. It used to be dropped in silence *along with
                        // everything after it* — `ORDER BY BM25(...) DESC, _key ASC
                        // LIMIT 3` lost the tie-break and the `LIMIT`, so the query
                        // returned every row instead of three.
                        if matches!(self.peek(), Tok::Comma) {
                            return Err(SqlError::UnexpectedToken {
                                expected: "end of ORDER BY",
                                got: "a second sort key after a scoring expression — \
                                      not supported; sort by the score alone".into(),
                            });
                        }
                        order_by = Some(OrderKey::Expr(other, ascending));
                    }
                }
            }
        }

        let mut limit = None;
        let mut offset = None;
        loop {
            match self.peek() {
                Tok::Kw(Kw::Limit) => {
                    self.advance();
                    limit = Some(self.expect_usize()?);
                }
                Tok::Kw(Kw::Offset) => {
                    self.advance();
                    offset = Some(self.expect_usize()?);
                }
                _ => break,
            }
        }

        Ok(SelectStmt {
            fields,
            source,
            conditions,
            group_by,
            having,
            distinct,
            order_by,
            limit,
            offset,
            score_projections,
        })
    }

    fn parse_field_list(&mut self) -> Result<(Vec<String>, Vec<(ScoreExpr, String)>), SqlError> {
        // `a.*` → `*` — strip a table-alias qualifier on a star projection
        // (DBeaver and other clients emit `SELECT a.* FROM t a`).
        if matches!(self.tokens.get(self.pos), Some(Tok::Ident(_)))
            && matches!(self.tokens.get(self.pos + 1), Some(Tok::Dot))
            && matches!(self.tokens.get(self.pos + 2), Some(Tok::Star))
        {
            self.advance(); // alias
            self.advance(); // dot
        }
        if matches!(self.peek(), Tok::Star) {
            self.advance();
            // Check for trailing score projections: SELECT *, BM25(...) AS score
            let mut score_projections = Vec::new();
            while matches!(self.peek(), Tok::Comma) {
                self.advance();
                let field = self.parse_field_or_bm25()?;
                match field {
                    FieldOrBm25::ScoreProjection { expr, alias } => {
                        score_projections.push((expr, alias));
                    }
                    FieldOrBm25::Bm25 { field, query } => {
                        let alias = format!("_bm25_{}_score", field);
                        score_projections.push((ScoreExpr::Bm25 { field, query }, alias));
                    }
                    FieldOrBm25::Field(_) => {
                        return Err(SqlError::InvalidValue("cannot mix * with plain fields; use score functions with AS alias after *".into()));
                    }
                }
            }
            return Ok((vec![], score_projections));
        }
        let mut fields = Vec::new();
        let mut score_projections = Vec::new();
        loop {
            let field = self.parse_field_or_bm25()?;
            match field {
                FieldOrBm25::Field(f) => fields.push(f),
                FieldOrBm25::Bm25 { field, query } => {
                    let alias = format!("_bm25_{}_score", field);
                    score_projections.push((ScoreExpr::Bm25 { field, query }, alias));
                }
                FieldOrBm25::ScoreProjection { expr, alias } => {
                    score_projections.push((expr, alias));
                }
            }
            if matches!(self.peek(), Tok::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        Ok((fields, score_projections))
    }

    fn parse_field_or_bm25(&mut self) -> Result<FieldOrBm25, SqlError> {
        // CASE WHEN … END — must be intercepted before expect_ident(), which
        // would otherwise swallow the CASE keyword as a bare field name.
        if matches!(self.peek(), Tok::Kw(Kw::Case)) {
            return self.parse_plain_case_projection();
        }
        // Strip an optional table-alias qualifier: `a.col` → `col`. sekejap selects
        // from a single collection, so the alias is cosmetic. Only when a real
        // column identifier follows the dot (leaves functions/`a.*` untouched).
        if matches!(self.tokens.get(self.pos), Some(Tok::Ident(_)))
            && matches!(self.tokens.get(self.pos + 1), Some(Tok::Dot))
            && matches!(self.tokens.get(self.pos + 2), Some(Tok::Ident(_)) | Some(Tok::Str(_)))
        {
            self.advance(); // alias
            self.advance(); // dot
        }
        let ident = self.expect_ident()?;
        // Score functions → parse as full ScoreExpr, optional AS alias
        if matches!(self.peek(), Tok::LParen) {
            let upper = ident.to_uppercase();
            if matches!(upper.as_str(),
                "BM25" | "BM25_NORM" | "VECTOR_COSINE" | "VECTOR_L2"
                | "VECTOR_DOT" | "VECTOR_L1" | "ST_DISTANCE" | "SEARCH_SCORE"
            ) {
                // Back up so parse_score_expr can consume the ident
                self.pos -= 1;
                let expr = self.parse_score_expr()?;
                // Check for optional AS alias
                if matches!(self.peek(), Tok::Kw(Kw::As)) {
                    self.advance();
                    let alias = self.expect_ident()?;
                    return Ok(FieldOrBm25::ScoreProjection { expr, alias });
                }
                // No AS alias — use legacy auto-naming for bare BM25
                if let ScoreExpr::Bm25 { ref field, ref query } = expr {
                    return Ok(FieldOrBm25::Bm25 { field: field.clone(), query: query.clone() });
                }
                return Err(SqlError::InvalidValue(
                    "score expression in SELECT requires AS alias".into(),
                ));
            }
        }
        if matches!(self.peek(), Tok::LParen) && ident.to_uppercase() == "ST_CENTROID" {
            self.advance();
            let field = self.expect_ident()?;
            self.expect_rparen()?;
            return Ok(FieldOrBm25::Field(format!("__ST_Centroid__{}", field)));
        }
        // ST_AsGeoJSON(field) — serialise a geometry field to a GeoJSON text
        // string in the SELECT list, matching PostGIS semantics.
        // Supports an optional AS alias:
        //   SELECT ST_AsGeoJSON(geometry) AS geom FROM places
        if matches!(self.peek(), Tok::LParen) && ident.to_uppercase() == "ST_ASGEOJSON" {
            self.advance(); // consume (
            let field = self.expect_ident()?;
            self.expect_rparen()?;
            let encoded = format!("__ST_AsGeoJSON__{}", field);
            // Optional AS alias — output key defaults to the inner field name.
            let encoded = if matches!(self.peek(), Tok::Kw(Kw::As)) {
                self.advance();
                let alias = self.expect_ident()?;
                format!("__AS__{}\x01{}", alias, encoded)
            } else {
                encoded
            };
            return Ok(FieldOrBm25::Field(encoded));
        }
        if matches!(self.peek(), Tok::LParen) {
            let func_upper = ident.to_uppercase();
            if matches!(
                func_upper.as_str(),
                "LENGTH" | "LEN" | "LOWER" | "UPPER" | "TRIM" | "LTRIM" | "RTRIM"
            ) {
                self.advance();
                let arg = self.expect_ident()?;
                self.expect_rparen()?;
                return Ok(FieldOrBm25::Field(format!(
                    "__FUNC__{}__{}",
                    func_upper, arg
                )));
            }
            if func_upper == "SUBSTRING" {
                self.advance();
                let arg1 = self.expect_ident()?;
                self.expect_comma()?;
                let start = self.expect_num()?;
                self.expect_comma()?;
                let len = self.expect_num()?;
                self.expect_rparen()?;
                if !start.is_finite() || !len.is_finite() || start < 0.0 || len < 0.0
                    || len > MAX_SUBSTRING_LEN as f64
                {
                    return Err(SqlError::InvalidValue(
                        "SUBSTRING start/length must be finite, non-negative, and bounded".into(),
                    ));
                }
                return Ok(FieldOrBm25::Field(format!(
                    "__FUNC__SUBSTRING__{}__{}_{}",
                    arg1, start as usize, len as usize
                )));
            }
            if func_upper == "REPLACE" {
                self.advance();
                let arg1 = self.expect_ident()?;
                self.expect_comma()?;
                let old = self.expect_str()?;
                self.expect_comma()?;
                let new = self.expect_str()?;
                self.expect_rparen()?;
                return Ok(FieldOrBm25::Field(format!(
                    "__FUNC__REPLACE__{}__{}__{}",
                    arg1, old, new
                )));
            }
            if func_upper == "CONCAT" {
                self.advance();
                let arg1 = self.expect_ident()?;
                self.expect_comma()?;
                let arg2 = self.expect_str()?;
                self.expect_rparen()?;
                return Ok(FieldOrBm25::Field(format!(
                    "__FUNC__CONCAT__{}__{}",
                    arg1, arg2
                )));
            }
            if func_upper == "NOW" {
                self.advance();
                self.expect_rparen()?;
                let expr = "__FUNC__NOW__".to_string();
                let expr = if matches!(self.peek(), Tok::Kw(Kw::As)) {
                    self.advance();
                    let alias = self.expect_ident()?;
                    format!("__AS__{}\x01{}", alias, expr)
                } else { expr };
                return Ok(FieldOrBm25::Field(expr));
            }
            if matches!(
                func_upper.as_str(),
                "YEAR" | "MONTH" | "DAY" | "HOUR" | "MINUTE" | "SECOND" | "DOW" | "QUARTER"
                | "AGE_DAYS" | "AGE_HOURS" | "JSON_ARRAY_LENGTH"
            ) {
                self.advance();
                let arg = self.expect_ident()?;
                self.expect_rparen()?;
                let expr = format!("__FUNC__{}__{}",  func_upper, arg);
                let expr = if matches!(self.peek(), Tok::Kw(Kw::As)) {
                    self.advance();
                    let alias = self.expect_ident()?;
                    format!("__AS__{}\x01{}", alias, expr)
                } else { expr };
                return Ok(FieldOrBm25::Field(expr));
            }
            if func_upper == "DATE_TRUNC" {
                self.advance(); // consume (
                let unit = self.expect_str()?;
                self.expect_comma()?;
                let arg = self.expect_ident()?;
                self.expect_rparen()?;
                let expr = format!("__FUNC__DATE_TRUNC__{}__{}",  unit, arg);
                let expr = if matches!(self.peek(), Tok::Kw(Kw::As)) {
                    self.advance();
                    let alias = self.expect_ident()?;
                    format!("__AS__{}\x01{}", alias, expr)
                } else { expr };
                return Ok(FieldOrBm25::Field(expr));
            }
            if func_upper == "UUIDV4" {
                self.advance();
                self.expect_rparen()?;
                return Ok(FieldOrBm25::Field("__FUNC__UUIDV4__".to_string()));
            }
            if func_upper == "UUIDV5" {
                self.advance();
                let namespace = self.expect_str()?;
                self.expect_comma()?;
                let name = self.expect_str()?;
                self.expect_rparen()?;
                return Ok(FieldOrBm25::Field(format!(
                    "__FUNC__UUIDV5__{}__{}",
                    namespace, name
                )));
            }
            // Aggregate functions: COUNT(*|field), SUM(field), AVG(field), MIN(field), MAX(field)
            if matches!(
                func_upper.as_str(),
                "COUNT" | "SUM" | "AVG" | "MIN" | "MAX"
            ) {
                self.advance(); // consume (
                let mut distinct = false;
                let arg = if func_upper == "COUNT" && matches!(self.peek(), Tok::Star) {
                    self.advance(); // consume *
                    "*".to_string()
                } else {
                    // COUNT(DISTINCT field) — consume the DISTINCT keyword first.
                    if func_upper == "COUNT" && matches!(self.peek(), Tok::Kw(Kw::Distinct)) {
                        self.advance();
                        distinct = true;
                    }
                    self.expect_ident()?
                };
                self.expect_rparen()?;
                let func_token = if distinct { "COUNTD" } else { func_upper.as_str() };
                let expr = format!("__AGG__{}__{}", func_token, arg);
                // Check for AS alias
                let expr = if matches!(self.peek(), Tok::Kw(Kw::As)) {
                    self.advance();
                    let alias = self.expect_ident()?;
                    format!("__AS__{}\x01{}", alias, expr)
                } else {
                    expr
                };
                return Ok(FieldOrBm25::Field(expr));
            }
        }
        // Check for JSON path operators (-> / ->>) in SELECT field list.
        let ident = self.parse_json_path_tail(ident);
        // Check for AS alias
        let ident = if matches!(self.peek(), Tok::Kw(Kw::As)) {
            self.advance();
            let alias = self.expect_ident()?;
            format!("__AS__{}\x01{}", alias, ident)
        } else {
            ident
        };
        Ok(FieldOrBm25::Field(ident))
    }

    /// Parse a `CASE WHEN <field> <op> <literal> THEN <literal> [ … ] [ELSE
    /// <literal>] END [AS alias]` expression in a plain (non-MATCH) SELECT list.
    ///
    /// Conditions reference bare row fields (no `var.` qualifier).  The whole
    /// expression is JSON-encoded into the `__CASE__<json>` projection sentinel
    /// so it rides inside the existing flat `Vec<String>` field list; the
    /// executor decodes and evaluates it per row (see `PlainCase`).  THEN/ELSE
    /// values are literals, matching the MATCH surface's CASE.
    fn parse_plain_case_projection(&mut self) -> Result<FieldOrBm25, SqlError> {
        use crate::query::{CaseCond, CmpOp, PlainCase};
        self.advance(); // consume CASE
        let mut branches: Vec<(CaseCond, Value)> = Vec::new();
        let mut else_val = Value::Null;
        loop {
            match self.peek().clone() {
                Tok::Kw(Kw::When) => {
                    self.advance(); // consume WHEN
                    let field = self.expect_ident()?;
                    let op: CmpOp = self.parse_cmp_op()?;
                    let val = self.parse_value()?;
                    self.expect_kw(Kw::Then, "THEN")?;
                    let then_val = self.parse_value()?;
                    branches.push((CaseCond { var: String::new(), field, op, val }, then_val));
                }
                Tok::Kw(Kw::Else) => {
                    self.advance(); // consume ELSE
                    else_val = self.parse_value()?;
                }
                _ => break,
            }
        }
        self.expect_kw(Kw::End, "END")?;
        if branches.is_empty() {
            return Err(SqlError::InvalidValue(
                "CASE requires at least one WHEN branch".into(),
            ));
        }
        let pc = PlainCase { branches, else_val };
        let json = serde_json::to_string(&pc)
            .map_err(|e| SqlError::InvalidValue(format!("CASE encoding failed: {e}")))?;
        let encoded = format!("__CASE__{}", json);
        // Optional AS alias — otherwise the output column defaults to "case".
        let encoded = if matches!(self.peek(), Tok::Kw(Kw::As)) {
            self.advance();
            let alias = self.expect_ident()?;
            format!("__AS__{}\x01{}", alias, encoded)
        } else {
            encoded
        };
        Ok(FieldOrBm25::Field(encoded))
    }

    fn parse_source(&mut self) -> Result<Source, SqlError> {
        match self.peek().clone() {
            Tok::Kw(Kw::All) => {
                self.advance();
                Ok(Source::All)
            }
            Tok::Ident(name) => {
                self.advance();
                // Optional `schema.` qualifier — `public` is transparent.
                if matches!(self.peek(), Tok::Dot) {
                    self.advance();
                    let table = self.expect_ident()?;
                    if !name.eq_ignore_ascii_case("public") {
                        return Err(SqlError::UndefinedSchema(name));
                    }
                    self.from_tables.push(table.clone());
                    return Ok(Source::Collection(table));
                }
                self.from_tables.push(name.clone());
                Ok(Source::Collection(name))
            }
            Tok::Eof => Err(SqlError::UnexpectedEnd {
                expected: "collection name or ALL",
            }),
            other => Err(SqlError::UnexpectedToken {
                expected: "collection name or ALL",
                got: format!("{other:?}"),
            }),
        }
    }

    fn parse_conditions(&mut self) -> Result<Vec<CondExpr>, SqlError> {
        // Parse: and_group (OR and_group)*
        // where and_group = condition (AND condition)*
        let first_group = self.parse_and_group()?;

        if !matches!(self.peek(), Tok::Kw(Kw::Or)) {
            // No OR — return conditions directly (most common path)
            return Ok(first_group);
        }

        // OR present — collect groups and wrap in a single Or CondExpr.
        let mut groups = vec![first_group];
        while matches!(self.peek(), Tok::Kw(Kw::Or)) {
            self.advance();
            groups.push(self.parse_and_group()?);
        }
        Ok(vec![CondExpr::Or(groups)])
    }

    fn parse_and_group(&mut self) -> Result<Vec<CondExpr>, SqlError> {
        let mut conds = vec![self.parse_condition()?];
        while matches!(self.peek(), Tok::Kw(Kw::And)) {
            self.advance();
            conds.push(self.parse_condition()?);
        }
        Ok(conds)
    }

    /// Given an already-resolved `field` string, parse the comparison RHS:
    /// `= val | != val | > val | < val | >= val | <= val | BETWEEN | IN | NOT IN | LIKE | ILIKE`.
    fn parse_field_compare(&mut self, field: String) -> Result<CondExpr, SqlError> {
        match self.peek().clone() {
            Tok::Eq => {
                self.advance();
                // = ANY($1) or = ANY(['a','b']) — PostgreSQL-style dynamic IN.
                if matches!(self.peek(), Tok::Kw(Kw::Any)) {
                    self.advance(); // consume ANY
                    self.expect_lparen()?;
                    // Try parameter first, then inline array literal.
                    let values = if let Tok::Param(idx) = self.peek().clone() {
                        self.advance();
                        match self.params.get(idx - 1) {
                            Some(Value::Array(arr)) => arr.clone(),
                            Some(v) => vec![v.clone()], // scalar → single-element IN
                            None => return Err(SqlError::ParamOutOfRange {
                                index: idx, count: self.params.len(),
                            }),
                        }
                    } else if matches!(self.peek(), Tok::LBracket) {
                        // Inline array literal: ['a', 'b', 3, true]
                        self.advance(); // consume [
                        let mut arr = Vec::new();
                        loop {
                            match self.peek().clone() {
                                Tok::RBracket => { self.advance(); break; }
                                Tok::Comma => { self.advance(); }
                                _ => arr.push(self.parse_value()?),
                            }
                        }
                        arr
                    } else {
                        // Single value: = ANY(some_val)
                        let v = self.parse_value()?;
                        match v {
                            Value::Array(arr) => arr,
                            scalar => vec![scalar],
                        }
                    };
                    self.expect_rparen()?;
                    return if values.len() == 1 {
                        Ok(CondExpr::Compare { field, op: CompareOp::Eq, value: values.into_iter().next().unwrap() })
                    } else {
                        Ok(CondExpr::In { field, values })
                    };
                }
                let v = self.parse_value()?;
                Ok(CondExpr::Compare { field, op: CompareOp::Eq, value: v })
            }
            Tok::Neq => {
                self.advance();
                let v = self.parse_value()?;
                Ok(CondExpr::Compare { field, op: CompareOp::Neq, value: v })
            }
            Tok::Gt => {
                self.advance();
                let v = self.parse_value()?;
                Ok(CondExpr::Compare { field, op: CompareOp::Gt, value: v })
            }
            Tok::Lt => {
                self.advance();
                let v = self.parse_value()?;
                Ok(CondExpr::Compare { field, op: CompareOp::Lt, value: v })
            }
            Tok::Gte => {
                self.advance();
                let v = self.parse_value()?;
                Ok(CondExpr::Compare { field, op: CompareOp::Gte, value: v })
            }
            Tok::Lte => {
                self.advance();
                let v = self.parse_value()?;
                Ok(CondExpr::Compare { field, op: CompareOp::Lte, value: v })
            }
            Tok::ArrayContains => {
                self.advance();
                // @> $1 (bound array param) or @> ['a', 'b'] (inline array)
                let values = if let Tok::Param(idx) = self.peek().clone() {
                    self.advance();
                    match self.params.get(idx - 1) {
                        Some(Value::Array(arr)) => arr.clone(),
                        Some(v) => vec![v.clone()],
                        None => return Err(SqlError::ParamOutOfRange {
                            index: idx, count: self.params.len(),
                        }),
                    }
                } else if matches!(self.peek(), Tok::LBracket) {
                    self.advance(); // consume [
                    let mut arr = Vec::new();
                    loop {
                        match self.peek().clone() {
                            Tok::RBracket => { self.advance(); break; }
                            Tok::Comma => { self.advance(); }
                            _ => arr.push(self.parse_value()?),
                        }
                    }
                    arr
                } else {
                    // Single value: field @> 'crime'
                    vec![self.parse_value()?]
                };
                Ok(CondExpr::ArrayContains { field, values })
            }
            Tok::Kw(Kw::Between) => {
                self.advance();
                let lo = self.expect_num()?;
                self.expect_kw(Kw::And, "AND")?;
                let hi = self.expect_num()?;
                Ok(CondExpr::Between { field, lo, hi })
            }
            Tok::Kw(Kw::In) => {
                self.advance();
                self.expect_lparen()?;
                let mut values = vec![self.parse_value()?];
                while matches!(self.peek(), Tok::Comma) {
                    self.advance();
                    values.push(self.parse_value()?);
                }
                if values.len() > MAX_IN_LIST {
                    return Err(SqlError::InvalidValue(format!("IN list too large (> {MAX_IN_LIST})")));
                }
                self.expect_rparen()?;
                Ok(CondExpr::In { field, values })
            }
            Tok::Kw(Kw::Not) => {
                self.advance();
                self.expect_kw(Kw::In, "IN")?;
                self.expect_lparen()?;
                let mut values = vec![self.parse_value()?];
                while matches!(self.peek(), Tok::Comma) {
                    self.advance();
                    values.push(self.parse_value()?);
                }
                if values.len() > MAX_IN_LIST {
                    return Err(SqlError::InvalidValue(format!("IN list too large (> {MAX_IN_LIST})")));
                }
                self.expect_rparen()?;
                Ok(CondExpr::Not(Box::new(CondExpr::In { field, values })))
            }
            Tok::Kw(Kw::Like) => {
                self.advance();
                let pattern = self.expect_str()?;
                Ok(CondExpr::Like { field, pattern, case_insensitive: false })
            }
            Tok::Kw(Kw::ILike) => {
                self.advance();
                let pattern = self.expect_str()?;
                Ok(CondExpr::Like { field, pattern, case_insensitive: true })
            }
            Tok::Kw(Kw::Is) => {
                self.advance();
                let negated = if matches!(self.peek(), Tok::Kw(Kw::Not)) {
                    self.advance();
                    true
                } else {
                    false
                };
                self.expect_kw(Kw::Null, "NULL")?;
                Ok(CondExpr::IsNull { field, negated })
            }
            Tok::Eof => Err(SqlError::UnexpectedEnd { expected: "comparison operator" }),
            other => Err(SqlError::UnexpectedToken {
                expected: "comparison operator (=, !=, <>, >, <, >=, <=, BETWEEN, IN, NOT IN, LIKE, ILIKE)",
                got: format!("{other:?}"),
            }),
        }
    }

    fn parse_condition(&mut self) -> Result<CondExpr, SqlError> {
        self.nested(Self::parse_condition_inner)
    }

    fn parse_condition_inner(&mut self) -> Result<CondExpr, SqlError> {
        // Parenthesized group: (a OR b) AND c, (a AND b), etc.
        if matches!(self.peek(), Tok::LParen) {
            self.advance(); // consume (
            let mut inner = self.parse_conditions()?;
            self.expect_rparen()?;
            return Ok(match inner.len() {
                0 => return Err(SqlError::UnexpectedToken {
                    expected: "condition inside parens",
                    got: "empty parens".to_string(),
                }),
                1 => inner.remove(0),
                // Multiple AND conditions — wrap as single-branch OR (acts as AND)
                _ => CondExpr::Or(vec![inner]),
            });
        }

        // NOT prefix: NOT <condition>
        if matches!(self.peek(), Tok::Kw(Kw::Not)) {
            self.advance();
            let inner = self.parse_condition()?;
            return Ok(CondExpr::Not(Box::new(inner)));
        }

        let field = self.expect_ident()?;

        // IS NULL / IS NOT NULL
        if matches!(self.peek(), Tok::Kw(Kw::Is)) {
            self.advance(); // consume IS
            let negated = if matches!(self.peek(), Tok::Kw(Kw::Not)) {
                self.advance(); // consume NOT
                true
            } else {
                false
            };
            self.expect_kw(Kw::Null, "NULL")?;
            return Ok(CondExpr::IsNull {
                field: self.parse_json_path_tail(field),
                negated,
            });
        }

        // Aggregate functions in HAVING: COUNT(*) > 5, SUM(price) < 100, etc.
        // expect_ident() returns lowercase keyword names ("count", "sum", …)
        let upper = field.to_uppercase();
        if matches!(upper.as_str(), "COUNT" | "SUM" | "AVG" | "MIN" | "MAX")
            && matches!(self.peek(), Tok::LParen)
        {
            self.advance(); // consume (
            let mut distinct = false;
            let arg = if upper == "COUNT" && matches!(self.peek(), Tok::Star) {
                self.advance();
                "*".to_string()
            } else {
                if upper == "COUNT" && matches!(self.peek(), Tok::Kw(Kw::Distinct)) {
                    self.advance();
                    distinct = true;
                }
                self.expect_ident()?
            };
            self.expect_rparen()?;
            let func_token = if distinct { "COUNTD" } else { upper.as_str() };
            let agg_key = format!("__AGG__{}__{}", func_token, arg);
            // Parse comparison operator + value (same logic as normal field below)
            return match self.peek().clone() {
                Tok::Gt => {
                    self.advance();
                    let v = self.parse_value()?;
                    Ok(CondExpr::Compare { field: agg_key, op: CompareOp::Gt, value: v })
                }
                Tok::Lt => {
                    self.advance();
                    let v = self.parse_value()?;
                    Ok(CondExpr::Compare { field: agg_key, op: CompareOp::Lt, value: v })
                }
                Tok::Gte => {
                    self.advance();
                    let v = self.parse_value()?;
                    Ok(CondExpr::Compare { field: agg_key, op: CompareOp::Gte, value: v })
                }
                Tok::Lte => {
                    self.advance();
                    let v = self.parse_value()?;
                    Ok(CondExpr::Compare { field: agg_key, op: CompareOp::Lte, value: v })
                }
                Tok::Eq => {
                    self.advance();
                    let v = self.parse_value()?;
                    Ok(CondExpr::Compare { field: agg_key, op: CompareOp::Eq, value: v })
                }
                Tok::Neq => {
                    self.advance();
                    let v = self.parse_value()?;
                    Ok(CondExpr::Compare { field: agg_key, op: CompareOp::Neq, value: v })
                }
                other => Err(SqlError::UnexpectedToken {
                    expected: "comparison operator after aggregate function",
                    got: format!("{other:?}"),
                }),
            };
        }

        // Spatial function calls: ST_DWithin(...), ST_Contains(...), etc.
        if matches!(
            upper.as_str(),
            "ST_DWITHIN"
                | "ST_CONTAINS"
                | "ST_WITHIN"
                | "ST_INTERSECTS"
                | "ST_DISTANCE"
                | "ST_LENGTH"
                | "ST_PERIMETER"
                | "ST_AREA"
        ) {
            return self.parse_spatial_function(&upper);
        }

        // VECTOR_NEAR similarity search: VECTOR_NEAR(field, [f32, ...], k)
        if upper == "VECTOR_NEAR" {
            return self.parse_vector_near_function();
        }

        // Date scalar functions on the LHS: YEAR(field) > 2023, MONTH(created_at) = 4, etc.
        if matches!(
            upper.as_str(),
            "YEAR" | "MONTH" | "DAY" | "HOUR" | "MINUTE" | "SECOND" | "DOW" | "QUARTER"
        ) && matches!(self.peek(), Tok::LParen)
        {
            self.advance(); // consume (
            let arg = self.expect_ident()?;
            self.expect_rparen()?;
            let func_field = format!("__FUNC__{}__{}",  upper, arg);
            return self.parse_field_compare(func_field);
        }
        if upper == "DATE_TRUNC" && matches!(self.peek(), Tok::LParen) {
            self.advance(); // consume (
            let unit = self.expect_str()?;
            self.expect_comma()?;
            let arg = self.expect_ident()?;
            self.expect_rparen()?;
            let func_field = format!("__FUNC__DATE_TRUNC__{}__{}",  unit, arg);
            return self.parse_field_compare(func_field);
        }

        // SEARCH('query text' [, typo => N]) — positional search index filter.
        // `typo => N` overrides the auto edit-distance (0 = exact, 1–2 = tolerant).
        if upper == "SEARCH" {
            self.expect_lparen()?;
            let query = self.expect_str()?;
            let mut typo = None;
            if matches!(self.peek(), Tok::Comma) {
                self.advance();
                let arg = self.expect_ident()?; // "typo"
                if !arg.eq_ignore_ascii_case("typo") {
                    return Err(SqlError::UnexpectedToken { expected: "typo", got: arg });
                }
                // accept `typo => N` or `typo = N`  (`=>` tokenizes as `=` then `>`)
                if !matches!(self.peek(), Tok::Eq) {
                    return Err(SqlError::UnexpectedToken {
                        expected: "=> or =", got: format!("{:?}", self.peek()) });
                }
                self.advance();                               // =
                if matches!(self.peek(), Tok::Gt) { self.advance(); } // the > of =>
                typo = Some(self.expect_num()? as u32);
            }
            self.expect_rparen()?;
            return Ok(CondExpr::Search { query, typo });
        }

        // BM25 full-text search: BM25(field, 'query') > min_score
        if upper == "BM25" {
            let bm25_expr = self.parse_bm25_function()?;
            // After BM25(field, 'query'), expect > min_score
            match self.peek().clone() {
                Tok::Gt => {
                    self.advance();
                    let min_score = self.expect_num()?;
                    if let CondExpr::Bm25Func { field, query } = bm25_expr {
                        Ok(CondExpr::Bm25 {
                            field,
                            query,
                            min_score,
                        })
                    } else {
                        unreachable!()
                    }
                }
                Tok::Gte => {
                    self.advance();
                    let min_score = self.expect_num()?;
                    if let CondExpr::Bm25Func { field, query } = bm25_expr {
                        Ok(CondExpr::Bm25 {
                            field,
                            query,
                            min_score, // For >=, we use the value directly (user can adjust)
                        })
                    } else {
                        unreachable!()
                    }
                }
                other => Err(SqlError::UnexpectedToken {
                    expected: "> or >= after BM25(...)",
                    got: format!("{other:?}"),
                }),
            }
        } else {
            // Resolve any JSON path operators (-> / ->>) after the field name.
            let field = self.parse_json_path_tail(field);
            self.parse_field_compare(field)
        }
    }

    // ── Spatial parsing ─────────────────────────────────────────────────

    /// Parse a spatial function call after the function name has been consumed.
    fn parse_spatial_function(&mut self, func: &str) -> Result<CondExpr, SqlError> {
        self.expect_lparen()?;
        let geom_field = self.expect_ident()?; // the geometry field
        self.expect_comma()?;

        match func {
            "ST_DWITHIN" => {
                // ST_DWithin(geometry, POINT(lon lat), distance_metres)
                let (lon, lat) = self.parse_point_literal()?;
                self.expect_comma()?;
                let distance = self.expect_num()?;
                self.expect_rparen()?;
                Ok(CondExpr::StDWithin {
                    lat,
                    lon,
                    distance_m: distance,
                })
            }
            "ST_CONTAINS" => {
                // POINT → StContainsPoint, POLYGON → StContains
                let next = self.expect_ident()?;
                match next.to_uppercase().as_str() {
                    "POINT" => {
                        self.expect_lparen()?;
                        let lon = self.expect_num()?;
                        let lat = self.expect_num()?;
                        self.expect_rparen()?;
                        self.expect_rparen()?;
                        Ok(CondExpr::StContainsPoint { lat, lon })
                    }
                    "POLYGON" => {
                        let ring = self.parse_polygon_literal()?;
                        self.expect_rparen()?;
                        Ok(CondExpr::StContains { ring })
                    }
                    _ => Err(SqlError::UnexpectedToken {
                        expected: "POINT or POLYGON",
                        got: next,
                    }),
                }
            }
            "ST_WITHIN" => {
                let ring = self.parse_polygon_with_keyword()?;
                self.expect_rparen()?;
                Ok(CondExpr::StWithin { ring })
            }
            "ST_INTERSECTS" => {
                let ring = self.parse_polygon_with_keyword()?;
                self.expect_rparen()?;
                Ok(CondExpr::StIntersects { ring })
            }
            "ST_DISTANCE" => {
                // ST_Distance(geometry, POINT(lon lat), max_metres)
                // The field was already consumed by the shared prefix.
                let (lon, lat) = self.parse_point_literal()?;
                self.expect_comma()?;
                let max_m = self.expect_num()?;
                self.expect_rparen()?;
                Ok(CondExpr::StDistance {
                    field: geom_field,
                    lat,
                    lon,
                    max_m,
                })
            }
            "ST_LENGTH" => {
                // ST_Length(geometry, min_metres)
                let min_m = self.expect_num()?;
                self.expect_rparen()?;
                Ok(CondExpr::StLength { field: geom_field, min_m })
            }
            "ST_PERIMETER" => {
                // ST_Perimeter(geometry, min_metres)
                let min_m = self.expect_num()?;
                self.expect_rparen()?;
                Ok(CondExpr::StPerimeter { field: geom_field, min_m })
            }
            "ST_AREA" => {
                // ST_Area(geometry, min_square_metres)
                let min_m2 = self.expect_num()?;
                self.expect_rparen()?;
                Ok(CondExpr::StArea { field: geom_field, min_m2 })
            }
            _ => Err(SqlError::UnexpectedToken {
                expected: "spatial function",
                got: func.to_string(),
            }),
        }
    }

    /// Parse BM25(field, 'query') → CondExpr::Bm25Func.
    /// The BM25 keyword has already been consumed by parse_condition.
    fn parse_bm25_function(&mut self) -> Result<CondExpr, SqlError> {
        self.expect_lparen()?;
        let field = self.expect_ident()?;
        self.expect_comma()?;
        let query = self.expect_str()?;
        self.expect_rparen()?;
        self.bm25_needs.push(field.clone());
        Ok(CondExpr::Bm25Func { field, query })
    }

    /// Parse `VECTOR_NEAR(field, [f32, ...], k)`.
    fn parse_vector_near_function(&mut self) -> Result<CondExpr, SqlError> {
        self.expect_lparen()?;
        let field = self.expect_ident()?;
        self.expect_comma()?;
        let query = self.parse_f32_array_or_param()?;
        self.expect_comma()?;
        let k = self.expect_usize()?;
        if k > MAX_VECTOR_K {
            return Err(SqlError::InvalidValue(format!("VECTOR_NEAR k too large (> {MAX_VECTOR_K})")));
        }
        self.expect_rparen()?;
        Ok(CondExpr::VectorNear { field, query, k })
    }

    /// Parse an f32 vector: either `[num, num, ...]` literal or `$N` param.
    fn parse_f32_array_or_param(&mut self) -> Result<Vec<f32>, SqlError> {
        if let Tok::Param(idx) = self.peek().clone() {
            self.advance();
            return match self.params.get(idx - 1) {
                Some(Value::Array(arr)) => {
                    arr.iter()
                        .map(|v| v.as_f64().map(|f| f as f32).ok_or(SqlError::ParamTypeMismatch {
                            index: idx,
                            expected: "array of numbers",
                        }))
                        .collect()
                }
                Some(_) => Err(SqlError::ParamTypeMismatch { index: idx, expected: "array of numbers" }),
                None => Err(SqlError::ParamOutOfRange { index: idx, count: self.params.len() }),
            };
        }
        self.parse_f32_array()
    }

    /// Parse a JSON-style f32 array literal: `[ num, num, ... ]`.
    fn parse_f32_array(&mut self) -> Result<Vec<f32>, SqlError> {
        self.expect_lbracket()?;
        let mut values = Vec::new();
        loop {
            match self.peek().clone() {
                Tok::RBracket => {
                    self.advance();
                    break;
                }
                Tok::Comma => {
                    self.advance();
                }
                Tok::Num(n) => {
                    self.advance();
                    values.push(n as f32);
                }
                Tok::Dash => {
                    self.advance();
                    match self.advance() {
                        Tok::Num(n) => values.push(-(n as f32)),
                        other => {
                            return Err(SqlError::UnexpectedToken {
                                expected: "number after -",
                                got: format!("{other:?}"),
                            })
                        }
                    }
                }
                other => {
                    return Err(SqlError::UnexpectedToken {
                        expected: "number or ]",
                        got: format!("{other:?}"),
                    })
                }
            }
        }
        Ok(values)
    }

    // ── Score expression parser (arithmetic ORDER BY) ─────────────────────────

    /// Entry point: parse a score expression with `+` / `-` at the top level.
    ///
    /// Grammar:
    /// ```text
    /// score_expr  = score_mul ( ('+' | '-') score_mul )*
    /// score_mul   = score_unary ( ('*' | '/') score_unary )*
    /// score_unary = '-' score_unary | score_atom
    /// score_atom  = '(' score_expr ')'
    ///             | BM25 '(' ident ',' string ')'
    ///             | VECTOR_COSINE '(' ident ',' f32_array ')'
    ///             | VECTOR_L2 '(' ident ',' f32_array ')'
    ///             | VECTOR_DOT '(' ident ',' f32_array ')'
    ///             | VECTOR_L1 '(' ident ',' f32_array ')'
    ///             | ST_DISTANCE '(' ident ',' POINT '(' lon lat ')' ')'
    ///             | ident [ json_path_tail ]
    ///             | number
    /// ```
    fn parse_score_expr(&mut self) -> Result<ScoreExpr, SqlError> {
        self.nested(Self::parse_score_expr_inner)
    }

    fn parse_score_expr_inner(&mut self) -> Result<ScoreExpr, SqlError> {
        let mut left = self.parse_score_mul()?;
        loop {
            match self.peek() {
                Tok::Plus => {
                    self.advance();
                    let right = self.parse_score_mul()?;
                    left = ScoreExpr::Add(Box::new(left), Box::new(right));
                }
                Tok::Dash => {
                    // Guard: don't consume '->' or '->>' (Arrow / LongArrow are
                    // their own tokens, so plain Dash here is always subtraction).
                    self.advance();
                    let right = self.parse_score_mul()?;
                    left = ScoreExpr::Sub(Box::new(left), Box::new(right));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_score_mul(&mut self) -> Result<ScoreExpr, SqlError> {
        self.nested(Self::parse_score_mul_inner)
    }

    fn parse_score_mul_inner(&mut self) -> Result<ScoreExpr, SqlError> {
        let mut left = self.parse_score_unary()?;
        loop {
            match self.peek() {
                Tok::Star => {
                    self.advance();
                    let right = self.parse_score_unary()?;
                    left = ScoreExpr::Mul(Box::new(left), Box::new(right));
                }
                Tok::Slash => {
                    self.advance();
                    let right = self.parse_score_unary()?;
                    left = ScoreExpr::Div(Box::new(left), Box::new(right));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_score_unary(&mut self) -> Result<ScoreExpr, SqlError> {
        self.nested(Self::parse_score_unary_inner)
    }

    fn parse_score_unary_inner(&mut self) -> Result<ScoreExpr, SqlError> {
        if matches!(self.peek(), Tok::Dash) {
            self.advance();
            let inner = self.parse_score_unary()?;
            return Ok(ScoreExpr::Neg(Box::new(inner)));
        }
        self.parse_score_atom()
    }

    fn parse_score_atom(&mut self) -> Result<ScoreExpr, SqlError> {
        match self.peek().clone() {
            Tok::Num(n) => {
                self.advance();
                Ok(ScoreExpr::Lit(n))
            }
            Tok::Param(idx) => {
                self.advance();
                match self.params.get(idx - 1) {
                    Some(Value::Number(n)) => Ok(ScoreExpr::Lit(n.as_f64().unwrap_or(0.0))),
                    Some(_) => Err(SqlError::ParamTypeMismatch { index: idx, expected: "number" }),
                    None => Err(SqlError::ParamOutOfRange { index: idx, count: self.params.len() }),
                }
            }
            Tok::LParen => {
                self.advance(); // consume '('
                let inner = self.parse_score_expr()?;
                self.expect_rparen()?;
                Ok(inner)
            }
            Tok::Ident(name) => {
                self.advance();
                match name.to_ascii_uppercase().as_str() {
                    "BM25" => {
                        self.expect_lparen()?;
                        let field = self.expect_score_field()?;
                        self.expect_comma()?;
                        let query = self.expect_str()?;
                        self.expect_rparen()?;
                        self.bm25_needs.push(field.clone());
                        Ok(ScoreExpr::Bm25 { field, query })
                    }
                    "BM25_NORM" => {
                        // BM25_NORM(field, 'query' [, k]) → BM25 saturated to [0,1].
                        self.expect_lparen()?;
                        let field = self.expect_score_field()?;
                        self.expect_comma()?;
                        let query = self.expect_str()?;
                        // Optional midpoint k. Default 1.0 — sekejap's BM25
                        // scores run small (sub-1 to low single digits), so a
                        // small k gives a usable [0,1] spread. Tune per corpus.
                        let k = if matches!(self.peek(), Tok::Comma) {
                            self.advance();
                            self.expect_num()?
                        } else {
                            1.0
                        };
                        self.expect_rparen()?;
                        self.bm25_needs.push(field.clone());
                        Ok(ScoreExpr::Bm25Norm { field, query, k })
                    }
                    "SEARCH_SCORE" => {
                        self.expect_lparen()?;
                        let query = self.expect_str()?;
                        self.expect_rparen()?;
                        self.needs_search_index = true;
                        Ok(ScoreExpr::SearchScore { query })
                    }
                    "VECTOR_COSINE" => {
                        self.expect_lparen()?;
                        let field = self.expect_score_field()?;
                        self.expect_comma()?;
                        let query = self.parse_f32_array_or_param()?;
                        self.expect_rparen()?;
                        Ok(ScoreExpr::VectorCosine { field, query })
                    }
                    "VECTOR_L2" => {
                        self.expect_lparen()?;
                        let field = self.expect_score_field()?;
                        self.expect_comma()?;
                        let query = self.parse_f32_array_or_param()?;
                        self.expect_rparen()?;
                        Ok(ScoreExpr::VectorL2 { field, query })
                    }
                    "VECTOR_DOT" => {
                        self.expect_lparen()?;
                        let field = self.expect_score_field()?;
                        self.expect_comma()?;
                        let query = self.parse_f32_array_or_param()?;
                        self.expect_rparen()?;
                        Ok(ScoreExpr::VectorDot { field, query })
                    }
                    "VECTOR_L1" => {
                        self.expect_lparen()?;
                        let field = self.expect_score_field()?;
                        self.expect_comma()?;
                        let query = self.parse_f32_array_or_param()?;
                        self.expect_rparen()?;
                        Ok(ScoreExpr::VectorL1 { field, query })
                    }
                    "ST_DISTANCE" => {
                        // ST_Distance(field, POINT(lon lat)) — metres, geodesic.
                        self.expect_lparen()?;
                        let field = self.expect_score_field()?;
                        self.expect_comma()?;
                        let (lon, lat) = self.parse_point_literal()?;
                        self.expect_rparen()?;
                        Ok(ScoreExpr::StDistance { field, lat, lon })
                    }
                    _ => {
                        // Plain field name, with optional JSON path (col->'key'->>'leaf').
                        let field = self.parse_json_path_tail(name);
                        Ok(ScoreExpr::Field(field))
                    }
                }
            }
            other => Err(SqlError::UnexpectedToken {
                expected: "score expression (number, field, BM25, SEARCH_SCORE, VECTOR_COSINE, VECTOR_L2, VECTOR_DOT, VECTOR_L1, ST_DISTANCE, or parentheses)",
                got: format!("{other:?}"),
            }),
        }
    }

    /// Parse `POINT(lon lat)` → `(lon, lat)`.
    fn parse_point_literal(&mut self) -> Result<(f64, f64), SqlError> {
        let kw = self.expect_ident()?;
        if kw.to_uppercase() != "POINT" {
            return Err(SqlError::UnexpectedToken {
                expected: "POINT",
                got: kw,
            });
        }
        self.expect_lparen()?;
        let lon = self.expect_num()?;
        let lat = self.expect_num()?;
        self.expect_rparen()?;
        Ok((lon, lat))
    }

    /// Parse `((lon1 lat1, lon2 lat2, ...))` → `Vec<[lat, lon]>`.
    fn parse_polygon_literal(&mut self) -> Result<Vec<[f64; 2]>, SqlError> {
        self.expect_lparen()?; // outer (
        self.expect_lparen()?; // inner (
        let mut ring = Vec::new();
        loop {
            let lon = self.expect_num()?;
            let lat = self.expect_num()?;
            ring.push([lat, lon]); // internal format: [lat, lon]
            if matches!(self.peek(), Tok::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.expect_rparen()?; // inner )
        self.expect_rparen()?; // outer )
        Ok(ring)
    }

    /// Parse `POLYGON((lon1 lat1, ...))` → `Vec<[lat, lon]>`.
    fn parse_polygon_with_keyword(&mut self) -> Result<Vec<[f64; 2]>, SqlError> {
        let kw = self.expect_ident()?;
        if kw.to_uppercase() != "POLYGON" {
            return Err(SqlError::UnexpectedToken {
                expected: "POLYGON",
                got: kw,
            });
        }
        self.parse_polygon_literal()
    }

    /// Parse: INSERT INTO collection (_key, field, ...) VALUES ('key', val, ...) [, (...), ...]
    /// Called after INSERT has already been consumed.
    /// Supports multi-row INSERT: VALUES (a, b), (c, d), (e, f)
    fn parse_insert_node(&mut self) -> Result<InsertStmt, SqlError> {
        self.expect_kw(Kw::Into, "INTO")?;
        let collection = self.parse_table_ref()?;
        self.expect_lparen()?;
        let mut fields = vec![self.expect_ident()?];
        while matches!(self.peek(), Tok::Comma) {
            self.advance();
            fields.push(self.expect_ident()?);
        }
        self.expect_rparen()?;
        self.expect_kw(Kw::Values, "VALUES")?;
        // Parse first tuple
        self.expect_lparen()?;
        let mut values = vec![self.parse_value()?];
        while matches!(self.peek(), Tok::Comma) {
            self.advance();
            values.push(self.parse_value()?);
        }
        self.expect_rparen()?;
        let mut rows = vec![values];
        // Parse additional tuples: , (val, val, ...)
        while matches!(self.peek(), Tok::Comma) {
            self.advance();
            self.expect_lparen()?;
            let mut values = vec![self.parse_value()?];
            while matches!(self.peek(), Tok::Comma) {
                self.advance();
                values.push(self.parse_value()?);
            }
            self.expect_rparen()?;
            rows.push(values);
            if rows.len() > MAX_INSERT_ROWS {
                return Err(SqlError::InvalidValue(format!(
                    "INSERT has too many rows (> {MAX_INSERT_ROWS})")));
            }
        }
        Ok(InsertStmt {
            collection,
            fields,
            rows,
        })
    }

    /// Parse: ('from')-[:KIND {strength: n, key: val}]->('to') [, ...]
    /// Called after INSERT has already been consumed.
    fn parse_insert_edge(&mut self) -> Result<Vec<EdgeInsert>, SqlError> {
        let mut edges = Vec::new();
        loop {
            // (from)
            self.expect_lparen()?;
            let from = self.expect_str()?;
            self.expect_rparen()?;
            // -[
            match self.peek() {
                Tok::Dash => {
                    self.advance();
                }
                Tok::Eof => return Err(SqlError::UnexpectedEnd { expected: "-" }),
                other => {
                    return Err(SqlError::UnexpectedToken {
                        expected: "-",
                        got: format!("{other:?}"),
                    })
                }
            }
            match self.peek() {
                Tok::LBracket => {
                    self.advance();
                }
                Tok::Eof => return Err(SqlError::UnexpectedEnd { expected: "[" }),
                other => {
                    return Err(SqlError::UnexpectedToken {
                        expected: "[",
                        got: format!("{other:?}"),
                    })
                }
            }
            // :KIND
            match self.peek() {
                Tok::Colon => {
                    self.advance();
                }
                Tok::Eof => return Err(SqlError::UnexpectedEnd { expected: ":" }),
                other => {
                    return Err(SqlError::UnexpectedToken {
                        expected: ":",
                        got: format!("{other:?}"),
                    })
                }
            }
            let edge_type = self.expect_ident()?;
            // Optional {props}
            let mut props_json = None;
            if matches!(self.peek(), Tok::LBrace) {
                self.advance();
                let mut map = serde_json::Map::new();
                while !matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                    let key = self.expect_ident()?;
                    match self.peek() {
                        Tok::Colon => {
                            self.advance();
                        }
                        Tok::Eof => return Err(SqlError::UnexpectedEnd { expected: ":" }),
                        other => {
                            return Err(SqlError::UnexpectedToken {
                                expected: ":",
                                got: format!("{other:?}"),
                            })
                        }
                    }
                    let val = self.parse_value()?;
                    map.insert(key, val);
                    if matches!(self.peek(), Tok::Comma) {
                        self.advance();
                    }
                }
                match self.peek() {
                    Tok::RBrace => {
                        self.advance();
                    }
                    _ => return Err(SqlError::UnexpectedEnd { expected: "}" }),
                }
                // No privileged keys: every prop (including any `strength`) rides
                // through to the edge as an ordinary attribute, routed by value type.
                if !map.is_empty() {
                    props_json = Some(
                        serde_json::to_string(&Value::Object(map))
                            .map_err(|e| SqlError::InvalidValue(e.to_string()))?,
                    );
                }
            }
            // ]->
            match self.peek() {
                Tok::RBracket => {
                    self.advance();
                }
                Tok::Eof => return Err(SqlError::UnexpectedEnd { expected: "]" }),
                other => {
                    return Err(SqlError::UnexpectedToken {
                        expected: "]",
                        got: format!("{other:?}"),
                    })
                }
            }
            match self.peek() {
                Tok::Arrow => {
                    self.advance();
                }
                Tok::Eof => return Err(SqlError::UnexpectedEnd { expected: "->" }),
                other => {
                    return Err(SqlError::UnexpectedToken {
                        expected: "->",
                        got: format!("{other:?}"),
                    })
                }
            }
            // (to)
            self.expect_lparen()?;
            let to = self.expect_str()?;
            self.expect_rparen()?;

            edges.push(EdgeInsert {
                from,
                to,
                edge_type,
                props_json,
            });

            if matches!(self.peek(), Tok::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        Ok(edges)
    }

    /// Parse: DELETE FROM collection|ALL [WHERE ...]
    /// Called after DELETE has already been consumed.
    fn parse_delete_node(&mut self) -> Result<DeleteStmt, SqlError> {
        self.expect_kw(Kw::From, "FROM")?;
        let source = self.parse_source()?;
        let mut conditions = Vec::new();
        if matches!(self.peek(), Tok::Kw(Kw::Where)) {
            self.advance();
            conditions = self.parse_conditions()?;
        }
        Ok(DeleteStmt { source, conditions })
    }

    /// Parse: ('from')-[:KIND]->('to') [, ...]
    /// Called after DELETE has already been consumed.
    fn parse_delete_edge(&mut self) -> Result<Vec<EdgeDelete>, SqlError> {
        let mut edges = Vec::new();
        loop {
            // (from)
            self.expect_lparen()?;
            let from = self.expect_str()?;
            self.expect_rparen()?;
            // -[
            match self.peek() {
                Tok::Dash => {
                    self.advance();
                }
                Tok::Eof => return Err(SqlError::UnexpectedEnd { expected: "-" }),
                other => {
                    return Err(SqlError::UnexpectedToken {
                        expected: "-",
                        got: format!("{other:?}"),
                    })
                }
            }
            match self.peek() {
                Tok::LBracket => {
                    self.advance();
                }
                Tok::Eof => return Err(SqlError::UnexpectedEnd { expected: "[" }),
                other => {
                    return Err(SqlError::UnexpectedToken {
                        expected: "[",
                        got: format!("{other:?}"),
                    })
                }
            }
            // :KIND
            match self.peek() {
                Tok::Colon => {
                    self.advance();
                }
                Tok::Eof => return Err(SqlError::UnexpectedEnd { expected: ":" }),
                other => {
                    return Err(SqlError::UnexpectedToken {
                        expected: ":",
                        got: format!("{other:?}"),
                    })
                }
            }
            let edge_type = self.expect_ident()?;
            // Optional {k:v, …} predicate — delete only edges matching these attrs.
            let mut props: Option<Value> = None;
            if matches!(self.peek(), Tok::LBrace) {
                self.advance();
                let mut map = serde_json::Map::new();
                while !matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                    let key = self.expect_ident()?;
                    match self.peek() {
                        Tok::Colon => { self.advance(); }
                        other => return Err(SqlError::UnexpectedToken {
                            expected: ":", got: format!("{other:?}"),
                        }),
                    }
                    let val = self.parse_value()?;
                    map.insert(key, val);
                    if matches!(self.peek(), Tok::Comma) { self.advance(); }
                }
                match self.peek() {
                    Tok::RBrace => { self.advance(); }
                    other => return Err(SqlError::UnexpectedToken {
                        expected: "}", got: format!("{other:?}"),
                    }),
                }
                props = Some(Value::Object(map));
            }
            // ]->
            match self.peek() {
                Tok::RBracket => {
                    self.advance();
                }
                Tok::Eof => return Err(SqlError::UnexpectedEnd { expected: "]" }),
                other => {
                    return Err(SqlError::UnexpectedToken {
                        expected: "]",
                        got: format!("{other:?}"),
                    })
                }
            }
            match self.peek() {
                Tok::Arrow => {
                    self.advance();
                }
                Tok::Eof => return Err(SqlError::UnexpectedEnd { expected: "->" }),
                other => {
                    return Err(SqlError::UnexpectedToken {
                        expected: "->",
                        got: format!("{other:?}"),
                    })
                }
            }
            // (to)
            self.expect_lparen()?;
            let to = self.expect_str()?;
            self.expect_rparen()?;

            edges.push(EdgeDelete {
                from,
                to,
                edge_type,
                props,
            });

            if matches!(self.peek(), Tok::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        Ok(edges)
    }

    fn parse_update(&mut self) -> Result<UpdateStmt, SqlError> {
        self.expect_kw(Kw::Update, "UPDATE")?;
        let collection = self.parse_table_ref()?;
        self.expect_kw(Kw::Set, "SET")?;
        // Parse one or more  field = value  pairs separated by commas
        let mut updates = Vec::new();
        loop {
            let field = self.expect_ident()?;
            match self.peek() {
                Tok::Eq => {
                    self.advance();
                }
                Tok::Eof => return Err(SqlError::UnexpectedEnd { expected: "=" }),
                other => {
                    return Err(SqlError::UnexpectedToken {
                        expected: "=",
                        got: format!("{other:?}"),
                    })
                }
            }
            let value = self.parse_value()?;
            updates.push((field, value));
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.advance(); // consume comma
        }
        let mut conditions = Vec::new();
        if matches!(self.peek(), Tok::Kw(Kw::Where)) {
            self.advance();
            conditions = self.parse_conditions()?;
        }
        Ok(UpdateStmt {
            collection,
            updates,
            conditions,
        })
    }

    /// Parse `UPDATE ('a')-[:TYPE {pred}]->('b') SET k=v[, …] [WHERE k=v [AND …]]`.
    /// The relationship is the mutation subject: `SET` targets the edge; endpoints
    /// and predicate only SELECT which edges to change.
    fn parse_update_edge(&mut self) -> Result<CompiledMutation, SqlError> {
        self.expect_kw(Kw::Update, "UPDATE")?;
        // ('from')
        self.expect_lparen()?;
        let from = self.expect_str()?;
        self.expect_rparen()?;
        self.eat(Tok::Dash, "-")?;
        self.eat(Tok::LBracket, "[")?;
        self.eat(Tok::Colon, ":")?;
        let edge_type = self.expect_ident()?;
        // optional inline {k:v} predicate
        let mut predicate = serde_json::Map::new();
        if matches!(self.peek(), Tok::LBrace) {
            self.advance();
            while !matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                let key = self.expect_ident()?;
                self.eat(Tok::Colon, ":")?;
                predicate.insert(key, self.parse_value()?);
                if matches!(self.peek(), Tok::Comma) { self.advance(); }
            }
            self.eat(Tok::RBrace, "}")?;
        }
        self.eat(Tok::RBracket, "]")?;
        self.eat(Tok::Arrow, "->")?;
        // ('to')
        self.expect_lparen()?;
        let to = self.expect_str()?;
        self.expect_rparen()?;
        // SET k=v[, …]  (SET is on the edge; an optional `r.` qualifier is stripped)
        self.expect_kw(Kw::Set, "SET")?;
        let mut sets = Vec::new();
        loop {
            let mut field = self.expect_ident()?;
            if matches!(self.peek(), Tok::Dot) { self.advance(); field = self.expect_ident()?; }
            if field == "_key" {
                return Err(SqlError::InvalidValue(
                    "cannot UPDATE _key (edge identity); delete and re-insert instead".into(),
                ));
            }
            match self.peek() {
                Tok::Eq => { self.advance(); }
                other => return Err(SqlError::UnexpectedToken { expected: "=", got: format!("{other:?}") }),
            }
            sets.push((field, self.parse_value()?));
            if matches!(self.peek(), Tok::Comma) { self.advance(); } else { break; }
        }
        // optional WHERE field=value [AND …]  — equality predicates on the edge
        if matches!(self.peek(), Tok::Kw(Kw::Where)) {
            self.advance();
            loop {
                let mut field = self.expect_ident()?;
                if matches!(self.peek(), Tok::Dot) { self.advance(); field = self.expect_ident()?; }
                match self.peek() {
                    Tok::Eq => { self.advance(); }
                    other => return Err(SqlError::UnexpectedToken { expected: "=", got: format!("{other:?}") }),
                }
                predicate.insert(field, self.parse_value()?);
                if matches!(self.peek(), Tok::Kw(Kw::And)) { self.advance(); } else { break; }
            }
        }
        Ok(CompiledMutation::UpdateEdge {
            from,
            to,
            edge_type,
            predicate: if predicate.is_empty() { None } else { Some(Value::Object(predicate)) },
            sets,
        })
    }

    /// Consume the next token if it matches `t` (by variant), else error `name`.
    fn eat(&mut self, t: Tok, name: &'static str) -> Result<(), SqlError> {
        if std::mem::discriminant(self.peek()) == std::mem::discriminant(&t) {
            self.advance();
            Ok(())
        } else {
            Err(SqlError::UnexpectedToken { expected: name, got: format!("{:?}", self.peek()) })
        }
    }

    /// Parse an optional `DEFAULT UUIDV4()` or `DEFAULT UUIDV5('ns', 'name')` clause.
    /// Returns `(default_uuid4, default_uuid5)`. Consumes tokens only when DEFAULT is present.
    fn parse_field_default(p: &mut Self) -> Result<(bool, Option<(String, String)>), SqlError> {
        if !matches!(p.peek(), Tok::Kw(Kw::Default)) {
            return Ok((false, None));
        }
        p.advance(); // consume DEFAULT
        if let Tok::Ident(ref name) = p.peek().clone() {
            let upper = name.to_uppercase();
            if upper == "UUIDV4" {
                p.advance();
                p.expect_lparen()?;
                p.expect_rparen()?;
                return Ok((true, None));
            } else if upper == "UUIDV5" {
                p.advance();
                p.expect_lparen()?;
                let ns = p.expect_str()?;
                p.expect_comma()?;
                let nm = p.expect_str()?;
                p.expect_rparen()?;
                return Ok((false, Some((ns, nm))));
            }
        }
        // DEFAULT followed by something we don't handle — ignore silently
        Ok((false, None))
    }

    /// Parse an optional `GENERATED ALWAYS AS (<expr>) STORED` clause. `GENERATED`,
    /// `ALWAYS`, and `STORED` are contextual identifiers (not reserved keywords).
    fn parse_generated(&mut self) -> Result<Option<GenExpr>, SqlError> {
        let is_generated = matches!(self.peek(), Tok::Ident(s) if s.eq_ignore_ascii_case("generated"));
        if !is_generated {
            return Ok(None);
        }
        self.advance(); // GENERATED
        // Optional `ALWAYS`
        if matches!(self.peek(), Tok::Ident(s) if s.eq_ignore_ascii_case("always")) {
            self.advance();
        }
        self.expect_kw(Kw::As, "AS")?;
        self.expect_lparen()?;
        let expr = self.parse_gen_expr()?;
        self.expect_rparen()?;
        // Optional `STORED` (the only mode we support; VIRTUAL is not).
        if matches!(self.peek(), Tok::Ident(s) if s.eq_ignore_ascii_case("stored")) {
            self.advance();
        }
        Ok(Some(expr))
    }

    /// Parse a generated-column expression: `a || b`, `concat_ws(sep, …)`,
    /// `lower/upper(x)`, `coalesce(…)`, field refs, and string literals.
    fn parse_gen_expr(&mut self) -> Result<GenExpr, SqlError> {
        let mut parts = vec![self.parse_gen_primary()?];
        while matches!(self.peek(), Tok::Concat) {
            self.advance(); // ||
            parts.push(self.parse_gen_primary()?);
        }
        Ok(if parts.len() == 1 { parts.pop().unwrap() } else { GenExpr::Concat(parts) })
    }

    fn parse_gen_primary(&mut self) -> Result<GenExpr, SqlError> {
        match self.peek().clone() {
            Tok::Str(s) => { self.advance(); Ok(GenExpr::Lit(s)) }
            Tok::LParen => { self.advance(); let e = self.parse_gen_expr()?; self.expect_rparen()?; Ok(e) }
            Tok::Ident(name) => {
                self.advance();
                let upper = name.to_ascii_uppercase();
                if matches!(self.peek(), Tok::LParen)
                    && matches!(upper.as_str(), "CONCAT" | "CONCAT_WS" | "LOWER" | "UPPER" | "COALESCE")
                {
                    self.expect_lparen()?;
                    let expr = match upper.as_str() {
                        "LOWER" => { let e = self.parse_gen_expr()?; GenExpr::Lower(Box::new(e)) }
                        "UPPER" => { let e = self.parse_gen_expr()?; GenExpr::Upper(Box::new(e)) }
                        "CONCAT_WS" => {
                            let sep = self.expect_str()?;
                            let mut args = Vec::new();
                            while matches!(self.peek(), Tok::Comma) {
                                self.advance();
                                args.push(self.parse_gen_expr()?);
                            }
                            GenExpr::ConcatWs(sep, args)
                        }
                        "COALESCE" | "CONCAT" => {
                            let mut args = vec![self.parse_gen_expr()?];
                            while matches!(self.peek(), Tok::Comma) {
                                self.advance();
                                args.push(self.parse_gen_expr()?);
                            }
                            if upper == "COALESCE" { GenExpr::Coalesce(args) } else { GenExpr::Concat(args) }
                        }
                        _ => unreachable!(),
                    };
                    self.expect_rparen()?;
                    Ok(expr)
                } else {
                    Ok(GenExpr::Field(name))
                }
            }
            other => Err(SqlError::UnexpectedToken {
                expected: "generated expression (field, string, ||, concat_ws, lower, upper, coalesce)",
                got: format!("{other:?}"),
            }),
        }
    }

    fn parse_create_table(&mut self) -> Result<TableSchema, SqlError> {
        self.expect_kw(Kw::Table, "TABLE")?;
        let collection = self.parse_table_ref()?;
        self.expect_lparen()?;

        let mut fields = Vec::new();
        loop {
            let field_name = self.expect_ident()?;
            let ty = self.parse_type()?;
            let is_primary_key =
                if field_name == "_key" && matches!(self.peek(), Tok::Kw(Kw::Primary)) {
                    self.expect_kw(Kw::Primary, "PRIMARY")?;
                    self.expect_kw(Kw::Key, "KEY")?;
                    true
                } else {
                    false
                };
            let is_timestamptz = field_name.ends_with("_at") || field_name.ends_with("_time");
            let default_now = is_timestamptz;
            let (default_uuid4, default_uuid5) = Self::parse_field_default(self)?;
            let generated = self.parse_generated()?;
            fields.push(FieldDef {
                name: field_name,
                ty,
                is_primary_key,
                is_timestamptz,
                default_now,
                default_uuid4,
                default_uuid5,
                generated,
            });
            if matches!(self.peek(), Tok::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.expect_rparen()?;

        // If the user didn't declare _key, inject it with DEFAULT UUIDV4() PRIMARY KEY.
        // Every collection must have a _key; making it auto-UUID is the safe default.
        if !fields.iter().any(|f| f.name == "_key") {
            fields.insert(
                0,
                FieldDef {
                    name: "_key".to_string(),
                    ty: FieldType::Text,
                    is_primary_key: true,
                    is_timestamptz: false,
                    default_now: false,
                    default_uuid4: true,
                    default_uuid5: None,
                    generated: None,
                },
            );
        }

        let mut schema = TableSchema {
            collection,
            fields,
            indexes: IndexHint::default(),
        };

        schema.fields.push(FieldDef {
            name: "_created_unix".to_string(),
            ty: FieldType::Integer,
            is_primary_key: false,
            is_timestamptz: false,
            default_now: true,
            default_uuid4: false,
            default_uuid5: None,
            generated: None,
        });
        schema.fields.push(FieldDef {
            name: "_updated_unix".to_string(),
            ty: FieldType::Integer,
            is_primary_key: false,
            is_timestamptz: false,
            default_now: true,
            default_uuid4: false,
            default_uuid5: None,
            generated: None,
        });

        Ok(schema)
    }

    /// Parse: ALTER TABLE collection <op>
    /// Called after ALTER has already been consumed by parse_mutation.
    ///
    /// Supported forms:
    /// - `ADD [COLUMN] name type [PRIMARY KEY] [NOT NULL]`
    /// - `DROP [COLUMN] [IF EXISTS] name`
    /// - `RENAME COLUMN old TO new`
    /// - `RENAME TO new_name`
    /// - `ALTER [COLUMN] name TYPE new_type`
    fn parse_alter_table(&mut self) -> Result<CompiledMutation, SqlError> {
        self.expect_kw(Kw::Table, "TABLE")?;
        let collection = self.parse_table_ref()?;

        match self.peek().clone() {
            // ADD [COLUMN] name type [PRIMARY KEY] [NOT NULL]
            Tok::Kw(Kw::Add) => {
                self.advance(); // consume ADD
                if matches!(self.peek(), Tok::Kw(Kw::Column)) {
                    self.advance(); // optional COLUMN
                }
                let col_name = self.expect_ident()?;
                let ty = self.parse_type()?;
                let mut is_primary_key = false;
                loop {
                    match self.peek().clone() {
                        Tok::Kw(Kw::Primary) => {
                            self.advance();
                            self.expect_kw(Kw::Key, "KEY")?;
                            is_primary_key = true;
                        }
                        Tok::Kw(Kw::Not) => {
                            self.advance();
                            // consume NULL — NOT NULL is noted but we don't track nullability yet
                            if matches!(self.peek(), Tok::Kw(Kw::Null)) {
                                self.advance();
                            }
                        }
                        _ => break,
                    }
                }
                let is_timestamptz = col_name.ends_with("_at") || col_name.ends_with("_time");
                let (default_uuid4, default_uuid5) = Self::parse_field_default(self)?;
                let generated = self.parse_generated()?;
                let def = FieldDef {
                    name: col_name,
                    ty,
                    is_primary_key,
                    is_timestamptz,
                    default_now: is_timestamptz,
                    default_uuid4,
                    default_uuid5,
                    generated,
                };
                Ok(CompiledMutation::AlterTable {
                    collection,
                    op: AlterTableOp::AddColumn { def },
                })
            }

            // DROP [COLUMN] [IF EXISTS] name
            Tok::Kw(Kw::Drop) => {
                self.advance(); // consume DROP
                if matches!(self.peek(), Tok::Kw(Kw::Column)) {
                    self.advance(); // optional COLUMN
                }
                let if_exists = if matches!(self.peek(), Tok::Kw(Kw::If)) {
                    self.advance();
                    self.expect_kw(Kw::Exists, "EXISTS")?;
                    true
                } else {
                    false
                };
                let name = self.expect_ident()?;
                Ok(CompiledMutation::AlterTable {
                    collection,
                    op: AlterTableOp::DropColumn { name, if_exists },
                })
            }

            // RENAME COLUMN old TO new  |  RENAME TO new_name
            Tok::Kw(Kw::Rename) => {
                self.advance(); // consume RENAME
                match self.peek().clone() {
                    Tok::Kw(Kw::Column) => {
                        self.advance(); // consume COLUMN
                        let old_name = self.expect_ident()?;
                        self.expect_kw(Kw::To, "TO")?;
                        let new_name = self.expect_ident()?;
                        Ok(CompiledMutation::AlterTable {
                            collection,
                            op: AlterTableOp::RenameColumn { old_name, new_name },
                        })
                    }
                    Tok::Kw(Kw::To) => {
                        self.advance(); // consume TO
                        let new_name = self.expect_ident()?;
                        Ok(CompiledMutation::AlterTable {
                            collection,
                            op: AlterTableOp::RenameTable { new_name },
                        })
                    }
                    other => Err(SqlError::UnexpectedToken {
                        expected: "COLUMN or TO",
                        got: format!("{other:?}"),
                    }),
                }
            }

            // ALTER [COLUMN] name TYPE new_type
            Tok::Kw(Kw::Alter) => {
                self.advance(); // consume ALTER
                if matches!(self.peek(), Tok::Kw(Kw::Column)) {
                    self.advance(); // optional COLUMN
                }
                let col_name = self.expect_ident()?;
                // TYPE is not a registered keyword — consumed as ident
                let type_kw = self.expect_ident()?;
                if type_kw.to_ascii_uppercase() != "TYPE" {
                    return Err(SqlError::UnexpectedToken {
                        expected: "TYPE",
                        got: type_kw,
                    });
                }
                let ty = self.parse_type()?;
                Ok(CompiledMutation::AlterTable {
                    collection,
                    op: AlterTableOp::AlterColumnType { name: col_name, ty },
                })
            }

            other => Err(SqlError::UnexpectedToken {
                expected: "ADD, DROP, RENAME, or ALTER",
                got: format!("{other:?}"),
            }),
        }
    }

    /// Parse: CREATE INDEX [name] ON collection USING method (field [, ...])
    /// Called after CREATE has already been consumed by parse_mutation.
    fn parse_create_index(&mut self) -> Result<CompiledMutation, SqlError> {
        self.expect_kw(Kw::Index, "INDEX")?;

        // Optional index name — if the next token is NOT `ON`, it's a name.
        let name = if !matches!(self.peek(), Tok::Kw(Kw::On)) {
            Some(self.expect_ident()?)
        } else {
            None
        };

        self.expect_kw(Kw::On, "ON")?;
        let collection = self.parse_table_ref()?;
        self.expect_kw(Kw::Using, "USING")?;
        let method_str = self.expect_ident()?;
        let method = match method_str.to_lowercase().as_str() {
            "btree"   => IndexMethod::Btree,
            "hash"    => IndexMethod::Hash,
            "gin"     => IndexMethod::Gin,
            "gist"    => IndexMethod::Gist,
            "bm25"    => IndexMethod::Bm25,
            "spatial" => IndexMethod::Spatial,
            "hnsw"    => IndexMethod::Hnsw,
            "search"  => IndexMethod::Search,
            other => return Err(SqlError::UnexpectedToken {
                expected: "btree, hash, gin, gist, bm25, spatial, hnsw, or search",
                got: other.to_string(),
            }),
        };

        self.expect_lparen()?;
        let mut fields = vec![self.expect_ident()?];
        while matches!(self.peek(), Tok::Comma) {
            self.advance();
            fields.push(self.expect_ident()?);
        }
        self.expect_rparen()?;

        Ok(CompiledMutation::CreateIndex { name, collection, method, fields })
    }

    fn parse_type(&mut self) -> Result<FieldType, SqlError> {
        let ident = self.expect_ident()?;
        match ident.to_uppercase().as_str() {
            "TEXT" => Ok(FieldType::Text),
            "INTEGER" => Ok(FieldType::Integer),
            "REAL" => Ok(FieldType::Real),
            "BOOL" | "BOOLEAN" => Ok(FieldType::Bool),
            "TIMESTAMPTZ" => Ok(FieldType::Timestamptz),
            "GEO" => Ok(FieldType::Geo),
            "VECTOR" => Ok(FieldType::Vector),
            "JSON" => Ok(FieldType::Json),
            _ => Err(SqlError::UnexpectedToken {
                expected: "TEXT, INTEGER, REAL, BOOLEAN, TIMESTAMPTZ, GEO, VECTOR, or JSON",
                got: ident,
            }),
        }
    }

    // ── MATCH parser ─────────────────────────────────────────────────────────

    /// Parse: MATCH (node)-[edge]->(node) [WHERE ...] RETURN vars [LIMIT n]
    /// Parse: MATCH (src) [WHERE conditions] INSERT (src)-[:kind]->(tgt)
    fn parse_match_insert(&mut self) -> Result<CompiledMutation, SqlError> {
        self.expect_kw(Kw::Match, "MATCH")?;

        // Parse source node with variable
        let src_node = self.parse_match_node()?;
        if src_node.var.is_none() {
            return Err(SqlError::UnexpectedToken {
                expected: "named node like (p:people) in MATCH source",
                got: format!("{:?}", src_node.label),
            });
        }

        // Parse optional WHERE conditions
        let mut conditions = Vec::new();
        if matches!(self.peek(), Tok::Kw(Kw::Where)) {
            self.advance();
            conditions = self.parse_match_conditions()?;
        }

        // Expect INSERT keyword
        self.expect_kw(Kw::Insert, "INSERT")?;

        // Parse edge pattern: (src)-[:kind {props}]->(tgt)
        // parse_match_node handles its own parentheses
        let _ = self.parse_match_node()?;

        // Consume dash before edge bracket
        if matches!(self.peek(), Tok::Dash) {
            self.advance();
        } else {
            return Err(SqlError::UnexpectedToken {
                expected: "-",
                got: format!("{:?}", self.peek()),
            });
        }

        // Parse edge type
        self.expect_lbracket()?;
        // Consume optional colon before edge kind (e.g. [:member_of])
        if matches!(self.peek(), Tok::Colon) {
            self.advance();
        }
        let edge_kind = self.expect_ident()?;
        // Optional edge props — every prop rides through as an ordinary attribute
        // (routed by value type); no privileged `strength` key.
        let mut props_json = None;
        if matches!(self.peek(), Tok::LBrace) {
            self.advance();
            let mut props_map = serde_json::Map::new();
            loop {
                if matches!(self.peek(), Tok::RBrace) {
                    self.advance();
                    break;
                }
                let key = self.expect_ident()?;
                self.expect_colon()?;
                let value = self.parse_value()?;
                props_map.insert(key, value);
                if matches!(self.peek(), Tok::Comma) {
                    self.advance();
                }
            }
            if !props_map.is_empty() {
                props_json = Some(serde_json::to_string(&props_map).unwrap());
            }
        }
        self.expect_rbracket()?;

        // Arrow
        if matches!(self.peek(), Tok::Arrow) {
            self.advance();
        }

        // Target node — parse_match_node handles its own parentheses
        let tgt_node = self.parse_match_node()?;

        // Build target slug from node
        let target_slug = if let Some(ref v) = tgt_node.var {
            if v.contains('/') {
                // Direct slug reference like classroom/A
                v.clone()
            } else if let Some(ref label) = tgt_node.label {
                let key = tgt_node
                    .props
                    .iter()
                    .find(|(k, _)| k == "_key")
                    .map(|(_, v)| v.as_str().unwrap_or(""))
                    .unwrap_or("");
                format!("{}/{}", label, key)
            } else {
                return Err(SqlError::UnexpectedToken {
                    expected: "named target like (classroom/C) or (:label {_key: val})",
                    got: format!("variable '{}' without label or slug", v),
                });
            }
        } else if let Some(ref label) = tgt_node.label {
            let key = tgt_node
                .props
                .iter()
                .find(|(k, _)| k == "_key")
                .map(|(_, v)| v.as_str().unwrap_or(""))
                .unwrap_or("");
            format!("{}/{}", label, key)
        } else {
            return Err(SqlError::UnexpectedToken {
                expected: "named target like (classroom/C)",
                got: "unnamed node".into(),
            });
        };

        // Build match_steps from conditions
        let mut match_steps = Vec::new();
        if let Some(label) = &src_node.label {
            match_steps.push(Step::Collection(sk_hash(label)));
        }
        for cond in conditions {
            match cond {
                MatchCond::NodeField {
                    field,
                    op,
                    value,
                } => match op {
                    CompareOp::Eq => match_steps.push(Step::WhereEq(field, value)),
                    CompareOp::Gt => {
                        if let Some(n) = value.as_f64() {
                            match_steps.push(Step::WhereGt(field, n));
                        }
                    }
                    CompareOp::Gte => {
                        if let Some(n) = value.as_f64() {
                            match_steps.push(Step::WhereGte(field, n));
                        }
                    }
                    CompareOp::Lt => {
                        if let Some(n) = value.as_f64() {
                            match_steps.push(Step::WhereLt(field, n));
                        }
                    }
                    CompareOp::Lte => {
                        if let Some(n) = value.as_f64() {
                            match_steps.push(Step::WhereLte(field, n));
                        }
                    }
                    CompareOp::Neq => match_steps.push(Step::WhereNeq(field, value)),
                },
            }
        }

        Ok(CompiledMutation::MatchInsert {
            match_steps,
            target: target_slug,
            edge_type: edge_kind,
            props: props_json,
        })
    }

    /// Parse: (var:label {key: val, ...})
    fn parse_match_node(&mut self) -> Result<MatchNode, SqlError> {
        self.expect_lparen()?;

        let mut var = None;
        let mut label = None;
        let mut props = Vec::new();

        // Check for empty node ()
        if matches!(self.peek(), Tok::RParen) {
            self.advance();
            return Ok(MatchNode { var, label, props });
        }

        // $N param as a slug — resolve to string, treat as a bare name (no var/label distinction)
        if let Tok::Param(idx) = self.peek().clone() {
            self.advance();
            let name = match self.params.get(idx - 1) {
                Some(Value::String(s)) => s.clone(),
                Some(_) => return Err(SqlError::ParamTypeMismatch { index: idx, expected: "string" }),
                None => return Err(SqlError::ParamOutOfRange { index: idx, count: self.params.len() }),
            };
            self.expect_rparen()?;
            return Ok(MatchNode { var: Some(name), label, props });
        }

        // Colon first means no var, just :label
        if matches!(self.peek(), Tok::Colon) {
            self.advance();
            label = Some(self.expect_ident()?);
        } else if matches!(self.peek(), Tok::Ident(_) | Tok::Str(_)) {
            let name = self.expect_ident()?;
            if matches!(self.peek(), Tok::Colon) {
                // var:label
                self.advance();
                var = Some(name);
                label = Some(self.expect_ident()?);
            } else {
                // just var, no label
                var = Some(name);
            }
        }

        // Optional inline props {key: val, ...}
        if matches!(self.peek(), Tok::LBrace) {
            self.advance();
            while !matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                let key = self.expect_ident()?;
                match self.peek() {
                    Tok::Colon => {
                        self.advance();
                    }
                    Tok::Eof => return Err(SqlError::UnexpectedEnd { expected: ":" }),
                    other => {
                        return Err(SqlError::UnexpectedToken {
                            expected: ":",
                            got: format!("{other:?}"),
                        })
                    }
                }
                let val = self.parse_value()?;
                props.push((key, val));
                if matches!(self.peek(), Tok::Comma) {
                    self.advance();
                }
            }
            match self.peek() {
                Tok::RBrace => {
                    self.advance();
                }
                _ => return Err(SqlError::UnexpectedEnd { expected: "}" }),
            }
        }

        self.expect_rparen()?;
        Ok(MatchNode { var, label, props })
    }

    /// Parse edge pattern + direction:
    ///   Forward:  -[var:kind *min..max]->
    ///   Backward: <-[var:kind]-
    /// Parse WHERE conditions: var.field OP value [AND var.field OP value]*
    fn parse_match_conditions(&mut self) -> Result<Vec<MatchCond>, SqlError> {
        let mut conds = vec![self.parse_match_cond()?];
        while matches!(self.peek(), Tok::Kw(Kw::And)) {
            self.advance();
            conds.push(self.parse_match_cond()?);
        }
        Ok(conds)
    }

    /// Parse: var.field OP value
    fn parse_match_cond(&mut self) -> Result<MatchCond, SqlError> {
        self.expect_ident()?; // consume the `var` name before the dot (a.field)
        match self.peek() {
            Tok::Dot => {
                self.advance();
            }
            Tok::Eof => return Err(SqlError::UnexpectedEnd { expected: "." }),
            other => {
                return Err(SqlError::UnexpectedToken {
                    expected: ".",
                    got: format!("{other:?}"),
                })
            }
        }
        let field = self.expect_ident()?;
        let op = match self.peek() {
            Tok::Eq => {
                self.advance();
                CompareOp::Eq
            }
            Tok::Neq => {
                self.advance();
                CompareOp::Neq
            }
            Tok::Gt => {
                self.advance();
                CompareOp::Gt
            }
            Tok::Lt => {
                self.advance();
                CompareOp::Lt
            }
            Tok::Gte => {
                self.advance();
                CompareOp::Gte
            }
            Tok::Lte => {
                self.advance();
                CompareOp::Lte
            }
            Tok::Eof => {
                return Err(SqlError::UnexpectedEnd {
                    expected: "comparison operator",
                })
            }
            other => {
                return Err(SqlError::UnexpectedToken {
                    expected: "comparison operator",
                    got: format!("{other:?}"),
                })
            }
        };
        let value = self.parse_value()?;
        Ok(MatchCond::NodeField {
            field,
            op,
            value,
        })
    }

    /// Parse: RETURN a, g, r
    // ── Aggregate MATCH parser ────────────────────────────────────────────────

    /// Parse a MATCH path in aggregate mode and build a [`MatchAggStmt`].
    ///
    /// The MATCH keyword must already have been consumed by the caller.
    ///
    /// Grammar:
    /// ```text
    /// (start)-[:edge]->(bind) [-[:edge]->(bind)]*
    /// RETURN expr AS alias [, ...]
    /// [GROUP BY var.field]
    /// [ORDER BY alias [ASC|DESC]]
    /// [LIMIT n]
    /// ```
    /// Parse `SELECT return_list FROM MATCH (start)-[edge]->(node)... [WHERE ...] [GROUP BY ...] [ORDER BY ...] [LIMIT n]`
    ///
    /// The SELECT list acts as the RETURN clause; no RETURN keyword is present.
    fn parse_select_from_match(&mut self) -> Result<crate::query::MatchAggStmt, SqlError> {
        use crate::query::{HopSpec, MatchAggStart, MatchAggStmt};

        self.expect_kw(Kw::Select, "SELECT")?;
        let distinct = if matches!(self.peek(), Tok::Kw(Kw::Distinct)) {
            self.advance();
            true
        } else {
            false
        };
        // `SELECT GRAPH FROM MATCH …` → graph-shaped output ({nodes, edges}) instead
        // of rows. `graph` is a CONTEXTUAL keyword: only special in the projection
        // slot immediately before FROM, so it stays usable as an ordinary identifier
        // / collection name everywhere else.
        let graph_output = matches!(self.peek(), Tok::Ident(s) if s.eq_ignore_ascii_case("graph"))
            && matches!(self.tokens.get(self.pos + 1), Some(Tok::Kw(Kw::From)));
        let returns = if graph_output {
            self.advance(); // consume GRAPH
            Vec::new()
        } else {
            self.parse_agg_return_list()?
        };

        self.expect_kw(Kw::From, "FROM")?;
        self.expect_kw(Kw::Match, "MATCH")?;

        // ── GQL path variable: `MATCH p = (a)-[…]->(b)` ───────────────────
        // Binds the whole path so `length(p)` / `nodes(p)` / `relationships(p)`
        // can read it. Detected by an identifier immediately followed by `=`.
        let path_var: Option<String> = if matches!(self.peek(), Tok::Ident(_))
            && matches!(self.tokens.get(self.pos + 1), Some(Tok::Eq))
        {
            let pv = self.expect_ident()?;
            self.advance(); // consume '='
            Some(pv)
        } else {
            None
        };

        // ── Start node ────────────────────────────────────────────────────
        let start_node = self.parse_match_node()?;
        let start_var:   Option<String> = start_node.var.clone();
        let start_label: Option<String> = start_node.label.clone();
        let mut start = match start_node.label {
            Some(ref lbl) => MatchAggStart::Collection(sk_hash(lbl)),
            None => match start_node.var {
                Some(ref v) => MatchAggStart::Slug(sk_hash(v)),
                None => MatchAggStart::All,
            },
        };

        // ── Hop chain (forward `-[:e]->` and backward `<-[:e]-`) ──────────
        // Inline props on destination nodes become equality WHERE conditions.
        let (hops, base_inline): (Vec<HopSpec>, _) = self.parse_hop_chain()?;

        // ── WHERE (same as parse_match_agg_path) ──────────────────────────
        let mut dest_where: Vec<crate::query::DestWhere> = base_inline;
        let mut func_filters: Vec<crate::query::MatchFuncFilter> = Vec::new();
        if matches!(self.peek(), Tok::Kw(Kw::Where)) {
            self.advance();
            loop {
                // Function filter (spatial/text) on a hop variable?
                if self.peek_is_match_filter_fn() {
                    func_filters.push(self.parse_match_func_filter()?);
                    if matches!(self.peek(), Tok::Kw(Kw::And)) { self.advance(); continue; }
                    else { break; }
                }
                let cond_var = self.expect_ident()?;
                self.expect_dot()?;
                let cond_field = self.expect_ident()?;

                // `field BETWEEN low AND high` → two conditions: `>= low` and `<= high`.
                if matches!(self.peek(), Tok::Kw(Kw::Between)) {
                    self.advance();
                    let low = self.parse_value()?;
                    self.expect_kw(Kw::And, "AND")?;
                    let high = self.parse_value()?;
                    dest_where.push(crate::query::DestWhere {
                        var: cond_var.clone(),
                        field: cond_field.clone(),
                        op: crate::query::CmpOp::Gte,
                        value: crate::query::WhereValue::Literal(low),
                    });
                    dest_where.push(crate::query::DestWhere {
                        var: cond_var,
                        field: cond_field,
                        op: crate::query::CmpOp::Lte,
                        value: crate::query::WhereValue::Literal(high),
                    });
                    if matches!(self.peek(), Tok::Kw(Kw::And)) {
                        self.advance();
                        continue;
                    }
                    break;
                }

                let op = self.parse_cmp_op()?;
                let cond_val = self.parse_value()?;

                let is_start_key = cond_field == "_key"
                    && op == crate::query::CmpOp::Eq
                    && start_var.as_ref().map_or(false, |sv| *sv == cond_var);

                if is_start_key {
                    if let Some(key_val) = cond_val.as_str() {
                        let full_slug = match start_label {
                            Some(ref lbl) => format!("{}/{}", lbl, key_val),
                            None => key_val.to_string(),
                        };
                        start = MatchAggStart::Slug(sk_hash(&full_slug));
                    }
                } else {
                    dest_where.push(crate::query::DestWhere {
                        var: cond_var, field: cond_field, op,
                        value: crate::query::WhereValue::Literal(cond_val),
                    });
                }

                if matches!(self.peek(), Tok::Kw(Kw::And)) {
                    self.advance();
                } else {
                    break;
                }
            }
        }

        // ── WITH stages (optional) ────────────────────────────────────────
        let mut with_stages_vec: Vec<crate::query::WithStage> = Vec::new();
        while matches!(self.peek(), Tok::Kw(Kw::With)) {
            self.advance(); // consume WITH
            let outputs = self.parse_with_output_list()?;
            self.expect_kw(Kw::Match, "MATCH")?;

            // Mirror parse_match_return_with: capture the stage's inline node
            // WHERE (e.g. `(c:clos WHERE _key = clo)`, which may reference a
            // prior WITH alias as a RowRef) alongside any trailing WHERE.
            let (mut ms, sv, sl, mut stage_inline_where) =
                self.parse_match_start_with_inline_where()?;
            let (mh, mut hop_inline) = self.parse_hop_chain()?;
            let mut wc = self.parse_match_where_clauses(&sv, &sl, &mut ms)?;
            wc.append(&mut stage_inline_where);
            wc.append(&mut hop_inline);

            with_stages_vec.push(crate::query::WithStage {
                outputs,
                match_start: ms,
                match_start_var: sv,
                match_hops: mh,
                where_clauses: wc,
            });
        }
        let with_stages = if with_stages_vec.is_empty() { None } else { Some(with_stages_vec) };

        // ── GROUP BY ──────────────────────────────────────────────────────
        // Supports multiple keys: GROUP BY a.city, b.role
        let group_by = if matches!(self.peek(), Tok::Kw(Kw::Group)) {
            self.advance();
            self.expect_kw(Kw::By, "BY")?;
            let mut keys: Vec<(String, String)> = Vec::new();
            loop {
                let var = self.expect_ident()?;
                self.expect_dot()?;
                let field = self.expect_ident()?;
                keys.push((var, field));
                if matches!(self.peek(), Tok::Comma) { self.advance(); } else { break; }
            }
            Some(keys)
        } else {
            None
        };

        // PG enforcement: Field returns (var.field) must appear in GROUP BY.
        if let Some(ref gkeys) = group_by {
            for (ret_expr, _alias) in &returns {
                if let crate::query::MatchAggReturn::Field { var, field } = ret_expr {
                    if field != "*" && !gkeys.iter().any(|(gv, gf)| gv == var && gf == field) {
                        return Err(SqlError::GroupByViolation(format!("{var}.{field}")));
                    }
                }
            }
        }

        // ── ORDER BY ──────────────────────────────────────────────────────
        let (order_by, order_score) = self.parse_match_order()?;

        // ── LIMIT ─────────────────────────────────────────────────────────
        let limit = if matches!(self.peek(), Tok::Kw(Kw::Limit)) {
            self.advance();
            Some(self.expect_usize()?)
        } else {
            None
        };

        // GQL: a path variable is read ONLY through length(p)/nodes(p)/relationships(p).
        // Any dotted field access on it (p.length, p.nodes, …) is a hard error —
        // there is exactly one spelling for path access.
        if let Some(ref pv) = path_var {
            let bad = returns.iter().any(|(r, _)|
                    r.references_var(pv) && !matches!(r, crate::query::MatchAggReturn::PathFn { .. }))
                || dest_where.iter().any(|dw| &dw.var == pv)
                || group_by.as_ref().map_or(false, |g| g.iter().any(|(v, _)| v == pv));
            if bad {
                return Err(SqlError::InvalidValue(format!(
                    "path variable '{pv}' is read only via length({pv}) / nodes({pv}) / relationships({pv}), not dotted field access")));
            }
        }

        // A bare identifier in the projection is a **variable reference** — that is
        // what makes `SELECT a FROM MATCH (a)-[:t]->(b)` mean "the whole of a".
        // So a bare identifier that names no bound variable is not a column, it is
        // a reference to something that does not exist, and it was resolving to
        // `null` in silence: `SELECT _key FROM MATCH (a:p)-[:next]->(b:p)` returned
        // `{"_key": null}` for every row, as did `SELECT n`, on a query that looks
        // exactly like the plain-SELECT form that works.
        //
        // Checked here because it is the first point at which both halves are
        // known: the projection is parsed before the pattern, so the bound names do
        // not exist yet when the reference is read.
        //
        // WITH stages introduce their own aliases, and a stage can rename anything,
        // so statements carrying one are left alone rather than guessed at.
        if with_stages.as_ref().map_or(true, |w| w.is_empty()) {
            let mut bound: Vec<&str> = Vec::new();
            if let Some(v) = &start_var { bound.push(v.as_str()) }
            for h in &hops {
                bound.push(h.node_bind.as_str());
                if let Some(e) = &h.edge_bind { bound.push(e.as_str()) }
            }
            for (ret, _alias) in &returns {
                if let crate::query::MatchAggReturn::Field { var, field } = ret {
                    if field == "*" && !bound.iter().any(|b| b == var) {
                        return Err(SqlError::InvalidValue(format!(
                            "`{var}` is not a bound variable in this MATCH. A bare \
                             name in the projection refers to a pattern variable; \
                             for a column of one end write `<var>.{var}`. Bound \
                             here: {}",
                            if bound.is_empty() { "none".to_string() } else { bound.join(", ") })));
                    }
                }
            }
        }
        Ok(MatchAggStmt { start, start_var, hops, returns, group_by, order_by, order_score, limit, dest_where, func_filters, with_stages, distinct, graph_output, path_var })
    }

    /// Parse `SELECT return_list FROM MATCH SHORTEST (a[:col])-[r*]->(b[:col])
    ///   WHERE a._key = 'x' AND b._key = 'y'
    ///   [AND ANY(n IN nodes(r) WHERE n.field op val)]
    ///   [ORDER BY alias [ASC|DESC]] [LIMIT n]`
    fn parse_select_from_match_shortest(
        &mut self,
    ) -> Result<crate::query::ShortestSelectStmt, SqlError> {
        use crate::query::{PathPredicate, ShortestSelectStmt, SimpleCond};

        self.expect_kw(Kw::Select, "SELECT")?;
        let returns = self.parse_agg_return_list()?;

        self.expect_kw(Kw::From, "FROM")?;
        self.expect_kw(Kw::Match, "MATCH")?;
        // SHORTEST ident (already confirmed by is_select_from_match_shortest)
        self.expect_ident()?; // consume "SHORTEST"

        // (start_bind[:col])
        self.expect_lparen()?;
        let start_bind = self.expect_ident()?;
        let start_col: Option<String> = if matches!(self.peek(), Tok::Colon) {
            self.advance(); Some(self.expect_ident()?)
        } else { None };
        self.expect_rparen()?;

        // -[path_bind*]->
        if !matches!(self.peek(), Tok::Dash) {
            return Err(SqlError::UnexpectedToken { expected: "-", got: format!("{:?}", self.peek()) });
        }
        self.advance();
        self.expect_lbracket()?;
        // Parse optional path_bind name
        let path_bind = match self.peek().clone() {
            Tok::Ident(name) => {
                self.advance();
                Some(name)
            }
            _ => None,
        };
        // Only `*` — an unconstrained walk — is supported here, and anything else
        // inside the brackets is refused rather than swallowed.
        //
        // This used to consume every token up to the `]` and drop it. So
        // `-[r:slow*]->` searched over *every* edge type, and `-[r*1..2]->`
        // searched to any depth: the constraints were parsed and thrown away with
        // nothing said. Worse than an ignored hint, it produced wrong answers —
        // `-[r:nonexistent*]->`, naming a type with no edges anywhere, still
        // reported a one-hop path, because the search never looked at the type.
        //
        // Refusing follows the rule the rest of the parser now keeps: a clause it
        // cannot apply is an error, not a shrug. Implementing typed or bounded
        // shortest paths is a feature, and this is the honest state until then.
        match self.peek().clone() {
            Tok::Star => { self.advance(); }
            Tok::Colon => {
                let ty = { self.advance(); self.expect_ident().unwrap_or_default() };
                return Err(SqlError::InvalidValue(format!(
                    "MATCH SHORTEST does not support an edge type yet, and `:{ty}` would be ignored: the search would run over every edge type and could return a path made of others. Write `-[{}*]->` for an unconstrained shortest path.",
                    path_bind.clone().unwrap_or_else(|| "r".into()))));
            }
            other => {
                return Err(SqlError::UnexpectedToken {
                    expected: "`*` — MATCH SHORTEST supports only an unconstrained walk, and a hop range would be ignored rather than applied",
                    got: format!("{other:?}"),
                });
            }
        }
        // A hop range after the `*` is refused for the same reason.
        if !matches!(self.peek(), Tok::RBracket) {
            return Err(SqlError::UnexpectedToken {
                expected: "`]` — MATCH SHORTEST does not support a hop range yet, and one written here would be ignored rather than applied",
                got: format!("{:?}", self.peek()),
            });
        }
        self.expect_rbracket()?;
        if !matches!(self.peek(), Tok::Arrow) {
            return Err(SqlError::UnexpectedToken { expected: "->", got: format!("{:?}", self.peek()) });
        }
        self.advance(); // consume ->

        // (end_bind[:col])
        self.expect_lparen()?;
        let end_bind = self.expect_ident()?;
        let end_col: Option<String> = if matches!(self.peek(), Tok::Colon) {
            self.advance(); Some(self.expect_ident()?)
        } else { None };
        self.expect_rparen()?;

        // WHERE — parse _key conditions for start/end, and optional path predicates
        self.expect_kw(Kw::Where, "WHERE")?;

        let mut from_slug: Option<String> = None;
        let mut to_slug:   Option<String> = None;
        let mut predicates: Vec<PathPredicate> = Vec::new();

        loop {
            // Path predicate: ANY/ALL/NONE/SINGLE (...)
            match self.peek().clone() {
                Tok::Kw(Kw::Any) | Tok::Kw(Kw::All) | Tok::Kw(Kw::None_) | Tok::Kw(Kw::Single) => {
                    let quantifier = self.advance();
                    self.expect_lparen()?;
                    // n IN nodes(path_bind) WHERE n.field op val
                    let _node_var = self.expect_ident()?; // the iteration variable name
                    self.expect_kw(Kw::In, "IN")?;
                    // nodes(path_bind_name)
                    let func_name = self.expect_ident()?;
                    if func_name.to_ascii_uppercase() != "NODES" {
                        return Err(SqlError::UnexpectedToken {
                            expected: "nodes(path_var)",
                            got: func_name,
                        });
                    }
                    self.expect_lparen()?;
                    let path_var = self.expect_ident()?;
                    self.expect_rparen()?;
                    self.expect_kw(Kw::Where, "WHERE")?;
                    // n.field op val
                    let _n_var = self.expect_ident()?; // should match node_var
                    self.expect_dot()?;
                    let field = self.expect_ident()?;
                    let op = self.parse_cmp_op()?;
                    let val = self.parse_value()?;
                    self.expect_rparen()?;
                    let cond = SimpleCond { field, op, val };
                    let pred = match quantifier {
                        Tok::Kw(Kw::Any)    => PathPredicate::Any    { var: path_var, cond },
                        Tok::Kw(Kw::All)    => PathPredicate::All    { var: path_var, cond },
                        Tok::Kw(Kw::None_)  => PathPredicate::None_  { var: path_var, cond },
                        Tok::Kw(Kw::Single) => PathPredicate::Single { var: path_var, cond },
                        _ => unreachable!(),
                    };
                    predicates.push(pred);
                }
                _ => {
                    // var._key = 'value' condition
                    let binding = self.expect_ident()?;
                    self.expect_dot()?;
                    let field = self.expect_ident()?;
                    if !matches!(self.peek(), Tok::Eq) {
                        return Err(SqlError::UnexpectedToken {
                            expected: "=",
                            got: format!("{:?}", self.peek()),
                        });
                    }
                    self.advance();
                    let value = self.parse_value()?;
                    let key_val = value.as_str().unwrap_or("").to_string();
                    if binding == start_bind && field == "_key" {
                        from_slug =
                            Some(shortest_slug(start_col.as_ref(), &key_val, &start_bind)?);
                    } else if binding == end_bind && field == "_key" {
                        to_slug = Some(shortest_slug(end_col.as_ref(), &key_val, &end_bind)?);
                    }
                    // Other conditions (e.g. _collection) are parsed but not acted on
                }
            }

            if matches!(self.peek(), Tok::Kw(Kw::And)) {
                self.advance();
            } else {
                break;
            }
        }

        let from_slug = from_slug.ok_or(SqlError::UnexpectedEnd { expected: "start node _key condition" })?;
        let to_slug   = to_slug  .ok_or(SqlError::UnexpectedEnd { expected: "end node _key condition" })?;

        // ORDER BY
        let order_by = if matches!(self.peek(), Tok::Kw(Kw::Order)) {
            self.advance();
            self.expect_kw(Kw::By, "BY")?;
            let mut alias = self.expect_ident()?;
            // Support `ORDER BY var.field` (destination field), not only a bare
            // SELECT alias. Stored as "var.field"; resolved at execution time.
            if matches!(self.peek(), Tok::Dot) {
                self.advance();
                let field = self.expect_ident()?;
                alias = format!("{alias}.{field}");
            }
            let ascending = if matches!(self.peek(), Tok::Kw(Kw::Desc)) {
                self.advance(); false
            } else {
                if matches!(self.peek(), Tok::Kw(Kw::Asc)) { self.advance(); }
                true
            };
            Some((alias, ascending))
        } else { None };

        // LIMIT
        let limit = if matches!(self.peek(), Tok::Kw(Kw::Limit)) {
            self.advance();
            Some(self.expect_usize()?)
        } else { None };

        // Same GQL rule for the SHORTEST path bind: length(r)/nodes(r)/relationships(r)
        // only — never dotted field access on the path variable.
        if let Some(ref pb) = path_bind {
            let bad = returns.iter().any(|(r, _)|
                r.references_var(pb) && !matches!(r, crate::query::MatchAggReturn::PathFn { .. }));
            if bad {
                return Err(SqlError::InvalidValue(format!(
                    "path variable '{pb}' is read only via length({pb}) / nodes({pb}) / relationships({pb}), not dotted field access")));
            }
        }

        Ok(ShortestSelectStmt { from_slug, to_slug, start_bind, end_bind, path_bind, returns, predicates, order_by, limit })
    }

    /// Parse `SELECT return_list FROM source1, source2, … [ORDER BY alias] [LIMIT n]`
    ///
    /// Each source is:
    /// - `MATCH SHORTEST (a)-[r*]->(b) WHERE a._key = 'x' AND b._key = 'y'`
    /// - `MATCH (a:col)-[r:edge]->(b:col) [WHERE …]`
    /// - `collection_name [AS alias]`
    fn parse_select_multi_from(
        &mut self,
    ) -> Result<crate::query::MultiFromStmt, SqlError> {
        use crate::query::{FromSource, MatchAggStart, MatchAggStmt, MultiFromStmt};

        self.expect_kw(Kw::Select, "SELECT")?;
        let returns = self.parse_agg_return_list()?;

        self.expect_kw(Kw::From, "FROM")?;

        // Parse each source
        let mut sources: Vec<FromSource> = Vec::new();
        loop {
            let src = match self.peek().clone() {
                Tok::Kw(Kw::Match) => {
                    self.advance(); // consume MATCH
                    // Check for SHORTEST
                    if matches!(self.peek(), Tok::Ident(ref name) if name.eq_ignore_ascii_case("SHORTEST")) {
                        // Delegate to a sub-parser for the SHORTEST pattern
                        // Re-use parse_select_from_match_shortest logic inline:
                        self.advance(); // consume SHORTEST
                        self.expect_lparen()?;
                        let start_bind = self.expect_ident()?;
                        let start_col: Option<String> = if matches!(self.peek(), Tok::Colon) {
                            self.advance(); Some(self.expect_ident()?)
                        } else { None };
                        self.expect_rparen()?;
                        self.advance(); // consume '-'
                        self.expect_lbracket()?;
                        let path_bind = match self.peek().clone() {
                            Tok::Ident(name) => { self.advance(); Some(name) }
                            _ => None,
                        };
                        loop {
                            match self.peek() {
                                Tok::RBracket | Tok::Eof => break,
                                _ => { self.advance(); }
                            }
                        }
                        self.expect_rbracket()?;
                        self.advance(); // consume ->
                        self.expect_lparen()?;
                        let end_bind = self.expect_ident()?;
                        let end_col: Option<String> = if matches!(self.peek(), Tok::Colon) {
                            self.advance(); Some(self.expect_ident()?)
                        } else { None };
                        self.expect_rparen()?;

                        self.expect_kw(Kw::Where, "WHERE")?;
                        let mut from_slug: Option<String> = None;
                        let mut to_slug: Option<String> = None;
                        loop {
                            let binding = self.expect_ident()?;
                            self.expect_dot()?;
                            let field = self.expect_ident()?;
                            if !matches!(self.peek(), Tok::Eq) {
                                return Err(SqlError::UnexpectedToken { expected: "=", got: format!("{:?}", self.peek()) });
                            }
                            self.advance();
                            let value = self.parse_value()?;
                            let key_val = value.as_str().unwrap_or("").to_string();
                            if binding == start_bind && field == "_key" {
                                from_slug = Some(
                                    shortest_slug(start_col.as_ref(), &key_val, &start_bind)?,
                                );
                            } else if binding == end_bind && field == "_key" {
                                to_slug = Some(
                                    shortest_slug(end_col.as_ref(), &key_val, &end_bind)?,
                                );
                            }
                            if matches!(self.peek(), Tok::Kw(Kw::And)) {
                                // Check if next is another _key condition (not a predicate)
                                if matches!(self.peek(), Tok::Kw(Kw::And)) {
                                    self.advance();
                                    if matches!(self.peek(), Tok::Kw(Kw::Any | Kw::All | Kw::None_ | Kw::Single)) {
                                        // predicates — skip for multi-from simplicity
                                        break;
                                    }
                                }
                            } else { break; }
                        }
                        let from_slug = from_slug.ok_or(SqlError::UnexpectedEnd { expected: "start _key" })?;
                        let to_slug   = to_slug  .ok_or(SqlError::UnexpectedEnd { expected: "end _key" })?;
                        FromSource::Shortest(crate::query::ShortestSelectStmt {
                            from_slug, to_slug, start_bind, end_bind, path_bind,
                            returns: vec![], predicates: vec![], order_by: None, limit: None,
                        })
                    } else {
                        // Regular MATCH hop chain
                        let start_node = self.parse_match_node()?;
                        let start_var = start_node.var.clone();
                        let start_label = start_node.label.clone();
                        let mut start = match start_node.label {
                            Some(ref lbl) => MatchAggStart::Collection(sk_hash(lbl)),
                            None => match start_node.var {
                                Some(ref v) => MatchAggStart::Slug(sk_hash(v)),
                                None => MatchAggStart::All,
                            },
                        };
                        let (hops, hop_inline) = self.parse_hop_chain()?;
                        // Optional WHERE (only _key on start var acted on)
                        if matches!(self.peek(), Tok::Kw(Kw::Where)) {
                            self.advance();
                            loop {
                                let cond_var = self.expect_ident()?;
                                self.expect_dot()?;
                                let cond_field = self.expect_ident()?;
                                if !matches!(self.peek(), Tok::Eq) {
                                    return Err(SqlError::UnexpectedToken { expected: "=", got: format!("{:?}", self.peek()) });
                                }
                                self.advance();
                                let cond_val = self.parse_value()?;
                                if cond_field == "_key" {
                                    if let Some(ref sv) = start_var {
                                        if *sv == cond_var {
                                            if let Some(key_val) = cond_val.as_str() {
                                                let full_slug = match start_label {
                                                    Some(ref lbl) => format!("{}/{}", lbl, key_val),
                                                    None => key_val.to_string(),
                                                };
                                                start = MatchAggStart::Slug(sk_hash(&full_slug));
                                            }
                                        }
                                    }
                                }
                                if matches!(self.peek(), Tok::Kw(Kw::And)) {
                                    self.advance();
                                } else { break; }
                            }
                        }
                        FromSource::Match(MatchAggStmt {
                            start, start_var, hops, returns: vec![], group_by: None, order_by: None, order_score: None, limit: None, dest_where: hop_inline, func_filters: vec![], with_stages: None, distinct: false, graph_output: false, path_var: None,
                        })
                    }
                }
                Tok::Ident(name) => {
                    let col_name = name.clone();
                    self.advance();
                    // Optional AS alias
                    let alias = if matches!(self.peek(), Tok::Kw(Kw::As)) {
                        self.advance();
                        self.expect_ident()?
                    } else {
                        col_name.clone()
                    };
                    FromSource::Collection { alias, name_hash: sk_hash(&col_name) }
                }
                other => return Err(SqlError::UnexpectedToken {
                    expected: "MATCH or collection name",
                    got: format!("{other:?}"),
                }),
            };
            sources.push(src);

            // Continue if there's a comma
            if matches!(self.peek(), Tok::Comma) {
                self.advance();
            } else {
                break;
            }
        }

        // Top-level WHERE — `var.field OP value [AND …]` across the joined sources.
        let mut where_clauses: Vec<crate::query::DestWhere> = Vec::new();
        if matches!(self.peek(), Tok::Kw(Kw::Where)) {
            self.advance();
            loop {
                let var = self.expect_ident()?;
                if matches!(self.peek(), Tok::Dot) { self.advance(); }
                let field = self.expect_ident()?;
                let op = self.parse_cmp_op()?;
                let value = self.parse_value()?;
                where_clauses.push(crate::query::DestWhere {
                    var, field, op,
                    value: crate::query::WhereValue::Literal(value),
                });
                if matches!(self.peek(), Tok::Kw(Kw::And)) { self.advance(); } else { break; }
            }
        }

        // ORDER BY
        let order_by = if matches!(self.peek(), Tok::Kw(Kw::Order)) {
            self.advance();
            self.expect_kw(Kw::By, "BY")?;
            let mut alias = self.expect_ident()?;
            // Support `ORDER BY var.field` (destination field), not only a bare
            // SELECT alias. Stored as "var.field"; resolved at execution time.
            if matches!(self.peek(), Tok::Dot) {
                self.advance();
                let field = self.expect_ident()?;
                alias = format!("{alias}.{field}");
            }
            let ascending = if matches!(self.peek(), Tok::Kw(Kw::Desc)) {
                self.advance(); false
            } else {
                if matches!(self.peek(), Tok::Kw(Kw::Asc)) { self.advance(); }
                true
            };
            Some((alias, ascending))
        } else { None };

        // LIMIT
        let limit = if matches!(self.peek(), Tok::Kw(Kw::Limit)) {
            self.advance();
            Some(self.expect_usize()?)
        } else { None };

        Ok(MultiFromStmt { sources, returns, where_clauses, order_by, limit })
    }

    /// Parse the aggregate RETURN list: `expr AS alias [, expr AS alias ...]`
    fn parse_agg_return_list(
        &mut self,
    ) -> Result<Vec<(crate::query::MatchAggReturn, String)>, SqlError> {
        let mut items = vec![self.parse_agg_return_item()?];
        while matches!(self.peek(), Tok::Comma) {
            self.advance();
            items.push(self.parse_agg_return_item()?);
        }
        Ok(items)
    }

    /// Parse a single aggregate return item: `var.field AS alias` or `SUM(math) AS alias`.
    /// Parse one primary of a MATCH string expression: `'lit'`, `var.field`, or a
    /// nested `concat/concat_ws/lower/coalesce(…)` call.
    fn parse_match_str_primary(&mut self) -> Result<crate::query::MatchStr, SqlError> {
        use crate::query::MatchStr;
        match self.peek().clone() {
            Tok::Str(s) => { self.advance(); Ok(MatchStr::Lit(s)) }
            Tok::Ident(name) => {
                let upper = name.to_ascii_uppercase();
                self.advance();
                if matches!(self.peek(), Tok::LParen)
                    && matches!(upper.as_str(), "CONCAT" | "CONCAT_WS" | "LOWER" | "COALESCE")
                {
                    self.parse_match_str_fn_body(&upper)
                } else if matches!(self.peek(), Tok::Dot) {
                    self.advance();
                    let field = self.expect_ident()?;
                    Ok(MatchStr::Field(name, field))
                } else {
                    Err(SqlError::UnexpectedToken {
                        expected: "var.field or a string function",
                        got: name,
                    })
                }
            }
            other => Err(SqlError::UnexpectedToken {
                expected: "string expression (var.field, 'literal', concat_ws, lower, coalesce)",
                got: format!("{other:?}"),
            }),
        }
    }

    /// Parse a full MATCH string expression: primary (`||` primary)*.
    fn parse_match_str(&mut self) -> Result<crate::query::MatchStr, SqlError> {
        use crate::query::MatchStr;
        let mut parts = vec![self.parse_match_str_primary()?];
        while matches!(self.peek(), Tok::Concat) {
            self.advance();
            parts.push(self.parse_match_str_primary()?);
        }
        Ok(if parts.len() == 1 { parts.pop().unwrap() } else { MatchStr::Concat(parts) })
    }

    /// Parse the argument list of a string function (name already consumed): the
    /// opening `(` is next.
    fn parse_match_str_fn_body(&mut self, upper: &str) -> Result<crate::query::MatchStr, SqlError> {
        use crate::query::MatchStr;
        self.expect_lparen()?;
        let e = match upper {
            "LOWER" => MatchStr::Lower(Box::new(self.parse_match_str()?)),
            "CONCAT_WS" => {
                let sep = self.expect_str()?;
                let mut parts = Vec::new();
                while matches!(self.peek(), Tok::Comma) {
                    self.advance();
                    parts.push(self.parse_match_str()?);
                }
                MatchStr::ConcatWs(sep, parts)
            }
            "CONCAT" | "COALESCE" => {
                let mut parts = vec![self.parse_match_str()?];
                while matches!(self.peek(), Tok::Comma) {
                    self.advance();
                    parts.push(self.parse_match_str()?);
                }
                if upper == "CONCAT" { MatchStr::Concat(parts) } else { MatchStr::Coalesce(parts) }
            }
            _ => unreachable!(),
        };
        self.expect_rparen()?;
        Ok(e)
    }

    fn parse_agg_return_item(
        &mut self,
    ) -> Result<(crate::query::MatchAggReturn, String), SqlError> {
        use crate::query::MatchAggReturn;

        let expr: MatchAggReturn = match self.peek().clone() {
            Tok::Kw(Kw::Count) => {
                self.advance();
                self.expect_lparen()?;
                // `COUNT(DISTINCT var.field)` counts unique values; otherwise
                // `COUNT(*)` / `COUNT(var.field)` is a plain row count.
                let is_distinct = matches!(self.peek(), Tok::Kw(Kw::Distinct));
                if is_distinct { self.advance(); }
                let ret = match self.peek() {
                    Tok::Star => { self.advance(); MatchAggReturn::Count }
                    _ => {
                        let var = self.expect_ident()?;
                        let field = if matches!(self.peek(), Tok::Dot) {
                            self.advance();
                            self.expect_ident()?
                        } else {
                            "*".to_string()
                        };
                        if is_distinct {
                            MatchAggReturn::CountDistinct { var, field }
                        } else {
                            MatchAggReturn::Count
                        }
                    }
                };
                self.expect_rparen()?;
                ret
            }
            Tok::Kw(ref kw) if matches!(kw, Kw::Sum | Kw::Avg | Kw::Min | Kw::Max) => {
                let func = kw.clone();
                self.advance();
                self.expect_lparen()?;
                let math = self.parse_math_expr()?;
                self.expect_rparen()?;
                match func {
                    Kw::Sum => MatchAggReturn::Sum(math),
                    Kw::Avg => MatchAggReturn::Avg(math),
                    Kw::Min => MatchAggReturn::Min(math),
                    Kw::Max => MatchAggReturn::Max(math),
                    _ => unreachable!(),
                }
            }
            Tok::Ident(ref name_peek) => {
                let upper = name_peek.to_ascii_uppercase();
                if upper.starts_with("PATH_")
                    || matches!(upper.as_str(), "AGE_DAYS" | "AGE_HOURS" | "JSON_ARRAY_LENGTH")
                {
                    let func_name = self.expect_ident()?.to_ascii_uppercase();
                    self.expect_lparen()?;
                    let var = self.expect_ident()?;
                    self.expect_dot()?;
                    let field = self.expect_ident()?;
                    self.expect_rparen()?;
                    path_func_variant(&func_name, var, field)?
                } else if upper == "NOW" {
                    self.advance(); // consume NOW
                    self.expect_lparen()?;
                    self.expect_rparen()?;
                    MatchAggReturn::Now
                } else if matches!(upper.as_str(), "LENGTH" | "NODES" | "RELATIONSHIPS") {
                    // Pure-GQL path functions over a path variable: `length(p)`,
                    // `nodes(p)`, `relationships(p)`. This is the ONLY way to read a
                    // path — dotted access (`p.length`) is rejected (see the
                    // path-var validation in parse_select_from_match).
                    use crate::query::PathFnKind;
                    let kind = match upper.as_str() {
                        "LENGTH" => PathFnKind::Length,
                        "NODES" => PathFnKind::Nodes,
                        _ => PathFnKind::Relationships,
                    };
                    self.advance(); // consume function name
                    self.expect_lparen()?;
                    let pvar = self.expect_ident()?;
                    self.expect_rparen()?;
                    MatchAggReturn::PathFn { var: pvar, kind }
                } else if matches!(upper.as_str(), "CONCAT" | "CONCAT_WS" | "LOWER" | "COALESCE") {
                    // String-building expression for materialized-view text columns.
                    self.advance(); // consume function name
                    MatchAggReturn::Str(self.parse_match_str_fn_body(&upper)?)
                } else {
                    let var = self.expect_ident()?;
                    if matches!(self.peek(), Tok::Dot) {
                        self.advance();
                        if matches!(self.peek(), Tok::Star) {
                            self.advance();
                            MatchAggReturn::AllFields { var }
                        } else {
                            let field = self.expect_ident()?;
                            // `var.field || …` → a string concatenation expression.
                            if matches!(self.peek(), Tok::Concat) {
                                use crate::query::MatchStr;
                                let mut parts = vec![MatchStr::Field(var, field)];
                                while matches!(self.peek(), Tok::Concat) {
                                    self.advance();
                                    parts.push(self.parse_match_str_primary()?);
                                }
                                MatchAggReturn::Str(MatchStr::Concat(parts))
                            } else {
                                MatchAggReturn::Field { var, field }
                            }
                        }
                    } else {
                        // Bare identifier — reference to a bound variable or WITH alias.
                        // Represented as Field { var, field: "*" } to signal "whole value".
                        MatchAggReturn::Field { var, field: "*".to_string() }
                    }
                }
            }
            Tok::Kw(Kw::Case) => {
                use crate::query::{CaseCond, CmpOp};
                self.advance(); // consume CASE
                let mut branches: Vec<(CaseCond, Value)> = Vec::new();
                let mut else_val = Value::Null;
                loop {
                    match self.peek().clone() {
                        Tok::Kw(Kw::When) => {
                            self.advance(); // consume WHEN
                            let var = self.expect_ident()?;
                            self.expect_dot()?;
                            let field = self.expect_ident()?;
                            let op: CmpOp = self.parse_cmp_op()?;
                            let val = self.parse_value()?;
                            self.expect_kw(Kw::Then, "THEN")?;
                            let then_val = self.parse_value()?;
                            branches.push((CaseCond { var, field, op, val }, then_val));
                        }
                        Tok::Kw(Kw::Else) => {
                            self.advance(); // consume ELSE
                            else_val = self.parse_value()?;
                        }
                        _ => break,
                    }
                }
                self.expect_kw(Kw::End, "END")?;
                MatchAggReturn::Case { branches, else_val }
            }
            other => {
                return Err(SqlError::UnexpectedToken {
                    expected: "field expression or aggregate function (SUM, AVG, MIN, MAX, COUNT, PATH_*, CASE, NOW, AGE_DAYS, AGE_HOURS, JSON_ARRAY_LENGTH)",
                    got: format!("{other:?}"),
                })
            }
        };

        // Optional AS alias
        let default_alias = match &expr {
            MatchAggReturn::Field { var, field } => {
                if field == "*" { var.clone() } else { format!("{}.{}", var, field) }
            }
            MatchAggReturn::AllFields { var } => var.clone(),
            MatchAggReturn::CountDistinct { .. } => "count".to_string(),
            MatchAggReturn::Sum(_) => "sum".to_string(),
            MatchAggReturn::Count => "count".to_string(),
            MatchAggReturn::Avg(_) => "avg".to_string(),
            MatchAggReturn::Min(_) => "min".to_string(),
            MatchAggReturn::Max(_) => "max".to_string(),
            MatchAggReturn::PathAvg { .. } => "path_avg".to_string(),
            MatchAggReturn::PathSum { .. } => "path_sum".to_string(),
            MatchAggReturn::PathMin { .. } => "path_min".to_string(),
            MatchAggReturn::PathMax { .. } => "path_max".to_string(),
            MatchAggReturn::PathProduct { .. } => "path_product".to_string(),
            MatchAggReturn::PathFirst { .. } => "path_first".to_string(),
            MatchAggReturn::PathLast { .. } => "path_last".to_string(),
            MatchAggReturn::Case { .. } => "case".to_string(),
            MatchAggReturn::AgeDays { .. } => "age_days".to_string(),
            MatchAggReturn::AgeHours { .. } => "age_hours".to_string(),
            MatchAggReturn::Now => "now".to_string(),
            MatchAggReturn::JsonArrayLen { .. } => "json_array_length".to_string(),
            MatchAggReturn::PathFn { kind, .. } => match kind {
                crate::query::PathFnKind::Length => "length".to_string(),
                crate::query::PathFnKind::Nodes => "nodes".to_string(),
                crate::query::PathFnKind::Relationships => "relationships".to_string(),
            },
            MatchAggReturn::Str(_) => "text".to_string(),
        };
        let alias = if matches!(self.peek(), Tok::Kw(Kw::As)) {
            self.advance();
            self.expect_ident()?
        } else {
            default_alias
        };

        Ok((expr, alias))
    }

    /// Parse a math expression: `primary (* | + | - | /) primary ...` (left-assoc).
    fn parse_math_expr(&mut self) -> Result<crate::query::MathExpr, SqlError> {
        let mut left = self.parse_math_primary()?;
        loop {
            match self.peek() {
                Tok::Star => {
                    self.advance();
                    let right = self.parse_math_primary()?;
                    left = crate::query::MathExpr::Mul(Box::new(left), Box::new(right));
                }
                Tok::Plus => {
                    self.advance();
                    let right = self.parse_math_primary()?;
                    left = crate::query::MathExpr::Add(Box::new(left), Box::new(right));
                }
                Tok::Dash => {
                    self.advance();
                    let right = self.parse_math_primary()?;
                    left = crate::query::MathExpr::Sub(Box::new(left), Box::new(right));
                }
                Tok::Slash => {
                    self.advance();
                    let right = self.parse_math_primary()?;
                    left = crate::query::MathExpr::Div(Box::new(left), Box::new(right));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    /// Parse a math primary: `var.field`, numeric literal, `$N` param, or `(expr)`.
    fn parse_math_primary(&mut self) -> Result<crate::query::MathExpr, SqlError> {
        match self.peek().clone() {
            Tok::Num(n) => {
                self.advance();
                Ok(crate::query::MathExpr::Literal(n))
            }
            Tok::Param(idx) => {
                self.advance();
                match self.params.get(idx - 1) {
                    Some(Value::Number(n)) => Ok(crate::query::MathExpr::Literal(
                        n.as_f64().unwrap_or(0.0),
                    )),
                    Some(_) => Err(SqlError::ParamTypeMismatch { index: idx, expected: "number" }),
                    None => Err(SqlError::ParamOutOfRange { index: idx, count: self.params.len() }),
                }
            }
            Tok::LParen => {
                self.advance();
                let inner = self.parse_math_expr()?;
                self.expect_rparen()?;
                Ok(inner)
            }
            Tok::Ident(_) | Tok::Kw(_) => {
                let var = self.expect_ident()?;
                if matches!(self.peek(), Tok::Dot) {
                    self.advance();
                    let field = self.expect_ident()?;
                    Ok(crate::query::MathExpr::VarField { var, field })
                } else {
                    // Bare identifier — reference to a WITH alias (top-level row key).
                    // VarField with field="*" signals "take the numeric value directly".
                    Ok(crate::query::MathExpr::VarField { var, field: "*".to_string() })
                }
            }
            other => Err(SqlError::UnexpectedToken {
                expected: "number, var.field, or (expr)",
                got: format!("{other:?}"),
            }),
        }
    }

    /// Parse a comparison operator token into a [`CmpOp`].
    fn parse_cmp_op(&mut self) -> Result<crate::query::CmpOp, SqlError> {
        use crate::query::CmpOp;
        match self.peek() {
            Tok::Eq  => { self.advance(); Ok(CmpOp::Eq) }
            Tok::Neq => { self.advance(); Ok(CmpOp::Neq) }
            Tok::Lt  => { self.advance(); Ok(CmpOp::Lt) }
            Tok::Gt  => { self.advance(); Ok(CmpOp::Gt) }
            Tok::Lte => { self.advance(); Ok(CmpOp::Lte) }
            Tok::Gte => { self.advance(); Ok(CmpOp::Gte) }
            Tok::Kw(Kw::ILike) => { self.advance(); Ok(CmpOp::ILike) }
            other => Err(SqlError::UnexpectedToken {
                expected: "comparison operator (=, !=, <>, <, >, <=, >=, ILIKE)",
                got: format!("{other:?}"),
            }),
        }
    }

    // ── WITH expression parser (for SELECT FROM MATCH ... WITH ... MATCH ...) ──

    /// Parse WITH output list: `expr AS alias [, expr AS alias]*`
    fn parse_with_output_list(&mut self) -> Result<Vec<(crate::query::WithOutExpr, String)>, SqlError> {
        let mut items = vec![self.parse_with_output_item()?];
        while matches!(self.peek(), Tok::Comma) {
            self.advance();
            items.push(self.parse_with_output_item()?);
        }
        Ok(items)
    }

    fn parse_with_output_item(&mut self) -> Result<(crate::query::WithOutExpr, String), SqlError> {
        use crate::query::{WithExpr, WithOutExpr};
        let expr = self.parse_with_out_expr()?;
        let alias = if matches!(self.peek(), Tok::Kw(Kw::As)) {
            self.advance();
            self.expect_ident()?
        } else {
            match &expr {
                WithOutExpr::Scalar(WithExpr::RowKey(key)) => key.clone(),
                WithOutExpr::Scalar(WithExpr::VarField { field, .. }) => field.clone(),
                _ => {
                    return Err(SqlError::UnexpectedToken {
                        expected: "AS alias (required for aggregate/complex expressions)",
                        got: format!("{:?}", self.peek()),
                    });
                }
            }
        };
        Ok((expr, alias))
    }

    fn parse_with_out_expr(&mut self) -> Result<crate::query::WithOutExpr, SqlError> {
        use crate::query::WithOutExpr;
        match self.peek().clone() {
            Tok::Kw(Kw::Sum) => {
                self.advance();
                Ok(WithOutExpr::Sum(self.parse_with_paren_math_expr()?))
            }
            Tok::Kw(Kw::Avg) => {
                self.advance();
                Ok(WithOutExpr::Avg(self.parse_with_paren_math_expr()?))
            }
            Tok::Kw(Kw::Min) => {
                self.advance();
                Ok(WithOutExpr::Min(self.parse_with_paren_math_expr()?))
            }
            Tok::Kw(Kw::Max) => {
                self.advance();
                Ok(WithOutExpr::Max(self.parse_with_paren_math_expr()?))
            }
            Tok::Kw(Kw::Count) => {
                self.advance();
                self.expect_lparen()?;
                if matches!(self.peek(), Tok::Star) { self.advance(); }
                self.expect_rparen()?;
                Ok(WithOutExpr::Count)
            }
            _ => Ok(WithOutExpr::Scalar(self.parse_with_math_expr()?)),
        }
    }

    fn parse_with_paren_math_expr(&mut self) -> Result<crate::query::WithExpr, SqlError> {
        self.expect_lparen()?;
        let expr = self.parse_with_math_expr()?;
        self.expect_rparen()?;
        Ok(expr)
    }

    fn parse_with_math_expr(&mut self) -> Result<crate::query::WithExpr, SqlError> {
        use crate::query::WithExpr;
        let mut left = self.parse_with_math_term()?;
        loop {
            match self.peek() {
                Tok::Plus => {
                    self.advance();
                    let right = self.parse_with_math_term()?;
                    left = WithExpr::Add(Box::new(left), Box::new(right));
                }
                Tok::Dash => {
                    self.advance();
                    let right = self.parse_with_math_term()?;
                    left = WithExpr::Sub(Box::new(left), Box::new(right));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_with_math_term(&mut self) -> Result<crate::query::WithExpr, SqlError> {
        use crate::query::WithExpr;
        let mut left = self.parse_with_math_primary()?;
        loop {
            match self.peek() {
                Tok::Star => {
                    self.advance();
                    let right = self.parse_with_math_primary()?;
                    left = WithExpr::Mul(Box::new(left), Box::new(right));
                }
                Tok::Slash => {
                    self.advance();
                    let right = self.parse_with_math_primary()?;
                    left = WithExpr::Div(Box::new(left), Box::new(right));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_with_math_primary(&mut self) -> Result<crate::query::WithExpr, SqlError> {
        use crate::query::WithExpr;
        match self.peek().clone() {
            Tok::Num(n) => {
                self.advance();
                Ok(WithExpr::Literal(n))
            }
            Tok::Param(idx) => {
                self.advance();
                match self.params.get(idx - 1) {
                    Some(Value::Number(n)) => Ok(WithExpr::Literal(n.as_f64().unwrap_or(0.0))),
                    Some(_) => Err(SqlError::ParamTypeMismatch { index: idx, expected: "number" }),
                    None => Err(SqlError::ParamOutOfRange { index: idx, count: self.params.len() }),
                }
            }
            Tok::LParen => {
                self.advance();
                let expr = self.parse_with_math_expr()?;
                self.expect_rparen()?;
                Ok(expr)
            }
            Tok::Ident(_) | Tok::Kw(_) => {
                let name = self.expect_ident()?;
                if matches!(self.peek(), Tok::Dot) {
                    self.advance();
                    let field = self.expect_ident()?;
                    Ok(WithExpr::VarField { var: name, field })
                } else {
                    Ok(WithExpr::RowKey(name))
                }
            }
            other => Err(SqlError::UnexpectedToken {
                expected: "number, identifier, or ( in WITH expression",
                got: format!("{other:?}"),
            }),
        }
    }

    /// Parse WHERE conditions that support both literals and RowRef (for multi-stage).
    /// Used after MATCH hops in WITH stages.
    fn parse_match_where_clauses(
        &mut self,
        start_var: &Option<String>,
        start_label: &Option<String>,
        start: &mut crate::query::MatchAggStart,
    ) -> Result<Vec<crate::query::DestWhere>, SqlError> {
        let mut dest_where: Vec<crate::query::DestWhere> = Vec::new();
        if !matches!(self.peek(), Tok::Kw(Kw::Where)) {
            return Ok(dest_where);
        }
        self.advance(); // consume WHERE
        loop {
            let cond_var = self.expect_ident()?;
            self.expect_dot()?;
            let cond_field = self.expect_ident()?;

            // `field BETWEEN low AND high` → two conditions: `>= low` and `<= high`.
            if matches!(self.peek(), Tok::Kw(Kw::Between)) {
                self.advance();
                let low = self.parse_value()?;
                self.expect_kw(Kw::And, "AND")?;
                let high = self.parse_value()?;
                dest_where.push(crate::query::DestWhere {
                    var: cond_var.clone(),
                    field: cond_field.clone(),
                    op: crate::query::CmpOp::Gte,
                    value: crate::query::WhereValue::Literal(low),
                });
                dest_where.push(crate::query::DestWhere {
                    var: cond_var,
                    field: cond_field,
                    op: crate::query::CmpOp::Lte,
                    value: crate::query::WhereValue::Literal(high),
                });
                if matches!(self.peek(), Tok::Kw(Kw::And)) {
                    self.advance();
                    continue;
                }
                break;
            }

            let op = self.parse_cmp_op()?;
            // RHS: literal value, param, or bare ident (RowRef)
            let (cond_val_opt, row_ref) = match self.peek().clone() {
                Tok::Ident(ref name) if !matches!(self.peek(), Tok::Kw(_)) => {
                    let rr = name.clone();
                    self.advance();
                    (None, Some(rr))
                }
                _ => {
                    let v = self.parse_value()?;
                    (Some(v), None)
                }
            };

            let is_start_key = cond_field == "_key"
                && op == crate::query::CmpOp::Eq
                && start_var.as_ref().map_or(false, |sv| *sv == cond_var)
                && cond_val_opt.is_some();

            if is_start_key {
                if let Some(ref cond_val) = cond_val_opt {
                    if let Some(key_val) = cond_val.as_str() {
                        let full_slug = match start_label {
                            Some(ref lbl) => format!("{}/{}", lbl, key_val),
                            None => key_val.to_string(),
                        };
                        *start = crate::query::MatchAggStart::Slug(sk_hash(&full_slug));
                    }
                }
            } else {
                let value = if let Some(rr) = row_ref {
                    crate::query::WhereValue::RowRef(rr)
                } else {
                    crate::query::WhereValue::Literal(cond_val_opt.unwrap())
                };
                dest_where.push(crate::query::DestWhere {
                    var: cond_var, field: cond_field, op, value,
                });
            }

            if matches!(self.peek(), Tok::Kw(Kw::And)) {
                self.advance();
            } else {
                break;
            }
        }
        Ok(dest_where)
    }

    /// Parse a MATCH...RETURN (with optional WITH stages) into a MatchAggStmt.
    /// Called from `parse_match_or_agg_inner` when MATCH...WITH or MATCH...RETURN with
    /// projections is detected.
    /// Parse a MATCH start node that may contain inline WHERE conditions inside the parens.
    /// Returns (MatchAggStart, start_var, start_label, inline_where_clauses).
    /// Handles patterns like: `('slug')`, `(var:col)`, `(var:col WHERE _key = ref)`
    fn parse_match_start_with_inline_where(&mut self) -> Result<(
        crate::query::MatchAggStart,
        Option<String>,
        Option<String>,
        Vec<crate::query::DestWhere>,
    ), SqlError> {
        self.expect_lparen()?;

        // Handle $N param as slug
        if let Tok::Param(idx) = self.peek().clone() {
            self.advance();
            let slug = match self.params.get(idx - 1) {
                Some(Value::String(s)) => s.clone(),
                Some(_) => return Err(SqlError::ParamTypeMismatch { index: idx, expected: "string" }),
                None => return Err(SqlError::ParamOutOfRange { index: idx, count: self.params.len() }),
            };
            self.expect_rparen()?;
            return Ok((crate::query::MatchAggStart::Slug(sk_hash(&slug)), None, None, vec![]));
        }

        // Handle string literal as slug: ('slug/path')
        if let Tok::Str(ref s) = self.peek().clone() {
            let slug = s.clone();
            self.advance();
            self.expect_rparen()?;
            return Ok((crate::query::MatchAggStart::Slug(sk_hash(&slug)), None, None, vec![]));
        }

        let name = self.expect_ident()?;
        if !matches!(self.peek(), Tok::Colon) {
            // Bare (name) — treated as slug
            self.expect_rparen()?;
            return Ok((crate::query::MatchAggStart::Slug(sk_hash(&name)), Some(name.clone()), None, vec![]));
        }
        self.advance(); // consume :
        let label = self.expect_ident()?;

        // Optional inline WHERE
        let mut inline_where: Vec<crate::query::DestWhere> = Vec::new();
        if matches!(self.peek(), Tok::Kw(Kw::Where)) {
            self.advance(); // consume WHERE
            loop {
                let field = self.expect_ident()?;
                let op = self.parse_cmp_op()?;
                // RHS: literal, param, or bare ident (RowRef)
                let value = match self.peek().clone() {
                    Tok::Str(s) => {
                        self.advance();
                        crate::query::WhereValue::Literal(Value::String(s))
                    }
                    Tok::Num(n) => {
                        self.advance();
                        let num = serde_json::Number::from_f64(n)
                            .map(Value::Number)
                            .unwrap_or(Value::Null);
                        crate::query::WhereValue::Literal(num)
                    }
                    Tok::Kw(Kw::True) => { self.advance(); crate::query::WhereValue::Literal(Value::Bool(true)) }
                    Tok::Kw(Kw::False) => { self.advance(); crate::query::WhereValue::Literal(Value::Bool(false)) }
                    Tok::Kw(Kw::Null) => { self.advance(); crate::query::WhereValue::Literal(Value::Null) }
                    Tok::Param(idx) => {
                        self.advance();
                        let val = self.params.get(idx - 1).cloned().ok_or(SqlError::ParamOutOfRange {
                            index: idx,
                            count: self.params.len(),
                        })?;
                        crate::query::WhereValue::Literal(val)
                    }
                    Tok::Ident(ref row_key) => {
                        let rr = row_key.clone();
                        self.advance();
                        crate::query::WhereValue::RowRef(rr)
                    }
                    ref other => return Err(SqlError::UnexpectedToken {
                        expected: "literal or row reference in WHERE",
                        got: format!("{other:?}"),
                    }),
                };
                inline_where.push(crate::query::DestWhere {
                    var: name.clone(), field, op, value,
                });
                if matches!(self.peek(), Tok::Kw(Kw::And)) {
                    self.advance();
                } else {
                    break;
                }
            }
        }
        self.expect_rparen()?;
        // _key optimization: if there's an `_key = literal` condition, upgrade to Slug.
        let mut start = crate::query::MatchAggStart::Collection(sk_hash(&label));
        inline_where.retain(|dw| {
            if dw.field == "_key" && dw.op == crate::query::CmpOp::Eq {
                if let crate::query::WhereValue::Literal(ref val) = dw.value {
                    if let Some(key_val) = val.as_str() {
                        let full_slug = format!("{}/{}", label, key_val);
                        start = crate::query::MatchAggStart::Slug(sk_hash(&full_slug));
                        return false; // consumed — remove from inline_where
                    }
                }
            }
            true
        });
        Ok((start, Some(name), Some(label), inline_where))
    }

    /// Parse hop chain: `-[edge_spec]->(node)` (forward) or `<-[edge_spec]-(node)`
    /// (backward), repeated.  Destination nodes may be anonymous (`(:label)`),
    /// carry inline props (`(:label {f: 'v'})`), or both — inline props are
    /// returned as equality WHERE conditions on that node.
    fn parse_hop_chain(&mut self)
        -> Result<(Vec<crate::query::HopSpec>, Vec<crate::query::DestWhere>), SqlError>
    {
        use crate::query::{HopSpec, DestWhere, WhereValue, CmpOp};
        let mut hops: Vec<HopSpec> = Vec::new();
        let mut inline_conds: Vec<DestWhere> = Vec::new();
        while matches!(self.peek(), Tok::Dash | Tok::BackArrow) {
            // `<-[...]-` is a backward hop; `-[...]->` is forward.
            let backward = matches!(self.peek(), Tok::BackArrow);
            self.advance(); // consume '-' or '<-'

            // ── Anonymous short edge (#4): `-->` / `<--` — any edge type, no
            // brackets. The leading `-`/`<-` is consumed; a token other than `[`
            // means the short form. Forward closes with `->` (`-` + `->` = `-->`),
            // backward closes with `-` (`<-` + `-` = `<--`). `--` (undirected) is
            // intentionally NOT accepted here (needs the executor change, #2).
            if !matches!(self.peek(), Tok::LBracket) {
                let ok = if backward {
                    matches!(self.peek(), Tok::Dash)
                } else {
                    matches!(self.peek(), Tok::Arrow)
                };
                if !ok {
                    return Err(SqlError::UnexpectedToken {
                        expected: if backward { "[ or `-` to close `<--`" } else { "[ or `->` to close `-->`" },
                        got: format!("{:?}", self.peek()),
                    });
                }
                self.advance(); // consume closing `->` (fwd) or `-` (bwd)

                // Optional post-arrow quantifier `{n,m}` (same as bracketed form).
                let (mut min_depth, mut max_depth) = (1u32, 1u32);
                if matches!(self.peek(), Tok::LBrace) {
                    let (mn, mx) = self.parse_brace_quantifier()?;
                    min_depth = mn;
                    max_depth = mx;
                    if max_depth > MAX_HOP_DEPTH || min_depth > MAX_HOP_DEPTH {
                        return Err(SqlError::InvalidValue(format!(
                            "graph hop depth exceeds the maximum ({MAX_HOP_DEPTH})")));
                    }
                    if min_depth > max_depth {
                        return Err(SqlError::InvalidValue(format!(
                            "graph hop range invalid: min {min_depth} > max {max_depth}")));
                    }
                }

                let node = self.parse_match_node()?;
                let node_label = node.label.clone();
                let node_bind = node.var.clone()
                    .unwrap_or_else(|| format!("__anon_hop{}", hops.len()));
                for (field, val) in node.props {
                    inline_conds.push(DestWhere {
                        var: node_bind.clone(),
                        field,
                        op: CmpOp::Eq,
                        value: WhereValue::Literal(val),
                    });
                }
                hops.push(HopSpec {
                    edge_type_hash: 0,
                    node_bind,
                    edge_bind: None,
                    min_depth,
                    max_depth,
                    node_label,
                    backward,
                    undirected: false, // `-->` / `<--` are directed
                });
                continue;
            }

            self.expect_lbracket()?;

            let mut edge_bind: Option<String> = None;
            let mut edge_type_hash: u64 = 0;
            let mut edge_props: Vec<(String, Value)> = Vec::new();

            match self.peek().clone() {
                Tok::Ident(name) => {
                    self.advance();
                    match self.peek() {
                        Tok::Colon => {
                            self.advance();
                            let et = self.expect_ident()?;
                            edge_bind = Some(name);
                            edge_type_hash = sk_hash(&et);
                        }
                        _ => { edge_bind = Some(name); }
                    }
                }
                Tok::Colon => {
                    self.advance();
                    let et = self.expect_ident()?;
                    edge_type_hash = sk_hash(&et);
                }
                _ => {}
            }

            let (mut min_depth, mut max_depth) = (1u32, 1u32);
            if matches!(self.peek(), Tok::Star) {
                self.advance();
                if let Tok::Num(_) = self.peek().clone() {
                    let mn = self.expect_num()? as u32;
                    if matches!(self.peek(), Tok::DotDot) {
                        self.advance();
                        let mx = self.expect_num()? as u32;
                        min_depth = mn;
                        max_depth = mx;
                    } else {
                        min_depth = mn;
                        max_depth = mn;
                    }
                }
            }
            if max_depth > MAX_HOP_DEPTH || min_depth > MAX_HOP_DEPTH {
                return Err(SqlError::InvalidValue(format!(
                    "graph hop depth exceeds the maximum ({MAX_HOP_DEPTH})")));
            }
            if min_depth > max_depth {
                return Err(SqlError::InvalidValue(format!(
                    "graph hop range invalid: min {min_depth} > max {max_depth}")));
            }
            // GQL inline edge props: `-[e {grade: 90}]->` → edge-field equality
            // conditions (lowered below). Unambiguous inside the brackets — the `*`
            // quantifier was already consumed, so a leading `{` can only be props.
            if matches!(self.peek(), Tok::LBrace) {
                self.advance();
                while !matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                    let key = self.expect_ident()?;
                    match self.peek() {
                        Tok::Colon => { self.advance(); }
                        other => return Err(SqlError::UnexpectedToken {
                            expected: ":",
                            got: format!("{other:?}"),
                        }),
                    }
                    let val = self.parse_value()?;
                    edge_props.push((key, val));
                    if matches!(self.peek(), Tok::Comma) { self.advance(); }
                }
                match self.peek() {
                    Tok::RBrace => { self.advance(); }
                    _ => return Err(SqlError::UnexpectedEnd { expected: "}" }),
                }
            }
            // Safety net: swallow any other stray tokens before `]` (back-compat).
            loop {
                match self.peek() {
                    Tok::RBracket | Tok::Eof => break,
                    _ => { self.advance(); }
                }
            }
            self.expect_rbracket()?;

            // Direction from leading + closing tokens:
            //   -[...]->   forward     (lead `-`,  close `->`)
            //   <-[...]-   backward    (lead `<-`, close `-`)
            //   -[...]-    undirected  (lead `-`,  close `-`)  — match either end
            let undirected;
            if backward {
                if !matches!(self.peek(), Tok::Dash) {
                    return Err(SqlError::UnexpectedToken {
                        expected: "-",
                        got: format!("{:?} (expected `-` to close backward `<-[…]-` hop)", self.peek()),
                    });
                }
                undirected = false;
            } else {
                match self.peek() {
                    Tok::Arrow => undirected = false,       // -[…]->
                    Tok::Dash  => undirected = true,        // -[…]-  (undirected)
                    other => return Err(SqlError::UnexpectedToken {
                        expected: "-> or -",
                        got: format!("{other:?} (expected `->` for a directed hop or `-` for an undirected `-[…]-` hop)"),
                    }),
                }
            }
            self.advance();

            // GQL graph-pattern quantifier in post-arrow position: `-[:e]->{1,5}`.
            // (openCypher's in-bracket `*1..5` is also accepted, parsed above.)
            // After the arrow the next token is a node `(`, so a `{` here is
            // unambiguously a quantifier.
            if matches!(self.peek(), Tok::LBrace) {
                let (mn, mx) = self.parse_brace_quantifier()?;
                min_depth = mn;
                max_depth = mx;
                if max_depth > MAX_HOP_DEPTH || min_depth > MAX_HOP_DEPTH {
                    return Err(SqlError::InvalidValue(format!(
                        "graph hop depth exceeds the maximum ({MAX_HOP_DEPTH})")));
                }
                if min_depth > max_depth {
                    return Err(SqlError::InvalidValue(format!(
                        "graph hop range invalid: min {min_depth} > max {max_depth}")));
                }
            }

            // Destination node — reuse the full node parser so it accepts
            // anonymous `(:label)`, inline props `(:label {f: 'v'})`, and bare `(v)`.
            let node = self.parse_match_node()?;
            let node_label = node.label.clone();
            let node_bind = node.var.clone()
                .unwrap_or_else(|| format!("__anon_hop{}", hops.len()));
            for (field, val) in node.props {
                inline_conds.push(DestWhere {
                    var: node_bind.clone(),
                    field,
                    op: CmpOp::Eq,
                    value: WhereValue::Literal(val),
                });
            }
            // Lower inline edge props to edge-field equality conditions. If props
            // were given without an explicit edge var, synthesize one so the filter
            // has a binding to reference (this also flips on the edge-meta path).
            if !edge_props.is_empty() && edge_bind.is_none() {
                edge_bind = Some(format!("__anon_edge{}", hops.len()));
            }
            if let Some(ref eb) = edge_bind {
                for (field, val) in std::mem::take(&mut edge_props) {
                    inline_conds.push(DestWhere {
                        var: eb.clone(),
                        field,
                        op: CmpOp::Eq,
                        value: WhereValue::Literal(val),
                    });
                }
            }
            hops.push(HopSpec { edge_type_hash, node_bind, edge_bind, min_depth, max_depth, node_label, backward, undirected });
        }
        Ok((hops, inline_conds))
    }

    /// Parse a GQL graph-pattern quantifier: `{n}` (exact), `{n,m}` (range),
    /// `{n,}` (n or more), `{,m}` (up to m). The caller has confirmed the current
    /// token is `{`. Returns `(min_depth, max_depth)`.
    fn parse_brace_quantifier(&mut self) -> Result<(u32, u32), SqlError> {
        self.advance(); // consume `{`
        let min = if let Tok::Num(_) = self.peek() {
            self.expect_num()? as u32
        } else {
            1 // `{,m}` → lower bound defaults to a single hop
        };
        let max = if matches!(self.peek(), Tok::Comma) {
            self.advance();
            if let Tok::Num(_) = self.peek() {
                self.expect_num()? as u32
            } else {
                MAX_HOP_DEPTH // `{n,}` → unbounded upper
            }
        } else {
            min // `{n}` → exact
        };
        match self.peek() {
            Tok::RBrace => { self.advance(); }
            other => return Err(SqlError::UnexpectedToken {
                expected: "}",
                got: format!("{other:?}"),
            }),
        }
        Ok((min, max))
    }

}

// ── Compiler ──────────────────────────────────────────────────────────────────

/// Convert a single CondExpr to a Step.
/// Resolve a `_key` comparison in a `SHORTEST` pattern to the slug it names.
///
/// The two pattern forms identify a node differently, which is forced rather
/// than chosen: `(a:p)` supplies the collection, so `_key` is the bare key and
/// the slug is built from both; a bare `(a)` has no collection to build with, so
/// the caller must give the whole slug.
///
/// Mixing them up produced silence. `(a)` with `'n0'` cannot match anything —
/// every slug contains a `/`, so no node is ever named `n0` — and the query
/// returned no rows as though the path did not exist. That is a statement about
/// the graph in answer to a question the graph was never asked, and it is
/// refusable without risk: the form is impossible, not merely unusual.
///
/// The mirror case, `(a:p)` with `'p/n0'`, builds `p/p/n0`. A key containing a
/// slash is legal, so that one *could* be someone's actual node and is left
/// alone.
fn shortest_slug(collection: Option<&String>, key_val: &str, binding: &str)
    -> Result<String, SqlError>
{
    match collection {
        Some(col) => Ok(format!("{col}/{key_val}")),
        None if key_val.contains('/') => Ok(key_val.to_string()),
        None => Err(SqlError::InvalidValue(format!(
            "`{binding}._key = '{key_val}'` cannot match any node: the pattern gives \
             `{binding}` no collection, so `_key` has to be a full slug like \
             'collection/{key_val}'. Either write it that way, or label the pattern \
             — `({binding}:collection)` — and keep the bare key."
        ))),
    }
}

fn compile_cond(cond: CondExpr) -> Step {
    match cond {
        CondExpr::Compare { field, op, value } => match op {
            CompareOp::Eq => Step::WhereEq(field, value),
            CompareOp::Neq => Step::WhereNeq(field, value),
            CompareOp::Gt => Step::WhereGt(field, value.as_f64().unwrap_or(0.0)),
            CompareOp::Lt => Step::WhereLt(field, value.as_f64().unwrap_or(0.0)),
            CompareOp::Gte => Step::WhereGte(field, value.as_f64().unwrap_or(0.0)),
            CompareOp::Lte => Step::WhereLte(field, value.as_f64().unwrap_or(0.0)),
        },
        CondExpr::Between { field, lo, hi } => Step::WhereBetween(field, lo, hi),
        CondExpr::In { field, values } => Step::WhereIn(field, values),
        CondExpr::ArrayContains { field, values } => Step::ArrayContains(field, values),
        CondExpr::Like {
            field,
            pattern,
            case_insensitive,
        } => Step::Like(field, pattern, case_insensitive),
        CondExpr::StDWithin {
            lat,
            lon,
            distance_m,
        } => Step::StDWithin(lat, lon, distance_m),
        CondExpr::StContainsPoint { lat, lon } => Step::StContainsPoint(lat, lon),
        CondExpr::StWithin { ring } => Step::StWithin(ring),
        CondExpr::StContains { ring } => Step::StContains(ring),
        CondExpr::StIntersects { ring } => Step::StIntersects(ring),
        CondExpr::StDistance {
            field,
            lat,
            lon,
            max_m,
        } => Step::StDistance(field, lat, lon, max_m),
        CondExpr::StLength { field, min_m } => Step::StLength(field, min_m),
        CondExpr::StPerimeter { field, min_m } => Step::StPerimeter(field, min_m),
        CondExpr::StArea { field, min_m2 } => Step::StArea(field, min_m2),
        CondExpr::Bm25 {
            field,
            query,
            min_score,
        } => Step::Bm25Filter(field, query, min_score),
        CondExpr::Bm25Func { .. } => unreachable!("Bm25Func should not reach compile_cond"),
        CondExpr::VectorNear { field, query, k } => Step::VectorNear { field, query, k },
        CondExpr::IsNull { field, negated } => Step::WhereIsNull(field, negated),
        CondExpr::Search { query, typo } => Step::SearchFilter(query, typo),
        CondExpr::Not(inner) => Step::WhereNot(Box::new(compile_cond(*inner))),
        CondExpr::Or(groups) => Step::WhereOr(
            groups
                .into_iter()
                .map(|group| group.into_iter().map(compile_cond).collect())
                .collect(),
        ),
    }
}

/// Append ORDER BY / OFFSET / LIMIT / SELECT steps to a pipeline.
fn append_tail(
    steps: &mut Vec<Step>,
    order_by: Option<OrderKey>,
    offset: Option<usize>,
    limit: Option<usize>,
    fields: Vec<String>,
    score_projections: Vec<(ScoreExpr, String)>,
) {
    if let Some(order_key) = order_by {
        match order_key {
            OrderKey::Fields(cols) => {
                steps.push(Step::Sort(cols));
            }
            OrderKey::Vector { field, query, metric } => {
                steps.push(Step::SortByVector { field, query, metric });
            }
            OrderKey::Expr(expr, ascending) => {
                steps.push(Step::SortByExpr { expr, ascending });
            }
        }
    }
    if let Some(n) = offset {
        steps.push(Step::Skip(n));
    }
    if let Some(n) = limit {
        steps.push(Step::Take(n));
    }
    if !score_projections.is_empty() {
        steps.push(Step::ScoreProject(score_projections));
    }
    if !fields.is_empty() {
        steps.push(Step::Select(fields));
    }
}

/// Spatial predicates that benefit from a grid *starter* (point-anchored radius/
/// distance over a large point collection): the grid narrows millions of points to
/// tens before any payload read.
///
/// Containment predicates (point-in-polygon and polygon–polygon) are deliberately
/// EXCLUDED. Polygon collections are small (hundreds–tens of thousands), the grid's
/// global cell size is dominated by the widest collection's extent (worldwide points
/// → ~degrees-wide cells) so its candidate lookup degenerates to a near-linear scan,
/// and the executor's FILTER path already does exact point-in-polygon via cached
/// polygon rings (no payload read). So route containment through the default
/// Collection-first plan → cheap exact filter over the collection's members.
fn is_grid_starter_cond(c: &CondExpr) -> bool {
    matches!(
        c,
        CondExpr::StDWithin { .. } | CondExpr::StDistance { .. }
    )
}

fn compile(stmt: SelectStmt) -> Vec<Step> {
    let SelectStmt {
        fields,
        source,
        conditions,
        group_by,
        having,
        distinct,
        order_by,
        limit,
        offset,
        score_projections,
    } = stmt;

    // ── Resolve ORDER BY alias → ScoreExpr ───────────────────────────────────────
    // When the user writes `SELECT BM25(f,'q') AS score ... ORDER BY score DESC`,
    // the parser sees `score` as a plain field name.  Replace it with the actual
    // ScoreExpr from the projection so SortByExpr evaluates it correctly.
    let order_by = match order_by {
        Some(OrderKey::Fields(ref cols)) if cols.len() == 1 && !score_projections.is_empty() => {
            let (ref name, ascending) = cols[0];
            if let Some((expr, _)) = score_projections.iter().find(|(_, alias)| alias == name) {
                Some(OrderKey::Expr(expr.clone(), ascending))
            } else {
                order_by
            }
        }
        other => other,
    };

    let mut steps: Vec<Step> = Vec::new();

    // ── Fast-path 1: Collection + WHERE _key = 'val' → O(1) One(hash) ───────────
    //
    // Instead of loading every node in the collection and scanning payloads,
    // compute the slug hash directly and emit a single-node lookup.
    if let Source::Collection(ref name) = source {
        let key_pos = conditions.iter().position(|c| {
            matches!(c, CondExpr::Compare {
                field,
                op: CompareOp::Eq,
                value: Value::String(_),
            } if field == "_key")
        });
        if let Some(pos) = key_pos {
            let key_val = match &conditions[pos] {
                CondExpr::Compare {
                    value: Value::String(s),
                    ..
                } => s.clone(),
                _ => unreachable!(),
            };
            steps.push(Step::One(sk_hash(&format!("{}/{}", name, key_val))));
            for (i, cond) in conditions.into_iter().enumerate() {
                if i != pos {
                    steps.push(compile_cond(cond));
                }
            }
            push_group_having_distinct(&mut steps, group_by, having, distinct);
            append_tail(&mut steps, order_by, offset, limit, fields, score_projections);
            return steps;
        }
    }

    // ── Fast-path 2: Collection + spatial WHERE → grid starter ───────────────────
    //
    // When the spatial index is available, we want the grid to produce the initial
    // candidate list (~tens of nodes) rather than loading the entire collection
    // (~thousands) and then filtering.  Emit spatial steps first with an empty
    // candidates list so they act as starters, then gate on _collection membership.
    if let Source::Collection(ref name) = source {
        if conditions.iter().any(is_grid_starter_cond) {
            let coll_name = name.clone();
            // Partition once: grid-startable spatial conds go first (as starters),
            // everything else (containment predicates + non-spatial filters) follows
            // after the collection gate, running as cheap exact filters.
            let (spatial_conds, other_conds): (Vec<CondExpr>, Vec<CondExpr>) =
                conditions.into_iter().partition(|c| is_grid_starter_cond(c));
            for cond in spatial_conds {
                steps.push(compile_cond(cond));
            }
            // Gate on collection membership (nodes without _collection are excluded)
            steps.push(Step::WhereEq(
                "_collection".to_string(),
                Value::String(coll_name),
            ));
            for cond in other_conds {
                steps.push(compile_cond(cond));
            }
            push_group_having_distinct(&mut steps, group_by, having, distinct);
            append_tail(&mut steps, order_by, offset, limit, fields, score_projections);
            return steps;
        }
    }

    // ── Fast-path 3: Collection + VECTOR_NEAR → HNSW starter ──────────────────────
    //
    // Fire the HNSW ANN index (a VectorNear starter with an empty candidate list) to
    // get the approximate top-k, THEN gate on _collection membership — instead of
    // loading the entire collection and exact-scanning it. Without this, a filtered
    // top-k query silently bypasses the built HNSW and does an O(N) exact scan.
    if let Source::Collection(ref name) = source {
        if conditions.iter().any(|c| matches!(c, CondExpr::VectorNear { .. })) {
            let coll_name = name.clone();
            let (vec_conds, other_conds): (Vec<CondExpr>, Vec<CondExpr>) =
                conditions.into_iter().partition(|c| matches!(c, CondExpr::VectorNear { .. }));
            // CORRECTNESS: the starter searches the vector index, which is keyed by
            // FIELD NAME — so it spans every collection using that field (a
            // materialised view mirroring its source's vectors is the common case).
            // Asking it for exactly `k` and *then* gating on `_collection` can throw
            // away every hit and answer empty for a query that has matches. Ask for a
            // wider band, gate, then cut back to the requested `k` so the result is
            // the k nearest *within the collection*.
            const VEC_SCOPE_OVERSAMPLE: usize = 16;
            let mut requested_k = 0usize;
            for cond in vec_conds {
                let widened = match cond {
                    CondExpr::VectorNear { field, query, k } => {
                        requested_k = k;
                        CondExpr::VectorNear {
                            field,
                            query,
                            k: k.saturating_mul(VEC_SCOPE_OVERSAMPLE).max(k),
                        }
                    }
                    other => other,
                };
                steps.push(compile_cond(widened)); // VectorNear starter → HNSW
            }
            steps.push(Step::WhereEq("_collection".to_string(), Value::String(coll_name)));
            if requested_k > 0 {
                // Trim the widened band back to the k nearest that are in-collection.
                steps.push(Step::Take(requested_k));
            }
            for cond in other_conds {
                steps.push(compile_cond(cond));
            }
            push_group_having_distinct(&mut steps, group_by, having, distinct);
            append_tail(&mut steps, order_by, offset, limit, fields, score_projections);
            return steps;
        }
    }

    // ── Default path ──────────────────────────────────────────────────────────────
    match source {
        Source::Collection(name) => steps.push(Step::Collection(sk_hash(&name))),
        Source::All => steps.push(Step::All),
    }
    for cond in conditions {
        steps.push(compile_cond(cond));
    }
    push_group_having_distinct(&mut steps, group_by, having, distinct);
    append_tail(&mut steps, order_by, offset, limit, fields, score_projections);
    steps
}

/// Emit GROUP BY / HAVING / DISTINCT steps between conditions and the tail.
fn push_group_having_distinct(
    steps: &mut Vec<Step>,
    group_by: Vec<String>,
    having: Vec<CondExpr>,
    distinct: bool,
) {
    if !group_by.is_empty() {
        steps.push(Step::GroupBy(group_by));
    }
    if !having.is_empty() {
        let having_steps: Vec<Step> = having.into_iter().map(compile_cond).collect();
        steps.push(Step::Having(having_steps));
    }
    if distinct {
        steps.push(Step::Distinct);
    }
}

/// If a `Value::Array` contains only numbers, return them as `Vec<f32>`.
/// Used to detect vector literals in INSERT/UPDATE values.
fn value_as_f32_vec(v: &Value) -> Option<Vec<f32>> {
    let arr = v.as_array()?;
    if arr.is_empty() {
        return None;
    }
    arr.iter()
        .map(|x| x.as_f64().map(|f| f as f32))
        .collect()
}

/// If a string value looks like JSON (`{...}` or `[...]`), parse it into a
/// native JSON object/array. This enables inserting geometry and nested data.
fn maybe_parse_json_string(value: Value) -> Value {
    if let Value::String(ref s) = value {
        let trimmed = s.trim();
        if (trimmed.starts_with('{') && trimmed.ends_with('}'))
            || (trimmed.starts_with('[') && trimmed.ends_with(']'))
        {
            if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
                return parsed;
            }
        }
    }
    value
}

/// Compile a single row of values (with the given fields and collection) into
/// (slug, payload_json, vectors). Shared logic for single-row and multi-row INSERT.
fn compile_insert_row(
    collection: &str,
    fields: &[String],
    values: Vec<Value>,
) -> Result<(String, String, Vec<(String, Vec<f32>)>), SqlError> {
    if fields.len() != values.len() {
        return Err(SqlError::FieldValueCountMismatch {
            fields: fields.len(),
            values: values.len(),
        });
    }
    // Empty-string sentinel means "_key absent — defer to schema default in execute()".
    let slug = match fields.iter().position(|f| f == "_key") {
        Some(idx) => match &values[idx] {
            Value::String(s) => format!("{}/{}", collection, s),
            other => {
                return Err(SqlError::InvalidValue(format!(
                    "_key must be a string, got {other}"
                )))
            }
        },
        None => String::new(), // deferred: execute() will generate via schema UUID default
    };
    let mut obj = serde_json::Map::new();
    obj.insert("_collection".into(), Value::String(collection.to_string()));
    // _id will be backfilled in execute() when slug is empty
    if !slug.is_empty() {
        obj.insert("_id".into(), Value::String(slug.clone()));
    }
    let mut vectors: Vec<(String, Vec<f32>)> = Vec::new();
    for (field, value) in fields.iter().zip(values) {
        if let Some(floats) = value_as_f32_vec(&value) {
            // Array-of-numbers → vector field, not stored in the JSON payload
            vectors.push((field.clone(), floats));
        } else {
            obj.insert(field.clone(), maybe_parse_json_string(value));
        }
    }
    let payload_json =
        serde_json::to_string(&obj).map_err(|e| SqlError::InvalidValue(e.to_string()))?;
    Ok((slug, payload_json, vectors))
}

fn compile_insert(stmt: InsertStmt) -> Result<CompiledMutation, SqlError> {
    if stmt.rows.len() == 1 {
        // Single-row: produce Insert (zero overhead for existing queries)
        let (slug, payload_json, vectors) =
            compile_insert_row(&stmt.collection, &stmt.fields, stmt.rows.into_iter().next().unwrap())?;
        Ok(CompiledMutation::Insert { collection: stmt.collection, slug, payload_json, vectors })
    } else {
        // Multi-row: produce InsertBatch
        let mut items = Vec::with_capacity(stmt.rows.len());
        for row in stmt.rows {
            items.push(compile_insert_row(&stmt.collection, &stmt.fields, row)?);
        }
        Ok(CompiledMutation::InsertBatch { collection: stmt.collection, items })
    }
}

/// Compile a scan-or-lookup starter for a keyed mutation. If `conditions`
/// contains `_key = '<literal>'`, the primary key IS the slug, so we start from
/// the direct `One(hash("collection/key"))` lookup (O(1)) and apply the
/// remaining conditions as filters — instead of scanning the whole collection
/// (O(N)). Consumes `conditions` to avoid cloning.
fn compile_keyed_steps(collection: &str, conditions: Vec<CondExpr>) -> Vec<Step> {
    let key_pos = conditions.iter().position(|c| match c {
        CondExpr::Compare { field, op: CompareOp::Eq, value } =>
            field == "_key" && value.is_string(),
        _ => false,
    });
    let mut steps = Vec::with_capacity(conditions.len() + 1);
    match key_pos {
        Some(i) => {
            let key = match &conditions[i] {
                CondExpr::Compare { value, .. } => value.as_str().unwrap_or_default().to_string(),
                _ => unreachable!(),
            };
            steps.push(Step::One(sk_hash(&format!("{collection}/{key}"))));
            for (j, cond) in conditions.into_iter().enumerate() {
                if j != i { steps.push(compile_cond(cond)); }
            }
        }
        None => {
            steps.push(Step::Collection(sk_hash(collection)));
            for cond in conditions { steps.push(compile_cond(cond)); }
        }
    }
    steps
}

fn compile_delete(stmt: DeleteStmt) -> Vec<Step> {
    match stmt.source {
        Source::Collection(name) => compile_keyed_steps(&name, stmt.conditions),
        Source::All => {
            let mut steps = vec![Step::All];
            for cond in stmt.conditions { steps.push(compile_cond(cond)); }
            steps
        }
    }
}

fn compile_update(stmt: UpdateStmt) -> CompiledMutation {
    CompiledMutation::Update {
        steps: compile_keyed_steps(&stmt.collection, stmt.conditions),
        updates: stmt.updates,
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// The result of parsing a MATCH statement — either a compiled step pipeline
/// (simple graph traversal) or an aggregate statement.
pub enum MatchOrAgg {
    /// Standard MATCH — compiled to a Vec<Step> pipeline.
    Steps(Vec<Step>),
    /// Aggregate MATCH — must be executed via `execute_match_agg`.
    Agg(crate::query::MatchAggStmt),
    /// `SELECT … FROM MATCH … UNION SELECT … FROM MATCH …` — each arm executed
    /// via `execute_match_agg`, results concatenated and de-duplicated.
    AggUnion(Vec<crate::query::MatchAggStmt>),
    /// `SELECT … FROM MATCH SHORTEST` — must be executed via `execute_shortest_select`.
    Shortest(crate::query::ShortestSelectStmt),
    /// `SELECT … FROM source1, source2, …` — must be executed via `execute_multi_from`.
    MultiFrom(crate::query::MultiFromStmt),
}

fn parse_match_or_agg_inner(
    sql: &str,
    params: Vec<Value>,
) -> Result<(MatchOrAgg, PlanNeeds), SqlError> {
    let tokens = tokenize(sql)?;
    // Bound parser recursion by rejecting pathologically-nested input up front.
    if max_nesting(&tokens) > MAX_NEST_DEPTH {
        return Err(SqlError::InvalidValue(format!(
            "query nesting too deep (> {MAX_NEST_DEPTH} levels of parentheses/brackets)"
        )));
    }
    lower_tokens(tokens, params)
}

/// A compiled query: the SQL is tokenized (and nesting-checked) ONCE; each run
/// re-lowers the cached tokens with fresh `$N` parameters, skipping
/// re-tokenization. Created by [`crate::CoreDB::prepare`]. Reusable and cheap to
/// clone-and-run for the same query shape with different parameter values.
pub struct PreparedQuery {
    tokens: Vec<Tok>,
}

impl PreparedQuery {
    pub(crate) fn compile(sql: &str) -> Result<Self, SqlError> {
        let tokens = tokenize(sql)?;
        if max_nesting(&tokens) > MAX_NEST_DEPTH {
            return Err(SqlError::InvalidValue(format!(
                "query nesting too deep (> {MAX_NEST_DEPTH} levels of parentheses/brackets)"
            )));
        }
        Ok(Self { tokens })
    }

    /// Lower the cached tokens into a plan, binding the given parameters.
    pub(crate) fn lower(&self, params: Vec<Value>) -> Result<(MatchOrAgg, PlanNeeds), SqlError> {
        lower_tokens(self.tokens.clone(), params)
    }
}

/// Turn a token stream + parameters into an executable plan. This is the second
/// half of `parse_match_or_agg_inner` — reused by [`PreparedQuery`] so the
/// (already-tokenized) SQL need not be re-tokenized on every execution.
/// Lower tokens to a plan, together with the index requirements the statement
/// carries — see [`Parser::bm25_needs`].
fn lower_tokens(
    tokens: Vec<Tok>,
    params: Vec<Value>,
) -> Result<(MatchOrAgg, PlanNeeds), SqlError> {
    // Multi-FROM: SELECT … FROM source1, source2, … (comma between FROM sources)
    if is_multi_from(&tokens) {
        let mut parser = Parser::with_params(tokens, params);
        let stmt = parser.parse_select_multi_from()?;
        return Ok((MatchOrAgg::MultiFrom(stmt), parser.needs()));
    }

    // SELECT … FROM MATCH [SHORTEST] — check before regular SELECT routing.
    if is_select_from_match(&tokens) {
        if is_select_from_match_shortest(&tokens) {
            let mut parser = Parser::with_params(tokens, params);
            let stmt = parser.parse_select_from_match_shortest()?;
            return Ok((MatchOrAgg::Shortest(stmt), parser.needs()));
        }
        let mut parser = Parser::with_params(tokens, params);
        let first = parser.parse_select_from_match()?;
        if matches!(parser.peek(), Tok::Kw(Kw::Union)) {
            let mut stmts = vec![first];
            while matches!(parser.peek(), Tok::Kw(Kw::Union)) {
                parser.advance();
                stmts.push(parser.parse_select_from_match()?);
            }
            return Ok((MatchOrAgg::AggUnion(stmts), parser.needs()));
        }
        return Ok((MatchOrAgg::Agg(first), parser.needs()));
    }

    // Bare `MATCH ...` (Cypher-style `MATCH ... RETURN` / `MATCH ... WITH`) is no
    // longer a valid surface.  Graph queries must be written as
    // `SELECT ... FROM MATCH (...)`, which is a strict superset (projection,
    // WHERE, GROUP BY, ORDER BY, aggregation, edge properties, UNION, WITH).
    if matches!(tokens.first(), Some(Tok::Kw(Kw::Match))) {
        return Err(SqlError::UnexpectedToken {
            expected: "SELECT ... FROM MATCH (...)",
            got: "bare MATCH statement — write it as `SELECT ... FROM MATCH (...)`".into(),
        });
    }

    // Everything else goes through the regular SELECT/DML pipeline.
    let mut parser = Parser::with_params(tokens, params);
    let stmt = parser.parse()?;
    // Guard: a trailing UNION on a PLAIN SELECT was previously dropped silently,
    // returning only the first branch (wrong, no error). UNION is supported only
    // between MATCH patterns. Fail loudly rather than serve a partial result.
    if matches!(parser.peek(), Tok::Kw(Kw::Union)) {
        return Err(SqlError::UnexpectedToken {
            expected: "end of query",
            got: "UNION — plain `SELECT … UNION SELECT …` is not supported; use \
                   `SELECT … FROM MATCH … UNION SELECT … FROM MATCH …`, or run the \
                   two SELECTs separately and merge"
                .into(),
        });
    }
    // Nothing may be left over. Without this the parser stopped at whatever it
    // understood and dropped the rest in silence, so a clause it does not
    // implement read as if it had been applied: `ORDER BY n ASC NULLS LAST`
    // returned the plain `ASC` order, and `WHERE a > 1 GARBAGE HERE` ran. Both
    // answered a different question from the one that was asked, with nothing to
    // say so.
    //
    // Reporting the leftover token is the whole value of the check — "end of
    // query" alone would leave the author guessing which part was not understood.
    if !matches!(parser.peek(), Tok::Eof) {
        return Err(SqlError::UnexpectedToken {
            expected: "end of query",
            got: format!("{:?} — this part of the statement was not understood, and \
                          ignoring it would answer a different question", parser.peek()),
        });
    }
    Ok((MatchOrAgg::Steps(compile(stmt)), parser.needs()))
}

/// Parse a MATCH statement and determine whether it is a simple graph query
/// or an aggregate (multi-hop with RETURN var.field / aggregate functions).
///
/// This is the unified entry-point called by `db.query()`.
///
/// # Errors
/// Returns [`SqlError`] if the statement is syntactically invalid.
pub fn parse_match_or_agg(sql: &str) -> Result<MatchOrAgg, SqlError> {
    parse_match_or_agg_inner(sql, vec![]).map(|(plan, _)| plan)
}

/// The same, keeping the index requirements the statement carries. `query()` uses
/// this one: dropping them is what let a `BM25` with no index answer "no rows".
pub(crate) fn parse_plan(
    sql: &str,
    params: Vec<Value>,
) -> Result<(MatchOrAgg, PlanNeeds), SqlError> {
    parse_match_or_agg_inner(sql, params)
}

/// Parse a MATCH/SELECT statement with parameter bindings (`$1`, `$2`, …).
pub fn parse_match_or_agg_params(sql: &str, params: Vec<Value>) -> Result<MatchOrAgg, SqlError> {
    parse_match_or_agg_inner(sql, params).map(|(plan, _)| plan)
}

/// Return `true` when `MATCH` is the first token and the stream contains a `WITH`
/// keyword or a `RETURN` followed by a `var.field` projection (dot after ident).
///
/// Simple `MATCH (a)-[:e]->(b) RETURN a, b` does NOT match — it routes through
/// `parse_match` instead.  `MATCH ... WITH ...` or `MATCH ... RETURN b._key AS k`
/// matches and routes through `parse_match_return_with`.
/// Return `true` when the SQL is `SELECT … FROM MATCH (…)-[…]->(…) …`.
fn is_select_from_match(tokens: &[Tok]) -> bool {
    if !matches!(tokens.first(), Some(Tok::Kw(Kw::Select))) {
        return false;
    }
    tokens.windows(2).any(|w| {
        matches!(w, [Tok::Kw(Kw::From), Tok::Kw(Kw::Match)])
    })
}

/// Return `true` when the SQL is `SELECT … FROM MATCH SHORTEST (…)-[…]->(…) …`.
fn is_select_from_match_shortest(tokens: &[Tok]) -> bool {
    // Look for FROM MATCH SHORTEST (3-token sequence)
    tokens.windows(3).any(|w| {
        matches!(w[0], Tok::Kw(Kw::From))
        && matches!(w[1], Tok::Kw(Kw::Match))
        && matches!(&w[2], Tok::Ident(name) if name.eq_ignore_ascii_case("SHORTEST"))
    })
}

/// Return `true` when the SQL has multiple comma-separated FROM sources.
///
/// Detects a top-level comma after the FROM keyword (paren depth = 0).
fn is_multi_from(tokens: &[Tok]) -> bool {
    if !matches!(tokens.first(), Some(Tok::Kw(Kw::Select))) {
        return false;
    }
    // Find the FROM keyword position
    let from_pos = match tokens.iter().position(|t| matches!(t, Tok::Kw(Kw::From))) {
        Some(p) => p,
        None => return false,
    };
    // Scan after FROM for a top-level comma. Depth counts ALL bracket kinds:
    // `()` node patterns, `[]` edge patterns, and `{}` inline props / `{n,m}`
    // quantifiers — a comma inside any of those (e.g. `{1,3}` or `{a:1, b:2}`)
    // is NOT a FROM-source separator.
    let mut depth: usize = 0;
    for tok in &tokens[from_pos + 1..] {
        match tok {
            Tok::LParen | Tok::LBracket | Tok::LBrace => depth += 1,
            Tok::RParen | Tok::RBracket | Tok::RBrace => { if depth > 0 { depth -= 1; } }
            Tok::Comma if depth == 0 => return true,
            // Stop scanning at ORDER/LIMIT/WHERE/GROUP (these appear after all FROM sources)
            Tok::Kw(Kw::Where | Kw::Order | Kw::Limit | Kw::Group) if depth == 0 => break,
            _ => {}
        }
    }
    false
}

/// Map an uppercase PATH_* / time function name + (var, field) to a `MatchAggReturn`.
fn path_func_variant(
    name: &str,
    var: String,
    field: String,
) -> Result<crate::query::MatchAggReturn, SqlError> {
    use crate::query::MatchAggReturn;
    match name {
        "PATH_AVG"     => Ok(MatchAggReturn::PathAvg { var, field }),
        "PATH_SUM"     => Ok(MatchAggReturn::PathSum { var, field }),
        "PATH_MIN"     => Ok(MatchAggReturn::PathMin { var, field }),
        "PATH_MAX"     => Ok(MatchAggReturn::PathMax { var, field }),
        "PATH_PRODUCT" => Ok(MatchAggReturn::PathProduct { var, field }),
        "PATH_FIRST"   => Ok(MatchAggReturn::PathFirst { var, field }),
        "PATH_LAST"    => Ok(MatchAggReturn::PathLast { var, field }),
        "AGE_DAYS"     => Ok(MatchAggReturn::AgeDays { var, field }),
        "AGE_HOURS"    => Ok(MatchAggReturn::AgeHours { var, field }),
        "JSON_ARRAY_LENGTH" => Ok(MatchAggReturn::JsonArrayLen { var, field }),
        _ => Err(SqlError::UnexpectedToken {
            expected: "known PATH_* or time function",
            got: name.to_string(),
        }),
    }
}

/// Look at the token stream to determine if a MATCH statement has an
/// aggregate RETURN clause (var.field or SUM/AVG/MIN/MAX/COUNT).
/// Parse an SQL SELECT string and compile it to a `Vec<Step>`.
///
/// This is a pure function — no DB access required.
/// You can call it to validate SQL ahead of time, or pass the result to
/// [`Set::from_steps`](crate::Set::from_steps).
///
/// # Errors
/// Returns [`SqlError`] if the SQL is syntactically invalid.
pub fn parse_and_compile(sql: &str) -> Result<Vec<Step>, SqlError> {
    match parse_match_or_agg(sql)? {
        MatchOrAgg::Steps(steps) => Ok(steps),
        MatchOrAgg::Agg(_) => Err(SqlError::UnexpectedToken {
            expected: "simple MATCH or SELECT (not aggregate MATCH)",
            got: "aggregate SELECT FROM MATCH".into(),
        }),
        MatchOrAgg::AggUnion(_) => Err(SqlError::UnexpectedToken {
            expected: "simple MATCH or SELECT (not aggregate MATCH UNION)",
            got: "aggregate SELECT FROM MATCH UNION".into(),
        }),
        MatchOrAgg::Shortest(_) => Err(SqlError::UnexpectedToken {
            expected: "simple MATCH or SELECT (not MATCH SHORTEST)",
            got: "SELECT FROM MATCH SHORTEST".into(),
        }),
        MatchOrAgg::MultiFrom(_) => Err(SqlError::UnexpectedToken {
            expected: "simple MATCH or SELECT (not multi-FROM)",
            got: "SELECT FROM multiple sources".into(),
        }),
    }
}

/// Find `kw` (lowercase) as a WHOLE word in `hay` (must be lowercase). Returns its
/// byte offset. Used for the light textual parse of the VIEW DDL header.
fn find_kw(hay: &str, kw: &str) -> Option<usize> {
    let mut start = 0;
    while let Some(p) = hay[start..].find(kw) {
        let idx = start + p;
        let before_ok = idx == 0 || !hay.as_bytes()[idx - 1].is_ascii_alphanumeric();
        let after = idx + kw.len();
        let after_ok = after >= hay.len() || !hay.as_bytes()[after].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return Some(idx);
        }
        start = idx + kw.len();
    }
    None
}

/// Textually recognize `CREATE [MATERIALIZED] [SEARCH] VIEW name AS <body>` and
/// `REFRESH MATERIALIZED VIEW name`. Returns `None` for anything else (falls through
/// to the token parser). Kept textual so the body SQL survives verbatim for refresh.
fn parse_view_ddl(sql: &str) -> Option<CompiledMutation> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let words: Vec<&str> = trimmed.split_whitespace().collect();
    if words.is_empty() {
        return None;
    }

    // REFRESH [MATERIALIZED] VIEW name
    if words[0].eq_ignore_ascii_case("refresh") {
        let mut i = 1;
        if words.get(i).map_or(false, |w| w.eq_ignore_ascii_case("materialized")) { i += 1; }
        if !words.get(i).map_or(false, |w| w.eq_ignore_ascii_case("view")) { return None; }
        let name = words.get(i + 1)?.trim_end_matches(';');
        return Some(CompiledMutation::RefreshView { name: name.to_string() });
    }

    // CREATE MATERIALIZED VIEW name [WITH (autoindex[ = true])] AS <body>
    // (No `SEARCH VIEW`: automatic refresh isn't embedded-native, so a preset that
    // implies it would mislead. `WITH (autoindex)` gives the auto-index convenience;
    // freshness is explicit via `REFRESH MATERIALIZED VIEW`.)
    if !(words[0].eq_ignore_ascii_case("create")
        && words.get(1).map_or(false, |w| w.eq_ignore_ascii_case("materialized"))
        && words.get(2).map_or(false, |w| w.eq_ignore_ascii_case("view")))
    {
        return None;
    }
    let name = words.get(3)?.to_string();

    // Body = raw text after the header's `AS`; between the name and `AS` is the optional
    // `WITH (…)` clause — `autoindex` there flips on auto-indexing.
    let lower = trimmed.to_ascii_lowercase();
    let vp = find_kw(&lower, "view")?;
    let rel = find_kw(&lower[vp + 4..], "as")?;
    let as_start = vp + 4 + rel;
    let header_mid = &lower[vp + 4..as_start]; // " name [with (autoindex …)]"
    let auto_index = header_mid.contains("autoindex");
    let query_sql = trimmed[as_start + 2..].trim().to_string();
    if query_sql.is_empty() {
        return None;
    }
    Some(CompiledMutation::CreateView { name, query_sql, auto_index })
}

/// Extract the root collection label of a `… FROM MATCH (var:collection)…` projection
/// (used to mirror the root's source vectors into a materialized view). Textual, on the
/// standard form; returns `None` if no `MATCH (…)` node is found.
pub fn extract_root_collection(sql: &str) -> Option<String> {
    let lower = sql.to_ascii_lowercase();
    let mp = find_kw(&lower, "match")?;
    let after = &sql[mp + 5..];
    let lp = after.find('(')?;
    let inner = &after[lp + 1..];
    let end = inner.find(|c: char| c == ')' || c == '-' || c == '{' || c == ',' || c.is_whitespace())
        .unwrap_or(inner.len());
    let node = &inner[..end]; // e.g. "s:song", ":song", or "song"
    let coll = match node.find(':') { Some(c) => &node[c + 1..], None => node };
    let coll = coll.trim();
    if coll.is_empty() { None } else { Some(coll.to_string()) }
}

fn parse_mutation_inner(sql: &str, params: Vec<Value>) -> Result<CompiledMutation, SqlError> {
    // `CREATE [MATERIALIZED|SEARCH] VIEW …` and `REFRESH MATERIALIZED VIEW …` are
    // handled textually so the body SQL (after `AS`) can be stored verbatim to re-run
    // on refresh (the tokenizer drops source positions). The body is validated when
    // the view is materialized.
    if let Some(m) = parse_view_ddl(sql) {
        return Ok(m);
    }

    let tokens = tokenize(sql)?;
    let mut parser = Parser::with_params(tokens, params);
    let parsed = parse_mutation_body(&mut parser)?;
    // Nothing may be left over — and on a mutation this is not a nicety.
    //
    // `DELETE FROM p GARBAGE WHERE n = 1` parsed as `DELETE FROM p`, found an
    // identifier where it looked for `WHERE`, concluded there was no `WHERE`, and
    // deleted **every row in the table**. The rest of the statement was dropped in
    // silence. A typo between the table name and the predicate was the difference
    // between removing one row and emptying the collection, with no error and
    // nothing in the result to say which had happened.
    if !matches!(parser.peek(), Tok::Eof) {
        return Err(SqlError::UnexpectedToken {
            expected: "end of statement",
            got: format!("{:?} — this part of the statement was not understood, and \
                          ignoring it would run a different statement", parser.peek()),
        });
    }
    Ok(parsed)
}

/// The statement dispatch itself. Split out so [`parse_mutation_inner`] can check
/// that the whole statement was consumed before acting on it.
fn parse_mutation_body(parser: &mut Parser) -> Result<CompiledMutation, SqlError> {
    match parser.peek().clone() {
        Tok::Kw(Kw::Insert) => {
            parser.advance(); // consume INSERT
            match parser.peek() {
                Tok::Kw(Kw::Into) => {
                    let stmt = parser.parse_insert_node()?;
                    compile_insert(stmt)
                }
                Tok::LParen => {
                    let edges = parser.parse_insert_edge()?;
                    Ok(CompiledMutation::InsertEdge(edges))
                }
                Tok::Eof => Err(SqlError::UnexpectedEnd {
                    expected: "INTO or (",
                }),
                other => Err(SqlError::UnexpectedToken {
                    expected: "INTO or (",
                    got: format!("{other:?}"),
                }),
            }
        }
        Tok::Kw(Kw::Delete) => {
            parser.advance(); // consume DELETE
            match parser.peek() {
                Tok::Kw(Kw::From) => {
                    let stmt = parser.parse_delete_node()?;
                    Ok(CompiledMutation::Delete(compile_delete(stmt)))
                }
                Tok::LParen => {
                    let edges = parser.parse_delete_edge()?;
                    Ok(CompiledMutation::DeleteEdge(edges))
                }
                Tok::Eof => Err(SqlError::UnexpectedEnd {
                    expected: "FROM or (",
                }),
                other => Err(SqlError::UnexpectedToken {
                    expected: "FROM or (",
                    got: format!("{other:?}"),
                }),
            }
        }
        Tok::Kw(Kw::Update) => {
            // `UPDATE ('a')-[:T …]->('b') …` (edge) vs `UPDATE table …` (node).
            if matches!(parser.tokens.get(parser.pos + 1), Some(Tok::LParen)) {
                parser.parse_update_edge()
            } else {
                let stmt = parser.parse_update()?;
                Ok(compile_update(stmt))
            }
        }
        Tok::Kw(Kw::Match) => parser.parse_match_insert(),
        Tok::Kw(Kw::Create) => {
            parser.advance(); // consume CREATE
            match parser.peek() {
                Tok::Kw(Kw::Index) => parser.parse_create_index(),
                _ => {
                    let schema = parser.parse_create_table()?;
                    Ok(CompiledMutation::CreateTable {
                        collection: schema.collection.clone(),
                        schema,
                    })
                }
            }
        }
        Tok::Kw(Kw::Drop) => {
            parser.advance(); // consume DROP
            match parser.peek().clone() {
                // DROP TABLE [IF EXISTS] collection
                Tok::Kw(Kw::Table) => {
                    parser.advance(); // consume TABLE
                    let if_exists = if matches!(parser.peek(), Tok::Kw(Kw::If)) {
                        parser.advance();
                        match parser.peek().clone() {
                            Tok::Kw(Kw::Exists) => { parser.advance(); true }
                            other => return Err(SqlError::UnexpectedToken {
                                expected: "EXISTS",
                                got: format!("{other:?}"),
                            }),
                        }
                    } else { false };
                    let collection = parser.parse_table_ref()?;
                    Ok(CompiledMutation::DropTable { collection, if_exists })
                }
                // DROP INDEX [IF EXISTS] ON collection USING method (field)
                Tok::Kw(Kw::Index) => {
                    parser.advance(); // consume INDEX
                    let if_exists = if matches!(parser.peek(), Tok::Kw(Kw::If)) {
                        parser.advance();
                        match parser.peek().clone() {
                            Tok::Kw(Kw::Exists) => { parser.advance(); true }
                            other => return Err(SqlError::UnexpectedToken {
                                expected: "EXISTS",
                                got: format!("{other:?}"),
                            }),
                        }
                    } else { false };
                    parser.expect_kw(Kw::On, "ON")?;
                    let collection = parser.parse_table_ref()?;
                    parser.expect_kw(Kw::Using, "USING")?;
                    let method_str = parser.expect_ident()?;
                    let method = match method_str.to_ascii_uppercase().as_str() {
                        "BTREE"   => IndexMethod::Btree,
                        "HASH"    => IndexMethod::Hash,
                        "GIN"     => IndexMethod::Gin,
                        "GIST"    => IndexMethod::Gist,
                        "BM25"    => IndexMethod::Bm25,
                        "SPATIAL" => IndexMethod::Spatial,
                        "HNSW"    => IndexMethod::Hnsw,
                        "SEARCH"  => IndexMethod::Search,
                        other => return Err(SqlError::UnexpectedToken {
                            expected: "BTREE, HASH, GIN, GIST, BM25, SPATIAL, HNSW, or SEARCH",
                            got: other.to_string(),
                        }),
                    };
                    parser.expect_lparen()?;
                    let field = parser.expect_ident()?;
                    parser.expect_rparen()?;
                    Ok(CompiledMutation::DropIndex { collection, method, field, if_exists })
                }
                other => Err(SqlError::UnexpectedToken {
                    expected: "TABLE or INDEX",
                    got: format!("{other:?}"),
                }),
            }
        }
        Tok::Kw(Kw::Alter) => {
            parser.advance(); // consume ALTER
            parser.parse_alter_table()
        }
        Tok::Kw(Kw::Reindex) => {
            parser.advance(); // consume REINDEX
            parser.expect_kw(Kw::On, "ON")?;
            let collection = parser.parse_table_ref()?;
            parser.expect_kw(Kw::Using, "USING")?;
            let method_str = parser.expect_ident()?;
            let method = match method_str.to_lowercase().as_str() {
                "btree"   => IndexMethod::Btree,
                "hash"    => IndexMethod::Hash,
                "gin"     => IndexMethod::Gin,
                "gist"    => IndexMethod::Gist,
                "bm25"    => IndexMethod::Bm25,
                "spatial" => IndexMethod::Spatial,
                "hnsw"    => IndexMethod::Hnsw,
                "search"  => IndexMethod::Search,
                other => return Err(SqlError::UnexpectedToken {
                    expected: "btree, hash, gin, gist, bm25, spatial, hnsw, or search",
                    got: other.to_string(),
                }),
            };
            parser.expect_lparen()?;
            let mut fields = vec![parser.expect_ident()?];
            while matches!(parser.peek(), Tok::Comma) {
                parser.advance();
                fields.push(parser.expect_ident()?);
            }
            parser.expect_rparen()?;
            Ok(CompiledMutation::Reindex { collection, method, fields })
        }
        Tok::Kw(Kw::Begin) => {
            parser.advance();
            Ok(CompiledMutation::Begin)
        }
        Tok::Kw(Kw::Commit) => {
            parser.advance();
            Ok(CompiledMutation::Commit)
        }
        Tok::Kw(Kw::Rollback) => {
            parser.advance();
            Ok(CompiledMutation::Rollback)
        }
        Tok::Kw(Kw::Compact) => {
            parser.advance();
            Ok(CompiledMutation::Compact)
        }
        Tok::Kw(Kw::Set) => {
            parser.advance(); // consume SET
            let var = parser.expect_ident()?;
            let var_upper = var.to_uppercase();
            if var_upper != "WAL_FORMAT" && var_upper != "WAL_MODE" && var_upper != "WAL_SYNC" {
                return Err(SqlError::UnexpectedToken {
                    expected: "WAL_FORMAT, WAL_MODE, or WAL_SYNC",
                    got: var,
                });
            }
            if !matches!(parser.peek(), Tok::Eq) {
                return Err(SqlError::UnexpectedToken { expected: "=", got: format!("{:?}", parser.peek()) });
            }
            parser.advance();
            let val = parser.expect_ident()?;
            if var_upper == "WAL_MODE" {
                let logical = match val.to_lowercase().as_str() {
                    "logical"  => true,
                    "physical" => false,
                    other => return Err(SqlError::UnexpectedToken {
                        expected: "physical or logical",
                        got: other.to_string(),
                    }),
                };
                return Ok(CompiledMutation::SetWalMode(logical));
            }
            if var_upper == "WAL_SYNC" {
                use crate::storage::wal::SyncLevel;
                let level = match val.to_lowercase().as_str() {
                    "full"    => SyncLevel::Full,
                    "barrier" => SyncLevel::Barrier,
                    "os"      => SyncLevel::Os,
                    other => return Err(SqlError::UnexpectedToken {
                        expected: "full, barrier, or os",
                        got: other.to_string(),
                    }),
                };
                return Ok(CompiledMutation::SetWalSync(level));
            }
            let format = match val.to_lowercase().as_str() {
                "json"   => crate::WalFormat::Json,
                "binary" => crate::WalFormat::Binary,
                other => return Err(SqlError::UnexpectedToken {
                    expected: "json or binary",
                    got: other.to_string(),
                }),
            };
            Ok(CompiledMutation::SetWalFormat(format))
        }
        Tok::Eof => Err(SqlError::UnexpectedEnd {
            expected: "INSERT, UPDATE, DELETE, CREATE, DROP, ALTER, REINDEX, COMPACT, SET, BEGIN, COMMIT, or ROLLBACK",
        }),
        other => Err(SqlError::UnexpectedToken {
            expected: "INSERT, UPDATE, DELETE, CREATE, DROP, ALTER, REINDEX, COMPACT, SET, BEGIN, COMMIT, or ROLLBACK",
            got: format!("{other:?}"),
        }),
    }
}

/// Parse a mutation SQL statement and compile it to a [`CompiledMutation`].
///
/// # Node INSERT
/// ```text
/// INSERT INTO collection (_key, field, ...) VALUES ('key', val, ...)
/// ```
///
/// # Edge INSERT (Cypher-style)
/// ```text
/// INSERT ('from')-[:KIND {strength: n, key: val}]->('to')
/// ```
///
/// # Node DELETE
/// ```text
/// DELETE FROM collection [WHERE field OP value [AND ...]]
/// DELETE FROM ALL [WHERE ...]
/// ```
///
/// # Edge DELETE (Cypher-style)
/// ```text
/// DELETE ('from')-[:KIND]->('to')
/// ```
///
/// # Errors
/// Returns [`SqlError`] if the SQL is syntactically invalid, or if an INSERT
/// is missing a `_key` field or has mismatched field/value counts.
pub fn parse_mutation(sql: &str) -> Result<CompiledMutation, SqlError> {
    parse_mutation_inner(sql, vec![])
}

/// Parse a mutation SQL statement with parameter bindings (`$1`, `$2`, …).
pub fn parse_mutation_params(sql: &str, params: Vec<Value>) -> Result<CompiledMutation, SqlError> {
    parse_mutation_inner(sql, params)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn step_names(steps: &[Step]) -> Vec<&'static str> {
        steps
            .iter()
            .map(|s| match s {
                Step::One(_) => "One",
                Step::Many(_) => "Many",
                Step::Collection(_) => "Collection",
                Step::All => "All",
                Step::Forward(_) => "Forward",
                Step::Backward(_) => "Backward",
                Step::Hops(_) => "Hops",
                Step::HopsTyped { .. } => "HopsTyped",
                Step::Leaves => "Leaves",
                Step::Roots => "Roots",
                Step::WhereEq(..) => "WhereEq",
                Step::WhereNeq(..) => "WhereNeq",
                Step::WhereGt(..) => "WhereGt",
                Step::WhereLt(..) => "WhereLt",
                Step::WhereGte(..) => "WhereGte",
                Step::WhereLte(..) => "WhereLte",
                Step::WhereBetween(..) => "WhereBetween",
                Step::WhereIn(..) => "WhereIn",
                Step::ArrayContains(..) => "ArrayContains",
                Step::Like(..) => "Like",
                Step::StDWithin(..) => "StDWithin",
                Step::StContainsPoint(..) => "StContainsPoint",
                Step::StWithin(..) => "StWithin",
                Step::StContains(..) => "StContains",
                Step::StIntersects(..) => "StIntersects",
                Step::StDistance(..) => "StDistance",
                Step::StLength(..) => "StLength",
                Step::StPerimeter(..) => "StPerimeter",
                Step::StArea(..) => "StArea",
                Step::VectorNear { .. } => "VectorNear",
                Step::SearchFilter(..) => "SearchFilter",
                Step::Bm25Filter(..) => "Bm25Filter",
                Step::ScoreProject(..) => "ScoreProject",
                Step::Intersect(_) => "Intersect",
                Step::Union(_) => "Union",
                Step::Subtract(_) => "Subtract",
                Step::WhereIsNull(..) => "WhereIsNull",
                Step::WhereNot(_) => "WhereNot",
                Step::WhereOr(_) => "WhereOr",
                Step::GroupBy(_) => "GroupBy",
                Step::Having(_) => "Having",
                Step::Distinct => "Distinct",
                Step::Sort(..) => "Sort",
                Step::SortByVector { .. } => "SortByVector",
                Step::SortByExpr { .. } => "SortByExpr",
                Step::Skip(_) => "Skip",
                Step::Take(_) => "Take",
                Step::Select(_) => "Select",
            })
            .collect()
    }

    #[test]
    fn parse_select_star_from_collection() {
        let steps = parse_and_compile("SELECT * FROM products").unwrap();
        assert_eq!(step_names(&steps), ["Collection"]);
    }

    #[test]
    fn parse_where_eq() {
        let steps = parse_and_compile("SELECT * FROM products WHERE category = 'cat3'").unwrap();
        assert_eq!(step_names(&steps), ["Collection", "WhereEq"]);
    }

    #[test]
    fn parse_where_neq() {
        let steps = parse_and_compile("SELECT * FROM products WHERE category != 'cat0'").unwrap();
        assert_eq!(step_names(&steps), ["Collection", "WhereNeq"]);
    }

    #[test]
    fn parse_where_neq_diamond_operator() {
        let steps = parse_and_compile("SELECT * FROM products WHERE status <> 'inactive'").unwrap();
        assert_eq!(step_names(&steps), ["Collection", "WhereNeq"]);
    }

    #[test]
    fn parse_range_and_order_limit() {
        let steps = parse_and_compile(
            "SELECT name, price FROM products WHERE price > 50 AND price <= 150 ORDER BY price ASC LIMIT 20"
        ).unwrap();
        assert_eq!(
            step_names(&steps),
            [
                "Collection",
                "WhereGt",
                "WhereLte",
                "Sort",
                "Take",
                "Select"
            ]
        );
    }

    #[test]
    fn parse_between() {
        let steps =
            parse_and_compile("SELECT * FROM items WHERE price BETWEEN 10 AND 100").unwrap();
        assert_eq!(step_names(&steps), ["Collection", "WhereBetween"]);
    }

    #[test]
    fn parse_in() {
        let steps =
            parse_and_compile("SELECT * FROM items WHERE status IN ('active', 'pending')").unwrap();
        assert_eq!(step_names(&steps), ["Collection", "WhereIn"]);
    }

    #[test]
    fn parse_like() {
        let steps = parse_and_compile("SELECT * FROM users WHERE name LIKE 'ali'").unwrap();
        assert_eq!(step_names(&steps), ["Collection", "Like"]);
    }

    #[test]
    fn parse_from_all() {
        let steps = parse_and_compile("SELECT * FROM ALL WHERE active = true").unwrap();
        assert_eq!(step_names(&steps), ["All", "WhereEq"]);
    }

    #[test]
    fn parse_offset_limit() {
        let steps = parse_and_compile("SELECT * FROM products LIMIT 10 OFFSET 5").unwrap();
        assert_eq!(step_names(&steps), ["Collection", "Skip", "Take"]);
    }

    #[test]
    fn parse_order_desc() {
        let steps = parse_and_compile("SELECT * FROM products ORDER BY price DESC").unwrap();
        assert_eq!(step_names(&steps), ["Collection", "Sort"]);
        if let Step::Sort(cols) = &steps[1] {
            assert_eq!(cols.len(), 1);
            assert!(!cols[0].1, "DESC means ascending=false");
        } else {
            panic!("expected Sort step");
        }
    }

    #[test]
    fn error_unexpected_token() {
        let err = parse_and_compile("INSERT INTO foo").unwrap_err();
        assert!(matches!(err, SqlError::UnexpectedToken { .. }));
    }

    #[test]
    fn error_unexpected_end() {
        let err = parse_and_compile("SELECT * FROM").unwrap_err();
        assert!(matches!(err, SqlError::UnexpectedEnd { .. }));
    }

    #[test]
    fn roundtrip_where_eq_bool() {
        let steps = parse_and_compile("SELECT * FROM products WHERE in_stock = true").unwrap();
        assert_eq!(step_names(&steps), ["Collection", "WhereEq"]);
        if let Step::WhereEq(_, v) = &steps[1] {
            assert_eq!(v, &serde_json::Value::Bool(true));
        } else {
            panic!();
        }
    }

    #[test]
    fn roundtrip_where_null() {
        let steps = parse_and_compile("SELECT * FROM items WHERE deleted_at = null").unwrap();
        if let Step::WhereEq(_, v) = &steps[1] {
            assert_eq!(v, &serde_json::Value::Null);
        } else {
            panic!();
        }
    }

    // ── INSERT / DELETE ───────────────────────────────────────────────────────

    #[test]
    fn parse_insert_basic() {
        let m = parse_mutation("INSERT INTO users (_key, name, age) VALUES ('alice', 'Alice', 30)")
            .unwrap();
        match m {
            CompiledMutation::Insert { slug, payload_json, .. } => {
                assert_eq!(slug, "users/alice");
                let v: serde_json::Value = serde_json::from_str(&payload_json).unwrap();
                assert_eq!(v["_collection"], "users");
                assert_eq!(v["_id"], "users/alice");
                assert_eq!(v["_key"], "alice");
                assert_eq!(v["name"], "Alice");
                assert_eq!(v["age"].as_f64(), Some(30.0));
            }
            _ => panic!("expected Insert"),
        }
    }

    /// ST_GeomFromGeoJSON('...') in INSERT VALUES must parse the JSON string
    /// into a proper JSON object stored under the geometry field.
    #[test]
    fn parse_insert_st_geomfromgeojson() {
        let sql = r#"INSERT INTO places (_key, name, geometry)
                     VALUES ('mel', 'Melbourne',
                             ST_GeomFromGeoJSON('{"type":"Point","coordinates":[144.9631,-37.8136]}'))"#;
        let m = parse_mutation(sql).unwrap();
        match m {
            CompiledMutation::Insert { payload_json, .. } => {
                let v: serde_json::Value = serde_json::from_str(&payload_json).unwrap();
                assert_eq!(v["geometry"]["type"], "Point");
                assert_eq!(v["geometry"]["coordinates"][0].as_f64().unwrap(), 144.9631);
                assert_eq!(v["geometry"]["coordinates"][1].as_f64().unwrap(), -37.8136);
            }
            _ => panic!("expected Insert"),
        }
    }

    /// ST_GeomFromGeoJSON with a malformed JSON string must return an error.
    #[test]
    fn parse_insert_st_geomfromgeojson_invalid_json() {
        let sql = r#"INSERT INTO places (_key, geometry) VALUES ('p1', ST_GeomFromGeoJSON('{bad}'))"#;
        assert!(parse_mutation(sql).is_err(), "should reject invalid JSON");
    }

    /// ST_GeomFromGeoJSON with valid JSON that is NOT a GeoJSON geometry
    /// (missing "type" field) must return an error.
    #[test]
    fn parse_insert_st_geomfromgeojson_not_geojson() {
        let sql = r#"INSERT INTO places (_key, geometry) VALUES ('p1', ST_GeomFromGeoJSON('{"name":"Fitzroy"}'))"#;
        assert!(parse_mutation(sql).is_err(), "should reject non-geometry JSON");
    }

    /// ST_AsGeoJSON(field) in the SELECT list must compile to a sentinel field
    /// name that the executor can identify and evaluate.
    #[test]
    fn parse_select_st_asgeojson() {
        let steps = parse_and_compile("SELECT ST_AsGeoJSON(geometry) FROM places").unwrap();
        // The Select step must contain the __ST_AsGeoJSON__ sentinel.
        let has_sentinel = steps.iter().any(|s| {
            if let Step::Select(fields) = s {
                fields.iter().any(|f| f.starts_with("__ST_AsGeoJSON__"))
            } else {
                false
            }
        });
        assert!(has_sentinel, "expected __ST_AsGeoJSON__ sentinel in Select step");
    }

    /// ST_AsGeoJSON(field) AS alias must encode both the sentinel and the alias.
    #[test]
    fn parse_select_st_asgeojson_with_alias() {
        let steps =
            parse_and_compile("SELECT ST_AsGeoJSON(geometry) AS geom FROM places").unwrap();
        let has_alias = steps.iter().any(|s| {
            if let Step::Select(fields) = s {
                fields
                    .iter()
                    .any(|f| f.contains("__AS__geom") && f.contains("__ST_AsGeoJSON__"))
            } else {
                false
            }
        });
        assert!(has_alias, "expected aliased __ST_AsGeoJSON__ in Select step");
    }

    #[test]
    fn parse_insert_with_bool_and_null() {
        let m =
            parse_mutation("INSERT INTO items (_key, active, deleted_at) VALUES ('x', true, null)")
                .unwrap();
        match m {
            CompiledMutation::Insert { payload_json, .. } => {
                let v: serde_json::Value = serde_json::from_str(&payload_json).unwrap();
                assert_eq!(v["active"], true);
                assert_eq!(v["deleted_at"], serde_json::Value::Null);
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parse_delete_from_collection() {
        let m = parse_mutation("DELETE FROM users WHERE name = 'alice'").unwrap();
        match m {
            CompiledMutation::Delete(steps) => {
                assert_eq!(step_names(&steps), ["Collection", "WhereEq"])
            }
            _ => panic!("expected Delete"),
        }
    }

    #[test]
    fn parse_delete_from_all() {
        let m = parse_mutation("DELETE FROM ALL").unwrap();
        match m {
            CompiledMutation::Delete(steps) => assert_eq!(step_names(&steps), ["All"]),
            _ => panic!("expected Delete"),
        }
    }

    #[test]
    fn parse_delete_multi_condition() {
        let m =
            parse_mutation("DELETE FROM products WHERE price < 1 AND in_stock = false").unwrap();
        match m {
            CompiledMutation::Delete(steps) => {
                assert_eq!(step_names(&steps), ["Collection", "WhereLt", "WhereEq"])
            }
            _ => panic!("expected Delete"),
        }
    }

    #[test]
    fn insert_missing_key_deferred() {
        // Missing _key is no longer an error at parse time — it compiles to an empty-slug
        // sentinel so execute() can fill it from a schema UUID default.
        let result = parse_mutation("INSERT INTO users (name, age) VALUES ('Alice', 30)");
        match result.unwrap() {
            CompiledMutation::Insert { slug, .. } => {
                assert!(slug.is_empty(), "empty slug signals deferred _key");
            }
            other => panic!("expected Insert, got {other:?}"),
        }
    }

    #[test]
    fn insert_field_value_mismatch_errors() {
        let err = parse_mutation("INSERT INTO users (slug, name) VALUES ('alice')").unwrap_err();
        assert!(matches!(
            err,
            SqlError::FieldValueCountMismatch {
                fields: 2,
                values: 1
            }
        ));
    }

    #[test]
    fn insert_non_string_key_errors() {
        let err =
            parse_mutation("INSERT INTO users (_key, name) VALUES (42, 'Alice')").unwrap_err();
        assert!(matches!(err, SqlError::InvalidValue(_)));
    }

    #[test]
    fn parse_mutation_rejects_select() {
        let err = parse_mutation("SELECT * FROM users").unwrap_err();
        assert!(matches!(err, SqlError::UnexpectedToken { .. }));
    }

    // ── MATCH unit tests ─────────────────────────────────────────────────────

    #[test]
    fn tokenize_match_forward() {
        let tokens = tokenize(
            "SELECT g.* FROM MATCH (a:artist)-[:has_genre]->(g:genre) WHERE a._key = 'the-vines'",
        )
        .unwrap();
        // Should contain: Match LParen Ident Colon Ident RParen Dash LBracket Colon Ident RBracket Arrow ...
        assert!(tokens.iter().any(|t| matches!(t, Tok::Kw(Kw::Match))));
        assert!(tokens.iter().any(|t| matches!(t, Tok::LBracket)));
        assert!(tokens.iter().any(|t| matches!(t, Tok::RBracket)));
        assert!(tokens.iter().any(|t| matches!(t, Tok::Arrow)));
        assert!(tokens.iter().any(|t| matches!(t, Tok::Dot)));
    }

    #[test]
    fn tokenize_backarrow() {
        let tokens = tokenize("<-[:follows]-").unwrap();
        assert_eq!(tokens[0], Tok::BackArrow);
        assert_eq!(tokens[1], Tok::LBracket);
    }

    #[test]
    fn tokenize_skips_comments() {
        // `-- …` line comments and `/* … */` block comments are skipped like whitespace,
        // while a lone `-` (negative number literal) still tokenizes correctly.
        let toks = tokenize(
            "SELECT a /* pick */ FROM t   -- trailing\n WHERE v = -5 /* end */",
        )
        .unwrap();
        assert!(toks.iter().any(|t| matches!(t, Tok::Kw(Kw::Where))));
        // No stray comment text leaked in as identifiers.
        assert!(!toks.iter().any(|t| matches!(t, Tok::Ident(s) if s == "pick" || s == "trailing" || s == "end")));
    }

    #[test]
    fn tokenize_dotdot() {
        let tokens = tokenize("*1..5").unwrap();
        assert_eq!(tokens[0], Tok::Star);
        assert_eq!(tokens[1], Tok::Num(1.0));
        assert_eq!(tokens[2], Tok::DotDot);
        assert_eq!(tokens[3], Tok::Num(5.0));
    }


    #[test]
    fn parse_ilike() {
        let steps = parse_and_compile("SELECT * FROM artist WHERE name ILIKE 'vines'").unwrap();
        assert_eq!(step_names(&steps), ["Collection", "Like"]);
        if let Step::Like(_, _, case_insensitive) = &steps[1] {
            assert!(*case_insensitive, "ILIKE should set case_insensitive=true");
        } else {
            panic!("expected Like step");
        }
    }


    // ── Spatial SQL tests ────────────────────────────────────────────────────

    #[test]
    fn parse_st_dwithin() {
        // Spatial-first optimisation: grid starter + collection gate, no Collection step
        let steps = parse_and_compile(
            "SELECT * FROM places WHERE ST_DWithin(geometry, POINT(144.9631 -37.8136), 5.0)",
        )
        .unwrap();
        assert_eq!(step_names(&steps), ["StDWithin", "WhereEq"]);
    }

    // Containment predicates (point-in-polygon, polygon–polygon) do NOT use the grid
    // starter: they compile Collection-first and run as an exact filter over cached
    // polygon rings. On a small polygon collection that beats a coarse-global-grid
    // candidate scan (see is_grid_starter_cond). So the plan is [Collection, St*].
    #[test]
    fn parse_st_contains_point() {
        let steps = parse_and_compile(
            "SELECT * FROM zones WHERE ST_Contains(geometry, POINT(144.9631 -37.8136))",
        )
        .unwrap();
        assert_eq!(step_names(&steps), ["Collection", "StContainsPoint"]);
    }

    #[test]
    fn parse_st_within_polygon() {
        let steps = parse_and_compile(
            "SELECT * FROM places WHERE ST_Within(geometry, POLYGON((144.95 -37.80, 144.98 -37.80, 144.98 -37.83, 144.95 -37.83, 144.95 -37.80)))"
        ).unwrap();
        assert_eq!(step_names(&steps), ["Collection", "StWithin"]);
    }

    #[test]
    fn parse_st_intersects_polygon() {
        let steps = parse_and_compile(
            "SELECT * FROM routes WHERE ST_Intersects(geometry, POLYGON((144.95 -37.80, 144.98 -37.80, 144.98 -37.83, 144.95 -37.83, 144.95 -37.80)))"
        ).unwrap();
        assert_eq!(step_names(&steps), ["Collection", "StIntersects"]);
    }

    #[test]
    fn parse_st_contains_polygon() {
        let steps = parse_and_compile(
            "SELECT * FROM zones WHERE ST_Contains(geometry, POLYGON((144.96 -37.81, 144.97 -37.81, 144.97 -37.82, 144.96 -37.82, 144.96 -37.81)))"
        ).unwrap();
        assert_eq!(step_names(&steps), ["Collection", "StContains"]);
    }

    #[test]
    fn parse_st_dwithin_with_other_filter() {
        // Spatial step first (starter), then collection gate, then non-spatial filter
        let steps = parse_and_compile(
            "SELECT * FROM places WHERE ST_DWithin(geometry, POINT(144.9631 -37.8136), 5.0) AND category = 'landmark'"
        ).unwrap();
        assert_eq!(step_names(&steps), ["StDWithin", "WhereEq", "WhereEq"]);
    }

    #[test]
    fn parse_bm25_filter() {
        let steps =
            parse_and_compile("SELECT * FROM articles WHERE BM25(body, 'rust tutorial') > 0.5")
                .unwrap();
        assert_eq!(step_names(&steps), ["Collection", "Bm25Filter"]);
    }

    #[test]
    fn parse_bm25_order_by() {
        let steps =
            parse_and_compile("SELECT * FROM articles ORDER BY BM25(body, 'rust tutorial') DESC")
                .unwrap();
        // Simple BM25 now compiles to SortByExpr (arithmetic expression path).
        assert_eq!(step_names(&steps), ["Collection", "SortByExpr"]);
    }

    #[test]
    fn parse_bm25_order_by_asc() {
        let steps =
            parse_and_compile("SELECT * FROM articles ORDER BY BM25(body, 'rust tutorial') ASC")
                .unwrap();
        assert_eq!(step_names(&steps), ["Collection", "SortByExpr"]);
    }

    #[test]
    fn parse_bm25_select_score() {
        let steps =
            parse_and_compile("SELECT title, BM25(body, 'rust tutorial') FROM articles").unwrap();
        assert_eq!(step_names(&steps), ["Collection", "ScoreProject", "Select"]);

        // With AS alias
        let steps =
            parse_and_compile("SELECT title, BM25(body, 'rust tutorial') AS score FROM articles").unwrap();
        assert_eq!(step_names(&steps), ["Collection", "ScoreProject", "Select"]);

        // SELECT * with score projection
        let steps =
            parse_and_compile("SELECT *, BM25(body, 'rust') AS relevance FROM articles").unwrap();
        assert_eq!(step_names(&steps), ["Collection", "ScoreProject"]);
    }

    #[test]
    fn parse_bm25_filter_and_order() {
        let steps = parse_and_compile(
            "SELECT * FROM articles WHERE category = 'tech' ORDER BY BM25(body, 'rust') DESC LIMIT 10",
        )
        .unwrap();
        assert_eq!(
            step_names(&steps),
            ["Collection", "WhereEq", "SortByExpr", "Take"]
        );
    }

    #[test]
    fn insert_json_string_auto_parsed() {
        let m = parse_mutation(
            r#"INSERT INTO places (_key, geometry) VALUES ('p1', '{"type":"Point","coordinates":[144.96,-37.81]}')"#
        ).unwrap();
        match m {
            CompiledMutation::Insert { payload_json, .. } => {
                let v: Value = serde_json::from_str(&payload_json).unwrap();
                assert!(
                    v["geometry"].is_object(),
                    "geometry should be parsed object"
                );
                assert_eq!(v["geometry"]["type"], "Point");
            }
            other => panic!("expected Insert, got {other:?}"),
        }
    }

    // ── Multi-row INSERT parse tests ────────────────────────────────────────

    #[test]
    fn parse_insert_multi_row() {
        let m = parse_mutation(
            "INSERT INTO users (_key, name, age) VALUES ('a', 'Alice', 30), ('b', 'Bob', 25), ('c', 'Carol', 28)"
        ).unwrap();
        match m {
            CompiledMutation::InsertBatch { collection, items } => {
                assert_eq!(collection, "users");
                assert_eq!(items.len(), 3);
                // First row
                assert_eq!(items[0].0, "users/a"); // slug
                let v: Value = serde_json::from_str(&items[0].1).unwrap();
                assert_eq!(v["name"], "Alice");
                assert_eq!(v["age"].as_f64(), Some(30.0));
                // Second row
                assert_eq!(items[1].0, "users/b");
                let v: Value = serde_json::from_str(&items[1].1).unwrap();
                assert_eq!(v["name"], "Bob");
                // Third row
                assert_eq!(items[2].0, "users/c");
                let v: Value = serde_json::from_str(&items[2].1).unwrap();
                assert_eq!(v["name"], "Carol");
            }
            other => panic!("expected InsertBatch, got {other:?}"),
        }
    }

    #[test]
    fn parse_insert_single_row_stays_insert() {
        // Single-row INSERT should produce Insert, not InsertBatch
        let m = parse_mutation(
            "INSERT INTO users (_key, name) VALUES ('a', 'Alice')"
        ).unwrap();
        assert!(matches!(m, CompiledMutation::Insert { .. }));
    }

    #[test]
    fn parse_insert_multi_row_with_params() {
        let m = parse_mutation_params(
            "INSERT INTO t (_key, val) VALUES ($1, $2), ($3, $4)",
            vec![
                Value::String("k1".into()), serde_json::json!(10),
                Value::String("k2".into()), serde_json::json!(20),
            ],
        ).unwrap();
        match m {
            CompiledMutation::InsertBatch { items, .. } => {
                assert_eq!(items.len(), 2);
                let v0: Value = serde_json::from_str(&items[0].1).unwrap();
                assert_eq!(v0["val"].as_f64(), Some(10.0));
                let v1: Value = serde_json::from_str(&items[1].1).unwrap();
                assert_eq!(v1["val"].as_f64(), Some(20.0));
            }
            other => panic!("expected InsertBatch, got {other:?}"),
        }
    }

    #[test]
    fn parse_insert_multi_row_field_count_mismatch() {
        // Second tuple has wrong number of values
        let result = parse_mutation(
            "INSERT INTO t (_key, a, b) VALUES ('k1', 1, 2), ('k2', 3)"
        );
        assert!(result.is_err());
    }

    // ── INSERT edge parse tests ─────────────────────────────────────────────

    #[test]
    fn insert_edge_simple() {
        let m = parse_mutation("INSERT ('a')-[:KNOWS]->('b')").unwrap();
        match m {
            CompiledMutation::InsertEdge(edges) => {
                assert_eq!(edges.len(), 1);
                assert_eq!(edges[0].from, "a");
                assert_eq!(edges[0].to, "b");
                assert_eq!(edges[0].edge_type, "KNOWS");
                // A naked edge carries no props.
                assert!(edges[0].props_json.is_none());
            }
            other => panic!("expected InsertEdge, got {other:?}"),
        }
    }

    #[test]
    fn insert_edge_strength_is_an_ordinary_prop() {
        // `strength` is no longer privileged — it rides through as a normal
        // attribute like any other primitive.
        let m = parse_mutation("INSERT ('a')-[:KNOWS {strength: 10}]->('b')").unwrap();
        match m {
            CompiledMutation::InsertEdge(edges) => {
                let props: Value =
                    serde_json::from_str(edges[0].props_json.as_ref().unwrap()).unwrap();
                assert_eq!(props["strength"], 10.0);
            }
            other => panic!("expected InsertEdge, got {other:?}"),
        }
    }

    #[test]
    fn insert_edge_with_props() {
        let m = parse_mutation("INSERT ('a')-[:KNOWS {strength: 5, since: 2024}]->('b')").unwrap();
        match m {
            CompiledMutation::InsertEdge(edges) => {
                let props: Value =
                    serde_json::from_str(edges[0].props_json.as_ref().unwrap()).unwrap();
                assert_eq!(props["since"], 2024.0);
                // strength stays in props now — nothing is extracted.
                assert_eq!(props["strength"], 5.0);
            }
            other => panic!("expected InsertEdge, got {other:?}"),
        }
    }

    #[test]
    fn insert_edge_multiple() {
        let m = parse_mutation("INSERT ('a')-[:X]->('b'), ('b')-[:Y]->('c')").unwrap();
        match m {
            CompiledMutation::InsertEdge(edges) => {
                assert_eq!(edges.len(), 2);
                assert_eq!(edges[0].from, "a");
                assert_eq!(edges[0].edge_type, "X");
                assert_eq!(edges[0].to, "b");
                assert_eq!(edges[1].from, "b");
                assert_eq!(edges[1].edge_type, "Y");
                assert_eq!(edges[1].to, "c");
            }
            other => panic!("expected InsertEdge, got {other:?}"),
        }
    }

    #[test]
    fn insert_edge_default_is_naked() {
        let m = parse_mutation("INSERT ('x')-[:links]->('y')").unwrap();
        match m {
            CompiledMutation::InsertEdge(edges) => {
                // No props at all — the default edge is zero.
                assert!(edges[0].props_json.is_none());
            }
            other => panic!("expected InsertEdge, got {other:?}"),
        }
    }

    // ── DELETE edge parse tests ─────────────────────────────────────────────

    #[test]
    fn delete_edge_parses() {
        let m = parse_mutation("DELETE ('a')-[:KNOWS]->('b')").unwrap();
        match m {
            CompiledMutation::DeleteEdge(edges) => {
                assert_eq!(edges.len(), 1);
                assert_eq!(edges[0].from, "a");
                assert_eq!(edges[0].to, "b");
                assert_eq!(edges[0].edge_type, "KNOWS");
            }
            other => panic!("expected DeleteEdge, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod debug_tests {
    use super::*;

    #[test]
    fn debug_tokenize_match_insert() {
        let sql = "MATCH (p:people) WHERE p.grade < 80 INSERT (p)-[:member_of]->(classroom/A)";
        let tokens = tokenize(sql).unwrap();
        assert!(!tokens.is_empty());

        let result = parse_mutation(sql).expect("MATCH INSERT should parse successfully");
        match result {
            CompiledMutation::MatchInsert {
                match_steps,
                target,
                edge_type,
                props,
            } => {
                assert_eq!(target, "classroom/A");
                assert_eq!(edge_type, "member_of");
                assert!(props.is_none());
                // Should have Collection step + WhereLt step
                assert_eq!(match_steps.len(), 2);
            }
            other => panic!("expected MatchInsert, got {other:?}"),
        }
    }

    #[test]
    fn match_insert_with_edge_props() {
        let sql = r#"MATCH (p:people) WHERE p.grade < 80 INSERT (p)-[:member_of {strength: 0.8, semester: "fall"}]->(classroom/A)"#;
        let result = parse_mutation(sql).expect("MATCH INSERT with edge props should parse");
        match result {
            CompiledMutation::MatchInsert {
                target,
                edge_type,
                props,
                ..
            } => {
                assert_eq!(target, "classroom/A");
                assert_eq!(edge_type, "member_of");
                assert!(props.is_some());
                let p: serde_json::Value = serde_json::from_str(props.as_ref().unwrap()).unwrap();
                assert_eq!(p["semester"], "fall");
                // strength is just another prop now.
                assert_eq!(p["strength"], 0.8);
            }
            other => panic!("expected MatchInsert, got {other:?}"),
        }
    }

    #[test]
    fn match_insert_without_where() {
        let sql = "MATCH (p:people) INSERT (p)-[:member_of]->(classroom/A)";
        let result = parse_mutation(sql).expect("MATCH INSERT without WHERE should parse");
        match result {
            CompiledMutation::MatchInsert {
                match_steps,
                target,
                edge_type,
                ..
            } => {
                assert_eq!(target, "classroom/A");
                assert_eq!(edge_type, "member_of");
                // Only Collection step, no WHERE
                assert_eq!(match_steps.len(), 1);
            }
            other => panic!("expected MatchInsert, got {other:?}"),
        }
    }

    #[test]
    fn match_insert_with_label_target() {
        let sql = "MATCH (p:people) INSERT (p)-[:member_of]->(:group {_key: 'alpha'})";
        let result = parse_mutation(sql).expect("MATCH INSERT with label target should parse");
        match result {
            CompiledMutation::MatchInsert {
                target, edge_type, ..
            } => {
                assert_eq!(target, "group/alpha");
                assert_eq!(edge_type, "member_of");
            }
            other => panic!("expected MatchInsert, got {other:?}"),
        }
    }

    #[test]
    fn match_insert_multiple_where() {
        let sql = "MATCH (p:people) WHERE p.grade >= 50 AND p.grade < 80 INSERT (p)-[:member_of]->(classroom/A)";
        let result = parse_mutation(sql).expect("MATCH INSERT with multiple WHERE should parse");
        match result {
            CompiledMutation::MatchInsert {
                match_steps,
                target,
                ..
            } => {
                assert_eq!(target, "classroom/A");
                // Collection + WhereGte + WhereLt
                assert_eq!(match_steps.len(), 3);
            }
            other => panic!("expected MatchInsert, got {other:?}"),
        }
    }
}

// ── SHOW ──────────────────────────────────────────────────────────────────────

/// Parsed form of a `SHOW EDGES` clause.
pub struct ShowEdgesStmt {
    /// `SHOW EDGES FROM <collection>` — filter source collection.
    pub from_col: Option<String>,
    /// `SHOW EDGES FROM x TO <collection>` — also filter target collection.
    pub to_col: Option<String>,
}

/// Parsed form of any `SHOW …` statement.
pub enum ShowStmt {
    /// `SHOW TABLES` — all collections with row counts.
    Tables,
    /// `SHOW EDGES [FROM col] [TO col]` — graph schema with edge counts.
    Edges(ShowEdgesStmt),
    /// `SHOW <collection>` — field structure for one collection.
    Collection(String),
    /// `SHOW CREATE TABLE <collection>` — DDL that would recreate this collection.
    CreateTable(String),
}

/// Parse any `SHOW` statement:
///
/// ```text
/// SHOW TABLES
/// SHOW EDGES [FROM collection] [TO collection]
/// SHOW CREATE TABLE <collection>
/// SHOW <collection_name>
/// ```
pub fn parse_show(sql: &str) -> Result<ShowStmt, SqlError> {
    let tokens = tokenize(sql)?;
    let mut p = Parser::new(tokens);

    p.expect_kw(Kw::Show, "SHOW")?;

    match p.peek().clone() {
        // SHOW TABLES (TABLE alone = list of tables)
        Tok::Ident(s) if s.to_ascii_uppercase() == "TABLES" => {
            p.advance();
            Ok(ShowStmt::Tables)
        }
        // SHOW CREATE TABLE <collection>  — TABLE keyword triggers the branch
        Tok::Kw(Kw::Table) => {
            p.advance();
            // If next token is a collection name it's a bare SHOW TABLE (= SHOW TABLES)
            // If preceded by CREATE it's SHOW CREATE TABLE — but CREATE is consumed before
            // we get here, so we check: is the next token an ident?
            match p.peek().clone() {
                Tok::Ident(_) => {
                    let name = p.expect_ident()?;
                    Ok(ShowStmt::CreateTable(name))
                }
                _ => Ok(ShowStmt::Tables),
            }
        }
        // SHOW CREATE TABLE <collection>
        Tok::Kw(Kw::Create) => {
            p.advance();
            // expect TABLE
            match p.peek().clone() {
                Tok::Kw(Kw::Table) => { p.advance(); }
                other => return Err(SqlError::UnexpectedToken {
                    expected: "TABLE",
                    got: format!("{other:?}"),
                }),
            }
            let name = p.expect_ident()?;
            Ok(ShowStmt::CreateTable(name))
        }
        // SHOW EDGES [FROM col] [TO col]
        Tok::Ident(s) if s.to_ascii_uppercase() == "EDGES" => {
            p.advance();

            let from_col = if matches!(p.peek(), Tok::Kw(Kw::From)) {
                p.advance();
                Some(p.expect_ident()?)
            } else {
                None
            };

            let to_col = match p.peek().clone() {
                Tok::Kw(Kw::To) => {
                    p.advance();
                    Some(p.expect_ident()?)
                }
                _ => None,
            };

            Ok(ShowStmt::Edges(ShowEdgesStmt { from_col, to_col }))
        }
        // SHOW <collection> — field structure
        Tok::Ident(_) => {
            let name = p.expect_ident()?;
            Ok(ShowStmt::Collection(name))
        }
        other => Err(SqlError::UnexpectedToken {
            expected: "TABLES, CREATE TABLE, EDGES, or a collection name",
            got: format!("{other:?}"),
        }),
    }
}

