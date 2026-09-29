# The supportive structure (design, 0.19)

Status: **design, for the owner's approval before any code.** It makes
`CONTRACT.md` "The shape -- the hyper contract" physical: section 1 (core) is
unchanged; section 2 (supportive) becomes one Anchor and one Register, with
every entry filed under its node 2.a-2.g. It replaces every piece of
supportive metadata added release by release. Drawn from two independent
reviews (four rounds each) of the 0.18.5 and 0.19 code; where they differed,
the choice and the reason are stated.

Paths are relative to the repository; `E/` = `core/engine/src/`,
`K/` = `core/kernel/src/`.

---

## 1. Core -- unchanged, frozen

- **1.a Pages and log.** The 4096-byte page and its header (`K/page.rs`), B-tree
  cells and overflow chains (`K/btree.rs`), the page-WAL frame and meta slot
  (`E/store/pagewal/`). Two things that look supportive stay here because
  they are physical plumbing: the publication hint in `readers.lock` (live
  readers depend on it; rewritten at open, never converted) and the free-page
  links (their meaning is owned by 2.a; no second free list).
- **1.b Rows.** Row keys, the dense row encoding whose header names its layout
  (`E/store/dense_v3.rs`), the key map, vector sidecars.
  **Reserved places** (owner decision 2026-09-29: the place is fixed now, the
  algorithm and tests come later; nothing reads or writes them yet, so they
  cost nothing):
  - value kind **8 = BYTES** (SQL `BYTEA`, also spelled `BLOB`): a binary
    value, encoded exactly as TEXT is (length-prefixed bytes, spilling to
    overflow pages), so a table without such a column is unaffected. Kinds 0-7
    are TEXT, INT, REAL, BOOL, JSON, GEO, VECTOR, POINT.
  - keyspace **0x61 = large-object chunks**, beside the vector sidecar 0x60:
    key `0x61 | collection | object id | chunk number`, obeying the keyspace
    invariant (the key begins with its owner). For values past one
    transaction's log allowance, streamed in pieces, as PostgreSQL's large
    objects are; the row holds the object id. Its id counter is a 2.b `NEXT`
    class and its metadata a 2.c `COLM` type when it is built.
