//! The CATALOG SURFACE a PostgreSQL client expects, as VIRTUAL ROWS.
//!
//! Nothing here is stored. Every relation below is computed from the catalog
//! readers the engine already has -- `Database::list_collections`,
//! `collection_info`, `list_indexes`, `row_count`, `graph_names`,
//! `edge_shape` -- at PREPARE, into a typed value list the compiler owns. A
//! `SELECT` over one of them is an ordinary `SELECT` whose driver is the
//! bounded in-memory row source of `compile/rows.rs`; `WHERE`, `ORDER BY`,
//! `LIMIT` and `DISTINCT` compose over that list.
//!
//! There are two layers, and the second is a projection of the first:
//!
//! * the `db_*` CORE ROWS (`docs/lang/QL_CONTRACT.md` §1, row "catalog"),
//!   which say what e4 has in e4's own words: a collection, a declared
//!   field, an index family, an edge type, a graph context;
//! * the `pg_catalog`, `information_schema`, PostGIS `geometry_columns` and
//!   `spatial_ref_sys` VIEWS, which are those same rows under PostgreSQL's
//!   column names and type OIDs, because that is what a driver reads.
//!
//! **The bound, stated.** Every relation's row count is a function of the
//! CATALOG, never of the rows in it: `db_tables` is one row per collection,
//! `db_columns` one per declared field, `db_indexes` one per index,
//! `db_contexts` one per interned context, and `db_edges` one per distinct
//! `(context, edge type, from collection, to collection)` that written edges
//! produce -- the one relation whose build walks a keyspace, capped at
//! [`EDGE_SHAPE_SEEKS`] descents and reported as truncated rather than
//! silently short. `db_tables.rows` is the LIVE ROW COUNT record
//! (`Database::row_count`), one point read per collection, not a walk.
//!
//! **Why the version string starts with `PostgreSQL`.** Drivers PARSE it.
//! pgjdbc reads the major number out of `SELECT version()` to decide which
//! protocol features and which catalog queries to use; psql prints it and
//! compares it against its own; QGIS shows it as the provider description.
//! A string that does not begin `PostgreSQL <major>.<minor>` is either
//! rejected or silently degrades the client to its oldest behaviour, so the
//! honest part -- which engine actually answered -- goes in the parenthesis
//! that PostgreSQL itself uses for the build: `PostgreSQL 16.0 (sekejap
//! 0.17.0)`.

use super::{SqlError, SqlResult2, SqlValue};
use sekejap_core::collections::{
    CollectionId, Database, DefaultValue, GraphContextId, IndexFamily, IndexState,
};
use sekejap_core::Kind;

/// How many descents `db_edges` may pay to derive the graph shape from
/// written edges. See `Database::edge_shape`: one descent per distinct
/// `(source entity, context, type, destination collection)`, so this is the
/// number of such runs the relation is willing to enumerate before it stops
/// and says it stopped.
pub const EDGE_SHAPE_SEEKS: usize = 65_536;

/// The one schema this engine has. `docs/lang/QL_CONTRACT.md` §2 places
/// `CREATE SCHEMA` and a real schema segment in Tier 2 (p2-schema-segment);
/// until that exists every collection is in `public`, and a statement that
/// writes `public.t` means `t`.
pub const PUBLIC: &str = "public";
/// The catalog name a driver sees. e4 has one database per file and no
/// cluster, so this is a constant rather than a lookup.
pub const DATABASE: &str = "sekejap";
/// The role name every answer reports. There is no authentication here: the
/// process that opened the file is the only user there is.
pub const USER: &str = "postgres";

/// The `SELECT version()` string. See the module note on why it starts with
/// `PostgreSQL`.
pub const VERSION: &str = concat!("PostgreSQL 16.0 (sekejap ", env!("CARGO_PKG_VERSION"), ")");

/// `db_version()`: the same fact without the PostgreSQL costume, for a
/// caller that is not a driver.
pub const DB_VERSION: &str = concat!("sekejap ", env!("CARGO_PKG_VERSION"));

// ── PostgreSQL type OIDs ─────────────────────────────────────────────────
//
// The numbers are PostgreSQL's own and are frozen in its catalog, so they
// are written here as constants rather than looked up. Only the two
// EXTENSION types have no fixed number: PostGIS and pgvector are assigned
// their OIDs at CREATE EXTENSION time in a real cluster, and clients detect
// both by type NAME, so any stable value works. These two are ours and are
// stated so a wire layer and a test can agree on them.

pub const OID_BOOL: i32 = 16;
pub const OID_BYTEA: i32 = 17;
pub const OID_CHAR: i32 = 18;
pub const OID_NAME: i32 = 19;
pub const OID_INT8: i32 = 20;
pub const OID_INT2: i32 = 21;
pub const OID_INT4: i32 = 23;
pub const OID_TEXT: i32 = 25;
pub const OID_OID: i32 = 26;
pub const OID_FLOAT4: i32 = 700;
pub const OID_FLOAT8: i32 = 701;
pub const OID_DATE: i32 = 1082;
pub const OID_TIMESTAMPTZ: i32 = 1184;
pub const OID_JSONB: i32 = 3802;
/// PostGIS `geometry`, assigned per install in a real cluster.
pub const OID_GEOMETRY: i32 = 18_000;
/// pgvector `vector`, assigned per install in a real cluster.
pub const OID_VECTOR: i32 = 18_001;

/// The namespace OID `public` is given in a stock PostgreSQL cluster.
const NS_PUBLIC: i64 = 2200;
/// The namespace OID `pg_catalog` is given in a stock PostgreSQL cluster.
const NS_PG_CATALOG: i64 = 11;
/// The namespace OID `information_schema` gets in a stock cluster. Not
/// fixed by PostgreSQL (it is created by a script), but stable here.
const NS_INFORMATION_SCHEMA: i64 = 13_000;
/// The owner every object reports. `10` is the bootstrap superuser.
const OWNER: i64 = 10;

