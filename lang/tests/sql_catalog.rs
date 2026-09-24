//! The CATALOG SURFACE: the `db_*` core rows, the `pg_catalog` and
//! `information_schema` views over them, PostGIS's two, the session facts a
//! driver asks for on connect, and the `SHOW` family.
//!
//! What is under test is that the rows AGREE WITH THE CATALOG THEY DESCRIBE.
//! The oracle is built here, in the test process: this file writes the
//! collections, the columns, the indexes and the edges itself, so it knows
//! what every relation must answer without asking the thing under test.
//! Nothing here is stored -- the rows are computed at prepare from the
//! catalog readers -- so a disagreement is a bug in the projection, which is
//! exactly what a view can get wrong.
//!
//! `docs/lang/QL_CONTRACT.md` §1 row "catalog" and §2 rows `SHOW ...` and
//! `SELECT version()`; `docs/dist/PG_SURFACE.md` is the list this file
//! checks against.

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::{CollectionId, Database, EntityId};
use sekejap_lang::catalog::{self, CatalogRelation};
use sekejap_lang::{Param, SqlDatabase, SqlError, SqlResult, SqlValue};
use serde_json::json;
use std::collections::BTreeSet;
use tempfile::TempDir;

fn config() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

/// The catalog this file builds, held as the oracle: what every relation
/// below must be a projection of.
struct Oracle {
    /// `(collection, columns in catalog order)`. `_key` is column one of
    /// every collection -- the external key `Database::put` maps and the
    /// PRIMARY KEY every constraint view names -- so it heads every list.
    tables: Vec<(&'static str, Vec<(&'static str, &'static str)>)>,
    /// `(table, index name, family, field)`.
    indexes: Vec<(&'static str, &'static str, &'static str, &'static str)>,
    /// `(edge type, from table, to table, context)`.
    edges: Vec<(&'static str, &'static str, &'static str, &'static str)>,
    /// Rows written per collection.
    rows: Vec<(&'static str, u64)>,
}

fn oracle() -> Oracle {
    Oracle {
        tables: vec![
            (
                "place",
                vec![
                    ("_key", "TEXT"),
                    ("name", "TEXT"),
                    ("population", "BIGINT"),
                    ("score", "DOUBLE PRECISION"),
                    ("open", "BOOLEAN"),
                    ("tags", "JSONB"),
                    ("born", "TIMESTAMPTZ"),
                    ("opened_on", "DATE"),
                    ("spot", "GEOMETRY(Point, 4326)"),
                    ("plot", "GEOMETRY"),  // Kind::Geo
                    ("emb", "VECTOR(4)"),
                ],
            ),
            (
                "note",
                vec![("_key", "TEXT"), ("body", "TEXT"), ("place_key", "TEXT")],
            ),
        ],
        indexes: vec![
            ("place", "place_name", "btree", "name"),
            ("place", "place_pop", "btree", "population"),
            ("place", "place_spot", "gist", "spot"),
            ("note", "note_body", "gin", "body"),
        ],
        edges: vec![("mentions", "note", "place", "(base graph)")],
        rows: vec![("place", 2), ("note", 2)],
    }
}

fn open() -> (TempDir, Database) {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("db"), config()).unwrap();
    db.sql(
        "CREATE TABLE place (name TEXT, population BIGINT, \
         score DOUBLE PRECISION, open BOOLEAN, tags JSONB, born TIMESTAMPTZ, \
         opened_on DATE, spot GEOMETRY(Point, 4326), plot GEOMETRY, emb VECTOR(4)) \
         WITH (index: none)",
        &[],
    )
    .unwrap();
    db.sql(
        "CREATE TABLE note (body TEXT NOT NULL, place_key TEXT) WITH (index: none)",
        &[],
    )
    .unwrap();
    db.sql("CREATE INDEX place_name ON place USING btree (name)", &[])
        .unwrap();
    db.sql("CREATE INDEX place_pop ON place USING btree (population)", &[])
        .unwrap();
    db.sql("CREATE INDEX place_spot ON place USING gist (spot)", &[])
        .unwrap();
    db.sql(
        "CREATE INDEX note_body ON note USING gin (to_tsvector('simple', body))",
        &[],
    )
    .unwrap();
    for (key, name, population) in [("p1", "Bandung", 2_500_000i64), ("p2", "Malang", 880_000)] {
        db.sql(
            "INSERT INTO place (_key, name, population) VALUES ($1, $2, $3)",
            &[
                Param::Text(key.into()),
                Param::Text(name.into()),
                Param::Int(population),
            ],
        )
        .unwrap();
    }
    for (key, body) in [("n1", "kebun kopi"), ("n2", "pasar pagi")] {
        db.sql(
            "INSERT INTO note (_key, body, place_key) VALUES ($1, $2, 'p1')",
            &[Param::Text(key.into()), Param::Text(body.into())],
        )
        .unwrap();
    }
    db.enable_graph().unwrap();
    let note = id_of(&db, "note", "n1");
    let place = id_of(&db, "place", "p1");
    db.link(note, "mentions", place, "", &json!({})).unwrap();
    db.commit().unwrap();
    (dir, db)
}

fn id_of(db: &Database, collection: &str, key: &str) -> EntityId {
    let id = db.collection(collection).unwrap().unwrap();
    db.get(id, key).unwrap().unwrap().id
}

// ── helpers ──────────────────────────────────────────────────────────────

fn run(db: &mut Database, text: &str) -> SqlResult {
    db.sql(text, &[])
        .unwrap_or_else(|e| panic!("`{text}` was refused: {e}"))
}

/// One SELECT, as `(columns, rows)`.
fn rows(db: &mut Database, text: &str) -> (Vec<String>, Vec<Vec<SqlValue>>) {
    match run(db, text) {
        SqlResult::Rows { columns, rows } => {
            (columns, rows.into_iter().map(|r| r.values).collect())
        }
        other => panic!("`{text}` answered {other:?}, not rows"),
    }
}

fn text_at(values: &[SqlValue], at: usize) -> String {
    match &values[at] {
        SqlValue::Text(t) => t.clone(),
        other => panic!("column {at} is {other:?}, not text"),
    }
}

fn int_at(values: &[SqlValue], at: usize) -> i64 {
    match &values[at] {
        SqlValue::Int(i) => *i,
        other => panic!("column {at} is {other:?}, not an integer"),
    }
}

/// The value of `column` in `row`, for a relation looked up by name.
fn cell(relation: &CatalogRelation, row: &[SqlValue], column: &str) -> SqlValue {
    row[relation
        .column_at(column)
        .unwrap_or_else(|| panic!("`{}` has no column `{column}`", relation.written()))]
    .clone()
}

fn relation(name: &str) -> &'static CatalogRelation {
    catalog::relation(name).unwrap_or_else(|| panic!("`{name}` is not a catalog relation"))
}

fn refuse(db: &mut Database, text: &str) -> SqlError {
    db.sql(text, &[])
        .err()
        .unwrap_or_else(|| panic!("`{text}` was accepted"))
}

// ── the db_* core rows ───────────────────────────────────────────────────

