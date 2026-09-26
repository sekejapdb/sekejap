//! The GQL body of `GRAPH_TABLE (<graph> ... RETURN ...)`, as parsed
//! (`docs/lang/GQL_PROFILE_DESIGN.md` §5.2).
//!
//! The tree holds what the text SAID and nothing the binder decides: a
//! variable is a [`Name`], not a slot; a label is the name written, not a
//! collection id. The binder (M2-C) turns it into slots and a plan.
//!
//! Each type is shaped so later milestones EXTEND it rather than rebuild it:
//!
//! * [`PathPattern`] is the path variable (`p =`), the prefix -- a path
//!   mode or a selector, never both (§4.3) -- and the elements.
//! * [`PathElement`] is a node, an edge (which may carry a [`Quantifier`]
//!   and a `COST`), or a parenthesised subpath with its quantifier (§4.1).
//! * [`Statement`] is a `MATCH` or an `OPTIONAL MATCH` (M3-F), a `LET`, a
//!   `FILTER` or a `FOR` (M3-B).
//! * [`LabelExpr`] is a name or an alternation; conjunction and negation are
//!   P1 variants.
//! * [`Expr`] is the M2 subset plus the M3-C scalar pack: arithmetic,
//!   `||`, `IS [NOT] NULL`, `IN (...)`, `CASE`, casts, `COALESCE`,
//!   `NULLIF` and the pack's functions ([`Func`]), plus the vertical
//!   aggregates of a `RETURN` ([`Expr::Aggregate`], M3-B).
//!
//! `Display` prints the tree back in ONE spelling -- labels with `:`, every
//! edge bracketed, every binary operator parenthesised, variables folded --
//! so two spellings of one pattern print the same text. A GQL `EXPLAIN`
//! prints it as its `statement:` line.

use super::schema::Name;
use crate::ast::CmpOp;
use sekejap_core::collections::Direction;
use std::fmt;

/// `SELECT ... FROM GRAPH_TABLE ( <graph> <body> ) [AS <alias>] ...`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GqlGraphTable {
    /// The graph context, a catalog identifier spelled as SQL spells one.
    pub(crate) graph: String,
    pub(crate) body: Pipeline,
    /// The relation's name in the outer `SELECT`.
    pub(crate) alias: Option<Name>,
    /// The outer `SELECT` over the relation (design §5.5), compiled as one
    /// more stage of the plan; `None` for `SELECT * FROM GRAPH_TABLE (...)`
    /// with no clause after it.
    pub(crate) outer: Option<Outer>,
}

/// The outer `SELECT <items> ... [WHERE ..] [GROUP BY ..] [HAVING ..]
/// [ORDER BY ..] [OFFSET ..] [LIMIT ..]` over a GQL relation: its `WHERE`,
/// the rest as a `RETURN` over the relation's columns, whose `GROUP BY` and
/// `ORDER BY` keys may be select-list positions, and its `HAVING`
/// (PostgreSQL's meaning: a predicate on the finished group, naming only a
/// group key or an aggregate; M3-D2, brief gap 2).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Outer {
    pub(crate) where_: Option<Expr>,
    pub(crate) select: Return,
    pub(crate) having: Option<Expr>,
}

/// The stages of a body, separated by `NEXT`: each stage's `RETURN` is the
/// next stage's working table.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Pipeline {
    pub(crate) stages: Vec<Stage>,
}

/// Zero or more statements, then the `RETURN` that projects the stage.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Stage {
    pub(crate) statements: Vec<Statement>,
    pub(crate) ret: Return,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Statement {
    /// `MATCH <pattern>, <pattern> ... [WHERE <expr>]`, or, `optional`,
    /// `OPTIONAL MATCH <pattern> [WHERE <expr>]`: one pattern (design Q3),
    /// whose `WHERE` decides whether a match exists.
    Match {
        patterns: Vec<PathPattern>,
        where_: Option<Expr>,
        optional: bool,
    },
    /// `LET a = <expr>, b = <expr> ...`: every expression is evaluated
    /// against the scope BEFORE the statement, so `b` cannot see `a`.
    Let(Vec<(Name, Expr)>),
    /// `FILTER <expr>`: keeps the rows where the condition is true.
    Filter(Expr),
    /// `FOR <var> IN <list>`: one row per element of the list.
    For { var: Name, list: Expr },
}

