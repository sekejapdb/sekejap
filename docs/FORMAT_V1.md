# Format v1

Candidate name: **e4-format-v1** for the V2/page-WAL storage line. This is
not an owner-declared release format yet. Phase 1 candidate qualification **passed on Linux**; [PHASE1_STATE.md](PHASE1_STATE.md) records the current work and
Linux evidence and remaining release requirements. Law 8 in [CONTRACT.md](../CONTRACT.md) is the
policy authority (the adopted text comes from the main worktree). Neither
same-build round trips nor prototype fixtures establish released compatibility.

## Actual candidate envelope

The typed collection path selects `PageWalStore` in
[src/collection_backend.rs](../src/collection_backend.rs). Its physical page
version, page-WAL magic, required feature bits and typed metadata versions are
separate identifiers; the candidate name does not replace these checks.

| Unit | Current representation and source |
|---|---|
| Physical page | 4096 bytes; magic `0x53454B32`; little-endian u16 version 1 at offset 4; kind, tree ID, slot count, page number, free pointer, sibling/child link, generation and CRC32C. CRC excludes its own bytes 36–39. Slot directory starts at 40. [kernel/src/page.rs](../kernel/src/page.rs), `PageMut::init`, `PageRef::open`, `seal`. |
| Metadata pages | Data pages 0 and 1 contain a 56-byte `E4PWAL02` slot: magic at 0, root u32 at 8, free-head u32 at 12, logical allowance u64 at 16, 16-byte database identity at 24, transaction u64 at 40, required features u64 at 48. [src/pagewal/format.rs](../src/pagewal/format.rs), `Header`, `disk_header`. |
| WAL frame | **4144 bytes**: 32-byte frame header, 4096-byte payload, 16-byte database identity. Header: `E4PWAL02` at 0, kind u32 at 8, page number u32 at 12, transaction u64 at 16, committed page count u32 at 24, CRC32C at 28. CRC covers the entire frame with its checksum field zeroed. [src/pagewal.rs](../src/pagewal.rs), `frame`, `read_frame`. |
| WAL publication | Kind 1 carries a page image; kind 2 commits a transaction and carries the running page-frame CRC in the payload's first u32. Inspection requires transaction sequence, database identity, extent and checkpoint-history agreement, including transaction metadata; only a verified committed prefix is published. [src/pagewal.rs](../src/pagewal.rs), `Pager::inspect_bounded`. |
| B-tree cells | Ordinary and compact leaf cells, interior child pointers and overflow references/chains retain their encodings. Every build reads both supported cell families. [kernel/src/btree.rs](../kernel/src/btree.rs), record encoders/decoders; structural checks in [kernel/src/verify.rs](../kernel/src/verify.rs). |
| Typed records | Dense-v3 positional records with immutable layout IDs, binary JSON, scalar/point values and separate f32 vector rows. Catalog/layout/counter packets retain independent copies. [src/dense_v3.rs](../src/dense_v3.rs), [src/collections.rs](../src/collections.rs), [COLLECTIONS.md](COLLECTIONS.md). |
| Typed resource policy | Ordinary collection header payload is 8 bytes; `create_limited` appends the kernel's 56-byte `E4LIMIT1` record, giving 64 bytes. Other lengths are unsupported. [src/collections.rs](../src/collections.rs), `HEADER_PLAIN`, `HEADER_LIMITED`, header codec. |

Required feature bit 0 is `COMPACT_CELLS`; the supported set is currently just
that bit. Writers adopt the file's declared set and ordinary writes preserve
it. The Cargo feature selects a creation default, not which existing family
can be read or written. Unknown required bits and physical page versions must
be refused before recovery or file mutation; the focused tests exercise
checksum-valid unsupported metadata, including either physical metadata copy.
The broader refusal review and all three Linux build-mode suites passed; see
[PHASE1_QUALIFICATION.md](PHASE1_QUALIFICATION.md).

Page bytes 18–19 and 32–35 are reserved and currently initialized to zero.
They stay zero under the current format; they are not free space for an
unannounced optimization. Bytes 24–31 already hold the publishing generation
written by `seal`; the older introductory comment calling them reserved does
not override the implementation. Any future header meaning requires an
explicit version/required-feature design that preserves old readers' safe
refusal and newer readers' and writers' support for shipped encodings.

`readers.lock` contains two 48-byte `E4PWHNT1` publication hints. Eight
`reader-N.lock` files and `writer.lock` participate in advisory locking. These
are derived coordination state, not entity/catalog payload, but they still
form a concurrency protocol. Reconstructibility does **not** make live deletion
or replacement safe: lock identity, admission and publication ordering must be
preserved. Rebuild only through the engine's coordinated/quiescent path; future
protocol changes require separate mixed-version concurrency analysis. See
[src/pagewal.rs](../src/pagewal.rs), coordination and snapshot admission code.

## Extension boundary