/// The `pg_type` rows this engine has: one per e4 `Kind`, plus the two
/// extension types, in OID order.
///
/// `(oid, typname, typcategory, typlen, namespace oid)`. `typlen` is -1 for
/// a variable-length type, which is what a driver checks before it trusts a
/// fixed width.
pub const PG_TYPES: &[(i32, &str, &str, i32, i64)] = &[
    (OID_BOOL, "bool", "B", 1, NS_PG_CATALOG),
    (OID_BYTEA, "bytea", "U", -1, NS_PG_CATALOG),
    (OID_INT8, "int8", "N", 8, NS_PG_CATALOG),
    (OID_TEXT, "text", "S", -1, NS_PG_CATALOG),
    (OID_FLOAT8, "float8", "N", 8, NS_PG_CATALOG),
    (OID_DATE, "date", "D", 4, NS_PG_CATALOG),
    (OID_TIMESTAMPTZ, "timestamptz", "D", 8, NS_PG_CATALOG),
    (OID_JSONB, "jsonb", "U", -1, NS_PG_CATALOG),
    // The extension types live in `public`, exactly where PostGIS and
    // pgvector put theirs, because that is where a client looks for them.
    (OID_GEOMETRY, "geometry", "U", -1, NS_PUBLIC),
    (OID_VECTOR, "vector", "U", -1, NS_PUBLIC),
];

/// The PostgreSQL type a declared e4 column is reported as: `(oid, typname)`.
///
/// `TIMESTAMPTZ` and `DATE` are both `Kind::Int` (`docs/lang/QL_CONTRACT.md`
/// §5 deviation 8: UTC microseconds), so the DECLARED spelling decides
/// between `int8`, `timestamptz` and `date`; `declared` is the catalog's own
/// record of that spelling and is empty for a collection that never had one.
pub fn pg_type_of(kind: &Kind, declared: Option<&str>) -> (i32, &'static str) {
    if let Some(declared) = declared {
        let upper = declared.to_ascii_uppercase();
        if upper.starts_with("TIMESTAMP") {
            return (OID_TIMESTAMPTZ, "timestamptz");
        }
        if upper.starts_with("DATE") {
            return (OID_DATE, "date");
        }
    }
    match kind {
        Kind::Text => (OID_TEXT, "text"),
        Kind::Int => (OID_INT8, "int8"),
        Kind::Real => (OID_FLOAT8, "float8"),
        Kind::Bool => (OID_BOOL, "bool"),
        Kind::Json => (OID_JSONB, "jsonb"),
        Kind::Geo | Kind::Point => (OID_GEOMETRY, "geometry"),
        Kind::Vector(_) => (OID_VECTOR, "vector"),
    }
}

/// The e4 word for a `Kind`, as `db_columns.kind` prints it.
fn kind_word(kind: &Kind) -> String {
    match kind {
        Kind::Text => "Text".into(),
        Kind::Int => "Int".into(),
        Kind::Real => "Real".into(),
        Kind::Bool => "Bool".into(),
        Kind::Json => "Json".into(),
        Kind::Geo => "Geo".into(),
        Kind::Point => "Point".into(),
        Kind::Vector(n) => format!("Vector({n})"),
    }
}

/// The word `db_indexes.family` prints, which is the word `CREATE INDEX`
/// writes for the family (`USING btree`, `gin`, `gist`, `exact`,
/// `quantized`, `vamana`), plus `geometry` for the geometry family.
fn family_word(family: IndexFamily) -> &'static str {
    match family {
        IndexFamily::Scalar => "btree",
        IndexFamily::Text => "gin",
        IndexFamily::SpatialPoint => "gist",
        IndexFamily::SpatialGeometry => "geometry",
        IndexFamily::ExactVector => "exact",
        IndexFamily::QuantizedVector => "quantized",
        IndexFamily::VamanaGraph => "vamana",
    }
}

fn state_word(state: &IndexState) -> String {
    match state {
        IndexState::Building { after } => format!("building (after {after})"),
        IndexState::Ready => "ready".into(),
        IndexState::Dropping => "dropping".into(),
    }
}

/// A stable synthetic OID for a named object.
///
/// PostgreSQL hands out OIDs from a counter; e4 has no such counter and no
/// place to keep one without a format change, so an object's OID is a hash
/// of its name, taken into the user-object range (>= 16,384) that a client
/// treats as "not a system object". FNV-1a, the same function e1's shim
/// used, so the two surfaces agree on what a collection's OID is.
///
/// This is the one number here that is DERIVED rather than read: two names
/// that collide would report one OID for two objects. The range is 2e9 wide
/// and a database holds thousands of collections at most, so the collision
/// probability is under 1e-5 at 10,000 collections; a client that follows
/// the OID back reaches a relation by name anyway.
pub fn object_oid(name: &str) -> i64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    16_385 + (h % 2_000_000_000) as i64
}

// ── the relation directory ───────────────────────────────────────────────

/// One column of a catalog relation: the name a client reads it by, and the
/// PostgreSQL type OID a wire layer must describe it with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatalogColumn {
    pub name: &'static str,
    pub oid: i32,
}

const fn c(name: &'static str, oid: i32) -> CatalogColumn {
    CatalogColumn { name, oid }
}

/// One virtual relation: where a statement writes it, and what it answers
/// with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatalogRelation {
    /// The schema a client may qualify it with. `""` for the `db_*` rows,
    /// which are e4's own and belong to no PostgreSQL schema.
    pub schema: &'static str,
    pub name: &'static str,
    pub columns: &'static [CatalogColumn],
}

impl CatalogRelation {
    /// The name a statement writes, qualified when it has a schema.
    pub fn written(&self) -> String {
        if self.schema.is_empty() {
            self.name.to_owned()
        } else {
            format!("{}.{}", self.schema, self.name)
        }
    }

    pub fn column_at(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|column| column.name.eq_ignore_ascii_case(name))
    }
}

// The `db_*` core rows.

