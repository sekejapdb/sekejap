# The supportive structure (format, 0.19)

Status: **approved by the owner 2026-09-29 and frozen** after two independent
final reviews (both "freeze after fixes"; every fix applied here). It makes
`CONTRACT.md` "The shape -- the hyper contract" physical: section 1 (core) is
unchanged except where named; section 2 (supportive) becomes one Anchor and one
Register with every entry filed under its node 2.a-2.g. It replaces the
supportive metadata added release by release. Numbers marked *estimate* or
*target* are not measurements; the gate in section 5 measures them.

Paths are relative to the repository; `E/` = `core/engine/src/`,
`K/` = `core/kernel/src/`.

Scope: the released typed-collection format (`E/store/`, `E/collections/`,
`E/index/`). The kernel's older `Store` backend (`K/keys.rs`, `K/graph.rs`,
`K/text.rs`) is used only by two benchmark binaries; it is not a released
format and is not converted.

---

## 1. Core -- frozen

- **1.a Pages and log.** The 4096-byte page and header (`K/page.rs`; the tree
  id is a u16 stamp, so at most 65,531 live trees after the reserved ids),
  B-tree cells and overflow chains (`K/btree.rs`), the page-WAL frame, meta
  slot, physical feature word and physical capacity
  (`E/store/pagewal/format.rs`). Physical plumbing that stays here: the
  publication hint in `readers.lock` (rewritten at open, never converted) and
  the free-page links (their meaning is owned by 2.a).
- **1.b Rows.** Row keys, the dense row encoding whose header names its layout
  (`E/store/dense_v3.rs`), the key map, vector sidecars. Values past one page
  spill to overflow chains, within the record and transaction limits.
  **Reserved places** (no code, no test yet; unused, they cost nothing):
  - **layout physical-kind code 8 = BYTES** (SQL `BYTEA`, also `BLOB`),
    encoded exactly as TEXT (length-prefixed bytes). Codes 0-7 are TEXT, INT,
    REAL, BOOL, JSON, GEO, VECTOR, POINT. In binary JSON (row extras, edge
    bags) tag 8 already means object, so bytes there take **tag 9**.
  - **keyspace 0x61 = large-object chunks**, beside the vector sidecar 0x60.
    Only the namespace is reserved; the key grammar (owner first, per the
    keyspace invariant) and the row's object reference are a core extension
    to be approved when large objects are built.
- **1.c Edges.** Adjacency keys with the optional id segment; property bags
  (version byte 1 plus name-keyed binary JSON). The bytes never change; each
  stored name is a column's **stored token** (2.c), resolved to a column id
  once per statement.
- **1.d Index postings.** Scalar, text (postings, norms, term statistics,
  segments), trigram, vector, spatial, endpoint sets. Scalar postings decode
  one scalar then one sequence. **Compound keys (F4) and edge-subject postings
  (F5) need a new posting grammar: a core extension, to be approved before
  those features ship.**
- **1.e Moves out of core** (counters and declaration-sized records):

| Today | Moves to |
|---|---|
| Row sequence (three replicas per collection flush) | 2.b `NEXT` |
| Edge-id allocator | 2.b `NEXT` |
| Row-count record | 2.g `rCNT` |
| Text corpus totals; vector-graph header (entry, nodes) | 2.g `tCRP`, `vENT` |

---

## 2. Supportive

### 2.0 The carrier

**2.0.1 Integers and names.** Every integer is big-endian unless stated; an
**id** in a key is an order-preserving variable-length unsigned integer of up
to 64 bits (the encoding core keys already use). Names are UTF-8, compared as
exact bytes, at most 255 bytes.

**2.0.2 The Anchor** -- the one entry point.

- Where: the primary tree, keys `[0,0,copy]`, copies 0-2: today's header keys
  and framing (2081 bytes: magic, u16 length, payload, zero padding, CRC32C
  little-endian). A released 0.18 binary refuses an intact `E4COLL3` header as
  "newer than this binary" before it writes (proven with the released binary
  in section 6, step 2).
- Magic `E4COLL3\0`. Payload (at most 2067 bytes):

| Field | Bytes |
|---|---|
| Register format, u16 = 1 | 2 |
| root count, u8 = 3; then per copy: tree id u16, root page u32 | 1 + 18 |
| census count, u8 | 1 |
| census lines, sorted by (kind, version, variant), no duplicates, at most 220 | 9 each, at most 1,980 |
| reserved, zero | the rest |

