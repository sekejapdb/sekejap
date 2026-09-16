# E4 Phase 1 format review — 2026-09-16

Scope: uncommitted `sekejap-e4-int` / `l2-integration` versus `d789fc2`; source review of AGENTS/README, current owner Law 8 in main checkout CONTRACT.md, and `/tmp/sekejap-e4-format-stability-report-20260916.md`. No performance experiments or Mac database tests. Findings below distinguish original candidate defects from fixes now prepared for parent Linux qualification.

## Conclusion

The per-database codec direction is correct. The candidate is not yet qualified as an immutable release baseline. Two concrete unsupported-version fallback defects permit writers to treat newer metadata as damaged, select an older sibling, then normalize/rewrite files. Minimal fixes and focused regressions are now present but require Linux red/green execution. The reference fixture suite also needs mandatory fixture presence, accurate format documentation, and identity/exact post-write content assertions. Naming a candidate is reasonable; claiming released-fixture compatibility before a released reference exists is not.

## Concrete findings and prepared fixes

1. **P1, typed release path: supported sibling masks intact unknown typed header.** Original `src/collections.rs:305-330` saves `Unsupported` but returns any `good` replica. A single checksummed `E4COLL9` replica beside two `E4COLL1` replicas therefore passes `typed_check`; `src/pagewal.rs:670-672` subsequently calls `finish_open`, which truncates uncommitted WAL and republishes hints. This defeats Law 8 before normal writes even begin. Original `parse_header` at 275-282 also labels unknown magic unsupported before validating the packet CRC, so simply returning that error first would incorrectly eliminate damaged-copy fallback. Fix: validate an unknown-version packet's checksum/shape first; return an intact `Unsupported` immediately from `replicas`. Known damaged replicas retain fallback.

2. **P1 for inherited Store scope: future superblock can be silently ignored.** `kernel/src/meta.rs:222-226` originally adopts either valid sibling for every other error, including unsupported logical or physical versions. Normal Store open validates through `read_limits` before WAL open, so making that decision fail closed also prevents recovery mutation. Snapshot reservation likewise first calls `limits::read`, which uses `Meta::read_latest`, before creating/writing its slot. Fix: propagate explicit unsupported logical-version errors. For physical-version errors, the page parser checks version before checksum, so only on this error path re-read the fixed 4 KiB metadata page and validate CRC; intact unsupported pages refuse, damaged ones may fall back. Existing public error taxonomy remains `kernel::Error::Corrupt` with precise reason; no persistent bytes change. This path is not the selected collection backend, and its earlier prototype formats need not be declared as E4 release formats merely because support was repaired.

3. **P1 qualification gap: silent fixture skip.** `tests/format_v1_compat.rs:31-38` returns None when INDEX is missing and all five tests return successfully. The mandatory release compatibility gate must fail on absent fixtures/index or run through a dedicated required profile that does so. A passing compilation is not fixture evidence.

4. **P1 format specification gap.** `docs/FORMAT_V1.md:5` specifies 4128-byte frames, while `src/pagewal.rs:19-20,52-57` writes 4144 (4096 + 32 header + 16 identity). It references `docs/DISK_FORMAT_AUDIT.md`, absent in this integration checkout. Correct the frame size and include the actual format specification before freezing. The inherited Store and PageWalStore version numbers describe different envelopes and should be stated separately.

5. **P2 fixture oracle gap.** `tests/format_v1_compat.rs:256-268` compares documents but not `EntityId`; manifest entity generation at `src/bin/format_fixture.rs:363-388` does not include entity IDs. Collection IDs/layout IDs/timestamp policy are compared. Post-write reopen only checks the three changed people keys; it does not verify every unaffected entity/vector/layout against the independent expected corpus. Add exact entity identity and exact scan content/set assertions and a complete expected post-mutation population. A raw row count cannot prove absence of duplicate/omitted identities.

## Files prepared and validation handoff