/// `RETURN [DISTINCT] <item>, ... | * [GROUP BY ...] [ORDER BY ...]
/// [OFFSET n] [LIMIT n]`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Return {
    pub(crate) distinct: bool,
    /// `RETURN *`: every variable in scope, in the order they were bound;
    /// `items` is then empty.
    pub(crate) star: bool,
    pub(crate) items: Vec<ReturnItem>,
    /// `None`: no `GROUP BY` written (a `RETURN` with an aggregate then
    /// groups by its other items).
    pub(crate) group_by: Option<Vec<Expr>>,
    pub(crate) order_by: Vec<OrderItem>,
    pub(crate) offset: Option<Count>,
    pub(crate) limit: Option<Count>,
}

/// `<expr> [ASC | DESC]`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OrderItem {
    pub(crate) expr: Expr,
    pub(crate) descending: bool,
}

/// An `OFFSET` or `LIMIT` count: an integer literal or a `$n` (design Q13).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Count {
    Lit(u64),
    /// One-based, as written.
    Param(usize),
}

/// `<expr> [AS <alias>]`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ReturnItem {
    pub(crate) expr: Expr,
    pub(crate) alias: Option<Name>,
}

/// `[p =] [<mode> | <selector>] <elements>`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PathPattern {
    /// The path variable, written before the prefix (brief §8.1).
    pub(crate) name: Option<Name>,
    pub(crate) prefix: Option<PathPrefix>,
    /// Nodes, edges and parenthesised subpaths, in written order. The
    /// parser guarantees an edge stands between two node positions (a node,
    /// or a subpath's end), and that two node patterns never touch unless
    /// one side is a subpath.
    pub(crate) elements: Vec<PathElement>,
}

impl PathPattern {
    /// A pattern of nodes and single edges only, with no prefix and no
    /// path variable: a chain of hops (M2-C), not an automaton.
    pub(crate) fn is_chain(&self) -> bool {
        self.name.is_none()
            && self.prefix.is_none()
            && self.elements.iter().all(|element| match element {
                PathElement::Node(_) => true,
                PathElement::Edge(edge) => edge.quantifier.is_none(),
                PathElement::Group { .. } => false,
            })
    }
}

/// What stands before a path pattern's elements: a path mode or a
/// selector. A selector implies `WALK` (§4.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathPrefix {
    Walk,
    Trail,
    Acyclic,
    Any,
    AnyShortest,
    /// `ANY CHEAPEST`: its edge carries the `COST`.
    AnyCheapest,
}

impl PathPrefix {
    pub(crate) fn written(self) -> &'static str {
        match self {
            Self::Walk => "WALK",
            Self::Trail => "TRAIL",
            Self::Acyclic => "ACYCLIC",
            Self::Any => "ANY",
            Self::AnyShortest => "ANY SHORTEST",
            Self::AnyCheapest => "ANY CHEAPEST",
        }
    }

    pub(crate) fn is_selector(self) -> bool {
        matches!(self, Self::Any | Self::AnyShortest | Self::AnyCheapest)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PathElement {
    Node(NodePattern),
    Edge(EdgePattern),
    /// `( <elements> ) <quantifier>`: a subpath repeated as a whole. Its
    /// elements are nodes and single edges, starting and ending on a node
    /// (a quantifier inside is refused, Q10).
    Group {
        elements: Vec<PathElement>,
        quantifier: Quantifier,
    },
}

/// `{m,n}`, `{m,}`, `{n}`, `?`, `*`, `+`: integer-literal bounds (brief
/// §7). `hi: None` is unbounded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Quantifier {
    pub(crate) lo: u32,
    pub(crate) hi: Option<u32>,
}

/// `( [var] [IS|: <label>] [WHERE <expr>] )`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NodePattern {
    pub(crate) var: Option<Name>,
    pub(crate) label: Option<LabelExpr>,
    /// The inline element predicate.
    pub(crate) where_: Option<Expr>,
}

/// `-[ [var] [IS|: <label>] [WHERE <expr>] ]->` and the other two
/// directions, full or abbreviated (`->`, `<-`, `-`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EdgePattern {
    pub(crate) var: Option<Name>,
    pub(crate) label: Option<LabelExpr>,
    pub(crate) where_: Option<Expr>,
    /// `COST <expr>`, the weight `ANY CHEAPEST` minimises.
    pub(crate) cost: Option<Expr>,
    pub(crate) direction: EdgeDirection,
    /// A quantified edge `-[e]->{1,3}`.
    pub(crate) quantifier: Option<Quantifier>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EdgeDirection {
    /// `-[..]->`: from the element on the left to the one on the right.
    Right,
    /// `<-[..]-`: from the element on the right to the one on the left.
    Left,
    /// `-[..]-`: either way.
    Any,
}

