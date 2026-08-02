//! `sekejap pg` — PostgreSQL wire-protocol listener.
//!
//! **Thin I/O adapter.** This file owns only the transport: arg parsing, the
//! `TcpListener`, one thread per connection, and a tiny read→feed→write loop. All
//! protocol logic lives in the sans-IO [`sekejap::pg::Connection`], so any other
//! language can host the same wire surface by feeding it socket bytes.
//!
//! Trust auth, localhost by default. Deliberately dependency-free: blocking
//! `std::net`, `Arc<RwLock<CoreDB>>` (shared reads, exclusive writes), no async
//! runtime, no new crates. A dev-facing access port, not a high-concurrency server.

use sekejap::pg::Connection;
use sekejap::CoreDB;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, RwLock};
use std::thread;

/// Entry point for `sekejap pg <db-path> [flags]`. Returns a process exit code.
pub fn run(args: &[String]) -> i32 {
    let mut path: Option<String> = None;
    let mut host = "127.0.0.1".to_string();
    let mut port: u16 = 5432;
    let mut read_only = false;

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--host" => if let Some(v) = it.next() { host = v.clone(); },
            "--port" => if let Some(v) = it.next() {
                match v.parse() { Ok(p) => port = p, Err(_) => { eprintln!("invalid --port {v}"); return 2; } }
            },
            "--read-only" => read_only = true,
            "--help" | "-h" => { print_pg_usage(); return 0; }
            other => {
                if !other.starts_with("--") && path.is_none() {
                    path = Some(other.to_string());
                } else {
                    eprintln!("unexpected argument: {other}");
                    return 2;
                }
            }
        }
    }

    // No auth yet (trust) — refuse to expose the port beyond localhost.
    let is_local = matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1");
    if !is_local {
        eprintln!("refusing to bind {host}: `sekejap pg` has no authentication yet (localhost only).");
        return 2;
    }

    let db = match &path {
        Some(p) => match CoreDB::open(p) {
            Ok(d) => d,
            Err(e) => { eprintln!("failed to open {p}: {e}"); return 1; }
        },
        None => CoreDB::new(),
    };
    let label = path.clone().unwrap_or_else(|| ":memory:".to_string());

    let addr = format!("{host}:{port}");
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => { eprintln!("failed to bind {addr}: {e}"); return 1; }
    };
    eprintln!(
        "sekejap serving {label} on postgres://{addr}  (auth: off, {})\n\
         connect: psql -h {host} -p {port} -U postgres",
        if read_only { "read-only" } else { "read-write" },
    );

    let shared = Arc::new(RwLock::new(db));
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let db = Arc::clone(&shared);
                thread::spawn(move || {
                    if let Err(e) = handle_client(s, db, read_only) {
                        if e.kind() != io::ErrorKind::UnexpectedEof {
                            eprintln!("pg connection error: {e}");
                        }
                    }
                });
            }
            Err(e) => eprintln!("pg accept error: {e}"),
        }
    }
    0
}

/// The entire per-connection I/O loop: read a chunk, feed it to the sans-IO
/// protocol engine, write back whatever it produced. The `Connection` handles
/// all framing, buffering partial messages across reads.
fn handle_client(mut s: TcpStream, db: Arc<RwLock<CoreDB>>, read_only: bool) -> io::Result<()> {
    s.set_nodelay(true).ok();
    let mut conn = Connection::new(db, read_only);
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = s.read(&mut buf)?;
        if n == 0 {
            return Ok(()); // client closed
        }
        let reply = conn.feed(&buf[..n]);
        if !reply.is_empty() {
            s.write_all(&reply)?;
        }
        if conn.is_closed() {
            return Ok(());
        }
    }
}

fn print_pg_usage() {
    println!(
        "sekejap pg <db-path> [flags]\n\
         \n\
         Speak the PostgreSQL wire protocol so any Postgres client can connect.\n\
         \n\
         Flags:\n\
         \x20 --host <addr>   bind address (default 127.0.0.1; localhost only for now)\n\
         \x20 --port <n>      port (default 5432)\n\
         \x20 --read-only     reject INSERT/UPDATE/DELETE/DDL\n\
         \n\
         Connect:\n\
         \x20 psql -h 127.0.0.1 -p 5432 -U postgres\n\
         \n\
         Supports the Simple and Extended query protocols (incl. $1 params). Any\n\
         username is accepted (trust auth); the database name is ignored."
    );
}