#[test]
fn db_tables_lists_every_collection_with_its_row_count_and_field_count() {
    let (_dir, mut db) = open();
    let oracle = oracle();
    let (columns, answer) = rows(&mut db, "SELECT * FROM db_tables ORDER BY name ASC");
    assert_eq!(columns, ["name", "id", "rows", "fields"]);
    let mut expected: Vec<(&str, usize, u64)> = oracle
        .tables
        .iter()
        .map(|(name, fields)| {
            let rows = oracle
                .rows
                .iter()
                .find(|(t, _)| t == name)
                .map_or(0, |(_, n)| *n);
            (*name, fields.len(), rows)
        })
        .collect();
    expected.sort_by_key(|(name, ..)| *name);
    assert_eq!(answer.len(), expected.len(), "one row per collection");
    for (row, (name, fields, count)) in answer.iter().zip(expected) {
        assert_eq!(text_at(row, 0), name);
        assert_eq!(int_at(row, 2), count as i64, "{name}: live row count");
        assert_eq!(int_at(row, 3), fields as i64, "{name}: field count");
        assert_eq!(
            int_at(row, 1),
            i64::from(db.collection(name).unwrap().unwrap().0),
            "{name}: collection id"
        );
    }
}

#[test]
fn db_columns_lists_every_declared_field_in_layout_order_with_its_declared_type() {
    let (_dir, mut db) = open();
    let oracle = oracle();
    let (columns, answer) = rows(&mut db, "SELECT * FROM db_columns WHERE table = 'place'");
    assert_eq!(
        columns,
        [
            "table",
            "name",
            "kind",
            "declared_type",
            "position",
            "not_null",
            "has_default"
        ]
    );
    let expected = &oracle.tables[0].1;
    assert_eq!(answer.len(), expected.len());
    for (at, (row, (name, declared))) in answer.iter().zip(expected).enumerate() {
        assert_eq!(text_at(row, 1), *name, "field {at}");
        assert_eq!(text_at(row, 3), *declared, "`{name}`: declared type");
        assert_eq!(int_at(row, 4), at as i64 + 1, "`{name}`: position");
    }
}

#[test]
fn db_columns_reports_the_not_null_a_column_was_declared_with() {
    let (_dir, mut db) = open();
    let (_, answer) = rows(
        &mut db,
        "SELECT name, not_null FROM db_columns WHERE table = 'note'",
    );
    let found: Vec<(String, SqlValue)> = answer
        .iter()
        .map(|row| (text_at(row, 0), row[1].clone()))
        .collect();
    assert_eq!(
        found,
        vec![
            // The external key is NOT NULL by construction: a row without a
            // key is not a row, and no declaration says so.
            ("_key".to_owned(), SqlValue::Bool(true)),
            ("body".to_owned(), SqlValue::Bool(true)),
            ("place_key".to_owned(), SqlValue::Bool(false)),
        ],
        "`body TEXT NOT NULL` is the only DECLARED NOT NULL column"
    );
}

#[test]
fn db_indexes_lists_every_index_with_the_family_its_create_statement_named() {
    let (_dir, mut db) = open();
    let oracle = oracle();
    let (columns, answer) = rows(&mut db, "SELECT * FROM db_indexes ORDER BY name ASC");
    assert_eq!(
        columns,
        ["table", "name", "family", "field", "expression", "state"]
    );
    let mut expected = oracle.indexes.clone();
    expected.sort_by_key(|(_, name, ..)| *name);
    assert_eq!(answer.len(), expected.len());
    for (row, (table, name, family, field)) in answer.iter().zip(expected) {
        assert_eq!(text_at(row, 0), table);
        assert_eq!(text_at(row, 1), name);
        assert_eq!(text_at(row, 2), family, "`{name}`: family");
        assert_eq!(text_at(row, 3), field, "`{name}`: field");
        assert_eq!(text_at(row, 5), "ready", "`{name}`: state");
    }
}

#[test]
fn db_edges_reports_the_collections_an_edge_type_connects() {
    let (_dir, mut db) = open();
    let oracle = oracle();
    let (columns, answer) = rows(&mut db, "SELECT * FROM db_edges");
    assert_eq!(columns, ["edge_type", "from_table", "to_table", "context"]);
    let found: Vec<(String, String, String, String)> = answer
        .iter()
        .map(|row| {
            (
                text_at(row, 0),
                text_at(row, 1),
                text_at(row, 2),
                text_at(row, 3),
            )
        })
        .collect();
    let expected: Vec<(String, String, String, String)> = oracle
        .edges
        .iter()
        .map(|(t, f, d, c)| {
            (
                (*t).to_owned(),
                (*f).to_owned(),
                (*d).to_owned(),
                (*c).to_owned(),
            )
        })
        .collect();
    assert_eq!(found, expected);
}

#[test]
fn db_contexts_lists_the_base_graph_and_every_interned_context() {
    let (_dir, mut db) = open();
    let (_, before) = rows(&mut db, "SELECT name FROM db_contexts");
    assert_eq!(
        before.iter().map(|r| text_at(r, 0)).collect::<Vec<_>>(),
        vec!["(base graph)"],
        "an edge written with no context is in the base graph, which has no descriptor"
    );
    db.create_graph_context("history").unwrap();
    db.commit().unwrap();
    let (_, after) = rows(&mut db, "SELECT name FROM db_contexts ORDER BY name ASC");
    assert_eq!(
        after.iter().map(|r| text_at(r, 0)).collect::<Vec<_>>(),
        vec!["(base graph)", "history"]
    );
}

#[test]
fn a_db_relation_composes_with_where_order_limit_and_distinct() {
    let (_dir, mut db) = open();
    // WHERE over a column of the view.
    let (_, filtered) = rows(&mut db, "SELECT name FROM db_tables WHERE name = 'note'");
    assert_eq!(filtered.len(), 1);
    assert_eq!(text_at(&filtered[0], 0), "note");
    // ORDER BY, both directions, over the same column.
    let (_, ascending) = rows(&mut db, "SELECT name FROM db_tables ORDER BY name ASC");
    let (_, descending) = rows(&mut db, "SELECT name FROM db_tables ORDER BY name DESC");
    let up: Vec<String> = ascending.iter().map(|r| text_at(r, 0)).collect();
    let mut down: Vec<String> = descending.iter().map(|r| text_at(r, 0)).collect();
    down.reverse();
    assert_eq!(up, down, "DESC is ASC reversed");
    assert_eq!(up, vec!["note", "place"]);
    // LIMIT counts the rows that survived.
    let (_, limited) = rows(&mut db, "SELECT name FROM db_tables ORDER BY name ASC LIMIT 1");
    assert_eq!(limited.len(), 1);
    assert_eq!(text_at(&limited[0], 0), "note");
    // DISTINCT folds the PROJECTED rows: every index in the fixture is on
    // one of two tables, so the distinct table column is those two.
    let (_, distinct) = rows(
        &mut db,
        "SELECT DISTINCT table FROM db_indexes ORDER BY table ASC",
    );
    assert_eq!(
        distinct.iter().map(|r| text_at(r, 0)).collect::<Vec<_>>(),
        vec!["note", "place"]
    );
    // A comparison, an IN list and a boolean OR over the same view.
    let (_, ranged) = rows(&mut db, "SELECT name FROM db_tables WHERE fields > 5");
    assert_eq!(ranged.len(), 1, "only `place` has more than five fields");
    let (_, listed) = rows(
        &mut db,
        "SELECT name FROM db_indexes WHERE family IN ('gin', 'gist') ORDER BY name ASC",
    );
    assert_eq!(
        listed.iter().map(|r| text_at(r, 0)).collect::<Vec<_>>(),
        vec!["note_body", "place_spot"]
    );
    let (_, either) = rows(
        &mut db,
        "SELECT name FROM db_indexes WHERE family = 'gin' OR field = 'name'",
    );
    assert_eq!(either.len(), 2);
}

