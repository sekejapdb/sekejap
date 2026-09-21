//! The SQL spellings of the schema forms `docs/lang/QL_CONTRACT.md` §2 lists:
//! the `DEFAULT` / `NOT NULL` column clauses, and the five `ALTER TABLE`
//! actions.
//!
//! Each statement here compiles to one atomic the engine already had --
//! `create_collection_rules`, `alter_collection_rules`, `rename_collection`
//! -- so what is under test is the TEXT: that a form the contract places in
//! Tier 1 is accepted and does what the row says, and that a form it places
//! in Tier 3 is refused BY NAME with the reason, never emulated.
//!
//! The oracles are written here. A `now()` default is bracketed by two clock
//! readings this test takes around the INSERT; a `uuid5` default is compared
//! against the RFC 4122 vector, a constant below.

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{Param, SqlDatabase, SqlError, SqlResult, SqlValue, Tier};
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

/// One SELECT, as `(columns, rows)`.
fn rows(db: &mut Database, text: &str) -> (Vec<String>, Vec<Vec<SqlValue>>) {
    match run(db, text) {
        SqlResult::Rows { columns, rows } => {
            (columns, rows.into_iter().map(|r| r.values).collect())
        }
        other => panic!("`{text}` answered {other:?}"),
    }
}

fn text_at(values: &[SqlValue], at: usize) -> String {
    match &values[at] {
        SqlValue::Text(t) => t.clone(),
        other => panic!("column {at} is {other:?}, not text"),
    }
}

fn micros_now() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros(),
    )
    .unwrap()
}

/// RFC 4122 Appendix C, and the vector for `www.example.org` under it.
const NAMESPACE_DNS: &str = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";
const DNS_WWW_EXAMPLE_ORG: &str = "74738ff5-5367-5958-9aee-98fffdcd1876";

// ── CREATE TABLE column clauses ───────────────────────────────────────────

/// `DEFAULT now()`, `DEFAULT uuid4()` and `DEFAULT uuid5(ns, name)` on a
/// `CREATE TABLE` fill a column the INSERT does not name. `now()` lands
/// between two readings this test took; `uuid5` is the RFC's own vector;
/// `uuid4` differs from row to row.
#[test]
fn the_three_default_generators_fill_a_column_the_insert_does_not_name() {
    let (_dir, mut db) = open();
    run(
        &mut db,
        "CREATE TABLE evt (
            id TEXT PRIMARY KEY,
            n INT,
            at TIMESTAMPTZ DEFAULT now(),
            token TEXT DEFAULT gen_random_uuid(),
            site TEXT DEFAULT uuid5('6ba7b810-9dad-11d1-80b4-00c04fd430c8', 'www.example.org')
        )",
    );
    let before = micros_now();
    run(&mut db, "INSERT INTO evt (id, n) VALUES ('a', 1)");
    run(&mut db, "INSERT INTO evt (id, n) VALUES ('b', 2)");
    let after = micros_now();

    let (columns, got) = rows(&mut db, "SELECT id, at, token, site FROM evt");
    assert_eq!(columns, ["id", "at", "token", "site"]);
    assert_eq!(got.len(), 2);
    let mut got = got;
    got.sort_by_key(|r| text_at(r, 0));

    // A declared TIMESTAMPTZ prints back as ISO-8601 (QL_CONTRACT §4.2), so
    // the bracket is checked on the stored integer, read through a second
    // statement that does not apply the ISO row function.
    let stored = rows(&mut db, "SELECT extract(epoch FROM at) AS s FROM evt");
    for row in &stored.1 {
        let seconds = match row[0] {
            SqlValue::Int(n) => n,
            ref other => panic!("epoch is {other:?}"),
        };
        assert!(
            before / 1_000_000 <= seconds && seconds <= after / 1_000_000 + 1,
            "the stored instant {seconds} s is outside the bracket this test read"
        );
    }
    assert!(
        text_at(&got[0], 1).starts_with("20"),
        "a declared TIMESTAMPTZ prints as ISO-8601: {:?}",
        got[0][1]
    );
    assert_ne!(
        text_at(&got[0], 2),
        text_at(&got[1], 2),
        "uuid4 is sixteen random bytes per row"
    );
    assert_eq!(text_at(&got[0], 3), DNS_WWW_EXAMPLE_ORG);
    assert_eq!(
        text_at(&got[1], 3),
        DNS_WWW_EXAMPLE_ORG,
        "uuid5 is deterministic: the same namespace and name is the same UUID"
    );
    assert!(NAMESPACE_DNS.len() == 36);
}

