# Phase 1 completion audit — 2026-09-16

Scope: storage format `e4-format-v1`, frozen source baseline `59d1cbc`, and
completion of the current storage phase. This audit does not require a public
product release before a storage baseline can be declared. No runtime edits or
new performance experiments were made for this audit.

**No additional persistent-format blocker was found.** The named baseline,
immutable binary/corpus capture, cross-binary proof and recovery CLI handoff
are now recorded in [FORMAT_BASELINE.md](FORMAT_BASELINE.md). Public packaging
and future multimodel implementations are separate work; existing Law 8
promises remain intact.

## What is already established

| Requirement | Verified source/evidence |
|---|---|
| One selected collection backend and explicit envelope | [FORMAT_V1.md](FORMAT_V1.md), [collection_backend.rs](../src/collection_backend.rs), [pagewal/format.rs](../src/pagewal/format.rs): `E4PWAL02`, 4096-byte physical v1 pages, 4144-byte WAL frames, database identity, transaction history and required features. |
| Existing-file codec/feature preservation | Database-installed codec on open/rollback; unchanged features on commits. Both cell families are readable in every build. [pagewal.rs](../src/pagewal.rs), [cell codec test](../tests/pagewal_cell_codec.rs), [creation test](../tests/pagewal_create_codec.rs). |
| Unsupported-version refusal before mutation | [Replica refusal tests](../tests/format_replica_refusal.rs), [compatibility tests](../tests/format_v1_compat.rs); inherited logical admission precedes version-specific parsing. Broken-code controls failed as intended. |
| Exact entity and identity compatibility oracle | Mandatory preserved candidate corpus; document, numeric-ID, scan, delete/reinsert and complete post-write/reopen checks. [FORMAT_V1.md](FORMAT_V1.md), [compatibility test](../tests/format_v1_compat.rs). |
| Linux qualification | [Qualification report](PHASE1_QUALIFICATION.md) and [target counts](phase1-evidence/test-results.json): default 422/0/2, compact/balance 427/0/2, retained features 427/0/2 (pass/fail/ignored). Raw logs preserve the failures and corrections. |
| Scoped recovery semantics | [Recovery runbook](PHASE1_RECOVERY_RUNBOOK.md), [raw repair test](../tests/pagewal_repair.rs), typed `rootless_collection_recovery_preserves_source_and_reports_vector_limit` in [collections.rs](../src/collections.rs). Both tests pass in all three retained Linux logs. |

## Completion evidence reviewed

1. **Named immutable baseline captured.** `e4-format-v1` is bound to engine
   `59d1cbc770284f160ffda53cc1ee545167733d11`. The clean source archive, frozen
   generator, repair binary and three separately hashed compatibility binaries
   are retained. The new five-database/66-file corpus is separate from the
   original prototype corpus. [Provenance](format-baseline-evidence/PROVENANCE.json)
   records toolchain, target, generator features and file hashes.
2. **Twenty cross-binary arms passed.** The independently reviewed
   [compact report](format-baseline-evidence/reports/compact.json) and
   [retained report](format-baseline-evidence/reports/retained.json) each contain
   ten PASS arms, 52 successful commands and `source_unchanged: true`. Both
   codecs, committed-WAL and checkpointed boundaries are covered. The current
   binary writes before the frozen baseline reads and writes; current then
   verifies the changed population. The mutator does not reopen after its
   final commit, so the next binary is first to read/recover that boundary.
   Expected documents, identities, deletes, schema and resource policy derive
   from pinned manifests plus explicit mutations, not readback. A pinned
   snapshot's complete old population is checked in pending-WAL arms.
3. **Both corpora are mandatory in ordinary tests.** The seven compatibility
   tests pass in [default](format-baseline-evidence/logs/fixtures-default.log),
   [compact](format-baseline-evidence/logs/fixtures-compact.log) and
   [retained](format-baseline-evidence/logs/fixtures-retained.log) modes.
   The source fixture is never passed to a writer; unique external copies are
   used and corpus hashes are rechecked. The original engine/runtime sources
   remain unchanged by this completion work.
4. **Recovery command handoff passed.** The retained repair binary produced
   606 verified current raw KV entries, zero uncertain candidates and no named
   losses/extents from a clean pending-WAL fixture. The
   [smoke report](format-baseline-evidence/repair-smoke.json) records exit 0,
   completion and unchanged full source file hashes. This is operational CLI
   coverage, not complete typed restoration or additional damaged-data proof.
   The [runbook](PHASE1_RECOVERY_RUNBOOK.md) states those limits.

