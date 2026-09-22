# Index contract — what every column type gets, and what you choose

Companion to `docs/lang/QL_CONTRACT.md`. That document fixes what the language
accepts. This one fixes **which indexes exist without being asked for, which
ones you declare, and why the line falls where it does.**

## The principle

> An index is declared when there is a decision. It is automatic when there
> is not.

A decision means two or more defensible answers with different costs. Choosing
between an exact vector index and a quantized one is a decision: recall against
hundreds of gigabytes. Choosing how to index a `SMALLINT` is not a decision.
There is one implementation, it costs a few bytes per row, and no one would
ever pick differently.

Asking a caller to opt into the second kind is ceremony with nothing behind it.
It also breaks the first five minutes: a table with a name, a number and a flag
is the first thing anyone writes, and every predicate over it would be refused.

The engine already works this way in one place. Every collection is given its
key mapping without being asked, which is why `WHERE _key = $1` answers with no
`CREATE INDEX` anywhere. This contract extends the line that already exists
rather than inventing a new one.

## What each type gets

**Automatic** means the index is created with the column and maintained on
every write, with no declaration. **Declared** means the statement refuses
until you create it, and the refusal names what it needs.

| declared type | stored as | automatic | you may declare | why the line is here |
|---|---|---|---|---|
| `TEXT`, `VARCHAR` | text | scalar (`btree`) | `gin` over `to_tsvector` | equality, ranges and prefix `LIKE` are choiceless. Full-text is a decision: an analyzer, and a large index |
| `INT`, `INTEGER`, `INT4`, `SMALLINT` | 64-bit int | scalar (`btree`) | — | one implementation, a few bytes per row |
| `BIGINT`, `INT8` | 64-bit int | scalar (`btree`) | — | as above |
| `REAL`, `FLOAT4` | 64-bit float | scalar (`btree`) | — | as above |
| `DOUBLE PRECISION` | 64-bit float | scalar (`btree`) | — | as above |
| `BOOLEAN`, `BOOL` | bool | scalar (`btree`) | — | two values; the index is nearly free |
| `TIMESTAMPTZ`, `TIMESTAMP` | int microseconds | scalar (`btree`) | — | ranges, `EXTRACT(YEAR …)` and `date_trunc` are all one range over this index |
| `DATE` | int microseconds | scalar (`btree`) | — | as above |
| `GEOMETRY(Point,4326)` | point | spatial point | — | one family applies to a point; nothing to weigh |
| `GEOMETRY(<other>,4326)` | geometry cells | geometry | — | the family follows from the declared shape |
| `VECTOR(n)` | f32 row | **nothing** | `exact`, `quantized`, `vamana` | the only genuine trade in the system: exactness against size against recall. See `docs/core/VECTOR_CAPACITY.md` |
| `JSONB`, `JSON` | binary JSON | **nothing** | **nothing exists** | see the gap below |

| `VECTOR(n)` | f32 row | **nothing** | `exact`, `quantized` | the only genuine trade in the system: exactness against size. See `docs/core/VECTOR_CAPACITY.md` |
| `JSONB`, `JSON` | binary JSON | **nothing** | a scalar (`btree`) over ONE extracted member: `CREATE INDEX i ON t ((col->>'member'))` | a document has no single order, so nothing is automatic; the member you filter on is a decision only the caller can make |

## The two with nothing automatic, and why they differ

**`VECTOR(n)` is deliberate.** At fifty million rows a vector column is
hundreds of gigabytes before any index, the three families differ by an order
of magnitude in size and by what they promise about the answer, and none is
right for everyone. This is the one place where the caller must state an
intent, and the refusal that names the families is doing its job.

### The three vector families, side by side

| family | spelling | what a query reads | the answer | bytes per row (128 lanes, measured) |
|---|---|---|---|---|
| `exact` | `USING exact (emb)` | every f32 sidecar | EXACT | 8 (a 6-byte locator plus its key) |
| `quantized` | `USING quantized (emb vector_cosine_ops)`, and `hnsw`/`ivfflat` | every int8 entry | approximate shortlist, f32 reranked | 148 |
| `vamana` | `USING vamana (emb vector_cosine_ops)`, and `diskann` | the nodes within the search list's reach | approximate shortlist, f32 reranked | 872, in TWO records: a 142-byte head in `0x7D` and the rest as edges in `0x7F` |