- Runtime: `src/collections.rs`, `kernel/src/meta.rs`.
- New focused integration test: `tests/format_replica_refusal.rs`.
- Pre-fix candidate runtime copies, preserving the previous team's codec work: `/tmp/e4-format-before-fixes/src/collections.rs` and `/tmp/e4-format-before-fixes/kernel/src/meta.rs`.
- Minimal only-this-review runtime patch: `/tmp/e4-format-refusal-fixes.patch` (forward broken candidate -> fixed).

Tests were authored before runtime edits; they have NOT been executed locally. Parent must run the new test against the preserved broken runtime on Linux, record failures, then restore the fixed runtime and rerun. The damaged-future-typed-copy case is expected to pass even before the fix; it is the negative control guarding CRC fallback.

Suggested Linux commands with approved artifact TMPDIR, serial execution:

```
cargo test --test format_replica_refusal -- --test-threads=1
cargo test --features compact-cells,sqlite-balance,keyspace-append,slotref-split --test format_replica_refusal -- --test-threads=1
cargo test --test collection_pagewal --test pagewal_recovery_identity --test pagewal_cell_codec --test pagewal_create_codec --test write_path -- --test-threads=1
cargo test -p kernel meta::tests -- --test-threads=1
```

New regression scope: each one-of-three future typed header, live and quiescent snapshots, each one-of-two future Store logical and physical header, exact source inventory including coordination files, uncommitted tail preservation, CRC-invalid future replica fallback. Parent may extend the required suite; no large performance run is needed.

## Codec assessment

All observed runtime leaf encoding calls now use the pool's installed database codec: `kernel/src/btree.rs` insertion, `kernel/src/bulk.rs` packing; ordinary/compact decoders in `verify.rs` and fast B-tree helpers decode both families unconditionally. PageWal opens install header feature bits and rollback reinstalls them. Header commit writes preserve state.features. Inherited Store saves the base logical version and emits it on checkpoint, with LIMITED separately derived from the persisted policy. I found no remaining build-default restamp in these reviewed paths.

Plain cells are legal under the compact feature and overflow markers remain ordinary. A database declaring no compact feature must not acquire compact cells via normal writing. A test that manually flips a compact file's feature off is decoder robustness evidence, not evidence of a historically valid old file. The true create-both-family fixtures are stronger evidence. The process-global create override is usable for fixtures but is a broad new public API: concurrent callers share its setting; per-create configuration would eventually be clearer. It need not block on-disk stabilization.

## Reader/WAL assessment

The new fold occurs only on a clean transaction boundary (`!dirty`, WAL end equals last commit), and the existing checkpoint guard defers while any reader is pinned. This preserves snapshot frames. The hard WAL cap is allowed to refuse writes; a failed writer must rollback/reopen, then continue when the old reader drops. The rewritten regression exercises refusal, poisoning and recovery. Strengthen it with exact full snapshot rows and full committed population if it becomes primary release evidence: a count and p0000 alone are not an exact snapshot oracle. No format change is required for this behavior.

A WAL-cap or rollback fixture is useful as lifecycle/recovery coverage, but these histories do not create a new persistent encoding and need not each multiply the immutable fixture matrix. Include a committed WAL fixture; keep small independent lifecycle regressions for cap/refusal/rollback/empty/deleted-only states. Include at least one >=3-page overflow and boundary-sized cells/value in a compact fixture or focused codec tests. Do not force impractical maximum u32 payload allocations merely to name v1.

## Unknown formats, future extensions, and coordination

Required bits are an adequate database-level admission mechanism only if every incompatible future page/record/catalog/index representation declares its requirement in that already-readable envelope before it is published. Physical page versions are already explicit u16 fields, even though the supported value is currently a constant. Reserved bytes are not mandatory: keep the old header decoder forever, and use a new explicit feature/version when needed. The existing page-WAL metadata scanner correctly refuses any intact unsupported metadata copy before selecting a sibling; its WAL scanner validates complete frames before finish_open. Do not rely on discovering a future nonmetadata page only after recovery: the database envelope must advertise the requirement.