// ── the information_schema and pg_catalog views ──────────────────────────

#[test]
fn every_relation_answers_every_one_of_its_own_columns() {
    let (_dir, mut db) = open();
    for relation in catalog::RELATIONS {
        let statement = format!("SELECT * FROM {}", relation.written());
        let (columns, answer) = rows(&mut db, &statement);
        let expected: Vec<String> = relation
            .columns
            .iter()
            .map(|c| c.name.to_owned())
            .collect();
        assert_eq!(columns, expected, "`{statement}` answered other columns");
        for row in &answer {
            assert_eq!(
                row.len(),
                relation.columns.len(),
                "`{statement}` answered a row of the wrong width"
            );
        }
        // Each column is also selectable by name, which is how a driver
        // writes its query.
        for column in relation.columns {
            let one = format!("SELECT {} FROM {}", column.name, relation.written());
            let (got, _) = rows(&mut db, &one);
            assert_eq!(got, vec![column.name.to_owned()], "`{one}`");
        }
    }
}

#[test]
fn the_relation_directory_is_well_formed_and_its_oids_are_postgresqls() {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for relation in catalog::RELATIONS {
        assert!(
            seen.insert(relation.written()),
            "`{}` appears twice in the directory",
            relation.written()
        );
        assert!(
            !relation.columns.is_empty(),
            "`{}` has no columns",
            relation.written()
        );
        let mut columns: BTreeSet<&str> = BTreeSet::new();
        for column in relation.columns {
            assert!(
                columns.insert(column.name),
                "`{}` names `{}` twice",
                relation.written(),
                column.name
            );
            assert_eq!(
                column.name,
                column.name.to_ascii_lowercase(),
                "a catalog column is spelled in lower case"
            );
            // The OID is one PostgreSQL has: every value here must be a type
            // this surface can also describe on the wire.
            assert!(
                [
                    catalog::OID_BOOL,
                    catalog::OID_BYTEA,
                    catalog::OID_CHAR,
                    catalog::OID_NAME,
                    catalog::OID_INT8,
                    catalog::OID_INT2,
                    catalog::OID_INT4,
                    catalog::OID_TEXT,
                    catalog::OID_OID,
                    catalog::OID_FLOAT4,
                    catalog::OID_FLOAT8,
                    catalog::OID_DATE,
                    catalog::OID_TIMESTAMPTZ,
                    catalog::OID_JSONB,
                    catalog::OID_GEOMETRY,
                    catalog::OID_VECTOR,
                ]
                .contains(&column.oid),
                "`{}.{}` carries OID {}, which is not one this surface names",
                relation.written(),
                column.name,
                column.oid
            );
        }
    }
    // The numbers themselves, written out, because they are PostgreSQL's and
    // a driver reads them as constants.
    assert_eq!(
        (
            catalog::OID_TEXT,
            catalog::OID_INT8,
            catalog::OID_FLOAT8,
            catalog::OID_BOOL,
            catalog::OID_JSONB,
            catalog::OID_TIMESTAMPTZ,
            catalog::OID_DATE,
            catalog::OID_BYTEA,
        ),
        (25, 20, 701, 16, 3802, 1184, 1082, 17)
    );
}

#[test]
fn pg_type_carries_one_row_per_kind_this_engine_has_plus_geometry_and_vector() {
    let (_dir, mut db) = open();
    let r = relation("pg_catalog.pg_type");
    let (_, answer) = rows(&mut db, "SELECT * FROM pg_catalog.pg_type");
    let found: Vec<(String, i64)> = answer
        .iter()
        .map(|row| {
            (
                match cell(r, row, "typname") {
                    SqlValue::Text(t) => t,
                    other => panic!("typname is {other:?}"),
                },
                match cell(r, row, "oid") {
                    SqlValue::Int(i) => i,
                    other => panic!("oid is {other:?}"),
                },
            )
        })
        .collect();
    assert_eq!(
        found,
        vec![
            ("bool".to_owned(), 16),
            ("bytea".to_owned(), 17),
            ("int8".to_owned(), 20),
            ("text".to_owned(), 25),
            ("float8".to_owned(), 701),
            ("date".to_owned(), 1082),
            ("timestamptz".to_owned(), 1184),
            ("jsonb".to_owned(), 3802),
            ("geometry".to_owned(), 18_000),
            ("vector".to_owned(), 18_001),
        ]
    );
}

#[test]
fn pg_class_and_pg_attribute_are_the_db_rows_under_postgresqls_names() {
    let (_dir, mut db) = open();
    let oracle = oracle();
    let class = relation("pg_catalog.pg_class");
    let (_, relations) = rows(&mut db, "SELECT * FROM pg_class ORDER BY relname ASC");
    assert_eq!(relations.len(), oracle.tables.len());
    let mut expected: Vec<&(&str, Vec<(&str, &str)>)> = oracle.tables.iter().collect();
    expected.sort_by_key(|(name, _)| *name);
    for (row, (name, fields)) in relations.iter().zip(&expected) {
        assert_eq!(cell(class, row, "relname"), SqlValue::Text((*name).into()));
        assert_eq!(cell(class, row, "relkind"), SqlValue::Text("r".into()));
        assert_eq!(
            cell(class, row, "relnatts"),
            SqlValue::Int(fields.len() as i64)
        );
        assert_eq!(
            cell(class, row, "oid"),
            SqlValue::Int(catalog::object_oid(name)),
            "a collection's OID is the stable hash of its name"
        );
        assert_eq!(cell(class, row, "relhasindex"), SqlValue::Bool(true));
    }

    // pg_attribute, keyed by the SAME OID pg_class reported.
    let attribute = relation("pg_catalog.pg_attribute");
    let statement = format!(
        "SELECT * FROM pg_attribute WHERE attrelid = {} ORDER BY attnum ASC",
        catalog::object_oid("note")
    );
    let (_, attributes) = rows(&mut db, &statement);
    let note_fields = &oracle.tables[1].1;
    assert_eq!(attributes.len(), note_fields.len());
    for (at, (row, (name, _))) in attributes.iter().zip(note_fields).enumerate() {
        assert_eq!(cell(attribute, row, "attname"), SqlValue::Text((*name).into()));
        assert_eq!(cell(attribute, row, "attnum"), SqlValue::Int(at as i64 + 1));
        assert_eq!(
            cell(attribute, row, "atttypid"),
            SqlValue::Int(i64::from(catalog::OID_TEXT)),
            "every `note` column is declared TEXT"
        );
    }
}

