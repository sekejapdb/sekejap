# Phase 2 workspace qualification

The reusable lean profile passed on server Linux on 2026-09-17: 92 checks in
nine groups per build, default and retained, 184 checks total. All 18 group
commands, log writers and harness checks returned zero. The two large
65,537-result pagination tests are deliberately outside this lean profile;
they already passed in the separate query-guard run.

This run uses the query-guard engine with the current quantized compatibility
and crash harnesses. The full workspace subsequently passed in both builds: **547 passed / zero
failed / two ignored** by default and **552 passed / zero failed / two ignored**
with retained features. These are regression results, not scale-performance
qualification. The existing ignores are `shuffled_entries_use_neighbor_capacity`
(the existing no-redistribution density case) and
`store_churn_audits_last_issued_page_write` (the large forensic probe).
The later R6 benchmark instrumentation and corrected SQLite schema are not
part of this source capture.

Evidence: [workspace-r1](phase2-evidence/workspace-r1/), including the exact
source inventory, group commands, logs, status table and summary. Native root
is `<scratch>`, pod `e4-phase2-20260916-c9rp8` in
namespace `sekejap-benchmark`. The command is
`tools/phase2_qualify.sh --profile lean --config both` with the explicit
authorized temporary root and new output directory recorded in metadata.

Coverage and limits are mapped in [PHASE2_TEST_GROUPS.md](PHASE2_TEST_GROUPS.md).
No Phase 2 acceptance or release is claimed by this result.