The exported artifact SHA-256 was independently checked as
`253a8baa88ba9a770d36dec44ce8bb8ca86a731e351183028e9666aabea23a9f`;
the original exported evidence archive as
`a0e5930c10b7e08122560ebb543341a6f241b7320ee63433ef77030f845f53d8`.
The baseline executable hash is
`18d251f43f744c0bbf4cd22a1b35b7a037c9b94ef76d1d17f3547457bfe57aff`.
These are distinct builds of the same engine revision, correctly described as
cross-build evidence. They provide the permanent frozen side for later engine
version tests; they do not manufacture evidence from a nonexistent later
public release. Later versions must use the actual preserved baseline binary
and retain existing corpus expectations.

## Final reusable-driver review

The [driver](../tools/format_reference_compat.py) requires the pinned INDEX,
checks exact corpus file hashes, compares distinct executable hashes, copies
fixtures outside the corpus and records each process command. Its future
workflow is documented in FORMAT_BASELINE. Future build provenance should
include the actual current source revision and feature set, alongside the
executable hash, rather than relying solely on a compiled revision label.

Review found one tooling defect unrelated to database runtime: lexical work
path checks could be bypassed by symlink/`..` parents, and parent directories
were created before source-containment rejection. The driver now resolves
paths before checks and creates no parent until the work path is outside the
corpus. [Path-only regressions](../tools/test_format_reference_paths.py)
failed for all three spellings on the preserved old driver and pass for all
three on the final driver; no databases or executables were used locally.
The [hash manifest](format-baseline-evidence/path-regression-manifest.json)
and [red](format-baseline-evidence/path-regression-red.log)/
[green](format-baseline-evidence/path-regression-green.log) logs preserve this
control. The final driver hash is
`336ad99ad4c4a32b8a7263698a9aaf88d4e44de3e8abc9b9af790624b37a6984`.
The final driver was then replayed on Linux: all twenty arms passed again,
with 52 successful commands and unchanged source per comparison build. See
[final compact report](format-baseline-evidence/reports/final-compact.json)
and [final retained report](format-baseline-evidence/reports/final-retained.json).
The exported [final driver](format-baseline-evidence/final-driver.py) is
byte-identical to the repository driver. Additional final-driver evidence archive
SHA-256: `564a93c218b3b1b56b265d3e520d9abd18a27586810a4f1b498cd992e15dcef3`.
The preserved binaries and engine bytes did not change. This closes the final
tooling check; no Phase 1 format-completion blocker remains in this audit.

## Recovery limits and their effect on the format decision

The [recovery contract](RECOVERY_CONTRACT.md) explicitly distinguishes current
data, uncertain candidates, known affected keys and unknown damaged extents.
The selected implementation follows that boundary: trusted metadata and tree
paths establish current raw-KV membership; damaged roots/both metadata copies
may leave only candidates; a complete corrupt WAL conservatively refuses raw
repair. No code infers that a surviving old value overrides a deleted/newer
one. [pagewal/repair.rs](../src/pagewal/repair.rs) and its fault test are the
current authority; older inherited-Store recovery matrix successes are not
proof of page-WAL corrupt-region salvage.

Typed candidate recovery discovers layout copies independently and retains raw
rows when decoding fails. It does not reconstruct vector sidecars into decoded
documents or establish current membership, and catalog names/policies remain
separate evidence. The generic CLI's `recover schema` selects the older
`DenseV3` key classifier, not `CollectionRecovery`; it must not be advertised
as the collection recovery command. See the runbook for the supported split.

These are real recovery/product limitations, **not evidence that today's byte
layout must change**. Phase 1 accepts the documented conservative classifications
and preserves evidence; it does not claim full Law 5 qualification. Full
rootless membership reconstruction, corrupt-WAL-region salvage, typed restore,
aggregate repair budgets and the full destination-failure/publication matrix
remain later recovery work. If a future design introduces new persistent
evidence, it must be explicitly enabled and preserve reading/writing the frozen
baseline; the format promise cannot be withdrawn to make that work easier.

No reserved-header expansion, hypothetical index encoding, new insertion
benchmark or E3 migration is required to finish this phase. Future real graph,
spatial, text and vector-navigation formats receive noncolliding namespaces,
explicit catalog/encoding versions and required features before publication,
plus permanent fixtures when shipped. EXPORT/IMPORT, SQL/adapters and public
release packaging remain required product work under [CONTRACT.md](../CONTRACT.md),
without becoming a circular prerequisite for naming the storage baseline.