#[test]
fn pg_attribute_reports_the_type_oid_each_declared_column_maps_to() {
    let (_dir, mut db) = open();
    let attribute = relation("pg_catalog.pg_attribute");
    let statement = format!(
        "SELECT * FROM pg_attribute WHERE attrelid = {}",
        catalog::object_oid("place")
    );
    let (_, answer) = rows(&mut db, &statement);
    let found: Vec<(String, i64)> = answer
        .iter()
        .map(|row| {
            (
                match cell(attribute, row, "attname") {
                    SqlValue::Text(t) => t,
                    other => panic!("{other:?}"),
                },
                match cell(attribute, row, "atttypid") {
                    SqlValue::Int(i) => i,
                    other => panic!("{other:?}"),
                },
            )
        })
        .collect();
    let expected: Vec<(String, i64)> = vec![
        ("_key", catalog::OID_TEXT),
        ("name", catalog::OID_TEXT),
        ("population", catalog::OID_INT8),
        ("score", catalog::OID_FLOAT8),
        ("open", catalog::OID_BOOL),
        ("tags", catalog::OID_JSONB),
        // TIMESTAMPTZ and DATE are both Kind::Int (QL_CONTRACT §5 deviation
        // 8), so only the DECLARED spelling tells them apart -- which is
        // what makes this the interesting row of the table.
        ("born", catalog::OID_TIMESTAMPTZ),
        ("opened_on", catalog::OID_DATE),
        ("spot", catalog::OID_GEOMETRY),
        ("plot", catalog::OID_GEOMETRY),
        ("emb", catalog::OID_VECTOR),
    ]
    .into_iter()
    .map(|(name, oid)| (name.to_owned(), i64::from(oid)))
    .collect();
    assert_eq!(found, expected);
}

#[test]
fn information_schema_columns_names_the_standard_type_of_every_declared_column() {
    let (_dir, mut db) = open();
    let r = relation("information_schema.columns");
    let (_, answer) = rows(
        &mut db,
        "SELECT * FROM information_schema.columns WHERE table_name = 'place'",
    );
    let found: Vec<(String, String, String)> = answer
        .iter()
        .map(|row| {
            (
                match cell(r, row, "column_name") {
                    SqlValue::Text(t) => t,
                    other => panic!("{other:?}"),
                },
                match cell(r, row, "data_type") {
                    SqlValue::Text(t) => t,
                    other => panic!("{other:?}"),
                },
                match cell(r, row, "udt_name") {
                    SqlValue::Text(t) => t,
                    other => panic!("{other:?}"),
                },
            )
        })
        .collect();
    assert_eq!(
        found,
        vec![
            ("_key", "text", "text"),
            ("name", "text", "text"),
            ("population", "bigint", "int8"),
            ("score", "double precision", "float8"),
            ("open", "boolean", "bool"),
            ("tags", "jsonb", "jsonb"),
            ("born", "timestamp with time zone", "timestamptz"),
            ("opened_on", "date", "date"),
            ("spot", "USER-DEFINED", "geometry"),
            ("plot", "USER-DEFINED", "geometry"),
            ("emb", "USER-DEFINED", "vector"),
        ]
        .into_iter()
        .map(|(a, b, c)| (a.to_owned(), b.to_owned(), c.to_owned()))
        .collect::<Vec<_>>()
    );
    // …and every one of them says which schema and catalog it is in.
    for row in &answer {
        assert_eq!(cell(r, row, "table_schema"), SqlValue::Text("public".into()));
        assert_eq!(
            cell(r, row, "table_catalog"),
            SqlValue::Text("sekejap".into())
        );
    }
}

#[test]
fn information_schema_tables_and_schemata_say_what_exists() {
    let (_dir, mut db) = open();
    let tables = relation("information_schema.tables");
    let (_, answer) = rows(
        &mut db,
        "SELECT * FROM information_schema.tables ORDER BY table_name ASC",
    );
    let found: Vec<(String, String)> = answer
        .iter()
        .map(|row| {
            (
                match cell(tables, row, "table_name") {
                    SqlValue::Text(t) => t,
                    other => panic!("{other:?}"),
                },
                match cell(tables, row, "table_type") {
                    SqlValue::Text(t) => t,
                    other => panic!("{other:?}"),
                },
            )
        })
        .collect();
    assert_eq!(
        found,
        vec![
            ("note".to_owned(), "BASE TABLE".to_owned()),
            ("place".to_owned(), "BASE TABLE".to_owned()),
        ]
    );
    let (_, schemata) = rows(
        &mut db,
        "SELECT schema_name FROM information_schema.schemata ORDER BY schema_name ASC",
    );
    assert_eq!(
        schemata.iter().map(|r| text_at(r, 0)).collect::<Vec<_>>(),
        vec!["information_schema", "pg_catalog", "public"]
    );
}

#[test]
fn the_primary_key_is_the_external_key_in_both_constraint_views() {
    let (_dir, mut db) = open();
    let constraints = relation("information_schema.table_constraints");
    let (_, answer) = rows(
        &mut db,
        "SELECT * FROM information_schema.table_constraints WHERE table_name = 'place'",
    );
    assert_eq!(answer.len(), 1, "one primary key per collection");
    assert_eq!(
        cell(constraints, &answer[0], "constraint_type"),
        SqlValue::Text("PRIMARY KEY".into())
    );
    assert_eq!(
        cell(constraints, &answer[0], "constraint_name"),
        SqlValue::Text("place_pkey".into())
    );
    let usage = relation("information_schema.key_column_usage");
    let (_, keys) = rows(
        &mut db,
        "SELECT * FROM information_schema.key_column_usage WHERE table_name = 'place'",
    );
    assert_eq!(keys.len(), 1);
    assert_eq!(
        cell(usage, &keys[0], "column_name"),
        SqlValue::Text("_key".into()),
        "the external key IS the primary key here (COLLECTIONS: one key mapping per collection)"
    );
    // pg_constraint reports the same one object, under PostgreSQL's names.
    let pg = relation("pg_catalog.pg_constraint");
    let (_, pg_rows) = rows(&mut db, "SELECT * FROM pg_constraint WHERE conname = 'place_pkey'");
    assert_eq!(pg_rows.len(), 1);
    assert_eq!(cell(pg, &pg_rows[0], "contype"), SqlValue::Text("p".into()));
    assert_eq!(
        cell(pg, &pg_rows[0], "conrelid"),
        SqlValue::Int(catalog::object_oid("place"))
    );
}