`readers.lock` and slot files are reconstructible coordination state, not semantic dataset payload. Their names/locking protocol remain operational compatibility concerns for supported concurrent processes. Document them as reconstructible only while quiescent; never unlink/recreate live locking inodes. The dataset identity, committed prefix and independent data/WAL remain authoritative. Different-user creation requires ordinary directory permissions, not an immutable fixture of particular owners/lock bytes.

Do not freeze nonexistent graph/spatial/text/vector-navigation encodings. Freeze the current entity/page/WAL/layout envelope and namespace ownership; reserve or centrally allocate new tags so they cannot collide with E4's current 0x10/0x20/0x40/0x60 and metadata families. When each actual index ships, give its catalog descriptor explicit family/encoding version, allocate a required capability before writing unreadable structures, add preserved query/mutation/reopen fixtures, and retain old-format readers AND writers. Opening/updating old data must not auto-enable that index or require a rebuild. Index-free old files continue working; explicit new-index creation may raise the required feature set atomically. Existing E3 tags must be mapped, not copied. Law 8 starts at the declared E4 release and imposes no E3 data migration.

The first immutable release corpus must be retained forever once declared. A later released/newer binary should read and mutate a copy, and the older released binary should reopen that copy while the feature set is unchanged. Before the first release, label same-generation/cross-build proofs accurately as candidate evidence. Regenerate candidate fixtures now if necessary; never replace a released corpus in place.

## Supplemental r2 admission audit

Parent Linux r1 reports two red controls caught, three focused green tests, seven fixture tests and write_path passed. Parent then identified a remaining inherited Store admission-order gap: from_page checked extension shape and minimum root-record length before inspecting its logical version. An intact version3 record with a new shorter layout or extension became ordinary corruption and could lose to a supported sibling. Confirmed by source analysis. Version admission now requires only the existing two-byte prefix before applying supported-version record and extension rules. Typed packet rules are unchanged.

Supplemental pure test `store_version_admission_precedes_version_specific_payload_parsing` covers eight future-format combinations (LIMITED on/off, short/full record, new extension absent/present), plus truncated-prefix and known-version malformed-record/extension negative controls. No local test execution. R1 source saved at `/tmp/e4-format-r1-before-admission/kernel/src/meta.rs`; parent can run the new test against that file for red evidence and the current checkout for green. Minimal forward patch `/tmp/e4-format-admission-order.patch`.

## Supplemental feature-policy test audit

Parent Linux default suite reported failure at the final redistribution-coverage assertion in `btree::split_byte_equivalence::both_split_implementations_write_identical_pages_for_every_corpus_state` and the >=90% fill assertion in `btree::tests::every_keyspace_packs_its_own_ascending_run`. Source confirms both require the optional sqlite-balance algorithm: default builds compile no neighbor redistribution, and plain non-rightmost ascending runs use balanced splits. No runtime change is warranted from these assertions alone. The full assertion logs remain with the parent; no byte/row mismatch was reported at this point.

Prepared test-only changes in kernel/src/btree.rs: branch reach equals whether sqlite-balance is compiled; all page-byte/root/row comparisons and one-/three-way split reach remain unconditional. The exact existing density target remains unchanged and runs with sqlite-balance, including the intended all-features candidate. The related mixed-access target <=4.30 is guarded by keyspace-append because its own original documentation records the compact+balance control at 4.734 and the optimized candidate at 4.059; standalone <=1.50 controls remain unconditional. No threshold changed and no persistent/runtime code changed.

Original source `/tmp/e4-phase1-btree-before-test-fixes.rs`; isolated patch `/tmp/e4-phase1-btree-test-policy.patch`. `git diff --check` passes. Parent Linux should run the focused kernel tests in default, compact+balance and all-features modes, then complete its release qualification.
