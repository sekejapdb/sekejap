//! `CREATE TABLE t (...) WITH (hash: [...], range: [...], ...)`: the INDEX
//! SUGAR of `docs/lang/QL_CONTRACT.md` §2.
//!
//! sekejap REFUSES a predicate on an unindexed column (§6, no silent scan),
//! and that is not what this sugar changes. What it changes is the CEREMONY:
//! the columns a caller wants answered are named once, where the table is
//! declared, instead of in five statements written after it. Nothing is
//! created that the caller did not name, and every mapping says out loud
//! which family it became and why -- `hash` and `range` are both a `btree`
//! because there is no separate hash family here, `fulltext` and `bm25` are
//! both a `gin` over `to_tsvector('simple', col)` because BM25 is how the one
//! text family scores rather than a family of its own.
//!
//! The ORACLE of the first test is the long-hand statements themselves: the
//! same schema is built twice, once through the sugar and once through one
//! `CREATE TABLE` and N `CREATE INDEX`, in two databases, and the CATALOG
//! DESCRIPTORS of the two are compared field by field -- not the count of
//! them. If the sugar compiled to anything other than what the row says it
//! compiles to, that comparison is what fails.

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::{Database, IndexFamily, IndexState};
use sekejap_core::Kind;
use sekejap_lang::{SqlDatabase, SqlError, SqlResult, SqlValue};
use tempfile::TempDir;

fn config() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn open() -> (TempDir, Database) {
    let dir = TempDir::new().unwrap();
    let db = Database::create(dir.path().join("db"), config()).unwrap();
    (dir, db)
}

fn run(db: &mut Database, text: &str) -> SqlResult {
    db.sql(text, &[])
        .unwrap_or_else(|e| panic!("`{text}` was refused: {e:?}"))
}

fn refuse(db: &mut Database, text: &str) -> SqlError {
    db.sql(text, &[])
        .expect_err(&format!("`{text}` was accepted"))
}

/// The notice a statement answered with. The sugar answers a NOTICE rather
/// than a bare count precisely because nothing may be created unannounced.
fn notice(db: &mut Database, text: &str) -> String {
    match run(db, text) {
        SqlResult::Notice(said) => said,
        other => panic!("`{text}` answered {other:?} rather than a notice"),
    }
}

/// The `_key`s one SELECT answered, sorted here rather than by an `ORDER BY`
/// -- the point under test is which rows the predicate reaches, and `_key` is
/// not one of the columns the `WITH` clause named.
fn keys(db: &mut Database, text: &str) -> Vec<String> {
    let mut out: Vec<String> = match run(db, text) {
        SqlResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|row| match &row.values[0] {
                SqlValue::Text(t) => t.clone(),
                other => panic!("column 0 is {other:?}, not text"),
            })
            .collect(),
        other => panic!("`{text}` answered {other:?} rather than rows"),
    };
    out.sort();
    out
}

/// One index descriptor, with the two fields that cannot match across two
/// separate databases dropped: the index IDENTITY and the per-index TREE id
/// are drawn from a monotone counter, so they say when an index was made and
/// not what it is.
type Descriptor = (String, String, IndexFamily, Kind, bool, IndexState, bool);

