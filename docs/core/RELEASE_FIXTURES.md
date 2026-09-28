# Release fixtures

Law 8 (`CONTRACT.md`) asks that a file a released build wrote opens in every
later build with the same answers: no migration, no index rebuild. The v2
corpora (`docs/format-v2-fixtures`, `docs/format-v2-baseline`) pin the entity
format. The release fixtures pin what each RELEASE shipped, written through
SQL the way a user writes it.

## What is kept

`docs/release-fixtures/<release>/`, one directory per release, never
regenerated:

| Path | What it is |
|---|---|
| `checkpointed/` | a database the release wrote, every commit folded into `data` |
| `wal-pending/` | the same database plus later commits still in its `wal`, so opening it replays them |
| `*/EXPECTED.json` | every query the generator asked and the rows the release answered |
| `INDEX.json` | the release, its commit, each file's size and SHA-256, and each `EXPECTED.json`'s SHA-256 |

The data is a small travel world: tables in two schemas, `NOT NULL` and
constant `DEFAULT` columns, a `UNIQUE` column, btree, `lower()`, `->>`,
full-text (one index maintained row by row, one built over existing rows,
which writes the packed segment tier), point, polygon, exact, quantized and
Vamana indexes (the Vamana one on a column no exact index covers, so the
graph is what answers), an edge table fixed through `base`, untyped edges in
`base` and in a graph context, a named property graph with an alias and
labels, a dropped table, and rows that were updated, deleted and upserted.
`INDEX.json` records each database's logical feature word.

A value is recorded exactly: an integer as `{"int": n}`, a float by its bits
(`{"float": "<u64 bits>"}`), JSON as `{"json": ...}`, a row identity as
`{"id": [collection, sequence]}`. A query must answer at least one row, so no
fixture passes by answering nothing.

## The test

`lang/tests/release_compat.rs`, for every release directory:

1. every release and database the test's own `RELEASES` list names is
   present, `INDEX.json` has the SHA-256 that list pins, and every file is
   the one the release wrote (size, SHA-256, no file added or missing); the
   WAL-pending database has a WAL to replay and the checkpointed one none;
2. this build opens a copy with the release's logical feature word unchanged
   and answers every query exactly as the release did; the preserved files
   are unchanged afterwards;
3. the copy takes inserts, updates (a point, a vector) and a delete,
   checkpoints and reopens with the feature word still unchanged, answers
   them through the release's indexes and graph, and still refuses what the
   release refused (a dropped table, `23505`, `23502`).

Not yet covered, and why `L8-COMPAT` stays pending: the RELEASED binary
reading a copy after this build wrote to it (rollback), a fixture per earlier
0.18 release, and the same bytes run on every supported platform (BM25
scores are compared by their bits).

When a later release changes an answer on purpose, the fixture is never
regenerated. The change is allowed only as a correction to documented query
behaviour, and the test gains a named exception giving the query, the old and
the new answer, the documented rule the old answer broke, the regression
test, and the release note. No exception may excuse a changed meaning of a
stored value, a lost row identity, a missing or rebuilt index, or an
automatic conversion.

## Writing a release's fixtures

The generator is `lang/examples/release_fixtures.rs`. It uses only SQL and
engine calls every 0.18 release has, and it is built against the release's
OWN source: a worktree at the tag with this one file copied in.

```sh
F=compact-cells,sqlite-balance,keyspace-append,slotref-split
TAG=v0.18.3                     # the release tag
WT=$SCRATCH/wt-$TAG             # a scratch directory outside the repository
git worktree add --detach "$WT" "$TAG"
mkdir -p "$WT/lang/examples"
cp lang/examples/release_fixtures.rs "$WT/lang/examples/"
(cd "$WT" && cargo build --release --locked -p sekejap-lang --example release_fixtures --features $F)
(cd docs/release-fixtures &&
  "$CARGO_TARGET_DIR/release/examples/release_fixtures" "${TAG#v}" "${TAG#v}" "$(git rev-parse "$TAG^{commit}")")
git worktree remove --force "$WT"
```

The generator refuses an output directory that exists. Before committing,
check the new files hold no path, account or host name (they hold only the
fixture's own data and the database's random identity).

## Preserved

| Release | Commit | Generated |
|---|---|---|
| 0.18.3 | `905124a` | 2026-09-28, from the tag, `--locked`, default retained features |

The 0.18.3 fixture also preserves one 0.18.3 behaviour: a text literal
written into a `JSONB` column is stored as a JSON string, where PostgreSQL
parses it (`sanur`'s `info`). A later build that parses such literals must
still read the stored string as a string.