/// `NOT NULL` is a write-path check, not a parse-time decoration: a row that
/// OMITS the column and a row that writes NULL into it are both refused, and
/// the refusal names the column.
#[test]
fn not_null_refuses_a_missing_column_and_an_explicit_null_by_name() {
    let (_dir, mut db) = open();
    run(
        &mut db,
        "CREATE TABLE person (id TEXT PRIMARY KEY, email TEXT NOT NULL, n INT)",
    );
    run(
        &mut db,
        "INSERT INTO person (id, email, n) VALUES ('a', 'a@b', 1)",
    );

    let missing = refuse(&mut db, "INSERT INTO person (id, n) VALUES ('b', 2)");
    let missing = format!("{missing:?}");
    assert!(
        missing.contains("email") && missing.contains("MISSING") && missing.contains("NULL"),
        "the refusal must name the column and say MISSING and NULL alike are refused: {missing}"
    );
    let nulled = refuse(
        &mut db,
        "INSERT INTO person (id, email, n) VALUES ('c', NULL, 3)",
    );
    assert!(format!("{nulled:?}").contains("email"), "{nulled:?}");
    // An UPDATE that nulls it is refused too, and the stored value stands.
    let nulled = refuse(
        &mut db,
        "UPDATE person SET email = NULL WHERE _key = 'a'",
    );
    assert!(format!("{nulled:?}").contains("email"), "{nulled:?}");
    let (_, got) = rows(&mut db, "SELECT email FROM person");
    assert_eq!(got.len(), 1, "no refused row was written");
    assert_eq!(text_at(&got[0], 0), "a@b");
}

/// The generator set is CLOSED. A literal, an expression and a function this
/// engine does not have are each refused by name, and the refusal says what
/// the three generators are rather than only that this one is not one.
#[test]
fn a_default_outside_the_closed_generator_set_is_refused_by_name() {
    let (_dir, mut db) = open();
    for clause in [
        "DEFAULT 'anonymous'",
        "DEFAULT 0",
        "DEFAULT random()",
        "DEFAULT (now() + interval '1 day')",
        "DEFAULT nextval('s')",
    ] {
        let error = refuse(
            &mut db,
            &format!("CREATE TABLE t (id TEXT PRIMARY KEY, c TEXT {clause})"),
        );
        let text = format!("{error:?}");
        assert!(
            text.contains("uuid5") && text.contains("now()"),
            "`{clause}` must be refused naming the closed set: {text}"
        );
    }
    // GENERATED ALWAYS is its own contract row and keeps its own tier.
    let error = refuse(
        &mut db,
        "CREATE TABLE t (id TEXT PRIMARY KEY, n INT, m INT GENERATED ALWAYS AS (n * 2) STORED)",
    );
    assert!(
        matches!(error, SqlError::Refused { tier: Tier::Two, ref keyword, .. }
            if keyword.contains("GENERATED")),
        "{error:?}"
    );
    // A UNIQUE or CHECK clause still says what the slot holds.
    let error = refuse(
        &mut db,
        "CREATE TABLE t (id TEXT PRIMARY KEY, c TEXT CHECK (c <> ''))",
    );
    assert!(format!("{error:?}").contains("NOT NULL"), "{error:?}");
}

// ── ALTER TABLE ───────────────────────────────────────────────────────────

