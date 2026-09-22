//! END-TO-END: the real `sekejap-pg` binary on a free localhost port, driven
//! by a real PostgreSQL client.
//!
//! `dist/tests/pg_wire.rs` proves the bytes against frames this repository
//! wrote, which can only ever prove that this repository agrees with itself.
//! This file proves the same surface against `postgres` 0.19 -- a client
//! nothing here wrote, which speaks the EXTENDED protocol with binary
//! parameters and binary results, reads `ParameterDescription` before it will
//! encode a value, and cancels out of band on a second connection. That is
//! what says a stock client can connect.
//!
//! Numbers this file measures rather than assumes, printed under
//! `--nocapture`: how long one full scan of the corpus takes, which is what
//! makes the cancel and the timeout tests meaningful rather than racy.
//!
//! ## What the catalog half asserts
//!
//! The single-relation `pg_catalog` / `information_schema` views answer rows
//! through `sekejap_lang`'s virtual catalog (docs/dist/PG_SURFACE.md); the JOIN
//! a schema tree issues is a named Tier-2 refusal (QL_CONTRACT §4.8).

use kernel::io::IoMode;
use kernel::store::{Config, SyncMode};
// `FallibleIterator` is what `Notifications::timeout_iter` yields through;
// `postgres` re-exports the trait so a caller need not name the crate.
use postgres::fallible_iterator::FallibleIterator;
use postgres::types::Type;
use postgres::{Client, NoTls, SimpleQueryMessage};
use sekejap_core::collections::Database;
use sekejap_lang::SqlDatabase;
use serde_json::json;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Rows the corpus holds. Large enough that ONE full scan projecting a long
/// text column is hundreds of milliseconds in release, which is the margin
/// the cancel test needs to be a test rather than a race.
const ROWS: usize = 120_000;
/// The margin the cancel test needs: the uncancelled scan must take at least
/// this much, or the corpus is too small for the test to mean anything and
/// the test says so instead of passing by luck. Measured on this host at
/// 152 ms for 120,000 rows (printed by the test), so the floor is a
/// two-thirds fraction of that and the cancel fires at a sixth of it.
const SCAN_FLOOR: Duration = Duration::from_millis(100);
/// How long the cancelling client waits before it fires.
const CANCEL_AFTER: Duration = Duration::from_millis(25);
/// Rows the fixture writes between durability barriers. The page WAL bounds
/// the bytes ONE uncommitted batch may hold, and the whole corpus passes it.
const COMMIT_BATCH: usize = 8_192;