const DB_TABLES: &[CatalogColumn] = &[
    c("name", OID_TEXT),
    c("id", OID_INT8),
    c("rows", OID_INT8),
    c("fields", OID_INT8),
];
const DB_COLUMNS: &[CatalogColumn] = &[
    c("table", OID_TEXT),
    c("name", OID_TEXT),
    c("kind", OID_TEXT),
    c("declared_type", OID_TEXT),
    c("position", OID_INT8),
    c("not_null", OID_BOOL),
    c("has_default", OID_BOOL),
];
const DB_INDEXES: &[CatalogColumn] = &[
    c("table", OID_TEXT),
    c("name", OID_TEXT),
    c("family", OID_TEXT),
    c("field", OID_TEXT),
    c("expression", OID_TEXT),
    c("state", OID_TEXT),
];
const DB_EDGES: &[CatalogColumn] = &[
    c("edge_type", OID_TEXT),
    c("from_table", OID_TEXT),
    c("to_table", OID_TEXT),
    c("context", OID_TEXT),
];
const DB_CONTEXTS: &[CatalogColumn] = &[c("name", OID_TEXT), c("id", OID_INT8)];

// `information_schema`. Column names and order are the standard's.

const IS_SCHEMATA: &[CatalogColumn] = &[
    c("catalog_name", OID_TEXT),
    c("schema_name", OID_TEXT),
    c("schema_owner", OID_TEXT),
    c("default_character_set_catalog", OID_TEXT),
    c("default_character_set_schema", OID_TEXT),
    c("default_character_set_name", OID_TEXT),
    c("sql_path", OID_TEXT),
];
const IS_TABLES: &[CatalogColumn] = &[
    c("table_catalog", OID_TEXT),
    c("table_schema", OID_TEXT),
    c("table_name", OID_TEXT),
    c("table_type", OID_TEXT),
    c("self_referencing_column_name", OID_TEXT),
    c("reference_generation", OID_TEXT),
    c("user_defined_type_catalog", OID_TEXT),
    c("user_defined_type_schema", OID_TEXT),
    c("user_defined_type_name", OID_TEXT),
    c("is_insertable_into", OID_TEXT),
    c("is_typed", OID_TEXT),
    c("commit_action", OID_TEXT),
];
const IS_COLUMNS: &[CatalogColumn] = &[
    c("table_catalog", OID_TEXT),
    c("table_schema", OID_TEXT),
    c("table_name", OID_TEXT),
    c("column_name", OID_TEXT),
    c("ordinal_position", OID_INT8),
    c("column_default", OID_TEXT),
    c("is_nullable", OID_TEXT),
    c("data_type", OID_TEXT),
    c("character_maximum_length", OID_INT8),
    c("numeric_precision", OID_INT8),
    c("numeric_scale", OID_INT8),
    c("datetime_precision", OID_INT8),
    c("udt_catalog", OID_TEXT),
    c("udt_schema", OID_TEXT),
    c("udt_name", OID_TEXT),
];
const IS_TABLE_CONSTRAINTS: &[CatalogColumn] = &[
    c("constraint_catalog", OID_TEXT),
    c("constraint_schema", OID_TEXT),
    c("constraint_name", OID_TEXT),
    c("table_catalog", OID_TEXT),
    c("table_schema", OID_TEXT),
    c("table_name", OID_TEXT),
    c("constraint_type", OID_TEXT),
    c("is_deferrable", OID_TEXT),
    c("initially_deferred", OID_TEXT),
    c("enforced", OID_TEXT),
];
const IS_KEY_COLUMN_USAGE: &[CatalogColumn] = &[
    c("constraint_catalog", OID_TEXT),
    c("constraint_schema", OID_TEXT),
    c("constraint_name", OID_TEXT),
    c("table_catalog", OID_TEXT),
    c("table_schema", OID_TEXT),
    c("table_name", OID_TEXT),
    c("column_name", OID_TEXT),
    c("ordinal_position", OID_INT8),
    c("position_in_unique_constraint", OID_INT8),
];

// `pg_catalog`. Each relation carries the columns a client actually reads,
// with PostgreSQL's own names and OIDs; `docs/dist/PG_SURFACE.md` lists them
// and says which of the real relation's columns are NOT here.

