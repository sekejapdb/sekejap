# Disk format freeze — release direction, 2026-09-15

## Current boundary — 2026-09-16 (supersedes historical status below)

The named storage baseline is **e4-format-v1**, engine commit `59d1cbc`.
Existing byte meanings are frozen under Law 8. [FORMAT_BASELINE.md](FORMAT_BASELINE.md)
records its immutable source/binaries/corpus, cross-build read/write rollback
checks and future release gate. [FORMAT_V1.md](FORMAT_V1.md) defines the actual
`E4PWAL02` envelope, typed layouts/identities, feature policy and extension rules.

The three-mode full Linux qualification passed; the original fixtures are
unchanged. A second, committed-reference corpus is separately pinned and
mandatory. The preserved default engine binary and compact/retained builds
passed20unchanged-feature upgrade/rollback arms, including pending WAL.
This is an honest cross-build baseline; later public versions do not yet exist.
Every later release must test against these retained bytes and executables.

The [recovery runbook](PHASE1_RECOVERY_RUNBOOK.md) identifies supported salvage,
current-versus-candidate evidence, source preservation and explicit limits.
Its actual PageWal repair CLI also passed a clean committed-WAL smoke.

The final evidence is verified and Phase1 storage-shape stabilization is complete;
[PHASE1_STATE.md](PHASE1_STATE.md) records execution status. It does not claim
all-eight-law product qualification or a public deployment. SQL, EXPORT/IMPORT,
multimodel indexes/adapters and release packaging remain product work. New
index families require noncolliding namespaces, explicit encoding/catalog
versions/features and immutable compatibility fixtures before they ship.
They must preserve the frozen entity representation. Coordination files remain
part of the locking/publication protocol and cannot be unlinked while live.

## Historical direction and evidence — 2026-09-15

The remainder retains the earlier decisions and measured evidence for their
named source revisions. Historical status and proposed release sequencing do
not supersede the current boundary above or the adopted Law 8 contract.

The owner's priority is now a durable disk-format contract that lets users
upgrade E4 without exporting and reimporting their databases. Do not make
every performance target a prerequisite for starting interface work. The
first seven laws retain their wording; the owner has now added Law 8 in
CONTRACT.md. A format freeze is not proof of production safety.

Status: **shape established; compatibility promise not yet qualified**.
This document records the direction, not a declaration of a frozen format.

## Owner requirements clarified after the pause

- First consumers are **app and app**. Pi-specific research can wait;
  target-device support remains in scope, but Pi benchmark convergence is not
  the immediate release objective.
- The product goal is strong **combined multimodel SELECT queries**, not
  matching SQLite's insert/update speed. About 1.75× SQLite write time can be
  acceptable when demonstrated query benefits justify it. This is conditional
  acceptance, not an unmeasured claim that E4 already has those benefits.
- Prefer a stable disk format across binary upgrades. Provide explicit
  **EXPORT / IMPORT** commands as well; routine upgrades should not depend on
  manual export/import. Their lossless scope must include identities, schemas,
  relationships, typed values, timestamp policy and index definitions.
- Preserve the E1/E3 interface direction: SQL, embedded/library APIs, CLI,
  language/device bindings and network adapters. Do not reduce the product
  scope to the current raw-KV prototype or one interface.
- The format contract includes persisted **index encodings and metadata**,
  not just pages and entity values. Law 8 now makes release compatibility a
  requirement; Laws 1–7 and the timestamp default retain their wording/policy.
  Safety, bounded disk use and honest cost reporting still apply.

The earlier SQLite 1.5× write threshold is no longer an unconditional release
veto. The reservation candidate was reverted under that earlier criterion;
this clarification does not automatically restore it or prove its disk cap.
Its measured tradeoff remains available for the integrated product decision.

E3 source inspection: `kernel/src/keys.rs` defines graph, property, text,
spatial and vector-navigation keyspaces; `README.md`, `wrappers/README.md`
and `docs/usage/connectivity.md` describe the interface surface. Wrapper
presence/documentation is not fresh proof of every platform's working status.
E4's current collection tags overlap E3 meanings (for example E4 collection
name tag `0x10` versus E3 geometry tag `0x10`). Reuse algorithms/interfaces
through an explicit namespace mapping, never copy these persistent tags blindly.

