//! `sekejap` -- the published crate, and the one name an application imports.
//!
//! One handle, [`Db`]; one error, [`Error`]; documents in and out as
//! `serde_json::Value`; SQL with `$n` parameters; edges through the graph
//! atomics. The whole surface, with the E4 call each item maps to, is
//! `docs/dist/RUST_API.md`.
//!
//! This crate is the Rust distribution of the three layers
//! (`docs/LAYERS.md`): `core` (the atomics), `lang` (SQL with `GRAPH_TABLE`)
//! and `dist` (the service wrapper). It re-exports them under one root, so a
//! caller that wants a layer directly writes `sekejap::core::Database`,
//! `sekejap::lang::SqlDatabase` or `sekejap::dist::service::ServiceDatabase`
//! and still takes one dependency.
//!
//! It adds no execution: every call below is a composition of calls those
//! three layers already export, and a construct with no atomic underneath is
//! REFUSED by name ([`Error::Refused`]), never emulated.
//!
//! ```no_run
//! use sekejap::{Db, FieldKind};
//! use serde_json::json;
//!
//! let db = Db::open("/var/lib/app")?;
//! db.create_collection("posts", &[("title", FieldKind::Text)])?;
//! db.put(("posts", "p1"), &serde_json::json!({ "title": "Hello" }))?;
//!
//! let rows = db.query("SELECT _key, title FROM posts WHERE _key = $1", &[json!("p1")])?;
//! for row in rows.iter() {
//!     println!("{:?}", row.to_object());
//! }
//! # Ok::<(), sekejap::Error>(())
//! ```

mod catalog;
mod db;
mod error;
mod rows;
mod scan;

pub use catalog::{Collection, Field, Index};
pub use db::{Db, Tx, KEY, MAX_NEIGHBOURS, QUERY_PAGE, SCAN_PAGE};
pub use error::{Error, Result};
pub use rows::{param_of, params_of, value_to_json, Row, Rows};
pub use scan::Scan;

/// The layers, for a caller that needs one directly.
pub use sekejap_core as core;
pub use sekejap_dist as dist;
pub use sekejap_lang as lang;

/// The store configuration [`Db::open_with`] takes, and its two enumerations.
pub use sekejap_core::{Config, IoMode, SyncMode};
/// The engine's row identity, and the identity of a collection.
pub use sekejap_core::collections::{CollectionId, EntityId};
/// Which family an index belongs to, and which way an edge points.
pub use sekejap_core::collections::{Direction, IndexFamily};
/// What a column stores. `Vector(n)` carries its dimension; `Point` and
/// `Geo` are the two spatial kinds.
pub use sekejap_core::Kind as FieldKind;
/// One value of one column, the parameter form of one, and the tier a
/// refused construct sits in.
pub use sekejap_lang::{Param, SqlError, SqlValue, Tier};

use serde_json::Value;

/// The crate version, as one string for a caller that reports it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Which of the two shapes a [`Db`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// One `Mutex<Database>`. Readers and the writer share it, so a read
    /// sees the write before it with no publication step.
    Single,
    /// `docs/dist/OPS_CONTRACT.md` §1: one writer, parallel readers on a
    /// published snapshot, and the commit-time change feed.
    Service,
}

/// A row's address: the collection it is in and its external key.
///
/// One argument rather than two bare strings, so the two cannot be swapped
/// silently. `("posts", "p1")` converts into it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Addr<'a> {
    pub collection: &'a str,
    pub key: &'a str,
}

impl<'a> Addr<'a> {
    pub fn new(collection: &'a str, key: &'a str) -> Self {
        Self { collection, key }
    }
}

impl<'a> From<(&'a str, &'a str)> for Addr<'a> {
    fn from((collection, key): (&'a str, &'a str)) -> Self {
        Self { collection, key }
    }
}

impl<'a> From<(&'a String, &'a String)> for Addr<'a> {
    fn from((collection, key): (&'a String, &'a String)) -> Self {
        Self { collection, key }
    }
}

impl<'a> From<&'a Document> for Addr<'a> {
    fn from(document: &'a Document) -> Self {
        Self {
            collection: &document.collection,
            key: &document.key,
        }
    }
}

impl std::fmt::Display for Addr<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.collection, self.key)
    }
}

/// One row as [`Db::scan`] and [`Db::neighbours`] hand it over.
#[derive(Clone, Debug, PartialEq)]
pub struct Document {
    pub collection: String,
    pub key: String,
    pub id: EntityId,
    /// The row's fields, with [`KEY`] set to `key`.
    pub fields: Value,
}

impl Document {
    /// This row's address.
    pub fn addr(&self) -> Addr<'_> {
        Addr {
            collection: &self.collection,
            key: &self.key,
        }
    }
    /// One field by name.
    pub fn field(&self, name: &str) -> Option<&Value> {
        self.fields.get(name)
    }
}

/// What the database occupies on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Storage {
    /// The data file.
    pub data_bytes: u64,
    /// The write-ahead log, which a checkpoint folds into the data file.
    pub wal_bytes: u64,
}

impl Storage {
    pub fn total_bytes(&self) -> u64 {
        self.data_bytes.saturating_add(self.wal_bytes)
    }
}