const PG_NAMESPACE: &[CatalogColumn] = &[
    c("oid", OID_OID),
    c("nspname", OID_NAME),
    c("nspowner", OID_OID),
    c("nspacl", OID_TEXT),
];
const PG_CLASS: &[CatalogColumn] = &[
    c("oid", OID_OID),
    c("relname", OID_NAME),
    c("relnamespace", OID_OID),
    c("reltype", OID_OID),
    c("relowner", OID_OID),
    c("relam", OID_OID),
    c("relpages", OID_INT4),
    c("reltuples", OID_FLOAT4),
    c("reltoastrelid", OID_OID),
    c("relhasindex", OID_BOOL),
    c("relisshared", OID_BOOL),
    c("relpersistence", OID_CHAR),
    c("relkind", OID_CHAR),
    c("relnatts", OID_INT2),
    c("relchecks", OID_INT2),
    c("relhasrules", OID_BOOL),
    c("relhastriggers", OID_BOOL),
    c("relhassubclass", OID_BOOL),
    c("relrowsecurity", OID_BOOL),
    c("relispopulated", OID_BOOL),
    c("relreplident", OID_CHAR),
    c("relispartition", OID_BOOL),
    c("reltablespace", OID_OID),
    c("relacl", OID_TEXT),
    c("reloptions", OID_TEXT),
];
const PG_ATTRIBUTE: &[CatalogColumn] = &[
    c("attrelid", OID_OID),
    c("attname", OID_NAME),
    c("atttypid", OID_OID),
    c("attstattarget", OID_INT4),
    c("attlen", OID_INT2),
    c("attnum", OID_INT2),
    c("attndims", OID_INT4),
    c("atttypmod", OID_INT4),
    c("attbyval", OID_BOOL),
    c("attstorage", OID_CHAR),
    c("attalign", OID_CHAR),
    c("attnotnull", OID_BOOL),
    c("atthasdef", OID_BOOL),
    c("attidentity", OID_CHAR),
    c("attgenerated", OID_CHAR),
    c("attisdropped", OID_BOOL),
    c("attislocal", OID_BOOL),
    c("attinhcount", OID_INT4),
    c("attcollation", OID_OID),
];
const PG_TYPE: &[CatalogColumn] = &[
    c("oid", OID_OID),
    c("typname", OID_NAME),
    c("typnamespace", OID_OID),
    c("typowner", OID_OID),
    c("typlen", OID_INT2),
    c("typbyval", OID_BOOL),
    c("typtype", OID_CHAR),
    c("typcategory", OID_CHAR),
    c("typispreferred", OID_BOOL),
    c("typisdefined", OID_BOOL),
    c("typdelim", OID_CHAR),
    c("typrelid", OID_OID),
    c("typelem", OID_OID),
    c("typarray", OID_OID),
    c("typnotnull", OID_BOOL),
    c("typbasetype", OID_OID),
    c("typtypmod", OID_INT4),
    c("typndims", OID_INT4),
];
const PG_INDEX: &[CatalogColumn] = &[
    c("indexrelid", OID_OID),
    c("indrelid", OID_OID),
    c("indnatts", OID_INT2),
    c("indnkeyatts", OID_INT2),
    c("indisunique", OID_BOOL),
    c("indisprimary", OID_BOOL),
    c("indisexclusion", OID_BOOL),
    c("indimmediate", OID_BOOL),
    c("indisclustered", OID_BOOL),
    c("indisvalid", OID_BOOL),
    c("indisready", OID_BOOL),
    c("indislive", OID_BOOL),
    c("indkey", OID_TEXT),
];
const PG_INDEXES: &[CatalogColumn] = &[
    c("schemaname", OID_NAME),
    c("tablename", OID_NAME),
    c("indexname", OID_NAME),
    c("tablespace", OID_NAME),
    c("indexdef", OID_TEXT),
];
const PG_DESCRIPTION: &[CatalogColumn] = &[
    c("objoid", OID_OID),
    c("classoid", OID_OID),
    c("objsubid", OID_INT4),
    c("description", OID_TEXT),
];
const PG_CONSTRAINT: &[CatalogColumn] = &[
    c("oid", OID_OID),
    c("conname", OID_NAME),
    c("connamespace", OID_OID),
    c("contype", OID_CHAR),
    c("condeferrable", OID_BOOL),
    c("condeferred", OID_BOOL),
    c("convalidated", OID_BOOL),
    c("conrelid", OID_OID),
    c("contypid", OID_OID),
    c("conindid", OID_OID),
    c("conkey", OID_TEXT),
];
const PG_TABLES: &[CatalogColumn] = &[
    c("schemaname", OID_NAME),
    c("tablename", OID_NAME),
    c("tableowner", OID_NAME),
    c("tablespace", OID_NAME),
    c("hasindexes", OID_BOOL),
    c("hasrules", OID_BOOL),
    c("hastriggers", OID_BOOL),
    c("rowsecurity", OID_BOOL),
];

// PostGIS. Minimal but exact: the two relations a spatial client cannot load
// a layer without.

const GEOMETRY_COLUMNS: &[CatalogColumn] = &[
    c("f_table_catalog", OID_TEXT),
    c("f_table_schema", OID_TEXT),
    c("f_table_name", OID_TEXT),
    c("f_geometry_column", OID_TEXT),
    c("coord_dimension", OID_INT8),
    c("srid", OID_INT8),
    c("type", OID_TEXT),
];
const SPATIAL_REF_SYS: &[CatalogColumn] = &[
    c("srid", OID_INT8),
    c("auth_name", OID_TEXT),
    c("auth_srid", OID_INT8),
    c("srtext", OID_TEXT),
    c("proj4text", OID_TEXT),
];

/// Every virtual relation this surface answers, in the order
/// `docs/dist/PG_SURFACE.md` lists them.
pub const RELATIONS: &[CatalogRelation] = &[
    CatalogRelation { schema: "", name: "db_tables", columns: DB_TABLES },
    CatalogRelation { schema: "", name: "db_columns", columns: DB_COLUMNS },
    CatalogRelation { schema: "", name: "db_indexes", columns: DB_INDEXES },
    CatalogRelation { schema: "", name: "db_edges", columns: DB_EDGES },
    CatalogRelation { schema: "", name: "db_contexts", columns: DB_CONTEXTS },
    CatalogRelation { schema: "information_schema", name: "schemata", columns: IS_SCHEMATA },
    CatalogRelation { schema: "information_schema", name: "tables", columns: IS_TABLES },
    CatalogRelation { schema: "information_schema", name: "columns", columns: IS_COLUMNS },
    CatalogRelation {
        schema: "information_schema",
        name: "table_constraints",
        columns: IS_TABLE_CONSTRAINTS,
    },
    CatalogRelation {
        schema: "information_schema",
        name: "key_column_usage",
        columns: IS_KEY_COLUMN_USAGE,
    },
    CatalogRelation { schema: "pg_catalog", name: "pg_namespace", columns: PG_NAMESPACE },
    CatalogRelation { schema: "pg_catalog", name: "pg_class", columns: PG_CLASS },
    CatalogRelation { schema: "pg_catalog", name: "pg_attribute", columns: PG_ATTRIBUTE },
    CatalogRelation { schema: "pg_catalog", name: "pg_type", columns: PG_TYPE },
    CatalogRelation { schema: "pg_catalog", name: "pg_index", columns: PG_INDEX },
    CatalogRelation { schema: "pg_catalog", name: "pg_indexes", columns: PG_INDEXES },
    CatalogRelation { schema: "pg_catalog", name: "pg_description", columns: PG_DESCRIPTION },
    CatalogRelation { schema: "pg_catalog", name: "pg_constraint", columns: PG_CONSTRAINT },
    CatalogRelation { schema: "pg_catalog", name: "pg_tables", columns: PG_TABLES },
    CatalogRelation { schema: "public", name: "geometry_columns", columns: GEOMETRY_COLUMNS },
    CatalogRelation { schema: "public", name: "spatial_ref_sys", columns: SPATIAL_REF_SYS },
];