#[test]
fn pg_index_and_pg_indexes_describe_the_same_indexes_db_indexes_does() {
    let (_dir, mut db) = open();
    let oracle = oracle();
    let indexes = relation("pg_catalog.pg_indexes");
    let (_, answer) = rows(&mut db, "SELECT * FROM pg_indexes ORDER BY indexname ASC");
    assert_eq!(answer.len(), oracle.indexes.len());
    let mut expected = oracle.indexes.clone();
    expected.sort_by_key(|(_, name, ..)| *name);
    for (row, (table, name, _, _)) in answer.iter().zip(&expected) {
        assert_eq!(cell(indexes, row, "indexname"), SqlValue::Text((*name).into()));
        assert_eq!(cell(indexes, row, "tablename"), SqlValue::Text((*table).into()));
        assert_eq!(
            cell(indexes, row, "schemaname"),
            SqlValue::Text("public".into())
        );
        let SqlValue::Text(def) = cell(indexes, row, "indexdef") else {
            panic!("indexdef is not text")
        };
        assert!(
            def.starts_with(&format!("CREATE INDEX {name} ON public.{table} USING ")),
            "`{name}` prints `{def}`"
        );
    }
    let index = relation("pg_catalog.pg_index");
    let (_, pg_index) = rows(
        &mut db,
        &format!(
            "SELECT * FROM pg_index WHERE indrelid = {}",
            catalog::object_oid("place")
        ),
    );
    assert_eq!(pg_index.len(), 3, "three indexes on `place`");
    for row in &pg_index {
        assert_eq!(cell(index, row, "indisvalid"), SqlValue::Bool(true));
        assert_eq!(cell(index, row, "indislive"), SqlValue::Bool(true));
        assert_eq!(cell(index, row, "indnatts"), SqlValue::Int(1));
    }
}

#[test]
fn pg_namespace_pg_tables_and_pg_description_answer_what_they_have() {
    let (_dir, mut db) = open();
    let (_, namespaces) = rows(&mut db, "SELECT nspname FROM pg_namespace ORDER BY nspname ASC");
    assert_eq!(
        namespaces.iter().map(|r| text_at(r, 0)).collect::<Vec<_>>(),
        vec!["information_schema", "pg_catalog", "public"]
    );
    let (_, tables) = rows(&mut db, "SELECT tablename FROM pg_tables ORDER BY tablename ASC");
    assert_eq!(
        tables.iter().map(|r| text_at(r, 0)).collect::<Vec<_>>(),
        vec!["note", "place"]
    );
    // Empty, and empty is the TRUE answer: e4 records no comment on any
    // object, so there is nothing for `pg_description` to hold.
    let (columns, described) = rows(&mut db, "SELECT * FROM pg_description");
    assert_eq!(columns, ["objoid", "classoid", "objsubid", "description"]);
    assert!(described.is_empty());
}

// ── PostGIS ──────────────────────────────────────────────────────────────

#[test]
fn geometry_columns_names_every_geometry_column_with_srid_4326() {
    let (_dir, mut db) = open();
    let r = relation("geometry_columns");
    let (_, answer) = rows(
        &mut db,
        "SELECT * FROM geometry_columns ORDER BY f_geometry_column ASC",
    );
    let found: Vec<(String, String, i64, i64, String)> = answer
        .iter()
        .map(|row| {
            (
                match cell(r, row, "f_table_name") {
                    SqlValue::Text(t) => t,
                    other => panic!("{other:?}"),
                },
                match cell(r, row, "f_geometry_column") {
                    SqlValue::Text(t) => t,
                    other => panic!("{other:?}"),
                },
                match cell(r, row, "coord_dimension") {
                    SqlValue::Int(i) => i,
                    other => panic!("{other:?}"),
                },
                match cell(r, row, "srid") {
                    SqlValue::Int(i) => i,
                    other => panic!("{other:?}"),
                },
                match cell(r, row, "type") {
                    SqlValue::Text(t) => t,
                    other => panic!("{other:?}"),
                },
            )
        })
        .collect();
    assert_eq!(
        found,
        vec![
            ("place".to_owned(), "plot".to_owned(), 2, 4326, "GEOMETRY".to_owned()),
            ("place".to_owned(), "spot".to_owned(), 2, 4326, "POINT".to_owned()),
        ],
        "two geometry columns, both WGS84, both two-dimensional"
    );
}

#[test]
fn spatial_ref_sys_holds_exactly_one_row_and_it_is_4326() {
    let (_dir, mut db) = open();
    let r = relation("spatial_ref_sys");
    let (_, answer) = rows(&mut db, "SELECT * FROM spatial_ref_sys");
    assert_eq!(
        answer.len(),
        1,
        "4326 is the only system e4 stores geometry in, so a second row would name one nothing is in"
    );
    assert_eq!(cell(r, &answer[0], "srid"), SqlValue::Int(4326));
    assert_eq!(cell(r, &answer[0], "auth_name"), SqlValue::Text("EPSG".into()));
    assert_eq!(cell(r, &answer[0], "auth_srid"), SqlValue::Int(4326));
    let SqlValue::Text(wkt) = cell(r, &answer[0], "srtext") else {
        panic!("srtext is not text")
    };
    assert!(wkt.contains("WGS 84") && wkt.contains("4326"), "{wkt}");
    let SqlValue::Text(proj) = cell(r, &answer[0], "proj4text") else {
        panic!("proj4text is not text")
    };
    assert_eq!(proj, "+proj=longlat +datum=WGS84 +no_defs");
    // …and the CRS lookup QGIS writes finds it.
    let (_, one) = rows(
        &mut db,
        "SELECT auth_name, auth_srid, srtext, proj4text FROM spatial_ref_sys WHERE srid = 4326",
    );
    assert_eq!(one.len(), 1);
}

// ── the session facts ────────────────────────────────────────────────────

#[test]
fn version_starts_with_postgresql_because_drivers_parse_it() {
    let (_dir, mut db) = open();
    let (columns, answer) = rows(&mut db, "SELECT version()");
    assert_eq!(columns, ["version"]);
    assert_eq!(text_at(&answer[0], 0), concat!("PostgreSQL 16.0 (sekejap ", env!("CARGO_PKG_VERSION"), ")"));
    assert!(
        text_at(&answer[0], 0).starts_with("PostgreSQL "),
        "a driver reads the major number out of this string to choose its catalog queries; the engine that answered goes in the parenthesis"
    );
    // The same fact without the costume, for a caller that is not a driver.
    let (columns, answer) = rows(&mut db, "SELECT db_version()");
    assert_eq!(columns, ["db_version"]);
    assert_eq!(text_at(&answer[0], 0), concat!("sekejap ", env!("CARGO_PKG_VERSION")));
}

