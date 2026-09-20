//! The statement, as written. Nothing here knows about indexes, entity ids or
//! drivers: the AST is the shape of the text, and `compile.rs` is the only
//! place that turns a shape into a `QueryRequest` or a write.

use crate::Kind;

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

/// The aggregate functions `docs/QL_CONTRACT.md` §4.7 accepts.
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
    /// `col / n` in a select list: the one grouping expression, written
    /// again where the answer reports it. Outside a folded answer it is an
    /// arithmetic expression like any other and reports the ranking value,
    /// which is what this select list has always done with an expression.
    Divided {
        column: String,
        divisor: i64,
    },
}

/// One element of a `GRAPH_TABLE` pattern's path.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct GraphHop {
    pub(super) edge_type: Option<String>,
    /// `Outgoing` for `-[..]->`, `Incoming` for `<-[..]-`, `Both` for `-[..]-`.
    pub(super) direction: GraphDirection,
    pub(super) min_depth: usize,
    pub(super) max_depth: usize,
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
    /// `COLUMNS (b.<field> AS <alias>)`.
    pub(super) columns: Vec<(SelectItem, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Source {
    Table(String),
    Graph(Box<GraphTable>),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct SelectStmt {
    pub(super) items: Vec<(SelectItem, Option<String>)>,
    pub(super) source: Source,
    pub(super) predicates: Vec<Predicate>,
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
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum IndexMethod {
    Btree(String),
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
    Delete {
        table: String,
        key: Literal,
    },
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
