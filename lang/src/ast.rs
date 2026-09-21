//! The statement, as written. Nothing here knows about indexes, entity ids or
//! drivers: the AST is the shape of the text, and `compile.rs` is the only
//! place that turns a shape into a `QueryRequest` or a write.

use super::functions::TimeUnit;
use sekejap_core::collections::ColumnRule;
use sekejap_core::Kind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    pub(super) fn written(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Ne => "<>",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        }
    }
}

/// A value a statement can write where a constant is expected.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Literal {
    Null,
    Bool(bool),
    /// A number and whether it was written without a fraction or an exponent.
    Num(f64, bool),
    Str(String),
    Param(usize),
    /// `(SELECT col FROM t WHERE _key = <literal>)` -- a scalar subquery,
    /// which is a constant by the time the outer statement runs.
    Subquery(Box<ScalarSubquery>),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ScalarSubquery {
    pub(super) column: String,
    pub(super) table: String,
    pub(super) key: Literal,
}

/// The vector-distance operators pgvector spells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum VecOp {
    Cosine,
    L2,
    NegativeDot,
}

/// A point, written the way PostGIS constructs one.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct PointArg {
    pub(super) lon: Literal,
    pub(super) lat: Literal,
}

/// A geometry argument to a spatial predicate.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum GeoArg {
    Point(PointArg),
    /// `ST_MakeEnvelope(minlon, minlat, maxlon, maxlat, 4326)`.
    Envelope {
        minlon: Literal,
        minlat: Literal,
        maxlon: Literal,
        maxlat: Literal,
    },
    /// A GeoJSON document, as a literal or through `ST_GeomFromGeoJSON`.
    GeoJson(Literal),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SpatialPredicate {
    DWithin,
    Intersects,
    Within,
    Contains,
}

/// The argument of `to_tsquery('simple', ...)` or of `bm25(col, ...)`.
///
/// Which of `TextMatch::Any`, `All` or `Phrase` it means depends on whether
/// the text holds `|`, `&` or a quoted phrase, and the text can arrive as a
/// parameter, so the reading happens in `compile.rs` rather than here.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct TsQuery {
    pub(super) source: Literal,
    /// True when the statement wrote `to_tsquery`, false for `bm25`, which
    /// takes the words as written with no operator syntax.
    pub(super) tsquery_syntax: bool,
}

/// The right-hand side of a date/time comparison: a written literal, or the
/// clock plus a folded interval.
///
/// `now()` and `current_date` are constants folded ONCE at prepare
/// (`docs/lang/QL_CONTRACT.md` §4.2), so `t > now() - interval '7 days'` is one
/// integer by the time the walk starts and the predicate is the ordinary
/// scalar Range over it.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum TimeValue {
    Lit(Literal),
    Clock {
        /// `current_date` truncates the clock to midnight UTC; `now()` does
        /// not.
        date_only: bool,
        /// The interval written beside it, in microseconds. Negative for a
        /// subtraction.
        offset: i64,
    },
}

/// A `WHERE` form over a declared TIMESTAMPTZ/DATE column that rewrites to
/// scalar index RANGES on that column's own btree.
///
/// Every shape here is index-side by construction: the function is folded
/// into bounds at prepare and the walk never evaluates it. A shape whose
/// pre-image is a SET of ranges rather than one (`EXTRACT(MONTH FROM t) = 6`
/// is one interval per year in the corpus) is a membership-set union, which
/// is what `OR` compiles to, and is refused while that union is unbuilt.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum TimeShape {
    Extract {
        unit: TimeUnit,
        op: CmpOp,
        value: Literal,
    },
    ExtractBetween {
        unit: TimeUnit,
        lower: Literal,
        upper: Literal,
    },
    Trunc {
        unit: TimeUnit,
        op: CmpOp,
        value: Literal,
    },
    TruncBetween {
        unit: TimeUnit,
        lower: Literal,
        upper: Literal,
    },
    /// `t::date <cmp> 'lit'`.
    CastDate {
        op: CmpOp,
        value: Literal,
    },
    /// `t <cmp> now() - interval '7 days'`.
    Clock {
        op: CmpOp,
        value: TimeValue,
    },
    ClockBetween {
        lower: TimeValue,
        upper: TimeValue,
    },
}