impl EdgeDirection {
    /// The engine's direction for a walk along this edge from the element
    /// on its left, or, `reversed`, from the one on its right.
    pub(crate) fn engine(self, reversed: bool) -> Direction {
        match (self, reversed) {
            (Self::Any, _) => Direction::Both,
            (Self::Right, false) | (Self::Left, true) => Direction::Outgoing,
            (Self::Left, false) | (Self::Right, true) => Direction::Incoming,
        }
    }
}

/// A label test. A label names a collection (for a node) or an edge type
/// (for an edge), spelled as SQL spells a name.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum LabelExpr {
    Name(String),
    /// `A|B|C`: any of them, in the order written.
    Or(Vec<LabelExpr>),
}

/// A literal constant.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Literal {
    Null,
    Bool(bool),
    /// A number and whether it was written without a fraction or an
    /// exponent, as the SQL lexer reports it.
    Num(f64, bool),
    Str(String),
}

/// A GQL expression: comparisons and boolean logic (M2) and the P0 scalar
/// pack (M3-C) over property references, variables, literals and `$n`
/// parameters.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Expr {
    Literal(Literal),
    /// `$n`, one-based, one namespace with the outer SQL.
    Param(usize),
    /// A bound variable, as a whole value.
    Var(Name),
    /// `var.property`. The property is spelled as written (a stored field
    /// name is case-sensitive).
    Property { var: Name, property: String },
    Compare {
        op: CmpOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    /// Unary `-`. A minus written before a number literal is part of the
    /// literal instead.
    Neg(Box<Expr>),
    Arith {
        op: ArithOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// `||`: text concatenation.
    Concat(Box<Expr>, Box<Expr>),
    /// `expr IS [NOT] NULL`.
    IsNull { expr: Box<Expr>, negated: bool },
    /// `expr [NOT] IN (member, ...)`.
    In {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    /// `expr [NOT] IN <list>`: membership in a list VALUE, such as a list
    /// parameter `$n` (design §7).
    Member {
        expr: Box<Expr>,
        list: Box<Expr>,
        negated: bool,
    },
    /// `CASE [operand] WHEN .. THEN .. [...] [ELSE ..] END`: the simple form
    /// when `operand` is written, the searched form when it is not.
    Case {
        operand: Option<Box<Expr>>,
        branches: Vec<(Expr, Expr)>,
        otherwise: Option<Box<Expr>>,
    },
    /// `CAST(expr AS type)` and `expr::type`, one node for both spellings.
    Cast { expr: Box<Expr>, to: CastType },
    /// `COALESCE(a, b, ...)`: the first argument that is not NULL.
    Coalesce(Vec<Expr>),
    /// `NULLIF(a, b)`: NULL when `a = b`, otherwise `a`.
    Nullif(Box<Expr>, Box<Expr>),
    /// A function of the pack, its arity already checked.
    Call { func: Func, args: Vec<Expr> },
    /// An aggregate: `COUNT(*)` (no `arg`), `COUNT([DISTINCT] x)`, `SUM`,
    /// `AVG`, `MIN`, `MAX`, `ARRAY_AGG`. In a `RETURN` item or its
    /// `ORDER BY` it is VERTICAL, over the working table's rows; in a `LET`,
    /// a `FILTER` or a `MATCH`'s `WHERE` it is HORIZONTAL, over the list
    /// variable its argument names (design §4.5, `horizontal.rs`).
    Aggregate {
        func: AggFunc,
        distinct: bool,
        arg: Option<Box<Expr>>,
    },
    /// A path or element function (design §4.4) over one argument.
    Graph { func: GraphFunc, arg: Box<Expr> },
    /// A list literal `[a, b, ...]`, possibly empty (design Q2).
    List(Vec<Expr>),
}

/// The path, element and list functions (design §4.4, brief §6): Google's
/// graph helper spellings, each over one argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GraphFunc {
    PathLength,
    PathFirst,
    PathLast,
    Nodes,
    Edges,
    IsAcyclic,
    IsTrail,
    ElementId,
    SourceNodeId,
    DestinationNodeId,
    Labels,
    PropertyNames,
    ArrayLength,
}

impl GraphFunc {
    /// Every one, in the order the messages list them.
    pub(crate) const ALL: &[Self] = &[
        Self::PathLength,
        Self::PathFirst,
        Self::PathLast,
        Self::Nodes,
        Self::Edges,
        Self::IsAcyclic,
        Self::IsTrail,
        Self::ElementId,
        Self::SourceNodeId,
        Self::DestinationNodeId,
        Self::Labels,
        Self::PropertyNames,
        Self::ArrayLength,
    ];

    /// The function a name calls, if it is one.
    pub(crate) fn named(upper: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|f| f.written() == upper)
    }

    pub(crate) fn written(self) -> &'static str {
        match self {
            Self::PathLength => "PATH_LENGTH",
            Self::PathFirst => "PATH_FIRST",
            Self::PathLast => "PATH_LAST",
            Self::Nodes => "NODES",
            Self::Edges => "EDGES",
            Self::IsAcyclic => "IS_ACYCLIC",
            Self::IsTrail => "IS_TRAIL",
            Self::ElementId => "ELEMENT_ID",
            Self::SourceNodeId => "SOURCE_NODE_ID",
            Self::DestinationNodeId => "DESTINATION_NODE_ID",
            Self::Labels => "LABELS",
            Self::PropertyNames => "PROPERTY_NAMES",
            Self::ArrayLength => "ARRAY_LENGTH",
        }
    }
}

