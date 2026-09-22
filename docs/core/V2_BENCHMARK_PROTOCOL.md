# V2 foundation benchmark protocol — 2026-09-16

`bench/src/bin/v2_foundation_bench.rs` is a fair, streaming, mixed-data comparison
of sekejap's typed collections (`e4_prototype::collections::Database`, its actual
public API) against SQLite with typed columns and one JSONB field, at
default N = 1,000,000 rows. It is a new, independently owned binary; it does
not modify `src/collections.rs`, Cargo.toml, the kernel, or any existing
benchmark/test.

This is a raw-write/mixed-CRUD comparison. It is **not** a measurement of
combined multimodel SELECT performance, and it does not claim evidence for
any of the eight laws in `CONTRACT.md` beyond what it directly measures
(exact verification of writes, deletes, and reopen).

## Data model

One collection, `people`, with fields:

| field | Kind | notes |
|---|---|---|
| name | Text | includes non-ASCII (東京-é) to exercise UTF-8 handling |
| born | Int | |
| born_year | Int | |
| active | Bool | |
| income | Real | nullable (`slot % 7 == 0`, ~14.3% of rows) |
| location | Point | GeoJSON-shaped `{"type":"Point","coordinates":[lon,lat]}` |
| profile | Json | nested object: languages array, preferences object, tags array with a null, household object |
| vector | Vector(dim) | default dim = 8, override with `V2_VECTOR_DIM` |

The external key (sekejap's `put(collection, key, doc)` key; SQLite's
`external_key` unique column) is a plain string, not part of the typed
document. Content is a **pure function of a `slot: u64` and a
`version: u64`** (same convention as `person()` in `bench/src/bin/people.rs` and
`doc()` in `dist/src/cli/collections.rs`): nothing about a row needs to be held
in memory to generate, write, or verify it — it is recomputed on demand,
including during verification and after reopen. This is why N=1,000,000 (or
larger smoke/scale values) never requires an in-memory table of documents.

Vector values are pre-rounded to their f32 representation before being
placed in the expected JSON (`(x as f32) as f64`), because both engines
store vector lanes as exact f32: sekejap's dense_v3 vector keyspace (see D22 in
CONTRACT.md) and the SQLite arm's `vector BLOB` of little-endian f32. This
avoids spurious verification failures from f64-vs-f32 rounding that would
not reflect a real difference between the engines.

## SQLite schema (typed columns + one JSONB field, not the whole document)

This is the COMPARISON ARM's schema, in SQLite's dialect, recorded as it was
run. It is not sekejap SQL and the documentation harness does not execute it:
`INTEGER PRIMARY KEY`, `UNIQUE` and `BLOB` are SQLite's, and the bracketed
line is a note about which columns the timestamps mode adds rather than
syntax.

```text
CREATE TABLE people(
  id INTEGER PRIMARY KEY,
  external_key TEXT NOT NULL UNIQUE,
  name TEXT NOT NULL,
  born INTEGER NOT NULL,
  born_year INTEGER NOT NULL,
  active INTEGER NOT NULL,
  income REAL,
  lon REAL NOT NULL,
  lat REAL NOT NULL,
  profile BLOB NOT NULL,   -- jsonb(), the nested "profile" object only
  vector BLOB NOT NULL     -- little-endian f32 lanes, exact dim match
  [, created INTEGER NOT NULL, updated INTEGER NOT NULL]  -- timestamps=on only
);
```

`id` is a plain `INTEGER PRIMARY KEY` (SQLite rowid alias), not
`WITHOUT ROWID`, since this benchmark has a single collection with no
composite key — `WITHOUT ROWID` composite keys are the right choice for
`dist/src/cli/collections.rs`'s two-collection harness, not here; this is a
deliberate, named schema choice, not a copy-paste of the other harness.
Scalars and the point get typed columns; only the nested `profile` object is
JSONB, matching the instruction not to duplicate the whole document as JSON.

## Fairness knobs matched between engines

- 8 MiB cache: kernel `Config::budget_bytes = 8<<20`; SQLite
  `PRAGMA cache_size=-8192`.
- `SyncMode::Full` for sekejap; `PRAGMA synchronous=FULL`, `fullfsync=ON`,
  `checkpoint_fullfsync=ON` for SQLite (inert on Linux/non-macOS but kept
  for parity with every other benchmark already in this repo, which sets
  these unconditionally).