/// A `WHERE` form over a TEXT column that rewrites to a text-key range.
///
/// `Lower*` needs the expression index `CREATE INDEX ... ON t (lower(col))`;
/// without it the form is REFUSED with that reason and never demoted to a
/// scan (`docs/lang/QL_CONTRACT.md` §4.1).
#[derive(Clone, Debug, PartialEq)]
pub(super) enum TextShape {
    LowerEq {
        value: Literal,
    },
    LowerPrefix {
        value: Literal,
        /// The spelling the statement used, for the EXPLAIN line.
        written: &'static str,
    },
    Prefix {
        value: Literal,
        written: &'static str,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Predicate {
    Compare {
        column: String,
        op: CmpOp,
        value: Literal,
    },
    Between {
        column: String,
        lower: Literal,
        upper: Literal,
    },
    IsNull {
        column: String,
        negated: bool,
    },
    IsMissing {
        column: String,
    },
    /// A range over the external-key mapping keyspace, written on `_key`.
    KeyCompare {
        op: CmpOp,
        value: Literal,
    },
    KeyBetween {
        lower: Literal,
        upper: Literal,
    },
    Text {
        column: String,
        query: TsQuery,
    },
    Spatial {
        predicate: SpatialPredicate,
        column: String,
        argument: GeoArg,
        /// Metres, for `ST_DWithin` only.
        metres: Option<Literal>,
    },
    /// `col IN (v1, v2, ...)`: a union of equalities on one index, which is
    /// one membership set (`docs/lang/QL_CONTRACT.md` §3).
    InList {
        column: String,
        values: Vec<Literal>,
    },
    /// `_key IN (v1, v2, ...)` over the external-key mapping keyspace.
    KeyInList {
        values: Vec<Literal>,
    },
    /// `EXISTS (SELECT 1 FROM t2 WHERE t2.<column> = <this table>._key)`, and
    /// the same question written `_key IN (SELECT t2.<column> FROM t2)`: a
    /// SEMI-JOIN, compiled to the membership set of the outer rows `t2`
    /// names.
    Semi {
        table: String,
        column: String,
    },
    /// A §4.2 date/time function folded into scalar ranges over `column`.
    Time { column: String, shape: TimeShape },
    /// A §4.1 string function folded into a text-key range over `column`.
    TextFn { column: String, shape: TextShape },
}

/// A `WHERE` clause, as written: the boolean tree over predicates.
///
/// `AND` is the conjunction a filter list already is, so the top level of a
/// statement is a `Vec<Expr>` and nested `And`s appear only where the shape
/// could not be flattened -- inside an `Or`, or under a `Not`.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Expr {
    Leaf(Predicate),
    Not(Box<Expr>),
    And(Vec<Expr>),
    Or(Vec<Expr>),
}

/// The §4.1 string functions that are ROW functions: one row in, one value
/// out, no read of any other row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StrFunc {
    Lower,
    Upper,
    Length,
    Concat,
    Substring,
    Left,
    Right,
    Trim,
    SplitPart,
    Replace,
    Position,
    StartsWith,
}

impl StrFunc {
    pub(crate) fn written(self) -> &'static str {
        match self {
            Self::Lower => "lower",
            Self::Upper => "upper",
            Self::Length => "length",
            Self::Concat => "concat",
            Self::Substring => "substring",
            Self::Left => "left",
            Self::Right => "right",
            Self::Trim => "trim",
            Self::SplitPart => "split_part",
            Self::Replace => "replace",
            Self::Position => "position",
            Self::StartsWith => "starts_with",
        }
    }
}

/// A ROW expression: one row in, one value out, evaluated over PROJECTED
/// values after the index-side stage.
///
/// `docs/lang/QL_CONTRACT.md` §4.1 and §4.2. Cost is proportional to the rows
/// RETURNED, never to the collection, and `EXPLAIN` prints the expression
/// under "row functions" so that cost is stated rather than inferred.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum RowExpr {
    /// A declared field of this row.
    Column(String),
    Lit(Literal),
    /// `now()` -- folded once at prepare, so every row of one statement sees
    /// the same instant.
    Now,
    /// `current_date` -- `now()` truncated to midnight UTC.
    CurrentDate,
    /// `interval '7 days'`, already folded to microseconds.
    Interval(i64),
    Extract {
        unit: TimeUnit,
        arg: Box<RowExpr>,
    },
    Trunc {
        unit: TimeUnit,
        arg: Box<RowExpr>,
    },
    /// `age(t)` is `now() - t`; `age(a, b)` is `a - b`. Both in
    /// microseconds, which is what §4.2 calls row arithmetic over Int
    /// microseconds.
    Age {
        left: Box<RowExpr>,
        right: Option<Box<RowExpr>>,
    },
    ToChar {
        arg: Box<RowExpr>,
        format: String,
    },
    /// `to_timestamp(seconds)`.
    ToTimestamp(Box<RowExpr>),
    /// `to_date('lit', fmt)` / `'lit'::date`.
    ToDate(Box<RowExpr>),
    /// `t::date`: the instant truncated to midnight UTC.
    CastDate(Box<RowExpr>),
    /// `x::text`.
    CastText(Box<RowExpr>),
    Str {
        func: StrFunc,
        args: Vec<RowExpr>,
    },
    Add(Box<RowExpr>, Box<RowExpr>),
    Sub(Box<RowExpr>, Box<RowExpr>),
    /// `a || b`, which is `concat` written as an operator.
    Concat(Box<RowExpr>, Box<RowExpr>),
}