/// The relation a statement's `FROM` names, if it names one.
///
/// A name may be written bare or qualified. `pg_catalog` and
/// `information_schema` are the two schemas PostgreSQL puts on every
/// session's `search_path` implicitly, so their relations resolve bare as
/// well as qualified -- except `information_schema.tables` and
/// `.columns`, whose bare spellings are ORDINARY names a collection may
/// have, and which therefore resolve only when the statement qualifies them.
/// `public.t` resolves to the collection `t`, because `public` is the one
/// schema there is.
pub fn relation(name: &str) -> Option<&'static CatalogRelation> {
    let (schema, bare) = match name.split_once('.') {
        Some((schema, bare)) => (Some(schema), bare),
        None => (None, name),
    };
    RELATIONS.iter().find(|r| {
        if !r.name.eq_ignore_ascii_case(bare) {
            return false;
        }
        match schema {
            Some(schema) => r.schema.eq_ignore_ascii_case(schema),
            // Bare: everything but the two `information_schema` relations
            // whose names a user collection can also have.
            None => !matches!(
                (r.schema, r.name),
                ("information_schema", "tables") | ("information_schema", "columns")
            ),
        }
    })
}

/// The `pg_catalog` relations this surface does NOT provide, with the reason.
///
/// They are listed rather than answered empty: an empty answer from
/// `pg_settings` reads as "this server has no settings", which is false, and
/// the eighth law of `docs/core/FOUNDATION_TEST_STANDARD.md` is that a
/// construct with no atomic is refused with a named reason. `refuse.rs`
/// carries the rows; this list is what `docs/dist/PG_SURFACE.md` prints and
/// what the test enumerates.
pub const NOT_PROVIDED: &[(&str, &str)] = &[
    ("pg_proc", "e4 has no function catalog: the §4.1/§4.2 functions are compiled by `lang`, not registered rows."),
    ("pg_settings", "e4 has no GUC table; the client GUCs a driver sets are accepted as notices and nothing else is settable."),
    ("pg_roles", "e4 has no authentication and no role catalog: the process that opened the file is the only user."),
    ("pg_database", "e4 is one database per file; there is no cluster to list."),
    ("pg_enum", "e4 has no enum types: a `Kind` is one of eight and none of them is user-defined."),
    ("pg_operator", "operators are compiled by `lang` against the index families; there is no operator catalog."),
    ("pg_am", "index families are `IndexFamily`, a closed set in the collection catalog, not access-method rows."),
    ("pg_trigger", "triggers are Tier 3: no atomic."),
    ("pg_rewrite", "user views and rules are Tier 3: no atomic."),
    ("pg_stat_activity", "there is no connection table: a connection is a process here."),
];

// ── building the rows ────────────────────────────────────────────────────

fn text(value: impl Into<String>) -> SqlValue {
    SqlValue::Text(value.into())
}

fn int(value: i64) -> SqlValue {
    SqlValue::Int(value)
}

const NULL: SqlValue = SqlValue::Null;

/// Everything one build reads from the catalog ONCE, so a relation that
/// needs collections and their fields and their indexes pays each reader a
/// single time.
struct Snapshot {
    /// `(name, id, rows, fields, indexes)` per collection, in catalog order.
    tables: Vec<Table>,
    /// True when `db_edges` stopped at its cap rather than at the end.
    edges_truncated: bool,
    edges: Vec<(String, String, String, String)>,
    contexts: Vec<(String, i64)>,
}

struct Table {
    name: String,
    id: CollectionId,
    rows: Option<u64>,
    /// `(field, kind, declared, not_null, has_default, default spelling)`.
    fields: Vec<Field>,
    /// `(name, family, field, expression, state, unique)`.
    indexes: Vec<Index>,
}

struct Field {
    name: String,
    kind: Kind,
    /// The declared SQL spelling the CATALOG recorded, which it does only
    /// for a column whose `Kind` does not name it (`TIMESTAMPTZ`, `DATE`:
    /// both `Kind::Int`). `None` for every other column, and for every
    /// collection created through the untyped `create_collection`.
    declared: Option<String>,
    not_null: bool,
    default: Option<String>,
}

impl Field {
    /// The SQL type this column is reported as: the catalog's recorded
    /// spelling when it has one, otherwise the `Kind`'s own. Never empty, so
    /// a client that reads a type always gets one it can write back.
    fn declared_sql(&self) -> String {
        self.declared
            .clone()
            .unwrap_or_else(|| declared_of(&self.kind))
    }
}

/// The SQL spelling of a `Kind`, for a column whose declared spelling the
/// catalog did not record.
pub fn declared_of(kind: &Kind) -> String {
    match kind {
        Kind::Text => "TEXT".into(),
        Kind::Int => "BIGINT".into(),
        Kind::Real => "DOUBLE PRECISION".into(),
        Kind::Bool => "BOOLEAN".into(),
        Kind::Json => "JSONB".into(),
        Kind::Geo => "GEOMETRY".into(),
        Kind::Point => "GEOMETRY(Point, 4326)".into(),
        Kind::Vector(n) => format!("VECTOR({n})"),
    }
}

struct Index {
    name: String,
    family: IndexFamily,
    field: String,
    /// The expression as it is written without its field (`lower`,
    /// `->>'status'`), which is the `db_indexes` column.
    expression: Option<String>,
    /// The whole target a `CREATE INDEX` writes (`lower(title)`,
    /// `(meta->>'status')`), which is what `pg_indexes.indexdef` needs: the
    /// two differ for an expression whose operator is infix.
    target: String,
    state: IndexState,
    unique: bool,
}

fn default_spelling(value: &DefaultValue) -> String {
    match value {
        DefaultValue::Now => "now()".into(),
        DefaultValue::Uuid4 => "uuid4()".into(),
        DefaultValue::Uuid5 { .. } => "uuid5(namespace, name)".into(),
    }
}