**What a write does, per family.** `exact` writes one locator. `quantized`
writes one entry. `vamana` LINKS the row into the graph: one greedy search
bounded by the build search list (L = 100), then at most R = 48 neighbour
ADJACENCY records read-modify-written, plus the amortised cost of the
neighbour prunes the new back edges trigger. A delete UNLINKS: at most R
records read and written, and the departing node's neighbourhood is
reconnected in the same step. An update that moved the vector is an unlink
followed by a link; one that did not touch the vector costs a comparison and
nothing else.

**What a vamana node is, and why the write cost does not grow with the
dimension.** A node is TWO records under one feature bit: its HEAD — the
6-byte locator, the quantizer scale and one int8 code per lane, byte for byte
what a `quantized` entry holds — in keyspace `0x7D`, and its neighbour list —
at most `4 + 2R * 12 = 1,156` bytes, always inside one page — in keyspace
`0x7F`. The head is written once, when the node is linked, and is never
rewritten: an update that moved the vector is an unlink followed by a link.
So an edge append or a reprune rewrites a few hundred bytes of ONE adjacency
record and never touches the codes. That matters past 512 lanes, where the
head alone exceeds a page: with the two in one record an insert at 4,096
lanes dirtied 2.65 MiB and every build transaction was refused by the
page-WAL's 16 MiB allowance whatever the batch size; split, the same insert
dirties about 260 KiB, and the figure is set by R and barely by the
dimension. The price is one extra point read per node a walk reaches, against
fewer bytes read per node.

**What a BUILD costs, and what bounds one transaction.** A late build is driven
in bounded transactions (`Database::build_index_to_ready(id, chunk_rows)`), and
one transaction may occupy at most 16 MiB of the page-WAL (`WAL_CAP`,
`core/engine/src/store/pagewal/mod.rs:30`, checked at `:509`). That ceiling is
not a policy a caller can raise: `wal_allowance()` at `:1244` is
`WAL_CAP.min(limits.1)`, so `set_runtime_limits` can only lower it. For `exact`
and `quantized` the ceiling is unreachable -- a locator is 8 bytes and an int8
entry is the vector's width. For `vamana` it is not, because a build
transaction's page footprint is set by the SEARCH and the degree rather than by
the row count: each insert writes its own node record AND read-modify-writes up
to R = 48 neighbour records scattered across the `0x7D` keyspace, and every
page so touched is framed. Measured over 10,000 rows at 4,096 lanes
(`bench/src/bin/vector10k.rs`): **1.53 MB of page-WAL per inserted node**, so
about ELEVEN nodes fill one transaction, the build is admitted only at 4 rows
per transaction, and it writes 15.3 GB of page-WAL in total -- ninety-four
times the 163.8 MB the vectors themselves occupy. At 128 lanes the same
statement is admitted at 256 rows per transaction. The consequence for the
query language is stated where it bites: `CREATE INDEX ... USING vamana`
compiles to `build_index_to_ready(id, 256)` with the chunk fixed in the
compiler (`lang/src/compile/plan.rs`), so at 4,096 lanes there is NO SQL
spelling of this index -- the statement is refused with
`ResourceLimit("page-WAL managed-byte allowance")`, and only a caller using the
engine atomic, which chooses its own transaction size, can build one.

**What a vamana index promises while it is BUILDING.** Nothing, and it says
so. A graph over part of a corpus answers a different question from a graph
over all of it, so a vector order over a `BUILDING` vamana index is REFUSED by
name (`index is not ready`) rather than answered from the part that exists.
That is the whole of this family's staleness story: there is no deferred
maintenance and no rebuild, so once it is READY the graph a query walks is
always the graph the committed rows describe
(`core/engine/tests/index_vector_vamana.rs`
`a_build_that_stops_part_way_leaves_the_index_not_ready_and_refuses_to_answer`).

