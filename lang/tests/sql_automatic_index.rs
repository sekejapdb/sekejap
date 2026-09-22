//! The AUTOMATIC indexes of `docs/lang/INDEX_CONTRACT.md`.
//!
//! The rule that contract fixes, in one line: **an index is DECLARED when
//! there is a decision and AUTOMATIC when there is not.** Choosing between an
//! exact vector index and a quantized one is a decision -- recall against
//! hundreds of gigabytes. Choosing how to index a `SMALLINT` is not: there is
//! one implementation, it costs a few bytes per row, and nobody would ever
//! pick differently. So every `TEXT`, `INT`, `REAL`, `BOOLEAN`, `TIMESTAMPTZ`,
//! `DATE` and `GEOMETRY` column is indexed with the collection, and `VECTOR`
//! and `JSONB` are not.
//!
//! Nothing here weakens `QL_CONTRACT` §6: a predicate on a column with no
//! index is still REFUSED and never demoted to a scan. What changes is which
//! columns have one without being asked, and the escape --
//! `WITH (index: none)` and `WITH (index: [column, ...])` -- is what this file
//! exercises beside the default.
//!
//! The ORACLE of the first test is a brute-force filter held in the test
//! process: every row this file writes is kept in a `Vec` and the answer of
//! each predicate is compared against the same predicate evaluated in Rust
//! over that `Vec`. An index that answered a subset, a superset or a
//! differently-ordered set is what that comparison catches; an index that was
//! never created is what the refusals catch.

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::{Database, IndexFamily, IndexState};
use sekejap_lang::{Param, SqlDatabase, SqlResult, SqlValue};
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

fn refuse(db: &mut Database, text: &str) -> String {
    db.sql(text, &[])
        .expect_err(&format!("`{text}` was accepted"))
        .to_string()
}

/// The `_key`s a SELECT answered, in the order it answered them.
fn keys(db: &mut Database, text: &str) -> Vec<String> {
    match run(db, text) {
        SqlResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|row| match &row.values[0] {
                SqlValue::Text(t) => t.clone(),
                other => panic!("column 0 is {other:?}, not text"),
            })
            .collect(),
        other => panic!("`{text}` answered {other:?} rather than rows"),
    }
}

/// The same, sorted, for a predicate whose statement fixes no order.
fn sorted(db: &mut Database, text: &str) -> Vec<String> {
    let mut out = keys(db, text);
    out.sort();
    out
}