- The resource policy is **not** in the Anchor: `LIMT` (2.a) is its only
  home. The physical capacity in 1.a must equal `LIMT`'s; disagreement is
  corruption.
- **Census line:** `kind 4 bytes | version u8 | variant u32`. The **variant**
  is the entry's discriminating field: `INDX` family, `JOBS` type, `COLM`
  type, `NAME` class, `NEXT` id class, `LAYT` highest physical kind; 0 for
  other kinds. The census only grows. An entry whose (kind, version, variant)
  is absent from the census is corruption.
- The Anchor is rewritten, in the same transaction as the change, when the
  census grows or a Register root moves.

**2.0.3 The Register** -- every other supportive fact.

- Where: three B-trees with **fixed tree ids 0xFFFD, 0xFFFE, 0xFFFF** (copies
  0-2), so no page holds two copies and repair can find the Register without
  the Anchor. Ignorable entries live in copy 0 only; each critical entry has
  one copy in each tree (overflow chains stay inside their tree).
- **Key:** `node u8 ('a'-'g') | owner class u8 | owner id | kind 4 bytes | item`.

| Owner class | 0 database (owner id 0) | 1 schema | 2 table | 3 index | 4 edge type | 5 graph | 6 job |
|---|---|---|---|---|---|---|---|

  The item is an id; for `LAYT` it is `layout id | part u8`; for `nIDX` it is
  `named class u8 | name bytes`. An entry's owner is the object it describes;
  its parent (for names) is a field.
- **Value:** `version u8 | payload length u32 | payload | crc32c`, the CRC
  (little-endian) over `key length u16 | key | version | payload length |
  payload`. Total value at most 8,128 bytes (payload at most 8,119); lengths
  are validated before any allocation.
- **Kind codes:** four ASCII letters, case-sensitive. **The first letter's
  case is the class and never changes; letters 2-4 are uppercase**, so no two
  kinds differ by case alone.

| First letter | Class | Copies | A reader that meets an intact entry it does not know |
|---|---|---|---|
| Upper (`COLM`) | critical | 3 | refuses the file by name, e.g. "needs a newer sekejap: COLM version 2" -- for an unknown kind, version or variant |
| Lower (`rCNT`) | ignorable | 1 | skips it |

- **Writers and unknown ignorable entries.** A writer that meets an ignorable
  entry of unknown kind, version or variant deletes it, in any node, before
  its first write to that entry's owner. Owners include dependents: a write
  to a table also owns the statistics of its indexes.
- **Damage.** A critical entry: damaged copies lose to intact ones;
  conflicting intact copies are corruption; an intact copy a reader does not
  support is never hidden by one it does. An ignorable entry that is damaged
  is treated as missing and rebuilt; it never refuses the file.
- **Versioning.** A shipped payload at a given version never changes. A writer
  uses the lowest version that expresses the content, so ordinary writes never
  raise the minimum reader.
- **Limits** -- enforced by **writers**, refused by name: entry value 8,128
  bytes; 1,600 live columns per table; 512 slots per `LAYT` part; 1,024
  indexes per owner; 64 running jobs; 64 registered kinds; 220 census lines.
  A reader enforces only the Anchor's 2067 bytes and its own memory bound, and
  answers "needs a newer sekejap" for an intact entry beyond these, so a later
  release can raise them. They bound metadata only; the column cap is a DDL
  check and costs a row nothing (gated at 50K rows, section 5). These are
  format caps; the raisable query caps of `cAPS` are a different thing (work
  and memory, with spilling past them per foundation D).
- **Publish.** One statement is one page-WAL transaction holding every entry
  it changes, the `nIDX` entries they imply, and the Anchor when the census
  grows or a root moves. Heavy work is a 2.f job.
- **Registry.** One table in code lists every kind, version and variant; this
  document mirrors it and a test compares the two. A new kind, version or
  variant needs a dated owner decision (class, node, size and count bounds)
  and ships with a fixture from the release binary and the previous release's
  refuse-or-skip test. Forbidden: changing a shipped payload, reusing a code,
  a new metadata key tag, a new feature bit, a new tail.

### 2.a Paging and space

| Kind | Class | Fields | Replaces |
|---|---|---|---|
| `TREE` | critical | tree id, owner, root page, state (live, freeing, free) | index tree tails; tree ids tied to index ids |
| `LIMT` | critical | the 56-byte resource policy | the policy block in the header |
| `cAPS` | ignorable | cap id, raised value | nothing (new: raisable caps, N10) |