/// An aggregate function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    ArrayAgg,
}

impl AggFunc {
    /// Every one, in the order the messages list them.
    pub(crate) const ALL: &[Self] = &[
        Self::Count,
        Self::Sum,
        Self::Avg,
        Self::Min,
        Self::Max,
        Self::ArrayAgg,
    ];

    /// The aggregate a name calls, if it is one.
    pub(crate) fn named(upper: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|f| f.written() == upper)
    }

    pub(crate) fn written(self) -> &'static str {
        match self {
            Self::Count => "COUNT",
            Self::Sum => "SUM",
            Self::Avg => "AVG",
            Self::Min => "MIN",
            Self::Max => "MAX",
            Self::ArrayAgg => "ARRAY_AGG",
        }
    }
}

impl Expr {
    /// The expressions this one is built from, in written order.
    pub(crate) fn children(&self) -> Vec<&Expr> {
        match self {
            Self::Literal(_) | Self::Param(_) | Self::Var(_) | Self::Property { .. } => Vec::new(),
            Self::Compare { left, right, .. }
            | Self::Arith { left, right, .. }
            | Self::And(left, right)
            | Self::Or(left, right)
            | Self::Concat(left, right)
            | Self::Nullif(left, right) => vec![&**left, &**right],
            Self::Member { expr, list, .. } => vec![&**expr, &**list],
            Self::Not(inner) | Self::Neg(inner) => vec![&**inner],
            Self::IsNull { expr, .. } | Self::Cast { expr, .. } => vec![&**expr],
            Self::In { expr, list, .. } => std::iter::once(&**expr).chain(list).collect(),
            Self::Case {
                operand,
                branches,
                otherwise,
            } => operand
                .as_deref()
                .into_iter()
                .chain(branches.iter().flat_map(|(when, then)| [when, then]))
                .chain(otherwise.as_deref())
                .collect(),
            Self::Coalesce(args) | Self::Call { args, .. } => args.iter().collect(),
            Self::Aggregate { arg, .. } => arg.as_deref().into_iter().collect(),
            Self::Graph { arg, .. } => vec![&**arg],
            Self::List(items) => items.iter().collect(),
        }
    }

    /// True when an aggregate stands anywhere in this expression.
    pub(crate) fn has_aggregate(&self) -> bool {
        matches!(self, Self::Aggregate { .. }) || self.children().into_iter().any(Self::has_aggregate)
    }
}

/// An arithmetic operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    /// `^`, which is `POWER`.
    Pow,
}

impl ArithOp {
    pub(crate) fn written(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Mod => "%",
            Self::Pow => "^",
        }
    }
}