#[test]
fn the_session_facts_a_driver_asks_for_on_connect_all_answer() {
    let (_dir, mut db) = open();
    for (statement, column, expected) in [
        ("SELECT current_schema()", "current_schema", "public"),
        ("SELECT current_database()", "current_database", "sekejap"),
        ("SELECT current_user", "current_user", "postgres"),
        ("SELECT session_user", "current_user", "postgres"),
    ] {
        let (columns, answer) = rows(&mut db, statement);
        assert_eq!(columns, [column.to_owned()], "`{statement}`");
        assert_eq!(text_at(&answer[0], 0), expected, "`{statement}`");
    }
    // Two facts in one statement, which is how pgjdbc asks.
    let (columns, answer) = rows(&mut db, "SELECT current_schema(), session_user");
    assert_eq!(columns, ["current_schema", "current_user"]);
    assert_eq!(text_at(&answer[0], 0), "public");
    assert_eq!(text_at(&answer[0], 1), "postgres");
    // The backend pid is THIS process, because a connection is a process.
    let (columns, answer) = rows(&mut db, "SELECT pg_backend_pid()");
    assert_eq!(columns, ["pg_backend_pid"]);
    assert_eq!(int_at(&answer[0], 0), i64::from(std::process::id()));
    // …and the liveness probe every pool sends.
    let (_, one) = rows(&mut db, "SELECT 1");
    assert_eq!(one[0][0], SqlValue::Int(1));
    // An alias renames the column, as it does anywhere else.
    let (columns, _) = rows(&mut db, "SELECT version() AS v");
    assert_eq!(columns, ["v"]);
}

#[test]
fn a_client_setting_is_accepted_as_a_notice_and_read_back_from_a_constant() {
    let (_dir, mut db) = open();
    for statement in [
        "SET client_encoding = 'UTF8'",
        "SET DateStyle TO 'ISO, MDY'",
        "SET TIME ZONE 'UTC'",
        "SET application_name = 'DBeaver 24.0'",
        "SET search_path = public",
        "SET extra_float_digits = 3",
    ] {
        match run(&mut db, statement) {
            SqlResult::Notice(text) => assert!(
                text.contains("accepted and not stored"),
                "`{statement}` answered `{text}`"
            ),
            other => panic!("`{statement}` answered {other:?}, not a notice"),
        }
    }
    // A SHOW of the same knob reports what this engine HAS, which is not
    // what the SET said -- and says so rather than echoing the SET back.
    for (statement, column, expected) in [
        ("SHOW client_encoding", "client_encoding", "UTF8"),
        ("SHOW DateStyle", "datestyle", "ISO, MDY"),
        ("SHOW TIME ZONE", "timezone", "UTC"),
        ("SHOW search_path", "search_path", "\"$user\", public"),
        ("SHOW server_version", "server_version", "16.0"),
    ] {
        let (columns, answer) = rows(&mut db, statement);
        assert_eq!(columns, [column.to_owned()], "`{statement}`");
        assert_eq!(text_at(&answer[0], 0), expected, "`{statement}`");
    }
    // `extra_float_digits` was SET to 3 and still reports 1: nothing was
    // stored, and the answer is the truth rather than the echo.
    let (_, answer) = rows(&mut db, "SHOW extra_float_digits");
    assert_eq!(text_at(&answer[0], 0), "1");
    // current_setting() reads the same constant, and an unknown knob is NULL
    // rather than an invention.
    let (_, answer) = rows(&mut db, "SELECT current_setting('client_encoding')");
    assert_eq!(text_at(&answer[0], 0), "UTF8");
    let (_, answer) = rows(&mut db, "SELECT current_setting('no_such_knob')");
    assert_eq!(answer[0][0], SqlValue::Null);
}

// ── the SHOW family ──────────────────────────────────────────────────────

#[test]
fn the_show_family_is_the_db_rows_said_in_one_word() {
    let (_dir, mut db) = open();
    // SHOW TABLES is SELECT * FROM db_tables.
    let (show_columns, show_rows) = rows(&mut db, "SHOW TABLES");
    let (select_columns, select_rows) = rows(&mut db, "SELECT * FROM db_tables");
    assert_eq!(show_columns, select_columns);
    assert_eq!(show_rows, select_rows);
    // SHOW EDGES is SELECT * FROM db_edges.
    let (show_columns, show_rows) = rows(&mut db, "SHOW EDGES");
    let (select_columns, select_rows) = rows(&mut db, "SELECT * FROM db_edges");
    assert_eq!(show_columns, select_columns);
    assert_eq!(show_rows, select_rows);
    // SHOW <collection> is db_columns for that one collection, with the
    // table column dropped because the statement already named it.
    let (columns, answer) = rows(&mut db, "SHOW note");
    assert_eq!(
        columns,
        ["name", "kind", "declared_type", "position", "not_null", "has_default"]
    );
    assert_eq!(
        answer.iter().map(|r| text_at(r, 0)).collect::<Vec<_>>(),
        vec!["_key", "body", "place_key"]
    );
    // SHOW INDEXES, whole and filtered.
    let (columns, all) = rows(&mut db, "SHOW INDEXES");
    assert_eq!(columns, ["name", "family", "field", "expression", "state"]);
    assert_eq!(all.len(), 4);
    let (_, on_place) = rows(&mut db, "SHOW INDEXES ON place");
    assert_eq!(on_place.len(), 3);
}

#[test]
fn show_create_table_prints_ddl_that_names_every_column_and_index() {
    let (_dir, mut db) = open();
    let (columns, answer) = rows(&mut db, "SHOW CREATE TABLE place");
    assert_eq!(columns, ["create_table"]);
    let ddl = text_at(&answer[0], 0);
    assert!(ddl.starts_with("CREATE TABLE place ("), "{ddl}");
    for (name, declared) in &oracle().tables[0].1 {
        assert!(
            ddl.contains(&format!("  {name} {declared}")),
            "`{name} {declared}` is not in:\n{ddl}"
        );
    }
    assert!(ddl.contains("_key TEXT PRIMARY KEY"), "{ddl}");
    for (_, name, _, _) in oracle().indexes.iter().filter(|(t, ..)| *t == "place") {
        assert!(
            ddl.contains(&format!("CREATE INDEX {name} ON place USING ")),
            "`{name}` is not in:\n{ddl}"
        );
    }
    // The NOT NULL a column was declared with is printed back.
    let (_, note) = rows(&mut db, "SHOW CREATE TABLE note");
    assert!(text_at(&note[0], 0).contains("body TEXT NOT NULL"));
}

// ── schema qualification ─────────────────────────────────────────────────

#[test]
fn a_schema_qualified_name_resolves_to_the_one_thing_it_can_mean() {
    let (_dir, mut db) = open();
    // `public.t` IS `t`: there is one user schema (CREATE SCHEMA is Tier 2),
    // so the qualifier is read and dropped.
    let (_, qualified) = rows(&mut db, "SELECT _key FROM public.place");
    let (_, bare) = rows(&mut db, "SELECT _key FROM place");
    assert_eq!(qualified, bare);
    // `pg_catalog.x` and bare `x` are the same relation.
    let (_, q) = rows(&mut db, "SELECT nspname FROM pg_catalog.pg_namespace");
    let (_, b) = rows(&mut db, "SELECT nspname FROM pg_namespace");
    assert_eq!(q, b);
    // `information_schema.tables` is NOT the collection `tables`: the bare
    // spelling is an ordinary name a collection may have, so only the
    // qualified one reaches the view.
    db.sql("CREATE TABLE tables (n BIGINT)", &[]).unwrap();
    db.sql("INSERT INTO tables (_key, n) VALUES ('t1', 7)", &[])
        .unwrap();
    let (columns, _) = rows(&mut db, "SELECT _key FROM tables");
    assert_eq!(columns, ["_key"], "the bare name reached the collection");
    let (columns, _) = rows(&mut db, "SELECT table_name FROM information_schema.tables");
    assert_eq!(
        columns,
        ["table_name"],
        "the qualified name reached the view"
    );
    // A table alias is read and dropped, which is how a driver writes it.
    let (_, aliased) = rows(&mut db, "SELECT relname FROM pg_catalog.pg_class c ORDER BY relname ASC");
    assert_eq!(aliased.len(), 3, "place, note and the new `tables`");
}

