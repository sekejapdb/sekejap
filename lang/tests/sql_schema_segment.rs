//! Named schemas through SQL (`docs/lang/QL_CONTRACT.md` §2): `CREATE
//! SCHEMA`, `schema.table` wherever a statement names a table, the catalog
//! views reporting the schema, and the geometry path of a GIS client run
//! against a table outside `public`.

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{SqlDatabase, SqlResult, SqlValue};
use tempfile::TempDir;

fn open(statements: &[&str]) -> (TempDir, Database) {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(
        dir.path().join("db"),
        Config {
            budget_bytes: 1 << 20,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )
    .unwrap();
    for statement in statements {
        db.sql(statement, &[])
            .unwrap_or_else(|e| panic!("`{statement}` was refused: {e}"));
    }
    (dir, db)
}

fn rows(db: &mut Database, text: &str) -> Vec<Vec<SqlValue>> {
    match db.sql(text, &[]) {
        Ok(SqlResult::Rows { rows, .. }) => rows.into_iter().map(|row| row.values).collect(),
        other => panic!("`{text}` answered {other:?}"),
    }
}

fn texts(db: &mut Database, text: &str) -> Vec<String> {
    let mut out: Vec<String> = rows(db, text)
        .into_iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    SqlValue::Text(t) => t.clone(),
                    SqlValue::Int(n) => n.to_string(),
                    SqlValue::Float(f) => f.to_string(),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect();
    out.sort();
    out
}

fn refusal(db: &mut Database, text: &str) -> String {
    format!("{}", db.sql(text, &[]).expect_err(&format!("`{text}` was accepted")))
}

const TWO_ORDERS: &[&str] = &[
    "CREATE SCHEMA sales",
    "CREATE TABLE orders (total INT)",
    "CREATE TABLE sales.orders (total INT)",
    "INSERT INTO orders (_key, total) VALUES ('a', 1)",
    "INSERT INTO sales.orders (_key, total) VALUES ('a', 100), ('b', 200)",
];

#[test]
fn a_qualified_name_reaches_its_own_schema_and_public_t_is_t() {
    let (_dir, mut db) = open(TWO_ORDERS);
    assert_eq!(texts(&mut db, "SELECT _key, total FROM orders"), ["a|1"]);
    assert_eq!(texts(&mut db, "SELECT _key, total FROM public.orders"), ["a|1"]);
    assert_eq!(
        texts(&mut db, "SELECT _key, total FROM sales.orders"),
        ["a|100", "b|200"]
    );
    assert_eq!(texts(&mut db, "SELECT _key FROM sales.orders WHERE total > 150"), ["b"]);
    db.sql("UPDATE sales.orders SET total = 300 WHERE _key = 'b'", &[]).unwrap();
    db.sql("DELETE FROM sales.orders WHERE _key = 'a'", &[]).unwrap();
    assert_eq!(texts(&mut db, "SELECT _key, total FROM sales.orders"), ["b|300"]);
    assert_eq!(texts(&mut db, "SELECT _key, total FROM orders"), ["a|1"], "public untouched");
    let error = refusal(&mut db, "SELECT _key FROM nowhere.orders");
    assert!(error.contains("nowhere.orders"), "{error}");
}

#[test]
fn the_catalog_views_report_the_schema() {
    let (_dir, mut db) = open(TWO_ORDERS);
    let schemata = texts(&mut db, "SELECT schema_name FROM information_schema.schemata");
    assert!(schemata.contains(&"sales".to_owned()), "{schemata:?}");
    assert!(schemata.contains(&"public".to_owned()), "{schemata:?}");
    assert_eq!(
        texts(
            &mut db,
            "SELECT table_schema, table_name FROM information_schema.tables WHERE table_name = 'orders'"
        ),
        ["public|orders", "sales|orders"]
    );
    let namespaces = texts(&mut db, "SELECT nspname FROM pg_catalog.pg_namespace");
    assert!(namespaces.contains(&"sales".to_owned()), "{namespaces:?}");
    // Two relations, two oids, two namespaces.
    let classes = rows(
        &mut db,
        "SELECT oid, relnamespace FROM pg_catalog.pg_class WHERE relname = 'orders'",
    );
    assert_eq!(classes.len(), 2);
    assert_ne!(classes[0][0], classes[1][0]);
    assert_ne!(classes[0][1], classes[1][1]);
    assert_eq!(
        texts(&mut db, "SELECT schemaname, tablename FROM pg_catalog.pg_tables WHERE tablename = 'orders'"),
        ["public|orders", "sales|orders"]
    );
    let tables = texts(&mut db, "SELECT name FROM db_tables");
    assert!(tables.contains(&"sales.orders".to_owned()), "{tables:?}");
    // The automatic index of a table in a named schema carries the schema in
    // its name, because index names are one namespace here.
    let indexes = texts(
        &mut db,
        "SELECT schemaname, indexname FROM pg_catalog.pg_indexes WHERE tablename = 'orders'",
    );
    assert!(
        indexes.iter().any(|i| i.starts_with("sales|sales_orders_total")),
        "{indexes:?}"
    );
    assert!(indexes.iter().any(|i| i.starts_with("public|orders_total")), "{indexes:?}");
}

#[test]
fn schema_ddl_is_restrict_and_says_what_it_did() {
    let (_dir, mut db) = open(TWO_ORDERS);
    let error = refusal(&mut db, "DROP SCHEMA sales");
    assert!(error.contains("orders"), "{error}");
    let error = refusal(&mut db, "DROP SCHEMA sales CASCADE");
    assert!(error.contains("CASCADE"), "{error}");
    let error = refusal(&mut db, "CREATE TABLE nowhere.t (x INT)");
    assert!(error.contains("does not exist"), "{error}");
    let error = refusal(&mut db, "CREATE SCHEMA pg_mine");
    assert!(error.contains("reserved"), "{error}");
    assert!(matches!(
        db.sql("CREATE SCHEMA IF NOT EXISTS sales", &[]).unwrap(),
        SqlResult::Notice(_)
    ));
    // A rename stays inside the schema.
    db.sql("ALTER TABLE sales.orders RENAME TO archive", &[]).unwrap();
    assert_eq!(texts(&mut db, "SELECT _key FROM sales.archive"), ["a", "b"]);
    assert_eq!(texts(&mut db, "SELECT _key FROM orders"), ["a"]);
    db.sql("DROP TABLE sales.archive", &[]).unwrap();
    db.sql("DROP SCHEMA sales", &[]).unwrap();
    assert!(matches!(
        db.sql("DROP SCHEMA IF EXISTS sales", &[]).unwrap(),
        SqlResult::Notice(_)
    ));
    let schemata = texts(&mut db, "SELECT schema_name FROM information_schema.schemata");
    assert!(!schemata.contains(&"sales".to_owned()), "{schemata:?}");
}

/// What a GIS client does with a layer that lives outside `public`: find it
/// in `geometry_columns` by schema and table, draw it with `ST_AsBinary`
/// under a `&&` canvas filter, and write an edit back as WKB.
#[test]
fn a_gis_layer_in_a_named_schema_is_found_drawn_and_edited() {
    let (_dir, mut db) = open(&[
        "CREATE SCHEMA gis",
        "CREATE TABLE gis.parcels (shape GEOMETRY(Polygon,4326), label TEXT)",
        "INSERT INTO gis.parcels (_key, shape, label) VALUES ('p1', ST_GeomFromText('POLYGON((0 0,1 0,1 1,0 1,0 0))', 4326), 'near')",
        "INSERT INTO gis.parcels (_key, shape, label) VALUES ('p2', ST_GeomFromText('POLYGON((50 50,51 50,51 51,50 51,50 50))', 4326), 'far')",
    ]);
    assert_eq!(
        texts(
            &mut db,
            "SELECT f_table_schema, f_table_name, f_geometry_column, srid FROM geometry_columns WHERE f_table_schema = 'gis'"
        ),
        ["gis|parcels|shape|4326"]
    );
    let drawn = rows(
        &mut db,
        "SELECT _key, ST_AsBinary(shape, 'NDR') FROM gis.parcels WHERE shape && ST_MakeEnvelope(-1, -1, 2, 2, 4326)",
    );
    assert_eq!(drawn.len(), 1);
    assert_eq!(drawn[0][0], SqlValue::Text("p1".into()));
    let SqlValue::Text(wkb) = &drawn[0][1] else {
        panic!("{:?}", drawn[0][1])
    };
    // The WKB goes back in unchanged, as an edit would send it.
    db.sql(
        &format!("UPDATE gis.parcels SET shape = ST_GeomFromWKB('{wkb}', 4326) WHERE _key = 'p2'"),
        &[],
    )
    .unwrap();
    assert_eq!(
        texts(&mut db, "SELECT _key FROM gis.parcels WHERE shape && ST_MakeEnvelope(-1, -1, 2, 2, 4326)"),
        ["p1", "p2"]
    );
}