/// An arithmetic `ORDER BY` expression: one key, per deviation 3.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum ScoreNode {
    Lit(f64),
    /// A named scalar column, read per candidate from its index.
    Column(String),
    Bm25 {
        column: String,
        query: TsQuery,
    },
    /// `col <=> $v` and friends: the DISTANCE, which lowers to the negation
    /// of `ScoreExpr::VectorSimilarity`.
    VecDistance {
        column: String,
        query: Literal,
        op: VecOp,
    },
    /// `ST_Distance(col, point)`, in metres.
    Distance {
        column: String,
        point: PointArg,
    },
    Add(Box<ScoreNode>, Box<ScoreNode>),
    Sub(Box<ScoreNode>, Box<ScoreNode>),
    Mul(Box<ScoreNode>, Box<ScoreNode>),
    Div(Box<ScoreNode>, Box<ScoreNode>),
    Neg(Box<ScoreNode>),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum OrderKey {
    /// `col ASC|DESC` over an indexed scalar.
    Column {
        column: String,
        descending: bool,
    },
    /// `col <-> point`.
    Distance {
        column: String,
        point: PointArg,
        descending: bool,
    },
    Vector {
        column: String,
        query: Literal,
        op: VecOp,
        descending: bool,
    },
    /// `ts_rank_cd(...)` or `bm25(col, q)` alone.
    Bm25 {
        column: String,
        query: TsQuery,
        descending: bool,
    },
    /// Any other arithmetic expression: one key, the Score atomic.
    Score {
        expr: ScoreNode,
        descending: bool,
    },
}

/// The aggregate functions `docs/lang/QL_CONTRACT.md` §4.7 accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AggFunc {
    Count,
    Sum,
    Min,
    Max,
    Avg,
}

impl AggFunc {
    pub(super) fn written(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Sum => "sum",
            Self::Min => "min",
            Self::Max => "max",
            Self::Avg => "avg",
        }
    }
}

/// `count(*)` against `count(col)`: the star is the only argument that names
/// no column, and only `count` accepts it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum AggArg {
    Star,
    Column(String),
}

/// `GROUP BY col` or `GROUP BY col / n` -- the one grouping EXPRESSION this
/// slice accepts, because it is computable index-side from an Int posting and
/// monotone in that index's own order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct GroupExpr {
    pub(super) column: String,
    pub(super) divisor: Option<i64>,
}

/// `HAVING <agg>(<arg>) <cmp> <value>`: a predicate on a finished group.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct HavingPredicate {
    pub(super) function: AggFunc,
    pub(super) argument: AggArg,
    pub(super) op: CmpOp,
    pub(super) value: Literal,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum SelectItem {
    Star,
    /// The E4 row identity. `Database::put` maps an external key onto it and
    /// the key itself lives in the row, so asking for the id is the one
    /// projection that reads no row.
    Id,
    /// The external key, which lives in the row like any other field.
    Key,
    Column(String),
    /// The ranking value of this statement's own `ORDER BY`, under an alias.
    OrderValue(String),
    /// `count(*)`, `count(col)`, `sum(col)`, `min(col)`, `max(col)`,
    /// `avg(col)`.
    Aggregate {
        function: AggFunc,
        argument: AggArg,
    },
    /// A §4.1 / §4.2 ROW function over this row's own projected values.
    Function(Box<RowExpr>),
    /// `col / n` in a select list: the one grouping expression, written
    /// again where the answer reports it. Outside a folded answer it is an
    /// arithmetic expression like any other and reports the ranking value,
    /// which is what this select list has always done with an expression.
    Divided {
        column: String,
        divisor: i64,
    },
}