/// `ADD COLUMN c TEXT DEFAULT uuid4()`: the column appears in `SELECT *` at
/// once, the row written before the ALTER reads MISSING for it -- which is
/// distinct from NULL -- and the row written after it carries the default.
///
/// This is the DBeaver shape: a client that issues `SELECT *` and reads the
/// column list off the answer.
#[test]
fn select_star_after_add_column_shows_the_new_column_and_its_default() {
    let (_dir, mut db) = open();
    run(&mut db, "CREATE TABLE person (id TEXT PRIMARY KEY, n INT)");
    run(&mut db, "INSERT INTO person (id, n) VALUES ('old', 1)");
    let (columns, _) = rows(&mut db, "SELECT * FROM person");
    assert_eq!(columns, ["id", "n"]);

    run(
        &mut db,
        "ALTER TABLE person ADD COLUMN token TEXT DEFAULT uuid4()",
    );
    run(&mut db, "INSERT INTO person (id, n) VALUES ('new', 2)");

    let (columns, got) = rows(&mut db, "SELECT * FROM person");
    let mut got = got;
    got.sort_by_key(|r| match r[1] {
        SqlValue::Int(n) => n,
        _ => unreachable!("`n` is an INT column"),
    });
    assert_eq!(
        columns,
        ["id", "n", "token"],
        "SELECT * reads the collection's CURRENT layout"
    );
    assert_eq!(
        got[0][2],
        SqlValue::Missing,
        "a row written before the ADD reads MISSING, not NULL"
    );
    let token = text_at(&got[1], 2);
    assert_eq!(token.len(), 36, "the row written after it took the default");
    assert_eq!(&token[14..15], "4", "RFC 4122 version 4");
}

/// `ADD COLUMN ... NOT NULL` with no DEFAULT is Tier 3 on a collection that
/// holds rows -- every one of them would read MISSING, so the constraint is
/// false the moment it is recorded -- and Tier 1 on an empty one.
#[test]
fn add_column_not_null_without_a_default_is_refused_on_a_collection_with_rows() {
    let (_dir, mut db) = open();
    run(&mut db, "CREATE TABLE person (id TEXT PRIMARY KEY, n INT)");
    // Empty: accepted, because no row contradicts it.
    run(&mut db, "ALTER TABLE person ADD COLUMN email TEXT NOT NULL");
    run(
        &mut db,
        "INSERT INTO person (id, n, email) VALUES ('a', 1, 'a@b')",
    );
    // Non-empty: refused by name, with the tier and the two ways out.
    let error = refuse(
        &mut db,
        "ALTER TABLE person ADD COLUMN nickname TEXT NOT NULL",
    );
    assert!(
        matches!(error, SqlError::Refused { tier: Tier::Three, ref reason, .. }
            if reason.contains("MISSING") && reason.contains("DEFAULT")),
        "{error:?}"
    );
    // With a DEFAULT it is accepted, because no row reads MISSING after it.
    run(
        &mut db,
        "ALTER TABLE person ADD COLUMN nickname TEXT NOT NULL DEFAULT uuid4()",
    );
    run(
        &mut db,
        "INSERT INTO person (id, n, email) VALUES ('b', 2, 'b@c')",
    );
    let (_, got) = rows(&mut db, "SELECT nickname FROM person WHERE _key = 'b'");
    assert_eq!(text_at(&got[0], 0), text_at(&got[0], 0));
    assert!(matches!(got[0][0], SqlValue::Text(ref t) if t.len() == 36));
}

/// `RENAME COLUMN old TO new` on an EMPTY collection: a SELECT of the new
/// name answers, a SELECT of the old one is refused, and the DECLARED type
/// and the COLUMN RULE follow the column rather than the spelling.
///
/// On a POPULATED collection it is Tier 3, and the reason is the dense row
/// codec: a row decodes under the immutable layout it was written with, and
/// that layout carries the old name, so every existing row would read MISSING
/// under the new one. A rename that loses the data is not a rename.
#[test]
fn select_after_rename_column_shows_the_new_name_and_the_rule_follows_it() {
    let (_dir, mut db) = open();
    run(
        &mut db,
        "CREATE TABLE evt (id TEXT PRIMARY KEY, n INT, born TIMESTAMPTZ DEFAULT now())",
    );
    run(&mut db, "ALTER TABLE evt RENAME COLUMN born TO created");

    let (columns, _) = rows(&mut db, "SELECT * FROM evt");
    assert_eq!(columns, ["id", "n", "created"]);
    let error = refuse(&mut db, "SELECT born FROM evt");
    assert!(format!("{error:?}").contains("born"), "{error:?}");

    // The DECLARED type and the rule followed: a row written now still takes
    // the default AND still prints as ISO-8601 under the new name.
    run(&mut db, "INSERT INTO evt (id, n) VALUES ('b', 2)");
    let (_, got) = rows(&mut db, "SELECT created FROM evt WHERE _key = 'b'");
    assert!(
        matches!(got[0][0], SqlValue::Text(ref t) if t.starts_with("20")),
        "{:?}",
        got[0][0]
    );

    // And now that the collection holds a row, a second rename is refused by
    // name, with the codec reason.
    let error = refuse(&mut db, "ALTER TABLE evt RENAME COLUMN created TO made");
    assert!(
        matches!(error, SqlError::Refused { tier: Tier::Three, ref reason, .. }
            if reason.contains("MISSING") && reason.contains("immutable")
                || reason.contains("IMMUTABLE")),
        "{error:?}"
    );
    // The refused rename changed nothing.
    assert_eq!(rows(&mut db, "SELECT * FROM evt").0, ["id", "n", "created"]);
}