/// `(name, field, family, expression?)` for every index a collection holds,
/// sorted by name.
fn indexes(db: &Database, table: &str) -> Vec<(String, String, IndexFamily, bool)> {
    let c = db.collection(table).unwrap().expect("the collection");
    let mut out: Vec<_> = db
        .list_indexes(c)
        .unwrap()
        .into_iter()
        .map(|i| {
            assert_eq!(
                i.state,
                IndexState::Ready,
                "`{}` was handed back half-built",
                i.name
            );
            (i.name, i.field, i.family, i.expression.is_some())
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn names(db: &Database, table: &str) -> Vec<String> {
    indexes(db, table).into_iter().map(|i| i.0).collect()
}

// ── the oracle ────────────────────────────────────────────────────────────

/// One row of `thing`, held in the test process so every answer below has a
/// brute-force computation to be compared against.
#[derive(Clone)]
struct Row {
    key: String,
    label: String,
    small: i64,
    big: i64,
    ratio: f64,
    exact: f64,
    live: bool,
    at: String,
    day: String,
}

/// Twelve rows, chosen so every predicate below has a proper subset to find:
/// `small` ranges over 0..12, `live` alternates, `label` sorts differently
/// from `_key`, and the two time columns advance one day at a time.
fn corpus() -> Vec<Row> {
    const LABELS: [&str; 12] = [
        "sawah", "kebun", "pasar", "danau", "hutan", "kopi", "taman", "desa", "bukit", "pantai",
        "muara", "jalan",
    ];
    (0..12)
        .map(|n| Row {
            key: format!("r{n:02}"),
            label: LABELS[n].to_owned(),
            small: n as i64,
            big: 1_000_000_000 + n as i64,
            ratio: n as f64 / 4.0,
            exact: n as f64 / 8.0,
            live: n % 2 == 0,
            at: format!("2026-03-{:02}T09:00:00Z", n + 1),
            day: format!("2026-03-{:02}", n + 1),
        })
        .collect()
}

/// `thing`, one column of every kind the contract's automatic column lists,
/// plus the two it does not (`JSONB`, `VECTOR`). NO `CREATE INDEX` anywhere.
const THING: &str = "CREATE TABLE thing (\
     label TEXT, \
     small SMALLINT, \
     big BIGINT, \
     ratio REAL, \
     exact DOUBLE PRECISION, \
     live BOOLEAN, \
     at TIMESTAMPTZ, \
     day DATE, \
     spot GEOMETRY(Point,4326), \
     plot GEOMETRY(Polygon,4326), \
     props JSONB, \
     emb VECTOR(4))";

fn fill(db: &mut Database, rows: &[Row]) {
    for (n, row) in rows.iter().enumerate() {
        run(
            db,
            &format!(
                "INSERT INTO thing (_key, label, small, big, ratio, exact, live, at, day, spot, plot, props, emb) \
                 VALUES ('{}', '{}', {}, {}, {}, {}, {}, '{}', '{}', \
                 '{{\"type\":\"Point\",\"coordinates\":[{:.4},{:.4}]}}', \
                 '{{\"type\":\"Polygon\",\"coordinates\":[[[106.0,-6.2],[106.6,-6.2],[106.6,-6.0],[106.0,-6.0],[106.0,-6.2]]]}}', \
                 '{{\"n\": {n}}}', '[1.0, 0.0, 0.0, 0.0]')",
                row.key,
                row.label,
                row.small,
                row.big,
                row.ratio,
                row.exact,
                row.live,
                row.at,
                row.day,
                106.0 + n as f64 * 0.01,
                -6.0 - n as f64 * 0.01,
            ),
        );
    }
    run(db, "COMMIT");
}

// ── the tests ─────────────────────────────────────────────────────────────

#[test]
fn a_table_created_with_no_indexes_answers_an_equality_a_range_and_an_order_by_on_every_eligible_kind(
) {
    let (_dir, mut db) = open();
    let said = match run(&mut db, THING) {
        SqlResult::Notice(said) => said,
        other => panic!("CREATE TABLE answered {other:?} rather than a notice"),
    };
    // Nothing is created that the caller was not told about, which is the
    // rule the key mapping and the WITH sugar already keep.
    assert!(
        said.contains("10 automatic index(es)")
            && said.contains("`thing_small_btree` (btree) over `small` INT")
            && said.contains("`thing_spot_gist` (gist) over `spot` GEOMETRY(Point,4326)"),
        "the statement lists what it created: {said}"
    );
    assert_eq!(
        names(&db, "thing"),
        [
            "thing_at_btree",
            "thing_big_btree",
            "thing_day_btree",
            "thing_exact_btree",
            "thing_label_btree",
            "thing_live_btree",
            "thing_plot_gist",
            "thing_ratio_btree",
            "thing_small_btree",
            "thing_spot_gist",
        ],
        "ten eligible columns, ten indexes; `props` and `emb` get nothing"
    );

    let rows = corpus();
    fill(&mut db, &rows);

    // Equality, on every eligible scalar kind, against the brute force.
    let oracle = |f: &dyn Fn(&Row) -> bool| -> Vec<String> {
        let mut out: Vec<String> = rows.iter().filter(|r| f(r)).map(|r| r.key.clone()).collect();
        out.sort();
        out
    };
    assert_eq!(
        sorted(&mut db, "SELECT _key FROM thing WHERE label = 'kebun'"),
        oracle(&|r| r.label == "kebun")
    );
    assert_eq!(
        sorted(&mut db, "SELECT _key FROM thing WHERE small = 7"),
        oracle(&|r| r.small == 7)
    );
    assert_eq!(
        sorted(&mut db, "SELECT _key FROM thing WHERE big = 1000000005"),
        oracle(&|r| r.big == 1_000_000_005)
    );
    assert_eq!(
        sorted(&mut db, "SELECT _key FROM thing WHERE ratio = 1.5"),
        oracle(&|r| r.ratio == 1.5)
    );
    assert_eq!(
        sorted(&mut db, "SELECT _key FROM thing WHERE exact = 0.75"),
        oracle(&|r| r.exact == 0.75)
    );
    assert_eq!(
        sorted(&mut db, "SELECT _key FROM thing WHERE live = true"),
        oracle(&|r| r.live)
    );
    assert_eq!(
        sorted(&mut db, "SELECT _key FROM thing WHERE day = '2026-03-05'"),
        oracle(&|r| r.day == "2026-03-05")
    );

    // Ranges, on the kinds a range is the reason to index.
    assert_eq!(
        sorted(&mut db, "SELECT _key FROM thing WHERE small BETWEEN 3 AND 6"),
        oracle(&|r| (3..=6).contains(&r.small))
    );
    assert_eq!(
        sorted(&mut db, "SELECT _key FROM thing WHERE big >= 1000000009"),
        oracle(&|r| r.big >= 1_000_000_009)
    );
    assert_eq!(
        sorted(&mut db, "SELECT _key FROM thing WHERE ratio < 1.0"),
        oracle(&|r| r.ratio < 1.0)
    );
    assert_eq!(
        sorted(&mut db, "SELECT _key FROM thing WHERE at >= '2026-03-10T00:00:00Z'"),
        oracle(&|r| r.at.as_str() >= "2026-03-10")
    );
    assert_eq!(
        sorted(&mut db, "SELECT _key FROM thing WHERE label LIKE 'k%'"),
        oracle(&|r| r.label.starts_with('k')),
        "a text prefix is a range over the same automatic btree"
    );

    // ORDER BY, which is the other half of what a btree is for. The oracle
    // sorts the corpus itself rather than asserting a written-out list.
    let mut by_label: Vec<&Row> = rows.iter().collect();
    by_label.sort_by(|a, b| a.label.cmp(&b.label));
    assert_eq!(
        keys(&mut db, "SELECT _key FROM thing ORDER BY label ASC"),
        by_label.iter().map(|r| r.key.clone()).collect::<Vec<_>>()
    );
    let mut by_ratio: Vec<&Row> = rows.iter().collect();
    by_ratio.sort_by(|a, b| b.ratio.partial_cmp(&a.ratio).unwrap());
    assert_eq!(
        keys(&mut db, "SELECT _key FROM thing ORDER BY ratio DESC LIMIT 4"),
        by_ratio
            .iter()
            .take(4)
            .map(|r| r.key.clone())
            .collect::<Vec<_>>()
    );

    // And the spatial half: the point family answers a radius, over the same
    // index nobody asked for.
    let near = sorted(
        &mut db,
        "SELECT _key FROM thing WHERE ST_DWithin(spot, ST_SetSRID(ST_MakePoint(106.0, -6.0), 4326)::geography, 2000)",
    );
    assert!(
        !near.is_empty() && near.len() < rows.len(),
        "the automatic point index answers a proper subset: {near:?}"
    );
}

#[test]
fn with_index_none_creates_none_and_the_predicate_is_then_refused_exactly_as_before() {
    let (_dir, mut db) = open();
    match run(
        &mut db,
        "CREATE TABLE bare (label TEXT, small SMALLINT) WITH (index: none)",
    ) {
        SqlResult::Affected(0) => {}
        other => panic!("a create that indexed nothing answered {other:?}"),
    }
    assert!(
        names(&db, "bare").is_empty(),
        "`index: none` creates none: {:?}",
        names(&db, "bare")
    );
    run(
        &mut db,
        "INSERT INTO bare (_key, label, small) VALUES ('a', 'x', 1)",
    );
    let said = refuse(&mut db, "SELECT _key FROM bare WHERE small = 1");
    assert!(
        said.contains("scalar index on `small` does not exist"),
        "the refusal is the one QL_CONTRACT §6 always gave, naming the column: {said}"
    );
}

#[test]
fn with_index_naming_one_column_creates_that_one_and_a_predicate_on_another_is_refused_naming_it() {
    let (_dir, mut db) = open();
    let said = match run(
        &mut db,
        "CREATE TABLE pick (a TEXT, b TEXT) WITH (index: [a])",
    ) {
        SqlResult::Notice(said) => said,
        other => panic!("answered {other:?}"),
    };
    assert!(
        said.contains("1 automatic index(es)") && said.contains("`pick_a_btree`"),
        "the notice names the one it made: {said}"
    );
    assert_eq!(names(&db, "pick"), ["pick_a_btree"]);
    run(&mut db, "INSERT INTO pick (_key, a, b) VALUES ('k', 'x', 'y')");
    assert_eq!(
        sorted(&mut db, "SELECT _key FROM pick WHERE a = 'x'"),
        ["k".to_owned()]
    );
    let said = refuse(&mut db, "SELECT _key FROM pick WHERE b = 'y'");
    assert!(
        said.contains("scalar index on `b` does not exist"),
        "the column the clause did not name is refused BY NAME: {said}"
    );

    // A column the table does not declare is refused before anything is
    // written, the way the sugar's own undeclared column is.
    let said = refuse(
        &mut db,
        "CREATE TABLE gone (a TEXT) WITH (index: [absent])",
    );
    assert!(
        said.contains("`absent` is not a column of `gone`"),
        "an undeclared column is refused by name: {said}"
    );
    assert!(db.collection("gone").unwrap().is_none());
}

#[test]
fn an_explicit_fulltext_entry_still_wins_under_index_none() {
    let (_dir, mut db) = open();
    let said = match run(
        &mut db,
        "CREATE TABLE doc (body TEXT, n INT) WITH (index: none, fulltext: [body])",
    ) {
        SqlResult::Notice(said) => said,
        other => panic!("answered {other:?}"),
    };
    assert!(
        said.contains("`fulltext` became a `gin`"),
        "the declared entry keeps its own notice: {said}"
    );
    assert_eq!(
        names(&db, "doc"),
        ["doc_body_gin"],
        "`index: none` turned off the automatic btree over `body` and left the declared gin"
    );
    run(
        &mut db,
        "INSERT INTO doc (_key, body, n) VALUES ('d1', 'kebun raya sawah', 1)",
    );
    assert_eq!(
        sorted(
            &mut db,
            "SELECT _key FROM doc WHERE to_tsvector('simple', body) @@ to_tsquery('simple', 'kebun')"
        ),
        ["d1".to_owned()]
    );
    // The btree that `index: none` refused is genuinely absent: the text
    // index answers a match and cannot answer an equality.
    let said = refuse(&mut db, "SELECT _key FROM doc WHERE body = 'kebun raya sawah'");
    assert!(
        said.contains("scalar index on `body` does not exist"),
        "a gin is not a btree: {said}"
    );

    // A declared spatial entry under `index: none` is the same story, and it
    // generates the SAME name the automatic one would have.
    run(
        &mut db,
        "CREATE TABLE site (loc GEOMETRY(Point,4326)) WITH (index: none, spatial: [loc])",
    );
    assert_eq!(names(&db, "site"), ["site_loc_gist"]);
}

#[test]
fn alter_table_add_column_of_an_eligible_kind_gets_its_index() {
    let (_dir, mut db) = open();
    run(&mut db, "CREATE TABLE grow (a TEXT) WITH (index: none)");
    run(&mut db, "INSERT INTO grow (_key, a) VALUES ('k1', 'x')");
    run(&mut db, "COMMIT");
    assert!(names(&db, "grow").is_empty());

    run(&mut db, "ALTER TABLE grow ADD COLUMN n INT");
    assert_eq!(
        names(&db, "grow"),
        ["grow_n_btree"],
        "an added column of an eligible kind is an indexed column"
    );
    // The index is built over the rows that were already there, so a
    // predicate answers as soon as the statement returns -- and a row written
    // before the column exists reads MISSING for it, which is not a match.
    run(&mut db, "INSERT INTO grow (_key, a, n) VALUES ('k2', 'y', 5)");
    run(&mut db, "COMMIT");
    assert_eq!(
        sorted(&mut db, "SELECT _key FROM grow WHERE n = 5"),
        ["k2".to_owned()]
    );

    // A VECTOR column added the same way gets NOTHING, for the same reason a
    // declared one does.
    run(&mut db, "ALTER TABLE grow ADD COLUMN emb VECTOR(4)");
    assert_eq!(names(&db, "grow"), ["grow_n_btree"]);
}

#[test]
fn a_vector_column_gets_nothing_automatically_and_its_predicate_names_every_family() {
    let (_dir, mut db) = open();
    run(&mut db, "CREATE TABLE vec (label TEXT, emb VECTOR(4))");
    assert_eq!(
        names(&db, "vec"),
        ["vec_label_btree"],
        "the TEXT column is automatic and the VECTOR one is not"
    );
    run(
        &mut db,
        "INSERT INTO vec (_key, label, emb) VALUES ('v1', 'x', '[1.0, 0.0, 0.0, 0.0]')",
    );
    run(&mut db, "COMMIT");
    let said = refuse(
        &mut db,
        "SELECT _key FROM vec ORDER BY emb <=> '[1.0, 0.0, 0.0, 0.0]' LIMIT 1",
    );
    assert!(
        said.contains("no vector index on `emb`")
            && said.contains("exact")
            && said.contains("quantized")
            && said.contains("vamana"),
        "the refusal names ALL THREE families, because that is the decision it is asking for: {said}"
    );

    // And a JSONB column is the contract's stated gap: no family covers it,
    // so naming it in `index:` is refused rather than silently ignored.
    let said = refuse(
        &mut db,
        "CREATE TABLE doc (payload JSONB) WITH (index: [payload])",
    );
    assert!(
        said.contains("JSONB") && said.contains("no family"),
        "a JSONB column named in `index:` is refused by name: {said}"
    );
}

#[test]
fn the_automatic_index_and_a_hand_written_create_index_for_the_same_column_do_not_produce_two_indexes(
) {
    let (_dir, mut db) = open();
    run(&mut db, "CREATE TABLE one (n INT, loc GEOMETRY(Point,4326))");
    assert_eq!(names(&db, "one"), ["one_loc_gist", "one_n_btree"]);

    let said = match run(&mut db, "CREATE INDEX one_n ON one USING btree (n)") {
        SqlResult::Notice(said) => said,
        other => panic!("a duplicate CREATE INDEX answered {other:?}"),
    };
    assert!(
        said.contains("`one_n_btree` already indexes `n`"),
        "the statement names the index that is already there: {said}"
    );
    assert_eq!(
        names(&db, "one"),
        ["one_loc_gist", "one_n_btree"],
        "one index over `n`, not two"
    );
    // The same for the spatial family, whose automatic index comes from the
    // declared shape rather than from a key.
    run(&mut db, "CREATE INDEX one_loc ON one USING gist (loc)");
    assert_eq!(names(&db, "one"), ["one_loc_gist", "one_n_btree"]);

    // A DIFFERENT index over the same column is not a duplicate and is
    // created: an expression index stores the fold, not the column.
    run(
        &mut db,
        "CREATE TABLE two (label TEXT) WITH (index: [label])",
    );
    run(&mut db, "CREATE INDEX two_label_lower ON two (lower(label))");
    assert_eq!(
        indexes(&db, "two"),
        vec![
            (
                "two_label_btree".to_owned(),
                "label".to_owned(),
                IndexFamily::Scalar,
                false
            ),
            (
                "two_label_lower".to_owned(),
                "label".to_owned(),
                IndexFamily::Scalar,
                true
            ),
        ],
        "the plain index and the expression index over one column are two indexes"
    );
}

#[test]
fn the_automatic_indexes_are_in_the_catalog_after_a_reopen_and_still_answer() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let before = {
        let mut db = Database::create(&path, config()).unwrap();
        run(&mut db, THING);
        fill(&mut db, &corpus());
        let held = indexes(&db, "thing");
        db.commit().unwrap();
        held
    };
    let mut db = Database::open(&path, config()).unwrap();
    assert_eq!(
        indexes(&db, "thing"),
        before,
        "an automatic index is an ordinary catalog descriptor and survives the close"
    );
    let rows = corpus();
    assert_eq!(
        sorted(&mut db, "SELECT _key FROM thing WHERE small BETWEEN 3 AND 6"),
        rows.iter()
            .filter(|r| (3..=6).contains(&r.small))
            .map(|r| r.key.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        sorted(&mut db, "SELECT _key FROM thing WHERE live = false"),
        rows.iter()
            .filter(|r| !r.live)
            .map(|r| r.key.clone())
            .collect::<Vec<_>>()
    );
    // And a row written AFTER the reopen is maintained in them, which is what
    // "maintained on every write" means.
    run(
        &mut db,
        "INSERT INTO thing (_key, label, small, live) VALUES ('r99', 'zzz', 4, false)",
    );
    run(&mut db, "COMMIT");
    assert!(sorted(&mut db, "SELECT _key FROM thing WHERE small = 4").contains(&"r99".to_owned()));
}

#[test]
fn the_override_is_statement_scoped_and_a_later_add_column_indexes_whatever_it_said() {
    // Nothing about `index: none` is stored: no descriptor field and no
    // feature bit, so the catalog carries no memory of the choice. That is
    // said out loud here, because the alternative -- a table that remembers
    // it opted out -- is a format change this does not make.
    let (_dir, mut db) = open();
    run(&mut db, "CREATE TABLE scoped (a TEXT) WITH (index: none)");
    assert!(names(&db, "scoped").is_empty());
    run(&mut db, "ALTER TABLE scoped ADD COLUMN b TEXT");
    assert_eq!(
        names(&db, "scoped"),
        ["scoped_b_btree"],
        "the ADD COLUMN indexes its column whatever the CREATE TABLE said"
    );
}

#[test]
fn a_rename_of_an_automatically_indexed_column_re_earns_the_index_under_the_new_name() {
    let (_dir, mut db) = open();
    run(&mut db, "CREATE TABLE ren (a TEXT)");
    assert_eq!(names(&db, "ren"), ["ren_a_btree"]);
    // The collection is empty, which `RENAME COLUMN` already required, so the
    // index over the old name holds no entry to move.
    run(&mut db, "ALTER TABLE ren RENAME COLUMN a TO b");
    assert_eq!(names(&db, "ren"), ["ren_b_btree"]);
    run(&mut db, "INSERT INTO ren (_key, b) VALUES ('k', 'x')");
    run(&mut db, "COMMIT");
    assert_eq!(
        sorted(&mut db, "SELECT _key FROM ren WHERE b = 'x'"),
        ["k".to_owned()]
    );

    // A HAND-WRITTEN index still refuses the rename by name: the caller chose
    // that name and the statement will not take it away silently.
    run(&mut db, "CREATE TABLE held (a TEXT) WITH (index: none)");
    run(&mut db, "CREATE INDEX mine ON held USING btree (a)");
    let said = refuse(&mut db, "ALTER TABLE held RENAME COLUMN a TO b");
    assert!(
        said.contains("the index `mine` names `a`"),
        "a hand-written index is still the refusal it was: {said}"
    );
}

#[test]
fn a_table_too_wide_to_carry_its_automatic_indexes_is_refused_with_the_number() {
    let (_dir, mut db) = open();
    let columns: Vec<String> = (0..70).map(|n| format!("c{n} INT")).collect();
    let said = refuse(
        &mut db,
        &format!("CREATE TABLE wide ({})", columns.join(", ")),
    );
    assert!(
        said.contains("would be 70") && said.contains("at most 64") && said.contains("index: none"),
        "the refusal counts, states the ceiling and names the escape: {said}"
    );
    assert!(
        db.collection("wide").unwrap().is_none(),
        "the refusal is raised while the statement compiles, so nothing was written"
    );
    // And the escape works, which is what makes the refusal actionable.
    run(
        &mut db,
        &format!("CREATE TABLE wide ({}) WITH (index: [c0])", columns.join(", ")),
    );
    assert_eq!(names(&db, "wide"), ["wide_c0_btree"]);
}

// ── what automatic costs, measured ────────────────────────────────────────

/// The one real objection to an automatic index is the WRITE, so it is
/// measured rather than argued about: a table of N scalar columns maintains N
/// postings per inserted row, including for the columns nobody filters.
///
/// The two arms differ ONLY in `WITH (index: none)`, and every insert is the
/// SAME statement text with `$n` parameters, so the parse and the compile
/// happen once (plan-cache hit) and what is timed is the write path. The
/// difference between the arms, divided by the number of columns, is the cost
/// of one index per inserted row.
///
/// `#[ignore]`d because it is a measurement and not an assertion -- it is run
/// on purpose (`--ignored --nocapture`) and its output is the table in the
/// report. What it asserts is only the shape nobody should have to re-derive:
/// the cost is LINEAR in the number of indexed columns, not worse.
#[test]
#[ignore]
fn the_per_insert_cost_of_the_automatic_indexes_is_linear_in_the_number_of_columns() {
    use std::time::Instant;
    const ROWS: usize = 20_000;

    fn measure(width: usize, automatic: bool) -> f64 {
        let dir = TempDir::new().unwrap();
        // A larger managed-byte allowance than the rest of this file uses: a
        // 32-column table with its 32 indexes writes more per row than the
        // 1 MiB default holds between commits, and an allowance refusal
        // inside the timed span would be measuring the allowance.
        let big = Config {
            budget_bytes: 64 << 20,
            ..config()
        };
        let mut db = Database::create(dir.path().join("db"), big).unwrap();
        let columns: Vec<String> = (0..width).map(|n| format!("c{n} INT")).collect();
        let list: Vec<String> = (0..width).map(|n| format!("c{n}")).collect();
        run(
            &mut db,
            &format!(
                "CREATE TABLE w ({}){}",
                columns.join(", "),
                if automatic { "" } else { " WITH (index: none)" }
            ),
        );
        // One statement text, bound `ROWS` times: `$1` is the key and `$2..`
        // are the columns, so the plan is compiled once and every iteration
        // after the first is a cache hit.
        let holes: Vec<String> = (0..=width).map(|n| format!("${}", n + 1)).collect();
        let sql = format!(
            "INSERT INTO w (_key, {}) VALUES ({})",
            list.join(", "),
            holes.join(", ")
        );
        let mut params: Vec<Param> = Vec::with_capacity(width + 1);
        // A warm-up row, so the compile is not inside the timed span.
        params.push(Param::Text("warm".to_owned()));
        for n in 0..width {
            params.push(Param::Int(n as i64));
        }
        db.sql(&sql, &params).unwrap();
        db.commit().unwrap();

        let started = Instant::now();
        for r in 0..ROWS {
            params[0] = Param::Text(format!("k{r:06}"));
            for (n, slot) in params.iter_mut().skip(1).enumerate() {
                *slot = Param::Int((r + n) as i64);
            }
            db.sql(&sql, &params).unwrap();
            // One commit per thousand rows, in BOTH arms, so the durability
            // barrier is a constant of the comparison rather than a term in
            // it.
            if r % 1_000 == 999 {
                db.commit().unwrap();
            }
        }
        db.commit().unwrap();
        started.elapsed().as_secs_f64() * 1e6 / ROWS as f64
    }

    println!("columns  index:none (us/insert)  automatic (us/insert)  per index (us)");
    let mut rows = Vec::new();
    for width in [1usize, 4, 8, 16, 32] {
        let bare = measure(width, false);
        let auto = measure(width, true);
        let per = (auto - bare) / width as f64;
        rows.push((width, bare, auto, per));
        println!("{width:>7}  {bare:>22.2}  {auto:>21.2}  {per:>13.2}");
    }
    // Linear, not worse. Per-index cost at 32 columns within a factor of
    // three of per-index cost at 8; a quadratic maintenance path would be
    // four times worse over that span.
    let eight = rows[2].3;
    let thirty_two = rows[4].3;
    assert!(
        thirty_two < eight * 3.0,
        "per-index cost should not grow with the number of columns: {eight:.2} us at 8, {thirty_two:.2} us at 32"
    );
}