/// One comparison written inside an edge element's inline `WHERE`, over a
/// property of the edge's own inline bag (`GRAPH_CONTRACT` 4.3).
#[derive(Clone, Debug, PartialEq)]
pub(super) struct EdgePredicate {
    pub(super) property: String,
    pub(super) op: CmpOp,
    pub(super) value: Literal,
}

/// One element of a `GRAPH_TABLE` pattern's path.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct GraphHop {
    pub(super) edge_type: Option<String>,
    /// `Outgoing` for `-[..]->`, `Incoming` for `<-[..]-`, `Both` for `-[..]-`.
    pub(super) direction: GraphDirection,
    pub(super) min_depth: usize,
    pub(super) max_depth: usize,
    /// The edge element's inline `WHERE`, a conjunction over the edge's own
    /// properties. Applied per hop, not to completed matches.
    pub(super) predicates: Vec<EdgePredicate>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GraphDirection {
    Outgoing,
    Incoming,
    Both,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct GraphTable {
    pub(super) context: String,
    /// The seeding element: its collection and the key equality that names
    /// one row. Only a key equality seeds in this slice.
    pub(super) seed_collection: String,
    pub(super) seed_key: Literal,
    pub(super) hop: GraphHop,
    /// The far element's collection, which is the collection the outer
    /// statement selects from.
    pub(super) target_collection: String,
    /// The far element's inline `WHERE`, a conjunction over the NODE's own
    /// fields. Applied per hop: a node it refuses is neither returned nor
    /// expanded.
    pub(super) node_predicates: Vec<Predicate>,
    /// `COLUMNS (b.<field> AS <alias>)` and `COLUMNS (r.<property> AS
    /// <alias>)`.
    pub(super) columns: Vec<(GraphColumn, String)>,
}

/// One entry of a `COLUMNS (...)` list: a field of the far NODE, or a
/// property of the EDGE the pattern bound.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum GraphColumn {
    Node(SelectItem),
    Edge(String),
}

