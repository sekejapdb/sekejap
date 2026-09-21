# Phase 2 admission review — 2026-09-16

Read-only review of current `src/collections.rs`, `src/collection_backend.rs`, `src/pagewal.rs`, `src/pagewal/format.rs`, `docs/core/FORMAT_V1.md`, the two-corpus compatibility tests and Law 8. No source or tracker changes made.

## Recommendation (revised after parent requirement clarification)

Use a **versioned typed header `E4COLL2\0`**, atomically introduced only by an explicit first `create_index` operation. Preserve `E4COLL1\0` and its exact existing payload for every database that has not explicitly enabled indexing. This supports late indexing of existing Phase-1 databases without copying their primary data or making creation-time capability choice permanent.

Frozen `parse_header` already recognizes intact packets starting with `E4COLL` whose magic differs from `E4COLL1\0` and returns Unsupported. `replicas` makes even one intact unsupported header replica authoritative. Typed writer admission checks this before WAL normalization or coordination-file creation. Thus the existing Phase-1 typed engine refuses an indexed database before mutation, although its physical pages and entity record encodings remain unchanged.

The COLL2 payload should carry explicit logical required-feature bits/envelope version, the existing next-collection/next-layout/resource-policy values, and index registry metadata such as next-index ID and live descriptor count (or a separately replicated registry reference). Reserve exact meanings and validate supported bits; do not infer index capability just from presence of arbitrary keys. Incremental creation must write all three header copies, all descriptor copies, registry accounting, and the complete READY index state in one atomic transaction, or use an explicit versioned BUILDING lifecycle. Keep COLL2 even after the last index is dropped; no implicit downgrade.

New engines continue reading and writing COLL1 and COLL2. Ordinary open/CRUD/checkpoint never promotes COLL1. An explicit failed or rolled-back create_index must leave the prior committed COLL1 state authoritative. This is an index/catalog opt-in, not an automatic engine-update conversion.

### Boundary: typed API versus raw byte-KV tooling

The raw `PageWalStore` API can bypass typed row/catalog invariants today. It understands unchanged physical bytes and will also be able to overwrite keys in COLL2 files; the logical header does not protect arbitrary raw writers. State clearly that opening/mutating a typed database through raw KV APIs is outside typed Database guarantees. Typed application entry points, repair/rebuild/restore tools that write typed databases, and any public dispatcher must use typed admission and must never silently fall back to raw writing after refusal. Read-only forensic export is a distinct operation and must preserve/report unknown typed metadata rather than downgrade it.

If the product later promises refusal for **every raw writer binary**, logical admission is insufficient and a physical required-feature promotion protocol is needed. That stronger promise should not be implied by tests only against typed APIs. This review recommends the typed boundary for this loop because it meets explicit late-index creation without an unnecessary physical format rewrite.

### Why not mutate physical feature bits?

Physical features are currently immutable: `disk_header` rejects two valid copies whose masks disagree; `Pager::inspect_bounded` rejects a WAL header whose mask differs from checkpoint metadata; rollback also requires features unchanged. Setting a new bit in a WAL transaction fails this model. Rewriting both metadata pages creates mixed-feature/crash and live-reader hazards. Leave those checks untouched.

A physical bit set on fresh database creation would guard frozen raw tools too, but would constrain existing Phase-1 files to copying/conversion or require a separate promotion design. It is not necessary for the typed admission contract above. Merely adding namespace keys without either COLL2 or physical refusal would be unsafe: frozen typed writers ignore unknown keys and could maintain entities while leaving indexes stale.

## Smallest catalog contract

