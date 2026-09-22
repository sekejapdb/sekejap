//! `sekejap-pg` -- the PostgreSQL wire-protocol listener.
//!
//! Arg parsing, one [`ServiceDatabase`] for the process, and
//! `sekejap_dist::pg::serve`. Everything the protocol means lives in
//! `dist/src/pg/`; this file owns the process.
//!
//! ```text
//! sekejap-pg <db-path> [--host 127.0.0.1] [--port 5432] [--allow-remote]
//!                      [--publish-interval <ms>] [--create]
//! ```
//!
//! Trust auth, no TLS (`docs/dist/OPS_CONTRACT.md` §9.4), loopback unless
//! `--allow-remote` says otherwise.
//!
//! **Shutdown.** `sekejap_dist::pg::serve` stops on a [`pg::Shutdown`] and
//! JOINS every connection thread before it returns, and this file then
//! closes the service -- which is the graceful path, and the one an
//! embedding caller drives. This BINARY installs no signal handler, because
//! the crate carries no signal dependency: a `SIGTERM` here is a process
//! exit, an open transaction rolls back exactly as a close rolls it back,
//! and the page WAL's recovery is what makes the difference invisible.
//! `docs/dist/WIRE_CONTRACT.md` §9 says the same in one row.

use std::path::PathBuf;
use std::time::Duration;

use kernel::io::IoMode;
use kernel::store::{Config, SyncMode};
use sekejap_core::collections::Database;
use sekejap_dist::pg;
use sekejap_dist::service::ServiceDatabase;

/// The page cache one server holds: 64 MiB. A stated number, not a probe.
const CACHE_BYTES: usize = 64 << 20;

fn main() -> std::process::ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("sekejap-pg: {message}");
            std::process::ExitCode::from(1)
        }
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let mut path: Option<PathBuf> = None;
    let mut host = "127.0.0.1".to_owned();
    let mut port: u16 = 5432;
    let mut allow_remote = false;
    let mut create = false;
    // §2: the publish interval. ZERO is the default HERE and nowhere else,
    // because a wire client expects to read its own writes on the next
    // statement, and that costs one snapshot mint per commit.
    let mut publish_interval = Duration::ZERO;

    let mut it = args.into_iter();
    while let Some(argument) = it.next() {
        match argument.as_str() {
            "--host" => host = it.next().ok_or("--host needs an address")?,
            "--port" => {
                port = it
                    .next()
                    .ok_or("--port needs a number")?
                    .parse()
                    .map_err(|_| "--port is not a port number")?;
            }
            "--allow-remote" => allow_remote = true,
            "--create" => create = true,
            "--publish-interval" => {
                let millis: u64 = it
                    .next()
                    .ok_or("--publish-interval needs milliseconds")?
                    .parse()
                    .map_err(|_| "--publish-interval is not a number of milliseconds")?;
                publish_interval = Duration::from_millis(millis);
            }
            "--help" | "-h" => {
                print_usage();
                return Ok(());
            }
            other if !other.starts_with("--") && path.is_none() => {
                path = Some(PathBuf::from(other));
            }
            other => return Err(format!("unexpected argument: {other}")),
        }
    }

    let path = path.ok_or("a database directory is required: sekejap-pg <db-path>")?;
    let config = Config {
        budget_bytes: CACHE_BYTES,
        io: IoMode::Buffered,
        // The default barrier, as PostgreSQL's own plain `fsync` is: a
        // client that wants the drive-cache barrier asks for it.
        sync: SyncMode::Normal,
    };
    if create && !path.exists() {
        Database::create(&path, config)
            .map_err(|e| format!("could not create {}: {e}", path.display()))?;
    }
    let service = ServiceDatabase::open(&path, config)
        .map_err(|e| format!("could not open {}: {e}", path.display()))?;
    service.set_publish_interval(publish_interval);

    let address = format!("{host}:{port}");
    let listener = pg::bind(&address, allow_remote).map_err(|e| e.to_string())?;
    let bound = listener.local_addr().map_err(|e| e.to_string())?;
    eprintln!(
        "sekejap-pg serving {} on postgres://{bound} (trust auth, no TLS -- OPS_CONTRACT §9.4)\n\
         connect: psql -h {host} -p {} -U sekejap -d sekejap",
        path.display(),
        bound.port()
    );

    let options = pg::ServerOptions::default();
    let result = pg::serve(listener, &service, &options).map_err(|e| e.to_string());
    // A close is not a commit: anything uncommitted is discarded.
    service.close().map_err(|e| e.to_string())?;
    result
}

fn print_usage() {
    println!(
        "sekejap-pg <db-path> [flags]\n\
         \n\
         Speak the PostgreSQL wire protocol so any Postgres client can connect.\n\
         \n\
         Flags:\n\
         \x20 --host <addr>              bind address (default 127.0.0.1)\n\
         \x20 --port <n>                 port (default 5432)\n\
         \x20 --allow-remote             permit a non-loopback bind; there is no TLS\n\
         \x20                            and no auth beyond trust (OPS_CONTRACT §9.4)\n\
         \x20 --create                   create the directory if it is not there\n\
         \x20 --publish-interval <ms>    OPS_CONTRACT §2's window (default 0, so a\n\
         \x20                            session reads its own writes)\n\
         \n\
         Supports the simple and extended query protocols with `$n` parameters,\n\
         portals with row limits, DECLARE/FETCH/CLOSE cursors, BEGIN/COMMIT/ROLLBACK\n\
         over the single writer, statement_timeout, CancelRequest, and LISTEN over\n\
         the change feed. Contract: docs/dist/WIRE_CONTRACT.md."
    );
}
