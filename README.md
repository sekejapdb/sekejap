# sekejap-e4 — the compact-entity engine

**One line:** sekejap at SQLite storage density — entities stored as typed,
schema-keyed records on a disk-first B-tree kernel, with the graph / vector /
spatial / text multi-model and the 7 laws.

**e4 is pure.** It stores **no JSON text, ever.** JSON is only a wire format at
the API boundary (input parsing, output rendering). On disk, a row is always a
typed positional record. There is **no legacy payload, no mixed-format reader,
no JSON fallback, and no migration path from e3's on-disk data.** e4 is seeded
from e3's *code*, not its *data*. A clean engine.

**Live work plan:** [sekejap-e4 in tracker](http://127.0.0.1:5156/ui/?j=sekejap-e4#tree)
([Todo board](http://127.0.0.1:5156/ui/?j=sekejap-e4#todo)). Read the
[agent-readable journey](http://127.0.0.1:5156/api/journeys/sekejap-e4) before
starting work and keep task status/evidence there. The existing `sekejap`
journey tracks E3. See [AGENTS.md](AGENTS.md) for the working convention.

This README is the full brief for a fresh engineer (human or Astra) with **no
prior context.** Read it top to bottom before touching code.

---

## 0. Why e4 exists — the finding that forced it

**Bounded redistribution loop (2026-09-14): rejected on correctness.** The
final policy completes the previously refused transactions and improves Pi
400K / 1.92M-mutation medians to **87.386 s**, versus retained E4 **105.241 s**
and SQLite **112.014 s**. It saves 6.67% final space. However, the shared Store
returns an incorrect 800K scan; a matching-feature reproduction preserves
800,000 rows with two ordering violations. Reopening a copy recovers all
800,000 point lookups, but changes the data file, so the cause remains open.
All runtime changes are reverted. A transaction-capacity regression, stricter
ordering oracle and matching lean-test features are retained. See the
[complete comparison and failure evidence](docs/PAIR_PACKING.md).

**Scattered-packing ablation (2026-09-14): rejected and reverted.** Compact
ordinary cells and whole-cell neighbor planning save space in some workloads,
but all three candidate combinations hit the 16 MiB WAL limit during 1,000
scattered inserts at 100K and 1M on both Pi and server. Baseline E4 and SQLite
complete those transactions. All twelve refused cases preserve their exact
committed population. Pi 400K / 1.92M-change single trials take **75.168 s
baseline E4 / 72.462 s combined / 78.309 s SQLite**, with final sizes
**129.888 / 121.229 / 129.446 MB**. Smaller files do not justify the transaction
regression. Storage code is restored to `bcd6448`; strict Law 2, scattered
density, resize and wider release gates remain open. See the
[native comparison, source analysis and rejection](docs/SCATTERED_PACKING.md).

**Fixed-work scaling / lean law groups (2026-09-14):** the seven laws now map
to repeatable `lean`, `scale` and `large` profiles. **74 lean tests pass per
platform; 363 full Mac tests pass.** Eighty scaling/10M arms and eight smoke
arms have exact matching oracles. The new coverage exposes failures:
1,000 scattered insertions at 1M take **1.005 s E4 / 0.425 s SQLite on Pi**;
the 100K scattered case retains **33.620 / 29.450 MB**, **14.16% larger**.
Local work is approximately stable, but strict **L2-WORK is FAIL** for measured
scattered growth. The architecture is retained; the public collection API is
not switched. See [the complete results, test commands and limits](docs/FOUNDATION_SCALING.md).

**Committed foundation / stale-version loop (2026-09-13):** baseline `ee98182`
and page-WAL v2 `2c56936` are preserved on branch `pagewal-foundation`. Exact
reproductions show each old-Store failure contains one stale, checksum-valid
4 KiB page; all parent pointers match the successful tree. Its native cause
remains unresolved. A related deterministic page-WAL snapshot defect is fixed
by binding WAL lookups to the expected frame checksum, with no disk-format or
lookup-entry size increase. **363 distinct Mac tests / 30 selected Pi tests
pass**. Three-run 400K / 1.92M-change medians: guarded E4 versus SQLite
**41.80 / 31.71 s on Mac**, **73.06 / 70.66 s on Pi**. Final logical size stays
**129.888 / 129.446 MB**; the fix is retained as `d0b96ee`. See the
[evidence and open gates](docs/PAGEWAL_STALE_FRAME.md).
The candidate remains separate from the collection Store and is not promoted.

**Owner acceptance update (2026-09-13):** elapsed time **below 1.5× SQLite is
acceptable**. The page-WAL v2 ordinary 400K workload passes on Mac (1.273×)
and Pi (1.049×), with final size only 0.34% above SQLite. Keep the architecture
and safety changes. The earlier 1.10× time target no longer blocks these cases.
Release qualification still requires resolving correctness/recovery gaps and
the remaining large-value/reader tests. The seven laws remain unchanged.

**Page-WAL qualification follow-up (2026-09-13):** the isolated candidate now
recovers a torn checkpoint extension from verified committed WAL, persists its
disk allowance, bounds snapshot admission, and provides source-preserving
raw-KV salvage with separate current rows and uncertain candidates. The full
Mac workspace suite passes 360 distinct test entries; selected Pi suites pass.
Repeated update/cap and four-arm SQLite comparisons are recorded in the
[qualification report](docs/PAGEWAL_QUALIFICATION.md).
**A new correctness failure in the previously accepted Store control is
preserved:** a Mac 400K round-6 check returned a round-4 row. Historical accepted
status is not fresh release qualification. Production source remains unchanged;
the page-WAL candidate is not promoted, and open F1 recovery/reader/parity gates
remain explicit. No SQL or new multimodel indexes were added.

**Foundation standard F1 / page-WAL exploration (2026-09-13):** the user
authorizes an isolated SQLite-style publication architecture before query and
multimodel-index expansion. The [F1 test standard](docs/FOUNDATION_TEST_STANDARD.md)
defines fixed payloads/N, load and repeated CRUD cases, all seven law categories,
SQLite time/size parity targets, peak/cap checks and promotion requirements.
Raw key/value pager results must remain separate from full collection results.
See [the architecture pilot and its open gates](docs/PAGEWAL_PILOT.md).
The pilot completed **39 Mac / 18 Pi arms** and is retained **only in isolation**.
At 400K / 1.92M mixed raw-KV changes, Mac current E4 / page-WAL / SQLite take
**63.024 / 40.892 / 31.046 s**; Pi **97.744 / 72.154 / 69.580 s**. Page-WAL's
ordinary-text final size is **0.34% above SQLite**. These 400K cases are single
probes. Resize remains larger/slower, independent salvage is absent, and the
[F1 qualification check](docs/FOUNDATION_GATES.json) refuses promotion.
Accepted production source and the seven-law contract remain unchanged.

**Commit-stage ablation (2026-09-13): rejected and reverted.** Skipping resets
of a provably empty physical WAL gains only **1.66%** on single-operation
commits and **0.88%** on update-only work. The 100K / 160K mixed-change means
are current E4 **7.238 s**, candidate **7.996 s**, SQLite **3.414 s**, excluding
load; E4 logical sizes are identical. **161 selected checks and 24 timing arms
pass correctness checks.** Diagnostic stages identify data/root and freelist
barriers as the major commit costs. Accepted engine unchanged; no Pi timing,
query execution or new multimodel indexes in this loop. See the
[stage breakdown, comparisons and rejection](docs/COMMIT_STAGE_ABLATION.md).

**Page-edit exploration (2026-09-13): rejected and reverted.** SQLite-inspired
scratch repacking and same-size cell replacement passed 176 selected checks,
but improved end-to-end mutation time by at most **5.52%**, below the 10% gate.
Across two Mac repetitions, 100K people / 160K mixed mutations took accepted
E4 **7.343 s**, best prototype **7.005 s**, SQLite **3.249 s**, excluding load.
No E4 logical-size reduction. The accepted persistent-freelist engine remains
unchanged; no query execution or new multimodel indexes were added. See the
[20-arm experiment and rejection](docs/PAGE_EDIT_EXPERIMENT.md).

**Collection write loop (2026-09-12):** unchanged 1,536-lane embeddings now
avoid redundant sidecar writes: updates are **40.4% faster on Mac / 64.5% on
Pi**, with **91.8% fewer issued bytes** versus the packing baseline. The 400K
mixed workload improves **7.1% / 9.6%**, but remains **2.76× / 2.12× SQLite
time**. A focused held-reader workload completes within a configured allowance
below twice its loaded footprint; this is not a universal expansion guarantee.
344 Mac tests and 163 Pi tests pass. See [results and limitations](docs/WRITE_PATH.md).

**Commit experiment (2026-09-12): rejected and reverted.** Bounded adjacent-page
write combining reduced syscall counts but produced no repeatable end-to-end
gain across 27 three-engine benchmark arms. Production code remains unchanged.
See [measurements and rejection](docs/COMMIT_EXPERIMENT.md).

**In-file freelist prototype (2026-09-12): measured, not promoted.** Small-commit
churn improves about 50% on Mac and Pi; 400K mixed churn improves 29.6% on Mac
but only 5.8% on Pi. The unchanged four-page bookkeeping allowance and overflow
plateau gates still fail. Production code is unchanged; the isolated Mac source
was reverted and the candidate archived. See [results and costs](docs/EMBEDDED_FREELIST.md).

**Persistent freelist loop (2026-09-12): retained.** Reusing the existing freelist
file removes recurring filename replacement and directory syncs while retaining
content durability. Small-commit mutation time falls **30.3% on Mac / 28.9% on
Pi versus E4 before this loop**, with identical final logical sizes and unchanged
capacity/plateau gates. At 400K / 1.92M mutations: Mac E4 before **104.58 s**, E4
after **87.30 s**, SQLite **38.43 s**; Pi **163.09 / 155.99 / 76.56 s**. The Pi
400K gain is only **4.4%**, and SQLite remains faster. **347 distinct Mac tests /
182 selected Pi tests pass**, plus 42 complete benchmark arms. See the
[full comparison, publication protocol and limitations](docs/PERSISTENT_FREELIST.md).

**Prototype evidence (2026-09-09):** the entry-first SQLite-inspired P1 loop
now reaches **1.0017–1.0631× SQLite file size** across 10K/40K scalar + point,
binary JSON, and large-JSON cases, with ordered and shuffled inserts and three
repetitions. See [P1 measurements](docs/ENTRY_RESULTS.md) and the
[concrete storage definition](docs/ENTRY_STORAGE.md). P1 has no vectors,
edges or secondary indexes in either arm. The earlier
[P0 mixed-data results](docs/PROTOTYPE_RESULTS.md) and
[P0 format](docs/PROTOTYPE.md) remain separate evidence. The full e4 API,
multimodel density, output-speed and 48M gates remain unfinished.

**20M population evidence (2026-09-10):** the three-arm full-size run completed:
E4 **265.46 s / 3.587 GB**, E4 with automatic Unix timestamps **359.98 s /
3.760 GB**, SQLite **364.56 s / 3.489 GB**. All 20M rows per arm verified.
See [dataset, fairness conditions and complete results](docs/PEOPLE_20M.md).
These are entry-only measurements with mixed scalar/JSON fields and points,
without graph edges, vectors or secondary indexes.

**Current 10M check (2026-09-10, after R1/R2):** E4 **1.793 GB / 119.61 s**,
E4 with automatic timestamps **1.877 GB / 125.30 s**, SQLite **1.742 GB /
124.62 s**. Plain E4 is **2.92% larger** than SQLite. All 10M rows per arm
verified; results and reproduction evidence retained, this run's generated
databases/input deleted (**8.662 GB**). See [10M report](docs/PEOPLE_10M.md)
for exact bytes, single-run timing limitations and cleanup evidence.

**Recovery R1 (2026-09-10):** a source-preserving salvage command now contains
broken overflow values, names unknown page extents, and keeps obsolete rootless
cells separate from current rows. A retained 40K mixed-data trial recovered
39,999 unaffected entities after one damaged overflow page. The loop also
reproduced an uncached-I/O failure independently of E4; macOS Direct requests
now explicitly fall back to Buffered. See [evidence, commands and open gates](docs/RECOVERY_R1.md).
**Schema recovery R2:** root-independent discovery of existing layout copies
now passes the 10K/40K fault matrix. One/two lost copies still permit exact
field decoding; all-copy loss preserves surviving raw rows and explicitly names
missing layout IDs. The scanner and codec are separate extension interfaces,
with no new normal storage bytes. See [commands, results and limits](docs/RECOVERY_R2.md).
Current-membership reconstruction and the full seven-law gate remain unfinished.

**Lean lifecycle gate (2026-09-10):** 100K/400K typed update/delete/reinsert,
snapshot and reopen checks exposed an inherited overflow reclamation leak.
The focused fix returns replaced/deleted chains to the existing protected
freelist; all pages now account, with **296 workspace tests passing**.
Snapshot churn still retains physical file space until an explicit rebuild.
See [results, tradeoffs and the E3 interface reuse path](docs/LEAN_FOUNDATION.md).

**Peak-space loop (2026-09-10):** measured load-only, repeated updates,
delete/reinsert, short/long snapshots and rebuild peaks at 100K/400K. A
commit-boundary reclamation fix cuts the 400K long-reader churn peak from
**723.3 to 581.4 MB**. With no pinned reader, explicit frequent checkpoints
cut mixed-churn E4 peak from **411.8 to 161.0 MB**, versus SQLite **139.9 MB**,
but E4 mutation time rises **24.9 to 119.1 s** in this diagnostic run.
Frequent publication worsens retained-version growth with an old reader.
The **2× disk budget is not enforced**; checkpoint policies remain benchmark
experiments. Interfaces are paused for resource-aware checkpoints and budget
admission. **298 distinct tests pass**, including the new policy regression.
See [peak/allocated-space tables, tradeoffs and evidence](docs/PEAK_SPACE.md).

**Sustained foundation follow-up (2026-09-11):** lifetime-aware page reuse,
byte-triggered publication, direct dense-v3 encoding/decoding, and an atomic
reader-registration fix pass **308 workspace tests** plus process-kill checks.
Three 100K pairs show E4 faster for fixed updates and typed reads, but about 11%
slower for mixed churn without a reader. The matching 400K / 12-cycle pair is
**201.74 s / 161.27 MB peak / 160.51 MB final** for E4 versus SQLite
**181.06 s / 141.02 MB peak / 132.77 MB final**. Both plateau in the final
matched cycles. Native SQLite automatic checkpoints are enabled. Fresh-load,
rolling-reader and maintenance-gap cases are reported separately. **Overall
parity, bounded metadata RAM and an enforced disk cap remain open; interfaces
remain paused.** Row bytes are unchanged; the derived freelist is now v2.
See [interpretation and validation](docs/SUSTAINED_FOUNDATION.md) and
[paired benchmark tables](docs/SUSTAINED_TABLES.md).

**Pi resource loop 1 (2026-09-11):** an explicit constrained entry-store mode
now persists limits for data, WAL, reuse bookkeeping, readers and record size.
It refuses growth before exceeding its managed logical allowance and publishes
each commit so crash restart requires no replay-page allocation. Failed writers
must reopen before further entry reads or writes. Resource regressions and
process-kill checks run on Mac and Raspberry Pi 5. This is not a filesystem
reservation, a whole-process memory budget or a capped corruption-repair path.
All 18 Pi pairs pass under a verified 128 MiB address-space limit: 196 state
checks and 32 snapshot checks. The 4M ordered-load files are **655.58 MiB E4 /
616.22 MiB SQLite**; ordinary load takes **65.94 / 63.51 s**. The final
constrained 4M timing is marked as affected by other Pi activity. Results,
tradeoffs and the remaining resource/repair gates are in
[the loop report](docs/RESOURCE_LOOP_1.md) and [all paired tables](docs/RESOURCE_TABLES.md).
Verified disposable benchmark databases were removed (**5.18 GB**).
The seven laws and opt-in timestamp policy are unchanged.

**Typed collection loop (2026-09-11):** the authorized internal API slice now
provides persistent collections, composite entity IDs, exact external keys,
typed CRUD, immutable schema versions, snapshots, rollback and optional managed
timestamps. The new failure tests caught and fixed a rollback validation gap.
This supersedes the earlier interface pause for this internal slice; SQL/service
shipping and the remaining resource/repair gates stay separate. The complete
API footprint includes external-key indexes and vector records, so the earlier
entry-only density percentages do not apply. **330 tests pass**, but a matched
400K/12-cycle check reaches **258.1 MiB E4 / 121.8 MiB SQLite peak**, exceeding
2× E4 loaded size. Reachable-page growth remains a foundation issue. See the [API/storage/recovery
contract](docs/COLLECTIONS.md) and [paired measurements](docs/COLLECTION_RESULTS.md).

**Delete/page-packing loop (2026-09-12):** bounded sibling merging and root
collapse eliminate the 30,217 empty entity/vector leaves found in the retained
400K fixture. Twelve-cycle collection churn now plateaus at **139.769 MiB**;
peak is **140.027 MiB**, versus old E4 **258.117 MiB** and SQLite **121.840 MiB**.
No rebuild or identity/format change is used. E4 churn averages **122.774 s**
versus SQLite **39.547 s**; packing adds about 8% to the old E4 control's time.
**339 Mac tests and 158 Pi tests pass.** Snapshot peaks still exceed 2×;
full write-speed parity and the constrained collection expansion gate remain
open. See [the packing report](docs/PACKING_LOOP.md) and
[complete paired tables](docs/PACKING_TABLES.md).

The user's entry-first instruction authorizes focused SQLite-inspired kernel
experiments: optional neighbor balancing and compact cells, retaining the
existing pager, WAL, checksums and snapshots. These are explicit exceptions
to the original encoding-only implementation scope below.

sekejap-e3 is correct, fast, and feature-complete (graph, vector, spatial,
text, SQL, service mode, introspection, change notifications). But at
population scale it is **~3.7× larger on disk than SQLite** for the same data,
because it stores every row as a JSON **text** document.

We proved why, from first principles. The same `person` row (`_key`,
`fullname`, `born`, `born_year`, a point), measured four ways:

| how the row is stored | bytes/row | `born=19490622` | the point | field names |
|---|---|---|---|---|
| **e3 (JSON text)** | **139** (+~60 spliced `_id`/timestamps) | `"born":19490622` = 15 B text | ~58 B text | **every row** |
| A0 (template + text value slots) | 89 | `19490622` = 8 B **text** | 51 B **text** | once |
| **SQLite (typed record)** | **47** | `0129673e` = **4 B** | **16 B** (2× f64) | once |
| from-scratch typed B-tree | **45** | `0129673e` = **4 B** | **16 B** | none in row |

Measured on disk: SQLite base table **52.8 B/row**; a from-scratch typed B-tree
(real 4 KiB pages) **56.9 B/row** — **SQLite parity.** e3's JSON payload is
**~277 B/row** at 48M. At 48M, e3 node payloads are **13.69 GB of a 33.8 GB file
(40.5%)**; typed (~57 B/row) cuts that to **~2.8 GB — ~32% of the whole file.**

**The entity payload gap has two encoding levers** (P1 additionally proved
that whole-file density depends on key/cell framing and page occupancy):
1. **Field names** are written in every row. Store them once, in the schema.
2. **Values are text**: `19490622` is 8 ASCII bytes, not a 4-byte int; a point
   is ~58 text bytes, not two 8-byte floats. Type them.

### Learn from A0 (the previous attempt — a dead end, kept only as a lesson)
A0 templated away the **field names** (lever 1) but **kept every value as JSON
text** (skipped lever 2). Result: only ~2× on the payload, ~11–20% on the file,
plus a new failure mode. It captured half the win and stopped. **e4 does BOTH
levers: names-in-schema AND typed values.** Never store a value as text.

---

## 1. What e4 is, precisely

e4 reuses e3's **proven kernel** — the disk-first B-tree: 4 KiB pages, WAL with
torn-tail proof, copy-on-write, per-page checksums, the verified bulk graft,
snapshot reads, a persisted freelist. That foundation passes the 7 laws at 48M.
Keep it.

What e4 replaces is the **entity encoding**: a row is a **typed, schema-keyed,
positional record** from the first byte written — never JSON text. Everything
else (graph edges, vector/spatial/text keyspaces, the query engine, SQL/GQL
MATCH, service mode, introspection, change notifications, timeout) is seeded
from e3's code and reads fields through typed accessors, not a JSON parser.

> Mental model: **e4 stores a row the way SQLite stores a row.** JSON only ever
> exists as bytes crossing the API — parsed into a typed record on the way in,
> rendered from a typed record on the way out. It is never the storage form.

---

## 2. The core design (what to build)

A row is a **typed record against a versioned layout**, modeled on SQLite's
record format plus a binary tail for schemaless fields:

```
record = [layout_id varint]
         [null bitmap]
         [typed slot 0][typed slot 1] … [typed slot k-1]   ← declared columns, positional
         [overflow: compact BINARY map of any fields not in the layout]
```

- **layout_id** → a versioned schema layout stored ONCE in the catalog: the
  ordered `(field_name, type)` list. Field names live here, never in a row. A
  schema change mints a **new layout version**; old rows keep their layout_id
  and still decode. No row is ever rewritten on a schema change (Law 2).
- **typed slot** → minimal-width per type:
  - integer → minimal-width big-endian (1–8 B), SQLite serial-type style
  - real → 8 B IEEE-754
  - bool → one null-bitmap-adjacent bit
  - text → varint length + UTF-8 bytes
  - timestamptz → typed integer, never ISO text
  - geo point → two f64 (16 B); richer geometry → compact binary (WKB-style)
  - vector → a reference; the fp32 stays in the vector keyspace, never inline
  - null → a bitmap bit, zero body bytes
- **overflow map** → sekejap's schemaless freedom, kept **without JSON text**:
  any field present in the document but not in the declared layout is encoded
  in a compact **binary** key→value map (a JSONB-style tail, binary, not text).
  Declared-but-absent fields cost one bitmap bit. Declared fields get full typed
  compaction; undeclared fields still round-trip.

The reader yields typed logical values to the query engine, and renders JSON
**only** when a caller asks for JSON at the API.

### Collection timestamps — accepted design decision

**Automatic timestamps are OFF by default.** Enable them explicitly with a
simple option when creating a collection; the collection stores the policy
so all writers follow it. Opting in adds managed `_created_unix` and
`_updated_unix` as typed integers. Ordinary user-supplied time fields remain
independent. This is a product/schema decision; **the laws remain exactly
seven**. See [the accepted decision and proposed syntax](docs/TIMESTAMPS.md).
The internal Rust [collection API](docs/COLLECTIONS.md) now implements this
policy, including creation-time preservation, monotonic update times and
policy persistence across reopen. SQL syntax remains proposed.

---

## 3. The five hard challenges (solve explicitly; no hand-waving)

1. **Schemaless / dynamic fields.** Declared → typed slots; undeclared → the
   binary overflow map. A document with fields the schema never declared must
   round-trip. Test this FIRST — it is the property a naive "typed columns
   only" design fails, and the reason the overflow map exists.

2. **The output contract (byte-exact echo is gone — define its replacement).**
   Because storage is typed, e4 re-renders on read; input formatting
   (whitespace, key order, `-0`, `1e3`, trailing zeros) does not survive, the
   same trade SQLite and Postgres make. **Decide and document the canonical
   output form** — e.g. declared-order keys, shortest round-trip float, no
   insignificant whitespace — and make every wrapper and test expect exactly
   that. State it loudly: it is a real, user-visible contract.

3. **Schema evolution via versioned layouts.** Add/drop/retype a column mints a
   new layout_id; old rows decode under their original layout. Never O(table)
   rewrites (Law 2). Lazy/background upgrade is optional, not required.

4. **Multi-model values.** Vectors stay in their keyspace (slot = reference,
   never inline fp32). Geo point → 2× f64; richer geometry → compact binary.
   Nested objects/arrays → the overflow map's binary sub-encoding. None of this
   may bloat the common scalar row.

5. **Law 5 blast radius.** The layout catalog is load-bearing: lose a layout
   descriptor and every row using it is unreadable — potentially a whole
   collection. **State the blast radius per the contract and prove recovery
   can't need the damaged thing.** Layouts are tiny and few: replicate them
   (e.g. two inline copies on separate leaves), bound the descriptor ID domain,
   and keep independently discoverable redundant schema evidence. Positional
   row values cannot reconstruct original field names or exact declarations
   by inference. See [the recovery contract](docs/RECOVERY_CONTRACT.md).

---

## 4. Seeded from e3's code (reuse the engine, not the data)

e3 is at `path/to/dir (branch `master`). e4 starts from e3's
source and keeps everything that does not depend on the payload being JSON
text. None of these read raw JSON directly — they go through field accessors,
which now decode a typed record:

- **Kernel** (`kernel/`): B-tree, WAL, graft, per-page checksums +
  owner-identity verification, snapshots, freelist, the format-version gate, the
  golden-fixture discipline, the published-tree structural verifier.
- **Graph**: edge keyspaces, `link`/`unlink`/`link_meta`, MATCH. Edges
  reference nodes by key/id and never touch payload bytes — carries over
  directly.
- **Vector / spatial / text** keyspaces + indexes (HNSW, grid, BM25, GIN,
  SEARCH): adapt field extraction to read a typed slot instead of parsing JSON.
- **Query engine** (`src/query.rs`, `src/exec.rs`): `Set`/`Step` executor,
  streaming `for_each_row`, covered/sorted/batched drivers, plan cache.
- **Service** (`open_as_service()`, snapshot reads), **introspection**
  (`SHOW …`, `EXPLAIN [ANALYZE]`, `information_schema.*`/`pg_indexes` — whose
  column-name+type catalog is exactly the schema source a typed layout needs),
  **change notifications**, **statement timeout**, **bounded aggregate
  summaries**.
- Benchmarks: `bench/popsim` (48M), `bench/popsim_sqlite` (SQLite size/load
  mirror), `bench/three_ways` (lean benchmark, `--gate-output`),
  `bench/spacebreak`.

The work is a focused payload-codec + accessor change, **not** a green-field
kernel. If you are rewriting the B-tree, stop — that is not the task.

---

## 5. The laws and the north star (non-negotiable)

The 7 laws (copied into [CONTRACT.md](CONTRACT.md)):
1. **Disk-first, bounded RAM** — data ≫ RAM on the device; never load the DB to
   answer a query.
2. **Cost ∝ change, not size** — writes and schema changes cost in proportion to
   what changed, never O(table). (Versioned layouts honor this.)
3. **Nothing fallible may delete** — recovery/cleanup never destroys the only
   copy.
4. **Name your sacrifice** — every trade stated (here: the byte-exact echo, and
   the layout-catalog blast radius).
5. **No corruption unrecoverable** — checksum every unit read, bounds-check
   everything off disk, recovery can't need the damaged thing, state the blast
   radius of one bad byte.
6. **Write never blocks read** — snapshot reads.
7. **Ingest usable on device** — bulk load within the device budget.

Audiences (why disk-first): embedded/embodied AI, mobile, games, science,
resource-constrained servers; ~70% of usage is one user per instance. Where
behavior is not graph-specific, **match PostgreSQL semantics.**

---

## 6. The measurable goal + the gate

**Primary target:** a row stored at **~57 B/row** (SQLite parity). At 48M,
node storage **~13.7 GB → ~2.8 GB**; whole file **33.8 GB → ~20–24 GB**
(membership/catalog/index overhead are separate, smaller follow-on levers). End
state: **≤ ~2× SQLite on disk**, approaching parity on the row itself.

**Must-not-regress:** every query family in the lean benchmark (`three_ways`) —
graph hops, vector KNN (recall 1.000), spatial, text, SQL — stays within its
current ratio to SQLite. `SELECT *` / `get` full-output cost ≤ ~1.2× (measure
typed decode; do not assume).

**Method:**
- **Tests first, failing** — write the test, see it fail, then fix. Unfixed
  defects live as `#[ignore]` tests.
- **48M gate**: `bench/popsim 48000000` on the server, with `bench/popsim_sqlite`
  as the size/load A/B. Report bytes/row + per-keyspace breakdown (`SHOW
  STORAGE`).
- **Lean gate**: `three_ways [rows] --scale --gate-output` — counts + timings
  vs SQLite, small and scaled so an O(N) defect shows as a rising ratio.
- Ablation-prove each win; name every sacrifice; full kernel + main suites are
  the commit gate. Big tests on `/Volumes/scratch` or the server, never the boot
  disk.

---

## 7. Suggested build order

1. **Spec the typed record + layout catalog** (§2) before coding: wire format,
   type tags, null bitmap, binary overflow-map, layout versioning, the catalog
   keyspace + Law-5 replication/recovery + canonical-output contract (§3.2).
2. **Codec in isolation** — encode/decode with round-trip property tests:
   (a) declared-schema row, (b) row with undeclared extra fields, (c) every type
   incl. null, (d) the number/float canonicalization. Measure bytes/row against
   the ~45–57 target on a popsim-shaped row.
3. **Wire the typed codec into the node store** as the ONLY payload path — every
   write produces a typed record, every read decodes one. No JSON branch.
4. **Field extraction for indexes/queries** reads typed slots (no JSON parse).
5. **Gates** — lean-benchmark no-regression, then the 48M size A/B.
6. **Wrappers + output contract** — every binding expects the canonical output
   form; document the semantic change in the changelog.

---

## 8. Provenance / pointers

- **e1** `path/to/dir — the original, **READ-ONLY oracle.** Never
  modify. Endgame: empty its content and drop e4's in — not a data migration.
- **e2** `path/to/dir — the kernel foundation e3 was seeded from.
- **e3** `path/to/dir (branch `master`) — the feature-complete
  engine e4's **code** is seeded from. The A0 dead end is on e3's branch
  `a0-rebase` — read it only to learn what not to do.
- Byte-level size analysis justifying e4:
  `/tmp/sekejap-a0-research-analysis-2026-09-09.md` — copy the relevant parts
  into e4's `docs/` early (`/tmp` is ephemeral).
- Consuming customer: **app** (`path/to/dir on crate 0.16.5) —
  introspection and service mode were built for it; e4 keeps those working.

---

## 9. Scope discipline (what NOT to do)

- **Never store JSON text on disk.** JSON is an API wire format only.
- **No legacy payload, no mixed-format reader, no JSON fallback, no migration
  from e3's data.** e4 is pure — one payload path, typed.
- Do **not** rewrite the kernel B-tree / WAL / graft — reuse e3's.
- Do **not** store values as text (A0's mistake) — type them.
- Do **not** store vectors inline — keep the vector keyspace.
- Do **not** rewrite every row on a schema change — version the layout.
- Do **not** widen a test limit or delete an index to manufacture a size win.
- Do **not** break the 7 laws to win bytes; if a law must bend, name the
  sacrifice and get sign-off.
- Ship nothing until the gates pass and the owner says otherwise.
