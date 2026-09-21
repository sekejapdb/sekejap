# Phase 1 complete — 2026-09-16

> Superseded 2026-09-21: the envelope named `e4-format-v1` here is now **sekejap disk format v2** ([core/FORMAT_V2.md](core/FORMAT_V2.md)), stamped into page bytes 18-19. e4 was never published; this document is the record of that pre-release baseline.

**Disk-format stabilization is complete.** The declared entity-storage baseline
is **e4-format-v1**, frozen engine `59d1cbc770284f160ffda53cc1ee545167733d11`.
Read [FORMAT_BASELINE.md](FORMAT_BASELINE.md) for artifacts and future upgrade
commands, [core/FORMAT_V2.md](core/FORMAT_V2.md) for byte/extension rules, and
[PHASE1_COMPLETION_AUDIT.md](PHASE1_COMPLETION_AUDIT.md) for acceptance evidence.
tracker task: `sekejap-e4/phase1-format-qualification`.

## Accepted result

- Typed collections use PageWalStore, with both cell encodings readable and
  writable in every build. Existing database features remain unchanged.
- Unsupported intact metadata is refused before mutation; CRC-damaged replicas
  can fall back. Logical version admission precedes version-specific parsing.
- Frozen source, actual Linux binaries, five independently specified baseline
  databases and provenance are retained. The earlier five prototype databases
  remain unchanged. Ordinary tests require both corpora (132 total files).
- Cross-binary read/update/insert/delete/commit/rollback checks cover all five
  baseline fixtures at checkpointed and committed-WAL boundaries, using the
  default preserved binary and compact/retained builds:20cases passed, then
  all20passed again after a source-preserving path guard was hardened.
- Full Linux workspace qualification remains422/427/427pass, zero failures,
  two pre-existing ignored tests per build. Expanded fixture tests7/7pass in
  each of the three modes. Three path regressions fail old driver/pass final.
- Recovery CLI smoke verifies606raw KV entries for283entities with unchanged
  source SHA-256 inventory. [The runbook](PHASE1_RECOVERY_RUNBOOK.md) explains
  raw/current/candidate distinctions and incomplete typed/rootless recovery.

The comparison binaries use the same frozen engine source with different build
features. This is cross-build qualification and the permanent baseline for
future versions; no later public-release binary is invented. Every later
engine release must be tested against the preserved baseline binaries/bytes.

## Repository and evidence

Main checkout `<home>/`, branch
`pagewal-foundation`, incorporates the qualified59d1cbc engine. The completion
commit adds fixtures/harness/docs only, with no engine encoding change. See
branch history for its commit. The integration worktree is `sekejap-e4-int`,
branch `l2-integration`. Main's268pre-existing untracked research files were
verified unchanged during initial integration. Original tracked policy edits
were already adopted by59d1cbc and remain backed up in the named Phase1 stash.

Prior full-suite evidence: `docs/phase1-evidence/`.
Baseline/rollback/CLI evidence: `docs/format-baseline-evidence/`, including exact
source archive, immutable binaries, raw logs/reports, provenance and SHA256SUMS.
Final Linux reference Job `e4-phase1-reference-r5-20260916`, root
`<scratch>`; markers
`REFERENCE_QUALIFIED`, `REFERENCE_EXIT=0`, follow-up `FINAL_DRIVER_EXIT=0`.

## Next boundary

Proceed to interfaces and multimodel implementation on this storage contract.
Do not restart general insertion optimization or import unfinished l24/Grok
no-clone experiments. Timestamps stay off by default and explicitly opt-in.
Future graph/spatial/fulltext/vector-navigation indexes need noncolliding
namespaces, explicit version/features and permanent fixtures before shipping;
new versions must preserve read/write support for the baseline.

Public product packaging, EXPORT/IMPORT, published API compatibility and wider
eight-law qualification remain product work. Historical resource, strict
scaling and broad recovery gates are not silently changed to PASS. No public
release or production deployment has occurred. Pi-specific research can wait.

## Access

server kubeconfig `<home>/`, namespace
`sekejap-benchmark`, PVC `sekejap-benchmark-data` at `<scratch>`. Reusable prefix:
`kubectl --kubeconfig <home>/ -n sekejap-benchmark`.
Use curl for tracker `http://127.0.0.1:5156/`; notes cap8000characters, GET after
writing. Mac database execution still requires the independent I/O control;
this loop's acceptance ran on Linux. No agent remains assigned to engine edits.