## Historical shape and candidate-r3 boundary

- The kernel uses 4096-byte checksummed, identity-checked slotted pages,
  B-tree leaves/interiors, free pages and overflow pages.
- Typed collections already have stable composite entity identities,
  immutable layout IDs, dense-v3 records, binary JSON, points, vector
  keyspaces and an explicit timestamp policy (off by default).
- **Accepted as of commit `64b6663`:** only a raw-KV page-WAL recovery fix —
  binding WAL recovery to the database identity and checkpoint history.
  `collections::Database` in that commit still writes through the inherited
  kernel `Store`, not `PageWalStore`. This commit does **not** contain the
  typed-collection integration described below.
- **Candidate r3, tested and retained for continued development** (source
  archive `ee3ae0196ae832de72d67e9c73218d071288f52578df5e07c5b8bce5652feaf1`;
  benchmark binary
  `d717f3955870867661d3ecd5df3d0e5f4e1c34c44d1ef7f112596b9b7656000a`):
  `collections::Database` stores through `PageWalStore`
  (`src/collection_backend.rs` is the one place the backend is selected);
  the inherited kernel `Store` is no longer a collection backend in this
  candidate. The data/WAL header is `E4PWAL02` (page-image WAL, two
  checkpoint metadata copies, identity-bound recovery: a foreign or stale
  WAL, or an unknown required feature, is refused before any byte changes).
  Physical data/WAL bytes are unchanged from the accepted raw pilot.
  Directory layout adds a reconstructible 96-byte `readers.lock` hint file
  plus eight zero-byte reader-slot files. The collection header payload is
  8 bytes for an ordinary database, unchanged; a `create_limited` database
  appends the kernel's 56-byte `E4LIMIT1` record for a 64-byte payload,
  refused if the payload length is anything else. Its Linux run reports
  `FINAL_EXIT0` (main) and `LEAN_EXIT0` (remaining lean/kernel groups):
  default and compact-cells feature builds each pass `lib` 25,
  `collection_pagewal` 15, `collections` 7, `pagewal` 13,
  `pagewal_repair` 1, `pagewal_recovery_identity` 6,
  `pagewal_transaction_capacity` 1, `recovery_faults` 14,
  `schema_recovery` 7 — 89 unique entries per feature mode (178 total,
  child subprocess summaries excluded), plus 74 further lean
  kernel/io/packing executions across both modes (252 total); release
  build passed; both 10K smoke runs (timestamps off/on) passed; both 1M
  mixed-CRUD benchmark runs (timestamps off/on) completed with every
  independent oracle check (exact IDs, deletes, timestamps, cross-engine
  CRC) passing at every phase and after reopen. Full numbers in
  `docs/V2_FOUNDATION_LOOP.md`. An earlier snapshot of this same candidate
  line (r2) was diagnostic only; r3 is the source actually tested and
  retained; the integration commit records this tested source without a
  co-author. The decision is
  a **functional-improvement retention for continued development** — not
  raw-write-optimization acceptance, not a stable disk-format freeze, and
  not a production release. The owner subsequently accepted disk behavior
  against SQLite: sampled peak allocated bytes are +0.70% with timestamps
  off and +4.18% with timestamps on; final logical size is +11–12%.
  A separate 2x physical-space ceiling is not a blocker for this loop.
  Allocated and logical peaks are both recorded, and `ResourceLimits`
  still does not guarantee a hard allocated-block reservation.
- `E4PWAL02` persists required feature bits, including compact cells.
  Since the loop-2 codec change (2026-09-16) the cell encoding is a
  property of the database, not of the build: every build decodes both
  cell families, a writer encodes new cells in the family the header
  declares, and opening never changes the declared bits. The
  `compact-cells` cargo feature only selects the default for databases
  created by that build. Header validation refuses any required bit
  outside the set this release implements before any byte is changed.
  The reference-candidate fixture set and its lean test are described in
  `FORMAT_V1.md`. Preserve that candidate corpus unchanged; when the owner
  names a release, capture a separate corpus from that binary and retain both.
