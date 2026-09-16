# E4 phase 1 compatibility gate — 2026-09-16

Scope: only `<home>/` changed. Existing fixture files, runtime, Cargo and documentation were not edited. This is candidate qualification tooling, not release qualification.

## Changes

- Missing `INDEX.json` now panics with a required-preserved-fixtures diagnostic instead of silently skipping. A pure filesystem regression calls the required-root helper with `Cargo.toml` as the root (a regular file cannot contain INDEX.json).
- Pins the existing INDEX SHA-256 (`e274ef52d6de6e5073bd6f37b753a52db9bf6ead0affacc3db571b2dbe4da64e`), verifies each manifest against its already-recorded index checksum, and verifies exact source file inventory. This closes the previous gap where manifests were parsed without checking their indexed checksum, and an edited manifest plus edited fixture could pass unnoticed. Future declared baselines must add their own deliberate fixture lock; do not regenerate this corpus in place.
- Rechecks preserved source manifest/file hashes after successful reader/writer operations and refusal checks. Copies remain the only databases opened.
- Compares every scanned key/document against the manifest with duplicate detection, not just scan count. Writer reopen checks all original records plus the independently specified update/insert/delete, not just three touched keys.
- Scratch copies use unique `tempfile::TempDir` paths under TMPDIR and clean up through RAII. On macOS TMPDIR must be inside `<scratch>`; Linux must be run with TMPDIR in the authorized server artifact area. This removes a hardcoded Mac scratch directory and concurrent-run path collisions.
- Adds checksum-valid physical page version refusal tests. For each of five fixtures, alter metadata copy 0, copy 1, or both to version 65535, recalculate CRC, and require writer/snapshot opens to report `unknown format version` without changing data/WAL. A supported sibling or committed WAL must not hide the unsupported copy.

Suite now has seven tests. Feature-bit preservation checks remain throughout normal writes.

## Validation actually run locally

Mac database execution intentionally not performed: AGENTS.md requires Linux qualification while Mac stale-read control remains unresolved.

1. Before edits, extracted the actual `fixtures_root` Rust helper into a standalone program, substituted a nonexistent root, and asserted `catch_unwind(fixtures_root).is_err()`. Rustc succeeded; execution exited **101**, printing the old skip message and `missing preserved fixture corpus was silently accepted`. Evidence: `/tmp/e4-compat-missing-fixtures-red.rs` and matching executable.
2. After edits, extracted actual `fixtures_root` plus `require_fixtures_root` into the same standalone harness. Execution exited **0** after catching the intended missing-fixture panic. Evidence: `/tmp/e4-compat-missing-fixtures-green.rs` and executable. This establishes red/green for the concrete silent-pass defect without database I/O.
3. Python's hashlib independently checked all five existing manifests against INDEX and all 60 source files against manifest lengths/checksums, including exact inventory: **pass**. Source fixtures remain unchanged.
4. Extracted suite SHA-256 helper and verified empty-input, `abc`, and actual INDEX bytes against known/independent SHA-256 results: **pass** with `rustc --edition 2021 --crate-name sha_check /tmp/e4-compat-sha-check.rs -o /tmp/e4-compat-sha-check && /tmp/e4-compat-sha-check`. An initial standalone compile omitted the edition and failed to import TryInto; rerunning with the project's 2021 edition corrected the harness only.
5. `rustfmt --check tests/format_v1_compat.rs`: **pass**.

## Required Linux execution (parent owns)

Run inside the integrated Linux checkout with TMPDIR already set to an existing isolated directory in the authorized Linux artifact area. No fixture generator invocation.

```sh
cargo test --test format_v1_compat -- --test-threads=1
cargo test --features compact-cells,sqlite-balance --test format_v1_compat -- --test-threads=1
cargo test --features compact-cells,sqlite-balance,keyspace-append,slotref-split --test format_v1_compat -- --test-threads=1
```

Full integration compilation and seven-test runtime results remain pending Linux evidence. Expected original manifests contain documents but not independently recorded per-entity numeric IDs; these tests cannot retroactively establish released identity stability. No graph/spatial/text/vector-nav persisted index corpus exists. Refusal assertions compare data/WAL bytes, not every coordination file. Fixtures are still codec-only prototype-build fixtures, not owner-declared release-binary fixtures; older-release downgrade remains unproved.