/// Read the catalog once. `graph` is false for every relation that has no
/// edge column, so the ordinary catalog views never pay the edge probe.
fn snapshot(db: &Database, graph: bool) -> SqlResult2<Snapshot> {
    let mut tables = Vec::new();
    for name in db.list_collections().map_err(SqlError::from)? {
        let Some(id) = db.collection(&name).map_err(SqlError::from)? else {
            // A name whose catalog entry went away between the two reads is
            // not an error: the listing is a walk, not a transaction.
            continue;
        };
        let info = db.collection_info(id).map_err(SqlError::from)?;
        // The EXTERNAL KEY is column one of every collection. `Database` keeps
        // it as the layout's own first field and `collection_info` takes it
        // out of the list, because it is not a column a `CREATE TABLE`
        // declared -- but it IS a column of every row, it is the PRIMARY KEY
        // every constraint view names, and a client that cannot see it
        // cannot map the key it is told about onto anything. So the catalog
        // relations put it back, first, NOT NULL and with no default.
        let mut fields = vec![Field {
            name: super::KEY_COLUMN.to_owned(),
            kind: Kind::Text,
            declared: Some("TEXT".to_owned()),
            not_null: true,
            default: None,
        }];
        fields.extend(info
            .layout
            .fields
            .iter()
            .map(|(field, kind)| {
                let rule = info.rules.iter().find(|(name, _)| name == field);
                Field {
                    name: field.clone(),
                    kind: kind.clone(),
                    declared: info
                        .declared
                        .iter()
                        .find(|(name, _)| name == field)
                        .map(|(_, declared)| declared.clone()),
                    not_null: rule.is_some_and(|(_, rule)| rule.not_null),
                    default: rule
                        .and_then(|(_, rule)| rule.default.as_ref())
                        .map(default_spelling),
                }
            }));
        let indexes = db
            .list_indexes(id)
            .map_err(SqlError::from)?
            .into_iter()
            .map(|index| Index {
                target: index
                    .expression
                    .as_ref()
                    .map_or_else(|| index.field.clone(), |e| e.target(&index.field)),
                expression: index.expression.map(|e| e.written()),
                name: index.name,
                family: index.family,
                field: index.field,
                state: index.state,
                unique: index.unique,
            })
            .collect();
        tables.push(Table {
            rows: db.row_count(id).map_err(SqlError::from)?,
            name,
            id,
            fields,
            indexes,
        });
    }
    let mut edges = Vec::new();
    let mut contexts = Vec::new();
    let mut edges_truncated = false;
    if graph {
        let (types, named_contexts) = db.graph_names().map_err(SqlError::from)?;
        // The base graph has no descriptor and no name, so a listing adds it.
        contexts.push((BASE_CONTEXT.to_owned(), 0));
        for (id, name) in &named_contexts {
            contexts.push((name.clone(), id.0 as i64));
        }
        let (shape, truncated) = db.edge_shape(EDGE_SHAPE_SEEKS).map_err(SqlError::from)?;
        edges_truncated = truncated;
        let table_of = |id: CollectionId| -> String {
            tables
                .iter()
                .find(|t| t.id == id)
                .map_or_else(|| format!("collection {}", id.0), |t| t.name.clone())
        };
        for row in shape {
            let edge_type = types
                .iter()
                .find(|(id, _)| *id == row.edge_type)
                .map_or_else(|| format!("type {}", row.edge_type.0), |(_, n)| n.clone());
            let context = if row.context == GraphContextId::BASE {
                BASE_CONTEXT.to_owned()
            } else {
                named_contexts
                    .iter()
                    .find(|(id, _)| *id == row.context)
                    .map_or_else(|| format!("context {}", row.context.0), |(_, n)| n.clone())
            };
            edges.push((edge_type, table_of(row.from), table_of(row.to), context));
        }
    }
    Ok(Snapshot {
        tables,
        edges,
        edges_truncated,
        contexts,
    })
}

/// The name the base graph is listed under. `GRAPH_CONTRACT` 3.1: a context
/// is a named graph and "no context" is the base graph, which has no
/// descriptor -- so the listing gives it this spelling rather than an empty
/// string a client would read as a missing value.
pub const BASE_CONTEXT: &str = "(base graph)";

/// The relations whose rows need the GRAPH name dictionary or the edge
/// keyspace probe.
fn needs_graph(relation: &CatalogRelation) -> bool {
    matches!(relation.name, "db_edges" | "db_contexts")
}