/// One `SET column = ...` value: a constant, or a row expression over the
/// same row (QL_CONTRACT §4.1 / §4.2).
#[derive(Clone, Debug, PartialEq)]
pub(super) enum SetValue {
    Lit(Literal),
    Row(RowExpr),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Source {
    Table(String),
    Graph(Box<GraphTable>),
    /// `FROM ALL`: every collection of the catalog at once (QL_CONTRACT §2).
    All,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct SelectStmt {
    pub(super) items: Vec<(SelectItem, Option<String>)>,
    pub(super) source: Source,
    pub(super) predicates: Vec<Expr>,
    /// `SELECT DISTINCT`, which is a group with no accumulators.
    pub(super) distinct: bool,
    pub(super) group: Option<GroupExpr>,
    pub(super) having: Vec<HavingPredicate>,
    pub(super) order: Option<OrderKey>,
    pub(super) limit: Option<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ColumnDef {
    pub(super) name: String,
    pub(super) kind: Kind,
    /// The declared SQL type, kept because TIMESTAMPTZ and DATE are both
    /// stored as `Kind::Int` and only the declaration says which.
    pub(super) declared: String,
    pub(super) primary_key: bool,
    /// `DEFAULT <generator>` and `NOT NULL`, as the per-field COLUMN RULE
    /// the collection descriptor records (QL_CONTRACT §2). `None` when the
    /// column clauses set neither.
    pub(super) rule: Option<ColumnRule>,
}

/// The one change an `ALTER TABLE` statement makes. Each is `alter_collection`
/// underneath: a new immutable `Layout` and a repointed catalog in one commit,
/// O(fields) and no row rewritten (QL_CONTRACT §2).
#[derive(Clone, Debug, PartialEq)]
pub(super) enum AlterAction {
    AddColumn(Box<ColumnDef>),
    DropColumn {
        column: String,
        if_exists: bool,
    },
    RenameColumn {
        from: String,
        to: String,
    },
    RenameTable {
        to: String,
    },
    /// `ALTER COLUMN c TYPE new_type`: Tier 2 within one `Kind`, Tier 3
    /// across `Kind`s. The declared spelling is rewritten and no row byte
    /// changes.
    ColumnType {
        column: String,
        kind: Kind,
        declared: String,
    },
}

impl AlterAction {
    /// The clause as a statement would have written it, for an EXPLAIN line.
    pub(super) fn written(&self) -> String {
        match self {
            Self::AddColumn(column) => {
                format!("ADD COLUMN {} {}", column.name, column.declared)
            }
            Self::DropColumn { column, if_exists } => format!(
                "DROP COLUMN {}{column}",
                if *if_exists { "IF EXISTS " } else { "" }
            ),
            Self::RenameColumn { from, to } => format!("RENAME COLUMN {from} TO {to}"),
            Self::RenameTable { to } => format!("RENAME TO {to}"),
            Self::ColumnType {
                column, declared, ..
            } => format!("ALTER COLUMN {column} TYPE {declared}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum IndexMethod {
    Btree(String),
    /// `CREATE INDEX i ON t (lower(col))`: an EXPRESSION scalar index over
    /// `lower(col)`, which is what makes `lower(col) = x` a range rather
    /// than a refusal (QL_CONTRACT §4.1).
    LowerBtree(String),
    /// `gin(to_tsvector('simple', col))`.
    Gin(String),
    Gist(String),
    Exact(String),
    /// `quantized(col vector_cosine_ops)`, and its hnsw/diskann/ivfflat
    /// aliases, which carry the alias name so a notice can say so.
    Quantized {
        column: String,
        alias: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Stmt {
    Select(Box<SelectStmt>),
    Explain(Box<SelectStmt>),
    Insert {
        table: String,
        columns: Vec<String>,
        rows: Vec<Vec<Literal>>,
    },
    Update {
        table: String,
        assignments: Vec<(String, Literal)>,
        key: Literal,
    },
    /// `UPDATE t SET ... WHERE <any predicate>` (QL_CONTRACT §2, T1 since
    /// `Database::update_where`). The key-equality form above stays its own
    /// statement: it is ONE point-get and one put, and routing it through a
    /// candidate walk would cost a prepared query to reach a row the key
    /// already names.
    UpdateWhere {
        table: String,
        assignments: Vec<(String, SetValue)>,
        predicates: Vec<Expr>,
    },
    Delete {
        table: String,
        key: Literal,
    },
    /// `DELETE FROM t WHERE <any predicate> [RESTRICT|CASCADE]`, and
    /// `DELETE FROM ALL` with `table: None`. RESTRICT is the default
    /// (GRAPH_CONTRACT 6.1).
    DeleteWhere {
        /// `None` is `FROM ALL`: every collection of the catalog.
        table: Option<String>,
        predicates: Vec<Expr>,
        cascade: bool,
    },
    /// `EXPLAIN UPDATE ...` / `EXPLAIN DELETE ...`: the plan of a write,
    /// printed WITHOUT running it, for the reason `ExplainDropTable` is not
    /// run either -- explaining a destructive statement by executing it is
    /// not an explanation.
    ExplainWrite(Box<Stmt>),
    /// `BEGIN BULK` / `END BULK` (`docs/dist/OPS_CONTRACT.md` §7).
    BeginBulk,
    EndBulk,
    CreateTable {
        table: String,
        columns: Vec<ColumnDef>,
    },
    CreateIndex {
        name: String,
        table: String,
        method: IndexMethod,
    },
    DropTable {
        table: String,
        if_exists: bool,
        /// `DROP TABLE t CASCADE`. RESTRICT is the default and the spelling
        /// `RESTRICT` is accepted for it: GRAPH_CONTRACT 6.1 makes the
        /// default refuse while any edge references a row of the table.
        cascade: bool,
    },
    /// `EXPLAIN DROP TABLE ...`. The only EXPLAIN that does not run its
    /// statement: printing the plan of a destructive DDL by executing it is
    /// not an explanation, it is the drop.
    ExplainDropTable {
        table: String,
        if_exists: bool,
        cascade: bool,
    },
    DropIndex {
        name: String,
        if_exists: bool,
    },
    AlterTable {
        table: String,
        action: AlterAction,
    },
    /// `EXPLAIN ALTER TABLE ...`. Like `EXPLAIN DROP TABLE`, it does not run
    /// its statement: printing the plan of a catalog rewrite by performing
    /// the rewrite is not an explanation.
    ExplainAlterTable {
        table: String,
        action: AlterAction,
    },
    Begin,
    Commit,
    Rollback,
    /// `SET LOCAL <name> = <value>`. Only `ef_search` and
    /// `diskann.query_search_list_size` change anything; the rest are a
    /// notice, because a knob this engine does not have must not read as one
    /// it silently honoured.
    SetLocal {
        name: String,
        value: Literal,
    },
}

// ── how a compiled form spells itself back ────────────────────────────────

impl Literal {
    /// The literal as a statement would have written it, for an EXPLAIN line.
    pub(super) fn written(&self) -> String {
        match self {
            Self::Null => "NULL".into(),
            Self::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
            Self::Num(value, exact) => {
                if *exact {
                    format!("{}", *value as i64)
                } else {
                    format!("{value}")
                }
            }
            Self::Str(text) => format!("'{text}'"),
            Self::Param(n) => format!("${n}"),
            Self::Subquery(_) => "(SELECT ...)".into(),
        }
    }
}

impl TimeValue {
    pub(super) fn written(&self) -> String {
        match self {
            Self::Lit(literal) => literal.written(),
            Self::Clock { date_only, offset } => {
                let clock = if *date_only { "current_date" } else { "now()" };
                match offset {
                    0 => clock.to_owned(),
                    n if *n < 0 => format!("{clock} - interval {}us", -n),
                    n => format!("{clock} + interval {n}us"),
                }
            }
        }
    }
}

impl TimeShape {
    pub(super) fn written(&self, column: &str) -> String {
        match self {
            Self::Extract { unit, op, value } => format!(
                "EXTRACT({} FROM {column}) {} {}",
                unit.written(),
                op.written(),
                value.written()
            ),
            Self::ExtractBetween { unit, lower, upper } => format!(
                "EXTRACT({} FROM {column}) BETWEEN {} AND {}",
                unit.written(),
                lower.written(),
                upper.written()
            ),
            Self::Trunc { unit, op, value } => format!(
                "date_trunc('{}', {column}) {} {}",
                unit.written(),
                op.written(),
                value.written()
            ),
            Self::TruncBetween { unit, lower, upper } => format!(
                "date_trunc('{}', {column}) BETWEEN {} AND {}",
                unit.written(),
                lower.written(),
                upper.written()
            ),
            Self::CastDate { op, value } => {
                format!("{column}::date {} {}", op.written(), value.written())
            }
            Self::Clock { op, value } => {
                format!("{column} {} {}", op.written(), value.written())
            }
            Self::ClockBetween { lower, upper } => format!(
                "{column} BETWEEN {} AND {}",
                lower.written(),
                upper.written()
            ),
        }
    }
}

impl TextShape {
    pub(super) fn written(&self, column: &str) -> String {
        match self {
            Self::LowerEq { value } => format!("lower({column}) = {}", value.written()),
            Self::LowerPrefix { value, written } => {
                format!("{written} over lower({column}), prefix {}", value.written())
            }
            Self::Prefix { value, written } => {
                format!("{written} over {column}, prefix {}", value.written())
            }
        }
    }
}

impl RowExpr {
    /// The expression as a statement would have written it. It names the
    /// output column when no alias was given and it is the EXPLAIN "row
    /// functions" line, so it is built from the tree rather than from the
    /// source text: the two cannot then disagree.
    pub(super) fn written(&self) -> String {
        match self {
            Self::Column(name) => name.clone(),
            Self::Lit(literal) => literal.written(),
            Self::Now => "now()".into(),
            Self::CurrentDate => "current_date".into(),
            Self::Interval(micros) => format!("interval {micros}us"),
            Self::Extract { unit, arg } => {
                format!("EXTRACT({} FROM {})", unit.written(), arg.written())
            }
            Self::Trunc { unit, arg } => {
                format!("date_trunc('{}', {})", unit.written(), arg.written())
            }
            Self::Age { left, right } => match right {
                None => format!("age({})", left.written()),
                Some(right) => format!("age({}, {})", left.written(), right.written()),
            },
            Self::ToChar { arg, format } => format!("to_char({}, '{format}')", arg.written()),
            Self::ToTimestamp(arg) => format!("to_timestamp({})", arg.written()),
            Self::ToDate(arg) => format!("to_date({})", arg.written()),
            Self::CastDate(arg) => format!("{}::date", arg.written()),
            Self::CastText(arg) => format!("{}::text", arg.written()),
            Self::Str { func, args } => format!(
                "{}({})",
                func.written(),
                args.iter().map(Self::written).collect::<Vec<_>>().join(", ")
            ),
            Self::Add(a, b) => format!("({} + {})", a.written(), b.written()),
            Self::Sub(a, b) => format!("({} - {})", a.written(), b.written()),
            Self::Concat(a, b) => format!("({} || {})", a.written(), b.written()),
        }
    }
}
