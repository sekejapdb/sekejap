# Derived-index rebuild qualification

Candidate evidence, 2026-09-17. The isolated Linux `rebuild-r2` correction passed
**23 checks in each ordinary and retained build**: 4 rebuild tests, 10 verifier
tests, 7 current-source-reader tests and 2 explicit-codec creation tests.

The two successful-rebuild cases that failed with `WriterLocked` in
`quant-rebuild-r1` now pass. Rebuild reuses the physical encoding already
validated by its locked source reader; it neither reopens that protected
source file nor changes the process-wide creation default.

The tests reconstruct derived indexes and catalog references into a separate
new destination, including quantized vectors and reverse graph references.
They check authoritative row/vector/edge preservation, independent destination
verification, source byte invariance and completion markers. Missing primary
rows, vector sidecars, primary edges or all copies of an authoritative
declaration prevent publication. One damaged metadata replica can be recovered
from agreeing intact replicas. Lifecycle, overlap, existing-destination, work
and disk-budget refusal cases remain explicit.

Limits: this is verified reconstruction from readable authoritative storage,
not a guarantee of recovering every corrupted database. Building/dropping
indexes are refused. Consistent removal of an edge and its reverse, or an entity and every
reference to it, cannot be distinguished from legitimate deletion without
external evidence. Originals remain untouched.

R2 isolates the source-lock correction. A subsequent candidate reserves marker,
coordination and pager-bootstrap bytes before creating the destination; its
strict total logical-byte budget is being tested in `approx-query-r1`. Logical
length limits do not bound filesystem allocated blocks or reserved extents.

Evidence: [phase2-evidence/rebuild-r2](phase2-evidence/rebuild-r2/).
The original failure remains in [quant-family-r1](phase2-evidence/quant-family-r1/).
No public release, full Phase 2 acceptance or commit is implied by these tests.

Follow-up: `approx-query-r1` now passes101 checks per default/retained build,
including5 rebuild tests and3 codec-creation tests. This qualifies the added
preflight reserve for completion/control files and pager creation, with
insufficient persisted-policy headroom refused before destination creation.
See [approx-query-r1 evidence](phase2-evidence/approx-query-r1/).