Current key allocation includes collection metadata/replicas, layout keys,
collection names (`0x10`), external-key mappings (`0x20`), entity rows (`0x40`)
and vector payloads (`0x60`) in [src/collections.rs](../src/collections.rs).
Future graph, spatial, text and vector-navigation indexes must allocate new,
noncolliding namespaces after auditing the complete registry; E3 tag values
cannot be copied blindly. Each persisted index family needs explicit encoding
version/catalog descriptors and any required feature declaration before it
ships. Enabling a new representation is explicit; ordinary upgrades may not
silently convert a database or force an index rebuild. All shipped encodings
remain readable **and writable** by newer releases under Law 8. Phase 1 defines
this extension policy; it neither implements those indexes nor claims their
compatibility has been tested. Add immutable fixtures when each family ships.

## Generator provenance

Written by `src/bin/format_fixture.rs` through the public `collections::Database` API over `PageWalStore` (not the old KV-projection helper).

- git HEAD: `d789fc2aa54bc0198417b696ac0b7c3a33902034`
- `git diff HEAD` sha256: `72d8e706028d816b4f1df0463cd4efb7a586dfed1b9eea12c7f3ca1b38f09dad`

`Database` / `PageWalStore` do not checkpoint on drop. A wal-pending fixture is `commit()` then drop with no `checkpoint()`.

## Fixture matrix

Path: `docs/format-v1-fixtures/`. Each directory has engine files plus `MANIFEST.json` (expected entities regenerated from the content function, not read back). `INDEX.json` lists the set. Every fixture holds the same logical content: 200 people (timestamps off; nullable real; point; binary JSON; non-ASCII; one deleted key; one deleted-then-reinserted key), 80 events (timestamps on; layout 2 then alter to layout 3 so both layouts coexist), 4 blobs (1536-lane vector and JSON > 4 KiB). Live entity count: 283.

| fixture | compact cells | checkpoint | limited | engine bytes | data | wal |
|---|---|---|---|---:|---:|---:|
| compact-off-checkpointed | off | yes | no | 249952 | 249856 | 0 |
| compact-off-wal-pending | off | no | no | 522144 | 8192 | 513856 |
| compact-on-checkpointed | on | yes | no | 249952 | 249856 | 0 |
| compact-on-wal-pending | on | no | no | 522144 | 8192 | 513856 |
| limited-checkpointed | off | yes | yes (`E4LIMIT1`) | 249952 | 249856 | 0 |

Total including manifests: about 3.1 MiB.

## Preservation and qualification

The five existing fixtures are immutable **codec-only candidate-build
evidence**, preserved alongside their original manifests and provenance.
Never regenerate or replace them in place, including when the owner selects a
release binary. Capture that binary's fixtures in a **new** versioned corpus
and retain both sets. The generator deletes its output directory first; use it
only against a fresh, explicitly chosen artifact directory, never the
preserved corpus. Ordinary tests must not invoke it.

[tests/format_v1_compat.rs](../tests/format_v1_compat.rs) requires the corpus;
missing fixtures fail qualification. It pins INDEX's SHA-256, checks each
manifest against INDEX, checks fixture checksums and exact file inventory,
and opens only unique temporary copies. It verifies every expected document
through get and scan, normal writes plus checkpoint/reopen without feature
promotion, source immutability, and whole-directory file-byte/inventory-preserving refusal of unknown
required bits, unsupported physical versions and foreign WAL.

The post-write oracle derives from the preserved manifest plus explicitly
requested mutations, never engine readback. Numeric identity expectations are
specified separately from the immutable manifests using the original generator's
insertion order: people 1–200 with reinserted slot 7 at ID 201, events 1–80,
blobs 1–4, and the compatibility writer's new person at ID 202. Get, numeric
get, scan, update and reopened writer checks enforce those identities, including
absence of deleted/retired IDs. This is candidate-corpus evidence, not a
released-binary upgrade proof. No persisted index-family or released-binary
rollback corpus exists yet. Full Linux workspace and feature-mode qualification is recorded separately
in [PHASE1_STATE.md](PHASE1_STATE.md); the three default, compact/balance and retained-feature runs passed
with their pre-existing ignored tests explicitly recorded.

Run on Linux with TMPDIR pointing to an existing isolated directory in the
authorized server artifact area:

```sh
cargo test --test format_v1_compat -- --test-threads=1
cargo test --features compact-cells,sqlite-balance --test format_v1_compat -- --test-threads=1
cargo test --features compact-cells,sqlite-balance,keyspace-append,slotref-split --test format_v1_compat -- --test-threads=1
```

Mac database execution remains subject to [AGENTS.md](../AGENTS.md)'s reboot
and independent I/O-control requirement. Its scratch path must be under
`<scratch>/`.

## Inherited kernel Store

`kernel/src/meta.rs` `FORMAT_VERSION` was build-dependent (`compact-cells` → 2, else 1) and a checkpoint restamped the superblock with the build's version. It now accepts versions 1 and 2 in every build and writes the file's own version. That Store is not the typed-collection release path (`src/collection_backend.rs` selects `PageWalStore` only).