- A reference-fixture pass (archive
  `b54d8f8e3a5fc71da6c667145153560bd82075a0713c1f253891127977aeb4d9`) built a
  small database with the accepted `64b6663` `Database` (old typed encoder
  over the old `Store`, since that commit predates the integration) and
  streamed its logical KV pairs into a fresh `PageWalStore` via a helper,
  not through candidate r3's integrated writer above. It is
  bounded preparatory evidence that the typed encoding survives moving into
  the page-WAL container, not proof of the candidate writer's byte-for-byte
  compatibility, and not a released or frozen baseline. The compatibility
  suite against candidate r3 now reports four passing checks — `compat-new`,
  `compat-old-raw`, `compat-old-typed`, `compat-old-typed-wal` — all EXIT0,
  with the original fixture hashes unchanged. See `docs/V2_COMPAT_FIXTURES.md`.
  No index-compatibility proof exists yet.

These pieces must become one selected release format and collection path
before promising that a released database survives an ordinary binary upgrade.

## Historical work outline before promising compatibility

1. Specify the chosen release format from the actual code: file/header
   identification, supported versions/features, page/cell/overflow encoding,
   WAL commit interpretation, catalog/layout encoding, identities and key tags,
   including the graph and each persisted index family's encoding/version.
   Set explicit behavior for unknown required features before modifying files.
2. Resolve format-affecting recovery questions: what identifies committed
   membership when metadata or WAL is damaged, and what schema/vector
   evidence must survive independently. If extra persistent information is
   necessary, choose it before the first compatibility promise. Do not label
   ambiguous recovered candidates as current committed rows.
3. Connect the existing typed collection boundary to the selected engine and
   verify its mixed-type CRUD, schema evolution, timestamp policies, failure
   behavior and reopen. **Done for the raw typed-collection path in
   candidate r3**, tested on Linux and retained for continued
   development (`docs/V2_COLLECTION_INTEGRATION.md`; lean coverage in
   `docs/FOUNDATION_LEAN_GROUPS.json`'s `typed` group; full evidence in
   `docs/V2_FOUNDATION_LOOP.md`) — a functional-improvement retention, not a
   stable format freeze or production release; the accepted `64b6663` commit
   itself does not contain this integration. Map E3's index families and
   interface semantics onto this path; qualify representative persisted
   indexes before declaring the combined format frozen. Do not claim raw-KV
   or single-process typed-CRUD tests cover indexed collections,
   cross-process concurrency at scale, or released-fixture compatibility
   (L8-COMPAT remains PENDING).
4. Preserve immutable databases written by a frozen reference binary. Test a
   newer binary reading, updating and reopening them, including committed WAL
   awaiting checkpoint, overflow values, earlier layout versions and indexes. Compare
   against independent expected entities and preserve the source fixtures.
   Same-build round trips alone do not prove upgrade compatibility.

After these pass, declare a named format version and enforce compatibility in
the lean suite. Interface development can then proceed while performance work
continues. Production readiness still needs its separate safety evidence.

## Format-neutral improvements and compatibility direction

Cache policy, syscall batching, disk reservation, page placement, split/merge
policy and packing within existing supported cell encodings can improve without
changing how existing bytes are interpreted. New indexes can use allocated
key tags and catalog descriptors rather than changing entity encodings.
This is an architectural allowance, not proof that every future optimization
or recovery solution is format-neutral.

The adopted upgrade promise is permanent read/write compatibility with earlier
released E4 formats, without a mandatory full rewrite. New binary versions
need not imply new disk versions. Optional new encodings require explicit
feature creation/conversion while preserving support for old encodings. Law 8
also requires minor-version rollback tests within an unchanged feature set;
older binaries may refuse explicitly enabled newer features. Released-binary
rollback evidence remains pending.

## Historical reservation evaluation

The reservation evaluation finished: 96 comparison arms verified, and the
candidate was reverted under the former timing criterion. Its source patch and
measurements are retained in `RESERVATION_LOOP.md` and linked artifacts.
Do not start another broad raw-write optimization search before resolving the
format/interface scope above. Establish representative app/app mixed
queries as the product benchmark before claiming the conditional write/query
tradeoff has been earned.
