//! What `Db::describe` answers. `docs/dist/RUST_API.md` §5.

use sekejap_core::collections::{IndexFamily, TextAnalyzer};
use sekejap_core::Kind;

/// The declared shape of one collection.
#[derive(Clone, Debug, PartialEq)]
pub struct Collection {
    /// The table's own name, without its schema.
    pub name: String,
    /// The schema the table belongs to: `public` unless it was created in a
    /// named one.
    pub schema: String,
    /// The declared columns, `_key` first.
    pub fields: Vec<Field>,
    pub indexes: Vec<Index>,
    /// Whether the collection stamps its rows with created/updated times.
    pub timestamps: bool,
    /// The LIVE row count, when the database keeps one for this collection.
    ///
    /// `None` is not "no rows": it is "this database has no live row-count
    /// record for this collection", which is every database written before
    /// the record existed and every collection a
    /// `Database::backfill_row_counts` has not reached. Then the number costs
    /// a walk, and [`crate::Db::scan_count_rows`] is the call that says so in
    /// its name.
    pub rows: Option<u64>,
    /// `Some` for an EDGE TABLE (`docs/core/EDGE_TABLES.md`): a table whose
    /// rows are edges. `None` for a table of rows.
    pub edge: Option<EdgeTableInfo>,
}

/// What an edge table declares. Table names are written as a statement
/// writes them: `t` in `public`, `schema.t` elsewhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeTableInfo {
    /// Each `REFERENCES` column and the table it names.
    pub references: Vec<(String, String)>,
    /// The primary key's columns; empty for a table without one.
    pub key: Vec<String>,
    /// The source and destination columns and their tables and the label
    /// (edge type) the edges carry -- `None` until the table's direction is
    /// fixed, by a property graph or by `ALTER PROPERTY GRAPH base`. `graph`
    /// is the first property graph, by name, that shows the table as an edge
    /// table, and `None` when only `base` does.
    pub source: Option<String>,
    pub source_table: Option<String>,
    pub destination: Option<String>,
    pub destination_table: Option<String>,
    pub label: Option<String>,
    pub graph: Option<String>,
}

impl Collection {
    /// One field by name.
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }
    /// Every index on one field.
    pub fn indexes_on<'a>(&'a self, field: &'a str) -> impl Iterator<Item = &'a Index> {
        self.indexes.iter().filter(move |i| i.field == field)
    }
}

/// One declared column.
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: String,
    /// What the row codec and every index key encode.
    pub kind: Kind,
    /// The SQL spelling the catalog recorded where the `Kind` does not carry
    /// it: `TIMESTAMPTZ` and `DATE` are both `Kind::Int`.
    pub declared: Option<String>,
    /// True for the column that supplies each row's key: `_key`, or the
    /// column a table named `PRIMARY KEY`.
    pub primary_key: bool,
    /// NOT NULL: always for `_key` and a named key column.
    pub not_null: bool,
    /// The column's DEFAULT as a `CREATE TABLE` writes it (`ulid()`,
    /// `now()`, `uuid5('...', '...')`), the key's own included.
    pub default: Option<String>,
}

/// One index in the catalog.
#[derive(Clone, Debug, PartialEq)]
pub struct Index {
    pub name: String,
    pub field: String,
    pub family: IndexFamily,
    /// For the text family, how values become terms: `Words` for
    /// `gin (to_tsvector('simple', col))`, `Trigram` for
    /// `gin (col gin_trgm_ops)`. `None` for every other family.
    pub analyzer: Option<TextAnalyzer>,
    pub unique: bool,
    /// False while the index is still being built: a query that needs it is
    /// refused rather than answered from a half-built index.
    pub ready: bool,
}