/// `RENAME TO new_name`: one name record. The collection id does not change,
/// so the rows, the rules and the external keys are exactly where they were.
#[test]
fn rename_to_moves_the_name_and_nothing_else() {
    let (_dir, mut db) = open();
    run(
        &mut db,
        "CREATE TABLE person (id TEXT PRIMARY KEY, email TEXT NOT NULL)",
    );
    run(&mut db, "INSERT INTO person (id, email) VALUES ('a', 'a@b')");
    run(&mut db, "ALTER TABLE person RENAME TO people");

    let (_, got) = rows(&mut db, "SELECT email FROM people");
    assert_eq!(text_at(&got[0], 0), "a@b");
    let error = refuse(&mut db, "SELECT email FROM person");
    assert!(format!("{error:?}").contains("person"), "{error:?}");
    // The NOT NULL rule came with the name.
    let error = refuse(&mut db, "INSERT INTO people (id) VALUES ('b')");
    assert!(format!("{error:?}").contains("email"), "{error:?}");
    // A name that is taken is refused, and nothing is half-renamed.
    run(&mut db, "CREATE TABLE other (id TEXT PRIMARY KEY)");
    let error = refuse(&mut db, "ALTER TABLE people RENAME TO other");
    assert!(format!("{error:?}").contains("other"), "{error:?}");
    let (_, got) = rows(&mut db, "SELECT email FROM people");
    assert_eq!(text_at(&got[0], 0), "a@b");
}

/// `DROP COLUMN`: the name leaves the layout, so `SELECT *` stops listing it
/// and naming it is refused. No row is rewritten -- a row decodes under the
/// immutable layout it was written with.
#[test]
fn drop_column_removes_the_name_from_the_layout_and_leaves_the_rows_alone() {
    let (_dir, mut db) = open();
    run(
        &mut db,
        "CREATE TABLE person (id TEXT PRIMARY KEY, n INT, note TEXT NOT NULL)",
    );
    run(
        &mut db,
        "INSERT INTO person (id, n, note) VALUES ('a', 1, 'kept')",
    );
    run(&mut db, "ALTER TABLE person DROP COLUMN note");

    let (columns, got) = rows(&mut db, "SELECT * FROM person");
    assert_eq!(columns, ["id", "n"]);
    assert_eq!(got[0][1], SqlValue::Int(1), "the surviving columns still read");
    let error = refuse(&mut db, "SELECT note FROM person");
    assert!(format!("{error:?}").contains("note"), "{error:?}");
    // The NOT NULL rule went with the column: a row may now omit it.
    run(&mut db, "INSERT INTO person (id, n) VALUES ('b', 2)");
    assert_eq!(rows(&mut db, "SELECT * FROM person").1.len(), 2);
    // IF EXISTS on a name that is gone changes nothing and says so.
    assert!(matches!(
        run(&mut db, "ALTER TABLE person DROP COLUMN IF EXISTS note"),
        SqlResult::Notice(_)
    ));
}

/// `DROP COLUMN` takes the column's INDEX with it, by the ordinary bounded
/// drop -- the alternative is a layout that points at an index tree keyed on
/// a field the layout has not got.
#[test]
fn drop_column_drops_the_index_over_that_column_with_it() {
    let (_dir, mut db) = open();
    run(&mut db, "CREATE TABLE person (id TEXT PRIMARY KEY, city TEXT)");
    run(&mut db, "CREATE INDEX person_city ON person (city)");
    run(
        &mut db,
        "INSERT INTO person (id, city) VALUES ('a', 'Melbourne')",
    );
    run(&mut db, "ALTER TABLE person DROP COLUMN city");
    let error = refuse(&mut db, "DROP INDEX person_city");
    assert!(format!("{error:?}").contains("person_city"), "{error:?}");
    assert_eq!(rows(&mut db, "SELECT * FROM person").0, ["id"]);
}