fn config() -> Config {
    Config {
        budget_bytes: 32 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

/// The corpus, as this file knows it. `n` is the unique ordinal; `born`
/// repeats every 120 rows; `alive` alternates.
fn born(at: usize) -> i64 {
    1_900 + (at as i64 % 120)
}

fn alive(at: usize) -> bool {
    at % 2 == 0
}

/// A running `sekejap-pg`, killed when it drops.
struct Server {
    child: Child,
    port: u16,
    _dir: TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn url(&self) -> String {
        format!(
            "host=127.0.0.1 port={} user=sekejap dbname=sekejap connect_timeout=10",
            self.port
        )
    }

    fn client(&self) -> Client {
        Client::connect(&self.url(), NoTls).expect("a stock PostgreSQL client connects")
    }
}

/// Build the corpus with the embedded handle -- building it through the wire
/// would prove nothing this file's own tests do not and would put the
/// fixture's cost inside the measurement -- then serve the directory.
fn start(rows: usize) -> Server {
    let dir = TempDir::new().expect("a temp dir");
    let path = dir.path().join("db");
    {
        let mut db = Database::create(&path, config()).expect("create");
        db.sql(
            "CREATE TABLE place (id TEXT PRIMARY KEY, name TEXT, n INT, born INT, alive BOOL, \
             profile JSONB)",
            &[],
        )
        .expect("create table");
        // QL_CONTRACT §6: a Tier-1 predicate is answered INDEX-side, so a
        // filter or an order over a column with no index is REFUSED rather
        // than turned into a scan.
        db.sql("CREATE INDEX place_n ON place USING btree (n)", &[])
            .expect("index n");
        db.sql("CREATE INDEX place_born ON place USING btree (born)", &[])
            .expect("index born");
        let collection = db
            .collection("place")
            .expect("catalog")
            .expect("the collection just created");
        for at in 0..rows {
            db.put(
                collection,
                &format!("k{at:06}"),
                &json!({
                    "id": format!("k{at:06}"),
                    "name": format!("place {at:06} with a description long enough that projecting it is not free"),
                    "n": at as i64,
                    "born": born(at),
                    "alive": alive(at),
                    "profile": {"rank": at as i64 % 7},
                }),
            )
            .expect("put");
            // One barrier per BATCH, not per row and not one for the whole
            // corpus: the page WAL bounds the bytes one uncommitted batch
            // may hold, and the whole of 120,000 rows passes it.
            if (at + 1) % COMMIT_BATCH == 0 {
                db.commit().expect("commit");
            }
        }
        db.commit().expect("commit");
    }

    // `--port 0` asks the operating system for a free port, and the binary
    // prints the one it got. Asking it beats picking one and hoping.
    let mut child = Command::new(env!("CARGO_BIN_EXE_sekejap-pg"))
        .arg(&path)
        .arg("--port")
        .arg("0")
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("sekejap-pg starts");
    let stderr = child.stderr.take().expect("piped stderr");
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    reader.read_line(&mut line).expect("the banner line");
    let port = line
        .split("postgres://127.0.0.1:")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|port| port.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("could not read the bound port from `{line}`"));
    // Drain the rest of stderr so the child never blocks on a full pipe.
    // Anything the server prints there is a fault it wants reported, so it
    // is echoed rather than swallowed.
    std::thread::spawn(move || {
        let mut sink = String::new();
        while reader.read_line(&mut sink).unwrap_or(0) > 0 {
            eprint!("[sekejap-pg] {sink}");
            sink.clear();
        }
    });

    Server {
        child,
        port,
        _dir: dir,
    }
}

// ── the shape a client actually uses ─────────────────────────────────────

/// Connect, create a table, insert with parameters, read typed rows back.
///
/// `rust-postgres` reads `ParameterDescription` before it will encode a
/// value and asks for BINARY results on every column, so this is also the
/// test that the declared `$n` OIDs and the binary encodings are right.
#[test]
fn a_stock_client_connects_creates_a_table_inserts_with_parameters_and_reads_typed_rows() {
    let server = start(64);
    let mut client = server.client();

    client
        .batch_execute("CREATE TABLE note (id TEXT PRIMARY KEY, body TEXT, weight INT, ok BOOL)")
        .expect("CREATE TABLE over the simple protocol");
    client
        .batch_execute("CREATE INDEX note_weight ON note USING btree (weight)")
        .expect("CREATE INDEX");

    // The `$n` types are DECLARED by the client, which is the door
    // `docs/dist/WIRE_CONTRACT.md` names: an undeclared `$n` is answered
    // `text`, and a client that wants an INT parameter says so.
    let insert = client
        .prepare_typed(
            "INSERT INTO note (id, body, weight, ok) VALUES ($1, $2, $3, $4)",
            &[Type::TEXT, Type::TEXT, Type::INT8, Type::BOOL],
        )
        .expect("prepare_typed");
    for at in 0i64..5 {
        let rows = client
            .execute(
                &insert,
                &[
                    &format!("n{at}"),
                    &format!("body {at}"),
                    &(at * 10),
                    &(at % 2 == 0),
                ],
            )
            .expect("INSERT with bound parameters");
        assert_eq!(rows, 1, "the tag says INSERT 0 1");
    }

    let rows = client
        .query(
            "SELECT id, body, weight, ok FROM note WHERE weight < 25 ORDER BY weight",
            &[],
        )
        .expect("SELECT typed rows");
    assert_eq!(rows.len(), 3, "weights 0, 10 and 20");
    let got: Vec<(String, String, i64, bool)> = rows
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3)))
        .collect();
    assert_eq!(
        got,
        vec![
            ("n0".to_owned(), "body 0".to_owned(), 0i64, true),
            ("n1".to_owned(), "body 1".to_owned(), 10i64, false),
            ("n2".to_owned(), "body 2".to_owned(), 20i64, true),
        ],
        "every column decodes as the type the RowDescription declared"
    );

    // By name, which is what says the column NAMES reached the client.
    assert_eq!(rows[0].get::<_, String>("id"), "n0");
    assert_eq!(rows[0].get::<_, i64>("weight"), 0i64);

    let updated = client
        .execute("UPDATE note SET ok = false WHERE _key = 'n0'", &[])
        .expect("UPDATE");
    assert_eq!(updated, 1, "the tag says UPDATE 1");
    let deleted = client
        .execute("DELETE FROM note WHERE _key = 'n4'", &[])
        .expect("DELETE");
    assert_eq!(deleted, 1, "the tag says DELETE 1");
}