/// Build one relation's rows from the catalog.
///
/// Returns the rows and, when the build had to stop at a stated cap, the
/// notice that says so -- `db_edges` is the only relation that can, and it
/// says how many descents it paid rather than returning a short answer that
/// looks complete.
pub fn build(db: &Database, relation: &CatalogRelation) -> SqlResult2<(Vec<Vec<SqlValue>>, Option<String>)> {
    let snapshot = snapshot(db, needs_graph(relation))?;
    let mut notice = None;
    let rows = match (relation.schema, relation.name) {
        ("", "db_tables") => snapshot
            .tables
            .iter()
            .map(|t| {
                vec![
                    text(&t.name),
                    int(i64::from(t.id.0)),
                    t.rows.map_or(NULL, |n| int(n as i64)),
                    int(t.fields.len() as i64),
                ]
            })
            .collect(),
        ("", "db_columns") => snapshot
            .tables
            .iter()
            .flat_map(|t| {
                t.fields.iter().enumerate().map(move |(at, f)| {
                    vec![
                        text(&t.name),
                        text(&f.name),
                        text(kind_word(&f.kind)),
                        text(f.declared_sql()),
                        int(at as i64 + 1),
                        SqlValue::Bool(f.not_null),
                        SqlValue::Bool(f.default.is_some()),
                    ]
                })
            })
            .collect(),
        ("", "db_indexes") => snapshot
            .tables
            .iter()
            .flat_map(|t| {
                t.indexes.iter().map(move |i| {
                    vec![
                        text(&t.name),
                        text(&i.name),
                        text(family_word(i.family)),
                        text(&i.field),
                        i.expression.as_ref().map_or(NULL, text),
                        text(state_word(&i.state)),
                    ]
                })
            })
            .collect(),
        ("", "db_edges") => {
            if snapshot.edges_truncated {
                notice = Some(format!(
                    "db_edges stopped at {EDGE_SHAPE_SEEKS} descents of the edge keyspace: the graph shape is DERIVED from written edges (GRAPH_CONTRACT 2.5) and the rows below are the triples found up to that cap, not every triple in the graph"
                ));
            }
            snapshot
                .edges
                .iter()
                .map(|(edge_type, from, to, context)| {
                    vec![text(edge_type), text(from), text(to), text(context)]
                })
                .collect()
        }
        ("", "db_contexts") => snapshot
            .contexts
            .iter()
            .map(|(name, id)| vec![text(name), int(*id)])
            .collect(),

        ("information_schema", "schemata") => [PUBLIC, "pg_catalog", "information_schema"]
            .iter()
            .map(|schema| {
                vec![
                    text(DATABASE),
                    text(*schema),
                    text(USER),
                    NULL,
                    NULL,
                    text("UTF8"),
                    NULL,
                ]
            })
            .collect(),
        ("information_schema", "tables") => snapshot
            .tables
            .iter()
            .map(|t| {
                vec![
                    text(DATABASE),
                    text(PUBLIC),
                    text(&t.name),
                    text("BASE TABLE"),
                    NULL,
                    NULL,
                    NULL,
                    NULL,
                    NULL,
                    text("YES"),
                    text("NO"),
                    NULL,
                ]
            })
            .collect(),
        ("information_schema", "columns") => snapshot
            .tables
            .iter()
            .flat_map(|t| {
                t.fields.iter().enumerate().map(move |(at, f)| {
                    let (_, typname) = pg_type_of(&f.kind, f.declared.as_deref());
                    vec![
                        text(DATABASE),
                        text(PUBLIC),
                        text(&t.name),
                        text(&f.name),
                        int(at as i64 + 1),
                        f.default.as_ref().map_or(NULL, text),
                        text(if f.not_null { "NO" } else { "YES" }),
                        text(data_type(typname)),
                        NULL,
                        NULL,
                        NULL,
                        NULL,
                        text(DATABASE),
                        text(udt_schema(typname)),
                        text(typname),
                    ]
                })
            })
            .collect(),
        ("information_schema", "table_constraints") => snapshot
            .tables
            .iter()
            .map(|t| {
                vec![
                    text(DATABASE),
                    text(PUBLIC),
                    text(format!("{}_pkey", t.name)),
                    text(DATABASE),
                    text(PUBLIC),
                    text(&t.name),
                    text("PRIMARY KEY"),
                    text("NO"),
                    text("NO"),
                    text("YES"),
                ]
            })
            .collect(),
        ("information_schema", "key_column_usage") => snapshot
            .tables
            .iter()
            .map(|t| {
                vec![
                    text(DATABASE),
                    text(PUBLIC),
                    text(format!("{}_pkey", t.name)),
                    text(DATABASE),
                    text(PUBLIC),
                    text(&t.name),
                    text(super::KEY_COLUMN),
                    int(1),
                    NULL,
                ]
            })
            .collect(),

        ("pg_catalog", "pg_namespace") => [
            (NS_PUBLIC, PUBLIC),
            (NS_PG_CATALOG, "pg_catalog"),
            (NS_INFORMATION_SCHEMA, "information_schema"),
        ]
        .iter()
        .map(|(oid, name)| vec![int(*oid), text(*name), int(OWNER), NULL])
        .collect(),
        ("pg_catalog", "pg_class") => snapshot
            .tables
            .iter()
            .map(|t| {
                vec![
                    int(object_oid(&t.name)),
                    text(&t.name),
                    int(NS_PUBLIC),
                    int(0),
                    int(OWNER),
                    int(0),
                    int(0),
                    SqlValue::Float(t.rows.unwrap_or(0) as f64),
                    int(0),
                    SqlValue::Bool(!t.indexes.is_empty()),
                    SqlValue::Bool(false),
                    text("p"),
                    text("r"),
                    int(t.fields.len() as i64),
                    int(0),
                    SqlValue::Bool(false),
                    SqlValue::Bool(false),
                    SqlValue::Bool(false),
                    SqlValue::Bool(false),
                    SqlValue::Bool(true),
                    text("d"),
                    SqlValue::Bool(false),
                    int(0),
                    NULL,
                    NULL,
                ]
            })
            .collect(),
        ("pg_catalog", "pg_attribute") => snapshot
            .tables
            .iter()
            .flat_map(|t| {
                let relid = object_oid(&t.name);
                t.fields.iter().enumerate().map(move |(at, f)| {
                    let (oid, _) = pg_type_of(&f.kind, f.declared.as_deref());
                    vec![
                        int(relid),
                        text(&f.name),
                        int(i64::from(oid)),
                        int(-1),
                        int(i64::from(type_len(oid))),
                        int(at as i64 + 1),
                        int(0),
                        int(-1),
                        SqlValue::Bool(type_len(oid) > 0),
                        text(if type_len(oid) > 0 { "p" } else { "x" }),
                        text("i"),
                        SqlValue::Bool(f.not_null),
                        SqlValue::Bool(f.default.is_some()),
                        text(""),
                        text(""),
                        SqlValue::Bool(false),
                        SqlValue::Bool(true),
                        int(0),
                        int(0),
                    ]
                })
            })
            .collect(),
        ("pg_catalog", "pg_type") => PG_TYPES
            .iter()
            .map(|(oid, name, category, len, namespace)| {
                vec![
                    int(i64::from(*oid)),
                    text(*name),
                    int(*namespace),
                    int(OWNER),
                    int(i64::from(*len)),
                    SqlValue::Bool(*len > 0),
                    text("b"),
                    text(*category),
                    SqlValue::Bool(false),
                    SqlValue::Bool(true),
                    text(","),
                    int(0),
                    int(0),
                    int(0),
                    SqlValue::Bool(false),
                    int(0),
                    int(-1),
                    int(0),
                ]
            })
            .collect(),
        ("pg_catalog", "pg_index") => snapshot
            .tables
            .iter()
            .flat_map(|t| {
                let relid = object_oid(&t.name);
                t.indexes.iter().map(move |i| {
                    vec![
                        int(object_oid(&i.name)),
                        int(relid),
                        int(1),
                        int(1),
                        SqlValue::Bool(i.unique),
                        SqlValue::Bool(false),
                        SqlValue::Bool(false),
                        SqlValue::Bool(true),
                        SqlValue::Bool(false),
                        SqlValue::Bool(i.state == IndexState::Ready),
                        SqlValue::Bool(i.state == IndexState::Ready),
                        SqlValue::Bool(i.state != IndexState::Dropping),
                        text(index_attnum(t, i)),
                    ]
                })
            })
            .collect(),
        ("pg_catalog", "pg_indexes") => snapshot
            .tables
            .iter()
            .flat_map(|t| {
                t.indexes
                    .iter()
                    .map(move |i| {
                        vec![
                            text(PUBLIC),
                            text(&t.name),
                            text(&i.name),
                            NULL,
                            text(index_def(&t.name, i)),
                        ]
                    })
            })
            .collect(),
        // Empty by construction: e4 records no comment on any object, and an
        // empty `pg_description` is the true answer rather than a stand-in.
        ("pg_catalog", "pg_description") => Vec::new(),
        ("pg_catalog", "pg_constraint") => snapshot
            .tables
            .iter()
            .map(|t| {
                vec![
                    int(object_oid(&format!("{}_pkey", t.name))),
                    text(format!("{}_pkey", t.name)),
                    int(NS_PUBLIC),
                    text("p"),
                    SqlValue::Bool(false),
                    SqlValue::Bool(false),
                    SqlValue::Bool(true),
                    int(object_oid(&t.name)),
                    int(0),
                    int(0),
                    text("1"),
                ]
            })
            .collect(),
        ("pg_catalog", "pg_tables") => snapshot
            .tables
            .iter()
            .map(|t| {
                vec![
                    text(PUBLIC),
                    text(&t.name),
                    text(USER),
                    NULL,
                    SqlValue::Bool(!t.indexes.is_empty()),
                    SqlValue::Bool(false),
                    SqlValue::Bool(false),
                    SqlValue::Bool(false),
                ]
            })
            .collect(),

        ("public", "geometry_columns") => snapshot
            .tables
            .iter()
            .flat_map(|t| {
                t.fields
                    .iter()
                    .filter(|f| matches!(f.kind, Kind::Geo | Kind::Point))
                    .map(move |f| {
                        vec![
                            text(DATABASE),
                            text(PUBLIC),
                            text(&t.name),
                            text(&f.name),
                            int(2),
                            int(SRID_WGS84),
                            text(match f.kind {
                                Kind::Point => "POINT",
                                _ => "GEOMETRY",
                            }),
                        ]
                    })
            })
            .collect(),
        // ONE row, exactly: 4326 is the only spatial reference system e4
        // has. `docs/core/SPATIAL_FUNCTIONS.md` pins the geometry model to
        // WGS84 lon/lat and the point index to `CRS_WGS84`, so a second row
        // here would name a system no stored geometry is in.
        ("public", "spatial_ref_sys") => vec![vec![
            int(SRID_WGS84),
            text("EPSG"),
            int(SRID_WGS84),
            text(WGS84_WKT),
            text("+proj=longlat +datum=WGS84 +no_defs"),
        ]],
        other => {
            return Err(SqlError::unsupported(format!(
                "no builder for the catalog relation `{}.{}`",
                other.0, other.1
            )))
        }
    };
    debug_assert!(
        rows.iter().all(|row| row.len() == relation.columns.len()),
        "`{}` built a row of the wrong width",
        relation.written()
    );
    Ok((rows, notice))
}

