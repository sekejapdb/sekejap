# Phase 1 continuation — 2026-09-16

**Current result: qualified storage candidate; retain.** Final report:
[PHASE1_QUALIFICATION.md](PHASE1_QUALIFICATION.md). Format contract:
[FORMAT_V1.md](FORMAT_V1.md). tracker owns live status at
`sekejap-e4/phase1-format-qualification`.

## Where to resume

Integration checkout: `<home>/`, branch
`l2-integration`, based on `d789fc2`. Main checkout `sekejap-e4` is separate
and its unrelated dirty work is preserved. This candidate incorporates the
owner's already-adopted eight-law policy from the main checkout. See the
current branch log for the scoped qualification commit. No production release
or deployment was performed.

The owner authorized Codex to lead Phase 1. The storage shape is established
enough to proceed to interfaces; do not restart general insertion optimization.
The first public release still needs its selected binary and independently
specified immutable release corpus captured. Preserve the existing codec-only
candidate corpus alongside it. Future index families need explicit namespace
allocation, encoding versions/features and their own permanent fixtures.

## What was retained

- Per-database codec selection; every build supports both declared families.
- Safe refusal of intact unsupported typed and inherited metadata replicas.
- Version admission before inherited version-dependent payload parsing.
- Mandatory checksummed fixtures, full document/numeric-ID/scan/write/reopen
  oracles, and whole-file source-preserving refusal checks.
- Previous measured append/slot-reference split improvements, with test
  coverage respecting enabled algorithms. No new speed hypothesis.
- Existing WAL cap unchanged; the limit test now pins a reader so automatic
  checkpointing cannot legitimately free its committed prefix.

Do not import unfinished `sekejap-e4-l24` or Grok no-clone experiments.
Current changes add no disk field or encoding. Timestamps remain opt-in.

## Final evidence

server isolated Job `e4-phase1-r2-20260916`, root
`<scratch>`, Rust 1.97.1, release profile,
2 CPU/2Gi limit, serial tests. Final marker `PHASE1_R2_EXIT=0`.
Full workspace default: 422 pass / 0 fail / 2 ignored; compact/balance:
427/0/2; retained features: 427/0/2. Existing ignores are listed in the report.
Focused refusal4, fixture/identity7, reader-cap1 and corrected limits1 passed.
Three deliberately broken previous-code regressions failed as intended.

Source SHA256:
`0fc052d05f55ec48eb3c45ca129319a8a97cb2d0ff15de2901131dbd12a71075`.
Verified exported evidence archive SHA256:
`93b1d2ad381a5db8d657939e0445a1f2603a31390974fb4a80d66665d9b79e73`.
Portable logs/source archive/manifest/counts: `docs/phase1-evidence/`.
All 66 original fixture-corpus files remain unchanged. The r1 failures and
corrections are retained; they are not hidden as green qualification.

## Access and scope

Kubeconfig `<home>/`, namespace
`sekejap-benchmark`, PVC `sekejap-benchmark-data` mounted `<scratch>`.
The reusable approval is saved for the command prefix:
`kubectl --kubeconfig <home>/ -n sekejap-benchmark`.
Keep this ordering. Do not alter production workloads.

tracker: `http://127.0.0.1:5156/`; use curl. Notes cap at 8000 characters;
GET after writing. Existing older handoffs now point here. Both agents
(format_review and compat_gate) completed their tasks. No agent is still
editing the candidate. Local database execution was not used as acceptance.