/// One prepared statement, executed again under new parameters. The second
/// execution parses nothing: it is a `Bind` over the compiled form
/// (`QL_CONTRACT` §2, the reusable `PreparedSql`).
#[test]
fn a_prepared_statement_is_reused_across_executions_and_answers_for_each_parameter() {
    let server = start(2_000);
    let mut client = server.client();

    let statement = client
        .prepare_typed("SELECT id, born FROM place WHERE born = $1 ORDER BY n", &[Type::INT8])
        .expect("prepare_typed");

    for value in [1_900i64, 1_955, 2_019] {
        let rows = client.query(&statement, &[&value]).expect("query");
        // The oracle: this file wrote `born(at)` for every `at`, so the rows
        // for a value are exactly the ordinals whose `born` is that value.
        let expected: Vec<String> = (0..2_000usize)
            .filter(|at| born(*at) == value)
            .map(|at| format!("k{at:06}"))
            .collect();
        assert_eq!(
            rows.iter().map(|row| row.get::<_, String>(0)).collect::<Vec<_>>(),
            expected,
            "the reused statement answers for ITS parameter, born = {value}"
        );
        for row in &rows {
            assert_eq!(row.get::<_, i64>(1), value);
        }
    }
}

/// A Tier-2 or Tier-3 construct reaches the client as
/// `0A000 feature_not_supported` with the contract's own named reason --
/// never emulated, and never a different code.
#[test]
fn a_refused_construct_reaches_the_client_as_0a000_with_the_contracts_reason() {
    let server = start(16);
    let mut client = server.client();

    let error = client
        .query("SELECT id FROM place UNION SELECT id FROM place", &[])
        .expect_err("UNION is Tier 3");
    let db_error = error.as_db_error().expect("a server ErrorResponse");
    assert_eq!(db_error.code().code(), "0A000");
    assert!(
        db_error.message().contains("UNION") && db_error.message().contains("no atomic"),
        "the refusal names the construct and carries the reason: {}",
        db_error.message()
    );

    let error = client
        .query("SELECT id FROM nowhere", &[])
        .expect_err("no such collection");
    assert_eq!(
        error.as_db_error().expect("a server error").code().code(),
        "42P01"
    );
}

/// §9.1 and §3: a statement that outruns `statement_timeout` reaches the
/// client as `57014 query_canceled`, which is what every pooler and every
/// driver already expects.
#[test]
fn a_statement_timeout_reaches_the_client_as_57014() {
    let server = start(ROWS);
    let mut client = server.client();

    client
        .batch_execute("SET statement_timeout = '1ms'")
        .expect("SET");
    let error = client
        .query("SELECT id, name, born FROM place", &[])
        .expect_err("a full scan cannot finish in one millisecond");
    let db_error = error.as_db_error().expect("a server ErrorResponse");
    assert_eq!(db_error.code().code(), "57014");
    assert!(
        db_error.message().contains("statement timeout")
            && db_error.message().contains("microseconds"),
        "the message carries the elapsed micros, which is what tells a timeout \
         from a cancel: {}",
        db_error.message()
    );

    client
        .batch_execute("SET statement_timeout = 0")
        .expect("SET");
    let rows = client
        .query("SELECT id FROM place WHERE n < 4 ORDER BY n", &[])
        .expect("cleared, the connection answers");
    assert_eq!(rows.len(), 4);
}