/// The declared types a cast reads a value into: the column types sekejap
/// stores (`CREATE TABLE`'s spellings), less the spatial and vector ones
/// (M6). `INT` and `BIGINT` are one type here, as they are one `Kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CastType {
    Text,
    Int,
    Float,
    Bool,
    Json,
    /// Microseconds since the epoch at midnight UTC of the day.
    Date,
    /// Microseconds since the epoch, UTC.
    Timestamp,
}

impl CastType {
    /// The one spelling the normal form prints.
    pub(crate) fn written(self) -> &'static str {
        match self {
            Self::Text => "TEXT",
            Self::Int => "BIGINT",
            Self::Float => "DOUBLE PRECISION",
            Self::Bool => "BOOLEAN",
            Self::Json => "JSONB",
            Self::Date => "DATE",
            Self::Timestamp => "TIMESTAMPTZ",
        }
    }

    /// PostgreSQL's internal type name, which names an unaliased cast
    /// column (`x::bigint` is the column `int8`).
    pub(crate) fn pg_name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Int => "int8",
            Self::Float => "float8",
            Self::Bool => "bool",
            Self::Json => "jsonb",
            Self::Date => "date",
            Self::Timestamp => "timestamptz",
        }
    }
}

/// The P0 scalar functions (brief §6): the math functions and the string
/// functions the SQL row functions share (`crate::functions`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Func {
    Abs,
    Sqrt,
    Power,
    Exp,
    Ln,
    Lower,
    Upper,
    Trim,
    Length,
    /// `substring(s, start [, count])`, also written
    /// `substring(s FROM start [FOR count])`.
    Substring,
    /// `concat(a, ...)`, which skips NULL arguments, unlike `||`.
    Concat,
}

impl Func {
    /// Every one, in the order the messages list them.
    pub(crate) const ALL: &[Self] = &[
        Self::Abs,
        Self::Sqrt,
        Self::Power,
        Self::Exp,
        Self::Ln,
        Self::Lower,
        Self::Upper,
        Self::Trim,
        Self::Length,
        Self::Substring,
        Self::Concat,
    ];

    /// The function a name calls, if the pack holds it.
    pub(crate) fn named(upper: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|f| f.written() == upper)
    }

    pub(crate) fn written(self) -> &'static str {
        match self {
            Self::Abs => "ABS",
            Self::Sqrt => "SQRT",
            Self::Power => "POWER",
            Self::Exp => "EXP",
            Self::Ln => "LN",
            Self::Lower => "LOWER",
            Self::Upper => "UPPER",
            Self::Trim => "TRIM",
            Self::Length => "LENGTH",
            Self::Substring => "SUBSTRING",
            Self::Concat => "CONCAT",
        }
    }

    /// How many arguments it takes, fewest and most.
    pub(crate) fn arity(self) -> (usize, usize) {
        match self {
            Self::Power => (2, 2),
            Self::Substring => (2, 3),
            Self::Concat => (1, usize::MAX),
            _ => (1, 1),
        }
    }
}

// ── the normal form ─────────────────────────────────────────────────────────

impl fmt::Display for GqlGraphTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(outer) = &self.outer {
            f.write_str("SELECT ")?;
            outer.select.items(f)?;
            f.write_str(" FROM ")?;
        }
        write!(f, "GRAPH_TABLE ({} {})", self.graph, self.body)?;
        if let Some(alias) = &self.alias {
            write!(f, " AS {alias}")?;
        }
        if let Some(outer) = &self.outer {
            if let Some(predicate) = &outer.where_ {
                write!(f, " WHERE {predicate}")?;
            }
            outer.select.clauses(f, outer.having.as_ref())?;
        }
        Ok(())
    }
}

impl fmt::Display for Pipeline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (at, stage) in self.stages.iter().enumerate() {
            if at > 0 {
                f.write_str(" NEXT ")?;
            }
            write!(f, "{stage}")?;
        }
        Ok(())
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for statement in &self.statements {
            write!(f, "{statement} ")?;
        }
        write!(f, "{}", self.ret)
    }
}

