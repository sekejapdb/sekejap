# Wire contract — the PostgreSQL protocol over a sekejap service

What `dist/src/pg/` speaks, so `psql`, DBeaver, pgjdbc, psycopg and QGIS
connect to a sekejap service with no sekejap-specific code.

It is a SURFACE, not a second engine. Every statement compiles through
`sekejap_lang::prepare_sql_with` or `sekejap_lang::SqlDatabase::sql` and runs
on the engine's own atomics over `dist/src/service/` (`OPS_CONTRACT.md` §1),
and a construct with no atomic is REFUSED by name with the contract's
reason — spelled `0A000 feature_not_supported`, which is what a PostgreSQL
client calls the same thing. Nothing here is emulated.

Companion documents: `docs/dist/OPS_CONTRACT.md` §9 (the four capabilities
the protocol has a first-class spelling for), `docs/lang/QL_CONTRACT.md` §2
(the statements, and the cursors), `docs/dist/PG_SURFACE.md` (the catalog
views — worker PGCAT's, not this document's).

---

## 0. The shape, in one place

```text
  bytes in ──▶ pg::Connection::feed ──▶ bytes out      (sans-IO, no socket)
                       │
                       ├─ read  ──▶ sekejap_lang::prepare_sql_with
                       │              over THIS connection's Snapshot
                       └─ write ──▶ ServiceDatabase::writer, the single
                                     writer, committed by COMMIT or by the
                                     statement itself
```

| Piece | File | What it is |
|---|---|---|
| The protocol engine | `dist/src/pg/connection.rs` | `Connection::feed(&[u8]) -> Vec<u8>`. No socket, no thread, no runtime. |
| The bytes | `dist/src/pg/frames.rs` | Every message this surface reads or writes, and nothing else. |
| The types | `dist/src/pg/types.rs` | The `pg_type` OID table, the text and binary encodings of one cell, `$n` decoding, the SQLSTATE map. |
| The transport | `dist/src/pg/server.rs` | `std::net` listener, one thread per connection, `std::thread::scope`. |
| The binary | `dist/src/cli/pg_server.rs` | `sekejap-pg <db-path> [--host] [--port] [--allow-remote] [--create] [--publish-interval <ms>]`. |

The split is the prior engine's (`e1:src/pg.rs` plus `e1:skcli/src/pg.rs`) and it is kept
for its reason: a sans-IO engine is testable a BYTE at a time, which is what
`dist/tests/pg_wire.rs` does, and it can be hosted over any transport by a
caller in any language.

---

## 1. Messages

### 1.1 Startup

| From the client | Answer |
|---|---|
| `SSLRequest` (80877103) | `N`. There is no TLS (§9.4 below, §5 here). The session continues in plaintext. |
| `GSSENCRequest` (80877104) | `N`, the same. |
| `CancelRequest` (80877102) | Nothing. See §4. |
| Protocol 3.0 (196608) | `AuthenticationOk` (trust), the `ParameterStatus` list below, `BackendKeyData`, `ReadyForQuery`. |
| anything else | `ErrorResponse 08P01` and the connection closes. |

The startup parameters are accepted and, apart from one, ignored: `user` and
`database` name nothing this service distinguishes (one directory, one
service), and every key is remembered so `SHOW` prints it back. The exception
is `statement_timeout`, which is the same GUC §2 routes.

`ParameterStatus` sent, in order: `server_version` = `16.0`,
`server_encoding` = `UTF8`, `client_encoding` = `UTF8`, `DateStyle` =
`ISO, MDY`, `IntervalStyle` = `postgres`, `integer_datetimes` = `on`,
`standard_conforming_strings` = `on`, `TimeZone` = `UTC`,
`application_name` = empty.