/// §9.2: a cancel arrives on a SECOND connection carrying the `(pid,
/// secret)` pair this backend published, and stops the statement in flight
/// with `57014`.
#[test]
fn a_cancel_from_a_second_client_stops_the_statement_in_flight_with_57014() {
    let server = start(ROWS);
    let mut client = server.client();

    // Measure first. A cancel test whose statement finishes before the
    // cancel lands passes by luck, so the margin is measured and asserted
    // rather than assumed.
    let started = Instant::now();
    let rows = client
        .query("SELECT id, name FROM place", &[])
        .expect("one full scan");
    let scan = started.elapsed();
    assert_eq!(rows.len(), ROWS, "the scan returned the whole corpus");
    println!("one full scan of {ROWS} rows: {scan:?} (cancel fires at {CANCEL_AFTER:?})");
    assert!(
        scan > SCAN_FLOOR,
        "the corpus is too small for this test to mean anything: one scan took {scan:?}, \
         which is under the {SCAN_FLOOR:?} floor the cancel needs"
    );

    let token = client.cancel_token();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(CANCEL_AFTER);
        token
            .cancel_query(NoTls)
            .expect("the cancel connection is accepted")
    });

    let error = client
        .query("SELECT id, name FROM place", &[])
        .expect_err("the cancel reached the statement in flight");
    canceller.join().expect("the cancelling thread");
    let db_error = error.as_db_error().expect("a server ErrorResponse");
    assert_eq!(db_error.code().code(), "57014");
    assert!(
        db_error.message().contains("CancelRequest"),
        "a cancel and a timeout share the code and are told apart by the message: {}",
        db_error.message()
    );
}

/// §9.3: `LISTEN` subscribes the session to the change feed, and a COMMIT on
/// ANOTHER connection becomes one `NotificationResponse`. Nothing arrives
/// before the commit.
#[test]
fn listen_delivers_a_notification_after_a_commit_made_on_another_connection() {
    let server = start(16);
    let mut listener = server.client();
    let mut writer = server.client();

    listener.batch_execute("LISTEN changes").expect("LISTEN");
    let quiet = listener
        .notifications()
        .timeout_iter(Duration::from_millis(150))
        .next()
        .expect("the iterator");
    assert!(quiet.is_none(), "no commit has happened since the LISTEN");

    writer
        .batch_execute(
            "INSERT INTO place (id, name, n, born, alive) \
             VALUES ('w1', 'written elsewhere', 900001, 2222, true)",
        )
        .expect("INSERT on the other connection");

    let mut notifications = listener.notifications();
    let mut iter = notifications.timeout_iter(Duration::from_secs(5));
    let notification = iter
        .next()
        .expect("the iterator")
        .expect("one notification for the committed batch");
    assert_eq!(notification.channel(), "changes");
    assert!(
        notification.payload().contains("sequence=")
            && notification.payload().contains("keys="),
        "the payload names the batch and the key count (§9.3): {}",
        notification.payload()
    );
    assert!(
        notification.payload().len() <= 8_000,
        "the protocol caps a payload at 8000 bytes"
    );
}

/// The DBeaver / pgjdbc connect sequence, statement by statement.
///
/// The session half and the single-relation catalog half both answer; the
/// JOIN form a schema tree issues is a named Tier-2 refusal.
#[test]
fn the_dbeaver_connect_sequence_is_answered_and_its_catalog_half_answers_rows() {
    let server = start(16);
    let mut client = server.client();

    // What the driver sends before it will do anything.
    for statement in [
        "SET client_encoding = 'UTF8'",
        "SET application_name = 'DBeaver 24'",
        "SET extra_float_digits = 3",
        "SET TIME ZONE 'UTC'",
    ] {
        client
            .batch_execute(statement)
            .unwrap_or_else(|e| panic!("`{statement}` was refused: {e}"));
    }

    let rows = client.simple_query("SELECT version()").expect("version()");
    let version = rows
        .iter()
        .find_map(|message| match message {
            SimpleQueryMessage::Row(row) => Some(row.get(0).expect("a version").to_owned()),
            _ => None,
        })
        .expect("one row");
    assert!(version.starts_with("PostgreSQL 16.0 (sekejap "), "{version}");

    let rows = client
        .simple_query("SELECT current_schema()")
        .expect("current_schema()");
    assert!(rows.iter().any(|message| matches!(
        message,
        SimpleQueryMessage::Row(row) if row.get(0) == Some("public")
    )));

    let rows = client
        .simple_query("SHOW client_encoding")
        .expect("SHOW client_encoding");
    assert!(rows.iter().any(|message| matches!(
        message,
        SimpleQueryMessage::Row(row) if row.get(0) == Some("UTF8")
    )));

    // The catalog half. A single-relation view answers rows through
    // sekejap_lang's virtual catalog (docs/dist/PG_SURFACE.md); the JOIN a
    // schema tree issues is Tier 2 by name (QL_CONTRACT §4.8) and comes back
    // as a named refusal, never as a shimmed answer.
    let rows = client
        .simple_query("SELECT table_name FROM information_schema.tables")
        .expect("information_schema.tables answers");
    assert!(
        rows.iter().any(|message| matches!(message, SimpleQueryMessage::Row(_))),
        "information_schema.tables answered no row"
    );
    let rows = client
        .simple_query("SELECT relname FROM pg_catalog.pg_class")
        .expect("pg_catalog.pg_class answers");
    assert!(rows.iter().any(|message| matches!(message, SimpleQueryMessage::Row(_))));
    let joined = "SELECT n.nspname, c.relname FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace";
    let error = client
        .simple_query(joined)
        .expect_err("a JOIN over the catalog views is Tier 2 by name");
    let db_error = error.as_db_error().expect("a server ErrorResponse");
    assert_eq!(db_error.code().code(), "0A000", "`{joined}`");
    assert!(
        db_error.message().to_ascii_uppercase().contains("JOIN"),
        "the refusal names the construct: {}",
        db_error.message()
    );

}