- Same batch size for both engines: env `V2_BATCH` (default 1000), applied
  as the transaction boundary — commit every `batch` operations, and again
  at the end of every phase.
- **`PRAGMA wal_autocheckpoint=1000`** (env `V2_SQLITE_AUTOCHECKPOINT`,
  default 1000 pages ≈ 4 MiB at the shared 4096-byte page size) is the main
  SQLite policy, chosen to match sekejap's own automatic page-WAL fold, which
  happens near its internal allowance, roughly 4 MiB (`src/pagewal.rs`).
  An earlier version of this benchmark used `wal_autocheckpoint=0`
  (unbounded WAL growth between explicit checkpoints), which the parent
  review correctly flagged as unfairly inflating SQLite's WAL/peak-disk
  numbers relative to an engine that folds automatically. `0` remains
  available via the env var strictly as a separate, clearly labelled
  no-auto-checkpoint experiment — it must never be presented as the main
  SQLite comparator. Both engines' **actual observed policy** (not just the
  PRAGMA/config we requested) is queried and recorded in the JSON report
  under `initial_policy` and, after reopen, `reopened_policy` — SQLite via
  `PRAGMA wal_autocheckpoint`/`synchronous`/`journal_mode`; sekejap as a fixed
  description of its automatic-fold behavior, since it has no equivalent
  runtime-queryable pragma today.
- In addition to each engine's own automatic policy, an **explicit
  checkpoint step** runs after every phase and once more at the very end,
  timed separately from write time (`checkpoint_seconds`, never folded into
  `seconds`). For sekejap this calls the new public `Database::checkpoint() ->
  Result<bool>`; the returned `bool` — whether the fold **completed** or
  was **deferred** (e.g. by a live reader) — is recorded per phase as
  `checkpoint_completed`. For SQLite it is `PRAGMA wal_checkpoint(TRUNCATE)`,
  with `completed` derived from `busy==0 && checkpointed_frames==wal_frames`.
  `checkpoint()` is never used to perform or substitute for a commit: every
  phase's transaction(s) are committed exactly once, inside `run_phase`,
  before the separate `checkpoint()` call runs against an already-durable,
  already-published database; the final end-of-run checkpoint call is
  likewise not preceded by an extra commit (an earlier version of this
  benchmark had that bug — attempting `COMMIT` with no open SQLite
  transaction — caught and fixed before this revision).
- Same row content/order per phase (see "Phases" below) and same vector
  dimension for both engines in a given run.
- Timestamps default off. `off|on` is a CLI argument (not two separate
  binaries), matching every other benchmark's convention. When on, both
  engines use a **deterministic per-phase clock** (`PhaseClock`,
  `phase_time(phase) = 1_800_000_000 + phase`), not a single frozen
  constant: it is set once, before that phase's first write, so within a
  phase every write shares one "now," and "now" strictly advances phase to
  phase. This reproduces the exact real-timestamp contract on both engines:
  a fresh identity's `created` and `updated` both equal its own creation
  phase's time; an existing identity's `created` never changes on a
  subsequent update, while its `updated` advances to the update's phase
  time. sekejap gets this for free through `Database::set_clock` and its own
  existing `created = old-or-now` / `updated = max(now, created, previous)`
  logic; the SQLite arm computes the same `now` value directly and only
  ever writes `updated` (never `created`) in its `UPDATE` statement, so an
  update can never disturb the original creation stamp on either engine.
  The independent oracle (`with_timestamps_at`) computes the same
  created/updated phase pair from the same group rules, not by calling
  either engine.
- SQLite PRAGMAs — including `wal_autocheckpoint` — are reapplied after
  reopen (`Db::reopen`), since a fresh `Connection` does not inherit the
  previous connection's non-persistent pragmas; the reapplied policy is
  then re-queried and recorded (`reopened_policy`), not assumed.
- Reopen genuinely closes before it opens. `Db::reopen` takes `self` by
  value and returns a new `Self`, explicitly `drop`-ing the old
  `Database`/`Connection` before constructing the replacement. An earlier
  revision wrote `*d = Database::open(path, cfg())?;` through a
  `&mut Database`: Rust evaluates the right-hand `open()` call before the
  assignment, so the *old* handle — and, for sekejap, its `writer.lock` — was
  still held open at that point, which would fail the new open with
  `WriterLocked` against a real page-WAL database rather than a real
  close-then-reopen. Neither engine uses an in-memory placeholder to paper
  over this; both arms close the real on-disk database and reopen the real
  file.