1. Reserve currently unused namespaces only after complete registry audit. Current families are `[00,00,copy]` global header; `[00,F0]+BE64(...)` layouts; `[01,copy]+ordered(collection)` collection descriptors; `[02,copy]+ordered(collection)` counters; `10` names; `20` external-key mappings; `40` rows; `60` vector payloads. New values such as `03` for index metadata and high tags for entries are possible; this report does not freeze numerical allocation. Do not copy E3 tags unexamined.
2. Add independently checksummed/padded descriptor replicas, using the existing three-copy metadata idiom. Include descriptor-envelope version, stable index ID, collection ID, immutable family code, family encoding version, semantic options, field names/types/layout-binding policy, and persisted lifecycle state. Key every entry by **index identity before item/value identity**, following CONTRACT.md's index scope rule. Family-specific parameter bytes must be canonical binary, not persisted JSON text.
3. Have an independently replicated catalog root/registry that detects missing descriptor sets (e.g. next index ID plus live descriptor count/identity registry). Scanning descriptors and accepting whatever survives is unsafe: losing all copies of one descriptor must not silently disable maintenance of that index. Stream catalog validation with a documented metadata/resource bound; do not load unbounded corpus-sized state at open.
4. Validate family/version/options/lifecycle combinations during both `typed_check` writer admission and snapshot admission, before `finish_open` normalizes WAL or creates coordination files. Do not defer unsupported-family refusal until the first indexed query or next write.
5. Validation must also run after rollback before clearing the failed state. Reuse existing unsupported-vs-corruption distinction: an intact unsupported replica refuses immediately; valid conflicting copies refuse; a genuinely corrupt copy may lose to an independent intact copy. Every header/descriptor copy must be examined, not only the first usable one.
6. A COLL2 file must contain valid index registry accounting even when empty. A COLL1 file has no catalog requirement, no added keys, and no opportunistic mask/header conversion during ordinary create/open/CRUD/checkpoint.
7. New engines retain read/write support for every shipped descriptor and family encoding. Unknown encodings remain refusal cases. Adding a variant requires explicit index creation or explicit conversion, independent of the engine binary version.

## Atomicity and recovery

All descriptor/root changes and their associated index-entry changes must use the same existing PageWalStore transaction as the entity changes, with the same `finish` poison-on-error discipline. No index-side autocommit. A failed multi-key update must not let commit publish a partial index state. Rollback clears index caches as well as existing collection/layout caches, reloads supported catalog state, and preserves existing snapshots.

Index build must not mark READY before complete entries and its publication marker are durable together. A corpus larger than the bounded transaction/WAL requires a versioned BUILDING lifecycle and incremental batches; queries must explicitly exclude incomplete indexes, and writes during build need a defined maintenance/replay mechanism. For a lean first API it is safe to support index creation only on an empty collection until the full existing-data build protocol is implemented, but do not misrepresent that as completed CREATE INDEX over existing data.

Derived-index corruption must never lead to automatic write-open/rebuild changing source files. Refuse or expose an explicit diagnostic/read-only path, preserve raw evidence, and make repair/rebuild an explicit operation. Primary graph edges are **not** disposable derived indexes; retain their durable canonical records independently from adjacency/navigation structures. Similarly vector payload namespace 60 is primary data, distinct from vector search structures.

The frozen raw repair binary may still inspect physical records because physical features are unchanged. Its output must not be presented as an index-consistent repaired typed database without new typed validation. New repair code must understand COLL2 and preserve unknown/corrupt catalog bytes honestly; no stripping COLL2 to force a typed open. Keep entity recoverability independent of index decoding wherever the existing raw/salvage interfaces permit, without weakening ordinary admission.

## Required evidence before accepting the envelope

* Existing default/compact/retained two-corpus compatibility suite and old-binary rollback matrix still pass on ordinary files. Preserve corpus hashes; ordinary CRUD/checkpoint keeps physical masks and existing typed header bytes/meaning.
* Explicit first-index creation covers existing COLL1 databases with compact OFF/ON and limits OFF/ON. COLL2 retains resource-policy semantics. Reopen and snapshot use declared options independent of build defaults.
* Preserved old typed binaries refuse COLL2 files, both checkpointed and WAL-pending, with whole-directory bytes/inventory unchanged. Explicitly document that raw byte-KV tools have a different invariant boundary.
* Future envelope/family versions, unknown family code, unsupported options, either/all descriptor replicas, valid conflicts and missing-all descriptor set refuse before mutation. Include torn/uncommitted WAL tail to ensure admission failure does not normalize it.
* Failure injection around descriptor copies/catalog root/entries/entity updates proves no partial publication. Snapshot opened before catalog commit keeps old view; one admitted after gets all new metadata and entries.
* Explicit first create_index on COLL1 publishes header/descriptor/entries together and survives checkpoint/reopen. Failed build/rollback leaves the previous committed header intact. Ordinary CRUD/checkpoint/rollback does not independently promote a header. Physical feature masks never change.

These are design recommendations, not verified new implementation or an all-laws pass. The smallest safe release is a truthful explicit index opt-in/admission envelope plus one fully maintained index family; later families extend descriptors without changing frozen primary data bytes.
