# sekejap disk format v2

The on-disk envelope as it stands today is **sekejap disk format v2**, and it
is stable the way SQLite's file format is stable. Every sekejap from 0.17.0
reads and writes v2.

What that means, in four sentences the rest of this document only elaborates:

- **Performance work leaves files byte-identical.** Cache policy, syscall
  batching, page placement, split and merge policy, packing inside a shipped
  cell encoding: all of these may change, and none of them may change a byte
  of a file that would otherwise have been written.
- **A new capability adds a keyspace plus an additive feature bit, and
  nothing else.** The bit is set in the same transaction as the first record
  that needs it, never cleared; a file declaring a bit outside the mask this
  build implements is refused before a record is read.
- **A change to the physical layout, the WAL protocol or a shipped encoding
  would be v3, and is not expected.**
- **There is no v1.** sekejap has never published a v1 file, and reads no
  file written by the engine it replaces. See [What v2 is not](#what-v2-is-not).

[CONTRACT.md](../CONTRACT.md) remains the policy authority; Law 8 is the law
this document serves. [FORMAT_BASELINE.md](FORMAT_BASELINE.md) records the
preserved binaries, the permanent corpora and the cross-build rollback gate.

## The stamp

Page header bytes 18-19 were reserved and zero, documented as not free for
silent use. They are the disk-format version:

- A **little-endian u16**, written as **2** on every page the store creates or
  rewrites. `PageMut::init` stamps a page the moment it is initialised, and
  `page::seal` — the one place a page image leaves for the medium — stamps it
  again, so a rewrite through `PageMut::reopen` carries it without every
  mutation site having to remember.
- **Checked at OPEN on both checkpoint metadata copies (data pages 0 and 1)
  before any other page of the file is read.** `disk_header` in
  [core/engine/src/store/pagewal/format.rs](../../core/engine/src/store/pagewal/format.rs)
  is the first read every open performs (`Pager::inspect_bounded`), and it
  judges each copy whose page checksum verifies. An intact copy claiming
  anything but 2 is refused even when its sibling says 2, for the same reason
  an intact unsupported feature set may not be hidden by a healthy sibling. A
  copy that fails its checksum is damage, not a foreign format, and may still
  fall back to its sibling.
- A value that is not 2 is refused as **`Unsupported`**, naming the number the
  file carries: **`sekejap disk format <n>; this build reads v2`**. No file is
  changed — not a data byte, not a WAL byte, not a coordination file, not the
  directory inventory.
- The same judgement applies to every page that arrives from the medium, not
  only to the two metadata copies: `PageRef::open` refuses an intact page
  whose stamp is not 2, so a file cannot be half v2. The stamp is read
  **after** the page checksum, never before.
- **WAL frames carry page images, so they carry the stamp.** A kind-1 frame's
  4096-byte payload is a sealed page; it is stamped when it is sealed, and
  `Pager::write_at` opens it before framing it.

The constant is `FORMAT_VERSION: u16 = 2`, defined once, in
[core/kernel/src/page.rs](../../core/kernel/src/page.rs) beside the header
field it is stamped into. `kernel::FORMAT_VERSION`,
`sekejap_core::FORMAT_VERSION` and `sekejap::FORMAT_VERSION` (beside
`sekejap::VERSION`) are re-exports of that one constant, not copies.

It is not `kernel::meta::FORMAT_VERSION`, which is the logical superblock
version of the inherited kernel `Store` (1 or 2) and says what that store's
pages CONTAIN. This one is the envelope: which sekejap disk format the file
is.

Tests: [core/kernel/tests/format_stamp.rs](../../core/kernel/tests/format_stamp.rs)
for the page header and the kernel `Store`'s two metadata slots,
[core/engine/tests/format_v2_compat.rs](../../core/engine/tests/format_v2_compat.rs)
for the page-WAL store's metadata copies, its data pages and its WAL frame
images, and [dist/rust/tests/api.rs](../../dist/rust/tests/api.rs) for the
published crate's re-export and for a database created through `Db::open`.

## What v2 is not

- **There is no v1.** No sekejap release ever published a v1 file. The
  pre-release corpus carried zero in bytes 18-19 because those bytes were
  reserved; a zero stamp is refused by the same sentence as any other
  non-2 value, naming 0.
- **sekejap reads no file written by the prior engine.** That engine,
  published as `sekejap-0.16.5`, is a different design with different
  persistent key tags — the collection-name tag `0x10` here is its geometry
  tag, to take one — and nothing in this build attempts to interpret one.
- **There is no silent conversion.** A file that is not v2 is named and
  refused with every byte where it was. An engine that could be wrong about
  what a file is has no business rewriting it (Law 3), and a refusal that
  names the number is the only honest answer available.
- **The stamp is not a build switch.** It does not depend on a cargo feature,
  on the cell family the file declares, or on which binary created the file.
  Every build of every sekejap from 0.17.0 writes 2 and reads 2.

## The envelope

The typed collection path selects `PageWalStore` in
[core/engine/src/store/mod.rs](../../core/engine/src/store/mod.rs) (re-exported
as `sekejap_core::collection_backend`). Its physical page version, page-WAL
magic, required feature bits, typed metadata versions and the disk-format
stamp are separate identifiers; none replaces another's check.

| Unit | Current representation and source |
|---|---|
| Physical page | 4096 bytes; magic `0x53454B32`; little-endian u16 version 1 at offset 4; kind, tree ID, slot count, page number, free pointer, sibling/child link, generation and CRC32C. CRC excludes its own bytes 36–39. Slot directory starts at 40. [core/kernel/src/page.rs](../../core/kernel/src/page.rs), `PageMut::init`, `PageRef::open`, `seal`. |
| Page header bytes 18–19 | **The disk-format stamp: little-endian u16, always 2** (`FORMAT_VERSION`). Written by `PageMut::init` and re-written by `seal`; covered by the page CRC; checked on both metadata copies at open before any other page is read, and on every page that arrives from the medium. Not 2 is refused as `Unsupported` naming `sekejap disk format <n>; this build reads v2`, with no file changed. [core/kernel/src/page.rs](../../core/kernel/src/page.rs). |
| Metadata pages | Data pages 0 and 1 contain a 56-byte `E4PWAL02` slot: magic at 0, root u32 at 8, free-head u32 at 12, logical allowance u64 at 16, 16-byte database identity at 24, transaction u64 at 40, required features u64 at 48. [core/engine/src/store/pagewal/format.rs](../../core/engine/src/store/pagewal/format.rs), `Header`, `disk_header`. |
| WAL frame | **4144 bytes**: 32-byte frame header, 4096-byte payload, 16-byte database identity. Header: `E4PWAL02` at 0, kind u32 at 8, page number u32 at 12, transaction u64 at 16, committed page count u32 at 24, CRC32C at 28. CRC covers the entire frame with its checksum field zeroed. The payload of a kind-1 frame is a sealed page image and carries the stamp. [core/engine/src/store/pagewal/mod.rs](../../core/engine/src/store/pagewal/mod.rs), `frame`, `read_frame`. |
| WAL publication | Kind 1 carries a page image; kind 2 commits a transaction and carries the running page-frame CRC in the payload's first u32. Inspection requires transaction sequence, database identity, extent and checkpoint-history agreement, including transaction metadata; only a verified committed prefix is published. [core/engine/src/store/pagewal/mod.rs](../../core/engine/src/store/pagewal/mod.rs), `Pager::inspect_bounded`. |
| B-tree cells | Ordinary and compact leaf cells, interior child pointers and overflow references/chains retain their encodings. Every build reads both supported cell families. [core/kernel/src/btree.rs](../../core/kernel/src/btree.rs), record encoders/decoders; structural checks in [core/kernel/src/verify.rs](../../core/kernel/src/verify.rs). |
| Typed records | Dense-v3 positional records with immutable layout IDs, binary JSON, scalar/point values and separate f32 vector rows. Catalog/layout/counter packets retain independent copies. [core/engine/src/store/dense_v3.rs](../../core/engine/src/store/dense_v3.rs), [core/engine/src/collections/mod.rs](../../core/engine/src/collections/mod.rs), [COLLECTIONS.md](COLLECTIONS.md). |
| Typed resource policy | Ordinary collection header payload is 8 bytes; `create_limited` appends the kernel's 56-byte `E4LIMIT1` record, giving 64 bytes. Other lengths are unsupported. [core/engine/src/collections/mod.rs](../../core/engine/src/collections/mod.rs), `HEADER_PLAIN`, `HEADER_LIMITED`, header codec. |

Required feature bit 0 is `COMPACT_CELLS`; the supported set is currently just
that bit. Writers adopt the file's declared set and ordinary writes preserve
it. The Cargo feature selects a creation default, not which existing family
can be read or written. Unknown required bits, unsupported physical page
versions and a disk-format stamp other than 2 are all refused before recovery
or file mutation; the focused tests exercise checksum-valid unsupported
metadata, including either physical metadata copy.

Page bytes 32–35 are reserved and currently initialized to zero. They stay
zero under v2; they are not free space for an unannounced optimization. Bytes
18–19 are no longer reserved: they are the stamp above. Bytes 24–31 hold the
publishing generation written by `seal`. Any future header meaning requires an
explicit version/required-feature design that preserves old readers' safe
refusal and newer readers' and writers' support for shipped encodings.

`readers.lock` contains two 48-byte `E4PWHNT1` publication hints. Eight
`reader-N.lock` files and `writer.lock` participate in advisory locking. These
are derived coordination state, not entity/catalog payload, but they still
form a concurrency protocol. Reconstructibility does **not** make live deletion
or replacement safe: lock identity, admission and publication ordering must be
preserved. Rebuild only through the engine's coordinated/quiescent path; future
protocol changes require separate mixed-version concurrency analysis. See
[core/engine/src/store/pagewal/mod.rs](../../core/engine/src/store/pagewal/mod.rs),
coordination and snapshot admission code.

## Extension boundary

Current key allocation includes collection metadata/replicas, layout keys, live
row-count records (`0x08`), collection names (`0x10`), external-key mappings
(`0x20`), entity rows (`0x40`) and vector payloads (`0x60`) in
[core/engine/src/collections/mod.rs](../../core/engine/src/collections/mod.rs).
The index families follow: `0x70` scalar entries, `0x71`/`0x72` the two graph
edge directions, `0x73` exact vector, `0x74` spatial point, `0x75`-`0x78` text
postings / norms / term statistics / corpus statistics, `0x79` quantized
vector, `0x7A`/`0x7B` packed text segments and norm blocks, `0x7C` geometry
cells, `0x7D` the VAMANA GRAPH's node records
([core/engine/src/index/vector/graph.rs](../../core/engine/src/index/vector/graph.rs),
behind `VAMANA_FEATURE = 0x8000`), `0x7E` graph ENDPOINT SETS
([core/engine/src/index/graph/endpoints.rs](../../core/engine/src/index/graph/endpoints.rs),
behind `ENDPOINT_FEATURE = 0x4000`), and `0x7F` the VAMANA GRAPH's ADJACENCY
records — the same family and the same `VAMANA_FEATURE = 0x8000`, in a second
keyspace because the two records have different lifetimes and different sizes
(below). The live per-collection row count took
`0x08` rather than `0x7D`, so `0x7D` was the last free tag in the index run and
the vamana family is what claimed it. **The index run is now FULL: `0x7F` was
the last free tag and the vamana family took it too.** Graph, spatial, text and vector-navigation
indexes allocate new, noncolliding namespaces after auditing the complete
registry; the prior engine's tag values cannot be copied blindly. Each
persisted index family needs explicit encoding version/catalog descriptors and
any required feature declaration before it ships. Enabling a new representation
is explicit; ordinary upgrades may not silently convert a database or force an
index rebuild. All shipped encodings remain readable **and writable** by newer
releases under Law 8. Adding a keyspace plus an additive feature bit is the
only permitted change to the on-disk format under v2. Add immutable fixtures
when each family ships.

### The vamana graph, as the worked example of this boundary

The Vamana/DiskANN graph is the one family since v2 froze that changed the
on-disk format, and it changed it in exactly the two ways this section permits
and in no other:

* **Two new keyspaces, `0x7D` and `0x7F`, under ONE feature bit.**
  `key = 0x7D || ordered(index id) || ordered(sequence)`. Sequence 0 is the
  per-index GRAPH HEADER (version, degree, build search list, alpha, entry
  point, node count); every other sequence is one node's HEAD, which is
  byte-for-byte a quantized entry (`locator:6 | scale:f64le | int8 codes`)
  and nothing else. That node's neighbour list is a SEPARATE record,
  `key = 0x7F || ordered(index id) || ordered(sequence)`, holding
  `own:u16be | back:u16be` and that many `(neighbour:u64be, distance:f32be)`
  edges. The two keyspaces hold exactly the same sequences: a node has one
  record in each, a delete removes both, and `verify_indexed_source` checks
  that claim in both directions. No existing keyspace, record or descriptor
  moved a byte.

  **Why two records and not one.** The head's length is the DIMENSION: at
  4,096 lanes it is 4,110 bytes, larger than a 4,096-byte page, so a single
  record spanning both lived in an overflow chain. Every back edge an insert
  appends is a read-modify-write of a NEIGHBOUR, and rewriting a shared
  record rewrote that neighbour's codes and its whole chain — `R = 48` times
  per insert. Measured at 4,096 lanes over a 600-node graph, one insert
  appended 2,780,624 bytes of WAL (2.65 MiB), so a build transaction of any
  size was refused by the page-WAL's 16 MiB managed-byte allowance and
  halving the batch could not help: the cost was never per row. Split, the
  same insert appended 265,216 bytes, an adjacency record is at most
  `4 + 2R * 12 = 1,156` bytes and therefore always inside one page, and the
  head is written ONCE when the node is linked and never rewritten
  (`core/engine/tests/index_vector_vamana_layout.rs`).
* **One additive feature bit, `0x8000`.** Set in the same transaction that
  creates the first vamana descriptor, never cleared, and
  `SUPPORTED_LOGICAL_FEATURES` moved from `0x7fff` to `0xffff` with it. A
  binary whose mask predates the bit refuses such a file WHOLE, at admission,
  as `Unsupported` and never as corruption
  (`core/engine/tests/format_vamana_compat.rs`
  `a_build_that_predates_the_vamana_bit_refuses_the_corpus_by_name_and_changes_no_byte`).

Its immutable fixtures are `docs/format-v2-vamana/`: two independently
captured corpora (checkpointed and wal-pending) at the layout above, written by
[bench/src/bin/vamana_format_fixture.rs](../../bench/src/bin/vamana_format_fixture.rs)
and never regenerated to make a later engine pass. Each MANIFEST carries the
generator's own brute-force expected neighbours, so the compatibility suite
compares the engine against numbers no engine produced.

```sh
cargo run -p sekejap-bench --release --bin vamana_format_fixture -- docs/format-v2-vamana
```

**The index DESCRIPTOR's own encoding versions** are the second additive
register, and the scalar family is the only one that has more than one. A
version is a TAIL: nothing before it moves, and each version's tail is fixed by
the thing it carries, never chosen by a writer.

| version | family | tail after the cursor | shipped with |
|---|---|---|---|
| 1 | every family | nothing | the original catalog |
| 2 | scalar, spatial point | `tree_id: u16 BE \|\| root: u32 BE` | `INDEX_TREE_FEATURE = 0x80` |
| 3 | scalar | version 2's tail, then a one-byte expression tag (`1` = `lower`) | `EXPRESSION_FEATURE = 0x400` |
| 4 | scalar | version 3's tail with tag `2` = `->>`, then `len: u16 BE \|\| member: UTF-8` (1..=128 bytes) | `JSON_EXPRESSION_FEATURE = 0x10000` |

Version 4 is the JSON-PATH expression index: a scalar index whose stored value
is `col->>'member'` over a `JSONB` column, added 2026-09-22. The member name
travels in the descriptor because it is part of the IDENTITY of the index a
predicate names -- two indexes over two members of one column are two
different indexes. The variant chooses the version
(`IndexExpr::descriptor_version`), so a reader that meets tag `2` under
version 3, or tag `1` under version 4, refuses the descriptor as `Unsupported`
rather than reading the next field at the wrong offset.

The version alone already stops an older binary -- version 4 is outside the
`(family 1, version 1 | 2 | 3)` pairs it admits -- and the feature bit is
taken anyway. That refusal fires when a DESCRIPTOR is read, deep inside
admission, and reads as "index family 1 encoding 4"; the bit refuses the file
WHOLE at `admit_features`, before a byte is touched, with the sentence every
other additive change uses. Law 8 asks for the refusal that cannot be mistaken
for damage.

## Fixture provenance

Written by [bench/src/bin/format_fixture.rs](../../bench/src/bin/format_fixture.rs)
through the public `collections::Database` API over `PageWalStore` (not the old
KV-projection helper), by the stamped binary:

```sh
cargo run -p sekejap-bench --release --bin format_fixture -- docs/format-v2-fixtures
cargo run -p sekejap-bench --release --bin format_fixture -- docs/format-v2-baseline
```

Two runs, two corpora: the 16-byte database identity is drawn fresh for each
created database, so the two are independent captures of the same logical
content rather than copies of one.

`Database` / `PageWalStore` do not checkpoint on drop. A wal-pending fixture is
`commit()` then drop with no `checkpoint()`.

## Fixture matrix

Path: `docs/format-v2-fixtures/`. Each directory has engine files plus
`MANIFEST.json` (expected entities regenerated from the content function, not
read back). `INDEX.json` lists the set. Every fixture holds the same logical
content: 200 people (timestamps off; nullable real; point; binary JSON;
non-ASCII; one deleted key; one deleted-then-reinserted key), 80 events
(timestamps on; layout 2 then alter to layout 3 so both layouts coexist),
4 blobs (1536-lane vector and JSON > 4 KiB). Live entity count: 283.

| fixture | compact cells | checkpoint | limited | engine bytes | data | wal |
|---|---|---|---|---:|---:|---:|
| compact-off-checkpointed | off | yes | no | 249952 | 249856 | 0 |
| compact-off-wal-pending | off | no | no | 522144 | 8192 | 513856 |
| compact-on-checkpointed | on | yes | no | 249952 | 249856 | 0 |
| compact-on-wal-pending | on | no | no | 522144 | 8192 | 513856 |
| limited-checkpointed | off | yes | yes (`E4LIMIT1`) | 249952 | 249856 | 0 |

Total including manifests: about 3.1 MiB. The sizes are unchanged from the
pre-release corpus: the stamp occupies two bytes that were already there.

`docs/format-v2-baseline/` holds a second, independently captured corpus of
the same five shapes. The compatibility suite requires and verifies **both**,
exercising ten databases; absence or modification fails. Neither corpus may be
replaced to make a future engine pass. New release evidence is additive.

## Preservation and qualification

[core/engine/tests/format_v2_compat.rs](../../core/engine/tests/format_v2_compat.rs)
requires both corpora; missing fixtures fail qualification. It pins each
INDEX's SHA-256, checks each manifest against INDEX, checks fixture checksums
and exact file inventory, and opens only unique temporary copies. It verifies
every expected document through get and scan, normal writes plus
checkpoint/reopen without feature promotion, source immutability, and
whole-directory file-byte/inventory-preserving refusal of unknown required
bits, unsupported physical versions, foreign WAL and a disk-format stamp that
is not 2.

The post-write oracle derives from the preserved manifest plus explicitly
requested mutations, never engine readback. Numeric identity expectations are
specified separately from the immutable manifests using the original
generator's insertion order: people 1–200 with reinserted slot 7 at ID 201,
events 1–80, blobs 1–4, and the compatibility writer's new person at ID 202.
Get, numeric get, scan, update and reopened writer checks enforce those
identities, including absence of deleted/retired IDs.

The legacy generator deletes its output directory first. The explicit capture
mode in [tools/format_reference_compat.py](../../tools/format_reference_compat.py)
wraps it with fresh staging and refuses an existing destination; it pins the
pre-release reference revision and so qualifies that historical capture,
not a v2 one. Ordinary tests never invoke either generator.

Run with TMPDIR pointing to an existing isolated directory in the authorized
artifact area:

```sh
cargo test --test format_v2_compat -- --test-threads=1
cargo test --features compact-cells,sqlite-balance --test format_v2_compat -- --test-threads=1
cargo test --features compact-cells,sqlite-balance,keyspace-append,slotref-split --test format_v2_compat -- --test-threads=1
```

Mac database execution remains subject to a reboot
and independent I/O-control requirement. Its scratch path must be under a
separate scratch volume.

## Inherited kernel Store

[core/kernel/src/meta.rs](../../core/kernel/src/meta.rs) `FORMAT_VERSION` was
build-dependent (`compact-cells` → 2, else 1) and a checkpoint restamped the
superblock with the build's version. It now accepts versions 1 and 2 in every
build and writes the file's own version. That Store is not the
typed-collection release path
([core/engine/src/store/mod.rs](../../core/engine/src/store/mod.rs) selects
`PageWalStore` only). Its pages carry the disk-format stamp like any other,
because they are ordinary pages.

## History

The sections below are the record of how the envelope reached v2. They are
kept because the reasoning is the evidence: they name the commits, the
qualification runs and the owner decisions that made the byte meanings what
they are. They do not supersede anything above.

Until 2026-09-21 the envelope was called **e4-format-v1**, a named *storage
baseline* rather than a published format: engine commit
`59d1cbc770284f160ffda53cc1ee545167733d11`, declared 2026-09-16, with the
fixture corpora `docs/format-v1-fixtures` and `docs/format-v1-baseline` and the
suite `core/engine/tests/format_v1_compat.rs`. That baseline was never
published, so no file of that name was ever in a user's hands. On 2026-09-21
the owner declared the envelope as it stood to be sekejap disk format v2,
stamped it into page bytes 18-19, regenerated both corpora with the stamped
binary, and removed the pre-release corpora. The paragraphs below were
`docs/core/FORMAT_FREEZE.md`, folded here when that file was removed.

### Current boundary — 2026-09-16 (superseded by the v2 declaration above)

The named storage baseline was **e4-format-v1**, engine commit `59d1cbc`.
Existing byte meanings are frozen under Law 8.
[FORMAT_BASELINE.md](FORMAT_BASELINE.md) records its immutable
source/binaries/corpus, cross-build read/write rollback checks and future
release gate. The sections above define the actual `E4PWAL02` envelope, typed
layouts/identities, feature policy and extension rules.

The three-mode full Linux qualification passed; the original fixtures were
unchanged. A second, committed-reference corpus was separately pinned and
mandatory. The preserved default engine binary and compact/retained builds
passed 20 unchanged-feature upgrade/rollback arms, including pending WAL.
This is an honest cross-build baseline; later public versions did not yet
exist. Every later release must test against retained bytes and executables.

The [recovery runbook](../PHASE1_RECOVERY_RUNBOOK.md) identifies supported
salvage, current-versus-candidate evidence, source preservation and explicit
limits. Its actual PageWal repair CLI also passed a clean committed-WAL smoke.

The final evidence is verified and Phase 1 storage-shape stabilization is
complete; [PHASE1_STATE.md](../PHASE1_STATE.md) records execution status. It
does not claim all-eight-law product qualification or a public deployment.
SQL, EXPORT/IMPORT, multimodel indexes/adapters and release packaging remain
product work. New index families require noncolliding namespaces, explicit
encoding/catalog versions/features and immutable compatibility fixtures before
they ship. They must preserve the frozen entity representation. Coordination
files remain part of the locking/publication protocol and cannot be unlinked
while live.

### Historical direction and evidence — 2026-09-15

The remainder retains the earlier decisions and measured evidence for their
named source revisions. Historical status and proposed release sequencing do
not supersede the boundary above or the adopted Law 8 contract.

The owner's priority was a durable disk-format contract that lets users
upgrade without exporting and reimporting their databases. Do not make every
performance target a prerequisite for starting interface work. The first seven
laws retain their wording; the owner added Law 8 in CONTRACT.md. A format
freeze is not proof of production safety.

Status at the time: **shape established; compatibility promise not yet
qualified**. That document recorded the direction, not a declaration of a
frozen format.

#### Owner requirements clarified after the pause

- First consumers are **downstream applications**. Device-specific research can wait;
  target-device support remains in scope, but device benchmark convergence is not
  the immediate release objective.
- The product goal is strong **combined multimodel SELECT queries**, not
  matching SQLite's insert/update speed. About 1.75× SQLite write time can be
  acceptable when demonstrated query benefits justify it. This is conditional
  acceptance, not an unmeasured claim that sekejap already has those benefits.
- Prefer a stable disk format across binary upgrades. Provide explicit
  **EXPORT / IMPORT** commands as well; routine upgrades should not depend on
  manual export/import. Their lossless scope must include identities, schemas,
  relationships, typed values, timestamp policy and index definitions.
- Preserve the interface direction the prior engine set: SQL, embedded/library APIs, CLI,
  language/device bindings and network adapters. Do not reduce the product
  scope to the current raw-KV prototype or one interface.
- The format contract includes persisted **index encodings and metadata**,
  not just pages and entity values. Law 8 makes release compatibility a
  requirement; Laws 1–7 and the timestamp default retain their wording/policy.
  Safety, bounded disk use and honest cost reporting still apply.

The earlier SQLite 1.5× write threshold is no longer an unconditional release
veto. The reservation candidate was reverted under that earlier criterion;
this clarification does not automatically restore it or prove its disk cap.
Its measured tradeoff remains available for the integrated product decision.

Prior-engine source inspection: `core/kernel/src/keys.rs` defines graph,
property, text, spatial and vector-navigation keyspaces; `README.md`,
`wrappers/README.md` and `docs/usage/connectivity.md` describe the interface
surface. Wrapper presence/documentation is not fresh proof of every platform's
working status. The collection tags here overlap the prior engine's meanings
(for example the collection-name tag `0x10` versus its geometry tag `0x10`).
Reuse algorithms/interfaces through an explicit namespace mapping, never copy
these persistent tags blindly.

### Historical shape and candidate-r3 boundary

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
  [V2_FOUNDATION_LOOP.md](V2_FOUNDATION_LOOP.md). An earlier snapshot of this
  same candidate line (r2) was diagnostic only; r3 is the source actually
  tested and retained; the integration commit records this tested source
  without a co-author. The decision was a **functional-improvement retention
  for continued development** — not raw-write-optimization acceptance, not a
  stable disk-format freeze, and not a production release. The owner
  subsequently accepted disk behavior against SQLite: sampled peak allocated
  bytes are +0.70% with timestamps off and +4.18% with timestamps on; final
  logical size is +11–12%. A separate 2x physical-space ceiling is not a
  blocker for this loop. Allocated and logical peaks are both recorded, and
  `ResourceLimits` still does not guarantee a hard allocated-block
  reservation.
- The LOGICAL index feature word is a separate, additive register from the
  physical `E4PWAL02` bits, and its one definition is
  `SUPPORTED_LOGICAL_FEATURES` in `core/engine/src/collections/mod.rs`, with
  the bit-by-bit table in the doc comment above it. It stood at `0xfff`, became
  `0x1fff` on 2026-09-21 when `COLUMN_RULES_FEATURE = 0x1000` was added for
  the per-field COLUMN RULE tail (`DEFAULT`, `NOT NULL`), and is `0x7fff` from
  2026-09-21, when `ROW_COUNT_FEATURE = 0x2000` was added for the LIVE ROW
  COUNT records of `core/engine/src/collections/row_count.rs` (key tag `0x08`,
  one 16-byte record per collection: `rows: u64 BE || generation: u64 BE`).
  `0x4000` is RESERVED for the parallel semi-join item and is deliberately NOT
  implemented by this build, so a file that declares it is refused here too.
  It is `0x17fff` from 2026-09-22, when `JSON_EXPRESSION_FEATURE = 0x10000`
  was added for the JSON-PATH expression index (descriptor version 4,
  `IndexExpr::JsonText`); `0x8000` is RESERVED for the graph vector family and
  is not implemented here, so the mask has a hole in it and a file declaring
  that bit is refused by this build. Every bit in it is
  the bit-by-bit table in the doc comment above it. It stood at `0xfff`, then
  `0x1fff` from 2026-09-21 when `COLUMN_RULES_FEATURE = 0x1000` was added for
  the per-field COLUMN RULE tail (`DEFAULT`, `NOT NULL`), and is `0x7fff` from
  2026-09-21 with `ENDPOINT_FEATURE = 0x4000`, the graph ENDPOINT SETS
  (`core/engine/src/index/graph/endpoints.rs`). `0x2000` is deliberately
  unimplemented in this mask and is reserved for the live per-collection row
  count, so a file that declares it is refused by this build exactly as any
  other unknown bit is. `JSON_EXPRESSION_FEATURE = 0x10000` (2026-09-22) took
  it to `0x17fff`, leaving `0x8000` reserved for the graph vector family.
  Every bit in it is
  additive: set in the same transaction as the first record that needs it,
  never cleared, and a file declaring a bit outside the mask is refused as
  `Unsupported` at admission, before a record is read (Law 8). Adding a bit is
  the only permitted change to the on-disk format.
- `E4PWAL02` persists required feature bits, including compact cells.
  Since the loop-2 codec change (2026-09-16) the cell encoding is a property
  of the database, not of the build: every build decodes both cell families, a
  writer encodes new cells in the family the header declares, and opening
  never changes the declared bits. The `compact-cells` cargo feature only
  selects the default for databases created by that build. Header validation
  refuses any required bit outside the set this release implements before any
  byte is changed.
- A reference-fixture pass (archive
  `b54d8f8e3a5fc71da6c667145153560bd82075a0713c1f253891127977aeb4d9`) built a
  small database with the accepted `64b6663` `Database` (old typed encoder
  over the old `Store`, since that commit predates the integration) and
  streamed its logical KV pairs into a fresh `PageWalStore` via a helper,
  not through candidate r3's integrated writer above. It is bounded
  preparatory evidence that the typed encoding survives moving into the
  page-WAL container, not proof of the candidate writer's byte-for-byte
  compatibility, and not a released or frozen baseline. The compatibility
  suite against candidate r3 reported four passing checks — `compat-new`,
  `compat-old-raw`, `compat-old-typed`, `compat-old-typed-wal` — all EXIT0,
  with the original fixture hashes unchanged. See
  [V2_COMPAT_FIXTURES.md](V2_COMPAT_FIXTURES.md).

### Historical work outline before promising compatibility

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
   candidate r3**, tested on Linux and retained for continued development
   ([V2_COLLECTION_INTEGRATION.md](V2_COLLECTION_INTEGRATION.md); lean
   coverage in `docs/FOUNDATION_LEAN_GROUPS.json`'s `typed` group; full
   evidence in [V2_FOUNDATION_LOOP.md](V2_FOUNDATION_LOOP.md)) — a
   functional-improvement retention, not a stable format freeze or production
   release; the accepted `64b6663` commit itself does not contain this
   integration. Map the prior engine's index families and interface semantics onto this
   path; qualify representative persisted indexes before declaring the
   combined format frozen. Do not claim raw-KV or single-process typed-CRUD
   tests cover indexed collections, cross-process concurrency at scale, or
   released-fixture compatibility.
4. Preserve immutable databases written by a frozen reference binary. Test a
   newer binary reading, updating and reopening them, including committed WAL
   awaiting checkpoint, overflow values, earlier layout versions and indexes.
   Compare against independent expected entities and preserve the source
   fixtures. Same-build round trips alone do not prove upgrade compatibility.

### Format-neutral improvements and compatibility direction

Cache policy, syscall batching, disk reservation, page placement, split/merge
policy and packing within existing supported cell encodings can improve without
changing how existing bytes are interpreted. New indexes can use allocated
key tags and catalog descriptors rather than changing entity encodings.
This is an architectural allowance, not proof that every future optimization
or recovery solution is format-neutral.

The adopted upgrade promise is permanent read/write compatibility without a
mandatory full rewrite. New binary versions need not imply new disk versions.
Optional new encodings require explicit feature creation/conversion while
preserving support for old encodings. Law 8 also requires minor-version
rollback tests within an unchanged feature set; older binaries may refuse
explicitly enabled newer features.

### Historical reservation evaluation

The reservation evaluation finished: 96 comparison arms verified, and the
candidate was reverted under the former timing criterion. Its source patch and
measurements are retained in `RESERVATION_LOOP.md` and linked artifacts.
Do not start another broad raw-write optimization search before resolving the
format/interface scope above. Establish representative consumer-application mixed
queries as the product benchmark before claiming the conditional write/query
tradeoff has been earned.
