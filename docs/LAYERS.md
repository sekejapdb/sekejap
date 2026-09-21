# Layers

The repository is three layers. Each is a crate, and the dependency direction
between them is enforced by the compiler, not by a convention:

```
dist/rust  ->  dist  ->  lang  ->  core
```

Never the reverse. `core` cannot name anything in `lang`, and `lang` cannot
name anything in `dist`. A fourth crate, `bench`, sits outside the rule: it
depends on all three because a measurement harness has to reach whatever it
measures.

## The crates

| Path | Crate | Library | What it is |
| --- | --- | --- | --- |
| `core/kernel/` | `kernel` | `kernel` | The pager, the B-tree, the WAL, the I/O. Unchanged by the restructure. |
| `core/engine/` | `sekejap-core` | `sekejap_core` | The engine: the row codec, the store, typed collections, the index families, the query engine, the fault suites. |
| `lang/` | `sekejap-lang` | `sekejap_lang` | The query language: lexer, AST, parser, compiler, `EXPLAIN`, the refusal table, the SQL functions. |
| `dist/` | `sekejap-dist` | `sekejap_dist` | Distribution: the operator binaries in `src/cli/`, the embedded service in `src/service/` (`docs/dist/OPS_CONTRACT.md` §1-§5), and the one surface this layer owes but does not yet build (`src/pg/`). |
| `dist/rust/` | `sekejap` | `sekejap` | THE PUBLISHED CRATE, 0.17.0: one handle (`Db`), one error, documents as `serde_json::Value`, SQL with `$n` parameters, edges through the graph atomics. Its whole surface, with the E4 call each item maps to, is `docs/dist/RUST_API.md`. It adds no execution. |
| `bench/` | `sekejap-bench` | -- | Every benchmark, fixture generator and probe, as binaries. |

The workspace at the root is virtual: it owns `members`,
`default-members = ["core/engine"]`, and the `workspace.package` version,
edition and license. A bare `cargo test` at the root therefore means the
engine, which is what the lean gate's command groups assume.

## What belongs where

**core** -- anything that decides what is on disk or what a query answers.
The row format, the page-WAL store and its recovery, the catalog, the index
families (scalar, text, vector, spatial point, spatial geometry, graph), the
planner, the drivers, the cursors, the budget. Contracts:
`docs/core/GRAPH_CONTRACT.md`, `docs/core/COLLECTIONS.md`,
`docs/core/FORMAT_V1.md`, `docs/core/RECOVERY_CONTRACT.md`,
`docs/core/SPATIAL_FUNCTIONS.md`, `docs/core/FOUNDATION_TEST_STANDARD.md`.
Module map: `docs/core/SOURCE_LAYOUT.md`.

**lang** -- anything that turns TEXT into a call the engine already has. It
adds no execution: every statement compiles to `prepare_query`, `put`,
`delete`, `create_*` or a drop step, so text queries and code queries run
through one engine. A construct with no atomic underneath is REFUSED with a
named tier and reason, never emulated. Contract:
`docs/lang/QL_CONTRACT.md`; test map: `docs/lang/CONTRACT_TEST_MAP.md`.

Because `Database` belongs to core, the four `Database::sql*` methods cannot
be an inherent `impl` here -- the orphan rule forbids it. They are the
`SqlDatabase` trait instead, with the same names and signatures; a caller adds
`use sekejap_lang::SqlDatabase;` and changes nothing else.

**dist/rust** -- the one crate an application depends on. It composes `core`,
`lang` and `dist` into a surface an application can hold: `Db::open` (or
`Db::open_service` for parallel readers), `put`/`get`/`delete`/`scan` by
collection and key, `execute`/`query`/`stream`/`explain`, `link`/`unlink`/
`neighbours`, `collections`/`describe`, the three `scan_count_*` walks that are
named as walks, and `Tx` for many writes under one barrier. Every `Db::` write
commits before it returns. A construct with no atomic is REFUSED by name
(`sekejap::Error::Refused`), never emulated. Contract: `docs/dist/RUST_API.md`;
test file: `dist/rust/tests/api.rs`.

**dist** -- anything an operator or a foreign runtime touches. The binaries in
`dist/src/cli/` (`sekejap`, `recover`, `collection_inspect`, `collections`,
`control_tree_audit`, `pagewal_repair`, `entry`, `lifecycle`), the embedded
wrapper and the PostgreSQL wire surface. Contract:
`docs/dist/OPS_CONTRACT.md`; wrapper targets: `dist/bindings/README.md`.

**bench** -- anything whose output is a number rather than an answer. It is
also where `rusqlite` and `postgres` belong: the comparison arms are harness
dependencies, not engine ones.

## Running each layer

```sh
export CARGO_TARGET_DIR=... TMPDIR=...
F=compact-cells,sqlite-balance,keyspace-append,slotref-split

cargo build  --workspace --all-targets --features $F     # everything compiles
cargo test   -p sekejap        --features $F -- --test-threads=1
cargo test   -p sekejap-core  --features $F -- --test-threads=1
cargo test   -p sekejap-lang  --features $F -- --test-threads=1
cargo test   -p sekejap-kernel        --features test-support -- --test-threads=1
cargo test   --workspace      --features $F -- --test-threads=1
```

A single test file is `-p <crate> --test <name>`; a single binary is
`cargo run -p sekejap-bench --release --bin <name>` or
`cargo run -p sekejap-dist --release --bin <name>`.

## The lean profile

The eight-law lean gate is unchanged in what it runs and how it is invoked:

```sh
python3 tools/run_foundation.py lean <scratch>
```

Its command groups live in `docs/FOUNDATION_LEAN_GROUPS.json`. The engine
groups (`pager`, `typed`, `compatibility`) name no package, so they resolve to
`default-members`, which is `core/engine`; the kernel groups already said
`-p sekejap-kernel`. The harness it builds, `foundation_scale`, is now a `bench`
binary, so the gate builds it with `-p sekejap-bench`. The standard itself is
`docs/core/FOUNDATION_TEST_STANDARD.md`.
