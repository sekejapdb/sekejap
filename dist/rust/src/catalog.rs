//! What `Db::describe` answers. `docs/dist/RUST_API.md` §5.

use sekejap_core::collections::IndexFamily;
use sekejap_core::Kind;

/// The declared shape of one collection.
#[derive(Clone, Debug, PartialEq)]
pub struct Collection {
    pub name: String,
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
    /// True only for `_key`, which is the external key of every row.
    pub primary_key: bool,
}

/// One index in the catalog.
#[derive(Clone, Debug, PartialEq)]
pub struct Index {
    pub name: String,
    pub field: String,
    pub family: IndexFamily,
    pub unique: bool,
    /// False while the index is still being built: a query that needs it is
    /// refused rather than answered from a half-built index.
    pub ready: bool,
}
