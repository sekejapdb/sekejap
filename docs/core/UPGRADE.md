# Upgrading a database's index formats

A newer sekejap opens an older database as it is: no migration, no forced
rebuild (`CONTRACT.md`, Law 8). What an upgrade adds is the newer build's
speed on indexes an older release built. It is always a choice, never
automatic, because once an index is rewritten in a newer format a release
that predates that format can no longer open the file.

## The rule

Every release that adds an index format ships, in the same release, the step
that rewrites the older format into it. A new format is rare and decided on
purpose (`FORMAT_V2.md`); the upgrade step is part of deciding it.

## Two ways to run it

**SQL**, from any client, the PostgreSQL wire or a binding -- PostgreSQL's
own statement:

```sql
REINDEX INDEX place_text;   -- one index
REINDEX TABLE place;        -- every index of one table
REINDEX SCHEMA travel;      -- every index of one schema
REINDEX DATABASE;           -- every index
```

Each rebuilds into this build's format and answers exactly as before. It
builds the new index beside the old one under a temporary name, swaps the
names in one commit, then drops the old one in bounded steps: a query sees
the old index or the new one, never a half-built one. An interrupted run is
finished by the next (`REINDEX` drops a temporary index a crash left, and
finishes an index left dropping). `CONCURRENTLY` is accepted; every rebuild
already runs beside the old index. `REINDEX SYSTEM` is refused (there are no
system catalogs). A missing index is `42704`, a missing table `42P01`, as in
PostgreSQL.

**The command line**, for a server (`cargo install sekejap-dist --bin
sekejap-upgrade`):

```text
sekejap-upgrade --check  <db-path>                  # read-only JSON report
sekejap-upgrade --apply  <db-path> [--backup <dir>] # back up, then rebuild
```

`--check` lists every index with its family and format (`current` or
`older`), the file's logical feature word, and whether each known release can
still open the file. `--apply` does nothing when nothing is older; otherwise
it takes the database's writer, copies every file to the backup directory
(default `<db-path>.before-upgrade-<unix seconds>`, refused if it exists),
runs `REINDEX INDEX` for each older index, and prints the report again.

## What is older today

| Release | Format a newer build rewrites |
|---|---|
| 0.18.x | none yet |

The text family's posting segment format 2 (0.19, `v019-a2`) will be the
first row: a text index built by 0.18.x keeps answering exactly, without the
new skipping, until it is upgraded.

## Tests

`lang/tests/sql_reindex.rs` (every index family keeps its answers and name;
`TABLE`/`SCHEMA`/`DATABASE`; a crashed run's leftovers; `UNIQUE`; refusals),
`lang/tests/release_compat.rs::a_release_file_reindexes_to_the_same_answers`
(the preserved 0.18.3 files), `dist/tests/upgrade_cli.rs` (the binary).