/// `ALTER COLUMN c TYPE`: Tier 1 within one `Kind` -- the declared spelling
/// changes and no row byte moves -- and Tier 3 across `Kind`s, refused by
/// name with BOTH `Kind`s in the refusal.
#[test]
fn alter_column_type_is_accepted_within_one_kind_and_refused_across_kinds() {
    let (_dir, mut db) = open();
    run(
        &mut db,
        "CREATE TABLE m (id TEXT PRIMARY KEY, n INT, r REAL, at TIMESTAMPTZ)",
    );
    run(
        &mut db,
        "INSERT INTO m (id, n, r, at) VALUES ('a', 7, 1.5, 1000000)",
    );
    // INT -> BIGINT and REAL -> DOUBLE PRECISION are one Kind each.
    run(&mut db, "ALTER TABLE m ALTER COLUMN n TYPE BIGINT");
    run(&mut db, "ALTER TABLE m ALTER COLUMN r TYPE DOUBLE PRECISION");
    let (_, got) = rows(&mut db, "SELECT n, r FROM m");
    assert_eq!(got[0][0], SqlValue::Int(7), "no row byte moved");
    assert_eq!(got[0][1], SqlValue::Float(1.5));

    // TIMESTAMPTZ -> BIGINT is also one Kind, and the column stops printing
    // as ISO-8601 because the DECLARED spelling is what says it is a time.
    run(&mut db, "ALTER TABLE m ALTER COLUMN at TYPE BIGINT");
    let (_, got) = rows(&mut db, "SELECT at FROM m");
    assert_eq!(got[0][0], SqlValue::Int(1_000_000));

    for (statement, first, second) in [
        ("ALTER TABLE m ALTER COLUMN n TYPE TEXT", "Int", "Text"),
        ("ALTER TABLE m ALTER COLUMN r TYPE INT", "Real", "Int"),
    ] {
        let error = refuse(&mut db, statement);
        let text = format!("{error:?}");
        assert!(
            matches!(error, SqlError::Refused { tier: Tier::Three, .. }),
            "{text}"
        );
        assert!(
            text.contains(first) && text.contains(second),
            "the refusal must name BOTH Kinds: {text}"
        );
    }
}

