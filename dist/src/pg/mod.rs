//! The PostgreSQL wire protocol, so `psql`, DBeaver, pgjdbc, psycopg and
//! QGIS connect to a sekejap service with no sekejap-specific code.
//!
//! Contract: `docs/dist/WIRE_CONTRACT.md` (the messages, the GUCs, the
//! SQLSTATE map, what is refused), over `docs/dist/OPS_CONTRACT.md` §9 and
//! `docs/lang/QL_CONTRACT.md` §2's cursors.
//!
//! ## The shape
//!
//! ```text
//!   bytes in ──▶ pg::Connection::feed ──▶ bytes out    (sans-IO, no socket)
//!                        │
//!                        ├─ read  ──▶ sekejap_lang::prepare_sql_with
//!                        │              over this connection's Snapshot
//!                        └─ write ──▶ ServiceDatabase::writer (the single
//!                                      writer), committed by COMMIT or by
//!                                      the statement itself
//! ```
//!
//! [`connection::Connection`] is the whole protocol brain and touches no
//! socket: it is handed the bytes that arrived and hands back the bytes to
//! send. [`server`] is the `std::net` adapter that moves those two buffers,
//! and `dist/src/cli/pg_server.rs` is the `sekejap-pg` binary over it. The
//! split is e1's (`e1:src/pg.rs` plus `e1:skcli/src/pg.rs`) and it is kept
//! for e1's reason: the protocol logic is then testable a BYTE at a time,
//! which is what `dist/tests/pg_wire.rs` does.
//!
//! ## What this layer does NOT do
//!
//! It adds no execution. Every statement compiles through
//! `sekejap_lang::prepare_sql_with` or `sekejap_lang::SqlDatabase::sql` and
//! runs on the engine's own atomics, and a construct with no atomic is
//! REFUSED by name with the contract's reason -- as `0A000
//! feature_not_supported`, which is what a PostgreSQL client calls the same
//! thing. There is no second planner, no emulation of a refused form, and no
//! statement answered from data this layer invented, with one stated
//! exception: `SELECT version()` and the `current_*` functions, which a
//! client sends before it will talk at all and which are constants rather
//! than queries.
//!
//! It has no TLS and no authentication beyond trust (§9.4), so
//! [`server::bind`] refuses a non-loopback address unless the caller says
//! otherwise in as many words.
//!
//! The `pg_catalog` and `information_schema` views are
//! `docs/dist/PG_SURFACE.md`'s, answered by `sekejap_lang` as virtual rows;
//! a catalog relation it does not provide is refused by name there, never
//! answered from a shim here.

pub mod connection;
pub mod frames;
pub mod server;
pub mod types;

pub use connection::{
    Answer, BackendKey, CancelToken, Connection, CURSOR_BYTES_CAP, CURSOR_ROW_CAP,
    NOTIFY_REFUSAL, SERVER_VERSION,
};
pub use frames::{FieldDescription, TransactionStatus};
pub use server::{bind, serve, Backends, ServerOptions, Shutdown};
pub use types::{oid, WireError};