// ── the statements a client actually issues ──────────────────────────────

/// The catalog statements DBeaver 24 / pgjdbc, psql and the QGIS PostgreSQL
/// provider issue on connect and on expanding a schema tree, in the
/// single-relation form this surface serves.
///
/// Collected from e1's `src/pg.rs` shim (the keys it pattern-matched on),
/// and e1's `tests/catalogue_and_ddl.rs`. Every one of them must ANSWER --
/// with rows or with no rows, but never with an error, because a client that
/// meets an error here stops before it has listed anything.
///
/// What is NOT in this list, and why, is `docs/dist/PG_SURFACE.md`'s last
/// section: psql's own `\d` and DBeaver's tree query are a JOIN of
/// `pg_class` with `pg_namespace` (plus `CASE`, `!~` and `::regclass`), and
/// `JOIN` is Tier 2 by name in `docs/lang/QL_CONTRACT.md` §4.8. It is
/// refused with that reason rather than answered by a second planner.
const CLIENT_STATEMENTS: &[&str] = &[
    // pgjdbc / DBeaver, on connect.
    "SELECT version()",
    "SELECT current_schema()",
    "SELECT current_database()",
    "SELECT current_schema(), session_user",
    "SELECT pg_backend_pid()",
    "SELECT 1",
    "SET client_encoding = 'UTF8'",
    "SET DateStyle TO 'ISO'",
    "SET application_name = 'DBeaver 24.0'",
    "SET extra_float_digits = 3",
    "SHOW TRANSACTION ISOLATION LEVEL",
    "SHOW client_encoding",
    "SHOW server_version",
    // pgjdbc's type resolution and its column probe.
    "SELECT oid, typname, typtype, typelem, typlen FROM pg_type",
    "SELECT oid, typname FROM pg_type WHERE oid = 18000",
    "SELECT * FROM place WHERE 1 <> 1 LIMIT 1",
    // The schema tree: namespaces, then relations, then columns.
    "SELECT oid, nspname FROM pg_namespace ORDER BY nspname ASC",
    "SELECT oid, relname, relkind, relnatts FROM pg_class WHERE relnamespace = 2200",
    "SELECT attrelid, attname, atttypid, attnum, attnotnull FROM pg_attribute WHERE attnum > 0",
    "SELECT schemaname, tablename FROM pg_tables WHERE schemaname = 'public'",
    "SELECT objoid, description FROM pg_description",
    // psql's \\dt and \\d, in the single-relation form.
    "SELECT tablename FROM pg_tables WHERE schemaname = 'public' ORDER BY tablename ASC",
    "SELECT indexname, indexdef FROM pg_indexes WHERE tablename = 'place'",
    "SELECT indexrelid, indisprimary, indisunique FROM pg_index WHERE indrelid = 16385",
    "SELECT conname, contype FROM pg_constraint WHERE conrelid = 16385",
    // information_schema, which every ORM and reporting tool reads.
    "SELECT table_name, table_type FROM information_schema.tables WHERE table_schema = 'public'",
    "SELECT column_name, data_type, is_nullable FROM information_schema.columns WHERE table_name = 'place'",
    "SELECT schema_name FROM information_schema.schemata",
    "SELECT constraint_name, constraint_type FROM information_schema.table_constraints WHERE table_name = 'place'",
    "SELECT column_name FROM information_schema.key_column_usage WHERE table_name = 'place'",
    // QGIS: the two relations a spatial layer cannot load without.
    "SELECT upper(type), srid, coord_dimension FROM geometry_columns WHERE f_table_name = 'place'",
    "SELECT f_table_schema, f_table_name, f_geometry_column FROM geometry_columns",
    "SELECT auth_name, auth_srid, srtext, proj4text FROM spatial_ref_sys WHERE srid = 4326",
    // e4's own words for the same facts.
    "SELECT * FROM db_tables",
    "SELECT * FROM db_columns WHERE table = 'place'",
    "SELECT * FROM db_indexes",
    "SELECT * FROM db_edges",
    "SELECT * FROM db_contexts",
    "SHOW TABLES",
    "SHOW place",
    "SHOW INDEXES ON place",
    "SHOW EDGES",
    "SHOW CREATE TABLE place",
];

#[test]
fn every_statement_a_client_issues_on_connect_answers_without_an_error() {
    let (_dir, mut db) = open();
    for statement in CLIENT_STATEMENTS {
        match db.sql(statement, &[]) {
            Ok(SqlResult::Rows { columns, .. }) => assert!(
                !columns.is_empty(),
                "`{statement}` answered rows with no columns"
            ),
            Ok(SqlResult::Notice(_)) => {}
            Ok(other) => panic!("`{statement}` answered {other:?}"),
            Err(error) => panic!("`{statement}` failed: {error}"),
        }
    }
}

#[test]
fn a_string_row_function_over_a_catalog_column_is_the_one_it_is_everywhere_else() {
    let (_dir, mut db) = open();
    // QGIS writes `upper(type)` over `geometry_columns`. Over a catalog view
    // that is the ordinary §4.1 row function -- one row in, one value out --
    // so it is compiled to the same `CompiledRow` a stored collection's
    // select list uses.
    let (columns, answer) = rows(
        &mut db,
        "SELECT upper(type) FROM geometry_columns ORDER BY f_geometry_column ASC",
    );
    assert_eq!(columns, ["upper(type)"]);
    assert_eq!(
        answer.iter().map(|r| text_at(r, 0)).collect::<Vec<_>>(),
        vec!["GEOMETRY", "POINT"]
    );
    // An alias names the column, and `lower` is the same function the
    // expression index folds with.
    let (columns, answer) = rows(&mut db, "SELECT lower(relkind) AS k FROM pg_class LIMIT 1");
    assert_eq!(columns, ["k"]);
    assert_eq!(text_at(&answer[0], 0), "r");
    // A §4.2 date function has nothing in a catalog row to act on, and says
    // so rather than answering NULL.
    let error = refuse(&mut db, "SELECT date_trunc('day', relname) FROM pg_class");
    assert!(
        format!("{error}").contains("§4.1 STRING set"),
        "{error}"
    );
}

// ── refusals ─────────────────────────────────────────────────────────────