impl fmt::Display for Statement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Match {
                patterns,
                where_,
                optional,
            } => {
                if *optional {
                    f.write_str("OPTIONAL ")?;
                }
                f.write_str("MATCH ")?;
                for (at, pattern) in patterns.iter().enumerate() {
                    if at > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{pattern}")?;
                }
                if let Some(predicate) = where_ {
                    write!(f, " WHERE {predicate}")?;
                }
                Ok(())
            }
            Self::Let(assignments) => {
                f.write_str("LET ")?;
                for (at, (name, expr)) in assignments.iter().enumerate() {
                    if at > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{name} = {expr}")?;
                }
                Ok(())
            }
            Self::Filter(predicate) => write!(f, "FILTER {predicate}"),
            Self::For { var, list } => write!(f, "FOR {var} IN {list}"),
        }
    }
}

impl fmt::Display for Count {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lit(n) => write!(f, "{n}"),
            Self::Param(n) => write!(f, "${n}"),
        }
    }
}

impl fmt::Display for Return {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RETURN ")?;
        self.items(f)?;
        self.clauses(f, None)
    }
}

impl Return {
    /// `[DISTINCT] * | item, ...`.
    fn items(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.distinct {
            f.write_str("DISTINCT ")?;
        }
        if self.star {
            f.write_str("*")?;
        }
        for (at, item) in self.items.iter().enumerate() {
            if at > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{}", item.expr)?;
            if let Some(alias) = &item.alias {
                write!(f, " AS {alias}")?;
            }
        }
        Ok(())
    }

    /// `[GROUP BY ..] [HAVING ..] [ORDER BY ..] [OFFSET n] [LIMIT n]`.
    /// `having` is `None` for a body `RETURN`, which has no `HAVING`; the
    /// outer `SELECT` (`GqlGraphTable`'s `Display`) passes its own.
    fn clauses(&self, f: &mut fmt::Formatter<'_>, having: Option<&Expr>) -> fmt::Result {
        if let Some(keys) = &self.group_by {
            f.write_str(" GROUP BY ")?;
            list(f, keys)?;
        }
        if let Some(having) = having {
            write!(f, " HAVING {having}")?;
        }
        for (at, item) in self.order_by.iter().enumerate() {
            f.write_str(if at == 0 { " ORDER BY " } else { ", " })?;
            write!(f, "{}", item.expr)?;
            if item.descending {
                f.write_str(" DESC")?;
            }
        }
        if let Some(offset) = &self.offset {
            write!(f, " OFFSET {offset}")?;
        }
        if let Some(limit) = &self.limit {
            write!(f, " LIMIT {limit}")?;
        }
        Ok(())
    }
}

impl fmt::Display for PathPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(name) = &self.name {
            write!(f, "{name} = ")?;
        }
        if let Some(prefix) = self.prefix {
            write!(f, "{} ", prefix.written())?;
        }
        elements(f, &self.elements)
    }
}

fn elements(f: &mut fmt::Formatter<'_>, elements: &[PathElement]) -> fmt::Result {
    for element in elements {
        match element {
            PathElement::Node(node) => write!(f, "{node}")?,
            PathElement::Edge(edge) => write!(f, "{edge}")?,
            PathElement::Group {
                elements: inner,
                quantifier,
            } => {
                f.write_str("(")?;
                self::elements(f, inner)?;
                write!(f, "){quantifier}")?;
            }
        }
    }
    Ok(())
}

impl fmt::Display for Quantifier {
    /// One spelling: `{m,n}`, or `{m,}` when unbounded.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.hi {
            Some(hi) => write!(f, "{{{},{hi}}}", self.lo),
            None => write!(f, "{{{},}}", self.lo),
        }
    }
}

/// `var:label WHERE expr`, the part between an element's brackets.
fn filler(
    f: &mut fmt::Formatter<'_>,
    var: &Option<Name>,
    label: &Option<LabelExpr>,
    where_: &Option<Expr>,
) -> fmt::Result {
    if let Some(var) = var {
        write!(f, "{var}")?;
    }
    if let Some(label) = label {
        write!(f, ":{label}")?;
    }
    if let Some(predicate) = where_ {
        if var.is_some() || label.is_some() {
            f.write_str(" ")?;
        }
        write!(f, "WHERE {predicate}")?;
    }
    Ok(())
}

impl fmt::Display for NodePattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("(")?;
        filler(f, &self.var, &self.label, &self.where_)?;
        f.write_str(")")
    }
}