tracker read `curl -s http://127.0.0.1:5156/api/journeys/sekejap-e4` returned exit 7 in this agent environment. Parent notified and owns tracking sync.

## Follow-up documentation scope from parent

Parent subsequently assigned `docs/FORMAT_V1.md` and `docs/FORMAT_FREEZE.md`; both now updated. FORMAT_V1 specifies the actual source-linked 4144-byte frame/metadata envelope, required immutable corpus policy, reserved physical bytes versus existing generation field, coordination protocol precautions, future index namespace/version/feature policy, and explicit pending release/Law8 limits. FORMAT_FREEZE prepends the superseding Phase1 current boundary while retaining and labeling historical evidence. All relative Markdown links resolve. Searched both files and found no obsolete 4128-byte size, nonexistent DISK_FORMAT_AUDIT link, silent skip statement, or instruction to overwrite the preserved corpus. No test/runtime/fixture changes accompanied the documentation follow-up.

## Bounded follow-up: independent IDs and whole-file refusal

Parent requested closing the candidate-corpus numeric-ID oracle gap without changing manifests. Confirmed original `src/bin/format_fixture.rs::populate`: people initially allocate IDs 1..200; slot13 is deleted; slot7 is deleted then reinserted as201; events independently allocate1..80 and blobs1..4. Added explicit `expected_entity_id` mapping documented against this insertion order, not engine reads. Existing seven-test suite now verifies IDs through get/get_by_id/scan, deleted IDs14 and (after writer mutation)2, retired pre-reinsert ID8, update return ID1, inserted person ID202, and all IDs after reopen. The original report's numeric identity limitation is superseded for this candidate corpus; released-binary identity compatibility remains unproved.

Refusal tests now snapshot and compare the entire copied directory's filename-to-bytes map, including coordination files, so additions/deletions or mutation of any file fail (not only data/WAL). Existing preserved source checks remain. The original report's data/WAL-only limitation is superseded.

Updated only `tests/format_v1_compat.rs` and corresponding `docs/FORMAT_V1.md` text during this follow-up. Pure extracted identity helper passed an independent simulation of the original insertion sequence: `rustc --edition 2021 --crate-name identity_oracle /tmp/e4-compat-identity-oracle.rs -o /tmp/e4-compat-identity-oracle && /tmp/e4-compat-identity-oracle`. Formatting passes; complete preserved fixture hash/inventory verification passes again. No Mac DB execution. Parent's r1 Linux source archive predates these assertions and needs supplemental final per-mode compat-suite runs, which parent owns.

## Full-suite follow-up: runtime WAL-limit oracle

Parent reported Linux r1 default root-lib red at `src/pagewal_hint_tests.rs:339`, `pagewal::hint_tests::runtime_limits_refuse_before_framing_and_survive_rollback`. Parent owns full assertion log retrieval. Pre-edit file preserved at `/tmp/e4-phase1-pagewal_hint_tests-before.rs`.

Source analysis distinguishes an obsolete refusal precondition from a runtime cap bug: three committed frames remain after the tracked-page checks. The test sets wal_bytes to committed+one frame (four frames). `checkpoint_due` is already true at half that allowance. New `fold_committed_wal_if_at_cap` can fold the old committed prefix before the next clean put when no reader is present, allowing the new three-frame transaction inside the unchanged four-frame cap. Expecting that commit to fail without pinning the prefix is no longer correct.

Changed only the WAL subsection of that test: retain a snapshot before setting the exact same cap; attempt two updates, each requiring the precise wal_bytes ResourceLimit; assert failed append never exceeds the cap; rollback must restore exact committed length and v2, keep the prior refused overflow key absent, and leave the pinned reader at v2 throughout. The second attempt proves the same WAL runtime allowance survives rollback. Drop reader before the unchanged data-extent checks. No runtime or limit change. Pure loop typecheck passes; no Mac database execution. Source SHA256 `7699104972b6df86c11fa4c2033d472a58ac0ffa1b58e8592a37ffd259b1b7b4`.

Required parent Linux green: `cargo test --lib pagewal::hint_tests::runtime_limits_refuse_before_framing_and_survive_rollback -- --exact --test-threads=1`; then rerun affected full root-lib modes/full qualification as warranted. At handoff this remains awaiting Linux execution; do not count source analysis as a runtime pass.
