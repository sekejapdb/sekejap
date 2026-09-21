# Phase 1 recovery runbook — e4-format-v1

> Superseded 2026-09-21: the envelope named `e4-format-v1` here is now **sekejap disk format v2** ([core/FORMAT_V2.md](core/FORMAT_V2.md)), stamped into page bytes 18-19. e4 was never published; this document is the record of that pre-release baseline.

Applies to the `59d1cbc` storage baseline: typed collections over `E4PWAL02`.
Use source-preserving salvage into a new destination. Successful salvage is
not automatic authorization to replace the source or declare every entity
recovered. Format rules: [core/FORMAT_V2.md](core/FORMAT_V2.md). Outcome definitions:
[RECOVERY_CONTRACT.md](RECOVERY_CONTRACT.md).

## Preserve the incident

Stop applications using the database and keep them stopped while copying or
recovering it. Preserve the entire directory, including `data`, `wal`,
`writer.lock` and coordination files, before ordinary writer reopen. A normal
writer opener may truncate an uncommitted WAL tail and rewrite publication
hints. Do not delete the WAL, checkpoints or reader files to make an open pass.
Never replace/unlink a live advisory-lock inode.

Record the binary SHA-256, source revision, feature set, original error and a
recursive file inventory with byte sizes/SHA-256. Work on an independent copy
in the authorized artifact area; preserve the original and its inventory.
The destination must be absent, its parent must exist, and it must not overlap
the source. Allow disk space for the source copy, output database, candidates
and reports: there is no aggregate repair disk-budget guarantee.

## Raw page-WAL salvage command

The existing [pagewal_repair binary](../src/bin/pagewal_repair.rs) directly
invokes [pagewal::recover_to](../src/pagewal/repair.rs). Given a retained binary
directory `BIN`, quiescent independent database copy `COPY`, nonexistent output
path `NEW_DEST`, and separate report path `REPORT`:

```sh
"$BIN/pagewal_repair" "$COPY" "$NEW_DEST" 16777216 > "$REPORT"
```

The final argument is the maximum materialized value size, **1–16 MiB**. Values
above the selected limit are reported as affected keys; do not interpret a
low limit as evidence of physical corruption. The output cache is 64 KiB and
the source WAL allowance is bounded at 16 MiB. These are individual limits,
not a proven whole-process or total-disk cap.

The command owns the existing writer lock, reads source data/WAL through
read-only handles, and never invokes the normal writer opener on the source.
It independently reopens/verifies output values against source values, rereads
candidate framing/bytes, checks source length/CRC fingerprints, and writes a
durable `COMPLETE.json` only after success. Preserve and compare your external
SHA-256 inventory as well. Its successful outputs are:

| Output | Meaning |
|---|---|
| `current/` | Fresh raw-KV database containing only values whose current membership is proven through a trustworthy committed root/path. This is a verified subset, not a completeness claim. |
| `candidates.bin` | Values with physical page/slot identity but uncertain current membership. Keep separate; do not merge automatically or select a “latest” survivor. |
| `losses.jsonl` | Streaming known affected keys and unknown damaged extents. An empty known-key list does not imply zero loss when unknown extents exist. |
| `COMPLETE.json` and stdout JSON | Counts, verification results, ignored uncommitted-tail bytes, source-preservation result and limitations. Completion means verified salvage; the source is not replaced. |

On any error, preserve the source and partial destination. No completion marker
means the result is incomplete. Retry into a **different new destination**;
the command refuses an existing destination. A complete corrupt WAL currently
causes refusal rather than partial-region replay. Do not remove that WAL and
promote checkpoint-era rows: newer writes/deletes may exist only in it.

`current/` is not a complete typed restore: it may lack required catalog,
layout, mapping or vector records. Typed validation must separately verify
those dependencies and exact identities. Preserve the evidence and use backup
or explicit recovery decisions where completeness cannot be established.
No automatic publication/replacement step is supplied by this command.

## Typed candidate evidence and command boundaries

The library path
`recovery::recover_typed_candidates(source, destination, &collections::CollectionRecovery, options)`
discovers layout copies without requiring intact tree ancestors, overlays
interpretable committed WAL, and exports raw rows, layouts, issues and decoded
**candidates**. It does not acquire a source writer lock; use only the already
quiescent independent copy. If WAL overlay cannot be interpreted, the report
and issues record the fallback to bare data. That fallback cannot establish
current membership.

CollectionRecovery preserves numeric identity in decoded rows, but vector-bearing
rows remain raw/unresolved without a candidate-aware sidecar resolver. Catalog
names and timestamp policies are separate evidence. See
[collections.rs](../src/collections.rs) and [recovery.rs](../src/recovery.rs).

The inherited [recover CLI](../src/bin/recover.rs) has different routes:

- `recover inspect COPY` is physical data-page inspection only; it does not
  interpret page-WAL commits or establish current membership.
- `recover schema` uses the older `DenseV3` key classifier and does not select
  typed collection entity keys. It is not the CollectionRecovery entry point.
- `recover salvage` targets the inherited kernel Store. `recover verify`
  expects its `COMPLETE`/`database` output or a schema-candidate archive; it
  does not verify pagewal_repair's `COMPLETE.json`/`current` format.
- `collection_inspect COPY` uses a normal snapshot and may reconstruct
  coordination state on a quiescent copy. It attributes healthy-page storage;
  it is not a damaged-database repair or completeness verifier.

## Executed regression evidence and reproduction

The actual Phase 1 Linux runner executed full workspace tests in default,
compact/balance and retained-feature builds. In every mode,
`source_preserving_repair_contains_damage_and_never_resurrects_deletes` and
`collections::tests::rootless_collection_recovery_preserves_source_and_reports_vector_limit`
passed. Raw logs: [default](phase1-evidence/logs/workspace-default.log),
[compact](phase1-evidence/logs/workspace-compact.log),
[retained](phase1-evidence/logs/workspace-retained.log).

The raw test exercises clean data, committed WAL deletes, damaged leaf,
overflow, root, both metadata copies, one metadata copy, freelist and corrupt
WAL; source data/WAL bytes are compared before/after. It checks exact surviving
values, deletion exclusion, current/candidate counts, completion markers and
destination/overlap refusal. The typed test checks independently discoverable
metadata, an uncheckpointed scalar row, exact decoded scalar candidates,
unresolved-vector reporting and source preservation.

Focused reproduction on Linux with `TMPDIR` in the authorized artifact area:

```sh
cargo test --release --locked --offline --test pagewal_repair -- --test-threads=1
cargo test --release --locked --offline --lib collections::tests::rootless_collection_recovery_preserves_source_and_reports_vector_limit -- --exact --test-threads=1
```

These are focused selections of tests executed by the full runner. The
reference-capture Linux job also executed the retained `pagewal_repair` binary
on an independent copy of the committed-WAL baseline fixture: **606 current
raw KV entries verified, zero uncertain candidates, zero known affected keys
and zero unknown extents**. `COMPLETE.json` was present, exit status was zero,
and the full source file SHA-256 inventory was unchanged. This is a clean
operational CLI smoke, not a new damaged-data coverage claim. The 606 entries
include metadata/mappings/vector payloads for 283 entities; they are not 606
people. See [baseline evidence](FORMAT_BASELINE.md).