- Prepared statements are cached and reused (`prepare_cached`) rather than
  re-parsed per operation.
- Timing is per-phase (one `Instant` per phase, covering every operation and
  intermediate commit in that phase), not per-operation, per the "avoid
  per-operation Instant overhead" instruction. Generation cost is not timed
  separately from writing, and — unlike verification — it is **not excluded
  from the reported totals either**: content is a cheap pure function with
  no file I/O, called inline inside the same loop that writes it, so it is
  genuinely part of each phase's `seconds` (and therefore of `total_seconds`)
  for both engines identically. This is a **named deviation** from
  `people.rs`'s convention of a separate, separately-timed fixture-generation
  pass over a JSONL file, not an attempt to hide cost — an earlier revision
  of this document incorrectly said generation was excluded from
  `total_seconds`; it is included, and `total_seconds_definition` in the
  JSON report says so explicitly now.
- Each arm gets its own `<arm-path>/tmp` directory — a subdirectory of the
  same directory the recursive disk sampler (`disk()`) already walks for
  that arm — and `TMPDIR`/`SQLITE_TMPDIR` are pointed at it before that
  arm's engine is opened. An earlier revision pointed both variables at a
  directory *outside* either engine's own directory (a shared
  `<root>/tmp`), so any temp files created during the run were invisible to
  every reported logical/allocated/peak figure; this is fixed by giving
  each arm its own env-pointed temp directory nested inside the same tree
  `disk()` walks, so total/peak space is now measured completely for that
  arm. Creation order differs by engine because `Database::create` creates
  its directory itself with a bare `fs::create_dir` (fails if the directory
  already exists; see `PageWalStore::open`): for the sekejap arm, the env vars
  are set to a not-yet-existing path first (sekejap does not read them during
  creation) and the `tmp` directory itself is created immediately after
  `Database::create` returns and the directory exists; for the SQLite arm,
  both the arm directory and its `tmp` subdirectory are created up front
  (via `fs::create_dir_all`, not the previous bare `fs::create_dir`, which
  would otherwise fail once the directory already exists) before the
  connection opens.

## Named asymmetry (recorded, not hidden)

`Database::put()` (sekejap's only write entry point available on the public
API) always performs its own internal existence lookup as part of its
upsert contract (`load_entity` before `write_entity`). The SQLite arm's
driver already knows, from the phase's own logic, whether a given operation
is a create or an update, and issues a plain `INSERT` or a keyed `UPDATE`
accordingly — it does not perform an equivalent existence lookup. This is
less SQLite-side work per write than sekejap's API requires per call, and it is
recorded here explicitly rather than presented as identical work. A more
"unfair-to-SQLite" alternative (a generic `INSERT ... ON CONFLICT DO
UPDATE`) was rejected because it would require a redundant round-trip that
the benchmark driver's own knowledge makes unnecessary for a typical
application; either choice should be named, and this is the one used here.

## Phases and row-selection convention

Reuses the residue-class convention already established in
`docs/core/FOUNDATION_TEST_STANDARD.md` (`W-UPDATE` 20% per round, `W-DELETE` 10%
per round, `W-MIXED` update 20%/delete 10%/insert 10% per round) and in
`dist/src/cli/collections.rs`'s `doc()` function (disjoint `slot % 5 == 0`
update group and `slot % 10 == N` delete/replace groups). The one explicit
deviation: `collections.rs`'s cases always pair a delete with an immediate
replacement in the same pass, so population never visibly drops. This
benchmark adds a **standalone delete phase with no immediate replacement**,
so the reduced-density state is directly observable rather than only ever
churning at constant count.

Groups, all against a slot index `0 <= slot < N`:

- **U** (update): `slot % 5 == 0` — 20%.
- **D** (delete-only, then reinsert): `slot % 10 == 1` — 10%.
- **M1** (mixed round 1 delete+replace): `slot % 10 == 6` — 10%.
- **M2** (mixed round 2 delete+replace): `slot % 10 == 2` — 10%.