`BackendKeyData` carries a real pair. The process id is this SERVER's own
counter (not the operating system's), and the secret comes from a SplitMix64
stream seeded from the clock and a heap address — enough that a secret is not
the connection's ordinal, and **stated as not cryptographic**
(`dist/src/pg/server.rs`, `SecretStream`).

### 1.2 Simple query

`Q` runs a `;`-separated statement list, in order, abandoning the rest at the
first error — PostgreSQL's rule. A row-returning statement answers
`RowDescription`, then one `DataRow` per row, then `CommandComplete`; the
rows are STREAMED page by page and nothing is held.

`CommandComplete` tags: `SELECT <n>`, `INSERT 0 <n>`, `UPDATE <n>`,
`DELETE <n>`, `FETCH <n>`, `MOVE <n>`, `CREATE TABLE`, `DROP TABLE`,
`ALTER TABLE`, `CREATE INDEX`, `DROP INDEX`, `BEGIN`, `COMMIT`, `ROLLBACK`,
`SET`, `RESET`, `DISCARD ALL`, `LISTEN`, `UNLISTEN`, `DECLARE CURSOR`,
`CLOSE CURSOR`.

### 1.3 Extended query

`Parse` / `Bind` / `Describe` / `Execute` / `Close` / `Sync` / `Flush` /
`Terminate`, all of them.

* **`Parse`** records the text and the `$n` type OIDs the client declared. A
  syntax error is raised HERE, not three messages later at `Execute`: a
  client that pipelines wants it at the message that caused it.
* **`Bind`** decodes each parameter by the declared OID (§3.2), in text or
  binary as the format codes say, and records the RESULT format codes. Every
  COUNT field is checked against what the frame can hold before it is
  believed — a two-byte `0xFFFF` is `-1`, and casting it to `usize` is a
  `Vec::with_capacity(usize::MAX)` (the prior engine's `src/pg.rs:249-286`, the same hole,
  fixed the same way).
* **`Describe('S')`** answers `ParameterDescription` then `RowDescription` or
  `NoData`. The columns are answered WITHOUT running the statement, from the
  source collection's declared types (§3.1); the statement is compiled with
  one PROBE parameter per `$n`, built from the declared OID, because a plan
  is what names the collection. A probe that does not compile answers
  `NoData` rather than a guess.
* **`Describe('P')`** runs the portal and holds its answer, which is what
  lets its columns be described; the `Execute` that follows does not repeat
  the run.
* **`Execute`** with NO row limit STREAMS (nothing held) and never sends a
  `RowDescription` — the client has one from `Describe`. With a row limit it
  hands out that many rows and answers `PortalSuspended`; the next `Execute`
  on the same portal resumes. See §6.
* **`Flush`** is a no-op: `feed` returns everything it produced, so nothing
  is held back.
* After an error, everything up to the next `Sync` is skipped, as the
  protocol says. A simple `Q` is its own unit of recovery and ends that skip
  at its own `ReadyForQuery`.

`ReadyForQuery` reports `I` idle, `T` inside a `BEGIN` block, `E` inside a
block that has failed. A statement issued in a failed block is refused
`25P02` until `COMMIT` (which rolls back and reports `ROLLBACK`) or
`ROLLBACK`.

### 1.4 Asynchronous

`NoticeResponse` carries every notice `sekejap_lang` attaches to a statement
and every `SET` acknowledgement that has something to say.
`NotificationResponse` is §5.

---

## 2. GUCs

| GUC | What it does |
|---|---|
| `statement_timeout` | **`OPS_CONTRACT` §9.1.** Routed to `ServiceDatabase::set_statement_timeout` (§3). Milliseconds bare, or a number with a unit (`us`, `ms`, `s`, `min`, `h`, `d`); `0` and `DEFAULT` clear it. `SHOW statement_timeout` prints it back in milliseconds. |
| `client_encoding`, `application_name`, `DateStyle`, `extra_float_digits`, `search_path`, `TIME ZONE`, … | Recorded and printed back by `SHOW`. They change nothing: this surface is UTF-8, UTC and ISO, and says so in the banner. |
| `ef_search`, `hnsw.ef_search`, `diskann.query_search_list_size`, `diskann.query_rescore` | Passed THROUGH to `sekejap_lang`, which owns them (`lang/src/compile/mod.rs`). |
| anything else | Accepted and recorded. `SHOW` prints it back. |

**Stated consequence of §9.1.** `OPS_CONTRACT` §3's bound is per SERVICE, as
§4's handle is ("per handle, and therefore per service"), so a session that
sets `statement_timeout` sets it for the server. That is the granularity the
contract has; a per-session bound would need a second knob §3 does not carry,
and inventing one here would be a second engine.

`RESET <name>` and `RESET ALL` clear it. `DISCARD ALL` drops this session's
prepared statements, portals and cursors.

---

## 3. Types

### 3.1 Columns: the OID table, and where it comes from

`docs/dist/PG_SURFACE.md` is where the catalog views and the OIDs they
publish belong, and worker PGCAT builds them. **That document is not on this
build's base commit**, so the table is defined in `dist/src/pg/types.rs`
(`pub mod oid`), and PGCAT's `pg_type` must publish the same numbers.
Everything but the last two is a fixed PostgreSQL system OID and cannot
differ.

| sekejap | OID | `typname` |
|---|---|---|
| `TEXT`, `VARCHAR`, `CHAR`, `UUID`, and every computed column | 25 | `text` |
| `INT`, `INTEGER`, `BIGINT`, `SMALLINT` (`Kind::Int`) | 20 | `int8` |
| `REAL`, `DOUBLE PRECISION` (`Kind::Real`) | 701 | `float8` |
| `BOOL` (`Kind::Bool`) | 16 | `bool` |
| `JSONB` (`Kind::Json`) | 3802 | `jsonb` |
| `TIMESTAMPTZ` | 1184 | `timestamptz` |
| `DATE` | 1082 | `date` |
| `GEOMETRY`, `GEOGRAPHY` (`Kind::Geo`, `Kind::Point`) | **18000** | `geometry` |
| `VECTOR(n)` (`Kind::Vector`) | **18001** | `vector` |

The last two are SYNTHETIC. PostGIS and pgvector both assign their type OIDs
per install rather than reserving one, and a client recognises them by NAME
through the catalog, so any stable number above the system range serves.
18000 is the prior engine's and is kept.

**A column's OID is data-independent, and that is deliberate.** It is read
from the collection's DECLARED spelling first — `TIMESTAMPTZ` and `DATE` are
both `Kind::Int` (`QL_CONTRACT` §5 deviation 8), so only the catalog says
which — then from the stored `Kind`, then `text`. It is never inferred from
the values, because `Describe('S')` is answered before a row is walked and a
client decodes every later row with what it read there; an OID inferred from
values would type the same statement differently at `Describe` and at
`Execute`. So a computed column — an aggregate, a row function, a literal,
`_id` — is `text`, carrying the text `sekejap_lang` prints.

### 3.2 Values out: text is the format, binary is what a driver asks for

Every kind has a TEXT encoding and that is the format this surface describes
itself as speaking:

| kind | text |
|---|---|
| `int8` | decimal |
| `float8` | shortest round-trip, with `NaN`, `Infinity`, `-Infinity` spelled as the server spells them |
| `bool` | `t` / `f` |
| `text` | the bytes |
| `jsonb` | compact JSON |
| `timestamptz` | ISO-8601, as `sekejap_lang` already prints a declared TIMESTAMPTZ (`lang/src/compile/row.rs`) |
| `vector` | pgvector's `[a,b,c]` |
| `geometry` | **GeoJSON text, today.** EWKB is the `p3-geometry-io` follow-up and is named here so it is not discovered later. |
| `_id` | `"<collection>:<sequence>"`, the same spelling `dist/rust/src/rows.rs:100` gives it |
| MISSING and NULL | SQL NULL, which the wire spells as a length of `-1` and not as a zero-length value |

A `Bind` may nevertheless ask for result format `1`, and the two drivers that
matter most do: `rust-postgres` asks for binary on every column of every
extended-protocol query, and pgjdbc asks for it on the types it knows. So a
binary request is HONOURED for the closed set that has a binary encoding here
— `bool`, `int2`, `int4`, `int8`, `float4`, `float8`, `text`, `varchar`,
`json`, `jsonb` (with its one-byte version stamp), `timestamptz`, `date` —
and every other OID is sent as its TEXT bytes, which is exactly right for a
type the client does not know either.

### 3.3 Values in: `$n`

A `$n` is decoded by the OID the `Parse` DECLARED, in the format the `Bind`
named. A position the `Parse` left undeclared is answered `text` (25) in
`ParameterDescription` — a real type whose value maps onto `Param::Text` with
nothing inferred — and a text parameter with no declared OID at all is read
by SHAPE (a whole number, then a number, then text), because `Param`'s type
is read from WHERE it is used and handing `Param::Text("42")` to an `INT`
column refuses where `Param::Int(42)` does not.

**So a client that wants an INT parameter says so.** `rust-postgres` spells
that `prepare_typed(sql, &[Type::INT8])`; pgjdbc spells it `setLong`; psycopg
sends the OID. This is the door, and it is the one the protocol already has.

---

## 4. Cancellation — `OPS_CONTRACT` §9.2

`BackendKeyData` hands out `(pid, secret)`. A `CancelRequest` arrives on a
SECOND connection, carries that pair, is never answered, and closes.
`dist/src/pg/server.rs` routes it to the named backend's `CancelToken` — and
only when BOTH halves match, which is the whole reason the protocol carries a
secret.

Two handles, deliberately:

* The **per-connection** `CancelToken` is what a `CancelRequest` fires. It
  stops the statements of ONE connection and no other. It is cleared at the
  `ReadyForQuery` that follows, which is PostgreSQL's rule: a cancel stops
  the statement IN FLIGHT and nothing after it.
* The **service's** `InterruptHandle` (`OPS_CONTRACT` §4) stops every
  statement the service has in flight, and is sticky until cleared. This
  surface never clears it; that one is the operator's.

Both are ORed into the cancellation closure every page of every walk polls.

---

## 5. `LISTEN` — `OPS_CONTRACT` §9.3

`LISTEN <channel>` subscribes this session to the change feed
(`ServiceDatabase::subscribe_changes`). Each COMMITTED batch becomes one
`NotificationResponse` per listening channel, delivered before the
`ReadyForQuery` that follows, or pushed to an idle session within one read
poll (50 ms). `UNLISTEN <channel>` and `UNLISTEN *` end it, and the
subscription is dropped when the last channel goes.

The payload names what moved and how much:

```text
sequence=<n> collections=<ids> edge_types=<ids> keys=<n> [keys_truncated=true] [rows_affected=<n>]
```

Bounded by construction, and truncated at 8,000 bytes — the protocol's own
cap, arriving at the same answer as §5's L1 bound.

Delivery follows §5 word for word: at the END of the transaction, never when
the write runs, and a rolled-back batch delivers NOTHING.

**`NOTIFY` as a client statement is REFUSED by name** (`0A000`), with the
reason: §9.3 makes the change feed the source of notifications, and there is
no second notification queue for a client-issued `NOTIFY` to write into.
Building one would be an atomic this engine does not have. The refusal names
the way through — `LISTEN` on a channel and commit a write.

---

## 6. Portals, cursors, and what a suspended answer HOLDS

`DECLARE <name> [BINARY] [NO SCROLL] CURSOR [WITH|WITHOUT HOLD] FOR <select>`,
`FETCH [FORWARD|NEXT] [n|ALL] [FROM|IN] <name>`, `MOVE [FORWARD] [n|ALL]
[FROM|IN] <name>`, `CLOSE <name>`, `CLOSE ALL` — `QL_CONTRACT` §2's T2 row,
T1 inside this session.

`Execute` with a row limit and `FETCH` both need a walk to be stopped and
resumed BETWEEN protocol messages. sekejap's `PreparedQuery` borrows the
`Database` handle for the life of the walk and is handed out through a
callback that owns the request (`PreparedSql::with_query`), so it cannot
outlive one call, and there is no public resume cursor for a read.

**So a portal or cursor that is given a row limit pages its answer ONCE —
through `PreparedSql::for_each_row_with`, so every page is charged against
the budget and sees the deadline and the cancel — and HOLDS the rows it has
not yet handed out.** That buffer is bounded and the bounds are stated:

| Ceiling | Value | Why |
|---|---|---|
| `CURSOR_ROW_CAP` | 65,536 rows | Four orders of magnitude more than any interactive `FETCH`, and small enough that the buffer cannot become the process's memory profile. |
| `CURSOR_BYTES_CAP` | 16 MiB | 65,536 rows of one column and 65,536 rows of a hundred are not the same quantity. |

An answer that passes either is **REFUSED naming the ceiling, never
truncated**, and the refusal names the two ways through: run the statement
with no row limit, which STREAMS and holds nothing, or add a `LIMIT`.

`MOVE` is a `FETCH` whose rows are discarded, over the same held answer, so
it costs no walk. `FETCH` past the end returns nothing and is not an error.

---

## 7. Transactions

`BEGIN` / `START TRANSACTION` takes the service's single writer and HOLDS it
(`OPS_CONTRACT` §1); `COMMIT` is `WriterGuard::commit` — the durability
barrier, then §5's event, then §2's republish — and `ROLLBACK` is
`WriterGuard::rollback`. A statement outside a block takes the writer,
runs, commits and releases it.

Three things this states rather than implies:

1. **A close is not a commit.** `Terminate`, a dropped socket, or a dropped
   `Connection` ROLLS BACK an open block.
2. **The writer is taken with `try_writer`, never waited on.** A second
   connection that asks for it while another holds it is refused `55006
   object_in_use` with `SECOND_WRITER_REFUSAL`. `OPS_CONTRACT` §1: a service
   that blocks to honour a bound has broken L6 to satisfy L1.
3. **DDL is not transactional.** `CREATE TABLE`, `ALTER TABLE` and the drop
   steps commit inside their own statement (`lang/src/compile/plan.rs`), so a
   `ROLLBACK` after one does not undo it. PostgreSQL's DDL is transactional;
   sekejap's is not, and there is no savepoint to make it so.

A read takes THIS connection's own snapshot handle
(`ServiceDatabase::open_reader`), re-minted when the service publishes a new
generation — so two connections walk at once instead of taking turns on one
published handle, at the cost of one reader slot each (`OPS_CONTRACT` §1
bounds them). `sekejap-pg` sets the publish interval to **zero** by default,
because a wire client expects to read its own writes on the next statement;
that costs one snapshot mint per commit, and `--publish-interval <ms>` buys
it back.

---

## 8. SQLSTATE map

| sekejap | SQLSTATE | PostgreSQL name |
|---|---|---|
| `SqlError::Refused` (Tier 2 / Tier 3), `SqlError::Unsupported`, `Error::Unsupported` | `0A000` | `feature_not_supported` |
| a boolean leaf with NO membership set (`core/engine/src/query/plan.rs`: a geometry predicate, a traversal, a JSON equality, a text phrase, `IS NULL` / `IS MISSING`) | `0A000` | `feature_not_supported` |
| `SqlError::Syntax` | `42601` | `syntax_error` |
| `no collection named …`, `Error::NotFound` | `42P01` | `undefined_table` |
| `Error::Corrupt`, `kernel::Error::Corrupt*` | `XX001` | `data_corrupted` |
| `QueryError::Cancelled`, `Error::Cancelled` | `57014` | `query_canceled` |
| `WorkResource::Deadline` (`OPS_CONTRACT` §3) | `57014` | `query_canceled` |
| a NAMED `QueryBudget` resource | `54000` | `program_limit_exceeded` |
| `Error::ReadOnly` | `25006` | `read_only_sql_transaction` |
| `SECOND_WRITER_REFUSAL` | `55006` | `object_in_use` |
| `Error::AlreadyExists` | `42P07` | `duplicate_table` |
| `SqlError::Parameter`, `Error::InvalidInput` | `22023` | `invalid_parameter_value` |
| no prepared statement by that name | `26000` | `invalid_sql_statement_name` |
| no portal or cursor by that name | `34000` | `invalid_cursor_name` |
| a frame this surface cannot read | `08P01` | `protocol_violation` |
| a statement in a failed block | `25P02` | `in_failed_sql_transaction` |
| anything else the store refuses | `XX000` | `internal_error` |

The four codes a statement reaches through `sekejap_lang` alone, each shown
as the statement that produces it. The harness runs these against the same
mapping function the wire surface uses (`dist/src/pg/types.rs::wire_error`),
so the table above and these blocks cannot disagree:

```sql refused
-- refused 0A000: JOIN
SELECT p.title FROM posts p JOIN people ON people._key = p.title
```

```sql refused
-- refused 0A000: WITH
WITH busy AS (SELECT _key FROM posts) SELECT _key FROM busy
```

```sql refused
-- refused 42601: a statement this grammar does not spell
SELECT _key FROM posts WHERE
```

```sql refused
-- refused 42P01: an undefined table
SELECT _key FROM nowhere
```

**The one departure from "a refusal is `0A000`", stated.** The single-writer
refusal is `55006`, not `0A000`, because it is TRANSIENT: `55006` is the code
a pooler retries on and `0A000` is the code it gives up on. Every other
service refusal is `0A000` with the reason as written.

**`57014` is shared by the cancel and the timeout on purpose.**
`OPS_CONTRACT` §9.2: that is what `psql`'s Ctrl-C, JDBC `Statement.cancel()`
and every pooler already expect, and a different code turns a cancel into an
unexpected fault in client code nobody here wrote. The two are told apart by
the MESSAGE — a timeout's carries the two microsecond readings §3 promises,
which is why they are read back out of the SQL layer's prose when it has
already flattened them.

---

## 9. What the wire does NOT get

| | Why |
|---|---|
| **TLS** | `OPS_CONTRACT` §9.4. `SSLRequest` is answered `N`. `pg::bind` REFUSES a non-loopback address unless the caller passes `--allow-remote`, and the refusal says why: trust auth plus no TLS on a reachable port is an unauthenticated database on the network. |
| **Authentication** | Trust. Any user name is accepted; the database name is ignored. |
| **`JOIN` over the catalog views** | The views themselves ARE on this build and answer rows over the wire (`docs/dist/PG_SURFACE.md`). What is not built is `JOIN`, so the multi-relation form a schema tree emits is refused `0A000` naming `JOIN`; a single-relation catalog query answers. |
| **`COPY`** | No `CopyInResponse` / `CopyData`. `OPS_CONTRACT` §7's bulk-load scope is the atomic it would ride; until that is built there is nothing to stream into. |
| **`NOTIFY`** | §5 above. |
| **Binary `geometry` / `vector`** | Their synthetic OIDs are not in the binary set; a binary request gets the TEXT bytes. EWKB is `p3-geometry-io`. |
| **Transactional DDL, savepoints, two-phase commit** | sekejap has one transaction per handle and no savepoint. |
| **`SELECT` with no `FROM`** | Except the fixed session rows below: `sekejap_lang` has no statement shape for it. |
| **Service mode and publish as wire statements** | `OPS_CONTRACT` §9.4: a connection is served by the process that opened the service, and the staleness window is a property of that process, not of the protocol. `--publish-interval` is the operator's knob. |
| **A signal handler in `sekejap-pg`** | Graceful shutdown IS built -- `pg::Shutdown` stops the accept loop and `pg::serve` JOINS every connection thread before returning, so the service is free the instant it does, and `sekejap-pg` then calls `ServiceDatabase::close`. What the BINARY has no door for is a signal: this crate carries no signal dependency, so a `SIGTERM` to `sekejap-pg` is a process exit. Nothing is lost by that which a close would not also discard -- an open transaction rolls back either way -- and the page WAL's recovery is what makes the difference invisible. An embedding caller that wants the graceful path holds the `Shutdown` and fires it. |

**The one thing answered from data this layer holds**, stated so it is not
discovered: `SELECT version()`, `SELECT current_schema()`, `SELECT
current_database()`, `SELECT current_user` / `session_user`. A client sends
them before it will talk at all, and each is a CONSTANT rather than a query.
`QL_CONTRACT` §2 places them in T2 as `p3-pg-surface`; when PGCAT builds that
row these become its rows.

```sql
SELECT version();
SELECT current_schema();
SELECT current_database();
SELECT current_user;
SELECT session_user
```

---

## 10. Tests

| File | What it proves |
|---|---|
| `dist/tests/pg_wire.rs` | The BYTES. Every frame is built in the test from the protocol's own layout and every reply parsed back the same way: the startup exchange and the parameters a client reads off it, the `SSLRequest` refusal, the simple protocol, the extended protocol with a declared `$n`, a rebind that parses nothing, portal suspend and resume, the `0xFFFF` count, each SQLSTATE, the transaction statuses, `LISTEN` ordering around a commit and a rollback, the cursors, and the two named refusals. |
| `dist/tests/pg_server.rs` | The real `sekejap-pg` binary on a free localhost port, driven by `postgres` 0.19 — a client nothing here wrote, which speaks the extended protocol with BINARY parameters and BINARY results, reads `ParameterDescription` before it will encode a value, and cancels out of band. Plus `psql` itself when it is installed, and the DBeaver connect sequence with its catalog half asserted to be a NAMED REFUSAL. |

Both hold their oracle in the test process. `pg_server.rs` MEASURES the cost
of one full scan and asserts a floor, so the cancel test is a test rather
than a race.