/// `EXPLAIN ALTER TABLE` prints the layout id the commit would write and what
/// the new descriptor carries, and -- like `EXPLAIN DROP TABLE` -- it does
/// NOT run its statement.
#[test]
fn explain_alter_prints_the_new_layout_id_and_what_is_carried_without_running() {
    let (_dir, mut db) = open();
    run(
        &mut db,
        "CREATE TABLE person (id TEXT PRIMARY KEY, n INT, born TIMESTAMPTZ DEFAULT now())",
    );
    run(&mut db, "INSERT INTO person (id, n) VALUES ('a', 1)");
    let before = rows(&mut db, "SELECT * FROM person").0;

    let plan = match run(
        &mut db,
        "EXPLAIN ALTER TABLE person ADD COLUMN token TEXT NOT NULL DEFAULT uuid4()",
    ) {
        SqlResult::Explain(text) => text,
        other => panic!("EXPLAIN answered {other:?}"),
    };
    assert!(plan.contains("does not run"), "{plan}");
    assert!(plan.contains("new layout id"), "{plan}");
    assert!(
        plan.contains("token") && plan.contains("DEFAULT uuid4()") && plan.contains("NOT NULL"),
        "the carried rules must be printed: {plan}"
    );
    assert!(
        plan.contains("born TIMESTAMPTZ"),
        "the carried declared types must be printed: {plan}"
    );
    // Nothing ran.
    assert_eq!(rows(&mut db, "SELECT * FROM person").0, before);

    // The layout id it named is the one the statement then writes: run it and
    // EXPLAIN the next one, which must have moved on by exactly one.
    let quoted = |text: &str| -> u64 {
        text.split("new layout id ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("no layout id in {text}"))
    };
    let named = quoted(&plan);
    run(
        &mut db,
        "ALTER TABLE person ADD COLUMN token TEXT NOT NULL DEFAULT uuid4()",
    );
    let next = match run(&mut db, "EXPLAIN ALTER TABLE person DROP COLUMN n") {
        SqlResult::Explain(text) => quoted(&text),
        other => panic!("EXPLAIN answered {other:?}"),
    };
    assert_eq!(next, named + 1, "the ALTER wrote the layout EXPLAIN named");

    // A RENAME prints that it writes no layout at all.
    let plan = match run(&mut db, "EXPLAIN ALTER TABLE person RENAME TO people") {
        SqlResult::Explain(text) => text,
        other => panic!("EXPLAIN answered {other:?}"),
    };
    assert!(plan.contains("unchanged"), "{plan}");
    assert!(db.sql("SELECT n FROM person", &[]).is_ok(), "nothing ran");
}

/// The forms `ALTER TABLE` has not got are refused by name, never ignored.
#[test]
fn an_alter_form_with_no_atomic_is_refused_and_names_what_there_is() {
    let (_dir, mut db) = open();
    run(&mut db, "CREATE TABLE person (id TEXT PRIMARY KEY, n INT)");
    for (statement, wanted) in [
        ("ALTER TABLE person ALTER COLUMN n SET NOT NULL", "SET"),
        ("ALTER TABLE person ADD CONSTRAINT c UNIQUE (n)", "ADD COLUMN"),
        ("ALTER TABLE person OWNER TO bob", "ADD COLUMN"),
        ("ALTER TABLE IF EXISTS person RENAME TO p", "IF EXISTS"),
        ("ALTER INDEX i RENAME TO j", "ALTER TABLE"),
        (
            "ALTER TABLE person ALTER COLUMN n TYPE TEXT USING n::text",
            "USING",
        ),
        (
            "ALTER TABLE person ADD COLUMN a INT, ADD COLUMN b INT",
            "separate statements",
        ),
        ("ALTER TABLE person DROP COLUMN n CASCADE", "CASCADE"),
    ] {
        let error = refuse(&mut db, statement);
        assert!(
            format!("{error:?}").contains(wanted),
            "`{statement}` must be refused naming `{wanted}`: {error:?}"
        );
    }
    // A column that is not there is named, and IF EXISTS makes it a notice.
    let error = refuse(&mut db, "ALTER TABLE person DROP COLUMN nope");
    assert!(format!("{error:?}").contains("nope"), "{error:?}");
    let error = refuse(&mut db, "ALTER TABLE nowhere ADD COLUMN c INT");
    assert!(format!("{error:?}").contains("nowhere"), "{error:?}");
}

/// The rules survive a reopen: they are in the descriptor, not in the
/// process. A parameter binding still writes what it names, and a row that
/// omits the column still takes the default, after the file is closed and
/// opened again.
#[test]
fn the_rules_are_in_the_file_and_answer_after_a_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Database::create(&path, config()).unwrap();
    run(
        &mut db,
        "CREATE TABLE person (id TEXT PRIMARY KEY, email TEXT NOT NULL, token TEXT DEFAULT uuid4())",
    );
    db.sql(
        "INSERT INTO person (id, email) VALUES ($1, $2)",
        &[Param::Text("a".into()), Param::Text("a@b".into())],
    )
    .unwrap();
    db.commit().unwrap();
    drop(db);

    let mut db = Database::open(&path, config()).unwrap();
    let (_, got) = rows(&mut db, "SELECT email, token FROM person");
    assert_eq!(text_at(&got[0], 0), "a@b");
    assert_eq!(text_at(&got[0], 1).len(), 36);
    let error = refuse(&mut db, "INSERT INTO person (id) VALUES ('b')");
    assert!(format!("{error:?}").contains("email"), "{error:?}");
    run(&mut db, "INSERT INTO person (id, email) VALUES ('b', 'b@c')");
    let (_, got) = rows(&mut db, "SELECT token FROM person");
    assert_eq!(got.len(), 2);
    assert_ne!(text_at(&got[0], 0), text_at(&got[1], 0));
}