These four groups are pairwise disjoint (residues 0 mod 5 vs. 1/6/2 mod 10
never coincide), so every slot has exactly one well-defined identity at
every phase, which is what makes the oracle a pure, streaming function.

| # | phase | operation | count |
|---|---|---|---|
| 0 | `load` | insert all slots, version 0 | N |
| 1 | `update_round_1` | update U, version 1 | N/5 |
| 2 | `update_round_2` | update U, version 2 | N/5 |
| 3 | `delete_only` | delete D, **no replacement** | N/10 |
| 4 | `replacement_reinsert` | insert D under brand-new keys, version 0 | N/10 |
| 5 | `mixed_round_1` | update U (version 3); delete+replace M1 under new keys | N/5 updates + N/10 deletes + N/10 creates |
| 6 | `mixed_round_2` | update U (version 4); delete+replace M2 under new keys | N/5 updates + N/10 deletes + N/10 creates |

Live population is N at every phase except phase 3 (`delete_only`), where it
is exactly `N - |D|` — the visible density drop the instructions ask for.
Reinsert/mixed-replacement keys (`person/reinsert-*`, `person/mixed{1,2}-*`)
are never reused from a deleted slot's old identity; sekejap's own sequence
allocator is monotonic per collection and never reuses a sequence number
(see `collections.rs`'s `allocate`), and the SQLite arm mirrors this with a
monotonically increasing `next_id` counter that is only advanced on a
genuine create.

## Oracle / verification

After every phase, and after the final reopen, an independent oracle
(`expected_stream`) streams the expected `(id, key, document)` triples **in
the same order the real scan/SELECT returns them** — ascending original
slot order for identities that have never moved, followed by each later
phase's freshly created identities in ascending slot order within their
group. This lets verification zip the real result stream against the
oracle stream positionally with O(1) extra memory (no hash map of N
documents), while still checking every field of every row exactly
(`assert_eq!` on the full JSON document, not just a CRC), plus:

- **exact expected id, not merely increasing**. An earlier revision only
  asserted `id > previous`, which cannot catch an id that is wrong but
  still larger than the last one. The oracle now independently derives the
  *exact* id each row must have, from sekejap's own sequence-allocation contract
  (a per-collection monotonic counter that advances only on a genuine
  create) mirrored exactly by the SQLite arm's `next_id`: a surviving
  original slot keeps its load-time id `slot + 1`; each later group's fresh
  identities get consecutive ids starting right after the highest id any
  earlier phase could have allocated (`N`, then `N + |D|`, then
  `N + |D| + |M1|`), assigned via `enumerate()` over the *same* filtered,
  ascending-slot iterator that `run_phase` uses to actually perform the
  creates — so the rank an id is derived from is the same rank the create
  ran in, never a separately-recomputed arithmetic formula that could
  silently drift from the real write order. `assert_eq!(id, expected_id)`
  runs on every row, at every phase, and after reopen; the strictly-
  increasing check is retained alongside it as a second, independent
  invariant;
- **deleted-key absence**: phase 3's oracle simply omits the delete group,
  so any leftover row there fails the row-count assertion;
- an accumulated CRC32C over id/key/document bytes, in addition to the
  per-row equality, as a second, independent check *within* one engine's
  run — and, explicitly, **across** the two engines: `main()` compares
  sekejap's and SQLite's accumulated CRC32C for every phase and after reopen
  (`cross_check_crc`) and fails loudly if they differ. Both arms serialize
  id (`to_le_bytes`), key (UTF-8 bytes) and document (`serde_json::to_vec`)
  identically, so if both engines truly match the same oracle their CRCs
  must also match each other; this is a real independent cross-engine
  invariant, not a restatement of the per-row oracle check the two arms
  already pass separately.

## Reporting

Per phase, the JSON report includes: exact operation counts
(creates/updates/deletes), write-only `seconds` (the phase's operations and
their intermediate commits, excluding checkpoint and verification),
`checkpoint_seconds` and `checkpoint_completed` from the separate explicit
checkpoint step described above, `checkpoint` (the engine-specific
checkpoint detail — sekejap's `completed` bool, SQLite's busy/wal_frames/
checkpointed_frames), cumulative write seconds, cumulative checkpoint
seconds and their sum (`cumulative_engine_seconds`), a 1&nbsp;ms-sampled
peak logical/allocated size (explicitly a **lower bound**, not a guaranteed
maximum or an enforced cap), final logical/allocated bytes for that phase,
live population, and the full verification result (including its own
`verify_seconds`).

After all phases: one more explicit checkpoint (`final_checkpoint_seconds`,
`final_checkpoint_completed`) with its own timing, a fresh reopen (drop and
recreate the handle; PRAGMAs reapplied for SQLite, then re-queried into
`reopened_policy`) with its own timing, and a full oracle-verification of
the reopened database. `total_seconds` is the sum of every phase's write
seconds, every phase's checkpoint seconds, the final checkpoint, and the
reopen — this is **engine-only** work. Oracle verification time is real,
necessary benchmark-driver work (an independent re-derivation and full
comparison of every row), but it is not either engine's cost: it is reported
per phase and as a separate cumulative total
(`cumulative_verify_seconds_excluded_from_total`,
`reopen_verify_seconds_excluded_from_total`) and is never added into
`total_seconds`. Content generation is not timed separately at all (see
"Fairness knobs" above for why) — it is real but negligible pure-CPU work
folded into the same write-seconds figure for both engines equally, which
is a stated, not hidden, choice.

Disk accounting (`disk()`) walks the whole per-engine directory recursively
(data/WAL/shared-memory/temp files together), reporting both logical bytes
(`len()`) and allocated bytes (`blocks()*512`) — it excludes only the
benchmark's own JSON/Markdown output and the binary itself, which live one
level up in the shared run root, not inside either engine's directory.
Each arm's `TMPDIR`/`SQLITE_TMPDIR` point at `<arm-path>/tmp`, a
subdirectory of the exact tree `disk()` walks for that arm (see "Fairness
knobs" above for the creation-order details), so any temp files either
engine creates during the run are included in that arm's logical/
allocated/peak totals — not left outside both engines' measured trees in a
separate root-level directory, which an earlier revision did.

Final output: `results.json` (full nested manifest for both engines) and
`REPORT.md` (a side-by-side Markdown table, one row per phase, plus a
totals/footer section) under the run root. No VACUUM/rebuild is run inside
the timed path; this benchmark does not implement an optional rebuild arm
at all (out of scope for this pass — a natural follow-up, not a hidden
step).

## Authorized directory

`ROOT` is a required, absolute CLI argument with no `..` components; the
binary creates `ROOT/v2-foundation-<N>-<unix-seconds>/` for this run's
artifacts (mirroring the timestamped-run-directory convention in
`bench/src/bin/people.rs`). It performs no path allow-listing beyond that — the
caller (the parent process, on the authorized Linux/PVC path) is
responsible for pointing it at an authorized location, since this binary
has no way to know which paths on the target host are authorized.

## How to run

```sh
cargo build --release --bin v2_foundation_bench

# full run, 1,000,000 rows, timestamps off (baseline)
./target/release/v2_foundation_bench /authorized/path off
# (N defaults to 1_000_000; pass it explicitly if you want a different size)
./target/release/v2_foundation_bench /authorized/path 1000000 off

# explicit-timestamps scenario
./target/release/v2_foundation_bench /authorized/path 1000000 on

# small smoke run
./target/release/v2_foundation_bench /authorized/path 10000 off
```

Env overrides: `V2_VECTOR_DIM` (default 8), `V2_BATCH` (default 1000),
`V2_SQLITE_AUTOCHECKPOINT` (default 1000 pages; set to `0` only to run a
separate, explicitly labelled no-auto-checkpoint experiment — never the
main comparator).

```sh
# separate experiment only, not the main result:
V2_SQLITE_AUTOCHECKPOINT=0 ./target/release/v2_foundation_bench /authorized/path 1000000 off
```

## Known limitations / not claimed

- This measures one collection with one schema shape; it is not a claim
  about multi-collection behavior (already covered by
  `dist/src/cli/collections.rs`) or about query/SELECT performance of any kind.
- Peaks are 1 ms-sampled logical/allocated lower bounds, matching every
  other benchmark's stated peak semantics in this repo — not a guaranteed
  maximum and not an enforced cap.
- No rebuild/VACUUM arm is included.
- This does not exercise `Database::update()`'s patch API — like
  `people.rs` and `collections.rs`, every write here is a full-document
  `put()`, which is the existing convention across this repo's benchmarks.
- Results depend on whatever `Database` public API exists at build time;
  if the parallel typed-collection/PageWalStore integration changes method
  signatures, this binary needs matching updates before it will build.
  As of this revision it calls the new `Database::checkpoint() -> Result<bool>`
  that the integration is adding; this binary cannot build before that
  method lands.

## The 50K battle: two sekejap arms per run, frozen Postgres and SQLite references

`bench/src/bin/battle50k.rs` is a different measurement from the one above:
not raw writes, but forty-one QUERY cases over a fixed 50,000-row corpus that
lives on disk — graph, vector, spatial, text, scalar, boolean, aggregate and
row-function shapes — asked of four arms, one arm per process.

| Arm | What it is |
| --- | --- |
| `e4` | an embedded `Database`, the battery built as `QueryRequest`s in Rust |
| `e4-sql` | the same database and the same battery, asked as SQL text |
| `postgres` | a real server: PostGIS + pgvector + pgvectorscale |
| `sqlite` | an embedded SQLite: FTS5, two R*Trees, and registered scalar functions that call `sekejap-core` itself |

The `sqlite` arm's `--db-dir` names the `.db` FILE, not a directory. SQLite
has no geometry type, no geodesic, no vector type and no traversal atomic, so
each of those reaches `sekejap-core` through a registered scalar function —
`geo_dist_m` is `spatial_math::wgs84_distance_metres`, the geometry
predicates are `spatial_geometry::{within, contains, intersects, dwithin_m}`,
and every radius candidate box is `spatial_math::radius_candidate_bounds`.
The ORACLE is therefore the same routine in both arms: a row-count difference
would be a real difference and not a second implementation of a predicate.
What the arm measures is SQLite's cost of REACHING that routine — an R*Tree
candidate, a per-row GeoJSON parse, a full scan for every vector case. A case
SQLite cannot express is reported as `n/a: <reason>`, never skipped, and the
statement each case ran is in the report's `sql` field and in
`<dump>/<case>/statement.sql`.

### Postgres and SQLite are constants; sekejap is what moves

Both external engines are frozen. Rerunning them once per sekejap pass measures
two engines that did not change, and costs a running Postgres server for a
number that did not move. `<scratch>/` holds
`pg-50k.json` and `sqlite-50k.json` beside a `README.md` and a
`manifest.json` recording the date, the engine versions (`SELECT version()`,
`SELECT postgis_full_version()`, `pg_extension`, and SQLite's
`sqlite_version()` as the arm itself read it into the report's `engine`
block), the machine, the corpus SHA-256 and the exact commands.

A comparison that names no `postgres` and no `sqlite` report fills both arms
from that directory:

```sh
# the ordinary pass: two E4 arms against the frozen references
python3 tools/battle50k_compare.py <e4.json> <e4-sql.json>

# a refreeze pass, or any run that wants live externals: name them
python3 tools/battle50k_compare.py <e4.json> <pg.json> <e4-sql.json> <sqlite.json>
```

Reports are routed by their own `arm` field, so the order on the command line
is not meaning. `--frozen <dir>` overrides the default directory.

**Provenance is checked, never assumed.** Whenever a frozen file is used, the
script hashes the corpus the live reports name and refuses unless it equals
the manifest's `corpus_sha256`, and refuses any report — live or frozen —
that is older than the corpus file itself. A refusal prints `REFUSED: …` on
stderr, names the number that differed and exits 2. There is no flag that
skips it: a frozen median measured on different rows is not a reference, it
is a wrong answer. Refreeze when the corpus changes, when either engine is
upgraded, or when a case is added to the battery.

### What the comparison prints

Stages, then one CASES table with a median per arm, the E4/PG and E4/SQLITE
ratios, the `e4-sql` run/prepare split, a row count per arm, and an
agreement verdict computed across EVERY arm that ran the case — not two —
printed with the row count they agreed on. Row-count agreement on every
filter case is the precondition: a latency ratio between two arms that
answered different questions is not a measurement. Then the approximate
recall-vs-latency sweeps, then every arm's deviations, then **ANOMALIES** —
every case where sekejap is slower than Postgres or than SQLite, worst ratio
first, and every disagreement. That last section is what the battery exists
for.