impl fmt::Display for EdgePattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.direction {
            EdgeDirection::Left => "<-[",
            EdgeDirection::Right | EdgeDirection::Any => "-[",
        })?;
        filler(f, &self.var, &self.label, &self.where_)?;
        if let Some(cost) = &self.cost {
            if self.var.is_some() || self.label.is_some() || self.where_.is_some() {
                f.write_str(" ")?;
            }
            write!(f, "COST {cost}")?;
        }
        f.write_str(match self.direction {
            EdgeDirection::Right => "]->",
            EdgeDirection::Left | EdgeDirection::Any => "]-",
        })?;
        if let Some(quantifier) = &self.quantifier {
            write!(f, "{quantifier}")?;
        }
        Ok(())
    }
}

impl fmt::Display for LabelExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Name(name) => f.write_str(name),
            Self::Or(options) => {
                for (at, option) in options.iter().enumerate() {
                    if at > 0 {
                        f.write_str("|")?;
                    }
                    write!(f, "{option}")?;
                }
                Ok(())
            }
        }
    }
}

impl fmt::Display for Literal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("NULL"),
            Self::Bool(true) => f.write_str("TRUE"),
            Self::Bool(false) => f.write_str("FALSE"),
            // An exact number prints as the integer it is; an inexact one
            // keeps its fraction (`1.0`), so the two stay distinguishable.
            Self::Num(value, true) => write!(f, "{value}"),
            Self::Num(value, false) => write!(f, "{value:?}"),
            Self::Str(text) => write!(f, "'{}'", text.replace('\'', "''")),
        }
    }
}

/// `a, b, c`.
fn list(f: &mut fmt::Formatter<'_>, items: &[Expr]) -> fmt::Result {
    for (at, item) in items.iter().enumerate() {
        if at > 0 {
            f.write_str(", ")?;
        }
        write!(f, "{item}")?;
    }
    Ok(())
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal(literal) => write!(f, "{literal}"),
            Self::Param(n) => write!(f, "${n}"),
            Self::Var(name) => write!(f, "{name}"),
            Self::Property { var, property } => write!(f, "{var}.{property}"),
            Self::Compare { op, left, right } => {
                write!(f, "({left} {} {right})", op.written())
            }
            Self::Not(inner) => write!(f, "(NOT {inner})"),
            Self::And(left, right) => write!(f, "({left} AND {right})"),
            Self::Or(left, right) => write!(f, "({left} OR {right})"),
            Self::Neg(inner) => write!(f, "(-{inner})"),
            Self::Arith { op, left, right } => write!(f, "({left} {} {right})", op.written()),
            Self::Concat(left, right) => write!(f, "({left} || {right})"),
            Self::IsNull { expr, negated } => {
                write!(f, "({expr} IS {}NULL)", if *negated { "NOT " } else { "" })
            }
            Self::In {
                expr,
                list: members,
                negated,
            } => {
                write!(f, "({expr} {}IN (", if *negated { "NOT " } else { "" })?;
                list(f, members)?;
                f.write_str("))")
            }
            Self::Member {
                expr,
                list,
                negated,
            } => write!(f, "({expr} {}IN {list})", if *negated { "NOT " } else { "" }),
            Self::Case {
                operand,
                branches,
                otherwise,
            } => {
                f.write_str("CASE")?;
                if let Some(operand) = operand {
                    write!(f, " {operand}")?;
                }
                for (when, then) in branches {
                    write!(f, " WHEN {when} THEN {then}")?;
                }
                if let Some(otherwise) = otherwise {
                    write!(f, " ELSE {otherwise}")?;
                }
                f.write_str(" END")
            }
            Self::Cast { expr, to } => write!(f, "CAST({expr} AS {})", to.written()),
            Self::Coalesce(args) => {
                f.write_str("COALESCE(")?;
                list(f, args)?;
                f.write_str(")")
            }
            Self::Nullif(left, right) => write!(f, "NULLIF({left}, {right})"),
            Self::Call { func, args } => {
                write!(f, "{}(", func.written())?;
                list(f, args)?;
                f.write_str(")")
            }
            Self::Aggregate {
                func,
                distinct,
                arg,
            } => {
                write!(f, "{}(", func.written())?;
                if *distinct {
                    f.write_str("DISTINCT ")?;
                }
                match arg {
                    Some(arg) => write!(f, "{arg})"),
                    None => f.write_str("*)"),
                }
            }
            Self::Graph { func, arg } => write!(f, "{}({arg})", func.written()),
            Self::List(items) => {
                f.write_str("[")?;
                list(f, items)?;
                f.write_str("]")
            }
        }
    }
}