/// Transactions over the wire, against the service's SINGLE writer: a
/// rollback leaves nothing and a commit leaves the row, as seen from a
/// SECOND connection.
#[test]
fn a_wire_transaction_rolls_back_or_commits_and_a_second_connection_sees_the_result() {
    let server = start(16);
    let mut writer = server.client();
    let mut reader = server.client();

    writer.batch_execute("BEGIN").expect("BEGIN");
    writer
        .batch_execute(
            "INSERT INTO place (id, name, n, born, alive) \
             VALUES ('r1', 'rolled back', 900011, 3131, true)",
        )
        .expect("INSERT inside the block");
    writer.batch_execute("ROLLBACK").expect("ROLLBACK");
    let rows = reader
        .query("SELECT id FROM place WHERE born = 3131", &[])
        .expect("read from the other connection");
    assert!(rows.is_empty(), "a rollback wrote nothing");

    writer.batch_execute("BEGIN").expect("BEGIN");
    writer
        .batch_execute(
            "INSERT INTO place (id, name, n, born, alive) \
             VALUES ('r2', 'committed', 900012, 3132, true)",
        )
        .expect("INSERT inside the block");
    writer.batch_execute("COMMIT").expect("COMMIT");
    let rows = reader
        .query("SELECT id FROM place WHERE born = 3132", &[])
        .expect("read from the other connection");
    assert_eq!(
        rows.len(),
        1,
        "a commit publishes, and the other connection's next statement sees it"
    );
    assert_eq!(rows[0].get::<_, String>(0), "r2");
}

/// `psql` itself, if it is installed. Skipped WITH A NAMED REASON when it is
/// not, rather than passing silently.
#[test]
fn psql_connects_and_runs_a_statement_when_psql_is_installed() {
    let Ok(found) = Command::new("which").arg("psql").output() else {
        println!("SKIPPED (named reason): `which` is not available on this host");
        return;
    };
    if !found.status.success() {
        println!(
            "SKIPPED (named reason): `psql` is not installed on this host, so the client this \
             surface exists for cannot be run here"
        );
        return;
    }
    let psql = String::from_utf8_lossy(&found.stdout).trim().to_owned();

    let server = start(16);
    let output = Command::new(&psql)
        .args([
            "-h",
            "127.0.0.1",
            "-p",
            &server.port.to_string(),
            "-U",
            "sekejap",
            "-d",
            "sekejap",
            "--no-psqlrc",
            "-t",
            "-A",
            "-c",
            "SELECT id, n FROM place WHERE n < 3 ORDER BY n",
        ])
        .output()
        .expect("psql runs");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "psql exited {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
    assert_eq!(
        stdout.trim(),
        "k000000|0\nk000001|1\nk000002|2",
        "psql printed the rows this file wrote"
    );
    println!("psql ({psql}) connected and printed:\n{}", stdout.trim());
}