/// The only SRID e4 has. WGS84 lon/lat, which is what every stored geometry
/// is in and what the point index pins (`CRS_WGS84`).
pub const SRID_WGS84: i64 = 4326;

const WGS84_WKT: &str = "GEOGCS[\"WGS 84\",DATUM[\"WGS_1984\",SPHEROID[\"WGS 84\",6378137,298.257223563,AUTHORITY[\"EPSG\",\"7030\"]],AUTHORITY[\"EPSG\",\"6326\"]],PRIMEM[\"Greenwich\",0,AUTHORITY[\"EPSG\",\"8901\"]],UNIT[\"degree\",0.0174532925199433,AUTHORITY[\"EPSG\",\"9122\"]],AUTHORITY[\"EPSG\",\"4326\"]]";

/// `information_schema.columns.data_type`, which is the SQL standard's
/// spelling where there is one and the udt name where there is not.
fn data_type(typname: &str) -> &'static str {
    match typname {
        "text" => "text",
        "int8" => "bigint",
        "float8" => "double precision",
        "bool" => "boolean",
        "jsonb" => "jsonb",
        "timestamptz" => "timestamp with time zone",
        "date" => "date",
        "bytea" => "bytea",
        "geometry" => "USER-DEFINED",
        "vector" => "USER-DEFINED",
        _ => "text",
    }
}

/// The schema `information_schema.columns.udt_schema` reports: the
/// extension types live in `public`, everything else in `pg_catalog`.
fn udt_schema(typname: &str) -> &'static str {
    match typname {
        "geometry" | "vector" => PUBLIC,
        _ => "pg_catalog",
    }
}

fn type_len(oid: i32) -> i32 {
    PG_TYPES
        .iter()
        .find(|(id, ..)| *id == oid)
        .map_or(-1, |(_, _, _, len, _)| *len)
}

/// `pg_index.indkey`: the attribute numbers the index is over, space
/// separated, as PostgreSQL's `int2vector` prints. An e4 index spans exactly
/// one declared field, so this is one number.
fn index_attnum(table: &Table, index: &Index) -> String {
    table
        .fields
        .iter()
        .position(|f| f.name == index.field)
        .map_or_else(|| "0".to_owned(), |at| (at + 1).to_string())
}

/// `pg_indexes.indexdef`: the `CREATE INDEX` a statement would write to
/// build this index again. Built from the descriptor, so it cannot disagree
/// with what is there.
fn index_def(table: &str, index: &Index) -> String {
    let unique = if index.unique { "UNIQUE " } else { "" };
    let target = &index.target;
    let method = match index.family {
        IndexFamily::Text => format!("gin (to_tsvector('simple', {target}))"),
        family => format!("{} ({target})", family_word(family)),
    };
    format!(
        "CREATE {unique}INDEX {} ON {PUBLIC}.{table} USING {method}",
        index.name
    )
}
