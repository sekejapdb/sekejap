# Vector and spatial qualification — 2026-09-17

Status: candidate correctness evidence on server Linux, not release acceptance.
Primary entity, point and f32 vector encodings remain unchanged. New indexes
require explicit feature creation; no Phase 2 format has been frozen.

| Qualification | Default | Retained compact build |
|---|---:|---:|
| Vector baseline and graph/scalar regressions | 25 passed | 25 passed |
| Expanded vector/spatial behavior, admission, write faults and spatial math | 45 passed | 45 passed |

All entries report zero failures and zero ignored tests. These are selected
tests, not full workspace counts. The earlier graph/scalar full workspace
qualification is recorded in `PHASE2_SCALAR_R6_RESULTS.md`.

Exact vector tests cover cosine, squared L2 and negative dot, independent
ranking, deterministic ties, candidate filtering before top-k, missing/null and
zero vectors, layout ordinals, collection/field isolation, late build/live CRUD,
drop/resume, rollback, snapshots, reopen and explicit work/cancellation errors.
Coordinates remain in primary sidecars; the new six-byte locator does not
duplicate embeddings.

Spatial tests cover WGS84 point bbox/radius/nearest queries, dateline and
longitude aliases, inclusive boundaries, independent membership/distance
expectations, conservative Hilbert candidates, CRUD/build/drop/snapshots/reopen
and resource bounds. Nearest search currently scans the selected population;
this qualification makes no logarithmic nearest-neighbor performance claim.

For both families, admission checks mutate each descriptor replica to an
intact unsupported family/version/option and verify refusal with source bytes
and inventory unchanged, including pending WAL tails. Additional cases exercise
missing feature admission, malformed postings/locators and lost/invalid vector
sidecars. Exhaustive injected key-write failures cover first feature/catalog
creation, another index, insert/update/null/delete, build progress/publication
and drop progress/finalization. Each failed operation checks writer poisoning,
old/published snapshots, rollback, retry, commit and reopen.

Two failed runs are preserved. `spatial-r1` failed to compile because shared
Cargo artifacts were newer than the copied source mtimes. Refreshing source
mtimes forced the correct candidate build without changing its contents.
`spatial-r2` found an incorrect expected snapshot result: its world bbox omitted
the row explicitly inserted during Building. That row was never deleted and
belongs in the expected result. The corrected test passes in both configurations.
No engine behavior was changed to satisfy that expectation.

Evidence, including both failures, scripts and exact source manifests:
`docs/phase2-evidence/vector-spatial-r1/`. Native run root:
`<scratch>`, isolated pod
`e4-phase2-20260916-c9rp8`. `vector-r1.exit` and `family-fault-r1.exit` are zero.
Later local fulltext, targeted field-reader and recovery work is excluded from
this tested source and must be qualified separately.

Still required: process-kill schedules, independent full-index verification and
source-preserving rebuild, preserved per-family old/new binary compatibility,
combined-query correctness and representative query/CRUD/memory/peak-disk
comparisons. A missing derived posting may be invisible to its own index scan;
full verification must derive expected entries from authoritative primary data.
Lost primary vectors or graph properties cannot be reconstructed from lookup
indexes, and that loss must remain explicit.