**Which family answers `ORDER BY emb <=> $1`.** The EXACT index answers it
unless the session has set `ef_search` (or its `diskann.query_search_list_size`
spelling), because without a shortlist bound there is nothing approximate to
ask for. Once a bound is named, or when the column has no exact index at all,
the order is APPROXIMATE and the answer comes from the `vamana` graph if the
column has one and from `quantized` otherwise — the two mean the same thing,
an `ef`-bounded shortlist of int8 candidates reranked exactly against the f32
sidecars, and differ only in how the shortlist is found, so the family a
caller had to ask for by name is the one that answers. The notice on the
statement says which did (`lang/tests/sql_vamana_order.rs`). A column with no
vector index at all is REFUSED by name, and the refusal lists all three
spellings.

**Where the line between `quantized` and `vamana` falls.** `quantized` reads
every entry, so its cost grows with the row count and its recall does not
depend on a knob. `vamana` reads the nodes its search list reaches — about
1,800 records out of 10,000, measured — so its cost grows with the search list
and barely with the corpus, and its recall is what the search list buys.
Measured over 10,000 clustered 128-lane rows: recall@10 of 0.84 at a search
list of 40, 0.92 at 100 and 0.99 at 200. Below a few hundred thousand rows the
linear scan is simply faster; past that the graph is the only one of the two
that stays affordable.

**`JSONB` gets nothing automatically, and that is a decision now rather than a
gap.** A document has no single value to order, so there is nothing an
automatic index could be an index OVER. What exists is the one thing a caller
can state: an EXPRESSION index over ONE member of the document, which stores
the member's TEXT in an ordinary scalar index.

```sql
CREATE TABLE ix_people (payload JSONB) WITH (index: none);
INSERT INTO ix_people (_key, payload) VALUES ('a', '{"status": "live", "tier": "gold"}');
INSERT INTO ix_people (_key, payload) VALUES ('b', '{"status": "draft"}');
INSERT INTO ix_people (_key, payload) VALUES ('c', '{"tier": "gold"}');
-- one member, declared
CREATE INDEX ix_people_status ON ix_people ((payload->>'status'));
-- and the equality over that same expression is answered from it
SELECT _key FROM ix_people WHERE payload->>'status' = 'live';
```

**The extraction rule, which is total.** `col->>'member'` has ONE defined
stored value for every document the column can hold:

| at the member | stored |
|---|---|
| a JSON string | that string, unchanged |
| a JSON number | its canonical JSON text (`7`, `-2.5`) |
| `true` / `false` | `true` / `false` |
| JSON `null` | the NULL key |
| the member is ABSENT | the NULL key |
| an object or an array | the NULL key |
| the COLUMN is null or missing | the NULL key |

The NULL key is the one key a missing value and a null value already share,
and no equality on a value can name it: a row whose member is absent is not
findable by `payload->>'member' = <anything>`. An object or an array collapses
to it rather than to its serialisation, which is where this deviates from
PostgreSQL: `->>` there returns the serialised JSON, and a serialisation is a
second encoding the disk format would have to freeze -- key order, spacing,
number form -- for a value no equality in this slice can usefully name.

**What is still refused.** `->` returns the JSON VALUE rather than its text and
has no scalar key, so it stays refused by name; so do `#>`, `#>>` and
`json_array_length`. `->>` itself compiles in exactly TWO positions -- the
target of a `CREATE INDEX`, and a WHERE equality that matches such an index --
and is refused by name everywhere else. A WHERE equality over a member with no
matching index is REFUSED naming the index it would need, never demoted to a
scan; and the member is part of the index's identity, so an index over
`payload->>'status'` does not answer a predicate over `payload->>'tier'`.

A general inverted index over whole documents, the equivalent of PostgreSQL's
`jsonb_path_ops`, is still not built: it is the larger and separate option, and
it is what a query that does not know its member in advance would need.

## Rows this table will grow

