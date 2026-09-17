# Phase 2 multi-family crash qualification

Linux crash-r1 completed with exit0 on 2026-09-17. Both default and retained
packing passed36 cases each. This is process-crash and injected-checkpoint
coverage, not a power-loss guarantee or complete recovery acceptance.

| Case group | Per build | What was checked |
|---|---:|---|
| Multi-family transaction | 6 | Kill before commit, after commit, and four timed commit-window kills; exact old or new entity/scalar/graph/vector/spatial/text state; held snapshot stays old |
| Interrupted index lifecycle | 24 | Scalar/vector/spatial/text build and drop, before/during/after commit; raw descriptor and posting/stat entries match an exact old or committed endpoint; operation resumes correctly |
| Deterministic checkpoint interruption | 6 | Existing PageWAL crash stages1..6 terminate abruptly; reopening retains the exact committed state |

For the transaction timing samples, default recovered the updated state in all
four cases. Retained recovered the original state once and updated state three
times. Every result matched one complete endpoint. This does not establish
which internal write the scheduler interrupted. Deterministic lifecycle
barriers and checkpoint stages are reported separately; existing exhaustive
in-crate write-fault tests remain companion evidence.

Each arm uses a fresh copied database. Sources remain unchanged. A separate
process holds and rechecks the old snapshot during transaction/lifecycle arms.
The controller uses queued binary-pipe readers to avoid buffered-line polling
errors, drains stderr and kills/reaps children on failure. Tests ran in the
isolated server job using locked offline release builds,2CPU/2GiB limits.

Commands, exact source inventory, binary hashes, observations, signatures and
completion report:
[phase2-evidence/crash-r1](phase2-evidence/crash-r1/).
The tested engine is the already-qualified query-r1 snapshot with the new test
harness; it excludes the ongoing verifier and later mixed-query changes.

Still pending: the complete corruption classifier and source-preserving derived
rebuild, final combined-query qualification, wider matched workloads and the
full Phase2 acceptance audit. No Phase2 release or commit is claimed.