- **1.c Edges.** Adjacency keys with the optional id segment, and property
  bags (`E/index/graph/mod.rs`). The bytes stay; what the names inside a bag
  mean moves to 2.c (a column's stored token).
- **1.d Index postings.** Scalar, text (postings, norms, term statistics,
  segments), trigram, vector, spatial, endpoint sets. Multi-column keys and
  edge-property postings are compositions of the existing self-delimiting
  encodings under the existing posting shape: no new core encoding.
- **1.e Moves out of core.** Four records grow with declarations or are
  counters, not data, and move to supportive:

| Today | Moves to |
|---|---|
| Row sequence (written as three 2081-byte records per insert commit) | 2.b `NEXT` |
| Edge-id allocator | 2.b `NEXT` |
| Row-count record | 2.g `rCNT` |
| Text corpus totals; vector-graph entry point | 2.g `tCRP`, `vENT` |

Side effect: an insert commit writes three small `NEXT` entries instead of
three 2081-byte catalog records.

---

## 2. Supportive

### 2.0 The carrier

**2.0.1 The Anchor** -- the one entry point.

- Where: the primary tree, keys `[0,0,copy]` for copies 0-2 -- today's catalog
  header keys and framing (2081 bytes: magic, u16 length, zero padding,
  CRC32C). Reusing them is what makes a released 0.18 binary refuse a 0.19
  file cleanly ("header version is newer") before it writes anything.
- Magic: `E4COLL3\0`.
- Payload (at most 2067 bytes): Register format (u16, value 1); the three
  Register roots (tree id u16, root page u32, one per copy); the **census** --
  one line per (kind, version, variant) the file uses, 9 bytes each, at most
  128 lines; the 56-byte resource policy when present.
- Rewritten only when the census grows, a Register root moves, or the policy
  changes. The census replaces all 24 feature bits.

**2.0.2 The Register** -- every other supportive fact.

- Where: three B-trees, one per copy, so no page ever holds two copies of an
  entry. Their tree ids are chosen by the upgrader or at create.
- **Key, node first** (rule 7 of the contract: a node's entries sit together):
  `node (a-g) | owner class | owner id | kind (4 ASCII letters) | item`.
  The item is an id, or the name bytes (at most 255) for the name index.
- **Value:** `version u8 | length u32 | payload | crc32c(key + value)`. The
  checksum covers the key, so a misplaced entry is detected.
- **Class by the kind's letter case, as PNG does** -- it can never drift:

| First letter | Class | Copies | A reader that does not know the kind |
|---|---|---|---|
| Upper (`COLM`) | critical | 3 | refuses the file by name: "needs a newer sekejap: entry kind COLM" |
| Lower (`rCNT`) | ignorable | 1 | skips it; a writer deletes an unknown 2.g entry before its first write to that entry's owner |

  Two classes only (one review proposed a third, write-critical class; the
  stale-statistic case it covered is handled by the deletion rule, which the
  contract adopted). A failed checksum is corruption; an intact unknown kind is
  "needs a newer sekejap", never corruption. The decision is made from the
  census at admission, before recovery or any write.
- **Versioning.** A shipped payload at a given version never changes. A writer
  uses the lowest version that can express the content, so ordinary writes
  never raise the minimum reader.
- **Limits** (checked at write, refused by name): entry value 8,128 bytes;
  1,600 live columns per table (PostgreSQL's limit); 1,024 indexes per owner;
  64 running jobs; 64 kinds; 128 census lines. These bound METADATA only: the
  8,128 bytes are one supportive entry (a column's definition, an index's
  parameters), never a row value -- row values of any size live in core (1.b)
  and spill to overflow chains past one page. The column cap is a check at
  DDL; a row costs what its table's own columns cost, never the cap
  (gated at 50K rows on narrow tables, section 5).
- **Publish rule.** One statement is one page-WAL transaction holding every
  entry it changes, their name-index entries and the Anchor if the census
  grew. A heavy change is a 2.f job (below).
- **Registry rule.** One table in code lists every kind; this document mirrors
  it and a test compares the two. A new kind or version needs a dated owner
  decision (class, node, size and count bounds) and ships with a fixture from
  the release binary and the previous release's refuse-or-skip test.
  Forbidden: changing a shipped payload, reusing a kind code, a new metadata
  key tag, a new feature bit, a new tail.

### 2.a Paging and space

| Kind | Class | Fields | Replaces |
|---|---|---|---|
| `TREE` | critical | tree id, owner, root page, state (live, freeing) | index tree tails; tree ids tied to index ids (now reusable once freed) |
| `LIMT` | critical | the 56-byte resource policy | the policy block in the catalog header |
| `cAPS` | ignorable | cap id, raised value | nothing (new: raisable caps, N10) |

### 2.b Identity and names

| Kind | Class | Fields | Replaces |
|---|---|---|---|
| `NEXT` | critical | id class, scope, next id | header counters, graph counters, row sequence, edge-id allocator |
| `NAME` | critical | class (schema, table, column, index, edge type, context, graph), id, parent id, name (<=255 bytes), state (live, dead) | catalog names, schema tails and records, index names, the edge-type and context dictionary |
| `nAME` | ignorable | (parent, class, name) -> id; rebuilt from `NAME` | the name-lookup keyspaces `0x10`, `0x11`, `0x12` |

Removed without replacement: the index registry and per-table index list
(Register key order lists them), and the 4,096-name dictionary cap.

### 2.c Schema

| Kind | Class | Fields | Replaces |
|---|---|---|---|
| `TABL` | critical | role (rows, edge table), timestamps, current layout, generation | the catalog record's fixed part and flags |
| `COLM` | critical | column id, stored token, type and dimension, declared spelling, NOT NULL, default for missing values, default for new writes, references, state (live, dead, shadow), cast-from | declared types, column rules and constant defaults, edge-table references |
| `LAYT` | critical | layout id, owner table, slots (column id, physical kind) | the layout descriptor and its 256-column limit |
| `KEYS` | critical | key column ids, generator | key specs, edge-table keys |

### 2.d Access paths

| Kind | Class | Fields | Replaces |
|---|---|---|---|
| `INDX` | critical | index id, owner (table or edge type), role (user, automatic, membership, locator, endpoint set), family, posting encoding, 1-16 column ids each with an optional expression, unique, family parameters, state (ready, shadow) | index descriptor versions 1-4, the endpoint-set flag, the 64-index cap |

Family parameters become fields instead of compiled constants: text analyzer
(trigram is analyzer 2), Unicode, BM25 and segment versions; vector dimension,
quantizer, graph version, degree, alpha; spatial grid, CRS, metric, levels.

### 2.e Graph

| Kind | Class | Fields | Replaces |
|---|---|---|---|
| `GRPH` | critical | graph id, schema id; the base graph's encoding and reverse-required flags | the graph header flags |
| `BIND` | critical | edge table id, source column id, destination column id, edge type id | the edge-table binding tail |
| `MEMB` | critical | graph id, table id, element name, vertex or edge, labels | the property-graph memberships tail |

### 2.f Jobs

| Kind | Class | Fields | Replaces |
|---|---|---|---|
| `JOBS` | critical | job id, type, target, phase, cursor, counters, shadow object, action list | the drop tail and scan, index Building/Dropping states and cursors, REINDEX temporary and retired names |

Job types, version 1: index build, index drop, table drop, swap, conversion,
validation, backfill, reshape, tree free, upgrade cleanup. Every job: build
beside what is served, publish in one commit, clean up in bounded steps (at
most 256 per commit), resume after a crash, verify before removing (Law 3).

### 2.g Statistics

| Kind | Class | Fields | If missing |
|---|---|---|---|
| `rCNT` | ignorable | rows, generation | `count(*)` walks; a backfill job rebuilds it |
| `tCRP` | ignorable | documents, tokens | recounted from norms, as a job |
| `vENT` | ignorable | entry, nodes, entry-at | the walk starts at the first node, then refreshes |

---

## 3. The move -- `sekejap-upgrade`, 0.18 to 0.19

A library function (embedded users have no command line); `sekejap-upgrade`
wraps it. 0.19 refuses to open an unconverted 0.18 file, naming the command,
before touching a byte (CONTRACT.md, Law 8 baseline). The 0.18 decoders are
kept frozen, read-only, for conversion only.

**3.a Steps**

1. Check, read-only: inventory every metadata family, layout, counter,
   unfinished job; unknown source semantics stop here.
2. Take the writer, checkpoint, back up (refused if the backup path exists).
3. Finish legacy work with legacy code: tables dropping, indexes building or
   dropping, REINDEX leftovers. The translation then sees only ready objects.
4. Build beside: the three Register trees, written in bounded commits; a
   marker entry records the cursor and the transaction number of its commit.
   The legacy header is untouched.
5. Verify (3.c).
6. Publish in one commit: the Anchor replaces `[0,0,copy]`, the marker goes,
   an upgrade-cleanup `JOBS` entry appears.
7. Clean up in steps of at most 256 deletes: the legacy metadata records,
   name keyspaces, per-index corpus and vector-graph header records.

The catalog moves in one pass, never lazily (a half-moved catalog would put
two decoders on the hot path). **No row, edge or posting is rewritten.**

**3.b Crash states**

| State | Recognised by | Finished by |
|---|---|---|
| Legacy | `E4COLL1`/`2`, no marker | nothing (0.19 refuses it until upgraded) |
| Building | legacy header plus marker | marker transaction equals the file's: resume at the cursor; otherwise someone wrote since: free the trees and restart |
| Published | `E4COLL3` plus the cleanup job | resume the deletes at the cursor |
| Clean | `E4COLL3`, no cleanup job | done |

**3.c Verified before any old record is removed (Law 3)**

1. The Register decoded by the new code equals the legacy decoder's schema,
   field by field.
2. Every critical entry has three agreeing copies with good checksums.
3. `nAME` rebuilt from `NAME` equals what was written.
4. Every `NEXT` is at least the legacy counter; `rCNT` equals the legacy one.
5. For each layout one row decodes identically; for each index the first
   posting resolves.
6. The verifier passes on a read-only open through the new roots.

**Verify is on by default** (owner decision 2026-09-29): after the catalog
checks, a full streaming pass reads every row, edge and posting and confirms
each decodes under the new catalog before any old record is removed. It costs
one read of the file; a database is upgraded once in its life. `--no-verify`
skips it for a caller who accepts the catalog checks alone.

**3.d Cost on 1M rows and 3M edges** (10 tables, 100 columns, 20 indexes)

| Item | Cost |
|---|---|
| Rows, edges, postings rewritten | 0 bytes |
| Register built | about 100 KiB; O(declarations), under a second |
| Legacy records freed | about 360 KiB, returned to the free list |
| Backup | one copy of the database; dominates |
| Verify (default on) | one streaming pass over the file |

---

## 4. Adding something new

The checklist is in `CONTRACT.md` ("Adding something new"). Worked example, a
stored generated column `total = price * qty`: it grows with declarations
(supportive); it is the shape of a table (2.c); `COLM` version 2 adds
"generated from", written only for generated columns; a writer that skipped
it would store stale totals, so critical -- an older release answers "needs a
newer sekejap: COLM version 2"; expression at most 1,024 bytes and 16 column
ids; backfill is the existing conversion job. No new kind, no new node.

---

## 5. Performance guard

| Foundation | Guard | Expected against 0.18.5 |
|---|---|---|
| A. ids | Row bytes unchanged (rows already name their layout); a column id resolved once per statement and layout; a 16-layout cache; id-keyed edge bags only after an edge table's first ALTER | Bytes per row and edge: 0. Point read, scan, filtered scan (faster: no per-row name comparison), edge write, graph walk: at most +5% wall time |
| C. jobs | Steps of at most 256 rows between foreground commits | Insert wall time once ready: 0%. During a build: p99 reported |
| Carrier | Register read on a cache miss only; `NEXT` and statistics share a leaf | Pages written per insert commit: at most today's. Reopen page accesses: at most today's |

The gate: the fixed measurement set at 50K then 1M on one device -- including
narrow tables at 50K, so the 1,600-column cap is shown to cost nothing at small
scale -- plus reopen
at 1, 100 and 10,000 tables and WAL bytes per DDL commit. Every metric and
operation at most 1.05 of 0.18.5, never averaged. Two arms: an upgraded 0.18.5
file and a new file, both with tables never altered (row, edge and posting
keys and values byte-identical to 0.18.5); a third, altered arm is reported.

---

## 6. Build order inside 0.19

Each step measured alone; no release binary writes a Register file before the
last step.

1. Baseline: fixtures written by the released 0.18.5 binary; the benchmark
   harness at 50K and 1M.
2. The carrier (2.0) and its verifier, behind a create switch; reopen, DDL WAL
   bytes and file bytes measured.
3. The entry kinds of 2.a-2.g replacing their legacy readers and writers on
   Register files; code modules mirror the nodes.
4. Freeze the 0.18 decoders read-only; the upgrader, its crash states, the
   0.18.5-fixture conversion tests and the 0.18.5 binary's refusal test.
5. The flexibility items on Register files, each measured alone: F8, F1 rows,
   F2, F4, F1 edges, F5, F6, F3, F7, N13, N6, N7.
6. The release: new files default to the Register, the upgrader is enabled,
   the gate runs on every arm.

## 7. Owner decisions

- 2026-09-29: verify on by default in the upgrader (`--no-verify` to skip).
- 2026-09-29: 1,600 columns per table; the other limits as written; the cap
  must cost nothing at small scale.
- Open: approval of this document as the frozen format, and the kind-code
  notation (four letters, PNG-style: the first letter's case is the class).