fn descriptors(db: &Database, table: &str) -> Vec<Descriptor> {
    let c = db.collection(table).unwrap().expect("the collection");
    let mut out: Vec<Descriptor> = db
        .list_indexes(c)
        .unwrap()
        .into_iter()
        .map(|i| {
            (
                i.name,
                i.field,
                i.family,
                i.kind,
                i.unique,
                i.state,
                i.expression.is_some(),
            )
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn layout(db: &Database, table: &str) -> Vec<(String, Kind)> {
    let c = db.collection(table).unwrap().expect("the collection");
    db.collection_info(c).unwrap().layout.fields.clone()
}

/// The columns both halves of the oracle declare: one of every `Kind` a
/// `WITH` key can name.
const COLUMNS: &str = "name TEXT, label TEXT, founded INT, rating DOUBLE PRECISION, active BOOLEAN, loc GEOMETRY(Point,4326), area GEOMETRY(Polygon,4326), emb VECTOR(4)";

#[test]
fn the_sugar_builds_exactly_the_catalog_the_long_hand_statements_build() {
    let (_sugar_dir, mut sugar) = open();
    let said = notice(
        &mut sugar,
        &format!(
            "CREATE TABLE t ({COLUMNS}) WITH (hash: [name, active], range: [founded, rating], fulltext: [label], spatial: [loc, area], vector: [emb], quantized: [emb])"
        ),
    );
    assert!(
        said.contains("9 index(es)"),
        "the statement says how many indexes it made: {said}"
    );

    // The long hand, in a SECOND database: one `CREATE TABLE` and one
    // `CREATE INDEX` per named column, written under the names the sugar
    // generates so that the descriptors are comparable name and all.
    let (_hand_dir, mut hand) = open();
    run(&mut hand, &format!("CREATE TABLE t ({COLUMNS})"));
    for statement in [
        "CREATE INDEX t_name_btree ON t USING btree (name)",
        "CREATE INDEX t_active_btree ON t USING btree (active)",
        "CREATE INDEX t_founded_btree ON t USING btree (founded)",
        "CREATE INDEX t_rating_btree ON t USING btree (rating)",
        "CREATE INDEX t_label_gin ON t USING gin (to_tsvector('simple', label))",
        "CREATE INDEX t_loc_gist ON t USING gist (loc)",
        "CREATE INDEX t_area_gist ON t USING gist (area)",
        "CREATE INDEX t_emb_exact ON t USING exact (emb)",
        "CREATE INDEX t_emb_quantized ON t USING quantized (emb)",
    ] {
        run(&mut hand, statement);
    }

    assert_eq!(
        layout(&sugar, "t"),
        layout(&hand, "t"),
        "the sugar declares the same columns in the same order"
    );
    let made = descriptors(&sugar, "t");
    assert_eq!(
        made,
        descriptors(&hand, "t"),
        "the sugar builds the same index DESCRIPTORS, not merely the same number of them"
    );
    assert_eq!(made.len(), 9, "nine columns were named, nine were indexed");
    // Every one of them is READY: the sugar runs each build to the end, the
    // way `CREATE INDEX` does, rather than handing back a half-built index.
    assert!(
        made.iter().all(|d| d.5 == IndexState::Ready),
        "every generated index is READY: {made:?}"
    );
}

#[test]
fn every_key_maps_to_the_family_the_contract_names_and_the_notice_says_so() {
    let (_dir, mut db) = open();

    let said = notice(
        &mut db,
        "CREATE TABLE a (c TEXT) WITH (hash: [c])",
    );
    assert!(
        said.contains("`hash` became a `btree` over `c`") && said.contains("a_c_btree"),
        "hash names the btree it became and the index it created: {said}"
    );
    assert_eq!(descriptors(&db, "a")[0].2, IndexFamily::Scalar);

    let said = notice(&mut db, "CREATE TABLE b (c INT) WITH (range: [c])");
    assert!(
        said.contains("`range` became a `btree` over `c`") && said.contains("b_c_btree"),
        "range names the btree it became: {said}"
    );
    assert_eq!(descriptors(&db, "b")[0].2, IndexFamily::Scalar);

    let said = notice(&mut db, "CREATE TABLE c (t TEXT) WITH (fulltext: [t])");
    assert!(
        said.contains("`fulltext` became a `gin` over `to_tsvector('simple', t)`")
            && said.contains("c_t_gin"),
        "fulltext names the gin and the tsvector it is over: {said}"
    );
    assert_eq!(descriptors(&db, "c")[0].2, IndexFamily::Text);

    let said = notice(&mut db, "CREATE TABLE d (t TEXT) WITH (bm25: [t])");
    assert!(
        said.contains("`bm25` became a `gin` over `to_tsvector('simple', t)`")
            && said.contains("d_t_gin"),
        "bm25 names the same gin: {said}"
    );
    assert_eq!(descriptors(&db, "d")[0].2, IndexFamily::Text);

    let said = notice(
        &mut db,
        "CREATE TABLE e (p GEOMETRY(Point,4326), g GEOMETRY(Polygon,4326)) WITH (spatial: [p, g])",
    );
    assert!(
        said.contains("`spatial` became a `gist` over `p`")
            && said.contains("`spatial` became a `gist` over `g`"),
        "spatial names the gist for both spatial kinds: {said}"
    );
    let spatial = descriptors(&db, "e");
    assert_eq!(spatial[0].2, IndexFamily::SpatialGeometry, "e_g_gist");
    assert_eq!(spatial[1].2, IndexFamily::SpatialPoint, "e_p_gist");

    let said = notice(
        &mut db,
        "CREATE TABLE f (v VECTOR(4)) WITH (vector: [v], quantized: [v])",
    );
    assert!(
        said.contains("`vector` became an `exact` vector index over `v`")
            && said.contains("`quantized` became a `quantized` vector index over `v`")
            && said.contains("APPROXIMATE"),
        "the two vector keys name their two families, and the approximate one says so: {said}"
    );
    let vectors = descriptors(&db, "f");
    assert_eq!(vectors[0].2, IndexFamily::ExactVector, "f_v_exact");
    assert_eq!(vectors[1].2, IndexFamily::QuantizedVector, "f_v_quantized");
}

#[test]
fn a_family_illegal_for_the_column_kind_is_refused_by_name() {
    let (_dir, mut db) = open();

    let said = refuse(
        &mut db,
        "CREATE TABLE t (c TEXT) WITH (spatial: [c])",
    )
    .to_string();
    assert!(
        said.contains("spatial") && said.contains("gist") && said.contains("Text"),
        "a gist on a TEXT column names the key, the family and the Kind: {said}"
    );

    let said = refuse(&mut db, "CREATE TABLE t (n INT) WITH (fulltext: [n])").to_string();
    assert!(
        said.contains("fulltext") && said.contains("gin") && said.contains("Int"),
        "a gin on an INT column names the key, the family and the Kind: {said}"
    );

    let said = refuse(&mut db, "CREATE TABLE t (v VECTOR(4)) WITH (hash: [v])").to_string();
    assert!(
        said.contains("hash") && said.contains("btree") && said.contains("Vector"),
        "a btree on a VECTOR column names the key, the family and the Kind: {said}"
    );

    let said = refuse(&mut db, "CREATE TABLE t (c TEXT) WITH (vector: [c])").to_string();
    assert!(
        said.contains("vector") && said.contains("exact") && said.contains("Text"),
        "an exact index on a TEXT column names the key, the family and the Kind: {said}"
    );

    // A refused clause creates NOTHING: the refusal is raised while the
    // statement compiles, before the collection is made.
    assert!(
        db.collection("t").unwrap().is_none(),
        "a refused WITH clause leaves no collection behind"
    );
}

#[test]
fn an_unknown_key_is_refused_by_name_and_lists_the_keys_that_exist() {
    let (_dir, mut db) = open();
    for (statement, named) in [
        ("CREATE TABLE t (c TEXT) WITH (gin: [c])", "gin"),
        ("CREATE TABLE t (c TEXT) WITH (btree: [c])", "btree"),
        // PostgreSQL's own storage parameters take this same clause; here
        // they are refused by name rather than ignored.
        ("CREATE TABLE t (c TEXT) WITH (fillfactor = [70])", "fillfactor"),
    ] {
        let said = refuse(&mut db, statement).to_string();
        assert!(
            said.contains(named),
            "the refusal names the key that was written: {said}"
        );
        for key in [
            "hash",
            "range",
            "fulltext",
            "bm25",
            "spatial",
            "vector",
            "quantized",
        ] {
            assert!(
                said.contains(key),
                "the refusal lists the key `{key}` that does exist: {said}"
            );
        }
        assert!(
            !said.contains("syntax"),
            "an unknown key is refused by name, not as a bare syntax error: {said}"
        );
    }
    assert!(db.collection("t").unwrap().is_none());
}

#[test]
fn a_column_the_table_does_not_declare_is_refused_by_name() {
    let (_dir, mut db) = open();
    let said = refuse(&mut db, "CREATE TABLE t (c TEXT) WITH (hash: [nowhere])").to_string();
    assert!(
        said.contains("nowhere") && said.contains("not a column"),
        "the refusal names the column the clause invented: {said}"
    );
    assert!(db.collection("t").unwrap().is_none());
}

#[test]
fn the_generated_name_is_table_column_family_and_a_collision_is_refused_by_name() {
    let (_dir, mut db) = open();
    notice(&mut db, "CREATE TABLE t (c TEXT, d INT) WITH (hash: [c])");
    assert_eq!(
        descriptors(&db, "t")[0].0,
        "t_c_btree",
        "the naming rule is <table>_<column>_<family>, and the family is the one the key BECAME"
    );

    // `hash` and `range` are one and the same btree, so naming both for one
    // column generates one name twice. That is told, not silently deduped.
    let said = refuse(
        &mut db,
        "CREATE TABLE u (c TEXT) WITH (hash: [c], range: [c])",
    )
    .to_string();
    assert!(
        said.contains("u_c_btree") && said.contains("earlier pair"),
        "two keys that are one family over one column collide by name: {said}"
    );
    assert!(db.collection("u").unwrap().is_none());

    // A name already held elsewhere in the database is refused too, because
    // `DROP INDEX <name>` resolves a bare name over the whole catalog.
    run(&mut db, "CREATE INDEX v_c_btree ON t USING btree (d)");
    let said = refuse(&mut db, "CREATE TABLE v (c TEXT) WITH (hash: [c])").to_string();
    assert!(
        said.contains("v_c_btree") && said.contains("already an index"),
        "a generated name that is held is refused and names it: {said}"
    );
    assert!(db.collection("v").unwrap().is_none());
}

#[test]
fn a_refusal_part_way_through_the_clause_leaves_neither_the_collection_nor_an_index() {
    let (_dir, mut db) = open();
    // Everything a compiler can decide is decided before a byte is written,
    // so reaching the run-time half takes a refusal only the ENGINE can
    // raise. This is one: an index name is 1..128 UTF-8 bytes, and the
    // generated name of the second column is longer than that while the
    // first column's is not. The first index is therefore created and built,
    // and the second is refused.
    let long = "c".repeat(140);
    let said = refuse(
        &mut db,
        &format!("CREATE TABLE t (a TEXT, {long} TEXT) WITH (hash: [a], hash: [{long}])"),
    )
    .to_string();
    assert!(
        said.contains("left NOTHING behind") && said.contains("t_a_btree"),
        "the refusal says what it removed, naming the index it had already built: {said}"
    );
    assert!(
        db.collection("t").unwrap().is_none(),
        "the collection the statement made is gone with it"
    );
    // And the name is free: the statement can be written again, correctly.
    notice(&mut db, "CREATE TABLE t (a TEXT) WITH (hash: [a])");
    assert_eq!(descriptors(&db, "t").len(), 1);
}

#[test]
fn a_query_that_needed_one_of_those_indexes_answers_immediately_after_the_single_statement() {
    let (_dir, mut db) = open();
    notice(
        &mut db,
        "CREATE TABLE place (name TEXT, kind TEXT, born INT, note TEXT) WITH (hash: [kind], range: [born], fulltext: [name])",
    );
    for (key, name, kind, born) in [
        ("p1", "north mill", "mill", 1901),
        ("p2", "south shop", "shop", 1955),
        ("p3", "east shop", "shop", 1990),
    ] {
        run(
            &mut db,
            &format!(
                "INSERT INTO place (_key, name, kind, born, note) VALUES ('{key}', '{name}', '{kind}', {born}, 'x')"
            ),
        );
    }

    // Three predicates, three families, and not one further statement
    // between the CREATE TABLE and the answer.
    assert_eq!(
        keys(&mut db, "SELECT _key FROM place WHERE kind = 'shop'"),
        vec!["p2".to_owned(), "p3".to_owned()]
    );
    assert_eq!(
        keys(&mut db, "SELECT _key FROM place WHERE born >= 1950"),
        vec!["p2".to_owned(), "p3".to_owned()]
    );
    assert_eq!(
        keys(
            &mut db,
            "SELECT _key FROM place WHERE to_tsvector('simple', name) @@ to_tsquery('simple', 'mill')"
        ),
        vec!["p1".to_owned()]
    );

    // The explicitness the sugar does not remove: `note` was not named, so a
    // predicate on it is still REFUSED by name rather than scanned.
    let said = refuse(&mut db, "SELECT _key FROM place WHERE note = 'x'").to_string();
    assert!(
        said.contains("note") && said.contains("does not exist"),
        "an unnamed column is still refused, which is the point of naming them: {said}"
    );
}

#[test]
fn if_not_exists_over_a_collection_that_is_there_creates_no_index_and_says_so() {
    let (_dir, mut db) = open();
    notice(&mut db, "CREATE TABLE t (c TEXT, d INT) WITH (hash: [c])");
    let said = notice(
        &mut db,
        "CREATE TABLE IF NOT EXISTS t (c TEXT, d INT) WITH (hash: [d])",
    );
    assert!(
        said.contains("already in the catalog") && said.contains("no index of the WITH clause"),
        "a create that did not happen indexes nothing, and says so: {said}"
    );
    assert_eq!(
        descriptors(&db, "t").len(),
        1,
        "the second statement created nothing at all"
    );
}

#[test]
fn a_with_clause_that_names_no_key_is_refused_by_name_rather_than_ignored() {
    let (_dir, mut db) = open();
    let said = refuse(&mut db, "CREATE TABLE t (c TEXT) WITH ()").to_string();
    assert!(
        said.contains("hash") && said.contains("quantized"),
        "an empty clause names the keys it could have taken: {said}"
    );
    assert!(db.collection("t").unwrap().is_none());
}