**The graph vector family LANDED.** It was named here as the one planned family
that would need a new keyspace and a new feature bit, and it took both: the
Vamana/DiskANN graph over quantised codes now ships as `vamana` (with `diskann`
as its second spelling), in keyspace `0x7D` behind feature bit `0x8000`
(`core/engine/src/index/vector/graph.rs`; `docs/core/FORMAT_V2.md` "Extension
boundary"). It is a third **declared** vector option beside `exact` and
`quantized`, declared for the reason they are: it is the same decision, at
another point on the curve. The row above and the family table beside it are
what it added; this paragraph is what is left of the plan.

One entry is still named here so the table above is understood as a shape that
extends, not a closed list.

**A JSON path, as an expression index.** The gap below is closable without a
new family. The scalar family already supports EXPRESSION indexes, behind
`EXPRESSION_FEATURE = 0x400`, which is how `CREATE INDEX i ON t (lower(col))`
stores the fold rather than the column. `IndexExpr` has exactly one variant
today, `Lower`. A second variant that extracts a JSON path would give:

```sql,proposed
CREATE INDEX people_status ON people ((payload->>'status'))
```

and a predicate over that same expression would be answered from it, which is
precisely how PostgreSQL indexes JSON. It reuses the scalar family, the scalar
keyspace and an already-shipped feature bit. What it needs is a descriptor that
can carry the path beside the variant byte, and a decision about whether an
older reader should refuse such a descriptor under its own additive bit. It
does not need a new index family, and it does not need a new keyspace.

A general inverted index over whole documents, the equivalent of PostgreSQL's
`jsonb_path_ops`, remains the larger and separate option. Path extraction
should come first, because it is cheaper and covers the common case: one field
inside a document that you filter on often.

**A graph vector family.** The `quantized` family is a bounded linear scan over
int8 codes: it reads every entry, which is correct and fast while a collection
is small and stops being either as it grows. A graph family, Vamana over
quantised codes, is the disk-first answer and would join `exact` and
`quantized` as a third **declared** vector option. It stays declared, because it
is the same decision the other two are, only with another point on the curve.
It is the one planned family that needs a new keyspace and a new feature bit.

**A JSON path, as an expression index. LANDED 2026-09-22.** It needed no new
family and no new keyspace, exactly as this section predicted. `IndexExpr`
gained a second variant, `JsonText`, beside `Lower`; the descriptor carries the
member name after the variant byte under a new encoding version, 4; and the
decision on the older reader was YES -- `JSON_EXPRESSION_FEATURE = 0x10000`
is taken, so a binary that predates it refuses such a file WHOLE at admission
rather than meeting the unknown descriptor version deeper in
(`docs/core/FORMAT_V2.md`, Extension boundary). What it reuses is the scalar
family, the scalar keyspace and the shipped `EXPRESSION_FEATURE = 0x400`
beside the new bit. The `JSONB` row of the table above and the section under
it now describe what exists; this row records that the growth happened.

The one thing still open here is a general inverted index over whole
documents, the equivalent of PostgreSQL's `jsonb_path_ops`: path extraction
came first because it is cheaper and covers the common case, one member inside
a document that you filter on often.

## What automatic costs, and how to refuse it

Every automatic index is maintained on every write. A table of twenty scalar
columns maintains twenty postings per inserted row, including for columns
nobody filters. That is the only real argument against this rule, and it is
answered by making the default overridable rather than by making every caller
opt in:

```sql
-- nothing automatic; declare by hand what you need
CREATE TABLE ix_events (event_id TEXT PRIMARY KEY, payload TEXT, seen_at TIMESTAMPTZ) WITH (index: none);
-- only this column
CREATE TABLE ix_audit (event_id TEXT PRIMARY KEY, payload TEXT, seen_at TIMESTAMPTZ) WITH (index: [seen_at]);
INSERT INTO ix_audit (_key, event_id, payload, seen_at) VALUES ('e1', 'e1', 'started', '2026-02-01T00:00:00Z');
-- and the range over the one indexed column answers
SELECT event_id FROM ix_audit WHERE seen_at >= '2026-01-01';
DROP TABLE ix_events
```

```sql refused
-- refused 0A000: `payload` was not named, so it has no index and the predicate is not demoted to a scan
SELECT event_id FROM ix_audit WHERE payload = 'x'
```

The escape is explicit and the default is helpful. Before this contract it was
the other way around: the default was unhelpful and there was no escape.

`index:` composes with the family keys of the `WITH (...)` clause
(`QL_CONTRACT` §2) rather than replacing them. An explicit `fulltext:`,
`spatial:` or `vector:` entry is honoured whatever `index:` says, because it
is a DECLARATION and this key governs only what happens without one:

```sql
-- the automatic btree over `body` is off; the declared gin is not
CREATE TABLE ix_post (body TEXT, n INT) WITH (index: none, fulltext: [body]);
INSERT INTO ix_post (_key, body, n) VALUES ('p1', 'kebun raya di kota bandung', 1);
SELECT _key FROM ix_post WHERE to_tsvector('simple', body) @@ to_tsquery('simple', 'kebun');
DROP TABLE ix_post
```

The key is STATEMENT-SCOPED and nothing about it is stored: no descriptor
field and no feature bit. A collection therefore carries no memory of having
opted out, and a later `ALTER TABLE ... ADD COLUMN` of an eligible kind
indexes that column whatever the `CREATE TABLE` said. Saying otherwise would
be a format change, and this contract makes none.

## Status

**Built.** Every row of the table above is what the engine does today.

- `CREATE TABLE` creates the automatic indexes IN THE SAME STATEMENT, so a
  predicate answers immediately after it and no `CREATE INDEX` is written
  (`lang/src/compile/ddl.rs::automatic_indexes` and `::automatic_index`).
  They are ordinary catalog descriptors built by the same
  `lang/src/compile/plan.rs::build_index` a hand-written `CREATE INDEX` runs.
- `ALTER TABLE ... ADD COLUMN` of an eligible kind creates one too, built over
  the rows already there. `RENAME COLUMN` drops the automatic index and
  re-earns it under the new name in the same statement; a HAND-WRITTEN index
  over the renamed column is still a refusal naming it.
- `WITH (index: none)` and `WITH (index: [column, …])` are the override, as an
  eighth key of the `WITH (...)` clause (`lang/src/parser/ddl.rs::WITH_KEYS`).
- One NOTICE lists what was created, under the names it created them under.
  Nothing is created that the caller was not told about.
- The generated name is the sugar's own `<table>_<column>_<family>`, so an
  automatic index and a declared one for the same column are ONE index. A
  hand-written `CREATE INDEX` whose descriptor matches an index already there
  -- same family, same field, same expression, same uniqueness -- answers with
  a NOTICE naming that index and creates nothing.
- No new family, no new keyspace tag and no feature bit. The scalar family
  already accepted exactly bool/int/real/text and the two spatial families
  already followed the declared shape; what changed is who gets one unasked.
- A table with more eligible columns than the 64-index ceiling a collection
  holds is REFUSED while it compiles, with the count, the ceiling and the
  escape written out -- not part-built and then refused by the engine half way
  down the column list.
- Tests: `lang/tests/sql_automatic_index.rs`.

**What it costs, measured.** A table of N `INT` columns, 20,000 inserts per
arm through one `$n`-parameterised statement, one commit per 1,000 rows, the
two arms differing only in `WITH (index: none)` (release build, macOS 26.6.2 on
Apple silicon, buffered I/O, `SyncMode::Full`):

| scalar columns | `index: none` | automatic | per index, per insert |
|---|---|---|---|
| 1 | 15.11 µs | 16.09 µs | 0.98 µs |
| 4 | 19.02 µs | 26.86 µs | 1.96 µs |
| 8 | 31.46 µs | 38.68 µs | 0.90 µs |
| 16 | 60.44 µs | 80.90 µs | 1.28 µs |
| 32 | 133.03 µs | 180.71 µs | 1.49 µs |

About **1.4 µs of write per index per row**, flat in the number of columns:
32 automatic indexes cost 47.7 µs of the 180.7 µs an insert into a 32-column
table takes, so a table wide enough for the cost to matter is a table wide
enough that the row itself dominates. That is the number the `index:` key
exists for; it is not a number that argues against the default.

**Still not built.** The graph vector family of the section above, unchanged,
and the general inverted index over whole documents. The `JSONB` member
expression index landed on 2026-09-22 and is DECLARED, never automatic: which
member you filter on is not something a `CREATE TABLE` can know.

The refusal for an unindexed predicate stays in every case. Nothing here
permits a silent scan; it changes only which indexes exist without being asked.