#[test]
fn a_pg_catalog_relation_this_surface_does_not_have_is_refused_by_name() {
    let (_dir, mut db) = open();
    for (name, reason) in catalog::NOT_PROVIDED {
        let statement = format!("SELECT * FROM {name}");
        let error = refuse(&mut db, &statement);
        match &error {
            SqlError::Refused { keyword, reason: got, .. } => {
                assert_eq!(
                    keyword.to_ascii_lowercase(),
                    *name,
                    "`{statement}` refused `{keyword}`"
                );
                assert!(
                    !got.is_empty() && got.contains("QL_CONTRACT"),
                    "`{name}` has no contract reason"
                );
            }
            other => panic!("`{statement}` produced {other:?}, not a refusal by name"),
        }
        assert!(!reason.is_empty(), "`{name}` has no reason in the list");
    }
    // postgis_version() is refused for the same rule and says what it is
    // waiting on, because a version string for an absent library is read as
    // a promise that the library answers.
    let error = refuse(&mut db, "SELECT _id FROM place WHERE postgis_version");
    assert!(
        format!("{error}").contains("p3-geometry-io"),
        "{error}"
    );
}

#[test]
fn a_predicate_the_row_list_cannot_answer_is_refused_rather_than_scanned() {
    let (_dir, mut db) = open();
    for (statement, wanted) in [
        (
            "SELECT * FROM db_tables WHERE no_such_column = 'x'",
            "has no column",
        ),
        (
            "SELECT count(*) FROM db_tables",
            "no aggregate driver",
        ),
        (
            "SELECT name FROM db_tables GROUP BY name",
            "no aggregate driver",
        ),
        (
            "SELECT name FROM db_tables ORDER BY name <-> ST_MakePoint(1, 2)::geography",
            "names an index",
        ),
    ] {
        let error = refuse(&mut db, statement);
        let shown = format!("{error}");
        assert!(shown.contains(wanted), "`{statement}` said `{shown}`");
    }
}

#[test]
fn a_constant_predicate_is_folded_at_prepare_over_a_view_and_over_a_collection() {
    let (_dir, mut db) = open();
    // A driver writes `WHERE 1<>1 LIMIT 1` to learn a result's COLUMNS
    // without fetching a row. The truth value is decided by the parser, so
    // there is nothing left for an index to answer: FALSE is a `LIMIT 0`
    // and TRUE is no predicate at all. Neither emulates anything.
    let (columns, answer) = rows(&mut db, "SELECT * FROM pg_class WHERE 1 <> 1");
    assert_eq!(columns.len(), relation("pg_catalog.pg_class").columns.len());
    assert!(answer.is_empty());
    let (_, all) = rows(&mut db, "SELECT relname FROM pg_class WHERE 1 = 1");
    assert_eq!(all.len(), 2);
    // Over a stored collection, the same two answers and the same columns.
    let (columns, answer) = rows(&mut db, "SELECT _key, name FROM place WHERE 1 <> 1 LIMIT 1");
    assert_eq!(columns, ["_key", "name"]);
    assert!(answer.is_empty(), "a constant FALSE returns no rows");
    let (_, every) = rows(&mut db, "SELECT _key FROM place WHERE 1 = 1");
    assert_eq!(every.len(), 2, "a constant TRUE is no predicate at all");
    // A constant nested under a boolean operator is NOT that shape, and is
    // refused by name rather than folded by a second evaluator.
    let error = refuse(&mut db, "SELECT _key FROM place WHERE 1 <> 1 OR name = 'x'");
    assert!(
        format!("{error}").contains("constant admits or rejects every row"),
        "{error}"
    );
}

// ── the plan ─────────────────────────────────────────────────────────────

#[test]
fn explain_names_the_rows_driver_and_states_what_it_cost() {
    let (_dir, mut db) = open();
    let SqlResult::Explain(text) = run(
        &mut db,
        "EXPLAIN SELECT name FROM db_tables WHERE fields > 5 ORDER BY name ASC LIMIT 1",
    ) else {
        panic!("EXPLAIN did not answer a plan")
    };
    assert!(text.contains("driver: rows(db_tables)"), "{text}");
    assert!(
        text.contains("2 row(s) built from the catalog at prepare"),
        "{text}"
    );
    assert!(text.contains("fields > 5"), "{text}");
    assert!(text.contains("order: name ASC"), "{text}");
    assert!(text.contains("limit: 1"), "{text}");
    assert!(
        text.contains("no index is opened, no row is read"),
        "the plan has to say what the driver costs:\n{text}"
    );
}

#[test]
fn the_rows_a_view_answers_track_a_catalog_change() {
    let (_dir, mut db) = open();
    let before = rows(&mut db, "SELECT name FROM db_tables").1.len();
    db.sql("CREATE TABLE extra (n BIGINT) WITH (index: none)", &[])
        .unwrap();
    assert_eq!(
        rows(&mut db, "SELECT name FROM db_tables").1.len(),
        before + 1,
        "a view built at prepare sees the collection that was just created"
    );
    db.sql("CREATE INDEX extra_n ON extra USING btree (n)", &[])
        .unwrap();
    let (_, indexes) = rows(&mut db, "SELECT name FROM db_indexes WHERE table = 'extra'");
    assert_eq!(indexes.len(), 1);
    assert_eq!(text_at(&indexes[0], 0), "extra_n");
    db.sql("DROP TABLE extra CASCADE", &[]).unwrap();
    assert_eq!(
        rows(&mut db, "SELECT name FROM db_tables").1.len(),
        before,
        "a dropped collection leaves every view at once"
    );
    assert!(
        rows(&mut db, "SELECT name FROM db_indexes WHERE table = 'extra'")
            .1
            .is_empty(),
        "its index left with it"
    );
}

#[test]
fn a_database_with_no_graph_answers_the_edge_views_empty_rather_than_failing() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("db"), config()).unwrap();
    db.sql("CREATE TABLE only (n BIGINT)", &[]).unwrap();
    db.commit().unwrap();
    let (columns, answer) = rows(&mut db, "SELECT * FROM db_edges");
    assert_eq!(columns.len(), 4);
    assert!(answer.is_empty(), "no graph feature is not a corrupt graph");
    // The BASE GRAPH is a property of the model, not of the file
    // (GRAPH_CONTRACT 3.1: no context means the base graph), so it is listed
    // whether or not an edge has ever been written. Nothing else is.
    let (_, contexts) = rows(&mut db, "SELECT name FROM db_contexts");
    assert_eq!(
        contexts.iter().map(|r| text_at(r, 0)).collect::<Vec<_>>(),
        vec!["(base graph)"],
        "no dictionary, no NAMED contexts, no error"
    );
    // …and the ordinary catalog views still answer.
    let (_, tables) = rows(&mut db, "SELECT name FROM db_tables");
    assert_eq!(tables.len(), 1);
}

#[test]
fn a_collection_id_a_view_reports_is_the_one_the_catalog_holds() {
    let (_dir, mut db) = open();
    let (_, answer) = rows(&mut db, "SELECT name, id FROM db_tables ORDER BY id ASC");
    for row in &answer {
        let name = text_at(row, 0);
        let CollectionId(id) = db.collection(&name).unwrap().unwrap();
        assert_eq!(int_at(row, 1), i64::from(id), "`{name}`");
    }
}