**Tree ids are storage slots, not identities** (identities are never reused;
slots may be). A slot is reassigned only when its `TREE` is `free`: every page
reclaimed, no snapshot or recovery reference, cached tree hints invalidated.
Ids 0, 1 and 0xFFFD-0xFFFF are reserved. With no free slot, index creation is
refused by name. An index tree is rebuilt from rows, never salvaged by a page
scan (a freed page keeps its old stamp).

### 2.b Identity and names

| Kind | Class | Fields | Replaces |
|---|---|---|---|
| `NEXT` | critical | id class, scope, next id | header counters, graph counters, row sequence, edge-id allocator |
| `NAME` | critical | class (schema, table, column, index, edge type, context, graph), id, parent id, name, state (live, dead) | catalog names, schema tails and records, index names, the edge-type and context dictionary |
| `nIDX` | ignorable | keyed (named class, name) under the parent -> id; rebuilt from `NAME` | name keyspaces `0x10`, `0x11`, `0x12` |

Not stored (derived): live counts of indexes, edge types and contexts; the
per-table index list (the Register's key order lists it). Dropped: the lookup
from an index id alone to its table (every reference names its owner) and the
4,096-name dictionary cap.

### 2.c Schema

| Kind | Class | Fields | Replaces |
|---|---|---|---|
| `TABL` | critical | role (rows, edge table), timestamps, current layout, generation | the catalog record's fixed part and flags |
| `COLM` | critical | column id, stored token, type and dimension, declared spelling, NOT NULL, default for missing values, default for new writes, references, state (live, dead, shadow), cast-from | declared types, column rules and constant defaults, edge-table references |
| `LAYT` | critical | per part (item `layout id \| part`): up to 512 slots of (column id, physical kind, vector dimension); parts numbered from 0, contiguous, all present | the layout descriptor and its 256-column limit |
| `KEYS` | critical | key column ids, generator | key specs, edge-table keys |

- The hidden columns `__e4_key`, `_created_unix`, `_updated_unix` get
  reserved column ids.
- A historical layout keeps its physical vector dimension in its slots,
  whatever a later `COLM` says.
- Conversion of 0.18 defaults: a legacy constant default becomes the
  **default for new writes** only; the default for missing values stays
  empty, because 0.18 never applied a default to rows already written.

### 2.d Access paths

| Kind | Class | Fields | Replaces |
|---|---|---|---|
| `INDX` | critical | index id, owner (table or edge type), role (user, automatic, membership, locator, endpoint set), family, posting encoding, 1-16 column ids each with an optional expression, unique, family parameters, state (building, ready, shadow; for endpoint sets, complete) | index descriptor versions 1-4 and their per-family option bytes, the endpoint-set flag, the 64-index cap |

Family parameters are fields: text analyzer (trigram = analyzer 2), Unicode,
BM25 and segment versions; vector dimension, quantizer, graph version, degree,
alpha, **build search-list size**; spatial grid, CRS, metric, levels, **cell
budget**.

### 2.e Graph

| Kind | Class | Fields | Replaces |
|---|---|---|---|
| `GRPH` | critical | graph id, schema id; the base graph's encoding and reverse-required flags | graph header flags |
| `BIND` | critical | edge table id, source column id, destination column id, edge type id, **declaring graph id (0 once that graph is dropped)** | the edge-table binding tail |
| `MEMB` | critical | graph id, table id, element name, vertex or edge, labels | the memberships tail; the upgrader synthesizes memberships for graphs that were implicit in 0.18 |

### 2.f Jobs

| Kind | Class | Fields | Replaces |
|---|---|---|---|
| `JOBS` | critical | job id, type, target, phase, cursor, counters, shadow object, action list, drop mode | the drop tail and scan, index Building/Dropping states and cursors, REINDEX temporary and retired names |

Types, version 1: index build, index drop, table drop, swap, conversion,
validation, backfill, reshape, tree free, upgrade cleanup. Every job builds
beside what is served, publishes in one commit, cleans up in bounded steps (at
most 256 per commit), resumes after a crash, and verifies before removing
(Law 3).

### 2.g Statistics

| Kind | Class | Fields | When missing, unknown or damaged |
|---|---|---|---|
| `rCNT` | ignorable | rows, generation | `count(*)` walks the rows; a backfill job rebuilds the record |
| `tCRP` | ignorable | documents, tokens | recounted exactly from the norms **before the first ranked query** (BM25 depends on it), then stored |
| `vENT` | ignorable | entry, nodes, entry-at | rebuilt before the first search or insert on that index (an absent header means an empty graph today) |

### 2.h The 24 legacy feature bits

| Bit | Legacy feature | Now |
|---|---|---|
| 0x1 | typed indexes exist | the Register itself |
| 0x2 | graph | census `GRPH` |
| 0x4, 0x20, 0x8000 | vector exact, quantized, Vamana | census `INDX` variants |
| 0x8, 0x100 | spatial point, geometry | census `INDX` variants |
| 0x10, 0x800000 | text, trigram | census `INDX` variant; analyzer field |
| 0x40 | packed segments (tombstone admission) | `INDX` text segment version |
| 0x80 | index trees | census `TREE` |
| 0x200 | drop | `JOBS` table-drop type |
| 0x400, 0x10000 | expressions, JSON expressions | `INDX` version |
| 0x800 | declared types | `COLM` |
| 0x1000, 0x200000 | column rules, constant defaults | `COLM` version |
| 0x2000 | row counts | `rCNT` |
| 0x4000 | endpoint sets (completeness) | `INDX` endpoint-set role and state |
| 0x20000 | schemas | `NAME` schema class |
| 0x40000 | edge ids | `NEXT` edge-id class |
| 0x80000 | edge tables | `BIND` |
| 0x100000 | key specs | `KEYS` |
| 0x400000 | property graphs | `GRPH`, `MEMB` |

### 2.i Entry payloads and codes, version 1

Integers are big-endian. Codes below are census variants and field values;
each is frozen once written by a released binary.

**`NEXT` id classes** (the item and the census variant): 1 table, 2 layout,
3 index, 4 row sequence, 5 edge id, 6 edge type, 7 context, 8 graph,
9 schema, 10 job, 11 column. Payload: next id, u64. The database counters
(classes 1-3) are owned by the database (owner class 0, id 0), node `b`.

**`LIMT`**: node `a`, owner the database, item 0. Payload: the kernel's
56-byte policy record, unchanged.

**`INDX` variants** (the index family): 0 scalar, 1 text (words), 2 text in
packed segments, 3 trigram, 4 exact vector, 5 quantized vector, 6 Vamana
graph, 7 spatial point, 8 geometry, 9 endpoint set. `INDX` versions: 1 plain
columns, 2 with a `lower` expression, 3 with a JSON member expression.

**`JOBS` types** (the variant): 1 index build, 2 index drop, 3 table drop,
4 swap, 5 conversion, 6 validation, 7 backfill, 8 reshape, 9 tree free,
10 upgrade cleanup.

**`NAME` classes** (the variant, and the class byte of an `nIDX` item):
1 schema, 2 table, 3 column, 4 index, 5 edge type, 6 context, 7 graph.

**`COLM` variants**: 0 a column; 1 declared spellings in use; 2 column rules
in use; 3 constant defaults in use; 8 reserved for BYTES. Every entry is
written as 1/0; lines 1/1-1/3 are the file's feature lines.

**Payloads, version 1** (strings are a u8 length and UTF-8 bytes):

| Kind | Key (node, owner, item) | Payload |
|---|---|---|
| `NAME` | `b`, the object (schema or table), 0 | parent id u64 (a table's schema, 0 for `public`), state u8 (0 live), name |
| `nIDX` | `b`, the parent (database for a schema, schema for a table), (class, name) | id u64 |
| `TABL` | `c`, the table, 0 | flags u8 (bit 0 timestamps), current layout u32, schema id u64 |
| `COLM` | `c`, the table, column id | state u8 (0 live, 1 retired), name, stored token, declared spelling (empty: none), rule length u16 + the 0.18 column-rule encoding |
| `LAYT` | `c`, the database, (layout id, part) | table u32, slot count u16 (at most 512), per slot: column id u64, type u8, vector dimension u32 |
| `KEYS` | `c`, the table, 0 | the 0.18 key-declaration encoding |
| `BIND` | `e`, the table, 0 | the 0.18 edge-table record encoding |
| `MEMB` | `e`, the table, 0 | the 0.18 membership encoding |
| `JOBS` | `f`, the table, job type (3: table drop) | phase u8, mode u8, removed u64 |
| `NEXT` | `b`, the owner, id class | next id u64 (columns: per table, from 16) |

**Row ids and edge ids are reserved in blocks of 1,024.** A table's row
`NEXT` (class 4) and the edge-id `NEXT` (class 5) hold a bound: every id
below it may have been handed out. A writer raises the bound one block at a
time, so a commit inside a block writes no counter. An id is never handed out
twice; a crash or a rollback can leave a gap, as a PostgreSQL sequence with a
cache does. NAMED COST: the commit that opens a block writes the entry's
three copies in three trees, two pages more than the 0.18 counter.

Column ids 1, 2 and 3 are the managed columns `__e4_key`, `_created_unix`
and `_updated_unix`; user columns start at 16. A layout slot names a column
id, and reading resolves it to the column's current name, so a rename is one
`COLM` write whatever the number of rows or layouts. The column's type lives
in the slots: a historical layout keeps the type its rows were written with.

**The legacy feature word.** Each bit of 2.h is exactly one census line:

| Bit | Line | Bit | Line |
|---|---|---|---|
| 0x2 | `GRPH` 1/0 | 0x2000 | `rCNT` 1/0 |
| 0x4 | `INDX` 1/4 | 0x4000 | `INDX` 1/9 |
| 0x8 | `INDX` 1/7 | 0x8000 | `INDX` 1/6 |
| 0x10 | `INDX` 1/1 | 0x10000 | `INDX` 3/0 |
| 0x20 | `INDX` 1/5 | 0x20000 | `NAME` 1/1 |
| 0x40 | `INDX` 1/2 | 0x40000 | `NEXT` 1/5 |
| 0x80 | `TREE` 1/0 | 0x80000 | `BIND` 1/0 |
| 0x100 | `INDX` 1/8 | 0x100000 | `KEYS` 1/0 |
| 0x200 | `JOBS` 1/3 | 0x200000 | `COLM` 1/3 |
| 0x400 | `INDX` 2/0 | 0x400000 | `MEMB` 1/0 |
| 0x800 | `COLM` 1/1 | 0x800000 | `INDX` 1/3 |
| 0x1000 | `COLM` 1/2 | | |

Bit 0x1 is the `NEXT` index-class line. The engine's feature word is derived
from the census, so a bit is never cleared once its line is written (the
census only grows); the index count is derived from the registry.

---

## 3. The move -- `sekejap-upgrade`, 0.18 to 0.19

A library function (embedded users have no command line); `sekejap-upgrade`
wraps it. 0.19 refuses to open an unconverted 0.18 file, naming the command,
before touching a byte (Law 8 baseline). The upgrader carries the 0.18 code
isolated inside it, to read the old catalog and to finish work already in
progress; nothing else in 0.19 uses it.

**3.a Build beside, then publish by rename** (implemented 2026-09-29; it
replaces the in-place marker first written here). The move is
`collections::upgrade::upgrade_format`, a rebuild into a Register file
(`collections::rebuild::upgrade_to_register`):

1. `<db>.v019-upgrading` is built from `<db>`, which is read and never
   written: rows, vector sidecars and edges copied byte for byte, every
   supportive fact translated into Register entries (each 0.18 layout gets
   its table from the table's current layout or from its rows; a layout no
   table and no row uses is not carried), every index rebuilt from the rows.
2. The build is compared with the source (rows, sidecars and edges equal,
   key for key) and verified independently, then marked complete.
3. Publication is two renames: `<db>` to `<db>.v018-backup`, then
   `<db>.v019-upgrading` to `<db>`. The original directory is the backup;
   it costs no copy.

The source must have every index READY and no drop in flight, as a rebuild
requires; a 0.18 release finishes that work first.

**3.b Cost.** One full read of the source, one write of the new file (rows,
edges and sidecars; indexes rebuilt), disk for both until the backup is
removed. The translation of the metadata itself is small.

**3.c Crash states**

| State | Recognised by | Finished by |
|---|---|---|
| Legacy | `<db>` is 0.18, no build beside it | 0.19 refuses it until upgraded |
| Building | `<db>` is 0.18, `<db>.v019-upgrading` holds the rebuild's incomplete marker | the next call deletes the partial build and starts again |
| Between the renames | no `<db>`, `<db>.v018-backup`, a complete `<db>.v019-upgrading` | the next call finishes the second rename |
| Done | `<db>` is a Register file | nothing |

**3.d Verified before publishing**

1. The Register decoded by the new code gives the same logical schema as the
   0.18 decoder, field by field: names, types, defaults, constraints, keys,
   graph memberships, indexes and their parameters, job states.
2. Every critical entry has three agreeing copies with good checksums.
3. `nIDX` rebuilt from `NAME` equals what was written.
4. Every `NEXT` is at least the legacy counter; `rCNT` equals the legacy one.
5. **Full pass, on by default** (owner decision): every row, edge and posting
   is interpreted through both catalogs, and the two interpretations agree.
   `--no-verify` skips only this step.
6. The verifier passes on a read-only open through the new roots.

**3.e Cost on 1M rows and 3M edges** (10 tables, 100 columns, 20 indexes;
*estimates*): rows, edges and postings rewritten by the translation: 0;
Register: about 100 KiB, built in about a second; legacy records freed: about
360 KiB; backup: one copy of the file; full verification: one read of the
file.

---

## 4. Adding something new

The checklist is in `CONTRACT.md` ("Adding something new"). Example, a stored
generated column `total = price * qty`: declaration-sized, so supportive; the
shape of a table, so 2.c; `COLM` version 2 adds "generated from", written only
for generated columns; a writer that skipped it would store stale totals, so
critical -- an older release answers "needs a newer sekejap: COLM version 2";
expression at most 1,024 bytes and 16 column ids; backfill is the conversion
job. No new kind, no new node.

---

## 5. Performance guard (targets)

| Part | Guard | Target against 0.18.5 |
|---|---|---|
| Foundation A (ids) | Row and edge bytes unchanged; a column id and a stored token resolved once per statement and layout; a 16-layout cache | Bytes per row and edge: 0. Point read, scan, filtered scan, edge write, graph walk: at most +5% wall time |
| Foundation C (jobs) | Steps of at most 256 rows between foreground commits | Insert wall time once ready: at most +5%. During a build: p99 reported |
| Carrier | Register read on a cache miss only | Pages written per insert commit: at most today's. Reopen page accesses: at most today's |

The gate: the fixed measurement set at 50K then 1M rows on one device,
including narrow tables at 50K (the 1,600-column cap must cost nothing), plus
reopen at 1, 100 and 10,000 tables and WAL bytes per DDL commit. Every metric
and operation at most 1.05 of 0.18.5, never averaged. Arms: an upgraded 0.18.5
file and a new file, tables never altered (row, edge and posting keys and
values byte-identical to 0.18.5); an altered arm, reported.

---

## 6. Build order inside 0.19

Each step is measured against its completed prerequisites. No release binary
writes a Register file before the last step.

1. Baseline: fixtures written by the released 0.18.5 binary (checkpointed and
   mid-log, with an edge table and an altered table); the benchmark harness at
   50K and 1M.
2. The carrier (2.0: Anchor, Register, census, framing, fixed tree ids) and
   its verifier, behind a create switch; the released 0.18.5 binary's refusal
   of an `E4COLL3` file tested here. New-file arm only; carrier operations
   measured. The refusal test is
   `supportive::anchor_tests::the_released_0185_binary_refuses_an_anchored_file`:
   it runs when `SEKEJAP_0185_OPEN_CHECK` names `core/engine/examples/open_check`
   built from the v0.18.5 tag (copied into a worktree at the tag, as the
   release fixtures are), and is skipped otherwise.
3. The entry kinds of 2.a-2.g, including `LAYT` parts, replacing their legacy
   readers and writers on Register files; code modules mirror the nodes.
   New-file arm; DDL WAL bytes and reopen measured from here.
4. The upgrader with the isolated 0.18 code, its marker and crash states, the
   0.18.5-fixture conversion tests. Upgraded arm measured.
5. The 0.19 versatility items on Register files, each measured against
   step 4: F1 on rows, F1 on edges, F2, N1, N6, N7.
6. The release: new files default to the Register, the upgrader is enabled,
   the gate runs on every arm.

Owner decision 2026-09-29: **the whole supportive structure ships enabled in
0.19** (steps 1-6); 0.19 is the versatility foundation. The remaining
flexibility (F3, F4, F5, F6, F7, N3, N10, N13 and the items needing no
format) is 0.20, as new entry versions on this format; F4 and F5 wait for
their posting grammar (1.d). Search (the text skip table, field weights and
fusion, analyzers, snippets, facets) is 0.21. The trigram index is in 0.19.

## 7. Owner decisions

- 2026-09-29: the design approved and frozen; four-letter kind codes, the
  first letter's case the class.
- 2026-09-29: verify on by default in the upgrader (`--no-verify` to skip).
- 2026-09-29: 1,600 columns per table; the cap must cost nothing at small
  scale.
- 2026-09-29: BYTES (layout kind 8, binary-JSON tag 9) and large objects
  (keyspace 0x61) reserved; built later.
- To approve later, as core extensions: the compound posting grammar (F4),
  edge-subject postings (F5), the large-object key grammar.
